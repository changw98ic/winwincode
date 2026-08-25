import assert from 'node:assert/strict'
import test from 'node:test'

import {
  ControlPlaneClientError,
  createControlPlaneHttpClient,
  createControlPlaneWebSocketClient,
  createProductSessionRuntimeProjectionSubscription,
  createStrongFlowProjectionSubscription,
} from '../apps/web/src/generated/control-plane-client.ts'

const schemaVersion = 'winwincode/v1'
const actor = { id: 'usr_00000000000000000000000001', kind: 'user' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const deliveryId = 'dlv_00000000000000000000000001'
const otherDeliveryId = 'dlv_00000000000000000000000002'
const productSessionId = 'psn_00000000000000000000000001'
const otherProductSessionId = 'psn_00000000000000000000000002'
const stageRunId = 'run_00000000000000000000000001'
const subscriptionId = 'sub_00000000000000000000000001'
const page = { hasMore: false, nextCursor: null }

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function eventId(value) {
  return canonicalId('evt', value)
}

function response(status, payload) {
  return {
    ok: status >= 200 && status < 300,
    status,
    async text() {
      return JSON.stringify(payload)
    },
  }
}

function completedCommandResponse(requestId, command, revision = 8) {
  return {
    schemaVersion,
    requestId,
    command,
    outcome: 'completed',
    previousRevision: revision - 1,
    currentRevision: revision,
    result: {
      id: productSessionId,
      revision,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
      title: 'Generated client session',
      state: 'cancelled',
      updatedAt: '2026-08-25T00:00:00.000Z',
    },
  }
}

function queryResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page,
  }
}

function readCursor(suffix = '1') {
  const sequence = Number(suffix)
  return {
    token: `sfread_${String(suffix).padStart(32, '0')}`,
    scope,
    deliveryId,
    deliveryRevision: Number(suffix),
    runtimeLedgerRevision: Number(suffix),
    runtimeAcceptedSequence: Number(suffix),
    publicationRevision: 0,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence,
      eventId: sequence === 0 ? null : eventId(sequence),
    },
  }
}

