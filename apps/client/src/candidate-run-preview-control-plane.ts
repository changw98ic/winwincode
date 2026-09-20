// SPDX-License-Identifier: Apache-2.0

import {
  candidateBoundaryError,
  ControlPlaneClientError,
  parseControlPlaneServerUrl,
  type ControlPlaneClient,
  type ControlPlaneOccupancyStatus,
  type ControlPlaneRunIdentityProjection,
  type ControlPlaneTransportFetch,
} from './community-control-plane-client.js'
import type {
  AuthorizedPreviewSourceFacts,
  CandidatePreviewIdentity,
  CandidatePreviewPort,
  ManagedRunControl,
  PreviewDiffChunkFacts,
  PreviewFileContentFacts,
  PreviewFileFacts,
} from './candidate-run-preview-view-model.js'
import {
  classifyPreviewFile,
  isCandidatePreviewIdentity,
  isRepositoryRelativePath,
  isSafePreviewOrigin,
  PREVIEW_LOG_MAX_LINES,
} from './candidate-run-preview-view-model.js'
import type {
  Actor,
  CandidateFilePage,
  CandidateFileContentChunkProjection,
  CandidateDiffChunkProjection,
  DeliveryEvidenceProjection,
  EvidenceArtifactDescriptorProjection,
  EvidenceArtifactContentChunkProjection,
  DeliveryId,
  QueryRequest,
  ManagedAppCommand,
  ManagedAppRunControlProjection,
  RepositoryScope,
  RequestId,
  Sha256Digest,
  StrongFlowReadCursor,
} from './generated/contracts.js'
import { matchesCanonicalSchema } from './generated/control-plane-client.js'
import { redactPublicText } from './public-redaction.js'

const CREATE_PATH = '/api/v1/previews'
const FILE_PAGE_LIMIT = 100
const MAX_FILE_PAGES = 8
const DIFF_CHUNK_BYTES = 64 * 1024
const MAX_DIFF_BYTES = 256 * 1024
const FILE_CONTENT_CHUNK_BYTES = 64 * 1024
const MAX_FILE_CONTENT_BYTES = 256 * 1024 * 1024
const LOG_READ_BYTES = 64 * 1024
const LOG_ARTIFACT_KINDS = new Set(['log', 'test_output', 'command_output'])

/** The only context accepted by candidate.files.list. It is a canonical
 * Delivery read cut, never a URL or device-local candidate projection. */
export interface CandidatePreviewFileContext {
  readonly deliveryId: DeliveryId
  readonly atCursor: StrongFlowReadCursor
  readonly candidateRef: string
  readonly candidateTreeId: string
  readonly diffSha256: Sha256Digest
}

/** Projects file-query context only from the canonical run identity read. */
export function candidatePreviewFileContextFromIdentity(
  projection: ControlPlaneRunIdentityProjection,
): CandidatePreviewFileContext | null {
  const candidate = projection.candidate
  if (projection.deliveryId === null || projection.readCursor === null || candidate === null) return null
  if (!/^git-candidate:sha256:[0-9a-f]{64}$/u.test(candidate.candidateRef)
    || !/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u.test(candidate.candidateTreeId)
    || !/^sha256:[0-9a-f]{64}$/u.test(candidate.diffSha256)) return null
  return Object.freeze({
    deliveryId: projection.deliveryId,
    atCursor: projection.readCursor,
    candidateRef: candidate.candidateRef,
    candidateTreeId: candidate.candidateTreeId,
    diffSha256: candidate.diffSha256,
  })
}

function candidateFileFacts(file: CandidateFilePage['items'][number]): PreviewFileFacts {
  const projected = classifyPreviewFile(file.path)
  if (projected.previewClass === 'degraded') return projected
  if (file.status === 'deleted') {
    return Object.freeze({
      ...projected,
      previewClass: 'degraded',
      degradationReason: '该文件已在候选版本中删除，只能查看变更 diff。',
    })
  }
  if (file.encoding === 'unknown-8bit') {
    if (projected.previewClass === 'image' || projected.previewClass === 'download') return projected
    return Object.freeze({
      ...projected,
      previewClass: 'degraded',
      degradationReason: '文件编码无法安全预览，当前暂不可打开。',
    })
  }
  if (file.binary) {
    if (projected.previewClass === 'image' || projected.previewClass === 'download') return projected
    return Object.freeze({
      ...projected,
      previewClass: 'degraded',
      degradationReason: '当前没有文件内容读取接口，二进制文件暂不可打开。',
    })
  }
  if (projected.previewClass === 'download') {
    return projected
  }
  return projected
}

