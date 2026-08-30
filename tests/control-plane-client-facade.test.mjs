import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  ['pnpm', 'exec', 'tsc', '-p', 'apps/client/tsconfig.runtime-tests.json', '--pretty', 'false'],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `ControlPlaneClient facade did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const facade = await import(`${pathToFileURL(resolve(
  root,
  '.cache/control-plane-client-tests/control-plane-client.js',
)).href}?run=${String(Date.now())}`)

const {
  ControlPlaneClientError,
  createControlPlaneClient,
  parseControlPlaneServerUrl,
} = facade

const schemaVersion = 'winwincode/v1'
const actor = { id: 'usr_00000000000000000000000001', kind: 'user' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const productSessionId = 'psn_00000000000000000000000001'
const subscriptionId = 'sub_00000000000000000000000001'

function requestId(value) {
  return `req_${String(value).padStart(26, '0')}`
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

function cancelCommand(value = 1) {
  return {
    schemaVersion,
    requestId: requestId(value),
    actor,
    scope,
    command: 'session.cancel',
    expectedRevision: 7,
    payload: {
      productSessionId,
      reason: 'user requested cancellation',
    },
  }
}

function commandResponse(request, revision = 8) {
  return {
    schemaVersion,
    requestId: request.requestId,
    command: request.command,
    outcome: 'completed',
    previousRevision: revision - 1,
    currentRevision: revision,
    result: {
      id: productSessionId,
      revision,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
      title: 'Facade session',
      state: 'cancelled',
      updatedAt: '2026-08-27T00:00:00.000Z',
    },
  }
}

function deliveryListQuery(value = 2) {
  return {
    schemaVersion,
    requestId: requestId(value),
    actor,
    scope,
    query: 'delivery.list',
    parameters: { states: [] },
    page: { cursor: null, limit: 25 },
  }
}

function queryResponse(request) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result: { kind: 'delivery_page', items: [] },
    page: { hasMore: false, nextCursor: null },
  }
}

function enterpriseOrganizationList(value = 20) {
  return {
    schemaVersion,
    requestId: requestId(value),
    actor,
    scope,
    query: 'enterprise.organization.list',
    parameters: { states: ['active'] },
    page: { cursor: null, limit: 100 },
  }
}

function enterpriseOrganizationUpdate(value = 21) {
  return {
    schemaVersion,
    requestId: requestId(value),
    actor,
    scope,
    command: 'enterprise.organization.update',
    expectedRevision: 7,
    payload: {
      organizationId: scope.organizationId,
      displayName: 'WinWinCode Enterprise',
      slug: 'winwincode-enterprise',
      state: 'active',
    },
  }
}

class FakeSocket {
  readyState = 0
  onopen = null
  onmessage = null
  onclose = null
  onerror = null
  sent = []

  send(payload) {
    this.sent.push(JSON.parse(payload))
  }

  close() {
    this.readyState = 3
  }

  open() {
    this.readyState = 1
    this.onopen?.({})
  }

  closeFromServer(code) {
    this.readyState = 3
    this.onclose?.({ code })
  }
}

test('serverUrl is mandatory, absolute, secret-free, and derives both transports', () => {
  assert.deepEqual(parseControlPlaneServerUrl('https://control.example/root/'), {
    serverUrl: 'https://control.example/root',
    webSocketUrl: 'wss://control.example/root',
  })
  assert.deepEqual(parseControlPlaneServerUrl('http://127.0.0.1:8787'), {
    serverUrl: 'http://127.0.0.1:8787',
    webSocketUrl: 'ws://127.0.0.1:8787',
  })

  for (const value of [
    undefined,
    '',
    '/relative',
    'wss://control.example',
    'https://user:secret@control.example',
    'https://control.example?token=secret',
    'https://control.example/#fragment',
  ]) {
    assert.throws(
      () => parseControlPlaneServerUrl(value),
      error => error instanceof ControlPlaneClientError && error.kind === 'configuration',
    )
  }
})

test('command retries preserve one envelope and use the HTTP URL derived from serverUrl', async () => {
  const requests = []
  let attempts = 0
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/root/',
    maxNetworkRetries: 1,
    async waitBeforeRetry() {},
    transport: {
      async fetch(input, init) {
        requests.push({ input, init: structuredClone(init) })
        attempts += 1
        if (attempts === 1) throw new Error('temporary network failure')
        const request = JSON.parse(init.body)
        return response(200, commandResponse(request))
      },
    },
  })
  const command = cancelCommand()

  const result = await client.command(command)

  assert.equal(result.outcome, 'completed')
  assert.deepEqual(requests.map(request => request.input), [
    'https://control.example/root/api/v1/commands',
    'https://control.example/root/api/v1/commands',
  ])
  assert.equal(requests[0].init.credentials, 'include')
  assert.equal(requests[0].init.body, requests[1].init.body)
  assert.deepEqual(JSON.parse(requests[1].init.body), command)
})

test('query cancellation reaches the transport and never retries the cancelled request', async () => {
  const controller = new AbortController()
  let attempts = 0
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example',
    maxNetworkRetries: 5,
    transport: {
      async fetch(_input, init) {
        attempts += 1
        return new Promise((_resolve, reject) => {
          init.signal?.addEventListener('abort', () => reject(new Error('aborted')), { once: true })
        })
      },
    },
  })
  const pending = client.query(deliveryListQuery(), { signal: controller.signal })
  controller.abort()

  await assert.rejects(
    pending,
    error => error instanceof ControlPlaneClientError
      && error.kind === 'cancelled'
      && error.code === 'REQUEST_CANCELLED',
  )
  assert.equal(attempts, 1)
})

test('enterprise queries and mutations use the one generated facade and preserve revision identity', async () => {
  const requests = []
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example',
    transport: {
      async fetch(input, init) {
        const request = JSON.parse(init.body)
        requests.push({ input, request })
        if (input.endsWith('/queries')) return response(200, {
          schemaVersion,
          requestId: request.requestId,
          query: request.query,
          result: {
            kind: 'enterprise_organization_page',
            snapshotRevision: 7,
            items: [],
          },
          page: { hasMore: false, nextCursor: null },
        })
        return response(200, {
          schemaVersion,
          requestId: request.requestId,
          command: request.command,
          outcome: 'completed',
          previousRevision: request.expectedRevision,
          currentRevision: 8,
          result: {
            id: scope.organizationId,
            displayName: request.payload.displayName,
            slug: request.payload.slug,
            state: request.payload.state,
            revision: 8,
            updatedAt: '2026-08-27T00:00:00.000Z',
          },
        })
      },
    },
  })
  const query = enterpriseOrganizationList()
  const command = enterpriseOrganizationUpdate()

  const snapshot = await client.query(query)
  const mutation = await client.command(command)

  assert.equal(snapshot.query, 'enterprise.organization.list')
  assert.equal(snapshot.result.snapshotRevision, 7)
  assert.equal(mutation.command, 'enterprise.organization.update')
  assert.equal(mutation.previousRevision, command.expectedRevision)
  assert.equal(mutation.currentRevision, 8)
  assert.deepEqual(requests, [
    { input: 'https://control.example/api/v1/queries', request: query },
    { input: 'https://control.example/api/v1/commands', request: command },
  ])
})

test('authentication, authorization, and schema versions use one safe error shape', async () => {
  const accessFailures = []
  let mode = 'authentication'
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example',
    onAccessFailure(error) {
      accessFailures.push(error)
    },
    transport: {
      async fetch(_input, init) {
        const request = JSON.parse(init.body)
        if (mode === 'version') {
          return response(200, { schemaVersion: 'winwincode/v999' })
        }
        if (mode === 'upgrade') {
          return response(426, {
            schemaVersion,
            requestId: request.requestId,
            error: {
              code: 'INVALID_REQUEST',
              message: 'client protocol is unsupported; upgrade to winwincode/v1',
              retryable: false,
              details: {
                reason: 'CLIENT_UPGRADE_REQUIRED',
                supportedSchemaVersion: schemaVersion,
              },
            },
          })
        }
        return response(mode === 'authentication' ? 401 : 403, {
          schemaVersion,
          requestId: request.requestId,
          error: {
            code: mode === 'authentication' ? 'AUTHENTICATION_REQUIRED' : 'PERMISSION_DENIED',
            message: mode === 'authentication' ? 'Sign in is required.' : 'Access is denied.',
            retryable: false,
            details: {},
          },
        })
      },
    },
  })

  await assert.rejects(
    client.query(deliveryListQuery(3)),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'authentication'
      && error.code === 'AUTHENTICATION_REQUIRED',
  )
  mode = 'authorization'
  await assert.rejects(
    client.query(deliveryListQuery(4)),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'authorization'
      && error.code === 'PERMISSION_DENIED',
  )
  mode = 'version'
  await assert.rejects(
    client.query(deliveryListQuery(5)),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'version'
      && error.code === 'SCHEMA_VERSION_MISMATCH'
      && JSON.stringify(error.details) === '{}',
  )
  await assert.rejects(
    client.query({ ...deliveryListQuery(6), schemaVersion: 'winwincode/v999' }),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'version'
      && error.code === 'SCHEMA_VERSION_MISMATCH',
  )
  mode = 'upgrade'
  await assert.rejects(
    client.query(deliveryListQuery(7)),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'version'
      && error.code === 'INVALID_REQUEST'
      && error.requestId === requestId(7)
      && error.details.reason === 'CLIENT_UPGRADE_REQUIRED'
      && error.details.supportedSchemaVersion === schemaVersion,
  )
  assert.deepEqual(accessFailures.map(error => error.kind), [
    'authentication',
    'authorization',
  ])
})

test('subscribe derives one WS URL, reports authorization loss, and closes on cancellation', async () => {
  const urls = []
  const sockets = []
  const accessFailures = []
  const controller = new AbortController()
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/root',
    onAccessFailure(error) {
      accessFailures.push(error)
    },
    transport: {
      createSocket(url) {
        urls.push(url)
        const socket = new FakeSocket()
        sockets.push(socket)
        return socket
      },
    },
  })
  const subscription = client.subscribe({
    subscriptionId,
    subscription: {
      scope,
      stream: { kind: 'product-session', productSessionId },
      eventTypes: ['product-session.changed.v1'],
    },
    signal: controller.signal,
    onEvent() {},
  })

  assert.deepEqual(urls, ['wss://control.example/root/api/v1/events'])
  sockets[0].open()
  assert.equal(sockets[0].sent[0].type, 'transport.subscribe.v1')
  sockets[0].closeFromServer(4403)
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  assert.deepEqual(accessFailures.map(error => [error.kind, error.code]), [
    ['authentication', 'AUTHENTICATION_REQUIRED'],
  ])
  controller.abort()
  assert.equal(sockets[0].readyState, 3)
  assert.equal(subscription.cursor, null)
})

test('apps/client exposes one facade and contains no Worker, Provider, or DSH remote path', () => {
  const source = readFileSync(resolve(root, 'apps/client/src/control-plane-client.ts'), 'utf8')
  const index = readFileSync(resolve(root, 'apps/client/src/index.ts'), 'utf8')
  assert.match(source, /createControlPlaneClient/u)
  assert.match(source, /serverUrl/u)
  assert.doesNotMatch(`${source}\n${index}`, /@deepseek-ai|execution-worker|provider-gateway/iu)
  assert.doesNotMatch(index, /generated\/control-plane-client/u)
})
