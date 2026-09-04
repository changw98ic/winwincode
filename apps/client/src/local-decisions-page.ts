// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientError } from './control-plane-client.js'
import {
  mountButton,
  mountEmptyState,
  mountErrorState,
  mountPageHeader,
  mountPanel,
  mountStatusBadge,
  type StatusTone,
} from './components/index.js'
import type {
  ChatInputInteractionProjection,
  ChatInteractionOptionProjection,
  DeliveryAttentionOptionProjection,
  InteractiveInputValue,
} from './generated/contracts.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import {
  approvalRiskDetail,
  boundApprovalText,
} from './approval-risk-detail.js'
import {
  mountApprovalRiskDetail,
  type ApprovalRiskDetailView,
} from './approval-risk-detail-view.js'
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
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
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

function updateDecisionContext(list: HTMLUListElement, entries: readonly string[]): void {
  entries.forEach((entry, index) => {
    const item = list.children[index]
    if (item !== undefined && item.textContent !== entry) item.textContent = entry
  })
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
  const layout = element(document, 'section', 'wwc-local-decisions')
  layout.dataset.wwcPage = 'management'
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: 'Local decisions',
      eyebrow: 'Approvals and attention',
      description: 'Review pending inputs, tool approvals, and business Attention with current revision bindings.',
      headingLevel: 2,
      className: 'wwc-local-decisions-heading',
    },
  })
  const heading = pageHeader.root
  const help = element(document, 'p', 'wwc-local-decisions-help')
  const statusBadge = mountStatusBadge({
    document,
    props: {
      label: 'Loading local decisions…',
      tone: 'info',
      live: 'polite',
      className: 'wwc-local-decisions-status',
    },
  })
  const status = statusBadge.root
  const retryButton = mountButton({
    document,
    props: {
      label: 'Retry snapshot',
      className: 'wwc-local-decisions-retry',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const retry = retryButton.root
  const reconnectButton = mountButton({
    document,
    props: {
      label: 'Reconnect events',
      className: 'wwc-local-decisions-reconnect',
      onActivate: () => { options.model.reconnect() },
    },
  })
  const reconnect = reconnectButton.root
  const errorState = mountErrorState({
    document,
    props: {
      title: 'Local decisions unavailable',
      message: '',
      actions: [retry, reconnect],
      visible: false,
      className: 'wwc-local-decisions-error',
    },
  })
  const error = errorState.root
  errorState.message.className = 'wwc-local-decisions-error-text'
  const inputsPanel = mountPanel({
    document,
    props: {
      id: 'wwc-local-inputs',
      headingLevel: 3,
      title: 'Pending inputs',
      description: 'Responses remain bound to the current ProductSession and execution identity.',
      className: 'wwc-local-inputs',
    },
  })
  const inputsSection = inputsPanel.root
  const inputsHeading = inputsPanel.title
  inputsHeading.className = 'wwc-local-decisions-section-heading'
  const inputs = element(document, 'ul', 'wwc-local-input-list')
  const inputsEmpty = mountEmptyState({
    document,
    props: {
      title: 'No pending inputs',
      detail: 'New input requests will appear here when a session needs a response.',
      className: 'wwc-local-input-empty',
      headingLevel: 3,
    },
  })
  const approvalsPanel = mountPanel({
    document,
    props: {
      id: 'wwc-local-approvals',
      headingLevel: 3,
      title: 'Tool approvals',
      description: 'Each decision is submitted with the current session identity and revision.',
      className: 'wwc-local-approvals',
    },
  })
  const approvalsSection = approvalsPanel.root
  const approvalsHeading = approvalsPanel.title
  approvalsHeading.className = 'wwc-local-decisions-section-heading'
  const approvals = element(document, 'ul', 'wwc-local-approval-list')
  const approvalsEmpty = mountEmptyState({
    document,
    props: {
      title: 'No pending tool approvals',
      detail: 'Tool requests that require a decision will appear here.',
      className: 'wwc-local-approval-empty',
      headingLevel: 3,
    },
  })
  const attentionPanel = mountPanel({
    document,
    props: {
      id: 'wwc-local-attention',
      headingLevel: 3,
      title: 'Business Attention',
      description: 'Resolve Delivery questions and blockers against the current Delivery revision.',
      className: 'wwc-local-attention',
    },
  })
  const attentionSection = attentionPanel.root
  const attentionHeading = attentionPanel.title
  attentionHeading.className = 'wwc-local-decisions-section-heading'
  const attention = element(document, 'ul', 'wwc-local-attention-list')
  const attentionEmpty = mountEmptyState({
    document,
    props: {
      title: 'No open business Attention',
      detail: 'Delivery questions and blocking decisions will appear here.',
      className: 'wwc-local-attention-empty',
      headingLevel: 3,
    },
  })
  let closed = false

  help.textContent = 'Inputs include exact current choices such as Plan Delta or Replan. Submitted responses are cleared immediately.'
  inputsPanel.content.append(inputs, inputsEmpty.root)
  approvalsPanel.content.append(approvals, approvalsEmpty.root)
  attentionPanel.content.append(attention, attentionEmpty.root)
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

  interface InputRow {
    current: LocalInputDecision
    readonly title: HTMLElement
    readonly context: HTMLUListElement
    readonly textForm: HTMLFormElement
    readonly response: HTMLTextAreaElement
    readonly submit: HTMLButtonElement
    readonly choices: HTMLElement
    readonly optionCollection: KeyedCollectionView<
      ChatInteractionOptionProjection,
      string,
      HTMLButtonElement
    >
    readonly cancel: HTMLButtonElement
    readonly onSubmit: (event: SubmitEvent) => void
    readonly onCancel: () => void
  }
  const inputRows = new WeakMap<HTMLLIElement, InputRow>()
  const inputCollection = mountKeyedCollection({
    parent: inputs,
    key: (item: LocalInputDecision) => item.projection.inputRequestId,
    create(item: LocalInputDecision) {
      const projection = item.projection
      const row = element(document, 'li', 'wwc-local-input')
      const title = element(document, 'h3', 'wwc-local-input-prompt')
      const context = decisionContext(document, ['', '', '', '', '', ''])
      const controls = element(document, 'div', 'wwc-local-input-controls')
      const textForm = element(document, 'form', 'wwc-local-input-form')
      const label = element(document, 'label', 'wwc-local-input-response-label')
      const response = element(document, 'textarea', 'wwc-local-input-response')
      const submit = element(document, 'button', 'wwc-local-input-submit')
      const choices = element(document, 'div', 'wwc-local-input-options')
      const cancel = element(document, 'button', 'wwc-local-input-cancel')
      const inputId = `wwc-local-input-response-${projection.inputRequestId}`
      label.htmlFor = inputId
      label.textContent = 'Response'
      response.id = inputId
      response.autocomplete = 'off'
      response.spellcheck = false
      submit.type = 'submit'
      submit.textContent = 'Submit response'
      submit.dataset.wwcComponent = 'button'
      submit.dataset.variant = 'primary'
      cancel.type = 'button'
      cancel.textContent = 'Cancel input'
      cancel.dataset.wwcComponent = 'button'
      cancel.dataset.variant = 'destructive'
      const optionStates = new WeakMap<HTMLButtonElement, ChatInteractionOptionProjection>()
      const optionListeners = new WeakMap<HTMLButtonElement, () => void>()
      const optionCollection = mountKeyedCollection<
        ChatInteractionOptionProjection,
        string,
        HTMLButtonElement
      >({
        parent: choices,
        key: option => option.id,
        create() {
          const choice = element(document, 'button', 'wwc-local-input-option')
          choice.type = 'button'
          choice.dataset.wwcComponent = 'button'
          choice.dataset.variant = 'default'
          const onClick = () => {
            if (options.readOnly === true) return
            const currentRow = inputRows.get(row)
            const currentOption = optionStates.get(choice)
            if (currentRow === undefined || currentOption === undefined) return
            const value: InteractiveInputValue = {
              mode: currentRow.current.projection.mode,
              value: currentOption.value,
            }
            void options.model.provideInput(
              currentRow.current.projection.inputRequestId,
              value,
            )
          }
          choice.addEventListener('click', onClick)
          optionListeners.set(choice, onClick)
          return choice
        },
        update(choice, option) {
          optionStates.set(choice, option)
          choice.textContent = option.label
        },
        remove(choice) {
          const onClick = optionListeners.get(choice)
          if (onClick !== undefined) choice.removeEventListener('click', onClick)
          optionListeners.delete(choice)
          optionStates.delete(choice)
        },
      })
      const onSubmit = (event: SubmitEvent) => {
        event.preventDefault()
        if (options.readOnly === true) return
        const current = inputRows.get(row)
        if (current === undefined || current.current.projection.mode !== 'text') return
        const value: InteractiveInputValue = {
          mode: current.current.projection.mode,
          value: current.response.value,
        }
        current.response.value = ''
        void options.model.provideInput(
          current.current.projection.inputRequestId,
          value,
        )
      }
      const onCancel = () => {
        if (options.readOnly === true) return
        const current = inputRows.get(row)
        if (current !== undefined) {
          void options.model.cancelInput(current.current.projection.inputRequestId)
        }
      }
      textForm.addEventListener('submit', onSubmit)
      cancel.addEventListener('click', onCancel)
      label.append(response)
      textForm.append(label, submit)
      controls.append(textForm, choices, cancel)
      row.append(title, context, controls)
      inputRows.set(row, {
        current: item,
        title,
        context,
        textForm,
        response,
        submit,
        choices,
        optionCollection,
        cancel,
        onSubmit,
        onCancel,
      })
      return row
    },
    update(row, item: LocalInputDecision) {
      const mounted = inputRows.get(row)
      if (mounted === undefined) return
      const projection = item.projection
      const decisionDisabled = localDecisionsPagePresentation(
        options.model.state,
      ).decisionsDisabled || options.readOnly === true || item.expired
      mounted.current = item
      mounted.title.textContent = projection.prompt
      row.dataset.state = item.expired ? 'expired' : 'pending'
      updateDecisionContext(mounted.context, [
        inputModeLabel(projection),
        inputStateLabel(item),
        projection.binding.sessionIdentity.stageRunId === undefined
          ? 'ProductSession-bound'
          : 'ProductSession and StageRun-bound',
        'Execution job and Worker session-bound',
        `Revision ${String(projection.revision)}`,
        `Expires ${projection.expiresAt}`,
      ])
      mounted.textForm.hidden = projection.mode !== 'text'
      mounted.choices.hidden = projection.mode === 'text'
      mounted.response.disabled = decisionDisabled
      mounted.submit.disabled = decisionDisabled
      mounted.cancel.disabled = decisionDisabled
      mounted.optionCollection.update(projection.mode === 'text' ? [] : projection.options)
      for (const choice of mounted.choices.children) {
        const button = choice as HTMLButtonElement
        button.disabled = decisionDisabled
      }
    },
    remove(row) {
      const mounted = inputRows.get(row)
      if (mounted === undefined) return
      mounted.response.value = ''
      mounted.textForm.removeEventListener('submit', mounted.onSubmit)
      mounted.cancel.removeEventListener('click', mounted.onCancel)
      mounted.optionCollection.close()
      inputRows.delete(row)
    },
  })

  interface ApprovalRow {
    current: LocalApprovalDecision
    readonly title: HTMLElement
    readonly risk: ApprovalRiskDetailView
    readonly context: HTMLUListElement
    readonly reason: HTMLTextAreaElement
    readonly approve: HTMLButtonElement
    readonly reject: HTMLButtonElement
    readonly onApprove: () => void
    readonly onReject: () => void
  }
  const approvalRows = new WeakMap<HTMLLIElement, ApprovalRow>()
  const approvalCollection = mountKeyedCollection({
    parent: approvals,
    key: (item: LocalApprovalDecision) => item.projection.id,
    create(item: LocalApprovalDecision) {
      const projection = item.projection
      const row = element(document, 'li', 'wwc-local-approval')
      const title = element(document, 'h3', 'wwc-local-approval-subject')
      // The risk block sits before the decision form so the scope, expiry, and
      // impact are already on screen when approve or reject is reached.
      const risk = mountApprovalRiskDetail({ document })
      const context = decisionContext(document, ['', '', '', '', ''])
      const form = element(document, 'form', 'wwc-local-approval-form')
      const label = element(document, 'label', 'wwc-local-approval-reason-label')
      const reason = element(document, 'textarea', 'wwc-local-approval-reason')
      const controls = element(document, 'div', 'wwc-local-approval-controls')
      const approve = element(document, 'button', 'wwc-local-approval-approve')
      const reject = element(document, 'button', 'wwc-local-approval-reject')
      const inputId = `wwc-local-approval-reason-${projection.id}`
      label.htmlFor = inputId
      label.textContent = 'Decision reason'
      reason.id = inputId
      reason.autocomplete = 'off'
      reason.spellcheck = false
      approve.type = 'button'
      approve.textContent = 'Approve'
      approve.dataset.wwcComponent = 'button'
      approve.dataset.variant = 'primary'
      reject.type = 'button'
      reject.textContent = 'Reject'
      reject.dataset.wwcComponent = 'button'
      reject.dataset.variant = 'destructive'
      const decide = (decision: 'approve' | 'reject') => {
        if (options.readOnly === true) return
        const current = approvalRows.get(row)
        if (current === undefined) return
        const explanation = current.reason.value
        current.reason.value = ''
        void options.model.decideApproval(
          current.current.projection.id,
          decision,
          explanation,
        )
      }
      const onApprove = () => { decide('approve') }
      const onReject = () => { decide('reject') }
      approve.addEventListener('click', onApprove)
      reject.addEventListener('click', onReject)
      label.append(reason)
      controls.append(approve, reject)
      form.append(label, controls)
      row.append(title, risk.root, context, form)
      approvalRows.set(row, {
        current: item,
        title,
        risk,
        context,
        reason,
        approve,
        reject,
        onApprove,
        onReject,
      })
      return row
    },
    update(row, item: LocalApprovalDecision) {
      const mounted = approvalRows.get(row)
      if (mounted === undefined) return
      const projection = item.projection
      const decisionDisabled = localDecisionsPagePresentation(
        options.model.state,
      ).decisionsDisabled || options.readOnly === true || item.expired
      mounted.current = item
      mounted.title.textContent = boundApprovalText(projection.subject).text
      row.dataset.state = item.expired ? 'expired' : 'pending'
      mounted.risk.update(approvalRiskDetail(projection, {
        expired: item.expired,
        nowMillis: Date.now,
      }))
      updateDecisionContext(mounted.context, [
        approvalStateLabel(item),
        projection.binding.sessionIdentity.stageRunId === undefined
          ? 'ProductSession-bound'
          : 'ProductSession and StageRun-bound',
        'Execution job and Worker session-bound',
        `Revision ${String(projection.revision)}`,
        `Expires ${projection.expiresAt}`,
      ])
      mounted.reason.disabled = decisionDisabled
      mounted.approve.disabled = decisionDisabled
      mounted.reject.disabled = decisionDisabled
    },
    remove(row) {
      const mounted = approvalRows.get(row)
      if (mounted === undefined) return
      mounted.reason.value = ''
      mounted.approve.removeEventListener('click', mounted.onApprove)
      mounted.reject.removeEventListener('click', mounted.onReject)
      mounted.risk.close()
      approvalRows.delete(row)
    },
  })

  interface AttentionRow {
    current: LocalAttentionDecision
    readonly title: HTMLElement
    readonly context: HTMLUListElement
    readonly optionCollection: KeyedCollectionView<
      DeliveryAttentionOptionProjection,
      string,
      HTMLLIElement
    >
    readonly resolution: HTMLTextAreaElement
    readonly resolve: HTMLButtonElement
    readonly dismiss: HTMLButtonElement
    readonly onResolve: () => void
    readonly onDismiss: () => void
  }
  const attentionRows = new WeakMap<HTMLLIElement, AttentionRow>()
  const attentionCollection = mountKeyedCollection({
    parent: attention,
    key: (item: LocalAttentionDecision) => item.projection.id,
    create(item: LocalAttentionDecision) {
      const projection = item.projection
      const row = element(document, 'li', 'wwc-local-attention-item')
      const title = element(document, 'h3', 'wwc-local-attention-title')
      const context = decisionContext(document, ['', '', '', '', ''])
      const choices = element(document, 'ul', 'wwc-local-attention-options')
      const form = element(document, 'form', 'wwc-local-attention-form')
      const label = element(document, 'label', 'wwc-local-attention-resolution-label')
      const resolution = element(document, 'textarea', 'wwc-local-attention-resolution')
      const controls = element(document, 'div', 'wwc-local-attention-controls')
      const resolve = element(document, 'button', 'wwc-local-attention-resolve')
      const dismiss = element(document, 'button', 'wwc-local-attention-dismiss')
      const inputId = `wwc-local-attention-resolution-${projection.id}`
      label.htmlFor = inputId
      label.textContent = 'Resolution'
      resolution.id = inputId
      resolution.autocomplete = 'off'
      resolution.spellcheck = false
      resolve.type = 'button'
      resolve.textContent = 'Resolve'
      resolve.dataset.wwcComponent = 'button'
      resolve.dataset.variant = 'primary'
      dismiss.type = 'button'
      dismiss.textContent = 'Dismiss'
      dismiss.dataset.wwcComponent = 'button'
      dismiss.dataset.variant = 'destructive'
      const optionRows = new WeakMap<HTMLLIElement, {
        readonly title: HTMLElement
        readonly description: HTMLElement
      }>()
      const optionCollection = mountKeyedCollection<
        DeliveryAttentionOptionProjection,
        string,
        HTMLLIElement
      >({
        parent: choices,
        key: option => option.id,
        create() {
          const optionItem = document.createElement('li')
          const optionTitle = element(document, 'strong', 'wwc-local-attention-option-label')
          const optionDescription = element(document, 'span', 'wwc-local-attention-option-description')
          optionItem.append(optionTitle, optionDescription)
          optionRows.set(optionItem, { title: optionTitle, description: optionDescription })
          return optionItem
        },
        update(optionItem, option) {
          const mountedOption = optionRows.get(optionItem)
          if (mountedOption === undefined) return
          mountedOption.title.textContent = option.label
          mountedOption.description.textContent = option.description
        },
        remove(optionItem) { optionRows.delete(optionItem) },
      })
      const decide = (decision: 'resolve' | 'dismiss') => {
        if (options.readOnly === true) return
        const current = attentionRows.get(row)
        if (current === undefined) return
        const explanation = current.resolution.value
        current.resolution.value = ''
        void options.model.resolveAttention(
          current.current.projection.id,
          decision,
          explanation,
        )
      }
      const onResolve = () => { decide('resolve') }
      const onDismiss = () => { decide('dismiss') }
      resolve.addEventListener('click', onResolve)
      dismiss.addEventListener('click', onDismiss)
      label.append(resolution)
      controls.append(resolve, dismiss)
      form.append(label, controls)
      row.append(title, context, choices, form)
      attentionRows.set(row, {
        current: item,
        title,
        context,
        optionCollection,
        resolution,
        resolve,
        dismiss,
        onResolve,
        onDismiss,
      })
      return row
    },
    update(row, item: LocalAttentionDecision) {
      const mounted = attentionRows.get(row)
      if (mounted === undefined) return
      const projection = item.projection
      const disabled = options.readOnly === true
        || localDecisionsPagePresentation(options.model.state).decisionsDisabled
      mounted.current = item
      mounted.title.textContent = projection.title
      row.dataset.state = projection.blocking ? 'blocking' : 'open'
      updateDecisionContext(mounted.context, [
        attentionTypeLabel(item),
        projection.blocking ? 'Blocking' : 'Non-blocking',
        projection.stageRunId === null ? 'Delivery-bound' : 'Delivery and StageRun-bound',
        item.candidateDigest === null ? 'No candidate is currently bound' : 'Current candidate-bound',
        `Delivery revision ${String(item.deliveryRevision)}`,
      ])
      mounted.optionCollection.update(projection.options)
      mounted.resolution.disabled = disabled
      mounted.resolve.disabled = disabled
      mounted.dismiss.disabled = disabled
    },
    remove(row) {
      const mounted = attentionRows.get(row)
      if (mounted === undefined) return
      mounted.resolution.value = ''
      mounted.resolve.removeEventListener('click', mounted.onResolve)
      mounted.dismiss.removeEventListener('click', mounted.onDismiss)
      mounted.optionCollection.close()
      attentionRows.delete(row)
    },
  })

  function render(state: LocalDecisionsViewModelState): void {
    if (closed) return
    const presentation = localDecisionsPagePresentation(state)
    const tone: StatusTone = presentation.errorText !== null
      ? 'danger'
      : state.realtime === 'reconnecting'
        ? 'warning'
        : presentation.busy
          ? 'info'
          : state.status === 'ready'
            ? 'success'
            : 'neutral'
    statusBadge.update({
      label: presentation.statusText,
      tone,
      live: 'polite',
      className: 'wwc-local-decisions-status',
    })
    layout.setAttribute('aria-busy', String(presentation.busy))
    errorState.update({
      title: 'Local decisions unavailable',
      message: presentation.errorText ?? '',
      actions: [retry, reconnect],
      visible: presentation.errorText !== null,
      className: 'wwc-local-decisions-error',
    })
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    inputCollection.update(state.inputs)
    approvalCollection.update(state.approvals)
    attentionCollection.update(state.attention)
    inputs.hidden = state.inputs.length === 0
    approvals.hidden = state.approvals.length === 0
    attention.hidden = state.attention.length === 0
    inputsEmpty.root.hidden = state.inputs.length !== 0
    approvalsEmpty.root.hidden = state.approvals.length !== 0
    attentionEmpty.root.hidden = state.attention.length !== 0
  }

  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      inputCollection.close()
      approvalCollection.close()
      attentionCollection.close()
      options.model.close()
      retryButton.close()
      reconnectButton.close()
      errorState.close()
      attentionEmpty.close()
      attentionPanel.close()
      approvalsEmpty.close()
      approvalsPanel.close()
      inputsEmpty.close()
      inputsPanel.close()
      statusBadge.close()
      pageHeader.close()
      options.root.replaceChildren()
    },
  }
}
