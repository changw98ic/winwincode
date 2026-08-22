/** Durable StrongFlow job language. Kernel events and UI projections stay separate. */

import {
  parseStrongFlowArtifactAs,
  type HumanReviewRecord,
} from './strongflow-artifact.js'

export const STRONGFLOW_JOB_EVENT_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_JOB_SNAPSHOT_SCHEMA_VERSION = 1 as const

declare const strongFlowIdentifierBrand: unique symbol

type StrongFlowIdentifier<Name extends string> = string & {
  readonly [strongFlowIdentifierBrand]: Name
}

export type JobId = StrongFlowIdentifier<'JobId'>
export type AttemptId = StrongFlowIdentifier<'AttemptId'>
export type StageRunId = StrongFlowIdentifier<'StageRunId'>
export type CandidateId = StrongFlowIdentifier<'CandidateId'>
export type RequirementId = StrongFlowIdentifier<'RequirementId'>
export type SolutionId = StrongFlowIdentifier<'SolutionId'>
export type DiagramId = StrongFlowIdentifier<'DiagramId'>
export type HumanReviewId = StrongFlowIdentifier<'HumanReviewId'>
export type KernelSessionId = StrongFlowIdentifier<'KernelSessionId'>

const IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/-]{0,199}$/u
const SEQUENCE_PATTERN = /^(?:0|[1-9][0-9]*)$/u

function identifier<Name extends string>(value: string, name: Name): StrongFlowIdentifier<Name> {
  if (!IDENTIFIER_PATTERN.test(value)) {
    throw new StrongFlowTransitionError(
      'INVALID_EVENT',
      `${name} must be a non-empty portable identifier of at most 200 characters`,
    )
  }
  return value as StrongFlowIdentifier<Name>
}

export function JobId(value: string): JobId {
  return identifier(value, 'JobId')
}

export function AttemptId(value: string): AttemptId {
  return identifier(value, 'AttemptId')
}

export function StageRunId(value: string): StageRunId {
  return identifier(value, 'StageRunId')
}

export function CandidateId(value: string): CandidateId {
  return identifier(value, 'CandidateId')
}

export function RequirementId(value: string): RequirementId {
  return identifier(value, 'RequirementId')
}

export function SolutionId(value: string): SolutionId {
  return identifier(value, 'SolutionId')
}

export function DiagramId(value: string): DiagramId {
  return identifier(value, 'DiagramId')
}

export function HumanReviewId(value: string): HumanReviewId {
  return identifier(value, 'HumanReviewId')
}

export function KernelSessionId(value: string): KernelSessionId {
  return identifier(value, 'KernelSessionId')
}

export type LosslessJsonValue =
  | null
  | boolean
  | number
  | string
  | readonly LosslessJsonValue[]
  | { readonly [key: string]: LosslessJsonValue }

export const STRONGFLOW_JOB_STATES = [
  'DEFINING_REQUIREMENTS',
  'DEFINING_SOLUTION',
  'DEFINING_DIAGRAMS',
  'AWAITING_HUMAN_REVIEW',
  'PLANNING',
  'EXECUTING',
  'VERIFYING',
  'REMEDIATING',
  'AWAITING_COMPLETION_GATE',
  'DELIVERING',
  'READY_TO_DELIVER',
  'INTERRUPTED',
  'FAILED',
  'REJECTED',
  'CANCELLED',
  'DELIVERED',
] as const

export type StrongFlowJobState = typeof STRONGFLOW_JOB_STATES[number]

export const STRONGFLOW_JOB_STAGES = [
  'REQUIREMENTS',
  'SOLUTION',
  'DIAGRAMS',
  'PLANNING',
  'EXECUTION',
  'VERIFICATION',
  'REMEDIATION',
  'DELIVERY',
] as const

export type StrongFlowJobStage = typeof STRONGFLOW_JOB_STAGES[number]

export type DefinitionRevisionScope = 'requirements' | 'solution' | 'diagrams'
export type StageFailureCategory = 'task' | 'infrastructure'
export type VerificationOutcome = 'passed' | 'remediation-required'
export type CompletionGateOutcome = 'passed' | 'failed'

export interface DefinitionIdentity {
  readonly requirementId: RequirementId
  readonly solutionId: SolutionId
  readonly systemArchitectureDiagramId: DiagramId
  readonly processFlowDiagramId: DiagramId
}

export interface JobDefinitionSnapshot {
  readonly requirementId?: RequirementId
  readonly solutionId?: SolutionId
  readonly systemArchitectureDiagramId?: DiagramId
  readonly processFlowDiagramId?: DiagramId
}

export type HumanReviewChannel = 'local-ui' | 'cli'

export type StrongFlowJobEventSource =
  | {
    readonly kind: 'system'
    readonly actorId: string
  }
  | {
    readonly kind: 'human'
    readonly actorId: string
    readonly channel: HumanReviewChannel
  }
  | {
    readonly kind: 'role'
    readonly actorId: string
    readonly kernelSessionId?: KernelSessionId
  }

export interface JobCreatedData {
  readonly title?: string
}

export interface StageStartedData {
  readonly stage: StrongFlowJobStage
  readonly stageRunId: StageRunId
  readonly attemptId: AttemptId
}

interface StageSucceededBase {
  readonly stage: StrongFlowJobStage
  readonly stageRunId: StageRunId
  readonly attemptId: AttemptId
}

export type StageSucceededData =
  | StageSucceededBase & {
    readonly stage: 'REQUIREMENTS'
    readonly requirementId: RequirementId
  }
  | StageSucceededBase & {
    readonly stage: 'SOLUTION'
    readonly requirementId: RequirementId
    readonly solutionId: SolutionId
  }
  | StageSucceededBase & {
    readonly stage: 'DIAGRAMS'
    readonly definition: DefinitionIdentity
  }
  | StageSucceededBase & {
    readonly stage: 'PLANNING'
  }
  | StageSucceededBase & {
    readonly stage: 'EXECUTION'
    readonly candidateId: CandidateId
  }
  | StageSucceededBase & {
    readonly stage: 'VERIFICATION'
    readonly candidateId: CandidateId
    readonly outcome: VerificationOutcome
  }
  | StageSucceededBase & {
    readonly stage: 'REMEDIATION'
    readonly candidateId: CandidateId
  }
  | StageSucceededBase & {
    readonly stage: 'DELIVERY'
    readonly candidateId: CandidateId
  }

