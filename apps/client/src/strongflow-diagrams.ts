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
  readonly onSelectNode?: (selection: StrongFlowDiagramSelection | null) => void
}

export interface StrongFlowDiagramSelection {
  readonly diagramId: string
  readonly nodeId: string
  readonly taskId: string | null
  readonly attentionItemIds: readonly string[]
  readonly diffPaths: readonly string[]
  readonly evidenceRefIds: readonly string[]
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
  let currentProjection: StrongFlowProjection | null = null
  let architectureGraphSelected: string | null = null
  let processGraphSelected: string | null = null
  let publishedSelectionFingerprint: string | null = null
  const architectureGraph = mountStrongFlowDiagramGraph({
    document,
    props: {
      ...emptyGraphProps('pending:architecture', 'Architecture'),
      onSelectNode: nodeId => selectDiagramNode('architecture', nodeId),
    },
  })
  const architectureNodesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const architectureEdgesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const processGraph = mountStrongFlowDiagramGraph({
    document,
    props: {
      ...emptyGraphProps('pending:process', 'Process'),
      onSelectNode: nodeId => selectDiagramNode('process', nodeId),
    },
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

  function emptyGraphProps(id: string, title: string) {
    return {
      id,
      title,
      nodes: [] as readonly [],
      edges: [] as readonly [],
      narrow: false,
    }
  }

  function executionProjection(projection: StrongFlowProjection) {
    const execution = projection.diagramExecution
    const review = projection.solutionReview
    if (execution === null || review === null) return null
    const reviewDigest = review.reviewSetSha256.replace(/^sha256:/u, '')
    if (execution.schemaVersion !== 1
      || execution.protocol !== 'winwincode.diagram-execution-projection.v1'
      || execution.architecture.kind !== 'system-architecture'
      || execution.process.kind !== 'process-flow'
      || String(execution.deliveryId) !== String(projection.delivery.deliveryId)
      || execution.deliveryRevision !== projection.delivery.deliveryRevision
      || execution.reviewSetSha256 !== reviewDigest
      || execution.architecture.diagramId !== review.architectureDiagram.id
      || execution.process.diagramId !== review.processDiagram.id) {
      throw new Error('StrongFlow diagram execution facts do not match the current review cut.')
    }
    return execution
  }

  function publishSelection(selection: StrongFlowDiagramSelection | null): void {
    const fingerprint = JSON.stringify(selection)
    if (fingerprint === publishedSelectionFingerprint) return
    publishedSelectionFingerprint = fingerprint
    options.onSelectNode?.(selection)
  }

  function diagramNodes(
    projection: StrongFlowProjection,
    kind: 'architecture' | 'process',
  ) {
    const review = projection.solutionReview!
    const diagram = kind === 'architecture'
      ? review.architectureDiagram
      : review.processDiagram
    const execution = executionProjection(projection)
    const executionDiagram = execution === null
      ? null
      : kind === 'architecture' ? execution.architecture : execution.process
    const stateByNode = new Map(executionDiagram?.nodes.map(node => [node.nodeId, node]) ?? [])
    if (executionDiagram !== null) {
      const reviewIds = new Set(diagram.nodes.map(node => node.id))
      const executionIds = new Set(executionDiagram.nodes.map(node => node.nodeId))
      if (reviewIds.size !== diagram.nodes.length
        || executionIds.size !== executionDiagram.nodes.length
        || executionDiagram.nodes.length !== reviewIds.size
        || executionDiagram.nodes.some(node => !reviewIds.has(node.nodeId))) {
        throw new Error('StrongFlow diagram execution nodes do not match the current review.')
      }
    }
    const files = new Map(execution?.details?.files.map(file => [file.id, file]) ?? [])
    return diagram.nodes.map(node => {
      const state = stateByNode.get(node.id)
      const linkedFiles = (state?.fileIds ?? []).map(fileId => files.get(fileId)).filter(
        (file): file is NonNullable<typeof file> => file !== undefined,
      )
      if (execution !== null && execution.details !== null
        && linkedFiles.length !== (state?.fileIds.length ?? 0)) {
        throw new Error('StrongFlow diagram execution file identities are not current.')
      }
      const affected = state !== undefined && state.state !== 'normal'
      return {
        ...node,
        executionState: state?.state ?? 'normal' as const,
        affectedFileCount: state?.affectedFileCount ?? 0,
        fileIds: state?.fileIds ?? [],
        linkedTaskId: affected
          ? execution?.details?.provenance.deliveryTaskId ?? null
          : null,
        linkedAttentionItemIds: [review.attentionItemId],
        linkedDiffPaths: linkedFiles.map(file => file.path),
        linkedEvidenceRefIds: affected
          ? execution?.details?.provenance.evidenceRefIds ?? []
          : [],
      }
    })
  }

  function selectDiagramNode(
    kind: 'architecture' | 'process',
    nodeId: string | null,
  ): void {
    if (kind === 'architecture') architectureGraphSelected = nodeId
    else processGraphSelected = nodeId
    if (nodeId !== null) {
      if (kind === 'architecture' && processGraphSelected !== null) processGraph.select(null)
      if (kind === 'process' && architectureGraphSelected !== null) architectureGraph.select(null)
    }
    const projection = currentProjection
    const review = projection?.solutionReview
    if (nodeId === null || projection === null || review === null || review === undefined) {
      if (architectureGraphSelected === null && processGraphSelected === null) {
        publishSelection(null)
      }
      return
    }
    const diagram = kind === 'architecture'
      ? review.architectureDiagram
      : review.processDiagram
    const node = diagramNodes(projection, kind).find(candidate => candidate.id === nodeId)
    if (node === undefined) throw new Error('Selected StrongFlow diagram node is not current.')
    publishSelection({
      diagramId: diagram.id,
      nodeId,
      taskId: node.linkedTaskId,
      attentionItemIds: node.linkedAttentionItemIds,
      diffPaths: node.linkedDiffPaths,
      evidenceRefIds: node.linkedEvidenceRefIds,
    })
  }

  function renderSolution(projection: StrongFlowProjection, narrow: boolean): void {
    const review = projection.solutionReview
    solutionEmpty.hidden = review !== null
    solutionState.hidden = review === null
    if (review === null) {
      architectureGraph.root.hidden = true
      processGraph.root.hidden = true
      architectureGraph.update({
        ...emptyGraphProps('pending:architecture', 'Architecture'),
        narrow,
        onSelectNode: nodeId => selectDiagramNode('architecture', nodeId),
      })
      processGraph.update({
        ...emptyGraphProps('pending:process', 'Process'),
        narrow,
        onSelectNode: nodeId => selectDiagramNode('process', nodeId),
      })
      updateGraphOmitted(architectureNodesOmitted, 0, 'diagram nodes')
      updateGraphOmitted(architectureEdgesOmitted, 0, 'diagram connections')
      updateGraphOmitted(processNodesOmitted, 0, 'diagram nodes')
      updateGraphOmitted(processEdgesOmitted, 0, 'diagram connections')
      return
    }
    architectureGraph.root.hidden = false
    processGraph.root.hidden = false
    const execution = executionProjection(projection)
    solutionState.dataset.deliveryStatus = projection.delivery.status
    solutionState.dataset.reviewStatus = review.reviewStatus
    solutionState.textContent = `Delivery ${projection.delivery.status}`
      + ` · solution review ${review.reviewStatus}`
      + (execution === null ? '' : ` · diagram ${execution.state}`)
    const architectureNodes = boundedItems(
      diagramNodes(projection, 'architecture'),
      limits.graphNodes,
    )
    const architectureEdges = boundedItems(review.architectureDiagram.edges, limits.graphEdges)
    architectureGraph.update({
      id: review.architectureDiagram.id,
      title: 'Architecture',
      nodes: architectureNodes.items,
      edges: architectureEdges.items,
      narrow,
      onSelectNode: nodeId => selectDiagramNode('architecture', nodeId),
    })
    updateGraphOmitted(architectureNodesOmitted, architectureNodes.omitted, 'diagram nodes')
    updateGraphOmitted(architectureEdgesOmitted, architectureEdges.omitted, 'diagram connections')
    const processNodes = boundedItems(diagramNodes(projection, 'process'), limits.graphNodes)
    const processEdges = boundedItems(review.processDiagram.edges, limits.graphEdges)
    processGraph.update({
      id: review.processDiagram.id,
      title: 'Process',
      nodes: processNodes.items,
      edges: processEdges.items,
      narrow,
      onSelectNode: nodeId => selectDiagramNode('process', nodeId),
    })
    updateGraphOmitted(processNodesOmitted, processNodes.omitted, 'diagram nodes')
    updateGraphOmitted(processEdgesOmitted, processEdges.omitted, 'diagram connections')
    if (architectureGraphSelected !== null) {
      selectDiagramNode('architecture', architectureGraphSelected)
    } else if (processGraphSelected !== null) {
      selectDiagramNode('process', processGraphSelected)
    }
  }

  function updateGraphOmitted(node: HTMLElement, omitted: number, label: string): void {
    const hidden = omitted === 0
    if (node.hidden !== hidden) node.hidden = hidden
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
      if (existing !== undefined) {
        executionSessions.insertBefore(view, existing)
        existing.remove()
      } else {
        executionSessions.append(view)
      }
      sessionViews.set(key, view)
    }
    for (const [key, view] of sessionViews) {
      if (liveKeys.has(key)) continue
      view.remove()
      sessionViews.delete(key)
    }
    sessions.items.forEach((session, index) => {
      const key = `${session.sessionBindingId}:${String(session.attempt)}`
      const view = sessionViews.get(key)
      const current = executionSessions.children[index]
      if (view !== undefined && current !== view) executionSessions.insertBefore(view, current ?? null)
    })
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
    currentProjection = projection
    if (projection === null) {
      solutionEmpty.hidden = false
      solutionState.hidden = true
      architectureGraph.root.hidden = true
      processGraph.root.hidden = true
      architectureGraph.update({
        ...emptyGraphProps('pending:architecture', 'Architecture'),
        narrow: input.narrow,
        onSelectNode: nodeId => selectDiagramNode('architecture', nodeId),
      })
      processGraph.update({
        ...emptyGraphProps('pending:process', 'Process'),
        narrow: input.narrow,
        onSelectNode: nodeId => selectDiagramNode('process', nodeId),
      })
      executionEmpty.hidden = false
      for (const view of sessionViews.values()) view.remove()
      sessionViews.clear()
      updateOmitted(executionOmitted, 0, 'runtime sessions')
      updateGraphOmitted(architectureNodesOmitted, 0, 'diagram nodes')
      updateGraphOmitted(architectureEdgesOmitted, 0, 'diagram connections')
      updateGraphOmitted(processNodesOmitted, 0, 'diagram nodes')
      updateGraphOmitted(processEdgesOmitted, 0, 'diagram connections')
      architectureGraphSelected = null
      processGraphSelected = null
      publishSelection(null)
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
