// SPDX-License-Identifier: Apache-2.0

import type {
  CandidateAvailability,
  CandidateFileProjection,
  CandidateHistoryItemProjection,
  DeliveryCriterionResultProjection,
  DeliveryVerdictProjection,
  EvidenceId,
  FrozenCandidateSummaryProjection,
  Revision,
} from './generated/contracts.js'
import { matchesCanonicalSchema } from './generated/control-plane-client.js'

/** The two layouts the Candidate Diff viewer renders for one exact file Diff. */
export type CandidateDiffViewMode = 'unified' | 'side-by-side'

export type CandidateDiffLineKind = 'context' | 'addition' | 'deletion'

export interface CandidateDiffLine {
  readonly kind: CandidateDiffLineKind
  readonly text: string
  readonly oldLine: number | null
  readonly newLine: number | null
}

export interface CandidateDiffHunk {
  readonly key: string
  readonly header: string
  readonly oldStart: number
  readonly newStart: number
  readonly lines: readonly CandidateDiffLine[]
}

export interface ParsedCandidateDiff {
  readonly fileHeaders: readonly string[]
  readonly hunks: readonly CandidateDiffHunk[]
  readonly lineCount: number
  /** True when the bounded Diff bytes end inside a line instead of at a line break. */
  readonly truncatedTail: boolean
}

interface HunkHeaderParts {
  readonly header: string
  readonly oldStart: number
  readonly newStart: number
}

function hunkHeader(line: string, index: number): HunkHeaderParts | null {
  const match = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/u.exec(line)
  if (match === null) return null
  return {
    header: line,
    oldStart: Number(match[1]),
    newStart: Number(match[2]),
  }
}

/** Parse one bounded per-file unified Git Diff into exact hunks, line numbers, and line kinds. */
export function parseCandidateDiff(content: string): ParsedCandidateDiff {
  const body = content.replace(/\r\n?/gu, '\n')
  const lines = body.split('\n')
  if (lines.at(-1) === '') lines.pop()
  const truncatedTail = !body.endsWith('\n')
  const fileHeaders: string[] = []
  const hunks: CandidateDiffHunk[] = []
  let current: { parts: HunkHeaderParts, lines: CandidateDiffLine[] } | null = null
  let oldLine = 0
  let newLine = 0
  let lineCount = 0
  for (const line of lines) {
    if (line.startsWith('\\ ')) continue
    const parts = hunkHeader(line, hunks.length + 1)
    if (parts !== null) {
      current = { parts, lines: [] }
      hunks.push(Object.freeze({
        key: `hunk:${String(hunks.length + 1)}`,
        header: parts.header,
        oldStart: parts.oldStart,
        newStart: parts.newStart,
        lines: current.lines,
      }))
      oldLine = parts.oldStart
      newLine = parts.newStart
      continue
    }
    if (current === null) {
      if (line.length > 0) fileHeaders.push(line)
      continue
    }
    const marker = line.charAt(0)
    const text = line.slice(1)
    if (marker === '+') {
      current.lines.push(Object.freeze({
        kind: 'addition', text, oldLine: null, newLine,
      }))
      newLine += 1
      lineCount += 1
      continue
    }
    if (marker === '-') {
      current.lines.push(Object.freeze({
        kind: 'deletion', text, oldLine, newLine: null,
      }))
      oldLine += 1
      lineCount += 1
      continue
    }
    current.lines.push(Object.freeze({
      kind: 'context', text, oldLine, newLine,
    }))
    oldLine += 1
    newLine += 1
    lineCount += 1
  }
  return Object.freeze({
    fileHeaders: Object.freeze(fileHeaders),
    hunks: Object.freeze(hunks.map(hunk => Object.freeze({
      ...hunk,
      lines: Object.freeze([...hunk.lines]),
    }))),
    lineCount,
    truncatedTail,
  })
}

export interface CandidateDiffFileHeaderRow {
  readonly kind: 'file-header'
  readonly key: string
  readonly text: string
}

