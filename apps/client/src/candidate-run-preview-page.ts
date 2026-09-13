// SPDX-License-Identifier: Apache-2.0

import type {
  CandidatePreviewState,
  CandidatePreviewViewModel,
  ManagedRunPhase,
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

export interface CandidateRunPreviewPageOptions {
  readonly root: HTMLElement
  readonly model: CandidatePreviewViewModel
  readonly taskHref?: string
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
  const parts = [
    `source ${identity.sourceId}`,
    `task ${identity.taskId ?? '—'}`,
    identity.attempt === null ? null : `attempt ${String(identity.attempt)}`,
    `commit ${shortCommit(identity.candidateCommit)}`,
    `tree ${identity.candidateTreeId === null ? '—' : identity.candidateTreeId.slice(0, 7)}`,
    `config ${identity.runConfigVersion ?? '—'}`,
  ].filter((part): part is string => part !== null)
  return parts.join(' · ')
}

function runPhase(state: CandidatePreviewState): ManagedRunPhase {
  return state.run?.phase ?? 'idle'
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

  const runBar = element(document, 'div', 'wwc-candidate-run-preview-run-bar')
  const runBadge = element(document, 'span', 'wwc-candidate-run-preview-run-badge')
  const startButton = element(document, 'button', 'wwc-candidate-run-preview-start')
  const stopButton = element(document, 'button', 'wwc-candidate-run-preview-stop')
  const restartButton = element(document, 'button', 'wwc-candidate-run-preview-restart')

  const previewBar = element(document, 'div', 'wwc-candidate-run-preview-preview-bar')
  const authorizeButton = element(document, 'button', 'wwc-candidate-run-preview-authorize')
  const revokeButton = element(document, 'button', 'wwc-candidate-run-preview-revoke')
  const accessLine = element(document, 'p', 'wwc-candidate-run-preview-access')

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
  const frameNotice = element(document, 'p', 'wwc-candidate-run-preview-frame-notice')

  const fileList = element(document, 'ul', 'wwc-candidate-run-preview-files')
  const logList = element(document, 'ul', 'wwc-candidate-run-preview-logs')
  const citation = element(document, 'p', 'wwc-candidate-run-preview-citation')

  let closed = false

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
  customHeight.type = 'number'
  customHeight.min = '200'
  customHeight.max = '4096'
  customHeight.placeholder = '高'
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

  runBar.append(runBadge, startButton, stopButton, restartButton)
  previewBar.append(authorizeButton, revokeButton, accessLine)
  viewportBar.append(desktopButton, mobileButton, customWidth, customHeight, applyCustom, viewportLabel)
  navBar.append(backButton, forwardButton, refreshButton, pathInput, goButton, navError)
  frameShell.append(frameNotice)
  section.append(
    topbar,
    heading,
    modeLine,
    identityLine,
    noticeLine,
    runBar,
    previewBar,
    viewportBar,
    navBar,
    frameShell,
    fileList,
    logList,
    citation,
  )
  topbar.append(back)
  options.root.replaceChildren(section)

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
      item.append(open, badge)
      fileList.append(item)
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
        segment.stream,
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
    startButton.disabled = state.status !== 'ready' || (hasRun && phase !== 'exited' && phase !== 'failed' && phase !== 'idle')
    stopButton.disabled = !hasRun || phase === 'idle' || phase === 'exited'
    restartButton.disabled = !hasRun

    const access = state.source?.access ?? 'none'
    accessLine.textContent = state.source === null
      ? '尚未授权远程预览。'
      : access === 'authorized'
        ? `已授权 · ${state.source.previewOrigin}`
        : access === 'revoked'
          ? '访问已撤销。'
          : '无预览访问。'
    revokeButton.disabled = state.source === null || access !== 'authorized'
    authorizeButton.disabled = state.status !== 'ready' || access === 'authorized'

    viewportLabel.textContent = `视口 ${state.viewport.preset} · ${String(state.viewport.width)}×${String(state.viewport.height)}`
    pathInput.value = state.navigation.path
    backButton.disabled = !state.navigation.canGoBack
    forwardButton.disabled = !state.navigation.canGoForward
    navError.hidden = state.navigation.lastError === null
    navError.textContent = state.navigation.lastError ?? ''

    const frameReady = state.source?.access === 'authorized'
    frameNotice.hidden = frameReady
    frameNotice.textContent = frameReady
      ? ''
      : '受控预览帧在授权后加载；此处不内嵌任意本机地址。'
    frameShell.dataset.viewport = `${String(state.viewport.width)}x${String(state.viewport.height)}`
    frameShell.dataset.path = state.navigation.path
    frameShell.dataset.origin = state.source?.previewOrigin ?? ''

    renderFiles(state.files)
    renderLogs(state.logSegments)

    const logCitation = state.logCitation
    citation.hidden = logCitation === null
    citation.textContent = logCitation === null
      ? ''
      : `${logCitation.sourceRef} · ${String(logCitation.lineStart)}–${String(logCitation.lineEnd)} · ${logCitation.redactedText}`
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
