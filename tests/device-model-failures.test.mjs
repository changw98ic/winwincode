import test from 'node:test'
import assert from 'node:assert/strict'
import { deviceFailureWithModelCauses } from '../scripts/lib/device-model-failures.mjs'

test('recovered provider errors cannot overwrite an independent terminal failure', () => {
  const failure = { code: 'DEVICE_USAGE_INCOMPLETE', status: 'infrastructure_error' }
  const evidence = { calls: [
    { jobId: 'job', exchangeId: 'early', failure: { code: 'DEVICE_PROVIDER_TRANSPORT_FAILED', retryable: false } },
    { jobId: 'job', exchangeId: 'recovered', terminalType: 'completed', failure: null },
    { jobId: 'other', exchangeId: 'foreign', failure: { code: 'DEVICE_PROVIDER_SSE_EVENT_INVALID' } },
  ] }
  const result = deviceFailureWithModelCauses(failure, evidence, ['job'])
  assert.equal(result.code, failure.code)
  assert.deepEqual(result.observedProviderFailures, [{ exchangeId: 'early', jobId: 'job',
    code: 'DEVICE_PROVIDER_TRANSPORT_FAILED', retryable: false }])
  assert.equal(result.rootCause, undefined)
})
