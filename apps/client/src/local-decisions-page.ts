// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientError } from './control-plane-client.js'
import type {
  ChatInputInteractionProjection,
  InteractiveInputValue,
} from './generated/contracts.js'
import type {
  LocalApprovalDecision,
  LocalAttentionDecision,
  LocalDecisionsViewModel,
  LocalDecisionsViewModelState,
  LocalInputDecision,
} from './local-decisions-view-model.js'

export interface LocalDecisionsPageOptions {
  readonly root: HTMLElement
  readonly model: LocalDecisionsViewModel
}

export interface LocalDecisionsPage {
  close(): void
}

export interface LocalDecisionsPagePresentation {
  readonly statusText: string
  readonly errorText: string | null
  readonly busy: boolean
  readonly retryVisible: boolean
  readonly reconnectVisible: boolean
  readonly decisionsDisabled: boolean
}

function knownDecisionError(error: ControlPlaneClientError): string | null {
  const labels: Readonly<Record<string, string>> = Object.freeze({
    LOCAL_DECISIONS_COMMAND_IN_FLIGHT: 'Wait for the current decision to finish.',
    LOCAL_DECISIONS_INPUT_STALE: 'Refresh and select a current pending input.',
    LOCAL_DECISIONS_INPUT_EXPIRED: 'This pending input expired. Refresh for the current list.',
    LOCAL_DECISIONS_INPUT_BINDING_INVALID: 'This input no longer matches the current session.',
    LOCAL_DECISIONS_INPUT_MODE_INVALID: 'Use the response type requested by the current input.',
    LOCAL_DECISIONS_INPUT_VALUE_REQUIRED: 'Enter a response for the current input.',
    LOCAL_DECISIONS_INPUT_OPTION_STALE: 'Choose one of the current input options.',
    LOCAL_DECISIONS_APPROVAL_STALE: 'Refresh and select a current pending approval.',
    LOCAL_DECISIONS_APPROVAL_EXPIRED: 'This approval expired. Refresh for the current list.',
    LOCAL_DECISIONS_APPROVAL_BINDING_INVALID: 'This approval no longer matches the current session.',
    LOCAL_DECISIONS_APPROVAL_REASON_REQUIRED: 'Explain this approval decision.',
    LOCAL_DECISIONS_ATTENTION_STALE: 'Refresh and select a current open Attention item.',
    LOCAL_DECISIONS_ATTENTION_RESOLUTION_REQUIRED: 'Explain this Attention decision.',
    LOCAL_DECISIONS_ATTENTION_BINDING_INVALID: 'The current Attention list is inconsistent. Refresh before deciding.',
    LOCAL_DECISIONS_DELIVERY_BINDING_INVALID: 'This Attention item no longer matches the current Delivery.',
    LOCAL_DECISIONS_REVISION_MISMATCH: 'The decision result belongs to another revision. Refresh and review it.',
    INVALID_CLIENT_REQUEST: 'Check the local user identity and repository scope, then retry.',
  })
  return labels[error.code] ?? null
}

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
  const known = knownDecisionError(error)
  if (known !== null) return known
  if (error.code === 'REVISION_CONFLICT') {
    return 'This item changed before the decision was saved. Review the current snapshot and try again.'
  }
  if (error.kind === 'authentication') return 'Sign in again to manage local decisions.'
  if (error.kind === 'authorization') return 'You do not have access to these local decisions.'
  if (error.kind === 'network') return 'The local Control Plane could not be reached. Check the connection and retry.'
  if (error.kind === 'version') return 'The Client and Server versions differ. Update the Client and retry.'
  if (error.kind === 'cancelled') return 'The local decision update was cancelled.'
  if (error.kind === 'configuration') {
    return 'Check the local server URL and repository scope configuration, then retry.'
  }
  return 'Local decisions could not be updated. Retry, or review the server status.'
}

