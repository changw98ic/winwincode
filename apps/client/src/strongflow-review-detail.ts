// SPDX-License-Identifier: Apache-2.0

import {
  retentionLabel,
  mountStrongFlowReviewAnnotations,
  type StrongFlowReviewAnnotations,
  type StrongFlowReviewAnnotationsPanel,
  type StrongFlowReviewAttentionAnchor,
} from './strongflow-review-annotations.js'
import type {
  ReviewActivitySegment,
  ReviewEvidenceArtifact,
  SolutionReviewAction,
  StrongFlowReviewStatus,
  StrongFlowReviewViewModel,
  StrongFlowReviewState,
} from './strongflow-review-view-model.js'

export interface StrongFlowReviewDetailOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowReviewViewModel
  readonly annotations: StrongFlowReviewAnnotations
  /**
   * Text previews download as plain text through this seam. Production wires a
   * Blob download; tests capture the payload instead of touching the network.
   */
  readonly onDownload?: (fileName: string, bytes: Uint8Array, mediaType: string) => void
}

export interface StrongFlowReviewDetailPage {
  readonly root: HTMLElement
  close(): void
}

export const STRONGFLOW_REVIEW_STATUS_LABEL: Readonly<
  Record<StrongFlowReviewStatus, string>
> = Object.freeze({
  idle: '尚未读取',
  loading: '正在读取审核产物…',
  ready: '审核产物已更新',
  refreshing: '正在刷新审核产物…',
  'authentication-required': '登录后查看审核产物',
  'authorization-denied': '当前范围无权查看审核产物',
  error: '上次读取失败',
  closed: '审核面板已关闭',
})

const VERDICT_LABEL: Readonly<Record<string, string>> = Object.freeze({
  pass: '通过',
  fail: '未通过',
  inconclusive: '无法定论',
  infra_error: '基础设施错误',
  pending: '未判定',
})

const AVAILABILITY_LABEL: Readonly<Record<string, string>> = Object.freeze({
  available: '保留中',
  released: '证据现已不可取',
})

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

function button(
  document: Document,
  className: string,
  label: string,
  onClick: () => void,
): HTMLButtonElement {
  const node = element(document, 'button', className)
  node.type = 'button'
  node.textContent = label
  node.addEventListener('click', onClick)
  return node
}

function fileNameFor(path: string): string {
  const base = path.split('/').at(-1) ?? 'candidate-preview'
  return `${base}.diff.txt`
}

/**
 * VER-12 bounded delivery export. Only schema-owned identities, enums and
 * counts leave the page; free-form descriptions, findings, source refs, logs
 * and paths deliberately remain in the authorized interactive view.
 */
export function deliveryReportText(state: StrongFlowReviewState): string | null {
  const detail = state.detail
  if (detail === null) return null
  const results = new Map((detail.verdict?.criteria ?? []).map(result => [result.criterionId, result]))
  const criteria = detail.requirements.acceptanceCriteria ?? []
  const requiredRisks = criteria.filter(criterion => (
    criterion.required && results.get(criterion.id)?.verdict !== 'pass'
  )).length
  const candidateTree = detail.currentCandidate?.candidateTreeId
  const safeTree = candidateTree !== undefined && /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u.test(candidateTree)
    ? candidateTree
    : 'none'
  const evidenceCounts = new Map<string, number>()
  for (const entry of detail.evidence) {
    evidenceCounts.set(entry.type, (evidenceCounts.get(entry.type) ?? 0) + 1)
  }
  const evidenceSummary = [...evidenceCounts]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([type, count]) => `${type}=${String(count)}`)
    .join(', ') || 'none'

  return [
    'WinWinCode Delivery Report',
    `Delivery: ${detail.deliveryId}`,
    `Revision: ${String(detail.deliveryRevision)}`,
    `Final candidate tree: ${safeTree}`,
    `Verdict: ${detail.verdict?.status ?? 'pending'}`,
    '',
    'Acceptance criteria:',
    ...criteria.map((criterion, index) => {
      const result = results.get(criterion.id)
      return `${String(index + 1)}. ${criterion.required ? 'required' : 'optional'}; `
        + `result=${result?.verdict ?? 'pending'}; evidence=${String(result?.evidenceRefs.length ?? 0)}; `
        + `verification=${criterion.verificationMethod === null ? 'unmapped' : 'mapped'}`
    }),
    '',
    `Evidence summary: ${evidenceSummary}`,
    `Residual risk: required_not_pass=${String(requiredRisks)}; `
      + `unresolved_findings=${String(detail.verdict?.unresolvedFindings.length ?? 0)}`,
  ].join('\n')
}

/**
 * RUN-07 safe preview rendering. Text chunks are the only bytes ever placed in
 * the DOM, always through `textContent`; degraded classes render their reason
 * instead of content, so HTML/SVG and binary never enter the page.
 */
