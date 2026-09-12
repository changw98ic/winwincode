// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneSubscription,
} from './community-control-plane-client.js'
import { createQueryCacheLifecycle } from '@winwincode/browser-core/query-cache'
import type {
  Actor,
  CommandAcceptedResponse,
  CommandCompletedResponse,
  ControlPlaneWebSocketSubscriptionId,
  CredentialReferenceCreateCompletedResponse,
  CredentialReferenceId,
  CredentialReferenceListResultResponse,
  CredentialReferenceProjection,
  CredentialReferenceRevokeCompletedResponse,
  CredentialReferenceRotateCompletedResponse,
  ModelRoute,
  OpaqueCursor,
  QueryResultResponse,
  RequestId,
  Scope,
  SettingsGetResultResponse,
  SettingsProjection,
  SettingsUpdateCompletedResponse,
} from './generated/contracts.js'
import {
  CommandName,
  ControlPlaneWebSocketEventType,
  QueryName,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const CREDENTIAL_PAGE_SIZE = 200
const MAX_CREDENTIAL_PAGES = 10

export type SettingsViewStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'refreshing'
  | 'authentication-required'
  | 'authorization-denied'
  | 'cancelled'
  | 'error'
  | 'closed'

export type SettingsRealtimeStatus =
  | 'inactive'
  | 'subscribed'
  | 'reloading'
  | 'reconnecting'
  | 'access-revoked'
  | 'closed'

export type SettingsInteractionStatus = 'idle' | 'submitting' | 'waiting' | 'error'

export interface SettingsInteractionState {
  readonly status: SettingsInteractionStatus
  readonly operation: SettingsOperation | null
  readonly error: ControlPlaneClientError | null
}

export type SettingsOperation =
  | 'settings.update'
  | 'credential.reference.create'
  | 'credential.reference.rotate'
  | 'credential.reference.revoke'

export interface SettingsViewModelState {
  readonly status: SettingsViewStatus
  readonly realtime: SettingsRealtimeStatus
  readonly settings: SettingsProjection | null
  readonly credentials: readonly CredentialReferenceProjection[]
  readonly interaction: SettingsInteractionState
  readonly error: ControlPlaneClientError | null
}

export type SettingsViewModelListener = (state: SettingsViewModelState) => void

export interface SettingsViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: Scope
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
}

export interface SettingsUpdateInput {
  readonly defaultModelRoute: ModelRoute | null
  readonly workerConcurrencyLimit: number
}

export interface CredentialReferenceCreateInput {
  readonly credentialReferenceId: CredentialReferenceId
  readonly displayName: string
  readonly providerId: string
  /** Write-only local secret-store locator. */
  readonly vaultLocator: string
}

export interface CredentialReferenceRotateInput {
  readonly credentialReferenceId: CredentialReferenceId
  /** Write-only local secret-store locator. */
  readonly vaultLocator: string
}

export interface SettingsViewModel {
  readonly state: SettingsViewModelState
  /** Browser draft owner; changes with the authenticated Actor or exact Scope. */
  readonly draftScope: string
  subscribe(listener: SettingsViewModelListener): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  updateSettings(input: SettingsUpdateInput): Promise<void>
  createCredentialReference(input: CredentialReferenceCreateInput): Promise<void>
  rotateCredentialReference(input: CredentialReferenceRotateInput): Promise<void>
  revokeCredentialReference(credentialReferenceId: CredentialReferenceId): Promise<void>
  cancelPending(): void
  reconnect(): void
  close(): void
}

interface SettingsQueryResponses {
  readonly [QueryName.SettingsGet]: SettingsGetResultResponse
  readonly [QueryName.CredentialReferenceList]: CredentialReferenceListResultResponse
}

interface SettingsCommandResponses {
  readonly [CommandName.SettingsUpdate]: SettingsUpdateCompletedResponse
  readonly [CommandName.CredentialReferenceCreate]: CredentialReferenceCreateCompletedResponse
  readonly [CommandName.CredentialReferenceRotate]: CredentialReferenceRotateCompletedResponse
  readonly [CommandName.CredentialReferenceRevoke]: CredentialReferenceRevokeCompletedResponse
}

function frozenInteraction(
  status: SettingsInteractionStatus,
  operation: SettingsOperation | null = null,
  error: ControlPlaneClientError | null = null,
): SettingsInteractionState {
  return Object.freeze({ status, operation, error })
}