export interface StageFailedData {
  readonly stage: StrongFlowJobStage
  readonly stageRunId: StageRunId
  readonly attemptId: AttemptId
  readonly category: StageFailureCategory
  readonly code: string
  readonly message: string
  readonly retryable: boolean
}

interface HumanReviewDataBase {
  readonly reviewId: HumanReviewId
  readonly reviewerId: string
  readonly definition: DefinitionIdentity
  readonly comment?: string
}

export interface HumanReviewApprovedData extends HumanReviewDataBase {}

export interface HumanReviewChangesRequestedData extends HumanReviewDataBase {
  readonly scope: DefinitionRevisionScope
}

export interface HumanReviewRejectedData extends HumanReviewDataBase {}

export interface JobInterruptedData {
  readonly reason: string
  readonly stageRunId?: StageRunId
}

export interface JobResumedData {
  readonly interruptionSequence: string
}

export interface JobCancelledData {
  readonly reason: string
}

export interface CompletionGatePassedData {
  readonly stageRunId: StageRunId
  readonly candidateId: CandidateId
}

export interface CompletionGateFailedData {
  readonly stageRunId: StageRunId
  readonly candidateId: CandidateId
  readonly reason: string
}

export interface JobDeliveredData {
  readonly candidateId: CandidateId
}

export interface StrongFlowJobEventDataByKind {
  readonly 'job.created': JobCreatedData
  readonly 'stage.started': StageStartedData
  readonly 'stage.succeeded': StageSucceededData
  readonly 'stage.failed': StageFailedData
  readonly 'human-review.approved': HumanReviewApprovedData
  readonly 'human-review.changes-requested': HumanReviewChangesRequestedData
  readonly 'human-review.rejected': HumanReviewRejectedData
  readonly 'job.interrupted': JobInterruptedData
  readonly 'job.resumed': JobResumedData
  readonly 'job.cancelled': JobCancelledData
  readonly 'completion-gate.passed': CompletionGatePassedData
  readonly 'completion-gate.failed': CompletionGateFailedData
  readonly 'job.delivered': JobDeliveredData
}

export type StrongFlowJobEventKind = keyof StrongFlowJobEventDataByKind

interface StrongFlowJobEventBase {
  readonly schemaVersion: typeof STRONGFLOW_JOB_EVENT_SCHEMA_VERSION
  readonly id: string
  readonly jobId: JobId
  readonly sequence: string
  readonly occurredAtMillis: number
  readonly source: StrongFlowJobEventSource
}

type StrongFlowJobEventFor<Kind extends StrongFlowJobEventKind> = StrongFlowJobEventBase & {
  readonly kind: Kind
  readonly data: StrongFlowJobEventDataByKind[Kind]
}

export type StrongFlowJobEvent = {
  readonly [Kind in StrongFlowJobEventKind]: StrongFlowJobEventFor<Kind>
}[StrongFlowJobEventKind]

export type CreateStrongFlowJobEventInput<Kind extends StrongFlowJobEventKind> = Omit<
  StrongFlowJobEventFor<Kind>,
  'schemaVersion' | 'id'
>

export interface ActiveStageRun {
  readonly stage: StrongFlowJobStage
  readonly stageRunId: StageRunId
  readonly attemptId: AttemptId
  readonly actorId: string
  readonly kernelSessionId?: KernelSessionId
  readonly startedAtMillis: number
}

export type JobStopKind =
  | 'task-failure'
  | 'infrastructure-failure'
  | 'human-rejection'
  | 'cancellation'
  | 'interruption'

export interface JobStopRecord {
  readonly kind: JobStopKind
  readonly occurredAtMillis: number
  readonly message: string
  readonly code?: string
  readonly retryable?: boolean
  readonly stage?: StrongFlowJobStage
  readonly stageRunId?: StageRunId
}

export interface JobInterruptionRecord {
  readonly sequence: string
  readonly resumeState: Exclude<StrongFlowJobState, 'INTERRUPTED'>
  readonly reason: string
  readonly stageRunId?: StageRunId
}

export interface CompletionGateRecord {
  readonly stageRunId: StageRunId
  readonly candidateId: CandidateId
  readonly outcome: CompletionGateOutcome
  readonly occurredAtMillis: number
  readonly reason?: string
}

export interface StrongFlowJobSnapshot {
  readonly schemaVersion: typeof STRONGFLOW_JOB_SNAPSHOT_SCHEMA_VERSION
  readonly jobId: JobId
  readonly title?: string
  readonly sequence: string
  readonly lastOccurredAtMillis: number
  readonly state: StrongFlowJobState
  readonly definitionRevision: number
  readonly definition: JobDefinitionSnapshot
  readonly approval?: HumanReviewRecord
  readonly lastHumanReview?: HumanReviewRecord
  readonly activeStage?: ActiveStageRun
  readonly candidateId?: CandidateId
  readonly completionGate?: CompletionGateRecord
  readonly interruption?: JobInterruptionRecord
  readonly lastStop?: JobStopRecord
  readonly deliveredAtMillis?: number
}

export type StrongFlowTransitionErrorCode =
  | 'INVALID_EVENT'
  | 'INVALID_SEQUENCE'
  | 'WRONG_JOB'
  | 'ILLEGAL_TRANSITION'
  | 'TERMINAL_JOB'
  | 'ACTIVE_STAGE_EXISTS'
  | 'STAGE_RUN_MISMATCH'
  | 'STALE_DEFINITION'
  | 'APPROVAL_REQUIRED'
  | 'CANDIDATE_MISMATCH'

export class StrongFlowTransitionError extends Error {
  readonly code: StrongFlowTransitionErrorCode

  constructor(code: StrongFlowTransitionErrorCode, message: string) {
    super(message)
    this.name = 'StrongFlowTransitionError'
    this.code = code
  }
}

const CONTROL_EVENTS = ['job.interrupted', 'job.cancelled'] as const
const STAGE_EVENTS = ['stage.started', 'stage.succeeded', 'stage.failed'] as const

