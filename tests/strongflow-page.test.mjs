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

const {
  mountStrongFlowCreatePage,
  mountStrongFlowPage,
  strongFlowPagePresentation,
} = page
const { boundedItems } = rendering
const { clientSurfaceFromHash, parseStrongFlowRouteHash, strongFlowRouteHash } = application
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
      createdAt: '2026-08-27T01:00:04.000Z',
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      sessionBindingId: 'binding:1',
      stageRunId: value === 1 ? stageRunId : `run_${String(value).padStart(26, '0')}`,
    })),
    verdict: {
      id: 'verdict:1',
      status: 'pass',
      producedAt: '2026-08-27T01:00:05.000Z',
      criteria: [{
        criterionId: 'criterion:1',
        evaluatedAt: '2026-08-27T01:00:05.000Z',
        evidenceRefs: ['evidence:1'],
        explanation: 'The exact check passed.',
        resultId: 'result:1',
        verdict: 'pass',
      }],
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
  parentNode = null
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  readOnly = false
  required = false
  type = ''
  value = ''
  href = ''
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
    const index = reference === null ? this.children.length : this.children.indexOf(reference)
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
    const listeners = this.listeners.get(name) ?? []
    this.listeners.set(name, listeners.filter(candidate => candidate !== listener))
  }

  emit(name, values = {}) {
    for (const listener of this.listeners.get(name) ?? []) listener(values)
  }

  focus() {}

  blur() {}
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

class FakeStrongFlowViewModel {
  constructor(initialState) {
    this.state = initialState
  }

  calls = []
  listeners = new Set()

  subscribe(listener) {
    this.listeners.add(listener)
    listener(this.state)
    return () => { this.listeners.delete(listener) }
  }

