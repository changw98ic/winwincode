import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import test from 'node:test'

import Ajv2020 from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'

const root = resolve(import.meta.dirname, '..')
const schemaRoot = join(root, 'schema', 'winwincode', 'v1')
const schemaFiles = Object.freeze([
  'domain.schema.json',
  'control-plane-http.schema.json',
  'control-plane-events.schema.json',
  'execution-port.schema.json',
])
const schemaBase = 'https://schemas.winwincode.dev/winwincode/v1/'

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function schema(name) {
  return json(join(schemaRoot, name))
}

function schemaDocuments() {
  return new Map(schemaFiles.map(name => {
    const document = schema(name)
    return [document.$id, document]
  }))
}

function visitSchema(value, callback, path = '#') {
  if (Array.isArray(value)) {
    value.forEach((entry, index) => visitSchema(entry, callback, `${path}/${index}`))
    return
  }
  if (value === null || typeof value !== 'object') return
  callback(value, path)
  for (const [key, entry] of Object.entries(value)) {
    visitSchema(entry, callback, `${path}/${key}`)
  }
}

function resolveSchemaRef(documents, sourceDocument, ref) {
  const resolved = new URL(ref, sourceDocument.$id)
  const documentId = `${resolved.origin}${resolved.pathname}`
  const document = documents.get(documentId)
  assert.ok(document, `missing schema document for ${ref} from ${sourceDocument.$id}`)

  let node = document
  if (resolved.hash !== '') {
    assert.match(resolved.hash, /^#(?:\/|$)/u, `unsupported schema fragment: ${ref}`)
    for (const segment of resolved.hash.slice(2).split('/').filter(Boolean)) {
      const key = decodeURIComponent(segment).replaceAll('~1', '/').replaceAll('~0', '~')
      node = node?.[key]
      assert.notEqual(node, undefined, `missing schema target: ${ref}`)
    }
  }
  return { document, node }
}

function schemaPropertyConst(documents, document, node, property, seen = new Set()) {
  if (node.properties?.[property]?.const !== undefined) {
    return node.properties[property].const
  }
  if (typeof node.$ref === 'string') {
    const key = `${document.$id}:${node.$ref}:${property}`
    if (seen.has(key)) return undefined
    seen.add(key)
    const resolved = resolveSchemaRef(documents, document, node.$ref)
    return schemaPropertyConst(documents, resolved.document, resolved.node, property, seen)
  }
  for (const branch of node.allOf ?? []) {
    const value = schemaPropertyConst(documents, document, branch, property, seen)
    if (value !== undefined) return value
  }
  if (Array.isArray(node.oneOf)) {
    const values = node.oneOf.map(branch => (
      schemaPropertyConst(documents, document, branch, property, new Set(seen))
    ))
    if (values.length > 0 && values.every(value => value === values[0])) return values[0]
  }
  return undefined
}

function schemaRequiresProperty(documents, document, node, property, seen = new Set()) {
  if (node.required?.includes(property)) return true
  if (typeof node.$ref === 'string') {
    const key = `${document.$id}:${node.$ref}:${property}`
    if (seen.has(key)) return false
    seen.add(key)
    const resolved = resolveSchemaRef(documents, document, node.$ref)
    return schemaRequiresProperty(documents, resolved.document, resolved.node, property, seen)
  }
  if ((node.allOf ?? []).some(branch => (
    schemaRequiresProperty(documents, document, branch, property, seen)
  ))) return true
  return Array.isArray(node.oneOf)
    && node.oneOf.length > 0
    && node.oneOf.every(branch => (
      schemaRequiresProperty(documents, document, branch, property, new Set(seen))
    ))
}

function contractValidator() {
  const ajv = new Ajv2020({ allErrors: true, strict: true })
  addFormats(ajv)
  for (const [keyword, schemaType] of [
    ['x-authority', 'string'],
    ['x-direction', 'string'],
    ['x-winwincode-openapi', 'object'],
    ['x-winwincode-semantics', 'object'],
    ['x-winwincode-transports', 'object'],
  ]) {
    ajv.addKeyword({ keyword, schemaType, valid: true })
  }
  for (const name of schemaFiles) ajv.addSchema(schema(name))
  return ajv
}

function validator(ajv, schemaId, definition) {
  const ref = definition === undefined
    ? schemaId
    : `${schemaId}#/$defs/${definition}`
  const validate = ajv.getSchema(ref)
  assert.ok(validate, `schema did not compile: ${ref}`)
  return validate
}

function assertValidation(validate, value, expected, name) {
  assert.equal(
    validate(value),
    expected,
    `${name}: ${JSON.stringify(validate.errors)}`,
  )
}

function scopeKey(scope) {
  return [
    scope.kind,
    scope.organizationId,
    scope.workspaceId ?? '',
    scope.projectId ?? '',
    scope.repositoryId ?? '',
  ].join('/')
}

function streamKey(stream) {
  return `${stream.kind}/${
    stream.deliveryId
      ?? stream.productSessionId
      ?? stream.leaseId
      ?? 'scope'
  }`
}

function cursorKey(cursor) {
  return `${scopeKey(cursor.scope)}:${streamKey(cursor.stream)}`
}

function transcriptBaseline(transcript, subscribe) {
  if (subscribe.type === 'transport.resume.v1') {
    const accepted = transcript.frames.find(frame => (
      frame.type === 'transport.resume-accepted.v1'
      && frame.subscriptionId === subscribe.subscriptionId
    ))
    if (accepted === undefined) return { error: 'resume acceptance is missing' }
    if (
      cursorKey(accepted.after) !== cursorKey(subscribe.after)
      || accepted.after.sequence !== subscribe.after.sequence
      || accepted.after.eventId !== subscribe.after.eventId
    ) return { error: 'resume acceptance changes the requested cursor' }
    return { cursor: accepted.after, authorizationEpoch: accepted.authorizationEpoch }
  }

  const accepted = transcript.frames.find(frame => (
    frame.type === 'transport.subscription-accepted.v1'
    && frame.subscriptionId === subscribe.subscriptionId
  ))
  if (accepted === undefined) return { error: 'subscription acceptance is missing' }
  if (cursorKey(accepted.cursor) !== cursorKey(subscribe.subscription)) {
    return { error: 'subscription acceptance crosses the requested stream' }
  }
  if (
    typeof subscribe.startAt === 'object'
    && (
      cursorKey(subscribe.startAt) !== cursorKey(subscribe.subscription)
      || accepted.cursor.sequence !== subscribe.startAt.sequence
      || accepted.cursor.eventId !== subscribe.startAt.eventId
    )
  ) return { error: 'subscription acceptance changes the HTTP snapshot cursor' }
  return { cursor: accepted.cursor, authorizationEpoch: accepted.authorizationEpoch }
}

function transcriptError(transcript) {
  const subscribe = transcript.frames.find(frame => (
    frame.type === 'transport.subscribe.v1'
    || frame.type === 'transport.resume.v1'
  ))
  if (subscribe === undefined) return 'subscription is missing'

  const expectedCursor = subscribe.subscription
  if (
    subscribe.type === 'transport.subscribe.v1'
    && typeof subscribe.startAt === 'object'
    && cursorKey(subscribe.startAt) !== cursorKey(expectedCursor)
  ) return 'snapshot cursor crosses the subscribed stream'
  const baseline = transcriptBaseline(transcript, subscribe)
  if (baseline.error !== undefined) return baseline.error
  let lastSequence = baseline.cursor.sequence
  const authorizationEpochBaseline = baseline.authorizationEpoch
  let revokedEpoch = null
  for (const frame of transcript.frames) {
    if (frame.type === 'transport.authorization-revoked.v1') {
      revokedEpoch = frame.authorizationEpoch
      continue
    }
    if (frame.type === 'transport.ack.v1') {
      if (
        scopeKey(frame.cursor.scope) !== scopeKey(expectedCursor.scope)
        || streamKey(frame.cursor.stream) !== streamKey(expectedCursor.stream)
      ) return 'ack cursor crosses the subscribed stream'
      continue
    }
    if (frame.type !== 'event.v1') continue
    if (scopeKey(frame.scope) !== scopeKey(expectedCursor.scope)) {
      return 'event crosses the subscribed tenant scope'
    }
    if (streamKey(frame.stream) !== streamKey(expectedCursor.stream)) {
      return 'event crosses the subscribed resource stream'
    }
    if (frame.sequence !== lastSequence + 1) return 'event sequence is not continuous'
    if (frame.authorizationEpoch < authorizationEpochBaseline) {
      return 'event authorization epoch predates the accepted baseline'
    }
    if (revokedEpoch !== null && frame.authorizationEpoch <= revokedEpoch) {
      return 'event was sent after authorization was revoked'
    }
    if (
      frame.stream.kind === 'delivery'
      && 'deliveryId' in frame.event
      && frame.event.deliveryId !== frame.stream.deliveryId
    ) return 'event payload crosses the delivery stream'
    if (
      frame.stream.kind === 'product-session'
      && 'productSessionId' in frame.event
      && frame.event.productSessionId !== frame.stream.productSessionId
    ) return 'event payload crosses the ProductSession stream'
    if (
      frame.source.kind === 'execution-worker'
      && frame.stream.kind === 'lease'
      && frame.source.leaseId !== frame.stream.leaseId
    ) return 'event source crosses the Lease stream'
    lastSequence = frame.sequence
  }
  return null
}

function markdownLinks(path) {
  const text = readFileSync(path, 'utf8')
  return [...text.matchAll(/\[[^\]]+\]\(([^)]+)\)/gu)]
    .map(match => match[1])
    .filter(target => !/^(?:https?:|mailto:|#)/u.test(target))
    .map(target => resolve(dirname(path), target.split('#', 1)[0]))
}

test('all public v1 schemas compile together with strict Draft 2020-12 references', () => {
  const ajv = contractValidator()

  for (const name of schemaFiles) {
    const document = schema(name)
    assert.equal(document.$id, `${schemaBase}${name}`)
    assert.ok(validator(ajv, document.$id))
  }
})

test('every raw reference resolves, including references inside generated OpenAPI metadata', () => {
  const documents = schemaDocuments()

  for (const document of documents.values()) {
    visitSchema(document, node => {
      if (typeof node.$ref === 'string') {
        resolveSchemaRef(documents, document, node.$ref)
      }
    })
  }
})

test('schema dependency direction keeps neutral event cursors out of transport cycles', () => {
  const documents = schemaDocuments()
  const dependencies = new Map(schemaFiles.map(name => [name, new Set()]))

  for (const [fileName, document] of schemaFiles.map(name => [name, schema(name)])) {
    visitSchema(document, node => {
      if (typeof node.$ref !== 'string') return
      const target = resolveSchemaRef(documents, document, node.$ref).document
      const targetName = new URL(target.$id).pathname.split('/').at(-1)
      if (targetName !== fileName) dependencies.get(fileName).add(targetName)
    })
  }

  assert.deepEqual([...dependencies.get('domain.schema.json')], [])
  assert.deepEqual([...dependencies.get('control-plane-events.schema.json')], [
    'domain.schema.json',
  ])
  assert.deepEqual([...dependencies.get('control-plane-http.schema.json')], [
    'domain.schema.json',
  ])
  assert.deepEqual([...dependencies.get('execution-port.schema.json')], [
    'domain.schema.json',
  ])

  const domain = documents.get(`${schemaBase}domain.schema.json`)
  assert.ok(domain.$defs.EventReadCursor)
  assert.equal(
    Object.keys(domain.$defs).some(name => name.startsWith('ControlPlaneWebSocket')),
    false,
  )
  const events = documents.get(`${schemaBase}control-plane-events.schema.json`)
  assert.deepEqual(events.$defs.ControlPlaneWebSocketSubscribeStartAt.oneOf, [
    { $ref: '#/$defs/ControlPlaneWebSocketSubscribeOrigin' },
    { $ref: './domain.schema.json#/$defs/EventReadCursor' },
  ])
})

test('public unions have one required discriminator and no schema copies', () => {
  const documents = schemaDocuments()
  const taggedUnions = [
    ['domain.schema.json', 'Actor', 'kind'],
    ['domain.schema.json', 'Scope', 'kind'],
    ['control-plane-http.schema.json', 'CommandRequest', 'command'],
    ['control-plane-http.schema.json', 'QueryRequest', 'query'],
    ['control-plane-events.schema.json', 'ControlPlaneWebSocketEventPayload', 'type'],
    ['control-plane-events.schema.json', 'ControlPlaneWebSocketClientFrame', 'type'],
    ['control-plane-events.schema.json', 'ControlPlaneWebSocketServerFrame', 'type'],
    ['execution-port.schema.json', 'ExecutionScope', 'kind'],
    ['execution-port.schema.json', 'ExecutionPortMessage', 'kind'],
  ]

  for (const [file, definition, discriminator] of taggedUnions) {
    const document = documents.get(`${schemaBase}${file}`)
    const union = document.$defs[definition]
    const values = union.oneOf.map(branch => {
      const value = schemaPropertyConst(
        documents,
        document,
        branch,
        discriminator,
      )
      assert.notEqual(value, undefined, `${definition} branch lacks ${discriminator}`)
      assert.equal(
        schemaRequiresProperty(documents, document, branch, discriminator),
        true,
        `${definition}.${value} does not require ${discriminator}`,
      )
      return value
    })
    assert.equal(new Set(values).size, values.length, `${definition} repeats a discriminator`)
  }

  const definitionsByFile = schemaFiles.map(file => (
    [file, new Set(Object.keys(documents.get(`${schemaBase}${file}`).$defs))]
  ))
  for (let left = 0; left < definitionsByFile.length; left += 1) {
    for (let right = left + 1; right < definitionsByFile.length; right += 1) {
      const [leftFile, leftDefinitions] = definitionsByFile[left]
      const [rightFile, rightDefinitions] = definitionsByFile[right]
      const copies = [...leftDefinitions].filter(name => rightDefinitions.has(name))
      assert.deepEqual(copies, [], `${leftFile} and ${rightFile} copy public definitions`)
    }
  }
})

test('public objects stay closed and error details inherit every authority redaction', () => {
  const documents = schemaDocuments()
  const openObjects = new Map(schemaFiles.map(file => [file, []]))
  for (const file of schemaFiles) {
    const document = documents.get(`${schemaBase}${file}`)
    visitSchema(document, (node, path) => {
      if (node.type === 'object' && node.additionalProperties !== false) {
        openObjects.get(file).push(path)
      }
    })
  }

  assert.deepEqual(openObjects.get('domain.schema.json'), [
    '#/$defs/CommandEnvelope/properties/payload',
    '#/$defs/ErrorDetails',
  ])
  assert.deepEqual(openObjects.get('control-plane-events.schema.json'), [])
  assert.deepEqual(openObjects.get('execution-port.schema.json'), [])
  for (const path of openObjects.get('control-plane-http.schema.json')) {
    assert.equal(
      path === '#/$defs/QueryEnvelope/properties/parameters'
      || /^#\/\$defs\/[A-Za-z][A-Za-z0-9]*(?:Command|Query|CompletedResponse|ResultResponse)\/allOf\/1$/u.test(path),
      true,
      `HTTP object is open outside an envelope specialization: ${path}`,
    )
  }

  const matrix = json(join(
    root,
    'docs',
    'contracts',
    'control-plane-api-coverage.matrix.json',
  ))
  const domain = documents.get(`${schemaBase}domain.schema.json`)
  const redactedErrorProperties = new Set(
    domain.$defs.ErrorDetails.propertyNames.not.enum,
  )
  for (const property of [
    ...matrix.authorityBoundaries.forbiddenPublicProperties,
    'password',
    'vaultLocator',
  ]) {
    assert.equal(
      redactedErrorProperties.has(property),
      true,
      `ErrorDetails permits authority-sensitive property ${property}`,
    )
  }

  const http = documents.get(`${schemaBase}control-plane-http.schema.json`)
  assert.equal(http.$defs.CredentialReferenceCreatePayload.properties.vaultLocator.writeOnly, true)
  for (const file of ['control-plane-http.schema.json', 'control-plane-events.schema.json', 'execution-port.schema.json']) {
    const document = documents.get(`${schemaBase}${file}`)
    visitSchema(document, (node, path) => {
      for (const property of Object.keys(node.properties ?? {})) {
        if (property === 'vaultLocator' && path === '#/$defs/CredentialReferenceCreatePayload') {
          continue
        }
        assert.equal(
          matrix.authorityBoundaries.forbiddenPublicProperties.includes(property),
          false,
          `${file}${path} exposes forbidden property ${property}`,
        )
      }
    })
  }
})

test('strict validation covers every canonical domain sample and keeps IDs distinct', () => {
  const ajv = contractValidator()
  const domainId = `${schemaBase}domain.schema.json`
  const samples = json(join(schemaRoot, 'domain.samples.json'))

  assert.equal(samples.schemaId, domainId)
  for (const sample of samples.cases) {
    assertValidation(
      validator(ajv, domainId, sample.definition),
      sample.value,
      sample.valid,
      sample.name,
    )
  }

  const idValues = Object.freeze([
    [domainId, 'ApprovalId', 'apr_01J00000000000000000000000'],
    [domainId, 'AttentionItemId', 'att_01J00000000000000000000000'],
    [domainId, 'ChatMessageId', 'msg_01J00000000000000000000000'],
    [domainId, 'CodexThreadId', 'cdx_01J00000000000000000000000'],
    [domainId, 'CredentialReferenceId', 'crd_01J00000000000000000000000'],
    [domainId, 'DeliveryId', 'dlv_01J00000000000000000000000'],
    [domainId, 'DeliveryTaskId', 'dtk_01J00000000000000000000000'],
    [domainId, 'EvidenceId', 'evd_01J00000000000000000000000'],
    [domainId, 'ExecutionJobId', 'job_01J00000000000000000000000'],
    [domainId, 'InputRequestId', 'inp_01J00000000000000000000000'],
    [domainId, 'LeaseId', 'lse_01J00000000000000000000000'],
    [domainId, 'OrganizationId', 'org_01J00000000000000000000000'],
    [domainId, 'ProductSessionId', 'psn_01J00000000000000000000000'],
    [domainId, 'ProjectId', 'prj_01J00000000000000000000000'],
    [domainId, 'PublicationId', 'pub_01J00000000000000000000000'],
    [domainId, 'RepositoryId', 'rep_01J00000000000000000000000'],
    [domainId, 'RequestId', 'req_01J00000000000000000000000'],
    [domainId, 'ServiceAccountId', 'svc_01J00000000000000000000000'],
    [domainId, 'StageRunId', 'run_01J00000000000000000000000'],
    [domainId, 'SystemActorId', 'sys_01J00000000000000000000000'],
    [domainId, 'UserId', 'usr_01J00000000000000000000000'],
    [domainId, 'WorkerId', 'wrk_01J00000000000000000000000'],
    [domainId, 'WorkerSessionId', 'wsn_01J00000000000000000000000'],
    [domainId, 'WorkspaceId', 'wsp_01J00000000000000000000000'],
    [domainId, 'ControlPlaneEventId', 'evt_01J00000000000000000000000'],
    [`${schemaBase}control-plane-events.schema.json`, 'ControlPlaneWebSocketSubscriptionId', 'sub_01J00000000000000000000000'],
    [`${schemaBase}execution-port.schema.json`, 'ArtifactId', 'art_01J00000000000000000000000'],
    [`${schemaBase}execution-port.schema.json`, 'ExecutionEventId', 'xevt_01J00000000000000000000000'],
    [`${schemaBase}execution-port.schema.json`, 'ExecutionMessageId', 'xmsg_01J00000000000000000000000'],
    [`${schemaBase}execution-port.schema.json`, 'ModelExchangeId', 'mdl_01J00000000000000000000000'],
    [`${schemaBase}execution-port.schema.json`, 'WorkerInstanceId', 'wki_01J00000000000000000000000'],
  ])
  for (const [schemaId, definition, ownValue] of idValues) {
    const validate = validator(ajv, schemaId, definition)
    assertValidation(validate, ownValue, true, definition)
    for (const [, otherDefinition, otherValue] of idValues) {
      if (otherValue !== ownValue) {
        assertValidation(
          validate,
          otherValue,
          false,
          `${definition} rejects ${otherDefinition}`,
        )
      }
    }
    assertValidation(validate, null, false, `${definition} rejects null`)
  }
})

test('strict HTTP validation covers requests, responses, errors, and negative boundaries', () => {
  const ajv = contractValidator()
  const httpId = `${schemaBase}control-plane-http.schema.json`
  const domainId = `${schemaBase}domain.schema.json`
  const examples = json(join(schemaRoot, 'examples', 'control-plane-http.examples.json'))
  const command = validator(ajv, httpId, 'CommandRequest')
  const query = validator(ajv, httpId, 'QueryRequest')

  for (const [name, value] of Object.entries(examples.positive)) {
    assertValidation(
      value.command === undefined ? query : command,
      value,
      true,
      name,
    )
  }
  for (const [name, value] of Object.entries({
    workerDrain: examples.idempotency.original,
    workerDrainRetry: examples.idempotency.retry,
    workerDrainConflictShape: examples.idempotency.conflict,
  })) assertValidation(command, value, true, name)

  assertValidation(
    validator(ajv, domainId, 'ErrorEnvelope'),
    examples.idempotency.expectedConflict,
    true,
    'idempotency expected conflict',
  )
  for (const [name, value] of Object.entries({
    revisionConflict: examples.revisionConflict,
    invalidCursor: examples.invalidCursor,
    readCursorExpired: examples.readCursorExpired,
  })) {
    assertValidation(
      validator(ajv, domainId, 'ErrorEnvelope'),
      value,
      true,
      name,
    )
  }
  const leakedError = structuredClone(examples.revisionConflict)
  leakedError.error.details.provider = {
    diagnostics: [{ apiKey: 'must-not-cross-the-boundary' }],
  }
  assertValidation(
    validator(ajv, domainId, 'ErrorEnvelope'),
    leakedError,
    false,
    'error details reject secret-bearing fields',
  )
  assertValidation(
    validator(ajv, httpId, 'CommandCompletedResponse'),
    examples.responses.commandCompleted,
    true,
    'commandCompleted',
  )
  assertValidation(
    validator(ajv, httpId, 'QueryResultResponse'),
    examples.responses.queryPage,
    true,
    'queryPage',
  )
  for (const name of [
    'chatMessagesPage',
    'deliveryDetailPendingReview',
    'runtimeProjection',
    'publicationProjection',
  ]) {
    assertValidation(
      validator(ajv, httpId, 'QueryResultResponse'),
      examples.responses[name],
      true,
      name,
    )
  }
  const crossRepositoryPublication = structuredClone(
    examples.responses.publicationProjection,
  )
  crossRepositoryPublication.result.target.repository = 'openai/winwincode'
  crossRepositoryPublication.result.target.headRepository = 'contributor/winwincode'
  assertValidation(
    validator(ajv, httpId, 'QueryResultResponse'),
    crossRepositoryPublication,
    true,
    'publication target preserves an exact fork identity',
  )
  const publicationWithoutHeadRepository = structuredClone(
    crossRepositoryPublication,
  )
  delete publicationWithoutHeadRepository.result.target.headRepository
  assertValidation(
    validator(ajv, httpId, 'QueryResultResponse'),
    publicationWithoutHeadRepository,
    false,
    'publication target requires its exact head repository',
  )
  const settledReviewExamples = {
    approvedReview: {
      reviewStatus: 'approved',
      decision: 'approve',
      comments: null,
      requestedChanges: null,
    },
    changesRequestedReview: {
      reviewStatus: 'changes_requested',
      decision: 'request_changes',
      comments: 'Please keep the retry boundary explicit.',
      requestedChanges: ['Add the bounded retry check.'],
    },
    rejectedReview: {
      reviewStatus: 'rejected',
      decision: 'reject',
      comments: null,
      requestedChanges: null,
    },
  }
  for (const [name, reviewFields] of Object.entries(settledReviewExamples)) {
    const response = structuredClone(examples.responses.deliveryDetailPendingReview)
    Object.assign(response.result.solutionReview, reviewFields, {
      reviewerId: 'usr_00000000000000000000000000',
      reviewedAt: '2026-08-24T09:02:00.000Z',
    })
    assertValidation(
      validator(ajv, httpId, 'QueryResultResponse'),
      response,
      true,
      name,
    )
  }
  const pendingReviewWithReviewer = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  pendingReviewWithReviewer.result.solutionReview.reviewerId = 'usr_00000000000000000000000000'
  pendingReviewWithReviewer.result.solutionReview.reviewedAt = '2026-08-24T09:02:00.000Z'
  const changesWithoutRequests = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  Object.assign(changesWithoutRequests.result.solutionReview, {
    reviewStatus: 'changes_requested',
    decision: 'request_changes',
    reviewerId: 'usr_00000000000000000000000000',
    reviewedAt: '2026-08-24T09:02:00.000Z',
    requestedChanges: null,
  })
  const pendingReviewWithComments = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  pendingReviewWithComments.result.solutionReview.comments = 'Caller-authored pending comment'
  const approvedReviewWithRequests = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  Object.assign(approvedReviewWithRequests.result.solutionReview, {
    reviewStatus: 'approved',
    decision: 'approve',
    reviewerId: 'usr_00000000000000000000000000',
    reviewedAt: '2026-08-24T09:02:00.000Z',
    requestedChanges: ['This cannot coexist with approval.'],
  })
  const plannerAssignedTaskOwner = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  plannerAssignedTaskOwner.result.solutionReview.taskProposals[0].ownerActorId =
    'usr_00000000000000000000000000'
  const legacyApprovedSolution = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  legacyApprovedSolution.result.solution = legacyApprovedSolution.result.solutionReview
  delete legacyApprovedSolution.result.solutionReview
  const rawAttentionReview = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  rawAttentionReview.result.solutionReview.context = { raw: true }
  const codexStageWithoutBinding = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  codexStageWithoutBinding.result.stages
    .find(stage => stage.actorType === 'codex').sessionBinding = null
  const humanStageWithExecutionBinding = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  const humanStage = humanStageWithExecutionBinding.result.stages
    .find(stage => stage.actorType === 'human')
  humanStage.sessionBinding = structuredClone(
    examples.responses.deliveryDetailPendingReview.result.stages
      .find(stage => stage.actorType === 'codex').sessionBinding,
  )
  const partialBindingWithThreadOnly = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  const partialBinding = partialBindingWithThreadOnly.result.stages
    .find(stage => stage.actorType === 'codex').sessionBinding
  partialBinding.workerSessionId = null
  const publicationWithArbitraryUrl = structuredClone(
    examples.responses.publicationProjection,
  )
  publicationWithArbitraryUrl.result.resourceRef =
    'https://token@example.com/repository/pull/42?access_token=secret#fragment'
  const publicationWithSecretBearingRepository = structuredClone(
    examples.responses.publicationProjection,
  )
  publicationWithSecretBearingRepository.result.resourceRef.repository =
    'openai/winwincode?access_token=secret'
  const publicationWithWebUrl = structuredClone(
    examples.responses.publicationProjection,
  )
  publicationWithWebUrl.result.resourceRef.webUrl =
    'https://github.com/openai/winwincode/pull/42'
  const publicationWithInvalidNumber = structuredClone(
    examples.responses.publicationProjection,
  )
  publicationWithInvalidNumber.result.resourceRef.number = 0
  for (const [name, value] of Object.entries({
    pendingReviewWithReviewer,
    pendingReviewWithComments,
    changesWithoutRequests,
    approvedReviewWithRequests,
    plannerAssignedTaskOwner,
    legacyApprovedSolution,
    rawAttentionReview,
    codexStageWithoutBinding,
    humanStageWithExecutionBinding,
    partialBindingWithThreadOnly,
    publicationWithArbitraryUrl,
    publicationWithSecretBearingRepository,
    publicationWithWebUrl,
    publicationWithInvalidNumber,
  })) {
    assertValidation(
      validator(ajv, httpId, 'QueryResultResponse'),
      value,
      false,
      name,
    )
  }
  assertValidation(
    validator(ajv, httpId, 'CredentialReferenceProjection'),
    examples.responses.credentialReference,
    true,
    'credentialReference',
  )

  const wrongVersion = structuredClone(examples.positive.sessionCreate)
  wrongVersion.schemaVersion = 'winwincode/v2'
  const missingRevision = structuredClone(examples.positive.sessionCreate)
  delete missingRevision.expectedRevision
  const nullPayload = structuredClone(examples.positive.sessionCreate)
  nullPayload.payload = null
  const swappedId = structuredClone(examples.positive.sessionCreate)
  swappedId.payload.productSessionId = swappedId.payload.repositoryId
  const extraField = structuredClone(examples.positive.sessionCreate)
  extraField.secret = 'must-not-cross-the-boundary'
  const callerAuthoredTaskBreakdown = structuredClone(
    examples.positive.deliveryApproveTaskBreakdown,
  )
  callerAuthoredTaskBreakdown.payload.tasks = [{
    id: 'dtk_01J00000000000000000000000',
    title: 'Caller replacement',
    goal: 'Bypass the reviewed task proposals.',
    acceptanceCriterionIds: ['criterion:1'],
    blockedByTaskIds: [],
    ownerActorId: null,
  }]
  for (const [name, value] of Object.entries({
    wrongVersion,
    missingRevision,
    nullPayload,
    swappedId,
    extraField,
    callerAuthoredTaskBreakdown,
  })) assertValidation(command, value, false, name)

  const leakedCredential = structuredClone(examples.responses.credentialReference)
  leakedCredential.vaultLocator = 'vault://internal/path'
  assertValidation(
    validator(ajv, httpId, 'CredentialReferenceProjection'),
    leakedCredential,
    false,
    'credential projection rejects its vault locator',
  )
})

test('HTTP response discriminators, repository scope, and actor references fail closed', () => {
  const ajv = contractValidator()
  const httpId = `${schemaBase}control-plane-http.schema.json`
  const examples = json(join(schemaRoot, 'examples', 'control-plane-http.examples.json'))
  const queryRequest = validator(ajv, httpId, 'QueryRequest')
  const queryResponse = validator(ajv, httpId, 'QueryResultResponse')
  const commandResponse = validator(ajv, httpId, 'CommandCompletedResponse')

  const relabeledQueryResponse = structuredClone(examples.responses.runtimeProjection)
  relabeledQueryResponse.query = 'delivery.get'
  assertValidation(
    queryResponse,
    relabeledQueryResponse,
    false,
    'query response cannot relabel a runtime result as delivery.get',
  )

  const wrongQueryResult = structuredClone(examples.responses.runtimeProjection)
  wrongQueryResult.result = structuredClone(examples.responses.queryPage.result)
  assertValidation(
    queryResponse,
    wrongQueryResult,
    false,
    'runtime.projection.get cannot return a Delivery page',
  )

  const relabeledCommandResponse = structuredClone(examples.responses.commandCompleted)
  relabeledCommandResponse.command = 'delivery.create'
  assertValidation(
    commandResponse,
    relabeledCommandResponse,
    false,
    'command response cannot relabel a Worker projection as delivery.create',
  )

  for (const query of [examples.positive.deliveryGet, examples.positive.runtimeProjectionGet]) {
    const wrongScope = structuredClone(query)
    wrongScope.scope = {
      kind: 'organization',
      organizationId: 'org_00000000000000000000000000',
    }
    assertValidation(
      queryRequest,
      wrongScope,
      false,
      `${query.query} requires a complete repository scope`,
    )
  }

  const forgedPublicationApprover = structuredClone(
    examples.responses.publicationProjection,
  )
  forgedPublicationApprover.result.approvedBy = 'Authorization: Bearer secret'
  assertValidation(
    queryResponse,
    forgedPublicationApprover,
    false,
    'publication approver must be a canonical ActorId',
  )

  const forgedAttentionResolver = structuredClone(
    examples.responses.deliveryDetailPendingReview,
  )
  forgedAttentionResolver.result.attention[0].assignedTo = 'credential=secret'
  assertValidation(
    queryResponse,
    forgedAttentionResolver,
    false,
    'Attention assignee must be a canonical ActorId',
  )
})

test('strict WebSocket validation covers every frame before transcript invariants', () => {
  const ajv = contractValidator()
  const eventsId = `${schemaBase}control-plane-events.schema.json`
  const validate = validator(ajv, eventsId)
  const valid = json(join(root, 'tests', 'fixtures', 'control-plane-websocket.valid.json'))
  const invalid = json(join(root, 'tests', 'fixtures', 'control-plane-websocket.invalid.json'))

  for (const transcript of valid.transcripts) {
    for (const frame of transcript.frames) {
      assertValidation(validate, frame, true, `${transcript.name}: ${frame.type}`)
    }
    assert.equal(transcriptError(transcript), null, transcript.name)
  }

  for (const transcript of invalid.transcripts) {
    const shapeRejected = transcript.frames.some(frame => !validate(frame))
    assert.equal(
      shapeRejected || transcriptError(transcript) !== null,
      true,
      `${transcript.name} unexpectedly passed shape and stream validation`,
    )
  }
})

test('strict ExecutionPort validation covers every positive and negative message', () => {
  const ajv = contractValidator()
  const executionId = `${schemaBase}execution-port.schema.json`
  const validate = validator(ajv, executionId)
  const valid = json(join(root, 'tests', 'fixtures', 'contracts', 'execution-port.valid.json'))
  const invalid = json(join(root, 'tests', 'fixtures', 'contracts', 'execution-port.invalid.json'))

  for (const message of valid.messages) {
    assertValidation(validate, message, true, message.kind)
  }
  for (const invalidCase of invalid.cases) {
    assertValidation(validate, invalidCase.message, false, invalidCase.name)
  }
})

test('architecture and README links keep every public contract discoverable', () => {
  const documents = [
    join(root, 'README.md'),
    join(root, 'docs', 'architecture.md'),
    join(root, 'docs', 'contracts', 'control-plane-websocket.md'),
    join(root, 'docs', 'contracts', 'execution-port-v1.md'),
    join(schemaRoot, 'README.md'),
  ]
  for (const document of documents) {
    for (const target of markdownLinks(document)) {
      assert.equal(
        readFileSync(target, 'utf8').length > 0,
        true,
        `${relative(root, document)} has a broken link to ${relative(root, target)}`,
      )
    }
  }

  const readme = readFileSync(join(root, 'README.md'), 'utf8')
  const architecture = readFileSync(join(root, 'docs', 'architecture.md'), 'utf8')
  for (const target of [
    'schema/winwincode/v1/control-plane-http.schema.json',
    'schema/winwincode/v1/control-plane-events.schema.json',
    'schema/winwincode/v1/execution-port.schema.json',
    'docs/contracts/control-plane-websocket.md',
    'docs/contracts/execution-port-v1.md',
  ]) {
    assert.equal(
      readme.includes(target) || architecture.includes(target),
      true,
      `public contract is not linked from README or architecture: ${target}`,
    )
  }
})
