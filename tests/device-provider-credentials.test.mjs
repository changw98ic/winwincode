// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { createDecipheriv, createECDH, hkdfSync } from 'node:crypto'
import test from 'node:test'
import { DEVICE_PROVIDER_ENCRYPTION_CONTEXT, seedDeviceLocalProvider } from '../scripts/device-production-fixture.mjs'
import { deviceTaskProvider } from '../scripts/run-device-task-vertical.mjs'

const references = [
  ['environment reference', '{env:FIXTURE_PROVIDER_TOKEN}'],
  ['file reference', '{file:/fixture/provider-token}'],
  ['embedded environment reference', 'prefix-{env:FIXTURE_PROVIDER_TOKEN}-suffix'],
  ['embedded file reference', 'prefix-{file:/fixture/provider-token}-suffix'],
]
const missingCredentials = [undefined, null, '', 123, { value: 'fixture-token' }]
const providers = [['glm', 'ZHIPU'], ['mimo', 'XIAOMI'], ['deepseek', 'DEEPSEEK'], ['qwen', 'OPENCODE']]

function assertSafeError(error, code, credential) {
  assert.ok(error instanceof Error, 'admission must reject invalid credentials')
  assert.equal(error.code, code)
  assert.equal(error.message, code)
  const exported = `${error.stack}\n${JSON.stringify(error)}`
  if (typeof credential === 'string' && credential.trim().length > 0) {
    assert.equal(exported.includes(credential), false, 'credential must not appear in the safe error')
  }
  assert.equal(exported.includes('fixture-private-header'), false)
}

function providerEnvironment(prefix, credential) {
  return {
    [`${prefix}_API_KEY`]: credential,
    [`${prefix}_BASE_URL`]: 'https://provider.invalid',
    [`${prefix}_MODEL`]: 'fixture-model',
    XIAOMI_RESPONSES_URL: 'https://provider.invalid/v1/responses',
  }
}

// This fixture replaces the external Device API. The real encrypted payload is
// decrypted by its receiving Device key, so valid credential bytes are observed
// across the production encryption boundary without any socket or model call.
function receivingDevice() {
  const keyPair = createECDH('prime256v1')
  keyPair.generateKeys()
  const snapshot = {
    clientNodeId: 'fixture-device-node',
    revision: 7,
    encryptionPublicKey: keyPair.getPublicKey().toString('base64'),
  }
  const received = { apiRequests: 0, encryptedSaves: 0, mutations: [] }
  const api = {
    async request(path, options = {}) {
      received.apiRequests++
      if (options.method === 'POST') {
        received.encryptedSaves++
        const envelope = options.body
        const shared = keyPair.computeSecret(Buffer.from(envelope.publicKey, 'base64'))
        const aad = `${DEVICE_PROVIDER_ENCRYPTION_CONTEXT}\n${snapshot.clientNodeId}\n${envelope.requestId}\n${snapshot.revision}`
        const key = Buffer.from(hkdfSync('sha256', shared, Buffer.from(DEVICE_PROVIDER_ENCRYPTION_CONTEXT), Buffer.from(aad), 32))
        const ciphertext = Buffer.from(envelope.ciphertext, 'base64')
        const decipher = createDecipheriv('aes-256-gcm', key, Buffer.from(envelope.nonce, 'base64'))
        decipher.setAAD(Buffer.from(aad))
        decipher.setAuthTag(ciphertext.subarray(-16))
        const plaintext = Buffer.concat([decipher.update(ciphertext.subarray(0, -16)), decipher.final()])
        received.mutations.push(JSON.parse(plaintext.toString('utf8')))
        key.fill(0)
        shared.fill(0)
        plaintext.fill(0)
        return { status: 202 }
      }
      if (path.includes('/receipts/')) {
        return { status: 200, json: { receipt: { outcome: 'saved' }, snapshot: {
          providers: [{ config: { providerId: 'fixture-provider' }, credentialConfigured: true }],
        } } }
      }
      return { status: 200, json: { snapshot } }
    },
  }
  return { api, received }
}

function seedOptions(api, credential) {
  return { api, apiKey: credential, publicClientId: 'fixture-client', providerId: 'fixture-provider',
    modelId: 'fixture-model', endpoint: 'https://provider.invalid/v1/messages', timeoutMillis: 100,
    customHeaders: { 'x-fixture-private': 'fixture-private-header' } }
}

