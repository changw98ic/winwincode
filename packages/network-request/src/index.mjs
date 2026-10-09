// SPDX-License-Identifier: Apache-2.0
import { policy } from './policy.generated.mjs'
export { policy }

export class NetworkError extends Error {
  constructor(failure, response, options = {}) {
    super(`Network request failed: ${failure.kind}`, options)
    this.name = 'NetworkError'
    this.failure = Object.freeze(safeFailure(failure))
    this.response = response
  }
}

// HTTP callers keep their domain error while retaining the request's retry facts.
export function withResponseFailure(error, response) {
  if (error !== null && typeof error === 'object' && error.cause === undefined && Object.isExtensible(error)) {
    Object.defineProperty(error, 'cause', {
      value: response.networkError instanceof NetworkError ? response.networkError : new NetworkError(httpFailure(response.status)),
      configurable: true,
    })
  }
  return error
}

export function retryAfter(value, now = Date.now()) {
  if (typeof value !== 'string') return null
  if (/^\s*\d+\s*$/u.test(value)) {
    const millis = Number(value) * 1000
    return Number.isSafeInteger(millis) ? millis : null
  }
  const instant = Date.parse(value)
  return Number.isFinite(instant) ? Math.max(0, instant - now) : null
}

export function httpFailure(status, retryAfterMs = null) {
  const kind = status === 401 ? 'authentication' : status === 403 ? 'authorization'
    : status === 408 ? 'timeout' : status === 429 ? 'rate_limited'
      : status === 425 || (status >= 500 && status <= 599) ? 'server_transient' : 'request_invalid'
  return { kind, acceptance: 'response_received', phase: 'response_headers', httpStatus: status, retryAfterMs, diagnostic: { code: 'http_status' } }
}

const errorKinds = new Set([...policy.transientKinds, 'authentication', 'authorization', 'request_invalid',
  'integrity_invalid', 'tls_invalid', 'cancelled', 'authority_expired', 'storage_unavailable'])
const diagnosticCodes = new Set(['http_status', 'dns', 'connect', 'io', 'timeout', 'tls', 'tls_certificate',
  'http_protocol', 'request_uri', 'request_headers', 'proxy', 'redirect', 'response_headers_too_large',
  'body_too_large', 'content_type', 'transport_other', 'json_syntax', 'json_schema', 'response_invariant',
  'schema_version', 'sse_framing', 'sse_event', 'stream_conversion', 'stream_incomplete', 'empty_response',
  'identity_conflict', 'credential_blocked', 'storage', 'configuration', 'authority_ended'])
const ioKinds = new Set(['timed_out', 'connection_refused', 'not_connected', 'network_unreachable',
  'host_unreachable', 'unexpected_eof', 'connection_reset', 'connection_aborted', 'broken_pipe',
  'interrupted', 'invalid_data', 'permission_denied', 'other'])
const phases = new Set(['connect', 'response_headers', 'response_body', 'stream', 'decode', 'persist'])
const transportCodes = new Set(['ETIMEDOUT', 'UND_ERR_CONNECT_TIMEOUT', 'UND_ERR_HEADERS_TIMEOUT', 'UND_ERR_BODY_TIMEOUT',
  'ECONNREFUSED', 'ENETUNREACH', 'EHOSTUNREACH', 'EAI_AGAIN', 'ENOTFOUND', 'ECONNRESET', 'ECONNABORTED', 'EPIPE',
  'CERT_HAS_EXPIRED', 'DEPTH_ZERO_SELF_SIGNED_CERT', 'SELF_SIGNED_CERT_IN_CHAIN', 'UNABLE_TO_VERIFY_LEAF_SIGNATURE',
  'ERR_TLS_CERT_ALTNAME_INVALID', 'ERR_INVALID_URL', 'ERR_INVALID_ARG_TYPE', 'ERR_UNESCAPED_CHARACTERS', 'UND_ERR_SOCKET'])
const isTransportCode = code => transportCodes.has(code) || (typeof code === 'string' && code.startsWith('HPE_'))

