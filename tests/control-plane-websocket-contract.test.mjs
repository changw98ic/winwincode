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
const domainSchemaPath = join(root, 'schema', 'winwincode', 'v1', 'domain.schema.json')
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
const domainSchema = JSON.parse(readFileSync(domainSchemaPath, 'utf8'))
const contract = readFileSync(contractPath, 'utf8')
const validFixture = JSON.parse(readFileSync(validFixturePath, 'utf8'))
const invalidFixture = JSON.parse(readFileSync(invalidFixturePath, 'utf8'))

const eventTypes = Object.freeze([
  'activity.recorded.v1',
  'approval.changed.v1',
  'attention.changed.v1',
  'chat-interactions.invalidated.v1',
  'delivery.changed.v1',
  'delivery-task.changed.v1',
  'enterprise-audit.invalidated.v1',
  'enterprise-fleet.invalidated.v1',
  'enterprise-integration.invalidated.v1',
  'enterprise-membership.invalidated.v1',
  'enterprise-team.invalidated.v1',
  'enterprise-role.invalidated.v1',
  'enterprise-organization.invalidated.v1',
  'enterprise-policy.invalidated.v1',
  'enterprise-project.invalidated.v1',
  'enterprise-usage.invalidated.v1',
  'presence.changed.v1',
  'product-session.message.appended.v1',
  'product-session.changed.v1',
  'runtime-projection.invalidated.v1',
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
  const branch = schema.$defs[name]
  if (branch.properties?.[propertyName]?.const !== undefined) {
    return branch.properties[propertyName].const
  }
  const values = branch.oneOf.map(nested => branchConst(nested, propertyName))
  assert.equal(new Set(values).size, 1, `${name} needs one nested ${propertyName}`)
  return values[0]
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

function validateSchemaNode(value, node, path = '$', document = schema) {
  if (node.$ref) {
    if (node.$ref.startsWith('./domain.schema.json')) {
      const name = node.$ref.split('/').at(-1)
      assert.ok(domainSchema.$defs[name], `missing domain schema definition: ${name}`)
      return validateSchemaNode(value, domainSchema.$defs[name], path, domainSchema)
    }
    assert.match(node.$ref, /^#\/\$defs\/[A-Za-z][A-Za-z0-9]+$/u)
    const name = node.$ref.split('/').at(-1)
    assert.ok(document.$defs[name], `missing local schema definition: ${name}`)
    return validateSchemaNode(value, document.$defs[name], path, document)
  }
  const errors = []
  if (node.oneOf) {
    const results = node.oneOf.map(branch => validateSchemaNode(value, branch, path, document))
    const matches = results.filter(errors => errors.length === 0)
    if (matches.length !== 1) errors.push(`${path}: expected exactly one matching union branch`)
  }
  if ('const' in node && value !== node.const) {
    return [`${path}: expected ${JSON.stringify(node.const)}`]
  }
  if (node.enum && !node.enum.includes(value)) return [`${path}: value is not in enum`]

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
    if (node.maxItems !== undefined && value.length > node.maxItems) {
      errors.push(`${path}: array is longer than maxItems`)
    }
    if (node.uniqueItems) {
      const encoded = value.map(item => JSON.stringify(item))
      if (new Set(encoded).size !== encoded.length) errors.push(`${path}: items are not unique`)
    }
    const prefixLength = node.prefixItems?.length ?? 0
    for (let index = 0; index < Math.min(prefixLength, value.length); index += 1) {
      errors.push(...validateSchemaNode(
        value[index],
        node.prefixItems[index],
        `${path}[${index}]`,
        document,
      ))
    }
    if (node.items === false && value.length > prefixLength) {
      errors.push(`${path}: array has items after the closed tuple prefix`)
    } else if (node.items && node.items !== true) {
      value.slice(prefixLength).forEach((item, offset) => {
        const index = prefixLength + offset
        errors.push(...validateSchemaNode(item, node.items, `${path}[${index}]`, document))
      })
    }
    return errors
  }
  if (node.type === 'object' || node.properties !== undefined) {
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
        errors.push(...validateSchemaNode(value[field], child, `${path}.${field}`, document))
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

function subscriptionBaseline(transcript, subscribe) {
  if (subscribe.type === 'transport.resume.v1') {
    const accepted = transcript.frames.find(frame => (
      frame.type === 'transport.resume-accepted.v1'
      && frame.subscriptionId === subscribe.subscriptionId
    ))
    if (accepted === undefined) return { error: 'resume acceptance is missing' }
    if (cursorKey(accepted.after) !== cursorKey(subscribe.after)) {
      return { error: 'resume acceptance crosses the requested stream' }
    }
    if (
      accepted.after.sequence !== subscribe.after.sequence
      || accepted.after.eventId !== subscribe.after.eventId
    ) return { error: 'resume acceptance changes the acknowledged cursor' }
    return {
      cursor: accepted.after,
      authorizationEpoch: accepted.authorizationEpoch,
    }
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
  return {
    cursor: accepted.cursor,
    authorizationEpoch: accepted.authorizationEpoch,
  }
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

function validateSessionIdentityJoin(frame) {
  if (
    frame.event.type !== 'runtime-projection.invalidated.v1'
    || frame.event.scopeKind !== 'delivery-stage'
  ) return null
  const identity = frame.event.sessionIdentity
  if (identity === undefined) return 'session event is missing sessionIdentity'
  for (const field of [
    'productSessionId',
    'stageRunId',
  ]) {
    if (identity[field] !== frame.event[field]) {
      return `session identity does not join event ${field}`
    }
  }
  if (frame.source.kind === 'execution-worker') {
    if (frame.source.sessionIdentity === undefined) {
      return 'session event worker source is missing sessionIdentity'
    }
    if (JSON.stringify(frame.source.sessionIdentity) !== JSON.stringify(identity)) {
      return 'session event source and payload identities differ'
    }
    for (const field of ['workerSessionId', 'codexThreadId']) {
      if (frame.source[field] !== identity[field]) {
        return `session event source does not join identity ${field}`
      }
    }
  }
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
  if (
    subscribe.type === 'transport.subscribe.v1'
    && typeof subscribe.startAt === 'object'
    && cursorKey(subscribe.startAt) !== cursorKey(expectedCursor)
  ) return 'snapshot cursor crosses the subscribed stream'
  const baseline = subscriptionBaseline(transcript, subscribe)
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
    if (frame.sequence !== lastSequence + 1) return 'event sequence is not continuous'
    if (frame.authorizationEpoch < authorizationEpochBaseline) {
      return 'event authorization epoch predates the accepted baseline'
    }
    const identityFailure = validateSessionIdentityJoin(frame)
    if (identityFailure) return identityFailure
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
    'https://schemas.winwincode.dev/winwincode/v1/control-plane-events.schema.json',
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
  assert.equal(domainSchema.$defs.EventReadPosition.minimum, 0)
  assert.equal(
    schema.$defs.ControlPlaneWebSocketAcknowledgedCursor.properties.sequence.$ref,
    '#/$defs/ControlPlaneWebSocketEventSequence',
  )
  assert.deepEqual(
    schema.$defs.ControlPlaneWebSocketEventPayload.oneOf.map(branch => branchConst(branch, 'type')).sort(),
    [...eventTypes].sort(),
  )
})

test('HTTP snapshots hand one exact event cursor to the first WebSocket subscription', () => {
  for (const name of [
    'ControlPlaneEventId',
    'EventReadPosition',
    'EventReadStream',
    'EventReadCursor',
  ]) {
    assert.ok(domainSchema.$defs[name], `${name} must have one shared domain definition`)
    assert.equal(schema.$defs[name], undefined, `${name} cannot be copied into the event schema`)
  }

  for (const name of [
    'ScopeEventReadCursor',
    'DeliveryEventReadCursor',
    'ProductSessionEventReadCursor',
    'LeaseEventReadCursor',
  ]) {
    assert.deepEqual(domainSchema.$defs[name].oneOf, [
      {
        properties: {
          sequence: { const: 0 },
          eventId: { type: 'null' },
        },
      },
      {
        properties: {
          sequence: { type: 'integer', minimum: 1, maximum: 9007199254740991 },
          eventId: { $ref: '#/$defs/ControlPlaneEventId' },
        },
      },
    ], name)
  }

  assert.deepEqual(schema.$defs.ControlPlaneWebSocketSubscribeFrame.properties.startAt, {
    $ref: '#/$defs/ControlPlaneWebSocketSubscribeStartAt',
  })
  assert.deepEqual(schema.$defs.ControlPlaneWebSocketSubscribeStartAt.oneOf, [
    { $ref: '#/$defs/ControlPlaneWebSocketSubscribeOrigin' },
    { $ref: './domain.schema.json#/$defs/EventReadCursor' },
  ])
  assert.deepEqual(schema.$defs.ControlPlaneWebSocketSubscribeOrigin.enum, [
    'latest',
    'earliest-available',
  ])

  assert.deepEqual(schema['x-winwincode-semantics'].snapshotHandoff, {
    httpCursorField: 'eventCursor',
    subscribeCursorField: 'startAt',
    cursorMustMatchSubscription: ['scope', 'stream'],
    acceptedBaselineFrame: 'transport.subscription-accepted.v1',
    firstEventSequence: 'accepted.cursor.sequence + 1',
    authorizationEpochBaseline: 'accepted.authorizationEpoch',
    retentionLossFrame: 'transport.reset-required.v1',
    retentionLossCloseCode: 4409,
  })
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

test('WebSocket authentication exists only on the HTTP upgrade', () => {
  assert.deepEqual(schema['x-winwincode-semantics'].authentication, {
    anonymousAllowed: false,
    upgradeOnly: true,
    sessionCookie: {
      location: 'cookie',
      name: 'wwc_session',
    },
    bearerAuth: {
      location: 'header',
      name: 'Authorization',
      scheme: 'Bearer',
      format: 'JWT',
    },
    credentialsInUrlQueryAllowed: false,
    credentialsInFramesAllowed: false,
  })
  assert.match(contract, /HTTP upgrade/u)
  assert.match(contract, /`wwc_session`/u)
  assert.match(contract, /`Authorization: Bearer <JWT>`/u)
  assert.match(contract, /URL query/u)
  assert.match(contract, /frame/u)
})

test('Chat event exposes only the canonical secret-safe message projection', () => {
  const event = schema.$defs.ControlPlaneWebSocketProductSessionMessageAppendedEvent

  assert.deepEqual(event.required, ['type', 'productSessionId', 'message'])
  assert.equal(event.properties.type.const, 'product-session.message.appended.v1')
  assert.equal(
    event.properties.message.$ref,
    './domain.schema.json#/$defs/ChatMessageProjection',
  )
  for (const forbidden of [
    'apiKey',
    'credential',
    'providerRequest',
    'providerResponse',
    'toolPayload',
  ]) assert.equal(event.properties[forbidden], undefined, forbidden)
})

test('resume, authorization recheck, and slow-client rules are machine visible', () => {
  assert.equal(schema.$defs.ControlPlaneWebSocketTransportLimits.properties.maxUnackedEvents.const, 256)
  assert.equal(schema.$defs.ControlPlaneWebSocketTransportLimits.properties.hardUnackedEvents.const, 1024)
  assert.equal(schema.$defs.ControlPlaneWebSocketTransportLimits.properties.ackDeadlineMillis.const, 30000)
  assert.equal(schema.$defs.ControlPlaneWebSocketTransportLimits.properties.backpressureCloseCode.const, 4408)
  assert.equal(schema.$defs.ControlPlaneWebSocketAuthorizationRevokedFrame.properties.closeCode.const, 4403)
  assert.equal(schema.$defs.ControlPlaneWebSocketResetRequiredFrame.properties.closeCode.const, 4409)
  assert.equal(
    schema.$defs.ControlPlaneWebSocketResetRequiredFrame.properties.reloadQueries,
    undefined,
  )
  assert.deepEqual(
    schema.$defs.ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent
      .properties.reloadQueries.prefixItems,
    [
      { $ref: '#/$defs/ControlPlaneWebSocketDeliveryGetReloadQuery' },
      { $ref: '#/$defs/ControlPlaneWebSocketRuntimeProjectionGetReloadQuery' },
    ],
  )
  assert.deepEqual(
    schema.$defs.ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent
      .properties.reloadQueries.prefixItems,
    [{ $ref: '#/$defs/ControlPlaneWebSocketRuntimeProjectionGetReloadQuery' }],
  )

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

test('enterprise invalidations bind each area to its one generated reload query', () => {
  const transcript = validFixture.transcripts.find(item => (
    item.name === 'enterprise-management-scope-invalidations'
  ))
  assert.ok(transcript)
  const organization = transcript.frames.find(frame => (
    frame.type === 'event.v1'
    && frame.event.type === 'enterprise-organization.invalidated.v1'
  ))
  assert.ok(organization)
  assert.deepEqual(validateSchemaNode(organization, schema), [])

  const crossed = structuredClone(organization)
  crossed.event.reloadQueries = ['enterprise.membership.list']
  assert.notDeepEqual(validateSchemaNode(crossed, schema), [])
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
    'delivery-runtime-invalidation-missing-stage',
    'product-runtime-invalidation-smuggles-delivery',
    'product-runtime-invalidation-uses-strongflow-pair',
    'reset-cannot-hard-code-delivery-reload-queries',
    'subscribe-cursor-crosses-stream',
    'subscription-accepted-cursor-crosses-stream',
    'first-event-skips-baseline-sequence',
    'event-authorization-epoch-before-baseline',
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

test('session-scoped frames reject unknown identity fields and source/payload joins', () => {
  const transcript = validFixture.transcripts.find(item => (
    item.name === 'product-session-resume'
  ))
  assert.ok(transcript)
  const frame = transcript.frames.find(item => (
    item.type === 'event.v1'
      && item.event.type === 'runtime-projection.invalidated.v1'
      && item.event.scopeKind === 'delivery-stage'
  ))
  assert.ok(frame)
  assert.deepEqual(validateSchemaNode(frame, schema), [])

  const unknown = structuredClone(frame)
  unknown.event.sessionIdentity.unexpected = true
  assert.notDeepEqual(validateSchemaNode(unknown, schema), [])

  const crossed = structuredClone(frame)
  crossed.event.sessionIdentity.stageRunId = 'run_01J00000000000000000000001'
  assert.equal(validateSessionIdentityJoin(crossed), 'session identity does not join event stageRunId')

  const sourceCrossed = structuredClone(frame)
  sourceCrossed.source.sessionIdentity.productSessionId = 'psn_01J00000000000000000000001'
  assert.equal(
    validateSessionIdentityJoin(sourceCrossed),
    'session event source and payload identities differ',
  )
})
