// SPDX-License-Identifier: Apache-2.0

import type { HomeRecentVisitStore } from './home-recent-visits.js'
import type {
  ControlPlaneClient,
  ControlPlaneDeviceSummary,
} from './community-control-plane-client.js'
import type { ClientsLoadStatus, ClientsViewModel } from './clients-view-model.js'
import {
  createHomeDashboardViewModel,
  type HomeDashboardState,
  type HomeDashboardViewModel,
} from './home-dashboard-view-model.js'
import type {
  Actor,
  ControlPlaneWebSocketSubscriptionId,
  RepositoryScope,
  RequestId,
} from './generated/contracts.js'

export type MyWorkSource = 'work' | 'clients'

export type MyWorkSourceState = 'loading' | 'ok' | 'unavailable'

export type MyWorkStatus = 'loading' | 'ready' | 'partial' | 'error' | 'closed'

export interface MyWorkClientsSummary {
  readonly ready: number
  readonly occupiedByMe: number
  readonly occupiedByOther: number
  readonly unavailable: number
  readonly total: number
}

export interface MyWorkClientsZone {
  readonly devices: readonly ControlPlaneDeviceSummary[]
  readonly status: ClientsLoadStatus
  readonly summary: MyWorkClientsSummary
}

export interface MyWorkCounts {
  readonly needsAttention: number
  readonly running: number
  readonly ready: number
  readonly waiting: number
  readonly failed: number
  readonly completed: number
  readonly clients: number
}

export interface MyWorkState {
  readonly status: MyWorkStatus
  readonly work: HomeDashboardState
  readonly clients: MyWorkClientsZone
  readonly counts: MyWorkCounts
  readonly sources: Readonly<Record<MyWorkSource, MyWorkSourceState>>
}

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
      needsAttention: input.work.counts.decisions,
      running: input.work.counts.running + input.work.counts.validating,
      ready: input.work.counts.ready,
      waiting: input.work.counts.backlog + input.work.counts.waiting,
      failed: input.work.counts.failed,
      completed: input.work.counts.completed,
      clients: input.devices.length,
    }),
    sources,
  })
}

function emptyWorkState(status: HomeDashboardState['status']): HomeDashboardState {
  return Object.freeze({
    status,
    decisions: Object.freeze([]),
    backlog: Object.freeze([]),
    running: Object.freeze([]),
    ready: Object.freeze([]),
    waiting: Object.freeze([]),
    validating: Object.freeze([]),
    failed: Object.freeze([]),
    completed: Object.freeze([]),
    visited: Object.freeze([]),
    counts: Object.freeze({
      decisions: 0,
      backlog: 0,
      running: 0,
      ready: 0,
      waiting: 0,
      validating: 0,
      failed: 0,
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
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  readonly clients: ClientsViewModel
  readonly visits?: HomeRecentVisitStore
}

export interface MyWorkViewModel {
  readonly work: HomeDashboardViewModel
  readonly state: MyWorkState
  subscribe(listener: (state: MyWorkState) => void): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  close(): void
}

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
      if (!closed && clients.state.devicesStatus === 'unloaded') {
        await clients.refresh()
      }
      project()
    },
    async refresh() {
      if (closed) return
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
