// SPDX-License-Identifier: Apache-2.0

import type {
  CandidatePreviewState,
  CandidatePreviewViewModel,
  ManagedRunPhase,
  PreviewFileContentState,
  PreviewFileFacts,
  PreviewLogSegmentFacts,
  PreviewViewportPreset,
} from './candidate-run-preview-view-model.js'
import {
  candidatePreviewAcceptanceEligible,
  candidatePreviewModeText,
  candidatePreviewModeTone,
  managedRunPhaseText,
  managedRunPhaseTone,
} from './candidate-run-preview-view-model.js'
import type { PageAnnotationViewModel } from './page-annotation-view-model.js'
import { mountPageAnnotationPage } from './page-annotation-page.js'
import { renderDiff } from './diff-preview.js'
import { redactPublicText } from './public-redaction.js'

export interface CandidateRunPreviewPageOptions {
  readonly root: HTMLElement
  readonly model: CandidatePreviewViewModel
  readonly taskHref?: string
  readonly annotations?: PageAnnotationViewModel
  readonly appOrigin?: string
  readonly devicePixelRatio?: number
  readonly deliveryId?: string
}

export interface CandidateRunPreviewPage {
  close(): void
}

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

function shortCommit(commit: string | null): string {
  return commit === null ? '—' : commit.slice(0, 7)
}

function modeBanner(state: CandidatePreviewState): string {
  if (state.identity === null) return '候选身份未知'
  const label = candidatePreviewModeText(state.identity.mode)
  if (state.identity.mode === 'live') {
    return `${label} · 不能作为冻结验收通过证明`
  }
  return candidatePreviewAcceptanceEligible(state.identity.mode)
    ? `${label} · 可作为验收预览来源`
    : label
}

function identityDetail(state: CandidatePreviewState): string {
  const identity = state.identity
  if (identity === null) return '等待候选运行身份'
  const run = identity.attempt === null ? '当前运行' : `第 ${String(identity.attempt)} 次运行`
  const version = identity.candidateCommit === null ? null : `版本 ${shortCommit(identity.candidateCommit)}`
  return ['当前任务', run, version].filter((part): part is string => part !== null).join(' · ')
}

function runPhase(state: CandidatePreviewState): ManagedRunPhase {
  return state.run?.phase ?? 'idle'
}

function annotationBindingKey(state: CandidatePreviewState): string {
  const identity = state.identity
  if (identity === null) return 'unknown'
  return [
    identity.mode,
    identity.sourceId,
    identity.workRunId,
    identity.repositoryBindingId,
    identity.taskId ?? '',
    identity.attempt === null ? '' : String(identity.attempt),
    identity.candidateCommit ?? '',
    identity.candidateTreeId ?? '',
  ].join('\u0000')
}

function fileClassText(file: PreviewFileFacts): string {
  switch (file.previewClass) {
    case 'markdown': return 'Markdown'
    case 'code': return '代码'
    case 'image': return '图片'
    case 'html': return 'HTML/SVG'
    case 'download': return '下载'
    case 'degraded': return '已降级'
  }
}

function logStreamText(stream: PreviewLogSegmentFacts['stream']): string {
  switch (stream) {
    case 'stdout': return '标准输出'
    case 'stderr': return '错误输出'
    case 'diagnostic': return '诊断'
  }
}

/**
 * WWX-RUN-04 main surface: one section that always shows mode, identity,
 * lease-bound run controls, authorized remote preview, viewport/navigation,
 * file list, and bounded log citations. DOM and ARIA only — every value comes
 * from the view-model snapshot.
 */
