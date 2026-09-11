// SPDX-License-Identifier: Apache-2.0

import type { ScopeRouteSelection } from '@winwincode/browser-core/scope-context'
import {
  DEFAULT_HOME_VISIT_LIMIT,
  browserHomeVisitStorage,
  createHomeRecentVisitStore,
  type HomeRecentVisitStore,
  type HomeVisit,
} from './home-recent-visits.js'
import {
  createAttentionCenterViewModel,
  orderedAttentionCenterItems,
  type AttentionCenterItem,
  type AttentionCenterViewModel,
  type AttentionCenterViewModelState,
} from './attention-center-view-model.js'
import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './community-control-plane-client.js'
import {
  createUsageHealthViewModel,
  type UsageHealthViewModel,
  type UsageHealthViewModelState,
} from './usage-health-view-model.js'
import type {
  Actor,
  ControlPlaneWebSocketSubscriptionId,
  DeliveryId,
  DeliveryListResultResponse,
  DeliveryProjection,
  DeliveryStatus,
  Instant,
  OpaqueCursor,
  RepositoryScope,
  RequestId,
  WorkRunId,
} from './generated/contracts.js'
import { DeliveryStatus as DeliveryStatusVocabulary, QueryName } from './generated/contracts.js'

/**
 * UI-504 composes the projections that already exist - the Attention Center,
 * the Delivery list, and the Usage/Worker health summary - into one bounded
 * first screen.  It adds no second business queue, no Portfolio and no new
 * background aggregate: every card is a projection of a server fact.
 */
export type HomeDashboardStatus = 'loading' | 'ready' | 'partial' | 'error' | 'closed'

export type HomeDashboardSource = 'delivery' | 'attention' | 'usage'

export type HomeDashboardSourceState = 'loading' | 'ok' | 'unavailable'

/** One bounded server page of the board's Delivery list projection. */
const DELIVERY_LIST_PAGE_LIMIT = 50
const DELIVERY_LIST_MAX_PAGES = 10

export type HomeDeliveryListStatus = 'loading' | 'ready' | 'refreshing' | 'error'

/**
 * The one Delivery read model the board mounts: a bounded, recent-first window
 * of the Scope's Deliveries.  A first load that fails has nothing to show
 * (`error`); a failed refresh keeps the loaded window.
 */
export interface HomeDeliveryListState {
  readonly status: HomeDeliveryListStatus
  readonly visible: readonly DeliveryProjection[]
  readonly loadedCount: number
}

export interface HomeDeliveryListViewModel {
  readonly state: HomeDeliveryListState
  subscribe(listener: (state: HomeDeliveryListState) => void): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  close(): void
}

interface HomeDeliveryListOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly nextRequestId: () => RequestId
}

/**
 * Load one consistent recent-first snapshot through bounded server pages.
 * Refresh swaps the whole loaded window only after the rebuild completes; a
 * first load publishes growing prefixes of the same page chain.
 */
function createHomeDeliveryListViewModel(
  options: HomeDeliveryListOptions,
): HomeDeliveryListViewModel {
  let listener: ((state: HomeDeliveryListState) => void) | null = null
  let loaded: readonly DeliveryProjection[] = []
  let status: HomeDeliveryListStatus = 'loading'
  let generation = 0
  let closed = false
  let rebuildChain: Promise<void> = Promise.resolve()

  function snapshot(): HomeDeliveryListState {
    return Object.freeze({
      status,
      visible: loaded,
      loadedCount: loaded.length,
    })
  }

  function publish(): void {
    listener?.(snapshot())
  }

  function failLoad(): void {
    status = 'error'
    publish()
  }

  async function rebuild(ownGeneration: number, firstLoad: boolean): Promise<void> {
    const collected: DeliveryProjection[] = []
    const seenIds = new Set<string>()
    const seenCursors = new Set<string>()
    let pageCursor: OpaqueCursor | null = null
    for (let pageIndex = 0; ; pageIndex += 1) {
      if (pageIndex >= DELIVERY_LIST_MAX_PAGES) {
        if (firstLoad) loaded = collected
        failLoad()
        return
      }
      let response: DeliveryListResultResponse
      try {
        response = await options.client.query({
          schemaVersion: 'winwincode/v1',
          requestId: options.nextRequestId(),
          actor: options.actor,
          scope: options.scope,
          query: QueryName.DeliveryList,
          parameters: { states: [] },
          page: { cursor: pageCursor, limit: DELIVERY_LIST_PAGE_LIMIT },
        }) as DeliveryListResultResponse
      } catch {
        if (superseded(ownGeneration)) return
        if (firstLoad) loaded = collected
        failLoad()
        return
      }
      if (superseded(ownGeneration)) return
      if (response.query !== QueryName.DeliveryList) {
        if (firstLoad) loaded = collected
        failLoad()
        return
      }
      for (const item of response.result.items) {
        if (seenIds.has(item.deliveryId)) continue
        seenIds.add(item.deliveryId)
        collected.push(item)
      }
      const page = response.page
      if (!page.hasMore) {
        pageCursor = null
      } else if (page.nextCursor !== null && !seenCursors.has(page.nextCursor)) {
        pageCursor = page.nextCursor
        seenCursors.add(pageCursor)
      } else {
        if (firstLoad) loaded = collected
        failLoad()
        return
      }
      if (firstLoad) {
        loaded = collected
        publish()
      }
      if (pageCursor === null) break
    }
    loaded = collected
    status = 'ready'
    publish()
  }

  function superseded(ownGeneration: number): boolean {
    return closed || ownGeneration !== generation
  }

  function scheduleRebuild(nextStatus: 'loading' | 'refreshing'): Promise<void> {
    generation += 1
    status = nextStatus
    publish()
    const firstLoad = nextStatus === 'loading'
    const chain = rebuildChain
    const run = (async () => {
      await chain.catch(() => undefined)
      if (superseded(generation)) return
      await rebuild(generation, firstLoad)
    })()
    rebuildChain = run
    return run
  }

  return {
    get state() {
      return snapshot()
    },
    subscribe(nextListener) {
      listener = nextListener
      nextListener(snapshot())
      return () => {
        if (listener === nextListener) listener = null
      }
    },
    start() {
      if (closed) return Promise.resolve()
      return scheduleRebuild('loading')
    },
    refresh() {
      if (closed) return Promise.resolve()
      return scheduleRebuild('refreshing')
    },
    close() {
      if (closed) return
      closed = true
      generation += 1
      listener = null
    },
  }
}

