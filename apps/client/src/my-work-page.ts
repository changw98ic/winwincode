// SPDX-License-Identifier: Apache-2.0

import {
  devicePresenceText,
  deviceStateText,
  deviceStateTone,
  relativeHeartbeatText,
} from './clients-view-model.js'
import type { ControlPlaneDeviceSummary } from './control-plane-client.js'
import {
  mountKeyedCollection,
  type KeyedCollectionView,
} from './components/keyed-collection.js'
import { surfaceHash, type ScopeRouteSelection } from './core/scope-context.js'
import { mountHomeDashboardPage, type HomeDashboardPage } from './home-dashboard-page.js'
import type {
  MyWorkClientsZone,
  MyWorkState,
  MyWorkViewModel,
} from './my-work-view-model.js'

/**
 * The fixed copy of the converged My Work first screen.  The section headings
 * carry the §16.2 semantics, and every deep link is scope-complete so the
 * post-login landing always continues an exact task.
 */
export interface MyWorkPresentation {
  readonly startLabel: string
  readonly startTaskEntryLabel: string
  readonly startChatLabel: string
  readonly startDeliveryLabel: string
  readonly clientsHeading: string
  readonly clientsDescription: string
  readonly clientsUnavailable: string
  readonly clientsEmpty: string
  readonly clientsHint: string
  readonly clientsCountLabel: (count: number) => string
  readonly deviceDetailLabel: (device: ControlPlaneDeviceSummary, at: string) => string
}

const PRESENTATION_SPEC: MyWorkPresentation = {
  startLabel: 'Start a new task',
  startTaskEntryLabel: 'Start a task on your Client',
  startChatLabel: 'Start a Chat task',
  startDeliveryLabel: 'Plan a StrongFlow Delivery',
  clientsHeading: 'Clients',
  clientsDescription: 'Connection and occupancy status of your coding devices.',
  clientsUnavailable: 'The Clients area is unreachable right now. The devices shown keep their last known status.',
  clientsEmpty: 'No Client is connected yet. Add your first device in the Clients area below.',
  clientsHint:
    'Connect, occupy, and manage devices in the Clients area; the Repositories area lists the repositories each device shares.',
  clientsCountLabel: count => (count === 1 ? '1 device' : `${String(count)} devices`),
  deviceDetailLabel: (device, heartbeat) => [
    `Capacity ${String(device.capacityUsed)} / ${String(device.capacityTotal)}`,
    heartbeat,
    `Version ${device.version}`,
  ].join(' · '),
}

const PRESENTATION: MyWorkPresentation = Object.freeze(PRESENTATION_SPEC)

export function myWorkPresentation(): MyWorkPresentation {
  return PRESENTATION
}

export interface MyWorkPageOptions {
  readonly root: HTMLElement
  readonly model: MyWorkViewModel
  /** The exact Scope path prefixed onto the start-task entry links. */
  readonly scopeSelection: ScopeRouteSelection
  readonly nowMillis?: () => number
  /**
   * Lifecycle ownership: `true` (the default) lets the page close the composed
   * model it mounted; a host that shares this model passes `false`.
   */
  readonly ownsModel?: boolean
}

