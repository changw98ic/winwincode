import { requestFetch as sendHttp, withResponseFailure } from '@winwincode/network-request'

import {
  ControlPlaneClientError,
  controlPlaneLoginFailure,
  createControlPlaneClient as createNeutralControlPlaneClient,
  parseControlPlaneServerUrl,
  type ControlPlaneClient as NeutralControlPlaneClient,
  type ControlPlaneClientOptions as NeutralControlPlaneClientOptions,
  type ControlPlaneClientErrorKind,
  type ControlPlaneClientTransport,
  type ControlPlaneHttpResponse,
  type ControlPlaneGeneratedTransport,
  type ControlPlaneInitializationStatus,
  type ControlPlaneLoginFailure,
  type ControlPlanePasswordChange,
  type ControlPlanePasswordCredentials,
  type ControlPlaneRequestOptions,
  type ControlPlaneServerLocation,
  type ControlPlaneSubscribeOptions as NeutralControlPlaneSubscribeOptions,
  type ControlPlaneSubscription,
  type ControlPlaneTransportFetch,
} from '@winwincode/control-plane-client'
import {
  ControlPlaneClientError as GeneratedControlPlaneClientError,
  createControlPlaneHttpClient,
  createControlPlaneWebSocketClient,
  matchesCanonicalSchema,
  type ControlPlaneWebSocketClientOptions,
} from './generated/control-plane-client.js'
import { ControlPlaneWebSocketSubscribeOrigin } from './generated/contracts.js'
import type {
  AuthSessionResponse,
  CommandAcceptedResponse,
  CommandCompletedResponse,
  CommandRequest,
  ControlPlaneWebSocketAuthorizationRevokedFrame,
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketResetRequiredFrame,
  ControlPlaneWebSocketSubscription,
  ControlPlaneWebSocketSubscriptionId,
  ControlPlaneWebSocketSubscribeStartAt,
  ErrorDetails,
  EventReadCursor,
  Actor,
  PageRequest,
  RepositoryScope,
  ProductSessionId,
  WorkItemId,
  DeliveryId,
  DeliveryDetailProjection,
  WorkItemsCreateCommand,
  WorkItemsCreateItem,
  WorkRunStartCommand,
  WorkRunCancelCommand,
  WorkRunAggregateProjection,
  WorkRunId,
  Sha256Digest,
  StrongFlowReadCursor,
  QueryRequest,
  QueryResultResponse,
  RequestId,
} from './generated/contracts.js'
import type { DeviceProviderView, DeviceRepositoryRegistrationReceipt, DeviceConfigurationEnvelope } from './generated/contracts.js'

export interface ControlPlaneRepositoryRegistration {
  readDevice(clientId: string, signal: AbortSignal): Promise<DeviceProviderView>
  submit(clientId: string, envelope: DeviceConfigurationEnvelope, signal: AbortSignal): Promise<void>
  receipt(clientId: string, requestId: string, signal: AbortSignal): Promise<{ online: boolean; receipt: DeviceRepositoryRegistrationReceipt | null }>
}

export type ControlPlaneManagedAppMode = 'live' | 'frozen-candidate'

export interface ControlPlaneManagedAppTemplate {
  readonly schemaVersion: 'winwincode/managed-app-template-v1'
  readonly mode: ControlPlaneManagedAppMode
  readonly cwd: string
  readonly argv: readonly string[]
  readonly healthCheck: { readonly path: string; readonly timeoutMs: number }
  readonly listenPort: number
}

export interface ControlPlaneManagedAppTemplateSnapshot {
  readonly revision: number | null
  readonly template: ControlPlaneManagedAppTemplate | null
}

export interface ControlPlaneManagedAppTemplatePort {
  load(repositoryBindingId: string, signal?: AbortSignal): Promise<ControlPlaneManagedAppTemplateSnapshot>
  save(repositoryBindingId: string, template: ControlPlaneManagedAppTemplate, signal?: AbortSignal): Promise<number>
}

export function createControlPlaneManagedAppTemplatePort(options: {
  readonly serverUrl: string
  readonly transport: ControlPlaneClientTransport
}): ControlPlaneManagedAppTemplatePort {
  const location = parseControlPlaneServerUrl(options.serverUrl)
  const transportFetch = options.transport.fetch
  const pathFor = (repositoryBindingId: string) =>
    `${location.serverUrl}/api/v1/repositories/${encodeURIComponent(repositoryBindingId)}/managed-app-template`
  async function request(repositoryBindingId: string, method: 'GET' | 'POST', signal: AbortSignal | undefined, template?: ControlPlaneManagedAppTemplate): Promise<unknown> {
    if (transportFetch === undefined) throw new Error('项目运行设置服务不可用。')
    const response = await sendHttp(transportFetch, pathFor(repositoryBindingId), {
      method,
      headers: {},
      credentials: 'include',
      cache: 'no-store',
      redirect: 'error',
      referrerPolicy: 'no-referrer',
      ...(signal === undefined ? {} : { signal }),
      ...(template === undefined ? {} : { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(template) }),
    })
    const source = await response.text()
    if (!response.ok) throw withResponseFailure(new Error(response.status === 401 ? '请重新登录。' : response.status === 403 ? '没有管理该项目的权限。' : '项目运行设置未保存，请稍后重试。'), response)
    try { return JSON.parse(source) as unknown } catch { throw new Error('项目运行设置返回了无效结果。') }
  }
  function parseSnapshot(value: unknown): ControlPlaneManagedAppTemplateSnapshot {
    if (!isRecord(value)
      || value.schemaVersion !== CONTROL_PLANE_SCHEMA_VERSION
      || (value.revision !== null && (typeof value.revision !== 'number' || !Number.isInteger(value.revision) || value.revision < 1))
      || (value.template !== null && !isManagedAppTemplate(value.template))) {
      throw new Error('项目运行设置返回了无效结果。')
    }
    return Object.freeze({
      revision: value.revision as number | null,
      template: value.template as ControlPlaneManagedAppTemplate | null,
    })
  }
  return {
    async load(repositoryBindingId, signal) {
      return parseSnapshot(await request(repositoryBindingId, 'GET', signal))
    },
    async save(repositoryBindingId, template, signal) {
      const value = await request(repositoryBindingId, 'POST', signal, template)
      const revision = isRecord(value) ? value.revision : undefined
      if (!isRecord(value) || value.schemaVersion !== CONTROL_PLANE_SCHEMA_VERSION
        || typeof revision !== 'number' || !Number.isInteger(revision) || revision < 1) throw new Error('项目运行设置返回了无效结果。')
      return revision
    },
  }
}

function isManagedAppTemplate(value: unknown): value is ControlPlaneManagedAppTemplate {
  const healthCheck = isRecord(value) ? value.healthCheck : null
  if (!isRecord(value) || value.schemaVersion !== 'winwincode/managed-app-template-v1'
    || 'env' in value
    || (value.mode !== 'live' && value.mode !== 'frozen-candidate')
    || typeof value.cwd !== 'string' || value.cwd.length === 0
    || !Array.isArray(value.argv) || value.argv.length === 0 || value.argv.some(item => typeof item !== 'string' || item.length === 0)
    || !isRecord(healthCheck) || typeof healthCheck.path !== 'string' || healthCheck.path.length === 0
    || typeof healthCheck.timeoutMs !== 'number' || !Number.isInteger(healthCheck.timeoutMs) || healthCheck.timeoutMs < 1
    || typeof value.listenPort !== 'number' || !Number.isInteger(value.listenPort) || value.listenPort < 1 || value.listenPort > 65535) return false
  return true
}

export function createControlPlaneRepositoryRegistration(options: {
  readonly serverUrl: string
  readonly transport: ControlPlaneClientTransport
}): ControlPlaneRepositoryRegistration {
  const location = parseControlPlaneServerUrl(options.serverUrl)
  const transportFetch = options.transport.fetch
  async function request(path: string, signal: AbortSignal, body?: DeviceConfigurationEnvelope): Promise<unknown> {
    if (transportFetch === undefined) throw new Error('项目接入服务不可用。')
    const response = await sendHttp(transportFetch, `${location.serverUrl}${path}`, {
      method: body === undefined ? 'GET' : 'POST', credentials: 'include', cache: 'no-store', redirect: 'error', signal,
      headers: { 'Content-Type': 'application/json' }, ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    })
    if (!response.ok) throw withResponseFailure(new Error(response.status === 401 ? '请重新登录。' : response.status === 403 ? '没有管理该设备的权限。' : '设备请求未完成，请检查连接后重试。'), response)
    return JSON.parse(await response.text()) as unknown
  }
  return {
    async readDevice(clientId, signal) {
      const result = await request(`/api/v1/clients/${encodeURIComponent(clientId)}/providers`, signal)
      if (!matchesCanonicalSchema('DeviceProviderView', result)) throw new Error('设备配置读取失败。')
      return result as DeviceProviderView
    },
    async submit(clientId, envelope, signal) {
      await request(`/api/v1/clients/${encodeURIComponent(clientId)}/repositories`, signal, envelope)
    },
    async receipt(clientId, requestId, signal) {
      const result = await request(`/api/v1/clients/${encodeURIComponent(clientId)}/repositories/receipts/${encodeURIComponent(requestId)}`, signal)
      if (!isRecord(result) || typeof result.online !== 'boolean'
        || (result.receipt !== null && !matchesCanonicalSchema('DeviceRepositoryRegistrationReceipt', result.receipt))) throw new Error('设备返回了无效的项目结果。')
      const receipt = result.receipt as DeviceRepositoryRegistrationReceipt | null
      if (receipt !== null && receipt.requestId !== requestId) throw new Error('设备返回了其他请求的结果。')
      if (receipt !== null && (receipt.outcome === 'registered') !== (receipt.repositoryBindingId !== null)) throw new Error('项目结果缺少对应的仓库身份。')
      return { online: result.online, receipt }
    },
  }
}

export {
  ControlPlaneClientError,
  controlPlaneLoginFailure,
  parseControlPlaneServerUrl,
}
export type {
  AuthSessionResponse,
  CommandAcceptedResponse,
  CommandCompletedResponse,
  CommandRequest,
  ControlPlaneRequestOptions,
  ControlPlaneServerLocation,
  ControlPlaneClientTransport,
  ControlPlaneClientErrorKind,
  ControlPlaneHttpResponse,
  ControlPlaneInitializationStatus,
  ControlPlaneLoginFailure,
  ControlPlanePasswordChange,
  ControlPlanePasswordCredentials,
  ControlPlaneSubscription,
  ControlPlaneTransportFetch,
  ControlPlaneWebSocketAuthorizationRevokedFrame,
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketSubscription,
  ControlPlaneWebSocketSubscriptionId,
  ControlPlaneWebSocketSubscribeStartAt,
  ErrorDetails,
  EventReadCursor,
  QueryRequest,
  QueryResultResponse,
  RequestId,
}

export type ControlPlaneClient = NeutralControlPlaneClient<
  AuthSessionResponse,
  CommandRequest,
  CommandAcceptedResponse | CommandCompletedResponse,
  QueryRequest,
  QueryResultResponse,
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketResetRequiredFrame,
  ControlPlaneWebSocketAuthorizationRevokedFrame,
  ControlPlaneWebSocketSubscription,
  ControlPlaneWebSocketSubscriptionId,
  ControlPlaneWebSocketSubscribeStartAt,
  EventReadCursor
>

export type ControlPlaneSession = AuthSessionResponse

export type ControlPlaneSubscribeOptions = NeutralControlPlaneSubscribeOptions<
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketResetRequiredFrame,
  ControlPlaneWebSocketAuthorizationRevokedFrame,
  ControlPlaneWebSocketSubscription,
  ControlPlaneWebSocketSubscriptionId,
  ControlPlaneWebSocketSubscribeStartAt,
  EventReadCursor
>

export type ControlPlaneClientOptions = Omit<NeutralControlPlaneClientOptions<
  AuthSessionResponse,
  CommandRequest,
  CommandAcceptedResponse | CommandCompletedResponse,
  QueryRequest,
  QueryResultResponse,
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketResetRequiredFrame,
  ControlPlaneWebSocketAuthorizationRevokedFrame,
  ControlPlaneWebSocketSubscription,
  ControlPlaneWebSocketSubscriptionId,
  ControlPlaneWebSocketSubscribeStartAt,
  EventReadCursor
>, 'generated'>

const generatedTransport = Object.freeze<ControlPlaneGeneratedTransport<
  AuthSessionResponse,
  CommandRequest,
  CommandAcceptedResponse | CommandCompletedResponse,
  QueryRequest,
  QueryResultResponse,
  ControlPlaneWebSocketEventFrame,
  ControlPlaneWebSocketResetRequiredFrame,
  ControlPlaneWebSocketAuthorizationRevokedFrame,
  ControlPlaneWebSocketSubscription,
  ControlPlaneWebSocketSubscriptionId,
  ControlPlaneWebSocketSubscribeStartAt,
  EventReadCursor
