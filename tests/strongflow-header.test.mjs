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

const {
  STRONGFLOW_IDENTITY_NOT_REPORTED,
  mountStrongFlowHeader,
  strongFlowConnectionLabel,
  strongFlowExecutionIdentity,
  strongFlowIdentityRows,
  strongFlowNextStep,
} = headerModule
const { mountStrongFlowPage, strongFlowPagePresentation } = pageModule

const deliveryId = 'dlv_00000000000000000000000001'
const productSessionId = 'psn_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const workerId = 'wrk_00000000000000000000000001'
const workerSessionId = 'wss_00000000000000000000000001'
const codexThreadId = 'cdx_00000000000000000000000001'
const executionJobId = 'job_00000000000000000000000001'
const leaseId = 'lease_00000000000000000000000001'
const olderRunId = 'run_00000000000000000000000002'

function attachedBinding() {
  return {
    attempt: 2,
    bindingId: 'bind_ui306_attached',
    boundAt: '2026-09-03T01:00:00.000Z',
    codexThreadId,
    executionJobId,
    fencingToken: 'fencing-ui306',
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
    bindingId: 'bind_ui306_unattached',
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
    candidateRef: 'refs/winwincode/candidate/ui306',
    candidateCommitId: '1'.repeat(40),
    candidateTreeId: '2'.repeat(40),
    deliverySpecId: 'spec:ui306',
    deliverySpecRevision: 3,
    diffSha256: `sha256:${'3'.repeat(64)}`,
    frozenAt: '2026-09-03T01:00:04.000Z',
    producerSessionBindingId: 'bind_ui306_attached',
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
      title: 'UI-306 next-action header',
      goal: 'Answer what happens now and what to do next.',
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
    stage: base.stages[0],
    verdict: base.verdict,
    ...overrides,
  }
}

function state(overrides = {}) {
  return {
    error: null,
    interaction: { status: 'idle', error: null },
    projection: projection(),
    realtime: 'subscribed',
    status: 'ready',
    ...overrides,
  }
}

