// SPDX-License-Identifier: Apache-2.0

import type {
  Actor,
  BrowserControlClientErrorKind,
  BrowserControlRequestOptions as ControlPlaneRequestOptions,
  BrowserControlSubscribeOptions,
  BrowserControlSubscription as ControlPlaneSubscription,
  ControlPlaneAuthSession,
  ControlPlaneCommandRequest,
  ControlPlaneCommandResponse,
  ControlPlaneWebSocketAuthorizationRevokedFrame,
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketResetRequiredFrame,
  ControlPlaneWebSocketSubscribeStartAt,
  ControlPlaneWebSocketSubscription,
  ControlPlaneWebSocketSubscriptionId,
  EventReadCursor,
  QueryCacheClientPort,
  Scope,
  ControlPlaneQueryRequest as QueryRequest,
  ControlPlaneQueryResponse as QueryResultResponse,
} from '@winwincode/contracts/browser-control'

export type QueryInvalidationReason =
  | 'authorization-epoch'
  | 'command'
  | 'event'
  | 'manual'
  | 'reconnect'
  | 'retention-loss'

export interface QueryInvalidation {
  readonly reason: QueryInvalidationReason
  readonly actor?: Actor
  readonly scope?: Scope
  readonly queries?: readonly QueryRequest['query'][]
  /** Security and retention boundaries discard the handoff snapshot as well as freshness. */
  readonly discard?: boolean
}

export interface QueryCacheSnapshot<
  ProductQueryResponse extends QueryResultResponse = QueryResultResponse,
> {
  readonly key: string
  readonly response: ProductQueryResponse
  readonly status: 'fresh' | 'stale'
}

export interface QueryCache<
  Client extends object,
  ProductQueryRequest extends QueryRequest = QueryRequest,
  ProductQueryResponse extends QueryResultResponse = QueryResultResponse,
> {
  readonly client: Client
  peek(query: ProductQueryRequest): QueryCacheSnapshot<ProductQueryResponse> | null
  invalidate(invalidation: QueryInvalidation): void
  revalidate(query: ProductQueryRequest): void
  clear(reason?: QueryInvalidationReason): void
  close(): void
}

/** Cache-only lifecycle seam shared by feature view-models; it owns no business state. */
export interface QueryCacheLifecycle<
  ProductQueryRequest extends QueryRequest = QueryRequest,
> {
  refresh(queries?: readonly QueryRequest['query'][]): void
  revalidate(...queries: readonly ProductQueryRequest[]): void
  close(): void
}

interface QueryCacheLifecycleTarget {
  invalidate(invalidation: QueryInvalidation): void
  revalidate(query: QueryRequest): void
}

const QUERY_CACHES = new WeakMap<object, QueryCacheLifecycleTarget>()

interface QueryCacheLifecycleClientPort {
  query(...arguments_: never[]): Promise<QueryResultResponse>
}

type LifecycleQueryRequest<Client extends QueryCacheLifecycleClientPort> =
  Parameters<Client['query']>[0] & QueryRequest

export function createQueryCacheLifecycle<Client extends QueryCacheLifecycleClientPort>(options: {
  readonly client: Client
  readonly actor: Actor
  readonly scope: Scope
}): QueryCacheLifecycle<LifecycleQueryRequest<Client>> {
  const actor = actorIdentity(options.actor)
  const scope = scopeIdentity(options.scope)

  function cache(): QueryCacheLifecycleTarget | undefined {
    return QUERY_CACHES.get(options.client)
  }

  return Object.freeze<QueryCacheLifecycle<LifecycleQueryRequest<Client>>>({
    refresh(queries?: readonly QueryRequest['query'][]) {
      cache()?.invalidate({
        actor: options.actor,
        scope: options.scope,
        reason: 'manual',
        ...(queries === undefined ? {} : { queries }),
      })
    },
    revalidate(...queries: readonly QueryRequest[]) {
      for (const query of queries) {
        if (actorIdentity(query.actor) !== actor || scopeIdentity(query.scope) !== scope) {
          throw clientFailure(
            'QUERY_CACHE_LIFECYCLE_MISMATCH',
            'The query retry does not belong to this cache lifecycle.',
            query.requestId,
          )
        }
        cache()?.revalidate(query)
      }
    },
    close() {
      cache()?.invalidate({
        actor: options.actor,
        scope: options.scope,
        reason: 'manual',
        discard: true,
      })
    },
  })
}

interface CachedSnapshot<ProductQueryResponse extends QueryResultResponse> {
  readonly response: ProductQueryResponse
  readonly version: number
}

