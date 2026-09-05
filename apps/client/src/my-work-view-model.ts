// SPDX-License-Identifier: Apache-2.0

import type {
  ControlPlaneClient,
  ControlPlaneDeviceSummary,
} from './control-plane-client.js'
import type { ClientsLoadStatus, ClientsViewModel } from './clients-view-model.js'
import {
  createHomeDashboardViewModel,
  type HomeDashboardState,
  type HomeDashboardViewModel,
} from './home-dashboard-view-model.js'
import type { HomeRecentVisitStore } from './home-recent-visits.js'
import type {
  Actor,
  ControlPlaneWebSocketSubscriptionId,
  RepositoryScope,
  RequestId,
} from './generated/contracts.js'

/**
 * UX-100.1 converges the post-login first screen: the §16.2 My Work semantics
 * (needs attention, in progress, recently completed, plus the start-task entry
 * and the Clients status zone) are projected from the projections that already
 * exist.  This module owns no task state of its own: `work` is the one composed
 * Home dashboard snapshot, and the device list is the shell-owned Clients area
 * model, so a second queue or a second directory can never drift.
 */
export type MyWorkSource = 'work' | 'clients'

export type MyWorkSourceState = 'loading' | 'ok' | 'unavailable'

export type MyWorkStatus = 'loading' | 'ready' | 'partial' | 'error' | 'closed'

/**
 * §16.2 Clients zone buckets.  Occupancy stays at the state-copy level: a
 * device held by somebody else is only ever "in use", never by whom.
 */
export interface MyWorkClientsSummary {
  readonly ready: number
  readonly occupiedByMe: number
  readonly occupiedByOther: number
  readonly unavailable: number
  readonly total: number
}

/** One read of the shared Clients area model, as the zone displays it. */
export interface MyWorkClientsZone {
  readonly devices: readonly ControlPlaneDeviceSummary[]
  readonly status: ClientsLoadStatus
  readonly summary: MyWorkClientsSummary
}

export interface MyWorkCounts {
  readonly needsAttention: number
  readonly running: number
  readonly completed: number
  readonly clients: number
}

export interface MyWorkState {
  readonly status: MyWorkStatus
  /** The one composed Attention/Delivery/Usage snapshot, reused verbatim. */
  readonly work: HomeDashboardState
  readonly clients: MyWorkClientsZone
  readonly counts: MyWorkCounts
  readonly sources: Readonly<Record<MyWorkSource, MyWorkSourceState>>
}

/** Bucket one device snapshot for the zone; pure and total over every state. */
export function myWorkClientsSummary(
  devices: readonly ControlPlaneDeviceSummary[],
): MyWorkClientsSummary {
  const summary = { ready: 0, occupiedByMe: 0, occupiedByOther: 0, unavailable: 0 }
  for (const device of devices) {
    if (device.presence === 'online' && device.occupancy === 'available') {
      summary.ready += 1
    } else if (device.occupancy === 'occupied-by-me') {
      summary.occupiedByMe += 1
    } else if (device.occupancy === 'occupied-by-other') {
      summary.occupiedByOther += 1
    } else {
      summary.unavailable += 1
    }
  }
  return Object.freeze({ ...summary, total: devices.length })
}

function workSourceState(status: HomeDashboardState['status']): MyWorkSourceState {
  return status === 'loading' ? 'loading' : status === 'ready' || status === 'partial' ? 'ok' : 'unavailable'
}

function clientsSourceState(status: ClientsLoadStatus): MyWorkSourceState {
  return status === 'loaded' ? 'ok' : status === 'unavailable' ? 'unavailable' : 'loading'
}

function myWorkStatus(sources: Readonly<Record<MyWorkSource, MyWorkSourceState>>): MyWorkStatus {
  const values = Object.values(sources)
  if (values.includes('loading')) return 'loading'
  if (sources.work === 'unavailable' && sources.clients === 'unavailable') return 'error'
  return values.includes('unavailable') ? 'partial' : 'ready'
}

/**
 * Project the reused snapshots into the converged My Work state.  A failed
 * read arrives as an `unavailable` source while both snapshots keep their last
 * served facts, so no section a user already saw is ever cleared.
 */
