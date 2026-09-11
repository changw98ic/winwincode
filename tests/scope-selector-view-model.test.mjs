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

const { createScopeSelectorViewModel } = await import(`${pathToFileURL(resolve(
  root,
  '.cache/scope-selector-tests/scope-selector-view-model.js',
)).href}`)

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

function model(selection = {
  organizationId: scopes[0].organizationId,
  workspaceId: scopes[0].workspaceId,
  projectId: scopes[0].projectId,
  repositoryId: scopes[0].repositoryId,
}, onSelectionChange) {
  return createScopeSelectorViewModel({
    authorizedScopes: scopes,
    selection,
    onSelectionChange,
  })
}

test('AuthSession scopes are the selector option and label authority', async () => {
  const selector = model()
  await selector.start()

  assert.equal(selector.state.status, 'ready')
  assert.deepEqual(selector.state.options.organizations, scopes.map(scope => ({
    id: scope.organizationId,
    label: scope.organizationId,
  })))
  assert.deepEqual(selector.state.options.projects, [{
    id: scopes[0].projectId,
    label: scopes[0].projectId,
  }])
  assert.deepEqual(selector.state.options.repositories, [{
    id: scopes[0].repositoryId,
    label: scopes[0].repositoryId,
  }])
  selector.close()
})

test('ancestor changes clear descendants and reject unauthorized values', async () => {
  const changes = []
  const selector = model(undefined, selection => changes.push(structuredClone(selection)))
  await selector.selectOrganization(scopes[1].organizationId)

  assert.deepEqual(selector.state.selection, {
    organizationId: scopes[1].organizationId,
    workspaceId: null,
    projectId: null,
    repositoryId: null,
  })
  assert.deepEqual(changes, [selector.state.selection])
  assert.throws(
    () => selector.selectWorkspace(scopes[0].workspaceId),
    error => error.code === 'SCOPE_SELECTION_NOT_AUTHORIZED',
  )
  selector.close()
})
