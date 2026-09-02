import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
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
    'apps/client/tsconfig.settings-tests.json',
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
  `Settings client did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const facade = await import(`${pathToFileURL(resolve(
  root,
  '.cache/settings-tests/control-plane-client.js',
)).href}`)
const settingsModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/settings-tests/settings-view-model.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/settings-tests/settings-page.js',
)).href}`)

const { createControlPlaneClient } = facade
const { createSettingsViewModel } = settingsModule
const { mountSettingsPage, settingsPagePresentation } = pageModule
const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const subscriptionId = 'sub_00000000000000000000000001'
const credentialId = 'crd_00000000000000000000000001'
const externalCredentialId = 'crd_00000000000000000000000002'

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function eventId(value) {
  return canonicalId('evt', value)
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function response(status, payload) {
  return {
    ok: status >= 200 && status < 300,
    status,
    async text() {
      return JSON.stringify(payload)
    },
  }
}

function queryResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: page(),
  }
}

function commandResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    command: request.command,
    outcome: 'completed',
    previousRevision: request.expectedRevision,
    currentRevision: result.revision,
    result,
  }
}

function terminalError(request, code, message) {
  return {
    schemaVersion,
    requestId: request.requestId,
    error: { code, message, retryable: false, details: {} },
  }
}

function credential(overrides = {}) {
  return {
    id: credentialId,
    providerId: 'openai-compatible',
    displayName: 'Local model key',
    secretState: 'available',
    rotationVersion: 1,
    lastRotatedAt: '2026-08-27T01:00:00.000Z',
    revokedAt: null,
    revision: 1,
    updatedAt: '2026-08-27T01:00:00.000Z',
    ...overrides,
  }
}

function scopeCursor(sequence = 0) {
  return {
    scope,
    stream: { kind: 'scope' },
    sequence,
    eventId: sequence === 0 ? null : eventId(sequence),
  }
}

function transportLimits() {
  return {
    maxUnackedEvents: 256,
    hardUnackedEvents: 1024,
    ackDeadlineMillis: 30_000,
    backpressureCloseCode: 4408,
  }
}

class FakeWebSocket {
  readyState = 0
  onopen = null
  onmessage = null
  onclose = null
  onerror = null
  sent = []

  send(source) {
    assert.equal(this.readyState, 1)
    this.sent.push(JSON.parse(source))
  }

  close() {
    this.readyState = 3
  }

  open() {
    this.readyState = 1
    this.onopen?.({})
  }

  receive(frame) {
    this.onmessage?.({ data: JSON.stringify(frame) })
  }
}