function deliveryProjection(cursor) {
  return {
    kind: 'delivery_detail',
    schemaVersion,
    deliveryId,
    deliveryRevision: cursor.deliveryRevision,
    readCursor: cursor,
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    status: 'executing',
    requirements: {
      deliverySpecId: 'spec:current',
      deliverySpecRevision: 1,
      title: 'Generated Web client verification',
      goal: 'Keep the browser projection consistent with Control Plane facts.',
      scope: ['Generated HTTP and WebSocket client'],
      outOfScope: [],
      constraints: [],
      acceptanceCriteria: [{
        id: 'criterion:client',
        description: 'The generated client validates and applies one canonical stream.',
        verificationMethod: 'Focused generated client tests',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: { kind: 'local-git', locator: 'workspace://repository' },
      baseRevision: '0123456789abcdef0123456789abcdef01234567',
      maxReworkAttempts: 2,
    },
    solutionReview: null,
    stages: [],
    tasks: [],
    attention: [],
    evidence: [],
    currentCandidate: null,
    verdict: null,
    publication: null,
  }
}

function deliveryRuntimeProjection(cursor) {
  return {
    kind: 'runtime_projection',
    productSessionId,
    deliveryId,
    stageRunId,
    revision: cursor.runtimeLedgerRevision,
    lastProjectionSequence: cursor.runtimeAcceptedSequence,
    readCursor: cursor,
    eventCursor: cursor.eventCursor,
    rebuiltAt: '2026-08-25T00:00:00.000Z',
    sessions: [],
  }
}

function productRuntimeProjection(revision = 1) {
  return {
    kind: 'runtime_projection',
    productSessionId,
    deliveryId: null,
    stageRunId: null,
    revision,
    lastProjectionSequence: revision,
    readCursor: null,
    eventCursor: {
      scope,
      stream: { kind: 'product-session', productSessionId },
      sequence: revision,
      eventId: revision === 0 ? null : eventId(revision),
    },
    rebuiltAt: '2026-08-25T00:00:00.000Z',
    sessions: [],
  }
}

function deferred() {
  let resolvePromise
  let rejectPromise
  const promise = new Promise((resolve, reject) => {
    resolvePromise = resolve
    rejectPromise = reject
  })
  return { promise, resolve: resolvePromise, reject: rejectPromise }
}

async function flush() {
  await new Promise(resolvePromise => setTimeout(resolvePromise, 1))
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  await new Promise(resolvePromise => setImmediate(resolvePromise))
}

class FakeWebSocket {
  readyState = 0
  onopen = null
  onmessage = null
  onclose = null
  onerror = null
  sent = []
  clientCloses = []

  send(payload) {
    assert.equal(this.readyState, 1)
    this.sent.push(JSON.parse(payload))
  }

  close(code, reason) {
    this.clientCloses.push({ code, reason })
    this.readyState = 3
  }

  open() {
    this.readyState = 1
    this.onopen?.({})
  }

  receive(frame) {
    this.onmessage?.({ data: JSON.stringify(frame) })
  }

  serverClose(code) {
    this.readyState = 3
    this.onclose?.({ code })
  }
}

function fakeWebSocketFactory() {
  const sockets = []
  const urls = []
  return {
    sockets,
    urls,
    createSocket(url) {
      urls.push(url)
      const socket = new FakeWebSocket()
      sockets.push(socket)
      return socket
    },
  }
}

function subscription(stream = { kind: 'delivery', deliveryId }) {
  return {
    scope,
    stream,
    eventTypes: ['runtime-projection.invalidated.v1'],
  }
}

function runtimeEvent(sequence, event, stream = { kind: 'delivery', deliveryId }) {
  return {
    type: 'event.v1',
    subscriptionId,
    eventId: eventId(sequence),
    scope,
    stream,
    sequence,
    occurredAt: '2026-08-25T00:00:00.000Z',
    authorizationEpoch: 1,
    source: {
      kind: 'control-plane',
      actor,
      component: 'test',
    },
    event,
  }
}

function deliveryChangedEvent(
  sequence,
  payloadDeliveryId = deliveryId,
  authorizationEpoch = 1,
) {
  return {
    ...runtimeEvent(sequence, {
      type: 'delivery.changed.v1',
      deliveryId: payloadDeliveryId,
      revision: sequence,
      changeKind: 'advanced',
    }),
    authorizationEpoch,
  }
}

function transportLimits() {
  return {
    maxUnackedEvents: 256,
    hardUnackedEvents: 1024,
    ackDeadlineMillis: 30_000,
    backpressureCloseCode: 4408,
  }
}

function acceptedFrame(
  activeScope = scope,
  stream = { kind: 'delivery', deliveryId },
  sequence = 0,
  authorizationEpoch = 1,
) {
  return {
    type: 'transport.subscription-accepted.v1',
    subscriptionId,
    cursor: {
      scope: activeScope,
      stream,
      sequence,
      eventId: sequence === 0 ? null : eventId(sequence),
    },
    authorizationEpoch,
    limits: transportLimits(),
  }
}

function acceptSubscription(socket, authorizationEpoch = 1) {
  const frame = socket.sent.find(candidate => candidate.type === 'transport.subscribe.v1')
  assert.ok(frame, 'the client must send a subscription before it can be accepted')
  const cursor = typeof frame.startAt === 'object'
    ? frame.startAt
    : {
        scope: frame.subscription.scope,
        stream: frame.subscription.stream,
        sequence: 0,
        eventId: null,
      }
  socket.receive({
    type: 'transport.subscription-accepted.v1',
    subscriptionId: frame.subscriptionId,
    cursor,
    authorizationEpoch,
    limits: transportLimits(),
  })
}

function resumeAcceptedFrame(
  after,
  replayThrough = after,
  authorizationEpoch = 1,
) {
  return {
    type: 'transport.resume-accepted.v1',
    subscriptionId,
    after,
    replayThrough,
    authorizationEpoch,
  }
}

test('HTTP command network retries resend the byte-identical request envelope', async () => {
  const requests = []
  let attempt = 0
  const client = createControlPlaneHttpClient({
    baseUrl: 'https://control.example/',
    maxNetworkRetries: 1,
    async waitBeforeRetry() {},
    async fetch(input, init) {
      requests.push({ input, init: structuredClone(init) })
      attempt += 1
      if (attempt === 1) throw new Error('socket carried a secret that must not escape')
      const request = JSON.parse(init.body)
      return response(200, completedCommandResponse(request.requestId, request.command))
    },
  })
  const command = {
    schemaVersion,
    requestId: requestId(1),
    actor,
    scope,
    command: 'session.cancel',
    expectedRevision: 7,
    payload: {
      productSessionId,
      reason: 'user requested cancellation',
    },
  }

  const result = await client.submitCommand(command)

  assert.equal(result.outcome, 'completed')
  assert.deepEqual(requests.map(entry => entry.input), [
    'https://control.example/api/v1/commands',
    'https://control.example/api/v1/commands',
  ])
  assert.equal(requests[0].init.body, requests[1].init.body)
  assert.deepEqual(JSON.parse(requests[1].init.body), command)
  assert.equal(JSON.parse(requests[1].init.body).requestId, requestId(1))
  assert.equal(JSON.parse(requests[1].init.body).expectedRevision, 7)
})

test('HTTP queries preserve opaque cursors and malformed errors stay bounded', async () => {
  const captured = []
  const client = createControlPlaneHttpClient({
    async fetch(input, init) {
      captured.push({ input, body: init.body })
      const request = JSON.parse(init.body)
      if (captured.length === 1) {
        return response(200, queryResponse(request, { kind: 'delivery_page', items: [] }))
      }
      return response(500, {
        requestId: request.requestId,
        schemaVersion,
        error: {
          code: 'INTERNAL_ERROR',
          message: 'provider payload leaked',
          retryable: false,
          details: {
            authorizationToken: 'do-not-copy',
          },
        },
      })
    },
  })
  const opaqueCursor = 'opaque_scope_query_filter_snapshot_01'
  const query = {
    schemaVersion,
    requestId: requestId(2),
    actor,
    scope,
    query: 'delivery.list',
    parameters: { states: [] },
    page: { cursor: opaqueCursor, limit: 25 },
  }

  await client.submitQuery(query)
  assert.equal(JSON.parse(captured[0].body).page.cursor, opaqueCursor)

  await assert.rejects(
    client.submitQuery({ ...query, requestId: requestId(3) }),
    error => {
      assert.ok(error instanceof ControlPlaneClientError)
      assert.deepEqual(Reflect.ownKeys(error).sort(), [
        'code',
        'details',
        'message',
        'requestId',
        'retryable',
      ])
      assert.equal(error.code, 'INVALID_RESPONSE')
      assert.equal(error.message, 'The Control Plane returned an invalid response.')
      assert.equal(error.requestId, requestId(3))
      assert.deepEqual(error.details, {})
      assert.doesNotMatch(JSON.stringify(error), /do-not-copy|provider payload leaked/u)
      return true
    },
  )
})

test('canonical HTTP errors expose only the five public safe fields', async () => {
  const client = createControlPlaneHttpClient({
    async fetch(_input, init) {
      const request = JSON.parse(init.body)
      return response(409, {
        schemaVersion,
        requestId: request.requestId,
        error: {
          code: 'REVISION_CONFLICT',
          message: 'Reload the current revision.',
          retryable: false,
          details: { currentRevision: 9 },
        },
      })
    },
  })

  await assert.rejects(
    client.submitCommand({
      schemaVersion,
      requestId: requestId(4),
      actor,
      scope,
      command: 'session.cancel',
      expectedRevision: 7,
      payload: { productSessionId, reason: 'cancel' },
    }),
    error => {
      assert.ok(error instanceof ControlPlaneClientError)
      assert.equal(error.code, 'REVISION_CONFLICT')
      assert.equal(error.message, 'Reload the current revision.')
      assert.equal(error.requestId, requestId(4))
      assert.equal(error.retryable, false)
      assert.deepEqual(error.details, { currentRevision: 9 })
      assert.equal('stack' in error, false)
      return true
    },
  )
})

test('WebSocket acknowledges only applied events, deduplicates, pongs, and resumes from that cursor', async () => {
  const factory = fakeWebSocketFactory()
  const secondEvent = deferred()
  const applied = []
  const client = createControlPlaneWebSocketClient({
    baseUrl: 'wss://control.example/',
    createSocket: factory.createSocket,
    reconnectDelayMillis: 0,
    async onEvent(frame) {
      applied.push(frame.eventId)
      if (frame.sequence === 2) await secondEvent.promise
    },
  })

  client.subscribe(subscriptionId, subscription())
  assert.equal(factory.urls[0], 'wss://control.example/api/v1/events')
  factory.sockets[0].open()
  assert.equal(factory.sockets[0].sent[0].type, 'transport.subscribe.v1')
  factory.sockets[0].receive(acceptedFrame())

  const eventOne = runtimeEvent(1, {
    type: 'runtime-projection.invalidated.v1',
    scopeKind: 'delivery-stage',
    productSessionId,
    deliveryId,
    stageRunId,
    projectionRevision: 1,
    lastProjectionSequence: 1,
    reloadQueries: ['delivery.get', 'runtime.projection.get'],
  })
  factory.sockets[0].receive(eventOne)
  await flush()
  assert.deepEqual(applied, [eventId(1)])
  assert.deepEqual(factory.sockets[0].sent.at(-1), {
    type: 'transport.ack.v1',
    subscriptionId,
    cursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 1,
      eventId: eventId(1),
    },
  })

  const eventTwo = runtimeEvent(2, {
    ...eventOne.event,
    projectionRevision: 2,
    lastProjectionSequence: 2,
  })
  factory.sockets[0].receive(eventTwo)
  await flush()
  assert.deepEqual(applied, [eventId(1), eventId(2)])
  assert.equal(factory.sockets[0].sent.filter(frame => frame.type === 'transport.ack.v1').length, 1)

  factory.sockets[0].serverClose(4408)
  await flush()
  assert.equal(factory.sockets.length, 2)
  factory.sockets[1].open()
  assert.deepEqual(factory.sockets[1].sent[0], {
    type: 'transport.resume.v1',
    subscriptionId,
    subscription: subscription(),
    after: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 1,
      eventId: eventId(1),
    },
  })
  factory.sockets[1].receive(resumeAcceptedFrame(factory.sockets[1].sent[0].after, {
    ...factory.sockets[1].sent[0].after,
    sequence: 2,
    eventId: eventId(2),
  }))

  secondEvent.resolve()
  await flush()
  assert.equal(factory.sockets[1].sent.at(-1).cursor.sequence, 2)
  factory.sockets[1].receive(eventTwo)
  await flush()
  assert.deepEqual(applied, [eventId(1), eventId(2)])
  assert.equal(factory.sockets[1].sent.at(-1).cursor.sequence, 2)

  factory.sockets[1].receive({
    type: 'transport.ping.v1',
    nonce: '0123456789abcdef',
    sentAt: '2026-08-25T00:00:00.000Z',
  })
  assert.deepEqual(factory.sockets[1].sent.at(-1), {
    type: 'transport.pong.v1',
    nonce: '0123456789abcdef',
  })
})

