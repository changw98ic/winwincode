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
