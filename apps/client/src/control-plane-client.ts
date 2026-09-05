// SPDX-License-Identifier: Apache-2.0

const {
  ControlPlaneClientError: GeneratedControlPlaneClientError,
  createControlPlaneHttpClient,
  createControlPlaneWebSocketClient,
  matchesCanonicalSchema,
} = await import('./generated/control-plane-client.js')
const { ControlPlaneWebSocketSubscribeOrigin } = await import(
  './generated/contracts.js'
)

type ControlPlaneFetch = import(
  './generated/control-plane-client.js'
).ControlPlaneFetch
type ControlPlaneHttpRequestInit = import(
  './generated/control-plane-client.js'
).ControlPlaneHttpRequestInit
type ControlPlaneHttpResponse = import(
  './generated/control-plane-client.js'
).ControlPlaneHttpResponse
type ControlPlaneWebSocketAcknowledgedCursor = import(
  './generated/contracts.js'
).ControlPlaneWebSocketAcknowledgedCursor
type ControlPlaneWebSocketFactory = import(
  './generated/control-plane-client.js'
).ControlPlaneWebSocketFactory
export type CommandAcceptedResponse = import(
  './generated/contracts.js'
).CommandAcceptedResponse
export type CommandCompletedResponse = import(
  './generated/contracts.js'
).CommandCompletedResponse
export type CommandRequest = import('./generated/contracts.js').CommandRequest
export type AuthSessionResponse = import('./generated/contracts.js').AuthSessionResponse
export type ControlPlaneWebSocketAuthorizationRevokedFrame = import(
  './generated/contracts.js'
).ControlPlaneWebSocketAuthorizationRevokedFrame
export type ControlPlaneWebSocketEventFrame = import(
  './generated/contracts.js'
).ControlPlaneWebSocketEventFrame
export type ControlPlaneWebSocketSubscription = import(
  './generated/contracts.js'
).ControlPlaneWebSocketSubscription
export type ControlPlaneWebSocketSubscriptionId = import(
  './generated/contracts.js'
).ControlPlaneWebSocketSubscriptionId
export type ControlPlaneWebSocketSubscribeStartAt = import(
  './generated/contracts.js'
).ControlPlaneWebSocketSubscribeStartAt
export type ErrorDetails = import('./generated/contracts.js').ErrorDetails
export type EventReadCursor = import('./generated/contracts.js').EventReadCursor
export type QueryRequest = import('./generated/contracts.js').QueryRequest
export type QueryResultResponse = import('./generated/contracts.js').QueryResultResponse
export type RequestId = import('./generated/contracts.js').RequestId

const CONTROL_PLANE_SCHEMA_VERSION = 'winwincode/v1'
const DEFAULT_NETWORK_RETRIES = 2
const DEFAULT_RECONNECT_DELAY_MILLIS = 250
const AUTH_SESSION_PATH = '/api/v1/auth/session'
const SERVER_INITIALIZATION_PATH = '/api/v1/server/initialization'
const RFC3339_INSTANT = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z$/u

/**
 * The one place that names the sign-in wire failure codes of
 * `POST /api/v1/auth/session`. Pages and view-models only ever see the
 * `ControlPlaneLoginFailure` union below. The Server deliberately folds
 * disabled accounts into the same `AUTHENTICATION_REQUIRED` rejection as a
 * wrong password, so no separate disabled wire code exists.
 */
const LOGIN_AUTHENTICATION_REQUIRED_CODE = 'AUTHENTICATION_REQUIRED'
const LOGIN_RATE_LIMITED_CODE = 'RATE_LIMITED'

export type ControlPlaneClientErrorKind =
  | 'authentication'
  | 'authorization'
  | 'cancelled'
  | 'configuration'
  | 'network'
  | 'protocol'
  | 'server'
  | 'version'

export interface ControlPlaneClientErrorFields {
  readonly kind: ControlPlaneClientErrorKind
  readonly code: string
  readonly message: string
  readonly requestId: RequestId | null
  readonly retryable: boolean
  readonly details?: ErrorDetails
  readonly cause?: unknown
}

/** The one error shape exposed by the browser network boundary. */
export class ControlPlaneClientError extends Error {
  readonly kind: ControlPlaneClientErrorKind
  readonly code: string
  readonly requestId: RequestId | null
  readonly retryable: boolean
  readonly details: ErrorDetails

  constructor(fields: ControlPlaneClientErrorFields) {
    super(fields.message, fields.cause === undefined ? undefined : { cause: fields.cause })
    this.name = 'ControlPlaneClientError'
    this.kind = fields.kind
    this.code = fields.code
    this.requestId = fields.requestId
    this.retryable = fields.retryable
    this.details = fields.details ?? {}
  }
}

export interface ControlPlaneServerLocation {
  /** Normalized HTTP(S) base used by commands and queries. */
  readonly serverUrl: string
  /** Derived WS(S) base used by subscriptions. */
  readonly webSocketUrl: string
}

/**
 * Validate the sole runtime address before the page or a transport is started.
 * Paths are allowed for reverse-proxy deployments; credentials, query, and hash are not.
 */
