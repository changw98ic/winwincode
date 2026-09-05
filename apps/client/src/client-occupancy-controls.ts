// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneDeviceSummary } from './control-plane-client.js'
import {
  deviceSupportsCancelAndRelease,
  deviceSupportsClaim,
  deviceSupportsRelease,
  type ClientOccupancyDangerAction,
  type ClientOccupancyFailure,
  type ClientOccupancyViewModel,
} from './client-occupancy-view-model.js'

export interface ClientOccupancyControlsOptions {
  readonly document: Document
  /** The card action row that receives the occupancy buttons. */
  readonly actions: HTMLElement
  /** The card notice row that receives the confirmation and failure copy. */
  readonly notice: HTMLElement
  readonly model: ClientOccupancyViewModel
}

export interface ClientOccupancyControls {
  /** Re-render the occupancy controls for the card's current device. */
  update(device: ControlPlaneDeviceSummary): void
  close(): void
}

/** Why releasing now drains first, and what confirming means. */
const DANGER_COPY: Readonly<Record<ClientOccupancyDangerAction, string>> = Object.freeze({
  release: 'Releasing now stops new tasks and lets the running tasks finish before the device frees.',
  'cancel-and-release': 'Stopping now cancels the running tasks and frees the device immediately.',
})

const CONFIRM_ACCEPT_TEXT: Readonly<Record<ClientOccupancyDangerAction, string>> = Object.freeze({
  release: 'Release device',
  'cancel-and-release': 'Cancel tasks and release',
})

/**
 * The one copy per occupancy failure; every entry also reaches the screen
 * reader through the alert role of the failure line.
 */
function failureText(failure: ClientOccupancyFailure): string {
  switch (failure) {
    case 'occupied-by-other': return 'Another user claimed the device first.'
    case 'not-holder': return 'You no longer hold this device.'
    case 'device-offline': return 'The device is offline right now.'
    case 'device-locked': return 'The device is locked.'
    case 'recovery-pending': return 'The device is waiting to recover. Try again after it recovers.'
    case 'rate-limited': return 'Too many attempts. Wait a moment, then try again.'
    case 'unavailable': return 'The request did not go through. Check the connection and try again.'
  }
}

/**
 * Mount the occupancy controls of one device card: the connect entry, the
 * holder's release and cancel-and-release entries, and the explicit
 * confirmation plus failure copy for the dangerous paths. The module owns
 * DOM and ARIA only; every click translates into one view-model intent.
 */
export function mountClientOccupancyControls(
  options: ClientOccupancyControlsOptions,
): ClientOccupancyControls {
  const document = options.document
  let currentClientId = ''

  const connect = document.createElement('button')
  connect.className = 'wwc-clients-card-connect'
  connect.type = 'button'
  connect.textContent = 'Connect'

  const release = document.createElement('button')
  release.className = 'wwc-clients-card-release wwc-clients-card-danger'
  release.type = 'button'
  release.textContent = 'Release'

  const cancel = document.createElement('button')
  cancel.className = 'wwc-clients-card-cancel-release wwc-clients-card-danger'
  cancel.type = 'button'
  cancel.textContent = 'Cancel and release'

  const confirm = document.createElement('div')
  confirm.className = 'wwc-clients-card-confirm'
  confirm.hidden = true

  const confirmText = document.createElement('p')
  confirmText.className = 'wwc-clients-card-confirm-text'

  const confirmAccept = document.createElement('button')
  confirmAccept.className = 'wwc-clients-card-confirm-accept wwc-clients-card-danger'
  confirmAccept.type = 'button'

  const confirmKeep = document.createElement('button')
  confirmKeep.className = 'wwc-clients-card-confirm-keep'
  confirmKeep.type = 'button'
  confirmKeep.textContent = 'Keep occupancy'

  const failure = document.createElement('p')
  failure.className = 'wwc-clients-card-error'
  failure.setAttribute('role', 'alert')
  failure.hidden = true

  confirm.append(confirmText, confirmAccept, confirmKeep)
  options.actions.append(connect, release, cancel)
  options.notice.append(confirm, failure)

  const onConnect = () => {
    options.model.requestClaim(currentClientId)
  }
  const onRelease = () => {
    options.model.requestRelease(currentClientId)
  }
  const onCancel = () => {
    options.model.requestCancelAndRelease(currentClientId)
  }
  const onConfirmAccept = () => {
    options.model.confirmPending(currentClientId)
  }
  const onConfirmKeep = () => {
    options.model.dismiss(currentClientId)
  }

  connect.addEventListener('click', onConnect)
  release.addEventListener('click', onRelease)
  cancel.addEventListener('click', onCancel)
  confirmAccept.addEventListener('click', onConfirmAccept)
  confirmKeep.addEventListener('click', onConfirmKeep)

  return {
    update(device) {
      currentClientId = device.clientId
      const interaction = options.model.interaction(device.clientId)
      const busy = interaction.kind === 'submitting'
      const busyAction = busy ? interaction.action : null
      options.actions.setAttribute('aria-busy', busy ? 'true' : 'false')

      connect.textContent = busyAction === 'claim' ? 'Connecting…' : 'Connect'
      connect.disabled = busy || !deviceSupportsClaim(device)

      const releaseApplies = deviceSupportsRelease(device)
      release.hidden = !releaseApplies
      release.textContent = busyAction === 'release' ? 'Releasing…' : 'Release'
      release.disabled = busy || !releaseApplies

      const cancelApplies = deviceSupportsCancelAndRelease(device)
      cancel.hidden = !cancelApplies
      cancel.textContent = busyAction === 'cancel-and-release' ? 'Stopping…' : 'Cancel and release'
      cancel.disabled = busy || !cancelApplies

      const failureLine = interaction.kind === 'failed' ? interaction.failure : null
      // A failed dangerous action keeps its armed confirmation: the draft
      // survives so the same explicit accept can retry the request.
      const armedAction = interaction.kind === 'confirming'
        ? interaction.action
        : (interaction.kind === 'failed' && interaction.action !== 'claim'
          ? interaction.action
          : null)
      if (armedAction === null) {
        confirm.hidden = true
        confirmText.textContent = ''
        confirmAccept.textContent = ''
      } else {
        confirm.hidden = false
        confirmText.textContent = DANGER_COPY[armedAction]
        confirmAccept.textContent = CONFIRM_ACCEPT_TEXT[armedAction]
      }
      if (failureLine === null) {
        failure.hidden = true
        failure.textContent = ''
      } else {
        failure.hidden = false
        failure.textContent = failureText(failureLine)
      }
    },
    close() {
      connect.removeEventListener('click', onConnect)
      release.removeEventListener('click', onRelease)
      cancel.removeEventListener('click', onCancel)
      confirmAccept.removeEventListener('click', onConfirmAccept)
      confirmKeep.removeEventListener('click', onConfirmKeep)
      connect.remove()
      release.remove()
      cancel.remove()
      confirm.remove()
      failure.remove()
    },
  }
}
