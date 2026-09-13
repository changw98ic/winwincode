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
const { createScopeSelectorViewModel } = modelModule

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

function modelOptions(selection = {
  organizationId: scopes[0].organizationId,
  workspaceId: scopes[0].workspaceId,
  projectId: scopes[0].projectId,
  repositoryId: scopes[0].repositoryId,
}) {
  return {
    authorizedScopes: scopes,
    selection,
  }
}

test('options come only from AuthSession hierarchy with no Enterprise queries', async () => {
  const model = createScopeSelectorViewModel(modelOptions())
  await model.start()

  assert.equal(model.state.status, 'ready')
  assert.deepEqual(model.state.options.organizations, [{
    id: scopes[0].organizationId,
    label: scopes[0].organizationId,
  }, {
    id: scopes[1].organizationId,
    label: scopes[1].organizationId,
  }])
  assert.deepEqual(model.state.options.projects, [{
    id: scopes[0].projectId,
    label: scopes[0].projectId,
  }])
  assert.deepEqual(model.state.options.repositories, [{
    id: scopes[0].repositoryId,
    label: scopes[0].repositoryId,
  }])
  assert.equal(model.state.error, null)
  model.close()
})

test('changing an ancestor updates the projected path without a remote cascade', async () => {
  const changes = []
  const model = createScopeSelectorViewModel({
    ...modelOptions({
      organizationId: scopes[0].organizationId,
      workspaceId: null,
      projectId: null,
      repositoryId: null,
    }),
    onSelectionChange(selection) { changes.push(structuredClone(selection)) },
  })
  await model.start()
  await model.selectOrganization(scopes[1].organizationId)

  assert.deepEqual(changes, [{
    organizationId: scopes[1].organizationId,
    workspaceId: null,
    projectId: null,
    repositoryId: null,
  }])
  assert.equal(model.state.selection.organizationId, scopes[1].organizationId)
  assert.deepEqual(model.state.options.workspaces, [
    { id: scopes[1].workspaceId, label: scopes[1].workspaceId },
  ])
  model.close()
})

test('selecting a non-authorized option fails closed without expanding the set', async () => {
  const model = createScopeSelectorViewModel(modelOptions())
  await model.start()
  assert.throws(
    () => model.selectRepository('rep_00000000000000000000000999'),
    error => error.code === 'SCOPE_SELECTION_NOT_AUTHORIZED',
  )
  model.close()
})

test('empty authorized hierarchy reports empty at organization', async () => {
  const model = createScopeSelectorViewModel({
    authorizedScopes: [],
    selection: {
      organizationId: null,
      workspaceId: null,
      projectId: null,
      repositoryId: null,
    },
  })
  await model.start()
  assert.equal(model.state.status, 'empty')
  assert.equal(model.state.emptyLevel, 'organization')
  model.close()
})
