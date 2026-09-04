// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneClientErrorKind,
} from './control-plane-client.js'
import {
  CommandName,
  QueryName,
  type Actor,
  type CommandAcceptedResponse,
  type CommandCompletedResponse,
  type DeliveryAdvanceCompletedResponse,
  type DeliveryId,
  type DeliveryListResultResponse,
  type DeliveryProjection,
  type DeliveryStatus,
  type OpaqueCursor,
  type PageInfo,
  type RepositoryScope,
  type RequestId,
  type Revision,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const DEFAULT_PAGE_LIMIT = 50
const DEFAULT_MAX_PAGES = 10

export type StrongFlowDeliveryOrder = 'recent' | 'title'

export type StrongFlowDeliveryListStatus = 'loading' | 'ready' | 'refreshing' | 'error'

export interface StrongFlowDeliveryListFilters {
  /** Title substring applied to loaded items; never a server query parameter. */
  readonly search: string
  /** Server-authoritative status selection; null lets the server return every status. */
  readonly status: DeliveryStatus | null
  /** Loaded items with open Attention only. */
  readonly attentionOnly: boolean
  readonly order: StrongFlowDeliveryOrder
}

export interface StrongFlowDeliveryListFailure {
  readonly kind: ControlPlaneClientErrorKind
  readonly code: string
  readonly message: string
  readonly requestId: RequestId | null
}

export interface StrongFlowDeliveryAdvanceState {
  readonly deliveryId: DeliveryId | null
  readonly failure: StrongFlowDeliveryListFailure | null
}

export interface StrongFlowDeliveryListState {
  readonly status: StrongFlowDeliveryListStatus
  readonly filters: StrongFlowDeliveryListFilters
  readonly visible: readonly DeliveryProjection[]
  readonly loadedCount: number
  readonly hasMore: boolean
  readonly loadingMore: boolean
  readonly moreFailure: StrongFlowDeliveryListFailure | null
  readonly error: StrongFlowDeliveryListFailure | null
  readonly advance: StrongFlowDeliveryAdvanceState
}

export interface StrongFlowDeliveryListViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly nextRequestId: () => RequestId
  /** Route lifetime signal; aborting it cancels the in-flight list request. */
  readonly signal?: AbortSignal
  readonly pageLimit?: number
  readonly maxPages?: number
}

export interface StrongFlowDeliveryListViewModel {
  readonly state: StrongFlowDeliveryListState
  subscribe(listener: (state: StrongFlowDeliveryListState) => void): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  loadMore(): Promise<void>
  setSearch(search: string): void
  /** Changes the server states parameter and rebuilds the snapshot from the first page. */
  setStatusFilter(status: DeliveryStatus | null): Promise<void>
  setAttentionOnly(attentionOnly: boolean): void
  setOrder(order: StrongFlowDeliveryOrder): void
  /**
   * Routes one card move through the exact delivery.advance command. The server
   * owns every transition rule; rejections never change the client column.
   */
  advanceDelivery(deliveryId: DeliveryId, expectedRevision: Revision): Promise<void>
  close(): void
}

function protocolFailure(
  code: string,
  message: string,
  requestId: RequestId | null = null,
): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code,
    message,
    requestId,
    retryable: false,
  })
}

function failureOf(error: unknown): StrongFlowDeliveryListFailure {
  if (error instanceof ControlPlaneClientError) {
    return Object.freeze({
      kind: error.kind,
      code: error.code,
      message: error.message,
      requestId: error.requestId,
    })
  }
  const candidate = error as {
    kind?: unknown
    code?: unknown
    message?: unknown
    requestId?: unknown
  }
  const kind = typeof candidate.kind === 'string' ? candidate.kind : 'protocol'
  const code = typeof candidate.code === 'string' ? candidate.code : 'DELIVERY_LIST_FAILED'
  const message = typeof candidate.message === 'string' && candidate.message.length > 0
    ? candidate.message
    : 'The Delivery list request failed.'
  const requestId = typeof candidate.requestId === 'string'
    ? candidate.requestId as RequestId
    : null
  return Object.freeze({
    kind: kind as ControlPlaneClientErrorKind,
    code,
    message,
    requestId,
  })
}