function safeFailure(value) {
  const diagnostic = value?.diagnostic
  const safe = { kind: errorKinds.has(value?.kind) ? value.kind : 'transport_interrupted',
    acceptance: ['not_sent', 'unknown', 'response_received'].includes(value?.acceptance) ? value.acceptance : 'unknown',
    phase: phases.has(value?.phase) ? value.phase : 'response_headers',
    httpStatus: Number.isInteger(value?.httpStatus) && value.httpStatus >= 100 && value.httpStatus <= 599 ? value.httpStatus : null,
    retryAfterMs: Number.isSafeInteger(value?.retryAfterMs) && value.retryAfterMs >= 0 ? value.retryAfterMs : null }
  if (diagnosticCodes.has(diagnostic?.code)) {
    safe.diagnostic = { code: diagnostic.code }
    if (['content_type', 'model', 'answers', 'answer_type', 'probabilities', 'choice', 'confidence',
      'usage_input_tokens', 'usage_output_tokens', 'scores', 'labels', 'entailment', 'contradiction', 'neutral'].includes(diagnostic.field)) safe.diagnostic.field = diagnostic.field
    if (ioKinds.has(diagnostic.ioKind)) safe.diagnostic.ioKind = diagnostic.ioKind
    for (const key of ['osCode', 'line', 'column']) {
      if (Number.isSafeInteger(diagnostic[key]) && (key === 'osCode' || diagnostic[key] >= 0)) safe.diagnostic[key] = diagnostic[key]
    }
  }
  if (safe.diagnostic === undefined) safe.diagnostic = { code: {
    protocol_invalid: 'response_invariant', integrity_invalid: 'response_invariant',
    tls_invalid: 'tls', timeout: 'timeout', empty_response: 'empty_response',
    stream_incomplete: 'stream_incomplete', cancelled: 'authority_ended', authority_expired: 'authority_ended',
    storage_unavailable: 'storage', request_invalid: 'configuration', connection_unavailable: 'connect',
  }[safe.kind] ?? 'transport_other' }
  return safe
}

export function classifyError(error, { notSent = false, phase = 'response_headers' } = {}) {
  if (error instanceof NetworkError) return safeFailure(error.failure)
  const seen = new Set()
  let current = error, code, networkFailure, precise = false, name = error?.name
  for (let depth = 0; current && depth < 8 && !seen.has(current); depth += 1) {
    seen.add(current)
    if (current instanceof NetworkError) return safeFailure(current.failure)
    const fact = current.networkFailure
    if (fact !== undefined && (!precise || diagnosticCodes.has(fact?.diagnostic?.code))) {
      networkFailure = safeFailure(fact)
      precise = diagnosticCodes.has(fact?.diagnostic?.code) || Number.isInteger(fact?.httpStatus)
    }
    if (['AbortError', 'TimeoutError'].includes(current.name)) name = current.name
    if (typeof current.code === 'string' && (code === undefined || isTransportCode(current.code))) code = current.code
    current = current.cause
  }
  if (networkFailure && (precise || !isTransportCode(code))
    && !['AbortError', 'TimeoutError'].includes(name)) return networkFailure
  let kind = 'transport_interrupted', diagnostic = { code: 'transport_other' }
  if (name === 'AbortError') { kind = 'cancelled'; diagnostic = { code: 'authority_ended' } }
  else if (name === 'TimeoutError' || ['ETIMEDOUT', 'UND_ERR_CONNECT_TIMEOUT', 'UND_ERR_HEADERS_TIMEOUT', 'UND_ERR_BODY_TIMEOUT'].includes(code)) {
    kind = 'timeout'; diagnostic = { code: 'timeout' }
  } else if (['ECONNREFUSED', 'ENETUNREACH', 'EHOSTUNREACH', 'EAI_AGAIN', 'ENOTFOUND'].includes(code)) {
    kind = 'connection_unavailable'
    diagnostic = ['EAI_AGAIN', 'ENOTFOUND'].includes(code) ? { code: 'dns' }
      : { code: 'io', ioKind: { ECONNREFUSED: 'connection_refused', ENETUNREACH: 'network_unreachable', EHOSTUNREACH: 'host_unreachable' }[code] }
  } else if (['ECONNRESET', 'ECONNABORTED', 'EPIPE'].includes(code)) {
    diagnostic = { code: 'io', ioKind: { ECONNRESET: 'connection_reset', ECONNABORTED: 'connection_aborted', EPIPE: 'broken_pipe' }[code] }
  } else if (['CERT_HAS_EXPIRED', 'DEPTH_ZERO_SELF_SIGNED_CERT', 'SELF_SIGNED_CERT_IN_CHAIN', 'UNABLE_TO_VERIFY_LEAF_SIGNATURE', 'ERR_TLS_CERT_ALTNAME_INVALID'].includes(code)) {
    kind = 'tls_invalid'; diagnostic = { code: 'tls_certificate' }
  } else if (['ERR_INVALID_URL', 'ERR_INVALID_ARG_TYPE', 'ERR_UNESCAPED_CHARACTERS'].includes(code)) {
    kind = 'request_invalid'; diagnostic = { code: 'request_uri' }
  } else if (code?.startsWith('HPE_')) diagnostic = { code: 'http_protocol' }
  else if (code === 'UND_ERR_SOCKET') diagnostic = { code: 'io', ioKind: 'unexpected_eof' }
  // Browser fetch has no trustworthy send-stage evidence.
  return safeFailure({ kind, acceptance: notSent ? 'not_sent' : 'unknown', phase, httpStatus: null, retryAfterMs: null, diagnostic })
}