function contractFake(options = {}) {
  const requests = []
  const sockets = []
  let settings = {
    revision: 1,
    defaultModelRoute: null,
    workerConcurrencyLimit: 2,
  }
  const credentials = new Map()
  const deferredCreates = new Map()

  async function fetch(input, init) {
    const request = JSON.parse(init.body)
    requests.push({ input, request })
    if (input.endsWith('/api/v1/queries')) {
      if (request.query === 'settings.get') {
        return response(200, queryResponse(request, settings))
      }
      if (request.query === 'credential.reference.list') {
        return response(200, queryResponse(request, {
          kind: 'credential_reference_page',
          items: [...credentials.values()],
        }))
      }
    }
    if (request.command === 'settings.update') {
      if (request.expectedRevision !== settings.revision) {
        return response(409, terminalError(request, 'REVISION_CONFLICT', 'Settings changed.'))
      }
      settings = {
        ...request.payload.patch,
        revision: settings.revision + 1,
      }
      return response(200, commandResponse(request, settings))
    }
    if (request.command === 'credential.reference.create') {
      if (
        credentials.has(request.payload.credentialReferenceId)
        || deferredCreates.has(request.payload.credentialReferenceId)
      ) {
        return response(409, terminalError(request, 'REVISION_CONFLICT', 'Reference exists.'))
      }
      const created = credential({
        id: request.payload.credentialReferenceId,
        displayName: request.payload.displayName,
        providerId: request.payload.providerId,
      })
      if (options.deferCreates === true) {
        deferredCreates.set(created.id, created)
        return response(200, {
          schemaVersion,
          requestId: request.requestId,
          command: request.command,
          outcome: 'accepted',
          acceptedAt: '2026-08-27T01:00:01.000Z',
          currentRevision: 0,
        })
      }
      credentials.set(created.id, created)
      return response(200, commandResponse(request, created))
    }
    if (request.command === 'credential.reference.rotate') {
      const current = credentials.get(request.payload.credentialReferenceId)
      if (current?.revision !== request.expectedRevision) {
        return response(409, terminalError(request, 'REVISION_CONFLICT', 'Reference changed.'))
      }
      const rotated = credential({
        ...current,
        revision: current.revision + 1,
        rotationVersion: current.rotationVersion + 1,
        lastRotatedAt: '2026-08-27T01:00:02.000Z',
        updatedAt: '2026-08-27T01:00:02.000Z',
      })
      credentials.set(rotated.id, rotated)
      return response(200, commandResponse(request, rotated))
    }
    if (request.command === 'credential.reference.revoke') {
      const current = credentials.get(request.payload.credentialReferenceId)
      if (current?.revision !== request.expectedRevision) {
        return response(409, terminalError(request, 'REVISION_CONFLICT', 'Reference changed.'))
      }
      const revoked = credential({
        ...current,
        revision: current.revision + 1,
        secretState: 'revoked',
        revokedAt: '2026-08-27T01:00:03.000Z',
        updatedAt: '2026-08-27T01:00:03.000Z',
      })
      credentials.set(revoked.id, revoked)
      return response(200, commandResponse(request, revoked))
    }
    return response(400, terminalError(request, 'INVALID_REQUEST', 'Unsupported request.'))
  }

  return {
    credentials,
    requests,
    sockets,
    fetch,
    createSocket() {
      const socket = new FakeWebSocket()
      sockets.push(socket)
      return socket
    },
    applyDeferredCreates() {
      for (const [id, created] of deferredCreates) credentials.set(id, created)
      deferredCreates.clear()
    },
    get settings() { return settings },
    set settings(value) { settings = value },
  }
}

async function flush() {
  await new Promise(resolvePromise => setTimeout(resolvePromise, 1))
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  await new Promise(resolvePromise => setImmediate(resolvePromise))
}

