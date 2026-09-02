import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync('corepack', [
  'pnpm',
  'exec',
  'tsc',
  '-p',
  'apps/client/tsconfig.ui-components-tests.json',
  '--pretty',
  'false',
  '--incremental',
  'false',
], { cwd: root, encoding: 'utf8' })
assert.equal(
  compiler.status,
  0,
  `Client reliability modules did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/ui-components-tests')
const reliability = await import(`${pathToFileURL(resolve(
  cache,
  'core/connection-state.js',
)).href}`)
const { ControlPlaneClientError } = await import(`${pathToFileURL(resolve(
  cache,
  'control-plane-client.js',
)).href}`)
const { mountConnectionBar } = await import(`${pathToFileURL(resolve(
  cache,
  'components/connection-bar.js',
)).href}`)
const { mountClientErrorBoundary } = await import(`${pathToFileURL(resolve(
  cache,
  'components/client-error-boundary.js',
)).href}`)

const {
  classifyClientFailure,
  createConnectionMonitor,
  createSafeDiagnostic,
  observeControlPlaneClient,
} = reliability

function error(kind, code, requestId = null, retryable = false) {
  return new ControlPlaneClientError({
    kind,
    code,
    message: 'SECRET_TOKEN /private/repository raw payload',
    requestId,
    retryable,
    details: { raw: 'SECRET_TOKEN' },
  })
}

test('one connection monitor uses the canonical global status vocabulary', () => {
  const times = [
    '2026-09-02T01:00:00.000Z',
    '2026-09-02T01:01:00.000Z',
  ]
  const monitor = createConnectionMonitor({ now: () => times.shift() ?? times[0] })
  assert.equal(monitor.state.status, 'reconnecting')
  monitor.connected('req_00000000000000000000000001')
  assert.equal(monitor.state.status, 'connected')
  assert.equal(monitor.state.lastSuccessfulAt, '2026-09-02T01:00:00.000Z')
  monitor.failure(error('network', 'NETWORK_ERROR', null, true), true)
  assert.equal(monitor.state.status, 'reconnecting')
  monitor.failure(error('network', 'NETWORK_ERROR', null, true), false)
  assert.equal(monitor.state.status, 'offline')
  monitor.authenticationRequired('AUTHENTICATION_REQUIRED')
  assert.equal(monitor.state.status, 'authentication-required')
  monitor.permissionDenied('PERMISSION_DENIED')
  assert.equal(monitor.state.status, 'permission-denied')
  monitor.versionMismatch('SCHEMA_VERSION_UNSUPPORTED')
  assert.equal(monitor.state.status, 'version-mismatch')
  monitor.refreshRequired('SUBSCRIPTION_RESET_REQUIRED')
  assert.equal(monitor.state.status, 'refresh-required')
  monitor.close()
})

test('HTTP and WebSocket activity feed the same connection monitor', async () => {
  const monitor = createConnectionMonitor({ now: () => '2026-09-02T01:00:00.000Z' })
  let subscriptionOptions = null
  let reconnects = 0
  const rawClient = {
    serverUrl: 'https://control.localhost',
    async restore() { return { actor: {}, authorizedScopes: [] } },
    async login() { throw new Error('not used') },
    async logout() {},
    async command() { throw new Error('not used') },
    async query(request) { return { requestId: request.requestId } },
    subscribe(options) {
      subscriptionOptions = options
      return {
        cursor: null,
        resume() {},
        reconnect() { reconnects += 1 },
        close() {},
      }
    },
    close() {},
  }
  const observed = observeControlPlaneClient({
    client: rawClient,
    monitor,
    online: () => true,
  })
  const subscription = observed.client.subscribe({
    subscriptionId: 'sub_00000000000000000000000001',
    subscription: {},
    async onEvent() {},
    async onResetRequired() { return null },
  })

  await subscriptionOptions.onEvent({})
  assert.equal(monitor.state.status, 'connected')
  await subscriptionOptions.onResetRequired({})
  assert.equal(monitor.state.status, 'refresh-required')
  await subscriptionOptions.onAuthorizationRevoked({})
  assert.equal(monitor.state.status, 'permission-denied')
  subscriptionOptions.onError(error('version', 'SCHEMA_VERSION_MISMATCH'))
  assert.equal(monitor.state.status, 'version-mismatch')
  observed.reconnectAll()
  assert.equal(monitor.state.status, 'reconnecting')
  assert.equal(reconnects, 1)
  subscription.close()
  observed.client.close()
  monitor.close()
})

test('failure classification and copied diagnostics keep only allowlisted fields', () => {
  const requestId = 'req_00000000000000000000000001'
  const failure = classifyClientFailure(
    error('authorization', 'PERMISSION_DENIED', requestId),
    'CLIENT_ROUTE_FAILURE',
    true,
  )
  assert.deepEqual(
    {
      category: failure.category,
      code: failure.code,
      requestId: failure.requestId,
      status: failure.connectionStatus,
    },
    {
      category: 'permission',
      code: 'PERMISSION_DENIED',
      requestId,
      status: 'permission-denied',
    },
  )
  const diagnostic = createSafeDiagnostic({
    connection: {
      status: 'permission-denied',
      code: failure.code,
      requestId,
      lastSuccessfulAt: '2026-09-02T00:59:00.000Z',
      revision: 4,
    },
    failure,
    scope: {
      kind: 'repository',
      organizationId: 'org_00000000000000000000000001',
      workspaceId: 'wsp_00000000000000000000000002',
      projectId: 'prj_00000000000000000000000003',
      repositoryId: 'rep_00000000000000000000000004',
      repositoryPath: '/private/repository',
    },
    surface: 'settings',
    generatedAt: '2026-09-02T01:02:00.000Z',
  })
  for (const visible of [
    'connection=permission-denied',
    'code=PERMISSION_DENIED',
    `requestId=${requestId}`,
    'scope=repository:org …000001 / workspace …000002 / project …000003 / repository …000004',
    'lastSuccessfulAt=2026-09-02T00:59:00.000Z',
  ]) assert.equal(diagnostic.includes(visible), true, visible)
  for (const hidden of [
    'SECRET_TOKEN',
    '/private/repository',
    'raw payload',
    'repositoryPath',
  ]) assert.equal(diagnostic.includes(hidden), false, hidden)

  const taintedFailure = classifyClientFailure(
    error('server', 'SECRET_TOKEN', requestId),
    'CLIENT_ROUTE_FAILURE',
    true,
  )
  const taintedDiagnostic = createSafeDiagnostic({
    connection: {
      status: 'refresh-required',
      code: 'SECRET_TOKEN',
      requestId,
      lastSuccessfulAt: 'SECRET_TOKEN',
      revision: 5,
    },
    failure: taintedFailure,
    scope: { kind: 'secret-token', repositoryPath: '/private/repository' },
    surface: 'settings',
    generatedAt: 'SECRET_TOKEN',
  })
  assert.equal(taintedFailure.code, 'CLIENT_ROUTE_FAILURE')
  assert.equal(taintedDiagnostic.includes('SECRET_TOKEN'), false)
  assert.equal(taintedDiagnostic.includes('/private/repository'), false)
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
  #textContent = ''

  get textContent() { return this.#textContent }
  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }

  append(...children) { this.children.push(...children) }
  replaceChildren(...children) { this.children = [...children] }
  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }
  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }
  removeEventListener(name, listener) {
    this.listeners.set(name, (this.listeners.get(name) ?? []).filter(item => item !== listener))
  }
  dispatch(name) {
    for (const listener of this.listeners.get(name) ?? []) listener({ preventDefault() {} })
  }
}

class FakeDocument {
  createElement(tagName) { return new FakeElement(this, tagName) }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function byClass(rootElement, className) {
  const found = descendants(rootElement).find(node => node.className === className)
  assert.notEqual(found, undefined, `missing .${className}`)
  return found
}

test('Connection Bar and Error Boundary expose recovery and copy actions without raw errors', async () => {
  const document = new FakeDocument()
  const copied = []
  let recoveries = 0
  let safeEntries = 0
  const connection = mountConnectionBar({
    document,
    props: {
      state: {
        status: 'offline',
        code: 'NETWORK_ERROR',
        requestId: null,
        lastSuccessfulAt: '2026-09-02T00:59:00.000Z',
        revision: 2,
      },
      diagnostic: 'safe connection diagnostic',
      onRecover() { recoveries += 1 },
      onCopy(value) { copied.push(value) },
    },
  })
  assert.equal(connection.root.dataset.connectionStatus, 'offline')
  assert.equal(byClass(connection.root, 'wwc-connection-status').dataset.wwcComponent, 'status-badge')
  byClass(connection.root, 'wwc-connection-recover').dispatch('click')
  byClass(connection.root, 'wwc-connection-copy').dispatch('click')
  await Promise.resolve()
  assert.equal(recoveries, 1)
  assert.deepEqual(copied, ['safe connection diagnostic'])

  const failure = classifyClientFailure(
    new Error('SECRET_TOKEN /private/repository'),
    'CLIENT_RENDER_FAILURE',
    true,
  )
  const boundary = mountClientErrorBoundary({
    document,
    props: {
      failure,
      diagnostic: 'safe boundary diagnostic',
      onRetry() { recoveries += 1 },
      onSafeEntry() { safeEntries += 1 },
      onCopy(value) { copied.push(value) },
    },
  })
  assert.equal(boundary.root.getAttribute('role'), 'alert')
  assert.equal(descendants(boundary.root).map(node => node.textContent).join(' ').includes('SECRET_TOKEN'), false)
  byClass(boundary.root, 'wwc-client-error-retry').dispatch('click')
  byClass(boundary.root, 'wwc-client-error-safe-entry').dispatch('click')
  byClass(boundary.root, 'wwc-client-error-copy').dispatch('click')
  await Promise.resolve()
  assert.equal(recoveries, 2)
  assert.equal(safeEntries, 1)
  assert.equal(copied.at(-1), 'safe boundary diagnostic')
  boundary.close()
  connection.close()
})

test('application routes and browser failures use one secret-safe boundary', () => {
  const application = readFileSync(resolve(root, 'apps/client/src/application.ts'), 'utf8')
  const boot = readFileSync(resolve(root, 'apps/client/src/boot.ts'), 'utf8')
  assert.match(application, /mountConnectionBar/u)
  assert.match(application, /mountClientErrorBoundary/u)
  assert.match(application, /showRouteFailure\(error, 'CHAT_ROUTE_FAILURE'\)/u)
  assert.match(application, /showRouteFailure\(error, 'STRONGFLOW_ROUTE_FAILURE'\)/u)
  assert.match(application, /showRouteFailure\(error, 'APPROVALS_ROUTE_FAILURE'\)/u)
  assert.match(application, /showRouteFailure\(error, 'ENTERPRISE_ROUTE_FAILURE'\)/u)
  assert.match(application, /addEventListener\('offline'/u)
  assert.match(application, /addEventListener\('unhandledrejection'/u)
  assert.doesNotMatch(application, /console\.|innerHTML|localStorage|sessionStorage/u)
  assert.doesNotMatch(boot, /error\.message|String\(error\)|console\./u)
})
