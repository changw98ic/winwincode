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
    'apps/client/tsconfig.client-login-tests.json',
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
  `login Client did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/client-login-tests')
// Plain module paths keep one ControlPlaneClientError class identity across
// the facade, the view-models, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')
const loginViewModelModule = await cachedModule('login-view-model.js')
const loginPageModule = await cachedModule('login-page.js')
const applicationModule = await cachedModule('application.js')

const {
  ControlPlaneClientError,
  controlPlaneLoginFailure,
  createControlPlaneClient,
} = facade
const { createLoginViewModel } = loginViewModelModule
const { mountLoginPage } = loginPageModule
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
const passwordMaterial = 'correct-horse-battery-staple'

function session() {
  return {
    schemaVersion,
    expiresAt: '2099-09-02T00:00:00.000Z',
    actor,
    authorizedScopes: [structuredClone(repositoryScope)],
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

function loginError(code, message, retryable = false) {
  return new ControlPlaneClientError({
    kind: code === 'AUTHENTICATION_REQUIRED' ? 'authentication' : 'server',
    code,
    message,
    requestId: null,
    retryable,
  })
}

test('facade exchanges username and password for one validated session', async () => {
  const requests = []
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/auth',
    transport: {
      async fetch(input, init) {
        requests.push({ input: String(input), init: structuredClone(init) })
        return response(201, session())
      },
    },
  })

  const created = await client.loginWithPassword({
    username: 'ada',
    password: passwordMaterial,
  })

  assert.deepEqual(created, session())
  assert.deepEqual(requests.map(request => [request.input, request.init.method]), [
    ['https://control.example/auth/api/v1/auth/session', 'POST'],
  ])
  const loginRequest = requests[0]
  assert.equal(loginRequest.init.credentials, 'include')
  assert.equal(loginRequest.init.redirect, 'error')
  assert.equal(loginRequest.init.cache, 'no-store')
  assert.equal(loginRequest.init.referrerPolicy, 'no-referrer')
  assert.equal(loginRequest.init.headers.Authorization, undefined)
  assert.deepEqual(JSON.parse(loginRequest.init.body), {
    schemaVersion,
    username: 'ada',
    password: passwordMaterial,
  })
  assert.equal(JSON.stringify(created).includes(passwordMaterial), false)
})

test('facade separates wrong credentials, rate limit, wrong state, and outage', async () => {
  const cases = [
    { status: 401, code: 'AUTHENTICATION_REQUIRED', kind: 'authentication', failure: 'invalid-credentials' },
    { status: 429, code: 'RATE_LIMITED', kind: 'server', failure: 'rate-limited', retryable: true },
    { status: 409, code: 'WRONG_STATE', kind: 'protocol', failure: 'unavailable' },
    { status: 503, code: 'SERVICE_UNAVAILABLE', kind: 'server', failure: 'unavailable', retryable: true },
  ]
  for (const candidate of cases) {
    const client = createControlPlaneClient({
      serverUrl: 'https://control.example',
      transport: {
        async fetch() {
          return response(candidate.status, errorPayload(
            candidate.code,
            'sign-in rejected',
            candidate.retryable === true,
          ))
        },
      },
    })
    await assert.rejects(
      client.loginWithPassword({ username: 'ada', password: passwordMaterial }),
      error => {
        assert.equal(error instanceof ControlPlaneClientError, true)
        assert.equal(error.code, candidate.code)
        assert.equal(error.kind, candidate.kind)
        assert.equal(error.retryable, candidate.retryable === true)
        assert.equal(controlPlaneLoginFailure(error), candidate.failure)
        assert.equal(JSON.stringify(error).includes(passwordMaterial), false)
        return true
      },
    )
  }
  const offline = createControlPlaneClient({
    serverUrl: 'https://control.example',
    transport: {
      async fetch() {
        throw new TypeError('network unreachable')
      },
    },
  })
  await assert.rejects(
    offline.loginWithPassword({ username: 'ada', password: passwordMaterial }),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'network'
      && controlPlaneLoginFailure(error) === 'unavailable',
  )
})

test('facade validates sign-in input before any request exists', async () => {
  let requests = 0
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example',
    transport: {
      async fetch() {
        requests += 1
        return response(201, session())
      },
    },
  })
  await assert.rejects(
    client.loginWithPassword({ username: '', password: passwordMaterial }),
    error => error.code === 'LOGIN_INPUT_INVALID',
  )
  await assert.rejects(
    client.loginWithPassword({ username: 'ada', password: '' }),
    error => error.code === 'LOGIN_INPUT_INVALID',
  )
  assert.equal(requests, 0)
})

test('facade reads initialization and rejects malformed initialization status', async () => {
  let payload = { schemaVersion, initialized: false }
  let status = 200
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example',
    transport: {
      async fetch(input, init) {
        assert.equal(init.method, 'GET')
        assert.equal(String(input).endsWith('/api/v1/server/initialization'), true)
        assert.equal(init.credentials, 'include')
        return status === 200 ? response(200, payload) : response(status, payload)
      },
    },
  })
  assert.deepEqual(await client.initializationStatus(), { initialized: false })
  payload = { schemaVersion, initialized: true }
  assert.deepEqual(await client.initializationStatus(), { initialized: true })
  payload = errorPayload('RESOURCE_NOT_FOUND', 'missing route')
  status = 404
  await assert.rejects(
    client.initializationStatus(),
    error => error.code === 'RESOURCE_NOT_FOUND'
      && controlPlaneLoginFailure(error) === 'unavailable',
  )
  status = 200
  payload = { schemaVersion, initialized: 'yes' }
  await assert.rejects(
    client.initializationStatus(),
    error => error.code === 'INVALID_SERVER_INITIALIZATION_RESPONSE',
  )
  payload = { schemaVersion: 'winwincode/v0', initialized: true }
  await assert.rejects(
    client.initializationStatus(),
    error => error.kind === 'version' && error.code === 'SCHEMA_VERSION_MISMATCH',
  )
})

test('login view-model publishes submissions and failure reasons without storing secrets', async () => {
  const attempts = []
  const client = {
    async loginWithPassword(credentials) {
      attempts.push({ username: credentials.username, password: credentials.password })
      throw loginError('AUTHENTICATION_REQUIRED', 'wrong password')
    },
    async login() {
      return session()
    },
    async initializationStatus() {
      return { initialized: true }
    },
  }
  const model = createLoginViewModel({ client })
  const seen = []
  model.subscribe(state => seen.push(state))
  assert.equal(model.state.status, 'idle')
  assert.equal(model.state.initialization, 'unknown')

  await model.login({ username: 'ada', password: passwordMaterial })

  assert.equal(model.state.status, 'idle')
  assert.equal(model.state.failure, 'invalid-credentials')
  assert.deepEqual(attempts, [{ username: 'ada', password: passwordMaterial }])
  assert.equal(JSON.stringify(seen).includes(passwordMaterial), false)

  model.dismissFailure()
  assert.equal(model.state.failure, null)

  let release
  client.loginWithPassword = async credentials => {
    attempts.push({ username: credentials.username, password: credentials.password })
    await new Promise(resolvePromise => { release = resolvePromise })
    return session()
  }
  client.initializationStatus = async () => ({ initialized: false })
  await model.refreshInitialization()
  assert.equal(model.state.initialization, 'uninitialized')
  const operation = model.login({ username: 'ada', password: passwordMaterial })
  assert.equal(model.state.status, 'submitting')
  assert.equal(model.state.source, 'sign-in')
  assert.equal(model.state.failure, null)
  release()
  await operation
  assert.equal(model.state.status, 'succeeded')
  assert.equal(model.state.failure, null)
  assert.equal(JSON.stringify(model.state).includes(passwordMaterial), false)
  model.close()
})

test('login view-model keeps rate limit, wrong credentials, and probe failures distinct', async () => {
  for (const [code, expected] of [
    ['RATE_LIMITED', 'rate-limited'],
    ['AUTHENTICATION_REQUIRED', 'invalid-credentials'],
  ]) {
    const model = createLoginViewModel({
      client: {
        async loginWithPassword() {
          throw loginError(code, 'rejected')
        },
        async login() { return session() },
        async initializationStatus() { return { initialized: true } },
      },
    })
    await model.login({ username: 'ada', password: passwordMaterial })
    assert.equal(model.state.failure, expected)
    assert.equal(model.state.status, 'idle')
    model.close()
  }
  const model = createLoginViewModel({
    client: {
      async loginWithPassword() { return session() },
      async login() { return session() },
      async initializationStatus() {
        throw new ControlPlaneClientError({
          kind: 'network',
          code: 'NETWORK_ERROR',
          message: 'unreachable',
          requestId: null,
          retryable: true,
        })
      },
    },
  })
  await model.refreshInitialization()
  assert.equal(model.state.initialization, 'unknown')
  await model.initialize('bootstrap-proof-material')
  assert.equal(model.state.status, 'succeeded')
  assert.equal(model.state.source, 'initialization')
  model.close()
})

class FakeElement {
  constructor(tagName, document) {
    this.tagName = tagName.toUpperCase()
    this.ownerDocument = document
  }

  children = []
  listeners = new Map()
  attributes = new Map()
  className = ''
  textContent = ''
  value = ''
  hidden = false
  disabled = false
  required = false
  id = ''
  name = ''
  type = ''
  htmlFor = ''
  maxLength = -1
  autocomplete = ''
  spellcheck = true

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

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) ?? new Set()
    listeners.add(listener)
    this.listeners.set(type, listeners)
  }

  removeEventListener(type, listener) {
    this.listeners.get(type)?.delete(listener)
  }

  emit(type, event = {}) {
    for (const listener of this.listeners.get(type) ?? []) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) {
    return new FakeElement(tagName, this)
  }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function hasClass(node, className) {
  return node.className.split(/\s+/u).includes(className)
}

function findOne(rootElement, className) {
  const node = descendants(rootElement).find(candidate => hasClass(candidate, className))
  assert.notEqual(node, undefined, `${className} is mounted`)
  return node
}

function loginFixture(clientOverrides = {}) {
  const document = new FakeDocument()
  const rootElement = new FakeElement('div', document)
  const client = {
    async loginWithPassword() {
      throw loginError('AUTHENTICATION_REQUIRED', 'wrong password')
    },
    async login() { return session() },
    async initializationStatus() { return { initialized: true } },
    ...clientOverrides,
  }
  const model = createLoginViewModel({ client })
  const page = mountLoginPage({ root: rootElement, model })
  return { rootElement, model, page, client }
}

function submitForm(rootElement, className) {
  findOne(rootElement, className).emit('submit', { preventDefault() {} })
}

test('login page keeps credentials out of the DOM and distinguishes failure states', async () => {
  let mode = 'invalid'
  const { rootElement, model } = loginFixture({
    async loginWithPassword(credentials) {
      if (mode === 'success') return session()
      throw loginError(mode, 'sign-in rejected')
    },
  })
  const region = findOne(rootElement, 'wwc-login')
  const username = findOne(rootElement, 'wwc-login-username')
  const password = findOne(rootElement, 'wwc-login-password')
  const error = findOne(rootElement, 'wwc-login-error')
  const initializationSection = findOne(rootElement, 'wwc-login-initialization')

  assert.equal(region.hidden, true)
  assert.equal(initializationSection.hidden, true)

  const failureExpectations = [
    { mode: 'AUTHENTICATION_REQUIRED', text: 'Incorrect username or password.' },
    { mode: 'RATE_LIMITED', text: 'Too many sign-in attempts. Wait a moment, then try again.' },
  ]
  for (const candidate of failureExpectations) {
    mode = candidate.mode
    username.value = 'ada'
    password.value = `${passwordMaterial}-1`
    submitForm(rootElement, 'wwc-login-form')
    assert.equal(password.value, '', 'password leaves the DOM before the await')
    assert.equal(username.value, 'ada', 'the username draft survives the attempt')
    await new Promise(resolvePromise => setImmediate(resolvePromise))
    assert.equal(model.state.status, 'idle')
    assert.equal(model.state.failure, candidate.mode === 'AUTHENTICATION_REQUIRED'
      ? 'invalid-credentials'
      : 'rate-limited')
    assert.equal(error.hidden, false)
    assert.equal(error.textContent, candidate.text)
    assert.equal(username.getAttribute('aria-invalid'), 'true')
    assert.equal(password.getAttribute('aria-describedby'), 'wwc-login-error')
    assert.equal(JSON.stringify(model.state).includes(passwordMaterial), false)
    assert.equal(
      descendants(rootElement).map(node => node.textContent).join(' ').includes(passwordMaterial),
      false,
    )
  }

  mode = 'success'
  password.value = passwordMaterial
  submitForm(rootElement, 'wwc-login-form')
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  assert.equal(model.state.status, 'succeeded')
  assert.equal(findOne(rootElement, 'wwc-login-status').textContent,
    'Signed in. Returning to your workspace…')
  assert.equal(findOne(rootElement, 'wwc-login-submit').disabled, true)
  model.close()
})

test('login page shows the initialization entry only while the server is uninitialized', async () => {
  const first = loginFixture({ async initializationStatus() { return { initialized: false } } })
  assert.equal(findOne(first.rootElement, 'wwc-login-initialization').hidden, true)
  await first.model.refreshInitialization()
  const initializationSection = findOne(first.rootElement, 'wwc-login-initialization')
  assert.equal(initializationSection.hidden, false)
  const proof = findOne(first.rootElement, 'wwc-login-initialization-proof')
  proof.value = 'bootstrap-proof-material'
  submitForm(first.rootElement, 'wwc-login-initialization-form')
  assert.equal(proof.value, '', 'the proof leaves the DOM before the await')
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  assert.equal(first.model.state.status, 'succeeded')
  assert.equal(first.model.state.source, 'initialization')
  assert.equal(
    descendants(first.rootElement).map(node => node.textContent).join(' ')
      .includes('bootstrap-proof-material'),
    false,
  )
  first.model.close()
  first.page.close()
  assert.deepEqual(first.rootElement.children, [])

  const second = loginFixture({ async initializationStatus() { return { initialized: true } } })
  await second.model.refreshInitialization()
  assert.equal(findOne(second.rootElement, 'wwc-login-initialization').hidden, true)
  second.model.close()
})

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

class ApplicationElement {
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

function settingsFacadeFake({ initialized = true } = {}) {
  let expired = true
  const queries = []
  const subscriptions = []
  const loginAttempts = []
  const proofAttempts = []
  const client = {
    queries,
    subscriptions,
    loginAttempts,
    proofAttempts,
    serverUrl: 'https://control.example/login-app',
    setExpired(next) { expired = next },
    async restore() {
      if (expired) {
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
    async login(proof) {
      proofAttempts.push(proof)
      expired = false
      return structuredClone(session())
    },
    async loginWithPassword(credentials) {
      loginAttempts.push({ username: credentials.username, password: credentials.password })
      if (expired) {
        throw new ControlPlaneClientError({
          kind: 'authentication',
          code: 'AUTHENTICATION_REQUIRED',
          message: 'The username or password is wrong.',
          requestId: null,
          retryable: false,
        })
      }
      return structuredClone(session())
    },
    async initializationStatus() {
      return { initialized }
    },
    async logout() {},
    async command() { throw new Error('not used') },
    async query(request) {
      queries.push(structuredClone(request))
      const page = { hasMore: false, nextCursor: null }
      const base = {
        schemaVersion,
        requestId: request.requestId,
        query: request.query,
        page,
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
      if (request.query === 'enterprise.organization.list') {
        return {
          ...base,
          result: {
            kind: 'enterprise_organization_page',
            snapshotRevision: 1,
            items: [{
              id: repositoryScope.organizationId,
              displayName: 'Acme',
              slug: 'acme',
              state: 'active',
              revision: 1,
              updatedAt: '2026-09-02T00:00:00.000Z',
            }],
          },
        }
      }
      if (request.query === 'enterprise.project.list') {
        return {
          ...base,
          result: {
            kind: 'enterprise_project_repository_page',
            snapshotRevision: 1,
            items: [{
              kind: 'project',
              projectId: repositoryScope.projectId,
              displayName: 'Acme project',
              repositoryCount: 1,
              state: 'active',
              revision: 1,
              updatedAt: '2026-09-02T00:00:00.000Z',
            }, {
              kind: 'repository',
              projectId: repositoryScope.projectId,
              repositoryId: repositoryScope.repositoryId,
              displayName: 'Acme repository',
              defaultBranch: 'main',
              state: 'active',
              revision: 1,
              updatedAt: '2026-09-02T00:00:00.000Z',
            }],
          },
        }
      }
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

test('expired sessions return to the original target after sign-in, and logout restores the page', async () => {
  const client = settingsFacadeFake()
  const fixture = mountApplication('#/settings', client)
  await waitFor(
    () => fixture.application.authSession.state.status === 'authentication-required',
    'expired session surfaced',
  )
  const login = applicationNode(fixture.rootElement, 'wwc-login')
  assert.equal(login.hidden, false, 'session expiry opens the login page')
  assert.equal(fixture.browser.location.hash, '#/settings')
  assert.equal(client.queries.length, 0, 'no product read happens while signed out')
  assert.equal(
    applicationNode(fixture.rootElement, 'wwc-login-initialization').hidden,
    true,
    'an initialized server hides the first-time entry',
  )

  const username = applicationNode(fixture.rootElement, 'wwc-login-username')
  const password = applicationNode(fixture.rootElement, 'wwc-login-password')
  const error = applicationNode(fixture.rootElement, 'wwc-login-error')
  username.value = 'ada'
  password.value = 'wrong-password-value'
  applicationNode(fixture.rootElement, 'wwc-login-form').dispatchEvent({
    type: 'submit',
    preventDefault() {},
  })
  assert.equal(password.value, '')
  await waitFor(() => error.hidden === false, 'form-level failure')
  assert.equal(error.textContent, 'Incorrect username or password.')
  assert.equal(fixture.application.authSession.state.status, 'authentication-required')
  assert.equal(fixture.browser.location.hash, '#/settings')

  client.setExpired(false)
  username.value = 'ada'
  password.value = passwordMaterial
  applicationNode(fixture.rootElement, 'wwc-login-form').dispatchEvent({
    type: 'submit',
    preventDefault() {},
  })
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-in',
    'sign-in restores the session',
  )
  assert.equal(login.hidden, true, 'the login page closes after sign-in')
  assert.equal(fixture.browser.location.hash, '#/settings', 'the original target is preserved')
  await waitFor(
    () => applicationDescendants(fixture.rootElement).some(
      candidate => candidate.className.split(/\s+/u).includes('wwc-settings'),
    ),
    'the original settings route mounted',
  )
  assert.deepEqual(client.loginAttempts, [
    { username: 'ada', password: 'wrong-password-value' },
    { username: 'ada', password: passwordMaterial },
  ])

  applicationNode(fixture.rootElement, 'wwc-auth-session-sign-out').dispatchEvent({ type: 'click' })
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-out',
    'logout completes',
  )
  await waitFor(() => login.hidden === false, 'logout returns to the login page')
  fixture.application.close()
  assert.deepEqual(fixture.rootElement.children, [])
})

test('an uninitialized server offers the bootstrap initialization entry from the login page', async () => {
  const client = settingsFacadeFake({ initialized: false })
  const fixture = mountApplication('#/settings', client)
  await waitFor(
    () => fixture.application.authSession.state.status === 'authentication-required',
    'signed-out state',
  )
  const initializationSection = applicationNode(fixture.rootElement, 'wwc-login-initialization')
  await waitFor(() => initializationSection.hidden === false, 'initialization entry')
  const proof = applicationNode(fixture.rootElement, 'wwc-login-initialization-proof')
  proof.value = 'bootstrap-proof-material'
  applicationNode(fixture.rootElement, 'wwc-login-initialization-form').dispatchEvent({
    type: 'submit',
    preventDefault() {},
  })
  assert.equal(proof.value, '')
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-in',
    'owner initialization signed in',
  )
  assert.deepEqual(client.proofAttempts, ['bootstrap-proof-material'])
  assert.equal(fixture.browser.location.hash, '#/settings')
  const login = applicationNode(fixture.rootElement, 'wwc-login')
  await waitFor(() => login.hidden === true, 'login page closes')
  fixture.application.close()
})
