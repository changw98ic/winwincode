// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import test from 'node:test'
import { createECDH } from 'node:crypto'
import { applyBenchmarkProviderCommand, openCodeTaskProvider, providerWorkflowLimiter } from '../scripts/benchmark/benchmark-opencode-routes.mjs'
import { benchmarkDeviceEnvironment, deviceTaskProvider, fusionDeviceProviders } from '../scripts/acceptance/run-device-task-vertical.mjs'
import { benchmarkConfiguration } from '../scripts/benchmark/run-real-task-benchmark.mjs'

const routes = { glm: { providerId: 'go-account-a', modelId: 'glm-5.3-flash',
  authorizationDirectory: '/private/account-a', organizationId: 'org-a' },
qwen: { providerId: 'go-account-b', modelId: 'qwen3.8-flash',
  authorizationDirectory: '/private/account-b', organizationId: 'org-b' } }
const environment = { WWC_DEVICE_TASK_OPENCODE_ROUTES: JSON.stringify(routes),
  XIAOMI_MODEL: 'mimo-v2.6-pro', XIAOMI_BASE_URL: 'https://mimo.invalid/anthropic', XIAOMI_API_KEY: 'mimo',
  DEEPSEEK_MODEL: 'deepseek-flash', DEEPSEEK_BASE_URL: 'https://deepseek.invalid/anthropic', DEEPSEEK_API_KEY: 'deepseek' }

test('encrypted benchmark Provider deletion accepts its native deleted receipt and rejects a failed mutation', async () => {
  const key = createECDH('prime256v1'); key.generateKeys()
  const snapshot = { clientNodeId: 'device', revision: 1, encryptionPublicKey: key.getPublicKey().toString('base64') }
  let receipt = null, outcome = 'deleted'
  const api = { async request(path, options) {
    if (options?.method === 'POST') {
      assert.equal(options.body.expectedRevision, snapshot.revision)
      assert.equal(options.body.operation, undefined, 'mutation must remain encrypted')
      receipt = { requestId: options.body.requestId, revision: 2, outcome }
      return { status: 202 }
    }
    return { json: { snapshot, receipt: path.includes('/receipts/') ? receipt : null } }
  } }
  const command = { operation: 'delete', config: { providerId: 'bootstrap', displayName: 'Bootstrap',
    endpoint: 'https://opencode.ai/inference/go/openai/v1/chat/completions', protocol: 'openai_chat_completions',
    modelIds: ['unused'], enabled: true } }
  assert.equal(await applyBenchmarkProviderCommand(api, 'client', command, 'deleted'), snapshot)
  outcome = 'invalid_request'
  await assert.rejects(applyBenchmarkProviderCommand(api, 'client', command, 'deleted'))
})

test('OAuth benchmark seats work with both legacy credentials absent, including native Fusion profile routes', () => {
  const a = deviceTaskProvider('glm', environment), b = deviceTaskProvider('qwen', environment)
  assert.equal(a.providerId, 'go-account-a'); assert.equal(b.providerId, 'go-account-b')
  assert.equal(a.apiKey, undefined); assert.equal(b.apiKey, undefined)
  assert.equal(a.protocol, 'openai_chat_completions')
  assert.equal(b.protocol, 'anthropic_messages')
  assert.equal(b.endpoint, 'https://opencode.ai/inference/go/anthropic/v1/messages')
  assert.equal(a.openCode.authorizationDirectory, '/private/account-a')
  const configured = benchmarkDeviceEnvironment(benchmarkConfiguration('main-C'), {}, environment)
  const profile = JSON.parse(configured.WWC_WORKER_FUSION)
  assert.deepEqual(profile.members.map(member => member.provider), ['go-account-a', 'xiaomi-mimo', 'deepseek', 'go-account-b'])
  assert.deepEqual(fusionDeviceProviders(profile, environment).map(provider => provider.modelId),
    ['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash'])
  assert.equal(configured.WWC_WORKER_MODEL_REASONING_EFFORT, 'max')
})

test('OAuth route rejects changed model, injected credential and duplicate provider identities', () => {
  for (const change of [
    value => { value.glm.modelId = 'glm-5.2' },
    value => { value.glm.apiKey = 'forbidden' },
    value => { value.qwen.providerId = value.glm.providerId },
  ]) {
    const value = structuredClone(routes); change(value)
    assert.throws(() => openCodeTaskProvider('glm', { WWC_DEVICE_TASK_OPENCODE_ROUTES: JSON.stringify(value) }))
  }
})

test('workflow admission limits each Provider to three, admits another Provider and releases failures', async () => {
  const limit = providerWorkflowLimiter(3), releases = [], started = []
  const calls = Array.from({ length: 4 }, (_, index) => limit('a', async () => {
    started.push(index)
    await new Promise(resolve => { releases[index] = resolve })
    if (index === 0) throw new Error('retained failure')
    return index
  }))
  const completion = Promise.allSettled(calls)
  assert.deepEqual(started, [0, 1, 2])
  assert.equal(await limit('b', async () => 'independent'), 'independent')
  releases[0](); await new Promise(resolve => setImmediate(resolve))
  assert.deepEqual(started, [0, 1, 2, 3])
  for (const release of releases.slice(1)) release()
  const outcomes = await completion
  assert.equal(outcomes[0].status, 'rejected')
  assert.deepEqual(outcomes.slice(1).map(value => value.value), [1, 2, 3])
  assert.equal(await limit('a', async () => 'recovered'), 'recovered')
})