function identityValue(rows, term) {
  const row = rows.find(candidate => candidate.term === term)
  assert.notEqual(row, undefined, `identity row missing: ${term}`)
  return row.value
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
    const listeners = this.listeners.get(name) ?? []
    this.listeners.set(name, listeners.filter(candidate => candidate !== listener))
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

function mountedPageModel(initial) {
  return {
    state: initial,
    calls: [],
    listener: null,
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

test('each Delivery situation answers with a distinct human status, reason, and next step', () => {
  const active = strongFlowNextStep(projection())
  const verifying = strongFlowNextStep(projection({
    delivery: delivery({
      stages: [stage({ stage: 'verifying', role: 'verifier', status: 'running' })],
      status: 'verifying',
    }),
    stage: stage({ stage: 'verifying', role: 'verifier', status: 'running' }),
  }))
  const waitingApproval = strongFlowNextStep(projection({
    delivery: delivery({ status: 'plan-review' }),
    solutionReview: {
      architectureDiagram: { nodes: [], edges: [] },
      processDiagram: { nodes: [], edges: [] },
      reviewStatus: 'pending',
    },
    stage: stage({
      actorType: 'human',
      id: olderRunId,
      role: 'reviewer',
      sessionBinding: null,
      stage: 'plan-review',
      status: 'waiting',
    }),
  }))
  const waitingInput = strongFlowNextStep(projection({
    delivery: delivery({
      attention: [{
        blocking: true,
        createdAt: '2026-09-03T01:00:00.000Z',
        deliverySpecId: 'spec:ui306',
        id: 'attention:1',
        options: [],
        resolutionSummary: null,
        resolvedAt: null,
        resolvedBy: null,
        stageRunId: null,
        status: 'open',
        title: 'Which repository should receive the result?',
        type: 'decision_required',
      }],
    }),
  }))
  const failed = strongFlowNextStep(projection({
    delivery: delivery({
      stages: [stage({ status: 'failed', finishedAt: '2026-09-03T01:05:00.000Z' })],
    }),
    stage: stage({ status: 'failed', finishedAt: '2026-09-03T01:05:00.000Z' }),
  }))
  const completed = strongFlowNextStep(projection({
    delivery: delivery({
      publication: {
        approvalAttentionItemId: 'attention:approval',
        approvedAt: '2026-09-03T02:00:00.000Z',
        approvedBy: 'usr_00000000000000000000000001',
        candidateRef: 'refs/winwincode/candidate/ui306',
        deliveryId,
        deliverySpecId: 'spec:ui306',
        deliverySpecRevision: 3,
        deliveryVerdictId: 'verdict:1',
        id: 'pub:1',
        publicationSetSha256: `sha256:${'4'.repeat(64)}`,
        resourceRef: null,
        revision: 1,
        state: 'published',
        target: { kind: 'github', repository: 'org/repo' },
        updatedAt: '2026-09-03T02:00:01.000Z',
        verdictStatus: 'pass',
      },
      status: 'delivered',
    }),
  }))

  const situations = [
    ['active', active],
    ['verifying', verifying],
    ['waiting-approval', waitingApproval],
    ['waiting-input', waitingInput],
    ['failed', failed],
    ['completed', completed],
  ]
  assert.equal(new Set(situations.map(([, item]) => item.category)).size, 6)
  assert.equal(new Set(situations.map(([, item]) => item.statusLabel)).size, 6)
  assert.equal(new Set(situations.map(([, item]) => item.nextStep)).size, 6)
  assert.equal(active.category, 'active')
  assert.equal(verifying.category, 'verifying')
  assert.equal(waitingApproval.category, 'waiting-approval')
  assert.equal(waitingInput.category, 'waiting-input')
  assert.equal(failed.category, 'failed')
  assert.equal(completed.category, 'completed')
  assert.equal(waitingApproval.reason !== null, true)
  assert.equal(waitingInput.reason !== null, true)
  assert.equal(failed.reason !== null, true)
  assert.equal(completed.reason !== null, true)
})

test('blocked reasons and next steps name the exact blocking fact instead of an error code', () => {
  const blocked = strongFlowNextStep(projection({
    delivery: delivery({
      attention: [{
        blocking: true,
        createdAt: '2026-09-03T01:00:00.000Z',
        deliverySpecId: 'spec:ui306',
        id: 'attention:1',
        options: [],
        resolutionSummary: null,
        resolvedAt: null,
        resolvedBy: null,
        stageRunId: null,
        status: 'open',
        title: 'Which repository should receive the result?',
        type: 'decision_required',
      }],
    }),
  }))
  assert.match(blocked.reason, /Which repository should receive the result\?/u)
  assert.match(blocked.nextStep, /Attention/u)
  assert.doesNotMatch(`${blocked.statusLabel} ${blocked.nextStep}`, /decision_required/u)

  const failedRun = strongFlowNextStep(projection({
    delivery: delivery({ stages: [stage({ status: 'failed' })] }),
    stage: stage({ status: 'failed' }),
  }))
  assert.match(failedRun.reason, /implementer/u)
  assert.match(failedRun.nextStep, /failure/u)

  const failedVerdict = strongFlowNextStep(projection({
    delivery: delivery({
      verdict: {
        candidateRef: 'refs/winwincode/candidate/ui306',
        criteria: [],
        deliverySpecId: 'spec:ui306',
        deliverySpecRevision: 3,
        id: 'verdict:1',
        producedAt: '2026-09-03T01:30:00.000Z',
        status: 'fail',
        unresolvedFindings: ['Acceptance 1 not met'],
      },
    }),
  }))
  assert.match(failedVerdict.reason, /1 unresolved finding/u)
  assert.match(failedVerdict.nextStep, /finding/u)

  const finalApproval = strongFlowNextStep(projection({
    delivery: delivery({ status: 'ready-to-deliver' }),
  }))
  assert.equal(finalApproval.category, 'waiting-approval')
  assert.match(finalApproval.nextStep, /final Delivery/u)

  const clarifying = strongFlowNextStep(projection({
    delivery: delivery({ status: 'clarifying' }),
  }))
  assert.equal(clarifying.category, 'waiting-input')
  assert.match(clarifying.nextStep, /answer|question|reply/iu)
})

test('the current run summary comes only from the canonical active StageRun', () => {
  const current = projection()
  current.delivery.stages = [
    stage({
      id: olderRunId,
      role: 'verifier',
      status: 'failed',
      sessionBinding: unattachedBinding(),
    }),
    stage(),
  ]
  const next = strongFlowNextStep(current)
  assert.equal(next.currentRun.attempt, 2)
  assert.equal(next.currentRun.role, 'implementer')
  assert.equal(next.currentRun.status, 'running')
  assert.equal(next.currentRun.phase, 'executing')
  const identity = strongFlowExecutionIdentity(current, 'subscribed')
  assert.equal(identity.stageRunId, stageRunId)
  assert.equal(identity.workerId, workerId)
  assert.equal(identity.attempt, 2)
  assert.notEqual(identity.stageRunId, olderRunId)
})

test('execution identity distinguishes every identity kind from exact binding facts', () => {
  const rows = strongFlowIdentityRows(strongFlowExecutionIdentity(projection(), 'subscribed'))
  assert.equal(identityValue(rows, 'ProductSession'), productSessionId)
  assert.equal(identityValue(rows, 'StageRun'), stageRunId)
  assert.equal(identityValue(rows, 'Attempt'), '2')
  assert.equal(identityValue(rows, 'ExecutionJob'), executionJobId)
  assert.equal(identityValue(rows, 'Worker'), workerId)
  assert.equal(identityValue(rows, 'WorkerSession'), workerSessionId)
  assert.equal(identityValue(rows, 'CodexThread'), codexThreadId)
  assert.equal(identityValue(rows, 'Model route'), STRONGFLOW_IDENTITY_NOT_REPORTED)
  assert.match(identityValue(rows, 'Candidate'), /refs\/winwincode\/candidate\/ui306/u)
  assert.equal(identityValue(rows, 'Lease'), leaseId)
  assert.equal(identityValue(rows, 'Events connection'), 'Live events connected')
  const raw = strongFlowExecutionIdentity(projection(), 'subscribed')
  assert.equal(raw.modelRoute, null)
  assert.equal(raw.leaseHeld, true)
})

test('absent binding facts stay explicitly unreported instead of guessed', () => {
  const detached = projection({
    stage: stage({
      attempt: 1,
      sessionBinding: unattachedBinding(),
      status: 'waiting',
    }),
  })
  const rows = strongFlowIdentityRows(strongFlowExecutionIdentity(detached, 'reconnecting'))
  assert.equal(identityValue(rows, 'ProductSession'), productSessionId)
  assert.equal(identityValue(rows, 'Worker'), STRONGFLOW_IDENTITY_NOT_REPORTED)
  assert.equal(identityValue(rows, 'WorkerSession'), STRONGFLOW_IDENTITY_NOT_REPORTED)
  assert.equal(identityValue(rows, 'CodexThread'), STRONGFLOW_IDENTITY_NOT_REPORTED)
  assert.equal(identityValue(rows, 'Lease'), STRONGFLOW_IDENTITY_NOT_REPORTED)
  assert.equal(identityValue(rows, 'Events connection'), 'Reconnecting…')
  const raw = strongFlowExecutionIdentity(detached, 'reconnecting')
  assert.equal(raw.leaseHeld, false)
  assert.equal(raw.modelRoute, null)

  const human = projection({
    stage: stage({
      actorType: 'human',
      attempt: 1,
      id: olderRunId,
      role: 'reviewer',
      sessionBinding: null,
      stage: 'plan-review',
      status: 'waiting',
    }),
  })
  const humanRows = strongFlowIdentityRows(strongFlowExecutionIdentity(human, 'subscribed'))
  assert.equal(identityValue(humanRows, 'ProductSession'), STRONGFLOW_IDENTITY_NOT_REPORTED)
  assert.equal(identityValue(humanRows, 'StageRun'), olderRunId)
  assert.equal(identityValue(humanRows, 'Worker'), STRONGFLOW_IDENTITY_NOT_REPORTED)

  const noCandidate = projection({
    delivery: delivery({ currentCandidate: null }),
    currentCandidate: null,
  })
  const candidateRows = strongFlowIdentityRows(
    strongFlowExecutionIdentity(noCandidate, 'subscribed'),
  )
  assert.equal(identityValue(candidateRows, 'Candidate'), 'None frozen yet')
})

test('connection labels stay human readable for every realtime state', () => {
  assert.equal(strongFlowConnectionLabel('subscribed'), 'Live events connected')
  assert.equal(strongFlowConnectionLabel('reloading'), 'Refreshing events…')
  assert.equal(strongFlowConnectionLabel('reconnecting'), 'Reconnecting…')
  assert.equal(strongFlowConnectionLabel('inactive'), 'Not connected')
  assert.equal(strongFlowConnectionLabel('access-revoked'), 'Access revoked')
  assert.equal(strongFlowConnectionLabel('closed'), 'Closed')
})

test('the header answers with human text first and keeps technical identity collapsible', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowHeader({ document })
  view.update(state())

  assert.equal(view.root.hidden, false)
  const status = findByClass(view.root, 'wwc-strongflow-header-status')
  const reason = findByClass(view.root, 'wwc-strongflow-header-reason')
  const next = findByClass(view.root, 'wwc-strongflow-header-next')
  assert.equal(status.getAttribute('role'), 'status')
  assert.equal(status.getAttribute('aria-live'), 'polite')
  assert.match(status.textContent, /In progress/u)
  assert.match(reason.textContent, /implementer/u)
  assert.match(next.textContent, /Next step:/u)
  assert.doesNotMatch(status.textContent, /dlv_|psn_|run_|cdx_|wrk_|wss_|lease_/u)
  assert.doesNotMatch(next.textContent, /dlv_|psn_|run_|cdx_/u)

  const list = findByClass(view.root, 'wwc-strongflow-identity-list')
  const toggle = findByClass(view.root, 'wwc-strongflow-identity-toggle')
  assert.equal(toggle.getAttribute('aria-expanded'), 'false')
  assert.equal(list.hidden, true)
  toggle.emit('click')
  assert.equal(toggle.getAttribute('aria-expanded'), 'true')
  assert.equal(list.hidden, false)
  assert.equal(toggle.getAttribute('aria-controls'), list.id)
  const terms = list.children
    .filter(row => row.tagName === 'DIV')
    .map(row => row.children[0].textContent)
  for (const term of [
    'ProductSession',
    'WorkerSession',
    'CodexThread',
    'StageRun',
    'Attempt',
    'Worker',
    'Model route',
    'Candidate',
    'Lease',
    'Events connection',
  ]) {
    assert.equal(terms.includes(term), true, `identity card missing term: ${term}`)
  }

  const firstStatusNode = status
  const firstListNode = list
  view.update(state())
  assert.equal(findByClass(view.root, 'wwc-strongflow-header-status'), firstStatusNode)
  assert.equal(findByClass(view.root, 'wwc-strongflow-identity-list'), firstListNode)

  toggle.emit('click')
  assert.equal(toggle.getAttribute('aria-expanded'), 'false')
  assert.equal(list.hidden, true)

  view.close()
  assert.deepEqual(view.root.children, [])
})

test('the header hides and reports nothing when the exact snapshot is unavailable', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowHeader({ document })
  view.update(state())
  assert.equal(view.root.hidden, false)
  view.update(state({
    error: {
      kind: 'authorization',
      code: 'AUTHORIZATION_DENIED',
      message: 'denied',
      requestId: null,
      retryable: false,
    },
    projection: null,
    realtime: 'access-revoked',
    status: 'authorization-denied',
  }))
  assert.equal(view.root.hidden, true)
  const list = findByClass(view.root, 'wwc-strongflow-identity-list')
  assert.equal(list.hidden, true)
  view.close()
})

test('the mounted page keeps the human header above the workspace and presentation text human', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = {
    state: state(),
    calls: [],
    listener: null,
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
    async loadStageRunCandidates() { return [] },
    async loadCandidateHistoricalReview() { return null },
    cancelPending() {},
    reconnect() {},
    close() { this.calls.push(['close']) },
  }
  const mounted = mountStrongFlowPage({ root: rootElement, model })
  const header = findByClass(rootElement, 'wwc-strongflow-header')
  assert.notEqual(header, null)
  assert.equal(header.hidden, false)
  const status = findByClass(header, 'wwc-strongflow-header-status')
  assert.match(status.textContent, /In progress/u)
  assert.doesNotMatch(status.textContent, /dlv_|executing/u)

  const presentation = strongFlowPagePresentation(state())
  assert.equal(presentation.statusText, '')
  assert.equal(strongFlowPagePresentation(state({
    projection: null,
    status: 'loading',
  })).statusText, 'Loading StrongFlow…')

  mounted.close()
  assert.deepEqual(model.calls.at(-1), ['close'])
  assert.deepEqual(rootElement.children, [])
})