>>({
  latestSubscriptionStart: ControlPlaneWebSocketSubscribeOrigin.Latest,
  createError(fields) {
    return new GeneratedControlPlaneClientError({
      ...fields,
      details: fields.details,
    })
  },
  errorFields(error) {
    if (!(error instanceof GeneratedControlPlaneClientError)) return null
    return {
      code: error.code,
      message: error.message,
      requestId: error.requestId,
      retryable: error.retryable,
      details: error.details,
    }
  },
  matchesAuthSession(value): value is AuthSessionResponse {
    return matchesCanonicalSchema('AuthSessionResponse', value)
  },
  createHttpClient(options) {
    const client = createControlPlaneHttpClient({
      baseUrl: options.baseUrl,
      fetch: (input, init) => (
        Reflect.apply(options.fetch, undefined, [input, {
          method: init.method,
          headers: init.headers,
          body: init.body,
          credentials: 'include',
          ...(init.signal === undefined ? {} : { signal: init.signal }),
        }])
      ),
      maxNetworkRetries: options.maxNetworkRetries,
      ...(options.waitBeforeRetry === undefined ? {} : { waitBeforeRetry: options.waitBeforeRetry }),
    })
    return {
      submitCommand(command) {
        return client.submitCommand(command)
      },
      submitQuery(query) {
        return client.submitQuery(query)
      },
    }
  },
  createWebSocketClient(options) {
    const onResetRequired = options.onResetRequired
    const socketOptions: ControlPlaneWebSocketClientOptions = {
      baseUrl: options.baseUrl,
      ...(options.createSocket === undefined ? {} : { createSocket: options.createSocket }),
      ...(options.reconnectDelayMillis === undefined ? {} : { reconnectDelayMillis: options.reconnectDelayMillis }),
      ...(options.onEventQueued === undefined ? {} : {
        onEventQueued: event => { options.onEventQueued?.(event) },
      }),
      onEvent: event => options.onEvent(event),
      ...(onResetRequired === undefined ? {} : {
        async onResetRequired(frame) {
          return onResetRequired(frame)
        },
      }),
      ...(options.onAuthorizationRevoked === undefined ? {} : {
        onAuthorizationRevoked: frame => options.onAuthorizationRevoked?.(frame),
      }),
      ...(options.onError === undefined ? {} : { onError: options.onError }),
    }
    const client = createControlPlaneWebSocketClient(socketOptions)
    return {
      get cursor() { return client.cursor },
      subscribe(subscriptionId, subscription, startAt) {
        client.subscribe(
          subscriptionId,
          subscription,
          startAt,
        )
      },
      resume() { client.resume() },
      reconnect() { client.reconnect() },
      close() { client.close() },
    }
  },
})

export function createControlPlaneClient(options: ControlPlaneClientOptions): ControlPlaneClient {
  return createNeutralControlPlaneClient({
    ...options,
    generated: generatedTransport,
  })
}

export interface WorkRunAggregateQueryInput {
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly requestId: RequestId
  readonly workItemId: WorkItemId | null
  readonly deliveryId: DeliveryId
  readonly atCursor?: import('./generated/contracts.js').StrongFlowReadCursor | null
  readonly workRunId?: WorkRunId | null
  readonly page?: PageRequest
}

export interface WorkItemsCreateInput {
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly requestId: RequestId
  readonly deliveryId: DeliveryId
  readonly expectedRevision: number
  readonly contractRevision: number
  readonly items: readonly WorkItemsCreateItem[]
}

export interface DeliveryCancelRequest {
  readonly deliveryId: DeliveryId
  readonly workRunId: WorkRunId
  readonly expectedRevision: number
  readonly requestId: RequestId
}

export function workRunCancelCommand(
  input: DeliveryCancelRequest,
  context: { readonly actor: Actor; readonly scope: RepositoryScope },
): WorkRunCancelCommand {
  return {
    schemaVersion: 'winwincode/v1',
    command: 'workrun.cancel',
    actor: context.actor,
    scope: context.scope,
    requestId: input.requestId,
    expectedRevision: input.expectedRevision,
    payload: {
      deliveryId: input.deliveryId,
      workRunId: input.workRunId,
    },
  }
}


export function workItemsCreateCommand(
  input: WorkItemsCreateInput,
): WorkItemsCreateCommand {
  return {
    schemaVersion: 'winwincode/v1',
    command: 'workitems.create',
    actor: input.actor,
    scope: input.scope,
    requestId: input.requestId,
    expectedRevision: input.expectedRevision,
    payload: {
      deliveryId: input.deliveryId,
      expectedRevision: input.expectedRevision,
      contractRevision: input.contractRevision,
      items: input.items,
    },
  }
}

export async function queryWorkRunAggregate(
  client: ControlPlaneClient,
  input: WorkRunAggregateQueryInput,
  requestOptions?: ControlPlaneRequestOptions,
): Promise<WorkRunAggregateProjection> {
  const response = await client.query({
    schemaVersion: 'winwincode/v1',
    requestId: input.requestId,
    actor: input.actor,
    scope: input.scope,
    query: 'workrun.get',
    parameters: {
      workItemId: input.workItemId,
      deliveryId: input.deliveryId,
      atCursor: input.atCursor ?? null,
      workRunId: input.workRunId ?? null,
    },
    page: input.page ?? { limit: 100, cursor: null },
  }, requestOptions)
  if (response.query !== 'workrun.get' || !matchesCanonicalSchema('WorkRunAggregateProjection', response.result)
    || response.result.readCursor.deliveryId !== input.deliveryId
    || response.result.readCursor.eventCursor.stream.deliveryId !== input.deliveryId
    || (['kind', 'organizationId', 'workspaceId', 'projectId', 'repositoryId'] as const).some(key =>
      response.result.readCursor.scope[key] !== input.scope[key]
      || response.result.readCursor.eventCursor.scope[key] !== input.scope[key])
    || (input.workItemId !== null && !response.result.items.some(item => item.id === input.workItemId))
    || response.result.items.some(item => item.workContractId !== response.result.contract.id)
    || response.result.runs.some(run => run.workContractId !== response.result.contract.id
      || !response.result.items.some(item => item.id === run.workItemId))
    || response.result.deviceBindings.some(binding =>
        !response.result.runs.some(run => run.id === binding.workRunId))
    || response.result.managedAppRunConfigs.some(config =>
        !response.result.runs.some(run => run.id === config.runId))
    || new Set(response.result.deviceBindings.map(binding => binding.workRunId)).size
      !== response.result.deviceBindings.length
    || (input.workRunId !== undefined && input.workRunId !== null
      && !response.result.runs.some(run => run.id === input.workRunId && (input.workItemId === null || run.workItemId === input.workItemId)))
    || (input.atCursor !== undefined && input.atCursor !== null
      && ((['token', 'deliveryId', 'deliveryRevision', 'runtimeLedgerRevision', 'runtimeAcceptedSequence', 'publicationRevision'] as const)
        .some(key => response.result.readCursor[key] !== input.atCursor?.[key])
        || (['kind', 'organizationId', 'workspaceId', 'projectId', 'repositoryId'] as const).some(key =>
          response.result.readCursor.scope[key] !== input.atCursor?.scope[key]
          || response.result.readCursor.eventCursor.scope[key] !== input.atCursor?.eventCursor.scope[key])
        || response.result.readCursor.eventCursor.eventId !== input.atCursor.eventCursor.eventId
        || response.result.readCursor.eventCursor.sequence !== input.atCursor.eventCursor.sequence
        || response.result.readCursor.eventCursor.stream.kind !== input.atCursor.eventCursor.stream.kind
        || response.result.readCursor.eventCursor.stream.deliveryId !== input.atCursor.eventCursor.stream.deliveryId))) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'INVALID_WORKRUN_PROJECTION',
      message: 'The Control Plane returned an invalid WorkRun projection.',
      requestId: input.requestId,
      retryable: false,
    })
  }
  return response.result
}

export async function queryDeliveryDetail(
  client: ControlPlaneClient,
  input: Pick<WorkRunAggregateQueryInput, 'actor' | 'scope' | 'requestId' | 'deliveryId'>,
): Promise<DeliveryDetailProjection> {
  const response = await client.query({
    schemaVersion: 'winwincode/v1',
    requestId: input.requestId,
    actor: input.actor,
    scope: input.scope,
    query: 'delivery.get',
    parameters: { deliveryId: input.deliveryId },
    page: { limit: 100, cursor: null },
  })
  if (response.query !== 'delivery.get'
    || !matchesCanonicalSchema('DeliveryDetailProjection', response.result)
    || response.result.deliveryId !== input.deliveryId) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'INVALID_DELIVERY_PROJECTION',
      message: 'The Control Plane returned an invalid Delivery projection.',
      requestId: input.requestId,
      retryable: false,
    })
  }
  return response.result
}

const CONTROL_PLANE_SCHEMA_VERSION = 'winwincode/v1'
const RFC3339_INSTANT = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z$/u

function isRecord(value: unknown): value is Readonly<Record<string, unknown>> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function signalIsAborted(signal: AbortSignal | undefined): boolean {
  return signal?.aborted ?? false
}

function cancelledError(requestId: RequestId | null): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'cancelled',
    code: 'REQUEST_CANCELLED',
    message: 'The Control Plane operation was cancelled.',
    requestId,
    retryable: false,
  })
}

function versionCode(code: string): boolean {
  return code === 'SCHEMA_VERSION_MISMATCH'
    || code === 'PROTOCOL_VERSION_UNSUPPORTED'
    || code === 'VERSION_MISMATCH'
}

function accessKind(code: string): ControlPlaneClientErrorKind | null {
  if (code === 'AUTHENTICATION_REQUIRED') return 'authentication'
  if (code === 'PERMISSION_DENIED') return 'authorization'
  return null
}

function versionDetails(details: ErrorDetails): boolean {
  return Reflect.get(details, 'reason') === 'CLIENT_UPGRADE_REQUIRED'
}

const REPOSITORY_DIRECTORY_LIST_PATH = '/api/v1/repositories'

export type ControlPlaneRepositoryDirtyState = 'clean' | 'dirty'

export type ControlPlaneRepositoryAvailability =
  | 'available'
  | 'dirty'
  | 'unavailable'
  | 'moved'
  | 'invalid_git'
  | 'permission_denied'
  | 'scan_failed'

export interface ControlPlaneRepositoryListInput {
  readonly clientId: string
}

export interface ControlPlaneRepositorySummary {
  readonly repositoryBindingId: string
  readonly displayName: string
  readonly defaultBranch: string
  readonly headCommit: string
  readonly dirtyState: ControlPlaneRepositoryDirtyState
  readonly availability: ControlPlaneRepositoryAvailability
}

const REPOSITORY_DIRTY_STATE_VALUES: readonly string[] = Object.freeze(['clean', 'dirty'])
const REPOSITORY_AVAILABILITY_VALUES: readonly string[] = Object.freeze([
  'available',
  'dirty',
  'unavailable',
  'moved',
  'invalid_git',
  'permission_denied',
  'scan_failed',
])

function invalidRepositoryListError(): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'INVALID_REPOSITORY_LIST_RESPONSE',
    message: 'The Control Plane server returned an invalid repository list.',
    requestId: null,
    retryable: false,
  })
}

function assertRepositoryListInput(clientId: string): void {
  if (!/^\d+$/u.test(clientId)) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'REPOSITORY_LIST_INPUT_INVALID',
      message: 'Select a Client to list its repositories.',
      requestId: null,
      retryable: false,
    })
  }
}

function repositorySummaryValue(value: unknown): ControlPlaneRepositorySummary {
  if (!isRecord(value)) throw invalidRepositoryListError()
  const dirtyState = value.dirtyState
  const availability = value.availability
  if (
    typeof value.repositoryBindingId !== 'string'
    || value.repositoryBindingId.length === 0
    || typeof value.displayName !== 'string'
    || value.displayName.length === 0
    || typeof value.defaultBranch !== 'string'
    || value.defaultBranch.length === 0
    || typeof value.headCommit !== 'string'
    || value.headCommit.length === 0
    || typeof dirtyState !== 'string'
    || !REPOSITORY_DIRTY_STATE_VALUES.includes(dirtyState)
    || typeof availability !== 'string'
    || !REPOSITORY_AVAILABILITY_VALUES.includes(availability)
  ) {
    throw invalidRepositoryListError()
  }
  return Object.freeze({
    repositoryBindingId: value.repositoryBindingId,
    displayName: value.displayName,
    defaultBranch: value.defaultBranch,
    headCommit: value.headCommit,
    dirtyState: dirtyState as ControlPlaneRepositoryDirtyState,
    availability: availability as ControlPlaneRepositoryAvailability,
  })
}