export interface CandidateDiffHunkHeaderRow {
  readonly kind: 'hunk-header'
  readonly key: string
  readonly hunkKey: string
  readonly header: string
  readonly expanded: boolean
  readonly hiddenContext: number
}

export interface CandidateDiffLineRow {
  readonly kind: 'line'
  readonly key: string
  readonly hunkKey: string
  /** Side-by-side rows carry `modified` when one removed line is paired with one added line. */
  readonly type: CandidateDiffLineKind | 'modified'
  readonly oldLine: number | null
  readonly newLine: number | null
  readonly oldText: string
  readonly newText: string
}

export type CandidateDiffRow =
  | CandidateDiffFileHeaderRow
  | CandidateDiffHunkHeaderRow
  | CandidateDiffLineRow

export interface CandidateDiffRowsOptions {
  readonly mode: CandidateDiffViewMode
  /** Hunks whose unchanged context lines are collapsed away. */
  readonly collapsedContextHunks: ReadonlySet<string>
  readonly limit: number
}

export interface CandidateDiffRowsResult {
  readonly rows: readonly CandidateDiffRow[]
  readonly totalRows: number
  readonly omittedRows: number
}

function lineRow(hunkKey: string, key: string, line: CandidateDiffLine): CandidateDiffLineRow {
  return Object.freeze({
    kind: 'line',
    key,
    hunkKey,
    type: line.kind,
    oldLine: line.oldLine,
    newLine: line.newLine,
    oldText: line.text,
    newText: line.text,
  })
}

function sideBySideLineRow(
  hunkKey: string,
  key: string,
  old: CandidateDiffLine | null,
  new_: CandidateDiffLine | null,
): CandidateDiffLineRow {
  const type: CandidateDiffLineKind | 'modified' = old === null
    ? 'addition'
    : new_ === null
      ? 'deletion'
      : old.text === new_.text
        ? 'context'
        : 'modified'
  return Object.freeze({
    kind: 'line',
    key,
    hunkKey,
    type,
    oldLine: old?.oldLine ?? null,
    newLine: new_?.newLine ?? null,
    oldText: old?.text ?? '',
    newText: new_?.text ?? '',
  })
}

function unifiedHunkRows(hunk: CandidateDiffHunk): readonly CandidateDiffRow[] {
  return hunk.lines.map((line, index) => lineRow(hunk.key, `${hunk.key}:line:${String(index)}`, line))
}

function sideBySideHunkRows(hunk: CandidateDiffHunk): readonly CandidateDiffRow[] {
  const rows: CandidateDiffRow[] = []
  let index = 0
  let current = hunk.lines[index]
  while (current !== undefined) {
    if (current.kind === 'context') {
      rows.push(sideBySideLineRow(
        hunk.key,
        `${hunk.key}:pair:${String(rows.length)}`,
        current,
        current,
      ))
      index += 1
      current = hunk.lines[index]
      continue
    }
    const deletions: CandidateDiffLine[] = []
    const additions: CandidateDiffLine[] = []
    while (current !== undefined && current.kind === 'deletion') {
      deletions.push(current)
      index += 1
      current = hunk.lines[index]
    }
    while (current !== undefined && current.kind === 'addition') {
      additions.push(current)
      index += 1
      current = hunk.lines[index]
    }
    const pairCount = Math.max(deletions.length, additions.length)
    for (let pair = 0; pair < pairCount; pair += 1) {
      rows.push(sideBySideLineRow(
        hunk.key,
        `${hunk.key}:pair:${String(rows.length)}`,
        deletions[pair] ?? null,
        additions[pair] ?? null,
      ))
    }
  }
  return rows
}

