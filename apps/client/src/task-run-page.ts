// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneTaskAnchor } from './community-control-plane-client.js'
import type {
  TaskRunApplyFacts,
  TaskRunCandidateFacts,
  TaskRunCriterionFacts,
  TaskRunEvidenceItemFacts,
  TaskRunIdentityFacts,
  TaskRunState,
  TaskRunViewModel,
  TaskRunWorkerSessionFacts,
} from './task-run-view-model.js'
import { runWorkGraphStateTone, taskRunCommitText } from './task-run-view-model.js'

export interface TaskRunPageOptions {
  readonly root: HTMLElement
  readonly model: TaskRunViewModel
  /** The task-board deep link, so the running task is never a dead end. */
  readonly homeHref?: string
  /** Exact Delivery review route; omitted when the route has no Delivery fact. */
  readonly reviewHref?: string
  /** 已知锚点(假优先种子/表单创建):提供设计稿 05 的任务名与改动/验收两行。 */
  readonly anchor?: {
    readonly title?: string
    readonly changes?: string
    readonly acceptance?: string
  }
}

export interface TaskRunPage {
  close(): void
}

/**
 * The one status line under the title.  The page is the StrongFlow task's own
 * run surface, so the line carries the surface label plus the zone status the
 * snapshot actually reports - never an invented stage.
 */
