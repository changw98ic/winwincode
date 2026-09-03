// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './control-plane-client.js'
import type {
  Actor,
  DeliveryCriterionResultProjection,
  DeliveryEvidenceProjection,
  EvidenceArtifactAccessProjection,
  EvidenceArtifactContentChunkProjection,
  EvidenceArtifactContentGetParameters,
  EvidenceArtifactContentGetResultResponse,
  EvidenceArtifactDescriptorProjection,
  EvidenceDetailProjection,
  EvidenceId,
  EvidenceGetResultResponse,
  EvidenceReadBinding,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
} from './generated/contracts.js'
import {
  EvidenceArtifactContentEncoding,
  EvidenceArtifactPreviewMode,
  EvidenceOutcome,
  QueryName,
} from './generated/contracts.js'
import { mountDrawer } from './components/drawer.js'
import { mountKeyedCollection } from './components/keyed-collection.js'
import { mountTabs, type TabsProps } from './components/tabs.js'
import type {
  StrongFlowEvidenceRouteState,
  StrongFlowEvidenceTabId,
} from './strongflow-route.js'
export type { StrongFlowEvidenceTabId } from './strongflow-route.js'
import {
  boundedItems,
  DEFAULT_STRONGFLOW_RENDER_LIMITS,
  strongFlowElement,
} from './strongflow-rendering.js'
import type {
  StrongFlowProjection,
  StrongFlowViewModel,
  StrongFlowViewModelState,
} from './strongflow-view-model.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const MAX_ARTIFACT_CHUNK_BYTES = 256 * 1024
const EVIDENCE_READ_PAGE_LIMIT = 1
const QUERY_PAGE = Object.freeze({ cursor: null, limit: 1 })

export type StrongFlowEvidenceCandidateState = 'current' | 'superseded' | 'no-candidate'

export type StrongFlowEvidenceOutcomeTone = 'pass' | 'business-fail' | 'infra' | 'neutral'

export interface StrongFlowEvidenceOutcomePresentation {
  readonly label: string
  readonly tone: StrongFlowEvidenceOutcomeTone
}

export interface StrongFlowEvidenceDownload {
  readonly fileName: string | null
  readonly mediaType: string
  readonly bytes: Uint8Array<ArrayBuffer>
}

/** Strict structural error view used for sanitized messages; never carries server details. */
export interface StrongFlowEvidenceErrorView {
  readonly kind: string
  readonly code: string
}

export interface StrongFlowEvidenceSelection {
  readonly row: DeliveryEvidenceProjection
  readonly binding: EvidenceReadBinding
  readonly candidateState: StrongFlowEvidenceCandidateState
}

export interface StrongFlowEvidenceDetailState {
  readonly status: 'loading' | 'ready' | 'error'
  readonly outcome: EvidenceOutcome | null
  readonly artifactAccess: EvidenceArtifactAccessProjection | null
  readonly error: ControlPlaneClientError | null
}

export interface StrongFlowEvidenceContentState {
  readonly status: 'idle' | 'loading' | 'ready' | 'download-only' | 'unavailable' | 'error'
  readonly artifact: EvidenceArtifactDescriptorProjection | null
  readonly text: string | null
  readonly loadedBytes: number
  readonly totalBytes: number
  readonly complete: boolean
  readonly truncated: boolean
  readonly error: ControlPlaneClientError | null
}

export interface StrongFlowEvidenceViewModelState {
  readonly tab: StrongFlowEvidenceTabId
  readonly projection: StrongFlowProjection | null
  readonly selected: StrongFlowEvidenceSelection | null
  readonly detail: StrongFlowEvidenceDetailState | null
  readonly content: StrongFlowEvidenceContentState | null
  readonly search: string
}

export type StrongFlowEvidenceViewModelListener = (
  state: StrongFlowEvidenceViewModelState,
) => void

export interface StrongFlowEvidenceViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly nextRequestId: () => RequestId
  readonly model: StrongFlowViewModel
  readonly route: StrongFlowEvidenceRouteState
  readonly onRouteChange: (route: StrongFlowEvidenceRouteState) => void
  readonly downloader?: (download: StrongFlowEvidenceDownload) => void
}

export interface StrongFlowEvidenceViewModel {
  readonly state: StrongFlowEvidenceViewModelState
  subscribe(listener: StrongFlowEvidenceViewModelListener): () => void
  selectTab(tab: StrongFlowEvidenceTabId): void
  openEvidence(evidenceId: EvidenceId): Promise<void>
  closeEvidence(): void
  retryDetail(): Promise<void>
  loadNextChunk(): Promise<void>
  selectArtifact(artifactId: string): Promise<void>
  downloadArtifact(): Promise<void>
  setSearch(value: string): void
  close(): void
}

export interface StrongFlowEvidenceOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly nextRequestId: () => RequestId
  readonly limits?: { readonly evidence: number }
  readonly route: StrongFlowEvidenceRouteState
  readonly onRouteChange: (route: StrongFlowEvidenceRouteState) => void
  readonly downloader?: (download: StrongFlowEvidenceDownload) => void
}

export interface StrongFlowEvidenceWorkbenchOptions extends StrongFlowEvidenceOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowViewModel
}

export interface StrongFlowEvidenceWorkbench {
  openEvidence(evidenceId: EvidenceId): Promise<void>
  close(): void
}

export interface StrongFlowEvidenceSummaryCounts {
  readonly total: number
  readonly pass: number
  readonly fail: number
  readonly inconclusive: number
  readonly infraError: number
}

export interface StrongFlowEvidenceSummary {
  readonly counts: StrongFlowEvidenceSummaryCounts
  readonly failures: readonly DeliveryCriterionResultProjection[]
  readonly omittedFailures: number
  readonly criterionIdsByEvidence: ReadonlyMap<string, readonly string[]>
}

const EVIDENCE_TABS: ReadonlyArray<{
  readonly id: StrongFlowEvidenceTabId
  readonly label: string
}> = Object.freeze([
  Object.freeze({ id: 'evidence', label: 'Evidence' }),
  Object.freeze({ id: 'tests', label: 'Tests' }),
  Object.freeze({ id: 'logs', label: 'Logs' }),
])

/**
 * Canonical outcome presentation only. The current producer emits no skipped or
 * structured test-case facts, so none are invented here.
 */
export const strongFlowEvidenceOutcomePresentation: ReadonlyMap<
  EvidenceOutcome,
  StrongFlowEvidenceOutcomePresentation
