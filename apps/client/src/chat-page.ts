// SPDX-License-Identifier: Apache-2.0

import type {
  ChatViewModel,
  ChatViewModelState,
} from './chat-view-model.js'
import type { ControlPlaneClientError } from './control-plane-client.js'
import type {
  ModelRoute,
  ProductSessionId,
} from './generated/contracts.js'

export interface ChatPageOptions {
  readonly root: HTMLElement
  readonly model: ChatViewModel
  readonly modelRoutes?: readonly ModelRoute[]
  readonly nextProductSessionId?: () => ProductSessionId
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

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
  if (error.kind === 'authentication') return 'Sign in again to continue this Chat.'
  if (error.kind === 'authorization') return 'You do not have access to this Chat.'
  if (error.kind === 'network') return 'The Chat server could not be reached. Check the connection and retry.'
  if (error.kind === 'version') return 'The Client and Server versions differ. Update the Client and retry.'
  if (error.kind === 'cancelled') return 'The Chat update was cancelled.'
  if (error.kind === 'configuration') return 'Chat needs a valid server configuration.'
  return 'Chat could not be updated. Retry, or review the server status.'
}

export function chatPagePresentation(state: ChatViewModelState): ChatPagePresentation {
  const error = state.interaction.error ?? state.messagePagination.error ?? state.error
  const running = state.session?.state === 'running'
  const continuing = state.session?.state === 'waiting_for_input'
  const mutationBusy = ['submitting', 'cancelling'].includes(state.interaction.status)
  return Object.freeze({
    statusText: stateLabel(state),
    emptyText: state.session === null
      ? 'Select a session to start chatting.'
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

/** Mount the default, keyboard-accessible Chat page against the read/write view-model only. */
export function mountChatPage(options: ChatPageOptions): ChatPage {
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
  let closed = false
  let renderedModelRoutes: readonly ModelRoute[] = Object.freeze([])

  sessionHeading.textContent = 'Sessions'
  newSession.type = 'button'
  newSession.textContent = 'New Chat'
  heading.textContent = 'Chat'
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  modelLabel.textContent = 'Default model'
  modelLabel.htmlFor = 'wwc-chat-model'
  modelSelect.id = 'wwc-chat-model'
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
  header.append(heading, status, modelLabel)
  conversation.append(header, error, loadEarlier, messages, empty, form)
  layout.append(sessionPanel, conversation)
  options.root.replaceChildren(layout)

  function render(state: ChatViewModelState): void {
    if (closed) return
    const presentation = chatPagePresentation(state)
    status.textContent = presentation.statusText
    heading.textContent = state.session?.title ?? 'Chat'
    messages.setAttribute('aria-busy', String(presentation.messageListBusy))
    composerLabel.textContent = presentation.composerLabel
    composer.disabled = presentation.composerDisabled
    send.disabled = presentation.composerDisabled || composer.value.trim().length === 0
    send.textContent = presentation.sendLabel
    cancel.hidden = !presentation.cancelVisible
    cancel.disabled = state.interaction.status === 'cancelling'
    empty.hidden = state.messages.length > 0
    empty.textContent = presentation.emptyText
    loadEarlier.hidden = !state.messagePagination.hasMore
    loadEarlier.disabled = state.messagePagination.status === 'loading'
    error.hidden = presentation.errorText === null
    errorText.textContent = presentation.errorText ?? ''
    retry.hidden = presentation.errorText === null

    const route = state.defaultModelRoute
    const routeByIdentity = new Map<string, ModelRoute>()
    for (const candidate of [route, ...(options.modelRoutes ?? [])]) {
      if (candidate === null) continue
      routeByIdentity.set(
        `${candidate.providerId}\u0000${candidate.modelId}\u0000${candidate.credentialReferenceId}`,
        candidate,
      )
    }
    renderedModelRoutes = Object.freeze([...routeByIdentity.values()])
    const previousModelIndex = modelSelect.selectedIndex
    modelSelect.replaceChildren()
    if (renderedModelRoutes.length === 0) {
      const option = document.createElement('option')
      option.value = ''
      option.textContent = 'No model configured'
      modelSelect.append(option)
    } else {
      modelSelect.append(...renderedModelRoutes.map((candidate, index) => {
        const option = document.createElement('option')
        option.value = String(index)
        option.textContent = `${candidate.providerId} / ${candidate.modelId}`
        return option
      }))
      modelSelect.selectedIndex = previousModelIndex < 0
        ? 0
        : Math.min(previousModelIndex, renderedModelRoutes.length - 1)
    }
    const pageUnavailable = state.status === 'authentication-required'
      || state.status === 'authorization-denied'
      || state.status === 'closed'
      || ['submitting', 'cancelling'].includes(state.interaction.status)
    modelSelect.disabled = renderedModelRoutes.length < 2 || pageUnavailable
    newSession.disabled = options.nextProductSessionId === undefined
      || renderedModelRoutes.length === 0
      || pageUnavailable

    sessionList.replaceChildren(...state.sessions.map(session => {
      const item = document.createElement('li')
      const button = document.createElement('button')
      button.type = 'button'
      button.textContent = session.title
      button.dataset.sessionId = session.id
      if (session.id === state.activeProductSessionId) button.setAttribute('aria-current', 'true')
      button.addEventListener('click', () => { void options.model.selectSession(session.id) })
      item.append(button)
      return item
    }))

    messages.replaceChildren(...state.messages.map(message => {
      const item = document.createElement('li')
      const article = document.createElement('article')
      const role = document.createElement('h3')
      const content = document.createElement('p')
      const stateText = messageStateText(message.state)
      article.dataset.role = message.role
      article.dataset.state = message.state
      article.setAttribute('aria-busy', String(message.state === 'streaming'))
      role.textContent = message.role === 'user' ? 'You' : 'WinWinCode'
      content.textContent = message.content.length === 0 && message.state === 'streaming'
        ? 'Responding…'
        : message.content
      article.append(role, content)
      if (stateText !== null) {
        const badge = document.createElement('span')
        badge.className = 'wwc-chat-message-state'
        badge.textContent = stateText
        article.append(badge)
      }
      item.append(article)
      return item
    }))
  }

  composer.addEventListener('input', () => {
    send.disabled = chatPagePresentation(options.model.state).composerDisabled
      || composer.value.trim().length === 0
  })
  composer.addEventListener('keydown', event => {
    if (chatComposerKeyAction(event) !== 'submit') return
    event.preventDefault()
    form.requestSubmit()
  })
  form.addEventListener('submit', event => {
    event.preventDefault()
    const draft = composer.value.trim()
    if (draft.length === 0) return
    void options.model.submitMessage(draft).then(() => {
      if (options.model.state.interaction.status !== 'error') {
        composer.value = ''
        render(options.model.state)
      }
    })
  })
  cancel.addEventListener('click', () => {
    void options.model.cancelSession('Stopped from the Chat page.')
  })
  newSession.addEventListener('click', () => {
    const route = renderedModelRoutes[modelSelect.selectedIndex]
    if (route === undefined || options.nextProductSessionId === undefined) return
    void options.model.createSession({
      productSessionId: options.nextProductSessionId(),
      title: 'New Chat',
      modelRoute: route,
    })
  })
  retry.addEventListener('click', () => { void options.model.refresh() })
  loadEarlier.addEventListener('click', () => { void options.model.loadMoreMessages() })

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
