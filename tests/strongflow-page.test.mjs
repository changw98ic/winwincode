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

const page = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-page.js',
)).href}`)
const rendering = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-rendering.js',
)).href}`)
const application = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/application.js',
)).href}`)

const { mountStrongFlowPage, strongFlowPagePresentation } = page
const { boundedItems } = rendering
const { clientSurfaceFromHash, strongFlowRouteHash } = application
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'

function many(count, create) {
  return Array.from({ length: count }, (_, index) => create(index + 1))
}

function diagram(kind) {
  return {
    id: `diagram:${kind}`,
    kind,
    title: `${kind} diagram`,
    nodes: many(5, value => ({
      id: `node:${String(value)}`,
      label: `Node ${String(value)}`,
      description: `Description ${String(value)}`,
      kind: 'component',
      trustBoundary: null,
      unresolved: false,
    })),
    edges: many(5, value => ({
      id: `edge:${String(value)}`,
      from: 'node:1',
      to: `node:${String(value)}`,
      label: `Edge ${String(value)}`,
    })),
  }
}

function projection() {
  const candidateRef = 'refs/winwincode/candidate/1'
  return {
    delivery: {
      schemaVersion: 'winwincode/v1',
      deliveryId,
      deliveryRevision: 4,
      status: 'executing',
      ownership: {
        organizationId: 'org_00000000000000000000000001',
        workspaceId: 'wsp_00000000000000000000000001',
        projectId: 'prj_00000000000000000000000001',
        repositoryId: 'rep_00000000000000000000000001',
      },
      requirements: {
        title: 'Bounded StrongFlow workspace',
        goal: 'Render the exact advanced workflow without unbounded DOM growth.',
      },
      tasks: many(5, value => ({
        id: `task:${String(value)}`,
        title: `Task ${String(value)}`,
        status: value === 1 ? 'active' : 'pending',
      })),
      stages: many(5, value => ({
        id: value === 1 ? stageRunId : `run_${String(value).padStart(26, '0')}`,
        stage: value === 1 ? 'executing' : 'verifying',
        role: 'implementer',
        status: value === 1 ? 'running' : 'waiting',
      })),
      attention: many(5, value => ({
        id: `attention:${String(value)}`,
        title: `Attention ${String(value)}`,
        status: value === 1 ? 'open' : 'resolved',
      })),
    },
    solutionReview: {
      reviewStatus: 'approved',
      architectureDiagram: diagram('system-architecture'),
      processDiagram: diagram('process-flow'),
    },
    stage: { id: stageRunId },
    runtime: {
      stageRunId,
      sessions: many(3, sessionValue => ({
        deliveryTaskId: `task:${String(sessionValue)}`,
        attempt: sessionValue,
        asOfSequence: sessionValue,
        agents: many(5, agentValue => ({
          threadId: `cdx_${String(agentValue).padStart(26, '0')}`,
          nickname: `Agent ${String(agentValue)}`,
          role: 'worker',
          status: agentValue === 1 ? 'running' : 'waiting',
        })),
        agentEdges: many(5, edgeValue => ({
          parentThreadId: 'cdx_00000000000000000000000001',
          childThreadId: `cdx_${String(edgeValue).padStart(26, '0')}`,
        })),
        activities: many(5, activityValue => ({
          activityType: 'test',
          status: activityValue === 1 ? 'running' : 'completed',
          outcome: activityValue === 1 ? 'observed' : 'succeeded',
        })),
        diffSummary: {
          changedFileCount: 3,
          additions: 20,
          deletions: 5,
          sourceRef: 'runtime:diff:1',
        },
      })),
    },
    evidence: many(5, value => ({
      id: `evidence:${String(value)}`,
      type: 'test',
      sourceRef: `artifact:test:${String(value)}`,
      candidateRef,
    })),
    verdict: {
      id: 'verdict:1',
      status: 'pass',
      producedAt: '2026-08-27T01:00:05.000Z',
    },
    attention: many(5, value => ({
      id: `attention:${String(value)}`,
      title: `Attention ${String(value)}`,
      status: value === 1 ? 'open' : 'resolved',
    })),
    currentCandidate: {
      candidateRef,
      candidateCommitId: '1111111111111111111111111111111111111111',
      candidateTreeId: '2222222222222222222222222222222222222222',
      diffSha256: `sha256:${'3'.repeat(64)}`,
      frozenAt: '2026-08-27T01:00:04.000Z',
    },
    publication: {
      state: 'pending',
      revision: 1,
      updatedAt: '2026-08-27T01:00:06.000Z',
    },
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-08-27T01:00:06.000Z',
      revisions: {
        delivery: 4,
        deliverySpec: 3,
        runtime: 8,
        publication: 1,
      },
      readCursor: {},
    },
  }
}

function state(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    projection: projection(),
    interaction: { status: 'idle', error: null },
    error: null,
    ...overrides,
  }
}

function error(kind, message) {
  return {
    kind,
    code: 'TEST_ERROR',
    message,
    requestId: null,
    retryable: false,
  }
}

function deliverySummary(value) {
  return {
    schemaVersion: 'winwincode/v1',
    deliveryId: `dlv_${String(value).padStart(26, '0')}`,
    title: `Delivery ${String(value)}`,
    revision: value,
    status: 'executing',
  }
}

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  type = ''
  href = ''
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }

  append(...children) {
    this.children.push(...children)
  }

  replaceChildren(...children) {
    this.children = [...children]
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value))
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  emit(name, values = {}) {
    for (const listener of this.listeners.get(name) ?? []) listener(values)
  }
}

class FakeDocument {
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

const limits = {
  deliveries: 2,
  tasks: 2,
  stages: 2,
  attention: 2,
  evidence: 2,
  runtimeSessions: 2,
  graphNodes: 2,
  graphEdges: 2,
  activities: 2,
}

test('render limits are deterministic and reject unbounded configuration', () => {
  assert.deepEqual(boundedItems([1, 2, 3, 4], 2), { items: [1, 2], omitted: 2 })
  assert.throws(() => boundedItems([1], 0), /between 1 and 500/u)
  assert.throws(() => boundedItems([1], 501), /between 1 and 500/u)
})

test('presentation keeps reconnect and errors understandable without raw server details', () => {
  const ready = strongFlowPagePresentation(state())
  assert.match(ready.statusText, /revision 4/u)
  const disconnected = strongFlowPagePresentation(state({
    realtime: 'reconnecting',
    error: error('network', 'http://worker.internal:9000/TOKEN'),
  }))
  assert.equal(disconnected.reconnectVisible, true)
  assert.match(disconnected.errorText, /connection and retry/u)
  assert.doesNotMatch(disconnected.errorText, /worker|9000|TOKEN/iu)
  const emptyError = strongFlowPagePresentation(state({
    status: 'error',
    realtime: 'reconnecting',
    projection: null,
    error: error('protocol', 'SECRET'),
  }))
  assert.equal(emptyError.retryVisible, true)
  assert.equal(emptyError.reconnectVisible, false)
})

test('workspace renders Delivery, solution, execution, candidate, Evidence, Verdict and Publication views within limits', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(5, deliverySummary),
    limits,
  })

  const status = findByClass(rootElement, 'wwc-strongflow-status')
  const alert = findByClass(rootElement, 'wwc-strongflow-error')
  const content = findByClass(rootElement, 'wwc-strongflow-content')
  const deliveryList = findByClass(rootElement, 'wwc-strongflow-delivery-list')
  const tasks = findByClass(rootElement, 'wwc-strongflow-task-list')
  const evidence = findByClass(rootElement, 'wwc-strongflow-evidence')
  const diagramNodes = findAllByClass(rootElement, 'wwc-strongflow-diagram-nodes')
  const executionSessions = findAllByClass(rootElement, 'wwc-strongflow-execution-session')
  assert.equal(status.getAttribute('role'), 'status')
  assert.equal(status.getAttribute('aria-live'), 'polite')
  assert.equal(alert.getAttribute('role'), 'alert')
  assert.equal(alert.getAttribute('aria-live'), 'assertive')
  assert.equal(content.getAttribute('aria-busy'), 'false')
  assert.equal(deliveryList.children.length, 2)
  assert.match(deliveryList.children[0].children[0].href, /^#\/strongflow\?delivery=/u)
  assert.equal(tasks.children.length, 2)
  assert.equal(evidence.children.length, 2)
  assert.equal(diagramNodes.length, 2)
  assert.equal(diagramNodes.every(list => list.children.length === 2), true)
  assert.equal(executionSessions.length, 2)
  assert.ok(findByClass(rootElement, 'wwc-strongflow-view-solution'))
  assert.ok(findByClass(rootElement, 'wwc-strongflow-view-execution'))
  assert.ok(findByClass(rootElement, 'wwc-strongflow-view-candidate'))
  assert.equal(findByClass(rootElement, 'wwc-strongflow-verdict').dataset.status, 'pass')
  assert.equal(findByClass(rootElement, 'wwc-strongflow-publication').dataset.status, 'pending')
  assert.equal(findAllByClass(rootElement, 'wwc-strongflow-omitted').length > 0, true)

  model.publish(state({
    realtime: 'reconnecting',
    error: error('network', 'Disconnected.'),
  }))
  findByClass(rootElement, 'wwc-strongflow-reconnect').emit('click')
  assert.deepEqual(model.calls.at(-1), ['reconnect'])
  model.publish(state({
    status: 'error',
    realtime: 'reconnecting',
    projection: null,
    error: error('protocol', 'Failed.'),
  }))
  findByClass(rootElement, 'wwc-strongflow-retry').emit('click')
  assert.deepEqual(model.calls.at(-1), ['refresh'])

  mounted.close()
  assert.deepEqual(model.calls.at(-1), ['close'])
  assert.deepEqual(rootElement.children, [])
})

test('default route remains Chat while StrongFlow query routes stay on the advanced surface', () => {
  assert.equal(clientSurfaceFromHash('').id, 'chat')
  assert.equal(clientSurfaceFromHash('#/chat').id, 'chat')
  assert.equal(clientSurfaceFromHash('#/strongflow?delivery=dlv_1').id, 'strongflow')
  assert.equal(
    strongFlowRouteHash(
      deliveryId,
      'psn_00000000000000000000000002',
      'run_00000000000000000000000003',
    ),
    `#/strongflow?delivery=${deliveryId}`
      + '&session=psn_00000000000000000000000002'
      + '&stageRun=run_00000000000000000000000003',
  )
})

