import { createHash } from 'node:crypto'

import {
  DELIVERY_SCHEMA_VERSION,
  parseAttentionItem,
  parseDelivery,
  type AttentionItem,
  type AttentionItemStatus,
  type CriterionResult,
  type Delivery,
  type DeliveryStatus,
  type StageRunId,
} from '@winwincode/contracts'

export const DELIVERY_ATTENTION_CLASSIFICATION_SCHEMA_VERSION = 1 as const
export const DELIVERY_ATTENTION_CONTEXT_PROTOCOL =
  'winwincode.delivery-attention.v1' as const

export const DELIVERY_ATTENTION_ACTIONS = Object.freeze([
  'start-rework',
  'retry-verification',
  'complete-verification',
  'resolve-verification-conflict',
  'clarify-scope',
] as const)

export type DeliveryAttentionAction = typeof DELIVERY_ATTENTION_ACTIONS[number]

export type DeliveryAttentionClassificationErrorCode =
  | 'INVALID_INPUT'
  | 'VERDICT_MISSING'
  | 'VERIFICATION_STAGE_MISMATCH'
  | 'ATTENTION_STALE'
  | 'ATTENTION_NON_ACTIONABLE'

export class DeliveryAttentionClassificationError extends Error {
  readonly code: DeliveryAttentionClassificationErrorCode

  constructor(
    code: DeliveryAttentionClassificationErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'DeliveryAttentionClassificationError'
    this.code = code
  }
}

export interface DeriveDeliveryVerdictAttentionInput {
  readonly delivery: Delivery
  readonly verificationStageRunId: StageRunId
  readonly createdAtMillis: number
}

/** Rebuildable classification; only its AttentionItem values enter Delivery. */
export interface DerivedDeliveryVerdictAttention {
  readonly schemaVersion: typeof DELIVERY_ATTENTION_CLASSIFICATION_SCHEMA_VERSION
  readonly verdictId: string
  readonly candidateRef: string
  readonly verificationStageRunId: StageRunId
  readonly attentionItems: readonly AttentionItem[]
}

interface AttentionContext {
  readonly protocol: typeof DELIVERY_ATTENTION_CONTEXT_PROTOCOL
  readonly verdictId: string
  readonly candidateRef: string
  readonly stageRunId: string
  readonly action: DeliveryAttentionAction
  readonly criterionResultId: string | null
  readonly criterionId: string | null
  readonly evidenceRefIds: readonly string[]
  readonly evidenceRefCount: number
  readonly evidenceSetSha256: string
  readonly unresolvedFindings: readonly string[]
  readonly unresolvedFindingCount: number
  readonly unresolvedFindingSetSha256: string
  readonly reworkAttemptsUsed: number
  readonly reworkAttemptsLimit: number
  readonly repeatedCriterionFailure: boolean
}

const MAX_CONTEXT_EVIDENCE_REFS = 32
const MAX_CONTEXT_UNRESOLVED_FINDINGS = 32

function attentionError(
  code: DeliveryAttentionClassificationErrorCode,
  message: string,
  options?: ErrorOptions,
): never {
  throw new DeliveryAttentionClassificationError(code, message, options)
}

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

function digest(value: unknown): string {
  return createHash('sha256').update(JSON.stringify(value)).digest('hex')
}

function digestId(prefix: string, value: unknown): string {
  return `${prefix}:${digest(value)}`
}

function criterionUnresolvedFindings(
  result: CriterionResult,
  unresolvedFindings: readonly string[],
): readonly string[] {
  return Object.freeze(unresolvedFindings.filter(finding => (
    finding.startsWith(`contradiction:${result.criterionId}:`)
    || finding.startsWith(`evidence-mismatch:${result.criterionId}:`)
  )).toSorted())
}

