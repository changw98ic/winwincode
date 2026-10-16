// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { randomBytes } from 'node:crypto'
import test from 'node:test'
import { assertProviderSecretsAbsent } from './fixtures/provider-secret-boundary.mjs'

test('Provider boundary sentinels reject raw and encoded credentials without echoing them', () => {
  const secret = randomBytes(32).toString('hex')
  assertProviderSecretsAbsent(Buffer.from('{"credentialReferenceId":"crd_fixture"}'), [secret], 'managed configuration')
  for (const bytes of [Buffer.from(secret), ...[0, 1, 2].map(prefix =>
    Buffer.from(Buffer.concat([Buffer.alloc(prefix, 97), Buffer.from(secret), Buffer.from('suffix')]).toString('base64')))]) {
    assert.throws(() => assertProviderSecretsAbsent(bytes, [secret], 'retained Worker payload'), error => {
      assert.equal(error.message.includes(secret), false)
      assert.equal(JSON.stringify(error).includes(secret), false)
      return error.code === 'ERR_ASSERTION'
    })
  }
})
