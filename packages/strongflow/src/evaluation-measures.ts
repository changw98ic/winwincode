import {
  ATTENTION_ITEM_TYPES,
  EVIDENCE_REF_TYPES,
  parseDelivery,
  parseDeliveryVerdict,
  type AttentionItemType,
  type Delivery,
  type DeliveryVerdict,
  type EvidenceRefType,
} from '@winwincode/contracts'

export const DELIVERY_MEASURES_SCHEMA_VERSION = 1 as const

export const DELIVERY_MEASURES_RUN_KINDS = Object.freeze([
  'deterministic',
  'live',
] as const)

export type DeliveryMeasuresRunKind = typeof DELIVERY_MEASURES_RUN_KINDS[number]

export const DELIVERY_MEASURES_RUN_STATES = Object.freeze([
  'running',
  'completed',
  'failed',
  'budget-exceeded',
  'interrupted',
] as const)

export type DeliveryMeasuresRunState = typeof DELIVERY_MEASURES_RUN_STATES[number]

export type DeliveryMeasureSourceKind =
  | 'evaluation-run'
  | 'delivery'
  | 'criterion'
  | 'criterion-result'
  | 'verdict'
  | 'evidence'
  | 'stage-run'
  | 'attention'
  | 'runtime-session'
  | 'runtime-event'
  | 'model-call'
  | 'pricing'

export interface DeliveryMeasureSourceRef {
  readonly kind: DeliveryMeasureSourceKind
  readonly ref: string
}

export interface SourcedMeasure<Value> {
  readonly value: Value
  readonly sourceRefs: readonly DeliveryMeasureSourceRef[]
}

export type DeliveryMeasuresModelCallStatus = 'running' | 'completed' | 'failed'

export interface DeliveryMeasuresModelCallFact {
  readonly sourceRef: string
  readonly status: DeliveryMeasuresModelCallStatus
  readonly startedAtMillis: number | null
  readonly finishedAtMillis: number | null
  readonly inputTokens: number | null
  readonly outputTokens: number | null
  readonly cacheReadTokens: number | null
  readonly cacheWriteTokens: number | null
  readonly costUsdMicros: number | null
}

export interface DeliveryMeasuresRuntimeEventLink {
  readonly eventId: string
  readonly sourceRef: string
  readonly sequence: string
}

export interface DeliveryMeasuresRuntimeAgent {
  readonly threadId: string
  readonly parentThreadId: string | null
  readonly status: string
  readonly firstEvent: DeliveryMeasuresRuntimeEventLink
  readonly latestEvent: DeliveryMeasuresRuntimeEventLink
}

export interface DeliveryMeasuresRuntimeActivity {
  readonly outcome:
    | 'observed'
    | 'succeeded'
    | 'task-failed'
    | 'timed-out'
    | 'policy-denied'
    | 'infrastructure-failed'
    | 'cancelled'
  readonly firstEvent: DeliveryMeasuresRuntimeEventLink
  readonly latestEvent: DeliveryMeasuresRuntimeEventLink
}

export interface DeliveryMeasuresRuntimeInteraction {
  readonly interactionType: 'execution-approval' | 'user-input'
  readonly blocking: boolean
  readonly status: 'pending' | 'resolved'
  readonly requestedEvent: DeliveryMeasuresRuntimeEventLink
  readonly resolvedEvent: DeliveryMeasuresRuntimeEventLink | null
}

export interface DeliveryMeasuresRuntimeFailure {
  readonly event: DeliveryMeasuresRuntimeEventLink
}

export interface DeliveryMeasuresRuntimeRecovery {
  readonly failureCount: number
  readonly recoveryCount: number
  readonly lastFailureEvent: DeliveryMeasuresRuntimeEventLink | null
  readonly latestRecoveryEvent: DeliveryMeasuresRuntimeEventLink | null
}

export interface DeliveryMeasuresRuntimeSession {
  readonly binding: {
    readonly id: string
    readonly stageRunId: string
  }
  readonly agents: readonly DeliveryMeasuresRuntimeAgent[]
  readonly activities: readonly DeliveryMeasuresRuntimeActivity[]
  readonly interactions: readonly DeliveryMeasuresRuntimeInteraction[]
  readonly failures: readonly DeliveryMeasuresRuntimeFailure[]
  readonly recovery: DeliveryMeasuresRuntimeRecovery
}

export interface DeliveryMeasuresRuntimeStage {
  readonly stageRun: {
    readonly id: string
  }
  readonly sessions: readonly DeliveryMeasuresRuntimeSession[]
}

export interface DeliveryMeasuresRuntimeProjection {
  readonly deliveryId: string
  readonly deliveryRevision: number
  readonly stages: readonly DeliveryMeasuresRuntimeStage[]
}

export interface CreateDeliveryMeasuresProjectionInput {
  readonly schemaVersion: typeof DELIVERY_MEASURES_SCHEMA_VERSION
  readonly runKind: DeliveryMeasuresRunKind
  readonly runId: string
  readonly runState: DeliveryMeasuresRunState
  readonly startedAtMillis: number
  readonly finishedAtMillis: number | null
  readonly delivery: Delivery | null
  readonly runtimeProjection: DeliveryMeasuresRuntimeProjection | null
  readonly requiredVerificationRoles: readonly string[]
  readonly modelCalls: readonly DeliveryMeasuresModelCallFact[]
  readonly pricingSource: string | null
  readonly historicalVerdicts?: readonly DeliveryVerdict[]
}

export type DeliveryCompletenessStatus =
  | 'complete'
  | 'failed'
  | 'blocked-by-infrastructure'
  | 'incomplete'
  | 'not-available'

export interface DeliveryCriterionMeasure {
  readonly criterionId: string
  readonly required: SourcedMeasure<boolean>
  readonly verdict: SourcedMeasure<
    'pass' | 'fail' | 'inconclusive' | 'infra_error' | 'missing'
  >
  readonly evidenceCount: SourcedMeasure<number>
}

export interface DeliveryCompletenessMeasures {
  readonly status: SourcedMeasure<DeliveryCompletenessStatus>
  readonly criterionCount: SourcedMeasure<number>
  readonly requiredCriterionCount: SourcedMeasure<number>
  readonly optionalCriterionCount: SourcedMeasure<number>
  readonly passCount: SourcedMeasure<number>
  readonly failCount: SourcedMeasure<number>
  readonly inconclusiveCount: SourcedMeasure<number>
  readonly infrastructureErrorCount: SourcedMeasure<number>
  readonly missingResultCount: SourcedMeasure<number>
  readonly requiredPassRate: SourcedMeasure<number | null>
  readonly criteria: readonly DeliveryCriterionMeasure[]
}

export type DeliveryConfidenceStatus =
  | 'not-evaluated'
  | 'insufficient'
  | 'independently-supported'

export interface DeliveryVerificationRoleMeasure {
  readonly role: string
  readonly required: SourcedMeasure<boolean>
  readonly settled: SourcedMeasure<boolean>
  readonly currentFindingCount: SourcedMeasure<number>
}

export interface DeliveryConfidenceMeasures {
  readonly status: SourcedMeasure<DeliveryConfidenceStatus>
  readonly currentCandidateEvidenceCount: SourcedMeasure<number>
  readonly referencedEvidenceCount: SourcedMeasure<number>
  readonly directEvidenceCount: SourcedMeasure<number>
  readonly reviewFindingCount: SourcedMeasure<number>
  readonly missingEvidenceRefCount: SourcedMeasure<number>
  readonly unresolvedFindingCount: SourcedMeasure<number>
  readonly requiredRoleCount: SourcedMeasure<number>
  readonly settledRoleCount: SourcedMeasure<number>
  readonly rolesWithCurrentFindingsCount: SourcedMeasure<number>
  readonly evidenceByType: Readonly<Record<EvidenceRefType, SourcedMeasure<number>>>
  readonly roles: readonly DeliveryVerificationRoleMeasure[]
}

export type DeliveryStabilityStatus =
  | 'stable'
  | 'reworked'
  | 'failed'
  | 'infrastructure-affected'
  | 'interrupted'

