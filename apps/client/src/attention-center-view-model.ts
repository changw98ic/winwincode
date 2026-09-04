// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneSubscription,
} from './control-plane-client.js'
import { createQueryCacheLifecycle } from './core/query-cache.js'
import type {
  Actor,
  ApprovalListResultResponse,
  ApprovalProjection,
  ChatInputInteractionProjection,
  ChatInteractionListResultResponse,
  ChatInteractionProjection,
  ControlPlaneWebSocketSubscriptionId,
  DeliveryGetResultResponse,
  DeliveryId,
  DeliveryListResultResponse,
  DeliveryProjection,
  Instant,
  OpaqueCursor,
  ProductSessionId,
  ProductSessionProjection,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
  SessionListResultResponse,
  StageRunId,
} from './generated/contracts.js'
import {
  ControlPlaneWebSocketEventType,
  QueryName,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const PAGE_SIZE = 200
const MAX_PAGES = 10
const SESSION_STATES = Object.freeze(['waiting_for_input', 'waiting_for_approval'] as const)
const INTERACTION_STATES = Object.freeze(['pending', 'expired'] as const)
const APPROVAL_STATES = Object.freeze(['pending', 'expired'] as const)

export type AttentionCenterStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'refreshing'
  | 'authentication-required'
  | 'authorization-denied'
  | 'cancelled'
  | 'error'
  | 'closed'

export type AttentionCenterRealtimeStatus =
  | 'inactive'
  | 'subscribed'
  | 'reloading'
  | 'reconnecting'
  | 'access-revoked'
  | 'closed'

export type AttentionCenterItemKind = 'input' | 'approval' | 'attention'

export type AttentionCenterUrgency =
  | 'blocking'
  | 'pending'
  | 'expired'
  | 'binding-invalid'

export interface AttentionCenterItem {
  readonly kind: AttentionCenterItemKind
  readonly id: string
  readonly title: string
  readonly blocking: boolean
  readonly expired: boolean
  readonly bindingValid: boolean
  readonly urgency: AttentionCenterUrgency
  readonly createdAt: Instant | null
  readonly expiresAt: Instant | null
  readonly productSessionId: ProductSessionId | null
  readonly sessionTitle: string | null
  readonly stageRunId: StageRunId | null
  readonly executionJobId: string | null
  readonly deliveryId: DeliveryId | null
  readonly deliveryTitle: string | null
  readonly candidateBound: boolean
  readonly revision: number
}

export interface AttentionCenterViewModelState {
  readonly status: AttentionCenterStatus
  readonly realtime: AttentionCenterRealtimeStatus
  readonly items: readonly AttentionCenterItem[]
  readonly error: ControlPlaneClientError | null
}

export type AttentionCenterListener = (state: AttentionCenterViewModelState) => void

export interface AttentionCenterViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  readonly nowMillis?: () => number
}

export interface AttentionCenterViewModel {
  readonly state: AttentionCenterViewModelState
  subscribe(listener: AttentionCenterListener): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  cancelPending(): void
  reconnect(): void
  close(): void
}

function initialState(): AttentionCenterViewModelState {
  return Object.freeze({
    status: 'idle',
    realtime: 'inactive',
    items: Object.freeze([]),
    error: null,
  })
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

function normalizedError(error: unknown, signal?: AbortSignal): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) return error
  if (signal?.aborted === true) return new ControlPlaneClientError({
    kind: 'cancelled',
    code: 'REQUEST_CANCELLED',
    message: 'The attention center request was cancelled.',
    requestId: null,
    retryable: false,
    cause: error,
  })
  return clientFailure(
    'ATTENTION_CENTER_FAILURE',
    'The Attention Center could not be updated.',
    error,
  )
}

function statusForError(error: ControlPlaneClientError): AttentionCenterStatus {
  if (error.kind === 'authentication') return 'authentication-required'
  if (error.kind === 'authorization') return 'authorization-denied'
  if (error.kind === 'cancelled') return 'cancelled'
  return 'error'
}

