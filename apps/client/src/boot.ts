// SPDX-License-Identifier: Apache-2.0

import { mountWinWinCodeClient } from './application.js'
import { readWinWinCodeClientRuntimeConfig } from './runtime-config.js'

function rootElement(): HTMLElement {
  const root = document.querySelector<HTMLElement>('[data-winwincode-client-root]')
  if (root === null) throw new Error('WinWinCode Client root element is missing.')
  return root
}

function renderStartupFailure(root: HTMLElement, error: unknown): void {
  const message = document.createElement('p')
  message.className = 'wwc-startup-error'
  message.setAttribute('role', 'alert')
  message.textContent = error instanceof Error
    ? error.message
    : 'WinWinCode Client could not read its runtime configuration.'
  root.replaceChildren(message)
}

const root = rootElement()
try {
  const config = readWinWinCodeClientRuntimeConfig()
  mountWinWinCodeClient({ root, serverUrl: config.serverUrl })
} catch (error) {
  renderStartupFailure(root, error)
}
