// SPDX-License-Identifier: Apache-2.0

import {
  type ControlPlaneClientDirectory,
  type ControlPlaneRepositorySummary,
} from './control-plane-client.js'

/** Whether, and how, the repository list was last read. */
export type RepositoriesLoadStatus = 'unloaded' | 'loading' | 'loaded' | 'unavailable'

export interface RepositoriesViewModelState {
  /** The selected Client device; the list stays empty until one is selected. */
  readonly clientId: string | null
  readonly repositories: readonly ControlPlaneRepositorySummary[]
  readonly status: RepositoriesLoadStatus
}

export type RepositoriesViewModelListener = (state: RepositoriesViewModelState) => void

export interface RepositoriesViewModel {
  readonly state: RepositoriesViewModelState
  subscribe(listener: RepositoriesViewModelListener): () => void
  /** Select one Client device and read its repositories; null clears the area. */
  showDevice(clientId: string | null): Promise<void>
  /** Re-read the list for the selected device; a failed read never discards shown cards. */
  refresh(): Promise<void>
  close(): void
}

/**
 * §16.5: the card shows the seven-character short HEAD hash; the wire keeps
 * the full Server-provided commit string and the presentation truncates.
 */
export function repositoryHeadShortText(repository: ControlPlaneRepositorySummary): string {
  return `HEAD ${repository.headCommit.slice(0, 7)}`
}

/** §16.5 dirty badge copy; the badge text carries the meaning, not color. */
export function repositoryDirtyText(repository: ControlPlaneRepositorySummary): string {
  return repository.dirtyState === 'dirty' ? 'Dirty' : 'Clean'
}

/** Non-color tone of the dirty badge. */
export function repositoryDirtyTone(
  repository: ControlPlaneRepositorySummary,
): 'success' | 'warning' {
  return repository.dirtyState === 'dirty' ? 'warning' : 'success'
}

/**
 * §16.5: one of the seven availability states is shown as a reason badge only
 * when it is not `available`; `available` renders no badge at all.
 */
export function repositoryAvailabilityText(
  repository: ControlPlaneRepositorySummary,
): string | null {
  switch (repository.availability) {
    case 'available': return null
    case 'dirty': return 'Not usable: the working tree is dirty'
    case 'unavailable': return 'Repository unavailable'
    case 'moved': return 'Repository moved on the device'
    case 'invalid_git': return 'Not a valid Git repository'
    case 'permission_denied': return 'Access denied on the device'
    case 'scan_failed': return 'The last repository scan failed'
  }
}

/** Non-color tone of the availability reason badge. */
export function repositoryAvailabilityTone(
  repository: ControlPlaneRepositorySummary,
): 'warning' | 'danger' {
  return repository.availability === 'unavailable'
    || repository.availability === 'invalid_git'
    || repository.availability === 'permission_denied'
    ? 'danger'
    : 'warning'
}

/**
 * Own the repository list reads for the selected Client device. The list is
 * Server snapshot state that the browser only displays; an unavailable read
 * marks the list and keeps the cards already shown for that device.
 */
export function createRepositoriesViewModel(options: {
  readonly client: ControlPlaneClientDirectory
}): RepositoriesViewModel {
  const client = options.client
  const listeners = new Set<RepositoriesViewModelListener>()
  const emptyState: RepositoriesViewModelState = Object.freeze({
    clientId: null,
    repositories: [],
    status: 'unloaded',
  })
  let current = emptyState
  let refreshEpoch = 0
  let closed = false

  function publish(next: RepositoriesViewModelState): void {
    current = next
    for (const listener of listeners) listener(current)
  }

  async function showDevice(clientId: string | null): Promise<void> {
    if (closed) return
    if (clientId === null) {
      refreshEpoch += 1
      publish(emptyState)
      return
    }
    const sameDevice = current.clientId === clientId
    // A selection already in flight is never repeated; a re-selection of the
    // same device keeps its shown cards during the read, while a different
    // device swaps to an empty loading list so cards never cross devices.
    if (sameDevice && current.status === 'loading') return
    const epoch = ++refreshEpoch
    if (!sameDevice || current.status === 'unloaded') {
      publish(Object.freeze({
        clientId,
        repositories: sameDevice ? current.repositories : [],
        status: 'loading',
      }))
    }
    try {
      const repositories = await client.listRepositories({ clientId })
      if (closed || epoch !== refreshEpoch) return
      publish(Object.freeze({ clientId, repositories, status: 'loaded' }))
    } catch {
      if (closed || epoch !== refreshEpoch) return
      publish(Object.freeze({
        clientId,
        repositories: current.clientId === clientId ? current.repositories : [],
        status: 'unavailable',
      }))
    }
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
    showDevice,
    async refresh() {
      if (closed || current.clientId === null) return
      await showDevice(current.clientId)
    },
    close() {
      if (closed) return
      closed = true
      refreshEpoch += 1
      listeners.clear()
    },
  }
}
