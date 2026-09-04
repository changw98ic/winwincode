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
  `Scope selector application boundary did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const applicationModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/scope-selector-tests/application.js',
)).href}`)
const { mountWinWinCodeClient } = applicationModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const repositoryOne = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const repositoryTwo = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000002',
  workspaceId: 'wsp_00000000000000000000000002',
  projectId: 'prj_00000000000000000000000002',
  repositoryId: 'rep_00000000000000000000000002',
}

function session(scopes) {
  return {
    schemaVersion,
    expiresAt: '2099-09-02T00:00:00.000Z',
    actor,
    authorizedScopes: scopes,
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

function facadeFake(initialSession) {
  let currentSession = initialSession
  let delayDeliveryList = false
  const queries = []
  const subscriptions = []
  const client = {
    queries,
    subscriptions,
    serverUrl: 'https://control.example/scope-selector',
    setSession(next) { currentSession = next },
    delayNextDeliveryList() { delayDeliveryList = true },
    async restore() { return structuredClone(currentSession) },
    async login() { return structuredClone(currentSession) },
    async logout() {},
    async command() { throw new Error('not used') },
    async query(request, options) {
      queries.push({ request: structuredClone(request), signal: options?.signal })
      if (request.query === 'delivery.list' && delayDeliveryList) {
        delayDeliveryList = false
        return new Promise((resolvePromise, rejectPromise) => {
          options.signal.addEventListener('abort', () => {
            rejectPromise(new DOMException('aborted', 'AbortError'))
          }, { once: true })
        })
      }
      if (request.query === 'enterprise.organization.list') return response(request, {
        kind: 'enterprise_organization_page',
        snapshotRevision: 1,
        items: [repositoryOne, repositoryTwo].map((scope, index) => ({
          id: scope.organizationId,
          displayName: index === 0 ? 'Acme' : 'Beta',
          slug: index === 0 ? 'acme' : 'beta',
          state: 'active',
          revision: 1,
          updatedAt: '2026-09-02T00:00:00.000Z',
        })),
      })
      if (request.query === 'enterprise.project.list') {
        const scope = request.scope.organizationId === repositoryOne.organizationId
          ? repositoryOne
          : repositoryTwo
        return response(request, {
          kind: 'enterprise_project_repository_page',
          snapshotRevision: 1,
          items: [{
            kind: 'project',
            projectId: scope.projectId,
            displayName: `${scope.projectId} name`,
            repositoryCount: 1,
            state: 'active',
            revision: 1,
            updatedAt: '2026-09-02T00:00:00.000Z',
          }, {
            kind: 'repository',
            projectId: scope.projectId,
            repositoryId: scope.repositoryId,
            displayName: `${scope.repositoryId} name`,
            defaultBranch: 'main',
            state: 'active',
            revision: 1,
            updatedAt: '2026-09-02T00:00:00.000Z',
          }],
        })
      }
      if (request.query === 'settings.get') return response(request, {
        revision: 1,
        defaultModelRoute: null,
        workerConcurrencyLimit: 2,
      })
      if (request.query === 'credential.reference.list') return response(request, {
        kind: 'credential_reference_page',
        items: [],
      })
      if (request.query === 'delivery.list') return response(request, {
        kind: 'delivery_page',
        items: [],
      })
      throw new Error(`unexpected query ${request.query}`)
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
  #textContent = ''
  get childNodes() { return this.children }
  get textContent() { return this.#textContent }
  set textContent(value) {
    this.#textContent = String(value)
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
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function scopeControls(rootElement) {
  return Object.fromEntries(
    descendants(rootElement)
      .filter(node => node.className === 'wwc-scope-selector-control')
      .map(node => [node.id.replace('wwc-scope-', ''), node]),
  )
}

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise(resolvePromise => setTimeout(resolvePromise, 10))
  }
  assert.fail(`timed out waiting for ${label}`)
}

function mount(hash, client) {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const browser = new FakeWindow(hash)
  const application = mountWinWinCodeClient({
    root: rootElement,
    serverUrl: client.serverUrl,
    window: browser,
    controlPlane: client,
  })
  return { application, browser, client, rootElement }
}

function choose(rootElement, level, value) {
  const control = scopeControls(rootElement)[level]
  assert.notEqual(control, undefined, `${level} control is mounted`)
  control.value = value
  control.dispatchEvent({ type: 'change' })
}

test('multi-Scope cascade blocks product reads until one exact repository is selected', async () => {
  const fixture = mount('#/settings', facadeFake(session([repositoryOne, repositoryTwo])))
  await waitFor(() => fixture.application.authSession.state.status === 'signed-in', 'session')
  await waitFor(() => Object.keys(scopeControls(fixture.rootElement)).length === 4, 'selector')
  assert.equal(
    fixture.client.queries.some(call => call.request.query === 'settings.get'),
    false,
  )

  choose(fixture.rootElement, 'organization', repositoryTwo.organizationId)
  await waitFor(() => fixture.browser.location.hash.includes(repositoryTwo.organizationId), 'organization URL')
  choose(fixture.rootElement, 'workspace', repositoryTwo.workspaceId)
  await waitFor(() => fixture.browser.location.hash.includes(repositoryTwo.workspaceId), 'workspace URL')
  choose(fixture.rootElement, 'project', repositoryTwo.projectId)
  await waitFor(() => fixture.browser.location.hash.includes(repositoryTwo.projectId), 'project URL')
  choose(fixture.rootElement, 'repository', repositoryTwo.repositoryId)
  await waitFor(
    () => fixture.client.queries.some(call => call.request.query === 'settings.get'),
    'repository-scoped product read',
  )
  const productReads = fixture.client.queries.filter(call => (
    call.request.query === 'settings.get' || call.request.query === 'credential.reference.list'
  ))
  assert.equal(productReads.every(call => (
    call.request.scope.repositoryId === repositoryTwo.repositoryId
  )), true)
  assert.match(fixture.browser.location.hash, /organizationId=.*workspaceId=.*projectId=.*repositoryId=/u)
  fixture.application.close()
})

test('switching an active Scope closes old subscriptions before the next context can load', async () => {
  const hash = '#/settings'
    + `?organizationId=${repositoryOne.organizationId}`
    + `&workspaceId=${repositoryOne.workspaceId}`
    + `&projectId=${repositoryOne.projectId}`
    + `&repositoryId=${repositoryOne.repositoryId}`
  const fixture = mount(hash, facadeFake(session([repositoryOne, repositoryTwo])))
  await waitFor(() => fixture.client.subscriptions.length > 0, 'settings subscription')
  const oldSubscription = fixture.client.subscriptions.at(-1)
  const mountedControls = scopeControls(fixture.rootElement)

  choose(fixture.rootElement, 'organization', repositoryTwo.organizationId)

  await waitFor(() => oldSubscription.handle.closed, 'old subscription close')
  const currentControls = scopeControls(fixture.rootElement)
  assert.equal(currentControls.organization, mountedControls.organization)
  assert.equal(currentControls.workspace, mountedControls.workspace)
  assert.equal(currentControls.project, mountedControls.project)
  assert.equal(currentControls.repository, mountedControls.repository)
  assert.equal(
    fixture.client.queries.filter(call => call.request.query === 'settings.get').length,
    1,
  )
  assert.equal(
    descendants(fixture.rootElement).some(node => node.className === 'wwc-settings-page'),
    false,
  )
  fixture.application.close()
})

test('switching Scope aborts an in-flight old repository route request', async () => {
  const hash = '#/strongflow'
    + `?organizationId=${repositoryOne.organizationId}`
    + `&workspaceId=${repositoryOne.workspaceId}`
    + `&projectId=${repositoryOne.projectId}`
    + `&repositoryId=${repositoryOne.repositoryId}`
  const client = facadeFake(session([repositoryOne, repositoryTwo]))
  client.delayNextDeliveryList()
  const fixture = mount(hash, client)
  await waitFor(
    () => fixture.client.queries.some(call => call.request.query === 'delivery.list'),
    'in-flight Delivery list',
  )
  const oldRead = fixture.client.queries.find(call => call.request.query === 'delivery.list')
  assert.equal(oldRead.signal.aborted, false)

  choose(fixture.rootElement, 'organization', repositoryTwo.organizationId)

  await waitFor(() => oldRead.signal.aborted, 'old route cancellation')
  assert.equal(
    descendants(fixture.rootElement).some(node => node.className === 'wwc-strongflow-page'),
    false,
  )
  fixture.application.close()
})

test('WebSocket revocation keeps the stale URL Scope denied across navigation', async () => {
  const hash = '#/settings'
    + `?organizationId=${repositoryOne.organizationId}`
    + `&workspaceId=${repositoryOne.workspaceId}`
    + `&projectId=${repositoryOne.projectId}`
    + `&repositoryId=${repositoryOne.repositoryId}`
  const fixture = mount(hash, facadeFake(session([repositoryOne, repositoryTwo])))
  await waitFor(() => fixture.client.subscriptions.length > 0, 'settings subscription')
  const subscription = fixture.client.subscriptions.at(-1)
  const readsBefore = fixture.client.queries.length

  await subscription.options.onAuthorizationRevoked(null)
  fixture.application.navigate('chat')

  await waitFor(() => descendants(fixture.rootElement).some(node => (
    node.className === 'wwc-scope-selector-access'
    && node.getAttribute('role') === 'alert'
  )), 'revoked Scope refusal')
  assert.equal(subscription.handle.closed, true)
  assert.equal(fixture.client.queries.length, readsBefore)
  assert.equal(
    descendants(fixture.rootElement).some(node => node.className === 'wwc-chat'),
    false,
  )
  fixture.application.close()
})

test('refresh restores an exact URL Scope and later AuthSession revocation fails closed', async () => {
  const hash = '#/settings'
    + `?organizationId=${repositoryTwo.organizationId}`
    + `&workspaceId=${repositoryTwo.workspaceId}`
    + `&projectId=${repositoryTwo.projectId}`
    + `&repositoryId=${repositoryTwo.repositoryId}`
  const client = facadeFake(session([repositoryOne, repositoryTwo]))
  const fixture = mount(hash, client)
  await waitFor(() => fixture.client.subscriptions.length > 0, 'restored settings subscription')
  const selectedRead = fixture.client.queries.find(call => call.request.query === 'settings.get')
  assert.equal(selectedRead.request.scope.repositoryId, repositoryTwo.repositoryId)
  const oldSubscription = fixture.client.subscriptions.at(-1)

  client.setSession(session([repositoryOne]))
  await fixture.application.authSession.restore()
  await waitFor(() => oldSubscription.handle.closed, 'revoked Scope cleanup')
  const access = descendants(fixture.rootElement).find(node => (
    node.className === 'wwc-scope-selector-access'
  ))
  assert.equal(access.getAttribute('role'), 'alert')
  assert.match(access.textContent, /no longer authorized/iu)
  assert.equal(
    fixture.client.queries.filter(call => call.request.query === 'settings.get').length,
    1,
  )
  fixture.application.close()
})