function repositoryListResponse(source: string): readonly ControlPlaneRepositorySummary[] {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  if (
    isRecord(value)
    && typeof value.schemaVersion === 'string'
    && value.schemaVersion !== CONTROL_PLANE_SCHEMA_VERSION
  ) {
    throw new ControlPlaneClientError({
      kind: 'version',
      code: 'SCHEMA_VERSION_MISMATCH',
      message: `The Control Plane server must use ${CONTROL_PLANE_SCHEMA_VERSION}.`,
      requestId: null,
      retryable: false,
    })
  }
  if (!isRecord(value) || !Array.isArray(value.repositories)) throw invalidRepositoryListError()
  return Object.freeze(value.repositories.map(repositorySummaryValue))
}

async function repositoriesListRequest(
  location: ControlPlaneServerLocation,
  transportFetch: ControlPlaneTransportFetch | undefined,
  clientId: string,
  signal: AbortSignal | undefined,
): Promise<readonly ControlPlaneRepositorySummary[]> {
  if (signalIsAborted(signal)) throw cancelledError(null)
  if (transportFetch === undefined) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'TRANSPORT_UNAVAILABLE',
      message: 'The browser HTTP transport is unavailable.',
      requestId: null,
      retryable: false,
    })
  }
  try {
    const response = await sendHttp(transportFetch,
      `${location.serverUrl}${REPOSITORY_DIRECTORY_LIST_PATH}`
        + `?clientId=${encodeURIComponent(clientId)}`,
      {
        method: 'GET',
        headers: {},
        redirect: 'error',
        cache: 'no-store',
        referrerPolicy: 'no-referrer',
        credentials: 'include',
        ...(signal === undefined ? {} : { signal }),
      },
    )
    const source = await response.text()
    if (!response.ok) throw withResponseFailure(clientDirectoryBoundaryError(response.status, source), response)
    if (response.status !== 200) throw invalidRepositoryListError()
    return repositoryListResponse(source)
  } catch (error) {
    if (signalIsAborted(signal)) throw cancelledError(null)
    if (error instanceof ControlPlaneClientError) throw error
    throw new ControlPlaneClientError({
      kind: 'network',
      code: 'NETWORK_ERROR',
      message: 'The Control Plane server could not be reached.',
      requestId: null,
      retryable: true,
      cause: error,
    })
  }
}

const CLIENT_DIRECTORY_LIST_PATH = '/api/v1/clients'
const CLIENT_DIRECTORY_CONNECT_PATH = '/api/v1/clients/connections'

export type ControlPlaneDevicePresence = 'online' | 'offline' | 'locked'

export type ControlPlaneDeviceOccupancy =
  | 'available'
  | 'occupied-by-me'
  | 'occupied-by-other'
  | 'draining'
  | 'recovery-pending'

export interface ControlPlaneDeviceSummary {
  readonly clientId: string
  readonly displayName: string
  readonly presence: ControlPlaneDevicePresence
  readonly occupancy: ControlPlaneDeviceOccupancy
  readonly capacityUsed: number
  readonly capacityTotal: number
  readonly lastHeartbeatAt: string
  readonly version: string
  readonly recoveryDeadlineAt?: string | null
}

export interface ControlPlaneClientConnectInput {
  readonly clientId: string
  readonly connectionCode: string
}

export type ControlPlaneClientAddFailure =
  | 'id-not-found'
  | 'client-offline'
  | 'code-invalid'
  | 'code-expired'
  | 'new-connections-forbidden'
  | 'client-locked'
  | 'rate-limited'
  | 'unavailable'

const CLIENT_ADD_FAILURE_CODES: Readonly<Record<string, ControlPlaneClientAddFailure>> =
  Object.freeze({
    CLIENT_NOT_FOUND: 'id-not-found',
    CLIENT_OFFLINE: 'client-offline',
    CONNECT_CODE_INVALID: 'code-invalid',
    CONNECT_CODE_EXPIRED: 'code-expired',
    CLIENT_CONNECTIONS_FORBIDDEN: 'new-connections-forbidden',
    CLIENT_LOCKED: 'client-locked',
    RATE_LIMITED: 'rate-limited',
  })

const DEVICE_PRESENCE_VALUES: readonly string[] = Object.freeze([
  'online',
  'offline',
  'locked',
])
const DEVICE_OCCUPANCY_VALUES: readonly string[] = Object.freeze([
  'available',
  'occupied-by-me',
  'occupied-by-other',
  'draining',
  'recovery-pending',
])

export function controlPlaneClientAddFailure(error: unknown): ControlPlaneClientAddFailure {
  if (error instanceof ControlPlaneClientError) {
    const failure = CLIENT_ADD_FAILURE_CODES[error.code]
    if (failure !== undefined) return failure
  }
  return 'unavailable'
}

function assertClientConnectInput(input: ControlPlaneClientConnectInput): void {
  const clientId = typeof input?.clientId === 'string' ? input.clientId : ''
  const connectionCode = typeof input?.connectionCode === 'string' ? input.connectionCode : ''
  const clientIdDigits = clientId.replace(/\D+/gu, '')
  const codeDigits = connectionCode.replace(/\D+/gu, '')
  if (!/^\d{9,12}$/u.test(clientIdDigits)) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'CLIENT_CONNECT_ID_INVALID',
      message: 'Enter the 9-12 digit Client ID shown on the device.',
      requestId: null,
      retryable: false,
    })
  }
  if (!/^\d{8}$/u.test(codeDigits)) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'CLIENT_CONNECT_CODE_INVALID',
      message: 'Enter the 8-digit connection code shown on the device.',
      requestId: null,
      retryable: false,
    })
  }
}

function clientDirectoryBoundaryError(
  status: number,
  source: string,
): ControlPlaneClientError {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  const error = isRecord(value) && isRecord(value.error) ? value.error : null
  const code = error !== null && typeof error.code === 'string'
    ? error.code
    : 'CLIENT_DIRECTORY_FAILED'
  const kind: ControlPlaneClientErrorKind = accessKind(code)
    ?? (code === 'RATE_LIMITED' || code === 'TOO_MANY_REQUESTS'
      ? 'server'
      : (versionCode(code)
        ? 'version'
        : (status >= 500 ? 'server' : 'protocol')))
  return new ControlPlaneClientError({
    kind,
    code,
    message: error !== null && typeof error.message === 'string'
      ? error.message
      : 'The Client directory request failed.',
    requestId: isRecord(value) && typeof value.requestId === 'string'
      ? value.requestId as RequestId
      : null,
    retryable: error !== null && error.retryable === true,
  })
}

function invalidDeviceListError(): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'INVALID_CLIENT_DIRECTORY_RESPONSE',
    message: 'The Control Plane server returned an invalid device list.',
    requestId: null,
    retryable: false,
  })
}

function deviceSummaryValue(value: unknown): ControlPlaneDeviceSummary {
  if (!isRecord(value)) throw invalidDeviceListError()
  const presence = value.presence
  const occupancy = value.occupancy
  const capacityUsed = value.capacityUsed
  const capacityTotal = value.capacityTotal
  const lastHeartbeatAt = value.lastHeartbeatAt
  if (
    typeof value.clientId !== 'string'
    || value.clientId.length === 0
    || typeof value.displayName !== 'string'
    || typeof presence !== 'string'
    || !DEVICE_PRESENCE_VALUES.includes(presence)
    || typeof occupancy !== 'string'
    || !DEVICE_OCCUPANCY_VALUES.includes(occupancy)
    || typeof capacityUsed !== 'number'
    || !Number.isInteger(capacityUsed)
    || capacityUsed < 0
    || typeof capacityTotal !== 'number'
    || !Number.isInteger(capacityTotal)
    || capacityTotal < capacityUsed
    || typeof lastHeartbeatAt !== 'string'
    || !RFC3339_INSTANT.test(lastHeartbeatAt)
    || Number.isNaN(Date.parse(lastHeartbeatAt))
    || typeof value.version !== 'string'
    || value.version.length === 0
  ) {
    throw invalidDeviceListError()
  }
  const recoveryDeadlineAt = value.recoveryDeadlineAt === undefined
    ? null
    : value.recoveryDeadlineAt
  if (
    recoveryDeadlineAt !== null
    && (typeof recoveryDeadlineAt !== 'string'
      || !RFC3339_INSTANT.test(recoveryDeadlineAt)
      || Number.isNaN(Date.parse(recoveryDeadlineAt)))
  ) {
    throw invalidDeviceListError()
  }
  return Object.freeze({
    clientId: value.clientId,
    displayName: value.displayName,
    presence: presence as ControlPlaneDevicePresence,
    occupancy: occupancy as ControlPlaneDeviceOccupancy,
    capacityUsed,
    capacityTotal,
    lastHeartbeatAt,
    version: value.version,
    ...(recoveryDeadlineAt === null ? {} : { recoveryDeadlineAt }),
  })
}

function deviceListResponse(source: string): readonly ControlPlaneDeviceSummary[] {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  if (
    isRecord(value)
    && typeof value.schemaVersion === 'string'
    && value.schemaVersion !== CONTROL_PLANE_SCHEMA_VERSION
  ) {
    throw new ControlPlaneClientError({
      kind: 'version',
      code: 'SCHEMA_VERSION_MISMATCH',
      message: `The Control Plane server must use ${CONTROL_PLANE_SCHEMA_VERSION}.`,
      requestId: null,
      retryable: false,
    })
  }
  if (!isRecord(value) || !Array.isArray(value.clients)) throw invalidDeviceListError()
  return Object.freeze(value.clients.map(deviceSummaryValue))
}

export interface ControlPlaneClientDirectory extends ControlPlaneClient {
  addClient(
    input: ControlPlaneClientConnectInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<readonly ControlPlaneDeviceSummary[]>
  listClients(
    options?: ControlPlaneRequestOptions,
  ): Promise<readonly ControlPlaneDeviceSummary[]>
  listRepositories(
    input: ControlPlaneRepositoryListInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<readonly ControlPlaneRepositorySummary[]>
}

export function createControlPlaneClientDirectory(options: {
  readonly client: ControlPlaneClient
  readonly transport?: ControlPlaneClientTransport
}): ControlPlaneClientDirectory {
  const location = parseControlPlaneServerUrl(options.client.serverUrl)
  const transportFetch = options.transport?.fetch
  const injected = options.client as Partial<ControlPlaneClientDirectory>
  const injectedAddClient = injected.addClient
  const injectedListClients = injected.listClients
  const injectedListRepositories = injected.listRepositories

  async function directoryRequest(
    path: string,
    method: 'GET' | 'POST',
    body: string | null,
    signal: AbortSignal | undefined,
  ): Promise<ControlPlaneHttpResponse> {
    if (signalIsAborted(signal)) throw cancelledError(null)
    if (transportFetch === undefined) {
      throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'TRANSPORT_UNAVAILABLE',
        message: 'The browser HTTP transport is unavailable.',
        requestId: null,
        retryable: false,
      })
    }
    return sendHttp(transportFetch, `${location.serverUrl}${path}`, {
      method,
      headers: body === null ? {} : { 'Content-Type': 'application/json' },
      ...(body === null ? {} : { body }),
      redirect: 'error',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      credentials: 'include',
      ...(signal === undefined ? {} : { signal }),
    })
  }

