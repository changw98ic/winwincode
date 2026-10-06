// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { encryptDeviceProviderEnvelope, waitFor } from '../lib/device-production-fixture.mjs'

export const OPEN_CODE_GO_ENDPOINT = 'https://opencode.ai/inference/go/openai/v1/chat/completions'
export const OPEN_CODE_GO_MESSAGES_ENDPOINT = 'https://opencode.ai/inference/go/anthropic/v1/messages'
const models = { glm: 'glm-5.3-flash', qwen: 'qwen3.8-flash' }

export function openCodeTaskProvider(name, environment) {
  if (environment.WWC_DEVICE_TASK_OPENCODE_ROUTES === undefined) return null
  const routes = JSON.parse(environment.WWC_DEVICE_TASK_OPENCODE_ROUTES)
  assert.ok(routes && typeof routes === 'object' && !Array.isArray(routes))
  assert.ok(Object.keys(routes).every(key => Object.hasOwn(models, key)))
  const route = routes[name]
  if (route === undefined) return null
  assert.deepEqual(Object.keys(route).sort(), ['authorizationDirectory', 'modelId', 'organizationId', 'providerId'])
  assert.equal(route.modelId, models[name], 'OAuth seat must use its frozen benchmark model')
  assert.match(route.providerId, /^[a-zA-Z0-9][a-zA-Z0-9_-]{0,99}$/u)
  assert.ok(typeof route.organizationId === 'string' && route.organizationId.length > 0)
  assert.ok(typeof route.authorizationDirectory === 'string' && route.authorizationDirectory.startsWith('/'))
  assert.equal(new Set(Object.values(routes).map(value => value.providerId)).size, Object.keys(routes).length)
  return { providerId: route.providerId, modelId: route.modelId,
    endpoint: name === 'qwen' ? OPEN_CODE_GO_MESSAGES_ENDPOINT : OPEN_CODE_GO_ENDPOINT,
    protocol: name === 'qwen' ? 'anthropic_messages' : 'openai_chat_completions', displayName: `OpenCode Go ${name}`,
    openCode: { authorizationDirectory: route.authorizationDirectory, organizationId: route.organizationId } }
}

export async function applyBenchmarkProviderCommand(api, publicClientId, command, expectedOutcome = 'saved') {
  const path = `/api/v1/clients/${encodeURIComponent(publicClientId)}/providers`
  // An out-of-process native import advances the private revision. A conflict
  // publishes that revision; the next encrypted command uses the new snapshot.
  for (let attempt = 0; attempt < 3; attempt++) {
    const snapshot = (await api.request(path)).json.snapshot
    const requestId = `benchmark_oauth_${randomUUID().replaceAll('-', '')}`
    const envelope = encryptDeviceProviderEnvelope(snapshot, requestId, command)
    assert.equal((await api.request(path, { method: 'POST', body: envelope })).status, 202)
    const result = await waitFor(async () => {
      const value = (await api.request(`${path}/receipts/${requestId}`)).json
      return value.receipt === null ? false : value
    }, 'benchmark OAuth configuration receipt', 60_000)
    if (result.receipt.outcome === 'revision_conflict') continue
    assert.equal(result.receipt.outcome, expectedOutcome, 'benchmark Provider configuration must complete')
    return result.snapshot
  }
  throw new Error('benchmark OAuth configuration revision did not settle')
}

export async function provisionBenchmarkOpenCode({ devicePath, api, providers, directory }) {
  const selected = providers.filter(provider => provider.openCode)
  if (selected.length === 0) return
  const store = join(devicePath.deviceData, 'providers')
  const probe = resolve(process.env.CARGO_TARGET_DIR ?? join(import.meta.dirname, '../..', 'target'),
    'debug/examples/opencode_go_probe')
  for (const provider of selected) {
    const identityFile = join(store, `benchmark-${provider.providerId}.identity.json`)
    if (!existsSync(identityFile)) {
      const identity = JSON.parse(execFileSync(probe, ['import-auth', store,
        provider.openCode.authorizationDirectory, provider.providerId, provider.openCode.organizationId, provider.modelId],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }))
      writeFileSync(identityFile, `${JSON.stringify(identity, null, 2)}\n`, { flag: 'wx', mode: 0o600 })
    }
    const identity = JSON.parse(readFileSync(identityFile))
    assert.equal(identity.providerId, provider.providerId)
    assert.equal(identity.organizationId, provider.openCode.organizationId)
  }
  const snapshot = await applyBenchmarkProviderCommand(api, devicePath.publicClientId,
    { operation: 'set_default_provider', providerId: providers[0].providerId })
  const accounts = []
  for (const provider of selected) {
    const actual = snapshot.providers.find(item => item.config.providerId === provider.providerId)
    const identity = JSON.parse(readFileSync(join(store, `benchmark-${provider.providerId}.identity.json`)))
    assert.equal(actual?.openCode?.accountRef, identity.accountRef)
    assert.equal(actual?.openCode?.organizationId, provider.openCode.organizationId)
    assert.equal(actual.config.endpoint, provider.endpoint)
    assert.equal(actual.config.protocol, provider.protocol)
    assert.ok(actual.config.modelIds.includes(provider.modelId) && actual.credentialConfigured)
    accounts.push({ ...identity, modelId: provider.modelId, endpoint: actual.config.endpoint })
  }
  assert.equal(new Set(accounts.map(account => account.accountRef)).size, accounts.length,
    'benchmark OAuth seats must use distinct accounts')
  const evidence = join(directory, `oauth-routes-${devicePath.publicClientId}.json`)
  const bytes = `${JSON.stringify(accounts, null, 2)}\n`
  if (!existsSync(evidence)) writeFileSync(evidence, bytes, { flag: 'wx', mode: 0o600 })
  else assert.equal(readFileSync(evidence, 'utf8'), bytes)
}

export function providerWorkflowLimiter(limit = 3) {
  const active = new Map(), waiting = new Map()
  return async (provider, execute) => {
    if ((active.get(provider) ?? 0) >= limit) {
      await new Promise(resolve => {
        if (!waiting.has(provider)) waiting.set(provider, [])
        waiting.get(provider).push(resolve)
      })
    } else active.set(provider, (active.get(provider) ?? 0) + 1)
    try { return await execute() }
    finally {
      const next = waiting.get(provider)?.shift()
      if (next) next()
      else active.set(provider, active.get(provider) - 1)
    }
  }
}