function transitionEvents(
  ...events: readonly StrongFlowJobEventKind[]
): readonly StrongFlowJobEventKind[] {
  return Object.freeze(events)
}

export const STRONGFLOW_JOB_TRANSITIONS: Readonly<
  Record<StrongFlowJobState, readonly StrongFlowJobEventKind[]>
> = Object.freeze({
  DEFINING_REQUIREMENTS: transitionEvents(...STAGE_EVENTS, ...CONTROL_EVENTS),
  DEFINING_SOLUTION: transitionEvents(...STAGE_EVENTS, ...CONTROL_EVENTS),
  DEFINING_DIAGRAMS: transitionEvents(...STAGE_EVENTS, ...CONTROL_EVENTS),
  AWAITING_HUMAN_REVIEW: transitionEvents(
    'human-review.approved',
    'human-review.changes-requested',
    'human-review.rejected',
    ...CONTROL_EVENTS,
  ),
  PLANNING: transitionEvents(
    ...STAGE_EVENTS,
    'human-review.changes-requested',
    ...CONTROL_EVENTS,
  ),
  EXECUTING: transitionEvents(
    ...STAGE_EVENTS,
    'human-review.changes-requested',
    ...CONTROL_EVENTS,
  ),
  VERIFYING: transitionEvents(
    ...STAGE_EVENTS,
    'human-review.changes-requested',
    ...CONTROL_EVENTS,
  ),
  REMEDIATING: transitionEvents(
    ...STAGE_EVENTS,
    'human-review.changes-requested',
    ...CONTROL_EVENTS,
  ),
  AWAITING_COMPLETION_GATE: transitionEvents(
    'completion-gate.passed',
    'completion-gate.failed',
    'human-review.changes-requested',
    ...CONTROL_EVENTS,
  ),
  DELIVERING: transitionEvents(
    ...STAGE_EVENTS,
    'human-review.changes-requested',
    ...CONTROL_EVENTS,
  ),
  READY_TO_DELIVER: transitionEvents(
    'job.delivered',
    'human-review.changes-requested',
    ...CONTROL_EVENTS,
  ),
  INTERRUPTED: transitionEvents('job.resumed', 'job.cancelled'),
  FAILED: transitionEvents(),
  REJECTED: transitionEvents(),
  CANCELLED: transitionEvents(),
  DELIVERED: transitionEvents(),
})

export const STRONGFLOW_STAGE_BY_STATE: Readonly<
  Partial<Record<StrongFlowJobState, StrongFlowJobStage>>
> =
  Object.freeze({
    DEFINING_REQUIREMENTS: 'REQUIREMENTS',
    DEFINING_SOLUTION: 'SOLUTION',
    DEFINING_DIAGRAMS: 'DIAGRAMS',
    PLANNING: 'PLANNING',
    EXECUTING: 'EXECUTION',
    VERIFYING: 'VERIFICATION',
    REMEDIATING: 'REMEDIATION',
    DELIVERING: 'DELIVERY',
  })

const TERMINAL_STATES: ReadonlySet<StrongFlowJobState> = new Set([
  'FAILED',
  'REJECTED',
  'CANCELLED',
  'DELIVERED',
])

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function transitionError(code: StrongFlowTransitionErrorCode, message: string): never {
  throw new StrongFlowTransitionError(code, message)
}

function assertPortableIdentifier(value: unknown, label: string): asserts value is string {
  if (typeof value !== 'string' || !IDENTIFIER_PATTERN.test(value)) {
    transitionError('INVALID_EVENT', `${label} is not a portable identifier`)
  }
}

function assertActorId(value: unknown, label: string): asserts value is string {
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.length > 200
    || value.trim() !== value
    || /[\u0000-\u001f\u007f]/u.test(value)
  ) {
    transitionError('INVALID_EVENT', `${label} is not a valid actor identity`)
  }
}

function assertNonEmptyText(value: unknown, label: string): asserts value is string {
  if (typeof value !== 'string' || value.trim().length === 0) {
    transitionError('INVALID_EVENT', `${label} must be a non-empty string`)
  }
}

function assertOptionalText(value: unknown, label: string): void {
  if (value !== undefined && typeof value !== 'string') {
    transitionError('INVALID_EVENT', `${label} must be a string when present`)
  }
}

function assertExactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
  label: string,
): void {
  const allowed = new Set([...required, ...optional])
  for (const key of required) {
    if (!Object.hasOwn(value, key)) transitionError('INVALID_EVENT', `${label}.${key} is required`)
  }
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) transitionError('INVALID_EVENT', `${label}.${key} is not allowed`)
  }
}

function assertLosslessJson(value: unknown, path: string, seen: Set<object>): void {
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return
  if (typeof value === 'number') {
    if (!Number.isFinite(value) || Object.is(value, -0)) {
      transitionError('INVALID_EVENT', `${path} is not lossless JSON`)
    }
    return
  }
  if (typeof value !== 'object') transitionError('INVALID_EVENT', `${path} is not JSON`)
  if (seen.has(value)) transitionError('INVALID_EVENT', `${path} contains a cycle`)
  seen.add(value)
  try {
    if (Array.isArray(value)) {
      const keys = Object.keys(value)
      if (keys.length !== value.length || keys.some((key, index) => key !== String(index))) {
        transitionError('INVALID_EVENT', `${path} is a sparse or decorated array`)
      }
      value.forEach((entry, index) => assertLosslessJson(entry, `${path}[${index}]`, seen))
      return
    }
    const prototype = Object.getPrototypeOf(value)
    if (prototype !== Object.prototype && prototype !== null) {
      transitionError('INVALID_EVENT', `${path} is not a plain JSON object`)
    }
    if (Object.getOwnPropertySymbols(value).length > 0) {
      transitionError('INVALID_EVENT', `${path} contains symbol keys`)
    }
    for (const [key, entry] of Object.entries(value)) {
      assertLosslessJson(entry, `${path}.${key}`, seen)
    }
  } finally {
    seen.delete(value)
  }
}

function deepFreeze(value: unknown): unknown {
  if (Array.isArray(value)) {
    for (const entry of value) deepFreeze(entry)
    return Object.freeze(value)
  }
  if (isRecord(value)) {
    for (const entry of Object.values(value)) deepFreeze(entry)
    return Object.freeze(value)
  }
  return value
}

