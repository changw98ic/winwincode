// SPDX-License-Identifier: Apache-2.0

import type {
  ChatViewModel,
  ChatViewModelState,
} from './chat-view-model.js'
import type { ControlPlaneClientError } from './control-plane-client.js'
import { mountButton } from './components/button.js'
import { mountFormField } from './components/form-field.js'
import { mountKeyedCollection } from './components/keyed-collection.js'
import type {
  ModelRouteAvailabilityProjection,
  ProductSessionId,
  RepositoryScope,
  Scope,
} from './generated/contracts.js'
import {
  ModelRouteAvailabilityReason,
  ModelRouteAvailabilityStatus,
} from './generated/contracts.js'
import {
  contextualDecisionPresentation,
  contextualDecisions,
} from './contextual-decision-view-model.js'
import {
  mountContextualDecisionCard,
  type ContextualDecisionCard,
} from './contextual-decision.js'

export interface ChatDeliveryCreateInput {
  readonly title: string
  readonly goal: string
  readonly baseRevision: string
  readonly scope: readonly string[]
  readonly outOfScope: readonly string[]
  readonly constraints: readonly string[]
  readonly sourceProductSessionId: ProductSessionId | null
  readonly acceptanceCriteria: readonly string[]
}

export interface ChatDeliveryCreatorState {
  readonly status: 'idle' | 'submitting' | 'waiting' | 'created' | 'error' | 'closed'
  readonly error: ControlPlaneClientError | null
}

/** Structural composition seam; Chat does not import the StrongFlow feature model. */
export interface ChatDeliveryCreator {
  readonly state: ChatDeliveryCreatorState
  subscribe(listener: (state: ChatDeliveryCreatorState) => void): () => void
  create(input: ChatDeliveryCreateInput): Promise<void>
  cancelPending(): void
  close(): void
}

export interface ChatPageOptions {
  readonly root: HTMLElement
  readonly model: ChatViewModel
  readonly nextProductSessionId?: () => ProductSessionId
  readonly deliveryCreator?: ChatDeliveryCreator
  readonly scope?: RepositoryScope
  readonly settingsHref?: string
  /** Deterministic clock for the contextual decision card; defaults to Date.now. */
  readonly nowMillis?: () => number
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
}

export interface ChatPage {
  close(): void
}

export interface ChatComposerKey {
  readonly key: string
  readonly shiftKey: boolean
  readonly isComposing: boolean
}

export type ChatComposerKeyAction = 'submit' | 'newline' | 'ignore'

export interface ChatPagePresentation {
  readonly statusText: string
  readonly emptyText: string
  readonly errorText: string | null
  readonly composerLabel: string
  readonly sendLabel: string
  readonly messageListBusy: boolean
  readonly composerDisabled: boolean
  readonly cancelVisible: boolean
}

export function chatComposerKeyAction(key: ChatComposerKey): ChatComposerKeyAction {
  if (key.key !== 'Enter') return 'ignore'
  if (key.isComposing || key.shiftKey) return 'newline'
  return 'submit'
}

function stateLabel(state: ChatViewModelState): string {
  if (state.status === 'loading') return 'Loading Chat…'
  if (state.status === 'refreshing') return 'Updating Chat…'
  if (state.status === 'authentication-required') return 'Sign in required'
  if (state.status === 'authorization-denied') return 'Access denied'
  if (state.status === 'cancelled') return 'Update cancelled'
  if (state.status === 'error') return 'Chat unavailable'
  if (state.status === 'closed') return 'Chat closed'
  if (state.realtime === 'reconnecting') return 'Reconnecting…'
  if (state.interaction.status === 'submitting') return 'Sending message…'
  if (state.interaction.status === 'cancelling') return 'Stopping run…'
  if (state.interaction.status === 'waiting') return 'Waiting for the server…'
  if (
    state.session === null
    && !readyModelRoutes(state).length
  ) return state.modelRouteAvailability?.reason === ModelRouteAvailabilityReason.NoProvider
    ? 'Model setup required'
    : 'Model route unavailable'
  if (state.session === null) return state.selectedModelRoute === null
    ? 'Choose a model route'
    : 'Ready for a new Chat'
  const sessionState = state.session?.state
  if (sessionState === 'running') return 'Running'
  if (sessionState === 'waiting_for_input') return 'Waiting for input'
  if (sessionState === 'waiting_for_approval') return 'Waiting for approval'
  if (sessionState === 'cancelled') return 'Cancelled'
  if (sessionState === 'closed') return 'Completed'
  if (sessionState === 'failed') return 'Failed'
  if (sessionState === 'idle') return 'Ready'
  return 'Select a session'
}

