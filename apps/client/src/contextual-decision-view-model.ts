// SPDX-License-Identifier: Apache-2.0

import type {
  ApprovalProjection,
  ChatInputInteractionProjection,
  DeliveryAttentionProjection,
  DeliveryId,
  Instant,
  InteractiveInputMode,
  ProductSessionId,
  StageRunId,
} from './generated/contracts.js'

/**
 * UI-502 embeds the decisions of the current Session/StageRun context where the
 * user already is.  This module is a pure projection of one server snapshot: it
 * never queries, never caches, and never owns business state, so the global
 * Attention Center, Chat, and StrongFlow keep exactly one shared server truth.
 */
export type ContextualDecisionKind = 'input' | 'approval' | 'attention'

export type ContextualDecisionUrgency =
  | 'blocking'
  | 'pending'
  | 'expired'
  | 'binding-invalid'

/**
 * One offered answer.  An Input option carries the canonical `value` the Worker
 * expects; a Delivery Attention option describes a choice and carries none.
 */
export interface ContextualDecisionOption {
  readonly id: string
  readonly label: string
  readonly value: string | null
}

/** One Delivery Attention item together with the exact Delivery binding. */
export interface ContextualDecisionAttention {
  readonly projection: DeliveryAttentionProjection
  readonly deliveryId: DeliveryId
  readonly deliveryRevision: number
}

/** The page-owned snapshot slice a contextual decision card projects. */
export interface ContextualDecisionSource {
  readonly inputs: readonly ChatInputInteractionProjection[]
  readonly approvals: readonly ApprovalProjection[]
  readonly attention: readonly ContextualDecisionAttention[]
  readonly nowMillis: number
  /** Highest number of decisions one card renders; the rest are counted. */
  readonly limit?: number
}

export interface ContextualDecisionItem {
  readonly kind: ContextualDecisionKind
  /** Stable server identity: inputRequestId, ApprovalId, or AttentionItemId. */
  readonly id: string
  /** Producer text: never rendered raw, the card binds it before display. */
  readonly title: string
  readonly urgency: ContextualDecisionUrgency
  readonly blocking: boolean
  readonly expired: boolean
  readonly bindingValid: boolean
  readonly createdAt: Instant | null
  readonly expiresAt: Instant | null
  readonly revision: number
  readonly productSessionId: ProductSessionId | null
  readonly stageRunId: StageRunId | null
  readonly deliveryId: DeliveryId | null
  /** Input answers only; every other kind stays null. */
  readonly mode: InteractiveInputMode | null
  /** Input answers only: an empty text response is a valid answer. */
  readonly allowEmpty: boolean
  readonly options: readonly ContextualDecisionOption[]
  /** True when the decision must carry a written reason or resolution. */
  readonly requiresNote: boolean
}

export interface ContextualDecisionCounts {
  readonly blocking: number
  readonly pending: number
  readonly expired: number
  readonly bindingInvalid: number
}

export interface ContextualDecisionView {
  readonly items: readonly ContextualDecisionItem[]
  readonly omitted: number
  readonly counts: ContextualDecisionCounts
}

export interface ContextualDecisionPresentationOptions {
  /** The page is still loading or replacing its snapshot. */
  readonly loading?: boolean
  /** One decision command is already in flight for this page. */
  readonly busy?: boolean
  /** Access, transport, or lifecycle state that forbids every decision. */
  readonly pageUnavailable?: boolean
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
}

export interface ContextualDecisionPresentation {
  readonly statusText: string
  readonly decisionsDisabled: boolean
  readonly counts: ContextualDecisionCounts
}

/** Render limit shared by Chat and StrongFlow: the card stays first-screen sized. */
export const DEFAULT_CONTEXTUAL_DECISION_LIMIT = 4

const URGENCY_RANK: Readonly<Record<ContextualDecisionUrgency, number>> = Object.freeze({
  blocking: 0,
  pending: 1,
  expired: 2,
  'binding-invalid': 3,
})

