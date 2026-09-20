// SPDX-License-Identifier: Apache-2.0

/**
 * WWX-RUN-04 candidate run preview view-model.
 *
 * One snapshot type for the preview main surface: candidate identity (RUN-02),
 * managed-run lifecycle with lease-bound idempotent controls (RUN-03), the
 * authorized remote preview source (RUN-05), viewport and navigation (RUN-06),
 * file preview classes (RUN-07), and bounded log/diagnostic citations (RUN-08).
 * The model never invents facts; a missing field stays null and the page names
 * the gap.
 */

export type CandidatePreviewMode = 'live' | 'frozen-candidate'

export type ManagedRunPhase =
  | 'idle'
  | 'starting'
  | 'ready'
  | 'failed'
  | 'exited'

export type PreviewViewportPreset = 'desktop' | 'mobile' | 'custom'

export type PreviewAccessState =
  | 'none'
  | 'authorized'
  | 'revoked'

export type PreviewFileClass =
  | 'markdown'
  | 'code'
  | 'image'
  | 'html'
  | 'download'
  | 'degraded'

export type PreviewTone = 'info' | 'success' | 'warning' | 'danger' | 'neutral'

/** RUN-02 identity: never interchangeable between live and frozen modes. */
export interface CandidatePreviewIdentity {
  readonly mode: CandidatePreviewMode
  readonly clientId: string
  readonly sourceId: string
  /** WorkRun identity used by the authorized preview source. */
  readonly workRunId: string
  readonly repositoryBindingId: string
  readonly taskId: string | null
  readonly attempt: number | null
  readonly candidateCommit: string | null
  readonly candidateTreeId: string | null
  readonly runConfigVersion: string | null
}

/** RUN-03 lease-bound control request. Duplicate starts reuse the same lease. */
export interface ManagedRunControl {
  readonly runId: string
  readonly leaseId: string
  readonly phase: ManagedRunPhase
  readonly startedAt: string | null
  readonly exitedAt: string | null
  readonly exitCode: number | null
  readonly failureReason: string | null
}

/** RUN-05 authorized tunnel source; local sockets never appear here. */
export interface AuthorizedPreviewSourceFacts {
  readonly previewAccessId: string
  readonly sourceId: string
  readonly mode: CandidatePreviewMode
  readonly candidateCommit: string | null
  /** Backend-facing origin only (e.g. https://preview.example/p/<sourceId>). */
  readonly previewOrigin: string
  readonly access: PreviewAccessState
  readonly accessExpiresAt: string | null
}

/** RUN-06 viewport and in-frame navigation. */
export interface PreviewViewportFacts {
  readonly preset: PreviewViewportPreset
  readonly width: number
  readonly height: number
}

export interface PreviewNavigationFacts {
  readonly path: string
  readonly canGoBack: boolean
  readonly canGoForward: boolean
  readonly lastError: string | null
}

/** RUN-07 one attachment or generated file under an authorized repo-relative path. */
export interface PreviewFileFacts {
  readonly path: string
  readonly previewClass: PreviewFileClass
  readonly degradationReason: string | null
  readonly totalBytes: number | null
}

/** One bounded chunk of the retained candidate file content. */
export interface PreviewFileContentFacts {
  readonly path: string
  readonly mediaType: string
  readonly contentEncoding: 'utf-8' | 'binary' | 'unknown-8bit'
  readonly dataBase64: string
  readonly offset: number
  readonly returnedBytes: number
  readonly totalBytes: number
  readonly nextOffset: number | null
}

export interface PreviewFileContentState extends PreviewFileContentFacts {
  readonly loading: boolean
  readonly error: string | null
  readonly retryable: boolean
}

export type PreviewDiffFileStatus =
  | 'added'
  | 'modified'
  | 'deleted'
  | 'renamed'
  | 'copied'
  | 'type_changed'

export interface PreviewDiffChunkFacts {
  readonly path: string
  readonly oldPath: string | null
  readonly status: PreviewDiffFileStatus
  readonly text: string
  readonly offset: number
  readonly returnedBytes: number
  readonly totalBytes: number
  readonly nextOffset: number | null
}

