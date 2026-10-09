import executionPortSchema from '../schema/winwincode/v1/execution-port.schema.json' with { type: 'json' }

const modelCodes = new Set(executionPortSchema.$defs.ExecutionPortErrorCode.enum.filter(code =>
  code.startsWith('DEVICE_PROVIDER_') || code.startsWith('DEVICE_MODEL_') || code === 'DEVICE_JEV_UNAVAILABLE'))
const legacyCodes = new Set([...modelCodes, 'DEVICE_PROVIDER_PROTOCOL_FAILED'])
const canonicalCodes = new Set(['AUTH', 'INVALID_REQUEST', 'RATE_LIMIT', 'QUOTA', 'TIMEOUT',
  'TRANSPORT', 'SERVER', 'CONTEXT_WINDOW_EXCEEDED', 'CONTENT_FILTER', 'PROVIDER_STREAM_FAILED'])
const diagnosticStages = new Set(['response_fields', 'response_lifecycle', 'sse_framing',
  'json_decode', 'sse_event', 'tool_arguments'])
const diagnosticEvents = new Set(['unknown', 'eof', 'ping', 'message_start', 'content_block_start',
  'content_block_delta', 'content_block_stop', 'message_delta', 'message_stop', 'error', 'chat.completion.chunk'])
const diagnosticPaths = new Set(['$', '$.type', '$.index', '$.id', '$.message', '$.message.id', '$.message.type',
  '$.message.role', '$.message.model', '$.content_block', '$.content_block.type', '$.content_block.text',
  '$.content_block.id', '$.content_block.name', '$.content_block.input', '$.content_block.input.input',
  '$.delta', '$.delta.type', '$.delta.text', '$.delta.thinking', '$.delta.signature', '$.delta.partial_json',
  '$.delta.stop_reason', '$.error', '$.error.type', '$.error.message', '$.choices[0].finish_reason',
  '$.choices[0].delta.tool_calls[].function.arguments', ...['$.usage', '$.message.usage'].flatMap(prefix =>
    ['', '.input_tokens', '.output_tokens', '.cache_read_input_tokens', '.cache_creation_input_tokens',
      '.service_tier', '.server_tool_use'].map(field => `${prefix}${field}`))])

// HTTP metadata is retained by the trusted adapter after its credential leak
// gate. Keep only typed fields and fixed parser locations in public evidence.
export function retainedProviderFailureMetadata(metadata) {
  const retained = {}
  if (Number.isInteger(metadata?.status) && metadata.status >= 100 && metadata.status <= 599) {
    retained.status = metadata.status
  }
  if (Number.isSafeInteger(metadata?.providerRetryAfterMillis) && metadata.providerRetryAfterMillis >= 0) {
    retained.providerRetryAfterMillis = metadata.providerRetryAfterMillis
  }
  if (typeof metadata?.providerRequestId === 'string' && /^[\x21-\x7e]{1,256}$/u.test(metadata.providerRequestId)) {
    retained.providerRequestId = metadata.providerRequestId
  }
  const diagnostic = metadata?.diagnostic
  if (diagnosticStages.has(diagnostic?.stage) && diagnosticEvents.has(diagnostic?.eventType)
    && diagnosticPaths.has(diagnostic?.fieldPath)) {
    retained.diagnostic = { stage: diagnostic.stage, eventType: diagnostic.eventType, fieldPath: diagnostic.fieldPath }
  }
  return retained
}

// Keep only authority-owned codes; upstream text is private and may contain credentials.
export function retainedModelFailure(error, { canonical = false, metadata } = {}) {
  let code = 'MODEL_STREAM_FAILED'
  if (canonical) {
    if (canonicalCodes.has(error?.code)) code = error.code
  } else if (modelCodes.has(error?.code)) {
    code = error.code
  } else if (error?.code === 'MODEL_STREAM_FAILED') {
    const legacyCode = typeof error.message === 'string' ? error.message.split(':', 1)[0] : null
    if (legacyCodes.has(legacyCode)) code = legacyCode
  }
  const legacy = !canonical && error?.code === 'MODEL_STREAM_FAILED' && code !== 'MODEL_STREAM_FAILED'
  return { code, retryable: error?.retryable === true, ...(legacy ? { legacy: true } : {}),
    ...retainedProviderFailureMetadata(metadata) }
}

export function deviceFailureWithModelCauses(failure, evidence, jobIds) {
  const jobs = new Set(jobIds)
  const causes = evidence.calls.filter(call => jobs.has(call.jobId) && call.failure)
    .map(call => ({ exchangeId: call.exchangeId, jobId: call.jobId, ...call.failure }))
  if (causes.length === 0) return failure
  // A call may have recovered before an unrelated terminal error. Only the
  // authority-owned terminal establishes the final code; history is context.
  return { ...failure, observedProviderFailures: causes }
}

