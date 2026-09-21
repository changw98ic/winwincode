import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { createHash } from 'node:crypto'
import test from 'node:test'

import {
  DEVICE_ONLY_PREREQUISITES,
  FORBIDDEN_SERVER_MODEL_ENVIRONMENT_KEYS,
  DEVICE_PROVIDER_KIND_DETERMINISTIC,
  DEVICE_PROVIDER_KIND_REAL,
  assertDeviceSecretsNeverOnServer,
  assertServerEnvironmentIsDeviceOnly,
  configuredDeviceModelRoute,
  configuredModelRoute,
  deterministicDeviceProvider,
  deviceCredentialReferenceId,
  deviceOnlyServerEnvironment,
  deviceProviderSecretBundle,
  normalizeAnthropicMessagesEndpoint,
  realDeviceProvider,
  selectDeviceProvider,
} from '../scripts/run-api-production-vertical.mjs'
import {
  DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER,
  DETERMINISTIC_VERIFICATION_CALL_ID,
  DETERMINISTIC_VERIFICATION_POLL_CALL_ID,
  DETERMINISTIC_VERIFICATION_PROTOCOL,
  deterministicVerificationProduct,
  parseProcessExitCode,
  resolveVerificationObservation,
  workInputFromRequest,
} from '../scripts/device-production-fixture.mjs'

const root = resolve(import.meta.dirname, '..')
const runnerPath = resolve(root, 'scripts/run-api-production-vertical.mjs')
const deviceTaskPath = resolve(root, 'scripts/run-device-task-vertical.mjs')
const glmPath = resolve(root, 'scripts/run-glm-ui-rework.mjs')
const fixturePath = resolve(root, 'scripts/device-production-fixture.mjs')

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
    // Pre-pairing real route keeps the selected Provider identity so Device
    // Worker env is not forced back to the deterministic fixture.
    const prePairingReal = configuredModelRoute({
      providerId: 'zhipu-glm',
      modelId: 'glm-5.3-flash',
      clientNodeId: null,
    })
    assert.equal(prePairingReal.providerId, 'zhipu-glm')
    assert.equal(prePairingReal.modelId, 'glm-5.3-flash')
    assert.equal(
      prePairingReal.credentialReferenceId,
      deviceCredentialReferenceId({
        clientNodeId: 'cix_device_session_required_placeholder',
        providerId: 'zhipu-glm',
      }),
    )
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
  assert.equal(provider.kind, DEVICE_PROVIDER_KIND_DETERMINISTIC)
  assert.equal(provider.fixture, true)
  assert.equal(Object.isFrozen(provider), true)
})

test('runner selects deterministic Device Provider when no secrets are present', () => {
  const provider = selectDeviceProvider({
    deviceProviderSecrets: [],
    deviceRoute: {
      providerId: 'zhipu-glm',
      modelId: 'glm-5.3-flash',
      clientNodeId: null,
      endpoint: 'https://open.bigmodel.cn/api/anthropic',
    },
  })
  assert.equal(provider.providerId, 'winwincode-device-deterministic')
  assert.equal(provider.modelId, 'device-deterministic-model')
  assert.equal(provider.kind, DEVICE_PROVIDER_KIND_DETERMINISTIC)
  assert.equal(provider.fixture, true)
})

test('runner selects device-local real Provider identity when secrets and modelRoute are present', () => {
  const provider = selectDeviceProvider({
    deviceProviderSecrets: ['secret-not-leaked-into-identity'],
    deviceRoute: {
      providerId: 'zhipu-glm',
      modelId: 'glm-5.3-flash',
      clientNodeId: null,
      endpoint: 'https://open.bigmodel.cn/api/anthropic',
    },
  })
  assert.equal(provider.kind, DEVICE_PROVIDER_KIND_REAL)
  assert.equal(provider.fixture, false)
  assert.equal(provider.providerId, 'zhipu-glm')
  assert.equal(provider.modelId, 'glm-5.3-flash')
  assert.equal(provider.protocol, 'anthropic_messages')
  assert.equal(provider.endpoint, 'https://open.bigmodel.cn/api/anthropic/v1/messages')
  assert.equal(Object.isFrozen(provider), true)
  // Identity never carries the API key.
  assert.equal(JSON.stringify(provider).includes('secret-not-leaked'), false)
})

