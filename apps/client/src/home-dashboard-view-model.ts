// SPDX-License-Identifier: Apache-2.0

import {
  createAttentionCenterViewModel,
  orderedAttentionCenterItems,
  type AttentionCenterItem,
  type AttentionCenterViewModel,
  type AttentionCenterViewModelState,
} from './attention-center-view-model.js'
import type { ControlPlaneClient } from './control-plane-client.js'
import type { ScopeRouteSelection } from './core/scope-context.js'
import {
  DEFAULT_HOME_VISIT_LIMIT,
  browserHomeVisitStorage,
  createHomeRecentVisitStore,
  type HomeRecentVisitStore,
  type HomeVisit,
} from './home-recent-visits.js'
import {
  createStrongFlowDeliveryListViewModel,
  type StrongFlowDeliveryListState,
  type StrongFlowDeliveryListViewModel,
} from './strongflow-delivery-list-view-model.js'
import {
  createUsageHealthViewModel,
  type UsageHealthViewModel,
  type UsageHealthViewModelState,
} from './usage-health-view-model.js'
import type {
  Actor,
  ControlPlaneWebSocketSubscriptionId,
  DeliveryId,
  DeliveryProjection,
  DeliveryStatus,
  Instant,
  RepositoryScope,
  RequestId,
  StageRunId,
} from './generated/contracts.js'
import { DeliveryStatus as DeliveryStatusVocabulary } from './generated/contracts.js'

/**
 * UI-504 composes the projections that already exist - the Attention Center,
 * the Delivery list, and the Usage/Worker health summary - into one bounded
 * first screen.  It adds no second business queue, no Portfolio and no new
 * background aggregate: every card is a projection of a server fact.
 */
export type HomeDashboardStatus = 'loading' | 'ready' | 'partial' | 'error' | 'closed'

export type HomeDashboardSource = 'delivery' | 'attention' | 'usage'

export type HomeDashboardSourceState = 'loading' | 'ok' | 'unavailable'

/** One Delivery card: the exact identity the StrongFlow deep link opens. */
export interface HomeDeliveryCard {
  readonly deliveryId: DeliveryId
  readonly title: string
  readonly status: DeliveryStatus
  readonly revision: number
  readonly updatedAt: Instant
  readonly openAttentionCount: number
  readonly activeStageRunId: StageRunId | null
  readonly failedTasks: number
  readonly blockedTasks: number
  readonly activeTasks: number
  readonly verifyingTasks: number
  readonly completedTasks: number
  readonly totalTasks: number
}

/** One Delivery the user opened recently, resolved against the loaded window. */
export interface HomeVisitedCard extends HomeDeliveryCard {
  readonly visitedAt: Instant
}

/** One pending decision: an input, a tool approval, or a business Attention. */
export interface HomeDecisionCard {
  readonly kind: AttentionCenterItem['kind']
  readonly id: string
  readonly title: string
  readonly urgency: AttentionCenterItem['urgency']
  readonly createdAt: Instant | null
  readonly expiresAt: Instant | null
  /** Expired and binding-invalid decisions can no longer be acted on. */
  readonly actionDisabled: boolean
  readonly productSessionId: AttentionCenterItem['productSessionId']
  readonly sessionTitle: string | null
  readonly deliveryId: AttentionCenterItem['deliveryId']
  readonly deliveryTitle: string | null
  readonly stageRunId: AttentionCenterItem['stageRunId']
}

export interface HomeDashboardCounts {
  readonly decisions: number
  readonly active: number
  readonly failing: number
  readonly completed: number
  readonly visited: number
}

export interface HomeDashboardState {
  readonly status: HomeDashboardStatus
  readonly decisions: readonly HomeDecisionCard[]
  readonly active: readonly HomeDeliveryCard[]
  readonly failing: readonly HomeDeliveryCard[]
  readonly completed: readonly HomeDeliveryCard[]
  readonly visited: readonly HomeVisitedCard[]
  readonly counts: HomeDashboardCounts
  readonly sources: Readonly<Record<HomeDashboardSource, HomeDashboardSourceState>>
  /** True only when every projection proves the Scope was never used. */
  readonly firstUse: boolean
}