export function mountCandidateRunPreviewPage(
  options: CandidateRunPreviewPageOptions,
): CandidateRunPreviewPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-candidate-run-preview')
  const topbar = element(document, 'div', 'wwc-candidate-run-preview-topbar')
  const back = element(document, 'a', 'wwc-candidate-run-preview-back')
  const heading = element(document, 'h2', 'wwc-candidate-run-preview-heading')
  const modeLine = element(document, 'p', 'wwc-candidate-run-preview-mode')
  const identityLine = element(document, 'p', 'wwc-candidate-run-preview-identity')
  const noticeLine = element(document, 'p', 'wwc-candidate-run-preview-notice')
  noticeLine.setAttribute('role', 'status')

  const runHeading = element(document, 'h3', 'wwc-candidate-run-preview-section-heading')
  const runBar = element(document, 'div', 'wwc-candidate-run-preview-run-bar')
  const runBadge = element(document, 'span', 'wwc-candidate-run-preview-run-badge')
  const startButton = element(document, 'button', 'wwc-candidate-run-preview-start')
  const stopButton = element(document, 'button', 'wwc-candidate-run-preview-stop')
  const restartButton = element(document, 'button', 'wwc-candidate-run-preview-restart')

  const previewHeading = element(document, 'h3', 'wwc-candidate-run-preview-section-heading')
  const previewBar = element(document, 'div', 'wwc-candidate-run-preview-preview-bar')
  const authorizeButton = element(document, 'button', 'wwc-candidate-run-preview-authorize')
  const revokeButton = element(document, 'button', 'wwc-candidate-run-preview-revoke')
  const accessLine = element(document, 'p', 'wwc-candidate-run-preview-access')

  const viewportHeading = element(document, 'h3', 'wwc-candidate-run-preview-section-heading')
  const viewportBar = element(document, 'div', 'wwc-candidate-run-preview-viewport-bar')
  const desktopButton = element(document, 'button', 'wwc-candidate-run-preview-viewport-desktop')
  const mobileButton = element(document, 'button', 'wwc-candidate-run-preview-viewport-mobile')
  const customWidth = element(document, 'input', 'wwc-candidate-run-preview-viewport-width')
  const customHeight = element(document, 'input', 'wwc-candidate-run-preview-viewport-height')
  const applyCustom = element(document, 'button', 'wwc-candidate-run-preview-viewport-custom')
  const viewportLabel = element(document, 'p', 'wwc-candidate-run-preview-viewport-label')

  const navBar = element(document, 'div', 'wwc-candidate-run-preview-nav-bar')
  const backButton = element(document, 'button', 'wwc-candidate-run-preview-nav-back')
  const forwardButton = element(document, 'button', 'wwc-candidate-run-preview-nav-forward')
  const refreshButton = element(document, 'button', 'wwc-candidate-run-preview-nav-refresh')
  const pathInput = element(document, 'input', 'wwc-candidate-run-preview-nav-path')
  const goButton = element(document, 'button', 'wwc-candidate-run-preview-nav-go')
  const navError = element(document, 'p', 'wwc-candidate-run-preview-nav-error')

  const frameShell = element(document, 'div', 'wwc-candidate-run-preview-frame-shell')
  const frame = element(document, 'iframe', 'wwc-candidate-run-preview-frame')
  const frameNotice = element(document, 'p', 'wwc-candidate-run-preview-frame-notice')
  const openPreview = element(document, 'a', 'wwc-candidate-run-preview-open')

  const fileHeading = element(document, 'h3', 'wwc-candidate-run-preview-section-heading')
  const fileList = element(document, 'ul', 'wwc-candidate-run-preview-files')
  const fileContentHeading = element(document, 'h3', 'wwc-candidate-run-preview-section-heading')
  const fileContentPanel = element(document, 'div', 'wwc-candidate-run-preview-file-content')
  const diffHeading = element(document, 'h3', 'wwc-candidate-run-preview-section-heading')
  const diffPanel = element(document, 'div', 'wwc-candidate-run-preview-diff')
  const logHeading = element(document, 'h3', 'wwc-candidate-run-preview-section-heading')
  const logList = element(document, 'ul', 'wwc-candidate-run-preview-logs')
  const citation = element(document, 'p', 'wwc-candidate-run-preview-citation')
  const annotationRoot = element(document, 'div', 'wwc-candidate-run-preview-annotations')

  let closed = false
  let currentFrameUrl: string | null = null
  let frameLoading = false
  let frameLoadError = false

  section.setAttribute('aria-label', '候选运行预览')
  heading.textContent = '候选运行预览'
  heading.id = 'wwc-candidate-run-preview-heading'
  section.setAttribute('aria-labelledby', heading.id)
  back.textContent = '返回任务'
  if (options.taskHref !== undefined) back.href = options.taskHref
  else back.hidden = true

  startButton.type = 'button'
  startButton.textContent = '启动'
  stopButton.type = 'button'
  stopButton.textContent = '停止'
  restartButton.type = 'button'
  restartButton.textContent = '重启'
  authorizeButton.type = 'button'
  authorizeButton.textContent = '授权预览'
  revokeButton.type = 'button'
  revokeButton.textContent = '撤销访问'
  desktopButton.type = 'button'
  desktopButton.textContent = '桌面'
  mobileButton.type = 'button'
  mobileButton.textContent = '手机'
  applyCustom.type = 'button'
  applyCustom.textContent = '自定义视口'
  customWidth.type = 'number'
  customWidth.min = '200'
  customWidth.max = '4096'
  customWidth.placeholder = '宽'
  customWidth.setAttribute('aria-label', '自定义视口宽度')
  customHeight.type = 'number'
  customHeight.min = '200'
  customHeight.max = '4096'
  customHeight.placeholder = '高'
  customHeight.setAttribute('aria-label', '自定义视口高度')
  backButton.type = 'button'
  backButton.textContent = '后退'
  forwardButton.type = 'button'
  forwardButton.textContent = '前进'
  refreshButton.type = 'button'
  refreshButton.textContent = '刷新'
  goButton.type = 'button'
  goButton.textContent = '打开'
  pathInput.type = 'text'
  pathInput.placeholder = '/path'
  pathInput.setAttribute('aria-label', '预览路径')
  frame.title = '受控候选预览'
  noticeLine.setAttribute('role', 'status')
  frameNotice.setAttribute('role', 'status')
  fileContentPanel.tabIndex = -1
  diffPanel.tabIndex = -1
  frame.setAttribute('sandbox', 'allow-downloads allow-forms allow-modals allow-popups allow-scripts')
  frame.setAttribute('referrerpolicy', 'no-referrer')
  frame.addEventListener('load', () => {
    if (closed) return
    frameLoading = false
    frameLoadError = false
    delete frameNotice.dataset.tone
    frameNotice.hidden = true
    frameNotice.textContent = ''
  })
  frame.addEventListener('error', () => {
    if (closed) return
    frameLoading = false
    frameLoadError = true
    frameNotice.dataset.tone = 'danger'
    frameNotice.hidden = false
    frameNotice.textContent = '预览加载失败。请刷新，或检查受控运行和短期访问是否仍有效。'
  })
  openPreview.textContent = '在新窗口打开'
  openPreview.target = '_blank'
  openPreview.rel = 'noopener noreferrer'

  runBar.append(runBadge, startButton, stopButton, restartButton)
  runHeading.textContent = '运行控制'
  previewBar.append(authorizeButton, revokeButton, accessLine, openPreview)
  previewHeading.textContent = '受控预览访问'
  viewportBar.append(desktopButton, mobileButton, customWidth, customHeight, applyCustom, viewportLabel)
  viewportHeading.textContent = '视口与导航'
  navBar.append(backButton, forwardButton, refreshButton, pathInput, goButton, navError)
  frameShell.append(frame, frameNotice)
  fileHeading.textContent = '授权文件'
  logHeading.textContent = '运行日志'
  section.append(
    topbar,
    heading,
    modeLine,
    identityLine,
    noticeLine,
    runHeading,
    runBar,
    previewHeading,
    previewBar,
    viewportHeading,
    viewportBar,
    navBar,
    frameShell,
    fileHeading,
    fileList,
    fileContentHeading,
    fileContentPanel,
    diffHeading,
    diffPanel,
    logHeading,
    logList,
    citation,
    annotationRoot,
  )
  topbar.append(back)
  options.root.replaceChildren(section)
  const annotationPage = options.annotations === undefined
    ? null
    : mountPageAnnotationPage({ root: annotationRoot, model: options.annotations })
  annotationRoot.hidden = annotationPage === null
  let annotationSurface = ''

  function renderFiles(files: readonly PreviewFileFacts[]): void {
    fileList.replaceChildren()
    if (files.length === 0) {
      const empty = element(document, 'li', 'wwc-candidate-run-preview-files-empty')
      empty.textContent = '尚未授权文件列表，或列表为空。'
      fileList.append(empty)
      return
    }
    for (const file of files) {
      const item = element(document, 'li', 'wwc-candidate-run-preview-file')
      const open = element(document, 'button', 'wwc-candidate-run-preview-file-open')
      open.type = 'button'
      open.textContent = file.path
      open.disabled = file.previewClass === 'degraded'
      if (file.degradationReason !== null) open.title = file.degradationReason
      open.addEventListener('click', () => {
        void options.model.openFile(file.path)
      })
      const badge = element(document, 'span', 'wwc-candidate-run-preview-file-class')
      badge.textContent = fileClassText(file)
      const diff = element(document, 'button', 'wwc-candidate-run-preview-file-diff')
      diff.type = 'button'
      diff.textContent = '查看变更 diff'
      const canDiff = file.previewClass === 'code' || file.previewClass === 'markdown'
      diff.disabled = !canDiff
      if (!canDiff) diff.title = file.degradationReason ?? '该文件不支持内联变更 diff。'
      diff.addEventListener('click', () => {
        void options.model.openDiff(file.path)
      })
      item.append(open, badge, diff)
      fileList.append(item)
    }
  }

  function renderFileContent(content: PreviewFileContentState | null): void {
    fileContentHeading.hidden = content === null
    fileContentPanel.hidden = content === null
    fileContentPanel.replaceChildren()
    if (content === null) return
    const label = element(document, 'p', 'wwc-candidate-run-preview-file-content-label')
    label.textContent = `${content.path} · 文件内容`
    fileContentPanel.append(label)
    if (content.loading) {
      const loading = element(document, 'p', 'wwc-candidate-run-preview-file-content-status')
      loading.setAttribute('role', 'status')
      loading.textContent = '正在读取有界文件内容…'
      fileContentPanel.append(loading)
      return
    }
    if (content.error !== null) {
      const error = element(document, 'p', 'wwc-candidate-run-preview-file-content-status')
      error.setAttribute('role', 'alert')
      error.dataset.tone = 'danger'
      error.textContent = `文件内容读取失败：${content.error}`
      fileContentPanel.append(error)
      if (content.retryable) {
        const retry = element(document, 'button', 'wwc-candidate-run-preview-file-content-retry')
        retry.type = 'button'
        retry.textContent = '重试读取文件'
        retry.addEventListener('click', () => {
          fileContentPanel.focus?.()
          void options.model.openFile(content.path)
        })
        fileContentPanel.append(retry)
      }
      return
    }
    let binary = ''
    try {
      binary = atob(content.dataBase64)
    } catch {
      binary = ''
    }
    const bytes = Uint8Array.from(binary, character => character.charCodeAt(0))
    const file = stateFile(content.path)
    if (file?.previewClass === 'html') {
      const safe = element(document, 'p', 'wwc-candidate-run-preview-file-content-status')
      safe.textContent = 'HTML/SVG 只可在独立受控预览来源中渲染；此处不执行文件内容。'
      fileContentPanel.append(safe)
    } else if (file?.previewClass === 'image'
      && content.nextOffset === null
      && /^image\/(?:png|jpeg|gif|webp|avif)$/u.test(content.mediaType)) {
      const image = element(document, 'img', 'wwc-candidate-run-preview-file-content-image')
      image.alt = content.path
      image.src = `data:${content.mediaType};base64,${content.dataBase64}`
      fileContentPanel.append(image)
    } else if (content.contentEncoding === 'utf-8'
      && (file?.previewClass === 'markdown' || file?.previewClass === 'code')) {
      const pre = element(document, 'pre', 'wwc-candidate-run-preview-file-content-code')
      pre.tabIndex = 0
      pre.textContent = new TextDecoder('utf-8', { fatal: false }).decode(bytes)
      fileContentPanel.append(pre)
    } else {
      const binaryNotice = element(document, 'p', 'wwc-candidate-run-preview-file-content-status')
      binaryNotice.textContent = '该文件按二进制内容处理，只提供安全下载。'
      fileContentPanel.append(binaryNotice)
    }
    const download = element(document, 'a', 'wwc-candidate-run-preview-file-content-download')
    download.textContent = content.nextOffset === null ? '下载文件' : '下载已读取内容'
    download.href = `data:application/octet-stream;base64,${content.dataBase64}`
    download.download = content.path.split('/').at(-1) ?? 'candidate-file'
    download.rel = 'noopener noreferrer'
    fileContentPanel.append(download)
    const meta = element(document, 'p', 'wwc-candidate-run-preview-file-content-meta')
    meta.textContent = content.nextOffset === null
      ? `已读取 ${String(content.returnedBytes)} / ${String(content.totalBytes)} 字节`
      : `已读取 ${String(content.returnedBytes)} / ${String(content.totalBytes)} 字节；文件过大，下载当前有界内容`
    fileContentPanel.append(meta)
  }

  function stateFile(path: string): PreviewFileFacts | undefined {
    return options.model.state.files.find(file => file.path === path)
  }

  function renderDiffPanel(state: CandidatePreviewState): void {
    const diff = state.fileDiff
    diffHeading.hidden = diff === null
    diffPanel.hidden = diff === null
    diffPanel.replaceChildren()
    if (diff === null) return
    const label = element(document, 'p', 'wwc-candidate-run-preview-diff-label')
    label.textContent = `${diff.path} · 变更 diff（不是原始文件内容）`
    diffPanel.append(label)
    if (diff.loading) {
      const loading = element(document, 'p', 'wwc-candidate-run-preview-diff-status')
      loading.setAttribute('role', 'status')
      loading.textContent = '正在读取候选变更 diff…'
      diffPanel.append(loading)
      return
    }
    if (diff.error !== null) {
      const error = element(document, 'p', 'wwc-candidate-run-preview-diff-status')
      error.setAttribute('role', 'alert')
      error.dataset.tone = 'danger'
      error.textContent = `变更 diff 读取失败：${diff.error}`
      diffPanel.append(error)
      if (diff.retryable) {
        const retry = element(document, 'button', 'wwc-candidate-run-preview-diff-retry')
        retry.type = 'button'
        retry.textContent = '重试读取 diff'
        retry.addEventListener('click', () => {
          diffPanel.focus?.()
          void options.model.openDiff(diff.path)
        })
        diffPanel.append(retry)
      }
      return
    }
    const pre = element(document, 'pre', 'wwc-candidate-run-preview-diff-code')
    pre.tabIndex = 0
    pre.setAttribute('aria-label', `${diff.path} 的变更 diff，减号为删除，加号为新增`)
    pre.append(renderDiff(document, diff.text))
    diffPanel.append(pre)
    const meta = element(document, 'p', 'wwc-candidate-run-preview-diff-meta')
    meta.textContent = `已读取 ${String(diff.returnedBytes)} / ${String(diff.totalBytes)} 字节`
    diffPanel.append(meta)
    if (diff.nextOffset !== null) {
      const more = element(document, 'button', 'wwc-candidate-run-preview-diff-more')
      more.type = 'button'
      more.textContent = '继续读取变更 diff'
      more.addEventListener('click', () => { void options.model.continueDiff() })
      diffPanel.append(more)
    }
  }

  function renderLogs(segments: readonly PreviewLogSegmentFacts[]): void {
    logList.replaceChildren()
    if (segments.length === 0) {
      const empty = element(document, 'li', 'wwc-candidate-run-preview-logs-empty')
      empty.textContent = '受管运行尚未产生日志分段。'
      logList.append(empty)
      return
    }
    for (const segment of segments) {
      const item = element(document, 'li', 'wwc-candidate-run-preview-log')
      const label = element(document, 'span', 'wwc-candidate-run-preview-log-label')
      label.textContent = [
        logStreamText(segment.stream),
        `${String(segment.lineCount)} 行`,
        segment.truncated ? `已截断至 ${String(segment.maxLines)}` : '完整',
      ].join(' · ')
      const cite = element(document, 'button', 'wwc-candidate-run-preview-log-cite')
      cite.type = 'button'
      cite.textContent = '引用前 3 行'
      cite.addEventListener('click', () => {
        void options.model.citeLog({
          segmentKey: segment.key,
          lineStart: 1,
          lineEnd: Math.min(3, segment.lineCount),
        })
      })
      item.append(label, cite)
      logList.append(item)
    }
  }

  function render(state: CandidatePreviewState): void {
    if (closed) return
    const mode = state.identity?.mode
    modeLine.textContent = modeBanner(state)
    modeLine.dataset.tone = mode === undefined
      ? 'neutral'
      : candidatePreviewModeTone(mode)
    identityLine.textContent = identityDetail(state)
    noticeLine.hidden = state.notice === null
    noticeLine.textContent = state.notice ?? ''

    const phase = runPhase(state)
    runBadge.textContent = managedRunPhaseText(phase)
    runBadge.dataset.tone = managedRunPhaseTone(phase)
    const hasRun = state.run !== null
    startButton.disabled = !state.runControlAvailable || state.status !== 'ready' || (hasRun && phase !== 'exited' && phase !== 'failed' && phase !== 'idle')
    stopButton.disabled = !state.runControlAvailable || !hasRun || phase === 'idle' || phase === 'exited'
    restartButton.disabled = !state.runControlAvailable || !hasRun
    const runControlTitle = state.runControlAvailable ? '' : '等待本地设备提供受管应用控制能力'
    startButton.title = runControlTitle
    stopButton.title = runControlTitle
    restartButton.title = runControlTitle

    const access = state.source?.access ?? 'none'
    accessLine.textContent = state.source === null
      ? '尚未授权远程预览。'
      : access === 'authorized'
        ? '已授权 · 短期访问'
        : access === 'revoked'
          ? '访问已撤销。'
          : '无预览访问。'
    revokeButton.disabled = state.source === null || access !== 'authorized'
    authorizeButton.disabled = state.status !== 'ready' || access === 'authorized'

    viewportLabel.textContent = `视口 ${state.viewport.preset} · ${String(state.viewport.width)}×${String(state.viewport.height)}`
    desktopButton.setAttribute('aria-pressed', String(state.viewport.preset === 'desktop'))
    mobileButton.setAttribute('aria-pressed', String(state.viewport.preset === 'mobile'))
    applyCustom.setAttribute('aria-pressed', String(state.viewport.preset === 'custom'))
    customWidth.value = state.viewport.preset === 'custom' ? String(state.viewport.width) : customWidth.value
    customHeight.value = state.viewport.preset === 'custom' ? String(state.viewport.height) : customHeight.value
    pathInput.value = state.navigation.path
    backButton.disabled = !state.navigation.canGoBack
    forwardButton.disabled = !state.navigation.canGoForward
    navError.hidden = state.navigation.lastError === null
    navError.textContent = state.navigation.lastError ?? ''

    const frameReady = state.source?.access === 'authorized'
    frameShell.dataset.viewport = `${String(state.viewport.width)}x${String(state.viewport.height)}`
    frameShell.dataset.path = state.navigation.path
    frameShell.setAttribute('style', `width:${String(state.viewport.width)}px;max-width:100%;height:${String(state.viewport.height)}px`)
    const frameUrl = frameReady && state.source !== null
      ? new URL(state.navigation.path.replace(/^\//u, ''), state.source.previewOrigin).toString()
      : null
    if (frameUrl === null) {
      currentFrameUrl = null
      frameLoading = false
      frameLoadError = false
      delete frameNotice.dataset.tone
      frame.removeAttribute('src')
      frame.hidden = true
      openPreview.hidden = true
    } else {
      if (currentFrameUrl !== frameUrl) {
        currentFrameUrl = frameUrl
        frameLoading = true
        frameLoadError = false
      }
      if (frame.src !== frameUrl) frame.src = frameUrl
      frame.hidden = false
      openPreview.hidden = false
      openPreview.href = frameUrl
    }
    frameNotice.hidden = frameReady && !frameLoading && !frameLoadError
    frameNotice.textContent = frameReady
      ? frameLoadError
        ? '预览加载失败。请刷新，或检查受控运行和短期访问是否仍有效。'
        : frameLoading
          ? '正在加载受控预览…'
          : ''
      : '受控预览帧在授权后加载；此处不内嵌任意本机地址。'
    if (!frameReady || !frameLoadError) delete frameNotice.dataset.tone
    if (options.annotations !== undefined && state.source !== null) {
      const nextSurface = [
        state.source.previewAccessId,
        state.source.access,
        state.navigation.path,
        state.viewport.width,
        state.viewport.height,
      ].join('|')
      if (annotationSurface !== nextSurface) {
        annotationSurface = nextSurface
        options.annotations.prepare({
          bindingKey: annotationBindingKey(state),
          pageUrl: frameUrl ?? options.appOrigin ?? 'about:blank',
          pagePath: state.navigation.path,
          access: state.source.access === 'authorized' ? 'authorized' : 'revoked',
          injectable: false,
          ...(options.deliveryId === undefined ? {} : { deliveryId: options.deliveryId }),
          ...(state.identity?.workRunId === undefined ? {} : { workRunId: state.identity.workRunId }),
          viewport: {
            width: state.viewport.width,
            height: state.viewport.height,
            devicePixelRatio: options.devicePixelRatio ?? 1,
          },
        })
      }
    }

    renderFiles(state.files)
    fileContentHeading.textContent = '文件内容'
    renderFileContent(state.fileContent)
    diffHeading.textContent = '变更 diff'
    renderDiffPanel(state)
    renderLogs(state.logSegments)

    const logCitation = state.logCitation
    citation.hidden = logCitation === null
    citation.title = logCitation === null ? '' : '来源已绑定当前运行'
    citation.textContent = logCitation === null
      ? ''
      : `运行日志 · 第 ${String(logCitation.lineStart)}–${String(logCitation.lineEnd)} 行 · ${redactPublicText(logCitation.redactedText)}`
  }

  startButton.addEventListener('click', () => {
    void options.model.startRun()
  })
  stopButton.addEventListener('click', () => {
    void options.model.stopRun()
  })
  restartButton.addEventListener('click', () => {
    void options.model.restartRun()
  })
  authorizeButton.addEventListener('click', () => {
    void options.model.authorizePreview()
  })
  revokeButton.addEventListener('click', () => {
    void options.model.revokePreview()
  })
  desktopButton.addEventListener('click', () => {
    options.model.setViewport('desktop')
  })
  mobileButton.addEventListener('click', () => {
    options.model.setViewport('mobile')
  })
  applyCustom.addEventListener('click', () => {
    const width = Number(customWidth.value)
    const height = Number(customHeight.value)
    if (!Number.isFinite(width) || !Number.isFinite(height)) return
    options.model.setViewport('custom', { width, height })
  })
  backButton.addEventListener('click', () => {
    options.model.goBack()
  })
  forwardButton.addEventListener('click', () => {
    options.model.goForward()
  })
  refreshButton.addEventListener('click', () => {
    options.model.refresh()
    if (!frame.hidden) frame.src = frame.src
  })
  goButton.addEventListener('click', () => {
    options.model.navigate(pathInput.value)
  })

  const unsubscribe = options.model.subscribe(render)
  render(options.model.state)
  void options.model.start()

  return {
    close() {
      closed = true
      unsubscribe()
      annotationPage?.close()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}

export function candidateRunPreviewViewportPresetOf(
  state: CandidatePreviewState,
): PreviewViewportPreset {
  return state.viewport.preset
}
