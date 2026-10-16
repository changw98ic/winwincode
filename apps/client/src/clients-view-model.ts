// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  controlPlaneClientAddFailure,
  type ControlPlaneClientAddFailure,
  type ControlPlaneClientDirectory,
  type ControlPlaneDeviceSummary,
} from './community-control-plane-client.js'
import type { DevicePairingResponse } from './generated/contracts.js'

export type ClientsAddFailure =
  | ControlPlaneClientAddFailure
  | 'invalid-client-id'
  | 'invalid-connection-code'

export type ClientsLoadStatus = 'unloaded' | 'loading' | 'loaded' | 'unavailable'

export interface ClientsConnectInput {
  readonly clientId: string
  readonly connectionCode: string
}

export interface ClientsViewModelState {
  readonly status: 'idle' | 'submitting' | 'succeeded'
  readonly failure: ClientsAddFailure | null
  readonly devices: readonly ControlPlaneDeviceSummary[]
  readonly devicesStatus: ClientsLoadStatus
  readonly pairing: DevicePairingResponse | null
  readonly pairingStatus: 'idle' | 'creating' | 'ready' | 'failed'
  readonly revokingNodeId: string | null
  readonly revokedNodeIds: readonly string[]
  readonly managementMessage: string
}

export type ClientsViewModelListener = (state: ClientsViewModelState) => void

export interface ClientsViewModel {
  readonly serverUrl: string
  readonly state: ClientsViewModelState
  subscribe(listener: ClientsViewModelListener): () => void
  addClient(input: ClientsConnectInput): Promise<void>
  createDevicePairing(): Promise<void>
  revokeDevice(clientNodeId: string): Promise<void>
  refresh(): Promise<void>
  dismissFailure(): void
  reset(): void
  close(): void
}

function state(
  status: ClientsViewModelState['status'],
  failure: ClientsAddFailure | null,
  devices: readonly ControlPlaneDeviceSummary[],
  devicesStatus: ClientsLoadStatus,
): Pick<ClientsViewModelState, 'status' | 'failure' | 'devices' | 'devicesStatus'> {
  return Object.freeze({ status, failure, devices, devicesStatus })
}

function digitsOf(value: string): string {
  return value.replace(/\D+/gu, '')
}

function shapeFailure(input: ClientsConnectInput): ClientsAddFailure | null {
  if (!/^\d{9,12}$/u.test(digitsOf(input.clientId))) return 'invalid-client-id'
  if (!/^\d{8}$/u.test(digitsOf(input.connectionCode))) return 'invalid-connection-code'
  return null
}

function addFailure(error: unknown): ClientsAddFailure {
  if (error instanceof ControlPlaneClientError) {
    if (error.code === 'CLIENT_CONNECT_ID_INVALID') return 'invalid-client-id'
    if (error.code === 'CLIENT_CONNECT_CODE_INVALID') return 'invalid-connection-code'
  }
  return controlPlaneClientAddFailure(error)
}

export function devicePresenceText(device: ControlPlaneDeviceSummary): string {
  if (device.presence === 'locked') return '已锁定'
  return device.presence === 'online' ? '在线' : '离线'
}

export function deviceStateText(device: ControlPlaneDeviceSummary): string {
  if (device.presence === 'locked') return '设备已锁定'
  if (device.occupancy === 'recovery-pending') {
    return '连接中断，正在等待恢复'
  }
  if (device.presence === 'offline') {
    return device.occupancy === 'available' ? '离线' : '离线，正在等待恢复'
  }
  switch (device.occupancy) {
    case 'available': return '在线，可以连接'
    case 'occupied-by-me': return '在线，已由你占用'
    case 'occupied-by-other': return '在线，正在使用'
    case 'draining': return '在线，正在完成当前任务'
  }
}

export function deviceStateTone(
  device: ControlPlaneDeviceSummary,
): 'info' | 'success' | 'warning' | 'danger' | 'neutral' {
  if (device.presence === 'locked') return 'danger'
  if (device.occupancy === 'recovery-pending' || device.occupancy === 'draining') {
    return 'warning'
  }
  if (device.occupancy === 'occupied-by-me') return 'info'
  if (device.occupancy === 'available' && device.presence === 'online') return 'success'
  return 'neutral'
}

export function relativeHeartbeatText(lastHeartbeatAt: string, nowMillis: number): string {
  const at = Date.parse(lastHeartbeatAt)
  if (Number.isNaN(at)) return '上次心跳时间未知'
  const elapsed = Math.max(0, nowMillis - at)
  const minutes = Math.floor(elapsed / 60_000)
  if (minutes < 1) return '刚刚收到心跳'
  if (minutes === 1) return '1 分钟前收到心跳'
  if (minutes < 60) return `${minutes} 分钟前收到心跳`
  const hours = Math.floor(minutes / 60)
  if (hours === 1) return '1 小时前收到心跳'
  if (hours < 24) return `${hours} 小时前收到心跳`
  const days = Math.floor(hours / 24)
  if (days === 1) return '昨天收到心跳'
  return `${days} 天前收到心跳`
}