test('review controls send only current view-model decisions and disable while waiting', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const current = projection()
  current.solutionReview.reviewStatus = 'pending'
  const model = new FakeStrongFlowViewModel(state({ projection: current }))
  const mounted = mountStrongFlowPage({ root: rootElement, model, limits })
  const actions = findByClass(rootElement, 'wwc-strongflow-solution-actions')
  actions.children[0].children[0].value = 'Approve the exact current review.'
  findByClass(rootElement, 'wwc-strongflow-approve-solution').emit('click')
  assert.deepEqual(model.calls.at(-1), ['decideSolutionReview', {
    action: 'approve',
    comments: 'Approve the exact current review.',
    requestedChanges: [],
  }])

  model.publish(state({
    projection: current,
    interaction: { status: 'waiting', error: null },
  }))
  assert.equal(findByClass(rootElement, 'wwc-strongflow-approve-solution').disabled, true)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-actions').getAttribute('aria-busy'), 'true')
  mounted.close()
})

test('verdict control stays hidden until all active StageRuns settle', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const current = projection()
  current.verdict = null
  const model = new FakeStrongFlowViewModel(state({ projection: current }))
  const mounted = mountStrongFlowPage({ root: rootElement, model, limits })

  assert.equal(findByClass(rootElement, 'wwc-strongflow-submit-verdict'), null)

  current.delivery.stages.forEach(stage => {
    stage.status = 'succeeded'
  })
  model.publish(state({ projection: current }))
  assert.notEqual(findByClass(rootElement, 'wwc-strongflow-submit-verdict'), null)
  mounted.close()
})

test('StrongFlow page modules call only the view-model and never render raw Diff or open transports', () => {
  const sources = [
    'strongflow-page.ts',
    'strongflow-diagrams.ts',
    'strongflow-candidate.ts',
    'strongflow-rendering.ts',
  ].map(path => readFileSync(resolve(root, 'apps/client/src', path), 'utf8')).join('\n')
  assert.match(sources, /options\.model\.start/u)
  assert.match(sources, /options\.model\.refresh/u)
  assert.match(sources, /model\.decideSolutionReview/u)
  assert.match(sources, /model\.resolveAttention/u)
  assert.match(sources, /detailsVisible: false|Diff digest|diffSummary/u)
  assert.doesNotMatch(
    sources,
    /\bfetch\s*\(|new\s+WebSocket|@deepseek-ai|dsh-typert|remote\.|\.query\s*\(|\.command\s*\(|innerHTML/iu,
  )
  assert.doesNotMatch(sources, /dataBase64|unifiedDiff|patchBytes|worker\.internal/iu)
})
