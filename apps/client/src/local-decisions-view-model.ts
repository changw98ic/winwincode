// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneSubscription,
} from './control-plane-client.js'
import { invalidateClientQueryCache } from './core/query-cache.js'
import type {
  Actor,
  ApprovalDecideCompletedResponse,
  ApprovalListResultResponse,
  ApprovalProjection,
  ChatInputInteractionProjection,
  ChatInteractionListResultResponse,
  CommandAcceptedResponse,
  CommandCompletedResponse,
  ControlPlaneWebSocketSubscriptionId,
  DeliveryAttentionProjection,
  DeliveryGetResultResponse,
  DeliveryId,
  DeliveryResolveAttentionCompletedResponse,
  InteractiveInputValue,
  InputRespondCompletedResponse,
  OpaqueCursor,
  ProductSessionId,
  ProductSessionProjection,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
  SessionGetResultResponse,
} from './generated/contracts.js'
import {
  CommandName,
  ControlPlaneWebSocketEventType,
  QueryName,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const PAGE_SIZE = 200
const MAX_PAGES = 10

export type LocalDecisionsStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'refreshing'
  | 'authentication-required'
  | 'authorization-denied'
  | 'cancelled'
  | 'error'
  | 'closed'

export type LocalDecisionsRealtimeStatus =
  | 'inactive'
  | 'subscribed'
  | 'reloading'
  | 'reconnecting'
  | 'access-revoked'
  | 'closed'

export type LocalDecisionOperation =
  | 'input.respond'
  | 'approval.decide'
  | 'delivery.resolve_attention'

export interface LocalDecisionInteractionState {
  readonly status: 'idle' | 'submitting' | 'waiting' | 'error'
  readonly operation: LocalDecisionOperation | null
  readonly targetId: string | null
  readonly error: ControlPlaneClientError | null
}

export interface LocalInputDecision {
  readonly projection: ChatInputInteractionProjection
  readonly expired: boolean
}

export interface LocalApprovalDecision {
  readonly projection: ApprovalProjection
  readonly expired: boolean
}

export interface LocalAttentionDecision {
  readonly projection: DeliveryAttentionProjection
  readonly deliveryId: DeliveryId
  readonly deliveryRevision: number
  readonly candidateDigest: string | null
}

export interface LocalDecisionsViewModelState {
  readonly status: LocalDecisionsStatus
  readonly realtime: LocalDecisionsRealtimeStatus
  readonly session: ProductSessionProjection | null
  readonly inputs: readonly LocalInputDecision[]
  readonly approvals: readonly LocalApprovalDecision[]
  readonly attention: readonly LocalAttentionDecision[]
  readonly interaction: LocalDecisionInteractionState
  readonly error: ControlPlaneClientError | null
}

export interface LocalDecisionsDeliveryOptions {
  readonly deliveryId: DeliveryId
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
}

export interface LocalDecisionsViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly productSessionId: ProductSessionId
  readonly interactionSubscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly delivery?: LocalDecisionsDeliveryOptions
  readonly nextRequestId: () => RequestId
  readonly nowMillis?: () => number
}

export type LocalDecisionsListener = (state: LocalDecisionsViewModelState) => void

export interface LocalDecisionsViewModel {
  readonly state: LocalDecisionsViewModelState
  subscribe(listener: LocalDecisionsListener): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  provideInput(inputRequestId: string, value: InteractiveInputValue): Promise<void>
  cancelInput(inputRequestId: string): Promise<void>
  decideApproval(
    approvalId: string,
    decision: 'approve' | 'reject',
    reason: string,
  ): Promise<void>
  resolveAttention(
    attentionItemId: string,
    decision: 'resolve' | 'dismiss',
    resolution: string,
  ): Promise<void>
  cancelPending(): void
  reconnect(): void
  close(): void
}

interface LocalDecisionSnapshot {
  readonly session: ProductSessionProjection
  readonly inputs: readonly LocalInputDecision[]
  readonly approvals: readonly LocalApprovalDecision[]
  readonly attention: readonly LocalAttentionDecision[]
}

interface LocalDecisionCommandResponses {
  readonly [CommandName.InputRespond]: InputRespondCompletedResponse
  readonly [CommandName.ApprovalDecide]: ApprovalDecideCompletedResponse
  readonly [CommandName.DeliveryResolveAttention]: DeliveryResolveAttentionCompletedResponse
}