/** Build the bounded, presentation-ready row window for one Candidate Diff layout. */
export function candidateDiffRows(
  parsed: ParsedCandidateDiff,
  options: CandidateDiffRowsOptions,
): CandidateDiffRowsResult {
  if (!Number.isInteger(options.limit) || options.limit < 1 || options.limit > 500) {
    throw new RangeError('Candidate Diff row limits must be integers between 1 and 500.')
  }
  const unbounded: CandidateDiffRow[] = parsed.fileHeaders.map((text, index) => Object.freeze({
    kind: 'file-header',
    key: `file-header:${String(index)}`,
    text,
  }))
  for (const hunk of parsed.hunks) {
    const collapsed = options.collapsedContextHunks.has(hunk.key)
    const hiddenContext = collapsed
      ? hunk.lines.filter(line => line.kind === 'context').length
      : 0
    unbounded.push(Object.freeze({
      kind: 'hunk-header',
      key: `hunk-header:${hunk.key}`,
      hunkKey: hunk.key,
      header: hunk.header,
      expanded: !collapsed,
      hiddenContext,
    }))
    const hunkRows = options.mode === 'side-by-side'
      ? sideBySideHunkRows(hunk)
      : unifiedHunkRows(hunk)
    for (const row of hunkRows) {
      if (collapsed && row.kind === 'line' && row.type === 'context') continue
      unbounded.push(row)
    }
  }
  const bounded = unbounded.slice(0, options.limit)
  return Object.freeze({
    rows: Object.freeze(bounded),
    totalRows: unbounded.length,
    omittedRows: Math.max(0, unbounded.length - bounded.length),
  })
}

/** Narrow viewports always fall back to the unified layout so lines stay readable. */
export function effectiveCandidateDiffView(
  preferred: CandidateDiffViewMode,
  narrowViewport: boolean,
): CandidateDiffViewMode {
  return narrowViewport ? 'unified' : preferred
}

/** Match the already rendered rows only, so searching never reads an unbounded Diff. */
export function candidateDiffMatchKeys(
  rows: readonly CandidateDiffRow[],
  query: string,
  limit: number,
): readonly string[] {
  if (!Number.isInteger(limit) || limit < 1 || limit > 500) {
    throw new RangeError('Candidate Diff match limits must be integers between 1 and 500.')
  }
  const needle = query.trim().toLocaleLowerCase()
  if (needle.length === 0) return Object.freeze([])
  const keys: string[] = []
  for (const row of rows) {
    if (row.kind !== 'line') continue
    const haystack = [row.oldText, row.newText]
    if (haystack.some(text => text.toLocaleLowerCase().includes(needle))) {
      keys.push(row.key)
      if (keys.length >= limit) break
    }
  }
  return Object.freeze(keys)
}

// ── Candidate comparison sources (UI-405) ───────────────────────────────────
//
// One comparison names two review cuts: the Delivery base revision
// (`before: null`) or a frozen Candidate, and the frozen Candidate to compare
// against. Every selection resolves against the current Delivery's frozen
// Candidate history, so a stale, foreign, or missing link is rejected instead
// of being compared silently. The single-file Diff model above stays the only
// reader of Diff bytes.

/** Exact frozen Candidate identity carried by one shareable comparison link. */
export interface CandidateComparisonIdentity {
  readonly candidateRef: string
  readonly diffSha256: string
}

/** One frozen Candidate the Delivery exposes as a comparison source. */
export interface CandidateComparisonChoice {
  readonly candidate: FrozenCandidateSummaryProjection
  readonly availability: CandidateAvailability
  /** True for the Candidate the current Delivery read cursor freezes. */
  readonly isCurrent: boolean
}

/** The Delivery facts every comparison is bound to. */
export interface CandidateComparisonContext {
  readonly deliverySpecId: string
  readonly deliverySpecRevision: Revision
  readonly choices: readonly CandidateComparisonChoice[]
}

/** One comparison request; a null `before` names the Delivery base revision. */
export interface CandidateComparisonRequest {
  readonly before: CandidateComparisonIdentity | null
  readonly after: CandidateComparisonIdentity
}

export type CandidateComparisonRejectionReason =
  | 'invalid-link'
  | 'same-candidate'
  | 'missing-candidate'
  | 'stale-candidate'
  | 'foreign-delivery'

export interface CandidateComparisonRejection {
  readonly reason: CandidateComparisonRejectionReason
  readonly message: string
  readonly candidateRef: string | null
}

