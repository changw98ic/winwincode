// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneSubscription,
} from './control-plane-client.js'
import type {
  Actor,
  ApprovalDecideCompletedResponse,
  ApprovalProjection,
  ApprovalListResultResponse,
  ChatInputInteractionProjection,
  ChatInteractionListResultResponse,
  ChatMessageProjection,
  ChatSubmitCompletedResponse,
  CommandAcceptedResponse,
  CommandCompletedResponse,
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketSubscriptionId,
  EventReadCursor,
  InputRespondCompletedResponse,
  InteractiveInputValue,
  ModelRoute,
  OpaqueCursor,
  PageInfo,
  ProductSessionId,
  ProductSessionProjection,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
  RuntimeProjectionSnapshot,
  RuntimeProjectionGetResultResponse,
  SessionModelSelection,
  SessionCancelCompletedResponse,
  SessionCreateCompletedResponse,
  SessionGetResultResponse,
  SessionMessagesListResultResponse,
  SettingsGetResultResponse,
} from './generated/contracts.js'
import {
  CommandName,
  ControlPlaneWebSocketEventType,
  QueryName,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const DEFAULT_MESSAGE_PAGE_SIZE = 50
const DEFAULT_RUNTIME_PAGE_SIZE = 50
const DEFAULT_APPROVAL_PAGE_SIZE = 50
const DEFAULT_INTERACTION_PAGE_SIZE = 50

export type ChatViewStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'refreshing'
  | 'cancelled'
  | 'authentication-required'
  | 'authorization-denied'
  | 'error'
  | 'closed'

export type ChatRealtimeStatus =
  | 'inactive'
  | 'subscribed'
  | 'reloading'
  | 'reconnecting'
  | 'access-revoked'
  | 'closed'

export type ChatPaginationStatus = 'idle' | 'loading' | 'error'

export type ChatInteractionStatus =
  | 'idle'
  | 'submitting'
  | 'cancelling'
  | 'waiting'
  | 'error'

export interface ChatInteractionState {
  readonly status: ChatInteractionStatus
  readonly error: ControlPlaneClientError | null
}

export interface ChatMessagePagination {
  readonly status: ChatPaginationStatus
  readonly hasMore: boolean
  readonly nextCursor: OpaqueCursor | null
  readonly error: ControlPlaneClientError | null
}

export interface ChatViewModelState {
  readonly status: ChatViewStatus
  readonly realtime: ChatRealtimeStatus
  readonly activeProductSessionId: ProductSessionId
  readonly sessions: readonly ProductSessionProjection[]
  readonly session: ProductSessionProjection | null
  readonly messages: readonly ChatMessageProjection[]
  readonly messagePagination: ChatMessagePagination
  readonly defaultModelRoute: ModelRoute | null
  readonly runtime: RuntimeProjectionSnapshot | null
  /** Exact Control Plane binding used by input.respond. */
  readonly pendingInputs: readonly ChatInputInteractionProjection[]
  /** Exact Control Plane binding used by approval.decide. */
  readonly pendingApprovals: readonly ApprovalProjection[]
  readonly interaction: ChatInteractionState
  readonly error: ControlPlaneClientError | null
}

export type ChatViewModelListener = (state: ChatViewModelState) => void

export interface ChatViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly productSessionId: ProductSessionId
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  readonly messagePageSize?: number
  readonly runtimePageSize?: number
  readonly approvalPageSize?: number
  readonly interactionPageSize?: number
  /** Deterministic clock used to reject expired interaction bindings. */
  readonly nowMillis?: () => number
}

export interface ChatCreateSessionInput {
  readonly productSessionId: ProductSessionId
  readonly title: string
  readonly modelSelection: SessionModelSelection
}

export interface ChatViewModel {
  readonly state: ChatViewModelState
  subscribe(listener: ChatViewModelListener): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  selectSession(productSessionId: ProductSessionId): Promise<void>
  createSession(input: ChatCreateSessionInput): Promise<void>
  submitMessage(message: string): Promise<void>
  cancelSession(reason: string): Promise<void>
  respondToInput(
    inputRequestId: string,
    status: 'provided' | 'cancelled',
    value: InteractiveInputValue | null,
  ): Promise<void>
  decideApproval(
    approvalId: string,
    decision: 'approve' | 'reject',
    reason: string,
  ): Promise<void>
  loadMoreMessages(): Promise<void>
  cancelPending(): void
  reconnect(): void
  close(): void
}

interface ChatSnapshot {
  readonly sessions: readonly ProductSessionProjection[]
  readonly session: ProductSessionProjection
  readonly messages: readonly ChatMessageProjection[]
  readonly messagePage: PageInfo
  readonly defaultModelRoute: ModelRoute | null
  readonly runtime: RuntimeProjectionSnapshot
  readonly pendingInputs: readonly ChatInputInteractionProjection[]
  readonly pendingApprovals: readonly ApprovalProjection[]
}

function frozenPagination(
  status: ChatPaginationStatus,
  page: PageInfo,
  error: ControlPlaneClientError | null = null,
): ChatMessagePagination {
  return Object.freeze({
    status,
    hasMore: page.hasMore,
    nextCursor: page.nextCursor,
    error,
  })
}

const EMPTY_PAGE = Object.freeze({ hasMore: false, nextCursor: null })

