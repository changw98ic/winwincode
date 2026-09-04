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
    'apps/client/tsconfig.strongflow-page-tests.json',
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
  `StrongFlow deep links did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const route = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-route.js',
)).href}`)

const deliveryId = 'dlv_00000000000000000000000001'
const productSessionId = 'psn_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const historyRunId = 'run_00000000000000000000000002'
const evidenceId = 'evd_00000000000000000000000001'
const scope = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}

function fullRoute(overrides = {}) {
  return {
    deliveryId,
    productSessionId,
    stageRunId,
    candidatePath: 'src/deep link.ts',
    candidateView: 'side-by-side',
    comparison: { status: 'none' },
    evidenceTab: 'logs',
    evidenceId,
    ...overrides,
  }
}

function hashWithFile(file) {
  return `#/strongflow?delivery=${deliveryId}&file=${encodeURIComponent(file)}`
}

test('one typed StrongFlow route round-trips Delivery, history, file, layout, and Scope', () => {
  const hash = route.strongFlowRouteHash(
    fullRoute(),
    scope,
    { taskId: 'dtk_portable_task', stageRunId: historyRunId },
  )

  assert.equal(
    hash,
    `#/strongflow?delivery=${deliveryId}`
      + `&session=${productSessionId}`
      + `&stageRun=${stageRunId}`
      + '&file=src%2Fdeep+link.ts'
      + '&view=side-by-side'
      + '&tab=logs'
      + `&evidence=${evidenceId}`
      + `&organizationId=${scope.organizationId}`
      + `&workspaceId=${scope.workspaceId}`
      + `&projectId=${scope.projectId}`
      + `&repositoryId=${scope.repositoryId}`
      + '&task=dtk_portable_task'
      + `&run=${historyRunId}`,
  )
  assert.deepEqual(route.parseStrongFlowRouteHash(hash), fullRoute())
  assert.deepEqual(route.strongFlowCandidateViewFromHash(hash), 'side-by-side')
  assert.deepEqual(route.strongFlowRawCandidateFileFromHash(hash), 'src/deep link.ts')
})

test('file paths with UTF-8 control bytes or Windows drive prefixes fail closed', () => {
  // Control bytes are built from code points so the source never carries them.
  const control = code => `src${String.fromCharCode(code)}`
  for (const file of [
    control(10),
    control(13),
    control(9),
    control(127),
    'C:evil.ts',
    'c:evil.ts',
    'Z:relative.txt',
  ]) {
    const parsed = route.parseStrongFlowRouteHash(hashWithFile(file))
    assert.equal(parsed.candidatePath, null, JSON.stringify(file))
    assert.deepEqual(route.parseStrongFlowRouteHash(route.strongFlowRouteHash(parsed)), parsed)
    // The rejected path is never written back to the canonical route.
    assert.equal(route.strongFlowRouteHash(parsed).includes('file='), false, JSON.stringify(file))
  }
})

test('path traversals, absolute paths, backslashes, and empty segments fail closed', () => {
  for (const file of [
    '../secret',
    'src/../secret',
    '/etc/secret',
    'src\\evil.ts',
    'src/./evil.ts',
    'src/',
    './secret',
  ]) {
    const parsed = route.parseStrongFlowRouteHash(hashWithFile(file))
    assert.equal(parsed.candidatePath, null, JSON.stringify(file))
    assert.equal(route.strongFlowRouteHash(parsed).includes('file='), false, JSON.stringify(file))
  }
})

test('canonical portable file paths and dtk_ task deep links stay valid', () => {
  for (const file of [
    'docs/architecture/deep link file.ts',
    'src/app.ts',
    '1:relative.txt',
    'a'.repeat(4_096),
  ]) {
    const hash = route.strongFlowRouteHash(fullRoute({
      candidatePath: file,
      evidenceTab: 'evidence',
      evidenceId: null,
    }))
    assert.equal(route.parseStrongFlowRouteHash(hash).candidatePath, file, JSON.stringify(file))
    assert.equal(route.strongFlowRawCandidateFileFromHash(hash), file, JSON.stringify(file))
  }
  // A digit before the colon is not a Windows drive prefix and stays portable.
  assert.equal(
    route.parseStrongFlowRouteHash(hashWithFile('1:relative.txt')).candidatePath,
    '1:relative.txt',
  )
  // A `dtk_` Task identity is a portable deep-link value for the history seam.
  const taskHash = route.strongFlowRouteHash(fullRoute({
    candidatePath: 'src/app.ts',
    evidenceTab: 'evidence',
    evidenceId: null,
  }), undefined, { taskId: 'dtk_portable_task', stageRunId: null })
  assert.equal(route.parseStrongFlowRouteHash(taskHash).candidatePath, 'src/app.ts')
  assert.ok(taskHash.includes('task=dtk_portable_task'), taskHash)
})

test('a Candidate file path longer than the canonical bound fails closed', () => {
  const parsed = route.parseStrongFlowRouteHash(hashWithFile(`${'a'.repeat(4_097)}.ts`))
  assert.equal(parsed.candidatePath, null)
  assert.equal(route.strongFlowRouteHash(parsed).includes('file='), false)
})

test('malformed and duplicate route values fail closed per typed field', () => {
  const parsed = (hash) => route.parseStrongFlowRouteHash(hash)
  assert.equal(
    parsed(`#/strongflow?delivery=${deliveryId}&delivery=${deliveryId}`).deliveryId,
    null,
  )
  assert.equal(
    parsed('#/strongflow?delivery=../../secret').deliveryId,
    null,
  )
  assert.equal(
    parsed(`#/strongflow?delivery=${deliveryId}&session=%2500&stageRun=not%20valid`)
      .productSessionId,
    null,
  )
  assert.equal(
    parsed(`#/strongflow?delivery=${deliveryId}&evidence=%3Cscript%3E`).evidenceId,
    null,
  )
  assert.equal(
    parsed(`#/strongflow?delivery=${deliveryId}&file=${deliveryId}&file=src%2Fapp.ts`)
      .candidatePath,
    null,
  )
  assert.equal(
    parsed(`#/strongflow?delivery=${deliveryId}&repositoryId=${scope.repositoryId}`
      + `&repositoryId=${scope.repositoryId}`).candidateView,
    'unified',
  )
})

test('Evidence and layout defaults fill in without inventing a binding identity', () => {
  assert.deepEqual(
    route.parseStrongFlowRouteHash(`#/strongflow?delivery=${deliveryId}`),
    fullRoute({
      productSessionId: null,
      stageRunId: null,
      candidatePath: null,
      candidateView: 'unified',
      evidenceTab: 'evidence',
      evidenceId: null,
    }),
  )
  assert.equal(
    route.strongFlowRouteHash(route.parseStrongFlowRouteHash('#/strongflow')),
    '#/strongflow?view=unified',
  )
  assert.equal(
    route.parseStrongFlowRouteHash('#/strongflow?tab=bogus&evidence=').evidenceTab,
    'evidence',
  )
})