  publish(next) {
    this.state = next
    for (const listener of this.listeners) listener(next)
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

class FakeStrongFlowCreateViewModel {
  state = { status: 'idle', error: null }
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

  async create(input) { this.calls.push(['create', input]) }
  cancelPending() { this.calls.push(['cancelPending']) }
  close() { this.calls.push(['close']) }
}

const pageActor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const pageScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}

function pageEvidenceClient() {
  return {
    queries: [],
    async query(request) {
      this.queries.push(request)
      if (request.query === 'evidence.get') {
        const binding = request.parameters
        return {
          schemaVersion: 'winwincode/v1',
          requestId: request.requestId,
          query: request.query,
          result: {
            kind: 'evidence_detail',
            artifactAccess: { state: 'unavailable', reason: 'no_authoritative_link' },
            evidence: {
              candidateRef: binding.candidateRef,
              createdAt: '2026-08-27T01:00:04.000Z',
              deliverySpecId: 'spec:1',
              deliverySpecRevision: 3,
              id: binding.evidenceId,
              sessionBindingId: binding.sessionBindingId,
              sourceRef: binding.sourceRef,
              stageRunId: binding.stageRunId,
              type: binding.type,
            },
            outcome: 'succeeded',
            readCursor: request.parameters.atCursor,
          },
          page: { hasMore: false, nextCursor: null },
        }
      }
      throw new Error(`unexpected page evidence query: ${request.query}`)
    },
  }
}

function pageEvidenceDeepLink() {
  const state = { hash: `#/strongflow?delivery=${deliveryId}` }
  const link = {
    get route() {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      const tab = parameters.get('tab')
      return {
        tab: tab === 'tests' || tab === 'logs' ? tab : 'evidence',
        evidenceId: parameters.get('evidence'),
      }
    },
    onRouteChange(route) {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      parameters.set('tab', route.tab)
      if (route.evidenceId === null) parameters.delete('evidence')
      else parameters.set('evidence', route.evidenceId)
      state.hash = `#/strongflow?${parameters.toString()}`
    },
    state,
  }
  return link
}

function pageEvidenceOptions(overrides = {}) {
  const link = pageEvidenceDeepLink()
  return {
    client: pageEvidenceClient(),
    actor: pageActor,
    scope: pageScope,
    nextRequestId: () => 'req_00000000000000000000000001',
    route: link.route,
    onRouteChange: link.onRouteChange,
    ...overrides,
  }
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
  const evidenceOptions = pageEvidenceOptions()
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(5, deliverySummary),
    limits,
    evidence: evidenceOptions,
  })

  const status = findByClass(rootElement, 'wwc-strongflow-status')
  const alert = findByClass(rootElement, 'wwc-strongflow-error')
  const content = findByClass(rootElement, 'wwc-strongflow-content')
  const deliveryList = findByClass(rootElement, 'wwc-strongflow-delivery-list')
  const tasks = findByClass(rootElement, 'wwc-strongflow-task-list')
  const evidenceTabs = findByClass(rootElement, 'wwc-strongflow-evidence-tabs')
  const evidenceList = findByClass(rootElement, 'wwc-strongflow-evidence-list')
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
  assert.equal(evidenceTabs.getAttribute('role'), 'tablist')
  assert.deepEqual(
    evidenceTabs.children.map(tab => tab.textContent),
    ['Evidence', 'Tests', 'Logs'],
  )
  assert.equal(evidenceList.children.length, 2)
  assert.equal(
    findAllByClass(rootElement, 'wwc-strongflow-omitted').some(note => (
      /3 more evidence records/u.test(note.textContent)
    )),
    true,
  )
  assert.equal(findByClass(rootElement, 'wwc-strongflow-evidence'), null)
  assert.equal(diagramNodes.length, 2)
  assert.equal(diagramNodes.every(list => list.children.length === 2), true)
  assert.equal(executionSessions.length, 2)
  assert.ok(findByClass(rootElement, 'wwc-strongflow-view-solution'))
  assert.ok(findByClass(rootElement, 'wwc-strongflow-view-execution'))
  assert.ok(findByClass(rootElement, 'wwc-strongflow-view-candidate'))
  assert.equal(findByClass(rootElement, 'wwc-strongflow-verdict').dataset.status, 'pass')
  assert.equal(findByClass(rootElement, 'wwc-strongflow-publication').dataset.status, 'pending')
  assert.equal(findAllByClass(rootElement, 'wwc-strongflow-omitted').length > 0, true)

  assert.equal(evidenceOptions.client.queries.length, 0)
  findAllByClass(rootElement, 'wwc-strongflow-evidence-row')[0].emit('click')
  assert.equal(evidenceOptions.client.queries.length, 1)
  assert.equal(evidenceOptions.client.queries[0].query, 'evidence.get')
  assert.equal(evidenceOptions.client.queries[0].parameters.evidenceId, 'evidence:1')

  for (const className of [
    'wwc-strongflow-stage-evidence-open',
    'wwc-strongflow-candidate-evidence-open',
    'wwc-strongflow-criterion-evidence-open',
  ]) {
    const entry = findByClass(rootElement, className)
    assert.notEqual(entry, null, `${className} must expose an exact Evidence entry point`)
    entry.emit('click')
  }
  assert.deepEqual(
    evidenceOptions.client.queries.slice(-3).map(query => query.parameters.evidenceId),
    ['evidence:1', 'evidence:1', 'evidence:1'],
  )

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
  assert.equal(findAllByClass(rootElement, 'wwc-strongflow-evidence-row').length, 0)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-evidence-host').hidden, true)
  findByClass(rootElement, 'wwc-strongflow-retry').emit('click')
  assert.deepEqual(model.calls.at(-1), ['refresh'])

  mounted.close()
  assert.deepEqual(model.calls.at(-1), ['close'])
  assert.deepEqual(rootElement.children, [])
})

