// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneDeviceSummary } from './community-control-plane-client.js'
import {
  deviceRecoveryDeadlineText,
  deviceSupportsCancelAndRelease,
  deviceSupportsClaim,
  deviceSupportsForceRelease,
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
  /** Epoch-milliseconds clock for the recovery-deadline text; defaults to now. */
  readonly now?: () => number
}

export interface ClientOccupancyControls {
  /** Re-render the occupancy controls for the card's current device. */
  update(device: ControlPlaneDeviceSummary): void
  close(): void
}

/** Why the destructive entry frees the device, and what confirming means. */
const DANGER_COPY: Readonly<Record<ClientOccupancyDangerAction, string>> = Object.freeze({
  release: '释放后将停止接收新任务，并在当前任务完成后解除占用。',
  'cancel-and-release': '停止后将取消正在运行的任务，并立即释放设备。',
  'force-release': '强制释放会立即结束中断的占用，使设备可以再次被占用。',
})

const CONFIRM_ACCEPT_TEXT: Readonly<Record<ClientOccupancyDangerAction, string>> = Object.freeze({
  release: '释放设备',
  'cancel-and-release': '取消任务并释放',
  'force-release': '强制释放',
})

/**
 * The one copy per occupancy failure; every entry also reaches the screen
 * reader through the alert role of the failure line. The failure union is the
 * card's translation of the facade's stable error categories, so no raw
 * server text and no other user's identity can reach the card.
 */
function failureText(failure: ClientOccupancyFailure): string {
  switch (failure) {
    case 'occupied-by-other': return '其他用户已先占用该设备。'
    case 'not-holder': return '你已不再占用该设备。'
    case 'device-gone': return '该设备已不存在。'
    case 'device-offline': return '该设备当前离线。'
    case 'device-locked': return '该设备已锁定。'
    case 'connections-forbidden': return '该设备不再接受新连接。'
    case 'account-denied': return '当前账号无权使用该设备。'
    case 'capacity-exhausted': return '该设备没有剩余容量。'
    case 'device-rejected': return '该设备拒绝了连接请求。'
    case 'ack-timeout': return '该设备未及时确认连接。'
    case 'recovery-pending': return '该设备正在等待恢复，请恢复后重试。'
    case 'permission-denied': return '只有设备所有者可以强制释放该设备。'
    case 'confirmation-required': return '释放设备需要明确确认，请重试。'
    case 'state-changed': return '请求生效前占用状态已发生变化。'
    case 'rate-limited': return '尝试次数过多，请稍后再试。'
    case 'unavailable': return '请求失败，请检查连接后重试。'
  }
}

/**
 * Mount the occupancy controls of one device card: the connect entry, the
 * holder's release and cancel-and-release entries, the Owner's force-release
 * entry for a recovery-pending device, and the explicit confirmation, recovery
 * deadline, and failure copy for the dangerous paths. The module owns DOM and
 * ARIA only; every click translates into one view-model intent.
 */
export function mountClientOccupancyControls(
  options: ClientOccupancyControlsOptions,
): ClientOccupancyControls {
  const document = options.document
  const nowMillis = options.now ?? Date.now
  let currentClientId = ''

  const connect = document.createElement('button')
  connect.className = 'wwc-clients-card-connect'
  connect.type = 'button'
  connect.textContent = '连接'

  const release = document.createElement('button')
  release.className = 'wwc-clients-card-release wwc-clients-card-danger'
  release.type = 'button'
  release.textContent = '释放'

  const cancel = document.createElement('button')
  cancel.className = 'wwc-clients-card-cancel-release wwc-clients-card-danger'
  cancel.type = 'button'
  cancel.textContent = '取消任务并释放'

  const force = document.createElement('button')
  force.className = 'wwc-clients-card-force-release wwc-clients-card-danger'
  force.type = 'button'
  force.textContent = '强制释放'

  // UI-100.3: the §12.4 recovery window of an interrupted lease. A plain
  // paragraph keeps the card's one alert channel reserved for real failures.
  const recovery = document.createElement('p')
  recovery.className = 'wwc-clients-card-recovery'
  recovery.hidden = true

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
  confirmKeep.textContent = '保持占用'

  const failure = document.createElement('p')
  failure.className = 'wwc-clients-card-error'
  failure.setAttribute('role', 'alert')
  failure.hidden = true

  confirm.append(confirmText, confirmAccept, confirmKeep)
  options.actions.append(connect, release, cancel, force)
  options.notice.append(recovery, confirm, failure)

  const onConnect = () => {
    options.model.requestClaim(currentClientId)
  }
  const onRelease = () => {
    options.model.requestRelease(currentClientId)
  }
  const onCancel = () => {
    options.model.requestCancelAndRelease(currentClientId)
  }
  const onForce = () => {
    options.model.requestForceRelease(currentClientId)
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
  force.addEventListener('click', onForce)
  confirmAccept.addEventListener('click', onConfirmAccept)
  confirmKeep.addEventListener('click', onConfirmKeep)

  return {
    update(device) {
      currentClientId = device.clientId
      const interaction = options.model.interaction(device.clientId)
      const busy = interaction.kind === 'submitting'
      const busyAction = busy ? interaction.action : null
      options.actions.setAttribute('aria-busy', busy ? 'true' : 'false')

      connect.textContent = busyAction === 'claim' ? '正在连接…' : '连接'
      connect.disabled = busy || !deviceSupportsClaim(device)

      const releaseApplies = deviceSupportsRelease(device)
      release.hidden = !releaseApplies
      release.textContent = busyAction === 'release' ? '正在释放…' : '释放'
      release.disabled = busy || !releaseApplies

      const cancelApplies = deviceSupportsCancelAndRelease(device)
      cancel.hidden = !cancelApplies
      cancel.textContent = busyAction === 'cancel-and-release' ? '正在停止…' : '取消任务并释放'
      cancel.disabled = busy || !cancelApplies

      // UI-100.3: the Owner force-release entry exists only for an interrupted
      // lease, and only when the composed facade exposes the seam. The Server
      // stays the one authority on who an Owner is; a denial keeps the entry
      // with honest copy instead of second-guessing the Server here.
      const forceApplies = deviceSupportsForceRelease(device)
        && options.model.supportsForceRelease()
      force.hidden = !forceApplies
      force.textContent = busyAction === 'force-release' ? '正在释放…' : '强制释放'
      force.disabled = busy || !forceApplies

      if (device.occupancy === 'recovery-pending') {
        recovery.hidden = false
        recovery.textContent = deviceRecoveryDeadlineText(device, nowMillis())
      } else {
        recovery.hidden = true
        recovery.textContent = ''
      }

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
      force.removeEventListener('click', onForce)
      confirmAccept.removeEventListener('click', onConfirmAccept)
      confirmKeep.removeEventListener('click', onConfirmKeep)
      connect.remove()
      release.remove()
      cancel.remove()
      force.remove()
      recovery.remove()
      confirm.remove()
      failure.remove()
    },
  }
}
