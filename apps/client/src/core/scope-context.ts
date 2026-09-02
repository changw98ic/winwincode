// SPDX-License-Identifier: Apache-2.0

import type {
  OrganizationId,
  ProjectId,
  RepositoryId,
  RepositoryScope,
  Scope,
  WorkspaceId,
} from '../generated/contracts.js'

const SCOPE_PARAMETER_NAMES = Object.freeze([
  'organizationId',
  'workspaceId',
  'projectId',
  'repositoryId',
] as const)

export interface ScopeRouteSelection {
  readonly organizationId: OrganizationId | null
  readonly workspaceId: WorkspaceId | null
  readonly projectId: ProjectId | null
  readonly repositoryId: RepositoryId | null
}

export interface CompleteRepositorySelection {
  readonly organizationId: OrganizationId
  readonly workspaceId: WorkspaceId
  readonly projectId: ProjectId
  readonly repositoryId: RepositoryId
}

export interface ScopeSelectionOptions {
  readonly organizations: readonly OrganizationId[]
  readonly workspaces: readonly WorkspaceId[]
  readonly projects: readonly ProjectId[]
  readonly repositories: readonly RepositoryId[]
}

interface ScopeResolutionBase {
  readonly selection: ScopeRouteSelection
  readonly options: ScopeSelectionOptions
}

export type ScopeContextResolution =
  | (ScopeResolutionBase & {
      readonly status: 'selected'
      readonly source: 'url' | 'only-compatible'
      readonly scope: Scope
    })
  | (ScopeResolutionBase & {
      readonly status: 'selection-required'
      readonly reason: 'multiple-compatible' | 'partial'
    })
  | (ScopeResolutionBase & {
      readonly status: 'empty'
      readonly reason: 'no-compatible'
    })
  | (ScopeResolutionBase & {
      readonly status: 'denied'
      readonly reason: 'invalid-route' | 'not-authorized'
    })

function emptySelection(): ScopeRouteSelection {
  return Object.freeze({
    organizationId: null,
    workspaceId: null,
    projectId: null,
    repositoryId: null,
  })
}

function hasWorkspace(scope: Scope): scope is Exclude<Scope, { readonly kind: 'organization' }> {
  return scope.kind !== 'organization'
}

function hasProject(scope: Scope): scope is Extract<Scope, { readonly kind: 'project' | 'repository' }> {
  return scope.kind === 'project' || scope.kind === 'repository'
}

function scopeSelection(scope: Scope): ScopeRouteSelection {
  return Object.freeze({
    organizationId: scope.organizationId,
    workspaceId: hasWorkspace(scope) ? scope.workspaceId : null,
    projectId: hasProject(scope) ? scope.projectId : null,
    repositoryId: scope.kind === 'repository' ? scope.repositoryId : null,
  })
}

function uniqueSorted<T extends string>(values: readonly T[]): readonly T[] {
  return Object.freeze([...new Set(values)].sort())
}

function selectionIsEmpty(selection: ScopeRouteSelection): boolean {
  return selection.organizationId === null
}

function selectionIsStructured(selection: ScopeRouteSelection): boolean {
  if (selection.organizationId === null) {
    return selection.workspaceId === null
      && selection.projectId === null
      && selection.repositoryId === null
  }
  if (selection.workspaceId === null) {
    return selection.projectId === null && selection.repositoryId === null
  }
  if (selection.projectId === null) return selection.repositoryId === null
  return true
}

function matchesSelection(scope: Scope, selection: ScopeRouteSelection): boolean {
  if (
    selection.organizationId !== null
    && scope.organizationId !== selection.organizationId
  ) return false
  if (selection.workspaceId !== null) {
    if (!hasWorkspace(scope) || scope.workspaceId !== selection.workspaceId) return false
  }
  if (selection.projectId !== null) {
    if (!hasProject(scope) || scope.projectId !== selection.projectId) return false
  }
  if (
    selection.repositoryId !== null
    && (scope.kind !== 'repository' || scope.repositoryId !== selection.repositoryId)
  ) return false
  return true
}

export function scopeSelectionOptions(
  scopes: readonly Scope[],
  selection: ScopeRouteSelection,
): ScopeSelectionOptions {
  const organizations = uniqueSorted(scopes.map(scope => scope.organizationId))
  const workspaces = selection.organizationId === null
    ? []
    : uniqueSorted(scopes.flatMap(scope => (
        scope.organizationId === selection.organizationId && hasWorkspace(scope)
          ? [scope.workspaceId]
          : []
      )))
  const projects = selection.organizationId === null || selection.workspaceId === null
    ? []
    : uniqueSorted(scopes.flatMap(scope => (
        scope.organizationId === selection.organizationId
        && hasProject(scope)
        && scope.workspaceId === selection.workspaceId
          ? [scope.projectId]
          : []
      )))
  const repositories = selection.organizationId === null
    || selection.workspaceId === null
    || selection.projectId === null
    ? []
    : uniqueSorted(scopes.flatMap(scope => (
        scope.kind === 'repository'
        && scope.organizationId === selection.organizationId
        && scope.workspaceId === selection.workspaceId
        && scope.projectId === selection.projectId
          ? [scope.repositoryId]
          : []
      )))
  return Object.freeze({ organizations, workspaces, projects, repositories })
}

