import type { ControlPlaneDeviceSummary } from './community-control-plane-client.js'
import {
  clientOccupancyErrorCategory,
  type ClientOccupancyErrorCategory,
} from './client-occupancy-facade.js'
import type { ClientsViewModel } from './clients-view-model.js'

export interface ClientOccupancyPort {
  claim(input: { readonly clientId: string }): Promise<void>
  release(input: {
    readonly clientId: string
    readonly mode?: 'release' | 'drain'
  }): Promise<void>
  cancelAndRelease(input: { readonly clientId: string }): Promise<void>
  forceRelease?(input: { readonly clientId: string }): Promise<void>
}

export type ClientOccupancyAction = 'claim' | 'release' | 'cancel-and-release' | 'force-release'

export type ClientOccupancyDangerAction =
  | 'release'
  | 'cancel-and-release'
  | 'force-release'

export type ClientOccupancyFailure =
  | 'occupied-by-other'
  | 'not-holder'
  | 'device-gone'
  | 'device-offline'
  | 'device-locked'
  | 'connections-forbidden'
  | 'account-denied'
  | 'capacity-exhausted'
  | 'device-rejected'
  | 'ack-timeout'
  | 'recovery-pending'
  | 'permission-denied'
  | 'confirmation-required'
  | 'state-changed'
  | 'rate-limited'
  | 'unavailable'

export type ClientOccupancyInteraction =
  | { readonly kind: 'rest' }
  | { readonly kind: 'confirming'; readonly action: ClientOccupancyDangerAction }
  | { readonly kind: 'submitting'; readonly action: ClientOccupancyAction }
  | {
    readonly kind: 'failed'
    readonly action: ClientOccupancyAction
    readonly failure: ClientOccupancyFailure
  }

const REST: ClientOccupancyInteraction = Object.freeze({ kind: 'rest' })

export function deviceSupportsClaim(device: ControlPlaneDeviceSummary): boolean {
  return device.presence === 'online' && device.occupancy === 'available'
}

export function deviceSupportsRelease(device: ControlPlaneDeviceSummary): boolean {
  return device.occupancy === 'occupied-by-me'
}

export function deviceSupportsCancelAndRelease(device: ControlPlaneDeviceSummary): boolean {
  if (device.occupancy === 'draining') return true
  return device.occupancy === 'occupied-by-me' && device.capacityUsed > 0
}

export function deviceSupportsForceRelease(device: ControlPlaneDeviceSummary): boolean {
  return device.occupancy === 'recovery-pending'
}

export function releaseDrainsFirst(device: ControlPlaneDeviceSummary): boolean {
  return device.capacityUsed > 0
}

const CATEGORY_FAILURES: Readonly<
  Record<ClientOccupancyErrorCategory, ClientOccupancyFailure | 'holder-denial'>
> = Object.freeze({
  'invalid-request': 'unavailable',
  'confirmation-required': 'confirmation-required',
  'client-not-found': 'device-gone',
  'client-offline': 'device-offline',
  'client-locked': 'device-locked',
  'new-connections-forbidden': 'connections-forbidden',
  'access-denied': 'account-denied',
  'occupied-by-other': 'occupied-by-other',
  'capacity-exhausted': 'capacity-exhausted',
  'occupancy-rejected': 'device-rejected',
  'occupancy-ack-timeout': 'ack-timeout',
  'recovery-pending': 'recovery-pending',
  'permission-denied': 'holder-denial',
  'no-active-occupancy': 'not-holder',
  'wrong-state': 'state-changed',
  'rate-limited': 'rate-limited',
  'unavailable': 'unavailable',
})

export function clientOccupancyFailure(
  error: unknown,
  action?: ClientOccupancyAction,
): ClientOccupancyFailure {
  const holderDenial = action === 'force-release' ? 'permission-denied' : 'not-holder'
  const resolve = (mapped: ClientOccupancyFailure | 'holder-denial'): ClientOccupancyFailure =>
    mapped === 'holder-denial' ? holderDenial : mapped
  return resolve(CATEGORY_FAILURES[clientOccupancyErrorCategory(error)])
}

