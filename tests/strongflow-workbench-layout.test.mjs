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

const page = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-page.js',
)).href}`)
const preferencesModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-layout-preferences.js',
)).href}`)

const { mountStrongFlowPage } = page
const { DEFAULT_STRONGFLOW_LAYOUT, normalizeStrongFlowLayoutPreferences } = preferencesModule

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
        productSessionId: `psn_${String(sessionValue).padStart(26, '0')}`,
        stageRunId,
        sessionBindingId: `bind:${String(sessionValue)}`,
        codexThreadId: `cdx_t${String(sessionValue).padStart(25, '0')}`,
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
        usage: null,
        plan: null,
        recovery: {
          failureCount: 0,
          lastFailureSourceRef: null,
          latestRecoverySourceRef: null,
          recoveryCount: 0,
          state: 'none',
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
    candidateFiles: {
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
    },
    interaction: { status: 'idle', error: null },
    error: null,
    ...overrides,
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
    const listeners = (this.listeners.get(name) ?? []).filter(candidate => candidate !== listener)
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
  elements = []

  createElement(tagName) {
    const element = new FakeElement(this, tagName)
    this.elements.push(element)
    return element
  }

  getElementById(id) {
    return this.elements.find(element => element.id === id) ?? null
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

function findFirst(node, predicate) {
  if (predicate(node)) return node
  for (const child of node.children) {
    const match = findFirst(child, predicate)
    if (match !== null) return match
  }
  return null
}

function findByRole(node, role) {
  return findFirst(node, candidate => candidate.getAttribute?.('role') === role)
}

class FakeStrongFlowViewModel {
  constructor(initialState) {
    this.state = initialState
  }

  calls = []
  draftScope = '["strongflow-workbench-test-actor","strongflow-workbench-test-scope"]'
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
  cancelPending() { this.calls.push(['cancelPending']) }
  reconnect() { this.calls.push(['reconnect']) }
  close() { this.calls.push(['close']) }
}

class FakeStorage {
  #entries = new Map()

  getItem(key) {
    return this.#entries.get(key) ?? null
  }

  setItem(key, value) {
    this.#entries.set(key, String(value))
  }

  removeItem(key) {
    this.#entries.delete(key)
  }

  snapshot() {
    return Object.fromEntries(this.#entries)
  }
}

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

test('desktop workbench renders navigation, main, context, and artifact landmarks side by side', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const storage = new FakeStorage()
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(5, deliverySummary),
    limits,
    storage,
  })

  const workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  assert.notEqual(workspace, null)
  assert.equal(workspace.tagName, 'SECTION')
  assert.equal(flatten(rootElement).filter(node => node.tagName === 'MAIN').length, 1)
  const navigation = findByClass(rootElement, 'wwc-strongflow-navigation')
  const mainRegion = findByClass(rootElement, 'wwc-strongflow-main-region')
  const context = findByClass(rootElement, 'wwc-strongflow-context')
  assert.notEqual(navigation, null)
  assert.notEqual(mainRegion, null)
  assert.notEqual(context, null)
  assert.equal(navigation.tagName, 'NAV')
  assert.equal(navigation.getAttribute('aria-label'), 'Delivery and Task navigation')
  assert.equal(mainRegion.tagName, 'SECTION')
  assert.equal(mainRegion.getAttribute('aria-label'), 'Delivery main content')
  assert.equal(context.tagName, 'ASIDE')
  assert.equal(context.getAttribute('aria-label'), 'Attention and Evidence context')

  // All four review surfaces stay reachable in one desktop viewport.
  assert.notEqual(findByClass(navigation, 'wwc-strongflow-delivery-list'), null)
  assert.notEqual(findByClass(navigation, 'wwc-strongflow-task-list'), null)
  assert.notEqual(findByClass(mainRegion, 'wwc-strongflow-actions'), null)
  assert.notEqual(findByClass(context, 'wwc-strongflow-attention-list'), null)

  const tree = flatten(workspace)
  const landmarkOrder = [navigation, mainRegion, context].map(region => tree.indexOf(region))
  assert.deepEqual([...landmarkOrder].sort((left, right) => left - right), landmarkOrder)
  mounted.close()
})

test('keyboard order follows navigation, main content, context, then artifacts', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(2, deliverySummary),
    limits,
    storage: new FakeStorage(),
  })

  const workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  const order = [
    findByClass(workspace, 'wwc-strongflow-navigation'),
    findByClass(workspace, 'wwc-strongflow-main-region'),
    findByClass(workspace, 'wwc-strongflow-context'),
    findByClass(workspace, 'wwc-strongflow-artifacts'),
  ]
  const tree = flatten(workspace)
  const positions = order.map(region => region === null ? -1 : tree.indexOf(region))
  for (const position of positions) assert.ok(position >= 0, 'each landmark is in the workspace')
  assert.deepEqual([...positions].sort((left, right) => left - right), positions)
  mounted.close()
})

