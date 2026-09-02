// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneSubscription,
} from './control-plane-client.js'
import type {
  Actor,
  AttentionItemId,
  CandidateHistoricalReviewGetResultResponse,
  CandidateHistoricalReviewProjection,
  CandidateHistoryItemProjection,
  CandidateHistoryListResultResponse,
  CommandAcceptedResponse,
  CommandCompletedResponse,
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketSubscriptionId,
  DeliveryAdvanceCompletedResponse,
  DeliveryAdvanceCommand,
  DeliveryApproveTaskBreakdownCompletedResponse,
  DeliveryCreateCommand,
  DeliveryCreateCompletedResponse,
  DeliveryDetailProjection,
  DeliveryGetResultResponse,
  DeliveryId,
  DeliveryProjection,
  DeliveryResolveAttentionCompletedResponse,
  DeliveryStageProjection,
  DeliverySubmitVerdictCompletedResponse,
  DeliveryTaskId,
  EventReadCursor,
  ProductSessionId,

  QueryResultResponse,
  RepositoryScope,
  RequestId,
  RuntimeProjectionGetResultResponse,
  RuntimeProjectionSnapshot,
  StageRunId,
  StrongFlowReadCursor,
} from './generated/contracts.js'
import {
  CommandName,
  ControlPlaneWebSocketEventType,
  QueryName,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const MAX_READ_CURSOR_RESTARTS = 2
const MAX_SNAPSHOT_CONSISTENCY_RETRIES = 40
const SNAPSHOT_CONSISTENCY_RETRY_DELAY_MILLIS = 50
const MAX_TRUSTED_FACTS_COMMAND_RETRIES = 400
const TRUSTED_FACTS_COMMAND_RETRY_DEADLINE_MILLIS = 20_000
const TRUSTED_FACTS_COMMAND_RETRY_DELAY_MILLIS = 50
const HISTORICAL_CANDIDATE_PAGE_LIMIT = 50

export type StrongFlowViewStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'refreshing'
  | 'authentication-required'
  | 'authorization-denied'
  | 'cancelled'
  | 'error'
  | 'closed'

export type StrongFlowRealtimeStatus =
  | 'inactive'
  | 'subscribed'
  | 'reloading'
  | 'reconnecting'
  | 'access-revoked'
  | 'closed'

export type StrongFlowInteractionStatus =
  | 'idle'
  | 'submitting'
  | 'waiting'
  | 'error'

export interface StrongFlowInteractionState {
  readonly status: StrongFlowInteractionStatus
  readonly error: ControlPlaneClientError | null
}

export interface StrongFlowProjectionRevisions {
  readonly delivery: number
  readonly deliverySpec: number
  readonly runtime: number
  readonly publication: number
}

export interface StrongFlowProjectionMetadata {
  readonly source: 'control-plane-snapshot'
  readonly updatedAt: string
  readonly revisions: StrongFlowProjectionRevisions
  readonly readCursor: StrongFlowReadCursor
}

export interface StrongFlowProjection {
  readonly delivery: DeliveryDetailProjection
  readonly solutionReview: DeliveryDetailProjection['solutionReview']
  readonly stage: DeliveryStageProjection
  readonly runtime: RuntimeProjectionSnapshot
  readonly evidence: DeliveryDetailProjection['evidence']
  readonly verdict: DeliveryDetailProjection['verdict']
  readonly attention: DeliveryDetailProjection['attention']
  readonly publication: DeliveryDetailProjection['publication']
  readonly currentCandidate: DeliveryDetailProjection['currentCandidate']
  readonly metadata: StrongFlowProjectionMetadata
}

export interface StrongFlowViewModelState {
  readonly status: StrongFlowViewStatus
  readonly realtime: StrongFlowRealtimeStatus
  readonly projection: StrongFlowProjection | null
  readonly interaction: StrongFlowInteractionState
  readonly error: ControlPlaneClientError | null
}

export type StrongFlowViewModelListener = (state: StrongFlowViewModelState) => void

export interface StrongFlowStageBinding {
  readonly productSessionId: ProductSessionId
  readonly stageRunId: StageRunId
}

export interface StrongFlowViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly deliveryId: DeliveryId
  readonly productSessionId: ProductSessionId
  readonly stageRunId: StageRunId
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  readonly onStageBindingChange?: (binding: StrongFlowStageBinding) => void
}

export interface StrongFlowSolutionReviewDecisionInput {
  readonly action: 'approve' | 'request_changes' | 'reject'
  readonly comments: string
  readonly requestedChanges: readonly string[]
}

export interface StrongFlowAttentionRemediationInput {
  readonly deliveryTaskId: DeliveryTaskId | null
  /** One node from the current solution-review diagrams. */
  readonly nodeId: string
  readonly instructions: string
}

export interface StrongFlowAttentionDecisionInput {
  readonly attentionItemId: AttentionItemId
  readonly decision: 'resolve' | 'dismiss'
  readonly resolution: string
  readonly remediation: StrongFlowAttentionRemediationInput | null
}

/** Exact server identity of one historical Candidate opened for read-only review. */
export interface StrongFlowHistoricalCandidateIdentity {
  readonly candidateRef: string
  readonly candidateTreeId: string
  readonly diffSha256: string
}

export interface StrongFlowViewModel {
  readonly state: StrongFlowViewModelState
  subscribe(listener: StrongFlowViewModelListener): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  decideSolutionReview(input: StrongFlowSolutionReviewDecisionInput): Promise<void>
  approveTaskBreakdown(): Promise<void>
  resolveAttention(input: StrongFlowAttentionDecisionInput): Promise<void>
  submitVerdict(): Promise<void>
  advanceDelivery(): Promise<void>
  /**
   * Read the exact RuntimeProjection of one historical StageRun at the current
   * snapshot's read cursor. Human review runs have no runtime binding and
   * resolve to null instead of a projection.
   */
  loadStageRunRuntime(stageRunId: StageRunId): Promise<RuntimeProjectionSnapshot | null>
  /** Read the exact Candidates a historical StageRun produced, from candidate.list. */
  loadStageRunCandidates(
    stageRunId: StageRunId,
  ): Promise<readonly CandidateHistoryItemProjection[]>
  /** Open one exact historical Candidate review (candidate.review.get), display-only. */
  loadCandidateHistoricalReview(
    candidate: StrongFlowHistoricalCandidateIdentity,
  ): Promise<CandidateHistoricalReviewProjection | null>
  cancelPending(): void
  reconnect(): void
  close(): void
}

interface StrongFlowSnapshot {
  readonly binding: StrongFlowStageBinding
  readonly delivery: DeliveryDetailProjection
  readonly runtime: RuntimeProjectionSnapshot
  readonly stage: DeliveryStageProjection
}

interface StrongFlowSnapshotMinimum {
  readonly eventSequence?: number
  readonly deliveryRevision?: number
  readonly runtimeRevision?: number
  readonly runtimeSequence?: number
  readonly announcedBinding?: StrongFlowStageBinding
}

interface StrongFlowQueryResponses {
  readonly [QueryName.DeliveryGet]: DeliveryGetResultResponse
  readonly [QueryName.RuntimeProjectionGet]: RuntimeProjectionGetResultResponse
  readonly [QueryName.CandidateList]: CandidateHistoryListResultResponse
  readonly [QueryName.CandidateReviewGet]: CandidateHistoricalReviewGetResultResponse
}

interface StrongFlowCommandResponses {
  readonly [CommandName.DeliveryCreate]: DeliveryCreateCompletedResponse
  readonly [CommandName.DeliveryApproveTaskBreakdown]: DeliveryApproveTaskBreakdownCompletedResponse
  readonly [CommandName.DeliveryAdvance]: DeliveryAdvanceCompletedResponse
  readonly [CommandName.DeliveryResolveAttention]: DeliveryResolveAttentionCompletedResponse
  readonly [CommandName.DeliverySubmitVerdict]: DeliverySubmitVerdictCompletedResponse
}