test('Provider settings use one facade, submit secrets once, and reload revoked metadata in real time', async () => {
  const fake = contractFake()
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/local',
    maxNetworkRetries: 0,
    reconnectDelayMillis: 0,
    transport: { fetch: fake.fetch, createSocket: fake.createSocket },
  })
  let nextRequest = 0
  const model = createSettingsViewModel({
    client,
    actor,
    scope,
    subscriptionId,
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
  })

  await model.start()
  assert.equal(model.state.settings.revision, 1)
  assert.deepEqual(model.state.credentials, [])
  assert.equal(fake.sockets.length, 1)
  const socket = fake.sockets[0]
  socket.open()
  assert.deepEqual(socket.sent[0], {
    type: 'transport.subscribe.v1',
    subscriptionId,
    subscription: {
      scope,
      stream: { kind: 'scope' },
      eventTypes: ['activity.recorded.v1'],
    },
    startAt: 'latest',
  })
  socket.receive({
    type: 'transport.subscription-accepted.v1',
    subscriptionId,
    cursor: scopeCursor(),
    authorizationEpoch: 1,
    limits: transportLimits(),
  })

  const createSecret = 'LOCAL_SECRET_CREATE_SENT_ONCE'
  await model.createCredentialReference({
    credentialReferenceId: credentialId,
    displayName: 'Local model key',
    providerId: 'openai-compatible',
    vaultLocator: createSecret,
  })
  const createRequest = fake.requests.find(({ request }) => (
    request.command === 'credential.reference.create'
  ))
  assert.equal(createRequest.request.expectedRevision, 0)
  assert.equal(createRequest.request.payload.vaultLocator, createSecret)
  assert.equal(createRequest.input, 'https://control.example/local/api/v1/commands')
  assert.equal(createRequest.input.includes(createSecret), false)
  assert.equal(JSON.stringify(model.state).includes(createSecret), false)
  assert.equal(JSON.stringify(model.state).includes('vaultLocator'), false)
  assert.equal(model.state.credentials[0].secretState, 'available')

  await model.updateSettings({
    defaultModelRoute: {
      providerId: 'openai-compatible',
      modelId: 'local-model',
      credentialReferenceId: credentialId,
    },
    workerConcurrencyLimit: 4,
  })
  const updateRequest = fake.requests.find(({ request }) => request.command === 'settings.update')
  assert.equal(updateRequest.request.expectedRevision, 1)
  assert.deepEqual(updateRequest.request.payload.patch.defaultModelRoute, {
    providerId: 'openai-compatible',
    modelId: 'local-model',
    credentialReferenceId: credentialId,
  })
  assert.equal(model.state.settings.revision, 2)

  const rotateSecret = 'LOCAL_SECRET_ROTATE_SENT_ONCE'
  await model.rotateCredentialReference({
    credentialReferenceId: credentialId,
    vaultLocator: rotateSecret,
  })
  const rotateRequest = fake.requests.find(({ request }) => (
    request.command === 'credential.reference.rotate'
  ))
  assert.equal(rotateRequest.request.expectedRevision, 1)
  assert.equal(rotateRequest.request.payload.vaultLocator, rotateSecret)
  assert.equal(rotateRequest.input.includes(rotateSecret), false)
  assert.equal(JSON.stringify(model.state).includes(rotateSecret), false)
  assert.equal(model.state.credentials[0].rotationVersion, 2)

  await model.revokeCredentialReference(credentialId)
  const revokeRequest = fake.requests.find(({ request }) => (
    request.command === 'credential.reference.revoke'
  ))
  assert.equal(revokeRequest.request.expectedRevision, 2)
  assert.equal(model.state.credentials[0].secretState, 'revoked')

  const externallyRevoked = credential({
    id: externalCredentialId,
    displayName: 'Externally revoked key',
    revision: 1,
    secretState: 'revoked',
    revokedAt: '2026-08-27T01:00:04.000Z',
    updatedAt: '2026-08-27T01:00:04.000Z',
  })
  fake.credentials.set(externalCredentialId, externallyRevoked)
  const readsBefore = fake.requests.filter(({ request }) => (
    request.query === 'credential.reference.list'
  )).length
  socket.receive({
    type: 'event.v1',
    subscriptionId,
    eventId: eventId(1),
    scope,
    stream: { kind: 'scope' },
    sequence: 1,
    occurredAt: '2026-08-27T01:00:04.000Z',
    authorizationEpoch: 1,
    source: { kind: 'control-plane', component: 'settings-contract-fake', actor },
    event: {
      type: 'activity.recorded.v1',
      actor,
      category: 'security',
      summary: 'Credential reference revoked.',
    },
  })
  await flush()
  assert.equal(fake.requests.filter(({ request }) => (
    request.query === 'credential.reference.list'
  )).length, readsBefore + 1)
  const reloadedReference = model.state.credentials.find(item => item.id === externalCredentialId)
  assert.equal(reloadedReference.secretState, 'revoked')
  assert.equal(reloadedReference.revokedAt, '2026-08-27T01:00:04.000Z')
  assert.equal(socket.sent.at(-1).type, 'transport.ack.v1')
  assert.equal(socket.sent.at(-1).cursor.sequence, 1)

  fake.settings = { ...fake.settings, revision: 3 }
  await model.updateSettings({ defaultModelRoute: null, workerConcurrencyLimit: 3 })
  assert.equal(model.state.interaction.error.code, 'REVISION_CONFLICT')
  assert.equal(
    settingsPagePresentation(model.state).errorText,
    'These settings changed before the update was saved. Review the current snapshot and try again.',
  )

  const mutationRequests = fake.requests.filter(({ request }) => 'command' in request)
  assert.deepEqual(mutationRequests.map(({ request }) => request.command), [
    'credential.reference.create',
    'settings.update',
    'credential.reference.rotate',
    'credential.reference.revoke',
    'settings.update',
  ])
  assert.equal(mutationRequests.every(({ input }) => input.endsWith('/api/v1/commands')), true)
  assert.equal(fake.requests.every(({ input }) => !input.includes('LOCAL_SECRET_')), true)
  model.close()
  client.close()
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
  id = ''
  htmlFor = ''
  value = ''
  min = ''
  max = ''
  step = ''
  autocomplete = ''
  spellcheck = true
  selected = false
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