  async function connectRequest(
    input: ControlPlaneClientConnectInput,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<readonly ControlPlaneDeviceSummary[]> {
    assertClientConnectInput(input)
    try {
      const response = await directoryRequest(
        CLIENT_DIRECTORY_CONNECT_PATH,
        'POST',
        JSON.stringify({
          schemaVersion: CONTROL_PLANE_SCHEMA_VERSION,
          clientId: input.clientId.replace(/\D+/gu, ''),
          connectionCode: input.connectionCode.replace(/\D+/gu, ''),
        }),
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(clientDirectoryBoundaryError(response.status, source), response)
      if (response.status !== 201) throw invalidDeviceListError()
      return deviceListResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw new ControlPlaneClientError({
        kind: 'network',
        code: 'NETWORK_ERROR',
        message: 'The Control Plane server could not be reached.',
        requestId: null,
        retryable: true,
        cause: error,
      })
    }
  }

  async function listRequest(
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<readonly ControlPlaneDeviceSummary[]> {
    try {
      const response = await directoryRequest(
        CLIENT_DIRECTORY_LIST_PATH,
        'GET',
        null,
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(clientDirectoryBoundaryError(response.status, source), response)
      if (response.status !== 200) throw invalidDeviceListError()
      return deviceListResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw new ControlPlaneClientError({
        kind: 'network',
        code: 'NETWORK_ERROR',
        message: 'The Control Plane server could not be reached.',
        requestId: null,
        retryable: true,
        cause: error,
      })
    }
  }

  const directory: ControlPlaneClientDirectory = {
    serverUrl: options.client.serverUrl,
    restore(requestOptions) {
      return options.client.restore(requestOptions)
    },
    initializeOwner(initialization, requestOptions) {
      return options.client.initializeOwner(initialization, requestOptions)
    },
    login(credentials, requestOptions) {
      return options.client.login(credentials, requestOptions)
    },
    changePassword(change, requestOptions) {
      return options.client.changePassword(change, requestOptions)
    },
    initializationStatus(requestOptions) {
      return options.client.initializationStatus(requestOptions)
    },
    logout(requestOptions) {
      return options.client.logout(requestOptions)
    },
    command(command, requestOptions) {
      return options.client.command(command, requestOptions)
    },
    query(query, requestOptions) {
      return options.client.query(query, requestOptions)
    },
    subscribe(subscriptionOptions) {
      return options.client.subscribe(subscriptionOptions)
    },
    addClient(rawInput, requestOptions) {
      const input: ControlPlaneClientConnectInput = {
        clientId: rawInput.clientId.replace(/\D+/gu, ''),
        connectionCode: rawInput.connectionCode.replace(/\D+/gu, ''),
      }
      if (typeof injectedAddClient === 'function') {
        return injectedAddClient.call(options.client, input, requestOptions)
      }
      return connectRequest(input, requestOptions)
    },
    listClients(requestOptions) {
      if (typeof injectedListClients === 'function') {
        return injectedListClients.call(options.client, requestOptions)
      }
      return listRequest(requestOptions)
    },
    async listRepositories(rawInput, requestOptions) {
      const input: ControlPlaneRepositoryListInput = {
        clientId: (typeof rawInput?.clientId === 'string' ? rawInput.clientId : '').trim(),
      }
      assertRepositoryListInput(input.clientId)
      if (typeof injectedListRepositories === 'function') {
        return injectedListRepositories.call(options.client, input, requestOptions)
      }
      return repositoriesListRequest(location, transportFetch, input.clientId, requestOptions?.signal)
    },
    close() {
      options.client.close()
    },
  }
  return Object.freeze(directory)
}

const CLIENT_OCCUPANCY_CLAIM_PATH = '/api/v1/clients/occupancy'
const CLIENT_OCCUPANCY_FORCE_RELEASE_PATH = '/api/v1/clients/occupancy/force-release'

export interface ControlPlaneOccupancyClaimInput {
  readonly clientId: string
}

export type ControlPlaneOccupancyReleaseMode = 'release' | 'drain' | 'cancel_and_release'

export interface ControlPlaneOccupancyReleaseInput {
  readonly clientId: string
  readonly mode: ControlPlaneOccupancyReleaseMode
  readonly confirm?: boolean
}

export interface ControlPlaneOccupancyStatusInput {
  readonly clientId: string
}

export type ControlPlaneOccupancyHolderState =
  | 'reserving'
  | 'occupied'
  | 'draining'
  | 'recovery_pending'

export interface ControlPlaneOccupancyAvailable {
  readonly occupancy: 'available'
  readonly presence: ControlPlaneDevicePresence
}

export interface ControlPlaneOccupiedByOther {
  readonly occupancy: 'occupied-by-other'
}

export interface ControlPlaneOccupancyHolderView {
  readonly occupancy: ControlPlaneOccupancyHolderState
  readonly presence: ControlPlaneDevicePresence
  readonly holderUserId: string
  readonly occupancyLeaseId: string
  readonly fencingToken: number
  readonly claimedAt: string | null
  readonly acknowledgedAt: string | null
  readonly recoveryDeadlineAt: string | null
  readonly capacityUsed: number
  readonly capacityTotal: number
}

export type ControlPlaneOccupancyStatus =
  | ControlPlaneOccupancyAvailable
  | ControlPlaneOccupiedByOther
  | ControlPlaneOccupancyHolderView

export interface ControlPlaneOccupancyReleaseOutcome {
  readonly occupancy: 'released' | 'draining'
  readonly occupancyLeaseId: string
  readonly mode: ControlPlaneOccupancyReleaseMode
}

export interface ControlPlaneOccupancyForceReleaseOutcome {
  readonly released: true
  readonly occupancyLeaseId: string
  readonly forceFenceToken: number
}

export type ControlPlaneOccupancyFailure =
  | 'invalid-request'
  | 'confirmation-required'
  | 'client-not-found'
  | 'client-offline'
  | 'client-locked'
  | 'new-connections-forbidden'
  | 'access-denied'
  | 'occupied-by-other'
  | 'capacity-exhausted'
  | 'occupancy-rejected'
  | 'occupancy-ack-timeout'
  | 'recovery-pending'
  | 'permission-denied'
  | 'no-active-occupancy'
  | 'wrong-state'
  | 'rate-limited'
  | 'unavailable'

const OCCUPANCY_FAILURE_CODES: Readonly<Record<string, ControlPlaneOccupancyFailure>> =
  Object.freeze({
    INVALID_REQUEST: 'invalid-request',
    CONFIRMATION_REQUIRED: 'confirmation-required',
    CLIENT_NOT_FOUND: 'client-not-found',
    CLIENT_OFFLINE: 'client-offline',
    CLIENT_LOCKED: 'client-locked',
    CLIENT_CONNECTIONS_FORBIDDEN: 'new-connections-forbidden',
    ACCESS_DENIED: 'access-denied',
    OCCUPIED_BY_OTHER: 'occupied-by-other',
    CAPACITY_EXHAUSTED: 'capacity-exhausted',
    OCCUPANCY_REJECTED: 'occupancy-rejected',
    OCCUPANCY_ACK_TIMEOUT: 'occupancy-ack-timeout',
    OCCUPANCY_RECOVERY_PENDING: 'recovery-pending',
    PERMISSION_DENIED: 'permission-denied',
    RESOURCE_NOT_FOUND: 'no-active-occupancy',
    WRONG_STATE: 'wrong-state',
    RATE_LIMITED: 'rate-limited',
  })

export function controlPlaneOccupancyFailure(error: unknown): ControlPlaneOccupancyFailure {
  if (error instanceof ControlPlaneClientError) {
    const failure = OCCUPANCY_FAILURE_CODES[error.code]
    if (failure !== undefined) return failure
  }
  return 'unavailable'
}

const OCCUPANCY_HOLDER_STATE_VALUES: readonly string[] = Object.freeze([
  'reserving',
  'occupied',
  'draining',
  'recovery_pending',
])
const OCCUPANCY_RELEASE_OUTCOME_VALUES: readonly string[] = Object.freeze([
  'released',
  'draining',
])
const OCCUPANCY_RELEASE_MODE_VALUES: readonly string[] = Object.freeze([
  'release',
  'drain',
  'cancel_and_release',
])

function invalidOccupancyResponseError(): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
    message: 'The Control Plane server returned an invalid occupancy response.',
    requestId: null,
    retryable: false,
  })
}

function occupancyNetworkError(cause: unknown): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'network',
    code: 'NETWORK_ERROR',
    message: 'The Control Plane server could not be reached.',
    requestId: null,
    retryable: true,
    cause,
  })
}

function assertOccupancyClientId(clientId: string): void {
  if (!/^\d{9,12}$/u.test(clientId)) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'CLIENT_OCCUPANCY_ID_INVALID',
      message: 'Select a Client to manage its occupancy.',
      requestId: null,
      retryable: false,
    })
  }
}

function occupancyInputClientId(
  input: ControlPlaneOccupancyClaimInput | ControlPlaneOccupancyReleaseInput
    | ControlPlaneOccupancyStatusInput,
): string {
  const raw = typeof input?.clientId === 'string' ? input.clientId : ''
  return raw.replace(/\D+/gu, '')
}

function clientOccupancyBoundaryError(
  status: number,
  source: string,
): ControlPlaneClientError {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  const error = isRecord(value) && isRecord(value.error) ? value.error : null
  const code = error !== null && typeof error.code === 'string'
    ? error.code
    : 'CLIENT_OCCUPANCY_FAILED'
  const kind: ControlPlaneClientErrorKind = accessKind(code)
    ?? (code === 'RATE_LIMITED' || code === 'TOO_MANY_REQUESTS'
      ? 'server'
      : (versionCode(code)
        ? 'version'
        : (status >= 500 ? 'server' : 'protocol')))
  return new ControlPlaneClientError({
    kind,
    code,
    message: error !== null && typeof error.message === 'string'
      ? error.message
      : 'The Client occupancy request failed.',
    requestId: isRecord(value) && typeof value.requestId === 'string'
      ? value.requestId as RequestId
      : null,
    retryable: error !== null && error.retryable === true,
  })
}

function occupancyPresenceValue(value: unknown): ControlPlaneDevicePresence {
  if (typeof value !== 'string' || !DEVICE_PRESENCE_VALUES.includes(value)) {
    throw invalidOccupancyResponseError()
  }
  return value as ControlPlaneDevicePresence
}

function occupancyInstantValue(value: unknown): string | null {
  if (
    typeof value !== 'string'
    || !RFC3339_INSTANT.test(value)
    || Number.isNaN(Date.parse(value))
  ) {
    throw invalidOccupancyResponseError()
  }
  return value
}

function occupancyNullableInstantValue(value: unknown): string | null {
  if (value === null) return null
  return occupancyInstantValue(value)
}

function occupancyHolderViewValue(
  value: Readonly<Record<string, unknown>>,
): ControlPlaneOccupancyHolderView {
  const occupancy = value.occupancy
  const fencingToken = value.fencingToken
  const capacityUsed = value.capacityUsed
  const capacityTotal = value.capacityTotal
  if (
    typeof occupancy !== 'string'
    || !OCCUPANCY_HOLDER_STATE_VALUES.includes(occupancy)
    || typeof value.holderUserId !== 'string'
    || value.holderUserId.length === 0
    || typeof value.occupancyLeaseId !== 'string'
    || value.occupancyLeaseId.length === 0
    || typeof fencingToken !== 'number'
    || !Number.isInteger(fencingToken)
    || fencingToken < 1
    || typeof capacityUsed !== 'number'
    || !Number.isInteger(capacityUsed)
    || capacityUsed < 0
    || typeof capacityTotal !== 'number'
    || !Number.isInteger(capacityTotal)
    || capacityTotal < capacityUsed
  ) {
    throw invalidOccupancyResponseError()
  }
  const presence = occupancyPresenceValue(value.presence)
  const claimedAt = occupancyNullableInstantValue(value.claimedAt)
  const acknowledgedAt = occupancyNullableInstantValue(value.acknowledgedAt)
  const recoveryDeadlineAt = occupancyNullableInstantValue(value.recoveryDeadlineAt)
  return Object.freeze({
    occupancy: occupancy as ControlPlaneOccupancyHolderState,
    presence,
    holderUserId: value.holderUserId,
    occupancyLeaseId: value.occupancyLeaseId,
    fencingToken,
    claimedAt,
    acknowledgedAt,
    recoveryDeadlineAt,
    capacityUsed,
    capacityTotal,
  })
}

function parsedOccupancyPayload(source: string): Readonly<Record<string, unknown>> {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  if (
    isRecord(value)
    && typeof value.schemaVersion === 'string'
    && value.schemaVersion !== CONTROL_PLANE_SCHEMA_VERSION
  ) {
    throw new ControlPlaneClientError({
      kind: 'version',
      code: 'SCHEMA_VERSION_MISMATCH',
      message: `The Control Plane server must use ${CONTROL_PLANE_SCHEMA_VERSION}.`,
      requestId: null,
      retryable: false,
    })
  }
  if (!isRecord(value)) throw invalidOccupancyResponseError()
  return value
}

function occupancyHolderViewResponse(source: string): ControlPlaneOccupancyHolderView {
  return occupancyHolderViewValue(parsedOccupancyPayload(source))
}

function occupancyStatusResponse(source: string): ControlPlaneOccupancyStatus {
  const value = parsedOccupancyPayload(source)
  if (value.occupancy === 'available') {
    return Object.freeze({
      occupancy: 'available',
      presence: occupancyPresenceValue(value.presence),
    })
  }
  if (value.occupancy === 'occupied-by-other') {
    return Object.freeze({ occupancy: 'occupied-by-other' })
  }
  return occupancyHolderViewValue(value)
}

