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
  `StrongFlow page did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const historySelectionModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-history-selection.js',
)).href}`)
const historyTreeModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-history-tree.js',
)).href}`)
const historyNavigationModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-history-navigation.js',
)).href}`)
const runDetailModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-run-detail.js',
)).href}`)
const page = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-page.js',
)).href}`)

const {
  strongFlowHistorySelectionFromHash,
  strongFlowHistoryHashWithSelection,
  strongFlowHistorySelectionForTree,
} = historySelectionModule
const { strongFlowHistoryTree, strongFlowCancellableWorkRuns, strongFlowCancellationTarget } = historyTreeModule
const { mountStrongFlowHistoryNavigation } = historyNavigationModule
const { mountStrongFlowRunDetail } = runDetailModule
const { mountStrongFlowPage } = page

const deliveryId = 'dlv_00000000000000000000000001'
const currentRunId = 'wrn_00000000000000000000000002'
const failedRunId = 'wrn_00000000000000000000000001'
const planningRunId = 'wrn_00000000000000000000000003'
const reviewRunId = 'wrn_00000000000000000000000004'

const limits = {
  tasks: 100,
  stages: 50,
  attention: 50,
  evidence: 100,
  runtimeSessions: 50,
  graphNodes: 100,
  graphEdges: 200,
  activities: 100,
}

class FakeDeliveryListModel {
  constructor(visible) {
    this.state = {
      status: 'ready',
      filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
      visible,
      loadedCount: visible.length,
      hasMore: false,
      loadingMore: false,
      moreFailure: null,
      error: null,
      advance: { deliveryId: null, failure: null },
    }
  }

  calls = []
  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }

  async refresh() { this.calls.push(['refresh']) }
  async loadMore() { this.calls.push(['loadMore']) }
  setSearch(value) { this.calls.push(['setSearch', value]) }
  async setStatusFilter(value) { this.calls.push(['setStatusFilter', value]) }
  setAttentionOnly(value) { this.calls.push(['setAttentionOnly', value]) }
  setOrder(value) { this.calls.push(['setOrder', value]) }
  async advanceDelivery(id, revision) { this.calls.push(['advanceDelivery', id, revision]) }
  close() { this.calls.push(['close']) }
}

function fakeDeliveryList(visible) {
  return new FakeDeliveryListModel(visible)
}

function historyProjection() {
  const projection = {
    delivery: {
      schemaVersion: 'winwincode/v1',
      deliveryId,
      deliveryRevision: 7,
      status: 'executing',
      ownership: {
        organizationId: 'org_00000000000000000000000001',
        workspaceId: 'wsp_00000000000000000000000001',
        projectId: 'prj_00000000000000000000000001',
        repositoryId: 'rep_00000000000000000000000001',
      },
      requirements: {
        title: 'History navigation Delivery',
        goal: 'Review every historical WorkRun attempt.',
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 3,
      },
      tasks: [
        {
          id: 'task:1',
          title: 'Implement the feature',
          goal: 'Ship the change.',
          status: 'active',
          owner: null,
          acceptanceCriterionIds: [],
          blockedByTaskIds: [],
          evidenceRefs: [],
          workRunIds: [failedRunId, currentRunId],
        },
        {
          id: 'task:2',
          title: 'Document the feature',
          goal: 'Explain the change.',
          status: 'pending',
          owner: null,
          acceptanceCriterionIds: [],
          blockedByTaskIds: [],
          evidenceRefs: [],
          workRunIds: [],
        },
      ],
      stages: [
        {
          id: failedRunId,
          stage: 'executing',
          role: 'implementer',
          status: 'failed',
          attempt: 1,
          actorType: 'codex',
          deliveryTaskId: 'task:1',
          startedAt: '2026-09-02T08:00:00.000Z',
          finishedAt: '2026-09-02T08:10:00.000Z',
          sessionBinding: {
            productSessionId: 'psn_00000000000000000000000001',
            executionJobId: 'job_00000000000000000000000001',
            bindingId: 'bind:1',
            boundAt: '2026-09-02T08:00:00.000Z',
            codexThreadId: 'cdx_00000000000000000000000001',
            fencingToken: 'fence:1',
            leaseId: 'lease:1',
            sessionIdentity: null,
            sourceIdentity: null,
            workRunId: failedRunId,
            workerId: 'wrk_00000000000000000000000001',
            workerSessionId: 'wsn_00000000000000000000000001',
            attempt: 1,
          },
        },
        {
          id: currentRunId,
          stage: 'executing',
          role: 'implementer',
          status: 'running',
          attempt: 2,
          actorType: 'codex',
          deliveryTaskId: 'task:1',
          startedAt: '2026-09-02T08:11:00.000Z',
          finishedAt: null,
          sessionBinding: {
            productSessionId: 'psn_00000000000000000000000002',
            executionJobId: 'job_00000000000000000000000002',
            bindingId: 'bind:2',
            boundAt: '2026-09-02T08:11:00.000Z',
            codexThreadId: 'cdx_00000000000000000000000002',
            fencingToken: 'fence:2',
            leaseId: 'lease:2',
            sessionIdentity: null,
            sourceIdentity: null,
            workRunId: currentRunId,
            workerId: 'wrk_00000000000000000000000002',
            workerSessionId: 'wsn_00000000000000000000000002',
            attempt: 2,
          },
        },
        {
          id: planningRunId,
          stage: 'planning',
          role: 'planner',
          status: 'succeeded',
          attempt: 1,
          actorType: 'codex',
          deliveryTaskId: null,
          startedAt: '2026-09-02T07:50:00.000Z',
          finishedAt: '2026-09-02T07:55:00.000Z',
          sessionBinding: {
            productSessionId: 'psn_00000000000000000000000003',
            executionJobId: 'job_00000000000000000000000003',
            bindingId: 'bind:3',
            boundAt: '2026-09-02T07:50:00.000Z',
            codexThreadId: 'cdx_00000000000000000000000003',
            fencingToken: 'fence:3',
            leaseId: 'lease:3',
            sessionIdentity: null,
            sourceIdentity: null,
            workRunId: planningRunId,
            workerId: 'wrk_00000000000000000000000003',
            workerSessionId: 'wsn_00000000000000000000000003',
            attempt: 1,
          },
        },
        {
          id: reviewRunId,
          stage: 'plan-review',
          role: 'reviewer',
          status: 'succeeded',
          attempt: 1,
          actorType: 'human',
          deliveryTaskId: null,
          startedAt: '2026-09-02T07:56:00.000Z',
          finishedAt: '2026-09-02T07:59:00.000Z',
          sessionBinding: null,
        },
      ],
      attention: [],
      evidence: [
        {
          id: 'evidence:failed',
          type: 'command',
          sourceRef: 'artifact:command:attempt-1',
          candidateRef: 'refs/winwincode/candidate/attempt-1',
          workRunId: failedRunId,
          sessionBindingId: 'bind:1',
          deliverySpecId: 'spec:1',
          deliverySpecRevision: 3,
          createdAt: '2026-09-02T08:09:00.000Z',
        },
        {
          id: 'evidence:current',
          type: 'test',
          sourceRef: 'artifact:test:attempt-2',
          candidateRef: 'refs/winwincode/candidate/attempt-2',
          workRunId: currentRunId,
          sessionBindingId: 'bind:2',
          deliverySpecId: 'spec:1',
          deliverySpecRevision: 3,
          createdAt: '2026-09-02T08:12:00.000Z',
        },
      ],
      solutionReview: null,
      diagramExecution: null,
      currentCandidate: {
        candidateRef: 'refs/winwincode/candidate/attempt-2',
        candidateCommitId: '1111111111111111111111111111111111111111',
        candidateTreeId: '2222222222222222222222222222222222222222',
        diffSha256: `sha256:${'3'.repeat(64)}`,
        frozenAt: '2026-09-02T08:12:30.000Z',
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 3,
      },
      verdict: null,
      publication: null,
      readCursor: {},
    },
    solutionReview: null,
    diagramExecution: null,
    stage: { id: currentRunId, sessionBinding: { workRunId: currentRunId } },
    runtime: { workRunId: currentRunId, sessions: [] },
    evidence: [
      {
        id: 'evidence:failed',
        type: 'command',
        sourceRef: 'artifact:command:attempt-1',
        candidateRef: 'refs/winwincode/candidate/attempt-1',
        workRunId: failedRunId,
      },
      {
        id: 'evidence:current',
        type: 'test',
        sourceRef: 'artifact:test:attempt-2',
        candidateRef: 'refs/winwincode/candidate/attempt-2',
        workRunId: currentRunId,
      },
    ],
    verdict: null,
    attention: [],
    publication: null,
    currentCandidate: {
      candidateRef: 'refs/winwincode/candidate/attempt-2',
      candidateCommitId: '1111111111111111111111111111111111111111',
      candidateTreeId: '2222222222222222222222222222222222222222',
      diffSha256: `sha256:${'3'.repeat(64)}`,
      frozenAt: '2026-09-02T08:12:30.000Z',
    },
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-09-02T08:12:30.000Z',
      revisions: { delivery: 7, deliverySpec: 3, runtime: 9, publication: 0 },
      readCursor: {},
    },
  }
  projection.workRunAggregate = {
    items: projection.delivery.tasks.map((task, index) => ({
      schemaVersion: 'winwincode/v1', id: task.id, workContractId: 'wct_00000000000000000000000001',
      workContractRevision: 1, revision: 1, state: index === 0 ? 'in_progress' : 'ready',
      title: task.title, goal: task.goal, criterionIds: [], dependsOn: [],
    })),
    runs: projection.delivery.stages
      .filter(stage => stage.actorType !== 'human' && stage.sessionBinding?.workRunId !== null)
      .map((stage, index) => ({
        id: stage.id,
        workContractId: 'wct_00000000000000000000000001', contractRevision: 1,
        workItemId: stage.deliveryTaskId ?? `wit_0000000000000000000000000${index + 1}`, workItemRevision: 1,
        revision: stage.attempt, state: stage.status === 'failed' ? 'failed' : 'running',
        executionJobId: `job_0000000000000000000000000${index + 1}`, attempt: stage.attempt,
        workerId: 'wrk_00000000000000000000000001', workerInstanceId: 'wki_00000000000000000000000001',
        workerSessionId: 'wsn_00000000000000000000000001', leaseId: 'lse_00000000000000000000000001',
        codexThreadId: stage.sessionBinding?.codexThreadId ?? null,
        fencingToken: '1', productSessionId: stage.sessionBinding?.productSessionId ?? null,
      })),
  }
  return projection
}