export function parseControlPlaneServerUrl(value: unknown): ControlPlaneServerLocation {
  if (typeof value !== 'string' || value.trim().length === 0) {
    throw configurationError(
      'SERVER_URL_REQUIRED',
      'Control Plane serverUrl is required before the client can start.',
    )
  }
  if (value !== value.trim()) {
    throw configurationError(
      'SERVER_URL_INVALID',
      'Control Plane serverUrl must not contain leading or trailing whitespace.',
    )
  }
  let parsed: URL
  try {
    parsed = new URL(value)
  } catch (cause) {
    throw configurationError(
      'SERVER_URL_INVALID',
      'Control Plane serverUrl must be an absolute HTTP or HTTPS URL.',
      cause,
    )
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    throw configurationError(
      'SERVER_URL_INVALID_PROTOCOL',
      'Control Plane serverUrl must use HTTP or HTTPS.',
    )
  }
  if (parsed.username.length > 0 || parsed.password.length > 0) {
    throw configurationError(
      'SERVER_URL_CREDENTIALS_FORBIDDEN',
      'Control Plane serverUrl must not contain credentials.',
    )
  }
  if (parsed.search.length > 0 || parsed.hash.length > 0) {
    throw configurationError(
      'SERVER_URL_COMPONENTS_FORBIDDEN',
      'Control Plane serverUrl must not contain a query or fragment.',
    )
  }
  parsed.pathname = parsed.pathname.replace(/\/+$/u, '')
  const normalizedServerUrl = parsed.toString().replace(/\/$/u, '')
  const webSocket = new URL(normalizedServerUrl)
  webSocket.protocol = parsed.protocol === 'https:' ? 'wss:' : 'ws:'
  return Object.freeze({
    serverUrl: normalizedServerUrl,
    webSocketUrl: webSocket.toString().replace(/\/$/u, ''),
  })
}

export interface ControlPlaneTransportRequestInit {
  readonly method: 'DELETE' | 'GET' | 'POST'
  readonly headers: Readonly<Record<string, string>>
  readonly body?: string
  readonly redirect?: 'error'
  readonly cache?: 'no-store'
  readonly referrerPolicy?: 'no-referrer'
  /** Cross-origin Control Plane sessions use the same secure cookie authentication as WebSocket. */
  readonly credentials: 'include'
  readonly signal?: AbortSignal
}

export type ControlPlaneTransportFetch = (
  input: string,
  init: ControlPlaneTransportRequestInit,
) => Promise<ControlPlaneHttpResponse>

export interface ControlPlaneClientTransport {
  /** Deterministic injection seam; production defaults to the browser HTTP transport. */
  readonly fetch?: ControlPlaneTransportFetch
  /** Deterministic injection seam; production defaults to the browser WebSocket transport. */
  readonly createSocket?: ControlPlaneWebSocketFactory
}

export interface ControlPlaneClientOptions {
  /** The only runtime network address. HTTP and WebSocket endpoints are derived from it. */
  readonly serverUrl: string
  readonly maxNetworkRetries?: number
  readonly reconnectDelayMillis?: number
  readonly waitBeforeRetry?: (attempt: number) => Promise<void>
  readonly onAccessFailure?: (error: ControlPlaneClientError) => void
  readonly transport?: ControlPlaneClientTransport
}

export interface ControlPlaneRequestOptions {
  readonly signal?: AbortSignal
}

export type ControlPlaneSession = AuthSessionResponse

/** Username and password material for one sign-in attempt. */
export interface ControlPlanePasswordCredentials {
  readonly username: string
  readonly password: string
}

/** Whether the Server still accepts the one-time bootstrap initialization. */
export interface ControlPlaneInitializationStatus {
  readonly initialized: boolean
}

/**
 * The one presentation-facing sign-in failure taxonomy. Server wire codes are
 * translated by `controlPlaneLoginFailure` and never read anywhere else.
 * Disabled accounts share the wrong-password rejection on the wire, so no
 * separate disabled presentation state exists.
 */
export type ControlPlaneLoginFailure =
  | 'invalid-credentials'
  | 'rate-limited'
  | 'unavailable'

export interface ControlPlaneSubscribeOptions {
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly subscription: ControlPlaneWebSocketSubscription
  readonly startAt?: ControlPlaneWebSocketSubscribeStartAt
  readonly signal?: AbortSignal
  /** Synchronous observer for a validated event waiting in the ordered application queue. */
  readonly onEventQueued?: (event: ControlPlaneWebSocketEventFrame) => void
  readonly onEvent: (event: ControlPlaneWebSocketEventFrame) => Promise<void> | void
  readonly onResetRequired?: (
    frame: import('./generated/contracts.js').ControlPlaneWebSocketResetRequiredFrame
      | null,
  ) => Promise<EventReadCursor> | EventReadCursor
  readonly onAuthorizationRevoked?: (
    frame: ControlPlaneWebSocketAuthorizationRevokedFrame | null,
  ) => Promise<void> | void
  readonly onError?: (error: ControlPlaneClientError) => void
}

