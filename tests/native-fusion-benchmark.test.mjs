// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { lstatSync, mkdtempSync, rmSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { buildBenchmarkPlan, executeBenchmarkCell } from '../scripts/run-real-task-benchmark.mjs'
import { benchmarkDeviceProfiles } from '../scripts/benchmark-device-adapter.mjs'
import { benchmarkDeviceEnvironment } from '../scripts/run-device-task-vertical.mjs'
import { deviceBenchmarkProfileKey } from '../scripts/device-agent-environment.mjs'
import { prepareDeviceBenchmarkProviderSlots } from '../scripts/device-task-runtime.mjs'

const environment = {
  ZHIPU_API_KEY: 'fixture-private-glm', ZHIPU_BASE_URL: 'https://glm.example.invalid', ZHIPU_MODEL: 'glm-5.3-flash',
  XIAOMI_API_KEY: 'fixture-private-mimo', XIAOMI_RESPONSES_URL: 'https://mimo.example.invalid/v1/responses', XIAOMI_MODEL: 'mimo-v2.6-pro',
  DEEPSEEK_API_KEY: 'fixture-private-deepseek', DEEPSEEK_BASE_URL: 'https://deepseek.example.invalid', DEEPSEEK_MODEL: 'deepseek-flash',
  OPENCODE_API_KEY: 'fixture-private-qwen', OPENCODE_BASE_URL: 'https://qwen.example.invalid/v1/chat/completions', OPENCODE_MODEL: 'qwen3.8-flash',
}
const plan = () => buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i}`) })

test('native Fusion comparison reaches one product task with all four panel routes and approval owner', async () => {
  const cell = plan().cells[4]
  let products = 0
  await executeBenchmarkCell(cell, { runModel: async request => {
    products++
    const policy = benchmarkDeviceEnvironment(request, { automaticTaskActions: true }, environment)
    assert.equal(policy.WWC_WORKER_APPROVAL_OWNER, 'execution_port')
    assert.equal(policy.WWC_BENCHMARK_SEALED_TOOLS, '1')
    assert.equal(request.executionFusion, true)
    assert.deepEqual(JSON.parse(policy.WWC_WORKER_FUSION).members.map(row => row.model),
      ['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash'])
    assert.equal(JSON.stringify(policy).includes('fixture-private'), false)
    return { status: 'completed', nativePanelFixture: true }
  }, aggregate: () => assert.fail('the producer may not submit an extra aggregation task') })
  assert.equal(products, 1)
})

test('legacy independent aggregation cannot dispatch in a new formal Fusion cell', async () => {
  const cell = { ...plan().cells[4], fusionKind: 'independent-aggregate' }
  let sends = 0
  await assert.rejects(executeBenchmarkCell(cell, {
    runModel: () => { sends++ }, aggregate: () => { sends++ },
  }), { code: 'BENCHMARK_FUSION_TOPOLOGY_INVALID' })
  assert.equal(sends, 0)
})

test('missing or contradictory native Fusion policy is rejected before any product send', async () => {
  for (const change of [{ executionFusion: false }, { fusionKind: null }]) {
    let sends = 0
    await assert.rejects(executeBenchmarkCell({ ...plan().cells[4], ...change }, { runModel: () => { sends++ } }),
      { code: 'BENCHMARK_FUSION_TOPOLOGY_INVALID' })
    assert.equal(sends, 0)
  }
  assert.throws(() => benchmarkDeviceEnvironment({ ...plan().cells[4], executionFusion: false }, {}, environment),
    { code: 'BENCHMARK_FUSION_TOPOLOGY_INVALID' })
})

test('effective Fusion policies use separate immutable Device profiles only when required', () => {
  const selected = { cells: plan().cells.filter(cell => ['main-A', 'main-C'].includes(cell.configurationId)) }
  const profiles = benchmarkDeviceProfiles(selected, { providerEnvironment: environment })
  assert.deepEqual(profiles.map(deviceBenchmarkProfileKey), ['main-A', 'main-A--native-panel', 'main-C'])
  assert.equal(benchmarkDeviceEnvironment(profiles[0], {}, environment).WWC_WORKER_FUSION, undefined)
  for (const profile of profiles.slice(1)) {
    assert.equal(JSON.parse(benchmarkDeviceEnvironment(profile, {}, environment).WWC_WORKER_FUSION).members.length, 4)
  }
})

test('expanded native panel Devices share the same three physical Provider lock slots', t => {
  const directory = mkdtempSync(join(tmpdir(), 'native-fusion-slots-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const selected = { cells: plan().cells.filter(cell => ['main-A', 'main-C'].includes(cell.configurationId)) }
  const profiles = benchmarkDeviceProfiles(selected, { providerEnvironment: environment })
  const providers = [{ providerId: 'fixture-provider' }]
  prepareDeviceBenchmarkProviderSlots({ directory, profiles, providers })
  const digest = createHash('sha256').update(providers[0].providerId).digest('hex')
  for (let slot = 0; slot < 3; slot++) {
    const shared = lstatSync(join(directory, 'model-provider-slots', digest, `slot-${slot}`))
    assert.equal(shared.nlink, profiles.length + 1)
    for (const [index, profile] of profiles.entries()) {
      const data = index === 0 ? join(directory, 'device-data') : join(directory, 'devices', deviceBenchmarkProfileKey(profile))
      const device = lstatSync(join(data, 'providers/model-provider-slots', digest, `slot-${slot}`))
      assert.deepEqual([device.dev, device.ino], [shared.dev, shared.ino])
      assert.equal(device.mode & 0o777, 0o600)
    }
  }
})
