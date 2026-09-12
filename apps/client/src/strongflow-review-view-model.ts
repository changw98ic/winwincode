// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './community-control-plane-client.js'
import type {
  Actor,
  CandidateAvailability,
  CandidateDiffChunkProjection,
  CandidateFileEncoding,
  CandidateFilePage,
  CandidateFileProjection,
  CandidateFileStatus,
  CandidateHistoricalReviewProjection,
  CandidateHistoryItemProjection,
  CommandRequest,
  DeliveryDetailProjection,
  DeliveryEvidenceProjection,
  EvidenceArtifactDescriptorProjection,
  EvidenceOutcome,
  QueryRequest,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
  RuntimeActivityProjection,
  RuntimeProjectionSnapshot,
  SolutionReviewProjection,
  StrongFlowReadCursor,
} from './generated/contracts.js'
import { CommandName, QueryName } from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
/** List page size; the Control Plane accepts 1..=200 per projection read. */
const LIST_PAGE_LIMIT = 100
/** Exact reads seal one delivery.get cut; the sealed page uses the same bound. */
const READ_PAGE_LIMIT = 100
/** One Candidate diff range; the server accepts at most 256 KiB per chunk. */
const DIFF_CHUNK_BYTES = 64 * 1024
/** Bounded number of file-list pages one refresh may read. */
const MAX_FILE_PAGES = 8
/** Bounded number of execution sessions one refresh may read. */
export const REVIEW_SESSION_LIMIT = 4
/** The runtime projection keeps at most 100 activities per session. */
const MAX_ACTIVITIES_PER_SESSION = 100
/** Paths that browsers execute in page context are never previewed inline. */
const EXECUTABLE_EXTENSIONS: readonly string[] = Object.freeze([
  '.html',
  '.htm',
  '.svg',
  '.xhtml',
  '.xht',
])

export type StrongFlowReviewStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'refreshing'
  | 'authentication-required'
  | 'authorization-denied'
  | 'error'
  | 'closed'

/**
 * RUN-07 degradation classes for one Candidate file preview. `text` is the only
 * class whose bytes are rendered; every other class stays metadata-only.
 */
export type ReviewPreviewClass =
  | 'text'
  | 'executable-document'
  | 'binary'
  | 'unknown-encoding'

export interface ReviewPreviewDecodedChunk {
  readonly offset: number
  readonly returnedBytes: number
  readonly text: string
}

export interface ReviewPreviewState {
  readonly path: string
  readonly previewClass: ReviewPreviewClass
  readonly degradationReason: string | null
  readonly chunks: readonly ReviewPreviewDecodedChunk[]
  readonly totalBytes: number
  readonly returnedBytes: number
  /** Null once the whole diff range has been read; set for the next range. */
  readonly nextOffset: number | null
}

export interface ReviewFileEntry {
  readonly path: string
  readonly oldPath: string | null
  readonly status: CandidateFileStatus
  readonly encoding: CandidateFileEncoding
  readonly binary: boolean
  readonly additions: number | null
  readonly deletions: number | null
  readonly preview: ReviewPreviewState | null
  readonly previewError: string | null
}

export interface ReviewActivity {
  readonly callId: string
  readonly activityType: RuntimeActivityProjection['activityType']
  readonly command: string | null
  readonly status: RuntimeActivityProjection['status']
  readonly outcome: RuntimeActivityProjection['outcome']
  readonly exitCode: number | null
  /** Original runtime position; snippets cite this instead of copying output. */
  readonly sourceRef: string
}

export interface ReviewActivitySegment {
  readonly key: string
  readonly productSessionId: string
  readonly workRunId: string | null
  readonly sessionBindingId: string
  readonly attempt: number
  /** The projection stops at 100 activities per session; the cut is shown. */
  readonly truncated: boolean
  readonly activities: readonly ReviewActivity[]
}

export interface ReviewEvidenceEntry {
  readonly evidence: DeliveryEvidenceProjection
  readonly outcome: EvidenceOutcome | null
  /** Server-owned artifact access; `unavailable` renders the fail-closed note. */
  readonly artifactState: 'available' | 'unavailable' | null
  readonly artifactReason: string | null
  readonly artifacts: readonly ReviewEvidenceArtifact[]
  readonly detailError: string | null
}

export type ReviewArtifactClass = 'inline-text' | 'executable-document' | 'download-only'

export interface ReviewArtifactChunk {
  readonly offset: number
  readonly returnedBytes: number
  readonly dataBase64: string
  readonly text: string | null
}

export interface ReviewEvidenceArtifact {
  readonly descriptor: EvidenceArtifactDescriptorProjection
  readonly previewClass: ReviewArtifactClass
  readonly chunks: readonly ReviewArtifactChunk[]
  readonly returnedBytes: number
  readonly totalBytes: number
  readonly nextOffset: number | null
  readonly error: string | null
}

export interface ReviewArtifactDownload {
  readonly fileName: string
  readonly mediaType: string
  readonly bytes: Uint8Array
}