// Whitelist persisted diagnostics before exporting them outside Device storage.
const networkKinds = new Set(['connection_unavailable', 'transport_interrupted', 'timeout', 'rate_limited',
  'server_transient', 'authentication', 'authorization', 'request_invalid', 'protocol_invalid',
  'integrity_invalid', 'tls_invalid', 'stream_incomplete', 'empty_response', 'cancelled', 'authority_expired', 'storage_unavailable'])
const diagnosticCodes = new Set(['http_status', 'dns', 'connect', 'io', 'timeout', 'tls', 'tls_certificate',
  'http_protocol', 'request_uri', 'request_headers', 'proxy', 'redirect', 'response_headers_too_large',
  'body_too_large', 'content_type', 'transport_other', 'json_syntax', 'json_schema', 'response_invariant',
  'schema_version', 'sse_framing', 'sse_event', 'stream_conversion', 'stream_incomplete', 'empty_response',
  'identity_conflict', 'credential_blocked', 'storage', 'configuration', 'authority_ended'])
const ioKinds = new Set(['timed_out', 'connection_refused', 'not_connected', 'network_unreachable',
  'host_unreachable', 'unexpected_eof', 'connection_reset', 'connection_aborted', 'broken_pipe',
  'interrupted', 'invalid_data', 'permission_denied', 'other'])
const safeInteger = value => Number.isSafeInteger(value) && value >= 0 ? value : null
export function retainedNetworkFailure(value) {
  if (!networkKinds.has(value?.kind)) return null
  const fact = { kind: value.kind,
    acceptance: ['not_sent', 'unknown', 'response_received'].includes(value.acceptance) ? value.acceptance : 'unknown',
    phase: ['connect', 'response_headers', 'response_body', 'stream', 'decode', 'persist'].includes(value.phase) ? value.phase : null,
    httpStatus: Number.isInteger(value.httpStatus) && value.httpStatus >= 100 && value.httpStatus <= 599 ? value.httpStatus : null,
    retryAfterMs: safeInteger(value.retryAfterMs) }
  if (diagnosticCodes.has(value.diagnostic?.code)) {
    const source = value.diagnostic
    fact.diagnostic = { code: source.code }
    if (['content_type', 'model', 'answers', 'answer_type', 'probabilities', 'choice', 'confidence',
      'usage_input_tokens', 'usage_output_tokens', 'scores', 'labels', 'entailment', 'contradiction', 'neutral'].includes(source.field)) fact.diagnostic.field = source.field
    if (ioKinds.has(source.ioKind)) fact.diagnostic.ioKind = source.ioKind
    for (const key of ['osCode', 'line', 'column']) {
      if (Number.isSafeInteger(source[key]) && (key === 'osCode' || source[key] >= 0)) fact.diagnostic[key] = source[key]
    }
    if (/^sse-[0-9a-f]{64}\.log$/u.test(source.responseLog)) fact.diagnostic.responseLog = source.responseLog
    if (['retained', 'write_failed'].includes(source.responseLogStatus)) fact.diagnostic.responseLogStatus = source.responseLogStatus
  }
  return fact
}
export function retainedJevFailure(value) {
  return { kind: ['unavailable', 'resourceExhausted', 'timeout', 'invalidRequest', 'invalidResponse', 'unsupported'].includes(value?.kind) ? value.kind : null,
    attempt: safeInteger(value?.attempt), connectionWait: value?.connectionWait === true,
    network: retainedNetworkFailure(value?.network) }
}
export function retainedStopReason(value) {
  return ['succeeded', 'response_completed', 'authority_ended', 'storage_failed', 'configuration_invalid',
    'retry_budget_exhausted', 'permanent_failure', 'reconcile_required', 'deferred'].includes(value) ? value : null
}

export function retainedRequestFailure(error) {
  const seen = new Set()
  let current = error, network = null, attempts = null, stopReason = null
  for (let depth = 0; current && depth < 8 && !seen.has(current); depth += 1) {
    seen.add(current)
    network ??= retainedNetworkFailure(current.networkFailure ?? current.failure)
    if (Array.isArray(current.networkAttempts)) {
      attempts = current.networkAttempts.map(fact => ({
        attempt: safeInteger(fact?.attempt), networkAttempt: safeInteger(fact?.networkAttempt),
        connectionWaits: safeInteger(fact?.connectionWaits),
        outcome: ['succeeded', 'failed'].includes(fact?.outcome) ? fact.outcome : null,
        failure: retainedNetworkFailure(fact?.failure),
      }))
      network = retainedNetworkFailure(current.networkFailure ?? current.failure)
        ?? attempts.findLast(fact => fact.failure)?.failure ?? network
      stopReason = retainedStopReason(current.networkStopReason)
      break
    }
    current = current.cause
  }
  return network || attempts ? { network, attempts, stopReason } : null
}