interface QueryFlight<
  ProductQueryRequest extends QueryRequest,
  ProductQueryResponse extends QueryResultResponse,
> {
  readonly controller: AbortController
  consumers: number
  invalidated: boolean
  nextRequest: ProductQueryRequest | null
  promise: Promise<ProductQueryResponse>
  settled: boolean
}

interface QueryEntry<
  ProductQueryRequest extends QueryRequest,
  ProductQueryResponse extends QueryResultResponse,
> {
  readonly actor: string
  readonly key: string
  readonly query: QueryRequest['query']
  readonly scope: string
  flight: QueryFlight<ProductQueryRequest, ProductQueryResponse> | null
  snapshot: CachedSnapshot<ProductQueryResponse> | null
  version: number
}

interface QueryCacheClientErrorFields {
  readonly kind: BrowserControlClientErrorKind
  readonly code: string
  readonly message: string
  readonly requestId: QueryRequest['requestId'] | null
  readonly retryable: boolean
}

/** Error raised by cache lifecycle and correlation checks before a transport is involved. */
export class QueryCacheClientError extends Error {
  readonly kind: BrowserControlClientErrorKind
  readonly code: string
  readonly requestId: QueryRequest['requestId'] | null
  readonly retryable: boolean
  readonly details = Object.freeze({})

  constructor(fields: QueryCacheClientErrorFields) {
    super(fields.message)
    this.name = 'QueryCacheClientError'
    this.kind = fields.kind
    this.code = fields.code
    this.requestId = fields.requestId
    this.retryable = fields.retryable
  }
}

function clientFailure(
  code: string,
  message: string,
  requestId: QueryRequest['requestId'] | null = null,
): QueryCacheClientError {
  return new QueryCacheClientError({
    kind: 'protocol',
    code,
    message,
    requestId,
    retryable: false,
  })
}

function cancelled(requestId: QueryRequest['requestId']): QueryCacheClientError {
  return new QueryCacheClientError({
    kind: 'cancelled',
    code: 'REQUEST_CANCELLED',
    message: 'The cached query consumer was cancelled.',
    requestId,
    retryable: false,
  })
}

function stableJson(value: unknown, seen = new Set<object>()): string {
  if (value === null) return 'null'
  if (typeof value === 'string' || typeof value === 'boolean') return JSON.stringify(value)
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) throw new TypeError('Query cache keys require finite numbers.')
    return JSON.stringify(value)
  }
  if (typeof value !== 'object') throw new TypeError('Query cache keys require JSON values.')
  if (seen.has(value)) throw new TypeError('Query cache keys cannot contain cycles.')
  seen.add(value)
  try {
    if (Array.isArray(value)) return `[${value.map(item => stableJson(item, seen)).join(',')}]`
    const record = value as Readonly<Record<string, unknown>>
    return `{${Object.keys(record)
      .filter(key => record[key] !== undefined)
      .sort((left, right) => left.localeCompare(right))
      .map(key => `${JSON.stringify(key)}:${stableJson(record[key], seen)}`)
      .join(',')}}`
  } finally {
    seen.delete(value)
  }
}

function actorIdentity(actor: Actor): string {
  return stableJson(actor)
}

function scopeIdentity(scope: Scope): string {
  return stableJson(scope)
}

/**
 * Stable transport-snapshot identity. Request IDs are deliberately excluded so
 * one authoritative snapshot can be handed to several independently correlated callers.
 */
export function queryCacheKey(query: QueryRequest): string {
  return stableJson({
    actor: query.actor,
    page: query.page,
    parameters: query.parameters,
    query: query.query,
    scope: query.scope,
  })
}

function correlate<ProductQueryResponse extends QueryResultResponse>(
  response: ProductQueryResponse,
  query: QueryRequest,
): ProductQueryResponse {
  if (response.requestId === query.requestId) return response
  return Object.freeze({ ...response, requestId: query.requestId })
}

function requireCorrelation<ProductQueryResponse extends QueryResultResponse>(
  response: ProductQueryResponse,
  query: QueryRequest,
): ProductQueryResponse {
  if (
    response.requestId !== query.requestId
    || response.query !== query.query
    || response.schemaVersion !== query.schemaVersion
  ) throw clientFailure(
    'QUERY_CORRELATION_MISMATCH',
    'The Control Plane query response does not match its request envelope.',
    query.requestId,
  )
  return response
}

function reloadQueries(
  frame: ControlPlaneWebSocketEventFrame,
): readonly QueryRequest['query'][] | undefined {
  const value = Reflect.get(frame.event, 'reloadQueries')
  if (!Array.isArray(value) || value.length === 0) return undefined
  return value as readonly QueryRequest['query'][]
}