export interface HomeDashboardLimits {
  readonly decisions: number
  readonly deliveries: number
  readonly visits: number
}

export const DEFAULT_HOME_DASHBOARD_LIMITS: HomeDashboardLimits = Object.freeze({
  decisions: 4,
  deliveries: 4,
  visits: DEFAULT_HOME_VISIT_LIMIT,
})

export interface HomeDashboardViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  /** One scope event subscription, opened by the Attention projection. */
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  /** Browser-only recent Delivery visits; defaults to the local storage store. */
  readonly visits?: HomeRecentVisitStore
  readonly limits?: HomeDashboardLimits
  readonly nowMillis?: () => number
}

export interface HomeDashboardViewModel {
  /** The Usage, Provider and Worker health projection the summary panel mounts. */
  readonly usage: UsageHealthViewModel
  readonly state: HomeDashboardState
  subscribe(listener: (state: HomeDashboardState) => void): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  close(): void
}

/** Deliveries whose work is in motion, including the states waiting on a user. */
const ACTIVE_DELIVERY_STATUSES: readonly DeliveryStatus[] = Object.freeze([
  DeliveryStatusVocabulary.Draft,
  DeliveryStatusVocabulary.Clarifying,
  DeliveryStatusVocabulary.Ready,
  DeliveryStatusVocabulary.Planning,
  DeliveryStatusVocabulary.PlanReview,
  DeliveryStatusVocabulary.Executing,
  DeliveryStatusVocabulary.Verifying,
])

function isActive(delivery: HomeDeliveryCard): boolean {
  return ACTIVE_DELIVERY_STATUSES.includes(delivery.status)
}

function isFailing(delivery: HomeDeliveryCard): boolean {
  return delivery.status === DeliveryStatusVocabulary.NeedsAttention
    || delivery.status === DeliveryStatusVocabulary.Reworking
    || delivery.failedTasks > 0
    || delivery.blockedTasks > 0
}

function isCompleted(delivery: HomeDeliveryCard): boolean {
  return delivery.status === DeliveryStatusVocabulary.Delivered
}

function recency(left: HomeDeliveryCard, right: HomeDeliveryCard): number {
  return right.updatedAt.localeCompare(left.updatedAt)
    || right.deliveryId.localeCompare(left.deliveryId)
}

/** In-progress Deliveries, most recently updated first. */
export function orderedHomeActiveCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return Object.freeze(cards.filter(isActive).sort(recency))
}

/** Failed or blocked Deliveries: hardest failure first, then recency. */
export function orderedHomeFailingCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return Object.freeze(cards.filter(isFailing).sort((left, right) =>
    right.failedTasks - left.failedTasks
    || right.blockedTasks - left.blockedTasks
    || recency(left, right)))
}

/** Recently completed Deliveries, most recent first. */
export function orderedHomeCompletedCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return Object.freeze(cards.filter(isCompleted).sort(recency))
}

/** Project every loaded Delivery summary into the card shape the dashboard renders. */
export function homeDeliveryCards(
  deliveries: readonly DeliveryProjection[],
): readonly HomeDeliveryCard[] {
  return Object.freeze(deliveries.map(delivery => Object.freeze({
    deliveryId: delivery.deliveryId,
    title: delivery.title,
    status: delivery.status,
    revision: delivery.revision,
    updatedAt: delivery.updatedAt,
    openAttentionCount: delivery.openAttentionCount,
    activeStageRunId: delivery.activeStageRunId,
    failedTasks: delivery.taskCounts.failed,
    blockedTasks: delivery.taskCounts.blocked,
    activeTasks: delivery.taskCounts.active,
    verifyingTasks: delivery.taskCounts.verifying,
    completedTasks: delivery.taskCounts.completed,
    totalTasks: delivery.taskCounts.total,
  })))
}