function withHistory(error, attempts, stopReason, cause) {
  let retained = error
  if (retained === null || typeof retained !== 'object' || !Object.isExtensible(retained)) {
    retained = new NetworkError(classifyError(error), undefined, { cause: error })
  }
  if (cause !== undefined && retained.cause === undefined) Object.defineProperty(retained, 'cause', { value: cause, configurable: true })
  Object.defineProperties(retained, {
    networkAttempts: { value: Object.freeze(attempts.slice()), configurable: true },
    networkStopReason: { value: stopReason, configurable: true },
  })
  return retained
}

export function decide(failure, { replay = 'replay_exact', attempt = 1, connectionAttempt = 1, maxAttempts = policy.maxAttempts, jitterMs = 0 } = {}) {
  if (!policy.transientKinds.includes(failure.kind)) return { action: 'stop' }
  if (replay === 'reconcile_first' && failure.acceptance !== 'not_sent') return { action: 'reconcile' }
  const disconnected = failure.acceptance === 'not_sent'
    && ['connection_unavailable', 'timeout'].includes(failure.kind)
  const durableControl = replay === 'replay_exact'
  if (!disconnected && !durableControl && attempt >= Math.max(1, maxAttempts)) return { action: 'stop' }
  const ordinal = Math.max(1, disconnected ? connectionAttempt : attempt)
  const exponential = policy.initialDelayMs * 2 ** Math.min(ordinal - 1, 52)
  const base = disconnected || durableControl ? Math.min(exponential, policy.maxConnectionDelayMs) : exponential
  const delayMs = Math.max(base, failure.retryAfterMs ?? 0) + Math.min(Math.max(jitterMs, 0), policy.jitterMs)
  return { action: delayMs > policy.maxImmediateWaitMs ? 'deferred_until' : 'retry_after', delayMs }
}

function stopped(options) {
  if (options.signal?.aborted) return 'cancelled'
  if (options.canStart?.() === false) return 'authority_expired'
  if (options.deadline !== undefined && performance.now() >= options.deadline) return 'timeout'
  return null
}

function requireActive(options) {
  const kind = stopped(options)
  if (kind !== null) throw new NetworkError({ kind, acceptance: 'not_sent', phase: 'connect', httpStatus: null, retryAfterMs: null })
}

export async function wait(delayMs, options = {}) {
  const deadline = performance.now() + delayMs
  while (performance.now() < deadline) {
    requireActive(options)
    await new Promise(resolve => setTimeout(resolve, Math.min(policy.authorityCheckMs, deadline - performance.now())))
  }
  requireActive(options)
}

