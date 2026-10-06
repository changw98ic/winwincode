import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { createHash } from 'node:crypto'
import test from 'node:test'
import { execFileSync } from 'node:child_process'
import { DatabaseSync } from 'node:sqlite'

import {
  DEVICE_ONLY_PREREQUISITES,
  FORBIDDEN_SERVER_MODEL_ENVIRONMENT_KEYS,
  assertDeviceSecretsNeverOnServer,
  assertServerEnvironmentIsDeviceOnly,
  configuredDeviceModelRoute,
  configuredModelRoute,
  deterministicDeviceProvider,
  deviceCredentialReferenceId,
  deviceOnlyServerEnvironment,
  deviceProviderSecretBundle,
} from '../scripts/acceptance/run-api-production-vertical.mjs'
import {
  runtimeChildEnvironment,
  resolveDeviceTaskApprovals,
  DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER,
  DETERMINISTIC_VERIFICATION_CALL_ID,
  DETERMINISTIC_VERIFICATION_POLL_CALL_ID,
  DETERMINISTIC_VERIFICATION_PROTOCOL,
  deterministicVerificationProduct,
  parseProcessExitCode,
  resolveVerificationObservation,
  observeToolProcess,
  deviceConnectCodePublished,
  deviceHelloAcknowledged,
  workInputFromRequest,
} from '../scripts/lib/device-production-fixture.mjs'

const root = resolve(import.meta.dirname, '..')
const runnerPath = resolve(root, 'scripts/acceptance/run-api-production-vertical.mjs')
const deviceTaskPath = resolve(root, 'scripts/acceptance/run-device-task-vertical.mjs')
const glmPath = resolve(root, 'scripts/benchmark/run-glm-ui-rework.mjs')
const fixturePath = resolve(root, 'scripts/lib/device-production-fixture.mjs')

test('reopening waits for the new daemon instance hello acknowledgement', () => {
  const database = new DatabaseSync(':memory:')
  try {
    database.exec(`CREATE TABLE device_identity (current_instance_id TEXT);
      CREATE TABLE client_outbox (kind TEXT, published INTEGER, client_instance_id TEXT);
      INSERT INTO device_identity VALUES ('old');
      INSERT INTO client_outbox VALUES ('client.hello', 1, 'old')`)
    assert.equal(deviceHelloAcknowledged(database, 'old'), false)
    assert.equal(deviceHelloAcknowledged(database, null), true)
    database.exec("UPDATE device_identity SET current_instance_id = 'new'; INSERT INTO client_outbox VALUES ('client.hello', 0, 'new')")
    assert.equal(deviceHelloAcknowledged(database, 'old'), false)
    database.exec("UPDATE client_outbox SET published = 1 WHERE client_instance_id = 'new'")
    assert.equal(deviceHelloAcknowledged(database, 'old'), true)
  } finally { database.close() }
})

test('Device-only prerequisite list covers the production acceptance path', () => {
  assert.deepEqual([...DEVICE_ONLY_PREREQUISITES], [
    'temporary-real-git-repository',
    'device-enroll-pair',
    'client-connect',
    'client-occupancy',
    'repository-binding',
    'device-local-provider',
    'worker-launch-grant',
    'launch-anchor',
    'chat-strongflow-cancel-restart',
  ])
})

test('device credential reference mirrors the Server route_reference formula', () => {
  const clientNodeId = 'cix_B2B2B2B2B2B2B2B2B2B2B2B2B2'
  const providerId = 'zhipu-glm'
  const digest = createHash('sha256')
    .update(`${clientNodeId}\n${providerId}`)
    .digest('hex')
    .toUpperCase()
  assert.equal(
    deviceCredentialReferenceId({ clientNodeId, providerId }),
    `crd_0${digest.slice(0, 25)}`,
  )
  const route = configuredDeviceModelRoute({
    clientNodeId,
    providerId,
    modelId: 'glm-5.3-flash',
  })
  assert.equal(route.providerId, 'zhipu-glm')
  assert.equal(route.modelId, 'glm-5.3-flash')
  assert.equal(route.credentialReferenceId, deviceCredentialReferenceId({ clientNodeId, providerId }))
})