function sourceState(
  status: 'idle' | 'loading' | 'ready' | 'refreshing' | string,
  failed: readonly string[],
): HomeDashboardSourceState {
  if (status === 'loading' || status === 'idle') return 'loading'
  return failed.includes(status) ? 'unavailable' : 'ok'
}

function deliverySourceState(state: StrongFlowDeliveryListState): HomeDashboardSourceState {
  return sourceState(state.status, ['error'])
}

function attentionSourceState(state: AttentionCenterViewModelState): HomeDashboardSourceState {
  return sourceState(state.status, [
    'error',
    'cancelled',
    'authentication-required',
    'authorization-denied',
  ])
}

function usageSourceState(state: UsageHealthViewModelState): HomeDashboardSourceState {
  return sourceState(state.status, [
    'error',
    'cancelled',
    'authentication-required',
    'authorization-denied',
  ])
}

function dashboardStatus(
  sources: Readonly<Record<HomeDashboardSource, HomeDashboardSourceState>>,
): HomeDashboardStatus {
  const values = Object.values(sources)
  if (values.includes('loading')) return 'loading'
  if (sources.delivery === 'unavailable' && sources.attention === 'unavailable') return 'error'
  return values.includes('unavailable') ? 'partial' : 'ready'
}

function visitedCards(
  cards: ReadonlyMap<DeliveryId, HomeDeliveryCard>,
  visits: readonly HomeVisit[],
): readonly HomeVisitedCard[] {
  const visited: HomeVisitedCard[] = []
  for (const visit of visits) {
    const card = cards.get(visit.deliveryId)
    if (card === undefined) continue
    visited.push(Object.freeze({ ...card, visitedAt: visit.at }))
  }
  return Object.freeze(visited)
}

/**
 * Project the three read models into one dashboard snapshot.  Pure, so the
 * section order, bounds and the first-use claim stay testable without a browser.
 */
export function homeDashboardState(input: {
  readonly deliveries: StrongFlowDeliveryListState
  readonly attention: AttentionCenterViewModelState
  readonly usage: UsageHealthViewModelState
  readonly visits: readonly HomeVisit[]
  readonly limits?: HomeDashboardLimits
}): HomeDashboardState {
  const limits = input.limits ?? DEFAULT_HOME_DASHBOARD_LIMITS
  const cards = homeDeliveryCards(input.deliveries.visible)
  const byId = new Map(cards.map(card => [card.deliveryId, card]))
  const active = orderedHomeActiveCards(cards)
  const failing = orderedHomeFailingCards(cards)
  const completed = orderedHomeCompletedCards(cards)
  const decisions = orderedAttentionCenterItems(input.attention.items)
  const visited = visitedCards(byId, input.visits)
  const sources: Readonly<Record<HomeDashboardSource, HomeDashboardSourceState>> = Object.freeze({
    delivery: deliverySourceState(input.deliveries),
    attention: attentionSourceState(input.attention),
    usage: usageSourceState(input.usage),
  })
  return Object.freeze({
    status: dashboardStatus(sources),
    decisions: Object.freeze(decisions.slice(0, limits.decisions).map(item => Object.freeze({
      kind: item.kind,
      id: item.id,
      title: item.title,
      urgency: item.urgency,
      createdAt: item.createdAt,
      expiresAt: item.expiresAt,
      actionDisabled: item.urgency === 'expired' || item.urgency === 'binding-invalid',
      productSessionId: item.productSessionId,
      sessionTitle: item.sessionTitle,
      deliveryId: item.deliveryId,
      deliveryTitle: item.deliveryTitle,
      stageRunId: item.stageRunId,
    }))),
    active: Object.freeze(active.slice(0, limits.deliveries)),
    failing: Object.freeze(failing.slice(0, limits.deliveries)),
    completed: Object.freeze(completed.slice(0, limits.deliveries)),
    visited: Object.freeze(visited.slice(0, limits.visits)),
    counts: Object.freeze({
      decisions: decisions.length,
      active: active.length,
      failing: failing.length,
      completed: completed.length,
      visited: visited.length,
    }),
    sources,
    // First use is a claim about this Scope, so every projection that could
    // contradict it has to be readable before the entry point is offered.
    firstUse: sources.delivery === 'ok'
      && sources.attention === 'ok'
      && input.deliveries.loadedCount === 0
      && input.attention.items.length === 0,
  })
}

