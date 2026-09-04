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

const history = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-history.js',
)).href}`)
const page = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-page.js',
)).href}`)

const {
  strongFlowHistorySelectionFromHash,
  strongFlowHistoryHashWithSelection,
  strongFlowHistoryTree,
  strongFlowHistorySelectionForTree,
  mountStrongFlowHistoryNavigation,
  mountStrongFlowRunDetail,
} = history
const { mountStrongFlowPage } = page

const deliveryId = 'dlv_00000000000000000000000001'
const currentRunId = 'run_00000000000000000000000002'
const failedRunId = 'run_00000000000000000000000001'
const planningRunId = 'run_00000000000000000000000003'
const reviewRunId = 'run_00000000000000000000000004'

const limits = {
  deliveries: 50,
  tasks: 100,
  stages: 50,
  attention: 50,
  evidence: 100,
  runtimeSessions: 50,
  graphNodes: 100,
  graphEdges: 200,
  activities: 100,
}

function historyProjection() {
  return {
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
        goal: 'Review every historical StageRun attempt.',
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
          stageRunIds: [failedRunId, currentRunId],
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
          stageRunIds: [],
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
            stageRunId: failedRunId,
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
            stageRunId: currentRunId,
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
            stageRunId: planningRunId,
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
          stageRunId: failedRunId,
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
          stageRunId: currentRunId,
          sessionBindingId: 'bind:2',
          deliverySpecId: 'spec:1',
          deliverySpecRevision: 3,
          createdAt: '2026-09-02T08:12:00.000Z',
        },
      ],
      solutionReview: null,
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
    stage: { id: currentRunId },
    runtime: { stageRunId: currentRunId, sessions: [] },
    evidence: [
      {
        id: 'evidence:failed',
        type: 'command',
        sourceRef: 'artifact:command:attempt-1',
        candidateRef: 'refs/winwincode/candidate/attempt-1',
        stageRunId: failedRunId,
      },
      {
        id: 'evidence:current',
        type: 'test',
        sourceRef: 'artifact:test:attempt-2',
        candidateRef: 'refs/winwincode/candidate/attempt-2',
        stageRunId: currentRunId,
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
}

test('history selection round-trips through presentation-only URL parameters', () => {
  const hash = `#/strongflow?delivery=${deliveryId}&session=psn_2&stageRun=${currentRunId}`
  assert.deepEqual(strongFlowHistorySelectionFromHash(hash), {
    taskId: null,
    stageRunId: null,
  })
  const deepLink = `${hash}&task=task%3A1&run=${failedRunId}`
  assert.deepEqual(strongFlowHistorySelectionFromHash(deepLink), {
    taskId: 'task:1',
    stageRunId: failedRunId,
  })
  const updated = strongFlowHistoryHashWithSelection(hash, {
    taskId: 'task:1',
    stageRunId: failedRunId,
  })
  assert.equal(updated, deepLink)
  assert.deepEqual(strongFlowHistorySelectionFromHash(updated), {
    taskId: 'task:1',
    stageRunId: failedRunId,
  })
  const cleared = strongFlowHistoryHashWithSelection(deepLink, {
    taskId: null,
    stageRunId: null,
  })
  assert.equal(cleared, hash)
})

test('history tree groups StageRuns by Task, keeps Delivery stages, and flags the current run', () => {
  const tree = strongFlowHistoryTree(historyProjection(), limits)
  assert.equal(tree.tasks.length, 2)
  assert.equal(tree.tasks[0].task.id, 'task:1')
  assert.deepEqual(
    tree.tasks[0].runs.map(run => run.stageRunId),
    [failedRunId, currentRunId],
  )
  assert.equal(tree.tasks[0].runs[0].attempt, 1)
  assert.equal(tree.tasks[0].runs[0].status, 'failed')
  assert.equal(tree.tasks[0].runs[0].isCurrent, false)
  assert.equal(tree.tasks[0].runs[0].deliveryTaskId, 'task:1')
  assert.equal(tree.tasks[0].runs[1].isCurrent, true)
  assert.deepEqual(
    tree.tasks[1].runs.map(run => run.stageRunId),
    [],
  )
  assert.deepEqual(
    tree.deliveryRuns.map(run => run.stageRunId),
    [planningRunId, reviewRunId],
  )
  assert.equal(tree.deliveryRuns[0].deliveryTaskId, null)
  assert.equal(tree.deliveryRuns[1].actorType, 'human')
  assert.equal(tree.deliveryRuns[1].binding, null)
  assert.deepEqual(
    tree.runs.map(run => run.stageRunId),
    [failedRunId, currentRunId, planningRunId, reviewRunId],
  )
  assert.equal(tree.currentStageRunId, currentRunId)

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

test('history selection drops stale identities instead of keeping a second state machine', () => {
  const tree = strongFlowHistoryTree(historyProjection(), limits)
  assert.deepEqual(
    strongFlowHistorySelectionForTree(tree, {
      taskId: 'task:1',
      stageRunId: failedRunId,
    }),
    { taskId: 'task:1', stageRunId: failedRunId },
  )
  assert.deepEqual(
    strongFlowHistorySelectionForTree(tree, {
      taskId: 'task:missing',
      stageRunId: failedRunId,
    }),
    { taskId: null, stageRunId: failedRunId },
  )
  assert.deepEqual(
    strongFlowHistorySelectionForTree(tree, {
      taskId: 'task:1',
      stageRunId: 'run_00000000000000000000000009',
    }),
    { taskId: 'task:1', stageRunId: null },
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
    limits,
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

test('task rows expand into clickable historical attempts while the current run stays highlighted', () => {
  const document = new FakeDocument()
  const { view, tasksParent, stagesParent, selections } = mountNavigation(document)
  view.update(historyProjection())

  const taskRows = tasksParent.children
  assert.equal(taskRows.length, 2)
  assert.equal(taskRows[0].dataset.status, 'active')
  assert.equal(taskRows[1].dataset.status, 'pending')
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
  assert.equal(runButtons[0].dataset.stageRunId, failedRunId)
  assert.equal(runButtons[0].dataset.attempt, '1')
  assert.match(runButtons[0].textContent, /attempt 1/u)
  assert.match(runButtons[0].textContent, /failed/u)
  assert.equal(runButtons[0].getAttribute('aria-pressed'), 'false')

  runButtons[0].emit('click')
  assert.deepEqual(selections.at(-1), { taskId: 'task:1', stageRunId: failedRunId })
  assert.equal(runButtons[0].getAttribute('aria-pressed'), 'true')
  assert.equal(view.selection().stageRunId, failedRunId)

  const currentMarker = findByClass(taskRows[0], 'wwc-strongflow-current-run')
  assert.notEqual(currentMarker, null)
  assert.equal(currentMarker.getAttribute('aria-current'), 'true')
  assert.equal(currentMarker.dataset.stageRunId, currentRunId)
  assert.equal(
    findAllByClass(taskRows[0], 'wwc-strongflow-run-button')
      .some(button => button.dataset.stageRunId === currentRunId),
    false,
  )

  const timelineButtons = findAllByClass(stagesParent, 'wwc-strongflow-run-button')
  assert.deepEqual(
    timelineButtons.map(button => button.dataset.stageRunId),
    [failedRunId, planningRunId, reviewRunId],
  )
  assert.equal(stagesParent.children.length, 4)
  assert.equal(stagesParent.children[0].dataset.status, 'failed')
  const timelineCurrent = findByClass(stagesParent, 'wwc-strongflow-current-run')
  assert.equal(timelineCurrent.dataset.stageRunId, currentRunId)

  toggle.emit('click')
  assert.equal(toggle.getAttribute('aria-expanded'), 'false')
  assert.equal(findByClass(taskRows[0], 'wwc-strongflow-run-list').hidden, true)
  view.close()
})

test('keyboard roving focus moves through Task toggles and attempts without a mouse', () => {
  const document = new FakeDocument()
  const { view, tasksParent, stagesParent } = mountNavigation(document)
  view.update(historyProjection())

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

test('the read-only historical run detail shows exact identity, binding, evidence, candidates and conclusion', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowRunDetail({ document, limits })
  const parent = document.createElement('section')
  parent.append(view.root)

  view.update(historyProjection(), { taskId: null, stageRunId: null })
  assert.equal(view.root.hidden, true)

  view.update(historyProjection(), { taskId: 'task:1', stageRunId: currentRunId })
  assert.equal(view.root.hidden, true)

  view.update(historyProjection(), { taskId: 'task:1', stageRunId: failedRunId })
  assert.equal(view.root.hidden, false)
  assert.match(findByClass(view.root, 'wwc-strongflow-history-note').textContent, /read-only/iu)
  const identity = allText(findByClass(view.root, 'wwc-strongflow-history-identity'))
  assert.match(identity, new RegExp(failedRunId, 'u'))
  assert.match(identity, /Attempt 1/u)
  assert.match(identity, /executing/u)
  assert.match(identity, /Implement the feature/u)
  const binding = allText(findByClass(view.root, 'wwc-strongflow-history-binding'))
  assert.match(binding, /psn_00000000000000000000000001/u)
  assert.match(binding, /job_00000000000000000000000001/u)
  assert.match(binding, /wsn_00000000000000000000000001/u)
  assert.match(binding, /cdx_00000000000000000000000001/u)
  const evidence = findByClass(view.root, 'wwc-strongflow-history-evidence')
  assert.equal(evidence.children.length, 1)
  assert.match(evidence.children[0].textContent, /evidence:failed/u)
  const candidates = findByClass(view.root, 'wwc-strongflow-history-candidates')
  assert.equal(candidates.children.length, 1)
  assert.match(candidates.children[0].textContent, /refs\/winwincode\/candidate\/attempt-1/u)
  assert.equal(
    candidates.children[0].dataset.current,
    undefined,
  )
  const conclusion = findByClass(view.root, 'wwc-strongflow-history-conclusion')
  assert.equal(conclusion.dataset.status, 'failed')
  assert.match(allText(conclusion), /failed/u)
  assert.match(allText(conclusion), /2026-09-02T08:10:00\.000Z/u)

  view.update(historyProjection(), { taskId: null, stageRunId: reviewRunId })
  assert.equal(view.root.hidden, false)
  assert.match(
    allText(findByClass(view.root, 'wwc-strongflow-history-binding-host')),
    /human|no .*binding/iu,
  )
  view.close()
})

class FakeStrongFlowViewModel {
  constructor(initialState) {
    this.state = initialState
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

  async start() { this.calls.push(['start']) }
  async refresh() { this.calls.push(['refresh']) }
  async decideSolutionReview(input) { this.calls.push(['decideSolutionReview', input]) }
  async approveTaskBreakdown() { this.calls.push(['approveTaskBreakdown']) }
  async resolveAttention(input) { this.calls.push(['resolveAttention', input]) }
  async submitVerdict() { this.calls.push(['submitVerdict']) }
  async advanceDelivery() { this.calls.push(['advanceDelivery']) }
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
  const initialHash = `#/strongflow?delivery=${deliveryId}&session=psn_2&stageRun=${currentRunId}`
  const location = new FakeHistoryLocation(`${initialHash}&task=task%3A1&run=${failedRunId}`)
  const model = new FakeStrongFlowViewModel({
    status: 'ready',
    realtime: 'subscribed',
    projection: historyProjection(),
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: [{
      schemaVersion: 'winwincode/v1',
      deliveryId,
      revision: 7,
      status: 'executing',
      title: 'History navigation Delivery',
      updatedAt: '2026-09-02T08:12:30.000Z',
    }],
    limits,
    historyLocation: location,
  })

  const taskRow = findByClass(rootElement, 'wwc-strongflow-task-list').children[0]
  assert.equal(taskRow.dataset.status, 'active')
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
  ).find(button => button.dataset.stageRunId === planningRunId)
  planningButton.emit('click')
  assert.deepEqual(location.replacements.at(-1), `${initialHash}&run=${planningRunId}`)
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
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
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

test('the page shows empty history notes instead of dead lists', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const projection = historyProjection()
  projection.delivery.tasks = []
  projection.delivery.stages = [projection.delivery.stages[2]]
  const model = new FakeStrongFlowViewModel({
    status: 'ready',
    realtime: 'subscribed',
    projection,
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: [],
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
