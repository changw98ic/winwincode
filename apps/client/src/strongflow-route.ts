// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryId,
  EvidenceId,
  ProductSessionId,
  StageRunId,
} from './generated/contracts.js'
import { matchesCanonicalSchema } from './generated/control-plane-client.js'
import { scopeHash, type ScopeRouteSelection } from './core/scope-context.js'
import type { CandidateDiffViewMode } from './strongflow-diff-model.js'
import {
  formatCandidateComparisonRequest,
  parseCandidateComparisonRequest,
  type CandidateComparisonRouteSelection,
} from './strongflow-diff-model.js'
import {
  strongFlowHistoryHashWithSelection,
  type StrongFlowHistorySelection,
} from './strongflow-history-selection.js'

export type StrongFlowEvidenceTabId = 'evidence' | 'preview' | 'tests' | 'logs'

/** Longest repository-relative path the canonical portable_path rules accept. */
const MAX_CANDIDATE_PATH_LENGTH = 4_096

const COMPARISON_FROM_PARAMETER = 'compareFrom'
const COMPARISON_TO_PARAMETER = 'compareTo'
/** `compareFrom` value that names the Delivery base revision. */
export const STRONGFLOW_COMPARISON_BASELINE_VALUE = 'baseline'

export interface StrongFlowEvidenceRouteState {
  readonly tab: StrongFlowEvidenceTabId
  readonly evidenceId: EvidenceId | null
}

export interface StrongFlowRoute {
  readonly deliveryId: DeliveryId | null
  readonly productSessionId: ProductSessionId | null
  readonly stageRunId: StageRunId | null
  readonly candidatePath: string | null
  readonly candidateView: CandidateDiffViewMode
  /** Candidate comparison requested by one shareable link. */
  readonly comparison: CandidateComparisonRouteSelection
  readonly evidenceTab: StrongFlowEvidenceTabId
  readonly evidenceId: EvidenceId | null
}

function canonicalParameter<Identity extends string>(
  parameters: URLSearchParams,
  name: string,
  schema: 'DeliveryId' | 'EvidenceId' | 'ProductSessionId' | 'StageRunId',
): Identity | null {
  const values = parameters.getAll(name)
  if (values.length !== 1) return null
  const value = values[0]
  return value !== undefined && matchesCanonicalSchema(schema, value)
    ? value as Identity
    : null
}

function singleParameter(parameters: URLSearchParams, name: string): string | null {
  const values = parameters.getAll(name)
  if (values.length !== 1) return null
  const value = values[0]
  return value === undefined || value.length === 0 ? null : value
}

function routeParameters(hash: string): URLSearchParams {
  const query = hash.indexOf('?')
  return new URLSearchParams(query < 0 ? '' : hash.slice(query + 1))
}

/**
 * Mirror the canonical portable_path rules at the single route boundary: a
 * bounded repository-relative path with no leading slash, no backslash, no
 * Windows drive prefix, no UTF-8 control byte (which subsumes NUL), and no
 * empty, `.`, or `..` segment. Only an ASCII letter followed by `:` opens a
 * drive prefix, so a digit before the colon stays portable.
 */
function isPortableCandidatePath(value: string): boolean {
  return value.length > 0
    && value.length <= MAX_CANDIDATE_PATH_LENGTH
    && !value.startsWith('/')
    && !value.includes('\\')
    && !/^[A-Za-z]:/u.test(value)
    && ![...value].some(character => {
      const code = character.codePointAt(0)
      return code !== undefined && (code <= 0x1f || code === 0x7f)
    })
    && value.split('/').every(segment => (
      segment.length > 0 && segment !== '.' && segment !== '..'
    ))
}

/**
 * A `file` parameter that is not a portable relative path fails closed: it
 * never reaches a query or a view-model and is never written back to the URL.
 */
function candidatePathParameter(parameters: URLSearchParams): string | null {
  const value = singleParameter(parameters, 'file')
  return value !== null && isPortableCandidatePath(value) ? value : null
}