/** Server-authoritative cursor discipline; the client never invents or recovers a cursor. */
function nextPageCursor(
  page: PageInfo,
  requestId: RequestId | null,
  seenCursors: ReadonlySet<string>,
): OpaqueCursor | null {
  if (!page.hasMore) {
    if (page.nextCursor !== null) {
      throw protocolFailure(
        'STRONGFLOW_DELIVERY_LIST_PAGE_INVALID',
        'The final Delivery list page returned an unexpected cursor.',
        requestId,
      )
    }
    return null
  }
  const next = page.nextCursor
  if (next === null || seenCursors.has(next)) {
    throw protocolFailure(
      'STRONGFLOW_DELIVERY_LIST_PAGE_INVALID',
      'The Delivery list continuation cursor is missing or repeated.',
      requestId,
    )
  }
  return next
}

function recentFirst(left: DeliveryProjection, right: DeliveryProjection): number {
  return right.updatedAt.localeCompare(left.updatedAt)
    || right.deliveryId.localeCompare(left.deliveryId)
}

function titleOrder(left: DeliveryProjection, right: DeliveryProjection): number {
  return left.title.localeCompare(right.title)
    || left.deliveryId.localeCompare(right.deliveryId)
}

export function createStrongFlowDeliveryListViewModel(
  options: StrongFlowDeliveryListViewModelOptions,
): StrongFlowDeliveryListViewModel {
  const pageLimit = options.pageLimit ?? DEFAULT_PAGE_LIMIT
  const maxPages = options.maxPages ?? DEFAULT_MAX_PAGES
  if (!Number.isInteger(pageLimit) || pageLimit < 1 || pageLimit > 500) {
    throw new RangeError('The Delivery list page limit must be an integer between 1 and 500.')
  }
  if (!Number.isInteger(maxPages) || maxPages < 1) {
    throw new RangeError('The Delivery list page bound must be a positive integer.')
  }

  const { actor, scope } = options
  const requestOptions = options.signal === undefined ? undefined : { signal: options.signal }
  let closed = false
  let generation = 0
  let listener: ((state: StrongFlowDeliveryListState) => void) | null = null
  let loaded: readonly DeliveryProjection[] = []
  let cursor: OpaqueCursor | null = null
  let hasMore = false
  let loadingMore = false
  let moreFailure: StrongFlowDeliveryListFailure | null = null
  let loadFailure: StrongFlowDeliveryListFailure | null = null
  let status: StrongFlowDeliveryListStatus = 'loading'
  let filters: StrongFlowDeliveryListFilters = Object.freeze({
    search: '',
    status: null,
    attentionOnly: false,
    order: 'recent',
  })
  let advancing: DeliveryId | null = null
  let advanceFailure: StrongFlowDeliveryListFailure | null = null
  let rebuildChain: Promise<void> = Promise.resolve()
  let loadMoreFlight: Promise<void> | null = null

  function visibleDeliveries(): readonly DeliveryProjection[] {
    const search = filters.search.trim().toLowerCase()
    const filtered = loaded.filter(delivery => {
      if (search.length > 0 && !delivery.title.toLowerCase().includes(search)) return false
      if (filters.attentionOnly && delivery.openAttentionCount === 0) return false
      return true
    })
    return Object.freeze([...filtered].sort(
      filters.order === 'recent' ? recentFirst : titleOrder,
    ))
  }

  function snapshot(): StrongFlowDeliveryListState {
    return Object.freeze({
      status,
      filters,
      visible: visibleDeliveries(),
      loadedCount: loaded.length,
      hasMore,
      loadingMore,
      moreFailure,
      error: loadFailure,
      advance: Object.freeze({ deliveryId: advancing, failure: advanceFailure }),
    })
  }

  function publish(): void {
    listener?.(snapshot())
  }

  function beginGeneration(nextStatus: StrongFlowDeliveryListStatus): number {
    generation += 1
    status = nextStatus
    moreFailure = null
    loadFailure = null
    // A takeover retires the in-flight continuation, so its flag cannot outlive it.
    loadingMore = false
    publish()
    return generation
  }

  function superseded(ownGeneration: number): boolean {
    return closed || ownGeneration !== generation
  }

  /** A first load that fails has nothing to show; a failed refresh keeps the loaded window. */
  function failLoad(failure: StrongFlowDeliveryListFailure, firstLoad: boolean): void {
    loadFailure = failure
    status = firstLoad ? 'error' : 'ready'
    publish()
  }

  function request(nextRequestId: RequestId, states: readonly string[], page: {
    readonly cursor: OpaqueCursor | null
    readonly limit: number
  }): Promise<DeliveryListResultResponse> {
    return options.client.query({
      schemaVersion: SCHEMA_VERSION,
      requestId: nextRequestId,
      actor,
      scope,
      query: QueryName.DeliveryList,
      parameters: { states: Object.freeze([...states]) },
      page,
    }, requestOptions) as Promise<DeliveryListResultResponse>
  }

  /**
   * Loads one consistent snapshot from the first page. Refresh swaps the whole
   * loaded window only after the rebuild completes; a first load publishes
   * growing prefixes of the same page chain.
   */
  async function rebuild(
    ownGeneration: number,
    firstLoad: boolean,
    targetCount: number,
  ): Promise<void> {
    const collected: DeliveryProjection[] = []
    const seenIds = new Set<string>()
    const seenCursors = new Set<string>()
    let pageCursor: OpaqueCursor | null = null
    for (let pageIndex = 0; ; pageIndex += 1) {
      if (pageIndex >= maxPages) {
        if (firstLoad) loaded = collected
        failLoad(protocolFailure(
          'STRONGFLOW_DELIVERY_LIST_PAGE_LIMIT',
          'The Delivery list exceeded the bounded page limit.',
          null,
        ), firstLoad)
        return
      }
      let response: DeliveryListResultResponse
      try {
        response = await request(
          options.nextRequestId(),
          filters.status === null ? [] : [filters.status],
          { cursor: pageCursor, limit: pageLimit },
        )
      } catch (error) {
        if (superseded(ownGeneration)) return
        if (firstLoad) loaded = collected
        failLoad(failureOf(error), firstLoad)
        return
      }
      if (superseded(ownGeneration)) return
      if (response.query !== QueryName.DeliveryList) {
        if (firstLoad) loaded = collected
        failLoad(protocolFailure(
          'STRONGFLOW_DELIVERY_LIST_PAGE_INVALID',
          'The StrongFlow route received another list response.',
          response.requestId,
        ), firstLoad)
        return
      }
      for (const item of response.result.items) {
        if (seenIds.has(item.deliveryId)) continue
        seenIds.add(item.deliveryId)
        collected.push(item)
      }
      try {
        pageCursor = nextPageCursor(response.page, response.requestId, seenCursors)
      } catch (error) {
        if (superseded(ownGeneration)) return
        if (firstLoad) loaded = collected
        failLoad(failureOf(error), firstLoad)
        return
      }
      if (firstLoad) {
        loaded = collected
        publish()
      }
      if (pageCursor === null || collected.length >= targetCount) break
    }
    loaded = collected
    cursor = pageCursor
    hasMore = pageCursor !== null
    status = 'ready'
    publish()
  }

  function scheduleRebuild(
    nextStatus: 'loading' | 'refreshing',
    targetCount: number,
  ): Promise<void> {
    const ownGeneration = beginGeneration(nextStatus)
    const firstLoad = nextStatus === 'loading'
    const chain = rebuildChain
    const run = (async () => {
      await chain.catch(() => undefined)
      if (superseded(ownGeneration)) return
      await rebuild(ownGeneration, firstLoad, targetCount)
    })()
    rebuildChain = run
    return run
  }

  async function runLoadMore(): Promise<void> {
    const chain = rebuildChain
    await chain.catch(() => undefined)
    if (closed || !hasMore || loadingMore) return
    const ownGeneration = generation
    const currentCursor = cursor
    if (currentCursor === null) {
      hasMore = false
      publish()
      return
    }
    loadingMore = true
    moreFailure = null
    publish()
    let response: DeliveryListResultResponse
    try {
      response = await request(
        options.nextRequestId(),
        filters.status === null ? [] : [filters.status],
        { cursor: currentCursor, limit: pageLimit },
      )
    } catch (error) {
      loadingMore = false
      if (superseded(ownGeneration)) return
      moreFailure = failureOf(error)
      publish()
      return
    }
    loadingMore = false
    if (superseded(ownGeneration)) return
    if (response.query !== QueryName.DeliveryList) {
      moreFailure = failureOf(protocolFailure(
        'STRONGFLOW_DELIVERY_LIST_PAGE_INVALID',
        'The StrongFlow route received another list response.',
        response.requestId,
      ))
      publish()
      return
    }
    const seenIds = new Set(loaded.map(item => item.deliveryId))
    const appended: DeliveryProjection[] = []
    for (const item of response.result.items) {
      if (seenIds.has(item.deliveryId)) continue
      seenIds.add(item.deliveryId)
      appended.push(item)
    }
    try {
      const next = nextPageCursor(response.page, response.requestId, new Set([currentCursor]))
      cursor = next
      hasMore = next !== null
    } catch (error) {
      hasMore = false
      moreFailure = failureOf(error)
      publish()
      return
    }
    loaded = Object.freeze([...loaded, ...appended])
    status = 'ready'
    publish()
  }

  const model: StrongFlowDeliveryListViewModel = {
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
      // The first load takes exactly one server page; further pages are explicit.
      return scheduleRebuild('loading', 0)
    },
    refresh() {
      if (closed) return Promise.resolve()
      return scheduleRebuild('refreshing', Math.max(loaded.length, pageLimit))
    },
    loadMore() {
      if (closed) return Promise.resolve()
      if (loadMoreFlight !== null) return loadMoreFlight
      const flight = runLoadMore().finally(() => {
        loadMoreFlight = null
      })
      loadMoreFlight = flight
      return flight
    },
    setSearch(search) {
      if (closed) return
      filters = Object.freeze({ ...filters, search })
      publish()
    },
    setStatusFilter(next) {
      if (closed) return Promise.resolve()
      filters = Object.freeze({ ...filters, status: next })
      return scheduleRebuild('refreshing', Math.max(loaded.length, pageLimit))
    },
    setAttentionOnly(attentionOnly) {
      if (closed) return
      filters = Object.freeze({ ...filters, attentionOnly })
      publish()
    },
    setOrder(order) {
      if (closed) return
      filters = Object.freeze({ ...filters, order })
      publish()
    },
    async advanceDelivery(deliveryId, expectedRevision) {
      if (closed || advancing !== null) return
      const chain = rebuildChain
      await chain.catch(() => undefined)
      if (closed || advancing !== null) return
      const ownGeneration = generation
      const requestId = options.nextRequestId()
      advancing = deliveryId
      advanceFailure = null
      publish()
      let response: CommandAcceptedResponse | CommandCompletedResponse
      try {
        response = await options.client.command({
          schemaVersion: SCHEMA_VERSION,
          requestId,
          actor,
          scope,
          command: CommandName.DeliveryAdvance,
          expectedRevision,
          payload: { deliveryId },
        }, requestOptions)
      } catch (error) {
        if (superseded(ownGeneration)) return
        advanceFailure = failureOf(error)
        publish()
        return
      }
      if (superseded(ownGeneration)) return
      if (response.requestId !== requestId || response.command !== CommandName.DeliveryAdvance) {
        advanceFailure = failureOf(protocolFailure(
          'STRONGFLOW_DELIVERY_LIST_COMMAND_MISMATCH',
          'The Control Plane returned another Delivery command result.',
          response.requestId,
        ))
        publish()
        return
      }
      if (response.outcome === 'accepted') {
        // The transition is accepted; the authoritative projection arrives through
        // the established command invalidation and a first-page rebuild.
        advancing = null
        publish()
        void model.refresh()
        return
      }
      const projection = (response as DeliveryAdvanceCompletedResponse).result
      const existing = loaded.findIndex(item => item.deliveryId === projection.deliveryId)
      loaded = existing < 0
        ? Object.freeze([...loaded, projection])
        : Object.freeze(loaded.map((item, index) => (index === existing ? projection : item)))
      advancing = null
      advanceFailure = null
      status = 'ready'
      publish()
    },
    close() {
      if (closed) return
      closed = true
      generation += 1
      listener = null
    },
  }
  return model
}