export type CandidateComparisonResolution =
  | {
      readonly status: 'resolved'
      readonly before: CandidateComparisonChoice | null
      readonly after: CandidateComparisonChoice
    }
  | {
      readonly status: 'rejected'
      readonly rejection: CandidateComparisonRejection
    }

/** What the canonical StrongFlow route carries for one comparison. */
export type CandidateComparisonRouteSelection =
  | { readonly status: 'none' }
  | { readonly status: 'invalid' }
  | { readonly status: 'requested'; readonly request: CandidateComparisonRequest }

/**
 * A Git reference never contains `~`, so it joins one Candidate reference and
 * one Diff digest into a single opaque, secret-free link token.
 */
const COMPARISON_IDENTITY_SEPARATOR = '~'

export function formatCandidateComparisonIdentity(
  identity: CandidateComparisonIdentity,
): string {
  return `${identity.candidateRef}${COMPARISON_IDENTITY_SEPARATOR}${identity.diffSha256}`
}

export function parseCandidateComparisonIdentity(
  value: unknown,
): CandidateComparisonIdentity | null {
  if (typeof value !== 'string') return null
  const separator = value.lastIndexOf(COMPARISON_IDENTITY_SEPARATOR)
  if (separator <= 0) return null
  const candidateRef = value.slice(0, separator)
  const diffSha256 = value.slice(separator + 1)
  if (candidateRef.length === 0 || diffSha256.length === 0) return null
  if (candidateRef.includes(COMPARISON_IDENTITY_SEPARATOR)) return null
  // Only a canonical digest binds a link token to one frozen Candidate.
  if (!matchesCanonicalSchema('Sha256Digest', diffSha256)) return null
  return Object.freeze({ candidateRef, diffSha256 })
}

export function formatCandidateComparisonRequest(
  request: CandidateComparisonRequest,
): { readonly before: string | null; readonly after: string } {
  return Object.freeze({
    before: request.before === null
      ? null
      : formatCandidateComparisonIdentity(request.before),
    after: formatCandidateComparisonIdentity(request.after),
  })
}

export function parseCandidateComparisonRequest(
  beforeValue: unknown,
  afterValue: unknown,
): CandidateComparisonRouteSelection {
  if (beforeValue === null && afterValue === null) return Object.freeze({ status: 'none' })
  const before = beforeValue === null ? null : parseCandidateComparisonIdentity(beforeValue)
  const after = parseCandidateComparisonIdentity(afterValue)
  if ((beforeValue !== null && before === null) || after === null) {
    return Object.freeze({ status: 'invalid' })
  }
  return Object.freeze({
    status: 'requested',
    request: Object.freeze({ before, after }),
  })
}

/** Project one frozen Candidate history page into comparison sources. */
export function candidateComparisonChoices(
  items: readonly CandidateHistoryItemProjection[],
): readonly CandidateComparisonChoice[] {
  return Object.freeze(items.map(item => Object.freeze({
    candidate: item.candidate,
    availability: item.availability,
    isCurrent: item.isCurrentAtReadCursor,
  })))
}

function candidateIdentity(
  candidate: FrozenCandidateSummaryProjection,
): CandidateComparisonIdentity {
  return Object.freeze({
    candidateRef: candidate.candidateRef,
    diffSha256: candidate.diffSha256,
  })
}

type CandidateLookup =
  | { readonly status: 'found'; readonly choice: CandidateComparisonChoice }
  | { readonly status: 'rejected'; readonly rejection: CandidateComparisonRejection }

