// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { createDecipheriv, createECDH, hkdfSync } from 'node:crypto'
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import test from 'node:test'
import { benchmarkDeviceProfiles, deviceBenchmarkExperimentBinding,
  removeTerminalBenchmarkPublicSmoke } from '../scripts/lib/benchmark-device-adapter.mjs'
import { buildBenchmarkPlan } from '../scripts/benchmark/run-real-task-benchmark.mjs'
import { benchmarkDeviceEnvironment } from '../scripts/acceptance/run-device-task-vertical.mjs'
import { runtimeChildEnvironment } from '../scripts/lib/device-production-fixture.mjs'

const models = ['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash']
const providerEnvironment = Object.fromEntries(['ZHIPU', 'XIAOMI', 'DEEPSEEK', 'OPENCODE']
  .flatMap((prefix, index) => [[`${prefix}_API_KEY`, 'fixture-only'],
    [`${prefix}_BASE_URL`, 'https://provider.invalid'], [`${prefix}_MODEL`, models[index]]]))

test('benchmark Device proxy is opt-in and stays out of Server and generic runtime environments', () => {
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) }).cells[0]
  const proxy = 'http://fixture-user:fixture-password@proxy.invalid:8080'
  const before = { HTTP_PROXY: process.env.HTTP_PROXY, HTTPS_PROXY: process.env.HTTPS_PROXY,
    WWC_DEVICE_PROVIDER_HTTPS_PROXY: process.env.WWC_DEVICE_PROVIDER_HTTPS_PROXY }
  const source = { WWC_DEVICE_PROVIDER_HTTPS_PROXY: proxy,
    HTTP_PROXY: 'http://ambient.invalid:8080', HTTPS_PROXY: 'http://ambient.invalid:8080' }
  const device = benchmarkDeviceEnvironment(cell, {}, source)
  assert.equal(device.WWC_DEVICE_PROVIDER_HTTPS_PROXY, proxy)
  assert.equal(device.HTTP_PROXY, undefined)
  assert.equal(device.HTTPS_PROXY, undefined)
  assert.equal(benchmarkDeviceEnvironment(cell, {}, { HTTPS_PROXY: proxy }).WWC_DEVICE_PROVIDER_HTTPS_PROXY, undefined)
  assert.equal(benchmarkDeviceEnvironment(cell, {}, {}).WWC_DEVICE_PROVIDER_HTTPS_PROXY, undefined)
  const runtime = runtimeChildEnvironment({ ...source, ...device, PATH: '/usr/bin:/bin' })
  assert.deepEqual(runtime, { PATH: '/usr/bin:/bin' })
  assert.deepEqual(source, { WWC_DEVICE_PROVIDER_HTTPS_PROXY: proxy,
    HTTP_PROXY: 'http://ambient.invalid:8080', HTTPS_PROXY: 'http://ambient.invalid:8080' })
  assert.deepEqual({ HTTP_PROXY: process.env.HTTP_PROXY, HTTPS_PROXY: process.env.HTTPS_PROXY,
    WWC_DEVICE_PROVIDER_HTTPS_PROXY: process.env.WWC_DEVICE_PROVIDER_HTTPS_PROXY }, before)
})

test('Device admission retains the full plan while provisioning only the selected available profiles', () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) })
  const original = JSON.stringify(plan)
  const options = { concurrency: 12, selectedConfigurationIds: ['main-C', 'main-A'],
    agentSettings: {}, providerEnvironment }
  const profiles = benchmarkDeviceProfiles(plan, options)
  assert.deepEqual(profiles.map(profile => profile.configurationId), ['main-A', 'main-C'])
  assert.equal(plan.cells.filter(cell => profiles.some(profile => profile.configurationId === cell.configurationId)).length, 200)
  assert.equal(plan.cells.length, 700)
  assert.equal(JSON.stringify(plan), original)
  assert.throws(() => benchmarkDeviceProfiles(plan, { agentSettings: {}, providerEnvironment }),
    { code: 'BENCHMARK_CONFIGURATION_UNAVAILABLE' })
  for (const configurationId of ['main-B', 'main-D', 'jev-context-only', 'jev-judge-only', 'jev-full']) {
    assert.throws(() => benchmarkDeviceProfiles(plan, { ...options,
      selectedConfigurationIds: ['main-A', configurationId] }), { code: 'BENCHMARK_CONFIGURATION_UNAVAILABLE' })
  }
  for (const selectedConfigurationIds of [[], ['main-A', 'main-A'], ['unknown'], 'main-A', [null]]) {
    assert.throws(() => benchmarkDeviceProfiles(plan, { ...options, selectedConfigurationIds }),
      { code: 'BENCHMARK_CONFIGURATION_SELECTION_INVALID' })
  }
})