export function createClientsViewModel(options: {
  readonly client: ControlPlaneClientDirectory
}): ClientsViewModel {
  const client = options.client
  const listeners = new Set<ClientsViewModelListener>()
  let current: ClientsViewModelState = Object.freeze({ ...state('idle', null, [], 'unloaded'),
    pairing: null, pairingStatus: 'idle', revokingNodeId: null,
    revokedNodeIds: Object.freeze([]), managementMessage: '' })
  let refreshEpoch = 0
  let managementEpoch = 0
  let closed = false

  function publish(next: Partial<ClientsViewModelState>): void {
    current = Object.freeze({ ...current, ...next })
    for (const listener of listeners) listener(current)
  }

  function managementFailure(error: unknown): string {
    return error instanceof ControlPlaneClientError && (error.kind === 'authorization'
      || error.code === 'PERMISSION_DENIED' || error.code === 'ACCESS_DENIED')
      ? '只有 Owner 可以配对或永久吊销设备。'
      : '设备管理请求未完成，请检查连接后重试。'
  }

  return {
    get serverUrl() { return client.serverUrl },
    get state() {
      return current
    },
    subscribe(listener) {
      if (closed) return () => {}
      listeners.add(listener)
      listener(current)
      return () => { listeners.delete(listener) }
    },
    async createDevicePairing() {
      if (closed || current.pairingStatus === 'creating') return
      const epoch = managementEpoch
      publish({ pairing: null, pairingStatus: 'creating', managementMessage: '' })
      try {
        const pairing = await client.createDevicePairing()
        if (closed || epoch !== managementEpoch) return
        publish({ pairing, pairingStatus: 'ready' })
      } catch (error) {
        if (closed || epoch !== managementEpoch) return
        publish({ pairingStatus: 'failed', managementMessage: managementFailure(error) })
      }
    },
    async revokeDevice(clientNodeId) {
      if (closed || current.revokingNodeId !== null || current.revokedNodeIds.includes(clientNodeId)) return
      const epoch = managementEpoch
      publish({ revokingNodeId: clientNodeId, managementMessage: '' })
      try {
        const result = await client.revokeDevice(clientNodeId)
        if (closed || epoch !== managementEpoch) return
        if (!result.revoked) throw new Error('Device revocation did not complete')
        publish({ revokingNodeId: null,
          revokedNodeIds: Object.freeze([...current.revokedNodeIds, clientNodeId]),
          managementMessage: '设备已永久吊销。重新接入需要新的配对令牌。' })
      } catch (error) {
        if (closed || epoch !== managementEpoch) return
        publish({ revokingNodeId: null, managementMessage: managementFailure(error) })
      }
    },
    async addClient(input) {
      if (closed || current.status === 'submitting') return
      const failure = shapeFailure(input)
      if (failure !== null) {
        publish(state('idle', failure, current.devices, current.devicesStatus))
        return
      }
      publish(state('submitting', null, current.devices, current.devicesStatus))
      try {
        const devices = await client.addClient({
          clientId: input.clientId,
          connectionCode: input.connectionCode,
        })
        if (closed) return
        publish(state('succeeded', null, devices, 'loaded'))
      } catch (error) {
        if (closed) return
        publish(state('idle', addFailure(error), current.devices, current.devicesStatus))
      }
    },
    async refresh() {
      if (closed) return
      const epoch = ++refreshEpoch
      if (current.devicesStatus === 'unloaded') {
        publish(state(current.status, current.failure, current.devices, 'loading'))
      }
      try {
        const devices = await client.listClients()
        if (closed || epoch !== refreshEpoch) return
        publish(state(current.status, current.failure, devices, 'loaded'))
      } catch {
        if (closed || epoch !== refreshEpoch) return
        publish(state(current.status, current.failure, current.devices, 'unavailable'))
      }
    },
    dismissFailure() {
      if (closed || current.failure === null) return
      publish(state(current.status, null, current.devices, current.devicesStatus))
    },
    reset() {
      if (closed) return
      managementEpoch += 1
      publish({ ...state('idle', null, current.devices, current.devicesStatus),
        pairing: null, pairingStatus: 'idle', revokingNodeId: null, managementMessage: '' })
    },
    close() {
      if (closed) return
      closed = true
      current = Object.freeze({ ...current, pairing: null })
      refreshEpoch += 1
      managementEpoch += 1
      listeners.clear()
    },
  }
}
