import { mountStrongFlowPage } from '/module/strongflow-page.js'
import { mountStrongFlowDiagramGraph } from '/module/strongflow-diagram-graph.js'

const root = document.querySelector('[data-winwincode-client-root]')
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const reviewSetSha256 = `sha256:${'a'.repeat(64)}`

function solutionNodes() {
  return [
    {
      id: 'platform:dsh',
      label: 'DSH',
      description: 'Chat shell and workbench surface.',
      kind: 'interaction',
      trustBoundary: 'DSH product shell',
      unresolved: false,
    },
    {
      id: 'platform:strongflow',
      label: 'WinWinCode',
      description: 'Delivery control plane.',
      kind: 'delivery-control',
      trustBoundary: 'Delivery control plane',
      unresolved: false,
    },
    {
      id: 'component:delivery-api',
      label: 'Delivery API',
      description: 'Serves delivery reads and commands.',
      kind: 'component',
      trustBoundary: 'Delivery control plane',
      unresolved: true,
    },
    {
      id: 'platform:codex-core',
      label: 'Codex Core',
      description: 'Execution kernel.',
      kind: 'execution',
      trustBoundary: 'Execution authority',
      unresolved: false,
    },
  ]
}

function solutionEdges() {
  return [
    { id: 'edge:dsh-submit', from: 'platform:dsh', to: 'platform:strongflow', label: 'submit' },
    {
      id: 'edge:control-api',
      from: 'platform:strongflow',
      to: 'component:delivery-api',
      label: 'calls',
    },
    {
      id: 'edge:api-exec',
      from: 'component:delivery-api',
      to: 'platform:codex-core',
      label: 'executes',
    },
  ]
}

function solutionDiagram(kind) {
  return {
    id: `diagram:${kind}`,
    kind,
    title: `${kind} diagram`,
    nodes: solutionNodes(),
    edges: solutionEdges(),
  }
}

const projection = {
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
      title: 'Real browser interactive diagram graphs',
      goal: 'Keep the approved diagrams interactive across live runtime snapshots.',
    },
    tasks: [{ id: 'task:browser', title: 'Verify the interactive graphs', status: 'active' }],
    stages: [{ id: stageRunId, stage: 'executing', role: 'implementer', status: 'running' }],
    attention: [{
      id: 'attention:browser',
      title: 'Review the interactive graph proof',
      status: 'open',
      type: 'decision_required',
    }],
  },
  solutionReview: {
    deliveryId,
    deliverySpecId: 'spec:browser',
    deliverySpecRevision: 3,
    reviewStatus: 'pending',
    reviewSetSha256,
    architectureDiagram: solutionDiagram('system-architecture'),
    processDiagram: solutionDiagram('process-flow'),
  },
  diagramExecution: {
    schemaVersion: 1,
    protocol: 'winwincode.diagram-execution-projection.v1',
    deliveryId,
    deliveryRevision: 4,
    reviewSetSha256,
    state: 'executing',
    architecture: {
      diagramId: 'diagram:system-architecture',
      kind: 'system-architecture',
      nodes: solutionNodes().map(node => ({
        nodeId: node.id,
        state: node.id === 'platform:dsh'
          ? 'affected-live'
          : node.id === 'platform:strongflow' ? 'affected-finished' : 'normal',
        affectedFileCount: node.id === 'platform:dsh' || node.id === 'platform:strongflow' ? 1 : 0,
        fileIds: [],
      })),
    },
    process: {
      diagramId: 'diagram:process-flow',
      kind: 'process-flow',
      nodes: solutionNodes().map(node => ({
        nodeId: node.id,
        state: 'normal',
        affectedFileCount: 0,
        fileIds: [],
      })),
    },
    affectedFileCount: 1,
    details: null,
    updatedAt: '2026-09-02T08:00:00.000Z',
  },
  stage: { id: stageRunId },
  runtime: {
    stageRunId,
    sessions: [{
      sessionBindingId: 'bind:1',
      executionJobId: 'job:1',
      deliveryTaskId: 'task:browser',
      attempt: 1,
      asOfSequence: 1,
      agents: [{
        threadId: 'cdx_00000000000000000000000001',
        nickname: 'Browser worker',
        role: 'worker',
        status: 'running',
      }],
      agentEdges: [],
      activities: [{ activityType: 'test', status: 'running', outcome: 'observed' }],
      diffSummary: null,
    }],
  },
  evidence: [],
  verdict: null,
  attention: [{
    id: 'attention:browser',
    title: 'Review the interactive graph proof',
    status: 'open',
    type: 'decision_required',
  }],
  currentCandidate: null,
  publication: null,
  metadata: {
    source: 'control-plane-snapshot',
    updatedAt: '2026-09-02T08:00:00.000Z',
    revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 1 },
    readCursor: {},
  },
}

