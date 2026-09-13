// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
} from './community-control-plane-client.js'
import {
  scopeSelectionOptions,
  type ScopeRouteSelection,
} from '@winwincode/browser-core/scope-context'
import type {
  OrganizationId,
  ProjectId,
  RepositoryId,
  Scope,
  WorkspaceId,
} from './generated/contracts.js'

export type ScopeSelectorStatus =
  | 'idle'
  | 'ready'
  | 'empty'
  | 'closed'

export interface ScopeSelectorOption<Id extends string = string> {
  readonly id: Id
  readonly label: string
}

export interface ScopeSelectorOptionsState {
  readonly organizations: readonly ScopeSelectorOption<OrganizationId>[]
  readonly workspaces: readonly ScopeSelectorOption<WorkspaceId>[]
  readonly projects: readonly ScopeSelectorOption<ProjectId>[]
  readonly repositories: readonly ScopeSelectorOption<RepositoryId>[]
}

export interface ScopeSelectorViewModelState {
  readonly status: ScopeSelectorStatus
  readonly selection: ScopeRouteSelection
  readonly options: ScopeSelectorOptionsState
  readonly emptyLevel: 'organization' | 'workspace' | 'project' | 'repository' | null
  readonly error: ControlPlaneClientError | null
}

export interface ScopeSelectorViewModelOptions {
  readonly authorizedScopes: readonly Scope[]
  readonly selection: ScopeRouteSelection
  readonly onSelectionChange?: (selection: ScopeRouteSelection) => void
}

export interface ScopeSelectorViewModel {
  readonly state: ScopeSelectorViewModelState
  subscribe(listener: (state: ScopeSelectorViewModelState) => void): () => void
  start(): Promise<void>
  retry(): Promise<void>
  selectOrganization(organizationId: OrganizationId): Promise<void>
  selectWorkspace(workspaceId: WorkspaceId): Promise<void>
  selectProject(projectId: ProjectId): Promise<void>
  selectRepository(repositoryId: RepositoryId): Promise<void>
  close(): void
}

function freezeSelection(selection: ScopeRouteSelection): ScopeRouteSelection {
  return Object.freeze({ ...selection })
}

function asOptions<Id extends string>(ids: readonly Id[]): readonly ScopeSelectorOption<Id>[] {
  return Object.freeze(ids.map(id => Object.freeze({ id, label: id })))
}

function emptyLevel(
  selection: ScopeRouteSelection,
  options: ScopeSelectorOptionsState,
): ScopeSelectorViewModelState['emptyLevel'] {
  if (options.organizations.length === 0) return 'organization'
  if (selection.organizationId !== null && options.workspaces.length === 0) return 'workspace'
  if (selection.workspaceId !== null && options.projects.length === 0) return 'project'
  if (selection.projectId !== null && options.repositories.length === 0) return 'repository'
  return null
}

function selectionError(): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'authorization',
    code: 'SCOPE_SELECTION_NOT_AUTHORIZED',
    message: 'The selected Scope is not present in the current browser session.',
    requestId: null,
    retryable: false,
  })
}

/**
 * Project the AuthSession hierarchy into Scope selector options.
 * Community never queries Enterprise management APIs for labels; option ids
 * come only from authorized scopes and display with the id itself.
 */
export function createScopeSelectorViewModel(
  options: ScopeSelectorViewModelOptions,
): ScopeSelectorViewModel {
  const listeners = new Set<(state: ScopeSelectorViewModelState) => void>()
  let selection = freezeSelection(options.selection)
  let closed = false

  function projectedOptions(): ScopeSelectorOptionsState {
    const ids = scopeSelectionOptions(options.authorizedScopes, selection)
    return Object.freeze({
      organizations: asOptions(ids.organizations),
      workspaces: asOptions(ids.workspaces),
      projects: asOptions(ids.projects),
      repositories: asOptions(ids.repositories),
    })
  }

  let current: ScopeSelectorViewModelState = Object.freeze({
    status: 'idle',
    selection,
    options: projectedOptions(),
    emptyLevel: null,
    error: null,
  })

  function publish(status: ScopeSelectorStatus): void {
    const nextOptions = projectedOptions()
    current = Object.freeze({
      status,
      selection,
      options: nextOptions,
      emptyLevel: status === 'ready' || status === 'empty'
        ? emptyLevel(selection, nextOptions)
        : null,
      error: null,
    })
    for (const listener of listeners) listener(current)
  }

  function requireOpen(): void {
    if (!closed) return
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'SCOPE_SELECTOR_CLOSED',
      message: 'The Scope selector is closed.',
      requestId: null,
      retryable: false,
    })
  }

  function assertOption<Id extends string>(value: Id, values: readonly { readonly id: Id }[]): void {
    if (values.some(option => option.id === value)) return
    throw selectionError()
  }

  async function load(): Promise<void> {
    requireOpen()
    const nextOptions = projectedOptions()
    if (nextOptions.organizations.length === 0) {
      publish('empty')
      return
    }
    const level = emptyLevel(selection, nextOptions)
    publish(level === null ? 'ready' : 'empty')
  }

  function choose(next: ScopeRouteSelection): Promise<void> {
    selection = freezeSelection(next)
    options.onSelectionChange?.(selection)
    if (closed) return Promise.resolve()
    return load()
  }

  return {
    get state() { return current },
    subscribe(listener) {
      requireOpen()
      listeners.add(listener)
      listener(current)
      return () => { listeners.delete(listener) }
    },
    start: load,
    retry: load,
    selectOrganization(organizationId) {
      requireOpen()
      assertOption(organizationId, projectedOptions().organizations)
      return choose({
        organizationId,
        workspaceId: null,
        projectId: null,
        repositoryId: null,
      })
    },
    selectWorkspace(workspaceId) {
      requireOpen()
      assertOption(workspaceId, projectedOptions().workspaces)
      return choose({
        organizationId: selection.organizationId,
        workspaceId,
        projectId: null,
        repositoryId: null,
      })
    },
    selectProject(projectId) {
      requireOpen()
      assertOption(projectId, projectedOptions().projects)
      return choose({
        organizationId: selection.organizationId,
        workspaceId: selection.workspaceId,
        projectId,
        repositoryId: null,
      })
    },
    selectRepository(repositoryId) {
      requireOpen()
      assertOption(repositoryId, projectedOptions().repositories)
      return choose({ ...selection, repositoryId })
    },
    close() {
      if (closed) return
      closed = true
      listeners.clear()
      current = Object.freeze({ ...current, status: 'closed', error: null })
    },
  }
}
