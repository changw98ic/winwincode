import { resolve } from 'node:path'

import {
  STRONGFLOW_ROLE_TOOLS,
  strongFlowPermissionPolicyForPreset,
  type StrongFlowPermissionPolicy,
  type StrongFlowRoleArtifactKind,
  type StrongFlowRoleTool,
} from '@winwincode/contracts'
import {
  GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION,
  type ApprovalDecision,
  type ApprovalResponse,
  type DynamicToolResponse,
  type GovernedSessionAuthority,
  type GovernedSessionEffectivePolicy,
} from '@winwincode/native'

import type {
  StrongFlowRoleContextInstallation,
  StrongFlowRoleContextInstallationRequest,
  StrongFlowRoleContextInstaller,
  StrongFlowRoleKernelEvent,
  StrongFlowRoleKernelLifecycle,
  StrongFlowRoleSessionContext,
} from './role-session.js'
import {
  resolveExistingStrongFlowWorkspacePath,
  resolveStrongFlowWorkspaceWritePath,
} from './workspace-policy.js'

const MAX_TOOL_OUTPUT_BYTES = 8 * 1024 * 1024
const MAX_APPROVAL_SCOPE_BYTES = 256 * 1024
const SENSITIVE_KEY = /(?:auth|credential|key|password|secret|token)/iu

export type StrongFlowRoleAuthorityErrorCode =
  | 'INVALID_AUTHORITY_CONTEXT'
  | 'ENFORCEMENT_UNAVAILABLE'
  | 'INVALID_KERNEL_EVENT'
  | 'TOOL_DENIED'
  | 'TOOL_ARGUMENT_INVALID'
  | 'TOOL_EXECUTION_FAILED'
  | 'APPROVAL_AUDIT_FAILED'
  | 'INSTALLATION_DISPOSED'

/** Stable denial at the role-authority boundary. */
export class StrongFlowRoleAuthorityError extends Error {
  readonly code: StrongFlowRoleAuthorityErrorCode

  constructor(
    code: StrongFlowRoleAuthorityErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowRoleAuthorityError'
    this.code = code
  }
}

export interface StrongFlowRoleToolExecutionRequest {
  readonly jobId: string
  readonly stageRunId: string
  readonly attemptId: string
  readonly roleId: string
  readonly contextId: string
  readonly kernelSessionLineageId: string
  readonly kernelSessionId: string
  readonly kernelStreamId: string
  readonly kernelSequence: string
  readonly turnId: string
  readonly callId: string
  readonly tool: StrongFlowRoleTool
  readonly arguments: Readonly<Record<string, unknown>>
  readonly resolvedWorkspacePaths: readonly string[]
  readonly signal: AbortSignal
}

/** Host-owned execution seam. Implementations must retain the supplied sandbox and root. */
export interface StrongFlowRoleToolExecutor {
  execute(request: StrongFlowRoleToolExecutionRequest): Promise<unknown>
}

export type StrongFlowRoleApprovalOutcome =
  | 'approved'
  | 'rejected'
  | 'cancelled'
  | 'unavailable'

export interface StrongFlowRoleApprovalSource {
  readonly authority: 'codex-core'
  readonly kernelSessionLineageId: string
  readonly kernelSessionId: string
  readonly kernelStreamId: string
  readonly kernelSequence: string
  readonly turnId?: string
}

export interface StrongFlowRoleApprovalRequest {
  readonly jobId: string
  readonly stageRunId: string
  readonly attemptId: string
  readonly roleId: string
  readonly contextId: string
  readonly operationKind: 'exec' | 'patch'
  readonly operationId: string
  readonly requestedScope: Readonly<Record<string, unknown>>
  readonly source: StrongFlowRoleApprovalSource
  readonly signal: AbortSignal
}

/** DSH-facing, non-model interaction seam for one-shot human decisions. */
export interface StrongFlowRoleApprovalInteraction {
  request(request: StrongFlowRoleApprovalRequest): Promise<StrongFlowRoleApprovalOutcome>
}

