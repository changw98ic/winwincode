// SPDX-License-Identifier: Apache-2.0

import { boundApprovalText } from './components/bounded-text.js'
import { mountPanel } from '@winwincode/browser-ui'
import { mountKeyedCollection } from './components/keyed-collection.js'
import type { InteractiveInputValue } from './generated/contracts.js'
import {
  contextualDecisionCapability,
  contextualDecisionKindLabel,
  type ContextualDecisionItem,
  type ContextualDecisionPresentation,
  type ContextualDecisionView,
} from './contextual-decision-view-model.js'

export interface ContextualDecisionActions {
  readonly provideInput?: (item: ContextualDecisionItem, value: InteractiveInputValue) => void
  readonly cancelInput?: (item: ContextualDecisionItem) => void
  readonly decideApproval?: (
    item: ContextualDecisionItem,
    decision: 'approve' | 'reject',
    reason: string,
  ) => void
  readonly resolveAttention?: (
    item: ContextualDecisionItem,
    decision: 'resolve' | 'dismiss',
    resolution: string,
  ) => void
}

export interface ContextualDecisionCardOptions {
  readonly root: HTMLElement
  readonly id: string
  readonly title?: string
  readonly description?: string
  readonly className?: string
  readonly actions?: ContextualDecisionActions
  readonly detailHref?: (item: ContextualDecisionItem) => string | null
  readonly readOnly?: boolean
}

export interface ContextualDecisionCardUpdate {
  readonly view: ContextualDecisionView
  readonly presentation: ContextualDecisionPresentation
  readonly note?: string | null
}

export interface ContextualDecisionCard {
  readonly root: HTMLElement
  update(input: ContextualDecisionCardUpdate): void
  close(): void
}

const CONTEXT_ENTRIES = 5

const STALE_TEXT = '此决策已不是当前状态，请刷新后重试。'
const LOCKED_TEXT = '当前页面状态下无法处理决策。'
const NOTE_REQUIRED_TEXT = '提交前请说明决策原因。'
const RESPONSE_REQUIRED_TEXT = '请输入此请求需要的回复。'
const RESPONSE_TOO_LONG_TEXT = '回复不能超过 20000 个字符。'

