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
    'apps/client/tsconfig.readiness-tests.json',
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
  `Readiness page boundary did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/readiness-tests/readiness-page.js',
)).href}`)
const { mountReadinessPage } = pageModule

const NOW = '2026-09-03T08:30:00.000Z'

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
  id = ''
  htmlFor = ''
  type = ''
  value = ''
  #ownText = ''

  // Real DOM textContent aggregates descendants; the fake mirrors that contract.
  get textContent() {
    return this.#ownText + this.children.map(child => child.textContent).join('')
  }
  set textContent(value) {
    this.#ownText = String(value)
    this.children = []
  }
  append(...children) { this.children.push(...children) }
  replaceChildren(...children) { this.children = [...children] }
  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }
  remove() {}
  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }
  removeEventListener(name, listener) {
    this.listeners.set(
      name,
      (this.listeners.get(name) ?? []).filter(candidate => candidate !== listener),
    )
  }
  dispatchEvent(event) {
    for (const listener of this.listeners.get(event.type) ?? []) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) { return new FakeElement(this, tagName) }
}

function item(id, status, reason = null, checkedAt = NOW) {
  return { id, status, reason, errorCode: null, checkedAt }
}

function freshInstallState() {
  return {
    status: 'attention',
    collapsed: false,
    items: [
      item('repository-scope', 'ready'),
      item('model-route', 'attention', 'no-provider'),
      item('credential-reference', 'attention', 'no-credential-reference'),
      item('server-worker-health', 'attention', 'no-worker-reported'),
      item('helper-availability', 'attention', 'no-enabled-worker-capacity'),
      item('first-chat-delivery', 'attention', 'no-chat-session'),
    ],
  }
}

function modelFake(initialState) {
  let current = structuredClone(initialState)
  let listener = null
  const calls = []
  return {
    calls,
    get state() { return current },
    subscribe(next) {
      listener = next
      next(current)
      return () => { listener = null }
    },
    updateContext: async () => {},
    setCollapsed(collapsed) { calls.push(['setCollapsed', collapsed]) },
    refresh: async () => { calls.push(['refresh']) },
    close() {},
    publish(next) {
      current = structuredClone(next)
      listener?.(current)
    },
  }
}

