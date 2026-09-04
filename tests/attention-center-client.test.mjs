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
    'apps/client/tsconfig.attention-center-tests.json',
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
  `Attention center client did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const viewModelModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/attention-center-tests/attention-center-view-model.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/attention-center-tests/attention-center-page.js',
)).href}`)

const { ControlPlaneClientError } = await import(`${pathToFileURL(resolve(
  root,
  '.cache/attention-center-tests/control-plane-client.js',
)).href}`)

const { createAttentionCenterViewModel } = viewModelModule
const {
  attentionCenterItemHash,
  attentionCenterPresentation,
  mountAttentionCenterPage,
  selectAttentionCenterItems,
} = pageModule
const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const productSessionId = 'psn_00000000000000000000000001'
const approvalSessionId = 'psn_00000000000000000000000002'
const executionJobId = 'job_00000000000000000000000001'
const workerSessionId = 'wss_00000000000000000000000001'
const codexThreadId = 'thr_00000000000000000000000001'
const stageRunId = 'str_00000000000000000000000001'
const deliveryId = 'dlv_00000000000000000000000001'
const inputRequestId = 'inp_00000000000000000000000001'
const approvalId = 'apr_00000000000000000000000001'
const attentionItemId = 'att_00000000000000000000000001'
const subscriptionId = 'sub_00000000000000000000000001'
const hiddenCandidateDigest = 'sha256:current-candidate-must-not-enter-dom'
const hiddenRepositoryLocator = 'ssh://user:token@private-host/secret/repository/path'
const hiddenToolPayload = 'raw-tool-payload=credential-secret'
const now = Date.parse('2026-09-03T03:00:00.000Z')

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function productSession(id, overrides = {}) {
  return {
    id,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision: 10,
    state: 'waiting_for_input',
    title: `Session ${id}`,
    updatedAt: '2026-09-03T02:30:00.000Z',
    ...overrides,
  }
}

function binding(overrides = {}) {
  return {
    productSessionId,
    executionJobId,
    workerSessionId,
    sessionIdentity: {
      productSessionId,
      workerSessionId,
      codexThreadId,
      stageRunId,
    },
    ...overrides,
  }
}

function input(overrides = {}) {
  return {
    kind: 'input',
    inputRequestId,
    revision: 4,
    state: 'pending',
    binding: binding(),
    mode: 'text',
    prompt: 'Describe the exact local change',
    options: [],
    allowEmpty: false,
    expiresAt: '2026-09-03T04:00:00.000Z',
    ...overrides,
  }
}

function approval(overrides = {}) {
  return {
    id: approvalId,
    revision: 7,
    state: 'pending',
    requestedAt: '2026-09-03T02:59:00.000Z',
    expiresAt: '2026-09-03T05:00:00.000Z',
    subject: 'Allow the projected repository action',
    binding: binding({
      productSessionId: approvalSessionId,
      sessionIdentity: {
        productSessionId: approvalSessionId,
        workerSessionId,
        codexThreadId,
      },
    }),
    ...overrides,
  }
}

function attention(overrides = {}) {
  return {
    assignedTo: null,
    blocking: true,
    createdAt: '2026-09-03T02:58:00.000Z',
    deliverySpecId: 'spec-1',
    id: attentionItemId,
    options: [],
    resolutionSummary: null,
    resolvedAt: null,
    resolvedBy: null,
    stageRunId,
    status: 'open',
    title: 'Review the proposed delivery scope',
    type: 'scope_change',
    ...overrides,
  }
}

function deliverySummary(overrides = {}) {
  return {
    activeStageRunId: stageRunId,
    deliveryId,
    openAttentionCount: 1,
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    revision: 13,
    schemaVersion,
    status: 'needs-attention',
    taskCounts: { active: 1, blocked: 0, completed: 2, failed: 0, pending: 1, total: 4, verifying: 0 },
    title: 'Delivery under attention',
    updatedAt: '2026-09-03T02:58:00.000Z',
    ...overrides,
  }
}