> = new Map<EvidenceOutcome, StrongFlowEvidenceOutcomePresentation>([
  [EvidenceOutcome.Succeeded, { label: 'Passed', tone: 'pass' }],
  [EvidenceOutcome.Failed, { label: 'Failed · business', tone: 'business-fail' }],
  [EvidenceOutcome.InfrastructureFailed, { label: 'Infrastructure error', tone: 'infra' }],
  [EvidenceOutcome.TimedOut, { label: 'Timed out', tone: 'infra' }],
  [EvidenceOutcome.PolicyDenied, { label: 'Policy denied', tone: 'neutral' }],
  [EvidenceOutcome.Cancelled, { label: 'Cancelled', tone: 'neutral' }],
  [EvidenceOutcome.Observed, { label: 'Observed · no structured verdict', tone: 'neutral' }],
])

export function strongFlowEvidenceErrorText(error: StrongFlowEvidenceErrorView): string {
  if (
    error.code === 'READ_CURSOR_EXPIRED'
    || error.code === 'REVISION_CONFLICT'
    || error.code === 'CANDIDATE_STALE'
  ) {
    return 'This Evidence moved with its Delivery snapshot. Refresh StrongFlow and reopen it.'
  }
  if (error.code === 'RESOURCE_NOT_FOUND') {
    return 'This Evidence is not part of the current Delivery snapshot.'
  }
  if (error.code === 'TRUSTED_FACTS_UNAVAILABLE') {
    return 'The authoritative Evidence facts are temporarily unavailable. Retry shortly.'
  }
  if (error.code === 'PERMISSION_DENIED') {
    return 'You do not have access to this Evidence.'
  }
  if (error.code === 'AUTHENTICATION_REQUIRED') {
    return 'Sign in again to open this Evidence.'
  }
  if (error.kind === 'network') {
    return 'The StrongFlow server could not be reached. Check the connection and retry.'
  }
  if (error.kind === 'cancelled') {
    return 'The Evidence read was cancelled.'
  }
  return 'The Evidence detail could not be opened. Retry.'
}

/** One tab filter: the Evidence tab keeps every type; Tests and Logs stay exact. */
export function strongFlowEvidenceRowsForTab(
  rows: readonly DeliveryEvidenceProjection[],
  tab: StrongFlowEvidenceTabId,
): readonly DeliveryEvidenceProjection[] {
  if (tab === 'tests') return rows.filter(row => row.type === 'test')
  if (tab === 'logs') {
    return rows.filter(row => row.type === 'command' || row.type === 'runtime_event')
  }
  return rows
}

/** Derive bounded criterion joins from the accepted Verdict and current Evidence rows only. */
export function strongFlowEvidenceSummary(
  projection: StrongFlowProjection,
  failureLimit = 5,
): StrongFlowEvidenceSummary {
  const criteria = projection.verdict?.criteria ?? []
  const evidenceIds = new Set(projection.evidence.map(row => row.id))
  const mutableJoins = new Map<string, string[]>()
  const counts = {
    total: criteria.length,
    pass: 0,
    fail: 0,
    inconclusive: 0,
    infraError: 0,
  }
  for (const criterion of criteria) {
    if (criterion.verdict === 'pass') counts.pass += 1
    else if (criterion.verdict === 'fail') counts.fail += 1
    else if (criterion.verdict === 'inconclusive') counts.inconclusive += 1
    else counts.infraError += 1
    for (const evidenceId of criterion.evidenceRefs) {
      if (!evidenceIds.has(evidenceId)) continue
      const joined = mutableJoins.get(evidenceId) ?? []
      if (!joined.includes(criterion.criterionId)) joined.push(criterion.criterionId)
      mutableJoins.set(evidenceId, joined)
    }
  }
  const failures = criteria.filter(criterion => criterion.verdict !== 'pass')
  const criterionIdsByEvidence = new Map<string, readonly string[]>(
    [...mutableJoins].map(([evidenceId, criterionIds]) => [
      evidenceId,
      Object.freeze([...criterionIds]),
    ]),
  )
  const boundedLimit = Math.max(0, failureLimit)
  return Object.freeze({
    counts: Object.freeze(counts),
    failures: Object.freeze(failures.slice(0, boundedLimit)),
    omittedFailures: Math.max(0, failures.length - boundedLimit),
    criterionIdsByEvidence,
  })
}

function clientFailure(code: string, message: string, cause?: unknown): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code,
    message,
    requestId: null,
    retryable: false,
    ...(cause === undefined ? {} : { cause }),
  })
}

function normalizedError(error: unknown): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) return error
  return clientFailure(
    'STRONGFLOW_EVIDENCE_VIEW_MODEL_FAILURE',
    'The Evidence detail could not be updated.',
    error,
  )
}

function jsonEqual(left: unknown, right: unknown): boolean {
  if (Object.is(left, right)) return true
  if (typeof left !== 'object' || left === null || typeof right !== 'object' || right === null) {
    return false
  }
  if (Array.isArray(left) || Array.isArray(right)) {
    if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) return false
    return left.every((value, index) => jsonEqual(value, right[index]))
  }
  const leftRecord = left as Readonly<Record<string, unknown>>
  const rightRecord = right as Readonly<Record<string, unknown>>
  const leftKeys = Object.keys(leftRecord).sort()
  const rightKeys = Object.keys(rightRecord).sort()
  return leftKeys.length === rightKeys.length
    && leftKeys.every((key, index) => (
      key === rightKeys[index] && jsonEqual(leftRecord[key], rightRecord[key])
    ))
}

function expectArtifactDescriptor(
  descriptor: EvidenceArtifactDescriptorProjection,
  selected: StrongFlowEvidenceSelection,
): void {
  const provenance = descriptor.provenance
  if (
    provenance.candidateRef !== selected.binding.candidateRef
    || provenance.deliveryId !== selected.binding.deliveryId
    || provenance.deliveryRevision !== selected.binding.atCursor.deliveryRevision
    || provenance.evidenceId !== selected.binding.evidenceId
    || provenance.sessionBindingId !== selected.binding.sessionBindingId
    || provenance.stageRunId !== selected.binding.stageRunId
  ) throw clientFailure(
    'STRONGFLOW_EVIDENCE_IDENTITY_MISMATCH',
    'The Evidence Artifact authority does not match the selected snapshot.',
  )
}