export interface PreviewDiffState {
  readonly path: string
  readonly oldPath: string | null
  readonly status: PreviewDiffFileStatus
  readonly text: string
  readonly returnedBytes: number
  readonly totalBytes: number
  readonly nextOffset: number | null
  readonly loading: boolean
  readonly error: string | null
  readonly retryable: boolean
}

/** RUN-08 one redacted, source-bound log citation. */
export interface PreviewLogCitation {
  readonly segmentKey: string
  readonly lineStart: number
  readonly lineEnd: number
  readonly redactedText: string
  readonly sourceRef: string
}

export interface PreviewLogSegmentFacts {
  readonly key: string
  readonly stream: 'stdout' | 'stderr' | 'diagnostic'
  readonly lineCount: number
  readonly truncated: boolean
  readonly maxLines: number
}

export interface CandidatePreviewState {
  readonly status: 'idle' | 'loading' | 'ready' | 'error'
  readonly identity: CandidatePreviewIdentity | null
  readonly run: ManagedRunControl | null
  readonly source: AuthorizedPreviewSourceFacts | null
  readonly viewport: PreviewViewportFacts
  readonly navigation: PreviewNavigationFacts
  readonly files: readonly PreviewFileFacts[]
  readonly fileContent: PreviewFileContentState | null
  readonly fileDiff: PreviewDiffState | null
  readonly logSegments: readonly PreviewLogSegmentFacts[]
  readonly logCitation: PreviewLogCitation | null
  readonly notice: string | null
  readonly runControlAvailable: boolean
}

export interface CandidatePreviewPort {
  /** Current identity for this candidate/work run; null when unknown. */
  loadIdentity(): Promise<CandidatePreviewIdentity | null>
  /** Lease-bound start. Calling start while a run is live returns the same run. */
  startRun?(input: {
    readonly identity: CandidatePreviewIdentity
    readonly requestId: string
  }): Promise<ManagedRunControl>
  stopRun?(input: {
    readonly runId: string
    readonly leaseId: string
    readonly requestId: string
  }): Promise<ManagedRunControl>
  restartRun?(input: {
    readonly runId: string
    readonly leaseId: string
    readonly requestId: string
  }): Promise<ManagedRunControl>
  queryRun?(input: {
    readonly runId: string
    readonly leaseId: string
    readonly requestId: string
  }): Promise<ManagedRunControl>
  /** Short-lived authorized preview access for this source. */
  authorizePreview(input: {
    readonly sourceId: string
    readonly requestId: string
  }): Promise<AuthorizedPreviewSourceFacts>
  revokePreview(input: {
    readonly previewAccessId: string
    readonly requestId: string
  }): Promise<AuthorizedPreviewSourceFacts>
  listFiles?(input: {
    readonly sourceId: string
  }): Promise<readonly PreviewFileFacts[]>
  readDiff?(input: {
    readonly sourceId: string
    readonly path: string
    readonly offset: number
  }): Promise<PreviewDiffChunkFacts>
  readFileContent(input: {
    readonly sourceId: string
    readonly path: string
    readonly offset: number
  }): Promise<PreviewFileContentFacts>
  listLogSegments?(input: {
    readonly runId: string
  }): Promise<readonly PreviewLogSegmentFacts[]>
  readLogCitation?(input: {
    readonly runId: string
    readonly segmentKey: string
    readonly lineStart: number
    readonly lineEnd: number
  }): Promise<PreviewLogCitation | null>
}

export interface CandidatePreviewViewModelOptions {
  readonly port: CandidatePreviewPort
  readonly nextRequestId: () => string
  readonly now?: () => string
}

export interface CandidatePreviewViewModel {
  readonly state: CandidatePreviewState
  subscribe(listener: (state: CandidatePreviewState) => void): () => void
  start(): Promise<void>
  close(): void
  startRun(): Promise<void>
  stopRun(): Promise<void>
  restartRun(): Promise<void>
  authorizePreview(): Promise<void>
  revokePreview(): Promise<void>
  setViewport(preset: PreviewViewportPreset, size?: { width: number; height: number }): void
  navigate(path: string): void
  refresh(): void
  goBack(): void
  goForward(): void
  openFile(path: string): void
  openDiff(path: string): Promise<void>
  continueDiff(): Promise<void>
  citeLog(input: { segmentKey: string; lineStart: number; lineEnd: number }): Promise<void>
}

