import { createHash } from 'node:crypto'

import {
  CRITERION_VERDICTS,
  RUNTIME_VERIFICATION_RESULT_PROTOCOL,
  STRONGFLOW_VERIFICATION_ROLE_IDS,
  parseDelivery,
  type AcceptanceCriterionId,
  type CriterionVerdict,
  type Delivery,
  type DeliveryId,
  type DeliverySpec,
  type DeliverySpecId,
  type RuntimeEvent,
  type RuntimeTerminalReason,
  type RuntimeVerificationEvidenceSource,
  type SessionBinding,
  type SessionBindingId,
  type StageRun,
  type StageRunId,
  type StrongFlowVerificationRoleId,
} from '@winwincode/contracts'

import {
  AcceptanceVerificationError,
  assertAcceptanceVerificationInputCurrent,
  type AcceptanceVerificationInput,
} from './acceptance-verification.js'
import {
  DeliveryCandidateEvidenceError,
  assertFrozenDeliveryCandidateCurrent,
  assertVerificationSessionReadOnly,
  resolveDeliveryEvidence,
  type DeliveryRuntimeEvidenceType,
  type FrozenDeliveryCandidate,
} from './candidate-evidence.js'
import {
  DeliveryRuntimeProjection,
  DeliveryRuntimeProjectionError,
  type DeliveryRuntimeEventLink,
  type DeliveryRuntimeEvidenceOutcome,
  type DeliverySessionRuntimeView,
} from './delivery-runtime-projection.js'

export const INDEPENDENT_VERIFICATION_SCHEMA_VERSION = 1 as const

export const INDEPENDENT_VERIFICATION_ROLES = STRONGFLOW_VERIFICATION_ROLE_IDS

export type IndependentVerificationRole = StrongFlowVerificationRoleId

export type IndependentVerificationErrorCode =
  | 'INVALID_INPUT'
  | 'INVALID_DELIVERY'
  | 'ACCEPTANCE_STALE'
  | 'CANDIDATE_STALE'
  | 'VERIFICATION_STAGE_MISMATCH'
  | 'VERIFICATION_SESSION_MISMATCH'
  | 'VERIFICATION_POLICY_MISMATCH'
  | 'RUNTIME_PROJECTION_FAILED'
  | 'RESULT_INVALID'
  | 'RESULT_IDENTITY_MISMATCH'
  | 'RESULT_EVIDENCE_MISMATCH'

export class IndependentVerificationError extends Error {
  readonly code: IndependentVerificationErrorCode

  constructor(
    code: IndependentVerificationErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'IndependentVerificationError'
    this.code = code
  }
}

export interface IndependentVerificationResultContract {
  readonly channel: 'codex-final-response'
  readonly protocol: typeof RUNTIME_VERIFICATION_RESULT_PROTOCOL
  readonly requiredResultFields: readonly [
    'protocol',
    'delivery_spec_id',
    'delivery_spec_revision',
    'candidate_ref',
    'findings',
  ]
  readonly requiredFindingFields: readonly [
    'finding_id',
    'criterion_id',
    'verdict',
    'explanation',
    'evidence_sources',
  ]
  readonly requiredEvidenceSourceFields: readonly ['type', 'event_id']
  readonly evidenceSourceTypes: readonly RuntimeVerificationEvidenceSource['type'][]
  readonly deliverySpecId: DeliverySpecId
  readonly deliverySpecRevision: number
  readonly candidateRef: string
  readonly criterionIds: readonly AcceptanceCriterionId[]
  readonly verdicts: readonly CriterionVerdict[]
}

/**
 * Exact model-visible input for one DSH/Codex verification session. The caller
 * sends this value through DSH; this module never creates or schedules an Agent.
 */
export interface IndependentVerificationSessionInput {
  readonly protocol: 'winwincode.independent-verification.v1'
  readonly role: IndependentVerificationRole
  readonly deliverySpec: DeliverySpec
  readonly acceptance: AcceptanceVerificationInput
  readonly candidate: FrozenDeliveryCandidate
  readonly resultContract: IndependentVerificationResultContract
}