export interface ControlPlaneSubscription {
  readonly cursor: ControlPlaneWebSocketAcknowledgedCursor | null
  resume(): void
  reconnect(): void
  close(): void
}

export interface ControlPlaneClient {
  readonly serverUrl: string
  /** Restore the secret-free identity and authorized scopes from the HttpOnly cookie. */
  restore(options?: ControlPlaneRequestOptions): Promise<ControlPlaneSession>
  login(
    bootstrapProof: string,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneSession>
  /** Exchange a username and password for one browser session. */
  loginWithPassword(
    credentials: ControlPlanePasswordCredentials,
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneSession>
  /** Read whether the Server still shows the first-time initialization entry. */
  initializationStatus(
    options?: ControlPlaneRequestOptions,
  ): Promise<ControlPlaneInitializationStatus>
  logout(options?: ControlPlaneRequestOptions): Promise<void>
  command(
    command: CommandRequest,
    options?: ControlPlaneRequestOptions,
  ): Promise<CommandAcceptedResponse | CommandCompletedResponse>
  query(query: QueryRequest, options?: ControlPlaneRequestOptions): Promise<QueryResultResponse>
  subscribe(options: ControlPlaneSubscribeOptions): ControlPlaneSubscription
  close(): void
}

function configurationError(code: string, message: string, cause?: unknown): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'configuration',
    code,
    message,
    requestId: null,
    retryable: false,
    ...(cause === undefined ? {} : { cause }),
  })
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

function signalIsAborted(signal: AbortSignal | undefined): boolean {
  return signal?.aborted ?? false
}

function isRecord(value: unknown): value is Readonly<Record<string, unknown>> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function sessionBoundaryError(status: number, source: string): ControlPlaneClientError {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    return new ControlPlaneClientError({
      kind: 'protocol',
      code: 'INVALID_AUTH_SESSION_RESPONSE',
      message: 'The authentication server returned an invalid response.',
      requestId: null,
      retryable: false,
    })
  }
  const error = isRecord(value) && isRecord(value.error) ? value.error : null
  const details = error !== null && isRecord(error.details)
    ? error.details as ErrorDetails
    : {}
  const code = error !== null && typeof error.code === 'string'
    ? error.code
    : (status === 401 ? 'AUTHENTICATION_REQUIRED' : 'AUTH_SESSION_FAILED')
  const kind = accessKind(code)
    ?? (versionCode(code) || versionDetails(details)
      ? 'version'
      : (status >= 500 ? 'server' : 'protocol'))
  const requestId = isRecord(value) && typeof value.requestId === 'string'
    ? value.requestId as RequestId
    : null
  return new ControlPlaneClientError({
    kind,
    code,
    message: error !== null && typeof error.message === 'string'
      ? error.message
      : 'The browser session request failed.',
    requestId,
    retryable: error !== null && error.retryable === true,
    details,
  })
}

function sessionResponse(source: string): ControlPlaneSession {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  const wrongVersion = isRecord(value)
    && typeof value.schemaVersion === 'string'
    && value.schemaVersion !== CONTROL_PLANE_SCHEMA_VERSION
  if (
    !matchesCanonicalSchema('AuthSessionResponse', value)
    || !isRecord(value)
    || typeof value.expiresAt !== 'string'
    || !RFC3339_INSTANT.test(value.expiresAt)
    || Number.isNaN(Date.parse(value.expiresAt))
  ) {
    throw new ControlPlaneClientError({
      kind: wrongVersion ? 'version' : 'protocol',
      code: wrongVersion ? 'SCHEMA_VERSION_MISMATCH' : 'INVALID_AUTH_SESSION_RESPONSE',
      message: 'The authentication server returned an invalid session response.',
      requestId: null,
      retryable: false,
    })
  }
  const response = value as AuthSessionResponse
  return Object.freeze({
    schemaVersion: response.schemaVersion,
    expiresAt: response.expiresAt,
    actor: Object.freeze({ ...response.actor }),
    authorizedScopes: Object.freeze(response.authorizedScopes.map(scope => Object.freeze({
      ...scope,
    }))),
  })
}

function accessKind(code: string): ControlPlaneClientErrorKind | null {
  if (code === 'AUTHENTICATION_REQUIRED') return 'authentication'
  if (code === 'PERMISSION_DENIED') return 'authorization'
  return null
}

/**
 * Validate sign-in input before a request exists. The password length bound
 * mirrors the bootstrap proof bound; neither the username nor the password is
 * ever copied into an error message.
 */
function assertLoginCredentials(credentials: ControlPlanePasswordCredentials): void {
  const username = typeof credentials?.username === 'string' ? credentials.username : ''
  const password = typeof credentials?.password === 'string' ? credentials.password : ''
  if (
    username.length === 0
    || username.length > 128
    || /\s/u.test(username)
    || password.length === 0
    || password.length > 4096
  ) {
    throw new ControlPlaneClientError({
      kind: 'authentication',
      code: 'LOGIN_INPUT_INVALID',
      message: 'Enter a valid username and password.',
      requestId: null,
      retryable: false,
    })
  }
}

