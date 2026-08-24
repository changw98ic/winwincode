import { createHash } from 'node:crypto'
import { resolve } from 'node:path'

import {
  DELIVERY_SCHEMA_VERSION,
  STRONGFLOW_VERIFICATION_ROLE_IDS,
  STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL,
  AttentionItemId,
  DeliveryId,
  DeliveryTaskId,
  SessionBindingId,
  StageRunId,
  parseAttentionItem,
  parseDelivery,
  parseDeliverySpec,
  parseDeliveryTask,
  parseFrozenDeliveryCandidate,
  parseStrongFlowDeliveryRemediation,
  parseSessionBinding,
  parseStageRun,
  type AttentionItem,
  type AttentionItemStatus,
  type Delivery,
  type DeliveryId as DeliveryIdentifier,
  type DeliverySpec,
  type DeliveryStatus,
  type DeliveryTask,
  type FrozenDeliveryCandidate,
  type RuntimeEvent,
  type SessionBinding,
  type StrongFlowDeliveryAuthentication,
  type StrongFlowDeliveryChannel,
  type StrongFlowDeliveryRemediation,
  type StrongFlowDiagramExecutionProjection,
  type StrongFlowRuntimeExecutionProjection,
  type StrongFlowVerificationRoleId,
  type StageRun,
  type StageRunActorType,
  type StageRunId as StageRunIdentifier,
  type DeliveryStage,
} from '@winwincode/contracts'

import {
  DeliveryStore,
  DeliveryStoreError,
  type DeliveryMutationOperation,
  type StoredDelivery,
} from './delivery-store.js'
import type { StrongFlowDeliveryAuthenticator } from './delivery-authenticator.js'
import { containsRawCredentialMaterial } from './credential-boundary.js'
import { freezeAcceptanceVerificationInput } from './acceptance-verification.js'
import { assertFrozenDeliveryCandidateCurrent } from './candidate-evidence.js'
import { computeDeliveryVerdict } from './delivery-verdict.js'
import {
  deliveryVerdictAttentionNextStatus,
  deriveDeliveryVerdictAttention,
  isDerivedDeliveryVerdictAttention,
} from './delivery-attention.js'
import {
  StrongFlowPlanReviewError,
  validateStrongFlowPlanReviewAttention,
  validateStrongFlowPlanReviewDecision,
  type ValidatedStrongFlowPlanReviewDecision,
} from './plan-review.js'
import {
  StrongFlowGitHubPublicationError,
  validateStrongFlowGitHubPublicationAttention,
  validateStrongFlowGitHubPublicationDecision,
  type ValidatedStrongFlowGitHubPublicationDecision,
} from './github-publication.js'
import {
  diagramExecutionAnnotationExists,
  projectStrongFlowDiagramExecution,
} from './diagram-execution-projection.js'
import { projectStrongFlowRuntimeExecution } from './runtime-execution-projection.js'
import type { StrongFlowExecutionSource } from './execution-source.js'

export const STRONGFLOW_SERVICE_SCHEMA_VERSION = DELIVERY_SCHEMA_VERSION

export type StrongFlowServiceErrorCode =
  | 'INVALID_SERVICE_OPTIONS'
  | 'INVALID_REQUEST'
  | 'DELIVERY_NOT_FOUND'
  | 'DELIVERY_CONFLICT'
  | 'REVISION_CONFLICT'
  | 'WRONG_DELIVERY_STATE'
  | 'ATTENTION_REQUIRED'
  | 'AUTHENTICATION_REQUIRED'
  | 'AUTHENTICATION_FAILED'
  | 'STORE_FAILURE'

export class StrongFlowServiceError extends Error {
  readonly code: StrongFlowServiceErrorCode
  readonly currentRevision: number | null

  constructor(
    code: StrongFlowServiceErrorCode,
    message: string,
    options: ErrorOptions & { readonly currentRevision?: number | null } = {},
  ) {
    super(message, options)
    this.name = 'StrongFlowServiceError'
    this.code = code
    this.currentRevision = options.currentRevision ?? null
  }
}

interface DeliveryMutationBase {
  readonly requestId: string
  readonly deliveryId: DeliveryIdentifier
  readonly expectedRevision: number
}

export interface CreateDeliveryInput {
  readonly requestId: string
  readonly spec: DeliverySpec
  readonly tasks: readonly DeliveryTask[]
}

export interface UpdateDeliverySpecInput extends DeliveryMutationBase {
  readonly spec: DeliverySpec
}

export interface StartStageInput extends DeliveryMutationBase {
  readonly stageRunId: StageRunIdentifier
  readonly deliveryTaskId: DeliveryTaskId | null
  readonly stage: DeliveryStage
  readonly actorType: StageRunActorType
  readonly role: string
  readonly attention: AttentionItem | null
}

export interface BindSessionInput extends DeliveryMutationBase {
  readonly bindingId: string
  readonly stageRunId: StageRunIdentifier
  readonly dshSessionId: string | null
  readonly codexSessionId: string | null
}

export interface ResolveAttentionInput extends DeliveryMutationBase {
  readonly attentionItemId: string
  readonly status: Exclude<AttentionItemStatus, 'open'>
  readonly resolution: string
  readonly remediation: StrongFlowDeliveryRemediation | null
  readonly channel: StrongFlowDeliveryChannel
  readonly authentication: StrongFlowDeliveryAuthentication
}

export interface SubmitVerdictInput extends DeliveryMutationBase {
  readonly candidate: FrozenDeliveryCandidate
  readonly runtimeEvents: readonly RuntimeEvent[]
  readonly requiredRoles: readonly StrongFlowVerificationRoleId[]
}

export interface StrongFlowServiceOptions {
  readonly home: string
  readonly authenticator: StrongFlowDeliveryAuthenticator
  readonly clock?: () => number
  readonly executionSource?: StrongFlowExecutionSource
}

export interface StrongFlowDeliveryProjection {
  readonly delivery: Delivery
  readonly diagramExecution: StrongFlowDiagramExecutionProjection | null
  readonly runtimeExecution: StrongFlowRuntimeExecutionProjection | null
}

const REQUEST_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u
const ACTOR_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u
const MAX_VERDICT_RUNTIME_EVENT_JSON_LENGTH = 16 * 1_024 * 1_024

const REVIEW_ATTENTION_TYPES: Readonly<Partial<Record<DeliveryStage, AttentionItem['type']>>> =
  Object.freeze({
    'plan-review': 'decision_required',
    'delivery-review': 'delivery_approval',
  })

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
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

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  expected: readonly string[],
  label: string,
): void {
  const keys = new Set(expected)
  if (Object.keys(value).length !== keys.size
    || expected.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !keys.has(key))) {
    throw new StrongFlowServiceError('INVALID_REQUEST', `${label} has an unexpected shape`)
  }
}

function requestRecord(value: unknown, expected: readonly string[], label: string): void {
  if (!isRecord(value)) {
    throw new StrongFlowServiceError('INVALID_REQUEST', `${label} must be an object`)
  }
  exactKeys(value, expected, label)
}