test('configuredModelRoute is Device-bound and never reads Server model env', () => {
  const previous = {
    WWC_SERVER_MODEL_PROVIDER_ID: process.env.WWC_SERVER_MODEL_PROVIDER_ID,
    WWC_SERVER_MODEL_ID: process.env.WWC_SERVER_MODEL_ID,
    WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID: process.env.WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID,
  }
  try {
    process.env.WWC_SERVER_MODEL_PROVIDER_ID = 'zhipu-glm'
    process.env.WWC_SERVER_MODEL_ID = 'should-not-be-used'
    process.env.WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID = 'crd_server_should_not_use'
    const placeholder = configuredModelRoute({})
    assert.equal(placeholder.providerId, 'winwincode-device-deterministic')
    assert.notEqual(placeholder.credentialReferenceId, 'crd_server_should_not_use')
    const deviceBound = configuredModelRoute({
      clientNodeId: 'cix_ABC',
      providerId: 'zhipu-glm',
      modelId: 'glm-5.3-flash',
    })
    assert.equal(deviceBound.providerId, 'zhipu-glm')
    assert.equal(deviceBound.credentialReferenceId, deviceCredentialReferenceId({
      clientNodeId: 'cix_ABC',
      providerId: 'zhipu-glm',
    }))
  } finally {
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) delete process.env[key]
      else process.env[key] = value
    }
  }
})

test('Server environments with centralized model variables are rejected', () => {
  for (const key of FORBIDDEN_SERVER_MODEL_ENVIRONMENT_KEYS) {
    assert.throws(
      () => assertServerEnvironmentIsDeviceOnly({ [key]: 'present' }),
      new RegExp(key, 'u'),
    )
  }
  assertServerEnvironmentIsDeviceOnly(deviceOnlyServerEnvironment({
    WWC_DEBUG_RUNTIME: '1',
  }))
  assert.throws(
    () => deviceOnlyServerEnvironment({
      WWC_SERVER_MODEL_API_KEY: 'secret',
    }),
    /WWC_SERVER_MODEL_API_KEY/u,
  )
})

test('Device Provider secrets never appear in Server environment values', () => {
  const secret = 'glm-secret-value-not-for-server'
  assert.throws(
    () => assertDeviceSecretsNeverOnServer(
      { WWC_DEBUG_RUNTIME_LOG: `/tmp/${secret}.log` },
      [secret],
    ),
    /Device Provider secrets/u,
  )
  assertDeviceSecretsNeverOnServer(
    { WWC_DEBUG_RUNTIME_LOG: '/tmp/server-runtime.log' },
    [secret],
  )
  const bundle = deviceProviderSecretBundle({
    providerId: 'zhipu-glm',
    modelId: 'glm-5.3-flash',
    apiKey: secret,
  })
  assert.equal(bundle.apiKey, secret)
  assert.equal(Object.isFrozen(bundle), true)
})

test('deterministic Device Provider is the acceptance default', () => {
  const provider = deterministicDeviceProvider()
  assert.equal(provider.providerId, 'winwincode-device-deterministic')
  assert.equal(provider.modelId, 'device-deterministic-model')
  assert.equal(Object.isFrozen(provider), true)
})

test('production scripts no longer put model secrets on the Server environment', () => {
  for (const path of [runnerPath, deviceTaskPath, glmPath, fixturePath]) {
    const source = readFileSync(path, 'utf8')
    assert.equal(
      /WWC_SERVER_MODEL_API_KEY\s*:/.test(source),
      false,
      `${path} must not assign WWC_SERVER_MODEL_API_KEY`,
    )
    assert.equal(
      /WWC_SERVER_MODEL_ANTHROPIC_ENDPOINT\s*:/.test(source),
      false,
      `${path} must not assign WWC_SERVER_MODEL_ANTHROPIC_ENDPOINT`,
    )
    assert.equal(
      /WWC_SERVER_MODEL_PROVIDER_ID\s*:\s*'zhipu-glm'/.test(source),
      false,
      `${path} must not put zhipu-glm on the Server environment`,
    )
  }
})

test('runner exports Device-only helpers used by acceptance scripts', () => {
  const source = readFileSync(runnerPath, 'utf8')
  assert.match(source, /establishDeviceOnlyExecutionPath/u)
  assert.match(source, /devicePrerequisites/u)
  assert.match(source, /DEVICE_SESSION_REQUIRED|device-session-required|Device before sending/u)
})

function strongFlowWorkInputRequest(workInput) {
  return {
    messages: [{
      role: 'user',
      content: `${DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER}\nStrongFlow workInput (canonical JSON):\n${JSON.stringify(workInput)}\n`,
    }],
  }
}