function deliveryDetail(overrides = {}) {
  return {
    deliveryId,
    deliveryRevision: 12,
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    attention: [attention()],
    currentCandidate: { diffSha256: hiddenCandidateDigest },
    requirements: {
      repository: { kind: 'local-git', locator: hiddenRepositoryLocator },
    },
    internalToolPayload: hiddenToolPayload,
    ...overrides,
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

function contractFake() {
  const queries = []
  const subscriptions = []
  const subscriptionHandles = []
  let currentSessions = [
    productSession(productSessionId),
    productSession(approvalSessionId, { state: 'waiting_for_approval' }),
  ]
  let currentInteractions = [input()]
  let currentApprovals = [approval()]
  let currentDeliverySummaries = [deliverySummary()]
  const currentDeliveryDetails = new Map([[deliveryId, deliveryDetail()]])

  return {
    queries,
    subscriptions,
    subscriptionHandles,
    get sessions() { return currentSessions },
    set sessions(value) { currentSessions = value },
    get interactions() { return currentInteractions },
    set interactions(value) { currentInteractions = value },
    get approvals() { return currentApprovals },
    set approvals(value) { currentApprovals = value },
    get deliverySummaries() { return currentDeliverySummaries },
    set deliverySummaries(value) { currentDeliverySummaries = value },
    set deliveryDetail(value) { currentDeliveryDetails.set(deliveryId, value) },
    setDeliveryDetailFor(deliveryIdValue, value) {
      currentDeliveryDetails.set(deliveryIdValue, value)
    },
    async query(request) {
      queries.push(structuredClone(request))
      if (request.query === 'session.list') {
        const states = request.parameters.states
        return queryResponse(request, {
          kind: 'product_session_page',
          items: currentSessions.filter(item => states.includes(item.state)),
        })
      }
      if (request.query === 'session.interactions.list') {
        return queryResponse(request, {
          kind: 'chat_interaction_page',
          items: currentInteractions.filter(item => (
            item.binding.productSessionId === request.parameters.productSessionId
          )),
        })
      }
      if (request.query === 'approval.list') {
        return queryResponse(request, { kind: 'approval_page', items: currentApprovals })
      }
      if (request.query === 'delivery.list') {
        return queryResponse(request, { kind: 'delivery_page', items: currentDeliverySummaries })
      }
      if (request.query === 'delivery.get') {
        return queryResponse(request, currentDeliveryDetails.get(request.parameters.deliveryId))
      }
      throw new Error(`unexpected query ${request.query}`)
    },
    subscribe(options) {
      subscriptions.push(options)
      const handle = {
        cursor: null,
        resume() {},
        reconnect() { handle.reconnected = true },
        close() { handle.closed = true },
      }
      subscriptionHandles.push(handle)
      return handle
    },
    close() {},
    serverUrl: 'https://control.example/local',
  }
}

function modelFor(client) {
  let nextRequest = 0
  return createAttentionCenterViewModel({
    client,
    actor,
    scope,
    subscriptionId,
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
    nowMillis: () => now,
  })
}

test('attention center loads every pending decision in the exact repository Scope through one bounded snapshot', async () => {
  const client = contractFake()
  const model = modelFor(client)
  await model.start()

  assert.deepEqual(client.queries.map(request => request.query), [
    'approval.list',
    'session.list',
    'delivery.list',
    'session.interactions.list',
    'delivery.get',
  ])
  assert.deepEqual(client.queries[0].parameters.states, ['pending', 'expired'])
  assert.deepEqual(client.queries[1].parameters.states, [
    'waiting_for_input',
    'waiting_for_approval',
  ])
  assert.deepEqual(client.queries[3].parameters, {
    productSessionId,
    states: ['pending', 'expired'],
  })
  assert.deepEqual(client.queries[4].parameters, { deliveryId })
  assert.deepEqual(client.subscriptions.map(item => item.subscription), [
    {
      scope,
      stream: { kind: 'scope' },
      eventTypes: [
        'product-session.changed.v1',
        'approval.changed.v1',
        'chat-interactions.invalidated.v1',
        'attention.changed.v1',
        'delivery.changed.v1',
      ],
    },
  ])

  const items = model.state.items
  assert.deepEqual(items.map(item => item.kind), ['attention', 'input', 'approval'])
  const attentionItem = items[0]
  assert.equal(attentionItem.id, attentionItemId)
  assert.equal(attentionItem.title, 'Review the proposed delivery scope')
  assert.equal(attentionItem.urgency, 'blocking')
  assert.equal(attentionItem.blocking, true)
  assert.equal(attentionItem.expired, false)
  assert.equal(attentionItem.bindingValid, true)
  assert.equal(attentionItem.deliveryId, deliveryId)
  assert.equal(attentionItem.stageRunId, stageRunId)
  assert.equal(attentionItem.createdAt, '2026-09-03T02:58:00.000Z')
  assert.equal(attentionItem.expiresAt, null)
  const inputItem = items[1]
  assert.equal(inputItem.id, inputRequestId)
  assert.equal(inputItem.title, 'Describe the exact local change')
  assert.equal(inputItem.urgency, 'pending')
  assert.equal(inputItem.productSessionId, productSessionId)
  assert.equal(inputItem.sessionTitle, 'Session psn_00000000000000000000000001')
  assert.equal(inputItem.stageRunId, stageRunId)
  assert.equal(inputItem.createdAt, null)
  assert.equal(inputItem.expiresAt, '2026-09-03T04:00:00.000Z')
  const approvalItem = items[2]
  assert.equal(approvalItem.id, approvalId)
  assert.equal(approvalItem.urgency, 'pending')
  assert.equal(approvalItem.productSessionId, approvalSessionId)
  assert.equal(approvalItem.createdAt, '2026-09-03T02:59:00.000Z')
  assert.equal(approvalItem.expiresAt, '2026-09-03T05:00:00.000Z')
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'subscribed')
  model.close()
})

test('expired, resolved, and Scope-violating items carry explicit fail-closed states', async () => {
  const client = contractFake()
  client.approvals = [
    approval({
      id: 'apr_00000000000000000000000002',
      state: 'expired',
      expiresAt: '2026-09-03T02:00:00.000Z',
    }),
  ]
  client.interactions = [
    input({
      inputRequestId: 'inp_00000000000000000000000002',
      state: 'expired',
      expiresAt: '2026-09-03T02:00:00.000Z',
    }),
    input({
      inputRequestId: 'inp_00000000000000000000000003',
      expiresAt: '2026-09-03T02:30:00.000Z',
    }),
    input({
      inputRequestId: 'inp_00000000000000000000000004',
      binding: binding({
        sessionIdentity: {
          productSessionId,
          workerSessionId: 'wss_00000000000000000000000009',
          codexThreadId,
        },
      }),
    }),
  ]
  client.deliverySummaries = [
    deliverySummary(),
    deliverySummary({
      deliveryId: 'dlv_00000000000000000000000002',
      title: 'Foreign delivery',
      openAttentionCount: 2,
      ownership: {
        organizationId: scope.organizationId,
        workspaceId: 'wsp_00000000000000000000000009',
        projectId: scope.projectId,
        repositoryId: scope.repositoryId,
      },
    }),
  ]
  client.setDeliveryDetailFor('dlv_00000000000000000000000002', deliveryDetail({
    deliveryId: 'dlv_00000000000000000000000002',
    deliveryRevision: 3,
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: 'wsp_00000000000000000000000009',
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    attention: [attention({ id: 'att_00000000000000000000000009' })],
  }))
  const model = modelFor(client)
  await model.start()

  const items = model.state.items
  const expiredApproval = items.find(item => item.id === 'apr_00000000000000000000000002')
  assert.equal(expiredApproval.expired, true)
  assert.equal(expiredApproval.urgency, 'expired')
  const serverExpiredInput = items.find(item => item.id === 'inp_00000000000000000000000002')
  assert.equal(serverExpiredInput.expired, true)
  assert.equal(serverExpiredInput.urgency, 'expired')
  const staleInput = items.find(item => item.id === 'inp_00000000000000000000000003')
  assert.equal(staleInput.expired, true)
  assert.equal(staleInput.urgency, 'expired')
  const foreignInput = items.find(item => item.id === 'inp_00000000000000000000000004')
  assert.equal(foreignInput.bindingValid, false)
  assert.equal(foreignInput.urgency, 'binding-invalid')
  const foreignDelivery = items.find(item => item.kind === 'attention' && !item.bindingValid)
  assert.notEqual(foreignDelivery, undefined)
  assert.equal(foreignDelivery.kind, 'attention')
  assert.equal(foreignDelivery.bindingValid, false)
  assert.equal(foreignDelivery.urgency, 'binding-invalid')
  assert.equal(foreignDelivery.title.includes('Foreign delivery'), false)
  assert.equal(foreignDelivery.deliveryId, null)
  assert.deepEqual(items.map(item => item.urgency), [
    'blocking',
    'expired',
    'expired',
    'expired',
    'binding-invalid',
    'binding-invalid',
  ])
  assert.equal(model.state.status, 'ready')
  model.close()
})

test('a ProductSession outside the exact Scope triggers no interaction query or visible input', async () => {
  const client = contractFake()
  client.sessions = [productSession(productSessionId, {
    repositoryId: 'rep_00000000000000000000000009',
  })]
  const model = modelFor(client)
  await model.start()
  assert.equal(
    client.queries.some(request => request.query === 'session.interactions.list'),
    false,
  )
  assert.equal(model.state.items.some(item => item.kind === 'input'), false)
  model.close()
})

test('an Approval without a current scoped ProductSession is binding-invalid', async () => {
  const client = contractFake()
  client.sessions = [productSession(productSessionId)]
  const model = modelFor(client)
  await model.start()
  const item = model.state.items.find(candidate => candidate.kind === 'approval')
  assert.notEqual(item, undefined)
  assert.equal(item.bindingValid, false)
  assert.equal(item.urgency, 'binding-invalid')
  model.close()
})

test('resolved Attention and closed decisions never appear in the open list', async () => {
  const client = contractFake()
  client.deliveryDetail = deliveryDetail({
    attention: [
      attention({ status: 'resolved', resolutionSummary: 'Scope accepted' }),
      attention({ id: 'att_00000000000000000000000002', status: 'dismissed' }),
    ],
  })
  const model = modelFor(client)
  await model.start()
  assert.equal(model.state.items.some(item => item.kind === 'attention'), false)
  model.close()
})

test('scope events refresh the list and authorization revocation fails closed', async () => {
  const client = contractFake()
  const model = modelFor(client)
  await model.start()
  assert.equal(model.state.items.length, 3)

  client.approvals = []
  await client.subscriptions[0].onEvent({})
  assert.equal(model.state.items.some(item => item.kind === 'approval'), false)
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'subscribed')

  client.subscriptions[0].onAuthorizationRevoked({})
  assert.equal(model.state.items.length, 0)
  assert.equal(model.state.status, 'authentication-required')
  assert.equal(model.state.realtime, 'access-revoked')
  assert.equal(client.subscriptionHandles[0].closed, true)
  model.close()

  const deniedClient = contractFake()
  const deniedModel = modelFor(deniedClient)
  await deniedModel.start()
  deniedClient.subscriptions[0].onError({
    kind: 'authorization',
    code: 'AUTHORIZATION_DENIED',
    message: 'revoked',
    requestId: null,
    retryable: false,
  })
  assert.equal(deniedModel.state.items.length, 0)
  assert.equal(deniedModel.state.status, 'authorization-denied')
  assert.equal(deniedModel.state.realtime, 'access-revoked')
  deniedModel.close()
})

