// SPDX-License-Identifier: Apache-2.0

import type {
  CandidateHistoricalReviewProjection,
  CandidateHistoryItemProjection,
  EvidenceId,
  RuntimeProjectionSnapshot,
  RuntimeSessionProjection,
  StageRunId,
} from './generated/contracts.js'
import {
  boundedItems,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'
import {
  mountStrongFlowActivityTimeline,
  strongFlowExecutionEvidenceLink,
  type StrongFlowExecutionEvidenceLink,
} from './strongflow-execution-graph.js'
import type { StrongFlowHistorySelection } from './strongflow-history-selection.js'
import type {
  StrongFlowHistoryEvidence,
  StrongFlowHistoryTree,
} from './strongflow-history-tree.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'

/** Exact server identity of one historical Candidate opened for review. */
export interface StrongFlowHistoryCandidateIdentity {
  readonly candidateRef: string
  readonly candidateTreeId: string
  readonly diffSha256: string
}

/**
 * Read-only loaders for the historical review payload. The StrongFlow
 * view-model implements them through the generated Control Plane facade, so
 * this module never builds a second network seam.
 */
export interface StrongFlowRunDetailLoaders {
  loadRuntime(
    stageRunId: StageRunId,
    signal: AbortSignal,
  ): Promise<RuntimeProjectionSnapshot | null>
  loadCandidates(
    stageRunId: StageRunId,
    signal: AbortSignal,
  ): Promise<readonly CandidateHistoryItemProjection[]>
  loadCandidateReview(
    candidate: StrongFlowHistoryCandidateIdentity,
    signal: AbortSignal,
  ): Promise<CandidateHistoricalReviewProjection | null>
}

export interface StrongFlowRunDetailOptions {
  readonly document: Document
  readonly limits: StrongFlowRenderLimits
  readonly loaders: StrongFlowRunDetailLoaders
}

export interface StrongFlowRunDetailState {
  readonly tree: StrongFlowHistoryTree | null
  readonly selection: StrongFlowHistorySelection
}

export interface StrongFlowRunDetailView {
  readonly root: HTMLElement
  update(state: StrongFlowRunDetailState): void
  close(): void
}

interface DefinitionRow {
  readonly value: HTMLElement
}

interface CandidateRowState {
  readonly button: HTMLButtonElement
  item: CandidateHistoryItemProjection
  onClick: () => void
}

type PanelStatus = 'idle' | 'unsupported' | 'loading' | 'ready' | 'error'

function setText(node: HTMLElement, text: string): void {
  if (node.textContent !== text) node.textContent = text
}

function definitionRow(
  document: Document,
  parent: HTMLElement,
  term: string,
): DefinitionRow {
  const termNode = document.createElement('dt')
  const valueNode = document.createElement('dd')
  termNode.textContent = term
  parent.append(termNode, valueNode)
  return { value: valueNode }
}

function applyDefinitions(rows: readonly DefinitionRow[], values: readonly string[]): void {
  rows.forEach((row, index) => {
    setText(row.value, values[index] ?? '—')
  })
}

function candidateIdentity(
  item: CandidateHistoryItemProjection,
): StrongFlowHistoryCandidateIdentity {
  return {
    candidateRef: item.candidate.candidateRef,
    candidateTreeId: item.candidate.candidateTreeId,
    diffSha256: item.candidate.diffSha256,
  }
}

function candidateIdentityKey(identity: StrongFlowHistoryCandidateIdentity): string {
  return JSON.stringify([
    identity.candidateRef,
    identity.candidateTreeId,
    identity.diffSha256,
  ])
}

function statusNode(
  document: Document,
  parent: HTMLElement,
  className: string,
  role: 'status' | 'alert',
): HTMLElement {
  const node = strongFlowElement(document, 'p', className)
  node.setAttribute('role', role)
  node.hidden = true
  parent.append(node)
  return node
}

/**
 * Read-only historical run review. Rebuilds the execution graph and timeline
 * of a historical Attempt strictly from that attempt's own RuntimeProjection
 * snapshot, loaded at the historical read cursor. No current-run state is
 * ever mixed into it, and no mutating control is exposed.
 */
export function mountStrongFlowRunDetail(
  options: StrongFlowRunDetailOptions,
): StrongFlowRunDetailView {
  const document = options.document
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-history')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const note = strongFlowElement(document, 'p', 'wwc-strongflow-history-note')
  const identityHost = strongFlowElement(document, 'div', 'wwc-strongflow-history-identity-host')
  const runtimeHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const runtimeHost = strongFlowElement(document, 'div', 'wwc-strongflow-history-runtime-host')
  const bindingHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const bindingHost = strongFlowElement(document, 'div', 'wwc-strongflow-history-binding-host')
  const evidenceHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const evidence = strongFlowElement(document, 'ul', 'wwc-strongflow-history-evidence')
  const evidenceOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const candidatesHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const candidatesHost = strongFlowElement(document, 'div', 'wwc-strongflow-history-candidates-host')
  const reviewHeading = strongFlowElement(document, 'h4', 'wwc-strongflow-subheading')
  const reviewHost = strongFlowElement(document, 'div', 'wwc-strongflow-history-review-host')
  const conclusion = strongFlowElement(
    document,
    'section',
    'wwc-strongflow-history-conclusion',
  )
  const conclusionStatus = document.createElement('strong')
  const conclusionText = document.createElement('p')
  root.hidden = true
  root.setAttribute('aria-label', 'Historical StageRun review')
  heading.textContent = 'Historical StageRun review'
  note.textContent = 'Read-only history: this StageRun is not the current run.'
  runtimeHeading.textContent = 'Runtime projection'
  bindingHeading.textContent = 'Runtime binding'
  evidenceHeading.textContent = 'Evidence from this run'
  candidatesHeading.textContent = 'Candidates from this run'
  reviewHeading.textContent = 'Historical candidate review'
  reviewHeading.hidden = true
  reviewHost.hidden = true
  evidence.setAttribute('aria-label', 'StageRun evidence')
  conclusion.append(conclusionStatus, conclusionText)
  root.append(
    heading,
    note,
    identityHost,
    runtimeHeading,
    runtimeHost,
    bindingHeading,
    bindingHost,
    evidenceHeading,
    evidence,
    evidenceOmitted,
    candidatesHeading,
    candidatesHost,
    reviewHeading,
    reviewHost,
    conclusion,
  )

  // Identity and binding definition lists keep one node per term and only
  // update text, so repeated snapshots never rebuild the detail DOM.
  const identityList = strongFlowElement(document, 'dl', 'wwc-strongflow-history-identity')
  identityHost.append(identityList)
  const identityRows = [
    'StageRun',
    'Attempt',
    'Stage',
    'Role',
    'Actor',
    'Task',
    'Status',
    'Started',
    'Finished',
  ].map(term => definitionRow(document, identityList, term))
  const bindingList = strongFlowElement(document, 'dl', 'wwc-strongflow-history-binding')
  const bindingEmpty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  bindingHost.append(bindingList, bindingEmpty)
  const bindingRows = [
    'ProductSession',
    'ExecutionJob',
    'Worker',
    'WorkerSession',
    'CodexThread',
  ].map(term => definitionRow(document, bindingList, term))
  bindingEmpty.textContent = 'Human review StageRun — no runtime binding.'

  const evidenceCollection = mountKeyedCollection<
    StrongFlowHistoryEvidence,
    string,
    HTMLLIElement
  >({
    parent: evidence,
    key: item => item.id,
    create: () => document.createElement('li'),
    update(item, entry) {
      item.dataset.evidenceId = entry.id
      setText(item, `${entry.type} · ${entry.id} · ${entry.sourceRef}`)
    },
  })

  let lastRunEvidence: readonly StrongFlowExecutionEvidenceLink[] = []

  function openEvidenceDetail(evidenceId: EvidenceId): void {
    const row = evidenceCollection.node(evidenceId)
    if (row === null) return
    row.dataset.evidenceTarget = 'true'
    row.scrollIntoView({ block: 'nearest' })
  }

  const runtimeStatus = statusNode(
    document,
    runtimeHost,
    'wwc-strongflow-history-runtime-status',
    'status',
  )
  const runtimeError = statusNode(
    document,
    runtimeHost,
    'wwc-strongflow-history-runtime-error',
    'alert',
  )
  const runtimeRetry = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-history-runtime-retry',
  ) as HTMLButtonElement
  const runtimeEmpty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  const runtimeList = strongFlowElement(document, 'dl', 'wwc-strongflow-history-runtime')
  const runtimeSessions = strongFlowElement(
    document,
    'ul',
    'wwc-strongflow-history-runtime-sessions',
  )
  runtimeRetry.type = 'button'
  runtimeRetry.textContent = 'Retry runtime projection'
  runtimeRetry.hidden = true
  runtimeEmpty.textContent = 'No runtime projection — this StageRun has no runtime binding.'
  runtimeEmpty.hidden = true
  runtimeHost.append(runtimeEmpty, runtimeList, runtimeSessions)
  const runtimeRows = ['Runtime revision', 'Accepted sequence', 'Rebuilt', 'Sessions']
    .map(term => definitionRow(document, runtimeList, term))

  const candidateRows = new WeakMap<HTMLLIElement, CandidateRowState>()
  const sessionTimelines: ReturnType<typeof mountStrongFlowActivityTimeline>[] = []
  const sessionsCollection = mountKeyedCollection<
    RuntimeSessionProjection,
    string,
    HTMLLIElement
  >({
    parent: runtimeSessions,
    key: session => `${session.productSessionId}:${session.stageRunId ?? 'none'}:${session.sessionBindingId}`,
    create: () => {
      const item = document.createElement('li')
      const summary = document.createElement('p')
      summary.className = 'wwc-strongflow-history-runtime-session'
      const timeline = mountStrongFlowActivityTimeline({
        document,
        limits: options.limits,
        onOpenEvidence: openEvidenceDetail,
      })
      item.append(summary, timeline.root)
      sessionTimelines.push(timeline)
      return item
    },
    update(item, session) {
      const summary = item.children[0] as HTMLElement
      item.dataset.codexThreadId = session.codexThreadId
      setText(
        summary,
        `attempt ${String(session.attempt)} · ${session.codexThreadId} · ${
          String(session.agents.length)
        } agents · as-of ${String(session.asOfSequence)}`,
      )
      const timelineRoot = item.children[1] as HTMLElement | undefined
      const timeline = sessionTimelines.find(entry => entry.root === timelineRoot)
      if (timeline === undefined) return
      timeline.update({
        session,
        evidence: lastRunEvidence,
        readOnly: true,
      })
    },
    remove(item) {
      const timelineRoot = item.children[1] as HTMLElement | undefined
      const index = sessionTimelines.findIndex(entry => entry.root === timelineRoot)
      const timeline = sessionTimelines[index]
      if (timeline !== undefined) {
        timeline.close()
        sessionTimelines.splice(index, 1)
      }
    },
  })

  const candidatesStatus = statusNode(
    document,
    candidatesHost,
    'wwc-strongflow-history-candidates-status',
    'status',
  )
  const candidatesError = statusNode(
    document,
    candidatesHost,
    'wwc-strongflow-history-candidates-error',
    'alert',
  )
  const candidatesRetry = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-history-candidates-retry',
  ) as HTMLButtonElement
  const candidates = strongFlowElement(document, 'ul', 'wwc-strongflow-history-candidates')
  const candidatesEmpty = strongFlowElement(document, 'p', 'wwc-strongflow-empty')
  candidatesRetry.type = 'button'
  candidatesRetry.textContent = 'Retry candidates'
  candidatesRetry.hidden = true
  candidatesEmpty.textContent = 'No candidates were produced by this run.'
  candidatesEmpty.hidden = true
  candidatesHost.append(candidatesRetry, candidates, candidatesEmpty)

  const candidatesCollection = mountKeyedCollection<
    CandidateHistoryItemProjection,
    string,
    HTMLLIElement
  >({
    parent: candidates,
    key: item => item.candidate.candidateRef,
    create: createCandidateRow,
    update: updateCandidateRow,
    remove(item) {
      const state = candidateRows.get(item)
      if (state === undefined) return
      state.button.removeEventListener('click', state.onClick)
      candidateRows.delete(item)
    },
  })

  const reviewStatus = statusNode(
    document,
    reviewHost,
    'wwc-strongflow-history-review-status',
    'status',
  )
  const reviewError = statusNode(
    document,
    reviewHost,
    'wwc-strongflow-history-review-error',
    'alert',
  )
  const reviewNote = strongFlowElement(document, 'p', 'wwc-strongflow-history-review-note')
  const reviewList = strongFlowElement(document, 'dl', 'wwc-strongflow-history-review')
  const reviewEvidence = strongFlowElement(
    document,
    'ul',
    'wwc-strongflow-history-review-evidence',
  )
  reviewNote.textContent = 'Historical review — display only. It never authorizes the current Delivery.'
  reviewHost.append(reviewNote, reviewList, reviewEvidence)
  const reviewRows = [
    'Candidate',
    'Commit',
    'Tree',
    'Diff digest',
    'Frozen',
    'Availability',
    'First seen revision',
    'Last seen revision',
    'Verdict',
  ].map(term => definitionRow(document, reviewList, term))

  const reviewEvidenceCollection = mountKeyedCollection<
    CandidateHistoricalReviewProjection['evidence'][number],
    string,
    HTMLLIElement
  >({
    parent: reviewEvidence,
    key: item => item.id,
    create: () => document.createElement('li'),
    update(item, entry) {
      item.dataset.evidenceId = entry.id
      setText(item, `${entry.type} · ${entry.id} · ${entry.sourceRef}`)
    },
  })

  let closed = false
  let reviewTarget: StageRunId | null = null
  let payloadKey: string | null = null
  let lastFingerprint: string | null = null
  let loadGeneration = 0
  let payloadController: AbortController | null = null
  let reviewGeneration = 0
  let reviewController: AbortController | null = null
  let runtimePanel: PanelStatus = 'idle'
  let candidatesPanel: PanelStatus = 'idle'
  let openCandidateKey: string | null = null
  let lastCandidateItems: readonly CandidateHistoryItemProjection[] = []

  function createCandidateRow(item: CandidateHistoryItemProjection): HTMLLIElement {
    const row = document.createElement('li')
    const button = document.createElement('button')
    button.type = 'button'
    button.className = 'wwc-strongflow-history-candidate'
    row.append(button)
    const state: CandidateRowState = {
      button,
      item,
      onClick: () => openCandidate(state),
    }
    button.addEventListener('click', state.onClick)
    candidateRows.set(row, state)
    return row
  }

  function updateCandidateRow(row: HTMLLIElement, item: CandidateHistoryItemProjection): void {
    const state = candidateRows.get(row)
    if (state === undefined) return
    state.item = item
    row.dataset.candidateRef = item.candidate.candidateRef
    row.dataset.availability = item.availability
    if (item.isCurrentAtReadCursor) row.dataset.current = 'true'
    else delete row.dataset.current
    state.button.textContent = `${item.candidate.candidateRef} · ${item.availability}`
    state.button.setAttribute(
      'aria-expanded',
      String(openCandidateKey === candidateIdentityKey(candidateIdentity(item))),
    )
  }

  function taskTitleOf(tree: StrongFlowHistoryTree, taskId: string | null): string {
    if (taskId === null) return 'Delivery-level'
    return tree.tasks.find(node => node.task.id === taskId)?.task.title ?? taskId
  }

  function setPanelStatus(
    status: HTMLElement,
    error: HTMLElement,
    retry: HTMLElement,
    state: PanelStatus,
    loadingText: string,
    errorText: string | null,
  ): void {
    status.hidden = state !== 'loading'
    if (state === 'loading') setText(status, loadingText)
    error.hidden = errorText === null
    if (errorText !== null) setText(error, errorText)
    retry.hidden = state !== 'error'
  }

  function resetRuntimePanel(state: PanelStatus): void {
    runtimePanel = state
    setPanelStatus(
      runtimeStatus,
      runtimeError,
      runtimeRetry,
      state,
      'Loading the exact runtime projection…',
      null,
    )
    runtimeEmpty.hidden = state !== 'unsupported'
    runtimeList.hidden = state !== 'ready'
    runtimeSessions.hidden = state !== 'ready'
    if (state !== 'ready') {
      applyDefinitions(runtimeRows, ['—', '—', '—', '—'])
      lastRunEvidence = []
      sessionsCollection.update([])
    }
  }

  function resetCandidatesPanel(state: PanelStatus): void {
    candidatesPanel = state
    setPanelStatus(
      candidatesStatus,
      candidatesError,
      candidatesRetry,
      state,
      'Loading the candidates produced by this run…',
      null,
    )
    lastCandidateItems = []
    candidatesEmpty.hidden = true
    if (state !== 'ready') candidatesCollection.update([])
  }

  function resetReviewPanel(state: PanelStatus): void {
    reviewHeading.hidden = state === 'idle'
    reviewHost.hidden = state === 'idle'
    reviewStatus.hidden = state !== 'loading'
    if (state === 'loading') setText(reviewStatus, 'Opening the historical candidate review…')
    reviewError.hidden = true
    reviewList.hidden = state !== 'ready'
    reviewEvidence.hidden = state !== 'ready'
    if (state !== 'ready') {
      applyDefinitions(reviewRows, reviewRows.map(() => '—'))
      reviewEvidenceCollection.update([])
    }
  }

  function abortCandidateReview(): void {
    reviewGeneration += 1
    reviewController?.abort()
    reviewController = null
  }

  function candidateReviewIsCurrent(
    generation: number,
    controller: AbortController,
    identityKey: string,
  ): boolean {
    return !closed
      && generation === reviewGeneration
      && controller === reviewController
      && !controller.signal.aborted
      && identityKey === openCandidateKey
  }

  function openCandidate(state: CandidateRowState): void {
    if (closed) return
    const identity = candidateIdentity(state.item)
    const identityKey = candidateIdentityKey(identity)
    abortCandidateReview()
    const controller = new AbortController()
    reviewController = controller
    const generation = reviewGeneration
    openCandidateKey = identityKey
    candidatesCollection.update(lastCandidateItems)
    resetReviewPanel('loading')
    void options.loaders.loadCandidateReview(identity, controller.signal).then(
      review => {
        if (!candidateReviewIsCurrent(generation, controller, identityKey)) return
        if (
          review === null
          || review.candidate.candidateRef !== identity.candidateRef
          || review.candidate.candidateTreeId !== identity.candidateTreeId
          || review.candidate.diffSha256 !== identity.diffSha256
        ) {
          reviewController = null
          resetReviewPanel('error')
          setText(reviewError, 'The historical candidate review could not be opened.')
          reviewError.hidden = false
          return
        }
        reviewController = null
        resetReviewPanel('ready')
        applyDefinitions(reviewRows, [
          review.candidate.candidateRef,
          review.candidate.candidateCommitId,
          review.candidate.candidateTreeId,
          review.candidate.diffSha256,
          review.candidate.frozenAt,
          String(review.availability),
          `r${String(review.firstSeenDeliveryRevision)}`,
          `r${String(review.lastSeenDeliveryRevision)}`,
          review.verdict === null
            ? 'None'
            : `${review.verdict.status} · ${review.verdict.producedAt}`,
        ])
        reviewEvidenceCollection.update([...review.evidence])
      },
      () => {
        if (!candidateReviewIsCurrent(generation, controller, identityKey)) return
        reviewController = null
        resetReviewPanel('error')
        setText(reviewError, 'The historical candidate review could not be opened.')
        reviewError.hidden = false
      },
    )
  }

  function runtimeFailure(): void {
    resetRuntimePanel('error')
    setText(runtimeError, 'The exact runtime projection for this StageRun is unavailable.')
    runtimeError.hidden = false
  }

  function loadRuntimeFor(stageRunId: StageRunId): void {
    const controller = payloadController
    if (controller === null) return
    resetRuntimePanel('loading')
    const generation = loadGeneration
    const current = () => !closed
      && generation === loadGeneration
      && controller === payloadController
      && !controller.signal.aborted
    void options.loaders.loadRuntime(stageRunId, controller.signal).then(
      snapshot => {
        if (!current()) return
        if (
          snapshot === null
          || snapshot.kind !== 'runtime_projection'
          || snapshot.stageRunId !== stageRunId
          || snapshot.deliveryId === null
        ) {
          runtimeFailure()
          return
        }
        runtimePanel = 'ready'
        setPanelStatus(runtimeStatus, runtimeError, runtimeRetry, 'ready', '', null)
        runtimeList.hidden = false
        runtimeSessions.hidden = false
        applyDefinitions(runtimeRows, [
          String(snapshot.revision),
          String(snapshot.lastProjectionSequence),
          snapshot.rebuiltAt,
          String(snapshot.sessions.length),
        ])
        sessionsCollection.update([...snapshot.sessions])
      },
      () => {
        if (current()) runtimeFailure()
      },
    )
  }

  function candidatesFailure(): void {
    resetCandidatesPanel('error')
    setText(candidatesError, 'The candidates of this StageRun could not be loaded.')
    candidatesError.hidden = false
  }

  function loadCandidatesFor(stageRunId: StageRunId): void {
    const controller = payloadController
    if (controller === null) return
    resetCandidatesPanel('loading')
    const generation = loadGeneration
    const current = () => !closed
      && generation === loadGeneration
      && controller === payloadController
      && !controller.signal.aborted
    void options.loaders.loadCandidates(stageRunId, controller.signal).then(
      items => {
        if (!current()) return
        candidatesPanel = 'ready'
        setPanelStatus(candidatesStatus, candidatesError, candidatesRetry, 'ready', '', null)
        lastCandidateItems = [...items]
        candidatesCollection.update(lastCandidateItems)
        candidatesEmpty.hidden = lastCandidateItems.length !== 0
      },
      () => {
        if (current()) candidatesFailure()
      },
    )
  }

  const onRuntimeRetry = () => {
    if (!closed && reviewTarget !== null && runtimePanel === 'error') loadRuntimeFor(reviewTarget)
  }
  const onCandidatesRetry = () => {
    if (!closed && reviewTarget !== null && candidatesPanel === 'error') {
      loadCandidatesFor(reviewTarget)
    }
  }
  runtimeRetry.addEventListener('click', onRuntimeRetry)
  candidatesRetry.addEventListener('click', onCandidatesRetry)

  function update(state: StrongFlowRunDetailState): void {
    if (closed) return
    const tree = state.tree
    const run = tree === null || state.selection.stageRunId === null
      ? null
      : tree.runs.find(candidate => candidate.stageRunId === state.selection.stageRunId) ?? null
    const reviewing = tree !== null && run !== null && !run.isCurrent
    const target = reviewing ? run.stageRunId : null
    const fingerprint = reviewing
      ? JSON.stringify([run, tree.currentCandidateRef, taskTitleOf(tree, run.deliveryTaskId)])
      : null
    const nextPayloadKey = reviewing
      ? JSON.stringify([run.stageRunId, tree.readCursor])
      : null
    if (nextPayloadKey === payloadKey && fingerprint === lastFingerprint) {
      // Equivalent snapshot: keep DOM identity, focus, and scroll untouched.
      return
    }
    const payloadChanged = nextPayloadKey !== payloadKey
    reviewTarget = target
    payloadKey = nextPayloadKey
    lastFingerprint = fingerprint
    if (!reviewing) {
      root.hidden = true
      if (payloadChanged) {
        loadGeneration += 1
        payloadController?.abort()
        payloadController = null
        abortCandidateReview()
        openCandidateKey = null
      }
      resetRuntimePanel('idle')
      resetCandidatesPanel('idle')
      resetReviewPanel('idle')
      return
    }
    if (payloadChanged) {
      loadGeneration += 1
      payloadController?.abort()
      payloadController = new AbortController()
      abortCandidateReview()
      openCandidateKey = null
      resetReviewPanel('idle')
      if (run.binding === null) {
        resetRuntimePanel('unsupported')
      } else {
        loadRuntimeFor(run.stageRunId)
      }
      loadCandidatesFor(run.stageRunId)
    }
    root.hidden = false
    root.dataset.stageRunId = run.stageRunId
    if (run.attempt !== null) root.dataset.attempt = String(run.attempt)
    else delete root.dataset.attempt
    applyDefinitions(identityRows, [
      run.stageRunId,
      run.attempt === null ? '—' : String(run.attempt),
      run.stage,
      run.role,
      run.actorType,
      taskTitleOf(tree, run.deliveryTaskId),
      run.status,
      run.startedAt,
      run.finishedAt ?? 'Not finished',
    ])
    bindingList.hidden = run.binding === null
    bindingEmpty.hidden = run.binding !== null
    if (run.binding !== null) {
      applyDefinitions(bindingRows, [
        run.binding.productSessionId,
        run.binding.executionJobId,
        run.binding.workerId ?? '—',
        run.binding.workerSessionId ?? '—',
        run.binding.codexThreadId ?? '—',
      ])
    }
    const boundedEvidence = boundedItems([...run.evidence], options.limits.evidence)
    evidenceCollection.update(boundedEvidence.items)
    evidenceOmitted.hidden = boundedEvidence.omitted === 0
    setText(evidenceOmitted, `${String(boundedEvidence.omitted)} more evidence records not shown.`)
    lastRunEvidence = boundedEvidence.items.map(strongFlowExecutionEvidenceLink)
    conclusion.dataset.status = run.status
    setText(conclusionStatus, run.status)
    setText(conclusionText, run.finishedAt === null
      ? 'This run has not finished yet.'
      : `Finished ${run.finishedAt}`)
  }

  return {
    root,
    update,
    close() {
      if (closed) return
      closed = true
      loadGeneration += 1
      payloadController?.abort()
      payloadController = null
      abortCandidateReview()
      runtimeRetry.removeEventListener('click', onRuntimeRetry)
      candidatesRetry.removeEventListener('click', onCandidatesRetry)
      evidenceCollection.close()
      sessionsCollection.close()
      candidatesCollection.close()
      reviewEvidenceCollection.close()
      // Release the last snapshot payloads so a closed view keeps no state.
      lastCandidateItems = []
      lastRunEvidence = []
      openCandidateKey = null
      reviewTarget = null
      payloadKey = null
      lastFingerprint = null
      root.remove?.()
    },
  }
}
