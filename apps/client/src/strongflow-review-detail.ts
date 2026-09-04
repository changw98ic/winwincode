// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './control-plane-client.js'
import type {
  Actor,
  DeliveryCriterionResultProjection,
  DeliveryEvidenceProjection,
  EvidenceId,
  PublicationDetailProjection,
  PublicationGetResultResponse,
  PublicationId,
  PublicationResourceRef,
  PublicationStepProjection,
  PublicationStatusHistoryProjection,
  PublicationTarget,
  RepositoryScope,
  RequestId,
} from './generated/contracts.js'
import { QueryName } from './generated/contracts.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import {
  boundedItems,
  DEFAULT_STRONGFLOW_RENDER_LIMITS,
  strongFlowElement,
} from './strongflow-rendering.js'
import type { StrongFlowProjection, StrongFlowViewModel } from './strongflow-view-model.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const RECEIPT_PAGE = Object.freeze({ cursor: null, limit: 1 })
const MAX_SUMMARY_VALUE_CHARS = 2_000
const MAX_SUMMARY_CHARS = 20_000
const DEFAULT_CRITERION_ROWS = 50
const DEFAULT_RECEIPT_HISTORY = 20
const REDACTED = '[redacted]'

/** Outcome of one AcceptanceCriterion as the sealed Verdict reports it. */
export type StrongFlowCriterionReviewOutcome =
  | 'pass'
  | 'fail'
  | 'inconclusive'
  | 'infra_error'
  | 'not_evaluated'

/** Read state of the verified Publication journal behind one receipt. */
export type StrongFlowReceiptStatus = 'not_loaded' | 'loading' | 'ready' | 'error'

type KnownVerdictStatus = Exclude<StrongFlowCriterionReviewOutcome, 'not_evaluated'>

export interface StrongFlowCriterionEvidenceReference {
  readonly evidenceId: EvidenceId
  readonly type: DeliveryEvidenceProjection['type']
  readonly sourceRef: string
  /** True only when the exact record exists at this Delivery read cursor. */
  readonly openable: boolean
}

export interface StrongFlowCriterionReviewRow {
  readonly criterionId: string
  readonly description: string
  readonly required: boolean
  readonly verificationMethod: string | null
  readonly outcome: StrongFlowCriterionReviewOutcome
  readonly explanation: string | null
  readonly evaluatedAt: string | null
  readonly evidence: readonly StrongFlowCriterionEvidenceReference[]
}

/** One Verdict result that no AcceptanceCriterion in the spec asked for. */
export interface StrongFlowUnmatchedCriterionResult {
  readonly criterionId: string
  readonly outcome: StrongFlowCriterionReviewOutcome
  readonly explanation: string | null
}

export interface StrongFlowVerdictReason {
  readonly criterionId: string
  readonly outcome: StrongFlowCriterionReviewOutcome
  readonly explanation: string
}

export interface StrongFlowVerdictReview {
  readonly verdictId: string | null
  readonly status: StrongFlowCriterionReviewOutcome
  readonly producedAt: string | null
  readonly candidateRef: string | null
  readonly unresolvedFindings: readonly string[]
  /** Why this status was reached: exactly one entry per non-pass criterion. */
  readonly reasons: readonly StrongFlowVerdictReason[]
}

export interface StrongFlowTechnicalEntry {
  readonly label: string
  readonly value: string
}

export interface StrongFlowTechnicalIdentity {
  /** Closed, secret-free identity fields in a stable display order. */
  readonly entries: readonly StrongFlowTechnicalEntry[]
  readonly evidenceReferences: readonly StrongFlowCriterionEvidenceReference[]
  readonly omittedEvidenceReferences: number
}

export interface StrongFlowPublicationExternalReference {
  readonly kind: PublicationResourceRef['kind']
  readonly repository: string
  readonly number: number
}

export interface StrongFlowPublicationReceiptRevision {
  readonly revision: number
  readonly state: string
  readonly updatedAt: string
  readonly retryable: boolean
  readonly cancellable: boolean
  readonly steps: readonly { readonly kind: string; readonly state: string }[]
}

export interface StrongFlowPublicationReceiptStep {
  readonly kind: string
  readonly state: string
  readonly outcomeCode: string | null
  readonly remoteWritePerformed: boolean | null
  readonly resourceRef: StrongFlowPublicationExternalReference | null
  readonly retryable: boolean
}

export interface StrongFlowPublicationReceipt {
  readonly present: boolean
  readonly publicationId: string | null
  readonly state: string | null
  readonly revision: number | null
  readonly updatedAt: string | null
  readonly deliveryVerdictId: string | null
  readonly approvedBy: string | null
  readonly approvedAt: string | null
  readonly publicationSetSha256: string | null
  readonly target: PublicationTarget | null
  readonly externalReferences: readonly StrongFlowPublicationExternalReference[]
  readonly detailStatus: StrongFlowReceiptStatus
  /** Known only from the verified Publication journal; null while unknown. */
  readonly retryable: boolean | null
  readonly cancellable: boolean | null
  readonly history: readonly StrongFlowPublicationReceiptRevision[]
  readonly steps: readonly StrongFlowPublicationReceiptStep[]
  readonly historyTruncated: boolean
}