/** Same fail-closed binding check as the Attention Center and local decisions. */
function bindingIsComplete(binding: {
  readonly productSessionId: ProductSessionId
  readonly sessionIdentity: {
    readonly productSessionId: ProductSessionId
    readonly workerSessionId: string
  }
  readonly workerSessionId: string
}): boolean {
  return binding.productSessionId === binding.sessionIdentity.productSessionId
    && binding.workerSessionId === binding.sessionIdentity.workerSessionId
}

function expired(instant: string, nowMillis: number): boolean {
  const parsed = Date.parse(instant)
  return !Number.isFinite(parsed) || parsed <= nowMillis
}

function urgencyOf(item: {
  readonly bindingValid: boolean
  readonly expired: boolean
  readonly blocking: boolean
}): ContextualDecisionUrgency {
  if (!item.bindingValid) return 'binding-invalid'
  if (item.expired) return 'expired'
  if (item.blocking) return 'blocking'
  return 'pending'
}

function inputDecision(
  projection: ChatInputInteractionProjection,
  nowMillis: number,
): ContextualDecisionItem {
  const bindingValid = bindingIsComplete(projection.binding)
  const options: readonly ContextualDecisionOption[] = projection.options.map(option =>
    Object.freeze({ id: option.id, label: option.label, value: option.value })
  )
  const isExpired = projection.state !== 'pending' || expired(projection.expiresAt, nowMillis)
  const item: ContextualDecisionItem = {
    kind: 'input',
    id: projection.inputRequestId,
    title: projection.prompt,
    urgency: 'pending',
    blocking: false,
    expired: isExpired,
    bindingValid,
    createdAt: null,
    expiresAt: projection.expiresAt,
    revision: projection.revision,
    productSessionId: projection.binding.productSessionId,
    stageRunId: projection.binding.sessionIdentity.stageRunId ?? null,
    deliveryId: null,
    mode: projection.mode,
    allowEmpty: projection.allowEmpty,
    options,
    requiresNote: false,
  }
  return Object.freeze({ ...item, urgency: urgencyOf(item) })
}

function approvalDecision(
  projection: ApprovalProjection,
  nowMillis: number,
): ContextualDecisionItem {
  const bindingValid = bindingIsComplete(projection.binding)
  const isExpired = projection.state !== 'pending' || expired(projection.expiresAt, nowMillis)
  const item: ContextualDecisionItem = {
    kind: 'approval',
    id: projection.id,
    title: projection.subject,
    urgency: 'pending',
    blocking: true,
    expired: isExpired,
    bindingValid,
    createdAt: projection.requestedAt,
    expiresAt: projection.expiresAt,
    revision: projection.revision,
    productSessionId: projection.binding.productSessionId,
    stageRunId: projection.binding.sessionIdentity.stageRunId ?? null,
    deliveryId: null,
    mode: null,
    allowEmpty: false,
    options: Object.freeze([]),
    requiresNote: true,
  }
  return Object.freeze({ ...item, urgency: urgencyOf(item) })
}

function attentionDecision(
  source: ContextualDecisionAttention,
): ContextualDecisionItem {
  const projection = source.projection
  // The card is a display projection: a producer that omits the option list
  // loses its choices here instead of taking the mounted page down.
  const options: readonly ContextualDecisionOption[] = (projection.options ?? []).map(option =>
    Object.freeze({ id: option.id, label: option.label, value: null })
  )
  const item: ContextualDecisionItem = {
    kind: 'attention',
    id: projection.id,
    title: projection.title,
    urgency: 'pending',
    blocking: projection.blocking,
    expired: false,
    bindingValid: projection.status === 'open',
    createdAt: projection.createdAt,
    expiresAt: null,
    revision: source.deliveryRevision,
    productSessionId: null,
    stageRunId: projection.stageRunId,
    deliveryId: source.deliveryId,
    mode: null,
    allowEmpty: false,
    options,
    requiresNote: true,
  }
  return Object.freeze({ ...item, urgency: urgencyOf(item) })
}