function renderPreview(
  document: Document,
  file: StrongFlowReviewState['files'][number],
  options: StrongFlowReviewDetailOptions,
): HTMLElement {
  const box = element(document, 'div', 'wwc-review-preview')
  const preview = file.preview
  box.dataset.reviewPath = file.path
  box.dataset.reviewClass = preview?.previewClass ?? 'none'
  box.dataset.reviewDegraded = preview === null || preview.previewClass !== 'text'
    ? 'true'
    : 'false'
  if (preview === null) {
    const empty = element(document, 'p', 'wwc-review-preview-empty')
    empty.textContent = file.previewError ?? '尚未读取该文件预览。'
    box.append(empty)
    return box
  }
  if (preview.previewClass !== 'text') {
    const degraded = element(document, 'p', 'wwc-review-preview-degraded')
    degraded.dataset.degraded = 'true'
    degraded.textContent = preview.degradationReason ?? '该内容已降级，仅保留元数据。'
    box.append(degraded)
  } else {
    const pre = element(document, 'pre', 'wwc-review-preview-text')
    const code = element(document, 'code', 'wwc-review-preview-code')
    code.textContent = preview.chunks.map(chunk => chunk.text).join('')
    pre.append(code)
    box.append(pre)
  }
  const meta = element(document, 'p', 'wwc-review-preview-meta')
  meta.textContent = `已读取 ${preview.returnedBytes} / ${preview.totalBytes} 字节`
  box.append(meta)
  if (preview.nextOffset !== null) {
    box.append(button(
      document,
      'wwc-review-preview-continue',
      '继续读取',
      () => {
        void options.model.continuePreview(file.path)
      },
    ))
  }
  if (preview.previewClass === 'text' && options.onDownload !== undefined) {
    box.append(button(
      document,
      'wwc-review-preview-download',
      '下载文本',
      () => {
        const payload = options.model.previewText(file.path)
        if (payload !== null) options.onDownload?.(
          fileNameFor(file.path),
          new TextEncoder().encode(payload),
          'text/plain;charset=utf-8',
        )
      },
    ))
  }
  if (file.previewError !== null) {
    const error = element(document, 'p', 'wwc-review-preview-error')
    error.textContent = `预览读取失败：${file.previewError}`
    box.append(error)
  }
  return box
}

function renderFileRows(
  document: Document,
  state: StrongFlowReviewState,
  files: HTMLElement,
  options: StrongFlowReviewDetailOptions,
): void {
  files.replaceChildren()
  if (state.detail === null || state.detail.currentCandidate === null) {
    const empty = element(document, 'li', 'wwc-review-file-empty')
    empty.textContent = '当前没有可审阅的 Candidate 产物。'
    files.append(empty)
    return
  }
  if (state.files.length === 0) {
    const empty = element(document, 'li', 'wwc-review-file-empty')
    empty.textContent = '当前 Candidate 没有变更文件。'
    files.append(empty)
    return
  }
  if (state.filesTruncated) {
    const note = element(document, 'li', 'wwc-review-file-more')
    note.append('文件列表已到达单页上限。', button(
      document,
      'wwc-review-file-continue',
      '继续读取文件列表',
      () => { void options.model.continueFiles() },
    ))
    files.append(note)
  }
  for (const file of state.files) {
    const item = element(document, 'li', 'wwc-review-file')
    item.dataset.reviewPath = file.path
    item.dataset.reviewStatus = file.status
    item.dataset.reviewEncoding = file.encoding
    const head = element(document, 'div', 'wwc-review-file-head')
    const pathNode = element(document, 'span', 'wwc-review-file-path')
    pathNode.textContent = file.path
    const fact = element(document, 'span', 'wwc-review-file-fact')
    fact.textContent = `${file.status} · ${file.encoding}`
    head.append(pathNode, fact, button(
      document,
      'wwc-review-file-open',
      '预览',
      () => {
        void options.model.openPreview(file.path)
      },
    ), button(
      document,
      'wwc-review-file-pin',
      '固定',
      () => options.annotations.pin({
        key: `file:${file.path}`,
        kind: 'file',
        label: `文件 ${file.path}`,
        retention: state.history.find(entry => entry.isCurrentAtReadCursor)?.availability ?? null,
      }),
    ))
    item.append(head, renderPreview(document, file, options))
    files.append(item)
  }
}

function renderSegments(
  document: Document,
  state: StrongFlowReviewState,
  segments: HTMLElement,
  selectedAttention: StrongFlowReviewAttentionAnchor | null,
  options: StrongFlowReviewDetailOptions,
): void {
  segments.replaceChildren()
  if (state.segments.length === 0) {
    const empty = element(document, 'li', 'wwc-review-segment-empty')
    empty.textContent = '当前范围没有可展示的运行活动。'
    segments.append(empty)
    return
  }
  for (const segment of state.segments) {
    segments.append(renderSegment(document, segment, selectedAttention, options))
  }
}