function initialState(): ChatViewModelState {
  return Object.freeze({
    status: 'idle',
    realtime: 'inactive',
    activeProductSessionId: '' as ProductSessionId,
    sessions: Object.freeze([]),
    session: null,
    messages: Object.freeze([]),
    messagePagination: frozenPagination('idle', EMPTY_PAGE),
    defaultModelRoute: null,
    runtime: null,
    pendingInputs: Object.freeze([]),
    pendingApprovals: Object.freeze([]),
    interaction: Object.freeze({ status: 'idle', error: null }),
    error: null,
  })
}

function frozenInteraction(
  status: ChatInteractionStatus,
  error: ControlPlaneClientError | null = null,
): ChatInteractionState {
  return Object.freeze({ status, error })
}

function requestPage(cursor: OpaqueCursor | null, limit: number) {
  return Object.freeze({ cursor, limit })
}

function assertPageSize(name: string, value: number | undefined, fallback: number): number {
  const result = value ?? fallback
  if (!Number.isInteger(result) || result < 1 || result > 200) {
    throw new RangeError(`${name} must be an integer between 1 and 200.`)
  }
  return result
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
  if (signal?.aborted === true) {
    return new ControlPlaneClientError({
      kind: 'cancelled',
      code: 'REQUEST_CANCELLED',
      message: 'The Chat view request was cancelled.',
      requestId: null,
      retryable: false,
      cause: error,
    })
  }
  return clientFailure(
    'CHAT_VIEW_MODEL_FAILURE',
    'The Chat projection could not be updated.',
    error,
  )
}

function statusForError(error: ControlPlaneClientError): ChatViewStatus {
  if (error.kind === 'authentication') return 'authentication-required'
  if (error.kind === 'authorization') return 'authorization-denied'
  if (error.kind === 'cancelled') return 'cancelled'
  return 'error'
}

interface ChatQueryResponses {
  readonly [QueryName.SessionList]: import(
    './generated/contracts.js'
  ).SessionListResultResponse
  readonly [QueryName.SessionGet]: SessionGetResultResponse
  readonly [QueryName.SessionMessagesList]: SessionMessagesListResultResponse
  readonly [QueryName.SettingsGet]: SettingsGetResultResponse
  readonly [QueryName.RuntimeProjectionGet]: RuntimeProjectionGetResultResponse
  readonly [QueryName.SessionInteractionsList]: ChatInteractionListResultResponse
  readonly [QueryName.ApprovalList]: ApprovalListResultResponse
}

interface ChatCommandResponses {
  readonly [CommandName.SessionCreate]: SessionCreateCompletedResponse
  readonly [CommandName.ChatSubmit]: ChatSubmitCompletedResponse
  readonly [CommandName.SessionCancel]: SessionCancelCompletedResponse
  readonly [CommandName.InputRespond]: InputRespondCompletedResponse
  readonly [CommandName.ApprovalDecide]: ApprovalDecideCompletedResponse
}

function expectResponse<Query extends keyof ChatQueryResponses>(
  response: QueryResultResponse,
  query: Query,
): ChatQueryResponses[Query] {
  if (response.query !== query) {
    throw clientFailure(
      'CHAT_QUERY_MISMATCH',
      `Expected ${query}, received ${response.query}.`,
    )
  }
  return response as ChatQueryResponses[Query]
}

function expectCompletedCommand<Command extends keyof ChatCommandResponses>(
  response: CommandAcceptedResponse | CommandCompletedResponse,
  command: Command,
): ChatCommandResponses[Command] | null {
  if (response.command !== command) throw clientFailure(
    'CHAT_COMMAND_MISMATCH',
    'The Control Plane returned another command result.',
  )
  if (response.outcome === 'accepted') return null
  return response as ChatCommandResponses[Command]
}

function assertPage(page: PageInfo, query: string): void {
  if (page.hasMore !== (page.nextCursor !== null)) {
    throw clientFailure(
      'CHAT_PAGE_INVALID',
      `${query} returned an inconsistent pagination cursor.`,
    )
  }
}

function sameScope(left: RepositoryScope, right: RepositoryScope): boolean {
  return left.kind === right.kind
    && left.organizationId === right.organizationId
    && left.workspaceId === right.workspaceId
    && left.projectId === right.projectId
    && left.repositoryId === right.repositoryId
}

function assertMessages(
  messages: readonly ChatMessageProjection[],
  productSessionId: ProductSessionId,
): readonly ChatMessageProjection[] {
  const ids = new Set<string>()
  const sequences = new Set<number>()
  for (const message of messages) {
    if (message.productSessionId !== productSessionId) {
      throw clientFailure(
        'CHAT_MESSAGE_SESSION_MISMATCH',
        'A Chat message belongs to another ProductSession.',
      )
    }
    if (ids.has(message.id) || sequences.has(message.sequence)) {
      throw clientFailure(
        'CHAT_MESSAGE_ORDER_INVALID',
        'The Chat message page contains duplicate identity or sequence values.',
      )
    }
    ids.add(message.id)
    sequences.add(message.sequence)
  }
  return Object.freeze([...messages].sort((left, right) => (
    left.sequence - right.sequence || left.id.localeCompare(right.id)
  )))
}

