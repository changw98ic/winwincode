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

function transcriptError(transcript) {
  const subscribe = transcript.frames.find(frame => (
    frame.type === 'transport.subscribe.v1'
    || frame.type === 'transport.resume.v1'
  ))
  if (subscribe === undefined) return 'subscription is missing'

  const expectedCursor = subscribe.subscription
  let lastSequence = subscribe.after?.sequence ?? 0
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
    if (frame.sequence <= lastSequence) return 'event sequence is not monotonic'
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

  const idValues = Object.freeze({
    DeliveryId: 'dlv_01J00000000000000000000000',
    ProductSessionId: 'psn_01J00000000000000000000000',
    StageRunId: 'run_01J00000000000000000000000',
    WorkerId: 'wrk_01J00000000000000000000000',
    WorkerSessionId: 'wsn_01J00000000000000000000000',
  })
  for (const [definition, ownValue] of Object.entries(idValues)) {
    const validate = validator(ajv, domainId, definition)
    assertValidation(validate, ownValue, true, definition)
    for (const otherValue of Object.values(idValues)) {
      if (otherValue !== ownValue) {
        assertValidation(validate, otherValue, false, `${definition} rejects ${otherValue}`)
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

  for (const [name, value] of Object.entries({
    sessionCreate: examples.positive.sessionCreate,
    workerDrain: examples.idempotency.original,
    workerDrainRetry: examples.idempotency.retry,
    workerDrainConflictShape: examples.idempotency.conflict,
  })) assertValidation(command, value, true, name)
  assertValidation(query, examples.positive.deliveryList, true, 'deliveryList')

  assertValidation(
    validator(ajv, domainId, 'ErrorEnvelope'),
    examples.idempotency.expectedConflict,
    true,
    'idempotency expected conflict',
  )
  for (const [name, value] of Object.entries({
    revisionConflict: examples.revisionConflict,
    invalidCursor: examples.invalidCursor,
  })) {
    assertValidation(
      validator(ajv, domainId, 'ErrorEnvelope'),
      value,
      true,
      name,
    )
  }
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
  for (const [name, value] of Object.entries({
    wrongVersion,
    missingRevision,
    nullPayload,
    swappedId,
    extraField,
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