function sameReadCursor(left: StrongFlowReadCursor, right: StrongFlowReadCursor): boolean {
  const scopeKeys = ['kind', 'organizationId', 'workspaceId', 'projectId', 'repositoryId'] as const
  return left.token === right.token
    && left.deliveryId === right.deliveryId
    && left.deliveryRevision === right.deliveryRevision
    && left.runtimeLedgerRevision === right.runtimeLedgerRevision
    && left.runtimeAcceptedSequence === right.runtimeAcceptedSequence
    && left.publicationRevision === right.publicationRevision
    && scopeKeys.every(key => left.scope[key] === right.scope[key]
      && left.eventCursor.scope[key] === right.eventCursor.scope[key])
    && left.eventCursor.stream.kind === right.eventCursor.stream.kind
    && left.eventCursor.stream.deliveryId === right.eventCursor.stream.deliveryId
    && left.eventCursor.eventId === right.eventCursor.eventId
    && left.eventCursor.sequence === right.eventCursor.sequence
}

function validateCandidateFilePage(
  page: CandidateFilePage,
  context: CandidatePreviewFileContext,
): void {
  if (page.kind !== 'candidate_file_page'
    || page.candidate.candidateRef !== context.candidateRef
    || page.candidate.candidateTreeId !== context.candidateTreeId
    || page.candidate.diffSha256 !== context.diffSha256
    || !sameReadCursor(page.readCursor, context.atCursor)) {
    throw failure('protocol', 'STALE_CANDIDATE_FILE_PAGE', '候选版本或读取游标已变化，请重新打开任务。')
  }
}

function hasCandidateFileContext(
  context: CandidatePreviewFileContext | undefined,
): context is CandidatePreviewFileContext {
  return context !== undefined
    && /^dlv_[0-9A-HJKMNP-TV-Z]{26}$/u.test(context.deliveryId)
    && /^git-candidate:sha256:[0-9a-f]{64}$/u.test(context.candidateRef)
    && /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u.test(context.candidateTreeId)
    && /^sha256:[0-9a-f]{64}$/u.test(context.diffSha256)
    && context.atCursor.deliveryId === context.deliveryId
    && context.atCursor.eventCursor.stream.deliveryId === context.deliveryId
}

function isRecord(value: unknown): value is Readonly<Record<string, unknown>> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function failure(kind: 'network' | 'protocol' | 'server', code: string, message: string): ControlPlaneClientError {
  return new ControlPlaneClientError({ kind, code, message, requestId: null, retryable: kind !== 'protocol' })
}

function parseGrant(
  source: string,
  identity: CandidatePreviewIdentity,
): AuthorizedPreviewSourceFacts {
  let value: unknown
  try {
    value = JSON.parse(source)
  } catch {
    value = null
  }
  if (!isRecord(value) || value.schemaVersion !== 'winwincode/v1'
    || typeof value.previewAccessId !== 'string'
    || !/^pva_[0-9a-f]{32}$/u.test(value.previewAccessId)
    || typeof value.previewUrl !== 'string' || !isSafePreviewOrigin(value.previewUrl)
    || typeof value.expiresAt !== 'string' || !Number.isFinite(Date.parse(value.expiresAt))
    || !isRecord(value.source)
    || value.source.sourceId !== identity.sourceId
    || value.source.workRunId !== identity.workRunId
    || value.source.repositoryBindingId !== identity.repositoryBindingId
    || value.source.mode !== identity.mode
    || (identity.candidateCommit === null
      ? value.source.candidateCommit !== undefined
      : value.source.candidateCommit !== identity.candidateCommit)) {
    throw failure('protocol', 'INVALID_PREVIEW_ACCESS_RESPONSE', 'Server 返回了无效的预览授权。')
  }
  const previewUrl = new URL(value.previewUrl)
  if (previewUrl.search !== '' || previewUrl.hash !== ''
    || previewUrl.username !== '' || previewUrl.password !== ''
    || previewUrl.pathname !== `/p/${value.previewAccessId}/${previewUrl.pathname.split('/')[3] ?? ''}/`
    || !/^[0-9a-f]{64}$/u.test(previewUrl.pathname.split('/')[3] ?? '')) {
    throw failure('protocol', 'INVALID_PREVIEW_ACCESS_RESPONSE', 'Server 返回了无效的预览授权。')
  }
  return Object.freeze({
    previewAccessId: value.previewAccessId,
    sourceId: identity.sourceId,
    mode: identity.mode,
    candidateCommit: identity.candidateCommit,
    previewOrigin: value.previewUrl,
    access: 'authorized',
    accessExpiresAt: value.expiresAt,
  })
}