test('WebSocket handler failure sends no acknowledgement and 4403 stops reconnects', async () => {
  const factory = fakeWebSocketFactory()
  const errors = []
  const client = createControlPlaneWebSocketClient({
    createSocket: factory.createSocket,
    reconnectDelayMillis: 0,
    async onEvent() {
      throw new Error('page rejected the event')
    },
    onError(error) {
      errors.push(error)
    },
  })

  client.subscribe(subscriptionId, subscription())
  factory.sockets[0].open()
  factory.sockets[0].receive(acceptedFrame())
  factory.sockets[0].receive(runtimeEvent(1, {
    type: 'runtime-projection.invalidated.v1',
    scopeKind: 'delivery-stage',
    productSessionId,
    deliveryId,
    stageRunId,
    projectionRevision: 1,
    lastProjectionSequence: 1,
    reloadQueries: ['delivery.get', 'runtime.projection.get'],
  }))
  await flush()
  assert.equal(factory.sockets[0].sent.some(frame => frame.type === 'transport.ack.v1'), false)
  assert.equal(errors.length, 1)
  assert.equal(errors[0].code, 'EVENT_HANDLER_FAILED')

  client.reconnect()
  assert.equal(factory.sockets.length, 2)
  factory.sockets[1].open()
  factory.sockets[1].serverClose(4403)
  await flush()
  assert.equal(factory.sockets.length, 2)
  assert.equal(client.cursor, null)
  assert.throws(() => client.reconnect(), error => (
    error instanceof ControlPlaneClientError && error.code === 'NO_SUBSCRIPTION'
  ))
})

test('WebSocket retries a failed event from the last acknowledged cursor without skipping it', async () => {
  const factory = fakeWebSocketFactory()
  const errors = []
  const attempts = []
  let failFirstAttempt = true
  const baseline = {
    scope,
    stream: { kind: 'delivery', deliveryId },
    sequence: 1,
    eventId: eventId(1),
  }
  const retriedEvent = runtimeEvent(2, {
    type: 'runtime-projection.invalidated.v1',
    scopeKind: 'delivery-stage',
    productSessionId,
    deliveryId,
    stageRunId,
    projectionRevision: 2,
    lastProjectionSequence: 2,
    reloadQueries: ['delivery.get', 'runtime.projection.get'],
  })
  const client = createControlPlaneWebSocketClient({
    createSocket: factory.createSocket,
    reconnectDelayMillis: 0,
    async onEvent(frame) {
      attempts.push(frame.eventId)
      if (failFirstAttempt) {
        failFirstAttempt = false
        throw new Error('page rejected the first application attempt')
      }
    },
    onError(error) {
      errors.push(error)
    },
  })

  client.subscribe(subscriptionId, subscription(), baseline)
  factory.sockets[0].open()
  acceptSubscription(factory.sockets[0])
  factory.sockets[0].receive(retriedEvent)
  await flush()

  assert.deepEqual(attempts, [eventId(2)])
  assert.equal(client.cursor.sequence, 1)
  assert.equal(errors[0].code, 'EVENT_HANDLER_FAILED')

  client.reconnect()
  factory.sockets[1].open()
  assert.deepEqual(factory.sockets[1].sent[0], {
    type: 'transport.resume.v1',
    subscriptionId,
    subscription: subscription(),
    after: baseline,
  })
  factory.sockets[1].receive(resumeAcceptedFrame(baseline, {
    ...baseline,
    sequence: 2,
    eventId: eventId(2),
  }))
  factory.sockets[1].receive(retriedEvent)
  await flush()

  assert.deepEqual(attempts, [eventId(2), eventId(2)])
  assert.equal(errors.length, 1)
  assert.deepEqual(factory.sockets[1].sent.at(-1), {
    type: 'transport.ack.v1',
    subscriptionId,
    cursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 2,
      eventId: eventId(2),
    },
  })
})

test('generic reset frames reject the removed fixed reload query list', async () => {
  const factory = fakeWebSocketFactory()
  const errors = []
  let resetCount = 0
  const client = createControlPlaneWebSocketClient({
    createSocket: factory.createSocket,
    async onEvent() {},
    async onResetRequired() {
      resetCount += 1
    },
    onError(error) {
      errors.push(error)
    },
  })
  client.subscribe(subscriptionId, subscription())
  factory.sockets[0].open()
  factory.sockets[0].receive(acceptedFrame())
  factory.sockets[0].receive({
    type: 'transport.reset-required.v1',
    subscriptionId,
    reason: 'cursor-expired',
    earliestAvailable: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 0,
      eventId: null,
    },
    closeCode: 4409,
    reloadQueries: ['delivery.get', 'runtime.projection.get'],
  })
  await flush()

  assert.equal(resetCount, 0)
  assert.equal(errors.length, 1)
  assert.equal(errors[0].code, 'INVALID_WEBSOCKET_FRAME')
  assert.equal(factory.sockets.length, 1)
})

