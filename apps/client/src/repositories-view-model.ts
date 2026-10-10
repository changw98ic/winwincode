import {
  type ControlPlaneClientDirectory,
  type ControlPlaneRepositorySummary,
} from './community-control-plane-client.js'

export type RepositoriesLoadStatus = 'unloaded' | 'loading' | 'loaded' | 'unavailable'

export interface RepositoriesViewModelState {
  readonly clientId: string | null
  readonly repositories: readonly ControlPlaneRepositorySummary[]
  readonly status: RepositoriesLoadStatus
}

export type RepositoriesViewModelListener = (state: RepositoriesViewModelState) => void

export interface RepositoriesViewModel {
  readonly state: RepositoriesViewModelState
  subscribe(listener: RepositoriesViewModelListener): () => void
  showDevice(clientId: string | null): Promise<void>
  refresh(): Promise<void>
  close(): void
}

export function repositoryHeadShortText(repository: ControlPlaneRepositorySummary): string {
  return `HEAD ${repository.headCommit.slice(0, 7)}`
}

export function repositoryDirtyText(repository: ControlPlaneRepositorySummary): string {
  return repository.dirtyState === 'dirty' ? '有改动' : '干净'
}

export function repositoryDirtyTone(
  repository: ControlPlaneRepositorySummary,
): 'success' | 'warning' {
  return repository.dirtyState === 'dirty' ? 'warning' : 'success'
}

export function repositoryAvailabilityText(
  repository: ControlPlaneRepositorySummary,
): string | null {
  switch (repository.availability) {
    case 'available': return null
    case 'dirty': return '不可用：工作区有未提交改动'
    case 'unavailable': return '仓库不可用'
    case 'moved': return '仓库已在设备上移动'
    case 'invalid_git': return '不是有效的 Git 仓库'
    case 'permission_denied': return '设备拒绝访问'
    case 'scan_failed': return '上次仓库扫描失败'
  }
}

export function repositoryAvailabilityTone(
  repository: ControlPlaneRepositorySummary,
): 'warning' | 'danger' {
  return repository.availability === 'unavailable'
    || repository.availability === 'invalid_git'
    || repository.availability === 'permission_denied'
    ? 'danger'
    : 'warning'
}

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
