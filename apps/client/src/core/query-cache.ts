// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneRequestOptions,
  type ControlPlaneSubscription,
} from '../control-plane-client.js'
import type {
  Actor,
  ControlPlaneWebSocketEventFrame,
  QueryRequest,
  QueryResultResponse,
  Scope,
} from '../generated/contracts.js'

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

export interface QueryCacheSnapshot {
  readonly key: string
  readonly response: QueryResultResponse
  readonly status: 'fresh' | 'stale'
}

export interface QueryCache {
  readonly client: ControlPlaneClient
  peek(query: QueryRequest): QueryCacheSnapshot | null
  invalidate(invalidation: QueryInvalidation): void
  clear(reason?: QueryInvalidationReason): void
  close(): void
}

const QUERY_CACHES = new WeakMap<ControlPlaneClient, QueryCache>()

/** Invalidates a cached facade when present and is a no-op for an injected uncached facade. */
export function invalidateClientQueryCache(
  client: ControlPlaneClient,
  invalidation: QueryInvalidation,
): void {
  QUERY_CACHES.get(client)?.invalidate(invalidation)
}

interface CachedSnapshot {
  readonly response: QueryResultResponse
  readonly version: number
}

interface QueryFlight {
  readonly controller: AbortController
  consumers: number
  invalidated: boolean
  nextRequest: QueryRequest | null
  promise: Promise<QueryResultResponse>
  settled: boolean
}

interface QueryEntry {
  readonly actor: string
  readonly key: string
  readonly query: QueryRequest['query']
  readonly scope: string
  flight: QueryFlight | null
  snapshot: CachedSnapshot | null
  version: number
}

function clientFailure(
  code: string,
  message: string,
  requestId: QueryRequest['requestId'] | null = null,
): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code,
    message,
    requestId,
    retryable: false,
  })
}

function cancelled(requestId: QueryRequest['requestId']): ControlPlaneClientError {
  return new ControlPlaneClientError({
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

function correlate(
  response: QueryResultResponse,
  query: QueryRequest,
): QueryResultResponse {
  if (response.requestId === query.requestId) return response
  return Object.freeze({ ...response, requestId: query.requestId }) as QueryResultResponse
}

function requireCorrelation(
  response: QueryResultResponse,
  query: QueryRequest,
): QueryResultResponse {
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

export function createQueryCache(options: { readonly client: ControlPlaneClient }): QueryCache {
  const entries = new Map<string, QueryEntry>()
  const subscriptions = new Set<ControlPlaneSubscription>()
  let closed = false

  function entryFor(query: QueryRequest): QueryEntry {
    const key = queryCacheKey(query)
    const existing = entries.get(key)
    if (existing !== undefined) return existing
    const created: QueryEntry = {
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

  function matches(entry: QueryEntry, invalidation: QueryInvalidation): boolean {
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

  function clear(_reason: QueryInvalidationReason = 'manual'): void {
    for (const entry of entries.values()) entry.flight?.controller.abort()
    entries.clear()
  }

  async function executeFlight(entry: QueryEntry, flight: QueryFlight, initial: QueryRequest) {
    let request = initial
    for (;;) {
      const response = requireCorrelation(
        await options.client.query(request, { signal: flight.controller.signal }),
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

  function startFlight(entry: QueryEntry, request: QueryRequest): QueryFlight {
    const controller = new AbortController()
    const flight: QueryFlight = {
      controller,
      consumers: 0,
      invalidated: false,
      nextRequest: null,
      promise: Promise.resolve(null as unknown as QueryResultResponse),
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
    entry: QueryEntry,
    flight: QueryFlight,
    query: QueryRequest,
    requestOptions?: ControlPlaneRequestOptions,
  ): Promise<QueryResultResponse> {
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
    query: QueryRequest,
    requestOptions?: ControlPlaneRequestOptions,
  ): Promise<QueryResultResponse> {
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
    options.client.close()
  }

  const client: ControlPlaneClient = {
    serverUrl: options.client.serverUrl,
    async restore(requestOptions) {
      const session = await options.client.restore(requestOptions)
      clear('authorization-epoch')
      return session
    },
    async login(bootstrapProof, requestOptions) {
      const session = await options.client.login(bootstrapProof, requestOptions)
      clear('authorization-epoch')
      return session
    },
    async logout(requestOptions) {
      clear('authorization-epoch')
      await options.client.logout(requestOptions)
    },
    async command(command, requestOptions) {
      const response = await options.client.command(command, requestOptions)
      invalidate({ actor: command.actor, scope: command.scope, reason: 'command' })
      return response
    },
    query: cachedQuery,
    subscribe(subscriptionOptions) {
      const scope = subscriptionOptions.subscription.scope
      let authorizationEpoch: number | null = null
      let active = true
      let raw: ControlPlaneSubscription
      raw = options.client.subscribe({
        ...subscriptionOptions,
        async onEvent(frame) {
          const epochChanged = authorizationEpoch !== null
            && frame.authorizationEpoch !== authorizationEpoch
          authorizationEpoch = frame.authorizationEpoch
          if (epochChanged) discardScope(scope, 'authorization-epoch')
          else {
            const queries = reloadQueries(frame)
            invalidate({ scope, reason: 'event', ...(queries === undefined ? {} : { queries }) })
          }
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

  const cache: QueryCache = {
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
    clear,
    close,
  }
  QUERY_CACHES.set(client, cache)
  return Object.freeze(cache)
}
