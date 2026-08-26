import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

import Ajv2020 from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'

const root = resolve(import.meta.dirname, '..')
const schemaPath = join(root, 'schema', 'winwincode', 'v1', 'execution-port.schema.json')
const domainSchemaPath = join(root, 'schema', 'winwincode', 'v1', 'domain.schema.json')
const validFixturePath = join(root, 'tests', 'fixtures', 'contracts', 'execution-port.valid.json')
const invalidFixturePath = join(root, 'tests', 'fixtures', 'contracts', 'execution-port.invalid.json')
const domainSchemaId = 'https://schemas.winwincode.dev/winwincode/v1/domain.schema.json'

const expectedKinds = [
  'worker.register',
  'worker.registration_result',
  'worker.capabilities',
  'worker.heartbeat',
  'worker.heartbeat_ack',
  'job.dispatch',
  'job.dispatch_result',
  'session.binding',
  'lease.renew',
  'runtime.event',
  'runtime.ack',
  'runtime.replay_request',
  'artifact.open',
  'artifact.chunk',
  'artifact.ack',
  'model.open',
  'model.chunk',
  'model.ack',
  'input.request',
  'input.response',
  'approval.request',
  'approval.decision',
  'job.cancel',
  'job.cancel_ack',
  'job.outcome',
  'job.outcome_ack',
]

const domainDefinitions = [
  'ApprovalId',
  'CodexThreadId',
  'DeliveryId',
  'DeliveryTaskId',
  'ExecutionJobId',
  'InputRequestId',
  'Instant',
  'InteractiveInputMode',
  'InteractiveInputValue',
  'LeaseId',
  'ProductSessionId',
  'RepositoryId',
  'RequestId',
  'SchemaVersion',
  'SessionBindingSourceIdentity',
  'SessionIdentity',
  'Sha256Digest',
  'StageRunId',
  'WorkerId',
  'WorkerInstanceId',
  'WorkerSessionId',
]

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function fallbackDomainSchema() {
  const id = prefix => ({
    type: 'string',
    pattern: `^${prefix}_[0-9A-HJKMNP-TV-Z]{26}$`,
  })
  const prefixes = {
    ApprovalId: 'apr',
    CodexThreadId: 'cdx',
    DeliveryId: 'dlv',
    DeliveryTaskId: 'dtk',
    LeaseId: 'lse',
    ProductSessionId: 'psn',
    RepositoryId: 'rep',
    RequestId: 'req',
    StageRunId: 'run',
    WorkerId: 'wrk',
    WorkerSessionId: 'wsn',
  }
  return {
    $schema: 'https://json-schema.org/draft/2020-12/schema',
    $id: domainSchemaId,
    $defs: {
      ...Object.fromEntries(Object.entries(prefixes).map(([name, prefix]) => [name, id(prefix)])),
      Instant: { type: 'string', format: 'date-time' },
      SchemaVersion: { const: 'winwincode/v1' },
      Sha256Digest: { type: 'string', pattern: '^sha256:[0-9a-f]{64}$' },
    },
  }
}

function validator(schema) {
  const ajv = new Ajv2020({ allErrors: true, strict: true })
  addFormats(ajv)
  for (const [keyword, schemaType] of [
    ['x-authority', 'string'],
    ['x-direction', 'string'],
    ['x-winwincode-semantics', 'object'],
    ['x-winwincode-transports', 'object'],
  ]) {
    ajv.addKeyword({ keyword, schemaType, valid: true })
  }
  ajv.addSchema(existsSync(domainSchemaPath) ? json(domainSchemaPath) : fallbackDomainSchema())
  return ajv.compile(schema)
}

function visit(node, callback, path = '#') {
  if (Array.isArray(node)) {
    node.forEach((value, index) => visit(value, callback, `${path}/${index}`))
    return
  }
  if (node === null || typeof node !== 'object') return
  callback(node, path)
  for (const [key, value] of Object.entries(node)) visit(value, callback, `${path}/${key}`)
}