test('pane resize handles update clamped browser preferences and persist them', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const storage = new FakeStorage()
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(2, deliverySummary),
    limits,
    storage,
  })

  const navigationHandle = findByClass(rootElement, 'wwc-strongflow-resize-navigation')
  const contextHandle = findByClass(rootElement, 'wwc-strongflow-resize-context')
  assert.notEqual(navigationHandle, null)
  assert.notEqual(contextHandle, null)
  assert.equal(navigationHandle.getAttribute('role'), 'separator')
  assert.equal(navigationHandle.getAttribute('aria-orientation'), 'vertical')
  assert.match(navigationHandle.getAttribute('aria-label'), /navigation width/iu)
  assert.equal(navigationHandle.tabIndex, 0)

  navigationHandle.emit('keydown', { key: 'ArrowRight', preventDefault() {} })
  let workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  assert.equal(
    workspace.getAttribute('data-navigation-width'),
    String(DEFAULT_STRONGFLOW_LAYOUT.navigationWidth + 2),
  )
  assert.equal(
    workspace.getAttribute('data-context-width'),
    String(DEFAULT_STRONGFLOW_LAYOUT.contextWidth),
  )
  assert.deepEqual(
    JSON.parse(storage.snapshot()['winwincode.strongflow.layout.v1']),
    normalizeStrongFlowLayoutPreferences({
      navigationWidth: DEFAULT_STRONGFLOW_LAYOUT.navigationWidth + 2,
    }),
  )

  for (let index = 0; index < 40; index += 1) {
    navigationHandle.emit('keydown', { key: 'ArrowRight', preventDefault() {} })
  }
  workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  assert.equal(workspace.getAttribute('data-navigation-width'), '45')

  contextHandle.emit('keydown', { key: 'ArrowLeft', preventDefault() {} })
  workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  assert.equal(
    workspace.getAttribute('data-context-width'),
    String(DEFAULT_STRONGFLOW_LAYOUT.contextWidth - 2),
  )
  mounted.close()
})

test('pointer dragging resizes panes against the rendered workspace width', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const storage = new FakeStorage()
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(2, deliverySummary),
    limits,
    storage,
  })

  const workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  const navigationHandle = findByClass(rootElement, 'wwc-strongflow-resize-navigation')
  const contextHandle = findByClass(rootElement, 'wwc-strongflow-resize-context')
  workspace.getBoundingClientRect = () => ({ width: 1_000 })

  navigationHandle.emit('pointerdown', {
    button: 0,
    clientX: 220,
    pointerId: 7,
    preventDefault() {},
  })
  navigationHandle.emit('pointermove', {
    clientX: 270,
    pointerId: 7,
    preventDefault() {},
  })
  navigationHandle.emit('pointerup', { pointerId: 7 })
  assert.equal(workspace.getAttribute('data-navigation-width'), '27')

  contextHandle.emit('pointerdown', {
    button: 0,
    clientX: 800,
    pointerId: 8,
    preventDefault() {},
  })
  contextHandle.emit('pointermove', {
    clientX: 850,
    pointerId: 8,
    preventDefault() {},
  })
  contextHandle.emit('pointerup', { pointerId: 8 })
  assert.equal(workspace.getAttribute('data-context-width'), '25')
  assert.deepEqual(
    JSON.parse(storage.snapshot()['winwincode.strongflow.layout.v1']),
    normalizeStrongFlowLayoutPreferences({
      navigationWidth: 27,
      contextWidth: 25,
    }),
  )
  mounted.close()
})

test('collapse toggles hide one pane and persist the collapsed preference', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const storage = new FakeStorage()
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(2, deliverySummary),
    limits,
    storage,
  })

  const collapseNavigation = findByClass(rootElement, 'wwc-strongflow-collapse-navigation')
  const collapseContext = findByClass(rootElement, 'wwc-strongflow-collapse-context')
  assert.notEqual(collapseNavigation, null)
  assert.notEqual(collapseContext, null)

  collapseNavigation.emit('click')
  let workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  assert.equal(workspace.getAttribute('data-navigation-collapsed'), 'true')
  assert.equal(
    JSON.parse(storage.snapshot()['winwincode.strongflow.layout.v1']).navigationCollapsed,
    true,
  )

  collapseContext.emit('click')
  workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  assert.equal(workspace.getAttribute('data-context-collapsed'), 'true')

  collapseNavigation.emit('click')
  workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  assert.equal(workspace.getAttribute('data-navigation-collapsed'), 'false')
  mounted.close()
})

