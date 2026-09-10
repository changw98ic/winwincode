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
  updateContextStatus(status: ScopeSelectorPageOptions['contextStatus']): void
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
  placeholder.textContent = `选择${label}`
  const nodes = options.map(option => {
    const node = document.createElement('option')
    node.value = option.id
    node.textContent = option.label
    return node
  })
  select.replaceChildren(placeholder, ...nodes)
  select.value = selected ?? ''
}

function statusMessage(state: ScopeSelectorViewModelState): string {
  if (state.status === 'loading') return '正在加载授权范围名称…'
  if (state.status === 'permission-denied') {
    return '部分范围名称不可用，仍可选择确切的授权范围。'
  }
  if (state.status === 'network-error') {
    return '范围名称刷新失败，请检查网络后重试。'
  }
  if (state.status === 'error') return '范围名称加载失败。'
  if (state.status === 'closed') return '范围选择器已关闭。'
  if (state.emptyLevel === 'organization') return '没有可用的授权组织。'
  if (state.emptyLevel === 'workspace') return '该组织下没有已授权的工作区。'
  if (state.emptyLevel === 'project') return '该工作区下没有已授权的项目。'
  if (state.emptyLevel === 'repository') return '该项目下没有已授权的仓库。'
  return '为此浏览器标签页选择确切的工作范围。'
}

function accessMessage(status: ScopeSelectorPageOptions['contextStatus']): string {
  if (status === 'denied') {
    return 'URL 中的范围已不再被授权，请选择确切的范围。'
  }
  if (status === 'empty') return '此区域没有兼容的授权范围。'
  if (status === 'selection-required') return '请先选择范围，再加载此区域。'
  return '当前范围'
}

/** Mount one accessible four-level selector backed only by its view-model facts. */
export function mountScopeSelectorPage(options: ScopeSelectorPageOptions): ScopeSelectorPage {
  const document = options.root.ownerDocument
  const region = element(document, 'section', 'wwc-scope-selector')
  // Design 03b: the Scope switcher renders as one compact disclosure; the
  // four-level form stays in the DOM but collapsed until explicitly opened.
  const compact = element(document, 'button', 'wwc-scope-selector-compact')
  const heading = element(document, 'h2', 'wwc-scope-selector-heading')
  const access = element(document, 'p', 'wwc-scope-selector-access')
  const controls = element(document, 'div', 'wwc-scope-selector-controls')
  const organization = field(document, 'wwc-scope-organization', '组织')
  const workspace = field(document, 'wwc-scope-workspace', '工作区')
  const project = field(document, 'wwc-scope-project', '项目')
  const repository = field(document, 'wwc-scope-repository', '仓库')
  const status = element(document, 'p', 'wwc-scope-selector-status')
  const retry = element(document, 'button', 'wwc-scope-selector-retry')
  let closed = false

  region.setAttribute('aria-label', '当前范围')
  heading.textContent = 'Scope'
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  retry.type = 'button'
  retry.textContent = '重新加载名称'
  controls.append(organization.root, workspace.root, project.root, repository.root)
  compact.type = 'button'
  compact.setAttribute('aria-expanded', 'false')
  compact.textContent = '项目范围 ▾'
  compact.addEventListener('click', () => {
    const expanded = compact.getAttribute('aria-expanded') === 'true'
    compact.setAttribute('aria-expanded', expanded ? 'false' : 'true')
    heading.hidden = !expanded
    controls.hidden = !expanded
    status.hidden = !expanded
    retry.hidden = !expanded
  })
  heading.hidden = true
  controls.hidden = true
  status.hidden = true
  region.append(compact, heading, access, controls, status, retry)
  options.root.replaceChildren(region)

  function updateContextStatus(
    contextStatus: ScopeSelectorPageOptions['contextStatus'],
  ): void {
    if (closed) return
    access.setAttribute('role', contextStatus === 'denied' ? 'alert' : 'status')
    access.textContent = accessMessage(contextStatus)
    access.hidden = contextStatus === 'selected'
  }

  updateContextStatus(options.contextStatus)

  function render(state: ScopeSelectorViewModelState): void {
    if (closed) return
    updateOptions(
      document,
      organization.select,
      state.options.organizations,
      state.selection.organizationId,
      '组织',
    )
    updateOptions(
      document,
      workspace.select,
      state.options.workspaces,
      state.selection.workspaceId,
      '工作区',
    )
    updateOptions(
      document,
      project.select,
      state.options.projects,
      state.selection.projectId,
      '项目',
    )
    updateOptions(
      document,
      repository.select,
      state.options.repositories,
      state.selection.repositoryId,
      '仓库',
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
    // Compact label mirrors the selected repository option's display name.
    const selected = Array.isArray(repository.select.options)
      ? repository.select.options.find(option => option.value === repository.select.value)
      : undefined
    if (selected !== undefined && selected.textContent) {
      compact.textContent = `${selected.textContent} ▾`
    }
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
    updateContextStatus,
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
