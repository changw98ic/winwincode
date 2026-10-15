// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientCandidates, ControlPlaneClientDirectory, ControlPlaneCandidateSummary, ControlPlaneCandidateApplyReceipt, ControlPlaneRepositorySummary } from './community-control-plane-client.js'
import type { StrongFlowReviewViewModel, StrongFlowReviewState } from './strongflow-review-view-model.js'
import { repositoryDisplayName } from './display-labels.js'
import { publicErrorText } from './public-redaction.js'

export interface CandidateApplicationOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowReviewViewModel
  readonly candidates: ControlPlaneClientCandidates
  readonly directory: ControlPlaneClientDirectory
  readonly claim: (clientId: string) => Promise<unknown>
  readonly onStatusChange?: (status: CandidateApplicationStatus) => void
}

export type CandidateApplicationStatus = 'loading' | 'applied' | 'not-applied' | 'unavailable'

export function candidateCanApply(state: StrongFlowReviewState): boolean {
  const detail = state.detail
  const candidate = detail?.currentCandidate
  const verdict = detail?.verdict
  return state.status === 'ready' && candidate != null && verdict != null
    && typeof candidate.candidateId === 'string' && typeof verdict.snapshotId === 'string'
    && verdict.status === 'pass' && verdict.candidateId === candidate.candidateId
    && verdict.candidateRef === candidate.candidateRef
    && candidate.deliverySpecId === detail?.requirements.deliverySpecId
    && candidate.deliverySpecRevision === detail?.requirements.deliverySpecRevision
    && verdict.deliverySpecId === detail?.requirements.deliverySpecId
    && verdict.deliverySpecRevision === detail?.requirements.deliverySpecRevision
    && verdict.unresolvedFindings.length === 0
    && detail.requirements.acceptanceCriteria.filter(item => item.required).every(item => (
      verdict.criteria.some(result => result.criterionId === item.id && result.verdict === 'pass' && result.evidenceRefs.length > 0
        && result.evidenceRefs.every(id => detail.evidence.some(evidence => evidence.id === id
          && evidence.candidateId === candidate.candidateId
          && evidence.snapshotId === verdict.snapshotId
          && evidence.deliverySpecId === candidate.deliverySpecId
          && evidence.deliverySpecRevision === candidate.deliverySpecRevision)))
    ))
}

const RESULTS: Record<string, string> = {
  applied: '已应用到项目', base_stale: '目标分支已变化，请重新生成候选版本',
  working_tree_dirty: '目标工作区有未提交改动，请先处理改动', merge_conflict: '合并发生冲突，可创建候选分支继续处理',
  candidate_missing: '设备上的候选版本已不可用', permission_denied: '没有应用此版本的权限',
  failed: '应用失败，请处理失败原因并重新生成候选版本',
  outcome_unknown: '应用结果尚未确认，请刷新设备结果',
}

const APPLICATION_STATUS = {
  applying: '正在应用。设备恢复连接后会核对原操作，请刷新设备结果。',
  applied: '设备已确认应用完成。',
  stale: '目标分支已偏离候选基线，请重新生成候选版本。',
  awaiting_retry: '设备确认上次操作尚未开始。检查原分支后，可重试原应用。',
  blocked: '上次应用结果尚未确认。已暂停再次应用，请检查设备并刷新结果。',
  failed: '设备已确认应用失败。请处理失败原因并重新生成候选版本。',
}

