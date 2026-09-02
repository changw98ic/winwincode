import { createHash } from 'node:crypto'

import {
  DELIVERY_SCHEMA_VERSION,
  STRONGFLOW_VERIFICATION_ROLE_IDS,
  parseCriterionResult,
  parseDelivery,
  parseDeliveryVerdict,
  parseEvidenceRef,
  type AcceptanceCriterion,
  type CriterionResult,
  type CriterionVerdict,
  type Delivery,
  type DeliveryVerdict,
  type DeliveryVerdictStatus,
  type EvidenceRef,
  type EvidenceRefType,
  type FrozenDeliveryCandidate,
  type RuntimeEvent,
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
} from './candidate-evidence.js'
import {
  IndependentVerificationError,
  projectIndependentVerification,
  type IndependentVerificationFinding,
  type IndependentVerificationProjection,
  type IndependentVerificationRole,
  type IndependentVerificationSessionSettlement,
} from './independent-verification.js'

export const DELIVERY_VERDICT_COMPUTATION_SCHEMA_VERSION = 1 as const

const MAX_RESULTS = 1_000
const MAX_EVIDENCE = 1_000
const MAX_EXPLANATION_LENGTH = 65_536

export type DeliveryVerdictComputationErrorCode =
  | 'INVALID_INPUT'
  | 'INVALID_DELIVERY'
  | 'ACCEPTANCE_STALE'
  | 'CANDIDATE_STALE'
  | 'VERIFICATION_INVALID'
  | 'EVIDENCE_INVALID'
  | 'RESULT_LIMIT_EXCEEDED'

export class DeliveryVerdictComputationError extends Error {
  readonly code: DeliveryVerdictComputationErrorCode

  constructor(
    code: DeliveryVerdictComputationErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'DeliveryVerdictComputationError'
    this.code = code
  }
}

export interface ComputeDeliveryVerdictInput {
  readonly delivery: Delivery
  readonly acceptance: AcceptanceVerificationInput
  readonly candidate: FrozenDeliveryCandidate
  readonly runtimeEvents: readonly RuntimeEvent[]
  readonly requiredRoles?: readonly StrongFlowVerificationRoleId[]
  readonly producedAtMillis: number
}

/**
 * Rebuildable result of evaluating current Codex facts. Only `evidence` and
 * `verdict` enter the canonical Delivery record.
 */
export interface ComputedDeliveryVerdict {
  readonly schemaVersion: typeof DELIVERY_VERDICT_COMPUTATION_SCHEMA_VERSION
  readonly acceptanceFreezeId: string
  readonly candidateRef: string
  readonly requiredRoles: readonly StrongFlowVerificationRoleId[]
  readonly evidence: readonly EvidenceRef[]
  readonly verdict: DeliveryVerdict
}

type FindingEvaluation = CriterionVerdict | 'evidence-mismatch'

interface CurrentRoleSessions {
  readonly role: IndependentVerificationRole
  readonly required: boolean
  readonly sessions: readonly IndependentVerificationSessionSettlement[]
}

interface CriterionComputation {
  readonly verdict: CriterionVerdict
  readonly evidenceIds: readonly string[]
  readonly explanation: string
  readonly unresolvedFindings: readonly string[]
}

