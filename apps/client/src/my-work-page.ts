import {
  devicePresenceText,
  deviceStateText,
  deviceStateTone,
  relativeHeartbeatText,
} from './clients-view-model.js'
import type { ControlPlaneDeviceSummary } from './community-control-plane-client.js'
import {
  mountKeyedCollection,
  type KeyedCollectionView,
} from './components/keyed-collection.js'
import { surfaceHash, type ScopeRouteSelection } from '@winwincode/browser-core/scope-context'
import { mountHomeDashboardPage, type HomeDashboardPage } from './home-dashboard-page.js'
import type {
  MyWorkClientsZone,
  MyWorkState,
  MyWorkViewModel,
} from './my-work-view-model.js'

export interface MyWorkPresentation {
  readonly startLabel: string
  readonly startTaskEntryLabel: string
  readonly startChatLabel: string
  readonly clientsHeading: string
  readonly clientsDescription: string
  readonly clientsUnavailable: string
  readonly clientsEmpty: string
  readonly clientsHint: string
  readonly clientsCountLabel: (count: number) => string
  readonly deviceDetailLabel: (device: ControlPlaneDeviceSummary, at: string) => string
}

const PRESENTATION_SPEC: MyWorkPresentation = {
  startLabel: '开始新任务',
  startTaskEntryLabel: '在你的设备上启动任务',
  startChatLabel: '发起对话任务',
  clientsHeading: '执行设备',
  clientsDescription: '编码设备的连接与占用状态。',
  clientsUnavailable: '执行设备区暂时不可达。以下设备保持最后已知状态。',
  clientsEmpty: '尚未连接执行设备，请先添加第一台设备。',
  clientsHint:
    '在执行设备区连接、占用和管理设备；仓库区列出各设备共享的仓库。',
  clientsCountLabel: count => `${String(count)} 台设备`,
  deviceDetailLabel: (device, heartbeat) => [
    `容量 ${String(device.capacityUsed)} / ${String(device.capacityTotal)}`,
    heartbeat,
    `版本 ${device.version}`,
  ].join(' · '),
}

const PRESENTATION: MyWorkPresentation = Object.freeze(PRESENTATION_SPEC)

export function myWorkPresentation(): MyWorkPresentation {
  return PRESENTATION
}

export interface MyWorkPageOptions {
  readonly root: HTMLElement
  readonly model: MyWorkViewModel
  readonly scopeSelection: ScopeRouteSelection
  readonly nowMillis?: () => number
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

export function mountMyWorkPage(
  options: MyWorkPageOptions,
): MyWorkPage {
  const document = options.root.ownerDocument
  const presentation = PRESENTATION
  const nowMillis = options.nowMillis ?? Date.now

  const layout = element(document, 'section', 'wwc-my-work')
  layout.dataset.wwcPage = 'home'

  const startLabel = element(document, 'p', 'wwc-my-work-start-label')
  startLabel.textContent = presentation.startLabel
  const startTaskEntry = element(document, 'a', 'wwc-my-work-start-task-entry')
  startTaskEntry.href = surfaceHash('/home/new-task', options.scopeSelection)
  startTaskEntry.textContent = presentation.startTaskEntryLabel
  const startChat = element(document, 'a', 'wwc-my-work-start-chat')
  startChat.href = surfaceHash('/chat', options.scopeSelection)
  startChat.textContent = presentation.startChatLabel
  const start = element(document, 'div', 'wwc-my-work-start')
  start.append(startLabel, startTaskEntry, startChat)
  start.hidden = true

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
  clientsZone.hidden = true

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
