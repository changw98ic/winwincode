// SPDX-License-Identifier: Apache-2.0

import type { StrongFlowProjection } from './strongflow-view-model.js'
import {
  appendOmittedCount,
  boundedItems,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'

function renderSolutionDiagram(
  document: Document,
  title: string,
  diagram: NonNullable<StrongFlowProjection['solutionReview']>['architectureDiagram'],
  limits: StrongFlowRenderLimits,
): HTMLElement {
  const section = strongFlowElement(document, 'section', 'wwc-strongflow-diagram')
  const heading = strongFlowElement(document, 'h4', 'wwc-strongflow-diagram-heading')
  const nodes = strongFlowElement(document, 'ul', 'wwc-strongflow-diagram-nodes')
  const edges = strongFlowElement(document, 'ul', 'wwc-strongflow-diagram-edges')
  const boundedNodes = boundedItems(diagram.nodes, limits.graphNodes)
  const boundedEdges = boundedItems(diagram.edges, limits.graphEdges)
  heading.textContent = title
  nodes.setAttribute('aria-label', `${title} nodes`)
  edges.setAttribute('aria-label', `${title} connections`)
  nodes.append(...boundedNodes.items.map(node => {
    const item = document.createElement('li')
    const label = document.createElement('strong')
    const description = document.createElement('p')
    label.textContent = node.label
    description.textContent = node.description
    item.dataset.kind = node.kind
    item.dataset.unresolved = String(node.unresolved)
    item.append(label, description)
    return item
  }))
  edges.append(...boundedEdges.items.map(edge => {
    const item = document.createElement('li')
    item.textContent = `${edge.from} → ${edge.to}: ${edge.label}`
    return item
  }))
  section.append(heading, nodes)
  appendOmittedCount(document, section, boundedNodes.omitted, 'diagram nodes')
  section.append(edges)
  appendOmittedCount(document, section, boundedEdges.omitted, 'diagram connections')
  return section
}

/**
 * Render the solution review diagrams. The execution graph and timeline live
 * in the dedicated keyed execution-graph view so high-frequency runtime
 * deltas never rebuild this node.
 */
export function renderStrongFlowSolutionDiagrams(
  document: Document,
  projection: StrongFlowProjection,
  limits: StrongFlowRenderLimits,
): HTMLElement {
  const root = strongFlowElement(document, 'div', 'wwc-strongflow-diagrams')
  const solution = strongFlowElement(document, 'section', 'wwc-strongflow-view-solution')
  const solutionHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  solution.dataset.view = 'solution'
  solutionHeading.textContent = 'Solution view'
  solution.append(solutionHeading)
  if (projection.solutionReview === null) {
    const empty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
    empty.textContent = 'No solution review is available yet.'
    solution.append(empty)
  } else {
    solution.append(
      renderSolutionDiagram(
        document,
        'Architecture',
        projection.solutionReview.architectureDiagram,
        limits,
      ),
      renderSolutionDiagram(
        document,
        'Process',
        projection.solutionReview.processDiagram,
        limits,
      ),
    )
  }
  root.append(solution)
  return root
}