export type StrongFlowCreateStatus =
  | 'idle'
  | 'submitting'
  | 'waiting'
  | 'created'
  | 'error'
  | 'closed'

export interface StrongFlowCreateState {
  readonly status: StrongFlowCreateStatus
  readonly error: ControlPlaneClientError | null
}

export interface StrongFlowCreateInput {
  readonly title: string
  readonly goal: string
  readonly baseRevision: string
  readonly acceptanceCriteria: readonly string[]
}

export interface StrongFlowCreateViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly nextDeliveryId: () => DeliveryId
  readonly nextRequestId: () => RequestId
  readonly onCreated: (deliveryId: DeliveryId) => void
}

export type StrongFlowCreateListener = (state: StrongFlowCreateState) => void

export interface StrongFlowCreateViewModel {
  readonly state: StrongFlowCreateState
  subscribe(listener: StrongFlowCreateListener): () => void
  create(input: StrongFlowCreateInput): Promise<void>
  cancelPending(): void
  close(): void
}

function initialState(): StrongFlowViewModelState {
  return Object.freeze({
    status: 'idle',
    realtime: 'inactive',
    projection: null,
    interaction: frozenInteraction('idle'),
    error: null,
  })
}

function frozenInteraction(
  status: StrongFlowInteractionStatus,
  error: ControlPlaneClientError | null = null,
): StrongFlowInteractionState {
  return Object.freeze({ status, error })
}

function requestPage() {
  return Object.freeze({ cursor: null, limit: 1 })
}

function clientFailure(code: string, message: string, cause?: unknown): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code,
    message,
    requestId: null,
    retryable: false,
    ...(cause === undefined ? {} : { cause }),
  })
}

function normalizedError(error: unknown, signal?: AbortSignal): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) return error
  if (signal?.aborted === true) return new ControlPlaneClientError({
    kind: 'cancelled',
    code: 'REQUEST_CANCELLED',
    message: 'The StrongFlow projection request was cancelled.',
    requestId: null,
    retryable: false,
    cause: error,
  })
  return clientFailure(
    'STRONGFLOW_VIEW_MODEL_FAILURE',
    'The StrongFlow projection could not be updated.',
    error,
  )
}

async function waitForRetry(signal: AbortSignal, delayMillis: number): Promise<void> {
  await new Promise<void>(resolve => {
    if (signal.aborted) {
      resolve()
      return
    }
    const timer = setTimeout(() => {
      signal.removeEventListener('abort', onAbort)
      resolve()
    }, delayMillis)
    function onAbort(): void {
      clearTimeout(timer)
      signal.removeEventListener('abort', onAbort)
      resolve()
    }
    signal.addEventListener('abort', onAbort, { once: true })
  })
}

async function waitForSnapshotConsistencyRetry(signal: AbortSignal): Promise<void> {
  await waitForRetry(signal, SNAPSHOT_CONSISTENCY_RETRY_DELAY_MILLIS)
}

function statusForError(error: ControlPlaneClientError): StrongFlowViewStatus {
  if (error.kind === 'authentication') return 'authentication-required'
  if (error.kind === 'authorization') return 'authorization-denied'
  if (error.kind === 'cancelled') return 'cancelled'
  return 'error'
}

function expectResponse<Query extends keyof StrongFlowQueryResponses>(
  response: QueryResultResponse,
  query: Query,
): StrongFlowQueryResponses[Query] {
  if (response.query !== query) throw clientFailure(
    'STRONGFLOW_QUERY_MISMATCH',
    'The Control Plane returned another StrongFlow query result.',
  )
  if (response.page.hasMore || response.page.nextCursor !== null) throw clientFailure(
    'STRONGFLOW_PAGE_INVALID',
    'A StrongFlow detail query returned an unexpected page cursor.',
  )
  return response as StrongFlowQueryResponses[Query]
}

function expectCompletedCommand<Command extends keyof StrongFlowCommandResponses>(
  response: CommandAcceptedResponse | CommandCompletedResponse,
  command: Command,
  requestId: RequestId,
): StrongFlowCommandResponses[Command] | null {
  if (response.requestId !== requestId || response.command !== command) throw clientFailure(
    'STRONGFLOW_COMMAND_MISMATCH',
    'The Control Plane returned another StrongFlow command result.',
  )
  if (response.outcome === 'accepted') return null
  return response as StrongFlowCommandResponses[Command]
}

function planReviewDecisionText(
  projection: StrongFlowProjection,
  input: StrongFlowSolutionReviewDecisionInput,
): string {
  const review = projection.solutionReview
  if (review === null || review.reviewStatus !== 'pending') throw clientFailure(
    'STRONGFLOW_SOLUTION_REVIEW_NOT_PENDING',
    'The current Delivery does not have a pending solution review.',
  )
  const comments = input.comments.trim()
  const requestedChanges = Object.freeze(input.requestedChanges.map(value => value.trim()).filter(
    value => value.length > 0,
  ))
  if (input.action === 'request_changes' && requestedChanges.length === 0) throw clientFailure(
    'STRONGFLOW_REQUESTED_CHANGES_REQUIRED',
    'Describe at least one change before returning the solution.',
  )
  if (input.action === 'reject' && comments.length === 0) throw clientFailure(
    'STRONGFLOW_REJECTION_REASON_REQUIRED',
    'Explain why the solution is being rejected.',
  )
  return JSON.stringify({
    schemaVersion: 1,
    protocol: 'winwincode.solution-review-decision.v1',
    deliveryId: projection.delivery.deliveryId,
    deliverySpecId: review.deliverySpecId,
    deliverySpecRevision: review.deliverySpecRevision,
    reviewStageRunId: review.reviewStageRunId,
    attentionItemId: review.attentionItemId,
    reviewSetSha256: review.reviewSetSha256.replace(/^sha256:/u, ''),
    action: input.action,
    comments,
    requestedChanges: input.action === 'request_changes' ? requestedChanges : null,
  })
}

function reworkInstructions(
  projection: StrongFlowProjection,
  input: StrongFlowAttentionRemediationInput,
): string {
  const candidate = projection.currentCandidate
  if (candidate === null) throw clientFailure(
    'STRONGFLOW_REWORK_CANDIDATE_REQUIRED',
    'Rework needs the current frozen candidate.',
  )
  const review = projection.solutionReview
  const nodeExists = review !== null && [
    ...review.architectureDiagram.nodes,
    ...review.processDiagram.nodes,
  ].some(node => node.id === input.nodeId)
  if (!nodeExists) throw clientFailure(
    'STRONGFLOW_REWORK_NODE_STALE',
    'Select a node from the current solution review before requesting rework.',
  )
  if (
    input.deliveryTaskId !== null
    && !projection.delivery.tasks.some(task => task.id === input.deliveryTaskId)
  ) throw clientFailure(
    'STRONGFLOW_REWORK_TASK_STALE',
    'Select a task from the current Delivery before requesting rework.',
  )
  const instructions = input.instructions.trim()
  if (instructions.length === 0) throw clientFailure(
    'STRONGFLOW_REWORK_INSTRUCTIONS_REQUIRED',
    'Describe the bounded rework before continuing.',
  )
  return JSON.stringify({
    protocol: 'winwincode.client-rework-instructions.v1',
    candidateDigest: candidate.diffSha256,
    deliveryTaskId: input.deliveryTaskId,
    nodeId: input.nodeId,
    instructions,
  })
}

function sameScope(left: RepositoryScope, right: RepositoryScope): boolean {
  return left.kind === right.kind
    && left.organizationId === right.organizationId
    && left.workspaceId === right.workspaceId
    && left.projectId === right.projectId
    && left.repositoryId === right.repositoryId
}

