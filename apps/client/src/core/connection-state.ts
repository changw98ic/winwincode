// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
  type ControlPlaneClientErrorKind,
  type ControlPlaneWebSocketAuthorizationRevokedFrame,
  type ControlPlaneSubscribeOptions,
  type ControlPlaneSubscription,
  type RequestId,
} from '../control-plane-client.js'

export type GlobalConnectionStatus =
  | 'connected'
  | 'reconnecting'
  | 'offline'
  | 'refresh-required'
  | 'authentication-required'
  | 'permission-denied'
  | 'version-mismatch'

export interface ConnectionSnapshot {
  readonly status: GlobalConnectionStatus
  readonly code: string | null
  readonly requestId: RequestId | null
  readonly lastSuccessfulAt: string | null
  readonly revision: number
}

export type ClientFailureCategory =
  | 'authentication'
  | 'permission'
  | 'version'
  | 'network'
  | 'configuration'
  | 'server'
  | 'protocol'
  | 'cancelled'
  | 'client'

export interface ClientFailure {
  readonly category: ClientFailureCategory
  readonly code: string
  readonly requestId: RequestId | null
  readonly retryable: boolean
  readonly connectionStatus: GlobalConnectionStatus
  readonly title: string
  readonly message: string
  readonly recoveryLabel: string
}

export interface ConnectionMonitorOptions {
  readonly now?: () => string
}

export interface ConnectionMonitor {
  readonly state: ConnectionSnapshot
  subscribe(listener: (state: ConnectionSnapshot) => void): () => void
  connected(requestId?: RequestId | null): void
  reconnecting(code?: string, requestId?: RequestId | null): void
  offline(code?: string, requestId?: RequestId | null): void
  refreshRequired(code?: string, requestId?: RequestId | null): void
  authenticationRequired(code?: string, requestId?: RequestId | null): void
  permissionDenied(code?: string, requestId?: RequestId | null): void
  versionMismatch(code?: string, requestId?: RequestId | null): void
  failure(error: unknown, online: boolean): void
  reset(): void
  close(): void
}

const PUBLIC_CODES = new Set([
  'APPROVAL_DENIED',
  'APPROVALS_ROUTE_FAILURE',
  'ARTIFACT_DIGEST_MISMATCH',
  'AUTHENTICATION_REQUIRED',
  'AUTH_SESSION_FAILED',
  'BOOTSTRAP_PROOF_INVALID',
  'CANCELLED',
  'CANDIDATE_STALE',
  'CAPABILITY_MISMATCH',
  'CHAT_ROUTE_FAILURE',
  'CLIENT_ASYNC_FAILURE',
  'CLIENT_CLOSED',
  'CLIENT_FAILURE',
  'CLIENT_OPERATION_FAILURE',
  'CLIENT_RENDER_FAILURE',
  'CLIENT_ROUTE_FAILURE',
  'CONNECTING',
  'ENTERPRISE_ROUTE_FAILURE',
  'EXECUTION_FAILED',
  'IDEMPOTENCY_CONFLICT',
  'INFRASTRUCTURE_ERROR',
  'INPUT_CLOSED',
  'INTERNAL_ERROR',
  'INVALID_AUTH_SESSION_RESPONSE',
  'INVALID_REQUEST',
  'INVALID_RESPONSE',
  'JOB_DISPATCH_CONFLICT',
  'LEASE_EXPIRED',
  'LOCAL_OPERATIONS_ROUTE_FAILURE',
  'MESSAGE_CONFLICT',
  'MODEL_STREAM_FAILED',
  'NETWORK_ERROR',
  'OFFLINE',
  'PERMISSION_DENIED',
  'PROTOCOL_VERSION_UNSUPPORTED',
  'RATE_LIMITED',
  'READ_CURSOR_EXPIRED',
  'RECONNECTING',
  'REFRESH_REQUIRED',
  'REQUEST_CANCELLED',
  'RESET_FAILED',
  'RESOURCE_NOT_FOUND',
  'REVISION_CONFLICT',
  'SCHEMA_VERSION_MISMATCH',
  'SCHEMA_VERSION_UNSUPPORTED',
  'SEQUENCE_GAP',
  'SERVICE_UNAVAILABLE',
  'SETTINGS_ROUTE_FAILURE',
  'STALE_FENCING_TOKEN',
  'STRONGFLOW_ROUTE_FAILURE',
  'SUBSCRIPTION_RESET_REQUIRED',
  'TRANSPORT_UNAVAILABLE',
  'TRUSTED_FACTS_UNAVAILABLE',
  'VERSION_MISMATCH',
  'WORKER_INSTANCE_CHANGED',
  'WORKER_NOT_REGISTERED',
  'WRONG_STATE',
])
const SAFE_REQUEST_ID = /^req_[0-9A-HJKMNP-TV-Z]{26}$/u
const RFC3339_INSTANT = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z$/u
const PUBLIC_SCOPE_KINDS = new Set(['organization', 'workspace', 'project', 'repository'])

