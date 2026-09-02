// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneSubscription,
} from './control-plane-client.js'
import { invalidateClientQueryCache } from './core/query-cache.js'
import type {
  Actor,
  CommandAcceptedResponse,
  CommandCompletedResponse,
  ControlPlaneWebSocketSubscriptionId,
  DeliveryDetailProjection,
  DeliveryGetResultResponse,
  DeliveryListResultResponse,
  DeliveryProjection,
  OpaqueCursor,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
  Scope,
  WorkerDrainCompletedResponse,
  WorkerEnableCompletedResponse,
  WorkerListResultResponse,
  WorkerProjection,
} from './generated/contracts.js'
import {
  CommandName,
  ControlPlaneWebSocketEventType,
  QueryName,
} from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const PAGE_SIZE = 200
const MAX_PAGES = 10
const WORKER_STATES = Object.freeze(['enabled', 'draining', 'offline'] as const)

export type LocalOperationsStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'refreshing'
  | 'authentication-required'
  | 'authorization-denied'
  | 'cancelled'
  | 'error'
  | 'closed'

export type LocalOperationsRealtimeStatus =
  | 'inactive'
  | 'subscribed'
  | 'reloading'
  | 'reconnecting'
  | 'access-revoked'
  | 'closed'

export type LocalOperationsInteractionStatus = 'idle' | 'submitting' | 'waiting' | 'error'
export type LocalOperationsOperation = 'worker.drain' | 'worker.enable'

export interface LocalOperationsInteractionState {
  readonly status: LocalOperationsInteractionStatus
  readonly operation: LocalOperationsOperation | null
  readonly workerId: WorkerProjection['id'] | null
  readonly error: ControlPlaneClientError | null
}

export type RepositoryGitRisk =
  | 'clear'
  | 'attention-required'
  | 'code-failure'
  | 'infrastructure-failure'
  | 'unknown'

export interface RepositoryDiagnosticsSummary {
  readonly available: boolean
  readonly repositoryIdentity: string | null
  readonly repositoryKind: 'local-git' | 'github' | null
  readonly baselineRevision: string | null
  readonly worktreeState: 'candidate-frozen' | 'no-candidate' | 'not-reported'
  readonly gitRisk: RepositoryGitRisk
  readonly openAttentionCount: number
  readonly pathsHidden: true
}

export type FailureClassification =
  | 'none'
  | 'resource-shortage'
  | 'code-failure'
  | 'infrastructure-failure'
  | 'unknown'

export interface LocalResourceSummary {
  readonly reportedWorkerCount: number
  readonly enabledWorkerCount: number
  readonly reportedCapacitySlots: number
  readonly cpu: 'not-reported'
  readonly memory: 'not-reported'
  readonly disk: 'not-reported'
  readonly cleanup: 'not-reported'
  readonly failureClassification: FailureClassification
}

export interface LocalOperationsViewModelState {
  readonly status: LocalOperationsStatus
  readonly realtime: LocalOperationsRealtimeStatus
  readonly workers: readonly WorkerProjection[]
  readonly repository: RepositoryDiagnosticsSummary
  readonly resources: LocalResourceSummary
  readonly interaction: LocalOperationsInteractionState
  readonly error: ControlPlaneClientError | null
}

export type LocalOperationsListener = (state: LocalOperationsViewModelState) => void

export interface LocalOperationsViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: Scope
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
}

export interface LocalOperationsViewModel {
  readonly state: LocalOperationsViewModelState
  subscribe(listener: LocalOperationsListener): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  drainWorker(workerId: WorkerProjection['id']): Promise<void>
  enableWorker(workerId: WorkerProjection['id']): Promise<void>
  cancelPending(): void
  reconnect(): void
  close(): void
}

interface LocalOperationsCommandResponses {
  readonly [CommandName.WorkerDrain]: WorkerDrainCompletedResponse
  readonly [CommandName.WorkerEnable]: WorkerEnableCompletedResponse
}

function interaction(
  status: LocalOperationsInteractionStatus,
  operation: LocalOperationsOperation | null = null,
  workerId: WorkerProjection['id'] | null = null,
  error: ControlPlaneClientError | null = null,
): LocalOperationsInteractionState {
  return Object.freeze({ status, operation, workerId, error })
}

