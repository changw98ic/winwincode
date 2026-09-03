// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode } from './components/mounted-view.js'

/** Deterministic geometry shared by every diagram graph instance. */
export const STRONGFLOW_DIAGRAM_GRAPH_COLUMN_WIDTH = 220
export const STRONGFLOW_DIAGRAM_GRAPH_ROW_HEIGHT = 120
/** Keeps absolutely centered nodes inside the scrollable canvas. */
export const STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET = 100
export const STRONGFLOW_DIAGRAM_GRAPH_ZOOM_STEP = 0.25
export const STRONGFLOW_DIAGRAM_GRAPH_ZOOM_MIN = 0.5
export const STRONGFLOW_DIAGRAM_GRAPH_ZOOM_MAX = 2.5

const NODE_KIND_ICONS: Readonly<Record<string, string>> = Object.freeze({
  interaction: '◉',
  'delivery-control': '⌂',
  execution: '⚙',
  repository: '▨',
  component: '▣',
  external: '⬡',
  'data-store': '▤',
  stage: '▶',
  decision: '◆',
})

export interface StrongFlowDiagramGraphNode {
  readonly id: string
  readonly label: string
  readonly description: string
  readonly kind: string
  readonly trustBoundary: string | null
  readonly unresolved: boolean
  readonly executionState?: 'normal' | 'affected-live' | 'affected-finished'
  readonly affectedFileCount?: number
  readonly fileIds?: readonly string[]
  readonly linkedTaskId?: string | null
  readonly linkedAttentionItemIds?: readonly string[]
  readonly linkedDiffPaths?: readonly string[]
  readonly linkedEvidenceRefIds?: readonly string[]
}

export interface StrongFlowDiagramGraphEdge {
  readonly id: string
  readonly from: string
  readonly to: string
  readonly label: string
}

export interface StrongFlowDiagramGraphProps {
  readonly id: string
  readonly title: string
  readonly nodes: readonly StrongFlowDiagramGraphNode[]
  readonly edges: readonly StrongFlowDiagramGraphEdge[]
  readonly narrow: boolean
  readonly onSelectNode?: (nodeId: string | null) => void
}

export interface StrongFlowDiagramGraphPosition {
  readonly rank: number
  readonly row: number
  readonly x: number
  readonly y: number
}

export interface StrongFlowDiagramGraphView {
  readonly root: HTMLElement
  update(props: Readonly<StrongFlowDiagramGraphProps>): void
  select(nodeId: string | null): void
  selectedNodeId(): string | null
  nodeElement(nodeId: string): HTMLButtonElement | null
  close(): void
}

export interface StrongFlowDiagramGraphMountOptions {
  readonly document: Document
  readonly props: Readonly<StrongFlowDiagramGraphProps>
}

interface MountedNode {
  readonly root: HTMLButtonElement
  readonly icon: HTMLElement
  readonly label: HTMLElement
  readonly onClick: () => void
  readonly onKeyDown: (event: KeyboardEvent) => void
  badge: HTMLElement | null
  executionBadge: HTMLElement | null
}

interface MountedEdge {
  readonly root: HTMLElement
  readonly line: HTMLElement
  readonly label: HTMLElement
}

interface MountedListRow {
  readonly root: HTMLElement
  readonly text: HTMLElement
}

interface MountedBoundary {
  readonly root: HTMLButtonElement
  readonly onClick: () => void
}

interface MountedGroup {
  readonly root: HTMLButtonElement
  readonly onClick: () => void
  readonly onKeyDown: (event: KeyboardEvent) => void
}

/**
 * Layer one approved diagram deterministically: the rank is the longest
 * acyclic edge path into a node, rows keep node-id order inside a rank, and
 * the same structure always produces the same geometry.
 */
export function strongFlowDiagramGraphLayout(
  nodes: readonly StrongFlowDiagramGraphNode[],
  edges: readonly StrongFlowDiagramGraphEdge[],
): ReadonlyMap<string, StrongFlowDiagramGraphPosition> {
  assertGraphIdentities(nodes, edges)
  const nodeIds = nodes.map(node => node.id).toSorted()
  const successors = new Map(nodeIds.map(id => [id, [] as string[]]))
  for (const edge of edges) successors.get(edge.from)?.push(edge.to)
  for (const adjacent of successors.values()) adjacent.sort()

  // Tarjan's SCC walk is seeded and traversed by canonical IDs, so every
  // cycle becomes one stable condensation node regardless of wire order.
  let nextIndex = 0
  const indexes = new Map<string, number>()
  const lowLinks = new Map<string, number>()
  const stack: string[] = []
  const onStack = new Set<string>()
  const components: string[][] = []
  const visit = (id: string): void => {
    const index = nextIndex
    nextIndex += 1
    indexes.set(id, index)
    lowLinks.set(id, index)
    stack.push(id)
    onStack.add(id)
    for (const successor of successors.get(id) ?? []) {
      if (!indexes.has(successor)) {
        visit(successor)
        lowLinks.set(id, Math.min(lowLinks.get(id)!, lowLinks.get(successor)!))
      } else if (onStack.has(successor)) {
        lowLinks.set(id, Math.min(lowLinks.get(id)!, indexes.get(successor)!))
      }
    }
    if (lowLinks.get(id) !== indexes.get(id)) return
    const component: string[] = []
    while (stack.length > 0) {
      const member = stack.pop()!
      onStack.delete(member)
      component.push(member)
      if (member === id) break
    }
    components.push(component.toSorted())
  }
  for (const id of nodeIds) if (!indexes.has(id)) visit(id)
  components.sort((left, right) => left[0]!.localeCompare(right[0]!))

  const componentByNode = new Map<string, number>()
  components.forEach((component, componentIndex) => {
    for (const id of component) componentByNode.set(id, componentIndex)
  })
  const componentPredecessors = new Map<number, Set<number>>(
    components.map((_, index) => [index, new Set<number>()]),
  )
  for (const edge of edges) {
    const from = componentByNode.get(edge.from)!
    const to = componentByNode.get(edge.to)!
    if (from !== to) componentPredecessors.get(to)!.add(from)
  }
  const componentRanks = new Map<number, number>()
  const componentRank = (component: number): number => {
    const memo = componentRanks.get(component)
    if (memo !== undefined) return memo
    let rank = 0
    for (const predecessor of [...componentPredecessors.get(component)!].sort()) {
      rank = Math.max(rank, componentRank(predecessor) + 1)
    }
    componentRanks.set(component, rank)
    return rank
  }
  const ranks = new Map<string, number>()
  components.forEach((component, index) => {
    const rank = componentRank(index)
    for (const id of component) ranks.set(id, rank)
  })

  const byRank = new Map<number, string[]>()
  for (const node of nodes) {
    const rank = ranks.get(node.id) ?? 0
    const members = byRank.get(rank) ?? []
    members.push(node.id)
    byRank.set(rank, members)
  }
  const layout = new Map<string, StrongFlowDiagramGraphPosition>()
  for (const rank of [...byRank.keys()].sort((left, right) => left - right)) {
    const members = (byRank.get(rank) ?? []).sort()
    members.forEach((id, row) => layout.set(id, {
      rank,
      row,
      x: rank * STRONGFLOW_DIAGRAM_GRAPH_COLUMN_WIDTH,
      y: row * STRONGFLOW_DIAGRAM_GRAPH_ROW_HEIGHT,
    }))
  }
  return layout
}