test('empty StrongFlow keeps one complete creation draft through command errors', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowCreateViewModel()
  const mounted = mountStrongFlowCreatePage({ root: rootElement, model, scope: {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001',
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
  } })
  const title = findByClass(rootElement, 'wwc-strongflow-create-title')
  const goal = findByClass(rootElement, 'wwc-strongflow-create-goal')
  const baseline = findByClass(rootElement, 'wwc-strongflow-create-baseline')
  const deliveryScope = findByClass(rootElement, 'wwc-strongflow-create-delivery-scope')
  const outOfScope = findByClass(rootElement, 'wwc-strongflow-create-out-of-scope')
  const constraints = findByClass(rootElement, 'wwc-strongflow-create-constraints')
  const criteria = findByClass(rootElement, 'wwc-strongflow-create-criteria')
  const scopeValue = findByClass(rootElement, 'wwc-strongflow-create-scope')
  title.value = 'First StrongFlow Delivery'
  goal.value = 'Open the advanced workspace.'
  baseline.value = '0123456789abcdef0123456789abcdef01234567'
  deliveryScope.value = 'Open the advanced workspace.'
  outOfScope.value = 'Replace the default Chat surface.'
  constraints.value = 'Keep the exact repository binding.'
  criteria.value = 'Load the real Delivery snapshot.\nSubscribe to Delivery events.'

  findByClass(rootElement, 'wwc-strongflow-create-form').emit('submit', {
    preventDefault() {},
  })
  assert.deepEqual(model.calls.at(-1), ['create', {
    title: 'First StrongFlow Delivery',
    goal: 'Open the advanced workspace.',
    baseRevision: '0123456789abcdef0123456789abcdef01234567',
    scope: ['Open the advanced workspace.'],
    outOfScope: ['Replace the default Chat surface.'],
    constraints: ['Keep the exact repository binding.'],
    sourceProductSessionId: null,
    acceptanceCriteria: [
      'Load the real Delivery snapshot.',
      'Subscribe to Delivery events.',
    ],
  }])
  assert.equal(scopeValue.readOnly, true)
  assert.match(scopeValue.value, /rep_00000000000000000000000001/u)

  model.publish({
    status: 'error',
    error: error('server', 'private command details'),
  })
  assert.equal(title.value, 'First StrongFlow Delivery')
  assert.equal(goal.value, 'Open the advanced workspace.')
  assert.equal(baseline.value, '0123456789abcdef0123456789abcdef01234567')
  assert.equal(deliveryScope.value, 'Open the advanced workspace.')
  assert.equal(outOfScope.value, 'Replace the default Chat surface.')
  assert.equal(constraints.value, 'Keep the exact repository binding.')
  assert.equal(criteria.value, 'Load the real Delivery snapshot.\nSubscribe to Delivery events.')
  assert.match(findByClass(rootElement, 'wwc-strongflow-create-error').textContent, /retry/iu)

  model.publish({ status: 'submitting', error: null })
  assert.equal(findByClass(rootElement, 'wwc-strongflow-create-submit').disabled, true)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-create-form').getAttribute('aria-busy'), 'true')
  const cancel = findByClass(rootElement, 'wwc-strongflow-create-cancel')
  assert.equal(cancel.hidden, false)
  cancel.emit('click')
  assert.deepEqual(model.calls.at(-1), ['cancelPending'])
  mounted.close()
  assert.deepEqual(model.calls.at(-1), ['close'])
  assert.deepEqual(rootElement.children, [])
})

test('default route remains Chat while StrongFlow query routes stay on the advanced surface', () => {
  assert.equal(clientSurfaceFromHash('').id, 'chat')
  assert.equal(clientSurfaceFromHash('#/chat').id, 'chat')
  assert.equal(clientSurfaceFromHash('#/strongflow?delivery=dlv_1').id, 'strongflow')
  assert.equal(
    strongFlowRouteHash({
      deliveryId,
      productSessionId: 'psn_00000000000000000000000002',
      stageRunId: 'run_00000000000000000000000003',
      evidenceTab: 'logs',
      evidenceId: 'evidence:1',
    }),
    `#/strongflow?delivery=${deliveryId}`
      + '&session=psn_00000000000000000000000002'
      + '&stageRun=run_00000000000000000000000003'
      + '&tab=logs&evidence=evidence%3A1',
  )
  assert.deepEqual(
    parseStrongFlowRouteHash(
      `#/strongflow?delivery=${deliveryId}`
        + '&session=psn_00000000000000000000000002'
        + '&stageRun=run_00000000000000000000000003'
        + '&tab=bogus&evidence=',
    ),
    {
      deliveryId,
      productSessionId: 'psn_00000000000000000000000002',
      stageRunId: 'run_00000000000000000000000003',
      evidenceTab: 'evidence',
      evidenceId: null,
    },
  )
})

test('typed StrongFlow routes reject values outside the canonical entity identities', () => {
  assert.deepEqual(
    parseStrongFlowRouteHash(
      '#/strongflow?delivery=../../private&session=%2500'
        + '&stageRun=not%20valid&evidence=%3Cscript%3E&tab=tests',
    ),
    {
      deliveryId: null,
      productSessionId: null,
      stageRunId: null,
      evidenceTab: 'tests',
      evidenceId: null,
    },
  )
})