function loginBoundaryError(status: number, source: string): ControlPlaneClientError {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  const error = isRecord(value) && isRecord(value.error) ? value.error : null
  const code = error !== null && typeof error.code === 'string'
    ? error.code
    : (status === 401 ? LOGIN_AUTHENTICATION_REQUIRED_CODE : 'AUTH_SESSION_FAILED')
  const kind: ControlPlaneClientErrorKind = code === LOGIN_AUTHENTICATION_REQUIRED_CODE
    ? 'authentication'
    : (code === LOGIN_RATE_LIMITED_CODE || status >= 500 ? 'server' : 'protocol')
  return new ControlPlaneClientError({
    kind,
    code,
    message: error !== null && typeof error.message === 'string'
      ? error.message
      : 'The sign-in request failed.',
    requestId: isRecord(value) && typeof value.requestId === 'string'
      ? value.requestId as RequestId
      : null,
    retryable: error !== null && error.retryable === true,
  })
}

/**
 * Translate one sign-in failure into the presentation taxonomy. Every wire
 * code stays inside this function; view-models and pages branch only on the
 * returned union. A 401 `AUTHENTICATION_REQUIRED` rejection covers wrong
 * credentials and disabled accounts alike (the Server does not distinguish
 * them), and every other failure — outage, protocol drift, wrong state — is
 * presented as an unavailable sign-in.
 */
export function controlPlaneLoginFailure(error: unknown): ControlPlaneLoginFailure {
  if (error instanceof ControlPlaneClientError) {
    if (error.code === LOGIN_RATE_LIMITED_CODE) return 'rate-limited'
    if (error.kind === 'authentication') return 'invalid-credentials'
  }
  return 'unavailable'
}

function initializationBoundaryError(
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
  return new ControlPlaneClientError({
    kind: status >= 500 ? 'server' : 'protocol',
    code: error !== null && typeof error.code === 'string'
      ? error.code
      : 'SERVER_INITIALIZATION_UNAVAILABLE',
    message: error !== null && typeof error.message === 'string'
      ? error.message
      : 'The Control Plane initialization status is unavailable.',
    requestId: isRecord(value) && typeof value.requestId === 'string'
      ? value.requestId as RequestId
      : null,
    retryable: error !== null && error.retryable === true,
  })
}

function initializationResponse(source: string): ControlPlaneInitializationStatus {
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
  if (!isRecord(value) || typeof value.initialized !== 'boolean') {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'INVALID_SERVER_INITIALIZATION_RESPONSE',
      message: 'The Control Plane server returned an invalid initialization status.',
      requestId: null,
      retryable: false,
    })
  }
  return Object.freeze({ initialized: value.initialized })
}

function versionCode(code: string): boolean {
  return code === 'SCHEMA_VERSION_MISMATCH'
    || code === 'PROTOCOL_VERSION_UNSUPPORTED'
    || code === 'VERSION_MISMATCH'
}

function versionDetails(details: ErrorDetails): boolean {
  return Reflect.get(details, 'reason') === 'CLIENT_UPGRADE_REQUIRED'
}

function normalizeError(error: unknown, requestId: RequestId | null): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) return error
  if (error instanceof GeneratedControlPlaneClientError) {
    const kind = accessKind(error.code)
      ?? (versionCode(error.code) || versionDetails(error.details)
        ? 'version'
        : (error.code === 'NETWORK_ERROR'
          ? 'network'
          : (error.code.startsWith('INVALID_')
            || error.code === 'RESET_FAILED'
            || error.code === 'TRANSPORT_UNAVAILABLE'
            ? 'protocol'
            : 'server')))
    return new ControlPlaneClientError({
      kind,
      code: error.code,
      message: error.message,
      requestId: error.requestId,
      retryable: error.retryable,
      details: error.details,
      cause: error,
    })
  }
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'CLIENT_FAILURE',
    message: 'The Control Plane client operation failed.',
    requestId,
    retryable: false,
    cause: error,
  })
}

function requestIdentity(value: CommandRequest | QueryRequest): RequestId | null {
  const candidate = Reflect.get(value, 'requestId')
  return typeof candidate === 'string' ? candidate as RequestId : null
}

function assertRequestVersion(value: CommandRequest | QueryRequest): void {
  if (Reflect.get(value, 'schemaVersion') === CONTROL_PLANE_SCHEMA_VERSION) return
  throw new ControlPlaneClientError({
    kind: 'version',
    code: 'SCHEMA_VERSION_MISMATCH',
    message: `Control Plane requests must use ${CONTROL_PLANE_SCHEMA_VERSION}.`,
    requestId: requestIdentity(value),
    retryable: false,
  })
}

