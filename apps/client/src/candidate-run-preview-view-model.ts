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
  readonly sourceId: string
  readonly workerSessionId: string
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
  readonly logSegments: readonly PreviewLogSegmentFacts[]
  readonly logCitation: PreviewLogCitation | null
  readonly notice: string | null
}

export interface CandidatePreviewPort {
  /** Current identity for this candidate/work run; null when unknown. */
  loadIdentity(): Promise<CandidatePreviewIdentity | null>
  /** Lease-bound start. Calling start while a run is live returns the same run. */
  startRun(input: {
    readonly identity: CandidatePreviewIdentity
    readonly requestId: string
  }): Promise<ManagedRunControl>
  stopRun(input: {
    readonly runId: string
    readonly leaseId: string
    readonly requestId: string
  }): Promise<ManagedRunControl>
  restartRun(input: {
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
    readonly sourceId: string
    readonly requestId: string
  }): Promise<AuthorizedPreviewSourceFacts>
  listFiles(input: {
    readonly sourceId: string
  }): Promise<readonly PreviewFileFacts[]>
  listLogSegments(input: {
    readonly runId: string
  }): Promise<readonly PreviewLogSegmentFacts[]>
  readLogCitation(input: {
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
  citeLog(input: { segmentKey: string; lineStart: number; lineEnd: number }): Promise<void>
}

const DESKTOP_VIEWPORT: PreviewViewportFacts = Object.freeze({
  preset: 'desktop',
  width: 1280,
  height: 800,
})

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
    logSegments: Object.freeze([]) as readonly PreviewLogSegmentFacts[],
    logCitation: null,
    notice: null,
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
    identity.sourceId,
    identity.workerSessionId,
    identity.repositoryBindingId,
    identity.taskId ?? '',
    identity.attempt === null ? '' : String(identity.attempt),
    identity.candidateCommit ?? '',
    identity.candidateTreeId ?? '',
    identity.runConfigVersion ?? '',
  ].join('|')
}

export function createCandidatePreviewViewModel(
  options: CandidatePreviewViewModelOptions,
): CandidatePreviewViewModel {
  let state = initialState()
  const listeners = new Set<(next: CandidatePreviewState) => void>()
  const backStack: string[] = []
  const forwardStack: string[] = []
  let closed = false
  /** RUN-03: one live run identity; a second start never opens a second run. */
  let activeRunKey: string | null = null

  function emit(patch: Partial<CandidatePreviewState>): void {
    state = Object.freeze({ ...state, ...patch })
    for (const listener of listeners) listener(state)
  }

  function notice(text: string | null): void {
    emit({ notice: text })
  }

  async function ensureIdentity(): Promise<CandidatePreviewIdentity | null> {
    if (state.identity !== null) return state.identity
    const identity = await options.port.loadIdentity()
    if (closed) return null
    emit({ identity })
    return identity
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
        if (identity.mode === 'frozen-candidate' && identity.candidateCommit === null) {
          emit({
            status: 'error',
            notice: '冻结候选预览缺少提交标识，拒绝启动。',
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
      const identity = await ensureIdentity()
      if (identity === null) {
        notice('缺少候选运行身份，无法启动受管应用。')
        return
      }
      const key = identityFingerprint(identity)
      if (activeRunKey === key && state.run !== null && state.run.phase !== 'exited' && state.run.phase !== 'failed') {
        // RUN-03 idempotent start: a second click keeps the existing run.
        notice('该候选已有一个进行中的受管运行，不会重复启动。')
        return
      }
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
      } catch {
        if (closed) return
        notice('启动受管应用失败。')
      }
    },
    async stopRun() {
      if (closed) return
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
      } catch {
        if (closed) return
        notice('停止受管应用失败。')
      }
    },
    async restartRun() {
      if (closed) return
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
        const files = await options.port.listFiles({ sourceId: source.sourceId })
        if (closed) return
        emit({
          source,
          files: files.map(file => classifyPreviewFile(file.path)),
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
          sourceId: source.sourceId,
          requestId: options.nextRequestId(),
        })
        if (closed) return
        emit({ source: next, files: [], notice: '预览访问已撤销。' })
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
      if (!path.startsWith('/')) {
        emit({
          navigation: Object.freeze({
            ...state.navigation,
            lastError: '预览路径必须以 / 开头。',
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
    openFile(path) {
      if (closed) return
      const file = classifyPreviewFile(path)
      if (file.previewClass === 'degraded') {
        notice(file.degradationReason)
        return
      }
      // RUN-06: opening a file is in-frame navigation, not a new window.
      this.navigate(path)
      notice(null)
    },
    async citeLog(input) {
      if (closed) return
      const run = state.run
      if (run === null) {
        notice('尚未启动受管运行，无法引用日志。')
        return
      }
      try {
        const citation = await options.port.readLogCitation({
          runId: run.runId,
          segmentKey: input.segmentKey,
          lineStart: input.lineStart,
          lineEnd: input.lineEnd,
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