test('refresh revalidates the snapshot and a network drop only degrades to reconnecting', async () => {
  const client = contractFake()
  const model = modelFor(client)
  await model.start()
  client.approvals = [approval({ subject: 'Fresh approval subject' })]
  await model.refresh()
  assert.equal(model.state.items.find(item => item.kind === 'approval').title, 'Fresh approval subject')
  assert.equal(model.state.status, 'ready')

  client.subscriptions[0].onError({
    kind: 'network',
    code: 'NETWORK_ERROR',
    message: 'socket dropped',
    requestId: null,
    retryable: true,
  })
  assert.equal(model.state.realtime, 'reconnecting')
  assert.equal(model.state.items.length > 0, true, 'a network drop must not discard the loaded list')
  model.reconnect()
  model.close()
})

test('an initial snapshot failure offers retry without a dead reconnect action', async () => {
  const client = contractFake()
  client.query = async request => {
    throw new ControlPlaneClientError({
      kind: 'network',
      code: 'NETWORK_ERROR',
      message: 'initial snapshot unavailable',
      requestId: request.requestId,
      retryable: true,
    })
  }
  const model = modelFor(client)
  await model.start()
  const presentation = attentionCenterPresentation(model.state, { kind: 'all', sort: 'urgency' })
  assert.equal(model.state.status, 'error')
  assert.equal(model.state.realtime, 'inactive')
  assert.equal(presentation.retryVisible, true)
  assert.equal(presentation.reconnectVisible, false)
  model.close()
})

