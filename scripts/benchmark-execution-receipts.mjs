import assert from 'node:assert/strict'
import { retainedModelFailure, retainedProviderFailureMetadata } from './device-model-failures.mjs'

const frozenModels = new Set(['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash'])
const attemptFields = new Set(['exchangeId', 'attemptNumber', 'adapterRequestId', 'state',
  'actualModels', 'failure', 'usage', 'failureChunksSha256', 'accountingChunksSha256', 'responseSha256'])

function assertProviderAttempts(call) {
  if (call.providerAttempts === undefined) return
  assert.ok(Array.isArray(call.providerAttempts), 'invalid retained Provider attempts')
  for (const [index, attempt] of call.providerAttempts.entries()) {
    assert.ok(attempt && Object.keys(attempt).every(key => attemptFields.has(key)),
      'Provider attempt includes non-receipt fields')
    assert.equal(attempt.exchangeId, call.exchangeId, 'Provider attempt exchange mismatch')
    assert.equal(attempt.attemptNumber, index + 1, 'Provider attempt numbers are incomplete')
    assert.equal(attempt.adapterRequestId, `device-${call.exchangeId}:attempt:${attempt.attemptNumber}`,
      'Provider attempt request identity mismatch')
    assert.ok(['prepared', 'invoking', 'completed', 'failed', 'not_sent', 'interrupted_unknown'].includes(attempt.state),
      'invalid Provider attempt state')
    assert.ok(Array.isArray(attempt.actualModels) && attempt.actualModels.every(model => model === call.requestedModel),
      'observed Provider attempt model differs from the frozen request')
    for (const key of ['failureChunksSha256', 'accountingChunksSha256', 'responseSha256']) {
      assert.ok(attempt[key] === null || (typeof attempt[key] === 'string' && /^[0-9a-f]{64}$/u.test(attempt[key])),
        'invalid Provider attempt receipt digest')
    }
    if (attempt.failure !== undefined) {
      const failure = attempt.failure
      assert.ok(failure && Object.keys(failure).every(key => ['code', 'retryable', 'legacy', 'status',
        'providerRetryAfterMillis', 'providerRequestId', 'diagnostic'].includes(key)),
        'Provider attempt failure contains private diagnostics')
      const safe = retainedModelFailure(failure.legacy === true
        ? { code: 'MODEL_STREAM_FAILED', message: failure.code, retryable: failure.retryable } : failure)
      const canonical = retainedModelFailure(failure, { canonical: true })
      assert.ok(typeof failure.retryable === 'boolean'
        && ((failure.code === safe.code && failure.legacy === safe.legacy)
          || (failure.code === canonical.code && failure.legacy === undefined)),
        'Provider attempt failure is not a retained authority-owned category')
      const metadata = Object.fromEntries(Object.entries(failure).filter(([key]) =>
        ['status', 'providerRetryAfterMillis', 'providerRequestId', 'diagnostic'].includes(key)))
      assert.deepEqual(metadata, retainedProviderFailureMetadata(metadata), 'invalid retained Provider failure metadata')
    }
    if (attempt.usage !== null) {
      const usage = attempt.usage
      assert.ok(usage && Object.keys(usage).every(key => ['inputTokens', 'outputTokens', 'totalTokens',
        'cachedTokens', 'cacheWriteTokens', 'reasoningTokens'].includes(key)), 'invalid Provider attempt usage fields')
      for (const key of ['inputTokens', 'outputTokens', 'totalTokens']) {
        assert.ok(Number.isSafeInteger(usage[key]) && usage[key] >= 0, 'invalid measured Provider attempt usage')
      }
      assert.equal(usage.totalTokens, usage.inputTokens + usage.outputTokens,
        'measured Provider attempt usage total mismatch')
      for (const key of ['cachedTokens', 'cacheWriteTokens', 'reasoningTokens']) {
        assert.ok(usage[key] === null || (Number.isSafeInteger(usage[key]) && usage[key] >= 0),
          'invalid optional measured Provider attempt usage')
      }
    }
    if (attempt.state === 'not_sent' || attempt.state === 'prepared') {
      assert.equal(attempt.usage, null, 'unsent Provider attempt has measured upstream usage')
      assert.equal(attempt.accountingChunksSha256, null, 'unsent Provider attempt has upstream accounting')
      assert.equal(attempt.responseSha256, null, 'unsent Provider attempt has an upstream response')
    }
  }
}

/** Check the observed identity and effort of every actual product model exchange. */
export function assertBenchmarkExecutionReceipts(receipts) {
  assert.ok(receipts && Array.isArray(receipts.calls) && receipts.calls.length > 0)
  const exchanges = new Set()
  for (const call of receipts.calls) {
    assert.ok(typeof call.exchangeId === 'string' && call.exchangeId.length > 0)
    assert.ok(!exchanges.has(call.exchangeId), 'duplicate model exchange')
    exchanges.add(call.exchangeId)
    assert.ok(frozenModels.has(call.requestedModel), 'model request left the frozen comparison set')
    assert.equal(call.reasoningEffort, 'max', 'model exchange did not request max reasoning effort')
    assert.ok(Array.isArray(call.actualModels), 'observed model identities are missing')
    assert.ok(call.actualModels.every(model => model === call.requestedModel),
      'observed model differs from the frozen request')
    if (call.terminalType === 'completed') {
      assert.ok(call.actualModels.length > 0, 'completed model exchange has no observed identity')
    }
    assertProviderAttempts(call)
  }
  return receipts
}