function mergeMessages(
  current: readonly ChatMessageProjection[],
  incoming: readonly ChatMessageProjection[],
  productSessionId: ProductSessionId,
): readonly ChatMessageProjection[] {
  const byId = new Map(current.map(message => [message.id, message]))
  for (const message of incoming) byId.set(message.id, message)
  return assertMessages([...byId.values()], productSessionId)
}

function orderSessions(
  sessions: readonly ProductSessionProjection[],
): readonly ProductSessionProjection[] {
  const byId = new Map(sessions.map(session => [session.id, session]))
  return Object.freeze([...byId.values()].sort((left, right) => (
    right.updatedAt.localeCompare(left.updatedAt) || left.id.localeCompare(right.id)
  )))
}

function assertRuntime(
  runtime: RuntimeProjectionSnapshot,
  scope: RepositoryScope,
  productSessionId: ProductSessionId,
): void {
  if (
    runtime.productSessionId !== productSessionId
    || runtime.deliveryId !== null
    || runtime.stageRunId !== null
    || runtime.readCursor !== null
    || runtime.eventCursor.stream.kind !== 'product-session'
    || runtime.eventCursor.stream.productSessionId !== productSessionId
    || !sameScope(runtime.eventCursor.scope, scope)
    || runtime.sessions.some(session => session.productSessionId !== productSessionId)
  ) {
    throw clientFailure(
      'CHAT_RUNTIME_SESSION_MISMATCH',
      'The runtime projection does not match the active Chat ProductSession.',
    )
  }
}

function assertBinding(
  binding: import('./generated/contracts.js').ChatInteractionBindingProjection,
  productSessionId: ProductSessionId,
): void {
  if (
    binding.productSessionId !== productSessionId
    || binding.sessionIdentity.productSessionId !== binding.productSessionId
    || binding.workerSessionId !== binding.sessionIdentity.workerSessionId
  ) throw clientFailure(
    'CHAT_INTERACTION_BINDING_MISMATCH',
    'A pending interaction does not match the active ProductSession binding.',
  )
}

function assertPendingInputs(
  interactions: ChatInteractionListResultResponse['result']['items'],
  productSessionId: ProductSessionId,
  nowMillis: number,
): readonly ChatInputInteractionProjection[] {
  const ids = new Set<string>()
  const result: ChatInputInteractionProjection[] = []
  for (const interaction of interactions) {
    if (interaction.kind !== 'input') continue
    assertBinding(interaction.binding, productSessionId)
    if (
      interaction.state !== 'pending'
      || ids.has(interaction.inputRequestId)
      || !Number.isFinite(Date.parse(interaction.expiresAt))
      || Date.parse(interaction.expiresAt) <= nowMillis
    ) {
      throw clientFailure(
        'CHAT_INPUT_PROJECTION_INVALID',
        'The pending input projection contains resolved or duplicate input.',
      )
    }
    ids.add(interaction.inputRequestId)
    result.push(interaction)
  }
  return Object.freeze(result)
}

function assertPendingApprovals(
  approvals: readonly ApprovalProjection[],
  productSessionId: ProductSessionId,
  nowMillis: number,
): readonly ApprovalProjection[] {
  const ids = new Set<string>()
  const result: ApprovalProjection[] = []
  for (const approval of approvals) {
    if (
      approval.binding.productSessionId
      !== approval.binding.sessionIdentity.productSessionId
      || approval.binding.workerSessionId
      !== approval.binding.sessionIdentity.workerSessionId
    ) throw clientFailure(
      'CHAT_INTERACTION_BINDING_MISMATCH',
      'A pending approval contains an inconsistent session binding.',
    )
    if (approval.binding.productSessionId !== productSessionId) continue
    if (
      !Number.isFinite(Date.parse(approval.expiresAt))
      || Date.parse(approval.expiresAt) <= nowMillis
    ) continue
    if (approval.state !== 'pending' || ids.has(approval.id)) {
      throw clientFailure(
        'CHAT_APPROVAL_PROJECTION_INVALID',
        'The pending approval projection contains resolved or duplicate approval.',
      )
    }
    ids.add(approval.id)
    result.push(approval)
  }
  return Object.freeze(result)
}

