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
    'apps/client/tsconfig.strongflow-header-tests.json',
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
  `StrongFlow header did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const headerModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-header-tests/strongflow-header.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-header-tests/strongflow-page.js',
)).href}`)

const { strongFlowLayoutMode } = pageModule
const { mountStrongFlowPage } = pageModule

const productSessionId = 'psn_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const workerId = 'wrk_00000000000000000000000001'
const workerSessionId = 'wss_00000000000000000000000001'
const codexThreadId = 'cdx_00000000000000000000000001'
const executionJobId = 'job_00000000000000000000000001'
const leaseId = 'lease_00000000000000000000000001'
const olderRunId = 'run_00000000000000000000000002'
const deliveryId = 'dlv_00000000000000000000000001'

function attachedBinding() {
  return {
    attempt: 2,
    bindingId: 'bind_review_attached',
    boundAt: '2026-09-03T01:00:00.000Z',
    codexThreadId,
    executionJobId,
    fencingToken: 'fencing-review',
    leaseId,
    productSessionId,
    sessionIdentity: {
      codexThreadId,
      productSessionId,
      stageRunId,
      workerSessionId,
    },
    sourceIdentity: {
      kind: 'execution-worker',
      leaseId,
      workerId,
      workerInstanceId: 'wri_00000000000000000000000001',
      workerSessionId,
    },
    stageRunId,
    workerId,
    workerSessionId,
  }
}

function unattachedBinding() {
  return {
    attempt: null,
    bindingId: 'bind_review_unattached',
    boundAt: '2026-09-03T01:00:00.000Z',
    codexThreadId: null,
    executionJobId,
    fencingToken: null,
    leaseId: null,
    productSessionId,
    sessionIdentity: null,
    sourceIdentity: null,
    stageRunId: null,
    workerId: null,
    workerSessionId: null,
  }
}

function stage(overrides = {}) {
  return {
    actorType: 'codex',
    attempt: 2,
    deliveryTaskId: null,
    finishedAt: null,
    id: stageRunId,
    role: 'implementer',
    sessionBinding: attachedBinding(),
    stage: 'executing',
    startedAt: '2026-09-03T00:59:00.000Z',
    status: 'running',
    ...overrides,
  }
}

function candidate() {
  return {
    candidateRef: 'refs/winwincode/candidate/review',
    candidateCommitId: '1'.repeat(40),
    candidateTreeId: '2'.repeat(40),
    deliverySpecId: 'spec:review',
    deliverySpecRevision: 3,
    diffSha256: `sha256:${'3'.repeat(64)}`,
    frozenAt: '2026-09-03T01:00:04.000Z',
    producerSessionBindingId: 'bind_review_attached',
    producerStageRunId: stageRunId,
  }
}

function delivery(overrides = {}) {
  return {
    attention: [],
    currentCandidate: candidate(),
    deliveryId,
    deliveryRevision: 4,
    evidence: [],
    kind: 'delivery_detail',
    ownership: {
      organizationId: 'org_00000000000000000000000001',
      workspaceId: 'wsp_00000000000000000000000001',
      projectId: 'prj_00000000000000000000000001',
      repositoryId: 'rep_00000000000000000000000001',
    },
    publication: null,
    readCursor: {},
    requirements: {
      title: 'UI-306 re-verification matrix',
      goal: 'Exactly one polite status in every state.',
    },
    schemaVersion: 'winwincode/v1',
    solutionReview: null,
    stages: [stage()],
    status: 'executing',
    tasks: [],
    verdict: null,
    ...overrides,
  }
}

function runtime() {
  return {
    deliveryId,
    eventCursor: { eventId: 'evt_1', sequence: 3, stream: { deliveryId, kind: 'delivery' } },
    kind: 'runtime_projection',
    lastProjectionSequence: 3,
    productSessionId,
    readCursor: {},
    rebuiltAt: '2026-09-03T01:00:05.000Z',
    revision: 8,
    sessions: [],
    stageRunId,
  }
}

function projection(overrides = {}) {
  const base = overrides.delivery ?? delivery()
  return {
    delivery: base,
    currentCandidate: base.currentCandidate,
    evidence: base.evidence,
    attention: base.attention,
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-09-03T01:00:06.000Z',
      readCursor: {},
      revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 1 },
    },
    publication: base.publication,
    runtime: runtime(),
    solutionReview: base.solutionReview,
    diagramExecution: null,
    stage: base.stages[0],
    verdict: base.verdict,
    ...overrides,
  }
}

