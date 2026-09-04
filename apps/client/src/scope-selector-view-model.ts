// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './control-plane-client.js'
import {
  scopeSelectionOptions,
  type ScopeRouteSelection,
} from './core/scope-context.js'
import type {
  Actor,
  EnterpriseOrganizationListResultResponse,
  EnterpriseProjectListResultResponse,
  OrganizationId,
  ProjectId,
  RepositoryId,
  RequestId,
  Scope,
  WorkspaceId,
} from './generated/contracts.js'
import { QueryName } from './generated/contracts.js'

const SCHEMA_VERSION = 'winwincode/v1'
const PAGE_LIMIT = 100

export type ScopeSelectorStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'empty'
  | 'permission-denied'
  | 'network-error'
  | 'error'
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
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly authorizedScopes: readonly Scope[]
  readonly selection: ScopeRouteSelection
  readonly nextRequestId: () => RequestId
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

function scopeDepth(scope: Scope): number {
  if (scope.kind === 'organization') return 1
  if (scope.kind === 'workspace') return 2
  if (scope.kind === 'project') return 3
  return 4
}

function scopeWithinOrganization(scope: Scope, organizationId: OrganizationId): boolean {
  return scope.organizationId === organizationId
}

function scopeWithinWorkspace(
  scope: Scope,
  organizationId: OrganizationId,
  workspaceId: WorkspaceId,
): boolean {
  return scope.organizationId === organizationId
    && scope.kind !== 'organization'
    && scope.workspaceId === workspaceId
}

function shallowest(scopes: readonly Scope[]): Scope | null {
  return [...scopes].sort((left, right) => scopeDepth(left) - scopeDepth(right))[0] ?? null
}

function freezeSelection(selection: ScopeRouteSelection): ScopeRouteSelection {
  return Object.freeze({ ...selection })
}

function asOptions<Id extends string>(
  ids: readonly Id[],
  labels: ReadonlyMap<string, string>,
): readonly ScopeSelectorOption<Id>[] {
  return Object.freeze(ids.map(id => Object.freeze({ id, label: labels.get(id) ?? id })))
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

function normalizeError(error: unknown, signal: AbortSignal): ControlPlaneClientError {
  if (error instanceof ControlPlaneClientError) return error
  if (signal.aborted) return new ControlPlaneClientError({
    kind: 'cancelled',
    code: 'REQUEST_CANCELLED',
    message: 'The Scope detail request was cancelled.',
    requestId: null,
    retryable: false,
  })
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'SCOPE_SELECTOR_QUERY_FAILURE',
    message: 'The authorized Scope details could not be loaded.',
    requestId: null,
    retryable: false,
    cause: error,
  })
}