export function localDecisionsPagePresentation(
  state: LocalDecisionsViewModelState,
): LocalDecisionsPagePresentation {
  const visibleError = state.interaction.error ?? state.error
  const pendingCount = state.inputs.filter(item => !item.expired).length
    + state.approvals.filter(item => !item.expired).length
    + state.attention.length
  const statusText = state.interaction.status === 'submitting'
    ? 'Submitting decision…'
    : state.interaction.status === 'waiting'
      ? 'Decision accepted · waiting for the current snapshot…'
      : state.status === 'loading'
        ? 'Loading local decisions…'
        : state.status === 'refreshing' || state.realtime === 'reloading'
          ? 'Updating local decisions…'
          : state.realtime === 'reconnecting'
            ? 'Reconnecting…'
            : state.status === 'authentication-required'
              ? 'Sign in required'
              : state.status === 'authorization-denied'
                ? 'Access denied'
                : state.status === 'cancelled'
                  ? 'Update cancelled'
                  : state.status === 'error'
                    ? 'Local decisions unavailable'
                    : state.status === 'closed'
                      ? 'Local decisions closed'
                      : `Ready · ${String(pendingCount)} pending decisions`
  const busy = state.status === 'loading'
    || state.status === 'refreshing'
    || state.realtime === 'reloading'
    || state.interaction.status === 'submitting'
    || state.interaction.status === 'waiting'
  return Object.freeze({
    statusText,
    errorText: errorLabel(visibleError),
    busy,
    retryVisible: visibleError !== null && state.realtime !== 'reconnecting',
    reconnectVisible: state.realtime === 'reconnecting',
    decisionsDisabled: busy
      || state.status === 'authentication-required'
      || state.status === 'authorization-denied'
      || state.status === 'closed',
  })
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
  const list = element(document, 'ul', 'wwc-local-decision-context')
  for (const entry of entries) {
    const item = document.createElement('li')
    item.textContent = entry
    list.append(item)
  }
  return list
}

function inputStateLabel(input: LocalInputDecision): string {
  return input.expired ? 'Expired · submission disabled' : 'Pending'
}

function approvalStateLabel(approval: LocalApprovalDecision): string {
  return approval.expired ? 'Expired · decision disabled' : 'Pending'
}

function inputModeLabel(input: ChatInputInteractionProjection): string {
  if (input.mode === 'text') return 'Text response'
  if (input.mode === 'confirmation') return 'Confirmation'
  return 'Choose one option'
}

function attentionTypeLabel(item: LocalAttentionDecision): string {
  if (item.projection.type === 'requirement_question') return 'Requirement question'
  if (item.projection.type === 'decision_required') return 'Decision required'
  if (item.projection.type === 'verification_blocked') return 'Verification blocked'
  if (item.projection.type === 'scope_change') return 'Scope change'
  return 'Delivery approval'
}

