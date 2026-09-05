// SPDX-License-Identifier: Apache-2.0

import {
  controlPlaneTaskModelRouteOptions,
  type ControlPlaneDeviceSummary,
  type ControlPlaneRepositorySummary,
  type ControlPlaneTaskAnchor,
  type ControlPlaneTaskCreateInput,
  type ControlPlaneTaskModelRouteOption,
  type ControlPlaneTaskPort,
} from './control-plane-client.js'
import type { ClientsLoadStatus, ClientsViewModel } from './clients-view-model.js'
import type {
  RepositoriesLoadStatus,
  RepositoriesViewModel,
} from './repositories-view-model.js'

/**
 * Why a submission cannot even start; every reason is a pre-request fact the
 * form already knows, plus the honest catch-all for a rejected creation.
 */
export type TaskEntryFailure =
  | 'no-occupied-client'
  | 'no-repository'
  | 'missing-base-branch'
  | 'missing-description'
  | 'missing-model-route'
  | 'unavailable'

/** The form drafts the browser owns while the page is mounted (ADR-0029). */
export interface TaskEntrySelection {
  readonly clientId: string | null
  readonly repositoryBindingId: string | null
  readonly baseBranch: string
  readonly description: string
  readonly modelRouteId: string | null
}

/**
 * One §16.6 form snapshot.  The Client and Repository options are the
 * shell-owned device and repository lists re-used verbatim — the Client list
 * is narrowed to the devices this user occupies (only an occupied Client can
 * start a task), so the form never grows a second directory.
 */
export interface TaskEntryState {
  readonly status: 'editing' | 'submitting' | 'started'
  readonly occupiedDevices: readonly ControlPlaneDeviceSummary[]
  readonly devicesStatus: ClientsLoadStatus
  readonly repositories: readonly ControlPlaneRepositorySummary[]
  readonly repositoriesStatus: RepositoriesLoadStatus
  readonly selection: TaskEntrySelection
  readonly modelRouteOptions: readonly ControlPlaneTaskModelRouteOption[]
  readonly failure: TaskEntryFailure | null
  readonly anchor: ControlPlaneTaskAnchor | null
}

export type TaskEntryListener = (state: TaskEntryState) => void

/**
 * Whether a device may start tasks right now: the user holds its occupancy
 * lease and the device can reach its Control Plane connection.
 */
export function deviceSupportsTaskStart(device: ControlPlaneDeviceSummary): boolean {
  return device.presence === 'online' && device.occupancy === 'occupied-by-me'
}

/** Whether one repository may take a new task: only an available binding. */
export function repositorySupportsTaskStart(
  repository: ControlPlaneRepositorySummary,
): boolean {
  return repository.availability === 'available'
}

/**
 * Own the §16.6 new-task form state machine.  The device and repository
 * snapshots stay in the shell-owned models; this model only keeps the form
 * drafts, derives the occupied-device and repository options, validates the
 * submission, and hands the started anchor to the page.
 */
