// SPDX-License-Identifier: Apache-2.0

/**
 * Every free-form producer text a page renders (approval summaries, decision
 * titles, option labels) is bounded to this many characters.  The producer
 * summary is free-form Worker text, so the Client never trusts its length even
 * though the Server already validated the projection.
 */
export const APPROVAL_TEXT_LIMIT = 200

const ELLIPSIS = '…'
const SEPARATOR = ' '
const WHITESPACE = /\s+/gu
// Bidi and zero-width controls change what an operator reads without changing
// the bytes a command runs, so they are removed before text is rendered.
const HIDDEN_CONTROLS = /[\u200B-\u200F\u202A-\u202E\u2066-\u2069\uFEFF]/gu

export interface ApprovalBoundText {
  readonly text: string
  readonly truncated: boolean
}

/** Fold, strip, and bound one free-form producer string for rendering. */
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