const DESKTOP_VIEWPORT: PreviewViewportFacts = Object.freeze({
  preset: 'desktop',
  width: 1280,
  height: 800,
})

type PreviewReadPhase = '文件内容' | '变更 diff'

interface PreviewReadFailure {
  readonly message: string
  readonly retryable: boolean
}

function previewReadFailure(error: unknown, phase: PreviewReadPhase): PreviewReadFailure {
  const record = error !== null && typeof error === 'object'
    ? error as Record<string, unknown>
    : null
  const code = typeof record?.code === 'string' ? record.code : null
  const kind = typeof record?.kind === 'string' ? record.kind : null
  const retryable = typeof record?.retryable === 'boolean'
    ? record.retryable
    : kind !== 'protocol'
  if (code === 'AUTHENTICATION_REQUIRED' || code === 'PERMISSION_DENIED'
    || code === 'PREVIEW_ACCESS_REJECTED') {
    return { message: '预览访问已失效，请重新授权。', retryable: false }
  }
  if (code === 'STALE_CANDIDATE_FILE_CONTENT' || code === 'STALE_CANDIDATE_DIFF_CHUNK'
    || code === 'CANDIDATE_FILE_CONTENT_CONTEXT_INVALID' || code === 'CANDIDATE_DIFF_CONTEXT_INVALID') {
    return { message: '候选版本或读取范围已变化，请重新打开任务。', retryable: false }
  }
  if (code === 'CANDIDATE_FILE_CONTENT_CONTEXT_UNAVAILABLE'
    || code === 'CANDIDATE_DIFF_CONTEXT_UNAVAILABLE') {
    return { message: `当前候选暂不可读取${phase}，请稍后重试。`, retryable }
  }
  return { message: `${phase}读取失败，请重试。`, retryable }
}

const MOBILE_VIEWPORT: PreviewViewportFacts = Object.freeze({
  preset: 'mobile',
  width: 390,
  height: 844,
})

/** RUN-08 hard segment ceiling; longer streams stay truncated, never unbounded. */
export const PREVIEW_LOG_MAX_LINES = 200

function initialState(): CandidatePreviewState {
  return Object.freeze({
    status: 'idle',
    identity: null,
    run: null,
    source: null,
    viewport: DESKTOP_VIEWPORT,
    navigation: Object.freeze({
      path: '/',
      canGoBack: false,
      canGoForward: false,
      lastError: null,
    }),
    files: Object.freeze([]) as readonly PreviewFileFacts[],
    fileContent: null,
    fileDiff: null,
    logSegments: Object.freeze([]) as readonly PreviewLogSegmentFacts[],
    logCitation: null,
    notice: null,
    runControlAvailable: false,
  })
}

/** RUN-02 copy: the mode banner never softens live vs frozen. */
export function candidatePreviewModeText(mode: CandidatePreviewMode): string {
  return mode === 'live'
    ? '实时开发预览'
    : '冻结候选验收预览'
}

export function candidatePreviewModeTone(mode: CandidatePreviewMode): PreviewTone {
  return mode === 'live' ? 'warning' : 'success'
}

/** RUN-02: live previews must never be presented as acceptance proof. */
export function candidatePreviewAcceptanceEligible(mode: CandidatePreviewMode): boolean {
  return mode === 'frozen-candidate'
}

export function managedRunPhaseText(phase: ManagedRunPhase): string {
  switch (phase) {
    case 'idle': return '未启动'
    case 'starting': return '启动中'
    case 'ready': return '已就绪'
    case 'failed': return '启动失败'
    case 'exited': return '已退出'
  }
}

export function managedRunPhaseTone(phase: ManagedRunPhase): PreviewTone {
  switch (phase) {
    case 'idle': return 'neutral'
    case 'starting': return 'info'
    case 'ready': return 'success'
    case 'failed': return 'danger'
    case 'exited': return 'neutral'
  }
}

/**
 * RUN-05: only Backend-visible origins. Localhost, link-local, and metadata
 * addresses are rejected at the view-model boundary.
 */