test('history selection round-trips through presentation-only URL parameters', () => {
  const hash = `#/strongflow?delivery=${deliveryId}&session=psn_2&workRun=${currentRunId}`
  assert.deepEqual(strongFlowHistorySelectionFromHash(hash), {
    taskId: null,
    workRunId: null,
  })
  const deepLink = `${hash}&task=task%3A1&run=${failedRunId}`
  assert.deepEqual(strongFlowHistorySelectionFromHash(deepLink), {
    taskId: 'task:1',
    workRunId: failedRunId,
  })
  const updated = strongFlowHistoryHashWithSelection(hash, {
    taskId: 'task:1',
    workRunId: failedRunId,
  })
  assert.equal(updated, deepLink)
  assert.deepEqual(strongFlowHistorySelectionFromHash(updated), {
    taskId: 'task:1',
    workRunId: failedRunId,
  })
  const cleared = strongFlowHistoryHashWithSelection(deepLink, {
    taskId: null,
    workRunId: null,
  })
  assert.equal(cleared, hash)
})

test('history tree groups WorkRuns by Task, keeps Delivery stages, and flags the current run', () => {
  const tree = strongFlowHistoryTree(historyProjection(), limits)
  assert.equal(tree.tasks.length, 2)
  assert.equal(tree.tasks[0].task.id, 'task:1')
  assert.deepEqual(
    tree.tasks[0].runs.map(run => run.workRunId),
    [failedRunId, currentRunId],
  )
  assert.equal(tree.tasks[0].runs[0].attempt, 1)
  assert.equal(tree.tasks[0].runs[0].status, 'failed')
  assert.equal(tree.tasks[0].runs[0].isCurrent, false)
  assert.equal(tree.tasks[0].runs[0].workItemId, 'task:1')
  assert.equal(tree.tasks[0].runs[1].isCurrent, true)
  assert.deepEqual(
    tree.tasks[1].runs.map(run => run.workRunId),
    [],
  )
  assert.deepEqual(
    tree.deliveryRuns.map(run => run.workRunId),
    [planningRunId],
  )
  assert.equal(tree.deliveryRuns[0].workItemId, 'wit_00000000000000000000000003')
  assert.deepEqual(
    tree.runs.map(run => run.workRunId),
    [failedRunId, currentRunId, planningRunId],
  )
  assert.equal(tree.currentWorkRunId, currentRunId)

  const failedRun = tree.tasks[0].runs[0]
  assert.equal(failedRun.evidenceCount, 1)
  assert.deepEqual(failedRun.candidateRefs, ['refs/winwincode/candidate/attempt-1'])
  assert.equal(failedRun.binding.productSessionId, 'psn_00000000000000000000000001')
  assert.equal(failedRun.binding.executionJobId, 'job_00000000000000000000000001')
  assert.equal(failedRun.binding.workerSessionId, 'wsn_00000000000000000000000001')
  assert.equal(failedRun.binding.codexThreadId, 'cdx_00000000000000000000000001')
  assert.equal(failedRun.producedCurrentCandidate, false)
  assert.equal(tree.tasks[0].runs[1].producedCurrentCandidate, true)
})