/** Browser facade for the existing short-lived Server preview access routes. */
export function createControlPlaneCandidatePreviewPort(options: {
  readonly serverUrl: string
  readonly fetch: ControlPlaneTransportFetch | undefined
  readonly identity: CandidatePreviewIdentity
  /** Canonical authenticated query seam used by the file-list adapter. */
  readonly client?: ControlPlaneClient
  readonly actor?: Actor
  readonly scope?: RepositoryScope
  readonly fileContext?: CandidatePreviewFileContext
  /** Current canonical run projection; no local log source is accepted. */
  readonly runIdentityProjection?: () => Promise<ControlPlaneRunIdentityProjection | null>
  /** Current holder lease used to fence managed-app commands. */
  readonly occupancyStatus?: (clientId: string) => Promise<ControlPlaneOccupancyStatus>
  /** Candidate run config selected by the canonical execution projection. */
  readonly managedAppRunConfig?: ManagedAppRunControlProjection
  readonly nextRequestId?: () => RequestId
}): CandidatePreviewPort {
  const location = parseControlPlaneServerUrl(options.serverUrl)
  const diffDecoders = new Map<string, {
    readonly decoder: TextDecoder
    readonly nextOffset: number
    readonly fileDiffSha256: string
  }>()
  const logDescriptors = new Map<string, {
    readonly evidence: DeliveryEvidenceProjection
    readonly descriptor: EvidenceArtifactDescriptorProjection
    readonly sourceRef: string
  }>()

  function evidenceBinding(projection: ControlPlaneRunIdentityProjection, evidence: DeliveryEvidenceProjection) {
    const sourceWorkRunId = projection.managedAppSourceRun?.id ?? projection.workRun.id
    if (projection.deliveryId === null || projection.readCursor === null
      || evidence.workRunId !== sourceWorkRunId
      || (projection.candidate !== null && projection.candidate !== undefined
        && evidence.candidateRef !== projection.candidate.candidateRef)) return null
    return {
      atCursor: projection.readCursor,
      candidateRef: evidence.candidateRef,
      deliveryId: projection.deliveryId,
      evidenceId: evidence.id,
      readPageLimit: 100,
      sessionBindingId: evidence.sessionBindingId,
      sourceRef: evidence.sourceRef,
      type: evidence.type,
      workRunId: evidence.workRunId,
    }
  }

  async function readLogArtifact(
    projection: ControlPlaneRunIdentityProjection,
    entry: { readonly evidence: DeliveryEvidenceProjection; readonly descriptor: EvidenceArtifactDescriptorProjection },
  ): Promise<string | null> {
    if (options.client === undefined || options.actor === undefined || options.scope === undefined
      || options.nextRequestId === undefined) return null
    const binding = evidenceBinding(projection, entry.evidence)
    const readCursor = projection.readCursor
    if (binding === null || readCursor === null) return null
    const response = await options.client.query({
      schemaVersion: 'winwincode/v1',
      requestId: options.nextRequestId(),
      actor: options.actor,
      scope: options.scope,
      query: 'evidence.artifact.content.get',
      parameters: {
        evidence: binding,
        artifactId: entry.descriptor.artifactId,
        artifactKind: entry.descriptor.kind,
        artifactDigest: entry.descriptor.digest,
        artifactMediaType: entry.descriptor.mediaType,
        artifactSizeBytes: entry.descriptor.sizeBytes,
        offset: 0,
        length: LOG_READ_BYTES,
      },
      page: { cursor: null, limit: 1 },
    } as QueryRequest)
    if (response.query !== 'evidence.artifact.content.get') return null
    if (!matchesCanonicalSchema('EvidenceArtifactContentResult', response.result)) return null
    const result = response.result as EvidenceArtifactContentChunkProjection | { readonly state: 'unavailable' }
    if (result.state !== 'available' || result.contentEncoding !== 'utf-8'
      || result.previewMode !== 'inline_text'
      || result.artifact.artifactId !== entry.descriptor.artifactId
      || result.artifact.digest !== entry.descriptor.digest
      || result.artifact.mediaType !== entry.descriptor.mediaType
      || result.artifact.sizeBytes !== entry.descriptor.sizeBytes
      || result.evidence.id !== entry.evidence.id
      || result.evidence.workRunId !== entry.evidence.workRunId
      || result.evidence.candidateRef !== entry.evidence.candidateRef
      || result.artifact.provenance.evidenceId !== entry.evidence.id
      || result.artifact.provenance.workRunId !== entry.evidence.workRunId
      || result.artifact.provenance.candidateRef !== entry.evidence.candidateRef
      || result.artifact.provenance.sessionBindingId !== entry.evidence.sessionBindingId
      || projection.deliveryId === null
      || result.artifact.provenance.deliveryId !== projection.deliveryId
      || !sameReadCursor(result.readCursor, readCursor)
      || result.offset !== 0
      || result.returnedBytes > LOG_READ_BYTES
      || result.totalBytes !== entry.descriptor.sizeBytes
      || result.returnedBytes > result.totalBytes
      || (result.nextOffset === null
        ? result.returnedBytes !== result.totalBytes
        : result.nextOffset !== result.returnedBytes
          || result.nextOffset > result.totalBytes)) {
      return null
    }
    try {
      const binary = atob(result.dataBase64)
      const bytes = Uint8Array.from(binary, character => character.charCodeAt(0))
      if (bytes.length !== result.returnedBytes) return null
      return new TextDecoder('utf-8', { fatal: true }).decode(bytes)
    } catch {
      return null
    }
  }

  async function listLogSegments(input: { readonly runId: string }): Promise<readonly import('./candidate-run-preview-view-model.js').PreviewLogSegmentFacts[]> {
    const projection = await options.runIdentityProjection?.()
    const previewRun = projection?.managedAppSourceRun ?? projection?.workRun
    if (projection === null || projection === undefined || previewRun?.id !== input.runId
      || projection.deliveryId === null || projection.readCursor === null
      || options.client === undefined || options.actor === undefined || options.scope === undefined
      || options.nextRequestId === undefined) return Object.freeze([])
    logDescriptors.clear()
    const segments: import('./candidate-run-preview-view-model.js').PreviewLogSegmentFacts[] = []
    const sourceEvidence = projection.managedAppSourceRun === null
      || projection.managedAppSourceRun === undefined
      ? projection.evidence
      : projection.managedAppSourceEvidence ?? Object.freeze([])
    for (const evidence of sourceEvidence) {
      const binding = evidenceBinding(projection, evidence)
      if (binding === null) continue
      const detailResponse = await options.client.query({
        schemaVersion: 'winwincode/v1',
        requestId: options.nextRequestId(),
        actor: options.actor,
        scope: options.scope,
        query: 'evidence.get',
        parameters: binding,
        page: { cursor: null, limit: 1 },
      } as QueryRequest)
      if (detailResponse.query !== 'evidence.get') continue
      if (!matchesCanonicalSchema('EvidenceDetailProjection', detailResponse.result)) continue
      const detail = detailResponse.result as { readonly evidence?: DeliveryEvidenceProjection; readonly artifactAccess?: { readonly state: string; readonly items?: readonly EvidenceArtifactDescriptorProjection[] } }
      if (detail.evidence?.id !== evidence.id || detail.artifactAccess?.state !== 'available') continue
      for (const descriptor of detail.artifactAccess.items ?? []) {
        if (!LOG_ARTIFACT_KINDS.has(descriptor.kind) || descriptor.previewMode !== 'inline_text') continue
        const key = `${evidence.id}:${descriptor.artifactId}`
        logDescriptors.set(key, { evidence, descriptor, sourceRef: evidence.sourceRef })
        const text = await readLogArtifact(projection, { evidence, descriptor })
        if (text === null) continue
        const lines = text.length === 0 ? [] : text.split(/\r?\n/u)
        segments.push(Object.freeze({
          key,
          stream: descriptor.fileName?.toLowerCase().includes('stderr') ? 'stderr' : descriptor.kind === 'log' ? 'stdout' : 'diagnostic',
          lineCount: Math.min(lines.length, PREVIEW_LOG_MAX_LINES),
          truncated: descriptor.sizeBytes > LOG_READ_BYTES || lines.length > PREVIEW_LOG_MAX_LINES,
          maxLines: PREVIEW_LOG_MAX_LINES,
        }))
      }
    }
    return Object.freeze(segments)
  }

  async function readLogCitation(input: { readonly runId: string; readonly segmentKey: string; readonly lineStart: number; readonly lineEnd: number }): Promise<import('./candidate-run-preview-view-model.js').PreviewLogCitation | null> {
    const projection = await options.runIdentityProjection?.()
    const entry = logDescriptors.get(input.segmentKey)
    const previewRun = projection?.managedAppSourceRun ?? projection?.workRun
    if (projection === null || projection === undefined || entry === undefined
      || previewRun?.id !== input.runId) return null
    const text = await readLogArtifact(projection, entry)
    if (text === null) return null
    const lines = text.split(/\r?\n/u)
    if (lines.length === 0 || lines[0] === '') return null
    const start = Math.max(1, Math.min(input.lineStart, PREVIEW_LOG_MAX_LINES))
    const end = Math.max(start, Math.min(input.lineEnd, PREVIEW_LOG_MAX_LINES))
    return Object.freeze({
      segmentKey: input.segmentKey,
      lineStart: start,
      lineEnd: Math.min(end, lines.length),
      redactedText: redactPublicText(lines.slice(start - 1, end).join('\n')),
      sourceRef: entry.sourceRef,
    })
  }

  async function request(path: string, method: 'DELETE' | 'POST', body?: string) {
    if (options.fetch === undefined) {
      throw failure('protocol', 'TRANSPORT_UNAVAILABLE', '浏览器 HTTP transport 不可用。')
    }
    try {
      return await Reflect.apply(options.fetch, undefined, [`${location.serverUrl}${path}`, {
        method,
        headers: body === undefined ? {} : { 'content-type': 'application/json' },
        ...(body === undefined ? {} : { body }),
        redirect: 'error',
        cache: 'no-store',
        referrerPolicy: 'no-referrer',
        credentials: 'include',
      }])
    } catch (error) {
      if (error instanceof ControlPlaneClientError) throw error
      throw failure('network', 'NETWORK_ERROR', '无法连接 Server 的预览服务。')
    }
  }

  function managedRunControl(source: string): ManagedRunControl {
    let value: unknown
    try {
      value = JSON.parse(source)
    } catch {
      throw failure('protocol', 'INVALID_MANAGED_APP_RESPONSE', 'Server 返回了无效的受管应用状态。')
    }
    if (!isRecord(value)
      || value.schemaVersion !== 'winwincode/managed-app-run-v1'
      || typeof value.runId !== 'string'
      || typeof value.leaseId !== 'string'
      || !['idle', 'starting', 'ready', 'failed', 'exited'].includes(String(value.phase))
      || (value.startedAt !== null && typeof value.startedAt !== 'string')
      || (value.exitedAt !== null && typeof value.exitedAt !== 'string')
      || (value.exitCode !== null && typeof value.exitCode !== 'number')
      || (value.failureReason !== null && typeof value.failureReason !== 'string')) {
      throw failure('protocol', 'INVALID_MANAGED_APP_RESPONSE', 'Server 返回了无效的受管应用状态。')
    }
    return Object.freeze({
      runId: value.runId,
      leaseId: value.leaseId,
      phase: value.phase as ManagedRunControl['phase'],
      startedAt: value.startedAt,
      exitedAt: value.exitedAt,
      exitCode: value.exitCode,
      failureReason: value.failureReason,
    })
  }

  async function managedAppRequest(
    operation: 'start' | 'stop' | 'restart' | 'query',
    runId: string,
    requestId: string,
    leaseId: string | null,
  ): Promise<ManagedRunControl> {
    if (options.occupancyStatus === undefined) {
      throw failure('protocol', 'MANAGED_APP_CONTEXT_UNAVAILABLE', '当前设备没有可用的占用上下文。')
    }
    const occupancy = await options.occupancyStatus(options.identity.clientId)
    if (!('occupancyLeaseId' in occupancy)
      || (occupancy.occupancy !== 'occupied' && occupancy.occupancy !== 'draining')) {
      throw failure('protocol', 'MANAGED_APP_CONTEXT_UNAVAILABLE', '当前设备没有可用的占用上下文。')
    }
    if (leaseId !== null && occupancy.occupancyLeaseId !== leaseId) {
      throw failure('protocol', 'MANAGED_APP_LEASE_STALE', '设备占用已变化，请重新打开候选预览。')
    }
    let config: ManagedAppCommand['config'] = null
    if (operation === 'start' || operation === 'restart') {
      const candidateConfig = options.managedAppRunConfig
      if (candidateConfig === undefined
        || candidateConfig.runId !== runId
        || candidateConfig.attempt !== options.identity.attempt
        || (options.identity.mode === 'frozen-candidate'
          && (candidateConfig.mode !== 'frozen-candidate'
            || candidateConfig.candidateCommit !== options.identity.candidateCommit))) {
        throw failure('protocol', 'MANAGED_APP_CONFIG_UNAVAILABLE', '当前候选没有可用的受管应用运行配置。')
      }
      // The Server resolves the complete executable config from the durable
      // WorkRun fact. The browser sends only the operation and run identity.
      config = null
    }
    const command: ManagedAppCommand = {
      schemaVersion: 'winwincode/managed-app-run-v1',
      operation: operation as ManagedAppCommand['operation'],
      idempotencyKey: requestId as ManagedAppCommand['idempotencyKey'],
      occupancyLeaseId: occupancy.occupancyLeaseId as ManagedAppCommand['occupancyLeaseId'],
      // The browser occupancy facade keeps this as a safe JS number; the
      // generated exchange alias is a wire bigint string. The managed-app
      // HTTP endpoint is JSON/u64 and therefore receives the numeric value.
      occupancyFencingToken: occupancy.fencingToken as unknown as ManagedAppCommand['occupancyFencingToken'],
      config,
      runId: runId as ManagedAppCommand['runId'],
    }
    const response = await request(
      `/api/v1/clients/${encodeURIComponent(options.identity.clientId)}/managed-app`,
      'POST',
      JSON.stringify(command),
    )
    const source = await response.text()
    if (!response.ok) {
      throw candidateBoundaryError(
        response.status,
        source,
        'MANAGED_APP_COMMAND_REJECTED',
        `Server 拒绝受管应用操作（HTTP ${String(response.status)}）。`,
      )
    }
    if (response.status !== 202) {
      throw failure('protocol', 'INVALID_MANAGED_APP_STATUS', 'Server 返回了无效的受管应用操作状态。')
    }
    return managedRunControl(source)
  }

  async function listFiles(input: { readonly sourceId: string }): Promise<readonly PreviewFileFacts[]> {
    const context = options.fileContext
    if (input.sourceId !== options.identity.sourceId) {
      throw failure('protocol', 'PREVIEW_IDENTITY_INVALID', '预览来源缺少有效运行身份。')
    }
    if (options.client === undefined || options.actor === undefined || options.scope === undefined
      || context === undefined || options.nextRequestId === undefined) {
      throw failure('protocol', 'CANDIDATE_FILE_CONTEXT_UNAVAILABLE', '当前候选没有可用的规范文件读取上下文。')
    }
    const files: PreviewFileFacts[] = []
    let cursor: string | null = null
    for (let pageCount = 0; pageCount < MAX_FILE_PAGES; pageCount += 1) {
      const response = await options.client.query({
        schemaVersion: 'winwincode/v1',
        requestId: options.nextRequestId(),
        actor: options.actor,
        scope: options.scope,
        query: 'candidate.files.list',
        parameters: {
          atCursor: context.atCursor,
          candidateRef: context.candidateRef,
          candidateTreeId: context.candidateTreeId,
          deliveryId: context.deliveryId,
          diffSha256: context.diffSha256,
          pathPrefix: null,
          readPageLimit: FILE_PAGE_LIMIT,
          statuses: [],
        },
        page: { cursor, limit: FILE_PAGE_LIMIT },
      } as QueryRequest)
      if (response.query !== 'candidate.files.list'
        || !matchesCanonicalSchema('CandidateFilePage', response.result)) {
        throw failure('protocol', 'INVALID_CANDIDATE_FILE_PAGE', '控制平面返回了无效的候选文件清单。')
      }
      const page = response.result as CandidateFilePage
      validateCandidateFilePage(page, context)
      files.push(...page.items.map(candidateFileFacts))
      if (!response.page.hasMore) return Object.freeze(files)
      if (response.page.nextCursor === null) {
        throw failure('protocol', 'INVALID_CANDIDATE_FILE_PAGE', '控制平面返回了无效的候选文件分页游标。')
      }
      cursor = response.page.nextCursor
    }
    throw failure('protocol', 'CANDIDATE_FILE_PAGE_LIMIT_EXCEEDED', '候选文件清单超过单次读取上限。')
  }

  async function readFileContent(input: {
    readonly sourceId: string
    readonly path: string
    readonly offset: number
  }): Promise<PreviewFileContentFacts> {
    const context = options.fileContext
    if (input.sourceId !== options.identity.sourceId || !isRepositoryRelativePath(input.path)
      || !Number.isSafeInteger(input.offset) || input.offset < 0 || input.offset >= MAX_FILE_CONTENT_BYTES) {
      throw failure('protocol', 'CANDIDATE_FILE_CONTENT_CONTEXT_INVALID', '候选文件内容的读取范围无效。')
    }
    if (options.client === undefined || options.actor === undefined || options.scope === undefined
      || context === undefined || options.nextRequestId === undefined) {
      throw failure('protocol', 'CANDIDATE_FILE_CONTENT_CONTEXT_UNAVAILABLE', '当前候选没有可用的规范文件内容读取上下文。')
    }
    const response = await options.client.query({
      schemaVersion: 'winwincode/v1',
      requestId: options.nextRequestId(),
      actor: options.actor,
      scope: options.scope,
      query: 'candidate.file.content.get',
      parameters: {
        atCursor: context.atCursor,
        candidateRef: context.candidateRef,
        candidateTreeId: context.candidateTreeId,
        deliveryId: context.deliveryId,
        diffSha256: context.diffSha256,
        path: input.path,
        offset: input.offset,
        length: Math.min(FILE_CONTENT_CHUNK_BYTES, MAX_FILE_CONTENT_BYTES - input.offset),
        readPageLimit: FILE_PAGE_LIMIT,
      },
      page: { cursor: null, limit: 1 },
    } as QueryRequest)
    if (response.query !== 'candidate.file.content.get'
      || !matchesCanonicalSchema('CandidateFileContentChunkProjection', response.result)) {
      throw failure('protocol', 'INVALID_CANDIDATE_FILE_CONTENT', '控制平面返回了无效的候选文件内容。')
    }
    const chunk = response.result as CandidateFileContentChunkProjection
    if (chunk.path !== input.path
      || chunk.offset !== input.offset
      || chunk.candidate.candidateRef !== context.candidateRef
      || chunk.candidate.candidateTreeId !== context.candidateTreeId
      || chunk.candidate.diffSha256 !== context.diffSha256
      || !sameReadCursor(chunk.readCursor, context.atCursor)
      || chunk.totalBytes > MAX_FILE_CONTENT_BYTES
      || chunk.returnedBytes > FILE_CONTENT_CHUNK_BYTES
      || chunk.offset + chunk.returnedBytes > chunk.totalBytes
      || (chunk.nextOffset === null
        ? chunk.offset + chunk.returnedBytes !== chunk.totalBytes
        : chunk.nextOffset !== chunk.offset + chunk.returnedBytes
          || chunk.nextOffset <= chunk.offset
          || chunk.nextOffset > chunk.totalBytes)) {
      throw failure('protocol', 'STALE_CANDIDATE_FILE_CONTENT', '候选版本或文件内容已变化，请重新打开任务。')
    }
    let bytes: Uint8Array
    try {
      const binary = atob(chunk.dataBase64)
      bytes = Uint8Array.from(binary, character => character.charCodeAt(0))
    } catch {
      throw failure('protocol', 'INVALID_CANDIDATE_FILE_CONTENT', '候选文件内容的编码无效。')
    }
    if (bytes.length !== chunk.returnedBytes) {
      throw failure('protocol', 'INVALID_CANDIDATE_FILE_CONTENT', '候选文件内容的长度无效。')
    }
    if (chunk.contentEncoding === 'utf-8') {
      try {
        new TextDecoder('utf-8', { fatal: true }).decode(bytes)
      } catch {
        throw failure('protocol', 'INVALID_CANDIDATE_FILE_CONTENT', '候选文件内容不是有效的 UTF-8 文本。')
      }
    }
    return Object.freeze({
      path: chunk.path,
      mediaType: chunk.mediaType,
      contentEncoding: chunk.contentEncoding,
      dataBase64: chunk.dataBase64,
      offset: chunk.offset,
      returnedBytes: chunk.returnedBytes,
      totalBytes: chunk.totalBytes,
      nextOffset: chunk.nextOffset,
    })
  }

  async function readDiff(input: {
    readonly sourceId: string
    readonly path: string
    readonly offset: number
  }): Promise<PreviewDiffChunkFacts> {
    const context = options.fileContext
    if (input.sourceId !== options.identity.sourceId || !isRepositoryRelativePath(input.path)
      || !Number.isSafeInteger(input.offset) || input.offset < 0 || input.offset >= MAX_DIFF_BYTES) {
      throw failure('protocol', 'CANDIDATE_DIFF_CONTEXT_INVALID', '候选变更 diff 的读取范围无效。')
    }
    if (options.client === undefined || options.actor === undefined || options.scope === undefined
      || context === undefined || options.nextRequestId === undefined) {
      throw failure('protocol', 'CANDIDATE_DIFF_CONTEXT_UNAVAILABLE', '当前候选没有可用的规范变更 diff 读取上下文。')
    }
    const response = await options.client.query({
      schemaVersion: 'winwincode/v1',
      requestId: options.nextRequestId(),
      actor: options.actor,
      scope: options.scope,
      query: 'candidate.diff.get',
      parameters: {
        atCursor: context.atCursor,
        candidateRef: context.candidateRef,
        candidateTreeId: context.candidateTreeId,
        deliveryId: context.deliveryId,
        diffSha256: context.diffSha256,
        path: input.path,
        offset: input.offset,
        length: DIFF_CHUNK_BYTES,
        readPageLimit: FILE_PAGE_LIMIT,
      },
      page: { cursor: null, limit: 1 },
    } as QueryRequest)
    if (response.query !== 'candidate.diff.get'
      || !matchesCanonicalSchema('CandidateDiffChunkProjection', response.result)) {
      throw failure('protocol', 'INVALID_CANDIDATE_DIFF_CHUNK', '控制平面返回了无效的候选变更 diff。')
    }
    const chunk = response.result as CandidateDiffChunkProjection
    if (chunk.path !== input.path
      || chunk.offset !== input.offset
      || chunk.candidate.candidateRef !== context.candidateRef
      || chunk.candidate.candidateTreeId !== context.candidateTreeId
      || chunk.candidate.diffSha256 !== context.diffSha256
      || !sameReadCursor(chunk.readCursor, context.atCursor)
      || chunk.binary
      || chunk.contentEncoding !== 'utf-8'
      || chunk.totalBytes > MAX_DIFF_BYTES
      || chunk.returnedBytes > DIFF_CHUNK_BYTES
      || (chunk.nextOffset === null
        ? chunk.offset + chunk.returnedBytes !== chunk.totalBytes
        : chunk.nextOffset !== chunk.offset + chunk.returnedBytes)) {
      throw failure('protocol', 'STALE_CANDIDATE_DIFF_CHUNK', '候选版本或变更 diff 已变化，请重新打开任务。')
    }
    let bytes: Uint8Array
    try {
      const binary = atob(chunk.dataBase64)
      bytes = Uint8Array.from(binary, character => character.charCodeAt(0))
    } catch {
      throw failure('protocol', 'INVALID_CANDIDATE_DIFF_CHUNK', '候选变更 diff 的内容编码无效。')
    }
    if (bytes.length !== chunk.returnedBytes || chunk.offset + chunk.returnedBytes > chunk.totalBytes
      || (chunk.nextOffset !== null
        && (chunk.nextOffset <= chunk.offset || chunk.nextOffset > chunk.totalBytes))) {
      throw failure('protocol', 'INVALID_CANDIDATE_DIFF_CHUNK', '候选变更 diff 的长度无效。')
    }
    const decoderKey = `${input.sourceId}\u0000${input.path}`
    const previousDecoder = diffDecoders.get(decoderKey)
    if (input.offset === 0) {
      diffDecoders.delete(decoderKey)
    } else if (previousDecoder === undefined
      || previousDecoder.nextOffset !== input.offset
      || previousDecoder.fileDiffSha256 !== chunk.fileDiffSha256) {
      throw failure('protocol', 'INVALID_CANDIDATE_DIFF_CHUNK', '候选变更 diff 的续读游标无效。')
    }
    try {
      const decoder = previousDecoder?.decoder ?? new TextDecoder('utf-8', { fatal: true })
      const text = decoder.decode(bytes, { stream: chunk.nextOffset !== null })
      if (chunk.nextOffset === null) diffDecoders.delete(decoderKey)
      else diffDecoders.set(decoderKey, {
        decoder,
        nextOffset: chunk.nextOffset,
        fileDiffSha256: chunk.fileDiffSha256,
      })
      return Object.freeze({
        path: chunk.path,
        oldPath: chunk.oldPath,
        status: chunk.status,
        text,
        offset: chunk.offset,
        returnedBytes: chunk.returnedBytes,
        totalBytes: chunk.totalBytes,
        nextOffset: chunk.nextOffset,
      })
    } catch {
      diffDecoders.delete(decoderKey)
      throw failure('protocol', 'INVALID_CANDIDATE_DIFF_CHUNK', '候选变更 diff 不是有效的 UTF-8 文本。')
    }
  }

  const base = {
    async loadIdentity() { return options.identity },
    async authorizePreview(input: { readonly sourceId: string; readonly requestId: string }) {
      if (!isCandidatePreviewIdentity(options.identity) || input.sourceId !== options.identity.sourceId) {
        throw failure('protocol', 'PREVIEW_IDENTITY_INVALID', '预览来源缺少有效运行身份。')
      }
      const response = await request(CREATE_PATH, 'POST', JSON.stringify({
        schemaVersion: 'winwincode/v1',
        clientId: options.identity.clientId,
        sourceId: options.identity.sourceId,
      }))
      const source = await response.text()
      if (!response.ok) {
        throw candidateBoundaryError(
          response.status,
          source,
          'PREVIEW_ACCESS_REJECTED',
          `Server 拒绝预览授权（HTTP ${String(response.status)}）。`,
        )
      }
      if (response.status !== 201) throw failure('protocol', 'INVALID_PREVIEW_ACCESS_STATUS', 'Server 返回了无效的预览授权状态。')
      const grant = parseGrant(source, options.identity)
      if (new URL(grant.previewOrigin).origin === new URL(location.serverUrl).origin) {
        throw failure('protocol', 'INVALID_PREVIEW_ACCESS_RESPONSE', '预览来源必须与管理 Server 使用独立来源。')
      }
      return grant
    },
    async revokePreview(input: { readonly previewAccessId: string; readonly requestId: string }) {
      if (!/^pva_[0-9a-f]{32}$/u.test(input.previewAccessId)) {
        throw failure('protocol', 'PREVIEW_ACCESS_ID_INVALID', '预览授权标识无效。')
      }
      const response = await request(`${CREATE_PATH}/${encodeURIComponent(input.previewAccessId)}`, 'DELETE')
      if (!response.ok) {
        const source = await response.text()
        throw candidateBoundaryError(
          response.status,
          source,
          'PREVIEW_REVOKE_REJECTED',
          `Server 拒绝撤销预览（HTTP ${String(response.status)}）。`,
        )
      }
      if (response.status !== 204) throw failure('protocol', 'INVALID_PREVIEW_REVOKE_STATUS', 'Server 返回了无效的预览撤销状态。')
      return Object.freeze({
        previewAccessId: input.previewAccessId,
        sourceId: options.identity.sourceId,
        mode: options.identity.mode,
        candidateCommit: options.identity.candidateCommit,
        previewOrigin: '',
        access: 'revoked',
        accessExpiresAt: null,
      })
    },
    readFileContent,
  }
  const managedAppRunConfig = options.managedAppRunConfig
  const managedAppControls = options.occupancyStatus !== undefined
    && managedAppRunConfig !== undefined
    ? {
      async startRun(input: { readonly identity: CandidatePreviewIdentity; readonly requestId: string }) {
        return managedAppRequest('start', managedAppRunConfig.runId, input.requestId, null)
      },
      async stopRun(input: { readonly runId: string; readonly leaseId: string; readonly requestId: string }) {
        return managedAppRequest('stop', input.runId, input.requestId, input.leaseId)
      },
      async restartRun(input: { readonly runId: string; readonly leaseId: string; readonly requestId: string }) {
        return managedAppRequest('restart', input.runId, input.requestId, input.leaseId)
      },
      async queryRun(input: { readonly runId: string; readonly leaseId: string; readonly requestId: string }) {
        return managedAppRequest('query', input.runId, input.requestId, input.leaseId)
      },
    }
    : {}
  return Object.freeze(
    options.client !== undefined
      && options.actor !== undefined
      && options.scope !== undefined
      && options.nextRequestId !== undefined
      && hasCandidateFileContext(options.fileContext)
      ? { ...base, ...managedAppControls, listFiles, readDiff, listLogSegments, readLogCitation }
      : options.runIdentityProjection !== undefined
        ? { ...base, ...managedAppControls, listLogSegments, readLogCitation }
        : { ...base, ...managedAppControls },
  )
}
