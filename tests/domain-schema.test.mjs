import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import test from 'node:test'

import Ajv2020 from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'

const root = resolve(import.meta.dirname, '..')
const schemaPath = join(root, 'schema/winwincode/v1/domain.schema.json')
const samplesPath = join(root, 'schema/winwincode/v1/domain.samples.json')

const ID_DEFINITIONS = Object.freeze({
  ApprovalId: 'apr_',
  AttentionItemId: 'att_',
  ChatMessageId: 'msg_',
  CodexThreadId: 'cdx_',
  CredentialReferenceId: 'crd_',
  DeliveryId: 'dlv_',
  DeliveryTaskId: 'dtk_',
  EvidenceId: 'evd_',
  ExecutionJobId: 'job_',
  InputRequestId: 'inp_',
  LeaseId: 'lse_',
  OrganizationId: 'org_',
  ProductSessionId: 'psn_',
  ProjectId: 'prj_',
  PublicationId: 'pub_',
  RepositoryId: 'rep_',
  RequestId: 'req_',
  ServiceAccountId: 'svc_',
  StageRunId: 'run_',
  SystemActorId: 'sys_',
  UserId: 'usr_',
  WorkerId: 'wrk_',
  WorkerSessionId: 'wsn_',
  WorkspaceId: 'wsp_',
})
const COMMAND_NAMES = Object.freeze([
  'session.create',
  'chat.submit',
  'input.respond',
  'session.cancel',
  'session.close',
  'delivery.create',
  'delivery.update_spec',
  'delivery.approve_task_breakdown',
  'delivery.advance',
  'delivery.resolve_attention',
  'delivery.submit_verdict',
  'settings.update',
  'credential.reference.create',
  'credential.reference.delete',
  'approval.decide',
  'worker.drain',
  'worker.enable',
  'publication.publish',
  'publication.cancel',
])
const DELIVERY_STATUSES = Object.freeze([
  'draft',
  'clarifying',
  'ready',
  'planning',
  'plan-review',
  'executing',
  'verifying',
  'reworking',
  'needs-attention',
  'ready-to-deliver',
  'delivered',
])
const DELIVERY_TASK_STATUSES = Object.freeze([
  'pending',
  'active',
  'blocked',
  'verifying',
  'completed',
  'failed',
])
const ERROR_CODES = Object.freeze([
  'INVALID_REQUEST',
  'AUTHENTICATION_REQUIRED',
  'PERMISSION_DENIED',
  'RESOURCE_NOT_FOUND',
  'IDEMPOTENCY_CONFLICT',
  'REVISION_CONFLICT',
  'WRONG_STATE',
  'RATE_LIMITED',
  'SERVICE_UNAVAILABLE',
  'INTERNAL_ERROR',
])

async function loadSchema() {
  return JSON.parse(await readFile(schemaPath, 'utf8'))
}

function ajvDefinitionValidator(schema, definitionName) {
  const ajv = new Ajv2020({ allErrors: true, strict: true })
  addFormats(ajv)
  return ajv.compile({
    ...schema,
    $id: schema.$id.replace(
      'domain.schema.json',
      `domain-${definitionName.toLowerCase()}.test.schema.json`,
    ),
    $ref: `#/$defs/${definitionName}`,
  })
}

function matchesType(value, expectedType) {
  if (expectedType === 'null') return value === null
  if (expectedType === 'array') return Array.isArray(value)
  if (expectedType === 'integer') return Number.isSafeInteger(value)
  if (expectedType === 'object') {
    return typeof value === 'object' && value !== null && !Array.isArray(value)
  }
  return typeof value === expectedType
}