test('history grouping and binding come from canonical WorkRun facts', () => {
  const projection = historyProjection()
  projection.delivery.stages[0].deliveryTaskId = 'task:2'
  projection.delivery.stages[0].sessionBinding = {
    ...projection.delivery.stages[0].sessionBinding,
    executionJobId: 'job_stage_must_not_win',
    workerId: 'wrk_stage_must_not_win',
    workerSessionId: 'wsn_stage_must_not_win',
    codexThreadId: 'cdx_stage_must_not_win',
  }
  const noStageRunId = 'wrn_00000000000000000000000005'
  projection.workRunAggregate.runs.push({
    ...projection.workRunAggregate.runs[0],
    id: noStageRunId,
    workItemId: 'task:2',
    executionJobId: 'job_00000000000000000000000005',
    workerId: 'wrk_00000000000000000000000005',
    workerInstanceId: 'wki_00000000000000000000000005',
    workerSessionId: 'wsn_00000000000000000000000005',
    leaseId: 'lse_00000000000000000000000005',
    codexThreadId: null,
    productSessionId: null,
  })
  const tree = strongFlowHistoryTree(projection, limits)
  assert.deepEqual(tree.tasks[0].runs.map(run => run.workRunId), [failedRunId, currentRunId])
  assert.deepEqual(tree.tasks[1].runs.map(run => run.workRunId), [noStageRunId])
  const run = tree.tasks[1].runs[0]
  assert.equal(run.stage, null)
  assert.equal(run.workItemId, 'task:2')
  assert.equal(run.binding?.productSessionId, null)
  assert.equal(run.binding?.executionJobId, 'job_00000000000000000000000005')
  assert.equal(run.binding?.workerSessionId, 'wsn_00000000000000000000000005')
  assert.equal(run.binding?.codexThreadId, null)

  const selectedWithoutStage = structuredClone(projection)
  selectedWithoutStage.stage = null
  selectedWithoutStage.runtime = {
    ...selectedWithoutStage.runtime,
    workRunId: noStageRunId,
  }
  const selectedTree = strongFlowHistoryTree(selectedWithoutStage, limits)
  assert.equal(selectedTree.currentWorkRunId, noStageRunId)
  assert.equal(
    selectedTree.tasks[1].runs.find(candidate => candidate.workRunId === noStageRunId)?.isCurrent,
    true,
  )
})

test('cancellation target selection is explicit across multiple active WorkRuns', () => {
  const projection = historyProjection()
  projection.workRunAggregate.runs = projection.workRunAggregate.runs.map((run, index) => ({
    ...run,
    state: index < 2 ? 'running' : index === 2 ? 'settled' : run.state,
  }))
  const active = strongFlowCancellableWorkRuns(projection)
  assert.deepEqual(active.map(run => run.id), [failedRunId, currentRunId])
  assert.equal(strongFlowCancellationTarget(projection, null), null)
  assert.equal(strongFlowCancellationTarget(projection, currentRunId)?.id, currentRunId)
  assert.equal(strongFlowCancellationTarget(projection, reviewRunId), null)
})

test('history tree keeps the exact deep-linked Task and WorkRun inside bounded navigation', () => {
  const projection = historyProjection()
  const deepRunId = 'wrn_00000000000000000000000005'
  const deepTaskId = 'task:deep-link-outside-first-page'
  const deepLinked = {
    ...projection,
    workRunAggregate: {
      ...projection.workRunAggregate,
      items: [...projection.workRunAggregate.items, {
        ...projection.workRunAggregate.items[1], id: deepTaskId,
      }],
      runs: [...projection.workRunAggregate.runs, {
        ...projection.workRunAggregate.runs[0], id: deepRunId, workItemId: deepTaskId,
      }],
    },
    delivery: {
      ...projection.delivery,
      tasks: [
        ...projection.delivery.tasks,
        {
          ...projection.delivery.tasks[1],
          id: deepTaskId,
          title: 'Deep-linked task',
          workRunIds: [deepRunId],
        },
      ],
      stages: [
        ...projection.delivery.stages,
        {
          ...projection.delivery.stages[0],
          id: deepRunId,
          deliveryTaskId: deepTaskId,
          sessionBinding: {
            ...projection.delivery.stages[0].sessionBinding,
            workRunId: deepRunId,
          },
        },
      ],
    },
  }
  const tree = strongFlowHistoryTree(
    deepLinked,
    { ...limits, tasks: 1, stages: 1 },
    { taskId: deepTaskId, workRunId: deepRunId },
  )

  assert.deepEqual(tree.tasks.map(node => node.task.id), [deepTaskId])
  assert.deepEqual(tree.tasks[0].runs.map(run => run.workRunId), [deepRunId])
  assert.deepEqual(tree.runs.map(run => run.workRunId), [deepRunId])
  assert.equal(tree.omittedTasks, 2)
  assert.equal(tree.omittedRuns, 2)
})

test('history selection keeps only canonical Task→WorkRun associations and drops stale identities', () => {
  const tree = strongFlowHistoryTree(historyProjection(), limits)
  assert.deepEqual(
    strongFlowHistorySelectionForTree(tree, {
      taskId: 'task:1',
      workRunId: failedRunId,
    }),
    { taskId: 'task:1', workRunId: failedRunId },
  )
  // A crossed task/run deep link cannot expand one Task while reviewing
  // another Task's run: the WorkRun's own WorkItem is the truth.
  assert.deepEqual(
    strongFlowHistorySelectionForTree(tree, {
      taskId: 'task:2',
      workRunId: failedRunId,
    }),
    { taskId: 'task:1', workRunId: failedRunId },
  )
  assert.deepEqual(
    strongFlowHistorySelectionForTree(tree, {
      taskId: 'task:missing',
      workRunId: failedRunId,
    }),
    { taskId: 'task:1', workRunId: failedRunId },
  )
  // A WorkRun keeps its own WorkItem association even when no Delivery task
  // row owns that item.
  assert.deepEqual(
    strongFlowHistorySelectionForTree(tree, {
      taskId: 'task:1',
      workRunId: planningRunId,
    }),
    { taskId: 'wit_00000000000000000000000003', workRunId: planningRunId },
  )
  assert.deepEqual(
    strongFlowHistorySelectionForTree(tree, {
      taskId: 'task:1',
      workRunId: 'wrn_00000000000000000000000009',
    }),
    { taskId: 'task:1', workRunId: null },
  )
})

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  parentNode = null
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  type = ''
  id = ''
  href = ''
  tabIndex = 0
  value = ''
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

  append(...children) {
    for (const child of children) this.insertBefore(child, null)
  }

  replaceChildren(...children) {
    for (const child of [...this.children]) child.remove()
    for (const child of children) this.insertBefore(child, null)
  }

  insertBefore(child, reference) {
    child.remove?.()
    const index = reference === null
      ? this.children.length
      : this.children.indexOf(reference)
    this.children.splice(index < 0 ? this.children.length : index, 0, child)
    child.parentNode = this
    return child
  }

  remove() {
    if (this.parentNode === null) return
    const index = this.parentNode.children.indexOf(this)
    if (index >= 0) this.parentNode.children.splice(index, 1)
    this.parentNode = null
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value))
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    const listeners = (this.listeners.get(name) ?? []).filter(
      candidate => candidate !== listener,
    )
    if (listeners.length === 0) this.listeners.delete(name)
    else this.listeners.set(name, listeners)
  }

  emit(name, values = {}) {
    for (const listener of this.listeners.get(name) ?? []) listener(values)
  }

  focus() {
    this.ownerDocument.activeElement = this
  }
}