function expectDetailResult(
  response: QueryResultResponse,
  selected: StrongFlowEvidenceSelection,
): EvidenceDetailProjection {
  if (response.query !== QueryName.EvidenceGet) throw clientFailure(
    'STRONGFLOW_EVIDENCE_QUERY_MISMATCH',
    'The Control Plane returned another Evidence detail result.',
  )
  if (response.page.hasMore || response.page.nextCursor !== null) throw clientFailure(
    'STRONGFLOW_EVIDENCE_PAGE_INVALID',
    'The Evidence detail query returned an unexpected page cursor.',
  )
  const result = (response as EvidenceGetResultResponse).result
  if (result.kind !== 'evidence_detail') throw clientFailure(
    'STRONGFLOW_EVIDENCE_RESULT_INVALID',
    'The Control Plane returned an invalid Evidence detail result.',
  )
  if (!jsonEqual(result.evidence, selected.row) || !jsonEqual(result.readCursor, selected.binding.atCursor)) {
    throw clientFailure(
      'STRONGFLOW_EVIDENCE_IDENTITY_MISMATCH',
      'The Evidence detail does not match the selected snapshot.',
    )
  }
  if (result.artifactAccess.state === 'available') {
    if (result.artifactAccess.items.length < 1 || result.artifactAccess.items.length > 32) {
      throw clientFailure(
        'STRONGFLOW_EVIDENCE_ARTIFACTS_INVALID',
        'The Evidence detail returned an invalid Artifact set.',
      )
    }
    const identities = new Set<string>()
    for (const descriptor of result.artifactAccess.items) {
      expectArtifactDescriptor(descriptor, selected)
      if (identities.has(descriptor.artifactId)) throw clientFailure(
        'STRONGFLOW_EVIDENCE_ARTIFACTS_INVALID',
        'The Evidence detail returned duplicate Artifact selectors.',
      )
      identities.add(descriptor.artifactId)
    }
  }
  return result
}

function expectContentResult(
  response: QueryResultResponse,
  selected: StrongFlowEvidenceSelection,
  descriptor: EvidenceArtifactDescriptorProjection,
  request: EvidenceArtifactContentGetParameters,
): EvidenceArtifactContentGetResultResponse['result'] {
  if (response.query !== QueryName.EvidenceArtifactContentGet) throw clientFailure(
    'STRONGFLOW_EVIDENCE_CONTENT_QUERY_MISMATCH',
    'The Control Plane returned another Evidence content result.',
  )
  if (response.page.hasMore || response.page.nextCursor !== null) throw clientFailure(
    'STRONGFLOW_EVIDENCE_CONTENT_PAGE_INVALID',
    'The Evidence content query returned an unexpected page cursor.',
  )
  const result = (response as EvidenceArtifactContentGetResultResponse).result
  if (result.state === 'unavailable') {
    if (
      result.artifactId !== descriptor.artifactId
      || result.evidenceId !== selected.binding.evidenceId
      || !jsonEqual(result.readCursor, selected.binding.atCursor)
    ) throw clientFailure(
      'STRONGFLOW_EVIDENCE_CONTENT_IDENTITY_MISMATCH',
      'The unavailable Evidence Artifact response does not match the request.',
    )
    return result
  }
  let bytes: Uint8Array<ArrayBuffer>
  try {
    bytes = decodeBase64(result.dataBase64)
  } catch (error) {
    throw clientFailure(
      'STRONGFLOW_EVIDENCE_CONTENT_RANGE_INVALID',
      'The Evidence Artifact response did not contain a valid bounded range.',
      error,
    )
  }
  const end = result.offset + result.returnedBytes
  const continuationValid = result.nextOffset === null
    ? (end === result.totalBytes || (result.truncated && end <= result.totalBytes))
    : result.nextOffset === end && result.nextOffset > result.offset && result.nextOffset < result.totalBytes
  if (
    !jsonEqual(result.evidence, selected.row)
    || !jsonEqual(result.readCursor, selected.binding.atCursor)
    || !jsonEqual(result.artifact, descriptor)
    || result.previewMode !== descriptor.previewMode
  ) throw clientFailure(
    'STRONGFLOW_EVIDENCE_CONTENT_IDENTITY_MISMATCH',
    'The Evidence Artifact response does not match the requested authority.',
  )
  if (
    result.offset !== request.offset
    || result.totalBytes !== descriptor.sizeBytes
    || result.returnedBytes !== bytes.byteLength
    || result.returnedBytes < 1
    || result.returnedBytes > request.length
    || end > result.totalBytes
    || !continuationValid
  ) throw clientFailure(
    'STRONGFLOW_EVIDENCE_CONTENT_RANGE_INVALID',
    'The Evidence Artifact response did not match the requested bounded range.',
  )
  return result
}

function decodeBase64(base64: string): Uint8Array<ArrayBuffer> {
  const binary = atob(base64)
  const bytes = new Uint8Array(new ArrayBuffer(binary.length))
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index)
  }
  return bytes
}

function candidateState(
  row: DeliveryEvidenceProjection,
  projection: StrongFlowProjection,
): StrongFlowEvidenceCandidateState {
  const candidate = projection.currentCandidate
  if (candidate === null) return 'no-candidate'
  return row.candidateRef === candidate.candidateRef ? 'current' : 'superseded'
}

function selectionFor(
  row: DeliveryEvidenceProjection,
  projection: StrongFlowProjection,
): StrongFlowEvidenceSelection {
  return Object.freeze({
    row,
    binding: Object.freeze({
      atCursor: projection.metadata.readCursor,
      candidateRef: row.candidateRef,
      deliveryId: projection.delivery.deliveryId,
      evidenceId: row.id,
      readPageLimit: EVIDENCE_READ_PAGE_LIMIT,
      sessionBindingId: row.sessionBindingId,
      sourceRef: row.sourceRef,
      stageRunId: row.stageRunId,
      type: row.type,
    }),
    candidateState: candidateState(row, projection),
  })
}

function writeRoute(
  onRouteChange: (route: StrongFlowEvidenceRouteState) => void,
  tab: StrongFlowEvidenceTabId,
  evidenceId: EvidenceId | null,
): void {
  onRouteChange(Object.freeze({ tab, evidenceId }))
}

function unavailableContent(): StrongFlowEvidenceContentState {
  return Object.freeze({
    status: 'unavailable',
    artifact: null,
    text: null,
    loadedBytes: 0,
    totalBytes: 0,
    complete: false,
    truncated: false,
    error: null,
  })
}

function firstDescriptor(
  access: EvidenceArtifactAccessProjection,
): EvidenceArtifactDescriptorProjection | null {
  return access.state === 'available' ? (access.items[0] ?? null) : null
}