function validationErrors(rootSchema, candidate, schema, path = '$') {
  if (typeof schema === 'boolean') return schema ? [] : [`${path}: rejected`]
  if (schema.$ref !== undefined) {
    const prefix = '#/$defs/'
    assert.ok(schema.$ref.startsWith(prefix), `unsupported test $ref: ${schema.$ref}`)
    return validationErrors(rootSchema, candidate, rootSchema.$defs[schema.$ref.slice(prefix.length)], path)
  }

  const errors = []
  for (const branch of schema.allOf ?? []) {
    errors.push(...validationErrors(rootSchema, candidate, branch, path))
  }
  if (schema.anyOf !== undefined) {
    const validBranches = schema.anyOf.filter(branch => (
      validationErrors(rootSchema, candidate, branch, path).length === 0
    ))
    if (validBranches.length === 0) errors.push(`${path}: did not match any allowed shape`)
  }
  if (schema.oneOf !== undefined) {
    const validBranches = schema.oneOf.filter(branch => (
      validationErrors(rootSchema, candidate, branch, path).length === 0
    ))
    if (validBranches.length !== 1) errors.push(`${path}: matched ${validBranches.length} shapes`)
  }
  if (schema.const !== undefined && !Object.is(candidate, schema.const)) {
    errors.push(`${path}: must equal ${JSON.stringify(schema.const)}`)
  }
  if (schema.enum !== undefined && !schema.enum.includes(candidate)) {
    errors.push(`${path}: is outside the enum`)
  }

  if (schema.type !== undefined && !matchesType(candidate, schema.type)) {
    errors.push(`${path}: must be ${schema.type}`)
    return errors
  }

  if (typeof candidate === 'string') {
    if (schema.minLength !== undefined && candidate.length < schema.minLength) {
      errors.push(`${path}: is too short`)
    }
    if (schema.pattern !== undefined && !new RegExp(schema.pattern, 'u').test(candidate)) {
      errors.push(`${path}: does not match ${schema.pattern}`)
    }
    if (schema.format === 'date-time') {
      const milliseconds = Date.parse(candidate)
      if (!Number.isFinite(milliseconds)) errors.push(`${path}: is not a date-time`)
    }
  }

  if (typeof candidate === 'number') {
    if (schema.minimum !== undefined && candidate < schema.minimum) {
      errors.push(`${path}: is below the minimum`)
    }
    if (schema.maximum !== undefined && candidate > schema.maximum) {
      errors.push(`${path}: is above the maximum`)
    }
  }

  if (Array.isArray(candidate)) {
    if (schema.minItems !== undefined && candidate.length < schema.minItems) {
      errors.push(`${path}: has too few items`)
    }
    if (schema.uniqueItems === true && new Set(candidate.map(value => JSON.stringify(value))).size !== candidate.length) {
      errors.push(`${path}: has duplicate items`)
    }
    if (schema.items !== undefined) {
      candidate.forEach((value, index) => {
        errors.push(...validationErrors(rootSchema, value, schema.items, `${path}[${index}]`))
      })
    }
  }

  if (matchesType(candidate, 'object')) {
    for (const required of schema.required ?? []) {
      if (!Object.hasOwn(candidate, required)) errors.push(`${path}.${required}: is required`)
    }
    for (const [name, value] of Object.entries(candidate)) {
      if (schema.properties?.[name] !== undefined) {
        errors.push(...validationErrors(rootSchema, value, schema.properties[name], `${path}.${name}`))
      } else if (schema.additionalProperties === false) {
        errors.push(`${path}.${name}: is not allowed`)
      } else if (typeof schema.additionalProperties === 'object') {
        errors.push(...validationErrors(rootSchema, value, schema.additionalProperties, `${path}.${name}`))
      }
    }
  }

  return errors
}

function assertValid(schema, definitionName, candidate) {
  assert.deepEqual(
    validationErrors(schema, candidate, { $ref: `#/$defs/${definitionName}` }),
    [],
  )
}

function assertInvalid(schema, definitionName, candidate) {
  assert.notDeepEqual(
    validationErrors(schema, candidate, { $ref: `#/$defs/${definitionName}` }),
    [],
  )
}