function emptyRepository(): RepositoryDiagnosticsSummary {
  return Object.freeze({
    available: false,
    repositoryIdentity: null,
    repositoryKind: null,
    baselineRevision: null,
    worktreeState: 'not-reported',
    gitRisk: 'unknown',
    openAttentionCount: 0,
    pathsHidden: true,
  })
}

function emptyResources(): LocalResourceSummary {
  return Object.freeze({
    reportedWorkerCount: 0,
    enabledWorkerCount: 0,
    reportedCapacitySlots: 0,
    cpu: 'not-reported',
    memory: 'not-reported',
    disk: 'not-reported',
    cleanup: 'not-reported',
    failureClassification: 'unknown',
  })
}

function initialState(): LocalOperationsViewModelState {
  return Object.freeze({
    status: 'idle',
    realtime: 'inactive',
    workers: Object.freeze([]),
    repository: emptyRepository(),
    resources: emptyResources(),
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
    message: 'The local operations request was cancelled.',
    requestId: null,
    retryable: false,
    cause: error,
  })
  return clientFailure(
    'LOCAL_OPERATIONS_FAILURE',
    'Local operations could not be updated.',
    error,
  )
}

function statusForError(error: ControlPlaneClientError): LocalOperationsStatus {
  if (error.kind === 'authentication') return 'authentication-required'
  if (error.kind === 'authorization') return 'authorization-denied'
  if (error.kind === 'cancelled') return 'cancelled'
  return 'error'
}

function page(cursor: OpaqueCursor | null) {
  return Object.freeze({ cursor, limit: PAGE_SIZE })
}

function nextCursor(
  response: { readonly page: { readonly hasMore: boolean; readonly nextCursor: OpaqueCursor | null } },
  seen: Set<OpaqueCursor>,
): OpaqueCursor | null {
  if (!response.page.hasMore) {
    if (response.page.nextCursor !== null) throw clientFailure(
      'LOCAL_OPERATIONS_PAGE_INVALID',
      'The final local operations page returned an unexpected cursor.',
    )
    return null
  }
  const next = response.page.nextCursor
  if (next === null || seen.has(next)) throw clientFailure(
    'LOCAL_OPERATIONS_CURSOR_INVALID',
    'The local operations list returned an invalid continuation cursor.',
  )
  seen.add(next)
  return next
}

function expectQuery<Query extends QueryResultResponse['query']>(
  response: QueryResultResponse,
  query: Query,
): Extract<QueryResultResponse, { readonly query: Query }> {
  if (response.query !== query) throw clientFailure(
    'LOCAL_OPERATIONS_QUERY_MISMATCH',
    'The Control Plane returned another local operations query result.',
  )
  return response as Extract<QueryResultResponse, { readonly query: Query }>
}

function safeIdentity(value: string): string {
  const suffix = value.slice(-6)
  return suffix.length === 0 ? 'hidden' : `…${suffix}`
}

function safeRevision(value: string): string {
  return /^[0-9a-f]{7,64}$/u.test(value) ? value.slice(0, 12) : 'Recorded · hidden by policy'
}

function gitRisk(detail: DeliveryDetailProjection): RepositoryGitRisk {
  if (detail.attention.some(item => item.status === 'open' && item.blocking)) {
    return 'attention-required'
  }
  if (detail.verdict?.status === 'fail') return 'code-failure'
  if (detail.verdict?.status === 'infra_error') return 'infrastructure-failure'
  if (detail.verdict?.status === 'pass') return 'clear'
  return 'unknown'
}

function repositorySummary(
  scope: RepositoryScope | null,
  detail: DeliveryDetailProjection | null,
): RepositoryDiagnosticsSummary {
  if (scope === null || detail === null) return emptyRepository()
  return Object.freeze({
    available: true,
    repositoryIdentity: safeIdentity(scope.repositoryId),
    repositoryKind: detail.requirements.repository.kind,
    baselineRevision: safeRevision(detail.requirements.baseRevision),
    worktreeState: detail.currentCandidate === null ? 'no-candidate' : 'candidate-frozen',
    gitRisk: gitRisk(detail),
    openAttentionCount: detail.attention.filter(item => item.status === 'open').length,
    pathsHidden: true,
  })
}