test('Device fixture workInput parser reads WorkRun contract criterion and verification method', () => {
  const workInput = {
    candidateRef: 'git-candidate:sha256:abc',
    deliverySpecId: 'delivery-spec-FVCV3K8Q8830EB8BSPQ9Y1N1AG',
    deliverySpecRevision: 1,
    workContract: {
      criteria: [{
        id: 'crt_G180XH7PSZJVRVJMJWNJY2YKKS',
        verificationMethod: 'git rev-parse --verify HEAD',
      }],
    },
    workItem: {
      criterionIds: ['crt_G180XH7PSZJVRVJMJWNJY2YKKS'],
    },
  }
  const parsed = workInputFromRequest(strongFlowWorkInputRequest(workInput))
  assert.ok(parsed !== null)
  assert.deepEqual(parsed.criterionIds, ['crt_G180XH7PSZJVRVJMJWNJY2YKKS'])
  assert.equal(parsed.verificationCommand, 'git rev-parse --verify HEAD')
  assert.equal(parsed.deliverySpecId, 'delivery-spec-FVCV3K8Q8830EB8BSPQ9Y1N1AG')
  assert.equal(parsed.candidateRef, 'git-candidate:sha256:abc')
})

test('Device fixture verification product cites sealed evidence and contract criterion ids', () => {
  const product = JSON.parse(deterministicVerificationProduct({
    passed: true,
    workInput: {
      candidateRef: 'git-candidate:sha256:abc',
      deliverySpecId: 'delivery-spec-FVCV3K8Q8830EB8BSPQ9Y1N1AG',
      deliverySpecRevision: 1,
      criterionIds: ['crt_G180XH7PSZJVRVJMJWNJY2YKKS'],
    },
  }))
  assert.equal(product.protocol, DETERMINISTIC_VERIFICATION_PROTOCOL)
  assert.equal(product.findings[0].criterion_id, 'crt_G180XH7PSZJVRVJMJWNJY2YKKS')
  assert.equal(product.findings[0].verdict, 'pass')
  assert.equal(product.findings[0].evidence_sources[0].source_id, DETERMINISTIC_VERIFICATION_CALL_ID)
})

test('Device fixture does not invent fail when verification exit code is still unsealed', () => {
  assert.equal(parseProcessExitCode('Process running with session ID 14484\nOutput:\n'), null)
  assert.equal(parseProcessExitCode('Exit code: 0\n'), 0)
  const running = resolveVerificationObservation({
    messages: [{
      type: 'function_call_output',
      call_id: DETERMINISTIC_VERIFICATION_CALL_ID,
      output: 'Process running with session ID 14484\n',
    }],
  })
  assert.equal(running.exitCode, null)
  assert.equal(running.hasToolOutput, true)
  const sealed = resolveVerificationObservation({
    messages: [{
      type: 'function_call_output',
      call_id: DETERMINISTIC_VERIFICATION_POLL_CALL_ID,
      output: 'Exit code: 0\n',
    }, {
      type: 'function_call_output',
      call_id: DETERMINISTIC_VERIFICATION_CALL_ID,
      output: 'Process running with session ID 1\n',
    }],
  })
  assert.equal(sealed.exitCode, 0)
  assert.equal(sealed.evidenceSourceId, DETERMINISTIC_VERIFICATION_CALL_ID)
})

test('Device fixture polls the existing executor process until its actual exit', () => {
  const messages = [{ type: 'tool_result', tool_use_id: 'loopback-executor-change', content: 'Process running with session ID 42\n' }]
  let observed = observeToolProcess({ messages }, 'loopback-executor-change')
  assert.equal(observed.sessionId, 42)
  assert.equal(observed.exitCode, null)
  messages.push({ type: 'tool_result', tool_use_id: observed.nextCallId, content: 'Process running with session ID 42\n' })
  observed = observeToolProcess({ messages }, 'loopback-executor-change')
  assert.equal(observed.nextCallId, 'loopback-executor-change-poll-2')
  messages.push({ type: 'tool_result', tool_use_id: observed.nextCallId, content: 'Process exited with code 1\n' })
  observed = observeToolProcess({ messages }, 'loopback-executor-change')
  assert.equal(observed.sessionId, null)
  assert.equal(observed.exitCode, 1)
})

test('device connection waits for the exact code publication acknowledgement', () => {
  const database = new DatabaseSync(':memory:')
  try {
    database.exec('CREATE TABLE client_outbox (kind TEXT, published INTEGER, payload BLOB)')
    database.prepare('INSERT INTO client_outbox VALUES (?, ?, ?)').run(
      'client.connect_code.published', 0,
      Buffer.from(JSON.stringify({ payload: { connectCodeId: 'current-code' } })),
    )
    assert.equal(deviceConnectCodePublished(database, 'current-code'), false)
    database.exec('UPDATE client_outbox SET published = 1')
    assert.equal(deviceConnectCodePublished(database, 'other-code'), false)
    assert.equal(deviceConnectCodePublished(database, 'current-code'), true)
  } finally {
    database.close()
  }
})