function byClass(rootElement, className) {
  const match = descendants(rootElement).find(node => node.className === className)
  assert.notEqual(match, undefined, `missing .${className}`)
  return match
}

function visibleText(node) {
  return descendants(node).map(current => current.textContent).join(' ')
}

function pageState(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    settings: {
      revision: 1,
      defaultModelRoute: null,
      workerConcurrencyLimit: 2,
    },
    credentials: [credential()],
    interaction: { status: 'idle', operation: null, error: null },
    error: null,
    ...overrides,
  }
}

const pageDraftScope = '["settings-page-test-actor","settings-page-test-scope"]'

test('settings keeps write-only inputs only in mounted controls until success or cancel', async () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const calls = []
  let state = pageState()
  let listener = () => {}
  const model = {
    draftScope: pageDraftScope,
    get state() { return state },
    subscribe(next) {
      listener = next
      next(state)
      return () => { listener = () => {} }
    },
    async start() {},
    async refresh() {},
    async updateSettings(input) { calls.push({ operation: 'settings', input }) },
    async createCredentialReference(input) { calls.push({ operation: 'create', input }) },
    async rotateCredentialReference(input) { calls.push({ operation: 'rotate', input }) },
    async revokeCredentialReference(id) { calls.push({ operation: 'revoke', id }) },
    cancelPending() {},
    reconnect() {},
    close() {},
  }
  const mounted = mountSettingsPage({ root: rootElement, model })
  assert.equal(byClass(rootElement, 'wwc-settings').dataset.wwcPage, 'management')
  assert.equal(byClass(rootElement, 'wwc-settings-heading').dataset.wwcComponent, 'page-header')
  assert.equal(byClass(rootElement, 'wwc-settings-status').dataset.wwcComponent, 'status-badge')
  assert.equal(byClass(rootElement, 'wwc-settings-retry').dataset.wwcComponent, 'button')
  assert.equal(byClass(rootElement, 'wwc-settings-route').dataset.wwcComponent, 'panel')
  assert.equal(byClass(rootElement, 'wwc-settings-create-credential').dataset.wwcComponent, 'panel')
  assert.equal(byClass(rootElement, 'wwc-settings-credentials').dataset.wwcComponent, 'panel')

  const createSecret = byClass(rootElement, 'wwc-settings-create-secret')
  const createForm = byClass(rootElement, 'wwc-settings-create-form')
  byClass(rootElement, 'wwc-settings-create-id').value = credentialId
  byClass(rootElement, 'wwc-settings-create-name').value = 'Replacement key'
  byClass(rootElement, 'wwc-settings-create-provider').value = 'openai-compatible'
  createSecret.value = 'PAGE_CREATE_SECRET'
  createForm.dispatch('submit')
  assert.equal(createSecret.value, 'PAGE_CREATE_SECRET')
  assert.equal(calls[0].input.vaultLocator, 'PAGE_CREATE_SECRET')

  state = pageState({
    interaction: {
      status: 'error',
      operation: 'credential.reference.create',
      error: { kind: 'network', code: 'TEST', message: 'offline' },
    },
  })
  listener(state)
  assert.equal(createSecret.value, 'PAGE_CREATE_SECRET')
  state = pageState({
    status: 'cancelled',
    interaction: {
      status: 'error',
      operation: null,
      error: { kind: 'cancelled', code: 'REQUEST_CANCELLED', message: 'cancelled' },
    },
  })
  listener(state)
  assert.equal(createSecret.value, '')

  const rotateSecret = byClass(rootElement, 'wwc-settings-rotate-secret')
  const rotateForm = byClass(rootElement, 'wwc-settings-rotate-form')
  rotateSecret.value = 'PAGE_ROTATE_SECRET'
  rotateForm.dispatch('submit')
  assert.equal(rotateSecret.value, 'PAGE_ROTATE_SECRET')
  assert.equal(calls[1].input.vaultLocator, 'PAGE_ROTATE_SECRET')

  state = pageState({
    interaction: {
      status: 'error',
      operation: 'credential.reference.rotate',
      error: { kind: 'network', code: 'TEST', message: 'offline' },
    },
  })
  listener(state)
  assert.equal(rotateSecret.value, 'PAGE_ROTATE_SECRET')
  rotateForm.dispatch('submit')
  assert.equal(calls[2].input.vaultLocator, 'PAGE_ROTATE_SECRET')
  state = pageState({ credentials: [credential({ revision: 2, rotationVersion: 2 })] })
  listener(state)
  assert.equal(rotateSecret.value, '')

  byClass(rootElement, 'wwc-settings-revoke').dispatch('click')
  assert.equal(calls[3].operation, 'revoke')
  assert.equal(calls[3].id, credentialId)

  assert.equal(visibleText(rootElement).includes('PAGE_CREATE_SECRET'), false)
  assert.equal(visibleText(rootElement).includes('PAGE_ROTATE_SECRET'), false)
  assert.equal(JSON.stringify(state).includes('PAGE_'), false)
  assert.equal(createSecret.type, 'password')
  assert.equal(createSecret.autocomplete, 'new-password')
  assert.equal(rotateSecret.type, 'password')
  assert.equal(rotateSecret.autocomplete, 'new-password')

  state = pageState({
    credentials: [credential({
      secretState: 'revoked',
      revokedAt: '2026-08-27T01:00:03.000Z',
      revision: 2,
    })],
  })
  listener(state)
  assert.equal(visibleText(rootElement).includes('Revoked'), true)
  assert.equal(byClass(rootElement, 'wwc-settings-rotate').disabled, true)
  assert.equal(byClass(rootElement, 'wwc-settings-revoke').disabled, true)

  state = pageState({ credentials: [] })
  listener(state)
  assert.equal(
    byClass(rootElement, 'wwc-settings-credential-empty').dataset.wwcComponent,
    'empty-state',
  )
  mounted.close()
})

