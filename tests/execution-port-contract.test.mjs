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
const productSessionBindingFixturePath = join(
  root,
  'tests',
  'fixtures',
  'contracts',
  'session-binding.product-session.valid.json',
)
const deliveryStageBindingFixturePath = join(
  root,
  'tests',
  'fixtures',
  'contracts',
  'session-binding.delivery-stage.valid.json',
)
const domainSchemaId = 'https://schemas.winwincode.dev/winwincode/v1/domain.schema.json'

const expectedKinds = [
  'action.enforcement_request',
  'action.enforcement_receipt',
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
  'ChangeBatchId',
  'CodexThreadId',
  'DeliveryId',
  'DeliveryTaskId',
  'ExecutionJobId',
  'InputRequestId',
  'Instant',
  'InteractiveInputMode',
  'InteractiveInputValue',
  'LeaseId',
  'ObservationId',
  'ProductSessionId',
  'RepositoryId',
  'RepositoryScope',
  'RequestId',
  'SchemaVersion',
  'SessionBindingSourceIdentity',
  'SessionIdentity',
  'Sha256Digest',
  'StageRunId',
  'UserActor',
  'WorkerId',
  'WorkerInstanceId',
  'WorkerSessionId',
  'WorkspaceRevision',
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
      ObservationId: { type: 'string', pattern: '^sha256:[0-9a-f]{64}$' },
    },
  }
}

