// SPDX-License-Identifier: Apache-2.0

import type {
  AcceptanceCriterionDraft,
  ClarificationComponent,
  ClarificationState,
  ClarificationViewModel,
} from './decision-clarification-view-model.js'
import { estimateDisplayText, missingRequiredFields } from './decision-clarification-view-model.js'

export interface DecisionClarificationPageOptions {
  readonly root: HTMLElement
  readonly model: ClarificationViewModel
  readonly onOpenAdvanced?: () => void
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

function componentKindLabel(component: ClarificationComponent): string {
  switch (component.kind) {
    case 'form': return '表单'
    case 'single-select': return '单选'
    case 'multi-select': return '多选'
    case 'text': return '文本'
    case 'table': return '表格'
    case 'comparison': return '方案对比'
    case 'action': return '动作'
  }
}

function criterionLine(criterion: AcceptanceCriterionDraft): string {
  const method = criterion.verificationMethod === null ? '未指定验证方法' : criterion.verificationMethod
  return `${criterion.required ? '必须' : '可选'} · ${criterion.title} · ${method}`
}

/**
 * DEC-08 progressive entry: Quick never auto-opens the full form; the advanced
 * control is explicit. Every rendered string is bound text, never HTML.
 */
export function mountDecisionClarificationPage(
  options: DecisionClarificationPageOptions,
): DecisionClarificationPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-decision-clarification')
  const heading = element(document, 'h2', 'wwc-decision-clarification-heading')
  const notice = element(document, 'p', 'wwc-decision-clarification-notice')
  const advanced = element(document, 'button', 'wwc-decision-clarification-advanced')
  const componentList = element(document, 'div', 'wwc-decision-clarification-components')
  const offlineToggle = element(document, 'button', 'wwc-decision-clarification-offline')
  const submit = element(document, 'button', 'wwc-decision-clarification-submit')
  const bindingLine = element(document, 'p', 'wwc-decision-clarification-binding')

  let closed = false

  section.setAttribute('aria-label', '需求澄清与方案对比')
  heading.textContent = '需求澄清与方案对比'
  heading.id = 'wwc-decision-clarification-heading'
  section.setAttribute('aria-labelledby', heading.id)
  notice.setAttribute('role', 'status')
  advanced.type = 'button'
  advanced.textContent = '展开高级需求 / 升级 StrongFlow'
  offlineToggle.type = 'button'
  offlineToggle.textContent = '标记离线草稿'
  submit.type = 'button'
  submit.textContent = '提交澄清'
  bindingLine.hidden = true

  function renderComponents(components: readonly ClarificationComponent[], state: ClarificationState): void {
    componentList.replaceChildren()
    if (components.length === 0) {
      const empty = element(document, 'p', 'wwc-decision-clarification-empty')
      empty.textContent = '没有可渲染的合法澄清组件。'
      componentList.append(empty)
      return
    }
    const answers = state.status === 'editing' ? state.draft.answers : []
    for (const component of components) {
      const card = element(document, 'article', 'wwc-decision-clarification-card')
      card.dataset.componentKind = component.kind
      const title = element(document, 'h3', 'wwc-decision-clarification-card-title')
      title.textContent = `${componentKindLabel(component)} · ${component.title}`
      card.append(title)

      if (component.estimate !== undefined) {
        const estimate = element(document, 'p', 'wwc-decision-clarification-estimate')
        estimate.textContent = estimateDisplayText(component.estimate)
        card.append(estimate)
      }

      if (component.kind === 'comparison') {
        const table = element(document, 'ul', 'wwc-decision-clarification-comparison')
        for (const row of component.rows ?? []) {
          const item = element(document, 'li', 'wwc-decision-clarification-comparison-row')
          item.textContent = `${row.label}: ${row.left} ↔ ${row.right}`
          table.append(item)
        }
        card.append(table)
      }

      if (component.criteria !== undefined) {
        const list = element(document, 'ul', 'wwc-decision-clarification-criteria')
        for (const criterion of component.criteria) {
          const item = element(document, 'li', 'wwc-decision-clarification-criterion')
          item.textContent = criterionLine(criterion)
          list.append(item)
        }
        card.append(list)
      }

      for (const field of component.fields ?? []) {
        const row = element(document, 'div', 'wwc-decision-clarification-field')
        const label = element(document, 'label', 'wwc-decision-clarification-label')
        label.textContent = field.label
        const input = element(document, 'input', 'wwc-decision-clarification-input')
        input.type = 'text'
        input.setAttribute('data-field-id', field.id)
        const answer = answers.find(item => item.fieldId === field.id)
        input.value = answer?.text ?? ''
        input.addEventListener('change', () => {
          options.model.setAnswer({
            fieldId: field.id,
            text: input.value,
            choiceIds: [],
            unknown: false,
            later: false,
          })
        })
        const later = element(document, 'button', 'wwc-decision-clarification-later')
        later.type = 'button'
        later.textContent = '稍后补充'
        later.disabled = !field.allowLater
        later.addEventListener('click', () => options.model.markLater(field.id))
        const unknown = element(document, 'button', 'wwc-decision-clarification-unknown')
        unknown.type = 'button'
        unknown.textContent = '不知道'
        unknown.disabled = !field.allowUnknown
        unknown.addEventListener('click', () => options.model.markUnknown(field.id))
        row.append(label, input, later, unknown)
        card.append(row)
      }

      if (state.status === 'editing' && (component.fields?.length ?? 0) > 0) {
        const missing = missingRequiredFields(component, state.draft.answers)
        const gap = element(document, 'p', 'wwc-decision-clarification-missing')
        gap.hidden = missing.length === 0
        gap.textContent = missing.length === 0
          ? ''
          : `本卡片缺少必填项：${missing.join('、')}`
        card.append(gap)
      }

      componentList.append(card)
    }
  }

  function render(state: ClarificationState): void {
    if (closed) return
    if (state.status === 'empty') {
      notice.textContent = '尚未加载澄清内容。'
      componentList.replaceChildren()
      submit.disabled = true
      offlineToggle.disabled = true
      bindingLine.hidden = true
      return
    }
    if (state.status === 'submitted') {
      notice.textContent = '澄清已提交。'
      submit.disabled = true
      offlineToggle.disabled = true
      bindingLine.hidden = false
      bindingLine.textContent = [
        state.binding.userId,
        state.binding.productSessionId,
        `rev ${String(state.binding.deliveryRevision)}`,
        state.binding.candidateRef ?? 'no-candidate',
        state.binding.requestId,
      ].join(' · ')
      return
    }
    notice.hidden = state.notice === null
    notice.textContent = state.notice ?? ''
    offlineToggle.textContent = state.draft.offline ? '取消离线草稿标记' : '标记离线草稿'
    submit.disabled = state.blockedHighRisk || state.draft.offline
    bindingLine.hidden = true
    renderComponents(state.components, state)
  }

  advanced.addEventListener('click', () => {
    options.onOpenAdvanced?.()
  })
  offlineToggle.addEventListener('click', () => {
    if (options.model.state.status !== 'editing') return
    options.model.setOffline(!options.model.state.draft.offline)
  })
  submit.addEventListener('click', () => {
    options.model.submit({
      userId: 'user_local',
      productSessionId: 'psn_local',
      deliveryRevision: 1,
      candidateRef: null,
    })
  })

  const unsubscribe = options.model.subscribe(render)
  section.append(heading, notice, advanced, componentList, offlineToggle, submit, bindingLine)
  options.root.replaceChildren(section)
  render(options.model.state)

  return {
    close() {
      closed = true
      unsubscribe()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