function versionCheckedResponse(
  response: ControlPlaneHttpResponse,
  source: string,
  requestId: RequestId,
): ControlPlaneHttpResponse {
  try {
    const value: unknown = JSON.parse(source)
    if (
      value !== null
      && typeof value === 'object'
      && typeof Reflect.get(value, 'schemaVersion') === 'string'
      && Reflect.get(value, 'schemaVersion') !== CONTROL_PLANE_SCHEMA_VERSION
    ) {
      throw new GeneratedControlPlaneClientError({
        code: 'SCHEMA_VERSION_MISMATCH',
        message: `The Control Plane server must use ${CONTROL_PLANE_SCHEMA_VERSION}.`,
        requestId,
        retryable: false,
        details: {},
      })
    }
  } catch (error) {
    if (error instanceof GeneratedControlPlaneClientError) throw error
  }
  return {
    ok: response.ok,
    status: response.status,
    async text() {
      return source
    },
  }
}

/** Create the only browser entry to Control Plane commands, queries, and subscriptions. */
export function createControlPlaneClient(options: ControlPlaneClientOptions): ControlPlaneClient {
  const location = parseControlPlaneServerUrl(options.serverUrl)
  const maximumRetries = options.maxNetworkRetries ?? DEFAULT_NETWORK_RETRIES
  const reconnectDelayMillis = options.reconnectDelayMillis ?? DEFAULT_RECONNECT_DELAY_MILLIS
  const transportFetch = options.transport?.fetch
  const waitBeforeRetry = options.waitBeforeRetry ?? (async () => {})
  const subscriptions = new Set<ControlPlaneSubscription>()
  let closed = false

  function requireOpen(requestId: RequestId | null = null): void {
    if (!closed) return
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'CLIENT_CLOSED',
      message: 'The Control Plane client is closed.',
      requestId,
      retryable: false,
    })
  }

  function reportAccessFailure(error: ControlPlaneClientError): void {
    if (error.kind !== 'authentication' && error.kind !== 'authorization') return
    try {
      options.onAccessFailure?.(error)
    } catch {
      // Authentication UI failures must not replace the canonical server error.
    }
  }

  function throwNormalized(error: unknown, requestId: RequestId | null): never {
    const normalized = normalizeError(error, requestId)
    reportAccessFailure(normalized)
    throw normalized
  }

  function transportRequest(
    input: string,
    init: Omit<ControlPlaneTransportRequestInit, 'credentials' | 'signal'>,
    signal: AbortSignal | undefined,
  ): Promise<ControlPlaneHttpResponse> {
    if (transportFetch === undefined) {
      throw new GeneratedControlPlaneClientError({
        code: 'TRANSPORT_UNAVAILABLE',
        message: 'The browser HTTP transport is unavailable.',
        requestId: null,
        retryable: false,
        details: {},
      })
    }
    return transportFetch(input, {
      ...init,
      credentials: 'include',
      ...(signal === undefined ? {} : { signal }),
    })
  }

  async function authSessionRequest(
    method: 'DELETE' | 'GET' | 'POST',
    bootstrapProof: string | null,
    requestOptions: ControlPlaneRequestOptions | undefined,
  ): Promise<ControlPlaneSession | null> {
    requireOpen()
    if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
    if (
      bootstrapProof !== null
      && (
        bootstrapProof.length === 0
        || bootstrapProof.length > 4096
        || /\s/u.test(bootstrapProof)
      )
    ) {
      throw new ControlPlaneClientError({
        kind: 'authentication',
        code: 'BOOTSTRAP_PROOF_INVALID',
        message: 'Enter a valid bootstrap proof.',
        requestId: null,
        retryable: false,
      })
    }
    try {
      const response = await transportRequest(
        `${location.serverUrl}${AUTH_SESSION_PATH}`,
        {
          method,
          headers: {
            ...(method === 'GET' ? {} : { 'Content-Type': 'application/json' }),
            ...(bootstrapProof === null ? {} : { Authorization: `Bearer ${bootstrapProof}` }),
          },
          ...(method === 'GET'
            ? {}
            : { body: JSON.stringify({ schemaVersion: CONTROL_PLANE_SCHEMA_VERSION }) }),
          redirect: 'error',
          cache: 'no-store',
          referrerPolicy: 'no-referrer',
        },
        requestOptions?.signal,
      )
      const source = await response.text()
      if (!response.ok) throw sessionBoundaryError(response.status, source)
      if (method === 'DELETE') {
        if (response.status === 204 && source.length === 0) return null
        throw new ControlPlaneClientError({
          kind: 'protocol',
          code: 'INVALID_AUTH_SESSION_RESPONSE',
          message: 'The authentication server returned an invalid logout response.',
          requestId: null,
          retryable: false,
        })
      }
      const expectedStatus = method === 'GET' ? 200 : 201
      if (response.status !== expectedStatus) throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'INVALID_AUTH_SESSION_RESPONSE',
        message: 'The authentication server returned an invalid session response.',
        requestId: null,
        retryable: false,
      })
      return sessionResponse(source)
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      const normalized = error instanceof ControlPlaneClientError
        ? error
        : new ControlPlaneClientError({
          kind: 'network',
          code: 'NETWORK_ERROR',
          message: 'The authentication server could not be reached.',
          requestId: null,
          retryable: true,
        })
      reportAccessFailure(normalized)
      throw normalized
    }
  }

  function requestFetch(signal: AbortSignal | undefined, requestId: RequestId): ControlPlaneFetch {
    return async (input: string, init: ControlPlaneHttpRequestInit) => {
      if (signalIsAborted(signal)) throw new GeneratedControlPlaneClientError({
        code: 'REQUEST_CANCELLED',
        message: 'The Control Plane operation was cancelled.',
        requestId,
        retryable: false,
        details: {},
      })
      let response: ControlPlaneHttpResponse
      try {
        response = await transportRequest(input, {
          method: init.method,
          headers: init.headers,
          body: init.body,
        }, signal)
      } catch (error) {
        if (signalIsAborted(signal)) throw new GeneratedControlPlaneClientError({
          code: 'REQUEST_CANCELLED',
          message: 'The Control Plane operation was cancelled.',
          requestId,
          retryable: false,
          details: {},
        })
        throw error
      }
      const source = await response.text()
      return versionCheckedResponse(response, source, requestId)
    }
  }

  async function execute<Request extends CommandRequest | QueryRequest, Result>(
    request: Request,
    requestOptions: ControlPlaneRequestOptions | undefined,
    invoke: (fetchImplementation: ControlPlaneFetch) => Promise<Result>,
  ): Promise<Result> {
    const requestId = requestIdentity(request)
    requireOpen(requestId)
    assertRequestVersion(request)
    if (signalIsAborted(requestOptions?.signal)) throw cancelledError(requestId)
    try {
      return await invoke(requestFetch(requestOptions?.signal, requestId as RequestId))
    } catch (error) {
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(requestId)
      throwNormalized(error, requestId)
    }
  }

  function httpClient(fetchImplementation: ControlPlaneFetch) {
    return createControlPlaneHttpClient({
      baseUrl: location.serverUrl,
      fetch: fetchImplementation,
      maxNetworkRetries: maximumRetries,
      async waitBeforeRetry(attempt) {
        await waitBeforeRetry(attempt)
      },
    })
  }

  return {
    serverUrl: location.serverUrl,
    async restore(requestOptions) {
      const session = await authSessionRequest('GET', null, requestOptions)
      if (session === null) throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'INVALID_AUTH_SESSION_RESPONSE',
        message: 'The authentication server did not return the current browser session.',
        requestId: null,
        retryable: false,
      })
      return session
    },
    async login(bootstrapProof, requestOptions) {
      const session = await authSessionRequest('POST', bootstrapProof, requestOptions)
      if (session === null) throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'INVALID_AUTH_SESSION_RESPONSE',
        message: 'The authentication server did not create a browser session.',
        requestId: null,
        retryable: false,
      })
      return session
    },
    async loginWithPassword(credentials, requestOptions) {
      requireOpen()
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      assertLoginCredentials(credentials)
      try {
        const response = await transportRequest(
          `${location.serverUrl}${AUTH_SESSION_PATH}`,
          {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
              schemaVersion: CONTROL_PLANE_SCHEMA_VERSION,
              username: credentials.username,
              password: credentials.password,
            }),
            redirect: 'error',
            cache: 'no-store',
            referrerPolicy: 'no-referrer',
          },
          requestOptions?.signal,
        )
        const source = await response.text()
        if (!response.ok) throw loginBoundaryError(response.status, source)
        if (response.status !== 201) throw new ControlPlaneClientError({
          kind: 'protocol',
          code: 'INVALID_AUTH_SESSION_RESPONSE',
          message: 'The authentication server returned an invalid session response.',
          requestId: null,
          retryable: false,
        })
        return sessionResponse(source)
      } catch (error) {
        if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
        const normalized = error instanceof ControlPlaneClientError
          ? error
          : new ControlPlaneClientError({
            kind: 'network',
            code: 'NETWORK_ERROR',
            message: 'The authentication server could not be reached.',
            requestId: null,
            retryable: true,
          })
        reportAccessFailure(normalized)
        throw normalized
      }
    },
    async initializationStatus(requestOptions) {
      requireOpen()
      if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
      try {
        const response = await transportRequest(
          `${location.serverUrl}${SERVER_INITIALIZATION_PATH}`,
          {
            method: 'GET',
            headers: {},
            redirect: 'error',
            cache: 'no-store',
            referrerPolicy: 'no-referrer',
          },
          requestOptions?.signal,
        )
        const source = await response.text()
        if (!response.ok) throw initializationBoundaryError(response.status, source)
        if (response.status !== 200) throw new ControlPlaneClientError({
          kind: 'protocol',
          code: 'INVALID_SERVER_INITIALIZATION_RESPONSE',
          message: 'The Control Plane server returned an invalid initialization status.',
          requestId: null,
          retryable: false,
        })
        return initializationResponse(source)
      } catch (error) {
        if (signalIsAborted(requestOptions?.signal)) throw cancelledError(null)
        const normalized = error instanceof ControlPlaneClientError
          ? error
          : new ControlPlaneClientError({
            kind: 'network',
            code: 'NETWORK_ERROR',
            message: 'The Control Plane server could not be reached.',
            requestId: null,
            retryable: true,
          })
        reportAccessFailure(normalized)
        throw normalized
      }
    },
    async logout(requestOptions) {
      await authSessionRequest('DELETE', null, requestOptions)
      for (const subscription of [...subscriptions]) subscription.close()
    },
    command(command, requestOptions) {
      return execute(command, requestOptions, fetchImplementation => (
        httpClient(fetchImplementation).submitCommand(command)
      ))
    },
    query(query, requestOptions) {
      return execute(query, requestOptions, fetchImplementation => (
        httpClient(fetchImplementation).submitQuery(query)
      ))
    },
    subscribe(subscriptionOptions) {
      requireOpen()
      if (signalIsAborted(subscriptionOptions.signal)) throw cancelledError(null)
      const generated = createControlPlaneWebSocketClient({
        baseUrl: location.webSocketUrl,
        ...(options.transport?.createSocket === undefined
          ? {}
          : { createSocket: options.transport.createSocket }),
        reconnectDelayMillis,
        ...(subscriptionOptions.onEventQueued === undefined
          ? {}
          : { onEventQueued: subscriptionOptions.onEventQueued }),
        onEvent: subscriptionOptions.onEvent,
        ...(subscriptionOptions.onResetRequired === undefined
          ? {}
          : { onResetRequired: subscriptionOptions.onResetRequired }),
        async onAuthorizationRevoked(frame) {
          const error = new ControlPlaneClientError({
            kind: 'authentication',
            code: 'AUTHENTICATION_REQUIRED',
            message: 'The Control Plane subscription authorization is no longer valid.',
            requestId: null,
            retryable: false,
          })
          reportAccessFailure(error)
          await subscriptionOptions.onAuthorizationRevoked?.(frame)
        },
        onError(error) {
          const normalized = normalizeError(error, null)
          reportAccessFailure(normalized)
          subscriptionOptions.onError?.(normalized)
        },
      })
      let active = true
      const onAbort = () => { handle.close() }
      const handle: ControlPlaneSubscription = {
        get cursor() {
          return generated.cursor
        },
        resume() {
          requireOpen()
          try {
            generated.resume()
          } catch (error) {
            throwNormalized(error, null)
          }
        },
        reconnect() {
          requireOpen()
          try {
            generated.reconnect()
          } catch (error) {
            throwNormalized(error, null)
          }
        },
        close() {
          if (!active) return
          active = false
          subscriptionOptions.signal?.removeEventListener('abort', onAbort)
          generated.close()
          subscriptions.delete(handle)
        },
      }
      try {
        generated.subscribe(
          subscriptionOptions.subscriptionId,
          subscriptionOptions.subscription,
          subscriptionOptions.startAt ?? ControlPlaneWebSocketSubscribeOrigin.Latest,
        )
      } catch (error) {
        throwNormalized(error, null)
      }
      subscriptions.add(handle)
      subscriptionOptions.signal?.addEventListener('abort', onAbort, { once: true })
      return handle
    },
    close() {
      if (closed) return
      closed = true
      for (const subscription of [...subscriptions]) subscription.close()
    },
  }
}

