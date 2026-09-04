// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './control-plane-client.js'
import type {
  Actor,
  OrganizationId,
  ProviderAccountConnectionId,
  ProviderAccountConnectionListResultResponse,
  ProviderAccountConnectionProjection,
  ProviderAccountOwner,
  ProviderAccountPoolId,
  ProviderAccountPoolListResultResponse,
  ProviderAccountPoolProjection,
  RequestId,
  Scope,
} from './generated/contracts.js'
import { CommandName, QueryName } from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const

export type ProviderAccountViewStatus = 'idle' | 'loading' | 'ready' | 'error' | 'closed'

export interface ProviderAccountViewModelState {
  readonly status: ProviderAccountViewStatus
  readonly connections: readonly ProviderAccountConnectionProjection[]
  readonly pools: readonly ProviderAccountPoolProjection[]
  readonly submitting: boolean
  readonly error: ControlPlaneClientError | null
}

export interface ProviderAccountViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: Scope
  readonly nextRequestId: () => RequestId
}

export interface ProviderAccountPoolInput {
  readonly id: ProviderAccountPoolId
  readonly revision: number
  readonly displayName: string
  readonly accountConnectionIds: readonly ProviderAccountConnectionId[]
  readonly allowedModelIds: readonly string[]
  readonly maxConcurrentPerAccount: number
  readonly monthlyTokenLimitPerAccount: number
  readonly sourcePolicy: 'enterprise_only' | 'allow_personal_default_personal' | 'allow_personal_default_pool'
}

export interface ProviderAccountViewModel {
  readonly state: ProviderAccountViewModelState
  readonly organizationId: OrganizationId
  subscribe(listener: (state: ProviderAccountViewModelState) => void): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  startPersonalConnection(id: ProviderAccountConnectionId, displayName: string): Promise<void>
  startOrganizationConnection(id: ProviderAccountConnectionId, displayName: string): Promise<void>
  completeConnection(connection: ProviderAccountConnectionProjection): Promise<void>
  refreshConnection(connection: ProviderAccountConnectionProjection): Promise<void>
  revokeConnection(connection: ProviderAccountConnectionProjection): Promise<void>
  upsertPool(input: ProviderAccountPoolInput): Promise<void>
  disablePool(pool: ProviderAccountPoolProjection): Promise<void>
  close(): void
}

function organizationId(scope: Scope): OrganizationId {
  return scope.organizationId
}

function normalizeError(error: unknown): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) return error
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'PROVIDER_ACCOUNT_FAILURE',
    message: 'Provider account operation failed.',
    requestId: null,
    retryable: false,
    cause: error,
  })
}

function requireUserActor(actor: Actor): Extract<Actor, { readonly kind: 'user' }> {
  if (actor.kind !== 'user') throw new ControlPlaneClientError({
    kind: 'authorization',
    code: 'PROVIDER_ACCOUNT_USER_REQUIRED',
    message: 'A user identity is required for a personal Provider account.',
    requestId: null,
    retryable: false,
  })
  return actor
}

function initialState(): ProviderAccountViewModelState {
  return Object.freeze({
    status: 'idle',
    connections: Object.freeze([]),
    pools: Object.freeze([]),
    submitting: false,
    error: null,
  })
}