function classifyFailure(
  workers: readonly WorkerProjection[],
  detail: DeliveryDetailProjection | null,
): FailureClassification {
  if (detail?.verdict?.status === 'fail') return 'code-failure'
  if (detail?.verdict?.status === 'infra_error') return 'infrastructure-failure'
  const enabled = workers.filter(worker => worker.state === 'enabled')
  const capacity = enabled.reduce((total, worker) => total + worker.capacity, 0)
  if (workers.length > 0 && (enabled.length === 0 || capacity === 0)) return 'resource-shortage'
  if (detail?.verdict?.status === 'pass') return 'none'
  return 'unknown'
}

function resourceSummary(
  workers: readonly WorkerProjection[],
  detail: DeliveryDetailProjection | null,
): LocalResourceSummary {
  const enabled = workers.filter(worker => worker.state === 'enabled')
  return Object.freeze({
    reportedWorkerCount: workers.length,
    enabledWorkerCount: enabled.length,
    reportedCapacitySlots: enabled.reduce((total, worker) => total + worker.capacity, 0),
    cpu: 'not-reported',
    memory: 'not-reported',
    disk: 'not-reported',
    cleanup: 'not-reported',
    failureClassification: classifyFailure(workers, detail),
  })
}

function resourceSummaryAfterWorker(
  workers: readonly WorkerProjection[],
  previous: FailureClassification,
): LocalResourceSummary {
  const refreshed = resourceSummary(workers, null)
  if (!['none', 'code-failure', 'infrastructure-failure'].includes(previous)) return refreshed
  return Object.freeze({ ...refreshed, failureClassification: previous })
}

function orderedWorkers(workers: readonly WorkerProjection[]): readonly WorkerProjection[] {
  return Object.freeze([...workers].sort((left, right) => left.id.localeCompare(right.id)))
}