export interface StrongFlowReviewDetail {
  /** The one-line delivery conclusion the main view leads with. */
  readonly conclusion: string
  readonly verdict: StrongFlowVerdictReview
  readonly criteria: readonly StrongFlowCriterionReviewRow[]
  readonly omittedCriteria: number
  readonly unmatchedResults: readonly StrongFlowUnmatchedCriterionResult[]
  readonly technical: StrongFlowTechnicalIdentity
  readonly publication: StrongFlowPublicationReceipt
}

export interface StrongFlowReviewDetailLimits {
  readonly criteria: number
  readonly evidence: number
  readonly receiptHistory: number
}

export interface StrongFlowReviewDetailOptions {
  readonly limits?: Partial<StrongFlowReviewDetailLimits>
  /** Receipt detail already read from the verified Publication journal. */
  readonly receiptDetail?: PublicationDetailProjection | null
  /** Read state of `receiptDetail`; derived from it when omitted. */
  readonly receiptStatus?: StrongFlowReceiptStatus
}

const VERDICT_STATUS_LABELS: Readonly<Record<KnownVerdictStatus, string>> = Object.freeze({
  pass: 'passed',
  fail: 'failed',
  inconclusive: 'inconclusive',
  infra_error: 'infrastructure error',
})

const CRITERION_OUTCOME_LABELS: Readonly<Record<StrongFlowCriterionReviewOutcome, string>> =
  Object.freeze({
    pass: 'Passed',
    fail: 'Failed',
    inconclusive: 'Inconclusive',
    infra_error: 'Infrastructure error',
    not_evaluated: 'Not evaluated',
  })

const SECRET_PATTERNS: readonly RegExp[] = Object.freeze([
  /sk-[A-Za-z0-9_-]{8,}/gu,
  /gh[pousr]_[A-Za-z0-9]{16,}/gu,
  /github_pat_[A-Za-z0-9_]{20,}/gu,
  /xox[baprs]-[A-Za-z0-9-]{10,}/gu,
  /AKIA[0-9A-Z]{16}/gu,
  /Bearer\s+[A-Za-z0-9._~+/=-]{12,}/giu,
  /(?:api[_-]?key|password|secret|token)\s*[:=]\s*[^\s;,]+/giu,
])

function verdictStatusLabel(status: StrongFlowCriterionReviewOutcome): string {
  return status === 'not_evaluated' ? 'not computed' : VERDICT_STATUS_LABELS[status]
}

/** Canonical short label for one criterion outcome. */
export function strongFlowCriterionOutcomeLabel(
  outcome: StrongFlowCriterionReviewOutcome,
): string {
  return CRITERION_OUTCOME_LABELS[outcome]
}

function defaultLimits(
  limits?: Partial<StrongFlowReviewDetailLimits>,
): StrongFlowReviewDetailLimits {
  return {
    criteria: limits?.criteria ?? DEFAULT_CRITERION_ROWS,
    evidence: limits?.evidence ?? DEFAULT_STRONGFLOW_RENDER_LIMITS.evidence,
    receiptHistory: limits?.receiptHistory ?? DEFAULT_RECEIPT_HISTORY,
  }
}

function externalReferenceOf(
  reference: PublicationResourceRef | null | undefined,
): StrongFlowPublicationExternalReference | null {
  if (reference === null || reference === undefined) return null
  return { kind: reference.kind, repository: reference.repository, number: reference.number }
}

/** True for a closed fact this panel can show; an omitted field is not a fact. */
function isKnown(value: string | null | undefined): value is string {
  return value !== null && value !== undefined && value.length > 0
}

function externalReferencesOf(
  publication: NonNullable<StrongFlowProjection['publication']>,
): readonly StrongFlowPublicationExternalReference[] {
  const reference = externalReferenceOf(publication.resourceRef)
  return reference === null ? [] : [reference]
}

function receiptStepsOf(
  steps: readonly PublicationStepProjection[],
): readonly StrongFlowPublicationReceiptStep[] {
  return steps.map(step => ({
    kind: step.kind,
    state: step.state,
    outcomeCode: step.outcomeCode,
    remoteWritePerformed: step.remoteWritePerformed,
    resourceRef: externalReferenceOf(step.resourceRef),
    retryable: step.retryable,
  }))
}

function receiptHistoryOf(
  history: readonly PublicationStatusHistoryProjection[],
  limit: number,
): readonly StrongFlowPublicationReceiptRevision[] {
  return boundedItems(history, limit).items.map(entry => ({
    revision: entry.revision,
    state: entry.state,
    updatedAt: entry.updatedAt,
    retryable: entry.retryable,
    cancellable: entry.cancellable,
    steps: entry.stepStates.map(step => ({ kind: step.kind, state: step.state })),
  }))
}