test('runtime children do not inherit orchestration credentials or injected startup code', () => {
  const environment = runtimeChildEnvironment({
    PATH: '/usr/bin:/bin', HOME: '/tmp/isolated-benchmark-home', LANG: 'C',
    ZHIPU_API_KEY: 'synthetic-provider-secret', OPENCODE_SESSION_VALUE: 'synthetic-session',
    GH_TOKEN: 'synthetic-publisher-secret', PRIVATE_GRADER_CREDENTIAL: 'synthetic-grader-secret',
    WWC_DEVICE_PROVIDER_API_KEY: 'synthetic-device-secret',
    WWC_DEVICE_PROVIDER_HTTPS_PROXY: 'http://synthetic-user:synthetic-password@proxy.invalid:8080',
    HTTP_PROXY: 'http://synthetic-user:synthetic-password@proxy.invalid:8080',
    HTTPS_PROXY: 'http://synthetic-user:synthetic-password@proxy.invalid:8080',
    NODE_OPTIONS: '--eval=throw new Error("injected")',
    DYLD_INSERT_LIBRARIES: '/tmp/untrusted.dylib',
  })
  const actual = JSON.parse(execFileSync(process.execPath, ['--input-type=module', '-e',
    'process.stdout.write(JSON.stringify(process.env))'], { env: environment, encoding: 'utf8' }))
  // CoreFoundation adds this platform-owned setting on macOS process startup.
  delete actual.__CF_USER_TEXT_ENCODING
  assert.deepEqual(actual, { PATH: '/usr/bin:/bin', HOME: '/tmp/isolated-benchmark-home', LANG: 'C' })
})


test('Device task approves only installed public smoke once with an exact current WorkRun binding', async () => {
  const run = {
    id: 'run', state: 'running', productSessionId: 'product', codexThreadId: 'core',
    workerSessionId: 'worker', executionJobId: 'job',
  }
  const original = {
    id: 'approval', revision: 3, state: 'pending', decisionEnabled: true,
    category: 'mcp', effectiveDecisionScope: 'once',
    binding: {
      executionJobId: 'job', productSessionId: 'product', workerSessionId: 'worker',
      sessionIdentity: { workRunId: 'run', codexThreadId: 'core', productSessionId: 'product', workerSessionId: 'worker' },
    },
    sanitizedDetail: {
      kind: 'available', operation: 'execute', reasonCode: 'mcp_permission',
      targetCount: 1, targetSummaries: ['server:benchmark_public_smoke'],
    },
  }
  const cases = [
    ['approve', () => {}],
    ['reject', a => { a.category = 'exec' }],
    ['reject', a => { a.category = 'patch' }],
    ['reject', a => { a.effectiveDecisionScope = 'session' }],
    ['reject', a => { a.sanitizedDetail.reasonCode = 'unknown' }],
    ['reject', a => { a.sanitizedDetail.kind = 'unavailable' }],
    ['reject', a => { a.sanitizedDetail.operation = 'write' }],
    ['reject', a => { a.sanitizedDetail.targetSummaries = ['server:other'] }],
    ['reject', a => { a.sanitizedDetail.targetCount = 2 }],
    ['reject', a => { a.sanitizedDetail.targetSummaries.push('server:other') }],
    [null, a => { a.state = 'resolved' }],
    [null, a => { a.decisionEnabled = false }],
    ...['executionJobId', 'productSessionId', 'workerSessionId'].map(key =>
      [null, a => { a.binding[key] = 'foreign' }]),
    ...['workRunId', 'codexThreadId', 'productSessionId', 'workerSessionId'].map(key =>
      [null, a => { a.binding.sessionIdentity[key] = 'foreign' }]),
  ]
  for (const [expected, mutate] of cases) {
    const approval = structuredClone(original)
    mutate(approval)
    const decisions = []
    const receipts = []
    const api = {
      query: async name => ({ page: { hasMore: false }, result: name === 'approval.list' ? { items: [original] } : approval }),
      command: async (name, revision, payload) => {
        assert.equal(name, 'approval.decide')
        assert.equal(revision, 3)
        assert.deepEqual(payload.binding, approval.binding)
        decisions.push(payload.decision)
        return { outcome: 'completed' }
      },
    }
    await resolveDeviceTaskApprovals({ api, runs: [run], publicSmokeId: 'benchmark_public_smoke', onDecision: receipt => receipts.push(receipt) })
    assert.deepEqual(decisions, expected === null ? [] : [expected])
    assert.equal(receipts.length, decisions.length)
    if (receipts.length) assert.equal(receipts[0].decision, expected)
  }
  const api = {
    query: async name => ({ page: { hasMore: false }, result: name === 'approval.list' ? { items: [original] } : original }),
    command: async (_name, _revision, payload) => {
      assert.equal(payload.decision, 'reject', 'uninstalled public smoke cannot be approved')
      return { outcome: 'completed' }
    },
  }
  await resolveDeviceTaskApprovals({ api, runs: [run], onDecision: () => {} })
  api.command = async () => { throw Object.assign(new Error('changed'), { code: 'REVISION_CONFLICT' }) }
  await resolveDeviceTaskApprovals({ api, runs: [run], onDecision: () => assert.fail('no receipt for failed command') })
  api.command = async () => { throw new Error('transport lost') }
  await assert.rejects(resolveDeviceTaskApprovals({ api, runs: [run], onDecision: () => {} }), /transport lost/u)
  api.query = async () => ({ page: { hasMore: true }, result: { items: [original] } })
  await assert.rejects(resolveDeviceTaskApprovals({ api, runs: [run], onDecision: () => {} }), /must be complete/u)
})

