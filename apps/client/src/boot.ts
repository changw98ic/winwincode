// SPDX-License-Identifier: Apache-2.0

import { mountWinWinCodeClient } from './application.js'
import { readWinWinCodeClientRuntimeConfig } from './runtime-config.js'

function rootElement(): HTMLElement {
  const root = document.querySelector<HTMLElement>('[data-winwincode-client-root]')
  if (root === null) throw new Error('WinWinCode Client root element is missing.')
  return root
}

function renderStartupFailure(root: HTMLElement): void {
  const message = document.createElement('p')
  message.className = 'wwc-startup-error'
  message.setAttribute('role', 'alert')
  message.textContent = 'WinWinCode Client could not read its runtime configuration. Error code: CLIENT_STARTUP_FAILURE.'
  root.replaceChildren(message)
}

const root = rootElement()
try {
  const config = readWinWinCodeClientRuntimeConfig()
  mountWinWinCodeClient({ root, serverUrl: config.serverUrl })
} catch {
  renderStartupFailure(root)
}
