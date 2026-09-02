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
    'apps/client/tsconfig.local-decisions-tests.json',
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
  `Local decisions client did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const viewModelModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/local-decisions-tests/local-decisions-view-model.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/local-decisions-tests/local-decisions-page.js',
)).href}`)

const { createLocalDecisionsViewModel } = viewModelModule
const { localDecisionsPagePresentation, mountLocalDecisionsPage } = pageModule
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
const otherProductSessionId = 'psn_00000000000000000000000002'
const executionJobId = 'job_00000000000000000000000001'
const workerSessionId = 'wss_00000000000000000000000001'
const codexThreadId = 'thr_00000000000000000000000001'
const stageRunId = 'str_00000000000000000000000001'
const deliveryId = 'dlv_00000000000000000000000001'
const inputRequestId = 'inp_00000000000000000000000001'
const choiceInputRequestId = 'inp_00000000000000000000000002'
const expiredInputRequestId = 'inp_00000000000000000000000003'
const approvalId = 'apr_00000000000000000000000001'
const expiredApprovalId = 'apr_00000000000000000000000002'
const otherApprovalId = 'apr_00000000000000000000000003'
const attentionItemId = 'att_00000000000000000000000001'
const interactionSubscriptionId = 'sub_00000000000000000000000001'
const deliverySubscriptionId = 'sub_00000000000000000000000002'
const hiddenCandidateDigest = 'sha256:current-candidate-must-not-enter-dom'
const hiddenRepositoryLocator = 'ssh://user:token@private-host/secret/repository/path'
const hiddenToolPayload = 'raw-tool-payload=credential-secret'
const now = Date.parse('2026-08-27T03:00:00.000Z')

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function session(overrides = {}) {
  return {
    id: productSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision: 10,
    state: 'waiting_for_input',
    title: 'Local decision session',
    updatedAt: '2026-08-27T03:00:00.000Z',
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
    expiresAt: '2026-08-27T04:00:00.000Z',
    ...overrides,
  }
}

function choiceInput(overrides = {}) {
  return input({
    inputRequestId: choiceInputRequestId,
    revision: 5,
    mode: 'single_choice',
    prompt: 'Choose the next planning action',
    options: [
      { label: 'Plan Delta', value: 'plan_delta' },
      { label: 'Replan', value: 'replan' },
    ],
    ...overrides,
  })
}

function approval(overrides = {}) {
  return {
    id: approvalId,
    revision: 7,
    state: 'pending',
    requestedAt: '2026-08-27T02:59:00.000Z',
    expiresAt: '2026-08-27T04:00:00.000Z',
    subject: 'Allow the projected repository action',
    binding: binding(),
    ...overrides,
  }
}

