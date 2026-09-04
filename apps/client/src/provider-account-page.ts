// SPDX-License-Identifier: Apache-2.0

import type { ProviderAccountConnectionId } from './generated/contracts.js'
import type {
  ProviderAccountViewModel,
  ProviderAccountViewModelState,
} from './provider-account-view-model.js'

export interface ProviderAccountPageOptions {
  readonly root: HTMLElement
  readonly model: ProviderAccountViewModel
  readonly nextConnectionId: () => ProviderAccountConnectionId
}

export interface ProviderAccountPage { close(): void }

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

function errorText(state: ProviderAccountViewModelState): string | null {
  const error = state.error
  if (error === null) return null
  if (error.kind === 'authentication') return 'Sign in again to manage your ChatGPT account.'
  if (error.kind === 'authorization') return 'This identity cannot manage the selected ChatGPT account.'
  if (error.kind === 'network') return 'The account service could not be reached.'
  if (error.code === 'REVISION_CONFLICT') return 'The account changed. Refresh and try again.'
  return 'The ChatGPT account operation did not finish. Refresh and try again.'
}

/** Mounts personal ChatGPT device sign-in without exposing provider credentials. */
export function mountProviderAccountPage(options: ProviderAccountPageOptions): ProviderAccountPage {
  const document = options.root.ownerDocument
  const section = element(document, 'section', 'wwc-provider-accounts')
  const heading = element(document, 'h2', 'wwc-provider-accounts-heading')
  const help = element(document, 'p', 'wwc-provider-accounts-help')
  const status = element(document, 'p', 'wwc-provider-accounts-status')
  const error = element(document, 'p', 'wwc-provider-accounts-error')
  const form = element(document, 'form', 'wwc-provider-accounts-form')
  const label = element(document, 'label', 'wwc-provider-accounts-label')
  const name = element(document, 'input', 'wwc-provider-accounts-name')
  const connect = element(document, 'button', 'wwc-provider-accounts-connect')
  const refresh = element(document, 'button', 'wwc-provider-accounts-refresh')
  const list = element(document, 'ul', 'wwc-provider-accounts-list')
  let closed = false

  heading.textContent = 'My ChatGPT account'
  help.textContent = 'Connect with the ChatGPT device sign-in page. WinWinCode stores the resulting credential only in its local secret store.'
  status.setAttribute('role', 'status')
  error.setAttribute('role', 'alert')
  label.textContent = 'Account name'
  name.required = true
  name.maxLength = 200
  name.autocomplete = 'off'
  label.append(name)
  connect.type = 'submit'
  connect.textContent = 'Connect ChatGPT'
  refresh.type = 'button'
  refresh.textContent = 'Refresh accounts'
  form.append(label, connect)
  section.append(heading, help, status, error, form, refresh, list)
  options.root.replaceChildren(section)

  function render(state: ProviderAccountViewModelState): void {
    if (closed) return
    const personal = state.connections.filter(connection => connection.owner.kind === 'user')
    status.textContent = state.status === 'loading'
      ? 'Loading ChatGPT accounts…'
      : state.submitting
        ? 'Saving ChatGPT account…'
        : `${personal.length} personal account${personal.length === 1 ? '' : 's'}`
    error.textContent = errorText(state) ?? ''
    error.hidden = state.error === null
    connect.disabled = state.submitting || state.status === 'loading'
    refresh.disabled = state.submitting || state.status === 'loading'
    list.replaceChildren(...personal.map(connection => {
      const item = element(document, 'li', 'wwc-provider-account-item')
      const title = element(document, 'h3', 'wwc-provider-account-title')
      const metadata = element(document, 'p', 'wwc-provider-account-metadata')
      const controls = element(document, 'div', 'wwc-provider-account-controls')
      title.textContent = connection.displayName
      metadata.textContent = `${connection.accountLabel ?? 'Account identity pending'} · ${connection.state}`
      if (connection.loginPrompt !== null) {
        const prompt = element(document, 'p', 'wwc-provider-account-login-prompt')
        const link = element(document, 'a', 'wwc-provider-account-login-link')
        const complete = element(document, 'button', 'wwc-provider-account-complete')
        link.href = connection.loginPrompt.verificationUrl
        link.target = '_blank'
        link.rel = 'noopener noreferrer'
        link.textContent = `Open sign-in page · code ${connection.loginPrompt.userCode}`
        complete.type = 'button'
        complete.textContent = 'I finished signing in'
        complete.disabled = state.submitting
        complete.addEventListener('click', () => { void options.model.completeConnection(connection) })
        prompt.append(link, complete)
        item.append(title, metadata, prompt)
      } else {
        item.append(title, metadata)
      }
      if (connection.state === 'active' || connection.state === 'refresh_required') {
        const renew = element(document, 'button', 'wwc-provider-account-renew')
        renew.type = 'button'
        renew.textContent = 'Refresh sign-in'
        renew.disabled = state.submitting
        renew.addEventListener('click', () => { void options.model.refreshConnection(connection) })
        controls.append(renew)
      }
      if (connection.state !== 'revoked') {
        const revoke = element(document, 'button', 'wwc-provider-account-revoke')
        revoke.type = 'button'
        revoke.textContent = 'Disconnect'
        revoke.disabled = state.submitting
        revoke.addEventListener('click', () => { void options.model.revokeConnection(connection) })
        controls.append(revoke)
      }
      item.append(controls)
      return item
    }))
  }

  form.addEventListener('submit', event => {
    event.preventDefault()
    const displayName = name.value.trim()
    if (displayName.length === 0) return
    void options.model.startPersonalConnection(options.nextConnectionId(), displayName).then(() => {
      if (options.model.state.error === null) name.value = ''
    })
  })
  refresh.addEventListener('click', () => { void options.model.refresh() })
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
