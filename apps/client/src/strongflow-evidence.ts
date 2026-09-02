// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './control-plane-client.js'
import type {
  Actor,
  DeliveryEvidenceProjection,
  EvidenceArtifactAccessProjection,
  EvidenceArtifactContentChunkProjection,
  EvidenceArtifactContentGetParameters,
  EvidenceArtifactContentGetResultResponse,
  EvidenceArtifactDescriptorProjection,
  EvidenceDetailProjection,
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

export type StrongFlowEvidenceTabId = 'evidence' | 'tests' | 'logs'

export type StrongFlowEvidenceCandidateState = 'current' | 'superseded' | 'no-candidate'

export type StrongFlowEvidenceOutcomeTone = 'pass' | 'business-fail' | 'infra' | 'neutral'

export interface StrongFlowEvidenceOutcomePresentation {
  readonly label: string
  readonly tone: StrongFlowEvidenceOutcomeTone
}

export interface StrongFlowEvidenceDeepLink {
  read(): URLSearchParams
  replace(hash: string): void
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
  readonly deepLink: StrongFlowEvidenceDeepLink
  readonly downloader?: (download: StrongFlowEvidenceDownload) => void
}

export interface StrongFlowEvidenceViewModel {
  readonly state: StrongFlowEvidenceViewModelState
  subscribe(listener: StrongFlowEvidenceViewModelListener): () => void
  selectTab(tab: StrongFlowEvidenceTabId): void
  openEvidence(evidenceId: string): Promise<void>
  closeEvidence(): void
  retryDetail(): Promise<void>
  loadNextChunk(): Promise<void>
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
  readonly deepLink?: StrongFlowEvidenceDeepLink
  readonly downloader?: (download: StrongFlowEvidenceDownload) => void
}

export interface StrongFlowEvidenceWorkbenchOptions extends StrongFlowEvidenceOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowViewModel
}

export interface StrongFlowEvidenceWorkbench {
  close(): void
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

function expectDetailResult(response: QueryResultResponse): EvidenceDetailProjection {
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
  return result
}

function expectContentResult(
  response: QueryResultResponse,
): EvidenceArtifactContentGetResultResponse['result'] {
  if (response.query !== QueryName.EvidenceArtifactContentGet) throw clientFailure(
    'STRONGFLOW_EVIDENCE_CONTENT_QUERY_MISMATCH',
    'The Control Plane returned another Evidence content result.',
  )
  if (response.page.hasMore || response.page.nextCursor !== null) throw clientFailure(
    'STRONGFLOW_EVIDENCE_CONTENT_PAGE_INVALID',
    'The Evidence content query returned an unexpected page cursor.',
  )
  return (response as EvidenceArtifactContentGetResultResponse).result
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

function writeDeepLink(
  deepLink: StrongFlowEvidenceDeepLink,
  tab: StrongFlowEvidenceTabId,
  evidenceId: string | null,
): void {
  const parameters = deepLink.read()
  parameters.set('tab', tab)
  if (evidenceId === null) parameters.delete('evidence')
  else parameters.set('evidence', evidenceId)
  deepLink.replace(`#/strongflow?${parameters.toString()}`)
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
  let pendingDeepLinkEvidence: string | null = null

  const initialParameters = options.deepLink.read()
  const initialTab = initialParameters.get('tab')
  if (initialTab === 'evidence' || initialTab === 'tests' || initialTab === 'logs') {
    currentState = Object.freeze({ ...currentState, tab: initialTab })
  }
  const initialEvidence = initialParameters.get('evidence')
  pendingDeepLinkEvidence = initialEvidence === null || initialEvidence.length === 0
    ? null
    : initialEvidence

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

  function contentForDetail(detail: EvidenceDetailProjection): StrongFlowEvidenceContentState {
    const descriptor = firstDescriptor(detail.artifactAccess)
    if (descriptor === null) return unavailableContent()
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

  async function runDetail(evidenceId: string): Promise<void> {
    const ownGeneration = generation
    const selected = currentState.selected
    if (selected === null || selected.row.id !== evidenceId) return
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
      })
      if (closed || ownGeneration !== generation) return
      const result = expectDetailResult(response)
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
      detailError(normalizedError(error))
    }
  }

