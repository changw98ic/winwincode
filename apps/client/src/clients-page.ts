// SPDX-License-Identifier: Apache-2.0

import { mountKeyedCollection } from './components/keyed-collection.js'
import type { ControlPlaneDeviceSummary } from './community-control-plane-client.js'
import {
  mountClientOccupancyControls,
  type ClientOccupancyControls,
} from './client-occupancy-controls.js'
import type { ClientOccupancyViewModel } from './client-occupancy-view-model.js'
import type {
  ClientsAddFailure,
  ClientsViewModel,
  ClientsViewModelState,
} from './clients-view-model.js'
import {
  devicePresenceText,
  deviceStateText,
  deviceStateTone,
  relativeHeartbeatText,
} from './clients-view-model.js'

export interface ClientsPageOptions {
  readonly root: HTMLElement
  readonly model: ClientsViewModel
  readonly occupancy?: ClientOccupancyViewModel
  readonly now?: () => number
  readonly onDeviceSelect?: (clientId: string | null) => void
}

export interface ClientsPage {
  close(): void
  setVisible(visible: boolean): void
}

interface ClientCardRefs {
  readonly card: HTMLElement
  readonly clientId: string
  readonly name: HTMLElement
  readonly presence: HTMLElement
  readonly stateText: HTMLElement
  readonly capacity: HTMLElement
  readonly heartbeat: HTMLElement
  readonly version: HTMLElement
  readonly actions: HTMLElement
  readonly notice: HTMLElement
  readonly occupancy: ClientOccupancyControls | null
  readonly select: HTMLButtonElement
  readonly revoke: HTMLButtonElement
  readonly revokeConfirmation: HTMLElement
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

function failureText(failure: ClientsAddFailure): string {
  switch (failure) {
    case 'invalid-client-id': return '输入设备上显示的 9-12 位设备 ID。'
    case 'invalid-connection-code': return '输入设备上显示的 8 位连接码。'
    case 'id-not-found': return '没有设备使用这个 ID。请核对设备上显示的 ID。'
    case 'client-offline': return '该设备当前离线，等它上线后再连接。'
    case 'code-invalid': return '连接码不正确，请在设备上核对后重试。'
    case 'code-expired': return '连接码已过期，请在设备上重新生成后重试。'
    case 'new-connections-forbidden': return '该设备不再接受新的连接。'
    case 'client-locked': return '该设备已锁定，请先在设备上解锁。'
    case 'rate-limited': return '连接尝试过于频繁，请稍后再试。'
    case 'unavailable': return '暂时无法添加设备，请检查连接后重试。'
  }
}

function statusText(state: ClientsViewModelState): string {
  if (state.status === 'submitting') return '正在连接设备…'
  if (state.status === 'succeeded') return '设备已接入。'
  return ''
}

function groupClientId(value: string): string {
  const digits = value.replace(/\D+/gu, '').slice(0, 12)
  return digits.replace(/(\d{4})(?=\d)/gu, '$1 ')
}

export function mountClientsPage(options: ClientsPageOptions): ClientsPage {
  const document = options.root.ownerDocument
  const now = options.now ?? (() => Date.now())
  const region = element(document, 'section', 'wwc-clients')
  const heading = element(document, 'p', 'wwc-clients-heading')
  const status = element(document, 'p', 'wwc-clients-status')
  const error = element(document, 'p', 'wwc-clients-error')
  const form = element(document, 'form', 'wwc-clients-add-form')
  const pairing = element(document, 'section', 'wwc-clients-pairing')
  const pairingHeading = element(document, 'h2', 'wwc-clients-add-heading')
  const pairingHint = element(document, 'p', 'wwc-clients-pairing-hint')
  const pair = element(document, 'button', 'wwc-clients-pair')
  const pairingDetails = element(document, 'div', 'wwc-clients-pairing-details')
  const pairingExpiry = element(document, 'p', 'wwc-clients-pairing-expiry')
  const pairingCommand = element(document, 'textarea', 'wwc-clients-control wwc-clients-pairing-command')
  const managementStatus = element(document, 'p', 'wwc-clients-management-status')
  const refreshDevices = element(document, 'button', 'wwc-clients-refresh')
  const formHeading = element(document, 'p', 'wwc-clients-add-heading')
  const idLabel = element(document, 'label', 'wwc-clients-label')
  const idInput = element(document, 'input', 'wwc-clients-control wwc-clients-id-input')
  const codeLabel = element(document, 'label', 'wwc-clients-label')
  const codeInput = element(document, 'input', 'wwc-clients-control wwc-clients-code-input')
  const submit = element(document, 'button', 'wwc-clients-add-submit')
  const empty = element(document, 'p', 'wwc-clients-empty')
  const list = element(document, 'div', 'wwc-clients-list')
  const cards = new WeakMap<HTMLElement, ClientCardRefs>()
  let closed = false
  let clearedAfterSuccess = false
  let selectedClientId: string | null = null
  let latestDevices: readonly ControlPlaneDeviceSummary[] = []
  let latestSnapshot = options.model.state

  region.setAttribute('aria-label', '执行设备')
  region.hidden = true
  heading.textContent = '执行设备'
  heading.id = 'wwc-clients-heading'
  region.setAttribute('aria-labelledby', heading.id)
  status.hidden = true
  error.setAttribute('role', 'alert')
  error.hidden = true
  error.id = 'wwc-clients-error'

  pairingHeading.textContent = '配对远程执行设备'
  pairingHint.textContent = '由 Owner 生成配对令牌。在执行任务的电脑上运行绑定命令，再启动 Device。'
  pair.type = 'button'
  pair.textContent = '生成配对令牌'
  pair.addEventListener('click', () => { void options.model.createDevicePairing() })
  pairingCommand.readOnly = true
  pairingCommand.rows = 5
  pairingCommand.setAttribute('aria-label', '远程设备绑定命令')
  const commandHint = element(document, 'p', 'wwc-clients-pairing-command-hint')
  commandHint.textContent = '将 /ABS/ca.pem 替换为该电脑上的 Server CA 证书绝对路径。令牌仅用于初次绑定。'
  pairingDetails.append(pairingExpiry, pairingCommand, commandHint)
  pairingDetails.hidden = true
  managementStatus.setAttribute('role', 'status')
  refreshDevices.type = 'button'
  refreshDevices.textContent = '刷新设备列表'
  refreshDevices.addEventListener('click', () => { void options.model.refresh() })
  pairing.append(pairingHeading, pairingHint, pair, pairingDetails, managementStatus, refreshDevices)

  formHeading.textContent = '接入执行设备'
  formHeading.id = 'wwc-clients-add-heading'
  form.setAttribute('aria-labelledby', formHeading.id)

  idInput.id = 'wwc-clients-id-input'
  idInput.name = 'clientId'
  idInput.type = 'text'
  idInput.autocomplete = 'off'
  idInput.spellcheck = false
  idInput.setAttribute('autocapitalize', 'none')
  idInput.setAttribute('inputmode', 'numeric')
  idInput.maxLength = 14
  idInput.required = true
  idLabel.htmlFor = idInput.id
  idLabel.textContent = '设备 ID'

  codeInput.id = 'wwc-clients-code-input'
  codeInput.name = 'connectionCode'
  codeInput.type = 'text'
  codeInput.autocomplete = 'one-time-code'
  codeInput.spellcheck = false
  codeInput.setAttribute('autocapitalize', 'none')
  codeInput.setAttribute('inputmode', 'numeric')
  codeInput.maxLength = 8
  codeInput.required = true
  codeLabel.htmlFor = codeInput.id
  codeLabel.textContent = '连接码'

  submit.type = 'submit'
  submit.textContent = '连接'
  form.append(formHeading, idLabel, idInput, codeLabel, codeInput, submit)

  empty.textContent = '还没有执行设备。先在上方连接你的设备。'
  empty.hidden = true

  region.append(heading, pairing, form, status, error, empty, list)
  options.root.replaceChildren(region)

  const cardCollection = mountKeyedCollection<ControlPlaneDeviceSummary, string, HTMLElement>({
    parent: list,
    key: device => device.clientId,
    create(device) {
      const card = element(document, 'article', 'wwc-clients-card')
      const name = element(document, 'p', 'wwc-clients-card-name')
      const presence = element(document, 'span', 'wwc-clients-card-presence')
      const stateText = element(document, 'p', 'wwc-clients-card-state')
      const capacity = element(document, 'p', 'wwc-clients-card-capacity')
      const heartbeat = element(document, 'p', 'wwc-clients-card-heartbeat')
      const version = element(document, 'p', 'wwc-clients-card-version')
      const actions = element(document, 'div', 'wwc-clients-card-actions')
      const notice = element(document, 'div', 'wwc-clients-card-notice')
      const occupancy = options.occupancy === undefined
        ? null
        : mountClientOccupancyControls({
          document,
          actions,
          notice,
          model: options.occupancy,
          now,
        })
      const select = element(document, 'button', 'wwc-clients-card-select')
      select.type = 'button'
      select.textContent = '仓库'
      select.hidden = options.onDeviceSelect === undefined
      select.setAttribute('aria-pressed', 'false')
      select.addEventListener('click', () => {
        selectedClientId = selectedClientId === device.clientId ? null : device.clientId
        applySelection()
        options.onDeviceSelect?.(selectedClientId)
      })
      actions.append(select)
      const revoke = element(document, 'button', 'wwc-clients-card-revoke')
      revoke.type = 'button'
      revoke.textContent = '永久吊销此设备'
      const revokeConfirmation = element(document, 'div', 'wwc-clients-revoke-confirmation')
      revokeConfirmation.hidden = true
      const revokeHint = element(document, 'p', 'wwc-clients-revoke-hint')
      revokeHint.textContent = '吊销后，该设备无法继续连接 Server。重新接入需要 Owner 生成新的配对令牌。'
      const confirmRevoke = element(document, 'button', 'wwc-clients-confirm-revoke')
      confirmRevoke.type = 'button'
      confirmRevoke.textContent = '确认永久吊销'
      const cancelRevoke = element(document, 'button', 'wwc-clients-cancel-revoke')
      cancelRevoke.type = 'button'
      cancelRevoke.textContent = '取消'
      let pendingRevokeNodeId: string | null = null
      revoke.addEventListener('click', () => {
        pendingRevokeNodeId = latestDevices.find(item => item.clientId === device.clientId)?.clientNodeId ?? null
        revokeConfirmation.hidden = pendingRevokeNodeId === null
      })
      cancelRevoke.addEventListener('click', () => { revokeConfirmation.hidden = true })
      confirmRevoke.addEventListener('click', () => {
        const current = latestDevices.find(item => item.clientId === device.clientId)
        revokeConfirmation.hidden = true
        if (current?.clientNodeId !== pendingRevokeNodeId || pendingRevokeNodeId === null) return
        void options.model.revokeDevice(pendingRevokeNodeId)
      })
      revokeConfirmation.append(revokeHint, confirmRevoke, cancelRevoke)
      actions.append(revoke, revokeConfirmation)
      card.append(name, presence, stateText, capacity, heartbeat, version, actions, notice)
      const refs: ClientCardRefs = {
        card, clientId: device.clientId, name, presence, stateText,
        capacity, heartbeat, version, actions, notice, occupancy, select, revoke, revokeConfirmation,
      }
      cards.set(card, refs)
      updateCard(refs, device)
      return card
    },
    update(node, device) {
      const refs = cards.get(node)
      if (refs === undefined) return
      updateCard(refs, device)
    },
    remove(node) {
      const refs = cards.get(node)
      refs?.occupancy?.close()
    },
  })

  function updateCard(refs: ClientCardRefs, device: ControlPlaneDeviceSummary): void {
    refs.card.setAttribute('aria-label', `${device.displayName}: ${deviceStateText(device)}`)
    refs.name.textContent = device.displayName
    refs.presence.textContent = devicePresenceText(device)
    refs.presence.dataset.tone = deviceStateTone(device)
    refs.stateText.textContent = deviceStateText(device)
    refs.capacity.textContent = `容量 ${device.capacityUsed} / ${device.capacityTotal}`
    refs.heartbeat.textContent = relativeHeartbeatText(device.lastHeartbeatAt, now())
    refs.version.textContent = `版本 ${device.version}`
    const revoked = device.clientNodeId !== undefined
      && latestSnapshot.revokedNodeIds.includes(device.clientNodeId)
    refs.occupancy?.update(revoked ? { ...device, presence: 'locked' } : device)
    refs.select.disabled = revoked
    refs.revoke.hidden = device.clientNodeId === undefined
    refs.revoke.disabled = latestSnapshot.revokingNodeId !== null || revoked
    refs.revoke.textContent = revoked ? '已永久吊销' : '永久吊销此设备'
    if (revoked) {
      refs.revokeConfirmation.hidden = true
      refs.stateText.textContent = '设备已永久吊销'
      refs.presence.textContent = '已吊销'
      refs.presence.dataset.tone = 'danger'
      refs.card.setAttribute('aria-label', `${device.displayName}: 设备已永久吊销`)
    }
  }

  function applySelection(): void {
    for (const node of list.children) {
      const refs = cards.get(node as HTMLElement)
      if (refs === undefined) continue
      refs.select.setAttribute(
        'aria-pressed',
        refs.clientId === selectedClientId ? 'true' : 'false',
      )
    }
  }

  function setFieldError(control: HTMLInputElement, hasError: boolean): void {
    if (hasError) {
      control.setAttribute('aria-invalid', 'true')
      control.setAttribute('aria-describedby', error.id)
    } else {
      control.removeAttribute('aria-invalid')
      control.removeAttribute('aria-describedby')
    }
  }

  function render(snapshot: ClientsViewModelState): void {
    if (closed) return
    latestSnapshot = snapshot
    pair.disabled = snapshot.pairingStatus === 'creating'
    pair.textContent = snapshot.pairingStatus === 'creating' ? '正在生成…' : '生成配对令牌'
    pairingDetails.hidden = snapshot.pairing === null
    pairingExpiry.textContent = snapshot.pairing === null ? ''
      : `令牌有效期至 ${new Date(snapshot.pairing.expiresAt).toLocaleString('zh-CN')}`
    const quote = (value: string) => `'${value.replace(/'/gu, `'\\''`)}'`
    pairingCommand.value = snapshot.pairing === null ? ''
      : `WWC_DEVICE_PAIRING_TOKEN=${quote(snapshot.pairing.pairingToken)} wwc device bind --server-url ${quote(options.model.serverUrl ?? 'https://HOST')} --ca-file /ABS/ca.pem\nwwc device serve`
    managementStatus.textContent = snapshot.managementMessage
    managementStatus.hidden = snapshot.managementMessage.length === 0
    refreshDevices.disabled = snapshot.devicesStatus === 'loading'
    const busy = snapshot.status === 'submitting'
    if (snapshot.status === 'succeeded' && !clearedAfterSuccess) {
      clearedAfterSuccess = true
      idInput.value = ''
      codeInput.value = ''
    }
    if (snapshot.status !== 'succeeded') clearedAfterSuccess = false
    const nextStatus = statusText(snapshot)
    status.textContent = nextStatus
    status.hidden = nextStatus.length === 0
    const nextFailure = snapshot.status === 'idle' && snapshot.failure !== null
      ? failureText(snapshot.failure)
      : null
    error.textContent = nextFailure ?? ''
    error.hidden = nextFailure === null
    const failureKind = snapshot.status === 'idle' ? snapshot.failure : null
    const idHasError = failureKind !== null && failureKind !== 'invalid-connection-code'
    const codeHasError = failureKind !== null && failureKind !== 'invalid-client-id'
    setFieldError(idInput, idHasError)
    setFieldError(codeInput, codeHasError)
    idInput.disabled = busy
    codeInput.disabled = busy
    submit.disabled = busy
    form.setAttribute('aria-busy', busy ? 'true' : 'false')
    latestDevices = snapshot.devices
    cardCollection.update(snapshot.devices)
    if (
      selectedClientId !== null
      && !snapshot.devices.some(device => device.clientId === selectedClientId)
    ) {
      selectedClientId = null
      options.onDeviceSelect?.(null)
    }
    applySelection()
    empty.hidden = snapshot.devices.length !== 0 || snapshot.devicesStatus === 'loading'
  }

  function clearErrorDraft(): void {
    options.model.dismissFailure()
  }

  const onConnect = (event: SubmitEvent) => {
    event.preventDefault()
    if (options.model.state.status === 'submitting') return
    const submittedClientId = idInput.value
    const submittedConnectionCode = codeInput.value
    codeInput.value = ''
    void options.model.addClient({
      clientId: submittedClientId,
      connectionCode: submittedConnectionCode,
    })
  }
  const onIdEdit = () => {
    const grouped = groupClientId(idInput.value)
    if (grouped !== idInput.value) idInput.value = grouped
    clearErrorDraft()
  }
  const onCodeEdit = () => {
    const digits = codeInput.value.replace(/\D+/gu, '').slice(0, 8)
    if (digits !== codeInput.value) codeInput.value = digits
    clearErrorDraft()
  }

  form.addEventListener('submit', onConnect)
  idInput.addEventListener('input', onIdEdit)
  codeInput.addEventListener('input', onCodeEdit)

  const unsubscribe = options.model.subscribe(render)
  const unsubscribeOccupancy = options.occupancy?.subscribe(() => {
    if (closed) return
    cardCollection.update(latestDevices)
  })

  return {
    setVisible(visible) {
      if (closed) return
      region.hidden = !visible
    },
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      unsubscribeOccupancy?.()
      cardCollection.close()
      idInput.value = ''
      codeInput.value = ''
      pairingCommand.value = ''
      form.removeEventListener('submit', onConnect)
      idInput.removeEventListener('input', onIdEdit)
      codeInput.removeEventListener('input', onCodeEdit)
      options.root.replaceChildren()
    },
  }
}
