import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.chat-integration-tests.json',
    '--pretty',
    'false',
    '--incremental',
    'false',
  ],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `Chat Control Plane integration did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const facade = await import(`${pathToFileURL(resolve(
  root,
  '.cache/chat-integration-tests/control-plane-client.js',
)).href}?run=${String(Date.now())}`)
const chat = await import(`${pathToFileURL(resolve(
  root,
  '.cache/chat-integration-tests/chat-view-model.js',
)).href}?run=${String(Date.now())}`)

const { ControlPlaneClientError, createControlPlaneClient } = facade
const { createChatViewModel } = chat
const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const projectScope = {
  kind: 'project',
  organizationId: scope.organizationId,
  workspaceId: scope.workspaceId,
  projectId: scope.projectId,
}
const productSessionId = 'psn_00000000000000000000000001'
const otherProductSessionId = 'psn_00000000000000000000000002'
const subscriptionId = 'sub_00000000000000000000000001'
const approvalId = 'apr_00000000000000000000000001'
const inputRequestId = 'inp_00000000000000000000000001'
const executionJobId = 'job_00000000000000000000000001'
const workerSessionId = 'wsn_00000000000000000000000001'
const codexThreadId = 'cdx_00000000000000000000000001'

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function eventId(value) {
  return canonicalId('evt', value)
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function session(id, revision = 1, state = 'running') {
  return {
    id,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision,
    state,
    title: id === productSessionId ? 'Primary Chat' : 'Second Chat',
    updatedAt: `2026-08-27T01:00:0${String(revision)}.000Z`,
  }
}

function message(sequence, id = productSessionId) {
  return {
    id: canonicalId('msg', sequence),
    productSessionId: id,
    role: sequence % 2 === 0 ? 'assistant' : 'user',
    content: `message ${String(sequence)}`,
    sequence,
    state: 'completed',
    createdAt: '2026-08-27T01:00:00.000Z',
    updatedAt: '2026-08-27T01:00:00.000Z',
  }
}

function cursor(id, sequence = 0) {
  return {
    scope,
    stream: { kind: 'product-session', productSessionId: id },
    sequence,
    eventId: sequence === 0 ? null : eventId(sequence),
  }
}

function runtime(id, sequence = 0) {
  return {
    kind: 'runtime_projection',
    productSessionId: id,
    deliveryId: null,
    stageRunId: null,
    readCursor: null,
    eventCursor: cursor(id, sequence),
    lastProjectionSequence: sequence,
    revision: 1,
    rebuiltAt: '2026-08-27T01:00:00.000Z',
    sessions: [],
  }
}

function binding(id = productSessionId) {
  return {
    productSessionId: id,
    executionJobId,
    workerSessionId,
    sessionIdentity: {
      productSessionId: id,
      workerSessionId,
      codexThreadId,
    },
  }
}

function pendingInput(id = productSessionId) {
  return {
    kind: 'input',
    inputRequestId,
    revision: 1,
    state: 'pending',
    binding: binding(id),
    mode: 'text',
    prompt: 'Continue?',
    options: [],
    allowEmpty: false,
    expiresAt: '2099-08-27T01:10:00.000Z',
  }
}

function approval(state = 'pending', revision = 1, id = productSessionId) {
  return {
    id: approvalId,
    requestedAt: '2026-08-27T01:00:00.000Z',
    expiresAt: '2099-08-27T01:10:00.000Z',
    revision,
    state,
    subject: 'Allow one bounded tool call',
    category: 'shell',
    effectiveDecisionScope: 'once',
    sanitizedDetail: { kind: 'unavailable', reason: 'producer_unavailable' },
    binding: binding(id),
  }
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

function queryResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: page(),
  }
}

function commandResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    command: request.command,
    outcome: 'completed',
    previousRevision: request.expectedRevision,
    currentRevision: result.revision,
    result,
  }
}

function terminalError(request, code, messageText) {
  return {
    schemaVersion,
    requestId: request.requestId,
    error: {
      code,
      message: messageText,
      retryable: false,
      details: {},
    },
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

function eventFrame(sequence, event, id = productSessionId) {
  return {
    type: 'event.v1',
    subscriptionId,
    eventId: eventId(sequence),
    scope,
    stream: { kind: 'product-session', productSessionId: id },
    sequence,
    occurredAt: '2026-08-27T01:00:00.000Z',
    authorizationEpoch: 1,
    source: { kind: 'control-plane', component: 'chat-contract-fake', actor },
    event,
  }
}

function acceptedFrame(activeCursor) {
  return {
    type: 'transport.subscription-accepted.v1',
    subscriptionId,
    cursor: activeCursor,
    authorizationEpoch: 1,
    limits: transportLimits(),
  }
}

function resumedFrame(after) {
  return {
    type: 'transport.resume-accepted.v1',
    subscriptionId,
    after,
    replayThrough: after,
    authorizationEpoch: 1,
  }
}

class FakeWebSocket {
  readyState = 0
  onopen = null
  onmessage = null
  onclose = null
  onerror = null
  sent = []
  clientCloses = []

  send(source) {
    assert.equal(this.readyState, 1)
    this.sent.push(JSON.parse(source))
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

function socketFactory() {
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

async function flush() {
  await new Promise(resolvePromise => setTimeout(resolvePromise, 1))
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  await new Promise(resolvePromise => setImmediate(resolvePromise))
}

function contractFake() {
  const sockets = socketFactory()
  const requests = []
  const sessions = new Map([
    [productSessionId, session(productSessionId)],
    [otherProductSessionId, session(otherProductSessionId, 1, 'idle')],
  ])
  const approvals = new Map([[approvalId, approval()]])

  async function fetch(input, init) {
    const request = JSON.parse(init.body)
    requests.push({ input, init: structuredClone(init), request })
    if (input.endsWith('/api/v1/queries')) {
      if (request.query === 'session.list') {
        return response(200, queryResponse(request, {
          kind: 'product_session_page',
          items: [...sessions.values()],
        }))
      }
      if (request.query === 'session.get') {
        return response(200, queryResponse(
          request,
          sessions.get(request.parameters.productSessionId),
        ))
      }
      if (request.query === 'session.messages.list') {
        return response(200, queryResponse(request, {
          kind: 'chat_message_page',
          items: request.parameters.productSessionId === productSessionId
            ? [message(1)]
            : [],
        }))
      }
      if (request.query === 'model.route.availability.list') {
        return response(200, queryResponse(request, {
          kind: 'model_route_availability_page',
          scope,
          settingsSource: scope,
          settingsRevision: 1,
          requestPoolSource: projectScope,
          requestPoolRevision: 5,
          defaultProviderId: 'provider',
          defaultModelId: 'model',
          status: 'enabled',
          reason: 'ready',
          items: [{
            route: {
              providerId: 'provider',
              modelId: 'model',
              credentialReferenceId: 'crd_00000000000000000000000001',
            },
            providerDisplayName: 'Repository Provider',
            modelDisplayName: 'Repository Model',
            catalogSource: scope,
            catalogVersion: 1,
            providerVersion: 1,
            modelVersion: 1,
            contextWindowTokens: 128_000,
            maxOutputTokens: 16_000,
            toolSupport: 'parallel',
            reasoningEfforts: ['medium', 'high'],
            credentialRotationVersion: 1,
            isDefault: true,
            status: 'enabled',
            reason: 'ready',
          }],
        }))
      }
      if (request.query === 'runtime.projection.get') {
        return response(200, queryResponse(
          request,
          runtime(request.parameters.productSessionId),
        ))
      }
      if (request.query === 'session.interactions.list') {
        return response(200, queryResponse(request, {
          kind: 'chat_interaction_page',
          items: request.parameters.productSessionId === productSessionId
            ? [pendingInput(), { kind: 'approval', approval: approval() }]
            : [],
        }))
      }
      if (request.query === 'approval.list') {
        return response(200, queryResponse(request, {
          kind: 'approval_page',
          items: [...approvals.values()].filter(item => item.state === 'pending'),
        }))
      }
    }

    if (request.command === 'session.cancel') {
      const active = sessions.get(request.payload.productSessionId)
      if (active === undefined || active.revision !== request.expectedRevision) {
        return response(409, terminalError(request, 'REVISION_CONFLICT', 'Session changed.'))
      }
      const cancelled = {
        ...active,
        revision: active.revision + 1,
        state: 'cancelled',
        updatedAt: '2026-08-27T01:00:03.000Z',
      }
      sessions.set(cancelled.id, cancelled)
      return response(200, commandResponse(request, cancelled))
    }

    if (request.command === 'approval.decide') {
      const active = approvals.get(request.payload.approvalId)
      if (
        active === undefined
        || active.revision !== request.expectedRevision
        || JSON.stringify(request.payload.binding) !== JSON.stringify(active.binding)
      ) {
        return response(409, terminalError(request, 'REVISION_CONFLICT', 'Approval changed.'))
      }
      const decided = {
        ...active,
        revision: active.revision + 1,
        state: request.payload.decision === 'approve' ? 'approved' : 'rejected',
      }
      approvals.set(decided.id, decided)
      return response(200, commandResponse(request, decided))
    }

    if (request.command === 'input.respond') {
      const payload = request.payload
      const bindingIsExact = payload.productSessionId === productSessionId
        && payload.executionJobId === executionJobId
        && payload.inputRequestId === inputRequestId
        && payload.workerSessionId === workerSessionId
        && payload.sessionIdentity.productSessionId === productSessionId
        && payload.sessionIdentity.workerSessionId === workerSessionId
        && payload.sessionIdentity.codexThreadId === codexThreadId
      if (!bindingIsExact) {
        return response(409, terminalError(
          request,
          'WRONG_STATE',
          'Input binding does not match the active ProductSession.',
        ))
      }
      const active = sessions.get(productSessionId)
      const updated = {
        ...active,
        revision: active.revision + 1,
        state: 'running',
        updatedAt: '2026-08-27T01:00:02.000Z',
      }
      sessions.set(updated.id, updated)
      return response(200, commandResponse(request, updated))
    }

    return response(400, terminalError(request, 'INVALID_REQUEST', 'Unsupported fake request.'))
  }

  return { approvals, fetch, requests, sessions, sockets }
}

test('Chat contract fake keeps sessions isolated, deduplicates events, and resumes the acknowledged cursor', async () => {
  const fake = contractFake()
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/root',
    maxNetworkRetries: 0,
    reconnectDelayMillis: 0,
    transport: {
      fetch: fake.fetch,
      createSocket: fake.sockets.createSocket,
    },
  })
  let nextRequest = 0
  const model = createChatViewModel({
    client,
    actor,
    scope,
    productSessionId,
    subscriptionId,
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
  })

  await model.start()
  assert.deepEqual(model.state.sessions.map(item => item.id).sort(), [
    productSessionId,
    otherProductSessionId,
  ])
  assert.equal(fake.sockets.sockets.length, 3)
  assert.deepEqual(fake.sockets.urls, [
    'wss://control.example/root/api/v1/events',
    'wss://control.example/root/api/v1/events',
    'wss://control.example/root/api/v1/events',
  ])

  const availabilitySocket = fake.sockets.sockets[0]
  availabilitySocket.open()
  assert.equal(availabilitySocket.sent[0].subscription.stream.kind, 'scope')
  assert.deepEqual(
    availabilitySocket.sent[0].subscription.eventTypes,
    ['model-route-availability.invalidated.v1'],
  )
  const requestPoolSocket = fake.sockets.sockets[1]
  requestPoolSocket.open()
  assert.deepEqual(requestPoolSocket.sent[0].subscription.scope, projectScope)

  const firstSocket = fake.sockets.sockets[2]
  firstSocket.open()
  const subscribe = firstSocket.sent[0]
  assert.equal(subscribe.type, 'transport.subscribe.v1')
  assert.deepEqual(subscribe.startAt, cursor(productSessionId))
  firstSocket.receive(acceptedFrame(subscribe.startAt))
  const appended = eventFrame(1, {
    type: 'product-session.message.appended.v1',
    productSessionId,
    message: message(2),
  })
  firstSocket.receive(appended)
  await flush()
  assert.deepEqual(model.state.messages.map(item => item.sequence), [1, 2])
  assert.equal(firstSocket.sent.at(-1).type, 'transport.ack.v1')
  assert.equal(firstSocket.sent.at(-1).cursor.sequence, 1)

  firstSocket.receive(appended)
  await flush()
  assert.deepEqual(model.state.messages.map(item => item.sequence), [1, 2])

  firstSocket.serverClose(1006)
  await flush()
  assert.equal(fake.sockets.sockets.length, 4)
  const resumedSocket = fake.sockets.sockets[3]
  resumedSocket.open()
  assert.equal(resumedSocket.sent[0].type, 'transport.resume.v1')
  assert.deepEqual(resumedSocket.sent[0].after, cursor(productSessionId, 1))
  resumedSocket.receive(resumedFrame(resumedSocket.sent[0].after))

  resumedSocket.receive(eventFrame(2, {
    type: 'approval.changed.v1',
    approvalId,
    productSessionId,
    state: 'pending',
    subject: 'Allow one bounded tool call',
    requestedBy: actor,
    decidedBy: null,
  }))
  await flush()
  assert.deepEqual(model.state.pendingApprovals.map(item => item.id), [approvalId])
  assert.equal(resumedSocket.sent.at(-1).cursor.sequence, 2)

  const messageIdentityBefore = model.state.messages.map(item => item.id)
  const approvalReadsBefore = fake.requests.filter(
    ({ request }) => request.query === 'approval.list',
  ).length
  resumedSocket.receive(eventFrame(3, {
    type: 'approval.changed.v1',
    approvalId,
    productSessionId: otherProductSessionId,
    state: 'pending',
    subject: 'Must not enter another Chat',
    requestedBy: actor,
    decidedBy: null,
  }))
  await flush()
  assert.deepEqual(model.state.messages.map(item => item.id), messageIdentityBefore)
  assert.equal(fake.requests.filter(
    ({ request }) => request.query === 'approval.list',
  ).length, approvalReadsBefore)
  assert.notEqual(resumedSocket.sent.at(-1).cursor?.sequence, 3)
  assert.equal(model.state.realtime, 'reconnecting')

  await model.selectSession(otherProductSessionId)
  assert.equal(model.state.activeProductSessionId, otherProductSessionId)
  assert.equal(model.state.messages.length, 0)
  fake.sessions.set(otherProductSessionId, session(otherProductSessionId, 1, 'running'))
  await model.refresh()
  await model.cancelSession('Stop only the selected Chat.')
  const cancel = fake.requests.find(({ request }) => request.command === 'session.cancel').request
  assert.equal(cancel.payload.productSessionId, otherProductSessionId)
  assert.equal(cancel.expectedRevision, 1)
  assert.equal(model.state.session.id, otherProductSessionId)
  assert.equal(model.state.session.state, 'cancelled')

  model.close()
  client.close()
})

test('browser commands preserve complete input and approval bindings and fail closed on mismatch', async () => {
  const fake = contractFake()
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/root',
    maxNetworkRetries: 0,
    transport: { fetch: fake.fetch },
  })
  const sessionIdentity = {
    productSessionId,
    workerSessionId,
    codexThreadId,
  }
  const input = await client.command({
    schemaVersion,
    requestId: requestId(80),
    actor,
    scope,
    command: 'input.respond',
    expectedRevision: 1,
    payload: {
      executionJobId,
      inputRequestId,
      productSessionId,
      sessionIdentity,
      status: 'provided',
      value: { mode: 'text', value: 'Continue with the selected option.' },
      workerSessionId,
    },
  })
  assert.equal(input.command, 'input.respond')
  assert.equal(input.result.id, productSessionId)

  await assert.rejects(client.command({
    schemaVersion,
    requestId: requestId(81),
    actor,
    scope,
    command: 'input.respond',
    expectedRevision: 2,
    payload: {
      executionJobId,
      inputRequestId,
      productSessionId: otherProductSessionId,
      sessionIdentity,
      status: 'provided',
      value: { mode: 'text', value: 'Must not cross sessions.' },
      workerSessionId,
    },
  }), error => error instanceof ControlPlaneClientError
    && error.code === 'WRONG_STATE'
    && error.retryable === false)

  const decision = await client.command({
    schemaVersion,
    requestId: requestId(82),
    actor,
    scope,
    command: 'approval.decide',
    expectedRevision: 1,
    payload: {
      approvalId,
      binding: binding(),
      decision: 'approve',
      reason: 'Allow this bounded action once.',
    },
  })
  assert.equal(decision.command, 'approval.decide')
  assert.equal(decision.result.id, approvalId)
  assert.equal(decision.result.state, 'approved')

  const commandRequests = fake.requests.filter(({ request }) => 'command' in request)
  assert.deepEqual(commandRequests.map(({ request }) => request.command), [
    'input.respond',
    'input.respond',
    'approval.decide',
  ])
  assert.equal(commandRequests.every(({ input }) => input.endsWith('/api/v1/commands')), true)
  assert.equal(commandRequests.every(({ init }) => init.credentials === 'include'), true)
  assert.deepEqual(commandRequests[0].request.payload.sessionIdentity, sessionIdentity)

  client.close()
})
