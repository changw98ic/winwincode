// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { deviceAgentEnvironment, deviceAgentEnvironmentKeys,
  assertDeviceAgentEnvironmentForwarded } from '../scripts/device-agent-environment.mjs'
import { benchmarkDeviceEnvironment } from '../scripts/run-device-task-vertical.mjs'

const profile = { configurationId: 'main-A', track: 'main', fusion: false, jev: false,
  jevContext: false, jevJudge: false, fusionKind: null, reasoningEffort: 'max', budgetLimits: null }
const policy = {
  PYTHONDONTWRITEBYTECODE: '1',
  WWC_WORKER_APPROVAL_OWNER: 'execution_port',
  WWC_WORKER_MODEL_REASONING_EFFORT: 'max',
  WWC_WORKER_FUSION: '{"members":["local-fixture"]}',
  WWC_WORKER_JEV_JUDGE: 'local-judge',
  WWC_WORKER_JEV_CONTEXT: '{"provider":"local-context","policy":{"limit":1}}',
  WWC_DEVICE_JEV_SETTINGS_FILE: '/private/fixture-settings.json',
  WWC_DEVICE_PROVIDER_HTTPS_PROXY: 'http://fixture-user:fixture-password@proxy.invalid:8080',
  WWC_BENCHMARK_SEALED_TOOLS: '1',
}

test('Device policy contract forwards all current declared fields for both approval owners', () => {
  assert.equal(deviceAgentEnvironmentKeys.length, 9)
  for (const owner of ['core', 'execution_port']) {
    const input = { ...policy, WWC_WORKER_APPROVAL_OWNER: owner }
    const forwarded = deviceAgentEnvironment(input, { strict: true })
    assert.deepEqual(forwarded, input)
    assert.doesNotThrow(() => assertDeviceAgentEnvironmentForwarded(input, forwarded))
  }
  assert.equal(deviceAgentEnvironment({}).WWC_WORKER_APPROVAL_OWNER, 'core')
})

test('Device boundary filters ambient credentials and process control fields', () => {
  const source = { ...policy, API_KEY: 'fixture-credential', ZHIPU_API_KEY: 'fixture-credential',
    AUTH_TOKEN: 'fixture-token', NODE_OPTIONS: '--import=untrusted', HOME: '/private/home',
    HTTP_PROXY: 'http://ambient.invalid' }
  const output = deviceAgentEnvironment(source)
  assert.deepEqual(output, policy)
  assert.equal(JSON.stringify(output).includes('fixture-credential'), false)
  assert.equal(JSON.stringify(output).includes('fixture-token'), false)
  assert.equal(source.API_KEY, 'fixture-credential')
})

test('strict policy producer rejects undeclared fields with a safe stable error', () => {
  assert.throws(() => deviceAgentEnvironment({ ...policy, WWC_NEW_POLICY: 'fixture-secret' }, { strict: true }), error => {
    assert.equal(error.code, 'DEVICE_AGENT_ENVIRONMENT_FIELD_UNDECLARED')
    assert.equal(error.field, 'WWC_NEW_POLICY')
    assert.equal(error.message.includes('fixture-secret'), false)
    return true
  })
})

test('invalid approval owners are rejected without exporting their values', () => {
  for (const owner of ['', 'EXECUTION_PORT', 'fixture-secret']) {
    assert.throws(() => deviceAgentEnvironment({ ...policy, WWC_WORKER_APPROVAL_OWNER: owner }), error => {
      assert.equal(error.code, 'DEVICE_AGENT_APPROVAL_OWNER_INVALID')
      assert.equal(error.message, 'DEVICE_AGENT_APPROVAL_OWNER_INVALID')
      assert.equal(Object.hasOwn(error, 'field'), false)
      return true
    })
  }
})

test('every missing or changed policy field prevents dispatch and reports only its field', () => {
  for (const field of deviceAgentEnvironmentKeys) {
    for (const mutation of [observed => { delete observed[field] }, observed => { observed[field] = 'replacement-secret' }]) {
      const observed = { ...policy }
      mutation(observed)
      assert.throws(() => assertDeviceAgentEnvironmentForwarded(policy, observed), error => {
        assert.equal(error.code, 'DEVICE_AGENT_POLICY_NOT_FORWARDED')
        assert.equal(error.field, field)
        assert.equal(error.message, `DEVICE_AGENT_POLICY_NOT_FORWARDED: ${field}`)
        assert.equal(JSON.stringify(error).includes('replacement-secret'), false)
        assert.equal(error.message.includes('fixture-password'), false)
        return true
      })
    }
  }
})

