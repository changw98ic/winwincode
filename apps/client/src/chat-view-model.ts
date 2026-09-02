// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneSubscription,
} from './control-plane-client.js'
import { createQueryCacheLifecycle } from './core/query-cache.js'
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
  ModelRouteAvailabilityListResultResponse,
  ModelRouteAvailabilityPage,
  ModelRouteAvailabilityProjection,
  OpaqueCursor,
  PageInfo,
  ProjectScope,
  ProductSessionId,
  ProductSessionProjection,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
  RuntimeProjectionSnapshot,
  RuntimeProjectionGetResultResponse,
  Scope,
  SessionCancelCompletedResponse,
  SessionCreateCompletedResponse,
  SessionGetResultResponse,
  SessionMessagesListResultResponse,
} from './generated/contracts.js'
import {
  CommandName,
  ControlPlaneWebSocketEventType,
  ModelRouteAvailabilityReason,
  ModelRouteAvailabilityStatus,
  QueryName,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const DEFAULT_MESSAGE_PAGE_SIZE = 50
const DEFAULT_RUNTIME_PAGE_SIZE = 50
const DEFAULT_APPROVAL_PAGE_SIZE = 50
const DEFAULT_INTERACTION_PAGE_SIZE = 50
const MODEL_ROUTE_PAGE_SIZE = 200
const MAX_MODEL_ROUTE_PAGES = 10

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
  readonly activeProductSessionId: ProductSessionId | null
  readonly sessions: readonly ProductSessionProjection[]
  readonly session: ProductSessionProjection | null
  readonly messages: readonly ChatMessageProjection[]
  readonly messagePagination: ChatMessagePagination
  readonly modelRouteAvailability: ModelRouteAvailabilityPage | null
  readonly selectedModelRoute: ModelRoute | null
  readonly modelRouteSelectionIssue: ModelRouteAvailabilityReason | null
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
  readonly productSessionId: ProductSessionId | null
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  /** Keep the browser route bound to the ProductSession selected by this view-model. */
  readonly onActiveSessionChange?: (productSessionId: ProductSessionId) => void
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
}

