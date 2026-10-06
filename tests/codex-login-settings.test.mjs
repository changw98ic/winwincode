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
const { mountDeviceProviderPanel } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`)

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
    assert.equal(key.value, '')
  } finally { panel.close(); dom.window.close() }
})
