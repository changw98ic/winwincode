import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const schemaPath = join(
  root,
  'schema',
  'winwincode',
  'v1',
  'control-plane-events.schema.json',
)
const contractPath = join(root, 'docs', 'contracts', 'control-plane-websocket.md')
const validFixturePath = join(
  root,
  'tests',
  'fixtures',
  'control-plane-websocket.valid.json',
)
const invalidFixturePath = join(
  root,
  'tests',
  'fixtures',
  'control-plane-websocket.invalid.json',
)

const schema = JSON.parse(readFileSync(schemaPath, 'utf8'))
const contract = readFileSync(contractPath, 'utf8')
const validFixture = JSON.parse(readFileSync(validFixturePath, 'utf8'))
const invalidFixture = JSON.parse(readFileSync(invalidFixturePath, 'utf8'))

const eventTypes = Object.freeze([
  'activity.recorded.v1',
  'approval.changed.v1',
  'attention.changed.v1',
  'delivery.changed.v1',
  'delivery-task.changed.v1',
  'presence.changed.v1',
  'product-session.changed.v1',
  'runtime-projection.appended.v1',
  'worker-health.changed.v1',
])

const clientFrameTypes = Object.freeze([
  'transport.ack.v1',
  'transport.pong.v1',
  'transport.resume.v1',
  'transport.subscribe.v1',
])

function branchConst(branchRef, propertyName) {
  const name = branchRef.$ref.split('/').at(-1)
  return schema.$defs[name].properties[propertyName].const
}

function collectRefs(value, refs = []) {
  if (Array.isArray(value)) {
    for (const item of value) collectRefs(item, refs)
    return refs
  }
  if (value === null || typeof value !== 'object') return refs
  if (typeof value.$ref === 'string') refs.push(value.$ref)
  for (const child of Object.values(value)) collectRefs(child, refs)
  return refs
}