function occupancyReleaseResponse(source: string): ControlPlaneOccupancyReleaseOutcome {
  const value = parsedOccupancyPayload(source)
  const occupancy = value.occupancy
  const mode = value.mode
  if (
    typeof occupancy !== 'string'
    || !OCCUPANCY_RELEASE_OUTCOME_VALUES.includes(occupancy)
    || typeof value.occupancyLeaseId !== 'string'
    || value.occupancyLeaseId.length === 0
    || typeof mode !== 'string'
    || !OCCUPANCY_RELEASE_MODE_VALUES.includes(mode)
  ) {
    throw invalidOccupancyResponseError()
  }
  return Object.freeze({
    occupancy: occupancy as ControlPlaneOccupancyReleaseOutcome['occupancy'],
    occupancyLeaseId: value.occupancyLeaseId,
    mode: mode as ControlPlaneOccupancyReleaseMode,
  })
}

function occupancyForceReleaseResponse(
  source: string,
): ControlPlaneOccupancyForceReleaseOutcome {
  const value = parsedOccupancyPayload(source)
  const forceFenceToken = value.forceFenceToken
  if (
    value.released !== true
    || typeof value.occupancyLeaseId !== 'string'
    || value.occupancyLeaseId.length === 0
    || typeof forceFenceToken !== 'number'
    || !Number.isInteger(forceFenceToken)
    || forceFenceToken < 1
  ) {
    throw invalidOccupancyResponseError()
  }
  return Object.freeze({
    released: true,
    occupancyLeaseId: value.occupancyLeaseId,
    forceFenceToken,
  })
}

export interface ControlPlaneClientOccupancy extends ControlPlaneClient {
  claimOccupancy(
    input: ControlPlaneOccupancyClaimInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyHolderView>
  occupancyStatus(
    input: ControlPlaneOccupancyStatusInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyStatus>
  releaseOccupancy(
    input: ControlPlaneOccupancyReleaseInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyReleaseOutcome>
  forceReleaseOccupancy(
    input: ControlPlaneOccupancyStatusInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneOccupancyForceReleaseOutcome>
}

export function createControlPlaneClientOccupancy(options: {
  readonly client: ControlPlaneClient
  readonly transport?: ControlPlaneClientTransport
}): ControlPlaneClientOccupancy {
  const location = parseControlPlaneServerUrl(options.client.serverUrl)
  const transportFetch = options.transport?.fetch
  const injected = options.client as Partial<ControlPlaneClientOccupancy>
  const injectedClaimOccupancy = injected.claimOccupancy
  const injectedOccupancyStatus = injected.occupancyStatus
  const injectedReleaseOccupancy = injected.releaseOccupancy
  const injectedForceReleaseOccupancy = injected.forceReleaseOccupancy
  const claimsInFlight = new Map<string, Promise<ControlPlaneOccupancyHolderView>>()

  function claimOnce(
    clientId: string,
    run: () => Promise<ControlPlaneOccupancyHolderView>,
  ): Promise<ControlPlaneOccupancyHolderView> {
    const pending = claimsInFlight.get(clientId)
    if (pending !== undefined) return pending
    const claim = run().finally(() => {
      claimsInFlight.delete(clientId)
    })
    claimsInFlight.set(clientId, claim)
    return claim
  }

  async function occupancyRequest(
    path: string,
    method: 'DELETE' | 'GET' | 'POST',
    body: string | null,
    signal: AbortSignal | undefined,
  ): Promise<ControlPlaneHttpResponse> {
    if (signalIsAborted(signal)) throw cancelledError(null)
    if (transportFetch === undefined) {
      throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'TRANSPORT_UNAVAILABLE',
        message: 'The browser HTTP transport is unavailable.',
        requestId: null,
        retryable: false,
      })
    }
    return sendHttp(transportFetch, `${location.serverUrl}${path}`, {
      method,
      headers: body === null ? {} : { 'Content-Type': 'application/json' },
      ...(body === null ? {} : { body }),
      redirect: 'error',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      credentials: 'include',
      ...(signal === undefined ? {} : { signal }),
    })
  }

  async function claimRequest(
    input: ControlPlaneOccupancyClaimInput,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<ControlPlaneOccupancyHolderView> {
    try {
      const response = await occupancyRequest(
        CLIENT_OCCUPANCY_CLAIM_PATH,
        'POST',
        JSON.stringify({
          schemaVersion: CONTROL_PLANE_SCHEMA_VERSION,
          clientId: input.clientId,
        }),
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(clientOccupancyBoundaryError(response.status, source), response)
      if (response.status !== 201) throw invalidOccupancyResponseError()
      return occupancyHolderViewResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw occupancyNetworkError(error)
    }
  }

  async function statusRequest(
    input: ControlPlaneOccupancyStatusInput,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<ControlPlaneOccupancyStatus> {
    try {
      const response = await occupancyRequest(
        `/api/v1/clients/${encodeURIComponent(input.clientId)}/occupancy`,
        'GET',
        null,
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(clientOccupancyBoundaryError(response.status, source), response)
      if (response.status !== 200) throw invalidOccupancyResponseError()
      return occupancyStatusResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw occupancyNetworkError(error)
    }
  }

  async function releaseRequest(
    input: ControlPlaneOccupancyReleaseInput,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<ControlPlaneOccupancyReleaseOutcome> {
    try {
      const response = await occupancyRequest(
        CLIENT_OCCUPANCY_CLAIM_PATH,
        'DELETE',
        JSON.stringify({
          schemaVersion: CONTROL_PLANE_SCHEMA_VERSION,
          clientId: input.clientId,
          mode: input.mode,
          ...(input.confirm === undefined ? {} : { confirm: input.confirm }),
        }),
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(clientOccupancyBoundaryError(response.status, source), response)
      if (response.status !== 200) throw invalidOccupancyResponseError()
      return occupancyReleaseResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw occupancyNetworkError(error)
    }
  }

  async function forceReleaseRequest(
    input: ControlPlaneOccupancyStatusInput,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<ControlPlaneOccupancyForceReleaseOutcome> {
    try {
      const response = await occupancyRequest(
        CLIENT_OCCUPANCY_FORCE_RELEASE_PATH,
        'POST',
        JSON.stringify({
          schemaVersion: CONTROL_PLANE_SCHEMA_VERSION,
          clientId: input.clientId,
        }),
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(clientOccupancyBoundaryError(response.status, source), response)
      if (response.status !== 200) throw invalidOccupancyResponseError()
      return occupancyForceReleaseResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw occupancyNetworkError(error)
    }
  }

  const occupancy: ControlPlaneClientOccupancy = {
    serverUrl: options.client.serverUrl,
    restore(requestOptions) {
      return options.client.restore(requestOptions)
    },
    initializeOwner(initialization, requestOptions) {
      return options.client.initializeOwner(initialization, requestOptions)
    },
    login(credentials, requestOptions) {
      return options.client.login(credentials, requestOptions)
    },
    changePassword(change, requestOptions) {
      return options.client.changePassword(change, requestOptions)
    },
    initializationStatus(requestOptions) {
      return options.client.initializationStatus(requestOptions)
    },
    logout(requestOptions) {
      return options.client.logout(requestOptions)
    },
    command(command, requestOptions) {
      return options.client.command(command, requestOptions)
    },
    query(query, requestOptions) {
      return options.client.query(query, requestOptions)
    },
    subscribe(subscriptionOptions) {
      return options.client.subscribe(subscriptionOptions)
    },
    claimOccupancy(rawInput, requestOptions) {
      try {
        const clientId = occupancyInputClientId(rawInput)
        assertOccupancyClientId(clientId)
        const input: ControlPlaneOccupancyClaimInput = { clientId }
        if (typeof injectedClaimOccupancy === 'function') {
          return claimOnce(clientId, () =>
            injectedClaimOccupancy.call(options.client, input, requestOptions))
        }
        return claimOnce(clientId, () => claimRequest(input, requestOptions))
      } catch (error) {
        return Promise.reject(error)
      }
    },
    async occupancyStatus(rawInput, requestOptions) {
      const clientId = occupancyInputClientId(rawInput)
      assertOccupancyClientId(clientId)
      const input: ControlPlaneOccupancyStatusInput = { clientId }
      if (typeof injectedOccupancyStatus === 'function') {
        return injectedOccupancyStatus.call(options.client, input, requestOptions)
      }
      return statusRequest(input, requestOptions)
    },
    async releaseOccupancy(rawInput, requestOptions) {
      const clientId = occupancyInputClientId(rawInput)
      assertOccupancyClientId(clientId)
      const input: ControlPlaneOccupancyReleaseInput = {
        clientId,
        mode: rawInput.mode,
        ...(rawInput.confirm === undefined ? {} : { confirm: rawInput.confirm }),
      }
      if (typeof injectedReleaseOccupancy === 'function') {
        return injectedReleaseOccupancy.call(options.client, input, requestOptions)
      }
      return releaseRequest(input, requestOptions)
    },
    async forceReleaseOccupancy(rawInput, requestOptions) {
      const clientId = occupancyInputClientId(rawInput)
      assertOccupancyClientId(clientId)
      const input: ControlPlaneOccupancyStatusInput = { clientId }
      if (typeof injectedForceReleaseOccupancy === 'function') {
        return injectedForceReleaseOccupancy.call(options.client, input, requestOptions)
      }
      return forceReleaseRequest(input, requestOptions)
    },
    close() {
      options.client.close()
    },
  }
  return Object.freeze(occupancy)
}

export type ControlPlaneCandidateState =
  | 'retained'
  | 'branch_created'
  | 'applied'
  | 'discarded'
  | 'failed'

export type ControlPlaneCandidateApplyStrategy =
  | 'create_branch'
  | 'fast_forward'
  | 'cherry_pick'
  | 'merge'

export type ControlPlaneCandidateApplyResult =
  | 'retained'
  | 'branch_created'
  | 'applied'
  | 'base_stale'
  | 'working_tree_dirty'
  | 'merge_conflict'
  | 'candidate_missing'
  | 'permission_denied'
  | 'discarded'
  | 'failed'

export interface ControlPlaneCandidateApplyReceipt {
  readonly localApplyReceiptId: string
  readonly candidateRef: string
  readonly repositoryBindingId: string
  readonly targetBranch: string
  readonly expectedHead: string
  readonly strategy: ControlPlaneCandidateApplyStrategy
  readonly result: ControlPlaneCandidateApplyResult
  readonly resultingCommit: string | null
  readonly conflictArtifactRef: string | null
  readonly createdAt: string
  readonly revision: number
}

export interface ControlPlaneCandidateSummary {
  readonly localCandidateReceiptId: string
  readonly candidateRef: string
  readonly repositoryBindingId: string
  readonly candidateCommit: string
  readonly localRefName: string
  readonly state: ControlPlaneCandidateState
  readonly createdAt: string
  readonly revision: number
  readonly branchName: string | null
  readonly history: readonly ControlPlaneCandidateApplyReceipt[]
}

export interface ControlPlaneCandidateReadInput {
  readonly clientId: string
}

export interface ControlPlaneCandidateBranchInput {
  readonly clientId: string
  readonly candidateRef: string
  readonly repositoryBindingId: string
}

export interface ControlPlaneCandidateApplyInput {
  readonly clientId: string
  readonly candidateRef: string
  readonly repositoryBindingId: string
  readonly targetBranch: string
  readonly expectedHead: string
}

export interface ControlPlaneCandidateDiscardInput {
  readonly clientId: string
  readonly candidateRef: string
  readonly repositoryBindingId: string
}

export interface ControlPlaneCandidateBranchOutcome {
  readonly candidate: ControlPlaneCandidateSummary
  readonly branchName: string
}

export type ControlPlaneCandidateActionFailure =
  | 'invalid-request'
  | 'candidate-not-found'
  | 'client-not-found'
  | 'client-offline'
  | 'permission-denied'
  | 'wrong-state'
  | 'rate-limited'
  | 'unavailable'

const CANDIDATE_ACTION_FAILURE_CODES: Readonly<Record<string, ControlPlaneCandidateActionFailure>> =
  Object.freeze({
    INVALID_REQUEST: 'invalid-request',
    RESOURCE_NOT_FOUND: 'candidate-not-found',
    CLIENT_NOT_FOUND: 'client-not-found',
    CLIENT_OFFLINE: 'client-offline',
    ACCESS_DENIED: 'permission-denied',
    PERMISSION_DENIED: 'permission-denied',
    WRONG_STATE: 'wrong-state',
    RATE_LIMITED: 'rate-limited',
  })

export function controlPlaneCandidateActionFailure(
  error: unknown,
): ControlPlaneCandidateActionFailure {
  if (error instanceof ControlPlaneClientError) {
    const failure = CANDIDATE_ACTION_FAILURE_CODES[error.code]
    if (failure !== undefined) return failure
    if (error.kind === 'authorization') return 'permission-denied'
  }
  return 'unavailable'
}

const CANDIDATE_STATE_VALUES: readonly string[] = Object.freeze([
  'retained',
  'branch_created',
  'applied',
  'discarded',
  'failed',
])
const CANDIDATE_APPLY_STRATEGY_VALUES: readonly string[] = Object.freeze([
  'create_branch',
  'fast_forward',
  'cherry_pick',
  'merge',
])
const CANDIDATE_APPLY_RESULT_VALUES: readonly string[] = Object.freeze([
  'retained',
  'branch_created',
  'applied',
  'base_stale',
  'working_tree_dirty',
  'merge_conflict',
  'candidate_missing',
  'permission_denied',
  'discarded',
  'failed',
])

const CLIENT_CANDIDATES_BRANCH_PATH = '/api/v1/clients/candidates/branch'
const CLIENT_CANDIDATES_APPLY_PATH = '/api/v1/clients/candidates/apply'
const CLIENT_CANDIDATES_DISCARD_PATH = '/api/v1/clients/candidates/discard'

function invalidCandidateResponseError(): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'INVALID_CLIENT_CANDIDATES_RESPONSE',
    message: 'The Control Plane server returned an invalid candidate response.',
    requestId: null,
    retryable: false,
  })
}

function candidateNetworkError(cause: unknown): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'network',
    code: 'NETWORK_ERROR',
    message: 'The Control Plane server could not be reached.',
    requestId: null,
    retryable: true,
    cause,
  })
}

export function candidateBoundaryError(
  status: number,
  source: string,
  fallbackCode = 'CLIENT_CANDIDATES_FAILED',
  fallbackMessage = 'The Client candidate request failed.',
): ControlPlaneClientError {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  const error = isRecord(value) && isRecord(value.error) ? value.error : null
  const code = error !== null && typeof error.code === 'string'
    ? error.code
    : fallbackCode
  const kind: ControlPlaneClientErrorKind = accessKind(code)
    ?? (code === 'RATE_LIMITED' || code === 'TOO_MANY_REQUESTS'
      ? 'server'
      : (versionCode(code)
        ? 'version'
        : (status >= 500 ? 'server' : 'protocol')))
  return new ControlPlaneClientError({
    kind,
    code,
    message: error !== null && typeof error.message === 'string'
      ? error.message
      : fallbackMessage,
    requestId: isRecord(value) && typeof value.requestId === 'string'
      ? value.requestId as RequestId
      : null,
    retryable: error !== null && typeof error.retryable === 'boolean'
      ? error.retryable
      : status >= 500,
    details: error !== null && isRecord(error.details) ? error.details as ErrorDetails : {},
  })
}

function parsedCandidatePayload(source: string): Readonly<Record<string, unknown>> {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  if (
    isRecord(value)
    && typeof value.schemaVersion === 'string'
    && value.schemaVersion !== CONTROL_PLANE_SCHEMA_VERSION
  ) {
    throw new ControlPlaneClientError({
      kind: 'version',
      code: 'SCHEMA_VERSION_MISMATCH',
      message: `The Control Plane server must use ${CONTROL_PLANE_SCHEMA_VERSION}.`,
      requestId: null,
      retryable: false,
    })
  }
  if (!isRecord(value)) throw invalidCandidateResponseError()
  return value
}

function candidateIdentityText(value: unknown): string {
  if (typeof value !== 'string' || value.length === 0 || value.length > 200) {
    throw invalidCandidateResponseError()
  }
  return value
}

function candidateShaValue(value: unknown): string {
  if (typeof value !== 'string' || !/^[0-9a-f]{4,64}$/iu.test(value)) {
    throw invalidCandidateResponseError()
  }
  return value
}

function candidateInstantValue(value: unknown): string {
  if (
    typeof value !== 'string'
    || !RFC3339_INSTANT.test(value)
    || Number.isNaN(Date.parse(value))
  ) {
    throw invalidCandidateResponseError()
  }
  return value
}

function candidateNullableShaValue(value: unknown): string | null {
  if (value === null) return null
  return candidateShaValue(value)
}

function candidateNullableText(value: unknown): string | null {
  if (value === null) return null
  const text = candidateIdentityText(value)
  return text
}

function candidateRevisionValue(value: unknown): number {
  if (typeof value !== 'number' || !Number.isInteger(value) || value < 0) {
    throw invalidCandidateResponseError()
  }
  return value
}

function candidateApplyReceiptValue(value: unknown): ControlPlaneCandidateApplyReceipt {
  if (!isRecord(value)) throw invalidCandidateResponseError()
  const strategy = value.strategy
  const result = value.result
  if (
    typeof value.localApplyReceiptId !== 'string'
    || value.localApplyReceiptId.length === 0
    || typeof value.candidateRef !== 'string'
    || value.candidateRef.length === 0
    || typeof value.repositoryBindingId !== 'string'
    || value.repositoryBindingId.length === 0
    || typeof value.targetBranch !== 'string'
    || value.targetBranch.length === 0
    || typeof value.expectedHead !== 'string'
    || value.expectedHead.length === 0
    || typeof strategy !== 'string'
    || !CANDIDATE_APPLY_STRATEGY_VALUES.includes(strategy)
    || typeof result !== 'string'
    || !CANDIDATE_APPLY_RESULT_VALUES.includes(result)
  ) {
    throw invalidCandidateResponseError()
  }
  return Object.freeze({
    localApplyReceiptId: value.localApplyReceiptId,
    candidateRef: value.candidateRef,
    repositoryBindingId: value.repositoryBindingId,
    targetBranch: value.targetBranch,
    expectedHead: value.expectedHead,
    strategy: strategy as ControlPlaneCandidateApplyStrategy,
    result: result as ControlPlaneCandidateApplyResult,
    resultingCommit: candidateNullableShaValue(value.resultingCommit),
    conflictArtifactRef: candidateNullableText(value.conflictArtifactRef),
    createdAt: candidateInstantValue(value.createdAt),
    revision: candidateRevisionValue(value.revision),
  })
}

function candidateSummaryValue(value: unknown): ControlPlaneCandidateSummary {
  if (!isRecord(value)) throw invalidCandidateResponseError()
  const state = value.state
  if (
    typeof value.localCandidateReceiptId !== 'string'
    || value.localCandidateReceiptId.length === 0
    || typeof value.candidateRef !== 'string'
    || value.candidateRef.length === 0
    || typeof value.repositoryBindingId !== 'string'
    || value.repositoryBindingId.length === 0
    || typeof value.localRefName !== 'string'
    || value.localRefName.length === 0
    || typeof state !== 'string'
    || !CANDIDATE_STATE_VALUES.includes(state)
    || !Array.isArray(value.history)
  ) {
    throw invalidCandidateResponseError()
  }
  return Object.freeze({
    localCandidateReceiptId: value.localCandidateReceiptId,
    candidateRef: value.candidateRef,
    repositoryBindingId: value.repositoryBindingId,
    candidateCommit: candidateShaValue(value.candidateCommit),
    localRefName: value.localRefName,
    state: state as ControlPlaneCandidateState,
    createdAt: candidateInstantValue(value.createdAt),
    revision: candidateRevisionValue(value.revision),
    branchName: candidateNullableText(value.branchName),
    history: Object.freeze(value.history.map(candidateApplyReceiptValue)),
  })
}

function candidateListResponse(source: string): readonly ControlPlaneCandidateSummary[] {
  const value = parsedCandidatePayload(source)
  if (!Array.isArray(value.candidates)) throw invalidCandidateResponseError()
  return Object.freeze(value.candidates.map(candidateSummaryValue))
}

function candidateBranchOutcomeResponse(source: string): ControlPlaneCandidateBranchOutcome {
  const value = parsedCandidatePayload(source)
  return Object.freeze({
    candidate: candidateSummaryValue(value.candidate),
    branchName: candidateIdentityText(value.branchName),
  })
}

function candidateSummaryResponse(source: string): ControlPlaneCandidateSummary {
  return candidateSummaryValue(parsedCandidatePayload(source).candidate)
}

function candidateInputFields(input: {
  readonly clientId: string
  readonly candidateRef?: string
  readonly repositoryBindingId?: string
}): { readonly clientId: string; readonly candidateRef: string; readonly repositoryBindingId: string } {
  const clientId = (typeof input?.clientId === 'string' ? input.clientId : '')
    .replace(/\D+/gu, '')
  assertOccupancyClientId(clientId)
  const candidateRef = (typeof input?.candidateRef === 'string' ? input.candidateRef : '').trim()
  if (candidateRef.length === 0 || candidateRef.length > 200 || /\s/u.test(candidateRef)) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'CLIENT_CANDIDATE_REF_INVALID',
      message: 'Select a candidate from the device list.',
      requestId: null,
      retryable: false,
    })
  }
  const repositoryBindingId =
    (typeof input?.repositoryBindingId === 'string' ? input.repositoryBindingId : '').trim()
  if (repositoryBindingId.length === 0 || repositoryBindingId.length > 200) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'CLIENT_CANDIDATE_BINDING_INVALID',
      message: 'The candidate is missing its repository binding.',
      requestId: null,
      retryable: false,
    })
  }
  return Object.freeze({ clientId, candidateRef, repositoryBindingId })
}