function assertGraphIdentities(
  nodes: readonly StrongFlowDiagramGraphNode[],
  edges: readonly StrongFlowDiagramGraphEdge[],
): void {
  const nodeIds = new Set<string>()
  for (const node of nodes) {
    if (nodeIds.has(node.id)) throw new TypeError(`duplicate diagram node identity: ${node.id}`)
    nodeIds.add(node.id)
  }
  const edgeIds = new Set<string>()
  for (const edge of edges) {
    if (edgeIds.has(edge.id)) throw new TypeError(`duplicate diagram edge identity: ${edge.id}`)
    edgeIds.add(edge.id)
    if (!nodeIds.has(edge.from) || !nodeIds.has(edge.to)) {
      throw new TypeError(`diagram edge ${edge.id} must join known nodes`)
    }
  }
}

function graphFingerprint(props: Readonly<StrongFlowDiagramGraphProps>): string {
  const nodes = props.nodes.toSorted((left, right) => left.id.localeCompare(right.id)).map(node => [
    node.id,
    node.label,
    node.description,
    node.kind,
    node.trustBoundary,
    node.unresolved,
    node.executionState ?? 'normal',
    node.affectedFileCount ?? 0,
    [...(node.fileIds ?? [])].toSorted(),
    node.linkedTaskId ?? null,
    [...(node.linkedAttentionItemIds ?? [])].toSorted(),
    [...(node.linkedDiffPaths ?? [])].toSorted(),
    [...(node.linkedEvidenceRefIds ?? [])].toSorted(),
  ])
  const edges = props.edges.toSorted((left, right) => left.id.localeCompare(right.id)).map(edge => [
    edge.id,
    edge.from,
    edge.to,
    edge.label,
  ])
  return JSON.stringify([props.id, props.title, props.narrow, nodes, edges])
}

function formatZoom(zoom: number): string {
  return String(Number(zoom.toFixed(2)))
}

function applyStyle(
  element: HTMLElement,
  properties: Readonly<Record<string, string>>,
): void {
  const style = element.style as CSSStyleDeclaration | undefined
  if (style === undefined || style === null) return
  for (const [name, value] of Object.entries(properties)) style.setProperty(name, value)
}

