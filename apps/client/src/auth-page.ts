// SPDX-License-Identifier: Apache-2.0

import type { AuthSessionViewModel } from './auth-view-model.js'

export interface AuthSessionPageOptions {
  readonly root: HTMLElement
  readonly model: AuthSessionViewModel
}

export interface AuthSessionPage {
  close(): void
}

/** Signed-in session control. Account creation and login live only on the login page. */
export function mountAuthSessionPage(options: AuthSessionPageOptions): AuthSessionPage {
  const document = options.root.ownerDocument
  const region = document.createElement('section')
  const status = document.createElement('p')
  const signOut = document.createElement('button')
  region.className = 'wwc-auth-session'
  region.setAttribute('aria-label', 'Browser session')
  status.className = 'wwc-auth-session-status'
  signOut.className = 'wwc-auth-session-sign-out'
  signOut.type = 'button'
  signOut.textContent = 'Sign out'
  region.append(status, signOut)
  options.root.replaceChildren(region)
  let closed = false

  const onSignOut = () => { void options.model.logout() }
  signOut.addEventListener('click', onSignOut)
  const unsubscribe = options.model.subscribe(state => {
    if (closed) return
    const signedIn = state.status === 'signed-in' && state.session !== null
    region.hidden = !signedIn
    status.textContent = signedIn ? 'Signed in' : ''
    signOut.disabled = state.status === 'signing-out'
  })

  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      signOut.removeEventListener('click', onSignOut)
      options.root.replaceChildren()
    },
  }
}