function sameOwnership(
  left: DeliveryDetailProjection['ownership'],
  right: RepositoryScope,
): boolean {
  return left.organizationId === right.organizationId
    && left.workspaceId === right.workspaceId
    && left.projectId === right.projectId
    && left.repositoryId === right.repositoryId
}

function sameReadCursor(left: StrongFlowReadCursor | null, right: StrongFlowReadCursor): boolean {
  if (left === null) return false
  return left.token === right.token
    && left.deliveryId === right.deliveryId
    && left.deliveryRevision === right.deliveryRevision
    && left.runtimeLedgerRevision === right.runtimeLedgerRevision
    && left.runtimeAcceptedSequence === right.runtimeAcceptedSequence
    && left.publicationRevision === right.publicationRevision
    && sameScope(left.scope, right.scope)
    && left.eventCursor.eventId === right.eventCursor.eventId
    && left.eventCursor.sequence === right.eventCursor.sequence
    && left.eventCursor.stream.kind === right.eventCursor.stream.kind
    && left.eventCursor.stream.deliveryId === right.eventCursor.stream.deliveryId
    && sameScope(left.eventCursor.scope, right.eventCursor.scope)
}

function assertDelivery(
  delivery: DeliveryDetailProjection,
  options: StrongFlowViewModelOptions,
  currentBinding: StrongFlowStageBinding,
  currentRevision: number | null,
  announcedBinding?: StrongFlowStageBinding,
): { readonly binding: StrongFlowStageBinding, readonly stage: DeliveryStageProjection } {
  const cursor = delivery.readCursor
  if (
    delivery.kind !== 'delivery_detail'
    || delivery.schemaVersion !== SCHEMA_VERSION
    || delivery.deliveryId !== options.deliveryId
    || delivery.deliveryRevision !== cursor.deliveryRevision
    || cursor.deliveryId !== options.deliveryId
    || !sameOwnership(delivery.ownership, options.scope)
    || !sameScope(cursor.scope, options.scope)
    || !sameScope(cursor.eventCursor.scope, options.scope)
    || cursor.eventCursor.stream.kind !== 'delivery'
    || cursor.eventCursor.stream.deliveryId !== options.deliveryId
  ) throw clientFailure(
    'STRONGFLOW_DELIVERY_MISMATCH',
    'The Delivery snapshot does not match the selected StrongFlow scope.',
  )
  if (new Set(delivery.stages.map(stage => stage.id)).size !== delivery.stages.length) {
    throw clientFailure(
      'STRONGFLOW_STAGE_BINDING_MISMATCH',
      'The Delivery snapshot contains repeated StageRun identities.',
    )
  }
  const currentIndex = delivery.stages.findIndex(stage => stage.id === currentBinding.stageRunId)
  const currentStage = delivery.stages[currentIndex]
  if (!stageMatchesBinding(currentStage, currentBinding)) throw clientFailure(
    'STRONGFLOW_STAGE_BINDING_MISMATCH',
    'The selected StageRun does not have the expected ProductSession binding.',
  )
  const activeIndex = delivery.stages.findLastIndex(stage => (
    stage.actorType === 'codex' && stage.sessionBinding !== null
  ))
  const stage = delivery.stages[activeIndex]
  if (stage === undefined || stage.actorType !== 'codex' || stage.sessionBinding === null) {
    throw clientFailure(
      'STRONGFLOW_STAGE_BINDING_MISMATCH',
      'The Delivery has no canonical Codex StageRun binding.',
    )
  }
  const binding = Object.freeze({
    productSessionId: stage.sessionBinding.productSessionId,
    stageRunId: stage.id,
  })
  if (!stageMatchesBinding(stage, binding)) throw clientFailure(
    'STRONGFLOW_STAGE_BINDING_MISMATCH',
    'The active StageRun has an inconsistent ProductSession binding.',
  )
  if (
    activeIndex < currentIndex
    || (
      currentRevision !== null
      && delivery.deliveryRevision === currentRevision
      && !sameStageBinding(binding, currentBinding)
    )
  ) throw clientFailure(
    'STRONGFLOW_STAGE_BINDING_ROLLBACK',
    'The Delivery attempted to move StrongFlow back to an older StageRun binding.',
  )
  if (announcedBinding !== undefined && !sameStageBinding(binding, announcedBinding)) {
    throw clientFailure(
      'STRONGFLOW_RUNTIME_EVENT_MISMATCH',
      'The runtime invalidation does not name the active canonical StageRun.',
    )
  }
  return Object.freeze({ binding, stage })
}

function announcedBindingIndex(
  delivery: DeliveryDetailProjection,
  announcedBinding: StrongFlowStageBinding,
): number {
  const index = delivery.stages.findIndex(stage => stage.id === announcedBinding.stageRunId)
  const stage = index < 0 ? undefined : delivery.stages[index]
  if (!stageMatchesBinding(stage, announcedBinding)) throw clientFailure(
    'STRONGFLOW_RUNTIME_EVENT_MISMATCH',
    'The runtime invalidation does not name a StageRun in the current Delivery.',
  )
  return index
}

function sameStageBinding(left: StrongFlowStageBinding, right: StrongFlowStageBinding): boolean {
  return left.productSessionId === right.productSessionId && left.stageRunId === right.stageRunId
}

function stageMatchesBinding(
  stage: DeliveryStageProjection | undefined,
  binding: StrongFlowStageBinding,
): boolean {
  return stage !== undefined
    && stage.actorType === 'codex'
    && stage.sessionBinding !== null
    && stage.sessionBinding.productSessionId === binding.productSessionId
    && (
      stage.sessionBinding.stageRunId === null
      || stage.sessionBinding.stageRunId === binding.stageRunId
    )
}

function assertRuntime(
  runtime: RuntimeProjectionSnapshot,
  cursor: StrongFlowReadCursor,
  options: StrongFlowViewModelOptions,
  binding: StrongFlowStageBinding,
): void {
  if (
    runtime.kind !== 'runtime_projection'
    || runtime.productSessionId !== binding.productSessionId
    || runtime.deliveryId !== options.deliveryId
    || runtime.stageRunId !== binding.stageRunId
    || runtime.revision !== cursor.runtimeLedgerRevision
    || runtime.lastProjectionSequence !== cursor.runtimeAcceptedSequence
    || !sameReadCursor(runtime.readCursor, cursor)
    || runtime.eventCursor.eventId !== cursor.eventCursor.eventId
    || runtime.eventCursor.sequence !== cursor.eventCursor.sequence
    || runtime.eventCursor.stream.kind !== 'delivery'
    || runtime.eventCursor.stream.deliveryId !== options.deliveryId
    || !sameScope(runtime.eventCursor.scope, options.scope)
    || runtime.sessions.some(session => (
      session.productSessionId !== binding.productSessionId
      || session.stageRunId !== binding.stageRunId
    ))
  ) throw clientFailure(
    'STRONGFLOW_RUNTIME_MISMATCH',
    'The runtime projection does not match the Delivery read cursor.',
  )
}

function assertCandidateIntegrity(delivery: DeliveryDetailProjection): void {
  const candidate = delivery.currentCandidate
  const verdict = delivery.verdict
  if (
    candidate !== null
    && (
      candidate.deliverySpecId !== delivery.requirements.deliverySpecId
      || candidate.deliverySpecRevision !== delivery.requirements.deliverySpecRevision
    )
  ) throw clientFailure(
    'STRONGFLOW_CANDIDATE_SPEC_MISMATCH',
    'The current candidate belongs to another Delivery specification.',
  )
  if (
    verdict !== null
    && (
      candidate === null
      || verdict.candidateRef !== candidate.candidateRef
      || verdict.deliverySpecId !== candidate.deliverySpecId
      || verdict.deliverySpecRevision !== candidate.deliverySpecRevision
    )
  ) throw clientFailure(
    'STRONGFLOW_VERDICT_CANDIDATE_MISMATCH',
    'The Delivery verdict belongs to another candidate and was rejected.',
  )
  const publication = delivery.publication
  if (
    (publication === null && delivery.readCursor.publicationRevision !== 0)
    || (
      publication !== null
      && (
        candidate === null
        || verdict === null
        || publication.deliveryId !== delivery.deliveryId
        || publication.deliverySpecId !== candidate.deliverySpecId
        || publication.deliverySpecRevision !== candidate.deliverySpecRevision
        || publication.candidateRef !== candidate.candidateRef
        || publication.deliveryVerdictId !== verdict.id
        || publication.verdictStatus !== 'pass'
        || publication.revision !== delivery.readCursor.publicationRevision
      )
    )
  ) throw clientFailure(
    'STRONGFLOW_PUBLICATION_MISMATCH',
    'The Publication snapshot does not match the current candidate and verdict.',
  )
}