/** Mount one solution-review diagram as a keyed, keyboard-navigable graph. */
export function mountStrongFlowDiagramGraph(
  options: StrongFlowDiagramGraphMountOptions,
): StrongFlowDiagramGraphView {
  const document = options.document
  const root = document.createElement('section')
  root.className = 'wwc-strongflow-graph'

  const heading = document.createElement('h4')
  heading.className = 'wwc-strongflow-graph-heading'
  const toolbar = document.createElement('div')
  toolbar.className = 'wwc-strongflow-graph-toolbar'
  const toggleView = document.createElement('button')
  toggleView.type = 'button'
  toggleView.className = 'wwc-strongflow-graph-toggle-view'
  const zoomOut = document.createElement('button')
  zoomOut.type = 'button'
  zoomOut.className = 'wwc-strongflow-graph-zoom-out'
  zoomOut.textContent = 'Zoom out'
  zoomOut.setAttribute('aria-label', `Zoom out ${options.props.title} graph`)
  const zoomIn = document.createElement('button')
  zoomIn.type = 'button'
  zoomIn.className = 'wwc-strongflow-graph-zoom-in'
  zoomIn.textContent = 'Zoom in'
  zoomIn.setAttribute('aria-label', `Zoom in ${options.props.title} graph`)
  const fit = document.createElement('button')
  fit.type = 'button'
  fit.className = 'wwc-strongflow-graph-fit'
  fit.textContent = 'Reset layout'
  fit.setAttribute('aria-label', `Restore the fitted ${options.props.title} layout`)
  toolbar.append(toggleView, zoomOut, zoomIn, fit)

  const boundaries = document.createElement('div')
  boundaries.className = 'wwc-strongflow-graph-boundaries'
  const viewport = document.createElement('div')
  viewport.className = 'wwc-strongflow-graph-viewport'
  viewport.setAttribute('role', 'group')
  viewport.tabIndex = 0
  const edgesRoot = document.createElement('div')
  edgesRoot.className = 'wwc-strongflow-graph-edges'
  edgesRoot.setAttribute('role', 'list')
  const canvas = document.createElement('div')
  canvas.className = 'wwc-strongflow-graph-canvas'
  canvas.append(edgesRoot)
  viewport.append(canvas)
  const overview = document.createElement('button')
  overview.type = 'button'
  overview.className = 'wwc-strongflow-graph-overview'
  const overviewCanvas = document.createElement('span')
  overviewCanvas.className = 'wwc-strongflow-graph-overview-canvas'
  const overviewEdges = document.createElement('span')
  overviewEdges.className = 'wwc-strongflow-graph-overview-edges'
  overviewEdges.setAttribute('aria-hidden', 'true')
  overviewCanvas.append(overviewEdges)
  overview.append(overviewCanvas)
  const detail = document.createElement('p')
  detail.className = 'wwc-strongflow-graph-detail'
  detail.setAttribute('aria-live', 'polite')
  const listRoot = document.createElement('ul')
  listRoot.className = 'wwc-strongflow-graph-list'
  root.append(heading, toolbar, boundaries, viewport, overview, detail, listRoot)

  let open = true
  let currentProps: Readonly<StrongFlowDiagramGraphProps> = options.props
  const nodeViews = new Map<string, MountedNode>()
  const edgeViews = new Map<string, MountedEdge>()
  const listRows = new Map<string, MountedListRow>()
  const boundaryHeaders = new Map<string, MountedBoundary>()
  const groupChips = new Map<string, MountedGroup>()
  const overviewNodes = new Map<string, HTMLElement>()
  const overviewEdgeViews = new Map<string, HTMLElement>()
  const collapsedBoundaries = new Set<string>()
  let selectedNodeId: string | null = null
  let zoom = 1
  let viewMode: 'graph' | 'list' = 'graph'
  let layout = new Map<string, StrongFlowDiagramGraphPosition>()
  let nodeOrder: readonly StrongFlowDiagramGraphNode[] = []
  let lastFingerprint: string | null = null

  const onNodeClick = (nodeId: string) => () => {
    select(selectedNodeId === nodeId ? null : nodeId)
  }

  const onNodeKeyDown = (origin: HTMLButtonElement) => (event: KeyboardEvent) => {
    const keys = ['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End']
    if (!keys.includes(event.key)) return
    event.preventDefault?.()
    if (event.key === 'Home' || event.key === 'End') {
      const targets = navigationTargets()
      const target = event.key === 'Home' ? targets[0] : targets.at(-1)
      if (target !== undefined) focusNavigationTarget(target)
      return
    }
    moveFocusFrom(origin, event.key)
  }

  const onViewportKeyDown = (event: KeyboardEvent) => {
    if (event.key === '+') {
      event.preventDefault?.()
      applyZoom(zoom + STRONGFLOW_DIAGRAM_GRAPH_ZOOM_STEP)
    } else if (event.key === '-') {
      event.preventDefault?.()
      applyZoom(zoom - STRONGFLOW_DIAGRAM_GRAPH_ZOOM_STEP)
    } else if (event.key === '0') {
      event.preventDefault?.()
      fitLayout()
    }
  }

  const onToggleViewClick = () => {
    viewMode = viewMode === 'graph' ? 'list' : 'graph'
    renderViewState()
  }
  const onZoomInClick = () => applyZoom(zoom + STRONGFLOW_DIAGRAM_GRAPH_ZOOM_STEP)
  const onZoomOutClick = () => applyZoom(zoom - STRONGFLOW_DIAGRAM_GRAPH_ZOOM_STEP)
  const onFitClick = () => fitLayout()
  const onOverviewClick = () => fitLayout()

  toggleView.addEventListener('click', onToggleViewClick)
  zoomIn.addEventListener('click', onZoomInClick)
  zoomOut.addEventListener('click', onZoomOutClick)
  fit.addEventListener('click', onFitClick)
  overview.addEventListener('click', onOverviewClick)
  viewport.addEventListener('keydown', onViewportKeyDown)

  function nodeById(id: string): StrongFlowDiagramGraphNode | undefined {
    return nodeOrder.find(node => node.id === id)
  }

  function navigationTargets(): readonly HTMLButtonElement[] {
    const targets: HTMLButtonElement[] = []
    for (const node of nodeOrder) {
      const view = nodeViews.get(node.id)
      if (view === undefined || view.root.hidden) continue
      targets.push(view.root)
    }
    for (const chip of groupChips.values()) targets.push(chip.root)
    return targets
  }

  function focusNavigationTarget(target: HTMLButtonElement): void {
    for (const candidate of navigationTargets()) {
      candidate.tabIndex = candidate === target ? 0 : -1
    }
    target.focus()
  }

  function moveFocusFrom(origin: HTMLButtonElement, key: string): void {
    const originX = Number(origin.dataset.x ?? '0')
    const originY = Number(origin.dataset.y ?? '0')
    const horizontal = key === 'ArrowLeft' || key === 'ArrowRight'
    let best: HTMLButtonElement | undefined
    let bestDistance = Number.POSITIVE_INFINITY
    for (const candidate of navigationTargets()) {
      if (candidate === origin || candidate.hidden) continue
      const candidateX = Number(candidate.dataset.x ?? '0')
      const candidateY = Number(candidate.dataset.y ?? '0')
      const dx = candidateX - originX
      const dy = candidateY - originY
      const forward = key === 'ArrowRight' || key === 'ArrowDown'
      const along = horizontal ? dx : dy
      if (forward ? along <= 0 : along >= 0) continue
      const distance = Math.abs(dx) + Math.abs(dy)
      if (distance < bestDistance) {
        best = candidate
        bestDistance = distance
      }
    }
    if (best !== undefined) focusNavigationTarget(best)
  }

  function boundaryOfNode(nodeId: string): string | null {
    return nodeById(nodeId)?.trustBoundary ?? null
  }

  function applyZoom(next: number): void {
    zoom = Math.min(
      STRONGFLOW_DIAGRAM_GRAPH_ZOOM_MAX,
      Math.max(STRONGFLOW_DIAGRAM_GRAPH_ZOOM_MIN, Number(next.toFixed(2))),
    )
    viewport.setAttribute('data-zoom', formatZoom(zoom))
    applyStyle(canvas, { transform: `scale(${formatZoom(zoom)})` })
  }

  function fitLayout(): void {
    const fittedZoom = viewport.clientWidth > 0 && viewport.clientHeight > 0
      && canvas.scrollWidth > 0 && canvas.scrollHeight > 0
      ? Math.min(
        1,
        viewport.clientWidth / canvas.scrollWidth,
        viewport.clientHeight / canvas.scrollHeight,
      )
      : 1
    applyZoom(fittedZoom)
    viewport.scrollTop = 0
    viewport.scrollLeft = 0
  }

  function renderViewState(): void {
    toggleView.textContent = viewMode === 'graph'
      ? 'Switch to list view'
      : 'Switch to graph view'
    toggleView.setAttribute('aria-pressed', String(viewMode === 'list'))
    viewport.hidden = viewMode === 'list'
    listRoot.hidden = viewMode === 'graph'
  }

  function nodeAriaLabel(node: StrongFlowDiagramGraphNode): string {
    const parts = [node.label, node.kind]
    if (node.trustBoundary !== null) parts.push(node.trustBoundary)
    if (node.unresolved) parts.push('Unresolved')
    if (node.executionState === 'affected-live') {
      parts.push(`Affected live, ${String(node.affectedFileCount ?? 0)} files`)
    } else if (node.executionState === 'affected-finished') {
      parts.push(`Affected finished, ${String(node.affectedFileCount ?? 0)} files`)
    }
    return parts.join(', ')
  }

  function ensureBadge(node: StrongFlowDiagramGraphNode, view: MountedNode): void {
    if (node.unresolved && view.badge === null) {
      const badge = document.createElement('span')
      badge.className = 'wwc-strongflow-graph-node-badge'
      badge.textContent = 'Unresolved'
      view.root.append(badge)
      view.badge = badge
    } else if (!node.unresolved && view.badge !== null) {
      view.badge.remove()
      view.badge = null
    }
  }

  function ensureExecutionBadge(
    node: StrongFlowDiagramGraphNode,
    view: MountedNode,
  ): void {
    const state = node.executionState ?? 'normal'
    if (state === 'normal') {
      view.executionBadge?.remove()
      view.executionBadge = null
      return
    }
    if (view.executionBadge === null) {
      const badge = document.createElement('span')
      badge.className = 'wwc-strongflow-graph-node-execution'
      view.root.append(badge)
      view.executionBadge = badge
    }
    const prefix = state === 'affected-live' ? '● Affected live' : '✓ Affected finished'
    const count = node.affectedFileCount ?? 0
    view.executionBadge.textContent = `${prefix} · ${String(count)} ${count === 1 ? 'file' : 'files'}`
  }

  function syncNode(node: StrongFlowDiagramGraphNode): MountedNode {
    let view = nodeViews.get(node.id)
    if (view === undefined) {
      const root = document.createElement('button')
      root.type = 'button'
      root.className = 'wwc-strongflow-graph-node'
      root.dataset.id = node.id
      root.tabIndex = -1
      const onClick = onNodeClick(node.id)
      const onKeyDown = onNodeKeyDown(root)
      root.addEventListener('click', onClick)
      root.addEventListener('keydown', onKeyDown)
      const icon = document.createElement('span')
      icon.className = 'wwc-strongflow-graph-node-icon'
      icon.setAttribute('aria-hidden', 'true')
      const label = document.createElement('span')
      label.className = 'wwc-strongflow-graph-node-label'
      root.append(icon, label)
      view = {
        root,
        icon,
        label,
        onClick,
        onKeyDown,
        badge: null,
        executionBadge: null,
      }
      nodeViews.set(node.id, view)
      canvas.append(root)
    }
    const position = layout.get(node.id)
    view.root.dataset.kind = node.kind
    view.root.dataset.unresolved = String(node.unresolved)
    view.root.dataset.executionState = node.executionState ?? 'normal'
    view.root.dataset.trustBoundary = node.trustBoundary ?? ''
    if (node.trustBoundary === null) delete view.root.dataset.trustBoundary
    view.root.dataset.x = String(position?.x ?? 0)
    view.root.dataset.y = String(position?.y ?? 0)
    applyStyle(view.root, {
      left: `${String((position?.x ?? 0) + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET)}px`,
      top: `${String((position?.y ?? 0) + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET)}px`,
    })
    view.root.setAttribute('aria-pressed', String(selectedNodeId === node.id))
    view.root.setAttribute('aria-label', nodeAriaLabel(node))
    view.icon.textContent = NODE_KIND_ICONS[node.kind] ?? '▢'
    view.label.textContent = node.label
    ensureBadge(node, view)
    ensureExecutionBadge(node, view)
    return view
  }

  function syncEdge(
    edge: StrongFlowDiagramGraphEdge,
    nodesById: ReadonlyMap<string, StrongFlowDiagramGraphNode>,
  ): MountedEdge {
    let view = edgeViews.get(edge.id)
    if (view === undefined) {
      const root = document.createElement('div')
      root.className = 'wwc-strongflow-graph-edge'
      root.dataset.id = edge.id
      root.setAttribute('role', 'listitem')
      const line = document.createElement('span')
      line.className = 'wwc-strongflow-graph-edge-line'
      line.setAttribute('aria-hidden', 'true')
      const label = document.createElement('span')
      label.className = 'wwc-strongflow-graph-edge-label'
      root.append(line, label)
      view = { root, line, label }
      edgeViews.set(edge.id, view)
      edgesRoot.append(root)
    }
    view.root.dataset.from = edge.from
    view.root.dataset.to = edge.to
    const fromLabel = nodesById.get(edge.from)?.label ?? edge.from
    const toLabel = nodesById.get(edge.to)?.label ?? edge.to
    view.root.setAttribute('aria-label', `${fromLabel} → ${toLabel}: ${edge.label}`)
    view.label.textContent = edge.label
    return view
  }

  function syncListRow(
    node: StrongFlowDiagramGraphNode,
    nodesById: ReadonlyMap<string, StrongFlowDiagramGraphNode>,
    connections: readonly StrongFlowDiagramGraphEdge[],
  ): MountedListRow {
    let row = listRows.get(node.id)
    if (row === undefined) {
      const root = document.createElement('li')
      root.className = 'wwc-strongflow-graph-list-node'
      root.dataset.id = node.id
      const text = document.createElement('span')
      text.className = 'wwc-strongflow-graph-list-text'
      root.append(text)
      row = { root, text }
      listRows.set(node.id, row)
      listRoot.append(root)
    }
    row.root.dataset.kind = node.kind
    row.root.dataset.unresolved = String(node.unresolved)
    row.root.dataset.executionState = node.executionState ?? 'normal'
    const lines = [`${node.label} — ${node.kind}`]
    if (node.trustBoundary !== null) lines.push(node.trustBoundary)
    if (node.unresolved) lines.push('Unresolved')
    if (node.executionState === 'affected-live') {
      lines.push(`Affected live — ${String(node.affectedFileCount ?? 0)} files`)
    } else if (node.executionState === 'affected-finished') {
      lines.push(`Affected finished — ${String(node.affectedFileCount ?? 0)} files`)
    }
    lines.push(node.description)
    for (const edge of connections) {
      const fromLabel = nodesById.get(edge.from)?.label ?? edge.from
      const toLabel = nodesById.get(edge.to)?.label ?? edge.to
      lines.push(`${fromLabel} → ${toLabel}: ${edge.label}`)
    }
    if (node.linkedTaskId !== undefined && node.linkedTaskId !== null) {
      lines.push(`Linked Task ${node.linkedTaskId}`)
    }
    for (const attentionId of node.linkedAttentionItemIds ?? []) {
      lines.push(`Linked Attention ${attentionId}`)
    }
    for (const path of node.linkedDiffPaths ?? []) lines.push(`Linked Diff ${path}`)
    for (const evidenceId of node.linkedEvidenceRefIds ?? []) {
      lines.push(`Linked Evidence ${evidenceId}`)
    }
    row.text.textContent = lines.join('. ')
    return row
  }

  function ensureBoundaryHeader(boundary: string, count: number): HTMLButtonElement {
    let view = boundaryHeaders.get(boundary)
    if (view === undefined) {
      const header = document.createElement('button')
      header.type = 'button'
      header.className = 'wwc-strongflow-graph-boundary'
      header.dataset.boundary = boundary
      const onClick = () => toggleBoundary(boundary)
      header.addEventListener('click', onClick)
      view = { root: header, onClick }
      boundaryHeaders.set(boundary, view)
      boundaries.append(header)
    }
    const header = view.root
    header.setAttribute('aria-expanded', String(!collapsedBoundaries.has(boundary)))
    header.setAttribute('aria-label', `Trust boundary ${boundary}, ${String(count)} nodes`)
    header.textContent = collapsedBoundaries.has(boundary)
      ? `Expand ${boundary} (${String(count)})`
      : `Collapse ${boundary} (${String(count)})`
    return header
  }

  function ensureGroupChip(
    boundary: string,
    count: number,
    position: StrongFlowDiagramGraphPosition,
  ): HTMLButtonElement {
    let view = groupChips.get(boundary)
    if (view === undefined) {
      const chip = document.createElement('button')
      chip.type = 'button'
      chip.className = 'wwc-strongflow-graph-group'
      chip.dataset.boundary = boundary
      chip.tabIndex = -1
      const onChipKeyDown = onNodeKeyDown(chip)
      const onClick = () => toggleBoundary(boundary)
      chip.addEventListener('click', onClick)
      chip.addEventListener('keydown', onChipKeyDown)
      view = { root: chip, onClick, onKeyDown: onChipKeyDown }
      groupChips.set(boundary, view)
      canvas.append(chip)
    }
    const chip = view.root
    chip.dataset.x = String(position.x)
    chip.dataset.y = String(position.y)
    applyStyle(chip, {
      left: `${String(position.x + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET)}px`,
      top: `${String(position.y + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET)}px`,
    })
    chip.setAttribute('aria-label', `${boundary}, ${String(count)} nodes, collapsed trust boundary`)
    chip.textContent = `${boundary} (${String(count)})`
    return chip
  }

  function toggleBoundary(boundary: string): void {
    const wasCollapsed = collapsedBoundaries.has(boundary)
    const memberViews = (boundaryMembers().get(boundary) ?? [])
      .map(member => nodeViews.get(member.id)?.root)
      .filter((node): node is HTMLButtonElement => node !== undefined)
    const chip = groupChips.get(boundary)?.root
    const ownsRovingAnchor = memberViews.some(node => node.tabIndex === 0)
      || chip?.tabIndex === 0
    const ownsFocus = memberViews.includes(document.activeElement as HTMLButtonElement)
      || document.activeElement === chip
    if (wasCollapsed) collapsedBoundaries.delete(boundary)
    else collapsedBoundaries.add(boundary)
    renderGraphState()
    const nextTarget = wasCollapsed
      ? memberViews[0]
      : groupChips.get(boundary)?.root
    if ((ownsFocus || ownsRovingAnchor) && nextTarget !== undefined) {
      focusNavigationTarget(nextTarget)
    } else {
      ensureNavigationAnchor()
    }
  }

  function ensureNavigationAnchor(): void {
    const targets = navigationTargets()
    const current = targets.find(target => target.tabIndex === 0)
    const anchor = current ?? targets[0]
    for (const target of targets) target.tabIndex = target === anchor ? 0 : -1
  }

  function collapsedNodeIds(): Set<string> {
    const hidden = new Set<string>()
    for (const [boundary, members] of boundaryMembers()) {
      if (!collapsedBoundaries.has(boundary)) continue
      for (const member of members) hidden.add(member.id)
    }
    return hidden
  }

  function boundaryMembers(): ReadonlyMap<string, readonly StrongFlowDiagramGraphNode[]> {
    const groups = new Map<string, StrongFlowDiagramGraphNode[]>()
    for (const node of nodeOrder) {
      if (node.trustBoundary === null) continue
      const members = groups.get(node.trustBoundary) ?? []
      members.push(node)
      groups.set(node.trustBoundary, members)
    }
    return groups
  }

  function renderGraphState(): void {
    const hiddenNodes = collapsedNodeIds()
    const groups = boundaryMembers()
    const liveChips = new Set<string>()
    for (const [boundary, members] of groups) {
      ensureBoundaryHeader(boundary, members.length)
      if (!collapsedBoundaries.has(boundary)) continue
      const positions = members
        .map(member => layout.get(member.id))
        .filter((position): position is StrongFlowDiagramGraphPosition => position !== undefined)
      const anchor = positions[0] ?? { rank: 0, row: 0, x: 0, y: 0 }
      ensureGroupChip(boundary, members.length, anchor)
      liveChips.add(boundary)
    }
    for (const [boundary, chip] of groupChips) {
      if (liveChips.has(boundary)) continue
      chip.root.removeEventListener('click', chip.onClick)
      chip.root.removeEventListener('keydown', chip.onKeyDown)
      chip.root.remove()
      groupChips.delete(boundary)
    }
    for (const [boundary, view] of boundaryHeaders) {
      if (groups.has(boundary)) continue
      view.root.removeEventListener('click', view.onClick)
      view.root.remove()
      boundaryHeaders.delete(boundary)
    }
    for (const boundary of [...groups.keys()].toSorted()) {
      const header = boundaryHeaders.get(boundary)?.root
      if (header !== undefined) boundaries.append(header)
    }
    for (const boundary of [...liveChips].toSorted()) {
      const chip = groupChips.get(boundary)?.root
      if (chip !== undefined) canvas.append(chip)
    }
    for (const node of nodeOrder) {
      const view = nodeViews.get(node.id)
      if (view === undefined) continue
      view.root.hidden = hiddenNodes.has(node.id)
    }
    const chipPositions = new Map<string, StrongFlowDiagramGraphPosition>()
    for (const boundary of liveChips) {
      const chip = groupChips.get(boundary)?.root
      if (chip === undefined) continue
      chipPositions.set(boundary, {
        rank: 0,
        row: 0,
        x: Number(chip.dataset.x ?? '0'),
        y: Number(chip.dataset.y ?? '0'),
      })
    }
    for (const [edgeId, view] of edgeViews) {
      const edge = currentProps.edges.find(candidate => candidate.id === edgeId)
      if (edge === undefined) continue
      const fromNode = nodeViews.get(edge.from)
      const toNode = nodeViews.get(edge.to)
      const fromBoundary = boundaryOfNode(edge.from)
      const toBoundary = boundaryOfNode(edge.to)
      const fromCollapsed = fromBoundary !== null && collapsedBoundaries.has(fromBoundary)
      const toCollapsed = toBoundary !== null && collapsedBoundaries.has(toBoundary)
      if (fromNode === undefined || toNode === undefined) {
        view.root.hidden = true
        continue
      }
      const fromIdentity = fromCollapsed && fromBoundary !== null
        ? `boundary:${fromBoundary}`
        : `node:${edge.from}`
      const toIdentity = toCollapsed && toBoundary !== null
        ? `boundary:${toBoundary}`
        : `node:${edge.to}`
      view.root.hidden = fromIdentity === toIdentity
      const fromPosition = fromNode.root.hidden && fromCollapsed && fromBoundary !== null
        ? chipPositions.get(fromBoundary) ?? layout.get(edge.from)
        : layout.get(edge.from)
      const toPosition = toNode.root.hidden && toCollapsed && toBoundary !== null
        ? chipPositions.get(toBoundary) ?? layout.get(edge.to)
        : layout.get(edge.to)
      const fromElement = fromCollapsed && fromBoundary !== null
        ? groupChips.get(fromBoundary)?.root ?? fromNode.root
        : fromNode.root
      const toElement = toCollapsed && toBoundary !== null
        ? groupChips.get(toBoundary)?.root ?? toNode.root
        : toNode.root
      const fromPoint = fromPosition ?? { x: 0, y: 0, rank: 0, row: 0 }
      const toPoint = toPosition ?? { x: 0, y: 0, rank: 0, row: 0 }
      const fromX = fromPoint.x + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET
        + fromElement.offsetWidth / 2
      const fromY = fromPoint.y + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET
        + fromElement.offsetHeight / 2
      const toX = toPoint.x + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET
        + toElement.offsetWidth / 2
      const toY = toPoint.y + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET
        + toElement.offsetHeight / 2
      const left = Math.min(fromX, toX)
      const top = Math.min(fromY, toY)
      const deltaX = toX - fromX
      const deltaY = toY - fromY
      const width = Math.max(1, Math.abs(deltaX))
      const height = Math.max(1, Math.abs(deltaY))
      const distance = Math.sqrt(deltaX ** 2 + deltaY ** 2)
      const angle = Math.atan2(deltaY, deltaX) * 180 / Math.PI
      const midpointX = Math.round((fromX + toX) / 2)
      const midpointY = Math.round((fromY + toY) / 2)
      view.root.dataset.x = String(midpointX)
      view.root.dataset.y = String(midpointY)
      applyStyle(view.root, {
        left: `${String(left)}px`,
        top: `${String(top)}px`,
        width: `${String(width)}px`,
        height: `${String(height)}px`,
      })
      applyStyle(view.line, {
        left: `${String(fromX - left)}px`,
        top: `${String(fromY - top)}px`,
        width: `${String(distance)}px`,
        transform: `rotate(${String(angle)}deg)`,
      })
      applyStyle(view.label, {
        left: `${String(midpointX - left)}px`,
        top: `${String(midpointY - top)}px`,
      })
    }
  }

  function renderOverview(
    nodes: readonly StrongFlowDiagramGraphNode[],
    edges: readonly StrongFlowDiagramGraphEdge[],
  ): void {
    const liveNodeIds = new Set(nodes.map(node => node.id))
    const liveEdgeIds = new Set(edges.map(edge => edge.id))
    const maxX = Math.max(1, ...nodes.map(node => layout.get(node.id)?.x ?? 0))
    const maxY = Math.max(1, ...nodes.map(node => layout.get(node.id)?.y ?? 0))
    overview.setAttribute(
      'aria-label',
      `Fit ${currentProps.title} overview, ${String(nodes.length)} nodes and ${String(edges.length)} connections`,
    )
    for (const node of nodes) {
      let marker = overviewNodes.get(node.id)
      if (marker === undefined) {
        marker = document.createElement('span')
        marker.className = 'wwc-strongflow-graph-overview-node'
        marker.dataset.id = node.id
        marker.setAttribute('aria-hidden', 'true')
        overviewNodes.set(node.id, marker)
        overviewCanvas.append(marker)
      }
      const position = layout.get(node.id) ?? { x: 0, y: 0 }
      applyStyle(marker, {
        left: `${String(position.x / maxX * 100)}%`,
        top: `${String(position.y / maxY * 100)}%`,
      })
      overviewCanvas.append(marker)
    }
    for (const [id, marker] of overviewNodes) {
      if (liveNodeIds.has(id)) continue
      marker.remove()
      overviewNodes.delete(id)
    }
    for (const edge of edges) {
      let line = overviewEdgeViews.get(edge.id)
      if (line === undefined) {
        line = document.createElement('span')
        line.className = 'wwc-strongflow-graph-overview-edge'
        line.dataset.id = edge.id
        line.setAttribute('aria-hidden', 'true')
        overviewEdgeViews.set(edge.id, line)
        overviewEdges.append(line)
      }
      const from = layout.get(edge.from) ?? { x: 0, y: 0 }
      const to = layout.get(edge.to) ?? { x: 0, y: 0 }
      const fromX = from.x / maxX * 100
      const fromY = from.y / maxY * 100
      const toX = to.x / maxX * 100
      const toY = to.y / maxY * 100
      const deltaX = toX - fromX
      const deltaY = toY - fromY
      applyStyle(line, {
        left: `${String(fromX)}%`,
        top: `${String(fromY)}%`,
        width: `${String(Math.sqrt(deltaX ** 2 + deltaY ** 2))}%`,
        transform: `rotate(${String(Math.atan2(deltaY, deltaX) * 180 / Math.PI)}deg)`,
      })
      overviewEdges.append(line)
    }
    for (const [id, line] of overviewEdgeViews) {
      if (liveEdgeIds.has(id)) continue
      line.remove()
      overviewEdgeViews.delete(id)
    }
  }

  function renderSelection(): void {
    for (const [nodeId, view] of nodeViews) {
      view.root.setAttribute('aria-pressed', String(selectedNodeId === nodeId))
    }
    if (selectedNodeId === null) {
      detail.textContent = 'Select a node to read its responsibility.'
      return
    }
    const node = nodeById(selectedNodeId)
    if (node === undefined) {
      detail.textContent = 'Select a node to read its responsibility.'
      return
    }
    const parts = [node.label, node.kind]
    if (node.trustBoundary !== null) parts.push(node.trustBoundary)
    if (node.unresolved) parts.push('Unresolved')
    if (node.executionState === 'affected-live') parts.push('Affected live')
    if (node.executionState === 'affected-finished') parts.push('Affected finished')
    const links: string[] = []
    if (node.linkedTaskId !== undefined && node.linkedTaskId !== null) {
      links.push(`Task ${node.linkedTaskId}`)
    }
    for (const attentionId of node.linkedAttentionItemIds ?? []) {
      links.push(`Attention ${attentionId}`)
    }
    for (const path of node.linkedDiffPaths ?? []) links.push(`Diff ${path}`)
    for (const evidenceId of node.linkedEvidenceRefIds ?? []) {
      links.push(`Evidence ${evidenceId}`)
    }
    detail.textContent = `${parts.join(' · ')}. ${node.description}${
      links.length === 0 ? '' : ` Linked facts: ${links.join(', ')}.`
    }`
  }

  function select(nodeId: string | null): void {
    const changed = selectedNodeId !== nodeId
    selectedNodeId = nodeId
    renderSelection()
    if (changed) currentProps.onSelectNode?.(nodeId)
  }

  function update(props: Readonly<StrongFlowDiagramGraphProps>): void {
    assertMounted(open, 'StrongFlowDiagramGraph')
    assertGraphIdentities(props.nodes, props.edges)
    const fingerprint = graphFingerprint(props)
    const sameGraph = fingerprint === lastFingerprint
    const previousId = currentProps.id
    const resetSelection = props.id !== previousId && selectedNodeId !== null
    currentProps = props
    if (sameGraph) return
    if (props.id !== previousId) {
      selectedNodeId = null
      collapsedBoundaries.clear()
      zoom = 1
      viewMode = 'graph'
    }
    lastFingerprint = fingerprint
    root.dataset.diagram = props.id
    heading.textContent = props.title
    viewport.setAttribute('aria-label', `${props.title} graph viewport`)
    viewport.setAttribute('data-viewport', props.narrow ? 'narrow' : 'wide')

    layout = new Map(strongFlowDiagramGraphLayout(props.nodes, props.edges))
    const nodesByInputId = new Map(props.nodes.map(node => [node.id, node]))
    nodeOrder = [...layout.keys()].map(id => nodesByInputId.get(id)!)
    const edgeOrder = props.edges.toSorted((left, right) => (
      (layout.get(left.from)?.rank ?? 0) - (layout.get(right.from)?.rank ?? 0)
      || (layout.get(left.to)?.rank ?? 0) - (layout.get(right.to)?.rank ?? 0)
      || left.id.localeCompare(right.id)
    ))
    const nodesById = new Map(props.nodes.map(node => [node.id, node]))
    const extentX = Math.max(0, ...nodeOrder.map(node => layout.get(node.id)?.x ?? 0))
      + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET * 2
    const extentY = Math.max(0, ...nodeOrder.map(node => layout.get(node.id)?.y ?? 0))
      + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET * 2
    applyStyle(canvas, { width: `${String(extentX)}px`, height: `${String(extentY)}px` })
    for (const node of nodeOrder) {
      const view = syncNode(node)
      canvas.append(view.root)
    }
    for (const [nodeId, view] of nodeViews) {
      if (nodesById.has(nodeId)) continue
      view.root.removeEventListener('click', view.onClick)
      view.root.removeEventListener('keydown', view.onKeyDown)
      view.root.remove()
      nodeViews.delete(nodeId)
    }
    const liveEdgeIds = new Set(edgeOrder.map(edge => edge.id))
    for (const edge of edgeOrder) {
      const view = syncEdge(edge, nodesById)
      edgesRoot.append(view.root)
    }
    for (const [edgeId, view] of edgeViews) {
      if (liveEdgeIds.has(edgeId)) continue
      view.root.remove()
      edgeViews.delete(edgeId)
    }
    for (const node of nodeOrder) {
      const connections = edgeOrder.filter(edge => edge.from === node.id || edge.to === node.id)
      const row = syncListRow(node, nodesById, connections)
      listRoot.append(row.root)
    }
    for (const [nodeId, row] of listRows) {
      if (nodesById.has(nodeId)) continue
      row.root.remove()
      listRows.delete(nodeId)
    }
    const retainedBoundaries = new Set(
      props.nodes
        .map(node => node.trustBoundary)
        .filter((boundary): boundary is string => boundary !== null),
    )
    for (const boundary of [...collapsedBoundaries]) {
      if (retainedBoundaries.has(boundary)) continue
      collapsedBoundaries.delete(boundary)
    }
    if (selectedNodeId !== null && !nodesById.has(selectedNodeId)) select(null)
    renderGraphState()
    renderOverview(nodeOrder, edgeOrder)
    renderSelection()
    renderViewState()
    applyZoom(zoom)
    ensureNavigationAnchor()
    if (resetSelection) currentProps.onSelectNode?.(null)
  }

  update(currentProps)

  return {
    root,
    update,
    select,
    selectedNodeId() {
      assertMounted(open, 'StrongFlowDiagramGraph')
      return selectedNodeId
    },
    nodeElement(nodeId) {
      assertMounted(open, 'StrongFlowDiagramGraph')
      return nodeViews.get(nodeId)?.root ?? null
    },
    close() {
      if (!open) return
      open = false
      toggleView.removeEventListener('click', onToggleViewClick)
      zoomIn.removeEventListener('click', onZoomInClick)
      zoomOut.removeEventListener('click', onZoomOutClick)
      fit.removeEventListener('click', onFitClick)
      overview.removeEventListener('click', onOverviewClick)
      viewport.removeEventListener('keydown', onViewportKeyDown)
      for (const view of nodeViews.values()) {
        view.root.removeEventListener('click', view.onClick)
        view.root.removeEventListener('keydown', view.onKeyDown)
      }
      for (const view of boundaryHeaders.values()) {
        view.root.removeEventListener('click', view.onClick)
      }
      for (const view of groupChips.values()) {
        view.root.removeEventListener('click', view.onClick)
        view.root.removeEventListener('keydown', view.onKeyDown)
      }
      nodeViews.clear()
      edgeViews.clear()
      listRows.clear()
      boundaryHeaders.clear()
      groupChips.clear()
      overviewNodes.clear()
      overviewEdgeViews.clear()
      removeNode(root)
    },
  }
}
