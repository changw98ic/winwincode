import {
  DeliveryId,
  SessionBindingId,
  StageRunId,
  type DeliveryId as DeliveryIdentifier,
  type EvidenceRefType,
  type SessionBindingId as SessionBindingIdentifier,
  type StageRunId as StageRunIdentifier,
} from './delivery.js'
import {
  RUNTIME_EVENT_KINDS,
  type RuntimeEventKind,
  type RuntimePlanItemStatus,
} from './runtime-events.js'

/** Bounded, rebuildable Codex activity shown by the StrongFlow workbench. */
export const STRONGFLOW_RUNTIME_EXECUTION_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_RUNTIME_EXECUTION_PROTOCOL =
  'winwincode.runtime-execution-projection.v1' as const

export const STRONGFLOW_RUNTIME_EXECUTION_LIMITS = Object.freeze({
  sessions: 256,
  planItems: 200,
  agents: 256,
  agentEdges: 512,
  activities: 100,
  interactions: 100,
  questions: 20,
  failures: 50,
  evidence: 100,
  usageMetrics: 64,
} as const)

export const STRONGFLOW_RUNTIME_AGENT_STATUSES = Object.freeze([
  'unknown',
  'waiting',
  'running',
  'completed',
  'interrupted',
  'failed',
  'closed',
] as const)

export type StrongFlowRuntimeAgentStatus =
  typeof STRONGFLOW_RUNTIME_AGENT_STATUSES[number]

export const STRONGFLOW_RUNTIME_ACTIVITY_STATUSES = Object.freeze([
  'running',
  'completed',
  'failed',
  'declined',
  'cancelled',
  'unknown',
] as const)

export type StrongFlowRuntimeActivityStatus =
  typeof STRONGFLOW_RUNTIME_ACTIVITY_STATUSES[number]

export const STRONGFLOW_RUNTIME_EVIDENCE_OUTCOMES = Object.freeze([
  'observed',
  'succeeded',
  'task-failed',
  'timed-out',
  'policy-denied',
  'infrastructure-failed',
  'cancelled',
] as const)

export type StrongFlowRuntimeEvidenceOutcome =
  typeof STRONGFLOW_RUNTIME_EVIDENCE_OUTCOMES[number]

export const STRONGFLOW_RUNTIME_RECOVERY_STATES = Object.freeze([
  'none',
  'required',
  'in-progress',
  'recovered',
] as const)

export type StrongFlowRuntimeRecoveryState =
  typeof STRONGFLOW_RUNTIME_RECOVERY_STATES[number]

export interface StrongFlowRuntimeEventReference {
  readonly eventId: string
  readonly sourceRef: string
  readonly sequence: string
  readonly kind: RuntimeEventKind
}

export interface StrongFlowRuntimePlanItem {
  readonly step: string
  readonly status: RuntimePlanItemStatus
}

export interface StrongFlowRuntimePlan {
  readonly itemId: string | null
  readonly explanation: string | null
  readonly items: readonly StrongFlowRuntimePlanItem[]
  readonly text: string | null
  readonly complete: boolean
  readonly latestEvent: StrongFlowRuntimeEventReference
}

export interface StrongFlowRuntimeAgent {
  readonly threadId: string
  readonly path: string | null
  readonly parentThreadId: string | null
  readonly nickname: string | null
  readonly role: string | null
  readonly status: StrongFlowRuntimeAgentStatus
  readonly latestEvent: StrongFlowRuntimeEventReference
}

export interface StrongFlowRuntimeAgentEdge {
  readonly parentThreadId: string
  readonly childThreadId: string
}

export interface StrongFlowRuntimeActivity {
  readonly callId: string
  readonly activityType: 'command' | 'test'
  readonly command: string | null
  readonly status: StrongFlowRuntimeActivityStatus
  readonly outcome: StrongFlowRuntimeEvidenceOutcome
  readonly exitCode: number | null
  readonly latestEvent: StrongFlowRuntimeEventReference
}

export interface StrongFlowRuntimeQuestion {
  readonly id: string
  readonly header: string
  readonly question: string
  readonly isSecret: boolean
}

export interface StrongFlowRuntimeInteraction {
  readonly id: string
  readonly interactionType: 'execution-approval' | 'user-input'
  readonly blocking: boolean
  readonly status: 'pending' | 'resolved'
  readonly questions: readonly StrongFlowRuntimeQuestion[]
  readonly requestedEvent: StrongFlowRuntimeEventReference
  readonly resolvedEvent: StrongFlowRuntimeEventReference | null
}