export function deviceRecoveryDeadlineText(
  device: ControlPlaneDeviceSummary,
  nowMillis: number,
): string {
  const deadline = device.recoveryDeadlineAt ?? null
  if (deadline === null || Number.isNaN(Date.parse(deadline))) {
    return '正在等待恢复 · 未报告恢复期限'
  }
  if (Date.parse(deadline) <= nowMillis) {
    return `恢复期限 ${deadline} 已过 · 设备所有者可以强制释放`
  }
  return `连接中断 · 预计在 ${deadline} 前恢复`
}

export type ClientOccupancyViewModelListener = () => void

export interface ClientOccupancyViewModel {
  interaction(clientId: string): ClientOccupancyInteraction
  supportsForceRelease(): boolean
  requestClaim(clientId: string): void
  requestRelease(clientId: string): void
  requestCancelAndRelease(clientId: string): void
  requestForceRelease(clientId: string): void
  confirmPending(clientId: string): void
  dismiss(clientId: string): void
  subscribe(listener: ClientOccupancyViewModelListener): () => void
  close(): void
}

export function createClientOccupancyViewModel(options: {
  readonly port: ClientOccupancyPort | null
  readonly clients: ClientsViewModel
  readonly classify?: (error: unknown, action: ClientOccupancyAction) => ClientOccupancyFailure
}): ClientOccupancyViewModel {
  const classify = options.classify ?? clientOccupancyFailure
  const interactions = new Map<string, ClientOccupancyInteraction>()
  const listeners = new Set<ClientOccupancyViewModelListener>()
  let closed = false

  function findDevice(clientId: string): ControlPlaneDeviceSummary | undefined {
    return options.clients.state.devices.find(device => device.clientId === clientId)
  }

  function publish(): void {
    for (const listener of listeners) listener()
  }

  function setInteraction(clientId: string, interaction: ClientOccupancyInteraction): void {
    const previous = interactions.get(clientId) ?? REST
    if (interactionKey(previous) === interactionKey(interaction)) return
    interactions.set(clientId, interaction)
    publish()
  }

  function pruneStaleInteractions(): void {
    let changed = false
    for (const [clientId, interaction] of interactions) {
      if (interaction.kind === 'rest' || interaction.kind === 'submitting') continue
      const device = findDevice(clientId)
      const stale = device === undefined || !actionApplies(interaction.action, device)
      if (!stale) continue
      interactions.set(clientId, REST)
      changed = true
    }
    if (changed) publish()
  }

  function actionApplies(
    action: ClientOccupancyAction,
    device: ControlPlaneDeviceSummary,
  ): boolean {
    if (action === 'claim') return deviceSupportsClaim(device)
    if (action === 'release') return deviceSupportsRelease(device)
    if (action === 'force-release') return deviceSupportsForceRelease(device)
    return deviceSupportsCancelAndRelease(device)
  }

  const unsubscribeClients = options.clients.subscribe(pruneStaleInteractions)

  function interactionKey(interaction: ClientOccupancyInteraction): string {
    if (interaction.kind === 'rest') return 'rest'
    if (interaction.kind === 'failed') {
      return `failed:${interaction.action}:${interaction.failure}`
    }
    return `${interaction.kind}:${interaction.action}`
  }

  async function submit(clientId: string, action: ClientOccupancyAction): Promise<void> {
    const port = options.port
    if (port === null) {
      setInteraction(clientId, { kind: 'failed', action, failure: 'unavailable' })
      return
    }
    setInteraction(clientId, { kind: 'submitting', action })
    try {
      const input = { clientId }
      if (action === 'claim') await port.claim(input)
      else if (action === 'release') {
        const device = findDevice(clientId)
        const mode = device !== undefined && releaseDrainsFirst(device) ? 'drain' : undefined
        await port.release(mode === undefined ? input : { ...input, mode })
      }
      else if (action === 'cancel-and-release') await port.cancelAndRelease(input)
      else {
        const forceRelease = port.forceRelease
        if (forceRelease === undefined) {
          setInteraction(clientId, { kind: 'failed', action, failure: 'unavailable' })
          return
        }
        await forceRelease(input)
      }
    } catch (error) {
      if (closed) return
      setInteraction(clientId, { kind: 'failed', action, failure: classify(error, action) })
      return
    }
    if (closed) return
    setInteraction(clientId, REST)
    await options.clients.refresh()
  }

  function requestDevice(
    clientId: string,
    action: ClientOccupancyAction,
  ): ControlPlaneDeviceSummary | null {
    if (closed) return null
    const current = interactions.get(clientId) ?? REST
    if (current.kind === 'submitting') return null
    const device = findDevice(clientId)
    if (device === undefined || !actionApplies(action, device)) return null
    return device
  }

  return {
    interaction(clientId) {
      return interactions.get(clientId) ?? REST
    },
    supportsForceRelease() {
      return options.port?.forceRelease !== undefined
    },
    requestClaim(clientId) {
      if (requestDevice(clientId, 'claim') === null) return
      void submit(clientId, 'claim')
    },
    requestRelease(clientId) {
      const device = requestDevice(clientId, 'release')
      if (device === null) return
      if (releaseDrainsFirst(device)) {
        setInteraction(clientId, { kind: 'confirming', action: 'release' })
        return
      }
      void submit(clientId, 'release')
    },
    requestCancelAndRelease(clientId) {
      if (requestDevice(clientId, 'cancel-and-release') === null) return
      setInteraction(clientId, { kind: 'confirming', action: 'cancel-and-release' })
    },
    requestForceRelease(clientId) {
      if (requestDevice(clientId, 'force-release') === null) return
      setInteraction(clientId, { kind: 'confirming', action: 'force-release' })
    },
    confirmPending(clientId) {
      if (closed) return
      const current = interactions.get(clientId) ?? REST
      if (current.kind === 'confirming') {
        void submit(clientId, current.action)
        return
      }
      if (current.kind === 'failed' && current.action !== 'claim') {
        void submit(clientId, current.action)
      }
    },
    dismiss(clientId) {
      if (closed) return
      const current = interactions.get(clientId) ?? REST
      if (current.kind !== 'confirming' && current.kind !== 'failed') return
      setInteraction(clientId, REST)
    },
    subscribe(listener) {
      if (closed) return () => {}
      listeners.add(listener)
      listener()
      return () => {
        listeners.delete(listener)
      }
    },
    close() {
      if (closed) return
      closed = true
      unsubscribeClients()
      listeners.clear()
      interactions.clear()
    },
  }
}

