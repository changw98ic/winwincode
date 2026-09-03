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
  `Readiness application boundary did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const applicationModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/readiness-tests/application.js',
)).href}`)
const { mountWinWinCodeClient } = applicationModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const repositoryScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const NOW = '2026-09-03T08:30:00.000Z'

function session() {
  return {
    schemaVersion,
    expiresAt: '2099-09-03T00:00:00.000Z',
    actor,
    authorizedScopes: [repositoryScope],
  }
}

function response(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: { hasMore: false, nextCursor: null },
  }
}

function availabilityPage(request, ready) {
  return {
    kind: 'model_route_availability_page',
    scope: request.scope,
    requestPoolSource: {
      kind: 'project',
      organizationId: request.scope.organizationId,
      workspaceId: request.scope.workspaceId,
      projectId: request.scope.projectId,
    },
    requestPoolRevision: 1,
    settingsRevision: 1,
    settingsSource: request.scope,
    defaultProviderId: ready ? 'openai' : null,
    defaultModelId: ready ? 'gpt-5' : null,
    status: ready ? 'enabled' : 'disabled',
    reason: ready ? 'ready' : 'no_provider',
    items: ready
      ? [{
          route: {
            providerId: 'openai',
            modelId: 'gpt-5',
            credentialReferenceId: 'crd_00000000000000000000000001',
          },
          status: 'enabled',
          reason: 'ready',
          isDefault: true,
          providerDisplayName: 'OpenAI',
          modelDisplayName: 'GPT-5',
          contextWindowTokens: 400000,
          maxOutputTokens: 128000,
          reasoningEfforts: [],
          toolSupport: 'parallel',
          catalogSource: request.scope,
          catalogVersion: 1,
          credentialRotationVersion: 1,
          providerVersion: 1,
          modelVersion: 1,
        }]
      : [],
  }
}

function facadeFake() {
  let complete = false
  const queries = []
  const subscriptions = []
  const client = {
    queries,
    subscriptions,
    serverUrl: 'https://control.example/readiness-app',
    completeAllSteps() { complete = true },
    async restore() { return structuredClone(session()) },
    async login() { return structuredClone(session()) },
    async logout() {},
    async command() { throw new Error('not used') },
    async query(request) {
      queries.push(structuredClone(request))
      if (request.query === 'enterprise.organization.list') return response(request, {
        kind: 'enterprise_organization_page',
        snapshotRevision: 1,
        items: [{
          id: repositoryScope.organizationId,
          displayName: 'Acme',
          slug: 'acme',
          state: 'active',
          revision: 1,
          updatedAt: NOW,
        }],
      })
      if (request.query === 'enterprise.project.list') return response(request, {
        kind: 'enterprise_project_repository_page',
        snapshotRevision: 1,
        items: [{
          kind: 'project',
          projectId: repositoryScope.projectId,
          displayName: 'Core',
          repositoryCount: 1,
          state: 'active',
          revision: 1,
          updatedAt: NOW,
        }, {
          kind: 'repository',
          projectId: repositoryScope.projectId,
          repositoryId: repositoryScope.repositoryId,
          displayName: 'Server',
          defaultBranch: 'main',
          state: 'active',
          revision: 1,
          updatedAt: NOW,
        }],
      })
      if (request.query === 'settings.get') return response(request, {
        revision: 1,
        defaultModelRoute: null,
        workerConcurrencyLimit: 2,
      })
      if (request.query === 'model.route.availability.list') {
        return response(request, availabilityPage(request, complete))
      }
      if (request.query === 'credential.reference.list') return response(request, {
        kind: 'credential_reference_page',
        items: complete
          ? [{
              id: 'crd_00000000000000000000000001',
              displayName: 'Primary provider key',
              providerId: 'openai',
              secretState: 'available',
              revokedAt: null,
              lastRotatedAt: NOW,
              rotationVersion: 1,
              revision: 1,
              updatedAt: NOW,
            }]
          : [],
      })
      if (request.query === 'worker.list') return response(request, {
        kind: 'worker_page',
        items: complete
          ? [{
              id: 'wrk_00000000000000000000000001',
              state: 'enabled',
              capacity: 2,
              lastHeartbeatAt: NOW,
              revision: 2,
            }]
          : [],
      })
      if (request.query === 'session.list') return response(request, {
        kind: 'product_session_page',
        items: complete
          ? [{
              id: 'psn_00000000000000000000000001',
              title: 'First Chat',
              projectId: repositoryScope.projectId,
              repositoryId: repositoryScope.repositoryId,
              state: 'active',
              revision: 1,
              createdAt: NOW,
              updatedAt: NOW,
            }]
          : [],
      })
      if (request.query === 'delivery.list') return response(request, {
        kind: 'delivery_page',
        items: complete
          ? [{
              deliveryId: 'dlv_00000000000000000000000001',
              title: 'First Delivery',
              projectId: repositoryScope.projectId,
              repositoryId: repositoryScope.repositoryId,
              state: 'draft',
              revision: 1,
              createdAt: NOW,
              updatedAt: NOW,
            }]
          : [],
      })
      throw new Error(`unexpected query: ${request.query}`)
    },
    subscribe(options) {
      const handle = {
        cursor: null,
        closed: false,
        resume() {},
        reconnect() {},
        close() { this.closed = true },
      }
      subscriptions.push({ options, handle })
      return handle
    },
    close() {},
  }
  return client
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
  tabIndex = 0
  href = ''
  id = ''
  htmlFor = ''
  type = ''
  value = ''
  #ownText = ''

  get childNodes() { return this.children }
  get textContent() {
    return this.#ownText + this.children.map(child => child.textContent).join('')
  }
  set textContent(value) {
    this.#ownText = String(value)
    this.children = []
  }
  append(...children) { this.children.push(...children) }
  replaceChildren(...children) { this.children = [...children] }
  insertBefore(node, current) {
    this.children = this.children.filter(child => child !== node)
    const index = current === null ? -1 : this.children.indexOf(current)
    if (index < 0) this.children.push(node)
    else this.children.splice(index, 0, node)
  }
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
    return !event.defaultPrevented
  }
}