function candidateFiles() {
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

function state(overrides = {}) {
  return {
    error: null,
    candidateFiles: candidateFiles(),
    interaction: { status: 'idle', error: null },
    projection: projection(),
    realtime: 'subscribed',
    status: 'ready',
    ...overrides,
  }
}

function clientError(kind, code) {
  return {
    kind,
    code,
    message: code,
    requestId: null,
    retryable: false,
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
    this.listeners.set(name, (this.listeners.get(name) ?? []).filter(
      candidate => candidate !== listener,
    ))
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

function textOf(node) {
  if (node.children.length === 0) return node.textContent
  return node.children.map(textOf).join(' ')
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

function mountedPageModel(initial) {
  return {
    state: initial,
    calls: [],
    listener: null,
    draftScope: 'scope:ui306',
    subscribe(listener) {
      this.listener = listener
      listener(this.state)
      return () => { this.listener = null }
    },
    async start() { this.calls.push(['start']) },
    async refresh() { this.calls.push(['refresh']) },
    async decideSolutionReview() {},
    async approveTaskBreakdown() {},
    async resolveAttention() {},
    async submitVerdict() {},
    async advanceDelivery() {},
    async loadStageRunRuntime() { return null },
    async loadCandidateFiles() { this.calls.push(['loadCandidateFiles']) },
    async loadMoreCandidateFiles() { this.calls.push(['loadMoreCandidateFiles']) },
    async selectCandidateFile() { this.calls.push(['selectCandidateFile']) },
    async loadMoreCandidateDiff() { this.calls.push(['loadMoreCandidateDiff']) },
    async loadStageRunCandidates() { return [] },
    async loadCandidateHistoricalReview() { return null },
    cancelPending() {},
    reconnect() {},
    close() { this.calls.push(['close']) },
    publish(next) {
      this.state = next
      this.listener?.(next)
    },
  }
}

function inAccessibilityTree(node) {
  for (let current = node; current !== null; current = current.parentNode) {
    if (current.hidden === true) return false
  }
  return true
}

// The mounted Delivery workspace owns its own list-state live regions
// (.wwc-delivery-feedback, .wwc-delivery-empty); they announce list state, not
// the StrongFlow next-step status, so they stay outside this page-chrome count.
const DELIVERY_WORKSPACE_LIVE_REGIONS = new Set([
  'wwc-delivery-feedback',
  'wwc-delivery-empty',
])

function collectPolite(node, matches = []) {
  const isPolite = node.getAttribute('role') === 'status'
    || node.getAttribute('aria-live') === 'polite'
  if (
    isPolite
    && !DELIVERY_WORKSPACE_LIVE_REGIONS.has(node.className)
    && textOf(node).trim().length > 0
    && inAccessibilityTree(node)
  ) {
    matches.push(node)
  }
  for (const child of node.children) collectPolite(child, matches)
  return matches
}

function mountAt(initial) {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = mountedPageModel(initial)
  const mounted = mountStrongFlowPage({ root: rootElement, model, deliveryList: fakeDeliveryList([]) })
  return { document, rootElement, model, mounted }
}

const hiddenHeaderStates = [
  ['loading', state({ status: 'loading', projection: null, realtime: 'inactive' })],
  ['authentication-required', state({
    status: 'authentication-required',
    projection: null,
    realtime: 'access-revoked',
    interaction: { status: 'error', error: clientError('authentication', 'AUTHENTICATION_REQUIRED') },
    error: clientError('authentication', 'AUTHENTICATION_REQUIRED'),
  })],
  ['authorization-denied', state({
    status: 'authorization-denied',
    projection: null,
    realtime: 'access-revoked',
    interaction: { status: 'error', error: clientError('authorization', 'AUTHORIZATION_DENIED') },
    error: clientError('authorization', 'AUTHORIZATION_DENIED'),
  })],
  ['cancelled', state({
    status: 'cancelled',
    projection: null,
    realtime: 'inactive',
    interaction: { status: 'error', error: clientError('cancelled', 'REQUEST_CANCELLED') },
    error: clientError('cancelled', 'REQUEST_CANCELLED'),
  })],
  ['error', state({
    status: 'error',
    projection: null,
    realtime: 'reconnecting',
    interaction: { status: 'error', error: clientError('network', 'NETWORK_UNREACHABLE') },
    error: clientError('network', 'NETWORK_UNREACHABLE'),
  })],
  ['closed', state({ status: 'closed', projection: null, realtime: 'closed' })],
  ['no delivery selected', state({ projection: null })],
]

const visibleHeaderStates = [
  ['idle with projection', state()],
  ['submitting with projection', state({
    interaction: { status: 'submitting', error: null },
  })],
  ['waiting with projection', state({
    interaction: { status: 'waiting', error: null },
  })],
  ['refreshing retains projection', state({ status: 'refreshing', realtime: 'reloading' })],
  ['reconnecting retains projection', state({ realtime: 'reconnecting' })],
]

test('exactly one accessible polite status exists in every header-visible page state', () => {
  const violations = []
  for (const [name, pageState] of visibleHeaderStates) {
    const { rootElement, model, mounted } = mountAt(pageState)
    model.publish(pageState)
    const polite = collectPolite(rootElement)
    if (polite.length !== 1) {
      violations.push(`${name}: ${String(polite.length)} polite statuses ${JSON.stringify(polite.map(textOf))}`)
    }
    mounted.close()
  }
  assert.equal(
    violations.length,
    0,
    `header-visible states must keep exactly one non-empty accessible polite status:\n${violations.join('\n')}`,
  )
})

test('exactly one accessible polite status exists in every header-hidden page state', () => {
  const violations = []
  for (const [name, pageState] of hiddenHeaderStates) {
    const { rootElement, model, mounted } = mountAt(pageState)
    model.publish(pageState)
    const header = findByClass(rootElement, 'wwc-strongflow-header')
    assert.notEqual(header, null, `${name}: header element missing`)
    assert.equal(header.hidden, true, `${name}: header must hide without a projection`)
    const polite = collectPolite(rootElement)
    if (polite.length !== 1) {
      violations.push(`${name}: ${String(polite.length)} polite statuses ${JSON.stringify(polite.map(textOf))}`)
    }
    mounted.close()
  }
  assert.equal(
    violations.length,
    0,
    `header-hidden states must keep exactly one non-empty accessible polite status:\n${violations.join('\n')}`,
  )
})

test('the header status never repeats the legacy status line text', () => {
  for (const [, pageState] of visibleHeaderStates) {
    const { rootElement, model, mounted } = mountAt(pageState)
    model.publish(pageState)
    const legacy = findByClass(rootElement, 'wwc-strongflow-status')
    const header = findByClass(rootElement, 'wwc-strongflow-header-status')
    if (legacy !== null && header !== null
      && textOf(legacy).trim().length > 0 && inAccessibilityTree(legacy)) {
      assert.notEqual(
        textOf(legacy),
        textOf(header),
        'the same status text is rendered by two live regions',
      )
    }
    mounted.close()
  }
})

test('a historical review keeps one polite status and the card marked current', async () => {
  const olderStage = stage({
    id: olderRunId,
    role: 'verifier',
    status: 'failed',
    sessionBinding: unattachedBinding(),
  })
  const currentStage = stage()
  const historical = projection({
    delivery: delivery({ stages: [olderStage, currentStage] }),
    stage: currentStage,
  })
  const pageState = state({ projection: historical })
  const { rootElement, model, mounted } = mountAt(pageState)
  const historicalButton = findAllByClass(rootElement, 'wwc-strongflow-run-button')
    .find(node => node.dataset.stageRunId === olderRunId)
  assert.notEqual(historicalButton, undefined)
  historicalButton.emit('click')
  model.publish(state({ projection: historical }))
  await new Promise(resolve => setImmediate(resolve))
  await new Promise(resolve => setImmediate(resolve))

  const polite = collectPolite(rootElement)
  assert.equal(
    polite.length,
    1,
    `settled historical review leaves ${String(polite.length)} polite statuses: ${JSON.stringify(polite.map(textOf))}`,
  )

  const header = findByClass(rootElement, 'wwc-strongflow-header')
  assert.match(textOf(header), /current/iu)
  const identityList = findByClass(header, 'wwc-strongflow-identity-list')
  const stageRunRow = identityList.children.find(
    row => row.children[0]?.textContent === 'StageRun',
  )
  assert.equal(
    stageRunRow?.children[1]?.textContent,
    stageRunId,
    'the identity card must keep reporting the canonical current StageRun',
  )
  mounted.close()
})

test('identity collapse and keyed updates survive repeated publishes', () => {
  const { rootElement, model, mounted } = mountAt(state())
  const toggle = findByClass(rootElement, 'wwc-strongflow-identity-toggle')
  const list = findByClass(rootElement, 'wwc-strongflow-identity-list')
  assert.equal(list.hidden, true)
  toggle.emit('click')
  assert.equal(toggle.getAttribute('aria-expanded'), 'true')
  assert.equal(list.hidden, false)
  assert.equal(toggle.getAttribute('aria-controls'), list.id)

  const statusNode = findByClass(rootElement, 'wwc-strongflow-header-status')
  model.publish(state())
  model.publish(state({
    projection: projection({ delivery: delivery({ status: 'ready-to-deliver' }) }),
  }))
  assert.equal(findByClass(rootElement, 'wwc-strongflow-header-status'), statusNode)
  assert.match(textOf(statusNode), /Waiting for your approval/u)
  mounted.close()
})

test('the narrow viewport boundary stays deterministic', () => {
  assert.equal(strongFlowLayoutMode(1023), 'narrow')
  assert.equal(strongFlowLayoutMode(1024), 'narrow')
  assert.equal(strongFlowLayoutMode(1025), 'wide')
})
