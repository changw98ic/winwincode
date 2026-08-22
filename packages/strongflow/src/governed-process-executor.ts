import { createHash } from 'node:crypto'
import { resolve } from 'node:path'

import type {
  GovernedCommandRequest,
  GovernedCommandResult,
} from '@winwincode/native'

import {
  StrongFlowRoleToolExecutionError,
  type StrongFlowRoleToolExecutionRequest,
  type StrongFlowRoleToolExecutor,
} from './role-authority.js'
import {
  strongFlowSecurityDigestText,
  type StrongFlowSecurityAuditEvent,
  type StrongFlowSecurityAuditSink,
  type StrongFlowSecurityAuditSource,
} from './security-audit.js'

export const STRONGFLOW_PROCESS_GRANT_SCHEMA_VERSION = 1 as const

const PROCESS_TOOLS = Object.freeze(['command.run', 'test.run'] as const)
const SAFE_IDENTIFIER = /^[A-Za-z0-9_.:-]{1,200}$/u
const SAFE_LOCALE = /^[A-Za-z0-9._@-]{1,64}$/u
const SENSITIVE_ARGUMENT = /^(?:Bearer\s|api_?key=|apikey=|authorization=|password=|secret=|token=)/iu

export interface StrongFlowProcessGrant {
  readonly schemaVersion: typeof STRONGFLOW_PROCESS_GRANT_SCHEMA_VERSION
  readonly grantId: string
  readonly jobId: string
  readonly stageRunId: string
  readonly attemptId: string
  readonly roleId: string
  readonly contextId: string
  readonly kernelSessionId: string
  readonly tool: 'command.run' | 'test.run'
  readonly argv: readonly string[]
  readonly cwd: string
  readonly environment: Readonly<Record<string, string>>
  readonly timeoutMillis: number
  readonly outputLimitBytes: number
}

/** Trusted host seam that resolves an exact approved plan, snapshot probe, or remediation grant. */
export interface StrongFlowProcessGrantAuthorizer {
  authorize(request: StrongFlowRoleToolExecutionRequest): Promise<StrongFlowProcessGrant | null>
}

export interface StrongFlowGovernedCommandKernelPort {
  executeGovernedCommand(request: GovernedCommandRequest): Promise<GovernedCommandResult>
  cancelGovernedCommand(sessionId: string, commandId: string): Promise<void>
}