class BrowserStrongFlowModel {
  draftScope = '["strongflow-diagram-browser-actor","strongflow-diagram-browser-scope"]'

  state = {
    status: 'ready',
    realtime: 'subscribed',
    projection,
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
  }

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  async start() {}
  async refresh() {}
  async loadCandidateFiles() {}
  async loadMoreCandidateFiles() {}
  async selectCandidateFile() {}
  async loadMoreCandidateDiff() {}
  async decideSolutionReview() {}
  async approveTaskBreakdown() {}
  async resolveAttention() {}
  async submitVerdict() {}
  async advanceDelivery() {}
  cancelPending() {}
  reconnect() {}
  close() {}

  publish(next) {
    this.state = next
    this.listener?.(next)
  }
}

const model = new BrowserStrongFlowModel()
const mounted = mountStrongFlowPage({
  root,
  model,
  deliveries: [{
    schemaVersion: 'winwincode/v1',
    deliveryId,
    title: 'Real browser StrongFlow diagrams',
    revision: 4,
    status: 'executing',
  }],
})

function architectureGraph() {
  return document.querySelector('.wwc-strongflow-graph[data-diagram="diagram:system-architecture"]')
}

function visible(element) {
  if (element === null || element.hidden) return false
  const style = getComputedStyle(element)
  const rectangle = element.getBoundingClientRect()
  return style.display !== 'none' && style.visibility !== 'hidden'
    && rectangle.width > 0 && rectangle.height > 0
}

function nodeRectangles(graph) {
  return [...graph.querySelectorAll('.wwc-strongflow-graph-node')].map(node => {
    const rectangle = node.getBoundingClientRect()
    return {
      id: node.dataset.id,
      x: Math.round(rectangle.x),
      y: Math.round(rectangle.y),
    }
  })
}

globalThis.measureDiagramEdgeGeometry = () => {
  const geometryProbe = mountStrongFlowDiagramGraph({
    document,
    props: {
      id: 'diagram:edge-geometry-probe',
      title: 'Edge geometry probe',
      narrow: window.innerWidth <= 420,
      nodes: [
        {
          id: 'probe:a',
          label: 'A',
          description: 'Origin.',
          kind: 'component',
          trustBoundary: null,
          unresolved: false,
        },
        {
          id: 'probe:b',
          label: 'B',
          description: 'First branch.',
          kind: 'component',
          trustBoundary: null,
          unresolved: false,
        },
        {
          id: 'probe:c',
          label: 'C',
          description: 'Second branch.',
          kind: 'component',
          trustBoundary: null,
          unresolved: false,
        },
      ],
      edges: [
        { id: 'probe:a-b', from: 'probe:a', to: 'probe:b', label: 'first' },
        { id: 'probe:a-c', from: 'probe:a', to: 'probe:c', label: 'second' },
      ],
    },
  })
  document.body.append(geometryProbe.root)
  const probeEdge = geometryProbe.root.querySelector(
    '.wwc-strongflow-graph-edge[data-id="probe:a-c"]',
  ).getBoundingClientRect()
  const probeFrom = geometryProbe.nodeElement('probe:a').getBoundingClientRect()
  const probeTo = geometryProbe.nodeElement('probe:c').getBoundingClientRect()
  const connected = probeEdge.left <= probeFrom.right + 1
    && probeEdge.right >= probeTo.left - 1
    && probeEdge.top <= probeFrom.bottom + 1
    && probeEdge.bottom >= probeTo.top - 1
  geometryProbe.close()
  return connected
}

