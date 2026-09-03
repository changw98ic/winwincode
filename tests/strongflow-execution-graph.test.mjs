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
  `StrongFlow page did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const graphModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-execution-graph.js',
)).href}`)

const { mountStrongFlowExecutionGraph } = graphModule

const rootThreadId = 'cdx_00000000000000000000000001'
const childThreadId = 'cdx_00000000000000000000000002'
const stageRunId = 'run_00000000000000000000000002'
const productSessionId = 'psn_00000000000000000000000001'

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

function runtimeSession(overrides = {}) {
  return {
    activities: [
      {
        activityType: 'command',
        callId: 'call:2',
        command: 'pnpm typecheck',
        exitCode: 0,
        outcome: 'succeeded',
        sourceRef: 'runtime:call-typecheck',
        status: 'completed',
      },
      {
        activityType: 'command',
        callId: 'call:3',
        command: 'pnpm test',
        exitCode: null,
        outcome: 'observed',
        sourceRef: 'runtime:call-test',
        status: 'running',
      },
      {
        activityType: 'test',
        callId: 'call:4',
        command: 'cargo test client',
        exitCode: 101,
        outcome: 'task-failed',
        sourceRef: 'runtime:call-cargo',
        status: 'failed',
      },
    ],
    agentEdges: [
      { childThreadId: childThreadId, parentThreadId: rootThreadId },
    ],
    agents: [
      {
        nickname: 'Root agent',
        parentThreadId: null,
        path: null,
        role: 'implementer',
        sourceRef: 'runtime:agent-root',
        status: 'running',
        threadId: rootThreadId,
      },
      {
        nickname: 'Child agent',
        parentThreadId: rootThreadId,
        path: '/child',
        role: 'reviewer',
        sourceRef: 'runtime:agent-child',
        status: 'completed',
        threadId: childThreadId,
      },
    ],
    asOfSequence: 27,
    attempt: 2,
    codexThreadId: rootThreadId,
    deliveryTaskId: 'task:1',
    diffSummary: {
      additions: 20,
      changedFileCount: 3,
      deletions: 5,
      detailsVisible: false,
      sourceRef: 'runtime:diff-1',
    },
    executionJobId: 'job_00000000000000000000000001',
    fencingToken: 'fence:1',
    leaseId: 'lease:1',
    plan: null,
    productSessionId,
    recovery: {
      failureCount: 1,
      lastFailureSourceRef: 'runtime:failure-1',
      latestRecoverySourceRef: 'runtime:recovery-1',
      recoveryCount: 1,
      state: 'recovered',
    },
    sessionBindingId: 'bind:1',
    stageRunId,
    usage: {
      sourceRef: 'runtime:usage-1',
      totals: [
        { name: 'input_tokens', value: 120 },
        { name: 'output_tokens', value: 45 },
      ],
    },
    workerSessionId: 'wsn_00000000000000000000000001',
    ...overrides,
  }
}

function evidenceRow(overrides = {}) {
  return {
    candidateRef: 'refs/winwincode/candidate/attempt-2',
    createdAt: '2026-09-02T08:09:00.000Z',
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 3,
    id: 'evidence:command-1',
    sessionBindingId: 'bind:1',
    sourceRef: 'runtime_event:event-1',
    stageRunId,
    type: 'command',
    ...overrides,
  }
}

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
  tabIndex = 0
  scrollIntoView() {
    this.scrolled = true
  }
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() {
    return this.children
  }

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

function mountGraph(document, overrides = {}) {
  const opened = []
  const view = mountStrongFlowExecutionGraph({
    document,
    limits,
    onOpenEvidence: evidenceId => { opened.push(evidenceId) },
    ...overrides,
  })
  return { view, opened }
}