export interface StrongFlowGovernedProcessExecutorOptions {
  readonly kernel: StrongFlowGovernedCommandKernelPort
  readonly grants: StrongFlowProcessGrantAuthorizer
  readonly delegate: StrongFlowRoleToolExecutor
  readonly securityAudit: StrongFlowSecurityAuditSink
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function frozenGrant(grant: StrongFlowProcessGrant): StrongFlowProcessGrant {
  return Object.freeze({
    ...grant,
    argv: Object.freeze([...grant.argv]),
    environment: Object.freeze({ ...grant.environment }),
  })
}

function validGrantShape(grant: StrongFlowProcessGrant): boolean {
  return isRecord(grant)
    && grant.schemaVersion === STRONGFLOW_PROCESS_GRANT_SCHEMA_VERSION
    && SAFE_IDENTIFIER.test(grant.grantId)
    && [
      grant.jobId,
      grant.stageRunId,
      grant.attemptId,
      grant.roleId,
      grant.contextId,
      grant.kernelSessionId,
    ].every(value => typeof value === 'string' && value.length > 0)
    && PROCESS_TOOLS.includes(grant.tool)
    && Array.isArray(grant.argv)
    && grant.argv.length > 0
    && grant.argv.every(argument => (
      typeof argument === 'string'
      && argument.length > 0
      && !argument.includes('\u0000')
      && !SENSITIVE_ARGUMENT.test(argument)
    ))
    && typeof grant.cwd === 'string'
    && grant.cwd.length > 0
    && isRecord(grant.environment)
    && !Array.isArray(grant.environment)
    && Object.entries(grant.environment).every(([name, value]) => (
      (name === 'LANG' || name === 'LC_ALL')
      && typeof value === 'string'
      && SAFE_LOCALE.test(value)
    ))
    && Number.isSafeInteger(grant.timeoutMillis)
    && grant.timeoutMillis > 0
    && grant.timeoutMillis <= 600_000
    && Number.isSafeInteger(grant.outputLimitBytes)
    && grant.outputLimitBytes > 0
    && grant.outputLimitBytes <= 8 * 1024 * 1024
}

function processArguments(request: StrongFlowRoleToolExecutionRequest): {
  readonly argv: readonly string[]
  readonly cwd: string
} {
  if (
    !PROCESS_TOOLS.includes(request.tool as typeof PROCESS_TOOLS[number])
    || !Array.isArray(request.arguments.argv)
    || request.arguments.argv.some(argument => typeof argument !== 'string')
    || request.resolvedWorkspacePaths.length !== 1
  ) throw new StrongFlowRoleToolExecutionError(
    'policy-denied',
    'PROCESS_REQUEST_INVALID',
    'process tool request did not preserve its validated argv and cwd',
  )
  return Object.freeze({
    argv: Object.freeze([...request.arguments.argv] as string[]),
    cwd: resolve(request.resolvedWorkspacePaths[0] as string),
  })
}

function exactGrant(
  grant: StrongFlowProcessGrant,
  request: StrongFlowRoleToolExecutionRequest,
  argv: readonly string[],
  cwd: string,
): StrongFlowProcessGrant {
  if (
    !validGrantShape(grant)
    || grant.jobId !== request.jobId
    || grant.stageRunId !== request.stageRunId
    || grant.attemptId !== request.attemptId
    || grant.roleId !== request.roleId
    || grant.contextId !== request.contextId
    || grant.kernelSessionId !== request.kernelSessionId
    || grant.tool !== request.tool
    || resolve(grant.cwd) !== cwd
    || grant.argv.length !== argv.length
    || grant.argv.some((argument, index) => argument !== argv[index])
  ) throw new StrongFlowRoleToolExecutionError(
    'policy-denied',
    'PROCESS_GRANT_MISMATCH',
    'process grant differs from the exact governed tool request',
  )
  return frozenGrant(grant)
}

function sourceFor(
  request: StrongFlowRoleToolExecutionRequest,
  operationId: string,
): StrongFlowSecurityAuditSource {
  return Object.freeze({
    authority: 'codex-core',
    kernelSessionLineageId: request.kernelSessionLineageId,
    kernelSessionId: request.kernelSessionId,
    kernelStreamId: request.kernelStreamId,
    kernelSequence: request.kernelSequence,
    turnId: request.turnId,
    operationId,
  })
}

function commandId(request: StrongFlowRoleToolExecutionRequest, grantId: string): string {
  return `command-${createHash('sha256').update(JSON.stringify([
    request.kernelSessionId,
    request.turnId,
    request.callId,
    grantId,
  ])).digest('hex')}`
}

function outputFacts(result: GovernedCommandResult): Readonly<Record<string, unknown>> {
  return Object.freeze({
    status: result.status,
    exitCode: result.exitCode ?? null,
    sandbox: result.sandbox,
    network: result.network,
    environmentNames: result.environmentNames,
    stdoutSha256: strongFlowSecurityDigestText(result.stdout),
    stdoutBytes: Buffer.byteLength(result.stdout),
    stderrSha256: strongFlowSecurityDigestText(result.stderr),
    stderrBytes: Buffer.byteLength(result.stderr),
  })
}

function nativeFailure(error: unknown): StrongFlowRoleToolExecutionError {
  const code = isRecord(error) && typeof error.code === 'string'
    ? error.code
    : 'GOVERNED_COMMAND_FAILED'
  if (code === 'GOVERNED_COMMAND_POLICY_DENIED' || code === 'INVALID_GOVERNED_COMMAND') {
    return new StrongFlowRoleToolExecutionError(
      'policy-denied',
      code,
      'native governed command policy rejected the request',
      { cause: error },
    )
  }
  return new StrongFlowRoleToolExecutionError(
    'sandbox-denied',
    code,
    'native governed command sandbox was unavailable or could not start',
    { cause: error },
  )
}

/** Immutable exact-grant catalog populated only from trusted StrongFlow artifacts. */
export class StrongFlowExactProcessGrantAuthorizer implements StrongFlowProcessGrantAuthorizer {
  readonly #grants: readonly StrongFlowProcessGrant[]

