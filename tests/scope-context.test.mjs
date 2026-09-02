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

const scopeContext = await import(`${pathToFileURL(resolve(
  root,
  '.cache/scope-selector-tests/core/scope-context.js',
)).href}`)
const {
  resolveScopeContext,
  scopeHash,
  scopeSelectionFromHash,
} = scopeContext

const organizationOne = 'org_00000000000000000000000001'
const organizationTwo = 'org_00000000000000000000000002'
const repositoryOne = {
  kind: 'repository',
  organizationId: organizationOne,
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const repositoryTwo = {
  kind: 'repository',
  organizationId: organizationTwo,
  workspaceId: 'wsp_00000000000000000000000002',
  projectId: 'prj_00000000000000000000000002',
  repositoryId: 'rep_00000000000000000000000002',
}

test('multiple repository grants require explicit selection and expose only factual ancestors', () => {
  const result = resolveScopeContext([repositoryOne, repositoryTwo], '#/chat', 'repository')

  assert.equal(result.status, 'selection-required')
  assert.equal(result.reason, 'multiple-compatible')
  assert.deepEqual(result.options.organizations, [organizationOne, organizationTwo])
  assert.deepEqual(result.options.workspaces, [])
  assert.deepEqual(result.options.projects, [])
  assert.deepEqual(result.options.repositories, [])
})

test('one compatible repository is restored deterministically and can be encoded in the URL', () => {
  const resolved = resolveScopeContext([repositoryOne], '#/strongflow?delivery=dlv_01', 'repository')

  assert.equal(resolved.status, 'selected')
  assert.equal(resolved.source, 'only-compatible')
  assert.deepEqual(resolved.scope, repositoryOne)
  const hash = scopeHash('#/strongflow?delivery=dlv_01', resolved.selection)
  assert.equal(
    hash,
    '#/strongflow?delivery=dlv_01'
      + `&organizationId=${organizationOne}`
      + `&workspaceId=${repositoryOne.workspaceId}`
      + `&projectId=${repositoryOne.projectId}`
      + `&repositoryId=${repositoryOne.repositoryId}`,
  )
  assert.deepEqual(scopeSelectionFromHash(hash), {
    organizationId: organizationOne,
    workspaceId: repositoryOne.workspaceId,
    projectId: repositoryOne.projectId,
    repositoryId: repositoryOne.repositoryId,
  })
})

test('an exact URL scope restores while an unauthorized URL fails closed', () => {
  const authorizedHash = scopeHash('#/chat?session=psn_01', {
    organizationId: organizationTwo,
    workspaceId: repositoryTwo.workspaceId,
    projectId: repositoryTwo.projectId,
    repositoryId: repositoryTwo.repositoryId,
  })
  const restored = resolveScopeContext(
    [repositoryOne, repositoryTwo],
    authorizedHash,
    'repository',
  )
  assert.equal(restored.status, 'selected')
  assert.equal(restored.source, 'url')
  assert.deepEqual(restored.scope, repositoryTwo)

  const revoked = resolveScopeContext([repositoryOne], authorizedHash, 'repository')
  assert.equal(revoked.status, 'denied')
  assert.equal(revoked.reason, 'not-authorized')
  assert.deepEqual(revoked.options.organizations, [organizationOne])
})

test('partial and structurally invalid paths do not silently choose another scope', () => {
  const partial = resolveScopeContext(
    [repositoryOne, repositoryTwo],
    `#/chat?organizationId=${organizationOne}`,
    'repository',
  )
  assert.equal(partial.status, 'selection-required')
  assert.deepEqual(partial.options.workspaces, [repositoryOne.workspaceId])

  const invalid = resolveScopeContext(
    [repositoryOne],
    `#/chat?organizationId=${organizationOne}&projectId=${repositoryOne.projectId}`,
    'repository',
  )
  assert.equal(invalid.status, 'denied')
  assert.equal(invalid.reason, 'invalid-route')
})