for (const [label, credential] of references) {
  test(`Device rejects ${label} before requesting or saving configuration`, async () => {
    const { api, received } = receivingDevice()
    let failure
    try { await seedDeviceLocalProvider(seedOptions(api, credential)) } catch (error) { failure = error }
    assert.equal(received.apiRequests, 0, 'unresolved credential must not reach the external Device API')
    assert.equal(received.encryptedSaves, 0)
    assert.equal(received.mutations.length, 0)
    assertSafeError(failure, 'PROVIDER_CREDENTIAL_REFERENCE_UNRESOLVED', credential)
  })
}

test('Device rejects empty and non-string credentials before requesting configuration', async () => {
  for (const credential of missingCredentials) {
    const { api, received } = receivingDevice()
    let failure
    try { await seedDeviceLocalProvider(seedOptions(api, credential)) } catch (error) { failure = error }
    assert.equal(received.apiRequests, 0)
    assert.equal(received.encryptedSaves, 0)
    assertSafeError(failure, 'PROVIDER_CREDENTIAL_MISSING', credential)
  }
})

for (const [name, prefix] of providers) {
  test(`${name} task configuration rejects every unresolved credential reference`, () => {
    for (const [, credential] of references) {
      let failure
      try { deviceTaskProvider(name, providerEnvironment(prefix, credential)) } catch (error) { failure = error }
      assertSafeError(failure, 'PROVIDER_CREDENTIAL_REFERENCE_UNRESOLVED', credential)
    }
  })
  test(`${name} task configuration rejects empty and non-string credentials safely`, () => {
    for (const credential of missingCredentials) {
      let failure
      try { deviceTaskProvider(name, providerEnvironment(prefix, credential)) } catch (error) { failure = error }
      assertSafeError(failure, 'PROVIDER_CREDENTIAL_MISSING', credential)
    }
  })
}

test('whitespace-only credentials stop before configuration requests for every entry', async () => {
  for (const credential of ['   ', '\t\n']) {
    const { api, received } = receivingDevice()
    let failure
    try { await seedDeviceLocalProvider(seedOptions(api, credential)) } catch (error) { failure = error }
    assert.equal(received.apiRequests, 0)
    assert.equal(received.encryptedSaves, 0)
    assertSafeError(failure, 'PROVIDER_CREDENTIAL_MISSING', credential)
    for (const [name, prefix] of providers) {
      assert.throws(() => deviceTaskProvider(name, providerEnvironment(prefix, credential)), error => {
        assertSafeError(error, 'PROVIDER_CREDENTIAL_MISSING', credential)
        return true
      })
    }
  }
})

test('unresolved private headers stop before configuration requests and task setup', async () => {
  for (const [, headerValue] of references) {
    const { api, received } = receivingDevice()
    let failure
    try {
      await seedDeviceLocalProvider({ ...seedOptions(api, 'fixture-opaque-token'),
        customHeaders: { 'x-fixture-private': headerValue } })
    } catch (error) { failure = error }
    assert.equal(received.apiRequests, 0)
    assert.equal(received.encryptedSaves, 0)
    assertSafeError(failure, 'PROVIDER_CREDENTIAL_REFERENCE_UNRESOLVED', headerValue)
    assert.throws(() => deviceTaskProvider('qwen', {
      ...providerEnvironment('OPENCODE', 'fixture-opaque-token'), OPENCODE_SESSION_VALUE: headerValue,
    }), error => {
      assertSafeError(error, 'PROVIDER_CREDENTIAL_REFERENCE_UNRESOLVED', headerValue)
      return true
    })
  }
})

test('Device encrypted save preserves an opaque credential and private headers byte for byte', async () => {
  const credential = '  fixture+opaque/{literal}:token==  '
  const { api, received } = receivingDevice()
  const result = await seedDeviceLocalProvider(seedOptions(api, credential))
  assert.equal(result.receiptOutcome, 'saved')
  assert.equal(received.apiRequests, 3)
  assert.equal(received.encryptedSaves, 1)
  assert.equal(received.mutations.length, 1)
  assert.deepEqual(Buffer.from(received.mutations[0].apiKey), Buffer.from(credential))
  assert.deepEqual(received.mutations[0].customHeaders, { 'x-fixture-private': 'fixture-private-header' })
})

test('all four task configurations preserve opaque credential bytes', () => {
  const credential = '  fixture+opaque/{literal}:token==  '
  for (const [name, prefix] of providers) {
    const environment = providerEnvironment(prefix, credential)
    const result = deviceTaskProvider(name, environment)
    assert.deepEqual(Buffer.from(result.apiKey), Buffer.from(credential))
    assert.equal(environment[`${prefix}_API_KEY`], credential)
  }
})