export type StrongFlowRoleApprovalAuditEvent =
  | {
    readonly schemaVersion: 1
    readonly type: 'strongflow.approval.requested'
    readonly jobId: string
    readonly stageRunId: string
    readonly attemptId: string
    readonly roleId: string
    readonly contextId: string
    readonly operationKind: 'exec' | 'patch'
    readonly operationId: string
    readonly requestedScope: Readonly<Record<string, unknown>>
    readonly source: StrongFlowRoleApprovalSource
  }
  | {
    readonly schemaVersion: 1
    readonly type: 'strongflow.approval.decided'
    readonly jobId: string
    readonly stageRunId: string
    readonly attemptId: string
    readonly roleId: string
    readonly contextId: string
    readonly operationKind: 'exec' | 'patch'
    readonly operationId: string
    readonly requestedScope: Readonly<Record<string, unknown>>
    readonly decision: StrongFlowRoleApprovalOutcome
    readonly source: StrongFlowRoleApprovalSource
  }

export interface StrongFlowRoleApprovalAuditSink {
  append(event: StrongFlowRoleApprovalAuditEvent): Promise<void> | void
}

export interface StrongFlowRoleAuthorityKernelPort {
  resolveApproval(response: ApprovalResponse): Promise<string>
  resolveDynamicTool(response: DynamicToolResponse): Promise<string>
}

export interface StrongFlowGovernedRoleContextInstallerOptions {
  readonly kernel: StrongFlowRoleAuthorityKernelPort
  readonly tools: StrongFlowRoleToolExecutor
  readonly approvals: StrongFlowRoleApprovalInteraction
  readonly approvalAudit: StrongFlowRoleApprovalAuditSink
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
  label: string,
): void {
  const allowed = new Set([...required, ...optional])
  if (
    required.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !allowed.has(key))
  ) throw new StrongFlowRoleAuthorityError(
    'TOOL_ARGUMENT_INVALID',
    `${label} has an unexpected shape`,
  )
}

function nonEmptyText(value: unknown, label: string): string {
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.trim() !== value
    || /\u0000/u.test(value)
  ) throw new StrongFlowRoleAuthorityError(
    'TOOL_ARGUMENT_INVALID',
    `${label} must be non-empty text`,
  )
  return value
}

