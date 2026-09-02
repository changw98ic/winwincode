// SPDX-License-Identifier: Apache-2.0

import type {
  OrganizationId,
  ProjectId,
  RepositoryId,
  WorkspaceId,
} from './generated/contracts.js'
import type {
  ScopeSelectorOption,
  ScopeSelectorViewModel,
  ScopeSelectorViewModelState,
} from './scope-selector-view-model.js'

export interface ScopeSelectorPageOptions {
  readonly root: HTMLElement
  readonly model: ScopeSelectorViewModel
  readonly contextStatus: 'selected' | 'selection-required' | 'empty' | 'denied'
}

export interface ScopeSelectorPage {
  close(): void
}

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

function field(
  document: Document,
  id: string,
  labelText: string,
): { readonly root: HTMLDivElement, readonly select: HTMLSelectElement } {
  const root = element(document, 'div', 'wwc-scope-selector-field')
  const label = element(document, 'label', 'wwc-scope-selector-label')
  const select = element(document, 'select', 'wwc-scope-selector-control')
  select.id = id
  label.htmlFor = id
  label.textContent = labelText
  root.append(label, select)
  return { root, select }
}

function updateOptions(
  document: Document,
  select: HTMLSelectElement,
  options: readonly ScopeSelectorOption[],
  selected: string | null,
  label: string,
): void {
  const placeholder = document.createElement('option')
  placeholder.value = ''
  placeholder.textContent = `Choose ${label}`
  const nodes = options.map(option => {
    const node = document.createElement('option')
    node.value = option.id
    node.textContent = option.label === option.id
      ? option.id
      : `${option.label} — ${option.id}`
    return node
  })
  select.replaceChildren(placeholder, ...nodes)
  select.value = selected ?? ''
}

function statusMessage(state: ScopeSelectorViewModelState): string {
  if (state.status === 'loading') return 'Loading authorized Scope names…'
  if (state.status === 'permission-denied') {
    return 'Some Scope names are unavailable for this identity. Exact authorized Scope IDs remain selectable.'
  }
  if (state.status === 'network-error') {
    return 'Scope names could not be refreshed. Check the network and retry.'
  }
  if (state.status === 'error') return 'Scope names could not be loaded.'
  if (state.status === 'closed') return 'Scope selector closed.'
  if (state.emptyLevel === 'organization') return 'No authorized organizations are available.'
  if (state.emptyLevel === 'workspace') return 'No authorized workspaces exist in this organization.'
  if (state.emptyLevel === 'project') return 'No authorized projects exist in this workspace.'
  if (state.emptyLevel === 'repository') return 'No authorized repositories exist in this project.'
  return 'Choose the exact Scope for this browser tab.'
}

function accessMessage(status: ScopeSelectorPageOptions['contextStatus']): string {
  if (status === 'denied') {
    return 'The Scope in this URL is no longer authorized. Choose an exact authorized Scope.'
  }
  if (status === 'empty') return 'This product area has no compatible authorized Scope.'
  if (status === 'selection-required') return 'Choose a Scope before this product area can load.'
  return 'Current Scope'
}

/** Mount one accessible four-level selector backed only by its view-model facts. */
export function mountScopeSelectorPage(options: ScopeSelectorPageOptions): ScopeSelectorPage {
  const document = options.root.ownerDocument
  const region = element(document, 'section', 'wwc-scope-selector')
  const heading = element(document, 'h2', 'wwc-scope-selector-heading')
  const access = element(document, 'p', 'wwc-scope-selector-access')
  const controls = element(document, 'div', 'wwc-scope-selector-controls')
  const organization = field(document, 'wwc-scope-organization', 'Organization')
  const workspace = field(document, 'wwc-scope-workspace', 'Workspace')
  const project = field(document, 'wwc-scope-project', 'Project')
  const repository = field(document, 'wwc-scope-repository', 'Repository')
  const status = element(document, 'p', 'wwc-scope-selector-status')
  const retry = element(document, 'button', 'wwc-scope-selector-retry')
  let closed = false

  region.setAttribute('aria-label', 'Current Scope')
  heading.textContent = 'Scope'
  access.setAttribute('role', options.contextStatus === 'denied' ? 'alert' : 'status')
  access.textContent = accessMessage(options.contextStatus)
  access.hidden = options.contextStatus === 'selected'
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  retry.type = 'button'
  retry.textContent = 'Retry Scope names'
  controls.append(organization.root, workspace.root, project.root, repository.root)
  region.append(heading, access, controls, status, retry)
  options.root.replaceChildren(region)

  function render(state: ScopeSelectorViewModelState): void {
    if (closed) return
    updateOptions(
      document,
      organization.select,
      state.options.organizations,
      state.selection.organizationId,
      'organization',
    )
    updateOptions(
      document,
      workspace.select,
      state.options.workspaces,
      state.selection.workspaceId,
      'workspace',
    )
    updateOptions(
      document,
      project.select,
      state.options.projects,
      state.selection.projectId,
      'project',
    )
    updateOptions(
      document,
      repository.select,
      state.options.repositories,
      state.selection.repositoryId,
      'repository',
    )
    const selectorClosed = state.status === 'closed'
    organization.select.disabled = selectorClosed || state.options.organizations.length === 0
    workspace.select.disabled = selectorClosed
      || state.selection.organizationId === null
      || state.options.workspaces.length === 0
    project.select.disabled = selectorClosed
      || state.selection.workspaceId === null
      || state.options.projects.length === 0
    repository.select.disabled = selectorClosed
      || state.selection.projectId === null
      || state.options.repositories.length === 0
    region.setAttribute('aria-busy', state.status === 'loading' ? 'true' : 'false')
    status.textContent = statusMessage(state)
    retry.hidden = state.status !== 'network-error'
  }

  const unsubscribe = options.model.subscribe(render)
  const onOrganization = () => {
    if (organization.select.value.length === 0) return
    void options.model.selectOrganization(organization.select.value as OrganizationId)
  }
  const onWorkspace = () => {
    if (workspace.select.value.length === 0) return
    void options.model.selectWorkspace(workspace.select.value as WorkspaceId)
  }
  const onProject = () => {
    if (project.select.value.length === 0) return
    void options.model.selectProject(project.select.value as ProjectId)
  }
  const onRepository = () => {
    if (repository.select.value.length === 0) return
    void options.model.selectRepository(repository.select.value as RepositoryId)
  }
  const onRetry = () => { void options.model.retry() }
  organization.select.addEventListener('change', onOrganization)
  workspace.select.addEventListener('change', onWorkspace)
  project.select.addEventListener('change', onProject)
  repository.select.addEventListener('change', onRepository)
  retry.addEventListener('click', onRetry)

  return {
    close() {
      if (closed) return
      closed = true
      organization.select.removeEventListener('change', onOrganization)
      workspace.select.removeEventListener('change', onWorkspace)
      project.select.removeEventListener('change', onProject)
      repository.select.removeEventListener('change', onRepository)
      retry.removeEventListener('click', onRetry)
      unsubscribe()
      options.model.close()
      options.root.replaceChildren()
    },
  }
}
