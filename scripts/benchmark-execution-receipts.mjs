import assert from 'node:assert/strict'

const frozenModels = new Set(['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash'])

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
  }
  return receipts
}