/** Owns the secret-free ChatGPT connection and enterprise pool contracts. */
export function createProviderAccountViewModel(
  options: ProviderAccountViewModelOptions,
): ProviderAccountViewModel {
  const listeners = new Set<(state: ProviderAccountViewModelState) => void>()
  const controllers = new Set<AbortController>()
  let current = initialState()
  let generation = 0
  let closed = false

  function publish(update: Partial<ProviderAccountViewModelState>): void {
    current = Object.freeze({ ...current, ...update })
    for (const listener of listeners) listener(current)
  }

  function controller(): AbortController {
    const value = new AbortController()
    controllers.add(value)
    return value
  }

  async function refresh(): Promise<void> {
    if (closed) return
    const ownGeneration = ++generation
    const active = controller()
    publish({ status: 'loading', error: null })
    try {
      const [connectionsValue, poolsValue] = await Promise.all([
        options.client.query({
          schemaVersion: SCHEMA_VERSION,
          requestId: options.nextRequestId(),
          actor: options.actor,
          scope: options.scope,
          query: QueryName.ProviderAccountConnectionList,
          parameters: { states: [] },
          page: { cursor: null, limit: 200 },
        }, { signal: active.signal }),
        options.client.query({
          schemaVersion: SCHEMA_VERSION,
          requestId: options.nextRequestId(),
          actor: options.actor,
          scope: options.scope,
          query: QueryName.ProviderAccountPoolList,
          parameters: { enabled: null },
          page: { cursor: null, limit: 200 },
        }, { signal: active.signal }),
      ])
      if (closed || ownGeneration !== generation) return
      if (
        connectionsValue.query !== QueryName.ProviderAccountConnectionList
        || poolsValue.query !== QueryName.ProviderAccountPoolList
      ) throw new Error('Provider account query response mismatch.')
      const connections = (connectionsValue as ProviderAccountConnectionListResultResponse).result.items
      const pools = (poolsValue as ProviderAccountPoolListResultResponse).result.items
      publish({
        status: 'ready',
        connections: Object.freeze([...connections]),
        pools: Object.freeze([...pools]),
        error: null,
      })
    } catch (error) {
      if (closed || ownGeneration !== generation) return
      publish({ status: 'error', error: normalizeError(error) })
    } finally {
      controllers.delete(active)
    }
  }

  async function mutate(request: Parameters<ControlPlaneClient['command']>[0]): Promise<void> {
    if (closed || current.submitting) return
    const active = controller()
    publish({ submitting: true, error: null })
    try {
      const response = await options.client.command(request, { signal: active.signal })
      if (response.outcome !== 'completed') {
        throw new Error('Provider account command was not completed synchronously.')
      }
      publish({ submitting: false })
      await refresh()
    } catch (error) {
      if (!closed) publish({ submitting: false, error: normalizeError(error) })
    } finally {
      controllers.delete(active)
    }
  }

  function startConnection(
    id: ProviderAccountConnectionId,
    displayName: string,
    owner: ProviderAccountOwner,
  ): Promise<void> {
    return mutate({
      schemaVersion: SCHEMA_VERSION,
      requestId: options.nextRequestId(),
      actor: options.actor,
      scope: options.scope,
      command: CommandName.ProviderAccountConnectionStart,
      expectedRevision: 0,
      payload: {
        accountConnectionId: id,
        displayName: displayName.trim(),
        loginMethod: 'chatgpt_device_code',
        owner,
        providerId: 'openai',
      },
    })
  }

  function connectionMutation(
    command: typeof CommandName.ProviderAccountConnectionComplete
      | typeof CommandName.ProviderAccountConnectionRefresh
      | typeof CommandName.ProviderAccountConnectionRevoke,
    connection: ProviderAccountConnectionProjection,
  ): Promise<void> {
    return mutate({
      schemaVersion: SCHEMA_VERSION,
      requestId: options.nextRequestId(),
      actor: options.actor,
      scope: options.scope,
      command,
      expectedRevision: connection.revision,
      payload: { accountConnectionId: connection.id },
    })
  }

  return {
    get state() { return current },
    organizationId: organizationId(options.scope),
    subscribe(listener) {
      listeners.add(listener)
      listener(current)
      return () => { listeners.delete(listener) }
    },
    start: refresh,
    refresh,
    startPersonalConnection(id, displayName) {
      const actor = requireUserActor(options.actor)
      return startConnection(id, displayName, { kind: 'user', userId: actor.id })
    },
    startOrganizationConnection(id, displayName) {
      return startConnection(id, displayName, {
        kind: 'organization',
        organizationId: organizationId(options.scope),
      })
    },
    completeConnection(connection) {
      return connectionMutation(CommandName.ProviderAccountConnectionComplete, connection)
    },
    refreshConnection(connection) {
      return connectionMutation(CommandName.ProviderAccountConnectionRefresh, connection)
    },
    revokeConnection(connection) {
      return connectionMutation(CommandName.ProviderAccountConnectionRevoke, connection)
    },
    upsertPool(input) {
      return mutate({
        schemaVersion: SCHEMA_VERSION,
        requestId: options.nextRequestId(),
        actor: options.actor,
        scope: options.scope,
        command: CommandName.ProviderAccountPoolUpsert,
        expectedRevision: input.revision,
        payload: {
          accountPoolId: input.id,
          displayName: input.displayName.trim(),
          accountConnectionIds: input.accountConnectionIds,
          allowedModelIds: input.allowedModelIds.map(value => value.trim()).filter(Boolean),
          maxConcurrentPerAccount: input.maxConcurrentPerAccount,
          monthlyTokenLimitPerAccount: input.monthlyTokenLimitPerAccount,
          sourcePolicy: input.sourcePolicy,
        },
      })
    },
    disablePool(pool) {
      return mutate({
        schemaVersion: SCHEMA_VERSION,
        requestId: options.nextRequestId(),
        actor: options.actor,
        scope: options.scope,
        command: CommandName.ProviderAccountPoolDisable,
        expectedRevision: pool.revision,
        payload: { accountPoolId: pool.id },
      })
    },
    close() {
      if (closed) return
      closed = true
      generation += 1
      for (const active of controllers) active.abort()
      controllers.clear()
      listeners.clear()
      current = Object.freeze({ ...current, status: 'closed', submitting: false })
    },
  }
}