function validator(schema, definitionName) {
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
  if (definitionName !== undefined) {
    ajv.addSchema(schema)
    return ajv.compile({ $ref: `${schema.$id}#/$defs/${definitionName}` })
  }
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

test('ExecutionPort seals typed stage input only on Delivery jobs', () => {
  const validate = validator(json(schemaPath))
  const fixture = json(validFixturePath)
  const dispatch = structuredClone(
    fixture.messages.find(message => message.kind === 'job.dispatch'),
  )
  assert.ok(dispatch)

  const missing = structuredClone(dispatch)
  delete missing.job.stageInput
  assert.equal(validate(missing), false, 'Delivery job requires typed stageInput')

  const chatWithStageInput = structuredClone(dispatch)
  chatWithStageInput.job.scope = {
    kind: 'product-session',
    productSessionId: dispatch.job.scope.productSessionId,
  }
  assert.equal(
    validate(chatWithStageInput),
    false,
    'ProductSession job rejects Delivery stageInput',
  )

  delete chatWithStageInput.job.stageInput
  assert.equal(
    validate(chatWithStageInput),
    true,
    `ProductSession job keeps its plain goal: ${JSON.stringify(validate.errors)}`,
  )
})

test('ExecutionPort carries one sealed replacement lineage on replacement dispatches', () => {
  const validate = validator(json(schemaPath))
  const fixture = json(validFixturePath)
  const firstDispatch = structuredClone(
    fixture.messages.find(message => message.kind === 'job.dispatch'),
  )
  assert.ok(firstDispatch)
  assert.equal(firstDispatch.replacementAuthority, null)
  assert.equal(validate(firstDispatch), true, JSON.stringify(validate.errors))

  const missing = structuredClone(firstDispatch)
  delete missing.replacementAuthority
  assert.equal(validate(missing), false, 'replacementAuthority is present even when null')

  const replacement = structuredClone(firstDispatch)
  replacement.job.attempt = 2
  replacement.lease = {
    ...replacement.lease,
    leaseId: 'lse_0000000000000000000000000B',
    workerInstanceId: 'wki_0000000000000000000000000C',
    attempt: 2,
    fencingToken: '43',
    issuedAt: '2026-08-24T12:11:00.000Z',
    expiresAt: '2026-08-24T12:21:00.000Z',
  }
  replacement.replacementAuthority = {
    receiptId: 'req_0000000000000000000000000D',
    receiptDigest: 'sha256:1111111111111111111111111111111111111111111111111111111111111111',
    logicalJobDigest: 'sha256:2222222222222222222222222222222222222222222222222222222222222222',
    scope: replacement.job.scope,
    predecessorLease: firstDispatch.lease,
    predecessorSessionIdentity: {
      productSessionId: replacement.job.scope.productSessionId,
      workerSessionId: 'wsn_0000000000000000000000000E',
      codexThreadId: 'cdx_0000000000000000000000000F',
      stageRunId: replacement.job.scope.stageRunId,
    },
    successorLease: replacement.lease,
    createdAt: '2026-08-24T12:11:00.000Z',
  }
  assert.equal(validate(replacement), true, JSON.stringify(validate.errors))

  replacement.replacementAuthority.predecessorSessionIdentity = null
  assert.equal(
    validate(replacement),
    true,
    'leased replacements can precede WorkerSession creation',
  )
})

test('ExecutionPort workspace write mode distinguishes readers from writers', () => {
  const validate = validator(json(schemaPath))
  const fixture = json(validFixturePath)
  const dispatch = structuredClone(
    fixture.messages.find(message => message.kind === 'job.dispatch'),
  )
  assert.ok(dispatch)
  dispatch.job.workspace.writeMode = 'read-only'
  assert.equal(validate(dispatch), true, JSON.stringify(validate.errors))
  dispatch.job.workspace.writeMode = 'unrestricted'
  assert.equal(validate(dispatch), false)
})

test('ExecutionPort requires measured safe-integer usage only for succeeded outcomes', () => {
  const validate = validator(json(schemaPath))
  const fixture = json(validFixturePath)
  const succeeded = fixture.messages.find(message => message.kind === 'job.outcome')
  assert.ok(succeeded, 'positive fixture includes one terminal Job outcome')

  const missingUsage = structuredClone(succeeded)
  delete missingUsage.outcome.usage
  assert.equal(validate(missingUsage), false, 'succeeded outcome requires usage')

  const unsafeUsage = structuredClone(succeeded)
  unsafeUsage.outcome.usage.tokens = 9_007_199_254_740_992
  assert.equal(validate(unsafeUsage), false, 'usage is bounded to JavaScript safe integers')

  const cancelled = structuredClone(missingUsage)
  cancelled.outcome.status = 'cancelled'
  assert.equal(validate(cancelled), true, 'cancelled outcome does not fabricate usage')
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

test('SessionBinding carries StageRun only for DeliveryStage jobs', () => {
  const schema = json(schemaPath)
  const validate = validator(schema)
  const productSession = json(productSessionBindingFixturePath)
  const deliveryStage = json(deliveryStageBindingFixturePath)

  assert.equal(schema.$defs.SessionBindingMessage.required.includes('stageRunId'), false)
  assert.equal(validate(productSession), true, JSON.stringify(validate.errors))
  assert.equal(Object.hasOwn(productSession, 'stageRunId'), false)
  assert.equal(Object.hasOwn(productSession.sessionIdentity, 'stageRunId'), false)

  assert.equal(validate(deliveryStage), true, JSON.stringify(validate.errors))
  assert.equal(deliveryStage.stageRunId, deliveryStage.sessionIdentity.stageRunId)
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

test('ChangeBatch contracts have one generated closed and bounded schema surface', () => {
  const schema = json(schemaPath)
  const openapi = json(join(root, 'schema/winwincode/v1/openapi.generated.json'))
  const schemaCollection = json(
    join(root, 'schema/winwincode/v1/schema-collection.generated.json'),
  )
  const normalizeOpenApiReferences = value => JSON.parse(
    JSON.stringify(value).replaceAll('#/components/schemas/', '#/$defs/'),
  )
  const definitions = [
    'AppliedFileSummary',
    'ChangeBatchIdentity',
    'ChangeBatchProgressEvent',
    'ChangeBatchProgressState',
    'ChangeBatchProposal',
    'ChangeBatchProposalEvent',
    'ChangeBatchReceipt',
    'DiagnosticBaseline',
    'DiagnosticBaselineComparison',
    'DiagnosticCategory',
    'DiagnosticChangeStatus',
    'DiagnosticComparisonEntry',
    'DiagnosticParserVersion',
    'DiagnosticSeverity',
    'FinalCandidateFreezeFact',
    'NormalizedDiagnostic',
    'NormalizerReceipt',
    'ObservationAcceptanceCriterion',
    'ObservationDataEgressPolicy',
    'ObservationDecision',
    'ObservationDeltaSummary',
    'ObservationFailedTestSummary',
    'ObservationIntent',
    'ObservationPromptInjectionScan',
    'ObservationPromptInjectionStatus',
    'ObservationReasonCode',
    'ObservationReceipt',
    'ObservationRequest',
    'ObservationResponse',
    'ObservationSecretScan',
    'ObservationSecretScanStatus',
    'ObservationSnippet',
    'ObservationSource',
    'ObservationUntrustedInput',
    'RepairClass',
    'RepairEnvelope',
    'RepairLoopBudget',
    'RepairLoopContextPack',
    'RepairLoopCounters',
    'RepairLoopStopReason',
    'RoleExecutionMode',
    'RoleSessionPolicy',
    'ValidationCommandLanguage',
    'ValidationCommandPhase',
    'ValidationCommandSpec',
    'ValidationConfiguration',
    'ValidationEnvironmentName',
    'ValidationEnvironmentVariable',
    'ValidationProfile',
    'ValidationProfileName',
    'ValidationProfileSelection',
    'ValidationProfileSelectionReasonCode',
    'ValidationSelectionSource',
    'ValidationReceipt',
  ]

  for (const name of definitions) {
    assert.ok(schema.$defs[name], `${name} must exist in the canonical source`)
    assert.deepEqual(
      normalizeOpenApiReferences(openapi.components.schemas[name]),
      schemaCollection.$defs[name],
      `${name} must have the same generated OpenAPI and JSON Schema shape`,
    )
    if (schema.$defs[name].type === 'object') {
      assert.equal(schema.$defs[name].additionalProperties, false)
    }
  }

  assert.deepEqual(
    openapi.components.schemas.WorkspaceRevision,
    schemaCollection.$defs.WorkspaceRevision,
  )
  assert.equal(
    schemaCollection.$defs.WorkspaceRevision.pattern,
    '^git-tree:(?:[0-9a-f]{40}|[0-9a-f]{64})$',
  )
  assert.equal(schema.$defs.ExecutionWorkspace.properties.checkoutRevision.type, 'string')
  assert.equal(schema.$defs.ExecutionWorkspace.properties.checkoutRevision.$ref, undefined)

  assert.deepEqual(schema.$defs.ChangeBatchProgressState.enum, [
    'proposed',
    'authorized',
    'apply_started',
    'applied',
    'rollback_started',
    'rolled_back',
    'validation_started',
    'validation_completed',
    'observation_requested',
    'observation_completed',
    'accepted',
    'repair_required',
    'infrastructure_failed',
  ])

  const typescript = readFileSync(
    join(root, 'apps/client/src/generated/contracts.ts'),
    'utf8',
  )
  const rust = readFileSync(
    join(root, 'crates/winwincode-execution-port/src/generated.rs'),
    'utf8',
  )
  for (const name of [
    'ChangeBatchProgressState',
    'ChangeBatchReceiptStatus',
    'DiagnosticCategory',
    'DiagnosticChangeStatus',
    'DiagnosticParserVersion',
    'DiagnosticSeverity',
    'ObservationDecision',
    'ObservationPromptInjectionStatus',
    'ObservationReasonCode',
    'ObservationSecretScanStatus',
    'ObservationSource',
    'RepairClass',
    'RepairLoopStopReason',
    'RoleExecutionMode',
    'ValidationCommandLanguage',
    'ValidationCommandPhase',
    'ValidationEnvironmentName',
    'ValidationProfileName',
    'ValidationProfileSelectionReasonCode',
    'ValidationSelectionSource',
    'ValidationReceiptStatus',
  ]) {
    const typescriptEnum = typescript.match(
      new RegExp(`export enum ${name} \\{([\\s\\S]*?)\\n\\}`, 'u'),
    )?.[1]
    const rustEnum = rust.match(
      new RegExp(`pub enum ${name} \\{([\\s\\S]*?)\\n\\}`, 'u'),
    )?.[1]
    assert.ok(typescriptEnum, `${name} must be generated for TypeScript`)
    assert.ok(rustEnum, `${name} must be generated for Rust`)
    for (const value of schema.$defs[name].enum) {
      assert.match(typescriptEnum, new RegExp(`= ${JSON.stringify(value)}`, 'u'))
      assert.match(rustEnum, new RegExp(`serde\\(rename = ${JSON.stringify(value)}\\)`, 'u'))
    }
  }

  const validateProposal = validator(schema, 'ChangeBatchProposal')
  const proposal = {
    schemaVersion: 1,
    disposition: 'final',
    validationProfile: 'fast',
    patch: '*** Begin Patch\n*** End Patch\n',
    acceptanceCriteriaIds: ['criterion-1'],
  }
  assert.equal(validateProposal(proposal), true, JSON.stringify(validateProposal.errors))
  assert.equal(validateProposal({ ...proposal, unknownField: true }), false)
  assert.equal(validateProposal({ ...proposal, schemaVersion: 2 }), false)
  assert.equal(validateProposal({ ...proposal, disposition: 'maybe' }), false)
  assert.equal(validateProposal({ ...proposal, patch: 'x'.repeat(524_289) }), false)
  assert.equal(validateProposal({ ...proposal, validationProfile: 'not valid' }), false)
  assert.equal(validateProposal({ ...proposal, acceptanceCriteriaIds: ['not valid'] }), false)
  assert.equal(validateProposal({
    ...proposal,
    acceptanceCriteriaIds: ['criterion-1', 'criterion-1'],
  }), false)

  const validationCommand = {
    id: 'typescript-check',
    phase: 'validation',
    language: 'typescript',
    argv: ['corepack', 'pnpm', 'typecheck'],
    workingDirectory: '.',
    allowedCompanionPaths: [],
    environment: [],
    network: false,
    timeoutMillis: 300_000,
    outputLimitBytes: 1_048_576,
  }
  const validateCommand = validator(schema, 'ValidationCommandSpec')
  assert.equal(validateCommand(validationCommand), true, JSON.stringify(validateCommand.errors))
  assert.equal(validateCommand({ ...validationCommand, unknownField: true }), false)
  assert.equal(validateCommand({
    ...validationCommand,
    diagnosticParserVersion: 'typescript_v1',
  }), true)
  assert.equal(validateCommand({
    ...validationCommand,
    diagnosticParserVersion: 'typescript_v2',
  }), false)
  assert.equal(validateCommand({ ...validationCommand, network: true }), false)
  assert.equal(validateCommand({ ...validationCommand, timeoutMillis: 86_400_001 }), false)
  assert.equal(validateCommand({ ...validationCommand, outputLimitBytes: 16_777_217 }), false)
  assert.equal(validateCommand({ ...validationCommand, argv: Array(257).fill('x') }), false)
  assert.equal(validateCommand({
    ...validationCommand,
    allowedCompanionPaths: ['generated/file.ts'],
  }), false)
  assert.equal(validateCommand({
    ...validationCommand,
    phase: 'codegen',
    allowedCompanionPaths: ['generated/file.ts'],
  }), true)
  assert.equal(validateCommand({
    ...validationCommand,
    phase: 'codegen',
    diagnosticParserVersion: 'typescript_v1',
    allowedCompanionPaths: ['generated/file.ts'],
  }), false)
  assert.equal(validateCommand({
    ...validationCommand,
    phase: 'codegen',
    allowedCompanionPaths: ['../generated/file.ts'],
  }), false)
  for (const path of ['.git/config', 'CON', 'trailing.', 'wild*card']) {
    assert.equal(validateCommand({
      ...validationCommand,
      phase: 'codegen',
      allowedCompanionPaths: [path],
    }), false, path)
  }

  const digest = `sha256:${'1'.repeat(64)}`
  const diagnostic = {
    diagnosticId: digest,
    parserVersion: 'typescript_v1',
    path: 'src/example.ts',
    code: 'TS2304',
    severity: 'error',
    line: 1,
    column: null,
    category: 'missing_symbol',
    messageDigest: digest,
    display: 'Cannot find name',
  }
  const validateDiagnostic = validator(schema, 'NormalizedDiagnostic')
  assert.equal(validateDiagnostic(diagnostic), true, JSON.stringify(validateDiagnostic.errors))
  assert.equal(validateDiagnostic({ ...diagnostic, unknown: true }), false)
  assert.equal(validateDiagnostic({ ...diagnostic, line: 0 }), false)
  assert.equal(validateDiagnostic({ ...diagnostic, line: undefined }), false)
  assert.equal(validateDiagnostic({ ...diagnostic, path: '../secret' }), false)
  assert.equal(validateDiagnostic({ ...diagnostic, category: 'maybe' }), false)
  assert.equal(validateDiagnostic({ ...diagnostic, display: 'x'.repeat(501) }), false)

  const validateBaseline = validator(schema, 'DiagnosticBaseline')
  const baseline = {
    workspaceRevision: `git-tree:${'a'.repeat(40)}`,
    parserVersions: ['typescript_v1'],
    diagnostics: [diagnostic],
    diagnosticSetDigest: digest,
  }
  assert.equal(validateBaseline(baseline), true, JSON.stringify(validateBaseline.errors))
  assert.equal(validateBaseline({ ...baseline, parserVersions: [] }), false)
  assert.equal(validateBaseline({ ...baseline, parserVersions: ['typescript_v1', 'typescript_v1'] }), false)
  assert.equal(validateBaseline({ ...baseline, diagnostics: Array(4097).fill(diagnostic) }), false)

  const validateComparison = validator(schema, 'DiagnosticBaselineComparison')
  const comparisonEntry = { status: 'new', diagnostic }
  const comparison = {
    baseRevision: baseline.workspaceRevision,
    resultRevision: `git-tree:${'b'.repeat(40)}`,
    baselineDigest: digest,
    resultDigest: digest,
    entries: [comparisonEntry],
    newCount: 1,
    resolvedCount: 0,
    unchangedCount: 0,
  }
  assert.equal(validateComparison(comparison), true, JSON.stringify(validateComparison.errors))
  assert.equal(validateComparison({ ...comparison, unknown: true }), false)
  assert.equal(validateComparison({
    ...comparison,
    entries: [{ ...comparisonEntry, status: 'changed' }],
  }), false)
  assert.equal(validateComparison({ ...comparison, entries: [comparisonEntry, comparisonEntry] }), false)
  assert.equal(validateComparison({ ...comparison, newCount: 4097 }), false)

  const validateSelection = validator(schema, 'ValidationProfileSelection')
  const selection = {
    profile: 'fast',
    source: 'explicit_configuration',
    executable: true,
    configurationDigest: `sha256:${'1'.repeat(64)}`,
    changedPathsDigest: `sha256:${'2'.repeat(64)}`,
    commandIds: ['typescript-check'],
    reasonCode: 'explicit_profile',
  }
  assert.equal(validateSelection(selection), true, JSON.stringify(validateSelection.errors))
  assert.equal(validateSelection({ ...selection, unknownField: true }), false)
  assert.equal(validateSelection({ ...selection, profile: 'custom' }), false)
  assert.equal(validateSelection({ ...selection, source: 'automatic_suggestion' }), false)
  assert.equal(validateSelection({
    profile: 'affected',
    source: 'automatic_suggestion',
    executable: false,
    changedPathsDigest: selection.changedPathsDigest,
    commandIds: [],
    reasonCode: 'lockfile_changed',
  }), true)
  assert.equal(validateSelection({
    profile: 'changed',
    source: 'automatic_suggestion',
    executable: false,
    changedPathsDigest: selection.changedPathsDigest,
    commandIds: [],
    reasonCode: 'lockfile_changed',
  }), false)
  assert.equal(validateSelection({ ...selection, reasonCode: 'mixed_languages' }), false)
  assert.equal(validateProposal({
    ...proposal,
    acceptanceCriteriaIds: Array.from({ length: 257 }, (_, index) => `criterion-${index}`),
  }), false)

  const validatePolicy = validator(schema, 'RoleSessionPolicy')
  const policy = {
    schemaVersion: 2,
    roleId: 'executor',
    workspaceMode: 'candidate-read-only',
    developerInstructions: 'Compose one bounded ChangeBatch proposal.',
    executionMode: 'delegated_batch',
  }
  assert.equal(validatePolicy(policy), true, JSON.stringify(validatePolicy.errors))
  assert.equal(validatePolicy({ ...policy, schemaVersion: 1 }), false)
  assert.equal(validatePolicy({ ...policy, executionMode: 'delegated_patch' }), false)

  const validateReceipt = validator(schema, 'ChangeBatchReceipt')
  const identity = {
    batchId: `sha256:${'0'.repeat(64)}`,
    runKey: 'run-key-1',
    jobId: 'job_00000000000000000000000000',
    attempt: 1,
    leaseId: 'lse_00000000000000000000000000',
    fencingToken: '1',
    sessionIdentity: {
      productSessionId: 'psn_00000000000000000000000000',
      workerSessionId: 'wsn_00000000000000000000000000',
      codexThreadId: 'cdx_00000000000000000000000000',
    },
    repositoryId: 'rep_00000000000000000000000000',
    workspaceRevision: `git-tree:${'0'.repeat(40)}`,
    turnId: 'turn-1',
    patchDigest: `sha256:${'1'.repeat(64)}`,
  }
  const observationIntent = {
    observationId: `sha256:${'2'.repeat(64)}`,
    identity,
    resultRevision: `git-tree:${'f'.repeat(40)}`,
    validationProfile: 'fast',
    profileDigest: digest,
    inputDigest: digest,
    deltaDigest: digest,
    deltaExact: true,
    hardCheckFailed: false,
    allChecksExecuted: true,
    secretScan: {
      status: 'clean',
      scannerVersion: 'secret-rules-v1',
      inputDigest: digest,
      outputDigest: digest,
      findingCount: 0,
    },
    promptInjectionScan: {
      status: 'clean',
      scannerVersion: 'prompt-rules-v1',
      rulesDigest: digest,
      inputDigest: digest,
      findingCount: 0,
    },
    dataEgress: {
      networkAllowed: false,
      externalArtifactReadsAllowed: false,
      providerFileUploadsAllowed: false,
    },
    untrustedInput: {
      trustLevel: 'untrusted',
      goalSummary: 'Check the bounded result.',
      acceptanceCriteria: [{ id: 'criterion-1', summary: 'The result is correct.' }],
      batchSummary: 'One exact update.',
      delta: {
        deltaDigest: digest,
        deltaExact: true,
        fileCount: 1,
        hunkCount: 1,
        summary: 'One exact file delta.',
      },
      newDiagnostics: [],
      failedTests: [],
      snippets: [],
      contentDigest: digest,
    },
  }
  const observationRequest = {
    schemaVersion: 1,
    intent: observationIntent,
    oneShot: true,
  }
  const validateObservationRequest = validator(schema, 'ObservationRequest')
  assert.equal(
    validateObservationRequest(observationRequest),
    true,
    JSON.stringify(validateObservationRequest.errors),
  )
  assert.equal(validateObservationRequest({ ...observationRequest, unknown: true }), false)
  assert.equal(validateObservationRequest({ ...observationRequest, schemaVersion: 2 }), false)
  assert.equal(validateObservationRequest({ ...observationRequest, oneShot: false }), false)
  assert.equal(validateObservationRequest({
    ...observationRequest,
    intent: { ...observationIntent, hardCheckFailed: true },
  }), false)
  assert.equal(validateObservationRequest({
    ...observationRequest,
    intent: {
      ...observationIntent,
      promptInjectionScan: {
        ...observationIntent.promptInjectionScan,
        status: 'unknown',
      },
    },
  }), false)
  assert.equal(validateObservationRequest({
    ...observationRequest,
    intent: {
      ...observationIntent,
      untrustedInput: {
        ...observationIntent.untrustedInput,
        snippets: Array(9).fill({
          path: 'src/lib.rs',
          startLine: 1,
          endLine: 1,
          content: 'bounded',
          contentDigest: digest,
        }),
      },
    },
  }), false)
  for (const path of ['../src/lib.rs', '.gIt/config', 'CON', 'src/trailing.', 'src/wild*card']) {
    assert.equal(validateObservationRequest({
      ...observationRequest,
      intent: {
        ...observationIntent,
        untrustedInput: {
          ...observationIntent.untrustedInput,
          snippets: [{
            path,
            startLine: 1,
            endLine: 1,
            content: 'bounded',
            contentDigest: digest,
          }],
        },
      },
    }), false, path)
  }

  const receipt = {
    identity,
    status: 'applied',
    baseRevision: `git-tree:${'0'.repeat(40)}`,
    resultRevision: `git-tree:${'f'.repeat(40)}`,
    deltaDigest: `sha256:${'4'.repeat(64)}`,
    deltaExact: true,
    files: [{
      path: 'new.txt',
      operation: 'create',
      afterSha256: `sha256:${'9'.repeat(64)}`,
      bytesBefore: 0,
      bytesAfter: 4,
      modeAfter: '0644',
    }],
    normalizer: null,
    validation: null,
    observation: null,
    artifactRef: null,
  }
  assert.equal(validateReceipt(receipt), true, JSON.stringify(validateReceipt.errors))
  assert.equal(validateReceipt({ ...receipt, status: 'partially_applied' }), true)
  assert.equal(validateReceipt({ ...receipt, files: [] }), false)
  assert.equal(validateReceipt({ ...receipt, status: 'rejected', files: [] }), true)
  assert.equal(validateReceipt({ ...receipt, status: 'rejected' }), false)
  assert.equal(validateReceipt({ ...receipt, resultRevision: undefined }), false)
  assert.equal(validateReceipt({ ...receipt, deltaExact: false }), false)
  const uncertain = {
    ...receipt,
    status: 'state_uncertain',
    deltaExact: false,
  }
  delete uncertain.resultRevision
  delete uncertain.deltaDigest
  assert.equal(validateReceipt(uncertain), true, JSON.stringify(validateReceipt.errors))
  assert.equal(validateReceipt({ ...uncertain, resultRevision: null }), false)
  assert.equal(validateReceipt({ ...uncertain, deltaDigest: null }), false)

  const budget = {
    maxRepairRounds: 3,
    maxObserverCalls: 4,
    maxPrimaryModelCalls: 8,
    maxTotalTokens: 1_000_000,
    maxTotalCostMicrounits: 10_000_000,
    maxWallTimeMillis: 3_600_000,
    maxChangeBatches: 4,
    maxContextPackBytes: 131_072,
  }
  const validateBudget = validator(schema, 'RepairLoopBudget')
  assert.equal(validateBudget(budget), true, JSON.stringify(validateBudget.errors))
  assert.equal(validateBudget({ ...budget, maxRepairRounds: 2 }), false)
  assert.equal(validateBudget({ ...budget, maxRepairRounds: 4 }), false)
  assert.equal(validateBudget({ ...budget, maxObserverCalls: 5 }), false)
  assert.equal(validateBudget({ ...budget, maxPrimaryModelCalls: 9 }), false)
  assert.equal(validateBudget({ ...budget, maxTotalTokens: 10_000_001 }), false)
  assert.equal(validateBudget({ ...budget, maxTotalCostMicrounits: 9_007_199_254_740_992 }), false)
  assert.equal(validateBudget({ ...budget, maxWallTimeMillis: 3_600_001 }), false)
  assert.equal(validateBudget({ ...budget, extra: 1 }), false)

  const counters = {
    repairRounds: 3,
    observerCalls: 0,
    primaryModelCalls: 4,
    totalTokens: 500_000,
    totalCostMicrounits: 5_000_000,
    elapsedMillis: 1_000,
    changeBatches: 4,
    contextPackBytes: 2_048,
  }
  const validateCounters = validator(schema, 'RepairLoopCounters')
  assert.equal(validateCounters(counters), true, JSON.stringify(validateCounters.errors))
  for (const [field, value] of [
    ['repairRounds', 4],
    ['observerCalls', 5],
    ['primaryModelCalls', 9],
    ['totalTokens', 10_000_001],
    ['elapsedMillis', 3_600_001],
    ['changeBatches', 5],
    ['contextPackBytes', 131_073],
  ]) {
    assert.equal(validateCounters({ ...counters, [field]: value }), false, field)
  }

  const repairEnvelope = {
    identity,
    repairRound: 3,
    observedRevision: receipt.resultRevision,
    deltaDigest: receipt.deltaDigest,
    reasonCode: 'targeted_repair_required',
    rootCauseSummary: 'One bounded correction remains.',
    diagnosticDigests: [],
    snippetArtifactRefs: [],
  }
  const validateRepairEnvelope = validator(schema, 'RepairEnvelope')
  assert.equal(
    validateRepairEnvelope(repairEnvelope),
    true,
    JSON.stringify(validateRepairEnvelope.errors),
  )
  assert.equal(validateRepairEnvelope({ ...repairEnvelope, repairRound: 4 }), false)

  const contextPack = {
    schemaVersion: 1,
    identity,
    observedRevision: receipt.resultRevision,
    proposalDisposition: 'continue',
    contextDigest: digest,
    serializedByteCount: 2_048,
    goalSummary: 'Complete the exact bounded change.',
    completedAcceptanceCriteria: [{
      id: 'criterion-complete',
      summary: 'The completed criterion remains satisfied.',
    }],
    incompleteAcceptanceCriteria: [{
      id: 'criterion-next',
      summary: 'The remaining criterion needs one repair.',
    }],
    repairEnvelope,
    latestReceipt: receipt,
    latestObservation: null,
    artifactRefs: [],
  }
  const validateContextPack = validator(schema, 'RepairLoopContextPack')
  assert.equal(
    validateContextPack(contextPack),
    true,
    JSON.stringify(validateContextPack.errors),
  )
  assert.equal(validateContextPack({ ...contextPack, latestObservation: undefined }), false)
  assert.equal(validateContextPack({ ...contextPack, serializedByteCount: 131_073 }), false)
  for (const forbidden of ['history', 'source', 'patch', 'rawLog']) {
    assert.equal(validateContextPack({ ...contextPack, [forbidden]: 'forbidden' }), false)
    assert.equal(Object.hasOwn(schema.$defs.RepairLoopContextPack.properties, forbidden), false)
  }

  const freezeFact = {
    schemaVersion: 1,
    identity,
    resultRevision: receipt.resultRevision,
    deltaDigest: receipt.deltaDigest,
    finalReceipt: receipt,
    finalObservation: null,
    counters,
    stopReason: 'accepted',
    contextPackDigest: digest,
    candidateArtifactRef: {
      artifactId: 'art_00000000000000000000000000',
      digest,
    },
    frozenAt: '2026-09-02T00:00:00.000Z',
  }
  const validateFreezeFact = validator(schema, 'FinalCandidateFreezeFact')
  assert.equal(
    validateFreezeFact(freezeFact),
    true,
    JSON.stringify(validateFreezeFact.errors),
  )
  assert.equal(validateFreezeFact({ ...freezeFact, finalObservation: undefined }), false)
  assert.equal(validateFreezeFact({ ...freezeFact, stopReason: 'wall_time_limit_reached' }), false)
  assert.match(rust, /pub struct RepairLoopBudget \{[\s\S]*max_repair_rounds: i64,/u)
})
