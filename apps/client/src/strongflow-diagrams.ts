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

export function renderStrongFlowDiagrams(
  document: Document,
  projection: StrongFlowProjection,
  limits: StrongFlowRenderLimits,
): HTMLElement {
  const root = strongFlowElement(document, 'div', 'wwc-strongflow-diagrams')
  const solution = strongFlowElement(document, 'section', 'wwc-strongflow-view-solution')
  const solutionHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const execution = strongFlowElement(document, 'section', 'wwc-strongflow-view-execution')
  const executionHeading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  solution.dataset.view = 'solution'
  execution.dataset.view = 'execution'
  solutionHeading.textContent = 'Solution view'
  executionHeading.textContent = 'Live execution view'
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
  execution.append(executionHeading)
  const sessions = boundedItems(projection.runtime.sessions, limits.runtimeSessions)
  if (sessions.items.length === 0) {
    const empty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
    empty.textContent = 'No live execution sessions are available.'
    execution.append(empty)
  } else {
    execution.append(...sessions.items.map(session => (
      renderExecutionSession(document, session, limits)
    )))
  }
  appendOmittedCount(document, execution, sessions.omitted, 'runtime sessions')
  root.append(solution, execution)
  return root
}