function buildReceipt(
  publication: StrongFlowProjection['publication'],
  detail: PublicationDetailProjection | null,
  status: StrongFlowReceiptStatus | undefined,
  historyLimit: number,
): StrongFlowPublicationReceipt {
  // The Delivery snapshot carries the Publication state at its own read cut,
  // while `publication.get` reports the verified journal, which can be one or
  // more revisions ahead. They describe the same receipt only when the closed
  // identity matches; then the journal summary is the newer truth.
  const matched = publication !== null && detail !== null && (
    detail.summary.id === publication.id
    && detail.summary.deliveryId === publication.deliveryId
    && detail.summary.candidateRef === publication.candidateRef
    && detail.summary.deliveryVerdictId === publication.deliveryVerdictId
  )
  const summary = matched ? detail.summary : publication
  const detailStatus: StrongFlowReceiptStatus = detail === null
    ? status ?? 'not_loaded'
    : matched
      ? status ?? 'ready'
      : 'error'

  return {
    present: summary !== null,
    publicationId: summary?.id ?? null,
    state: summary?.state ?? null,
    revision: summary?.revision ?? null,
    updatedAt: summary?.updatedAt ?? null,
    deliveryVerdictId: summary?.deliveryVerdictId ?? null,
    approvedBy: summary?.approvedBy ?? null,
    approvedAt: summary?.approvedAt ?? null,
    publicationSetSha256: summary?.publicationSetSha256 ?? null,
    target: summary?.target ?? null,
    externalReferences: summary === null ? [] : externalReferencesOf(summary),
    detailStatus,
    retryable: matched ? detail.retryable : null,
    cancellable: matched ? detail.cancellable : null,
    history: matched ? receiptHistoryOf(detail.history, historyLimit) : [],
    steps: matched ? receiptStepsOf(detail.steps) : [],
    historyTruncated: matched
      ? detail.historyTruncated || detail.history.length > historyLimit
      : false,
  }
}

/**
 * The one review detail read model: delivery conclusion first, then the
 * technical identity, the per-criterion Verdict results and the Publication
 * receipt. Every exposed fact comes from the closed StrongFlow projection plus
 * the verified Publication journal; nothing here derives a business fact.
 */
export function strongFlowReviewDetail(
  projection: StrongFlowProjection,
  options: StrongFlowReviewDetailOptions = {},
): StrongFlowReviewDetail {
  const limits = defaultLimits(options.limits)
  const candidate = projection.currentCandidate
  const verdict = projection.verdict
  const publication = projection.publication
  // The closed contract always carries these arrays; a thin or historical
  // snapshot must still render, so a missing array reads as empty.
  const evidenceRecords = projection.evidence ?? []
  const evidenceById = new Map<string, DeliveryEvidenceProjection>(
    evidenceRecords.map(item => [item.id, item]),
  )

  function evidenceReferencesOf(
    refs: readonly EvidenceId[],
  ): readonly StrongFlowCriterionEvidenceReference[] {
    const references: StrongFlowCriterionEvidenceReference[] = []
    for (const ref of refs) {
      const record = evidenceById.get(ref)
      if (record === undefined) continue
      references.push({
        evidenceId: ref,
        type: record.type,
        sourceRef: record.sourceRef,
        openable: true,
      })
    }
    return references
  }

  const resultsById = new Map<string, DeliveryCriterionResultProjection>(
    (verdict?.criteria ?? []).map(result => [result.criterionId, result]),
  )
  const boundedCriteria = boundedItems(
    projection.delivery.requirements.acceptanceCriteria ?? [],
    limits.criteria,
  )
  const criteria = boundedCriteria.items.map((criterion): StrongFlowCriterionReviewRow => {
    const result = resultsById.get(criterion.id)
    return {
      criterionId: criterion.id,
      description: criterion.description,
      required: criterion.required,
      verificationMethod: criterion.verificationMethod,
      outcome: result?.verdict ?? 'not_evaluated',
      explanation: result?.explanation ?? null,
      evaluatedAt: result?.evaluatedAt ?? null,
      evidence: evidenceReferencesOf(result?.evidenceRefs ?? []),
    }
  })
  const matchedIds = new Set(boundedCriteria.items.map(criterion => criterion.id))
  const unmatchedResults = (verdict?.criteria ?? [])
    .filter(result => !matchedIds.has(result.criterionId))
    .map((result): StrongFlowUnmatchedCriterionResult => ({
      criterionId: result.criterionId,
      outcome: result.verdict,
      explanation: result.explanation,
    }))

  const verdictReview: StrongFlowVerdictReview = {
    verdictId: verdict?.id ?? null,
    status: verdict?.status ?? 'not_evaluated',
    producedAt: verdict?.producedAt ?? null,
    candidateRef: verdict?.candidateRef ?? null,
    unresolvedFindings: verdict?.unresolvedFindings ?? [],
    reasons: (verdict?.criteria ?? [])
      .filter(result => result.verdict !== 'pass')
      .map((result): StrongFlowVerdictReason => ({
        criterionId: result.criterionId,
        outcome: result.verdict,
        explanation: result.explanation,
      })),
  }

  const technicalEntries: StrongFlowTechnicalEntry[] = [
    {
      label: 'Delivery',
      value: `${projection.delivery.deliveryId} r${String(projection.delivery.deliveryRevision)}`,
    },
    {
      label: 'Delivery spec',
      value: `${projection.delivery.requirements.deliverySpecId} r${String(
        projection.delivery.requirements.deliverySpecRevision,
      )}`,
    },
  ]
  if (candidate !== null) {
    technicalEntries.push(
      { label: 'Candidate reference', value: candidate.candidateRef },
      { label: 'Candidate commit', value: candidate.candidateCommitId },
      { label: 'Candidate tree', value: candidate.candidateTreeId },
      { label: 'Candidate Diff digest', value: candidate.diffSha256 },
    )
  }
  if (projection.solutionReview !== null && isKnown(projection.solutionReview.reviewSetSha256)) {
    technicalEntries.push({
      label: 'Approved review set digest',
      value: projection.solutionReview.reviewSetSha256,
    })
  }
  if (publication !== null && isKnown(publication.publicationSetSha256)) {
    technicalEntries.push({
      label: 'Publication set digest',
      value: publication.publicationSetSha256,
    })
  }

  const boundedEvidence = boundedItems(evidenceRecords, limits.evidence)
  const publicationLabel = publication === null
    ? 'not created'
    : publication.state ?? 'unknown'

  return {
    conclusion: `Delivery ${projection.delivery.status} · Verdict ${
      verdictStatusLabel(verdictReview.status)
    } · Publication ${publicationLabel}`,
    verdict: verdictReview,
    criteria,
    omittedCriteria: boundedCriteria.omitted,
    unmatchedResults,
    technical: {
      entries: technicalEntries,
      evidenceReferences: boundedEvidence.items.map(item => ({
        evidenceId: item.id,
        type: item.type,
        sourceRef: item.sourceRef,
        openable: evidenceById.has(item.id),
      })),
      omittedEvidenceReferences: boundedEvidence.omitted,
    },
    publication: buildReceipt(
      publication,
      options.receiptDetail ?? null,
      options.receiptStatus,
      limits.receiptHistory,
    ),
  }
}