test('canonical domain IDs use non-interchangeable prefixes', async () => {
  const schema = await loadSchema()

  assert.equal(schema.$schema, 'https://json-schema.org/draft/2020-12/schema')
  assert.equal(
    schema.$id,
    'https://schemas.winwincode.dev/winwincode/v1/domain.schema.json',
  )

  const suffix = '01J00000000000000000000000'
  for (const [definitionName, prefix] of Object.entries(ID_DEFINITIONS)) {
    const definition = schema.$defs[definitionName]
    assert.deepEqual(
      { type: definition.type, pattern: definition.pattern },
      {
        type: 'string',
        pattern: `^${prefix}[0-9A-HJKMNP-TV-Z]{26}$`,
      },
    )

    const ownValue = `${prefix}${suffix}`
    assert.match(ownValue, new RegExp(definition.pattern, 'u'))
    for (const otherPrefix of Object.values(ID_DEFINITIONS)) {
      if (otherPrefix === prefix) continue
      assert.doesNotMatch(`${otherPrefix}${suffix}`, new RegExp(definition.pattern, 'u'))
    }
  }
})

test('shared scalar values reject null, invalid versions, unsafe revisions, and malformed hashes', async () => {
  const schema = await loadSchema()

  assertValid(schema, 'SchemaVersion', 'winwincode/v1')
  assertInvalid(schema, 'SchemaVersion', 'winwincode/v2')
  assertInvalid(schema, 'SchemaVersion', null)

  assertValid(schema, 'Revision', 0)
  assertValid(schema, 'Revision', 9007199254740991)
  assertInvalid(schema, 'Revision', -1)
  assertInvalid(schema, 'Revision', 1.5)
  assertInvalid(schema, 'Revision', null)

  assertValid(schema, 'Instant', '2026-08-24T09:10:11.123Z')
  assertInvalid(schema, 'Instant', '2026-08-24T09:10:11+08:00')
  assertInvalid(schema, 'Instant', null)

  assertValid(schema, 'Sha256Digest', `sha256:${'a'.repeat(64)}`)
  assertInvalid(schema, 'Sha256Digest', `sha256:${'A'.repeat(64)}`)
  assertInvalid(schema, 'Sha256Digest', 'a'.repeat(64))
})

test('Actor and Scope keep identity kind and ownership ancestry explicit', async () => {
  const schema = await loadSchema()
  const suffix = '01J00000000000000000000000'

  assertValid(schema, 'Actor', { kind: 'user', id: `usr_${suffix}` })
  assertValid(schema, 'Actor', { kind: 'service_account', id: `svc_${suffix}` })
  assertValid(schema, 'Actor', { kind: 'system', id: `sys_${suffix}` })
  assertInvalid(schema, 'Actor', { kind: 'user', id: `svc_${suffix}` })
  assertInvalid(schema, 'Actor', { kind: 'user', id: null })

  assertValid(schema, 'OrganizationScope', {
    kind: 'organization',
    organizationId: `org_${suffix}`,
  })
  assertValid(schema, 'WorkspaceScope', {
    kind: 'workspace',
    organizationId: `org_${suffix}`,
    workspaceId: `wsp_${suffix}`,
  })
  assertValid(schema, 'ProjectScope', {
    kind: 'project',
    organizationId: `org_${suffix}`,
    workspaceId: `wsp_${suffix}`,
    projectId: `prj_${suffix}`,
  })
  assertValid(schema, 'RepositoryScope', {
    kind: 'repository',
    organizationId: `org_${suffix}`,
    workspaceId: `wsp_${suffix}`,
    projectId: `prj_${suffix}`,
    repositoryId: `rep_${suffix}`,
  })
  assertInvalid(schema, 'Scope', {
    kind: 'project',
    organizationId: `org_${suffix}`,
    projectId: `prj_${suffix}`,
  })
  assertInvalid(schema, 'Scope', {
    kind: 'repository',
    organizationId: `org_${suffix}`,
    workspaceId: `wsp_${suffix}`,
    projectId: `prj_${suffix}`,
    repositoryId: null,
  })
  assertValid(schema, 'LocalDefaultScope', {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000000',
    workspaceId: 'wsp_00000000000000000000000000',
    projectId: 'prj_00000000000000000000000000',
    repositoryId: 'rep_00000000000000000000000000',
  })
  assertInvalid(schema, 'LocalDefaultScope', {
    kind: 'repository',
    organizationId: `org_${suffix}`,
    workspaceId: 'wsp_00000000000000000000000000',
    projectId: 'prj_00000000000000000000000000',
    repositoryId: 'rep_00000000000000000000000000',
  })
})