function modelRouteReady(candidate: ModelRouteAvailabilityProjection): boolean {
  return candidate.status === ModelRouteAvailabilityStatus.Enabled
    && candidate.reason === ModelRouteAvailabilityReason.Ready
}

function readyModelRoutes(
  state: ChatViewModelState,
): readonly ModelRouteAvailabilityProjection[] {
  return state.modelRouteAvailability?.items.filter(modelRouteReady) ?? []
}

function modelRouteReasonLabel(reason: ModelRouteAvailabilityReason): string {
  if (reason === ModelRouteAvailabilityReason.Ready) return 'Ready'
  if (reason === ModelRouteAvailabilityReason.NoProvider) return 'No Provider'
  if (reason === ModelRouteAvailabilityReason.CredentialMissingOrRevoked) {
    return 'Credential missing or revoked'
  }
  if (reason === ModelRouteAvailabilityReason.DefaultRouteInvalid) {
    return 'Default route invalid'
  }
  if (reason === ModelRouteAvailabilityReason.ProviderOrModelDisabled) {
    return 'Provider or model disabled'
  }
  return 'Request pool unavailable'
}

function modelRouteSourceLabel(scope: Scope): string {
  if (scope.kind === 'organization') return 'Organization scope'
  if (scope.kind === 'workspace') return 'Workspace scope'
  if (scope.kind === 'project') return 'Project scope'
  return 'Repository scope'
}

function modelRouteIdentity(route: ModelRouteAvailabilityProjection['route']): string {
  return `${route.providerId}\u0000${route.modelId}\u0000${route.credentialReferenceId}`
}

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
  if (error.code === 'IDEMPOTENCY_CONFLICT') {
    return 'This New Chat request conflicts with an earlier request. Start a fresh New Chat.'
  }
  if (error.code === 'INVALID_REQUEST') {
    return 'The selected model is not available for this repository. Choose another model and retry.'
  }
  if (error.code === 'WRONG_STATE') {
    return 'This Chat identity is already in use. Start a fresh New Chat.'
  }
  if (error.code === 'SERVICE_UNAVAILABLE') {
    return 'The model request pool or selected model is temporarily unavailable. Retry in a moment.'
  }
  if (error.code === 'TRUSTED_FACTS_UNAVAILABLE') {
    return 'The configured Provider or model is unavailable. Review Settings before retrying.'
  }
  if (error.kind === 'authentication') return 'Sign in again to continue this Chat.'
  if (error.kind === 'authorization') return 'You do not have access to this Chat.'
  if (error.kind === 'network') return 'The Chat server could not be reached. Check the connection and retry.'
  if (error.kind === 'version') return 'The Client and Server versions differ. Update the Client and retry.'
  if (error.kind === 'cancelled') return 'The Chat update was cancelled.'
  if (error.kind === 'configuration') return 'Chat needs a valid server configuration.'
  return 'Chat could not be updated. Retry, or review the server status.'
}

function modelRouteEmptyText(state: ChatViewModelState): string {
  const reason = state.modelRouteAvailability?.reason
  if (reason === ModelRouteAvailabilityReason.CredentialMissingOrRevoked) {
    return 'The configured model credential is missing or revoked. Review Settings.'
  }
  if (reason === ModelRouteAvailabilityReason.DefaultRouteInvalid) {
    return 'The default model route is invalid. Review Settings.'
  }
  if (reason === ModelRouteAvailabilityReason.ProviderOrModelDisabled) {
    return 'The configured Provider or model is disabled. Review Settings.'
  }
  if (reason === ModelRouteAvailabilityReason.RequestPoolUnavailable) {
    return 'The selected model request pool is unavailable. Retry or review Settings.'
  }
  return 'No model route is configured because no Provider is available. Open Settings before creating a Chat.'
}