function resolveInternalRef(ref) {
  assert.match(ref, /^#\/\$defs\/[A-Za-z][A-Za-z0-9]+$/u)
  const name = ref.split('/').at(-1)
  assert.ok(schema.$defs[name], `missing local schema definition: ${name}`)
  return schema.$defs[name]
}

function validateSchemaNode(value, node, path = '$') {
  if (node.$ref) {
    if (node.$ref.startsWith('./domain.schema.json')) {
      return value === null ? [`${path}: canonical domain value cannot be null`] : []
    }
    return validateSchemaNode(value, resolveInternalRef(node.$ref), path)
  }
  if (node.oneOf) {
    const results = node.oneOf.map(branch => validateSchemaNode(value, branch, path))
    const matches = results.filter(errors => errors.length === 0)
    return matches.length === 1
      ? []
      : [`${path}: expected exactly one matching union branch`]
  }
  if ('const' in node && value !== node.const) {
    return [`${path}: expected ${JSON.stringify(node.const)}`]
  }
  if (node.enum && !node.enum.includes(value)) return [`${path}: value is not in enum`]

  const errors = []
  if (node.type === 'null') {
    if (value !== null) errors.push(`${path}: expected null`)
    return errors
  }
  if (node.type === 'string') {
    if (typeof value !== 'string') return [`${path}: expected string`]
    if (node.minLength !== undefined && value.length < node.minLength) {
      errors.push(`${path}: string is shorter than minLength`)
    }
    if (node.maxLength !== undefined && value.length > node.maxLength) {
      errors.push(`${path}: string is longer than maxLength`)
    }
    if (node.pattern && !(new RegExp(node.pattern, 'u')).test(value)) {
      errors.push(`${path}: string does not match pattern`)
    }
    return errors
  }
  if (node.type === 'integer') {
    if (!Number.isSafeInteger(value)) return [`${path}: expected safe integer`]
    if (node.minimum !== undefined && value < node.minimum) {
      errors.push(`${path}: integer is below minimum`)
    }
    if (node.maximum !== undefined && value > node.maximum) {
      errors.push(`${path}: integer is above maximum`)
    }
    return errors
  }
  if (node.type === 'array') {
    if (!Array.isArray(value)) return [`${path}: expected array`]
    if (node.minItems !== undefined && value.length < node.minItems) {
      errors.push(`${path}: array is shorter than minItems`)
    }
    if (node.uniqueItems) {
      const encoded = value.map(item => JSON.stringify(item))
      if (new Set(encoded).size !== encoded.length) errors.push(`${path}: items are not unique`)
    }
    if (node.items) {
      value.forEach((item, index) => {
        errors.push(...validateSchemaNode(item, node.items, `${path}[${index}]`))
      })
    }
    return errors
  }
  if (node.type === 'object') {
    if (value === null || Array.isArray(value) || typeof value !== 'object') {
      return [`${path}: expected object`]
    }
    for (const field of node.required ?? []) {
      if (!(field in value)) errors.push(`${path}: missing ${field}`)
    }
    if (node.additionalProperties === false) {
      for (const field of Object.keys(value)) {
        if (!(field in (node.properties ?? {}))) errors.push(`${path}: unexpected ${field}`)
      }
    }
    for (const [field, child] of Object.entries(node.properties ?? {})) {
      if (field in value) {
        errors.push(...validateSchemaNode(value[field], child, `${path}.${field}`))
      }
    }
  }
  return errors
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

function validateFrameShape(frame) {
  const allowedTypes = new Set([
    ...clientFrameTypes,
    ...schema.$defs.ControlPlaneWebSocketServerFrame.oneOf.map(branch => branchConst(branch, 'type')),
  ])
  if (!allowedTypes.has(frame.type)) return 'frame type is not in the protocol union'
  if (frame.type !== 'event.v1') return null
  for (const field of [
    'subscriptionId',
    'eventId',
    'scope',
    'stream',
    'sequence',
    'occurredAt',
    'source',
    'authorizationEpoch',
    'event',
  ]) {
    if (!(field in frame)) return `event is missing ${field}`
  }
  if (!eventTypes.includes(frame.event.type)) return 'event payload is not supported'
  return null
}

function validateTranscript(transcript) {
  const shapeFailure = transcript.frames
    .map(validateFrameShape)
    .find(Boolean)
  if (shapeFailure) return shapeFailure

  const subscribe = transcript.frames.find(frame => (
    frame.type === 'transport.subscribe.v1'
    || frame.type === 'transport.resume.v1'
  ))
  if (!subscribe) return 'subscription is missing'
  const expectedCursor = subscribe.subscription

  let lastSequence = subscribe.after?.sequence ?? 0
  let revokedEpoch = null
  for (const frame of transcript.frames) {
    if (frame.type === 'transport.authorization-revoked.v1') {
      revokedEpoch = frame.authorizationEpoch
      continue
    }
    if (frame.type === 'transport.ack.v1') {
      if (cursorKey(frame.cursor) !== cursorKey(expectedCursor)) {
        return 'ack cursor crosses the subscribed stream'
      }
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

test('event schema has one stable source and reuses canonical domain IDs', () => {
  assert.equal(
    schema.$id,
    'https://schemas.winwincode.dev/v1/control-plane-events.schema.json',
  )
  assert.equal(schema.$schema, 'https://json-schema.org/draft/2020-12/schema')
  assert.deepEqual(Object.keys(schema.$defs).filter(name => (
    /^(?:Organization|Workspace|Project|Repository|Delivery|DeliveryTask|ProductSession|StageRun|Worker|WorkerSession|Lease|Approval|AttentionItem|User|CodexThread)Id$/u
      .test(name)
  )), [])

  const domainRefs = collectRefs(schema).filter(ref => ref.includes('domain.schema.json'))
  assert.ok(domainRefs.length >= 30)
  for (const ref of domainRefs) {
    assert.match(ref, /^\.\/domain\.schema\.json#\/\$defs\/[A-Za-z][A-Za-z0-9]+$/u)
  }

  for (const ref of collectRefs(schema).filter(ref => ref.startsWith('#/'))) {
    resolveInternalRef(ref)
  }
})

test('event envelope carries ordered scope, time, source, and authorization facts', () => {
  const eventFrame = schema.$defs.ControlPlaneWebSocketEventFrame
  for (const field of [
    'type',
    'subscriptionId',
    'eventId',
    'scope',
    'stream',
    'sequence',
    'occurredAt',
    'source',
    'authorizationEpoch',
    'event',
  ]) assert.ok(eventFrame.required.includes(field), field)

  assert.equal(eventFrame.properties.type.const, 'event.v1')
  assert.equal(eventFrame.properties.scope.$ref, './domain.schema.json#/$defs/Scope')
  assert.equal(eventFrame.properties.occurredAt.$ref, './domain.schema.json#/$defs/Instant')
  assert.equal(schema.$defs.ControlPlaneWebSocketEventSequence.minimum, 1)
  assert.equal(schema.$defs.ControlPlaneWebSocketStreamPosition.minimum, 0)
  assert.deepEqual(
    schema.$defs.ControlPlaneWebSocketEventPayload.oneOf.map(branch => branchConst(branch, 'type')).sort(),
    [...eventTypes].sort(),
  )
})

test('client frames only subscribe, resume, acknowledge, or answer a heartbeat', () => {
  assert.deepEqual(
    schema.$defs.ControlPlaneWebSocketClientFrame.oneOf.map(branch => branchConst(branch, 'type')).sort(),
    [...clientFrameTypes].sort(),
  )
  assert.equal(JSON.stringify(schema.$defs.ControlPlaneWebSocketClientFrame).includes('Command'), false)
  assert.equal(JSON.stringify(schema.$defs.ControlPlaneWebSocketClientFrame).includes('command'), false)
  assert.match(contract, /主要业务写入只走 HTTP/u)
  assert.match(contract, /WebSocket[^\n]+不接受主要业务 command/u)
})

test('resume, authorization recheck, and slow-client rules are machine visible', () => {
  assert.equal(schema.$defs.ControlPlaneWebSocketTransportLimits.properties.maxUnackedEvents.const, 256)
  assert.equal(schema.$defs.ControlPlaneWebSocketTransportLimits.properties.hardUnackedEvents.const, 1024)
  assert.equal(schema.$defs.ControlPlaneWebSocketTransportLimits.properties.ackDeadlineMillis.const, 30000)
  assert.equal(schema.$defs.ControlPlaneWebSocketTransportLimits.properties.backpressureCloseCode.const, 4408)
  assert.equal(schema.$defs.ControlPlaneWebSocketAuthorizationRevokedFrame.properties.closeCode.const, 4403)
  assert.equal(schema.$defs.ControlPlaneWebSocketResetRequiredFrame.properties.closeCode.const, 4409)

  for (const phrase of [
    '最后一个已确认 cursor',
    '每次续传前',
    '每一批重放前',
    '每次实时发送前',
    '暂停发送新的实时事件',
    'HTTP Query',
  ]) assert.equal(contract.includes(phrase), true, phrase)
})

test('positive transcripts cover every public event and reconnect control frame', () => {
  const coveredEvents = new Set()
  const coveredFrames = new Set()
  for (const transcript of validFixture.transcripts) {
    for (const frame of transcript.frames) {
      assert.deepEqual(validateSchemaNode(frame, schema), [], `${transcript.name}: ${frame.type}`)
    }
    assert.equal(validateTranscript(transcript), null, transcript.name)
    for (const frame of transcript.frames) {
      coveredFrames.add(frame.type)
      if (frame.type === 'event.v1') coveredEvents.add(frame.event.type)
    }
  }
  assert.deepEqual([...coveredEvents].sort(), [...eventTypes].sort())
  for (const frameType of [
    ...clientFrameTypes,
    'event.v1',
    'transport.authorization-revoked.v1',
    'transport.backpressure.v1',
    'transport.reset-required.v1',
    'transport.resume-accepted.v1',
    'transport.subscription-accepted.v1',
  ]) assert.equal(coveredFrames.has(frameType), true, frameType)
})

test('negative transcripts reject commands, missing metadata, and crossed streams', () => {
  const expectedCases = new Set([
    'business-command-over-websocket',
    'missing-event-id',
    'cross-tenant-stream',
    'cross-delivery-stream',
    'cross-product-session-stream',
    'cross-lease-stream',
    'non-monotonic-sequence',
    'ack-for-another-stream',
    'event-after-authorization-revocation',
  ])
  assert.deepEqual(
    new Set(invalidFixture.transcripts.map(transcript => transcript.name)),
    expectedCases,
  )
  for (const transcript of invalidFixture.transcripts) {
    const schemaFailures = transcript.frames.flatMap(frame => validateSchemaNode(frame, schema))
    assert.equal(
      schemaFailures.length > 0 || validateTranscript(transcript) !== null,
      true,
      transcript.name,
    )
  }
})