export function createTaskEntryViewModel(options: {
  readonly clients: ClientsViewModel
  readonly repositories: RepositoriesViewModel
  readonly port: ControlPlaneTaskPort
  /** Fake §16.6 route catalog by default; MODEL routing replaces the list. */
  readonly modelRouteOptions?: readonly ControlPlaneTaskModelRouteOption[]
}): TaskEntryViewModel {
  const clients = options.clients
  const repositories = options.repositories
  const port = options.port
  const modelRouteOptions = Object.freeze(
    (options.modelRouteOptions ?? controlPlaneTaskModelRouteOptions()).slice(),
  )

  const listeners = new Set<TaskEntryListener>()
  let closed = false
  let status: TaskEntryState['status'] = 'editing'
  let failure: TaskEntryFailure | null = null
  let anchor: ControlPlaneTaskAnchor | null = null
  let selection: TaskEntrySelection = Object.freeze({
    clientId: null,
    repositoryBindingId: null,
    baseBranch: '',
    description: '',
    modelRouteId: modelRouteOptions[0]?.routeId ?? null,
  })
  let currentState: TaskEntryState = project()

  function project(): TaskEntryState {
    return Object.freeze({
      status,
      occupiedDevices: Object.freeze(
        clients.state.devices.filter(deviceSupportsTaskStart),
      ),
      devicesStatus: clients.state.devicesStatus,
      repositories: repositories.state.repositories,
      repositoriesStatus: repositories.state.status,
      selection,
      modelRouteOptions,
      failure,
      anchor,
    })
  }

  function publish(): void {
    currentState = project()
    for (const listener of listeners) listener(currentState)
  }

  function setSelection(next: TaskEntrySelection): void {
    selection = Object.freeze(next)
  }

  /**
   * Keep the form honest when the world moves: a selected Client that left
   * the occupied set, or a selected repository that left the list, drops back
   * to an empty choice instead of naming a task target that no longer exists.
   * Returns the Client id whose repositories the form no longer shows.
   */
  function reconcileWithSnapshots(): string | null {
    let droppedClient: string | null = null
    if (
      selection.clientId !== null
      && clients.state.devices.filter(deviceSupportsTaskStart).every(
        device => device.clientId !== selection.clientId,
      )
    ) {
      droppedClient = selection.clientId
      setSelection({
        ...selection,
        clientId: null,
        repositoryBindingId: null,
        baseBranch: '',
      })
    }
    if (
      selection.repositoryBindingId !== null
      && (selection.clientId === null
        || repositories.state.clientId !== selection.clientId
        || repositories.state.repositories.every(
          repository => repository.repositoryBindingId !== selection.repositoryBindingId,
        ))
    ) {
      setSelection({ ...selection, repositoryBindingId: null, baseBranch: '' })
    }
    return droppedClient
  }

  const unsubscribeClients = clients.subscribe(() => {
    if (closed) return
    const droppedClient = reconcileWithSnapshots()
    publish()
    // The dropped device keeps no repository selection anywhere: the one
    // shared repository area clears with the form.
    if (droppedClient !== null) void repositories.showDevice(null)
  })

  const unsubscribeRepositories = repositories.subscribe(() => {
    if (closed) return
    reconcileWithSnapshots()
    // A fresh repository list with no choice yet selects its first usable
    // binding and defaults the base branch to that repository's default.
    if (
      selection.clientId !== null
      && repositories.state.clientId === selection.clientId
      && selection.repositoryBindingId === null
    ) {
      const first = repositories.state.repositories.find(repositorySupportsTaskStart)
      if (first !== undefined) {
        setSelection({
          ...selection,
          repositoryBindingId: first.repositoryBindingId,
          baseBranch: first.defaultBranch,
        })
      }
    }
    publish()
  })

  function defaultBranchOf(repositoryBindingId: string | null): string {
    if (repositoryBindingId === null) return ''
    return repositories.state.repositories.find(
      candidate => candidate.repositoryBindingId === repositoryBindingId,
    )?.defaultBranch ?? ''
  }

  return {
    get state() {
      return currentState
    },
    subscribe(listener) {
      if (closed) return () => {}
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    selectClient(clientId) {
      if (closed || status === 'submitting') return
      const device = clientId === null
        ? null
        : clients.state.devices.find(
            candidate => candidate.clientId === clientId && deviceSupportsTaskStart(candidate),
          ) ?? null
      const nextClientId = device === null ? null : clientId
      if (nextClientId === selection.clientId) return
      setSelection({
        ...selection,
        clientId: nextClientId,
        repositoryBindingId: null,
        baseBranch: '',
      })
      failure = null
      publish()
      // The repository select is driven by the one shell-owned repository
      // list, so the device switch reads that device's bindings through it.
      void repositories.showDevice(nextClientId)
    },
    selectRepository(repositoryBindingId) {
      if (closed || status === 'submitting') return
      if (repositoryBindingId === selection.repositoryBindingId) return
      setSelection({
        ...selection,
        repositoryBindingId,
        // A different target repository restarts the base-branch draft on its
        // own default; re-selecting the same choice keeps the user's draft.
        baseBranch: defaultBranchOf(repositoryBindingId),
      })
      failure = null
      publish()
    },
    setBaseBranch(value) {
      if (closed) return
      setSelection({ ...selection, baseBranch: value })
      publish()
    },
    setDescription(value) {
      if (closed) return
      setSelection({ ...selection, description: value })
      publish()
    },
    selectModelRoute(routeId) {
      if (closed || status === 'submitting') return
      const known = routeId === null
        ? null
        : modelRouteOptions.find(option => option.routeId === routeId) ?? null
      const nextRouteId = known?.routeId ?? null
      if (nextRouteId === selection.modelRouteId) return
      setSelection({ ...selection, modelRouteId: nextRouteId })
      failure = null
      publish()
    },
    submit() {
      if (closed || status !== 'editing') return
      const device = selection.clientId === null
        ? undefined
        : clients.state.devices.find(
            candidate => candidate.clientId === selection.clientId
              && deviceSupportsTaskStart(candidate),
          )
      if (device === undefined) {
        failure = 'no-occupied-client'
        publish()
        return
      }
      const repository = selection.repositoryBindingId === null
        ? undefined
        : repositories.state.repositories.find(
            candidate => candidate.repositoryBindingId === selection.repositoryBindingId,
          )
      if (repository === undefined) {
        failure = 'no-repository'
        publish()
        return
      }
      const baseBranch = selection.baseBranch.trim()
      if (baseBranch.length === 0) {
        failure = 'missing-base-branch'
        publish()
        return
      }
      const description = selection.description.trim()
      if (description.length === 0) {
        failure = 'missing-description'
        publish()
        return
      }
      if (selection.modelRouteId === null) {
        failure = 'missing-model-route'
        publish()
        return
      }
      const input: ControlPlaneTaskCreateInput = {
        clientId: device.clientId,
        repositoryBindingId: repository.repositoryBindingId,
        baseBranch,
        description,
        modelRouteId: selection.modelRouteId,
      }
      status = 'submitting'
      failure = null
      publish()
      void port.create(input).then(created => {
        if (closed) return
        anchor = created
        status = 'started'
        publish()
      }, () => {
        if (closed) return
        // A rejected creation keeps every draft so the same explicit submit
        // can retry once the connection recovers.
        status = 'editing'
        failure = 'unavailable'
        publish()
      })
    },
    dismissFailure() {
      if (closed || failure === null) return
      failure = null
      publish()
    },
    async refresh() {
      if (closed) return
      await Promise.allSettled([clients.refresh(), repositories.refresh()])
    },
    async start() {
      if (closed) return
      if (clients.state.devicesStatus === 'unloaded') await clients.refresh()
      publish()
    },
    close() {
      if (closed) return
      closed = true
      unsubscribeClients()
      unsubscribeRepositories()
      listeners.clear()
    },
  }
}

export interface TaskEntryViewModel {
  /** The current form snapshot; every listener receives it on subscribe. */
  readonly state: TaskEntryState
  subscribe(listener: TaskEntryListener): () => void
  /** Choose the occupied Client the task runs on; null clears the form. */
  selectClient(clientId: string | null): void
  /** Choose one of the selected Client's repositories. */
  selectRepository(repositoryBindingId: string | null): void
  /** Edit the base-branch draft; the default is the repository default. */
  setBaseBranch(value: string): void
  /** Edit the task description draft. */
  setDescription(value: string): void
  /** Choose one model route option, or clear the choice. */
  selectModelRoute(routeId: string | null): void
  /** Validate and create the task; the anchor arrives through `state.anchor`. */
  submit(): void
  dismissFailure(): void
  /** Re-read the shell-owned device and repository snapshots. */
  refresh(): Promise<void>
  /** First read for a freshly mounted form. */
  start(): Promise<void>
  close(): void
}