function criterionAction(
  delivery: Delivery,
  result: CriterionResult,
  unresolvedFindings: readonly string[],
): {
  readonly action: DeliveryAttentionAction
  readonly type: AttentionItem['type']
  readonly title: string
  readonly label: string
  readonly description: string
} {
  if (result.verdict === 'fail') {
    const reworkAttemptsUsed = delivery.stageRuns.filter(run => (
      run.stage === 'reworking'
    )).length
    const repeatedCriterionFailure = delivery.attentionItems.some((item) => {
      if (item.status !== 'resolved') return false
      const context = parseAttentionContext(item)
      return context?.action === 'start-rework'
        && context.criterionId === result.criterionId
        && context.verdictId !== delivery.verdict?.id
    })
    if (repeatedCriterionFailure || reworkAttemptsUsed >= delivery.spec.maxReworkAttempts) {
      return Object.freeze({
        action: 'clarify-scope',
        type: 'scope_change',
        title: repeatedCriterionFailure
          ? 'Repeated criterion failure requires definition review'
          : 'Rework limit is exhausted',
        label: 'Review delivery definition',
        description: 'Return to clarification and approve a revised DeliverySpec before more code execution.',
      })
    }
    return Object.freeze({
      action: 'start-rework',
      type: 'decision_required',
      title: 'Acceptance criterion requires rework',
      label: 'Start rework',
      description: 'Open a bounded rework StageRun for the failed criterion.',
    })
  }
  if (result.verdict === 'infra_error') {
    return Object.freeze({
      action: 'retry-verification',
      type: 'verification_blocked',
      title: 'Verification infrastructure must be retried',
      label: 'Retry verification',
      description: 'Run verification again on the unchanged candidate.',
    })
  }
  if (unresolvedFindings.some(finding => finding.startsWith('contradiction:'))) {
    return Object.freeze({
      action: 'resolve-verification-conflict',
      type: 'decision_required',
      title: 'Independent verification findings conflict',
      label: 'Resolve and reverify',
      description: 'Resolve the cited disagreement and verify the same candidate again.',
    })
  }
  return Object.freeze({
    action: 'complete-verification',
    type: 'verification_blocked',
    title: 'Verification evidence is incomplete',
    label: 'Complete verification',
    description: 'Collect current direct evidence and verify the same candidate again.',
  })
}

function contextFor(input: {
  readonly delivery: Delivery
  readonly stageRunId: StageRunId
  readonly action: DeliveryAttentionAction
  readonly result: CriterionResult | null
  readonly unresolvedFindings: readonly string[]
}): string {
  const verdict = input.delivery.verdict!
  const evidenceRefIds = input.result?.evidenceRefs.toSorted() ?? []
  const unresolvedFindings = input.unresolvedFindings.toSorted()
  const reworkAttemptsUsed = input.delivery.stageRuns.filter(run => (
    run.stage === 'reworking'
  )).length
  const repeatedCriterionFailure = input.result === null
    ? false
    : input.delivery.attentionItems.some((item) => {
      if (item.status !== 'resolved') return false
      const prior = parseAttentionContext(item)
      return prior?.action === 'start-rework'
        && prior.criterionId === input.result!.criterionId
        && prior.verdictId !== input.delivery.verdict?.id
    })
  const context: AttentionContext = Object.freeze({
    protocol: DELIVERY_ATTENTION_CONTEXT_PROTOCOL,
    verdictId: verdict.id,
    candidateRef: verdict.candidateRef,
    stageRunId: input.stageRunId,
    action: input.action,
    criterionResultId: input.result?.id ?? null,
    criterionId: input.result?.criterionId ?? null,
    evidenceRefIds: Object.freeze(evidenceRefIds.slice(0, MAX_CONTEXT_EVIDENCE_REFS)),
    evidenceRefCount: evidenceRefIds.length,
    evidenceSetSha256: digest(evidenceRefIds),
    unresolvedFindings: Object.freeze(
      unresolvedFindings.slice(0, MAX_CONTEXT_UNRESOLVED_FINDINGS),
    ),
    unresolvedFindingCount: unresolvedFindings.length,
    unresolvedFindingSetSha256: digest(unresolvedFindings),
    reworkAttemptsUsed,
    reworkAttemptsLimit: input.delivery.spec.maxReworkAttempts,
    repeatedCriterionFailure,
  })
  return JSON.stringify(context)
}