function immutableClone<Value>(value: Value): Value {
  assertLosslessJson(value, 'value', new Set())
  const clone: unknown = JSON.parse(JSON.stringify(value))
  return deepFreeze(clone) as Value
}

function assertSource(value: unknown): asserts value is StrongFlowJobEventSource {
  if (!isRecord(value)) transitionError('INVALID_EVENT', 'event.source must be an object')
  if (value.kind === 'system') {
    assertExactKeys(value, ['kind', 'actorId'], [], 'event.source')
  } else if (value.kind === 'human') {
    assertExactKeys(value, ['kind', 'actorId', 'channel'], [], 'event.source')
    if (!['local-ui', 'cli'].includes(String(value.channel))) {
      transitionError('INVALID_EVENT', 'event.source.channel is invalid')
    }
  } else if (value.kind === 'role') {
    assertExactKeys(value, ['kind', 'actorId'], ['kernelSessionId'], 'event.source')
    if (value.kernelSessionId !== undefined) {
      assertPortableIdentifier(value.kernelSessionId, 'event.source.kernelSessionId')
    }
  } else {
    transitionError('INVALID_EVENT', 'event.source.kind is invalid')
  }
  assertActorId(value.actorId, 'event.source.actorId')
}

function assertDefinitionIdentity(value: unknown, label: string): asserts value is DefinitionIdentity {
  if (!isRecord(value)) transitionError('INVALID_EVENT', `${label} must be an object`)
  assertExactKeys(value, [
    'requirementId',
    'solutionId',
    'systemArchitectureDiagramId',
    'processFlowDiagramId',
  ], [], label)
  assertPortableIdentifier(value.requirementId, `${label}.requirementId`)
  assertPortableIdentifier(value.solutionId, `${label}.solutionId`)
  assertPortableIdentifier(
    value.systemArchitectureDiagramId,
    `${label}.systemArchitectureDiagramId`,
  )
  assertPortableIdentifier(value.processFlowDiagramId, `${label}.processFlowDiagramId`)
}

function assertStageRunFields(value: Record<string, unknown>, label: string): void {
  if (!STRONGFLOW_JOB_STAGES.includes(value.stage as StrongFlowJobStage)) {
    transitionError('INVALID_EVENT', `${label}.stage is invalid`)
  }
  assertPortableIdentifier(value.stageRunId, `${label}.stageRunId`)
  assertPortableIdentifier(value.attemptId, `${label}.attemptId`)
}

function assertReviewData(
  value: Record<string, unknown>,
  label: string,
  includeScope: boolean,
): void {
  assertExactKeys(
    value,
    [
      'reviewId',
      'reviewerId',
      'definition',
      ...(includeScope ? ['scope'] : []),
    ],
    ['comment'],
    label,
  )
  assertPortableIdentifier(value.reviewId, `${label}.reviewId`)
  assertActorId(value.reviewerId, `${label}.reviewerId`)
  assertDefinitionIdentity(value.definition, `${label}.definition`)
  assertOptionalText(value.comment, `${label}.comment`)
  if (includeScope && !['requirements', 'solution', 'diagrams'].includes(String(value.scope))) {
    transitionError('INVALID_EVENT', `${label}.scope is invalid`)
  }
}

function assertEventData(kind: StrongFlowJobEventKind, value: unknown): void {
  if (!isRecord(value)) transitionError('INVALID_EVENT', `event.data for ${kind} must be an object`)
  switch (kind) {
    case 'job.created':
      assertExactKeys(value, [], ['title'], 'event.data')
      assertOptionalText(value.title, 'event.data.title')
      return
    case 'stage.started':
      assertExactKeys(value, ['stage', 'stageRunId', 'attemptId'], [], 'event.data')
      assertStageRunFields(value, 'event.data')
      return
    case 'stage.succeeded': {
      const stage = value.stage
      const common = ['stage', 'stageRunId', 'attemptId']
      if (stage === 'REQUIREMENTS') {
        assertExactKeys(value, [...common, 'requirementId'], [], 'event.data')
        assertPortableIdentifier(value.requirementId, 'event.data.requirementId')
      } else if (stage === 'SOLUTION') {
        assertExactKeys(value, [...common, 'requirementId', 'solutionId'], [], 'event.data')
        assertPortableIdentifier(value.requirementId, 'event.data.requirementId')
        assertPortableIdentifier(value.solutionId, 'event.data.solutionId')
      } else if (stage === 'DIAGRAMS') {
        assertExactKeys(value, [...common, 'definition'], [], 'event.data')
        assertDefinitionIdentity(value.definition, 'event.data.definition')
      } else if (stage === 'PLANNING') {
        assertExactKeys(value, common, [], 'event.data')
      } else if (stage === 'EXECUTION' || stage === 'REMEDIATION' || stage === 'DELIVERY') {
        assertExactKeys(value, [...common, 'candidateId'], [], 'event.data')
        assertPortableIdentifier(value.candidateId, 'event.data.candidateId')
      } else if (stage === 'VERIFICATION') {
        assertExactKeys(value, [...common, 'candidateId', 'outcome'], [], 'event.data')
        assertPortableIdentifier(value.candidateId, 'event.data.candidateId')
        if (!['passed', 'remediation-required'].includes(String(value.outcome))) {
          transitionError('INVALID_EVENT', 'event.data.outcome is invalid')
        }
      } else {
        transitionError('INVALID_EVENT', 'event.data.stage is invalid')
      }
      assertStageRunFields(value, 'event.data')
      return
    }
    case 'stage.failed':
      assertExactKeys(value, [
        'stage',
        'stageRunId',
        'attemptId',
        'category',
        'code',
        'message',
        'retryable',
      ], [], 'event.data')
      assertStageRunFields(value, 'event.data')
      if (!['task', 'infrastructure'].includes(String(value.category))) {
        transitionError('INVALID_EVENT', 'event.data.category is invalid')
      }
      assertNonEmptyText(value.code, 'event.data.code')
      assertNonEmptyText(value.message, 'event.data.message')
      if (typeof value.retryable !== 'boolean') {
        transitionError('INVALID_EVENT', 'event.data.retryable must be a boolean')
      }
      return
    case 'human-review.approved':
    case 'human-review.rejected':
      assertReviewData(value, 'event.data', false)
      return
    case 'human-review.changes-requested':
      assertReviewData(value, 'event.data', true)
      return
    case 'job.interrupted':
      assertExactKeys(value, ['reason'], ['stageRunId'], 'event.data')
      assertNonEmptyText(value.reason, 'event.data.reason')
      if (value.stageRunId !== undefined) {
        assertPortableIdentifier(value.stageRunId, 'event.data.stageRunId')
      }
      return
    case 'job.resumed':
      assertExactKeys(value, ['interruptionSequence'], [], 'event.data')
      if (typeof value.interruptionSequence !== 'string'
        || !SEQUENCE_PATTERN.test(value.interruptionSequence)) {
        transitionError('INVALID_EVENT', 'event.data.interruptionSequence is invalid')
      }
      return
    case 'job.cancelled':
      assertExactKeys(value, ['reason'], [], 'event.data')
      assertNonEmptyText(value.reason, 'event.data.reason')
      return
    case 'completion-gate.passed':
      assertExactKeys(value, ['stageRunId', 'candidateId'], [], 'event.data')
      assertPortableIdentifier(value.stageRunId, 'event.data.stageRunId')
      assertPortableIdentifier(value.candidateId, 'event.data.candidateId')
      return
    case 'completion-gate.failed':
      assertExactKeys(value, ['stageRunId', 'candidateId', 'reason'], [], 'event.data')
      assertPortableIdentifier(value.stageRunId, 'event.data.stageRunId')
      assertPortableIdentifier(value.candidateId, 'event.data.candidateId')
      assertNonEmptyText(value.reason, 'event.data.reason')
      return
    case 'job.delivered':
      assertExactKeys(value, ['candidateId'], [], 'event.data')
      assertPortableIdentifier(value.candidateId, 'event.data.candidateId')
  }
}