test('read-only Settings disables mutation controls and ignores synthetic submits', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const calls = []
  const state = pageState()
  const model = {
    draftScope: pageDraftScope,
    state,
    subscribe(next) { next(state); return () => {} },
    async start() {},
    async refresh() {},
    async updateSettings() { calls.push('settings') },
    async createCredentialReference() { calls.push('create') },
    async rotateCredentialReference() { calls.push('rotate') },
    async revokeCredentialReference() { calls.push('revoke') },
    cancelPending() {},
    reconnect() {},
    close() {},
  }
  const mounted = mountSettingsPage({ root: rootElement, model, readOnly: true })
  for (const className of [
    'wwc-settings-provider',
    'wwc-settings-save-route',
    'wwc-settings-create-submit',
    'wwc-settings-rotate',
    'wwc-settings-revoke',
  ]) assert.equal(byClass(rootElement, className).disabled, true, className)
  byClass(rootElement, 'wwc-settings-route-form').dispatch('submit')
  byClass(rootElement, 'wwc-settings-create-form').dispatch('submit')
  byClass(rootElement, 'wwc-settings-rotate-form').dispatch('submit')
  byClass(rootElement, 'wwc-settings-revoke').dispatch('click')
  assert.deepEqual(calls, [])
  mounted.close()
})

test('an accepted Credential rotation keeps its row submission until the refreshed rotationVersion confirms', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  let current = pageState()
  let listener = () => {}
  const model = {
    draftScope: pageDraftScope,
    get state() { return current },
    subscribe(next) {
      listener = next
      next(current)
      return () => { listener = () => {} }
    },
    async start() {},
    async refresh() {},
    async updateSettings() {},
    async createCredentialReference() {},
    async rotateCredentialReference() {},
    async revokeCredentialReference() {},
    cancelPending() {},
    reconnect() {},
    close() {},
  }
  const mounted = mountSettingsPage({ root: rootElement, model })
  const rotateSecret = byClass(rootElement, 'wwc-settings-rotate-secret')
  rotateSecret.value = 'ASYNC_ROTATE_SECRET'
  rotateSecret.dispatch('input')

  current = pageState({ credentials: [credential({ revision: 2 })] })
  listener(current)
  const conflict = byClass(rootElement, 'wwc-settings-rotate-conflict')
  assert.equal(conflict.hidden, false)
  assert.equal(
    descendants(conflict).some(node => node.getAttribute('aria-hidden') === 'true'),
    true,
  )
  byClass(rootElement, 'wwc-settings-rotate-keep-draft').dispatch('click')
  byClass(rootElement, 'wwc-settings-rotate-form').dispatch('submit')

  current = pageState({
    credentials: [credential({ revision: 2 })],
    interaction: {
      status: 'waiting',
      operation: 'credential.reference.rotate',
      error: null,
    },
  })
  listener(current)
  current = pageState({
    status: 'refreshing',
    realtime: 'reloading',
    credentials: [credential({ revision: 2 })],
  })
  listener(current)
  assert.equal(rotateSecret.value, 'ASYNC_ROTATE_SECRET')

  current = pageState({
    credentials: [credential({ revision: 3, rotationVersion: 2 })],
  })
  listener(current)

  assert.equal(rotateSecret.value, '')
  mounted.close()
})