function fixTarget(single) {
  if (single.id === 'repository-scope') return null
  if (single.id === 'model-route') return { href: '#/settings?repositoryId=rep_one', label: 'Open Settings' }
  if (single.id === 'first-chat-delivery') {
    return { href: '#/chat?repositoryId=rep_one', label: 'Start your first Chat' }
  }
  return { href: '#/settings/runtime?repositoryId=rep_one', label: 'Open local diagnostics' }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function mount(state) {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const model = modelFake(state)
  const page = mountReadinessPage({
    root: rootElement,
    model,
    fixTarget,
  })
  return { page, model, rootElement }
}

test('every checklist item shows its status, reason, check time, and real fix entry', () => {
  const { page, rootElement } = mount(freshInstallState())

  const section = descendants(rootElement).find(node => node.className === 'wwc-readiness')
  assert.notEqual(section, undefined)
  assert.equal(section.getAttribute('aria-label'), 'First-run readiness')

  const summary = descendants(rootElement).find(node => (
    node.className === 'wwc-readiness-summary'
  ))
  assert.match(summary.textContent, /1 of 6 complete/u)

  const items = descendants(rootElement).filter(node => (
    node.className === 'wwc-readiness-item'
  ))
  assert.equal(items.length, 6)
  const byId = Object.fromEntries(items.map(node => [node.dataset.itemId, node]))
  assert.deepEqual(Object.keys(byId), [
    'repository-scope',
    'model-route',
    'credential-reference',
    'server-worker-health',
    'helper-availability',
    'first-chat-delivery',
  ])
  assert.equal(byId['repository-scope'].dataset.status, 'ready')
  assert.match(byId['repository-scope'].textContent, /Complete/u)
  assert.equal(byId['model-route'].dataset.status, 'attention')
  assert.match(byId['model-route'].textContent, /provider is configured/u)
  assert.match(byId['model-route'].textContent, /2026-09-03T08:30:00.000Z/u)
  assert.equal(byId['helper-availability'].dataset.status, 'attention')
  assert.match(byId['helper-availability'].textContent, /execution capacity/u)

  const fixes = descendants(rootElement).filter(node => (
    node.className === 'wwc-readiness-fix'
  ))
  assert.equal(fixes.length, 5)
  const modelFix = fixes.find(node => node.textContent === 'Open Settings')
  assert.equal(modelFix.href, '#/settings?repositoryId=rep_one')
  const chatFix = fixes.find(node => node.textContent === 'Start your first Chat')
  assert.equal(chatFix.href, '#/chat?repositoryId=rep_one')

  const serialized = JSON.stringify({
    text: rootElement.textContent,
  })
  assert.equal(serialized.includes('crd_'), false)
  assert.equal(serialized.includes('secret'), false)
  page.close()
})

test('collapse hides the checklist body while the summary stays visible', () => {
  const { page, model, rootElement } = mount(freshInstallState())

  const toggle = descendants(rootElement).find(node => (
    node.className === 'wwc-readiness-toggle'
  ))
  assert.equal(toggle.getAttribute('aria-expanded'), 'true')
  assert.equal(toggle.getAttribute('aria-controls'), 'wwc-readiness-items')
  toggle.dispatchEvent({ type: 'click' })
  assert.deepEqual(model.calls, [['setCollapsed', true]])

  model.publish({ ...freshInstallState(), collapsed: true })
  const items = descendants(rootElement).find(node => (
    node.className === 'wwc-readiness-items'
  ))
  assert.equal(items.hidden, true)
  const updatedToggle = descendants(rootElement).find(node => (
    node.className === 'wwc-readiness-toggle'
  ))
  assert.equal(updatedToggle.getAttribute('aria-expanded'), 'false')
  const summary = descendants(rootElement).find(node => (
    node.className === 'wwc-readiness-summary'
  ))
  assert.equal(summary.hidden, false)
  page.close()
})

test('recheck triggers a fresh model refresh and complete state is announced', async () => {
  const { page, model, rootElement } = mount(freshInstallState())

  const recheck = descendants(rootElement).find(node => (
    node.className === 'wwc-readiness-recheck'
  ))
  recheck.dispatchEvent({ type: 'click' })
  await new Promise(resolvePromise => setTimeout(resolvePromise, 0))
  assert.deepEqual(model.calls, [['refresh']])

  model.publish({
    status: 'ready',
    collapsed: false,
    items: freshInstallState().items.map(single => item(single.id, 'ready')),
  })
  const summary = descendants(rootElement).find(node => (
    node.className === 'wwc-readiness-summary'
  ))
  assert.match(summary.textContent, /6 of 6 complete/u)
  assert.match(summary.textContent, /complete/iu)
  page.close()
})

test('blocked and unavailable items explain themselves without fake check times', () => {
  const { page, rootElement } = mount({
    status: 'attention',
    collapsed: false,
    items: [
      item('repository-scope', 'attention', 'scope-selection-required'),
      item('model-route', 'blocked', null, null),
      item('credential-reference', 'blocked', null, null),
      item('server-worker-health', 'unavailable', null, NOW, 'NETWORK_UNREACHABLE'),
      item('helper-availability', 'blocked', null, null),
      item('first-chat-delivery', 'blocked', null, null),
    ],
  })

  const items = descendants(rootElement).filter(node => (
    node.className === 'wwc-readiness-item'
  ))
  const byId = Object.fromEntries(items.map(node => [node.dataset.itemId, node]))
  assert.match(byId['repository-scope'].textContent, /Choose an authorized repository Scope/u)
  assert.match(byId['model-route'].textContent, /Waiting for the repository Scope/u)
  assert.equal(byId['model-route'].textContent.includes(NOW), false)
  assert.match(byId['server-worker-health'].textContent, /could not run/u)
  assert.match(byId['server-worker-health'].textContent, /2026-09-03T08:30:00.000Z/u)
  const scopeFixes = descendants(rootElement).filter(node => (
    node.className === 'wwc-readiness-fix'
  ))
  assert.equal(scopeFixes.length, 0)
  assert.match(byId['repository-scope'].textContent, /Scope selector/u)
  page.close()
})