/** Build the repository and local Worker surface from canonical Control Plane projections. */
export function createLocalOperationsViewModel(
  options: LocalOperationsViewModelOptions,
): LocalOperationsViewModel {
  const listeners = new Set<LocalOperationsListener>()
  const controllers = new Set<AbortController>()
  const repositoryScope: RepositoryScope | null = options.scope.kind === 'repository'
    ? options.scope
    : null
  let currentState = initialState()
  let realtime: ControlPlaneSubscription | null = null
  let generation = 0
  let closed = false

  function publish(state: LocalOperationsViewModelState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<LocalOperationsViewModelState>): void {
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

  async function workers(signal: AbortSignal): Promise<readonly WorkerProjection[]> {
    const items: WorkerProjection[] = []
    const seen = new Set<OpaqueCursor>()
    let cursor: OpaqueCursor | null = null
    for (let pageIndex = 0; pageIndex < MAX_PAGES; pageIndex += 1) {
      const response: WorkerListResultResponse = expectQuery(await options.client.query({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.WorkerList,
        parameters: { states: WORKER_STATES },
        page: page(cursor),
      }, { signal }), QueryName.WorkerList)
      items.push(...response.result.items)
      cursor = nextCursor(response, seen)
      if (cursor === null) return orderedWorkers(items)
    }
    throw clientFailure(
      'LOCAL_OPERATIONS_PAGE_LIMIT_EXCEEDED',
      'The Worker list exceeded the bounded page limit.',
    )
  }

  async function latestDelivery(signal: AbortSignal): Promise<DeliveryDetailProjection | null> {
    if (repositoryScope === null) return null
    const summaries: DeliveryProjection[] = []
    const seen = new Set<OpaqueCursor>()
    let cursor: OpaqueCursor | null = null
    for (let pageIndex = 0; pageIndex < MAX_PAGES; pageIndex += 1) {
      const response: DeliveryListResultResponse = expectQuery(await options.client.query({
        schemaVersion: SCHEMA_VERSION,
        actor: options.actor,
        scope: repositoryScope,
        requestId: options.nextRequestId(),
        query: QueryName.DeliveryList,
        parameters: { states: [] },
        page: page(cursor),
      }, { signal }), QueryName.DeliveryList)
      summaries.push(...response.result.items)
      cursor = nextCursor(response, seen)
      if (cursor === null) break
      if (pageIndex === MAX_PAGES - 1) throw clientFailure(
        'LOCAL_OPERATIONS_PAGE_LIMIT_EXCEEDED',
        'The Delivery list exceeded the bounded page limit.',
      )
    }
    const latest = [...summaries].sort((left, right) => (
      right.updatedAt.localeCompare(left.updatedAt) || right.deliveryId.localeCompare(left.deliveryId)
    ))[0]
    if (latest === undefined) return null
    const detail: DeliveryGetResultResponse = expectQuery(await options.client.query({
      schemaVersion: SCHEMA_VERSION,
      actor: options.actor,
      scope: repositoryScope,
      requestId: options.nextRequestId(),
      query: QueryName.DeliveryGet,
      parameters: { deliveryId: latest.deliveryId },
      page: { cursor: null, limit: 1 },
    }, { signal }), QueryName.DeliveryGet)
    if (detail.page.hasMore || detail.page.nextCursor !== null) throw clientFailure(
      'LOCAL_OPERATIONS_PAGE_INVALID',
      'The repository detail returned an unexpected page cursor.',
    )
    return detail.result
  }

  async function snapshot(signal: AbortSignal): Promise<{
    readonly workers: readonly WorkerProjection[]
    readonly detail: DeliveryDetailProjection | null
  }> {
    const workerSnapshot = await workers(signal)
    return Object.freeze({ workers: workerSnapshot, detail: await latestDelivery(signal) })
  }

  async function load(replace: boolean, realtimeStatus: LocalOperationsRealtimeStatus): Promise<void> {
    if (closed) throw clientFailure('LOCAL_OPERATIONS_CLOSED', 'The local operations view is closed.')
    generation += 1
    const ownGeneration = generation
    abortRequests()
    const active = controller()
    patch({
      status: replace ? 'loading' : 'refreshing',
      realtime: realtimeStatus,
      ...(replace
        ? { workers: Object.freeze([]), repository: emptyRepository(), resources: emptyResources() }
        : {}),
      interaction: interaction('idle'),
      error: null,
    })
    try {
      const value = await snapshot(active.signal)
      if (!isCurrent(ownGeneration)) return
      publish({
        status: 'ready',
        realtime: realtimeStatus === 'reloading' ? 'subscribed' : realtimeStatus,
        workers: value.workers,
        repository: repositorySummary(repositoryScope, value.detail),
        resources: resourceSummary(value.workers, value.detail),
        interaction: interaction('idle'),
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
        interaction: interaction('error', null, null, normalized),
        error: normalized,
      })
    } finally {
      release(active)
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
      workers: Object.freeze([]),
      repository: emptyRepository(),
      resources: emptyResources(),
      interaction: interaction('error', null, null, error),
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
          message: 'Local operations event authorization is no longer valid.',
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

  function findWorker(workerId: WorkerProjection['id']): WorkerProjection | null {
    return currentState.workers.find(worker => worker.id === workerId) ?? null
  }

  function commandFailure(
    code: string,
    message: string,
    operation: LocalOperationsOperation,
    workerId: WorkerProjection['id'],
  ): void {
    patch({ interaction: interaction('error', operation, workerId, clientFailure(code, message)) })
  }

  async function runCommand<Command extends keyof LocalOperationsCommandResponses>(
    command: Command,
    worker: WorkerProjection,
    request: (requestId: RequestId) => Parameters<ControlPlaneClient['command']>[0],
  ): Promise<void> {
    if (closed) throw clientFailure('LOCAL_OPERATIONS_CLOSED', 'The local operations view is closed.')
    if (
      currentState.interaction.status === 'submitting'
      || currentState.interaction.status === 'waiting'
    ) {
      commandFailure(
        'LOCAL_OPERATIONS_COMMAND_IN_FLIGHT',
        'Wait for the current Worker change to finish.',
        command,
        worker.id,
      )
      return
    }
    const active = controller()
    const ownGeneration = generation
    const requestId = options.nextRequestId()
    patch({ interaction: interaction('submitting', command, worker.id) })
    try {
      const response = await options.client.command(request(requestId), { signal: active.signal })
      if (!isCurrent(ownGeneration)) return
      if (response.requestId !== requestId || response.command !== command) throw clientFailure(
        'LOCAL_OPERATIONS_COMMAND_MISMATCH',
        'The Control Plane returned another Worker command result.',
      )
      if (response.outcome === 'accepted') {
        patch({ interaction: interaction('waiting', command, worker.id) })
        return
      }
      const completed = response as LocalOperationsCommandResponses[Command]
      if (completed.previousRevision !== worker.revision || completed.result.id !== worker.id) {
        throw clientFailure(
          'LOCAL_OPERATIONS_COMMAND_MISMATCH',
          'The Control Plane returned another Worker command result.',
        )
      }
      const updated = orderedWorkers([
        ...currentState.workers.filter(candidate => candidate.id !== worker.id),
        completed.result,
      ])
      patch({
        workers: updated,
        resources: resourceSummaryAfterWorker(
          updated,
          currentState.resources.failureClassification,
        ),
        interaction: interaction('idle'),
        error: null,
      })
    } catch (error) {
      if (!isCurrent(ownGeneration)) return
      patch({ interaction: interaction('error', command, worker.id, normalizedError(error, active.signal)) })
    } finally {
      release(active)
    }
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
      await load(false, realtime === null ? 'inactive' : 'subscribed')
      if (currentState.status === 'ready' && realtime === null && !closed) subscribeRealtime()
    },
    async drainWorker(workerId) {
      const worker = findWorker(workerId)
      if (worker === null) {
        commandFailure(
          'LOCAL_OPERATIONS_WORKER_STALE',
          'Refresh local Workers and select a current Worker.',
          CommandName.WorkerDrain,
          workerId,
        )
        return
      }
      if (worker.state === 'draining') {
        patch({ interaction: interaction('idle'), error: null })
        return
      }
      if (worker.state === 'offline') {
        commandFailure(
          'LOCAL_OPERATIONS_WORKER_OFFLINE',
          'Enable the offline Worker before requesting a drain.',
          CommandName.WorkerDrain,
          worker.id,
        )
        return
      }
      await runCommand(
        CommandName.WorkerDrain,
        worker,
        requestId => ({
          ...requestBase(),
          requestId,
          command: CommandName.WorkerDrain,
          expectedRevision: worker.revision,
          payload: {
            workerId: worker.id,
            reason: 'Requested from the local operations page.',
          },
        }),
      )
    },
    async enableWorker(workerId) {
      const worker = findWorker(workerId)
      if (worker === null) {
        commandFailure(
          'LOCAL_OPERATIONS_WORKER_STALE',
          'Refresh local Workers and select a current Worker.',
          CommandName.WorkerEnable,
          workerId,
        )
        return
      }
      if (worker.state === 'enabled') {
        patch({ interaction: interaction('idle'), error: null })
        return
      }
      await runCommand(
        CommandName.WorkerEnable,
        worker,
        requestId => ({
          ...requestBase(),
          requestId,
          command: CommandName.WorkerEnable,
          expectedRevision: worker.revision,
          payload: { workerId: worker.id },
        }),
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
        message: 'The local operations request was cancelled.',
        requestId: null,
        retryable: false,
      })
      publish({
        ...currentState,
        status: 'cancelled',
        realtime: 'inactive',
        interaction: interaction('error', null, null, error),
        error,
      })
    },
    reconnect() {
      if (closed) throw clientFailure('LOCAL_OPERATIONS_CLOSED', 'The local operations view is closed.')
      if (realtime === null) throw clientFailure(
        'LOCAL_OPERATIONS_SUBSCRIPTION_INACTIVE',
        'Local operations events are not active.',
      )
      patch({ realtime: 'reconnecting', error: null })
      realtime.reconnect()
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
      abortRequests()
      realtime?.close()
      realtime = null
      publish({
        status: 'closed',
        realtime: 'closed',
        workers: Object.freeze([]),
        repository: emptyRepository(),
        resources: emptyResources(),
        interaction: interaction('idle'),
        error: null,
      })
      listeners.clear()
    },
  }
}