function messageDefinitions(schema) {
  return schema.$defs.ExecutionPortMessage.oneOf.map(({ $ref }) => {
    assert.match($ref, /^#\/\$defs\/[A-Za-z][A-Za-z0-9]+$/u)
    return schema.$defs[$ref.slice('#/$defs/'.length)]
  })
}

test('ExecutionPort publishes one closed transport-neutral message union', () => {
  const schema = json(schemaPath)

  assert.equal(schema.$schema, 'https://json-schema.org/draft/2020-12/schema')
  assert.equal(schema.$id, 'https://schemas.winwincode.dev/winwincode/v1/execution-port.schema.json')
  assert.equal(schema.$ref, '#/$defs/ExecutionPortMessage')

  const messages = messageDefinitions(schema)
  assert.deepEqual(messages.map(message => message.properties.kind.const).sort(), [...expectedKinds].sort())

  visit(schema, (node, path) => {
    if (node.type === 'object') {
      assert.equal(node.additionalProperties, false, `${path} must reject undeclared fields`)
      assert.ok(Array.isArray(node.required), `${path} must declare required fields`)
    }
    if (typeof node.$ref === 'string' && !node.$ref.startsWith('#/')) {
      assert.match(node.$ref, /^\.\/domain\.schema\.json#\/\$defs\/[A-Za-z][A-Za-z0-9]+$/u)
    }
  })

  const externalDefinitions = new Set()
  visit(schema, node => {
    if (typeof node.$ref !== 'string' || node.$ref.startsWith('#/')) return
    externalDefinitions.add(node.$ref.slice(node.$ref.lastIndexOf('/') + 1))
  })
  assert.deepEqual([...externalDefinitions].sort(), domainDefinitions)
})

test('ExecutionPort accepts a positive sample for every message kind', () => {
  const schema = json(schemaPath)
  const validate = validator(schema)
  const fixture = json(validFixturePath)

  assert.deepEqual(fixture.messages.map(message => message.kind).sort(), [...expectedKinds].sort())
  for (const message of fixture.messages) {
    assert.equal(validate(message), true, `${message.kind}: ${JSON.stringify(validate.errors)}`)
  }
})

test('ExecutionPort rejects lease escape, canonical writes, secrets, and malformed ordering', () => {
  const validate = validator(json(schemaPath))
  const fixture = json(invalidFixturePath)

  assert.ok(fixture.cases.length >= 8)
  for (const invalidCase of fixture.cases) {
    assert.equal(validate(invalidCase.message), false, `${invalidCase.name} unexpectedly passed`)
    assert.ok(validate.errors.length > 0, `${invalidCase.name} did not report a schema error`)
  }
})

test('ExecutionPort makes every job-scoped Worker write lease-bound', () => {
  const schema = json(schemaPath)
  const workerLeaseWrites = messageDefinitions(schema).filter(
    definition => definition['x-direction'] === 'worker-to-control-plane'
      && definition['x-authority'] === 'lease-write',
  )

  assert.deepEqual(
    workerLeaseWrites.map(definition => definition.properties.kind.const).sort(),
    [
      'approval.request',
      'artifact.chunk',
      'artifact.open',
      'input.request',
      'job.cancel_ack',
      'job.dispatch_result',
      'job.outcome',
      'model.ack',
      'model.open',
      'runtime.event',
      'session.binding',
    ],
  )
  for (const definition of workerLeaseWrites) {
    assert.ok(definition.required.includes('lease'))
    assert.equal(definition.properties.lease.$ref, '#/$defs/ExecutionLeaseStamp')
  }
})

test('ExecutionPort supports default Chat jobs without inventing a Delivery', () => {
  const schema = json(schemaPath)
  const scope = schema.$defs.ExecutionScope
  const fixture = json(validFixturePath)

  assert.deepEqual(scope.oneOf.map(branch => branch.$ref), [
    '#/$defs/ProductSessionExecutionScope',
    '#/$defs/DeliveryStageExecutionScope',
  ])
  assert.deepEqual(schema.$defs.ProductSessionExecutionScope.required, [
    'kind',
    'productSessionId',
  ])
  assert.equal(
    schema.$defs.ProductSessionExecutionScope.properties.kind.const,
    'product-session',
  )
  assert.equal(schema.$defs.ProductSessionExecutionScope.properties.deliveryId, undefined)
  assert.equal(schema.$defs.ProductSessionExecutionScope.properties.stageRunId, undefined)
  assert.deepEqual(schema.$defs.DeliveryStageExecutionScope.required, [
    'kind',
    'productSessionId',
    'deliveryId',
    'stageRunId',
  ])
  assert.equal(
    schema.$defs.DeliveryStageExecutionScope.properties.kind.const,
    'delivery-stage',
  )
  assert.equal(
    schema.$defs.DeliveryStageExecutionScope.properties.reworkAuthorization.$ref,
    '#/$defs/DeliveryReworkAuthorizationScope',
  )
  assert.deepEqual(schema.$defs.DeliveryReworkAuthorizationScope.required, [
    'authorizationDigest',
    'candidateRef',
    'diffSha256',
    'sourceCandidateCommitId',
    'sourceCandidateTreeId',
    'requiresFullReverification',
    'targets',
  ])
  assert.equal(
    schema.$defs.DeliveryReworkAuthorizationScope.properties.targets.items.$ref,
    '#/$defs/DeliveryReworkTargetScope',
  )
  assert.deepEqual(schema.$defs.DeliveryReworkTargetScope.required, [
    'deliveryTaskId',
    'diagramId',
    'nodeId',
    'filePath',
    'sourceHunkSha256',
    'evidenceRefIds',
  ])

  const scopeSchema = {
    ...schema,
    $id: 'https://schemas.winwincode.dev/winwincode/v1/execution-scope.test.schema.json',
    $ref: '#/$defs/ExecutionScope',
  }
  const validate = validator(scopeSchema)
  assert.deepEqual(fixture.executionScopes.map(entry => entry.kind), [
    'product-session',
    'delivery-stage',
  ])
  for (const sample of fixture.executionScopes) {
    assert.equal(validate(sample), true, `${sample.kind}: ${JSON.stringify(validate.errors)}`)
  }
})

test('ProductSession runtime messages carry no fabricated StageRun identity', () => {
  const schema = json(schemaPath)
  const validate = validator(schema)
  const fixture = json(validFixturePath)
  const runtime = structuredClone(
    fixture.messages.find(message => message.kind === 'runtime.event'),
  )

  assert.ok(runtime)
  delete runtime.sessionIdentity.stageRunId
  assert.equal(
    validate(runtime),
    true,
    `ProductSession runtime event must omit StageRun: ${JSON.stringify(validate.errors)}`,
  )
})

test('ExecutionPort input response maps provided and empty terminal values exactly', () => {
  const schema = json(schemaPath)
  const fixture = json(validFixturePath)
  const validate = validator(schema)
  const provided = fixture.messages.find(message => message.kind === 'input.response')

  assert.ok(provided)
  assert.ok(schema.$defs.InputResponseMessage.required.includes('value'))
  assert.deepEqual(schema.$defs.InputResponseMessage.properties.value.oneOf, [
    { $ref: './domain.schema.json#/$defs/InteractiveInputValue' },
    { type: 'null' },
  ])
  assert.equal(validate(provided), true, JSON.stringify(validate.errors))
  assert.equal(validate({ ...provided, status: 'cancelled', value: null }), true)
  assert.equal(validate({ ...provided, status: 'expired', value: null }), true)
  assert.equal(validate({ ...provided, status: 'provided', value: null }), false)
  assert.equal(validate({ ...provided, status: 'cancelled' }), false)
  assert.equal(validate({ ...provided, status: 'cancelled', value: provided.value }), false)
})

test('ExecutionPort freezes retry, replay, restart, and fencing outcomes', () => {
  const semantics = json(schemaPath)['x-winwincode-semantics']

  assert.deepEqual(semantics, {
    duplicateDispatch: {
      condition: 'same_request_job_attempt_fence_and_payload_digest',
      result: 'duplicate',
      effect: 'reuse_worker_session_without_second_execution',
    },
    eventReplay: {
      condition: 'same_event_id_sequence_and_payload_digest',
      result: 'duplicate',
      effect: 'ack_without_second_persist_or_projection',
    },
    outOfOrderEvent: {
      condition: 'sequence_after_highest_contiguous_plus_one',
      result: 'gap',
      effect: 'keep_ack_and_request_replay_from_next_sequence',
    },
    reconnectResume: {
      condition: 'worker_reconnects_with_active_lease',
      result: 'replay_required',
      effect: 'replay_original_events_after_ack_sequence_in_order',
    },
    expiredLease: {
      condition: 'worker_write_after_expires_at',
      result: 'rejected_expired_lease',
      effect: 'persist_nothing',
    },
    workerRestart: {
      condition: 'worker_instance_id_changes',
      result: 'reacquire_required',
      effect: 'reject_prior_instance_writes_until_new_lease',
    },
    staleFencingToken: {
      condition: 'fencing_token_below_current_job_token',
      result: 'rejected_stale_fencing_token',
      effect: 'persist_nothing',
    },
    sessionBinding: {
      authority: 'accepted_session.binding',
      exactIdentity: [
        'productSessionId',
        'workerSessionId',
        'codexThreadId',
        'stageRunId',
        'executionJobId',
        'attempt',
        'workerId',
        'leaseId',
        'fencingToken',
        'sourceIdentity',
      ],
      duplicate: 'same_identity_is_idempotent_changed_identity_is_conflict',
      runtimeBeforeBinding: 'reject_before_persist_and_projection',
      runtimeThreadMismatch: 'reject_before_persist_and_projection',
      sessionIdentityBlock: 'sessionIdentity',
      sourceIdentity: 'SessionBindingSourceIdentity',
    },
    transportParity: {
      condition: 'local_or_remote_adapter',
      result: 'same_messages_and_outcomes',
      effect: 'transport_details_remain_outside_contract',
    },
    workerAuthority: {
      condition: 'worker_message',
      result: 'runtime_fact_or_lease_scoped_artifact_only',
      effect: 'canonical_product_state_remains_control_plane_owned',
    },
  })
})

test('ExecutionPort does not expose database, credential, transport, or Codex internals', () => {
  const schema = json(schemaPath)
  const forbiddenProperties = /^(?:apiKey|database|databaseRow|deliveryPatch|deliveryVerdict|grpcAddress|httpHeaders|organization|providerCredential|rbac|secret|socketPath|sql|table|turn|agentGraph|codexPlan)$/u
  const allowedCodexReferences = new Set(['codexThreadId'])

  visit(schema, (node, path) => {
    for (const property of Object.keys(node.properties ?? {})) {
      assert.doesNotMatch(property, forbiddenProperties, `${path} exposes ${property}`)
      if (/codex/iu.test(property)) {
        assert.ok(allowedCodexReferences.has(property), `${path} exposes Codex internal field ${property}`)
      }
    }
  })
})