test('each waiting session follows every bounded interaction page', async () => {
  const client = contractFake()
  const query = client.query.bind(client)
  const continuation = 'cursor_attention_inputs_page_2'
  client.query = async request => {
    const response = await query(request)
    if (request.query !== 'session.interactions.list') return response
    if (request.page.cursor === null) {
      return {
        ...response,
        result: { ...response.result, items: [input()] },
        page: { hasMore: true, nextCursor: continuation },
      }
    }
    assert.equal(request.page.cursor, continuation)
    return {
      ...response,
      result: {
        ...response.result,
        items: [input({ inputRequestId: 'inp_00000000000000000000000002' })],
      },
      page: { hasMore: false, nextCursor: null },
    }
  }
  const model = modelFor(client)
  await model.start()
  assert.deepEqual(
    client.queries
      .filter(request => request.query === 'session.interactions.list')
      .map(request => request.page.cursor),
    [null, continuation],
  )
  assert.deepEqual(
    model.state.items.filter(item => item.kind === 'input').map(item => item.id),
    [inputRequestId, 'inp_00000000000000000000000002'],
  )
  model.close()
})

test('authorization loss during refresh clears cards and closes the live subscription', async () => {
  const client = contractFake()
  const model = modelFor(client)
  await model.start()
  assert.equal(model.state.items.length, 3)
  const query = client.query.bind(client)
  client.query = async request => {
    if (request.query === 'approval.list') {
      throw new ControlPlaneClientError({
        kind: 'authorization',
        code: 'AUTHORIZATION_DENIED',
        message: 'private authorization detail',
        requestId: request.requestId,
        retryable: false,
      })
    }
    return query(request)
  }
  await model.refresh()
  assert.equal(model.state.status, 'authorization-denied')
  assert.equal(model.state.realtime, 'access-revoked')
  assert.deepEqual(model.state.items, [])
  assert.equal(client.subscriptionHandles[0].closed, true)
  model.close()
})

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  parentNode = null
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  type = ''
  value = ''
  id = ''
  tabIndex = 0
  title = ''
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

  get href() { return this.getAttribute('href') ?? '' }

  set href(value) { this.setAttribute('href', value) }

  append(...children) {
    for (const child of children) this.insertBefore(child, null)
  }

  replaceChildren(...children) {
    for (const child of [...this.children]) child.remove()
    for (const child of children) this.insertBefore(child, null)
  }

  insertBefore(child, reference) {
    child.remove?.()
    const index = reference === null ? this.children.length : this.children.indexOf(reference)
    this.children.splice(index < 0 ? this.children.length : index, 0, child)
    child.parentNode = this
    return child
  }

  remove() {
    if (this.parentNode === null) return
    const index = this.parentNode.children.indexOf(this)
    if (index >= 0) this.parentNode.children.splice(index, 1)
    this.parentNode = null
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value))
  }

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }

  removeEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    this.listeners.set(name, current.filter(candidate => candidate !== listener))
  }

  dispatch(name) {
    const event = { preventDefault() {} }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function allByClass(rootElement, className) {
  return descendants(rootElement).filter(node => node.className === className)
}

function byClass(rootElement, className) {
  const match = allByClass(rootElement, className)[0]
  assert.notEqual(match, undefined, `missing .${className}`)
  return match
}

function visibleText(node) {
  return descendants(node).map(current => current.textContent).join(' ')
}

function centerItem(overrides = {}) {
  return {
    kind: 'input',
    id: inputRequestId,
    title: 'Describe the exact local change',
    blocking: false,
    expired: false,
    bindingValid: true,
    urgency: 'pending',
    createdAt: null,
    expiresAt: '2026-09-03T04:00:00.000Z',
    productSessionId,
    sessionTitle: 'Session psn_00000000000000000000000001',
    stageRunId,
    executionJobId,
    deliveryId: null,
    deliveryTitle: null,
    candidateBound: false,
    revision: 4,
    ...overrides,
  }
}

function centerState(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    items: [
      centerItem(),
      centerItem({
        kind: 'attention',
        id: attentionItemId,
        title: 'Review the proposed delivery scope',
        blocking: true,
        urgency: 'blocking',
        createdAt: '2026-09-03T02:58:00.000Z',
        expiresAt: null,
        productSessionId: null,
        sessionTitle: null,
        executionJobId: null,
        deliveryId,
        deliveryTitle: 'Delivery under attention',
        candidateBound: true,
        revision: 12,
      }),
      centerItem({
        kind: 'approval',
        id: approvalId,
        title: 'Allow the projected repository action',
        createdAt: '2026-09-03T02:59:00.000Z',
        expiresAt: '2026-09-03T05:00:00.000Z',
        productSessionId: approvalSessionId,
        sessionTitle: 'Session psn_00000000000000000000000002',
        stageRunId: null,
      }),
      centerItem({
        id: 'inp_00000000000000000000000009',
        title: 'Too late',
        expired: true,
        urgency: 'expired',
        expiresAt: '2026-09-03T02:00:00.000Z',
      }),
      centerItem({
        kind: 'attention',
        id: 'att_00000000000000000000000009',
        title: 'Foreign delivery attention',
        blocking: true,
        bindingValid: false,
        urgency: 'binding-invalid',
        deliveryId: 'dlv_00000000000000000000000009',
        deliveryTitle: 'Foreign delivery',
      }),
    ],
    error: null,
    ...overrides,
  }
}