function sameSelection(
  left: StrongFlowEvidenceSelection,
  right: StrongFlowEvidenceSelection,
): boolean {
  return jsonEqual(left.row, right.row) && jsonEqual(left.binding, right.binding)
}

/** Build the exact StrongFlow Evidence workbench read model from the page projection. */
export function createStrongFlowEvidenceViewModel(
  options: StrongFlowEvidenceViewModelOptions,
): StrongFlowEvidenceViewModel {
  const listeners = new Set<StrongFlowEvidenceViewModelListener>()
  let currentState: StrongFlowEvidenceViewModelState = Object.freeze({
    tab: 'evidence',
    projection: null,
    selected: null,
    detail: null,
    content: null,
    search: '',
  })
  let generation = 0
  let closed = false
  let pendingDeepLinkEvidence: EvidenceId | null = null
  let detailController: AbortController | null = null
  let contentController: AbortController | null = null
  let contentDecoder: TextDecoder | null = null
  let decoderArtifactId: string | null = null

  currentState = Object.freeze({ ...currentState, tab: options.route.tab })
  pendingDeepLinkEvidence = options.route.evidenceId

  function publish(state: StrongFlowEvidenceViewModelState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<StrongFlowEvidenceViewModelState>): void {
    publish({ ...currentState, ...update })
  }

  function detailError(error: ControlPlaneClientError): void {
    patch({
      detail: Object.freeze({
        status: 'error',
        outcome: null,
        artifactAccess: null,
        error,
      }),
    })
  }

  function cancelDetail(): void {
    detailController?.abort()
    detailController = null
  }

  function cancelContent(): void {
    contentController?.abort()
    contentController = null
    contentDecoder = null
    decoderArtifactId = null
  }

  function cancelReads(): void {
    cancelDetail()
    cancelContent()
  }

  function decoderFor(descriptor: EvidenceArtifactDescriptorProjection): TextDecoder {
    if (contentDecoder === null || decoderArtifactId !== descriptor.artifactId) {
      contentDecoder = new TextDecoder('utf-8', { fatal: true })
      decoderArtifactId = descriptor.artifactId
    }
    return contentDecoder
  }

  function contentForDescriptor(
    descriptor: EvidenceArtifactDescriptorProjection,
  ): StrongFlowEvidenceContentState {
    const downloadOnly = descriptor.previewMode === EvidenceArtifactPreviewMode.DownloadOnly
    return Object.freeze({
      status: downloadOnly ? 'download-only' : 'idle',
      artifact: descriptor,
      text: null,
      loadedBytes: 0,
      totalBytes: descriptor.sizeBytes,
      complete: false,
      truncated: false,
      error: null,
    })
  }

  function contentForDetail(detail: EvidenceDetailProjection): StrongFlowEvidenceContentState {
    const descriptor = firstDescriptor(detail.artifactAccess)
    if (descriptor === null) return unavailableContent()
    return contentForDescriptor(descriptor)
  }

  async function runDetail(evidenceId: EvidenceId): Promise<void> {
    const ownGeneration = generation
    const selected = currentState.selected
    if (selected === null || selected.row.id !== evidenceId) return
    cancelDetail()
    cancelContent()
    const controller = new AbortController()
    detailController = controller
    patch({
      detail: Object.freeze({
        status: 'loading',
        outcome: null,
        artifactAccess: null,
        error: null,
      }),
      content: null,
      search: '',
    })
    try {
      const response = await options.client.query({
        schemaVersion: SCHEMA_VERSION,
        requestId: options.nextRequestId(),
        actor: options.actor,
        scope: options.scope,
        query: QueryName.EvidenceGet,
        parameters: selected.binding,
        page: QUERY_PAGE,
      }, { signal: controller.signal })
      if (closed || ownGeneration !== generation) return
      const result = expectDetailResult(response, selected)
      if (detailController === controller) detailController = null
      patch({
        detail: Object.freeze({
          status: 'ready',
          outcome: result.outcome,
          artifactAccess: result.artifactAccess,
          error: null,
        }),
        content: contentForDetail(result),
      })
      const descriptor = firstDescriptor(result.artifactAccess)
      if (descriptor !== null && descriptor.previewMode === EvidenceArtifactPreviewMode.InlineText) {
        await loadNextChunk()
      }
    } catch (error) {
      if (closed || ownGeneration !== generation) return
      if (detailController === controller) detailController = null
      detailError(normalizedError(error))
    }
  }

  async function openEvidence(evidenceId: EvidenceId): Promise<void> {
    if (closed) throw clientFailure(
      'STRONGFLOW_EVIDENCE_VIEW_MODEL_CLOSED',
      'The Evidence workbench is closed.',
    )
    const projection = currentState.projection
    if (projection === null) {
      detailError(clientFailure(
        'STRONGFLOW_EVIDENCE_SNAPSHOT_REQUIRED',
        'Refresh StrongFlow before opening Evidence detail.',
      ))
      return
    }
    const row = projection.evidence.find(candidate => candidate.id === evidenceId)
    if (row === undefined) {
      detailError(clientFailure(
        'STRONGFLOW_EVIDENCE_NOT_IN_SNAPSHOT',
        'Choose an Evidence record from the current snapshot.',
      ))
      return
    }
    pendingDeepLinkEvidence = null
    generation += 1
    cancelReads()
    patch({
      selected: selectionFor(row, projection),
      detail: Object.freeze({
        status: 'loading',
        outcome: null,
        artifactAccess: null,
        error: null,
      }),
      content: null,
      search: '',
    })
    writeRoute(options.onRouteChange, currentState.tab, evidenceId)
    await runDetail(evidenceId)
  }

  function onModelState(state: StrongFlowViewModelState): void {
    if (closed) return
    const projection = state.projection
    const selected = currentState.selected
    let nextSelected: StrongFlowEvidenceSelection | null = null
    if (projection !== null && selected !== null) {
      const row = projection.evidence.find(candidate => candidate.id === selected.row.id)
      if (row !== undefined) nextSelected = selectionFor(row, projection)
    }
    const identityChanged = selected !== null
      && nextSelected !== null
      && !sameSelection(selected, nextSelected)
    if (projection === null && selected !== null) {
      pendingDeepLinkEvidence = selected.row.id
    }
    if (selected !== null && (nextSelected === null || identityChanged)) {
      generation += 1
      cancelReads()
    }
    patch({
      projection,
      selected: nextSelected,
      detail: nextSelected === null
        ? null
        : identityChanged
          ? Object.freeze({
              status: 'loading',
              outcome: null,
              artifactAccess: null,
              error: null,
            })
          : currentState.detail,
      content: nextSelected === null || identityChanged ? null : currentState.content,
      search: nextSelected === null || identityChanged ? '' : currentState.search,
    })
    if (identityChanged && nextSelected !== null) void runDetail(nextSelected.row.id)
    if (pendingDeepLinkEvidence !== null && projection !== null) {
      const evidenceId = pendingDeepLinkEvidence
      pendingDeepLinkEvidence = null
      if (projection.evidence.some(row => row.id === evidenceId)) {
        void openEvidence(evidenceId)
      } else {
        writeRoute(options.onRouteChange, currentState.tab, null)
      }
    } else if (projection !== null && selected !== null && nextSelected === null) {
      writeRoute(options.onRouteChange, currentState.tab, null)
    }
  }

  const unsubscribeModel = options.model.subscribe(onModelState)

  function chunkParameters(
    binding: EvidenceReadBinding,
    descriptor: EvidenceArtifactDescriptorProjection,
    offset: number,
  ): EvidenceArtifactContentGetParameters {
    const length = Math.min(
      MAX_ARTIFACT_CHUNK_BYTES,
      Math.max(1, descriptor.sizeBytes - offset),
    )
    return Object.freeze({
      artifactDigest: descriptor.digest,
      artifactId: descriptor.artifactId,
      artifactKind: descriptor.kind,
      artifactMediaType: descriptor.mediaType,
      artifactSizeBytes: descriptor.sizeBytes,
      evidence: binding,
      length,
      offset,
    })
  }

  async function fetchChunk(
    ownGeneration: number,
    binding: EvidenceReadBinding,
    descriptor: EvidenceArtifactDescriptorProjection,
    offset: number,
    controller: AbortController,
  ): Promise<EvidenceArtifactContentChunkProjection | null> {
    const parameters = chunkParameters(binding, descriptor, offset)
    const response = await options.client.query({
      schemaVersion: SCHEMA_VERSION,
      requestId: options.nextRequestId(),
      actor: options.actor,
      scope: options.scope,
      query: QueryName.EvidenceArtifactContentGet,
      parameters,
      page: QUERY_PAGE,
    }, { signal: controller.signal })
    if (closed || ownGeneration !== generation) return null
    const selected = currentState.selected
    if (selected === null || !jsonEqual(selected.binding, binding)) return null
    const result = expectContentResult(response, selected, descriptor, parameters)
    if (result.state === 'unavailable') return null
    return result
  }

  function applyChunk(
    state: StrongFlowEvidenceContentState,
    chunk: EvidenceArtifactContentChunkProjection,
  ): StrongFlowEvidenceContentState {
    const inlineEligible = chunk.contentEncoding === EvidenceArtifactContentEncoding.Utf8
      && chunk.previewMode === EvidenceArtifactPreviewMode.InlineText
    if (!inlineEligible) {
      return Object.freeze({
        ...state,
        status: 'download-only',
        text: null,
        complete: false,
      })
    }
    const previous = state.text ?? ''
    const complete = chunk.nextOffset === null
    const decoder = decoderFor(chunk.artifact)
    const text = chunk.offset === state.loadedBytes
      ? previous + decoder.decode(decodeBase64(chunk.dataBase64), { stream: !complete })
      : previous
    if (complete) {
      contentDecoder = null
      decoderArtifactId = null
    }
    return Object.freeze({
      ...state,
      status: 'ready',
      artifact: chunk.artifact,
      text,
      loadedBytes: Math.max(state.loadedBytes, chunk.offset + chunk.returnedBytes),
      totalBytes: chunk.totalBytes,
      complete,
      truncated: chunk.truncated,
      error: null,
    })
  }

  async function loadNextChunk(): Promise<void> {
    if (closed) return
    const content = currentState.content
    const selected = currentState.selected
    if (
      content === null
      || selected === null
      || content.artifact === null
      || (content.status !== 'idle' && content.status !== 'ready')
      || content.complete
      || content.loadedBytes >= content.totalBytes
    ) return
    const ownGeneration = generation
    contentController?.abort()
    contentController = null
    if (decoderArtifactId !== content.artifact.artifactId || content.loadedBytes === 0) {
      contentDecoder = null
      decoderArtifactId = null
    }
    const controller = new AbortController()
    contentController = controller
    patch({ content: Object.freeze({ ...content, status: 'loading' }) })
    try {
      const chunk = await fetchChunk(
        ownGeneration,
        selected.binding,
        content.artifact,
        content.loadedBytes,
        controller,
      )
      if (closed || ownGeneration !== generation || chunk === null) {
        if (contentController === controller) contentController = null
        if (chunk === null && !closed && ownGeneration === generation) {
          patch({ content: unavailableContent() })
        }
        return
      }
      if (contentController === controller) contentController = null
      patch({ content: applyChunk(currentState.content ?? content, chunk) })
    } catch (error) {
      if (closed || ownGeneration !== generation) return
      if (contentController === controller) contentController = null
      contentDecoder = null
      decoderArtifactId = null
      patch({ content: Object.freeze({
        ...(currentState.content ?? content),
        status: 'error',
        error: normalizedError(error),
      }) })
    }
  }

  async function downloadArtifact(): Promise<void> {
    if (closed) return
    const content = currentState.content
    const selected = currentState.selected
    if (content === null || selected === null || content.artifact === null) return
    const descriptor = content.artifact
    generation += 1
    cancelContent()
    const ownGeneration = generation
    const controller = new AbortController()
    contentController = controller
    const parts: Uint8Array<ArrayBuffer>[] = []
    let offset = 0
    try {
      for (;;) {
        const chunk = await fetchChunk(
          ownGeneration,
          selected.binding,
          descriptor,
          offset,
          controller,
        )
        if (closed || ownGeneration !== generation) return
        if (chunk === null) {
          if (contentController === controller) contentController = null
          patch({ content: unavailableContent() })
          return
        }
        parts.push(decodeBase64(chunk.dataBase64))
        const nextOffset = chunk.nextOffset
        if (nextOffset === null) break
        offset = nextOffset
      }
      const totalBytes = parts.reduce((sum, part) => sum + part.length, 0)
      const bytes = new Uint8Array(new ArrayBuffer(totalBytes))
      let cursor = 0
      for (const part of parts) {
        bytes.set(part, cursor)
        cursor += part.length
      }
      ;(options.downloader ?? browserDownloader())({
        fileName: descriptor.fileName,
        mediaType: descriptor.mediaType,
        bytes,
      })
      if (contentController === controller) contentController = null
    } catch (error) {
      if (closed || ownGeneration !== generation) return
      if (contentController === controller) contentController = null
      patch({ content: Object.freeze({
        ...content,
        status: 'error',
        error: normalizedError(error),
      }) })
    }
  }

  return {
    get state() {
      return currentState
    },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    selectTab(tab) {
      if (closed || currentState.tab === tab) return
      writeRoute(options.onRouteChange, tab, currentState.selected?.row.id ?? null)
      patch({ tab })
    },
    async openEvidence(evidenceId) {
      await openEvidence(evidenceId)
    },
    closeEvidence() {
      if (
        closed
        || (currentState.selected === null && pendingDeepLinkEvidence === null)
      ) return
      pendingDeepLinkEvidence = null
      generation += 1
      cancelReads()
      writeRoute(options.onRouteChange, currentState.tab, null)
      patch({ selected: null, detail: null, content: null, search: '' })
    },
    async retryDetail() {
      if (currentState.selected === null) return
      generation += 1
      cancelReads()
      await runDetail(currentState.selected.row.id)
    },
    async loadNextChunk() {
      await loadNextChunk()
    },
    async selectArtifact(artifactId) {
      if (closed) return
      const access = currentState.detail?.artifactAccess
      if (access === null || access === undefined || access.state !== 'available') return
      const descriptor = access.items.find(item => item.artifactId === artifactId)
      if (descriptor === undefined || currentState.content?.artifact?.artifactId === artifactId) return
      generation += 1
      cancelContent()
      patch({ content: contentForDescriptor(descriptor), search: '' })
      if (descriptor.previewMode === EvidenceArtifactPreviewMode.InlineText) await loadNextChunk()
    },
    async downloadArtifact() {
      await downloadArtifact()
    },
    setSearch(value) {
      if (currentState.search === value) return
      patch({ search: value })
    },
    close() {
      if (closed) return
      closed = true
      generation += 1
      cancelReads()
      unsubscribeModel()
      listeners.clear()
    },
  }
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${String(bytes)} B`
  if (bytes < 1024 * 1024) return `${String(Math.round(bytes / 1024))} KiB`
  return `${String(Math.round(bytes / (1024 * 1024)))} MiB`
}

function candidateStateText(state: StrongFlowEvidenceCandidateState): string {
  if (state === 'current') return 'current candidate'
  if (state === 'superseded') return 'superseded candidate'
  return 'no current candidate'
}

/** Mount the Evidence, Tests, and Logs workbench against one StrongFlow view-model. */
export function mountStrongFlowEvidence(
  options: StrongFlowEvidenceWorkbenchOptions,
): StrongFlowEvidenceWorkbench {
  const document = options.root.ownerDocument
  const limits = options.limits ?? { evidence: DEFAULT_STRONGFLOW_RENDER_LIMITS.evidence }
  const model = createStrongFlowEvidenceViewModel({
    client: options.client,
    actor: options.actor,
    scope: options.scope,
    nextRequestId: options.nextRequestId,
    model: options.model,
    route: options.route,
    onRouteChange: options.onRouteChange,
    ...(options.downloader === undefined ? {} : { downloader: options.downloader }),
  })
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-evidence-workbench')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const summary = strongFlowElement(document, 'section', 'wwc-strongflow-evidence-summary')
  const summaryCounts = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-summary-counts')
  const summaryFailures = strongFlowElement(document, 'ul', 'wwc-strongflow-evidence-failures')
  const summaryOmitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const list = strongFlowElement(document, 'ul', 'wwc-strongflow-evidence-list')
  const omitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const drawerContent = strongFlowElement(document, 'div', 'wwc-strongflow-evidence-drawer-body')
  const detailPanel = strongFlowElement(document, 'section', 'wwc-strongflow-evidence-detail')
  const outcome = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-detail-outcome')
  const candidate = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-detail-candidate')
  const detailSummary = strongFlowElement(document, 'dl', 'wwc-strongflow-evidence-detail-summary')
  const bindingTerm = document.createElement('dt')
  const bindingValue = document.createElement('dd')
  const detailErrorNode = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-error')
  const artifactList = strongFlowElement(document, 'ul', 'wwc-strongflow-evidence-artifacts')
  const artifact = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-artifact')
  const contentScope = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-content-scope')
  const contentText = strongFlowElement(document, 'pre', 'wwc-strongflow-evidence-content-text')
  const search = strongFlowElement(
    document,
    'input',
    'wwc-strongflow-evidence-search',
  ) as HTMLInputElement
  const download = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-evidence-download',
  ) as HTMLButtonElement
  const retryDetail = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-evidence-retry',
  ) as HTMLButtonElement
  const loadMore = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-evidence-load-more',
  ) as HTMLButtonElement
  let closed = false
  let drawerProps = {
    id: 'strongflow-evidence-drawer',
    title: 'Evidence detail',
    open: false,
    content: drawerContent,
    closeLabel: 'Close Evidence detail',
    className: 'wwc-strongflow-evidence-drawer',
    onClose: () => { model.closeEvidence() },
  }
  let tabsProps: TabsProps = {
    id: 'strongflow-evidence-tabs',
    label: 'Evidence views',
    className: 'wwc-strongflow-evidence-tabs',
    tabs: EVIDENCE_TABS.map(tab => ({
      id: tab.id,
      label: tab.label,
      panelId: `strongflow-evidence-panel-${tab.id}`,
    })),
    selectedId: model.state.tab,
    onSelect: (id: string) => { model.selectTab(id as StrongFlowEvidenceTabId) },
  }
  const tabs = mountTabs({ document, props: tabsProps })
  const drawer = mountDrawer({ document, props: drawerProps })
  const panels = new Map<StrongFlowEvidenceTabId, HTMLElement>()
  let evidenceOpener: HTMLElement | null = null
  let drawerWasOpen = false
  function rememberEvidenceOpener(): void {
    const active = document.activeElement
    if (
      active !== null
      && active !== drawer.closeButton
      && typeof Reflect.get(active, 'focus') === 'function'
    ) {
      evidenceOpener = active as HTMLElement
    }
  }
  for (const tab of EVIDENCE_TABS) {
    const panel = strongFlowElement(document, 'section', 'wwc-strongflow-evidence-panel')
    panel.id = `strongflow-evidence-panel-${tab.id}`
    panel.setAttribute('role', 'tabpanel')
    panel.setAttribute('aria-labelledby', `strongflow-evidence-tabs-${tab.id}`)
    panel.tabIndex = 0
    panels.set(tab.id, panel)
  }

  heading.textContent = 'Evidence, test results, and logs'
  summary.setAttribute('aria-label', 'Current Verdict evidence summary')
  summaryFailures.setAttribute('aria-label', 'Key criterion failures')
  bindingTerm.textContent = 'Authority binding'
  detailSummary.append(bindingTerm, bindingValue)
  detailErrorNode.setAttribute('role', 'alert')
  detailErrorNode.setAttribute('aria-live', 'assertive')
  search.type = 'search'
  search.setAttribute('aria-label', 'Search loaded log text')
  search.setAttribute('placeholder', 'Search loaded text')
  download.type = 'button'
  download.textContent = 'Download artifact'
  retryDetail.type = 'button'
  retryDetail.textContent = 'Retry Evidence detail'
  loadMore.type = 'button'
  loadMore.textContent = 'Load next bounded range'
  detailPanel.append(
    outcome,
    candidate,
    detailSummary,
    detailErrorNode,
    artifactList,
    search,
    artifact,
    contentScope,
    contentText,
    retryDetail,
    loadMore,
    download,
  )
  drawerContent.append(detailPanel)
  summary.append(summaryCounts, summaryFailures, summaryOmitted)
  root.append(heading, summary, tabs.root, ...panels.values(), drawer.root)
  options.root.replaceChildren(root)

  interface EvidenceRowItem {
    readonly row: DeliveryEvidenceProjection
    readonly candidateState: StrongFlowEvidenceCandidateState
    readonly criterionIds: readonly string[]
  }
  const rowRecords = new WeakMap<HTMLElement, {
    readonly onClick: () => void
    readonly title: HTMLElement
    readonly source: HTMLElement
    readonly candidate: HTMLElement
    readonly criteria: HTMLElement
  }>()
  const rows = mountKeyedCollection<EvidenceRowItem, string, HTMLLIElement>({
    parent: list,
    key: item => item.row.id,
    create() {
      const item = strongFlowElement(document, 'li', 'wwc-strongflow-evidence-row')
      const open = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-evidence-open',
      ) as HTMLButtonElement
      const title = document.createElement('strong')
      const source = document.createElement('p')
      const candidate = document.createElement('span')
      const criteria = strongFlowElement(document, 'span', 'wwc-strongflow-evidence-criteria')
      open.type = 'button'
      open.textContent = 'Open'
      const onClick = () => {
        const identity = item.dataset.evidenceId
        if (identity === undefined) return
        rememberEvidenceOpener()
        void model.openEvidence(identity as EvidenceId)
      }
      item.addEventListener('click', onClick)
      item.append(title, source, candidate, criteria, open)
      rowRecords.set(item, { onClick, title, source, candidate, criteria })
      return item
    },
    update(item, value) {
      const record = rowRecords.get(item)
      if (record === undefined) return
      item.dataset.evidenceId = value.row.id
      item.dataset.evidenceType = value.row.type
      item.dataset.candidateState = value.candidateState
      record.title.textContent = `${value.row.type} · ${value.row.id}`
      record.source.textContent = `${value.row.sourceRef} · ${value.row.createdAt} · ${
          value.row.stageRunId
        }`
      record.candidate.textContent = candidateStateText(value.candidateState)
      record.criteria.textContent = value.criterionIds.length === 0
        ? 'No current criterion join'
        : `Criteria: ${value.criterionIds.join(', ')}`
    },
    remove(item) {
      const record = rowRecords.get(item)
      if (record === undefined) return
      item.removeEventListener?.('click', record.onClick)
      rowRecords.delete(item)
    },
  })

  const artifactRecords = new WeakMap<HTMLElement, { readonly onClick: () => void }>()
  const artifacts = mountKeyedCollection<
    EvidenceArtifactDescriptorProjection,
    string,
    HTMLLIElement
  >({
    parent: artifactList,
    key: descriptor => descriptor.artifactId,
    create() {
      const item = document.createElement('li')
      const select = strongFlowElement(
        document,
        'button',
        'wwc-strongflow-evidence-artifact-select',
      ) as HTMLButtonElement
      select.type = 'button'
      const onClick = () => {
        const artifactId = item.dataset.artifactId
        if (artifactId !== undefined) void model.selectArtifact(artifactId)
      }
      select.addEventListener('click', onClick)
      item.append(select)
      artifactRecords.set(item, { onClick })
      return item
    },
    update(item, descriptor) {
      item.dataset.artifactId = descriptor.artifactId
      const select = item.children[0]
      if (select !== undefined) {
        select.textContent = descriptor.fileName ?? descriptor.artifactId
        select.setAttribute(
          'aria-pressed',
          model.state.content?.artifact?.artifactId === descriptor.artifactId ? 'true' : 'false',
        )
      }
    },
    remove(item) {
      const record = artifactRecords.get(item)
      const select = item.children[0]
      if (record !== undefined && select !== undefined) {
        select.removeEventListener?.('click', record.onClick)
      }
      artifactRecords.delete(item)
    },
  })

  function renderDetail(state: StrongFlowEvidenceViewModelState): void {
    const selected = state.selected
    drawerProps = {
      ...drawerProps,
      title: selected === null ? 'Evidence detail' : `Evidence ${selected.row.id}`,
      open: selected !== null,
    }
    drawer.update(drawerProps)
    if (selected === null && drawerWasOpen) {
      evidenceOpener?.focus()
      evidenceOpener = null
    }
    drawerWasOpen = selected !== null
    detailPanel.hidden = selected === null
    if (selected === null) {
      detailPanel.removeAttribute('data-evidence-id')
      detailPanel.removeAttribute('data-candidate-state')
      detailPanel.setAttribute('aria-busy', 'false')
      artifacts.update([])
      return
    }
    const detail = state.detail
    const content = state.content
    detailPanel.dataset.evidenceId = selected.row.id
    detailPanel.dataset.candidateState = selected.candidateState
    detailPanel.dataset.status = detail?.status ?? 'loading'
    detailPanel.setAttribute(
      'aria-busy',
      String(detail === null || detail.status === 'loading' || content?.status === 'loading'),
    )
    bindingValue.textContent = `${selected.row.candidateRef} · ${selected.row.stageRunId} · ${
      selected.row.sessionBindingId
    } · Delivery ${selected.binding.deliveryId} at revision ${
      String(selected.binding.atCursor.deliveryRevision)
    }`
    candidate.textContent = candidateStateText(selected.candidateState)
    detailErrorNode.hidden = true
    detailErrorNode.textContent = ''
    retryDetail.hidden = true
    artifactList.hidden = true
    artifact.hidden = true
    contentScope.hidden = true
    contentText.hidden = true
    search.hidden = true
    loadMore.hidden = true
    download.hidden = true
    if (detail === null) {
      artifacts.update([])
      outcome.textContent = 'Loading Evidence detail…'
      return
    }
    const presentation = detail.outcome === null
      ? null
      : strongFlowEvidenceOutcomePresentation.get(detail.outcome) ?? null
    outcome.textContent = presentation === null ? 'Outcome not reported' : presentation.label
    outcome.dataset.tone = presentation?.tone ?? 'neutral'
    outcome.setAttribute('aria-label', `Evidence outcome: ${outcome.textContent}`)
    if (detail.status === 'error' && detail.error !== null) {
      artifacts.update([])
      detailErrorNode.hidden = false
      detailErrorNode.textContent = strongFlowEvidenceErrorText(detail.error)
      retryDetail.hidden = false
      return
    }
    const descriptors = detail.artifactAccess?.state === 'available'
      ? detail.artifactAccess.items
      : []
    artifacts.update(descriptors)
    artifactList.hidden = descriptors.length === 0
    if (content === null || content.status === 'unavailable') {
      artifact.hidden = false
      artifact.textContent = 'Artifact content is not available for this Evidence.'
    } else if (content.status === 'download-only') {
      artifact.hidden = false
      artifact.textContent = 'Binary or download-only artifact. Use the download control.'
      download.hidden = false
    } else if (content.status === 'error' && content.error !== null) {
      detailErrorNode.hidden = false
      detailErrorNode.textContent = strongFlowEvidenceErrorText(content.error)
    } else if (content.artifact !== null) {
      const needle = state.search.trim().toLowerCase()
      const lines = (content.text ?? '').split('\n')
      contentText.textContent = needle.length === 0
        ? content.text ?? ''
        : lines.filter(line => line.toLowerCase().includes(needle)).join('\n')
      contentScope.textContent = content.status === 'loading'
        ? 'Loading the next bounded Artifact range…'
        : `${formatBytes(content.loadedBytes)} of ${
        formatBytes(content.totalBytes)
      } loaded${content.truncated ? ' · truncated at the source' : ''}${
        content.complete ? '' : ' · more ranges available'
      }`
      artifact.textContent = `${content.artifact.fileName ?? content.artifact.artifactId} · ${
        content.artifact.mediaType
      }`
      artifact.hidden = false
      contentScope.hidden = false
      contentText.hidden = false
      search.hidden = state.tab !== 'logs'
      if (search.value !== state.search) search.value = state.search
      loadMore.hidden = content.complete || content.status === 'loading'
      download.hidden = false
    }
  }

  function renderSummary(projection: StrongFlowProjection | null): StrongFlowEvidenceSummary | null {
    summaryFailures.replaceChildren()
    if (projection === null || projection.verdict === null) {
      summaryCounts.textContent = 'No current Verdict evidence summary.'
      summaryOmitted.hidden = true
      return null
    }
    const value = strongFlowEvidenceSummary(projection)
    summaryCounts.textContent = `${String(value.counts.total)} criteria · ${
      String(value.counts.pass)
    } passed · ${String(value.counts.fail)} failed · ${
      String(value.counts.inconclusive)
    } inconclusive · ${String(value.counts.infraError)} infrastructure errors`
    for (const failure of value.failures) {
      const item = document.createElement('li')
      item.dataset.verdict = failure.verdict
      item.textContent = `${failure.criterionId} · ${failure.verdict} · ${failure.explanation}`
      summaryFailures.append(item)
    }
    summaryOmitted.hidden = value.omittedFailures === 0
    summaryOmitted.textContent = `${String(value.omittedFailures)} more criterion failures not shown.`
    return value
  }

  function render(state: StrongFlowEvidenceViewModelState): void {
    if (closed) return
    if (tabsProps.selectedId !== state.tab) {
      tabsProps = { ...tabsProps, selectedId: state.tab }
      tabs.update(tabsProps)
    }
    const projection = state.projection
    const evidenceSummary = renderSummary(projection)
    const source = projection === null ? [] : projection.evidence
    const filtered = strongFlowEvidenceRowsForTab(source, state.tab)
    const bounded = boundedItems(filtered, limits.evidence)
    rows.update(bounded.items.map(row => ({
      row,
      candidateState: projection === null
        ? 'no-candidate' as StrongFlowEvidenceCandidateState
        : candidateState(row, projection),
      criterionIds: evidenceSummary?.criterionIdsByEvidence.get(row.id) ?? [],
    })))
    omitted.hidden = bounded.omitted === 0
    const omittedText = `${String(bounded.omitted)} more evidence records not rendered.`
    if (omitted.textContent !== omittedText) omitted.textContent = omittedText
    for (const [tab, panel] of panels) {
      const selected = tab === state.tab
      panel.hidden = !selected
      panel.setAttribute('aria-busy', String(selected && projection === null))
      if (selected) panel.append(list, omitted)
    }
    renderDetail(state)
  }

  const onSearchInput = () => { model.setSearch(search.value) }
  const onDownload = () => { void model.downloadArtifact() }
  const onRetryDetail = () => { void model.retryDetail() }
  const onLoadMore = () => { void model.loadNextChunk() }
  search.addEventListener('input', onSearchInput)
  download.addEventListener('click', onDownload)
  retryDetail.addEventListener('click', onRetryDetail)
  loadMore.addEventListener('click', onLoadMore)

  const unsubscribe = model.subscribe(render)

  return {
    async openEvidence(evidenceId) {
      rememberEvidenceOpener()
      await model.openEvidence(evidenceId)
    },
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      search.removeEventListener?.('input', onSearchInput)
      download.removeEventListener?.('click', onDownload)
      retryDetail.removeEventListener?.('click', onRetryDetail)
      loadMore.removeEventListener?.('click', onLoadMore)
      rows.close()
      artifacts.close()
      tabs.close()
      drawer.close()
      model.close()
      options.root.replaceChildren()
    },
  }
}

function browserDownloader(): (download: StrongFlowEvidenceDownload) => void {
  return value => {
    const href = URL.createObjectURL(new Blob([value.bytes], { type: value.mediaType }))
    const anchor = document.createElement('a')
    anchor.href = href
    anchor.download = value.fileName ?? 'evidence-artifact'
    anchor.click()
    URL.revokeObjectURL(href)
  }
}