export interface DeliveryStabilityMeasures {
  readonly status: SourcedMeasure<DeliveryStabilityStatus>
  readonly failedStageCount: SourcedMeasure<number>
  readonly cancelledStageCount: SourcedMeasure<number>
  readonly retriedStageCount: SourcedMeasure<number>
  readonly reworkStageCount: SourcedMeasure<number>
  readonly taskFailureCount: SourcedMeasure<number>
  readonly timeoutCount: SourcedMeasure<number>
  readonly policyDenialCount: SourcedMeasure<number>
  readonly infrastructureFailureCount: SourcedMeasure<number>
  readonly cancelledActivityCount: SourcedMeasure<number>
  readonly runtimeFailureEventCount: SourcedMeasure<number>
  readonly reportedRecoveryFailureCount: SourcedMeasure<number>
  readonly recoveryEventCount: SourcedMeasure<number>
  readonly failedAgentCount: SourcedMeasure<number>
  readonly interruptedAgentCount: SourcedMeasure<number>
  readonly historicalNonPassVerdictCount: SourcedMeasure<number>
}

export type DeliveryHumanDependenceStatus =
  | 'none'
  | 'review-only'
  | 'intervention-required'
  | 'blocked'

export interface DeliveryHumanDependenceMeasures {
  readonly status: SourcedMeasure<DeliveryHumanDependenceStatus>
  readonly humanStageCount: SourcedMeasure<number>
  readonly codexStageCount: SourcedMeasure<number>
  readonly attentionCount: SourcedMeasure<number>
  readonly openAttentionCount: SourcedMeasure<number>
  readonly resolvedAttentionCount: SourcedMeasure<number>
  readonly dismissedAttentionCount: SourcedMeasure<number>
  readonly blockingAttentionCount: SourcedMeasure<number>
  readonly openBlockingAttentionCount: SourcedMeasure<number>
  readonly attentionByType: Readonly<Record<AttentionItemType, SourcedMeasure<number>>>
  readonly executionApprovalRequestCount: SourcedMeasure<number>
  readonly executionApprovalResolutionCount: SourcedMeasure<number>
  readonly userInputRequestCount: SourcedMeasure<number>
  readonly userInputResolutionCount: SourcedMeasure<number>
  readonly pendingInteractionCount: SourcedMeasure<number>
}

export interface DeliveryEfficiencyMeasures {
  readonly runElapsedMillis: SourcedMeasure<number | null>
  readonly settledStageMillis: SourcedMeasure<number>
  readonly unfinishedStageCount: SourcedMeasure<number>
  readonly stageCount: SourcedMeasure<number>
  readonly modelCallCount: SourcedMeasure<number>
  readonly completedModelCallCount: SourcedMeasure<number>
  readonly failedModelCallCount: SourcedMeasure<number>
  readonly runningModelCallCount: SourcedMeasure<number>
  readonly missingUsageCallCount: SourcedMeasure<number>
  readonly inputTokens: SourcedMeasure<number>
  readonly outputTokens: SourcedMeasure<number>
  readonly cacheReadTokens: SourcedMeasure<number>
  readonly cacheWriteTokens: SourcedMeasure<number>
  readonly totalTokens: SourcedMeasure<number>
  readonly costUsdMicros: SourcedMeasure<number>
  readonly missingCostCallCount: SourcedMeasure<number>
  readonly pricingSource: SourcedMeasure<string | null>
  readonly modelElapsedMillis: SourcedMeasure<number | null>
  readonly missingModelTimingCallCount: SourcedMeasure<number>
  readonly agentCount: SourcedMeasure<number>
  readonly subagentCount: SourcedMeasure<number>
  readonly parallelismObservationAvailable: SourcedMeasure<boolean>
  readonly maxConcurrentAgents: SourcedMeasure<number | null>
  readonly parallelExecutionObserved: SourcedMeasure<boolean>
}

export type DeliveryOutcomeClassification =
  | 'proven-success'
  | 'claimed-without-proof'
  | 'proof-not-claimed'
  | 'no-success-claim'

export interface DeliveryOutcomeMeasures {
  readonly successClaimPresent: SourcedMeasure<boolean>
  readonly completionProofPresent: SourcedMeasure<boolean>
  readonly falseSuccessRisk: SourcedMeasure<boolean>
  readonly falseFailureRisk: SourcedMeasure<boolean>
  readonly classification: SourcedMeasure<DeliveryOutcomeClassification>
}

export interface DeliveryMeasuresProjection {
  readonly schemaVersion: typeof DELIVERY_MEASURES_SCHEMA_VERSION
  readonly runKind: DeliveryMeasuresRunKind
  readonly runId: string
  readonly runState: DeliveryMeasuresRunState
  readonly deliveryId: string | null
  readonly deliveryRevision: number | null
  readonly outcome: DeliveryOutcomeMeasures
  readonly dimensions: {
    readonly completeness: DeliveryCompletenessMeasures
    readonly confidence: DeliveryConfidenceMeasures
    readonly stability: DeliveryStabilityMeasures
    readonly humanDependence: DeliveryHumanDependenceMeasures
    readonly efficiency: DeliveryEfficiencyMeasures
  }
}

export interface GroupedDeliveryMeasures {
  readonly deterministic: readonly DeliveryMeasuresProjection[]
  readonly live: readonly DeliveryMeasuresProjection[]
}

export type DeliveryMeasuresErrorCode =
  | 'INVALID_INPUT'
  | 'DUPLICATE_FACT'
  | 'RELATIONSHIP_MISMATCH'

export class DeliveryMeasuresError extends Error {
  readonly code: DeliveryMeasuresErrorCode
  readonly path: string

  constructor(code: DeliveryMeasuresErrorCode, path: string, message: string) {
    super(message)
    this.name = 'DeliveryMeasuresError'
    this.code = code
    this.path = path
  }
}

interface NormalizedRuntimeFacts {
  readonly sessions: readonly NormalizedRuntimeSession[]
}

interface NormalizedRuntimeSession {
  readonly source: DeliveryMeasureSourceRef
  readonly bindingId: string
  readonly stageRunId: string
  readonly agents: readonly DeliveryMeasuresRuntimeAgent[]
  readonly activities: readonly DeliveryMeasuresRuntimeActivity[]
  readonly interactions: readonly DeliveryMeasuresRuntimeInteraction[]
  readonly failures: readonly DeliveryMeasuresRuntimeFailure[]
  readonly recovery: DeliveryMeasuresRuntimeRecovery
}

interface NormalizedInput {
  readonly runKind: DeliveryMeasuresRunKind
  readonly runId: string
  readonly runState: DeliveryMeasuresRunState
  readonly startedAtMillis: number
  readonly finishedAtMillis: number | null
  readonly delivery: Delivery | null
  readonly runtime: NormalizedRuntimeFacts | null
  readonly requiredVerificationRoles: readonly string[]
  readonly modelCalls: readonly DeliveryMeasuresModelCallFact[]
  readonly pricingSource: string | null
  readonly historicalVerdicts: readonly DeliveryVerdict[]
}

const portableRunIdPattern = /^[A-Za-z0-9][A-Za-z0-9._:@/-]{0,199}$/u
const runtimeActivityOutcomes = new Set([
  'observed',
  'succeeded',
  'task-failed',
  'timed-out',
  'policy-denied',
  'infrastructure-failed',
  'cancelled',
])
const runtimeInteractionTypes = new Set(['execution-approval', 'user-input'])
const runtimeInteractionStatuses = new Set(['pending', 'resolved'])

function measuresError(
  code: DeliveryMeasuresErrorCode,
  path: string,
  message: string,
): never {
  throw new DeliveryMeasuresError(code, path, message)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) measuresError('INVALID_INPUT', path, `${path} must be an object`)
  return value
}

function nonEmptyText(value: unknown, path: string): string {
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > 65_536
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    measuresError('INVALID_INPUT', path, `${path} must be bounded non-empty text`)
  }
  return value
}

function portableRunId(value: unknown, path: string): string {
  if (typeof value !== 'string' || !portableRunIdPattern.test(value)) {
    measuresError('INVALID_INPUT', path, `${path} must be a portable run identity`)
  }
  return value
}

function nonNegativeInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    measuresError('INVALID_INPUT', path, `${path} must be a non-negative safe integer`)
  }
  return Number(value)
}

function nullableNonNegativeInteger(value: unknown, path: string): number | null {
  return value === null ? null : nonNegativeInteger(value, path)
}

function safeSum(values: readonly number[], path: string): number {
  let total = 0
  for (const value of values) {
    const next = total + value
    if (!Number.isSafeInteger(next)) {
      measuresError('INVALID_INPUT', path, `${path} exceeds the safe integer range`)
    }
    total = next
  }
  return total
}

function stringEnum<const Values extends readonly string[]>(
  value: unknown,
  values: Values,
  path: string,
): Values[number] {
  if (typeof value !== 'string' || !values.includes(value)) {
    measuresError('INVALID_INPUT', path, `${path} is unsupported`)
  }
  return value as Values[number]
}

