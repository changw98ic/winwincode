// SPDX-License-Identifier: Apache-2.0

import { mountKeyedCollection } from './components/keyed-collection.js'
import type { ControlPlaneDeviceSummary } from './control-plane-client.js'
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
  /**
   * CLIENT-300.5: the occupancy interaction model behind the device card
   * connect, release, and cancel-and-release controls. When absent the cards
   * render without the occupancy actions.
   */
  readonly occupancy?: ClientOccupancyViewModel
  /** Epoch-milliseconds clock for the relative heartbeat text. */
  readonly now?: () => number
  /**
   * REPO-100.3: raised when the user selects a device card for its repository
   * list, and with null when the selection is toggled off or the selected
   * device leaves the list.
   */
  readonly onDeviceSelect?: (clientId: string | null) => void
}

export interface ClientsPage {
  close(): void
  /** Show or hide the area; drafts survive visibility changes and re-renders. */
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
    case 'invalid-client-id': return 'Enter the 9-12 digit Client ID shown on the device.'
    case 'invalid-connection-code': return 'Enter the 8-digit connection code shown on the device.'
    case 'id-not-found': return 'No Client has this ID. Check the ID shown on the device.'
    case 'client-offline': return 'That Client is offline right now. Connect when it is back online.'
    case 'code-invalid': return 'The connection code is wrong. Check the code on the device and try again.'
    case 'code-expired': return 'The connection code expired. Generate a new code on the device and try again.'
    case 'new-connections-forbidden': return 'That Client no longer accepts new connections.'
    case 'client-locked': return 'That Client is locked. Unlock it on the device first.'
    case 'rate-limited': return 'Too many connection attempts. Wait a moment, then try again.'
    case 'unavailable': return 'Adding a Client is unavailable right now. Check the connection and try again.'
  }
}

function statusText(state: ClientsViewModelState): string {
  if (state.status === 'submitting') return 'Connecting to the Client…'
  if (state.status === 'succeeded') return 'Client added.'
  return ''
}

/** Display grouping only: digits stay the wire shape, groups of four for humans. */
function groupClientId(value: string): string {
  const digits = value.replace(/\D+/gu, '').slice(0, 12)
  return digits.replace(/(\d{4})(?=\d)/gu, '$1 ')
}

/**
 * Mount the signed-in Clients area: the add-Client form and the device card
 * list. The area keeps one polite alert channel and renders its titles as
 * styled paragraphs, mirroring the sign-in page's shell audit constraints.
 */
export function mountClientsPage(options: ClientsPageOptions): ClientsPage {
  const document = options.root.ownerDocument
  const now = options.now ?? (() => Date.now())
  const region = element(document, 'section', 'wwc-clients')
  const heading = element(document, 'p', 'wwc-clients-heading')
  const status = element(document, 'p', 'wwc-clients-status')
  const error = element(document, 'p', 'wwc-clients-error')
  const form = element(document, 'form', 'wwc-clients-add-form')
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
  // REPO-100.3: the device card selection that drives the repository area.
  let selectedClientId: string | null = null
  // CLIENT-300.5: the snapshot the occupancy interaction re-renders against.
  let latestDevices: readonly ControlPlaneDeviceSummary[] = []

  region.setAttribute('aria-label', 'Clients')
  region.hidden = true
  heading.textContent = 'Clients'
  heading.id = 'wwc-clients-heading'
  region.setAttribute('aria-labelledby', heading.id)
  status.hidden = true
  error.setAttribute('role', 'alert')
  error.hidden = true
  error.id = 'wwc-clients-error'

  formHeading.textContent = 'Add a Client'
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
  idLabel.textContent = 'Client ID'

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
  codeLabel.textContent = 'Connection code'

  submit.type = 'submit'
  submit.textContent = 'Connect'
  form.append(formHeading, idLabel, idInput, codeLabel, codeInput, submit)

  empty.textContent = 'No Clients yet. Connect your first device above.'
  empty.hidden = true

  region.append(heading, form, status, error, empty, list)
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
      // CLIENT-300.5: the occupancy controls replace the Wave 2 placeholder
      // buttons; without an occupancy model the actions keep only the
      // repository toggle.
      const occupancy = options.occupancy === undefined
        ? null
        : mountClientOccupancyControls({
          document,
          actions,
          notice,
          model: options.occupancy,
        })
      // REPO-100.3: the repository list of this device opens from a pressed
      // toggle so keyboard and screen-reader users get the same selection.
      const select = element(document, 'button', 'wwc-clients-card-select')
      select.type = 'button'
      select.textContent = 'Repositories'
      select.setAttribute('aria-pressed', 'false')
      select.addEventListener('click', () => {
        selectedClientId = selectedClientId === device.clientId ? null : device.clientId
        applySelection()
        options.onDeviceSelect?.(selectedClientId)
      })
      actions.append(select)
      card.append(name, presence, stateText, capacity, heartbeat, version, actions, notice)
      const refs: ClientCardRefs = {
        card, clientId: device.clientId, name, presence, stateText,
        capacity, heartbeat, version, actions, notice, occupancy, select,
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
    refs.capacity.textContent = `Capacity ${device.capacityUsed} / ${device.capacityTotal}`
    refs.heartbeat.textContent = relativeHeartbeatText(device.lastHeartbeatAt, now())
    refs.version.textContent = `Version ${device.version}`
    refs.occupancy?.update(device)
  }

  /** Reflect the single card selection across every mounted select toggle. */
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
    // Shape errors mark their own field; every Server rejection marks the form.
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
    // A device that left the list can stay selected nowhere; the repository
    // area hears the drop through the same callback as a manual toggle.
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
    // The dynamic code is short-lived secret material; it leaves the DOM
    // before the await, mirroring the sign-in password draft.
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
  // CLIENT-300.5: an interaction change re-renders the occupancy controls of
  // the shown snapshot without re-reading the Server.
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
      form.removeEventListener('submit', onConnect)
      idInput.removeEventListener('input', onIdEdit)
      codeInput.removeEventListener('input', onCodeEdit)
      options.root.replaceChildren()
    },
  }
}