test('Evidence entry controls preserve keyed identity across equivalent projections', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    limits,
    evidence: pageEvidenceOptions(),
  })
  const classNames = [
    'wwc-strongflow-stage-evidence-open',
    'wwc-strongflow-candidate-evidence-open',
    'wwc-strongflow-criterion-evidence-open',
  ]
  const entries = classNames.map(className => findByClass(rootElement, className))

  model.publish(state())

  for (const [index, className] of classNames.entries()) {
    assert.equal(
      findByClass(rootElement, className),
      entries[index],
      `${className} must retain its keyed DOM identity`,
    )
  }
  mounted.close()
})

test('closing StrongFlow removes listeners from retained Evidence entry controls', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    limits,
    evidence: pageEvidenceOptions(),
  })
  const entries = [
    'wwc-strongflow-stage-evidence-open',
    'wwc-strongflow-candidate-evidence-open',
    'wwc-strongflow-criterion-evidence-open',
  ].map(className => findByClass(rootElement, className))

  mounted.close()

  for (const entry of entries) {
    assert.deepEqual(entry.listeners.get('click') ?? [], [])
  }
})

test('review controls send only current view-model decisions and disable while waiting', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const current = projection()
  current.solutionReview.reviewStatus = 'pending'
  const model = new FakeStrongFlowViewModel(state({ projection: current }))
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    limits,
    evidence: pageEvidenceOptions(),
  })
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

test('StrongFlow keyed updates retain workspace, review drafts, focus, scroll, and large views', () => {
  const document = new FakeDocument()
  document.activeElement = null
  const rootElement = document.createElement('main')
  const current = projection()
  current.solutionReview.reviewStatus = 'pending'
  const model = new FakeStrongFlowViewModel(state({ projection: current }))
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(5, deliverySummary),
    limits,
    evidence: pageEvidenceOptions(),
  })
  const workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  const deliveryList = findByClass(rootElement, 'wwc-strongflow-delivery-list')
  const taskList = findByClass(rootElement, 'wwc-strongflow-task-list')
  const actions = findByClass(rootElement, 'wwc-strongflow-solution-actions')
  const comments = actions.children[0].children[0]
  const attentionGroup = findByClass(rootElement, 'wwc-strongflow-attention-actions')
  const resolution = attentionGroup.children[1].children[0]
  const candidate = findByClass(rootElement, 'wwc-strongflow-view-candidate')
  const diagrams = findByClass(rootElement, 'wwc-strongflow-diagrams')
  const deliveryRow = deliveryList.children[0]
  const taskRow = taskList.children[0]
  const approve = findByClass(rootElement, 'wwc-strongflow-approve-solution')

  comments.value = 'dirty solution review'
  comments.selectionStart = 8
  resolution.value = 'dirty Attention note'
  document.activeElement = comments
  workspace.scrollTop = 108
  deliveryList.scrollTop = 21

  for (let index = 0; index < 200; index += 1) {
    model.publish(state({
      projection: current,
      realtime: index % 2 === 0 ? 'reloading' : 'subscribed',
      interaction: {
        status: index % 2 === 0 ? 'waiting' : 'idle',
        error: null,
      },
    }))
  }

  assert.equal(findByClass(rootElement, 'wwc-strongflow-workspace'), workspace)
  assert.equal(deliveryList.children[0], deliveryRow)
  assert.equal(taskList.children[0], taskRow)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-view-candidate'), candidate)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-diagrams'), diagrams)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-attention-actions'), attentionGroup)
  assert.equal(comments.value, 'dirty solution review')
  assert.equal(comments.selectionStart, 8)
  assert.equal(resolution.value, 'dirty Attention note')
  assert.equal(document.activeElement, comments)
  assert.equal(workspace.scrollTop, 108)
  assert.equal(deliveryList.scrollTop, 21)
  assert.equal(deliveryList.children.length, 2)
  assert.equal(taskList.children.length, 2)
  assert.equal(findAllByClass(rootElement, 'wwc-strongflow-view-candidate').length, 1)

  mounted.close()
  assert.equal((approve.listeners.get('click') ?? []).length, 0)
  assert.equal(comments.value, '')
  assert.equal(resolution.value, '')
  assert.equal(model.listeners.size, 0)
})

test('verdict control stays hidden until all active StageRuns settle', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const current = projection()
  current.verdict = null
  const model = new FakeStrongFlowViewModel(state({ projection: current }))
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    limits,
    evidence: pageEvidenceOptions(),
  })

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