export function isSafePreviewOrigin(origin: string): boolean {
  let parsed: URL
  try {
    parsed = new URL(origin)
  } catch {
    return false
  }
  if (parsed.protocol !== 'https:' && parsed.protocol !== 'http:') return false
  const host = parsed.hostname.toLowerCase()
  if (host === 'localhost' || host.endsWith('.localhost')) return false
  if (host === '127.0.0.1' || host === '::1' || host === '[::1]') return false
  if (host.startsWith('169.254.')) return false
  if (host === '0.0.0.0') return false
  return host.length > 0
}

function isPortablePreviewId(value: string | null): boolean {
  return value !== null && value.length > 0 && value.length <= 200
    && /^[A-Za-z0-9_.:/-]+$/u.test(value)
}

const GIT_OBJECT_ID = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u

/** Validates the route-carried identity before it can authorize a source. */
export function isCandidatePreviewIdentity(identity: CandidatePreviewIdentity): boolean {
  return /^\d{9,12}$/u.test(identity.clientId)
    && isPortablePreviewId(identity.sourceId)
    && identity.workRunId.startsWith('wrn_')
    && isPortablePreviewId(identity.workRunId)
    && isPortablePreviewId(identity.repositoryBindingId)
    && (identity.taskId === null || isPortablePreviewId(identity.taskId))
    && (identity.attempt === null || (Number.isSafeInteger(identity.attempt) && identity.attempt > 0))
    && (identity.candidateTreeId === null || GIT_OBJECT_ID.test(identity.candidateTreeId))
    && (identity.runConfigVersion === null || isPortablePreviewId(identity.runConfigVersion))
    && (identity.mode === 'live'
      ? identity.candidateCommit === null
      : identity.candidateCommit !== null && GIT_OBJECT_ID.test(identity.candidateCommit))
}

