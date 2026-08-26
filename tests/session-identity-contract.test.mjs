import assert from 'node:assert/strict'
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs'
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
  stageRunId: 'run_00000000000000000000000001',
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
  join(root, 'apps', 'web', 'src', 'generated', 'contracts.ts'),
  join(root, 'apps', 'web', 'src', 'generated', 'control-plane-client.ts'),
])

// Migration fixtures are the only public test input directory allowed to
// carry the old names while the one-time conversion is exercised.
const migrationInputAllowlist = [
  join(root, 'tests', 'fixtures', 'session-identity-migration'),
]

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

test('SessionIdentity keeps three session identities and an optional Delivery StageRun', () => {
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
      stageRunId: '#/$defs/StageRunId',
    },
  )

  const validate = validator()
  assert.equal(validate(identity), true, JSON.stringify(validate.errors))
  const productSessionIdentity = structuredClone(identity)
  delete productSessionIdentity.stageRunId
  assert.equal(validate(productSessionIdentity), true, JSON.stringify(validate.errors))
  const foreignPrefix = {
    productSessionId: 'wsn_00000000000000000000000001',
    workerSessionId: 'cdx_00000000000000000000000001',
    codexThreadId: 'run_00000000000000000000000001',
    stageRunId: 'psn_00000000000000000000000001',
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
  for (const name of ['InputRespondPayload', 'DeliveryStageSessionBindingProjection']) {
    const definition = http.$defs[name]
    if (name === 'InputRespondPayload') {
      assert.deepEqual(definition.properties.sessionIdentity, {
        $ref: './domain.schema.json#/$defs/SessionIdentity',
      }, name)
    } else {
      assert.deepEqual(definition.properties.sessionIdentity, {
        oneOf: [
          { $ref: './domain.schema.json#/$defs/SessionIdentity' },
          { type: 'null' },
        ],
      }, name)
    }
    assert.ok(definition.required.includes('sessionIdentity'), name)
  }

  const deliveryInvalidation = events.$defs.ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent
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

function filesUnder(directory) {
  if (!existsSync(directory)) return []
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name)
    return entry.isDirectory() ? filesUnder(path) : [path]
  })
}

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

  // Keep migration input explicit: it is never traversed as a public output.
  for (const directory of migrationInputAllowlist) {
    assert.equal(existsSync(directory), true, `missing migration input allowlist: ${directory}`)
    for (const path of filesUnder(directory)) assert.equal(statSync(path).isFile(), true)
  }
})

test('the differential seed translator delegates legacy sessions to the canonical migration', () => {
  const path = join(
    root,
    'crates',
    'winwincode-control-plane',
    'tests',
    'support',
    'differential_runner.rs',
  )
  const source = readFileSync(path, 'utf8')
  const start = source.indexOf('fn migrate_legacy_snapshot')
  const end = source.indexOf('\nfn visit_legacy_task_graph', start)
  assert.notEqual(start, -1, 'missing differential seed migration')
  assert.notEqual(end, -1, 'differential seed migration boundary changed')
  const migration = source.slice(start, end)

  assert.match(migration, /migrate_legacy_delivery_json\(/u)
  for (const duplicatePath of [
    'dshSessionId',
    'codexSessionId',
    'canonical_id("psn_"',
    'canonical_id("wsn_"',
    'canonical_id("cdx_"',
  ]) {
    assert.equal(
      migration.includes(duplicatePath),
      false,
      `differential seed migration duplicated ${duplicatePath}`,
    )
  }
})