class FakeDocument {
  createElement(tagName) { return new FakeElement(this, tagName) }
}

class FakeWindow {
  constructor(hash) {
    this.location.hash = hash
    this.history.replaceState = (_state, _unused, url) => {
      const hashIndex = url.indexOf('#')
      this.location.hash = hashIndex < 0 ? '' : url.slice(hashIndex)
    }
  }

  location = { hash: '', pathname: '/', search: '' }
  history = { replaceState() {} }
  listeners = new Map()
  entropy = 0
  navigator = { onLine: true }
  crypto = {
    getRandomValues: value => {
      this.entropy += 1
      value.fill(this.entropy)
      return value
    },
  }

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

  emit(name) {
    for (const listener of this.listeners.get(name) ?? []) listener({})
  }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise(resolvePromise => setTimeout(resolvePromise, 10))
  }
  assert.fail(`timed out waiting for ${label}`)
}

function mount(hash) {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const browser = new FakeWindow(hash)
  const client = facadeFake()
  const application = mountWinWinCodeClient({
    root: rootElement,
    serverUrl: client.serverUrl,
    window: browser,
    controlPlane: client,
  })
  return { application, browser, client, rootElement }
}

function readinessSection(rootElement) {
  return descendants(rootElement).find(node => node.className === 'wwc-readiness') ?? null
}

function readinessItems(rootElement) {
  return descendants(rootElement).filter(node => node.className === 'wwc-readiness-item')
}