function attentionForCriterion(input: {
  readonly delivery: Delivery
  readonly stageRunId: StageRunId
  readonly result: CriterionResult
  readonly createdAtMillis: number
}): AttentionItem {
  const verdict = input.delivery.verdict!
  const unresolvedFindings = criterionUnresolvedFindings(
    input.result,
    verdict.unresolvedFindings,
  )
  const classification = criterionAction(input.delivery, input.result, unresolvedFindings)
  return parseAttentionItem({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: digestId('attention', {
      verdictId: verdict.id,
      stageRunId: input.stageRunId,
      action: classification.action,
      criterionResultId: input.result.id,
    }),
    deliveryId: input.delivery.id,
    deliverySpecId: input.delivery.spec.id,
    stageRunId: input.stageRunId,
    type: classification.type,
    title: classification.title,
    context: contextFor({
      delivery: input.delivery,
      stageRunId: input.stageRunId,
      action: classification.action,
      result: input.result,
      unresolvedFindings,
    }),
    options: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: classification.action,
      label: classification.label,
      description: classification.description,
    }],
    assignedTo: null,
    blocking: true,
    status: 'open',
    resolution: null,
    resolvedBy: null,
    createdAtMillis: input.createdAtMillis,
    resolvedAtMillis: null,
  })
}

function attentionForUnscopedFinding(input: {
  readonly delivery: Delivery
  readonly stageRunId: StageRunId
  readonly finding: string
  readonly createdAtMillis: number
}): AttentionItem {
  const verdict = input.delivery.verdict!
  const action = 'clarify-scope' as const
  return parseAttentionItem({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: digestId('attention', {
      verdictId: verdict.id,
      stageRunId: input.stageRunId,
      action,
      finding: input.finding,
    }),
    deliveryId: input.delivery.id,
    deliverySpecId: input.delivery.spec.id,
    stageRunId: input.stageRunId,
    type: 'scope_change',
    title: 'A verification finding is outside the approved scope',
    context: contextFor({
      delivery: input.delivery,
      stageRunId: input.stageRunId,
      action,
      result: null,
      unresolvedFindings: [input.finding],
    }),
    options: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: action,
      label: 'Clarify scope',
      description: 'Return to clarification and revise the DeliverySpec before execution.',
    }],
    assignedTo: null,
    blocking: true,
    status: 'open',
    resolution: null,
    resolvedBy: null,
    createdAtMillis: input.createdAtMillis,
    resolvedAtMillis: null,
  })
}

function parseAttentionContext(item: AttentionItem): AttentionContext | null {
  let value: unknown
  try {
    value = JSON.parse(item.context)
  } catch {
    return null
  }
  if (!isRecord(value) || value.protocol !== DELIVERY_ATTENTION_CONTEXT_PROTOCOL) return null
  const expectedKeys = [
    'protocol',
    'verdictId',
    'candidateRef',
    'stageRunId',
    'action',
    'criterionResultId',
    'criterionId',
    'evidenceRefIds',
    'evidenceRefCount',
    'evidenceSetSha256',
    'unresolvedFindings',
    'unresolvedFindingCount',
    'unresolvedFindingSetSha256',
    'reworkAttemptsUsed',
    'reworkAttemptsLimit',
    'repeatedCriterionFailure',
  ]
  if (Object.keys(value).length !== expectedKeys.length
    || expectedKeys.some(key => !Object.hasOwn(value, key))
    || typeof value.verdictId !== 'string'
    || typeof value.candidateRef !== 'string'
    || typeof value.stageRunId !== 'string'
    || typeof value.action !== 'string'
    || !DELIVERY_ATTENTION_ACTIONS.includes(value.action as DeliveryAttentionAction)
    || (value.criterionResultId !== null && typeof value.criterionResultId !== 'string')
    || (value.criterionId !== null && typeof value.criterionId !== 'string')
    || !Array.isArray(value.evidenceRefIds)
    || value.evidenceRefIds.some(entry => typeof entry !== 'string')
    || !Number.isSafeInteger(value.evidenceRefCount)
    || Number(value.evidenceRefCount) < value.evidenceRefIds.length
    || typeof value.evidenceSetSha256 !== 'string'
    || !/^[a-f0-9]{64}$/u.test(value.evidenceSetSha256)
    || !Array.isArray(value.unresolvedFindings)
    || value.unresolvedFindings.some(entry => typeof entry !== 'string')
    || !Number.isSafeInteger(value.unresolvedFindingCount)
    || Number(value.unresolvedFindingCount) < value.unresolvedFindings.length
    || typeof value.unresolvedFindingSetSha256 !== 'string'
    || !/^[a-f0-9]{64}$/u.test(value.unresolvedFindingSetSha256)
    || !Number.isSafeInteger(value.reworkAttemptsUsed)
    || Number(value.reworkAttemptsUsed) < 0
    || !Number.isSafeInteger(value.reworkAttemptsLimit)
    || Number(value.reworkAttemptsLimit) < 0
    || typeof value.repeatedCriterionFailure !== 'boolean') {
    return attentionError(
      'ATTENTION_STALE',
      'delivery outcome Attention context is malformed',
    )
  }
  return immutable(value as unknown as AttentionContext)
}