function frozenRecord(value: Record<string, unknown>): Readonly<Record<string, unknown>> {
  const clone = structuredClone(value)
  const pending: object[] = [clone]
  while (pending.length > 0) {
    const current = pending.pop()
    if (current === undefined || Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function permissionFor(context: StrongFlowRoleSessionContext): StrongFlowPermissionPolicy {
  const policy = strongFlowPermissionPolicyForPreset(context.roleSpec.permissionPreset)
  if (
    policy.subject !== 'model-role'
    || context.roleSpec.workspaceMode !== context.workspace.mode
    || context.roleSpec.id !== context.workspace.roleId
  ) throw new StrongFlowRoleAuthorityError(
    'INVALID_AUTHORITY_CONTEXT',
    `role ${context.roleSpec.id} does not match its immutable workspace and policy`,
  )
  return policy
}

/** Build the only native authority envelope accepted for this immutable role context. */
export function createStrongFlowRoleKernelAuthority(
  context: StrongFlowRoleSessionContext,
): GovernedSessionAuthority {
  const policy = permissionFor(context)
  return Object.freeze({
    schemaVersion: GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION,
    roleId: context.roleSpec.id,
    permissionPreset: context.roleSpec.permissionPreset,
    workspaceMode: context.roleSpec.workspaceMode,
    workspaceRoot: context.workspace.path,
    systemInstructions: context.roleSpec.systemInstructions,
    reasoningEffort: context.roleSpec.reasoningEffort,
    visibleTools: Object.freeze([...policy.tools.allowed]),
  })
}

/** Reject a newly started thread unless every kernel-observed role setting is exact. */
export function verifyStrongFlowRoleKernelEvidence(
  context: StrongFlowRoleSessionContext,
  evidence: GovernedSessionEffectivePolicy | undefined,
): GovernedSessionEffectivePolicy {
  const authority = createStrongFlowRoleKernelAuthority(context)
  const policy = permissionFor(context)
  const filesystem = policy.filesystem.mode === 'candidate-write'
    ? 'managed-workspace-write'
    : 'managed-read-only'
  if (
    evidence === undefined
    || evidence.schemaVersion !== GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION
    || evidence.authority !== 'codex-core'
    || evidence.roleId !== authority.roleId
    || evidence.permissionPreset !== authority.permissionPreset
    || evidence.workspaceMode !== authority.workspaceMode
    || resolve(evidence.workspaceRoot) !== resolve(authority.workspaceRoot)
    || evidence.visibleTools.length !== authority.visibleTools.length
    || evidence.visibleTools.some((tool, index) => tool !== authority.visibleTools[index])
    || evidence.filesystem !== filesystem
    || evidence.network !== 'restricted'
    || evidence.process !== 'dynamic-tools-only'
    || evidence.environment !== 'empty'
    || evidence.approvalPolicy !== 'on-request'
    || evidence.approvalsReviewer !== 'user'
    || evidence.loginShell !== false
    || evidence.environmentSelections.length !== 0
    || evidence.instructionSources.length !== 0
  ) throw new StrongFlowRoleAuthorityError(
    'ENFORCEMENT_UNAVAILABLE',
    `embedded Codex did not prove the complete authority for role ${context.roleSpec.id}`,
  )
  return evidence
}

function eventMessage(event: StrongFlowRoleKernelEvent): Record<string, unknown> {
  const payload = event.event.payload
  if (!isRecord(payload)) {
    throw new StrongFlowRoleAuthorityError('INVALID_KERNEL_EVENT', 'kernel event is not an object')
  }
  const message = isRecord(payload.msg) ? payload.msg : payload
  if (message.type !== event.event.kind) {
    throw new StrongFlowRoleAuthorityError(
      'INVALID_KERNEL_EVENT',
      `kernel event kind ${event.event.kind} differs from its payload`,
    )
  }
  return message
}

async function validatedToolArguments(
  tool: StrongFlowRoleTool,
  value: unknown,
  context: StrongFlowRoleSessionContext,
): Promise<{
  readonly arguments: Readonly<Record<string, unknown>>
  readonly resolvedWorkspacePaths: readonly string[]
}> {
  if (!isRecord(value)) {
    throw new StrongFlowRoleAuthorityError(
      'TOOL_ARGUMENT_INVALID',
      `tool ${tool} arguments must be an object`,
    )
  }
  const resolvedPaths: string[] = []
  switch (tool) {
    case 'artifact.read':
      exactKeys(value, ['artifactId'], [], tool)
      nonEmptyText(value.artifactId, `${tool}.artifactId`)
      break
    case 'artifact.write':
      exactKeys(value, ['kind', 'artifact'], [], tool)
      if (!context.roleSpec.requiredOutputArtifacts.includes(
        nonEmptyText(value.kind, `${tool}.kind`) as StrongFlowRoleArtifactKind,
      )) {
        throw new StrongFlowRoleAuthorityError(
          'TOOL_DENIED',
          `${tool}.kind is outside the assigned role output contract`,
        )
      }
      if (!isRecord(value.artifact)) {
        throw new StrongFlowRoleAuthorityError(
          'TOOL_ARGUMENT_INVALID',
          `${tool}.artifact must be an object`,
        )
      }
      break
    case 'workspace.read': {
      exactKeys(value, ['path'], [], tool)
      const path = nonEmptyText(value.path, `${tool}.path`)
      resolvedPaths.push(await resolveExistingStrongFlowWorkspacePath(context.workspace.path, path))
      break
    }
    case 'code.search': {
      exactKeys(value, ['query'], ['paths'], tool)
      nonEmptyText(value.query, `${tool}.query`)
      if (value.paths !== undefined) {
        if (!Array.isArray(value.paths)) {
          throw new StrongFlowRoleAuthorityError(
            'TOOL_ARGUMENT_INVALID',
            `${tool}.paths must be an array`,
          )
        }
        for (const [index, pathInput] of value.paths.entries()) {
          const path = nonEmptyText(pathInput, `${tool}.paths[${index}]`)
          resolvedPaths.push(await resolveExistingStrongFlowWorkspacePath(
            context.workspace.path,
            path,
          ))
        }
      }
      break
    }
    case 'candidate.diff': {
      exactKeys(value, [], ['path'], tool)
      if (value.path !== undefined) {
        const path = nonEmptyText(value.path, `${tool}.path`)
        resolvedPaths.push(await resolveExistingStrongFlowWorkspacePath(
          context.workspace.path,
          path,
        ))
      }
      break
    }
    case 'command.run':
    case 'test.run': {
      exactKeys(value, ['argv'], ['cwd'], tool)
      if (
        !Array.isArray(value.argv)
        || value.argv.length === 0
        || value.argv.some(argument => (
          typeof argument !== 'string' || argument.length === 0 || /\u0000/u.test(argument)
        ))
      ) throw new StrongFlowRoleAuthorityError(
        'TOOL_ARGUMENT_INVALID',
        `${tool}.argv must contain non-empty command arguments`,
      )
      if (value.cwd !== undefined) {
        const path = nonEmptyText(value.cwd, `${tool}.cwd`)
        resolvedPaths.push(await resolveExistingStrongFlowWorkspacePath(
          context.workspace.path,
          path,
        ))
      } else {
        resolvedPaths.push(context.workspace.path)
      }
      break
    }
    case 'candidate.patch': {
      exactKeys(value, ['path', 'patch'], [], tool)
      const path = nonEmptyText(value.path, `${tool}.path`)
      nonEmptyText(value.patch, `${tool}.patch`)
      resolvedPaths.push(await resolveStrongFlowWorkspaceWritePath(context.workspace.path, path))
      break
    }
  }
  return Object.freeze({
    arguments: frozenRecord(value),
    resolvedWorkspacePaths: Object.freeze(resolvedPaths),
  })
}

function toolOutputText(value: unknown): string {
  const text = typeof value === 'string' ? value : JSON.stringify(value)
  if (text === undefined || Buffer.byteLength(text) > MAX_TOOL_OUTPUT_BYTES) {
    throw new StrongFlowRoleAuthorityError(
      'TOOL_EXECUTION_FAILED',
      'StrongFlow tool output is empty or exceeds its bounded result size',
    )
  }
  return text
}

function sourceFor(
  event: StrongFlowRoleKernelEvent,
  turnId: string | undefined,
): StrongFlowRoleApprovalSource {
  return Object.freeze({
    authority: 'codex-core',
    kernelSessionLineageId: event.kernelSessionLineageId,
    kernelSessionId: event.kernelSessionId,
    kernelStreamId: event.kernelStreamId,
    kernelSequence: event.event.sequence.toString(),
    ...(turnId === undefined ? {} : { turnId }),
  })
}

function redactedScope(value: Record<string, unknown>): Readonly<Record<string, unknown>> {
  const walk = (input: unknown, key = ''): unknown => {
    if (SENSITIVE_KEY.test(key)) return '[REDACTED]'
    if (Array.isArray(input)) return input.map(entry => walk(entry))
    if (isRecord(input)) {
      return Object.fromEntries(Object.entries(input).map(([childKey, child]) => (
        [childKey, walk(child, childKey)]
      )))
    }
    if (typeof input === 'string') return input.length > 16_384
      ? `${input.slice(0, 16_384)}…`
      : input
    if (typeof input === 'number' || typeof input === 'boolean' || input === null) return input
    return String(input)
  }
  const result = walk(value)
  if (!isRecord(result)) {
    throw new StrongFlowRoleAuthorityError('INVALID_KERNEL_EVENT', 'approval scope is invalid')
  }
  const serialized = JSON.stringify(result)
  if (Buffer.byteLength(serialized) > MAX_APPROVAL_SCOPE_BYTES) {
    return Object.freeze({ summary: 'Approval scope exceeded the durable audit size limit.' })
  }
  return frozenRecord(result)
}

function approvalDecision(outcome: StrongFlowRoleApprovalOutcome): ApprovalDecision {
  switch (outcome) {
    case 'approved': return Object.freeze({ kind: 'approved' })
    case 'cancelled': return Object.freeze({ kind: 'abort' })
    case 'rejected':
      return Object.freeze({ kind: 'denied', rejection: 'The human reviewer rejected this operation.' })
    case 'unavailable':
      return Object.freeze({ kind: 'denied', rejection: 'No DSH approval answerer was available.' })
  }
}

class GovernedInstallation implements StrongFlowRoleContextInstallation {
  readonly contextId
  readonly #request: StrongFlowRoleContextInstallationRequest
  readonly #policy: StrongFlowPermissionPolicy
  readonly #options: StrongFlowGovernedRoleContextInstallerOptions
  readonly #handled = new Set<string>()
  #disposed = false

  constructor(
    request: StrongFlowRoleContextInstallationRequest,
    options: StrongFlowGovernedRoleContextInstallerOptions,
  ) {
    this.contextId = request.context.contextId
    this.#request = request
    this.#policy = permissionFor(request.context)
    this.#options = options
  }

  async handleEvent(event: StrongFlowRoleKernelEvent): Promise<void> {
    if (this.#disposed) {
      throw new StrongFlowRoleAuthorityError(
        'INSTALLATION_DISPOSED',
        `role authority ${this.contextId} is already disposed`,
      )
    }
    if (
      event.contextId !== this.contextId
      || event.kernelSessionId !== this.#request.kernel.kernelSessionId
      || event.kernelStreamId !== this.#request.kernel.kernelStreamId
    ) throw new StrongFlowRoleAuthorityError(
      'INVALID_KERNEL_EVENT',
      'kernel event does not belong to this installed role authority',
    )
    if (event.event.kind === 'dynamic_tool_call_request') {
      await this.#handleTool(event)
    } else if (
      event.event.kind === 'exec_approval_request'
      || event.event.kind === 'apply_patch_approval_request'
    ) {
      await this.#handleApproval(event)
    }
  }

  dispose(): void {
    this.#disposed = true
    this.#handled.clear()
  }

  async #handleTool(event: StrongFlowRoleKernelEvent): Promise<void> {
    const message = eventMessage(event)
    const callId = nonEmptyText(message.call_id, 'dynamic-tool call id')
    const turnId = nonEmptyText(message.turn_id, 'dynamic-tool turn id')
    const namespace = nonEmptyText(message.namespace, 'dynamic-tool namespace')
    const name = nonEmptyText(message.tool, 'dynamic-tool name')
    const qualified = `${namespace}.${name}`
    const key = `tool:${callId}`
    if (this.#handled.has(key)) {
      throw new StrongFlowRoleAuthorityError(
        'INVALID_KERNEL_EVENT',
        `dynamic-tool call ${callId} was already handled`,
      )
    }
    this.#handled.add(key)
    const allowed = this.#policy.tools.allowed.includes(qualified as StrongFlowRoleTool)
    if (!allowed || !STRONGFLOW_ROLE_TOOLS.includes(qualified as StrongFlowRoleTool)) {
      await this.#options.kernel.resolveDynamicTool({
        sessionId: event.kernelSessionId,
        callId,
        success: false,
        text: `StrongFlow denied tool ${qualified} for role ${this.#request.context.roleSpec.id}.`,
      })
      return
    }
    let response: Omit<DynamicToolResponse, 'sessionId' | 'callId'>
    try {
      const validated = await validatedToolArguments(
        qualified as StrongFlowRoleTool,
        message.arguments,
        this.#request.context,
      )
      const output = await this.#options.tools.execute(Object.freeze({
        jobId: this.#request.context.jobId,
        stageRunId: this.#request.context.stageRunId,
        attemptId: this.#request.context.attemptId,
        roleId: this.#request.context.roleSpec.id,
        contextId: this.#request.context.contextId,
        kernelSessionLineageId: this.#request.context.kernelSessionLineageId,
        kernelSessionId: event.kernelSessionId,
        kernelStreamId: event.kernelStreamId,
        kernelSequence: event.event.sequence.toString(),
        turnId,
        callId,
        tool: qualified as StrongFlowRoleTool,
        arguments: validated.arguments,
        resolvedWorkspacePaths: validated.resolvedWorkspacePaths,
        signal: this.#request.signal,
      }))
      response = {
        success: true,
        text: toolOutputText(output),
      }
    } catch (error) {
      response = {
        success: false,
        text: error instanceof StrongFlowRoleAuthorityError
          ? `StrongFlow denied ${qualified}: ${error.message}`
          : `StrongFlow tool ${qualified} failed inside its host executor.`,
      }
    }
    await this.#options.kernel.resolveDynamicTool({
      sessionId: event.kernelSessionId,
      callId,
      ...response,
    })
  }

  async #handleApproval(event: StrongFlowRoleKernelEvent): Promise<void> {
    const message = eventMessage(event)
    const operationKind = event.event.kind === 'exec_approval_request' ? 'exec' : 'patch'
    const operationId = nonEmptyText(
      message.approval_id ?? message.call_id ?? message.id,
      'approval operation id',
    )
    const turnId = message.turn_id === undefined
      ? undefined
      : nonEmptyText(message.turn_id, 'approval turn id')
    const key = `approval:${operationKind}:${operationId}`
    if (this.#handled.has(key)) {
      throw new StrongFlowRoleAuthorityError(
        'INVALID_KERNEL_EVENT',
        `approval ${operationId} was already handled`,
      )
    }
    this.#handled.add(key)
    const requestedScope = redactedScope(message)
    const source = sourceFor(event, turnId)
    const common = Object.freeze({
      schemaVersion: 1 as const,
      jobId: this.#request.context.jobId,
      stageRunId: this.#request.context.stageRunId,
      attemptId: this.#request.context.attemptId,
      roleId: this.#request.context.roleSpec.id,
      contextId: this.#request.context.contextId,
      operationKind,
      operationId,
      requestedScope,
      source,
    })
    try {
      await this.#options.approvalAudit.append(Object.freeze({
        ...common,
        type: 'strongflow.approval.requested',
      }))
      const outcome = await this.#options.approvals.request(Object.freeze({
        jobId: common.jobId,
        stageRunId: common.stageRunId,
        attemptId: common.attemptId,
        roleId: common.roleId,
        contextId: common.contextId,
        operationKind,
        operationId,
        requestedScope,
        source,
        signal: this.#request.signal,
      }))
      if (!['approved', 'rejected', 'cancelled', 'unavailable'].includes(outcome)) {
        throw new Error('DSH approval answerer returned an unknown outcome')
      }
      await this.#options.approvalAudit.append(Object.freeze({
        ...common,
        type: 'strongflow.approval.decided',
        decision: outcome,
      }))
      const response: ApprovalResponse = Object.freeze({
        sessionId: event.kernelSessionId,
        kind: operationKind,
        operationId,
        ...(turnId === undefined ? {} : { turnId }),
        decision: approvalDecision(outcome),
      })
      await this.#options.kernel.resolveApproval(response)
    } catch (error) {
      try {
        await this.#options.kernel.resolveApproval({
          sessionId: event.kernelSessionId,
          kind: operationKind,
          operationId,
          ...(turnId === undefined ? {} : { turnId }),
          decision: Object.freeze({
            kind: 'denied',
            rejection: 'StrongFlow could not obtain and audit a human decision.',
          }),
        })
      } catch {
        // The original audit/interaction failure remains the authoritative setup failure.
      }
      throw new StrongFlowRoleAuthorityError(
        'APPROVAL_AUDIT_FAILED',
        `approval ${operationId} could not be decided through the DSH audit surface`,
        { cause: error },
      )
    }
  }
}