export function assertStrongFlowJobEvent(value: unknown): asserts value is StrongFlowJobEvent {
  assertLosslessJson(value, 'event', new Set())
  if (!isRecord(value)) transitionError('INVALID_EVENT', 'event must be an object')
  assertExactKeys(value, [
    'schemaVersion',
    'id',
    'jobId',
    'sequence',
    'occurredAtMillis',
    'source',
    'kind',
    'data',
  ], [], 'event')
  if (value.schemaVersion !== STRONGFLOW_JOB_EVENT_SCHEMA_VERSION) {
    transitionError('INVALID_EVENT', 'event schemaVersion is unsupported')
  }
  assertPortableIdentifier(value.jobId, 'event.jobId')
  if (typeof value.sequence !== 'string' || !SEQUENCE_PATTERN.test(value.sequence)) {
    transitionError('INVALID_SEQUENCE', 'event.sequence must be a canonical decimal string')
  }
  if (value.id !== strongFlowJobEventId(value.jobId, value.sequence)) {
    transitionError('INVALID_EVENT', 'event.id does not match its job and sequence')
  }
  if (
    typeof value.occurredAtMillis !== 'number'
    || !Number.isSafeInteger(value.occurredAtMillis)
    || value.occurredAtMillis < 0
  ) {
    transitionError('INVALID_EVENT', 'event.occurredAtMillis must be a non-negative safe integer')
  }
  assertSource(value.source)
  if (!Object.hasOwn(STRONGFLOW_JOB_EVENT_KINDS, String(value.kind))) {
    transitionError('INVALID_EVENT', 'event.kind is unsupported')
  }
  assertEventData(value.kind as StrongFlowJobEventKind, value.data)
}

const STRONGFLOW_JOB_EVENT_KINDS: Readonly<Record<StrongFlowJobEventKind, true>> = Object.freeze({
  'job.created': true,
  'stage.started': true,
  'stage.succeeded': true,
  'stage.failed': true,
  'human-review.approved': true,
  'human-review.changes-requested': true,
  'human-review.rejected': true,
  'job.interrupted': true,
  'job.resumed': true,
  'job.cancelled': true,
  'completion-gate.passed': true,
  'completion-gate.failed': true,
  'job.delivered': true,
})

export function strongFlowJobEventId(jobId: string, sequence: string): string {
  return `${jobId}@${sequence}`
}

export function createStrongFlowJobEvent<Kind extends StrongFlowJobEventKind>(
  input: CreateStrongFlowJobEventInput<Kind>,
): StrongFlowJobEventFor<Kind> {
  const event = {
    ...input,
    schemaVersion: STRONGFLOW_JOB_EVENT_SCHEMA_VERSION,
    id: strongFlowJobEventId(input.jobId, input.sequence),
  }
  assertStrongFlowJobEvent(event)
  return immutableClone(event) as StrongFlowJobEventFor<Kind>
}

function definitionIdentity(snapshot: StrongFlowJobSnapshot): DefinitionIdentity | undefined {
  const value = snapshot.definition
  if (
    value.requirementId === undefined
    || value.solutionId === undefined
    || value.systemArchitectureDiagramId === undefined
    || value.processFlowDiagramId === undefined
  ) return undefined
  return {
    requirementId: value.requirementId,
    solutionId: value.solutionId,
    systemArchitectureDiagramId: value.systemArchitectureDiagramId,
    processFlowDiagramId: value.processFlowDiagramId,
  }
}

function definitionsEqual(left: DefinitionIdentity, right: DefinitionIdentity): boolean {
  return left.requirementId === right.requirementId
    && left.solutionId === right.solutionId
    && left.systemArchitectureDiagramId === right.systemArchitectureDiagramId
    && left.processFlowDiagramId === right.processFlowDiagramId
}

function requireCurrentDefinition(
  snapshot: StrongFlowJobSnapshot,
  supplied: DefinitionIdentity,
): DefinitionIdentity {
  const current = definitionIdentity(snapshot)
  if (current === undefined || !definitionsEqual(current, supplied)) {
    transitionError('STALE_DEFINITION', 'human review does not reference the current definition')
  }
  return current
}

function requireCurrentApproval(snapshot: StrongFlowJobSnapshot): HumanReviewRecord {
  const definition = definitionIdentity(snapshot)
  if (
    definition === undefined
    || snapshot.approval?.payload.decision !== 'approved'
    || !definitionsEqual(snapshot.approval.payload.definition, definition)
  ) {
    transitionError('APPROVAL_REQUIRED', 'the current definition has no matching human approval')
  }
  return snapshot.approval
}