/** Rebuildable association between one role-scoped StageRun and one existing session. */
export interface IndependentVerificationAssignment {
  readonly schemaVersion: typeof INDEPENDENT_VERIFICATION_SCHEMA_VERSION
  readonly assignmentRef: string
  readonly deliveryId: DeliveryId
  readonly deliverySpecId: DeliverySpecId
  readonly deliverySpecRevision: number
  readonly candidateRef: string
  readonly stageRunId: StageRunId
  readonly sessionBindingId: SessionBindingId
  readonly dshSessionId: string
  readonly codexSessionId: string
  readonly role: IndependentVerificationRole
  readonly sessionInput: IndependentVerificationSessionInput
}

export interface CreateIndependentVerificationAssignmentInput {
  readonly delivery: Delivery
  readonly acceptance: AcceptanceVerificationInput
  readonly candidate: FrozenDeliveryCandidate
  readonly stageRunId: StageRunId | string
  readonly sessionBindingId: SessionBindingId | string
}

export interface IndependentVerificationSupportingEvidence {
  readonly type: RuntimeVerificationEvidenceSource['type']
  readonly eventId: string
  readonly sourceRef: string
  readonly outcome: DeliveryRuntimeEvidenceOutcome
}

interface ResolvedRuntimeEvidenceFact {
  readonly type: DeliveryRuntimeEvidenceType
  readonly eventId: string
  readonly sourceRef: string
  readonly outcome: DeliveryRuntimeEvidenceOutcome
}

export interface IndependentVerificationFinding {
  readonly findingId: string
  readonly deliverySpecId: DeliverySpecId
  readonly deliverySpecRevision: number
  readonly candidateRef: string
  readonly criterionId: AcceptanceCriterionId | null
  readonly verdict: CriterionVerdict
  readonly explanation: string
  readonly role: IndependentVerificationRole
  readonly stageRunId: StageRunId
  readonly sessionBindingId: SessionBindingId
  readonly event: DeliveryRuntimeEventLink
  readonly sourceRef: string
  readonly supportingEvidence: readonly IndependentVerificationSupportingEvidence[]
}

export type IndependentVerificationSettlementState =
  | 'missing'
  | 'waiting'
  | 'running'
  | 'settled'
  | 'incomplete'
  | 'failed'
  | 'cancelled'

export interface IndependentVerificationSessionSettlement {
  readonly role: IndependentVerificationRole
  readonly stageRun: StageRun
  readonly assignment: IndependentVerificationAssignment | null
  readonly state: IndependentVerificationSettlementState
  readonly terminalReason: RuntimeTerminalReason | null
  readonly terminalEvent: DeliveryRuntimeEventLink | null
  /** Direct Codex projection, including its Agent graph. No second graph is created here. */
  readonly runtimeSession: DeliverySessionRuntimeView | null
  readonly findings: readonly IndependentVerificationFinding[]
}

export interface IndependentVerificationRequiredSettlement {
  readonly role: IndependentVerificationRole
  readonly state: IndependentVerificationSettlementState
  readonly stageRunIds: readonly StageRunId[]
  readonly sessionBindingIds: readonly SessionBindingId[]
}

export interface IndependentVerificationContradiction {
  readonly criterionId: AcceptanceCriterionId
  readonly verdicts: readonly CriterionVerdict[]
  readonly findingEventIds: readonly string[]
}

export interface ProjectIndependentVerificationInput {
  readonly delivery: Delivery
  readonly acceptance: AcceptanceVerificationInput
  readonly candidate: FrozenDeliveryCandidate
  readonly runtimeEvents: readonly RuntimeEvent[]
  readonly requiredRoles?: readonly IndependentVerificationRole[]
}

/** Pure, rebuildable reviewer/verifier view over Delivery and RuntimeSessionLedger facts. */
export interface IndependentVerificationProjection {
  readonly schemaVersion: typeof INDEPENDENT_VERIFICATION_SCHEMA_VERSION
  readonly deliveryId: DeliveryId
  readonly deliverySpecId: DeliverySpecId
  readonly deliverySpecRevision: number
  readonly acceptanceFreezeId: string
  readonly candidateRef: string
  readonly requiredRoles: readonly IndependentVerificationRole[]
  readonly requiredSettlements: readonly IndependentVerificationRequiredSettlement[]
  readonly sessions: readonly IndependentVerificationSessionSettlement[]
  readonly findings: readonly IndependentVerificationFinding[]
  readonly contradictions: readonly IndependentVerificationContradiction[]
}