function interaction(
  status: LocalDecisionInteractionState['status'],
  operation: LocalDecisionOperation | null = null,
  targetId: string | null = null,
  error: ControlPlaneClientError | null = null,
): LocalDecisionInteractionState {
  return Object.freeze({ status, operation, targetId, error })
}

function initialState(): LocalDecisionsViewModelState {
  return Object.freeze({
    status: 'idle',
    realtime: 'inactive',
    session: null,
    inputs: Object.freeze([]),
    approvals: Object.freeze([]),
    attention: Object.freeze([]),
    interaction: interaction('idle'),
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
    message: 'The local decision request was cancelled.',
    requestId: null,
    retryable: false,
    cause: error,
  })
  return clientFailure('LOCAL_DECISIONS_FAILURE', 'Local decisions could not be updated.', error)
}

function statusForError(error: ControlPlaneClientError): LocalDecisionsStatus {
  if (error.kind === 'authentication') return 'authentication-required'
  if (error.kind === 'authorization') return 'authorization-denied'
  if (error.kind === 'cancelled') return 'cancelled'
  return 'error'
}

function page(cursor: OpaqueCursor | null, limit = PAGE_SIZE) {
  return Object.freeze({ cursor, limit })
}

function expectQuery<Query extends QueryResultResponse['query']>(
  response: QueryResultResponse,
  query: Query,
): Extract<QueryResultResponse, { readonly query: Query }> {
  if (response.query !== query) throw clientFailure(
    'LOCAL_DECISIONS_QUERY_MISMATCH',
    'The Control Plane returned another local decision query result.',
  )
  return response as Extract<QueryResultResponse, { readonly query: Query }>
}

function cursorAfter(
  response: { readonly page: { readonly hasMore: boolean; readonly nextCursor: OpaqueCursor | null } },
  seen: Set<OpaqueCursor>,
): OpaqueCursor | null {
  if (!response.page.hasMore) {
    if (response.page.nextCursor !== null) throw clientFailure(
      'LOCAL_DECISIONS_PAGE_INVALID',
      'The final local decision page returned an unexpected cursor.',
    )
    return null
  }
  const next = response.page.nextCursor
  if (next === null || seen.has(next)) throw clientFailure(
    'LOCAL_DECISIONS_CURSOR_INVALID',
    'The local decision list returned an invalid continuation cursor.',
  )
  seen.add(next)
  return next
}

function bindingMatches(
  binding: ChatInputInteractionProjection['binding'],
  productSessionId: ProductSessionId,
): boolean {
  return binding.productSessionId === productSessionId
    && binding.sessionIdentity.productSessionId === productSessionId
    && binding.workerSessionId === binding.sessionIdentity.workerSessionId
}

function expired(expiresAt: string, nowMillis: number): boolean {
  const instant = Date.parse(expiresAt)
  return !Number.isFinite(instant) || instant <= nowMillis
}

function safeInputs(
  items: readonly ChatInputInteractionProjection[],
  productSessionId: ProductSessionId,
  nowMillis: number,
): readonly LocalInputDecision[] {
  const seen = new Set<string>()
  const result: LocalInputDecision[] = []
  for (const projection of items) {
    if (!bindingMatches(projection.binding, productSessionId) || seen.has(projection.inputRequestId)) {
      throw clientFailure(
        'LOCAL_DECISIONS_INPUT_BINDING_INVALID',
        'A local input does not match its complete ProductSession binding.',
      )
    }
    seen.add(projection.inputRequestId)
    result.push(Object.freeze({
      projection,
      expired: projection.state !== 'pending' || expired(projection.expiresAt, nowMillis),
    }))
  }
  return Object.freeze(result)
}

function safeApprovals(
  items: readonly ApprovalProjection[],
  productSessionId: ProductSessionId,
  nowMillis: number,
): readonly LocalApprovalDecision[] {
  const seen = new Set<string>()
  const result: LocalApprovalDecision[] = []
  for (const projection of items) {
    if (projection.binding.productSessionId !== productSessionId) continue
    if (!bindingMatches(projection.binding, productSessionId) || seen.has(projection.id)) {
      throw clientFailure(
        'LOCAL_DECISIONS_APPROVAL_BINDING_INVALID',
        'A local approval does not match its complete ProductSession binding.',
      )
    }
    seen.add(projection.id)
    result.push(Object.freeze({
      projection,
      expired: projection.state !== 'pending' || expired(projection.expiresAt, nowMillis),
    }))
  }
  return Object.freeze(result)
}

