// SPDX-License-Identifier: Apache-2.0

import { scopeHash, type ScopeRouteSelection } from './core/scope-context.js'
import type {
  ApprovalProjection,
  DeliveryId,
  DeliveryProjection,
  ProductSessionId,
  StageRunId,
} from './generated/contracts.js'
import { matchesCanonicalSchema } from './generated/control-plane-client.js'
import { strongFlowRouteHash, type StrongFlowRoute } from './strongflow-route.js'

/**
 * UI-506 derives notification-worthy facts from the projections that already
 * exist: pending Approvals and Delivery summaries.  There is no second business
 * queue here, only one stable identity per server event and its state.
 */
export type AttentionSignalKind = 'attention' | 'approval' | 'completion' | 'failure'

export interface AttentionSignal {
  readonly kind: AttentionSignalKind
  /** The authoritative server identity this signal is derived from. */
  readonly id: string
  /** Identity plus the state that makes one notification unique. */
  readonly identity: string
  /** Secret-safe headline: never an Approval subject, prompt, or command body. */
  readonly title: string
  /** Secret-safe context line: the Delivery title, or the decision surface. */
  readonly context: string
  /** How many underlying server entries this signal stands for. */
  readonly weight: number
  readonly revision: number
  readonly deliveryId: DeliveryId | null
  readonly stageRunId: StageRunId | null
  readonly productSessionId: ProductSessionId | null
}

export interface AttentionSignalInput {
  readonly approvals: readonly ApprovalProjection[]
  readonly deliveries: readonly DeliveryProjection[]
  readonly nowMillis: number
}

export interface AttentionSignalBadge {
  readonly total: number
  readonly attention: number
  readonly approval: number
  readonly completion: number
  readonly failure: number
}

const SIGNAL_RANK: Readonly<Record<AttentionSignalKind, number>> = Object.freeze({
  attention: 0,
  approval: 1,
  failure: 2,
  completion: 3,
})

function canonical<Identity extends string>(
  schema: 'ApprovalId' | 'DeliveryId' | 'ProductSessionId' | 'StageRunId',
  value: Identity | null,
): Identity | null {
  return value !== null && matchesCanonicalSchema(schema, value) ? value : null
}

function expired(instant: string, nowMillis: number): boolean {
  const parsed = Date.parse(instant)
  return !Number.isFinite(parsed) || parsed <= nowMillis
}

/** The decision still needs the user only while the Approval is pending and unexpired. */
function approvalIsOpen(projection: ApprovalProjection, nowMillis: number): boolean {
  return projection.state === 'pending' && !expired(projection.expiresAt, nowMillis)
}

function deliveryContext(title: string | null): string {
  return title === null ? 'Delivery · unnamed delivery' : `Delivery · ${title}`
}

function originForApproval(
  projection: ApprovalProjection,
  origins: ReadonlyMap<StageRunId, DeliveryProjection>,
): DeliveryProjection | null {
  const boundStageRunId = canonical(
    'StageRunId',
    projection.binding.sessionIdentity.stageRunId ?? null,
  )
  if (boundStageRunId === null) return null
  return origins.get(boundStageRunId) ?? null
}

/**
 * One notification-worthy fact per pending Approval and per Delivery state that
 * needs the user.  Identities that carry a non-canonical server identifier are
 * dropped, so a notification can never link into a fabricated context.
 */
