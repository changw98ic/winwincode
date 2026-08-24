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
const actor = { id: 'user-1', kind: 'user' }
const scope = {
  kind: 'repository',
  organizationId: 'organization-1',
  workspaceId: 'workspace-1',
  projectId: 'project-1',
  repositoryId: 'repository-1',
}
const deliveryId = 'delivery-1'
const productSessionId = 'product-session-1'
const stageRunId = 'stage-run-1'
const page = { hasMore: false, nextCursor: null }

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
      kind: 'mutation_receipt',
      resourceId: productSessionId,
      revision,
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
  return {
    token: `read-cut-${suffix}`,
    scope,
    deliveryId,
    deliveryRevision: Number(suffix),
    runtimeLedgerRevision: Number(suffix),
    runtimeAcceptedSequence: Number(suffix),
    publicationRevision: 0,
  }
}

function deliveryProjection(cursor) {
  return {
    kind: 'delivery_detail',
    schemaVersion,
    deliveryId,
    deliveryRevision: cursor.deliveryRevision,
    readCursor: cursor,
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
    rebuiltAt: '2026-08-25T00:00:00Z',
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
    rebuiltAt: '2026-08-25T00:00:00Z',
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
    subscriptionId: 'subscription-1',
    eventId: `event-${String(sequence)}`,
    scope,
    stream,
    sequence,
    occurredAt: '2026-08-25T00:00:00Z',
    authorizationEpoch: 1,
    source: {
      kind: 'control-plane',
      actor,
      component: 'test',
    },
    event,
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
    requestId: 'request-command-1',
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
  assert.equal(JSON.parse(requests[1].init.body).requestId, 'request-command-1')
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
  const opaqueCursor = 'opaque::scope/query/filter/snapshot::do-not-parse'
  const query = {
    schemaVersion,
    requestId: 'request-query-1',
    actor,
    scope,
    query: 'delivery.list',
    parameters: { states: [] },
    page: { cursor: opaqueCursor, limit: 25 },
  }

  await client.submitQuery(query)
  assert.equal(JSON.parse(captured[0].body).page.cursor, opaqueCursor)

  await assert.rejects(
    client.submitQuery({ ...query, requestId: 'request-query-2' }),
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
      assert.equal(error.requestId, 'request-query-2')
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
      requestId: 'request-conflict-1',
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
      assert.equal(error.requestId, 'request-conflict-1')
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

  client.subscribe('subscription-1', subscription())
  assert.equal(factory.urls[0], 'wss://control.example/api/v1/events')
  factory.sockets[0].open()
  assert.equal(factory.sockets[0].sent[0].type, 'transport.subscribe.v1')

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
  assert.deepEqual(applied, ['event-1'])
  assert.deepEqual(factory.sockets[0].sent.at(-1), {
    type: 'transport.ack.v1',
    subscriptionId: 'subscription-1',
    cursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 1,
      eventId: 'event-1',
    },
  })

  const eventTwo = runtimeEvent(2, {
    ...eventOne.event,
    projectionRevision: 2,
    lastProjectionSequence: 2,
  })
  factory.sockets[0].receive(eventTwo)
  await flush()
  assert.deepEqual(applied, ['event-1', 'event-2'])
  assert.equal(factory.sockets[0].sent.filter(frame => frame.type === 'transport.ack.v1').length, 1)

  factory.sockets[0].serverClose(4408)
  await flush()
  assert.equal(factory.sockets.length, 2)
  factory.sockets[1].open()
  assert.deepEqual(factory.sockets[1].sent[0], {
    type: 'transport.resume.v1',
    subscriptionId: 'subscription-1',
    subscription: subscription(),
    after: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 1,
      eventId: 'event-1',
    },
  })

  secondEvent.resolve()
  await flush()
  assert.equal(factory.sockets[1].sent.at(-1).cursor.sequence, 2)
  factory.sockets[1].receive(eventTwo)
  await flush()
  assert.deepEqual(applied, ['event-1', 'event-2'])
  assert.equal(factory.sockets[1].sent.at(-1).cursor.sequence, 2)

  factory.sockets[1].receive({
    type: 'transport.ping.v1',
    nonce: '0123456789abcdef',
    sentAt: '2026-08-25T00:00:00Z',
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

  client.subscribe('subscription-1', subscription())
  factory.sockets[0].open()
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
  client.subscribe('subscription-1', subscription())
  factory.sockets[0].open()
  factory.sockets[0].receive({
    type: 'transport.reset-required.v1',
    subscriptionId: 'subscription-1',
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
    subscriptionId: 'subscription-1',
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return `request-strongflow-${String(requestSequence)}`
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
  sockets.sockets[0].receive(runtimeEvent(1, {
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
    subscriptionId: 'subscription-1',
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return `request-partial-${String(requestSequence)}`
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
    subscriptionId: 'subscription-1',
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return `request-expired-${String(requestSequence)}`
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
    subscriptionId: 'subscription-1',
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      terminalRequestSequence += 1
      return `request-terminal-${String(terminalRequestSequence)}`
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
    subscriptionId: 'subscription-1',
    eventTypes: ['runtime-projection.invalidated.v1'],
    createRequestId() {
      requestSequence += 1
      return `request-product-${String(requestSequence)}`
    },
    async onSnapshot(snapshot) {
      snapshots.push(snapshot)
    },
  })

  await product.start()
  assert.equal(sockets.sockets.length, 1)
  sockets.sockets[0].open()
  sockets.sockets[0].serverClose(4409)
  await flush()
  assert.equal(sockets.sockets.length, 2)
  sockets.sockets[1].open()
  sockets.sockets[1].receive(runtimeEvent(1, {
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
