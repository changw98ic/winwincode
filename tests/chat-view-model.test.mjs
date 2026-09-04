import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
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
    'apps/client/tsconfig.chat-tests.json',
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
  `Chat view-model did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const module = await import(`${pathToFileURL(resolve(
  root,
  '.cache/chat-view-model-tests/chat-view-model.js',
)).href}?run=${String(Date.now())}`)
const facade = await import(`${pathToFileURL(resolve(
  root,
  '.cache/chat-view-model-tests/control-plane-client.js',
)).href}`)

const { createChatViewModel } = module
const { ControlPlaneClientError } = facade
const schemaVersion = 'winwincode/v1'
const productSessionId = 'psn_00000000000000000000000001'
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
const subscriptionId = 'sub_00000000000000000000000001'
const credentialReferenceId = 'crd_00000000000000000000000001'
const modelRoute = {
  providerId: 'provider',
  modelId: 'model',
  credentialReferenceId,
}
const secondModelRoute = {
  providerId: 'provider-two',
  modelId: 'model-two',
  credentialReferenceId: 'crd_00000000000000000000000002',
}

function requestId(value) {
  return `req_${String(value).padStart(26, '0')}`
}

function page(nextCursor = null) {
  return { hasMore: nextCursor !== null, nextCursor }
}

function session(revision = 1, state = 'running') {
  return {
    id: productSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision,
    state,
    title: 'Chat session',
    updatedAt: '2026-08-27T01:00:00.000Z',
  }
}

function message(sequence, state = 'completed', content = `message ${String(sequence)}`) {
  return {
    id: `msg_${String(sequence).padStart(26, '0')}`,
    productSessionId,
    role: sequence % 2 === 0 ? 'assistant' : 'user',
    content,
    sequence,
    state,
    createdAt: '2026-08-27T01:00:00.000Z',
    updatedAt: '2026-08-27T01:00:00.000Z',
  }
}

function eventCursor(sequence = 0) {
  return {
    eventId: sequence === 0 ? null : `evt_${String(sequence).padStart(26, '0')}`,
    sequence,
    scope,
    stream: { kind: 'product-session', productSessionId },
  }
}

function runtime(revision = 1, sequence = 0) {
  return {
    kind: 'runtime_projection',
    productSessionId,
    deliveryId: null,
    stageRunId: null,
    readCursor: null,
    eventCursor: eventCursor(sequence),
    lastProjectionSequence: sequence,
    revision,
    rebuiltAt: '2026-08-27T01:00:00.000Z',
    sessions: [],
  }
}

function binding(sessionId = productSessionId) {
  return {
    productSessionId: sessionId,
    executionJobId: 'job_00000000000000000000000001',
    workerSessionId: 'wsn_00000000000000000000000001',
    sessionIdentity: {
      productSessionId: sessionId,
      workerSessionId: 'wsn_00000000000000000000000001',
      codexThreadId: 'cdx_00000000000000000000000001',
    },
  }
}

function input(value = 1, sessionId = productSessionId) {
  return {
    kind: 'input',
    inputRequestId: `inp_${String(value).padStart(26, '0')}`,
    revision: value,
    state: 'pending',
    binding: binding(sessionId),
    mode: 'single_choice',
    prompt: 'Select the target.',
    options: [{
      id: `ich_${String(value).padStart(26, '0')}`,
      value: 'candidate',
      label: 'Candidate workspace',
    }],
    allowEmpty: false,
    expiresAt: '2099-08-27T01:10:00.000Z',
  }
}

function approval(value = 1, sessionId = productSessionId) {
  return {
    id: `apr_${String(value).padStart(26, '0')}`,
    requestedAt: '2026-08-27T01:00:00.000Z',
    expiresAt: '2099-08-27T01:10:00.000Z',
    revision: value,
    state: 'pending',
    subject: 'Run the approved test command.',
    binding: binding(sessionId),
  }
}

function availableRoute(route = modelRoute, overrides = {}) {
  return {
    route,
    providerDisplayName: 'Repository Provider',
    modelDisplayName: 'Repository Model',
    catalogSource: scope,
    catalogVersion: 3,
    providerVersion: 2,
    modelVersion: 4,
    contextWindowTokens: 128_000,
    maxOutputTokens: 16_000,
    toolSupport: 'parallel',
    reasoningEfforts: ['medium', 'high'],
    credentialRotationVersion: 1,
    isDefault: true,
    status: 'enabled',
    reason: 'ready',
    ...overrides,
  }
}

function routeAvailability(items = [availableRoute()], overrides = {}) {
  return {
    kind: 'model_route_availability_page',
    scope,
    settingsSource: scope,
    settingsRevision: 3,
    requestPoolSource: projectScope,
    requestPoolRevision: 5,
    defaultProviderId: modelRoute.providerId,
    defaultModelId: modelRoute.modelId,
    status: 'enabled',
    reason: 'ready',
    items,
    ...overrides,
  }
}

function response(query, result, pageValue = page()) {
  return {
    schemaVersion,
    requestId: requestId(90),
    query,
    result,
    page: pageValue,
  }
}

function completed(command, result, revision = result.revision) {
  return {
    schemaVersion,
    requestId: requestId(91),
    command,
    outcome: 'completed',
    previousRevision: revision - 1,
    currentRevision: revision,
    result,
  }
}

function defaults() {
  return new Map([
    ['session.list', response(
      'session.list',
      { kind: 'product_session_page', items: [session()] },
    )],
    ['session.get', response('session.get', session())],
    ['session.messages.list', response(
      'session.messages.list',
      { kind: 'chat_message_page', items: [message(2), message(1)] },
    )],
    ['model.route.availability.list', response(
      'model.route.availability.list',
      routeAvailability(),
    )],
    ['runtime.projection.get', response('runtime.projection.get', runtime())],
    ['session.interactions.list', response(
      'session.interactions.list',
      {
        kind: 'chat_interaction_page',
        items: [input(), { kind: 'approval', approval: approval() }],
      },
    )],
    ['approval.list', response(
      'approval.list',
      { kind: 'approval_page', items: [approval()] },
    )],
  ])
}

class FakeClient {
  constructor() {
    this.responses = defaults()
  }

  calls = []
  commandCalls = []
  queues = new Map()
  commandQueues = new Map()
  subscription = null
  availabilitySubscriptions = []
  subscriptionClosed = false
  availabilitySubscriptionsClosed = 0
  reconnects = 0
  queryImplementation = null

  enqueue(query, value) {
    const queue = this.queues.get(query) ?? []
    queue.push(value)
    this.queues.set(query, queue)
  }

  enqueueCommand(command, value) {
    const queue = this.commandQueues.get(command) ?? []
    queue.push(value)
    this.commandQueues.set(command, queue)
  }

  async query(request, options) {
    this.calls.push(structuredClone(request))
    if (this.queryImplementation !== null) {
      return this.queryImplementation(request, options)
    }
    const queue = this.queues.get(request.query)
    const value = queue?.shift() ?? this.responses.get(request.query)
    if (value instanceof Error) throw value
    return structuredClone(value)
  }

  async command(request) {
    this.commandCalls.push(structuredClone(request))
    const queue = this.commandQueues.get(request.command)
    const value = queue?.shift()
    if (value === undefined) throw new Error(`No fake command response for ${request.command}`)
    if (value instanceof Error) throw value
    return structuredClone(value)
  }

  subscribe(options) {
    const availability = options.subscription.stream.kind === 'scope'
    if (availability) {
      this.availabilitySubscriptions.push(options)
    } else {
      this.subscription = options
      this.subscriptionClosed = false
    }
    return {
      cursor: null,
      resume() {},
      reconnect: () => { this.reconnects += 1 },
      close: () => {
        if (availability) this.availabilitySubscriptionsClosed += 1
        else this.subscriptionClosed = true
      },
    }
  }

  close() {}
}

function view(client = new FakeClient(), overrides = {}) {
  let next = 0
  return {
    client,
    model: createChatViewModel({
      client,
      actor,
      scope,
      productSessionId,
      subscriptionId,
      nextRequestId() {
        next += 1
        return requestId(next)
      },
      messagePageSize: 2,
      ...overrides,
    }),
  }
}

test('initial HTTP snapshot publishes one stable Chat state before subscribing at its cursor', async () => {
  const { client, model } = view()
  await model.start()

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'subscribed')
  assert.equal(model.state.session.state, 'running')
  assert.deepEqual(model.state.messages.map(item => item.sequence), [1, 2])
  assert.equal(model.state.modelRouteAvailability.items[0].status, 'enabled')
  assert.equal(model.state.modelRouteAvailability.items[0].reason, 'ready')
  assert.deepEqual(model.state.modelRouteAvailability.items[0].catalogSource, scope)
  assert.deepEqual(model.state.selectedModelRoute, modelRoute)
  assert.equal(model.state.runtime.productSessionId, productSessionId)
  assert.deepEqual(model.state.pendingInputs.map(item => item.inputRequestId), [input().inputRequestId])
  assert.deepEqual(model.state.pendingApprovals.map(item => item.id), [approval().id])
  assert.deepEqual(client.calls.map(call => call.query).sort(), [
    'approval.list',
    'model.route.availability.list',
    'runtime.projection.get',
    'session.get',
    'session.interactions.list',
    'session.list',
    'session.messages.list',
  ])
  assert.deepEqual(client.subscription.startAt, eventCursor())
  assert.deepEqual(client.subscription.subscription, {
    scope,
    stream: { kind: 'product-session', productSessionId },
    eventTypes: [
      'product-session.changed.v1',
      'product-session.message.appended.v1',
      'runtime-projection.invalidated.v1',
      'approval.changed.v1',
      'chat-interactions.invalidated.v1',
    ],
  })
  assert.deepEqual(
    client.availabilitySubscriptions.map(value => value.subscription),
    [scope, projectScope].map(value => ({
      scope: value,
      stream: { kind: 'scope' },
      eventTypes: ['model-route-availability.invalidated.v1'],
    })),
  )
})

test('message pagination and message events remain ordered and replace streaming projections', async () => {
  const { client, model } = view()
  client.responses.set('session.messages.list', response(
    'session.messages.list',
    { kind: 'chat_message_page', items: [message(2), message(1)] },
    page('cursor_0000000001'),
  ))
  await model.start()
  client.enqueue('session.messages.list', response(
    'session.messages.list',
    { kind: 'chat_message_page', items: [message(4), message(3)] },
  ))

  await model.loadMoreMessages()
  assert.deepEqual(model.state.messages.map(item => item.sequence), [1, 2, 3, 4])
  assert.equal(model.state.messagePagination.hasMore, false)

  await client.subscription.onEvent({
    event: {
      type: 'product-session.message.appended.v1',
      productSessionId,
      message: message(4, 'completed', 'final content'),
    },
  })
  assert.equal(model.state.messages.length, 4)
  assert.equal(model.state.messages.at(-1).content, 'final content')
  assert.equal(model.state.realtime, 'subscribed')
})

test('session, runtime, and approval events reload only their authoritative HTTP projection', async () => {
  const { client, model } = view()
  await model.start()
  client.enqueue('session.get', response('session.get', session(2, 'closed')))
  await client.subscription.onEvent({
    event: {
      type: 'product-session.changed.v1',
      productSessionId,
      revision: 2,
      status: 'completed',
    },
  })
  assert.equal(model.state.session.state, 'closed')

  client.enqueue(
    'runtime.projection.get',
    response('runtime.projection.get', runtime(2, 8)),
  )
  await client.subscription.onEvent({
    event: {
      type: 'runtime-projection.invalidated.v1',
      scopeKind: 'product-session',
      productSessionId,
      projectionRevision: 2,
      lastProjectionSequence: 8,
      reloadQueries: ['runtime.projection.get'],
    },
  })
  assert.equal(model.state.runtime.revision, 2)

  client.enqueue('approval.list', response(
    'approval.list',
    { kind: 'approval_page', items: [] },
  ))
  await client.subscription.onEvent({
    event: {
      type: 'approval.changed.v1',
      productSessionId,
      approvalId: approval().id,
      state: 'approved',
    },
  })
  assert.deepEqual(model.state.pendingApprovals, [])
})

test('interaction invalidation reloads bound input and approval snapshots', async () => {
  const { client, model } = view()
  await model.start()
  client.enqueue('session.interactions.list', response(
    'session.interactions.list',
    { kind: 'chat_interaction_page', items: [] },
  ))
  client.enqueue('approval.list', response(
    'approval.list',
    { kind: 'approval_page', items: [] },
  ))
  await client.subscription.onEvent({
    event: {
      type: 'chat-interactions.invalidated.v1',
      productSessionId,
      revision: 3,
      reloadQueries: ['session.interactions.list', 'approval.list'],
    },
  })
  assert.deepEqual(model.state.pendingInputs, [])
  assert.deepEqual(model.state.pendingApprovals, [])
})

test('input and approval commands copy the exact unforgeable projection binding', async () => {
  const { client, model } = view()
  await model.start()
  client.enqueueCommand('input.respond', completed('input.respond', session(2)))
  client.enqueue('session.interactions.list', response(
    'session.interactions.list',
    { kind: 'chat_interaction_page', items: [] },
  ))
  await model.respondToInput(input().inputRequestId, 'provided', {
    mode: 'single_choice',
    value: 'candidate',
  })
  assert.deepEqual(client.commandCalls[0], {
    schemaVersion,
    requestId: client.commandCalls[0].requestId,
    actor,
    scope,
    command: 'input.respond',
    expectedRevision: input().revision,
    payload: {
      executionJobId: binding().executionJobId,
      inputRequestId: input().inputRequestId,
      productSessionId,
      sessionIdentity: binding().sessionIdentity,
      status: 'provided',
      value: { mode: 'single_choice', value: 'candidate' },
      workerSessionId: binding().workerSessionId,
    },
  })

  client.enqueueCommand('approval.decide', completed('approval.decide', {
    ...approval(2),
    state: 'approved',
  }))
  client.enqueue('session.interactions.list', response(
    'session.interactions.list',
    { kind: 'chat_interaction_page', items: [] },
  ))
  client.enqueue('approval.list', response(
    'approval.list',
    { kind: 'approval_page', items: [] },
  ))
  await model.decideApproval(approval().id, 'approve', '  reviewed  ')
  assert.deepEqual(client.commandCalls[1], {
    schemaVersion,
    requestId: client.commandCalls[1].requestId,
    actor,
    scope,
    command: 'approval.decide',
    expectedRevision: approval().revision,
    payload: {
      approvalId: approval().id,
      binding: binding(),
      decision: 'approve',
      reason: 'reviewed',
    },
  })
})

test('message submit and stop commands use the current revision then publish server results', async () => {
  const { client, model } = view()
  await model.start()
  client.enqueueCommand('chat.submit', completed('chat.submit', session(2)))
  client.enqueue('session.messages.list', response(
    'session.messages.list',
    { kind: 'chat_message_page', items: [message(1), message(2), message(3)] },
  ))

  await model.submitMessage('  continue with the fix  ')
  assert.deepEqual(client.commandCalls[0], {
    schemaVersion,
    requestId: client.commandCalls[0].requestId,
    actor,
    scope,
    command: 'chat.submit',
    expectedRevision: 1,
    payload: {
      productSessionId,
      message: 'continue with the fix',
    },
  })
  assert.equal(model.state.session.revision, 2)
  assert.deepEqual(model.state.messages.map(item => item.sequence), [1, 2, 3])
  assert.equal(model.state.interaction.status, 'idle')

  client.enqueueCommand('session.cancel', completed(
    'session.cancel',
    session(3, 'cancelled'),
  ))
  await model.cancelSession('  user stopped this run  ')
  assert.equal(client.commandCalls[1].expectedRevision, 2)
  assert.equal(client.commandCalls[1].payload.reason, 'user stopped this run')
  assert.equal(model.state.session.state, 'cancelled')

  await model.submitMessage('   ')
  assert.equal(client.commandCalls.length, 2)
  assert.equal(model.state.interaction.error.code, 'CHAT_MESSAGE_REQUIRED')
})

test('selecting a listed session replaces the snapshot and realtime stream', async () => {
  const { client, model } = view()
  await model.start()
  const nextProductSessionId = 'psn_00000000000000000000000002'
  const nextSession = {
    ...session(4, 'idle'),
    id: nextProductSessionId,
    title: 'Second session',
  }
  const nextRuntime = structuredClone(runtime(4, 12))
  nextRuntime.productSessionId = nextProductSessionId
  nextRuntime.eventCursor.stream.productSessionId = nextProductSessionId
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [session(), nextSession] },
  ))
  client.responses.set('session.get', response('session.get', nextSession))
  client.responses.set('session.messages.list', response(
    'session.messages.list',
    { kind: 'chat_message_page', items: [] },
  ))
  client.responses.set('runtime.projection.get', response(
    'runtime.projection.get',
    nextRuntime,
  ))
  client.responses.set('session.interactions.list', response(
    'session.interactions.list',
    { kind: 'chat_interaction_page', items: [] },
  ))
  client.responses.set('approval.list', response(
    'approval.list',
    { kind: 'approval_page', items: [] },
  ))

  await model.selectSession(nextProductSessionId)
  assert.equal(model.state.activeProductSessionId, nextProductSessionId)
  assert.equal(model.state.session.title, 'Second session')
  assert.deepEqual(client.subscription.subscription.stream, {
    kind: 'product-session',
    productSessionId: nextProductSessionId,
  })
  assert.equal(client.subscriptionClosed, false)
})

test('creating a Chat uses the selected model then activates its server snapshot', async () => {
  const { client, model } = view()
  await model.start()
  const nextProductSessionId = 'psn_00000000000000000000000003'
  const createdSession = {
    ...session(1, 'idle'),
    id: nextProductSessionId,
    title: 'Created Chat',
  }
  const createdRuntime = structuredClone(runtime(1, 0))
  createdRuntime.productSessionId = nextProductSessionId
  createdRuntime.eventCursor.stream.productSessionId = nextProductSessionId
  const selectedModelRoute = structuredClone(model.state.selectedModelRoute)
  client.enqueueCommand('session.create', completed('session.create', createdSession))
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [session(), createdSession] },
  ))
  client.responses.set('session.get', response('session.get', createdSession))
  client.responses.set('session.messages.list', response(
    'session.messages.list',
    { kind: 'chat_message_page', items: [] },
  ))
  client.responses.set('runtime.projection.get', response(
    'runtime.projection.get',
    createdRuntime,
  ))
  client.responses.set('session.interactions.list', response(
    'session.interactions.list',
    { kind: 'chat_interaction_page', items: [] },
  ))
  client.responses.set('approval.list', response(
    'approval.list',
    { kind: 'approval_page', items: [] },
  ))

  await model.createSession({
    productSessionId: nextProductSessionId,
    title: '  Created Chat  ',
  })

  assert.deepEqual(client.commandCalls.at(-1), {
    schemaVersion,
    requestId: client.commandCalls.at(-1).requestId,
    actor,
    scope,
    command: 'session.create',
    expectedRevision: 0,
    payload: {
      productSessionId: nextProductSessionId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
      title: 'Created Chat',
      modelRoute: selectedModelRoute,
    },
  })
  assert.equal(model.state.activeProductSessionId, nextProductSessionId)
  assert.equal(model.state.session.id, nextProductSessionId)
  assert.equal(model.state.status, 'ready')
  assert.deepEqual(client.subscription.subscription.stream, {
    kind: 'product-session',
    productSessionId: nextProductSessionId,
  })
})

test('an empty Chat snapshot loads route availability and creates the first bound session', async () => {
  const client = new FakeClient()
  const activated = []
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [] },
  ))
  const { model } = view(client, {
    productSessionId: null,
    onActiveSessionChange: id => { activated.push(id) },
  })

  await model.start()

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'inactive')
  assert.equal(model.state.activeProductSessionId, null)
  assert.equal(model.state.session, null)
  assert.equal(model.state.modelRouteAvailability.items[0].status, 'enabled')
  assert.deepEqual(model.state.selectedModelRoute, modelRoute)
  assert.deepEqual(client.calls.map(call => call.query).sort(), [
    'model.route.availability.list',
    'session.list',
  ])
  assert.equal(client.subscription, null)
  assert.deepEqual(
    client.availabilitySubscriptions.map(value => value.subscription.scope),
    [scope, projectScope],
  )

  const createdSession = {
    ...session(1, 'idle'),
    title: 'First Chat',
  }
  client.enqueueCommand('session.create', completed('session.create', createdSession))
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [createdSession] },
  ))
  client.responses.set('session.get', response('session.get', createdSession))
  client.responses.set('session.messages.list', response(
    'session.messages.list',
    { kind: 'chat_message_page', items: [] },
  ))
  client.responses.set('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(1, 0),
  ))
  client.responses.set('session.interactions.list', response(
    'session.interactions.list',
    { kind: 'chat_interaction_page', items: [] },
  ))
  client.responses.set('approval.list', response(
    'approval.list',
    { kind: 'approval_page', items: [] },
  ))

  await model.createSession({
    productSessionId,
    title: 'First Chat',
  })

  assert.deepEqual(activated, [productSessionId])
  assert.equal(model.state.activeProductSessionId, productSessionId)
  assert.equal(model.state.session.title, 'First Chat')
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'subscribed')
})

test('first-session command failures preserve the empty shell and exact facade error', async () => {
  for (const [kind, code] of [
    ['server', 'IDEMPOTENCY_CONFLICT'],
    ['authorization', 'PERMISSION_DENIED'],
    ['server', 'SERVICE_UNAVAILABLE'],
  ]) {
    const client = new FakeClient()
    client.responses.set('session.list', response(
      'session.list',
      { kind: 'product_session_page', items: [] },
    ))
    const { model } = view(client, { productSessionId: null })
    await model.start()
    client.enqueueCommand('session.create', new ControlPlaneClientError({
      kind,
      code,
      message: 'private server diagnostic',
      requestId: requestId(80),
      retryable: code === 'SERVICE_UNAVAILABLE',
    }))

    await model.createSession({
      productSessionId,
      title: 'First Chat',
    })

    assert.equal(model.state.activeProductSessionId, null)
    assert.equal(model.state.session, null)
    assert.equal(model.state.interaction.status, 'error')
    assert.equal(model.state.interaction.error.code, code)
  }
})

test('refresh preserves a disabled selected route reason and creation fails closed', async () => {
  const client = new FakeClient()
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [] },
  ))
  const { model } = view(client, { productSessionId: null })
  await model.start()

  assert.equal(model.state.modelRouteAvailability.items[0].status, 'enabled')
  assert.deepEqual(model.state.selectedModelRoute, modelRoute)

  client.enqueue('session.list', client.responses.get('session.list'))
  client.enqueue('model.route.availability.list', response(
    'model.route.availability.list',
    routeAvailability([
      availableRoute(modelRoute, {
        status: 'disabled',
        reason: 'credential_missing_or_revoked',
        credentialRotationVersion: null,
      }),
    ], {
      status: 'disabled',
      reason: 'credential_missing_or_revoked',
    }),
  ))
  await model.refresh()

  assert.equal(model.state.modelRouteAvailability.items[0].reason, 'credential_missing_or_revoked')
  assert.equal(model.state.selectedModelRoute, null)
  assert.equal(model.state.modelRouteSelectionIssue, 'credential_missing_or_revoked')
  await model.createSession({ productSessionId, title: 'Must not start' })
  assert.equal(client.commandCalls.length, 0)
  assert.equal(model.state.interaction.error.code, 'CHAT_MODEL_ROUTE_UNAVAILABLE')

  client.enqueue('session.list', client.responses.get('session.list'))
  client.enqueue(
    'model.route.availability.list',
    client.responses.get('model.route.availability.list'),
  )
  await model.refresh()
  assert.equal(model.state.modelRouteAvailability.items[0].status, 'enabled')
  assert.equal(model.state.selectedModelRoute, null, 'refresh must not silently replace a lost route')
  assert.equal(model.state.modelRouteSelectionIssue, 'credential_missing_or_revoked')
  model.selectModelRoute(model.state.modelRouteAvailability.items[0].route)
  assert.deepEqual(model.state.selectedModelRoute, modelRoute)
  assert.equal(model.state.modelRouteSelectionIssue, null)
})

test('closed server route reasons never produce a selectable model route', async () => {
  for (const reason of [
    'credential_missing_or_revoked',
    'provider_or_model_disabled',
    'request_pool_unavailable',
  ]) {
    const client = new FakeClient()
    client.responses.set('session.list', response(
      'session.list',
      { kind: 'product_session_page', items: [] },
    ))
    client.responses.set('model.route.availability.list', response(
      'model.route.availability.list',
      routeAvailability([
        availableRoute(modelRoute, { status: 'disabled', reason }),
      ], { status: 'disabled', reason }),
    ))
    const { model } = view(client, { productSessionId: null })

    await model.start()

    assert.equal(model.state.modelRouteAvailability.items[0].reason, reason)
    assert.equal(model.state.selectedModelRoute, null)
  }
})

test('availability invalidation reloads only routes and never switches to another ready route', async () => {
  const client = new FakeClient()
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [] },
  ))
  client.responses.set('model.route.availability.list', response(
    'model.route.availability.list',
    routeAvailability([
      availableRoute(),
      availableRoute(secondModelRoute, {
        providerDisplayName: 'Second Provider',
        modelDisplayName: 'Second Model',
        isDefault: false,
      }),
    ]),
  ))
  const { model } = view(client, { productSessionId: null })
  const observedStatuses = []
  model.subscribe(state => observedStatuses.push(state.status))
  await model.start()
  assert.deepEqual(model.state.selectedModelRoute, modelRoute)

  client.enqueue('model.route.availability.list', response(
    'model.route.availability.list',
    routeAvailability([
      availableRoute(modelRoute, {
        status: 'disabled',
        reason: 'request_pool_unavailable',
      }),
      availableRoute(secondModelRoute, {
        providerDisplayName: 'Second Provider',
        modelDisplayName: 'Second Model',
        isDefault: false,
      }),
    ], { requestPoolRevision: 6 }),
  ))
  const callsBefore = client.calls.length
  const poolSubscription = client.availabilitySubscriptions.findLast(value => (
    value.subscription.scope.kind === 'project'
  ))
  await poolSubscription.onEvent({
    event: {
      type: 'model-route-availability.invalidated.v1',
      source: 'request_pool',
      sourceRevision: 6,
      reloadQueries: ['model.route.availability.list'],
    },
  })

  assert.deepEqual(
    client.calls.slice(callsBefore).map(call => call.query),
    ['model.route.availability.list'],
  )
  assert.equal(model.state.selectedModelRoute, null)
  assert.equal(model.state.modelRouteSelectionIssue, 'request_pool_unavailable')
  assert.equal(model.state.modelRouteAvailability.items[1].status, 'enabled')
  assert.equal(observedStatuses.includes('refreshing'), true)
})

test('all four authoritative invalidations reload the sole availability query once', async () => {
  const client = new FakeClient()
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [] },
  ))
  const { model } = view(client, { productSessionId: null })
  await model.start()

  for (const source of [
    'settings',
    'provider_catalog',
    'credential_reference',
    'request_pool',
  ]) {
    client.enqueue('model.route.availability.list', source === 'request_pool'
      ? response(
          'model.route.availability.list',
          routeAvailability(undefined, { requestPoolRevision: 6 }),
        )
      : client.responses.get('model.route.availability.list'))
    const subscription = client.availabilitySubscriptions.findLast(value => (
      value.subscription.scope.kind === (source === 'request_pool' ? 'project' : 'repository')
    ))
    const callsBefore = client.calls.length
    await subscription.onEvent({
      event: {
        type: 'model-route-availability.invalidated.v1',
        source,
        sourceRevision: source === 'request_pool' ? 6 : 4,
        reloadQueries: ['model.route.availability.list'],
      },
    })
    assert.deepEqual(
      client.calls.slice(callsBefore).map(call => call.query),
      ['model.route.availability.list'],
    )
  }
})

test('request-pool subscriptions accept another Repository in the returned Project only', async () => {
  const otherRepositoryScope = {
    ...scope,
    repositoryId: 'rep_00000000000000000000000002',
  }
  const client = new FakeClient()
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [] },
  ))
  client.responses.set('model.route.availability.list', response(
    'model.route.availability.list',
    routeAvailability([
      availableRoute(modelRoute, {
        catalogSource: otherRepositoryScope,
      }),
    ], {
      scope: otherRepositoryScope,
      settingsSource: otherRepositoryScope,
      requestPoolSource: projectScope,
    }),
  ))
  const { model } = view(client, {
    productSessionId: null,
    scope: otherRepositoryScope,
  })

  await model.start()

  assert.equal(model.state.status, 'ready')
  assert.deepEqual(
    client.availabilitySubscriptions.map(value => value.subscription.scope),
    [otherRepositoryScope, projectScope],
  )
})

test('a request-pool source from another Project fails closed before subscribing', async () => {
  const client = new FakeClient()
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [] },
  ))
  client.responses.set('model.route.availability.list', response(
    'model.route.availability.list',
    routeAvailability(undefined, {
      requestPoolSource: {
        ...projectScope,
        projectId: 'prj_00000000000000000000000099',
      },
    }),
  ))
  const { model } = view(client, { productSessionId: null })

  await model.start()

  assert.equal(model.state.status, 'error')
  assert.equal(model.state.error.code, 'CHAT_MODEL_ROUTE_REQUEST_POOL_SCOPE_MISMATCH')
  assert.equal(model.state.modelRouteAvailability, null)
  assert.deepEqual(client.availabilitySubscriptions, [])
})

test('route availability pagination keeps one stable server snapshot and all candidates', async () => {
  const client = new FakeClient()
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [] },
  ))
  client.enqueue('model.route.availability.list', response(
    'model.route.availability.list',
    routeAvailability([availableRoute()]),
    page('cursor_routes_2'),
  ))
  client.enqueue('model.route.availability.list', response(
    'model.route.availability.list',
    routeAvailability([
      availableRoute(secondModelRoute, {
        providerDisplayName: 'Second Provider',
        modelDisplayName: 'Second Model',
        isDefault: false,
      }),
    ]),
  ))
  const { model } = view(client, { productSessionId: null })

  await model.start()

  assert.deepEqual(
    model.state.modelRouteAvailability.items.map(item => item.route.modelId),
    ['model', 'model-two'],
  )
  assert.deepEqual(
    client.calls
      .filter(call => call.query === 'model.route.availability.list')
      .map(call => call.page.cursor),
    [null, 'cursor_routes_2'],
  )
})

test('route availability pagination rejects a changed request-pool authority cut', async () => {
  const client = new FakeClient()
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [] },
  ))
  client.enqueue('model.route.availability.list', response(
    'model.route.availability.list',
    routeAvailability([availableRoute()]),
    page('cursor_routes_2'),
  ))
  client.enqueue('model.route.availability.list', response(
    'model.route.availability.list',
    routeAvailability([availableRoute(secondModelRoute)], {
      requestPoolRevision: 6,
    }),
  ))
  const { model } = view(client, { productSessionId: null })

  await model.start()

  assert.equal(model.state.status, 'error')
  assert.equal(model.state.error.code, 'CHAT_MODEL_ROUTE_SNAPSHOT_MISMATCH')
  assert.equal(model.state.modelRouteAvailability, null)
})

test('route availability permission failure clears model facts and denies creation', async () => {
  const client = new FakeClient()
  client.responses.set('session.list', response(
    'session.list',
    { kind: 'product_session_page', items: [] },
  ))
  client.responses.set('model.route.availability.list', new ControlPlaneClientError({
    kind: 'authorization',
    code: 'PERMISSION_DENIED',
    message: 'private credential diagnostic',
    requestId: requestId(81),
    retryable: false,
  }))
  const { model } = view(client, { productSessionId: null })

  await model.start()

  assert.equal(model.state.status, 'authorization-denied')
  assert.equal(model.state.modelRouteAvailability, null)
  assert.equal(model.state.selectedModelRoute, null)
  await model.createSession({ productSessionId, title: 'Denied Chat' })
  assert.equal(client.commandCalls.length, 0)
})

test('reset clears the old view, reloads all snapshots, and hands back the new HTTP cursor', async () => {
  const { client, model } = view()
  const observed = []
  model.subscribe(state => observed.push(state))
  await model.start()
  client.enqueue('session.get', response('session.get', session(3, 'waiting_for_input')))
  client.enqueue('session.messages.list', response(
    'session.messages.list',
    { kind: 'chat_message_page', items: [message(5)] },
  ))
  client.enqueue(
    'model.route.availability.list',
    client.responses.get('model.route.availability.list'),
  )
  client.enqueue('runtime.projection.get', response('runtime.projection.get', runtime(3, 9)))
  client.enqueue('approval.list', response(
    'approval.list',
    { kind: 'approval_page', items: [] },
  ))

  const cursor = await client.subscription.onResetRequired()
  assert.deepEqual(cursor, eventCursor(9))
  assert.equal(observed.some(state => (
    state.status === 'refreshing'
    && state.realtime === 'reloading'
    && state.session === null
    && state.messages.length === 0
  )), true)
  assert.equal(model.state.session.revision, 3)
  assert.deepEqual(model.state.messages.map(item => item.sequence), [5])
})

test('authorization revocation clears protected snapshots and network errors expose reconnect state', async () => {
  const { client, model } = view()
  await model.start()
  const network = new ControlPlaneClientError({
    kind: 'network',
    code: 'NETWORK_ERROR',
    message: 'Disconnected.',
    requestId: null,
    retryable: true,
  })
  client.subscription.onError(network)
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'reconnecting')
  assert.equal(model.state.error, network)
  model.reconnect()
  assert.equal(client.reconnects, 3)

  await client.subscription.onAuthorizationRevoked()
  assert.equal(model.state.status, 'authentication-required')
  assert.equal(model.state.realtime, 'access-revoked')
  assert.equal(model.state.session, null)
  assert.deepEqual(model.state.messages, [])
  assert.equal(client.subscriptionClosed, true)
  assert.equal(client.availabilitySubscriptionsClosed, 2)
})

test('cancelling an in-flight snapshot prevents stale publication', async () => {
  const { client, model } = view()
  client.queryImplementation = (_request, options) => new Promise((_resolve, reject) => {
    options.signal.addEventListener('abort', () => reject(new Error('aborted')), { once: true })
  })
  const start = model.start()
  model.cancelPending()
  await start
  assert.equal(model.state.status, 'cancelled')
  assert.equal(model.state.realtime, 'inactive')
  assert.equal(model.state.session, null)
})

test('cross-session or partial snapshots fail without publishing mixed Chat state', async () => {
  const { client, model } = view()
  client.responses.set('session.get', response('session.get', {
    ...session(),
    id: 'psn_00000000000000000000000099',
  }))
  await model.start()
  assert.equal(model.state.status, 'error')
  assert.equal(model.state.error.code, 'CHAT_SESSION_SCOPE_MISMATCH')
  assert.equal(model.state.session, null)
  assert.deepEqual(model.state.messages, [])
  assert.equal(client.subscription, null)
})

test('cross-session pending interaction binding fails closed before a command can run', async () => {
  const { client, model } = view()
  client.responses.set('session.interactions.list', response(
    'session.interactions.list',
    { kind: 'chat_interaction_page', items: [input(1, 'psn_00000000000000000000000099')] },
  ))
  await model.start()
  assert.equal(model.state.status, 'error')
  assert.equal(model.state.error.code, 'CHAT_INTERACTION_BINDING_MISMATCH')
  assert.deepEqual(model.state.pendingInputs, [])
  assert.equal(client.commandCalls.length, 0)
})

test('an interaction that expires after refresh cannot issue a stale command', async () => {
  let now = Date.parse('2026-08-27T01:00:00.000Z')
  const { client, model } = view(new FakeClient(), { nowMillis: () => now })
  await model.start()
  now = Date.parse('2100-08-27T01:00:00.000Z')
  await model.respondToInput(input().inputRequestId, 'provided', {
    mode: 'single_choice',
    value: 'candidate',
  })
  assert.equal(client.commandCalls.length, 0)
  assert.equal(model.state.interaction.error.code, 'CHAT_INPUT_EXPIRED')
  await model.decideApproval(approval().id, 'approve', 'reviewed')
  assert.equal(client.commandCalls.length, 0)
  assert.equal(model.state.interaction.error.code, 'CHAT_APPROVAL_EXPIRED')
})

test('Chat view-model source has no second transport or legacy DSH Remote path', () => {
  const source = readFileSync(resolve(root, 'apps/client/src/chat-view-model.ts'), 'utf8')
  assert.match(source, /options\.client\.query/u)
  assert.match(source, /options\.client\.subscribe/u)
  assert.doesNotMatch(source, /\bfetch\s*\(|new\s+WebSocket|@deepseek-ai|dsh-typert|remote\./iu)
})
