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
  Instant,
  OpaqueCursor,
  RepositoryScope,
  RequestId,
  WorkItemState,
  WorkRunId,
} from './generated/contracts.js'
import { QueryName, WorkItemState as WorkItemStateVocabulary } from './generated/contracts.js'

export type HomeDashboardStatus = 'loading' | 'ready' | 'partial' | 'error' | 'closed'

export type HomeDashboardSource = 'delivery' | 'attention' | 'usage'

export type HomeDashboardSourceState = 'loading' | 'ok' | 'unavailable'

const DELIVERY_LIST_PAGE_LIMIT = 50
const DELIVERY_LIST_MAX_PAGES = 10

export type HomeDeliveryListStatus = 'loading' | 'ready' | 'refreshing' | 'error'

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

export function createHomeDeliveryListViewModel(
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
    const ownGeneration = ++generation
    status = nextStatus
    publish()
    const firstLoad = nextStatus === 'loading'
    const chain = rebuildChain
    const run = (async () => {
      await chain.catch(() => undefined)
      if (superseded(ownGeneration)) return
      await rebuild(ownGeneration, firstLoad)
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


export interface HomeDeliveryCard {
  readonly deliveryId: DeliveryId
  readonly title: string
  readonly status: WorkItemState
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

export interface HomeDecisionCard {
  readonly kind: AttentionCenterItem['kind']
  readonly id: string
  readonly title: string
  readonly urgency: AttentionCenterItem['urgency']
  readonly createdAt: Instant | null
  readonly expiresAt: Instant | null
  readonly actionDisabled: boolean
  readonly productSessionId: AttentionCenterItem['productSessionId']
  readonly sessionTitle: string | null
  readonly deliveryId: AttentionCenterItem['deliveryId']
  readonly deliveryTitle: string | null
  readonly workRunId: AttentionCenterItem['workRunId']
  readonly attentionCount?: number
}

export interface HomeVisitedCard extends HomeDeliveryCard {
  readonly visitedAt: Instant
}

export interface HomeDashboardCounts {
  readonly decisions: number
  readonly backlog: number
  readonly running: number
  readonly ready: number
  readonly waiting: number
  readonly validating: number
  readonly failed: number
  readonly completed: number
  readonly visited: number
}

export interface HomeDashboardState {
  readonly status: HomeDashboardStatus
  readonly decisions: readonly HomeDecisionCard[]
  readonly backlog: readonly HomeDeliveryCard[]
  readonly running: readonly HomeDeliveryCard[]
  readonly ready: readonly HomeDeliveryCard[]
  readonly waiting: readonly HomeDeliveryCard[]
  readonly validating: readonly HomeDeliveryCard[]
  readonly failed: readonly HomeDeliveryCard[]
  readonly completed: readonly HomeDeliveryCard[]
  readonly visited: readonly HomeVisitedCard[]
  readonly counts: HomeDashboardCounts
  readonly sources: Readonly<Record<HomeDashboardSource, HomeDashboardSourceState>>
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
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  readonly visits?: HomeRecentVisitStore
  readonly limits?: HomeDashboardLimits
  readonly nowMillis?: () => number
}

export interface HomeDashboardViewModel {
  readonly usage: UsageHealthViewModel
  readonly state: HomeDashboardState
  subscribe(listener: (state: HomeDashboardState) => void): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  close(): void
}

export type HomeDeliverySection =
  | 'backlog'
  | 'ready'
  | 'running'
  | 'waiting'
  | 'validating'
  | 'failed'
  | 'completed'

const RUNNING_DELIVERY_STATUSES: readonly WorkItemState[] = Object.freeze([
  WorkItemStateVocabulary.InProgress,
  WorkItemStateVocabulary.Rework,
])

const READY_DELIVERY_STATUSES: readonly WorkItemState[] = Object.freeze([
  WorkItemStateVocabulary.Ready,
])

const WAITING_DELIVERY_STATUSES: readonly WorkItemState[] = Object.freeze([
  WorkItemStateVocabulary.WaitingDependency,
  WorkItemStateVocabulary.WaitingHuman,
])

const VALIDATING_DELIVERY_STATUSES: readonly WorkItemState[] = Object.freeze([
  WorkItemStateVocabulary.CandidateReady,
  WorkItemStateVocabulary.Validating,
])

function isFailed(delivery: HomeDeliveryCard): boolean {
  return delivery.status === WorkItemStateVocabulary.Failed
    || delivery.status === WorkItemStateVocabulary.Cancelled
    || delivery.status === WorkItemStateVocabulary.WaitingDependency
    || delivery.failedTasks > 0
    || delivery.blockedTasks > 0
    || (delivery.status === WorkItemStateVocabulary.WaitingHuman
      && delivery.openAttentionCount > 0)
}

function isRunning(delivery: HomeDeliveryCard): boolean {
  return RUNNING_DELIVERY_STATUSES.includes(delivery.status)
}

function isReady(delivery: HomeDeliveryCard): boolean {
  return READY_DELIVERY_STATUSES.includes(delivery.status)
}

function isWaiting(delivery: HomeDeliveryCard): boolean {
  return WAITING_DELIVERY_STATUSES.includes(delivery.status)
}

export function homeDeliverySection(delivery: HomeDeliveryCard): HomeDeliverySection {
  if (isFailed(delivery)) return 'failed'
  if (delivery.status === WorkItemStateVocabulary.Backlog) return 'backlog'
  if (isRunning(delivery)) return 'running'
  if (isReady(delivery)) return 'ready'
  if (isWaiting(delivery)) return 'waiting'
  if (VALIDATING_DELIVERY_STATUSES.includes(delivery.status)) return 'validating'
  return 'completed'
}

function recency(left: HomeDeliveryCard, right: HomeDeliveryCard): number {
  return right.updatedAt.localeCompare(left.updatedAt)
    || right.deliveryId.localeCompare(left.deliveryId)
}

function orderedSection(
  section: HomeDeliverySection,
  cards: readonly HomeDeliveryCard[],
  compare: (left: HomeDeliveryCard, right: HomeDeliveryCard) => number,
): readonly HomeDeliveryCard[] {
  return Object.freeze(cards.filter(card => homeDeliverySection(card) === section).sort(compare))
}

export function orderedHomeRunningCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return orderedSection('running', cards, recency)
}

export function orderedHomeBacklogCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return orderedSection('backlog', cards, recency)
}

export function orderedHomeFailedCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return orderedSection('failed', cards, (left, right) =>
    right.failedTasks - left.failedTasks
    || right.blockedTasks - left.blockedTasks
    || recency(left, right))
}

export function orderedHomeReadyCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return orderedSection('ready', cards, recency)
}