test('expanding Device admission preserves the settings binding and changed JEV settings change its identity', () => {
  const options = { experimentId: 'cloud', agentSettings: {}, providerEvidence: [],
    frozenSourceIdentity: {}, productSourceSealSha256: 'fixture-seal' }
  const binding = value => deviceBenchmarkExperimentBinding({ ...options, ...value }, {}, { tasks: [] })
  assert.deepEqual(binding({ concurrency: 1, selectedConfigurationIds: ['main-A'] }),
    binding({ concurrency: 12, selectedConfigurationIds: ['main-A', 'main-C'] }))
  assert.notEqual(binding({}).agentSettingsSha256,
    binding({ agentSettings: { jevSettingsFile: '/private/real-settings' } }).agentSettingsSha256)
  assert.equal(Object.hasOwn(binding({}), 'publicSmokeExecutionLock'), false)
  const executionLock = resolve('private-cloud-smoke.lock')
  assert.equal(binding({ publicSmokeExecutionLock: 'private-cloud-smoke.lock' }).publicSmokeExecutionLock, executionLock)
  assert.notDeepEqual(binding({ publicSmokeExecutionLock: executionLock }), binding({}))
  assert.notDeepEqual(binding({ publicSmokeExecutionLock: executionLock }),
    binding({ publicSmokeExecutionLock: `${executionLock}.other` }))
  for (const publicSmokeExecutionLock of ['', null, 1, {}]) assert.throws(() => binding({ publicSmokeExecutionLock }),
    /public smoke execution lock must be a host path/u)
})

