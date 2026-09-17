// SPDX-License-Identifier: Apache-2.0

import type { SolutionReviewDiagramProjection } from './generated/contracts.js'

/** Render the sealed graph as SVG with a complete, readable text equivalent. */
export function solutionDiagram(document: Document, diagram: SolutionReviewDiagramProjection, markerId: string): HTMLElement {
  const figure = document.createElement('figure')
  figure.className = 'wwc-review-diagram'
  const caption = document.createElement('figcaption')
  caption.textContent = diagram.title
  figure.append(caption)
  if (diagram.nodes.length === 0) {
    const empty = document.createElement('p')
    empty.textContent = '方案尚未提供图中节点。'
    figure.append(empty)
    return figure
  }
  const svg = (tag: string, attributes: Record<string, string | number> = {}) => {
    const node = document.createElementNS('http://www.w3.org/2000/svg', tag)
    for (const [name, value] of Object.entries(attributes)) node.setAttribute(name, String(value))
    return node
  }
  const columns = diagram.kind === 'process-flow' ? 1 : Math.min(3, diagram.nodes.length)
  const width = columns * 280, height = Math.ceil(diagram.nodes.length / columns) * 150 + 30
  const graph = svg('svg', { viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': diagram.title })
  graph.setAttribute('style', `min-width: ${columns * 240}px`)
  const defs = svg('defs')
  const marker = svg('marker', { id: markerId, viewBox: '0 0 10 10', refX: 9, refY: 5, markerWidth: 7, markerHeight: 7, orient: 'auto-start-reverse' })
  marker.append(svg('path', { d: 'M 0 0 L 10 5 L 0 10 z', fill: 'currentColor' }))
  defs.append(marker); graph.append(defs)
  const positions = new Map(diagram.nodes.map((node, index) => [node.id, { x: (index % columns) * 280 + 30, y: Math.floor(index / columns) * 150 + 20 }]))
  for (const edge of diagram.edges) {
    const from = positions.get(edge.from), to = positions.get(edge.to)
    if (from === undefined || to === undefined) continue
    const sameRow = from.y === to.y
    const right = from.x < to.x
    const x1 = from.x + (sameRow ? (right ? 220 : 0) : 110), y1 = from.y + (sameRow ? 40 : 80)
    const x2 = to.x + (sameRow ? (right ? 0 : 220) : 110), y2 = to.y + (sameRow ? 40 : 0)
    const line = svg('path', { d: `M ${x1} ${y1} L ${x2} ${y2}`, fill: 'none', stroke: 'currentColor', 'stroke-width': 2, 'marker-end': `url(#${markerId})` })
    const title = svg('title'); title.textContent = edge.label; line.append(title); graph.append(line)
  }
  for (const node of diagram.nodes) {
    const position = positions.get(node.id)
    if (position === undefined) continue
    const group = svg('g')
    const title = svg('title'); title.textContent = `${node.label}：${node.description}`
    group.append(title, svg('rect', { x: position.x, y: position.y, width: 220, height: 80, rx: 8, class: node.unresolved ? 'wwc-review-diagram-node unresolved' : 'wwc-review-diagram-node' }))
    const letters = Array.from(node.label)
    for (let line = 0; line < Math.min(2, Math.ceil(letters.length / 14)); line += 1) {
      const label = svg('text', { x: position.x + 110, y: position.y + 32 + line * 22, 'text-anchor': 'middle' })
      label.textContent = letters.slice(line * 14, (line + 1) * 14).join('') + (line === 1 && letters.length > 28 ? '…' : '')
      group.append(label)
    }
    graph.append(group)
  }
  const scroll = document.createElement('div'); scroll.className = 'wwc-review-diagram-scroll'; scroll.append(graph)
  const details = document.createElement('details')
  const summary = document.createElement('summary'); summary.textContent = '节点说明与连接关系'
  const list = document.createElement('ul')
  const labels = new Map(diagram.nodes.map(node => [node.id, node.label]))
  for (const text of [
    ...diagram.nodes.map(node => `${node.label}：${node.description}${node.unresolved ? '（待确认）' : ''}`),
    ...diagram.edges.map(edge => `${labels.get(edge.from) ?? edge.from} → ${labels.get(edge.to) ?? edge.to}：${edge.label}`),
  ]) { const item = document.createElement('li'); item.textContent = text; list.append(item) }
  details.append(summary, list); figure.append(scroll, details)
  return figure
}