function requestId(value: unknown): string {
  if (typeof value !== 'string' || !REQUEST_ID_PATTERN.test(value)) {
    throw new StrongFlowServiceError('INVALID_REQUEST', 'requestId is invalid')
  }
  return value
}

function expectedRevision(value: unknown): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1) {
    throw new StrongFlowServiceError(
      'INVALID_REQUEST',
      'expectedRevision must be a positive safe integer',
    )
  }
  return Number(value)
}

function authenticationRequest(
  value: unknown,
  channelValue: unknown,
): {
  readonly channel: StrongFlowDeliveryChannel
  readonly authentication: StrongFlowDeliveryAuthentication
} {
  if (value === undefined) {
    throw new StrongFlowServiceError(
      'AUTHENTICATION_REQUIRED',
      'resolving business Attention requires authentication',
    )
  }
  if ((channelValue !== 'local-ui' && channelValue !== 'cli') || !isRecord(value)) {
    throw new StrongFlowServiceError(
      'INVALID_REQUEST',
      'business Attention authentication is malformed',
    )
  }
  exactKeys(value, ['scheme', 'proof'], 'resolveAttention authentication')
  if ((value.scheme !== 'local-session' && value.scheme !== 'local-peer')
    || typeof value.proof !== 'string'
    || value.proof.length < 16
    || value.proof.length > 8_192
    || ((channelValue === 'local-ui') !== (value.scheme === 'local-session'))) {
    throw new StrongFlowServiceError(
      'INVALID_REQUEST',
      'business Attention authentication is malformed',
    )
  }
  return Object.freeze({
    channel: channelValue,
    authentication: Object.freeze({ scheme: value.scheme, proof: value.proof }),
  })
}

function authenticatedActorId(value: unknown): string {
  if (typeof value !== 'string' || !ACTOR_ID_PATTERN.test(value)) {
    throw new StrongFlowServiceError(
      'AUTHENTICATION_FAILED',
      'business Attention authentication returned an invalid actor identity',
    )
  }
  return value
}

function nowFrom(clock: () => number): number {
  const value = clock()
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new StrongFlowServiceError(
      'INVALID_SERVICE_OPTIONS',
      'StrongFlow service clock returned an invalid timestamp',
    )
  }
  return value
}

function requestDigest(operation: DeliveryMutationOperation, value: unknown): string {
  return createHash('sha256').update(JSON.stringify({ operation, value })).digest('hex')
}

function sameExternalDeliveryBinding(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right)
}

function serviceFault(error: unknown): StrongFlowServiceError {
  if (error instanceof StrongFlowServiceError) return error
  if (error instanceof DeliveryStoreError) {
    switch (error.code) {
      case 'DELIVERY_NOT_FOUND':
        return new StrongFlowServiceError('DELIVERY_NOT_FOUND', error.message, { cause: error })
      case 'DELIVERY_ALREADY_EXISTS':
      case 'REQUEST_CONFLICT':
      case 'DELIVERY_ID_MISMATCH':
        return new StrongFlowServiceError('DELIVERY_CONFLICT', error.message, { cause: error })
      case 'REVISION_CONFLICT':
        return new StrongFlowServiceError('REVISION_CONFLICT', error.message, { cause: error })
      case 'INVALID_STORE_OPTIONS':
        return new StrongFlowServiceError('INVALID_REQUEST', error.message, { cause: error })
      case 'STORE_CORRUPT':
      case 'STORE_IO_ERROR':
        return new StrongFlowServiceError('STORE_FAILURE', error.message, { cause: error })
    }
  }
  return new StrongFlowServiceError(
    'INVALID_REQUEST',
    'StrongFlow delivery request is invalid',
    { cause: error },
  )
}

function cloneWithRevision(
  current: Delivery,
  now: number,
  changes: Partial<Omit<Delivery, 'schemaVersion' | 'id' | 'revision' | 'createdAtMillis'>>,
): Delivery {
  return parseDelivery({
    ...current,
    ...changes,
    revision: current.revision + 1,
    updatedAtMillis: now,
  })
}

function currentStageRun(delivery: Delivery, stageRunId: StageRunIdentifier): StageRun {
  const run = delivery.stageRuns.find(entry => entry.id === stageRunId)
  if (run === undefined) {
    throw new StrongFlowServiceError(
      'INVALID_REQUEST',
      `StageRun ${stageRunId} does not belong to Delivery ${delivery.id}`,
    )
  }
  return run
}

function activeStageRuns(delivery: Delivery): readonly StageRun[] {
  return delivery.stageRuns.filter(run => run.status === 'running' || run.status === 'waiting')
}

interface StageStartTransition {
  readonly previousRun: StageRun | null
  readonly nextStatus: DeliveryStatus
}

function stageStartTransition(
  delivery: Delivery,
  stage: DeliveryStage,
): StageStartTransition {
  if (delivery.attentionItems.some(item => item.blocking && item.status === 'open')) {
    throw new StrongFlowServiceError(
      'ATTENTION_REQUIRED',
      'a blocking AttentionItem must be resolved before another stage can start',
    )
  }
  const active = activeStageRuns(delivery)
  if (active.length > 1) {
    throw new StrongFlowServiceError(
      'DELIVERY_CONFLICT',
      'Delivery has more than one active StageRun',
    )
  }
  const previousRun = active[0] ?? null
  const transition = (nextStatus: DeliveryStatus): StageStartTransition => Object.freeze({
    previousRun,
    nextStatus,
  })
  switch (delivery.status) {
    case 'draft':
    case 'clarifying':
      if (previousRun === null && stage === 'clarifying') return transition('clarifying')
      break
    case 'ready':
      if (previousRun === null && stage === 'planning') return transition('planning')
      break
    case 'planning':
      if (previousRun === null && stage === 'planning') return transition('planning')
      if (previousRun?.stage === 'planning' && stage === 'plan-review') {
        return transition('needs-attention')
      }
      break
    case 'executing':
      if (previousRun === null && stage === 'executing') return transition('executing')
      if (previousRun?.stage === 'executing' && stage === 'verifying') {
        return transition('verifying')
      }
      break
    case 'verifying':
      if (previousRun === null && stage === 'verifying') return transition('verifying')
      if (previousRun?.stage === 'verifying' && stage === 'verifying') {
        return transition('verifying')
      }
      break
    case 'reworking':
      if (previousRun === null && stage === 'reworking') return transition('reworking')
      if (previousRun?.stage === 'reworking' && stage === 'verifying') {
        return transition('verifying')
      }
      break
    case 'ready-to-deliver':
      if (previousRun === null && stage === 'delivery-review') {
        return transition('needs-attention')
      }
      break
    case 'plan-review':
    case 'needs-attention':
    case 'delivered':
      break
  }
  throw new StrongFlowServiceError(
    delivery.status === 'needs-attention' ? 'ATTENTION_REQUIRED' : 'WRONG_DELIVERY_STATE',
    `Delivery ${delivery.id} cannot start ${stage} while ${delivery.status}`,
  )
}