export function orderedHomeWaitingCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return orderedSection('waiting', cards, recency)
}

export function orderedHomeValidatingCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return orderedSection('validating', cards, recency)
}

export function orderedHomeCompletedCards(
  cards: readonly HomeDeliveryCard[],
): readonly HomeDeliveryCard[] {
  return orderedSection('completed', cards, recency)
}

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
    failedTasks: delivery.workItemCounts.failed,
    blockedTasks: delivery.workItemCounts.waitingDependency + delivery.workItemCounts.waitingHuman,
    activeTasks: delivery.workItemCounts.inProgress,
    verifyingTasks: delivery.workItemCounts.validating,
    completedTasks: delivery.workItemCounts.done,
    totalTasks: delivery.workItemCounts.total,
  })))
}

export function homeDecisionCards(
  items: readonly AttentionCenterItem[],
): readonly HomeDecisionCard[] {
  const cards: HomeDecisionCard[] = []
  const attentionByDelivery = new Map<DeliveryId, number>()
  const cardIndexByDelivery = new Map<DeliveryId, number>()
  for (const item of orderedAttentionCenterItems(items)) {
    const card: HomeDecisionCard = {
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
    }
    if (item.kind !== 'attention' || item.deliveryId === null) {
      cards.push(Object.freeze(card))
      continue
    }
    const existingIndex = cardIndexByDelivery.get(item.deliveryId)
    const count = (attentionByDelivery.get(item.deliveryId) ?? 0) + 1
    attentionByDelivery.set(item.deliveryId, count)
    if (existingIndex === undefined) {
      cardIndexByDelivery.set(item.deliveryId, cards.length)
      cards.push(Object.freeze({ ...card, attentionCount: count }))
    } else {
      const existing = cards[existingIndex]
      if (existing === undefined) continue
      cards[existingIndex] = Object.freeze({
        ...existing,
        attentionCount: count,
        actionDisabled: existing.actionDisabled && card.actionDisabled,
      })
    }
  }
  return Object.freeze(cards)
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
  const sections: Record<HomeDeliverySection, HomeDeliveryCard[]> = {
    backlog: [],
    running: [],
    ready: [],
    waiting: [],
    validating: [],
    failed: [],
    completed: [],
  }
  for (const card of cards) sections[homeDeliverySection(card)].push(card)
  const backlog = orderedHomeBacklogCards(sections.backlog)
  const running = orderedHomeRunningCards(sections.running)
  const ready = orderedHomeReadyCards(sections.ready)
  const waiting = orderedHomeWaitingCards(sections.waiting)
  const validating = orderedHomeValidatingCards(sections.validating)
  const failed = orderedHomeFailedCards(sections.failed)
  const completed = orderedHomeCompletedCards(sections.completed)
  const visited = visitedCards(byId, input.visits ?? [])
  const decisions = homeDecisionCards(input.attention.items)
  const visibleDecisions = decisions.slice(0, limits.decisions)
  const visibleBacklog = backlog.slice(0, limits.deliveries)
  const visibleRunning = running.slice(0, limits.deliveries)
  const visibleReady = ready.slice(0, limits.deliveries)
  const visibleWaiting = waiting.slice(0, limits.deliveries)
  const visibleValidating = validating.slice(0, limits.deliveries)
  const visibleFailed = failed.slice(0, limits.deliveries)
  const visibleCompleted = completed.slice(0, limits.deliveries)
  const sources: Readonly<Record<HomeDashboardSource, HomeDashboardSourceState>> = Object.freeze({
    delivery: deliverySourceState(input.deliveries),
    attention: attentionSourceState(input.attention),
    usage: usageSourceState(input.usage),
  })
  return Object.freeze({
    status: dashboardStatus(sources),
    decisions: Object.freeze(visibleDecisions),
    backlog: Object.freeze(visibleBacklog),
    running: Object.freeze(visibleRunning),
    ready: Object.freeze(visibleReady),
    waiting: Object.freeze(visibleWaiting),
    validating: Object.freeze(visibleValidating),
    failed: Object.freeze(visibleFailed),
    completed: Object.freeze(visibleCompleted),
    visited: Object.freeze(visited.slice(0, limits.visits)),
    counts: Object.freeze({
      decisions: visibleDecisions.length,
      backlog: visibleBacklog.length,
      running: visibleRunning.length,
      ready: visibleReady.length,
      waiting: visibleWaiting.length,
      validating: visibleValidating.length,
      failed: visibleFailed.length,
      completed: visibleCompleted.length,
      visited: visited.length,
    }),
    sources,
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
    backlog: Object.freeze([]),
    running: Object.freeze([]),
    ready: Object.freeze([]),
    waiting: Object.freeze([]),
    validating: Object.freeze([]),
    failed: Object.freeze([]),
    completed: Object.freeze([]),
    visited: Object.freeze([]),
    counts: Object.freeze({
      decisions: 0,
      backlog: 0,
      running: 0,
      ready: 0,
      waiting: 0,
      validating: 0,
      failed: 0,
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
