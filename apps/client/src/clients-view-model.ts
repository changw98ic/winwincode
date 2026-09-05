// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  controlPlaneClientAddFailure,
  type ControlPlaneClientAddFailure,
  type ControlPlaneClientDirectory,
  type ControlPlaneDeviceSummary,
} from './control-plane-client.js'

/**
 * The facade-owned wire taxonomy plus the two pre-request shape reasons the
 * view-model detects before a request exists.
 */
export type ClientsAddFailure =
  | ControlPlaneClientAddFailure
  | 'invalid-client-id'
  | 'invalid-connection-code'

/** Whether, and how, the device list was last read. */
export type ClientsLoadStatus = 'unloaded' | 'loading' | 'loaded' | 'unavailable'

export interface ClientsConnectInput {
  /** Raw field value; grouping separators are stripped by the facade. */
  readonly clientId: string
  /** Raw field value. */
  readonly connectionCode: string
}

export interface ClientsViewModelState {
  readonly status: 'idle' | 'submitting' | 'succeeded'
  readonly failure: ClientsAddFailure | null
  readonly devices: readonly ControlPlaneDeviceSummary[]
  readonly devicesStatus: ClientsLoadStatus
}

export type ClientsViewModelListener = (state: ClientsViewModelState) => void

export interface ClientsViewModel {
  readonly state: ClientsViewModelState
  subscribe(listener: ClientsViewModelListener): () => void
  /** Submit one add-Client attempt; a submission in progress is never repeated. */
  addClient(input: ClientsConnectInput): Promise<void>
  /** Re-read the device list; a failed read never discards the shown cards. */
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
): ClientsViewModelState {
  return Object.freeze({ status, failure, devices, devicesStatus })
}

/** The device digit shape; grouping separators never reach validation. */
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

/**
 * §12.1 presence column: one short word, shown separately from the combined
 * Presence×Occupancy state copy.
 */
export function devicePresenceText(device: ControlPlaneDeviceSummary): string {
  if (device.presence === 'locked') return 'Locked'
  return device.presence === 'online' ? 'Online' : 'Offline'
}

/**
 * §12.1: the six canonical Presence×Occupancy states each name one explicit
 * copy. Every remaining combination resolves to a total, honest fallback so
 * the card never shows an empty state.
 */
export function deviceStateText(device: ControlPlaneDeviceSummary): string {
  if (device.presence === 'locked') return 'Client locked'
  if (device.occupancy === 'recovery-pending') {
    return 'Connection interrupted, waiting to recover'
  }
  if (device.presence === 'offline') {
    return device.occupancy === 'available' ? 'Offline' : 'Offline, waiting to recover'
  }
  switch (device.occupancy) {
    case 'available': return 'Online, ready to connect'
    case 'occupied-by-me': return 'Online, occupied by you'
    case 'occupied-by-other': return 'Online, in use'
    case 'draining': return 'Online, finishing current tasks'
  }
}

/** Non-color tone of the presence badge; the text still carries the meaning. */
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
  if (Number.isNaN(at)) return 'Last heartbeat unknown'
  const elapsed = Math.max(0, nowMillis - at)
  const minutes = Math.floor(elapsed / 60_000)
  if (minutes < 1) return 'Last heartbeat just now'
  if (minutes === 1) return 'Last heartbeat 1 minute ago'
  if (minutes < 60) return `Last heartbeat ${minutes} minutes ago`
  const hours = Math.floor(minutes / 60)
  if (hours === 1) return 'Last heartbeat 1 hour ago'
  if (hours < 24) return `Last heartbeat ${hours} hours ago`
  const days = Math.floor(hours / 24)
  if (days === 1) return 'Last heartbeat yesterday'
  return `Last heartbeat ${days} days ago`
}

/**
 * Own the add-Client submission and the device list reads for the Clients
 * area. Failure reasons stay in the facade-owned presentation taxonomy, and
 * the device list is Server snapshot state that the browser only displays.
 */
export function createClientsViewModel(options: {
  readonly client: ControlPlaneClientDirectory
}): ClientsViewModel {
  const client = options.client
  const listeners = new Set<ClientsViewModelListener>()
  let current = state('idle', null, [], 'unloaded')
  let refreshEpoch = 0
  let closed = false

  function publish(next: ClientsViewModelState): void {
    current = next
    for (const listener of listeners) listener(current)
  }

  return {
    get state() {
      return current
    },
    subscribe(listener) {
      if (closed) return () => {}
      listeners.add(listener)
      listener(current)
      return () => { listeners.delete(listener) }
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
        // An unavailable read only marks the list; the shown cards survive so a
        // transient failure never erases the Server snapshot already displayed.
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
      publish(state('idle', null, current.devices, current.devicesStatus))
    },
    close() {
      if (closed) return
      closed = true
      refreshEpoch += 1
      listeners.clear()
    },
  }
}