function assertStageRunBound(delivery: Delivery, run: StageRun): void {
  const bound = delivery.sessionBindings.some(binding => (
    binding.stageRunId === run.id
    && (run.actorType === 'codex'
      ? binding.codexSessionId !== null
      : binding.dshSessionId !== null)
  ))
  if (!bound) {
    throw new StrongFlowServiceError(
      'WRONG_DELIVERY_STATE',
      `${run.actorType} StageRun ${run.id} must bind its owning session before it can finish`,
    )
  }
}

function stageTask(
  delivery: Delivery,
  taskId: DeliveryTaskId | null,
): DeliveryTask | null {
  if (taskId === null) return null
  const task = delivery.tasks.find(entry => entry.id === taskId)
  if (task === undefined) {
    throw new StrongFlowServiceError(
      'INVALID_REQUEST',
      'StageRun references a DeliveryTask that does not exist',
    )
  }
  const tasksById = new Map(delivery.tasks.map(entry => [entry.id, entry]))
  if (task.blockedByTaskIds.some(dependencyId => (
    tasksById.get(dependencyId)?.status !== 'completed'
  ))) {
    throw new StrongFlowServiceError(
      'WRONG_DELIVERY_STATE',
      `DeliveryTask ${task.id} is still blocked by an incomplete DeliveryTask`,
    )
  }
  return task
}

function assertStageTaskState(
  stage: DeliveryStage,
  task: DeliveryTask | null,
  previousRun: StageRun | null,
): void {
  const taskStage = stage === 'executing' || stage === 'verifying' || stage === 'reworking'
  if (!taskStage && task !== null) {
    throw new StrongFlowServiceError(
      'INVALID_REQUEST',
      `${stage} is a Delivery-level stage and cannot target a DeliveryTask`,
    )
  }
  if (stage === 'verifying'
    && previousRun !== null
    && previousRun.deliveryTaskId !== (task?.id ?? null)) {
    throw new StrongFlowServiceError(
      'DELIVERY_CONFLICT',
      'verification must target the DeliveryTask produced by the previous stage',
    )
  }
  if (task === null) return
  const acceptedStatuses: readonly DeliveryTask['status'][] = stage === 'executing'
    ? ['pending']
    : stage === 'reworking'
      ? ['failed']
      : previousRun?.stage === 'verifying'
        ? ['verifying']
        : previousRun === null
        ? ['active', 'failed', 'verifying']
        : ['active']
  if (!acceptedStatuses.includes(task.status)) {
    throw new StrongFlowServiceError(
      'WRONG_DELIVERY_STATE',
      `DeliveryTask ${task.id} cannot enter ${stage} while ${task.status}`,
    )
  }
}

function settleStageRun(
  runs: readonly StageRun[],
  previousRun: StageRun | null,
  now: number,
): readonly StageRun[] {
  if (previousRun === null) return runs
  return Object.freeze(runs.map(run => (
    run.id === previousRun.id
      ? parseStageRun({ ...run, status: 'succeeded', finishedAtMillis: now })
      : run
  )))
}

function validateReviewAttention(
  delivery: Delivery,
  previousRun: StageRun | null,
  stageRun: StageRun,
  attention: AttentionItem | null,
  now: number,
): AttentionItem | null {
  const expectedType = REVIEW_ATTENTION_TYPES[stageRun.stage]
  if (expectedType === undefined) {
    if (attention !== null) {
      throw new StrongFlowServiceError(
        'INVALID_REQUEST',
        'only a human review stage can open business Attention through startStage',
      )
    }
    return null
  }
  if (attention === null
    || attention.deliveryId !== delivery.id
    || attention.deliverySpecId !== delivery.spec.id
    || attention.stageRunId !== stageRun.id
    || attention.type !== expectedType
    || !attention.blocking
    || attention.status !== 'open') {
    throw new StrongFlowServiceError(
      'ATTENTION_REQUIRED',
      `${stageRun.stage} requires one linked open blocking ${expectedType} AttentionItem`,
    )
  }
  if (attention.createdAtMillis > now) {
    throw new StrongFlowServiceError(
      'INVALID_REQUEST',
      'review Attention creation time is later than the service clock',
    )
  }
  if (delivery.attentionItems.some(item => item.id === attention.id)) {
    throw new StrongFlowServiceError(
      'DELIVERY_CONFLICT',
      'AttentionItem identity already exists',
    )
  }
  if (stageRun.stage === 'plan-review') {
    if (previousRun?.stage !== 'planning') {
      throw new StrongFlowServiceError(
        'WRONG_DELIVERY_STATE',
        'plan review must follow the current planning StageRun',
      )
    }
    try {
      validateStrongFlowPlanReviewAttention(
        delivery,
        previousRun,
        stageRun,
        attention,
        now,
      )
    } catch (error) {
      if (error instanceof StrongFlowPlanReviewError) {
        throw new StrongFlowServiceError(
          error.code === 'STALE_REVIEW_SET' ? 'DELIVERY_CONFLICT' : 'INVALID_REQUEST',
          error.message,
          { cause: error },
        )
      }
      throw error
    }
  }
  if (stageRun.stage === 'delivery-review'
    && delivery.spec.publicationTarget !== null) {
    try {
      validateStrongFlowGitHubPublicationAttention(delivery, stageRun, attention, now)
    } catch (error) {
      if (error instanceof StrongFlowGitHubPublicationError) {
        throw new StrongFlowServiceError(
          error.code === 'STALE_PUBLICATION_SET'
            ? 'DELIVERY_CONFLICT'
            : 'INVALID_REQUEST',
          error.message,
          { cause: error },
        )
      }
      throw error
    }
  }
  return attention
}

function attentionStageRun(delivery: Delivery, item: AttentionItem): StageRun | null {
  if (item.stageRunId === null) return null
  return currentStageRun(delivery, item.stageRunId)
}