export interface ReviewHistoryEntry {
  readonly candidateRef: string
  readonly candidateTreeId: string
  readonly diffSha256: string
  readonly availability: CandidateAvailability
  readonly isCurrentAtReadCursor: boolean
  readonly firstSeenDeliveryRevision: number
  readonly lastSeenDeliveryRevision: number
  readonly reviewDeliveryRevision: number | null
  readonly review: CandidateHistoricalReviewProjection | null
  readonly reviewError: string | null
}

export interface ReviewWorkingPlanProgress {
  readonly completed: number
  readonly inProgress: number
  readonly pending: number
  readonly total: number
  readonly sourceRef: string
}

export interface ReviewAcceptedCriteriaProgress {
  readonly accepted: number
  readonly failed: number
  readonly inconclusive: number
  readonly infraError: number
  readonly pending: number
  readonly total: number
}

export interface ReviewProgress {
  readonly workingPlan: ReviewWorkingPlanProgress | null
  readonly acceptedCriteria: ReviewAcceptedCriteriaProgress
}

/**
 * RUN-08 one redacted, traceable error citation. The projection never carries
 * copied log bodies: traceability comes from the original run identity plus the
 * runtime sourceRef.
 */
export interface ReviewSnippet {
  readonly sourceRef: string
  readonly workRunId: string | null
  readonly sessionBindingId: string
  readonly productSessionId: string
  readonly command: string | null
  readonly exitCode: number | null
  readonly outcome: ReviewActivity['outcome']
}

export interface StrongFlowReviewState {
  readonly status: StrongFlowReviewStatus
  readonly detail: DeliveryDetailProjection | null
  readonly files: readonly ReviewFileEntry[]
  readonly filesTruncated: boolean
  readonly segments: readonly ReviewActivitySegment[]
  readonly evidence: readonly ReviewEvidenceEntry[]
  readonly history: readonly ReviewHistoryEntry[]
  readonly progress: ReviewProgress
  readonly error: ControlPlaneClientError | null
}

export type StrongFlowReviewListener = (state: StrongFlowReviewState) => void

export interface StrongFlowReviewViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly deliveryId: string
  readonly nextRequestId: () => RequestId
}

export interface StrongFlowReviewViewModel {
  readonly state: StrongFlowReviewState
  subscribe(listener: StrongFlowReviewListener): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  continueFiles(): Promise<void>
  openPreview(path: string): Promise<void>
  continuePreview(path: string): Promise<void>
  /** Accumulated preview text for one file; null for degraded previews. */
  previewText(path: string): string | null
  openEvidenceDetail(evidenceId: string): Promise<void>
  openArtifact(evidenceId: string, artifactId: string): Promise<void>
  continueArtifact(evidenceId: string, artifactId: string): Promise<void>
  artifactDownload(evidenceId: string, artifactId: string): ReviewArtifactDownload | null
  openHistoricalReview(candidateRef: string): Promise<void>
  decideSolutionReview(
    action: SolutionReviewAction,
    note: string,
  ): Promise<'accepted' | 'completed'>
  snippetFor(segmentKey: string, callId: string): ReviewSnippet | null
  close(): void
}

export type SolutionReviewAction = 'approve' | 'request_changes' | 'reject'

/** Exact canonical payload sealed and rechecked by the Controller. */
export function solutionReviewDecisionResolution(
  review: SolutionReviewProjection,
  action: SolutionReviewAction,
  note: string,
): string {
  const normalized = note.trim()
  if (normalized === '') {
    throw reviewFailure('SOLUTION_REVIEW_NOTE_REQUIRED', 'A review note is required.')
  }
  const requestedChanges = action === 'request_changes'
    ? normalized.split(/\r?\n/u).map(item => item.trim()).filter(Boolean)
    : null
  if (action === 'request_changes' && requestedChanges?.length === 0) {
    throw reviewFailure('SOLUTION_REVIEW_CHANGES_REQUIRED', 'At least one requested change is required.')
  }
  return JSON.stringify({
    schemaVersion: 1,
    protocol: 'winwincode.solution-review-decision.v1',
    deliveryId: review.deliveryId,
    deliverySpecId: review.deliverySpecId,
    deliverySpecRevision: review.deliverySpecRevision,
    attentionItemId: review.attentionItemId,
    reviewSetSha256: review.reviewSetSha256,
    action,
    comments: action === 'request_changes' ? null : normalized,
    requestedChanges,
  })
}

function emptyState(): StrongFlowReviewState {
  return Object.freeze({
    status: 'idle' as const,
    detail: null,
    files: Object.freeze([]) as readonly ReviewFileEntry[],
    filesTruncated: false,
    segments: Object.freeze([]) as readonly ReviewActivitySegment[],
    evidence: Object.freeze([]) as readonly ReviewEvidenceEntry[],
    history: Object.freeze([]) as readonly ReviewHistoryEntry[],
    progress: Object.freeze({
      workingPlan: null,
      acceptedCriteria: Object.freeze({
        accepted: 0,
        failed: 0,
        inconclusive: 0,
        infraError: 0,
        pending: 0,
        total: 0,
      }),
    }),
    error: null,
  })
}

/**
 * Separate advisory model-plan progress from Controller-owned acceptance facts.
 * Counts stay exact; this projection never invents a percentage.
 */