// ---------------------------------------------------------------------------
// Repository directory: the authorized repository list (REPO-100.3).
//
// FAKE-DRIVEN SHAPES: the route and payload shape in this block are the
// presentation-side contract for the repository list. The Server owns the
// real wire route and payload names; when the Server landing settles them,
// only the path constant and the parsers in this block change. Pages and
// view-models only ever see the typed unions and summaries below. The enum
// values mirror the canonical contracts (`RepositoryDirtyState`,
// `RepositoryAvailability`) without importing the contracts source.
// ---------------------------------------------------------------------------

const REPOSITORY_DIRECTORY_LIST_PATH = '/api/v1/repositories'

export type ControlPlaneRepositoryDirtyState = 'clean' | 'dirty'

/** The seven canonical repository availability projections. */
export type ControlPlaneRepositoryAvailability =
  | 'available'
  | 'dirty'
  | 'unavailable'
  | 'moved'
  | 'invalid_git'
  | 'permission_denied'
  | 'scan_failed'

/** One repository list read: the Client device whose bindings are listed. */
export interface ControlPlaneRepositoryListInput {
  readonly clientId: string
}

/** One repository card. The Server owns every field; the browser only displays. */
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

/**
 * Validate the list input before a request exists: the query names one Client
 * device, so the identity must be a non-empty digit string with no grouping
 * separators or path-shaped text.
 */
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
    const response = await transportFetch(
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
    if (!response.ok) throw clientDirectoryBoundaryError(response.status, source)
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
    })
  }
}