type OccupancyFacadeMethod = (input: {
  readonly clientId: string
  readonly mode?: 'release' | 'drain'
}) => Promise<void>

function facadeMethod(value: unknown): OccupancyFacadeMethod | null {
  if (typeof value !== 'function') return null
  return value as OccupancyFacadeMethod
}

export function clientOccupancyPortFromFacade(facade: object): ClientOccupancyPort | null {
  const methods = facade as Record<string, unknown>
  const claim =
    facadeMethod(methods['claim']) ?? facadeMethod(methods['claimOccupancy'])
  const release =
    facadeMethod(methods['release']) ?? bindOccupancyRelease(methods['releaseOccupancy'])
  const cancelAndRelease =
    facadeMethod(methods['cancelAndRelease']) ??
    bindOccupancyMode(methods['releaseOccupancy'], 'cancel_and_release', true)
  if (claim === null || release === null || cancelAndRelease === null) return null
  const forceRelease = facadeMethod(methods['forceRelease']) ??
    facadeMethod(methods['forceReleaseOccupancy'])
  return {
    claim: input => claim(input),
    release: input => release(input),
    cancelAndRelease: input => cancelAndRelease(input),
    ...(forceRelease === null ? {} : { forceRelease: input => forceRelease(input) }),
  }
}

function bindOccupancyRelease(value: unknown): OccupancyFacadeMethod | null {
  const method = facadeMethod(value)
  if (method === null) return null
  const bound = method as (input: Record<string, unknown>) => Promise<void>
  return input => bound({ ...input, mode: input.mode ?? 'release' })
}

function bindOccupancyMode(
  value: unknown,
  mode: 'cancel_and_release',
  confirm?: boolean,
): OccupancyFacadeMethod | null {
  const method = facadeMethod(value)
  if (method === null) return null
  const bound = method as (input: Record<string, unknown>) => Promise<void>
  return input => bound({ ...input, mode, confirm })
}