test('the execution graph shows parent-child agents, current activity, tools, Diff, usage and failures at a glance', () => {
  const document = new FakeDocument()
  const { view } = mountGraph(document)
  document.createElement('section').append(view.root)
  view.update({
    heading: 'Live execution view',
    emptyText: 'No live execution sessions are available.',
    sessions: [runtimeSession()],
    evidence: [],
    approvals: [],
    readOnly: false,
  })

  const sessions = findAllByClass(view.root, 'wwc-strongflow-execution-session')
  assert.equal(sessions.length, 1)
  const session = sessions[0]
  assert.equal(session.dataset.attempt, '2')
  assert.match(allText(findByClass(session, 'wwc-strongflow-execution-heading')), /Task task:1/u)

  const agents = findAllByClass(session, 'wwc-strongflow-agent-node')
  assert.equal(agents.length, 2)
  assert.equal(agents[0].dataset.depth, '0')
  assert.equal(agents[1].dataset.depth, '1')
  assert.match(allText(agents[0]), /Root agent/u)
  assert.match(allText(agents[0]), /implementer/u)
  assert.match(allText(agents[0]), /running/u)
  assert.match(allText(agents[1]), /Child agent/u)
  assert.match(allText(agents[1]), /completed/u)

  const outcome = allText(findByClass(session, 'wwc-strongflow-execution-outcome'))
  assert.match(outcome, /pnpm test/u, 'the running activity is the current activity')
  assert.match(outcome, /3/u, 'Diff changed file count is visible')
  assert.match(outcome, /input_tokens/u, 'usage totals are visible')
  assert.match(outcome, /recovered/u, 'the recovery state is visible')
  assert.match(outcome, /1/u, 'failure counts are visible')
  view.close()
})

test('the timeline folds and filters by existing activity type and jumps only to matching Evidence', () => {
  const document = new FakeDocument()
  const { view, opened } = mountGraph(document)
  document.createElement('section').append(view.root)
  view.update({
    heading: 'Live execution view',
    emptyText: 'No live execution sessions are available.',
    sessions: [runtimeSession()],
    evidence: [evidenceRow()],
    approvals: [],
    readOnly: false,
  })

  const session = findByClass(view.root, 'wwc-strongflow-execution-session')
  const chips = findAllByClass(session, 'wwc-strongflow-activity-filter')
  assert.deepEqual(
    chips.map(chip => chip.dataset.activityType),
    ['command', 'test'],
  )
  assert.equal(chips.every(chip => chip.getAttribute('aria-pressed') === 'true'), true)
  assert.match(chips[0].textContent, /command · 2/u)
  assert.match(chips[1].textContent, /test · 1/u)

  const groups = findAllByClass(session, 'wwc-strongflow-activity-group')
  assert.equal(groups.length, 2)
  assert.equal(groups[0].dataset.activityType, 'command')
  const groupToggle = findByClass(groups[0], 'wwc-strongflow-activity-group-toggle')
  assert.equal(groupToggle.tagName, 'BUTTON')
  assert.equal(groupToggle.getAttribute('aria-expanded'), 'true')
  assert.equal(groupToggle.getAttribute('aria-controls'), `wwc-strongflow-activity-rows-${groups[0].dataset.activityType}-${session.dataset.sessionKey}`)
  const commandRows = findAllByClass(groups[0], 'wwc-strongflow-activity-row')
  assert.equal(commandRows.length, 2)
  assert.equal(groupToggle.type, 'button')

  groupToggle.emit('click')
  assert.equal(groupToggle.getAttribute('aria-expanded'), 'false')
  const commandList = findByClass(groups[0], 'wwc-strongflow-activity-rows')
  assert.equal(commandList.hidden, true)
  groupToggle.emit('click')
  assert.equal(groupToggle.getAttribute('aria-expanded'), 'true')
  assert.equal(commandList.hidden, false)

  chips[1].emit('click')
  assert.equal(chips[1].getAttribute('aria-pressed'), 'false')
  assert.equal(groups[1].hidden, true, 'the filter hides the whole type group')
  chips[1].emit('click')
  assert.equal(chips[1].getAttribute('aria-pressed'), 'true')
  assert.equal(groups[1].hidden, false)

  const rows = findAllByClass(session, 'wwc-strongflow-activity-row')
  const commandRow = rows.find(row => row.dataset.callId === 'call:2')
  const testRow = rows.find(row => row.dataset.callId === 'call:4')
  assert.notEqual(commandRow, undefined)
  const jump = findByClass(commandRow, 'wwc-strongflow-activity-evidence')
  assert.notEqual(jump, null, 'a matching Evidence record enables the jump')
  jump.emit('click')
  assert.deepEqual(opened, ['evidence:command-1'])
  assert.equal(
    findByClass(testRow, 'wwc-strongflow-activity-evidence'),
    null,
    'a test activity without matching Evidence never renders a fake jump',
  )
  view.close()
})