function requireCandidate(snapshot: StrongFlowJobSnapshot, candidateId: CandidateId): void {
  if (snapshot.candidateId === undefined || snapshot.candidateId !== candidateId) {
    transitionError('CANDIDATE_MISMATCH', 'event does not reference the current candidate')
  }
}

function requireHumanReviewSource(
  source: StrongFlowJobEventSource,
  reviewerId: string,
): asserts source is Extract<StrongFlowJobEventSource, { readonly kind: 'human' }> {
  if (source.kind !== 'human' || source.actorId !== reviewerId) {
    transitionError('INVALID_EVENT', 'human review must come from its named human reviewer')
  }
}

function requireControlSource(source: StrongFlowJobEventSource, label: string): void {
  if (source.kind === 'role') {
    transitionError('INVALID_EVENT', `${label} cannot be submitted by a model role`)
  }
}

function expectedStage(snapshot: StrongFlowJobSnapshot): StrongFlowJobStage {
  const expected = STRONGFLOW_STAGE_BY_STATE[snapshot.state]
  if (expected === undefined) {
    transitionError('ILLEGAL_TRANSITION', `state ${snapshot.state} does not run a role stage`)
  }
  return expected
}

function requireMatchingStage(
  snapshot: StrongFlowJobSnapshot,
  data: StageSucceededData | StageFailedData,
  source: StrongFlowJobEventSource,
): ActiveStageRun {
  const active = snapshot.activeStage
  if (
    active === undefined
    || active.stage !== data.stage
    || active.stageRunId !== data.stageRunId
    || active.attemptId !== data.attemptId
  ) {
    transitionError('STAGE_RUN_MISMATCH', 'stage settlement does not match the active run')
  }
  if (source.kind === 'human') {
    transitionError('INVALID_EVENT', 'a human source cannot settle a role stage')
  }
  if (source.kind === 'role' && active.actorId !== source.actorId) {
    transitionError('STAGE_RUN_MISMATCH', 'stage settlement changed its role identity')
  }
  if (
    source.kind === 'role'
    && active.kernelSessionId !== undefined
    && source.kernelSessionId !== active.kernelSessionId
  ) {
    transitionError('STAGE_RUN_MISMATCH', 'stage settlement changed its kernel session identity')
  }
  return active
}

function reviewRecord(
  data: HumanReviewApprovedData | HumanReviewChangesRequestedData | HumanReviewRejectedData,
  decision: HumanReviewRecord['payload']['decision'],
  jobId: JobId,
  occurredAtMillis: number,
  channel: HumanReviewChannel,
): HumanReviewRecord {
  return parseStrongFlowArtifactAs('HUMAN_REVIEW_RECORD', {
    schemaVersion: 1,
    artifactKind: 'HUMAN_REVIEW_RECORD',
    artifactId: data.reviewId,
    jobId,
    sourceArtifacts: [
      { artifactKind: 'REQUIREMENT_SPEC', artifactId: data.definition.requirementId },
      { artifactKind: 'SOLUTION_DESIGN', artifactId: data.definition.solutionId },
      {
        artifactKind: 'SYSTEM_ARCHITECTURE_DIAGRAM',
        artifactId: data.definition.systemArchitectureDiagramId,
      },
      {
        artifactKind: 'PROCESS_FLOW_DIAGRAM',
        artifactId: data.definition.processFlowDiagramId,
      },
    ],
    producer: { kind: 'human', actorId: data.reviewerId, channel },
    kernelEventInterval: null,
    createdAtMillis: occurredAtMillis,
    payload: {
      definition: data.definition,
      decision,
      comment: data.comment ?? null,
      scope: 'scope' in data ? data.scope : null,
    },
  })
}

function revisedDefinition(
  definition: JobDefinitionSnapshot,
  scope: DefinitionRevisionScope,
): JobDefinitionSnapshot {
  if (scope === 'requirements') return {}
  if (scope === 'solution') {
    return definition.requirementId === undefined
      ? {}
      : { requirementId: definition.requirementId }
  }
  return {
    ...(definition.requirementId === undefined
      ? {}
      : { requirementId: definition.requirementId }),
    ...(definition.solutionId === undefined ? {} : { solutionId: definition.solutionId }),
  }
}

function revisionState(scope: DefinitionRevisionScope): StrongFlowJobState {
  if (scope === 'requirements') return 'DEFINING_REQUIREMENTS'
  if (scope === 'solution') return 'DEFINING_SOLUTION'
  return 'DEFINING_DIAGRAMS'
}

function nextSequence(sequence: string): string {
  return (BigInt(sequence) + 1n).toString()
}

type StrongFlowJobSnapshotChanges = {
  readonly [Key in keyof StrongFlowJobSnapshot]?: StrongFlowJobSnapshot[Key] | undefined
}

function withEvent(
  snapshot: StrongFlowJobSnapshot,
  event: StrongFlowJobEvent,
  changes: StrongFlowJobSnapshotChanges,
): StrongFlowJobSnapshot {
  const next: Record<string, unknown> = {
    ...snapshot,
    ...changes,
    sequence: event.sequence,
    lastOccurredAtMillis: event.occurredAtMillis,
  }
  for (const [key, value] of Object.entries(next)) {
    if (value === undefined) delete next[key]
  }
  return immutableClone(next) as unknown as StrongFlowJobSnapshot
}

