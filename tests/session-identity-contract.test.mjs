import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

import Ajv2020 from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'

const root = resolve(import.meta.dirname, '..')
const schemaRoot = join(root, 'schema', 'winwincode', 'v1')
const domain = JSON.parse(readFileSync(join(schemaRoot, 'domain.schema.json'), 'utf8'))
const execution = JSON.parse(readFileSync(join(schemaRoot, 'execution-port.schema.json'), 'utf8'))
const http = JSON.parse(readFileSync(join(schemaRoot, 'control-plane-http.schema.json'), 'utf8'))
const events = JSON.parse(readFileSync(join(schemaRoot, 'control-plane-events.schema.json'), 'utf8'))

const sessionExecutionDefinitions = [
  'RuntimeEventMessage',
  'RuntimeAckMessage',
  'RuntimeReplayRequestMessage',
  'ArtifactOpenMessage',
  'ArtifactChunkMessage',
  'ArtifactAckMessage',
  'ModelOpenMessage',
  'ModelChunkMessage',
  'ModelAckMessage',
  'InputRequestMessage',
  'InputResponseMessage',
  'ApprovalRequestMessage',
  'ApprovalDecisionMessage',
  'JobCancelMessage',
  'JobCancelAckMessage',
  'JobOutcomeMessage',
  'JobOutcomeAckMessage',
  'SessionBindingMessage',
]

const identity = {
  productSessionId: 'psn_00000000000000000000000001',
  workerSessionId: 'wsn_00000000000000000000000001',
  codexThreadId: 'cdx_00000000000000000000000001',
  workRunId: 'wrn_00000000000000000000000001',
}

const legacySessionKeys = [
  'sessionId',
  'dshSessionId',
  'codexSessionId',
  'session_id',
  'dsh_session_id',
  'codex_session_id',
]

const publicContractFiles = [
  'domain.schema.json',
  'execution-port.schema.json',
  'control-plane-http.schema.json',
  'control-plane-events.schema.json',
  'schema-collection.generated.json',
  'openapi.generated.json',
].map(name => join(schemaRoot, name)).concat([
  join(root, 'crates', 'winwincode-domain', 'src', 'generated.rs'),
  join(root, 'crates', 'winwincode-api', 'src', 'generated.rs'),
  join(root, 'apps', 'client', 'src', 'generated', 'contracts.ts'),
  join(root, 'apps', 'client', 'src', 'generated', 'control-plane-client.ts'),
])

function validator() {
  const ajv = new Ajv2020({ allErrors: true, strict: true })
  addFormats(ajv)
  ajv.addSchema(domain)
  return ajv.compile({
    $schema: domain.$schema,
    $id: `${domain.$id.replace(/\.json$/u, '')}/session-identity-test`,
    $ref: `${domain.$id}#/$defs/SessionIdentity`,
  })
}

test('SessionIdentity keeps three session identities and an optional Delivery WorkRun', () => {
  const definition = domain.$defs.SessionIdentity
  assert.ok(definition)
  assert.equal(definition.type, 'object')
  assert.equal(definition.additionalProperties, false)
  assert.deepEqual(definition.required, [
    'productSessionId',
    'workerSessionId',
    'codexThreadId',
  ])
  assert.deepEqual(
    Object.fromEntries(Object.entries(definition.properties).map(([name, value]) => [name, value.$ref])),
    {
      productSessionId: '#/$defs/ProductSessionId',
      workerSessionId: '#/$defs/WorkerSessionId',
      codexThreadId: '#/$defs/CodexThreadId',
      workRunId: '#/$defs/WorkRunId',
    },
  )

  const validate = validator()
  assert.equal(validate(identity), true, JSON.stringify(validate.errors))
  const productSessionIdentity = structuredClone(identity)
  delete productSessionIdentity.workRunId
  assert.equal(validate(productSessionIdentity), true, JSON.stringify(validate.errors))
  const foreignPrefix = {
    productSessionId: 'wsn_00000000000000000000000001',
    workerSessionId: 'cdx_00000000000000000000000001',
    codexThreadId: 'wrn_00000000000000000000000001',
    workRunId: 'psn_00000000000000000000000001',
  }
  for (const field of Object.keys(identity)) {
    assert.equal(validate({ ...identity, [field]: foreignPrefix[field] }), false,
      `${field} accepted a foreign canonical prefix`)
    const lowercase = identity[field].slice(0, -1) + 'a'
    assert.equal(validate({ ...identity, [field]: lowercase }), false,
      `${field} accepted a lowercase identity`)
    assert.equal(validate({ ...identity, [field]: identity[field].slice(0, -1) }), false,
      `${field} accepted a short identity`)
    assert.equal(validate({ ...identity, [field]: `${identity[field]}0` }), false,
      `${field} accepted an overlong identity`)
  }
  assert.equal(validate({ ...identity, unknown: true }), false)
  assert.equal(validate({ ...identity, workerSessionId: identity.codexThreadId }), false)
})

test('session-scoped ExecutionPort entries reuse the complete identity block', () => {
  for (const name of sessionExecutionDefinitions) {
    const definition = execution.$defs[name]
    assert.ok(definition, name)
    assert.deepEqual(
      definition.properties.sessionIdentity,
      { $ref: './domain.schema.json#/$defs/SessionIdentity' },
      `${name} must reference the shared block`,
    )
    assert.ok(definition.required.includes('sessionIdentity'), `${name} must require sessionIdentity`)
  }
})

test('session-scoped HTTP and WebSocket entries reuse the complete identity block', () => {
  for (const name of ['InputRespondPayload']) {
    const definition = http.$defs[name]
    if (name === 'InputRespondPayload') {
      assert.deepEqual(definition.properties.sessionIdentity, {
        $ref: './domain.schema.json#/$defs/SessionIdentity',
      }, name)
    }
    assert.ok(definition.required.includes('sessionIdentity'), name)
  }

  const deliveryInvalidation = events.$defs.ControlPlaneWebSocketWorkRunRuntimeProjectionInvalidatedEvent
  assert.deepEqual(deliveryInvalidation.properties.sessionIdentity, {
    $ref: './domain.schema.json#/$defs/SessionIdentity',
  })
  assert.ok(deliveryInvalidation.required.includes('sessionIdentity'))

  const workerSource = events.$defs.ControlPlaneWebSocketSessionExecutionWorkerSource
  assert.ok(workerSource)
  assert.deepEqual(workerSource.properties.sessionIdentity, {
    $ref: './domain.schema.json#/$defs/SessionIdentity',
  })
  assert.ok(workerSource.required.includes('sessionIdentity'))
})

function publicLegacyViolations(path) {
  const source = readFileSync(path, 'utf8')
  const violations = []
  for (const key of legacySessionKeys) {
    const pattern = new RegExp(`\\b${key}\\b`, 'g')
    if (pattern.test(source)) violations.push(key)
  }
  return violations
}

test('canonical public contract paths reject legacy session keys', () => {
  const violations = []
  for (const path of publicContractFiles) {
    assert.equal(existsSync(path), true, `missing public contract path: ${path}`)
    violations.push(...publicLegacyViolations(path).map(detail => `${path}: ${detail}`))
  }
  assert.deepEqual(violations, [])
})