function safeCode(value: unknown, fallback: string): string {
  if (typeof value === 'string' && PUBLIC_CODES.has(value)) return value
  return PUBLIC_CODES.has(fallback) ? fallback : 'CLIENT_FAILURE'
}

function safeRequestId(value: unknown): RequestId | null {
  return typeof value === 'string' && SAFE_REQUEST_ID.test(value)
    ? value as RequestId
    : null
}

function safeTimestamp(value: unknown): string {
  return typeof value === 'string'
    && RFC3339_INSTANT.test(value)
    && !Number.isNaN(Date.parse(value))
    ? value
    : 'not available'
}

function categoryForKind(kind: ControlPlaneClientErrorKind): ClientFailureCategory {
  if (kind === 'authorization') return 'permission'
  return kind
}

function connectionForKind(
  kind: ControlPlaneClientErrorKind,
  online: boolean,
): GlobalConnectionStatus {
  if (kind === 'authentication') return 'authentication-required'
  if (kind === 'authorization') return 'permission-denied'
  if (kind === 'version') return 'version-mismatch'
  if (kind === 'network') return online ? 'reconnecting' : 'offline'
  return kind === 'cancelled' ? 'reconnecting' : 'refresh-required'
}

const FAILURE_COPY: Readonly<Record<ClientFailureCategory, Readonly<{
  title: string
  message: string
  recoveryLabel: string
}>>> = Object.freeze({
  authentication: Object.freeze({
    title: 'Session expired',
    message: 'Sign in again to restore access. Unsaved fields remain in this browser view.',
    recoveryLabel: 'Sign in again',
  }),
  permission: Object.freeze({
    title: 'Access changed',
    message: 'The current identity no longer has access to this area. Return to a permitted area.',
    recoveryLabel: 'Return to Chat',
  }),
  version: Object.freeze({
    title: 'Client and Server versions differ',
    message: 'Update the Client before retrying this Server operation.',
    recoveryLabel: 'Return to Chat',
  }),
  network: Object.freeze({
    title: 'Connection interrupted',
    message: 'The current view is preserved while the Client reconnects.',
    recoveryLabel: 'Reconnect',
  }),
  configuration: Object.freeze({
    title: 'Client configuration needs attention',
    message: 'Review the local runtime configuration before retrying.',
    recoveryLabel: 'Retry route',
  }),
  server: Object.freeze({
    title: 'Server operation failed',
    message: 'Retry this route. If it fails again, copy the diagnostic summary.',
    recoveryLabel: 'Retry route',
  }),
  protocol: Object.freeze({
    title: 'Server response needs a full refresh',
    message: 'Reload this route from the current Server snapshot.',
    recoveryLabel: 'Refresh route',
  }),
  cancelled: Object.freeze({
    title: 'Operation cancelled',
    message: 'The route changed before this operation finished.',
    recoveryLabel: 'Retry route',
  }),
  client: Object.freeze({
    title: 'This area stopped unexpectedly',
    message: 'Retry this route or return to Chat. Diagnostic details exclude request payloads and local paths.',
    recoveryLabel: 'Retry route',
  }),
})