test('benchmark policy producer selects the approval owner and ignores provider credentials', () => {
  for (const automaticTaskActions of [false, true]) {
    const environment = benchmarkDeviceEnvironment(profile, { automaticTaskActions }, {
      API_KEY: 'fixture-credential', WWC_DEVICE_PROVIDER_HTTPS_PROXY: policy.WWC_DEVICE_PROVIDER_HTTPS_PROXY,
    })
    assert.equal(environment.WWC_WORKER_APPROVAL_OWNER, automaticTaskActions ? 'execution_port' : 'core')
    assert.equal(environment.WWC_DEVICE_PROVIDER_HTTPS_PROXY, policy.WWC_DEVICE_PROVIDER_HTTPS_PROXY)
    assert.equal(environment.WWC_WORKER_MODEL_REASONING_EFFORT, 'max')
    assert.equal(environment.PYTHONDONTWRITEBYTECODE, '1')
    assert.deepEqual(Object.keys(environment), deviceAgentEnvironmentKeys)
    assert.equal(Object.hasOwn(environment, 'API_KEY'), false)
  }
})

test('real runtime rejects the old missing-owner process boundary before provisioning or a formal callback', t => {
  const directory = mkdtempSync(join(tmpdir(), 'device-agent-policy-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const runtimeUrl = process.env.WWC_TEST_DEVICE_TASK_RUNTIME_MODULE
    ?? new URL('../scripts/device-task-runtime.mjs', import.meta.url).href
  const apiUrl = new URL('./run-api-production-vertical.mjs', runtimeUrl).href
  const child = `
    import assert from 'node:assert/strict';
    import {mock} from 'node:test';
    const [runtimeUrl, apiUrl, directory, profileJson] = process.argv.slice(1);
    const original = await import(apiUrl);
    let externalEntryCalls = 0, formalCallbacks = 0, apiCalls = 0;
    const api = new Proxy({}, {get() { return async () => { apiCalls++; throw new Error('external request forbidden'); }; }});
    mock.module(apiUrl, {namedExports: {...original, runApiProductionVertical: async options => {
      externalEntryCalls++;
      const deviceEnvironment = {...options.deviceAgentEnvironment};
      assert.equal(deviceEnvironment.WWC_WORKER_APPROVAL_OWNER, 'execution_port');
      delete deviceEnvironment.WWC_WORKER_APPROVAL_OWNER;
      return options.scenario.run({api, devicePath: {deviceEnvironment}, repository: directory});
    }}});
    const {withDeviceTaskRuntime} = await import(runtimeUrl);
    await assert.rejects(withDeviceTaskRuntime({directory, profiles: [JSON.parse(profileJson)],
      providers: [{providerId: 'local-fixture', modelId: 'local-fixture', endpoint: 'https://provider.invalid',
        protocol: 'openai_chat_completions', apiKey: 'fixture-only'}],
      agentSettings: {}, automaticTaskActions: true, providerEnvironment: {}, build: false,
    }, async () => {formalCallbacks++; return 'forbidden-formal-run';}),
      {code: 'DEVICE_AGENT_POLICY_NOT_FORWARDED', field: 'WWC_WORKER_APPROVAL_OWNER'});
    assert.equal(externalEntryCalls, 1);
    assert.equal(apiCalls, 0);
    assert.equal(formalCallbacks, 0);
    process.stdout.write(JSON.stringify({externalEntryCalls, apiCalls, formalCallbacks}) + '\\n');
  `
  const output = execFileSync(process.execPath, ['--experimental-test-module-mocks', '--input-type=module',
    '-e', child, runtimeUrl, apiUrl, directory, JSON.stringify(profile)], { encoding: 'utf8', timeout: 15_000,
    env: { PATH: process.env.PATH, HOME: tmpdir(), TMPDIR: tmpdir() }, stdio: ['ignore', 'pipe', 'pipe'] })
  assert.deepEqual(JSON.parse(output.trim()), { externalEntryCalls: 1, apiCalls: 0, formalCallbacks: 0 })
})