  constructor(grants: readonly StrongFlowProcessGrant[]) {
    if (!Array.isArray(grants) || grants.some(grant => !validGrantShape(grant))) {
      throw new StrongFlowRoleToolExecutionError(
        'policy-denied',
        'PROCESS_GRANT_INVALID',
        'process grant catalog contains an invalid grant',
      )
    }
    this.#grants = Object.freeze(grants.map(frozenGrant))
  }

  authorize(request: StrongFlowRoleToolExecutionRequest): Promise<StrongFlowProcessGrant | null> {
    const requested = processArguments(request)
    const grant = this.#grants.find(candidate => (
      candidate.jobId === request.jobId
      && candidate.stageRunId === request.stageRunId
      && candidate.attemptId === request.attemptId
      && candidate.roleId === request.roleId
      && candidate.contextId === request.contextId
      && candidate.kernelSessionId === request.kernelSessionId
      && candidate.tool === request.tool
      && resolve(candidate.cwd) === requested.cwd
      && candidate.argv.length === requested.argv.length
      && candidate.argv.every((argument, index) => argument === requested.argv[index])
    ))
    return Promise.resolve(grant ?? null)
  }
}

/** Routes only exact trusted process grants into the native macOS/Linux sandbox. */
export class StrongFlowGovernedProcessExecutor implements StrongFlowRoleToolExecutor {
  readonly #options: StrongFlowGovernedProcessExecutorOptions

  constructor(options: StrongFlowGovernedProcessExecutorOptions) {
    if (
      !isRecord(options)
      || typeof options.kernel?.executeGovernedCommand !== 'function'
      || typeof options.kernel?.cancelGovernedCommand !== 'function'
      || typeof options.grants?.authorize !== 'function'
      || typeof options.delegate?.execute !== 'function'
      || typeof options.securityAudit?.append !== 'function'
    ) throw new StrongFlowRoleToolExecutionError(
      'policy-denied',
      'PROCESS_EXECUTOR_INVALID',
      'governed process executor requires kernel, grant, delegate, and audit ports',
    )
    this.#options = options
  }

