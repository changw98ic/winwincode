// SPDX-License-Identifier: Apache-2.0

import type {
  CandidateFileProjection,
  CandidateHistoricalReviewProjection,
  CandidateHistoryItemProjection,
  EvidenceId,
} from './generated/contracts.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import {
  candidateComparisonBaselineSide,
  candidateComparisonChoices,
  candidateComparisonDefaultRequest,
  compareCandidateReviews,
  resolveCandidateComparison,
  type CandidateComparisonChoice,
  type CandidateComparisonContext,
  type CandidateComparisonRequest,
  type CandidateComparisonResolution,
  type CandidateComparisonResult,
  type CandidateComparisonRouteSelection,
  type CandidateComparisonSide,
} from './strongflow-diff-model.js'
import { STRONGFLOW_COMPARISON_BASELINE_VALUE } from './strongflow-route.js'
import {
  boundedItems,
  strongFlowElement,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'
import type {
  StrongFlowCandidateFilesState,
  StrongFlowProjection,
} from './strongflow-view-model.js'

/**
 * Changed paths are a review aid, not an inventory: the bounded row window
 * keeps one reworked Candidate from rebuilding a very large Diff list.
 */
const COMPARISON_FILE_ROW_LIMIT = 50

const INVALID_LINK_MESSAGE =
  'This Candidate comparison link is not valid. Choose the Candidates to compare again.'

/** Exact server identity of one compared Candidate review read. */
export interface StrongFlowCandidateComparisonIdentity {
  readonly candidateRef: string
  readonly candidateTreeId: string
  readonly diffSha256: string
}

export interface StrongFlowCandidateComparisonOptions {
  readonly document: Document
  readonly limits: StrongFlowRenderLimits
  /** Reads the exact frozen Candidate history of the current Delivery. */
  readonly loadCandidates: (
    signal: AbortSignal,
  ) => Promise<readonly CandidateHistoryItemProjection[]>
  /** Reads one compared Candidate's display-only Evidence and Verdict facts. */
  readonly loadReview: (
    candidate: StrongFlowCandidateComparisonIdentity,
    signal: AbortSignal,
  ) => Promise<CandidateHistoricalReviewProjection | null>
  /** Publishes the selection so the canonical route stays shareable. */
  readonly onSelectionChange?: (request: CandidateComparisonRequest) => void
}

export interface StrongFlowCandidateComparisonProps {
  readonly projection: StrongFlowProjection | null
  readonly candidateFiles: StrongFlowCandidateFilesState
  /** Comparison carried by the canonical route, when one is present. */
  readonly requested: CandidateComparisonRouteSelection
  /** Digest of the Candidate frozen before an approved bounded rework. */
  readonly reworkBaselineDigest: string | null
}

export interface StrongFlowCandidateComparisonView {
  readonly root: HTMLElement
  update(props: StrongFlowCandidateComparisonProps): void
  close(): void
}

interface CurrentInventory {
  readonly candidateRef: string
  readonly files: readonly CandidateFileProjection[]
  readonly known: boolean
}

interface ComparisonOptionRow {
  readonly value: string
  readonly label: string
}

interface FileRow {
  readonly path: string
  readonly kind: 'added' | 'removed' | 'changed'
}

interface EvidenceRow {
  readonly id: EvidenceId
  readonly kind: 'added' | 'removed'
}

/**
 * Build one compared review cut. Only the Delivery's current Candidate owns a
 * readable changed-file inventory, so every other side stays without one
 * instead of inventing paths the Delivery never delivered.
 */
export function strongFlowCandidateComparisonSide(
  choice: CandidateComparisonChoice | null,
  review: CandidateHistoricalReviewProjection | null,
  current: CurrentInventory | null,
): CandidateComparisonSide {
  if (choice === null) return candidateComparisonBaselineSide()
  const inventory = current !== null
    && current.known
    && current.candidateRef === choice.candidate.candidateRef
    ? current.files
    : null
  return Object.freeze({
    role: 'candidate',
    candidate: choice.candidate,
    availability: choice.availability,
    files: inventory === null ? null : Object.freeze([...inventory]),
    evidenceIds: Object.freeze((review?.evidence ?? []).map(item => item.id)),
    verdict: review?.verdict ?? null,
  })
}

function candidateLabel(choice: CandidateComparisonChoice): string {
  const current = choice.isCurrent ? ' · current' : ''
  const released = choice.availability === 'released' ? ' · released' : ''
  return `${choice.candidate.candidateRef}${current}${released}`
}

function formatCount(value: number): string {
  return value.toLocaleString('en-US')
}

/** Mount the bounded Candidate comparison workbench for one Delivery. */
export function mountStrongFlowCandidateComparison(
  options: StrongFlowCandidateComparisonOptions,
): StrongFlowCandidateComparisonView {
  const { document } = options
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-candidate-comparison')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const empty = strongFlowElement(document, 'p', 'wwc-strongflow-candidate-comparison-empty')
  // ADR-0029 §5: a rejection is an error condition, so it carries a non-color
  // icon and role="alert" while the page's single polite live region stays
  // reserved for progress announcements.
  const alert = strongFlowElement(document, 'div', 'wwc-strongflow-candidate-comparison-alert')
  const alertIcon = strongFlowElement(
    document,
    'span',
    'wwc-strongflow-candidate-comparison-alert-icon',
  )
  const alertText = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-candidate-comparison-alert-text',
  )
  const controls = strongFlowElement(
    document,
    'div',
    'wwc-strongflow-candidate-comparison-controls',
  )
  const fromLabel = document.createElement('label')
  const from = strongFlowElement(
    document,
    'select',
    'wwc-strongflow-candidate-comparison-from',
  ) as HTMLSelectElement
  const toLabel = document.createElement('label')
  const to = strongFlowElement(
    document,
    'select',
    'wwc-strongflow-candidate-comparison-to',
  ) as HTMLSelectElement
  const status = strongFlowElement(document, 'p', 'wwc-strongflow-candidate-comparison-status')
  const summary = strongFlowElement(document, 'dl', 'wwc-strongflow-candidate-comparison-summary')

  function summaryRow(termText: string, className: string): HTMLElement {
    const row = strongFlowElement(document, 'div', 'wwc-strongflow-candidate-comparison-row')
    const term = document.createElement('dt')
    const value = strongFlowElement(document, 'dd', className)
    term.textContent = termText
    row.append(term, value)
    summary.append(row)
    return value
  }

  const files = summaryRow('Changed files', 'wwc-strongflow-candidate-comparison-files')
  const filesSummary = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-candidate-comparison-files-summary',
  )
  const filesUnavailable = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-candidate-comparison-files-unavailable',
  )
  const filesOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const fileList = strongFlowElement(
    document,
    'ul',
    'wwc-strongflow-candidate-comparison-file-list',
  )
  const diff = summaryRow('Diff', 'wwc-strongflow-candidate-comparison-diff')
  const evidence = summaryRow('Evidence', 'wwc-strongflow-candidate-comparison-evidence')
  const evidenceSummary = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-candidate-comparison-evidence-summary',
  )
  const evidenceOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const evidenceList = strongFlowElement(
    document,
    'ul',
    'wwc-strongflow-candidate-comparison-evidence-list',
  )
  const verdict = summaryRow('Verdict', 'wwc-strongflow-candidate-comparison-verdict')

  heading.textContent = 'Candidate comparison'
  empty.textContent = 'No frozen Candidate can be compared yet.'
  empty.hidden = true
  alertIcon.setAttribute('aria-hidden', 'true')
  alertIcon.textContent = '!'
  alert.setAttribute('role', 'alert')
  alert.append(alertIcon, alertText)
  alert.hidden = true
  fromLabel.textContent = 'Compare from'
  fromLabel.append(from)
  toLabel.textContent = 'Compare to'
  toLabel.append(to)
  controls.append(fromLabel, toLabel)
  filesUnavailable.hidden = true
  filesOmitted.hidden = true
  evidenceOmitted.hidden = true
  files.append(filesSummary, filesUnavailable, fileList, filesOmitted)
  evidence.append(evidenceSummary, evidenceList, evidenceOmitted)
  summary.hidden = true
  status.hidden = true
  root.append(heading, empty, alert, controls, status, summary)

  const fromOptions = mountKeyedCollection<ComparisonOptionRow, string, HTMLOptionElement>({
    parent: from,
    key: row => row.value,
    create: () => document.createElement('option'),
    update(option, row) {
      option.value = row.value
      option.textContent = row.label
    },
  })
  const toOptions = mountKeyedCollection<ComparisonOptionRow, string, HTMLOptionElement>({
    parent: to,
    key: row => row.value,
    create: () => document.createElement('option'),
    update(option, row) {
      option.value = row.value
      option.textContent = row.label
    },
  })
  const fileRows = mountKeyedCollection<FileRow, string, HTMLLIElement>({
    parent: fileList,
    key: row => `${row.kind}:${row.path}`,
    create: () => document.createElement('li'),
    update(row, current_) {
      row.dataset.kind = current_.kind
      row.dataset.path = current_.path
      row.textContent = `${current_.kind === 'added' ? '+' : current_.kind === 'removed' ? '−' : '~'} ${current_.path}`
    },
  })
  const evidenceRows = mountKeyedCollection<EvidenceRow, string, HTMLLIElement>({
    parent: evidenceList,
    key: row => `${row.kind}:${row.id}`,
    create: () => document.createElement('li'),
    update(row, current_) {
      row.dataset.kind = current_.kind
      row.textContent = `${current_.kind === 'added' ? '+' : '−'} ${current_.id}`
    },
  })

  let open = true
  let current: StrongFlowCandidateComparisonProps | null = null
  let choices: readonly CandidateComparisonChoice[] = []
  let historyDeliveryId: string | null = null
  let historyLoading = false
  let historyToken = 0
  let sideToken = 0
  let historyController: AbortController | null = null
  let reviewController: AbortController | null = null
  /** Review reads cached for one comparison, so repeated snapshots never refetch. */
  let cachedReviews: {
    readonly key: string
    readonly before: CandidateHistoricalReviewProjection | null
    readonly after: CandidateHistoricalReviewProjection | null
  } | null = null

  function setAlert(message: string | null): void {
    const visible = message !== null
    if (alert.hidden !== !visible) alert.hidden = !visible
    if (alertText.textContent !== (message ?? '')) alertText.textContent = message ?? ''
  }

  function setStatus(message: string | null): void {
    const visible = message !== null
    if (status.hidden !== !visible) status.hidden = !visible
    if (status.textContent !== (message ?? '')) status.textContent = message ?? ''
  }

  function comparisonContext(): CandidateComparisonContext {
    const projection = current?.projection ?? null
    return {
      deliverySpecId: projection?.delivery.requirements.deliverySpecId ?? '',
      deliverySpecRevision: projection?.delivery.requirements.deliverySpecRevision ?? 0,
      choices,
    }
  }

  function selectableChoices(
    context_: CandidateComparisonContext,
  ): readonly CandidateComparisonChoice[] {
    return context_.choices.filter(choice => (
      choice.candidate.deliverySpecId === context_.deliverySpecId
      && choice.candidate.deliverySpecRevision === context_.deliverySpecRevision
    ))
  }

  function renderOptions(selectable: readonly CandidateComparisonChoice[]): void {
    fromOptions.update([
      {
        value: STRONGFLOW_COMPARISON_BASELINE_VALUE,
        label: 'Delivery baseline (base revision)',
      },
      ...selectable.map(choice => ({
        value: choice.candidate.candidateRef,
        label: candidateLabel(choice),
      })),
    ])
    toOptions.update(selectable.map(choice => ({
      value: choice.candidate.candidateRef,
      label: candidateLabel(choice),
    })))
  }

  function selectOptions(request: CandidateComparisonRequest): void {
    from.value = request.before === null
      ? STRONGFLOW_COMPARISON_BASELINE_VALUE
      : request.before.candidateRef
    to.value = request.after.candidateRef
  }

  function currentInventory(projection: StrongFlowProjection | null): CurrentInventory | null {
    const candidate = projection?.currentCandidate ?? null
    if (candidate === null || current === null) return null
    return {
      candidateRef: candidate.candidateRef,
      files: current.candidateFiles.items,
      known: current.candidateFiles.status === 'ready',
    }
  }

  function renderSummary(result: CandidateComparisonResult): void {
    files.dataset.known = String(result.files.known)
    files.dataset.added = String(result.files.added.length)
    files.dataset.removed = String(result.files.removed.length)
    files.dataset.changed = String(result.files.changed.length)
    filesSummary.textContent = result.files.known
      ? `${formatCount(result.files.added.length)} added · ${formatCount(
        result.files.removed.length,
      )} removed · ${formatCount(result.files.changed.length)} changed · +${formatCount(
        result.files.additions ?? 0,
      )} −${formatCount(result.files.deletions ?? 0)}`
      : 'The compared pair has no readable changed-file inventory.'
    filesUnavailable.hidden = result.files.known
    fileList.hidden = result.files.changes.length === 0
    const boundedFiles = boundedItems(result.files.changes, COMPARISON_FILE_ROW_LIMIT)
    fileRows.update(boundedFiles.items)
    const filesHidden = boundedFiles.omitted === 0
    if (filesOmitted.hidden !== filesHidden) filesOmitted.hidden = filesHidden
    filesOmitted.textContent = filesHidden
      ? ''
      : `${formatCount(boundedFiles.omitted)} more compared changed paths not rendered.`
    diff.dataset.changed = String(result.diffChanged)
    diff.textContent = result.diffChanged
      ? 'The compared Candidate Diff differs.'
      : 'Both compared Candidates share one Diff digest.'
    evidence.dataset.added = String(result.evidence.added.length)
    evidence.dataset.removed = String(result.evidence.removed.length)
    evidenceSummary.textContent = `${formatCount(result.evidence.added.length)} added · ${formatCount(
      result.evidence.removed.length,
    )} removed · ${formatCount(result.evidence.unchangedCount)} unchanged`
    const boundedEvidence = boundedItems(
      [
        ...result.evidence.added.map(id => ({ id, kind: 'added' as const })),
        ...result.evidence.removed.map(id => ({ id, kind: 'removed' as const })),
      ],
      options.limits.evidence,
    )
    evidenceList.hidden = boundedEvidence.items.length === 0
    evidenceRows.update(boundedEvidence.items)
    const evidenceHidden = boundedEvidence.omitted === 0
    if (evidenceOmitted.hidden !== evidenceHidden) evidenceOmitted.hidden = evidenceHidden
    evidenceOmitted.textContent = evidenceHidden
      ? ''
      : `${formatCount(boundedEvidence.omitted)} more Evidence records not rendered.`
    verdict.dataset.changed = String(result.verdict.changed)
    verdict.textContent = [
      result.verdict.beforeStatus === null
        ? 'No Verdict before'
        : `${result.verdict.beforeStatus} before`,
      result.verdict.afterStatus === null
        ? 'no Verdict after'
        : `${result.verdict.afterStatus} after`,
      ...result.verdict.criteria.map(criterion => `${criterion.criterionId} ${
        criterion.before ?? 'none'
      } → ${criterion.after ?? 'none'}`),
    ].join(' · ')
  }

  function clearSummary(): void {
    summary.hidden = true
    cachedReviews = null
    fileRows.update([])
    evidenceRows.update([])
  }

  /** The compared pair, independent of the change inventory that may grow. */
  function requestKey(
    resolution: Extract<CandidateComparisonResolution, { status: 'resolved' }>,
  ): string {
    return JSON.stringify([
      resolution.before?.candidate.candidateRef ?? null,
      resolution.after.candidate.candidateRef,
      resolution.before?.candidate.diffSha256 ?? null,
      resolution.after.candidate.diffSha256,
    ])
  }

  function loadReviews(
    resolution: Extract<CandidateComparisonResolution, { status: 'resolved' }>,
    key: string,
    token: number,
  ): void {
    reviewController?.abort()
    reviewController = new AbortController()
    const beforeReview = resolution.before === null
      ? Promise.resolve(null)
      : options.loadReview(resolution.before.candidate, reviewController.signal)
    const afterReview = options.loadReview(resolution.after.candidate, reviewController.signal)
    void Promise.all([beforeReview, afterReview]).then(([before, after]) => {
      if (!open || token !== sideToken) return
      cachedReviews = { key, before, after }
      // Repaint through render so the summary is rebuilt from the cached reads
      // together with the current change inventory.
      render()
    }).catch(() => {
      if (!open || token !== sideToken) return
      cachedReviews = null
      summary.hidden = true
      setStatus('The compared Candidate review could not be loaded. Choose the comparison again.')
    })
  }

  function render(): void {
    const props = current
    if (props === null) return
    const projection = props.projection
    if (projection === null) {
      empty.hidden = false
      controls.hidden = true
      setAlert(null)
      setStatus(null)
      fromOptions.update([])
      toOptions.update([])
      clearSummary()
      return
    }
    const context_ = comparisonContext()
    const selectable = selectableChoices(context_)
    renderOptions(selectable)
    if (historyLoading) {
      empty.hidden = true
      controls.hidden = selectable.length === 0
      summary.hidden = true
      setAlert(null)
      setStatus('Loading the frozen Candidates of this Delivery…')
      return
    }
    const defaults = {
      reworkBaselineDigest: props.reworkBaselineDigest,
      reworkStage: projection.delivery.stages.some(stage => stage.stage === 'reworking'),
    }
    // The selectors only ever offer the Candidates this Delivery exposes, so
    // the default pair is drawn from that same bounded set.
    const fallback = candidateComparisonDefaultRequest(
      { ...context_, choices: selectable },
      defaults,
    )
    if (fallback === null) {
      empty.hidden = false
      controls.hidden = true
      summary.hidden = true
      setAlert(null)
      setStatus(null)
      return
    }
    // A rejected or missing link never renders a comparison: the selectors
    // recover to the Delivery default instead of the dead pair.
    if (props.requested.status === 'invalid') {
      empty.hidden = true
      controls.hidden = false
      selectOptions(fallback)
      clearSummary()
      setStatus(null)
      setAlert(INVALID_LINK_MESSAGE)
      return
    }
    const request = props.requested.status === 'requested' ? props.requested.request : fallback
    const resolution = resolveCandidateComparison(context_, request)
    if (resolution.status === 'rejected') {
      empty.hidden = true
      controls.hidden = false
      selectOptions(fallback)
      clearSummary()
      setStatus(null)
      setAlert(resolution.rejection.message)
      return
    }
    empty.hidden = true
    controls.hidden = false
    setAlert(null)
    selectOptions(request)
    const inventory = currentInventory(projection)
    const key = requestKey(resolution)
    const cached = cachedReviews !== null && cachedReviews.key === key
      ? cachedReviews
      : null
    if (cached === null) {
      clearSummary()
      setStatus('Loading the compared Candidate review…')
      const token = sideToken + 1
      sideToken = token
      loadReviews(resolution, key, token)
      return
    }
    // The sides are rebuilt on every snapshot, so a change inventory that grew
    // with more loaded files is reported without reading the reviews again.
    const before = strongFlowCandidateComparisonSide(
      resolution.before,
      cached.before,
      inventory,
    )
    const after = strongFlowCandidateComparisonSide(
      resolution.after,
      cached.after,
      inventory,
    )
    summary.hidden = false
    setStatus(null)
    renderSummary(compareCandidateReviews(before, after))
  }

  async function loadHistory(): Promise<void> {
    const token = historyToken + 1
    historyToken = token
    historyLoading = true
    historyController?.abort()
    historyController = new AbortController()
    try {
      const items = await options.loadCandidates(historyController.signal)
      if (!open || token !== historyToken) return
      choices = candidateComparisonChoices(items)
      historyLoading = false
      render()
    } catch {
      if (!open || token !== historyToken) return
      choices = []
      historyLoading = false
      render()
    }
  }

  function publishSelection(): void {
    const selectable = selectableChoices(comparisonContext())
    const after = selectable.find(choice => choice.candidate.candidateRef === to.value)
    if (after === undefined || current === null) return
    const baseline = from.value === STRONGFLOW_COMPARISON_BASELINE_VALUE
    let before: CandidateComparisonRequest['before'] = null
    if (!baseline) {
      const found = selectable.find(choice => choice.candidate.candidateRef === from.value)
      if (found === undefined) return
      before = Object.freeze({
        candidateRef: found.candidate.candidateRef,
        diffSha256: found.candidate.diffSha256,
      })
    }
    const request: CandidateComparisonRequest = Object.freeze({
      before,
      after: Object.freeze({
        candidateRef: after.candidate.candidateRef,
        diffSha256: after.candidate.diffSha256,
      }),
    })
    // The panel shows its own selection immediately; the canonical route
    // follows through the page so the link stays shareable.
    current = { ...current, requested: { status: 'requested', request } }
    options.onSelectionChange?.(request)
    render()
  }

  const onFromChange = () => {
    publishSelection()
  }
  const onToChange = () => {
    publishSelection()
  }
  from.addEventListener('change', onFromChange)
  to.addEventListener('change', onToChange)

  function emptyCandidateFiles(): StrongFlowCandidateFilesState {
    return {
      status: 'idle',
      items: [],
      hasMore: false,
      previewLimited: false,
      selectedPath: null,
      diff: {
        status: 'idle',
        path: null,
        content: '',
        loadedBytes: 0,
        totalBytes: null,
        hasMore: false,
        previewLimited: false,
        fileDiffSha256: null,
        unavailableReason: null,
        error: null,
      },
      error: null,
    }
  }

  function update(props: StrongFlowCandidateComparisonProps): void {
    if (!open) throw new Error('StrongFlow Candidate comparison view is closed.')
    const deliveryId = props.projection?.delivery.deliveryId ?? null
    current = props
    if (props.projection === null) {
      historyToken += 1
      sideToken += 1
      historyController?.abort()
      reviewController?.abort()
      historyController = null
      reviewController = null
      historyDeliveryId = null
      choices = []
      historyLoading = false
      render()
      return
    }
    if (deliveryId !== historyDeliveryId) {
      historyDeliveryId = deliveryId
      choices = []
      historyLoading = true
      render()
      void loadHistory()
      return
    }
    render()
  }

  update({
    projection: null,
    candidateFiles: emptyCandidateFiles(),
    requested: { status: 'none' },
    reworkBaselineDigest: null,
  })

  return {
    root,
    update,
    close() {
      if (!open) return
      open = false
      historyToken += 1
      sideToken += 1
      historyController?.abort()
      reviewController?.abort()
      from.removeEventListener('change', onFromChange)
      to.removeEventListener('change', onToChange)
      fromOptions.close()
      toOptions.close()
      fileRows.close()
      evidenceRows.close()
      root.remove()
    },
  }
}
