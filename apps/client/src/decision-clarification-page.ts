// SPDX-License-Identifier: Apache-2.0

import type {
  ClarificationState,
  ClarificationViewModel,
} from './decision-clarification-view-model.js'

export interface DecisionClarificationPageOptions {
  readonly root: HTMLElement
  readonly model: ClarificationViewModel
  readonly backHref?: string
  readonly window?: Window
}

export interface DecisionClarificationPage {
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

const FIELD_OPTIONS = Object.freeze([
  { field: 'title', label: '标题', required: true, rows: 1 },
  { field: 'goal', label: '目标', required: true, rows: 5 },
  { field: 'scope', label: '范围内（每行一项）', required: false, rows: 4 },
  { field: 'outOfScope', label: '范围外（每行一项）', required: false, rows: 4 },
  { field: 'constraints', label: '约束（每行一项）', required: false, rows: 4 },
] as const)

/** Fixed native controls are the whole render protocol; arbitrary HTML/actions never enter it. */
export function mountDecisionClarificationPage(
  options: DecisionClarificationPageOptions,
): DecisionClarificationPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-decision-clarification')
  const topbar = element(document, 'div', 'wwc-decision-clarification-topbar')
  const back = element(document, 'a', 'wwc-decision-clarification-back')
  const refresh = element(document, 'button', 'wwc-decision-clarification-refresh')
  const heading = element(document, 'h2', 'wwc-decision-clarification-heading')
  const notice = element(document, 'p', 'wwc-decision-clarification-notice')
  const binding = element(document, 'p', 'wwc-decision-clarification-binding')
  const form = element(document, 'form', 'wwc-decision-clarification-form')
  const fields = element(document, 'div', 'wwc-decision-clarification-fields')
  const criteriaHeading = element(document, 'h3', 'wwc-decision-clarification-section-heading')
  const criteria = element(document, 'div', 'wwc-decision-clarification-criteria')
  const addCriterion = element(document, 'button', 'wwc-decision-clarification-add')
  const impactHeading = element(document, 'h3', 'wwc-decision-clarification-section-heading')
  const impact = element(document, 'div', 'wwc-decision-clarification-impact')
  const conflicts = element(document, 'div', 'wwc-decision-clarification-conflicts')
  const submit = element(document, 'button', 'wwc-decision-clarification-submit')
  let closed = false
  let renderedFormRevision = -1
  let editableControls: (HTMLInputElement | HTMLTextAreaElement | HTMLButtonElement)[] = []

  section.setAttribute('aria-labelledby', 'wwc-decision-clarification-heading')
  heading.id = 'wwc-decision-clarification-heading'
  heading.textContent = '编辑需求与验收'
  back.textContent = '返回任务'
  if (options.backHref === undefined) back.hidden = true
  else back.href = options.backHref
  refresh.type = 'button'
  refresh.textContent = '检查最新版本'
  refresh.addEventListener('click', () => { void options.model.refresh() })
  notice.setAttribute('role', 'status')
  criteriaHeading.textContent = '验收条件'
  addCriterion.type = 'button'
  addCriterion.textContent = '添加验收条件'
  addCriterion.addEventListener('click', () => options.model.addCriterion())
  impactHeading.textContent = '生效影响预览'
  submit.type = 'submit'
  submit.textContent = '提交规范修订'
  form.addEventListener('submit', event => {
    event.preventDefault()
    void options.model.submit()
  })

  function renderFields(state: ClarificationState): void {
    fields.replaceChildren()
    for (const definition of FIELD_OPTIONS) {
      const row = element(document, 'div', 'wwc-decision-clarification-field')
      const label = element(document, 'label', 'wwc-decision-clarification-label')
      const id = `wwc-decision-clarification-${definition.field}`
      label.htmlFor = id
      label.textContent = definition.label
      const control = definition.rows === 1
        ? element(document, 'input', 'wwc-decision-clarification-input')
        : element(document, 'textarea', 'wwc-decision-clarification-input')
      control.id = id
      control.value = state.values[definition.field]
      control.required = definition.required
      control.setAttribute('maxlength', definition.field === 'title' ? '500' : '65536')
      if ('rows' in control) control.rows = definition.rows
      control.addEventListener('input', () => options.model.edit(definition.field, control.value))
      editableControls.push(control)
      const deferred = element(document, 'div', 'wwc-decision-clarification-deferred')
      const unknown = element(document, 'button', 'wwc-decision-clarification-unknown')
      const later = element(document, 'button', 'wwc-decision-clarification-later')
      unknown.type = 'button'
      later.type = 'button'
      unknown.textContent = '不知道'
      later.textContent = '稍后补充'
      unknown.addEventListener('click', () => {
        options.model.markUnknown(definition.field)
        control.value = options.model.state.values[definition.field]
      })
      later.addEventListener('click', () => {
        options.model.markLater(definition.field)
        control.value = options.model.state.values[definition.field]
      })
      editableControls.push(unknown, later)
      deferred.append(unknown, later)
      row.append(label, control, deferred)
      fields.append(row)
    }
  }

