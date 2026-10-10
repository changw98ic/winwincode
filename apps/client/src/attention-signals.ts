import { scopeHash, type ScopeRouteSelection } from '@winwincode/browser-core/scope-context'
import type {
  ApprovalProjection,
  DeliveryId,
  DeliveryProjection,
  ProductSessionId,
  WorkRunId,
} from './generated/contracts.js'
import { matchesCanonicalSchema } from './generated/control-plane-client.js'

export type AttentionSignalKind = 'attention' | 'approval' | 'completion' | 'failure'

export interface AttentionSignal {
  readonly kind: AttentionSignalKind
  readonly id: string
  readonly identity: string
  readonly title: string
  readonly context: string
  readonly weight: number
  readonly revision: number
  readonly deliveryId: DeliveryId | null
  readonly workRunId: WorkRunId | null
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
  schema: 'ApprovalId' | 'DeliveryId' | 'ProductSessionId' | 'WorkRunId',
  value: Identity | null,
): Identity | null {
  return value !== null && matchesCanonicalSchema(schema, value) ? value : null
}

function expired(instant: string, nowMillis: number): boolean {
  const parsed = Date.parse(instant)
  return !Number.isFinite(parsed) || parsed <= nowMillis
}

function approvalIsOpen(projection: ApprovalProjection, nowMillis: number): boolean {
  return projection.state === 'pending' && !expired(projection.expiresAt, nowMillis)
}

function deliveryContext(title: string | null): string {
  return title === null ? '交付 · 未命名交付' : `交付 · ${title}`
}

function originForApproval(
  projection: ApprovalProjection,
  origins: ReadonlyMap<WorkRunId, DeliveryProjection>,
): DeliveryProjection | null {
  const boundWorkRunId = canonical(
    'WorkRunId',
    projection.binding.sessionIdentity.workRunId ?? null,
  )
  if (boundWorkRunId === null) return null
  return origins.get(boundWorkRunId) ?? null
}

export function attentionSignals(input: AttentionSignalInput): readonly AttentionSignal[] {
  const origins = new Map<WorkRunId, DeliveryProjection>()
  const signals: AttentionSignal[] = []
  for (const delivery of input.deliveries) {
    const deliveryId = canonical('DeliveryId', delivery.deliveryId)
    if (deliveryId === null) continue
    const activeWorkRunId = canonical('WorkRunId', delivery.activeWorkRunId ?? null)
    if (activeWorkRunId !== null) origins.set(activeWorkRunId, delivery)
    if (delivery.status === 'waiting_human' && delivery.openAttentionCount > 0) {
      signals.push(Object.freeze({
        kind: 'attention',
        id: deliveryId,
        identity: `attention:${deliveryId}:${String(delivery.openAttentionCount)}`,
        title: '交付需要处理',
        context: deliveryContext(delivery.title),
        weight: delivery.openAttentionCount,
        revision: delivery.revision,
        deliveryId,
        workRunId: activeWorkRunId,
        productSessionId: null,
      }))
    }
    if (delivery.workItemCounts.failed > 0) {
      signals.push(Object.freeze({
        kind: 'failure',
        id: deliveryId,
        identity: `failure:${deliveryId}:${String(delivery.workItemCounts.failed)}`,
        title: delivery.workItemCounts.failed === 1 ? '任务失败' : '多项任务失败',
        context: deliveryContext(delivery.title),
        weight: delivery.workItemCounts.failed,
        revision: delivery.revision,
        deliveryId,
        workRunId: activeWorkRunId,
        productSessionId: null,
      }))
    }
    if (delivery.status === 'done') {
      signals.push(Object.freeze({
        kind: 'completion',
        id: deliveryId,
        identity: `completion:${deliveryId}:done`,
        title: '交付已完成',
        context: deliveryContext(delivery.title),
        weight: 1,
        revision: delivery.revision,
        deliveryId,
        workRunId: activeWorkRunId,
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
      title: '工具调用等待批准',
      context: origin === null ? '打开会话决策' : deliveryContext(origin.title),
      weight: 1,
      revision: projection.revision,
      deliveryId: origin === null ? null : canonical('DeliveryId', origin.deliveryId),
      workRunId: canonical(
        'WorkRunId',
        projection.binding.sessionIdentity.workRunId ?? null,
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

export function attentionSignalsTitle(base: string, badge: AttentionSignalBadge): string {
  return badge.total === 0 ? base : `(${String(badge.total)}) ${base}`
}

export function attentionSignalRouteHash(selection: ScopeRouteSelection): string {
  return scopeHash('#/home?filter=attention', selection)
}

export interface AttentionSignalGate {
  admit(signals: readonly AttentionSignal[]): readonly AttentionSignal[]
  forget(identity: string): void
  known(): readonly string[]
  close(): void
}

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