export interface MyWorkPage {
  close(): void
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

interface DeviceRowParts {
  readonly node: HTMLLIElement
  readonly name: HTMLElement
  readonly presence: HTMLElement
  readonly state: HTMLElement
  readonly detail: HTMLElement
}

/**
 * Mount the converged My Work first screen: the §16.2 start-task entry, the
 * existing Attention-first work sections, and the Clients status zone.  The
 * work sections are the existing Home dashboard page itself, so the surface
 * keeps exactly one polite live region and every deep link it guarantees.
 */
export function mountMyWorkPage(
  options: MyWorkPageOptions,
): MyWorkPage {
  const document = options.root.ownerDocument
  const presentation = PRESENTATION
  const nowMillis = options.nowMillis ?? Date.now

  // The layout carries the Home page hook for the shell styles, while its own
  // class keeps the work dashboard the one `.wwc-home` element on the surface.
  const layout = element(document, 'section', 'wwc-my-work')
  layout.dataset.wwcPage = 'home'

  const startLabel = element(document, 'p', 'wwc-my-work-start-label')
  startLabel.textContent = presentation.startLabel
  // UI-100.2: the §16.6 new-task form deep link reuses the scope-complete
  // start-entry pattern of the Chat and Delivery links.
  const startTaskEntry = element(document, 'a', 'wwc-my-work-start-task-entry')
  startTaskEntry.href = surfaceHash('/home/new-task', options.scopeSelection)
  startTaskEntry.textContent = presentation.startTaskEntryLabel
  const startChat = element(document, 'a', 'wwc-my-work-start-chat')
  startChat.href = surfaceHash('/chat', options.scopeSelection)
  startChat.textContent = presentation.startChatLabel
  const startDelivery = element(document, 'a', 'wwc-my-work-start-delivery')
  startDelivery.href = surfaceHash('/strongflow', options.scopeSelection)
  startDelivery.textContent = presentation.startDeliveryLabel
  const start = element(document, 'div', 'wwc-my-work-start')
  start.append(startLabel, startTaskEntry, startChat, startDelivery)

  // The work sections are the existing Home dashboard page; this composition
  // reuses its live region, deep links, and first-use entry instead of a copy.
  const workRoot = element(document, 'div', 'wwc-my-work-work')
  const workPage: HomeDashboardPage = mountHomeDashboardPage({
    root: workRoot,
    model: options.model.work,
    scopeSelection: options.scopeSelection,
    ownsModel: false,
  })

  const clientsHeading = element(document, 'h3', 'wwc-my-work-clients-heading')
  clientsHeading.id = 'wwc-my-work-clients-heading'
  clientsHeading.textContent = presentation.clientsHeading
  const clientsCount = element(document, 'span', 'wwc-my-work-clients-count')
  const clientsHeader = element(document, 'header', 'wwc-my-work-clients-header')
  clientsHeader.append(clientsHeading, clientsCount)
  const clientsDescription = element(document, 'p', 'wwc-my-work-clients-description')
  clientsDescription.textContent = presentation.clientsDescription
  const clientsUnavailable = element(document, 'p', 'wwc-my-work-clients-unavailable')
  clientsUnavailable.textContent = presentation.clientsUnavailable
  clientsUnavailable.hidden = true
  const clientsEmpty = element(document, 'p', 'wwc-my-work-clients-empty')
  clientsEmpty.textContent = presentation.clientsEmpty
  clientsEmpty.hidden = true
  const deviceList = element(document, 'ul', 'wwc-my-work-clients-devices')
  const clientsHint = element(document, 'p', 'wwc-my-work-clients-hint')
  clientsHint.textContent = presentation.clientsHint
  const clientsZone = element(document, 'section', 'wwc-my-work-clients')
  clientsZone.setAttribute('aria-labelledby', clientsHeading.id)
  clientsZone.append(
    clientsHeader,
    clientsDescription,
    clientsUnavailable,
    clientsEmpty,
    deviceList,
    clientsHint,
  )

  layout.append(start, workRoot, clientsZone)
  options.root.replaceChildren(layout)

  const rowParts = new WeakMap<HTMLLIElement, DeviceRowParts>()

  function createDeviceRow(): HTMLLIElement {
    const node = element(document, 'li', 'wwc-clients-card wwc-my-work-clients-device')
    const name = element(document, 'p', 'wwc-clients-card-name')
    const presence = element(document, 'span', 'wwc-clients-card-presence')
    const state = element(document, 'p', 'wwc-clients-card-state')
    const detail = element(document, 'p', 'wwc-my-work-clients-device-detail')
    node.append(name, presence, state, detail)
    rowParts.set(node, { node, name, presence, state, detail })
    return node
  }

  function updateDeviceRow(node: HTMLLIElement, device: ControlPlaneDeviceSummary): void {
    const parts = rowParts.get(node)
    if (parts === undefined) return
    node.dataset.clientId = device.clientId
    parts.name.textContent = device.displayName
    parts.presence.dataset.tone = deviceStateTone(device)
    parts.presence.textContent = devicePresenceText(device)
    parts.state.textContent = deviceStateText(device)
    parts.detail.textContent = presentation.deviceDetailLabel(
      device,
      relativeHeartbeatText(device.lastHeartbeatAt, nowMillis()),
    )
  }

  const devices: KeyedCollectionView<
    ControlPlaneDeviceSummary,
    string,
    HTMLLIElement
  > = mountKeyedCollection({
    parent: deviceList,
    key: device => device.clientId,
    create: createDeviceRow,
    update: updateDeviceRow,
  })

  let closed = false

  function renderClients(zone: MyWorkClientsZone): void {
    if (closed) return
    devices.update(zone.devices)
    clientsCount.textContent = presentation.clientsCountLabel(zone.summary.total)
    // A failed read marks the zone; the served device rows survive it.
    clientsUnavailable.hidden = zone.status !== 'unavailable'
    clientsEmpty.hidden = !(zone.status === 'loaded' && zone.devices.length === 0)
  }

  function render(state: MyWorkState): void {
    if (closed) return
    layout.setAttribute('aria-busy', String(state.status === 'loading'))
    renderClients(state.clients)
  }

  const unsubscribe = options.model.subscribe(render)

  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      devices.close()
      workPage.close()
      options.root.replaceChildren()
      if (options.ownsModel !== false) options.model.close()
    },
  }
}
