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
    'apps/client/tsconfig.scope-selector-tests.json',
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
  `Scope selector boundary did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/scope-selector-tests/scope-selector-page.js',
)).href}`)
const { mountScopeSelectorPage } = pageModule

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
  #textContent = ''

  get textContent() { return this.#textContent }
  set textContent(value) {
    this.#textContent = String(value)
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

function state(status = 'ready') {
  return {
    status,
    selection: {
      organizationId: null,
      workspaceId: null,
      projectId: null,
      repositoryId: null,
    },
    options: {
      organizations: [
        { id: 'org_00000000000000000000000001', label: 'Acme' },
        { id: 'org_00000000000000000000000002', label: 'Beta' },
      ],
      workspaces: [],
      projects: [],
      repositories: [],
    },
    emptyLevel: null,
    error: null,
  }
}

function modelFake() {
  let current = state()
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
    start: async () => {},
    retry: async () => { calls.push(['retry']) },
    selectOrganization: async id => { calls.push(['organization', id]) },
    selectWorkspace: async id => { calls.push(['workspace', id]) },
    selectProject: async id => { calls.push(['project', id]) },
    selectRepository: async id => { calls.push(['repository', id]) },
    close() {},
    publish(next) {
      current = next
      listener?.(current)
    },
  }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

test('scope selector uses labelled native controls and keeps unavailable descendants disabled', async () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const model = modelFake()
  const page = mountScopeSelectorPage({
    root: rootElement,
    model,
    contextStatus: 'selection-required',
  })

  const region = descendants(rootElement).find(node => node.className === 'wwc-scope-selector')
  const controls = descendants(rootElement).filter(node => node.tagName === 'SELECT')
  assert.equal(region.getAttribute('aria-label'), 'Current Scope')
  assert.equal(controls.length, 4)
  assert.equal(controls[0].disabled, false)
  assert.equal(controls[1].disabled, true)
  assert.equal(controls[2].disabled, true)
  assert.equal(controls[3].disabled, true)
  assert.deepEqual(
    descendants(rootElement).filter(node => node.tagName === 'LABEL').map(node => node.textContent),
    ['Organization', 'Workspace', 'Project', 'Repository'],
  )
  controls[0].value = 'org_00000000000000000000000002'
  controls[0].dispatchEvent({ type: 'change' })
  await new Promise(resolvePromise => setTimeout(resolvePromise, 0))
  assert.deepEqual(model.calls, [['organization', 'org_00000000000000000000000002']])
  page.close()
})

test('revoked URL context is announced and network metadata failures offer retry', async () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const model = modelFake()
  const page = mountScopeSelectorPage({
    root: rootElement,
    model,
    contextStatus: 'denied',
  })
  const access = descendants(rootElement).find(node => (
    node.className === 'wwc-scope-selector-access'
  ))
  assert.equal(access.getAttribute('role'), 'alert')
  assert.match(access.textContent, /no longer authorized/iu)

  model.publish({ ...state('network-error'), error: { code: 'NETWORK_ERROR' } })
  const retry = descendants(rootElement).find(node => (
    node.className === 'wwc-scope-selector-retry'
  ))
  assert.equal(retry.hidden, false)
  retry.dispatchEvent({ type: 'click' })
  await new Promise(resolvePromise => setTimeout(resolvePromise, 0))
  assert.deepEqual(model.calls, [['retry']])
  page.close()
})

test('context access status updates in place without replacing Scope controls', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const model = modelFake()
  const page = mountScopeSelectorPage({
    root: rootElement,
    model,
    contextStatus: 'selection-required',
  })
  const controls = descendants(rootElement).filter(node => node.tagName === 'SELECT')
  const access = descendants(rootElement).find(node => (
    node.className === 'wwc-scope-selector-access'
  ))

  page.updateContextStatus('selected')

  assert.equal(access.hidden, true)
  assert.deepEqual(
    descendants(rootElement).filter(node => node.tagName === 'SELECT'),
    controls,
  )

  page.updateContextStatus('denied')

  assert.equal(access.hidden, false)
  assert.equal(access.getAttribute('role'), 'alert')
  assert.match(access.textContent, /no longer authorized/iu)
  assert.deepEqual(
    descendants(rootElement).filter(node => node.tagName === 'SELECT'),
    controls,
  )
  page.close()
})