function assertAttentionResolution(
  delivery: Delivery,
  item: AttentionItem,
  decision: Exclude<AttentionItemStatus, 'open'>,
  planReviewDecision: ValidatedStrongFlowPlanReviewDecision | null,
): {
  readonly linkedRun: StageRun | null
  readonly nextStatus: DeliveryStatus
} {
  if (item.blocking && delivery.status !== 'needs-attention') {
    throw new StrongFlowServiceError(
      'WRONG_DELIVERY_STATE',
      'blocking business Attention can be resolved only while the Delivery needs attention',
    )
  }
  const run = attentionStageRun(delivery, item)
  if (run?.stage === 'plan-review') {
    if (item.type !== 'decision_required'
      || run.actorType !== 'human'
      || run.status !== 'waiting'
    ) {
      throw new StrongFlowServiceError(
        'WRONG_DELIVERY_STATE',
        'plan review must be approved into execution or dismissed back to planning',
      )
    }
    if (planReviewDecision === null) {
      throw new StrongFlowServiceError(
        'INVALID_REQUEST',
        'plan review requires a structured decision tied to the current review set',
      )
    }
    assertStageRunBound(delivery, run)
    return Object.freeze({
      linkedRun: run,
      nextStatus: planReviewDecision.nextStatus,
    })
  }
  if (run?.stage === 'delivery-review') {
    if (item.type !== 'delivery_approval'
      || run.actorType !== 'human'
      || run.status !== 'waiting'
    ) {
      throw new StrongFlowServiceError(
        'WRONG_DELIVERY_STATE',
        'delivery review must be approved into delivery or dismissed into bounded rework',
      )
    }
    assertStageRunBound(delivery, run)
    return Object.freeze({
      linkedRun: run,
      nextStatus: decision === 'resolved' ? 'delivered' : 'reworking',
    })
  }
  if (isDerivedDeliveryVerdictAttention(item)) {
    return Object.freeze({
      linkedRun: run,
      nextStatus: deliveryVerdictAttentionNextStatus(delivery, item, decision),
    })
  }
  switch (item.type) {
    case 'requirement_question':
      return Object.freeze({
        linkedRun: run,
        nextStatus: decision === 'resolved' ? 'ready' : 'clarifying',
      })
    case 'verification_blocked':
      return Object.freeze({
        linkedRun: run,
        nextStatus: decision === 'resolved' ? 'verifying' : 'reworking',
      })
    case 'scope_change':
      return Object.freeze({ linkedRun: run, nextStatus: 'clarifying' })
    case 'decision_required':
    case 'delivery_approval':
      throw new StrongFlowServiceError(
        'WRONG_DELIVERY_STATE',
        `${item.type} Attention is not linked to an actionable current decision`,
      )
  }
}

interface ValidatedAttentionResolution {
  readonly storedResolution: string
  readonly annotatedRework: boolean
  readonly reworkTaskId: DeliveryTaskId | null
}

function validateAttentionRemediation(
  delivery: Delivery,
  item: AttentionItem,
  decision: Exclude<AttentionItemStatus, 'open'>,
  summary: string,
  remediation: StrongFlowDeliveryRemediation | null,
  diagramExecution: StrongFlowDiagramExecutionProjection | null,
): ValidatedAttentionResolution {
  const run = attentionStageRun(delivery, item)
  const annotatedRework = run?.stage === 'delivery-review' && decision === 'dismissed'
  if (!annotatedRework) {
    if (remediation !== null) {
      throw new StrongFlowServiceError(
        'INVALID_REQUEST',
        'structured remediation is accepted only when a delivery review is dismissed',
      )
    }
    return Object.freeze({
      storedResolution: summary,
      annotatedRework: false,
      reworkTaskId: null,
    })
  }
  if (remediation === null) {
    throw new StrongFlowServiceError(
      'ATTENTION_REQUIRED',
      'dismissing a delivery review requires current structured diagram annotations',
    )
  }
  const candidate = assertFrozenDeliveryCandidateCurrent(delivery, remediation.candidate)
  if (delivery.verdict?.status !== 'pass'
    || delivery.verdict.candidateRef !== candidate.candidateRef) {
    throw new StrongFlowServiceError(
      'DELIVERY_CONFLICT',
      'diagram annotations must identify the currently reviewed passing candidate',
    )
  }
  if (diagramExecution?.state !== 'execution-finished'
    || diagramExecution.details?.candidate.candidateRef !== candidate.candidateRef
    || diagramExecution.details.diffSha256 !== candidate.diffSha256) {
    throw new StrongFlowServiceError(
      'DELIVERY_CONFLICT',
      'diagram annotations require the current frozen diagram projection',
    )
  }
  const producerTaskId = diagramExecution.details.provenance.deliveryTaskId
  if (remediation.deliveryTaskId !== producerTaskId) {
    throw new StrongFlowServiceError(
      'DELIVERY_CONFLICT',
      'diagram annotations must retain the reviewed candidate writer task scope',
    )
  }
  if (remediation.deliveryTaskId === null) {
    if (delivery.tasks.length > 0) {
      throw new StrongFlowServiceError(
        'DELIVERY_CONFLICT',
        'taskless diagram remediation is accepted only for a taskless Delivery',
      )
    }
  } else {
    const task = delivery.tasks.find(entry => entry.id === remediation.deliveryTaskId)
    if (task?.status !== 'completed') {
      throw new StrongFlowServiceError(
        'WRONG_DELIVERY_STATE',
        'diagram annotations must target one completed DeliveryTask from the reviewed candidate',
      )
    }
  }
  const changedPaths = new Set(candidate.changedPaths.map(entry => entry.path))
  const evidenceById = new Map(delivery.evidence.map(entry => [entry.id, entry]))
  for (const annotation of remediation.annotations) {
    if (!changedPaths.has(annotation.filePath)) {
      throw new StrongFlowServiceError(
        'DELIVERY_CONFLICT',
        `diagram annotation ${annotation.id} references a path outside the frozen diff`,
      )
    }
    if (!diagramExecutionAnnotationExists(diagramExecution, annotation)) {
      throw new StrongFlowServiceError(
        'DELIVERY_CONFLICT',
        `diagram annotation ${annotation.id} is stale or does not match an exact diff hunk`,
      )
    }
    for (const evidenceRefId of annotation.evidenceRefIds) {
      const evidence = evidenceById.get(evidenceRefId)
      if (evidence === undefined
        || evidence.deliverySpecId !== delivery.spec.id
        || evidence.deliverySpecRevision !== delivery.spec.revision
        || evidence.candidateRef !== candidate.candidateRef) {
        throw new StrongFlowServiceError(
          'DELIVERY_CONFLICT',
          `diagram annotation ${annotation.id} cites stale or foreign evidence`,
        )
      }
    }
  }
  const storedResolution = JSON.stringify({
    schemaVersion: remediation.schemaVersion,
    protocol: STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL,
    summary,
    deliveryTaskId: remediation.deliveryTaskId,
    candidateRef: candidate.candidateRef,
    diffSha256: candidate.diffSha256,
    annotations: remediation.annotations,
  })
  return Object.freeze({
    storedResolution,
    annotatedRework: true,
    reworkTaskId: remediation.deliveryTaskId,
  })
}

function settleAttentionStageRun(
  runs: readonly StageRun[],
  item: AttentionItem,
  now: number,
): readonly StageRun[] {
  if (item.stageRunId === null) return runs
  return Object.freeze(runs.map(run => (
    run.id === item.stageRunId && (run.status === 'running' || run.status === 'waiting')
      ? parseStageRun({ ...run, status: 'succeeded', finishedAtMillis: now })
      : run
  )))
}

/**
 * The one host-owned Delivery service. It stores delivery facts and references
 * Codex/DSH sessions without taking over their execution or persistence.
 */
export class StrongFlowService {
  readonly home: string
  readonly #authenticator: StrongFlowDeliveryAuthenticator
  readonly #clock: () => number
  readonly #executionSource: StrongFlowExecutionSource | null