/**
 * Bounds a value and then removes credential-shaped substrings. Verification
 * digests and repository refs survive; provider tokens do not.
 */
export function strongFlowSecretSafeText(
  value: string,
  options: { readonly maxLength?: number } = {},
): string {
  const maxLength = options.maxLength ?? MAX_SUMMARY_VALUE_CHARS
  const bounded = value.length > maxLength
    ? `${value.slice(0, maxLength)} …[truncated]`
    : value
  let result = bounded
  for (const pattern of SECRET_PATTERNS) result = result.replace(pattern, REDACTED)
  return result
}

function secretLine(line: string): string {
  return strongFlowSecretSafeText(line, { maxLength: MAX_SUMMARY_VALUE_CHARS })
}

/**
 * The copyable technical summary, one secret-safe line per closed fact. The
 * sealed read-cursor token is deliberately absent: it authenticates a read cut
 * and is not a verification fact.
 */
export function strongFlowTechnicalSummaryLines(
  detail: StrongFlowReviewDetail,
): readonly string[] {
  const lines: string[] = ['StrongFlow technical summary']
  for (const entry of detail.technical.entries) {
    lines.push(secretLine(`${entry.label}: ${entry.value}`))
  }
  lines.push(secretLine(`Evidence (${String(detail.technical.evidenceReferences.length)}):`))
  for (const reference of detail.technical.evidenceReferences) {
    lines.push(secretLine(
      `  - ${reference.type} ${reference.evidenceId} ${reference.sourceRef}`,
    ))
  }
  lines.push(secretLine(`Verdict: ${verdictStatusLabel(detail.verdict.status)}`))
  for (const reason of detail.verdict.reasons) {
    lines.push(secretLine(`  - ${reason.criterionId} ${reason.outcome}: ${reason.explanation}`))
  }
  lines.push(secretLine(
    `Unresolved findings (${String(detail.verdict.unresolvedFindings.length)}):`,
  ))
  for (const finding of detail.verdict.unresolvedFindings) {
    lines.push(secretLine(`  - ${finding}`))
  }
  lines.push(secretLine(`Publication: ${detail.publication.state ?? 'not created'}`))
  if (detail.publication.publicationId !== null) {
    lines.push(secretLine(
      `  - receipt ${detail.publication.publicationId} r${String(
        detail.publication.revision ?? 0,
      )} set ${detail.publication.publicationSetSha256 ?? 'unknown'}`,
    ))
  }
  for (const reference of detail.publication.externalReferences) {
    lines.push(secretLine(
      `  - ${reference.kind} ${reference.repository} #${String(reference.number)}`,
    ))
  }
  return lines
}

/** The whole technical summary as one copyable, secret-safe block. */
export function strongFlowTechnicalSummary(detail: StrongFlowReviewDetail): string {
  const text = strongFlowTechnicalSummaryLines(detail).join('\n')
  return text.length > MAX_SUMMARY_CHARS
    ? `${text.slice(0, MAX_SUMMARY_CHARS)}\n…[truncated]`
    : text
}

export interface StrongFlowReceiptLoaderOptions {
  readonly client: Pick<ControlPlaneClient, 'query'>
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly nextRequestId: () => RequestId
}

export interface StrongFlowReceiptLoader {
  /** Read one verified Publication journal detail; null once this loader closed. */
  load(publicationId: PublicationId): Promise<PublicationDetailProjection | null>
  close(): void
}

function receiptFailure(code: string, message: string, cause?: unknown): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code,
    message,
    requestId: null,
    retryable: false,
    ...(cause === undefined ? {} : { cause }),
  })
}

/**
 * Reads `publication.get` only. That query is side-effect-free: the panel can
 * show retry and cancel traceability, but it never issues a publication write,
 * merge, rebase, push or another external side effect.
 */
export function createStrongFlowReceiptLoader(
  options: StrongFlowReceiptLoaderOptions,
): StrongFlowReceiptLoader {
  let closed = false
  return {
    async load(publicationId: PublicationId): Promise<PublicationDetailProjection | null> {
      if (closed) return null
      const response = await options.client.query({
        schemaVersion: SCHEMA_VERSION,
        requestId: options.nextRequestId(),
        actor: options.actor,
        scope: options.scope,
        query: QueryName.PublicationGet,
        parameters: { publicationId },
        page: RECEIPT_PAGE,
      })
      if (closed) return null
      if (response.query !== QueryName.PublicationGet) {
        throw receiptFailure(
          'STRONGFLOW_RECEIPT_QUERY_MISMATCH',
          'The Control Plane returned another Publication result.',
        )
      }
      if (response.page.hasMore || response.page.nextCursor !== null) {
        throw receiptFailure(
          'STRONGFLOW_RECEIPT_PAGE_INVALID',
          'The Publication receipt query returned an unexpected page cursor.',
        )
      }
      const result = (response as PublicationGetResultResponse).result
      if (result.kind !== 'publication_detail') {
        throw receiptFailure(
          'STRONGFLOW_RECEIPT_RESULT_INVALID',
          'The Control Plane returned an invalid Publication receipt result.',
        )
      }
      return result
    },
    close() {
      closed = true
    },
  }
}