test('an accepted Credential create settles from the production event reload exactly once', async () => {
  const fake = contractFake({ deferCreates: true })
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/local',
    maxNetworkRetries: 0,
    reconnectDelayMillis: 0,
    transport: { fetch: fake.fetch, createSocket: fake.createSocket },
  })
  let nextRequest = 0
  const model = createSettingsViewModel({
    client,
    actor,
    scope,
    subscriptionId,
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
  })
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const mounted = mountSettingsPage({ root: rootElement, model })
  await model.start()
  assert.equal(model.state.status, 'ready')
  const socket = fake.sockets[0]
  socket.open()
  socket.receive({
    type: 'transport.subscription-accepted.v1',
    subscriptionId,
    cursor: scopeCursor(),
    authorizationEpoch: 1,
    limits: transportLimits(),
  })

  const createSecret = byClass(rootElement, 'wwc-settings-create-secret')
  byClass(rootElement, 'wwc-settings-create-id').value = externalCredentialId
  byClass(rootElement, 'wwc-settings-create-name').value = 'Deferred Credential'
  byClass(rootElement, 'wwc-settings-create-provider').value = 'openai-compatible'
  createSecret.value = 'DEFERRED_CREATE_SECRET'
  byClass(rootElement, 'wwc-settings-create-form').dispatch('submit')
  await flush()
  assert.equal(model.state.interaction.status, 'waiting')
  assert.equal(model.state.credentials.length, 0)
  assert.equal(createSecret.value, 'DEFERRED_CREATE_SECRET')

  fake.applyDeferredCreates()
  socket.receive({
    type: 'event.v1',
    subscriptionId,
    eventId: eventId(1),
    scope,
    stream: { kind: 'scope' },
    sequence: 1,
    occurredAt: '2026-08-27T01:00:05.000Z',
    authorizationEpoch: 1,
    source: { kind: 'control-plane', component: 'settings-contract-fake', actor },
    event: {
      type: 'activity.recorded.v1',
      actor,
      category: 'security',
      summary: 'Credential reference created.',
    },
  })
  await flush()

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.interaction.status, 'idle')
  assert.equal(
    model.state.credentials.some(reference => reference.id === externalCredentialId),
    true,
  )
  assert.equal(createSecret.value, '')
  const createCommands = fake.requests.filter(({ request }) => (
    request.command === 'credential.reference.create'
  ))
  assert.equal(createCommands.length, 1)
  assert.equal(
    fake.requests.every(({ input }) => !input.includes('DEFERRED_CREATE_SECRET')),
    true,
  )
  mounted.close()
  client.close()
})

