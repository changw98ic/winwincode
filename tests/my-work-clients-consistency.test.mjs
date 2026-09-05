// [UI-100.3] My Work Clients zone and the Clients page render one shared
// Clients area model, so every Server snapshot must flow to both surfaces
// with the same state copy, the same counts, and the same failure survival.
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
  `My Work Clients consistency did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/my-work-tests')
// Plain module paths keep one module identity across the reused projections
// and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')
const clientsViewModelModule = await cachedModule('clients-view-model.js')
const occupancyViewModelModule = await cachedModule('client-occupancy-view-model.js')
const dashboardModule = await cachedModule('home-dashboard-view-model.js')
const myWorkViewModelModule = await cachedModule('my-work-view-model.js')
const myWorkPageModule = await cachedModule('my-work-page.js')
const clientsPageModule = await cachedModule('clients-page.js')

const { ControlPlaneClientError } = facade
const { createClientsViewModel } = clientsViewModelModule
const { createClientOccupancyViewModel } = occupancyViewModelModule
const { homeDashboardState } = dashboardModule
const { createMyWorkViewModel } = myWorkViewModelModule
const { mountMyWorkPage } = myWorkPageModule
const { mountClientsPage } = clientsPageModule

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
const futureDeadline = '2026-09-04T01:00:00.000Z'

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
  return {
    serverUrl: 'https://control.example/my-work',
    get deliveries() { return state.deliveries },
    set deliveries(value) { state.deliveries = value },
    async query(request) {
      return respond(request, state)
    },
    subscribe() {
      return { cursor: null, resume() {}, reconnect() {}, close() {} }
    },
    close() {},
  }
}

function clientsFake(devices) {
  let current = devices
  let failList = false
  return {
    get devices() { return current },
    set devices(value) { current = value },
    failNextList() { failList = true },
    async addClient() { return current },
    async listClients() {
      if (failList) {
        failList = false
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
  }
}

/** One deterministic occupancy port without the Owner seam (portFake shape). */
function portFake() {
  const pending = []
  function track() {
    return new Promise((resolvePromise, rejectPromise) => {
      pending.push({ resolve: resolvePromise, reject: rejectPromise })
    })
  }
  return {
    pending,
    claim() { return track() },
    release() { return track() },
    cancelAndRelease() { return track() },
  }
}

/**
 * One shared Clients area model drives both surfaces, exactly like the shell
 * composition: the Clients page and the My Work zone subscribe to the same
 * view-model and therefore the same Server snapshot.
 */
function consistencyFixture({ devices = [device()] } = {}) {
  const document = new FakeDocument()
  const myWorkRoot = new FakeElement(document, 'div')
  const clientsRoot = new FakeElement(document, 'div')
  const client = contractFake({ deliveries: [deliverySummary()] })
  const directory = clientsFake(devices)
  const clients = createClientsViewModel({ client: directory })
  const occupancy = createClientOccupancyViewModel({ port: portFake(), clients })
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
  const myWork = mountMyWorkPage({
    root: myWorkRoot,
    model,
    scopeSelection,
    nowMillis: () => NOW,
    ownsModel: false,
  })
  const clientsPage = mountClientsPage({
    root: clientsRoot,
    model: clients,
    occupancy,
    now: () => NOW,
  })
  clientsPage.setVisible(true)
  return { myWorkRoot, clientsRoot, directory, clients, occupancy, model, myWork, clientsPage }
}

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
    this.attributes = new Map()
    this.children = []
    this.parentNode = null
    this.listeners = new Map()
    this.dataset = {}
    this.className = ''
    this.disabled = false
    this.hidden = false
    this.tabIndex = 0
    this.href = ''
    this.id = ''
    this.htmlFor = ''
    this.name = ''
    this.required = false
    this.spellcheck = true
    this.autocomplete = ''
    this.type = ''
    this.value = ''
    this.maxLength = -1
    this.title = ''
    this.#textContent = ''
    this.checkValidity = () => true
  }
  #textContent = ''

  get textContent() {
    return this.#textContent
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

function zoneRows(myWorkRoot) {
  return allByClass(byClass(myWorkRoot, 'wwc-my-work-clients'), 'wwc-my-work-clients-device')
}

function pageCards(clientsRoot) {
  return allByClass(clientsRoot, 'wwc-clients-card')
}

function rowFor(rootElement, displayName) {
  return allByClass(rootElement, 'wwc-my-work-clients-device')
    .find(node => descendants(node).some(child => child.textContent === displayName))
}

function cardFor(clientsRoot, displayName) {
  return pageCards(clientsRoot)
    .find(node => descendants(node).some(child => child.textContent === displayName))
}

function stateText(node) {
  return descendants(node).find(child => child.className === 'wwc-clients-card-state').textContent
}

function presenceText(node) {
  return descendants(node).find(child => child.className === 'wwc-clients-card-presence')
    .textContent
}

/**
 * The one consistency assertion: for the snapshot the shared model serves,
 * both surfaces print the same state copy per device and the zone summary
 * matches the served list.
 */
function assertSurfacesAgree(fixtureHandle, expectedDevices) {
  const { myWorkRoot, clientsRoot, clients, model } = fixtureHandle
  const zone = byClass(myWorkRoot, 'wwc-my-work-clients')
  assert.deepEqual(model.state.clients.devices, expectedDevices)
  assert.deepEqual(clients.state.devices, expectedDevices)
  for (const deviceSnapshot of expectedDevices) {
    const row = rowFor(myWorkRoot, deviceSnapshot.displayName)
    const card = cardFor(clientsRoot, deviceSnapshot.displayName)
    assert.notEqual(row, undefined, `the zone renders ${deviceSnapshot.displayName}`)
    assert.notEqual(card, undefined, `the Clients page renders ${deviceSnapshot.displayName}`)
    assert.equal(
      stateText(row),
      stateText(card),
      `${deviceSnapshot.displayName}: the zone and the Clients page name the same state`,
    )
    assert.equal(presenceText(row), presenceText(card))
  }
  assert.equal(
    byClass(zone, 'wwc-my-work-clients-count').textContent,
    expectedDevices.length === 1 ? '1 device' : `${expectedDevices.length} devices`,
  )
  assert.equal(model.state.clients.summary.total, expectedDevices.length)
  assert.equal(
    byClass(myWorkRoot, 'wwc-my-work').getAttribute('aria-busy'),
    'false',
    'the settled snapshot leaves the surface out of busy',
  )
}

test('one snapshot flows to the My Work zone and the Clients page with the same state copy', async () => {
  const handle = consistencyFixture({
    devices: [
      device({ clientId: '100000000001', displayName: 'MacBook Pro', occupancy: 'available' }),
      device({
        clientId: '100000000002',
        displayName: 'Team Mac Studio',
        occupancy: 'occupied-by-other',
      }),
    ],
  })
  await handle.model.start()
  assert.equal(handle.clients.state.devicesStatus, 'loaded')
  assertSurfacesAgree(handle, [
    device({ clientId: '100000000001', displayName: 'MacBook Pro', occupancy: 'available' }),
    device({
      clientId: '100000000002',
      displayName: 'Team Mac Studio',
      occupancy: 'occupied-by-other',
    }),
  ])
  handle.occupancy.close()
  handle.clientsPage.close()
  handle.myWork.close()
})

test('draining and recovery-pending states reach both surfaces; only the card shows the deadline', async () => {
  const handle = consistencyFixture({
    devices: [
      device({ clientId: '100000000001', displayName: 'MacBook Pro', occupancy: 'available' }),
    ],
  })
  await handle.model.start()

  const next = [
    device({
      clientId: '100000000001',
      displayName: 'MacBook Pro',
      occupancy: 'occupied-by-me',
      capacityUsed: 2,
    }),
    device({
      clientId: '100000000002',
      displayName: 'Draining Rig',
      occupancy: 'draining',
      capacityUsed: 1,
    }),
    device({
      clientId: '100000000003',
      displayName: 'Recovering Rig',
      occupancy: 'recovery-pending',
      capacityUsed: 1,
      recoveryDeadlineAt: futureDeadline,
    }),
  ]
  handle.directory.devices = next
  await handle.clients.refresh()
  assertSurfacesAgree(handle, next)

  // The interaction surface (the Clients page card) carries the §12.4 window;
  // the status zone stays at the same state copy without inventing details.
  const recoveringCard = cardFor(handle.clientsRoot, 'Recovering Rig')
  const recovery = descendants(recoveringCard)
    .find(child => child.className === 'wwc-clients-card-recovery')
  assert.notEqual(recovery, undefined)
  assert.equal(recovery.hidden, false)
  assert.equal(recovery.textContent, `Connection interrupted · recovers by ${futureDeadline}`)
  assert.equal(
    stateText(rowFor(handle.myWorkRoot, 'Recovering Rig')),
    stateText(recoveringCard),
    'both surfaces still name the identical recovery state',
  )
  handle.occupancy.close()
  handle.clientsPage.close()
  handle.myWork.close()
})

test('a failed device read marks both surfaces and keeps the same served snapshot', async () => {
  const handle = consistencyFixture({
    devices: [device({ displayName: 'MacBook Pro', occupancy: 'occupied-by-me' })],
  })
  await handle.model.start()

  handle.directory.failNextList()
  await handle.clients.refresh()
  assert.equal(handle.clients.state.devicesStatus, 'unavailable')
  assert.equal(handle.model.state.clients.status, 'unavailable')
  assert.equal(handle.model.state.sources.clients, 'unavailable')
  assert.equal(handle.model.state.status, 'partial')

  // Neither surface clears the served rows, and the state copy stays put.
  assert.equal(pageCards(handle.clientsRoot).length, 1)
  assert.equal(zoneRows(handle.myWorkRoot).length, 1)
  assert.equal(
    stateText(rowFor(handle.myWorkRoot, 'MacBook Pro')),
    stateText(cardFor(handle.clientsRoot, 'MacBook Pro')),
  )
  assert.equal(
    byClass(handle.myWorkRoot, 'wwc-my-work-clients-unavailable').hidden,
    false,
    'the zone names the read failure',
  )
  assert.equal(
    byClass(handle.myWorkRoot, 'wwc-my-work-clients-empty').hidden,
    true,
    'a failed read never reads as an empty directory',
  )
  handle.occupancy.close()
  handle.clientsPage.close()
  handle.myWork.close()
})

test('after the failed read, the next good snapshot flows to both surfaces unchanged', async () => {
  const handle = consistencyFixture({
    devices: [device({ displayName: 'MacBook Pro' })],
  })
  await handle.model.start()
  handle.directory.failNextList()
  await handle.clients.refresh()

  const next = [
    device({
      clientId: '100000000002',
      displayName: 'Team Mac Studio',
      occupancy: 'draining',
      capacityUsed: 1,
    }),
  ]
  handle.directory.devices = next
  await handle.clients.refresh()
  assertSurfacesAgree(handle, next)
  assert.equal(
    byClass(handle.myWorkRoot, 'wwc-my-work-clients-unavailable').hidden,
    true,
    'the recovered read clears the zone failure mark',
  )
  assert.equal(
    byClass(handle.clientsRoot, 'wwc-clients-empty').hidden,
    true,
    'the Clients page leaves its empty state hidden while devices are served',
  )
  handle.occupancy.close()
  handle.clientsPage.close()
  handle.myWork.close()
})

test('an empty directory reads as empty on both surfaces with a zero count', async () => {
  const handle = consistencyFixture({ devices: [] })
  await handle.model.start()
  assertSurfacesAgree(handle, [])
  assert.equal(
    byClass(handle.myWorkRoot, 'wwc-my-work-clients-empty').hidden,
    false,
    'the zone reports the empty directory',
  )
  assert.equal(
    byClass(handle.clientsRoot, 'wwc-clients-empty').hidden,
    false,
    'the Clients page reports the empty directory',
  )
  assert.equal(pageCards(handle.clientsRoot).length, 0)
  assert.equal(zoneRows(handle.myWorkRoot).length, 0)
  handle.occupancy.close()
  handle.clientsPage.close()
  handle.myWork.close()
})
