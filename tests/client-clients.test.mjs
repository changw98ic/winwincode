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
    'apps/client/tsconfig.client-clients-tests.json',
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
  `Clients area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/client-clients-tests')
// Plain module paths keep one ControlPlaneClientError class identity across
// the facade, the view-models, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')
const clientsViewModelModule = await cachedModule('clients-view-model.js')
const occupancyViewModelModule = await cachedModule('client-occupancy-view-model.js')
const clientsPageModule = await cachedModule('clients-page.js')
const applicationModule = await cachedModule('application.js')

const {
  ControlPlaneClientError,
  controlPlaneClientAddFailure,
  createControlPlaneClientDirectory,
} = facade
const { createClientsViewModel } = clientsViewModelModule
const { createClientOccupancyViewModel } = occupancyViewModelModule
const { mountClientsPage } = clientsPageModule
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
const connectPath = 'https://control.example/api/v1/clients/connections'
const listPath = 'https://control.example/api/v1/clients'
const validConnectInput = { clientId: '1234 5678 9012', connectionCode: '98765432' }
const fixedNow = Date.parse('2026-09-04T00:02:00.000Z')

function session() {
  return {
    schemaVersion,
    expiresAt: '2099-09-02T00:00:00.000Z',
    actor,
    authorizedScopes: [structuredClone(repositoryScope)],
  }
}

function device(overrides = {}) {
  return {
    clientId: '123456789012',
    displayName: 'Wenjie MacBook Pro',
    presence: 'online',
    occupancy: 'available',
    capacityUsed: 3,
    capacityTotal: 8,
    lastHeartbeatAt: '2026-09-04T00:00:00.000Z',
    version: '1.2.3',
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

function directoryError(code, retryable = false) {
  return new ControlPlaneClientError({
    kind: 'server',
    code,
    message: 'add-client rejected',
    requestId: null,
    retryable,
  })
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

test('facade exchanges grouped ID and dynamic code for the frozen fresh device list', async () => {
  const requests = []
  const directory = directoryFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), init: structuredClone(init) })
      return response(201, { schemaVersion, clients: [device()] })
    },
  })

  const devices = await directory.addClient(validConnectInput)

  assert.deepEqual(devices, [device()])
  assert.equal(Object.isFrozen(devices), true)
  assert.equal(Object.isFrozen(devices[0]), true)
  assert.deepEqual(requests.map(request => [request.input, request.init.method]), [
    [connectPath, 'POST'],
  ])
  const connectRequest = requests[0]
  assert.equal(connectRequest.init.credentials, 'include')
  assert.equal(connectRequest.init.redirect, 'error')
  assert.equal(connectRequest.init.cache, 'no-store')
  assert.equal(connectRequest.init.referrerPolicy, 'no-referrer')
  assert.deepEqual(JSON.parse(connectRequest.init.body), {
    schemaVersion,
    clientId: '123456789012',
    connectionCode: '98765432',
  })
})

test('facade reads the device list and rejects malformed directory responses', async () => {
  let payload = { schemaVersion, clients: [device(), device({ clientId: '999999999999' })] }
  let status = 200
  const directory = directoryFixture({
    async fetch(input, init) {
      assert.equal(init.method, 'GET')
      assert.equal(String(input), listPath)
      assert.equal(init.credentials, 'include')
      return response(status, payload)
    },
  })
  assert.deepEqual(await directory.listClients(), [
    device(),
    device({ clientId: '999999999999' }),
  ])
  payload = errorPayload('SERVICE_UNAVAILABLE', 'outage', true)
  status = 503
  await assert.rejects(
    directory.listClients(),
    error => error.code === 'SERVICE_UNAVAILABLE'
      && error.kind === 'server'
      && error.retryable === true,
  )
  status = 200
  payload = { schemaVersion, clients: 'nope' }
  await assert.rejects(
    directory.listClients(),
    error => error.code === 'INVALID_CLIENT_DIRECTORY_RESPONSE',
  )
  payload = { schemaVersion, clients: [device({ presence: 'sleeping' })] }
  await assert.rejects(
    directory.listClients(),
    error => error.code === 'INVALID_CLIENT_DIRECTORY_RESPONSE',
  )
  payload = { schemaVersion, clients: [device({ capacityUsed: 9 })] }
  await assert.rejects(
    directory.listClients(),
    error => error.code === 'INVALID_CLIENT_DIRECTORY_RESPONSE',
  )
  payload = { schemaVersion: 'winwincode/v0', clients: [] }
  await assert.rejects(
    directory.listClients(),
    error => error.kind === 'version' && error.code === 'SCHEMA_VERSION_MISMATCH',
  )
})

test('facade separates the seven connect failures from outages', async () => {
  const cases = [
    { status: 404, code: 'CLIENT_NOT_FOUND', failure: 'id-not-found' },
    { status: 409, code: 'CLIENT_OFFLINE', failure: 'client-offline' },
    { status: 400, code: 'CONNECT_CODE_INVALID', failure: 'code-invalid' },
    { status: 410, code: 'CONNECT_CODE_EXPIRED', failure: 'code-expired' },
    { status: 403, code: 'CLIENT_CONNECTIONS_FORBIDDEN', failure: 'new-connections-forbidden' },
    { status: 423, code: 'CLIENT_LOCKED', failure: 'client-locked' },
    { status: 429, code: 'RATE_LIMITED', failure: 'rate-limited', retryable: true },
  ]
  for (const candidate of cases) {
    const directory = directoryFixture({
      async fetch() {
        return response(candidate.status, errorPayload(
          candidate.code,
          'add-client rejected',
          candidate.retryable === true,
        ))
      },
    })
    await assert.rejects(
      directory.addClient(validConnectInput),
      error => {
        assert.equal(error instanceof ControlPlaneClientError, true)
        assert.equal(error.code, candidate.code)
        assert.equal(error.retryable, candidate.retryable === true)
        assert.equal(controlPlaneClientAddFailure(error), candidate.failure)
        return true
      },
    )
  }
  const offline = directoryFixture({
    async fetch() {
      throw new TypeError('network unreachable')
    },
  })
  await assert.rejects(
    offline.addClient(validConnectInput),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'network'
      && controlPlaneClientAddFailure(error) === 'unavailable',
  )
  const unknown = directoryFixture({
    async fetch() {
      return response(409, errorPayload('WRONG_STATE', 'wrong state'))
    },
  })
  await assert.rejects(
    unknown.addClient(validConnectInput),
    error => controlPlaneClientAddFailure(error) === 'unavailable',
  )
})

test('facade validates connect input before any request exists', async () => {
  let requests = 0
  const directory = directoryFixture({
    async fetch() {
      requests += 1
      return response(201, { schemaVersion, clients: [] })
    },
  })
  await assert.rejects(
    directory.addClient({ clientId: '12345678', connectionCode: '98765432' }),
    error => error.code === 'CLIENT_CONNECT_ID_INVALID',
  )
  await assert.rejects(
    directory.addClient({ clientId: '1234567890123', connectionCode: '98765432' }),
    error => error.code === 'CLIENT_CONNECT_ID_INVALID',
  )
  await assert.rejects(
    directory.addClient({ clientId: '123456789012', connectionCode: '9876543' }),
    error => error.code === 'CLIENT_CONNECT_CODE_INVALID',
  )
  assert.equal(requests, 0)
})

test('directory reuses an injected facade that already implements the directory', async () => {
  const attempts = []
  const directory = createControlPlaneClientDirectory({
    client: baseClient({
      async addClient(input) {
        attempts.push(['add', input])
        return [device({ clientId: input.clientId })]
      },
      async listClients() {
        attempts.push(['list'])
        return [device({ clientId: '999999999999' })]
      },
    }),
    transport: {
      async fetch() {
        throw new Error('the directory must stay on the injected seam')
      },
    },
  })
  assert.deepEqual(await directory.addClient(validConnectInput), [
    device({ clientId: '123456789012' }),
  ])
  assert.deepEqual(await directory.listClients(), [device({ clientId: '999999999999' })])
  assert.deepEqual(attempts, [
    ['add', { clientId: '123456789012', connectionCode: '98765432' }],
    ['list'],
  ])
})

function clientsClientFake(overrides = {}) {
  return {
    addCalls: [],
    listCalls: 0,
    async addClient(input) {
      this.addCalls.push({ ...input })
      return [device()]
    },
    async listClients() {
      this.listCalls += 1
      return [device()]
    },
    ...overrides,
  }
}

test('clients view-model publishes submissions, failure reasons, and blocks double submits', async () => {
  for (const [code, expected] of [
    ['CLIENT_NOT_FOUND', 'id-not-found'],
    ['CLIENT_OFFLINE', 'client-offline'],
    ['CONNECT_CODE_INVALID', 'code-invalid'],
    ['CONNECT_CODE_EXPIRED', 'code-expired'],
    ['CLIENT_CONNECTIONS_FORBIDDEN', 'new-connections-forbidden'],
    ['CLIENT_LOCKED', 'client-locked'],
    ['RATE_LIMITED', 'rate-limited'],
  ]) {
    const model = createClientsViewModel({
      client: clientsClientFake({
        async addClient() {
          throw directoryError(code)
        },
      }),
    })
    await model.addClient(validConnectInput)
    assert.equal(model.state.status, 'idle')
    assert.equal(model.state.failure, expected)
    model.dismissFailure()
    assert.equal(model.state.failure, null)
    model.close()
  }

  let release
  let addCalls = 0
  const model = createClientsViewModel({
    client: clientsClientFake({
      async addClient(input) {
        addCalls += 1
        this.addCalls.push({ ...input })
        await new Promise(resolvePromise => { release = resolvePromise })
        return [device(), device({ clientId: '999999999999' })]
      },
    }),
  })
  const first = model.addClient(validConnectInput)
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  assert.equal(model.state.status, 'submitting')
  await model.addClient(validConnectInput)
  assert.equal(addCalls, 1, 'a submission in progress is never repeated')
  release()
  await first
  assert.equal(model.state.status, 'succeeded')
  assert.equal(model.state.failure, null)
  assert.deepEqual(model.state.devices, [device(), device({ clientId: '999999999999' })])
  model.close()
})

test('clients view-model validates ID and code shapes before the facade is called', async () => {
  const client = clientsClientFake()
  const model = createClientsViewModel({ client })
  await model.addClient({ clientId: '12345678', connectionCode: '98765432' })
  assert.equal(model.state.failure, 'invalid-client-id')
  assert.equal(client.addCalls.length, 0)
  await model.addClient({ clientId: '1234567890123', connectionCode: '98765432' })
  assert.equal(model.state.failure, 'invalid-client-id')
  assert.equal(client.addCalls.length, 0)
  await model.addClient({ clientId: '123456789012', connectionCode: '9876543' })
  assert.equal(model.state.failure, 'invalid-connection-code')
  assert.equal(client.addCalls.length, 0)
  await model.addClient(validConnectInput)
  assert.equal(model.state.status, 'succeeded')
  assert.deepEqual(client.addCalls, [{
    clientId: '1234 5678 9012',
    connectionCode: '98765432',
  }])
  model.close()
})

test('clients view-model refresh keeps shown cards across an unavailable read', async () => {
  let failList = false
  let listCalls = 0
  const model = createClientsViewModel({
    client: clientsClientFake({
      async listClients() {
        listCalls += 1
        if (failList) throw new ControlPlaneClientError({
          kind: 'network',
          code: 'NETWORK_ERROR',
          message: 'unreachable',
          requestId: null,
          retryable: true,
        })
        return [device()]
      },
    }),
  })
  assert.equal(model.state.devicesStatus, 'unloaded')
  await model.refresh()
  assert.equal(model.state.devicesStatus, 'loaded')
  assert.deepEqual(model.state.devices, [device()])
  failList = true
  await model.refresh()
  assert.equal(model.state.devicesStatus, 'unavailable')
  assert.deepEqual(model.state.devices, [device()], 'a failed read never erases shown cards')
  assert.equal(listCalls, 2)
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
  href = ''
  id = ''
  htmlFor = ''
  name = ''
  required = false
  spellcheck = true
  autocomplete = ''
  type = ''
  value = ''
  maxLength = -1
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

function clientsFixture({ devices = [device()], addError = null, now = fixedNow } = {}) {
  const document = new PageDocument()
  const rootElement = new PageElement(document, 'div')
  const client = {
    addCalls: [],
    async addClient(input) {
      this.addCalls.push({ ...input })
      if (addError !== null) throw directoryError(addError)
      return devices
    },
    async listClients() {
      return devices
    },
  }
  const model = createClientsViewModel({ client })
  const page = mountClientsPage({ root: rootElement, model, now: () => now })
  page.setVisible(true)
  return { rootElement, model, page, client }
}

function submitForm(rootElement) {
  findOne(rootElement, 'wwc-clients-add-form').emit('submit', { preventDefault() {} })
}

test('clients page groups the ID while typing and validates shapes without a request', () => {
  const { rootElement, client } = clientsFixture()
  const idInput = findOne(rootElement, 'wwc-clients-id-input')
  const codeInput = findOne(rootElement, 'wwc-clients-code-input')
  const error = findOne(rootElement, 'wwc-clients-error')
  assert.equal(findOne(rootElement, 'wwc-clients').hidden, false)

  idInput.value = '1234567890123'
  idInput.emit('input')
  assert.equal(idInput.value, '1234 5678 9012', 'the ID renders in groups of four digits')

  idInput.value = '12345678'
  codeInput.value = '98765432'
  submitForm(rootElement)
  assert.equal(client.addCalls.length, 0, 'a malformed ID never reaches the facade')
  assert.equal(error.hidden, false)
  assert.equal(error.textContent, 'Enter the 9-12 digit Client ID shown on the device.')
  assert.equal(idInput.getAttribute('aria-invalid'), 'true')
  assert.equal(codeInput.getAttribute('aria-invalid'), null)

  idInput.value = '1234 5678 9012'
  codeInput.value = '9876543'
  submitForm(rootElement)
  assert.equal(error.textContent, 'Enter the 8-digit connection code shown on the device.')
  assert.equal(idInput.getAttribute('aria-invalid'), null)
  assert.equal(codeInput.getAttribute('aria-invalid'), 'true')
  assert.equal(client.addCalls.length, 0)
})

test('clients page renders the seven failure copies and keeps the ID draft', async () => {
  const failureCases = [
    { code: 'CLIENT_NOT_FOUND', text: 'No Client has this ID. Check the ID shown on the device.' },
    { code: 'CLIENT_OFFLINE', text: 'That Client is offline right now. Connect when it is back online.' },
    { code: 'CONNECT_CODE_INVALID', text: 'The connection code is wrong. Check the code on the device and try again.' },
    { code: 'CONNECT_CODE_EXPIRED', text: 'The connection code expired. Generate a new code on the device and try again.' },
    { code: 'CLIENT_CONNECTIONS_FORBIDDEN', text: 'That Client no longer accepts new connections.' },
    { code: 'CLIENT_LOCKED', text: 'That Client is locked. Unlock it on the device first.' },
    { code: 'RATE_LIMITED', text: 'Too many connection attempts. Wait a moment, then try again.' },
  ]
  for (const candidate of failureCases) {
    const { rootElement, model } = clientsFixture({ addError: candidate.code })
    const idInput = findOne(rootElement, 'wwc-clients-id-input')
    const codeInput = findOne(rootElement, 'wwc-clients-code-input')
    const error = findOne(rootElement, 'wwc-clients-error')
    idInput.value = '1234 5678 9012'
    codeInput.value = '98765432'
    submitForm(rootElement)
    assert.equal(codeInput.value, '', 'the dynamic code leaves the DOM before the await')
    assert.equal(idInput.value, '1234 5678 9012', 'the ID draft survives the attempt')
    await new Promise(resolvePromise => setImmediate(resolvePromise))
    assert.equal(model.state.status, 'idle')
    assert.equal(model.state.failure !== null, true)
    assert.equal(error.hidden, false)
    assert.equal(error.textContent, candidate.text)
    assert.equal(findOne(rootElement, 'wwc-clients-add-form').getAttribute('aria-busy'), 'false')
    model.close()
  }
})

test('clients page renders the device cards with the six Presence×Occupancy states', async () => {
  const sixStates = [
    { clientId: '100000000001', displayName: 'MacBook Pro', presence: 'online', occupancy: 'available' },
    { clientId: '100000000002', displayName: 'Mac Studio', presence: 'online', occupancy: 'occupied-by-me' },
    { clientId: '100000000003', displayName: 'Linux Box', presence: 'online', occupancy: 'occupied-by-other' },
    { clientId: '100000000004', displayName: 'Draining Rig', presence: 'online', occupancy: 'draining' },
    { clientId: '100000000005', displayName: 'Recovering Rig', presence: 'offline', occupancy: 'recovery-pending' },
    { clientId: '100000000006', displayName: 'Locked Rig', presence: 'locked', occupancy: 'available' },
  ]
  const stateTexts = [
    'Online, ready to connect',
    'Online, occupied by you',
    'Online, in use',
    'Online, finishing current tasks',
    'Connection interrupted, waiting to recover',
    'Client locked',
  ]
  const presenceTexts = ['Online', 'Online', 'Online', 'Online', 'Offline', 'Locked']
  const { rootElement, model } = clientsFixture({ devices: sixStates.map(device) })
  await model.refresh()
  const cards = findAll(rootElement, 'wwc-clients-card')
  assert.equal(cards.length, 6)
  for (const [index, card] of cards.entries()) {
    assert.equal(
      findOne(card, 'wwc-clients-card-name').textContent,
      sixStates[index].displayName,
    )
    assert.equal(
      findOne(card, 'wwc-clients-card-presence').textContent,
      presenceTexts[index],
    )
    assert.equal(findOne(card, 'wwc-clients-card-state').textContent, stateTexts[index])
    assert.equal(findOne(card, 'wwc-clients-card-capacity').textContent, 'Capacity 3 / 8')
    assert.equal(
      findOne(card, 'wwc-clients-card-heartbeat').textContent,
      'Last heartbeat 2 minutes ago',
    )
    assert.equal(findOne(card, 'wwc-clients-card-version').textContent, 'Version 1.2.3')
  }
  model.close()
})

test('clients page keeps card identity across refreshes and mounts the occupancy actions', async () => {
  let devices = [device(), device({ clientId: '999999999999', displayName: 'Team Mac Studio' })]
  const model = createClientsViewModel({
    client: {
      async addClient() { return devices },
      async listClients() { return devices },
    },
  })
  const occupancyModel = createClientOccupancyViewModel({
    port: null,
    clients: model,
  })
  const document = new PageDocument()
  const rootElement = new PageElement(document, 'div')
  const page = mountClientsPage({ root: rootElement, model, occupancy: occupancyModel, now: () => fixedNow })
  page.setVisible(true)
  const list = findOne(rootElement, 'wwc-clients-list')
  await model.refresh()
  assert.equal(findAll(rootElement, 'wwc-clients-card').length, 2)
  const firstCard = list.children[0]
  const secondCard = list.children[1]
  assert.equal(
    findOne(firstCard, 'wwc-clients-card-connect').disabled,
    false,
    'a free online device offers the connect entry',
  )
  assert.equal(
    findOne(firstCard, 'wwc-clients-card-release').hidden,
    true,
    'a free device hides the holder release entry',
  )
  assert.equal(
    findOne(firstCard, 'wwc-clients-card-cancel-release').hidden,
    true,
    'a free device hides the immediate stop entry',
  )

  devices = [
    device({ clientId: '999999999999', displayName: 'Renamed Mac Studio' }),
    device(),
    device({ clientId: '100000000001', displayName: 'New Rig' }),
  ]
  await model.refresh()
  assert.equal(findAll(rootElement, 'wwc-clients-card').length, 3)
  assert.equal(list.children[0], secondCard, 'the retained card keeps its node identity')
  assert.equal(list.children[1], firstCard)
  assert.equal(
    findOne(secondCard, 'wwc-clients-card-name').textContent,
    'Renamed Mac Studio',
    'the retained card updates in place',
  )
  assert.equal(findOne(rootElement, 'wwc-clients-empty').hidden, true)

  devices = []
  await model.refresh()
  assert.equal(findAll(rootElement, 'wwc-clients-card').length, 0)
  assert.equal(findOne(rootElement, 'wwc-clients-empty').hidden, false)
  occupancyModel.close()
  page.close()
  assert.deepEqual(rootElement.children, [])
})

test('clients page disables the submit for the whole submission and clears drafts on success', async () => {
  let release
  const { rootElement, model, client } = clientsFixture()
  const idInput = findOne(rootElement, 'wwc-clients-id-input')
  const codeInput = findOne(rootElement, 'wwc-clients-code-input')
  const submit = findOne(rootElement, 'wwc-clients-add-submit')
  client.addClient = async input => {
    client.addCalls.push({ ...input })
    await new Promise(resolvePromise => { release = resolvePromise })
    return [device()]
  }
  idInput.value = '1234 5678 9012'
  codeInput.value = '98765432'
  submitForm(rootElement)
  assert.equal(submit.disabled, true, 'the submit is disabled while connecting')
  assert.equal(idInput.disabled, true)
  assert.equal(codeInput.disabled, true)
  assert.equal(
    findOne(rootElement, 'wwc-clients-add-form').getAttribute('aria-busy'),
    'true',
  )
  submitForm(rootElement)
  assert.equal(client.addCalls.length, 1, 'a second submit during flight is ignored')
  release()
  await waitFor(() => model.state.status === 'succeeded', 'the submission settles')
  assert.equal(findOne(rootElement, 'wwc-clients-status').textContent, 'Client added.')
  assert.equal(submit.disabled, false)
  assert.equal(idInput.value, '', 'the ID draft is cleared after success')
  assert.equal(codeInput.value, '')
  assert.equal(findAll(rootElement, 'wwc-clients-card').length, 1)
  model.close()
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

function clientsFacadeFake({ expired = true, devices = [device()] } = {}) {
  let sessionExpired = expired
  const queries = []
  const subscriptions = []
  const addCalls = []
  const client = {
    queries,
    subscriptions,
    addCalls,
    serverUrl: 'https://control.example/clients-app',
    directoryDevices: devices,
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
    async addClient(input) {
      addCalls.push({ ...input })
      this.directoryDevices = [...this.directoryDevices, device({
        clientId: '999999999999',
        displayName: 'Newly Connected Rig',
      })]
      return structuredClone(this.directoryDevices)
    },
    async listClients() {
      return structuredClone(this.directoryDevices)
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

test('the signed-in Clients area lists devices, adds one, and hides on sign-out', async () => {
  const client = clientsFacadeFake({ expired: true })
  const fixture = mountApplication('#/settings', client)
  await waitFor(
    () => fixture.application.authSession.state.status === 'authentication-required',
    'expired session surfaced',
  )
  await waitFor(
    () => applicationNode(fixture.rootElement, 'wwc-clients').hidden === true,
    'the Clients area stays hidden while signed out',
  )
  assert.equal(client.queries.length, 0, 'no device read happens while signed out')

  client.setExpired(false)
  const authSession = fixture.application.authSession
  await authSession.restore()
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-in',
    'session restored',
  )
  await waitFor(
    () => applicationNode(fixture.rootElement, 'wwc-clients').hidden === false,
    'the Clients area opens for a signed-in user',
  )
  await waitFor(
    () => applicationFindAll(fixture.rootElement, 'wwc-clients-card').length === 1,
    'the device card list rendered',
  )
  const card = applicationNode(fixture.rootElement, 'wwc-clients-card')
  assert.equal(applicationNode(fixture.rootElement, 'wwc-clients-card-state').textContent,
    'Online, ready to connect')
  assert.equal(
    applicationNode(fixture.rootElement, 'wwc-clients-card-connect').disabled,
    false,
    'a free online device offers the connect entry',
  )
  assert.equal(
    applicationNode(fixture.rootElement, 'wwc-clients-card-release').hidden,
    true,
    'a free device hides the holder release entry',
  )

  const idInput = applicationNode(fixture.rootElement, 'wwc-clients-id-input')
  const codeInput = applicationNode(fixture.rootElement, 'wwc-clients-code-input')
  idInput.value = '1234 5678 9012'
  codeInput.value = '98765432'
  applicationNode(fixture.rootElement, 'wwc-clients-add-form').dispatchEvent({
    type: 'submit',
    preventDefault() {},
  })
  await waitFor(
    () => applicationFindAll(fixture.rootElement, 'wwc-clients-card').length === 2,
    'the fresh list renders the added device',
  )
  assert.deepEqual(client.addCalls, [{
    clientId: '123456789012',
    connectionCode: '98765432',
  }])

  await authSession.logout()
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-out',
    'logout completes',
  )
  await waitFor(
    () => applicationNode(fixture.rootElement, 'wwc-clients').hidden === true,
    'the Clients area hides after sign-out',
  )
  fixture.application.close()
  assert.deepEqual(fixture.rootElement.children, [])
})

function applicationFindAll(rootElement, className) {
  return applicationDescendants(rootElement).filter(
    candidate => candidate.className.split(/\s+/u).includes(className),
  )
}