test('stored preferences are restored on the next mount', () => {
  const storage = new FakeStorage()
  storage.setItem('winwincode.strongflow.layout.v1', JSON.stringify({
    navigationWidth: 30,
    contextWidth: 24,
    navigationCollapsed: true,
    contextCollapsed: false,
    artifactsTab: 'evidence',
  }))
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(2, deliverySummary),
    limits,
    storage,
  })

  const workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  assert.equal(workspace.getAttribute('data-navigation-width'), '30')
  assert.equal(workspace.getAttribute('data-context-width'), '24')
  assert.equal(workspace.getAttribute('data-navigation-collapsed'), 'true')
  assert.equal(workspace.getAttribute('data-context-collapsed'), 'false')
  const tablist = findByRole(findByClass(rootElement, 'wwc-strongflow-artifacts'), 'tablist')
  assert.notEqual(tablist, null)
  const selected = findAllByClass(rootElement, 'wwc-strongflow-artifact-tab')
    .find(tab => tab.getAttribute('aria-selected') === 'true')
  assert.equal(selected?.dataset.artifactTab, 'evidence')
  mounted.close()
})

test('artifact tabs switch between solution, execution, candidate, and evidence panels', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(2, deliverySummary),
    limits,
    storage: new FakeStorage(),
  })

  const artifacts = findByClass(rootElement, 'wwc-strongflow-artifacts')
  const tabs = findAllByClass(rootElement, 'wwc-strongflow-artifact-tab')
  assert.deepEqual(tabs.map(tab => tab.dataset.artifactTab), [
    'solution',
    'execution',
    'candidate',
    'evidence',
  ])
  for (const tab of tabs) {
    assert.equal(tab.getAttribute('role'), 'tab')
    assert.match(tab.getAttribute('aria-controls'), /^wwc-strongflow-artifact-panel-/u)
  }

  const panels = findAllByClass(artifacts, 'wwc-strongflow-artifact-panel')
  const panel = tab => panels.find(item => item.dataset.artifactTab === tab)
  assert.notEqual(findByClass(artifacts, 'wwc-strongflow-view-solution'), null)
  assert.notEqual(findByClass(artifacts, 'wwc-strongflow-view-execution'), null)
  assert.notEqual(findByClass(artifacts, 'wwc-strongflow-view-candidate'), null)
  assert.notEqual(findByClass(artifacts, 'wwc-strongflow-evidence'), null)
  assert.equal(panel('solution').hidden, false)
  assert.equal(panel('execution').hidden, true)
  assert.equal(panel('candidate').hidden, true)
  assert.equal(panel('evidence').hidden, true)

  tabs.find(tab => tab.dataset.artifactTab === 'candidate').emit('click')
  assert.equal(panel('solution').hidden, true)
  assert.equal(panel('candidate').hidden, false)

  tabs.find(tab => tab.dataset.artifactTab === 'evidence').emit('click')
  assert.equal(panel('evidence').hidden, false)
  assert.equal(panel('candidate').hidden, true)

  let prevented = false
  tabs[0].emit('keydown', {
    key: 'ArrowRight',
    preventDefault() { prevented = true },
  })
  assert.equal(prevented, true)
  assert.equal(tabs[1].getAttribute('aria-selected'), 'true')
  assert.equal(document.activeElement, tabs[1])
  mounted.close()
})

test('narrow viewport degrades panes into labeled drawer navigation and tabbed context', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const storage = new FakeStorage()
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(2, deliverySummary),
    limits,
    storage,
    viewport: { width: 420 },
  })

  const workspace = findByClass(rootElement, 'wwc-strongflow-workspace')
  assert.equal(workspace.getAttribute('data-viewport'), 'narrow')
  // Panes themselves collapse; their content stays reachable through drawers.
  const navigationDrawer = findByClass(rootElement, 'wwc-strongflow-navigation-drawer')
  const contextDrawer = findByClass(rootElement, 'wwc-strongflow-context-drawer')
  assert.notEqual(navigationDrawer, null)
  assert.notEqual(contextDrawer, null)
  assert.equal(navigationDrawer.getAttribute('role'), 'dialog')
  assert.equal(navigationDrawer.getAttribute('aria-modal'), 'false')
  assert.notEqual(findByClass(navigationDrawer, 'wwc-strongflow-delivery-list'), null)
  assert.notEqual(findByClass(contextDrawer, 'wwc-strongflow-attention-list'), null)
  assert.equal(navigationDrawer.hidden, true)
  assert.equal(contextDrawer.hidden, true)

  const openNavigation = findByClass(rootElement, 'wwc-strongflow-open-navigation')
  const openContext = findByClass(rootElement, 'wwc-strongflow-open-context')
  assert.notEqual(openNavigation, null)
  assert.notEqual(openContext, null)
  openNavigation.focus()
  openNavigation.emit('click')
  assert.equal(findByClass(rootElement, 'wwc-strongflow-navigation-drawer').hidden, false)
  const navigationClose = findByClass(navigationDrawer, 'wwc-drawer-close')
  assert.equal(document.activeElement, navigationClose)
  let escapePrevented = false
  navigationDrawer.emit('keydown', {
    key: 'Escape',
    preventDefault() { escapePrevented = true },
  })
  assert.equal(escapePrevented, true)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-navigation-drawer').hidden, true)
  assert.equal(document.activeElement, openNavigation)
  openContext.emit('click')
  assert.equal(findByClass(rootElement, 'wwc-strongflow-context-drawer').hidden, false)

  // Narrow mode still shows review actions and artifact tabs; no resize handles.
  assert.equal(findByClass(rootElement, 'wwc-strongflow-resize-navigation').hidden, true)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-resize-context').hidden, true)
  assert.notEqual(findByClass(rootElement, 'wwc-strongflow-actions'), null)
  assert.deepEqual(
    findAllByClass(rootElement, 'wwc-strongflow-artifact-tab').map(tab => tab.dataset.artifactTab),
    ['solution', 'execution', 'candidate', 'evidence'],
  )
  mounted.close()
})

