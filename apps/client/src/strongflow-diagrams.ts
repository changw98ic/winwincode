// SPDX-License-Identifier: Apache-2.0

import type { StrongFlowProjection } from './strongflow-view-model.js'
import {
  mountStrongFlowDiagramGraph,
} from './strongflow-diagram-graph.js'
import {
  appendOmittedCount,
  boundedItems,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'

export interface StrongFlowDiagramsUpdate {
  readonly projection: StrongFlowProjection | null
  readonly narrow: boolean
}

export interface StrongFlowDiagramsView {
  readonly root: HTMLElement
  update(input: Readonly<StrongFlowDiagramsUpdate>): void
  close(): void
}

export interface StrongFlowDiagramsMountOptions {
  readonly document: Document
  readonly limits: StrongFlowRenderLimits
}

/** Mount the solution graphs and the live execution view as one stable region. */
export function mountStrongFlowDiagrams(
  options: StrongFlowDiagramsMountOptions,
): StrongFlowDiagramsView {
  const document = options.document
  const limits = options.limits
  const root = strongFlowElement(document, 'div', 'wwc-strongflow-diagrams')
  const solution = strongFlowElement(document, 'section', 'wwc-strongflow-view-solution')
  const solutionHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const solutionState = strongFlowElement(document, 'p', 'wwc-strongflow-solution-state')
  const solutionEmpty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  const architectureGraph = mountStrongFlowDiagramGraph({
    document,
    props: emptyGraphProps('pending:architecture'),
  })
  const architectureNodesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const architectureEdgesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const processGraph = mountStrongFlowDiagramGraph({
    document,
    props: emptyGraphProps('pending:process'),
  })
  const processNodesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const processEdgesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const execution = strongFlowElement(document, 'section', 'wwc-strongflow-view-execution')
  const executionHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const executionEmpty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  const executionSessions = strongFlowElement(
    document,
    'div',
    'wwc-strongflow-execution-sessions',
  )
  const executionOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')

  let open = true
  const sessionViews = new Map<string, HTMLElement>()

  solution.dataset.view = 'solution'
  execution.dataset.view = 'execution'
  solutionHeading.textContent = 'Solution view'
  executionHeading.textContent = 'Live execution view'
  solutionEmpty.textContent = 'No solution review is available yet.'
  executionEmpty.textContent = 'No live execution sessions are available.'
  architectureGraph.root.append(architectureNodesOmitted, architectureEdgesOmitted)
  processGraph.root.append(processNodesOmitted, processEdgesOmitted)
  solution.append(
    solutionHeading,
    solutionState,
    solutionEmpty,
    architectureGraph.root,
    processGraph.root,
  )
  execution.append(executionHeading, executionEmpty, executionSessions, executionOmitted)
  root.append(solution, execution)

  function emptyGraphProps(id: string) {
    return {
      id,
      title: 'Architecture',
      nodes: [] as readonly [],
      edges: [] as readonly [],
      narrow: false,
    }
  }

  function renderSolution(projection: StrongFlowProjection, narrow: boolean): void {
    const review = projection.solutionReview
    solutionEmpty.hidden = review !== null
    solutionState.hidden = review === null
    if (review === null) {
      architectureGraph.update({ ...emptyGraphProps('pending:architecture'), narrow })
      processGraph.update({ ...emptyGraphProps('pending:process'), narrow })
      return
    }
    solutionState.dataset.deliveryStatus = projection.delivery.status
    solutionState.dataset.reviewStatus = review.reviewStatus
    solutionState.textContent = `Delivery ${projection.delivery.status}`
      + ` · solution review ${review.reviewStatus}`
    const architectureNodes = boundedItems(review.architectureDiagram.nodes, limits.graphNodes)
    const architectureEdges = boundedItems(review.architectureDiagram.edges, limits.graphEdges)
    architectureGraph.update({
      id: review.architectureDiagram.id,
      title: 'Architecture',
      nodes: architectureNodes.items,
      edges: architectureEdges.items,
      narrow,
    })
    updateGraphOmitted(architectureNodesOmitted, architectureNodes.omitted, 'diagram nodes')
    updateGraphOmitted(architectureEdgesOmitted, architectureEdges.omitted, 'diagram connections')
    const processNodes = boundedItems(review.processDiagram.nodes, limits.graphNodes)
    const processEdges = boundedItems(review.processDiagram.edges, limits.graphEdges)
    processGraph.update({
      id: review.processDiagram.id,
      title: 'Process',
      nodes: processNodes.items,
      edges: processEdges.items,
      narrow,
    })
    updateGraphOmitted(processNodesOmitted, processNodes.omitted, 'diagram nodes')
    updateGraphOmitted(processEdgesOmitted, processEdges.omitted, 'diagram connections')
  }

  function updateGraphOmitted(node: HTMLElement, omitted: number, label: string): void {
    node.hidden = omitted === 0
    const text = `${String(omitted)} more ${label} not rendered.`
    if (node.textContent !== text) node.textContent = text
  }

  function renderExecution(projection: StrongFlowProjection): void {
    const sessions = boundedItems(projection.runtime.sessions, limits.runtimeSessions)
    executionEmpty.hidden = sessions.items.length > 0
    const liveKeys = new Set<string>()
    for (const session of sessions.items) {
      const key = `${session.sessionBindingId}:${String(session.attempt)}`
      liveKeys.add(key)
      const fingerprint = JSON.stringify(session)
      const existing = sessionViews.get(key)
      if (existing !== undefined && existing.dataset.fingerprint === fingerprint) continue
      const view = renderExecutionSession(document, session, limits)
      view.dataset.fingerprint = fingerprint
      if (existing !== undefined) existing.remove()
      sessionViews.set(key, view)
      executionSessions.append(view)
    }
    for (const [key, view] of sessionViews) {
      if (liveKeys.has(key)) continue
      view.remove()
      sessionViews.delete(key)
    }
    updateOmitted(executionOmitted, sessions.omitted, 'runtime sessions')
  }

  function updateOmitted(node: HTMLElement, count: number, label: string): void {
    node.hidden = count === 0
    const text = `${String(count)} more ${label} not rendered.`
    if (node.textContent !== text) node.textContent = text
  }

  function update(input: Readonly<StrongFlowDiagramsUpdate>): void {
    if (!open) throw new Error('StrongFlowDiagrams is closed.')
    const projection = input.projection
    if (projection === null) {
      solutionEmpty.hidden = false
      solutionState.hidden = true
      architectureGraph.update({ ...emptyGraphProps('pending:architecture'), narrow: input.narrow })
      processGraph.update({ ...emptyGraphProps('pending:process'), narrow: input.narrow })
      executionEmpty.hidden = false
      for (const view of sessionViews.values()) view.remove()
      sessionViews.clear()
      updateOmitted(executionOmitted, 0, 'runtime sessions')
      return
    }
    renderSolution(projection, input.narrow)
    renderExecution(projection)
  }

  update({ projection: null, narrow: false })

  return {
    root,
    update,
    close() {
      if (!open) return
      open = false
      architectureGraph.close()
      processGraph.close()
      for (const view of sessionViews.values()) view.remove()
      sessionViews.clear()
      root.remove()
    },
  }
}

function renderExecutionSession(
  document: Document,
  session: StrongFlowProjection['runtime']['sessions'][number],
  limits: StrongFlowRenderLimits,
): HTMLElement {
  const section = strongFlowElement(document, 'section', 'wwc-strongflow-execution-session')
  const heading = strongFlowElement(document, 'h4', 'wwc-strongflow-execution-heading')
  const metadata = strongFlowElement(document, 'p', 'wwc-strongflow-execution-metadata')
  const agents = strongFlowElement(document, 'ul', 'wwc-strongflow-agent-nodes')
  const edges = strongFlowElement(document, 'ul', 'wwc-strongflow-agent-edges')
  const activities = strongFlowElement(document, 'ul', 'wwc-strongflow-activities')
  const boundedAgents = boundedItems(session.agents, limits.graphNodes)
  const boundedEdges = boundedItems(session.agentEdges, limits.graphEdges)
  const boundedActivities = boundedItems(session.activities, limits.activities)
  heading.textContent = session.deliveryTaskId === null
    ? 'Delivery execution'
    : `Task ${session.deliveryTaskId}`
  metadata.textContent = `Attempt ${String(session.attempt)} · sequence ${String(session.asOfSequence)}`
  agents.setAttribute('aria-label', 'Execution agents')
  edges.setAttribute('aria-label', 'Execution agent connections')
  activities.setAttribute('aria-label', 'Execution activities')
  agents.append(...boundedAgents.items.map(agent => {
    const item = document.createElement('li')
    item.dataset.status = agent.status
    item.textContent = `${agent.nickname ?? agent.threadId} · ${agent.role ?? 'agent'} · ${agent.status}`
    return item
  }))
  edges.append(...boundedEdges.items.map(edge => {
    const item = document.createElement('li')
    item.textContent = `${edge.parentThreadId} → ${edge.childThreadId}`
    return item
  }))
  activities.append(...boundedActivities.items.map(activity => {
    const item = document.createElement('li')
    item.dataset.status = activity.status
    item.textContent = `${activity.activityType} · ${activity.status} · ${activity.outcome}`
    return item
  }))
  section.append(heading, metadata, agents)
  appendOmittedCount(document, section, boundedAgents.omitted, 'execution agents')
  section.append(edges)
  appendOmittedCount(document, section, boundedEdges.omitted, 'agent connections')
  section.append(activities)
  appendOmittedCount(document, section, boundedActivities.omitted, 'activities')
  if (session.diffSummary !== null) {
    const diff = strongFlowElement(document, 'p', 'wwc-strongflow-live-diff')
    diff.textContent = `${String(session.diffSummary.changedFileCount)} files · +${String(
      session.diffSummary.additions,
    )} / −${String(session.diffSummary.deletions)}`
    diff.dataset.sourceRef = session.diffSummary.sourceRef
    section.append(diff)
  }
  return section
}
