import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
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
    'apps/client/tsconfig.readiness-tests.json',
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
  `Readiness boundary did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const modelModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/readiness-tests/readiness-view-model.js',
)).href}`)
const facadeModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/readiness-tests/control-plane-client.js',
)).href}`)
const { createReadinessViewModel } = modelModule
const { ControlPlaneClientError } = facadeModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const repositoryScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const NOW = '2026-09-03T08:30:00.000Z'

const WORKER_STATES = Object.freeze(['enabled', 'draining', 'offline'])

function response(request, result, page = { hasMore: false, nextCursor: null }) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page,
  }
}

function freshInstallServer() {
  const queries = []
  return {
    queries,
    async query(request) {
      queries.push(structuredClone(request))
      if (request.query === 'model.route.availability.list') return response(request, {
        kind: 'model_route_availability_page',
        scope: repositoryScope,
        requestPoolSource: {
          kind: 'project',
          organizationId: repositoryScope.organizationId,
          workspaceId: repositoryScope.workspaceId,
          projectId: repositoryScope.projectId,
        },
        requestPoolRevision: 1,
        settingsRevision: 1,
        settingsSource: repositoryScope,
        defaultProviderId: null,
        defaultModelId: null,
        status: 'disabled',
        reason: 'no_provider',
        items: [],
      })
      if (request.query === 'credential.reference.list') return response(request, {
        kind: 'credential_reference_page',
        items: [],
      })
      if (request.query === 'worker.list') return response(request, {
        kind: 'worker_page',
        items: [],
      })
      if (request.query === 'session.list') return response(request, {
        kind: 'product_session_page',
        items: [],
      })
      if (request.query === 'delivery.list') return response(request, {
        kind: 'delivery_page',
        items: [],
      })
      throw new Error(`unexpected query: ${request.query}`)
    },
  }
}

function readyServer() {
  const queries = []
  return {
    queries,
    async query(request) {
      queries.push(structuredClone(request))
      if (request.query === 'model.route.availability.list') return response(request, {
        kind: 'model_route_availability_page',
        scope: repositoryScope,
        requestPoolSource: {
          kind: 'project',
          organizationId: repositoryScope.organizationId,
          workspaceId: repositoryScope.workspaceId,
          projectId: repositoryScope.projectId,
        },
        requestPoolRevision: 2,
        settingsRevision: 3,
        settingsSource: repositoryScope,
        defaultProviderId: 'openai',
        defaultModelId: 'gpt-5',
        status: 'enabled',
        reason: 'ready',
        items: [{
          route: {
            providerId: 'openai',
            modelId: 'gpt-5',
            credentialReferenceId: 'crd_00000000000000000000000001',
          },
          status: 'enabled',
          reason: 'ready',
          isDefault: true,
          providerDisplayName: 'OpenAI',
          modelDisplayName: 'GPT-5',
          contextWindowTokens: 400000,
          maxOutputTokens: 128000,
          reasoningEfforts: [],
          toolSupport: 'parallel',
          catalogSource: repositoryScope,
          catalogVersion: 1,
          credentialRotationVersion: 1,
          providerVersion: 1,
          modelVersion: 1,
        }],
      })
      if (request.query === 'credential.reference.list') return response(request, {
        kind: 'credential_reference_page',
        items: [{
          id: 'crd_00000000000000000000000001',
          displayName: 'Primary provider key',
          providerId: 'openai',
          secretState: 'available',
          revokedAt: null,
          lastRotatedAt: '2026-09-01T00:00:00.000Z',
          rotationVersion: 1,
          revision: 1,
          updatedAt: '2026-09-01T00:00:00.000Z',
        }],
      })
      if (request.query === 'worker.list') return response(request, {
        kind: 'worker_page',
        items: [{
          id: 'wrk_00000000000000000000000001',
          state: 'enabled',
          capacity: 2,
          lastHeartbeatAt: NOW,
          revision: 4,
        }],
      })
      if (request.query === 'session.list') return response(request, {
        kind: 'product_session_page',
        items: [{
          id: 'psn_00000000000000000000000001',
          title: 'First Chat',
          projectId: repositoryScope.projectId,
          repositoryId: repositoryScope.repositoryId,
          state: 'active',
          revision: 1,
          createdAt: NOW,
          updatedAt: NOW,
        }],
      })
      if (request.query === 'delivery.list') return response(request, {
        kind: 'delivery_page',
        items: [{
          deliveryId: 'dlv_00000000000000000000000001',
          title: 'First Delivery',
          projectId: repositoryScope.projectId,
          repositoryId: repositoryScope.repositoryId,
          state: 'draft',
          revision: 1,
          createdAt: NOW,
          updatedAt: NOW,
        }],
      })
      throw new Error(`unexpected query: ${request.query}`)
    },
  }
}

function readyContext() {
  return { status: 'ready', actor, scope: repositoryScope }
}

function viewModelOptions(server) {
  return {
    client: {
      serverUrl: 'https://control.example/readiness',
      async restore() { throw new Error('not used') },
      async login() { throw new Error('not used') },
      async logout() {},
      async command() { throw new Error('not used') },
      query(request, options) { return server.query(request, options) },
      subscribe() { throw new Error('not used') },
      close() {},
    },
    serverStatus: () => 'connected',
    now: () => NOW,
    nextRequestId: (() => {
      let request = 0
      return () => {
        request += 1
        return `req_0000000000000000000000000${request}`
      }
    })(),
  }
}

async function settled(viewModel) {
  await viewModel.updateContext(readyContext())
}

test('a fresh install reports every first-run check with a reason and check time', async () => {
  const server = freshInstallServer()
  const viewModel = createReadinessViewModel(viewModelOptions(server))

  const states = []
  viewModel.subscribe(state => { states.push(state) })
  await settled(viewModel)

  const final = viewModel.state
  assert.equal(final.status, 'attention')
  assert.equal(final.collapsed, false)
  assert.deepEqual(final.items.map(item => item.id), [
    'repository-scope',
    'model-route',
    'credential-reference',
    'server-worker-health',
    'helper-availability',
    'first-chat-delivery',
  ])
  assert.deepEqual(final.items, [
    {
      id: 'repository-scope',
      status: 'ready',
      reason: null,
      errorCode: null,
      checkedAt: NOW,
    },
    {
      id: 'model-route',
      status: 'attention',
      reason: 'no-provider',
      errorCode: null,
      checkedAt: NOW,
    },
    {
      id: 'credential-reference',
      status: 'attention',
      reason: 'no-credential-reference',
      errorCode: null,
      checkedAt: NOW,
    },
    {
      id: 'server-worker-health',
      status: 'attention',
      reason: 'no-worker-reported',
      errorCode: null,
      checkedAt: NOW,
    },
    {
      id: 'helper-availability',
      status: 'attention',
      reason: 'no-enabled-worker-capacity',
      errorCode: null,
      checkedAt: NOW,
    },
    {
      id: 'first-chat-delivery',
      status: 'attention',
      reason: 'no-chat-session',
      errorCode: null,
      checkedAt: NOW,
    },
  ])
  const checked = final.items.filter(item => item.status !== 'blocked')
  assert.equal(checked.every(item => item.checkedAt === NOW), true)

  const queryNames = server.queries.map(query => query.query).sort()
  assert.deepEqual(queryNames, [
    'credential.reference.list',
    'delivery.list',
    'model.route.availability.list',
    'session.list',
    'worker.list',
  ])
  assert.equal(server.queries.every(query => (
    query.scope.repositoryId === repositoryScope.repositoryId
    && query.actor.id === actor.id
  )), true)
  viewModel.close()
  assert.equal(viewModel.state.status, 'closed')
})

test('a completed workspace reports every check ready and collapses on request', async () => {
  const server = readyServer()
  const viewModel = createReadinessViewModel(viewModelOptions(server))
  await settled(viewModel)

  assert.equal(viewModel.state.status, 'ready')
  assert.deepEqual(viewModel.state.items.map(item => item.status), [
    'ready',
    'ready',
    'ready',
    'ready',
    'ready',
    'ready',
  ])

  viewModel.setCollapsed(true)
  assert.equal(viewModel.state.collapsed, true)
  viewModel.close()
})

test('a missing repository Scope blocks the dependent checks without product reads', async () => {
  const server = freshInstallServer()
  const viewModel = createReadinessViewModel(viewModelOptions(server))
  await viewModel.updateContext({ status: 'no-scope', reason: 'selection-required' })

  assert.deepEqual(viewModel.state.items[0], {
    id: 'repository-scope',
    status: 'attention',
    reason: 'scope-selection-required',
    errorCode: null,
    checkedAt: NOW,
  })
  assert.equal(server.queries.length, 0)
  for (const item of viewModel.state.items.slice(1)) {
    assert.equal(item.status, 'blocked')
    assert.equal(item.checkedAt, null)
  }
  viewModel.close()
})

test('a failed check query reports an unavailable item with the error code only', async () => {
  const server = freshInstallServer()
  const failingServer = {
    async query(request) {
      if (request.query === 'worker.list') {
        throw new ControlPlaneClientError({
          kind: 'network',
          code: 'NETWORK_UNREACHABLE',
          message: 'secret-ish transport detail',
          requestId: null,
          retryable: true,
        })
      }
      return server.query(request)
    },
  }
  const viewModel = createReadinessViewModel(viewModelOptions(failingServer))
  await settled(viewModel)

  const workerHealth = viewModel.state.items.find(item => item.id === 'server-worker-health')
  assert.deepEqual(workerHealth, {
    id: 'server-worker-health',
    status: 'unavailable',
    reason: null,
    errorCode: 'NETWORK_UNREACHABLE',
    checkedAt: NOW,
  })
  const serialized = JSON.stringify(viewModel.state)
  assert.equal(serialized.includes('secret-ish transport detail'), false)
  viewModel.close()
})

test('an unreachable server fails the server health check without marking it unavailable', async () => {
  const server = freshInstallServer()
  const options = viewModelOptions(server)
  options.serverStatus = () => 'reconnecting'
  const viewModel = createReadinessViewModel(options)
  await settled(viewModel)

  const serverHealth = viewModel.state.items.find(item => item.id === 'server-worker-health')
  assert.equal(serverHealth.status, 'attention')
  assert.equal(serverHealth.reason, 'server-unreachable')
  viewModel.close()
})

test('recheck issues fresh reads after the workspace changed', async () => {
  const server = freshInstallServer()
  const viewModel = createReadinessViewModel(viewModelOptions(server))
  await settled(viewModel)
  assert.equal(viewModel.state.status, 'attention')

  server.query = readyServer().query
  await viewModel.refresh()
  assert.equal(viewModel.state.status, 'ready')
  assert.equal(viewModel.state.items.every(item => item.checkedAt === NOW), true)
  viewModel.close()
})

test('a signed-out context reports the scope check without querying the workspace', async () => {
  const server = freshInstallServer()
  const viewModel = createReadinessViewModel(viewModelOptions(server))
  await viewModel.updateContext({ status: 'signed-out' })

  assert.equal(viewModel.state.items[0].status, 'attention')
  assert.equal(viewModel.state.items[0].reason, 'signed-out')
  assert.equal(server.queries.length, 0)
  viewModel.close()
})

test('changing context detaches an in-flight read so the shared cache can cancel it', async () => {
  let delayedSignal = null
  const server = freshInstallServer()
  const delayedServer = {
    async query(request, requestOptions) {
      if (request.query === 'delivery.list') {
        return new Promise((_resolve, rejectPromise) => {
          delayedSignal = requestOptions?.signal ?? null
          delayedSignal?.addEventListener('abort', () => {
            rejectPromise(new DOMException('aborted', 'AbortError'))
          }, { once: true })
        })
      }
      return server.query(request, requestOptions)
    },
  }
  const viewModel = createReadinessViewModel(viewModelOptions(delayedServer))
  const checking = viewModel.updateContext(readyContext())
  await new Promise(resolvePromise => setTimeout(resolvePromise, 0))
  assert.notEqual(delayedSignal, null, 'the delivery read must be in flight')
  assert.equal(delayedSignal.aborted, false)

  await viewModel.updateContext({ status: 'no-scope', reason: 'selection-required' })
  await new Promise(resolvePromise => setTimeout(resolvePromise, 0))
  assert.equal(delayedSignal.aborted, true, 'a Scope change must cancel the stale read')

  await checking
  viewModel.close()
})

test('a model-route page from another repository is reported unavailable', async () => {
  const server = freshInstallServer()
  const otherRepository = {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000009',
    workspaceId: 'wsp_00000000000000000000000009',
    projectId: 'prj_00000000000000000000000009',
    repositoryId: 'rep_00000000000000000000000009',
  }
  const originalQuery = server.query
  server.query = async request => {
    if (request.query === 'model.route.availability.list') {
      const page = await freshInstallServer().query(request)
      return { ...page, result: { ...page.result, scope: otherRepository } }
    }
    return originalQuery(request)
  }
  const viewModel = createReadinessViewModel(viewModelOptions(server))
  await settled(viewModel)

  const modelRoute = viewModel.state.items.find(item => item.id === 'model-route')
  assert.equal(modelRoute.status, 'unavailable')
  assert.equal(modelRoute.errorCode, 'READINESS_SCOPE_MISMATCH')
  viewModel.close()
})