test('StrongFlow reloads delivery then runtime at the exact returned read cursor before publishing', async () => {
  const queries = []
  const snapshots = []
  const sockets = fakeWebSocketFactory()
  let revision = 0
  const httpClient = {
    async submitCommand() {
      throw new Error('not used')
    },
    async submitQuery(request) {
      queries.push(structuredClone(request))
      if (request.query === 'delivery.get') {
        revision += 1
        const cursor = readCursor(String(revision))
        return queryResponse(request, deliveryProjection(cursor))
      }
      const cursor = request.parameters.atCursor
      return queryResponse(request, deliveryRuntimeProjection(cursor))
    },
  }
  let requestSequence = 0
  const strongFlow = createStrongFlowProjectionSubscription({
    httpClient,
    baseUrl: 'wss://control.example',
    createSocket: sockets.createSocket,
    actor,
    scope,
    deliveryId,
    productSessionId,
    stageRunId,
    subscriptionId,
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return requestId(100 + requestSequence)
    },
    async onSnapshot(snapshot) {
      snapshots.push(snapshot)
    },
  })

  await strongFlow.start()
  assert.deepEqual(queries.map(query => query.query), [
    'delivery.get',
    'runtime.projection.get',
  ])
  assert.deepEqual(queries[1].parameters.atCursor, queries[0].parameters.atCursor ?? snapshots[0].cursor)
  assert.deepEqual(queries[1].parameters.atCursor, snapshots[0].delivery.readCursor)
  assert.deepEqual(strongFlow.cursor, snapshots[0].cursor)
  assert.equal(sockets.sockets.length, 1)

  sockets.sockets[0].open()
  acceptSubscription(sockets.sockets[0])
  sockets.sockets[0].receive(runtimeEvent(2, {
    type: 'runtime-projection.invalidated.v1',
    scopeKind: 'delivery-stage',
    productSessionId,
    deliveryId,
    stageRunId,
    projectionRevision: 2,
    lastProjectionSequence: 2,
    reloadQueries: ['delivery.get', 'runtime.projection.get'],
  }))
  await flush()
  assert.deepEqual(queries.slice(2).map(query => query.query), [
    'delivery.get',
    'runtime.projection.get',
  ])
  assert.deepEqual(queries[3].parameters.atCursor, snapshots[1].delivery.readCursor)
  assert.equal(snapshots.length, 2)
  assert.equal(sockets.sockets[0].sent.at(-1).type, 'transport.ack.v1')

  sockets.sockets[0].serverClose(4409)
  await flush()
  assert.deepEqual(queries.slice(4).map(query => query.query), [
    'delivery.get',
    'runtime.projection.get',
  ])
  assert.equal(snapshots.length, 3)
  assert.equal(sockets.sockets.length, 2)
})

test('StrongFlow discards a partial pair and does not subscribe until a full retry succeeds', async () => {
  const snapshots = []
  const sockets = fakeWebSocketFactory()
  let failRuntime = true
  let requestSequence = 0
  const cursor = readCursor('4')
  const subscriptionClient = createStrongFlowProjectionSubscription({
    httpClient: {
      async submitCommand() {
        throw new Error('not used')
      },
      async submitQuery(request) {
        if (request.query === 'delivery.get') {
          return queryResponse(request, deliveryProjection(cursor))
        }
        if (failRuntime) throw new Error('runtime read failed')
        return queryResponse(request, deliveryRuntimeProjection(cursor))
      },
    },
    createSocket: sockets.createSocket,
    actor,
    scope,
    deliveryId,
    productSessionId,
    stageRunId,
    subscriptionId,
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return requestId(200 + requestSequence)
    },
    async onSnapshot(snapshot) {
      snapshots.push(snapshot)
    },
  })

  await assert.rejects(subscriptionClient.start(), /runtime read failed/u)
  assert.equal(snapshots.length, 0)
  assert.equal(sockets.sockets.length, 0)

  failRuntime = false
  await subscriptionClient.start()
  assert.equal(snapshots.length, 1)
  assert.equal(sockets.sockets.length, 1)
})

test('expired StrongFlow read cuts restart the whole pair while invalid cursors stay terminal', async () => {
  const queries = []
  const snapshots = []
  const sockets = fakeWebSocketFactory()
  let requestSequence = 0
  let deliveryRead = 0
  let expireFirstRuntimeRead = true
  const client = createStrongFlowProjectionSubscription({
    httpClient: {
      async submitCommand() {
        throw new Error('not used')
      },
      async submitQuery(request) {
        queries.push(structuredClone(request))
        if (request.query === 'delivery.get') {
          deliveryRead += 1
          const cursor = readCursor(String(deliveryRead))
          return queryResponse(request, deliveryProjection(cursor))
        }
        if (expireFirstRuntimeRead) {
          expireFirstRuntimeRead = false
          throw new ControlPlaneClientError({
            code: 'READ_CURSOR_EXPIRED',
            message: 'The bounded read cut expired.',
            requestId: request.requestId,
            retryable: true,
            details: {},
          })
        }
        return queryResponse(request, deliveryRuntimeProjection(request.parameters.atCursor))
      },
    },
    createSocket: sockets.createSocket,
    actor,
    scope,
    deliveryId,
    productSessionId,
    stageRunId,
    subscriptionId,
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return requestId(300 + requestSequence)
    },
    async onSnapshot(snapshot) {
      snapshots.push(snapshot)
    },
  })

  await client.start()
  assert.deepEqual(queries.map(query => query.query), [
    'delivery.get',
    'runtime.projection.get',
    'delivery.get',
    'runtime.projection.get',
  ])
  assert.equal(Object.hasOwn(queries[0].parameters, 'atCursor'), false)
  assert.equal(Object.hasOwn(queries[2].parameters, 'atCursor'), false)
  assert.deepEqual(queries[1].parameters.atCursor, readCursor('1'))
  assert.deepEqual(queries[3].parameters.atCursor, readCursor('2'))
  assert.equal(snapshots.length, 1)
  assert.deepEqual(snapshots[0].cursor, readCursor('2'))
  assert.equal(sockets.sockets.length, 1)

  const terminalSockets = fakeWebSocketFactory()
  let terminalRequestSequence = 0
  const terminal = createStrongFlowProjectionSubscription({
    httpClient: {
      async submitCommand() {
        throw new Error('not used')
      },
      async submitQuery(request) {
        if (request.query === 'delivery.get') {
          return queryResponse(request, deliveryProjection(readCursor('7')))
        }
        throw new ControlPlaneClientError({
          code: 'INVALID_REQUEST',
          message: 'The cursor belongs to another scope.',
          requestId: request.requestId,
          retryable: false,
          details: {},
        })
      },
    },
    createSocket: terminalSockets.createSocket,
    actor,
    scope,
    deliveryId,
    productSessionId,
    stageRunId,
    subscriptionId,
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      terminalRequestSequence += 1
      return requestId(400 + terminalRequestSequence)
    },
    async onSnapshot() {
      assert.fail('invalid cursor must not publish')
    },
  })

  await assert.rejects(terminal.start(), error => (
    error instanceof ControlPlaneClientError && error.code === 'INVALID_REQUEST'
  ))
  assert.equal(terminalRequestSequence, 2)
  assert.equal(terminalSockets.sockets.length, 0)
})