const emptyScopeSelection = {
  organizationId: scope.organizationId,
  workspaceId: scope.workspaceId,
  projectId: scope.projectId,
  repositoryId: scope.repositoryId,
}

function fakeModel(initialStateValue) {
  let state = initialStateValue
  const listeners = new Set()
  let closeCalls = 0
  return {
    get state() { return state },
    get closeCalls() { return closeCalls },
    subscribe(listener) {
      listeners.add(listener)
      listener(state)
      return () => { listeners.delete(listener) }
    },
    publish(next) {
      state = next
      for (const listener of [...listeners]) listener(state)
    },
    async start() {},
    async refresh() {},
    cancelPending() {},
    reconnect() {},
    close() { closeCalls += 1 },
  }
}

test('the Attention Center API exposes browsing only and can never fabricate decision writes', () => {
  const client = contractFake()
  const model = modelFor(client)
  for (const forbidden of [
    'provideInput',
    'cancelInput',
    'decideApproval',
    'resolveAttention',
    'command',
    'batchDecide',
    'bulkResolve',
  ]) assert.equal(model[forbidden], undefined, forbidden)
  model.close()
})

test('presentation filters by kind, keeps fail-closed labels, and never hides state', () => {
  const state = centerState()
  const all = attentionCenterPresentation(state, { kind: 'all', sort: 'urgency' })
  assert.equal(all.statusText, 'Ready · 2 need a decision · 1 blocking · 1 expired · 1 binding invalid')
  assert.equal(all.errorText, null)
  assert.equal(all.busy, false)
  assert.equal(all.retryVisible, false)
  assert.equal(all.actionsDisabled, false)
  assert.deepEqual(selectAttentionCenterItems(state, { kind: 'all', sort: 'urgency' }).map(item => item.id), [
    attentionItemId,
    inputRequestId,
    approvalId,
    'inp_00000000000000000000000009',
    'att_00000000000000000000000009',
  ])
  assert.deepEqual(
    selectAttentionCenterItems(state, { kind: 'approval', sort: 'urgency' }).map(item => item.id),
    [approvalId],
  )
  assert.deepEqual(
    selectAttentionCenterItems(state, { kind: 'attention', sort: 'urgency' }).map(item => item.id),
    [attentionItemId, 'att_00000000000000000000000009'],
  )
  const newest = selectAttentionCenterItems(state, { kind: 'attention', sort: 'newest' })
  assert.deepEqual(newest.map(item => item.id), [attentionItemId, 'att_00000000000000000000000009'])
  const byExpiry = selectAttentionCenterItems(state, { kind: 'all', sort: 'expiry' })
  assert.equal(byExpiry[0].id, 'inp_00000000000000000000000009')

  const revoked = attentionCenterPresentation(centerState({
    status: 'authentication-required',
    realtime: 'access-revoked',
    items: [],
    error: { kind: 'authentication', code: 'AUTHENTICATION_REQUIRED', message: 'x', requestId: null, retryable: false },
  }), { kind: 'all', sort: 'urgency' })
  assert.equal(revoked.actionsDisabled, true)
  assert.equal(revoked.retryVisible, false)
  assert.equal(revoked.reconnectVisible, false)
  assert.notEqual(revoked.errorText, null)
  assert.equal(revoked.statusText.includes('sign in'), true)

  const denied = attentionCenterPresentation(centerState({
    status: 'authorization-denied',
    realtime: 'access-revoked',
    items: [],
  }), { kind: 'all', sort: 'urgency' })
  assert.equal(denied.actionsDisabled, true)
})

