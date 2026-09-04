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
    'apps/client/tsconfig.scope-selector-tests.json',
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
  `Scope selector boundary did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const modelModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/scope-selector-tests/scope-selector-view-model.js',
)).href}`)
const facadeModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/scope-selector-tests/control-plane-client.js',
)).href}`)
const { createScopeSelectorViewModel } = modelModule
const { ControlPlaneClientError } = facadeModule

const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scopes = [
  {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001',
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
  },
  {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000002',
    workspaceId: 'wsp_00000000000000000000000002',
    projectId: 'prj_00000000000000000000000002',
    repositoryId: 'rep_00000000000000000000000002',
  },
]

function response(request, result) {
  return {
    schemaVersion: 'winwincode/v1',
    requestId: request.requestId,
    query: request.query,
    result,
    page: { hasMore: false, nextCursor: null },
  }
}

function clientFake(queryImplementation) {
  const calls = []
  return {
    calls,
    serverUrl: 'https://control.example/scope-selector',
    async restore() { throw new Error('not used') },
    async login() { throw new Error('not used') },
    async logout() {},
    async command() { throw new Error('not used') },
    async query(request, options) {
      calls.push({ request: structuredClone(request), signal: options?.signal })
      return queryImplementation(request, options)
    },
    subscribe() { throw new Error('not used') },
    close() {},
  }
}

function modelOptions(client, selection = {
  organizationId: scopes[0].organizationId,
  workspaceId: scopes[0].workspaceId,
  projectId: scopes[0].projectId,
  repositoryId: scopes[0].repositoryId,
}) {
  let request = 0
  return {
    client,
    actor,
    authorizedScopes: scopes,
    selection,
    nextRequestId() {
      request += 1
      return `req_0000000000000000000000000${request}`
    },
  }
}

test('generated organization/project queries enrich only exact AuthSession hierarchy facts', async () => {
  const client = clientFake(async request => {
    if (request.query === 'enterprise.organization.list') return response(request, {
      kind: 'enterprise_organization_page',
      snapshotRevision: 4,
      items: [{
        id: scopes[0].organizationId,
        displayName: 'Acme',
        slug: 'acme',
        state: 'active',
        revision: 4,
        updatedAt: '2026-09-02T00:00:00.000Z',
      }, {
        id: 'org_00000000000000000000000999',
        displayName: 'Untrusted extra organization',
        slug: 'extra',
        state: 'active',
        revision: 1,
        updatedAt: '2026-09-02T00:00:00.000Z',
      }],
    })
    return response(request, {
      kind: 'enterprise_project_repository_page',
      snapshotRevision: 7,
      items: [{
        kind: 'project',
        projectId: scopes[0].projectId,
        displayName: 'Workbench',
        repositoryCount: 1,
        state: 'active',
        revision: 7,
        updatedAt: '2026-09-02T00:00:00.000Z',
      }, {
        kind: 'repository',
        projectId: scopes[0].projectId,
        repositoryId: scopes[0].repositoryId,
        displayName: 'Client',
        defaultBranch: 'main',
        state: 'active',
        revision: 7,
        updatedAt: '2026-09-02T00:00:00.000Z',
      }, {
        kind: 'repository',
        projectId: scopes[0].projectId,
        repositoryId: 'rep_00000000000000000000000999',
        displayName: 'Untrusted extra repository',
        defaultBranch: 'main',
        state: 'active',
        revision: 7,
        updatedAt: '2026-09-02T00:00:00.000Z',
      }],
    })
  })
  const model = createScopeSelectorViewModel(modelOptions(client))

  await model.start()

  assert.equal(model.state.status, 'ready')
  assert.deepEqual(model.state.options.organizations, [{
    id: scopes[0].organizationId,
    label: 'Acme',
  }, {
    id: scopes[1].organizationId,
    label: scopes[1].organizationId,
  }])
  assert.deepEqual(model.state.options.projects, [{
    id: scopes[0].projectId,
    label: 'Workbench',
  }])
  assert.deepEqual(model.state.options.repositories, [{
    id: scopes[0].repositoryId,
    label: 'Client',
  }])
  assert.deepEqual(client.calls.map(call => ({
    query: call.request.query,
    requestId: call.request.requestId,
    scope: call.request.scope,
  })), [{
    query: 'enterprise.organization.list',
    requestId: 'req_00000000000000000000000001',
    scope: scopes[0],
  }, {
    query: 'enterprise.project.list',
    requestId: 'req_00000000000000000000000002',
    scope: scopes[0],
  }])
  model.close()
})

test('changing an ancestor aborts its stale cascade before publishing the new path', async () => {
  const pending = []
  const client = clientFake((request, options) => new Promise((resolvePromise, rejectPromise) => {
    const signal = options.signal
    signal.addEventListener('abort', () => {
      rejectPromise(new DOMException('aborted', 'AbortError'))
    }, { once: true })
    pending.push({ request, resolve: resolvePromise, signal })
  }))
  const changes = []
  const model = createScopeSelectorViewModel({
    ...modelOptions(client, {
      organizationId: scopes[0].organizationId,
      workspaceId: null,
      projectId: null,
      repositoryId: null,
    }),
    onSelectionChange(selection) { changes.push(structuredClone(selection)) },
  })
  const first = model.start()
  await new Promise(resolvePromise => setTimeout(resolvePromise, 0))
  const second = model.selectOrganization(scopes[1].organizationId)

  assert.equal(pending[0].signal.aborted, true)
  assert.deepEqual(changes, [{
    organizationId: scopes[1].organizationId,
    workspaceId: null,
    projectId: null,
    repositoryId: null,
  }])
  pending[1].resolve(response(pending[1].request, {
    kind: 'enterprise_organization_page',
    snapshotRevision: 2,
    items: [],
  }))
  await Promise.all([first, second])
  assert.equal(model.state.selection.organizationId, scopes[1].organizationId)
  assert.deepEqual(model.state.options.workspaces, [
    { id: scopes[1].workspaceId, label: scopes[1].workspaceId },
  ])
  model.close()
})

test('permission and network failures remain visible without expanding authorized options', async () => {
  let failure = new ControlPlaneClientError({
    kind: 'authorization',
    code: 'PERMISSION_DENIED',
    message: 'private details',
    requestId: null,
    retryable: false,
  })
  const client = clientFake(async () => { throw failure })
  const model = createScopeSelectorViewModel(modelOptions(client, {
    organizationId: scopes[0].organizationId,
    workspaceId: null,
    projectId: null,
    repositoryId: null,
  }))

  await model.start()
  assert.equal(model.state.status, 'permission-denied')
  assert.equal(model.state.error.code, 'PERMISSION_DENIED')
  assert.deepEqual(model.state.options.organizations.map(option => option.id), [
    scopes[0].organizationId,
    scopes[1].organizationId,
  ])

  failure = new ControlPlaneClientError({
    kind: 'network',
    code: 'NETWORK_ERROR',
    message: 'private network details',
    requestId: null,
    retryable: true,
  })
  await model.retry()
  assert.equal(model.state.status, 'network-error')
  assert.equal(model.state.error.code, 'NETWORK_ERROR')
  model.close()
})
