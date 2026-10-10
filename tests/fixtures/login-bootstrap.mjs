export const bootstrapOwnerUsername = 'owner'
export const bootstrapOwnerPassword = 'browser-owner-password-1'

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

export function loginErrorNode(document) {
  return document.querySelector('.wwc-login-error')
}

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
