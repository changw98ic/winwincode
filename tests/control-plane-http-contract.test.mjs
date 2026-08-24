import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const schemaRoot = join(root, 'schema', 'winwincode', 'v1')

const COMMANDS = Object.freeze([
  'session.create',
  'chat.submit',
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

const QUERIES = Object.freeze([
  'session.list',
  'session.get',
  'delivery.list',
  'delivery.get',
  'settings.get',
  'credential.reference.list',
  'credential.reference.get',
  'approval.list',
  'approval.get',
  'worker.list',
  'worker.get',
  'publication.list',
  'publication.get',
])

const ERROR_STATUS = Object.freeze({
  INVALID_REQUEST: 400,
  AUTHENTICATION_REQUIRED: 401,
  PERMISSION_DENIED: 403,
  RESOURCE_NOT_FOUND: 404,
  IDEMPOTENCY_CONFLICT: 409,
  REVISION_CONFLICT: 409,
  WRONG_STATE: 409,
  RATE_LIMITED: 429,
  INTERNAL_ERROR: 500,
  SERVICE_UNAVAILABLE: 503,
})

async function json(...parts) {
  return JSON.parse(await readFile(join(schemaRoot, ...parts), 'utf8'))
}

function collectRefs(value, refs = []) {
  if (Array.isArray(value)) {
    for (const entry of value) collectRefs(entry, refs)
    return refs
  }
  if (typeof value !== 'object' || value === null) return refs
  for (const [key, entry] of Object.entries(value)) {
    if (key === '$ref') refs.push(entry)
    else collectRefs(entry, refs)
  }
  return refs
}

function localDefinition(schema, ref) {
  const prefix = '#/$defs/'
  assert.match(ref, /^#\/\$defs\/[A-Za-z][A-Za-z0-9]*$/u)
  return schema.$defs[ref.slice(prefix.length)]
}

function specializationNames(schema, unionName, discriminator) {
  return schema.$defs[unionName].oneOf.map(({ $ref }) => {
    const definition = localDefinition(schema, $ref)
    const specialization = definition.allOf.find(part => part.type === 'object')
    assert.ok(specialization, `${$ref} must specialize a shared envelope`)
    return specialization.properties[discriminator].const
  })
}

test('HTTP contract specializes every accepted command without copying domain primitives', async () => {
  const schema = await json('control-plane-http.schema.json')

  assert.equal(schema.$schema, 'https://json-schema.org/draft/2020-12/schema')
  assert.equal(
    schema.$id,
    'https://schemas.winwincode.dev/winwincode/v1/control-plane-http.schema.json',
  )
  assert.deepEqual(specializationNames(schema, 'CommandRequest', 'command'), COMMANDS)

  const forbiddenCopies = [
    'RequestId',
    'Revision',
    'Actor',
    'Scope',
    'OrganizationId',
    'WorkspaceId',
    'ProjectId',
    'RepositoryId',
    'DeliveryId',
    'ProductSessionId',
    'WorkerId',
  ]
  for (const name of forbiddenCopies) assert.equal(schema.$defs[name], undefined)

  const refs = collectRefs(schema)
  assert.ok(refs.includes('./domain.schema.json#/$defs/CommandEnvelope'))
  for (const name of forbiddenCopies) {
    assert.ok(
      refs.includes(`./domain.schema.json#/$defs/${name}`),
      `HTTP contract must reuse domain ${name}`,
    )
  }
  assert.equal(
    refs.some(ref => typeof ref !== 'string'
      || (!ref.startsWith('#/$defs/') && !ref.startsWith('./domain.schema.json#/$defs/'))),
    false,
  )
})

test('HTTP query contract covers every current read surface with an opaque stable cursor', async () => {
  const schema = await json('control-plane-http.schema.json')
  assert.deepEqual(specializationNames(schema, 'QueryRequest', 'query'), QUERIES)

  const queryEnvelope = schema.$defs.QueryEnvelope
  assert.deepEqual(queryEnvelope.required, [
    'schemaVersion',
    'requestId',
    'query',
    'actor',
    'scope',
    'parameters',
    'page',
  ])
  assert.equal(queryEnvelope.properties.actor.$ref, './domain.schema.json#/$defs/Actor')
  assert.equal(queryEnvelope.properties.scope.$ref, './domain.schema.json#/$defs/Scope')
  assert.equal(queryEnvelope.properties.page.$ref, '#/$defs/PageRequest')
  assert.equal(schema.$defs.OpaqueCursor.type, 'string')
  assert.equal(schema.$defs.PageRequest.properties.limit.maximum, 200)
  assert.equal(schema.$defs.PageInfo.properties.nextCursor.oneOf[1].type, 'null')
  assert.equal(
    schema.$defs.QueryResultResponse.properties.result.$ref,
    '#/$defs/QueryResult',
  )
  assert.deepEqual(
    schema.$defs.QueryResult.oneOf.map(entry => entry.$ref),
    [
      '#/$defs/ProductSessionProjection',
      './domain.schema.json#/$defs/DeliveryProjection',
      '#/$defs/SettingsProjection',
      '#/$defs/CredentialReferenceProjection',
      '#/$defs/ApprovalProjection',
      '#/$defs/WorkerProjection',
      '#/$defs/PublicationProjection',
      '#/$defs/ProductSessionPage',
      '#/$defs/DeliveryPage',
      '#/$defs/CredentialReferencePage',
      '#/$defs/ApprovalPage',
      '#/$defs/WorkerPage',
      '#/$defs/PublicationPage',
    ],
  )

  const pagination = schema['x-winwincode-semantics'].pagination
  assert.equal(pagination.order, 'snapshot_then_updated_at_then_id')
  assert.equal(pagination.cursor, 'opaque_scope_query_filter_bound')
  assert.equal(pagination.invalidCursorError, 'INVALID_REQUEST')
})

test('HTTP command retry, revision conflict, and errors have stable machine semantics', async () => {
  const schema = await json('control-plane-http.schema.json')
  const semantics = schema['x-winwincode-semantics']

  assert.deepEqual(semantics.writeTransport, {
    accepted: 'HTTP_POST_COMMAND',
    rejected: ['HTTP_WEBSOCKET_COMMAND', 'HTTP_ROUTE_ALIAS'],
  })
  assert.deepEqual(semantics.idempotency, {
    identity: 'actor+scope+requestId',
    sameRequest: 'return_original_status_and_body_without_reexecution',
    changedRequest: 'IDEMPOTENCY_CONFLICT',
  })
  assert.deepEqual(semantics.revision, {
    checkedBeforeMutation: true,
    conflict: 'REVISION_CONFLICT',
    reports: ['expectedRevision', 'currentRevision'],
  })
  assert.equal(
    schema.$defs.CommandCompletedResponse.properties.result.$ref,
    '#/$defs/CommandResult',
  )

  assert.deepEqual(
    Object.fromEntries(Object.entries(semantics.errors).map(([code, value]) => [
      code,
      value.httpStatus,
    ])),
    ERROR_STATUS,
  )
  assert.equal(semantics.errors.RATE_LIMITED.retryable, true)
  assert.equal(semantics.errors.SERVICE_UNAVAILABLE.retryable, true)
  for (const code of Object.keys(ERROR_STATUS).filter(code => (
    code !== 'RATE_LIMITED' && code !== 'SERVICE_UNAVAILABLE'
  ))) assert.equal(semantics.errors[code].retryable, false)
})

test('OpenAPI 3.1 exposes one command route and one query route with no legacy aliases', async () => {
  const openapi = await json('control-plane-http.openapi.json')

  assert.match(openapi.openapi, /^3\.1\./u)
  assert.deepEqual(Object.keys(openapi.paths), ['/api/v1/commands', '/api/v1/queries'])
  assert.equal(
    openapi.paths['/api/v1/commands'].post.requestBody.content['application/json'].schema.$ref,
    './control-plane-http.schema.json#/$defs/CommandRequest',
  )
  assert.equal(
    openapi.paths['/api/v1/queries'].post.requestBody.content['application/json'].schema.$ref,
    './control-plane-http.schema.json#/$defs/QueryRequest',
  )

  const commandResponses = openapi.paths['/api/v1/commands'].post.responses
  const queryResponses = openapi.paths['/api/v1/queries'].post.responses
  assert.deepEqual(Object.keys(commandResponses), [
    '200',
    '202',
    '400',
    '401',
    '403',
    '404',
    '409',
    '429',
    '500',
    '503',
  ])
  assert.deepEqual(Object.keys(queryResponses), [
    '200',
    '400',
    '401',
    '403',
    '404',
    '429',
    '500',
    '503',
  ])
})

test('positive and negative samples pin retries, conflicts, cursors, and secret-safe output', async () => {
  const examples = await json('examples', 'control-plane-http.examples.json')

  assert.equal(examples.schemaVersion, 1)
  assert.deepEqual(examples.idempotency.retry, examples.idempotency.original)
  assert.equal(
    examples.idempotency.conflict.requestId,
    examples.idempotency.original.requestId,
  )
  assert.notDeepEqual(examples.idempotency.conflict, examples.idempotency.original)
  assert.equal(examples.idempotency.expectedConflict.error.code, 'IDEMPOTENCY_CONFLICT')
  assert.equal(examples.revisionConflict.error.code, 'REVISION_CONFLICT')
  assert.deepEqual(examples.revisionConflict.error.details, {
    expectedRevision: 18,
    currentRevision: 19,
  })
  assert.equal(examples.invalidCursor.error.code, 'INVALID_REQUEST')
  assert.equal(examples.invalidCursor.error.details.field, 'page.cursor')

  const serialized = JSON.stringify(examples.responses)
  for (const forbidden of ['secretMaterial', 'accessToken', 'apiKey']) {
    assert.equal(serialized.includes(forbidden), false)
  }
  assert.equal(examples.responses.credentialReference.secretState, 'available')
})