function candidateApplyFields(input: ControlPlaneCandidateApplyInput): {
  readonly clientId: string
  readonly candidateRef: string
  readonly repositoryBindingId: string
  readonly targetBranch: string
  readonly expectedHead: string
} {
  const identity = candidateInputFields(input)
  const targetBranch = (typeof input?.targetBranch === 'string' ? input.targetBranch : '').trim()
  if (
    targetBranch.length === 0
    || targetBranch.length > 200
    || /\s/u.test(targetBranch)
    || targetBranch.startsWith('-')
    || targetBranch.includes('..')
    || targetBranch.endsWith('/')
    || targetBranch.endsWith('.lock')
  ) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'CLIENT_CANDIDATE_TARGET_BRANCH_INVALID',
      message: 'Enter the target branch of the apply.',
      requestId: null,
      retryable: false,
    })
  }
  const expectedHead = (typeof input?.expectedHead === 'string' ? input.expectedHead : '').trim()
  if (!/^[0-9a-f]{4,64}$/iu.test(expectedHead)) {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'CLIENT_CANDIDATE_EXPECTED_HEAD_INVALID',
      message: 'Enter the expected HEAD commit of the target branch.',
      requestId: null,
      retryable: false,
    })
  }
  return Object.freeze({ ...identity, targetBranch, expectedHead })
}