function rejectionText(item: ContextualDecisionItem): string {
  return item.urgency === 'expired' || item.urgency === 'binding-invalid'
    ? STALE_TEXT
    : LOCKED_TEXT
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

function decisionContext(document: Document, entries: readonly string[]): HTMLUListElement {
  const list = element(document, 'ul', 'wwc-contextual-decision-context')
  for (const entry of entries) {
    const item = document.createElement('li')
    item.textContent = entry
    list.append(item)
  }
  return list
}

function updateDecisionContext(list: HTMLUListElement, entries: readonly string[]): void {
  entries.forEach((entry, index) => {
    const item = list.children[index]
    if (item !== undefined && item.textContent !== entry) item.textContent = entry
  })
}

function inputValueTooLong(value: string): boolean {
  const codePoints = value[Symbol.iterator]()
  for (let count = 0; count <= 20000; count += 1) {
    if (codePoints.next().done) return false
  }
  return true
}

function noteLabel(kind: ContextualDecisionItem['kind']): string {
  if (kind === 'approval') return '决策理由'
  if (kind === 'attention') return '决策说明'
  return '回复'
}

function decidesInline(
  item: ContextualDecisionItem,
  actions: ContextualDecisionActions | undefined,
): boolean {
  if (item.kind === 'input') {
    return actions?.provideInput !== undefined || actions?.cancelInput !== undefined
  }
  if (item.kind === 'approval') return actions?.decideApproval !== undefined
  return actions?.resolveAttention !== undefined
}

interface DecisionRow {
  current: ContextualDecisionItem
  readonly title: HTMLElement
  readonly context: HTMLUListElement
  readonly responseLabel: HTMLLabelElement
  readonly response: HTMLTextAreaElement
  readonly optionButtons: readonly HTMLButtonElement[]
  readonly optionHandlers: readonly (() => void)[]
  readonly optionList: HTMLElement
  readonly other: HTMLButtonElement
  otherSelected: boolean
  readonly submit: HTMLButtonElement
  readonly secondary: HTMLButtonElement
  readonly rejected: HTMLParagraphElement
  readonly detail: HTMLAnchorElement
  readonly onSubmit: () => void
  readonly onSecondary: () => void
  readonly onOther: () => void
}

export function mountContextualDecisionCard(
  options: ContextualDecisionCardOptions,
): ContextualDecisionCard {
  const document = options.root.ownerDocument
  const defaultTitle = options.title ?? '当前上下文中的决策'
  const defaultDescription = options.description
    ?? '此上下文正在等待处理的输入、审批和待办事项。'
  const className = options.className ?? 'wwc-contextual-decision'
  const panel = mountPanel({
    document,
    props: {
      id: options.id,
      title: defaultTitle,
      description: defaultDescription,
      headingLevel: 3,
      className,
    },
  })
  const note = element(document, 'p', 'wwc-contextual-decision-note')
  const list = element(document, 'ul', 'wwc-contextual-decision-list')
  const omitted = element(document, 'p', 'wwc-contextual-decision-omitted')
  note.hidden = true
  omitted.hidden = true
  panel.content.append(note, list, omitted)
  options.root.replaceChildren(panel.root)

  let presentation: ContextualDecisionPresentation = Object.freeze({
    statusText: '',
    decisionsDisabled: false,
    counts: Object.freeze({ blocking: 0, pending: 0, expired: 0, bindingInvalid: 0 }),
  })
  const rows = new Map<HTMLLIElement, DecisionRow>()

  function showRejection(parts: DecisionRow, text: string): void {
    parts.rejected.textContent = text
    parts.rejected.hidden = false
  }

  function refuse(parts: DecisionRow): void {
    showRejection(parts, rejectionText(parts.current))
  }

  function decidePrimary(row: HTMLLIElement): void {
    const parts = rows.get(row)
    if (parts === undefined) return
    const item = parts.current
    if (contextualDecisionCapability(item, presentation).disabled) {
      refuse(parts)
      return
    }
    if (item.kind === 'input') {
      const mode = item.mode
      const otherMode = mode === 'single_choice' && item.allowOther && parts.otherSelected
      if (mode === null || (mode !== 'text' && !otherMode)) return
      const value = parts.response.value
      if (inputValueTooLong(value)) {
        showRejection(parts, RESPONSE_TOO_LONG_TEXT)
        return
      }
      if (value.trim().length === 0 && item.allowEmpty !== true) {
        showRejection(parts, RESPONSE_REQUIRED_TEXT)
        return
      }
      options.actions?.provideInput?.(item, { mode, value })
      return
    }
    const text = parts.response.value
    if (text.trim().length === 0) {
      showRejection(parts, NOTE_REQUIRED_TEXT)
      return
    }
    if (item.kind === 'approval') options.actions?.decideApproval?.(item, 'approve', text)
    else if (item.kind === 'attention') options.actions?.resolveAttention?.(item, 'resolve', text)
  }

  function decideSecondary(row: HTMLLIElement): void {
    const parts = rows.get(row)
    if (parts === undefined) return
    const item = parts.current
    if (contextualDecisionCapability(item, presentation).disabled) {
      refuse(parts)
      return
    }
    if (item.kind === 'input') {
      options.actions?.cancelInput?.(item)
      return
    }
    const text = parts.response.value
    if (text.trim().length === 0) {
      showRejection(parts, NOTE_REQUIRED_TEXT)
      return
    }
    if (item.kind === 'approval') options.actions?.decideApproval?.(item, 'reject', text)
    else if (item.kind === 'attention') options.actions?.resolveAttention?.(item, 'dismiss', text)
  }

  function decideOption(row: HTMLLIElement, value: string): void {
    const parts = rows.get(row)
    if (parts === undefined) return
    const item = parts.current
    const mode = item.mode
    if (item.kind !== 'input' || mode === null || mode === 'text') return
    if (contextualDecisionCapability(item, presentation).disabled) {
      refuse(parts)
      return
    }
    if (!item.options.some(option => option.value === value)) return
    options.actions?.provideInput?.(item, { mode, value })
  }

  function chooseOther(row: HTMLLIElement): void {
    const parts = rows.get(row)
    if (parts === undefined) return
    const item = parts.current
    if (item.kind !== 'input' || item.mode !== 'single_choice' || !item.allowOther) return
    if (contextualDecisionCapability(item, presentation).disabled) {
      refuse(parts)
      return
    }
    parts.otherSelected = true
    parts.other.setAttribute('aria-pressed', 'true')
    parts.responseLabel.hidden = false
    parts.submit.hidden = false
    parts.secondary.hidden = false
    parts.rejected.hidden = true
  }

  const collection = mountKeyedCollection<ContextualDecisionItem, string, HTMLLIElement>({
    parent: list,
    key: item => `${item.kind}:${item.id}`,
    create(item: ContextualDecisionItem) {
      const row = element(document, 'li', 'wwc-contextual-decision-item')
      const title = element(document, 'h4', 'wwc-contextual-decision-title')
      const context = decisionContext(document, Array.from({ length: CONTEXT_ENTRIES }, () => ''))
      const responseLabel = element(document, 'label', 'wwc-contextual-decision-response-label')
      const response = element(document, 'textarea', 'wwc-contextual-decision-response')
      const submit = element(document, 'button', 'wwc-contextual-decision-submit')
      const secondary = element(document, 'button', 'wwc-contextual-decision-secondary')
      const optionList = element(document, 'div', 'wwc-contextual-decision-options')
      const other = element(document, 'button', 'wwc-contextual-decision-option')
      other.type = 'button'
      other.dataset.wwcComponent = 'button'
      other.dataset.variant = 'default'
      other.dataset.wwcInputOther = 'true'
      other.textContent = '其他'
      other.setAttribute('aria-pressed', 'false')
      const rejected = element(document, 'p', 'wwc-contextual-decision-rejected')
      const detail = element(document, 'a', 'wwc-contextual-decision-detail')
      response.id = `${options.id}-response-${item.kind}-${item.id}`
      responseLabel.htmlFor = response.id
      response.autocomplete = 'off'
      response.spellcheck = false
      submit.type = 'button'
      submit.dataset.wwcComponent = 'button'
      submit.dataset.variant = 'primary'
      secondary.type = 'button'
      secondary.dataset.wwcComponent = 'button'
      secondary.dataset.variant = 'destructive'
      rejected.hidden = true
      detail.hidden = true
      const optionHandlers: (() => void)[] = []
      const optionButtons = item.options.map(option => {
        const choice = element(document, 'button', 'wwc-contextual-decision-option')
        choice.type = 'button'
        choice.dataset.wwcComponent = 'button'
        choice.dataset.variant = 'default'
        if (option.value !== null) choice.dataset.wwcOptionValue = option.value
        const onChoose = () => {
          if (option.value !== null) decideOption(row, option.value)
        }
        choice.addEventListener('click', onChoose)
        optionHandlers.push(onChoose)
        optionList.append(choice)
        return choice
      })
      const onOther = () => { chooseOther(row) }
      other.addEventListener('click', onOther)
      optionList.append(other)
      const onSubmit = () => { decidePrimary(row) }
      const onSecondary = () => { decideSecondary(row) }
      submit.addEventListener('click', onSubmit)
      secondary.addEventListener('click', onSecondary)
      responseLabel.textContent = noteLabel(item.kind)
      responseLabel.append(response)
      row.append(title, context, responseLabel, optionList, submit, secondary, rejected, detail)
      rows.set(row, {
        current: item,
        title,
        context,
        responseLabel,
        response,
        optionButtons,
        optionHandlers,
        optionList,
        other,
        otherSelected: false,
        submit,
        secondary,
        rejected,
        detail,
        onSubmit,
        onSecondary,
        onOther,
      })
      return row
    },
    update(row, item: ContextualDecisionItem) {
      const parts = rows.get(row)
      if (parts === undefined) return
      const capability = contextualDecisionCapability(item, presentation)
      parts.current = item
      row.dataset.kind = item.kind
      row.dataset.urgency = item.urgency
      parts.title.textContent = boundApprovalText(item.title).text
      updateDecisionContext(parts.context, [
        contextualDecisionKindLabel(item.kind),
        capability.stateLabel,
        item.workRunId === null ? '绑定 ProductSession' : '绑定 ProductSession 与 WorkRun',
        item.deliveryId === null ? '未绑定交付' : '已绑定交付',
        item.expiresAt === null ? '没有过期时间' : `过期时间 ${item.expiresAt}`,
      ])
      const inline = decidesInline(item, options.actions)
      const otherAllowed = item.kind === 'input' && item.mode === 'single_choice' && item.allowOther
      if (!otherAllowed) parts.otherSelected = false
      parts.other.hidden = !inline || !otherAllowed
      parts.other.disabled = capability.disabled
      parts.other.setAttribute('aria-pressed', String(parts.otherSelected))
      const inputTextMode = item.mode === 'text' || (otherAllowed && parts.otherSelected)
      const textMode = inline && (item.kind === 'input' ? inputTextMode : item.requiresNote)
      parts.responseLabel.hidden = !textMode
      parts.response.disabled = capability.disabled
      const choiceMode = inline && item.kind === 'input' && item.mode !== 'text'
      parts.optionList.hidden = !choiceMode
      parts.optionButtons.forEach((button, index) => {
        const option = item.options[index]
        button.hidden = !choiceMode || option === undefined
        button.disabled = capability.disabled
        if (option !== undefined) {
          if (option.value !== null) button.dataset.wwcOptionValue = option.value
          button.textContent = boundApprovalText(option.label).text
        }
      })
      parts.submit.hidden = !inline || (item.kind === 'input' && !inputTextMode)
      parts.submit.textContent = item.kind === 'approval'
        ? '批准'
        : item.kind === 'attention'
          ? '解决'
          : '提交回复'
      parts.submit.disabled = capability.disabled
      parts.secondary.hidden = !inline || (item.kind === 'input' && !inputTextMode)
      parts.secondary.textContent = item.kind === 'approval'
        ? '拒绝'
        : item.kind === 'attention'
          ? '忽略'
          : '取消输入'
      parts.secondary.disabled = capability.disabled
      parts.rejected.hidden = !inline || !capability.disabled
      if (inline && capability.disabled) parts.rejected.textContent = rejectionText(item)
      const href = item.urgency === 'expired' || item.urgency === 'binding-invalid'
        ? null
        : options.detailHref?.(item) ?? null
      parts.detail.hidden = href === null
      if (href === null) {
        parts.detail.removeAttribute('href')
        parts.detail.textContent = ''
      } else {
        parts.detail.href = href
        parts.detail.textContent = '打开所属决策页面'
      }
    },
    remove(row) {
      const parts = rows.get(row)
      if (parts === undefined) return
      parts.response.value = ''
      parts.submit.removeEventListener('click', parts.onSubmit)
      parts.secondary.removeEventListener('click', parts.onSecondary)
      parts.other.removeEventListener('click', parts.onOther)
      parts.optionButtons.forEach((button, index) => {
        const handler = parts.optionHandlers[index]
        if (handler !== undefined) button.removeEventListener('click', handler)
      })
      rows.delete(row)
    },
  })

  return {
    root: panel.root,
    update(input: ContextualDecisionCardUpdate) {
      presentation = input.presentation
      panel.update({
        id: options.id,
        title: defaultTitle,
        description: defaultDescription,
        headingLevel: 3,
        className,
        busy: input.presentation.decisionsDisabled,
      })
      const noteText = input.note ?? (
        input.presentation.statusText.length === 0 ? null : input.presentation.statusText
      )
      note.hidden = noteText === null || noteText.length === 0
      if (noteText !== null && noteText.length > 0) note.textContent = noteText
      collection.update(input.view.items)
      omitted.hidden = input.view.omitted === 0
      if (input.view.omitted > 0) {
        omitted.textContent = `${String(input.view.omitted)} more decisions not shown.`
      }
      panel.root.hidden = input.view.items.length === 0
    },
    close() {
      collection.close()
      panel.close()
    },
  }
}