function page(cursor: OpaqueCursor | null) {
  return Object.freeze({ cursor, limit: PAGE_SIZE })
}

function expectQuery<Query extends QueryResultResponse['query']>(
  response: QueryResultResponse,
  query: Query,
): Extract<QueryResultResponse, { readonly query: Query }> {
  if (response.query !== query) throw clientFailure(
    'ATTENTION_CENTER_QUERY_MISMATCH',
    'The Control Plane returned another Attention Center query result.',
  )
  return response as Extract<QueryResultResponse, { readonly query: Query }>
}

type PagedResponse = {
  readonly page: { readonly hasMore: boolean; readonly nextCursor: OpaqueCursor | null }
}

function cursorAfter(response: PagedResponse, seen: Set<OpaqueCursor>): OpaqueCursor | null {
  if (!response.page.hasMore) {
    if (response.page.nextCursor !== null) throw clientFailure(
      'ATTENTION_CENTER_PAGE_INVALID',
      'The final Attention Center page returned an unexpected cursor.',
    )
    return null
  }
  const next = response.page.nextCursor
  if (next === null || seen.has(next)) throw clientFailure(
    'ATTENTION_CENTER_CURSOR_INVALID',
    'The Attention Center list returned an invalid continuation cursor.',
  )
  seen.add(next)
  return next
}

function expired(expiresAt: string, nowMillis: number): boolean {
  const instant = Date.parse(expiresAt)
  return !Number.isFinite(instant) || instant <= nowMillis
}

/** Complete ProductSession binding check, shared with the local decision surface. */
function bindingIsComplete(binding: {
  readonly productSessionId: ProductSessionId
  readonly sessionIdentity: { readonly productSessionId: ProductSessionId; readonly workerSessionId: string }
  readonly workerSessionId: string
}): boolean {
  return binding.productSessionId === binding.sessionIdentity.productSessionId
    && binding.workerSessionId === binding.sessionIdentity.workerSessionId
}

function urgencyOf(item: {
  readonly blocking: boolean
  readonly expired: boolean
  readonly bindingValid: boolean
}): AttentionCenterUrgency {
  if (!item.bindingValid) return 'binding-invalid'
  if (item.expired) return 'expired'
  if (item.blocking) return 'blocking'
  return 'pending'
}

const URGENCY_RANK: Readonly<Record<AttentionCenterUrgency, number>> = Object.freeze({
  blocking: 0,
  pending: 1,
  expired: 2,
  'binding-invalid': 3,
})

/** Canonical browsing order: blocking first, soonest expiry next, stable by identity. */
export function orderedAttentionCenterItems(
  items: readonly AttentionCenterItem[],
): readonly AttentionCenterItem[] {
  return Object.freeze([...items].sort((left, right) => {
    const rank = URGENCY_RANK[left.urgency] - URGENCY_RANK[right.urgency]
    if (rank !== 0) return rank
    const leftExpiry = left.expiresAt === null ? Number.POSITIVE_INFINITY : Date.parse(left.expiresAt)
    const rightExpiry = right.expiresAt === null
      ? Number.POSITIVE_INFINITY
      : Date.parse(right.expiresAt)
    if (leftExpiry !== rightExpiry) return leftExpiry - rightExpiry
    if (left.kind !== right.kind) return left.kind.localeCompare(right.kind)
    return left.id.localeCompare(right.id)
  }))
}

interface InputSource {
  readonly session: ProductSessionId
  readonly title: string
}

function inputItem(
  projection: ChatInputInteractionProjection,
  source: InputSource,
  clock: number,
): AttentionCenterItem {
  const bindingValid = bindingIsComplete(projection.binding)
    && projection.binding.productSessionId === source.session
  const isExpired = projection.state !== 'pending' || expired(projection.expiresAt, clock)
  const item: AttentionCenterItem = {
    kind: 'input',
    id: projection.inputRequestId,
    title: projection.prompt,
    blocking: false,
    expired: isExpired,
    bindingValid,
    urgency: 'pending',
    createdAt: null,
    expiresAt: projection.expiresAt,
    productSessionId: source.session,
    sessionTitle: source.title,
    stageRunId: projection.binding.sessionIdentity.stageRunId ?? null,
    executionJobId: projection.binding.executionJobId,
    deliveryId: null,
    deliveryTitle: null,
    candidateBound: false,
    revision: projection.revision,
  }
  return Object.freeze({ ...item, urgency: urgencyOf(item) })
}