function emptyState(): HomeDashboardState {
  return Object.freeze({
    status: 'loading',
    decisions: Object.freeze([]),
    active: Object.freeze([]),
    failing: Object.freeze([]),
    completed: Object.freeze([]),
    visited: Object.freeze([]),
    counts: Object.freeze({
      decisions: 0,
      active: 0,
      failing: 0,
      completed: 0,
      visited: 0,
    }),
    sources: Object.freeze({
      delivery: 'loading',
      attention: 'loading',
      usage: 'loading',
    }),
    firstUse: false,
  })
}

function closedState(): HomeDashboardState {
  return Object.freeze({
    ...emptyState(),
    status: 'closed' as const,
    sources: Object.freeze({
      delivery: 'unavailable',
      attention: 'unavailable',
      usage: 'unavailable',
    }),
  })
}

function scopeSelection(scope: RepositoryScope): ScopeRouteSelection {
  return Object.freeze({
    organizationId: scope.organizationId,
    workspaceId: scope.workspaceId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
  })
}

/** Compose the existing Attention, Delivery and Usage projections into one dashboard. */
export function createHomeDashboardViewModel(
  options: HomeDashboardViewModelOptions,
): HomeDashboardViewModel {
  const limits = options.limits ?? DEFAULT_HOME_DASHBOARD_LIMITS
  const nowMillis = options.nowMillis ?? Date.now
  const selection = scopeSelection(options.scope)
  const visits = options.visits ?? createHomeRecentVisitStore({
    storage: browserHomeVisitStorage(typeof window === 'undefined' ? null : window),
  })
  const attention = createAttentionCenterViewModel({
    client: options.client,
    actor: options.actor,
    scope: options.scope,
    subscriptionId: options.subscriptionId,
    nextRequestId: options.nextRequestId,
    ...(options.nowMillis === undefined ? {} : { nowMillis: options.nowMillis }),
  })
  const deliveries = createStrongFlowDeliveryListViewModel({
    client: options.client,
    actor: options.actor,
    scope: options.scope,
    nextRequestId: options.nextRequestId,
  })
  const usage = createUsageHealthViewModel({
    client: options.client,
    actor: options.actor,
    scope: options.scope,
    nextRequestId: options.nextRequestId,
  })

  const listeners = new Set<(state: HomeDashboardState) => void>()
  let currentState = emptyState()
  let closed = false

  function publish(state: HomeDashboardState): void {
    currentState = state
    for (const listener of listeners) listener(currentState)
  }

  function project(): void {
    if (closed) return
    publish(homeDashboardState({
      deliveries: deliveries.state,
      attention: attention.state,
      usage: usage.state,
      visits: visits.visits(selection, nowMillis()),
      limits,
    }))
  }

  const unsubscribeAttention = attention.subscribe(() => { project() })
  const unsubscribeDeliveries = deliveries.subscribe(() => { project() })
  const unsubscribeUsage = usage.subscribe(() => { project() })

  return {
    get usage() { return usage },
    get state() { return currentState },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async start() {
      await Promise.allSettled([attention.start(), deliveries.start(), usage.start()])
      project()
    },
    async refresh() {
      if (closed) return
      await Promise.allSettled([
        attention.refresh(),
        deliveries.refresh(),
        usage.refresh(),
      ])
      project()
    },
    close() {
      if (closed) return
      closed = true
      unsubscribeAttention()
      unsubscribeDeliveries()
      unsubscribeUsage()
      listeners.clear()
      attention.close()
      deliveries.close()
      usage.close()
      publish(closedState())
    },
  }
}
