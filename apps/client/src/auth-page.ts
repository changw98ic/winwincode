// SPDX-License-Identifier: Apache-2.0

import type {
  AuthSessionViewModel,
  AuthSessionViewModelState,
} from './auth-view-model.js'

export interface AuthSessionPageOptions {
  readonly root: HTMLElement
  readonly model: AuthSessionViewModel
}

export interface AuthSessionPage {
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

function statusText(state: AuthSessionViewModelState): string {
  switch (state.status) {
    case 'signed-out': return 'Signed out'
    case 'restoring': return 'Restoring browser session…'
    case 'signing-in': return 'Signing in…'
    case 'signed-in': return `Signed in until ${state.session?.expiresAt ?? 'the current session expires'}`
    case 'signing-out': return 'Signing out…'
    case 'authentication-required': return 'Sign in required'
    case 'error': return 'Session unavailable'
    case 'closed': return 'Session controls closed'
  }
}

function errorText(state: AuthSessionViewModelState): string {
  if (state.error === null) return ''
  if (state.error.kind === 'authentication') return 'The bootstrap proof was rejected or expired.'
  if (state.error.kind === 'network') return 'The authentication server could not be reached.'
  if (state.error.kind === 'version') return 'The Client and Server versions differ.'
  if (state.error.kind === 'cancelled') return 'The session request was cancelled.'
  return 'The browser session could not be updated.'
}

/** Mount the write-only bootstrap form and browser-session close control. */
export function mountAuthSessionPage(options: AuthSessionPageOptions): AuthSessionPage {
  const document = options.root.ownerDocument
  const region = element(document, 'section', 'wwc-auth-session')
  const status = element(document, 'p', 'wwc-auth-session-status')
  const error = element(document, 'p', 'wwc-auth-session-error')
  const form = element(document, 'form', 'wwc-auth-session-form')
  const label = element(document, 'label', 'wwc-auth-session-label')
  const proof = element(document, 'input', 'wwc-auth-session-proof')
  const signIn = element(document, 'button', 'wwc-auth-session-sign-in')
  const signOut = element(document, 'button', 'wwc-auth-session-sign-out')
  let closed = false

  region.setAttribute('aria-label', 'Browser session')
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  error.setAttribute('role', 'alert')
  proof.id = 'wwc-auth-session-proof'
  proof.type = 'password'
  proof.autocomplete = 'off'
  proof.spellcheck = false
  proof.setAttribute('autocapitalize', 'none')
  label.htmlFor = proof.id
  label.textContent = 'Bootstrap proof'
  label.append(proof)
  signIn.type = 'submit'
  signIn.textContent = 'Sign in'
  signOut.type = 'button'
  signOut.textContent = 'Sign out'
  form.append(label, signIn)
  region.append(status, error, form, signOut)
  options.root.replaceChildren(region)

  function render(state: AuthSessionViewModelState): void {
    if (closed) return
    const busy = state.status === 'restoring'
      || state.status === 'signing-in'
      || state.status === 'signing-out'
    status.textContent = statusText(state)
    error.textContent = errorText(state)
    error.hidden = state.error === null
    proof.disabled = busy || state.status === 'signed-in' || state.status === 'closed'
    signIn.disabled = busy || state.status === 'signed-in' || state.status === 'closed'
    signOut.disabled = busy || state.status !== 'signed-in'
    form.hidden = state.status === 'signed-in'
    signOut.hidden = state.status !== 'signed-in'
  }

  const unsubscribe = options.model.subscribe(render)
  const onSubmit = (event: SubmitEvent) => {
    event.preventDefault()
    const submittedProof = proof.value
    proof.value = ''
    void options.model.login(submittedProof)
  }
  const onSignOut = () => { void options.model.logout() }
  form.addEventListener('submit', onSubmit)
  signOut.addEventListener('click', onSignOut)

  return {
    close() {
      if (closed) return
      closed = true
      proof.value = ''
      form.removeEventListener('submit', onSubmit)
      signOut.removeEventListener('click', onSignOut)
      unsubscribe()
      options.root.replaceChildren()
    },
  }
}
