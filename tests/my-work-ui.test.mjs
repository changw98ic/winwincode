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
    'apps/client/tsconfig.my-work-tests.json',
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
  `My Work area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/my-work-tests')
// Plain module paths keep one module identity across the facade, the reused
// projections, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')
const clientsViewModelModule = await cachedModule('clients-view-model.js')
const dashboardModule = await cachedModule('home-dashboard-view-model.js')
const myWorkViewModelModule = await cachedModule('my-work-view-model.js')
const myWorkPageModule = await cachedModule('my-work-page.js')

const { ControlPlaneClientError } = facade
const { createClientsViewModel } = clientsViewModelModule
const { homeDashboardState } = dashboardModule
const { createMyWorkViewModel, myWorkClientsSummary, myWorkState } = myWorkViewModelModule
const { mountMyWorkPage, myWorkPresentation } = myWorkPageModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const scopeSelection = {
  organizationId: scope.organizationId,
  workspaceId: scope.workspaceId,
  projectId: scope.projectId,
  repositoryId: scope.repositoryId,
}
const subscriptionId = 'sub_00000000000000000000000001'
const NOW = Date.parse('2026-09-04T00:02:00.000Z')

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

const SIX_DEVICE_STATES = [
  device({ clientId: '100000000001', displayName: 'MacBook Pro', occupancy: 'available' }),
  device({ clientId: '100000000002', displayName: 'Mac Studio', occupancy: 'occupied-by-me' }),
  device({ clientId: '100000000003', displayName: 'Linux Box', occupancy: 'occupied-by-other' }),
  device({ clientId: '100000000004', displayName: 'Draining Rig', occupancy: 'draining' }),
  device({
    clientId: '100000000005',
    displayName: 'Recovering Rig',
    presence: 'offline',
    occupancy: 'recovery-pending',
  }),
  device({
    clientId: '100000000006',
    displayName: 'Locked Rig',
    presence: 'locked',
    occupancy: 'available',
  }),
]

function deliverySummary(overrides = {}) {
  return {
    activeStageRunId: null,
    deliveryId: 'dlv_00000000000000000000000001',
    openAttentionCount: 0,
    ownership: { ...scope },
    revision: 3,
    schemaVersion,
    status: 'executing',
    taskCounts: { active: 1, blocked: 0, completed: 0, failed: 0, pending: 0, total: 1, verifying: 0 },
    title: 'Delivery',
    updatedAt: '2026-09-03T08:00:00.000Z',
    ...overrides,
  }
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function respond(request, state) {
  const result = () => {
    if (request.query === 'delivery.list') {
      return { kind: 'delivery_page', items: state.deliveries }
    }
    if (request.query === 'delivery.get') {
      const delivery = state.deliveries.find(
        item => item.deliveryId === request.parameters.deliveryId,
      )
      return {
        deliveryId: delivery.deliveryId,
        deliveryRevision: delivery.revision,
        ownership: delivery.ownership,
        attention: [],
        currentCandidate: null,
        requirements: { repository: { kind: 'local-git', locator: 'local-repository' } },
        internalToolPayload: null,
      }
    }
    if (request.query === 'session.list') {
      return { kind: 'product_session_page', items: [] }
    }
    if (request.query === 'session.interactions.list') {
      return { kind: 'chat_interaction_page', items: [] }
    }
    if (request.query === 'approval.list') {
      return { kind: 'approval_page', items: [] }
    }
    if (request.query === 'worker.list') {
      return { kind: 'worker_page', items: [] }
    }
    if (request.query === 'credential.reference.list') {
      return { kind: 'credential_reference_page', items: [] }
    }
    if (request.query === 'settings.get') {
      return { revision: 1, defaultModelRoute: null, workerConcurrencyLimit: 2 }
    }
    if (request.query === 'model.route.availability.list') {
      return {
        kind: 'model_route_availability_page',
        scope: request.scope,
        requestPoolSource: request.scope,
        requestPoolRevision: 1,
        settingsRevision: 1,
        settingsSource: request.scope,
        defaultProviderId: null,
        defaultModelId: null,
        status: 'disabled',
        reason: 'no_provider',
        items: [],
      }
    }
    throw new Error(`unexpected query ${request.query}`)
  }
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result: result(),
    page: page(),
  }
}

function contractFake(initialState = {}) {
  const state = {
    deliveries: [],
    ...initialState,
  }
  let failQueries = false
  const queries = []
  return {
    queries,
    serverUrl: 'https://control.example/my-work',
    get deliveries() { return state.deliveries },
    set deliveries(value) { state.deliveries = value },
    failNextQueries() { failQueries = true },
    async query(request) {
      queries.push(structuredClone(request))
      if (failQueries) {
        throw new ControlPlaneClientError({
          kind: 'network',
          code: 'NETWORK_ERROR',
          message: 'unreachable',
          requestId: null,
          retryable: true,
        })
      }
      return respond(request, state)
    },
    subscribe() {
      return { cursor: null, resume() {}, reconnect() {}, close() {} }
    },
    close() {},
  }
}

function clientsFake(devices, overrides = {}) {
  let current = devices
  let failList = false
  return {
    listCalls: 0,
    get devices() { return current },
    set devices(value) { current = value },
    failNextList() { failList = true },
    async listClients() {
      this.listCalls += 1
      if (failList) {
        throw new ControlPlaneClientError({
          kind: 'network',
          code: 'NETWORK_ERROR',
          message: 'unreachable',
          requestId: null,
          retryable: true,
        })
      }
      return current
    },
    async addClient() {
      return current
    },
    ...overrides,
  }
}

function myWorkFixture({
  deliveries = [],
  devices = [device()],
} = {}) {
  const client = contractFake({ deliveries })
  const directory = clientsFake(devices)
  const clients = createClientsViewModel({ client: directory })
  const model = createMyWorkViewModel({
    client,
    actor,
    scope,
    subscriptionId,
    nextRequestId: (() => {
      let next = 0
      return () => `req_${String(++next).padStart(26, '0')}`
    })(),
    clients,
  })
  return { client, directory, clients, model }
}

test('the Clients zone buckets every Presence×Occupancy state without naming holders', () => {
  const summary = myWorkClientsSummary(SIX_DEVICE_STATES)
  assert.deepEqual(summary, {
    ready: 1,
    occupiedByMe: 1,
    occupiedByOther: 1,
    unavailable: 3,
    total: 6,
  })
})

function deliveryListState(status, visible) {
  return {
    status,
    filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
    visible,
    loadedCount: visible.length,
    hasMore: false,
    loadingMore: false,
    moreFailure: null,
    error: null,
    advance: { deliveryId: null, failure: null },
  }
}

function attentionState(status) {
  return {
    status,
    items: [],
    counts: { total: 0, blocking: 0, pending: 0, expired: 0, bindingInvalid: 0 },
  }
}

function usageState(status) {
  return {
    status,
    worker: null,
    credentials: [],
    modelRoute: null,
    sources: { worker: status, credentials: status, modelRoute: status },
  }
}

function workState(deliveriesStatus, attentionStatus, deliveriesStatusItems = [], usageStatus = deliveriesStatus) {
  return homeDashboardState({
    deliveries: deliveryListState(deliveriesStatus, deliveriesStatusItems),
    attention: attentionState(attentionStatus),
    usage: usageState(usageStatus),
    visits: [],
  })
}

test('myWorkState groups the reused snapshots into the canonical My Work zones', () => {
  const executing = deliverySummary()
  const delivered = deliverySummary({
    deliveryId: 'dlv_00000000000000000000000002',
    status: 'delivered',
    title: 'Delivered',
  })
  const work = workState('ready', 'ready', [executing, delivered], 'ready')
  const state = myWorkState({
    work,
    devices: SIX_DEVICE_STATES,
    devicesStatus: 'loaded',
  })

  assert.equal(state.status, 'ready')
  assert.equal(state.work, work, 'the reused snapshot is projected, never copied')
  assert.deepEqual(state.counts, {
    needsAttention: 0,
    running: 1,
    completed: 1,
    clients: 6,
  })
  assert.deepEqual(state.sources, { work: 'ok', clients: 'ok' })
  assert.equal(state.clients.status, 'loaded')
  assert.equal(state.clients.summary.total, 6)
})

test('a failed read marks the sources unavailable and keeps every served list', () => {
  const running = deliverySummary()
  const completed = deliverySummary({
    deliveryId: 'dlv_00000000000000000000000002',
    status: 'delivered',
    title: 'Delivered',
  })
  const failed = deliverySummary({
    deliveryId: 'dlv_00000000000000000000000003',
    status: 'needs_attention',
    title: 'Failing',
    taskCounts: { active: 0, blocked: 0, completed: 1, failed: 2, pending: 0, total: 3, verifying: 0 },
  })
  // Every owning projection reports its failed read, and every one of them
  // still carries the last served facts.
  const work = workState('error', 'error', [running, completed, failed])
  const state = myWorkState({
    work,
    devices: SIX_DEVICE_STATES.slice(0, 2),
    devicesStatus: 'unavailable',
  })

  assert.deepEqual(state.sources, { work: 'unavailable', clients: 'unavailable' })
  assert.equal(state.status, 'error')
  assert.equal(
    state.work.active.length + state.work.failing.length + state.work.completed.length,
    3,
    'a failed read never clears the served work lists',
  )
  assert.deepEqual(state.counts, {
    needsAttention: 1,
    running: 1,
    completed: 1,
    clients: 2,
  })
  assert.equal(
    state.clients.devices.length,
    2,
    'a failed read never clears the served devices',
  )
})

test('loading and partial sources map to honest statuses', () => {
  const loading = myWorkState({
    work: workState('loading', 'loading'),
    devices: [],
    devicesStatus: 'loading',
  })
  assert.equal(loading.status, 'loading')
  assert.deepEqual(loading.sources, { work: 'loading', clients: 'loading' })

  const partial = myWorkState({
    work: workState('ready', 'ready', [deliverySummary()], 'ready'),
    devices: [],
    devicesStatus: 'unavailable',
  })
  assert.equal(partial.status, 'partial')
  assert.deepEqual(partial.sources, { work: 'ok', clients: 'unavailable' })
})

test('the composed view model starts the existing projections and reads the shared Clients model once', async () => {
  const { model, directory } = myWorkFixture({
    deliveries: [deliverySummary()],
  })
  await model.start()

  assert.equal(model.state.status, 'ready', JSON.stringify(model.state.sources))
  assert.deepEqual(model.state.sources, { work: 'ok', clients: 'ok' })
  assert.equal(model.state.counts.running, 1)
  assert.equal(model.state.counts.clients, 1)
  assert.equal(
    model.state.work,
    model.work.state,
    'My Work projects the one live dashboard snapshot',
  )
  assert.equal(
    directory.listCalls,
    1,
    'the Clients zone reuses the shell model instead of a second device read',
  )
  model.close()
})

test('a failed refresh keeps the shown work sections and devices and marks the gaps', async () => {
  const { client, directory, clients, model } = myWorkFixture({
    deliveries: [
      deliverySummary(),
      deliverySummary({
        deliveryId: 'dlv_00000000000000000000000002',
        status: 'delivered',
        title: 'Delivered',
      }),
    ],
    devices: [device()],
  })
  await model.start()
  assert.equal(model.state.counts.running, 1)
  assert.equal(model.state.counts.completed, 1)

  client.failNextQueries()
  directory.failNextList()
  await model.refresh()

  assert.deepEqual(model.state.sources, { work: 'ok', clients: 'unavailable' })
  assert.equal(model.state.status, 'partial')
  assert.equal(
    model.state.work.active.length + model.state.work.completed.length,
    2,
    'a failed refresh never clears the served work sections',
  )
  assert.equal(
    model.state.clients.devices.length,
    1,
    'a failed refresh never clears the served devices',
  )
  assert.equal(clients.state.devices.length, 1)
  model.close()
})

test('closing My Work closes the composed projection but never the shared Clients model', async () => {
  const { clients, model } = myWorkFixture()
  await model.start()
  model.close()
  assert.equal(model.state.status, 'closed')
  assert.equal(model.work.state.status, 'closed')

  await clients.refresh()
  assert.equal(clients.state.devicesStatus, 'loaded')
  assert.equal(model.state.status, 'closed', 'a closed My Work model stays closed')
})

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
  value = ''
  id = ''
  tabIndex = 0
  title = ''
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

  get href() { return this.getAttribute('href') ?? '' }

  set href(value) { this.setAttribute('href', value) }

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

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }

  removeEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    this.listeners.set(name, current.filter(candidate => candidate !== listener))
  }

  dispatch(name) {
    const event = { preventDefault() {} }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function allByClass(rootElement, className) {
  return descendants(rootElement).filter(node => node.className.split(/\s+/u).includes(className))
}

function byClass(rootElement, className) {
  const match = allByClass(rootElement, className)[0]
  assert.notEqual(match, undefined, `missing .${className}`)
  return match
}

function visibleText(node) {
  return descendants(node).map(current => current.textContent).join(' ')
}

function pageFixture({ deliveries = [], devices = [device()] } = {}) {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const { client, directory, clients, model } = myWorkFixture({ deliveries, devices })
  const page = mountMyWorkPage({
    root: rootElement,
    model,
    scopeSelection,
    nowMillis: () => NOW,
  })
  return { rootElement, client, directory, clients, model, page }
}

test('the My Work page mounts the start entry, the reused work sections, and the Clients zone', async () => {
  const { rootElement, model } = pageFixture({
    deliveries: [deliverySummary()],
    devices: SIX_DEVICE_STATES,
  })
  await model.start()
  assert.equal(model.state.status, 'ready', JSON.stringify(model.state.sources))

  const layout = byClass(rootElement, 'wwc-my-work')
  assert.equal(layout.dataset.wwcPage, 'home', 'the layout carries the Home page style hook')
  assert.equal(
    allByClass(rootElement, 'wwc-home').length,
    1,
    'the nested Home dashboard stays the one .wwc-home element on the surface',
  )

  const start = byClass(rootElement, 'wwc-my-work-start')
  assert.match(visibleText(start), /Start a new task/u)
  const chat = byClass(start, 'wwc-my-work-start-chat')
  assert.equal(
    chat.href,
    '#/chat?organizationId=org_00000000000000000000000001'
      + '&workspaceId=wsp_00000000000000000000000001'
      + '&projectId=prj_00000000000000000000000001'
      + '&repositoryId=rep_00000000000000000000000001',
  )
  const delivery = byClass(start, 'wwc-my-work-start-delivery')
  assert.equal(
    delivery.href,
    '#/strongflow?organizationId=org_00000000000000000000000001'
      + '&workspaceId=wsp_00000000000000000000000001'
      + '&projectId=prj_00000000000000000000000001'
      + '&repositoryId=rep_00000000000000000000000001',
  )

  const work = byClass(rootElement, 'wwc-my-work-work')
  const liveRegions = descendants(work).filter(
    node => node.getAttribute('aria-live') === 'polite',
  )
  assert.equal(liveRegions.length, 1, 'the reused dashboard keeps the single polite live region')
  assert.equal(
    allByClass(work, 'wwc-home-card').some(card => card.dataset.status === 'executing'),
    true,
    'the running Delivery section renders the reused projection',
  )

  const zone = byClass(rootElement, 'wwc-my-work-clients')
  assert.equal(byClass(zone, 'wwc-my-work-clients-heading').textContent, 'Clients')
  assert.equal(byClass(zone, 'wwc-my-work-clients-count').textContent, '6 devices')
  const rows = allByClass(zone, 'wwc-my-work-clients-device')
  assert.equal(rows.length, 6)
  assert.equal(byClass(rows[0], 'wwc-clients-card-name').textContent, 'MacBook Pro')
  assert.equal(byClass(rows[0], 'wwc-clients-card-presence').textContent, 'Online')
  assert.equal(byClass(rows[0], 'wwc-clients-card-state').textContent, 'Online, ready to connect')
  assert.match(
    byClass(rows[1], 'wwc-clients-card-state').textContent,
    /occupied by you/u,
  )
  assert.match(
    visibleText(rows[4]),
    /Connection interrupted, waiting to recover/u,
    'an offline recovering device never pretends to be available',
  )
  assert.match(visibleText(rows[0]), /Capacity 3 \/ 8/u)
  assert.match(visibleText(rows[0]), /Last heartbeat 2 minutes ago/u)
  assert.match(visibleText(rows[0]), /Version 1\.2\.3/u)
  model.close()
})

test('device rows keep node identity across refreshes and survive an unavailable read', async () => {
  const { rootElement, directory, clients, model } = pageFixture({
    devices: [device(), device({ clientId: '999999999999', displayName: 'Team Mac Studio' })],
  })
  await model.start()
  const zone = byClass(rootElement, 'wwc-my-work-clients')
  const list = byClass(zone, 'wwc-my-work-clients-devices')
  assert.equal(allByClass(zone, 'wwc-my-work-clients-device').length, 2)
  const firstRow = list.children[0]
  const secondRow = list.children[1]

  directory.devices = [
    device({ clientId: '999999999999', displayName: 'Renamed Mac Studio' }),
    device(),
    device({ clientId: '100000000001', displayName: 'New Rig' }),
  ]
  await clients.refresh()

  assert.equal(allByClass(zone, 'wwc-my-work-clients-device').length, 3)
  assert.equal(list.children[0], secondRow, 'the retained row keeps its node identity')
  assert.equal(list.children[1], firstRow)
  assert.equal(
    byClass(secondRow, 'wwc-clients-card-name').textContent,
    'Renamed Mac Studio',
    'the retained row updates in place',
  )

  directory.failNextList()
  await clients.refresh()
  assert.equal(clients.state.devicesStatus, 'unavailable')
  assert.equal(
    allByClass(zone, 'wwc-my-work-clients-device').length,
    3,
    'a failed read keeps the shown device rows',
  )
  assert.equal(byClass(zone, 'wwc-my-work-clients-unavailable').hidden, false)
  assert.match(
    byClass(zone, 'wwc-my-work-clients-unavailable').textContent,
    /keep their last known status/u,
  )
  model.close()
})

test('the Clients zone reports an honest empty state after a successful empty read', async () => {
  const { rootElement, directory, clients, model } = pageFixture({ devices: [] })
  await model.start()
  const zone = byClass(rootElement, 'wwc-my-work-clients')
  assert.equal(byClass(zone, 'wwc-my-work-clients-empty').hidden, false)
  assert.match(byClass(zone, 'wwc-my-work-clients-empty').textContent, /No Client is connected yet/u)

  directory.devices = [device()]
  await clients.refresh()
  assert.equal(byClass(zone, 'wwc-my-work-clients-empty').hidden, true)
  assert.equal(allByClass(zone, 'wwc-my-work-clients-device').length, 1)
  model.close()
})

test('the page close clears the root once and the shared Clients model survives', async () => {
  const { rootElement, clients, model, page } = pageFixture()
  await model.start()
  page.close()
  assert.deepEqual(rootElement.children, [])
  page.close()

  await clients.refresh()
  assert.equal(clients.state.devicesStatus, 'loaded')
  assert.equal(model.state.status, 'closed')
})

test('the presentation copy stays fixed and names the Clients and Repositories hierarchy', () => {
  const presentation = myWorkPresentation()
  assert.equal(Object.isFrozen(presentation), true)
  assert.match(presentation.clientsHint, /Clients area/u)
  assert.match(presentation.clientsHint, /Repositories area/u)
  assert.match(presentation.startChatLabel, /Chat/u)
  assert.match(presentation.startDeliveryLabel, /StrongFlow/u)
})