/** Canonical browsing order: blocking first, soonest expiry next, stable identity. */
export function orderedContextualDecisions(
  items: readonly ContextualDecisionItem[],
): readonly ContextualDecisionItem[] {
  return Object.freeze([...items].sort((left, right) => {
    const rank = URGENCY_RANK[left.urgency] - URGENCY_RANK[right.urgency]
    if (rank !== 0) return rank
    const leftExpiry = left.expiresAt === null
      ? Number.POSITIVE_INFINITY
      : Date.parse(left.expiresAt)
    const rightExpiry = right.expiresAt === null
      ? Number.POSITIVE_INFINITY
      : Date.parse(right.expiresAt)
    if (leftExpiry !== rightExpiry) return leftExpiry - rightExpiry
    if (left.kind !== right.kind) return left.kind.localeCompare(right.kind)
    return left.id.localeCompare(right.id)
  }))
}

export interface BoundedContextualDecisions {
  readonly items: readonly ContextualDecisionItem[]
  readonly omitted: number
}

/** One bounded card slice: identity is stable, so keyed rows never reshuffle. */
export function boundedContextualDecisions(
  items: readonly ContextualDecisionItem[],
  limit: number,
): BoundedContextualDecisions {
  const ordered = orderedContextualDecisions(items)
  if (limit <= 0) return Object.freeze({ items: Object.freeze([]), omitted: ordered.length })
  if (ordered.length <= limit) {
    return Object.freeze({ items: ordered, omitted: 0 })
  }
  return Object.freeze({
    items: Object.freeze(ordered.slice(0, limit)),
    omitted: ordered.length - limit,
  })
}

/** Project one page-owned snapshot slice into the contextual decision card facts. */
export function contextualDecisions(source: ContextualDecisionSource): ContextualDecisionView {
  const limit = source.limit ?? DEFAULT_CONTEXTUAL_DECISION_LIMIT
  const decisions = [
    ...source.inputs.map(projection => inputDecision(projection, source.nowMillis)),
    ...source.approvals.map(projection => approvalDecision(projection, source.nowMillis)),
    ...source.attention.map(attentionDecision),
  ]
  const bounded = boundedContextualDecisions(decisions, limit)
  return Object.freeze({
    items: bounded.items,
    omitted: bounded.omitted,
    counts: Object.freeze({
      blocking: decisions.filter(item => item.urgency === 'blocking').length,
      pending: decisions.filter(item => item.urgency === 'pending').length,
      expired: decisions.filter(item => item.urgency === 'expired').length,
      bindingInvalid: decisions.filter(item => item.urgency === 'binding-invalid').length,
    }),
  })
}

/** Map one card snapshot to the visible, non-live card text and capability. */
export function contextualDecisionPresentation(
  view: ContextualDecisionView,
  options: ContextualDecisionPresentationOptions = {},
): ContextualDecisionPresentation {
  const waiting = view.counts.blocking + view.counts.pending
  const statusText = options.loading === true
    ? 'Loading decisions…'
    : waiting === 0
      ? 'No decision is waiting on you in this context'
      : view.omitted > 0
        ? `${String(waiting)} need a decision · ${String(view.omitted)} more not shown`
        : `${String(waiting)} need a decision`
  return Object.freeze({
    statusText,
    decisionsDisabled: options.readOnly === true
      || options.pageUnavailable === true
      || options.busy === true,
    counts: view.counts,
  })
}

/** Whether one row keeps its controls reachable, and the visible state label. */
export function contextualDecisionCapability(
  item: ContextualDecisionItem,
  presentation: ContextualDecisionPresentation,
): { readonly disabled: boolean; readonly stateLabel: string } {
  const disabled = presentation.decisionsDisabled
    || item.urgency === 'expired'
    || item.urgency === 'binding-invalid'
  const stateLabel = item.urgency === 'blocking'
    ? 'Blocking · needs a decision now'
    : item.urgency === 'pending'
      ? 'Needs a decision'
      : item.urgency === 'expired'
        ? 'Expired · decision disabled'
        : 'Binding invalid · decision disabled'
  return Object.freeze({ disabled, stateLabel })
}

/** The kind label of one decision row; never derived from producer text. */
export function contextualDecisionKindLabel(kind: ContextualDecisionKind): string {
  if (kind === 'input') return 'Input'
  if (kind === 'approval') return 'Tool approval'
  return 'Business Attention'
}