export interface StrongFlowRuntimeFailure {
  readonly message: string
  readonly code: string | null
  readonly event: StrongFlowRuntimeEventReference
}

export interface StrongFlowRuntimeRecovery {
  readonly state: StrongFlowRuntimeRecoveryState
  readonly failureCount: number
  readonly recoveryCount: number
  readonly lastFailureEvent: StrongFlowRuntimeEventReference | null
  readonly latestRecoveryEvent: StrongFlowRuntimeEventReference | null
}

export interface StrongFlowRuntimeDiffSummary {
  readonly changedFileCount: number
  readonly additions: number
  readonly deletions: number
  /** Raw paths and hunks stay outside the live runtime view. */
  readonly detailsVisible: false
  readonly event: StrongFlowRuntimeEventReference
}

export interface StrongFlowRuntimeUsage {
  readonly totals: Readonly<Record<string, number>>
  readonly event: StrongFlowRuntimeEventReference
}

export interface StrongFlowRuntimeEvidence {
  readonly type: Exclude<EvidenceRefType, 'pull_request'>
  readonly outcome: StrongFlowRuntimeEvidenceOutcome
  readonly sourceRef: string
  readonly eventId: string
}

export interface StrongFlowRuntimeSessionProjection {
  readonly stageRunId: StageRunIdentifier
  readonly sessionBindingId: SessionBindingIdentifier
  readonly dshSessionId: string | null
  readonly codexSessionId: string | null
  readonly asOfSequence: string
  readonly plan: StrongFlowRuntimePlan | null
  readonly agents: readonly StrongFlowRuntimeAgent[]
  readonly agentEdges: readonly StrongFlowRuntimeAgentEdge[]
  readonly activities: readonly StrongFlowRuntimeActivity[]
  readonly interactions: readonly StrongFlowRuntimeInteraction[]
  readonly failures: readonly StrongFlowRuntimeFailure[]
  readonly recovery: StrongFlowRuntimeRecovery
  readonly diffSummary: StrongFlowRuntimeDiffSummary | null
  readonly usage: StrongFlowRuntimeUsage | null
  readonly evidence: readonly StrongFlowRuntimeEvidence[]
}

export interface StrongFlowRuntimeExecutionProjection {
  readonly schemaVersion: typeof STRONGFLOW_RUNTIME_EXECUTION_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_RUNTIME_EXECUTION_PROTOCOL
  readonly deliveryId: DeliveryIdentifier
  readonly deliveryRevision: number
  readonly sessions: readonly StrongFlowRuntimeSessionProjection[]
}

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u
const SEQUENCE_PATTERN = /^(?:0|[1-9][0-9]{0,39})$/u
const USAGE_KEY_PATTERN = /^[A-Za-z][A-Za-z0-9._:-]{0,99}$/u
const MAX_TEXT_LENGTH = 65_536

function failure(path: string, message: string): never {
  throw new TypeError(`${path} ${message}`)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) return failure(path, 'must be an object')
  return value
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  keys: readonly string[],
  path: string,
): void {
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    failure(path, 'has an unexpected shape')
  }
}

function boundedArray(
  value: unknown,
  limit: number,
  path: string,
): readonly unknown[] {
  if (!Array.isArray(value) || value.length > limit) {
    return failure(path, `must be an array with at most ${String(limit)} entries`)
  }
  return value
}

function text(value: unknown, path: string, allowEmpty = false): string {
  if (typeof value !== 'string'
    || value.length > MAX_TEXT_LENGTH
    || (!allowEmpty && value.trim().length === 0)
    || /[\u0000\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    return failure(path, 'must be bounded text')
  }
  return value
}

function nullableText(value: unknown, path: string): string | null {
  return value === null ? null : text(value, path)
}

function portableId(value: unknown, path: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    return failure(path, 'must be a portable identifier')
  }
  return value
}

function nullablePortableId(value: unknown, path: string): string | null {
  return value === null ? null : portableId(value, path)
}

function identifier<Identifier>(
  value: unknown,
  path: string,
  factory: (input: string) => Identifier,
): Identifier {
  try {
    return factory(portableId(value, path))
  } catch {
    return failure(path, 'is invalid')
  }
}