globalThis.measureLargeGraphFit = () => {
  const nodes = Array.from({ length: 24 }, (_, index) => ({
    id: `large:${String(index).padStart(2, '0')}`,
    label: `Large ${String(index + 1)}`,
    description: 'Bounded large-graph fit probe.',
    kind: 'component',
    trustBoundary: null,
    unresolved: false,
  }))
  const probe = mountStrongFlowDiagramGraph({
    document,
    props: {
      id: 'diagram:large-fit-probe',
      title: 'Large fit probe',
      narrow: window.innerWidth <= 420,
      nodes,
      edges: nodes.slice(1).map((node, index) => ({
        id: `large-edge:${String(index).padStart(2, '0')}`,
        from: nodes[index].id,
        to: node.id,
        label: 'next',
      })),
    },
  })
  document.body.append(probe.root)
  const viewport = probe.root.querySelector('.wwc-strongflow-graph-viewport')
  const canvas = probe.root.querySelector('.wwc-strongflow-graph-canvas')
  probe.root.querySelector('.wwc-strongflow-graph-fit').click()
  const zoom = Number(viewport.getAttribute('data-zoom'))
  const result = {
    fits: canvas.scrollWidth * zoom <= viewport.clientWidth + 1
      && canvas.scrollHeight * zoom <= viewport.clientHeight + 1,
    positiveFinite: Number.isFinite(zoom) && zoom > 0,
    zoom,
  }
  probe.close()
  return result
}

globalThis.runDiagramGraphScenario = () => {
  const graph = architectureGraph()
  const viewport = graph.querySelector('.wwc-strongflow-graph-viewport')
  const nodes = [...graph.querySelectorAll('.wwc-strongflow-graph-node')]
  const edges = [...graph.querySelectorAll('.wwc-strongflow-graph-edge')]
  const unresolvedNode = graph.querySelector(
    '.wwc-strongflow-graph-node[data-id="component:delivery-api"]',
  )
  const liveNode = graph.querySelector(
    '.wwc-strongflow-graph-node[data-id="platform:dsh"]',
  )
  const finishedNode = graph.querySelector(
    '.wwc-strongflow-graph-node[data-id="platform:strongflow"]',
  )
  const firstNode = nodes[0]
  firstNode.focus()
  firstNode.dispatchEvent(new KeyboardEvent('keydown', {
    key: 'ArrowRight',
    bubbles: true,
    cancelable: true,
  }))
  const keyboardFocus = {
    id: document.activeElement?.dataset.id ?? null,
  }
  const positions = nodeRectangles(graph)
  unresolvedNode.click()
  const zoomIn = graph.querySelector('.wwc-strongflow-graph-zoom-in')
  zoomIn.click()
  const zoomTransform = getComputedStyle(
    viewport.querySelector('.wwc-strongflow-graph-canvas'),
  ).transform
  const boundary = [...graph.querySelectorAll('.wwc-strongflow-graph-boundary')]
    .find(header => header.dataset.boundary === 'Delivery control plane')
  boundary.click()
  const boundaryState = {
    collapsed: boundary.getAttribute('aria-expanded'),
    memberHidden: graph.querySelector(
      '.wwc-strongflow-graph-node[data-id="component:delivery-api"]',
    ).hidden,
    chipVisible: visible(graph.querySelector('.wwc-strongflow-graph-group')),
  }
  const toggle = graph.querySelector('.wwc-strongflow-graph-toggle-view')
  toggle.click()
  const edgeGeometryConnected = globalThis.measureDiagramEdgeGeometry()

  return {
    boundary: boundaryState,
    detail: graph.querySelector('.wwc-strongflow-graph-detail').textContent,
    edges: edges.map(edge => ({
      ariaLabel: edge.getAttribute('aria-label'),
      id: edge.dataset.id,
    })),
    edgeGeometryConnected,
    overview: {
      label: graph.querySelector('.wwc-strongflow-graph-overview').getAttribute('aria-label'),
      nodes: graph.querySelectorAll('.wwc-strongflow-graph-overview-node').length,
    },
    keyboardFocus,
    listEquivalent: (() => {
      const list = graph.querySelector('.wwc-strongflow-graph-list')
      const row = list.querySelector('.wwc-strongflow-graph-list-node[data-id="component:delivery-api"]')
      return {
        hidden: list.hidden,
        rowText: row.textContent,
        rowCount: list.querySelectorAll('.wwc-strongflow-graph-list-node').length,
      }
    })(),
    nodes: {
      count: nodes.length,
      positions,
      unresolved: {
        badge: unresolvedNode.querySelector('.wwc-strongflow-graph-node-badge')?.textContent ?? null,
        color: getComputedStyle(unresolvedNode).backgroundColor,
        icon: unresolvedNode.querySelector('.wwc-strongflow-graph-node-icon')?.textContent ?? null,
      },
    },
    statusSignals: [
      unresolvedNode.querySelector('.wwc-strongflow-graph-node-badge'),
      liveNode.querySelector('.wwc-strongflow-graph-node-execution'),
      finishedNode.querySelector('.wwc-strongflow-graph-node-execution'),
    ].map(signal => {
      const icon = signal.querySelector('.wwc-strongflow-graph-status-icon')
      return {
        iconHidden: icon.getAttribute('aria-hidden'),
        iconText: icon.textContent,
        text: signal.textContent,
      }
    }),
    selection: {
      ariaPressed: unresolvedNode.getAttribute('aria-pressed'),
      label: unresolvedNode.getAttribute('aria-label'),
    },
    stateChips: {
      deliveryStatus: document.querySelector('.wwc-strongflow-solution-state')?.dataset.deliveryStatus,
      text: document.querySelector('.wwc-strongflow-solution-state')?.textContent,
    },
    viewport: {
      label: viewport.getAttribute('aria-label'),
      mode: viewport.getAttribute('data-viewport'),
      role: viewport.getAttribute('role'),
      transform: zoomTransform,
      zoom: viewport.getAttribute('data-zoom'),
    },
  }
}