function approvalItem(
  projection: ApprovalProjection,
  titles: Map<string, string>,
  clock: number,
  scopedSessionIds: ReadonlySet<string>,
): AttentionCenterItem {
  const bindingValid = bindingIsComplete(projection.binding)
    && scopedSessionIds.has(projection.binding.productSessionId)
  const isExpired = projection.state !== 'pending' || expired(projection.expiresAt, clock)
  const item: AttentionCenterItem = {
    kind: 'approval',
    id: projection.id,
    title: projection.subject,
    blocking: false,
    expired: isExpired,
    bindingValid,
    urgency: 'pending',
    createdAt: projection.requestedAt,
    expiresAt: projection.expiresAt,
    productSessionId: projection.binding.productSessionId,
    sessionTitle: titles.get(projection.binding.productSessionId) ?? null,
    stageRunId: projection.binding.sessionIdentity.stageRunId ?? null,
    executionJobId: projection.binding.executionJobId,
    deliveryId: null,
    deliveryTitle: null,
    candidateBound: false,
    revision: projection.revision,
  }
  return Object.freeze({ ...item, urgency: urgencyOf(item) })
}

interface AttentionSource {
  readonly deliveryId: DeliveryId
  readonly deliveryTitle: string
  readonly deliveryRevision: number
  readonly candidateBound: boolean
}

/** Generic fail-closed placeholder for a Delivery whose ownership left the selected Scope:
    no foreign title, identifier, or link enters the rendered card. */
function bindingInvalidAttention(summary: DeliveryProjection): AttentionCenterItem {
  const item: AttentionCenterItem = {
    kind: 'attention',
    id: `delivery ${summary.deliveryId}`,
    title: 'Delivery outside the current repository Scope',
    blocking: true,
    expired: false,
    bindingValid: false,
    urgency: 'pending',
    createdAt: null,
    expiresAt: null,
    productSessionId: null,
    sessionTitle: null,
    stageRunId: null,
    executionJobId: null,
    deliveryId: null,
    deliveryTitle: null,
    candidateBound: false,
    revision: summary.revision,
  }
  return Object.freeze({ ...item, urgency: urgencyOf(item) })
}

function attentionItems(
  detail: DeliveryGetResultResponse['result'],
  source: AttentionSource,
): readonly AttentionCenterItem[] {
  return Object.freeze(detail.attention
    .filter(projection => projection.status === 'open')
    .map(projection => {
      const item: AttentionCenterItem = {
        kind: 'attention',
        id: projection.id,
        title: projection.title,
        blocking: projection.blocking,
        expired: false,
        bindingValid: true,
        urgency: 'pending',
        createdAt: projection.createdAt,
        expiresAt: null,
        productSessionId: null,
        sessionTitle: null,
        stageRunId: projection.stageRunId,
        executionJobId: null,
        deliveryId: source.deliveryId,
        deliveryTitle: source.deliveryTitle,
        candidateBound: source.candidateBound,
        revision: source.deliveryRevision,
      }
      return Object.freeze({ ...item, urgency: urgencyOf(item) })
    }))
}

