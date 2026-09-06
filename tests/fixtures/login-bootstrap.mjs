// SPDX-License-Identifier: Apache-2.0

/**
 * Shared login-page bootstrap helper for real-browser fixtures.
 *
 * The AUTH-100.3 session contract initializes the first Owner account from
 * one request: the Bearer bootstrap proof plus the Owner credentials the
 * operator chose. Browser fixtures drive that request through the login
 * page's first-time initialization form, exactly as a user would.
 */

export const bootstrapOwnerUsername = 'owner'
export const bootstrapOwnerPassword = 'browser-owner-password-1'

/**
 * Fills the login page's first-time initialization form and submits it.
 * Returns the proof input's value after the submission (the form clears the
 * proof and the password before the await, so this is an empty string).
 */
export function submitLoginInitialization(document, proof) {
  const proofInput = document.querySelector('.wwc-login-initialization-proof')
  const usernameInput = document.querySelector('.wwc-login-initialization-username')
  const passwordInput = document.querySelector('.wwc-login-initialization-password')
  const form = document.querySelector('.wwc-login-initialization-form')
  if (
    proofInput === null || usernameInput === null
    || passwordInput === null || form === null
  ) {
    throw new Error('the login page initialization form is not mounted')
  }
  usernameInput.value = bootstrapOwnerUsername
  passwordInput.value = bootstrapOwnerPassword
  proofInput.value = proof
  form.requestSubmit()
  return proofInput.value
}

/** The login page renders initialization rejections on this element. */
export function loginErrorNode(document) {
  return document.querySelector('.wwc-login-error')
}

/**
 * Fills the login page's username + password sign-in form and submits it.
 * Returns the password input's value after the submission (the form clears
 * the password before the await, so this is an empty string).
 */
export function submitOwnerSignIn(document) {
  const usernameInput = document.querySelector('.wwc-login-username')
  const passwordInput = document.querySelector('.wwc-login-password')
  const form = document.querySelector('.wwc-login-form')
  if (usernameInput === null || passwordInput === null || form === null) {
    throw new Error('the login page sign-in form is not mounted')
  }
  usernameInput.value = bootstrapOwnerUsername
  passwordInput.value = bootstrapOwnerPassword
  form.requestSubmit()
  return passwordInput.value
}