function comparisonChoice(
  context: CandidateComparisonContext,
  identity: CandidateComparisonIdentity,
): CandidateLookup {
  const choice = context.choices.find(
    item => item.candidate.candidateRef === identity.candidateRef,
  )
  if (choice === undefined) {
    return {
      status: 'rejected',
      rejection: {
        reason: 'missing-candidate',
        message: 'This comparison link names a Candidate the current Delivery does not expose.',
        candidateRef: identity.candidateRef,
      },
    }
  }
  if (choice.candidate.diffSha256 !== identity.diffSha256) {
    return {
      status: 'rejected',
      rejection: {
        reason: 'stale-candidate',
        message: 'This comparison link is stale: the named Candidate is no longer the frozen one.',
        candidateRef: identity.candidateRef,
      },
    }
  }
  if (
    choice.candidate.deliverySpecId !== context.deliverySpecId
    || choice.candidate.deliverySpecRevision !== context.deliverySpecRevision
  ) {
    return {
      status: 'rejected',
      rejection: {
        reason: 'foreign-delivery',
        message: 'This comparison link names a Candidate frozen under another Delivery Spec.',
        candidateRef: identity.candidateRef,
      },
    }
  }
  return { status: 'found', choice }
}

/** Resolve one comparison request against the current Delivery, failing closed. */
export function resolveCandidateComparison(
  context: CandidateComparisonContext,
  request: CandidateComparisonRequest,
): CandidateComparisonResolution {
  if (request.before !== null && request.before.candidateRef === request.after.candidateRef) {
    return Object.freeze({
      status: 'rejected',
      rejection: Object.freeze({
        reason: 'same-candidate',
        message: 'Select two different Candidates to compare.',
        candidateRef: request.after.candidateRef,
      }),
    })
  }
  const before = request.before === null
    ? ({ status: 'found' as const, choice: null })
    : comparisonChoice(context, request.before)
  if (before.status === 'rejected') {
    return Object.freeze({
      status: 'rejected',
      rejection: Object.freeze(before.rejection),
    })
  }
  const after = comparisonChoice(context, request.after)
  if (after.status === 'rejected') {
    return Object.freeze({
      status: 'rejected',
      rejection: Object.freeze(after.rejection),
    })
  }
  return Object.freeze({
    status: 'resolved',
    before: before.choice === null ? null : Object.freeze(before.choice),
    after: Object.freeze(after.choice),
  })
}

/** Inputs that decide the comparison a panel shows before a link is shared. */
export interface CandidateComparisonDefaults {
  /** Digest of the Candidate frozen before an approved bounded rework. */
  readonly reworkBaselineDigest: string | null
  /**
   * True when this Delivery's stage list carries a reworking stage, which
   * stays true after the rework finishes, so returning to the Delivery keeps
   * the rework pair as the default comparison.
   */
  readonly reworkStage: boolean
}

/**
 * Default to the rework before/after pair when this Delivery returned from
 * bounded rework, and to the Delivery baseline otherwise. A Delivery without a
 * frozen Candidate has no comparison at all.
 */
export function candidateComparisonDefaultRequest(
  context: CandidateComparisonContext,
  defaults: CandidateComparisonDefaults,
): CandidateComparisonRequest | null {
  const newestFirst = [...context.choices].reverse()
  const current = newestFirst.find(choice => choice.isCurrent)
  if (current === undefined) return null
  const after = candidateIdentity(current.candidate)
  if (defaults.reworkBaselineDigest !== null) {
    const reworked = newestFirst.find(choice => !choice.isCurrent
      && choice.candidate.diffSha256 === defaults.reworkBaselineDigest)
    if (reworked !== undefined) {
      return Object.freeze({ before: candidateIdentity(reworked.candidate), after })
    }
  }
  if (defaults.reworkStage) {
    const previous = newestFirst.find(choice => !choice.isCurrent)
    if (previous !== undefined) {
      return Object.freeze({ before: candidateIdentity(previous.candidate), after })
    }
  }
  return Object.freeze({ before: null, after })
}

export type CandidateComparisonRole = 'baseline' | 'candidate'

/** One compared review cut; the baseline cut owns no Candidate identity. */
export interface CandidateComparisonSide {
  readonly role: CandidateComparisonRole
  readonly candidate: FrozenCandidateSummaryProjection | null
  readonly availability: CandidateAvailability | null
  /** Changed-file inventory; null when the Delivery cannot display that side. */
  readonly files: readonly CandidateFileProjection[] | null
  readonly evidenceIds: readonly EvidenceId[]
  readonly verdict: DeliveryVerdictProjection | null
}

