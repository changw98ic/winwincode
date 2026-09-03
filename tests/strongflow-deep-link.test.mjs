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
const candidateRef = `git-candidate:sha256:${'a'.repeat(64)}`
const scope = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}

test('one typed StrongFlow route round-trips Delivery, history, Candidate, panel, file, line, and Scope', () => {
  const hash = route.strongFlowRouteHash({
    deliveryId,
    productSessionId,
    stageRunId,
    historySelection: {
      taskId: 'task:deep-link',
      stageRunId: historyRunId,
    },
    candidateRef,
    panel: 'candidate',
    candidatePath: 'src/deep link.ts',
    candidateView: 'side-by-side',
    candidateLine: 37,
  }, scope)

  assert.equal(
    hash,
    `#/strongflow?delivery=${deliveryId}`
      + `&session=${productSessionId}`
      + `&stageRun=${stageRunId}`
      + '&task=task%3Adeep-link'
      + `&run=${historyRunId}`
      + `&candidate=${encodeURIComponent(candidateRef)}`
      + '&panel=candidate'
      + '&file=src%2Fdeep+link.ts'
      + '&view=side-by-side'
      + '&line=37'
      + `&organizationId=${scope.organizationId}`
      + `&workspaceId=${scope.workspaceId}`
      + `&projectId=${scope.projectId}`
      + `&repositoryId=${scope.repositoryId}`,
  )
  assert.deepEqual(route.strongFlowRouteRequestFromHash(hash), {
    status: 'valid',
    request: {
      deliveryId,
      productSessionId,
      stageRunId,
      historySelection: {
        taskId: 'task:deep-link',
        stageRunId: historyRunId,
      },
      candidateRef,
      panel: 'candidate',
      candidatePath: 'src/deep link.ts',
      candidateView: 'side-by-side',
      candidateLine: 37,
    },
  })
})

test('malformed, duplicate, and structurally incomplete route values fail closed', () => {
  for (const hash of [
    `#/strongflow?delivery=${deliveryId}&delivery=${deliveryId}`,
    `#/strongflow?delivery=${deliveryId}&repositoryId=${scope.repositoryId}&repositoryId=${scope.repositoryId}`,
    `#/strongflow?delivery=${deliveryId}&token=secret`,
    '#/strongflow?delivery=../../secret',
    `#/strongflow?delivery=${deliveryId}&file=../secret`,
    `#/strongflow?delivery=${deliveryId}&line=9`,
    `#/strongflow?delivery=${deliveryId}&stageRun=${stageRunId}`,
    `#/strongflow?session=${productSessionId}&stageRun=${stageRunId}`,
    `#/strongflow?delivery=${deliveryId}&panel=unknown`,
    `#/strongflow?delivery=${deliveryId}&candidate=git-candidate%3Asha256%3Ashort`,
  ]) assert.deepEqual(route.strongFlowRouteRequestFromHash(hash), {
    status: 'invalid',
    reason: 'invalid-route',
  })
})

function detail(overrides = {}) {
  return {
    kind: 'delivery_detail',
    deliveryId,
    ownership: scope,
    readCursor: { scope: { kind: 'repository', ...scope } },
    tasks: [
      { id: 'task:deep-link', stageRunIds: [historyRunId] },
      { id: 'task:other', stageRunIds: [stageRunId] },
    ],
    stages: [
      {
        id: historyRunId,
        actorType: 'codex',
        deliveryTaskId: 'task:deep-link',
        sessionBinding: {
          productSessionId: 'psn_00000000000000000000000002',
          stageRunId: historyRunId,
        },
      },
      {
        id: stageRunId,
        actorType: 'codex',
        deliveryTaskId: 'task:other',
        sessionBinding: { productSessionId, stageRunId },
      },
    ],
    currentCandidate: { candidateRef },
    ...overrides,
  }
}

function request(overrides = {}) {
  return {
    deliveryId,
    productSessionId,
    stageRunId,
    historySelection: { taskId: 'task:deep-link', stageRunId: historyRunId },
    candidateRef,
    panel: 'candidate',
    candidatePath: 'src/deep-link.ts',
    candidateView: 'unified',
    candidateLine: null,
    ...overrides,
  }
}

test('route resolution keeps exact related identities and fills only omitted canonical values', () => {
  assert.deepEqual(route.resolveStrongFlowRoute(request(), detail(), { kind: 'repository', ...scope }), {
    status: 'selected',
    target: request(),
  })

  const omitted = route.resolveStrongFlowRoute(request({
    deliveryId: null,
    productSessionId: null,
    stageRunId: null,
    historySelection: { taskId: null, stageRunId: historyRunId },
    candidateRef: null,
    panel: null,
    candidatePath: null,
    candidateView: null,
  }), detail(), { kind: 'repository', ...scope })
  assert.equal(omitted.status, 'selected')
  assert.equal(omitted.target.historySelection.taskId, 'task:deep-link')
  assert.equal(omitted.target.stageRunId, stageRunId)
})

test('cross-Scope, stale StageRun/Candidate, deleted Task/Attempt, and crossed Task associations fail closed', () => {
  const repositoryScope = { kind: 'repository', ...scope }
  const cases = [
    [request(), detail(), { ...repositoryScope, repositoryId: 'rep_00000000000000000000000002' }],
    [request({ stageRunId: historyRunId }), detail(), repositoryScope],
    [request({ candidateRef: `git-candidate:sha256:${'b'.repeat(64)}` }), detail(), repositoryScope],
    [request({ historySelection: { taskId: 'task:deleted', stageRunId: null } }), detail(), repositoryScope],
    [request({ historySelection: { taskId: null, stageRunId: 'run_00000000000000000000000003' } }), detail(), repositoryScope],
    [request({ historySelection: { taskId: 'task:other', stageRunId: historyRunId } }), detail(), repositoryScope],
  ]
  for (const args of cases) {
    const result = route.resolveStrongFlowRoute(...args)
    assert.equal(result.status, 'unavailable')
    assert.match(result.message, /StrongFlow|StageRun|Candidate|Task|Attempt/)
  }
})
