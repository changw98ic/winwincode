// SPDX-License-Identifier: Apache-2.0

import { boundApprovalText } from './approval-risk-detail.js'
import { mountPanel } from './components/panel.js'
import { mountKeyedCollection } from './components/keyed-collection.js'
import type { InteractiveInputValue } from './generated/contracts.js'
import {
  contextualDecisionCapability,
  contextualDecisionKindLabel,
  type ContextualDecisionItem,
  type ContextualDecisionPresentation,
  type ContextualDecisionView,
} from './contextual-decision-view-model.js'

/**
 * UI-502 one decision a mounted page can submit.  Every action goes through the
 * page's own view-model command, so the card adds no second mutation authority
 * and no second copy of the server state behind the decision.
 */
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
  /** Stable panel id; it also namespaces the per-row control ids. */
  readonly id: string
  readonly title?: string
  readonly description?: string
  readonly className?: string
  readonly actions?: ContextualDecisionActions
  /**
   * Canonical link to the surface that owns this decision when the mounted page
   * does not decide it itself.  `null` renders no link for that row.
   */
  readonly detailHref?: (item: ContextualDecisionItem) => string | null
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
}

export interface ContextualDecisionCardUpdate {
  readonly view: ContextualDecisionView
  readonly presentation: ContextualDecisionPresentation
  /** Static host context, such as where the same Delivery review is decided. */
  readonly note?: string | null
}

export interface ContextualDecisionCard {
  readonly root: HTMLElement
  update(input: ContextualDecisionCardUpdate): void
  close(): void
}

const CONTEXT_ENTRIES = 5

const STALE_TEXT = 'This decision is no longer current. Refresh for the current state.'
const LOCKED_TEXT = 'Decisions are unavailable in this page state.'
const NOTE_REQUIRED_TEXT = 'Explain this decision before submitting it.'
const RESPONSE_REQUIRED_TEXT = 'Enter the response this input asks for.'

/** Why one row cannot be decided right now: the row itself, or the page state. */
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

function noteLabel(kind: ContextualDecisionItem['kind']): string {
  if (kind === 'approval') return 'Decision reason'
  if (kind === 'attention') return 'Decision note'
  return 'Response'
}

/**
 * A row is decided inline only when the mounted page actually owns that
 * command; otherwise the row is a summary that opens the owning surface.
 */
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
  readonly submit: HTMLButtonElement
  readonly secondary: HTMLButtonElement
  readonly rejected: HTMLParagraphElement
  readonly detail: HTMLAnchorElement
  readonly onSubmit: () => void
  readonly onSecondary: () => void
}

/**
 * Mount the bounded decision card of one Session/StageRun context.  The card is
 * a projection with the page's own commands behind it: it is hidden when the
 * context has no decision, and it is never a live region, so the page keeps its
 * single polite announcement channel.
 */
export function mountContextualDecisionCard(
  options: ContextualDecisionCardOptions,
): ContextualDecisionCard {
  const document = options.root.ownerDocument
  const defaultTitle = options.title ?? 'Decisions in this context'
  const defaultDescription = options.description
    ?? 'The inputs, approvals, and Attention this exact context is waiting on.'
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

  // The capability of the latest card update; row handlers read it at the
  // moment of activation so a stale row can never submit behind the page.
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
      if (mode === null || mode !== 'text') return
      const value = parts.response.value.trim()
      if (value.length === 0 && item.allowEmpty !== true) {
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

  const collection = mountKeyedCollection<ContextualDecisionItem, string, HTMLLIElement>({
    parent: list,
    // Kind and identity are one stable decision; a row never changes kind.
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
        submit,
        secondary,
        rejected,
        detail,
        onSubmit,
        onSecondary,
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
      // Producer summaries are free-form, so a card never renders one raw.
      parts.title.textContent = boundApprovalText(item.title).text
      updateDecisionContext(parts.context, [
        contextualDecisionKindLabel(item.kind),
        capability.stateLabel,
        item.stageRunId === null ? 'ProductSession-bound' : 'ProductSession and StageRun-bound',
        item.deliveryId === null ? 'No Delivery binding' : 'Delivery-bound',
        item.expiresAt === null ? 'No expiry deadline' : `Expires ${item.expiresAt}`,
      ])
      const inline = decidesInline(item, options.actions)
      const textMode = inline && (item.kind === 'input' ? item.mode === 'text' : item.requiresNote)
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
      parts.submit.hidden = !inline || (item.kind === 'input' && item.mode !== 'text')
      parts.submit.textContent = item.kind === 'approval'
        ? 'Approve'
        : item.kind === 'attention'
          ? 'Resolve'
          : 'Submit response'
      parts.submit.disabled = capability.disabled
      parts.secondary.hidden = !inline || (item.kind === 'input' && item.mode !== 'text')
      parts.secondary.textContent = item.kind === 'approval'
        ? 'Reject'
        : item.kind === 'attention'
          ? 'Dismiss'
          : 'Cancel input'
      parts.secondary.disabled = capability.disabled
      // A stale row keeps whatever the user already typed: a rejected decision
      // never costs the user their input, and the text clears only when the
      // server stops listing this decision and the row is removed.
      parts.rejected.hidden = !inline || !capability.disabled
      if (inline && capability.disabled) parts.rejected.textContent = rejectionText(item)
      // Navigation stays available while the page refreshes; only a decision
      // that can no longer be made loses its link.
      const href = item.urgency === 'expired' || item.urgency === 'binding-invalid'
        ? null
        : options.detailHref?.(item) ?? null
      parts.detail.hidden = href === null
      if (href === null) {
        parts.detail.removeAttribute('href')
        parts.detail.textContent = ''
      } else {
        parts.detail.href = href
        parts.detail.textContent = 'Open the owning decision surface'
      }
    },
    remove(row) {
      const parts = rows.get(row)
      if (parts === undefined) return
      parts.response.value = ''
      parts.submit.removeEventListener('click', parts.onSubmit)
      parts.secondary.removeEventListener('click', parts.onSecondary)
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
      // The host note takes precedence; without one the card shows its own
      // count so the cost of the context stays readable at a glance.
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