/** Keeps navigation inside the granted preview URL path. */
export function isSafePreviewPath(path: string): boolean {
  if (!path.startsWith('/') || path.startsWith('//') || path.length > 4096 || path.includes('\\')) {
    return false
  }
  try {
    return (path.split(/[?#]/u, 1)[0] ?? '').split('/').every(segment => {
      const decoded = decodeURIComponent(segment)
      return decoded !== '.' && decoded !== '..' && !/[\u0000-\u001f\u007f]/u.test(decoded)
    })
  } catch {
    return false
  }
}

/** RUN-07: attachments only under a normalized repository-relative path. */
export function isRepositoryRelativePath(path: string): boolean {
  if (path.length === 0 || path.length > 4096) return false
  if (path.startsWith('/') || path.includes('\\')) return false
  if (/^[A-Za-z]:/.test(path)) return false
  const segments = path.split('/')
  return segments.every(segment => segment !== '' && segment !== '.' && segment !== '..')
}

export function classifyPreviewFile(path: string): PreviewFileFacts {
  if (!isRepositoryRelativePath(path)) {
    return Object.freeze({
      path,
      previewClass: 'degraded',
      degradationReason: '路径不是规范化的仓库相对位置，已拒绝渲染。',
      totalBytes: null,
    })
  }
  const lower = path.toLowerCase()
  if (lower.endsWith('.md') || lower.endsWith('.markdown')) {
    return Object.freeze({
      path,
      previewClass: 'markdown',
      degradationReason: null,
      totalBytes: null,
    })
  }
  if (lower.endsWith('.html') || lower.endsWith('.htm') || lower.endsWith('.svg')) {
    return Object.freeze({
      path,
      previewClass: 'html',
      degradationReason: '可执行文档在独立预览来源渲染，不与管理界面同源。',
      totalBytes: null,
    })
  }
  if (/\.(png|jpe?g|gif|webp|avif)$/.test(lower)) {
    return Object.freeze({
      path,
      previewClass: 'image',
      degradationReason: null,
      totalBytes: null,
    })
  }
  if (/\.(ts|tsx|js|jsx|mjs|cjs|rs|json|toml|yml|yaml|css|html|py|go|java|kt|swift|sh)$/.test(lower)) {
    return Object.freeze({
      path,
      previewClass: 'code',
      degradationReason: null,
      totalBytes: null,
    })
  }
  return Object.freeze({
    path,
    previewClass: 'download',
    degradationReason: '二进制或未知类型仅提供下载，不在管理界面内联渲染。',
    totalBytes: null,
  })
}

function clampViewport(size: { width: number; height: number }): PreviewViewportFacts {
  const width = Math.min(4096, Math.max(200, Math.round(size.width)))
  const height = Math.min(4096, Math.max(200, Math.round(size.height)))
  return Object.freeze({ preset: 'custom', width, height })
}

function identityFingerprint(identity: CandidatePreviewIdentity): string {
  return [
    identity.mode,
    identity.clientId,
    identity.sourceId,
    identity.workRunId,
    identity.repositoryBindingId,
    identity.taskId ?? '',
    identity.attempt === null ? '' : String(identity.attempt),
    identity.candidateCommit ?? '',
    identity.candidateTreeId ?? '',
    identity.runConfigVersion ?? '',
  ].join('|')
}

function projectPreviewFiles(files: readonly PreviewFileFacts[]): readonly PreviewFileFacts[] {
  return files.map(file => {
    const projected = classifyPreviewFile(file.path)
    const backendDegraded = file.previewClass === 'degraded'
    const unsafePath = projected.previewClass === 'degraded'
    const previewClass = unsafePath || backendDegraded ? 'degraded' : projected.previewClass
    const degradationReason = unsafePath
      ? projected.degradationReason
      : backendDegraded
        ? (file.degradationReason ?? '该文件暂不可在预览中打开。')
        : projected.degradationReason
    const totalBytes = file.totalBytes !== null && Number.isSafeInteger(file.totalBytes) && file.totalBytes >= 0
      ? file.totalBytes
      : projected.totalBytes
    return Object.freeze({
      ...projected,
      previewClass,
      degradationReason,
      totalBytes,
    })
  })
}

export function createCandidatePreviewViewModel(
  options: CandidatePreviewViewModelOptions,
): CandidatePreviewViewModel {
  let state = Object.freeze({
    ...initialState(),
    runControlAvailable: options.port.startRun !== undefined
      && options.port.stopRun !== undefined
      && options.port.restartRun !== undefined,
  })
  const listeners = new Set<(next: CandidatePreviewState) => void>()
  const backStack: string[] = []
  const forwardStack: string[] = []
  let closed = false
  /** RUN-03: one live run identity; a second start never opens a second run. */
  let activeRunKey: string | null = null
  let pendingStartKey: string | null = null
  let pendingDiffPath: string | null = null

  function emit(patch: Partial<CandidatePreviewState>): void {
    state = Object.freeze({ ...state, ...patch })
    for (const listener of listeners) listener(state)
  }

  function notice(text: string | null): void {
    emit({ notice: text })
  }

  async function refreshLogs(runId: string): Promise<void> {
    if (options.port.listLogSegments === undefined) return
    try {
      const segments = await options.port.listLogSegments({ runId })
      if (!closed && state.run?.runId === runId) emit({ logSegments: projectLogSegments(segments) })
    } catch {
      // A run remains usable when its optional diagnostics endpoint is unavailable.
    }
  }

  async function ensureIdentity(): Promise<CandidatePreviewIdentity | null> {
    if (state.identity !== null) return state.identity
    const identity = await options.port.loadIdentity()
    if (closed) return null
    emit({ identity })
    return identity
  }

  async function loadDiffChunk(path: string, offset: number): Promise<void> {
    const readDiff = options.port.readDiff
    const identity = await ensureIdentity()
    if (readDiff === undefined || identity === null || closed) return
    const current = state.fileDiff
    if (current !== null && current.path !== path && offset !== 0) return
    try {
      const chunk = await readDiff({ sourceId: identity.sourceId, path, offset })
      if (closed || chunk.path !== path || chunk.offset !== offset) return
      const previous = offset === 0 || current === null || current.path !== path ? '' : current.text
      emit({
        fileDiff: Object.freeze({
          path,
          oldPath: chunk.oldPath,
          status: chunk.status,
          text: previous + chunk.text,
          returnedBytes: previous === '' ? chunk.returnedBytes : (current?.returnedBytes ?? 0) + chunk.returnedBytes,
          totalBytes: chunk.totalBytes,
          nextOffset: chunk.nextOffset,
          loading: false,
          error: null,
          retryable: false,
        }),
        notice: null,
      })
    } catch (error) {
      if (closed) return
      const failure = previewReadFailure(error, '变更 diff')
      emit({
        fileDiff: Object.freeze({
          path,
          oldPath: current?.path === path ? current.oldPath : null,
          status: current?.path === path ? current.status : 'modified',
          text: current?.path === path ? current.text : '',
          returnedBytes: current?.path === path ? current.returnedBytes : 0,
          totalBytes: current?.path === path ? current.totalBytes : 0,
          nextOffset: null,
          loading: false,
          error: failure.message,
          retryable: failure.retryable,
        }),
        notice: null,
      })
    }
  }

  async function loadFileContent(path: string): Promise<void> {
    const readFileContent = options.port.readFileContent
    const identity = await ensureIdentity()
    if (identity === null || closed) return
    emit({
      fileContent: Object.freeze({
        path,
        mediaType: 'application/octet-stream',
        contentEncoding: 'unknown-8bit',
        dataBase64: '',
        offset: 0,
        returnedBytes: 0,
        totalBytes: 0,
        nextOffset: null,
        loading: true,
        error: null,
        retryable: false,
      }),
      notice: null,
    })
    try {
      const content = await readFileContent({ sourceId: identity.sourceId, path, offset: 0 })
      if (closed || content.path !== path || content.offset !== 0) return
      emit({ fileContent: Object.freeze({ ...content, loading: false, error: null, retryable: false }), notice: null })
    } catch (error) {
      if (closed) return
      const failure = previewReadFailure(error, '文件内容')
      emit({
        fileContent: Object.freeze({
          path,
          mediaType: 'application/octet-stream',
          contentEncoding: 'unknown-8bit',
          dataBase64: '',
          offset: 0,
          returnedBytes: 0,
          totalBytes: 0,
          nextOffset: null,
          loading: false,
          error: failure.message,
          retryable: failure.retryable,
        }),
        notice: null,
      })
    }
  }

  return {
    get state() {
      return state
    },
    subscribe(listener) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    async start() {
      if (closed) return
      emit({ status: 'loading', notice: null })
      try {
        const identity = await ensureIdentity()
        if (closed) return
        if (identity === null) {
          emit({
            status: 'error',
            notice: '无法读取候选运行身份。请从任务看板重新打开本预览。',
          })
          return
        }
        if (!isCandidatePreviewIdentity(identity)) {
          emit({
            status: 'error',
            notice: '候选运行身份不完整或无效，拒绝打开预览。',
          })
          return
        }
        emit({ status: 'ready', notice: null })
      } catch {
        if (closed) return
        emit({
          status: 'error',
          notice: '加载候选运行预览失败。',
        })
      }
    },
    close() {
      closed = true
      listeners.clear()
    },
    async startRun() {
      if (closed) return
      if (options.port.startRun === undefined) {
        notice('本地 Client 尚未发布受管应用控制能力。')
        return
      }
      const identity = await ensureIdentity()
      if (identity === null) {
        notice('缺少候选运行身份，无法启动受管应用。')
        return
      }
      if (!isCandidatePreviewIdentity(identity)) {
        notice('候选运行身份不完整或无效，拒绝启动受管应用。')
        return
      }
      const key = identityFingerprint(identity)
      if (activeRunKey === key && state.run !== null && state.run.phase !== 'exited' && state.run.phase !== 'failed') {
        // RUN-03 idempotent start: a second click keeps the existing run.
        notice('该候选已有一个进行中的受管运行，不会重复启动。')
        return
      }
      if (pendingStartKey === key) {
        notice('该候选正在启动受管运行，请等待当前请求完成。')
        return
      }
      pendingStartKey = key
      notice(null)
      try {
        const run = await options.port.startRun({
          identity,
          requestId: options.nextRequestId(),
        })
        if (closed) return
        activeRunKey = key
        emit({
          run,
          notice: run.phase === 'failed'
            ? (run.failureReason ?? '受管应用启动失败。')
            : null,
        })
        await refreshLogs(run.runId)
      } catch {
        if (closed) return
        notice('启动受管应用失败。')
      } finally {
        if (pendingStartKey === key) pendingStartKey = null
      }
    },
    async stopRun() {
      if (closed) return
      if (options.port.stopRun === undefined) {
        notice('本地 Client 尚未发布受管应用控制能力。')
        return
      }
      const run = state.run
      if (run === null || run.phase === 'idle' || run.phase === 'exited') {
        notice('当前没有可停止的受管运行。')
        return
      }
      try {
        const next = await options.port.stopRun({
          runId: run.runId,
          leaseId: run.leaseId,
          requestId: options.nextRequestId(),
        })
        if (closed) return
        activeRunKey = null
        emit({ run: next, notice: null })
        await refreshLogs(next.runId)
      } catch {
        if (closed) return
        notice('停止受管应用失败。')
      }
    },
    async restartRun() {
      if (closed) return
      if (options.port.restartRun === undefined) {
        notice('本地 Client 尚未发布受管应用控制能力。')
        return
      }
      const run = state.run
      if (run === null) {
        notice('尚未启动受管运行，无法重启。')
        return
      }
      try {
        const next = await options.port.restartRun({
          runId: run.runId,
          leaseId: run.leaseId,
          requestId: options.nextRequestId(),
        })
        if (closed) return
        emit({ run: next, notice: null })
        await refreshLogs(next.runId)
      } catch {
        if (closed) return
        notice('重启受管应用失败。')
      }
    },
    async authorizePreview() {
      if (closed) return
      const identity = await ensureIdentity()
      if (identity === null) {
        notice('缺少候选运行身份，无法授权预览。')
        return
      }
      if (!isCandidatePreviewIdentity(identity)) {
        notice('候选运行身份不完整或无效，拒绝授权预览。')
        return
      }
      try {
        const source = await options.port.authorizePreview({
          sourceId: identity.sourceId,
          requestId: options.nextRequestId(),
        })
        if (closed) return
        if (!isSafePreviewOrigin(source.previewOrigin)) {
          emit({
            source: null,
            notice: '预览来源不是 Backend 可达的安全地址，已拒绝。',
          })
          return
        }
        if (source.sourceId !== identity.sourceId
          || source.mode !== identity.mode
          || source.candidateCommit !== identity.candidateCommit) {
          emit({
            source: null,
            notice: '预览授权与当前候选身份不匹配，已拒绝。',
          })
          return
        }
        const files = options.port.listFiles === undefined
          ? []
          : await options.port.listFiles({ sourceId: source.sourceId })
        if (closed) return
        emit({
          source,
          files: projectPreviewFiles(files),
          fileContent: null,
          fileDiff: null,
          notice: source.access === 'revoked' ? '预览访问已撤销。' : null,
        })
      } catch {
        if (closed) return
        notice('授权远程预览失败。')
      }
    },
    async revokePreview() {
      if (closed) return
      const source = state.source
      if (source === null) {
        notice('当前没有可撤销的预览访问。')
        return
      }
      try {
        const next = await options.port.revokePreview({
          previewAccessId: source.previewAccessId,
          requestId: options.nextRequestId(),
        })
        if (closed) return
        emit({ source: next, files: [], fileContent: null, fileDiff: null, notice: '预览访问已撤销。' })
      } catch {
        if (closed) return
        notice('撤销预览访问失败。')
      }
    },
    setViewport(preset, size) {
      if (closed) return
      if (preset === 'desktop') {
        emit({ viewport: DESKTOP_VIEWPORT })
        return
      }
      if (preset === 'mobile') {
        emit({ viewport: MOBILE_VIEWPORT })
        return
      }
      emit({ viewport: clampViewport(size ?? { width: 800, height: 600 }) })
    },
    navigate(path) {
      if (closed) return
      if (!isSafePreviewPath(path)) {
        emit({
          navigation: Object.freeze({
            ...state.navigation,
            lastError: '预览路径必须是安全的站内绝对路径。',
          }),
        })
        return
      }
      const current = state.navigation.path
      if (path === current) return
      backStack.push(current)
      forwardStack.length = 0
      emit({
        navigation: Object.freeze({
          path,
          canGoBack: backStack.length > 0,
          canGoForward: false,
          lastError: null,
        }),
      })
    },
    refresh() {
      if (closed) return
      emit({
        navigation: Object.freeze({
          ...state.navigation,
          lastError: null,
        }),
      })
    },
    goBack() {
      if (closed) return
      const previous = backStack.pop()
      if (previous === undefined) return
      forwardStack.push(state.navigation.path)
      emit({
        navigation: Object.freeze({
          path: previous,
          canGoBack: backStack.length > 0,
          canGoForward: true,
          lastError: null,
        }),
      })
    },
    goForward() {
      if (closed) return
      const next = forwardStack.pop()
      if (next === undefined) return
      backStack.push(state.navigation.path)
      emit({
        navigation: Object.freeze({
          path: next,
          canGoBack: true,
          canGoForward: forwardStack.length > 0,
          lastError: null,
        }),
      })
    },
    async openFile(path) {
      if (closed) return
      const authorizedFile = state.files.find(candidate => candidate.path === path)
      const file = authorizedFile ?? classifyPreviewFile(path)
      if (file.previewClass === 'degraded') {
        notice(file.degradationReason)
        return
      }
      if (state.source?.access !== 'authorized' || authorizedFile === undefined) {
        notice('请先授权并从当前文件清单中选择文件。')
        return
      }
      if (file.previewClass === 'html') {
        // HTML/SVG stays on the separately authorized preview origin; the
        // management document never receives untrusted markup.
        this.navigate(`/${path}`)
        emit({ fileContent: null, notice: 'HTML/SVG 仅在独立受控预览来源中渲染。' })
        return
      }
      await loadFileContent(path)
    },
    async openDiff(path) {
      if (closed) return
      const file = state.files.find(candidate => candidate.path === path)
      if (file === undefined || (file.previewClass !== 'code' && file.previewClass !== 'markdown')) {
        notice('该文件没有可内联查看的变更 diff。')
        return
      }
      if (options.port.readDiff === undefined) {
        notice('当前候选没有可用的变更 diff 读取能力。')
        return
      }
      if (pendingDiffPath === path) return
      pendingDiffPath = path
      emit({
        fileDiff: Object.freeze({
          path,
          oldPath: null,
          status: 'modified',
          text: '',
          returnedBytes: 0,
          totalBytes: 0,
          nextOffset: null,
          loading: true,
          error: null,
          retryable: false,
        }),
      })
      try {
        await loadDiffChunk(path, 0)
      } finally {
        if (pendingDiffPath === path) pendingDiffPath = null
      }
    },
    async continueDiff() {
      const diff = state.fileDiff
      if (closed || diff === null || diff.loading || diff.nextOffset === null) return
      emit({ fileDiff: Object.freeze({ ...diff, loading: true, error: null }) })
      await loadDiffChunk(diff.path, diff.nextOffset)
    },
    async citeLog(input) {
      if (closed) return
      const run = state.run
      if (run === null) {
        notice('尚未启动受管运行，无法引用日志。')
        return
      }
      if (options.port.readLogCitation === undefined) {
        notice('当前运行没有可引用的日志来源。')
        return
      }
      try {
        const lineStart = Math.max(1, Math.floor(input.lineStart))
        const lineEnd = Math.max(lineStart, Math.floor(input.lineEnd))
        const citation = await options.port.readLogCitation({
          runId: run.runId,
          segmentKey: input.segmentKey,
          lineStart,
          lineEnd,
        })
        if (closed) return
        if (citation === null) {
          notice('日志片段不可用。')
          return
        }
        emit({ logCitation: citation, notice: null })
      } catch {
        if (closed) return
        notice('读取日志片段失败。')
      }
    },
  }
}

/** Pure projection used by tests and the page when a run finishes loading. */
export function projectLogSegments(
  segments: readonly PreviewLogSegmentFacts[],
): readonly PreviewLogSegmentFacts[] {
  return segments.map(segment => Object.freeze({
    ...segment,
    maxLines: Math.min(segment.maxLines, PREVIEW_LOG_MAX_LINES),
    truncated: segment.truncated || segment.lineCount > Math.min(segment.maxLines, PREVIEW_LOG_MAX_LINES),
  }))
}