/** The Delivery base revision: an empty change set with no review facts yet. */
export function candidateComparisonBaselineSide(): CandidateComparisonSide {
  return Object.freeze({
    role: 'baseline',
    candidate: null,
    availability: null,
    files: Object.freeze([]),
    evidenceIds: Object.freeze([]),
    verdict: null,
  })
}

export interface CandidateComparisonFileChange {
  readonly path: string
  readonly kind: 'added' | 'removed' | 'changed'
  readonly before: CandidateFileProjection | null
  readonly after: CandidateFileProjection | null
}

export interface CandidateComparisonFilesSummary {
  /** False when one side has no readable inventory, so no path is comparable. */
  readonly known: boolean
  readonly changes: readonly CandidateComparisonFileChange[]
  readonly added: readonly string[]
  readonly removed: readonly string[]
  readonly changed: readonly string[]
  readonly beforeAdditions: number | null
  readonly beforeDeletions: number | null
  readonly additions: number | null
  readonly deletions: number | null
}

export interface CandidateComparisonEvidenceSummary {
  readonly added: readonly EvidenceId[]
  readonly removed: readonly EvidenceId[]
  readonly unchangedCount: number
}

export interface CandidateComparisonCriterionChange {
  readonly criterionId: string
  readonly before: DeliveryCriterionResultProjection['verdict'] | null
  readonly after: DeliveryCriterionResultProjection['verdict'] | null
}

export interface CandidateComparisonVerdictSummary {
  readonly beforeStatus: DeliveryVerdictProjection['status'] | null
  readonly afterStatus: DeliveryVerdictProjection['status'] | null
  readonly changed: boolean
  readonly criteria: readonly CandidateComparisonCriterionChange[]
}

export interface CandidateComparisonResult {
  readonly diffChanged: boolean
  readonly files: CandidateComparisonFilesSummary
  readonly evidence: CandidateComparisonEvidenceSummary
  readonly verdict: CandidateComparisonVerdictSummary
  /** True when one comparable section differs between the two sides. */
  readonly changed: boolean
}

function comparePath(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0
}

function fileTotals(
  files: readonly CandidateFileProjection[] | null,
): { readonly additions: number | null; readonly deletions: number | null } {
  if (files === null) return { additions: null, deletions: null }
  let additions = 0
  let deletions = 0
  for (const file of files) {
    additions += file.additions ?? 0
    deletions += file.deletions ?? 0
  }
  return { additions, deletions }
}

/** An unreadable inventory never invents line counts. */
const NO_FILE_TOTALS = Object.freeze({ additions: null, deletions: null })

function sameChangedFile(
  left: CandidateFileProjection,
  right: CandidateFileProjection,
): boolean {
  return left.status === right.status
    && left.additions === right.additions
    && left.deletions === right.deletions
    && left.binary === right.binary
    && left.encoding === right.encoding
    && left.oldPath === right.oldPath
}

/**
 * Compare two review cuts into one stable summary: changed paths, Diff digest,
 * Evidence set, and Verdict. Ordering is canonical, so repeated comparisons of
 * the same two Candidates read identically.
 */