test('product-session reset and invalidation reload runtime only and never invent Delivery identity', async () => {
  const queries = []
  const snapshots = []
  const sockets = fakeWebSocketFactory()
  let requestSequence = 0
  let revision = 0
  const product = createProductSessionRuntimeProjectionSubscription({
    httpClient: {
      async submitCommand() {
        throw new Error('not used')
      },
      async submitQuery(request) {
        queries.push(structuredClone(request))
        revision += 1
        return queryResponse(request, productRuntimeProjection(revision))
      },
    },
    createSocket: sockets.createSocket,
    actor,
    scope,
    productSessionId,
    subscriptionId,
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return requestId(500 + requestSequence)
    },
    async onSnapshot(snapshot) {
      snapshots.push(snapshot)
    },
  })

  await product.start()
  assert.equal(sockets.sockets.length, 1)
  sockets.sockets[0].open()
  acceptSubscription(sockets.sockets[0])
  sockets.sockets[0].serverClose(4409)
  await flush()
  assert.equal(sockets.sockets.length, 2)
  sockets.sockets[1].open()
  acceptSubscription(sockets.sockets[1])
  sockets.sockets[1].receive(runtimeEvent(3, {
    type: 'runtime-projection.invalidated.v1',
    scopeKind: 'product-session',
    productSessionId,
    projectionRevision: 3,
    lastProjectionSequence: 3,
    reloadQueries: ['runtime.projection.get'],
  }, { kind: 'product-session', productSessionId }))
  await flush()

  assert.equal(snapshots.length, 3)
  assert.ok(queries.every(query => query.query === 'runtime.projection.get'))
  assert.ok(queries.every(query => query.parameters.kind === 'product-session'))
  assert.ok(queries.every(query => !Object.hasOwn(query.parameters, 'deliveryId')))
  assert.ok(queries.every(query => !Object.hasOwn(query.parameters, 'stageRunId')))
  assert.ok(queries.every(query => !Object.hasOwn(query.parameters, 'atCursor')))
})

test('recursive error cleaning rejects every canonical sensitive key and prototype control key', async () => {
  const sensitiveDetails = [
    { apiKey: 'KEY-LEAK' },
    { nested: { password: 'PASSWORD-LEAK' } },
    { nested: [{ vaultLocator: 'VAULT-LEAK' }] },
    { toolPayload: 'TOOL-LEAK' },
  ]

  for (const [index, details] of sensitiveDetails.entries()) {
    const currentRequestId = requestId(600 + index)
    const client = createControlPlaneHttpClient({
      async fetch() {
        return response(500, {
          schemaVersion,
          requestId: currentRequestId,
          error: {
            code: 'INTERNAL_ERROR',
            message: 'The request failed.',
            retryable: false,
            details,
          },
        })
      },
    })
    await assert.rejects(
      client.submitQuery({
        schemaVersion,
        requestId: currentRequestId,
        actor,
        scope,
        query: 'delivery.list',
        parameters: { states: [] },
        page: { cursor: null, limit: 25 },
      }),
      error => {
        assert.ok(error instanceof ControlPlaneClientError)
        assert.equal(error.code, 'INVALID_RESPONSE')
        assert.doesNotMatch(JSON.stringify(error), /LEAK/u)
        return true
      },
    )
  }

  const prototypeRequestId = requestId(610)
  const prototypeClient = createControlPlaneHttpClient({
    async fetch() {
      return {
        ok: false,
        status: 500,
        async text() {
          return JSON.stringify({
            schemaVersion,
            requestId: prototypeRequestId,
            error: {
              code: 'INTERNAL_ERROR',
              message: 'The request failed.',
              retryable: false,
              details: JSON.parse('{"__proto__":{"polluted":"PROTOTYPE-LEAK"}}'),
            },
          })
        },
      }
    },
  })
  await assert.rejects(
    prototypeClient.submitQuery({
      schemaVersion,
      requestId: prototypeRequestId,
      actor,
      scope,
      query: 'delivery.list',
      parameters: { states: [] },
      page: { cursor: null, limit: 25 },
    }),
    error => {
      assert.ok(error instanceof ControlPlaneClientError)
      assert.equal(error.code, 'INVALID_RESPONSE')
      assert.equal(Reflect.get(error.details, 'polluted'), undefined)
      return true
    },
  )
})

test('WebSocket rejects sequence gaps instead of acknowledging past missing facts', async () => {
  const factory = fakeWebSocketFactory()
  const applied = []
  const errors = []
  const client = createControlPlaneWebSocketClient({
    createSocket: factory.createSocket,
    onEvent(frame) {
      applied.push(frame.sequence)
    },
    onError(error) {
      errors.push(error)
    },
  })
  client.subscribe(subscriptionId, {
    scope,
    stream: { kind: 'delivery', deliveryId },
    eventTypes: ['delivery.changed.v1'],
  })
  factory.sockets[0].open()
  factory.sockets[0].receive(acceptedFrame())
  factory.sockets[0].receive(deliveryChangedEvent(1))
  factory.sockets[0].receive(deliveryChangedEvent(3))
  await flush()

  assert.deepEqual(applied, [1])
  assert.deepEqual(
    factory.sockets[0].sent
      .filter(frame => frame.type === 'transport.ack.v1')
      .map(frame => frame.cursor.sequence),
    [1],
  )
  assert.equal(errors.at(-1)?.code, 'INVALID_WEBSOCKET_FRAME')
  assert.equal(factory.sockets[0].clientCloses.at(-1)?.code, 1011)
})