class FakeDocument {
  activeElement = null

  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

function findByClass(node, className) {
  if (node.className === className) return node
  for (const child of node.children) {
    const match = findByClass(child, className)
    if (match !== null) return match
  }
  return null
}

function findAllByClass(node, className, matches = []) {
  if (node.className === className) matches.push(node)
  for (const child of node.children) findAllByClass(child, className, matches)
  return matches
}

function flatten(node) {
  return [node, ...node.children.flatMap(child => flatten(child))]
}

function allText(node) {
  return flatten(node).map(entry => entry.textContent ?? '').join(' ')
}

function mountNavigation(document, overrides = {}) {
  const tasksParent = document.createElement('ul')
  const stagesParent = document.createElement('ol')
  const tasksOmitted = document.createElement('p')
  const stagesOmitted = document.createElement('p')
  const tasksEmpty = document.createElement('p')
  const stagesEmpty = document.createElement('p')
  const selections = []
  const view = mountStrongFlowHistoryNavigation({
    document,
    tasksParent,
    stagesParent,
    tasksOmitted,
    stagesOmitted,
    tasksEmpty,
    stagesEmpty,
    onSelect: selection => { selections.push(selection) },
    ...overrides,
  })
  return {
    view,
    tasksParent,
    stagesParent,
    tasksOmitted,
    stagesOmitted,
    tasksEmpty,
    stagesEmpty,
    selections,
  }
}

test('history navigation marks queued, leased, and running WorkRuns cancellable', () => {
  const document = new FakeDocument()
  const tasksParent = document.createElement('ul')
  const stagesParent = document.createElement('ol')
  const empty = document.createElement('p')
  const view = mountStrongFlowHistoryNavigation({
    document,
    tasksParent,
    stagesParent,
    tasksOmitted: document.createElement('p'),
    stagesOmitted: document.createElement('p'),
    tasksEmpty: empty,
    stagesEmpty: empty,
    onSelect() {},
    onOpenEvidence() {},
  })
  const projection = historyProjection()
  projection.workRunAggregate.runs = projection.workRunAggregate.runs.map((run, index) => ({
    ...run,
    state: index === 0 ? 'leased' : index === 1 ? 'running' : index === 2 ? 'queued' : 'settled',
  }))
  const tree = strongFlowHistoryTree(projection, limits)
  view.update(tree)
  const rows = [...stagesParent.children]
  assert.deepEqual(rows.map(row => row.dataset.cancellable), ['true', 'true', 'true'])
  assert.deepEqual(rows.map(row => row.children[0].dataset.workRunState), ['leased', 'running', 'queued'])
  view.close()
})

test('task rows expand into clickable historical attempts while the current run stays highlighted', () => {
  const document = new FakeDocument()
  const { view, tasksParent, stagesParent, selections } = mountNavigation(document)
  view.update(strongFlowHistoryTree(historyProjection(), limits))

  const taskRows = tasksParent.children
  assert.equal(taskRows.length, 2)
  assert.equal(taskRows[0].dataset.status, 'in_progress')
  assert.equal(taskRows[1].dataset.status, 'ready')
  const toggle = findByClass(taskRows[0], 'wwc-strongflow-history-toggle')
  assert.equal(toggle.tagName, 'BUTTON')
  assert.equal(toggle.getAttribute('aria-expanded'), 'false')
  assert.match(toggle.textContent, /Implement the feature/u)

  toggle.emit('click')
  assert.equal(toggle.getAttribute('aria-expanded'), 'true')
  const runList = findByClass(taskRows[0], 'wwc-strongflow-run-list')
  assert.notEqual(runList, null)
  assert.equal(runList.hidden, false)
  const runButtons = findAllByClass(taskRows[0], 'wwc-strongflow-run-button')
  assert.equal(runButtons.length, 1)
  assert.equal(runButtons[0].dataset.workRunId, failedRunId)
  assert.equal(runButtons[0].dataset.attempt, '1')
  assert.match(runButtons[0].textContent, /attempt 1/u)
  assert.match(runButtons[0].textContent, /failed/u)
  assert.equal(runButtons[0].getAttribute('aria-pressed'), 'false')

  runButtons[0].emit('click')
  assert.deepEqual(selections.at(-1), { taskId: 'task:1', workRunId: failedRunId })
  assert.equal(runButtons[0].getAttribute('aria-pressed'), 'true')
  assert.equal(view.selection().workRunId, failedRunId)

  const currentMarker = findByClass(taskRows[0], 'wwc-strongflow-current-run')
  assert.notEqual(currentMarker, null)
  assert.equal(currentMarker.getAttribute('aria-current'), 'true')
  assert.equal(currentMarker.dataset.workRunId, currentRunId)
  assert.equal(
    findAllByClass(taskRows[0], 'wwc-strongflow-run-button')
      .some(button => button.dataset.workRunId === currentRunId),
    false,
  )

  const timelineButtons = findAllByClass(stagesParent, 'wwc-strongflow-run-button')
  assert.deepEqual(
    timelineButtons.map(button => button.dataset.workRunId),
    [failedRunId, planningRunId],
  )
  assert.equal(stagesParent.children.length, 3)
  assert.equal(stagesParent.children[0].dataset.status, 'failed')
  const timelineCurrent = findByClass(stagesParent, 'wwc-strongflow-current-run')
  assert.equal(timelineCurrent.dataset.workRunId, currentRunId)

  toggle.emit('click')
  assert.equal(toggle.getAttribute('aria-expanded'), 'false')
  assert.equal(findByClass(taskRows[0], 'wwc-strongflow-run-list').hidden, true)
  view.close()
})

test('keyboard roving focus moves through Task toggles and attempts without a mouse', () => {
  const document = new FakeDocument()
  const { view, tasksParent, stagesParent } = mountNavigation(document)
  view.update(strongFlowHistoryTree(historyProjection(), limits))

  const firstToggle = findByClass(tasksParent.children[0], 'wwc-strongflow-history-toggle')
  const secondToggle = findByClass(tasksParent.children[1], 'wwc-strongflow-history-toggle')
  assert.equal(firstToggle.tabIndex, 0)
  assert.equal(secondToggle.tabIndex, -1)

  firstToggle.focus()
  firstToggle.emit('keydown', { key: 'ArrowDown', preventDefault() {} })
  assert.equal(document.activeElement, secondToggle)
  secondToggle.emit('keydown', { key: 'ArrowUp', preventDefault() {} })
  assert.equal(document.activeElement, firstToggle)

  firstToggle.emit('keydown', { key: 'ArrowRight', preventDefault() {} })
  assert.equal(firstToggle.getAttribute('aria-expanded'), 'true')
  const runButtons = findAllByClass(tasksParent.children[0], 'wwc-strongflow-run-button')
  assert.equal(document.activeElement, runButtons[0])
  runButtons[0].emit('keydown', { key: 'ArrowDown', preventDefault() {} })
  const nestedCurrent = findByClass(tasksParent.children[0], 'wwc-strongflow-current-run')
  assert.equal(document.activeElement, nestedCurrent)
  runButtons[0].emit('keydown', { key: 'ArrowLeft', preventDefault() {} })
  assert.equal(document.activeElement, firstToggle)
  assert.equal(firstToggle.getAttribute('aria-expanded'), 'false')
  firstToggle.emit('keydown', { key: 'Home', preventDefault() {} })
  assert.equal(document.activeElement, firstToggle)
  firstToggle.emit('keydown', { key: 'End', preventDefault() {} })
  const lastTimelineButton = findAllByClass(stagesParent, 'wwc-strongflow-run-button').at(-1)
  assert.equal(document.activeElement, lastTimelineButton)
  view.close()
})

const historicalRuntimeSnapshot = workRunId => ({
  kind: 'runtime_projection',
  productSessionId: 'psn_00000000000000000000000001',
  deliveryId,
  workRunId,
  revision: 11,
  lastProjectionSequence: 27,
  rebuiltAt: '2026-09-02T08:09:30.000Z',
  readCursor: {},
  eventCursor: {},
  sessions: [{
    productSessionId: 'psn_00000000000000000000000001',
    workRunId,
    sessionBindingId: 'bind:1',
    executionJobId: 'job_00000000000000000000000001',
    workerSessionId: 'wsn_00000000000000000000000001',
    codexThreadId: 'cdx_00000000000000000000000001',
    fencingToken: 'fence:1',
    leaseId: 'lease:1',
    attempt: 1,
    deliveryTaskId: 'task:1',
    asOfSequence: 27,
    diffSummary: null,
    plan: null,
    usage: null,
    recovery: {
      failureCount: 0,
      lastFailureSourceRef: null,
      latestRecoverySourceRef: null,
      recoveryCount: 0,
      state: 'none',
    },
    agents: [],
    agentEdges: [],
    activities: [{
      activityType: 'shell_command',
      callId: 'call:1',
      command: 'cargo test',
      outcome: 'succeeded',
      exitCode: 0,
      sourceRef: 'artifact:runtime:1',
      status: 'succeeded',
    }],
  }],
})

const historicalCandidateItem = producerWorkRunId => ({
  availability: 'available',
  candidate: {
    candidateRef: 'refs/winwincode/candidate/attempt-1',
    candidateCommitId: '4444444444444444444444444444444444444444',
    candidateTreeId: '5555555555555555555555555555555555555555',
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 3,
    diffSha256: `sha256:${'4'.repeat(64)}`,
    frozenAt: '2026-09-02T08:09:30.000Z',
    producerSessionBindingId: 'bind:1',
    producerWorkRunId,
  },
  firstSeenDeliveryRevision: 6,
  isCurrentAtReadCursor: false,
  lastSeenDeliveryRevision: 7,
  reviewDeliveryRevision: null,
})

const historicalCandidateReview = identity => ({
  availability: 'available',
  candidate: historicalCandidateItem(failedRunId).candidate,
  currentAuthorization: false,
  displayOnly: true,
  evidence: [{
    id: 'evidence:failed',
    type: 'command',
    sourceRef: 'artifact:command:attempt-1',
    candidateRef: identity.candidateRef,
    workRunId: failedRunId,
    sessionBindingId: 'bind:1',
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 3,
    createdAt: '2026-09-02T08:09:00.000Z',
  }],
  firstSeenDeliveryRevision: 6,
  kind: 'candidate_historical_review',
  lastSeenDeliveryRevision: 7,
  readCursor: {},
  reviewDeliveryRevision: null,
  verdict: null,
})

function detailLoaders(records = { runtime: [], candidates: [], review: [] }) {
  return {
    loadRuntime: async workRunId => {
      records.runtime.push(workRunId)
      return historicalRuntimeSnapshot(workRunId)
    },
    loadCandidates: async workRunId => {
      records.candidates.push(workRunId)
      return workRunId === failedRunId ? [historicalCandidateItem(workRunId)] : []
    },
    loadCandidateReview: async identity => {
      records.review.push(identity)
      return historicalCandidateReview(identity)
    },
  }
}

function nextTick() {
  return new Promise(resolve => { setImmediate(resolve) })
}

test('the read-only historical run detail shows exact identity, binding, runtime, evidence, candidates and conclusion', async () => {
  const document = new FakeDocument()
  const records = { runtime: [], candidates: [], review: [] }
  const view = mountStrongFlowRunDetail({
    document,
    limits,
    loaders: detailLoaders(records),
  })
  const parent = document.createElement('section')
  parent.append(view.root)
  const tree = strongFlowHistoryTree(historyProjection(), limits)

  view.update({ tree, selection: { taskId: null, workRunId: null } })
  assert.equal(view.root.hidden, true)

  view.update({ tree, selection: { taskId: 'task:1', workRunId: currentRunId } })
  assert.equal(view.root.hidden, true)

  view.update({ tree, selection: { taskId: 'task:1', workRunId: failedRunId } })
  assert.equal(view.root.hidden, false)
  assert.deepEqual(records.runtime, [failedRunId])
  assert.deepEqual(records.candidates, [failedRunId])
  assert.match(findByClass(view.root, 'wwc-strongflow-history-note').textContent, /read-only/iu)
  const identity = allText(findByClass(view.root, 'wwc-strongflow-history-identity'))
  assert.match(identity, new RegExp(failedRunId, 'u'))
  assert.match(identity, /Attempt 1/u)
  assert.match(identity, /Status failed/u)
  assert.match(identity, /Implement the feature/u)
  const binding = allText(findByClass(view.root, 'wwc-strongflow-history-binding'))
  assert.match(binding, /psn_00000000000000000000000001/u)
  assert.match(binding, /job_00000000000000000000000001/u)
  assert.match(binding, /wsn_00000000000000000000000001/u)
  assert.match(binding, /cdx_00000000000000000000000001/u)
  const evidence = findByClass(view.root, 'wwc-strongflow-history-evidence')
  assert.equal(evidence.children.length, 1)
  assert.match(evidence.children[0].textContent, /evidence:failed/u)
  const conclusion = findByClass(view.root, 'wwc-strongflow-history-conclusion')
  assert.equal(conclusion.dataset.status, 'failed')
  assert.match(allText(conclusion), /failed/u)
  assert.match(allText(conclusion), /not finished/u)

  await nextTick()
  const runtime = allText(findByClass(view.root, 'wwc-strongflow-history-runtime'))
  assert.match(runtime, /11/u)
  assert.match(
    allText(findByClass(view.root, 'wwc-strongflow-history-runtime-sessions')),
    /cdx_00000000000000000000000001/u,
  )
  const activities = findByClass(view.root, 'wwc-strongflow-activity-rows')
  assert.equal(activities.children.length, 1)
  assert.match(allText(activities.children[0]), /cargo test/u)

  const candidates = findByClass(view.root, 'wwc-strongflow-history-candidates')
  const candidateButton = findByClass(candidates, 'wwc-strongflow-history-candidate')
  assert.match(candidateButton.textContent, /refs\/winwincode\/candidate\/attempt-1/u)
  assert.equal(candidates.children[0].dataset.current, undefined)
  candidateButton.emit('click')
  await nextTick()
  assert.deepEqual(records.review, [{
    candidateRef: 'refs/winwincode/candidate/attempt-1',
    candidateTreeId: '5555555555555555555555555555555555555555',
    diffSha256: `sha256:${'4'.repeat(64)}`,
  }])
  const review = allText(findByClass(view.root, 'wwc-strongflow-history-review'))
  assert.match(review, /4444444444444444444444444444444444444444/u)
  assert.match(review, /r6/u)
  assert.match(
    allText(findByClass(view.root, 'wwc-strongflow-history-review-note')),
    /never authorizes/u,
  )
  const reviewEvidence = findByClass(view.root, 'wwc-strongflow-history-review-evidence')
  assert.equal(reviewEvidence.children.length, 1)

  view.close()
})

test('historical Candidate review keeps the latest selection when responses finish out of order', async () => {
  const document = new FakeDocument()
  const first = historicalCandidateItem(failedRunId)
  const second = {
    ...first,
    candidate: {
      ...first.candidate,
      candidateRef: 'refs/winwincode/candidate/attempt-2',
      candidateCommitId: '6666666666666666666666666666666666666666',
      candidateTreeId: '7777777777777777777777777777777777777777',
      diffSha256: `sha256:${'8'.repeat(64)}`,
    },
  }
  let resolveFirst
  let resolveSecond
  const firstReview = new Promise(resolve => {
    resolveFirst = resolve
  })
  const secondReview = new Promise(resolve => {
    resolveSecond = resolve
  })
  const reviewSignals = new Map()
  const reviewFor = item => ({
    ...historicalCandidateReview(item.candidate),
    candidate: item.candidate,
  })
  const view = mountStrongFlowRunDetail({
    document,
    limits,
    loaders: {
      loadRuntime: async workRunId => historicalRuntimeSnapshot(workRunId),
      loadCandidates: async () => [first, second],
      loadCandidateReview: (identity, signal) => {
        reviewSignals.set(identity.candidateRef, signal)
        return identity.candidateRef === first.candidate.candidateRef ? firstReview : secondReview
      },
    },
  })
  document.createElement('section').append(view.root)
  view.update({
    tree: strongFlowHistoryTree(historyProjection(), limits),
    selection: { taskId: 'task:1', workRunId: failedRunId },
  })
  await nextTick()

  const buttons = findAllByClass(view.root, 'wwc-strongflow-history-candidate')
  buttons[0].emit('click')
  buttons[1].emit('click')
  assert.equal(
    reviewSignals.get(first.candidate.candidateRef)?.aborted,
    true,
    'selecting Candidate B aborts the obsolete Candidate A request',
  )
  resolveSecond(reviewFor(second))
  await nextTick()
  const review = findByClass(view.root, 'wwc-strongflow-history-review')
  assert.match(allText(review), new RegExp(second.candidate.candidateCommitId, 'u'))

  resolveFirst(reviewFor(first))
  await nextTick()
  assert.match(
    allText(review),
    new RegExp(second.candidate.candidateCommitId, 'u'),
    'a stale response must not replace the latest Candidate selection',
  )
  view.close()
})

function emptyCandidateFilesState() {
  return {
    status: 'idle',
    items: [],
    hasMore: false,
    previewLimited: false,
    selectedPath: null,
    diff: {
      status: 'idle',
      path: null,
      content: '',
      loadedBytes: 0,
      totalBytes: null,
      hasMore: false,
      previewLimited: false,
      fileDiffSha256: null,
      unavailableReason: null,
      error: null,
    },
    error: null,
  }
}

class FakeStrongFlowViewModel {
  constructor(initialState) {
    this.state = initialState
  }