test('CommandEnvelope requires actor, scope, request identity, and optimistic revision', async () => {
  const schema = await loadSchema()
  const command = {
    schemaVersion: 'winwincode/v1',
    command: 'session.create',
    actor: {
      kind: 'system',
      id: 'sys_00000000000000000000000000',
    },
    scope: {
      kind: 'repository',
      organizationId: 'org_00000000000000000000000000',
      workspaceId: 'wsp_00000000000000000000000000',
      projectId: 'prj_00000000000000000000000000',
      repositoryId: 'rep_00000000000000000000000000',
    },
    requestId: 'req_01J00000000000000000000000',
    expectedRevision: 0,
    payload: {},
  }

  assert.deepEqual(schema.$defs.CommandName.enum, COMMAND_NAMES)
  assertValid(schema, 'CommandEnvelope', command)
  const { actor: _actor, ...withoutActor } = command
  assertInvalid(schema, 'CommandEnvelope', withoutActor)
  assertInvalid(schema, 'CommandEnvelope', { ...command, expectedRevision: null })
  assertInvalid(schema, 'CommandEnvelope', { ...command, command: 'delivery.delete' })
  assertInvalid(schema, 'CommandEnvelope', { ...command, schemaVersion: 'winwincode/v2' })
  assertInvalid(schema, 'CommandEnvelope', { ...command, transportHint: 'http' })
})

test('DeliveryProjection exposes a minimal page view with complete repository ownership', async () => {
  const schema = await loadSchema()
  const projection = {
    schemaVersion: 'winwincode/v1',
    deliveryId: 'dlv_01J00000000000000000000000',
    revision: 18,
    ownership: {
      organizationId: 'org_01J00000000000000000000000',
      workspaceId: 'wsp_01J00000000000000000000000',
      projectId: 'prj_01J00000000000000000000000',
      repositoryId: 'rep_01J00000000000000000000000',
    },
    title: 'Freeze the delivery contract',
    status: 'executing',
    taskCounts: {
      total: 4,
      pending: 1,
      active: 1,
      blocked: 0,
      verifying: 1,
      completed: 1,
      failed: 0,
    },
    activeStageRunId: 'run_01J00000000000000000000000',
    openAttentionCount: 0,
    updatedAt: '2026-08-24T09:10:11.123Z',
  }

  assert.deepEqual(schema.$defs.DeliveryStatus.enum, DELIVERY_STATUSES)
  assert.deepEqual(schema.$defs.DeliveryTaskStatus.enum, DELIVERY_TASK_STATUSES)
  assertValid(schema, 'DeliveryProjection', projection)
  assertValid(schema, 'DeliveryProjection', { ...projection, activeStageRunId: null })

  const { organizationId: _organizationId, ...incompleteOwnership } = projection.ownership
  assertInvalid(schema, 'DeliveryProjection', {
    ...projection,
    ownership: incompleteOwnership,
  })
  assertInvalid(schema, 'DeliveryProjection', {
    ...projection,
    ownership: {
      ...projection.ownership,
      repositoryId: 'prj_01J00000000000000000000000',
    },
  })
  assertInvalid(schema, 'DeliveryProjection', { ...projection, title: null })
  assertInvalid(schema, 'DeliveryProjection', { ...projection, status: 'archived' })
  assertInvalid(schema, 'DeliveryProjection', { ...projection, schemaVersion: 'winwincode/v2' })
  assertInvalid(schema, 'DeliveryProjection', {
    ...projection,
    domainState: {
      spec: {},
      evidence: [],
      sessionBindings: [],
    },
  })
})