test('WebSocket validates every event payload against its exact stream resource', async t => {
  const otherWorkerId = canonicalId('wrk', 2)
  const workerId = canonicalId('wrk', 1)
  const cases = [
    {
      name: 'product session',
      stream: { kind: 'product-session', productSessionId },
      event: {
        type: 'product-session.changed.v1',
        productSessionId: otherProductSessionId,
        revision: 1,
        status: 'active',
      },
    },
    {
      name: 'chat message',
      stream: { kind: 'product-session', productSessionId },
      event: {
        type: 'product-session.message.appended.v1',
        productSessionId: otherProductSessionId,
        message: {
          id: canonicalId('msg', 1),
          productSessionId: otherProductSessionId,
          sequence: 1,
          role: 'assistant',
          state: 'completed',
          content: 'Applied the focused change.',
          createdAt: '2026-08-25T00:00:00.000Z',
          updatedAt: '2026-08-25T00:00:01.000Z',
        },
      },
    },
    {
      name: 'approval',
      stream: { kind: 'product-session', productSessionId },
      event: {
        type: 'approval.changed.v1',
        approvalId: canonicalId('apr', 1),
        productSessionId: otherProductSessionId,
        state: 'pending',
        subject: 'Approve the command.',
        requestedBy: actor,
        decidedBy: null,
      },
    },
    {
      name: 'attention',
      stream: { kind: 'delivery', deliveryId },
      event: {
        type: 'attention.changed.v1',
        attentionItemId: canonicalId('att', 1),
        deliveryId: otherDeliveryId,
        revision: 1,
        status: 'blocking',
        category: 'business',
        summary: 'Review the delivery.',
        assignedTo: null,
      },
    },
    {
      name: 'delivery',
      stream: { kind: 'delivery', deliveryId },
      event: deliveryChangedEvent(1, otherDeliveryId).event,
    },
    {
      name: 'delivery task',
      stream: { kind: 'delivery', deliveryId },
      event: {
        type: 'delivery-task.changed.v1',
        deliveryId: otherDeliveryId,
        deliveryTaskId: canonicalId('dtk', 1),
        revision: 1,
        changeKind: 'started',
      },
    },
    {
      name: 'runtime invalidation',
      stream: { kind: 'delivery', deliveryId },
      event: {
        type: 'runtime-projection.invalidated.v1',
        scopeKind: 'delivery-stage',
        productSessionId,
        deliveryId: otherDeliveryId,
        stageRunId,
        projectionRevision: 1,
        lastProjectionSequence: 1,
        reloadQueries: ['delivery.get', 'runtime.projection.get'],
      },
    },
    {
      name: 'presence',
      stream: { kind: 'product-session', productSessionId },
      event: {
        type: 'presence.changed.v1',
        userId: actor.id,
        productSessionId: otherProductSessionId,
        state: 'online',
        observedAt: '2026-08-25T00:00:00.000Z',
      },
    },
    {
      name: 'worker health',
      stream: { kind: 'lease', workerId, leaseId: canonicalId('lse', 1) },
      event: {
        type: 'worker-health.changed.v1',
        workerId: otherWorkerId,
        status: 'healthy',
        observedAt: '2026-08-25T00:00:00.000Z',
        activeLeaseCount: 0,
        availableCapacity: 1,
      },
    },
    {
      name: 'activity',
      stream: { kind: 'delivery', deliveryId },
      event: {
        type: 'activity.recorded.v1',
        actor,
        deliveryId: otherDeliveryId,
        category: 'product',
        summary: 'Delivery advanced.',
      },
    },
  ]

  for (const current of cases) {
    await t.test(current.name, async () => {
      const factory = fakeWebSocketFactory()
      const applied = []
      const errors = []
      const client = createControlPlaneWebSocketClient({
        createSocket: factory.createSocket,
        onEvent(frame) {
          applied.push(frame)
        },
        onError(error) {
          errors.push(error)
        },
      })
      client.subscribe(subscriptionId, {
        scope,
        stream: current.stream,
        eventTypes: [current.event.type],
      })
      factory.sockets[0].open()
      factory.sockets[0].receive(acceptedFrame(scope, current.stream))
      factory.sockets[0].receive(runtimeEvent(1, current.event, current.stream))
      await flush()
      assert.equal(applied.length, 0)
      assert.equal(errors.at(-1)?.code, 'INVALID_WEBSOCKET_FRAME')
      assert.equal(factory.sockets[0].clientCloses.at(-1)?.code, 1011)
    })
  }
})

test('WebSocket requires an accepted authorization epoch before applying events', async t => {
  async function run(currentName, accept, eventEpoch) {
    await t.test(currentName, async () => {
      const factory = fakeWebSocketFactory()
      const applied = []
      const errors = []
      const client = createControlPlaneWebSocketClient({
        createSocket: factory.createSocket,
        onEvent(frame) {
          applied.push(frame)
        },
        onError(error) {
          errors.push(error)
        },
      })
      client.subscribe(subscriptionId, subscription())
      factory.sockets[0].open()
      if (accept) factory.sockets[0].receive(acceptedFrame(scope, subscription().stream, 0, 7))
      factory.sockets[0].receive({
        ...runtimeEvent(1, {
          type: 'runtime-projection.invalidated.v1',
          scopeKind: 'delivery-stage',
          productSessionId,
          deliveryId,
          stageRunId,
          projectionRevision: 1,
          lastProjectionSequence: 1,
          reloadQueries: ['delivery.get', 'runtime.projection.get'],
        }),
        authorizationEpoch: eventEpoch,
      })
      await flush()
      assert.equal(applied.length, 0)
      assert.equal(errors.at(-1)?.code, 'INVALID_WEBSOCKET_FRAME')
    })
  }

  await run('event before acceptance', false, 1)
  await run('stale epoch after acceptance', true, 6)
})

test('switching subscriptions prevents an old async handler from advancing the new cursor', async () => {
  const factory = fakeWebSocketFactory()
  const pending = deferred()
  const client = createControlPlaneWebSocketClient({
    createSocket: factory.createSocket,
    async onEvent() {
      await pending.promise
    },
  })
  client.subscribe(subscriptionId, subscription())
  factory.sockets[0].open()
  factory.sockets[0].receive(acceptedFrame())
  factory.sockets[0].receive(runtimeEvent(1, {
    type: 'runtime-projection.invalidated.v1',
    scopeKind: 'delivery-stage',
    productSessionId,
    deliveryId,
    stageRunId,
    projectionRevision: 1,
    lastProjectionSequence: 1,
    reloadQueries: ['delivery.get', 'runtime.projection.get'],
  }))
  await flush()

  const nextSubscriptionId = 'sub_00000000000000000000000002'
  const nextStream = { kind: 'delivery', deliveryId: otherDeliveryId }
  client.subscribe(nextSubscriptionId, {
    scope,
    stream: nextStream,
    eventTypes: ['runtime-projection.invalidated.v1'],
  })
  factory.sockets[1].open()
  factory.sockets[1].receive({
    ...acceptedFrame(scope, nextStream),
    subscriptionId: nextSubscriptionId,
  })
  pending.resolve()
  await flush()

  assert.equal(
    factory.sockets[1].sent.some(frame => (
      frame.type === 'transport.ack.v1' && frame.subscriptionId === subscriptionId
    )),
    false,
  )
  assert.equal(client.cursor, null)
})

test('projection subscriptions clear snapshots and notify the caller on authorization revocation', async () => {
  const sockets = fakeWebSocketFactory()
  const cleared = []
  const revoked = []
  let requestSequence = 0
  const cursor = readCursor('20')
  const client = createStrongFlowProjectionSubscription({
    httpClient: {
      async submitCommand() {
        throw new Error('not used')
      },
      async submitQuery(request) {
        return request.query === 'delivery.get'
          ? queryResponse(request, deliveryProjection(cursor))
          : queryResponse(request, deliveryRuntimeProjection(cursor))
      },
    },
    createSocket: sockets.createSocket,
    actor,
    scope,
    deliveryId,
    productSessionId,
    stageRunId,
    subscriptionId,
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return requestId(700 + requestSequence)
    },
    async onSnapshot() {},
    async onSnapshotCleared(reason) {
      cleared.push(reason)
    },
    async onAuthorizationRevoked(frame) {
      revoked.push(frame)
    },
  })
  await client.start()
  sockets.sockets[0].open()
  acceptSubscription(sockets.sockets[0])
  assert.deepEqual(client.cursor, cursor)

  sockets.sockets[0].receive({
    type: 'transport.authorization-revoked.v1',
    subscriptionId,
    authorizationEpoch: 2,
    closeCode: 4403,
  })
  await flush()

  assert.equal(client.cursor, null)
  assert.deepEqual(cleared, ['authorization-revoked'])
  assert.equal(revoked.length, 1)
})