  calls = []
  draftScope = '["strongflow-history-test-actor","strongflow-history-test-scope"]'
  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }

  async start() { this.calls.push(['start']) }
  async refresh() { this.calls.push(['refresh']) }
  async loadCandidateFiles() { this.calls.push(['loadCandidateFiles']) }
  async loadMoreCandidateFiles() { this.calls.push(['loadMoreCandidateFiles']) }
  async selectCandidateFile(path) { this.calls.push(['selectCandidateFile', path]) }
  async loadMoreCandidateDiff() { this.calls.push(['loadMoreCandidateDiff']) }
  async decideSolutionReview(input) { this.calls.push(['decideSolutionReview', input]) }
  async approveTaskBreakdown() { this.calls.push(['approveTaskBreakdown']) }
  async resolveAttention(input) { this.calls.push(['resolveAttention', input]) }
  async submitVerdict() { this.calls.push(['submitVerdict']) }
  async advanceDelivery() { this.calls.push(['advanceDelivery']) }
  async cancelWorkRun(request) { this.calls.push(['cancelWorkRun', request]) }
  async loadWorkRunRuntime(workRunId, signal) {
    this.calls.push(['loadWorkRunRuntime', workRunId, signal])
    return null
  }
  async loadWorkRunCandidates(workRunId, signal) {
    this.calls.push(['loadWorkRunCandidates', workRunId, signal])
    return []
  }
  async loadCandidateHistoricalReview(candidate, signal) {
    this.calls.push(['loadCandidateHistoricalReview', candidate, signal])
    return null
  }
  cancelPending() { this.calls.push(['cancelPending']) }
  reconnect() { this.calls.push(['reconnect']) }
  close() { this.calls.push(['close']) }
}