/** Read the raw Candidate `file` parameter, before portable-path validation. */
export function strongFlowRawCandidateFileFromHash(hash: string): string | null {
  return singleParameter(routeParameters(hash), 'file')
}

/** Read the Candidate Diff layout from the canonical StrongFlow route. */
export function strongFlowCandidateViewFromHash(
  hash: string,
): CandidateDiffViewMode | null {
  const value = singleParameter(routeParameters(hash), 'view')
  return value === 'side-by-side' || value === 'unified' ? value : null
}

/**
 * Read one shareable Candidate comparison from the canonical StrongFlow route.
 * The baseline side names the Delivery base revision, so it carries no value;
 * anything else that is not one exact frozen Candidate identity fails closed.
 */
export function strongFlowComparisonFromHash(
  hash: string,
): CandidateComparisonRouteSelection {
  const parameters = routeParameters(hash)
  const from = singleParameter(parameters, COMPARISON_FROM_PARAMETER)
  const to = singleParameter(parameters, COMPARISON_TO_PARAMETER)
  if (from === STRONGFLOW_COMPARISON_BASELINE_VALUE) {
    return parseCandidateComparisonRequest(null, to)
  }
  return parseCandidateComparisonRequest(from, to)
}

/** Parse the complete StrongFlow browser route once at the application boundary. */
export function parseStrongFlowRouteHash(hash: string): StrongFlowRoute {
  const parameters = routeParameters(hash)
  const tab = singleParameter(parameters, 'tab')
  return Object.freeze({
    deliveryId: canonicalParameter<DeliveryId>(parameters, 'delivery', 'DeliveryId'),
    productSessionId: canonicalParameter<ProductSessionId>(
      parameters,
      'session',
      'ProductSessionId',
    ),
    stageRunId: canonicalParameter<StageRunId>(parameters, 'stageRun', 'StageRunId'),
    candidatePath: candidatePathParameter(parameters),
    candidateView: strongFlowCandidateViewFromHash(hash) ?? 'unified',
    comparison: strongFlowComparisonFromHash(hash),
    evidenceTab: tab === 'preview' || tab === 'tests' || tab === 'logs' ? tab : 'evidence',
    evidenceId: canonicalParameter<EvidenceId>(parameters, 'evidence', 'EvidenceId'),
  })
}

/** Format every StrongFlow route field through the same typed route boundary. */
export function strongFlowRouteHash(
  route: StrongFlowRoute,
  scope?: ScopeRouteSelection,
  historySelection?: StrongFlowHistorySelection,
): string {
  const parameters = new URLSearchParams()
  if (route.deliveryId !== null) parameters.set('delivery', route.deliveryId)
  if (route.productSessionId !== null) parameters.set('session', route.productSessionId)
  if (route.stageRunId !== null) parameters.set('stageRun', route.stageRunId)
  if (route.candidatePath !== null) parameters.set('file', route.candidatePath)
  parameters.set('view', route.candidateView)
  if (route.comparison.status === 'requested') {
    const comparison = formatCandidateComparisonRequest(route.comparison.request)
    if (comparison.before !== null) parameters.set(COMPARISON_FROM_PARAMETER, comparison.before)
    parameters.set(COMPARISON_TO_PARAMETER, comparison.after)
  }
  if (route.evidenceTab !== 'evidence' || route.evidenceId !== null) {
    parameters.set('tab', route.evidenceTab)
  }
  if (route.evidenceId !== null) parameters.set('evidence', route.evidenceId)
  const query = parameters.toString()
  const hash = query.length === 0 ? '#/strongflow' : `#/strongflow?${query}`
  const scopedHash = scope === undefined ? hash : scopeHash(hash, scope)
  return historySelection === undefined
    ? scopedHash
    : strongFlowHistoryHashWithSelection(scopedHash, historySelection)
}
