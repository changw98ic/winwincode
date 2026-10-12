// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { webcrypto } from 'node:crypto'
import { resolve } from 'node:path'
import { setTimeout } from 'node:timers/promises'
import test from 'node:test'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const bundle = await build({
  entryPoints: [resolve(import.meta.dirname, '../apps/client/src/device-provider-panel.ts')],
  bundle: true, write: false, platform: 'node', format: 'esm',
})

for (const mode of ['json_schema', 'json_object', 'text']) test(`OpenAI Responses saves ${mode}, a custom HTTPS endpoint, API key, and explicit headers encrypted for the Device`, async () => {
  const dom = new JSDOM('<main></main>', { url: 'https://winwincode.example/' })
  Object.defineProperty(dom.window, 'crypto', { value: webcrypto })
  const root = dom.window.document.querySelector('main')
  const pair = await webcrypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits'])
  const device = 'cnd_00000000000000000000000001'
  const snapshot = { clientNodeId: device, revision: 0, providers: [],
    encryptionPublicKey: Buffer.from(await webcrypto.subtle.exportKey('raw', pair.publicKey)).toString('base64') }
  const view = { schemaVersion: 'winwincode/v1', online: true, snapshot, receipt: null }
  let envelope
  const panel = mountDeviceProviderPanel({ root, serverUrl: 'https://winwincode.example/', fetch: async (url, options) => {
    let body = view
    if (new URL(url).pathname === '/api/v1/clients') body = { clients: [{ clientId: device, displayName: 'This Device' }] }
    if (options.method === 'POST') { envelope = JSON.parse(options.body); body = {} }
    if (new URL(url).pathname.includes('/receipts/')) body = { ...view, receipt: { requestId: envelope.requestId, outcome: 'saved', revision: 1 } }
    return { ok: true, status: 200, text: async () => JSON.stringify(body) }
  } })
  try {
    for (let attempt = 0; root.querySelector('#wwc-device-provider-id').disabled && attempt < 100; attempt += 1) await setTimeout(10)
    const document = dom.window.document
    const protocol = document.querySelector('#wwc-device-provider-protocol')
    assert.ok([...protocol.options].some(option => option.value === 'openai_responses'))
    protocol.value = 'openai_responses'
    protocol.dispatchEvent(new dom.window.Event('change'))
    document.querySelector('#wwc-device-provider-id').value = 'custom-responses'
    document.querySelector('#wwc-device-provider-name').value = 'Custom Responses'
    document.querySelector('#wwc-device-provider-models').value = 'custom-model'
    const endpoint = document.querySelector('#wwc-device-provider-endpoint')
    const key = document.querySelector('#wwc-device-provider-key')
    const headers = document.querySelector('#wwc-device-provider-headers')
    assert.equal(endpoint.readOnly, false)
    assert.equal(key.closest('label').hidden, false)
    assert.equal(headers.closest('label').hidden, false)
    assert.equal(document.querySelector('#wwc-device-provider-authorize').hidden, true)
    const structuredOutput = document.querySelector('#wwc-device-provider-responses-structured-output')
    assert.ok(structuredOutput)
    assert.equal(structuredOutput.closest('label').hidden, false)
    assert.equal(structuredOutput.value, 'json_schema')
    assert.deepEqual([...structuredOutput.options].map(option => [option.value, option.textContent]), [
      ['json_schema', 'JSON Schema（默认）'], ['json_object', 'JSON Object'], ['text', '文本（本地 JSON 校验）'],
    ])
    structuredOutput.value = mode
    endpoint.value = 'https://models.example/v1/responses'
    key.value = 'responses-test-key'
    headers.value = '{"x-openai-internal-codex-responses-lite":"true"}'
    root.querySelector('form').dispatchEvent(new dom.window.Event('submit', { cancelable: true }))
    for (let attempt = 0; envelope === undefined && attempt < 100; attempt += 1) await setTimeout(10)
    assert.ok(envelope)
    assert.ok(!JSON.stringify(envelope).includes('responses-test-key'))
    const remote = await webcrypto.subtle.importKey('raw', Buffer.from(envelope.publicKey, 'base64'), { name: 'ECDH', namedCurve: 'P-256' }, false, [])
    const shared = await webcrypto.subtle.deriveBits({ name: 'ECDH', public: remote }, pair.privateKey, 256)
    const material = await webcrypto.subtle.importKey('raw', shared, 'HKDF', false, ['deriveKey'])
    const encoder = new TextEncoder()
    const aad = encoder.encode(`winwincode.device-provider.v1\n${device}\n${envelope.requestId}\n0`)
    const decryptKey = await webcrypto.subtle.deriveKey({ name: 'HKDF', hash: 'SHA-256', salt: encoder.encode('winwincode.device-provider.v1'), info: aad }, material, { name: 'AES-GCM', length: 256 }, false, ['decrypt'])
    const plaintext = await webcrypto.subtle.decrypt({ name: 'AES-GCM', iv: Buffer.from(envelope.nonce, 'base64'), additionalData: aad }, decryptKey, Buffer.from(envelope.ciphertext, 'base64'))
    const mutation = JSON.parse(new TextDecoder().decode(plaintext))
    assert.equal(mutation.config.protocol, 'openai_responses')
    assert.equal(mutation.config.endpoint, 'https://models.example/v1/responses')
    assert.equal(mutation.config.responsesStructuredOutput, mode)
    assert.equal(mutation.operation, 'save')
    assert.equal(mutation.apiKey, 'responses-test-key')
    assert.deepEqual(mutation.customHeaders, { 'x-openai-internal-codex-responses-lite': 'true' })
    assert.equal(key.value, '')
    assert.equal(headers.value, '')
  } finally { panel.close(); dom.window.close() }
})
const { mountDeviceProviderPanel } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`)

for (const mode of ['json_object', 'text']) test(`editing Responses preserves ${mode} and switching protocols clears the field`, async () => {
  const dom = new JSDOM('<main></main>', { url: 'https://winwincode.example/' })
  Object.defineProperty(dom.window, 'crypto', { value: webcrypto })
  const root = dom.window.document.querySelector('main')
  const pair = await webcrypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits'])
  const device = 'cnd_00000000000000000000000001'
  const config = { providerId: 'custom-responses', displayName: 'Custom Responses', endpoint: 'https://models.example/v1/responses',
    protocol: 'openai_responses', responsesStructuredOutput: mode, modelIds: ['custom-model'], enabled: true }
  const snapshot = { clientNodeId: device, revision: 0, providers: [{ config, credentialConfigured: true }],
    encryptionPublicKey: Buffer.from(await webcrypto.subtle.exportKey('raw', pair.publicKey)).toString('base64') }
  const view = { schemaVersion: 'winwincode/v1', online: true, snapshot, receipt: null }
  const envelopes = []
  const panel = mountDeviceProviderPanel({ root, serverUrl: 'https://winwincode.example/', fetch: async (url, options) => {
    let body = view
    if (new URL(url).pathname === '/api/v1/clients') body = { clients: [{ clientId: device, displayName: 'This Device' }] }
    if (options.method === 'POST') { envelopes.push(JSON.parse(options.body)); body = {} }
    if (new URL(url).pathname.includes('/receipts/')) body = { ...view, receipt: { requestId: envelopes.at(-1).requestId, outcome: 'saved', revision: 1 } }
    return { ok: true, status: 200, text: async () => JSON.stringify(body) }
  } })
  try {
    for (let attempt = 0; root.querySelector('li button') === null && attempt < 100; attempt += 1) await setTimeout(10)
    const edit = root.querySelector('li button')
    assert.ok(edit, 'the Device report with the selected Responses format must load')
    edit.click()
    const structuredOutput = root.querySelector('#wwc-device-provider-responses-structured-output')
    assert.equal(structuredOutput.value, mode)
    root.querySelector('form').dispatchEvent(new dom.window.Event('submit', { cancelable: true }))
    for (let attempt = 0; (envelopes.length < 1 || root.querySelector('#wwc-device-provider-id').disabled) && attempt < 100; attempt += 1) await setTimeout(10)
    assert.equal(envelopes.length, 1)
    const protocol = root.querySelector('#wwc-device-provider-protocol')
    protocol.value = 'anthropic_messages'
    protocol.dispatchEvent(new dom.window.Event('change'))
    assert.equal(structuredOutput.closest('label').hidden, true)
    assert.equal(structuredOutput.value, 'json_schema')
    root.querySelector('form').dispatchEvent(new dom.window.Event('submit', { cancelable: true }))
    for (let attempt = 0; envelopes.length < 2 && attempt < 100; attempt += 1) await setTimeout(10)
    assert.equal(envelopes.length, 2)
    const mutations = []
    for (const envelope of envelopes) {
      const remote = await webcrypto.subtle.importKey('raw', Buffer.from(envelope.publicKey, 'base64'), { name: 'ECDH', namedCurve: 'P-256' }, false, [])
      const shared = await webcrypto.subtle.deriveBits({ name: 'ECDH', public: remote }, pair.privateKey, 256)
      const material = await webcrypto.subtle.importKey('raw', shared, 'HKDF', false, ['deriveKey'])
      const encoder = new TextEncoder()
      const aad = encoder.encode(`winwincode.device-provider.v1\n${device}\n${envelope.requestId}\n0`)
      const decryptKey = await webcrypto.subtle.deriveKey({ name: 'HKDF', hash: 'SHA-256', salt: encoder.encode('winwincode.device-provider.v1'), info: aad }, material, { name: 'AES-GCM', length: 256 }, false, ['decrypt'])
      const plaintext = await webcrypto.subtle.decrypt({ name: 'AES-GCM', iv: Buffer.from(envelope.nonce, 'base64'), additionalData: aad }, decryptKey, Buffer.from(envelope.ciphertext, 'base64'))
      mutations.push(JSON.parse(new TextDecoder().decode(plaintext)))
    }
    assert.equal(mutations[0].config.responsesStructuredOutput, mode)
    assert.equal(mutations[0].apiKey, undefined, 'editing keeps the Device-owned API key')
    assert.equal(mutations[1].config.protocol, 'anthropic_messages')
    assert.equal(Object.hasOwn(mutations[1].config, 'responsesStructuredOutput'), false)
  } finally { panel.close(); dom.window.close() }
})

for (const [protocolId, endpointUrl, operation] of [
  ['codex_chatgpt', 'https://chatgpt.com/backend-api/codex/responses', 'save'],
  ['chatgpt_plan', 'https://api.openai.com/v1/responses', 'authorize'],
]) test(`${protocolId} fixes the endpoint and encrypts ${operation} with no API key`, async () => {
  const dom = new JSDOM('<main></main>', { url: 'https://winwincode.example/' })
  Object.defineProperty(dom.window, 'crypto', { value: webcrypto })
  const root = dom.window.document.querySelector('main')
  const pair = await webcrypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits'])
  const device = 'cnd_00000000000000000000000001'
  const snapshot = { clientNodeId: device, revision: 0, providers: [],
    encryptionPublicKey: Buffer.from(await webcrypto.subtle.exportKey('raw', pair.publicKey)).toString('base64') }
  const view = { schemaVersion: 'winwincode/v1', online: true, snapshot, receipt: null }
  let envelope
  const panel = mountDeviceProviderPanel({ root, serverUrl: 'https://winwincode.example/', fetch: async (url, options) => {
    let body = view
    if (new URL(url).pathname === '/api/v1/clients') body = { clients: [{ clientId: device, displayName: 'This Device' }] }
    if (options.method === 'POST') { envelope = JSON.parse(options.body); body = {} }
    if (new URL(url).pathname.includes('/receipts/')) body = { ...view, receipt: { requestId: envelope.requestId, outcome: 'saved', revision: 1 } }
    return { ok: true, status: 200, text: async () => JSON.stringify(body) }
  } })
  try {
    for (let attempt = 0; root.querySelector('#wwc-device-provider-id').disabled && attempt < 100; attempt += 1) await setTimeout(10)
    const document = dom.window.document
    document.querySelector('#wwc-device-provider-id').value = 'codex-personal'
    document.querySelector('#wwc-device-provider-name').value = 'My Codex'
    document.querySelector('#wwc-device-provider-models').value = 'test-model'
    const protocol = document.querySelector('#wwc-device-provider-protocol')
    protocol.value = protocolId
    protocol.dispatchEvent(new dom.window.Event('change'))
    const endpoint = document.querySelector('#wwc-device-provider-endpoint')
    const key = document.querySelector('#wwc-device-provider-key')
    assert.equal(endpoint.value, endpointUrl)
    assert.equal(endpoint.readOnly, true)
    assert.equal(key.closest('label').hidden, true)
    assert.equal(document.querySelector('#wwc-device-provider-responses-structured-output').closest('label').hidden, true)
    key.value = 'stale-api-key-must-not-travel'
    const authorize = root.querySelector('#wwc-device-provider-authorize')
    assert.equal(authorize.hidden, operation !== 'authorize')
    if (operation === 'authorize') authorize.click()
    else root.querySelector('form').dispatchEvent(new dom.window.Event('submit', { cancelable: true }))
    for (let attempt = 0; envelope === undefined && attempt < 100; attempt += 1) await setTimeout(10)
    assert.ok(envelope)
    const remote = await webcrypto.subtle.importKey('raw', Buffer.from(envelope.publicKey, 'base64'), { name: 'ECDH', namedCurve: 'P-256' }, false, [])
    const shared = await webcrypto.subtle.deriveBits({ name: 'ECDH', public: remote }, pair.privateKey, 256)
    const material = await webcrypto.subtle.importKey('raw', shared, 'HKDF', false, ['deriveKey'])
    const encoder = new TextEncoder()
    const aad = encoder.encode(`winwincode.device-provider.v1\n${device}\n${envelope.requestId}\n0`)
    const decryptKey = await webcrypto.subtle.deriveKey({ name: 'HKDF', hash: 'SHA-256', salt: encoder.encode('winwincode.device-provider.v1'), info: aad }, material, { name: 'AES-GCM', length: 256 }, false, ['decrypt'])
    const plaintext = await webcrypto.subtle.decrypt({ name: 'AES-GCM', iv: Buffer.from(envelope.nonce, 'base64'), additionalData: aad }, decryptKey, Buffer.from(envelope.ciphertext, 'base64'))
    const mutation = JSON.parse(new TextDecoder().decode(plaintext))
    assert.equal(mutation.config.protocol, protocolId)
    assert.equal(mutation.operation, operation)
    assert.equal(mutation.config.endpoint, endpoint.value)
    assert.equal(mutation.apiKey, undefined)
    assert.equal(Object.hasOwn(mutation.config, 'responsesStructuredOutput'), false)
    assert.equal(key.value, '')
  } finally { panel.close(); dom.window.close() }
})