/** Installer that binds post-start event handling after native policy evidence is accepted. */
export class StrongFlowGovernedRoleContextInstaller implements StrongFlowRoleContextInstaller {
  readonly #options: StrongFlowGovernedRoleContextInstallerOptions

  constructor(options: StrongFlowGovernedRoleContextInstallerOptions) {
    if (
      !isRecord(options)
      || typeof options.kernel?.resolveApproval !== 'function'
      || typeof options.kernel?.resolveDynamicTool !== 'function'
      || typeof options.tools?.execute !== 'function'
      || typeof options.approvals?.request !== 'function'
      || typeof options.approvalAudit?.append !== 'function'
    ) throw new StrongFlowRoleAuthorityError(
      'INVALID_AUTHORITY_CONTEXT',
      'governed role installer requires kernel, tool, DSH approval, and audit ports',
    )
    this.#options = options
  }

  async install(
    request: StrongFlowRoleContextInstallationRequest,
  ): Promise<StrongFlowRoleContextInstallation> {
    verifyStrongFlowRoleKernelEvidence(request.context, request.kernel.effectivePolicy)
    return new GovernedInstallation(request, this.#options)
  }
}

export function strongFlowRoleKernelLifecycleHasCompleteEvidence(
  context: StrongFlowRoleSessionContext,
  kernel: StrongFlowRoleKernelLifecycle,
): boolean {
  try {
    verifyStrongFlowRoleKernelEvidence(context, kernel.effectivePolicy)
    return true
  } catch {
    return false
  }
}
