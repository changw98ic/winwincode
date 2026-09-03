// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './control-plane-client.js'
import type { GlobalConnectionStatus } from './core/connection-state.js'
import { createQueryCacheLifecycle } from './core/query-cache.js'
import type {
  Actor,
  ModelRouteAvailabilityListResultResponse,
  ModelRouteAvailabilityPage,
  ModelRouteAvailabilityProjection,
  OpaqueCursor,
  PageInfo,
  QueryRequest,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
  Scope,
  WorkerProjection,
} from './generated/contracts.js'
import {
  ModelRouteAvailabilityReason,
  QueryName,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const PRESENCE_PAGE_SIZE = 200
const SESSION_PAGE_SIZE = 50
const DELIVERY_PAGE_SIZE = 50
const MAX_PAGES = 10
const WORKER_STATES = Object.freeze(['enabled', 'draining', 'offline'] as const)

export type ReadinessItemId =
  | 'repository-scope'
  | 'model-route'
  | 'credential-reference'
  | 'server-worker-health'
  | 'helper-availability'
  | 'first-chat-delivery'

export type ReadinessItemStatus = 'ready' | 'attention' | 'blocked' | 'unavailable'

/** Closed, secret-safe checklist outcomes; raw server or transport text never reaches this union. */
export type ReadinessReason =
  | 'signed-out'
  | 'scope-selection-required'
  | 'scope-not-authorized'
  | 'scope-empty'
  | 'no-provider'
  | 'credential-missing-or-revoked'
  | 'default-route-invalid'
  | 'provider-or-model-disabled'
  | 'request-pool-unavailable'
  | 'no-ready-route'
  | 'no-credential-reference'
  | 'credential-reference-unavailable'
  | 'server-unreachable'
  | 'no-worker-reported'
  | 'no-enabled-worker-capacity'
  | 'no-chat-session'
  | 'no-delivery'

export interface ReadinessItemState {
  readonly id: ReadinessItemId
  readonly status: ReadinessItemStatus
  readonly reason: ReadinessReason | null
  readonly errorCode: string | null
  readonly checkedAt: string | null
}

/**
 * Current browser facts the checklist reads instead of owning authorization state.
 * The application derives it from the one AuthSession and Scope resolution.
 */
export type ReadinessContext =
  | { readonly status: 'signed-out' }
  | {
      readonly status: 'no-scope'
      readonly reason: 'selection-required' | 'denied' | 'empty'
    }
  | { readonly status: 'ready'; readonly actor: Actor; readonly scope: RepositoryScope }

export type ReadinessStatus = 'idle' | 'checking' | 'ready' | 'attention' | 'closed'

export interface ReadinessViewModelState {
  readonly status: ReadinessStatus
  readonly collapsed: boolean
  readonly items: readonly ReadinessItemState[]
}

export interface ReadinessViewModelOptions {
  readonly client: ControlPlaneClient
  /** Current shell connection facts; the checklist owns no second connection monitor. */
  readonly serverStatus: () => GlobalConnectionStatus
  readonly now?: () => string
  readonly nextRequestId: () => RequestId
}

export interface ReadinessViewModel {
  readonly state: ReadinessViewModelState
  subscribe(listener: (state: ReadinessViewModelState) => void): () => void
  updateContext(context: ReadinessContext): Promise<void>
  setCollapsed(collapsed: boolean): void
  refresh(): Promise<void>
  close(): void
}

const ITEM_ORDER: readonly ReadinessItemId[] = Object.freeze([
  'repository-scope',
  'model-route',
  'credential-reference',
  'server-worker-health',
  'helper-availability',
  'first-chat-delivery',
])

function blockedItems(): readonly ReadinessItemState[] {
  return Object.freeze(ITEM_ORDER.map(id => Object.freeze({
    id,
    status: 'blocked' as const,
    reason: null,
    errorCode: null,
    checkedAt: null,
  })))
}

function clientFailure(code: string, message: string, cause?: unknown): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code,
    message,
    requestId: null,
    retryable: false,
    ...(cause === undefined ? {} : { cause }),
  })
}