function renderSegment(
  document: Document,
  segment: ReviewActivitySegment,
  selectedAttention: StrongFlowReviewAttentionAnchor | null,
  options: StrongFlowReviewDetailOptions,
): HTMLElement {
  const item = element(document, 'li', 'wwc-review-segment')
  item.dataset.reviewSegmentKey = segment.key
  const head = element(document, 'p', 'wwc-review-segment-head')
  head.textContent = `会话 ${segment.productSessionId} · 尝试 ${segment.attempt}`
    + (segment.workRunId === null ? '' : ` · WorkRun ${segment.workRunId}`)
    + (segment.truncated ? ' · 已达单会话活动上限（仅展示前 100 条）' : '')
  const list = element(document, 'ul', 'wwc-review-activities')
  if (segment.activities.length === 0) {
    const empty = element(document, 'li', 'wwc-review-activity-empty')
    empty.textContent = '该会话暂无活动。'
    list.append(empty)
  }
  for (const activity of segment.activities) {
    const row = element(document, 'li', 'wwc-review-activity')
    row.dataset.reviewCallId = activity.callId
    row.dataset.reviewOutcome = activity.outcome
    row.dataset.reviewSourceRef = activity.sourceRef
    const command = element(document, 'span', 'wwc-review-activity-command')
    command.textContent = activity.command ?? `(${activity.activityType})`
    const fact = element(document, 'span', 'wwc-review-activity-fact')
    fact.textContent = `${activity.status} · outcome ${activity.outcome}`
      + (activity.exitCode === null ? '' : ` · 退出码 ${activity.exitCode}`)
    const source = element(document, 'span', 'wwc-review-activity-source')
    source.textContent = `来源 ${activity.sourceRef}`
    row.append(command, fact, source)
    if (activity.outcome === 'task-failed' || activity.outcome === 'timed-out') {
      row.append(button(
        document,
        'wwc-review-activity-cite',
        '引用到 Attention',
        () => {
          const snippet = options.model.snippetFor(segment.key, activity.callId)
          if (snippet === null) return
          options.annotations.attachSnippet(selectedAttention, snippet)
        },
      ))
    }
    list.append(row)
  }
  item.append(head, list)
  return item
}

function renderEvidenceArtifact(
  document: Document,
  evidenceId: string,
  artifact: ReviewEvidenceArtifact,
  options: StrongFlowReviewDetailOptions,
): HTMLElement {
  const box = element(document, 'div', 'wwc-review-artifact')
  box.dataset.reviewArtifactId = artifact.descriptor.artifactId
  box.dataset.reviewArtifactClass = artifact.previewClass
  const name = element(document, 'p', 'wwc-review-artifact-name')
  name.textContent = artifact.descriptor.fileName ?? artifact.descriptor.artifactId
  const facts = element(document, 'p', 'wwc-review-artifact-facts')
  facts.textContent = `${artifact.descriptor.kind} · ${artifact.descriptor.mediaType} · ${String(artifact.totalBytes)} 字节`
  box.append(name, facts)

  if (artifact.previewClass === 'inline-text' && artifact.chunks.length > 0) {
    const preview = element(document, 'pre', 'wwc-review-artifact-preview')
    const code = element(document, 'code', 'wwc-review-artifact-preview-code')
    code.textContent = artifact.chunks.map(chunk => chunk.text ?? '').join('')
    preview.append(code)
    box.append(preview)
  } else if (artifact.previewClass !== 'inline-text') {
    const degraded = element(document, 'p', 'wwc-review-artifact-degraded')
    degraded.dataset.degraded = 'true'
    degraded.textContent = artifact.previewClass === 'executable-document'
      ? 'HTML/SVG 不与管理界面同源执行；内容仅可按原媒体类型下载。'
      : '二进制或不支持内联的内容不进入页面；仅提供有界读取后下载。'
    box.append(degraded)
  }

  if (artifact.chunks.length > 0) {
    const progress = element(document, 'p', 'wwc-review-artifact-progress')
    progress.textContent = `已读取 ${String(artifact.returnedBytes)} / ${String(artifact.totalBytes)} 字节`
    box.append(progress)
  }
  box.append(button(
    document,
    'wwc-review-artifact-pin',
    '固定',
    () => options.annotations.pin({
      key: `artifact:${artifact.descriptor.artifactId}`,
      kind: 'artifact',
      label: `产物 ${artifact.descriptor.fileName ?? artifact.descriptor.artifactId}`,
      retention: 'available',
    }),
  ))
  if (artifact.chunks.length === 0) {
    box.append(button(
      document,
      'wwc-review-artifact-open',
      artifact.previewClass === 'inline-text' ? '预览' : '读取下载内容',
      () => { void options.model.openArtifact(evidenceId, artifact.descriptor.artifactId) },
    ))
  } else if (artifact.nextOffset !== null) {
    box.append(button(
      document,
      'wwc-review-artifact-continue',
      '继续读取',
      () => { void options.model.continueArtifact(evidenceId, artifact.descriptor.artifactId) },
    ))
  } else if (options.onDownload !== undefined) {
    box.append(button(
      document,
      'wwc-review-artifact-download',
      '下载',
      () => {
        const download = options.model.artifactDownload(evidenceId, artifact.descriptor.artifactId)
        if (download !== null) {
          options.onDownload?.(download.fileName, download.bytes, download.mediaType)
        }
      },
    ))
  }
  if (artifact.error !== null) {
    const error = element(document, 'p', 'wwc-review-artifact-error')
    error.textContent = `产物读取失败：${artifact.error}`
    box.append(error)
  }
  return box
}