function sequence(value: unknown, path: string): string {
  if (typeof value !== 'string' || !SEQUENCE_PATTERN.test(value)) {
    return failure(path, 'must be a canonical non-negative sequence')
  }
  return value
}

function nonNegativeInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    return failure(path, 'must be a non-negative safe integer')
  }
  return Number(value)
}

function integerOrNull(value: unknown, path: string): number | null {
  if (value === null) return null
  if (!Number.isSafeInteger(value) || Object.is(value, -0)) {
    return failure(path, 'must be an integer or null')
  }
  return Number(value)
}

function unique(values: readonly string[], path: string): void {
  if (new Set(values).size !== values.length) failure(path, 'must not contain duplicates')
}

function member<Value extends string>(
  value: unknown,
  values: readonly Value[],
  path: string,
): Value {
  if (typeof value !== 'string' || !values.includes(value as Value)) {
    return failure(path, 'is unsupported')
  }
  return value as Value
}

function parseEventReference(
  value: unknown,
  path: string,
): StrongFlowRuntimeEventReference {
  const input = record(value, path)
  exactKeys(input, ['eventId', 'sourceRef', 'sequence', 'kind'], path)
  const eventId = portableId(input.eventId, `${path}.eventId`)
  const sourceRef = portableId(input.sourceRef, `${path}.sourceRef`)
  if (sourceRef !== `runtime_event:${eventId}`) {
    failure(path, 'eventId and sourceRef do not agree')
  }
  return Object.freeze({
    eventId,
    sourceRef,
    sequence: sequence(input.sequence, `${path}.sequence`),
    kind: member(input.kind, RUNTIME_EVENT_KINDS, `${path}.kind`),
  })
}

function parsePlan(value: unknown, path: string): StrongFlowRuntimePlan {
  const input = record(value, path)
  exactKeys(input, ['itemId', 'explanation', 'items', 'text', 'complete', 'latestEvent'], path)
  if (typeof input.complete !== 'boolean') failure(`${path}.complete`, 'must be a boolean')
  const items = boundedArray(
    input.items,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.planItems,
    `${path}.items`,
  ).map((entry, index) => {
    const itemPath = `${path}.items[${String(index)}]`
    const item = record(entry, itemPath)
    exactKeys(item, ['step', 'status'], itemPath)
    return Object.freeze({
      step: text(item.step, `${itemPath}.step`),
      status: member(
        item.status,
        ['pending', 'in_progress', 'completed'] as const,
        `${itemPath}.status`,
      ),
    })
  })
  return Object.freeze({
    itemId: nullablePortableId(input.itemId, `${path}.itemId`),
    explanation: nullableText(input.explanation, `${path}.explanation`),
    items: Object.freeze(items),
    text: nullableText(input.text, `${path}.text`),
    complete: input.complete,
    latestEvent: parseEventReference(input.latestEvent, `${path}.latestEvent`),
  })
}

function parseAgent(value: unknown, path: string): StrongFlowRuntimeAgent {
  const input = record(value, path)
  exactKeys(input, [
    'threadId',
    'path',
    'parentThreadId',
    'nickname',
    'role',
    'status',
    'latestEvent',
  ], path)
  return Object.freeze({
    threadId: portableId(input.threadId, `${path}.threadId`),
    path: nullableText(input.path, `${path}.path`),
    parentThreadId: nullablePortableId(input.parentThreadId, `${path}.parentThreadId`),
    nickname: nullableText(input.nickname, `${path}.nickname`),
    role: nullableText(input.role, `${path}.role`),
    status: member(
      input.status,
      STRONGFLOW_RUNTIME_AGENT_STATUSES,
      `${path}.status`,
    ),
    latestEvent: parseEventReference(input.latestEvent, `${path}.latestEvent`),
  })
}