test('real Device Provider identity selection still refuses Server model environment keys', () => {
  const previous = {
    WWC_SERVER_MODEL_PROVIDER_ID: process.env.WWC_SERVER_MODEL_PROVIDER_ID,
    WWC_SERVER_MODEL_ID: process.env.WWC_SERVER_MODEL_ID,
    WWC_SERVER_MODEL_API_KEY: process.env.WWC_SERVER_MODEL_API_KEY,
  }
  try {
    process.env.WWC_SERVER_MODEL_PROVIDER_ID = 'zhipu-glm'
    process.env.WWC_SERVER_MODEL_ID = 'glm-5.3-flash'
    process.env.WWC_SERVER_MODEL_API_KEY = 'server-side-should-be-ignored'
    const provider = selectDeviceProvider({
      deviceProviderSecrets: ['device-only-secret'],
      deviceRoute: {
        providerId: 'zhipu-glm',
        modelId: 'glm-5.3-flash',
        endpoint: 'https://open.bigmodel.cn/api/anthropic',
      },
      environment: {
        ZHIPU_BASE_URL: 'https://open.bigmodel.cn/api/anthropic',
        WWC_SERVER_MODEL_PROVIDER_ID: 'zhipu-glm',
        WWC_SERVER_MODEL_API_KEY: 'server-side-should-be-ignored',
      },
    })
    assert.equal(provider.kind, DEVICE_PROVIDER_KIND_REAL)
    assert.equal(provider.providerId, 'zhipu-glm')
    assert.equal(JSON.stringify(provider).includes('server-side-should-be-ignored'), false)
    assert.throws(
      () => assertServerEnvironmentIsDeviceOnly({
        WWC_SERVER_MODEL_API_KEY: 'server-side-should-be-ignored',
      }),
      /WWC_SERVER_MODEL_API_KEY/u,
    )
  } finally {
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) delete process.env[key]
      else process.env[key] = value
    }
  }
})

test('normalizeAnthropicMessagesEndpoint appends /v1/messages and rejects unsafe URLs', () => {
  assert.equal(
    normalizeAnthropicMessagesEndpoint('https://open.bigmodel.cn/api/anthropic'),
    'https://open.bigmodel.cn/api/anthropic/v1/messages',
  )
  assert.equal(
    normalizeAnthropicMessagesEndpoint('https://open.bigmodel.cn/api/anthropic/v1/messages'),
    'https://open.bigmodel.cn/api/anthropic/v1/messages',
  )
  assert.throws(() => normalizeAnthropicMessagesEndpoint('http://open.bigmodel.cn/api'), /https/u)
  assert.throws(() => normalizeAnthropicMessagesEndpoint('https://user:pass@example.com/v1'), /credentials/u)
})

test('realDeviceProvider freezes identity without embedding secrets', () => {
  const provider = realDeviceProvider({
    providerId: 'zhipu-glm',
    modelId: 'glm-5.3-flash',
    endpoint: 'https://open.bigmodel.cn/api/anthropic',
  })
  assert.equal(provider.kind, DEVICE_PROVIDER_KIND_REAL)
  assert.equal(provider.seedMode, 'encrypted-web-to-device-apply-real-endpoint')
  assert.equal(Object.isFrozen(provider), true)
  assert.equal('apiKey' in provider, false)
})

test('production scripts no longer hardcode deterministicDeviceProvider as the only seed identity', () => {
  const source = readFileSync(runnerPath, 'utf8')
  assert.match(source, /selectDeviceProvider\s*\(/u)
  assert.match(source, /deviceProviderEndpoint/u)
  assert.equal(
    /const deviceProvider = deterministicDeviceProvider\(\)/u.test(source),
    false,
    'run-api-production-vertical.mjs must select Provider identity from secrets+route',
  )
  const deviceTaskSource = readFileSync(deviceTaskPath, 'utf8')
  assert.match(deviceTaskSource, /deviceProviderEndpoint/u)
  assert.match(deviceTaskSource, /ZHIPU_BASE_URL/u)
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
  assert.equal(sealed.evidenceSourceId, DETERMINISTIC_VERIFICATION_POLL_CALL_ID)
})