/** Derive focused business Attention from one current, evidence-bound verdict. */
export function deriveDeliveryVerdictAttention(
  input: DeriveDeliveryVerdictAttentionInput,
): DerivedDeliveryVerdictAttention {
  if (!isRecord(input)
    || typeof input.verificationStageRunId !== 'string'
    || !Number.isSafeInteger(input.createdAtMillis)
    || input.createdAtMillis < 0) {
    return attentionError('INVALID_INPUT', 'delivery Attention input is malformed')
  }
  let delivery: Delivery
  try {
    delivery = parseDelivery(input.delivery)
  } catch (error) {
    return attentionError('INVALID_INPUT', 'delivery Attention requires a valid Delivery', {
      cause: error,
    })
  }
  const verdict = delivery.verdict
  if (verdict === null) {
    return attentionError('VERDICT_MISSING', 'delivery Attention requires a DeliveryVerdict')
  }
  const stageRun = delivery.stageRuns.find(run => run.id === input.verificationStageRunId)
  if (stageRun === undefined
    || stageRun.stage !== 'verifying'
    || stageRun.actorType !== 'codex'
    || !delivery.sessionBindings.some(binding => (
      binding.stageRunId === stageRun.id && binding.codexSessionId !== null
    ))) {
    return attentionError(
      'VERIFICATION_STAGE_MISMATCH',
      'delivery Attention must cite the bound verification StageRun',
    )
  }
  if (input.createdAtMillis < verdict.producedAtMillis) {
    return attentionError(
      'INVALID_INPUT',
      'delivery Attention cannot precede its DeliveryVerdict',
    )
  }
  if (verdict.status === 'pass') {
    return immutable({
      schemaVersion: DELIVERY_ATTENTION_CLASSIFICATION_SCHEMA_VERSION,
      verdictId: verdict.id,
      candidateRef: verdict.candidateRef,
      verificationStageRunId: stageRun.id,
      attentionItems: [],
    })
  }

  const criteriaById = new Map(delivery.spec.acceptanceCriteria.map(criterion => (
    [criterion.id, criterion] as const
  )))
  const attentionItems = verdict.criteria
    .filter(result => criteriaById.get(result.criterionId)?.required === true)
    .filter(result => result.verdict !== 'pass')
    .map(result => attentionForCriterion({
      delivery,
      stageRunId: stageRun.id,
      result,
      createdAtMillis: input.createdAtMillis,
    }))

  for (const finding of verdict.unresolvedFindings.filter(entry => (
    entry.startsWith('unscoped-finding:')
  )).toSorted()) {
    attentionItems.push(attentionForUnscopedFinding({
      delivery,
      stageRunId: stageRun.id,
      finding,
      createdAtMillis: input.createdAtMillis,
    }))
  }

  const hasExistingBlocker = verdict.unresolvedFindings.some(finding => (
    finding.startsWith('blocking-attention:')
  ))
  if (attentionItems.length === 0 && !hasExistingBlocker) {
    return attentionError(
      'ATTENTION_NON_ACTIONABLE',
      'non-passing DeliveryVerdict does not identify a delivery-level next action',
    )
  }
  return immutable({
    schemaVersion: DELIVERY_ATTENTION_CLASSIFICATION_SCHEMA_VERSION,
    verdictId: verdict.id,
    candidateRef: verdict.candidateRef,
    verificationStageRunId: stageRun.id,
    attentionItems: attentionItems.toSorted((left, right) => left.id.localeCompare(right.id)),
  })
}