function applyStageSucceeded(
  snapshot: StrongFlowJobSnapshot,
  event: StrongFlowJobEventFor<'stage.succeeded'>,
): StrongFlowJobSnapshot {
  requireMatchingStage(snapshot, event.data, event.source)
  const data = event.data
  switch (data.stage) {
    case 'REQUIREMENTS':
      return withEvent(snapshot, event, {
        state: 'DEFINING_SOLUTION',
        definition: { requirementId: data.requirementId },
        activeStage: undefined,
        approval: undefined,
        candidateId: undefined,
        completionGate: undefined,
      })
    case 'SOLUTION':
      if (snapshot.definition.requirementId !== data.requirementId) {
        transitionError('STALE_DEFINITION', 'solution references a stale requirement')
      }
      return withEvent(snapshot, event, {
        state: 'DEFINING_DIAGRAMS',
        definition: {
          requirementId: data.requirementId,
          solutionId: data.solutionId,
        },
        activeStage: undefined,
        approval: undefined,
        candidateId: undefined,
        completionGate: undefined,
      })
    case 'DIAGRAMS':
      if (
        snapshot.definition.requirementId !== data.definition.requirementId
        || snapshot.definition.solutionId !== data.definition.solutionId
      ) {
        transitionError('STALE_DEFINITION', 'diagrams reference stale requirement or solution')
      }
      return withEvent(snapshot, event, {
        state: 'AWAITING_HUMAN_REVIEW',
        definition: data.definition,
        activeStage: undefined,
        approval: undefined,
        candidateId: undefined,
        completionGate: undefined,
      })
    case 'PLANNING':
      requireCurrentApproval(snapshot)
      return withEvent(snapshot, event, {
        state: 'EXECUTING',
        activeStage: undefined,
      })
    case 'EXECUTION':
      requireCurrentApproval(snapshot)
      return withEvent(snapshot, event, {
        state: 'VERIFYING',
        activeStage: undefined,
        candidateId: data.candidateId,
        completionGate: undefined,
      })
    case 'VERIFICATION':
      requireCurrentApproval(snapshot)
      requireCandidate(snapshot, data.candidateId)
      return withEvent(snapshot, event, {
        state: data.outcome === 'passed' ? 'AWAITING_COMPLETION_GATE' : 'REMEDIATING',
        activeStage: undefined,
        completionGate: undefined,
      })
    case 'REMEDIATION':
      requireCurrentApproval(snapshot)
      requireCandidate(snapshot, data.candidateId)
      return withEvent(snapshot, event, {
        state: 'VERIFYING',
        activeStage: undefined,
        candidateId: data.candidateId,
        completionGate: undefined,
      })
    case 'DELIVERY':
      requireCurrentApproval(snapshot)
      requireCandidate(snapshot, data.candidateId)
      if (snapshot.completionGate?.outcome !== 'passed') {
        transitionError('APPROVAL_REQUIRED', 'delivery requires a passed completion gate')
      }
      return withEvent(snapshot, event, {
        state: 'READY_TO_DELIVER',
        activeStage: undefined,
      })
  }
}

function applyHumanReview(
  snapshot: StrongFlowJobSnapshot,
  event: Extract<StrongFlowJobEvent, {
    readonly kind:
      | 'human-review.approved'
      | 'human-review.changes-requested'
      | 'human-review.rejected'
  }>,
): StrongFlowJobSnapshot {
  if (snapshot.activeStage !== undefined) {
    transitionError('ACTIVE_STAGE_EXISTS', 'human review cannot replace an active stage run')
  }
  requireHumanReviewSource(event.source, event.data.reviewerId)
  requireCurrentDefinition(snapshot, event.data.definition)
  if (event.kind === 'human-review.approved') {
    const approval = reviewRecord(
      event.data,
      'approved',
      event.jobId,
      event.occurredAtMillis,
      event.source.channel,
    )
    return withEvent(snapshot, event, {
      state: 'PLANNING',
      approval,
      lastHumanReview: approval,
      lastStop: undefined,
    })
  }
  if (event.kind === 'human-review.rejected') {
    const rejection = reviewRecord(
      event.data,
      'rejected',
      event.jobId,
      event.occurredAtMillis,
      event.source.channel,
    )
    return withEvent(snapshot, event, {
      state: 'REJECTED',
      approval: undefined,
      lastHumanReview: rejection,
      lastStop: {
        kind: 'human-rejection',
        occurredAtMillis: event.occurredAtMillis,
        message: event.data.comment ?? 'The human reviewer rejected the definition.',
      },
    })
  }
  const changes = reviewRecord(
    event.data,
    'changes-requested',
    event.jobId,
    event.occurredAtMillis,
    event.source.channel,
  )
  return withEvent(snapshot, event, {
    state: revisionState(event.data.scope),
    definitionRevision: snapshot.definitionRevision + 1,
    definition: revisedDefinition(snapshot.definition, event.data.scope),
    approval: undefined,
    lastHumanReview: changes,
    activeStage: undefined,
    candidateId: undefined,
    completionGate: undefined,
    interruption: undefined,
    lastStop: undefined,
  })
}

