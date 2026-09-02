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
const modelRoute = {
  providerId: 'provider-one',
  modelId: 'model-one',
  credentialReferenceId: 'crd_PRIVATE_REFERENCE_00000001',
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
    defaultModelRoute: modelRoute,
    runtime: null,
    pendingApprovals: [],
    interaction: { status: 'idle', error: null },
    error: null,
    ...overrides,
  }
}

function controlPlaneError(kind, message) {
  return {
    name: 'ControlPlaneClientError',
    kind,
    code: 'TEST_ERROR',
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
    this.children = []
  }

  append(...children) {
    this.children.push(...children)
  }

  replaceChildren(...children) {
    this.children = [...children]
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
}

class FakeDocument {
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

  async createSession(input) {
    this.calls.push(['createSession', input])
  }

  async submitMessage(value) {
    this.calls.push(['submitMessage', value])
  }

  async cancelSession(reason) {
    this.calls.push(['cancelSession', reason])
  }
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

test('mounted Chat page exposes accessible state and delegates every interaction to its view-model', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeChatViewModel(state())
  const mounted = mountChatPage({
    root: rootElement,
    model,
    modelRoutes: [{
      providerId: 'provider-two',
      modelId: 'model-two',
      credentialReferenceId: 'crd_PRIVATE_REFERENCE_00000002',
    }],
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
  assert.equal(modelSelect.children[0].textContent, 'provider-one / model-one')
  assert.doesNotMatch(modelSelect.children[0].textContent, /PRIVATE_REFERENCE/u)

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
  assert.equal(model.calls.at(-1)[1].modelRoute.credentialReferenceId, modelRoute.credentialReferenceId)

  mounted.close()
  assert.deepEqual(model.calls.at(-1), ['close'])
  assert.deepEqual(rootElement.children, [])
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