export interface StrongFlowReviewDetailPanelOptions {
  readonly document: Document
  readonly root: HTMLElement
  readonly model: StrongFlowViewModel
  /** Receipt reads; omitted when no Control Plane read boundary is available. */
  readonly receipts?: StrongFlowReceiptLoader | null
  readonly limits?: Partial<StrongFlowReviewDetailLimits>
  /** Presentation-only capability; Server authorization stays authoritative. */
  readonly readOnly?: boolean
  /** Opens one exact Evidence record in the existing Evidence workbench. */
  readonly onOpenEvidence?: (evidenceId: EvidenceId) => void
  /** Copy seam; defaults to the browser clipboard when it is available. */
  readonly copy?: (text: string) => Promise<void> | void
}

export interface StrongFlowReviewDetailPanel {
  readonly root: HTMLElement
  update(): void
  close(): void
}

type SectionId = 'technical' | 'criteria' | 'publication'

const SECTION_LABELS: Readonly<Record<SectionId, string>> = Object.freeze({
  technical: 'Technical identity',
  criteria: 'Verdict criteria',
  publication: 'Publication receipt',
})

interface SectionState {
  readonly section: HTMLElement
  readonly toggle: HTMLButtonElement
  readonly body: HTMLElement
  readonly onToggle: () => void
}

interface CriterionRow {
  readonly item: HTMLElement
  readonly outcome: HTMLElement
  readonly title: HTMLElement
  readonly explanation: HTMLElement
  readonly evidenceHost: HTMLElement
  readonly evidenceButtons: KeyedCollectionView<EvidenceId, string, HTMLButtonElement>
}

function fillDefinitions(
  document: Document,
  node: HTMLElement,
  entries: readonly StrongFlowTechnicalEntry[],
): void {
  node.replaceChildren()
  for (const entry of entries) {
    const term = document.createElement('dt')
    const description = document.createElement('dd')
    term.className = 'wwc-strongflow-review-detail-term'
    description.className = 'wwc-strongflow-review-detail-description'
    term.textContent = entry.label
    description.textContent = entry.value
    node.append(term, description)
  }
}

function defaultCopier(document: Document): (text: string) => Promise<void> {
  const clipboard = document.defaultView?.navigator?.clipboard
  if (clipboard === undefined || typeof clipboard.writeText !== 'function') {
    return async () => {
      throw new Error('CLIPBOARD_UNAVAILABLE')
    }
  }
  return text => clipboard.writeText(text)
}

/**
 * The technical review panel: the delivery conclusion stays visible in the main
 * view while the technical identity, the per-criterion Verdict results and the
 * Publication receipt are one disclosure away. It renders read-only facts and
 * opens Evidence; it adds no second command and no second authority.
 */
