import assert from 'node:assert/strict'
import test from 'node:test'

import { containsRawCredentialMaterial } from '../packages/strongflow/dist/index.js'

test('Delivery credential boundary rejects raw values and permits references', () => {
  const exact = 'fixture-exact-secret-value'
  const denied = [
    { authorization: 'Bearer fixture-value' },
    'api_key=fixture-value',
    'eyJheader.payload.signature',
    'https://user:password@example.test/repository',
    { nested: { client_secret: 'fixture-value' } },
    exact,
  ]
  for (const value of denied) {
    assert.equal(
      containsRawCredentialMaterial(value, [exact]),
      true,
      JSON.stringify(value),
    )
  }

  const allowed = [
    { credential: 'dsh-reference-only' },
    { token: '[REDACTED]' },
    'Authorization: Bearer [REDACTED]',
    'Use ${API_KEY} from the DSH credential store.',
    { sourceRef: 'runtime-event:codex-session/42' },
  ]
  for (const value of allowed) {
    assert.equal(containsRawCredentialMaterial(value), false, JSON.stringify(value))
  }
})