test('viewport mode changes re-render without losing the current projection', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  let width = 1400
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(2, deliverySummary),
    limits,
    storage: new FakeStorage(),
    viewport: {
      get width() {
        return width
      },
    },
  })
  assert.equal(findByClass(rootElement, 'wwc-strongflow-workspace').getAttribute('data-viewport'), 'wide')
  width = 400
  model.publish(state({ status: 'refreshing' }))
  assert.equal(findByClass(rootElement, 'wwc-strongflow-workspace').getAttribute('data-viewport'), 'narrow')
  assert.notEqual(findByClass(rootElement, 'wwc-strongflow-heading').textContent.match(
    /Bounded StrongFlow workspace/u,
  ), null)
  mounted.close()
})

test('workspace uses the canonical 64rem breakpoint', () => {
  assert.equal(page.strongFlowLayoutMode(900), 'narrow')
  assert.equal(page.strongFlowLayoutMode(1_024), 'narrow')
  assert.equal(page.strongFlowLayoutMode(1_025), 'wide')
})

test('two hundred equivalent state snapshots preserve drafts, focus, and artifact identity', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const initialState = state()
  initialState.projection.solutionReview.reviewStatus = 'pending'
  const model = new FakeStrongFlowViewModel(initialState)
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: many(2, deliverySummary),
    limits,
    storage: new FakeStorage(),
    viewport: { width: 1_440 },
  })

  const solutionActions = findByClass(rootElement, 'wwc-strongflow-solution-actions')
  const attentionActions = findByClass(rootElement, 'wwc-strongflow-attention-actions')
  const comments = findFirst(solutionActions, node => node.tagName === 'TEXTAREA')
  const attentionDraft = findFirst(attentionActions, node => node.tagName === 'TEXTAREA')
  const taskRow = findByClass(rootElement, 'wwc-strongflow-task-list').children[0]
  const candidate = findByClass(rootElement, 'wwc-strongflow-view-candidate')
  const diagrams = findByClass(rootElement, 'wwc-strongflow-diagrams')
  const selectedTab = findAllByClass(rootElement, 'wwc-strongflow-artifact-tab')
    .find(tab => tab.getAttribute('aria-selected') === 'true')
  comments.value = 'Keep this review draft'
  attentionDraft.value = 'Keep this Attention draft'
  attentionDraft.focus()
  candidate.scrollTop = 37

  for (let index = 0; index < 200; index += 1) {
    model.publish({
      ...structuredClone(initialState),
      status: index % 2 === 0 ? 'refreshing' : 'ready',
    })
  }

  assert.equal(findFirst(solutionActions, node => node.tagName === 'TEXTAREA'), comments)
  assert.equal(comments.value, 'Keep this review draft')
  assert.equal(findFirst(attentionActions, node => node.tagName === 'TEXTAREA'), attentionDraft)
  assert.equal(attentionDraft.value, 'Keep this Attention draft')
  assert.equal(document.activeElement, attentionDraft)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-task-list').children[0], taskRow)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-view-candidate'), candidate)
  assert.equal(candidate.scrollTop, 37)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-diagrams'), diagrams)

  selectedTab.focus()
  model.publish(structuredClone(initialState))
  assert.equal(
    findAllByClass(rootElement, 'wwc-strongflow-artifact-tab')
      .find(tab => tab.getAttribute('aria-selected') === 'true'),
    selectedTab,
  )
  assert.equal(document.activeElement, selectedTab)
  mounted.close()
})