class FakeHistoryLocation {
  #hash

  replacements = []

  constructor(hash) {
    this.#hash = hash
  }

  hash() {
    return this.#hash
  }

  replaceHash(next) {
    this.#hash = next
    this.replacements.push(next)
  }
}

test('the page restores history selection from a deep link and keeps the URL authoritative', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const initialHash = `#/strongflow?delivery=${deliveryId}&session=psn_2&workRun=${currentRunId}`
  const location = new FakeHistoryLocation(`${initialHash}&task=task%3A1&run=${failedRunId}`)
  const model = new FakeStrongFlowViewModel({
    status: 'ready',
    realtime: 'subscribed',
    projection: historyProjection(),
    candidateFiles: emptyCandidateFilesState(),
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([{
      schemaVersion: 'winwincode/v1',
      deliveryId,
      revision: 7,
      status: 'executing',
      title: 'History navigation Delivery',
      updatedAt: '2026-09-02T08:12:30.000Z',
    }]),
    limits,
    historyLocation: location,
  })

  const taskRow = findByClass(rootElement, 'wwc-strongflow-task-list').children[0]
  assert.equal(taskRow.dataset.status, 'in_progress')
  const toggle = findByClass(taskRow, 'wwc-strongflow-history-toggle')
  assert.equal(toggle.getAttribute('aria-expanded'), 'true')
  const runButtons = findAllByClass(taskRow, 'wwc-strongflow-run-button')
  assert.equal(runButtons[0].getAttribute('aria-pressed'), 'true')
  const detail = findByClass(rootElement, 'wwc-strongflow-history')
  assert.equal(detail.hidden, false)
  assert.match(allText(detail), new RegExp(failedRunId, 'u'))

  const planningButton = findAllByClass(
    rootElement,
    'wwc-strongflow-run-button',
  ).find(button => button.dataset.workRunId === planningRunId)
  planningButton.emit('click')
  assert.deepEqual(location.replacements.at(-1), `${initialHash}&task=wit_00000000000000000000000003&run=${planningRunId}`)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-history').hidden, false)
  assert.match(
    allText(findByClass(rootElement, 'wwc-strongflow-history')),
    new RegExp(planningRunId, 'u'),
  )
  assert.equal(location.replacements.length, 1, 'the initial deep link does not rewrite the URL')

  const currentMarker = findByClass(rootElement, 'wwc-strongflow-current-run')
  currentMarker.emit('click')
  assert.equal(
    findByClass(rootElement, 'wwc-strongflow-history').hidden,
    true,
    'the current run never opens as history',
  )
  assert.deepEqual(
    location.replacements.at(-1),
    initialHash,
    'leaving history clears the presentation parameters',
  )

  mounted.close()
})