function checkedDetail(
  response: DeliveryGetResultResponse,
  options: LocalDecisionsViewModelOptions,
): readonly LocalAttentionDecision[] {
  const detail = response.result
  if (
    options.delivery === undefined
    || detail.deliveryId !== options.delivery.deliveryId
    || detail.ownership.organizationId !== options.scope.organizationId
    || detail.ownership.workspaceId !== options.scope.workspaceId
    || detail.ownership.projectId !== options.scope.projectId
    || detail.ownership.repositoryId !== options.scope.repositoryId
  ) throw clientFailure(
    'LOCAL_DECISIONS_DELIVERY_BINDING_INVALID',
    'The Attention snapshot does not match the selected repository Delivery.',
  )
  const seen = new Set<string>()
  const attention: LocalAttentionDecision[] = []
  for (const projection of detail.attention) {
    if (projection.status !== 'open') continue
    if (seen.has(projection.id)) throw clientFailure(
      'LOCAL_DECISIONS_ATTENTION_BINDING_INVALID',
      'The Attention snapshot contains a duplicate decision identity.',
    )
    seen.add(projection.id)
    attention.push(Object.freeze({
      projection,
      deliveryId: detail.deliveryId,
      deliveryRevision: detail.deliveryRevision,
      candidateDigest: detail.currentCandidate?.diffSha256 ?? null,
    }))
  }
  return Object.freeze(attention)
}

function validateInputValue(
  input: ChatInputInteractionProjection,
  value: InteractiveInputValue,
): ControlPlaneClientError | null {
  if (value.mode !== input.mode) return clientFailure(
    'LOCAL_DECISIONS_INPUT_MODE_INVALID',
    'Use the response type requested by the current input.',
  )
  if (input.mode === 'text') {
    if (!input.allowEmpty && value.value.length === 0) return clientFailure(
      'LOCAL_DECISIONS_INPUT_VALUE_REQUIRED',
      'Enter a response for the current input.',
    )
    return null
  }
  if (!input.options.some(option => option.value === value.value)) return clientFailure(
    'LOCAL_DECISIONS_INPUT_OPTION_STALE',
    'Choose one current input option.',
  )
  return null
}

