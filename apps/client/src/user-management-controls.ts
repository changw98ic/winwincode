// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneUserSummary } from './control-plane-client.js'
import {
  type UserResetFailure,
  type UserStateAction,
  type UserStateFailure,
  type UserManagementViewModel,
} from './user-management-view-model.js'

export interface UserRowControlsOptions {
  readonly document: Document
  /** The row action area that receives the state and reset buttons. */
  readonly actions: HTMLElement
  /** The row notice area that receives the confirmation and failure copy. */
  readonly notice: HTMLElement
  readonly model: UserManagementViewModel
}

export interface UserRowControls {
  /** Re-render the row controls for the row's current account. */
  update(user: ControlPlaneUserSummary): void
  close(): void
}

/** Why each account state change is consequential, and what confirming means. */
const STATE_CONFIRM_COPY: Readonly<Record<UserStateAction, string>> = Object.freeze({
  disable: 'Disabling signs this user out everywhere and blocks further sign-in.',
  enable: 'Enabling restores this account\'s sign-in immediately.',
})

const STATE_CONFIRM_ACCEPT_TEXT: Readonly<Record<UserStateAction, string>> = Object.freeze({
  disable: 'Disable user',
  enable: 'Enable user',
})

/**
 * The one copy per row failure; every entry also reaches the screen reader
 * through the alert role of the failure line.
 */
function stateFailureText(failure: UserStateFailure): string {
  switch (failure) {
    case 'wrong-state': return 'The account already changed state. Retry to confirm.'
    case 'revision-conflict': return 'The account changed while you worked. Retry on the current state.'
    case 'user-not-found': return 'This account no longer exists.'
    case 'permission-denied': return 'Only the Owner can manage users.'
    case 'authentication-required': return 'Sign in again to continue managing users.'
    case 'current-password-wrong': return 'The current password is wrong. Check it and try again.'
    case 'username-conflict': return 'That username already belongs to another account.'
    case 'invalid-request': return 'The request was rejected as invalid. Check the form and try again.'
    case 'unavailable': return 'The request did not go through. Check the connection and try again.'
  }
}

function resetFailureText(failure: UserResetFailure): string {
  if (failure === 'password-shape') {
    return 'Enter the passwords before submitting.'
  }
  return stateFailureText(failure)
}

/**
 * Mount the action controls of one user row: the disable/enable entry with
 * its explicit confirmation, and the Owner password reset entry. The module
 * owns DOM and ARIA only; every click translates into one view-model intent.
 */
export function mountUserRowControls(options: UserRowControlsOptions): UserRowControls {
  const document = options.document
  let currentUserId = ''

  const stateButton = document.createElement('button')
  stateButton.className = 'wwc-users-row-state-button wwc-users-row-danger'
  stateButton.type = 'button'

  const resetButton = document.createElement('button')
  resetButton.className = 'wwc-users-row-reset'
  resetButton.type = 'button'
  resetButton.textContent = 'Reset password'

  const confirm = document.createElement('div')
  confirm.className = 'wwc-users-row-confirm'
  confirm.hidden = true

  const confirmText = document.createElement('p')
  confirmText.className = 'wwc-users-row-confirm-text'

  const confirmAccept = document.createElement('button')
  confirmAccept.className = 'wwc-users-row-confirm-accept wwc-users-row-danger'
  confirmAccept.type = 'button'

  const confirmKeep = document.createElement('button')
  confirmKeep.className = 'wwc-users-row-confirm-keep'
  confirmKeep.type = 'button'
  confirmKeep.textContent = 'Keep unchanged'

  const failure = document.createElement('p')
  failure.className = 'wwc-users-row-error'
  failure.setAttribute('role', 'alert')
  failure.hidden = true

  confirm.append(confirmText, confirmAccept, confirmKeep)
  options.actions.append(stateButton, resetButton)
  options.notice.append(confirm, failure)

  const onStateAction = () => {
    const interaction = options.model.rowInteraction(currentUserId)
    if (interaction.kind === 'rest') {
      // The row's durable state names the offered direction.
      const action = userStateAction(options.model, currentUserId)
      if (action === null) return
      options.model.requestStateChange(currentUserId, action)
      return
    }
    // Confirming and failed drafts both submit through the explicit accept.
    options.model.confirmStateChange(currentUserId)
  }
  const onReset = () => {
    options.model.requestOwnerReset(currentUserId)
  }
  const onConfirmAccept = () => {
    options.model.confirmStateChange(currentUserId)
  }
  const onConfirmKeep = () => {
    options.model.dismissStateChange(currentUserId)
  }

  stateButton.addEventListener('click', onStateAction)
  resetButton.addEventListener('click', onReset)
  confirmAccept.addEventListener('click', onConfirmAccept)
  confirmKeep.addEventListener('click', onConfirmKeep)

  return {
    update(user) {
      currentUserId = user.userId
      const interaction = options.model.rowInteraction(user.userId)
      const busy = interaction.kind === 'submitting'
      const busyAction = busy ? interaction.action : null
      options.actions.setAttribute('aria-busy', busy ? 'true' : 'false')

      const action = userStateAction(options.model, user.userId)
      const stateApplies = action !== null
      const label = action === 'disable' ? 'Disable user' : 'Enable user'
      stateButton.textContent = busyAction === null ? label : (
        busyAction === 'disable' ? 'Disabling…' : 'Enabling…'
      )
      stateButton.disabled = busy || !stateApplies

      const resetInteraction = options.model.resetInteraction(user.userId)
      const resetBusy = resetInteraction.kind === 'submitting'
      resetButton.textContent = resetBusy ? 'Resetting…' : 'Reset password'
      resetButton.disabled = resetBusy

      // A failed dangerous action keeps its armed confirmation: the draft
      // survives so the same explicit accept can retry the request.
      const armedAction = interaction.kind === 'confirming'
        ? interaction.action
        : (interaction.kind === 'failed' ? interaction.action : null)
      if (armedAction === null) {
        confirm.hidden = true
        confirmText.textContent = ''
        confirmAccept.textContent = ''
      } else {
        confirm.hidden = false
        confirmText.textContent = STATE_CONFIRM_COPY[armedAction]
        confirmAccept.textContent = STATE_CONFIRM_ACCEPT_TEXT[armedAction]
      }
      if (interaction.kind === 'failed') {
        failure.hidden = false
        failure.textContent = stateFailureText(interaction.failure)
      } else if (resetInteraction.kind === 'failed') {
        failure.hidden = false
        failure.textContent = resetFailureText(resetInteraction.failure)
      } else {
        failure.hidden = true
        failure.textContent = ''
      }
    },
    close() {
      stateButton.removeEventListener('click', onStateAction)
      resetButton.removeEventListener('click', onReset)
      confirmAccept.removeEventListener('click', onConfirmAccept)
      confirmKeep.removeEventListener('click', onConfirmKeep)
      stateButton.remove()
      resetButton.remove()
      confirm.remove()
      failure.remove()
    },
  }
}

/** The state change the row's durable state supports; null when none does. */
function userStateAction(
  model: UserManagementViewModel,
  userId: string,
): UserStateAction | null {
  const user = model.state.users.find(candidate => candidate.userId === userId)
  if (user === undefined) return null
  return user.state === 'active' ? 'disable' : 'enable'
}