/** Build the complete Chat read model from HTTP snapshots and one WebSocket subscription. */
export function createChatViewModel(options: ChatViewModelOptions): ChatViewModel {
  const messagePageSize = assertPageSize(
    'messagePageSize',
    options.messagePageSize,
    DEFAULT_MESSAGE_PAGE_SIZE,
  )
  const runtimePageSize = assertPageSize(
    'runtimePageSize',
    options.runtimePageSize,
    DEFAULT_RUNTIME_PAGE_SIZE,
  )
  const approvalPageSize = assertPageSize(
    'approvalPageSize',
    options.approvalPageSize,
    DEFAULT_APPROVAL_PAGE_SIZE,
  )
  const interactionPageSize = assertPageSize(
    'interactionPageSize',
    options.interactionPageSize,
    DEFAULT_INTERACTION_PAGE_SIZE,
  )
  const listeners = new Set<ChatViewModelListener>()
  const controllers = new Set<AbortController>()
  let currentState = initialState()
  let realtime: ControlPlaneSubscription | null = null
  let generation = 0
  let closed = false
  let activeProductSessionId = options.productSessionId
  const nowMillis = options.nowMillis ?? Date.now

  currentState = Object.freeze({
    ...currentState,
    activeProductSessionId,
  })

  function publish(update: ChatViewModelState): void {
    currentState = Object.freeze(update)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<ChatViewModelState>): void {
    publish({ ...currentState, ...update })
  }

  function controller(): AbortController {
    const next = new AbortController()
    controllers.add(next)
    return next
  }

  function releaseController(value: AbortController): void {
    controllers.delete(value)
  }

  function abortRequests(): void {
    for (const active of controllers) active.abort()
    controllers.clear()
  }

  function isCurrent(ownGeneration: number): boolean {
    return !closed && generation === ownGeneration
  }

  function requestBase() {
    return {
      schemaVersion: SCHEMA_VERSION,
      actor: options.actor,
      scope: options.scope,
    }
  }

  async function querySnapshot(signal: AbortSignal): Promise<ChatSnapshot> {
    const [
      sessionsValue,
      sessionValue,
      messagesValue,
      settingsValue,
      runtimeValue,
      interactionsValue,
      approvalsValue,
    ] = (
      await Promise.all([
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionList,
          parameters: { states: [] },
          page: requestPage(null, 50),
        }, { signal }),
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionGet,
          parameters: { productSessionId: activeProductSessionId },
          page: requestPage(null, 1),
        }, { signal }),
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionMessagesList,
          parameters: { productSessionId: activeProductSessionId },
          page: requestPage(null, messagePageSize),
        }, { signal }),
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SettingsGet,
          parameters: {},
          page: requestPage(null, 1),
        }, { signal }),
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.RuntimeProjectionGet,
          parameters: {
            kind: 'product-session',
            productSessionId: activeProductSessionId,
          },
          page: requestPage(null, runtimePageSize),
        }, { signal }),
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionInteractionsList,
          parameters: {
            productSessionId: activeProductSessionId,
            states: ['pending'],
          },
          page: requestPage(null, interactionPageSize),
        }, { signal }),
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.ApprovalList,
          parameters: { states: ['pending'] },
          page: requestPage(null, approvalPageSize),
        }, { signal }),
      ])
    )
    const sessions = expectResponse(sessionsValue, QueryName.SessionList)
    const session = expectResponse(sessionValue, QueryName.SessionGet)
    const messages = expectResponse(messagesValue, QueryName.SessionMessagesList)
    const settings = expectResponse(settingsValue, QueryName.SettingsGet)
    const runtime = expectResponse(runtimeValue, QueryName.RuntimeProjectionGet)
    const interactions = expectResponse(
      interactionsValue,
      QueryName.SessionInteractionsList,
    )
    const approvals = expectResponse(approvalsValue, QueryName.ApprovalList)
    for (const response of [
      sessions,
      session,
      messages,
      settings,
      runtime,
      interactions,
      approvals,
    ]) {
      assertPage(response.page, response.query)
    }
    if (
      session.result.id !== activeProductSessionId
      || session.result.projectId !== options.scope.projectId
      || session.result.repositoryId !== options.scope.repositoryId
    ) {
      throw clientFailure(
        'CHAT_SESSION_SCOPE_MISMATCH',
        'The ProductSession snapshot does not match the active repository.',
      )
    }
    for (const item of sessions.result.items) {
      if (
        item.projectId !== options.scope.projectId
        || item.repositoryId !== options.scope.repositoryId
      ) throw clientFailure(
        'CHAT_SESSION_LIST_SCOPE_MISMATCH',
        'The session list contains a ProductSession from another repository.',
      )
    }
    const sessionById = new Map(sessions.result.items.map(item => [item.id, item]))
    sessionById.set(session.result.id, session.result)
    const orderedSessions = orderSessions([...sessionById.values()])
    assertRuntime(runtime.result, options.scope, activeProductSessionId)
    return Object.freeze({
      sessions: orderedSessions,
      session: session.result,
      messages: assertMessages(messages.result.items, activeProductSessionId),
      messagePage: messages.page,
      defaultModelRoute: settings.result.defaultModelRoute,
      runtime: runtime.result,
      pendingInputs: assertPendingInputs(
        interactions.result.items,
        activeProductSessionId,
        nowMillis(),
      ),
      pendingApprovals: assertPendingApprovals(
        approvals.result.items,
        activeProductSessionId,
        nowMillis(),
      ),
    })
  }

  function publishSnapshot(
    snapshot: ChatSnapshot,
    realtimeStatus: ChatRealtimeStatus,
  ): void {
    publish({
      status: 'ready',
      realtime: realtimeStatus,
      activeProductSessionId,
      sessions: snapshot.sessions,
      session: snapshot.session,
      messages: snapshot.messages,
      messagePagination: frozenPagination('idle', snapshot.messagePage),
      defaultModelRoute: snapshot.defaultModelRoute,
      runtime: snapshot.runtime,
      pendingInputs: snapshot.pendingInputs,
      pendingApprovals: snapshot.pendingApprovals,
      interaction: frozenInteraction('idle'),
      error: null,
    })
  }

  function clearForReset(): void {
    publish({
      status: 'refreshing',
      realtime: 'reloading',
      activeProductSessionId,
      sessions: Object.freeze([]),
      session: null,
      messages: Object.freeze([]),
      messagePagination: frozenPagination('idle', EMPTY_PAGE),
      defaultModelRoute: null,
      runtime: null,
      pendingInputs: Object.freeze([]),
      pendingApprovals: Object.freeze([]),
      interaction: frozenInteraction('idle'),
      error: null,
    })
  }

  async function completeSnapshot(
    ownGeneration: number,
    clear: boolean,
  ): Promise<EventReadCursor | null> {
    const active = controller()
    if (clear) clearForReset()
    try {
      const snapshot = await querySnapshot(active.signal)
      if (!isCurrent(ownGeneration)) return null
      publishSnapshot(snapshot, realtime === null ? 'inactive' : 'subscribed')
      return snapshot.runtime.eventCursor
    } catch (error) {
      if (!isCurrent(ownGeneration)) return null
      const normalized = normalizedError(error, active.signal)
      patch({
        status: statusForError(normalized),
        realtime: normalized.kind === 'authentication' || normalized.kind === 'authorization'
          ? 'access-revoked'
          : 'reconnecting',
        error: normalized,
      })
      throw normalized
    } finally {
      releaseController(active)
    }
  }

  async function reloadSession(
    event: ControlPlaneWebSocketEventFrame,
    ownGeneration: number,
  ): Promise<void> {
    if (event.event.type !== 'product-session.changed.v1') throw clientFailure(
      'CHAT_SESSION_EVENT_INVALID',
      'The ProductSession reload requires a ProductSession change event.',
    )
    const active = controller()
    try {
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.SessionGet,
        parameters: { productSessionId: activeProductSessionId },
        page: requestPage(null, 1),
      }, { signal: active.signal }), QueryName.SessionGet)
      if (!isCurrent(ownGeneration)) return
      assertPage(response.page, response.query)
      if (
        response.result.id !== activeProductSessionId
        || response.result.projectId !== options.scope.projectId
        || response.result.repositoryId !== options.scope.repositoryId
        || response.result.revision < event.event.revision
      ) throw clientFailure(
        'CHAT_SESSION_EVENT_STALE',
        'The ProductSession snapshot is older than its change event.',
      )
      patch({
        status: 'ready',
        realtime: 'subscribed',
        session: response.result,
        sessions: orderSessions([
          ...currentState.sessions.filter(item => item.id !== response.result.id),
          response.result,
        ]),
        interaction: frozenInteraction('idle'),
        error: null,
      })
    } finally {
      releaseController(active)
    }
  }

  async function reloadRuntime(
    event: ControlPlaneWebSocketEventFrame,
    ownGeneration: number,
  ): Promise<void> {
    const active = controller()
    try {
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.RuntimeProjectionGet,
        parameters: {
          kind: 'product-session',
          productSessionId: activeProductSessionId,
        },
        page: requestPage(null, runtimePageSize),
      }, { signal: active.signal }), QueryName.RuntimeProjectionGet)
      if (!isCurrent(ownGeneration)) return
      assertPage(response.page, response.query)
      assertRuntime(response.result, options.scope, activeProductSessionId)
      if (
        event.event.type === 'runtime-projection.invalidated.v1'
        && response.result.revision < event.event.projectionRevision
      ) throw clientFailure(
        'CHAT_RUNTIME_EVENT_STALE',
        'The runtime snapshot is older than its invalidation event.',
      )
      patch({ status: 'ready', realtime: 'subscribed', runtime: response.result, error: null })
    } finally {
      releaseController(active)
    }
  }

  async function reloadApprovals(ownGeneration: number): Promise<void> {
    const active = controller()
    try {
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.ApprovalList,
        parameters: { states: ['pending'] },
        page: requestPage(null, approvalPageSize),
      }, { signal: active.signal }), QueryName.ApprovalList)
      if (!isCurrent(ownGeneration)) return
      assertPage(response.page, response.query)
      patch({
        status: 'ready',
        realtime: 'subscribed',
        pendingApprovals: assertPendingApprovals(
          response.result.items,
          activeProductSessionId,
          nowMillis(),
        ),
        error: null,
      })
    } finally {
      releaseController(active)
    }
  }

  async function reloadInteractions(ownGeneration: number): Promise<void> {
    const active = controller()
    try {
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.SessionInteractionsList,
        parameters: {
          productSessionId: activeProductSessionId,
          states: ['pending'],
        },
        page: requestPage(null, interactionPageSize),
      }, { signal: active.signal }), QueryName.SessionInteractionsList)
      if (!isCurrent(ownGeneration)) return
      assertPage(response.page, response.query)
      patch({
        status: 'ready',
        realtime: 'subscribed',
        pendingInputs: assertPendingInputs(
          response.result.items,
          activeProductSessionId,
          nowMillis(),
        ),
        error: null,
      })
    } finally {
      releaseController(active)
    }
  }

  async function applyEvent(frame: ControlPlaneWebSocketEventFrame): Promise<void> {
    const ownGeneration = generation
    if (!isCurrent(ownGeneration)) return
    patch({ realtime: 'reloading' })
    try {
      const event = frame.event
      if ('productSessionId' in event && event.productSessionId !== activeProductSessionId) {
        throw clientFailure(
          'CHAT_EVENT_SESSION_MISMATCH',
          'A Chat event belongs to another ProductSession.',
        )
      }
      if (event.type === 'product-session.changed.v1') {
        await reloadSession(frame, ownGeneration)
      } else if (event.type === 'product-session.message.appended.v1') {
        const incoming = assertMessages([event.message], activeProductSessionId)
        if (isCurrent(ownGeneration)) patch({
          status: 'ready',
          realtime: 'subscribed',
          messages: mergeMessages(
            currentState.messages,
            incoming,
            activeProductSessionId,
          ),
          interaction: frozenInteraction('idle'),
          error: null,
        })
      } else if (event.type === 'runtime-projection.invalidated.v1') {
        if (event.scopeKind !== 'product-session') throw clientFailure(
          'CHAT_RUNTIME_EVENT_SCOPE_MISMATCH',
          'A Delivery runtime event reached the Chat subscription.',
        )
        await reloadRuntime(frame, ownGeneration)
      } else if (event.type === 'approval.changed.v1') {
        await reloadApprovals(ownGeneration)
      } else if (event.type === 'chat-interactions.invalidated.v1') {
        await Promise.all([
          reloadInteractions(ownGeneration),
          reloadApprovals(ownGeneration),
        ])
      } else if (isCurrent(ownGeneration)) {
        patch({ realtime: 'subscribed' })
      }
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      const normalized = normalizedError(error)
      patch({ realtime: 'reconnecting', error: normalized })
      throw normalized
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
      activeProductSessionId,
      sessions: Object.freeze([]),
      session: null,
      messages: Object.freeze([]),
      messagePagination: frozenPagination('idle', EMPTY_PAGE),
      defaultModelRoute: null,
      runtime: null,
      pendingInputs: Object.freeze([]),
      pendingApprovals: Object.freeze([]),
      interaction: frozenInteraction('error', error),
      error,
    })
  }

  function subscribeRealtime(cursor: EventReadCursor): void {
    realtime?.close()
    realtime = options.client.subscribe({
      subscriptionId: options.subscriptionId,
      subscription: {
        scope: options.scope,
        stream: { kind: 'product-session', productSessionId: activeProductSessionId },
        eventTypes: [
          ControlPlaneWebSocketEventType.ProductSessionChangedV1,
          ControlPlaneWebSocketEventType.ProductSessionMessageAppendedV1,
          ControlPlaneWebSocketEventType.RuntimeProjectionInvalidatedV1,
          ControlPlaneWebSocketEventType.ApprovalChangedV1,
          ControlPlaneWebSocketEventType.ChatInteractionsInvalidatedV1,
        ],
      },
      startAt: cursor,
      onEvent: applyEvent,
      async onResetRequired() {
        const ownGeneration = generation
        const next = await completeSnapshot(ownGeneration, true)
        if (next === null) throw clientFailure(
          'CHAT_RESET_SUPERSEDED',
          'The Chat reset was replaced by a newer operation.',
        )
        return next
      },
      onAuthorizationRevoked() {
        accessRevoked(new ControlPlaneClientError({
          kind: 'authentication',
          code: 'AUTHENTICATION_REQUIRED',
          message: 'The Chat subscription authorization is no longer valid.',
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

  function interactionFailure(code: string, message: string): void {
    patch({ interaction: frozenInteraction('error', clientFailure(code, message)) })
  }

  function applySessionMutation(session: ProductSessionProjection): void {
    if (
      session.id !== activeProductSessionId
      || session.projectId !== options.scope.projectId
      || session.repositoryId !== options.scope.repositoryId
    ) throw clientFailure(
      'CHAT_MUTATION_SESSION_MISMATCH',
      'The Chat command returned another ProductSession.',
    )
    patch({
      activeProductSessionId,
      session,
      sessions: orderSessions([
        ...currentState.sessions.filter(item => item.id !== session.id),
        session,
      ]),
      interaction: frozenInteraction('idle'),
    })
  }

  async function reloadFirstMessagePage(ownGeneration: number): Promise<void> {
    const active = controller()
    try {
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.SessionMessagesList,
        parameters: { productSessionId: activeProductSessionId },
        page: requestPage(null, messagePageSize),
      }, { signal: active.signal }), QueryName.SessionMessagesList)
      if (!isCurrent(ownGeneration)) return
      assertPage(response.page, response.query)
      patch({
        messages: assertMessages(response.result.items, activeProductSessionId),
        messagePagination: frozenPagination('idle', response.page),
      })
    } finally {
      releaseController(active)
    }
  }

  async function submitMessage(message: string): Promise<void> {
    const value = message.trim()
    if (value.length === 0) {
      interactionFailure('CHAT_MESSAGE_REQUIRED', 'Enter a message before sending.')
      return
    }
    const session = currentState.session
    if (session === null) {
      interactionFailure('CHAT_SESSION_REQUIRED', 'Select a Chat session before sending.')
      return
    }
    const ownGeneration = generation
    const active = controller()
    patch({ interaction: frozenInteraction('submitting') })
    try {
      const response = await options.client.command({
        ...requestBase(),
        requestId: options.nextRequestId(),
        command: CommandName.ChatSubmit,
        expectedRevision: session.revision,
        payload: { productSessionId: activeProductSessionId, message: value },
      }, { signal: active.signal })
      if (!isCurrent(ownGeneration)) return
      const completed = expectCompletedCommand(response, CommandName.ChatSubmit)
      if (completed === null) {
        patch({ interaction: frozenInteraction('waiting') })
        return
      }
      applySessionMutation(completed.result)
      await reloadFirstMessagePage(ownGeneration)
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      patch({ interaction: frozenInteraction('error', normalizedError(error, active.signal)) })
    } finally {
      releaseController(active)
    }
  }

  async function createSession(input: ChatCreateSessionInput): Promise<void> {
    const title = input.title.trim()
    if (title.length === 0) {
      interactionFailure('CHAT_SESSION_TITLE_REQUIRED', 'Enter a title for the new Chat.')
      return
    }
    const ownGeneration = generation
    const active = controller()
    patch({ interaction: frozenInteraction('submitting') })
    try {
      const response = await options.client.command({
        ...requestBase(),
        requestId: options.nextRequestId(),
        command: CommandName.SessionCreate,
        expectedRevision: 0,
        payload: {
          productSessionId: input.productSessionId,
          projectId: options.scope.projectId,
          repositoryId: options.scope.repositoryId,
          title,
          modelSelection: input.modelSelection,
        },
      }, { signal: active.signal })
      if (!isCurrent(ownGeneration)) return
      const completed = expectCompletedCommand(response, CommandName.SessionCreate)
      if (completed === null) {
        patch({ interaction: frozenInteraction('waiting') })
        return
      }
      if (completed.result.id !== input.productSessionId) throw clientFailure(
        'CHAT_CREATE_SESSION_MISMATCH',
        'The new Chat response returned another ProductSession.',
      )
      activeProductSessionId = input.productSessionId
      applySessionMutation(completed.result)
      await load(true)
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      patch({ interaction: frozenInteraction('error', normalizedError(error, active.signal)) })
    } finally {
      releaseController(active)
    }
  }

  async function cancelSession(reason: string): Promise<void> {
    const value = reason.trim()
    if (value.length === 0) {
      interactionFailure('CHAT_CANCEL_REASON_REQUIRED', 'Explain why the run is being stopped.')
      return
    }
    const session = currentState.session
    if (session === null) {
      interactionFailure('CHAT_SESSION_REQUIRED', 'Select a Chat session before stopping it.')
      return
    }
    const ownGeneration = generation
    const active = controller()
    patch({ interaction: frozenInteraction('cancelling') })
    try {
      const response = await options.client.command({
        ...requestBase(),
        requestId: options.nextRequestId(),
        command: CommandName.SessionCancel,
        expectedRevision: session.revision,
        payload: { productSessionId: activeProductSessionId, reason: value },
      }, { signal: active.signal })
      if (!isCurrent(ownGeneration)) return
      const completed = expectCompletedCommand(response, CommandName.SessionCancel)
      if (completed === null) {
        patch({ interaction: frozenInteraction('waiting') })
        return
      }
      applySessionMutation(completed.result)
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      patch({ interaction: frozenInteraction('error', normalizedError(error, active.signal)) })
    } finally {
      releaseController(active)
    }
  }

  async function respondToInput(
    inputRequestId: string,
    status: 'provided' | 'cancelled',
    value: InteractiveInputValue | null,
  ): Promise<void> {
    const input = currentState.pendingInputs.find(item => item.inputRequestId === inputRequestId)
    if (input === undefined) {
      interactionFailure('CHAT_INPUT_REQUIRED', 'Select one current pending input request.')
      return
    }
    if (Date.parse(input.expiresAt) <= nowMillis()) {
      interactionFailure('CHAT_INPUT_EXPIRED', 'The pending input request has expired.')
      return
    }
    assertBinding(input.binding, activeProductSessionId)
    const ownGeneration = generation
    const active = controller()
    patch({ interaction: frozenInteraction('submitting') })
    try {
      const response = await options.client.command({
        ...requestBase(),
        requestId: options.nextRequestId(),
        command: CommandName.InputRespond,
        expectedRevision: input.revision,
        payload: {
          executionJobId: input.binding.executionJobId,
          inputRequestId: input.inputRequestId,
          productSessionId: input.binding.productSessionId,
          sessionIdentity: input.binding.sessionIdentity,
          status,
          value,
          workerSessionId: input.binding.workerSessionId,
        },
      }, { signal: active.signal })
      if (!isCurrent(ownGeneration)) return
      const completed = expectCompletedCommand(response, CommandName.InputRespond)
      if (completed === null) {
        patch({ interaction: frozenInteraction('waiting') })
        return
      }
      applySessionMutation(completed.result)
      await reloadInteractions(ownGeneration)
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      patch({ interaction: frozenInteraction('error', normalizedError(error, active.signal)) })
    } finally {
      releaseController(active)
    }
  }

  async function decideApproval(
    approvalId: string,
    decision: 'approve' | 'reject',
    reason: string,
  ): Promise<void> {
    const approval = currentState.pendingApprovals.find(item => item.id === approvalId)
    if (approval === undefined) {
      interactionFailure('CHAT_APPROVAL_REQUIRED', 'Select one current pending approval.')
      return
    }
    if (Date.parse(approval.expiresAt) <= nowMillis()) {
      interactionFailure('CHAT_APPROVAL_EXPIRED', 'The pending approval has expired.')
      return
    }
    assertBinding(approval.binding, activeProductSessionId)
    const ownGeneration = generation
    const active = controller()
    patch({ interaction: frozenInteraction('submitting') })
    try {
      const response = await options.client.command({
        ...requestBase(),
        requestId: options.nextRequestId(),
        command: CommandName.ApprovalDecide,
        expectedRevision: approval.revision,
        payload: {
          approvalId: approval.id,
          binding: approval.binding,
          decision,
          reason: reason.trim(),
        },
      }, { signal: active.signal })
      if (!isCurrent(ownGeneration)) return
      const completed = expectCompletedCommand(response, CommandName.ApprovalDecide)
      if (completed === null) {
        patch({ interaction: frozenInteraction('waiting') })
        return
      }
      await Promise.all([
        reloadInteractions(ownGeneration),
        reloadApprovals(ownGeneration),
      ])
      if (isCurrent(ownGeneration)) patch({ interaction: frozenInteraction('idle') })
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      patch({ interaction: frozenInteraction('error', normalizedError(error, active.signal)) })
    } finally {
      releaseController(active)
    }
  }

  async function load(replace: boolean): Promise<void> {
    if (closed) throw clientFailure('CHAT_VIEW_MODEL_CLOSED', 'The Chat view-model is closed.')
    generation += 1
    const ownGeneration = generation
    abortRequests()
    realtime?.close()
    realtime = null
    patch({
      status: replace ? 'loading' : 'refreshing',
      realtime: 'inactive',
      ...(replace
        ? {
            activeProductSessionId,
            sessions: Object.freeze([]),
            session: null,
            messages: Object.freeze([]),
            messagePagination: frozenPagination('idle', EMPTY_PAGE),
            defaultModelRoute: null,
            runtime: null,
            pendingInputs: Object.freeze([]),
            pendingApprovals: Object.freeze([]),
            interaction: frozenInteraction('idle'),
          }
        : {}),
      error: null,
    })
    try {
      const cursor = await completeSnapshot(ownGeneration, false)
      if (cursor === null || !isCurrent(ownGeneration)) return
      subscribeRealtime(cursor)
    } catch {
      // completeSnapshot has already published one bounded error.
    }
  }

  return {
    get state() {
      return currentState
    },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async start() {
      await load(true)
    },
    async refresh() {
      await load(false)
    },
    async selectSession(productSessionId) {
      if (closed) throw clientFailure('CHAT_VIEW_MODEL_CLOSED', 'The Chat view-model is closed.')
      if (productSessionId === activeProductSessionId && currentState.status === 'ready') return
      activeProductSessionId = productSessionId
      await load(true)
    },
    async createSession(input) {
      await createSession(input)
    },
    async submitMessage(message) {
      await submitMessage(message)
    },
    async cancelSession(reason) {
      await cancelSession(reason)
    },
    async respondToInput(inputRequestId, status, value) {
      await respondToInput(inputRequestId, status, value)
    },
    async decideApproval(approvalId, decision, reason) {
      await decideApproval(approvalId, decision, reason)
    },
    async loadMoreMessages() {
      if (closed) throw clientFailure('CHAT_VIEW_MODEL_CLOSED', 'The Chat view-model is closed.')
      const cursor = currentState.messagePagination.nextCursor
      if (!currentState.messagePagination.hasMore || cursor === null) return
      const ownGeneration = generation
      const active = controller()
      patch({
        messagePagination: frozenPagination('loading', currentState.messagePagination),
      })
      try {
        const response = expectResponse(await options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionMessagesList,
          parameters: { productSessionId: activeProductSessionId },
          page: requestPage(cursor, messagePageSize),
        }, { signal: active.signal }), QueryName.SessionMessagesList)
        if (!isCurrent(ownGeneration)) return
        assertPage(response.page, response.query)
        const messages = assertMessages(response.result.items, activeProductSessionId)
        patch({
          messages: mergeMessages(
            currentState.messages,
            messages,
            activeProductSessionId,
          ),
          messagePagination: frozenPagination('idle', response.page),
        })
      } catch (error) {
        if (!isCurrent(ownGeneration)) return
        const normalized = normalizedError(error, active.signal)
        patch({
          messagePagination: frozenPagination(
            normalized.kind === 'cancelled' ? 'idle' : 'error',
            currentState.messagePagination,
            normalized,
          ),
        })
      } finally {
        releaseController(active)
      }
    },
    cancelPending() {
      if (closed) return
      generation += 1
      abortRequests()
      realtime?.close()
      realtime = null
      patch({
        status: 'cancelled',
        realtime: 'inactive',
        error: new ControlPlaneClientError({
          kind: 'cancelled',
          code: 'REQUEST_CANCELLED',
          message: 'The Chat view request was cancelled.',
          requestId: null,
          retryable: false,
        }),
      })
    },
    reconnect() {
      if (closed) throw clientFailure('CHAT_VIEW_MODEL_CLOSED', 'The Chat view-model is closed.')
      if (realtime === null) throw clientFailure(
        'CHAT_SUBSCRIPTION_INACTIVE',
        'The Chat subscription is not active.',
      )
      patch({ realtime: 'reconnecting', error: null })
      realtime.reconnect()
    },
    close() {
      if (closed) return
      closed = true
      generation += 1
      abortRequests()
      realtime?.close()
      realtime = null
      publish({
        status: 'closed',
        realtime: 'closed',
        activeProductSessionId,
        sessions: Object.freeze([]),
        session: null,
        messages: Object.freeze([]),
        messagePagination: frozenPagination('idle', EMPTY_PAGE),
        defaultModelRoute: null,
        runtime: null,
        pendingInputs: Object.freeze([]),
        pendingApprovals: Object.freeze([]),
        interaction: frozenInteraction('idle'),
        error: null,
      })
      listeners.clear()
    },
  }
}