export function classifyClientFailure(
  error: unknown,
  fallbackCode: string,
  online: boolean,
): ClientFailure {
  const normalizedFallback = safeCode(fallbackCode, 'CLIENT_FAILURE')
  if (!(error instanceof ControlPlaneClientError)) {
    const copy = FAILURE_COPY.client
    return Object.freeze({
      category: 'client',
      code: normalizedFallback,
      requestId: null,
      retryable: true,
      connectionStatus: 'refresh-required',
      ...copy,
    })
  }
  const category = categoryForKind(error.kind)
  return Object.freeze({
    category,
    code: safeCode(error.code, normalizedFallback),
    requestId: safeRequestId(error.requestId),
    retryable: error.retryable,
    connectionStatus: connectionForKind(error.kind, online),
    ...FAILURE_COPY[category],
  })
}

function snapshot(
  status: GlobalConnectionStatus,
  prior: ConnectionSnapshot | null,
  code: string | null = null,
  requestId: RequestId | null = null,
  lastSuccessfulAt: string | null = prior?.lastSuccessfulAt ?? null,
): ConnectionSnapshot {
  return Object.freeze({
    status,
    code,
    requestId,
    lastSuccessfulAt,
    revision: (prior?.revision ?? 0) + 1,
  })
}

export function createConnectionMonitor(
  options: ConnectionMonitorOptions = {},
): ConnectionMonitor {
  const now = options.now ?? (() => new Date().toISOString())
  const listeners = new Set<(state: ConnectionSnapshot) => void>()
  let current = snapshot('reconnecting', null, 'CONNECTING')
  let closed = false

  function publish(next: ConnectionSnapshot): void {
    if (closed) return
    current = next
    for (const listener of listeners) listener(current)
  }

  function transition(
    status: GlobalConnectionStatus,
    code: string,
    requestId: RequestId | null = null,
  ): void {
    publish(snapshot(status, current, safeCode(code, 'CLIENT_FAILURE'), safeRequestId(requestId)))
  }

  return {
    get state() { return current },
    subscribe(listener) {
      if (closed) throw new Error('Connection monitor is closed.')
      listeners.add(listener)
      listener(current)
      return () => { listeners.delete(listener) }
    },
    connected(requestId = null) {
      if (
        current.status === 'authentication-required'
        || current.status === 'permission-denied'
        || current.status === 'version-mismatch'
      ) return
      publish(snapshot('connected', current, null, safeRequestId(requestId), now()))
    },
    reconnecting(code = 'RECONNECTING', requestId = null) {
      transition('reconnecting', code, requestId)
    },
    offline(code = 'OFFLINE', requestId = null) {
      transition('offline', code, requestId)
    },
    refreshRequired(code = 'REFRESH_REQUIRED', requestId = null) {
      transition('refresh-required', code, requestId)
    },
    authenticationRequired(code = 'AUTHENTICATION_REQUIRED', requestId = null) {
      transition('authentication-required', code, requestId)
    },
    permissionDenied(code = 'PERMISSION_DENIED', requestId = null) {
      transition('permission-denied', code, requestId)
    },
    versionMismatch(code = 'VERSION_MISMATCH', requestId = null) {
      transition('version-mismatch', code, requestId)
    },
    failure(error, online) {
      const failure = classifyClientFailure(error, 'CLIENT_OPERATION_FAILURE', online)
      if (failure.category === 'cancelled') return
      transition(failure.connectionStatus, failure.code, failure.requestId)
    },
    reset() {
      publish(snapshot('reconnecting', current, 'RECONNECTING'))
    },
    close() {
      if (closed) return
      closed = true
      listeners.clear()
    },
  }
}

function abbreviatedId(label: string, value: unknown): string | null {
  if (typeof value !== 'string' || !/^[a-z]{3}_[0-9A-HJKMNP-TV-Z]{26}$/u.test(value)) return null
  return `${label} …${value.slice(-6)}`
}

export function scopeSummary(scope: unknown): string {
  if (typeof scope !== 'object' || scope === null) return 'none'
  const record = scope as Readonly<Record<string, unknown>>
  const kind = typeof record.kind === 'string' && PUBLIC_SCOPE_KINDS.has(record.kind)
    ? record.kind
    : 'unknown'
  const ids = [
    abbreviatedId('org', record.organizationId),
    abbreviatedId('workspace', record.workspaceId),
    abbreviatedId('project', record.projectId),
    abbreviatedId('repository', record.repositoryId),
  ].filter((value): value is string => value !== null)
  return `${kind}:${ids.length === 0 ? 'none' : ids.join(' / ')}`
}

