import assert from 'node:assert/strict'
import test from 'node:test'

import { validateFusionInput } from '../packages/contracts/src/fusion.ts'

const input = {
  question: 'Check the captured candidate',
  canonicalContext: {},
  constraints: [],
  expectedOutputSchema: {},
  providerCandidates: ['a', 'b', 'c'].map(id => ({ id, provider: id, model: 'm' })),
  budget: { candidateTimeoutMillis: null, maxTotalTokens: null },
}

test('Fusion optional limits retain null as unlimited across host validation', () => {
  assert.equal(validateFusionInput(input), null)
  for (const key of ['candidateTimeoutMillis', 'maxTotalTokens']) {
    assert.equal(validateFusionInput({ ...input, budget: { ...input.budget, [key]: 1 } }), null)
    for (const value of [0, -1, 0.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1, '1']) {
      assert.equal(validateFusionInput({ ...input, budget: { ...input.budget, [key]: value } }),
        'Fusion budget limits must be positive')
    }
  }
})
