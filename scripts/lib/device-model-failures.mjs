import executionPortSchema from '../../schema/winwincode/v1/execution-port.schema.json' with { type: 'json' }

const modelCodes = new Set(executionPortSchema.$defs.ExecutionPortErrorCode.enum.filter(code =>
  code.startsWith('DEVICE_PROVIDER_') || code.startsWith('DEVICE_MODEL_') || code === 'DEVICE_JEV_UNAVAILABLE'))
const canonicalCodes = new Set(['AUTH', 'INVALID_REQUEST', 'RATE_LIMIT', 'QUOTA', 'TIMEOUT',
  'TRANSPORT', 'SERVER', 'CONTEXT_WINDOW_EXCEEDED', 'PROVIDER_STREAM_FAILED'])

// Keep only authority-owned codes; upstream text is private and may contain credentials.
export function retainedModelFailure(error, { canonical = false } = {}) {
  let code = 'MODEL_STREAM_FAILED'
  if (canonical) {
    if (canonicalCodes.has(error?.code)) code = error.code
  } else if (modelCodes.has(error?.code)) {
    code = error.code
  }
  return { code, retryable: error?.retryable === true }
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