function candidateDigestFromReference(candidateRef: string): string | null {
  const digest = candidateRef.startsWith('git-candidate:')
    ? candidateRef.slice('git-candidate:'.length)
    : null
  return digest !== null && /^sha256:[0-9a-f]{64}$/u.test(digest) ? digest : null
}

/** The authority exposes a verdict action only after every active StageRun settles. */
export function canSubmitStrongFlowVerdict(projection: StrongFlowProjection): boolean {
  return projection.currentCandidate !== null
    && projection.verdict === null
    && !projection.delivery.stages.some(stage => ['running', 'waiting'].includes(stage.status))
}

function projectionUpdatedAt(snapshot: StrongFlowSnapshot): string {
  const { delivery, runtime } = snapshot
  const timestamps = [
    runtime.rebuiltAt,
    delivery.currentCandidate?.frozenAt,
    delivery.verdict?.producedAt,
    delivery.publication?.approvedAt,
    delivery.publication?.updatedAt,
    delivery.solutionReview?.reviewedAt,
    ...delivery.stages.flatMap(stage => [stage.startedAt, stage.finishedAt]),
    ...delivery.evidence.map(item => item.createdAt),
    ...delivery.attention.flatMap(item => [item.createdAt, item.resolvedAt]),
  ].filter((value): value is string => value !== null && value !== undefined)
  if (timestamps.some(value => Number.isNaN(Date.parse(value)))) throw clientFailure(
    'STRONGFLOW_TIMESTAMP_INVALID',
    'A StrongFlow projection timestamp is invalid.',
  )
  const updatedAt = timestamps.sort().at(-1)
  if (updatedAt === undefined) throw clientFailure(
    'STRONGFLOW_TIMESTAMP_MISSING',
    'The StrongFlow projection has no authoritative update time.',
  )
  return updatedAt
}

function projectionFromSnapshot(snapshot: StrongFlowSnapshot): StrongFlowProjection {
  const { delivery, runtime, stage } = snapshot
  assertCandidateIntegrity(delivery)
  return Object.freeze({
    delivery,
    solutionReview: delivery.solutionReview,
    stage,
    runtime,
    evidence: Object.freeze([...delivery.evidence]),
    verdict: delivery.verdict,
    attention: Object.freeze([...delivery.attention]),
    publication: delivery.publication,
    currentCandidate: delivery.currentCandidate,
    metadata: Object.freeze({
      source: 'control-plane-snapshot',
      updatedAt: projectionUpdatedAt(snapshot),
      revisions: Object.freeze({
        delivery: delivery.deliveryRevision,
        deliverySpec: delivery.requirements.deliverySpecRevision,
        runtime: runtime.revision,
        publication: delivery.readCursor.publicationRevision,
      }),
      readCursor: delivery.readCursor,
    }),
  })
}

function createdDeliveryMatchesScope(
  delivery: DeliveryProjection,
  deliveryId: DeliveryId,
  scope: RepositoryScope,
): boolean {
  return delivery.deliveryId === deliveryId
    && delivery.ownership.organizationId === scope.organizationId
    && delivery.ownership.workspaceId === scope.workspaceId
    && delivery.ownership.projectId === scope.projectId
    && delivery.ownership.repositoryId === scope.repositoryId
}

/** Create the first Delivery through canonical commands before the read-model workbench mounts. */
export function createStrongFlowCreateViewModel(
  options: StrongFlowCreateViewModelOptions,
): StrongFlowCreateViewModel {
  const listeners = new Set<StrongFlowCreateListener>()
  let currentState: StrongFlowCreateState = Object.freeze({ status: 'idle', error: null })
  let active: AbortController | null = null
  let closed = false
  let attempt: {
    readonly inputKey: string
    readonly createRequest: DeliveryCreateCommand
    created: DeliveryProjection | null
    advanceRequest: DeliveryAdvanceCommand | null
  } | null = null

  function publish(status: StrongFlowCreateStatus, error: ControlPlaneClientError | null): void {
    currentState = Object.freeze({ status, error })
    for (const listener of listeners) listener(currentState)
  }

  function invalid(code: string, message: string): void {
    publish('error', clientFailure(code, message))
  }

  function completedDelivery(
    response: CommandAcceptedResponse | CommandCompletedResponse,
    command: CommandName.DeliveryCreate | CommandName.DeliveryAdvance,
    requestId: RequestId,
    deliveryId: DeliveryId,
  ): DeliveryProjection | null {
    const completed = expectCompletedCommand(response, command, requestId)
    if (completed === null) return null
    if (
      !createdDeliveryMatchesScope(completed.result, deliveryId, options.scope)
      || completed.result.revision !== completed.currentRevision
    ) throw clientFailure(
      'STRONGFLOW_CREATE_RESPONSE_MISMATCH',
      'The Delivery command returned another repository revision.',
    )
    return completed.result
  }

  async function create(input: StrongFlowCreateInput): Promise<void> {
    if (closed) throw clientFailure(
      'STRONGFLOW_CREATE_VIEW_MODEL_CLOSED',
      'The StrongFlow creation view-model is closed.',
    )
    if (currentState.status === 'submitting' || currentState.status === 'waiting') {
      return
    }
    const title = input.title.trim()
    const goal = input.goal.trim()
    const baseRevision = input.baseRevision.trim()
    const acceptanceCriteria = input.acceptanceCriteria
      .map(value => value.trim())
      .filter(value => value.length > 0)
    if (title.length === 0) {
      invalid('STRONGFLOW_CREATE_TITLE_REQUIRED', 'Enter a title for the new Delivery.')
      return
    }
    if (goal.length === 0) {
      invalid('STRONGFLOW_CREATE_GOAL_REQUIRED', 'Enter the Delivery goal.')
      return
    }
    if (baseRevision.length === 0) {
      invalid('STRONGFLOW_CREATE_BASE_REVISION_REQUIRED', 'Enter the repository baseline revision.')
      return
    }
    if (acceptanceCriteria.length === 0) {
      invalid(
        'STRONGFLOW_CREATE_ACCEPTANCE_REQUIRED',
        'Enter at least one initial acceptance criterion.',
      )
      return
    }
    const inputKey = JSON.stringify({ title, goal, baseRevision, acceptanceCriteria })
    if (attempt !== null && attempt.inputKey !== inputKey) {
      invalid(
        'STRONGFLOW_CREATE_DRAFT_CHANGED_AFTER_SUBMIT',
        'Retry the submitted Delivery draft before starting another conversion.',
      )
      return
    }
    if (currentState.status === 'created') return
    if (attempt === null) {
      const deliveryId = options.nextDeliveryId()
      attempt = {
        inputKey,
        createRequest: {
          schemaVersion: SCHEMA_VERSION,
          requestId: options.nextRequestId(),
          actor: options.actor,
          scope: options.scope,
          command: CommandName.DeliveryCreate,
          expectedRevision: 0,
          payload: {
            deliveryId,
            spec: {
              acceptanceCriteria: acceptanceCriteria.map((criterion, index) => ({
                id: `criterion:${String(index + 1)}`,
                required: true,
                title: criterion,
              })),
              baseRevision,
              goal,
              publicationTarget: null,
              repositoryId: options.scope.repositoryId,
              title,
            },
            tasks: [],
          },
        },
        created: null,
        advanceRequest: null,
      }
    }
    const currentAttempt = attempt
    const deliveryId = currentAttempt.createRequest.payload.deliveryId
    active?.abort()
    const controller = new AbortController()
    active = controller
    publish('submitting', null)
    try {
      if (currentAttempt.created === null) {
        const createResponse = await options.client.command(
          currentAttempt.createRequest,
          { signal: controller.signal },
        )
        if (closed || active !== controller) return
        const created = completedDelivery(
          createResponse,
          CommandName.DeliveryCreate,
          currentAttempt.createRequest.requestId,
          deliveryId,
        )
        if (created === null) {
          publish('waiting', null)
          return
        }
        currentAttempt.created = created
      }
      currentAttempt.advanceRequest ??= {
        schemaVersion: SCHEMA_VERSION,
        requestId: options.nextRequestId(),
        actor: options.actor,
        scope: options.scope,
        command: CommandName.DeliveryAdvance,
        expectedRevision: currentAttempt.created.revision,
        payload: { deliveryId },
      }
      const advanceRequest = currentAttempt.advanceRequest
      const advanceResponse = await options.client.command(
        advanceRequest,
        { signal: controller.signal },
      )
      if (closed || active !== controller) return
      const advanced = completedDelivery(
        advanceResponse,
        CommandName.DeliveryAdvance,
        advanceRequest.requestId,
        deliveryId,
      )
      if (advanced === null) {
        publish('waiting', null)
        return
      }
      if (advanced.activeStageRunId === null) throw clientFailure(
        'STRONGFLOW_CREATE_STAGE_REQUIRED',
        'The new Delivery did not expose its executable StrongFlow stage.',
      )
      publish('created', null)
      options.onCreated(deliveryId)
    } catch (error) {
      if (closed || active !== controller) return
      publish('error', normalizedError(error, controller.signal))
    } finally {
      if (active === controller) active = null
    }
  }

  return {
    get state() {
      return currentState
    },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async create(input) {
      await create(input)
    },
    cancelPending() {
      if (closed) return
      if (active === null) {
        if (currentState.status === 'waiting') publish('error', new ControlPlaneClientError({
          kind: 'cancelled',
          code: 'REQUEST_CANCELLED',
          message: 'Delivery creation was cancelled locally.',
          requestId: null,
          retryable: false,
        }))
        return
      }
      const controller = active
      active = null
      controller.abort()
      publish('error', new ControlPlaneClientError({
        kind: 'cancelled',
        code: 'REQUEST_CANCELLED',
        message: 'Delivery creation was cancelled locally.',
        requestId: null,
        retryable: false,
      }))
    },
    close() {
      if (closed) return
      closed = true
      active?.abort()
      active = null
      publish('closed', null)
      listeners.clear()
    },
  }
}