/** Build local input, approval, and business Attention decisions from public projections only. */
export function createLocalDecisionsViewModel(
  options: LocalDecisionsViewModelOptions,
): LocalDecisionsViewModel {
  const listeners = new Set<LocalDecisionsListener>()
  const controllers = new Set<AbortController>()
  const subscriptions = new Set<ControlPlaneSubscription>()
  const inFlight = new Set<string>()
  const nowMillis = options.nowMillis ?? Date.now
  let currentState = initialState()
  let generation = 0
  let closed = false

  function publish(state: LocalDecisionsViewModelState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<LocalDecisionsViewModelState>): void {
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

  function closeSubscriptions(): void {
    for (const subscription of subscriptions) subscription.close()
    subscriptions.clear()
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

  async function interactionItems(signal: AbortSignal): Promise<readonly ChatInputInteractionProjection[]> {
    const items: ChatInputInteractionProjection[] = []
    const seen = new Set<OpaqueCursor>()
    let cursor: OpaqueCursor | null = null
    for (let index = 0; index < MAX_PAGES; index += 1) {
      const response: ChatInteractionListResultResponse = expectQuery(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.SessionInteractionsList,
        parameters: {
          productSessionId: options.productSessionId,
          states: ['pending', 'expired'],
        },
        page: page(cursor),
      }, { signal }), QueryName.SessionInteractionsList)
      items.push(...response.result.items.filter(item => item.kind === 'input'))
      cursor = cursorAfter(response, seen)
      if (cursor === null) return Object.freeze(items)
    }
    throw clientFailure(
      'LOCAL_DECISIONS_PAGE_LIMIT_EXCEEDED',
      'The input list exceeded the bounded page limit.',
    )
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
        parameters: { states: ['pending', 'expired'] },
        page: page(cursor),
      }, { signal }), QueryName.ApprovalList)
      items.push(...response.result.items)
      cursor = cursorAfter(response, seen)
      if (cursor === null) return Object.freeze(items)
    }
    throw clientFailure(
      'LOCAL_DECISIONS_PAGE_LIMIT_EXCEEDED',
      'The approval list exceeded the bounded page limit.',
    )
  }

  async function snapshot(signal: AbortSignal): Promise<LocalDecisionSnapshot> {
    const [sessionValue, inputValues, approvalValues, deliveryValue] = await Promise.all([
      options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.SessionGet,
        parameters: { productSessionId: options.productSessionId },
        page: page(null, 1),
      }, { signal }),
      interactionItems(signal),
      approvalItems(signal),
      options.delivery === undefined
        ? Promise.resolve(null)
        : options.client.query({
            ...requestBase(),
            requestId: options.nextRequestId(),
            query: QueryName.DeliveryGet,
            parameters: { deliveryId: options.delivery.deliveryId },
            page: page(null, 1),
          }, { signal }),
    ])
    const session: SessionGetResultResponse = expectQuery(sessionValue, QueryName.SessionGet)
    if (
      session.page.hasMore
      || session.page.nextCursor !== null
      || session.result.id !== options.productSessionId
      || session.result.projectId !== options.scope.projectId
      || session.result.repositoryId !== options.scope.repositoryId
    ) throw clientFailure(
      'LOCAL_DECISIONS_SESSION_BINDING_INVALID',
      'The local decision ProductSession does not match the selected repository.',
    )
    let attention: readonly LocalAttentionDecision[] = Object.freeze([])
    if (deliveryValue !== null) {
      const delivery: DeliveryGetResultResponse = expectQuery(deliveryValue, QueryName.DeliveryGet)
      if (delivery.page.hasMore || delivery.page.nextCursor !== null) throw clientFailure(
        'LOCAL_DECISIONS_PAGE_INVALID',
        'The Attention detail returned an unexpected page cursor.',
      )
      attention = checkedDetail(delivery, options)
    }
    const clock = nowMillis()
    return Object.freeze({
      session: session.result,
      inputs: safeInputs(inputValues, options.productSessionId, clock),
      approvals: safeApprovals(approvalValues, options.productSessionId, clock),
      attention,
    })
  }

  async function load(replace: boolean, realtime: LocalDecisionsRealtimeStatus): Promise<void> {
    if (closed) throw clientFailure('LOCAL_DECISIONS_CLOSED', 'The local decisions view is closed.')
    generation += 1
    const ownGeneration = generation
    inFlight.clear()
    abortRequests()
    const active = controller()
    patch({
      status: replace ? 'loading' : 'refreshing',
      realtime,
      ...(replace
        ? {
            session: null,
            inputs: Object.freeze([]),
            approvals: Object.freeze([]),
            attention: Object.freeze([]),
          }
        : {}),
      interaction: interaction('idle'),
      error: null,
    })
    try {
      const value = await snapshot(active.signal)
      if (!isCurrent(ownGeneration)) return
      publish({
        status: 'ready',
        realtime: realtime === 'reloading' ? 'subscribed' : realtime,
        session: value.session,
        inputs: value.inputs,
        approvals: value.approvals,
        attention: value.attention,
        interaction: interaction('idle'),
        error: null,
      })
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      const normalized = normalizedError(error, active.signal)
      patch({
        status: statusForError(normalized),
        realtime: normalized.kind === 'authentication' || normalized.kind === 'authorization'
          ? 'access-revoked'
          : 'reconnecting',
        interaction: interaction('error', null, null, normalized),
        error: normalized,
      })
    } finally {
      release(active)
    }
  }

  function accessRevoked(error: ControlPlaneClientError): void {
    generation += 1
    inFlight.clear()
    abortRequests()
    closeSubscriptions()
    publish({
      status: statusForError(error),
      realtime: 'access-revoked',
      session: null,
      inputs: Object.freeze([]),
      approvals: Object.freeze([]),
      attention: Object.freeze([]),
      interaction: interaction('error', null, null, error),
      error,
    })
  }

  function subscriptionCallbacks() {
    return {
      async onEvent() {
        await load(false, 'reloading')
      },
      onAuthorizationRevoked() {
        accessRevoked(new ControlPlaneClientError({
          kind: 'authentication',
          code: 'AUTHENTICATION_REQUIRED',
          message: 'Local decision event authorization is no longer valid.',
          requestId: null,
          retryable: false,
        }))
      },
      onError(error: ControlPlaneClientError) {
        if (closed) return
        if (error.kind === 'authentication' || error.kind === 'authorization') {
          accessRevoked(error)
          return
        }
        patch({ realtime: 'reconnecting', error })
      },
    }
  }

  function subscribeRealtime(): void {
    closeSubscriptions()
    subscriptions.add(options.client.subscribe({
      subscriptionId: options.interactionSubscriptionId,
      subscription: {
        scope: options.scope,
        stream: { kind: 'product-session', productSessionId: options.productSessionId },
        eventTypes: [
          ControlPlaneWebSocketEventType.ProductSessionChangedV1,
          ControlPlaneWebSocketEventType.ApprovalChangedV1,
          ControlPlaneWebSocketEventType.ChatInteractionsInvalidatedV1,
        ],
      },
      ...subscriptionCallbacks(),
    }))
    if (options.delivery !== undefined) subscriptions.add(options.client.subscribe({
      subscriptionId: options.delivery.subscriptionId,
      subscription: {
        scope: options.scope,
        stream: { kind: 'delivery', deliveryId: options.delivery.deliveryId },
        eventTypes: [
          ControlPlaneWebSocketEventType.AttentionChangedV1,
          ControlPlaneWebSocketEventType.DeliveryChangedV1,
        ],
      },
      ...subscriptionCallbacks(),
    }))
    patch({ realtime: 'subscribed' })
  }

  function failure(
    code: string,
    message: string,
    operation: LocalDecisionOperation,
    targetId: string,
  ): void {
    patch({ interaction: interaction('error', operation, targetId, clientFailure(code, message)) })
  }

  async function runCommand<Command extends keyof LocalDecisionCommandResponses>(
    operation: Command,
    targetId: string,
    expectedRevision: number,
    request: (requestId: RequestId) => Parameters<ControlPlaneClient['command']>[0],
    apply: (response: LocalDecisionCommandResponses[Command]) => void,
  ): Promise<void> {
    const key = `${operation}\u0000${targetId}`
    if (inFlight.has(key)) return
    if (currentState.interaction.status === 'submitting' || currentState.interaction.status === 'waiting') {
      failure(
        'LOCAL_DECISIONS_COMMAND_IN_FLIGHT',
        'Wait for the current decision to finish.',
        operation,
        targetId,
      )
      return
    }
    inFlight.add(key)
    const active = controller()
    const ownGeneration = generation
    const requestId = options.nextRequestId()
    patch({ interaction: interaction('submitting', operation, targetId) })
    let waiting = false
    try {
      const response: CommandAcceptedResponse | CommandCompletedResponse = await options.client.command(
        request(requestId),
        { signal: active.signal },
      )
      if (!isCurrent(ownGeneration)) return
      if (response.requestId !== requestId || response.command !== operation) throw clientFailure(
        'LOCAL_DECISIONS_COMMAND_MISMATCH',
        'The Control Plane returned another local decision result.',
      )
      if (response.outcome === 'accepted') {
        waiting = true
        patch({ interaction: interaction('waiting', operation, targetId) })
        return
      }
      const completed = response as LocalDecisionCommandResponses[Command]
      if (completed.previousRevision !== expectedRevision) throw clientFailure(
        'LOCAL_DECISIONS_REVISION_MISMATCH',
        'The Control Plane returned a decision for another revision.',
      )
      apply(completed)
      patch({ interaction: interaction('idle'), error: null })
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      patch({ interaction: interaction('error', operation, targetId, normalizedError(error, active.signal)) })
    } finally {
      release(active)
      if (!waiting) inFlight.delete(key)
    }
  }

  function inputById(inputRequestId: string): LocalInputDecision | null {
    return currentState.inputs.find(item => item.projection.inputRequestId === inputRequestId) ?? null
  }

  async function respondInput(
    inputRequestId: string,
    status: 'provided' | 'cancelled',
    value: InteractiveInputValue | null,
  ): Promise<void> {
    const item = inputById(inputRequestId)
    if (item === null) {
      failure(
        'LOCAL_DECISIONS_INPUT_STALE',
        'Refresh and select a current pending input.',
        CommandName.InputRespond,
        inputRequestId,
      )
      return
    }
    if (
      item.projection.state === 'expired'
      || item.expired
      || expired(item.projection.expiresAt, nowMillis())
    ) {
      failure(
        'LOCAL_DECISIONS_INPUT_EXPIRED',
        'The pending input has expired.',
        CommandName.InputRespond,
        inputRequestId,
      )
      return
    }
    if (item.projection.state !== 'pending') {
      failure(
        'LOCAL_DECISIONS_INPUT_STALE',
        'Refresh and select a current pending input.',
        CommandName.InputRespond,
        inputRequestId,
      )
      return
    }
    if (!bindingMatches(item.projection.binding, options.productSessionId)) {
      failure(
        'LOCAL_DECISIONS_INPUT_BINDING_INVALID',
        'The pending input binding is no longer current.',
        CommandName.InputRespond,
        inputRequestId,
      )
      return
    }
    if (status === 'provided') {
      if (value === null) {
        failure(
          'LOCAL_DECISIONS_INPUT_VALUE_REQUIRED',
          'Enter a response for the current input.',
          CommandName.InputRespond,
          inputRequestId,
        )
        return
      }
      const validation = validateInputValue(item.projection, value)
      if (validation !== null) {
        patch({ interaction: interaction('error', CommandName.InputRespond, inputRequestId, validation) })
        return
      }
    } else if (value !== null) {
      failure(
        'LOCAL_DECISIONS_CANCEL_VALUE_FORBIDDEN',
        'Cancelling an input cannot submit a response value.',
        CommandName.InputRespond,
        inputRequestId,
      )
      return
    }
    const binding = item.projection.binding
    await runCommand(
      CommandName.InputRespond,
      inputRequestId,
      item.projection.revision,
      requestId => ({
        ...requestBase(),
        requestId,
        command: CommandName.InputRespond,
        expectedRevision: item.projection.revision,
        payload: {
          executionJobId: binding.executionJobId,
          inputRequestId: item.projection.inputRequestId,
          productSessionId: binding.productSessionId,
          sessionIdentity: binding.sessionIdentity,
          status,
          value,
          workerSessionId: binding.workerSessionId,
        },
      }),
      response => {
        if (response.result.id !== options.productSessionId) throw clientFailure(
          'LOCAL_DECISIONS_SESSION_BINDING_INVALID',
          'The input response returned another ProductSession.',
        )
        patch({
          session: response.result,
          inputs: Object.freeze(currentState.inputs.filter(candidate => (
            candidate.projection.inputRequestId !== inputRequestId
          ))),
        })
      },
    )
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
      invalidateClientQueryCache(options.client, {
        actor: options.actor,
        scope: options.scope,
        reason: 'manual',
      })
      await load(false, subscriptions.size === 0 ? 'inactive' : 'subscribed')
      if (currentState.status === 'ready' && subscriptions.size === 0 && !closed) subscribeRealtime()
    },
    async provideInput(inputRequestId, value) {
      await respondInput(inputRequestId, 'provided', value)
    },
    async cancelInput(inputRequestId) {
      await respondInput(inputRequestId, 'cancelled', null)
    },
    async decideApproval(approvalId, decision, reason) {
      const item = currentState.approvals.find(candidate => candidate.projection.id === approvalId)
      if (item === undefined) {
        failure(
          'LOCAL_DECISIONS_APPROVAL_STALE',
          'Refresh and select a current pending approval.',
          CommandName.ApprovalDecide,
          approvalId,
        )
        return
      }
      if (
        item.projection.state === 'expired'
        || item.expired
        || expired(item.projection.expiresAt, nowMillis())
      ) {
        failure(
          'LOCAL_DECISIONS_APPROVAL_EXPIRED',
          'The pending approval has expired.',
          CommandName.ApprovalDecide,
          approvalId,
        )
        return
      }
      if (item.projection.state !== 'pending') {
        failure(
          'LOCAL_DECISIONS_APPROVAL_STALE',
          'Refresh and select a current pending approval.',
          CommandName.ApprovalDecide,
          approvalId,
        )
        return
      }
      if (!bindingMatches(item.projection.binding, options.productSessionId)) {
        failure(
          'LOCAL_DECISIONS_APPROVAL_BINDING_INVALID',
          'The pending approval binding is no longer current.',
          CommandName.ApprovalDecide,
          approvalId,
        )
        return
      }
      const explanation = reason.trim()
      if (explanation.length === 0) {
        failure(
          'LOCAL_DECISIONS_APPROVAL_REASON_REQUIRED',
          'Explain this approval decision.',
          CommandName.ApprovalDecide,
          approvalId,
        )
        return
      }
      await runCommand(
        CommandName.ApprovalDecide,
        approvalId,
        item.projection.revision,
        requestId => ({
          ...requestBase(),
          requestId,
          command: CommandName.ApprovalDecide,
          expectedRevision: item.projection.revision,
          payload: {
            approvalId: item.projection.id,
            binding: item.projection.binding,
            decision,
            reason: explanation,
          },
        }),
        response => {
          if (response.result.id !== item.projection.id) throw clientFailure(
            'LOCAL_DECISIONS_APPROVAL_BINDING_INVALID',
            'The approval response returned another approval.',
          )
          patch({
            approvals: Object.freeze(currentState.approvals.filter(candidate => (
              candidate.projection.id !== approvalId
            ))),
          })
        },
      )
    },
    async resolveAttention(attentionItemId, decision, resolution) {
      const item = currentState.attention.find(candidate => (
        candidate.projection.id === attentionItemId
      ))
      if (item === undefined || item.projection.status !== 'open') {
        failure(
          'LOCAL_DECISIONS_ATTENTION_STALE',
          'Refresh and select a current open Attention item.',
          CommandName.DeliveryResolveAttention,
          attentionItemId,
        )
        return
      }
      const explanation = resolution.trim()
      if (explanation.length === 0) {
        failure(
          'LOCAL_DECISIONS_ATTENTION_RESOLUTION_REQUIRED',
          'Explain the Attention decision.',
          CommandName.DeliveryResolveAttention,
          attentionItemId,
        )
        return
      }
      await runCommand(
        CommandName.DeliveryResolveAttention,
        attentionItemId,
        item.deliveryRevision,
        requestId => ({
          ...requestBase(),
          requestId,
          command: CommandName.DeliveryResolveAttention,
          expectedRevision: item.deliveryRevision,
          payload: {
            attentionItemId: item.projection.id,
            deliveryId: item.deliveryId,
            decision,
            resolution: explanation,
            remediation: null,
          },
        }),
        response => {
          if (response.result.deliveryId !== item.deliveryId) throw clientFailure(
            'LOCAL_DECISIONS_DELIVERY_BINDING_INVALID',
            'The Attention response returned another Delivery.',
          )
          patch({
            attention: Object.freeze(currentState.attention.filter(candidate => (
              candidate.projection.id !== attentionItemId
            ))),
          })
        },
      )
    },
    cancelPending() {
      if (closed) return
      generation += 1
      inFlight.clear()
      abortRequests()
      closeSubscriptions()
      const error = new ControlPlaneClientError({
        kind: 'cancelled',
        code: 'REQUEST_CANCELLED',
        message: 'The local decision request was cancelled.',
        requestId: null,
        retryable: false,
      })
      patch({
        status: 'cancelled',
        realtime: 'inactive',
        interaction: interaction('error', null, null, error),
        error,
      })
    },
    reconnect() {
      if (closed) throw clientFailure('LOCAL_DECISIONS_CLOSED', 'The local decisions view is closed.')
      if (subscriptions.size === 0) throw clientFailure(
        'LOCAL_DECISIONS_SUBSCRIPTION_INACTIVE',
        'Local decision events are not active.',
      )
      patch({ realtime: 'reconnecting', error: null })
      for (const subscription of subscriptions) subscription.reconnect()
      void load(false, 'reloading')
    },
    close() {
      if (closed) return
      closed = true
      invalidateClientQueryCache(options.client, {
        actor: options.actor,
        scope: options.scope,
        reason: 'manual',
        discard: true,
      })
      generation += 1
      inFlight.clear()
      abortRequests()
      closeSubscriptions()
      publish({
        status: 'closed',
        realtime: 'closed',
        session: null,
        inputs: Object.freeze([]),
        approvals: Object.freeze([]),
        attention: Object.freeze([]),
        interaction: interaction('idle'),
        error: null,
      })
      listeners.clear()
    },
  }
}
