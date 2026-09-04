// SPDX-License-Identifier: Apache-2.0

import type { StrongFlowProjection } from './strongflow-view-model.js'
import {
  mountStrongFlowDiagramGraph,
} from './strongflow-diagram-graph.js'
import {
  boundedEdgesForNodes,
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

/**
 * Mount the solution review graphs as one stable region. The live execution
 * graph and activity timeline live in the dedicated keyed execution-graph view
 * (strongflow-execution-graph.ts), so high-frequency runtime deltas never
 * rebuild this node.
 */
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

  let open = true

  solution.dataset.view = 'solution'
  solutionHeading.textContent = 'Solution view'
  solutionEmpty.textContent = 'No solution review is available yet.'
  architectureGraph.root.append(architectureNodesOmitted, architectureEdgesOmitted)
  processGraph.root.append(processNodesOmitted, processEdgesOmitted)
  solution.append(
    solutionHeading,
    solutionState,
    solutionEmpty,
    architectureGraph.root,
    processGraph.root,
  )
  root.append(solution)

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
    if (execution.schemaVersion !== 1
      || execution.protocol !== 'winwincode.diagram-execution-projection.v1'
      || execution.architecture.kind !== 'system-architecture'
      || execution.process.kind !== 'process-flow'
      || String(execution.deliveryId) !== String(projection.delivery.deliveryId)
      || execution.deliveryRevision !== projection.delivery.deliveryRevision
      || execution.reviewSetSha256 !== review.reviewSetSha256
      || execution.architecture.diagramId !== review.architectureDiagram.id
      || execution.process.diagramId !== review.processDiagram.id) {
      throw new Error('StrongFlow diagram execution facts do not match the current review cut.')
    }
    return execution
  }

  function currentExecutionDetails(projection: StrongFlowProjection) {
    const execution = executionProjection(projection)
    const details = execution?.details
    const candidate = projection.currentCandidate
    if (details === null || details === undefined || candidate === null) return null
    const source = details.candidate
    if (source.candidateRef !== candidate.candidateRef
      || source.deliverySpecId !== candidate.deliverySpecId
      || source.deliverySpecRevision !== candidate.deliverySpecRevision
      || source.candidateCommitId !== candidate.candidateCommitId
      || source.candidateTreeId !== candidate.candidateTreeId
      || source.diffSha256 !== candidate.diffSha256
      || source.frozenAt !== candidate.frozenAt
      || details.diffSha256 !== source.diffSha256
      || source.producerStageRunId !== details.provenance.stageRunId
      || source.producerSessionBindingId !== details.provenance.sessionBindingId) return null
    return details
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
    const details = currentExecutionDetails(projection)
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
    const executionFiles = execution?.details?.files ?? []
    const files = new Map(executionFiles.map(file => [file.id, file]))
    if (files.size !== executionFiles.length) {
      throw new Error('StrongFlow diagram execution file identities are not unique.')
    }
    const linkedAttentionItemIds = projection.attention.some(item => (
      item.id === review.attentionItemId
      && item.deliverySpecId === review.deliverySpecId
    )) ? [review.attentionItemId] : []
    const provenance = details?.provenance ?? null
    const linkedTaskId = provenance?.deliveryTaskId ?? null
    const currentTaskId = linkedTaskId !== null && provenance !== null
      && projection.delivery.tasks.some(task => (
        String(task.id) === String(linkedTaskId)
        && task.stageRunIds.some(stageRunId => (
          String(stageRunId) === String(provenance.stageRunId)
        ))
      )) ? linkedTaskId : null
    const linkedEvidenceRefIds = provenance?.evidenceRefIds.filter(evidenceRefId => (
      projection.evidence.some(evidence => (
        String(evidence.id) === String(evidenceRefId)
        && evidence.candidateRef === projection.currentCandidate?.candidateRef
        && evidence.deliverySpecId === projection.currentCandidate.deliverySpecId
        && evidence.deliverySpecRevision === projection.currentCandidate.deliverySpecRevision
        && String(evidence.stageRunId) === String(provenance.stageRunId)
        && evidence.sessionBindingId === provenance.sessionBindingId
      ))
    )) ?? []
    return diagram.nodes.map(node => {
      const state = stateByNode.get(node.id)
      const linkedFiles = (state?.fileIds ?? []).map(fileId => files.get(fileId)).filter(
        (file): file is NonNullable<typeof file> => file !== undefined,
      )
      if (execution !== null && execution.details !== null && (
        linkedFiles.length !== (state?.fileIds.length ?? 0)
        || linkedFiles.some(file => !file.nodeIds.includes(node.id))
      )) {
        throw new Error('StrongFlow diagram execution file identities are not current.')
      }
      const affected = state !== undefined && state.state !== 'normal'
      return {
        ...node,
        executionState: state?.state ?? 'normal' as const,
        affectedFileCount: state?.affectedFileCount ?? 0,
        fileIds: state?.fileIds ?? [],
        linkedTaskId: affected ? currentTaskId : null,
        linkedAttentionItemIds,
        linkedDiffPaths: details === null ? [] : linkedFiles.map(file => file.path),
        linkedEvidenceRefIds: affected ? linkedEvidenceRefIds : [],
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
    const architectureEdges = boundedEdgesForNodes(
      review.architectureDiagram.edges,
      new Set(architectureNodes.items.map(node => node.id)),
      limits.graphEdges,
    )
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
    const processEdges = boundedEdgesForNodes(
      review.processDiagram.edges,
      new Set(processNodes.items.map(node => node.id)),
      limits.graphEdges,
    )
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
      root.remove()
    },
  }
}