function parseActivity(value: unknown, path: string): StrongFlowRuntimeActivity {
  const input = record(value, path)
  exactKeys(input, [
    'callId',
    'activityType',
    'command',
    'status',
    'outcome',
    'exitCode',
    'latestEvent',
  ], path)
  return Object.freeze({
    callId: portableId(input.callId, `${path}.callId`),
    activityType: member(
      input.activityType,
      ['command', 'test'] as const,
      `${path}.activityType`,
    ),
    command: nullableText(input.command, `${path}.command`),
    status: member(
      input.status,
      STRONGFLOW_RUNTIME_ACTIVITY_STATUSES,
      `${path}.status`,
    ),
    outcome: member(
      input.outcome,
      STRONGFLOW_RUNTIME_EVIDENCE_OUTCOMES,
      `${path}.outcome`,
    ),
    exitCode: integerOrNull(input.exitCode, `${path}.exitCode`),
    latestEvent: parseEventReference(input.latestEvent, `${path}.latestEvent`),
  })
}

function parseQuestion(value: unknown, path: string): StrongFlowRuntimeQuestion {
  const input = record(value, path)
  exactKeys(input, ['id', 'header', 'question', 'isSecret'], path)
  if (typeof input.isSecret !== 'boolean') failure(`${path}.isSecret`, 'must be a boolean')
  return Object.freeze({
    id: portableId(input.id, `${path}.id`),
    header: text(input.header, `${path}.header`),
    question: text(input.question, `${path}.question`),
    isSecret: input.isSecret,
  })
}

function parseInteraction(
  value: unknown,
  path: string,
): StrongFlowRuntimeInteraction {
  const input = record(value, path)
  exactKeys(input, [
    'id',
    'interactionType',
    'blocking',
    'status',
    'questions',
    'requestedEvent',
    'resolvedEvent',
  ], path)
  if (typeof input.blocking !== 'boolean') failure(`${path}.blocking`, 'must be a boolean')
  const questions = boundedArray(
    input.questions,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.questions,
    `${path}.questions`,
  ).map((entry, index) => parseQuestion(entry, `${path}.questions[${String(index)}]`))
  unique(questions.map(question => question.id), `${path}.questions`)
  const status = member(input.status, ['pending', 'resolved'] as const, `${path}.status`)
  const resolvedEvent = input.resolvedEvent === null
    ? null
    : parseEventReference(input.resolvedEvent, `${path}.resolvedEvent`)
  if ((status === 'resolved') !== (resolvedEvent !== null)) {
    failure(path, 'status and resolved event do not agree')
  }
  return Object.freeze({
    id: portableId(input.id, `${path}.id`),
    interactionType: member(
      input.interactionType,
      ['execution-approval', 'user-input'] as const,
      `${path}.interactionType`,
    ),
    blocking: input.blocking,
    status,
    questions: Object.freeze(questions),
    requestedEvent: parseEventReference(input.requestedEvent, `${path}.requestedEvent`),
    resolvedEvent,
  })
}

function parseFailure(value: unknown, path: string): StrongFlowRuntimeFailure {
  const input = record(value, path)
  exactKeys(input, ['message', 'code', 'event'], path)
  return Object.freeze({
    message: text(input.message, `${path}.message`),
    code: nullableText(input.code, `${path}.code`),
    event: parseEventReference(input.event, `${path}.event`),
  })
}

function parseRecovery(value: unknown, path: string): StrongFlowRuntimeRecovery {
  const input = record(value, path)
  exactKeys(input, [
    'state',
    'failureCount',
    'recoveryCount',
    'lastFailureEvent',
    'latestRecoveryEvent',
  ], path)
  const state = member(input.state, STRONGFLOW_RUNTIME_RECOVERY_STATES, `${path}.state`)
  const failureCount = nonNegativeInteger(input.failureCount, `${path}.failureCount`)
  const recoveryCount = nonNegativeInteger(input.recoveryCount, `${path}.recoveryCount`)
  const lastFailureEvent = input.lastFailureEvent === null
    ? null
    : parseEventReference(input.lastFailureEvent, `${path}.lastFailureEvent`)
  const latestRecoveryEvent = input.latestRecoveryEvent === null
    ? null
    : parseEventReference(input.latestRecoveryEvent, `${path}.latestRecoveryEvent`)
  if ((state === 'none') !== (failureCount === 0)
    || recoveryCount > failureCount
    || (failureCount === 0) !== (lastFailureEvent === null)
    || ((state === 'in-progress' || state === 'recovered') && latestRecoveryEvent === null)) {
    failure(path, 'state, counts, and event references do not agree')
  }
  return Object.freeze({
    state,
    failureCount,
    recoveryCount,
    lastFailureEvent,
    latestRecoveryEvent,
  })
}