export function compareCandidateReviews(
  before: CandidateComparisonSide,
  after: CandidateComparisonSide,
): CandidateComparisonResult {
  const known = before.files !== null && after.files !== null
  const baseline = before.role === 'baseline' && before.candidate === null
  const beforeFiles = new Map((before.files ?? []).map(file => [file.path, file]))
  const afterFiles = new Map((after.files ?? []).map(file => [file.path, file]))
  const added: string[] = []
  const removed: string[] = []
  const changed: string[] = []
  if (known) {
    if (baseline) {
      // Against the Delivery base revision the Candidate's own changed-file
      // statuses already say which paths it adds, removes, or rewrites.
      for (const [path, file] of afterFiles) {
        if (file.status === 'deleted') removed.push(path)
        else if (file.status === 'modified' || file.status === 'type_changed') changed.push(path)
        else added.push(path)
      }
    } else {
      for (const path of afterFiles.keys()) {
        if (!beforeFiles.has(path)) added.push(path)
      }
      for (const path of beforeFiles.keys()) {
        if (!afterFiles.has(path)) removed.push(path)
      }
      for (const [path, afterFile] of afterFiles) {
        const beforeFile = beforeFiles.get(path)
        if (beforeFile === undefined || sameChangedFile(beforeFile, afterFile)) continue
        changed.push(path)
      }
    }
  }
  const changes: CandidateComparisonFileChange[] = []
  for (const path of [...added].sort(comparePath)) {
    changes.push(Object.freeze({
      path,
      kind: 'added',
      before: null,
      after: afterFiles.get(path) ?? null,
    }))
  }
  for (const path of [...removed].sort(comparePath)) {
    changes.push(Object.freeze({
      path,
      kind: 'removed',
      before: beforeFiles.get(path) ?? null,
      after: null,
    }))
  }
  for (const path of [...changed].sort(comparePath)) {
    changes.push(Object.freeze({
      path,
      kind: 'changed',
      before: beforeFiles.get(path) ?? null,
      after: afterFiles.get(path) ?? null,
    }))
  }
  const beforeEvidence = [...before.evidenceIds].sort(comparePath)
  const afterEvidence = [...after.evidenceIds].sort(comparePath)
  const evidenceAdded = afterEvidence.filter(id => !before.evidenceIds.includes(id))
  const evidenceRemoved = beforeEvidence.filter(id => !after.evidenceIds.includes(id))
  const beforeTotals = known ? fileTotals(before.files) : NO_FILE_TOTALS
  const afterTotals = known ? fileTotals(after.files) : NO_FILE_TOTALS
  const criteria: CandidateComparisonCriterionChange[] = []
  const criterionIds = new Set([
    ...(before.verdict?.criteria ?? []).map(item => item.criterionId),
    ...(after.verdict?.criteria ?? []).map(item => item.criterionId),
  ])
  for (const criterionId of [...criterionIds].sort(comparePath)) {
    const beforeCriterion = before.verdict?.criteria.find(
      item => item.criterionId === criterionId,
    )
    const afterCriterion = after.verdict?.criteria.find(
      item => item.criterionId === criterionId,
    )
    const beforeVerdict = beforeCriterion?.verdict ?? null
    const afterVerdict = afterCriterion?.verdict ?? null
    // The summary reports Verdict *changes*, so an unchanged criterion stays
    // out of the list entirely.
    if (beforeVerdict === afterVerdict) continue
    criteria.push(Object.freeze({
      criterionId,
      before: beforeVerdict,
      after: afterVerdict,
    }))
  }
  const beforeStatus = before.verdict?.status ?? null
  const afterStatus = after.verdict?.status ?? null
  const verdictChanged = beforeStatus !== afterStatus
    || criteria.some(criterion => criterion.before !== criterion.after)
  const diffChanged = before.candidate === null
    || before.candidate.diffSha256 !== after.candidate?.diffSha256
  return Object.freeze({
    diffChanged,
    files: Object.freeze({
      known,
      changes: Object.freeze(changes),
      added: Object.freeze(added.sort(comparePath)),
      removed: Object.freeze(removed.sort(comparePath)),
      changed: Object.freeze(changed.sort(comparePath)),
      beforeAdditions: beforeTotals.additions,
      beforeDeletions: beforeTotals.deletions,
      additions: afterTotals.additions,
      deletions: afterTotals.deletions,
    }),
    evidence: Object.freeze({
      added: Object.freeze(evidenceAdded),
      removed: Object.freeze(evidenceRemoved),
      unchangedCount: afterEvidence.filter(id => before.evidenceIds.includes(id)).length,
    }),
    verdict: Object.freeze({
      beforeStatus,
      afterStatus,
      changed: verdictChanged,
      criteria: Object.freeze(criteria),
    }),
    changed: diffChanged
      || (known && changes.length > 0)
      || evidenceAdded.length > 0
      || evidenceRemoved.length > 0
      || verdictChanged,
  })
}