export function chatPagePresentation(state: ChatViewModelState): ChatPagePresentation {
  const error = state.interaction.error ?? state.messagePagination.error ?? state.error
  const running = state.session?.state === 'running'
  const continuing = state.session?.state === 'waiting_for_input'
  const mutationBusy = ['submitting', 'cancelling'].includes(state.interaction.status)
  return Object.freeze({
    statusText: stateLabel(state),
    emptyText: state.session === null
      ? readyModelRoutes(state).length > 0
        ? 'Create your first Chat to start a conversation.'
        : modelRouteEmptyText(state)
      : 'No messages yet. Send a message to begin.',
    errorText: errorLabel(error),
    composerLabel: running
      ? 'Steer the current run'
      : continuing
        ? 'Continue the conversation'
        : 'Message WinWinCode',
    sendLabel: running ? 'Steer' : continuing ? 'Continue' : 'Send',
    messageListBusy: state.status === 'loading'
      || state.status === 'refreshing'
      || state.realtime === 'reloading',
    composerDisabled: state.session === null
      || mutationBusy
      || state.status === 'authentication-required'
      || state.status === 'authorization-denied'
      || state.status === 'closed',
    cancelVisible: running,
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

function messageStateText(state: string): string | null {
  if (state === 'streaming') return 'Streaming'
  if (state === 'cancelled') return 'Cancelled'
  if (state === 'failed') return 'Failed'
  return null
}

function confirmedRequirement(state: ChatViewModelState): string | null {
  return [...state.messages].reverse().find(message => (
    message.role === 'user'
    && message.state === 'completed'
    && message.content.trim().length > 0
  ))?.content.trim() ?? null
}

function deliveryConversionError(state: ChatDeliveryCreatorState): string | null {
  const error = state.error
  if (error === null) return null
  if (error.code.startsWith('STRONGFLOW_CREATE_')) return error.message
  if (error.kind === 'authentication') return 'Sign in again before creating this Delivery.'
  if (error.kind === 'authorization') {
    return 'You do not have permission to create a Delivery in this repository.'
  }
  if (error.kind === 'network') {
    return 'The StrongFlow server could not be reached. The confirmed Chat draft is still here.'
  }
  if (error.kind === 'cancelled') {
    return 'Delivery creation was cancelled. The confirmed Chat draft is still here.'
  }
  if (error.code === 'REVISION_CONFLICT') {
    return 'The Delivery changed before StrongFlow could start. Retry the same confirmed draft.'
  }
  return 'The Delivery could not be created. The confirmed Chat draft is still here; retry it.'
}

const CONVERSION_DIALOG_HEADING_ID = 'wwc-chat-convert-heading'

function focusableElement(value: Element | null | undefined): HTMLElement | null {
  if (value === null || typeof value !== 'object') return null
  return typeof Reflect.get(value, 'focus') === 'function'
    ? value as HTMLElement
    : null
}

/** Mount the default, keyboard-accessible Chat page against the read/write view-model only. */
export function mountChatPage(options: ChatPageOptions): ChatPage {
  const readOnly = options.readOnly === true
  const nowMillis = options.nowMillis ?? Date.now
  const document = options.root.ownerDocument
  const layout = element(document, 'div', 'wwc-chat')
  const sessionPanel = element(document, 'aside', 'wwc-chat-sessions')
  const sessionHeading = element(document, 'h2', 'wwc-chat-sessions-heading')
  const newSession = element(document, 'button', 'wwc-chat-new-session')
  const sessionList = element(document, 'ul', 'wwc-chat-session-list')
  const conversation = element(document, 'section', 'wwc-chat-conversation')
  const header = element(document, 'header', 'wwc-chat-conversation-header')
  const heading = element(document, 'h2', 'wwc-chat-heading')
  const status = element(document, 'p', 'wwc-chat-status')
  const modelLabel = element(document, 'label', 'wwc-chat-model-label')
  const modelSelect = element(document, 'select', 'wwc-chat-model')
  const modelSettings = element(document, 'a', 'wwc-chat-model-settings')
  const modelNotice = element(document, 'p', 'wwc-chat-model-notice')
  const convertDelivery = mountButton({
    document,
    props: {
      className: 'wwc-chat-convert-delivery',
      label: 'Convert to StrongFlow',
      type: 'button',
      variant: 'primary',
    },
  })
  const error = element(document, 'div', 'wwc-chat-error')
  const errorText = element(document, 'span', 'wwc-chat-error-text')
  const retry = element(document, 'button', 'wwc-chat-retry')
  const messages = element(document, 'ol', 'wwc-chat-messages')
  const empty = element(document, 'p', 'wwc-chat-empty')
  const loadEarlier = element(document, 'button', 'wwc-chat-load-earlier')
  const form = element(document, 'form', 'wwc-chat-composer')
  const composerLabel = element(document, 'label', 'wwc-chat-composer-label')
  const composer = element(document, 'textarea', 'wwc-chat-composer-input')
  const controls = element(document, 'div', 'wwc-chat-composer-controls')
  const cancel = element(document, 'button', 'wwc-chat-cancel')
  const send = element(document, 'button', 'wwc-chat-send')
  const conversion = element(document, 'section', 'wwc-chat-convert')
  const conversionHeading = element(document, 'h3', 'wwc-chat-convert-heading')
  const conversionDetail = element(document, 'p', 'wwc-chat-convert-detail')
  const conversionForm = element(document, 'form', 'wwc-chat-convert-form')
  const conversionTitle = element(document, 'input', 'wwc-chat-convert-title')
  const conversionGoal = element(document, 'textarea', 'wwc-chat-convert-goal')
  const conversionSourceSession = element(
    document,
    'input',
    'wwc-chat-convert-source-session',
  )
  const conversionScope = element(document, 'input', 'wwc-chat-convert-scope')
  const conversionModel = element(document, 'input', 'wwc-chat-convert-model')
  const conversionBaseline = element(document, 'input', 'wwc-chat-convert-baseline')
  const conversionDeliveryScope = element(document, 'textarea', 'wwc-chat-convert-delivery-scope')
  const conversionOutOfScope = element(document, 'textarea', 'wwc-chat-convert-out-of-scope')
  const conversionConstraints = element(document, 'textarea', 'wwc-chat-convert-constraints')
  const conversionCriteria = element(document, 'textarea', 'wwc-chat-convert-criteria')
  const confirmationLabel = element(document, 'label', 'wwc-chat-convert-confirm-label')
  const confirmation = element(document, 'input', 'wwc-chat-convert-confirm')
  const confirmationText = element(document, 'span', 'wwc-chat-convert-confirm-text')
  const conversionError = element(document, 'p', 'wwc-chat-convert-error')
  const conversionSubmit = mountButton({
    document,
    props: {
      className: 'wwc-chat-convert-submit',
      label: 'Confirm and create Delivery',
      type: 'submit',
      variant: 'primary',
    },
  })
  const conversionCancel = mountButton({
    document,
    props: {
      className: 'wwc-chat-convert-cancel',
      label: 'Cancel conversion',
      type: 'button',
    },
  })
  let closed = false
  let conversionOpen = false
  let conversionSessionId: ProductSessionId | null = null
  let conversionFocusReturn: HTMLElement | null = null

  // UI-502: the Session's own pending inputs and approvals, decided in place
  // through this page's view-model commands instead of a detour to the global
  // Attention Center.  The card is a projection of this page's snapshot, so it
  // cannot drift from the state the rest of the page renders.
  // The card mounts into this detached root, so a hidden card adds no node to
  // the conversation and the page layout stays byte-identical when idle.
  const decisionCard: ContextualDecisionCard = mountContextualDecisionCard({
    root: document.createElement('div'),
    id: 'wwc-chat-decisions',
    title: 'Decisions in this Chat',
    description: 'Answer this Session input or approve this tool call without leaving Chat.',
    readOnly,
    actions: {
      provideInput(item, value) {
        if (readOnly) return
        void options.model.respondToInput(item.id, 'provided', value)
      },
      cancelInput(item) {
        if (readOnly) return
        void options.model.respondToInput(item.id, 'cancelled', null)
      },
      decideApproval(item, decision, reason) {
        if (readOnly) return
        void options.model.decideApproval(item.id, decision, reason)
      },
    },
  })

  sessionHeading.textContent = 'Sessions'
  newSession.type = 'button'
  newSession.textContent = 'New Chat'
  heading.textContent = 'Chat'
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  modelLabel.textContent = 'Model route'
  modelLabel.htmlFor = 'wwc-chat-model'
  modelSelect.id = 'wwc-chat-model'
  modelSettings.href = options.settingsHref ?? '#/settings'
  modelSettings.textContent = 'Review model routes in Settings'
  modelNotice.setAttribute('role', 'status')
  modelNotice.setAttribute('aria-live', 'polite')
  modelNotice.hidden = true
  error.setAttribute('role', 'alert')
  error.setAttribute('aria-live', 'assertive')
  error.hidden = true
  retry.type = 'button'
  retry.textContent = 'Retry'
  messages.setAttribute('aria-label', 'Chat messages')
  messages.setAttribute('aria-live', 'polite')
  messages.setAttribute('aria-relevant', 'additions text')
  loadEarlier.type = 'button'
  loadEarlier.textContent = 'Load earlier messages'
  composerLabel.htmlFor = 'wwc-chat-composer'
  composer.id = 'wwc-chat-composer'
  composer.rows = 3
  composer.autocomplete = 'off'
  cancel.type = 'button'
  cancel.textContent = 'Stop'
  send.type = 'submit'

  modelLabel.append(modelSelect)
  error.append(errorText, retry)
  controls.append(cancel, send)
  form.append(composerLabel, composer, controls)
  sessionPanel.append(sessionHeading, newSession, sessionList)
  conversion.hidden = true
  // UI-604: the panel is a dialog in fact but was announced as plain page content,
  // opened without moving focus, and could only be dismissed with the pointer.
  conversion.setAttribute('role', 'dialog')
  conversion.setAttribute('aria-modal', 'false')
  conversionHeading.id = CONVERSION_DIALOG_HEADING_ID
  conversion.setAttribute('aria-labelledby', CONVERSION_DIALOG_HEADING_ID)
  convertDelivery.root.setAttribute('aria-controls', CONVERSION_DIALOG_HEADING_ID)
  convertDelivery.root.setAttribute('aria-expanded', 'false')
  conversionHeading.textContent = 'Confirm StrongFlow Delivery'
  conversionDetail.textContent = 'Review the confirmed requirement and exact repository context before creating a Delivery.'
  conversionTitle.type = 'text'
  conversionTitle.required = true
  conversionGoal.required = true
  conversionSourceSession.type = 'text'
  conversionSourceSession.readOnly = true
  conversionScope.type = 'text'
  conversionScope.readOnly = true
  conversionModel.type = 'text'
  conversionModel.readOnly = true
  conversionBaseline.type = 'text'
  conversionBaseline.required = true
  conversionDeliveryScope.required = true
  conversionCriteria.required = true
  confirmation.type = 'checkbox'
  confirmationText.textContent = 'I confirmed this target and Repository Scope.'
  confirmationLabel.append(confirmation, confirmationText)
  conversionError.setAttribute('role', 'alert')
  conversionError.setAttribute('aria-live', 'assertive')
  const conversionFields = [
    mountFormField({
      document,
      props: { id: 'chat-convert-title', label: 'Delivery title', control: conversionTitle, required: true },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-goal', label: 'Confirmed goal', control: conversionGoal, required: true },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-session', label: 'Source Chat', control: conversionSourceSession },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-scope', label: 'Repository Scope', control: conversionScope },
    }),
    mountFormField({
      document,
      props: { id: 'chat-convert-model', label: 'Model context', control: conversionModel },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-baseline',
        label: 'Baseline revision',
        control: conversionBaseline,
        required: true,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-delivery-scope',
        label: 'In scope',
        help: 'Enter one confirmed result per line.',
        control: conversionDeliveryScope,
        required: true,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-out-of-scope',
        label: 'Out of scope',
        help: 'Enter one explicit exclusion per line.',
        control: conversionOutOfScope,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-constraints',
        label: 'Constraints',
        help: 'Enter one confirmed constraint per line.',
        control: conversionConstraints,
      },
    }),
    mountFormField({
      document,
      props: {
        id: 'chat-convert-criteria',
        label: 'Initial acceptance criteria',
        help: 'Enter one required result per line.',
        control: conversionCriteria,
        required: true,
      },
    }),
  ]
  conversionForm.append(
    ...conversionFields.map(field => field.root),
    confirmationLabel,
    conversionError,
    conversionSubmit.root,
    conversionCancel.root,
  )
  conversion.append(conversionHeading, conversionDetail, conversionForm)
  header.append(
    heading,
    status,
    modelLabel,
    modelSettings,
    modelNotice,
    convertDelivery.root,
  )
  conversation.append(header, decisionCard.root, conversion, error, loadEarlier, messages, empty, form)
  layout.append(sessionPanel, conversation)
  options.root.replaceChildren(layout)

  type ModelOption =
    | { readonly key: 'empty' | 'placeholder'; readonly candidate: null }
    | { readonly key: string; readonly candidate: ModelRouteAvailabilityProjection }
  const modelOptions = mountKeyedCollection<ModelOption, string, HTMLOptionElement>({
    parent: modelSelect,
    key: item => item.key,
    create: () => document.createElement('option'),
    update(option, item) {
      option.value = item.key
      if (item.candidate === null) {
        option.textContent = item.key === 'empty'
          ? 'No model route configured'
          : 'Choose an available model route'
        option.disabled = false
        return
      }
      const candidate = item.candidate
      const source = modelRouteSourceLabel(candidate.catalogSource)
      const defaultLabel = candidate.isDefault ? ' · Default' : ''
      option.textContent = `${source} · ${candidate.providerDisplayName} / `
        + `${candidate.modelDisplayName}${defaultLabel} · `
        + modelRouteReasonLabel(candidate.reason)
      option.disabled = !modelRouteReady(candidate)
    },
  })
  const sessionRows = new WeakMap<HTMLLIElement, {
    readonly button: HTMLButtonElement
    readonly onClick: () => void
  }>()
  const sessionCollection = mountKeyedCollection({
    parent: sessionList,
    key: (session: ChatViewModelState['sessions'][number]) => session.id,
    create(session: ChatViewModelState['sessions'][number]) {
      const item = document.createElement('li')
      const button = document.createElement('button')
      const onClick = () => {
        const productSessionId = button.dataset.sessionId as ProductSessionId | undefined
        if (productSessionId !== undefined) void options.model.selectSession(productSessionId)
      }
      button.type = 'button'
      button.addEventListener('click', onClick)
      item.append(button)
      sessionRows.set(item, { button, onClick })
      return item
    },
    update(item, session: ChatViewModelState['sessions'][number]) {
      const row = sessionRows.get(item)
      if (row === undefined) return
      row.button.textContent = session.title
      row.button.dataset.sessionId = session.id
      if (session.id === options.model.state.activeProductSessionId) {
        row.button.setAttribute('aria-current', 'true')
      } else {
        row.button.removeAttribute('aria-current')
      }
    },
    remove(item) {
      const row = sessionRows.get(item)
      if (row === undefined) return
      row.button.removeEventListener('click', row.onClick)
      sessionRows.delete(item)
    },
  })
  const messageRows = new WeakMap<HTMLLIElement, {
    readonly article: HTMLElement
    readonly role: HTMLElement
    readonly content: HTMLElement
    readonly badge: HTMLElement
  }>()
  const messageCollection = mountKeyedCollection({
    parent: messages,
    key: (message: ChatViewModelState['messages'][number]) => message.id,
    create() {
      const item = document.createElement('li')
      const article = document.createElement('article')
      const role = document.createElement('h3')
      const content = document.createElement('p')
      const badge = document.createElement('span')
      badge.className = 'wwc-chat-message-state'
      article.append(role, content, badge)
      item.append(article)
      messageRows.set(item, { article, role, content, badge })
      return item
    },
    update(item, message: ChatViewModelState['messages'][number]) {
      const row = messageRows.get(item)
      if (row === undefined) return
      const stateText = messageStateText(message.state)
      row.article.dataset.role = message.role
      row.article.dataset.state = message.state
      row.article.setAttribute('aria-busy', String(message.state === 'streaming'))
      row.role.textContent = message.role === 'user' ? 'You' : 'WinWinCode'
      row.content.textContent = message.content.length === 0 && message.state === 'streaming'
        ? 'Responding…'
        : message.content
      row.badge.hidden = stateText === null
      row.badge.textContent = stateText ?? ''
    },
    remove(item) { messageRows.delete(item) },
  })

  function render(state: ChatViewModelState): void {
    if (closed) return
    const presentation = chatPagePresentation(state)
    if (conversionSessionId !== null && conversionSessionId !== state.session?.id) {
      conversionOpen = false
      conversionSessionId = null
      conversionTitle.value = ''
      conversionGoal.value = ''
      conversionBaseline.value = ''
      conversionDeliveryScope.value = ''
      conversionOutOfScope.value = ''
      conversionConstraints.value = ''
      conversionCriteria.value = ''
      confirmation.checked = false
    }
    status.textContent = presentation.statusText
    heading.textContent = state.session?.title ?? 'Chat'
    messages.setAttribute('aria-busy', String(presentation.messageListBusy))
    composerLabel.textContent = presentation.composerLabel
    composer.disabled = readOnly || presentation.composerDisabled
    send.disabled = readOnly || presentation.composerDisabled || composer.value.trim().length === 0
    send.textContent = presentation.sendLabel
    cancel.hidden = !presentation.cancelVisible
    cancel.disabled = readOnly || state.interaction.status === 'cancelling'
    empty.hidden = state.messages.length > 0
    empty.textContent = presentation.emptyText
    loadEarlier.hidden = !state.messagePagination.hasMore
    loadEarlier.disabled = state.messagePagination.status === 'loading'
    error.hidden = presentation.errorText === null
    errorText.textContent = presentation.errorText ?? ''
    retry.hidden = presentation.errorText === null

    const renderedModelRoutes = state.modelRouteAvailability?.items ?? Object.freeze([])
    const availableRoutes = renderedModelRoutes.filter(modelRouteReady)
    const selectedIdentity = state.selectedModelRoute === null
      ? null
      : modelRouteIdentity(state.selectedModelRoute)
    const optionsToRender: readonly ModelOption[] = renderedModelRoutes.length === 0
      ? [{ key: 'empty', candidate: null }]
      : [
          ...(selectedIdentity === null && availableRoutes.length > 0
            ? [{ key: 'placeholder' as const, candidate: null }]
            : []),
          ...renderedModelRoutes.map(candidate => ({
            key: modelRouteIdentity(candidate.route),
            candidate,
          })),
        ]
    modelOptions.update(optionsToRender)
    const selectedOption = selectedIdentity === null
      ? optionsToRender.findIndex(item => item.candidate === null)
      : optionsToRender.findIndex(item => item.key === selectedIdentity)
    if (modelSelect.selectedIndex !== Math.max(0, selectedOption)) {
      modelSelect.selectedIndex = Math.max(0, selectedOption)
    }
    const pageUnavailable = state.status === 'authentication-required'
      || state.status === 'authorization-denied'
      || state.status === 'closed'
      || state.status === 'loading'
      || state.status === 'refreshing'
      || state.status === 'cancelled'
      || state.status === 'error'
      || ['submitting', 'cancelling'].includes(state.interaction.status)
    modelSelect.disabled = availableRoutes.length === 0 || pageUnavailable
    modelSettings.hidden = availableRoutes.length > 0
    modelNotice.hidden = state.modelRouteSelectionIssue === null
    modelNotice.textContent = state.modelRouteSelectionIssue === null
      ? ''
      : 'The previously selected model route is no longer ready: '
        + `${modelRouteReasonLabel(state.modelRouteSelectionIssue)}. `
        + 'Choose an enabled route.'
    newSession.disabled = readOnly || options.nextProductSessionId === undefined
      || state.selectedModelRoute === null
      || pageUnavailable
    convertDelivery.update({
      className: 'wwc-chat-convert-delivery',
      label: 'Convert to StrongFlow',
      type: 'button',
      variant: 'primary',
      disabled: readOnly || options.deliveryCreator === undefined
        || options.scope === undefined
        || state.session === null
        || confirmedRequirement(state) === null
        || pageUnavailable,
      onActivate() {
        const session = options.model.state.session
        const requirement = confirmedRequirement(options.model.state)
        if (
          options.deliveryCreator === undefined
          || options.scope === undefined
          || session === null
          || requirement === null
        ) return
        if (conversionSessionId !== session.id) {
          conversionSessionId = session.id
          conversionTitle.value = session.title
          conversionGoal.value = requirement
          conversionSourceSession.value = session.id
          conversionScope.value = [
            options.scope.organizationId,
            options.scope.workspaceId,
            options.scope.projectId,
            options.scope.repositoryId,
          ].join(' / ')
          const route = options.model.state.selectedModelRoute
          conversionModel.value = route === null
            ? 'Model context unavailable'
            : `${route.providerId} / ${route.modelId}`
          conversionBaseline.value = ''
          conversionDeliveryScope.value = requirement
          conversionOutOfScope.value = ''
          conversionConstraints.value = ''
          conversionCriteria.value = ''
          confirmation.checked = false
        }
        conversionOpen = true
        renderConversion(options.deliveryCreator.state)
      },
    })

    sessionCollection.update(state.sessions)
    messageCollection.update(state.messages)

    const decisions = contextualDecisions({
      inputs: state.pendingInputs,
      approvals: state.pendingApprovals,
      attention: [],
      nowMillis: nowMillis(),
    })
    decisionCard.update({
      view: decisions,
      presentation: contextualDecisionPresentation(decisions, {
        busy: ['submitting', 'cancelling', 'waiting'].includes(state.interaction.status),
        pageUnavailable: state.status === 'authentication-required'
          || state.status === 'authorization-denied'
          || state.status === 'closed',
        readOnly,
      }),
    })
  }

  function closeConversion(): void {
    if (!conversionOpen) return
    conversionOpen = false
    conversionSessionId = null
    renderConversion(options.deliveryCreator?.state ?? { status: 'idle', error: null })
  }

  function renderConversion(state: ChatDeliveryCreatorState): void {
    if (closed) return
    const busy = state.status === 'submitting' || state.status === 'waiting'
    const wasOpen = !conversion.hidden
    conversion.hidden = !conversionOpen
    convertDelivery.root.setAttribute('aria-expanded', String(conversionOpen))
    if (conversionOpen && !wasOpen) {
      // UI-604: a keyboard user activating the trigger has to land inside the
      // dialog, and the trigger has to be remembered so closing restores them.
      conversionFocusReturn = focusableElement(document.activeElement)
      conversionTitle.focus()
    } else if (!conversionOpen && wasOpen) {
      conversionFocusReturn?.focus()
      conversionFocusReturn = null
    }
    conversionForm.setAttribute('aria-busy', String(busy))
    const visibleError = deliveryConversionError(state)
    conversionError.hidden = visibleError === null
    conversionError.textContent = visibleError ?? ''
    conversionSubmit.update({
      className: 'wwc-chat-convert-submit',
      label: 'Confirm and create Delivery',
      busy,
      busyLabel: state.status === 'waiting' ? 'Waiting for Delivery…' : 'Creating Delivery…',
      disabled: readOnly || !conversionOpen || state.status === 'created' || state.status === 'closed',
      type: 'submit',
      variant: 'primary',
    })
    conversionCancel.update({
      className: 'wwc-chat-convert-cancel',
      label: busy ? 'Cancel pending creation' : 'Cancel conversion',
      type: 'button',
      onActivate() {
        if (options.deliveryCreator === undefined) return
        if (busy) {
          options.deliveryCreator.cancelPending()
          return
        }
        closeConversion()
      },
    })
  }

  const onConversionKeyDown = (event: KeyboardEvent) => {
    if (event.key !== 'Escape' || !conversionOpen) return
    event.preventDefault()
    closeConversion()
  }
  const onComposerInput = () => {
    send.disabled = readOnly || chatPagePresentation(options.model.state).composerDisabled
      || composer.value.trim().length === 0
  }
  const onModelRouteChange = () => {
    const selectedOption = modelSelect.children[modelSelect.selectedIndex] as
      | HTMLOptionElement
      | undefined
    const selected = options.model.state.modelRouteAvailability?.items.find(candidate => (
      modelRouteIdentity(candidate.route) === selectedOption?.value
    ))
    if (selected === undefined || !modelRouteReady(selected)) return
    options.model.selectModelRoute(selected.route)
  }
  const onComposerKeydown = (event: KeyboardEvent) => {
    if (readOnly) return
    if (chatComposerKeyAction(event) !== 'submit') return
    event.preventDefault()
    form.requestSubmit()
  }
  const onComposerSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (readOnly) return
    const draft = composer.value.trim()
    if (draft.length === 0) return
    void options.model.submitMessage(draft).then(() => {
      if (options.model.state.interaction.status !== 'error') {
        composer.value = ''
        render(options.model.state)
      }
    })
  }
  const onConversionSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    if (readOnly) return
    if (
      options.deliveryCreator === undefined
      || !conversionOpen
      || conversionSessionId === null
      || !confirmation.checked
      || ['submitting', 'waiting', 'created', 'closed'].includes(
        options.deliveryCreator.state.status,
      )
    ) return
    void options.deliveryCreator.create({
      title: conversionTitle.value,
      goal: conversionGoal.value,
      baseRevision: conversionBaseline.value,
      scope: conversionDeliveryScope.value.split(/\r?\n/u),
      outOfScope: conversionOutOfScope.value.split(/\r?\n/u),
      constraints: conversionConstraints.value.split(/\r?\n/u),
      sourceProductSessionId: conversionSessionId,
      acceptanceCriteria: conversionCriteria.value.split(/\r?\n/u),
    })
  }
  const onCancel = () => {
    if (readOnly) return
    void options.model.cancelSession('Stopped from the Chat page.')
  }
  const onNewSession = () => {
    if (readOnly) return
    if (options.model.state.selectedModelRoute === null
      || options.nextProductSessionId === undefined) return
    void options.model.createSession({
      productSessionId: options.nextProductSessionId(),
      title: 'New Chat',
    })
  }
  const onRetry = () => { void options.model.refresh() }
  const onLoadEarlier = () => { void options.model.loadMoreMessages() }

  composer.addEventListener('input', onComposerInput)
  modelSelect.addEventListener('change', onModelRouteChange)
  composer.addEventListener('keydown', onComposerKeydown)
  form.addEventListener('submit', onComposerSubmit)
  conversionForm.addEventListener('submit', onConversionSubmit)
  conversion.addEventListener('keydown', onConversionKeyDown)
  cancel.addEventListener('click', onCancel)
  newSession.addEventListener('click', onNewSession)
  retry.addEventListener('click', onRetry)
  loadEarlier.addEventListener('click', onLoadEarlier)

  const unsubscribe = options.model.subscribe(render)
  const unsubscribeDeliveryCreator = options.deliveryCreator?.subscribe(renderConversion)
  void options.model.start()

  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      unsubscribeDeliveryCreator?.()
      composer.removeEventListener('input', onComposerInput)
      modelSelect.removeEventListener('change', onModelRouteChange)
      composer.removeEventListener('keydown', onComposerKeydown)
      form.removeEventListener('submit', onComposerSubmit)
      conversionForm.removeEventListener('submit', onConversionSubmit)
      conversion.removeEventListener('keydown', onConversionKeyDown)
      cancel.removeEventListener('click', onCancel)
      newSession.removeEventListener('click', onNewSession)
      retry.removeEventListener('click', onRetry)
      loadEarlier.removeEventListener('click', onLoadEarlier)
      for (const field of conversionFields) field.close()
      decisionCard.close()
      convertDelivery.close()
      conversionSubmit.close()
      conversionCancel.close()
      options.deliveryCreator?.close()
      messageCollection.close()
      sessionCollection.close()
      modelOptions.close()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