function parseDiffSummary(value: unknown, path: string): StrongFlowRuntimeDiffSummary {
  const input = record(value, path)
  exactKeys(input, [
    'changedFileCount',
    'additions',
    'deletions',
    'detailsVisible',
    'event',
  ], path)
  if (input.detailsVisible !== false) {
    failure(`${path}.detailsVisible`, 'must remain false in the runtime projection')
  }
  return Object.freeze({
    changedFileCount: nonNegativeInteger(
      input.changedFileCount,
      `${path}.changedFileCount`,
    ),
    additions: nonNegativeInteger(input.additions, `${path}.additions`),
    deletions: nonNegativeInteger(input.deletions, `${path}.deletions`),
    detailsVisible: false,
    event: parseEventReference(input.event, `${path}.event`),
  })
}

function parseUsage(value: unknown, path: string): StrongFlowRuntimeUsage {
  const input = record(value, path)
  exactKeys(input, ['totals', 'event'], path)
  const totalsInput = record(input.totals, `${path}.totals`)
  if (Object.keys(totalsInput).length > STRONGFLOW_RUNTIME_EXECUTION_LIMITS.usageMetrics) {
    failure(`${path}.totals`, 'contains too many metrics')
  }
  const totals: Record<string, number> = {}
  for (const [key, value] of Object.entries(totalsInput)) {
    if (!USAGE_KEY_PATTERN.test(key)) failure(`${path}.totals`, 'contains an invalid metric name')
    totals[key] = nonNegativeInteger(value, `${path}.totals.${key}`)
  }
  return Object.freeze({
    totals: Object.freeze(totals),
    event: parseEventReference(input.event, `${path}.event`),
  })
}

function parseEvidence(value: unknown, path: string): StrongFlowRuntimeEvidence {
  const input = record(value, path)
  exactKeys(input, ['type', 'outcome', 'sourceRef', 'eventId'], path)
  const type = member(
    input.type,
    ['test', 'command', 'diff', 'file', 'commit', 'runtime_event', 'review_finding'] as const,
    `${path}.type`,
  )
  const eventId = portableId(input.eventId, `${path}.eventId`)
  const sourceRef = portableId(input.sourceRef, `${path}.sourceRef`)
  if (sourceRef !== `runtime_event:${eventId}`) {
    failure(path, 'eventId and sourceRef do not agree')
  }
  return Object.freeze({
    type,
    outcome: member(
      input.outcome,
      STRONGFLOW_RUNTIME_EVIDENCE_OUTCOMES,
      `${path}.outcome`,
    ),
    sourceRef,
    eventId,
  })
}