function statusLineText(snapshot: TaskRunState): string {
  const zone = snapshot.status === 'loading'
    ? '正在加载'
    : snapshot.status === 'partial'
      ? '部分身份信息不可用'
      : '运行中'
  return `强流程 · ${zone}`
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

const ROW_TERMS = Object.freeze([
  '执行设备',
  '仓库',
  '占用状态',
  '工作契约',
  '验收条件',
  '负责人',
  '依赖项',
  '阻塞项',
  '证据',
  '执行会话',
  '候选结果',
  '应用结果',
]) as readonly string[]

/**
 * Mount the §16.7 run page in the design-05 shape: back link and the 流程与记录
 * display slot on one row, the running-task title with its status line and
 * description, the collapsed identity table, and the StrongFlow-owned
 * approval actions (visible but disabled - this page never fakes a decision).
 * The module owns DOM and ARIA only; every value is projected from the one run
 * view-model snapshot, and a missing row names its gap instead of inventing
 * state.
 */
export function mountTaskRunPage(options: TaskRunPageOptions): TaskRunPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-task-run')
  const topbar = element(document, 'div', 'wwc-task-run-topbar')
  const back = element(document, 'a', 'wwc-task-run-back')
  const topbarActions = element(document, 'div', 'wwc-task-run-topbar-actions')
  const recordSlot = element(document, 'span', 'wwc-task-run-topbar-record')
  const reviewLink = element(document, 'a', 'wwc-task-run-review-link')
  const separator = element(document, 'span', 'wwc-task-run-topbar-separator')
  const moreSlot = element(document, 'span', 'wwc-task-run-topbar-more')
  const heading = element(document, 'h2', 'wwc-task-run-heading')
  const statusLine = element(document, 'p', 'wwc-task-run-status')
  const description = element(document, 'p', 'wwc-task-run-description')
  // 设计稿 05:描述段下方的「改动：…」「验收：…」两行。
  const changesLine = element(document, 'p', 'wwc-task-run-scope-line')
  const acceptanceLine = element(document, 'p', 'wwc-task-run-scope-line')
  const zone = element(document, 'section', 'wwc-task-run-identity')
  // Design page 05: the full identity table stays collapsed behind one row;
  // the stage the user acts on stays up front.
  const identityToggle = element(document, 'button', 'wwc-task-run-identity-toggle')
  const rows = element(document, 'div', 'wwc-task-run-rows')
  const identityNotice = element(document, 'p', 'wwc-task-run-identity-notice')
  const actions = element(document, 'div', 'wwc-task-run-actions')
  const approve = element(document, 'button', 'wwc-task-run-approve')
  const requestChange = element(document, 'button', 'wwc-task-run-request-change')

  let closed = false

  section.setAttribute('aria-label', '运行中的任务')
  heading.id = 'wwc-task-run-heading'
  heading.textContent = options.anchor?.title ?? '运行中的任务'
  section.setAttribute('aria-labelledby', heading.id)
  statusLine.textContent = statusLineText(options.model.state)
  description.hidden = true
  zone.setAttribute('aria-label', '完整运行身份')
  identityToggle.type = 'button'
  identityToggle.setAttribute('aria-expanded', 'false')
  identityToggle.setAttribute('aria-controls', 'wwc-task-run-rows')
  const expandedToggleText = `展开完整运行身份 · ${String(ROW_TERMS.length)} 行`
  const collapsedToggleText = `收起完整运行身份 · ${String(ROW_TERMS.length)} 行`
  identityToggle.textContent = expandedToggleText
  identityToggle.addEventListener('click', () => {
    const expanded = identityToggle.getAttribute('aria-expanded') === 'true'
    identityToggle.setAttribute('aria-expanded', expanded ? 'false' : 'true')
    identityToggle.textContent = expanded ? expandedToggleText : collapsedToggleText
    rows.hidden = expanded
  })
  identityNotice.setAttribute('role', 'status')
  identityNotice.hidden = true
  // Design page 05 display slot: 流程与记录 and ⋯ 更多 are placeholders; the
  // commands stay StrongFlow-owned, so they are rendered inert.
  recordSlot.textContent = '流程与记录'
  reviewLink.textContent = '查看审核产物'
  if (options.reviewHref !== undefined) reviewLink.href = options.reviewHref
  else reviewLink.hidden = true
  separator.textContent = '|'
  moreSlot.textContent = '⋯ 更多'
  back.textContent = '返回看板'
  if (options.homeHref !== undefined) back.href = options.homeHref
  else back.hidden = true

  approve.type = 'button'
  approve.disabled = true
  approve.textContent = '批准方案并执行'
  requestChange.type = 'button'
  requestChange.disabled = true
  requestChange.textContent = '提出修改'
  const strongFlowNote = '方案批准与修改需在 StrongFlow 交付流程中处理；此页面暂不直接执行该操作。'
  approve.title = strongFlowNote
  requestChange.title = strongFlowNote

  interface RowRefs {
    readonly row: HTMLElement
    readonly term: HTMLElement
    readonly value: HTMLElement
    readonly badge: HTMLElement
    readonly detail: HTMLElement
  }

  function createRow(term: string): RowRefs {
    const row = element(document, 'div', 'wwc-task-run-row')
    row.dataset.taskRunRow = term
    const termNode = element(document, 'p', 'wwc-task-run-row-term')
    termNode.textContent = term
    const value = element(document, 'p', 'wwc-task-run-row-value')
    const badge = element(document, 'span', 'wwc-task-run-row-badge')
    badge.hidden = true
    const detail = element(document, 'p', 'wwc-task-run-row-detail')
    detail.hidden = true
    value.append(badge)
    row.append(termNode, value, detail)
    return { row, term: termNode, value, badge, detail }
  }

  const rowsByTerm = new Map(ROW_TERMS.map(term => [term, createRow(term)]))
  const clientRow = rowsByTerm.get('执行设备') as RowRefs
  const repositoryRow = rowsByTerm.get('仓库') as RowRefs
  const occupancyRow = rowsByTerm.get('占用状态') as RowRefs
  const contractRow = rowsByTerm.get('工作契约') as RowRefs
  const criteriaRow = rowsByTerm.get('验收条件') as RowRefs
  const ownerRow = rowsByTerm.get('负责人') as RowRefs
  const dependenciesRow = rowsByTerm.get('依赖项') as RowRefs
  const blockersRow = rowsByTerm.get('阻塞项') as RowRefs
  const evidenceRow = rowsByTerm.get('证据') as RowRefs
  const workerRow = rowsByTerm.get('执行会话') as RowRefs
  const candidateRow = rowsByTerm.get('候选结果') as RowRefs
  const applyRow = rowsByTerm.get('应用结果') as RowRefs
  rows.id = 'wwc-task-run-rows'
  rows.hidden = true
  rows.append(...ROW_TERMS.map(term => (rowsByTerm.get(term) as RowRefs).row))
  zone.append(identityToggle, rows, identityNotice)
  topbarActions.append(recordSlot, reviewLink, separator, moreSlot)
  topbar.append(back, topbarActions)
  actions.append(approve, requestChange)
  changesLine.textContent = options.anchor?.changes ?? ''
  changesLine.hidden = options.anchor?.changes === undefined
  acceptanceLine.textContent = options.anchor?.acceptance ?? ''
  acceptanceLine.hidden = options.anchor?.acceptance === undefined
  section.append(topbar, heading, statusLine, description, changesLine, acceptanceLine, zone, actions)
  options.root.replaceChildren(section)

  function renderBadge(refs: RowRefs, text: string | null, tone: string | null): void {
    if (text === null || tone === null) {
      refs.badge.hidden = true
      refs.badge.textContent = ''
      delete refs.badge.dataset.tone
      return
    }
    refs.badge.hidden = false
    refs.badge.textContent = text
    refs.badge.dataset.tone = tone
  }

  function renderRow(
    refs: RowRefs,
    value: string | null,
    badge: { readonly text: string; readonly tone: string } | null,
    detail: string | null,
    pendingText: string,
  ): void {
    refs.row.hidden = false
    if (value === null) {
      // A row without a fact names the gap; it never invents state.
      refs.value.classList.add('wwc-task-run-row-pending')
      refs.value.textContent = pendingText
      refs.value.append(refs.badge)
      renderBadge(refs, null, null)
    } else {
      refs.value.classList.remove('wwc-task-run-row-pending')
      refs.value.textContent = value
      refs.value.append(refs.badge)
      renderBadge(refs, badge?.text ?? null, badge?.tone ?? null)
    }
    if (detail === null) {
      refs.detail.hidden = true
      refs.detail.textContent = ''
    } else {
      refs.detail.hidden = false
      refs.detail.textContent = detail
    }
  }

  function renderWorkerSessions(sessions: readonly TaskRunWorkerSessionFacts[]): string {
    if (sessions.length === 0) return '没有正在运行的执行会话。'
    return sessions.map(session => [
      session.workerSessionId,
      session.stateText,
      session.startedAt === null ? null : `启动于 ${session.startedAt}`,
    ].filter(part => part !== null).join(' · ')).join('; ')
  }

  function renderCandidate(candidate: TaskRunCandidateFacts | null): string | null {
    if (candidate === null) return null
    return [candidate.candidateRef, candidate.branchName]
      .filter(part => part !== null)
      .join(' · ')
  }

  function renderApply(apply: TaskRunApplyFacts | null): { value: string; detail: string | null } | null {
    if (apply === null) return null
    const commit = taskRunCommitText(apply.resultingCommit)
    return {
      value: apply.resultText,
      detail: [
        `策略 ${apply.strategy}`,
        `目标分支 ${apply.targetBranch}`,
        commit === null ? null : `结果 HEAD ${commit}`,
      ].filter(part => part !== null).join(' · '),
    }
  }

  function renderCriteria(criteria: readonly TaskRunCriterionFacts[]): {
    readonly value: string
    readonly detail: string
  } | null {
    if (criteria.length === 0) return null
    return {
      value: `${String(criteria.length)} 项`,
      detail: criteria.map(criterion =>
        `${criterion.required ? '必验' : '可选'}：${criterion.description}${
          criterion.verificationMethod === null ? '' : `（${criterion.verificationMethod}）`
        }`).join('\n'),
    }
  }

  function renderEvidence(evidence: readonly TaskRunEvidenceItemFacts[]): {
    readonly value: string
    readonly detail: string
  } | null {
    if (evidence.length === 0) return null
    return {
      value: `${String(evidence.length)} 条`,
      detail: evidence.map(entry =>
        `${entry.typeText} ${entry.evidenceId} · ${entry.sourceRef}`).join('\n'),
    }
  }

  function renderIdentity(identity: TaskRunIdentityFacts | null): void {
    renderRow(
      contractRow,
      identity === null
        ? null
        : identity.contract?.objective ?? '尚未绑定工作契约。',
      null,
      identity?.contract === null || identity?.contract === undefined
        ? null
        : `${identity.contract.contractId} · 修订 ${String(identity.contract.revision)} · ${identity.contract.authorityText}`,
      '正在加载工作契约…',
    )
    const criteria = identity === null ? null : renderCriteria(identity.criteria)
    renderRow(
      criteriaRow,
      identity === null ? null : criteria?.value ?? '没有登记的验收条件。',
      null,
      criteria?.detail ?? null,
      '正在加载验收条件…',
    )
    renderRow(
      ownerRow,
      identity === null ? null : identity.owner ?? '未指定负责人。',
      null,
      null,
      '正在加载负责人…',
    )
    renderRow(
      dependenciesRow,
      identity === null
        ? null
        : identity.dependencies.length === 0 ? '没有依赖项。' : identity.dependencies.join(' · '),
      identity === null || identity.graphState === null || identity.graphStateText === null
        ? null
        : { text: identity.graphStateText, tone: runWorkGraphStateTone(identity.graphState) },
      null,
      '正在加载依赖项…',
    )
    renderRow(
      blockersRow,
      identity === null
        ? null
        : identity.blockers.length === 0 ? '没有阻塞项。' : identity.blockers.join(' · '),
      null,
      null,
      '正在加载阻塞项…',
    )
    const evidence = identity === null ? null : renderEvidence(identity.evidence)
    renderRow(
      evidenceRow,
      identity === null ? null : evidence?.value ?? '尚无证据记录。',
      null,
      evidence?.detail ?? null,
      '正在加载证据…',
    )
    renderRow(
      workerRow,
      identity === null ? null : renderWorkerSessions(identity.workerSessions),
      null,
      null,
      '正在加载执行会话信息…',
    )
    renderRow(
      candidateRow,
      identity === null
        ? null
        : renderCandidate(identity.candidate) ?? '尚无候选结果。',
      identity?.candidate == null
        ? null
        : { text: identity.candidate.stateText, tone: identity.candidate.tone },
      null,
      '正在加载候选结果信息…',
    )
    const apply = identity === null ? null : renderApply(identity.apply)
    renderRow(
      applyRow,
      apply === null ? (identity === null ? null : '尚无应用记录。') : apply.value,
      null,
      apply?.detail ?? null,
      '正在加载应用记录…',
    )
  }

  function render(snapshot: TaskRunState): void {
    if (closed) return
    section.setAttribute('aria-busy', String(snapshot.status === 'loading'))
    statusLine.textContent = statusLineText(snapshot)
    if (snapshot.taskDescription !== null) {
      description.textContent = snapshot.taskDescription
      description.hidden = false
    } else {
      description.hidden = true
      description.textContent = ''
    }
    renderRow(
      clientRow,
      snapshot.client === null ? null : snapshot.client.displayName,
      snapshot.client === null
        ? null
        : { text: snapshot.client.stateText, tone: snapshot.client.tone },
      null,
      '正在加载执行设备信息…',
    )
    renderRow(
      repositoryRow,
      snapshot.repository === null
        ? null
        : `${snapshot.repository.displayName} · 基准分支 ${snapshot.repository.defaultBranch}`,
      null,
      null,
      '正在加载仓库信息…',
    )
    renderRow(
      occupancyRow,
      snapshot.occupancy === null ? null : snapshot.occupancy.stateText,
      snapshot.occupancy === null
        ? null
        : { text: snapshot.occupancy.capacityText, tone: snapshot.occupancy.tone },
      null,
      '正在加载占用信息…',
    )
    renderIdentity(snapshot.identity)
    identityNotice.hidden = snapshot.identityStatus !== 'unavailable'
    identityNotice.textContent = snapshot.identityStatus === 'unavailable'
      ? '任务详情当前不可达。执行设备、仓库与占用状态保留最后已知值。'
      : ''
  }

  const unsubscribe = options.model.subscribe(render)

  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      options.root.replaceChildren()
    },
  }
}
