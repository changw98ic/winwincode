// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneDeviceSummary,
} from './control-plane-client.js'
import type { ClientsViewModel } from './clients-view-model.js'

/**
 * The occupancy seam the controls consume, derived from the frozen
 * claim/get/release/force-release browser facade (CLIENT-300.4). Every method
 * resolves when the Server accepted the request and rejects with the one
 * `ControlPlaneClientError` identity; the resulting occupancy fact always
 * arrives through the next device-list snapshot, never from this return value.
 */
export interface ClientOccupancyPort {
  /** Claim the free device for the signed-in identity; the same holder repeats idempotently. */
  claim(input: { readonly clientId: string }): Promise<void>
  /** Release the held device; the Server drains while tasks still run. */
  release(input: { readonly clientId: string }): Promise<void>
  /** Stop the running tasks and release the device immediately. */
  cancelAndRelease(input: { readonly clientId: string }): Promise<void>
}

/** The one occupancy interaction a device card can be in. */
export type ClientOccupancyAction = 'claim' | 'release' | 'cancel-and-release'

/** The two actions that must pass an explicit confirmation before they submit. */
export type ClientOccupancyDangerAction = 'release' | 'cancel-and-release'

/**
 * The one presentation-facing occupancy failure taxonomy. Wire codes are
 * translated by the classifier seam below and never read anywhere else; the
 * facade-owned classifier replaces the provisional default when it lands.
 */
export type ClientOccupancyFailure =
  | 'occupied-by-other'
  | 'not-holder'
  | 'device-offline'
  | 'device-locked'
  | 'recovery-pending'
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

/** Whether the device card may offer the connect (claim) entry right now. */
export function deviceSupportsClaim(device: ControlPlaneDeviceSummary): boolean {
  return device.presence === 'online' && device.occupancy === 'available'
}

/** Whether the card belongs to the signed-in holder and can be released. */
export function deviceSupportsRelease(device: ControlPlaneDeviceSummary): boolean {
  return device.occupancy === 'occupied-by-me'
}

/**
 * Whether the immediate stop entry applies: the holder still runs tasks, or
 * the device is already draining its last tasks for this holder.
 */
export function deviceSupportsCancelAndRelease(device: ControlPlaneDeviceSummary): boolean {
  if (device.occupancy === 'draining') return true
  return device.occupancy === 'occupied-by-me' && device.capacityUsed > 0
}

/** Whether releasing this device drains first instead of freeing it directly. */
export function releaseDrainsFirst(device: ControlPlaneDeviceSummary): boolean {
  return device.capacityUsed > 0
}

/**
 * The provisional wire-code translation used until the facade-owned error
 * classifier (CLIENT-300.4) is injected through `classify`. Stable codes map
 * to their presentation reasons; everything else stays honestly unavailable.
 */
const OCCUPANCY_FAILURE_CODES: Readonly<Record<string, ClientOccupancyFailure>> =
  Object.freeze({
    OCCUPANCY_HELD_BY_OTHER: 'occupied-by-other',
    OCCUPANCY_NOT_HELD: 'not-holder',
    CLIENT_OFFLINE: 'device-offline',
    CLIENT_LOCKED: 'device-locked',
    OCCUPANCY_RECOVERY_PENDING: 'recovery-pending',
    RATE_LIMITED: 'rate-limited',
  })

/**
 * Translate one occupancy rejection into the presentation taxonomy. Every
 * wire code stays inside this function; view-models and pages branch only on
 * the returned union.
 */
export function clientOccupancyFailure(error: unknown): ClientOccupancyFailure {
  if (error instanceof ControlPlaneClientError) {
    const mapped = OCCUPANCY_FAILURE_CODES[error.code]
    if (mapped !== undefined) return mapped
    if (error.kind === 'authorization') return 'not-holder'
  }
  return 'unavailable'
}

export type ClientOccupancyViewModelListener = () => void

/**
 * Own the per-device occupancy interaction state machine for the Clients
 * area. The device-list snapshot stays the single occupancy authority: this
 * model only arms confirmations, submits one deduplicated request per device,
 * and asks the clients model to re-read the snapshot after a request lands.
 */