test('settings keyed updates preserve route drafts, Credential row identity, focus, and scroll', () => {
  const document = new FakeDocument()
  document.activeElement = null
  const rootElement = new FakeElement(document, 'div')
  let current = pageState()
  let listener = () => {}
  const model = {
    draftScope: pageDraftScope,
    get state() { return current },
    subscribe(next) {
      listener = next
      next(current)
      return () => { listener = () => {} }
    },
    async start() {},
    async refresh() {},
    async updateSettings() {},
    async createCredentialReference() {},
    async rotateCredentialReference() {},
    async revokeCredentialReference() {},
    cancelPending() {},
    reconnect() {},
    close() {},
  }
  const mounted = mountSettingsPage({ root: rootElement, model })
  const provider = byClass(rootElement, 'wwc-settings-provider')
  const credentialSelect = byClass(rootElement, 'wwc-settings-credential')
  const references = byClass(rootElement, 'wwc-settings-credential-list')
  const credentialRow = byClass(rootElement, 'wwc-settings-credential-item')
  const rotateSecret = byClass(rootElement, 'wwc-settings-rotate-secret')
  const rotateForm = byClass(rootElement, 'wwc-settings-rotate-form')
  const credentialChoice = credentialSelect.children[1]

  provider.value = 'dirty-provider'
  provider.selectionStart = 6
  provider.dispatch('input')
  credentialSelect.value = credentialId
  credentialSelect.dispatch('change')
  rotateSecret.value = 'dirty-secret'
  rotateSecret.selectionStart = 5
  document.activeElement = rotateSecret
  references.scrollTop = 84

  for (let index = 0; index < 200; index += 1) {
    current = pageState({
      realtime: index % 2 === 0 ? 'reloading' : 'subscribed',
      credentials: [credential({
        displayName: index === 199 ? 'Updated local key' : 'Local model key',
        revision: index + 2,
      })],
    })
    listener(current)
  }

  assert.equal(byClass(rootElement, 'wwc-settings-credential-item'), credentialRow)
  assert.equal(credentialSelect.children[1], credentialChoice)
  assert.equal(byClass(rootElement, 'wwc-settings-credential-title').textContent, 'Updated local key')
  assert.equal(provider.value, 'dirty-provider')
  assert.equal(provider.selectionStart, 6)
  assert.equal(credentialSelect.value, credentialId)
  assert.equal(rotateSecret.value, 'dirty-secret')
  assert.equal(rotateSecret.selectionStart, 5)
  assert.equal(document.activeElement, rotateSecret)
  assert.equal(references.scrollTop, 84)
  assert.equal(references.children.length, 1)
  assert.equal(credentialSelect.children.length, 2)

  mounted.close()
  assert.equal((provider.listeners.get('input') ?? []).length, 0)
  assert.equal((rotateForm.listeners.get('submit') ?? []).length, 0)
  assert.equal(rotateSecret.value, '')
})

test('settings merges clean fields, exposes revision conflicts, and submits one draft snapshot', async () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  let current = pageState({
    settings: {
      revision: 4,
      defaultModelRoute: {
        providerId: 'server-provider-a',
        modelId: 'server-model-a',
        credentialReferenceId: credentialId,
      },
      workerConcurrencyLimit: 2,
    },
  })
  let listener = () => {}
  const calls = []
  let finishUpdate = () => {}
  const model = {
    draftScope: pageDraftScope,
    get state() { return current },
    subscribe(next) {
      listener = next
      next(current)
      return () => { listener = () => {} }
    },
    async start() {},
    async refresh() {},
    updateSettings(input) {
      calls.push(structuredClone(input))
      current = pageState({
        ...current,
        interaction: { status: 'submitting', operation: 'settings.update', error: null },
      })
      listener(current)
      return new Promise(resolve => { finishUpdate = resolve })
    },
    async createCredentialReference() {},
    async rotateCredentialReference() {},
    async revokeCredentialReference() {},
    cancelPending() {},
    reconnect() {},
    close() {},
  }
  const mounted = mountSettingsPage({ root: rootElement, model })
  const provider = byClass(rootElement, 'wwc-settings-provider')
  const modelId = byClass(rootElement, 'wwc-settings-model')
  const concurrency = byClass(rootElement, 'wwc-settings-concurrency')
  const form = byClass(rootElement, 'wwc-settings-route-form')
  const conflict = byClass(rootElement, 'wwc-settings-route-conflict')

  provider.value = 'browser-provider'
  provider.dispatch('input')
  current = pageState({
    settings: {
      revision: 5,
      defaultModelRoute: {
        providerId: 'server-provider-b',
        modelId: 'server-model-b',
        credentialReferenceId: credentialId,
      },
      workerConcurrencyLimit: 3,
    },
  })
  listener(current)

  assert.equal(provider.value, 'browser-provider')
  assert.equal(modelId.value, 'server-model-b')
  assert.equal(concurrency.value, '3')
  assert.equal(conflict.hidden, false)
  assert.match(visibleText(conflict), /Provider ID.*server-provider-b.*browser-provider/u)
  assert.equal(byClass(rootElement, 'wwc-settings-save-route').disabled, true)

  byClass(rootElement, 'wwc-settings-route-keep-draft').dispatch('click')
  assert.equal(conflict.hidden, true)
  assert.equal(byClass(rootElement, 'wwc-settings-save-route').disabled, false)
  form.dispatch('submit')
  assert.deepEqual(calls[0], {
    defaultModelRoute: {
      providerId: 'browser-provider',
      modelId: 'server-model-b',
      credentialReferenceId: credentialId,
    },
    workerConcurrencyLimit: 3,
  })

  provider.value = 'attempted-late-edit'
  provider.dispatch('input')
  assert.equal(provider.value, 'attempted-late-edit')
  current = pageState({
    settings: {
      revision: 6,
      defaultModelRoute: {
        providerId: 'browser-provider',
        modelId: 'server-model-b',
        credentialReferenceId: credentialId,
      },
      workerConcurrencyLimit: 3,
    },
    interaction: { status: 'idle', operation: null, error: null },
  })
  listener(current)
  finishUpdate()
  await Promise.resolve()
  assert.equal(provider.value, 'browser-provider')

  provider.value = 'draft-to-discard'
  provider.dispatch('input')
  current = pageState({
    settings: {
      revision: 7,
      defaultModelRoute: {
        providerId: 'external-provider',
        modelId: 'server-model-c',
        credentialReferenceId: credentialId,
      },
      workerConcurrencyLimit: 4,
    },
  })
  listener(current)
  byClass(rootElement, 'wwc-settings-route-use-server').dispatch('click')
  assert.equal(provider.value, 'external-provider')
  assert.equal(modelId.value, 'server-model-c')
  assert.equal(concurrency.value, '4')
  mounted.close()
})