function assertPage(page: PageInfo, query: string): void {
  if (page.hasMore !== (page.nextCursor !== null)) throw clientFailure(
    'READINESS_PAGE_INVALID',
    `${query} returned an inconsistent pagination cursor.`,
  )
}

function sameScope(left: Scope, right: RepositoryScope): boolean {
  return left.kind === 'repository'
    && left.organizationId === right.organizationId
    && left.workspaceId === right.workspaceId
    && left.projectId === right.projectId
    && left.repositoryId === right.repositoryId
}

function isReadyRoute(candidate: ModelRouteAvailabilityProjection): boolean {
  return candidate.status === 'enabled' && candidate.reason === 'ready'
}

function modelRouteReason(
  page: ModelRouteAvailabilityPage,
  items: readonly ModelRouteAvailabilityProjection[],
): ReadinessReason {
  const reason = items[0]?.reason ?? page.reason
  if (reason === ModelRouteAvailabilityReason.CredentialMissingOrRevoked) {
    return 'credential-missing-or-revoked'
  }
  if (reason === ModelRouteAvailabilityReason.DefaultRouteInvalid) return 'default-route-invalid'
  if (reason === ModelRouteAvailabilityReason.ProviderOrModelDisabled) {
    return 'provider-or-model-disabled'
  }
  if (reason === ModelRouteAvailabilityReason.RequestPoolUnavailable) {
    return 'request-pool-unavailable'
  }
  if (reason === ModelRouteAvailabilityReason.NoProvider) return 'no-provider'
  return 'no-ready-route'
}

function item(
  id: ReadinessItemId,
  status: ReadinessItemStatus,
  checkedAt: string,
  reason: ReadinessReason | null = null,
  errorCode: string | null = null,
): ReadinessItemState {
  return Object.freeze({ id, status, reason, errorCode, checkedAt })
}

function unavailableItem(
  id: ReadinessItemId,
  checkedAt: string,
  error: unknown,
): ReadinessItemState {
  if (error instanceof ControlPlaneClientError) {
    return item(id, 'unavailable', checkedAt, null, error.code)
  }
  return item(id, 'unavailable', checkedAt, null, 'READINESS_CHECK_FAILED')
}

