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
}

interface MountedEdge {
  readonly root: HTMLElement
  readonly label: HTMLElement
}

interface MountedListRow {
  readonly root: HTMLElement
  readonly text: HTMLElement
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
  const predecessors = new Map<string, string[]>()
  for (const node of nodes) predecessors.set(node.id, [])
  for (const edge of edges) {
    const incoming = predecessors.get(edge.to)
    if (incoming === undefined || !predecessors.has(edge.from)) continue
    incoming.push(edge.from)
  }
  const ranks = new Map<string, number>()
  const visiting = new Set<string>()
  const rankOf = (id: string): number => {
    const memo = ranks.get(id)
    if (memo !== undefined) return memo
    if (visiting.has(id)) return -1
    visiting.add(id)
    let rank = 0
    for (const parent of predecessors.get(id) ?? []) {
      const parentRank = rankOf(parent)
      if (parentRank >= 0) rank = Math.max(rank, parentRank + 1)
    }
    visiting.delete(id)
    ranks.set(id, rank)
    return rank
  }
  for (const node of nodes) rankOf(node.id)

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
  const detail = document.createElement('p')
  detail.className = 'wwc-strongflow-graph-detail'
  detail.setAttribute('aria-live', 'polite')
  const listRoot = document.createElement('ul')
  listRoot.className = 'wwc-strongflow-graph-list'
  root.append(heading, toolbar, boundaries, viewport, detail, listRoot)

  let open = true
  let currentProps: Readonly<StrongFlowDiagramGraphProps> = options.props
  const nodeViews = new Map<string, MountedNode>()
  const edgeViews = new Map<string, MountedEdge>()
  const listRows = new Map<string, MountedListRow>()
  const boundaryHeaders = new Map<string, HTMLButtonElement>()
  const groupChips = new Map<string, HTMLButtonElement>()
  const chipViews = new Map<string, ((event: KeyboardEvent) => void)>()
  const collapsedBoundaries = new Set<string>()
  let selectedNodeId: string | null = null
  let zoom = 1
  let viewMode: 'graph' | 'list' = 'graph'
  let layout = new Map<string, StrongFlowDiagramGraphPosition>()
  let nodeOrder: readonly StrongFlowDiagramGraphNode[] = []

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