// One network attempt per callback. Protocol polling and durable outboxes use decide instead.
export async function executeRequest(sendOnce, options = {}) {
  let attempt = 1, connections = 0, networkAttempt = 0, lastError
  const attempts = []
  for (;;) {
    try { requireActive(options) } catch (error) { throw withHistory(error, attempts, 'authority_ended', lastError) }
    networkAttempt += 1
    // Storage failures must prevent sending; they never become network retries.
    try {
      if (options.beforeAttempt !== undefined) await options.beforeAttempt({ attempt, networkAttempt, connectionAttempts: connections })
    } catch (error) { throw withHistory(error, attempts, 'storage_failed', lastError) }
    try { requireActive(options) } catch (error) { throw withHistory(error, attempts, 'authority_ended', lastError) }
    const controller = new AbortController()
    const abort = () => controller.abort()
    options.signal?.addEventListener('abort', abort, { once: true })
    let check, result, caught, failed = false
    const interrupted = new Promise((_, reject) => {
      check = setInterval(() => {
        try { requireActive(options) } catch (error) { controller.abort(); reject(error) }
      }, policy.authorityCheckMs)
    })
    try {
      result = await Promise.race([sendOnce({ signal: controller.signal, attempt, networkAttempt }), interrupted])
    } catch (error) {
      failed = true
      const kind = stopped(options)
      caught = kind === null ? error : new NetworkError({ kind, acceptance: 'unknown', phase: 'response_headers', httpStatus: null, retryAfterMs: null, diagnostic: { code: 'authority_ended' } }, undefined, { cause: error })
    } finally {
      clearInterval(check)
      options.signal?.removeEventListener('abort', abort)
    }
    if (!failed) {
      const fact = Object.freeze({ attempt, networkAttempt, connectionWaits: 0, outcome: 'succeeded' })
      attempts.push(fact)
      try { await options.onAttempt?.(fact) } catch (error) { throw withHistory(error, attempts, 'storage_failed', lastError) }
      return result
    }
    const failure = classifyError(caught)
    const disconnected = failure.acceptance === 'not_sent'
      && ['connection_unavailable', 'timeout'].includes(failure.kind)
    connections = disconnected ? connections + 1 : 0
    const fact = Object.freeze({ attempt, networkAttempt, connectionWaits: connections, outcome: 'failed', failure })
    attempts.push(fact)
    lastError = caught
    try { await options.onAttempt?.(fact) } catch (error) { throw withHistory(error, attempts, 'storage_failed', caught) }
    const decision = decide(failure, { ...options, attempt, connectionAttempt: connections, jitterMs: options.jitter?.() ?? randomJitter() })
    if (decision.action !== 'retry_after') throw withHistory(caught, attempts,
      ['cancelled', 'authority_expired'].includes(failure.kind) || stopped(options) !== null ? 'authority_ended'
        : decision.action === 'reconcile' ? 'reconcile_required' : decision.action === 'deferred_until' ? 'deferred'
        : policy.transientKinds.includes(failure.kind) ? 'retry_budget_exhausted' : 'permanent_failure')
    try {
      if (options.waitBeforeRetry !== undefined) await options.waitBeforeRetry(attempt, decision.delayMs)
      else await wait(decision.delayMs, options)
    } catch (error) { throw withHistory(error, attempts, 'authority_ended', caught) }
    if (!disconnected) attempt += 1
  }
}

function randomJitter() {
  if (globalThis.crypto?.getRandomValues !== undefined) return globalThis.crypto.getRandomValues(new Uint8Array(1))[0]
  return Math.floor(Math.random() * (policy.jitterMs + 1))
}

// Buffer HTTP bodies inside the attempt so truncated responses use the same retry budget.
export async function executeFetch(fetcher, input, init, options = {}) {
  try {
    return await executeRequest(async ({ signal }) => {
      const response = await fetcher(input, { ...init, signal })
      const text = await response.text()
      const buffered = { ok: response.ok, status: response.status, headers: response.headers, text: async () => text }
      if (!response.ok) throw new NetworkError(httpFailure(response.status, retryAfter(response.headers?.get?.('retry-after'))), buffered)
      return buffered
    }, { signal: init?.signal, ...options })
  } catch (error) {
    if (error instanceof NetworkError && error.response !== undefined) {
      Object.defineProperty(error.response, 'networkError', { value: error, configurable: true })
      return error.response
    }
    throw error
  }
}

// Read methods may replay exactly. Other HTTP operations need an explicit
// proof of idempotency before callers opt in to replay_exact.
export function requestFetch(fetcher, input, init, options = {}) {
  return executeFetch(fetcher, input, init, { replay: ['GET', 'HEAD'].includes(init?.method ?? 'GET') ? 'replay_exact' : 'reconcile_first', ...options })
}