test('item entry links open the authoritative source context with the exact Scope preserved', () => {
  assert.equal(
    attentionCenterItemHash(centerItem(), emptyScopeSelection),
    `#/attention?session=${productSessionId}`
      + `&organizationId=${scope.organizationId}&workspaceId=${scope.workspaceId}`
      + `&projectId=${scope.projectId}&repositoryId=${scope.repositoryId}`,
  )
  assert.equal(
    attentionCenterItemHash(centerItem({
      kind: 'approval',
      productSessionId: approvalSessionId,
      stageRunId: null,
    }), emptyScopeSelection),
    `#/attention?session=${approvalSessionId}`
      + `&organizationId=${scope.organizationId}&workspaceId=${scope.workspaceId}`
      + `&projectId=${scope.projectId}&repositoryId=${scope.repositoryId}`,
  )
  const strongflowHash = attentionCenterItemHash(centerItem({
    kind: 'attention',
    deliveryId,
    stageRunId,
  }), emptyScopeSelection)
  assert.match(strongflowHash, /^#\/strongflow\?/u)
  assert.match(strongflowHash, new RegExp(`delivery=${deliveryId}`), 'delivery id must be present')
  assert.match(strongflowHash, /stageRun=str_00000000000000000000000001/u)
  assert.match(strongflowHash, /repositoryId=rep_00000000000000000000000001/u)
})

test('the mounted center shows safe cards, disables fail-closed actions, and keeps drafts and controls across reloads', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const state = centerState()
  const model = fakeModel(state)
  const mounted = mountAttentionCenterPage({
    root: rootElement,
    model,
    scopeSelection: emptyScopeSelection,
    readOnly: false,
  })
  assert.equal(byClass(rootElement, 'wwc-attention-center').dataset.wwcPage, 'management')
  assert.equal(byClass(rootElement, 'wwc-attention-center-heading').dataset.wwcComponent, 'page-header')
  assert.equal(byClass(rootElement, 'wwc-attention-center-status').dataset.wwcComponent, 'status-badge')
  assert.equal(byClass(rootElement, 'wwc-attention-center-refresh').dataset.wwcComponent, 'button')
  const kindSelect = byClass(rootElement, 'wwc-attention-center-kind')
  const sortSelect = byClass(rootElement, 'wwc-attention-center-sort')
  const cards = byClass(rootElement, 'wwc-attention-center-list')
  const cardNodes = [...cards.children]
  assert.equal(cardNodes.length, 5)

  const text = visibleText(rootElement)
  assert.equal(text.includes('Review the proposed delivery scope'), true)
  assert.equal(text.includes('Describe the exact local change'), true)
  assert.equal(hiddenCandidateDigest.includes('not-enter-dom') && text.includes(hiddenCandidateDigest), false)
  assert.equal(text.includes('Task ·'), true)
  assert.equal(text.includes('Candidate ·'), true)
  for (const secret of [executionJobId, workerSessionId, codexThreadId, hiddenRepositoryLocator, hiddenToolPayload]) {
    assert.equal(text.includes(secret), false, secret)
  }
  const blockingCard = cardNodes.find(node => node.dataset.urgency === 'blocking')
  assert.notEqual(blockingCard, undefined)
  const invalidCard = cardNodes.find(node => node.dataset.urgency === 'binding-invalid')
  assert.notEqual(invalidCard, undefined)
  assert.equal(byClass(invalidCard, 'wwc-attention-card-action').getAttribute('aria-disabled'), 'true')
  assert.equal(byClass(invalidCard, 'wwc-attention-card-action').getAttribute('href'), null)
  const expiredCard = cardNodes.find(node => node.dataset.urgency === 'expired')
  assert.equal(byClass(expiredCard, 'wwc-attention-card-action').getAttribute('aria-disabled'), 'true')
  assert.equal(byClass(expiredCard, 'wwc-attention-card-action').getAttribute('href'), null)
  const actionableCard = cardNodes.find(node => node.dataset.urgency === 'pending')
  assert.equal(byClass(actionableCard, 'wwc-attention-card-action').getAttribute('aria-disabled'), null)

  kindSelect.value = 'attention'
  kindSelect.dispatch('change')
  const visibleAfterFilter = [...byClass(rootElement, 'wwc-attention-center-list').children]
  assert.equal(visibleAfterFilter.length, 2)
  assert.equal(visibleAfterFilter.includes(cardNodes[0]), true, 'keyed nodes survive filtering')

  kindSelect.value = 'all'
  kindSelect.dispatch('change')
  model.publish(centerState({ realtime: 'reloading' }))
  assert.equal(byClass(rootElement, 'wwc-attention-center-kind'), kindSelect, 'filter control survives reloads')
  assert.equal(byClass(rootElement, 'wwc-attention-center-sort'), sortSelect, 'sort control survives reloads')
  assert.equal([...byClass(rootElement, 'wwc-attention-center-list').children].includes(cardNodes[0]), true)
  mounted.close()
})