test('invalid settings scope fails before transport and produces a clear page prompt', async () => {
  const fake = contractFake()
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/local',
    maxNetworkRetries: 0,
    transport: { fetch: fake.fetch },
  })
  let nextRequest = 400
  const model = createSettingsViewModel({
    client,
    actor,
    scope: { ...scope, repositoryId: 'not-a-repository-id' },
    subscriptionId,
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
  })
  await model.start()
  assert.equal(fake.requests.length, 0)
  assert.equal(model.state.error.code, 'INVALID_CLIENT_REQUEST')
  assert.equal(
    settingsPagePresentation(model.state).errorText,
    'Check the local user identity and workspace scope configuration, then retry.',
  )
  model.close()
  client.close()
})

test('settings page keeps its network boundary in the view-model and renders only safe errors', () => {
  const pageSource = readFileSync(resolve(root, 'apps/client/src/settings-page.ts'), 'utf8')
  const draftSource = readFileSync(resolve(root, 'apps/client/src/editable-draft.ts'), 'utf8')
  const viewModelSource = readFileSync(
    resolve(root, 'apps/client/src/settings-view-model.ts'),
    'utf8',
  )
  assert.doesNotMatch(pageSource, /\bfetch\s*\(/u)
  assert.doesNotMatch(pageSource, /new\s+WebSocket/u)
  assert.doesNotMatch(pageSource, /innerHTML/u)
  assert.doesNotMatch(pageSource, /console\./u)
  assert.doesNotMatch(`${pageSource}\n${draftSource}`, /localStorage|sessionStorage/u)
  assert.doesNotMatch(viewModelSource, /\bfetch\s*\(/u)
  assert.doesNotMatch(viewModelSource, /new\s+WebSocket/u)
  assert.doesNotMatch(viewModelSource, /console\./u)
  assert.equal((viewModelSource.match(/\.\/control-plane-client\.js/gu) ?? []).length, 1)

  const configuration = pageState({
    status: 'error',
    error: {
      kind: 'configuration',
      code: 'SERVER_URL_COMPONENTS_FORBIDDEN',
      message: 'unsafe raw server message',
      requestId: null,
      retryable: false,
    },
  })
  assert.equal(
    settingsPagePresentation(configuration).errorText,
    'Check the local server URL and workspace scope configuration, then retry.',
  )
  const serverError = pageState({
    status: 'error',
    error: {
      kind: 'server',
      code: 'INTERNAL_ERROR',
      message: 'secret-shaped upstream detail',
      requestId: null,
      retryable: false,
    },
  })
  assert.equal(
    settingsPagePresentation(serverError).errorText,
    'Provider settings could not be updated. Retry, or review the server status.',
  )
})