function cleanupFixture(t) {
  const directory = mkdtempSync(resolve(tmpdir(), 'benchmark-terminal-smoke-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const launch = { directory, productSessionId: 'psn_01J00000000000000000000001',
    deliveryId: 'dlv_01J00000000000000000000001' }
  const id = `benchmark_public_smoke_${launch.productSessionId}`
  const neighbor = 'benchmark_public_smoke_psn_01J00000000000000000000002'
  const report = { ...launch, complete: false, errorCode: 'DEVICE_PRODUCT_FAILED',
    publicClientId: 'client-owned', publicSmoke: { id } }
  const reportPath = resolve(directory, 'device-task-result.json')
  const save = () => writeFileSync(reportPath, `${JSON.stringify(report)}\n`)
  save()
  const readCursor = { deliveryId: launch.deliveryId, token: 'current' }
  const delivery = { deliveryId: launch.deliveryId, readCursor, status: 'failed', attention: [] }
  const aggregate = { readCursor, runs: [{ id: 'run-owned', state: 'failed' }],
    items: [{ id: 'item-owned', state: 'failed' }] }
  const device = createECDH('prime256v1')
  device.generateKeys()
  let revision = 0
  const servers = new Set([id, neighbor])
  const mutations = []
  const api = {
    async query(name, parameters) {
      assert.equal(parameters.deliveryId, launch.deliveryId)
      if (name === 'delivery.get') return { result: delivery }
      assert.equal(name, 'workrun.get')
      assert.equal(parameters.workItemId, null)
      assert.deepEqual(parameters.atCursor, delivery.readCursor)
      return { result: aggregate }
    },
    async request(path, options) {
      assert.ok(path.startsWith('/api/v1/clients/client-owned/extensions'))
      if (options?.method === 'POST') {
        const envelope = options.body
        assert.equal(envelope.expectedRevision, revision)
        const context = 'winwincode.device-extensions.v1'
        const aad = `${context}\n${envelope.clientNodeId}\n${envelope.requestId}\n${revision}`
        const shared = device.computeSecret(Buffer.from(envelope.publicKey, 'base64'))
        const key = hkdfSync('sha256', shared, Buffer.from(context), Buffer.from(aad), 32)
        const cipher = createDecipheriv('aes-256-gcm', key, Buffer.from(envelope.nonce, 'base64'))
        const bytes = Buffer.from(envelope.ciphertext, 'base64')
        cipher.setAAD(Buffer.from(aad))
        cipher.setAuthTag(bytes.subarray(-16))
        const mutation = JSON.parse(Buffer.concat([cipher.update(bytes.subarray(0, -16)), cipher.final()]).toString())
        assert.deepEqual(mutation, { operation: 'delete', kind: 'mcp', id })
        mutations.push(mutation)
        servers.delete(mutation.id)
        revision += 1
        return { status: 202 }
      }
      return { status: 200, json: { online: true,
        snapshot: { clientNodeId: 'device-owned', revision,
          encryptionPublicKey: device.getPublicKey().toString('base64'),
          mcpServers: [...servers].map(serverId => ({ id: serverId })) },
        ...(path.includes('/receipts/') ? { receipt: { outcome: 'deleted' } } : {}),
      } }
    },
  }
  return { launch, id, neighbor, report, reportPath, save, delivery, aggregate, mutations, servers,
    runtime: { api, devicePath: { publicClientId: report.publicClientId } } }
}

test('terminal failed tasks delete only their own MCP and preserve the original result evidence', async t => {
  const fixture = cleanupFixture(t)
  const original = readFileSync(fixture.reportPath, 'utf8')
  assert.deepEqual(await removeTerminalBenchmarkPublicSmoke(fixture.launch, fixture.runtime), { outcome: 'deleted' })
  assert.equal(fixture.servers.has(fixture.id), false)
  assert.equal(fixture.servers.has(fixture.neighbor), true)
  assert.equal(fixture.mutations.length, 1)
  assert.equal(readFileSync(fixture.reportPath, 'utf8'), original)
  const receipt = JSON.parse(readFileSync(resolve(fixture.launch.directory, 'public-smoke-removal.json'), 'utf8'))
  assert.deepEqual(receipt, { id: fixture.id, publicClientId: 'client-owned',
    readCursor: fixture.delivery.readCursor, receipt: { outcome: 'deleted' } })
  assert.deepEqual(await removeTerminalBenchmarkPublicSmoke(fixture.launch, fixture.runtime), { outcome: 'deleted' })
  assert.equal(fixture.mutations.length, 1, 'retained cleanup does not issue another mutation')
})

test('stale failed reports cannot remove public smoke while any role or item remains active', async t => {
  for (const state of ['queued', 'leased', 'running']) {
    const fixture = cleanupFixture(t)
    fixture.aggregate.runs.push({ id: 'role-still-active', state })
    assert.equal(await removeTerminalBenchmarkPublicSmoke(fixture.launch, fixture.runtime), null)
    assert.equal(fixture.mutations.length, 0)
    assert.equal(fixture.servers.has(fixture.id), true)
    assert.equal(existsSync(resolve(fixture.launch.directory, 'public-smoke-removal.json')), false)
  }
  const fixture = cleanupFixture(t)
  fixture.aggregate.items[0].state = 'ready'
  assert.equal(await removeTerminalBenchmarkPublicSmoke(fixture.launch, fixture.runtime), null)
  assert.equal(fixture.mutations.length, 0)
})

test('cleanup rejects mixed projections and foreign MCP or Device identities before deletion', async t => {
  for (const kind of ['cursor', 'server', 'device']) {
    const fixture = cleanupFixture(t)
    if (kind === 'cursor') fixture.aggregate.readCursor = { ...fixture.aggregate.readCursor, token: 'older' }
    if (kind === 'server') fixture.report.publicSmoke.id = fixture.neighbor
    if (kind === 'device') fixture.report.publicClientId = 'client-other'
    fixture.save()
    await assert.rejects(removeTerminalBenchmarkPublicSmoke(fixture.launch, fixture.runtime))
    assert.equal(fixture.mutations.length, 0)
    assert.equal(fixture.servers.has(fixture.id), true)
  }
})