/** Mount secret-safe input, approval, and business Attention controls against one view-model. */
export function mountLocalDecisionsPage(options: LocalDecisionsPageOptions): LocalDecisionsPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'main', 'wwc-local-decisions')
  const heading = element(document, 'h1', 'wwc-local-decisions-heading')
  const help = element(document, 'p', 'wwc-local-decisions-help')
  const status = element(document, 'p', 'wwc-local-decisions-status')
  const error = element(document, 'div', 'wwc-local-decisions-error')
  const errorText = element(document, 'span', 'wwc-local-decisions-error-text')
  const retry = element(document, 'button', 'wwc-local-decisions-retry')
  const reconnect = element(document, 'button', 'wwc-local-decisions-reconnect')
  const inputsSection = element(document, 'section', 'wwc-local-inputs')
  const inputsHeading = element(document, 'h2', 'wwc-local-decisions-section-heading')
  const inputs = element(document, 'ul', 'wwc-local-input-list')
  const approvalsSection = element(document, 'section', 'wwc-local-approvals')
  const approvalsHeading = element(document, 'h2', 'wwc-local-decisions-section-heading')
  const approvals = element(document, 'ul', 'wwc-local-approval-list')
  const attentionSection = element(document, 'section', 'wwc-local-attention')
  const attentionHeading = element(document, 'h2', 'wwc-local-decisions-section-heading')
  const attention = element(document, 'ul', 'wwc-local-attention-list')
  let closed = false

  heading.textContent = 'Local decisions'
  help.textContent = 'Inputs include exact current choices such as Plan Delta or Replan. Submitted responses are cleared immediately.'
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  error.setAttribute('role', 'alert')
  error.setAttribute('aria-live', 'assertive')
  retry.type = 'button'
  retry.textContent = 'Retry snapshot'
  reconnect.type = 'button'
  reconnect.textContent = 'Reconnect events'
  error.append(errorText, retry, reconnect)
  inputsHeading.textContent = 'Pending inputs'
  approvalsHeading.textContent = 'Tool approvals'
  attentionHeading.textContent = 'Business Attention'
  inputs.setAttribute('aria-live', 'polite')
  approvals.setAttribute('aria-live', 'polite')
  attention.setAttribute('aria-live', 'polite')
  inputsSection.append(inputsHeading, inputs)
  approvalsSection.append(approvalsHeading, approvals)
  attentionSection.append(attentionHeading, attention)
  layout.append(
    heading,
    help,
    status,
    error,
    inputsSection,
    approvalsSection,
    attentionSection,
  )
  options.root.replaceChildren(layout)

  function renderInput(
    item: LocalInputDecision,
    disabled: boolean,
    inputIndex: number,
  ): HTMLLIElement {
    const projection = item.projection
    const row = element(document, 'li', 'wwc-local-input')
    const title = element(document, 'h3', 'wwc-local-input-prompt')
    const context = decisionContext(document, [
      inputModeLabel(projection),
      inputStateLabel(item),
      projection.binding.sessionIdentity.stageRunId === undefined
        ? 'ProductSession-bound'
        : 'ProductSession and StageRun-bound',
      'Execution job and Worker session-bound',
      `Revision ${String(projection.revision)}`,
      `Expires ${projection.expiresAt}`,
    ])
    const controls = element(document, 'div', 'wwc-local-input-controls')
    const cancel = element(document, 'button', 'wwc-local-input-cancel')
    const decisionDisabled = disabled || item.expired
    title.textContent = projection.prompt
    cancel.type = 'button'
    cancel.textContent = 'Cancel input'
    cancel.disabled = decisionDisabled
    cancel.addEventListener('click', () => { void options.model.cancelInput(projection.inputRequestId) })

    if (projection.mode === 'text') {
      const form = element(document, 'form', 'wwc-local-input-form')
      const label = element(document, 'label', 'wwc-local-input-response-label')
      const response = element(document, 'textarea', 'wwc-local-input-response')
      const submit = element(document, 'button', 'wwc-local-input-submit')
      const inputId = `wwc-local-input-response-${String(inputIndex)}`
      label.htmlFor = inputId
      label.textContent = 'Response'
      response.id = inputId
      response.autocomplete = 'off'
      response.spellcheck = false
      response.disabled = decisionDisabled
      submit.type = 'submit'
      submit.textContent = 'Submit response'
      submit.disabled = decisionDisabled
      form.addEventListener('submit', event => {
        event.preventDefault()
        const value: InteractiveInputValue = { mode: projection.mode, value: response.value }
        response.value = ''
        void options.model.provideInput(projection.inputRequestId, value)
      })
      label.append(response)
      form.append(label, submit)
      controls.append(form)
    } else {
      const choices = element(document, 'div', 'wwc-local-input-options')
      for (const option of projection.options) {
        const choice = element(document, 'button', 'wwc-local-input-option')
        choice.type = 'button'
        choice.textContent = option.label
        choice.disabled = decisionDisabled
        choice.addEventListener('click', () => {
          const value: InteractiveInputValue = { mode: projection.mode, value: option.value }
          void options.model.provideInput(projection.inputRequestId, value)
        })
        choices.append(choice)
      }
      controls.append(choices)
    }
    controls.append(cancel)
    row.append(title, context, controls)
    return row
  }

  function renderApproval(
    item: LocalApprovalDecision,
    disabled: boolean,
    approvalIndex: number,
  ): HTMLLIElement {
    const projection = item.projection
    const row = element(document, 'li', 'wwc-local-approval')
    const title = element(document, 'h3', 'wwc-local-approval-subject')
    const context = decisionContext(document, [
      approvalStateLabel(item),
      projection.binding.sessionIdentity.stageRunId === undefined
        ? 'ProductSession-bound'
        : 'ProductSession and StageRun-bound',
      'Execution job and Worker session-bound',
      `Revision ${String(projection.revision)}`,
      `Expires ${projection.expiresAt}`,
    ])
    const form = element(document, 'form', 'wwc-local-approval-form')
    const label = element(document, 'label', 'wwc-local-approval-reason-label')
    const reason = element(document, 'textarea', 'wwc-local-approval-reason')
    const controls = element(document, 'div', 'wwc-local-approval-controls')
    const approve = element(document, 'button', 'wwc-local-approval-approve')
    const reject = element(document, 'button', 'wwc-local-approval-reject')
    const decisionDisabled = disabled || item.expired
    const inputId = `wwc-local-approval-reason-${String(approvalIndex)}`
    title.textContent = projection.subject
    label.htmlFor = inputId
    label.textContent = 'Decision reason'
    reason.id = inputId
    reason.autocomplete = 'off'
    reason.spellcheck = false
    reason.disabled = decisionDisabled
    approve.type = 'button'
    approve.textContent = 'Approve'
    approve.disabled = decisionDisabled
    reject.type = 'button'
    reject.textContent = 'Reject'
    reject.disabled = decisionDisabled
    const decide = (decision: 'approve' | 'reject') => {
      const explanation = reason.value
      reason.value = ''
      void options.model.decideApproval(projection.id, decision, explanation)
    }
    approve.addEventListener('click', () => { decide('approve') })
    reject.addEventListener('click', () => { decide('reject') })
    label.append(reason)
    controls.append(approve, reject)
    form.append(label, controls)
    row.append(title, context, form)
    return row
  }

  function renderAttention(
    item: LocalAttentionDecision,
    disabled: boolean,
    attentionIndex: number,
  ): HTMLLIElement {
    const projection = item.projection
    const row = element(document, 'li', 'wwc-local-attention-item')
    const title = element(document, 'h3', 'wwc-local-attention-title')
    const context = decisionContext(document, [
      attentionTypeLabel(item),
      projection.blocking ? 'Blocking' : 'Non-blocking',
      projection.stageRunId === null ? 'Delivery-bound' : 'Delivery and StageRun-bound',
      item.candidateDigest === null ? 'No candidate is currently bound' : 'Current candidate-bound',
      `Delivery revision ${String(item.deliveryRevision)}`,
    ])
    const choices = element(document, 'ul', 'wwc-local-attention-options')
    const form = element(document, 'form', 'wwc-local-attention-form')
    const label = element(document, 'label', 'wwc-local-attention-resolution-label')
    const resolution = element(document, 'textarea', 'wwc-local-attention-resolution')
    const controls = element(document, 'div', 'wwc-local-attention-controls')
    const resolve = element(document, 'button', 'wwc-local-attention-resolve')
    const dismiss = element(document, 'button', 'wwc-local-attention-dismiss')
    const inputId = `wwc-local-attention-resolution-${String(attentionIndex)}`
    title.textContent = projection.title
    for (const option of projection.options) {
      const optionItem = document.createElement('li')
      const optionTitle = element(document, 'strong', 'wwc-local-attention-option-label')
      const optionDescription = element(document, 'span', 'wwc-local-attention-option-description')
      optionTitle.textContent = option.label
      optionDescription.textContent = option.description
      optionItem.append(optionTitle, optionDescription)
      choices.append(optionItem)
    }
    label.htmlFor = inputId
    label.textContent = 'Resolution'
    resolution.id = inputId
    resolution.autocomplete = 'off'
    resolution.spellcheck = false
    resolution.disabled = disabled
    resolve.type = 'button'
    resolve.textContent = 'Resolve'
    resolve.disabled = disabled
    dismiss.type = 'button'
    dismiss.textContent = 'Dismiss'
    dismiss.disabled = disabled
    const decide = (decision: 'resolve' | 'dismiss') => {
      const explanation = resolution.value
      resolution.value = ''
      void options.model.resolveAttention(projection.id, decision, explanation)
    }
    resolve.addEventListener('click', () => { decide('resolve') })
    dismiss.addEventListener('click', () => { decide('dismiss') })
    label.append(resolution)
    controls.append(resolve, dismiss)
    form.append(label, controls)
    row.append(title, context, choices, form)
    return row
  }

  function empty(label: string, className: string): HTMLLIElement {
    const item = element(document, 'li', className)
    item.textContent = label
    return item
  }

  function render(state: LocalDecisionsViewModelState): void {
    if (closed) return
    const presentation = localDecisionsPagePresentation(state)
    status.textContent = presentation.statusText
    layout.setAttribute('aria-busy', String(presentation.busy))
    error.hidden = presentation.errorText === null
    errorText.textContent = presentation.errorText ?? ''
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    inputs.replaceChildren(...state.inputs.map((item, index) => (
      renderInput(item, presentation.decisionsDisabled, index)
    )))
    approvals.replaceChildren(...state.approvals.map((item, index) => (
      renderApproval(item, presentation.decisionsDisabled, index)
    )))
    attention.replaceChildren(...state.attention.map((item, index) => (
      renderAttention(item, presentation.decisionsDisabled, index)
    )))
    if (state.inputs.length === 0) inputs.append(empty('No pending inputs.', 'wwc-local-input-empty'))
    if (state.approvals.length === 0) {
      approvals.append(empty('No pending tool approvals.', 'wwc-local-approval-empty'))
    }
    if (state.attention.length === 0) {
      attention.append(empty('No open business Attention.', 'wwc-local-attention-empty'))
    }
  }

  retry.addEventListener('click', () => { void options.model.refresh() })
  reconnect.addEventListener('click', () => { options.model.reconnect() })
  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