export function applyStrongFlowJobEvent(
  snapshot: StrongFlowJobSnapshot | undefined,
  event: StrongFlowJobEvent,
): StrongFlowJobSnapshot {
  assertStrongFlowJobEvent(event)
  if (snapshot === undefined) {
    if (event.kind !== 'job.created') {
      transitionError('ILLEGAL_TRANSITION', 'the first job event must be job.created')
    }
    if (event.sequence !== '1') {
      transitionError('INVALID_SEQUENCE', 'job.created must have sequence 1')
    }
    if (event.source.kind !== 'system') {
      transitionError('INVALID_EVENT', 'job.created must come from the StrongFlow system')
    }
    return immutableClone({
      schemaVersion: STRONGFLOW_JOB_SNAPSHOT_SCHEMA_VERSION,
      jobId: event.jobId,
      ...(event.data.title === undefined ? {} : { title: event.data.title }),
      sequence: event.sequence,
      lastOccurredAtMillis: event.occurredAtMillis,
      state: 'DEFINING_REQUIREMENTS',
      definitionRevision: 1,
      definition: {},
    })
  }
  if (event.jobId !== snapshot.jobId) {
    transitionError('WRONG_JOB', `event belongs to ${event.jobId}, not ${snapshot.jobId}`)
  }
  if (event.sequence !== nextSequence(snapshot.sequence)) {
    transitionError(
      'INVALID_SEQUENCE',
      `event sequence ${event.sequence} does not follow ${snapshot.sequence}`,
    )
  }
  if (event.occurredAtMillis < snapshot.lastOccurredAtMillis) {
    transitionError('INVALID_EVENT', 'event time moved backwards')
  }
  const allowed = STRONGFLOW_JOB_TRANSITIONS[snapshot.state]
  if (!allowed.includes(event.kind)) {
    transitionError(
      TERMINAL_STATES.has(snapshot.state) ? 'TERMINAL_JOB' : 'ILLEGAL_TRANSITION',
      `${event.kind} is not legal from ${snapshot.state}`,
    )
  }

  switch (event.kind) {
    case 'job.created':
      return transitionError('ILLEGAL_TRANSITION', 'job.created may only be the first event')
    case 'stage.started': {
      if (snapshot.activeStage !== undefined) {
        transitionError('ACTIVE_STAGE_EXISTS', 'a stage run is already active')
      }
      if (event.source.kind === 'human') {
        transitionError('INVALID_EVENT', 'a human source cannot run a role stage')
      }
      const stage = expectedStage(snapshot)
      if (stage !== event.data.stage) {
        transitionError(
          'ILLEGAL_TRANSITION',
          `state ${snapshot.state} requires stage ${stage}, not ${event.data.stage}`,
        )
      }
      if (!stage.startsWith('REQUIRE')
        && !['SOLUTION', 'DIAGRAMS'].includes(stage)) requireCurrentApproval(snapshot)
      return withEvent(snapshot, event, {
        activeStage: {
          stage,
          stageRunId: event.data.stageRunId,
          attemptId: event.data.attemptId,
          actorId: event.source.actorId,
          ...(event.source.kind === 'role' && event.source.kernelSessionId !== undefined
            ? { kernelSessionId: event.source.kernelSessionId }
            : {}),
          startedAtMillis: event.occurredAtMillis,
        },
      })
    }
    case 'stage.succeeded':
      return applyStageSucceeded(snapshot, event)
    case 'stage.failed': {
      requireMatchingStage(snapshot, event.data, event.source)
      return withEvent(snapshot, event, {
        state: 'FAILED',
        activeStage: undefined,
        lastStop: {
          kind: event.data.category === 'task'
            ? 'task-failure'
            : 'infrastructure-failure',
          occurredAtMillis: event.occurredAtMillis,
          message: event.data.message,
          code: event.data.code,
          retryable: event.data.retryable,
          stage: event.data.stage,
          stageRunId: event.data.stageRunId,
        },
      })
    }
    case 'human-review.approved':
    case 'human-review.changes-requested':
    case 'human-review.rejected':
      return applyHumanReview(snapshot, event)
    case 'job.interrupted': {
      if (event.source.kind === 'human') {
        transitionError('INVALID_EVENT', 'job interruption must come from runtime control')
      }
      if (
        event.data.stageRunId !== undefined
        && snapshot.activeStage?.stageRunId !== event.data.stageRunId
      ) {
        transitionError('STAGE_RUN_MISMATCH', 'interruption names a different stage run')
      }
      const resumeState = snapshot.state as Exclude<StrongFlowJobState, 'INTERRUPTED'>
      return withEvent(snapshot, event, {
        state: 'INTERRUPTED',
        activeStage: undefined,
        interruption: {
          sequence: event.sequence,
          resumeState,
          reason: event.data.reason,
          ...(event.data.stageRunId === undefined
            ? {}
            : { stageRunId: event.data.stageRunId }),
        },
        lastStop: {
          kind: 'interruption',
          occurredAtMillis: event.occurredAtMillis,
          message: event.data.reason,
          ...(snapshot.activeStage === undefined
            ? {}
            : {
              stage: snapshot.activeStage.stage,
              stageRunId: snapshot.activeStage.stageRunId,
            }),
        },
      })
    }
    case 'job.resumed':
      requireControlSource(event.source, 'job resume')
      if (
        snapshot.interruption === undefined
        || snapshot.interruption.sequence !== event.data.interruptionSequence
      ) {
        transitionError('ILLEGAL_TRANSITION', 'resume does not reference the current interruption')
      }
      return withEvent(snapshot, event, {
        state: snapshot.interruption.resumeState,
        interruption: undefined,
      })
    case 'job.cancelled':
      requireControlSource(event.source, 'job cancellation')
      return withEvent(snapshot, event, {
        state: 'CANCELLED',
        activeStage: undefined,
        interruption: undefined,
        lastStop: {
          kind: 'cancellation',
          occurredAtMillis: event.occurredAtMillis,
          message: event.data.reason,
        },
      })
    case 'completion-gate.passed':
      requireControlSource(event.source, 'completion gate')
      requireCurrentApproval(snapshot)
      requireCandidate(snapshot, event.data.candidateId)
      return withEvent(snapshot, event, {
        state: 'DELIVERING',
        completionGate: {
          stageRunId: event.data.stageRunId,
          candidateId: event.data.candidateId,
          outcome: 'passed',
          occurredAtMillis: event.occurredAtMillis,
        },
      })
    case 'completion-gate.failed':
      requireControlSource(event.source, 'completion gate')
      requireCurrentApproval(snapshot)
      requireCandidate(snapshot, event.data.candidateId)
      return withEvent(snapshot, event, {
        state: 'REMEDIATING',
        completionGate: {
          stageRunId: event.data.stageRunId,
          candidateId: event.data.candidateId,
          outcome: 'failed',
          occurredAtMillis: event.occurredAtMillis,
          reason: event.data.reason,
        },
      })
    case 'job.delivered':
      requireControlSource(event.source, 'job delivery')
      requireCurrentApproval(snapshot)
      requireCandidate(snapshot, event.data.candidateId)
      if (
        snapshot.completionGate?.outcome !== 'passed'
        || snapshot.completionGate.candidateId !== event.data.candidateId
      ) {
        transitionError('APPROVAL_REQUIRED', 'DELIVERED requires a passed current completion gate')
      }
      return withEvent(snapshot, event, {
        state: 'DELIVERED',
        deliveredAtMillis: event.occurredAtMillis,
      })
  }
}

export function projectStrongFlowJob(
  events: readonly StrongFlowJobEvent[],
): StrongFlowJobSnapshot {
  if (events.length === 0) {
    transitionError('INVALID_EVENT', 'a job projection requires at least one event')
  }
  let snapshot: StrongFlowJobSnapshot | undefined
  for (const event of events) snapshot = applyStrongFlowJobEvent(snapshot, event)
  if (snapshot === undefined) transitionError('INVALID_EVENT', 'job projection produced no state')
  return snapshot
}
