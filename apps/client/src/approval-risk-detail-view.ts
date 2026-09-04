// SPDX-License-Identifier: Apache-2.0

import type { ApprovalFieldKey, ApprovalRiskDetail } from './approval-risk-detail.js'

export interface ApprovalRiskDetailViewOptions {
  readonly document: Document
  readonly className?: string
  readonly headingLevel?: 3 | 4 | 5
}

export interface ApprovalRiskDetailView {
  readonly root: HTMLElement
  update(detail: ApprovalRiskDetail): void
  close(): void
}

interface FieldParts {
  readonly item: HTMLDivElement
  readonly term: HTMLElement
  readonly command: HTMLElement
  readonly value: HTMLElement
  readonly note: HTMLElement
}

const RISK_TONES: Readonly<Record<ApprovalRiskDetail['risk']['level'], string>> = Object.freeze({
  high: 'danger',
  elevated: 'warning',
  moderate: 'info',
  unknown: 'neutral',
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

const FIELD_KEYS: readonly ApprovalFieldKey[] = Object.freeze([
  'command',
  'cwd',
  'fileImpact',
  'networkTargets',
  'mcpTarget',
  'requestedReason',
])

/**
 * Mount the fixed secret-safe risk block of one Approval card.  Updating only
 * rewrites text nodes, so the structure an operator reads never changes under
 * a refresh.
 */
export function mountApprovalRiskDetail(
  options: ApprovalRiskDetailViewOptions,
): ApprovalRiskDetailView {
  const document = options.document
  const headingLevel = options.headingLevel ?? 4
  const root = element(document, 'section', options.className ?? 'wwc-approval-risk')
  root.dataset.wwcComponent = 'approval-risk'
  const heading = headingLevel === 3
    ? element(document, 'h3', 'wwc-approval-risk-heading')
    : headingLevel === 4
      ? element(document, 'h4', 'wwc-approval-risk-heading')
      : element(document, 'h5', 'wwc-approval-risk-heading')
  heading.textContent = 'Risk and impact'
  root.setAttribute('aria-labelledby', 'wwc-approval-risk-heading')
  const summary = element(document, 'p', 'wwc-approval-risk-summary')
  const level = element(document, 'span', 'wwc-approval-risk-level')
  const impact = element(document, 'span', 'wwc-approval-risk-impact')
  summary.append(level, impact)
  const rationale = element(document, 'p', 'wwc-approval-risk-rationale')
  const statement = element(document, 'p', 'wwc-approval-risk-impact-statement')
  const fields = element(document, 'dl', 'wwc-approval-risk-fields')
  const fieldParts = new Map<ApprovalFieldKey, FieldParts>()
  for (const key of FIELD_KEYS) {
    const item = element(document, 'div', 'wwc-approval-risk-field')
    item.dataset.fieldKey = key
    const term = element(document, 'dt', 'wwc-approval-risk-field-label')
    const command = element(document, 'code', 'wwc-approval-risk-command-text')
    const value = element(document, 'span', 'wwc-approval-risk-field-text')
    const note = element(document, 'span', 'wwc-approval-risk-field-note')
    const description = element(document, 'dd', 'wwc-approval-risk-field-value')
    description.append(command, value, note)
    item.append(term, description)
    fields.append(item)
    fieldParts.set(key, { item, term, command, value, note })
  }
  const scope = element(document, 'p', 'wwc-approval-risk-scope')
  const expiry = element(document, 'p', 'wwc-approval-risk-expiry')
  const target = element(document, 'p', 'wwc-approval-risk-target')
  root.append(heading, summary, rationale, statement, fields, scope, expiry, target)

  let open = true

  function renderField(detail: ApprovalRiskDetail, key: ApprovalFieldKey): void {
    const parts = fieldParts.get(key)
    if (parts === undefined) return
    const field = detail.fieldByKey[key]
    parts.item.dataset.availability = field.availability
    parts.term.textContent = field.label
    const commandVisible = field.text !== null
    parts.command.hidden = !commandVisible
    parts.command.textContent = commandVisible ? field.text : ''
    parts.value.hidden = commandVisible
    parts.value.textContent = commandVisible
      ? ''
      : (field.withheldLabel ?? '')
    parts.note.hidden = field.note === null
    parts.note.textContent = field.note ?? ''
  }

  return {
    root,
    update(detail: ApprovalRiskDetail): void {
      if (!open) return
      root.dataset.risk = detail.risk.level
      root.dataset.impact = detail.impact
      // One Approval carries one risk block, so the heading id stays unique.
      heading.id = `wwc-approval-risk-heading-${detail.approvalId}`
      root.setAttribute('aria-labelledby', heading.id)
      level.dataset.tone = RISK_TONES[detail.risk.level]
      level.textContent = detail.risk.label
      impact.textContent = detail.impactLabel
      rationale.textContent = detail.risk.rationale
      statement.hidden = detail.impactStatements.length === 0
      statement.textContent = detail.impactStatements.join(' ')
      for (const key of FIELD_KEYS) renderField(detail, key)
      scope.textContent = `${detail.decisionScope.label} · ${detail.decisionScope.detail}`
      expiry.textContent = detail.expiry.label
      target.textContent = detail.executionTarget.label
    },
    close() {
      if (!open) return
      open = false
      root.replaceChildren()
      root.remove()
    },
  }
}
