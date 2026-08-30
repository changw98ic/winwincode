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

function contractFake() {
  const requests = []
  const sockets = []
  let settings = {
    revision: 1,
    defaultModelRoute: null,
    workerConcurrencyLimit: 2,
  }
  const credentials = new Map()

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
      if (credentials.has(request.payload.credentialReferenceId)) {
        return response(409, terminalError(request, 'REVISION_CONFLICT', 'Reference exists.'))
      }
      const created = credential({
        id: request.payload.credentialReferenceId,
        displayName: request.payload.displayName,
        providerId: request.payload.providerId,
      })
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
    this.children = []
  }

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

  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
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

test('settings page clears each write-only input before handing it to the view-model', async () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const calls = []
  let state = pageState()
  let listener = () => {}
  const model = {
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

  const createSecret = byClass(rootElement, 'wwc-settings-create-secret')
  const createForm = byClass(rootElement, 'wwc-settings-create-form')
  byClass(rootElement, 'wwc-settings-create-id').value = credentialId
  byClass(rootElement, 'wwc-settings-create-name').value = 'Replacement key'
  byClass(rootElement, 'wwc-settings-create-provider').value = 'openai-compatible'
  createSecret.value = 'PAGE_CREATE_SECRET'
  createForm.dispatch('submit')
  assert.equal(createSecret.value, '')
  assert.equal(calls[0].input.vaultLocator, 'PAGE_CREATE_SECRET')

  const rotateSecret = byClass(rootElement, 'wwc-settings-rotate-secret')
  const rotateForm = byClass(rootElement, 'wwc-settings-rotate-form')
  rotateSecret.value = 'PAGE_ROTATE_SECRET'
  rotateForm.dispatch('submit')
  assert.equal(rotateSecret.value, '')
  assert.equal(calls[1].input.vaultLocator, 'PAGE_ROTATE_SECRET')

  byClass(rootElement, 'wwc-settings-revoke').dispatch('click')
  assert.equal(calls[2].operation, 'revoke')
  assert.equal(calls[2].id, credentialId)

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
  const viewModelSource = readFileSync(
    resolve(root, 'apps/client/src/settings-view-model.ts'),
    'utf8',
  )
  assert.doesNotMatch(pageSource, /\bfetch\s*\(/u)
  assert.doesNotMatch(pageSource, /new\s+WebSocket/u)
  assert.doesNotMatch(pageSource, /innerHTML/u)
  assert.doesNotMatch(pageSource, /console\./u)
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