export interface SafeDiagnosticInput {
  readonly connection: ConnectionSnapshot
  readonly failure?: ClientFailure | null
  readonly scope: unknown
  readonly surface: string
  readonly generatedAt: string
}

export function createSafeDiagnostic(input: SafeDiagnosticInput): string {
  const surface = /^[a-z][a-z-]{0,31}$/u.test(input.surface) ? input.surface : 'unknown'
  const code = input.failure?.code ?? input.connection.code ?? 'NONE'
  const requestId = input.failure?.requestId ?? input.connection.requestId
  return [
    'WinWinCode Client diagnostic',
    `generatedAt=${safeTimestamp(input.generatedAt)}`,
    `surface=${surface}`,
    `connection=${input.connection.status}`,
    `code=${safeCode(code, 'CLIENT_FAILURE')}`,
    `requestId=${requestId ?? 'none'}`,
    `scope=${scopeSummary(input.scope)}`,
    `lastSuccessfulAt=${input.connection.lastSuccessfulAt === null
      ? 'none'
      : safeTimestamp(input.connection.lastSuccessfulAt)}`,
  ].join('\n')
}

export interface ObservedControlPlaneClient {
  readonly client: ControlPlaneClient
  reconnectAll(): void
}

export interface ObserveControlPlaneClientOptions {
  readonly client: ControlPlaneClient
  readonly monitor: ConnectionMonitor
  readonly online: () => boolean
  /** Called after the feature has handled and closed its revoked subscription. */
  readonly onAuthorizationRevoked?: (
    frame: ControlPlaneWebSocketAuthorizationRevokedFrame | null,
  ) => Promise<void> | void
}

export function observeControlPlaneClient(
  options: ObserveControlPlaneClientOptions,
): ObservedControlPlaneClient {
  const subscriptions = new Set<ControlPlaneSubscription>()

  async function observe<T>(
    operation: Promise<T>,
    requestId: RequestId | null = null,
  ): Promise<T> {
    try {
      const result = await operation
      options.monitor.connected(requestId)
      return result
    } catch (error) {
      options.monitor.failure(error, options.online())
      throw error
    }
  }

  const client: ControlPlaneClient = {
    serverUrl: options.client.serverUrl,
    restore(requestOptions) {
      return observe(options.client.restore(requestOptions))
    },
    login(bootstrapProof, requestOptions) {
      return observe(options.client.login(bootstrapProof, requestOptions))
    },
    logout(requestOptions) {
      return observe(options.client.logout(requestOptions))
    },
    command(command, requestOptions) {
      return observe(options.client.command(command, requestOptions), command.requestId)
    },
    query(query, requestOptions) {
      return observe(options.client.query(query, requestOptions), query.requestId)
    },
    subscribe(subscriptionOptions) {
      let active = true
      let raw: ControlPlaneSubscription
      const onResetRequired = subscriptionOptions.onResetRequired
      const observedOptions: ControlPlaneSubscribeOptions = {
        ...subscriptionOptions,
        async onEvent(event) {
          options.monitor.connected()
          await subscriptionOptions.onEvent(event)
        },
        ...(onResetRequired === undefined
          ? {}
          : {
              async onResetRequired(frame) {
                options.monitor.refreshRequired('SUBSCRIPTION_RESET_REQUIRED')
                return onResetRequired(frame)
              },
            }),
        async onAuthorizationRevoked(frame) {
          options.monitor.permissionDenied('PERMISSION_REVOKED')
          await subscriptionOptions.onAuthorizationRevoked?.(frame)
          await options.onAuthorizationRevoked?.(frame)
        },
        onError(error) {
          options.monitor.failure(error, options.online())
          subscriptionOptions.onError?.(error)
        },
      }
      raw = options.client.subscribe(observedOptions)
      const subscription: ControlPlaneSubscription = {
        get cursor() { return raw.cursor },
        resume() { raw.resume() },
        reconnect() {
          options.monitor.reconnecting()
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
    close() {
      for (const subscription of [...subscriptions]) subscription.close()
      options.client.close()
    },
  }

  return Object.freeze({
    client,
    reconnectAll() {
      options.monitor.reconnecting()
      for (const subscription of subscriptions) subscription.reconnect()
    },
  })
}