test('the next-step status is announced by a single polite live region, not duplicated', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = mountedPageModel(state())
  const mounted = mountStrongFlowPage({ root: rootElement, model })
  const legacyStatus = findByClass(rootElement, 'wwc-strongflow-status')
  const headerStatus = findByClass(rootElement, 'wwc-strongflow-header-status')
  const liveRegions = [legacyStatus, headerStatus].filter(node => (
    node.getAttribute('role') === 'status'
    && node.getAttribute('aria-live') === 'polite'
    && node.textContent.length > 0
  ))
  assert.equal(
    liveRegions.length,
    1,
    `the same next-step status is announced by ${String(liveRegions.length)} polite live regions`,
  )
  mounted.close()
})

test('an open historical review is not mistaken for the unmarked header identity card', () => {
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
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = mountedPageModel(state({ projection: historical }))
  const mounted = mountStrongFlowPage({ root: rootElement, model })
  const historicalButton = findAllByClass(rootElement, 'wwc-strongflow-run-button')
    .find(node => node.dataset.stageRunId === olderRunId)
  assert.notEqual(historicalButton, undefined)
  historicalButton.emit('click')
  model.publish(state({ projection: historical }))
  const header = findByClass(rootElement, 'wwc-strongflow-header')
  const identityList = findByClass(header, 'wwc-strongflow-identity-list')
  const stageRunRow = identityList.children.find(
    row => row.children[0]?.textContent === 'StageRun',
  )
  const marksCurrent = /current/iu.test(textOf(header))
  const showsReviewedRun = stageRunRow?.children[1]?.textContent === olderRunId
  assert.ok(
    marksCurrent || showsReviewedRun,
    'the header identity card reports the current run without any visible current marker '
      + 'while a historical StageRun review is open',
  )
  mounted.close()
})