  async function openEvidence(evidenceId: string): Promise<void> {
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
    generation += 1
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
    writeDeepLink(options.deepLink, currentState.tab, evidenceId)
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
    patch({
      projection,
      selected: nextSelected,
      detail: nextSelected === null ? null : currentState.detail,
      content: nextSelected === null ? null : currentState.content,
    })
    if (pendingDeepLinkEvidence !== null && projection !== null) {
      const evidenceId = pendingDeepLinkEvidence
      pendingDeepLinkEvidence = null
      if (projection.evidence.some(row => row.id === evidenceId)) {
        void openEvidence(evidenceId)
      }
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
  ): Promise<EvidenceArtifactContentChunkProjection | null> {
    const response = await options.client.query({
      schemaVersion: SCHEMA_VERSION,
      requestId: options.nextRequestId(),
      actor: options.actor,
      scope: options.scope,
      query: QueryName.EvidenceArtifactContentGet,
      parameters: chunkParameters(binding, descriptor, offset),
      page: QUERY_PAGE,
    })
    if (closed || ownGeneration !== generation) return null
    const result = expectContentResult(response)
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
    const text = chunk.offset === state.loadedBytes
      ? previous + new TextDecoder().decode(decodeBase64(chunk.dataBase64))
      : previous
    return Object.freeze({
      ...state,
      status: 'ready',
      artifact: chunk.artifact,
      text,
      loadedBytes: Math.max(state.loadedBytes, chunk.offset + chunk.returnedBytes),
      totalBytes: chunk.totalBytes,
      complete: chunk.nextOffset === null,
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
    patch({ content: Object.freeze({ ...content, status: 'loading' }) })
    try {
      const chunk = await fetchChunk(
        ownGeneration,
        selected.binding,
        content.artifact,
        content.loadedBytes,
      )
      if (closed || ownGeneration !== generation || chunk === null) {
        if (chunk === null && !closed && ownGeneration === generation) {
          patch({ content: unavailableContent() })
        }
        return
      }
      patch({ content: applyChunk(currentState.content ?? content, chunk) })
    } catch (error) {
      if (closed || ownGeneration !== generation) return
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
    const ownGeneration = generation
    const parts: Uint8Array<ArrayBuffer>[] = []
    let offset = 0
    try {
      for (;;) {
        const chunk = await fetchChunk(ownGeneration, selected.binding, descriptor, offset)
        if (closed || ownGeneration !== generation) return
        if (chunk === null) {
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
    } catch (error) {
      if (closed || ownGeneration !== generation) return
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
      writeDeepLink(options.deepLink, tab, currentState.selected?.row.id ?? null)
      patch({ tab })
    },
    async openEvidence(evidenceId) {
      await openEvidence(evidenceId)
    },
    closeEvidence() {
      if (closed || currentState.selected === null) return
      generation += 1
      writeDeepLink(options.deepLink, currentState.tab, null)
      patch({ selected: null, detail: null, content: null, search: '' })
    },
    async retryDetail() {
      if (currentState.selected === null) return
      generation += 1
      await runDetail(currentState.selected.row.id)
    },
    async loadNextChunk() {
      await loadNextChunk()
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
  const deepLink = options.deepLink ?? browserDeepLink(options.root)
  const model = createStrongFlowEvidenceViewModel({
    client: options.client,
    actor: options.actor,
    scope: options.scope,
    nextRequestId: options.nextRequestId,
    model: options.model,
    deepLink,
    ...(options.downloader === undefined ? {} : { downloader: options.downloader }),
  })
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-evidence-workbench')
  const heading = strongFlowElement(document, 'h3', 'wwc-strongflow-section-heading')
  const list = strongFlowElement(document, 'ul', 'wwc-strongflow-evidence-list')
  const omitted = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  const drawerContent = strongFlowElement(document, 'div', 'wwc-strongflow-evidence-drawer-body')
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

  heading.textContent = 'Evidence, test results, and logs'
  search.type = 'search'
  search.setAttribute('aria-label', 'Search loaded log text')
  search.setAttribute('placeholder', 'Search loaded text')
  download.type = 'button'
  download.textContent = 'Download artifact'
  retryDetail.type = 'button'
  retryDetail.textContent = 'Retry Evidence detail'
  loadMore.type = 'button'
  loadMore.textContent = 'Load next bounded range'
  root.append(heading, tabs.root, list, omitted, drawer.root)
  options.root.replaceChildren(root)

  interface EvidenceRowItem {
    readonly row: DeliveryEvidenceProjection
    readonly candidateState: StrongFlowEvidenceCandidateState
  }
  const rowRecords = new WeakMap<HTMLElement, { readonly onClick: () => void }>()
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
      open.type = 'button'
      open.textContent = 'Open'
      const onClick = () => {
        const identity = item.dataset.evidenceId
        if (identity === undefined) return
        void model.openEvidence(identity)
      }
      item.addEventListener('click', onClick)
      item.append(title, source, candidate, open)
      rowRecords.set(item, { onClick })
      return item
    },
    update(item, value) {
      if (rowRecords.get(item) === undefined) return
      item.dataset.evidenceId = value.row.id
      item.dataset.evidenceType = value.row.type
      item.dataset.candidateState = value.candidateState
      const title = item.children[0]
      const source = item.children[1]
      const candidate = item.children[2]
      if (title !== undefined) title.textContent = `${value.row.type} · ${value.row.id}`
      if (source !== undefined) {
        source.textContent = `${value.row.sourceRef} · ${value.row.createdAt} · ${
          value.row.stageRunId
        }`
      }
      if (candidate !== undefined) candidate.textContent = candidateStateText(value.candidateState)
    },
    remove(item) {
      const record = rowRecords.get(item)
      if (record === undefined) return
      item.removeEventListener?.('click', record.onClick)
      rowRecords.delete(item)
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
    if (selected === null) {
      drawerContent.replaceChildren()
      return
    }
    const detail = state.detail
    const panel = strongFlowElement(document, 'section', 'wwc-strongflow-evidence-detail')
    panel.dataset.evidenceId = selected.row.id
    panel.dataset.candidateState = selected.candidateState
    const outcome = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-detail-outcome')
    const candidate = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-detail-candidate')
    const summary = strongFlowElement(document, 'dl', 'wwc-strongflow-evidence-detail-summary')
    const bindingTerm = document.createElement('dt')
    const bindingValue = document.createElement('dd')
    bindingTerm.textContent = 'Authority binding'
    bindingValue.textContent = `${selected.row.candidateRef} · ${selected.row.stageRunId} · ${
      selected.row.sessionBindingId
    } · Delivery ${selected.binding.deliveryId} at revision ${
      String(selected.binding.atCursor.deliveryRevision)
    }`
    summary.append(bindingTerm, bindingValue)
    candidate.textContent = candidateStateText(selected.candidateState)
    panel.append(outcome, candidate, summary)
    if (detail === null) {
      panel.dataset.status = 'loading'
      outcome.textContent = 'Loading Evidence detail…'
      drawerContent.replaceChildren(panel)
      return
    }
    panel.dataset.status = detail.status
    const presentation = detail.outcome === null
      ? null
      : strongFlowEvidenceOutcomePresentation.get(detail.outcome) ?? null
    outcome.textContent = presentation === null ? 'Outcome not reported' : presentation.label
    outcome.dataset.tone = presentation?.tone ?? 'neutral'
    if (detail.status === 'error' && detail.error !== null) {
      const failure = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-error')
      failure.textContent = strongFlowEvidenceErrorText(detail.error)
      panel.append(failure, retryDetail)
      drawerContent.replaceChildren(panel)
      return
    }
    const artifact = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-artifact')
    const content = state.content
    if (content === null || content.status === 'unavailable') {
      artifact.textContent = 'Artifact content is not available for this Evidence.'
      panel.append(artifact)
    } else if (content.status === 'download-only') {
      artifact.textContent = 'Binary or download-only artifact. Use the download control.'
      panel.append(artifact, download)
    } else if (content.status === 'error' && content.error !== null) {
      const failure = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-error')
      failure.textContent = strongFlowEvidenceErrorText(content.error)
      panel.append(failure)
    } else if (content.artifact !== null) {
      const scope = strongFlowElement(document, 'p', 'wwc-strongflow-evidence-content-scope')
      const text = strongFlowElement(document, 'pre', 'wwc-strongflow-evidence-content-text')
      const needle = state.search.trim().toLowerCase()
      const lines = (content.text ?? '').split('\n')
      text.textContent = needle.length === 0
        ? content.text ?? ''
        : lines.filter(line => line.toLowerCase().includes(needle)).join('\n')
      scope.textContent = `${formatBytes(content.loadedBytes)} of ${
        formatBytes(content.totalBytes)
      } loaded${content.truncated ? ' · truncated at the source' : ''}${
        content.complete ? '' : ' · more ranges available'
      }`
      artifact.textContent = `${content.artifact.fileName ?? content.artifact.artifactId} · ${
        content.artifact.mediaType
      }`
      if (state.tab === 'logs') panel.append(search)
      panel.append(artifact, scope, text)
      if (!content.complete) panel.append(loadMore)
      panel.append(download)
    }
    drawerContent.replaceChildren(panel)
  }

  function render(state: StrongFlowEvidenceViewModelState): void {
    if (closed) return
    if (tabsProps.selectedId !== state.tab) {
      tabsProps = { ...tabsProps, selectedId: state.tab }
      tabs.update(tabsProps)
    }
    const projection = state.projection
    const source = projection === null ? [] : projection.evidence
    const filtered = strongFlowEvidenceRowsForTab(source, state.tab)
    const bounded = boundedItems(filtered, limits.evidence)
    rows.update(bounded.items.map(row => ({
      row,
      candidateState: projection === null
        ? 'no-candidate' as StrongFlowEvidenceCandidateState
        : candidateState(row, projection),
    })))
    omitted.hidden = bounded.omitted === 0
    const omittedText = `${String(bounded.omitted)} more evidence records not rendered.`
    if (omitted.textContent !== omittedText) omitted.textContent = omittedText
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
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      search.removeEventListener?.('input', onSearchInput)
      download.removeEventListener?.('click', onDownload)
      retryDetail.removeEventListener?.('click', onRetryDetail)
      loadMore.removeEventListener?.('click', onLoadMore)
      rows.close()
      tabs.close()
      drawer.close()
      model.close()
      options.root.replaceChildren()
    },
  }
}

function browserDeepLink(root: HTMLElement): StrongFlowEvidenceDeepLink {
  const view = root.ownerDocument.defaultView
  return {
    read() {
      const hash = view?.location.hash ?? ''
      const query = hash.indexOf('?')
      return new URLSearchParams(query < 0 ? '' : hash.slice(query + 1))
    },
    replace(hash) {
      if (view === null) return
      view.history.replaceState(null, '', `${view.location.pathname}${view.location.search}${hash}`)
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
