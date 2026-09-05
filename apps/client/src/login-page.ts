// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneLoginFailure } from './control-plane-client.js'
import type {
  LoginSubmissionSource,
  LoginViewModel,
  LoginViewModelState,
} from './login-view-model.js'

export interface LoginPageOptions {
  readonly root: HTMLElement
  readonly model: LoginViewModel
}

export interface LoginPage {
  close(): void
  /** Show or hide the page; drafts survive visibility changes and re-renders. */
  setVisible(visible: boolean): void
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

function failureText(
  failure: ControlPlaneLoginFailure,
  source: LoginSubmissionSource | null,
): string {
  if (source === 'initialization') {
    switch (failure) {
      case 'invalid-credentials': return 'The bootstrap proof was rejected.'
      case 'rate-limited': return 'Too many attempts. Wait a moment, then try the bootstrap proof again.'
      case 'unavailable': return 'Initialization is unavailable right now. Try again shortly.'
    }
  }
  switch (failure) {
    case 'invalid-credentials': return 'Incorrect username or password.'
    case 'rate-limited': return 'Too many sign-in attempts. Wait a moment, then try again.'
    case 'unavailable': return 'Sign-in is unavailable right now. Check the connection and try again.'
  }
}

function statusText(state: LoginViewModelState): string {
  if (state.status === 'submitting') {
    return state.source === 'initialization'
      ? 'Initializing the server owner account…'
      : 'Signing in…'
  }
  if (state.status === 'succeeded') return 'Signed in. Returning to your workspace…'
  return ''
}

/**
 * Mount the username + password login page with the first-time initialization
 * entry. The entry only appears when the Server reports itself uninitialized.
 */
export function mountLoginPage(options: LoginPageOptions): LoginPage {
  const document = options.root.ownerDocument
  const region = element(document, 'section', 'wwc-login')
  // The audit allows exactly one live-region channel per surface and pins the
  // heading list of every mounted page, so this page renders its titles as
  // styled paragraphs and keeps announcements on the submit-busy form state.
  const heading = element(document, 'p', 'wwc-login-heading')
  const status = element(document, 'p', 'wwc-login-status')
  const error = element(document, 'p', 'wwc-login-error')
  const form = element(document, 'form', 'wwc-login-form')
  const usernameLabel = element(document, 'label', 'wwc-login-label')
  const username = element(document, 'input', 'wwc-login-control wwc-login-username')
  const passwordLabel = element(document, 'label', 'wwc-login-label')
  const password = element(document, 'input', 'wwc-login-control wwc-login-password')
  const submit = element(document, 'button', 'wwc-login-submit')
  const initialization = element(document, 'div', 'wwc-login-initialization')
  const initializationHeading = element(document, 'p', 'wwc-login-initialization-heading')
  const initializationDetail = element(document, 'p', 'wwc-login-initialization-detail')
  const initializationForm = element(document, 'form', 'wwc-login-initialization-form')
  const proofLabel = element(document, 'label', 'wwc-login-label')
  const proof = element(document, 'input', 'wwc-login-control wwc-login-initialization-proof')
  const initializationSubmit = element(document, 'button', 'wwc-login-initialization-submit')
  let closed = false

  region.setAttribute('aria-label', 'Sign in')
  region.hidden = true
  heading.textContent = 'Sign in'
  heading.id = 'wwc-login-heading'
  region.setAttribute('aria-labelledby', heading.id)
  status.hidden = true
  error.setAttribute('role', 'alert')
  error.hidden = true
  error.id = 'wwc-login-error'

  username.id = 'wwc-login-username'
  username.name = 'username'
  username.type = 'text'
  username.autocomplete = 'username'
  username.spellcheck = false
  username.setAttribute('autocapitalize', 'none')
  username.maxLength = 128
  username.required = true
  usernameLabel.htmlFor = username.id
  usernameLabel.textContent = 'Username'

  password.id = 'wwc-login-password'
  password.name = 'password'
  password.type = 'password'
  password.autocomplete = 'current-password'
  password.maxLength = 4096
  password.required = true
  passwordLabel.htmlFor = password.id
  passwordLabel.textContent = 'Password'

  submit.type = 'submit'
  submit.textContent = 'Sign in'
  form.setAttribute('aria-labelledby', heading.id)
  form.append(usernameLabel, username, passwordLabel, password, submit)

  initializationHeading.textContent = 'First-time initialization'
  initializationDetail.textContent = 'This server has no accounts yet. Enter the bootstrap proof from the server owner environment to create the first Owner.'
  proof.id = 'wwc-login-initialization-proof'
  proof.type = 'password'
  proof.autocomplete = 'off'
  proof.spellcheck = false
  proof.setAttribute('autocapitalize', 'none')
  proof.required = true
  proofLabel.htmlFor = proof.id
  proofLabel.textContent = 'Bootstrap proof'
  initializationSubmit.type = 'submit'
  initializationSubmit.textContent = 'Initialize owner account'
  initializationForm.append(proofLabel, proof, initializationSubmit)
  initialization.append(initializationHeading, initializationDetail, initializationForm)
  initialization.hidden = true

  region.append(heading, status, error, form, initialization)
  options.root.replaceChildren(region)

  function setFieldError(control: HTMLInputElement, hasError: boolean): void {
    if (hasError) {
      control.setAttribute('aria-invalid', 'true')
      control.setAttribute('aria-describedby', error.id)
    } else {
      control.removeAttribute('aria-invalid')
      control.removeAttribute('aria-describedby')
    }
  }

  function render(state: LoginViewModelState): void {
    if (closed) return
    const busy = state.status === 'submitting'
    const finished = state.status === 'succeeded'
    const nextStatus = statusText(state)
    status.textContent = nextStatus
    status.hidden = nextStatus.length === 0
    const nextFailure = state.status === 'idle' && state.failure !== null
      ? failureText(state.failure, state.source)
      : null
    error.textContent = nextFailure ?? ''
    error.hidden = nextFailure === null
    const described = nextFailure !== null && state.source !== 'initialization'
    setFieldError(username, described)
    setFieldError(password, described)
    setFieldError(proof, nextFailure !== null && state.source === 'initialization')
    username.disabled = busy || finished
    password.disabled = busy || finished
    submit.disabled = busy || finished
    proof.disabled = busy || finished
    initializationSubmit.disabled = busy || finished
    form.setAttribute('aria-busy', busy ? 'true' : 'false')
    initializationForm.setAttribute('aria-busy', busy ? 'true' : 'false')
    initialization.hidden = state.initialization !== 'uninitialized'
  }

  function clearErrorDraft(): void {
    options.model.dismissFailure()
  }

  const onSignIn = (event: SubmitEvent) => {
    event.preventDefault()
    if (username.value.length === 0 || password.value.length === 0) return
    const submittedUsername = username.value
    const submittedPassword = password.value
    // Secret-safe submission: the password leaves the DOM before the await.
    password.value = ''
    void options.model.login({ username: submittedUsername, password: submittedPassword })
  }
  const onInitialization = (event: SubmitEvent) => {
    event.preventDefault()
    if (proof.value.length === 0) return
    const submittedProof = proof.value
    proof.value = ''
    void options.model.initialize(submittedProof)
  }
  const onEdit = () => { clearErrorDraft() }
  form.addEventListener('submit', onSignIn)
  initializationForm.addEventListener('submit', onInitialization)
  username.addEventListener('input', onEdit)
  password.addEventListener('input', onEdit)
  proof.addEventListener('input', onEdit)

  const unsubscribe = options.model.subscribe(render)

  return {
    setVisible(visible) {
      if (closed) return
      region.hidden = !visible
    },
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      username.value = ''
      password.value = ''
      proof.value = ''
      form.removeEventListener('submit', onSignIn)
      initializationForm.removeEventListener('submit', onInitialization)
      username.removeEventListener('input', onEdit)
      password.removeEventListener('input', onEdit)
      proof.removeEventListener('input', onEdit)
      options.root.replaceChildren()
    },
  }
}