/** Build the exact StrongFlow read model from one bounded HTTP pair and one event stream. */
export function createStrongFlowViewModel(
  options: StrongFlowViewModelOptions,
): StrongFlowViewModel {
  const listeners = new Set<StrongFlowViewModelListener>()
  const controllers = new Set<AbortController>()
  let currentState = initialState()
  let realtime: ControlPlaneSubscription | null = null
  let generation = 0
  let closed = false
  let activeBinding: StrongFlowStageBinding = Object.freeze({
    productSessionId: options.productSessionId,
    stageRunId: options.stageRunId,
  })
  let acceptedDeliveryRevision: number | null = null
  let supersedingGeneration: number | null = null

  function publish(state: StrongFlowViewModelState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<StrongFlowViewModelState>): void {
    publish({ ...currentState, ...update })
  }

  function controller(): AbortController {
    const value = new AbortController()
    controllers.add(value)
    return value
  }

  function releaseController(value: AbortController): void {
    controllers.delete(value)
  }

  function abortRequests(): void {
    for (const active of controllers) active.abort()
    controllers.clear()
  }

  function closeRealtime(): void {
    const active = realtime
    realtime = null
    active?.close()
  }

  function operationIsCurrent(ownGeneration: number): boolean {
    return !closed && ownGeneration === generation
  }

  function requestBase() {
    return {
      schemaVersion: SCHEMA_VERSION,
      actor: options.actor,
      scope: options.scope,
    }
  }

  async function querySnapshot(
    signal: AbortSignal,
    minimum: StrongFlowSnapshotMinimum,
  ): Promise<StrongFlowSnapshot> {
    let restarts = 0
    let consistencyRetries = 0
    async function retryForConsistency(): Promise<boolean> {
      if (consistencyRetries >= MAX_SNAPSHOT_CONSISTENCY_RETRIES) return false
      consistencyRetries += 1
      await waitForSnapshotConsistencyRetry(signal)
      return true
    }
    for (;;) {
      const deliveryResponse = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.DeliveryGet,
        parameters: { deliveryId: options.deliveryId },
        page: requestPage(),
      }, { signal }), QueryName.DeliveryGet)
      const deliveryEventSequence = deliveryResponse.result.readCursor.eventCursor.sequence
      if (
        minimum.eventSequence !== undefined
        && deliveryEventSequence < minimum.eventSequence
      ) {
        if (await retryForConsistency()) continue
        throw clientFailure(
          'STRONGFLOW_SNAPSHOT_STALE',
          'The StrongFlow snapshot is older than its invalidation event.',
        )
      }
      const invalidationWasSuperseded = minimum.eventSequence !== undefined
        && deliveryEventSequence > minimum.eventSequence
      const consistencyMinimum = invalidationWasSuperseded ? {} : minimum
      const isBehindAnnouncedDelivery = consistencyMinimum.deliveryRevision !== undefined
        && deliveryResponse.result.deliveryRevision < consistencyMinimum.deliveryRevision
        && (
          acceptedDeliveryRevision === null
          || consistencyMinimum.deliveryRevision > acceptedDeliveryRevision
        )
      if (isBehindAnnouncedDelivery) {
        if (await retryForConsistency()) continue
        throw clientFailure(
          'STRONGFLOW_SNAPSHOT_STALE',
          'The StrongFlow snapshot is older than its invalidation event.',
        )
      }
      const active = assertDelivery(
        deliveryResponse.result,
        options,
        activeBinding,
        acceptedDeliveryRevision,
        consistencyMinimum.announcedBinding,
      )
      if (minimum.announcedBinding !== undefined) {
        const announcedIndex = announcedBindingIndex(
          deliveryResponse.result,
          minimum.announcedBinding,
        )
        if (
          invalidationWasSuperseded
          && announcedIndex > deliveryResponse.result.stages.findIndex(
            stage => stage.id === active.binding.stageRunId,
          )
        ) throw clientFailure(
          'STRONGFLOW_RUNTIME_EVENT_MISMATCH',
          'The runtime invalidation names a future StageRun.',
        )
      }
      if (acceptedDeliveryRevision !== null
        && deliveryResponse.result.deliveryRevision < acceptedDeliveryRevision) throw clientFailure(
        'STRONGFLOW_SNAPSHOT_STALE',
        'The StrongFlow snapshot is older than its accepted revision.',
      )
      if (consistencyMinimum.deliveryRevision !== undefined
        && deliveryResponse.result.deliveryRevision < consistencyMinimum.deliveryRevision) {
        if (await retryForConsistency()) continue
        throw clientFailure(
          'STRONGFLOW_SNAPSHOT_STALE',
          'The StrongFlow snapshot is older than its invalidation event.',
        )
      }
      let runtimeResponse: RuntimeProjectionGetResultResponse
      try {
        runtimeResponse = expectResponse(await options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.RuntimeProjectionGet,
          parameters: {
            kind: 'delivery-stage',
            productSessionId: active.binding.productSessionId,
            deliveryId: options.deliveryId,
            stageRunId: active.binding.stageRunId,
            atCursor: deliveryResponse.result.readCursor,
          },
          page: requestPage(),
        }, { signal }), QueryName.RuntimeProjectionGet)
      } catch (error) {
        if (
          error instanceof ControlPlaneClientError
          && error.code === 'READ_CURSOR_EXPIRED'
          && restarts < MAX_READ_CURSOR_RESTARTS
        ) {
          restarts += 1
          continue
        }
        throw error
      }
      assertRuntime(
        runtimeResponse.result,
        deliveryResponse.result.readCursor,
        options,
        active.binding,
      )
      if (
        (consistencyMinimum.runtimeRevision !== undefined
          && runtimeResponse.result.revision < consistencyMinimum.runtimeRevision)
        || (consistencyMinimum.runtimeSequence !== undefined
          && runtimeResponse.result.lastProjectionSequence < consistencyMinimum.runtimeSequence)
      ) {
        if (await retryForConsistency()) continue
        throw clientFailure(
          'STRONGFLOW_SNAPSHOT_STALE',
          'The StrongFlow snapshot is older than its invalidation event.',
        )
      }
      return Object.freeze({
        binding: active.binding,
        delivery: deliveryResponse.result,
        runtime: runtimeResponse.result,
        stage: active.stage,
      })
    }
  }

  async function completeSnapshot(
    ownGeneration: number,
    realtimeStatus: StrongFlowRealtimeStatus,
    minimum: StrongFlowSnapshotMinimum = {},
  ): Promise<EventReadCursor | null> {
    const active = controller()
    let published = false
    try {
      const snapshot = await querySnapshot(active.signal, minimum)
      if (!operationIsCurrent(ownGeneration)) return null
      const projection = projectionFromSnapshot(snapshot)
      if (!sameStageBinding(activeBinding, snapshot.binding)) {
        options.onStageBindingChange?.(snapshot.binding)
        activeBinding = snapshot.binding
      }
      acceptedDeliveryRevision = snapshot.delivery.deliveryRevision
      publish({
        status: 'ready',
        realtime: realtimeStatus,
        projection,
        interaction: frozenInteraction('idle'),
        error: null,
      })
      published = true
      return projection.metadata.readCursor.eventCursor
    } catch (error) {
      if (!operationIsCurrent(ownGeneration)) return null
      const normalized = normalizedError(error, active.signal)
      publish({
        status: statusForError(normalized),
        realtime: normalized.kind === 'authentication' || normalized.kind === 'authorization'
          ? 'access-revoked'
          : 'reconnecting',
        projection: null,
        interaction: frozenInteraction('error', normalized),
        error: normalized,
      })
      throw normalized
    } finally {
      if (supersedingGeneration === ownGeneration && !published) supersedingGeneration = null
      releaseController(active)
    }
  }

  function beginReload(
    status: StrongFlowViewStatus,
    retainProjection = false,
  ): void {
    publish({
      status,
      realtime: realtime === null ? 'inactive' : 'reloading',
      projection: retainProjection ? currentState.projection : null,
      interaction: frozenInteraction('idle'),
      error: null,
    })
  }

  function validateEvent(frame: ControlPlaneWebSocketEventFrame): void {
    const event = frame.event
    if ('deliveryId' in event && event.deliveryId !== options.deliveryId) throw clientFailure(
      'STRONGFLOW_EVENT_DELIVERY_MISMATCH',
      'A StrongFlow event belongs to another Delivery.',
    )
    if (event.type === ControlPlaneWebSocketEventType.RuntimeProjectionInvalidatedV1) {
      if (
        event.scopeKind !== 'delivery-stage'
        || event.deliveryId !== options.deliveryId
        || event.reloadQueries.length !== 2
        || event.reloadQueries[0] !== QueryName.DeliveryGet
        || event.reloadQueries[1] !== QueryName.RuntimeProjectionGet
      ) throw clientFailure(
        'STRONGFLOW_RUNTIME_EVENT_MISMATCH',
        'The runtime invalidation does not match the selected Delivery StageRun.',
      )
    }
  }

  async function reloadFromEvent(frame: ControlPlaneWebSocketEventFrame): Promise<void> {
    validateEvent(frame)
    const runtimeEvent = frame.event.type
      === ControlPlaneWebSocketEventType.RuntimeProjectionInvalidatedV1
      && frame.event.scopeKind === 'delivery-stage'
      ? frame.event
      : null
    const minimum: StrongFlowSnapshotMinimum = frame.event.type
      === ControlPlaneWebSocketEventType.DeliveryChangedV1
      ? {
          eventSequence: frame.sequence,
          deliveryRevision: frame.event.revision,
        }
      : runtimeEvent !== null
        ? {
            eventSequence: frame.sequence,
            announcedBinding: Object.freeze({
              productSessionId: runtimeEvent.productSessionId,
              stageRunId: runtimeEvent.stageRunId,
            }),
            ...(runtimeEvent.lastProjectionSequence === 0
              ? { deliveryRevision: runtimeEvent.projectionRevision }
              : {
                  runtimeRevision: runtimeEvent.projectionRevision,
                  runtimeSequence: runtimeEvent.lastProjectionSequence,
                }),
          }
        : {}
    generation += 1
    const ownGeneration = generation
    supersedingGeneration = ownGeneration
    abortRequests()
    beginReload('refreshing', true)
    await completeSnapshot(ownGeneration, 'subscribed', minimum)
  }

  function accessRevoked(error: ControlPlaneClientError): void {
    generation += 1
    supersedingGeneration = null
    abortRequests()
    closeRealtime()
    publish({
      status: statusForError(error),
      realtime: 'access-revoked',
      projection: null,
      interaction: frozenInteraction('error', error),
      error,
    })
  }

  function subscribeRealtime(cursor: EventReadCursor): void {
    closeRealtime()
    let ownRealtime: ControlPlaneSubscription | null = null
    ownRealtime = options.client.subscribe({
      subscriptionId: options.subscriptionId,
      subscription: {
        scope: options.scope,
        stream: { kind: 'delivery', deliveryId: options.deliveryId },
        eventTypes: [
          ControlPlaneWebSocketEventType.DeliveryChangedV1,
          ControlPlaneWebSocketEventType.DeliveryTaskChangedV1,
          ControlPlaneWebSocketEventType.AttentionChangedV1,
          ControlPlaneWebSocketEventType.RuntimeProjectionInvalidatedV1,
        ],
      },
      startAt: cursor,
      onEvent(frame) {
        if (closed || realtime !== ownRealtime) return
        return reloadFromEvent(frame)
      },
      async onResetRequired() {
        if (closed || realtime !== ownRealtime) throw clientFailure(
          'STRONGFLOW_RESET_SUPERSEDED',
          'The StrongFlow reset was replaced by a newer subscription.',
        )
        generation += 1
        const ownGeneration = generation
        supersedingGeneration = ownGeneration
        abortRequests()
        beginReload('refreshing')
        const next = await completeSnapshot(ownGeneration, 'subscribed')
        if (next === null) throw clientFailure(
          'STRONGFLOW_RESET_SUPERSEDED',
          'The StrongFlow reset was replaced by a newer operation.',
        )
        return next
      },
      onAuthorizationRevoked() {
        if (closed || realtime !== ownRealtime) return
        accessRevoked(new ControlPlaneClientError({
          kind: 'authentication',
          code: 'AUTHENTICATION_REQUIRED',
          message: 'The StrongFlow subscription authorization is no longer valid.',
          requestId: null,
          retryable: false,
        }))
      },
      onError(error) {
        if (closed || realtime !== ownRealtime) return
        if (error.kind === 'authentication' || error.kind === 'authorization') {
          accessRevoked(error)
          return
        }
        if (
          error.code === 'REQUEST_CANCELLED'
          && error.requestId === null
          && supersedingGeneration === generation
        ) {
          supersedingGeneration = null
          return
        }
        if (supersedingGeneration === generation) supersedingGeneration = null
        patch({ realtime: 'reconnecting', error })
      },
    })
    realtime = ownRealtime
    patch({ realtime: 'subscribed' })
  }

  async function initialLoad(): Promise<void> {
    if (closed) throw clientFailure(
      'STRONGFLOW_VIEW_MODEL_CLOSED',
      'The StrongFlow view-model is closed.',
    )
    generation += 1
    const ownGeneration = generation
    abortRequests()
    closeRealtime()
    beginReload('loading')
    try {
      const cursor = await completeSnapshot(ownGeneration, 'inactive')
      if (cursor === null || !operationIsCurrent(ownGeneration)) return
      subscribeRealtime(cursor)
    } catch {
      // completeSnapshot already published one bounded error.
    }
  }

  async function refresh(): Promise<void> {
    if (closed) throw clientFailure(
      'STRONGFLOW_VIEW_MODEL_CLOSED',
      'The StrongFlow view-model is closed.',
    )
    generation += 1
    const ownGeneration = generation
    abortRequests()
    beginReload('refreshing', true)
    try {
      await completeSnapshot(ownGeneration, realtime === null ? 'inactive' : 'subscribed')
    } catch {
      // completeSnapshot already published one bounded error.
    }
  }

  function interactionFailure(code: string, message: string): void {
    patch({ interaction: frozenInteraction('error', clientFailure(code, message)) })
  }

  function commandProjection(): StrongFlowProjection | null {
    const projection = currentState.projection
    if (projection === null) {
      interactionFailure(
        'STRONGFLOW_DECISION_SNAPSHOT_REQUIRED',
        'Refresh StrongFlow before making a decision.',
      )
    }
    return projection
  }

  async function runCommand<Command extends keyof StrongFlowCommandResponses>(
    command: Command,
    request: (projection: StrongFlowProjection, requestId: RequestId) => Parameters<
      ControlPlaneClient['command']
    >[0],
  ): Promise<void> {
    if (closed) throw clientFailure(
      'STRONGFLOW_VIEW_MODEL_CLOSED',
      'The StrongFlow view-model is closed.',
    )
    if (
      currentState.interaction.status === 'submitting'
      || currentState.interaction.status === 'waiting'
    ) {
      interactionFailure(
        'STRONGFLOW_DECISION_IN_FLIGHT',
        'Wait for the current StrongFlow decision to finish.',
      )
      return
    }
    const projection = commandProjection()
    if (projection === null) return
    const ownGeneration = generation
    const active = controller()
    let requestId = options.nextRequestId()
    let commandRequest = request(projection, requestId)
    const trustedFactsRetryDeadline = Date.now()
      + TRUSTED_FACTS_COMMAND_RETRY_DEADLINE_MILLIS
    patch({ interaction: frozenInteraction('submitting') })
    try {
      let trustedFactsRetries = 0
      let response: CommandAcceptedResponse | CommandCompletedResponse
      for (;;) {
        try {
          response = await options.client.command(commandRequest, {
            signal: active.signal,
          })
          break
        } catch (error) {
          if (
            !(error instanceof ControlPlaneClientError)
            || error.code !== 'TRUSTED_FACTS_UNAVAILABLE'
            || error.retryable !== true
            || trustedFactsRetries >= MAX_TRUSTED_FACTS_COMMAND_RETRIES
            || Date.now() >= trustedFactsRetryDeadline
          ) throw error
          trustedFactsRetries += 1
          await waitForRetry(active.signal, TRUSTED_FACTS_COMMAND_RETRY_DELAY_MILLIS)
          if (!operationIsCurrent(ownGeneration)) return
          requestId = options.nextRequestId()
          commandRequest = { ...commandRequest, requestId }
        }
      }
      if (!operationIsCurrent(ownGeneration)) return
      expectCompletedCommand(response, command, requestId)
      patch({ interaction: frozenInteraction('waiting') })
    } catch (error) {
      if (!operationIsCurrent(ownGeneration)) return
      patch({ interaction: frozenInteraction('error', normalizedError(error, active.signal)) })
    } finally {
      releaseController(active)
    }
  }

  async function decideSolutionReview(
    input: StrongFlowSolutionReviewDecisionInput,
  ): Promise<void> {
    const projection = commandProjection()
    if (projection === null) return
    let resolution: string
    try {
      resolution = planReviewDecisionText(projection, input)
    } catch (error) {
      patch({ interaction: frozenInteraction('error', normalizedError(error)) })
      return
    }
    const review = projection.solutionReview
    if (review === null) return
    await runCommand(CommandName.DeliveryResolveAttention, (current, requestId) => ({
      ...requestBase(),
      requestId,
      command: CommandName.DeliveryResolveAttention,
      expectedRevision: current.metadata.revisions.delivery,
      payload: {
        deliveryId: current.delivery.deliveryId,
        attentionItemId: review.attentionItemId,
        decision: 'resolve',
        resolution,
        remediation: null,
      },
    }))
  }

  async function approveTaskBreakdown(): Promise<void> {
    const projection = commandProjection()
    if (projection === null) return
    const review = projection.solutionReview
    if (review === null || review.reviewStatus !== 'approved') {
      interactionFailure(
        'STRONGFLOW_APPROVED_REVIEW_REQUIRED',
        'Approve the current solution review before promoting its task breakdown.',
      )
      return
    }
    await runCommand(CommandName.DeliveryApproveTaskBreakdown, (current, requestId) => ({
      ...requestBase(),
      requestId,
      command: CommandName.DeliveryApproveTaskBreakdown,
      expectedRevision: current.metadata.revisions.delivery,
      payload: {
        deliveryId: current.delivery.deliveryId,
        reviewSetSha256: review.reviewSetSha256,
      },
    }))
  }

  async function resolveAttention(input: StrongFlowAttentionDecisionInput): Promise<void> {
    const projection = commandProjection()
    if (projection === null) return
    const attention = projection.attention.find(item => item.id === input.attentionItemId)
    if (attention === undefined || attention.status !== 'open') {
      interactionFailure(
        'STRONGFLOW_ATTENTION_STALE',
        'Refresh StrongFlow and select an open Attention record.',
      )
      return
    }
    const resolution = input.resolution.trim()
    if (resolution.length === 0) {
      interactionFailure(
        'STRONGFLOW_ATTENTION_RESOLUTION_REQUIRED',
        'Explain the Attention decision before continuing.',
      )
      return
    }
    let remediation: {
      readonly candidateDigest: NonNullable<StrongFlowProjection['currentCandidate']>['diffSha256']
      readonly deliveryTaskId: DeliveryTaskId | null
      readonly instructions: string
    } | null = null
    if (input.remediation !== null) {
      try {
        const instructions = reworkInstructions(projection, input.remediation)
        const candidate = projection.currentCandidate
        if (candidate === null) return
        remediation = {
          candidateDigest: candidate.diffSha256,
          deliveryTaskId: input.remediation.deliveryTaskId,
          instructions,
        }
      } catch (error) {
        patch({ interaction: frozenInteraction('error', normalizedError(error)) })
        return
      }
    }
    await runCommand(CommandName.DeliveryResolveAttention, (current, requestId) => ({
      ...requestBase(),
      requestId,
      command: CommandName.DeliveryResolveAttention,
      expectedRevision: current.metadata.revisions.delivery,
      payload: {
        deliveryId: current.delivery.deliveryId,
        attentionItemId: input.attentionItemId,
        decision: input.decision,
        resolution,
        remediation,
      },
    }))
  }

  async function submitVerdict(): Promise<void> {
    const projection = commandProjection()
    if (projection === null) return
    const candidate = projection.currentCandidate
    if (candidate === null) {
      interactionFailure(
        'STRONGFLOW_VERDICT_CANDIDATE_REQUIRED',
        'Wait for a frozen candidate before requesting a verdict.',
      )
      return
    }
    if (projection.delivery.stages.some(stage => ['running', 'waiting'].includes(stage.status))) {
      interactionFailure(
        'STRONGFLOW_VERDICT_STAGES_ACTIVE',
        'Wait for all active verification stages to finish before requesting a verdict.',
      )
      return
    }
    if (projection.verdict !== null) {
      interactionFailure(
        'STRONGFLOW_VERDICT_ALREADY_AVAILABLE',
        'The current Delivery already has a verdict.',
      )
      return
    }
    const candidateDigest = candidateDigestFromReference(candidate.candidateRef)
    if (candidateDigest === null) {
      interactionFailure(
        'STRONGFLOW_CANDIDATE_REFERENCE_INVALID',
        'The current candidate does not expose a canonical Git reference.',
      )
      return
    }
    await runCommand(CommandName.DeliverySubmitVerdict, (current, requestId) => ({
      ...requestBase(),
      requestId,
      command: CommandName.DeliverySubmitVerdict,
      expectedRevision: current.metadata.revisions.delivery,
      payload: {
        deliveryId: current.delivery.deliveryId,
        candidateDigest,
      },
    }))
  }

  async function advanceDelivery(): Promise<void> {
    await runCommand(CommandName.DeliveryAdvance, (current, requestId) => ({
      ...requestBase(),
      requestId,
      command: CommandName.DeliveryAdvance,
      expectedRevision: current.metadata.revisions.delivery,
      payload: { deliveryId: current.delivery.deliveryId },
    }))
  }

  /** Guard shared by the read-only historical review queries. */
  function requireOpenViewModel(): void {
    if (closed) throw clientFailure(
      'STRONGFLOW_VIEW_MODEL_CLOSED',
      'The StrongFlow view-model is closed.',
    )
  }

  function historicalStage(
    requestedStageRunId: StageRunId,
  ): StrongFlowProjection['delivery']['stages'][number] | undefined {
    const projection = currentState.projection
    if (projection === null) return undefined
    return projection.delivery.stages.find(stage => stage.id === requestedStageRunId)
  }

  async function loadStageRunRuntime(
    requestedStageRunId: StageRunId,
  ): Promise<RuntimeProjectionSnapshot | null> {
    requireOpenViewModel()
    const projection = currentState.projection
    if (projection === null) return null
    const stage = historicalStage(requestedStageRunId)
    if (stage === undefined || stage.actorType !== 'codex' || stage.sessionBinding === null) {
      return null
    }
    const binding = Object.freeze({
      productSessionId: stage.sessionBinding.productSessionId,
      stageRunId: stage.id,
    })
    const active = controller()
    try {
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.RuntimeProjectionGet,
        parameters: {
          kind: 'delivery-stage',
          productSessionId: binding.productSessionId,
          deliveryId: options.deliveryId,
          stageRunId: binding.stageRunId,
          atCursor: projection.delivery.readCursor,
        },
        page: requestPage(),
      }, { signal: active.signal }), QueryName.RuntimeProjectionGet)
      assertRuntime(response.result, projection.delivery.readCursor, options, binding)
      return response.result
    } finally {
      releaseController(active)
    }
  }

  async function loadStageRunCandidates(
    requestedStageRunId: StageRunId,
  ): Promise<readonly CandidateHistoryItemProjection[]> {
    requireOpenViewModel()
    const projection = currentState.projection
    if (projection === null) return []
    const active = controller()
    try {
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.CandidateList,
        parameters: {
          deliveryId: options.deliveryId,
          atCursor: projection.delivery.readCursor,
          readPageLimit: HISTORICAL_CANDIDATE_PAGE_LIMIT,
        },
        page: { cursor: null, limit: HISTORICAL_CANDIDATE_PAGE_LIMIT },
      }, { signal: active.signal }), QueryName.CandidateList)
      const result = response.result
      if (
        result.kind !== 'candidate_history_page'
        || !sameReadCursor(result.readCursor, projection.delivery.readCursor)
      ) throw clientFailure(
        'STRONGFLOW_CANDIDATE_HISTORY_MISMATCH',
        'The Candidate history does not match the current Delivery read cursor.',
      )
      return result.items.filter(
        item => item.candidate.producerStageRunId === requestedStageRunId,
      )
    } finally {
      releaseController(active)
    }
  }

  async function loadCandidateHistoricalReview(
    candidate: StrongFlowHistoricalCandidateIdentity,
  ): Promise<CandidateHistoricalReviewProjection | null> {
    requireOpenViewModel()
    const projection = currentState.projection
    if (projection === null) return null
    const active = controller()
    try {
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.CandidateReviewGet,
        parameters: {
          deliveryId: options.deliveryId,
          atCursor: projection.delivery.readCursor,
          candidateRef: candidate.candidateRef,
          candidateTreeId: candidate.candidateTreeId,
          diffSha256: candidate.diffSha256,
          readPageLimit: HISTORICAL_CANDIDATE_PAGE_LIMIT,
        },
        page: requestPage(),
      }, { signal: active.signal }), QueryName.CandidateReviewGet)
      const result = response.result
      if (
        result.kind !== 'candidate_historical_review'
        || result.candidate.candidateRef !== candidate.candidateRef
        || result.candidate.candidateTreeId !== candidate.candidateTreeId
        || result.candidate.diffSha256 !== candidate.diffSha256
        || result.displayOnly !== true
        || result.currentAuthorization !== false
      ) throw clientFailure(
        'STRONGFLOW_CANDIDATE_REVIEW_MISMATCH',
        'The Control Plane returned another historical Candidate review.',
      )
      return result
    } finally {
      releaseController(active)
    }
  }

  return {
    get state() {
      return currentState
    },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async start() {
      await initialLoad()
    },
    async refresh() {
      await refresh()
    },
    async decideSolutionReview(input) {
      await decideSolutionReview(input)
    },
    async approveTaskBreakdown() {
      await approveTaskBreakdown()
    },
    async resolveAttention(input) {
      await resolveAttention(input)
    },
    async submitVerdict() {
      await submitVerdict()
    },
    async advanceDelivery() {
      await advanceDelivery()
    },
    async loadStageRunRuntime(stageRunId) {
      return loadStageRunRuntime(stageRunId)
    },
    async loadStageRunCandidates(stageRunId) {
      return loadStageRunCandidates(stageRunId)
    },
    async loadCandidateHistoricalReview(candidate) {
      return loadCandidateHistoricalReview(candidate)
    },
    cancelPending() {
      if (closed) return
      generation += 1
      supersedingGeneration = null
      abortRequests()
      closeRealtime()
      publish({
        status: 'cancelled',
        realtime: 'inactive',
        projection: null,
        interaction: frozenInteraction('error', new ControlPlaneClientError({
          kind: 'cancelled',
          code: 'REQUEST_CANCELLED',
          message: 'The StrongFlow projection request was cancelled.',
          requestId: null,
          retryable: false,
        })),
        error: new ControlPlaneClientError({
          kind: 'cancelled',
          code: 'REQUEST_CANCELLED',
          message: 'The StrongFlow projection request was cancelled.',
          requestId: null,
          retryable: false,
        }),
      })
    },
    reconnect() {
      if (closed) throw clientFailure(
        'STRONGFLOW_VIEW_MODEL_CLOSED',
        'The StrongFlow view-model is closed.',
      )
      if (realtime === null) throw clientFailure(
        'STRONGFLOW_SUBSCRIPTION_INACTIVE',
        'The StrongFlow subscription is not active.',
      )
      patch({ realtime: 'reconnecting', error: null })
      realtime.reconnect()
    },
    close() {
      if (closed) return
      closed = true
      generation += 1
      supersedingGeneration = null
      abortRequests()
      closeRealtime()
      publish({
        status: 'closed',
        realtime: 'closed',
        projection: null,
        interaction: frozenInteraction('idle'),
        error: null,
      })
      listeners.clear()
    },
  }
}