export interface ChatViewModel {
  readonly state: ChatViewModelState
  subscribe(listener: ChatViewModelListener): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  selectSession(productSessionId: ProductSessionId): Promise<void>
  selectModelRoute(modelRoute: ModelRoute): void
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
  readonly session: ProductSessionProjection | null
  readonly messages: readonly ChatMessageProjection[]
  readonly messagePage: PageInfo
  readonly modelRouteAvailability: ModelRouteAvailabilityPage
  readonly runtime: RuntimeProjectionSnapshot | null
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
    activeProductSessionId: null,
    sessions: Object.freeze([]),
    session: null,
    messages: Object.freeze([]),
    messagePagination: frozenPagination('idle', EMPTY_PAGE),
    modelRouteAvailability: null,
    selectedModelRoute: null,
    modelRouteSelectionIssue: null,
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
  readonly [QueryName.ModelRouteAvailabilityList]: ModelRouteAvailabilityListResultResponse
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

function sameProject(
  project: ProjectScope,
  repository: RepositoryScope,
): boolean {
  return project.organizationId === repository.organizationId
    && project.workspaceId === repository.workspaceId
    && project.projectId === repository.projectId
}

function scopeIdentity(scope: Scope): string {
  if (scope.kind === 'organization') return `${scope.kind}\u0000${scope.organizationId}`
  if (scope.kind === 'workspace') {
    return `${scope.kind}\u0000${scope.organizationId}\u0000${scope.workspaceId}`
  }
  if (scope.kind === 'project') {
    return `${scope.kind}\u0000${scope.organizationId}\u0000${scope.workspaceId}`
      + `\u0000${scope.projectId}`
  }
  return `${scope.kind}\u0000${scope.organizationId}\u0000${scope.workspaceId}`
    + `\u0000${scope.projectId}\u0000${scope.repositoryId}`
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

function modelRouteIdentity(route: ModelRoute): string {
  return `${route.providerId}\u0000${route.modelId}\u0000${route.credentialReferenceId}`
}

function isReadyModelRoute(candidate: ModelRouteAvailabilityProjection): boolean {
  return candidate.status === ModelRouteAvailabilityStatus.Enabled
    && candidate.reason === ModelRouteAvailabilityReason.Ready
}

function availabilitySubscriptionId(
  subscriptionId: ControlPlaneWebSocketSubscriptionId,
  offset: number,
): ControlPlaneWebSocketSubscriptionId {
  const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'
  const value = [...subscriptionId]
  let carry = offset
  for (let index = value.length - 1; index >= 4 && carry > 0; index -= 1) {
    const digit = alphabet.indexOf(value[index] ?? '')
    if (digit < 0) throw clientFailure(
      'CHAT_SUBSCRIPTION_ID_INVALID',
      'The Chat subscription identity is not canonical.',
    )
    const sum = digit + carry
    value[index] = alphabet[sum % alphabet.length] ?? '0'
    carry = Math.floor(sum / alphabet.length)
  }
  if (carry > 0) throw clientFailure(
    'CHAT_SUBSCRIPTION_ID_EXHAUSTED',
    'The Chat subscription identity range is exhausted.',
  )
  return value.join('') as ControlPlaneWebSocketSubscriptionId
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
  const queryCache = createQueryCacheLifecycle(options)
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
  let modelRouteRealtime: ControlPlaneSubscription[] = []
  let generation = 0
  let closed = false
  let activeProductSessionId = options.productSessionId
  let lastNotifiedProductSessionId = options.productSessionId
  let selectedModelRouteIdentity: string | null = null
  let modelRouteSelectionEstablished = false
  let modelRouteSelectionIssue: ModelRouteAvailabilityReason | null = null
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

  function reconcileSelectedModelRoute(
    availability: ModelRouteAvailabilityPage,
  ): ModelRoute | null {
    if (!modelRouteSelectionEstablished) {
      const defaultRoute = availability.items.find(candidate => (
        candidate.isDefault && isReadyModelRoute(candidate)
      ))
      if (defaultRoute === undefined) return null
      selectedModelRouteIdentity = modelRouteIdentity(defaultRoute.route)
      modelRouteSelectionEstablished = true
      modelRouteSelectionIssue = null
      return defaultRoute.route
    }
    if (selectedModelRouteIdentity === null) return null
    const matching = availability.items.find(candidate => (
      modelRouteIdentity(candidate.route) === selectedModelRouteIdentity
    ))
    if (matching === undefined || !isReadyModelRoute(matching)) {
      modelRouteSelectionIssue = matching?.reason ?? availability.reason
      selectedModelRouteIdentity = null
      return null
    }
    return matching.route
  }

  function clearModelRouteSelection(): void {
    if (selectedModelRouteIdentity !== null || currentState.selectedModelRoute !== null) {
      modelRouteSelectionEstablished = true
    }
    selectedModelRouteIdentity = null
  }

  function notifyActiveSession(): void {
    if (
      activeProductSessionId === null
      || activeProductSessionId === lastNotifiedProductSessionId
    ) return
    lastNotifiedProductSessionId = activeProductSessionId
    options.onActiveSessionChange?.(activeProductSessionId)
  }

  function requireActiveSession(): ProductSessionId {
    if (activeProductSessionId !== null) return activeProductSessionId
    throw clientFailure(
      'CHAT_SESSION_REQUIRED',
      'Select or create a Chat session before continuing.',
    )
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

  async function modelRouteAvailability(
    signal: AbortSignal,
  ): Promise<ModelRouteAvailabilityPage> {
    const items: ModelRouteAvailabilityProjection[] = []
    const identities = new Set<string>()
    const cursors = new Set<OpaqueCursor>()
    let firstPage: ModelRouteAvailabilityPage | null = null
    let cursor: OpaqueCursor | null = null
    for (let index = 0; index < MAX_MODEL_ROUTE_PAGES; index += 1) {
      const response: ModelRouteAvailabilityListResultResponse = expectResponse(
        await options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.ModelRouteAvailabilityList,
          parameters: {},
          page: requestPage(cursor, MODEL_ROUTE_PAGE_SIZE),
        }, { signal }), QueryName.ModelRouteAvailabilityList)
      assertPage(response.page, response.query)
      if (!sameScope(response.result.scope, options.scope)) throw clientFailure(
        'CHAT_MODEL_ROUTE_SCOPE_MISMATCH',
        'The model-route availability page belongs to another repository.',
      )
      if (!sameProject(response.result.requestPoolSource, options.scope)) {
        throw clientFailure(
          'CHAT_MODEL_ROUTE_REQUEST_POOL_SCOPE_MISMATCH',
          'The model-route request-pool source belongs to another Project.',
        )
      }
      if (firstPage === null) {
        firstPage = response.result
      } else if (
        !sameScope(firstPage.scope, response.result.scope)
        || firstPage.settingsRevision !== response.result.settingsRevision
        || firstPage.defaultProviderId !== response.result.defaultProviderId
        || firstPage.defaultModelId !== response.result.defaultModelId
        || firstPage.status !== response.result.status
        || firstPage.reason !== response.result.reason
        || firstPage.requestPoolRevision !== response.result.requestPoolRevision
        || scopeIdentity(firstPage.requestPoolSource)
          !== scopeIdentity(response.result.requestPoolSource)
        || (firstPage.settingsSource === null) !== (response.result.settingsSource === null)
        || (
          firstPage.settingsSource !== null
          && response.result.settingsSource !== null
          && scopeIdentity(firstPage.settingsSource)
            !== scopeIdentity(response.result.settingsSource)
        )
      ) throw clientFailure(
        'CHAT_MODEL_ROUTE_SNAPSHOT_MISMATCH',
        'The paginated model-route availability response changed during one read.',
      )
      for (const item of response.result.items) {
        const identity = modelRouteIdentity(item.route)
        if (identities.has(identity)) throw clientFailure(
          'CHAT_MODEL_ROUTE_DUPLICATE',
          'The model-route availability page contains duplicate routes.',
        )
        identities.add(identity)
        items.push(item)
      }
      if (!response.page.hasMore) return Object.freeze({
        ...firstPage,
        items: Object.freeze(items),
      })
      const next: OpaqueCursor | null = response.page.nextCursor
      if (next === null || cursors.has(next)) throw clientFailure(
        'CHAT_MODEL_ROUTE_CURSOR_INVALID',
        'The model-route availability query returned an invalid continuation cursor.',
      )
      cursors.add(next)
      cursor = next
    }
    throw clientFailure(
      'CHAT_MODEL_ROUTE_PAGE_LIMIT',
      'The model-route availability query exceeded the bounded page limit.',
    )
  }

  async function querySnapshot(signal: AbortSignal): Promise<ChatSnapshot> {
    if (activeProductSessionId === null) {
      const [sessionsValue, routes] = await Promise.all([
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionList,
          parameters: { states: [] },
          page: requestPage(null, 50),
        }, { signal }),
        modelRouteAvailability(signal),
      ])
      const sessions = expectResponse(sessionsValue, QueryName.SessionList)
      assertPage(sessions.page, sessions.query)
      for (const item of sessions.result.items) {
        if (
          item.projectId !== options.scope.projectId
          || item.repositoryId !== options.scope.repositoryId
        ) throw clientFailure(
          'CHAT_SESSION_LIST_SCOPE_MISMATCH',
          'The session list contains a ProductSession from another repository.',
        )
      }
      const orderedSessions = orderSessions(sessions.result.items)
      const firstSession = orderedSessions[0]
      if (firstSession !== undefined) {
        activeProductSessionId = firstSession.id
        return querySnapshot(signal)
      }
      return Object.freeze({
        sessions: orderedSessions,
        session: null,
        messages: Object.freeze([]),
        messagePage: EMPTY_PAGE,
        modelRouteAvailability: routes,
        runtime: null,
        pendingInputs: Object.freeze([]),
        pendingApprovals: Object.freeze([]),
      })
    }
    const productSessionId = activeProductSessionId
    const [
      sessionsValue,
      sessionValue,
      messagesValue,
      routes,
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
          parameters: { productSessionId },
          page: requestPage(null, 1),
        }, { signal }),
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionMessagesList,
          parameters: { productSessionId },
          page: requestPage(null, messagePageSize),
        }, { signal }),
        modelRouteAvailability(signal),
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.RuntimeProjectionGet,
          parameters: {
            kind: 'product-session',
            productSessionId,
          },
          page: requestPage(null, runtimePageSize),
        }, { signal }),
        options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionInteractionsList,
          parameters: {
            productSessionId,
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
      runtime,
      interactions,
      approvals,
    ]) {
      assertPage(response.page, response.query)
    }
    if (
      session.result.id !== productSessionId
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
    assertRuntime(runtime.result, options.scope, productSessionId)
    return Object.freeze({
      sessions: orderedSessions,
      session: session.result,
      messages: assertMessages(messages.result.items, productSessionId),
      messagePage: messages.page,
      modelRouteAvailability: routes,
      runtime: runtime.result,
      pendingInputs: assertPendingInputs(
        interactions.result.items,
        productSessionId,
        nowMillis(),
      ),
      pendingApprovals: assertPendingApprovals(
        approvals.result.items,
        productSessionId,
        nowMillis(),
      ),
    })
  }

  function publishSnapshot(
    snapshot: ChatSnapshot,
    realtimeStatus: ChatRealtimeStatus,
  ): void {
    const selectedModelRoute = reconcileSelectedModelRoute(
      snapshot.modelRouteAvailability,
    )
    publish({
      status: 'ready',
      realtime: realtimeStatus,
      activeProductSessionId,
      sessions: snapshot.sessions,
      session: snapshot.session,
      messages: snapshot.messages,
      messagePagination: frozenPagination('idle', snapshot.messagePage),
      modelRouteAvailability: snapshot.modelRouteAvailability,
      selectedModelRoute,
      modelRouteSelectionIssue,
      runtime: snapshot.runtime,
      pendingInputs: snapshot.pendingInputs,
      pendingApprovals: snapshot.pendingApprovals,
      interaction: frozenInteraction('idle'),
      error: null,
    })
    notifyActiveSession()
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
      modelRouteAvailability: null,
      selectedModelRoute: null,
      modelRouteSelectionIssue,
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
      return snapshot.runtime?.eventCursor ?? null
    } catch (error) {
      if (!isCurrent(ownGeneration)) return null
      const normalized = normalizedError(error, active.signal)
      clearModelRouteSelection()
      patch({
        status: statusForError(normalized),
        realtime: normalized.kind === 'authentication' || normalized.kind === 'authorization'
          ? 'access-revoked'
          : 'reconnecting',
        modelRouteAvailability: null,
        selectedModelRoute: null,
        modelRouteSelectionIssue,
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
      const productSessionId = requireActiveSession()
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.SessionGet,
        parameters: { productSessionId },
        page: requestPage(null, 1),
      }, { signal: active.signal }), QueryName.SessionGet)
      if (!isCurrent(ownGeneration)) return
      assertPage(response.page, response.query)
      if (
        response.result.id !== productSessionId
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
      const productSessionId = requireActiveSession()
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.RuntimeProjectionGet,
        parameters: {
          kind: 'product-session',
          productSessionId,
        },
        page: requestPage(null, runtimePageSize),
      }, { signal: active.signal }), QueryName.RuntimeProjectionGet)
      if (!isCurrent(ownGeneration)) return
      assertPage(response.page, response.query)
      assertRuntime(response.result, options.scope, productSessionId)
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
      const productSessionId = requireActiveSession()
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
          productSessionId,
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
      const productSessionId = requireActiveSession()
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.SessionInteractionsList,
        parameters: {
          productSessionId,
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
          productSessionId,
          nowMillis(),
        ),
        error: null,
      })
    } finally {
      releaseController(active)
    }
  }

  async function reloadModelRouteAvailability(
    ownGeneration: number,
    minimumRequestPoolRevision: number | null = null,
  ): Promise<void> {
    const active = controller()
    try {
      const availability = await modelRouteAvailability(active.signal)
      if (!isCurrent(ownGeneration)) return
      if (
        minimumRequestPoolRevision !== null
        && availability.requestPoolRevision < minimumRequestPoolRevision
      ) throw clientFailure(
        'CHAT_MODEL_ROUTE_EVENT_STALE',
        'The model-route availability snapshot is older than its request-pool event.',
      )
      const selectedModelRoute = reconcileSelectedModelRoute(availability)
      patch({
        status: 'ready',
        modelRouteAvailability: availability,
        selectedModelRoute,
        modelRouteSelectionIssue,
        interaction: frozenInteraction('idle'),
        error: null,
      })
      subscribeModelRouteAvailability(availability)
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      const normalized = normalizedError(error, active.signal)
      if (normalized.kind === 'authentication' || normalized.kind === 'authorization') {
        accessRevoked(normalized)
      } else {
        clearModelRouteSelection()
        patch({
          status: statusForError(normalized),
          modelRouteAvailability: null,
          selectedModelRoute: null,
          modelRouteSelectionIssue,
          error: normalized,
        })
      }
      throw normalized
    } finally {
      releaseController(active)
    }
  }

  async function applyModelRouteEvent(
    frame: ControlPlaneWebSocketEventFrame,
    binding: {
      readonly scope: Scope
      readonly authority: boolean
      readonly requestPool: boolean
    },
  ): Promise<void> {
    const ownGeneration = generation
    if (!isCurrent(ownGeneration)) return
    const event = frame.event
    if (
      event.type !== 'model-route-availability.invalidated.v1'
      || event.reloadQueries.length !== 1
      || event.reloadQueries[0] !== QueryName.ModelRouteAvailabilityList
    ) throw clientFailure(
      'CHAT_MODEL_ROUTE_EVENT_INVALID',
      'The model-route subscription received an invalid reload instruction.',
    )
    if (event.source === 'request_pool') {
      const availability = currentState.modelRouteAvailability
      if (
        !binding.requestPool
        || availability === null
        || scopeIdentity(binding.scope) !== scopeIdentity(availability.requestPoolSource)
      ) throw clientFailure(
        'CHAT_MODEL_ROUTE_EVENT_SCOPE_MISMATCH',
        'The request-pool invalidation came from another Project.',
      )
      if (event.sourceRevision <= availability.requestPoolRevision) return
    } else if (!binding.authority) {
      throw clientFailure(
        'CHAT_MODEL_ROUTE_EVENT_SCOPE_MISMATCH',
        'The model-route authority invalidation came from an unrelated Scope.',
      )
    }
    patch({ status: 'refreshing' })
    await reloadModelRouteAvailability(
      ownGeneration,
      event.source === 'request_pool' ? event.sourceRevision : null,
    )
  }

  async function applyEvent(frame: ControlPlaneWebSocketEventFrame): Promise<void> {
    const ownGeneration = generation
    if (!isCurrent(ownGeneration)) return
    patch({ realtime: 'reloading' })
    try {
      const productSessionId = requireActiveSession()
      const event = frame.event
      if ('productSessionId' in event && event.productSessionId !== productSessionId) {
        throw clientFailure(
          'CHAT_EVENT_SESSION_MISMATCH',
          'A Chat event belongs to another ProductSession.',
        )
      }
      if (event.type === 'product-session.changed.v1') {
        await reloadSession(frame, ownGeneration)
      } else if (event.type === 'product-session.message.appended.v1') {
        const incoming = assertMessages([event.message], productSessionId)
        if (isCurrent(ownGeneration)) patch({
          status: 'ready',
          realtime: 'subscribed',
          messages: mergeMessages(
            currentState.messages,
            incoming,
            productSessionId,
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

  function closeModelRouteRealtime(): void {
    for (const subscription of modelRouteRealtime) subscription.close()
    modelRouteRealtime = []
  }

  function accessRevoked(error: ControlPlaneClientError): void {
    generation += 1
    abortRequests()
    realtime?.close()
    realtime = null
    closeModelRouteRealtime()
    clearModelRouteSelection()
    publish({
      status: statusForError(error),
      realtime: 'access-revoked',
      activeProductSessionId,
      sessions: Object.freeze([]),
      session: null,
      messages: Object.freeze([]),
      messagePagination: frozenPagination('idle', EMPTY_PAGE),
      modelRouteAvailability: null,
      selectedModelRoute: null,
      modelRouteSelectionIssue: null,
      runtime: null,
      pendingInputs: Object.freeze([]),
      pendingApprovals: Object.freeze([]),
      interaction: frozenInteraction('error', error),
      error,
    })
  }

  function subscribeModelRouteAvailability(
    availability: ModelRouteAvailabilityPage,
  ): void {
    closeModelRouteRealtime()
    const bindings = new Map<string, {
      scope: Scope
      authority: boolean
      requestPool: boolean
    }>()
    function add(scope: Scope, source: 'authority' | 'request-pool'): void {
      const identity = scopeIdentity(scope)
      const current = bindings.get(identity) ?? {
        scope,
        authority: false,
        requestPool: false,
      }
      if (source === 'authority') current.authority = true
      else current.requestPool = true
      bindings.set(identity, current)
    }
    add(availability.scope, 'authority')
    if (availability.settingsSource !== null) {
      add(availability.settingsSource, 'authority')
    }
    for (const candidate of availability.items) {
      add(candidate.catalogSource, 'authority')
    }
    add(availability.requestPoolSource, 'request-pool')

    modelRouteRealtime = [...bindings.values()].map((binding, index) => (
      options.client.subscribe({
        subscriptionId: availabilitySubscriptionId(options.subscriptionId, index + 1),
        subscription: {
          scope: binding.scope,
          stream: { kind: 'scope' },
          eventTypes: [
            ControlPlaneWebSocketEventType.ModelRouteAvailabilityInvalidatedV1,
          ],
        },
        onEvent(frame) {
          return applyModelRouteEvent(frame, binding)
        },
        async onResetRequired(frame) {
          if (frame === null) throw clientFailure(
            'CHAT_MODEL_ROUTE_RESET_INVALID',
            'The model-route subscription reset did not include a replay cursor.',
          )
          const ownGeneration = generation
          patch({ status: 'refreshing' })
          await reloadModelRouteAvailability(ownGeneration)
          return frame.earliestAvailable
        },
        onAuthorizationRevoked() {
          accessRevoked(new ControlPlaneClientError({
            kind: 'authentication',
            code: 'AUTHENTICATION_REQUIRED',
            message: 'The model-route subscription authorization is no longer valid.',
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
          patch({ status: statusForError(error), error })
        },
      })
    ))
  }

  function subscribeRealtime(cursor: EventReadCursor): void {
    const productSessionId = requireActiveSession()
    realtime?.close()
    realtime = options.client.subscribe({
      subscriptionId: options.subscriptionId,
      subscription: {
        scope: options.scope,
        stream: { kind: 'product-session', productSessionId },
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
      const productSessionId = requireActiveSession()
      const response = expectResponse(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.SessionMessagesList,
        parameters: { productSessionId },
        page: requestPage(null, messagePageSize),
      }, { signal: active.signal }), QueryName.SessionMessagesList)
      if (!isCurrent(ownGeneration)) return
      assertPage(response.page, response.query)
      patch({
        messages: assertMessages(response.result.items, productSessionId),
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
    const productSessionId = requireActiveSession()
    const ownGeneration = generation
    const active = controller()
    patch({ interaction: frozenInteraction('submitting') })
    try {
      const response = await options.client.command({
        ...requestBase(),
        requestId: options.nextRequestId(),
        command: CommandName.ChatSubmit,
        expectedRevision: session.revision,
        payload: { productSessionId, message: value },
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
    if (currentState.status !== 'ready') {
      interactionFailure(
        'CHAT_MODEL_ROUTE_REFRESH_REQUIRED',
        'Wait for the current model-route refresh before creating a Chat.',
      )
      return
    }
    const modelRoute = currentState.selectedModelRoute
    if (
      modelRoute === null
      || currentState.modelRouteAvailability === null
      || !currentState.modelRouteAvailability.items.some(candidate => (
        isReadyModelRoute(candidate)
        && modelRouteIdentity(candidate.route) === modelRouteIdentity(modelRoute)
      ))
    ) {
      interactionFailure(
        'CHAT_MODEL_ROUTE_UNAVAILABLE',
        'Select a currently available model route before creating a Chat.',
      )
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
          modelRoute,
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
      notifyActiveSession()
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
    const productSessionId = requireActiveSession()
    const ownGeneration = generation
    const active = controller()
    patch({ interaction: frozenInteraction('cancelling') })
    try {
      const response = await options.client.command({
        ...requestBase(),
        requestId: options.nextRequestId(),
        command: CommandName.SessionCancel,
        expectedRevision: session.revision,
        payload: { productSessionId, reason: value },
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
    assertBinding(input.binding, requireActiveSession())
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
    assertBinding(approval.binding, requireActiveSession())
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
    closeModelRouteRealtime()
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
            modelRouteAvailability: null,
            selectedModelRoute: null,
            modelRouteSelectionIssue,
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
      if (!isCurrent(ownGeneration)) return
      if (currentState.modelRouteAvailability !== null) {
        subscribeModelRouteAvailability(currentState.modelRouteAvailability)
      }
      if (cursor !== null) subscribeRealtime(cursor)
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
      queryCache.refresh()
      await load(false)
    },
    async selectSession(productSessionId) {
      if (closed) throw clientFailure('CHAT_VIEW_MODEL_CLOSED', 'The Chat view-model is closed.')
      if (productSessionId === activeProductSessionId && currentState.status === 'ready') return
      activeProductSessionId = productSessionId
      await load(true)
    },
    selectModelRoute(modelRoute) {
      if (closed) throw clientFailure('CHAT_VIEW_MODEL_CLOSED', 'The Chat view-model is closed.')
      if (currentState.status !== 'ready') {
        interactionFailure(
          'CHAT_MODEL_ROUTE_REFRESH_REQUIRED',
          'Wait for the current model-route refresh before selecting a model.',
        )
        return
      }
      const identity = modelRouteIdentity(modelRoute)
      const selected = currentState.modelRouteAvailability?.items.find(candidate => (
        isReadyModelRoute(candidate)
        && modelRouteIdentity(candidate.route) === identity
      ))
      if (selected === undefined) {
        interactionFailure(
          'CHAT_MODEL_ROUTE_UNAVAILABLE',
          'Refresh Chat and select a currently available model route.',
        )
        return
      }
      selectedModelRouteIdentity = identity
      modelRouteSelectionEstablished = true
      modelRouteSelectionIssue = null
      patch({
        selectedModelRoute: selected.route,
        modelRouteSelectionIssue: null,
        interaction: frozenInteraction('idle'),
      })
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
        const productSessionId = requireActiveSession()
        const response = expectResponse(await options.client.query({
          ...requestBase(),
          requestId: options.nextRequestId(),
          query: QueryName.SessionMessagesList,
          parameters: { productSessionId },
          page: requestPage(cursor, messagePageSize),
        }, { signal: active.signal }), QueryName.SessionMessagesList)
        if (!isCurrent(ownGeneration)) return
        assertPage(response.page, response.query)
        const messages = assertMessages(response.result.items, productSessionId)
        patch({
          messages: mergeMessages(
            currentState.messages,
            messages,
            productSessionId,
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
      closeModelRouteRealtime()
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
      if (realtime === null && modelRouteRealtime.length === 0) throw clientFailure(
        'CHAT_SUBSCRIPTION_INACTIVE',
        'The Chat subscription is not active.',
      )
      patch({ realtime: 'reconnecting', error: null })
      realtime?.reconnect()
      for (const subscription of modelRouteRealtime) subscription.reconnect()
      void load(false)
    },
    close() {
      if (closed) return
      closed = true
      queryCache.close()
      generation += 1
      abortRequests()
      realtime?.close()
      realtime = null
      closeModelRouteRealtime()
      clearModelRouteSelection()
      publish({
        status: 'closed',
        realtime: 'closed',
        activeProductSessionId,
        sessions: Object.freeze([]),
        session: null,
        messages: Object.freeze([]),
        messagePagination: frozenPagination('idle', EMPTY_PAGE),
        modelRouteAvailability: null,
        selectedModelRoute: null,
        modelRouteSelectionIssue: null,
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