/** Build the first-run checklist from existing health, settings, auth, and Scope projections only. */
export function createReadinessViewModel(
  options: ReadinessViewModelOptions,
): ReadinessViewModel {
  const listeners = new Set<(state: ReadinessViewModelState) => void>()
  const now = options.now ?? (() => new Date().toISOString())
  const controllers = new Set<AbortController>()
  let currentState: ReadinessViewModelState = Object.freeze({
    status: 'idle',
    collapsed: false,
    items: blockedItems(),
  })
  let lastContext: ReadinessContext | null = null
  let lastIdentity: string | null = null
  let generation = 0
  let closed = false

  function publish(state: ReadinessViewModelState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function requireOpen(): void {
    if (closed) throw clientFailure('READINESS_CLOSED', 'The readiness checklist is closed.')
  }

  function controller(): AbortController {
    const value = new AbortController()
    controllers.add(value)
    return value
  }

  function release(value: AbortController): void {
    controllers.delete(value)
  }

  function requestBase(actor: Actor, scope: RepositoryScope) {
    return {
      schemaVersion: SCHEMA_VERSION,
      actor,
      scope,
    }
  }

  async function modelRouteAvailability(
    actor: Actor,
    scope: RepositoryScope,
    signal: AbortSignal,
  ): Promise<{
    readonly page: ModelRouteAvailabilityPage
    readonly items: readonly ModelRouteAvailabilityProjection[]
  }> {
    const items: ModelRouteAvailabilityProjection[] = []
    let firstPage: ModelRouteAvailabilityPage | null = null
    const cursors = new Set<OpaqueCursor>()
    let cursor: OpaqueCursor | null = null
    for (let index = 0; index < MAX_PAGES; index += 1) {
      const response: ModelRouteAvailabilityListResultResponse = await options.client.query({
        ...requestBase(actor, scope),
        requestId: options.nextRequestId(),
        query: QueryName.ModelRouteAvailabilityList,
        parameters: {},
        page: { cursor, limit: PRESENCE_PAGE_SIZE },
      }, { signal }) as ModelRouteAvailabilityListResultResponse
      if (response.query !== QueryName.ModelRouteAvailabilityList) throw clientFailure(
        'READINESS_QUERY_MISMATCH',
        'The readiness checklist received another model-route response.',
      )
      assertPage(response.page, response.query)
      if (!sameScope(response.result.scope, scope)) throw clientFailure(
        'READINESS_SCOPE_MISMATCH',
        'The model-route availability page belongs to another repository.',
      )
      if (firstPage === null) firstPage = response.result
      for (const candidate of response.result.items) {
        items.push(candidate)
        if (isReadyRoute(candidate)) {
          return Object.freeze({ page: firstPage, items: Object.freeze(items) })
        }
      }
      if (!response.page.hasMore) {
        return Object.freeze({ page: firstPage, items: Object.freeze(items) })
      }
      const next = response.page.nextCursor
      if (next === null || cursors.has(next)) throw clientFailure(
        'READINESS_CURSOR_INVALID',
        'The readiness checklist received an invalid continuation cursor.',
      )
      cursors.add(next)
      cursor = next
    }
    throw clientFailure(
      'READINESS_PAGE_LIMIT_EXCEEDED',
      'The model-route availability read exceeded the bounded page limit.',
    )
  }

  async function collectPages(
    build: (cursor: OpaqueCursor | null) => QueryRequest,
    signal: AbortSignal,
  ): Promise<readonly unknown[]> {
    const expectedQuery = build(null).query
    const items: unknown[] = []
    const cursors = new Set<OpaqueCursor>()
    let cursor: OpaqueCursor | null = null
    for (let index = 0; index < MAX_PAGES; index += 1) {
      const response: QueryResultResponse = await options.client.query(
        build(cursor),
        { signal },
      )
      if (response.query !== expectedQuery) throw clientFailure(
        'READINESS_QUERY_MISMATCH',
        'The readiness checklist received another query response.',
      )
      assertPage(response.page, expectedQuery)
      const result = (response as { readonly result?: { readonly items?: unknown } }).result
      const pageItems = (result as { readonly items?: unknown } | undefined)?.items
      if (!Array.isArray(pageItems)) throw clientFailure(
        'READINESS_PROJECTION_INVALID',
        'The readiness checklist received a page without list items.',
      )
      items.push(...pageItems)
      if (!response.page.hasMore) return Object.freeze(items)
      const next = response.page.nextCursor
      if (next === null || cursors.has(next)) throw clientFailure(
        'READINESS_CURSOR_INVALID',
        'The readiness checklist received an invalid continuation cursor.',
      )
      cursors.add(next)
      cursor = next
    }
    throw clientFailure(
      'READINESS_PAGE_LIMIT_EXCEEDED',
      'The readiness checklist read exceeded the bounded page limit.',
    )
  }

  async function evaluate(
    context: Extract<ReadinessContext, { readonly status: 'ready' }>,
    signal: AbortSignal,
  ): Promise<{
    readonly items: readonly ReadinessItemState[]
    /** Resolves only when every underlying read settled, so rounds release their cancellation handle late. */
    readonly settled: Promise<unknown>
  }> {
    const checkedAt = now()
    const { actor, scope } = context

    const modelRouteRead = modelRouteAvailability(actor, scope, signal)
    const credentialsRead = collectPages(cursor => ({
      ...requestBase(actor, scope),
      requestId: options.nextRequestId(),
      query: QueryName.CredentialReferenceList,
      parameters: { providerId: null },
      page: { cursor, limit: PRESENCE_PAGE_SIZE },
    }), signal)
    const workersRead = collectPages(cursor => ({
      ...requestBase(actor, scope),
      requestId: options.nextRequestId(),
      query: QueryName.WorkerList,
      parameters: { states: WORKER_STATES },
      page: { cursor, limit: PRESENCE_PAGE_SIZE },
    }), signal)
    const sessionsRead = collectPages(cursor => ({
      ...requestBase(actor, scope),
      requestId: options.nextRequestId(),
      query: QueryName.SessionList,
      parameters: { states: [] },
      page: { cursor, limit: SESSION_PAGE_SIZE },
    }), signal).catch(error => {
      if (error instanceof ControlPlaneClientError) throw error
      throw clientFailure('READINESS_CHECK_FAILED', 'The session presence read failed.', error)
    })
    const deliveriesRead = collectPages(cursor => ({
      ...requestBase(actor, scope),
      requestId: options.nextRequestId(),
      query: QueryName.DeliveryList,
      parameters: { states: [] },
      page: { cursor, limit: DELIVERY_PAGE_SIZE },
    }), signal).catch(error => {
      if (error instanceof ControlPlaneClientError) throw error
      throw clientFailure('READINESS_CHECK_FAILED', 'The delivery presence read failed.', error)
    })

    const modelRoute = modelRouteRead
      .then(availability => {
        const ready = availability.items.some(candidate => isReadyRoute(candidate))
        return ready
          ? item('model-route', 'ready', checkedAt)
          : item('model-route', 'attention', checkedAt, modelRouteReason(
              availability.page,
              availability.items,
            ))
      })
      .catch(error => unavailableItem('model-route', checkedAt, error))

    const credentials = credentialsRead.then(items => {
      const references = items as readonly { readonly secretState?: string }[]
      if (references.length === 0) {
        return item('credential-reference', 'attention', checkedAt, 'no-credential-reference')
      }
      return references.some(reference => reference.secretState === 'available')
        ? item('credential-reference', 'ready', checkedAt)
        : item(
            'credential-reference',
            'attention',
            checkedAt,
            'credential-reference-unavailable',
          )
    }).catch(error => unavailableItem('credential-reference', checkedAt, error))

    const workers = workersRead.then(items => {
      const projections = items as readonly WorkerProjection[]
      const serverReachable = options.serverStatus() === 'connected'
      let health: ReadinessItemState
      if (!serverReachable) {
        health = item('server-worker-health', 'attention', checkedAt, 'server-unreachable')
      } else if (projections.length === 0) {
        health = item('server-worker-health', 'attention', checkedAt, 'no-worker-reported')
      } else {
        health = item('server-worker-health', 'ready', checkedAt)
      }
      const helper = projections.some(worker => (
        worker.state === 'enabled' && worker.capacity > 0
      ))
        ? item('helper-availability', 'ready', checkedAt)
        : item('helper-availability', 'attention', checkedAt, 'no-enabled-worker-capacity')
      return [health, helper]
    }).catch((error: unknown) => [
      unavailableItem('server-worker-health', checkedAt, error),
      unavailableItem('helper-availability', checkedAt, error),
    ])

    const firstChatDelivery = Promise.all([sessionsRead, deliveriesRead])
      .then(([sessionItems, deliveryItems]) => {
        if (sessionItems.length === 0) {
          return item('first-chat-delivery', 'attention', checkedAt, 'no-chat-session')
        }
        return deliveryItems.length === 0
          ? item('first-chat-delivery', 'attention', checkedAt, 'no-delivery')
          : item('first-chat-delivery', 'ready', checkedAt)
      })
      .catch((error: unknown) => unavailableItem('first-chat-delivery', checkedAt, error))

    const settled = Promise.allSettled([
      modelRouteRead,
      credentialsRead,
      workersRead,
      sessionsRead,
      deliveriesRead,
    ])
    const itemResults = await Promise.all([
      Promise.resolve(item('repository-scope', 'ready', checkedAt)),
      modelRoute,
      credentials,
      workers,
      firstChatDelivery,
    ])
    return {
      items: Object.freeze(itemResults.flat()),
      settled,
    }
  }

  function overallStatus(items: readonly ReadinessItemState[]): ReadinessStatus {
    return items.some(candidate => (
      candidate.status === 'attention' || candidate.status === 'unavailable'
    ))
      ? 'attention'
      : 'ready'
  }

  function contextIdentity(context: ReadinessContext): string {
    if (context.status === 'signed-out') return 'signed-out'
    if (context.status === 'no-scope') return `no-scope ${context.reason}`
    return [
      'ready',
      context.actor.kind,
      context.actor.id,
      context.scope.organizationId,
      context.scope.workspaceId,
      context.scope.projectId,
      context.scope.repositoryId,
    ].join(' ')
  }

  async function run(context: ReadinessContext): Promise<void> {
    requireOpen()
    generation += 1
    const ownGeneration = generation
    const identity = contextIdentity(context)
    if (identity !== lastIdentity) {
      // Leaving one context detaches this checklist's consumers immediately so the
      // shared QueryCache can cancel superseded Scope flights without delay.
      for (const active of controllers) active.abort()
      controllers.clear()
    }
    lastIdentity = identity
    const active = controller()
    lastContext = context
    if (context.status !== 'ready') {
      const checkedAt = now()
      const scopeReason: ReadinessReason | null = context.status === 'signed-out'
        ? 'signed-out'
        : context.reason === 'selection-required'
          ? 'scope-selection-required'
          : context.reason === 'denied'
            ? 'scope-not-authorized'
            : 'scope-empty'
      publish({
        status: overallStatus([item('repository-scope', 'attention', checkedAt, scopeReason)]),
        collapsed: currentState.collapsed,
        items: Object.freeze([
          item('repository-scope', 'attention', checkedAt, scopeReason),
          ...blockedItems().slice(1),
        ]),
      })
      release(active)
      return
    }
    publish({ ...currentState, status: 'checking' })
    try {
      const { items, settled } = await evaluate(context, active.signal)
      // The cancellation handle outlives the published items: reads that are still
      // in flight must stay abortable until they settle, or a Scope change could
      // never cancel them through the shared QueryCache.
      const releaseWhenSettled = () => release(active)
      void settled.then(releaseWhenSettled, releaseWhenSettled)
      if (closed || generation !== ownGeneration) return
      publish({ status: overallStatus(items), collapsed: currentState.collapsed, items })
    } catch (error) {
      release(active)
      throw error
    }
  }

  return {
    get state() { return currentState },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    updateContext: run,
    setCollapsed(collapsed) {
      requireOpen()
      publish({ ...currentState, collapsed })
    },
    async refresh() {
      requireOpen()
      if (lastContext === null) return
      if (lastContext.status === 'ready') {
        // An explicit recheck must observe Server facts changed since the cached snapshot.
        createQueryCacheLifecycle({
          client: options.client,
          actor: lastContext.actor,
          scope: lastContext.scope,
        }).refresh([
          QueryName.ModelRouteAvailabilityList,
          QueryName.CredentialReferenceList,
          QueryName.WorkerList,
          QueryName.SessionList,
          QueryName.DeliveryList,
        ])
      }
      await run(lastContext)
    },
    close() {
      if (closed) return
      closed = true
      generation += 1
      for (const active of controllers) active.abort()
      controllers.clear()
      publish({ status: 'closed', collapsed: currentState.collapsed, items: blockedItems() })
      listeners.clear()
    },
  }
}
