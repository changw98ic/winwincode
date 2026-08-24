import { createHash } from 'node:crypto'

import {
  DELIVERY_SCHEMA_VERSION,
  STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
  STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL,
  DeliveryTaskId,
  StageRunId,
  parseAttentionItem,
  parseStrongFlowDeliveryAdvanceRequest,
  parseStrongFlowPlanReviewSolution,
  parseStrongFlowRemediationAnnotations,
  type Delivery,
  type DeliveryStage,
  type FrozenDeliveryCandidate,
  type RuntimeEvent,
  type SessionBinding,
  type StageRun,
  type StrongFlowDeliveryAdvanceErrorCode,
  type StrongFlowDeliveryAdvanceOutcome,
  type StrongFlowDeliveryAdvanceRequest,
  type StrongFlowPlanReviewSolution,
  type StrongFlowRemediationAnnotation,
  type StrongFlowRoleId,
} from '@winwincode/contracts'

import {
  freezeAcceptanceVerificationInput,
} from './acceptance-verification.js'
import {
  DeliveryRuntimeProjection,
} from './delivery-runtime-projection.js'
import {
  StrongFlowService,
  StrongFlowServiceError,
} from './delivery-service.js'
import {
  createStrongFlowGitHubPublicationAttention,
} from './github-publication.js'
import {
  createIndependentVerificationAssignment,
  projectIndependentVerification,
  serializeIndependentVerificationSessionInput,
  type IndependentVerificationAssignment,
} from './independent-verification.js'
import {
  LocalGitDeliveryWorkspace,
  LocalGitDeliveryWorkspaceError,
} from './local-git-delivery-workspace.js'
import {
  assertStrongFlowPlanReviewCurrent,
  createStrongFlowPlanReviewAttention,
} from './plan-review.js'

const PLANNING_RESULT_PROTOCOL = 'winwincode.planning-result.v1' as const
const MAX_STRUCTURED_RESULT_ATTEMPTS = 2

export interface StrongFlowAdvanceModelRoute {
  readonly provider: string
  readonly model: string
  readonly maxTokens?: number
}

export interface StrongFlowAdvanceCaller {
  readonly dshSessionId: string
  readonly modelRoute: StrongFlowAdvanceModelRoute
}

export interface StrongFlowStageRoleSession {
  readonly dshSessionId: string
  readonly codexSessionId: string
  turn(prompt: string, signal?: AbortSignal): Promise<void>
  dispose(): Promise<void>
}

export interface OpenStrongFlowStageRoleSessionInput {
  readonly dshSessionId: string
  readonly role: StrongFlowRoleId
  readonly cwd: string
  readonly modelRoute: StrongFlowAdvanceModelRoute
  readonly signal?: AbortSignal
}

type StrongFlowRecoveryAction =
  | {
      readonly kind: 'resolve-delivery-attention'
      readonly attentionItemIds: readonly string[]
    }
  | {
      readonly kind: 'create-stage-session'
      readonly stageRunId: string
      readonly stage: DeliveryStage
      readonly actorType: StageRun['actorType']
      readonly role: string
    }
  | {
      readonly kind: 'resume-stage-session'
      readonly stageRunId: string
      readonly sessionBindingId: string
      readonly dshSessionId: string
      readonly codexSessionId: string
      readonly rolloutPath: string
      readonly pendingInteractionIds: readonly string[]
    }
  | {
      readonly kind: 'resolve-runtime-interaction'
      readonly stageRunId: string
      readonly sessionBindingId: string
      readonly dshSessionId: string
      readonly codexSessionId: string
      readonly interactionIds: readonly string[]
    }
  | {
      readonly kind: 'review-stage-output'
      readonly stageRunId: string
      readonly sessionBindingId: string
      readonly dshSessionId: string
      readonly codexSessionId: string
      readonly runtimeStatus: 'completed' | 'aborted' | 'failed'
    }
  | {
      readonly kind: 'continue-stage'
      readonly stageRunId: string
      readonly sessionBindingId: string
      readonly dshSessionId: string
      readonly codexSessionId: string
    }
  | {
      readonly kind: 'start-stage'
      readonly stage: DeliveryStage
      readonly deliveryTaskId: string | null
    }
  | { readonly kind: 'delivery-complete' }

export interface StrongFlowStageRuntime {
  reconcileDelivery(
    deliveryId: string,
    candidate: FrozenDeliveryCandidate | null,
  ): Promise<{
    readonly delivery: Delivery
    readonly nextAction: StrongFlowRecoveryAction
  }>
  openRoleSession(input: OpenStrongFlowStageRoleSessionInput): Promise<StrongFlowStageRoleSession>
  readRuntimeSessionEvents(dshSessionId: string): Promise<readonly RuntimeEvent[]>
}

export interface StrongFlowDeliveryStageCoordinatorOptions {
  readonly service: StrongFlowService
  readonly runtime: StrongFlowStageRuntime
  readonly workspace: LocalGitDeliveryWorkspace
}

export interface StrongFlowDeliveryAdvanceResult {
  readonly delivery: Delivery
  readonly outcome: StrongFlowDeliveryAdvanceOutcome
}

export class StrongFlowDeliveryStageCoordinatorError extends Error {
  readonly code: StrongFlowDeliveryAdvanceErrorCode
  readonly currentRevision: number | null

  constructor(
    code: StrongFlowDeliveryAdvanceErrorCode,
    message: string,
    currentRevision: number | null = null,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowDeliveryStageCoordinatorError'
    this.code = code
    this.currentRevision = currentRevision
  }
}

interface PlanningResult {
  readonly solution: StrongFlowPlanReviewSolution
  readonly risks: readonly string[]
  readonly unresolvedItems: readonly string[]
}

function coordinatorError(
  code: StrongFlowDeliveryAdvanceErrorCode,
  message: string,
  currentRevision: number | null = null,
  cause?: unknown,
): never {
  throw new StrongFlowDeliveryStageCoordinatorError(
    code,
    message,
    currentRevision,
    cause === undefined ? undefined : { cause },
  )
}