function parseSession(
  value: unknown,
  path: string,
): StrongFlowRuntimeSessionProjection {
  const input = record(value, path)
  exactKeys(input, [
    'stageRunId',
    'sessionBindingId',
    'dshSessionId',
    'codexSessionId',
    'asOfSequence',
    'plan',
    'agents',
    'agentEdges',
    'activities',
    'interactions',
    'failures',
    'recovery',
    'diffSummary',
    'usage',
    'evidence',
  ], path)
  const agents = boundedArray(
    input.agents,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.agents,
    `${path}.agents`,
  ).map((entry, index) => parseAgent(entry, `${path}.agents[${String(index)}]`))
  unique(agents.map(agent => agent.threadId), `${path}.agents`)
  const agentEdges = boundedArray(
    input.agentEdges,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.agentEdges,
    `${path}.agentEdges`,
  ).map((entry, index) => {
    const edgePath = `${path}.agentEdges[${String(index)}]`
    const edge = record(entry, edgePath)
    exactKeys(edge, ['parentThreadId', 'childThreadId'], edgePath)
    return Object.freeze({
      parentThreadId: portableId(edge.parentThreadId, `${edgePath}.parentThreadId`),
      childThreadId: portableId(edge.childThreadId, `${edgePath}.childThreadId`),
    })
  })
  unique(
    agentEdges.map(edge => `${edge.parentThreadId}\u0000${edge.childThreadId}`),
    `${path}.agentEdges`,
  )
  const knownThreadIds = new Set(agents.map(agent => agent.threadId))
  if (agentEdges.some(edge => (
    !knownThreadIds.has(edge.parentThreadId) || !knownThreadIds.has(edge.childThreadId)
  ))) failure(`${path}.agentEdges`, 'references an agent outside this bounded view')
  const expectedEdges = new Set(agents.flatMap(agent => agent.parentThreadId === null
    ? []
    : [`${agent.parentThreadId}\u0000${agent.threadId}`]))
  const actualEdges = new Set(agentEdges.map(edge => (
    `${edge.parentThreadId}\u0000${edge.childThreadId}`
  )))
  if (expectedEdges.size !== actualEdges.size
    || [...expectedEdges].some(edge => !actualEdges.has(edge))) {
    failure(`${path}.agentEdges`, 'does not match the visible Agent parent relationships')
  }
  const activities = boundedArray(
    input.activities,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.activities,
    `${path}.activities`,
  ).map((entry, index) => parseActivity(entry, `${path}.activities[${String(index)}]`))
  unique(activities.map(activity => activity.callId), `${path}.activities`)
  const interactions = boundedArray(
    input.interactions,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.interactions,
    `${path}.interactions`,
  ).map((entry, index) => parseInteraction(
    entry,
    `${path}.interactions[${String(index)}]`,
  ))
  unique(interactions.map(interaction => interaction.id), `${path}.interactions`)
  const failures = boundedArray(
    input.failures,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.failures,
    `${path}.failures`,
  ).map((entry, index) => parseFailure(entry, `${path}.failures[${String(index)}]`))
  const evidence = boundedArray(
    input.evidence,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.evidence,
    `${path}.evidence`,
  ).map((entry, index) => parseEvidence(entry, `${path}.evidence[${String(index)}]`))
  unique(evidence.map(entry => `${entry.sourceRef}\u0000${entry.type}`), `${path}.evidence`)
  return Object.freeze({
    stageRunId: identifier(input.stageRunId, `${path}.stageRunId`, StageRunId),
    sessionBindingId: identifier(
      input.sessionBindingId,
      `${path}.sessionBindingId`,
      SessionBindingId,
    ),
    dshSessionId: nullablePortableId(input.dshSessionId, `${path}.dshSessionId`),
    codexSessionId: nullablePortableId(input.codexSessionId, `${path}.codexSessionId`),
    asOfSequence: sequence(input.asOfSequence, `${path}.asOfSequence`),
    plan: input.plan === null ? null : parsePlan(input.plan, `${path}.plan`),
    agents: Object.freeze(agents),
    agentEdges: Object.freeze(agentEdges),
    activities: Object.freeze(activities),
    interactions: Object.freeze(interactions),
    failures: Object.freeze(failures),
    recovery: parseRecovery(input.recovery, `${path}.recovery`),
    diffSummary: input.diffSummary === null
      ? null
      : parseDiffSummary(input.diffSummary, `${path}.diffSummary`),
    usage: input.usage === null ? null : parseUsage(input.usage, `${path}.usage`),
    evidence: Object.freeze(evidence),
  })
}

/** Parse the safe, bounded execution view returned by the StrongFlow host. */
export function parseStrongFlowRuntimeExecutionProjection(
  value: unknown,
  path = 'runtimeExecutionProjection',
): StrongFlowRuntimeExecutionProjection {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'deliveryId',
    'deliveryRevision',
    'sessions',
  ], path)
  if (input.schemaVersion !== STRONGFLOW_RUNTIME_EXECUTION_SCHEMA_VERSION
    || input.protocol !== STRONGFLOW_RUNTIME_EXECUTION_PROTOCOL) {
    failure(path, 'uses an unsupported protocol')
  }
  const sessions = boundedArray(
    input.sessions,
    STRONGFLOW_RUNTIME_EXECUTION_LIMITS.sessions,
    `${path}.sessions`,
  ).map((entry, index) => parseSession(entry, `${path}.sessions[${String(index)}]`))
  unique(sessions.map(session => session.sessionBindingId), `${path}.sessions`)
  return Object.freeze({
    schemaVersion: STRONGFLOW_RUNTIME_EXECUTION_SCHEMA_VERSION,
    protocol: STRONGFLOW_RUNTIME_EXECUTION_PROTOCOL,
    deliveryId: identifier(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    deliveryRevision: (() => {
      const revision = nonNegativeInteger(input.deliveryRevision, `${path}.deliveryRevision`)
      if (revision === 0) failure(`${path}.deliveryRevision`, 'must be positive')
      return revision
    })(),
    sessions: Object.freeze(sessions),
  })
}