export function myWorkState(input: {
  readonly work: HomeDashboardState
  readonly devices: readonly ControlPlaneDeviceSummary[]
  readonly devicesStatus: ClientsLoadStatus
}): MyWorkState {
  const sources: Readonly<Record<MyWorkSource, MyWorkSourceState>> = Object.freeze({
    work: workSourceState(input.work.status),
    clients: clientsSourceState(input.devicesStatus),
  })
  return Object.freeze({
    status: myWorkStatus(sources),
    work: input.work,
    clients: Object.freeze({
      devices: Object.freeze([...input.devices]),
      status: input.devicesStatus,
      summary: myWorkClientsSummary(input.devices),
    }),
    counts: Object.freeze({
      needsAttention: input.work.counts.decisions + input.work.counts.failing,
      running: input.work.counts.active,
      completed: input.work.counts.completed,
      clients: input.devices.length,
    }),
    sources,
  })
}

function emptyWorkState(status: HomeDashboardState['status']): HomeDashboardState {
  // The dashboard model publishes its own loading and closed shapes; this
  // mirror only has to agree with it until the first real snapshot arrives.
  return Object.freeze({
    status,
    decisions: Object.freeze([]),
    active: Object.freeze([]),
    failing: Object.freeze([]),
    completed: Object.freeze([]),
    visited: Object.freeze([]),
    counts: Object.freeze({
      decisions: 0,
      active: 0,
      failing: 0,
      completed: 0,
      visited: 0,
    }),
    sources: Object.freeze({
      delivery: 'loading',
      attention: 'loading',
      usage: 'loading',
    }),
    firstUse: false,
  })
}

function closedState(previous: MyWorkState): MyWorkState {
  return Object.freeze({
    ...previous,
    status: 'closed' as const,
    sources: Object.freeze({
      work: 'unavailable' as const,
      clients: 'unavailable' as const,
    }),
  })
}

export interface MyWorkViewModelOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  /** One scope event subscription, opened by the reused dashboard projection. */
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  /**
   * The shell-owned Clients area model.  My Work subscribes to it and may
   * trigger one first read, but never closes or replaces it.
   */
  readonly clients: ClientsViewModel
  /** Browser-only recent Delivery visits; defaults to the local storage store. */
  readonly visits?: HomeRecentVisitStore
}

export interface MyWorkViewModel {
  /** The composed Attention/Delivery/Usage projection the work sections render. */
  readonly work: HomeDashboardViewModel
  readonly state: MyWorkState
  subscribe(listener: (state: MyWorkState) => void): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  close(): void
}

/** Compose the existing Home dashboard projection and the shared Clients area model. */
export function createMyWorkViewModel(
  options: MyWorkViewModelOptions,
): MyWorkViewModel {
  const work = createHomeDashboardViewModel({
    client: options.client,
    actor: options.actor,
    scope: options.scope,
    subscriptionId: options.subscriptionId,
    nextRequestId: options.nextRequestId,
    ...(options.visits === undefined ? {} : { visits: options.visits }),
  })
  const clients = options.clients

  const listeners = new Set<(state: MyWorkState) => void>()
  let currentState = myWorkState({
    work: emptyWorkState('loading'),
    devices: [],
    devicesStatus: 'unloaded',
  })
  let closed = false

  function publish(state: MyWorkState): void {
    currentState = state
    for (const listener of listeners) listener(currentState)
  }

  function project(): void {
    if (closed) return
    publish(myWorkState({
      work: work.state,
      devices: clients.state.devices,
      devicesStatus: clients.state.devicesStatus,
    }))
  }

  const unsubscribeWork = work.subscribe(() => { project() })
  const unsubscribeClients = clients.subscribe(() => { project() })

  return {
    get work() { return work },
    get state() { return currentState },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async start() {
      if (closed) return
      await work.start()
      // The shell refreshes the Clients area on sign-in; this first read only
      // covers the case where My Work renders before that read ever ran.
      if (!closed && clients.state.devicesStatus === 'unloaded') {
        await clients.refresh()
      }
      project()
    },
    async refresh() {
      if (closed) return
      // A failed read keeps every served list in its owning model; the
      // re-projection below therefore never clears a shown section.
      await Promise.allSettled([work.refresh(), clients.refresh()])
      project()
    },
    close() {
      if (closed) return
      closed = true
      unsubscribeWork()
      unsubscribeClients()
      listeners.clear()
      work.close()
      publish(closedState(currentState))
    },
  }
}