test('HTTP rejects a response result that belongs to another request discriminator', async () => {
  const queryClient = createControlPlaneHttpClient({
    async fetch(_input, init) {
      const request = JSON.parse(init.body)
      return response(200, queryResponse(request, { kind: 'worker_page', items: [] }))
    },
  })
  await assert.rejects(
    queryClient.submitQuery({
      schemaVersion,
      requestId: requestId(800),
      actor,
      scope,
      query: 'delivery.list',
      parameters: { states: [] },
      page: { cursor: null, limit: 25 },
    }),
    error => error instanceof ControlPlaneClientError && error.code === 'INVALID_RESPONSE',
  )

  const commandClient = createControlPlaneHttpClient({
    async fetch(_input, init) {
      const request = JSON.parse(init.body)
      const result = completedCommandResponse(request.requestId, request.command)
      return response(200, {
        ...result,
        result: { resourceKind: 'worker', revision: result.currentRevision },
      })
    },
  })
  await assert.rejects(
    commandClient.submitCommand({
      schemaVersion,
      requestId: requestId(801),
      actor,
      scope,
      command: 'session.cancel',
      expectedRevision: 7,
      payload: { productSessionId, reason: 'cancel' },
    }),
    error => error instanceof ControlPlaneClientError && error.code === 'INVALID_RESPONSE',
  )

  let fetchCalls = 0
  const requestClient = createControlPlaneHttpClient({
    async fetch() {
      fetchCalls += 1
      assert.fail('an invalid request branch must not reach the network')
    },
  })
  await assert.rejects(
    requestClient.submitQuery({
      schemaVersion,
      requestId: requestId(802),
      actor,
      scope,
      query: 'delivery.list',
      parameters: { states: [], workerId: canonicalId('wrk', 1) },
      page: { cursor: null, limit: 25 },
    }),
    error => error instanceof ControlPlaneClientError && error.code === 'INVALID_CLIENT_REQUEST',
  )
  assert.equal(fetchCalls, 0)
})

test('closing during an in-flight StrongFlow reload prevents later publication', async () => {
  const firstRead = deferred()
  const snapshots = []
  let requestSequence = 0
  const cursor = readCursor('30')
  const client = createStrongFlowProjectionSubscription({
    httpClient: {
      async submitCommand() {
        throw new Error('not used')
      },
      async submitQuery(request) {
        if (request.query === 'delivery.get') await firstRead.promise
        return request.query === 'delivery.get'
          ? queryResponse(request, deliveryProjection(cursor))
          : queryResponse(request, deliveryRuntimeProjection(cursor))
      },
    },
    createSocket() {
      assert.fail('a closed subscription must not open a WebSocket')
    },
    actor,
    scope,
    deliveryId,
    productSessionId,
    stageRunId,
    subscriptionId,
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return requestId(900 + requestSequence)
    },
    async onSnapshot(snapshot) {
      snapshots.push(snapshot)
    },
  })

  const start = client.start()
  client.close()
  firstRead.resolve()
  await start
  assert.equal(snapshots.length, 0)
  assert.equal(client.cursor, null)
})

test('WebSocket binds acceptance to the HTTP cursor and authorization baseline', async t => {
  const baseline = {
    scope,
    stream: { kind: 'delivery', deliveryId },
    sequence: 5,
    eventId: eventId(5),
  }

  await t.test('changed accepted cursor is rejected', async () => {
    const factory = fakeWebSocketFactory()
    const errors = []
    const client = createControlPlaneWebSocketClient({
      createSocket: factory.createSocket,
      async onEvent() {},
      onError(error) {
        errors.push(error)
      },
    })
    client.subscribe(subscriptionId, subscription(), baseline)
    factory.sockets[0].open()
    assert.deepEqual(factory.sockets[0].sent[0].startAt, baseline)
    factory.sockets[0].receive(acceptedFrame(
      scope,
      baseline.stream,
      baseline.sequence - 1,
      7,
    ))
    await flush()
    assert.equal(errors.at(-1)?.code, 'INVALID_WEBSOCKET_FRAME')
    assert.equal(factory.sockets[0].clientCloses.at(-1)?.code, 1011)
  })

  await t.test('first event follows the accepted cursor and may use a newer epoch', async () => {
    const factory = fakeWebSocketFactory()
    const applied = []
    const client = createControlPlaneWebSocketClient({
      createSocket: factory.createSocket,
      async onEvent(frame) {
        applied.push(frame.sequence)
      },
    })
    client.subscribe(subscriptionId, subscription(), baseline)
    factory.sockets[0].open()
    factory.sockets[0].receive({
      ...acceptedFrame(scope, baseline.stream, baseline.sequence, 7),
      cursor: baseline,
    })
    factory.sockets[0].receive({
      ...runtimeEvent(6, {
        type: 'runtime-projection.invalidated.v1',
        scopeKind: 'delivery-stage',
        productSessionId,
        deliveryId,
        stageRunId,
        projectionRevision: 6,
        lastProjectionSequence: 6,
        reloadQueries: ['delivery.get', 'runtime.projection.get'],
      }),
      authorizationEpoch: 8,
    })
    await flush()
    assert.deepEqual(applied, [6])
    assert.equal(client.cursor?.sequence, 6)
    assert.equal(factory.sockets[0].sent.at(-1).cursor.sequence, 6)
  })
})