function verdictError(
  code: DeliveryVerdictComputationErrorCode,
  message: string,
  options?: ErrorOptions,
): never {
  throw new DeliveryVerdictComputationError(code, message, options)
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
): void {
  const allowed = new Set([...required, ...optional])
  if (required.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !allowed.has(key))) {
    verdictError('INVALID_INPUT', 'delivery verdict computation input has an unexpected shape')
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

function digestId(prefix: string, value: unknown): string {
  return `${prefix}:${createHash('sha256').update(JSON.stringify(value)).digest('hex')}`
}

function boundedExplanation(parts: readonly string[]): string {
  const value = parts.join(' | ')
  if (value.length <= MAX_EXPLANATION_LENGTH) return value
  return `${value.slice(0, MAX_EXPLANATION_LENGTH - 20)} [summary truncated]`
}

function currentInputs(input: ComputeDeliveryVerdictInput): {
  readonly delivery: Delivery
  readonly acceptance: AcceptanceVerificationInput
  readonly candidate: FrozenDeliveryCandidate
  readonly producedAtMillis: number
} {
  let delivery: Delivery
  try {
    delivery = parseDelivery(input.delivery)
  } catch (error) {
    return verdictError('INVALID_DELIVERY', 'verdict computation requires a valid Delivery', {
      cause: error,
    })
  }
  let acceptance: AcceptanceVerificationInput
  try {
    acceptance = assertAcceptanceVerificationInputCurrent(delivery, input.acceptance)
  } catch (error) {
    return verdictError(
      error instanceof AcceptanceVerificationError ? 'ACCEPTANCE_STALE' : 'INVALID_INPUT',
      'verdict computation does not use the current approved acceptance input',
      { cause: error },
    )
  }
  let candidate: FrozenDeliveryCandidate
  try {
    candidate = assertFrozenDeliveryCandidateCurrent(delivery, input.candidate)
  } catch (error) {
    return verdictError(
      error instanceof DeliveryCandidateEvidenceError ? 'CANDIDATE_STALE' : 'INVALID_INPUT',
      'verdict computation does not use the current frozen candidate',
      { cause: error },
    )
  }
  if (acceptance.deliverySpecId !== candidate.deliverySpecId
    || acceptance.deliverySpecRevision !== candidate.deliverySpecRevision) {
    return verdictError(
      'CANDIDATE_STALE',
      'approved acceptance and frozen candidate identify different DeliverySpec revisions',
    )
  }
  if (!Number.isSafeInteger(input.producedAtMillis)
    || input.producedAtMillis < delivery.updatedAtMillis
    || Object.is(input.producedAtMillis, -0)) {
    return verdictError(
      'INVALID_INPUT',
      'verdict producedAtMillis must not precede the current Delivery',
    )
  }
  const latestRuntimeTime = input.runtimeEvents.reduce((latest, event) => (
    typeof event.occurredAtMillis === 'number' && Number.isSafeInteger(event.occurredAtMillis)
      ? Math.max(latest, event.occurredAtMillis)
      : latest
  ), 0)
  if (input.producedAtMillis < latestRuntimeTime) {
    return verdictError(
      'INVALID_INPUT',
      'verdict producedAtMillis must not precede its runtime evidence',
    )
  }
  return Object.freeze({
    delivery,
    acceptance,
    candidate,
    producedAtMillis: input.producedAtMillis,
  })
}

function verificationProjection(
  input: ComputeDeliveryVerdictInput,
  current: ReturnType<typeof currentInputs>,
): IndependentVerificationProjection {
  try {
    return projectIndependentVerification({
      delivery: current.delivery,
      acceptance: current.acceptance,
      candidate: current.candidate,
      runtimeEvents: input.runtimeEvents,
      ...(input.requiredRoles === undefined ? {} : { requiredRoles: input.requiredRoles }),
    })
  } catch (error) {
    return verdictError(
      error instanceof IndependentVerificationError
        ? 'VERIFICATION_INVALID'
        : 'INVALID_INPUT',
      'independent verification facts cannot produce a DeliveryVerdict',
      { cause: error },
    )
  }
}

function latestSessions(
  projection: IndependentVerificationProjection,
  role: IndependentVerificationRole,
): readonly IndependentVerificationSessionSettlement[] {
  const matching = projection.sessions.filter(session => session.role === role)
  const latest = matching.toSorted((left, right) => (
    right.stageRun.attempt - left.stageRun.attempt
    || right.stageRun.startedAtMillis - left.stageRun.startedAtMillis
    || right.stageRun.id.localeCompare(left.stageRun.id)
  ))[0]
  if (latest === undefined) return Object.freeze([])
  return Object.freeze(matching.filter(session => session.stageRun.id === latest.stageRun.id))
}

function currentRoleSessions(
  projection: IndependentVerificationProjection,
): readonly CurrentRoleSessions[] {
  return Object.freeze(STRONGFLOW_VERIFICATION_ROLE_IDS.flatMap((role) => {
    const sessions = latestSessions(projection, role)
    const required = projection.requiredRoles.includes(role)
    if (!required && sessions.length === 0) return []
    return [Object.freeze({ role, required, sessions })]
  }))
}

function evidenceIdentity(input: {
  readonly candidateRef: string
  readonly stageRunId: string
  readonly sessionBindingId: string
  readonly type: EvidenceRefType
  readonly sourceRef: string
}): string {
  return digestId('evidence', input)
}

function materializeEvidence(
  current: ReturnType<typeof currentInputs>,
  finding: IndependentVerificationFinding,
  evidenceByKey: Map<string, EvidenceRef>,
): readonly EvidenceRef[] {
  const facts = [
    Object.freeze({
      type: 'review_finding' as const,
      sourceRef: finding.sourceRef,
    }),
    ...finding.supportingEvidence.map(source => Object.freeze({
      type: source.type,
      sourceRef: source.sourceRef,
    })),
  ]
  const materialized = facts.map((fact) => {
    const key = [
      finding.stageRunId,
      finding.sessionBindingId,
      fact.type,
      fact.sourceRef,
    ].join('\u0000')
    const existing = evidenceByKey.get(key)
    if (existing !== undefined) return existing
    let evidence: EvidenceRef
    try {
      evidence = parseEvidenceRef({
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: evidenceIdentity({
          candidateRef: current.candidate.candidateRef,
          stageRunId: finding.stageRunId,
          sessionBindingId: finding.sessionBindingId,
          type: fact.type,
          sourceRef: fact.sourceRef,
        }),
        deliveryId: current.delivery.id,
        deliverySpecId: current.delivery.spec.id,
        deliverySpecRevision: current.delivery.spec.revision,
        stageRunId: finding.stageRunId,
        sessionBindingId: finding.sessionBindingId,
        candidateRef: current.candidate.candidateRef,
        type: fact.type,
        sourceRef: fact.sourceRef,
        createdAtMillis: current.producedAtMillis,
      })
    } catch (error) {
      return verdictError('EVIDENCE_INVALID', 'projected evidence cannot be persisted', {
        cause: error,
      })
    }
    evidenceByKey.set(key, evidence)
    if (evidenceByKey.size > MAX_EVIDENCE) {
      return verdictError('RESULT_LIMIT_EXCEEDED', 'verdict evidence exceeds the supported limit')
    }
    return evidence
  })
  return Object.freeze(materialized)
}

function infrastructureOutcome(outcome: string): boolean {
  return outcome === 'timed-out'
    || outcome === 'policy-denied'
    || outcome === 'infrastructure-failed'
    || outcome === 'cancelled'
}

function findingEvaluation(finding: IndependentVerificationFinding): FindingEvaluation {
  if (finding.verdict === 'infra_error' || finding.verdict === 'inconclusive') {
    return finding.verdict
  }
  if (finding.supportingEvidence.length === 0) return 'evidence-mismatch'
  if (finding.supportingEvidence.some(source => infrastructureOutcome(source.outcome))) {
    return 'infra_error'
  }
  if (finding.verdict === 'pass' && finding.supportingEvidence.some(source => (
    source.outcome === 'task-failed'
    || ((source.type === 'command' || source.type === 'test') && source.outcome !== 'succeeded')
  ))) return 'evidence-mismatch'
  return finding.verdict
}

function settlementFailure(
  state: IndependentVerificationSessionSettlement['state'],
): CriterionVerdict | null {
  switch (state) {
    case 'failed':
    case 'cancelled': return 'infra_error'
    case 'missing':
    case 'waiting':
    case 'running':
    case 'incomplete': return 'inconclusive'
    case 'settled': return null
  }
}

function currentSessionFindings(
  session: IndependentVerificationSessionSettlement,
): readonly IndependentVerificationFinding[] {
  if (session.state !== 'settled' || session.terminalEvent === null) {
    return Object.freeze([])
  }
  const terminalSequence = BigInt(session.terminalEvent.sequence)
  const eligible = session.findings.filter(finding => (
    BigInt(finding.event.sequence) <= terminalSequence
  ))
  const latestSequence = eligible.reduce<bigint | null>((latest, finding) => {
    const sequence = BigInt(finding.event.sequence)
    return latest === null || sequence > latest ? sequence : latest
  }, null)
  return latestSequence === null
    ? Object.freeze([])
    : Object.freeze(eligible.filter(finding => BigInt(finding.event.sequence) === latestSequence))
}

function criterionResultId(input: {
  readonly deliverySpecId: string
  readonly candidateRef: string
  readonly criterionId: string
  readonly verdict: CriterionVerdict
  readonly evidenceIds: readonly string[]
  readonly explanation: string
}): string {
  return digestId('criterion-result', input)
}

function computeCriterion(
  criterion: AcceptanceCriterion,
  roles: readonly CurrentRoleSessions[],
  current: ReturnType<typeof currentInputs>,
  evidenceByKey: Map<string, EvidenceRef>,
): CriterionComputation {
  const evaluations: FindingEvaluation[] = []
  const rawVerdicts = new Set<CriterionVerdict>()
  const evidenceIds = new Set<string>()
  const explanationParts: string[] = []
  const unresolvedFindings: string[] = []

  for (const role of roles) {
    if (role.sessions.length === 0) {
      evaluations.push('inconclusive')
      explanationParts.push(`${role.role}: required verification session is missing`)
      continue
    }
    for (const session of role.sessions) {
      const settlement = settlementFailure(session.state)
      if (settlement !== null) {
        evaluations.push(settlement)
        explanationParts.push(
          `${role.role}/${session.stageRun.id}: verification is ${session.state}`,
        )
        continue
      }
      const findings = currentSessionFindings(session)
        .filter(finding => finding.criterionId === criterion.id)
        .toSorted((left, right) => (
          left.event.eventId.localeCompare(right.event.eventId)
          || left.findingId.localeCompare(right.findingId)
        ))
      if (findings.length === 0) {
        evaluations.push('inconclusive')
        explanationParts.push(
          `${role.role}/${session.stageRun.id}: no current finding for ${criterion.id}`,
        )
        continue
      }
      for (const finding of findings) {
        rawVerdicts.add(finding.verdict)
        const evaluation = findingEvaluation(finding)
        evaluations.push(evaluation)
        for (const evidence of materializeEvidence(current, finding, evidenceByKey)) {
          evidenceIds.add(evidence.id)
        }
        explanationParts.push(
          `${finding.role}/${finding.findingId} (${finding.verdict}): ${finding.explanation}`,
        )
        if (evaluation === 'evidence-mismatch') {
          unresolvedFindings.push(
            `evidence-mismatch:${criterion.id}:${finding.role}:${finding.findingId}`,
          )
        }
      }
    }
  }

  const contradictory = rawVerdicts.has('pass') && rawVerdicts.has('fail')
  if (contradictory) {
    unresolvedFindings.push(
      `contradiction:${criterion.id}:${[...rawVerdicts].sort().join(',')}`,
    )
  }
  let verdict: CriterionVerdict
  if (contradictory || evaluations.includes('evidence-mismatch')) {
    verdict = 'inconclusive'
  } else if (evaluations.includes('fail')) {
    verdict = 'fail'
  } else if (evaluations.includes('infra_error')) {
    verdict = 'infra_error'
  } else if (evaluations.length === 0 || evaluations.includes('inconclusive')) {
    verdict = 'inconclusive'
  } else {
    verdict = 'pass'
  }
  const orderedEvidenceIds = Object.freeze([...evidenceIds].sort())
  return Object.freeze({
    verdict,
    evidenceIds: orderedEvidenceIds,
    explanation: boundedExplanation(explanationParts.length === 0
      ? [`No current independent finding exists for ${criterion.id}`]
      : explanationParts),
    unresolvedFindings: Object.freeze(unresolvedFindings.sort()),
  })
}

function deliveryVerdictStatus(
  delivery: Delivery,
  results: readonly CriterionResult[],
  unresolvedFindings: readonly string[],
): DeliveryVerdictStatus {
  const resultsByCriterion = new Map(results.map(result => [result.criterionId, result]))
  const required = delivery.spec.acceptanceCriteria
    .filter(criterion => criterion.required)
    .map(criterion => resultsByCriterion.get(criterion.id)!)
  if (required.some(result => result.verdict === 'fail')) return 'fail'
  if (required.some(result => result.verdict === 'infra_error')) return 'infra_error'
  if (required.some(result => result.verdict === 'inconclusive')
    || unresolvedFindings.length > 0) return 'inconclusive'
  return 'pass'
}

function assertComputedRelationships(
  current: ReturnType<typeof currentInputs>,
  evidence: readonly EvidenceRef[],
  verdict: DeliveryVerdict,
): void {
  try {
    parseDelivery({
      ...current.delivery,
      evidence: [...current.delivery.evidence, ...evidence],
      verdict,
      updatedAtMillis: Math.max(current.delivery.updatedAtMillis, current.producedAtMillis),
    })
  } catch (error) {
    verdictError(
      'EVIDENCE_INVALID',
      'computed verdict facts do not match the current Delivery relationships',
      { cause: error },
    )
  }
}

/** Compute one deterministic, fail-closed verdict from current Codex and Delivery facts. */
export function computeDeliveryVerdict(
  input: ComputeDeliveryVerdictInput,
): ComputedDeliveryVerdict {
  if (!isRecord(input) || !Array.isArray(input.runtimeEvents)) {
    return verdictError('INVALID_INPUT', 'delivery verdict computation input is malformed')
  }
  exactKeys(input, [
    'delivery',
    'acceptance',
    'candidate',
    'runtimeEvents',
    'producedAtMillis',
  ], ['requiredRoles'])
  const current = currentInputs(input)
  const projection = verificationProjection(input, current)
  const roles = currentRoleSessions(projection)
  const evidenceByKey = new Map<string, EvidenceRef>()
  const unresolved = new Set<string>()

  for (const item of current.delivery.attentionItems) {
    if (item.blocking && item.status === 'open' && item.type !== 'delivery_approval') {
      unresolved.add(`blocking-attention:${item.id}`)
    }
  }
  const currentFindings = roles.flatMap(role => (
    role.sessions.flatMap(session => currentSessionFindings(session))
  ))
  for (const finding of currentFindings.filter(entry => entry.criterionId === null)) {
    materializeEvidence(current, finding, evidenceByKey)
    unresolved.add(`unscoped-finding:${finding.role}:${finding.findingId}:${finding.verdict}`)
  }

  const results = current.delivery.spec.acceptanceCriteria.map((criterion) => {
    const computed = computeCriterion(criterion, roles, current, evidenceByKey)
    for (const finding of computed.unresolvedFindings) unresolved.add(finding)
    return parseCriterionResult({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: criterionResultId({
        deliverySpecId: current.delivery.spec.id,
        candidateRef: current.candidate.candidateRef,
        criterionId: criterion.id,
        verdict: computed.verdict,
        evidenceIds: computed.evidenceIds,
        explanation: computed.explanation,
      }),
      deliveryId: current.delivery.id,
      deliverySpecId: current.delivery.spec.id,
      criterionId: criterion.id,
      candidateRef: current.candidate.candidateRef,
      verdict: computed.verdict,
      evidenceRefs: computed.evidenceIds,
      explanation: computed.explanation,
      evaluatedAtMillis: current.producedAtMillis,
    })
  })
  if (results.length > MAX_RESULTS) {
    return verdictError('RESULT_LIMIT_EXCEEDED', 'criterion results exceed the supported limit')
  }
  const unresolvedFindings = Object.freeze([...unresolved].sort())
  const status = deliveryVerdictStatus(current.delivery, results, unresolvedFindings)
  let verdict: DeliveryVerdict
  try {
    verdict = parseDeliveryVerdict({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: digestId('delivery-verdict', {
        deliverySpecId: current.delivery.spec.id,
        deliverySpecRevision: current.delivery.spec.revision,
        candidateRef: current.candidate.candidateRef,
        status,
        criteria: results.map(result => ({
          id: result.id,
          criterionId: result.criterionId,
          verdict: result.verdict,
          evidenceRefs: result.evidenceRefs,
          explanation: result.explanation,
        })),
        unresolvedFindings,
      }),
      deliveryId: current.delivery.id,
      deliverySpecId: current.delivery.spec.id,
      candidateRef: current.candidate.candidateRef,
      status,
      criteria: results,
      unresolvedFindings,
      producedAtMillis: current.producedAtMillis,
    })
  } catch (error) {
    return verdictError('EVIDENCE_INVALID', 'computed DeliveryVerdict is invalid', {
      cause: error,
    })
  }
  const evidence = Object.freeze([...evidenceByKey.values()].sort((left, right) => (
    left.id.localeCompare(right.id)
  )))
  assertComputedRelationships(current, evidence, verdict)
  return immutable({
    schemaVersion: DELIVERY_VERDICT_COMPUTATION_SCHEMA_VERSION,
    acceptanceFreezeId: current.acceptance.freezeId,
    candidateRef: current.candidate.candidateRef,
    requiredRoles: projection.requiredRoles,
    evidence,
    verdict,
  })
}