test('high-frequency deltas keep DOM identity, a bounded window, and filter, fold and focus context', () => {
  const document = new FakeDocument()
  const boundedLimits = { ...limits, activities: 3 }
  const view = mountStrongFlowExecutionGraph({
    document,
    limits: boundedLimits,
    onOpenEvidence: () => {},
  })
  document.createElement('section').append(view.root)
  const buildSession = index => runtimeSession({
    activities: Array.from({ length: 5 }, (_, activityIndex) => ({
      activityType: activityIndex % 2 === 0 ? 'command' : 'test',
      callId: `call:${String(activityIndex + 1)}`,
      command: `pnpm task ${String(activityIndex + 1)} r${String(index)}`,
      exitCode: 0,
      outcome: activityIndex === 4 && index % 2 === 0 ? 'observed' : 'succeeded',
      sourceRef: `runtime:call-${String(activityIndex + 1)}`,
      status: activityIndex === 4 && index % 2 === 0 ? 'running' : 'completed',
    })),
    asOfSequence: index,
  })
  view.update({
    heading: 'Live execution view',
    emptyText: 'No live execution sessions are available.',
    sessions: [buildSession(0)],
    evidence: [],
    approvals: [],
    readOnly: false,
  })

  const session = findByClass(view.root, 'wwc-strongflow-execution-session')
  const rowsBefore = findAllByClass(session, 'wwc-strongflow-activity-row')
  assert.equal(rowsBefore.length, 3, 'the render window is bounded by the activity limit')
  const omitted = findByClass(session, 'wwc-strongflow-activity-omitted')
  assert.match(omitted.textContent, /2 more runtime activities/u)
  const stableRow = rowsBefore.find(row => row.dataset.callId === 'call:1')
  const chip = findByClass(session, 'wwc-strongflow-activity-filter')
  chip.emit('click')
  assert.equal(chip.getAttribute('aria-pressed'), 'false')
  const groupToggle = findByClass(session, 'wwc-strongflow-activity-group-toggle')
  groupToggle.emit('click')
  stableRow.focus()
  const focusedBefore = document.activeElement
  const nodeCountBefore = flatten(view.root).length

  for (let index = 1; index <= 200; index += 1) {
    view.update({
      heading: 'Live execution view',
      emptyText: 'No live execution sessions are available.',
      sessions: [buildSession(index)],
      evidence: [],
      approvals: [],
      readOnly: false,
    })
  }

  const sessionAfter = findByClass(view.root, 'wwc-strongflow-execution-session')
  assert.equal(sessionAfter, session, 'the session section keeps its DOM identity')
  const rowsAfter = findAllByClass(session, 'wwc-strongflow-activity-row')
  assert.equal(rowsAfter.length, 3, 'the bounded window never grows with deltas')
  assert.equal(
    rowsAfter.find(row => row.dataset.callId === 'call:1'),
    stableRow,
    'stable activity rows keep their DOM node across deltas',
  )
  assert.equal(document.activeElement, focusedBefore, 'focus survives every delta')
  const chipAfter = findByClass(session, 'wwc-strongflow-activity-filter')
  assert.equal(chipAfter, chip, 'the filter chip keeps its node')
  assert.equal(chipAfter.getAttribute('aria-pressed'), 'false', 'the filter survives deltas')
  assert.equal(
    findByClass(session, 'wwc-strongflow-activity-group-toggle').getAttribute('aria-expanded'),
    'false',
    'the fold state survives deltas',
  )
  assert.equal(
    flatten(view.root).length <= nodeCountBefore + 2,
    true,
    'the total DOM size stays bounded across deltas',
  )
  view.close()
})