test('projection wrappers reject extra cursors and incomplete DTOs before publication', async t => {
  await t.test('extra StrongFlow cursor field', async () => {
    const snapshots = []
    const sockets = fakeWebSocketFactory()
    let requestSequence = 0
    const cursor = { ...readCursor('40'), unexpected: 'not-canonical' }
    const client = createStrongFlowProjectionSubscription({
      httpClient: {
        async submitCommand() {
          throw new Error('not used')
        },
        async submitQuery(request) {
          return request.query === 'delivery.get'
            ? queryResponse(request, deliveryProjection(cursor))
            : queryResponse(request, deliveryRuntimeProjection(cursor))
        },
      },
      createSocket: sockets.createSocket,
      actor,
      scope,
      deliveryId,
      productSessionId,
      stageRunId,
      subscriptionId,
      eventTypes: ['runtime-projection.invalidated.v1'],
      createRequestId() {
        requestSequence += 1
        return requestId(1_000 + requestSequence)
      },
      async onSnapshot(snapshot) {
        snapshots.push(snapshot)
      },
    })
    await assert.rejects(client.start(), error => (
      error instanceof ControlPlaneClientError && error.code === 'INVALID_RESPONSE'
    ))
    assert.equal(snapshots.length, 0)
    assert.equal(sockets.sockets.length, 0)
  })

  await t.test('missing runtime projection field', async () => {
    const snapshots = []
    const sockets = fakeWebSocketFactory()
    let requestSequence = 0
    const { sessions: _sessions, ...incomplete } = productRuntimeProjection(1)
    const client = createProductSessionRuntimeProjectionSubscription({
      httpClient: {
        async submitCommand() {
          throw new Error('not used')
        },
        async submitQuery(request) {
          return queryResponse(request, incomplete)
        },
      },
      createSocket: sockets.createSocket,
      actor,
      scope,
      productSessionId,
      subscriptionId,
      eventTypes: ['runtime-projection.invalidated.v1'],
      createRequestId() {
        requestSequence += 1
        return requestId(1_100 + requestSequence)
      },
      async onSnapshot(snapshot) {
        snapshots.push(snapshot)
      },
    })
    await assert.rejects(client.start(), error => (
      error instanceof ControlPlaneClientError && error.code === 'INVALID_RESPONSE'
    ))
    assert.equal(snapshots.length, 0)
    assert.equal(sockets.sockets.length, 0)
  })
})

test('close and authorization or reset boundaries cancel old event completions', async t => {
  for (const boundary of ['close', 'authorization', 'reset']) {
    await t.test(boundary, async () => {
      const factory = fakeWebSocketFactory()
      const pending = deferred()
      const revoked = []
      const client = createControlPlaneWebSocketClient({
        createSocket: factory.createSocket,
        async onEvent() {
          await pending.promise
        },
        async onResetRequired() {
          return {
            scope,
            stream: { kind: 'delivery', deliveryId },
            sequence: 10,
            eventId: eventId(10),
          }
        },
        async onAuthorizationRevoked(frame) {
          revoked.push(frame)
        },
      })
      client.subscribe(subscriptionId, subscription())
      factory.sockets[0].open()
      factory.sockets[0].receive(acceptedFrame())
      factory.sockets[0].receive(runtimeEvent(1, {
        type: 'runtime-projection.invalidated.v1',
        scopeKind: 'delivery-stage',
        productSessionId,
        deliveryId,
        stageRunId,
        projectionRevision: 1,
        lastProjectionSequence: 1,
        reloadQueries: ['delivery.get', 'runtime.projection.get'],
      }))
      await flush()

      if (boundary === 'close') client.close()
      if (boundary === 'authorization') factory.sockets[0].serverClose(4403)
      if (boundary === 'reset') factory.sockets[0].serverClose(4409)
      pending.resolve()
      await flush()

      assert.equal(client.cursor, null)
      assert.equal(
        factory.sockets[0].sent.some(frame => frame.type === 'transport.ack.v1'),
        false,
      )
      assert.equal(revoked.length, boundary === 'authorization' ? 1 : 0)
    })
  }
})

test('StrongFlow reset discards stale reloads and publishes only the new cursor', async () => {
  const pendingOldRead = deferred()
  const snapshots = []
  const cleared = []
  const sockets = fakeWebSocketFactory()
  let deliveryRead = 0
  let requestSequence = 0
  const client = createStrongFlowProjectionSubscription({
    httpClient: {
      async submitCommand() {
        throw new Error('not used')
      },
      async submitQuery(request) {
        if (request.query === 'delivery.get') {
          deliveryRead += 1
          if (deliveryRead === 2) await pendingOldRead.promise
          return queryResponse(request, deliveryProjection(readCursor(String(deliveryRead))))
        }
        return queryResponse(request, deliveryRuntimeProjection(request.parameters.atCursor))
      },
    },
    createSocket: sockets.createSocket,
    actor,
    scope,
    deliveryId,
    productSessionId,
    stageRunId,
    subscriptionId,
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return requestId(1_200 + requestSequence)
    },
    async onSnapshot(snapshot) {
      snapshots.push(snapshot)
    },
    async onSnapshotCleared(reason) {
      cleared.push(reason)
    },
  })

  await client.start()
  sockets.sockets[0].open()
  acceptSubscription(sockets.sockets[0])
  sockets.sockets[0].receive(runtimeEvent(2, {
    type: 'runtime-projection.invalidated.v1',
    scopeKind: 'delivery-stage',
    productSessionId,
    deliveryId,
    stageRunId,
    projectionRevision: 2,
    lastProjectionSequence: 2,
    reloadQueries: ['delivery.get', 'runtime.projection.get'],
  }))
  await flush()
  assert.equal(deliveryRead, 2)

  sockets.sockets[0].serverClose(4409)
  assert.equal(client.cursor, null)
  pendingOldRead.resolve()
  await flush()
  await flush()

  assert.deepEqual(snapshots.map(snapshot => snapshot.cursor.deliveryRevision), [1, 3])
  assert.deepEqual(cleared, ['reset'])
  assert.equal(client.cursor?.deliveryRevision, 3)
  assert.equal(sockets.sockets.length, 2)
  sockets.sockets[1].open()
  assert.deepEqual(sockets.sockets[1].sent[0].startAt, readCursor('3').eventCursor)
})

test('failed StrongFlow reset keeps the cleared projection empty', async () => {
  const sockets = fakeWebSocketFactory()
  const errors = []
  let visibleSnapshot = null
  let failReload = false
  let requestSequence = 0
  const cursor = readCursor('50')
  const client = createStrongFlowProjectionSubscription({
    httpClient: {
      async submitCommand() {
        throw new Error('not used')
      },
      async submitQuery(request) {
        if (failReload) throw new Error('reset read failed')
        return request.query === 'delivery.get'
          ? queryResponse(request, deliveryProjection(cursor))
          : queryResponse(request, deliveryRuntimeProjection(cursor))
      },
    },
    createSocket: sockets.createSocket,
    actor,
    scope,
    deliveryId,
    productSessionId,
    stageRunId,
    subscriptionId,
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return requestId(1_300 + requestSequence)
    },
    async onSnapshot(snapshot) {
      visibleSnapshot = snapshot
    },
    async onSnapshotCleared() {
      visibleSnapshot = null
    },
    onError(error) {
      errors.push(error)
    },
  })

  await client.start()
  sockets.sockets[0].open()
  acceptSubscription(sockets.sockets[0])
  assert.notEqual(visibleSnapshot, null)
  failReload = true
  sockets.sockets[0].serverClose(4409)
  await flush()
  await flush()

  assert.equal(visibleSnapshot, null)
  assert.equal(client.cursor, null)
  assert.equal(sockets.sockets.length, 1)
  assert.equal(errors.at(-1)?.code, 'RESET_FAILED')
})