function attention(overrides = {}) {
  return {
    assignedTo: null,
    blocking: true,
    createdAt: '2026-08-27T02:58:00.000Z',
    deliverySpecId: 'spec-1',
    id: attentionItemId,
    options: [
      {
        id: 'safe-option-internal-id',
        label: 'Accept current scope',
        description: 'Continue with the current bounded delivery scope.',
      },
    ],
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

function detail(overrides = {}) {
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

function deliveryProjection(revision = 13) {
  return { deliveryId, revision }
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

function contractFake() {
  const queries = []
  const commands = []
  const subscriptions = []
  let currentSession = session()
  let currentInteractions = [
    input(),
    choiceInput(),
    input({
      inputRequestId: expiredInputRequestId,
      revision: 6,
      state: 'expired',
      expiresAt: '2026-08-27T02:00:00.000Z',
    }),
  ]
  let currentApprovals = [
    approval(),
    approval({
      id: expiredApprovalId,
      revision: 8,
      state: 'expired',
      expiresAt: '2026-08-27T02:00:00.000Z',
    }),
    approval({
      id: otherApprovalId,
      binding: binding({
        productSessionId: otherProductSessionId,
        sessionIdentity: {
          productSessionId: otherProductSessionId,
          workerSessionId,
          codexThreadId,
        },
      }),
    }),
  ]
  let currentDetail = detail()
  let deferred = null

  function complete(request) {
    if (request.command === 'input.respond') {
      currentInteractions = currentInteractions.filter(item => (
        item.inputRequestId !== request.payload.inputRequestId
      ))
      currentSession = session({ revision: currentSession.revision + 1 })
      return commandResponse(request, currentSession)
    }
    if (request.command === 'approval.decide') {
      const decided = currentApprovals.find(item => item.id === request.payload.approvalId)
      currentApprovals = currentApprovals.filter(item => item.id !== request.payload.approvalId)
      return commandResponse(request, {
        ...decided,
        revision: decided.revision + 1,
        state: request.payload.decision === 'approve' ? 'approved' : 'rejected',
      })
    }
    if (request.command === 'delivery.resolve_attention') {
      currentDetail = detail({
        ...currentDetail,
        deliveryRevision: currentDetail.deliveryRevision + 1,
        attention: currentDetail.attention.filter(item => item.id !== request.payload.attentionItemId),
      })
      return commandResponse(request, deliveryProjection(currentDetail.deliveryRevision))
    }
    throw new Error(`unexpected command ${request.command}`)
  }

  return {
    queries,
    commands,
    subscriptions,
    get session() { return currentSession },
    set session(value) { currentSession = value },
    get interactions() { return currentInteractions },
    set interactions(value) { currentInteractions = value },
    get approvals() { return currentApprovals },
    set approvals(value) { currentApprovals = value },
    get detail() { return currentDetail },
    set detail(value) { currentDetail = value },
    deferNextCommand() {
      assert.equal(deferred, null)
      deferred = {}
    },
    finishDeferredCommand() {
      assert.notEqual(deferred, null)
      deferred.resolve(complete(deferred.request))
      deferred = null
    },
    async query(request) {
      queries.push(structuredClone(request))
      if (request.query === 'session.get') return queryResponse(request, currentSession)
      if (request.query === 'session.interactions.list') {
        return queryResponse(request, { kind: 'chat_interaction_page', items: currentInteractions })
      }
      if (request.query === 'approval.list') {
        return queryResponse(request, { kind: 'approval_page', items: currentApprovals })
      }
      if (request.query === 'delivery.get') return queryResponse(request, currentDetail)
      throw new Error(`unexpected query ${request.query}`)
    },
    command(request) {
      commands.push(structuredClone(request))
      if (deferred !== null && deferred.request === undefined) {
        deferred.request = request
        return new Promise(resolvePromise => { deferred.resolve = resolvePromise })
      }
      return Promise.resolve(complete(request))
    },
    subscribe(options) {
      subscriptions.push(options)
      return {
        cursor: null,
        resume() {},
        reconnect() { this.reconnected = true },
        close() { this.closed = true },
      }
    },
    close() {},
    serverUrl: 'https://control.example/local',
  }
}

function modelFor(client) {
  let nextRequest = 0
  return createLocalDecisionsViewModel({
    client,
    actor,
    scope,
    productSessionId,
    interactionSubscriptionId,
    delivery: { deliveryId, subscriptionId: deliverySubscriptionId },
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
    nowMillis: () => now,
  })
}

test('local decisions snapshot retains exact bindings but discards raw Delivery and tool payloads', async () => {
  const client = contractFake()
  const model = modelFor(client)
  await model.start()

  assert.deepEqual(client.queries.map(request => request.query), [
    'session.get',
    'session.interactions.list',
    'approval.list',
    'delivery.get',
  ])
  assert.deepEqual(client.queries[1].parameters, {
    productSessionId,
    states: ['pending', 'expired'],
  })
  assert.deepEqual(client.queries[2].parameters.states, ['pending', 'expired'])
  assert.equal(model.state.inputs.length, 3)
  assert.equal(model.state.inputs.find(item => item.projection.inputRequestId === expiredInputRequestId).expired, true)
  assert.deepEqual(model.state.approvals.map(item => item.projection.id), [approvalId, expiredApprovalId])
  assert.equal(model.state.attention[0].candidateDigest, hiddenCandidateDigest)
  assert.equal(JSON.stringify(model.state).includes(hiddenRepositoryLocator), false)
  assert.equal(JSON.stringify(model.state).includes(hiddenToolPayload), false)
  assert.equal(JSON.stringify(model.state).includes(executionJobId), true)
  assert.equal(JSON.stringify(model.state).includes(workerSessionId), true)
  assert.equal(JSON.stringify(model.state).includes(stageRunId), true)
  assert.deepEqual(client.subscriptions.map(item => item.subscription), [
    {
      scope,
      stream: { kind: 'product-session', productSessionId },
      eventTypes: [
        'product-session.changed.v1',
        'approval.changed.v1',
        'chat-interactions.invalidated.v1',
      ],
    },
    {
      scope,
      stream: { kind: 'delivery', deliveryId },
      eventTypes: ['attention.changed.v1', 'delivery.changed.v1'],
    },
  ])
  model.close()
})

test('input responses bind the full execution identity and duplicate or expired submissions fail closed', async () => {
  const client = contractFake()
  const model = modelFor(client)
  await model.start()
  client.deferNextCommand()
  const first = model.provideInput(inputRequestId, { mode: 'text', value: 'private response' })
  const duplicate = model.provideInput(inputRequestId, { mode: 'text', value: 'duplicate response' })
  assert.equal(client.commands.length, 1)
  assert.deepEqual(client.commands[0], {
    schemaVersion,
    actor,
    scope,
    requestId: client.commands[0].requestId,
    command: 'input.respond',
    expectedRevision: 4,
    payload: {
      executionJobId,
      inputRequestId,
      productSessionId,
      sessionIdentity: binding().sessionIdentity,
      status: 'provided',
      value: { mode: 'text', value: 'private response' },
      workerSessionId,
    },
  })
  client.finishDeferredCommand()
  await Promise.all([first, duplicate])
  assert.equal(model.state.inputs.some(item => item.projection.inputRequestId === inputRequestId), false)

  const beforeExpired = client.commands.length
  await model.provideInput(expiredInputRequestId, { mode: 'text', value: 'too late' })
  assert.equal(client.commands.length, beforeExpired)
  assert.equal(model.state.interaction.error.code, 'LOCAL_DECISIONS_INPUT_EXPIRED')

  await model.provideInput(choiceInputRequestId, { mode: 'single_choice', value: 'old_option' })
  assert.equal(client.commands.length, beforeExpired)
  assert.equal(model.state.interaction.error.code, 'LOCAL_DECISIONS_INPUT_OPTION_STALE')
  await model.provideInput(choiceInputRequestId, { mode: 'single_choice', value: 'replan' })
  assert.equal(client.commands.at(-1).payload.value.value, 'replan')
  model.close()

  const cancelClient = contractFake()
  const cancelModel = modelFor(cancelClient)
  await cancelModel.start()
  await cancelModel.cancelInput(choiceInputRequestId)
  assert.deepEqual(cancelClient.commands.at(-1).payload, {
    executionJobId,
    inputRequestId: choiceInputRequestId,
    productSessionId,
    sessionIdentity: binding().sessionIdentity,
    status: 'cancelled',
    value: null,
    workerSessionId,
  })
  assert.equal(cancelClient.commands.at(-1).expectedRevision, 5)
  cancelModel.close()
})

test('approval and Attention decisions carry current revisions and deduplicate in-flight clicks', async () => {
  const client = contractFake()
  const model = modelFor(client)
  await model.start()

  client.deferNextCommand()
  const approvalFirst = model.decideApproval(approvalId, 'approve', 'Current projection reviewed')
  const approvalDuplicate = model.decideApproval(approvalId, 'reject', 'duplicate')
  assert.equal(client.commands.length, 1)
  assert.deepEqual(client.commands[0].payload, {
    approvalId,
    binding: binding(),
    decision: 'approve',
    reason: 'Current projection reviewed',
  })
  assert.equal(client.commands[0].expectedRevision, 7)
  client.finishDeferredCommand()
  await Promise.all([approvalFirst, approvalDuplicate])
  assert.equal(model.state.approvals.some(item => item.projection.id === approvalId), false)
  const afterApproval = client.commands.length
  await model.decideApproval(expiredApprovalId, 'approve', 'too late')
  assert.equal(client.commands.length, afterApproval)
  assert.equal(model.state.interaction.error.code, 'LOCAL_DECISIONS_APPROVAL_EXPIRED')

  client.deferNextCommand()
  const attentionFirst = model.resolveAttention(attentionItemId, 'resolve', 'Scope confirmed')
  const attentionDuplicate = model.resolveAttention(attentionItemId, 'dismiss', 'duplicate')
  assert.equal(client.commands.length, afterApproval + 1)
  assert.equal(client.commands.at(-1).expectedRevision, 12)
  assert.deepEqual(client.commands.at(-1).payload, {
    attentionItemId,
    deliveryId,
    decision: 'resolve',
    resolution: 'Scope confirmed',
    remediation: null,
  })
  client.finishDeferredCommand()
  await Promise.all([attentionFirst, attentionDuplicate])
  assert.equal(model.state.attention.length, 0)
  model.close()
})

test('event invalidation reloads input, approval, and Attention snapshots and reconnect is explicit', async () => {
  const client = contractFake()
  const model = modelFor(client)
  await model.start()
  client.interactions = [choiceInput({ revision: 20 })]
  client.approvals = [approval({ revision: 21, subject: 'Fresh approval' })]
  client.detail = detail({
    deliveryRevision: 22,
    attention: [attention({ title: 'Fresh Attention' })],
  })
  await client.subscriptions[0].onEvent({})
  assert.deepEqual(model.state.inputs.map(item => item.projection.revision), [20])
  assert.deepEqual(model.state.approvals.map(item => item.projection.revision), [21])
  assert.equal(model.state.attention[0].deliveryRevision, 22)
  assert.equal(model.state.realtime, 'subscribed')

  client.subscriptions[0].onError({
    kind: 'network',
    code: 'NETWORK_ERROR',
    message: 'socket dropped',
    requestId: null,
    retryable: true,
  })
  assert.equal(model.state.realtime, 'reconnecting')
  model.reconnect()
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
  autocomplete = ''
  spellcheck = true
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

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

function pageState(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    session: session(),
    inputs: [
      { projection: input(), expired: false },
      { projection: choiceInput(), expired: false },
      {
        projection: input({
          inputRequestId: expiredInputRequestId,
          revision: 6,
          state: 'expired',
          expiresAt: '2026-08-27T02:00:00.000Z',
        }),
        expired: true,
      },
    ],
    approvals: [
      { projection: approval(), expired: false },
      {
        projection: approval({
          id: expiredApprovalId,
          revision: 8,
          state: 'expired',
          expiresAt: '2026-08-27T02:00:00.000Z',
        }),
        expired: true,
      },
    ],
    attention: [{
      projection: attention(),
      deliveryId,
      deliveryRevision: 12,
      candidateDigest: hiddenCandidateDigest,
    }],
    interaction: { status: 'idle', operation: null, targetId: null, error: null },
    error: null,
    ...overrides,
  }
}

test('local decisions page exposes safe labels, clears responses synchronously, and disables expired controls', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const calls = []
  let state = pageState()
  let listener = () => {}
  const model = {
    get state() { return state },
    subscribe(next) {
      listener = next
      next(state)
      return () => { listener = () => {} }
    },
    async start() {},
    async refresh() { calls.push({ operation: 'refresh' }) },
    async provideInput(id, value) { calls.push({ operation: 'input', id, value }) },
    async cancelInput(id) { calls.push({ operation: 'cancel-input', id }) },
    async decideApproval(id, decision, reason) {
      calls.push({ operation: 'approval', id, decision, reason })
    },
    async resolveAttention(id, decision, resolution) {
      calls.push({ operation: 'attention', id, decision, resolution })
    },
    cancelPending() {},
    reconnect() { calls.push({ operation: 'reconnect' }) },
    close() {},
  }
  const mounted = mountLocalDecisionsPage({ root: rootElement, model })
  assert.equal(byClass(rootElement, 'wwc-local-decisions').dataset.wwcPage, 'management')
  assert.equal(byClass(rootElement, 'wwc-local-decisions-heading').dataset.wwcComponent, 'page-header')
  assert.equal(byClass(rootElement, 'wwc-local-decisions-status').dataset.wwcComponent, 'status-badge')
  assert.equal(byClass(rootElement, 'wwc-local-decisions-retry').dataset.wwcComponent, 'button')
  assert.equal(byClass(rootElement, 'wwc-local-inputs').dataset.wwcComponent, 'panel')
  assert.equal(byClass(rootElement, 'wwc-local-approvals').dataset.wwcComponent, 'panel')
  assert.equal(byClass(rootElement, 'wwc-local-attention').dataset.wwcComponent, 'panel')
  const text = visibleText(rootElement)
  assert.equal(text.includes('Plan Delta'), true)
  assert.equal(text.includes('Replan'), true)
  assert.equal(text.includes('plan_delta'), false)
  assert.equal(text.includes(hiddenCandidateDigest), false)
  assert.equal(text.includes(hiddenRepositoryLocator), false)
  assert.equal(text.includes(hiddenToolPayload), false)
  assert.equal(text.includes(productSessionId), false)
  assert.equal(text.includes(stageRunId), false)
  assert.equal(text.includes(executionJobId), false)
  assert.equal(text.includes(workerSessionId), false)

  const response = byClass(rootElement, 'wwc-local-input-response')
  response.value = 'sensitive input response'
  byClass(rootElement, 'wwc-local-input-form').dispatch('submit')
  assert.equal(response.value, '')
  assert.deepEqual(calls.at(-1), {
    operation: 'input',
    id: inputRequestId,
    value: { mode: 'text', value: 'sensitive input response' },
  })

  const optionButtons = allByClass(rootElement, 'wwc-local-input-option')
  optionButtons[1].dispatch('click')
  assert.deepEqual(calls.at(-1), {
    operation: 'input',
    id: choiceInputRequestId,
    value: { mode: 'single_choice', value: 'replan' },
  })

  const reason = byClass(rootElement, 'wwc-local-approval-reason')
  reason.value = 'sensitive approval reason'
  byClass(rootElement, 'wwc-local-approval-approve').dispatch('click')
  assert.equal(reason.value, '')
  assert.deepEqual(calls.at(-1), {
    operation: 'approval',
    id: approvalId,
    decision: 'approve',
    reason: 'sensitive approval reason',
  })

  const resolution = byClass(rootElement, 'wwc-local-attention-resolution')
  resolution.value = 'sensitive business resolution'
  byClass(rootElement, 'wwc-local-attention-resolve').dispatch('click')
  assert.equal(resolution.value, '')
  assert.deepEqual(calls.at(-1), {
    operation: 'attention',
    id: attentionItemId,
    decision: 'resolve',
    resolution: 'sensitive business resolution',
  })

  assert.equal(allByClass(rootElement, 'wwc-local-input-submit').at(-1).disabled, true)
  assert.equal(allByClass(rootElement, 'wwc-local-input-cancel').at(-1).disabled, true)
  assert.equal(allByClass(rootElement, 'wwc-local-approval-approve').at(-1).disabled, true)
  assert.equal(allByClass(rootElement, 'wwc-local-approval-reject').at(-1).disabled, true)

  state = pageState({ realtime: 'reconnecting' })
  listener(state)
  assert.equal(byClass(rootElement, 'wwc-local-decisions-reconnect').hidden, false)

  state = pageState({ inputs: [], approvals: [], attention: [] })
  listener(state)
  assert.equal(byClass(rootElement, 'wwc-local-input-empty').dataset.wwcComponent, 'empty-state')
  assert.equal(byClass(rootElement, 'wwc-local-approval-empty').dataset.wwcComponent, 'empty-state')
  assert.equal(byClass(rootElement, 'wwc-local-attention-empty').dataset.wwcComponent, 'empty-state')
  mounted.close()
})

test('local decision keyed updates preserve row drafts, focus, scroll, and bounded DOM', () => {
  const document = new FakeDocument()
  document.activeElement = null
  const rootElement = new FakeElement(document, 'div')
  let state = pageState()
  let listener = () => {}
  const model = {
    get state() { return state },
    subscribe(next) {
      listener = next
      next(state)
      return () => { listener = () => {} }
    },
    async start() {},
    async refresh() {},
    async provideInput() {},
    async cancelInput() {},
    async decideApproval() {},
    async resolveAttention() {},
    cancelPending() {},
    reconnect() {},
    close() {},
  }
  const mounted = mountLocalDecisionsPage({ root: rootElement, model })
  const inputList = byClass(rootElement, 'wwc-local-input-list')
  const approvalList = byClass(rootElement, 'wwc-local-approval-list')
  const attentionList = byClass(rootElement, 'wwc-local-attention-list')
  const inputRow = inputList.children[0]
  const approvalRow = approvalList.children[0]
  const attentionRow = attentionList.children[0]
  const response = byClass(inputRow, 'wwc-local-input-response')
  const inputForm = byClass(inputRow, 'wwc-local-input-form')
  const reason = byClass(approvalRow, 'wwc-local-approval-reason')
  const resolution = byClass(attentionRow, 'wwc-local-attention-resolution')
  const attentionOption = byClass(attentionRow, 'wwc-local-attention-option-label').parentNode

  response.value = 'draft input'
  reason.value = 'draft approval'
  resolution.value = 'draft Attention'
  response.selectionStart = 4
  document.activeElement = response
  inputList.scrollTop = 45
  approvalList.scrollTop = 46
  attentionList.scrollTop = 47

  for (let index = 0; index < 200; index += 1) {
    state = pageState({
      realtime: index % 2 === 0 ? 'reloading' : 'subscribed',
      inputs: [
        { projection: input({ prompt: index === 199 ? 'Updated prompt' : 'Describe the exact local change' }), expired: false },
        { projection: choiceInput(), expired: false },
      ],
      approvals: [{ projection: approval({ subject: index === 199 ? 'Updated approval' : 'Allow the projected repository action' }), expired: false }],
      attention: [{
        projection: attention({ title: index === 199 ? 'Updated Attention' : 'Review the proposed delivery scope' }),
        deliveryId,
        deliveryRevision: 12 + index,
        candidateDigest: hiddenCandidateDigest,
      }],
    })
    listener(state)
  }

  assert.equal(inputList.children[0], inputRow)
  assert.equal(approvalList.children[0], approvalRow)
  assert.equal(attentionList.children[0], attentionRow)
  assert.equal(byClass(attentionRow, 'wwc-local-attention-option-label').parentNode, attentionOption)
  assert.equal(byClass(inputRow, 'wwc-local-input-prompt').textContent, 'Updated prompt')
  assert.equal(byClass(approvalRow, 'wwc-local-approval-subject').textContent, 'Updated approval')
  assert.equal(byClass(attentionRow, 'wwc-local-attention-title').textContent, 'Updated Attention')
  assert.equal(response.value, 'draft input')
  assert.equal(reason.value, 'draft approval')
  assert.equal(resolution.value, 'draft Attention')
  assert.equal(response.selectionStart, 4)
  assert.equal(document.activeElement, response)
  assert.equal(inputList.scrollTop, 45)
  assert.equal(approvalList.scrollTop, 46)
  assert.equal(attentionList.scrollTop, 47)
  assert.equal(inputList.children.length, 2)
  assert.equal(approvalList.children.length, 1)
  assert.equal(attentionList.children.length, 1)

  mounted.close()
  assert.equal((inputForm.listeners.get('submit') ?? []).length, 0)
  assert.equal(response.value, '')
  assert.equal(reason.value, '')
  assert.equal(resolution.value, '')
})

test('local decisions presentation never exposes raw server messages and source uses one facade path', () => {
  const stale = pageState({
    interaction: {
      status: 'error',
      operation: 'approval.decide',
      targetId: approvalId,
      error: {
        kind: 'server',
        code: 'REVISION_CONFLICT',
        message: `private ${hiddenToolPayload} ${hiddenRepositoryLocator}`,
        requestId: null,
        retryable: false,
      },
    },
  })
  assert.equal(
    localDecisionsPagePresentation(stale).errorText,
    'This item changed before the decision was saved. Review the current snapshot and try again.',
  )
  assert.equal(localDecisionsPagePresentation(stale).errorText.includes(hiddenToolPayload), false)

  const viewModelSource = readFileSync(
    resolve(root, 'apps/client/src/local-decisions-view-model.ts'),
    'utf8',
  )
  const pageSource = readFileSync(
    resolve(root, 'apps/client/src/local-decisions-page.ts'),
    'utf8',
  )
  for (const source of [viewModelSource, pageSource]) {
    assert.doesNotMatch(source, /\bfetch\s*\(/u)
    assert.doesNotMatch(source, /new\s+WebSocket/u)
    assert.doesNotMatch(source, /innerHTML/u)
    assert.doesNotMatch(source, /console\./u)
    assert.doesNotMatch(source, /node:fs|child_process|\bprocess\.|localStorage|sessionStorage/u)
    assert.doesNotMatch(source, /navigator\.|performance\./u)
  }
  assert.equal((viewModelSource.match(/\.\/control-plane-client\.js/gu) ?? []).length, 1)
  assert.equal((pageSource.match(/\.\/local-decisions-view-model\.js/gu) ?? []).length, 1)
})
