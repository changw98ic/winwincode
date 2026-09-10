// SPDX-License-Identifier: Apache-2.0
//
// UI-604 keyboard, focus, and semantics findings that can be pinned with a
// deterministic DOM.  The shell-level findings (surface slot live region,
// repeated navigation bypass, one heading per page, and the collection live
// regions) are covered by the real-browser suite in
// tests/ui604-shell-a11y-browser.test.mjs because they need real focus order
// and real layout.

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
    'apps/client/tsconfig.ui604-a11y-tests.json',
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
  `UI-604 audit modules did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/ui604-a11y-tests')
const chatModule = await import(`${pathToFileURL(resolve(
  cache,
  'chat-page.js',
)).href}`)
const panelModule = await import(`${pathToFileURL(resolve(
  root,
  'packages/browser-ui/dist/index.js',
)).href}`)

const { mountChatPage } = chatModule
const { mountPanel } = panelModule

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
  checked = false
  selectedIndex = -1
  tabIndex = -1
  value = ''
  #textContent = ''

  get textContent() {
    return this.#textContent + this.children.map(child => child.textContent).join('')
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

  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }

  addEventListener(name, listener) {
    this.listeners.set(name, [...(this.listeners.get(name) ?? []), listener])
  }

  removeEventListener(name, listener) {
    this.listeners.set(
      name,
      (this.listeners.get(name) ?? []).filter(candidate => candidate !== listener),
    )
  }

  emit(name, values = {}) {
    let prevented = false
    const event = {
      target: this,
      preventDefault() { prevented = true },
      ...values,
    }
    let current = this
    while (current !== null) {
      for (const listener of current.listeners.get(name) ?? []) listener(event)
      current = current.parentNode
    }
    return prevented
  }

  requestSubmit() { this.emit('submit') }

  focus() { this.ownerDocument.activeElement = this }

  click() { this.emit('click') }
}

class FakeDocument {
  activeElement = null
  createElement(tagName) { return new FakeElement(this, tagName) }
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

// --- Chat conversion dialog focus and Escape (audit finding A4) --------------

const scopeSelection = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const productSessionId = 'psn_00000000000000000000000001'
const modelRoute = {
  providerId: 'browser-provider',
  modelId: 'browser-model',
  credentialReferenceId: 'crd_00000000000000000000000001',
}

function modelRouteOption() {
  return {
    route: modelRoute,
    providerDisplayName: 'Browser Provider',
    modelDisplayName: 'Browser Model',
    catalogSource: { kind: 'repository', ...scopeSelection },
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
  }
}

function chatSession(id = productSessionId) {
  return {
    id,
    projectId: scopeSelection.projectId,
    repositoryId: scopeSelection.repositoryId,
    revision: 1,
    state: 'idle',
    title: 'Confirmed Chat',
    updatedAt: '2026-08-27T01:00:00.000Z',
  }
}

function chatState(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    activeProductSessionId: productSessionId,
    sessions: [chatSession()],
    session: chatSession(),
    messages: [{
      id: 'msg_00000000000000000000000001',
      productSessionId,
      role: 'user',
      content: 'Ship the confirmed requirement.',
      sequence: 1,
      state: 'completed',
      createdAt: '2026-08-27T01:00:00.000Z',
      updatedAt: '2026-08-27T01:00:00.000Z',
    }],
    messagePagination: { status: 'idle', hasMore: false, nextCursor: null, error: null },
    modelRouteAvailability: {
      kind: 'model_route_availability_page',
      scope: { kind: 'repository', ...scopeSelection },
      settingsSource: { kind: 'repository', ...scopeSelection },
      settingsRevision: 1,
      requestPoolSource: { kind: 'repository', ...scopeSelection },
      requestPoolRevision: 1,
      defaultProviderId: modelRoute.providerId,
      defaultModelId: modelRoute.modelId,
      status: 'enabled',
      reason: 'ready',
      items: [modelRouteOption()],
    },
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

class FakeChatModel {
  constructor(initialState) { this.state = initialState }

  listener = null
  calls = []

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }

  async start() {}
  async refresh() {}
  async loadMoreMessages() {}
  reconnect() {}
  cancelPending() {}
  async selectSession() {}
  selectModelRoute() {}
  async createSession() {}
  async submitMessage() {}
  async cancelSession() {}
  close() {}
}

class FakeDeliveryCreator {
  state = { status: 'idle', error: null }
  listener = null
  calls = []

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
  cancelPending() {}
  close() {}
}

function mountedChat(stateOverrides = {}) {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'main')
  const model = new FakeChatModel(chatState(stateOverrides))
  const deliveryCreator = new FakeDeliveryCreator()
  const page = mountChatPage({
    root: rootElement,
    model,
    deliveryCreator,
    scope: scopeSelection,
    nextProductSessionId: () => 'psn_00000000000000000000000002',
  })
  return { document, rootElement, model, deliveryCreator, page }
}

test('A4 the Chat conversion panel is a named dialog tied to its trigger', () => {
  const { rootElement, page } = mountedChat()
  const trigger = findByClass(rootElement, 'wwc-chat-delegation-chip')
  assert.equal(trigger.getAttribute('aria-expanded'), 'false')
  assert.notEqual(trigger.getAttribute('aria-controls'), null)

  trigger.click()
  const dialog = findByClass(rootElement, 'wwc-chat-convert')
  assert.equal(dialog.hidden, false, 'activation opens the conversion dialog')
  assert.equal(dialog.getAttribute('role'), 'dialog')
  assert.equal(dialog.getAttribute('aria-modal'), 'false')
  const labelledBy = dialog.getAttribute('aria-labelledby')
  assert.notEqual(labelledBy, null, 'the dialog must be named by its heading')
  const heading = findByClass(rootElement, 'wwc-chat-convert-heading')
  assert.equal(heading.id, labelledBy)
  assert.notEqual(heading.textContent, '')
  assert.equal(trigger.getAttribute('aria-expanded'), 'true')
  assert.equal(trigger.getAttribute('aria-controls'), labelledBy)
  page.close()
})

test('A4 opening the conversion dialog moves focus into its first field', () => {
  const { document, rootElement, page } = mountedChat()
  const trigger = findByClass(rootElement, 'wwc-chat-delegation-chip')
  trigger.focus()
  trigger.click()
  const title = findByClass(rootElement, 'wwc-chat-convert-title')
  assert.equal(
    document.activeElement,
    title,
    'keyboard users must land on the first dialog field, not stay on the trigger',
  )
  page.close()
})

test('A4 Escape closes the conversion dialog and returns focus to the trigger', () => {
  const { document, rootElement, page } = mountedChat()
  const trigger = findByClass(rootElement, 'wwc-chat-delegation-chip')
  trigger.focus()
  trigger.click()
  const dialog = findByClass(rootElement, 'wwc-chat-convert')
  assert.equal(dialog.hidden, false)

  const prevented = dialog.emit('keydown', { key: 'Escape', cancelable: true })
  assert.equal(prevented, true, 'Escape must be consumed by the dialog')
  assert.equal(dialog.hidden, true, 'Escape closes the dialog')
  assert.equal(trigger.getAttribute('aria-expanded'), 'false')
  assert.equal(
    document.activeElement,
    trigger,
    'focus must return to the control that opened the dialog',
  )
  page.close()
})

// --- Panel heading level (audit finding A6) ----------------------------------

test('A6 panels can nest one level below the single page heading', () => {
  const document = new FakeDocument()
  const control = new FakeElement(document, 'div')
  const defaultLevel = mountPanel({
    document,
    props: { id: 'panel-default', title: 'Default level', control },
  })
  assert.equal(defaultLevel.title.tagName, 'H2')
  const nested = mountPanel({
    document,
    props: { id: 'panel-nested', title: 'Nested level', control, headingLevel: 3 },
  })
  assert.equal(nested.title.tagName, 'H3')
  assert.equal(nested.root.getAttribute('aria-labelledby'), nested.title.id)
})