export interface ControlPlaneClientCandidates extends ControlPlaneClient {
  listDeviceCandidates(
    input: ControlPlaneCandidateReadInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<readonly ControlPlaneCandidateSummary[]>
  createCandidateBranch(
    input: ControlPlaneCandidateBranchInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneCandidateBranchOutcome>
  applyCandidate(
    input: ControlPlaneCandidateApplyInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneCandidateApplyReceipt>
  discardCandidate(
    input: ControlPlaneCandidateDiscardInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneCandidateSummary>
}

export function createControlPlaneClientCandidates(options: {
  readonly client: ControlPlaneClient
  readonly transport?: ControlPlaneClientTransport
}): ControlPlaneClientCandidates {
  const location = parseControlPlaneServerUrl(options.client.serverUrl)
  const transportFetch = options.transport?.fetch
  const injected = options.client as Partial<ControlPlaneClientCandidates>
  const injectedListCandidates = injected.listDeviceCandidates
  const injectedCreateCandidateBranch = injected.createCandidateBranch
  const injectedApplyCandidate = injected.applyCandidate
  const injectedDiscardCandidate = injected.discardCandidate

  async function candidateRequest(
    path: string,
    method: 'GET' | 'POST',
    body: string | null,
    signal: AbortSignal | undefined,
  ): Promise<ControlPlaneHttpResponse> {
    if (signalIsAborted(signal)) throw cancelledError(null)
    if (transportFetch === undefined) {
      throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'TRANSPORT_UNAVAILABLE',
        message: 'The browser HTTP transport is unavailable.',
        requestId: null,
        retryable: false,
      })
    }
    return sendHttp(transportFetch, `${location.serverUrl}${path}`, {
      method,
      headers: body === null ? {} : { 'Content-Type': 'application/json' },
      ...(body === null ? {} : { body }),
      redirect: 'error',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      credentials: 'include',
      ...(signal === undefined ? {} : { signal }),
    })
  }

  async function listCandidatesRequest(
    input: ControlPlaneCandidateReadInput,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<readonly ControlPlaneCandidateSummary[]> {
    try {
      const response = await candidateRequest(
        `/api/v1/clients/${encodeURIComponent(input.clientId)}/candidates`,
        'GET',
        null,
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(candidateBoundaryError(response.status, source), response)
      if (response.status !== 200) throw invalidCandidateResponseError()
      return candidateListResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw candidateNetworkError(error)
    }
  }

  async function branchRequest(
    input: ControlPlaneCandidateBranchInput,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<ControlPlaneCandidateBranchOutcome> {
    try {
      const response = await candidateRequest(
        CLIENT_CANDIDATES_BRANCH_PATH,
        'POST',
        JSON.stringify({
          schemaVersion: CONTROL_PLANE_SCHEMA_VERSION,
          clientId: input.clientId,
          candidateRef: input.candidateRef,
          repositoryBindingId: input.repositoryBindingId,
        }),
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(candidateBoundaryError(response.status, source), response)
      if (response.status !== 201) throw invalidCandidateResponseError()
      return candidateBranchOutcomeResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw candidateNetworkError(error)
    }
  }

  async function applyRequest(
    input: ControlPlaneCandidateApplyInput,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<ControlPlaneCandidateApplyReceipt> {
    try {
      const response = await candidateRequest(
        CLIENT_CANDIDATES_APPLY_PATH,
        'POST',
        JSON.stringify({
          schemaVersion: CONTROL_PLANE_SCHEMA_VERSION,
          clientId: input.clientId,
          candidateRef: input.candidateRef,
          repositoryBindingId: input.repositoryBindingId,
          targetBranch: input.targetBranch,
          expectedHead: input.expectedHead,
        }),
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(candidateBoundaryError(response.status, source), response)
      if (response.status !== 201) throw invalidCandidateResponseError()
      return candidateApplyReceiptValue(parsedCandidatePayload(source).receipt)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw candidateNetworkError(error)
    }
  }

  async function discardRequest(
    input: ControlPlaneCandidateDiscardInput,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<ControlPlaneCandidateSummary> {
    try {
      const response = await candidateRequest(
        CLIENT_CANDIDATES_DISCARD_PATH,
        'POST',
        JSON.stringify({
          schemaVersion: CONTROL_PLANE_SCHEMA_VERSION,
          clientId: input.clientId,
          candidateRef: input.candidateRef,
          repositoryBindingId: input.repositoryBindingId,
        }),
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(candidateBoundaryError(response.status, source), response)
      if (response.status !== 200) throw invalidCandidateResponseError()
      return candidateSummaryResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      if (error instanceof ControlPlaneClientError) throw error
      throw candidateNetworkError(error)
    }
  }

  const candidates: ControlPlaneClientCandidates = {
    serverUrl: options.client.serverUrl,
    restore(requestOptions) {
      return options.client.restore(requestOptions)
    },
    initializeOwner(initialization, requestOptions) {
      return options.client.initializeOwner(initialization, requestOptions)
    },
    login(credentials, requestOptions) {
      return options.client.login(credentials, requestOptions)
    },
    changePassword(change, requestOptions) {
      return options.client.changePassword(change, requestOptions)
    },
    initializationStatus(requestOptions) {
      return options.client.initializationStatus(requestOptions)
    },
    logout(requestOptions) {
      return options.client.logout(requestOptions)
    },
    command(command, requestOptions) {
      return options.client.command(command, requestOptions)
    },
    query(query, requestOptions) {
      return options.client.query(query, requestOptions)
    },
    subscribe(subscriptionOptions) {
      return options.client.subscribe(subscriptionOptions)
    },
    async listDeviceCandidates(rawInput, requestOptions) {
      const input: ControlPlaneCandidateReadInput = {
        clientId: (typeof rawInput?.clientId === 'string' ? rawInput.clientId : '')
          .replace(/\D+/gu, ''),
      }
      assertOccupancyClientId(input.clientId)
      if (typeof injectedListCandidates === 'function') {
        return injectedListCandidates.call(options.client, input, requestOptions)
      }
      return listCandidatesRequest(input, requestOptions)
    },
    async createCandidateBranch(rawInput, requestOptions) {
      const input = candidateInputFields(rawInput)
      if (typeof injectedCreateCandidateBranch === 'function') {
        return injectedCreateCandidateBranch.call(options.client, input, requestOptions)
      }
      return branchRequest(input, requestOptions)
    },
    async applyCandidate(rawInput, requestOptions) {
      const input = candidateApplyFields(rawInput)
      if (typeof injectedApplyCandidate === 'function') {
        return injectedApplyCandidate.call(options.client, input, requestOptions)
      }
      return applyRequest(input, requestOptions)
    },
    async discardCandidate(rawInput, requestOptions) {
      const input = candidateInputFields(rawInput)
      if (typeof injectedDiscardCandidate === 'function') {
        return injectedDiscardCandidate.call(options.client, input, requestOptions)
      }
      return discardRequest(input, requestOptions)
    },
    close() {
      options.client.close()
    },
  }
  return Object.freeze(candidates)
}

export interface ControlPlaneTaskModelRouteOption {
  readonly routeId: string
  readonly label: string
  readonly detail: string
}

const TASK_MODEL_ROUTE_FAKE_OPTIONS: readonly ControlPlaneTaskModelRouteOption[] =
  Object.freeze([
    Object.freeze({
      routeId: 'route_default',
      label: '工作区默认',
      detail: '设置中配置的默认模型路由。',
    }),
    Object.freeze({
      routeId: 'route_long_context',
      label: '长上下文',
      detail: '适合大型仓库的宽上下文路由。',
    }),
    Object.freeze({
      routeId: 'route_fast',
      label: '快速',
      detail: '适合快速迭代的低延迟路由。',
    }),
  ])

export function controlPlaneTaskModelRouteOptions(): readonly ControlPlaneTaskModelRouteOption[] {
  return TASK_MODEL_ROUTE_FAKE_OPTIONS
}

export interface ControlPlaneTaskCreateInput {
  readonly clientId: string
  readonly repositoryBindingId: string
  readonly baseBranch: string
  readonly description: string
  readonly modelRouteId: string
}

export interface ControlPlaneTaskAnchor {
  readonly taskId: import('./generated/contracts.js').WorkItemId
  readonly clientId: string
  readonly repositoryBindingId: string
  readonly baseBranch: string
  readonly description: string
  readonly modelRouteId: string
  readonly title?: string
  readonly changes?: string
  readonly acceptance?: string
  readonly managedAppRunConfig?: import('./generated/contracts.js').ManagedAppRunControlProjection
}

export interface ControlPlaneTaskPort {
  create(input: ControlPlaneTaskCreateInput): Promise<ControlPlaneTaskAnchor>
  describe(taskId: string): ControlPlaneTaskAnchor | null
}

export type ControlPlaneWorkerSessionLaunchInput = {
  readonly clientId: string
  readonly repositoryBindingId: string
} & ({ readonly workRunId: WorkRunId } | { readonly productSession: { readonly id: ProductSessionId; readonly scope: RepositoryScope } })

export interface ControlPlaneWorkerSessionPort {
  launch(input: ControlPlaneWorkerSessionLaunchInput): Promise<void>
}

export function createControlPlaneWorkerSessionPort(options: {
  readonly client: ControlPlaneClient
  readonly transport?: ControlPlaneClientTransport
}): ControlPlaneWorkerSessionPort {
  const location = parseControlPlaneServerUrl(options.client.serverUrl)
  const transportFetch = options.transport?.fetch
  return Object.freeze({
    async launch(input: ControlPlaneWorkerSessionLaunchInput): Promise<void> {
      if (transportFetch === undefined) throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'TRANSPORT_UNAVAILABLE',
        message: 'The browser HTTP transport is unavailable.',
        requestId: null,
        retryable: false,
      })
      let response: ControlPlaneHttpResponse
      try {
        response = await sendHttp(transportFetch, `${location.serverUrl}/api/v1/sessions`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            schemaVersion: 'winwincode/v1',
            clientId: input.clientId,
            repositoryBindingId: input.repositoryBindingId,
            ...("workRunId" in input ? { workRunId: input.workRunId } : { productSession: input.productSession }),
          }),
          redirect: 'error',
          cache: 'no-store',
          referrerPolicy: 'no-referrer',
          credentials: 'include',
        })
      } catch (cause) {
        throw new ControlPlaneClientError({
          kind: 'network',
          code: 'NETWORK_ERROR',
          message: 'The Control Plane server could not be reached.',
          requestId: null,
          retryable: true,
          cause,
        })
      }
      const source = await response.text()
      if (!response.ok) throw withResponseFailure(clientOccupancyBoundaryError(response.status, source), response)
      if (response.status !== 201) {
        throw new ControlPlaneClientError({
          kind: response.status === 401 ? 'authentication' : 'protocol',
          code: 'WORKER_SESSION_LAUNCH_FAILED',
          message: 'The Device Worker session could not be launched.',
          requestId: null,
          retryable: response.status >= 500,
        })
      }
      let body: unknown
      try {
        body = JSON.parse(source)
      } catch (cause) {
        throw new ControlPlaneClientError({
          kind: 'protocol',
          code: 'INVALID_WORKER_SESSION_RESPONSE',
          message: 'The Control Plane returned an invalid Worker session.',
          requestId: null,
          retryable: false,
          cause,
        })
      }
      if (typeof body !== 'object' || body === null
        || Reflect.get(body, 'clientId') !== input.clientId
        || Reflect.get(body, 'repositoryBindingId') !== input.repositoryBindingId
        || ('workRunId' in input ? Reflect.get(body, 'workRunId') !== input.workRunId : Reflect.get(body, 'productSessionId') !== input.productSession.id)) {
        throw new ControlPlaneClientError({
          kind: 'protocol',
          code: 'INVALID_WORKER_SESSION_RESPONSE',
          message: 'The Control Plane returned an invalid Worker session.',
          requestId: null,
          retryable: false,
        })
      }
    },
  })
}

export function createControlPlaneTaskPort(options: {
  readonly client: ControlPlaneClient
  readonly actor: () => Actor | null
  readonly scope: () => RepositoryScope | null
  readonly nextRequestId: () => RequestId
  readonly nextWorkItemId: () => import('./generated/contracts.js').WorkItemId
  readonly workerSessions: ControlPlaneWorkerSessionPort
  readonly deliveryContext: () => Promise<{
    readonly deliveryId: DeliveryId
    readonly expectedRevision: number
    readonly contractRevision: number
    readonly criterionId: import('./generated/contracts.js').CriterionId
  } | null>
}): ControlPlaneTaskPort {
  const anchors = new Map<string, ControlPlaneTaskAnchor>()
  return Object.freeze({
    async create(input: ControlPlaneTaskCreateInput): Promise<ControlPlaneTaskAnchor> {
      const context = await options.deliveryContext()
      if (context === null) throw new Error('创建任务项前请选择交付。')
      const actor = options.actor()
      const scope = options.scope()
      if (actor === null || scope === null) throw new Error('创建任务项前请登录并选择仓库范围。')
      const taskId = options.nextWorkItemId()
      const command = workItemsCreateCommand({
        actor,
        scope,
        requestId: options.nextRequestId(),
        deliveryId: context.deliveryId,
        expectedRevision: context.expectedRevision,
        contractRevision: context.contractRevision,
        items: [{
          id: taskId,
          title: input.description.slice(0, 500),
          goal: input.description,
          criterionIds: [context.criterionId],
          dependsOn: [],
        }],
      })
      const response = await options.client.command(command)
      if (!('result' in response) || response.outcome !== 'completed') {
        throw new Error('控制平面未完成任务项创建。')
      }
      if (!('items' in response.result)) throw new Error('控制平面返回了意外的命令结果。')
      const item = response.result.items.find(candidate => candidate.id === taskId)
      if (item === undefined) throw new Error('控制平面未返回已创建的任务项。')
      const startCommand: WorkRunStartCommand = {
        schemaVersion: 'winwincode/v1',
        command: 'workrun.start',
        actor,
        scope,
        requestId: options.nextRequestId(),
        expectedRevision: response.currentRevision,
        payload: {
          deliveryId: context.deliveryId,
          dispatchProfile: 'executor',
        },
      }
      const started = await options.client.command(startCommand)
      if (!('result' in started) || started.outcome !== 'completed'
        || started.command !== 'workrun.start') {
        throw new Error('控制平面未完成工作运行创建。')
      }
      const workRunId = started.result.activeWorkRunId
      if (workRunId === null || workRunId === undefined) {
        throw new Error('控制平面未返回活动工作运行。')
      }
      await options.workerSessions.launch({
        clientId: input.clientId,
        repositoryBindingId: input.repositoryBindingId,
        workRunId,
      })
      const aggregate = await queryWorkRunAggregate(options.client, {
        actor,
        scope,
        requestId: options.nextRequestId(),
        workItemId: taskId,
        deliveryId: context.deliveryId,
        workRunId,
      })
      if (!aggregate.runs.some(run => run.id === workRunId && run.workItemId === taskId)
        || !aggregate.deviceBindings.some(binding => binding.workRunId === workRunId)) {
        throw new Error('工作运行没有绑定到所选执行设备。')
      }
      const anchor = Object.freeze({ ...input, taskId })
      anchors.set(taskId, anchor)
      return anchor
    },
    describe(taskId: string): ControlPlaneTaskAnchor | null {
      return anchors.get(taskId) ?? null
    },
  })
}

export function createControlPlaneTaskFake(options?: {
  readonly nextTaskId?: () => import('./generated/contracts.js').WorkItemId
  readonly seed?: readonly ControlPlaneTaskAnchor[]
}): ControlPlaneTaskPort {
  const nextTaskId = options?.nextTaskId
  let sequence = 0
  const anchors = new Map<string, ControlPlaneTaskAnchor>()
  for (const anchor of options?.seed ?? []) anchors.set(anchor.taskId, anchor)
  return Object.freeze({
    async create(input: ControlPlaneTaskCreateInput): Promise<ControlPlaneTaskAnchor> {
      sequence += 1
      const taskId = nextTaskId !== undefined
        ? nextTaskId()
        : `wit_${String(sequence).padStart(26, '0')}` as import('./generated/contracts.js').WorkItemId
      if (!/^wit_[0-9A-HJKMNP-TV-Z]{26}$/u.test(taskId)) {
        throw new Error('task creation requires a canonical WorkItemId')
      }
      const anchor: ControlPlaneTaskAnchor = Object.freeze({
        taskId,
        clientId: input.clientId,
        repositoryBindingId: input.repositoryBindingId,
        baseBranch: input.baseBranch,
        description: input.description,
        modelRouteId: input.modelRouteId,
      })
      anchors.set(taskId, anchor)
      return anchor
    },
    describe(taskId: string): ControlPlaneTaskAnchor | null {
      return anchors.get(taskId) ?? null
    },
  })
}

export type ControlPlaneRunWorkerSessionState =
  | 'reserving'
  | 'launching'
  | 'running'
  | 'draining'
  | 'stopped'
  | 'failed'

export interface ControlPlaneRunIdentityProjection {
  readonly taskId: import('./generated/contracts.js').WorkItemId
  readonly deliveryId: DeliveryId | null
  readonly readCursor: StrongFlowReadCursor | null
  readonly clientId: string
  readonly repositoryBindingId: string
  readonly managedAppRunConfig: import('./generated/contracts.js').ManagedAppRunControlProjection | null
  readonly managedAppSourceRun: import('./generated/contracts.js').WorkRun | null
  readonly managedAppSourceEvidence?: readonly import('./generated/contracts.js').DeliveryEvidenceProjection[]
  readonly contract: import('./generated/contracts.js').WorkContract | null
  readonly item: import('./generated/contracts.js').WorkItem | null
  readonly graphItem: import('./generated/contracts.js').WorkGraphItemProjection | null
  readonly workRun: import('./generated/contracts.js').WorkRun
  readonly candidate: ControlPlaneRunCandidateProjection | null
  readonly owner: string | null
  readonly evidence: readonly import('./generated/contracts.js').DeliveryEvidenceProjection[]
}

export interface ControlPlaneRunCandidateProjection {
  readonly candidateId: import('./generated/contracts.js').CandidateId
  readonly candidateDigest: Sha256Digest
  readonly candidateRef: string
  readonly candidateCommitId: string
  readonly candidateTreeId: string
  readonly diffSha256: Sha256Digest
  readonly state: ControlPlaneCandidateState | 'produced'
  readonly branchName: string | null
  readonly history: readonly ControlPlaneCandidateApplyReceipt[]
}

function projectManagedAppRunConfig(
  config: import('./generated/contracts.js').ManagedAppRunControlProjection | undefined,
  sourceRun: import('./generated/contracts.js').WorkRun | undefined,
  taskId: import('./generated/contracts.js').WorkItemId,
  candidateCommitId: string | null,
): import('./generated/contracts.js').ManagedAppRunControlProjection | null {
  if (config === undefined || sourceRun === undefined
    || sourceRun.workItemId !== taskId
    || String(config.runId) !== sourceRun.id
    || config.attempt !== sourceRun.attempt) return null
  if (config.mode === 'frozen-candidate') {
    if (candidateCommitId === null || config.candidateCommit !== candidateCommitId) return null
  } else if (config.mode === 'live'
    && (config.candidateCommit !== null || candidateCommitId !== null)) {
    return null
  } else if (config.mode !== 'live') {
    return null
  }
  return Object.freeze({
    ...config,
  })
}

function canonicalCandidateObjectId(value: unknown, label: 'commit' | 'tree'): string {
  if (typeof value !== 'string' || !/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u.test(value)) {
    throw new Error(`控制平面返回的候选${label}身份无效。`)
  }
  return value
}

export interface ControlPlaneRunIdentityPort {
  read(anchor: ControlPlaneTaskAnchor): Promise<ControlPlaneRunIdentityProjection>
}

export function createControlPlaneRunIdentityPort(options: {
  readonly client: ControlPlaneClient
  readonly candidates: ControlPlaneClientCandidates
  readonly actor: () => Actor | null
  readonly scope: () => RepositoryScope | null
  readonly deliveryId: () => DeliveryId | null
  readonly nextRequestId: () => RequestId
}): ControlPlaneRunIdentityPort {
  return Object.freeze({
    async read(anchor: ControlPlaneTaskAnchor): Promise<ControlPlaneRunIdentityProjection> {
      const actor = options.actor()
      const scope = options.scope()
      const deliveryId = options.deliveryId()
      if (actor === null || scope === null || deliveryId === null) {
        throw new Error('读取工作运行前请登录、选择仓库范围并打开交付。')
      }
      const detail = await queryDeliveryDetail(options.client, {
        actor,
        scope,
        requestId: options.nextRequestId(),
        deliveryId,
      })
      const aggregate = await queryWorkRunAggregate(options.client, {
        actor,
        scope,
        requestId: options.nextRequestId(),
        deliveryId,
        workItemId: anchor.taskId,
        atCursor: detail.readCursor,
      })
      const producerWorkRunId = detail.currentCandidate?.producerWorkRunId ?? null
      const workRun = aggregate.runs
        .filter(run => run.workItemId === anchor.taskId
          && (producerWorkRunId === null || run.id === producerWorkRunId))
        .sort((left, right) => right.attempt - left.attempt || right.revision - left.revision)[0]
      if (workRun === undefined) {
        throw new Error(producerWorkRunId === null
          ? '此交付没有该任务项的工作运行。'
          : '当前候选的生产工作运行不存在。')
      }
      const binding = aggregate.deviceBindings.find(candidate => candidate.workRunId === workRun.id)
      if (binding === undefined) throw new Error('此工作运行尚未分配执行设备。')
      const currentCandidate = detail.currentCandidate
      let candidate: ControlPlaneRunCandidateProjection | null = null
      if (currentCandidate !== null) {
        const candidateIdentity = {
          candidateCommitId: canonicalCandidateObjectId(currentCandidate.candidateCommitId, 'commit'),
          candidateTreeId: canonicalCandidateObjectId(currentCandidate.candidateTreeId, 'tree'),
        }
        candidate = Object.freeze({
          candidateRef: currentCandidate.candidateRef,
          candidateId: currentCandidate.candidateId,
          candidateDigest: currentCandidate.candidateDigest,
          candidateCommitId: candidateIdentity.candidateCommitId,
          candidateTreeId: candidateIdentity.candidateTreeId,
          diffSha256: currentCandidate.diffSha256,
          state: 'produced',
          branchName: null,
          history: Object.freeze([]),
        })
        const deviceCandidates = await options.candidates.listDeviceCandidates({
          clientId: binding.clientId,
        })
        const deviceCandidate = deviceCandidates.find(
          item => item.candidateRef === currentCandidate.candidateRef,
        )
        if (deviceCandidate !== undefined) {
          if (deviceCandidate.repositoryBindingId !== binding.repositoryBindingId
            || deviceCandidate.candidateCommit !== candidateIdentity.candidateCommitId) {
            throw new Error('设备候选与当前工作运行不匹配。')
          }
          candidate = Object.freeze({
            candidateRef: currentCandidate.candidateRef,
          candidateId: currentCandidate.candidateId,
          candidateDigest: currentCandidate.candidateDigest,
            candidateCommitId: candidateIdentity.candidateCommitId,
            candidateTreeId: candidateIdentity.candidateTreeId,
            diffSha256: currentCandidate.diffSha256,
            state: deviceCandidate.state,
            branchName: deviceCandidate.branchName,
            history: deviceCandidate.history,
          })
        }
      }
      const managedAppSource = (() => {
        const candidateCommitId = currentCandidate === null ? null : candidate?.candidateCommitId ?? null
        const candidates = aggregate.managedAppRunConfigs
          .map(config => {
            const sourceRun = aggregate.runs.find(run => run.id === config.runId)
            const projected = projectManagedAppRunConfig(config, sourceRun, anchor.taskId, candidateCommitId)
            if (projected === null || sourceRun === undefined) return null
            const sourceBinding = aggregate.deviceBindings.find(item => item.workRunId === sourceRun.id)
            if (sourceBinding === undefined
              || sourceBinding.clientId !== binding.clientId
              || sourceBinding.repositoryBindingId !== binding.repositoryBindingId
              || sourceRun.id === currentCandidate?.producerWorkRunId) return null
            return Object.freeze({ config: projected, sourceRun })
          })
          .filter((entry): entry is {
            readonly config: import('./generated/contracts.js').ManagedAppRunControlProjection
            readonly sourceRun: import('./generated/contracts.js').WorkRun
          } => entry !== null)
        return candidates.length === 1 ? (candidates[0] ?? null) : null
      })()
      return Object.freeze({
        taskId: anchor.taskId,
        deliveryId,
        readCursor: detail.readCursor,
        clientId: binding.clientId,
        repositoryBindingId: binding.repositoryBindingId,
        managedAppRunConfig: managedAppSource?.config ?? null,
        managedAppSourceRun: managedAppSource?.sourceRun ?? null,
        managedAppSourceEvidence: managedAppSource === null
          ? Object.freeze([])
          : Object.freeze(detail.evidence.filter(entry => entry.workRunId === managedAppSource.sourceRun.id)),
        contract: aggregate.contract,
        item: aggregate.items.find(item => item.id === anchor.taskId) ?? null,
        graphItem: aggregate.graphItems.find(item => item.workItemId === anchor.taskId) ?? null,
        workRun,
        candidate,
        owner: null,
        evidence: Object.freeze(detail.evidence.filter(entry => entry.workRunId === workRun.id)),
      })
    },
  })
}

export function createControlPlaneRunIdentityFake(options?: {
  readonly managedAppRunConfig?: import('./generated/contracts.js').ManagedAppRunControlProjection
}): ControlPlaneRunIdentityPort {
  return Object.freeze({
    async read(
      anchor: ControlPlaneTaskAnchor,
    ): Promise<ControlPlaneRunIdentityProjection> {
      const suffix = anchor.taskId.slice(4) || '00000000000000000000000000'
      const workItemId = `wit_${suffix}` as import('./generated/contracts.js').WorkItemId
      const workRun = Object.freeze({
        schemaVersion: 'winwincode/v1',
        id: `wrn_${suffix}`,
        workContractId: `wct_${suffix}`,
        contractRevision: 1,
        workItemId,
        workItemRevision: 1,
        revision: 1,
        state: 'running',
        executionJobId: `job_${suffix}`,
        attempt: 1,
        workerId: `wrk_${suffix}`,
        workerInstanceId: `wki_${suffix}`,
        workerSessionId: `wsn_${suffix}`,
        leaseId: `lse_${suffix}`,
        fencingToken: '1',
        productSessionId: `psn_${suffix}`,
        codexThreadId: null,
        candidateDigest: null,
      }) as import('./generated/contracts.js').WorkRun
      const managedAppRunConfig = projectManagedAppRunConfig(
        options?.managedAppRunConfig,
        workRun,
        workItemId,
        null,
      )
      return Object.freeze({
        taskId: anchor.taskId,
        deliveryId: null,
        readCursor: null,
        clientId: anchor.clientId,
        repositoryBindingId: anchor.repositoryBindingId,
        managedAppRunConfig,
        managedAppSourceRun: managedAppRunConfig === null ? null : workRun,
        managedAppSourceEvidence: Object.freeze([]),
        contract: null,
        item: null,
        graphItem: null,
        workRun,
        candidate: null,
        owner: null,
        evidence: Object.freeze([]),
      })
    },
  })
}