export function reviewProgress(
  detail: DeliveryDetailProjection,
  snapshots: readonly RuntimeProjectionSnapshot[],
): ReviewProgress {
  const latestPlan = snapshots
    .flatMap(snapshot => snapshot.sessions)
    .filter(session => session.plan !== null)
    .sort((left, right) => right.attempt - left.attempt || right.asOfSequence - left.asOfSequence)[0]
    ?.plan ?? null
  const workingPlan = latestPlan === null
    ? null
    : Object.freeze({
        completed: latestPlan.items.filter(item => item.status === 'completed').length,
        inProgress: latestPlan.items.filter(item => item.status === 'in_progress').length,
        pending: latestPlan.items.filter(item => item.status === 'pending').length,
        total: latestPlan.items.length,
        sourceRef: latestPlan.sourceRef,
      })

  const criteria = detail.requirements.acceptanceCriteria ?? []
  const results = new Map((detail.verdict?.criteria ?? []).map(result => [result.criterionId, result]))
  const verdicts = criteria.flatMap(criterion => {
    const result = results.get(criterion.id)
    return result === undefined ? [] : [result.verdict]
  })
  return Object.freeze({
    workingPlan,
    acceptedCriteria: Object.freeze({
      accepted: verdicts.filter(verdict => verdict === 'pass').length,
      failed: verdicts.filter(verdict => verdict === 'fail').length,
      inconclusive: verdicts.filter(verdict => verdict === 'inconclusive').length,
      infraError: verdicts.filter(verdict => verdict === 'infra_error').length,
      pending: criteria.length - verdicts.length,
      total: criteria.length,
    }),
  })
}

function reviewFailure(code: string, message: string): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code,
    message,
    requestId: null,
    retryable: false,
  })
}

/** RUN-07: which previews may render, and why everything else degrades. */
export function classifyPreview(file: {
  readonly path: string
  readonly binary: boolean
  readonly encoding: CandidateFileEncoding
}): { previewClass: ReviewPreviewClass; degradationReason: string | null } {
  const lowered = file.path.toLowerCase()
  if (EXECUTABLE_EXTENSIONS.some(extension => lowered.endsWith(extension))) {
    return {
      previewClass: 'executable-document',
      degradationReason: '可执行文档（HTML/SVG）不与管理界面同源渲染，仅保留元数据。',
    }
  }
  if (file.binary || file.encoding === 'binary') {
    return { previewClass: 'binary', degradationReason: '二进制内容不内联渲染，仅保留元数据。' }
  }
  if (file.encoding === 'unknown-8bit') {
    return {
      previewClass: 'unknown-encoding',
      degradationReason: '编码未知，文本预览已收敛，仅保留元数据。',
    }
  }
  return { previewClass: 'text', degradationReason: null }
}

function decodeBase64Utf8(value: string): string {
  const binary = atob(value)
  const bytes = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index)
  return new TextDecoder('utf-8', { fatal: false }).decode(bytes)
}

function decodeBase64(value: string): Uint8Array {
  const binary = atob(value)
  const bytes = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index)
  return bytes
}

function artifactClass(descriptor: EvidenceArtifactDescriptorProjection): ReviewArtifactClass {
  const mediaType = descriptor.mediaType.toLowerCase().split(';', 1)[0]
  const fileName = descriptor.fileName?.toLowerCase() ?? ''
  if (mediaType === 'text/html' || mediaType === 'image/svg+xml'
    || /\.(?:html?|xhtml|xht|svg)$/u.test(fileName)) return 'executable-document'
  return descriptor.previewMode === 'inline_text' ? 'inline-text' : 'download-only'
}

function evidenceBinding(
  detail: DeliveryDetailProjection,
  evidence: DeliveryEvidenceProjection,
) {
  return {
    atCursor: detail.readCursor,
    candidateRef: evidence.candidateRef,
    deliveryId: detail.deliveryId,
    evidenceId: evidence.id,
    readPageLimit: READ_PAGE_LIMIT,
    sessionBindingId: evidence.sessionBindingId,
    sourceRef: evidence.sourceRef,
    type: evidence.type,
    workRunId: evidence.workRunId,
  }
}

function chunkPreviewState(
  file: Pick<ReviewFileEntry, 'path' | 'binary' | 'encoding'>,
  chunk: CandidateDiffChunkProjection,
): ReviewPreviewState {
  const classification = classifyPreview(file)
  const isText = classification.previewClass === 'text'
  return Object.freeze({
    path: file.path,
    previewClass: classification.previewClass,
    degradationReason: classification.degradationReason,
    chunks: Object.freeze(isText
      ? [Object.freeze({
          offset: chunk.offset,
          returnedBytes: chunk.returnedBytes,
          text: decodeBase64Utf8(chunk.dataBase64),
        })]
      : []),
    totalBytes: chunk.totalBytes,
    returnedBytes: chunk.returnedBytes,
    nextOffset: chunk.nextOffset,
  })
}

