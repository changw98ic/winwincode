import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.client-repositories-tests.json',
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
  `Repositories area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/client-repositories-tests')
// Plain module paths keep one ControlPlaneClientError class identity across
// the facade, the view-models, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')
const repositoriesViewModelModule = await cachedModule('repositories-view-model.js')
const repositoriesPageModule = await cachedModule('repositories-page.js')
const applicationModule = await cachedModule('application.js')

const {
  ControlPlaneClientError,
  createControlPlaneClientDirectory,
} = facade
const { createRepositoriesViewModel } = repositoriesViewModelModule
const { mountRepositoriesPage } = repositoriesPageModule
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
const listPath = 'https://control.example/api/v1/repositories?clientId=123456789012'

function session() {
  return {
    schemaVersion,
    expiresAt: '2099-09-02T00:00:00.000Z',
    actor,
    authorizedScopes: [structuredClone(repositoryScope)],
  }
}

function repository(overrides = {}) {
  return {
    repositoryBindingId: 'rbd_00000000000000000000000001',
    displayName: 'WinWinCode',
    defaultBranch: 'main',
    headCommit: 'abc1234def5678abc1234def5678abc1234def56',
    dirtyState: 'clean',
    availability: 'available',
    ...overrides,
  }
}

function response(status, payload = '') {
  return {
    ok: status >= 200 && status < 300,
    status,
    async text() {
      return typeof payload === 'string' ? payload : JSON.stringify(payload)
    },
  }
}

function errorPayload(code, message, retryable = false) {
  return {
    schemaVersion,
    requestId: 'req_00000000000000000000000001',
    error: { code, message, retryable, details: {} },
  }
}

function baseClient(overrides = {}) {
  return {
    serverUrl: 'https://control.example',
    async restore() { return session() },
    async login() { return session() },
    async loginWithPassword() { return session() },
    async initializationStatus() { return { initialized: true } },
    async logout() {},
    async command() { throw new Error('not used') },
    async query() { throw new Error('not used') },
    subscribe() { throw new Error('not used') },
    close() {},
    ...overrides,
  }
}

function directoryFixture(transport, baseOverrides = {}) {
  return createControlPlaneClientDirectory({
    client: baseClient(baseOverrides),
    transport,
  })
}

test('facade reads the repository list for one Client and freezes the summaries', async () => {
  const requests = []
  const directory = directoryFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), init: structuredClone(init) })
      return response(200, {
        schemaVersion,
        repositories: [
          repository(),
          repository({
            repositoryBindingId: 'rbd_00000000000000000000000002',
            displayName: 'n0vel',
            defaultBranch: 'develop',
            headCommit: 'def4567890def4567890def4567890def4567890',
            dirtyState: 'dirty',
          }),
        ],
      })
    },
  })

  const repositories = await directory.listRepositories({ clientId: '123456789012' })

  assert.deepEqual(repositories, [
    repository(),
    repository({
      repositoryBindingId: 'rbd_00000000000000000000000002',
      displayName: 'n0vel',
      defaultBranch: 'develop',
      headCommit: 'def4567890def4567890def4567890def4567890',
      dirtyState: 'dirty',
    }),
  ])
  assert.equal(Object.isFrozen(repositories), true)
  assert.equal(Object.isFrozen(repositories[0]), true)
  assert.deepEqual(requests.map(request => [request.input, request.init.method]), [
    [listPath, 'GET'],
  ])
  const listRequest = requests[0]
  assert.equal(listRequest.init.credentials, 'include')
  assert.equal(listRequest.init.redirect, 'error')
  assert.equal(listRequest.init.cache, 'no-store')
  assert.equal(listRequest.init.referrerPolicy, 'no-referrer')
  assert.equal(listRequest.init.body, undefined)
})

test('facade rejects malformed repository list responses', async () => {
  let payload = { schemaVersion, repositories: [repository()] }
  let status = 200
  const directory = directoryFixture({
    async fetch(input, init) {
      assert.equal(init.method, 'GET')
      assert.equal(String(input), listPath)
      return response(status, payload)
    },
  })
  assert.deepEqual(await directory.listRepositories({ clientId: '123456789012' }), [
    repository(),
  ])
  payload = errorPayload('SERVICE_UNAVAILABLE', 'outage', true)
  status = 503
  await assert.rejects(
    directory.listRepositories({ clientId: '123456789012' }),
    error => error.code === 'SERVICE_UNAVAILABLE'
      && error.kind === 'server'
      && error.retryable === true,
  )
  status = 200
  for (const malformed of [
    { schemaVersion, repositories: 'nope' },
    { schemaVersion },
    { schemaVersion, repositories: [null] },
    { schemaVersion, repositories: [{ ...repository(), repositoryBindingId: '' }] },
    { schemaVersion, repositories: [{ ...repository(), displayName: '' }] },
    { schemaVersion, repositories: [{ ...repository(), defaultBranch: '' }] },
    { schemaVersion, repositories: [{ ...repository(), headCommit: '' }] },
    { schemaVersion, repositories: [{ ...repository(), dirtyState: 'tidy' }] },
    { schemaVersion, repositories: [{ ...repository(), availability: 'expired' }] },
  ]) {
    payload = malformed
    await assert.rejects(
      directory.listRepositories({ clientId: '123456789012' }),
      error => error.code === 'INVALID_REPOSITORY_LIST_RESPONSE',
    )
  }
  payload = { schemaVersion: 'winwincode/v0', repositories: [] }
  await assert.rejects(
    directory.listRepositories({ clientId: '123456789012' }),
    error => error.kind === 'version' && error.code === 'SCHEMA_VERSION_MISMATCH',
  )
  const offline = directoryFixture({
    async fetch() {
      throw new TypeError('network unreachable')
    },
  })
  await assert.rejects(
    offline.listRepositories({ clientId: '123456789012' }),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'network'
      && error.code === 'NETWORK_ERROR',
  )
})

test('facade validates the repository list input before any request exists', async () => {
  let requests = 0
  const directory = directoryFixture({
    async fetch() {
      requests += 1
      return response(200, { schemaVersion, repositories: [] })
    },
  })
  for (const clientId of ['', 'abc', '12 34', '/etc/123', '123456789012; x']) {
    await assert.rejects(
      directory.listRepositories({ clientId }),
      error => error.code === 'REPOSITORY_LIST_INPUT_INVALID',
    )
  }
  assert.equal(requests, 0)
})

test('directory reuses an injected facade that already implements the repository list', async () => {
  const attempts = []
  const directory = createControlPlaneClientDirectory({
    client: baseClient({
      async listRepositories(input) {
        attempts.push({ ...input })
        return [repository({ displayName: input.clientId })]
      },
    }),
    transport: {
      async fetch() {
        throw new Error('the repository list must stay on the injected seam')
      },
    },
  })
  assert.deepEqual(await directory.listRepositories({ clientId: ' 123456789012 ' }), [
    repository({ displayName: '123456789012' }),
  ])
  assert.deepEqual(attempts, [{ clientId: '123456789012' }])
})

function repositoriesClientFake(overrides = {}) {
  return {
    listCalls: [],
    failNext: false,
    repositories: [repository()],
    async listRepositories(input) {
      this.listCalls.push({ ...input })
      if (this.failNext) {
        throw new ControlPlaneClientError({
          kind: 'network',
          code: 'NETWORK_ERROR',
          message: 'unreachable',
          requestId: null,
          retryable: true,
        })
      }
      return this.repositories
    },
    ...overrides,
  }
}

test('repositories view-model selects a device, loads its list, and clears on null', async () => {
  const client = repositoriesClientFake()
  const model = createRepositoriesViewModel({ client })
  assert.equal(model.state.clientId, null)
  assert.equal(model.state.status, 'unloaded')

  await model.showDevice('123456789012')
  assert.equal(model.state.clientId, '123456789012')
  assert.equal(model.state.status, 'loaded')
  assert.deepEqual(model.state.repositories, [repository()])
  assert.deepEqual(client.listCalls, [{ clientId: '123456789012' }])

  await model.refresh()
  assert.deepEqual(client.listCalls, [
    { clientId: '123456789012' },
    { clientId: '123456789012' },
  ])

  const seen = []
  model.subscribe(state => seen.push(state.clientId))
  await model.showDevice(null)
  assert.equal(model.state.clientId, null)
  assert.equal(model.state.repositories.length, 0)
  assert.equal(model.state.status, 'unloaded')
  assert.deepEqual(seen, ['123456789012', null], 'clearing publishes the emptied state')
  model.close()
})

test('repositories view-model keeps shown cards across an unavailable read and never swaps devices', async () => {
  let calls = 0
  let release
  const model = createRepositoriesViewModel({
    client: {
      async listRepositories(input) {
        calls += 1
        if (calls >= 3) {
          await new Promise(resolvePromise => { release = resolvePromise })
        }
        if (calls === 2 || calls >= 3) {
          throw new ControlPlaneClientError({
            kind: 'network',
            code: 'NETWORK_ERROR',
            message: 'unreachable',
            requestId: null,
            retryable: true,
          })
        }
        return [repository()]
      },
    },
  })
  await model.showDevice('123456789012')
  assert.equal(model.state.status, 'loaded')

  const second = model.showDevice('123456789012')
  await second
  assert.equal(calls, 2, 'a repeated selection issues a fresh read')
  assert.equal(model.state.status, 'unavailable')
  assert.deepEqual(model.state.repositories, [repository()], 'a failed read keeps shown cards')

  // A different device selected while its own read hangs in flight: the shown
  // cards of the previous device must never bleed into the new selection.
  const third = model.showDevice('999999999999')
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  assert.equal(model.state.clientId, '999999999999')
  assert.equal(model.state.status, 'loading')
  assert.equal(model.state.repositories.length, 0, 'cards never cross devices')
  await model.showDevice('999999999999')
  assert.equal(calls, 3, 'a selection already in flight is never repeated')
  release()
  await third
  assert.equal(model.state.status, 'unavailable')
  assert.equal(model.state.repositories.length, 0)
  model.close()
})

class PageElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }
  parentNode = null
  attributes = new Map()
  children = []
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  tabIndex = 0
  id = ''
  type = ''
  #textContent = ''
  get childNodes() { return this.children }
  get textContent() { return this.#textContent }
  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }
  append(...children) {
    for (const child of children) child.parentNode = this
    this.children.push(...children)
  }
  replaceChildren(...children) {
    for (const child of this.children) child.parentNode = null
    for (const child of children) child.parentNode = this
    this.children = [...children]
  }
  insertBefore(node, current) {
    this.children = this.children.filter(child => child !== node)
    const index = current === null ? -1 : this.children.indexOf(current)
    if (index < 0) this.children.push(node)
    else this.children.splice(index, 0, node)
    node.parentNode = this
  }
  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }
  remove() {
    const parent = this.parentNode
    if (parent !== null) parent.children = parent.children.filter(child => child !== this)
    this.parentNode = null
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
  dispatchEvent(event) {
    for (const listener of this.listeners.get(event.type) ?? []) listener(event)
    return !event.defaultPrevented
  }
  emit(type, event = {}) { this.dispatchEvent({ type, ...event }) }
}

class PageDocument {
  createElement(tagName) { return new PageElement(this, tagName) }
}

function pageDescendants(node) {
  return [node, ...node.children.flatMap(child => pageDescendants(child))]
}

function hasClass(node, className) {
  return node.className.split(/\s+/u).includes(className)
}

function findOne(rootElement, className) {
  const node = pageDescendants(rootElement).find(candidate => hasClass(candidate, className))
  assert.notEqual(node, undefined, `${className} is mounted`)
  return node
}

function findAll(rootElement, className) {
  return pageDescendants(rootElement).filter(candidate => hasClass(candidate, className))
}

function repositoriesFixture({ repositories = [repository()] } = {}) {
  const document = new PageDocument()
  const rootElement = new PageElement(document, 'div')
  const client = repositoriesClientFake({ repositories })
  const model = createRepositoriesViewModel({ client })
  const page = mountRepositoriesPage({ root: rootElement, model })
  page.setVisible(true)
  return { rootElement, model, page, client }
}

function renderedText(rootElement) {
  return pageDescendants(rootElement).map(node => node.textContent).join('\n')
}

test('repositories page renders every card field and never renders a path', async () => {
  const { rootElement, model } = repositoriesFixture({
    repositories: [
      repository(),
      repository({
        repositoryBindingId: 'rbd_00000000000000000000000002',
        displayName: 'n0vel',
        defaultBranch: 'develop',
        headCommit: 'def4567890def4567890def4567890def4567890',
        dirtyState: 'dirty',
      }),
    ],
  })
  await model.showDevice('123456789012')
  const cards = findAll(rootElement, 'wwc-repositories-card')
  assert.equal(cards.length, 2)

  assert.equal(findOne(cards[0], 'wwc-repositories-card-name').textContent, 'WinWinCode')
  assert.equal(findOne(cards[0], 'wwc-repositories-card-branch').textContent, 'main')
  assert.equal(findOne(cards[0], 'wwc-repositories-card-dirty').textContent, 'Clean')
  assert.equal(findOne(cards[0], 'wwc-repositories-card-dirty').dataset.tone, 'success')
  assert.equal(findOne(cards[0], 'wwc-repositories-card-head').textContent, 'HEAD abc1234')
  assert.equal(findOne(cards[0], 'wwc-repositories-card-availability').hidden, true)

  assert.equal(findOne(cards[1], 'wwc-repositories-card-name').textContent, 'n0vel')
  assert.equal(findOne(cards[1], 'wwc-repositories-card-branch').textContent, 'develop')
  assert.equal(findOne(cards[1], 'wwc-repositories-card-dirty').textContent, 'Dirty')
  assert.equal(findOne(cards[1], 'wwc-repositories-card-dirty').dataset.tone, 'warning')
  assert.equal(findOne(cards[1], 'wwc-repositories-card-head').textContent, 'HEAD def4567')
  assert.equal(findOne(cards[1], 'wwc-repositories-card-availability').hidden, true)

  // §16.5: the card surface carries display names, branches, dirty state, and
  // the short HEAD hash only; no absolute path appears anywhere.
  assert.doesNotMatch(renderedText(rootElement), /\/Users\/|\/home\/|\/Volumes\//u)
  model.close()
})

test('repositories page shows the availability reason badge for every non-available state', async () => {
  const cases = [
    { availability: 'dirty', text: 'Not usable: the working tree is dirty', tone: 'warning' },
    { availability: 'unavailable', text: 'Repository unavailable', tone: 'danger' },
    { availability: 'moved', text: 'Repository moved on the device', tone: 'warning' },
    { availability: 'invalid_git', text: 'Not a valid Git repository', tone: 'danger' },
    { availability: 'permission_denied', text: 'Access denied on the device', tone: 'danger' },
    { availability: 'scan_failed', text: 'The last repository scan failed', tone: 'warning' },
  ]
  for (const candidate of cases) {
    const { rootElement, model } = repositoriesFixture({
      repositories: [repository({ availability: candidate.availability })],
    })
    await model.showDevice('123456789012')
    const availability = findOne(rootElement, 'wwc-repositories-card-availability')
    assert.equal(availability.hidden, false, `${candidate.availability} shows its reason badge`)
    assert.equal(availability.textContent, candidate.text)
    assert.equal(availability.dataset.tone, candidate.tone)
    assert.doesNotMatch(renderedText(rootElement), /\/Users\/|\/home\/|\/Volumes\//u)
    model.close()
  }
  const available = repositoriesFixture()
  await available.model.showDevice('123456789012')
  assert.equal(
    findOne(available.rootElement, 'wwc-repositories-card-availability').hidden,
    true,
    'the available state renders no badge',
  )
  available.model.close()
})

test('repositories page renders the selection hint, the empty copy, and the unavailable alert', async () => {
  const { rootElement, model, page, client } = repositoriesFixture({ repositories: [] })
  const hint = findOne(rootElement, 'wwc-repositories-hint')
  const empty = findOne(rootElement, 'wwc-repositories-empty')
  const error = findOne(rootElement, 'wwc-repositories-error')

  assert.equal(hint.hidden, false)
  assert.equal(hint.textContent, 'Select a Client above to see its repositories.')
  assert.equal(empty.hidden, true)
  assert.equal(error.hidden, true)

  await model.showDevice('123456789012')
  assert.equal(model.state.status, 'loaded')
  assert.equal(hint.hidden, true)
  assert.equal(empty.hidden, false)
  assert.equal(empty.textContent, 'No repositories are authorized for this Client yet.')
  assert.equal(error.hidden, true)
  assert.deepEqual(client.listCalls, [{ clientId: '123456789012' }])

  client.failNext = true
  await model.refresh()
  assert.equal(model.state.status, 'unavailable')
  assert.equal(error.hidden, false)
  assert.equal(
    error.textContent,
    'Listing repositories is unavailable right now. Check the connection and try again.',
  )
  assert.equal(empty.hidden, true, 'an unavailable read never claims the empty state')
  assert.equal(findAll(rootElement, 'wwc-repositories-card').length, 0)

  page.close()
  assert.deepEqual(rootElement.children, [])
  model.close()
})

class ApplicationElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }
  parentNode = null
  attributes = new Map()
  children = []
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  tabIndex = 0
  id = ''
  type = ''
  value = ''
  #textContent = ''
  get childNodes() { return this.children }
  get textContent() { return this.#textContent }
  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }
  append(...children) {
    for (const child of children) child.parentNode = this
    this.children.push(...children)
  }
  replaceChildren(...children) {
    for (const child of this.children) child.parentNode = null
    for (const child of children) child.parentNode = this
    this.children = [...children]
  }
  insertBefore(node, current) {
    this.children = this.children.filter(child => child !== node)
    const index = current === null ? -1 : this.children.indexOf(current)
    if (index < 0) this.children.push(node)
    else this.children.splice(index, 0, node)
    node.parentNode = this
  }
  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }
  remove() {
    const parent = this.parentNode
    if (parent !== null) parent.children = parent.children.filter(child => child !== this)
    this.parentNode = null
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
  dispatchEvent(event) {
    for (const listener of this.listeners.get(event.type) ?? []) listener(event)
    return !event.defaultPrevented
  }
}

class ApplicationDocument {
  createElement(tagName) { return new ApplicationElement(this, tagName) }
}

class ApplicationWindow {
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

function repositoriesFacadeFake({ expired = true, repositories = [repository()] } = {}) {
  let sessionExpired = expired
  const queries = []
  const repositoryCalls = []
  const client = {
    queries,
    repositoryCalls,
    serverUrl: 'https://control.example/clients-app',
    setExpired(next) { sessionExpired = next },
    async restore() {
      if (sessionExpired) {
        throw new ControlPlaneClientError({
          kind: 'authentication',
          code: 'AUTHENTICATION_REQUIRED',
          message: 'The browser session is missing.',
          requestId: null,
          retryable: false,
        })
      }
      return structuredClone(session())
    },
    async login() {
      return structuredClone(session())
    },
    async loginWithPassword() {
      return structuredClone(session())
    },
    async initializationStatus() {
      return { initialized: true }
    },
    async logout() {},
    async command() { throw new Error('not used') },
    async query(request) {
      queries.push(structuredClone(request))
      const base = {
        schemaVersion,
        requestId: request.requestId,
        query: request.query,
        page: { hasMore: false, nextCursor: null },
      }
      if (request.query === 'settings.get') {
        return { ...base, result: { revision: 1, defaultModelRoute: null, workerConcurrencyLimit: 2 } }
      }
      if (request.query === 'credential.reference.list') {
        return { ...base, result: { kind: 'credential_reference_page', items: [] } }
      }
      if (request.query === 'delivery.list') {
        return { ...base, result: { kind: 'delivery_page', items: [] } }
      }
      throw new Error(`unexpected query ${request.query}`)
    },
    async addClient() {
      return [repository()]
    },
    async listClients() {
      return [{
        clientId: '123456789012',
        displayName: 'Wenjie MacBook Pro',
        presence: 'online',
        occupancy: 'available',
        capacityUsed: 3,
        capacityTotal: 8,
        lastHeartbeatAt: '2026-09-04T00:00:00.000Z',
        version: '1.2.3',
      }]
    },
    async listRepositories(input) {
      repositoryCalls.push({ ...input })
      return structuredClone(repositories)
    },
    subscribe(options) {
      return {
        cursor: null,
        resume() {},
        reconnect() {},
        close() {},
      }
    },
    close() {},
  }
  return client
}

function mountApplication(hash, client) {
  const document = new ApplicationDocument()
  const rootElement = new ApplicationElement(document, 'div')
  const browser = new ApplicationWindow(hash)
  const application = mountWinWinCodeClient({
    root: rootElement,
    serverUrl: client.serverUrl,
    window: browser,
    controlPlane: client,
  })
  return { application, browser, rootElement, client }
}

function applicationDescendants(node) {
  return [node, ...node.children.flatMap(child => applicationDescendants(child))]
}

function applicationNode(rootElement, className) {
  const node = applicationDescendants(rootElement).find(
    candidate => candidate.className.split(/\s+/u).includes(className),
  )
  assert.notEqual(node, undefined, `${className} is mounted`)
  return node
}

function applicationFindAll(rootElement, className) {
  return applicationDescendants(rootElement).filter(
    candidate => candidate.className.split(/\s+/u).includes(className),
  )
}

function waitFor(predicate, label) {
  return (async () => {
    const deadline = Date.now() + 5_000
    while (Date.now() < deadline) {
      if (predicate()) return
      await new Promise(resolvePromise => setTimeout(resolvePromise, 10))
    }
    assert.fail(`timed out waiting for ${label}`)
  })()
}

test('the signed-in repository area follows the selected device card and hides on sign-out', async () => {
  const client = repositoriesFacadeFake({ expired: true })
  const fixture = mountApplication('#/settings', client)
  await waitFor(
    () => fixture.application.authSession.state.status === 'authentication-required',
    'expired session surfaced',
  )
  await waitFor(
    () => applicationNode(fixture.rootElement, 'wwc-repositories').hidden === true,
    'the repository area stays hidden while signed out',
  )

  client.setExpired(false)
  const authSession = fixture.application.authSession
  await authSession.restore()
  await waitFor(
    () => applicationNode(fixture.rootElement, 'wwc-repositories').hidden === false,
    'the repository area opens for a signed-in user',
  )
  const hint = applicationNode(fixture.rootElement, 'wwc-repositories-hint')
  assert.equal(hint.hidden, false, 'the unselected area explains itself')
  assert.equal(applicationFindAll(fixture.rootElement, 'wwc-repositories-card').length, 0)
  assert.equal(client.repositoryCalls.length, 0, 'no repository read happens before a selection')

  await waitFor(
    () => applicationFindAll(fixture.rootElement, 'wwc-clients-card').length === 1,
    'the device card list rendered',
  )
  applicationNode(fixture.rootElement, 'wwc-clients-card-select').dispatchEvent({
    type: 'click',
  })
  await waitFor(
    () => applicationFindAll(fixture.rootElement, 'wwc-repositories-card').length === 1,
    'the selected device renders its repository card',
  )
  assert.deepEqual(client.repositoryCalls, [{ clientId: '123456789012' }])
  const card = applicationNode(fixture.rootElement, 'wwc-repositories-card')
  assert.equal(applicationNode(fixture.rootElement, 'wwc-repositories-card-name').textContent,
    'WinWinCode')
  assert.equal(applicationNode(fixture.rootElement, 'wwc-repositories-card-branch').textContent,
    'main')
  assert.equal(applicationNode(fixture.rootElement, 'wwc-repositories-card-dirty').textContent,
    'Clean')
  assert.equal(applicationNode(fixture.rootElement, 'wwc-repositories-card-head').textContent,
    'HEAD abc1234')
  assert.equal(
    applicationNode(fixture.rootElement, 'wwc-repositories-card-availability').hidden,
    true,
  )
  assert.equal(
    applicationNode(fixture.rootElement, 'wwc-clients-card-select').getAttribute('aria-pressed'),
    'true',
  )
  const rendered = applicationDescendants(fixture.rootElement)
    .map(node => node.textContent)
    .join('\n')
  assert.doesNotMatch(rendered, /\/Users\/|\/home\/|\/Volumes\//u)

  applicationNode(fixture.rootElement, 'wwc-clients-card-select').dispatchEvent({
    type: 'click',
  })
  await waitFor(
    () => applicationNode(fixture.rootElement, 'wwc-repositories-hint').hidden === false,
    'toggling the selection off returns the hint',
  )
  assert.equal(applicationFindAll(fixture.rootElement, 'wwc-repositories-card').length, 0)
  assert.equal(client.repositoryCalls.length, 1, 'clearing the selection issues no read')

  await authSession.logout()
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-out',
    'logout completes',
  )
  await waitFor(
    () => applicationNode(fixture.rootElement, 'wwc-repositories').hidden === true,
    'the repository area hides after sign-out',
  )
  fixture.application.close()
  assert.deepEqual(fixture.rootElement.children, [])
})
