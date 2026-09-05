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
    'apps/client/tsconfig.client-occupancy-tests.json',
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
  `Occupancy area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/client-occupancy-tests')
// Plain module paths keep one ControlPlaneClientError class identity across
// the facade, the view-models, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')
const clientsViewModelModule = await cachedModule('clients-view-model.js')
const occupancyViewModelModule = await cachedModule('client-occupancy-view-model.js')
const clientsPageModule = await cachedModule('clients-page.js')

const { ControlPlaneClientError } = facade
const { createClientsViewModel } = clientsViewModelModule
const {
  clientOccupancyFailure,
  clientOccupancyPortFromFacade,
  createClientOccupancyViewModel,
} = occupancyViewModelModule
const { mountClientsPage } = clientsPageModule

const fixedNow = Date.parse('2026-09-04T00:02:00.000Z')

function device(overrides = {}) {
  return {
    clientId: '123456789012',
    displayName: 'Wenjie MacBook Pro',
    presence: 'online',
    occupancy: 'available',
    capacityUsed: 0,
    capacityTotal: 8,
    lastHeartbeatAt: '2026-09-04T00:00:00.000Z',
    version: '1.2.3',
    ...overrides,
  }
}

function occupancyError(code, kind = 'server') {
  return new ControlPlaneClientError({
    kind,
    code,
    message: 'occupancy rejected',
    requestId: null,
    retryable: false,
  })
}

/** One deterministic occupancy port: every call records and waits for a settle. */
function portFake() {
  const calls = []
  const pending = []
  function track(action, input) {
    calls.push({ action, clientId: input.clientId })
    return new Promise((resolvePromise, rejectPromise) => {
      pending.push({ action, resolve: resolvePromise, reject: rejectPromise })
    })
  }
  return {
    calls,
    pending,
    claim(input) { return track('claim', input) },
    release(input) { return track('release', input) },
    cancelAndRelease(input) { return track('cancel-and-release', input) },
  }
}

function clientsFake(devices) {
  let current = devices
  const listCalls = []
  return {
    get devices() { return current },
    set devices(next) { current = next },
    listCalls,
    async addClient() { return current },
    async listClients() {
      listCalls.push(current)
      return current
    },
  }
}

async function occupancyFixture({
  devices = [device()],
  port = portFake(),
  classify = null,
} = {}) {
  const document = new PageDocument()
  const rootElement = new PageElement(document, 'div')
  const client = clientsFake(devices)
  const clientsModel = createClientsViewModel({ client })
  const occupancyModel = createClientOccupancyViewModel({
    port,
    clients: clientsModel,
    ...(classify === null ? {} : { classify }),
  })
  const page = mountClientsPage({
    root: rootElement,
    model: clientsModel,
    occupancy: occupancyModel,
    now: () => fixedNow,
  })
  page.setVisible(true)
  await clientsModel.refresh()
  return { rootElement, client, clientsModel, occupancyModel, page, port }
}

class PageElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
    this.attributes = new Map()
    this.children = []
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
    this.#textContent = ''
    this.parentNode = null
    this.checkValidity = () => true
  }
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

function cardAt(rootElement, index) {
  const cards = pageDescendants(rootElement)
    .filter(candidate => hasClass(candidate, 'wwc-clients-card'))
  assert.ok(cards.length > index, `card ${index} is mounted`)
  return cards[index]
}

function findAllCards(rootElement) {
  return pageDescendants(rootElement)
    .filter(candidate => hasClass(candidate, 'wwc-clients-card'))
}

async function showDevices(fixture, devices) {
  if (devices !== null) fixture.client.devices = devices
  await fixture.clientsModel.refresh()
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

test('occupancy controls cover the five occupancy states and the presence guards', async () => {
  const { rootElement, occupancyModel, page } = await occupancyFixture({
    devices: [
      device({ clientId: '100000000001', occupancy: 'available' }),
      device({ clientId: '100000000002', occupancy: 'occupied-by-me', capacityUsed: 0 }),
      device({ clientId: '100000000003', occupancy: 'occupied-by-me', capacityUsed: 2 }),
      device({ clientId: '100000000004', occupancy: 'occupied-by-other' }),
      device({ clientId: '100000000005', occupancy: 'draining' }),
      device({ clientId: '100000000006', occupancy: 'recovery-pending' }),
      device({ clientId: '100000000007', occupancy: 'available', presence: 'offline' }),
      device({ clientId: '100000000008', occupancy: 'available', presence: 'locked' }),
    ],
  })
  const connectOf = card => findOne(card, 'wwc-clients-card-connect')
  const releaseOf = card => findOne(card, 'wwc-clients-card-release')
  const cancelOf = card => findOne(card, 'wwc-clients-card-cancel-release')

  const available = cardAt(rootElement, 0)
  assert.equal(connectOf(available).disabled, false, 'a free online device offers connect')
  assert.equal(releaseOf(available).hidden, true)
  assert.equal(cancelOf(available).hidden, true)
  assert.equal(findOne(available, 'wwc-clients-card-confirm').hidden, true)

  const idleMine = cardAt(rootElement, 1)
  assert.equal(connectOf(idleMine).disabled, true)
  assert.equal(releaseOf(idleMine).hidden, false, 'the holder can release')
  assert.equal(releaseOf(idleMine).disabled, false)
  assert.equal(cancelOf(idleMine).hidden, true, 'an idle device hides the immediate stop')

  const busyMine = cardAt(rootElement, 2)
  assert.equal(releaseOf(busyMine).hidden, false)
  assert.equal(cancelOf(busyMine).hidden, false, 'a busy holder can stop and release')

  const other = cardAt(rootElement, 3)
  assert.equal(connectOf(other).disabled, true, 'an occupied device never offers connect')
  assert.equal(releaseOf(other).hidden, true)
  assert.equal(cancelOf(other).hidden, true)

  const draining = cardAt(rootElement, 4)
  assert.equal(connectOf(draining).disabled, true)
  assert.equal(releaseOf(draining).hidden, true, 'a draining device has nothing left to release')
  assert.equal(cancelOf(draining).hidden, false)
  assert.equal(cancelOf(draining).disabled, false)

  const recovering = cardAt(rootElement, 5)
  assert.equal(connectOf(recovering).disabled, true, 'recovery blocks preemption')
  assert.equal(releaseOf(recovering).hidden, true)
  assert.equal(cancelOf(recovering).hidden, true)

  assert.equal(connectOf(cardAt(rootElement, 6)).disabled, true, 'an offline device cannot connect')
  assert.equal(connectOf(cardAt(rootElement, 7)).disabled, true, 'a locked device cannot connect')
  assert.equal(occupancyModel.interaction('100000000001').kind, 'rest')
  page.close()
  occupancyModel.close()
})

test('connect submits one deduplicated claim and re-reads the Server snapshot', async () => {
  const fixture = await occupancyFixture({ devices: [device()] })
  const { rootElement, client, occupancyModel, port } = fixture
  const card = cardAt(rootElement, 0)
  const connect = findOne(card, 'wwc-clients-card-connect')
  connect.emit('click')

  assert.deepEqual(port.calls, [{ action: 'claim', clientId: '123456789012' }])
  const busyConnect = findOne(card, 'wwc-clients-card-connect')
  assert.equal(busyConnect.disabled, true, 'the in-flight claim disables the entry')
  assert.equal(busyConnect.textContent, 'Connecting…')
  assert.equal(findOne(card, 'wwc-clients-card-actions').getAttribute('aria-busy'), 'true')
  connect.emit('click')
  connect.emit('click')
  assert.equal(port.calls.length, 1, 'a second click during flight is never repeated')

  // The holder fact arrives through the next Server snapshot, never from the
  // port result: the refreshed list drives the card copy.
  client.devices = [device({ occupancy: 'occupied-by-me' })]
  port.pending[0].resolve()
  await waitFor(
    () => findOne(card, 'wwc-clients-card-state').textContent === 'Online, occupied by you',
    'the refreshed snapshot reaches the card',
  )
  assert.ok(client.listCalls.length >= 2, 'the landed claim re-read the device list')
  assert.equal(occupancyModel.interaction('123456789012').kind, 'rest')
  assert.equal(findOne(card, 'wwc-clients-card-actions').getAttribute('aria-busy'), 'false')
})

test('a busy release requires the explicit confirmation and keeps the failed draft', async () => {
  const { rootElement, occupancyModel, port } = await occupancyFixture({
    devices: [device({ occupancy: 'occupied-by-me', capacityUsed: 2 })],
  })
  const card = cardAt(rootElement, 0)
  findOne(card, 'wwc-clients-card-release').emit('click')

  assert.equal(port.calls.length, 0, 'the dangerous release waits for the explicit accept')
  const confirm = findOne(card, 'wwc-clients-card-confirm')
  assert.equal(confirm.hidden, false)
  assert.equal(
    findOne(card, 'wwc-clients-card-confirm-text').textContent,
    'Releasing now stops new tasks and lets the running tasks finish before the device frees.',
  )
  assert.equal(findOne(card, 'wwc-clients-card-confirm-accept').textContent, 'Release device')

  findOne(card, 'wwc-clients-card-confirm-accept').emit('click')
  assert.deepEqual(port.calls, [{ action: 'release', clientId: '123456789012' }])
  port.pending[0].reject(occupancyError('PERMISSION_DENIED', 'authorization'))

  await waitFor(
    () => occupancyModel.interaction('123456789012').kind === 'failed',
    'the rejection reaches the interaction',
  )
  assert.equal(
    findOne(card, 'wwc-clients-card-error').textContent,
    'You no longer hold this device.',
  )
  assert.equal(findOne(card, 'wwc-clients-card-error').hidden, false)
  assert.equal(
    confirm.hidden,
    false,
    'the armed confirmation survives the failure as the retry draft',
  )

  findOne(card, 'wwc-clients-card-confirm-accept').emit('click')
  assert.equal(port.calls.length, 2, 'the same explicit accept retries the request')
  port.pending[1].resolve()
  await waitFor(
    () => occupancyModel.interaction('123456789012').kind === 'rest',
    'the retry settles',
  )
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, true)
  assert.equal(findOne(card, 'wwc-clients-card-error').hidden, true)
})

test('an idle release frees the device without a confirmation', async () => {
  const { rootElement, occupancyModel, port } = await occupancyFixture({
    devices: [device({ occupancy: 'occupied-by-me', capacityUsed: 0 })],
  })
  const card = cardAt(rootElement, 0)
  findOne(card, 'wwc-clients-card-release').emit('click')
  assert.deepEqual(port.calls, [{ action: 'release', clientId: '123456789012' }])
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, true)
  port.pending[0].resolve()
  await waitFor(() => occupancyModel.interaction('123456789012').kind === 'rest', 'release lands')
})

test('cancel and release always asks first, and Keep drops the armed draft', async () => {
  const { rootElement, occupancyModel, port } = await occupancyFixture({
    devices: [device({ occupancy: 'occupied-by-me', capacityUsed: 3 })],
  })
  const card = cardAt(rootElement, 0)
  const cancel = findOne(card, 'wwc-clients-card-cancel-release')
  cancel.emit('click')

  assert.equal(port.calls.length, 0)
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, false)
  assert.equal(
    findOne(card, 'wwc-clients-card-confirm-text').textContent,
    'Stopping now cancels the running tasks and frees the device immediately.',
  )
  assert.equal(
    findOne(card, 'wwc-clients-card-confirm-accept').textContent,
    'Cancel tasks and release',
  )

  findOne(card, 'wwc-clients-card-confirm-keep').emit('click')
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, true)
  assert.equal(occupancyModel.interaction('123456789012').kind, 'rest')

  cancel.emit('click')
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, false)
  findOne(card, 'wwc-clients-card-confirm-accept').emit('click')
  assert.deepEqual(port.calls, [{ action: 'cancel-and-release', clientId: '123456789012' }])
  port.pending[0].resolve()
  await waitFor(() => occupancyModel.interaction('123456789012').kind === 'rest', 'stop lands')
})

test('a failed claim shows the classified copy and the entry retries', async () => {
  const { rootElement, occupancyModel, port } = await occupancyFixture({
    devices: [device()],
  })
  const card = cardAt(rootElement, 0)
  const connect = findOne(card, 'wwc-clients-card-connect')
  connect.emit('click')
  port.pending[0].reject(occupancyError('RATE_LIMITED'))

  await waitFor(
    () => occupancyModel.interaction('123456789012').kind === 'failed',
    'the claim rejection reaches the interaction',
  )
  assert.equal(
    findOne(card, 'wwc-clients-card-error').textContent,
    'Too many attempts. Wait a moment, then try again.',
  )
  assert.equal(
    findOne(card, 'wwc-clients-card-connect').disabled,
    false,
    'a failed claim leaves the entry usable for the retry',
  )

  findOne(card, 'wwc-clients-card-connect').emit('click')
  assert.equal(port.calls.length, 2, 'the retry submits a fresh claim')
  port.pending[1].resolve()
  await waitFor(() => occupancyModel.interaction('123456789012').kind === 'rest', 'retry lands')
})

test('every taxonomy failure carries its own honest copy', async () => {
  const failures = [
    ['occupied-by-other', 'Another user claimed the device first.'],
    ['not-holder', 'You no longer hold this device.'],
    ['device-offline', 'The device is offline right now.'],
    ['device-locked', 'The device is locked.'],
    ['recovery-pending', 'The device is waiting to recover. Try again after it recovers.'],
    ['rate-limited', 'Too many attempts. Wait a moment, then try again.'],
    ['unavailable', 'The request did not go through. Check the connection and try again.'],
  ]
  for (const [failure, copy] of failures) {
    const { rootElement, occupancyModel, port } = await occupancyFixture({
      devices: [device()],
      classify: () => failure,
    })
    const card = cardAt(rootElement, 0)
    findOne(card, 'wwc-clients-card-connect').emit('click')
    assert.equal(occupancyModel.interaction('123456789012').kind, 'submitting')
    port.pending[0].reject(occupancyError('SOMETHING_ELSE'))
    await waitFor(
      () => occupancyModel.interaction('123456789012').kind === 'failed',
      `the ${failure} rejection reaches the interaction`,
    )
    assert.equal(findOne(card, 'wwc-clients-card-error').hidden, false)
    assert.equal(findOne(card, 'wwc-clients-card-error').textContent, copy)
    occupancyModel.dismiss('123456789012')
    assert.equal(findOne(card, 'wwc-clients-card-error').hidden, true)
    occupancyModel.close()
  }
})

test('the provisional classifier maps the stable wire codes', () => {
  assert.equal(
    clientOccupancyFailure(occupancyError('OCCUPANCY_HELD_BY_OTHER')),
    'occupied-by-other',
  )
  assert.equal(
    clientOccupancyFailure(occupancyError('OCCUPANCY_NOT_HELD')),
    'not-holder',
  )
  assert.equal(clientOccupancyFailure(occupancyError('CLIENT_OFFLINE')), 'device-offline')
  assert.equal(clientOccupancyFailure(occupancyError('CLIENT_LOCKED')), 'device-locked')
  assert.equal(
    clientOccupancyFailure(occupancyError('OCCUPANCY_RECOVERY_PENDING')),
    'recovery-pending',
  )
  assert.equal(clientOccupancyFailure(occupancyError('RATE_LIMITED')), 'rate-limited')
  assert.equal(
    clientOccupancyFailure(occupancyError('PERMISSION_DENIED', 'authorization')),
    'not-holder',
  )
  assert.equal(
    clientOccupancyFailure(occupancyError('SOMETHING_ELSE')),
    'unavailable',
    'an unknown wire code stays honestly unavailable',
  )
  assert.equal(clientOccupancyFailure(new Error('boom')), 'unavailable')
})

test('a snapshot that moves past the armed draft drops it without submitting', async () => {
  const fixture = await occupancyFixture({
    devices: [device({ occupancy: 'occupied-by-me', capacityUsed: 1 })],
  })
  const { rootElement, occupancyModel, port } = fixture
  const card = cardAt(rootElement, 0)
  findOne(card, 'wwc-clients-card-release').emit('click')
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, false)

  // The Server hands the device to another user while the draft is armed.
  await showDevices(fixture, [device({ occupancy: 'occupied-by-other' })])
  assert.equal(
    occupancyModel.interaction('123456789012').kind,
    'rest',
    'the stale draft is dropped',
  )
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, true)
  assert.equal(findOne(card, 'wwc-clients-card-release').hidden, true)
  assert.equal(port.calls.length, 0, 'a stale draft never reaches the facade')
})

test('the occupancy re-render keeps the card node identity in place', async () => {
  const { rootElement, occupancyModel } = await occupancyFixture({
    devices: [device()],
  })
  const card = cardAt(rootElement, 0)
  const connect = findOne(card, 'wwc-clients-card-connect')
  connect.emit('click')
  await waitFor(
    () => findOne(card, 'wwc-clients-card-connect').disabled,
    'the card re-renders the busy entry',
  )
  assert.equal(cardAt(rootElement, 0), card, 'the interaction never recreates the card')
  occupancyModel.close()
})

test('a port-less composition reports the honest unavailable failure', async () => {
  const document = new PageDocument()
  const rootElement = new PageElement(document, 'div')
  const client = clientsFake([device()])
  const clientsModel = createClientsViewModel({ client })
  const occupancyModel = createClientOccupancyViewModel({
    port: null,
    clients: clientsModel,
  })
  const page = mountClientsPage({
    root: rootElement,
    model: clientsModel,
    occupancy: occupancyModel,
    now: () => fixedNow,
  })
  page.setVisible(true)
  await clientsModel.refresh()
  const card = cardAt(rootElement, 0)
  findOne(card, 'wwc-clients-card-connect').emit('click')
  assert.equal(occupancyModel.interaction('123456789012').kind, 'failed')
  assert.equal(
    findOne(card, 'wwc-clients-card-error').textContent,
    'The request did not go through. Check the connection and try again.',
  )
  page.close()
  occupancyModel.close()
  clientsModel.close()
})

test('the facade adapter separates the holder stop from the Owner force-release and rejects incomplete facades', async () => {
  const calls = []
  // UI-100.3: the landed facade seam — the holder's cancel-and-release goes
  // through releaseOccupancy with the explicit mode, and the Owner recovery
  // cleanup goes through the dedicated force-release entry.
  const completeFacade = {
    claimOccupancy: input => { calls.push(['claim', input.clientId]); return Promise.resolve() },
    releaseOccupancy: input => {
      calls.push(['release', input.clientId, input.mode, input.confirm === true])
      return Promise.resolve()
    },
    forceReleaseOccupancy: input => {
      calls.push(['forceRelease', input.clientId])
      return Promise.resolve()
    },
  }
  const port = clientOccupancyPortFromFacade(completeFacade)
  assert.notEqual(port, null)
  await port.claim({ clientId: '123456789012' })
  await port.cancelAndRelease({ clientId: '123456789012' })
  await port.forceRelease({ clientId: '123456789012' })
  assert.deepEqual(calls, [
    ['claim', '123456789012'],
    ['release', '123456789012', 'cancel_and_release', true],
    ['forceRelease', '123456789012'],
  ])

  // A facade without the force-release seam still serves the holder paths.
  const holderOnly = clientOccupancyPortFromFacade({
    claim: completeFacade.claimOccupancy,
    releaseOccupancy: completeFacade.releaseOccupancy,
  })
  assert.notEqual(holderOnly, null)
  assert.equal(holderOnly.forceRelease, undefined)

  assert.equal(clientOccupancyPortFromFacade({ claim: completeFacade.claimOccupancy }), null)
  assert.equal(
    clientOccupancyPortFromFacade({
      claim: completeFacade.claimOccupancy,
      forceReleaseOccupancy: completeFacade.forceReleaseOccupancy,
    }),
    null,
    'a facade with no holder release path composes no port',
  )
})

test('a failed list read keeps the cards and the armed draft', async () => {
  const fixture = await occupancyFixture({
    devices: [device({ occupancy: 'occupied-by-me', capacityUsed: 1 })],
  })
  const { rootElement, client, clientsModel, occupancyModel } = fixture
  const card = cardAt(rootElement, 0)
  findOne(card, 'wwc-clients-card-cancel-release').emit('click')
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, false)

  client.listClients = async () => {
    throw occupancyError('SERVICE_UNAVAILABLE')
  }
  await clientsModel.refresh()
  assert.equal(findAllCards(rootElement).length, 1, 'the failed read never erases the cards')
  assert.equal(
    occupancyModel.interaction('123456789012').kind,
    'confirming',
    'the armed draft survives a read failure',
  )
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, false)
  occupancyModel.close()
})