/** RUN-07/VER-07 review surface: real Control Plane projections, safe degradation. */
export function mountStrongFlowReviewDetail(
  options: StrongFlowReviewDetailOptions,
): StrongFlowReviewDetailPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-review')
  section.setAttribute('aria-labelledby', 'wwc-review-title')
  const heading = element(document, 'h2', 'wwc-review-heading')
  heading.id = 'wwc-review-title'
  heading.textContent = 'StrongFlow 审核'
  const statusNode = element(document, 'p', 'wwc-review-status')
  const errorNode = element(document, 'p', 'wwc-review-error')
  errorNode.hidden = true
  const summary = element(document, 'div', 'wwc-review-summary')
  const solutionReview = element(document, 'section', 'wwc-review-solution')
  solutionReview.setAttribute('aria-labelledby', 'wwc-review-solution-heading')
  const solutionHeading = element(document, 'h3', 'wwc-review-solution-heading')
  solutionHeading.id = 'wwc-review-solution-heading'
  solutionHeading.textContent = '方案审核'
  const solutionWhy = element(document, 'p', 'wwc-review-solution-why')
  const solutionChecks = element(document, 'p', 'wwc-review-solution-checks')
  const solutionSummary = element(document, 'p', 'wwc-review-solution-summary')
  const solutionApproach = element(document, 'ul', 'wwc-review-solution-approach')
  const solutionRisks = element(document, 'ul', 'wwc-review-solution-risks')
  const solutionNoteLabel = element(document, 'label', 'wwc-review-solution-note-label')
  solutionNoteLabel.htmlFor = 'wwc-review-solution-note'
  solutionNoteLabel.textContent = '审核意见（要求修改时每行一项）'
  const solutionNote = element(document, 'textarea', 'wwc-review-solution-note')
  solutionNote.id = 'wwc-review-solution-note'
  solutionNote.rows = 4
  solutionNote.maxLength = 2_000
  const solutionMessage = element(document, 'p', 'wwc-review-solution-message')
  solutionMessage.setAttribute('aria-live', 'polite')
  const solutionActions = element(document, 'div', 'wwc-review-solution-actions')
  let solutionBusy = false
  let solutionSubmitted = false
  let solutionIdentity: string | null = null

  const decisionButton = (label: string, action: SolutionReviewAction): HTMLButtonElement => button(
    document,
    `wwc-review-solution-action wwc-review-solution-${action.replace('_', '-')}`,
    label,
    () => { void submitSolutionDecision(action) },
  )
  const approveSolution = decisionButton('批准方案', 'approve')
  const changeSolution = decisionButton('要求修改', 'request_changes')
  const rejectSolution = decisionButton('拒绝方案', 'reject')
  solutionActions.append(approveSolution, changeSolution, rejectSolution)
  solutionReview.append(
    solutionHeading,
    solutionWhy,
    solutionChecks,
    solutionSummary,
    solutionApproach,
    solutionRisks,
    solutionNoteLabel,
    solutionNote,
    solutionActions,
    solutionMessage,
  )
  const progress = element(document, 'section', 'wwc-review-progress')
  progress.setAttribute('aria-label', '任务进度')
  const criteriaHeading = element(document, 'h3', 'wwc-review-criteria-heading')
  criteriaHeading.textContent = '逐项验收结果'
  const criteriaList = element(document, 'ol', 'wwc-review-criteria')
  const report = element(document, 'section', 'wwc-review-report')
  report.setAttribute('aria-labelledby', 'wwc-review-report-heading')
  const attentionBar = element(document, 'div', 'wwc-review-attention-bar')
  const filesHeading = element(document, 'h3', 'wwc-review-files-heading')
  filesHeading.textContent = '候选产物（文件与文档预览）'
  const files = element(document, 'ul', 'wwc-review-files')
  const runtimeHeading = element(document, 'h3', 'wwc-review-runtime-heading')
  runtimeHeading.textContent = '运行日志与诊断'
  const segments = element(document, 'ul', 'wwc-review-segments')
  const evidenceHeading = element(document, 'h3', 'wwc-review-evidence-heading')
  evidenceHeading.textContent = '验收证据'
  const evidenceList = element(document, 'ul', 'wwc-review-evidences')
  const historyHeading = element(document, 'h3', 'wwc-review-history-heading')
  historyHeading.textContent = '候选历史与历史结论'
  const historyList = element(document, 'ul', 'wwc-review-histories')
  const annotationsHost = element(document, 'div', 'wwc-review-annotations-host')
  const annotationsPanel: StrongFlowReviewAnnotationsPanel = mountStrongFlowReviewAnnotations({
    root: annotationsHost,
    annotations: options.annotations,
  })
  section.append(
    heading,
    statusNode,
    errorNode,
    summary,
    solutionReview,
    progress,
    criteriaHeading,
    criteriaList,
    report,
    attentionBar,
    filesHeading,
    files,
    runtimeHeading,
    segments,
    evidenceHeading,
    evidenceList,
    historyHeading,
    historyList,
    annotationsHost,
  )
  options.root.replaceChildren(section)

  let selectedAttentionId: string | null = null

  async function submitSolutionDecision(action: SolutionReviewAction): Promise<void> {
    if (solutionBusy || solutionSubmitted) return
    if (solutionNote.value.trim() === '') {
      solutionMessage.textContent = '请先填写审核意见。'
      solutionNote.focus()
      return
    }
    solutionBusy = true
    solutionMessage.textContent = '正在提交决策…'
    renderSolutionReview(options.model.state)
    try {
      const outcome = await options.model.decideSolutionReview(action, solutionNote.value)
      solutionSubmitted = true
      solutionNote.value = ''
      solutionMessage.textContent = outcome === 'completed'
        ? '决策已生效。'
        : '决策已由 Controller 接受，等待权威投影更新。'
    } catch (error) {
      const code = typeof error === 'object' && error !== null && 'code' in error
        ? String(error.code)
        : 'UNKNOWN'
      const stale = ['REVISION_CONFLICT', 'CANDIDATE_STALE', 'WRONG_STATE', 'SOLUTION_REVIEW_STALE']
        .includes(code)
      solutionMessage.textContent = stale
        ? `当前页面已过期（${code}），请刷新后重试。`
        : `提交失败（${code}），请重试。`
    } finally {
      solutionBusy = false
      renderSolutionReview(options.model.state)
    }
  }

  function renderSolutionReview(state: StrongFlowReviewState): void {
    const review = state.detail?.solutionReview ?? null
    solutionReview.hidden = review === null
    if (review === null) return
    const identity = `${review.attentionItemId}:${review.reviewSetSha256}`
    if (solutionIdentity !== identity) {
      solutionIdentity = identity
      solutionSubmitted = false
      solutionMessage.textContent = ''
    }
    const attention = state.detail?.attention.find(item => item.id === review.attentionItemId)
    const pending = review.reviewStatus === 'pending' && attention?.status === 'open'
    solutionReview.dataset.reviewSolutionStatus = review.reviewStatus
    solutionWhy.textContent = pending
      ? `为什么需要你：${attention.title}。该方案将决定任务拆分与执行范围，需要人工确认。`
      : `该方案审核已结束，当前状态：${review.reviewStatus}。历史页面不能再次授权。`
    solutionChecks.textContent = '自动检查已完成：Controller 已验证并封存当前方案；'
      + `包含 ${String(review.workItemProposals.length)} 个任务、${String(review.components.length)} 个组件、`
      + `${String(review.connections.length)} 条连接和 2 张结构化图；封存摘要 ${review.reviewSetSha256}。`
    solutionSummary.textContent = `方案摘要：${review.summary}`
    solutionApproach.replaceChildren(...review.approach.map(item => {
      const row = document.createElement('li')
      row.textContent = item
      return row
    }))
    const risks = [...review.risks, ...review.unresolvedItems]
    solutionRisks.replaceChildren(...(risks.length === 0 ? ['没有已记录的风险或未决项。'] : risks).map(item => {
      const row = document.createElement('li')
      row.textContent = item
      return row
    }))
    const disabled = !pending || solutionBusy || solutionSubmitted
    solutionNote.disabled = !pending || solutionBusy || solutionSubmitted
    for (const control of [approveSolution, changeSolution, rejectSolution]) control.disabled = disabled
  }

  function selectedAttention(): StrongFlowReviewAttentionAnchor | null {
    const attention = options.model.state.detail?.attention ?? []
    const open = attention.filter(item => item.status === 'open')
    const found = open.find(item => item.id === selectedAttentionId) ?? open[0]
    if (found === undefined) return null
    return { id: found.id, title: found.title }
  }

  function renderAttentionBar(state: StrongFlowReviewState): void {
    attentionBar.replaceChildren()
    const open = state.detail?.attention.filter(item => item.status === 'open') ?? []
    const label = element(document, 'span', 'wwc-review-attention-label')
    if (open.length === 0) {
      label.textContent = '当前没有待处理的 Attention；错误引用将无法关联。'
      attentionBar.append(label)
      return
    }
    label.textContent = '错误引用关联到：'
    attentionBar.append(label)
    for (const item of open) {
      const choice = button(
        document,
        'wwc-review-attention-target',
        item.title,
        () => {
          selectedAttentionId = item.id
          render(options.model.state)
        },
      )
      choice.dataset.reviewAttentionId = item.id
      choice.dataset.reviewAttentionSelected = selectedAttention() === null
        ? 'false'
        : (selectedAttention()?.id === item.id ? 'true' : 'false')
      attentionBar.append(choice)
    }
  }

  function renderSummary(state: StrongFlowReviewState): void {
    summary.replaceChildren()
    const detail = state.detail
    if (detail === null) return
    summary.dataset.reviewDeliveryStatus = detail.status
    summary.dataset.reviewRevision = String(detail.deliveryRevision)
    const title = element(document, 'p', 'wwc-review-summary-title')
    title.textContent = detail.requirements.title
    const facts = element(document, 'p', 'wwc-review-summary-facts')
    const candidate = detail.currentCandidate
    const verdict = detail.verdict
    facts.textContent = [
      `状态 ${detail.status}`,
      `修订 ${String(detail.deliveryRevision)}`,
      candidate === null ? '当前无 Candidate' : `Candidate ${candidate.candidateRef}`,
      verdict === null ? '尚无结论' : `结论 ${VERDICT_LABEL[verdict.status] ?? verdict.status}`,
    ].join(' · ')
    summary.append(title, facts)
  }

  function renderProgress(state: StrongFlowReviewState): void {
    progress.replaceChildren()
    const working = element(document, 'article', 'wwc-review-progress-group')
    working.dataset.progressKind = 'working-plan'
    const workingHeading = element(document, 'h3', 'wwc-review-progress-heading')
    workingHeading.textContent = '执行计划（模型工作步骤）'
    const workingFacts = element(document, 'p', 'wwc-review-progress-facts')
    const plan = state.progress.workingPlan
    workingFacts.textContent = plan === null
      ? '尚无计划步骤记录。'
      : `共 ${String(plan.total)} 项 · 已完成 ${String(plan.completed)} 项 · `
        + `进行中 ${String(plan.inProgress)} 项 · 待处理 ${String(plan.pending)} 项`
    working.append(workingHeading, workingFacts)

    const acceptance = element(document, 'article', 'wwc-review-progress-group')
    acceptance.dataset.progressKind = 'accepted-criteria'
    const acceptanceHeading = element(document, 'h3', 'wwc-review-progress-heading')
    acceptanceHeading.textContent = '验收条件（Controller 判定）'
    const acceptanceFacts = element(document, 'p', 'wwc-review-progress-facts')
    const criteria = state.progress.acceptedCriteria
    acceptanceFacts.textContent = `共 ${String(criteria.total)} 项 · 通过 ${String(criteria.accepted)} 项 · `
      + `未通过 ${String(criteria.failed)} 项 · 无法定论 ${String(criteria.inconclusive)} 项 · `
      + `基础设施错误 ${String(criteria.infraError)} 项 · 未判定 ${String(criteria.pending)} 项`
    acceptance.append(acceptanceHeading, acceptanceFacts)
    const maxAttempt = state.segments.reduce((current, segment) => Math.max(current, segment.attempt), 0)
    const rework = element(document, 'p', 'wwc-review-progress-facts')
    const budget = state.detail?.requirements.maxReworkAttempts
    rework.textContent = maxAttempt === 0
      ? '当前读取范围内无法确认返工次数。'
      : `已发生返工 ${String(Math.max(0, maxAttempt - 1))} 次`
        + (typeof budget === 'number' ? ` · 预算 ${String(budget)} 次` : '')
    acceptance.append(rework)
    progress.append(working, acceptance)
  }

  function renderCriteria(state: StrongFlowReviewState): void {
    criteriaList.replaceChildren()
    const detail = state.detail
    if (detail === null || detail.requirements.acceptanceCriteria.length === 0) {
      const empty = element(document, 'li', 'wwc-review-criterion-empty')
      empty.textContent = '当前交付没有可展示的验收条件。'
      criteriaList.append(empty)
      return
    }
    const results = new Map((detail.verdict?.criteria ?? []).map(result => [result.criterionId, result]))
    const evidence = new Map(state.evidence.map(entry => [entry.evidence.id, entry]))
    for (const criterion of detail.requirements.acceptanceCriteria) {
      const result = results.get(criterion.id)
      const verdict = result?.verdict ?? 'pending'
      const item = element(document, 'li', 'wwc-review-criterion')
      item.dataset.reviewCriterionId = criterion.id
      item.dataset.reviewCriterionResult = verdict
      item.dataset.reviewCriterionRequired = criterion.required ? 'true' : 'false'
      const head = element(document, 'h4', 'wwc-review-criterion-head')
      head.textContent = `${criterion.id} · ${criterion.required ? '必需' : '可选'} · ${VERDICT_LABEL[verdict] ?? verdict}`
      const description = element(document, 'p', 'wwc-review-criterion-description')
      description.textContent = criterion.description
      const method = element(document, 'p', 'wwc-review-criterion-method')
      method.textContent = criterion.verificationMethod === null
        ? '验证方式未配置；该项不会因此视为通过。'
        : `验证方式：${criterion.verificationMethod}`
      item.append(head, description, method)
      if (result === undefined) {
        const pending = element(document, 'p', 'wwc-review-criterion-pending')
        pending.textContent = '当前 Candidate 没有该项独立结果；未执行、未映射都不是通过。'
        item.append(pending)
      } else {
        const explanation = element(document, 'p', 'wwc-review-criterion-explanation')
        explanation.textContent = `判定说明：${result.explanation}`
        const evaluated = element(document, 'p', 'wwc-review-criterion-evaluated')
        evaluated.textContent = `判定时间：${result.evaluatedAt}`
        item.append(explanation, evaluated)
        const refs = element(document, 'ul', 'wwc-review-criterion-evidence')
        if (result.evidenceRefs.length === 0) {
          const missing = element(document, 'li', 'wwc-review-criterion-evidence-missing')
          missing.textContent = '该结果没有引用验收证据。'
          refs.append(missing)
        }
        for (const evidenceRef of result.evidenceRefs) {
          const entry = evidence.get(evidenceRef)
          const row = element(document, 'li', 'wwc-review-criterion-evidence-row')
          row.dataset.reviewEvidenceMapped = entry === undefined ? 'false' : 'true'
          row.textContent = entry === undefined
            ? `证据 ${evidenceRef} 当前不可用。`
            : `${entry.evidence.type} · ${entry.evidence.id} · WorkRun ${entry.evidence.workRunId}`
          refs.append(row)
        }
        item.append(refs)
      }
      criteriaList.append(item)
    }
  }

  function renderReport(state: StrongFlowReviewState): void {
    report.replaceChildren()
    const heading = element(document, 'h3', 'wwc-review-report-heading')
    heading.id = 'wwc-review-report-heading'
    heading.textContent = '交付报告与剩余风险'
    report.append(heading)
    const detail = state.detail
    if (detail === null) return
    const results = new Map((detail.verdict?.criteria ?? []).map(result => [result.criterionId, result]))
    const risks = detail.requirements.acceptanceCriteria
      .filter(criterion => criterion.required && results.get(criterion.id)?.verdict !== 'pass')
      .map(criterion => `${criterion.id}：${VERDICT_LABEL[results.get(criterion.id)?.verdict ?? 'pending']}`)
    risks.push(...(detail.verdict?.unresolvedFindings ?? []))
    const riskList = element(document, 'ul', 'wwc-review-risks')
    if (risks.length === 0) {
      const empty = element(document, 'li', 'wwc-review-risk-empty')
      empty.textContent = '当前 Candidate 没有已记录的剩余风险。'
      riskList.append(empty)
    } else {
      for (const risk of risks) {
        const row = element(document, 'li', 'wwc-review-risk')
        row.textContent = risk
        riskList.append(row)
      }
    }
    const scopeNote = element(document, 'p', 'wwc-review-report-note')
    scopeNote.textContent = '自动检查只证明对应断言，不代表覆盖全部用户体验；'
      + '视觉差异需结合基线报告区分环境变化与业务回归。'
    report.append(riskList, scopeNote)
    const text = deliveryReportText(state)
    if (text !== null && options.onDownload !== undefined) {
      report.append(button(document, 'wwc-review-report-download', '下载安全交付报告', () => {
        options.onDownload?.(
          `delivery-${detail.deliveryId}-report.txt`,
          new TextEncoder().encode(text),
          'text/plain;charset=utf-8',
        )
      }))
    }
  }

  function renderEvidence(state: StrongFlowReviewState): void {
    evidenceList.replaceChildren()
    if (state.evidence.length === 0) {
      const empty = element(document, 'li', 'wwc-review-evidence-empty')
      empty.textContent = '当前 Candidate 没有已记录的验收证据。'
      evidenceList.append(empty)
      return
    }
    for (const entry of state.evidence) {
      const item = element(document, 'li', 'wwc-review-evidence')
      item.dataset.reviewEvidenceId = entry.evidence.id
      item.dataset.reviewEvidenceType = entry.evidence.type
      if (entry.outcome !== null) item.dataset.reviewOutcome = entry.outcome
      const head = element(document, 'span', 'wwc-review-evidence-head')
      head.textContent = `${entry.evidence.type} · ${entry.evidence.sourceRef}`
      const workRun = element(document, 'span', 'wwc-review-evidence-run')
      workRun.textContent = `WorkRun ${entry.evidence.workRunId}`
      const artifacts = element(document, 'span', 'wwc-review-evidence-artifacts')
      artifacts.dataset.reviewArtifactState = entry.artifactState ?? 'unknown'
      artifacts.textContent = entry.artifactState === null
        ? '产物可访问性未读取'
        : (entry.artifactState === 'available' ? '产物可访问' : '产物不可访问')
      item.append(head, workRun, artifacts, button(
        document,
        'wwc-review-evidence-detail',
        '查看详情',
        () => {
          void options.model.openEvidenceDetail(entry.evidence.id)
        },
      ))
      for (const artifact of entry.artifacts) {
        item.append(renderEvidenceArtifact(document, entry.evidence.id, artifact, options))
      }
      if (entry.artifactState === 'unavailable') {
        const unavailable = element(document, 'p', 'wwc-review-evidence-unavailable')
        unavailable.dataset.reviewUnavailable = 'true'
        unavailable.textContent = '生产者未保留该证据与产物的精确授权链接；'
          + '内容默认收敛，不提供读取或下载。'
        item.append(unavailable)
      }
      if (entry.detailError !== null) {
        const error = element(document, 'p', 'wwc-review-evidence-error')
        error.textContent = `证据详情读取失败：${entry.detailError}`
        item.append(error)
      }
      evidenceList.append(item)
    }
  }

  function renderHistory(state: StrongFlowReviewState): void {
    historyList.replaceChildren()
    if (state.history.length === 0) {
      const empty = element(document, 'li', 'wwc-review-history-empty')
      empty.textContent = '该交付暂无候选历史。'
      historyList.append(empty)
      return
    }
    for (const entry of state.history) {
      const pinKey = `candidate:${entry.candidateRef}`
      if (options.annotations.isPinned(pinKey)) {
        options.annotations.setPinRetention(pinKey, entry.availability)
      }
      const item = element(document, 'li', 'wwc-review-history')
      item.dataset.reviewCandidateRef = entry.candidateRef
      item.dataset.reviewAvailability = entry.availability
      item.dataset.reviewCurrent = entry.isCurrentAtReadCursor ? 'true' : 'false'
      const head = element(document, 'span', 'wwc-review-history-head')
      head.textContent = entry.isCurrentAtReadCursor
        ? `${entry.candidateRef}（当前候选）`
        : entry.candidateRef
      const availability = element(document, 'span', 'wwc-review-history-availability')
      availability.dataset.reviewAvailability = entry.availability
      availability.textContent = AVAILABILITY_LABEL[entry.availability] ?? retentionLabel(null)
      item.append(
        head,
        availability,
        button(
          document,
          'wwc-review-history-pin',
          options.annotations.isPinned(pinKey) ? '取消固定' : '固定',
          () => {
            options.annotations.togglePin({
              key: pinKey,
              kind: 'candidate',
              label: `候选 ${entry.candidateRef}`,
              retention: entry.availability,
            })
          },
        ),
        button(
          document,
          'wwc-review-history-open',
          '查看历史结论',
          () => {
            void options.model.openHistoricalReview(entry.candidateRef)
          },
        ),
      )
      if (entry.review !== null) {
        const review = element(document, 'div', 'wwc-review-history-review')
        review.dataset.reviewDisplayOnly = 'true'
        review.dataset.reviewCurrentAuthorization = 'false'
        const verdict = entry.review.verdict
        const verdictNode = element(document, 'p', 'wwc-review-history-verdict')
        verdictNode.textContent = verdict === null
          ? '该候选当时没有结论记录。'
          : `历史结论：${VERDICT_LABEL[verdict.status] ?? verdict.status}`
            + `（证据 ${String(entry.review.evidence.length)} 条）`
        const note = element(document, 'p', 'wwc-review-history-note')
        note.textContent = '历史结论按原样保留、仅供展示；不能授权当前候选。'
        const availabilityNote = element(document, 'p', 'wwc-review-history-availability-note')
        availabilityNote.textContent = `证据保留状态：${retentionLabel(entry.review.availability)}`
        review.append(verdictNode, note, availabilityNote)
        item.append(review)
      }
      if (entry.reviewError !== null) {
        const error = element(document, 'p', 'wwc-review-history-error')
        error.textContent = `历史结论读取失败：${entry.reviewError}`
        item.append(error)
      }
      historyList.append(item)
    }
  }

  function render(state: StrongFlowReviewState): void {
    statusNode.textContent = STRONGFLOW_REVIEW_STATUS_LABEL[state.status]
    section.dataset.reviewStatus = state.status
    errorNode.hidden = state.error === null
    errorNode.textContent = state.error === null ? '' : `读取失败：${state.error.code}`
    renderSummary(state)
    renderSolutionReview(state)
    renderProgress(state)
    renderCriteria(state)
    renderReport(state)
    renderAttentionBar(state)
    renderFileRows(document, state, files, options)
    renderSegments(document, state, segments, selectedAttention(), options)
    renderEvidence(state)
    renderHistory(state)
  }

  const unsubscribe = options.model.subscribe(nextState => {
    render(nextState)
  })
  render(options.model.state)

  return {
    root: section,
    close(): void {
      unsubscribe()
      annotationsPanel.close()
      section.remove()
    },
  }
}