test('historical attempt review blocks current Delivery mutation controls', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const current = historyProjection()
  current.delivery.status = 'ready-to-deliver'
  const model = new FakeStrongFlowViewModel({
    status: 'ready',
    realtime: 'subscribed',
    projection: current,
    candidateFiles: emptyCandidateFilesState(),
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    limits,
    historyLocation: new FakeHistoryLocation(
      `#/strongflow?delivery=${deliveryId}&task=task%3A1&run=${failedRunId}`,
    ),
  })

  const advance = findByClass(rootElement, 'wwc-strongflow-advance-delivery')
  assert.equal(advance.disabled, true)
  advance.emit('click')
  assert.equal(model.calls.some(([name]) => name === 'advanceDelivery'), false)

  mounted.close()
})

test('historical review disables every current Delivery mutation control, double-guards handlers, and restores on return', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const current = historyProjection()
  current.delivery.status = 'ready-to-deliver'
  current.workRunAggregate.runs.forEach(run => { run.state = 'settled' })
  current.currentCandidate.producerWorkRunId = currentRunId
  const review = {
    deliveryId,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 3,
    planningWorkRunId: planningRunId,
    planningSessionBindingId: 'bind:3',
    reviewWorkRunId: reviewRunId,
    attentionItemId: 'att_00000000000000000000000001',
    reviewSetSha256: `sha256:${'1'.repeat(64)}`,
    reviewStatus: 'pending',
    decision: null,
    comments: null,
    requestedChanges: null,
    reviewerId: null,
    reviewedAt: null,
    solutionId: 'solution:1',
    summary: 'Review the historic plan.',
    approach: [],
    components: [],
    connections: [],
    architectureDiagram: {
      id: 'diagram:architecture',
      kind: 'system-architecture',
      title: 'Architecture',
      nodes: [{
        id: 'node:1',
        label: 'Worker',
        description: '',
        kind: 'component',
        trustBoundary: null,
        unresolved: false,
      }],
      edges: [],
    },
    processDiagram: { id: 'diagram:process', kind: 'process-flow', title: 'Process', nodes: [], edges: [] },
    risks: [],
    unresolvedItems: [],
    taskProposals: [],
  }
  current.delivery.solutionReview = review
  current.solutionReview = review
  const attentionRecord = {
    id: 'att_00000000000000000000000001',
    deliverySpecId: 'spec:1',
    workRunId: failedRunId,
    type: 'delivery_approval',
    title: 'Approve delivery',
    options: [],
    blocking: true,
    status: 'open',
    assignedTo: null,
    createdAt: '2026-09-02T08:00:00.000Z',
    resolvedAt: null,
    resolvedBy: null,
    resolutionSummary: null,
  }
  current.delivery.attention = [attentionRecord]
  current.attention = [attentionRecord]
  const model = new FakeStrongFlowViewModel({
    status: 'ready',
    realtime: 'subscribed',
    projection: current,
    candidateFiles: emptyCandidateFilesState(),
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    limits,
    historyLocation: new FakeHistoryLocation(
      `#/strongflow?delivery=${deliveryId}&task=task%3A1&run=${failedRunId}`,
    ),
  })

  const blocked = [
    'wwc-strongflow-advance-delivery',
    'wwc-strongflow-approve-solution',
    'wwc-strongflow-request-changes',
    'wwc-strongflow-reject-solution',
    'wwc-strongflow-submit-verdict',
    'wwc-strongflow-resolve-attention',
    'wwc-strongflow-dismiss-attention',
  ]
  for (const className of blocked) {
    assert.equal(
      findByClass(rootElement, className).disabled,
      true,
      `${className} must be disabled while a historical run is open`,
    )
  }
  for (const className of ['wwc-strongflow-approve-solution', 'wwc-strongflow-request-changes', 'wwc-strongflow-reject-solution', 'wwc-strongflow-submit-verdict', 'wwc-strongflow-advance-delivery', 'wwc-strongflow-resolve-attention', 'wwc-strongflow-dismiss-attention']) {
    findByClass(rootElement, className).emit('click')
  }
  const mutationNames = [
    'decideSolutionReview',
    'approveTaskBreakdown',
    'resolveAttention',
    'submitVerdict',
    'advanceDelivery',
  ]
  assert.equal(
    model.calls.some(([name]) => mutationNames.includes(name)),
    false,
    'no current Delivery command may be issued from historical review',
  )

  const currentMarker = findByClass(rootElement, 'wwc-strongflow-current-run')
  currentMarker.emit('click')
  assert.equal(
    findByClass(rootElement, 'wwc-strongflow-advance-delivery').disabled,
    false,
    'returning to the current run restores mutation controls',
  )
  assert.equal(findByClass(rootElement, 'wwc-strongflow-approve-solution').disabled, false)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-resolve-attention').disabled, false)
  findByClass(rootElement, 'wwc-strongflow-advance-delivery').emit('click')
  assert.equal(model.calls.some(([name]) => name === 'advanceDelivery'), true)

  mounted.close()
})