function initialState(): SettingsViewModelState {
  return Object.freeze({
    status: 'idle',
    realtime: 'inactive',
    settings: null,
    credentials: Object.freeze([]),
    interaction: frozenInteraction('idle'),
    error: null,
  })
}

function page(cursor: OpaqueCursor | null, limit: number) {
  return Object.freeze({ cursor, limit })
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
    message: '设置请求已取消。',
    requestId: null,
    retryable: false,
    cause: error,
  })
  return clientFailure(
    'SETTINGS_VIEW_MODEL_FAILURE',
    '无法更新设置。',
    error,
  )
}

function statusForError(error: ControlPlaneClientError): SettingsViewStatus {
  if (error.kind === 'authentication') return 'authentication-required'
  if (error.kind === 'authorization') return 'authorization-denied'
  if (error.kind === 'cancelled') return 'cancelled'
  return 'error'
}

function expectQuery<Query extends keyof SettingsQueryResponses>(
  response: QueryResultResponse,
  query: Query,
): SettingsQueryResponses[Query] {
  if (response.query !== query) throw clientFailure(
    'SETTINGS_QUERY_MISMATCH',
    '控制平面返回了其他设置查询结果。',
  )
  return response as SettingsQueryResponses[Query]
}

function expectCompletedCommand<Command extends keyof SettingsCommandResponses>(
  response: CommandAcceptedResponse | CommandCompletedResponse,
  command: Command,
  requestId: RequestId,
): SettingsCommandResponses[Command] | null {
  if (response.requestId !== requestId || response.command !== command) throw clientFailure(
    'SETTINGS_COMMAND_MISMATCH',
    '控制平面返回了其他设置命令结果。',
  )
  if (response.outcome === 'accepted') return null
  return response as SettingsCommandResponses[Command]
}

function orderCredentials(
  credentials: readonly CredentialReferenceProjection[],
): readonly CredentialReferenceProjection[] {
  return Object.freeze([...credentials].sort((left, right) => (
    left.displayName.localeCompare(right.displayName) || left.id.localeCompare(right.id)
  )))
}

function checkedText(value: string, code: string, message: string): string {
  const result = value.trim()
  if (result.length === 0) throw clientFailure(code, message)
  return result
}

function checkedSecret(value: string): string {
  if (value.length === 0) throw clientFailure(
    'CREDENTIAL_SECRET_REQUIRED',
    '提交凭据引用前，请选择本地密钥。',
  )
  return value
}

