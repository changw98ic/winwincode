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
  '.cache/strongflow-page-tests/strongflow-diagram-graph.js',
)).href}`)
const page = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-page.js',
)).href}`)
const diagramsModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-diagrams.js',
)).href}`)
const {
  mountStrongFlowDiagramGraph,
  strongFlowDiagramGraphLayout,
} = graphModule
const { mountStrongFlowPage } = page
const { mountStrongFlowDiagrams } = diagramsModule

const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'

function many(count, create) {
  return Array.from({ length: count }, (_, index) => create(index + 1))
}

function solutionDiagramNodes() {
  return [
    {
      id: 'platform:dsh',
      label: 'DSH',
      description: 'Chat and workbench shell.',
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
      id: 'component:api',
      label: 'API',
      description: 'Serves delivery reads.',
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

function solutionDiagramEdges() {
  return [
    { id: 'edge:dsh-submit', from: 'platform:dsh', to: 'platform:strongflow', label: 'submit' },
    { id: 'edge:control-api', from: 'platform:strongflow', to: 'component:api', label: 'calls' },
    { id: 'edge:api-exec', from: 'component:api', to: 'platform:codex-core', label: 'executes' },
  ]
}

function graphProps(overrides = {}) {
  return {
    id: 'architecture:test',
    title: 'Architecture',
    nodes: solutionDiagramNodes(),
    edges: solutionDiagramEdges(),
    narrow: false,
    ...overrides,
  }
}

test('layout ranks nodes by longest path and orders ranks by node id', () => {
  const layout = strongFlowDiagramGraphLayout(
    solutionDiagramNodes(),
    solutionDiagramEdges(),
  )
  assert.equal(layout.get('platform:dsh').rank, 0)
  assert.equal(layout.get('platform:strongflow').rank, 1)
  assert.equal(layout.get('component:api').rank, 2)
  assert.equal(layout.get('platform:codex-core').rank, 3)
  for (const position of layout.values()) {
    assert.equal(position.y, position.row * 120)
    assert.equal(position.x, position.rank * 220)
  }
})

test('layout is deterministic for reordered input and survives cycles', () => {
  const nodes = [...solutionDiagramNodes()].reverse()
  const edges = [...solutionDiagramEdges(), {
    id: 'edge:cycle',
    from: 'platform:codex-core',
    to: 'platform:dsh',
    label: 'events',
  }]
  const first = strongFlowDiagramGraphLayout(solutionDiagramNodes(), solutionDiagramEdges())
  const second = strongFlowDiagramGraphLayout(nodes, solutionDiagramEdges())
  assert.deepEqual([...first.entries()], [...second.entries()])
  const cyclic = strongFlowDiagramGraphLayout(solutionDiagramNodes(), edges)
  assert.equal(cyclic.size, solutionDiagramNodes().length)
  assert.ok([...cyclic.values()].every(position => Number.isFinite(position.rank)))

  const cycleNodes = solutionDiagramNodes().slice(0, 3)
  const cycleEdges = [
    { id: 'cycle:1', from: cycleNodes[0].id, to: cycleNodes[1].id, label: 'next' },
    { id: 'cycle:2', from: cycleNodes[1].id, to: cycleNodes[2].id, label: 'next' },
    { id: 'cycle:3', from: cycleNodes[2].id, to: cycleNodes[0].id, label: 'next' },
  ]
  const canonicalCycle = [...strongFlowDiagramGraphLayout(cycleNodes, cycleEdges).entries()]
    .sort(([left], [right]) => left.localeCompare(right))
  const reorderedCycle = [...strongFlowDiagramGraphLayout(
    [cycleNodes[1], cycleNodes[2], cycleNodes[0]],
    [cycleEdges[2], cycleEdges[0], cycleEdges[1]],
  ).entries()].sort(([left], [right]) => left.localeCompare(right))
  assert.deepEqual(
    reorderedCycle,
    canonicalCycle,
    'winwincode-8pc: cyclic geometry must not depend on snapshot array order',
  )
})

test('graph renders keyed nodes and labelled edges with multi-signal node states', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })

  const heading = findFirst(view.root, node => node.tagName === 'H4')
  assert.equal(heading.textContent, 'Architecture')

  const viewport = findByClass(view.root, 'wwc-strongflow-graph-viewport')
  assert.equal(viewport.getAttribute('role'), 'group')
  assert.match(viewport.getAttribute('aria-label'), /Architecture graph viewport/u)
  assert.equal(viewport.getAttribute('data-zoom'), '1')

  const nodeButtons = findAllByClass(view.root, 'wwc-strongflow-graph-node')
  assert.deepEqual(nodeButtons.map(node => node.dataset.id), [
    'platform:dsh',
    'platform:strongflow',
    'component:api',
    'platform:codex-core',
  ])
  const nodeLabels = new Map([
    ['platform:dsh', 'DSH'],
    ['platform:strongflow', 'WinWinCode'],
    ['component:api', 'API'],
    ['platform:codex-core', 'Codex Core'],
  ])
  for (const node of nodeButtons) {
    assert.equal(node.tagName, 'BUTTON')
    assert.ok(node.dataset.kind.length > 0)
    assert.ok(node.getAttribute('aria-label').includes(nodeLabels.get(node.dataset.id)))
    assert.equal(
      findByClass(node, 'wwc-strongflow-graph-node-label').textContent,
      nodeLabels.get(node.dataset.id),
    )
  }
  const unresolvedNode = nodeButtons.find(node => node.dataset.id === 'component:api')
  assert.equal(unresolvedNode.dataset.unresolved, 'true')
  const unresolvedBadge = findByClass(unresolvedNode, 'wwc-strongflow-graph-node-badge')
  assert.equal(unresolvedBadge.textContent, 'Unresolved')
  const icon = findByClass(unresolvedNode, 'wwc-strongflow-graph-node-icon')
  assert.ok(icon.textContent.length > 0)
  const resolvedNode = nodeButtons.find(node => node.dataset.id === 'platform:dsh')
  assert.equal(resolvedNode.dataset.unresolved, 'false')
  assert.equal(findByClass(resolvedNode, 'wwc-strongflow-graph-node-badge'), null)

  const edgeNodes = findAllByClass(view.root, 'wwc-strongflow-graph-edge')
  assert.deepEqual(edgeNodes.map(edge => edge.dataset.id), [
    'edge:dsh-submit',
    'edge:control-api',
    'edge:api-exec',
  ])
  assert.equal(
    edgeNodes[0].getAttribute('aria-label'),
    'DSH → WinWinCode: submit',
  )
  assert.equal(findByClass(edgeNodes[0], 'wwc-strongflow-graph-edge-label').textContent, 'submit')
  assert.equal(
    findByClass(edgeNodes[0], 'wwc-strongflow-graph-edge-line').getAttribute('aria-hidden'),
    'true',
  )

  const overview = findByClass(view.root, 'wwc-strongflow-graph-overview')
  assert.equal(overview.tagName, 'BUTTON')
  assert.match(overview.getAttribute('aria-label'), /4 nodes and 3 connections/u)
  assert.equal(findAllByClass(overview, 'wwc-strongflow-graph-overview-node').length, 4)
  assert.equal(findAllByClass(overview, 'wwc-strongflow-graph-overview-edge').length, 3)

  assert.equal(view.selectedNodeId(), null)
  view.close()
})

test('keyed graph updates reject duplicate node identities', () => {
  const document = new FakeDocument()
  const nodes = solutionDiagramNodes()
  assert.throws(() => {
    const view = mountStrongFlowDiagramGraph({
      document,
      props: graphProps({ nodes: [...nodes, { ...nodes[0] }] }),
    })
    view.close()
  }, /duplicate.*node|node.*duplicate/iu, 'winwincode-met: duplicate nodes must fail fast')
})

test('decorative node-kind icons stay out of the accessible name', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })
  const icons = findAllByClass(view.root, 'wwc-strongflow-graph-node-icon')
  assert.equal(
    icons.every(icon => icon.getAttribute('aria-hidden') === 'true'),
    true,
    'winwincode-ec6: decorative status glyphs must be hidden from assistive technology',
  )
  view.close()
})

test('unresolved and execution status signals use their own hidden icons', () => {
  const document = new FakeDocument()
  const nodes = solutionDiagramNodes().map(node => ({
    ...node,
    executionState: node.id === 'platform:dsh'
      ? 'affected-live'
      : node.id === 'platform:strongflow' ? 'affected-finished' : 'normal',
    affectedFileCount: node.id === 'platform:dsh' || node.id === 'platform:strongflow' ? 1 : 0,
  }))
  const view = mountStrongFlowDiagramGraph({
    document,
    props: graphProps({ nodes }),
  })
  const graphNodes = findAllByClass(view.root, 'wwc-strongflow-graph-node')
  const unresolved = graphNodes.find(node => node.dataset.id === 'component:api')
  const affectedLive = graphNodes.find(node => node.dataset.id === 'platform:dsh')
  const affectedFinished = graphNodes.find(node => node.dataset.id === 'platform:strongflow')
  const statusBadges = [
    findByClass(unresolved, 'wwc-strongflow-graph-node-badge'),
    findByClass(affectedLive, 'wwc-strongflow-graph-node-execution'),
    findByClass(affectedFinished, 'wwc-strongflow-graph-node-execution'),
  ]
  for (const badge of statusBadges) {
    assert.ok(badge)
    assert.equal(
      badge.children.some(child => (
        child.getAttribute('aria-hidden') === 'true' && child.textContent.length > 0
      )),
      true,
      'each status needs its own decorative icon, hidden from assistive technology',
    )
  }
  assert.match(textContentOf(unresolved), /Unresolved/u)
  assert.match(textContentOf(affectedLive), /Affected live/u)
  assert.match(textContentOf(affectedFinished), /Affected finished/u)
  view.close()
})

test('keyed graph updates reject duplicate edge identities', () => {
  const document = new FakeDocument()
  const edges = solutionDiagramEdges()
  assert.throws(() => {
    const view = mountStrongFlowDiagramGraph({
      document,
      props: graphProps({ edges: [...edges, { ...edges[0] }] }),
    })
    view.close()
  }, /duplicate.*edge|edge.*duplicate/iu, 'winwincode-met: duplicate edges must fail fast')
})

test('fresh mounts use the same graph and list order for reordered snapshots', () => {
  const document = new FakeDocument()
  const canonical = mountStrongFlowDiagramGraph({ document, props: graphProps() })
  const reordered = mountStrongFlowDiagramGraph({
    document,
    props: graphProps({
      nodes: [...solutionDiagramNodes()].reverse(),
      edges: [...solutionDiagramEdges()].reverse(),
    }),
  })
  const identities = (root, className) => findAllByClass(root, className)
    .map(node => node.dataset.id)
  assert.deepEqual(
    identities(reordered.root, 'wwc-strongflow-graph-node'),
    identities(canonical.root, 'wwc-strongflow-graph-node'),
    'winwincode-met: graph DOM order must not depend on first snapshot order',
  )
  assert.deepEqual(
    identities(reordered.root, 'wwc-strongflow-graph-list-node'),
    identities(canonical.root, 'wwc-strongflow-graph-list-node'),
    'winwincode-met: equivalent list order must match the graph order',
  )
  canonical.close()
  reordered.close()
})

test('selecting a node updates detail, aria state, and notifies the page', () => {
  const document = new FakeDocument()
  const selected = []
  const view = mountStrongFlowDiagramGraph({
    document,
    props: graphProps({ onSelectNode: id => selected.push(id) }),
  })

  const apiNode = view.nodeElement('component:api')
  apiNode.emit('click')
  assert.equal(view.selectedNodeId(), 'component:api')
  assert.equal(apiNode.getAttribute('aria-pressed'), 'true')
  assert.deepEqual(selected, ['component:api'])
  const detail = findByClass(view.root, 'wwc-strongflow-graph-detail')
  assert.match(detail.textContent, /Serves delivery reads\./u)
  assert.match(detail.textContent, /Unresolved/u)
  assert.match(detail.textContent, /Delivery control plane/u)

  apiNode.emit('click')
  assert.equal(view.selectedNodeId(), null)
  assert.equal(apiNode.getAttribute('aria-pressed'), 'false')
  assert.deepEqual(selected, ['component:api', null])
  view.close()
})

test('replacing the canonical graph clears selection through the page callback', () => {
  const document = new FakeDocument()
  const selected = []
  const view = mountStrongFlowDiagramGraph({
    document,
    props: graphProps({ onSelectNode: id => selected.push(id) }),
  })
  view.nodeElement('component:api').emit('click')

  view.update(graphProps({
    id: 'diagram:replacement',
    onSelectNode: id => selected.push(id),
  }))

  assert.equal(view.selectedNodeId(), null)
  assert.deepEqual(selected, ['component:api', null])
  view.close()
})

test('arrow keys move focus spatially and Home and End reach the extremes', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })

  const dsh = view.nodeElement('platform:dsh')
  dsh.focus()
  let prevented = 0
  dsh.emit('keydown', { key: 'ArrowRight', preventDefault() { prevented += 1 } })
  assert.equal(document.activeElement, view.nodeElement('platform:strongflow'))
  document.activeElement.emit('keydown', { key: 'ArrowRight', preventDefault() { prevented += 1 } })
  assert.equal(document.activeElement, view.nodeElement('component:api'))
  document.activeElement.emit('keydown', { key: 'ArrowLeft', preventDefault() { prevented += 1 } })
  assert.equal(document.activeElement, view.nodeElement('platform:strongflow'))
  document.activeElement.emit('keydown', { key: 'Home', preventDefault() { prevented += 1 } })
  assert.equal(document.activeElement, dsh)
  dsh.emit('keydown', { key: 'End', preventDefault() { prevented += 1 } })
  assert.equal(document.activeElement, view.nodeElement('platform:codex-core'))
  assert.equal(prevented, 5)
  assert.equal(view.nodeElement('platform:codex-core').tabIndex, 0)
  assert.equal(dsh.tabIndex, -1)
  view.close()
})

test('zoom buttons and viewport keys scale the graph and fit restores it', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })
  const viewport = findByClass(view.root, 'wwc-strongflow-graph-viewport')

  findByClass(view.root, 'wwc-strongflow-graph-zoom-in').emit('click')
  assert.equal(viewport.getAttribute('data-zoom'), '1.25')
  findByClass(view.root, 'wwc-strongflow-graph-zoom-in').emit('click')
  assert.equal(viewport.getAttribute('data-zoom'), '1.5')
  findByClass(view.root, 'wwc-strongflow-graph-zoom-out').emit('click')
  findByClass(view.root, 'wwc-strongflow-graph-zoom-out').emit('click')
  findByClass(view.root, 'wwc-strongflow-graph-zoom-out').emit('click')
  assert.equal(viewport.getAttribute('data-zoom'), '0.75')

  viewport.emit('keydown', { key: '+', preventDefault() {} })
  assert.equal(viewport.getAttribute('data-zoom'), '1')
  viewport.emit('keydown', { key: '-', preventDefault() {} })
  assert.equal(viewport.getAttribute('data-zoom'), '0.75')
  viewport.emit('keydown', { key: '0', preventDefault() {} })
  assert.equal(viewport.getAttribute('data-zoom'), '1')

  findByClass(view.root, 'wwc-strongflow-graph-zoom-in').emit('click')
  assert.equal(viewport.getAttribute('data-zoom'), '1.25')
  viewport.scrollTop = 90
  findByClass(view.root, 'wwc-strongflow-graph-fit').emit('click')
  assert.equal(viewport.getAttribute('data-zoom'), '1')
  assert.equal(viewport.scrollTop, 0)

  const canvas = findByClass(view.root, 'wwc-strongflow-graph-canvas')
  viewport.clientWidth = 600
  viewport.clientHeight = 300
  canvas.scrollWidth = 1_000
  canvas.scrollHeight = 300
  findByClass(view.root, 'wwc-strongflow-graph-zoom-in').emit('click')
  findByClass(view.root, 'wwc-strongflow-graph-overview').emit('click')
  assert.equal(viewport.getAttribute('data-zoom'), '0.6')
  view.close()
})

test('fit shows a large bounded graph below the manual zoom minimum', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })
  const viewport = findByClass(view.root, 'wwc-strongflow-graph-viewport')
  const canvas = findByClass(view.root, 'wwc-strongflow-graph-canvas')
  viewport.clientWidth = 300
  viewport.clientHeight = 240
  canvas.scrollWidth = 1_500
  canvas.scrollHeight = 800

  findByClass(view.root, 'wwc-strongflow-graph-fit').emit('click')

  assert.equal(
    viewport.getAttribute('data-zoom'),
    '0.2',
    'the full measured canvas must fit instead of stopping at the manual 0.5 limit',
  )
  assert.equal(viewport.scrollLeft, 0)
  assert.equal(viewport.scrollTop, 0)
  view.close()
})

test('trust boundary groups collapse into one chip and expand back', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })

  const headers = findAllByClass(view.root, 'wwc-strongflow-graph-boundary')
  assert.deepEqual(headers.map(header => header.dataset.boundary), [
    'DSH product shell',
    'Delivery control plane',
    'Execution authority',
  ])
  assert.equal(headers.every(header => header.getAttribute('aria-expanded') === 'true'), true)

  const controlPlane = headers.find(header => header.dataset.boundary === 'Delivery control plane')
  controlPlane.emit('click')
  assert.equal(controlPlane.getAttribute('aria-expanded'), 'false')
  assert.equal(view.nodeElement('platform:strongflow').hidden, true)
  assert.equal(view.nodeElement('component:api').hidden, true)
  const chip = findByClass(view.root, 'wwc-strongflow-graph-group')
  assert.equal(chip.dataset.boundary, 'Delivery control plane')
  assert.match(chip.getAttribute('aria-label'), /Delivery control plane, 2 nodes/u)

  const dsh = view.nodeElement('platform:dsh')
  dsh.emit('keydown', { key: 'ArrowRight', preventDefault() {} })
  assert.equal(document.activeElement.dataset.boundary, 'Delivery control plane')

  document.activeElement.emit('keydown', { key: 'ArrowRight', preventDefault() {} })
  assert.equal(document.activeElement, view.nodeElement('platform:codex-core'))

  controlPlane.emit('click')
  assert.equal(controlPlane.getAttribute('aria-expanded'), 'true')
  assert.equal(view.nodeElement('platform:strongflow').hidden, false)
  assert.equal(findByClass(view.root, 'wwc-strongflow-graph-group'), null)
  view.close()
})

function mountTwoBoundaryGraph(document) {
  const nodes = [
    {
      id: 'node:a',
      label: 'A',
      description: 'First boundary.',
      kind: 'component',
      trustBoundary: 'Boundary A',
      unresolved: false,
    },
    {
      id: 'node:b',
      label: 'B',
      description: 'Second boundary.',
      kind: 'component',
      trustBoundary: 'Boundary B',
      unresolved: false,
    },
  ]
  return mountStrongFlowDiagramGraph({
    document,
    props: graphProps({
      nodes,
      edges: [{ id: 'edge:a-b', from: 'node:a', to: 'node:b', label: 'crosses' }],
    }),
  })
}

test('collapsing the roving node promotes a visible keyboard anchor', () => {
  const document = new FakeDocument()
  const view = mountTwoBoundaryGraph(document)
  const headers = findAllByClass(view.root, 'wwc-strongflow-graph-boundary')
  headers.find(header => header.dataset.boundary === 'Boundary A').emit('click')
  const visibleTargets = [
    ...findAllByClass(view.root, 'wwc-strongflow-graph-node'),
    ...findAllByClass(view.root, 'wwc-strongflow-graph-group'),
  ].filter(target => !target.hidden)
  assert.equal(
    visibleTargets.some(target => target.tabIndex === 0),
    true,
    'winwincode-ec6: collapsing the roving anchor must promote a visible target',
  )
  view.close()
})

test('collapsing two trust boundaries retains their cross-boundary edge', () => {
  const document = new FakeDocument()
  const view = mountTwoBoundaryGraph(document)
  const headers = findAllByClass(view.root, 'wwc-strongflow-graph-boundary')
  headers.find(header => header.dataset.boundary === 'Boundary A').emit('click')
  headers.find(header => header.dataset.boundary === 'Boundary B').emit('click')
  const edge = findByClass(view.root, 'wwc-strongflow-graph-edge')
  assert.equal(
    edge.hidden,
    false,
    'winwincode-ec6: an edge between two collapsed groups remains visible',
  )
  view.close()
})

test('close removes listeners from every retained graph control', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })
  const header = findAllByClass(view.root, 'wwc-strongflow-graph-boundary')
    .find(candidate => candidate.dataset.boundary === 'Delivery control plane')
  header.emit('click')
  const chip = findByClass(view.root, 'wwc-strongflow-graph-group')
  const node = view.nodeElement('platform:dsh')

  view.close()

  assert.equal(node.listeners.size, 0)
  assert.equal(header.listeners.size, 0, 'winwincode-ec6: close must detach boundary listeners')
  assert.equal(chip.listeners.size, 0, 'winwincode-ec6: close must detach group listeners')
})

test('the equivalent list view stays reachable for accessibility users', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })

  const list = findByClass(view.root, 'wwc-strongflow-graph-list')
  const toggle = findByClass(view.root, 'wwc-strongflow-graph-toggle-view')
  assert.equal(list.hidden, true)
  assert.equal(toggle.getAttribute('aria-pressed'), 'false')
  assert.match(toggle.textContent, /Switch to list view/u)

  toggle.emit('click')
  assert.equal(toggle.getAttribute('aria-pressed'), 'true')
  assert.equal(list.hidden, false)
  const rows = findAllByClass(list, 'wwc-strongflow-graph-list-node')
  assert.deepEqual(rows.map(row => row.dataset.id), [
    'platform:dsh',
    'platform:strongflow',
    'component:api',
  ].concat(['platform:codex-core']))
  const apiRow = rows.find(row => row.dataset.id === 'component:api')
  const apiRowText = textContentOf(apiRow)
  assert.match(apiRowText, /Serves delivery reads\./u)
  assert.match(apiRowText, /Unresolved/u)
  assert.match(apiRowText, /WinWinCode → API: calls/u)
  assert.match(apiRowText, /API → Codex Core: executes/u)

  toggle.emit('click')
  assert.equal(list.hidden, true)
  assert.equal(toggle.getAttribute('aria-pressed'), 'false')
  view.close()
})

test('equivalent snapshot updates keep node identity, focus, and view state', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })
  const apiNode = view.nodeElement('component:api')
  const apiEdge = findAllByClass(view.root, 'wwc-strongflow-graph-edge')
    .find(edge => edge.dataset.id === 'edge:api-exec')

  apiNode.emit('click')
  findByClass(view.root, 'wwc-strongflow-graph-zoom-in').emit('click')
  findByClass(view.root, 'wwc-strongflow-graph-toggle-view').emit('click')
  const focused = view.nodeElement('platform:dsh')
  focused.focus()

  view.update(graphProps())
  assert.equal(view.nodeElement('component:api'), apiNode)
  assert.equal(view.selectedNodeId(), 'component:api')
  assert.equal(findByClass(view.root, 'wwc-strongflow-graph-viewport').getAttribute('data-zoom'), '1.25')
  assert.equal(findByClass(view.root, 'wwc-strongflow-graph-list').hidden, false)
  assert.equal(document.activeElement, focused)
  assert.equal(viewportOf(view).contains(focused), true)
  assert.equal(
    findAllByClass(view.root, 'wwc-strongflow-graph-edge')
      .find(edge => edge.dataset.id === 'edge:api-exec') === apiEdge,
    true,
    'canonical edge identity survives an equivalent snapshot',
  )
  view.close()
})

function viewportOf(view) {
  return findByClass(view.root, 'wwc-strongflow-graph-viewport')
}

test('updates change only affected nodes and drop removed selections', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps() })
  const apiNode = view.nodeElement('component:api')
  apiNode.emit('click')

  const renamed = solutionDiagramNodes().map(node => node.id === 'component:api'
    ? { ...node, label: 'API v2' }
    : node)
  view.update(graphProps({ nodes: renamed }))
  assert.equal(view.nodeElement('component:api'), apiNode)
  assert.match(textContentOf(apiNode), /API v2/u)
  assert.equal(view.selectedNodeId(), 'component:api')

  const withoutApi = renamed.filter(node => node.id !== 'component:api')
  const withoutApiEdges = solutionDiagramEdges().filter(edge => (
    edge.from !== 'component:api' && edge.to !== 'component:api'
  ))
  view.update(graphProps({ nodes: withoutApi, edges: withoutApiEdges }))
  assert.equal(view.nodeElement('component:api'), null)
  assert.equal(view.selectedNodeId(), null)
  view.close()
})

test('narrow viewports mark the graph for stacked styling', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagramGraph({ document, props: graphProps({ narrow: true }) })
  assert.equal(
    findByClass(view.root, 'wwc-strongflow-graph-viewport').getAttribute('data-viewport'),
    'narrow',
  )
  view.close()
})

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

function projection(overrides = {}) {
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
        title: 'Interactive diagram workbench',
        goal: 'Render the exact approved diagrams without rebuilding unrelated DOM.',
      },
      tasks: many(2, value => ({
        id: `task:${String(value)}`,
        title: `Task ${String(value)}`,
        status: value === 1 ? 'active' : 'pending',
      })),
      stages: many(2, value => ({
        id: value === 1 ? stageRunId : `run_${String(value).padStart(26, '0')}`,
        stage: value === 1 ? 'executing' : 'verifying',
        role: 'implementer',
        status: value === 1 ? 'running' : 'waiting',
      })),
      attention: [{
        id: 'attention:1',
        title: 'Attention 1',
        status: 'open',
      }],
    },
    solutionReview: {
      deliveryId,
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      reviewSetSha256: `sha256:${'a'.repeat(64)}`,
      attentionItemId: 'attention:1',
      reviewStatus: 'pending',
      architectureDiagram: diagram('system-architecture'),
      processDiagram: diagram('process-flow'),
    },
    stage: { id: stageRunId },
    runtime: {
      stageRunId,
      sessions: many(2, value => ({
        sessionBindingId: `bind:${String(value)}`,
        executionJobId: `job:${String(value)}`,
        deliveryTaskId: `task:${String(value)}`,
        attempt: 1,
        asOfSequence: value,
        agents: many(3, agentValue => ({
          threadId: `cdx_${String(agentValue).padStart(26, '0')}`,
          nickname: `Agent ${String(agentValue)}`,
          role: 'worker',
          status: agentValue === 1 ? 'running' : 'waiting',
        })),
        agentEdges: [],
        activities: [],
        diffSummary: null,
      })),
    },
    evidence: [],
    verdict: null,
    attention: [{
      id: 'attention:1',
      title: 'Attention 1',
      status: 'open',
    }],
    currentCandidate: null,
    diagramExecution: null,
    publication: null,
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-09-02T08:00:00.000Z',
      revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 1 },
      readCursor: {},
    },
    ...overrides,
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

class FakeStrongFlowViewModel {
  constructor(initialState) {
    this.state = initialState
  }

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

  async start() {}
  async refresh() {}
  async decideSolutionReview() {}
  async approveTaskBreakdown() {}
  async resolveAttention() {}
  async submitVerdict() {}
  async advanceDelivery() {}
  cancelPending() {}
  reconnect() {}
  close() {}
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
}

const limits = {
  deliveries: 2,
  tasks: 2,
  stages: 2,
  attention: 2,
  evidence: 2,
  runtimeSessions: 2,
  graphNodes: 3,
  graphEdges: 3,
  activities: 2,
}

test('the page mounts both solution graphs with state chips inside the solution view', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: [],
    limits,
    storage: new FakeStorage(),
  })

  const solution = findByClass(rootElement, 'wwc-strongflow-view-solution')
  const graphs = findAllByClass(solution, 'wwc-strongflow-graph')
  assert.equal(graphs.length, 2)
  assert.deepEqual(graphs.map(graph => graph.dataset.diagram), [
    'diagram:system-architecture',
    'diagram:process-flow',
  ])
  const architectureNodes = findAllByClass(graphs[0], 'wwc-strongflow-graph-node')
  assert.equal(architectureNodes.length, 3)
  const omitted = findAllByClass(graphs[0], 'wwc-strongflow-omitted')
  assert.equal(omitted.length > 0
    && omitted.some(note => /2 more diagram nodes not rendered\./u.test(note.textContent)), true)

  const stateLine = findByClass(solution, 'wwc-strongflow-solution-state')
  assert.match(stateLine.textContent, /Delivery executing/u)
  assert.match(stateLine.textContent, /solution review pending/u)
  assert.equal(stateLine.dataset.deliveryStatus, 'executing')
  assert.equal(stateLine.dataset.reviewStatus, 'pending')

  assert.notEqual(findByClass(rootElement, 'wwc-strongflow-view-execution'), null)
  mounted.close()
})

test('canonical diagram execution state reaches matching graph nodes as a text signal', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const current = projection()
  const reviewSetSha256 = 'a'.repeat(64)
  current.solutionReview.reviewSetSha256 = `sha256:${reviewSetSha256}`
  current.diagramExecution = {
    schemaVersion: 1,
    protocol: 'winwincode.diagram-execution-projection.v1',
    deliveryId: current.delivery.deliveryId,
    deliveryRevision: current.delivery.deliveryRevision,
    reviewSetSha256,
    state: 'executing',
    affectedFileCount: 1,
    architecture: {
      diagramId: current.solutionReview.architectureDiagram.id,
      kind: 'system-architecture',
      nodes: current.solutionReview.architectureDiagram.nodes.map(node => ({
        nodeId: node.id,
        state: node.id === 'node:1' ? 'affected-live' : 'normal',
        affectedFileCount: node.id === 'node:1' ? 1 : 0,
        fileIds: [],
      })),
    },
    process: {
      diagramId: current.solutionReview.processDiagram.id,
      kind: 'process-flow',
      nodes: current.solutionReview.processDiagram.nodes.map(node => ({
        nodeId: node.id,
        state: 'normal',
        affectedFileCount: 0,
        fileIds: [],
      })),
    },
    details: null,
    updatedAtMillis: 1,
  }
  const model = new FakeStrongFlowViewModel(state({ projection: current }))
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: [],
    limits,
    storage: new FakeStorage(),
  })

  const architecture = findAllByClass(
    findByClass(rootElement, 'wwc-strongflow-view-solution'),
    'wwc-strongflow-graph',
  )[0]
  const affectedNode = findAllByClass(architecture, 'wwc-strongflow-graph-node')
    .find(node => node.dataset.id === 'node:1')
  assert.match(
    textContentOf(affectedNode),
    /affected.*live/iu,
    'winwincode-4hc: canonical affected-live state needs a visible text signal',
  )
  assert.match(
    affectedNode.getAttribute('aria-label'),
    /affected.*live/iu,
    'winwincode-4hc: canonical affected-live state needs an accessible text signal',
  )
  affectedNode.emit('click')
  const detail = findByClass(architecture, 'wwc-strongflow-graph-detail').textContent
  assert.doesNotMatch(detail, /Task |Diff |Evidence /u)

  const mismatched = structuredClone(model.state)
  mismatched.projection.diagramExecution.deliveryRevision += 1
  assert.throws(
    () => model.publish(mismatched),
    /diagram execution facts do not match the current review cut/u,
  )
  mounted.close()
})

test('node selection links only exact canonical Task Attention Diff and Evidence facts', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const current = projection()
  const fileId = 'diagram-file:1'
  const evidenceId = 'evidence:1'
  current.evidence = [{
    id: evidenceId,
    type: 'diff',
    sourceRef: 'candidate.diff:1',
    candidateRef: 'git-candidate:1',
  }]
  const executionNode = node => ({
    nodeId: node.id,
    state: node.id === 'node:1' ? 'affected-finished' : 'normal',
    affectedFileCount: node.id === 'node:1' ? 1 : 0,
    fileIds: node.id === 'node:1' ? [fileId] : [],
  })
  current.diagramExecution = {
    schemaVersion: 1,
    protocol: 'winwincode.diagram-execution-projection.v1',
    deliveryId: current.delivery.deliveryId,
    deliveryRevision: current.delivery.deliveryRevision,
    reviewSetSha256: 'a'.repeat(64),
    state: 'execution-finished',
    architecture: {
      diagramId: current.solutionReview.architectureDiagram.id,
      kind: 'system-architecture',
      nodes: current.solutionReview.architectureDiagram.nodes.map(executionNode),
    },
    process: {
      diagramId: current.solutionReview.processDiagram.id,
      kind: 'process-flow',
      nodes: current.solutionReview.processDiagram.nodes.map(node => ({
        nodeId: node.id,
        state: 'normal',
        affectedFileCount: 0,
        fileIds: [],
      })),
    },
    affectedFileCount: 1,
    details: {
      files: [{ id: fileId, path: 'src/linked.ts', nodeIds: ['node:1'] }],
      provenance: { deliveryTaskId: 'task:1', evidenceRefIds: [evidenceId] },
    },
    updatedAtMillis: 2,
  }
  const model = new FakeStrongFlowViewModel(state({ projection: current }))
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: [],
    limits,
    storage: new FakeStorage(),
  })

  const architecture = findAllByClass(
    findByClass(rootElement, 'wwc-strongflow-view-solution'),
    'wwc-strongflow-graph',
  )[0]
  const node = findAllByClass(architecture, 'wwc-strongflow-graph-node')
    .find(candidate => candidate.dataset.id === 'node:1')
  node.emit('click')

  const taskRows = findByClass(rootElement, 'wwc-strongflow-task-list').children
  assert.equal(taskRows[0].dataset.diagramLinked, 'true')
  assert.equal(taskRows[1].dataset.diagramLinked, 'false')
  assert.equal(
    findByClass(rootElement, 'wwc-strongflow-attention-list').children[0]
      .dataset.diagramLinked,
    'true',
  )
  assert.equal(
    findByClass(rootElement, 'wwc-strongflow-candidate-host').dataset.diagramDiffPaths,
    'src/linked.ts',
  )
  const evidenceRows = findAllByClass(rootElement, 'wwc-strongflow-evidence')
    .flatMap(list => list.children)
  assert.equal(
    evidenceRows.filter(row => row.dataset.evidenceRefId === evidenceId)
      .every(row => row.dataset.diagramLinked === 'true'),
    true,
  )
  assert.match(
    findByClass(architecture, 'wwc-strongflow-graph-detail').textContent,
    /Task task:1.*Attention attention:1.*Diff src\/linked\.ts.*Evidence evidence:1/u,
  )
  mounted.close()
})

test('node selection omits canonical identities absent from the current delivery cut', () => {
  const document = new FakeDocument()
  const current = projection()
  const fileId = 'diagram-file:missing'
  const executionNode = node => ({
    nodeId: node.id,
    state: node.id === 'node:1' ? 'affected-finished' : 'normal',
    affectedFileCount: node.id === 'node:1' ? 1 : 0,
    fileIds: node.id === 'node:1' ? [fileId] : [],
  })
  current.delivery.tasks = []
  current.delivery.attention = []
  current.attention = []
  current.solutionReview.attentionItemId = 'attention:missing'
  current.diagramExecution = {
    schemaVersion: 1,
    protocol: 'winwincode.diagram-execution-projection.v1',
    deliveryId: current.delivery.deliveryId,
    deliveryRevision: current.delivery.deliveryRevision,
    reviewSetSha256: 'a'.repeat(64),
    state: 'execution-finished',
    architecture: {
      diagramId: current.solutionReview.architectureDiagram.id,
      kind: 'system-architecture',
      nodes: current.solutionReview.architectureDiagram.nodes.map(executionNode),
    },
    process: {
      diagramId: current.solutionReview.processDiagram.id,
      kind: 'process-flow',
      nodes: current.solutionReview.processDiagram.nodes.map(node => ({
        nodeId: node.id,
        state: 'normal',
        affectedFileCount: 0,
        fileIds: [],
      })),
    },
    affectedFileCount: 1,
    details: {
      files: [{ id: fileId, path: 'src/missing.ts', nodeIds: ['node:1'] }],
      provenance: {
        deliveryTaskId: 'task:missing',
        evidenceRefIds: ['evidence:missing'],
      },
    },
    updatedAtMillis: 2,
  }
  let selected = null
  const mounted = mountStrongFlowDiagrams({
    document,
    limits,
    onSelectNode: value => { selected = value },
  })
  mounted.update({ projection: current, narrow: false })

  findAllByClass(mounted.root, 'wwc-strongflow-graph-node')
    .find(node => node.dataset.id === 'node:1')
    .emit('click')
  assert.deepEqual(selected, {
    diagramId: current.solutionReview.architectureDiagram.id,
    nodeId: 'node:1',
    taskId: null,
    attentionItemIds: [],
    diffPaths: [],
    evidenceRefIds: [],
  })
  mounted.close()
})

test('a missing solution review exposes only the empty state and resets graph chrome', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const withoutReview = projection({ solutionReview: null })
  const model = new FakeStrongFlowViewModel(state({ projection: withoutReview }))
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: [],
    limits,
    storage: new FakeStorage(),
  })

  const solution = findByClass(rootElement, 'wwc-strongflow-view-solution')
  const graphs = findAllByClass(solution, 'wwc-strongflow-graph')
  assert.equal(findByClass(solution, 'wwc-strongflow-empty').hidden, false)
  assert.equal(
    graphs.every(graph => graph.hidden),
    true,
    'winwincode-7m1: empty reviews must not expose duplicate graph toolbars',
  )
  mounted.close()
})

test('two hundred runtime snapshots keep graph state and isolated session partitions', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: [],
    limits,
    storage: new FakeStorage(),
  })

  const graphs = findAllByClass(
    findByClass(rootElement, 'wwc-strongflow-view-solution'),
    'wwc-strongflow-graph',
  )
  const architectureNode = findAllByClass(graphs[0], 'wwc-strongflow-graph-node')[0]
  architectureNode.focus()
  architectureNode.emit('click')
  findByClass(graphs[0], 'wwc-strongflow-graph-zoom-in').emit('click')
  const diagramsRoot = findByClass(rootElement, 'wwc-strongflow-diagrams')
  const initialSessions = findAllByClass(rootElement, 'wwc-strongflow-execution-session')
  const untouchedSession = initialSessions[1]

  for (let index = 0; index < 200; index += 1) {
    const next = structuredClone(model.state)
    next.projection.runtime.sessions[0].asOfSequence = 100 + index
    next.projection.runtime.sessions[0].agents[0].status = index % 2 === 0 ? 'running' : 'waiting'
    next.status = index % 2 === 0 ? 'refreshing' : 'ready'
    model.publish(next)
  }

  assert.equal(
    findAllByClass(findByClass(rootElement, 'wwc-strongflow-view-solution'), 'wwc-strongflow-graph')[0],
    graphs[0],
  )
  assert.equal(
    findAllByClass(graphs[0], 'wwc-strongflow-graph-node')[0],
    architectureNode,
  )
  assert.equal(document.activeElement, architectureNode)
  assert.equal(architectureNode.getAttribute('aria-pressed'), 'true')
  assert.equal(
    findByClass(graphs[0], 'wwc-strongflow-graph-viewport').getAttribute('data-zoom'),
    '1.25',
  )
  assert.equal(findByClass(rootElement, 'wwc-strongflow-diagrams'), diagramsRoot)
  const finalSessions = findAllByClass(rootElement, 'wwc-strongflow-execution-session')
  assert.equal(
    finalSessions.includes(untouchedSession),
    true,
    'unchanged session partition keeps identity',
  )
  assert.equal(
    finalSessions[1] === untouchedSession,
    true,
    'winwincode-5nw: unchanged session stays in canonical snapshot order',
  )
  assert.match(
    textContentOf(finalSessions[0]),
    /Task task:1/u,
    'winwincode-5nw: changed session stays in canonical snapshot order',
  )
  mounted.close()
})

test('two hundred unrelated runtime snapshots perform zero graph DOM writes', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveries: [],
    limits,
    storage: new FakeStorage(),
  })

  const graphs = findAllByClass(
    findByClass(rootElement, 'wwc-strongflow-view-solution'),
    'wwc-strongflow-graph',
  )
  let writes = 0
  const observeWrites = node => {
    const setAttribute = node.setAttribute.bind(node)
    const removeAttribute = node.removeAttribute.bind(node)
    node.setAttribute = (...args) => {
      writes += 1
      setAttribute(...args)
    }
    node.removeAttribute = (...args) => {
      writes += 1
      removeAttribute(...args)
    }
    for (const child of node.children) observeWrites(child)
  }
  for (const graph of graphs) observeWrites(graph)

  for (let index = 0; index < 200; index += 1) {
    const next = structuredClone(model.state)
    next.projection.runtime.sessions[0].asOfSequence = 100 + index
    next.projection.metadata.revisions.runtime = 9 + index
    model.publish(next)
  }

  assert.equal(
    writes,
    0,
    'winwincode-a79: equivalent diagram props must cause zero graph DOM writes',
  )
  mounted.close()
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
  scrollTop = 0
  scrollLeft = 0
  clientWidth = 0
  clientHeight = 0
  scrollWidth = 0
  scrollHeight = 0
  offsetWidth = 0
  offsetHeight = 0
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

  contains(candidate) {
    if (candidate === this) return true
    return this.children.some(child => child.contains(candidate))
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

function findFirst(node, predicate) {
  if (predicate(node)) return node
  for (const child of node.children) {
    const match = findFirst(child, predicate)
    if (match !== null) return match
  }
  return null
}

/** Composite text of an element with structured children, mirroring real DOM textContent. */
function textContentOf(node) {
  let text = ''
  const visit = element => {
    if (element.children.length === 0) text += element.textContent
    else for (const child of element.children) visit(child)
  }
  visit(node)
  return text
}
