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
import { candidateCanApply, mountCandidateApplication, type CandidateApplicationOptions, type CandidateApplicationStatus } from './candidate-application.js'
import { solutionDiagram } from './solution-diagram.js'
import { attentionTitle } from './display-labels.js'
import { mountTabs } from './components/tabs.js'
import { renderDiff } from './diff-preview.js'

export interface StrongFlowReviewDetailOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowReviewViewModel
  readonly annotations: StrongFlowReviewAnnotations
  readonly candidateActions?: Omit<CandidateApplicationOptions, 'root' | 'model'>
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
  ready: '已更新',
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
  infra_error: '验证环境异常',
  pending: '未判定',
})

const ROLE_LABEL: Readonly<Record<string, string>> = { executor: '实现', reviewer: '独立审查', verifier: '验证', remediator: '返工' }

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

function disclosure(document: Document, label: string, ...content: HTMLElement[]): HTMLDetailsElement {
  const details = element(document, 'details', 'wwc-review-section')
  const summary = document.createElement('summary')
  summary.textContent = label
  const body = element(document, 'div', 'wwc-review-section-body')
  body.append(...content)
  details.append(summary, body)
  return details
}

const EVIDENCE_LABEL: Readonly<Record<string, string>> = {
  command: '命令执行', test: '测试', review: '代码审查', screenshot: '截图',
  artifact: '产物', validation: '验证',
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
    pre.tabIndex = 0
    pre.setAttribute('aria-label', `${file.path} 的差异，减号为删除，加号为新增`)
    const code = renderDiff(document, preview.chunks.map(chunk => chunk.text).join(''))
    pre.append(code)
    box.append(pre)
  }
  const meta = element(document, 'p', 'wwc-review-preview-meta')
  meta.textContent = `已读取 ${preview.returnedBytes} / ${preview.totalBytes} 字节`
  if (preview.nextOffset !== null) box.append(meta)
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
    empty.textContent = '尚未产生可查看的变更。'
    files.append(empty)
    return
  }
  if (state.files.length === 0) {
    const empty = element(document, 'li', 'wwc-review-file-empty')
    empty.textContent = '当前版本没有变更文件。'
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
    fact.textContent = ({ added: '新增', modified: '修改', deleted: '删除', renamed: '重命名', copied: '复制', type_changed: '类型变更', unmerged: '待合并' } as Readonly<Record<string, string>>)[file.status] ?? '变更'
    const size = element(document, 'span', 'wwc-review-file-size')
    if (file.additions !== null && file.deletions !== null) size.textContent = `+${file.additions} −${file.deletions}`
    head.append(pathNode, fact, size, button(
      document,
      'wwc-review-file-open',
      '查看差异',
      () => {
        void options.model.openPreview(file.path)
      },
    ), button(
      document,
      'wwc-review-file-pin',
      '加入审阅笔记',
      () => options.annotations.pin({
        key: `file:${file.path}`,
        kind: 'file',
        label: `文件 ${file.path}`,
        retention: state.history.find(entry => entry.isCurrentAtReadCursor)?.availability ?? null,
      }),
    ))
    item.append(head)
    if (file.preview !== null || file.previewError !== null) item.append(renderPreview(document, file, options))
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
  head.textContent = `${ROLE_LABEL[segment.role ?? ''] ?? '执行'} · 会话 ${segment.productSessionId} · 尝试 ${segment.attempt}`
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
        '关联到待处理项',
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
  name.textContent = artifact.descriptor.fileName ?? '验收附件'
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
    '加入审阅笔记',
    () => options.annotations.pin({
      key: `artifact:${artifact.descriptor.artifactId}`,
      kind: 'artifact',
      label: `产物 ${artifact.descriptor.fileName ?? '验收附件'}`,
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
  heading.textContent = '交付详情'
  const statusNode = element(document, 'p', 'wwc-review-status')
  const refresh = button(document, 'wwc-review-refresh', '刷新交付进度', () => { void options.model.refresh() })
  const errorNode = element(document, 'p', 'wwc-review-error')
  errorNode.hidden = true
  const summary = element(document, 'div', 'wwc-review-summary')
  const evidenceReads = new Set<string>()
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
  const solutionDiagrams = element(document, 'div', 'wwc-review-solution-diagrams')
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
    solutionDiagrams,
    solutionRisks,
    solutionNoteLabel,
    solutionNote,
    solutionActions,
    solutionMessage,
  )
  const progress = element(document, 'section', 'wwc-review-progress')
  const applicationHost = element(document, 'section', 'wwc-review-application')
  let applicationStatus: CandidateApplicationStatus = 'unavailable'
  const application = options.candidateActions === undefined ? null : mountCandidateApplication({
    ...options.candidateActions, root: applicationHost, model: options.model,
    onStatusChange(status) { applicationStatus = status; renderSummary(options.model.state) },
  })
  progress.setAttribute('aria-label', '任务进度')
  const criteriaList = element(document, 'ol', 'wwc-review-criteria')
  const report = element(document, 'section', 'wwc-review-report')
  report.setAttribute('aria-labelledby', 'wwc-review-report-heading')
  const attentionBar = element(document, 'div', 'wwc-review-attention-bar')
  const files = element(document, 'ul', 'wwc-review-files')
  const segments = element(document, 'ul', 'wwc-review-segments')
  const evidenceList = element(document, 'ul', 'wwc-review-evidences')
  const historyList = element(document, 'ul', 'wwc-review-histories')
  const annotationsHost = element(document, 'div', 'wwc-review-annotations-host')
  const annotationsPanel: StrongFlowReviewAnnotationsPanel = mountStrongFlowReviewAnnotations({
    root: annotationsHost,
    annotations: options.annotations,
  })
  const evidenceSection = disclosure(document, '完整报告与证据', evidenceList, report)
  const applicationSection = disclosure(document, '应用到项目', applicationHost)
  applicationSection.hidden = true
  const historySection = disclosure(document, '版本历史', historyList)
  const diagnostics = disclosure(document, '运行记录与诊断', progress, attentionBar, segments, annotationsHost)
  const solutionSection = disclosure(document, '执行方案', solutionReview)
  const records = disclosure(document, '更多记录', historySection, diagnostics)
  const taskContent = element(document, 'div', 'wwc-review-task-content')
  const overviewPanel = element(document, 'section', 'wwc-review-tab-panel')
  const changesPanel = element(document, 'section', 'wwc-review-tab-panel')
  const acceptancePanel = element(document, 'section', 'wwc-review-tab-panel')
  const panels = [overviewPanel, changesPanel, acceptancePanel]
  const tabIds = ['task', 'changes', 'acceptance'] as const
  for (const [index, panel] of panels.entries()) {
    panel.id = `wwc-review-panel-${tabIds[index]}`
    panel.setAttribute('role', 'tabpanel')
    panel.setAttribute('aria-labelledby', `wwc-review-tabs-${tabIds[index]}`)
    panel.tabIndex = 0
  }
  overviewPanel.append(taskContent, solutionSection, applicationSection)
  changesPanel.append(files)
  acceptancePanel.append(criteriaList, evidenceSection)
  let selectedTab: string = 'task'
  function tabProps() {
    const state = options.model.state
    const counts = state.progress.acceptedCriteria
    return { id: 'wwc-review-tabs', label: '任务详情', selectedId: selectedTab,
      tabs: [
        { id: 'task', label: '任务内容', panelId: overviewPanel.id },
        { id: 'changes', label: `代码变更 ${state.files.length}${state.filesTruncated ? '+' : ''}`, panelId: changesPanel.id },
        { id: 'acceptance', label: `验收结果 ${counts.accepted}/${counts.total}`, panelId: acceptancePanel.id },
      ], onSelect: selectTab }
  }
  function updateTabs(): void {
    const focused = tabs.root.contains(document.activeElement)
    tabs.update(tabProps())
    panels.forEach((panel, index) => { panel.hidden = tabIds[index] !== selectedTab })
    if (focused) tabs.tab(selectedTab).focus()
  }
  function selectTab(id: string): void {
    selectedTab = id
    updateTabs()
    tabs.tab(id).focus()
    const first = options.model.state.files[0]
    if (id === 'changes' && first?.preview === null && first.previewError === null) {
      void options.model.openPreview(first.path)
    }
  }
  const tabs = mountTabs({ document, props: tabProps() })
  updateTabs()
  let solutionStatus: string | null = null
  const toolbar = element(document, 'div', 'wwc-review-toolbar')
  toolbar.append(heading, statusNode, refresh)
  section.append(toolbar, errorNode, summary, tabs.root, ...panels, records)
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
        : '已收到审核意见，正在更新任务状态。'
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
    solutionSection.hidden = review === null
    if (review === null) return
    const identity = `${review.attentionItemId}:${review.reviewSetSha256}`
    if (solutionIdentity !== identity) {
      solutionIdentity = identity
      solutionSubmitted = false
      solutionMessage.textContent = ''
    }
    const attention = state.detail?.attention.find(item => item.id === review.attentionItemId)
    const pending = review.reviewStatus === 'pending' && attention?.status === 'open'
    if (solutionStatus !== review.reviewStatus) {
      solutionStatus = review.reviewStatus
      solutionSection.open = pending
    }
    solutionReview.dataset.reviewSolutionStatus = review.reviewStatus
    solutionWhy.textContent = pending
      ? '请检查目标、执行范围与下方方案，确认后继续。'
      : '方案审核已结束，审核意见已记录。'
    solutionChecks.textContent = `包含 ${review.workItemProposals.length} 项执行任务。`
    solutionDiagrams.replaceChildren(
      solutionDiagram(document, review.architectureDiagram, 'wwc-architecture-arrow'),
      solutionDiagram(document, review.processDiagram, 'wwc-process-arrow'),
    )
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
    return { id: found.id, title: attentionTitle(found.title) }
  }

  function renderAttentionBar(state: StrongFlowReviewState): void {
    attentionBar.replaceChildren()
    const open = state.detail?.attention.filter(item => item.status === 'open') ?? []
    const label = element(document, 'span', 'wwc-review-attention-label')
    if (open.length === 0) {
      label.textContent = '当前没有待处理项。'
      attentionBar.append(label)
      return
    }
    label.textContent = '错误引用关联到：'
    attentionBar.append(label)
    for (const item of open) {
      const choice = button(
        document,
        'wwc-review-attention-target',
        attentionTitle(item.title),
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
    heading.textContent = detail.requirements.title
    summary.dataset.reviewDeliveryStatus = detail.status
    summary.dataset.reviewRevision = String(detail.deliveryRevision)
    const facts = element(document, 'div', 'wwc-review-summary-facts')
    const active = state.segments.findLast(segment => ['running', 'queued', 'leased'].includes(segment.state ?? ''))
    const pendingPlan = detail.solutionReview?.reviewStatus === 'pending'
    const canApply = candidateCanApply(state)
    const stateLabel = pendingPlan ? '等待确认方案' : active !== undefined
      ? `${ROLE_LABEL[active.role ?? ''] ?? '执行'}${active.state === 'running' ? '中' : '等待启动'}`
      : detail.status === 'done' ? '已验收'
      : canApply ? '验收通过'
      : detail.verdict === null ? '等待验收' : detail.verdict.status === 'pass' ? '验收待核对' : `验收${VERDICT_LABEL[detail.verdict.status] ?? '待确认'}`
    const criteria = state.progress.acceptedCriteria
    const unresolved = (detail.verdict?.unresolvedFindings ?? []).length
    const fileCount = `${state.files.length}${state.filesTruncated ? '+' : ''}`
    const rework = detail.reworkAttemptsUsed === undefined ? '返工次数未记录' : `返工 ${detail.reworkAttemptsUsed} 次`
    const factItems = [
      { text: `验收通过 ${criteria.accepted}/${criteria.total}`, className: 'wwc-review-summary-fact-acceptance' },
      ...(unresolved === 0 ? [] : [{ text: `未解决 ${unresolved} 项`, className: 'wwc-review-summary-fact-unresolved' }]),
      { text: rework, className: 'wwc-review-summary-fact-rework' },
      { text: `修改文件 ${fileCount} 个`, className: 'wwc-review-summary-fact-files' },
      { text: `应用状态：${applicationStatus === 'applied' ? '已应用' : applicationStatus === 'not-applied' ? '未应用' : applicationStatus === 'loading' ? '正在读取' : '暂无法确认'}`, className: 'wwc-review-summary-fact-application' },
      ...(detail.status === 'done' ? [] : [{ text: stateLabel, className: 'wwc-review-summary-fact-state' }]),
    ]
    for (const factItem of factItems) {
      const fact = element(document, 'span', `wwc-review-summary-fact ${factItem.className}`)
      fact.textContent = factItem.text
      facts.append(fact)
    }
    summary.append(facts)
    if (!pendingPlan) {
      const action = button(document, 'wwc-review-primary', canApply && application !== null
        ? (detail.status === 'done' ? '查看应用结果' : '应用到项目') : '查看验收结果', () => {
        if (canApply && application !== null) {
          selectTab('task')
          applicationSection.hidden = false
          applicationSection.open = true
          applicationSection.querySelector('summary')?.focus()
          applicationSection.scrollIntoView({ block: 'nearest', behavior: 'smooth' })
        } else selectTab('acceptance')
      })
      summary.append(action)
    }
    evidenceSection.firstElementChild!.textContent = `完整报告与证据 · ${state.evidence.length} 条证据`
    historySection.firstElementChild!.textContent = `版本历史 · ${state.history.length} 个版本`
    updateTabs()
    renderTaskContent(state)
  }

  function renderTaskContent(state: StrongFlowReviewState): void {
    taskContent.replaceChildren()
    const detail = state.detail
    if (detail === null) return
    const requirements = detail.requirements
    const goalHeading = element(document, 'h3', 'wwc-review-content-heading')
    goalHeading.textContent = '要做什么'
    const goal = element(document, 'p', 'wwc-review-goal')
    goal.textContent = requirements.goal?.trim() || requirements.title
    taskContent.append(goalHeading, goal)
    const scope = requirements.scope ?? []
    if (scope.length > 0) {
      const scopeHeading = element(document, 'h4', 'wwc-review-content-heading')
      scopeHeading.textContent = '具体工作'
      const list = element(document, 'ul', 'wwc-review-scope')
      for (const text of scope) {
        const item = document.createElement('li')
        item.textContent = text
        list.append(item)
      }
      taskContent.append(scopeHeading, list)
    }
    const resultHeading = element(document, 'h3', 'wwc-review-content-heading')
    resultHeading.textContent = '完成情况'
    const results = new Map((detail.verdict?.criteria ?? []).map(result => [result.criterionId, result.verdict]))
    const outcome = element(document, 'ul', 'wwc-review-outcomes')
    // Show unresolved criteria first; the acceptance tab always retains the full list.
    const criteria = [...requirements.acceptanceCriteria].sort((a, b) =>
      Number(results.get(a.id) === 'pass') - Number(results.get(b.id) === 'pass'))
    for (const criterion of criteria.slice(0, 3)) {
      const result = results.get(criterion.id) ?? 'pending'
      const row = element(document, 'li', 'wwc-review-outcome')
      row.dataset.result = result
      const label = element(document, 'span', 'wwc-review-outcome-label')
      label.textContent = result === 'pass' ? '✓ 通过' : VERDICT_LABEL[result] ?? '待确认'
      const content = element(document, 'span', 'wwc-review-outcome-description')
      content.textContent = criterion.description
      row.append(label, content)
      outcome.append(row)
    }
    if (criteria.length === 0) {
      const empty = document.createElement('li')
      empty.textContent = '尚未设置验收条件。'
      outcome.append(empty)
    }
    taskContent.append(resultHeading, outcome)
    const links = element(document, 'div', 'wwc-review-content-links')
    if (state.files.length > 0) links.append(button(document, 'wwc-review-content-link',
      `查看 ${state.files.length}${state.filesTruncated ? '+' : ''} 个文件的修改`, () => selectTab('changes')))
    if (criteria.length > 3) links.append(button(document, 'wwc-review-content-link',
      `查看全部 ${criteria.length} 项验收`, () => selectTab('acceptance')))
    if (links.childElementCount > 0) taskContent.append(links)
    const bounds = [...(requirements.constraints ?? []).map(text => `约束：${text}`),
      ...(requirements.outOfScope ?? []).map(text => `不包含：${text}`)]
    if (bounds.length > 0) {
      const list = element(document, 'ul', 'wwc-review-scope')
      for (const text of bounds) { const row = document.createElement('li'); row.textContent = text; list.append(row) }
      taskContent.append(disclosure(document, '范围与限制', list))
    }
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
    acceptanceHeading.textContent = '验收条件'
    const acceptanceFacts = element(document, 'p', 'wwc-review-progress-facts')
    const criteria = state.progress.acceptedCriteria
    acceptanceFacts.textContent = `共 ${String(criteria.total)} 项 · 通过 ${String(criteria.accepted)} 项 · `
      + `未通过 ${String(criteria.failed)} 项 · 无法定论 ${String(criteria.inconclusive)} 项 · `
      + `基础设施错误 ${String(criteria.infraError)} 项 · 未判定 ${String(criteria.pending)} 项`
    acceptance.append(acceptanceHeading, acceptanceFacts)
    const rework = element(document, 'p', 'wwc-review-progress-facts')
    const budget = state.detail?.requirements.maxReworkAttempts
    const attempts = state.detail?.reworkAttemptsUsed
    rework.textContent = attempts === undefined
      ? '当前读取范围内无法确认返工次数。'
      : `已发生返工 ${String(attempts)} 次`
        + (typeof budget === 'number' ? ` · 预算 ${String(budget)} 次` : '')
    acceptance.append(rework)
    progress.append(working, acceptance)
    const findings = state.detail?.verdict?.unresolvedFindings ?? []
    if (findings.length > 0) {
      const raw = element(document, 'p', 'wwc-review-diagnostic-findings')
      raw.textContent = findings.join('\n')
      progress.append(disclosure(document, '审查问题原始记录', raw))
    }
  }

  function renderCriteria(state: StrongFlowReviewState): void {
    const expanded = new Set([...criteriaList.querySelectorAll<HTMLDetailsElement>('details[open]')].map(item => item.dataset.reviewCriterionId))
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
      head.textContent = `${VERDICT_LABEL[verdict] ?? '待确认'}${criterion.required ? '' : ' · 可选'}`
      const description = element(document, 'p', 'wwc-review-criterion-description')
      description.textContent = criterion.description
      const method = element(document, 'p', 'wwc-review-criterion-method')
      method.textContent = criterion.verificationMethod === null
        ? '验证方式未配置；该项不会因此视为通过。'
        : `验证方式：${criterion.verificationMethod}`
      const explanationSection = disclosure(document, '结论与对应证据', method)
      explanationSection.dataset.reviewCriterionId = criterion.id
      explanationSection.open = expanded.has(criterion.id) || verdict !== 'pass'
      explanationSection.addEventListener('toggle', () => {
        if (!explanationSection.open || !explanationSection.isConnected) return
        for (const evidenceId of result?.evidenceRefs ?? []) {
          const entry = options.model.state.evidence.find(item => item.evidence.id === evidenceId)
          const key = `${detail.readCursor.token}:${evidenceId}`
          if (entry === undefined || entry.outcome !== null || entry.detailError !== null || evidenceReads.has(key)) continue
          evidenceReads.add(key)
          void options.model.openEvidenceDetail(evidenceId).finally(() => evidenceReads.delete(key))
        }
      })
      const body = explanationSection.lastElementChild!
      item.append(head, description, explanationSection)
      if (result === undefined) {
        const pending = element(document, 'p', 'wwc-review-criterion-pending')
        pending.textContent = '尚未获得该项的独立验证结果。'
        body.append(pending)
      } else {
        const explanation = element(document, 'p', 'wwc-review-criterion-explanation')
        explanation.textContent = result.explanation.includes('direct Evidence does not match the approved verification method')
          ? '现有证据与确认的验证方式不一致，需要核查后重新验证。'
          : result.explanation.includes('is supported by current direct Evidence')
            ? '判定有当前版本的执行证据支持。' : `判定说明：${result.explanation}`
        const evaluated = element(document, 'p', 'wwc-review-criterion-evaluated')
        evaluated.textContent = `判定时间：${new Date(result.evaluatedAt).toLocaleString('zh-CN')}`
        body.append(explanation, evaluated)
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
          if (entry === undefined) row.textContent = '引用的证据当前不可用。'
          else {
            const short = element(document, 'span', 'wwc-review-criterion-evidence-summary')
            const outcome = entry.outcome === null ? '执行结论尚未读取' : ({
              observed: '已观察', succeeded: '成功', failed: '失败', timed_out: '超时',
              policy_denied: '执行被拒绝', infrastructure_failed: '执行环境异常', cancelled: '已取消',
            }[entry.outcome] ?? entry.outcome)
            const availability = entry.artifactState === null ? '附件状态尚未读取'
              : entry.artifactState === 'available' ? `附件可用${entry.artifacts.length > 0 ? ` · ${entry.artifacts.length} 个附件` : ''}`
                : '附件不可用'
            short.textContent = `${EVIDENCE_LABEL[entry.evidence.type] ?? '验收'} · ${outcome} · ${availability}`
            row.append(short, button(document, 'wwc-review-criterion-evidence-open',
              `查看第 ${state.evidence.indexOf(entry) + 1} 条证据`, () => {
              selectTab('acceptance')
              evidenceSection.open = true
              void options.model.openEvidenceDetail(entry.evidence.id)
              evidenceSection.scrollIntoView({ block: 'nearest', behavior: 'smooth' })
              }))
          }
          refs.append(row)
        }
        body.append(refs)
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
      .map(criterion => `${criterion.description}：${VERDICT_LABEL[results.get(criterion.id)?.verdict ?? 'pending']}`)
    const findings = detail.verdict?.unresolvedFindings ?? []
    if (findings.length > 0) risks.push(`还有 ${findings.length} 项审查问题待处理，详见运行记录与诊断。`)
    const riskList = element(document, 'ul', 'wwc-review-risks')
    if (risks.length === 0) {
      const empty = element(document, 'li', 'wwc-review-risk-empty')
      empty.textContent = '没有已记录的剩余问题。'
      riskList.append(empty)
    } else {
      for (const risk of risks) {
        const row = element(document, 'li', 'wwc-review-risk')
        row.textContent = risk
        riskList.append(row)
      }
    }
    report.append(riskList)
    const text = deliveryReportText(state)
    if (text !== null && options.onDownload !== undefined) {
      report.append(button(document, 'wwc-review-report-download', '下载验收报告', () => {
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
      empty.textContent = '当前版本尚无验收证据。'
      evidenceList.append(empty)
      return
    }
    for (const entry of state.evidence) {
      const item = element(document, 'li', 'wwc-review-evidence')
      item.dataset.reviewEvidenceId = entry.evidence.id
      item.dataset.reviewEvidenceType = entry.evidence.type
      if (entry.outcome !== null) item.dataset.reviewOutcome = entry.outcome
      const head = element(document, 'span', 'wwc-review-evidence-head')
      head.textContent = `${EVIDENCE_LABEL[entry.evidence.type] ?? '验收'}证据 ${state.evidence.indexOf(entry) + 1}`
      const workRun = element(document, 'span', 'wwc-review-evidence-run')
      workRun.textContent = `证据：${entry.evidence.id} · 执行：${entry.evidence.workRunId} · 来源：${entry.evidence.sourceRef}`
      const artifacts = element(document, 'span', 'wwc-review-evidence-artifacts')
      artifacts.dataset.reviewArtifactState = entry.artifactState ?? 'unknown'
      artifacts.textContent = entry.artifactState === null
        ? '展开查看执行结果和附件'
        : (entry.artifactState === 'available' ? '产物可访问' : '产物不可访问')
      item.append(head, artifacts, button(
        document,
        'wwc-review-evidence-detail',
        '查看详情',
        () => {
          void options.model.openEvidenceDetail(entry.evidence.id)
        },
      ))
      item.append(disclosure(document, '来源详情', workRun))
      if (entry.outcome !== null) {
        const result = element(document, 'p', 'wwc-review-evidence-result')
        const label = {
          observed: '已观察', succeeded: '成功', failed: '失败', timed_out: '超时',
          policy_denied: '执行被拒绝', infrastructure_failed: '执行环境异常', cancelled: '已取消',
        }[entry.outcome]
        result.textContent = `已核验执行结果：${label ?? entry.outcome}`
        item.append(result)
        const criteria = state.detail?.verdict?.criteria
          .filter(criterion => criterion.evidenceRefs.includes(entry.evidence.id)) ?? []
        for (const criterion of criteria) {
          const method = state.detail?.requirements.acceptanceCriteria
            .find(item => item.id === criterion.criterionId)?.verificationMethod
          if (method === null || method === undefined) continue
          const command = element(document, 'pre', 'wwc-review-evidence-command')
          command.textContent = `验收命令：${method}`
          item.append(command)
        }
      }
      for (const artifact of entry.artifacts) {
        item.append(renderEvidenceArtifact(document, entry.evidence.id, artifact, options))
      }
      if (entry.artifactState === 'unavailable') {
        const unavailable = element(document, 'p', 'wwc-review-evidence-unavailable')
        unavailable.dataset.reviewUnavailable = 'true'
        const commandEvidence = entry.evidence.sourceRef.startsWith('runtime_event:')
          && (entry.evidence.type === 'command' || entry.evidence.type === 'test')
        if (commandEvidence) artifacts.textContent = '命令执行证据'
        unavailable.textContent = commandEvidence
          ? '此证据保留执行结果与来源引用，未附完整输出文件。'
          : '此证据未提供可读取的附件。'
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
        ? `版本 ${state.history.indexOf(entry) + 1} · 当前版本`
        : `版本 ${state.history.indexOf(entry) + 1}`
      const availability = element(document, 'span', 'wwc-review-history-availability')
      availability.dataset.reviewAvailability = entry.availability
      availability.textContent = AVAILABILITY_LABEL[entry.availability] ?? retentionLabel(null)
      item.append(
        head,
        availability,
        button(
          document,
          'wwc-review-history-pin',
          options.annotations.isPinned(pinKey) ? '移出审阅笔记' : '加入审阅笔记',
          () => {
            options.annotations.togglePin({
              key: pinKey,
              kind: 'candidate',
              label: head.textContent,
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
    refresh.disabled = state.status === 'loading' || state.status === 'refreshing'
    statusNode.textContent = STRONGFLOW_REVIEW_STATUS_LABEL[state.status]
    statusNode.hidden = state.status === 'ready'
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
  const refreshTimer = setInterval(() => {
    const state = options.model.state
    if (state.status === 'ready' && state.detail?.verdict === null
      && !state.files.some(file => file.preview !== null)
      && !state.evidence.some(entry => entry.outcome !== null)
      && !document.activeElement?.matches('input, textarea, select')) void options.model.refresh()
  }, 5000)

  return {
    root: section,
    close(): void {
      clearInterval(refreshTimer)
      unsubscribe()
      tabs.close()
      annotationsPanel.close()
      application?.close()
      section.remove()
    },
  }
}