  function renderCriteria(state: ClarificationState): void {
    criteria.replaceChildren()
    for (const criterion of state.criteria) {
      const row = element(document, 'fieldset', 'wwc-decision-clarification-criterion')
      const legend = element(document, 'legend', 'wwc-decision-clarification-criterion-id')
      const label = element(document, 'label', 'wwc-decision-clarification-label')
      const title = element(document, 'input', 'wwc-decision-clarification-input')
      const requiredLabel = element(document, 'label', 'wwc-decision-clarification-required')
      const required = element(document, 'input', 'wwc-decision-clarification-required-input')
      const method = element(document, 'p', 'wwc-decision-clarification-method')
      const remove = element(document, 'button', 'wwc-decision-clarification-remove')
      const inputId = `wwc-decision-criterion-${criterion.id}`
      legend.textContent = criterion.id
      label.htmlFor = inputId
      label.textContent = '验收描述'
      title.id = inputId
      title.value = criterion.title
      title.required = true
      title.setAttribute('maxlength', '2000')
      title.addEventListener('input', () => options.model.updateCriterion(criterion.id, { title: title.value }))
      required.type = 'checkbox'
      required.checked = criterion.required
      required.addEventListener('change', () => options.model.updateCriterion(criterion.id, { required: required.checked }))
      requiredLabel.textContent = '必须验收'
      requiredLabel.append(required)
      method.textContent = criterion.verificationMethod === null
        ? '验证方式：由服务端在提交时确认'
        : `验证方式：${criterion.verificationMethod}`
      remove.type = 'button'
      remove.textContent = '删除'
      remove.addEventListener('click', () => options.model.removeCriterion(criterion.id))
      editableControls.push(title, required, remove)
      row.append(legend, label, title, requiredLabel, method, remove)
      criteria.append(row)
    }
  }

  function renderImpact(state: ClarificationState): void {
    impact.replaceChildren()
    const candidate = element(document, 'p', 'wwc-decision-clarification-impact-warning')
    candidate.textContent = state.snapshot?.candidateRef === null || state.snapshot === null
      ? '提交会创建新的规范与工作契约修订。'
      : `提交会创建新的规范与工作契约修订；当前候选 ${state.snapshot.candidateRef} 将保留为历史记录，但不再授权当前执行。`
    impact.append(candidate)
    if (state.changes.length === 0) {
      const empty = element(document, 'p', 'wwc-decision-clarification-impact-empty')
      empty.textContent = '没有待提交改动。'
      impact.append(empty)
      return
    }
    const list = element(document, 'ul', 'wwc-decision-clarification-change-list')
    for (const change of state.changes) {
      const item = element(document, 'li', 'wwc-decision-clarification-change')
      item.textContent = `${change.label}：${change.before || '（空）'} → ${change.after || '（空）'}`
      list.append(item)
    }
    impact.append(list)
  }

  function renderConflicts(state: ClarificationState): void {
    conflicts.replaceChildren()
    conflicts.hidden = state.conflicts.length === 0
    if (state.conflicts.length === 0) return
    const title = element(document, 'p', 'wwc-decision-clarification-conflict-title')
    title.textContent = '检测到并发修改：'
    const list = element(document, 'ul', 'wwc-decision-clarification-conflict-list')
    for (const conflict of state.conflicts) {
      const item = element(document, 'li', 'wwc-decision-clarification-conflict')
      item.textContent = `${conflict.field}：服务端“${conflict.serverValue}”，本页“${conflict.draftValue}”`
      list.append(item)
    }
    const keep = element(document, 'button', 'wwc-decision-clarification-keep')
    const useServer = element(document, 'button', 'wwc-decision-clarification-use-server')
    keep.type = 'button'
    useServer.type = 'button'
    keep.textContent = '保留本页草稿'
    useServer.textContent = '采用服务端值'
    keep.addEventListener('click', () => options.model.resolveConflicts('keep-draft'))
    useServer.addEventListener('click', () => options.model.resolveConflicts('use-server'))
    conflicts.append(title, list, keep, useServer)
  }

  function render(state: ClarificationState): void {
    if (closed) return
    notice.hidden = state.notice === null
    notice.textContent = state.notice ?? ''
    form.hidden = state.status !== 'editing'
    refresh.disabled = state.busy
    submit.disabled = !state.dirty || state.busy || state.offline || state.conflicts.length > 0
    addCriterion.disabled = state.busy
    binding.hidden = state.snapshot === null
    if (state.snapshot !== null) {
      binding.textContent = [
        state.snapshot.actorId,
        state.snapshot.productSessionId ?? '无产品会话',
        `交付修订 ${String(state.snapshot.deliveryRevision)}`,
        `规范修订 ${String(state.snapshot.deliverySpecRevision)}`,
        state.snapshot.candidateRef ?? '无当前候选',
        state.lastRequestId ?? '尚未提交',
      ].join(' · ')
    }
    if (state.status === 'editing' && state.formRevision !== renderedFormRevision) {
      renderedFormRevision = state.formRevision
      editableControls = []
      renderFields(state)
      renderCriteria(state)
    }
    for (const control of editableControls) control.disabled = state.busy
    renderImpact(state)
    renderConflicts(state)
  }

  topbar.append(back, refresh)
  form.append(fields, criteriaHeading, criteria, addCriterion, impactHeading, impact, conflicts, submit)
  section.append(topbar, heading, notice, binding, form)
  options.root.replaceChildren(section)
  const unsubscribe = options.model.subscribe(render)
  const onOnline = () => options.model.setOffline(false)
  const onOffline = () => options.model.setOffline(true)
  options.window?.addEventListener('online', onOnline)
  options.window?.addEventListener('offline', onOffline)
  if (options.window !== undefined) options.model.setOffline(options.window.navigator.onLine === false)
  render(options.model.state)

  return {
    close() {
      closed = true
      unsubscribe()
      options.window?.removeEventListener('online', onOnline)
      options.window?.removeEventListener('offline', onOffline)
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