function deliveryListSourceState(status: string): HomeDashboardSourceState {
  return status === 'loading' ? 'loading' : status === 'error' ? 'unavailable' : 'ok'
}


/** One Delivery card: the exact identity the StrongFlow deep link opens. */
export interface HomeDeliveryCard {
  readonly deliveryId: DeliveryId
  readonly title: string
  readonly status: DeliveryStatus
  readonly revision: number
  readonly updatedAt: Instant
  readonly openAttentionCount: number
  readonly activeWorkRunId: WorkRunId | null
  readonly failedTasks: number
  readonly blockedTasks: number
  readonly activeTasks: number
  readonly verifyingTasks: number
  readonly completedTasks: number
  readonly totalTasks: number
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
  readonly workRunId: AttentionCenterItem['workRunId']
}

export interface HomeDashboardCounts {
  readonly decisions: number
  readonly active: number
  readonly failing: number
  readonly completed: number
  readonly visited: number
}

/** 设计稿 04 折叠行之外的浏览器本地区块:最近打开过的交付。 */
export interface HomeVisitedCard extends HomeDeliveryCard {
  readonly visitedAt: Instant
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
    activeWorkRunId: delivery.activeWorkRunId ?? null,
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

function deliverySourceState(state: HomeDeliveryListState): HomeDashboardSourceState {
  return deliveryListSourceState(state.status)
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

/**
 * Project the three read models into one dashboard snapshot.  Pure, so the
 * section order, bounds and the first-use claim stay testable without a browser.
 */
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

export function homeDashboardState(input: {
  readonly deliveries: HomeDeliveryListState
  readonly attention: AttentionCenterViewModelState
  readonly usage: UsageHealthViewModelState
  readonly visits?: readonly HomeVisit[]
  readonly limits?: HomeDashboardLimits
}): HomeDashboardState {
  const limits = input.limits ?? DEFAULT_HOME_DASHBOARD_LIMITS
  const cards = homeDeliveryCards(input.deliveries.visible)
  const byId = new Map(cards.map(card => [card.deliveryId, card] as const))
  const active = orderedHomeActiveCards(cards)
  const failing = orderedHomeFailingCards(cards)
  const completed = orderedHomeCompletedCards(cards)
  const visited = visitedCards(byId, input.visits ?? [])
  const decisions = orderedAttentionCenterItems(input.attention.items)
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
      workRunId: item.workRunId,
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

/** Compose the existing Attention, Delivery and Usage projections into one dashboard. */
function scopeSelection(scope: RepositoryScope): ScopeRouteSelection {
  return Object.freeze({
    organizationId: scope.organizationId,
    workspaceId: scope.workspaceId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
  })
}

export function createHomeDashboardViewModel(
  options: HomeDashboardViewModelOptions,
): HomeDashboardViewModel {
  const selection = scopeSelection(options.scope)
  const visits = options.visits ?? createHomeRecentVisitStore({
    storage: browserHomeVisitStorage(typeof window === "undefined" ? null : window),
  })
  const limits = options.limits ?? DEFAULT_HOME_DASHBOARD_LIMITS
  const nowMillis = options.nowMillis ?? Date.now
  const attention = createAttentionCenterViewModel({
    client: options.client,
    actor: options.actor,
    scope: options.scope,
    subscriptionId: options.subscriptionId,
    nextRequestId: options.nextRequestId,
    ...(options.nowMillis === undefined ? {} : { nowMillis: options.nowMillis }),
  })
  const deliveries = createHomeDeliveryListViewModel({
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