// ---------------------------------------------------------------------------
// Client directory: add-Client connect and the device card list (CLIENT-200.4).
//
// FAKE-DRIVEN SHAPES: the routes and payload shapes in this block are the
// presentation-side contract for the add-Client form and the device list.
// The server-connect-flow lane owns the real wire routes and payload names;
// when those land, only the path constants and parsers in this block change.
// Pages and view-models only ever see the typed unions and summaries below.
// ---------------------------------------------------------------------------

const CLIENT_DIRECTORY_LIST_PATH = '/api/v1/clients'
const CLIENT_DIRECTORY_CONNECT_PATH = '/api/v1/clients/connections'

/** §12.1: presence is displayed separately from occupancy, never merged. */
export type ControlPlaneDevicePresence = 'online' | 'offline' | 'locked'

/** §12.1 occupancy states projected for device cards. */
export type ControlPlaneDeviceOccupancy =
  | 'available'
  | 'occupied-by-me'
  | 'occupied-by-other'
  | 'draining'
  | 'recovery-pending'

/** One device card. The Server owns every field; the browser only displays. */
export interface ControlPlaneDeviceSummary {
  readonly clientId: string
  readonly displayName: string
  readonly presence: ControlPlaneDevicePresence
  readonly occupancy: ControlPlaneDeviceOccupancy
  readonly capacityUsed: number
  readonly capacityTotal: number
  readonly lastHeartbeatAt: string
  readonly version: string
}