function verificationError(
  code: IndependentVerificationErrorCode,
  message: string,
  options?: ErrorOptions,
): never {
  throw new IndependentVerificationError(code, message, options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  required: readonly string[],
  optional: readonly string[],
  label: string,
): void {
  const allowed = new Set([...required, ...optional])
  if (required.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !allowed.has(key))) {
    return verificationError('INVALID_INPUT', `${label} has an unexpected shape`)
  }
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

function currentInputs(input: {
  readonly delivery: Delivery
  readonly acceptance: AcceptanceVerificationInput
  readonly candidate: FrozenDeliveryCandidate
}): {
    readonly delivery: Delivery
    readonly acceptance: AcceptanceVerificationInput
    readonly candidate: FrozenDeliveryCandidate
  } {
  let delivery: Delivery
  try {
    delivery = parseDelivery(input.delivery)
  } catch (error) {
    return verificationError(
      'INVALID_DELIVERY',
      'independent verification requires a valid Delivery',
      { cause: error },
    )
  }
  let acceptance: AcceptanceVerificationInput
  try {
    acceptance = assertAcceptanceVerificationInputCurrent(delivery, input.acceptance)
  } catch (error) {
    return verificationError(
      error instanceof AcceptanceVerificationError ? 'ACCEPTANCE_STALE' : 'INVALID_INPUT',
      'independent verification does not use the current approved acceptance input',
      { cause: error },
    )
  }
  let candidate: FrozenDeliveryCandidate
  try {
    candidate = assertFrozenDeliveryCandidateCurrent(delivery, input.candidate)
  } catch (error) {
    return verificationError(
      error instanceof DeliveryCandidateEvidenceError ? 'CANDIDATE_STALE' : 'INVALID_INPUT',
      'independent verification does not use the current frozen candidate',
      { cause: error },
    )
  }
  if (acceptance.deliverySpecId !== candidate.deliverySpecId
    || acceptance.deliverySpecRevision !== candidate.deliverySpecRevision) {
    return verificationError(
      'RESULT_IDENTITY_MISMATCH',
      'approved acceptance and frozen candidate identify different DeliverySpec revisions',
    )
  }
  return Object.freeze({ delivery, acceptance, candidate })
}

function verificationRole(value: string): IndependentVerificationRole | null {
  return INDEPENDENT_VERIFICATION_ROLES.includes(value as IndependentVerificationRole)
    ? value as IndependentVerificationRole
    : null
}

function stageAndBinding(
  delivery: Delivery,
  candidate: FrozenDeliveryCandidate,
  stageRunId: string,
  sessionBindingId: string,
): { readonly stageRun: StageRun; readonly binding: SessionBinding } {
  const stageRun = delivery.stageRuns.find(entry => entry.id === stageRunId)
  const producer = delivery.stageRuns.find(entry => entry.id === candidate.producerStageRunId)
  if (stageRun === undefined
    || producer === undefined
    || stageRun.stage !== 'verifying'
    || stageRun.actorType !== 'codex'
    || stageRun.deliveryTaskId !== producer.deliveryTaskId
    || producer.finishedAtMillis === null
    || stageRun.startedAtMillis < producer.finishedAtMillis
    || verificationRole(stageRun.role) === null) {
    return verificationError(
      'VERIFICATION_STAGE_MISMATCH',
      'verification assignment does not identify a role-scoped verification StageRun for the current candidate',
    )
  }
  const binding = delivery.sessionBindings.find(entry => entry.id === sessionBindingId)
  const producerBinding = delivery.sessionBindings.find(
    entry => entry.id === candidate.producerSessionBindingId,
  )
  if (binding === undefined
    || producerBinding === undefined
    || binding.deliveryId !== delivery.id
    || binding.stageRunId !== stageRun.id
    || binding.dshSessionId === null
    || binding.codexSessionId === null
    || binding.boundAtMillis < stageRun.startedAtMillis
    || binding.id === producerBinding.id
    || binding.dshSessionId === producerBinding.dshSessionId
    || binding.codexSessionId === producerBinding.codexSessionId) {
    return verificationError(
      'VERIFICATION_SESSION_MISMATCH',
      'verification assignment must use its own complete DSH and Codex SessionBinding',
    )
  }
  return Object.freeze({ stageRun, binding })
}

function assignmentIdentity(value: Omit<IndependentVerificationAssignment, 'assignmentRef'>): string {
  return `verification-assignment:sha256:${createHash('sha256')
    .update(JSON.stringify(value))
    .digest('hex')}`
}

/** Bind exact approved inputs to one already-created DSH/Codex role session. */
export function createIndependentVerificationAssignment(
  input: CreateIndependentVerificationAssignmentInput,
): IndependentVerificationAssignment {
  if (!isRecord(input)) {
    return verificationError('INVALID_INPUT', 'verification assignment input must be an object')
  }
  exactKeys(input, [
    'delivery',
    'acceptance',
    'candidate',
    'stageRunId',
    'sessionBindingId',
  ], [], 'verification assignment input')
  if (typeof input.stageRunId !== 'string' || typeof input.sessionBindingId !== 'string') {
    return verificationError(
      'INVALID_INPUT',
      'verification assignment StageRun and SessionBinding identities must be strings',
    )
  }
  const { delivery, acceptance, candidate } = currentInputs(input)
  const { stageRun, binding } = stageAndBinding(
    delivery,
    candidate,
    input.stageRunId,
    input.sessionBindingId,
  )
  const role = verificationRole(stageRun.role)!
  try {
    assertVerificationSessionReadOnly(stageRun, binding, [])
  } catch (error) {
    return verificationError(
      'VERIFICATION_POLICY_MISMATCH',
      'verification assignment does not use the canonical read-only candidate policy',
      { cause: error },
    )
  }
  const sessionInput: IndependentVerificationSessionInput = immutable({
    protocol: 'winwincode.independent-verification.v1',
    role,
    deliverySpec: delivery.spec,
    acceptance,
    candidate,
    resultContract: {
      channel: 'codex-final-response',
      protocol: RUNTIME_VERIFICATION_RESULT_PROTOCOL,
      requiredResultFields: [
        'protocol',
        'delivery_spec_id',
        'delivery_spec_revision',
        'candidate_ref',
        'findings',
      ],
      requiredFindingFields: [
        'finding_id',
        'criterion_id',
        'verdict',
        'explanation',
        'evidence_sources',
      ],
      requiredEvidenceSourceFields: ['type', 'event_id'],
      evidenceSourceTypes: [
        'test',
        'command',
        'diff',
        'file',
        'commit',
        'runtime_event',
      ],
      deliverySpecId: delivery.spec.id,
      deliverySpecRevision: delivery.spec.revision,
      candidateRef: candidate.candidateRef,
      criterionIds: acceptance.criteria.map(entry => entry.criterion.id),
      verdicts: CRITERION_VERDICTS,
    },
  })
  const unsigned: Omit<IndependentVerificationAssignment, 'assignmentRef'> = immutable({
    schemaVersion: INDEPENDENT_VERIFICATION_SCHEMA_VERSION,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    deliverySpecRevision: delivery.spec.revision,
    candidateRef: candidate.candidateRef,
    stageRunId: stageRun.id,
    sessionBindingId: binding.id,
    dshSessionId: binding.dshSessionId!,
    codexSessionId: binding.codexSessionId!,
    role,
    sessionInput,
  })
  return immutable({ ...unsigned, assignmentRef: assignmentIdentity(unsigned) })
}

/** Canonical JSON body that DSH can submit as the first turn of the assigned session. */
export function serializeIndependentVerificationSessionInput(
  assignment: IndependentVerificationAssignment,
): string {
  return JSON.stringify(assignment.sessionInput)
}

function eventLink(event: RuntimeEvent): DeliveryRuntimeEventLink {
  return Object.freeze({
    eventId: event.id,
    sourceRef: `runtime_event:${event.id}`,
    sessionId: event.source.sessionId,
    kernelSessionId: event.source.kernelSessionId,
    sequence: event.cursor.sequence,
    kind: event.kind,
  })
}

function matchingEvents(
  binding: SessionBinding,
  events: readonly RuntimeEvent[],
): readonly RuntimeEvent[] {
  return events.filter(event => (
    isRecord(event)
    && isRecord(event.source)
    && event.source.sessionId === binding.dshSessionId
    && event.source.kernelSessionId === binding.codexSessionId
  ))
}

function latestTurnEvents(events: readonly RuntimeEvent[]): readonly RuntimeEvent[] {
  let latestStartSequence = 0n
  for (const event of events) {
    if (event.kind !== 'turn.started') continue
    const sequence = BigInt(event.cursor.sequence)
    if (sequence > latestStartSequence) latestStartSequence = sequence
  }
  return Object.freeze(events.filter(event => (
    BigInt(event.cursor.sequence) >= latestStartSequence
  )))
}

function rawVerificationResult(event: RuntimeEvent): boolean {
  return event.data.type === 'agent_message'
    && typeof event.data.message === 'string'
    && event.data.message.includes(RUNTIME_VERIFICATION_RESULT_PROTOCOL)
}

function evidenceId(
  assignment: IndependentVerificationAssignment,
  eventId: string,
  type: DeliveryRuntimeEvidenceType,
): string {
  const digest = createHash('sha256').update(JSON.stringify({
    assignmentRef: assignment.assignmentRef,
    eventId,
    type,
  })).digest('hex')
  return `verification-projection:${digest}`
}

function evidenceTimestamp(
  stageRun: StageRun,
  binding: SessionBinding,
  event: RuntimeEvent,
): number {
  return Math.max(
    stageRun.startedAtMillis,
    binding.boundAtMillis,
    event.occurredAtMillis ?? 0,
  )
}

function resolvedRuntimeEvidence(
  input: {
    readonly delivery: Delivery
    readonly acceptance: AcceptanceVerificationInput
    readonly candidate: FrozenDeliveryCandidate
    readonly assignment: IndependentVerificationAssignment
    readonly stageRun: StageRun
    readonly binding: SessionBinding
    readonly event: RuntimeEvent
    readonly type: DeliveryRuntimeEvidenceType
    readonly runtimeEvents: readonly RuntimeEvent[]
  },
): ResolvedRuntimeEvidenceFact {
  try {
    const resolved = resolveDeliveryEvidence({
      delivery: input.delivery,
      acceptance: input.acceptance,
      candidate: input.candidate,
      evidenceId: evidenceId(input.assignment, input.event.id, input.type),
      stageRunId: input.stageRun.id,
      sessionBindingId: input.binding.id,
      source: { kind: 'runtime-event', type: input.type, eventId: input.event.id },
      runtimeEvents: input.runtimeEvents,
      createdAtMillis: evidenceTimestamp(input.stageRun, input.binding, input.event),
    })
    return Object.freeze({
      type: input.type,
      eventId: input.event.id,
      sourceRef: resolved.evidence.sourceRef,
      outcome: resolved.outcome,
    })
  } catch (error) {
    return verificationError(
      'RESULT_EVIDENCE_MISMATCH',
      `verification result cites invalid evidence event ${input.event.id}`,
      { cause: error },
    )
  }
}

function findingsFromEvent(input: {
  readonly delivery: Delivery
  readonly acceptance: AcceptanceVerificationInput
  readonly candidate: FrozenDeliveryCandidate
  readonly assignment: IndependentVerificationAssignment
  readonly stageRun: StageRun
  readonly binding: SessionBinding
  readonly event: RuntimeEvent
  readonly sessionEvents: readonly RuntimeEvent[]
  readonly runtimeEvents: readonly RuntimeEvent[]
}): readonly IndependentVerificationFinding[] {
  const semantic = input.event.semantic
  if (semantic?.kind !== 'verification-result') {
    if (rawVerificationResult(input.event)) {
      return verificationError(
        'RESULT_INVALID',
        `verification result event ${input.event.id} is not a complete structured final response`,
      )
    }
    return Object.freeze([])
  }
  if (semantic.deliverySpecId !== input.delivery.spec.id
    || semantic.deliverySpecRevision !== input.delivery.spec.revision
    || semantic.candidateRef !== input.candidate.candidateRef) {
    return verificationError(
      'RESULT_IDENTITY_MISMATCH',
      `verification result event ${input.event.id} identifies another spec or candidate`,
    )
  }
  const findingIds = new Set(semantic.findings.map(finding => finding.findingId))
  if (findingIds.size !== semantic.findings.length) {
    return verificationError(
      'RESULT_INVALID',
      `verification result event ${input.event.id} repeats a finding identity`,
    )
  }
  const findingEvidence = resolvedRuntimeEvidence({
    ...input,
    event: input.event,
    type: 'review_finding',
  })
  return immutable(semantic.findings.map((finding) => {
    const criterion = finding.criterionId === null
      ? null
      : input.delivery.spec.acceptanceCriteria.find(entry => entry.id === finding.criterionId)
    if (finding.criterionId !== null && criterion === undefined) {
      return verificationError(
        'RESULT_IDENTITY_MISMATCH',
        `verification result event ${input.event.id} identifies an unknown acceptance criterion`,
      )
    }
    if (!/^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u.test(finding.findingId)
      || finding.explanation.length > 65_536) {
      return verificationError(
        'RESULT_INVALID',
        `verification result event ${input.event.id} contains an invalid finding`,
      )
    }
    const support = finding.evidenceSources.map((source) => {
      const evidenceEvent = input.sessionEvents.find(event => event.id === source.eventId)
      if (evidenceEvent === undefined
        || BigInt(evidenceEvent.cursor.sequence) >= BigInt(input.event.cursor.sequence)) {
        return verificationError(
          'RESULT_EVIDENCE_MISMATCH',
          `verification result event ${input.event.id} cites missing or later evidence`,
        )
      }
      const resolved = resolvedRuntimeEvidence({
        ...input,
        event: evidenceEvent,
        type: source.type,
      })
      return Object.freeze({
        type: source.type,
        eventId: resolved.eventId,
        sourceRef: resolved.sourceRef,
        outcome: resolved.outcome,
      })
    })
    return immutable({
      findingId: finding.findingId,
      deliverySpecId: input.delivery.spec.id,
      deliverySpecRevision: input.delivery.spec.revision,
      candidateRef: input.candidate.candidateRef,
      criterionId: criterion?.id ?? null,
      verdict: finding.verdict,
      explanation: finding.explanation,
      role: input.assignment.role,
      stageRunId: input.stageRun.id,
      sessionBindingId: input.binding.id,
      event: eventLink(input.event),
      sourceRef: findingEvidence.sourceRef,
      supportingEvidence: support,
    })
  }))
}

function terminalState(
  stageRun: StageRun,
  events: readonly RuntimeEvent[],
  findings: readonly IndependentVerificationFinding[],
): {
    readonly state: IndependentVerificationSettlementState
    readonly terminalReason: RuntimeTerminalReason | null
    readonly terminalEvent: DeliveryRuntimeEventLink | null
  } {
  let state: IndependentVerificationSettlementState = 'waiting'
  let terminalReason: RuntimeTerminalReason | null = null
  let terminalEvent: RuntimeEvent | null = null
  let lastTurnStartSequence = 0n
  for (const event of events) {
    if (event.kind === 'turn.started') {
      state = 'running'
      terminalReason = null
      terminalEvent = null
      lastTurnStartSequence = BigInt(event.cursor.sequence)
    } else if (event.kind === 'failure') {
      state = 'failed'
      terminalReason = event.terminalReason ?? 'failed'
      terminalEvent = event
    } else if (event.kind === 'turn.aborted') {
      state = event.terminalReason === 'cancelled' || event.terminalReason === 'aborted'
        ? 'cancelled'
        : 'failed'
      terminalReason = event.terminalReason ?? 'aborted'
      terminalEvent = event
    } else if (event.kind === 'turn.completed') {
      terminalReason = event.terminalReason ?? 'unknown'
      terminalEvent = event
      state = terminalReason === 'completed' ? 'settled' : 'failed'
    }
  }
  if (stageRun.status === 'failed') {
    state = 'failed'
    terminalReason ??= 'failed'
  } else if (stageRun.status === 'cancelled') {
    state = 'cancelled'
    terminalReason ??= 'cancelled'
  } else if (stageRun.status === 'succeeded' && (state === 'waiting' || state === 'running')) {
    state = 'incomplete'
  }
  if (state === 'settled' && terminalEvent !== null) {
    const terminalSequence = BigInt(terminalEvent.cursor.sequence)
    const currentFindings = findings.filter(finding => {
      const sequence = BigInt(finding.event.sequence)
      return sequence >= lastTurnStartSequence && sequence <= terminalSequence
    })
    if (currentFindings.length === 0) state = 'incomplete'
  }
  return Object.freeze({
    state,
    terminalReason,
    terminalEvent: terminalEvent === null ? null : eventLink(terminalEvent),
  })
}

function requiredRoles(value: unknown): readonly IndependentVerificationRole[] {
  const roles = value === undefined ? ['reviewer', 'verifier'] : value
  if (!Array.isArray(roles)
    || roles.length < 2
    || roles.some(role => typeof role !== 'string' || verificationRole(role) === null)
    || new Set(roles).size !== roles.length
    || !roles.includes('reviewer')
    || !roles.includes('verifier')) {
    return verificationError(
      'INVALID_INPUT',
      'required verification roles must contain reviewer and verifier exactly once',
    )
  }
  return Object.freeze(INDEPENDENT_VERIFICATION_ROLES.filter(role => roles.includes(role)))
}

function aggregateSettlementState(
  sessions: readonly IndependentVerificationSessionSettlement[],
): IndependentVerificationSettlementState {
  if (sessions.length === 0) return 'missing'
  const states = new Set(sessions.map(session => session.state))
  if (states.has('failed')) return 'failed'
  if (states.has('cancelled')) return 'cancelled'
  if (states.has('incomplete')) return 'incomplete'
  if (states.has('running')) return 'running'
  if (states.has('waiting')) return 'waiting'
  if (states.has('missing')) return 'missing'
  return 'settled'
}

function requiredSettlement(
  role: IndependentVerificationRole,
  sessions: readonly IndependentVerificationSessionSettlement[],
): IndependentVerificationRequiredSettlement {
  const matching = sessions.filter(session => session.role === role)
  const latest = matching.toSorted((left, right) => (
    right.stageRun.attempt - left.stageRun.attempt
    || right.stageRun.startedAtMillis - left.stageRun.startedAtMillis
    || right.stageRun.id.localeCompare(left.stageRun.id)
  ))[0]
  const current = latest === undefined
    ? []
    : matching.filter(session => session.stageRun.id === latest.stageRun.id)
  return Object.freeze({
    role,
    state: aggregateSettlementState(current),
    stageRunIds: Object.freeze([...new Set(current.map(session => session.stageRun.id))].sort()),
    sessionBindingIds: Object.freeze(current.flatMap(session => (
      session.assignment === null ? [] : [session.assignment.sessionBindingId]
    )).sort()),
  })
}

function contradictions(
  delivery: Delivery,
  findings: readonly IndependentVerificationFinding[],
): readonly IndependentVerificationContradiction[] {
  return Object.freeze(delivery.spec.acceptanceCriteria.flatMap((criterion) => {
    const matching = findings.filter(finding => finding.criterionId === criterion.id)
    const verdicts = CRITERION_VERDICTS.filter(verdict => (
      matching.some(finding => finding.verdict === verdict)
    ))
    if (verdicts.length < 2) return []
    return [Object.freeze({
      criterionId: criterion.id,
      verdicts: Object.freeze(verdicts),
      findingEventIds: Object.freeze(matching.map(finding => finding.event.eventId).sort()),
    })]
  }))
}

/**
 * Project independent role settlements and exact findings. Runtime parallelism
 * remains the Codex Agent graph exposed by each existing runtimeSession value.
 */
export function projectIndependentVerification(
  input: ProjectIndependentVerificationInput,
): IndependentVerificationProjection {
  if (!isRecord(input) || !Array.isArray(input.runtimeEvents)) {
    return verificationError('INVALID_INPUT', 'independent verification input is malformed')
  }
  exactKeys(input, [
    'delivery',
    'acceptance',
    'candidate',
    'runtimeEvents',
  ], ['requiredRoles'], 'independent verification input')
  const current = currentInputs(input)
  const required = requiredRoles(input.requiredRoles)
  let runtime: ReturnType<DeliveryRuntimeProjection['replay']>
  try {
    runtime = new DeliveryRuntimeProjection({ delivery: current.delivery })
      .replay(input.runtimeEvents)
  } catch (error) {
    return verificationError(
      error instanceof DeliveryRuntimeProjectionError
        ? 'RUNTIME_PROJECTION_FAILED'
        : 'INVALID_INPUT',
      'independent verification could not rebuild the supplied runtime ledgers',
      { cause: error },
    )
  }
  const producer = current.delivery.stageRuns.find(
    stageRun => stageRun.id === current.candidate.producerStageRunId,
  )!
  const verificationRuns = current.delivery.stageRuns.filter(stageRun => (
    stageRun.stage === 'verifying'
    && stageRun.actorType === 'codex'
    && stageRun.deliveryTaskId === producer.deliveryTaskId
    && producer.finishedAtMillis !== null
    && stageRun.startedAtMillis >= producer.finishedAtMillis
  ))
  const sessions: IndependentVerificationSessionSettlement[] = []
  for (const stageRun of verificationRuns) {
    const role = verificationRole(stageRun.role)
    if (role === null) {
      return verificationError(
        'VERIFICATION_STAGE_MISMATCH',
        `verification StageRun ${stageRun.id} does not use an independent verification role`,
      )
    }
    const bindings = current.delivery.sessionBindings.filter(
      binding => binding.stageRunId === stageRun.id,
    )
    if (bindings.length === 0) {
      sessions.push(immutable({
        role,
        stageRun,
        assignment: null,
        state: stageRun.status === 'failed'
          ? 'failed'
          : stageRun.status === 'cancelled'
            ? 'cancelled'
            : 'missing',
        terminalReason: stageRun.status === 'failed'
          ? 'failed'
          : stageRun.status === 'cancelled'
            ? 'cancelled'
            : null,
        terminalEvent: null,
        runtimeSession: null,
        findings: [],
      }))
      continue
    }
    for (const binding of bindings) {
      const assignment = createIndependentVerificationAssignment({
        delivery: current.delivery,
        acceptance: current.acceptance,
        candidate: current.candidate,
        stageRunId: stageRun.id,
        sessionBindingId: binding.id,
      })
      const events = matchingEvents(binding, input.runtimeEvents)
      if (events.some(event => event.source.roleId !== role)) {
        return verificationError(
          'VERIFICATION_SESSION_MISMATCH',
          `verification SessionBinding ${binding.id} contains another role identity`,
        )
      }
      try {
        assertVerificationSessionReadOnly(stageRun, binding, input.runtimeEvents)
      } catch (error) {
        return verificationError(
          'VERIFICATION_POLICY_MISMATCH',
          `verification SessionBinding ${binding.id} violated its read-only policy`,
          { cause: error },
        )
      }
      const runtimeSession = runtime.stages
        .find(stage => stage.stageRun.id === stageRun.id)
        ?.sessions.find(session => session.binding.id === binding.id) ?? null
      if (runtimeSession === null) {
        return verificationError(
          'RUNTIME_PROJECTION_FAILED',
          `verification SessionBinding ${binding.id} is absent from the runtime projection`,
        )
      }
      // A correction turn supersedes an earlier malformed result attempt while
      // the append-only RuntimeSessionLedger retains both turns. Only the
      // newest turn can contribute verification findings or result evidence.
      const findings = latestTurnEvents(events).flatMap((event) => {
        return findingsFromEvent({
          ...current,
          assignment,
          stageRun,
          binding,
          event,
          sessionEvents: events,
          runtimeEvents: input.runtimeEvents,
        })
      })
      const terminal = terminalState(stageRun, events, findings)
      sessions.push(immutable({
        role,
        stageRun,
        assignment,
        ...terminal,
        runtimeSession,
        findings,
      }))
    }
  }
  sessions.sort((left, right) => (
    left.stageRun.startedAtMillis - right.stageRun.startedAtMillis
    || left.stageRun.id.localeCompare(right.stageRun.id)
    || (left.assignment?.sessionBindingId ?? '').localeCompare(
      right.assignment?.sessionBindingId ?? '',
    )
  ))
  const findings = Object.freeze(sessions.flatMap(session => session.findings))
  return immutable({
    schemaVersion: INDEPENDENT_VERIFICATION_SCHEMA_VERSION,
    deliveryId: current.delivery.id,
    deliverySpecId: current.delivery.spec.id,
    deliverySpecRevision: current.delivery.spec.revision,
    acceptanceFreezeId: current.acceptance.freezeId,
    candidateRef: current.candidate.candidateRef,
    requiredRoles: required,
    requiredSettlements: required.map(role => requiredSettlement(role, sessions)),
    sessions,
    findings,
    contradictions: contradictions(current.delivery, findings),
  })
}