test('equivalent snapshots preserve historical detail DOM identity and focus', async () => {
  const document = new FakeDocument()
  const view = mountStrongFlowRunDetail({
    document,
    limits,
    loaders: detailLoaders(),
  })
  const parent = document.createElement('section')
  parent.append(view.root)
  const selection = { taskId: 'task:1', workRunId: failedRunId }
  view.update({ tree: strongFlowHistoryTree(historyProjection(), limits), selection })
  await nextTick()

  const evidence = findByClass(view.root, 'wwc-strongflow-history-evidence')
  const identity = findByClass(view.root, 'wwc-strongflow-history-identity')
  const activities = findByClass(view.root, 'wwc-strongflow-activity-rows')
  const evidenceBefore = [...evidence.children]
  const identityBefore = [...identity.children]
  const activityBefore = [...activities.children]
  activityBefore[0].focus()

  // A new snapshot object with equivalent content must not rebuild any detail row.
  view.update({ tree: strongFlowHistoryTree(historyProjection(), limits), selection })
  assert.deepEqual([...evidence.children], evidenceBefore)
  assert.deepEqual([...identity.children], identityBefore)
  assert.deepEqual([...activities.children], activityBefore)
  assert.equal(document.activeElement, activityBefore[0])

  // Real content changes update the same keyed nodes instead of replacing them.
  const changed = historyProjection()
  changed.evidence.push({
    id: 'evidence:failed:extra',
    type: 'test',
    sourceRef: 'artifact:test:attempt-1b',
    candidateRef: 'refs/winwincode/candidate/attempt-1',
    workRunId: failedRunId,
  })
  view.update({ tree: strongFlowHistoryTree(changed, limits), selection })
  assert.equal(evidence.children.length, 2)
  assert.equal(evidence.children[0], evidenceBefore[0], 'existing rows keep their node identity')

  view.close()
})

test('timeline ArrowLeft stays inside the timeline instead of collapsing the Task tree', () => {
  const document = new FakeDocument()
  const { view, tasksParent, stagesParent } = mountNavigation(document)
  view.update(strongFlowHistoryTree(historyProjection(), limits))
  const toggle = findByClass(tasksParent.children[0], 'wwc-strongflow-history-toggle')
  toggle.emit('click')
  assert.equal(toggle.getAttribute('aria-expanded'), 'true')

  const timelineButtons = findAllByClass(stagesParent, 'wwc-strongflow-run-button')
  const taskOwned = timelineButtons.find(button => button.dataset.workRunId === failedRunId)
  taskOwned.focus()
  taskOwned.emit('keydown', { key: 'ArrowLeft', preventDefault() {} })
  assert.equal(toggle.getAttribute('aria-expanded'), 'true', 'the Task tree stays expanded')
  assert.equal(document.activeElement, taskOwned, 'focus stays on the timeline row')
  view.close()
})

test('a crossed task/run deep link stays unchanged and fails closed with an explicit alert', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const initialHash = `#/strongflow?delivery=${deliveryId}&session=psn_2&workRun=${currentRunId}`
  const location = new FakeHistoryLocation(
    `${initialHash}&task=task%3A2&run=${failedRunId}`,
  )
  const model = new FakeStrongFlowViewModel({
    status: 'ready',
    realtime: 'subscribed',
    projection: historyProjection(),
    candidateFiles: emptyCandidateFilesState(),
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    limits,
    historyLocation: location,
  })

  const taskRows = findByClass(rootElement, 'wwc-strongflow-task-list').children
  assert.equal(
    findByClass(taskRows[0], 'wwc-strongflow-history-toggle').getAttribute('aria-expanded'),
    'false',
    'the unrelated canonical Task is not silently opened',
  )
  assert.equal(
    findByClass(taskRows[1], 'wwc-strongflow-history-toggle').getAttribute('aria-expanded'),
    'true',
  )
  const detail = findByClass(rootElement, 'wwc-strongflow-history')
  assert.equal(detail.hidden, true)
  const routeError = findByClass(rootElement, 'wwc-strongflow-history-route-error')
  assert.equal(routeError.getAttribute('role'), 'alert')
  assert.equal(routeError.hidden, false)
  assert.match(allText(routeError), /Task or Attempt/u)
  assert.deepEqual(location.replacements, [], 'the crossed deep link is never rewritten')
  assert.deepEqual(
    model.calls.filter(([name]) => name === 'loadWorkRunRuntime').map(([, id]) => id),
    [],
  )

  mounted.close()
})

test('an unchanged historical selection reloads exact payloads when the snapshot cut advances', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const initial = historyProjection()
  initial.delivery.readCursor = { token: 'read-cut-1' }
  initial.metadata.readCursor = initial.delivery.readCursor
  const model = new FakeStrongFlowViewModel({
    status: 'ready',
    realtime: 'subscribed',
    projection: initial,
    candidateFiles: emptyCandidateFilesState(),
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    limits,
    historyLocation: new FakeHistoryLocation(
      `#/strongflow?delivery=${deliveryId}&task=task%3A1&run=${failedRunId}`,
    ),
  })
  assert.equal(
    model.calls.filter(([name]) => name === 'loadWorkRunRuntime').length,
    1,
  )
  assert.equal(
    model.calls.filter(([name]) => name === 'loadWorkRunCandidates').length,
    1,
  )
  const initialRuntimeCall = model.calls.find(([name]) => name === 'loadWorkRunRuntime')
  const initialCandidateCall = model.calls.find(([name]) => name === 'loadWorkRunCandidates')

  const advanced = structuredClone(initial)
  advanced.delivery.readCursor = { token: 'read-cut-2' }
  advanced.metadata.readCursor = advanced.delivery.readCursor
  model.publish({ ...model.state, projection: advanced })

  assert.equal(initialRuntimeCall?.[2]?.aborted, true)
  assert.equal(initialCandidateCall?.[2]?.aborted, true)

  assert.equal(
    model.calls.filter(([name]) => name === 'loadWorkRunRuntime').length,
    2,
    'the exact RuntimeProjection must follow the selected snapshot cut',
  )
  assert.equal(
    model.calls.filter(([name]) => name === 'loadWorkRunCandidates').length,
    2,
    'historical Candidate availability must follow the selected snapshot cut',
  )
  mounted.close()
})

test('the page shows empty history notes instead of dead lists', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const projection = historyProjection()
  projection.workRunAggregate = { items: [], runs: [] }
  projection.delivery.tasks = []
  projection.delivery.stages = [projection.delivery.stages[2]]
  const model = new FakeStrongFlowViewModel({
    status: 'ready',
    realtime: 'subscribed',
    projection,
    candidateFiles: emptyCandidateFilesState(),
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    limits,
  })
  const taskList = findByClass(rootElement, 'wwc-strongflow-task-list')
  assert.equal(taskList.children.length, 0)
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-tasks-empty').textContent,
    /no .*task/iu,
  )
  mounted.close()
})