/** One add-Client attempt: the device identity plus its dynamic code. */
export interface ControlPlaneClientConnectInput {
  /** 9-12 digits; grouping separators are stripped by the facade. */
  readonly clientId: string
  /** Exactly 8 digits. */
  readonly connectionCode: string
}

/**
 * The one presentation-facing add-Client failure taxonomy. Wire codes are
 * translated by `controlPlaneClientAddFailure` and never read anywhere else.
 * `unavailable` is the catch-all for outages, protocol drift, and expired
 * browser sessions, mirroring the sign-in taxonomy above.
 */
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

/**
 * Translate one add-Client failure into the presentation taxonomy. Every wire
 * code stays inside this function; view-models and pages branch only on the
 * returned union.
 */
export function controlPlaneClientAddFailure(error: unknown): ControlPlaneClientAddFailure {
  if (error instanceof ControlPlaneClientError) {
    const failure = CLIENT_ADD_FAILURE_CODES[error.code]
    if (failure !== undefined) return failure
  }
  return 'unavailable'
}

/**
 * Validate connect input before a request exists, mirroring the sign-in input
 * bound. Grouping separators are stripped here, so the facade is the one place
 * that owns the digit shape; the failure code says which field is wrong.
 */
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
    ?? (code === 'RATE_LIMITED'
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
  return Object.freeze({
    clientId: value.clientId,
    displayName: value.displayName,
    presence: presence as ControlPlaneDevicePresence,
    occupancy: occupancy as ControlPlaneDeviceOccupancy,
    capacityUsed,
    capacityTotal,
    lastHeartbeatAt,
    version: value.version,
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

/**
 * The base facade extended with the two Client directory reads/writes. The
 * decorator delegates every base method, so hosts can pass it anywhere a
 * `ControlPlaneClient` is accepted and keep one session and one error identity.
 */
export interface ControlPlaneClientDirectory extends ControlPlaneClient {
  /** Submit one dynamic-code connect; resolves to the fresh device list. */
  addClient(
    input: ControlPlaneClientConnectInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<readonly ControlPlaneDeviceSummary[]>
  /** Read the device card list for the signed-in identity. */
  listClients(
    options?: ControlPlaneRequestOptions,
  ): Promise<readonly ControlPlaneDeviceSummary[]>
  /** Read the repository list bound to one Client device. */
  listRepositories(
    input: ControlPlaneRepositoryListInput,
    options?: ControlPlaneRequestOptions,
  ): Promise<readonly ControlPlaneRepositorySummary[]>
}

/**
 * Extend the one Control Plane facade with the Client directory. Expected
 * add-Client failures stay in the add form (the base client keeps reporting
 * connection health), and an injected facade that already implements the
 * directory methods is reused verbatim so deterministic fixtures and host
 * composition keep their single seam.
 */
export function createControlPlaneClientDirectory(options: {
  readonly client: ControlPlaneClient
  /** Same deterministic transport seam the base facade was created with. */
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
    return transportFetch(`${location.serverUrl}${path}`, {
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
      if (!response.ok) throw clientDirectoryBoundaryError(response.status, source)
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
      if (!response.ok) throw clientDirectoryBoundaryError(response.status, source)
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
      })
    }
  }

  const directory: ControlPlaneClientDirectory = {
    serverUrl: options.client.serverUrl,
    restore(requestOptions) {
      return options.client.restore(requestOptions)
    },
    login(bootstrapProof, requestOptions) {
      return options.client.login(bootstrapProof, requestOptions)
    },
    loginWithPassword(credentials, requestOptions) {
      return options.client.loginWithPassword(credentials, requestOptions)
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
      // The facade is the one place that owns the digit shape, so an injected
      // directory implementation receives the same normalized input the wire
      // path would send.
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
      // The facade is the one place that owns the query identity, so an
      // injected directory implementation receives the same normalized input
      // the wire path would send. Every rejection surfaces as a promise
      // rejection, mirroring the connect path above.
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