globalThis.runDiagramStabilityScenario = () => {
  const graph = architectureGraph()
  graph.querySelector('.wwc-strongflow-graph-toggle-view').click()
  const boundary = [...graph.querySelectorAll('.wwc-strongflow-graph-boundary')]
    .find(header => header.dataset.boundary === 'Delivery control plane')
  if (boundary.getAttribute('aria-expanded') === 'false') boundary.click()
  const node = graph.querySelector('.wwc-strongflow-graph-node[data-id="platform:dsh"]')
  node.click()
  node.focus()
  const selectedBefore = node.getAttribute('aria-pressed')
  const observer = new MutationObserver(() => {})
  observer.observe(graph, { attributes: true, childList: true, characterData: true, subtree: true })
  for (let index = 0; index < 100; index += 1) {
    const next = structuredClone(model.state)
    next.projection.runtime.sessions[0].asOfSequence = 100 + index
    next.projection.runtime.sessions[0].agents[0].status = index % 2 === 0 ? 'running' : 'waiting'
    next.status = index % 2 === 0 ? 'refreshing' : 'ready'
    model.publish(next)
  }
  const graphMutations = observer.takeRecords().length
  observer.disconnect()
  const after = graph.querySelector('.wwc-strongflow-graph-node[data-id="platform:dsh"]')
  return {
    focusKept: document.activeElement === node,
    graphMutations,
    nodeIdentity: after === node,
    pressedKept: after.getAttribute('aria-pressed') === selectedBefore,
    zoomKept: graph.querySelector('.wwc-strongflow-graph-viewport').getAttribute('data-zoom'),
  }
}

globalThis.waitForDiagramViewportMode = mode => new Promise((resolvePromise, reject) => {
  const deadline = Date.now() + 5_000
  const check = () => {
    const viewport = architectureGraph()?.querySelector('.wwc-strongflow-graph-viewport')
    if (viewport?.getAttribute('data-viewport') === mode) {
      resolvePromise(viewport.getAttribute('data-viewport'))
      return
    }
    if (Date.now() >= deadline) {
      reject(new Error(`timed out waiting for ${mode} diagram viewport`))
      return
    }
    setTimeout(check, 20)
  }
  check()
})

globalThis.closeStrongFlowDiagramGraphFixture = () => { mounted.close() }