export function mountCandidateApplication(options: CandidateApplicationOptions): { close(): void } {
  const document = options.root.ownerDocument
  const node = <K extends keyof HTMLElementTagNameMap>(tag: K, text = '') => {
    const value = document.createElement(tag); value.textContent = text; return value
  }
  const form = node('form')
  form.className = 'wwc-candidate-application'
  const heading = node('h3', '应用到项目')
  const status = node('p')
  status.setAttribute('role', 'status')
  const eligibility = node('p')
  eligibility.className = 'wwc-candidate-eligibility'
  const destination = node('select')
  destination.id = 'wwc-apply-destination'
  const destinationLabel = node('label', '项目与执行设备')
  destinationLabel.htmlFor = destination.id; destinationLabel.append(destination)
  const branch = node('input')
  branch.id = 'wwc-apply-branch'; branch.required = true; branch.maxLength = 255
  const branchLabel = node('label', '目标分支')
  branchLabel.htmlFor = branch.id; branchLabel.append(branch)
  const head = node('input')
  head.id = 'wwc-apply-head'; head.required = true; head.pattern = '(?:[0-9a-f]{40}|[0-9a-f]{64})'
  head.readOnly = true
  const headLabel = node('label', '候选版本基线提交')
  headLabel.htmlFor = head.id; headLabel.append(head)
  const details = node('details')
  const commitResult = node('p')
  details.append(node('summary', '分支检查详情'), headLabel, node('p', '目标分支必须仍停在候选版本的基线提交。设备会在应用前再次核对。'), commitResult)
  const confirm = node('input'); confirm.type = 'checkbox'; confirm.required = true
  const confirmLabel = node('label')
  confirmLabel.append(confirm, document.createTextNode('已检查变更和验收结果，确认应用到此分支'))
  const apply = node('button', '应用到项目'); apply.type = 'submit'
  const refresh = node('button', '刷新设备结果'); refresh.type = 'button'
  const createBranch = node('button', '创建候选分支处理冲突'); createBranch.type = 'button'; createBranch.hidden = true
  const fields = node('div'); fields.className = 'wwc-candidate-application-fields'
  fields.append(destinationLabel, branchLabel, confirmLabel, apply)
  form.append(heading, eligibility, status, fields, details, refresh, createBranch)
  options.root.replaceChildren(form)
  type Target = { clientId: string; candidate: ControlPlaneCandidateSummary; repository: ControlPlaneRepositorySummary }
  let targets: Target[] = []
  let identity: string | null = null
  let busy = false
  let closed = false
  let generation = 0
  function canApply(candidate: ControlPlaneCandidateSummary): boolean {
    const application = candidate.application
    return (candidate.state === 'retained' || candidate.state === 'branch_created')
      && (application == null || (application.status === 'awaiting_retry'
        && application.baseCommit === candidate.baseCommit))
  }
  function lock(): void {
    const selected = targets[destination.selectedIndex]?.candidate
    const applied = selected?.state === 'applied' || selected?.application?.status === 'applied'
    fields.hidden = applied
    eligibility.hidden = applied
    headLabel.hidden = applied
    eligibility.textContent = busy || options.model.state.status === 'refreshing'
      ? '正在核对当前版本和验收结果。'
      : candidateCanApply(options.model.state)
      ? '当前版本的必需验收项均已通过，可检查变更后应用。'
      : '当前版本尚未通过全部必需验收项，请先查看验收结果和证据。'
    const disabled = applied || busy || !candidateCanApply(options.model.state)
      || selected === undefined || !canApply(selected)
    apply.disabled = disabled
    apply.textContent = selected?.application?.status === 'awaiting_retry' ? '重试原应用' : '应用到项目'
    for (const control of [destination, branch, head, confirm]) control.disabled = disabled
    if (selected?.application != null) branch.disabled = true
    refresh.disabled = busy
    createBranch.disabled = busy || targets.length === 0
  }
  function receipt(result: ControlPlaneCandidateApplyReceipt): void {
    status.textContent = `${RESULTS[result.result] ?? result.result} · ${result.targetBranch}`
    commitResult.textContent = result.resultingCommit === null ? '' : `应用后的提交：${result.resultingCommit}`
    createBranch.hidden = result.result !== 'merge_conflict'
    options.onStatusChange?.(result.result === 'applied' ? 'applied'
      : result.result === 'outcome_unknown' ? 'unavailable' : 'not-applied')
  }
  function select(): void {
    const target = targets[destination.selectedIndex]
    if (target === undefined) return
    createBranch.hidden = true
    commitResult.textContent = ''
    branch.value = target.candidate.application?.targetBranch ?? target.repository.defaultBranch
    head.value = target.candidate.application?.baseCommit ?? target.candidate.baseCommit
    confirm.checked = false
    const previous = target.candidate.history.at(-1)
    const application = target.candidate.application
    if (application != null) {
      status.textContent = APPLICATION_STATUS[application.status]
      if (application.baseCommit !== target.candidate.baseCommit) status.textContent = '应用记录与当前候选基线不一致，请刷新设备结果。'
      options.onStatusChange?.(application.status === 'applied' ? 'applied'
        : application.status === 'applying' || application.status === 'blocked' ? 'unavailable' : 'not-applied')
    } else if (previous !== undefined) receipt(previous)
    else { status.textContent = '该版本尚未应用到项目。'; options.onStatusChange?.('not-applied') }
    lock()
    if (target.candidate.state === 'applied') apply.disabled = true
  }
  async function load(): Promise<void> {
    options.onStatusChange?.('loading')
    const candidate = options.model.state.detail?.currentCandidate
    const current = ++generation
    targets = []; destination.replaceChildren(); createBranch.hidden = true; lock()
    if (candidate == null) { status.textContent = '等待产生可验收的版本。'; options.onStatusChange?.('unavailable'); return }
    status.textContent = '正在读取设备保留的版本…'
    try {
      const devices = await options.directory.listClients()
      const entries: { target: Target; label: string }[] = []
      for (const device of devices) {
        const [candidates, repositories] = await Promise.all([
          options.candidates.listDeviceCandidates({ clientId: device.clientId }),
          options.directory.listRepositories({ clientId: device.clientId }),
        ])
        for (const local of candidates) {
          const repository = repositories.find(item => item.repositoryBindingId === local.repositoryBindingId)
          if (repository === undefined || local.candidateCommit !== candidate.candidateCommitId || local.state === 'discarded') continue
          entries.push({ target: { clientId: device.clientId, candidate: local, repository }, label: `${repositoryDisplayName(repository.displayName, repository.repositoryBindingId, document.defaultView)} · ${device.displayName}` })
        }
      }
      if (closed || current !== generation) return
      targets = entries.map(entry => entry.target)
      destination.replaceChildren(...entries.map((entry, index) => {
        const option = node('option', entry.label); option.value = String(index); return option
      }))
      if (targets.length === 0) { status.textContent = '设备尚未保留这个版本，连接执行设备后刷新。'; options.onStatusChange?.('unavailable') }
      else select()
    } catch { if (!closed && current === generation) { status.textContent = '无法读取设备版本，请检查连接后重试。'; options.onStatusChange?.('unavailable') } }
    finally { if (!closed && current === generation) lock() }
  }
  async function submit(createOnly: boolean): Promise<void> {
    const target = targets[destination.selectedIndex]
    if (busy || target === undefined || (!createOnly && (!canApply(target.candidate)
      || !candidateCanApply(options.model.state) || !confirm.checked || !form.reportValidity()))) return
    const commit = options.model.state.detail?.currentCandidate?.candidateCommitId
    const selectedBranch = branch.value.trim(), expectedHead = head.value.trim()
    busy = true; lock(); status.textContent = createOnly ? '正在创建候选分支…' : '正在核对版本并应用…'
    try {
      await options.model.refresh()
      if (closed) return
      if (!createOnly) {
        const candidates = await options.candidates.listDeviceCandidates({ clientId: target.clientId })
        if (closed) return
        const current = candidates.find(candidate => candidate.candidateRef === target.candidate.candidateRef
          && candidate.repositoryBindingId === target.repository.repositoryBindingId
          && candidate.candidateCommit === commit)
        if (current === undefined) throw new Error('设备上的候选版本已变化，请刷新设备结果。')
        target.candidate = current
        if (!canApply(current)) throw new Error(current.application == null
          ? '当前候选版本不能再次应用。' : APPLICATION_STATUS[current.application.status])
        if (expectedHead !== current.baseCommit || (current.application != null
          && selectedBranch !== current.application.targetBranch)) throw new Error('应用参数与原操作不一致，请刷新设备结果。')
      }
      if (options.model.state.detail?.currentCandidate?.candidateCommitId !== commit || (!createOnly && !candidateCanApply(options.model.state))) throw new Error('版本或验收结果已变化，请重新检查。')
      if (!createOnly && commit !== undefined) await options.model.acceptDelivery(commit)
      if (closed) return
      if (!createOnly && (options.model.state.detail?.currentCandidate?.candidateCommitId !== commit || !candidateCanApply(options.model.state))) throw new Error('交付确认后版本已变化，请重新检查。')
      await options.claim(target.clientId)
      if (closed) return
      const input = { clientId: target.clientId, candidateRef: target.candidate.candidateRef, repositoryBindingId: target.repository.repositoryBindingId }
      if (createOnly) {
        const outcome = await options.candidates.createCandidateBranch(input)
        if (!closed) status.textContent = `已创建候选分支 ${outcome.branchName}，可在项目中检查并处理冲突。`
      } else {
        const result = await options.candidates.applyCandidate({ ...input, targetBranch: selectedBranch, expectedHead })
        if (result.candidateRef !== input.candidateRef || result.repositoryBindingId !== input.repositoryBindingId || result.targetBranch !== selectedBranch || result.expectedHead !== expectedHead) throw new Error('应用回执与当前操作不一致，请刷新确认。')
        if (!closed) { receipt(result); if (result.result === 'applied') target.candidate = { ...target.candidate, state: 'applied' } }
      }
    } catch (error) { if (!closed) status.textContent = publicErrorText(error, '应用请求未完成') }
    finally { if (!closed && !createOnly) await load(); busy = false; if (!closed) lock() }
  }
  form.addEventListener('submit', event => { event.preventDefault(); void submit(false) })
  createBranch.addEventListener('click', () => { void submit(true) })
  destination.addEventListener('change', select)
  branch.addEventListener('input', () => { confirm.checked = false })
  head.addEventListener('input', () => { confirm.checked = false })
  refresh.addEventListener('click', () => { void options.model.refresh().then(load) })
  const unsubscribe = options.model.subscribe(state => {
    const next = state.detail?.currentCandidate?.candidateCommitId ?? null
    if (identity !== next || (!busy && targets.length === 0 && state.status === 'ready' && state.detail?.verdict?.status === 'pass')) { identity = next; void load() }
    lock()
  })
  void load()
  return { close() { closed = true; generation += 1; unsubscribe(); options.root.replaceChildren() } }
}