test('read-only centers and failures present explicit, non-actionable states', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const model = fakeModel(centerState())
  const mounted = mountAttentionCenterPage({
    root: rootElement,
    model,
    scopeSelection: emptyScopeSelection,
    readOnly: true,
  })
  const pendingCard = [...byClass(rootElement, 'wwc-attention-center-list').children]
    .find(node => node.dataset.urgency === 'pending')
  assert.equal(byClass(pendingCard, 'wwc-attention-card-action').getAttribute('aria-disabled'), 'true')
  assert.equal(byClass(pendingCard, 'wwc-attention-card-action').getAttribute('href'), null)
  mounted.close()

  const failedRoot = new FakeElement(document, 'div')
  const failedModel = fakeModel(centerState({
    status: 'error',
    realtime: 'reconnecting',
    items: [],
    error: { kind: 'network', code: 'NETWORK_ERROR', message: 'x', requestId: null, retryable: true },
  }))
  const failedMount = mountAttentionCenterPage({
    root: failedRoot,
    model: failedModel,
    scopeSelection: emptyScopeSelection,
    readOnly: false,
  })
  const presentation = attentionCenterPresentation(failedModel.state, { kind: 'all', sort: 'urgency' })
  assert.equal(presentation.retryVisible, true)
  assert.equal(presentation.busy, true)
  assert.notEqual(presentation.errorText, null)
  assert.equal(byClass(failedRoot, 'wwc-attention-center-list').children.length, 0)
  failedMount.close()
})

test('closing the Attention Center page closes its model exactly once', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const model = fakeModel(centerState())
  const mounted = mountAttentionCenterPage({
    root: rootElement,
    model,
    scopeSelection: emptyScopeSelection,
  })
  mounted.close()
  mounted.close()
  assert.equal(model.closeCalls, 1)
})