  async execute(request: StrongFlowRoleToolExecutionRequest): Promise<unknown> {
    if (!PROCESS_TOOLS.includes(request.tool as typeof PROCESS_TOOLS[number])) {
      return this.#options.delegate.execute(request)
    }
    const requested = processArguments(request)
    let grant: StrongFlowProcessGrant | null
    try {
      grant = await this.#options.grants.authorize(request)
    } catch (error) {
      throw new StrongFlowRoleToolExecutionError(
        'policy-denied',
        'PROCESS_AUTHORIZATION_FAILED',
        'trusted process grant authorizer did not return a decision',
        { cause: error },
      )
    }
    const source = sourceFor(request, grant?.grantId ?? request.callId)
    if (grant === null) {
      await this.#audit(
        request,
        'strongflow.security.process.denied',
        'policy-denied',
        source,
        Object.freeze({ tool: request.tool, code: 'PROCESS_GRANT_REQUIRED' }),
      )
      throw new StrongFlowRoleToolExecutionError(
        'policy-denied',
        'PROCESS_GRANT_REQUIRED',
        'no exact trusted process grant matched this tool request',
      )
    }
    const accepted = exactGrant(grant, request, requested.argv, requested.cwd)
    const id = commandId(request, accepted.grantId)
    const commandSource = sourceFor(request, id)
    await this.#audit(
      request,
      'strongflow.security.process.authorized',
      'authorized',
      commandSource,
      Object.freeze({
        grantId: accepted.grantId,
        tool: accepted.tool,
        argvSha256: strongFlowSecurityDigestText(JSON.stringify(accepted.argv)),
        argumentCount: accepted.argv.length,
        cwdSha256: strongFlowSecurityDigestText(resolve(accepted.cwd)),
        environmentNames: Object.keys(accepted.environment).sort(),
        timeoutMillis: accepted.timeoutMillis,
        outputLimitBytes: accepted.outputLimitBytes,
      }),
    )
    if (request.signal.aborted) {
      throw new StrongFlowRoleToolExecutionError(
        'task-failed',
        'PROCESS_CANCELLED',
        'governed process request was cancelled before execution',
      )
    }
    const abort = (): void => {
      void this.#options.kernel.cancelGovernedCommand(request.kernelSessionId, id).catch(
        () => undefined,
      )
    }
    request.signal.addEventListener('abort', abort, { once: true })
    let result: GovernedCommandResult
    try {
      result = await this.#options.kernel.executeGovernedCommand(Object.freeze({
        schemaVersion: 1,
        sessionId: request.kernelSessionId,
        commandId: id,
        tool: accepted.tool,
        argv: accepted.argv,
        cwd: resolve(accepted.cwd),
        environment: accepted.environment,
        timeoutMillis: accepted.timeoutMillis,
        outputLimitBytes: accepted.outputLimitBytes,
      }))
    } catch (error) {
      const failure = nativeFailure(error)
      await this.#audit(
        request,
        'strongflow.security.process.failed',
        failure.kind,
        commandSource,
        Object.freeze({ grantId: accepted.grantId, tool: accepted.tool, code: failure.code }),
      )
      throw failure
    } finally {
      request.signal.removeEventListener('abort', abort)
    }
    const facts = outputFacts(result)
    if (result.status === 'sandbox-denied') {
      await this.#audit(
        request,
        'strongflow.security.process.denied',
        'sandbox-denied',
        commandSource,
        facts,
      )
      throw new StrongFlowRoleToolExecutionError(
        'sandbox-denied',
        'PROCESS_SANDBOX_DENIED',
        'platform sandbox denied the governed command operation',
      )
    }
    if (result.status !== 'exited' || result.exitCode !== 0) {
      await this.#audit(
        request,
        'strongflow.security.process.failed',
        'task-failed',
        commandSource,
        facts,
      )
      throw new StrongFlowRoleToolExecutionError(
        'task-failed',
        result.status === 'exited'
          ? 'PROCESS_EXIT_NONZERO'
          : `PROCESS_${result.status.toUpperCase().replaceAll('-', '_')}`,
        'governed command completed without a successful task result',
      )
    }
    await this.#audit(
      request,
      'strongflow.security.process.completed',
      'completed',
      commandSource,
      facts,
    )
    return Object.freeze({
      status: 'completed',
      exitCode: result.exitCode,
      stdout: result.stdout,
      stderr: result.stderr,
      sandbox: result.sandbox,
      network: result.network,
      environmentNames: result.environmentNames,
    })
  }

  async #audit(
    request: StrongFlowRoleToolExecutionRequest,
    type: StrongFlowSecurityAuditEvent['type'],
    outcome: StrongFlowSecurityAuditEvent['outcome'],
    source: StrongFlowSecurityAuditSource,
    facts: Readonly<Record<string, unknown>>,
  ): Promise<void> {
    try {
      await this.#options.securityAudit.append(Object.freeze({
        schemaVersion: 1,
        type,
        jobId: request.jobId,
        stageRunId: request.stageRunId,
        attemptId: request.attemptId,
        roleId: request.roleId,
        contextId: request.contextId,
        source,
        outcome,
        facts,
      }))
    } catch (error) {
      throw new StrongFlowRoleToolExecutionError(
        'policy-denied',
        'SECURITY_AUDIT_FAILED',
        'required process security fact could not be recorded',
        { cause: error },
      )
    }
  }
}
