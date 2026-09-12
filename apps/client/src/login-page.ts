// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneLoginFailure } from './community-control-plane-client.js'
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
      case 'invalid-credentials': return '引导凭证被拒绝。'
      case 'rate-limited': return '尝试次数过多，请稍后再试。'
      case 'unavailable': return '暂时无法初始化，请稍后再试。'
    }
  }
  switch (failure) {
    case 'invalid-credentials': return '用户名或密码不正确。'
    case 'rate-limited': return '登录尝试次数过多，请稍后再试。'
    case 'unavailable': return '暂时无法登录，请检查连接后重试。'
  }
}

function statusText(state: LoginViewModelState): string {
  if (state.status === 'submitting') {
    return state.source === 'initialization'
      ? '正在初始化服务器所有者账号…'
      : '正在登录…'
  }
  if (state.status === 'succeeded') return '登录成功，正在返回工作区…'
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
  // Design page 01: the password field carries a visibility toggle.
  const passwordToggle = element(document, 'button', 'wwc-login-password-toggle')
  const passwordWrap = element(document, 'div', 'wwc-login-password-wrap')
  const submit = element(document, 'button', 'wwc-login-submit')
  const initialization = element(document, 'div', 'wwc-login-initialization')
  const initializationHeading = element(document, 'p', 'wwc-login-initialization-heading')
  const initializationDetail = element(document, 'p', 'wwc-login-initialization-detail')
  const initializationForm = element(document, 'form', 'wwc-login-initialization-form')
  const initializationUsernameLabel = element(document, 'label', 'wwc-login-label')
  const initializationUsername = element(
    document,
    'input',
    'wwc-login-control wwc-login-initialization-username',
  )
  const initializationPasswordLabel = element(document, 'label', 'wwc-login-label')
  const initializationPassword = element(
    document,
    'input',
    'wwc-login-control wwc-login-initialization-password',
  )
  const proofLabel = element(document, 'label', 'wwc-login-label')
  const proof = element(document, 'input', 'wwc-login-control wwc-login-initialization-proof')
  const initializationSubmit = element(document, 'button', 'wwc-login-initialization-submit')
  let closed = false

  region.setAttribute('aria-label', '登录')
  region.hidden = true
  heading.textContent = '登录 WinWinCode'
  const subtitle = element(document, 'p', 'wwc-login-subtitle')
  subtitle.textContent = '进入你的工作空间'
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
  usernameLabel.textContent = '账号'

  password.id = 'wwc-login-password'
  password.name = 'password'
  password.type = 'password'
  password.autocomplete = 'current-password'
  password.maxLength = 4096
  password.required = true
  passwordLabel.htmlFor = password.id
  passwordLabel.textContent = '密码'

  passwordToggle.type = 'button'
  passwordToggle.setAttribute('aria-label', '显示密码')
  passwordToggle.setAttribute('aria-pressed', 'false')
  // Inline SVG eye icon. Set through innerHTML on purpose: it is purely
  // presentational, and a data:/http icon URL would surface as a network
  // request, which the security audits correctly treat as a leak.
  passwordToggle.innerHTML = [
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"',
    ' stroke="currentColor" stroke-width="2" stroke-linecap="round"',
    ' stroke-linejoin="round" aria-hidden="true" width="20" height="20">',
    '<path d="M2 12s3.5-6 10-6 10 6 10 6-3.5 6-10 6-10-6-10-6Z"/>',
    '<circle cx="12" cy="12" r="3"/>',
    '</svg>',
  ].join('')
  passwordToggle.addEventListener('click', () => {
    const visible = password.type === 'text'
    password.type = visible ? 'password' : 'text'
    passwordToggle.setAttribute('aria-pressed', visible ? 'false' : 'true')
    passwordToggle.setAttribute('aria-label', visible ? '显示密码' : '隐藏密码')
  })
  passwordWrap.append(password, passwordToggle)

  submit.type = 'submit'
  submit.textContent = '登录'
  form.setAttribute('aria-labelledby', heading.id)
  form.append(usernameLabel, username, passwordLabel, passwordWrap, submit)

  initializationHeading.textContent = '首次初始化'
  initializationDetail.textContent = '服务器还没有账号。输入服务器所有者环境中的引导凭证，创建第一个所有者账号。'
  initializationUsername.id = 'wwc-login-initialization-username'
  initializationUsername.name = 'username'
  initializationUsername.type = 'text'
  initializationUsername.autocomplete = 'username'
  initializationUsername.spellcheck = false
  initializationUsername.setAttribute('autocapitalize', 'none')
  initializationUsername.maxLength = 128
  initializationUsername.required = true
  initializationUsernameLabel.htmlFor = initializationUsername.id
  initializationUsernameLabel.textContent = '所有者账号'

  initializationPassword.id = 'wwc-login-initialization-password'
  initializationPassword.name = 'password'
  initializationPassword.type = 'password'
  initializationPassword.autocomplete = 'new-password'
  initializationPassword.maxLength = 4096
  initializationPassword.required = true
  initializationPasswordLabel.htmlFor = initializationPassword.id
  initializationPasswordLabel.textContent = '所有者密码'

  proof.id = 'wwc-login-initialization-proof'
  proof.type = 'password'
  proof.autocomplete = 'off'
  proof.spellcheck = false
  proof.setAttribute('autocapitalize', 'none')
  proof.required = true
  proofLabel.htmlFor = proof.id
  proofLabel.textContent = '引导凭证'
  initializationSubmit.type = 'submit'
  initializationSubmit.textContent = '初始化所有者账号'
  initializationForm.append(
    initializationUsernameLabel,
    initializationUsername,
    initializationPasswordLabel,
    initializationPassword,
    proofLabel,
    proof,
    initializationSubmit,
  )
  initialization.append(initializationHeading, initializationDetail, initializationForm)
  initialization.hidden = true

  region.append(heading, subtitle, status, error, form, initialization)
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
    setFieldError(initializationUsername, nextFailure !== null && state.source === 'initialization')
    setFieldError(initializationPassword, nextFailure !== null && state.source === 'initialization')
    username.disabled = busy || finished
    password.disabled = busy || finished
    submit.disabled = busy || finished
    proof.disabled = busy || finished
    initializationUsername.disabled = busy || finished
    initializationPassword.disabled = busy || finished
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
    if (
      proof.value.length === 0
      || initializationUsername.value.length === 0
      || initializationPassword.value.length === 0
    ) return
    const submittedProof = proof.value
    const submittedUsername = initializationUsername.value
    const submittedPassword = initializationPassword.value
    proof.value = ''
    initializationPassword.value = ''
    void options.model.initialize({
      bootstrapProof: submittedProof,
      username: submittedUsername,
      password: submittedPassword,
    })
  }
  const onEdit = () => { clearErrorDraft() }
  form.addEventListener('submit', onSignIn)
  initializationForm.addEventListener('submit', onInitialization)
  username.addEventListener('input', onEdit)
  password.addEventListener('input', onEdit)
  proof.addEventListener('input', onEdit)
  initializationUsername.addEventListener('input', onEdit)
  initializationPassword.addEventListener('input', onEdit)

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
      initializationUsername.value = ''
      initializationPassword.value = ''
      form.removeEventListener('submit', onSignIn)
      initializationForm.removeEventListener('submit', onInitialization)
      username.removeEventListener('input', onEdit)
      password.removeEventListener('input', onEdit)
      proof.removeEventListener('input', onEdit)
      initializationUsername.removeEventListener('input', onEdit)
      initializationPassword.removeEventListener('input', onEdit)
      options.root.replaceChildren()
    },
  }
}