function exactScope(
  scopes: readonly Scope[],
  selection: ScopeRouteSelection,
): Scope | null {
  const expectedKind = selection.repositoryId !== null
    ? 'repository'
    : selection.projectId !== null
      ? 'project'
      : selection.workspaceId !== null
        ? 'workspace'
        : selection.organizationId !== null
          ? 'organization'
          : null
  if (expectedKind === null) return null
  return scopes.find(scope => (
    scope.kind === expectedKind && matchesSelection(scope, selection)
  )) ?? null
}

export function scopeSelectionFromHash(hash: string): ScopeRouteSelection {
  const queryIndex = hash.indexOf('?')
  if (queryIndex < 0) return emptySelection()
  const parameters = new URLSearchParams(hash.slice(queryIndex + 1))
  return Object.freeze({
    organizationId: parameters.get('organizationId') as OrganizationId | null,
    workspaceId: parameters.get('workspaceId') as WorkspaceId | null,
    projectId: parameters.get('projectId') as ProjectId | null,
    repositoryId: parameters.get('repositoryId') as RepositoryId | null,
  })
}

/** Preserve route-owned entity IDs while replacing only the exact Scope path. */
export function scopeHash(
  hash: string,
  selection: ScopeRouteSelection | CompleteRepositorySelection,
): string {
  const queryIndex = hash.indexOf('?')
  const path = queryIndex < 0 ? hash : hash.slice(0, queryIndex)
  const parameters = new URLSearchParams(queryIndex < 0 ? '' : hash.slice(queryIndex + 1))
  for (const name of SCOPE_PARAMETER_NAMES) {
    const value = selection[name]
    if (value === null) parameters.delete(name)
    else parameters.set(name, value)
  }
  const query = parameters.toString()
  return query.length === 0 ? path : `${path}?${query}`
}

/**
 * Resolve one current Scope exclusively from the URL and current AuthSession facts.
 * A parent identity is an option only when an authorized Scope explicitly carries it.
 */
export function resolveScopeContext(
  scopes: readonly Scope[],
  hash: string,
  requirement: 'scope' | 'repository',
): ScopeContextResolution {
  const selection = scopeSelectionFromHash(hash)
  const options = scopeSelectionOptions(scopes, selection)
  if (!selectionIsStructured(selection)) {
    return Object.freeze({ status: 'denied', reason: 'invalid-route', selection, options })
  }
  if (scopes.length === 0) {
    return Object.freeze({ status: 'empty', reason: 'no-compatible', selection, options })
  }

  if (selectionIsEmpty(selection)) {
    const candidates = requirement === 'repository'
      ? scopes.filter((scope): scope is RepositoryScope => scope.kind === 'repository')
      : (() => {
          const organizations = scopes.filter(scope => scope.kind === 'organization')
          return organizations.length > 0 ? organizations : scopes
        })()
    if (candidates.length === 0) {
      return Object.freeze({ status: 'empty', reason: 'no-compatible', selection, options })
    }
    if (candidates.length === 1) {
      const scope = candidates[0] as Scope
      return Object.freeze({
        status: 'selected',
        source: 'only-compatible',
        scope,
        selection: scopeSelection(scope),
        options: scopeSelectionOptions(scopes, scopeSelection(scope)),
      })
    }
    return Object.freeze({
      status: 'selection-required',
      reason: 'multiple-compatible',
      selection,
      options,
    })
  }

  const matching = scopes.filter(scope => matchesSelection(scope, selection))
  if (matching.length === 0) {
    return Object.freeze({ status: 'denied', reason: 'not-authorized', selection, options })
  }
  const exact = exactScope(scopes, selection)
  if (exact !== null && (requirement === 'scope' || exact.kind === 'repository')) {
    return Object.freeze({ status: 'selected', source: 'url', scope: exact, selection, options })
  }
  return Object.freeze({ status: 'selection-required', reason: 'partial', selection, options })
}

/** Keep only the current exact Scope when changing product areas. */
export function surfaceHash(path: string, selection: ScopeRouteSelection): string {
  return scopeHash(`#${path}`, selection)
}
