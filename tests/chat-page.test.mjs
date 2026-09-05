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
    'apps/client/tsconfig.chat-page-tests.json',
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
  `Chat page did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const page = await import(`${pathToFileURL(resolve(
  root,
  '.cache/chat-page-tests/chat-page.js',
)).href}?run=${String(Date.now())}`)

const {
  chatComposerKeyAction,
  chatPagePresentation,
  mountChatPage,
} = page
const productSessionId = 'psn_00000000000000000000000001'
const otherProductSessionId = 'psn_00000000000000000000000002'
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
const modelRoute = {
  providerId: 'provider-one',
  modelId: 'model-one',
  credentialReferenceId: 'crd_PRIVATE_REFERENCE_00000001',
}
const secondModelRoute = {
  providerId: 'provider-two',
  modelId: 'model-two',
  credentialReferenceId: 'crd_PRIVATE_REFERENCE_00000002',
}

function modelRouteOption(route = modelRoute, overrides = {}) {
  return {
    route,
    providerDisplayName: route === modelRoute ? 'Primary Provider' : 'Second Provider',
    modelDisplayName: route === modelRoute ? 'Primary Model' : 'Second Model',
    catalogSource: scope,
    catalogVersion: 3,
    providerVersion: 2,
    modelVersion: 4,
    contextWindowTokens: 128_000,
    maxOutputTokens: 16_000,
    toolSupport: 'parallel',
    reasoningEfforts: ['medium', 'high'],
    credentialRotationVersion: 1,
    isDefault: route === modelRoute,
    status: 'enabled',
    reason: 'ready',
    ...overrides,
  }
}

function modelRouteAvailability(items = [modelRouteOption()], overrides = {}) {
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

function session(
  id = productSessionId,
  state = 'running',
  title = 'Primary Chat',
) {
  return {
    id,
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
    revision: 1,
    state,
    title,
    updatedAt: '2026-08-27T01:00:00.000Z',
  }
}

function message() {
  return {
    id: 'msg_00000000000000000000000001',
    productSessionId,
    role: 'assistant',
    content: '<script>not markup</script>',
    sequence: 1,
    state: 'streaming',
    createdAt: '2026-08-27T01:00:00.000Z',
    updatedAt: '2026-08-27T01:00:00.000Z',
  }
}

function decisionBinding(sessionId = productSessionId) {
  return {
    productSessionId: sessionId,
    executionJobId: 'job_00000000000000000000000001',
    workerSessionId: 'wsn_00000000000000000000000001',
    sessionIdentity: {
      productSessionId: sessionId,
      workerSessionId: 'wsn_00000000000000000000000001',
      codexThreadId: 'cdx_00000000000000000000000001',
      stageRunId: 'run_00000000000000000000000001',
    },
  }
}

function pendingInput(overrides = {}) {
  return {
    kind: 'input',
    inputRequestId: 'inp_00000000000000000000000001',
    revision: 2,
    state: 'pending',
    prompt: 'Select the workspace to continue in.',
    binding: decisionBinding(),
    mode: 'single_choice',
    options: [
      { id: 'ich_00000000000000000000000001', value: 'candidate', label: 'Candidate workspace' },
    ],
    allowEmpty: false,
    expiresAt: '2099-08-27T01:10:00.000Z',
    ...overrides,
  }
}

function pendingApproval(overrides = {}) {
  return {
    id: 'apr_00000000000000000000000001',
    requestedAt: '2026-08-27T01:00:00.000Z',
    expiresAt: '2099-08-27T01:10:00.000Z',
    revision: 4,
    state: 'pending',
    subject: 'Run the approved test command.',
    binding: decisionBinding(),
    ...overrides,
  }
}

function state(overrides = {}) {
  const activeSession = session()
  return {
    status: 'ready',
    realtime: 'subscribed',
    activeProductSessionId: productSessionId,
    sessions: [
      activeSession,
      session(otherProductSessionId, 'idle', 'Second Chat'),
    ],
    session: activeSession,
    messages: [message()],
    messagePagination: {
      status: 'idle',
      hasMore: true,
      nextCursor: 'cursor_0000000001',
      error: null,
    },
    modelRouteAvailability: modelRouteAvailability(),
    selectedModelRoute: modelRoute,
    modelRouteSelectionIssue: null,
    runtime: null,
    pendingInputs: [],
    pendingApprovals: [],
    interaction: { status: 'idle', error: null },
    error: null,
    ...overrides,
  }
}

function controlPlaneError(kind, message, code = 'TEST_ERROR') {
  return {
    name: 'ControlPlaneClientError',
    kind,
    code,
    message,
    requestId: null,
    retryable: false,
  }
}

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
  id = ''
  htmlFor = ''
  rows = 0
  autocomplete = ''
  selectedIndex = -1
  value = ''
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

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    this.listeners.set(name, listeners.filter(candidate => candidate !== listener))
  }

  emit(name, values = {}) {
    let prevented = false
    const event = {
      preventDefault() { prevented = true },
      ...values,
    }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
    return prevented
  }

  requestSubmit() {
    this.emit('submit')
  }

  focus() {
    this.ownerDocument.activeElement = this
  }
}

class FakeDocument {
  activeElement = null

  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

function findByClass(node, className) {
  if (node.className === className) return node
  for (const child of node.children) {
    const match = findByClass(child, className)
    if (match !== null) return match
  }
  return null
}

function findAllByClass(node, className, matches = []) {
  if (node.className === className) matches.push(node)
  for (const child of node.children) findAllByClass(child, className, matches)
  return matches
}

class FakeChatViewModel {
  constructor(initialState) {
    this.state = initialState
  }

  calls = []
  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  async start() { this.calls.push(['start']) }
  async refresh() { this.calls.push(['refresh']) }
  async loadMoreMessages() { this.calls.push(['loadMoreMessages']) }
  reconnect() { this.calls.push(['reconnect']) }
  cancelPending() { this.calls.push(['cancelPending']) }
  close() { this.calls.push(['close']) }

  async selectSession(id) {
    this.calls.push(['selectSession', id])
  }

  selectModelRoute(route) {
    this.calls.push(['selectModelRoute', route])
    this.state = { ...this.state, selectedModelRoute: route }
  }

  async createSession(input) {
    this.calls.push(['createSession', input])
  }

  async submitMessage(value) {
    this.calls.push(['submitMessage', value])
  }

  async cancelSession(reason) {
    this.calls.push(['cancelSession', reason])
  }

  async respondToInput(inputRequestId, status, value) {
    this.calls.push(['respondToInput', inputRequestId, status, value])
  }

  async decideApproval(approvalId, decision, reason) {
    this.calls.push(['decideApproval', approvalId, decision, reason])
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }
}

class FakeDeliveryCreator {
  state = { status: 'idle', error: null }
  calls = []
  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }

  async create(input) { this.calls.push(['create', input]) }
  cancelPending() { this.calls.push(['cancelPending']) }
  close() { this.calls.push(['close']) }
}

test('composer keyboard behavior preserves newlines and submits only plain Enter', () => {
  assert.equal(chatComposerKeyAction({ key: 'Enter', shiftKey: false, isComposing: false }), 'submit')
  assert.equal(chatComposerKeyAction({ key: 'Enter', shiftKey: true, isComposing: false }), 'newline')
  assert.equal(chatComposerKeyAction({ key: 'Enter', shiftKey: false, isComposing: true }), 'newline')
  assert.equal(chatComposerKeyAction({ key: 'A', shiftKey: false, isComposing: false }), 'ignore')
})

test('presentation explains running, continuing, busy, access, and network states', () => {
  assert.deepEqual(
    {
      label: chatPagePresentation(state()).sendLabel,
      cancel: chatPagePresentation(state()).cancelVisible,
    },
    { label: 'Steer', cancel: true },
  )
  assert.equal(chatPagePresentation(state({
    session: session(productSessionId, 'waiting_for_input'),
  })).sendLabel, 'Continue')
  assert.equal(chatPagePresentation(state({ realtime: 'reloading' })).messageListBusy, true)
  assert.equal(chatPagePresentation(state({
    status: 'authentication-required',
    error: controlPlaneError('authentication', 'TOKEN=do-not-render'),
  })).errorText, 'Sign in again to continue this Chat.')

  const networkText = chatPagePresentation(state({
    error: controlPlaneError('network', 'http://worker.internal:9000/TOKEN'),
  })).errorText
  assert.match(networkText, /connection and retry/u)
  assert.doesNotMatch(networkText, /worker|9000|TOKEN/iu)
  assert.equal(chatPagePresentation(state({ status: 'closed' })).composerDisabled, true)
})

test('presentation explains first-Chat model setup and bounded creation failures', () => {
  const firstChat = chatPagePresentation(state({
    activeProductSessionId: null,
    sessions: [],
    session: null,
    messages: [],
  }))
  assert.equal(firstChat.statusText, 'Ready for a new Chat')
  assert.match(firstChat.emptyText, /first Chat/iu)

  const noModel = chatPagePresentation(state({
    activeProductSessionId: null,
    sessions: [],
    session: null,
    messages: [],
    modelRouteAvailability: modelRouteAvailability([], {
      settingsSource: null,
      settingsRevision: null,
      defaultProviderId: null,
      defaultModelId: null,
      status: 'disabled',
      reason: 'no_provider',
    }),
    selectedModelRoute: null,
  }))
  assert.equal(noModel.statusText, 'Model setup required')
  assert.match(noModel.emptyText, /No model route is configured/iu)

  const errors = [
    ['IDEMPOTENCY_CONFLICT', /earlier request/iu],
    ['PERMISSION_DENIED', /do not have access/iu],
    ['INVALID_REQUEST', /selected model/iu],
    ['SERVICE_UNAVAILABLE', /temporarily unavailable/iu],
    ['TRUSTED_FACTS_UNAVAILABLE', /Provider or model is unavailable/iu],
  ]
  for (const [code, pattern] of errors) {
    const errorText = chatPagePresentation(state({
      interaction: {
        status: 'error',
        error: controlPlaneError(
          code === 'PERMISSION_DENIED' ? 'authorization' : 'server',
          'private server diagnostic',
          code,
        ),
      },
    })).errorText
    assert.match(errorText, pattern)
    assert.doesNotMatch(errorText, /private server diagnostic/iu)
  }
})

test('Chat keeps the Session decisions on the first screen and retires the card when they close', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeChatViewModel(state({
    pendingInputs: [pendingInput()],
    pendingApprovals: [pendingApproval()],
  }))
  const mounted = mountChatPage({ root: rootElement, model })

  const card = findByClass(rootElement, 'wwc-contextual-decision')
  assert.equal(card.hidden, false)
  const items = findAllByClass(rootElement, 'wwc-contextual-decision-item')
  assert.equal(items.length, 2)
  // The blocking tool approval is the first row of the card.
  assert.equal(items[0].dataset.kind, 'approval')
  assert.equal(items[0].dataset.urgency, 'blocking')
  assert.match(
    findByClass(items[0], 'wwc-contextual-decision-title').textContent,
    /Run the approved test command/u,
  )
  assert.equal(items[1].dataset.kind, 'input')
  // The card is not a second announcement channel: Chat keeps one polite status.
  assert.equal(findByClass(card, 'wwc-contextual-decision-note').getAttribute('aria-live'), null)

  model.publish(state())
  assert.equal(findByClass(rootElement, 'wwc-contextual-decision').hidden, true)
  assert.deepEqual(findAllByClass(rootElement, 'wwc-contextual-decision-item'), [])

  mounted.close()
  assert.deepEqual(model.calls.at(-1), ['close'])
})

test('Chat decides an approval and answers an input through its own commands', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeChatViewModel(state({
    pendingInputs: [pendingInput()],
    pendingApprovals: [pendingApproval()],
  }))
  const mounted = mountChatPage({ root: rootElement, model })

  const items = findAllByClass(rootElement, 'wwc-contextual-decision-item')
  const approvalRow = items.find(row => row.dataset.kind === 'approval')
  const reason = findByClass(approvalRow, 'wwc-contextual-decision-response')
  reason.value = 'Reviewed the exact command.'
  findByClass(approvalRow, 'wwc-contextual-decision-submit').emit('click')
  assert.deepEqual(model.calls.at(-1), [
    'decideApproval',
    'apr_00000000000000000000000001',
    'approve',
    'Reviewed the exact command.',
  ])
  // The typed reason survives until the server stops listing the decision.
  assert.equal(reason.value, 'Reviewed the exact command.')

  const inputRow = items.find(row => row.dataset.kind === 'input')
  findByClass(inputRow, 'wwc-contextual-decision-option').emit('click')
  assert.deepEqual(model.calls.at(-1), [
    'respondToInput',
    'inp_00000000000000000000000001',
    'provided',
    { mode: 'single_choice', value: 'candidate' },
  ])

  // A read-only Chat renders the decisions but submits nothing.
  const sealedRoot = document.createElement('main')
  const sealed = mountChatPage({
    root: sealedRoot,
    model: new FakeChatViewModel(state({ pendingApprovals: [pendingApproval()] })),
    readOnly: true,
  })
  const sealedRow = findAllByClass(sealedRoot, 'wwc-contextual-decision-item')[0]
  assert.equal(findByClass(sealedRow, 'wwc-contextual-decision-submit').disabled, true)
  findByClass(sealedRow, 'wwc-contextual-decision-submit').emit('click')
  assert.deepEqual(model.calls.filter(([name]) => name === 'decideApproval').length, 1)
  sealed.close()
  mounted.close()
})

test('mounted Chat page exposes accessible state and delegates every interaction to its view-model', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeChatViewModel(state())
  model.state = state({
    modelRouteAvailability: modelRouteAvailability([
      modelRouteOption(),
      modelRouteOption(secondModelRoute),
    ]),
  })
  const mounted = mountChatPage({
    root: rootElement,
    model,
    nextProductSessionId: () => 'psn_00000000000000000000000003',
  })

  const status = findByClass(rootElement, 'wwc-chat-status')
  const alert = findByClass(rootElement, 'wwc-chat-error')
  const messages = findByClass(rootElement, 'wwc-chat-messages')
  const composer = findByClass(rootElement, 'wwc-chat-composer-input')
  const send = findByClass(rootElement, 'wwc-chat-send')
  const cancel = findByClass(rootElement, 'wwc-chat-cancel')
  const loadEarlier = findByClass(rootElement, 'wwc-chat-load-earlier')
  const newSession = findByClass(rootElement, 'wwc-chat-new-session')
  const modelSelect = findByClass(rootElement, 'wwc-chat-model')
  const sessionList = findByClass(rootElement, 'wwc-chat-session-list')

  assert.equal(status.getAttribute('role'), 'status')
  assert.equal(status.getAttribute('aria-live'), 'polite')
  assert.equal(alert.getAttribute('role'), 'alert')
  assert.equal(alert.getAttribute('aria-live'), 'assertive')
  assert.equal(messages.getAttribute('aria-live'), 'polite')
  assert.equal(messages.getAttribute('aria-busy'), 'false')
  assert.equal(messages.children[0].children[0].children[1].textContent, '<script>not markup</script>')
  assert.match(modelSelect.children[0].textContent, /Repository scope.*Primary Provider/iu)
  assert.match(modelSelect.children[0].textContent, /Primary Model/iu)
  assert.doesNotMatch(modelSelect.children[0].textContent, /PRIVATE_REFERENCE/u)

  modelSelect.selectedIndex = 1
  modelSelect.emit('change')
  assert.deepEqual(model.calls.at(-1), ['selectModelRoute', secondModelRoute])

  composer.value = '  steer this run  '
  composer.emit('input')
  assert.equal(send.disabled, false)
  assert.equal(composer.emit('keydown', {
    key: 'Enter',
    shiftKey: false,
    isComposing: false,
  }), true)
  await new Promise(resolve => setImmediate(resolve))
  assert.deepEqual(model.calls.at(-1), ['submitMessage', 'steer this run'])
  assert.equal(composer.value, '')

  composer.value = 'keep the newline'
  composer.emit('keydown', { key: 'Enter', shiftKey: true, isComposing: false })
  assert.equal(model.calls.filter(([name]) => name === 'submitMessage').length, 1)

  cancel.emit('click')
  loadEarlier.emit('click')
  sessionList.children[1].children[0].emit('click')
  newSession.emit('click')
  await new Promise(resolve => setImmediate(resolve))
  assert.deepEqual(model.calls.slice(-4).map(([name]) => name), [
    'cancelSession',
    'loadMoreMessages',
    'selectSession',
    'createSession',
  ])
  assert.deepEqual(model.calls.at(-1)[1], {
    productSessionId: 'psn_00000000000000000000000003',
    title: 'New Chat',
  })

  mounted.close()
  assert.deepEqual(model.calls.at(-1), ['close'])
  assert.deepEqual(rootElement.children, [])
})

test('read-only Chat keeps reads available and blocks every write action', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeChatViewModel(state())
  const mounted = mountChatPage({
    root: rootElement,
    model,
    readOnly: true,
    nextProductSessionId: () => 'psn_00000000000000000000000003',
  })
  const composer = findByClass(rootElement, 'wwc-chat-composer-input')
  const send = findByClass(rootElement, 'wwc-chat-send')
  const cancel = findByClass(rootElement, 'wwc-chat-cancel')
  const newSession = findByClass(rootElement, 'wwc-chat-new-session')
  assert.equal(composer.disabled, true)
  assert.equal(send.disabled, true)
  assert.equal(cancel.disabled, true)
  assert.equal(newSession.disabled, true)
  composer.value = 'must stay local'
  findByClass(rootElement, 'wwc-chat-composer').emit('submit')
  cancel.emit('click')
  newSession.emit('click')
  await Promise.resolve()
  assert.equal(model.calls.some(([name]) => (
    name === 'submitMessage' || name === 'cancelSession' || name === 'createSession'
  )), false)
  assert.equal(findByClass(rootElement, 'wwc-chat-retry').disabled, false)
  mounted.close()
})

test('Chat keyed updates retain session, message, model, composer, focus, and scroll identity', () => {
  const document = new FakeDocument()
  document.activeElement = null
  const rootElement = document.createElement('main')
  const model = new FakeChatViewModel(state())
  const mounted = mountChatPage({ root: rootElement, model })
  const sessions = findByClass(rootElement, 'wwc-chat-session-list')
  const messages = findByClass(rootElement, 'wwc-chat-messages')
  const modelSelect = findByClass(rootElement, 'wwc-chat-model')
  const composer = findByClass(rootElement, 'wwc-chat-composer-input')
  const sessionRow = sessions.children[0]
  const messageRow = messages.children[0]
  const modelOption = modelSelect.children[0]
  composer.value = 'dirty composer'
  composer.selectionStart = 7
  composer.scrollTop = 31
  document.activeElement = composer
  messages.scrollTop = 72

  for (let index = 0; index < 200; index += 1) {
    model.publish(state({
      realtime: index % 2 === 0 ? 'reloading' : 'subscribed',
      interaction: { status: index % 2 === 0 ? 'waiting' : 'idle', error: null },
    }))
  }

  assert.equal(sessions.children[0], sessionRow)
  assert.equal(messages.children[0], messageRow)
  assert.equal(modelSelect.children[0], modelOption)
  assert.equal(composer.value, 'dirty composer')
  assert.equal(composer.selectionStart, 7)
  assert.equal(composer.scrollTop, 31)
  assert.equal(document.activeElement, composer)
  assert.equal(messages.scrollTop, 72)
  assert.equal(sessions.children.length, 2)
  assert.equal(messages.children.length, 1)
  mounted.close()
  assert.equal(model.listener, null)
})

test('Chat confirms one editable requirement draft before converting it to StrongFlow', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const confirmedRequirement = {
    ...message(),
    role: 'user',
    state: 'completed',
    content: 'Implement the requirement confirmed in this Chat.',
  }
  const model = new FakeChatViewModel(state({ messages: [confirmedRequirement] }))
  const deliveryCreator = new FakeDeliveryCreator()
  const mounted = mountChatPage({
    root: rootElement,
    model,
    nextProductSessionId: () => 'psn_00000000000000000000000003',
    deliveryCreator,
    scope,
  })

  const open = findByClass(rootElement, 'wwc-chat-convert-delivery')
  assert.equal(open.disabled, false)
  assert.equal(open.getAttribute('aria-expanded'), 'false')
  assert.notEqual(open.getAttribute('aria-controls'), null)
  open.focus()
  open.emit('click')
  const form = findByClass(rootElement, 'wwc-chat-convert-form')
  const confirmationPanel = findByClass(rootElement, 'wwc-chat-convert')
  const title = findByClass(rootElement, 'wwc-chat-convert-title')
  const goal = findByClass(rootElement, 'wwc-chat-convert-goal')
  const baseline = findByClass(rootElement, 'wwc-chat-convert-baseline')
  const deliveryScope = findByClass(rootElement, 'wwc-chat-convert-delivery-scope')
  const outOfScope = findByClass(rootElement, 'wwc-chat-convert-out-of-scope')
  const constraints = findByClass(rootElement, 'wwc-chat-convert-constraints')
  const criteria = findByClass(rootElement, 'wwc-chat-convert-criteria')
  const source = findByClass(rootElement, 'wwc-chat-convert-source-session')
  const repositoryScope = findByClass(rootElement, 'wwc-chat-convert-scope')
  const modelContext = findByClass(rootElement, 'wwc-chat-convert-model')
  const confirmation = findByClass(rootElement, 'wwc-chat-convert-confirm')
  assert.equal(confirmationPanel.hidden, false)
  // UI-604: the confirmation is a named dialog that takes focus on open and
  // hands it back to the trigger on Escape.
  assert.equal(confirmationPanel.getAttribute('role'), 'dialog')
  assert.equal(confirmationPanel.getAttribute('aria-modal'), 'false')
  const labelledBy = confirmationPanel.getAttribute('aria-labelledby')
  assert.equal(labelledBy, findByClass(rootElement, 'wwc-chat-convert-heading').id)
  assert.equal(open.getAttribute('aria-expanded'), 'true')
  assert.equal(open.getAttribute('aria-controls'), labelledBy)
  assert.equal(document.activeElement, title)
  assert.equal(findByClass(rootElement, 'wwc-chat-convert-submit').disabled, false)
  assert.equal(title.value, 'Primary Chat')
  assert.equal(goal.value, 'Implement the requirement confirmed in this Chat.')
  assert.equal(deliveryScope.value, 'Implement the requirement confirmed in this Chat.')
  assert.equal(source.value, productSessionId)
  assert.equal(source.readOnly, true)
  assert.match(repositoryScope.value, /rep_00000000000000000000000001/u)
  assert.equal(repositoryScope.readOnly, true)
  assert.match(modelContext.value, /provider-one.*model-one/iu)
  assert.doesNotMatch(modelContext.value, /PRIVATE_REFERENCE/u)

  baseline.value = '0123456789abcdef0123456789abcdef01234567'
  deliveryScope.value = 'Implement the confirmed requirement.'
  outOfScope.value = 'Replace Chat.'
  constraints.value = 'Keep the repository binding.'
  criteria.value = 'The confirmed result is delivered.\nThe real snapshot is subscribed.'
  form.emit('submit')
  assert.equal(deliveryCreator.calls.length, 0, 'explicit confirmation is required')
  confirmation.checked = true
  form.emit('submit')
  await Promise.resolve()
  assert.deepEqual(deliveryCreator.calls.at(-1), ['create', {
    title: 'Primary Chat',
    goal: 'Implement the requirement confirmed in this Chat.',
    baseRevision: '0123456789abcdef0123456789abcdef01234567',
    scope: ['Implement the confirmed requirement.'],
    outOfScope: ['Replace Chat.'],
    constraints: ['Keep the repository binding.'],
    sourceProductSessionId: productSessionId,
    acceptanceCriteria: [
      'The confirmed result is delivered.',
      'The real snapshot is subscribed.',
    ],
  }])

  deliveryCreator.publish({
    status: 'error',
    error: controlPlaneError('authorization', 'private permission detail', 'PERMISSION_DENIED'),
  })
  assert.equal(goal.value, 'Implement the requirement confirmed in this Chat.')
  assert.equal(baseline.value, '0123456789abcdef0123456789abcdef01234567')
  assert.match(findByClass(rootElement, 'wwc-chat-convert-error').textContent, /permission/iu)
  assert.doesNotMatch(findByClass(rootElement, 'wwc-chat-convert-error').textContent, /private/iu)

  deliveryCreator.publish({ status: 'submitting', error: null })
  assert.equal(findByClass(rootElement, 'wwc-chat-convert-submit').disabled, true)
  findByClass(rootElement, 'wwc-chat-convert-cancel').emit('click')
  assert.deepEqual(deliveryCreator.calls.at(-1), ['cancelPending'])

  const prevented = confirmationPanel.emit('keydown', { key: 'Escape', cancelable: true })
  assert.equal(prevented, true)
  assert.equal(confirmationPanel.hidden, true)
  assert.equal(open.getAttribute('aria-expanded'), 'false')
  assert.equal(document.activeElement, open)

  mounted.close()
  assert.deepEqual(deliveryCreator.calls.at(-1), ['close'])
})

test('Chat page keeps an invalid route visible, blocks creation, and links to Settings', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeChatViewModel(state({
    activeProductSessionId: null,
    sessions: [],
    session: null,
    messages: [],
    modelRouteAvailability: modelRouteAvailability([modelRouteOption(modelRoute, {
      status: 'disabled',
      reason: 'credential_missing_or_revoked',
    })], {
      status: 'disabled',
      reason: 'credential_missing_or_revoked',
    }),
    selectedModelRoute: null,
    modelRouteSelectionIssue: 'credential_missing_or_revoked',
  }))
  const mounted = mountChatPage({
    root: rootElement,
    model,
    nextProductSessionId: () => 'psn_00000000000000000000000003',
  })

  const modelSelect = findByClass(rootElement, 'wwc-chat-model')
  const newSession = findByClass(rootElement, 'wwc-chat-new-session')
  const settings = findByClass(rootElement, 'wwc-chat-model-settings')
  const notice = findByClass(rootElement, 'wwc-chat-model-notice')
  assert.equal(modelSelect.children[0].disabled, true)
  assert.match(modelSelect.children[0].textContent, /credential missing or revoked/iu)
  assert.equal(newSession.disabled, true)
  assert.equal(settings.href, '#/settings')
  assert.equal(settings.hidden, false)
  assert.match(notice.textContent, /previously selected.*credential/iu)
  assert.equal(notice.hidden, false)
  mounted.close()
})

test('Chat page source has no transport, legacy Remote, secret rendering, or HTML injection path', () => {
  const source = readFileSync(resolve(root, 'apps/client/src/chat-page.ts'), 'utf8')
  assert.match(source, /aria-live', 'polite'/u)
  assert.match(source, /aria-live', 'assertive'/u)
  assert.match(source, /aria-busy/u)
  assert.match(source, /event\.isComposing|chatComposerKeyAction\(event\)/u)
  assert.match(source, /event\.shiftKey|chatComposerKeyAction\(event\)/u)
  assert.match(source, /form\.requestSubmit\(\)/u)
  assert.match(source, /options\.model\.submitMessage/u)
  assert.doesNotMatch(
    source,
    /\bfetch\s*\(|new\s+WebSocket|@deepseek-ai|dsh-typert|remote\.|\.query\s*\(|\.command\s*\(|innerHTML/iu,
  )
  assert.doesNotMatch(source, /https?:\/\/|wss?:\/\/|worker\.internal|CREDENTIAL_SECRET/iu)
})