export function isDerivedDeliveryVerdictAttention(item: AttentionItem): boolean {
  return parseAttentionContext(item) !== null
}

/** Reject stale or caller-invented delivery-outcome Attention before resolution. */
export function assertDeliveryVerdictAttentionCurrent(
  deliveryValue: Delivery,
  itemValue: AttentionItem,
): AttentionItem {
  const delivery = parseDelivery(deliveryValue)
  const item = parseAttentionItem(itemValue)
  const context = parseAttentionContext(item)
  if (context === null) {
    return attentionError(
      'ATTENTION_NON_ACTIONABLE',
      'AttentionItem is not a classified delivery outcome',
    )
  }
  if (delivery.verdict === null
    || context.verdictId !== delivery.verdict.id
    || context.candidateRef !== delivery.verdict.candidateRef
    || context.stageRunId !== item.stageRunId) {
    return attentionError('ATTENTION_STALE', 'delivery outcome Attention is stale')
  }
  const classification = deriveDeliveryVerdictAttention({
    delivery,
    verificationStageRunId: context.stageRunId as StageRunId,
    createdAtMillis: item.createdAtMillis,
  })
  const expected = classification.attentionItems.find(entry => entry.id === item.id)
  if (expected === undefined || JSON.stringify(expected) !== JSON.stringify(item)) {
    return attentionError(
      'ATTENTION_STALE',
      'delivery outcome Attention does not match current verdict facts',
    )
  }
  const expectedIds = classification.attentionItems.map(entry => entry.id).toSorted()
  const currentIds = delivery.attentionItems.filter((entry) => {
    const entryContext = parseAttentionContext(entry)
    return entryContext?.verdictId === delivery.verdict!.id
  }).map(entry => entry.id).toSorted()
  if (JSON.stringify(expectedIds) !== JSON.stringify(currentIds)) {
    return attentionError(
      'ATTENTION_STALE',
      'current verdict does not retain its complete classified Attention set',
    )
  }
  return item
}

/** Determine the one stage reached after all current verdict Attention is acknowledged. */
export function deliveryVerdictAttentionNextStatus(
  deliveryValue: Delivery,
  itemValue: AttentionItem,
  decision: Exclude<AttentionItemStatus, 'open'>,
): DeliveryStatus {
  const delivery = parseDelivery(deliveryValue)
  const item = assertDeliveryVerdictAttentionCurrent(delivery, itemValue)
  if (decision !== 'resolved') {
    return attentionError(
      'ATTENTION_NON_ACTIONABLE',
      'classified delivery outcome Attention must be resolved before stage movement',
    )
  }
  if (delivery.attentionItems.some(entry => (
    entry.id !== item.id && entry.blocking && entry.status === 'open'
  ))) return 'needs-attention'

  const context = parseAttentionContext(item)!
  const classification = deriveDeliveryVerdictAttention({
    delivery,
    verificationStageRunId: context.stageRunId as StageRunId,
    createdAtMillis: item.createdAtMillis,
  })
  const actions = new Set(classification.attentionItems.map(entry => (
    parseAttentionContext(entry)!.action
  )))
  if (actions.has('clarify-scope')) return 'clarifying'
  if (actions.has('start-rework')) return 'reworking'
  return 'verifying'
}