  constructor(options: StrongFlowServiceOptions) {
    if (!isRecord(options)
      || Object.keys(options).some(key => ![
        'home',
        'authenticator',
        'clock',
        'executionSource',
      ].includes(key))
      || typeof options.home !== 'string'
      || options.home.length === 0
      || typeof options.authenticator?.authenticate !== 'function'
      || (options.clock !== undefined && typeof options.clock !== 'function')
      || (options.executionSource !== undefined
        && typeof options.executionSource.read !== 'function')) {
      throw new StrongFlowServiceError(
        'INVALID_SERVICE_OPTIONS',
        'StrongFlow service requires a home, an authenticator, and optional clock and execution source',
      )
    }
    this.home = resolve(options.home)
    this.#authenticator = options.authenticator
    this.#clock = options.clock ?? Date.now
    this.#executionSource = options.executionSource ?? null
  }

  async createDelivery(inputValue: CreateDeliveryInput): Promise<Delivery> {
    try {
      requestRecord(inputValue, ['requestId', 'spec', 'tasks'], 'createDelivery input')
      const parsedRequestId = requestId(inputValue.requestId)
      if (containsRawCredentialMaterial(inputValue)) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'createDelivery input contains raw credential material',
        )
      }
      const spec = parseDeliverySpec(inputValue.spec)
      const tasks = inputValue.tasks.map((task, index) => (
        parseDeliveryTask(task, `createDelivery.tasks[${String(index)}]`)
      ))
      const now = nowFrom(this.#clock)
      if (spec.createdAtMillis > now) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'DeliverySpec creation time is later than the service clock',
        )
      }
      const snapshot = parseDelivery({
        schemaVersion: STRONGFLOW_SERVICE_SCHEMA_VERSION,
        id: spec.deliveryId,
        revision: 1,
        status: 'draft',
        spec,
        tasks,
        stageRuns: [],
        sessionBindings: [],
        attentionItems: [],
        evidence: [],
        verdict: null,
        createdAtMillis: now,
        updatedAtMillis: now,
      })
      const digest = requestDigest('delivery.created', { spec, tasks })
      try {
        const store = await DeliveryStore.create({
          home: this.home,
          requestId: parsedRequestId,
          requestDigest: digest,
          snapshot,
        })
        return (await store.read()).snapshot
      } catch (error) {
        if (!(error instanceof DeliveryStoreError)
          || error.code !== 'DELIVERY_ALREADY_EXISTS') throw error
        const store = await DeliveryStore.open(this.home, snapshot.id)
        const stored = await store.read()
        const first = stored.records[0]
        if (first?.requestId === parsedRequestId
          && first.requestDigest === digest
          && first.operation === 'delivery.created') return first.snapshot
        throw error
      }
    } catch (error) {
      throw serviceFault(error)
    }
  }

  async updateDeliverySpec(inputValue: UpdateDeliverySpecInput): Promise<Delivery> {
    try {
      requestRecord(
        inputValue,
        ['requestId', 'deliveryId', 'expectedRevision', 'spec'],
        'updateDeliverySpec input',
      )
      const input = Object.freeze({
        requestId: requestId(inputValue.requestId),
        deliveryId: DeliveryId(inputValue.deliveryId),
        expectedRevision: expectedRevision(inputValue.expectedRevision),
        spec: parseDeliverySpec(inputValue.spec),
      })
      if (containsRawCredentialMaterial(input)) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'updateDeliverySpec input contains raw credential material',
        )
      }
      const store = await DeliveryStore.open(this.home, input.deliveryId)
      const digest = requestDigest('delivery.spec.updated', input)
      const replay = await this.#replay(store, input.requestId, digest, 'delivery.spec.updated')
      if (replay !== undefined) return replay
      const current = await this.#current(store, input.expectedRevision)
      if (current.status !== 'draft'
        && current.status !== 'clarifying'
        && current.status !== 'ready') {
        throw new StrongFlowServiceError(
          'WRONG_DELIVERY_STATE',
          'DeliverySpec can be replaced only before planning or after explicit clarification',
        )
      }
      if (input.spec.deliveryId !== current.id
        || input.spec.id === current.spec.id
        || input.spec.revision !== current.spec.revision + 1
        || input.spec.createdAtMillis < current.spec.createdAtMillis
        || !sameExternalDeliveryBinding(input.spec.sourceRef, current.spec.sourceRef)
        || !sameExternalDeliveryBinding(
          input.spec.publicationTarget,
          current.spec.publicationTarget,
        )) {
        throw new StrongFlowServiceError(
          'DELIVERY_CONFLICT',
          'replacement DeliverySpec changes its Delivery, source issue, publication target, or revision identity',
        )
      }
      const now = nowFrom(this.#clock)
      if (input.spec.createdAtMillis > now) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'replacement DeliverySpec creation time is later than the service clock',
        )
      }
      const next = cloneWithRevision(current, now, {
        status: 'ready',
        spec: input.spec,
        tasks: Object.freeze([]),
        stageRuns: Object.freeze([]),
        sessionBindings: Object.freeze([]),
        attentionItems: Object.freeze([]),
        evidence: Object.freeze([]),
        verdict: null,
        updatedAtMillis: now,
      })
      return (await store.append({
        requestId: input.requestId,
        requestDigest: digest,
        operation: 'delivery.spec.updated',
        expectedRevision: input.expectedRevision,
        snapshot: next,
      })).snapshot
    } catch (error) {
      throw serviceFault(error)
    }
  }

  async startStage(inputValue: StartStageInput): Promise<Delivery> {
    try {
      requestRecord(inputValue, [
        'requestId',
        'deliveryId',
        'expectedRevision',
        'stageRunId',
        'deliveryTaskId',
        'stage',
        'actorType',
        'role',
        'attention',
      ], 'startStage input')
      const input = Object.freeze({
        requestId: requestId(inputValue.requestId),
        deliveryId: DeliveryId(inputValue.deliveryId),
        expectedRevision: expectedRevision(inputValue.expectedRevision),
        stageRunId: StageRunId(inputValue.stageRunId),
        deliveryTaskId: inputValue.deliveryTaskId === null
          ? null
          : DeliveryTaskId(inputValue.deliveryTaskId),
        stage: inputValue.stage,
        actorType: inputValue.actorType,
        role: inputValue.role,
        attention: inputValue.attention === null
          ? null
          : parseAttentionItem(inputValue.attention, 'startStage.attention'),
      })
      if (containsRawCredentialMaterial(input)) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'startStage input contains raw credential material',
        )
      }
      const store = await DeliveryStore.open(this.home, input.deliveryId)
      const digest = requestDigest('stage.started', input)
      const replay = await this.#replay(store, input.requestId, digest, 'stage.started')
      if (replay !== undefined) return replay
      const current = await this.#current(store, input.expectedRevision)
      const transition = stageStartTransition(current, input.stage)
      if (current.stageRuns.some(run => run.id === input.stageRunId)) {
        throw new StrongFlowServiceError('DELIVERY_CONFLICT', 'StageRun identity already exists')
      }
      if (transition.previousRun !== null) assertStageRunBound(current, transition.previousRun)
      const task = stageTask(current, input.deliveryTaskId)
      assertStageTaskState(input.stage, task, transition.previousRun)
      const reviewStage = REVIEW_ATTENTION_TYPES[input.stage] !== undefined
      if ((reviewStage && input.actorType !== 'human')
        || (!reviewStage && input.actorType !== 'codex')) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          `${input.stage} requires a ${reviewStage ? 'human' : 'codex'} StageRun actor`,
        )
      }
      if (input.stage === 'verifying'
        && !STRONGFLOW_VERIFICATION_ROLE_IDS.includes(
          input.role as typeof STRONGFLOW_VERIFICATION_ROLE_IDS[number],
        )) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'verifying requires reviewer, verifier, or adversarial-verifier role',
        )
      }
      const reworkAttemptsUsed = current.stageRuns.filter(run => (
        run.stage === 'reworking'
      )).length
      if (input.stage === 'reworking') {
        if (input.role !== 'remediator') {
          throw new StrongFlowServiceError(
            'INVALID_REQUEST',
            'reworking requires the canonical remediator role',
          )
        }
        if (reworkAttemptsUsed >= current.spec.maxReworkAttempts) {
          throw new StrongFlowServiceError(
            'WRONG_DELIVERY_STATE',
            'the approved DeliverySpec rework limit is exhausted',
          )
        }
      }
      const matchingRuns = current.stageRuns.filter(run => (
        run.stage === input.stage
        && run.deliveryTaskId === input.deliveryTaskId
        && run.role === input.role
      ))
      const now = nowFrom(this.#clock)
      const stageRun = parseStageRun({
        schemaVersion: STRONGFLOW_SERVICE_SCHEMA_VERSION,
        id: input.stageRunId,
        deliveryId: current.id,
        deliveryTaskId: input.deliveryTaskId,
        stage: input.stage,
        actorType: input.actorType,
        role: input.role,
        status: reviewStage ? 'waiting' : 'running',
        attempt: input.stage === 'reworking'
          ? reworkAttemptsUsed + 1
          : matchingRuns.length + 1,
        startedAtMillis: now,
        finishedAtMillis: null,
      })
      const attention = validateReviewAttention(
        current,
        transition.previousRun,
        stageRun,
        input.attention,
        now,
      )
      const taskStatus = input.stage === 'verifying' ? 'verifying' : 'active'
      const tasks = current.tasks.map(task => (
        task.id === input.deliveryTaskId
          ? parseDeliveryTask({ ...task, status: taskStatus })
          : task
      ))
      const settledRuns = settleStageRun(current.stageRuns, transition.previousRun, now)
      const next = cloneWithRevision(current, now, {
        status: transition.nextStatus,
        tasks: Object.freeze(tasks),
        stageRuns: Object.freeze([...settledRuns, stageRun]),
        attentionItems: attention === null
          ? current.attentionItems
          : Object.freeze([...current.attentionItems, attention]),
        updatedAtMillis: now,
      })
      return (await store.append({
        requestId: input.requestId,
        requestDigest: digest,
        operation: 'stage.started',
        expectedRevision: input.expectedRevision,
        snapshot: next,
      })).snapshot
    } catch (error) {
      throw serviceFault(error)
    }
  }

  async bindSession(inputValue: BindSessionInput): Promise<Delivery> {
    try {
      requestRecord(inputValue, [
        'requestId',
        'deliveryId',
        'expectedRevision',
        'bindingId',
        'stageRunId',
        'dshSessionId',
        'codexSessionId',
      ], 'bindSession input')
      const input = Object.freeze({
        requestId: requestId(inputValue.requestId),
        deliveryId: DeliveryId(inputValue.deliveryId),
        expectedRevision: expectedRevision(inputValue.expectedRevision),
        bindingId: SessionBindingId(inputValue.bindingId),
        stageRunId: StageRunId(inputValue.stageRunId),
        dshSessionId: inputValue.dshSessionId,
        codexSessionId: inputValue.codexSessionId,
      })
      if (containsRawCredentialMaterial(input)) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'bindSession input contains raw credential material',
        )
      }
      const store = await DeliveryStore.open(this.home, input.deliveryId)
      const digest = requestDigest('session.bound', input)
      const replay = await this.#replay(store, input.requestId, digest, 'session.bound')
      if (replay !== undefined) return replay
      const current = await this.#current(store, input.expectedRevision)
      const run = currentStageRun(current, input.stageRunId)
      if (run.status !== 'running' && run.status !== 'waiting') {
        throw new StrongFlowServiceError(
          'WRONG_DELIVERY_STATE',
          'a session can bind only to an active StageRun',
        )
      }
      const runsById = new Map(current.stageRuns.map(entry => [entry.id, entry]))
      if (current.sessionBindings.some(binding => (
        binding.id === input.bindingId
        || (input.codexSessionId !== null && binding.codexSessionId === input.codexSessionId)
        || (input.dshSessionId !== null
          && binding.dshSessionId === input.dshSessionId
          && (run.actorType === 'codex'
            || runsById.get(binding.stageRunId)?.actorType === 'codex'))
      ))) {
        throw new StrongFlowServiceError(
          'DELIVERY_CONFLICT',
          'session or SessionBinding identity is already assigned',
        )
      }
      const now = nowFrom(this.#clock)
      const binding: SessionBinding = parseSessionBinding({
        schemaVersion: STRONGFLOW_SERVICE_SCHEMA_VERSION,
        id: input.bindingId,
        deliveryId: current.id,
        stageRunId: input.stageRunId,
        dshSessionId: input.dshSessionId,
        codexSessionId: input.codexSessionId,
        boundAtMillis: now,
      })
      const next = cloneWithRevision(current, now, {
        sessionBindings: Object.freeze([...current.sessionBindings, binding]),
        updatedAtMillis: now,
      })
      return (await store.append({
        requestId: input.requestId,
        requestDigest: digest,
        operation: 'session.bound',
        expectedRevision: input.expectedRevision,
        snapshot: next,
      })).snapshot
    } catch (error) {
      throw serviceFault(error)
    }
  }

  async resolveAttention(inputValue: ResolveAttentionInput): Promise<Delivery> {
    try {
      if (!isRecord(inputValue)) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'resolveAttention input must be an object',
        )
      }
      if (!Object.hasOwn(inputValue, 'authentication')) {
        throw new StrongFlowServiceError(
          'AUTHENTICATION_REQUIRED',
          'resolving business Attention requires authentication',
        )
      }
      requestRecord(inputValue, [
        'requestId',
        'deliveryId',
        'expectedRevision',
        'attentionItemId',
        'status',
        'resolution',
        'remediation',
        'channel',
        'authentication',
      ], 'resolveAttention input')
      const authentication = authenticationRequest(
        inputValue.authentication,
        inputValue.channel,
      )
      const authenticated = await this.#authenticator.authenticate(authentication)
      if (authenticated === undefined) {
        throw new StrongFlowServiceError(
          'AUTHENTICATION_FAILED',
          'business Attention authentication failed',
        )
      }
      const input = Object.freeze({
        requestId: requestId(inputValue.requestId),
        deliveryId: DeliveryId(inputValue.deliveryId),
        expectedRevision: expectedRevision(inputValue.expectedRevision),
        attentionItemId: AttentionItemId(inputValue.attentionItemId),
        status: inputValue.status,
        resolution: inputValue.resolution,
        remediation: inputValue.remediation === null
          ? null
          : parseStrongFlowDeliveryRemediation(
            inputValue.remediation,
            'resolveAttention.remediation',
          ),
        resolvedBy: authenticatedActorId(authenticated.actorId),
      })
      if (containsRawCredentialMaterial(input)) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'resolveAttention input contains raw credential material',
        )
      }
      const store = await DeliveryStore.open(this.home, input.deliveryId)
      const digest = requestDigest('attention.resolved', input)
      const replay = await this.#replay(store, input.requestId, digest, 'attention.resolved')
      if (replay !== undefined) return replay
      const current = await this.#current(store, input.expectedRevision)
      const item = current.attentionItems.find(entry => entry.id === input.attentionItemId)
      if (item === undefined || item.status !== 'open') {
        throw new StrongFlowServiceError(
          'DELIVERY_CONFLICT',
          'AttentionItem is missing or already settled',
        )
      }
      const linkedRun = attentionStageRun(current, item)
      let planReviewDecision: ValidatedStrongFlowPlanReviewDecision | null = null
      let githubPublicationDecision: ValidatedStrongFlowGitHubPublicationDecision | null = null
      if (linkedRun?.stage === 'plan-review') {
        if (input.remediation !== null) {
          throw new StrongFlowServiceError(
            'INVALID_REQUEST',
            'plan review does not accept delivery-candidate remediation annotations',
          )
        }
        try {
          planReviewDecision = validateStrongFlowPlanReviewDecision(
            current,
            item,
            input.status,
            input.resolution,
          )
        } catch (error) {
          if (error instanceof StrongFlowPlanReviewError) {
            throw new StrongFlowServiceError(
              error.code === 'STALE_REVIEW_SET'
                ? 'DELIVERY_CONFLICT'
                : 'INVALID_REQUEST',
              error.message,
              { cause: error },
            )
          }
          throw error
        }
      }
      if (linkedRun?.stage === 'delivery-review'
        && current.spec.publicationTarget !== null
        && input.status === 'resolved') {
        if (input.remediation !== null) {
          throw new StrongFlowServiceError(
            'INVALID_REQUEST',
            'GitHub publication approval does not accept remediation annotations',
          )
        }
        try {
          githubPublicationDecision = validateStrongFlowGitHubPublicationDecision(
            current,
            item,
            input.status,
            input.resolution,
          )
        } catch (error) {
          if (error instanceof StrongFlowGitHubPublicationError) {
            throw new StrongFlowServiceError(
              error.code === 'STALE_PUBLICATION_SET'
                ? 'DELIVERY_CONFLICT'
                : 'INVALID_REQUEST',
              error.message,
              { cause: error },
            )
          }
          throw error
        }
      }
      const transition = assertAttentionResolution(
        current,
        item,
        input.status,
        planReviewDecision,
      )
      const requiresDiagramProjection = linkedRun?.stage === 'delivery-review'
        && input.status === 'dismissed'
        && input.remediation !== null
      const diagramExecution = requiresDiagramProjection
        && this.#executionSource !== null
        ? projectStrongFlowDiagramExecution(
          current,
          await this.#executionSource.read(current),
        )
        : null
      const validatedResolution = planReviewDecision !== null
        ? Object.freeze({
          storedResolution: planReviewDecision.storedResolution,
          annotatedRework: false,
          reworkTaskId: null,
        })
        : githubPublicationDecision !== null
          ? Object.freeze({
            storedResolution: githubPublicationDecision.storedResolution,
            annotatedRework: false,
            reworkTaskId: null,
          })
          : validateAttentionRemediation(
            current,
            item,
            input.status,
            input.resolution,
            input.remediation,
            diagramExecution,
          )
      const now = nowFrom(this.#clock)
      const resolved = parseAttentionItem({
        ...item,
        status: input.status,
        resolution: validatedResolution.storedResolution,
        resolvedBy: input.resolvedBy,
        resolvedAtMillis: now,
      })
      const attentionItems = current.attentionItems.map(entry => (
        entry.id === item.id ? resolved : entry
      ))
      const nextStatus = attentionItems.some(entry => entry.blocking && entry.status === 'open')
        ? 'needs-attention'
        : transition.nextStatus
      const startsAnnotatedRework = transition.linkedRun?.stage === 'delivery-review'
        && input.status === 'dismissed'
        && nextStatus === 'reworking'
        && validatedResolution.annotatedRework
      const resumesVerification = nextStatus === 'verifying'
        && transition.linkedRun?.deliveryTaskId != null
      const startsRework = nextStatus === 'reworking'
        && transition.linkedRun?.deliveryTaskId != null
      const next = cloneWithRevision(current, now, {
        status: nextStatus,
        stageRuns: settleAttentionStageRun(current.stageRuns, item, now),
        attentionItems: Object.freeze(attentionItems),
        tasks: startsAnnotatedRework && validatedResolution.reworkTaskId !== null
          ? Object.freeze(current.tasks.map(task => (
            task.id === validatedResolution.reworkTaskId
              ? parseDeliveryTask({ ...task, status: 'failed' })
              : task
          )))
          : resumesVerification
            ? Object.freeze(current.tasks.map(task => (
              task.id === transition.linkedRun!.deliveryTaskId
                ? parseDeliveryTask({ ...task, status: 'verifying' })
                : task
            )))
            : startsRework
              ? Object.freeze(current.tasks.map(task => (
                task.id === transition.linkedRun!.deliveryTaskId
                  ? parseDeliveryTask({ ...task, status: 'failed' })
                  : task
              )))
              : current.tasks,
        verdict: startsAnnotatedRework ? null : current.verdict,
        updatedAtMillis: now,
      })
      return (await store.append({
        requestId: input.requestId,
        requestDigest: digest,
        operation: 'attention.resolved',
        expectedRevision: input.expectedRevision,
        snapshot: next,
      })).snapshot
    } catch (error) {
      throw serviceFault(error)
    }
  }

  async submitVerdict(inputValue: SubmitVerdictInput): Promise<Delivery> {
    try {
      requestRecord(inputValue, [
        'requestId',
        'deliveryId',
        'expectedRevision',
        'candidate',
        'runtimeEvents',
        'requiredRoles',
      ], 'submitVerdict input')
      if (!Array.isArray(inputValue.runtimeEvents)
        || inputValue.runtimeEvents.length > 65_536
        || inputValue.runtimeEvents.some(event => !isRecord(event))) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'submitVerdict runtimeEvents must be a bounded object array',
        )
      }
      let runtimeEventJson: string
      try {
        runtimeEventJson = JSON.stringify(inputValue.runtimeEvents)
      } catch (error) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'submitVerdict runtimeEvents must be JSON serializable',
          { cause: error },
        )
      }
      if (runtimeEventJson.length > MAX_VERDICT_RUNTIME_EVENT_JSON_LENGTH) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'submitVerdict runtimeEvents exceed the request size limit',
        )
      }
      if (!Array.isArray(inputValue.requiredRoles)
        || inputValue.requiredRoles.length < 2
        || inputValue.requiredRoles.length > STRONGFLOW_VERIFICATION_ROLE_IDS.length
        || inputValue.requiredRoles.some(role => (
          !STRONGFLOW_VERIFICATION_ROLE_IDS.includes(role)
        ))
        || new Set(inputValue.requiredRoles).size !== inputValue.requiredRoles.length
        || !inputValue.requiredRoles.includes('reviewer')
        || !inputValue.requiredRoles.includes('verifier')) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'submitVerdict requiredRoles must contain reviewer and verifier exactly once',
        )
      }
      const input = Object.freeze({
        requestId: requestId(inputValue.requestId),
        deliveryId: DeliveryId(inputValue.deliveryId),
        expectedRevision: expectedRevision(inputValue.expectedRevision),
        candidate: parseFrozenDeliveryCandidate(inputValue.candidate, 'submitVerdict.candidate'),
        runtimeEvents: immutable(inputValue.runtimeEvents),
        requiredRoles: Object.freeze(
          STRONGFLOW_VERIFICATION_ROLE_IDS.filter(role => inputValue.requiredRoles.includes(role)),
        ),
      })
      if (containsRawCredentialMaterial(input)) {
        throw new StrongFlowServiceError(
          'INVALID_REQUEST',
          'submitVerdict input contains raw credential material',
        )
      }
      const store = await DeliveryStore.open(this.home, input.deliveryId)
      const digest = requestDigest('verdict.submitted', input)
      const replay = await this.#replay(store, input.requestId, digest, 'verdict.submitted')
      if (replay !== undefined) return replay
      const current = await this.#current(store, input.expectedRevision)
      if (current.status !== 'verifying') {
        throw new StrongFlowServiceError(
          'WRONG_DELIVERY_STATE',
          'DeliveryVerdict can be submitted only while verifying',
        )
      }
      const verifying = activeStageRuns(current).filter(run => run.stage === 'verifying')
      if (verifying.length !== 1) {
        throw new StrongFlowServiceError(
          'WRONG_DELIVERY_STATE',
          'DeliveryVerdict requires exactly one active verification StageRun',
        )
      }
      assertStageRunBound(current, verifying[0]!)
      const now = nowFrom(this.#clock)
      const computed = computeDeliveryVerdict({
        delivery: current,
        acceptance: freezeAcceptanceVerificationInput(current),
        candidate: input.candidate,
        runtimeEvents: input.runtimeEvents,
        requiredRoles: input.requiredRoles,
        producedAtMillis: now,
      })
      const evidenceIds = new Set(current.evidence.map(entry => entry.id))
      if (computed.evidence.some(entry => evidenceIds.has(entry.id))) {
        throw new StrongFlowServiceError('DELIVERY_CONFLICT', 'EvidenceRef identity already exists')
      }
      const evaluated = parseDelivery({
        ...current,
        evidence: [...current.evidence, ...computed.evidence],
        verdict: computed.verdict,
        updatedAtMillis: now,
      })
      const classification = deriveDeliveryVerdictAttention({
        delivery: evaluated,
        verificationStageRunId: verifying[0]!.id,
        createdAtMillis: now,
      })
      const attentionIds = new Set(current.attentionItems.map(item => item.id))
      if (classification.attentionItems.some(item => attentionIds.has(item.id))) {
        throw new StrongFlowServiceError(
          'DELIVERY_CONFLICT',
          'classified AttentionItem identity already exists',
        )
      }
      const hasBlockingAttention = current.attentionItems.some(item => (
        item.blocking && item.status === 'open'
      )) || classification.attentionItems.length > 0
      if (computed.verdict.status !== 'pass' && !hasBlockingAttention) {
        throw new StrongFlowServiceError(
          'ATTENTION_REQUIRED',
          'non-passing verdict has no actionable business Attention',
        )
      }
      const status: DeliveryStatus = computed.verdict.status === 'pass'
        ? 'ready-to-deliver'
        : 'needs-attention'
      const stageRuns = current.stageRuns.map(run => (
        run.id === verifying[0]!.id
          ? parseStageRun({
            ...run,
            status: computed.verdict.status === 'infra_error' ? 'failed' : 'succeeded',
            finishedAtMillis: now,
          })
          : run
      ))
      const tasks = current.tasks.map(task => (
        computed.verdict.status === 'pass'
          ? parseDeliveryTask({ ...task, status: 'completed' })
          : task.status === 'verifying'
          ? parseDeliveryTask({
            ...task,
            status: computed.verdict.status === 'fail'
                ? 'failed'
                : 'verifying',
          })
          : task
      ))
      const next = cloneWithRevision(current, now, {
        status,
        tasks: Object.freeze(tasks),
        stageRuns: Object.freeze(stageRuns),
        attentionItems: Object.freeze([
          ...current.attentionItems,
          ...classification.attentionItems,
        ]),
        evidence: Object.freeze([...current.evidence, ...computed.evidence]),
        verdict: computed.verdict,
        updatedAtMillis: now,
      })
      return (await store.append({
        requestId: input.requestId,
        requestDigest: digest,
        operation: 'verdict.submitted',
        expectedRevision: input.expectedRevision,
        snapshot: next,
      })).snapshot
    } catch (error) {
      throw serviceFault(error)
    }
  }

  async getDeliveryProjection(
    deliveryId: DeliveryIdentifier | string,
  ): Promise<StrongFlowDeliveryProjection> {
    try {
      const parsedId = DeliveryId(deliveryId)
      const delivery = (await DeliveryStore.open(this.home, parsedId).then(store => store.read())).snapshot
      const facts = this.#executionSource === null
        ? null
        : await this.#executionSource.read(delivery)
      const diagramExecution = facts === null
        ? null
        : projectStrongFlowDiagramExecution(delivery, facts)
      const runtimeExecution = facts === null
        ? null
        : projectStrongFlowRuntimeExecution(delivery, facts.runtimeEvents)
      return Object.freeze({ delivery, diagramExecution, runtimeExecution })
    } catch (error) {
      throw serviceFault(error)
    }
  }

  async #current(store: DeliveryStore, revision: number): Promise<Delivery> {
    const current = (await store.read()).snapshot
    if (current.revision !== revision) {
      throw new StrongFlowServiceError(
        'REVISION_CONFLICT',
        `Delivery ${current.id} is at revision ${String(current.revision)}`,
        { currentRevision: current.revision },
      )
    }
    return current
  }

  async #replay(
    store: DeliveryStore,
    id: string,
    digest: string,
    operation: DeliveryMutationOperation,
  ): Promise<Delivery | undefined> {
    const stored: StoredDelivery = await store.read()
    const record = stored.records.find(entry => entry.requestId === id)
    if (record === undefined) return undefined
    if (record.requestDigest !== digest || record.operation !== operation) {
      throw new StrongFlowServiceError(
        'DELIVERY_CONFLICT',
        `request ${id} was already used for another Delivery mutation`,
      )
    }
    return record.snapshot
  }
}