/** Build the local settings surface from generated Settings and Credential reference contracts. */
export function createSettingsViewModel(options: SettingsViewModelOptions): SettingsViewModel {
  const queryCache = createQueryCacheLifecycle(options)
  const listeners = new Set<SettingsViewModelListener>()
  const controllers = new Set<AbortController>()
  let currentState = initialState()
  let realtime: ControlPlaneSubscription | null = null
  let generation = 0
  let closed = false
  const draftScope = JSON.stringify([options.actor, options.scope])

  function publish(state: SettingsViewModelState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<SettingsViewModelState>): void {
    publish({ ...currentState, ...update })
  }

  function controller(): AbortController {
    const value = new AbortController()
    controllers.add(value)
    return value
  }

  function releaseController(value: AbortController): void {
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

  async function credentialReferences(signal: AbortSignal): Promise<readonly CredentialReferenceProjection[]> {
    const items: CredentialReferenceProjection[] = []
    const seenCursors = new Set<OpaqueCursor>()
    let cursor: OpaqueCursor | null = null
    for (let pageIndex = 0; pageIndex < MAX_CREDENTIAL_PAGES; pageIndex += 1) {
      const response: CredentialReferenceListResultResponse = expectQuery(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.CredentialReferenceList,
        parameters: { providerId: null },
        page: page(cursor, CREDENTIAL_PAGE_SIZE),
      }, { signal }), QueryName.CredentialReferenceList)
      items.push(...response.result.items)
      if (!response.page.hasMore) {
        if (response.page.nextCursor !== null) throw clientFailure(
          'CREDENTIAL_PAGE_INVALID',
          '凭据列表末页返回了意外的游标。',
        )
        return orderCredentials(items)
      }
      const next: OpaqueCursor | null = response.page.nextCursor
      if (next === null || seenCursors.has(next)) throw clientFailure(
        'CREDENTIAL_CURSOR_INVALID',
        '凭据列表返回了无效的续页游标。',
      )
      seenCursors.add(next)
      cursor = next
    }
    throw clientFailure(
      'CREDENTIAL_PAGE_LIMIT_EXCEEDED',
      '凭据列表超过了分页上限。',
    )
  }

  async function snapshot(signal: AbortSignal): Promise<{
    readonly settings: SettingsProjection
    readonly credentials: readonly CredentialReferenceProjection[]
  }> {
    const settings = expectQuery(await options.client.query({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.SettingsGet,
      parameters: {},
      page: page(null, 1),
    }, { signal }), QueryName.SettingsGet)
    if (settings.page.hasMore || settings.page.nextCursor !== null) throw clientFailure(
      'SETTINGS_PAGE_INVALID',
      '设置查询返回了意外的分页游标。',
    )
    return Object.freeze({
      settings: settings.result,
      credentials: await credentialReferences(signal),
    })
  }

  async function load(replace: boolean, realtimeStatus: SettingsRealtimeStatus): Promise<void> {
    if (closed) throw clientFailure('SETTINGS_VIEW_MODEL_CLOSED', '设置视图已关闭。')
    generation += 1
    const ownGeneration = generation
    abortRequests()
    const active = controller()
    patch({
      status: replace ? 'loading' : 'refreshing',
      realtime: realtimeStatus,
      ...(replace ? { settings: null, credentials: Object.freeze([]) } : {}),
      interaction: frozenInteraction('idle'),
      error: null,
    })
    try {
      const value = await snapshot(active.signal)
      if (!isCurrent(ownGeneration)) return
      publish({
        status: 'ready',
        realtime: realtimeStatus === 'reloading' ? 'subscribed' : realtimeStatus,
        settings: value.settings,
        credentials: value.credentials,
        interaction: frozenInteraction('idle'),
        error: null,
      })
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      const normalized = normalizedError(error, active.signal)
      publish({
        ...currentState,
        status: statusForError(normalized),
        realtime: normalized.kind === 'authentication' || normalized.kind === 'authorization'
          ? 'access-revoked'
          : 'reconnecting',
        interaction: frozenInteraction('error', null, normalized),
        error: normalized,
      })
    } finally {
      releaseController(active)
    }
  }

  function accessRevoked(error: ControlPlaneClientError): void {
    generation += 1
    abortRequests()
    realtime?.close()
    realtime = null
    publish({
      status: statusForError(error),
      realtime: 'access-revoked',
      settings: null,
      credentials: Object.freeze([]),
      interaction: frozenInteraction('error', null, error),
      error,
    })
  }

  function subscribeRealtime(): void {
    realtime?.close()
    realtime = options.client.subscribe({
      subscriptionId: options.subscriptionId,
      subscription: {
        scope: options.scope,
        stream: { kind: 'scope' },
        eventTypes: [ControlPlaneWebSocketEventType.ActivityRecordedV1],
      },
      async onEvent() {
        await load(false, 'reloading')
      },
      onAuthorizationRevoked() {
        accessRevoked(new ControlPlaneClientError({
          kind: 'authentication',
          code: 'AUTHENTICATION_REQUIRED',
          message: '设置事件授权已失效。',
          requestId: null,
          retryable: false,
        }))
      },
      onError(error) {
        if (closed) return
        if (error.kind === 'authentication' || error.kind === 'authorization') {
          accessRevoked(error)
          return
        }
        patch({ realtime: 'reconnecting', error })
      },
    })
    patch({ realtime: 'subscribed' })
  }

  function interactionFailure(code: string, message: string, operation: SettingsOperation): void {
    patch({ interaction: frozenInteraction('error', operation, clientFailure(code, message)) })
  }

  async function runCommand<Command extends keyof SettingsCommandResponses>(
    command: Command,
    expectedRevision: number,
    request: (requestId: RequestId) => Parameters<ControlPlaneClient['command']>[0],
    apply: (response: SettingsCommandResponses[Command]) => void,
  ): Promise<void> {
    if (closed) throw clientFailure('SETTINGS_VIEW_MODEL_CLOSED', '设置视图已关闭。')
    if (
      currentState.interaction.status === 'submitting'
      || currentState.interaction.status === 'waiting'
    ) {
      interactionFailure(
        'SETTINGS_DECISION_IN_FLIGHT',
        '请等待当前设置更改完成。',
        command,
      )
      return
    }
    if (!Number.isSafeInteger(expectedRevision) || expectedRevision < 0) {
      interactionFailure(
        'SETTINGS_REVISION_REQUIRED',
        '请刷新设置后再提交此更改。',
        command,
      )
      return
    }
    const ownGeneration = generation
    const active = controller()
    const requestId = options.nextRequestId()
    patch({ interaction: frozenInteraction('submitting', command) })
    try {
      const response = await options.client.command(request(requestId), { signal: active.signal })
      if (!isCurrent(ownGeneration)) return
      const completed = expectCompletedCommand(response, command, requestId)
      if (completed === null) {
        patch({ interaction: frozenInteraction('waiting', command) })
        return
      }
      if (completed.previousRevision !== expectedRevision) throw clientFailure(
        'SETTINGS_COMMAND_REVISION_MISMATCH',
        '控制平面返回了其他设置修订版的结果。',
      )
      apply(completed)
      patch({ interaction: frozenInteraction('idle'), error: null })
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      patch({ interaction: frozenInteraction('error', command, normalizedError(error, active.signal)) })
    } finally {
      releaseController(active)
    }
  }

  function mergeCredential(reference: CredentialReferenceProjection): void {
    patch({
      credentials: orderCredentials([
        ...currentState.credentials.filter(item => item.id !== reference.id),
        reference,
      ]),
    })
  }

  return {
    get state() { return currentState },
    draftScope,
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
      queryCache.refresh()
      await load(false, realtime === null ? 'inactive' : 'subscribed')
      if (currentState.status === 'ready' && realtime === null && !closed) subscribeRealtime()
    },
    async updateSettings(input) {
      const settings = currentState.settings
      if (settings === null) {
        interactionFailure(
          'SETTINGS_SNAPSHOT_REQUIRED',
          '请刷新设置后再保存模型路由。',
          CommandName.SettingsUpdate,
        )
        return
      }
      if (
        !Number.isSafeInteger(input.workerConcurrencyLimit)
        || input.workerConcurrencyLimit < 1
        || input.workerConcurrencyLimit > 10_000
      ) {
        interactionFailure(
          'SETTINGS_CONCURRENCY_INVALID',
          '执行并发数必须在 1 到 10000 之间。',
          CommandName.SettingsUpdate,
        )
        return
      }
      let route: ModelRoute | null = null
      if (input.defaultModelRoute !== null) {
        let providerId: string
        let modelId: string
        try {
          providerId = checkedText(
            input.defaultModelRoute.providerId,
            'SETTINGS_PROVIDER_REQUIRED',
            '请输入模型服务商 ID。',
          )
          modelId = checkedText(
            input.defaultModelRoute.modelId,
            'SETTINGS_MODEL_REQUIRED',
            '请输入模型 ID。',
          )
        } catch (error) {
          patch({
            interaction: frozenInteraction(
              'error',
              CommandName.SettingsUpdate,
              normalizedError(error),
            ),
          })
          return
        }
        const reference = currentState.credentials.find(
          item => item.id === input.defaultModelRoute?.credentialReferenceId,
        )
        if (
          reference === undefined
          || reference.providerId !== providerId
          || reference.secretState !== 'available'
        ) {
          interactionFailure(
            'SETTINGS_CREDENTIAL_ROUTE_INVALID',
            '请选择该模型服务商的可用凭据引用。',
            CommandName.SettingsUpdate,
          )
          return
        }
        route = {
          providerId,
          modelId,
          credentialReferenceId: reference.id,
        }
      }
      await runCommand(
        CommandName.SettingsUpdate,
        settings.revision,
        requestId => ({
          ...requestBase(),
          requestId,
          command: CommandName.SettingsUpdate,
          expectedRevision: settings.revision,
          payload: {
            patch: {
              defaultModelRoute: route,
              workerConcurrencyLimit: input.workerConcurrencyLimit,
            },
          },
        }),
        response => { patch({ settings: response.result }) },
      )
    },
    async createCredentialReference(input) {
      let displayName: string
      let providerId: string
      let vaultLocator: string
      try {
        displayName = checkedText(
          input.displayName,
          'CREDENTIAL_DISPLAY_NAME_REQUIRED',
          '请输入凭据显示名称。',
        )
        providerId = checkedText(
          input.providerId,
          'CREDENTIAL_PROVIDER_REQUIRED',
          '请输入凭据的模型服务商 ID。',
        )
        vaultLocator = checkedSecret(input.vaultLocator)
      } catch (error) {
        patch({
          interaction: frozenInteraction(
            'error',
            CommandName.CredentialReferenceCreate,
            normalizedError(error),
          ),
        })
        return
      }
      await runCommand(
        CommandName.CredentialReferenceCreate,
        0,
        requestId => ({
          ...requestBase(),
          requestId,
          command: CommandName.CredentialReferenceCreate,
          expectedRevision: 0,
          payload: {
            credentialReferenceId: input.credentialReferenceId,
            displayName,
            providerId,
            vaultLocator,
          },
        }),
        response => {
          if (response.result.id !== input.credentialReferenceId) throw clientFailure(
            'CREDENTIAL_CREATE_MISMATCH',
            '控制平面返回了其他凭据引用。',
          )
          mergeCredential(response.result)
        },
      )
    },
    async rotateCredentialReference(input) {
      const reference = currentState.credentials.find(item => item.id === input.credentialReferenceId)
      if (reference === undefined) {
        interactionFailure(
          'CREDENTIAL_REFERENCE_STALE',
          '请刷新设置并选择当前凭据引用。',
          CommandName.CredentialReferenceRotate,
        )
        return
      }
      let vaultLocator: string
      try {
        vaultLocator = checkedSecret(input.vaultLocator)
      } catch (error) {
        patch({
          interaction: frozenInteraction(
            'error',
            CommandName.CredentialReferenceRotate,
            normalizedError(error),
          ),
        })
        return
      }
      await runCommand(
        CommandName.CredentialReferenceRotate,
        reference.revision,
        requestId => ({
          ...requestBase(),
          requestId,
          command: CommandName.CredentialReferenceRotate,
          expectedRevision: reference.revision,
          payload: { credentialReferenceId: reference.id, vaultLocator },
        }),
        response => { mergeCredential(response.result) },
      )
    },
    async revokeCredentialReference(credentialReferenceId) {
      const reference = currentState.credentials.find(item => item.id === credentialReferenceId)
      if (reference === undefined) {
        interactionFailure(
          'CREDENTIAL_REFERENCE_STALE',
          '请刷新设置并选择当前凭据引用。',
          CommandName.CredentialReferenceRevoke,
        )
        return
      }
      await runCommand(
        CommandName.CredentialReferenceRevoke,
        reference.revision,
        requestId => ({
          ...requestBase(),
          requestId,
          command: CommandName.CredentialReferenceRevoke,
          expectedRevision: reference.revision,
          payload: { credentialReferenceId: reference.id },
        }),
        response => { mergeCredential(response.result) },
      )
    },
    cancelPending() {
      if (closed) return
      generation += 1
      abortRequests()
      realtime?.close()
      realtime = null
      const error = new ControlPlaneClientError({
        kind: 'cancelled',
        code: 'REQUEST_CANCELLED',
        message: '设置请求已取消。',
        requestId: null,
        retryable: false,
      })
      publish({
        ...currentState,
        status: 'cancelled',
        realtime: 'inactive',
        interaction: frozenInteraction('error', null, error),
        error,
      })
    },
    reconnect() {
      if (closed) throw clientFailure('SETTINGS_VIEW_MODEL_CLOSED', '设置视图已关闭。')
      if (realtime === null) throw clientFailure(
        'SETTINGS_SUBSCRIPTION_INACTIVE',
        '设置事件未启用。',
      )
      patch({ realtime: 'reconnecting', error: null })
      realtime.reconnect()
      void load(false, 'reloading')
    },
    close() {
      if (closed) return
      closed = true
      queryCache.close()
      generation += 1
      abortRequests()
      realtime?.close()
      realtime = null
      publish({
        status: 'closed',
        realtime: 'closed',
        settings: null,
        credentials: Object.freeze([]),
        interaction: frozenInteraction('idle'),
        error: null,
      })
      listeners.clear()
    },
  }
}