test('the shell mounts the first-run checklist and reuses the shared query cache', async () => {
  const fixture = mount('#/settings')
  await waitFor(() => fixture.application.authSession.state.status === 'signed-in', 'session')
  await waitFor(() => readinessSection(fixture.rootElement) !== null, 'checklist section')
  await waitFor(
    () => readinessSection(fixture.rootElement).textContent.includes('1 of 6 complete'),
    'checked checklist',
  )

  const items = Object.fromEntries(readinessItems(fixture.rootElement).map(node => [
    node.dataset.itemId,
    node,
  ]))
  assert.equal(items['repository-scope'].dataset.status, 'ready')
  assert.equal(items['model-route'].dataset.status, 'attention')
  const modelFix = descendants(items['model-route']).find(node => (
    node.className === 'wwc-readiness-fix'
  ))
  assert.match(modelFix.href, /^#\/settings\?organizationId=/u)
  assert.match(modelFix.href, /repositoryId=rep_00000000000000000000000001/u)

  for (const name of [
    'settings.get',
    'credential.reference.list',
    'model.route.availability.list',
    'worker.list',
    'session.list',
    'delivery.list',
  ]) {
    assert.equal(
      fixture.client.queries.filter(query => query.query === name).length,
      1,
      `${name} should be read once through the shared cache`,
    )
  }

  const serialized = JSON.stringify(readinessSection(fixture.rootElement).textContent)
  assert.equal(serialized.includes('crd_'), false)

  fixture.browser.location.hash = '#/strongflow'
  fixture.browser.emit('hashchange')
  await waitFor(
    () => descendants(fixture.rootElement).some(node => (
      typeof node.className === 'string'
      && node.className.split(' ').includes('wwc-strongflow')
    )),
    'strongflow create page',
  )
  await waitFor(() => readinessItems(fixture.rootElement).length === 6, 'checklist re-render')
  // The closed Settings feature discards its Scope snapshot by contract, so the
  // checklist re-reads once on the next surface; it must never poll per render.
  await new Promise(resolvePromise => setTimeout(resolvePromise, 120))
  for (const name of [
    'credential.reference.list',
    'model.route.availability.list',
    'worker.list',
    'session.list',
    'delivery.list',
  ]) {
    const reads = fixture.client.queries.filter(query => query.query === name).length
    assert.equal(
      reads <= 2,
      true,
      `${name} was read ${reads} times; the checklist must not poll per render`,
    )
  }
  fixture.application.close()
})

test('recheck after completing the steps reports ready and issues fresh reads', async () => {
  const fixture = mount('#/settings')
  await waitFor(
    () => readinessSection(fixture.rootElement)?.textContent.includes('1 of 6 complete') === true,
    'initial checklist',
  )
  assert.match(readinessSection(fixture.rootElement).textContent, /1 of 6 complete/u)

  fixture.client.completeAllSteps()
  const recheck = descendants(readinessSection(fixture.rootElement)).find(node => (
    node.className === 'wwc-readiness-recheck'
  ))
  recheck.dispatchEvent({ type: 'click' })
  await waitFor(
    () => readinessSection(fixture.rootElement).textContent.includes('6 of 6 complete'),
    'complete checklist',
  )
  assert.equal(
    fixture.client.queries.filter(query => query.query === 'settings.get').length,
    1,
    'recheck must not re-read unrelated queries',
  )
  assert.equal(
    fixture.client.queries.filter(query => query.query === 'session.list').length,
    2,
    'recheck re-reads session presence',
  )

  const toggle = descendants(readinessSection(fixture.rootElement)).find(node => (
    node.className === 'wwc-readiness-toggle'
  ))
  toggle.dispatchEvent({ type: 'click' })
  await waitFor(
    () => descendants(readinessSection(fixture.rootElement)).find(node => (
      node.className === 'wwc-readiness-items'
    ))?.hidden === true,
    'collapsed checklist',
  )
  fixture.application.close()
})

test('the local diagnostics page reopens a collapsed checklist', async () => {
  const fixture = mount('#/settings/runtime')
  await waitFor(() => readinessSection(fixture.rootElement) !== null, 'checklist section')
  await waitFor(
    () => descendants(fixture.rootElement).some(node => (
      node.className === 'wwc-local-operations'
    )),
    'local diagnostics page',
  )
  await waitFor(() => readinessItems(fixture.rootElement).length === 6, 'checklist items')

  const toggle = descendants(readinessSection(fixture.rootElement)).find(node => (
    node.className === 'wwc-readiness-toggle'
  ))
  toggle.dispatchEvent({ type: 'click' })
  await waitFor(
    () => descendants(readinessSection(fixture.rootElement)).find(node => (
      node.className === 'wwc-readiness-items'
    ))?.hidden === true,
    'collapsed checklist',
  )

  const open = descendants(fixture.rootElement).find(node => (
    node.className === 'wwc-local-readiness-open'
  ))
  assert.notEqual(open, undefined, 'diagnostics page needs the reopen entry')
  open.dispatchEvent({ type: 'click' })
  await waitFor(
    () => descendants(readinessSection(fixture.rootElement)).find(node => (
      node.className === 'wwc-readiness-items'
    ))?.hidden === false,
    'reopened checklist',
  )
  fixture.application.close()
})