export function mountStrongFlowReviewDetailPanel(
  options: StrongFlowReviewDetailPanelOptions,
): StrongFlowReviewDetailPanel {
  const { document, model } = options
  const readOnly = options.readOnly === true
  const copier = options.copy ?? defaultCopier(document)
  const limits = defaultLimits(options.limits)
  let closed = false
  let receiptDetail: PublicationDetailProjection | null = null
  let receiptStatus: StrongFlowReceiptStatus = 'not_loaded'
  let receiptRequest: PublicationId | null = null
  const expanded = new Set<SectionId>()

  const root = options.root
  root.className = 'wwc-strongflow-review-detail'
  root.setAttribute('aria-label', 'Delivery review detail')

  const heading = strongFlowElement(document, 'h4', 'wwc-strongflow-review-detail-heading')
  heading.textContent = 'Delivery review detail'
  const conclusion = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-review-detail-conclusion',
  )
  const hint = strongFlowElement(document, 'p', 'wwc-strongflow-review-detail-hint')
  hint.textContent = 'Open a section to verify the technical identity behind this delivery.'

  const technicalBody = strongFlowElement(document, 'div', 'wwc-strongflow-review-detail-body')
  const criteriaBody = strongFlowElement(document, 'div', 'wwc-strongflow-review-detail-body')
  const publicationBody = strongFlowElement(document, 'div', 'wwc-strongflow-review-detail-body')

  const sections = new Map<SectionId, SectionState>()
  const sectionBodies: readonly (readonly [SectionId, HTMLElement])[] = [
    ['technical', technicalBody],
    ['criteria', criteriaBody],
    ['publication', publicationBody],
  ]
  for (const [id, body] of sectionBodies) {
    const section = strongFlowElement(document, 'section', 'wwc-strongflow-review-detail-section')
    section.dataset.section = id
    const toggle = document.createElement('button') as HTMLButtonElement
    toggle.type = 'button'
    toggle.className = 'wwc-strongflow-review-detail-toggle'
    toggle.textContent = SECTION_LABELS[id]
    toggle.setAttribute('aria-expanded', 'false')
    toggle.setAttribute('aria-controls', `wwc-strongflow-review-detail-body-${id}`)
    body.id = `wwc-strongflow-review-detail-body-${id}`
    body.hidden = true
    const onToggle = () => {
      if (expanded.has(id)) expanded.delete(id)
      else expanded.add(id)
      if (id === 'publication' && expanded.has(id)) void loadReceipt()
      applyFold()
      update()
    }
    toggle.addEventListener('click', onToggle)
    section.append(toggle, body)
    sections.set(id, { section, toggle, body, onToggle })
  }

  // Technical identity
  const technical = strongFlowElement(document, 'dl', 'wwc-strongflow-review-detail-technical')
  const technicalEvidence = strongFlowElement(
    document,
    'ul',
    'wwc-strongflow-review-detail-technical-evidence',
  )
  const technicalOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const copy = document.createElement('button') as HTMLButtonElement
  copy.type = 'button'
  copy.className = 'wwc-strongflow-review-detail-copy'
  copy.textContent = 'Copy technical summary'
  const summary = document.createElement('textarea') as HTMLTextAreaElement
  summary.className = 'wwc-strongflow-review-detail-summary'
  summary.readOnly = true
  summary.rows = 6
  summary.setAttribute('aria-label', 'Technical summary')
  const copyStatus = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-review-detail-copy-status',
  )
  technicalBody.append(
    technical,
    technicalEvidence,
    technicalOmitted,
    copy,
    summary,
    copyStatus,
  )

  // Verdict criteria
  const criteria = strongFlowElement(document, 'ul', 'wwc-strongflow-review-detail-criteria')
  const criteriaOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const reasons = strongFlowElement(document, 'ul', 'wwc-strongflow-review-detail-reasons')
  const unmatched = strongFlowElement(
    document,
    'ul',
    'wwc-strongflow-review-detail-unmatched',
  )
  const findingsHeading = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-review-detail-findings-heading',
  )
  findingsHeading.textContent = 'Unresolved findings'
  const findings = strongFlowElement(document, 'ul', 'wwc-strongflow-review-detail-findings')
  criteriaBody.append(criteria, criteriaOmitted, reasons, unmatched, findingsHeading, findings)

  // Publication receipt
  const receipt = strongFlowElement(document, 'dl', 'wwc-strongflow-review-detail-receipt')
  const receiptState = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-review-detail-receipt-status',
  )
  const receiptHistoryHeading = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-review-detail-receipt-history-heading',
  )
  receiptHistoryHeading.textContent = 'Receipt history'
  const receiptHistory = strongFlowElement(
    document,
    'ol',
    'wwc-strongflow-review-detail-receipt-history',
  )
  const receiptStepsHeading = strongFlowElement(
    document,
    'p',
    'wwc-strongflow-review-detail-receipt-steps-heading',
  )
  receiptStepsHeading.textContent = 'Publication steps'
  const receiptSteps = strongFlowElement(
    document,
    'ul',
    'wwc-strongflow-review-detail-receipt-steps',
  )
  const receiptRefresh = document.createElement('button') as HTMLButtonElement
  receiptRefresh.type = 'button'
  receiptRefresh.className = 'wwc-strongflow-review-detail-receipt-refresh'
  receiptRefresh.textContent = 'Refresh receipt'
  publicationBody.append(
    receipt,
    receiptState,
    receiptHistoryHeading,
    receiptHistory,
    receiptStepsHeading,
    receiptSteps,
    receiptRefresh,
  )

  function mountEvidenceButtons(
    host: HTMLElement,
    className: string,
  ): KeyedCollectionView<EvidenceId, string, HTMLButtonElement> {
    return mountKeyedCollection<EvidenceId, string, HTMLButtonElement>({
      parent: host,
      key: evidenceId => evidenceId,
      create(evidenceId) {
        const button = document.createElement('button') as HTMLButtonElement
        button.type = 'button'
        button.className = className
        button.dataset.evidenceRefId = evidenceId
        button.addEventListener('click', () => options.onOpenEvidence?.(evidenceId))
        return button
      },
      update(button, evidenceId) {
        button.textContent = `Open evidence ${evidenceId}`
        button.disabled = readOnly
        button.dataset.evidenceRefId = evidenceId
      },
    })
  }

  interface EvidenceReferenceRow {
    readonly item: HTMLElement
    readonly text: HTMLElement
    readonly evidenceButtons: KeyedCollectionView<EvidenceId, string, HTMLButtonElement>
  }
  const evidenceReferenceRows = new WeakMap<HTMLElement, EvidenceReferenceRow>()
  const technicalEvidenceCollection = mountKeyedCollection<
    StrongFlowCriterionEvidenceReference,
    string,
    HTMLElement
  >({
    parent: technicalEvidence,
    key: reference => reference.evidenceId,
    create(reference) {
      const item = document.createElement('li')
      const text = strongFlowElement(
        document,
        'span',
        'wwc-strongflow-review-detail-evidence-ref',
      )
      const host = strongFlowElement(
        document,
        'div',
        'wwc-strongflow-review-detail-evidence-actions',
      )
      const buttons = mountEvidenceButtons(
        host,
        'wwc-strongflow-review-detail-evidence-open',
      )
      item.append(text, host)
      evidenceReferenceRows.set(item, { item, text, evidenceButtons: buttons })
      return item
    },
    update(item, reference) {
      const state = evidenceReferenceRows.get(item)
      if (state === undefined) return
      item.dataset.evidenceRefId = reference.evidenceId
      state.text.textContent = `${reference.type} ${reference.evidenceId} ${reference.sourceRef}`
      state.evidenceButtons.update([reference.evidenceId])
    },
    remove(item) {
      const state = evidenceReferenceRows.get(item)
      if (state !== undefined) {
        state.evidenceButtons.close()
        evidenceReferenceRows.delete(item)
      }
    },
  })

  const criterionRows = new WeakMap<HTMLElement, CriterionRow>()
  const criterionCollection = mountKeyedCollection<
    StrongFlowCriterionReviewRow,
    string,
    HTMLElement
  >({
    parent: criteria,
    key: row => row.criterionId,
    create() {
      const item = document.createElement('li')
      const outcome = strongFlowElement(
        document,
        'span',
        'wwc-strongflow-review-detail-criterion-outcome',
      )
      const title = strongFlowElement(
        document,
        'p',
        'wwc-strongflow-review-detail-criterion-title',
      )
      const explanation = strongFlowElement(
        document,
        'p',
        'wwc-strongflow-review-detail-criterion-explanation',
      )
      const evidenceHost = strongFlowElement(
        document,
        'div',
        'wwc-strongflow-review-detail-criterion-evidence',
      )
      const buttons = mountEvidenceButtons(
        evidenceHost,
        'wwc-strongflow-review-detail-criterion-evidence-open',
      )
      item.append(outcome, title, explanation, evidenceHost)
      criterionRows.set(item, { item, outcome, title, explanation, evidenceHost, evidenceButtons: buttons })
      return item
    },
    update(item, row) {
      const state = criterionRows.get(item)
      if (state === undefined) return
      item.dataset.criterionId = row.criterionId
      item.dataset.outcome = row.outcome
      state.outcome.textContent = strongFlowCriterionOutcomeLabel(row.outcome)
      state.title.textContent = `${row.criterionId} · ${row.required ? 'required' : 'optional'}`
        + ` · ${row.description}`
        + (row.verificationMethod === null ? '' : ` · verified by ${row.verificationMethod}`)
        + (row.evaluatedAt === null ? '' : ` · evaluated ${row.evaluatedAt}`)
      state.explanation.textContent = row.explanation ?? 'No Verdict result yet.'
      state.evidenceButtons.update(row.evidence.map(entry => entry.evidenceId))
      state.evidenceHost.hidden = row.evidence.length === 0
    },
    remove(item) {
      const state = criterionRows.get(item)
      if (state !== undefined) {
        state.evidenceButtons.close()
        criterionRows.delete(item)
      }
    },
  })

  function applyFold(): void {
    for (const state of sections.values()) {
      const open = expanded.has(state.section.dataset.section as SectionId)
      state.toggle.setAttribute('aria-expanded', String(open))
      state.body.hidden = !open
    }
  }

  async function loadReceipt(force = false): Promise<void> {
    if (closed || options.receipts === null || options.receipts === undefined) return
    const publicationId = model.state.projection?.publication?.id ?? null
    if (publicationId === null) return
    if (!force && receiptRequest === publicationId && receiptStatus !== 'error') return
    receiptRequest = publicationId
    receiptStatus = 'loading'
    update()
    try {
      const detail = await options.receipts.load(publicationId)
      if (closed || receiptRequest !== publicationId) return
      receiptDetail = detail
      receiptStatus = 'ready'
    } catch {
      if (closed || receiptRequest !== publicationId) return
      receiptDetail = null
      receiptStatus = 'error'
    }
    update()
  }

  function onRefreshReceipt(): void {
    receiptDetail = null
    void loadReceipt(true)
  }
  receiptRefresh.addEventListener('click', onRefreshReceipt)

  const onCopyClick = () => {
    void onCopy()
  }
  copy.addEventListener('click', onCopyClick)

  async function onCopy(): Promise<void> {
    const projection = model.state.projection
    if (projection === null) return
    const text = strongFlowTechnicalSummary(
      strongFlowReviewDetail(projection, { limits, receiptDetail, receiptStatus }),
    )
    summary.value = text
    try {
      await copier(text)
      copyStatus.textContent = 'Technical summary copied.'
    } catch {
      copyStatus.textContent = 'Copy is unavailable. Select the summary text to copy it.'
    }
    update()
  }

  function update(): void {
    if (closed) return
    const projection = model.state.projection
    root.hidden = projection === null
    if (projection === null) {
      conclusion.textContent = ''
      criterionCollection.update([])
      technicalEvidenceCollection.update([])
      fillDefinitions(document, technical, [])
      fillDefinitions(document, receipt, [])
      return
    }
    const detail = strongFlowReviewDetail(projection, {
      limits,
      receiptDetail,
      receiptStatus,
    })
    conclusion.textContent = detail.conclusion

    fillDefinitions(document, technical, detail.technical.entries)
    technicalEvidenceCollection.update(
      detail.technical.evidenceReferences.filter(reference => reference.openable),
    )
    technicalEvidence.hidden = detail.technical.evidenceReferences.length === 0
    technicalOmitted.hidden = detail.technical.omittedEvidenceReferences === 0
    technicalOmitted.textContent = detail.technical.omittedEvidenceReferences === 0
      ? ''
      : `${String(detail.technical.omittedEvidenceReferences)} more evidence records not shown.`
    copy.disabled = readOnly
    summary.hidden = summary.value.length === 0
    copyStatus.hidden = copyStatus.textContent.length === 0

    criterionCollection.update(detail.criteria)
    criteriaOmitted.hidden = detail.omittedCriteria === 0
    criteriaOmitted.textContent = detail.omittedCriteria === 0
      ? ''
      : `${String(detail.omittedCriteria)} more acceptance criteria not shown.`
    reasons.replaceChildren()
    for (const reason of detail.verdict.reasons) {
      const item = document.createElement('li')
      item.textContent = `${reason.criterionId} ${reason.outcome}: ${reason.explanation}`
      reasons.append(item)
    }
    reasons.hidden = detail.verdict.reasons.length === 0
    unmatched.replaceChildren()
    for (const result of detail.unmatchedResults) {
      const item = document.createElement('li')
      item.textContent = 'Verdict result without an acceptance criterion: '
        + `${result.criterionId} ${result.outcome}`
      unmatched.append(item)
    }
    unmatched.hidden = detail.unmatchedResults.length === 0
    findingsHeading.hidden = detail.verdict.unresolvedFindings.length === 0
    findings.replaceChildren()
    for (const finding of detail.verdict.unresolvedFindings) {
      const item = document.createElement('li')
      item.textContent = finding
      findings.append(item)
    }
    findings.hidden = detail.verdict.unresolvedFindings.length === 0

    const entries: StrongFlowTechnicalEntry[] = []
    if (detail.publication.present) {
      entries.push(
        { label: 'Receipt', value: detail.publication.publicationId ?? 'unknown' },
        {
          label: 'State',
          value: `${detail.publication.state ?? 'unknown'} r${String(
            detail.publication.revision ?? 0,
          )}`,
        },
        { label: 'Verdict receipt', value: detail.publication.deliveryVerdictId ?? 'unknown' },
        {
          label: 'Publication set digest',
          value: detail.publication.publicationSetSha256 ?? 'unknown',
        },
        { label: 'Approved by', value: detail.publication.approvedBy ?? 'unknown' },
        { label: 'Approved at', value: detail.publication.approvedAt ?? 'unknown' },
      )
      const target = detail.publication.target
      if (target !== null && target !== undefined) {
        entries.push({
          label: 'Target',
          value: `${target.provider} ${target.headRepository} ${target.headBranch}`
            + ` -> ${target.baseBranch}`,
        })
      }
      for (const reference of detail.publication.externalReferences) {
        entries.push({
          label: 'External reference',
          value: `${reference.repository} #${String(reference.number)} (${reference.kind})`,
        })
      }
      if (detail.publication.retryable !== null) {
        entries.push({
          label: 'Retryable',
          value: detail.publication.retryable ? 'yes' : 'no',
        })
      }
      if (detail.publication.cancellable !== null) {
        entries.push({
          label: 'Cancellable',
          value: detail.publication.cancellable ? 'yes' : 'no',
        })
      }
    }
    fillDefinitions(document, receipt, entries)
    receipt.hidden = !detail.publication.present
    receiptState.hidden = !detail.publication.present
    receiptState.textContent = !detail.publication.present
      ? ''
      : detail.publication.detailStatus === 'ready'
        ? 'Receipt read from the verified Publication journal.'
        : detail.publication.detailStatus === 'loading'
          ? 'Reading the Publication journal…'
          : detail.publication.detailStatus === 'error'
            ? 'The Publication receipt is not available right now. Retry and cancel stay unknown.'
            : 'Reading the Publication journal receipt on demand.'
    receiptHistoryHeading.hidden = detail.publication.history.length === 0
    receiptHistoryHeading.textContent = detail.publication.historyTruncated
      ? 'Receipt history (older revisions omitted)'
      : 'Receipt history'
    receiptHistory.replaceChildren()
    for (const entry of detail.publication.history) {
      const item = document.createElement('li')
      item.textContent = `r${String(entry.revision)} ${entry.state} · retryable ${
        entry.retryable ? 'yes' : 'no'
      } · cancellable ${entry.cancellable ? 'yes' : 'no'}`
      receiptHistory.append(item)
    }
    receiptStepsHeading.hidden = detail.publication.steps.length === 0
    receiptSteps.replaceChildren()
    for (const step of detail.publication.steps) {
      const item = document.createElement('li')
      item.textContent = `${step.kind} ${step.state}`
        + (step.outcomeCode === null ? '' : ` (${step.outcomeCode})`)
        + (step.resourceRef === null
          ? ''
          : ` · ${step.resourceRef.repository} #${String(step.resourceRef.number)}`)
      receiptSteps.append(item)
    }
    receiptRefresh.hidden = !detail.publication.present
    receiptRefresh.disabled = readOnly || receiptStatus === 'loading'

    if (
      expanded.has('publication')
      && detail.publication.present
      && detail.publication.detailStatus === 'not_loaded'
    ) void loadReceipt()
  }

  root.append(
    heading,
    conclusion,
    hint,
    sections.get('technical')!.section,
    sections.get('criteria')!.section,
    sections.get('publication')!.section,
  )

  const unsubscribe = model.subscribe(() => update())
  applyFold()
  update()

  return {
    root,
    update,
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      copy.removeEventListener('click', onCopyClick)
      receiptRefresh.removeEventListener('click', onRefreshReceipt)
      for (const state of sections.values()) {
        state.toggle.removeEventListener('click', state.onToggle)
      }
      criterionCollection.close()
      technicalEvidenceCollection.close()
      options.receipts?.close()
      root.replaceChildren()
    },
  }
}