  toggleView.addEventListener('click', onToggleViewClick)
  zoomIn.addEventListener('click', onZoomInClick)
  zoomOut.addEventListener('click', onZoomOutClick)
  fit.addEventListener('click', onFitClick)
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
    for (const chip of groupChips.values()) targets.push(chip)
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
    zoom = 1
    viewport.setAttribute('data-zoom', formatZoom(zoom))
    applyStyle(canvas, { transform: 'scale(1)' })
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
      const label = document.createElement('span')
      label.className = 'wwc-strongflow-graph-node-label'
      root.append(icon, label)
      view = { root, icon, label, onClick, onKeyDown, badge: null }
      nodeViews.set(node.id, view)
      canvas.append(root)
    }
    const position = layout.get(node.id)
    view.root.dataset.kind = node.kind
    view.root.dataset.unresolved = String(node.unresolved)
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
      const label = document.createElement('span')
      label.className = 'wwc-strongflow-graph-edge-label'
      root.append(label)
      view = { root, label }
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
    const lines = [`${node.label} — ${node.kind}`]
    if (node.trustBoundary !== null) lines.push(node.trustBoundary)
    if (node.unresolved) lines.push('Unresolved')
    lines.push(node.description)
    for (const edge of connections) {
      const fromLabel = nodesById.get(edge.from)?.label ?? edge.from
      const toLabel = nodesById.get(edge.to)?.label ?? edge.to
      lines.push(`${fromLabel} → ${toLabel}: ${edge.label}`)
    }
    row.text.textContent = lines.join('. ')
    return row
  }

  function ensureBoundaryHeader(boundary: string, count: number): HTMLButtonElement {
    let header = boundaryHeaders.get(boundary)
    if (header === undefined) {
      header = document.createElement('button')
      header.type = 'button'
      header.className = 'wwc-strongflow-graph-boundary'
      header.dataset.boundary = boundary
      header.addEventListener('click', () => toggleBoundary(boundary))
      boundaryHeaders.set(boundary, header)
      boundaries.append(header)
    }
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
    let chip = groupChips.get(boundary)
    if (chip === undefined) {
      chip = document.createElement('button')
      chip.type = 'button'
      chip.className = 'wwc-strongflow-graph-group'
      chip.dataset.boundary = boundary
      chip.tabIndex = -1
      const onChipKeyDown = onNodeKeyDown(chip)
      chip.addEventListener('click', () => toggleBoundary(boundary))
      chip.addEventListener('keydown', onChipKeyDown)
      chipViews.set(boundary, onChipKeyDown)
      groupChips.set(boundary, chip)
      canvas.append(chip)
    }
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
    if (collapsedBoundaries.has(boundary)) collapsedBoundaries.delete(boundary)
    else collapsedBoundaries.add(boundary)
    renderGraphState()
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
      const onKeyDown = chipViews.get(boundary)
      if (onKeyDown !== undefined) chip.removeEventListener('keydown', onKeyDown)
      chip.remove()
      chipViews.delete(boundary)
      groupChips.delete(boundary)
    }
    for (const [boundary, header] of boundaryHeaders) {
      if (groups.has(boundary)) continue
      header.remove()
      boundaryHeaders.delete(boundary)
    }
    for (const node of nodeOrder) {
      const view = nodeViews.get(node.id)
      if (view === undefined) continue
      view.root.hidden = hiddenNodes.has(node.id)
    }
    const chipPositions = new Map<string, StrongFlowDiagramGraphPosition>()
    for (const boundary of liveChips) {
      const chip = groupChips.get(boundary)
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
      const bothHidden = fromNode.root.hidden && toNode.root.hidden
      view.root.hidden = bothHidden
      const fromPosition = fromNode.root.hidden && fromCollapsed && fromBoundary !== null
        ? chipPositions.get(fromBoundary) ?? layout.get(edge.from)
        : layout.get(edge.from)
      const toPosition = toNode.root.hidden && toCollapsed && toBoundary !== null
        ? chipPositions.get(toBoundary) ?? layout.get(edge.to)
        : layout.get(edge.to)
      const fromPoint = fromPosition ?? { x: 0, y: 0, rank: 0, row: 0 }
      const toPoint = toPosition ?? { x: 0, y: 0, rank: 0, row: 0 }
      const midpointX = Math.round((fromPoint.x + toPoint.x) / 2)
        + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET
      const midpointY = Math.round((fromPoint.y + toPoint.y) / 2)
        + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET
      view.root.dataset.x = String(midpointX)
      view.root.dataset.y = String(midpointY)
      applyStyle(view.root, {
        left: `${String(midpointX)}px`,
        top: `${String(midpointY)}px`,
      })
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
    detail.textContent = `${parts.join(' · ')}. ${node.description}`
  }

  function select(nodeId: string | null): void {
    const changed = selectedNodeId !== nodeId
    selectedNodeId = nodeId
    renderSelection()
    if (changed) currentProps.onSelectNode?.(nodeId)
  }

  function update(props: Readonly<StrongFlowDiagramGraphProps>): void {
    assertMounted(open, 'StrongFlowDiagramGraph')
    if (props.id !== currentProps.id) {
      selectedNodeId = null
      collapsedBoundaries.clear()
      zoom = 1
      viewMode = 'graph'
    }
    currentProps = props
    root.dataset.diagram = props.id
    heading.textContent = props.title
    viewport.setAttribute('aria-label', `${props.title} graph viewport`)
    viewport.setAttribute('data-viewport', props.narrow ? 'narrow' : 'wide')

    nodeOrder = [...props.nodes]
    layout = new Map(strongFlowDiagramGraphLayout(props.nodes, props.edges))
    const nodesById = new Map(props.nodes.map(node => [node.id, node]))
    const extentX = Math.max(0, ...nodeOrder.map(node => layout.get(node.id)?.x ?? 0))
      + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET * 2
    const extentY = Math.max(0, ...nodeOrder.map(node => layout.get(node.id)?.y ?? 0))
      + STRONGFLOW_DIAGRAM_GRAPH_ORIGIN_OFFSET * 2
    applyStyle(canvas, { width: `${String(extentX)}px`, height: `${String(extentY)}px` })
    for (const node of props.nodes) syncNode(node)
    for (const [nodeId, view] of nodeViews) {
      if (nodesById.has(nodeId)) continue
      view.root.removeEventListener('click', view.onClick)
      view.root.removeEventListener('keydown', view.onKeyDown)
      view.root.remove()
      nodeViews.delete(nodeId)
    }
    for (const edge of props.edges) syncEdge(edge, nodesById)
    for (const [edgeId, view] of edgeViews) {
      if (props.edges.some(edge => edge.id === edgeId)) continue
      view.root.remove()
      edgeViews.delete(edgeId)
    }
    for (const node of props.nodes) {
      const connections = props.edges.filter(edge => edge.from === node.id || edge.to === node.id)
      syncListRow(node, nodesById, connections)
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
    renderSelection()
    renderViewState()
    applyZoom(zoom)
    const targets = navigationTargets()
    const anchorFocused = targets.some(target => target.tabIndex === 0)
    if (!anchorFocused && targets.length > 0) {
      for (const target of targets) target.tabIndex = target === targets[0] ? 0 : -1
    }
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
      viewport.removeEventListener('keydown', onViewportKeyDown)
      for (const view of nodeViews.values()) {
        view.root.removeEventListener('click', view.onClick)
        view.root.removeEventListener('keydown', view.onKeyDown)
      }
      nodeViews.clear()
      edgeViews.clear()
      listRows.clear()
      boundaryHeaders.clear()
      groupChips.clear()
      chipViews.clear()
      removeNode(root)
    },
  }
}