export interface ClientOccupancyViewModel {
  /** The current interaction of one device; unknown devices rest. */
  interaction(clientId: string): ClientOccupancyInteraction
  /** Connect (claim) the free device; an in-flight request is never repeated. */
  requestClaim(clientId: string): void
  /**
   * Release the held device; a busy device first arms the explicit
   * drain-release confirmation instead of submitting.
   */
  requestRelease(clientId: string): void
  /** Arm the explicit cancel-and-release confirmation for the held device. */
  requestCancelAndRelease(clientId: string): void
  /** Submit the armed dangerous action; only a confirmation state submits. */
  confirmPending(clientId: string): void
  /** Drop the armed confirmation or the shown failure of one device. */
  dismiss(clientId: string): void
  subscribe(listener: ClientOccupancyViewModelListener): () => void
  close(): void
}

/**
 * Create the occupancy interaction model. `port` stays null until the
 * facade-backed adapter is composed; a null port submits nothing and every
 * action resolves to the honest unavailable failure.
 */
export function createClientOccupancyViewModel(options: {
  readonly port: ClientOccupancyPort | null
  readonly clients: ClientsViewModel
  /** Facade-owned classifier seam; the default reads stable wire codes only. */
  readonly classify?: (error: unknown) => ClientOccupancyFailure
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

  /**
   * A snapshot that moved past the armed intent or the shown failure drops
   * it: the world changed, so the draft is stale and the card returns to the
   * actions the new snapshot supports.
   */
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
      else if (action === 'release') await port.release(input)
      else await port.cancelAndRelease(input)
    } catch (error) {
      if (closed) return
      setInteraction(clientId, { kind: 'failed', action, failure: classify(error) })
      return
    }
    if (closed) return
    setInteraction(clientId, REST)
    // The Server snapshot stays the single occupancy authority, so every
    // landed request re-reads the device list instead of migrating the card.
    await options.clients.refresh()
  }

  /**
   * One guard for every entry: an in-flight request is never repeated, and a
   * request the current snapshot no longer supports is ignored. Returns the
   * validating device fact so the caller can decide the confirmation path.
   */
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
    requestClaim(clientId) {
      if (requestDevice(clientId, 'claim') === null) return
      void submit(clientId, 'claim')
    },
    requestRelease(clientId) {
      const device = requestDevice(clientId, 'release')
      if (device === null) return
      // An idle device frees directly; a busy device must pass the explicit
      // drain-release confirmation first.
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
    confirmPending(clientId) {
      if (closed) return
      const current = interactions.get(clientId) ?? REST
      if (current.kind === 'confirming') {
        void submit(clientId, current.action)
        return
      }
      // A failed dangerous action keeps its armed draft: the same explicit
      // accept retries it. A failed claim retries through connect itself.
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

type OccupancyFacadeMethod = (input: { readonly clientId: string }) => Promise<void>

function facadeMethod(value: unknown): OccupancyFacadeMethod | null {
  if (typeof value !== 'function') return null
  return value as OccupancyFacadeMethod
}

/**
 * Adapt the frozen occupancy facade to the port the controls consume. The
 * facade names the immediate stop path force-release; the controls call it
 * cancel-and-release. Returns null while the facade methods have not landed,
 * so hosts compose the port only when the seam exists.
 */
export function clientOccupancyPortFromFacade(facade: object): ClientOccupancyPort | null {
  const methods = facade as Record<string, unknown>
  const claim =
    facadeMethod(methods['claim']) ?? facadeMethod(methods['claimOccupancy'])
  const release =
    facadeMethod(methods['release']) ??
    bindOccupancyMode(methods['releaseOccupancy'], 'release')
  const cancelAndRelease =
    facadeMethod(methods['cancelAndRelease']) ??
    facadeMethod(methods['forceRelease']) ??
    bindOccupancyMode(methods['forceReleaseOccupancy'], 'cancel_and_release', true)
  if (claim === null || release === null || cancelAndRelease === null) return null
  return {
    claim: input => claim(input),
    release: input => release(input),
    cancelAndRelease: input => cancelAndRelease(input),
  }
}

function bindOccupancyMode(
  value: unknown,
  mode: 'release' | 'cancel_and_release',
  confirm?: boolean,
): OccupancyFacadeMethod | null {
  const method = facadeMethod(value)
  if (method === null) return null
  const bound = method as (input: Record<string, unknown>) => Promise<void>
  return input => bound({ ...input, mode, confirm })
}
