// SPDX-License-Identifier: Apache-2.0

export const APPROVAL_TEXT_LIMIT = 200

const ELLIPSIS = '…'
const SEPARATOR = ' '
const WHITESPACE = /\s+/gu
const HIDDEN_CONTROLS = /[\u200B-\u200F\u202A-\u202E\u2066-\u2069\uFEFF]/gu

export interface ApprovalBoundText {
  readonly text: string
  readonly truncated: boolean
}

export function boundApprovalText(value: string): ApprovalBoundText {
  const cleaned = (value ?? '').replace(HIDDEN_CONTROLS, '').replace(WHITESPACE, SEPARATOR).trim()
  if (cleaned.length <= APPROVAL_TEXT_LIMIT) {
    return Object.freeze({ text: cleaned, truncated: false })
  }
  return Object.freeze({
    text: `${cleaned.slice(0, APPROVAL_TEXT_LIMIT - 1)}${ELLIPSIS}`,
    truncated: true,
  })
}
