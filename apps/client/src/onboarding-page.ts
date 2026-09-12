// SPDX-License-Identifier: Apache-2.0

import {
  mountPageHeader,
} from '@winwincode/browser-ui'

/**
 * Design page 02: 首次设置 第 1 步 — 连接执行设备. A bare-canvas flow page
 * (no sidebar): brand row, step eyebrow, the display title, the pairing-code
 * form, the 「如何启动 Client」 disclosure, and the what's-next footer.
 */
export interface OnboardingPageOptions {
  readonly root: HTMLElement
  /** Submits the pairing code; resolves on success, rejects with a message. */
  readonly connect: (connectionCode: string) => Promise<void>
  /** Signed-out entry in the brand row (设计稿 02 右上角). */
  readonly onSignOut: () => void
}

export interface OnboardingPage {
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

const PAIRING_CODE_PATTERN = /^\d{8}$/u

export function mountOnboardingPage(options: OnboardingPageOptions): OnboardingPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'section', 'wwc-onboarding')

  const brandRow = element(document, 'div', 'wwc-onboarding-brand-row')
  const brand = element(document, 'p', 'wwc-onboarding-brand')
  brand.textContent = 'WinWinCode'
  const brandSub = element(document, 'span', 'wwc-onboarding-brand-sub')
  brandSub.textContent = '社区版'
  brand.append(brandSub)
  const signOut = element(document, 'button', 'wwc-onboarding-sign-out')
  signOut.type = 'button'
  signOut.textContent = '退出登录'
  signOut.addEventListener('click', options.onSignOut)
  brandRow.append(brand, signOut)

  const eyebrow = element(document, 'p', 'wwc-onboarding-eyebrow')
  eyebrow.textContent = '首次设置 · 第 1 步，共 3 步'

  const pageHeader = mountPageHeader({
    document,
    props: {
      title: '连接执行设备',
      headingLevel: 1,
      className: 'wwc-onboarding-heading',
    },
  })

  const subtitle = element(document, 'p', 'wwc-onboarding-subtitle')
  subtitle.textContent = '在执行任务的电脑上启动执行设备客户端，输入它显示的配对码。'

  const form = element(document, 'form', 'wwc-onboarding-form')
  const codeLabel = element(document, 'label', 'wwc-onboarding-label')
  codeLabel.htmlFor = 'wwc-onboarding-code'
  codeLabel.textContent = '配对码'
  const code = element(document, 'input', 'wwc-onboarding-control')
  code.id = 'wwc-onboarding-code'
  code.inputMode = 'numeric'
  code.autocomplete = 'off'
  code.placeholder = '输入配对码'
  const submit = element(document, 'button', 'wwc-onboarding-submit')
  submit.type = 'submit'
  submit.textContent = '连接设备'
  const feedback = element(document, 'p', 'wwc-onboarding-feedback')
  feedback.setAttribute('role', 'status')
  form.append(codeLabel, code, submit, feedback)

  const helpContent = element(document, 'div', 'wwc-onboarding-help')
  const helpText = element(document, 'p', 'wwc-onboarding-help-text')
  helpText.textContent = '在执行任务的电脑上安装并启动 WinWinCode 客户端；启动后会显示 8 位配对码，把它填在这里即可连接。'
  helpContent.append(helpText)
  const helpToggle = element(document, 'button', 'wwc-onboarding-help-toggle')
  helpToggle.type = 'button'
  helpToggle.setAttribute('aria-expanded', 'false')
  helpToggle.setAttribute('aria-controls', 'wwc-onboarding-help-content')
  helpContent.id = 'wwc-onboarding-help-content'
  helpContent.hidden = true
  const helpLabel = element(document, 'span', 'wwc-onboarding-help-label')
  helpLabel.textContent = '如何启动执行设备客户端'
  helpToggle.append(helpLabel)
  helpToggle.addEventListener('click', () => {
    const expanded = helpToggle.getAttribute('aria-expanded') === 'true'
    helpToggle.setAttribute('aria-expanded', expanded ? 'false' : 'true')
    helpContent.hidden = expanded
  })

  const footer = element(document, 'p', 'wwc-onboarding-footer')
  footer.textContent = '接下来：选择仓库、设置模型'

  form.addEventListener('submit', event => {
    event.preventDefault()
    const pairingCode = code.value.replace(/\D+/gu, '')
    if (!PAIRING_CODE_PATTERN.test(pairingCode)) {
      feedback.textContent = '配对码是执行设备客户端显示的 8 位数字。'
      return
    }
    submit.disabled = true
    feedback.textContent = '正在连接执行设备…'
    void options.connect(pairingCode)
      .then(() => {
        feedback.textContent = '已连接。正在打开执行设备页…'
      })
      .catch((error: unknown) => {
        submit.disabled = false
        feedback.textContent = error instanceof Error && error.message.length > 0
          ? error.message
          : '连接失败。请确认客户端已启动并重新输入配对码。'
      })
  })

  layout.append(brandRow, eyebrow, pageHeader.root, subtitle, form, helpToggle, helpContent, footer)
  options.root.replaceChildren(layout)

  return {
    close() {
      options.root.replaceChildren()
    },
  }
}