type QueryCacheCompatibleClient<Client extends object> = Client extends QueryCacheClientPort<
  infer ProductSession,
  infer ProductCommandRequest,
  infer ProductCommandResponse,
  infer ProductQueryRequest,
  infer ProductQueryResponse,
  infer ProductEventFrame,
  infer ProductResetFrame,
  infer ProductAuthorizationRevokedFrame,
  infer ProductSubscription,
  infer ProductSubscriptionId,
  infer ProductStartAt,
  infer ProductEventReadCursor
> ? readonly [ProductQueryRequest, ProductQueryResponse] : never

type QueryCacheQueryRequest<Client extends object> = QueryCacheCompatibleClient<Client>[0]
type QueryCacheQueryResponse<Client extends object> = QueryCacheCompatibleClient<Client>[1]

export function createQueryCache<Client extends object>(
  options: { readonly client: Client } & (
    QueryCacheCompatibleClient<Client> extends never ? never : unknown
  ),
): QueryCache<
  Client,
  QueryCacheQueryRequest<Client>,
  QueryCacheQueryResponse<Client>
>
export function createQueryCache<
  ProductSession extends ControlPlaneAuthSession,
  ProductCommandRequest extends ControlPlaneCommandRequest,
  ProductCommandResponse extends ControlPlaneCommandResponse,
  ProductQueryRequest extends QueryRequest,
  ProductQueryResponse extends QueryResultResponse,
  ProductEventFrame extends ControlPlaneWebSocketEventFrame,
  ProductResetFrame extends ControlPlaneWebSocketResetRequiredFrame,
  ProductAuthorizationRevokedFrame extends ControlPlaneWebSocketAuthorizationRevokedFrame,
  ProductSubscription extends ControlPlaneWebSocketSubscription,
  ProductSubscriptionId extends ControlPlaneWebSocketSubscriptionId,
  ProductStartAt extends ControlPlaneWebSocketSubscribeStartAt,
  ProductEventReadCursor extends EventReadCursor,
  Client extends QueryCacheClientPort<
    ProductSession,
    ProductCommandRequest,
    ProductCommandResponse,
    ProductQueryRequest,
    ProductQueryResponse,
    ProductEventFrame,
    ProductResetFrame,
    ProductAuthorizationRevokedFrame,
    ProductSubscription,
    ProductSubscriptionId,
    ProductStartAt,
    ProductEventReadCursor
  >,