function immutable<Value>(value: Value): Value {
  const clone = structuredClone(value)
  const pending: object[] = []
  if (typeof clone === 'object' && clone !== null) pending.push(clone)
  while (pending.length > 0) {
    const current = pending.pop()!
    if (Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function exactKeys(value: Record<string, unknown>, keys: readonly string[]): boolean {
  const expected = new Set(keys)
  return Object.keys(value).length === expected.size
    && keys.every(key => Object.hasOwn(value, key))
    && Object.keys(value).every(key => expected.has(key))
}

function boundedStringList(value: unknown): readonly string[] {
  if (!Array.isArray(value)
    || value.length > 200
    || value.some(entry => (
      typeof entry !== 'string' || entry.trim().length === 0 || entry.length > 65_536
    ))
    || new Set(value).size !== value.length) {
    return coordinatorError('STAGE_OUTPUT_INVALID', 'planner returned an invalid string list')
  }
  return Object.freeze(value as string[])
}

function digestIdentity(delivery: Delivery, kind: string, ordinal = 1): string {
  const digest = createHash('sha256').update(JSON.stringify({
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    deliverySpecRevision: delivery.spec.revision,
    kind,
    ordinal,
  })).digest('hex').slice(0, 32)
  return `sf-${kind}-${digest}`
}

function requestId(request: StrongFlowDeliveryAdvanceRequest, suffix: string): string {
  return `${request.requestId.slice(0, 420)}:${suffix.slice(0, 70)}`
}

function outcome(
  kind: StrongFlowDeliveryAdvanceOutcome['kind'],
  message: string,
  stageRunId: string | null = null,
  dshSessionId: string | null = null,
): StrongFlowDeliveryAdvanceOutcome {
  return immutable({
    kind,
    message,
    stageRunId: stageRunId === null ? null : StageRunId(stageRunId),
    dshSessionId,
  })
}

function signalOptions(signal: AbortSignal | undefined): { readonly signal?: AbortSignal } {
  return signal === undefined ? {} : { signal }
}

function runFor(delivery: Delivery, stageRunId: string): StageRun {
  const run = delivery.stageRuns.find(entry => entry.id === stageRunId)
  if (run === undefined) {
    return coordinatorError('DELIVERY_STATE_CONFLICT', 'next stage no longer exists')
  }
  return run
}

function bindingFor(delivery: Delivery, run: StageRun): SessionBinding | null {
  return delivery.sessionBindings.find(entry => entry.stageRunId === run.id) ?? null
}

function strongFlowRole(value: string): StrongFlowRoleId {
  if (value === 'planner'
    || value === 'executor'
    || value === 'reviewer'
    || value === 'verifier'
    || value === 'adversarial-verifier'
    || value === 'remediator'
    || value === 'requirements'
    || value === 'solution') return value
  return coordinatorError('DELIVERY_STATE_CONFLICT', `stage role ${value} is unsupported`)
}

function nestedRecord(
  value: Readonly<Record<string, unknown>>,
  key: string,
): Readonly<Record<string, unknown>> | undefined {
  const nested = value[key]
  return isRecord(nested) ? nested : undefined
}

function messageText(event: RuntimeEvent): string | null {
  if (typeof event.data.message === 'string' && event.data.message.length > 0) {
    return event.data.message
  }
  const content = nestedRecord(event.data, 'item')?.content
  if (!Array.isArray(content)) return null
  const text = content.flatMap(entry => (
    isRecord(entry) && typeof entry.text === 'string' ? [entry.text] : []
  )).join('')
  return text.length === 0 ? null : text
}

function assistantMessage(event: RuntimeEvent): boolean {
  if (event.kind !== 'message.completed') return false
  if (event.data.type === 'agent_message') return true
  return nestedRecord(event.data, 'item')?.type === 'AgentMessage'
}

function latestTurnEvents(events: readonly RuntimeEvent[]): readonly RuntimeEvent[] {
  const latestStart = events
    .filter(event => event.kind === 'turn.started')
    .reduce((latest, event) => {
      const sequence = BigInt(event.cursor.sequence)
      return sequence > latest ? sequence : latest
    }, 0n)
  return Object.freeze(events.filter(event => BigInt(event.cursor.sequence) >= latestStart))
}

function assertCompletedTurn(events: readonly RuntimeEvent[], role: string): void {
  const current = latestTurnEvents(events)
  const terminal = current.findLast(event => (
    event.kind === 'turn.completed' || event.kind === 'turn.aborted'
  ))
  const failed = current.some(event => event.kind === 'failure')
  if (terminal?.kind !== 'turn.completed'
    || terminal.terminalReason !== 'completed'
    || failed) {
    return coordinatorError(
      'STAGE_OUTPUT_INVALID',
      `${role} Session did not finish one successful Codex turn`,
    )
  }
}

function planningResult(events: readonly RuntimeEvent[]): PlanningResult {
  assertCompletedTurn(events, 'planner')
  const text = latestTurnEvents(events).findLast(assistantMessage)
  const raw = text === undefined ? null : messageText(text)
  if (raw === null) {
    return coordinatorError('STAGE_OUTPUT_INVALID', 'planner returned no final response')
  }
  let parsed: unknown
  try {
    parsed = JSON.parse(raw)
  } catch (error) {
    return coordinatorError(
      'STAGE_OUTPUT_INVALID',
      'planner final response is not strict JSON',
      null,
      error,
    )
  }
  if (!isRecord(parsed)
    || !exactKeys(parsed, ['protocol', 'solution', 'risks', 'unresolvedItems'])
    || parsed.protocol !== PLANNING_RESULT_PROTOCOL) {
    return coordinatorError(
      'STAGE_OUTPUT_INVALID',
      'planner final response does not follow winwincode.planning-result.v1',
    )
  }
  try {
    return immutable({
      solution: parseStrongFlowPlanReviewSolution(parsed.solution),
      risks: boundedStringList(parsed.risks),
      unresolvedItems: boundedStringList(parsed.unresolvedItems),
    })
  } catch (error) {
    if (error instanceof StrongFlowDeliveryStageCoordinatorError) throw error
    return coordinatorError(
      'STAGE_OUTPUT_INVALID',
      'planner returned an invalid structured solution',
      null,
      error,
    )
  }
}

function planningPrompt(delivery: Delivery): string {
  return [
    JSON.stringify({
      protocol: 'winwincode.planning.v1',
      deliverySpec: delivery.spec,
      instruction: [
        'Use Codex update_plan and multi-agent collaboration only when useful.',
        'Prepare the exact implementation plan and one structured solution for human review.',
        'Do not modify the repository and do not create another task authority.',
      ],
    }),
    '',
    'Return the final response as one plain JSON object and no markdown.',
    `Protocol: ${PLANNING_RESULT_PROTOCOL}.`,
    'Exact top-level fields: protocol, solution, risks, unresolvedItems.',
    'solution exact fields: id, summary, approach, components, connections.',
    'Each component exact fields: id, label, responsibility, kind, trustBoundary, unresolved, repositoryPathPrefixes.',
    'Each connection exact fields: id, from, to, label.',
    'Connection endpoints may use platform:dsh, platform:strongflow, platform:codex-core, platform:repository, or a component id.',
  ].join('\n')
}

function planningCorrectionPrompt(): string {
  return [
    'Correct the preceding planner result now.',
    'Return one strict JSON object and no markdown or explanation.',
    `Use protocol ${PLANNING_RESULT_PROTOCOL} and exact fields protocol, solution, risks, unresolvedItems.`,
    'Do not change repository files.',
  ].join('\n')
}

function executionPrompt(delivery: Delivery): string {
  const review = assertStrongFlowPlanReviewCurrent(delivery)
  return JSON.stringify({
    protocol: 'winwincode.execution.v1',
    deliverySpec: delivery.spec,
    approvedPlanReview: {
      solution: review.context.solution,
      risks: review.context.risks,
      unresolvedItems: review.context.unresolvedItems,
      decision: review.decision,
    },
    instruction: [
      'Implement only this approved delivery in the isolated candidate worktree.',
      'Run the checks needed by the acceptance criteria.',
      'Leave intended source changes uncommitted; the stage controller freezes them.',
      'Do not approve or verify your own work.',
    ],
  })
}

interface ApprovedDiagramRemediation {
  readonly annotations: readonly StrongFlowRemediationAnnotation[]
}

function approvedDiagramRemediation(
  delivery: Delivery,
  run: StageRun,
): ApprovedDiagramRemediation {
  const item = delivery.attentionItems.findLast(entry => {
    if (entry.type !== 'delivery_approval'
      || entry.status !== 'dismissed'
      || entry.resolution === null
      || entry.stageRunId === null
      || entry.resolvedAtMillis === null
      || entry.resolvedAtMillis > run.startedAtMillis
      || entry.deliverySpecId !== delivery.spec.id) return false
    return delivery.stageRuns.some(reviewRun => (
      reviewRun.id === entry.stageRunId
      && reviewRun.stage === 'delivery-review'
      && reviewRun.status === 'succeeded'
    ))
  })
  if (item === undefined) {
    return coordinatorError(
      'DELIVERY_STATE_CONFLICT',
      'remediator StageRun has no approved diagram annotations',
      delivery.revision,
    )
  }
  let value: unknown
  try {
    value = JSON.parse(item.resolution!)
  } catch (error) {
    return coordinatorError(
      'DELIVERY_STATE_CONFLICT',
      'approved diagram remediation is not valid JSON',
      delivery.revision,
      error,
    )
  }
  if (!isRecord(value)
    || !exactKeys(value, [
      'schemaVersion',
      'protocol',
      'summary',
      'deliveryTaskId',
      'candidateRef',
      'diffSha256',
      'annotations',
    ])
    || value.schemaVersion !== STRONGFLOW_DELIVERY_API_SCHEMA_VERSION
    || value.protocol !== STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL
    || typeof value.summary !== 'string'
    || value.summary.trim().length === 0
    || value.summary.length > 65_536
    || typeof value.candidateRef !== 'string'
    || !/^git-candidate:sha256:[a-f0-9]{64}$/u.test(value.candidateRef)
    || typeof value.diffSha256 !== 'string'
    || !/^[a-f0-9]{64}$/u.test(value.diffSha256)) {
    return coordinatorError(
      'DELIVERY_STATE_CONFLICT',
      'approved diagram remediation record is invalid',
      delivery.revision,
    )
  }
  try {
    if (value.deliveryTaskId !== run.deliveryTaskId) {
      return coordinatorError(
        'DELIVERY_STATE_CONFLICT',
        'remediator StageRun broadened the approved DeliveryTask scope',
        delivery.revision,
      )
    }
    return immutable({
      annotations: parseStrongFlowRemediationAnnotations(
        value.annotations,
        'approvedRemediation.annotations',
      ),
    })
  } catch (error) {
    if (error instanceof StrongFlowDeliveryStageCoordinatorError) throw error
    return coordinatorError(
      'DELIVERY_STATE_CONFLICT',
      'approved diagram annotations are invalid',
      delivery.revision,
      error,
    )
  }
}

function remediationPrompt(
  delivery: Delivery,
  run: StageRun,
): string {
  const approved = approvedDiagramRemediation(delivery, run)
  return JSON.stringify({
    protocol: 'winwincode.remediation.v1',
    deliverySpec: delivery.spec,
    approvedAnnotations: approved.annotations,
    instruction: [
      'Modify only what the approved annotations require inside the existing candidate worktree.',
      'Treat every annotated file path and hunk as a hard boundary; do not broaden scope.',
      'Run the checks needed by the DeliverySpec after the correction.',
      'Leave intended source changes uncommitted; the stage controller freezes them.',
      'Do not approve or verify your own work.',
    ],
  })
}

function firstVerificationPrompt(assignment: IndependentVerificationAssignment): string {
  return [
    serializeIndependentVerificationSessionInput(assignment),
    '',
    'Evidence collection turn:',
    '- Inspect the exact candidate and run relevant checks through Codex tools.',
    '- Do not modify the candidate.',
    '- End with a short plain-text observation.',
    '- Do not emit the final winwincode.independent-verification-result.v1 JSON yet.',
    'A later turn supplies the exact RuntimeEvent IDs you may cite.',
  ].join('\n')
}

interface AllowedVerificationEvidence {
  readonly citation: { readonly type: string; readonly event_id: string }
  readonly outcome: string
}

function finalVerificationPrompt(
  assignment: IndependentVerificationAssignment,
  evidence: readonly AllowedVerificationEvidence[],
  correctionReason: string | null = null,
): string {
  return [
    correctionReason === null
      ? 'Return the final verification result now as one plain JSON object and no markdown.'
      : `Correct the rejected verification result (${correctionReason}) as one plain JSON object and no markdown.`,
    `Role: ${assignment.role}.`,
    `Result contract: ${JSON.stringify(assignment.sessionInput.resultContract)}.`,
    'The exact normalized evidence sources observed in the evidence turn are:',
    JSON.stringify(evidence),
    'Every evidence_sources object must copy one citation object from that list exactly.',
    'Evaluate every required criterion and preserve the exact spec revision and candidate_ref.',
  ].join('\n')
}

function deliveryReviewAttention(
  delivery: Delivery,
  candidate: FrozenDeliveryCandidate,
  stageRunId: string,
  attentionItemId: string,
  assignedTo: string,
) {
  if (delivery.spec.publicationTarget !== null) {
    return createStrongFlowGitHubPublicationAttention({
      delivery,
      candidate,
      attentionItemId,
      reviewStageRunId: stageRunId,
      assignedTo,
      preparedAtMillis: delivery.updatedAtMillis,
    })
  }
  return parseAttentionItem({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: attentionItemId,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    stageRunId,
    type: 'delivery_approval',
    title: '审核当前候选和验收结论',
    context: JSON.stringify({
      candidateRef: candidate.candidateRef,
      deliveryVerdictId: delivery.verdict?.id ?? null,
      message: '当前冻结候选已经通过独立 reviewer 和 verifier。',
    }),
    options: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'approve-delivery',
      label: '批准交付',
      description: '批准当前冻结候选和通过的验收结论。',
    }],
    assignedTo,
    blocking: true,
    status: 'open',
    resolution: null,
    resolvedBy: null,
    createdAtMillis: delivery.updatedAtMillis,
    resolvedAtMillis: null,
  })
}

function mappedServiceError(error: StrongFlowServiceError): StrongFlowDeliveryStageCoordinatorError {
  const code: StrongFlowDeliveryAdvanceErrorCode = error.code === 'DELIVERY_NOT_FOUND'
    ? 'DELIVERY_NOT_FOUND'
    : error.code === 'REVISION_CONFLICT'
      ? 'REVISION_CONFLICT'
      : 'DELIVERY_STATE_CONFLICT'
  return new StrongFlowDeliveryStageCoordinatorError(
    code,
    error.message,
    error.currentRevision,
    { cause: error },
  )
}

/** Host-owned stage driver. Codex remains the sole owner of model, tool, and Agent execution. */
export class StrongFlowDeliveryStageCoordinator {
  readonly #service: StrongFlowService
  readonly #runtime: StrongFlowStageRuntime
  readonly #workspace: LocalGitDeliveryWorkspace
  readonly #activeDeliveries = new Set<string>()

  constructor(options: StrongFlowDeliveryStageCoordinatorOptions) {
    if (!(options?.service instanceof StrongFlowService)
      || typeof options.runtime?.reconcileDelivery !== 'function'
      || typeof options.runtime?.openRoleSession !== 'function'
      || typeof options.runtime?.readRuntimeSessionEvents !== 'function'
      || !(options.workspace instanceof LocalGitDeliveryWorkspace)) {
      throw new StrongFlowDeliveryStageCoordinatorError(
        'INVALID_REQUEST',
        'stage coordinator options are invalid',
      )
    }
    this.#service = options.service
    this.#runtime = options.runtime
    this.#workspace = options.workspace
  }

  async advance(
    requestValue: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    options: { readonly signal?: AbortSignal } = {},
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    let request: StrongFlowDeliveryAdvanceRequest
    try {
      request = parseStrongFlowDeliveryAdvanceRequest(requestValue)
    } catch (error) {
      return coordinatorError('INVALID_REQUEST', 'stage advance request is invalid', null, error)
    }
    if (typeof caller?.dshSessionId !== 'string'
      || caller.dshSessionId.length === 0
      || typeof caller.modelRoute?.provider !== 'string'
      || caller.modelRoute.provider.length === 0
      || typeof caller.modelRoute.model !== 'string'
      || caller.modelRoute.model.length === 0) {
      return coordinatorError(
        'MODEL_SELECTION_REQUIRED',
        'select one DSH provider and model before advancing StrongFlow',
      )
    }
    options.signal?.throwIfAborted()
    if (this.#activeDeliveries.has(request.deliveryId)) {
      return coordinatorError('STAGE_BUSY', 'this Delivery is already advancing')
    }
    this.#activeDeliveries.add(request.deliveryId)
    try {
      return await this.#advanceUnlocked(request, caller, options.signal)
    } catch (error) {
      if (error instanceof StrongFlowDeliveryStageCoordinatorError) throw error
      if (error instanceof StrongFlowServiceError) throw mappedServiceError(error)
      if (error instanceof LocalGitDeliveryWorkspaceError) {
        throw new StrongFlowDeliveryStageCoordinatorError(
          error.code === 'UNSUPPORTED_REPOSITORY'
            ? 'UNSUPPORTED_REPOSITORY'
            : error.code === 'OPERATION_ABORTED'
              ? 'OPERATION_ABORTED'
              : 'CANDIDATE_FAILURE',
          error.message,
          null,
          { cause: error },
        )
      }
      if (options.signal?.aborted === true) {
        return coordinatorError('OPERATION_ABORTED', 'stage advance was aborted', null, error)
      }
      return coordinatorError('INTERNAL_ERROR', 'stage advance failed', null, error)
    } finally {
      this.#activeDeliveries.delete(request.deliveryId)
    }
  }

  async #advanceUnlocked(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    const projection = await this.#service.getDeliveryProjection(request.deliveryId)
    if (projection.delivery.revision !== request.expectedRevision) {
      return coordinatorError(
        'REVISION_CONFLICT',
        'Delivery changed before this stage could advance',
        projection.delivery.revision,
      )
    }
    const candidate = await this.#workspace.currentCandidate(
      projection.delivery,
      signalOptions(signal),
    )
    const recovered = await this.#runtime.reconcileDelivery(request.deliveryId, candidate)
    if (recovered.delivery.revision !== request.expectedRevision) {
      return coordinatorError(
        'REVISION_CONFLICT',
        'Delivery changed while rebuilding its next stage',
        recovered.delivery.revision,
      )
    }
    return this.#driveAction(request, caller, recovered.delivery, recovered.nextAction, signal)
  }

  async #driveAction(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    delivery: Delivery,
    action: StrongFlowRecoveryAction,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    switch (action.kind) {
      case 'delivery-complete':
        return immutable({
          delivery,
          outcome: outcome('delivery-complete', 'Delivery 已经完成。'),
        })
      case 'resolve-delivery-attention':
        return immutable({
          delivery,
          outcome: outcome(
            'attention-required',
            '请先处理当前人工审核或业务决定。',
            delivery.stageRuns.find(run => run.status === 'waiting')?.id ?? null,
            caller.dshSessionId,
          ),
        })
      case 'resolve-runtime-interaction':
        return coordinatorError(
          'RUNTIME_INTERACTION_REQUIRED',
          '请打开绑定的 Chat Session 处理 Codex 的权限或输入请求。',
          delivery.revision,
        )
      case 'continue-stage':
        return immutable({
          delivery,
          outcome: outcome(
            'stage-busy',
            '这个阶段仍在执行，请查看实时状态。',
            action.stageRunId,
            action.dshSessionId,
          ),
        })
      case 'start-stage':
        return this.#startStage(request, caller, delivery, action, signal)
      case 'create-stage-session': {
        const run = runFor(delivery, action.stageRunId)
        if (action.actorType === 'human') {
          const bound = await this.#bindHuman(request, delivery, run, caller.dshSessionId)
          return immutable({
            delivery: bound,
            outcome: outcome(
              'attention-required',
              '人工审核已绑定到当前 Chat Session。',
              run.id,
              caller.dshSessionId,
            ),
          })
        }
        return this.#openBindAndRun(request, caller, delivery, run, signal)
      }
      case 'resume-stage-session': {
        if (action.pendingInteractionIds.length > 0) {
          return coordinatorError(
            'RUNTIME_INTERACTION_REQUIRED',
            '恢复前需要在绑定的 Chat Session 处理未完成的交互。',
            delivery.revision,
          )
        }
        const run = runFor(delivery, action.stageRunId)
        return this.#openAndRun(
          request,
          caller,
          delivery,
          run,
          action.dshSessionId,
          true,
          signal,
        )
      }
      case 'review-stage-output': {
        const run = runFor(delivery, action.stageRunId)
        if (action.runtimeStatus !== 'completed') {
          const events = await this.#runtime.readRuntimeSessionEvents(action.dshSessionId)
          const failedTurns = events.filter(event => (
            event.kind === 'turn.aborted'
            || event.kind === 'failure'
            || (event.kind === 'turn.completed' && event.terminalReason !== 'completed')
          )).length
          if (failedTurns >= MAX_STRUCTURED_RESULT_ATTEMPTS) {
            return coordinatorError(
              'STAGE_OUTPUT_INVALID',
              `${run.role} Session 已达到失败重试上限。`,
              delivery.revision,
            )
          }
          return this.#openAndRun(
            request,
            caller,
            delivery,
            run,
            action.dshSessionId,
            true,
            signal,
          )
        }
        return this.#reviewCompletedStage(
          request,
          caller,
          delivery,
          run,
          action.dshSessionId,
          signal,
        )
      }
    }
  }

  async #startStage(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    delivery: Delivery,
    action: Extract<StrongFlowRecoveryAction, { readonly kind: 'start-stage' }>,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    if (action.stage === 'delivery-review') {
      const candidate = await this.#requiredCandidate(delivery, signal)
      const ordinal = delivery.stageRuns.filter(run => run.stage === 'delivery-review').length + 1
      const stageRunId = StageRunId(digestIdentity(delivery, 'delivery-review', ordinal))
      const attentionItemId = digestIdentity(delivery, 'delivery-attention', ordinal)
      const attention = deliveryReviewAttention(
        delivery,
        candidate,
        stageRunId,
        attentionItemId,
        caller.dshSessionId,
      )
      const started = await this.#service.startStage({
        requestId: requestId(request, 'start-delivery-review'),
        deliveryId: delivery.id,
        expectedRevision: delivery.revision,
        stageRunId,
        deliveryTaskId: null,
        stage: 'delivery-review',
        actorType: 'human',
        role: 'approver',
        attention,
      })
      const bound = await this.#bindHuman(request, started, runFor(started, stageRunId), caller.dshSessionId)
      return immutable({
        delivery: bound,
        outcome: outcome(
          'delivery-review-ready',
          '独立验证已通过，请审核当前冻结候选。',
          stageRunId,
          caller.dshSessionId,
        ),
      })
    }
    const role = this.#roleForStage(delivery, action.stage)
    const ordinal = delivery.stageRuns.filter(run => run.role === role).length + 1
    const stageRunId = StageRunId(digestIdentity(delivery, `${role}-stage`, ordinal))
    const started = await this.#service.startStage({
      requestId: requestId(request, `start-${role}-${String(ordinal)}`),
      deliveryId: delivery.id,
      expectedRevision: delivery.revision,
      stageRunId,
      deliveryTaskId: action.deliveryTaskId === null
        ? null
        : DeliveryTaskId(action.deliveryTaskId),
      stage: action.stage,
      actorType: 'codex',
      role,
      attention: null,
    })
    return this.#openBindAndRun(request, caller, started, runFor(started, stageRunId), signal)
  }

  #roleForStage(delivery: Delivery, stage: DeliveryStage): StrongFlowRoleId {
    if (stage === 'planning') return 'planner'
    if (stage === 'executing') return 'executor'
    if (stage === 'reworking') return 'remediator'
    if (stage === 'verifying') {
      const writer = delivery.stageRuns.findLast(run => (
        run.stage === 'executing' || run.stage === 'reworking'
      ))
      const later = writer === undefined
        ? []
        : delivery.stageRuns.filter(run => (
          run.stage === 'verifying' && run.startedAtMillis >= writer.startedAtMillis
        ))
      if (!later.some(run => run.role === 'reviewer')) return 'reviewer'
      if (!later.some(run => run.role === 'verifier')) return 'verifier'
    }
    return coordinatorError(
      'DELIVERY_STATE_CONFLICT',
      `StrongFlow does not have an automatic role for ${stage}`,
    )
  }

  async #bindHuman(
    request: StrongFlowDeliveryAdvanceRequest,
    delivery: Delivery,
    run: StageRun,
    dshSessionId: string,
  ): Promise<Delivery> {
    return this.#service.bindSession({
      requestId: requestId(request, `bind-human-${run.id.slice(-32)}`),
      deliveryId: delivery.id,
      expectedRevision: delivery.revision,
      bindingId: digestIdentity(delivery, `human-binding-${run.id.slice(-24)}`),
      stageRunId: run.id,
      dshSessionId,
      codexSessionId: null,
    })
  }

  async #workspaceForRole(
    delivery: Delivery,
    role: StrongFlowRoleId,
    signal?: AbortSignal,
  ): Promise<string> {
    if (role === 'planner' || role === 'requirements' || role === 'solution') {
      if (delivery.spec.repository.kind !== 'local-git') {
        return coordinatorError(
          'UNSUPPORTED_REPOSITORY',
          'browser-driven stages currently require a local Git repository',
        )
      }
      return delivery.spec.repository.locator
    }
    return (await this.#workspace.prepare(delivery, signalOptions(signal))).path
  }

  async #openBindAndRun(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    delivery: Delivery,
    run: StageRun,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    const dshSessionId = digestIdentity(delivery, `${run.role}-session`, run.attempt)
    const role = strongFlowRole(run.role)
    const session = await this.#runtime.openRoleSession({
      dshSessionId,
      role,
      cwd: await this.#workspaceForRole(delivery, role, signal),
      modelRoute: caller.modelRoute,
      ...(signal === undefined ? {} : { signal }),
    })
    let bound = delivery
    try {
      bound = await this.#service.bindSession({
        requestId: requestId(request, `bind-${run.role}-${String(run.attempt)}`),
        deliveryId: delivery.id,
        expectedRevision: delivery.revision,
        bindingId: digestIdentity(delivery, `${run.role}-binding`, run.attempt),
        stageRunId: run.id,
        dshSessionId: session.dshSessionId,
        codexSessionId: session.codexSessionId,
      })
      return await this.#runOpenedSession(request, caller, bound, runFor(bound, run.id), session, false, signal)
    } finally {
      await session.dispose()
    }
  }

  async #openAndRun(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    delivery: Delivery,
    run: StageRun,
    dshSessionId: string,
    resumed: boolean,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    const role = strongFlowRole(run.role)
    const session = await this.#runtime.openRoleSession({
      dshSessionId,
      role,
      cwd: await this.#workspaceForRole(delivery, role, signal),
      modelRoute: caller.modelRoute,
      ...(signal === undefined ? {} : { signal }),
    })
    const binding = bindingFor(delivery, run)
    if (binding !== null && binding.codexSessionId !== session.codexSessionId) {
      await session.dispose()
      return coordinatorError(
        'DELIVERY_STATE_CONFLICT',
        'resumed Codex Session no longer matches its Delivery binding',
        delivery.revision,
      )
    }
    try {
      return await this.#runOpenedSession(request, caller, delivery, run, session, resumed, signal)
    } finally {
      await session.dispose()
    }
  }

  async #turn(
    session: StrongFlowStageRoleSession,
    prompt: string,
    role: string,
    signal?: AbortSignal,
  ): Promise<readonly RuntimeEvent[]> {
    await session.turn(prompt, signal)
    if (signal?.aborted === true) {
      return coordinatorError('OPERATION_ABORTED', 'stage advance was aborted')
    }
    const events = await this.#runtime.readRuntimeSessionEvents(session.dshSessionId)
    assertCompletedTurn(events, role)
    return events
  }

  async #runOpenedSession(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    delivery: Delivery,
    run: StageRun,
    session: StrongFlowStageRoleSession,
    resumed: boolean,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    switch (run.role) {
      case 'planner': {
        await this.#turn(
          session,
          resumed
            ? `Continue the approved planning stage.\n\n${planningPrompt(delivery)}`
            : planningPrompt(delivery),
          run.role,
          signal,
        )
        let result: PlanningResult
        try {
          result = planningResult(
            await this.#runtime.readRuntimeSessionEvents(session.dshSessionId),
          )
        } catch (error) {
          if (!(error instanceof StrongFlowDeliveryStageCoordinatorError)
            || error.code !== 'STAGE_OUTPUT_INVALID') throw error
          await this.#turn(session, planningCorrectionPrompt(), run.role, signal)
          result = planningResult(
            await this.#runtime.readRuntimeSessionEvents(session.dshSessionId),
          )
        }
        return this.#completePlanning(request, caller, delivery, run, result)
      }
      case 'executor':
        await this.#turn(
          session,
          resumed
            ? `Continue the assigned implementation and leave the intended changes uncommitted.\n\n${executionPrompt(delivery)}`
            : executionPrompt(delivery),
          run.role,
          signal,
        )
        return this.#completeCandidateWriter(request, caller, delivery, run, signal)
      case 'remediator':
        await this.#turn(
          session,
          resumed
            ? `Continue only the approved diagram remediation.\n\n${remediationPrompt(delivery, run)}`
            : remediationPrompt(delivery, run),
          run.role,
          signal,
        )
        return this.#completeCandidateWriter(request, caller, delivery, run, signal)
      case 'reviewer':
      case 'verifier':
      case 'adversarial-verifier':
        await this.#ensureVerificationResult(delivery, run, session, signal)
        return this.#completeVerificationRole(request, caller, delivery, run, signal)
      case 'requirements':
      case 'solution':
        return coordinatorError(
          'DELIVERY_STATE_CONFLICT',
          `${run.role} automatic stage driving is not part of this delivery path`,
          delivery.revision,
        )
      default:
        return coordinatorError('DELIVERY_STATE_CONFLICT', 'stage role is unsupported')
    }
  }

  async #reviewCompletedStage(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    delivery: Delivery,
    run: StageRun,
    dshSessionId: string,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    switch (run.role) {
      case 'planner': {
        try {
          return this.#completePlanning(
            request,
            caller,
            delivery,
            run,
            planningResult(await this.#runtime.readRuntimeSessionEvents(dshSessionId)),
          )
        } catch (error) {
          if (!(error instanceof StrongFlowDeliveryStageCoordinatorError)
            || error.code !== 'STAGE_OUTPUT_INVALID') throw error
          return this.#openAndRun(request, caller, delivery, run, dshSessionId, true, signal)
        }
      }
      case 'executor':
      case 'remediator':
        return this.#completeCandidateWriter(request, caller, delivery, run, signal)
      case 'reviewer':
      case 'verifier':
      case 'adversarial-verifier': {
        const candidate = await this.#requiredCandidate(delivery, signal)
        const accepted = await this.#verificationAccepted(delivery, run, candidate)
        if (!accepted) {
          return this.#openAndRun(request, caller, delivery, run, dshSessionId, true, signal)
        }
        return this.#completeVerificationRole(request, caller, delivery, run, signal)
      }
      default:
        return coordinatorError(
          'DELIVERY_STATE_CONFLICT',
          `completed ${run.role} output has no automatic transition`,
          delivery.revision,
        )
    }
  }

  async #completePlanning(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    delivery: Delivery,
    run: StageRun,
    result: PlanningResult,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    const reviewStageRunId = StageRunId(digestIdentity(delivery, 'plan-review-stage'))
    const attentionItemId = digestIdentity(delivery, 'plan-review-attention')
    const attention = createStrongFlowPlanReviewAttention({
      delivery,
      attentionItemId,
      reviewStageRunId,
      assignedTo: caller.dshSessionId,
      solution: result.solution,
      risks: result.risks,
      unresolvedItems: result.unresolvedItems,
      preparedAtMillis: delivery.updatedAtMillis,
    })
    const review = await this.#service.startStage({
      requestId: requestId(request, 'start-plan-review'),
      deliveryId: delivery.id,
      expectedRevision: delivery.revision,
      stageRunId: reviewStageRunId,
      deliveryTaskId: null,
      stage: 'plan-review',
      actorType: 'human',
      role: 'reviewer',
      attention,
    })
    const bound = await this.#bindHuman(
      request,
      review,
      runFor(review, reviewStageRunId),
      caller.dshSessionId,
    )
    return immutable({
      delivery: bound,
      outcome: outcome(
        'plan-review-ready',
        '方案、系统架构图和流程图已经生成，请人工审核后再执行。',
        reviewStageRunId,
        caller.dshSessionId,
      ),
    })
  }

  async #completeCandidateWriter(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    delivery: Delivery,
    run: StageRun,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    await this.#workspace.freezeCandidateFacts(delivery, signalOptions(signal))
    const reviewerOrdinal = delivery.stageRuns.filter(entry => entry.role === 'reviewer').length + 1
    const reviewerStageRunId = StageRunId(
      digestIdentity(delivery, 'reviewer-stage', reviewerOrdinal),
    )
    const verifying = await this.#service.startStage({
      requestId: requestId(request, `start-reviewer-${String(reviewerOrdinal)}`),
      deliveryId: delivery.id,
      expectedRevision: delivery.revision,
      stageRunId: reviewerStageRunId,
      deliveryTaskId: run.deliveryTaskId,
      stage: 'verifying',
      actorType: 'codex',
      role: 'reviewer',
      attention: null,
    })
    const candidate = await this.#requiredCandidate(verifying, signal)
    return immutable({
      delivery: verifying,
      outcome: outcome(
        'candidate-ready-for-review',
        `候选 ${candidate.candidateCommitId.slice(0, 12)} 已冻结，下一步由 reviewer 独立检查。`,
        reviewerStageRunId,
        null,
      ),
    })
  }

  async #requiredCandidate(
    delivery: Delivery,
    signal?: AbortSignal,
  ): Promise<FrozenDeliveryCandidate> {
    const candidate = await this.#workspace.currentCandidate(delivery, signalOptions(signal))
    if (candidate === null) {
      return coordinatorError(
        'CANDIDATE_FAILURE',
        'current Delivery has no frozen Git candidate',
        delivery.revision,
      )
    }
    return candidate
  }

  async #allRuntimeEvents(delivery: Delivery): Promise<readonly RuntimeEvent[]> {
    const events: RuntimeEvent[] = []
    for (const binding of delivery.sessionBindings) {
      if (binding.dshSessionId === null || binding.codexSessionId === null) continue
      events.push(...await this.#runtime.readRuntimeSessionEvents(binding.dshSessionId))
    }
    return Object.freeze(events)
  }

  async #evidenceForBinding(
    delivery: Delivery,
    bindingId: string,
  ): Promise<readonly AllowedVerificationEvidence[]> {
    const events = await this.#allRuntimeEvents(delivery)
    const projection = new DeliveryRuntimeProjection({ delivery }).replay(events)
    const session = projection.stages.flatMap(stage => stage.sessions)
      .find(entry => entry.binding.id === bindingId)
    if (session === undefined) {
      return coordinatorError('STAGE_OUTPUT_INVALID', 'verification runtime projection is missing')
    }
    const evidence = session.evidenceLinks
      .filter(link => link.type !== 'review_finding')
      .map(link => Object.freeze({
        citation: Object.freeze({ type: link.type, event_id: link.eventId }),
        outcome: link.outcome,
      }))
    return Object.freeze(evidence)
  }

  async #verificationAccepted(
    delivery: Delivery,
    run: StageRun,
    candidate: FrozenDeliveryCandidate,
  ): Promise<boolean> {
    const binding = bindingFor(delivery, run)
    if (binding === null) return false
    const events = await this.#allRuntimeEvents(delivery)
    const evidence = await this.#evidenceForBinding(delivery, binding.id)
    const allowed = new Set(evidence.map(entry => (
      `${entry.citation.type}\u0000${entry.citation.event_id}`
    )))
    const sessionEvents = events.filter(event => (
      event.source.sessionId === binding.dshSessionId
      && event.source.kernelSessionId === binding.codexSessionId
    ))
    const semantic = latestTurnEvents(sessionEvents).findLast(event => (
      event.semantic?.kind === 'verification-result'
    ))?.semantic
    if (semantic?.kind !== 'verification-result'
      || semantic.findings.some(finding => finding.evidenceSources.some(source => (
        !allowed.has(`${source.type}\u0000${source.eventId}`)
      )))) return false
    try {
      const projection = projectIndependentVerification({
        delivery,
        acceptance: freezeAcceptanceVerificationInput(delivery),
        candidate,
        runtimeEvents: events,
        requiredRoles: ['reviewer', 'verifier'],
      })
      const settlement = projection.sessions.find(session => (
        session.assignment?.sessionBindingId === binding.id
      ))
      return settlement?.state === 'settled' && settlement.findings.length > 0
    } catch {
      return false
    }
  }

  async #ensureVerificationResult(
    delivery: Delivery,
    run: StageRun,
    session: StrongFlowStageRoleSession,
    signal?: AbortSignal,
  ): Promise<void> {
    const binding = bindingFor(delivery, run)
    if (binding === null) {
      return coordinatorError('DELIVERY_STATE_CONFLICT', 'verification Session is unbound')
    }
    const candidate = await this.#requiredCandidate(delivery, signal)
    await this.#workspace.assertCandidate(delivery, candidate, signalOptions(signal))
    const assignment = createIndependentVerificationAssignment({
      delivery,
      acceptance: freezeAcceptanceVerificationInput(delivery),
      candidate,
      stageRunId: run.id,
      sessionBindingId: binding.id,
    })
    let evidence = await this.#evidenceForBinding(delivery, binding.id)
    if (evidence.length === 0) {
      await this.#turn(session, firstVerificationPrompt(assignment), run.role, signal)
      await this.#workspace.assertCandidate(delivery, candidate, signalOptions(signal))
      evidence = await this.#evidenceForBinding(delivery, binding.id)
    }
    if (evidence.length === 0) {
      return coordinatorError(
        'STAGE_OUTPUT_INVALID',
        `${run.role} produced no citable command or test evidence`,
      )
    }
    if (await this.#verificationAccepted(delivery, run, candidate)) return
    const currentEvents = await this.#runtime.readRuntimeSessionEvents(session.dshSessionId)
    const priorResultCount = currentEvents.filter(event => (
      event.semantic?.kind === 'verification-result'
    )).length
    for (let attempt = priorResultCount; attempt < MAX_STRUCTURED_RESULT_ATTEMPTS; attempt += 1) {
      await this.#turn(
        session,
        finalVerificationPrompt(
          assignment,
          evidence,
          attempt === 0 ? null : 'RESULT_INVALID',
        ),
        run.role,
        signal,
      )
      await this.#workspace.assertCandidate(delivery, candidate, signalOptions(signal))
      if (await this.#verificationAccepted(delivery, run, candidate)) return
    }
    return coordinatorError(
      'STAGE_OUTPUT_INVALID',
      `${run.role} exhausted the structured verification-result correction limit`,
    )
  }

  async #completeVerificationRole(
    request: StrongFlowDeliveryAdvanceRequest,
    caller: StrongFlowAdvanceCaller,
    delivery: Delivery,
    run: StageRun,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryAdvanceResult> {
    const candidate = await this.#requiredCandidate(delivery, signal)
    if (!(await this.#verificationAccepted(delivery, run, candidate))) {
      return coordinatorError(
        'STAGE_OUTPUT_INVALID',
        `${run.role} result is not a valid independent verification result`,
      )
    }
    if (run.role === 'reviewer') {
      const verifierOrdinal = delivery.stageRuns.filter(entry => entry.role === 'verifier').length + 1
      const verifierStageRunId = StageRunId(
        digestIdentity(delivery, 'verifier-stage', verifierOrdinal),
      )
      const verifying = await this.#service.startStage({
        requestId: requestId(request, `start-verifier-${String(verifierOrdinal)}`),
        deliveryId: delivery.id,
        expectedRevision: delivery.revision,
        stageRunId: verifierStageRunId,
        deliveryTaskId: run.deliveryTaskId,
        stage: 'verifying',
        actorType: 'codex',
        role: 'verifier',
        attention: null,
      })
      return immutable({
        delivery: verifying,
        outcome: outcome(
          'reviewer-complete',
          'reviewer 已完成独立检查，下一步由 verifier 执行验收。',
          verifierStageRunId,
          null,
        ),
      })
    }
    if (run.role !== 'verifier') {
      return coordinatorError(
        'DELIVERY_STATE_CONFLICT',
        `${run.role} is not the final required verification role`,
      )
    }
    const events = await this.#allRuntimeEvents(delivery)
    const evaluated = await this.#service.submitVerdict({
      requestId: requestId(request, 'submit-verdict'),
      deliveryId: delivery.id,
      expectedRevision: delivery.revision,
      candidate,
      runtimeEvents: events,
      requiredRoles: ['reviewer', 'verifier'],
    })
    if (evaluated.status !== 'ready-to-deliver') {
      return immutable({
        delivery: evaluated,
        outcome: outcome(
          'attention-required',
          '独立验证没有全部通过，请处理当前发现后再继续。',
          run.id,
          caller.dshSessionId,
        ),
      })
    }
    return this.#startStage(
      request,
      caller,
      evaluated,
      { kind: 'start-stage', stage: 'delivery-review', deliveryTaskId: null },
      signal,
    )
  }
}
