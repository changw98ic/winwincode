// [UI-100.3] Occupancy card close-out details, aligned with the landed
// occupancy facade (CLIENT-300.4): the recovery-pending card shows the §12.4
// recovery window and the Owner force-release entry, the draining card keeps
// the explicit cancel-and-release confirmation, and the wire carries the
// recovery deadline fail-closed.
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
  `Occupancy card details did not compile:\n${compiler.stdout}${compiler.stderr}`,
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

const {
  ControlPlaneClientError,
  createControlPlaneClientDirectory,
} = facade
const { createClientsViewModel } = clientsViewModelModule
const {
  clientOccupancyFailure,
  clientOccupancyPortFromFacade,
  createClientOccupancyViewModel,
  deviceRecoveryDeadlineText,
} = occupancyViewModelModule
const { mountClientsPage } = clientsPageModule

const schemaVersion = 'winwincode/v1'
const fixedNow = Date.parse('2026-09-04T00:02:00.000Z')
const futureDeadline = '2026-09-04T01:00:00.000Z'
const pastDeadline = '2026-09-03T23:00:00.000Z'

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

/** One deterministic occupancy port with the Owner seam; every call records. */
function portFake({ withForceRelease = true } = {}) {
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
    ...(withForceRelease
      ? { forceRelease(input) { return track('force-release', input) } }
      : {}),
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

async function fixture({
  devices = [device({ occupancy: 'recovery-pending', recoveryDeadlineAt: futureDeadline })],
  port = portFake(),
} = {}) {
  const document = new PageDocument()
  const rootElement = new PageElement(document, 'div')
  const client = clientsFake(devices)
  const clientsModel = createClientsViewModel({ client })
  const occupancyModel = createClientOccupancyViewModel({ port, clients: clientsModel })
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

async function showDevices(fixtureHandle, devices) {
  fixtureHandle.client.devices = devices
  await fixtureHandle.clientsModel.refresh()
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

test('the recovery-pending card shows the recovery window and the Owner force-release entry', () => {
  const future = deviceRecoveryDeadlineText(
    device({ occupancy: 'recovery-pending', recoveryDeadlineAt: futureDeadline }),
    fixedNow,
  )
  assert.equal(future, `Connection interrupted · recovers by ${futureDeadline}`)
  const passed = deviceRecoveryDeadlineText(
    device({ occupancy: 'recovery-pending', recoveryDeadlineAt: pastDeadline }),
    fixedNow,
  )
  assert.equal(
    passed,
    `Recovery deadline ${pastDeadline} has passed · the device Owner can force-release`,
  )
  const unreported = deviceRecoveryDeadlineText(
    device({ occupancy: 'recovery-pending', recoveryDeadlineAt: null }),
    fixedNow,
  )
  assert.equal(unreported, 'Waiting to recover · no recovery deadline was reported')
  const malformed = deviceRecoveryDeadlineText(
    device({ occupancy: 'recovery-pending', recoveryDeadlineAt: 'not-an-instant' }),
    fixedNow,
  )
  assert.equal(malformed, 'Waiting to recover · no recovery deadline was reported')
})

test('the recovery-pending card prints its deadline and only the Owner entry, no holder actions', async () => {
  const { rootElement, page, occupancyModel } = await fixture({
    devices: [device({ occupancy: 'recovery-pending', recoveryDeadlineAt: futureDeadline })],
  })
  const card = cardAt(rootElement, 0)
  const recovery = findOne(card, 'wwc-clients-card-recovery')
  assert.equal(recovery.hidden, false)
  assert.equal(recovery.textContent, `Connection interrupted · recovers by ${futureDeadline}`)

  const force = findOne(card, 'wwc-clients-card-force-release')
  assert.equal(force.hidden, false, 'the Owner entry is visible on the interrupted card')
  assert.equal(force.disabled, false)
  assert.equal(findOne(card, 'wwc-clients-card-connect').disabled, true)
  assert.equal(findOne(card, 'wwc-clients-card-release').hidden, true)
  assert.equal(findOne(card, 'wwc-clients-card-cancel-release').hidden, true)
  assert.equal(occupancyModel.supportsForceRelease(), true)
  page.close()
  occupancyModel.close()
})

test('a passed recovery window names the Owner cleanup on the card', async () => {
  const { rootElement, page, occupancyModel } = await fixture({
    devices: [device({ occupancy: 'recovery-pending', recoveryDeadlineAt: pastDeadline })],
  })
  const recovery = findOne(cardAt(rootElement, 0), 'wwc-clients-card-recovery')
  assert.equal(
    recovery.textContent,
    `Recovery deadline ${pastDeadline} has passed · the device Owner can force-release`,
  )
  page.close()
  occupancyModel.close()
})

test('force release always asks first, submits through the Owner seam, and re-reads the snapshot', async () => {
  const handle = await fixture()
  const { rootElement, client, occupancyModel, port } = handle
  const card = cardAt(rootElement, 0)
  findOne(card, 'wwc-clients-card-force-release').emit('click')

  assert.deepEqual(port.calls, [], 'the dangerous force release waits for the explicit accept')
  const confirm = findOne(card, 'wwc-clients-card-confirm')
  assert.equal(confirm.hidden, false)
  assert.equal(
    findOne(card, 'wwc-clients-card-confirm-text').textContent,
    'Force-releasing now ends the interrupted occupancy immediately so the device can be claimed again.',
  )
  assert.equal(findOne(card, 'wwc-clients-card-confirm-accept').textContent, 'Force release')

  // Keep drops the armed draft without submitting.
  findOne(card, 'wwc-clients-card-confirm-keep').emit('click')
  assert.equal(confirm.hidden, true)
  assert.equal(occupancyModel.interaction('123456789012').kind, 'rest')

  findOne(card, 'wwc-clients-card-force-release').emit('click')
  findOne(card, 'wwc-clients-card-confirm-accept').emit('click')
  assert.deepEqual(port.calls, [{ action: 'force-release', clientId: '123456789012' }])
  const listCallsBefore = client.listCalls.length
  port.pending[0].resolve()
  await waitFor(
    () => occupancyModel.interaction('123456789012').kind === 'rest',
    'the force release lands',
  )
  assert.ok(client.listCalls.length > listCallsBefore, 'the landed request re-read the device list')
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, true)
  occupancyModel.close()
})

test('a non-Owner denial keeps the armed draft with the honest Owner-only copy', async () => {
  const { rootElement, occupancyModel, port } = await fixture()
  const card = cardAt(rootElement, 0)
  findOne(card, 'wwc-clients-card-force-release').emit('click')
  findOne(card, 'wwc-clients-card-confirm-accept').emit('click')
  port.pending[0].reject(occupancyError('PERMISSION_DENIED', 'authorization'))

  await waitFor(
    () => occupancyModel.interaction('123456789012').kind === 'failed',
    'the denial reaches the interaction',
  )
  assert.equal(occupancyModel.interaction('123456789012').failure, 'permission-denied')
  assert.equal(
    findOne(card, 'wwc-clients-card-error').textContent,
    'Only the device Owner can force-release this device.',
  )
  assert.equal(
    findOne(card, 'wwc-clients-card-confirm').hidden,
    false,
    'the armed confirmation survives the denial as the retry draft',
  )

  findOne(card, 'wwc-clients-card-confirm-accept').emit('click')
  assert.equal(port.calls.length, 2, 'the same explicit accept retries the request')
  port.pending[1].resolve()
  await waitFor(() => occupancyModel.interaction('123456789012').kind === 'rest', 'the retry lands')
  occupancyModel.close()
})

test('a facade without the force-release seam hides the Owner entry but prints the deadline', async () => {
  const { rootElement, page, occupancyModel } = await fixture({
    port: portFake({ withForceRelease: false }),
  })
  const card = cardAt(rootElement, 0)
  assert.equal(occupancyModel.supportsForceRelease(), false)
  assert.equal(findOne(card, 'wwc-clients-card-recovery').hidden, false)
  const force = findOne(card, 'wwc-clients-card-force-release')
  assert.equal(force.hidden, true, 'an uncomposable entry never renders as clickable')
  page.close()
  occupancyModel.close()
})

test('a snapshot that leaves recovery drops the armed force-release draft without submitting', async () => {
  const handle = await fixture()
  const { rootElement, occupancyModel, port } = handle
  const card = cardAt(rootElement, 0)
  findOne(card, 'wwc-clients-card-force-release').emit('click')
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, false)

  // The Server recovered the lease while the draft was armed.
  await showDevices(handle, [device({ occupancy: 'available' })])
  assert.equal(occupancyModel.interaction('123456789012').kind, 'rest', 'the stale draft dropped')
  assert.equal(findOne(card, 'wwc-clients-card-confirm').hidden, true)
  assert.equal(findOne(card, 'wwc-clients-card-recovery').hidden, true)
  assert.equal(findOne(card, 'wwc-clients-card-force-release').hidden, true)
  assert.equal(port.calls.length, 0, 'a stale draft never reaches the facade')
  occupancyModel.close()
})

test('the draining card keeps the explicit cancel-and-release confirmation on the aligned port', async () => {
  const handle = await fixture({
    devices: [device({ occupancy: 'draining', capacityUsed: 2 })],
  })
  const { rootElement, clientsModel, occupancyModel, port } = handle
  const card = cardAt(rootElement, 0)
  assert.equal(findOne(card, 'wwc-clients-card-state').textContent,
    'Online, finishing current tasks')
  assert.equal(findOne(card, 'wwc-clients-card-release').hidden, true)
  const cancel = findOne(card, 'wwc-clients-card-cancel-release')
  assert.equal(cancel.hidden, false)

  cancel.emit('click')
  assert.deepEqual(port.calls, [], 'the destructive stop waits for the explicit accept')
  assert.equal(
    findOne(card, 'wwc-clients-card-confirm-text').textContent,
    'Stopping now cancels the running tasks and frees the device immediately.',
  )
  assert.equal(findOne(card, 'wwc-clients-card-confirm-accept').textContent,
    'Cancel tasks and release')

  findOne(card, 'wwc-clients-card-confirm-accept').emit('click')
  assert.deepEqual(port.calls, [{ action: 'cancel-and-release', clientId: '123456789012' }])
  port.pending[0].resolve()
  await waitFor(
    () => occupancyModel.interaction('123456789012').kind === 'rest',
    'the stop lands',
  )
  assert.equal(clientsModel.state.devicesStatus, 'loaded')
  occupancyModel.close()
})

test('the default classifier sharpens the holder denial per action and reads facade codes', () => {
  assert.equal(
    clientOccupancyFailure(occupancyError('PERMISSION_DENIED', 'authorization')),
    'not-holder',
    'a holder path denial keeps the not-holder copy',
  )
  assert.equal(
    clientOccupancyFailure(occupancyError('PERMISSION_DENIED', 'authorization'), 'force-release'),
    'permission-denied',
  )
  assert.equal(
    clientOccupancyFailure(occupancyError('OCCUPANCY_NOT_HELD'), 'force-release'),
    'permission-denied',
  )
  assert.equal(
    clientOccupancyFailure(occupancyError('OCCUPIED_BY_OTHER')),
    'occupied-by-other',
    'the landed facade wire code maps onto the presentation taxonomy',
  )
  assert.equal(
    clientOccupancyFailure(occupancyError('OCCUPANCY_RECOVERY_PENDING')),
    'recovery-pending',
  )
  assert.equal(
    clientOccupancyFailure(occupancyError('SOMETHING_ELSE'), 'force-release'),
    'unavailable',
    'an unknown wire code stays honestly unavailable',
  )
})

function wireResponse(status, payload) {
  return {
    ok: status >= 200 && status < 300,
    status,
    async text() { return JSON.stringify(payload) },
  }
}

test('the device-list wire carries the recovery deadline fail-closed', async () => {
  const withDeadline = {
    schemaVersion,
    clients: [device({ occupancy: 'recovery-pending', recoveryDeadlineAt: futureDeadline })],
  }
  const directory = createControlPlaneClientDirectory({
    client: { serverUrl: 'https://control.example', close() {} },
    transport: {
      async fetch() { return wireResponse(200, withDeadline) },
    },
  })
  const devices = await directory.listClients()
  assert.deepEqual(devices, [
    device({ occupancy: 'recovery-pending', recoveryDeadlineAt: futureDeadline }),
  ])
  assert.equal(Object.isFrozen(devices[0]), true)

  const absent = createControlPlaneClientDirectory({
    client: { serverUrl: 'https://control.example', close() {} },
    transport: {
      async fetch() { return wireResponse(200, { schemaVersion, clients: [device()] }) },
    },
  })
  const plain = await absent.listClients()
  assert.equal('recoveryDeadlineAt' in plain[0], false, 'an absent field stays absent')

  const malformed = createControlPlaneClientDirectory({
    client: { serverUrl: 'https://control.example', close() {} },
    transport: {
      async fetch() {
        return wireResponse(200, {
          schemaVersion,
          clients: [device({ recoveryDeadlineAt: 'yesterday-ish' })],
        })
      },
    },
  })
  await assert.rejects(
    malformed.listClients(),
    error => error.code === 'INVALID_CLIENT_DIRECTORY_RESPONSE',
  )
})