function source(kind: DeliveryMeasureSourceKind, ref: string): DeliveryMeasureSourceRef {
  return Object.freeze({ kind, ref: nonEmptyText(ref, 'sourceRef') })
}

function sourceKey(value: DeliveryMeasureSourceRef): string {
  return `${value.kind}\u0000${value.ref}`
}

function sources(
  values: readonly DeliveryMeasureSourceRef[],
): readonly DeliveryMeasureSourceRef[] {
  const unique = new Map(values.map(value => [sourceKey(value), value]))
  return Object.freeze([...unique.values()].toSorted((left, right) => (
    left.kind.localeCompare(right.kind) || left.ref.localeCompare(right.ref)
  )))
}

function measured<Value>(
  value: Value,
  sourceRefs: readonly DeliveryMeasureSourceRef[],
): SourcedMeasure<Value> {
  const normalized = sources(sourceRefs)
  if (normalized.length === 0) {
    measuresError('INVALID_INPUT', 'sourceRefs', 'every measure requires a source reference')
  }
  return Object.freeze({ value, sourceRefs: normalized })
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

function eventLink(value: unknown, path: string): DeliveryMeasuresRuntimeEventLink {
  const input = record(value, path)
  const sequence = nonEmptyText(input.sequence, `${path}.sequence`)
  try {
    if (BigInt(sequence) < 0n) throw new Error('negative sequence')
  } catch {
    measuresError('INVALID_INPUT', `${path}.sequence`, 'runtime sequence must be non-negative')
  }
  return Object.freeze({
    eventId: nonEmptyText(input.eventId, `${path}.eventId`),
    sourceRef: nonEmptyText(input.sourceRef, `${path}.sourceRef`),
    sequence,
  })
}

function optionalEventLink(
  value: unknown,
  path: string,
): DeliveryMeasuresRuntimeEventLink | null {
  return value === null ? null : eventLink(value, path)
}

function normalizeAgent(value: unknown, path: string): DeliveryMeasuresRuntimeAgent {
  const input = record(value, path)
  return Object.freeze({
    threadId: nonEmptyText(input.threadId, `${path}.threadId`),
    parentThreadId: input.parentThreadId === null
      ? null
      : nonEmptyText(input.parentThreadId, `${path}.parentThreadId`),
    status: nonEmptyText(input.status, `${path}.status`),
    firstEvent: eventLink(input.firstEvent, `${path}.firstEvent`),
    latestEvent: eventLink(input.latestEvent, `${path}.latestEvent`),
  })
}

function normalizeActivity(value: unknown, path: string): DeliveryMeasuresRuntimeActivity {
  const input = record(value, path)
  const outcome = nonEmptyText(input.outcome, `${path}.outcome`)
  if (!runtimeActivityOutcomes.has(outcome)) {
    measuresError('INVALID_INPUT', `${path}.outcome`, 'runtime activity outcome is unsupported')
  }
  return Object.freeze({
    outcome: outcome as DeliveryMeasuresRuntimeActivity['outcome'],
    firstEvent: eventLink(input.firstEvent, `${path}.firstEvent`),
    latestEvent: eventLink(input.latestEvent, `${path}.latestEvent`),
  })
}

function normalizeInteraction(
  value: unknown,
  path: string,
): DeliveryMeasuresRuntimeInteraction {
  const input = record(value, path)
  const interactionType = nonEmptyText(input.interactionType, `${path}.interactionType`)
  const status = nonEmptyText(input.status, `${path}.status`)
  if (!runtimeInteractionTypes.has(interactionType)) {
    measuresError(
      'INVALID_INPUT',
      `${path}.interactionType`,
      'runtime interaction type is unsupported',
    )
  }
  if (!runtimeInteractionStatuses.has(status)) {
    measuresError('INVALID_INPUT', `${path}.status`, 'runtime interaction status is unsupported')
  }
  if (typeof input.blocking !== 'boolean') {
    measuresError('INVALID_INPUT', `${path}.blocking`, 'runtime interaction blocking must be boolean')
  }
  const resolvedEvent = optionalEventLink(input.resolvedEvent, `${path}.resolvedEvent`)
  if ((status === 'resolved') !== (resolvedEvent !== null)) {
    measuresError(
      'RELATIONSHIP_MISMATCH',
      path,
      'resolved runtime interactions require one resolution event',
    )
  }
  return Object.freeze({
    interactionType: interactionType as DeliveryMeasuresRuntimeInteraction['interactionType'],
    blocking: input.blocking,
    status: status as DeliveryMeasuresRuntimeInteraction['status'],
    requestedEvent: eventLink(input.requestedEvent, `${path}.requestedEvent`),
    resolvedEvent,
  })
}

function normalizeRecovery(value: unknown, path: string): DeliveryMeasuresRuntimeRecovery {
  const input = record(value, path)
  return Object.freeze({
    failureCount: nonNegativeInteger(input.failureCount, `${path}.failureCount`),
    recoveryCount: nonNegativeInteger(input.recoveryCount, `${path}.recoveryCount`),
    lastFailureEvent: optionalEventLink(input.lastFailureEvent, `${path}.lastFailureEvent`),
    latestRecoveryEvent: optionalEventLink(
      input.latestRecoveryEvent,
      `${path}.latestRecoveryEvent`,
    ),
  })
}

function normalizeRuntime(
  value: unknown,
  delivery: Delivery | null,
): NormalizedRuntimeFacts | null {
  if (value === null) return null
  if (delivery === null) {
    measuresError(
      'RELATIONSHIP_MISMATCH',
      'runtimeProjection',
      'runtime projection requires a Delivery',
    )
  }
  const input = record(value, 'runtimeProjection')
  if (input.deliveryId !== delivery.id || input.deliveryRevision !== delivery.revision) {
    measuresError(
      'RELATIONSHIP_MISMATCH',
      'runtimeProjection',
      'runtime projection must match the measured Delivery revision',
    )
  }
  if (!Array.isArray(input.stages)) {
    measuresError('INVALID_INPUT', 'runtimeProjection.stages', 'runtime stages must be an array')
  }
  const deliveryStages = new Map<string, Delivery['stageRuns'][number]>(
    delivery.stageRuns.map(stage => [stage.id, stage]),
  )
  const deliveryBindings = new Map<string, Delivery['sessionBindings'][number]>(
    delivery.sessionBindings.map(binding => [binding.id, binding]),
  )
  const seenStages = new Set<string>()
  const seenBindings = new Set<string>()
  const sessions: NormalizedRuntimeSession[] = []
  input.stages.forEach((stageValue, stageIndex) => {
    const stagePath = `runtimeProjection.stages[${String(stageIndex)}]`
    const stage = record(stageValue, stagePath)
    const stageRun = record(stage.stageRun, `${stagePath}.stageRun`)
    const stageRunId = nonEmptyText(stageRun.id, `${stagePath}.stageRun.id`)
    if (!deliveryStages.has(stageRunId)) {
      measuresError(
        'RELATIONSHIP_MISMATCH',
        `${stagePath}.stageRun.id`,
        'runtime stage is absent from the Delivery',
      )
    }
    if (seenStages.has(stageRunId)) {
      measuresError('DUPLICATE_FACT', `${stagePath}.stageRun.id`, 'runtime stage is duplicated')
    }
    seenStages.add(stageRunId)
    if (!Array.isArray(stage.sessions)) {
      measuresError('INVALID_INPUT', `${stagePath}.sessions`, 'runtime sessions must be an array')
    }
    stage.sessions.forEach((sessionValue, sessionIndex) => {
      const sessionPath = `${stagePath}.sessions[${String(sessionIndex)}]`
      const session = record(sessionValue, sessionPath)
      const binding = record(session.binding, `${sessionPath}.binding`)
      const bindingId = nonEmptyText(binding.id, `${sessionPath}.binding.id`)
      const bindingStageRunId = nonEmptyText(
        binding.stageRunId,
        `${sessionPath}.binding.stageRunId`,
      )
      const deliveryBinding = deliveryBindings.get(bindingId)
      if (deliveryBinding === undefined
        || deliveryBinding.stageRunId !== stageRunId
        || bindingStageRunId !== stageRunId) {
        measuresError(
          'RELATIONSHIP_MISMATCH',
          `${sessionPath}.binding`,
          'runtime session binding does not match its Delivery stage',
        )
      }
      if (seenBindings.has(bindingId)) {
        measuresError('DUPLICATE_FACT', `${sessionPath}.binding.id`, 'runtime binding is duplicated')
      }
      seenBindings.add(bindingId)
      for (const collection of ['agents', 'activities', 'interactions', 'failures'] as const) {
        if (!Array.isArray(session[collection])) {
          measuresError(
            'INVALID_INPUT',
            `${sessionPath}.${collection}`,
            `runtime ${collection} must be an array`,
          )
        }
      }
      const agentFacts = session.agents as readonly unknown[]
      const activityFacts = session.activities as readonly unknown[]
      const interactionFacts = session.interactions as readonly unknown[]
      const failureFacts = session.failures as readonly unknown[]
      const agents = agentFacts.map((agent, index) => normalizeAgent(
        agent,
        `${sessionPath}.agents[${String(index)}]`,
      ))
      if (new Set(agents.map(agent => agent.threadId)).size !== agents.length) {
        measuresError('DUPLICATE_FACT', `${sessionPath}.agents`, 'runtime agent is duplicated')
      }
      sessions.push(Object.freeze({
        source: source('runtime-session', `runtime_session:${bindingId}`),
        bindingId,
        stageRunId,
        agents: Object.freeze(agents),
        activities: Object.freeze(activityFacts.map((activity, index) => normalizeActivity(
          activity,
          `${sessionPath}.activities[${String(index)}]`,
        ))),
        interactions: Object.freeze(interactionFacts.map((interaction, index) => (
          normalizeInteraction(interaction, `${sessionPath}.interactions[${String(index)}]`)
        ))),
        failures: Object.freeze(failureFacts.map((failure, index) => {
          const failureInput = record(failure, `${sessionPath}.failures[${String(index)}]`)
          return Object.freeze({
            event: eventLink(
              failureInput.event,
              `${sessionPath}.failures[${String(index)}].event`,
            ),
          })
        })),
        recovery: normalizeRecovery(session.recovery, `${sessionPath}.recovery`),
      }))
    })
  })
  return Object.freeze({ sessions: Object.freeze(sessions) })
}

function normalizeModelCall(
  value: unknown,
  path: string,
): DeliveryMeasuresModelCallFact {
  const input = record(value, path)
  const status = stringEnum(
    input.status,
    ['running', 'completed', 'failed'] as const,
    `${path}.status`,
  )
  const startedAtMillis = nullableNonNegativeInteger(
    input.startedAtMillis,
    `${path}.startedAtMillis`,
  )
  const finishedAtMillis = nullableNonNegativeInteger(
    input.finishedAtMillis,
    `${path}.finishedAtMillis`,
  )
  if ((startedAtMillis === null) !== (finishedAtMillis === null)) {
    measuresError(
      'RELATIONSHIP_MISMATCH',
      path,
      'model call timing must provide both timestamps or neither timestamp',
    )
  }
  if (startedAtMillis !== null
    && finishedAtMillis !== null
    && finishedAtMillis < startedAtMillis) {
    measuresError('RELATIONSHIP_MISMATCH', path, 'model call finishes before it starts')
  }
  const tokens = [
    nullableNonNegativeInteger(input.inputTokens, `${path}.inputTokens`),
    nullableNonNegativeInteger(input.outputTokens, `${path}.outputTokens`),
    nullableNonNegativeInteger(input.cacheReadTokens, `${path}.cacheReadTokens`),
    nullableNonNegativeInteger(input.cacheWriteTokens, `${path}.cacheWriteTokens`),
  ] as const
  if (tokens.some(value => value === null) && tokens.some(value => value !== null)) {
    measuresError(
      'RELATIONSHIP_MISMATCH',
      path,
      'model call usage must provide all token counters or no token counters',
    )
  }
  return Object.freeze({
    sourceRef: nonEmptyText(input.sourceRef, `${path}.sourceRef`),
    status,
    startedAtMillis,
    finishedAtMillis,
    inputTokens: tokens[0],
    outputTokens: tokens[1],
    cacheReadTokens: tokens[2],
    cacheWriteTokens: tokens[3],
    costUsdMicros: nullableNonNegativeInteger(input.costUsdMicros, `${path}.costUsdMicros`),
  })
}

function normalizeInput(value: CreateDeliveryMeasuresProjectionInput): NormalizedInput {
  const input = record(value, 'input')
  if (input.schemaVersion !== DELIVERY_MEASURES_SCHEMA_VERSION) {
    measuresError('INVALID_INPUT', 'input.schemaVersion', 'unsupported measures schema version')
  }
  const runKind = stringEnum(
    input.runKind,
    DELIVERY_MEASURES_RUN_KINDS,
    'input.runKind',
  )
  const runState = stringEnum(
    input.runState,
    DELIVERY_MEASURES_RUN_STATES,
    'input.runState',
  )
  const runId = portableRunId(input.runId, 'input.runId')
  const startedAtMillis = nonNegativeInteger(input.startedAtMillis, 'input.startedAtMillis')
  const finishedAtMillis = nullableNonNegativeInteger(
    input.finishedAtMillis,
    'input.finishedAtMillis',
  )
  if (finishedAtMillis !== null && finishedAtMillis < startedAtMillis) {
    measuresError('RELATIONSHIP_MISMATCH', 'input', 'evaluation run finishes before it starts')
  }
  if (runState === 'running' && finishedAtMillis !== null) {
    measuresError('RELATIONSHIP_MISMATCH', 'input.finishedAtMillis', 'running run is unfinished')
  }
  if (runState !== 'running' && finishedAtMillis === null) {
    measuresError('RELATIONSHIP_MISMATCH', 'input.finishedAtMillis', 'terminal run needs a finish time')
  }
  let delivery: Delivery | null
  try {
    delivery = input.delivery === null ? null : parseDelivery(input.delivery)
  } catch (error) {
    return measuresError(
      'INVALID_INPUT',
      'input.delivery',
      `Delivery is invalid: ${error instanceof Error ? error.message : String(error)}`,
    )
  }
  if (!Array.isArray(input.requiredVerificationRoles)
    || input.requiredVerificationRoles.length === 0) {
    measuresError(
      'INVALID_INPUT',
      'input.requiredVerificationRoles',
      'at least one verification role is required',
    )
  }
  const requiredVerificationRoles = input.requiredVerificationRoles.map((role, index) => (
    nonEmptyText(role, `input.requiredVerificationRoles[${String(index)}]`)
  ))
  if (new Set(requiredVerificationRoles).size !== requiredVerificationRoles.length) {
    measuresError(
      'DUPLICATE_FACT',
      'input.requiredVerificationRoles',
      'verification roles must be unique',
    )
  }
  if (!Array.isArray(input.modelCalls)) {
    measuresError('INVALID_INPUT', 'input.modelCalls', 'modelCalls must be an array')
  }
  const modelCalls = input.modelCalls.map((call, index) => normalizeModelCall(
    call,
    `input.modelCalls[${String(index)}]`,
  ))
  if (new Set(modelCalls.map(call => call.sourceRef)).size !== modelCalls.length) {
    measuresError('DUPLICATE_FACT', 'input.modelCalls', 'model call source is duplicated')
  }
  const pricingSource = input.pricingSource === null
    ? null
    : nonEmptyText(input.pricingSource, 'input.pricingSource')
  if (input.historicalVerdicts !== undefined && !Array.isArray(input.historicalVerdicts)) {
    measuresError(
      'INVALID_INPUT',
      'input.historicalVerdicts',
      'historical verdicts must be an array',
    )
  }
  const historicalVerdicts = (input.historicalVerdicts ?? []).map((verdict, index) => {
    try {
      return parseDeliveryVerdict(verdict, `historicalVerdicts[${String(index)}]`)
    } catch (error) {
      return measuresError(
        'INVALID_INPUT',
        `input.historicalVerdicts[${String(index)}]`,
        `historical Verdict is invalid: ${error instanceof Error ? error.message : String(error)}`,
      )
    }
  })
  if (new Set(historicalVerdicts.map(verdict => verdict.id)).size !== historicalVerdicts.length) {
    measuresError('DUPLICATE_FACT', 'input.historicalVerdicts', 'historical Verdict is duplicated')
  }
  if (delivery !== null && historicalVerdicts.some(verdict => (
    verdict.deliveryId !== delivery.id || verdict.deliverySpecId !== delivery.spec.id
  ))) {
    measuresError(
      'RELATIONSHIP_MISMATCH',
      'input.historicalVerdicts',
      'historical Verdict must belong to the measured Delivery specification',
    )
  }
  return Object.freeze({
    runKind,
    runId,
    runState,
    startedAtMillis,
    finishedAtMillis,
    delivery,
    runtime: normalizeRuntime(input.runtimeProjection, delivery),
    requiredVerificationRoles: Object.freeze(requiredVerificationRoles.toSorted()),
    modelCalls: Object.freeze(modelCalls),
    pricingSource,
    historicalVerdicts: Object.freeze(historicalVerdicts),
  })
}

function deliveryCollectionSource(delivery: Delivery, suffix: string): DeliveryMeasureSourceRef {
  return source('delivery', `delivery:${delivery.id}@${String(delivery.revision)}#/${suffix}`)
}

function deliveryRootSource(delivery: Delivery): DeliveryMeasureSourceRef {
  return source('delivery', `delivery:${delivery.id}@${String(delivery.revision)}`)
}

function criterionSource(delivery: Delivery, criterionId: string): DeliveryMeasureSourceRef {
  return source(
    'criterion',
    `delivery:${delivery.id}@${String(delivery.revision)}#/spec/acceptanceCriteria/${criterionId}`,
  )
}

function criterionResultSource(resultId: string): DeliveryMeasureSourceRef {
  return source('criterion-result', `criterion_result:${resultId}`)
}

function verdictSource(verdict: DeliveryVerdict): DeliveryMeasureSourceRef {
  return source('verdict', `delivery_verdict:${verdict.id}`)
}

function evidenceSource(evidenceId: string): DeliveryMeasureSourceRef {
  return source('evidence', `evidence:${evidenceId}`)
}

function stageSource(stageRunId: string): DeliveryMeasureSourceRef {
  return source('stage-run', `stage_run:${stageRunId}`)
}

function attentionSource(attentionId: string): DeliveryMeasureSourceRef {
  return source('attention', `attention:${attentionId}`)
}

function runtimeEventSource(link: DeliveryMeasuresRuntimeEventLink): DeliveryMeasureSourceRef {
  return source('runtime-event', link.sourceRef)
}

function modelCallSource(call: DeliveryMeasuresModelCallFact): DeliveryMeasureSourceRef {
  return source('model-call', call.sourceRef)
}

function runSource(input: NormalizedInput, suffix = ''): DeliveryMeasureSourceRef {
  return source('evaluation-run', `evaluation_run:${input.runId}${suffix}`)
}

function completenessMeasures(
  input: NormalizedInput,
): DeliveryCompletenessMeasures {
  const delivery = input.delivery
  const root = delivery === null ? runSource(input, '#/delivery') : deliveryRootSource(delivery)
  const criterionCollection = delivery === null
    ? root
    : deliveryCollectionSource(delivery, 'spec/acceptanceCriteria')
  const resultCollection = delivery?.verdict === null || delivery === null
    ? root
    : verdictSource(delivery.verdict)
  const criteria = delivery?.spec.acceptanceCriteria ?? []
  const resultByCriterion = new Map(
    delivery?.verdict?.criteria.map(result => [result.criterionId, result]) ?? [],
  )
  const rows = criteria.map((criterion): DeliveryCriterionMeasure => {
    const criterionFact = criterionSource(delivery!, criterion.id)
    const result = resultByCriterion.get(criterion.id)
    const resultFact = result === undefined ? resultCollection : criterionResultSource(result.id)
    const resultVerdict: DeliveryCriterionMeasure['verdict']['value'] = result?.verdict
      ?? 'missing'
    return Object.freeze({
      criterionId: criterion.id,
      required: measured(criterion.required, [criterionFact]),
      verdict: measured(resultVerdict, [criterionFact, resultFact]),
      evidenceCount: measured(result?.evidenceRefs.length ?? 0, [resultFact]),
    })
  })
  const verdicts = rows.map(row => row.verdict.value)
  const requiredRows = rows.filter(row => row.required.value)
  const count = (verdict: DeliveryCriterionMeasure['verdict']['value']) => (
    verdicts.filter(value => value === verdict).length
  )
  let status: DeliveryCompletenessStatus
  if (delivery?.verdict === null || delivery === null) status = 'not-available'
  else if (requiredRows.some(row => row.verdict.value === 'fail')) status = 'failed'
  else if (requiredRows.some(row => row.verdict.value === 'infra_error')) {
    status = 'blocked-by-infrastructure'
  } else if (requiredRows.every(row => row.verdict.value === 'pass')
    && delivery.verdict.status === 'pass') status = 'complete'
  else status = 'incomplete'
  const rowSources = rows.flatMap(row => row.verdict.sourceRefs)
  const countSources = rowSources.length === 0 ? [criterionCollection] : rowSources
  return Object.freeze({
    status: measured(status, [root, resultCollection, ...rowSources]),
    criterionCount: measured(rows.length, [criterionCollection]),
    requiredCriterionCount: measured(requiredRows.length, [criterionCollection]),
    optionalCriterionCount: measured(rows.length - requiredRows.length, [criterionCollection]),
    passCount: measured(count('pass'), countSources),
    failCount: measured(count('fail'), countSources),
    inconclusiveCount: measured(count('inconclusive'), countSources),
    infrastructureErrorCount: measured(count('infra_error'), countSources),
    missingResultCount: measured(count('missing'), countSources),
    requiredPassRate: measured(
      requiredRows.length === 0
        ? null
        : requiredRows.filter(row => row.verdict.value === 'pass').length / requiredRows.length,
      requiredRows.length === 0
        ? [criterionCollection]
        : requiredRows.flatMap(row => row.verdict.sourceRefs),
    ),
    criteria: Object.freeze(rows),
  })
}

function latestRoleStages(delivery: Delivery, role: string): readonly Delivery['stageRuns'][number][] {
  const matching = delivery.stageRuns.filter(stage => (
    stage.role === role && stage.stage === 'verifying' && stage.actorType === 'codex'
  ))
  const latestAttempt = matching.reduce((maximum, stage) => Math.max(maximum, stage.attempt), 0)
  return Object.freeze(matching.filter(stage => stage.attempt === latestAttempt))
}

function confidenceMeasures(
  input: NormalizedInput,
): DeliveryConfidenceMeasures {
  const delivery = input.delivery
  const root = delivery === null ? runSource(input, '#/delivery') : deliveryRootSource(delivery)
  const evidenceCollection = delivery === null
    ? root
    : deliveryCollectionSource(delivery, 'evidence')
  const verdict = delivery?.verdict ?? null
  const verdictFact = verdict === null ? root : verdictSource(verdict)
  const currentCandidateEvidence = delivery === null || verdict === null
    ? []
    : delivery.evidence.filter(reference => (
      reference.deliverySpecId === delivery.spec.id
      && reference.deliverySpecRevision === delivery.spec.revision
      && reference.candidateRef === verdict.candidateRef
    ))
  const evidenceById = new Map(currentCandidateEvidence.map(reference => [reference.id, reference]))
  const referencedIds = new Set(verdict?.criteria.flatMap(result => result.evidenceRefs) ?? [])
  const referencedEvidence = [...referencedIds].flatMap(id => {
    const reference = evidenceById.get(id)
    return reference === undefined ? [] : [reference]
  })
  const missingIds = [...referencedIds].filter(id => !evidenceById.has(id))
  const directEvidence = referencedEvidence.filter(reference => reference.type !== 'review_finding')
  const reviewFindings = currentCandidateEvidence.filter(reference => (
    reference.type === 'review_finding'
  ))
  const roleRows = input.requiredVerificationRoles.map((role): DeliveryVerificationRoleMeasure => {
    const requiredFact = runSource(input, `#/requiredVerificationRoles/${encodeURIComponent(role)}`)
    const stages = delivery === null ? [] : latestRoleStages(delivery, role)
    const stageIds = new Set(stages.map(stage => stage.id))
    const findings = reviewFindings.filter(reference => stageIds.has(reference.stageRunId))
    const stageFacts = stages.map(stage => stageSource(stage.id))
    const findingFacts = findings.map(reference => evidenceSource(reference.id))
    return Object.freeze({
      role,
      required: measured(true, [requiredFact]),
      settled: measured(
        stages.length > 0 && stages.every(stage => stage.status === 'succeeded'),
        [requiredFact, ...(stageFacts.length === 0 ? [root] : stageFacts)],
      ),
      currentFindingCount: measured(
        findings.length,
        [requiredFact, ...(findingFacts.length === 0 ? [evidenceCollection] : findingFacts)],
      ),
    })
  })
  const allCriterionResultsHaveDirectEvidence = verdict !== null && verdict.criteria.every(result => (
    result.evidenceRefs.some(id => evidenceById.get(id)?.type !== 'review_finding'
      && evidenceById.has(id))
  ))
  const independentlySupported = verdict !== null
    && missingIds.length === 0
    && verdict.unresolvedFindings.length === 0
    && allCriterionResultsHaveDirectEvidence
    && roleRows.every(role => role.settled.value && role.currentFindingCount.value > 0)
  const status: DeliveryConfidenceStatus = verdict === null
    ? 'not-evaluated'
    : independentlySupported
      ? 'independently-supported'
      : 'insufficient'
  const evidenceTypeMeasures = Object.fromEntries(EVIDENCE_REF_TYPES.map(type => {
    const matching = referencedEvidence.filter(reference => reference.type === type)
    return [type, measured(
      matching.length,
      matching.length === 0
        ? [evidenceCollection]
        : matching.map(reference => evidenceSource(reference.id)),
    )]
  })) as Readonly<Record<EvidenceRefType, SourcedMeasure<number>>>
  const referencedSources = referencedEvidence.map(reference => evidenceSource(reference.id))
  const currentSources = currentCandidateEvidence.map(reference => evidenceSource(reference.id))
  const roleSources = roleRows.flatMap(role => [
    ...role.settled.sourceRefs,
    ...role.currentFindingCount.sourceRefs,
  ])
  return Object.freeze({
    status: measured(status, [verdictFact, ...roleSources, ...referencedSources]),
    currentCandidateEvidenceCount: measured(
      currentCandidateEvidence.length,
      currentSources.length === 0 ? [evidenceCollection] : currentSources,
    ),
    referencedEvidenceCount: measured(
      referencedEvidence.length,
      referencedSources.length === 0 ? [verdictFact, evidenceCollection] : referencedSources,
    ),
    directEvidenceCount: measured(
      directEvidence.length,
      directEvidence.length === 0
        ? [verdictFact, evidenceCollection]
        : directEvidence.map(reference => evidenceSource(reference.id)),
    ),
    reviewFindingCount: measured(
      reviewFindings.length,
      reviewFindings.length === 0
        ? [evidenceCollection]
        : reviewFindings.map(reference => evidenceSource(reference.id)),
    ),
    missingEvidenceRefCount: measured(missingIds.length, [verdictFact, evidenceCollection]),
    unresolvedFindingCount: measured(verdict?.unresolvedFindings.length ?? 0, [verdictFact]),
    requiredRoleCount: measured(roleRows.length, [runSource(input, '#/requiredVerificationRoles')]),
    settledRoleCount: measured(
      roleRows.filter(role => role.settled.value).length,
      roleSources,
    ),
    rolesWithCurrentFindingsCount: measured(
      roleRows.filter(role => role.currentFindingCount.value > 0).length,
      roleSources,
    ),
    evidenceByType: Object.freeze(evidenceTypeMeasures),
    roles: Object.freeze(roleRows),
  })
}

function eventSourcesOr(
  values: readonly DeliveryMeasureSourceRef[],
  fallback: DeliveryMeasureSourceRef,
): readonly DeliveryMeasureSourceRef[] {
  return values.length === 0 ? [fallback] : values
}

function stabilityMeasures(input: NormalizedInput): DeliveryStabilityMeasures {
  const delivery = input.delivery
  const root = delivery === null ? runSource(input, '#/delivery') : deliveryRootSource(delivery)
  const stageCollection = delivery === null
    ? root
    : deliveryCollectionSource(delivery, 'stageRuns')
  const runtimeRoot = runSource(input, '#/runtimeProjection')
  const stages = delivery?.stageRuns ?? []
  const sessions = input.runtime?.sessions ?? []
  const activities = sessions.flatMap(session => session.activities)
  const agents = sessions.flatMap(session => session.agents)
  const activitySources = (outcome: DeliveryMeasuresRuntimeActivity['outcome']) => activities
    .filter(activity => activity.outcome === outcome)
    .map(activity => runtimeEventSource(activity.latestEvent))
  const agentSources = (status: string) => agents
    .filter(agent => agent.status === status)
    .map(agent => runtimeEventSource(agent.latestEvent))
  const failedStages = stages.filter(stage => stage.status === 'failed')
  const cancelledStages = stages.filter(stage => stage.status === 'cancelled')
  const retriedStages = stages.filter(stage => stage.attempt > 1)
  const reworkStages = stages.filter(stage => stage.stage === 'reworking')
  const runtimeFailures = sessions.flatMap(session => session.failures)
  const recoveryCount = safeSum(
    sessions.map(session => session.recovery.recoveryCount),
    'runtimeProjection.recoveryCount',
  )
  const recoveryFailureCount = safeSum(
    sessions.map(session => session.recovery.failureCount),
    'runtimeProjection.failureCount',
  )
  const recoverySources = sessions.flatMap(session => (
    session.recovery.latestRecoveryEvent === null
      ? []
      : [runtimeEventSource(session.recovery.latestRecoveryEvent)]
  ))
  const recoveryFailureSources = sessions.flatMap(session => (
    session.recovery.lastFailureEvent === null
      ? []
      : [runtimeEventSource(session.recovery.lastFailureEvent)]
  ))
  const historicalNonPass = input.historicalVerdicts.filter(verdict => verdict.status !== 'pass')
  const taskFailures = activitySources('task-failed')
  const timeouts = activitySources('timed-out')
  const policyDenials = activitySources('policy-denied')
  const infrastructureFailures = activitySources('infrastructure-failed')
  const cancelledActivities = activitySources('cancelled')
  const failedAgents = agentSources('failed')
  const interruptedAgents = agentSources('interrupted')
  const infrastructureAffected = timeouts.length > 0
    || policyDenials.length > 0
    || infrastructureFailures.length > 0
    || cancelledActivities.length > 0
    || input.historicalVerdicts.some(verdict => verdict.status === 'infra_error')
  const instabilityPresent = failedStages.length > 0
    || cancelledStages.length > 0
    || retriedStages.length > 0
    || reworkStages.length > 0
    || taskFailures.length > 0
    || runtimeFailures.length > 0
    || recoveryCount > 0
    || failedAgents.length > 0
    || interruptedAgents.length > 0
    || historicalNonPass.length > 0
  const success = delivery?.status === 'delivered' || delivery?.status === 'ready-to-deliver'
  let status: DeliveryStabilityStatus
  if (input.runState === 'interrupted' || input.runState === 'budget-exceeded') {
    status = 'interrupted'
  }
  else if (infrastructureAffected) status = 'infrastructure-affected'
  else if (success && instabilityPresent) status = 'reworked'
  else if (instabilityPresent || input.runState === 'failed') status = 'failed'
  else status = 'stable'
  const statusSources = [
    runSource(input),
    root,
    ...failedStages.map(stage => stageSource(stage.id)),
    ...cancelledStages.map(stage => stageSource(stage.id)),
    ...retriedStages.map(stage => stageSource(stage.id)),
    ...reworkStages.map(stage => stageSource(stage.id)),
    ...taskFailures,
    ...timeouts,
    ...policyDenials,
    ...infrastructureFailures,
    ...historicalNonPass.map(verdictSource),
  ]
  return Object.freeze({
    status: measured(status, statusSources),
    failedStageCount: measured(
      failedStages.length,
      failedStages.length === 0 ? [stageCollection] : failedStages.map(stage => stageSource(stage.id)),
    ),
    cancelledStageCount: measured(
      cancelledStages.length,
      cancelledStages.length === 0
        ? [stageCollection]
        : cancelledStages.map(stage => stageSource(stage.id)),
    ),
    retriedStageCount: measured(
      retriedStages.length,
      retriedStages.length === 0
        ? [stageCollection]
        : retriedStages.map(stage => stageSource(stage.id)),
    ),
    reworkStageCount: measured(
      reworkStages.length,
      reworkStages.length === 0 ? [stageCollection] : reworkStages.map(stage => stageSource(stage.id)),
    ),
    taskFailureCount: measured(taskFailures.length, eventSourcesOr(taskFailures, runtimeRoot)),
    timeoutCount: measured(timeouts.length, eventSourcesOr(timeouts, runtimeRoot)),
    policyDenialCount: measured(
      policyDenials.length,
      eventSourcesOr(policyDenials, runtimeRoot),
    ),
    infrastructureFailureCount: measured(
      infrastructureFailures.length,
      eventSourcesOr(infrastructureFailures, runtimeRoot),
    ),
    cancelledActivityCount: measured(
      cancelledActivities.length,
      eventSourcesOr(cancelledActivities, runtimeRoot),
    ),
    runtimeFailureEventCount: measured(
      runtimeFailures.length,
      runtimeFailures.length === 0
        ? [runtimeRoot]
        : runtimeFailures.map(failure => runtimeEventSource(failure.event)),
    ),
    reportedRecoveryFailureCount: measured(
      recoveryFailureCount,
      eventSourcesOr(recoveryFailureSources, runtimeRoot),
    ),
    recoveryEventCount: measured(
      recoveryCount,
      eventSourcesOr(recoverySources, runtimeRoot),
    ),
    failedAgentCount: measured(
      failedAgents.length,
      eventSourcesOr(failedAgents, runtimeRoot),
    ),
    interruptedAgentCount: measured(
      interruptedAgents.length,
      eventSourcesOr(interruptedAgents, runtimeRoot),
    ),
    historicalNonPassVerdictCount: measured(
      historicalNonPass.length,
      historicalNonPass.length === 0
        ? [runSource(input, '#/historicalVerdicts')]
        : historicalNonPass.map(verdictSource),
    ),
  })
}

function humanDependenceMeasures(
  input: NormalizedInput,
): DeliveryHumanDependenceMeasures {
  const delivery = input.delivery
  const root = delivery === null ? runSource(input, '#/delivery') : deliveryRootSource(delivery)
  const stageCollection = delivery === null
    ? root
    : deliveryCollectionSource(delivery, 'stageRuns')
  const attentionCollection = delivery === null
    ? root
    : deliveryCollectionSource(delivery, 'attentionItems')
  const runtimeRoot = runSource(input, '#/runtimeProjection')
  const stages = delivery?.stageRuns ?? []
  const attentions = delivery?.attentionItems ?? []
  const interactions = input.runtime?.sessions.flatMap(session => session.interactions) ?? []
  const humanStages = stages.filter(stage => stage.actorType === 'human')
  const codexStages = stages.filter(stage => stage.actorType === 'codex')
  const openAttentions = attentions.filter(item => item.status === 'open')
  const resolvedAttentions = attentions.filter(item => item.status === 'resolved')
  const dismissedAttentions = attentions.filter(item => item.status === 'dismissed')
  const blockingAttentions = attentions.filter(item => item.blocking)
  const openBlockingAttentions = attentions.filter(item => item.blocking && item.status === 'open')
  const approvals = interactions.filter(item => item.interactionType === 'execution-approval')
  const userInputs = interactions.filter(item => item.interactionType === 'user-input')
  const pending = interactions.filter(item => item.status === 'pending')
  const pendingBlocking = pending.filter(item => item.blocking)
  const substantiveAttention = attentions.filter(item => [
    'requirement_question',
    'verification_blocked',
    'scope_change',
  ].includes(item.type))
  let status: DeliveryHumanDependenceStatus
  if (openBlockingAttentions.length > 0 || pendingBlocking.length > 0) status = 'blocked'
  else if (openAttentions.length > 0
    || pending.length > 0
    || approvals.length > 0
    || userInputs.length > 0
    || substantiveAttention.length > 0) status = 'intervention-required'
  else if (humanStages.length > 0 || attentions.length > 0) status = 'review-only'
  else status = 'none'
  const attentionFacts = attentions.map(item => attentionSource(item.id))
  const interactionFacts = interactions.map(item => runtimeEventSource(item.requestedEvent))
  const interactionSources = (
    values: readonly DeliveryMeasuresRuntimeInteraction[],
    resolved = false,
  ) => eventSourcesOr(values.flatMap(value => {
    const link = resolved ? value.resolvedEvent : value.requestedEvent
    return link === null ? [] : [runtimeEventSource(link)]
  }), runtimeRoot)
  const attentionSources = (
    values: typeof attentions,
  ) => values.length === 0 ? [attentionCollection] : values.map(item => attentionSource(item.id))
  const byType = Object.fromEntries(ATTENTION_ITEM_TYPES.map(type => {
    const matching = attentions.filter(item => item.type === type)
    return [type, measured(matching.length, attentionSources(matching))]
  })) as Readonly<Record<AttentionItemType, SourcedMeasure<number>>>
  return Object.freeze({
    status: measured(status, [root, ...attentionFacts, ...interactionFacts]),
    humanStageCount: measured(
      humanStages.length,
      humanStages.length === 0 ? [stageCollection] : humanStages.map(stage => stageSource(stage.id)),
    ),
    codexStageCount: measured(
      codexStages.length,
      codexStages.length === 0 ? [stageCollection] : codexStages.map(stage => stageSource(stage.id)),
    ),
    attentionCount: measured(attentions.length, attentionSources(attentions)),
    openAttentionCount: measured(openAttentions.length, attentionSources(openAttentions)),
    resolvedAttentionCount: measured(
      resolvedAttentions.length,
      attentionSources(resolvedAttentions),
    ),
    dismissedAttentionCount: measured(
      dismissedAttentions.length,
      attentionSources(dismissedAttentions),
    ),
    blockingAttentionCount: measured(
      blockingAttentions.length,
      attentionSources(blockingAttentions),
    ),
    openBlockingAttentionCount: measured(
      openBlockingAttentions.length,
      attentionSources(openBlockingAttentions),
    ),
    attentionByType: Object.freeze(byType),
    executionApprovalRequestCount: measured(
      approvals.length,
      interactionSources(approvals),
    ),
    executionApprovalResolutionCount: measured(
      approvals.filter(item => item.status === 'resolved').length,
      interactionSources(approvals.filter(item => item.status === 'resolved'), true),
    ),
    userInputRequestCount: measured(userInputs.length, interactionSources(userInputs)),
    userInputResolutionCount: measured(
      userInputs.filter(item => item.status === 'resolved').length,
      interactionSources(userInputs.filter(item => item.status === 'resolved'), true),
    ),
    pendingInteractionCount: measured(pending.length, interactionSources(pending)),
  })
}

interface ParallelismObservation {
  readonly available: boolean
  readonly maximum: number | null
  readonly sources: readonly DeliveryMeasureSourceRef[]
}

function parallelismObservation(
  input: NormalizedInput,
): ParallelismObservation {
  if (input.runtime === null) {
    return Object.freeze({
      available: false,
      maximum: null,
      sources: Object.freeze([runSource(input, '#/runtimeProjection')]),
    })
  }
  let maximum = 0
  let maximumSources: readonly DeliveryMeasureSourceRef[] = []
  for (const session of input.runtime.sessions) {
    const points = session.agents.flatMap((agent, index) => {
      const start = BigInt(agent.firstEvent.sequence)
      const finish = BigInt(agent.latestEvent.sequence)
      if (finish < start) {
        measuresError(
          'RELATIONSHIP_MISMATCH',
          'runtimeProjection.agents',
          'agent lifecycle finishes before it starts',
        )
      }
      return [
        { sequence: start, direction: 1, index, agent },
        { sequence: finish, direction: -1, index, agent },
      ]
    }).toSorted((left, right) => (
      left.sequence < right.sequence
        ? -1
        : left.sequence > right.sequence
          ? 1
          : right.direction - left.direction || left.index - right.index
    ))
    const active = new Set<DeliveryMeasuresRuntimeAgent>()
    for (const point of points) {
      if (point.direction === 1) {
        active.add(point.agent)
        if (active.size > maximum) {
          maximum = active.size
          maximumSources = sources([
            session.source,
            ...[...active].flatMap(agent => [
              runtimeEventSource(agent.firstEvent),
              runtimeEventSource(agent.latestEvent),
            ]),
          ])
        }
      } else {
        active.delete(point.agent)
      }
    }
  }
  return Object.freeze({
    available: true,
    maximum,
    sources: maximumSources.length === 0
      ? Object.freeze([runSource(input, '#/runtimeProjection')])
      : maximumSources,
  })
}

function efficiencyMeasures(input: NormalizedInput): DeliveryEfficiencyMeasures {
  const delivery = input.delivery
  const root = delivery === null ? runSource(input, '#/delivery') : deliveryRootSource(delivery)
  const stageCollection = delivery === null
    ? root
    : deliveryCollectionSource(delivery, 'stageRuns')
  const stages = delivery?.stageRuns ?? []
  const settledStages = stages.filter(stage => stage.finishedAtMillis !== null)
  const unfinishedStages = stages.filter(stage => stage.finishedAtMillis === null)
  const settledStageMillis = safeSum(settledStages.map(stage => (
    stage.finishedAtMillis! - stage.startedAtMillis
  )), 'delivery.stageRuns.elapsedMillis')
  const calls = input.modelCalls
  const callCollection = runSource(input, '#/modelCalls')
  const completedCalls = calls.filter(call => call.status === 'completed')
  const failedCalls = calls.filter(call => call.status === 'failed')
  const runningCalls = calls.filter(call => call.status === 'running')
  const callsWithUsage = calls.filter(call => call.inputTokens !== null)
  const callsWithoutUsage = calls.filter(call => call.inputTokens === null)
  const callsWithCost = calls.filter(call => call.costUsdMicros !== null)
  const callsWithoutCost = calls.filter(call => call.costUsdMicros === null)
  const callsWithTiming = calls.filter(call => call.startedAtMillis !== null)
  const callsWithoutTiming = calls.filter(call => call.startedAtMillis === null)
  const callSources = (values: readonly DeliveryMeasuresModelCallFact[]) => (
    values.length === 0 ? [callCollection] : values.map(modelCallSource)
  )
  const tokenTotal = (key: 'inputTokens' | 'outputTokens' | 'cacheReadTokens' | 'cacheWriteTokens') => (
    safeSum(callsWithUsage.map(call => call[key]!), `modelCalls.${key}`)
  )
  const inputTokens = tokenTotal('inputTokens')
  const outputTokens = tokenTotal('outputTokens')
  const cacheReadTokens = tokenTotal('cacheReadTokens')
  const cacheWriteTokens = tokenTotal('cacheWriteTokens')
  const modelElapsedMillis = callsWithoutTiming.length === 0
    ? safeSum(callsWithTiming.map(call => (
      call.finishedAtMillis! - call.startedAtMillis!
    )), 'modelCalls.elapsedMillis')
    : null
  const runtime = input.runtime
  const agentCount = runtime?.sessions.reduce((total, session) => total + session.agents.length, 0) ?? 0
  const subagentCount = runtime?.sessions.reduce((total, session) => (
    total + session.agents.filter(agent => agent.parentThreadId !== null).length
  ), 0) ?? 0
  const agentSources = runtime?.sessions.flatMap(session => session.agents.flatMap(agent => [
    runtimeEventSource(agent.firstEvent),
    runtimeEventSource(agent.latestEvent),
  ])) ?? []
  const runtimeRoot = runSource(input, '#/runtimeProjection')
  const parallelism = parallelismObservation(input)
  const pricingFact = input.pricingSource === null
    ? runSource(input, '#/pricingSource')
    : source('pricing', input.pricingSource)
  return Object.freeze({
    runElapsedMillis: measured(
      input.finishedAtMillis === null ? null : input.finishedAtMillis - input.startedAtMillis,
      [runSource(input)],
    ),
    settledStageMillis: measured(
      settledStageMillis,
      settledStages.length === 0
        ? [stageCollection]
        : settledStages.map(stage => stageSource(stage.id)),
    ),
    unfinishedStageCount: measured(
      unfinishedStages.length,
      unfinishedStages.length === 0
        ? [stageCollection]
        : unfinishedStages.map(stage => stageSource(stage.id)),
    ),
    stageCount: measured(stages.length, [stageCollection]),
    modelCallCount: measured(calls.length, [callCollection]),
    completedModelCallCount: measured(completedCalls.length, callSources(completedCalls)),
    failedModelCallCount: measured(failedCalls.length, callSources(failedCalls)),
    runningModelCallCount: measured(runningCalls.length, callSources(runningCalls)),
    missingUsageCallCount: measured(callsWithoutUsage.length, callSources(callsWithoutUsage)),
    inputTokens: measured(inputTokens, callSources(callsWithUsage)),
    outputTokens: measured(outputTokens, callSources(callsWithUsage)),
    cacheReadTokens: measured(cacheReadTokens, callSources(callsWithUsage)),
    cacheWriteTokens: measured(cacheWriteTokens, callSources(callsWithUsage)),
    totalTokens: measured(
      safeSum(
        [inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens],
        'modelCalls.totalTokens',
      ),
      callSources(callsWithUsage),
    ),
    costUsdMicros: measured(
      safeSum(callsWithCost.map(call => call.costUsdMicros!), 'modelCalls.costUsdMicros'),
      callSources(callsWithCost),
    ),
    missingCostCallCount: measured(callsWithoutCost.length, callSources(callsWithoutCost)),
    pricingSource: measured(input.pricingSource, [pricingFact]),
    modelElapsedMillis: measured(modelElapsedMillis, callSources(calls)),
    missingModelTimingCallCount: measured(
      callsWithoutTiming.length,
      callSources(callsWithoutTiming),
    ),
    agentCount: measured(
      agentCount,
      agentSources.length === 0 ? [runtimeRoot] : agentSources,
    ),
    subagentCount: measured(
      subagentCount,
      agentSources.length === 0 ? [runtimeRoot] : agentSources,
    ),
    parallelismObservationAvailable: measured(parallelism.available, parallelism.sources),
    maxConcurrentAgents: measured(parallelism.maximum, parallelism.sources),
    parallelExecutionObserved: measured(
      parallelism.maximum !== null && parallelism.maximum >= 2,
      parallelism.sources,
    ),
  })
}

function outcomeMeasures(
  input: NormalizedInput,
  completeness: DeliveryCompletenessMeasures,
  confidence: DeliveryConfidenceMeasures,
): DeliveryOutcomeMeasures {
  const delivery = input.delivery
  const root = delivery === null ? runSource(input, '#/delivery') : deliveryRootSource(delivery)
  const successClaim = input.runState === 'completed'
    || delivery?.status === 'ready-to-deliver'
    || delivery?.status === 'delivered'
  const completionProof = completeness.status.value === 'complete'
    && confidence.status.value === 'independently-supported'
  const falseSuccessRisk = successClaim && !completionProof
  const falseFailureRisk = !successClaim && completionProof
  const classification: DeliveryOutcomeClassification = completionProof
    ? successClaim ? 'proven-success' : 'proof-not-claimed'
    : successClaim ? 'claimed-without-proof' : 'no-success-claim'
  const claimSources = [runSource(input), root]
  const proofSources = [
    ...completeness.status.sourceRefs,
    ...confidence.status.sourceRefs,
  ]
  return Object.freeze({
    successClaimPresent: measured(successClaim, claimSources),
    completionProofPresent: measured(completionProof, proofSources),
    falseSuccessRisk: measured(falseSuccessRisk, [...claimSources, ...proofSources]),
    falseFailureRisk: measured(falseFailureRisk, [...claimSources, ...proofSources]),
    classification: measured(classification, [...claimSources, ...proofSources]),
  })
}

/**
 * Derive explainable Delivery measures from canonical Delivery and runtime facts.
 * The projection carries no scheduling authority and deliberately has no total score.
 */
export function createDeliveryMeasuresProjection(
  value: CreateDeliveryMeasuresProjectionInput,
): DeliveryMeasuresProjection {
  const input = normalizeInput(value)
  const completeness = completenessMeasures(input)
  const confidence = confidenceMeasures(input)
  const stability = stabilityMeasures(input)
  const humanDependence = humanDependenceMeasures(input)
  const efficiency = efficiencyMeasures(input)
  return immutable({
    schemaVersion: DELIVERY_MEASURES_SCHEMA_VERSION,
    runKind: input.runKind,
    runId: input.runId,
    runState: input.runState,
    deliveryId: input.delivery?.id ?? null,
    deliveryRevision: input.delivery?.revision ?? null,
    outcome: outcomeMeasures(input, completeness, confidence),
    dimensions: {
      completeness,
      confidence,
      stability,
      humanDependence,
      efficiency,
    },
  })
}

/** Keep scripted and paid/live evidence in separate report lanes. */
export function groupDeliveryMeasuresByRunKind(
  reports: readonly DeliveryMeasuresProjection[],
): GroupedDeliveryMeasures {
  if (!Array.isArray(reports)) {
    measuresError('INVALID_INPUT', 'reports', 'reports must be an array')
  }
  const identities = new Set<string>()
  const deterministic: DeliveryMeasuresProjection[] = []
  const live: DeliveryMeasuresProjection[] = []
  reports.forEach((report, index) => {
    if (!isRecord(report)
      || report.schemaVersion !== DELIVERY_MEASURES_SCHEMA_VERSION
      || !DELIVERY_MEASURES_RUN_KINDS.includes(report.runKind as DeliveryMeasuresRunKind)
      || typeof report.runId !== 'string') {
      measuresError(
        'INVALID_INPUT',
        `reports[${String(index)}]`,
        'report is not a Delivery measures projection',
      )
    }
    const projection = report as unknown as DeliveryMeasuresProjection
    const identity = `${projection.runKind}\u0000${projection.runId}`
    if (identities.has(identity)) {
      measuresError('DUPLICATE_FACT', `reports[${String(index)}]`, 'report run is duplicated')
    }
    identities.add(identity)
    if (projection.runKind === 'deterministic') deterministic.push(projection)
    else live.push(projection)
  })
  return immutable({
    deterministic: deterministic.toSorted((left, right) => left.runId.localeCompare(right.runId)),
    live: live.toSorted((left, right) => left.runId.localeCompare(right.runId)),
  })
}