test('authorized task Sessions automatically approve actions while preserving current authority checks', async () => {
  const run = { id: 'run', state: 'running', productSessionId: 'product', codexThreadId: 'core',
    workerSessionId: 'worker', executionJobId: 'job' }
  const original = {
    id: 'approval', revision: 3, state: 'pending', decisionEnabled: true,
    category: 'shell', effectiveDecisionScope: 'once',
    binding: { executionJobId: 'job', productSessionId: 'product', workerSessionId: 'worker',
      sessionIdentity: { workRunId: 'run', codexThreadId: 'core', productSessionId: 'product', workerSessionId: 'worker' } },
    sanitizedDetail: { kind: 'available', operation: 'execute', reasonCode: 'sandbox_escalation',
      workingDirectory: 'workspace', riskLevel: 'high', targetCount: 1,
      targetSummaries: ['program:zsh;argument_count:2'] },
  }
  const cases = [
    ['approve', () => {}],
    ['approve', a => { a.sanitizedDetail.reasonCode = 'network_access' }],
    ['approve', a => { a.category = 'network'; a.sanitizedDetail.reasonCode = 'network_access' }],
    ['approve', a => { a.category = 'filesystem_write'; Object.assign(a.sanitizedDetail,
      { operation: 'modify', reasonCode: 'filesystem_write', workingDirectory: null }) }],
    ['reject', a => { a.category = 'mcp'; a.sanitizedDetail.reasonCode = 'mcp_permission' }],
    ['reject', a => { a.sanitizedDetail.kind = 'unavailable' }],
    ['reject', a => { a.sanitizedDetail.reasonCode = 'unknown' }],
    ['reject', a => { a.sanitizedDetail.workingDirectory = 'unknown' }],
    ['reject', a => { a.sanitizedDetail.targetCount = 0 }],
    ['reject', a => { a.sanitizedDetail.targetSummaries = [] }],
    ['reject', a => { a.effectiveDecisionScope = 'session' }],
    [null, a => { a.state = 'expired' }],
    [null, a => { a.decisionEnabled = false }],
    ...['executionJobId', 'productSessionId', 'workerSessionId'].map(key =>
      [null, a => { a.binding[key] = 'foreign' }]),
    ...['workRunId', 'codexThreadId', 'productSessionId', 'workerSessionId'].map(key =>
      [null, a => { a.binding.sessionIdentity[key] = 'foreign' }]),
  ]
  for (const [expected, mutate] of cases) {
    const approval = structuredClone(original)
    mutate(approval)
    const decisions = []
    const api = {
      query: async name => ({ page: { hasMore: false }, result: name === 'approval.list' ? { items: [original] } : approval }),
      command: async (name, revision, payload) => {
        assert.equal(name, 'approval.decide'); assert.equal(revision, approval.revision)
        assert.deepEqual(payload.binding, approval.binding)
        decisions.push(payload.decision)
        return { outcome: 'completed' }
      },
    }
    await resolveDeviceTaskApprovals({ api, runs: [run], automaticTaskActions: true, onDecision: () => {} })
    assert.deepEqual(decisions, expected === null ? [] : [expected])
  }
  const api = { query: async name => ({ page: { hasMore: false },
    result: name === 'approval.list' ? { items: [original] } : original }),
  command: async () => assert.fail('terminal WorkRun cannot approve') }
  await resolveDeviceTaskApprovals({ api, runs: [{ ...run, state: 'cancelled' }],
    automaticTaskActions: true, onDecision: () => {} })
})
