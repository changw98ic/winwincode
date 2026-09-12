// SPDX-License-Identifier: Apache-2.0

import { formatInstant } from './format-instant.js'

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
    case 'signed-out': return '已退出登录'
    case 'restoring': return '正在恢复浏览器会话…'
    case 'signed-in': return state.session?.expiresAt === undefined
      ? '已登录（本会话内有效）'
      : `已登录，有效期至 ${formatInstant(state.session.expiresAt)}`
    case 'signing-out': return '正在退出登录…'
    case 'authentication-required': return '需要登录'
    case 'error': return '会话不可用'
    case 'closed': return '会话控件已关闭'
  }
}

function errorText(state: AuthSessionViewModelState): string {
  if (state.error === null) return ''
  if (state.error.kind === 'authentication') return '引导凭证被拒绝或已过期。'
  if (state.error.kind === 'network') return '无法连接认证服务器。'
  if (state.error.kind === 'version') return '客户端与服务器版本不一致。'
  if (state.error.kind === 'cancelled') return '会话请求已取消。'
  return '浏览器会话更新失败。'
}

/** Show browser-session status and sign-out; login belongs to the login page. */
export function mountAuthSessionPage(options: AuthSessionPageOptions): AuthSessionPage {
  const document = options.root.ownerDocument
  const region = element(document, 'section', 'wwc-auth-session')
  const status = element(document, 'p', 'wwc-auth-session-status')
  const error = element(document, 'p', 'wwc-auth-session-error')
  const signOut = element(document, 'button', 'wwc-auth-session-sign-out')
  let closed = false

  region.setAttribute('aria-label', '浏览器会话')
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  error.setAttribute('role', 'alert')
  signOut.type = 'button'
  signOut.textContent = '退出登录'
  region.append(status, error, signOut)
  options.root.replaceChildren(region)

  function render(state: AuthSessionViewModelState): void {
    if (closed) return
    const busy = state.status === 'restoring' || state.status === 'signing-out'
    status.textContent = statusText(state)
    error.textContent = errorText(state)
    error.hidden = state.error === null
    signOut.disabled = busy || state.status !== 'signed-in'
    signOut.hidden = state.status !== 'signed-in'
  }

  const unsubscribe = options.model.subscribe(render)
  const onSignOut = () => { void options.model.logout() }
  signOut.addEventListener('click', onSignOut)

  return {
    close() {
      if (closed) return
      closed = true
      signOut.removeEventListener('click', onSignOut)
      unsubscribe()
      options.root.replaceChildren()
    },
  }
}