>(
  options: { readonly client: Client },
): QueryCache<Client, ProductQueryRequest, ProductQueryResponse> {
  const sourceClient = options.client
  const entries = new Map<string, QueryEntry<ProductQueryRequest, ProductQueryResponse>>()
  const subscriptions = new Set<ControlPlaneSubscription>()
  let closed = false

  function entryFor(
    query: ProductQueryRequest,
  ): QueryEntry<ProductQueryRequest, ProductQueryResponse> {
    const key = queryCacheKey(query)
    const existing = entries.get(key)
    if (existing !== undefined) return existing
    const created: QueryEntry<ProductQueryRequest, ProductQueryResponse> = {
      actor: actorIdentity(query.actor),
      key,
      query: query.query,
      scope: scopeIdentity(query.scope),
      flight: null,
      snapshot: null,
      version: 0,
    }
    entries.set(key, created)
    return created
  }

  function matches(
    entry: QueryEntry<ProductQueryRequest, ProductQueryResponse>,
    invalidation: QueryInvalidation,
  ): boolean {
    return (invalidation.actor === undefined
      || entry.actor === actorIdentity(invalidation.actor))
      && (invalidation.scope === undefined
        || entry.scope === scopeIdentity(invalidation.scope))
      && (invalidation.queries === undefined
        || invalidation.queries.some(query => query === entry.query))
  }

  function invalidate(invalidation: QueryInvalidation): void {
    for (const [key, entry] of entries) {
      if (!matches(entry, invalidation)) continue
      if (invalidation.discard === true) {
        entry.flight?.controller.abort()
        entries.delete(key)
        continue
      }
      if (entry.flight !== null) {
        if (!entry.flight.invalidated) {
          entry.version += 1
          entry.flight.invalidated = true
        }
        continue
      }
      if (entry.snapshot !== null && entry.snapshot.version === entry.version) entry.version += 1
    }
  }

  function revalidate(query: QueryRequest): void {
    const key = queryCacheKey(query)
    const entry = entries.get(key)
    entry?.flight?.controller.abort()
    entries.delete(key)
  }

  function clear(_reason: QueryInvalidationReason = 'manual'): void {
    for (const entry of entries.values()) entry.flight?.controller.abort()
    entries.clear()
  }

  async function executeFlight(
    entry: QueryEntry<ProductQueryRequest, ProductQueryResponse>,
    flight: QueryFlight<ProductQueryRequest, ProductQueryResponse>,
    initial: ProductQueryRequest,
  ) {
    let request = initial
    for (;;) {
      const response = requireCorrelation(
        await sourceClient.query(request, { signal: flight.controller.signal }),
        request,
      )
      if (flight.controller.signal.aborted) throw cancelled(request.requestId)
      if (!flight.invalidated) {
        entry.snapshot = { response, version: entry.version }
        return response
      }
      const next = flight.nextRequest
      if (next === null) return response
      flight.invalidated = false
      flight.nextRequest = null
      request = next
    }
  }

  function startFlight(
    entry: QueryEntry<ProductQueryRequest, ProductQueryResponse>,
    request: ProductQueryRequest,
  ): QueryFlight<ProductQueryRequest, ProductQueryResponse> {
    const controller = new AbortController()
    const flight: QueryFlight<ProductQueryRequest, ProductQueryResponse> = {
      controller,
      consumers: 0,
      invalidated: false,
      nextRequest: null,
      promise: new Promise<ProductQueryResponse>(() => {}),
      settled: false,
    }
    entry.flight = flight
    flight.promise = executeFlight(entry, flight, request).finally(() => {
      flight.settled = true
      if (entry.flight === flight) entry.flight = null
      if (
        entry.snapshot === null
        && entry.flight === null
        && entries.get(entry.key) === entry
      ) entries.delete(entry.key)
    })
    return flight
  }

  function consume(
    entry: QueryEntry<ProductQueryRequest, ProductQueryResponse>,
    flight: QueryFlight<ProductQueryRequest, ProductQueryResponse>,
    query: ProductQueryRequest,
    requestOptions?: ControlPlaneRequestOptions,
  ): Promise<ProductQueryResponse> {
    const signal = requestOptions?.signal
    if (signal?.aborted === true) return Promise.reject(cancelled(query.requestId))
    flight.consumers += 1
    return new Promise((resolve, reject) => {
      let finished = false
      const finish = () => {
        if (finished) return
        finished = true
        signal?.removeEventListener('abort', onAbort)
        flight.consumers -= 1
        if (flight.consumers === 0 && !flight.settled && entry.flight === flight) {
          queueMicrotask(() => {
            if (flight.consumers === 0 && !flight.settled && entry.flight === flight) {
              flight.controller.abort()
            }
          })
        }
      }
      const onAbort = () => {
        finish()
        reject(cancelled(query.requestId))
      }
      signal?.addEventListener('abort', onAbort, { once: true })
      void flight.promise.then(response => {
        if (finished) return
        finish()
        resolve(correlate(response, query))
      }, error => {
        if (finished) return
        finish()
        reject(error)
      })
    })
  }

  function cachedQuery(
    query: ProductQueryRequest,
    requestOptions?: ControlPlaneRequestOptions,
  ): Promise<ProductQueryResponse> {
    if (closed) return Promise.reject(clientFailure(
      'QUERY_CACHE_CLOSED',
      'The query cache is closed.',
      query.requestId,
    ))
    const entry = entryFor(query)
    if (entry.snapshot !== null && entry.snapshot.version === entry.version) {
      if (requestOptions?.signal?.aborted === true) {
        return Promise.reject(cancelled(query.requestId))
      }
      return Promise.resolve(correlate(entry.snapshot.response, query))
    }
    const flight = entry.flight ?? startFlight(entry, query)
    if (flight.invalidated) flight.nextRequest = query
    return consume(entry, flight, query, requestOptions)
  }

  function discardScope(scope: Scope, reason: QueryInvalidationReason): void {
    invalidate({ scope, reason, discard: true })
  }

  function close(): void {
    if (closed) return
    closed = true
    for (const subscription of [...subscriptions]) subscription.close()
    clear()
    QUERY_CACHES.delete(client)
    sourceClient.close()
  }

  const overrides: QueryCacheClientPort<
    ProductSession,
    ProductCommandRequest,
    ProductCommandResponse,
    ProductQueryRequest,
    ProductQueryResponse,
    ProductEventFrame,
    ProductResetFrame,
    ProductAuthorizationRevokedFrame,
    ProductSubscription,
    ProductSubscriptionId,
    ProductStartAt,
    ProductEventReadCursor
  > = {
    serverUrl: sourceClient.serverUrl,
    async restore(requestOptions) {
      const session = await sourceClient.restore(requestOptions)
      clear('authorization-epoch')
      return session
    },
    async initializeOwner(initialization, requestOptions) {
      const session = await sourceClient.initializeOwner(initialization, requestOptions)
      clear('authorization-epoch')
      return session
    },
    async login(credentials, requestOptions) {
      const session = await sourceClient.login(credentials, requestOptions)
      clear('authorization-epoch')
      return session
    },
    changePassword(change, requestOptions) {
      return sourceClient.changePassword(change, requestOptions)
    },
    async initializationStatus(requestOptions) {
      return sourceClient.initializationStatus(requestOptions)
    },
    async logout(requestOptions) {
      clear('authorization-epoch')
      await sourceClient.logout(requestOptions)
    },
    async command(command, requestOptions) {
      const response = await sourceClient.command(command, requestOptions)
      invalidate({ actor: command.actor, scope: command.scope, reason: 'command' })
      return response
    },
    query: cachedQuery,
    subscribe(subscriptionOptions) {
      const scope = subscriptionOptions.subscription.scope
      let authorizationEpoch: number | null = null
      const queuedFrames = new WeakSet<object>()
      let active = true
      let raw: ControlPlaneSubscription
      function invalidateForEvent(frame: ControlPlaneWebSocketEventFrame): void {
        const epochChanged = authorizationEpoch !== null
          && frame.authorizationEpoch !== authorizationEpoch
        authorizationEpoch = frame.authorizationEpoch
        if (epochChanged) discardScope(scope, 'authorization-epoch')
        else {
          const queries = reloadQueries(frame)
          invalidate({ scope, reason: 'event', ...(queries === undefined ? {} : { queries }) })
        }
      }
      raw = sourceClient.subscribe({
        ...subscriptionOptions,
        onEventQueued(frame) {
          queuedFrames.add(frame)
          invalidateForEvent(frame)
          subscriptionOptions.onEventQueued?.(frame)
        },
        async onEvent(frame) {
          if (!queuedFrames.delete(frame)) invalidateForEvent(frame)
          await subscriptionOptions.onEvent(frame)
        },
        async onResetRequired(frame) {
          discardScope(scope, 'retention-loss')
          if (subscriptionOptions.onResetRequired === undefined) throw clientFailure(
            'RESET_REQUIRED',
            'The subscription needs a complete HTTP reload.',
          )
          return subscriptionOptions.onResetRequired(frame)
        },
        async onAuthorizationRevoked(frame) {
          discardScope(scope, 'authorization-epoch')
          await subscriptionOptions.onAuthorizationRevoked?.(frame)
        },
        onError(error) {
          if (error.kind === 'authentication' || error.kind === 'authorization') {
            discardScope(scope, 'authorization-epoch')
          }
          subscriptionOptions.onError?.(error)
        },
      })
      const subscription: ControlPlaneSubscription = {
        get cursor() { return raw.cursor },
        resume() {
          discardScope(scope, 'reconnect')
          raw.resume()
        },
        reconnect() {
          discardScope(scope, 'reconnect')
          raw.reconnect()
        },
        close() {
          if (!active) return
          active = false
          subscriptions.delete(subscription)
          raw.close()
        },
      }
      subscriptions.add(subscription)
      return subscription
    },
    close,
  }

  const client = new Proxy(sourceClient, {
    get(target, property, receiver) {
      if (Reflect.has(overrides, property)) return Reflect.get(overrides, property, overrides)
      const value: unknown = Reflect.get(target, property, receiver)
      return typeof value === 'function' ? value.bind(target) : value
    },
    set(target, property, value, receiver) {
      if (Reflect.has(overrides, property)) {
        return Reflect.set(overrides, property, value, overrides)
      }
      return Reflect.set(target, property, value, receiver)
    },
  })

  const cache: QueryCache<Client, ProductQueryRequest, ProductQueryResponse> = {
    client,
    peek(query: QueryRequest) {
      const entry = entries.get(queryCacheKey(query))
      if (entry?.snapshot === null || entry?.snapshot === undefined) return null
      return Object.freeze({
        key: entry.key,
        response: entry.snapshot.response,
        status: entry.snapshot.version === entry.version ? 'fresh' : 'stale',
      })
    },
    invalidate,
    revalidate,
    clear,
    close,
  }
  QUERY_CACHES.set(client, cache)
  return Object.freeze(cache)
}