function errorStatus(error: ControlPlaneClientError): ScopeSelectorStatus {
  if (error.kind === 'authentication' || error.kind === 'authorization') {
    return 'permission-denied'
  }
  if (error.kind === 'network' || error.kind === 'server') return 'network-error'
  return 'error'
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

/** Load display facts while keeping the AuthSession hierarchy as the only option authority. */
export function createScopeSelectorViewModel(
  options: ScopeSelectorViewModelOptions,
): ScopeSelectorViewModel {
  const listeners = new Set<(state: ScopeSelectorViewModelState) => void>()
  const organizationLabels = new Map<string, string>()
  const projectLabels = new Map<string, string>()
  const repositoryLabels = new Map<string, string>()
  let selection = freezeSelection(options.selection)
  let controller: AbortController | null = null
  let generation = 0
  let closed = false

  function projectedOptions(): ScopeSelectorOptionsState {
    const ids = scopeSelectionOptions(options.authorizedScopes, selection)
    return Object.freeze({
      organizations: asOptions(ids.organizations, organizationLabels),
      workspaces: asOptions(ids.workspaces, new Map()),
      projects: asOptions(ids.projects, projectLabels),
      repositories: asOptions(ids.repositories, repositoryLabels),
    })
  }

  let current: ScopeSelectorViewModelState = Object.freeze({
    status: 'idle',
    selection,
    options: projectedOptions(),
    emptyLevel: null,
    error: null,
  })

  function publish(
    status: ScopeSelectorStatus,
    error: ControlPlaneClientError | null = null,
  ): void {
    const nextOptions = projectedOptions()
    current = Object.freeze({
      status,
      selection,
      options: nextOptions,
      emptyLevel: status === 'ready' || status === 'empty'
        ? emptyLevel(selection, nextOptions)
        : null,
      error,
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

  async function loadOrganizationLabels(signal: AbortSignal): Promise<void> {
    const organizationId = selection.organizationId
    if (organizationId === null) return
    const scope = shallowest(options.authorizedScopes.filter(candidate => (
      scopeWithinOrganization(candidate, organizationId)
    )))
    if (scope === null) throw selectionError()
    const requestId = options.nextRequestId()
    const value = await options.client.query({
      schemaVersion: SCHEMA_VERSION,
      requestId,
      actor: options.actor,
      scope,
      query: QueryName.EnterpriseOrganizationList,
      parameters: { states: [] },
      page: { cursor: null, limit: PAGE_LIMIT },
    }, { signal })
    if (value.query !== QueryName.EnterpriseOrganizationList || value.requestId !== requestId) {
      throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'SCOPE_SELECTOR_RESPONSE_MISMATCH',
        message: 'The Scope selector received another organization response.',
        requestId,
        retryable: false,
      })
    }
    const response = value as EnterpriseOrganizationListResultResponse
    const authorized = new Set(scopeSelectionOptions(
      options.authorizedScopes,
      selection,
    ).organizations)
    for (const item of response.result.items) {
      if (authorized.has(item.id)) organizationLabels.set(item.id, item.displayName)
    }
  }

  async function loadProjectLabels(signal: AbortSignal): Promise<void> {
    const { organizationId, workspaceId } = selection
    if (organizationId === null || workspaceId === null) return
    const scope = shallowest(options.authorizedScopes.filter(candidate => (
      scopeWithinWorkspace(candidate, organizationId, workspaceId)
    )))
    if (scope === null) throw selectionError()
    const requestId = options.nextRequestId()
    const value = await options.client.query({
      schemaVersion: SCHEMA_VERSION,
      requestId,
      actor: options.actor,
      scope,
      query: QueryName.EnterpriseProjectList,
      parameters: { states: [], includeRepositories: true },
      page: { cursor: null, limit: PAGE_LIMIT },
    }, { signal })
    if (value.query !== QueryName.EnterpriseProjectList || value.requestId !== requestId) {
      throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'SCOPE_SELECTOR_RESPONSE_MISMATCH',
        message: 'The Scope selector received another project response.',
        requestId,
        retryable: false,
      })
    }
    const response = value as EnterpriseProjectListResultResponse
    const factual = scopeSelectionOptions(options.authorizedScopes, selection)
    const projects = new Set(factual.projects)
    const repositories = new Set(factual.repositories)
    for (const item of response.result.items) {
      if (item.kind === 'project' && projects.has(item.projectId)) {
        projectLabels.set(item.projectId, item.displayName)
      } else if (item.kind === 'repository' && repositories.has(item.repositoryId)) {
        repositoryLabels.set(item.repositoryId, item.displayName)
      }
    }
  }

  async function load(): Promise<void> {
    requireOpen()
    controller?.abort()
    const operationController = new AbortController()
    controller = operationController
    generation += 1
    const operationGeneration = generation
    publish('loading')
    try {
      await loadOrganizationLabels(operationController.signal)
      if (closed || generation !== operationGeneration) return
      await loadProjectLabels(operationController.signal)
      if (closed || generation !== operationGeneration) return
      const level = emptyLevel(selection, projectedOptions())
      publish(level === null ? 'ready' : 'empty')
    } catch (error) {
      if (closed || generation !== operationGeneration) return
      const normalized = normalizeError(error, operationController.signal)
      if (normalized.kind === 'cancelled') return
      publish(errorStatus(normalized), normalized)
    } finally {
      if (controller === operationController) controller = null
    }
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
      generation += 1
      controller?.abort()
      controller = null
      current = Object.freeze({ ...current, status: 'closed', error: null })
      listeners.clear()
    },
  }
}
