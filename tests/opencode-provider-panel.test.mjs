// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import test from 'node:test'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'
import { createECDH, createDecipheriv, hkdfSync, webcrypto } from 'node:crypto'

async function moduleFor(path) {
  const result = await build({ entryPoints: [path], bundle: true, write: false, platform: 'browser', format: 'esm' })
  return import(`data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString('base64')}`)
}
const { mountOpenCodeProviderPanel } = await moduleFor('apps/client/src/opencode-provider-panel.ts')
const { mountDeviceProviderPanel } = await moduleFor('apps/client/src/device-provider-panel.ts')
const accountRef = 'oca_00000000000000000000000001'
function snapshot() {
  return { clientNodeId: 'device-test', revision: 1, encryptionPublicKey: 'fixture',
    providers: ['org-a', 'org-b'].map((org, index) => ({
      config: { providerId: `go-${index}`, displayName: org, endpoint: 'https://opencode.ai/inference/go/openai/v1/chat/completions',
        protocol: 'openai_chat_completions', modelIds: ['glm-5.3-flash'], enabled: true }, credentialConfigured: true,
      openCode: { accountRef, organizationId: org, organizationName: org },
    })), openCodeAccounts: [{ accountRef, issuer: 'https://opencode.ai/console', subject: 'actual-a', email: 'actual-a@example.test',
      state: 'authorized', credentialVersion: 1, usage: { organizationId: 'org-a', updatedAtMs: 1000,
        rolling: { percent: 5, status: 'ok', resetsAt: 'tomorrow' }, weekly: { percent: 10, status: 'ok', resetsAt: 'next week' },
        monthly: { percent: 20, status: 'ok', resetsAt: 'next month' } } }],
    openCodeLogins: [{ loginId: 'ocl_00000000000000000000000001', state: 'authorized', expiresAtMs: 2000, pollAfterMs: 0,
      accountRef, organizations: [{ id: 'org-a', name: 'A' }, { id: 'org-b', name: 'B' }] }],
  }
}
function surface() {
  const dom = new JSDOM('<main></main>', { url: 'https://client.test/' })
  Object.defineProperty(dom.window, 'crypto', { value: webcrypto })
  return { dom, root: dom.window.document.querySelector('main') }
}

test('one account displays quota once and connects the explicitly selected organization', () => {
  const { dom, root } = surface(); const sent = []
  const panel = mountOpenCodeProviderPanel({ root, snapshot, available: () => true, send: async command => { sent.push(command) }, report: assert.fail })
  panel.render()
  assert.equal(root.textContent.match(/滚动窗口/g).length, 1)
  assert.match(root.textContent, /实际登录账号：actual-a@example.test/)
  root.querySelector('select').value = 'org-b'
  root.querySelector('[aria-label="Go 模型（可选）"]').value = 'qwen3.8-flash'
  Array.from(root.querySelectorAll('button')).find(button => button.textContent === '添加 Go 连接').click()
  assert.equal(sent[0].operation, 'connect_opencode')
  assert.equal(sent[0].organizationId, 'org-b')
  assert.equal(sent[0].modelId, 'qwen3.8-flash')
  assert.equal(sent[0].loginId, snapshot().openCodeLogins[0].loginId)
  assert.match(sent[0].providerId, /^opencode-go-[0-9a-f]{32}$/u)
  panel.close(); dom.window.close()
})

test('pending login validates the official link, obeys poll time and stops timers on close', t => {
  t.mock.timers.enable({ apis: ['setTimeout', 'Date'], now: 1000 })
  const { dom, root } = surface(); const sent = []; let available = true
  const state = snapshot(); state.openCodeLogins = [{ loginId: 'ocl_00000000000000000000000001', state: 'pending',
    verificationUri: 'https://evil.test/console/device', userCode: 'SAFE-CODE', expiresAtMs: 50_000, pollAfterMs: 6000, organizations: [] }]
  const panel = mountOpenCodeProviderPanel({ root, snapshot: () => state, available: () => available,
    send: async command => { sent.push(command) }, report: assert.fail })
  panel.render(); assert.equal(root.querySelector('a'), null)
  state.openCodeLogins[0].verificationUri = 'https://opencode.ai/console/device?user_code=SAFE-CODE'
  panel.render(); assert.equal(root.querySelector('a').rel, 'noopener noreferrer')
  t.mock.timers.tick(4999); assert.equal(sent.length, 0)
  t.mock.timers.tick(1); assert.equal(sent[0].operation, 'poll_opencode_login')
  available = false; panel.render(); assert.ok(Array.from(root.querySelectorAll('button')).every(button => button.disabled))
  available = true; panel.render(); panel.close(); t.mock.timers.tick(60_000)
  assert.equal(sent.length, 1); dom.window.close()
})

test('the full Provider panel encrypts OAuth commands and locks immutable connection fields', async () => {
  const { dom, root } = surface(); const privateKey = createECDH('prime256v1'); privateKey.generateKeys()
  const state = snapshot(); state.encryptionPublicKey = privateKey.getPublicKey().toString('base64')
  let command = null; let receipt = null
  const view = () => ({ schemaVersion: 'winwincode/v1', online: true, snapshot: state, receipt })
  const panel = mountDeviceProviderPanel({ root, serverUrl: 'https://server.test/', fetch: async (url, options) => {
    if (new URL(url).pathname === '/api/v1/clients') return new Response(JSON.stringify({ clients: [{ clientId: 'client-a', displayName: 'A' }] }))
    if (options.method === 'POST') {
      const envelope = JSON.parse(options.body)
      assert.deepEqual(Object.keys(envelope).sort(), ['ciphertext', 'clientNodeId', 'expectedRevision', 'nonce', 'publicKey', 'requestId'])
      const context = 'winwincode.device-provider.v1'
      const aad = `${context}\n${envelope.clientNodeId}\n${envelope.requestId}\n${envelope.expectedRevision}`
      const key = hkdfSync('sha256', privateKey.computeSecret(Buffer.from(envelope.publicKey, 'base64')), Buffer.from(context), Buffer.from(aad), 32)
      const bytes = Buffer.from(envelope.ciphertext, 'base64')
      const cipher = createDecipheriv('aes-256-gcm', key, Buffer.from(envelope.nonce, 'base64')); cipher.setAAD(Buffer.from(aad)); cipher.setAuthTag(bytes.subarray(-16))
      command = JSON.parse(Buffer.concat([cipher.update(bytes.subarray(0, -16)), cipher.final()]))
      receipt = { requestId: envelope.requestId, outcome: 'saved', revision: ++state.revision }
    }
    return new Response(JSON.stringify(view()))
  } })
  await panel.refresh()
  Array.from(root.querySelectorAll('button')).find(button => button.textContent === '编辑').click()
  for (const id of ['id', 'endpoint', 'protocol', 'models', 'key', 'headers']) assert.equal(root.querySelector(`#wwc-device-provider-${id}`).disabled, true)
  assert.equal(root.querySelector('#wwc-device-provider-name').disabled, false)
  Array.from(root.querySelectorAll('button')).find(button => button.textContent === '登录 OpenCode 账号').click()
  for (let attempt = 0; command === null && attempt < 100; attempt += 1) await new Promise(resolve => setTimeout(resolve, 10))
  assert.deepEqual(command, { operation: 'begin_opencode_login' })
  panel.close(); assert.equal(root.childNodes.length, 0); dom.window.close()
})