test('read-only historical projections hide approvals and mark the graph read-only while Evidence jumps still work', () => {
  const document = new FakeDocument()
  const { view, opened } = mountGraph(document)
  document.createElement('section').append(view.root)
  view.update({
    heading: 'Runtime projection',
    emptyText: 'No runtime projection — this StageRun has no runtime binding.',
    sessions: [runtimeSession()],
    evidence: [evidenceRow()],
    approvals: [{
      id: 'attention:1',
      title: 'Approve delivery',
      type: 'delivery_approval',
      status: 'open',
      blocking: true,
    }],
    readOnly: true,
  })

  const session = findByClass(view.root, 'wwc-strongflow-execution-session')
  assert.equal(session.dataset.readOnly, 'true')
  assert.match(allText(session), /read-only/iu)
  assert.equal(findByClass(session, 'wwc-strongflow-execution-approvals'), null)
  const rows = findAllByClass(session, 'wwc-strongflow-activity-row')
  const jump = findByClass(rows[0], 'wwc-strongflow-activity-evidence')
  jump.emit('click')
  assert.deepEqual(opened, ['evidence:command-1'])
  view.close()
})

test('open gating approvals are tied to the current attempt as display-only chips', () => {
  const document = new FakeDocument()
  const { view } = mountGraph(document)
  document.createElement('section').append(view.root)
  view.update({
    heading: 'Live execution view',
    emptyText: 'No live execution sessions are available.',
    sessions: [runtimeSession()],
    evidence: [],
    approvals: [
      {
        id: 'attention:1',
        title: 'Approve the solution',
        type: 'delivery_approval',
        status: 'open',
        blocking: true,
      },
      {
        id: 'attention:2',
        title: 'Scope question',
        type: 'requirement_question',
        status: 'open',
        blocking: false,
      },
    ],
    readOnly: false,
  })

  const approvals = findByClass(view.root, 'wwc-strongflow-execution-approvals')
  assert.notEqual(approvals, null)
  const chips = findAllByClass(approvals, 'wwc-strongflow-approval-chip')
  assert.equal(chips.length, 2)
  assert.equal(chips[0].dataset.blocking, 'true')
  assert.match(allText(chips[0]), /Approve the solution/u)
  assert.match(allText(chips[0]), /open/u)
  assert.equal(approvals.getAttribute('aria-label'), 'Approvals and attention')
  view.close()
})

test('an empty session list renders the shared empty state and hidden sections instead of stale DOM', () => {
  const document = new FakeDocument()
  const { view } = mountGraph(document)
  document.createElement('section').append(view.root)
  view.update({
    heading: 'Live execution view',
    emptyText: 'No live execution sessions are available.',
    sessions: [runtimeSession()],
    evidence: [],
    approvals: [],
    readOnly: false,
  })
  view.update({
    heading: 'Live execution view',
    emptyText: 'No live execution sessions are available.',
    sessions: [],
    evidence: [],
    approvals: [],
    readOnly: false,
  })
  assert.equal(findAllByClass(view.root, 'wwc-strongflow-execution-session').length, 0)
  const empty = findByClass(view.root, 'wwc-strongflow-execution-empty')
  assert.notEqual(empty, null)
  assert.equal(empty.hidden, false)
  assert.match(empty.textContent, /No live execution sessions/u)
  view.close()
})