export function attentionSignals(input: AttentionSignalInput): readonly AttentionSignal[] {
  const origins = new Map<StageRunId, DeliveryProjection>()
  const signals: AttentionSignal[] = []
  for (const delivery of input.deliveries) {
    const deliveryId = canonical('DeliveryId', delivery.deliveryId)
    if (deliveryId === null) continue
    const activeStageRunId = canonical('StageRunId', delivery.activeStageRunId)
    if (activeStageRunId !== null) origins.set(activeStageRunId, delivery)
    if (delivery.status === 'needs-attention' && delivery.openAttentionCount > 0) {
      signals.push(Object.freeze({
        kind: 'attention',
        id: deliveryId,
        identity: `attention:${deliveryId}:${String(delivery.openAttentionCount)}`,
        title: 'Delivery needs attention',
        context: deliveryContext(delivery.title),
        weight: delivery.openAttentionCount,
        revision: delivery.revision,
        deliveryId,
        stageRunId: activeStageRunId,
        productSessionId: null,
      }))
    }
    if (delivery.taskCounts.failed > 0) {
      signals.push(Object.freeze({
        kind: 'failure',
        id: deliveryId,
        identity: `failure:${deliveryId}:${String(delivery.taskCounts.failed)}`,
        title: delivery.taskCounts.failed === 1 ? 'Task failed' : 'Tasks failed',
        context: deliveryContext(delivery.title),
        weight: delivery.taskCounts.failed,
        revision: delivery.revision,
        deliveryId,
        stageRunId: activeStageRunId,
        productSessionId: null,
      }))
    }
    if (delivery.status === 'delivered') {
      signals.push(Object.freeze({
        kind: 'completion',
        id: deliveryId,
        identity: `completion:${deliveryId}:delivered`,
        title: 'Delivery delivered',
        context: deliveryContext(delivery.title),
        weight: 1,
        revision: delivery.revision,
        deliveryId,
        stageRunId: activeStageRunId,
        productSessionId: null,
      }))
    }
  }
  for (const projection of input.approvals) {
    const approvalId = canonical('ApprovalId', projection.id)
    if (approvalId === null || !approvalIsOpen(projection, input.nowMillis)) continue
    const origin = originForApproval(projection, origins)
    const productSessionId = canonical(
      'ProductSessionId',
      projection.binding.productSessionId,
    )
    signals.push(Object.freeze({
      kind: 'approval',
      id: approvalId,
      identity: `approval:${approvalId}:pending`,
      title: 'Tool approval requested',
      context: origin === null ? 'Open the session decisions' : deliveryContext(origin.title),
      weight: 1,
      revision: projection.revision,
      deliveryId: origin === null ? null : canonical('DeliveryId', origin.deliveryId),
      stageRunId: canonical(
        'StageRunId',
        projection.binding.sessionIdentity.stageRunId ?? null,
      ),
      productSessionId,
    }))
  }
  return Object.freeze(signals.sort((left, right) => {
    const rank = SIGNAL_RANK[left.kind] - SIGNAL_RANK[right.kind]
    if (rank !== 0) return rank
    return left.identity.localeCompare(right.identity)
  }))
}

export function attentionSignalBadge(signals: readonly AttentionSignal[]): AttentionSignalBadge {
  const badge = {
    total: 0,
    attention: 0,
    approval: 0,
    completion: 0,
    failure: 0,
  }
  for (const signal of signals) {
    badge[signal.kind] += signal.weight
    badge.total += signal.weight
  }
  return Object.freeze(badge)
}

/** The page title count: `(4) WinWinCode`, or the plain base when nothing needs the user. */
export function attentionSignalsTitle(base: string, badge: AttentionSignalBadge): string {
  return badge.total === 0 ? base : `(${String(badge.total)}) ${base}`
}

/**
 * The exact still-authorized context for one signal: execution facts open the
 * StrongFlow StageRun that raised them, an Approval opens the authoritative
 * decision surface and carries the origin with it.
 */
export function attentionSignalRouteHash(
  signal: AttentionSignal,
  selection: ScopeRouteSelection,
): string {
  if (signal.kind !== 'approval' && signal.deliveryId !== null) {
    const route: StrongFlowRoute = {
      deliveryId: signal.deliveryId,
      productSessionId: null,
      stageRunId: signal.stageRunId,
      candidatePath: null,
      candidateView: 'unified',
      comparison: { status: 'none' },
      evidenceTab: 'evidence',
      evidenceId: null,
    }
    return strongFlowRouteHash(route, selection)
  }
  const parameters = new URLSearchParams()
  if (signal.productSessionId !== null) parameters.set('session', signal.productSessionId)
  if (signal.kind === 'approval' && signal.deliveryId !== null && signal.stageRunId !== null) {
    parameters.set('delivery', signal.deliveryId)
    parameters.set('stageRun', signal.stageRunId)
  }
  const query = parameters.toString()
  return scopeHash(query.length === 0 ? '#/attention' : `#/attention?${query}`, selection)
}

export interface AttentionSignalGate {
  /** Return only the signals whose identity has never been admitted before. */
  admit(signals: readonly AttentionSignal[]): readonly AttentionSignal[]
  /** Drop one identity so a returning state can notify again. */
  forget(identity: string): void
  known(): readonly string[]
  close(): void
}

/** Remembers admitted identities only; it holds no business state and no queue. */
export function createAttentionSignalGate(): AttentionSignalGate {
  const known = new Set<string>()
  const gate: AttentionSignalGate = {
    admit(signals) {
      const admitted: AttentionSignal[] = []
      for (const signal of signals) {
        if (known.has(signal.identity)) continue
        known.add(signal.identity)
        admitted.push(signal)
      }
      return Object.freeze(admitted)
    },
    forget(identity) { known.delete(identity) },
    known() { return Object.freeze([...known]) },
    close() { known.clear() },
  }
  return Object.freeze(gate)
}