type DeliveryGetResponse = Extract<QueryResultResponse, { query: 'delivery.get' }>
type CandidateListResponse = Extract<QueryResultResponse, { query: 'candidate.list' }>
type CandidateFilesListResponse = Extract<QueryResultResponse, { query: 'candidate.files.list' }>
type CandidateDiffGetResponse = Extract<QueryResultResponse, { query: 'candidate.diff.get' }>
type RuntimeProjectionGetResponse = Extract<QueryResultResponse, { query: 'runtime.projection.get' }>
type WorkRunGetResponse = Extract<QueryResultResponse, { query: 'workrun.get' }>
type EvidenceGetResponse = Extract<QueryResultResponse, { query: 'evidence.get' }>
type EvidenceArtifactContentGetResponse = Extract<
  QueryResultResponse,
  { query: 'evidence.artifact.content.get' }
>
type CandidateReviewGetResponse = Extract<QueryResultResponse, { query: 'candidate.review.get' }>

/** StrongFlow review data and its one Controller-owned solution-review decision. */
export function createStrongFlowReviewViewModel(
  options: StrongFlowReviewViewModelOptions,
): StrongFlowReviewViewModel {
  const listeners = new Set<StrongFlowReviewListener>()
  const controllers = new Set<AbortController>()
  let currentState = emptyState()
  let nextFileCursor: string | null = null
  let closed = false

  function publish(state: StrongFlowReviewState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<StrongFlowReviewState>): void {
    publish({ ...currentState, ...update })
  }

  function requireOpen(): void {
    if (closed) throw reviewFailure('STRONGFLOW_REVIEW_CLOSED', 'The review panel is closed.')
  }

  function abortRequests(): void {
    for (const controller of controllers) controller.abort()
    controllers.clear()
  }

  function requestBase() {
    return {
      schemaVersion: SCHEMA_VERSION,
      actor: options.actor,
      scope: options.scope,
    }
  }

  function page(cursor: unknown) {
    return { cursor: (cursor as never) ?? null, limit: LIST_PAGE_LIMIT }
  }

  async function read<T extends QueryResultResponse>(
    build: () => QueryRequest,
    query: QueryRequest['query'],
  ): Promise<T> {
    const controller = new AbortController()
    controllers.add(controller)
    try {
      const response = await options.client.query(build(), { signal: controller.signal })
      if (response.query !== query) {
        throw reviewFailure(
          'STRONGFLOW_REVIEW_QUERY_MISMATCH',
          'The Control Plane returned another StrongFlow review query result.',
        )
      }
      return response as T
    } finally {
      controllers.delete(controller)
    }
  }

  function readDeliveryDetail(): Promise<DeliveryDetailProjection> {
    return read<DeliveryGetResponse>(() => ({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.DeliveryGet,
      parameters: { deliveryId: options.deliveryId as never },
      page: page(null),
    }) as QueryRequest, QueryName.DeliveryGet).then(response => response.result)
  }

  function readCandidateHistory(
    atCursor: StrongFlowReadCursor,
  ): Promise<readonly CandidateHistoryItemProjection[]> {
    const items: CandidateHistoryItemProjection[] = []
    let cursor: unknown = null
    let pageCount = 0
    const readNextPage = async (): Promise<readonly CandidateHistoryItemProjection[]> => {
      pageCount += 1
      const response = await read<CandidateListResponse>(() => ({
        ...requestBase(),
        requestId: options.nextRequestId(),
        query: QueryName.CandidateList,
        parameters: {
          atCursor,
          deliveryId: options.deliveryId as never,
          readPageLimit: READ_PAGE_LIMIT,
        },
        page: page(cursor),
      }) as QueryRequest, QueryName.CandidateList)
      items.push(...response.result.items)
      if (!response.page.hasMore || response.page.nextCursor === null) return items
      if (pageCount >= MAX_FILE_PAGES) {
        throw reviewFailure(
          'STRONGFLOW_REVIEW_PAGE_LIMIT_EXCEEDED',
          'The Candidate history read exceeded its bounded page limit.',
        )
      }
      cursor = response.page.nextCursor
      return readNextPage()
    }
    return readNextPage()
  }

  function readFilePage(
    atCursor: StrongFlowReadCursor,
    candidate: { candidateRef: string; candidateTreeId: string; diffSha256: string },
    cursor: unknown,
  ): Promise<{ page: CandidateFilePage; nextCursor: string | null }> {
    return read<CandidateFilesListResponse>(() => ({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.CandidateFilesList,
      parameters: {
        atCursor,
        candidateRef: candidate.candidateRef,
        candidateTreeId: candidate.candidateTreeId,
        deliveryId: options.deliveryId as never,
        diffSha256: candidate.diffSha256 as never,
        pathPrefix: null,
        readPageLimit: READ_PAGE_LIMIT,
        statuses: [],
      },
      page: page(cursor),
    }) as QueryRequest, QueryName.CandidateFilesList)
      .then(response => ({
        page: response.result,
        nextCursor: response.page.hasMore ? response.page.nextCursor : null,
      }))
  }

  function readDiffChunk(
    atCursor: StrongFlowReadCursor,
    candidate: { candidateRef: string; candidateTreeId: string; diffSha256: string },
    path: string,
    offset: number,
  ): Promise<CandidateDiffChunkProjection> {
    return read<CandidateDiffGetResponse>(() => ({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.CandidateDiffGet,
      parameters: {
        atCursor,
        candidateRef: candidate.candidateRef,
        candidateTreeId: candidate.candidateTreeId,
        deliveryId: options.deliveryId as never,
        diffSha256: candidate.diffSha256 as never,
        path,
        offset,
        length: DIFF_CHUNK_BYTES,
        readPageLimit: READ_PAGE_LIMIT,
      },
      page: page(null),
    }) as QueryRequest, QueryName.CandidateDiffGet).then(response => response.result)
  }

  function readWorkRun(
    atCursor: StrongFlowReadCursor,
    workRunId: string,
  ): Promise<WorkRunGetResponse['result']> {
    return read<WorkRunGetResponse>(() => ({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.WorkRunGet,
      parameters: {
        deliveryId: options.deliveryId as never,
        workItemId: null,
        workRunId: workRunId as never,
        atCursor,
      },
      page: page(null),
    }) as QueryRequest, QueryName.WorkRunGet).then(response => response.result)
  }

  function readRuntimeWorkRun(
    atCursor: StrongFlowReadCursor,
    stage: { workRunId: string; productSessionId: string },
  ): Promise<RuntimeProjectionSnapshot> {
    return read<RuntimeProjectionGetResponse>(() => ({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.RuntimeProjectionGet,
      parameters: {
        kind: 'work-run',
        deliveryId: options.deliveryId as never,
        workRunId: stage.workRunId as never,
        productSessionId: stage.productSessionId as never,
        atCursor,
      },
      page: page(null),
    }) as QueryRequest, QueryName.RuntimeProjectionGet).then(response => response.result)
  }

  function readEvidenceDetail(
    detail: DeliveryDetailProjection,
    evidence: DeliveryEvidenceProjection,
  ): Promise<EvidenceGetResponse['result']> {
    return read<EvidenceGetResponse>(() => ({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.EvidenceGet,
      parameters: evidenceBinding(detail, evidence),
      page: page(null),
    }) as QueryRequest, QueryName.EvidenceGet).then(response => response.result)
  }

  function readArtifactChunk(
    detail: DeliveryDetailProjection,
    evidence: DeliveryEvidenceProjection,
    descriptor: EvidenceArtifactDescriptorProjection,
    offset: number,
  ): Promise<EvidenceArtifactContentGetResponse['result']> {
    return read<EvidenceArtifactContentGetResponse>(() => ({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.EvidenceArtifactContentGet,
      parameters: {
        evidence: evidenceBinding(detail, evidence),
        artifactId: descriptor.artifactId,
        artifactKind: descriptor.kind,
        artifactDigest: descriptor.digest,
        artifactMediaType: descriptor.mediaType,
        artifactSizeBytes: descriptor.sizeBytes,
        offset,
        length: DIFF_CHUNK_BYTES,
      },
      page: page(null),
    }) as QueryRequest, QueryName.EvidenceArtifactContentGet).then(response => response.result)
  }

  function readHistoricalReview(
    atCursor: StrongFlowReadCursor,
    entry: { candidateRef: string; candidateTreeId: string; diffSha256: string },
  ): Promise<CandidateHistoricalReviewProjection> {
    return read<CandidateReviewGetResponse>(() => ({
      ...requestBase(),
      requestId: options.nextRequestId(),
      query: QueryName.CandidateReviewGet,
      parameters: {
        atCursor,
        candidateRef: entry.candidateRef,
        candidateTreeId: entry.candidateTreeId,
        deliveryId: options.deliveryId as never,
        diffSha256: entry.diffSha256 as never,
        readPageLimit: READ_PAGE_LIMIT,
      },
      page: page(null),
    }) as QueryRequest, QueryName.CandidateReviewGet).then(response => response.result)
  }

  function fileEntries(files: readonly CandidateFileProjection[]): readonly ReviewFileEntry[] {
    return Object.freeze(files.map(file => Object.freeze({
      path: file.path,
      oldPath: file.oldPath,
      status: file.status,
      encoding: file.encoding,
      binary: file.binary,
      additions: file.additions,
      deletions: file.deletions,
      preview: null,
      previewError: null,
    })))
  }

  function activitySegments(
    snapshot: RuntimeProjectionSnapshot,
  ): readonly ReviewActivitySegment[] {
    return Object.freeze(snapshot.sessions.slice(0, REVIEW_SESSION_LIMIT).map(session => {
      const key = `${session.productSessionId}:${session.sessionBindingId}`
      return Object.freeze({
        key,
        productSessionId: session.productSessionId,
        workRunId: session.workRunId,
        sessionBindingId: session.sessionBindingId,
        attempt: session.attempt,
        truncated: session.activities.length >= MAX_ACTIVITIES_PER_SESSION,
        activities: Object.freeze(session.activities.slice(0, MAX_ACTIVITIES_PER_SESSION).map(activity => Object.freeze({
          callId: activity.callId,
          activityType: activity.activityType,
          command: activity.command,
          status: activity.status,
          outcome: activity.outcome,
          exitCode: activity.exitCode,
          sourceRef: activity.sourceRef,
        }))),
      })
    }))
  }

  function statusFromError(error: ControlPlaneClientError): StrongFlowReviewStatus {
    if (error.kind === 'authentication') return 'authentication-required'
    if (error.kind === 'authorization') return 'authorization-denied'
    return 'error'
  }

  async function load(kind: 'loading' | 'refreshing'): Promise<void> {
    requireOpen()
    abortRequests()
    patch({ status: kind, error: null })
    try {
      const detail = await readDeliveryDetail()
      const atCursor = detail.readCursor
      const candidate = detail.currentCandidate
      const [history, fileResult] = await Promise.all([
        readCandidateHistory(atCursor),
        candidate === null
          ? Promise.resolve(null)
          : readFilePage(atCursor, candidate, null),
      ])
      const files = fileResult === null
        ? Object.freeze([]) as readonly ReviewFileEntry[]
        : fileEntries(fileResult.page.items)
      nextFileCursor = fileResult?.nextCursor ?? null
      const filesTruncated = nextFileCursor !== null

      const workRunIds = [...new Set([
        ...(candidate === null ? [] : [candidate.producerWorkRunId]),
        ...detail.evidence.map(entry => entry.workRunId),
      ])].slice(0, REVIEW_SESSION_LIMIT)
      const aggregates = await Promise.all(
        workRunIds.map(workRunId => readWorkRun(atCursor, workRunId).catch(() => null)),
      )
      const runtimeTargets = aggregates.flatMap(aggregate => aggregate?.runs ?? [])
        .filter(run => run.productSessionId !== null && workRunIds.includes(run.id))
        .map(run => ({ workRunId: run.id, productSessionId: run.productSessionId as string }))
      const snapshots = await Promise.all(
        runtimeTargets.map(target => readRuntimeWorkRun(atCursor, target).catch(() => null)),
      )
      const segments = snapshots.flatMap(snapshot => (
        snapshot === null ? [] : activitySegments(snapshot)
      ))

      patch({
        status: 'ready',
        detail,
        files,
        filesTruncated,
        segments,
        evidence: Object.freeze(detail.evidence.map(entry => Object.freeze({
          evidence: entry,
          outcome: null,
          artifactState: null,
          artifactReason: null,
          artifacts: Object.freeze([]),
          detailError: null,
        }))),
        history: Object.freeze(history.map(item => Object.freeze({
          candidateRef: item.candidate.candidateRef,
          candidateTreeId: item.candidate.candidateTreeId,
          diffSha256: item.candidate.diffSha256,
          availability: item.availability,
          isCurrentAtReadCursor: item.isCurrentAtReadCursor,
          firstSeenDeliveryRevision: item.firstSeenDeliveryRevision,
          lastSeenDeliveryRevision: item.lastSeenDeliveryRevision,
          reviewDeliveryRevision: item.reviewDeliveryRevision,
          review: null,
          reviewError: null,
        }))),
        progress: reviewProgress(detail, snapshots.flatMap(snapshot => (
          snapshot === null ? [] : [snapshot]
        ))),
        error: null,
      })
    } catch (error) {
      if (error instanceof ControlPlaneClientError && error.kind === 'cancelled') return
      if (error instanceof ControlPlaneClientError) {
        patch({ status: statusFromError(error), error })
        return
      }
      patch({
        status: 'error',
        error: reviewFailure(
          'STRONGFLOW_REVIEW_READ_FAILED',
          error instanceof Error ? error.message : 'The StrongFlow review read failed.',
        ),
      })
    }
  }

  function updateFile(path: string, update: Partial<ReviewFileEntry>): void {
    patch({
      files: Object.freeze(currentState.files.map(file => (
        file.path === path ? Object.freeze({ ...file, ...update }) : file
      ))),
    })
  }

  function updateArtifact(
    evidenceId: string,
    artifactId: string,
    update: (artifact: ReviewEvidenceArtifact) => ReviewEvidenceArtifact,
  ): void {
    patch({
      evidence: Object.freeze(currentState.evidence.map(entry => (
        entry.evidence.id !== evidenceId ? entry : Object.freeze({
          ...entry,
          artifacts: Object.freeze(entry.artifacts.map(artifact => (
            artifact.descriptor.artifactId === artifactId ? update(artifact) : artifact
          ))),
        })
      ))),
    })
  }

  function candidateIdentity() {
    return currentState.detail?.currentCandidate ?? null
  }

  async function loadPreviewChunk(path: string, offset: number): Promise<void> {
    const file = currentState.files.find(entry => entry.path === path)
    const candidate = candidateIdentity()
    const detail = currentState.detail
    if (file === undefined || candidate === null || detail === null) {
      updateFile(path, {
        previewError: '当前没有可读取的 Candidate，或读取游标尚未就绪。',
      })
      return
    }
    try {
      const chunk = await readDiffChunk(detail.readCursor, candidate, path, offset)
      const addition = chunkPreviewState(file, chunk)
      if (chunk.path !== path || chunk.offset !== offset) {
        throw reviewFailure('STRONGFLOW_REVIEW_CHUNK_MISMATCH', 'The Candidate diff chunk does not match the requested range.')
      }
      const merged: ReviewPreviewState = file.preview === null
        ? addition
        : Object.freeze({
            ...file.preview,
            chunks: Object.freeze([...file.preview.chunks, ...addition.chunks]),
            returnedBytes: file.preview.returnedBytes + chunk.returnedBytes,
            nextOffset: chunk.nextOffset,
          })
      updateFile(path, { preview: merged, previewError: null })
    } catch (error) {
      const message = error instanceof Error ? error.message : 'preview read failed'
      updateFile(path, { previewError: message })
    }
  }

  async function loadArtifactChunk(
    evidenceId: string,
    artifactId: string,
    offset: number,
  ): Promise<void> {
    const detail = currentState.detail
    const evidenceEntry = currentState.evidence.find(entry => entry.evidence.id === evidenceId)
    const artifact = evidenceEntry?.artifacts.find(
      entry => entry.descriptor.artifactId === artifactId,
    )
    if (detail === null || evidenceEntry === undefined || artifact === undefined) return
    try {
      const result = await readArtifactChunk(
        detail,
        evidenceEntry.evidence,
        artifact.descriptor,
        offset,
      )
      if (result.state === 'unavailable') {
        updateArtifact(evidenceId, artifactId, current => Object.freeze({
          ...current,
          nextOffset: null,
          error: '该证据与产物的精确授权链接现已不可用。',
        }))
        return
      }
      if (result.artifact.artifactId !== artifactId
        || result.artifact.digest !== artifact.descriptor.digest
        || result.evidence.id !== evidenceId
        || result.offset !== offset
        || result.totalBytes !== artifact.descriptor.sizeBytes) {
        throw reviewFailure(
          'STRONGFLOW_REVIEW_ARTIFACT_MISMATCH',
          'The Evidence artifact chunk does not match the authorized selector.',
        )
      }
      const chunk = Object.freeze({
        offset: result.offset,
        returnedBytes: result.returnedBytes,
        dataBase64: result.dataBase64,
        text: artifact.previewClass === 'inline-text'
          && result.previewMode === 'inline_text'
          && result.contentEncoding === 'utf-8'
          ? decodeBase64Utf8(result.dataBase64)
          : null,
      })
      updateArtifact(evidenceId, artifactId, current => Object.freeze({
        ...current,
        chunks: Object.freeze(offset === 0 ? [chunk] : [...current.chunks, chunk]),
        returnedBytes: offset === 0
          ? result.returnedBytes
          : current.returnedBytes + result.returnedBytes,
        nextOffset: result.nextOffset,
        error: null,
      }))
    } catch (error) {
      updateArtifact(evidenceId, artifactId, current => Object.freeze({
        ...current,
        error: error instanceof Error ? error.message : 'artifact read failed',
      }))
    }
  }

  const viewModel: StrongFlowReviewViewModel = {
    get state(): StrongFlowReviewState {
      return currentState
    },

    subscribe(listener: StrongFlowReviewListener): () => void {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    },

    async start(): Promise<void> {
      await load('loading')
    },

    async refresh(): Promise<void> {
      await load('refreshing')
    },

    async continueFiles(): Promise<void> {
      const detail = currentState.detail
      const candidate = candidateIdentity()
      const cursor = nextFileCursor
      if (detail === null || candidate === null || cursor === null) return
      try {
        const result = await readFilePage(detail.readCursor, candidate, cursor)
        const known = new Set(currentState.files.map(file => file.path))
        const additions = fileEntries(result.page.items).filter(file => !known.has(file.path))
        nextFileCursor = result.nextCursor
        patch({
          files: Object.freeze([...currentState.files, ...additions]),
          filesTruncated: nextFileCursor !== null,
        })
      } catch (error) {
        patch({
          error: reviewFailure(
            'STRONGFLOW_REVIEW_FILE_PAGE_FAILED',
            error instanceof Error ? error.message : 'The next Candidate file page failed.',
          ),
        })
      }
    },

    async openPreview(path: string): Promise<void> {
      await loadPreviewChunk(path, 0)
    },

    async continuePreview(path: string): Promise<void> {
      const file = currentState.files.find(entry => entry.path === path)
      const nextOffset = file?.preview?.nextOffset
      if (file === undefined || nextOffset === null || nextOffset === undefined) return
      await loadPreviewChunk(path, nextOffset)
    },

    previewText(path: string): string | null {
      const preview = currentState.files.find(entry => entry.path === path)?.preview
      if (preview === null || preview === undefined || preview.previewClass !== 'text') return null
      return preview.chunks.map(chunk => chunk.text).join('')
    },

    async openEvidenceDetail(evidenceId: string): Promise<void> {
      const detail = currentState.detail
      const entry = currentState.evidence.find(item => item.evidence.id === evidenceId)
      if (detail === null || entry === undefined) return
      try {
        const result = await readEvidenceDetail(
          detail,
          entry.evidence,
        )
        const access = result.artifactAccess
        patch({
          evidence: Object.freeze(currentState.evidence.map(item => (
            item.evidence.id !== evidenceId ? item : Object.freeze({
              ...item,
              outcome: result.outcome,
              artifactState: access.state,
              artifactReason: access.state === 'unavailable' ? access.reason : null,
              artifacts: access.state === 'available'
                ? Object.freeze(access.items.map(descriptor => Object.freeze({
                    descriptor,
                    previewClass: artifactClass(descriptor),
                    chunks: Object.freeze([]),
                    returnedBytes: 0,
                    totalBytes: descriptor.sizeBytes,
                    nextOffset: 0,
                    error: null,
                  })))
                : Object.freeze([]),
              detailError: null,
            })
          ))),
        })
      } catch (error) {
        const message = error instanceof Error ? error.message : 'evidence read failed'
        patch({
          evidence: Object.freeze(currentState.evidence.map(item => (
            item.evidence.id !== evidenceId
              ? item
              : Object.freeze({ ...item, detailError: message })
          ))),
        })
      }
    },

    async openArtifact(evidenceId: string, artifactId: string): Promise<void> {
      await loadArtifactChunk(evidenceId, artifactId, 0)
    },

    async continueArtifact(evidenceId: string, artifactId: string): Promise<void> {
      const artifact = currentState.evidence.find(item => item.evidence.id === evidenceId)
        ?.artifacts.find(item => item.descriptor.artifactId === artifactId)
      if (artifact?.nextOffset === null || artifact?.nextOffset === undefined) return
      await loadArtifactChunk(evidenceId, artifactId, artifact.nextOffset)
    },

    artifactDownload(evidenceId: string, artifactId: string): ReviewArtifactDownload | null {
      const artifact = currentState.evidence.find(item => item.evidence.id === evidenceId)
        ?.artifacts.find(item => item.descriptor.artifactId === artifactId)
      if (artifact === undefined || artifact.nextOffset !== null || artifact.error !== null) return null
      const decoded = artifact.chunks.map(chunk => decodeBase64(chunk.dataBase64))
      const length = decoded.reduce((total, chunk) => total + chunk.byteLength, 0)
      const bytes = new Uint8Array(length)
      let offset = 0
      for (const chunk of decoded) {
        bytes.set(chunk, offset)
        offset += chunk.byteLength
      }
      return Object.freeze({
        fileName: artifact.descriptor.fileName ?? `${artifact.descriptor.artifactId}.bin`,
        mediaType: artifact.descriptor.mediaType,
        bytes,
      })
    },

    async openHistoricalReview(candidateRef: string): Promise<void> {
      const detail = currentState.detail
      const entry = currentState.history.find(item => item.candidateRef === candidateRef)
      if (detail === null || entry === undefined) return
      try {
        const review = await readHistoricalReview(detail.readCursor, entry)
        patch({
          history: Object.freeze(currentState.history.map(item => (
            item.candidateRef !== candidateRef ? item : Object.freeze({
              ...item,
              review,
              reviewError: null,
            })
          ))),
        })
      } catch (error) {
        const message = error instanceof Error ? error.message : 'historical review read failed'
        patch({
          history: Object.freeze(currentState.history.map(item => (
            item.candidateRef !== candidateRef
              ? item
              : Object.freeze({ ...item, reviewError: message })
          ))),
        })
      }
    },

    async decideSolutionReview(
      action: SolutionReviewAction,
      note: string,
    ): Promise<'accepted' | 'completed'> {
      requireOpen()
      const detail = currentState.detail
      const review = detail?.solutionReview
      if (detail === null || detail === undefined || review === null || review === undefined
        || review.reviewStatus !== 'pending'
        || !detail.attention.some(item => item.id === review.attentionItemId && item.status === 'open')) {
        throw reviewFailure(
          'SOLUTION_REVIEW_STALE',
          'The solution review is no longer the current open Attention item.',
        )
      }
      const controller = new AbortController()
      controllers.add(controller)
      try {
        const request = {
          ...requestBase(),
          requestId: options.nextRequestId(),
          command: CommandName.DeliveryResolveAttention,
          expectedRevision: detail.deliveryRevision,
          payload: {
            attentionItemId: review.attentionItemId,
            decision: action === 'approve' ? 'resolve' : 'dismiss',
            deliveryId: review.deliveryId,
            remediation: null,
            resolution: solutionReviewDecisionResolution(review, action, note),
          },
        } as CommandRequest
        const response = await options.client.command(request, { signal: controller.signal })
        if (response.outcome === 'completed' && response.command !== CommandName.DeliveryResolveAttention) {
          throw reviewFailure(
            'STRONGFLOW_REVIEW_COMMAND_MISMATCH',
            'The Control Plane returned another command result.',
          )
        }
        if (response.outcome === 'completed') await load('refreshing')
        return response.outcome
      } finally {
        controllers.delete(controller)
      }
    },

    snippetFor(segmentKey: string, callId: string): ReviewSnippet | null {
      const segment = currentState.segments.find(item => item.key === segmentKey)
      const activity = segment?.activities.find(item => item.callId === callId)
      if (segment === undefined || activity === undefined) return null
      return Object.freeze({
        sourceRef: activity.sourceRef,
        workRunId: segment.workRunId,
        sessionBindingId: segment.sessionBindingId,
        productSessionId: segment.productSessionId,
        command: activity.command,
        exitCode: activity.exitCode,
        outcome: activity.outcome,
      })
    },

    close(): void {
      if (closed) return
      closed = true
      nextFileCursor = null
      abortRequests()
      listeners.clear()
      publish({ ...currentState, status: 'closed' })
    },
  }

  return viewModel
}
