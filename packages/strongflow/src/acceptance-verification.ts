import { createHash } from 'node:crypto'

import {
  parseDelivery,
  type AcceptanceCriterion,
  type AttentionItemId,
  type Delivery,
  type DeliveryId,
  type DeliverySpecId,
  type SessionBindingId,
  type StageRunId,
} from '@winwincode/contracts'

export const ACCEPTANCE_VERIFICATION_SCHEMA_VERSION = 1 as const

export type AcceptanceVerificationErrorCode =
  | 'INVALID_DELIVERY'
  | 'ACCEPTANCE_NOT_APPROVED'
  | 'APPROVAL_SESSION_UNBOUND'
  | 'APPROVAL_AMBIGUOUS'
  | 'ACCEPTANCE_INPUT_STALE'

export class AcceptanceVerificationError extends Error {
  readonly code: AcceptanceVerificationErrorCode

  constructor(
    code: AcceptanceVerificationErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'AcceptanceVerificationError'
    this.code = code
  }
}

export interface DeclaredAcceptanceEvidenceRequirement {
  readonly kind: 'declared-check'
  readonly verificationMethod: string
}

export interface AttentionAcceptanceEvidenceRequirement {
  readonly kind: 'attention-required'
  readonly attentionType: 'verification_blocked'
  readonly reason: 'verification_method_missing'
}

export type AcceptanceEvidenceRequirement =
  | DeclaredAcceptanceEvidenceRequirement
  | AttentionAcceptanceEvidenceRequirement

export interface AcceptanceVerificationCriterion {
  /** Exact current DeliverySpec criterion; this projection does not rewrite it. */
  readonly criterion: AcceptanceCriterion
  readonly evidenceRequirement: AcceptanceEvidenceRequirement
}

export interface AcceptanceApprovalReference {
  readonly attentionItemId: AttentionItemId
  readonly stageRunId: StageRunId
  readonly sessionBindingIds: readonly SessionBindingId[]
  readonly approvedBy: string
  readonly approvedAtMillis: number
  readonly resolution: string
}

/**
 * Immutable verifier input derived from the current Delivery facts. It is not
 * an eleventh persisted domain object and cannot change Delivery state.
 */
export interface AcceptanceVerificationInput {
  readonly schemaVersion: typeof ACCEPTANCE_VERIFICATION_SCHEMA_VERSION
  readonly freezeId: string
  readonly deliveryId: DeliveryId
  readonly deliverySpecId: DeliverySpecId
  readonly deliverySpecRevision: number
  readonly approval: AcceptanceApprovalReference
  readonly criteria: readonly AcceptanceVerificationCriterion[]
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

function parsedDelivery(value: Delivery): Delivery {
  try {
    return parseDelivery(value)
  } catch (error) {
    throw new AcceptanceVerificationError(
      'INVALID_DELIVERY',
      'acceptance verification requires a valid Delivery',
      { cause: error },
    )
  }
}

function approvedPlanReview(delivery: Delivery): AcceptanceApprovalReference {
  const runs = new Map(delivery.stageRuns.map(run => [run.id, run]))
  const approvals = delivery.attentionItems.flatMap((item) => {
    if (item.type !== 'decision_required'
      || item.status !== 'resolved'
      || item.deliverySpecId !== delivery.spec.id
      || item.stageRunId === null
      || item.resolvedBy === null
      || item.resolvedAtMillis === null
      || item.resolution === null) return []
    const run = runs.get(item.stageRunId)
    if (run?.stage !== 'plan-review'
      || run.actorType !== 'human'
      || run.status !== 'succeeded') return []
    const bindingIds = delivery.sessionBindings
      .filter(binding => binding.stageRunId === run.id && binding.dshSessionId !== null)
      .map(binding => binding.id)
      .sort()
    if (bindingIds.length === 0) {
      throw new AcceptanceVerificationError(
        'APPROVAL_SESSION_UNBOUND',
        `approved plan review ${run.id} has no bound DSH session`,
      )
    }
    return [Object.freeze({
      attentionItemId: item.id,
      stageRunId: run.id,
      sessionBindingIds: Object.freeze(bindingIds),
      approvedBy: item.resolvedBy,
      approvedAtMillis: item.resolvedAtMillis,
      resolution: item.resolution,
    })]
  })
  if (approvals.length === 0) {
    throw new AcceptanceVerificationError(
      'ACCEPTANCE_NOT_APPROVED',
      'the current DeliverySpec has no approved human plan review',
    )
  }
  if (approvals.length > 1) {
    throw new AcceptanceVerificationError(
      'APPROVAL_AMBIGUOUS',
      'the current DeliverySpec has more than one approved plan review',
    )
  }
  return approvals[0]!
}

function evidenceRequirement(criterion: AcceptanceCriterion): AcceptanceEvidenceRequirement {
  return criterion.verificationMethod === null
    ? Object.freeze({
        kind: 'attention-required',
        attentionType: 'verification_blocked',
        reason: 'verification_method_missing',
      })
    : Object.freeze({
        kind: 'declared-check',
        verificationMethod: criterion.verificationMethod,
      })
}

function freezeIdentity(value: Omit<AcceptanceVerificationInput, 'freezeId'>): string {
  return `sha256:${createHash('sha256').update(JSON.stringify(value)).digest('hex')}`
}

/**
 * Freeze the exact current criteria after the human plan-review decision.
 * Missing verification methods remain visible Attention requirements.
 */
export function freezeAcceptanceVerificationInput(
  deliveryValue: Delivery,
): AcceptanceVerificationInput {
  const delivery = parsedDelivery(deliveryValue)
  const approval = approvedPlanReview(delivery)
  const criteria = delivery.spec.acceptanceCriteria.map(criterion => Object.freeze({
    criterion,
    evidenceRequirement: evidenceRequirement(criterion),
  }))
  const unsigned = Object.freeze({
    schemaVersion: ACCEPTANCE_VERIFICATION_SCHEMA_VERSION,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    deliverySpecRevision: delivery.spec.revision,
    approval,
    criteria: Object.freeze(criteria),
  })
  return immutable({ ...unsigned, freezeId: freezeIdentity(unsigned) })
}

/** Reject a changed or superseded verifier input before evidence evaluation. */
export function assertAcceptanceVerificationInputCurrent(
  deliveryValue: Delivery,
  input: AcceptanceVerificationInput,
): AcceptanceVerificationInput {
  const delivery = parsedDelivery(deliveryValue)
  if (input.deliveryId !== delivery.id
    || input.deliverySpecId !== delivery.spec.id
    || input.deliverySpecRevision !== delivery.spec.revision) {
    throw new AcceptanceVerificationError(
      'ACCEPTANCE_INPUT_STALE',
      'acceptance verification input does not match the current DeliverySpec',
    )
  }
  let current: AcceptanceVerificationInput
  try {
    current = freezeAcceptanceVerificationInput(delivery)
  } catch (error) {
    if (error instanceof AcceptanceVerificationError
      && error.code !== 'INVALID_DELIVERY') {
      throw new AcceptanceVerificationError(
        'ACCEPTANCE_INPUT_STALE',
        'the approval behind this acceptance verification input is no longer current',
        { cause: error },
      )
    }
    throw error
  }
  if (JSON.stringify(input) !== JSON.stringify(current)) {
    throw new AcceptanceVerificationError(
      'ACCEPTANCE_INPUT_STALE',
      'acceptance verification input was changed after it was frozen',
    )
  }
  return current
}