/** Build the global Attention Center from authoritative Control Plane projections only. */
export function createAttentionCenterViewModel(
  options: AttentionCenterViewModelOptions,
): AttentionCenterViewModel {
  const queryCache = createQueryCacheLifecycle(options)
  const listeners = new Set<AttentionCenterListener>()
  const controllers = new Set<AbortController>()
  let subscription: ControlPlaneSubscription | null = null
  const nowMillis = options.nowMillis ?? Date.now
  let currentState = initialState()
  let generation = 0
  let closed = false

  function publish(state: AttentionCenterViewModelState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<AttentionCenterViewModelState>): void {
    publish({ ...currentState, ...update })
  }

  function controller(): AbortController {
    const value = new AbortController()
    controllers.add(value)
    return value
  }

  function release(value: AbortController): void {
    controllers.delete(value)
  }

  function abortRequests(): void {
    for (const active of controllers) active.abort()
    controllers.clear()
  }

  function isCurrent(ownGeneration: number): boolean {
    return !closed && ownGeneration === generation
  }

  function requestBase() {
    return {
      schemaVersion: SCHEMA_VERSION,
      actor: options.actor,
      scope: options.scope,
    }
  }

  async function approvalItems(signal: AbortSignal): Promise<readonly ApprovalProjection[]> {
    const items: ApprovalProjection[] = []
    const seen = new Set<OpaqueCursor>()
    let cursor: OpaqueCursor | null = null
    for (let index = 0; index < MAX_PAGES; index += 1) {
      const response: ApprovalListResultResponse = expectQuery(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.ApprovalList,
        parameters: { states: APPROVAL_STATES },
        page: page(cursor),
      }, { signal }), QueryName.ApprovalList)
      items.push(...response.result.items)
      cursor = cursorAfter(response, seen)
      if (cursor === null) return Object.freeze(items)
    }
    throw clientFailure(
      'ATTENTION_CENTER_PAGE_LIMIT_EXCEEDED',
      'The Approval list exceeded the bounded page limit.',
    )
  }

  async function sessionItems(signal: AbortSignal): Promise<readonly ProductSessionProjection[]> {
    const items: ProductSessionProjection[] = []
    const seen = new Set<OpaqueCursor>()
    let cursor: OpaqueCursor | null = null
    for (let index = 0; index < MAX_PAGES; index += 1) {
      const response: SessionListResultResponse = expectQuery(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.SessionList,
        parameters: { states: SESSION_STATES },
        page: page(cursor),
      }, { signal }), QueryName.SessionList)
      items.push(...response.result.items)
      cursor = cursorAfter(response, seen)
      if (cursor === null) return Object.freeze(items)
    }
    throw clientFailure(
      'ATTENTION_CENTER_PAGE_LIMIT_EXCEEDED',
      'The ProductSession list exceeded the bounded page limit.',
    )
  }

  async function deliverySummaries(signal: AbortSignal): Promise<readonly DeliveryProjection[]> {
    const items: DeliveryProjection[] = []
    const seen = new Set<OpaqueCursor>()
    let cursor: OpaqueCursor | null = null
    for (let index = 0; index < MAX_PAGES; index += 1) {
      const response: DeliveryListResultResponse = expectQuery(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.DeliveryList,
        parameters: { states: [] },
        page: page(cursor),
      }, { signal }), QueryName.DeliveryList)
      items.push(...response.result.items)
      cursor = cursorAfter(response, seen)
      if (cursor === null) return Object.freeze(items)
    }
    throw clientFailure(
      'ATTENTION_CENTER_PAGE_LIMIT_EXCEEDED',
      'The Delivery list exceeded the bounded page limit.',
    )
  }

  async function inputItems(
    signal: AbortSignal,
    sessions: readonly InputSource[],
  ): Promise<readonly AttentionCenterItem[]> {
    const clock = nowMillis()
    const pages = await Promise.all(sessions.map(async source => {
      const items: ChatInteractionProjection[] = []
      const seen = new Set<OpaqueCursor>()
      let cursor: OpaqueCursor | null = null
      for (let index = 0; index < MAX_PAGES; index += 1) {
        const response: ChatInteractionListResultResponse = expectQuery(await options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionInteractionsList,
          parameters: {
            productSessionId: source.session,
            states: INTERACTION_STATES,
          },
          page: page(cursor),
        }, { signal }), QueryName.SessionInteractionsList)
        items.push(...response.result.items)
        cursor = cursorAfter(response, seen)
        if (cursor === null) return Object.freeze(items
          .filter(item => item.kind === 'input')
          .map(item => inputItem(item, source, clock)))
      }
      throw clientFailure(
        'ATTENTION_CENTER_PAGE_LIMIT_EXCEEDED',
        'The input interaction list exceeded the bounded page limit.',
      )
    }))
    return Object.freeze(pages.flat())
  }

  async function attentionItemsFor(
    signal: AbortSignal,
    summaries: readonly DeliveryProjection[],
  ): Promise<readonly AttentionCenterItem[]> {
    const withOpenAttention = summaries.filter(summary => summary.openAttentionCount > 0)
    const details = await Promise.all(withOpenAttention.map(async summary => {
      const response: DeliveryGetResultResponse = expectQuery(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.DeliveryGet,
        parameters: { deliveryId: summary.deliveryId },
        page: page(null),
      }, { signal }), QueryName.DeliveryGet)
      if (response.page.hasMore || response.page.nextCursor !== null) throw clientFailure(
        'ATTENTION_CENTER_PAGE_INVALID',
        'The Attention detail returned an unexpected page cursor.',
      )
      return { summary, detail: response.result }
    }))
    return Object.freeze(details.flatMap(({ summary, detail }) => {
      const ownershipMatches = detail !== undefined
        && detail.deliveryId === summary.deliveryId
        && detail.ownership.organizationId === options.scope.organizationId
        && detail.ownership.workspaceId === options.scope.workspaceId
        && detail.ownership.projectId === options.scope.projectId
        && detail.ownership.repositoryId === options.scope.repositoryId
      if (!ownershipMatches) return [bindingInvalidAttention(summary)]
      return attentionItems(detail, {
        deliveryId: summary.deliveryId,
        deliveryTitle: summary.title,
        deliveryRevision: detail.deliveryRevision,
        candidateBound: detail.currentCandidate !== null,
      })
    }))
  }

  async function snapshot(signal: AbortSignal): Promise<readonly AttentionCenterItem[]> {
    const [approvalValues, sessionValues, deliveryValues] = await Promise.all([
      approvalItems(signal),
      sessionItems(signal),
      deliverySummaries(signal),
    ])
    const clock = nowMillis()
    // Exact Scope check: a joined ProductSession outside the selected repository
    // never contributes titles, inputs, or interaction queries.
    const scopedSessions = sessionValues.filter(session =>
      session.projectId === options.scope.projectId
      && session.repositoryId === options.scope.repositoryId)
    const titles = new Map(scopedSessions.map(session => [session.id, session.title]))
    const scopedSessionIds: ReadonlySet<string> = new Set(scopedSessions.map(session => session.id))
    const inputSources: readonly InputSource[] = scopedSessions
      .filter(session => session.state === 'waiting_for_input')
      .map(session => ({ session: session.id, title: session.title }))
    const seenApprovals = new Set<string>()
    for (const approval of approvalValues) {
      if (seenApprovals.has(approval.id)) throw clientFailure(
        'ATTENTION_CENTER_APPROVAL_BINDING_INVALID',
        'The Approval list contains a duplicate decision identity.',
      )
      seenApprovals.add(approval.id)
    }
    const inputs = await inputItems(signal, inputSources)
    const attention = await attentionItemsFor(signal, deliveryValues)
    return orderedAttentionCenterItems([
      ...inputs,
      ...approvalValues.map(approval => approvalItem(approval, titles, clock, scopedSessionIds)),
      ...attention,
    ])
  }

  async function load(replace: boolean, realtimeStatus: AttentionCenterRealtimeStatus): Promise<void> {
    if (closed) throw clientFailure(
      'ATTENTION_CENTER_CLOSED',
      'The Attention Center view is closed.',
    )
    generation += 1
    const ownGeneration = generation
    abortRequests()
    const active = controller()
    patch({
      status: replace ? 'loading' : 'refreshing',
      realtime: realtimeStatus,
      ...(replace ? { items: Object.freeze([]) } : {}),
      error: null,
    })
    try {
      const items = await snapshot(active.signal)
      if (!isCurrent(ownGeneration)) return
      publish({
        status: 'ready',
        realtime: realtimeStatus === 'reloading' ? 'subscribed' : realtimeStatus,
        items,
        error: null,
      })
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      const normalized = normalizedError(error, active.signal)
      if (normalized.kind === 'authentication' || normalized.kind === 'authorization') {
        revokeAccess(normalized)
        return
      }
      patch({
        status: statusForError(normalized),
        realtime: subscription === null ? 'inactive' : 'reconnecting',
        error: normalized,
      })
    } finally {
      release(active)
    }
  }

  /** One fail-closed settlement for lost authentication or authorization. */
  function revokeAccess(error: ControlPlaneClientError): void {
    generation += 1
    abortRequests()
    subscription?.close()
    subscription = null
    publish({
      status: statusForError(error),
      realtime: 'access-revoked',
      items: Object.freeze([]),
      error,
    })
  }

  function subscribeRealtime(): void {
    subscription?.close()
    subscription = options.client.subscribe({
      subscriptionId: options.subscriptionId,
      subscription: {
        scope: options.scope,
        stream: { kind: 'scope' },
        eventTypes: [
          ControlPlaneWebSocketEventType.ProductSessionChangedV1,
          ControlPlaneWebSocketEventType.ApprovalChangedV1,
          ControlPlaneWebSocketEventType.ChatInteractionsInvalidatedV1,
          ControlPlaneWebSocketEventType.AttentionChangedV1,
          ControlPlaneWebSocketEventType.DeliveryChangedV1,
        ],
      },
      async onEvent() {
        await load(false, 'reloading')
      },
      onAuthorizationRevoked() {
        revokeAccess(new ControlPlaneClientError({
          kind: 'authentication',
          code: 'AUTHENTICATION_REQUIRED',
          message: 'Attention Center event authorization is no longer valid.',
          requestId: null,
          retryable: false,
        }))
      },
      onError(error: ControlPlaneClientError) {
        if (closed) return
        if (error.kind === 'authentication' || error.kind === 'authorization') {
          revokeAccess(error)
          return
        }
        patch({ realtime: 'reconnecting', error })
      },
    })
    patch({ realtime: 'subscribed' })
  }

  return {
    get state() { return currentState },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async start() {
      await load(true, 'inactive')
      if (currentState.status === 'ready' && !closed) subscribeRealtime()
    },
    async refresh() {
      if (currentState.status === 'authentication-required'
        || currentState.status === 'authorization-denied') return
      queryCache.refresh()
      await load(false, subscription === null ? 'inactive' : 'subscribed')
      if (currentState.status === 'ready' && subscription === null && !closed) subscribeRealtime()
    },
    cancelPending() {
      if (closed) return
      if (currentState.status === 'authentication-required'
        || currentState.status === 'authorization-denied') return
      generation += 1
      abortRequests()
      subscription?.close()
      subscription = null
      const error = new ControlPlaneClientError({
        kind: 'cancelled',
        code: 'REQUEST_CANCELLED',
        message: 'The attention center request was cancelled.',
        requestId: null,
        retryable: false,
      })
      patch({
        status: 'cancelled',
        realtime: 'inactive',
        error,
      })
    },
    reconnect() {
      if (closed) throw clientFailure(
        'ATTENTION_CENTER_CLOSED',
        'The Attention Center view is closed.',
      )
      if (subscription === null) throw clientFailure(
        'ATTENTION_CENTER_SUBSCRIPTION_INACTIVE',
        'Attention Center events are not active.',
      )
      patch({ realtime: 'reconnecting', error: null })
      subscription.reconnect()
      void load(false, 'reloading')
    },
    close() {
      if (closed) return
      closed = true
      queryCache.close()
      generation += 1
      abortRequests()
      subscription?.close()
      subscription = null
      publish({
        status: 'closed',
        realtime: 'closed',
        items: Object.freeze([]),
        error: null,
      })
      listeners.clear()
    },
  }
}