test('runtime projection scope is either Chat or one complete Delivery stage', async () => {
  const schema = await loadSchema()
  const validateItem = ajvDefinitionValidator(schema, 'RuntimeProjectionItem')
  const validateSnapshot = ajvDefinitionValidator(schema, 'RuntimeProjectionSnapshot')
  const item = {
    productSessionId: 'psn_01J00000000000000000000000',
    workerSessionId: 'wsn_01J00000000000000000000000',
    codexThreadId: 'cdx_01J00000000000000000000000',
    leaseId: 'lse_01J00000000000000000000000',
    projectionSequence: 1,
    projectionKind: 'activity',
    summary: 'Started.',
    occurredAt: '2026-08-24T09:10:11.123Z',
  }
  const snapshot = {
    kind: 'runtime_projection',
    revision: 1,
    productSessionId: item.productSessionId,
    deliveryId: null,
    stageRunId: null,
    lastProjectionSequence: 1,
    items: [item],
    rebuiltAt: '2026-08-24T09:10:12.123Z',
  }

  assert.equal(validateItem(item), true, JSON.stringify(validateItem.errors))
  assert.equal(validateSnapshot(snapshot), true, JSON.stringify(validateSnapshot.errors))
  assert.equal(validateItem({
    ...item,
    deliveryId: 'dlv_01J00000000000000000000000',
  }), false)
  assert.equal(validateSnapshot({
    ...snapshot,
    deliveryId: 'dlv_01J00000000000000000000000',
  }), false)
  assert.equal(validateSnapshot({
    ...snapshot,
    stageRunId: 'run_01J00000000000000000000000',
  }), false)
})

test('ErrorEnvelope keeps machine codes and retry behavior stable', async () => {
  const schema = await loadSchema()
  const terminalError = {
    schemaVersion: 'winwincode/v1',
    requestId: 'req_01J00000000000000000000000',
    error: {
      code: 'REVISION_CONFLICT',
      message: 'The delivery changed before this command was applied.',
      retryable: false,
      details: {
        expectedRevision: 18,
        actualRevision: 19,
      },
    },
  }
  const retryableError = {
    ...terminalError,
    error: {
      code: 'SERVICE_UNAVAILABLE',
      message: 'No healthy worker is currently available.',
      retryable: true,
      details: {},
    },
  }

  assert.deepEqual(schema.$defs.ErrorCode.enum, ERROR_CODES)
  assertValid(schema, 'ErrorEnvelope', terminalError)
  assertValid(schema, 'ErrorEnvelope', retryableError)
  assertInvalid(schema, 'ErrorEnvelope', {
    ...terminalError,
    error: { ...terminalError.error, retryable: true },
  })
  assertInvalid(schema, 'ErrorEnvelope', {
    ...retryableError,
    error: { ...retryableError.error, retryable: false },
  })
  assertInvalid(schema, 'ErrorEnvelope', {
    ...terminalError,
    error: { ...terminalError.error, code: 'UNKNOWN' },
  })
  assertInvalid(schema, 'ErrorEnvelope', {
    ...terminalError,
    error: { ...terminalError.error, details: null },
  })
  const { requestId: _requestId, ...withoutRequestId } = terminalError
  assertInvalid(schema, 'ErrorEnvelope', withoutRequestId)
  assertInvalid(schema, 'ErrorEnvelope', { ...terminalError, schemaVersion: 'winwincode/v2' })
})

test('fixed positive and negative samples pin missing, null, enum, version, and error behavior', async () => {
  const schema = await loadSchema()
  const samples = JSON.parse(await readFile(samplesPath, 'utf8'))

  assert.equal(samples.schemaId, schema.$id)
  assert.deepEqual(
    [...new Set(samples.cases.map(sample => sample.category))].sort(),
    ['enum', 'error_semantics', 'missing', 'null', 'valid', 'version'],
  )
  assert.equal(
    new Set(samples.cases.map(sample => sample.name)).size,
    samples.cases.length,
  )

  for (const sample of samples.cases) {
    assert.ok(schema.$defs[sample.definition], `${sample.name}: unknown definition`)
    if (sample.valid) assertValid(schema, sample.definition, sample.value)
    else assertInvalid(schema, sample.definition, sample.value)
  }
})
