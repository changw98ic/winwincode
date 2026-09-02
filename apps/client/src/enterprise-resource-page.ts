// SPDX-License-Identifier: Apache-2.0

import {
  mountButton,
  mountErrorState,
  mountPageHeader,
  mountPanel,
  mountStatusBadge,
  type StatusTone,
} from './components/index.js'
import type {
  CommandRequest,
  ActorId,
  EnterpriseMembershipId,
  EnterpriseMembershipProjection,
  EnterpriseMembershipUpdatePayload,
  EnterpriseOrganizationProjection,
  EnterpriseOrganizationUpdatePayload,
  EnterpriseProjectProjection,
  EnterpriseProjectRepositoryUpdatePayload,
  EnterpriseRepositoryProjection,
  EnterpriseRoleId,
  EnterpriseTeamId,
  OrganizationId,
  OrganizationScope,
  ProjectId,
  RepositoryId,
} from './generated/contracts.js'
import type {
  EnterpriseManagementCommandContext,
  EnterpriseManagementViewModel,
  EnterpriseManagementViewModelState,
} from './enterprise-management-view-model.js'

export interface EnterpriseResourcePageOptions {
  readonly root: HTMLElement
  readonly model: EnterpriseManagementViewModel
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
}

export interface EnterpriseResourcePage {
  close(): void
}

type EnterpriseResourceArea = 'organization' | 'members' | 'projects'

export interface EnterpriseRoleGroup {
  readonly roleId: EnterpriseRoleId
  readonly memberCount: number
}

export interface EnterpriseResourceSnapshot {
  readonly organizations: readonly EnterpriseOrganizationProjection[]
  readonly members: readonly EnterpriseMembershipProjection[]
  readonly projects: readonly EnterpriseProjectProjection[]
  readonly repositories: readonly EnterpriseRepositoryProjection[]
  /** Teams are a read-only grouping of the canonical member role assignments. */
  readonly roleGroups: readonly EnterpriseRoleGroup[]
}

export interface EnterpriseResourcePagePresentation {
  readonly statusText: string
  readonly errorText: string | null
  readonly busy: boolean
  readonly retryVisible: boolean
  readonly reconnectVisible: boolean
  readonly mutationsDisabled: Readonly<Record<EnterpriseResourceArea, boolean>>
}

function visibleError(
  state: EnterpriseManagementViewModelState,
): EnterpriseManagementViewModelState['error'] {
  return state.interaction.error
    ?? state.error
    ?? state.areas.organization.error
    ?? state.areas.members.error
    ?? state.areas.projects.error
}

function errorLabel(error: EnterpriseManagementViewModelState['error']): string | null {
  if (error === null) return null
  if (error.code === 'REVISION_CONFLICT') {
    return 'These enterprise resources changed before the update was saved. Review the current snapshot and try again.'
  }
  if (error.code === 'ENTERPRISE_MANAGEMENT_SNAPSHOT_STALE') {
    return 'The update was saved, but this page is still waiting for the new revision. Refresh again.'
  }
  if (error.code === 'ENTERPRISE_MANAGEMENT_PERMISSION_REQUIRED') {
    return 'Your current role does not allow this enterprise resource change.'
  }
  if (error.kind === 'authentication') return 'Sign in again to manage enterprise resources.'
  if (error.kind === 'authorization') return 'Your current role does not allow this enterprise resource operation.'
  if (error.kind === 'network') return 'The enterprise Control Plane could not be reached. Check the connection and retry.'
  if (error.kind === 'version') return 'The Client and Server versions differ. Update the Client and retry.'
  if (error.kind === 'cancelled') return 'The enterprise resource update was cancelled.'
  if (error.kind === 'configuration' || error.code === 'INVALID_CLIENT_REQUEST') {
    return 'Check the signed-in identity and enterprise scope, then retry.'
  }
  return 'Enterprise resources could not be updated. Refresh the current snapshot and retry.'
}

function mutationDisabled(
  state: EnterpriseManagementViewModelState,
  area: EnterpriseResourceArea,
  busy: boolean,
): boolean {
  const current = state.areas[area]
  return busy
    || current.permission !== 'allowed'
    || current.revision === null
    || current.status === 'permission-denied'
    || state.status === 'authentication-required'
    || state.status === 'authorization-denied'
    || state.status === 'closed'
}

export function enterpriseResourcePagePresentation(
  state: EnterpriseManagementViewModelState,
): EnterpriseResourcePagePresentation {
  const error = visibleError(state)
  const busy = state.status === 'loading'
    || state.realtime === 'reloading'
    || state.interaction.status === 'submitting'
    || state.interaction.status === 'waiting'
  const statusText = state.interaction.status === 'submitting'
    ? 'Saving enterprise resource…'
    : state.interaction.status === 'waiting'
      ? 'Change accepted · waiting for the current snapshot…'
      : state.status === 'loading'
        ? 'Loading enterprise resources…'
        : state.realtime === 'reloading'
          ? 'Updating enterprise resources…'
          : state.realtime === 'reconnecting'
            ? 'Reconnecting…'
            : state.status === 'authentication-required'
              ? 'Sign in required'
              : state.status === 'authorization-denied'
                ? 'Access denied'
                : state.status === 'cancelled'
                  ? 'Update cancelled'
                  : state.status === 'error'
                    ? 'Enterprise resources unavailable'
                    : state.status === 'closed'
                      ? 'Enterprise resources closed'
                      : 'Enterprise resources ready'
  return Object.freeze({
    statusText,
    errorText: errorLabel(error),
    busy,
    retryVisible: error !== null && state.realtime !== 'reconnecting',
    reconnectVisible: state.realtime === 'reconnecting',
    mutationsDisabled: Object.freeze({
      organization: mutationDisabled(state, 'organization', busy),
      members: mutationDisabled(state, 'members', busy),
      projects: mutationDisabled(state, 'projects', busy),
    }),
  })
}

export function enterpriseResourceSnapshot(
  state: EnterpriseManagementViewModelState,
): EnterpriseResourceSnapshot {
  const organizations = state.areas.organization.pages.flatMap(page => (
    page.query === 'enterprise.organization.list' ? page.result.items : []
  ))
  const members = state.areas.members.pages.flatMap(page => (
    page.query === 'enterprise.membership.list' ? page.result.items : []
  ))
  const projectResources = state.areas.projects.pages.flatMap(page => (
    page.query === 'enterprise.project.list' ? page.result.items : []
  ))
  const projects = projectResources.filter(
    (item): item is EnterpriseProjectProjection => item.kind === 'project',
  )
  const repositories = projectResources.filter(
    (item): item is EnterpriseRepositoryProjection => item.kind === 'repository',
  )
  const roleCounts = new Map<EnterpriseRoleId, number>()
  for (const member of members) {
    for (const assignment of member.roleAssignments) {
      roleCounts.set(assignment.roleId, (roleCounts.get(assignment.roleId) ?? 0) + 1)
    }
  }
  const roleGroups = [...roleCounts.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([roleId, memberCount]) => Object.freeze({ roleId, memberCount }))
  return Object.freeze({
    organizations: Object.freeze(organizations),
    members: Object.freeze(members),
    projects: Object.freeze(projects),
    repositories: Object.freeze(repositories),
    roleGroups: Object.freeze(roleGroups),
  })
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

function enterprisePanel(
  document: Document,
  id: string,
  title: string,
  description: string,
  className: string,
) {
  const panel = mountPanel({
    document,
    props: { id, title, description, className },
  })
  panel.title.className = 'wwc-enterprise-section-heading'
  return panel
}

function labelledInput(
  document: Document,
  id: string,
  labelText: string,
  className: string,
): { readonly label: HTMLLabelElement; readonly input: HTMLInputElement } {
  const label = element(document, 'label', `${className}-label`)
  const input = element(document, 'input', className)
  label.htmlFor = id
  label.textContent = labelText
  input.id = id
  input.type = 'text'
  input.required = true
  input.autocomplete = 'off'
  label.append(input)
  return Object.freeze({ label, input })
}

function labelledSelect(
  document: Document,
  id: string,
  labelText: string,
  className: string,
  values: readonly (readonly [string, string])[],
): { readonly label: HTMLLabelElement; readonly select: HTMLSelectElement } {
  const label = element(document, 'label', `${className}-label`)
  const select = element(document, 'select', className)
  label.htmlFor = id
  label.textContent = labelText
  select.id = id
  for (const [value, text] of values) {
    const option = document.createElement('option')
    option.value = value
    option.textContent = text
    select.append(option)
  }
  label.append(select)
  return Object.freeze({ label, select })
}

function shortId(value: string): string {
  return `…${value.slice(-6)}`
}

function areaLabel(
  state: EnterpriseManagementViewModelState,
  area: EnterpriseResourceArea,
): string {
  const current = state.areas[area]
  if (current.permission === 'denied') return 'Access denied for this section.'
  if (current.status === 'loading' || current.status === 'refreshing') return 'Updating this section…'
  if (current.status === 'revision-conflict') return 'This section changed. Review it before retrying.'
  if (current.status === 'error') return 'This section is unavailable. Refresh and retry.'
  return current.revision === null ? 'No current snapshot.' : `Revision ${String(current.revision)}`
}

function organizationCommand(
  context: EnterpriseManagementCommandContext,
  payload: EnterpriseOrganizationUpdatePayload,
): CommandRequest {
  return {
    schemaVersion: 'winwincode/v1',
    actor: context.actor,
    scope: context.scope,
    requestId: context.requestId,
    command: 'enterprise.organization.update',
    expectedRevision: context.expectedRevision,
    payload,
  }
}

function membershipCommand(
  context: EnterpriseManagementCommandContext,
  payload: EnterpriseMembershipUpdatePayload,
): CommandRequest {
  const scope = organizationScope(context.scope)
  return {
    schemaVersion: 'winwincode/v1',
    actor: context.actor,
    scope,
    requestId: context.requestId,
    command: 'enterprise.membership.update',
    expectedRevision: context.expectedRevision,
    payload,
  }
}

function organizationScope(scope: EnterpriseManagementCommandContext['scope']): OrganizationScope {
  if (scope.kind !== 'organization') {
    throw new Error('Membership management requires an organization scope.')
  }
  return scope
}

function projectCommand(
  context: EnterpriseManagementCommandContext,
  payload: EnterpriseProjectRepositoryUpdatePayload,
): CommandRequest {
  return {
    schemaVersion: 'winwincode/v1',
    actor: context.actor,
    scope: context.scope,
    requestId: context.requestId,
    command: 'enterprise.project_repository.update',
    expectedRevision: context.expectedRevision,
    payload,
  }
}

/** Mount organization, identity, team/role, project, and repository management. */
export function mountEnterpriseResourcePage(
  options: EnterpriseResourcePageOptions,
): EnterpriseResourcePage {
  const document = options.root.ownerDocument
  const layout = element(document, 'main', 'wwc-enterprise-resources')
  layout.dataset.wwcPage = 'management'
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: 'Enterprise resources and access',
      eyebrow: 'Enterprise administration',
      description: 'Manage organizations, members, role assignments, projects, and repositories.',
      headingLevel: 1,
      className: 'wwc-enterprise-resources-heading',
    },
  })
  const heading = pageHeader.root
  const statusBadge = mountStatusBadge({
    document,
    props: {
      label: 'Loading enterprise resources…',
      tone: 'info',
      live: 'polite',
      className: 'wwc-enterprise-resources-status',
    },
  })
  const status = statusBadge.root
  const retryButton = mountButton({
    document,
    props: {
      label: 'Retry snapshot',
      className: 'wwc-enterprise-resources-retry',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const retry = retryButton.root
  const reconnectButton = mountButton({
    document,
    props: {
      label: 'Reconnect events',
      className: 'wwc-enterprise-resources-reconnect',
      onActivate: () => { options.model.reconnect() },
    },
  })
  const reconnect = reconnectButton.root
  const errorState = mountErrorState({
    document,
    props: {
      title: 'Enterprise resources unavailable',
      message: '',
      actions: [retry, reconnect],
      visible: false,
      className: 'wwc-enterprise-resources-error',
    },
  })
  const error = errorState.root
  const errorText = errorState.message
  errorText.className = 'wwc-enterprise-resources-error-text'
  const organization = createOrganizationSection(document, options.model)
  const membership = createMembershipSection(document, options.model)
  const roles = createRoleSection(document)
  const projects = createProjectSection(document, options.model)
  let closed = false

  layout.append(
    heading,
    status,
    error,
    organization.section,
    membership.section,
    roles.section,
    projects.section,
  )
  options.root.replaceChildren(layout)

  function render(state: EnterpriseManagementViewModelState): void {
    if (closed) return
    const presentation = enterpriseResourcePagePresentation(state)
    const snapshot = enterpriseResourceSnapshot(state)
    const tone: StatusTone = presentation.errorText !== null
      ? 'danger'
      : state.realtime === 'reconnecting'
        ? 'warning'
        : presentation.busy
          ? 'info'
          : state.status === 'ready' || state.status === 'partial'
            ? 'success'
            : 'neutral'
    statusBadge.update({
      label: presentation.statusText,
      tone,
      live: 'polite',
      className: 'wwc-enterprise-resources-status',
    })
    layout.setAttribute('aria-busy', String(presentation.busy))
    errorState.update({
      title: 'Enterprise resources unavailable',
      message: presentation.errorText ?? '',
      actions: [retry, reconnect],
      visible: presentation.errorText !== null,
      className: 'wwc-enterprise-resources-error',
    })
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    organization.render(
      snapshot.organizations,
      state,
      options.readOnly === true || presentation.mutationsDisabled.organization,
    )
    membership.render(
      snapshot.members,
      state,
      options.readOnly === true || presentation.mutationsDisabled.members,
    )
    roles.render(snapshot.roleGroups, state)
    projects.render(
      snapshot.projects,
      snapshot.repositories,
      state,
      options.readOnly === true || presentation.mutationsDisabled.projects,
    )
  }

  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      options.model.close()
      projects.close()
      roles.close()
      membership.close()
      organization.close()
      reconnectButton.close()
      retryButton.close()
      errorState.close()
      statusBadge.close()
      pageHeader.close()
      options.root.replaceChildren()
    },
  }
}

interface OrganizationSection {
  readonly section: HTMLElement
  close(): void
  render(
    organizations: readonly EnterpriseOrganizationProjection[],
    state: EnterpriseManagementViewModelState,
    disabled: boolean,
  ): void
}

function createOrganizationSection(
  document: Document,
  model: EnterpriseManagementViewModel,
): OrganizationSection {
  const panel = enterprisePanel(
    document,
    'wwc-enterprise-organizations',
    'Organizations',
    'Organization identity and lifecycle in the current enterprise snapshot.',
    'wwc-enterprise-organizations',
  )
  const section = panel.root
  const heading = panel.title
  const areaStatus = element(document, 'p', 'wwc-enterprise-organization-status')
  const list = element(document, 'ul', 'wwc-enterprise-organization-list')
  const fieldset = element(document, 'fieldset', 'wwc-enterprise-organization-fields')
  const legend = element(document, 'legend', 'wwc-enterprise-form-heading')
  const form = element(document, 'form', 'wwc-enterprise-organization-form')
  const id = labelledInput(document, 'wwc-enterprise-organization-id', 'Organization ID', 'wwc-enterprise-organization-id')
  const name = labelledInput(document, 'wwc-enterprise-organization-name', 'Display name', 'wwc-enterprise-organization-name')
  const slug = labelledInput(document, 'wwc-enterprise-organization-slug', 'Slug', 'wwc-enterprise-organization-slug')
  const state = labelledSelect(document, 'wwc-enterprise-organization-state', 'State', 'wwc-enterprise-organization-state', [
    ['active', 'Active'],
    ['suspended', 'Suspended'],
    ['archived', 'Archived'],
  ])
  const save = element(document, 'button', 'wwc-enterprise-organization-save')
  heading.id = 'wwc-enterprise-organizations-heading'
  heading.textContent = 'Organizations'
  section.setAttribute('aria-labelledby', heading.id)
  areaStatus.setAttribute('aria-live', 'polite')
  list.setAttribute('aria-live', 'polite')
  legend.textContent = 'Create or update organization'
  id.input.pattern = 'org_[0-9A-HJKMNP-TV-Z]{26}'
  save.type = 'submit'
  save.textContent = 'Save organization'
  save.dataset.wwcComponent = 'button'
  save.dataset.variant = 'primary'
  form.addEventListener('submit', event => {
    event.preventDefault()
    const payload: EnterpriseOrganizationUpdatePayload = {
      organizationId: id.input.value as OrganizationId,
      displayName: name.input.value,
      slug: slug.input.value,
      state: state.select.value as EnterpriseOrganizationProjection['state'],
    }
    void model.execute('organization', context => (
      organizationCommand(context, payload)
    ))
  })
  form.append(id.label, name.label, slug.label, state.label, save)
  fieldset.append(legend, form)
  panel.content.append(areaStatus, list, fieldset)
  return {
    section,
    close() { panel.close() },
    render(organizations, viewState, disabled) {
      areaStatus.textContent = areaLabel(viewState, 'organization')
      fieldset.disabled = disabled
      list.replaceChildren(...organizations.map(item => {
        const row = element(document, 'li', 'wwc-enterprise-organization')
        const title = element(document, 'h3', 'wwc-enterprise-resource-title')
        const summary = element(document, 'p', 'wwc-enterprise-resource-summary')
        const edit = element(document, 'button', 'wwc-enterprise-organization-edit')
        const archive = element(document, 'button', 'wwc-enterprise-organization-archive')
        title.textContent = item.displayName
        summary.textContent = `${item.slug} · ${item.state} · ${shortId(item.id)} · revision ${String(item.revision)}`
        edit.type = 'button'
        edit.textContent = 'Edit organization'
        edit.dataset.wwcComponent = 'button'
        edit.dataset.variant = 'default'
        edit.disabled = disabled
        edit.addEventListener('click', () => {
          id.input.value = item.id
          name.input.value = item.displayName
          slug.input.value = item.slug
          state.select.value = item.state
        })
        archive.type = 'button'
        archive.textContent = item.state === 'archived' ? 'Organization archived' : 'Archive organization'
        archive.dataset.wwcComponent = 'button'
        archive.dataset.variant = 'destructive'
        archive.disabled = disabled || item.state === 'archived'
        archive.addEventListener('click', () => {
          void model.execute('organization', context => organizationCommand(context, {
            organizationId: item.id,
            displayName: item.displayName,
            slug: item.slug,
            state: 'archived',
          }))
        })
        row.append(title, summary, edit, archive)
        return row
      }))
      if (organizations.length === 0) {
        const empty = element(document, 'li', 'wwc-enterprise-organization-empty')
        empty.dataset.state = 'empty'
        empty.textContent = viewState.areas.organization.permission === 'denied'
          ? 'Organization data is not available for your current role.'
          : 'No organizations in the current snapshot.'
        list.append(empty)
      }
    },
  }
}

interface MembershipSection {
  readonly section: HTMLElement
  close(): void
  render(
    members: readonly EnterpriseMembershipProjection[],
    state: EnterpriseManagementViewModelState,
    disabled: boolean,
  ): void
}

function createMembershipSection(
  document: Document,
  model: EnterpriseManagementViewModel,
): MembershipSection {
  const panel = enterprisePanel(
    document,
    'wwc-enterprise-members',
    'Members',
    'Member state, teams, and versioned role assignments.',
    'wwc-enterprise-members',
  )
  const section = panel.root
  const heading = panel.title
  const areaStatus = element(document, 'p', 'wwc-enterprise-members-status')
  const list = element(document, 'ul', 'wwc-enterprise-member-list')
  const fieldset = element(document, 'fieldset', 'wwc-enterprise-member-fields')
  const legend = element(document, 'legend', 'wwc-enterprise-form-heading')
  const form = element(document, 'form', 'wwc-enterprise-member-form')
  const id = labelledInput(document, 'wwc-enterprise-member-id', 'Membership ID', 'wwc-enterprise-member-id')
  const actor = labelledInput(document, 'wwc-enterprise-member-actor', 'Actor ID', 'wwc-enterprise-member-actor')
  const displayName = labelledInput(document, 'wwc-enterprise-member-name', 'Display name', 'wwc-enterprise-member-name')
  const teams = labelledInput(document, 'wwc-enterprise-member-teams', 'Team IDs, separated by commas', 'wwc-enterprise-member-teams')
  const roles = labelledInput(document, 'wwc-enterprise-member-roles', 'Role IDs with version, for example rol_…@1', 'wwc-enterprise-member-roles')
  const state = labelledSelect(document, 'wwc-enterprise-member-state', 'Member state', 'wwc-enterprise-member-state', [
    ['invited', 'Invited'],
    ['active', 'Active'],
    ['disabled', 'Disabled'],
  ])
  const save = element(document, 'button', 'wwc-enterprise-member-save')
  heading.id = 'wwc-enterprise-members-heading'
  heading.textContent = 'Members'
  section.setAttribute('aria-labelledby', heading.id)
  areaStatus.setAttribute('aria-live', 'polite')
  list.setAttribute('aria-live', 'polite')
  legend.textContent = 'Add or update member and role assignment'
  id.input.pattern = 'mbr_[0-9A-HJKMNP-TV-Z]{26}'
  actor.input.pattern = '(usr|svc|sys)_[0-9A-HJKMNP-TV-Z]{26}'
  save.type = 'submit'
  save.textContent = 'Save member assignment'
  save.dataset.wwcComponent = 'button'
  save.dataset.variant = 'primary'
  form.addEventListener('submit', event => {
    event.preventDefault()
    const teamIds = [...new Set(
      teams.input.value.split(',').map(value => value.trim()).filter(Boolean),
    )] as EnterpriseTeamId[]
    const grants = [...new Set(
      roles.input.value.split(',').map(value => value.trim()).filter(Boolean),
    )].map(value => {
      const [roleId, rawVersion] = value.split('@')
      const roleVersion = Number(rawVersion)
      if (roleId === undefined || !Number.isSafeInteger(roleVersion) || roleVersion < 1) {
        throw new Error('Each role assignment requires a positive role version.')
      }
      return { roleId: roleId as EnterpriseRoleId, roleVersion }
    })
    void model.execute('members', context => {
      const scope = organizationScope(context.scope)
      return membershipCommand(context, {
        membershipId: id.input.value as EnterpriseMembershipId,
        actorId: actor.input.value as ActorId,
        displayName: displayName.input.value,
        state: state.select.value as EnterpriseMembershipProjection['state'],
        teamIds,
        roleAssignments: grants.map(grant => ({
          ...grant,
          scope,
          scopeMode: 'descendants',
          notBefore: null,
          expiresAt: null,
        })),
      })
    })
  })
  form.append(id.label, actor.label, displayName.label, teams.label, roles.label, state.label, save)
  fieldset.append(legend, form)
  panel.content.append(areaStatus, list, fieldset)
  return {
    section,
    close() { panel.close() },
    render(members, viewState, disabled) {
      areaStatus.textContent = areaLabel(viewState, 'members')
      fieldset.disabled = disabled
      list.replaceChildren(...members.map(member => {
        const row = element(document, 'li', 'wwc-enterprise-member')
        const title = element(document, 'h3', 'wwc-enterprise-resource-title')
        const summary = element(document, 'p', 'wwc-enterprise-resource-summary')
        const edit = element(document, 'button', 'wwc-enterprise-member-edit')
        const disable = element(document, 'button', 'wwc-enterprise-member-disable')
        title.textContent = member.displayName
        summary.textContent = `${member.state} · ${String(member.roleAssignments.length)} roles · ${shortId(member.id)} · revision ${String(member.revision)}`
        edit.type = 'button'
        edit.textContent = 'Edit role assignment'
        edit.dataset.wwcComponent = 'button'
        edit.dataset.variant = 'default'
        edit.disabled = disabled
        edit.addEventListener('click', () => {
          id.input.value = member.id
          actor.input.value = member.actorId
          displayName.input.value = member.displayName
          teams.input.value = member.teamIds.join(', ')
          roles.input.value = member.roleAssignments
            .map(assignment => `${assignment.roleId}@${String(assignment.roleVersion)}`)
            .join(', ')
          state.select.value = member.state
        })
        disable.type = 'button'
        disable.textContent = member.state === 'disabled' ? 'Member disabled' : 'Disable member'
        disable.dataset.wwcComponent = 'button'
        disable.dataset.variant = 'destructive'
        disable.disabled = disabled || member.state === 'disabled'
        disable.addEventListener('click', () => {
          void model.execute('members', context => membershipCommand(context, {
            membershipId: member.id,
            actorId: member.actorId,
            displayName: member.displayName,
            teamIds: member.teamIds,
            roleAssignments: member.roleAssignments,
            state: 'disabled',
          }))
        })
        row.append(title, summary, edit, disable)
        return row
      }))
      if (members.length === 0) {
        const empty = element(document, 'li', 'wwc-enterprise-member-empty')
        empty.dataset.state = 'empty'
        empty.textContent = viewState.areas.members.permission === 'denied'
          ? 'Membership data is not available for your current role.'
          : 'No members in the current snapshot.'
        list.append(empty)
      }
    },
  }
}

interface RoleSection {
  readonly section: HTMLElement
  close(): void
  render(
    roleGroups: readonly EnterpriseRoleGroup[],
    state: EnterpriseManagementViewModelState,
  ): void
}

function createRoleSection(document: Document): RoleSection {
  const panel = enterprisePanel(
    document,
    'wwc-enterprise-roles',
    'Teams and roles',
    'Read-only grouping from current member role assignments.',
    'wwc-enterprise-roles',
  )
  const section = panel.root
  const heading = panel.title
  const help = element(document, 'p', 'wwc-enterprise-role-help')
  const list = element(document, 'ul', 'wwc-enterprise-role-list')
  heading.id = 'wwc-enterprise-roles-heading'
  heading.textContent = 'Teams and roles'
  section.setAttribute('aria-labelledby', heading.id)
  help.textContent = 'Teams are grouped from current member role assignments. Assign roles from the Members section.'
  list.setAttribute('aria-live', 'polite')
  panel.content.append(help, list)
  return {
    section,
    close() { panel.close() },
    render(roleGroups, viewState) {
      list.replaceChildren(...roleGroups.map(group => {
        const row = element(document, 'li', 'wwc-enterprise-role')
        row.textContent = `Role ${shortId(group.roleId)} · ${String(group.memberCount)} members`
        return row
      }))
      if (roleGroups.length === 0) {
        const empty = element(document, 'li', 'wwc-enterprise-role-empty')
        empty.dataset.state = 'empty'
        empty.textContent = viewState.areas.members.permission === 'denied'
          ? 'Role assignments are not available for your current role.'
          : 'No role assignments in the current snapshot.'
        list.append(empty)
      }
    },
  }
}

interface ProjectSection {
  readonly section: HTMLElement
  close(): void
  render(
    projects: readonly EnterpriseProjectProjection[],
    repositories: readonly EnterpriseRepositoryProjection[],
    state: EnterpriseManagementViewModelState,
    disabled: boolean,
  ): void
}

function createProjectSection(
  document: Document,
  model: EnterpriseManagementViewModel,
): ProjectSection {
  const panel = enterprisePanel(
    document,
    'wwc-enterprise-projects',
    'Projects and repositories',
    'Project and repository identity, state, and current revision.',
    'wwc-enterprise-projects',
  )
  const section = panel.root
  const heading = panel.title
  const areaStatus = element(document, 'p', 'wwc-enterprise-project-status')
  const projectHeading = element(document, 'h3', 'wwc-enterprise-subsection-heading')
  const projectList = element(document, 'ul', 'wwc-enterprise-project-list')
  const repositoryHeading = element(document, 'h3', 'wwc-enterprise-subsection-heading')
  const repositoryList = element(document, 'ul', 'wwc-enterprise-repository-list')
  const fieldset = element(document, 'fieldset', 'wwc-enterprise-project-fields')
  const legend = element(document, 'legend', 'wwc-enterprise-form-heading')
  const form = element(document, 'form', 'wwc-enterprise-project-form')
  const kind = labelledSelect(document, 'wwc-enterprise-project-kind', 'Resource kind', 'wwc-enterprise-project-kind', [
    ['project', 'Project'],
    ['repository', 'Repository'],
  ])
  const projectId = labelledInput(document, 'wwc-enterprise-project-id', 'Project ID', 'wwc-enterprise-project-id')
  const repositoryId = labelledInput(document, 'wwc-enterprise-repository-id', 'Repository ID, for repository only', 'wwc-enterprise-repository-id')
  const name = labelledInput(document, 'wwc-enterprise-project-name', 'Display name', 'wwc-enterprise-project-name')
  const state = labelledSelect(document, 'wwc-enterprise-project-state', 'State', 'wwc-enterprise-project-state', [
    ['active', 'Active'],
    ['archived', 'Archived'],
  ])
  const save = element(document, 'button', 'wwc-enterprise-project-save')
  heading.id = 'wwc-enterprise-projects-heading'
  heading.textContent = 'Projects and repositories'
  section.setAttribute('aria-labelledby', heading.id)
  areaStatus.setAttribute('aria-live', 'polite')
  projectHeading.textContent = 'Projects'
  repositoryHeading.textContent = 'Repositories'
  projectList.setAttribute('aria-live', 'polite')
  repositoryList.setAttribute('aria-live', 'polite')
  legend.textContent = 'Create or update project or repository'
  projectId.input.pattern = 'prj_[0-9A-HJKMNP-TV-Z]{26}'
  repositoryId.input.required = false
  repositoryId.input.pattern = 'rep_[0-9A-HJKMNP-TV-Z]{26}'
  save.type = 'submit'
  save.textContent = 'Save project or repository'
  save.dataset.wwcComponent = 'button'
  save.dataset.variant = 'primary'
  form.addEventListener('submit', event => {
    event.preventDefault()
    const resourceKind = kind.select.value as 'project' | 'repository'
    const payload: EnterpriseProjectRepositoryUpdatePayload = {
      kind: resourceKind,
      displayName: name.input.value,
      projectId: projectId.input.value as ProjectId,
      repositoryId: resourceKind === 'repository'
        ? repositoryId.input.value as RepositoryId
        : null,
      state: state.select.value as 'active' | 'archived',
    }
    void model.execute('projects', context => projectCommand(context, payload))
  })
  form.append(kind.label, projectId.label, repositoryId.label, name.label, state.label, save)
  fieldset.append(legend, form)
  panel.content.append(
    areaStatus,
    projectHeading,
    projectList,
    repositoryHeading,
    repositoryList,
    fieldset,
  )
  function row(
    resource: EnterpriseProjectProjection | EnterpriseRepositoryProjection,
    disabled: boolean,
  ): HTMLLIElement {
    const item = element(document, 'li', resource.kind === 'project'
      ? 'wwc-enterprise-project'
      : 'wwc-enterprise-repository')
    const title = element(document, 'h4', 'wwc-enterprise-resource-title')
    const summary = element(document, 'p', 'wwc-enterprise-resource-summary')
    const edit = element(document, 'button', 'wwc-enterprise-project-edit')
    const archive = element(document, 'button', 'wwc-enterprise-project-archive')
    title.textContent = resource.displayName
    summary.textContent = resource.kind === 'project'
      ? `${resource.state} · ${String(resource.repositoryCount)} repositories · ${shortId(resource.projectId)} · revision ${String(resource.revision)}`
      : `${resource.state} · ${resource.defaultBranch} · ${shortId(resource.repositoryId)} · revision ${String(resource.revision)}`
    edit.type = 'button'
    edit.textContent = `Edit ${resource.kind}`
    edit.dataset.wwcComponent = 'button'
    edit.dataset.variant = 'default'
    edit.disabled = disabled
    edit.addEventListener('click', () => {
      kind.select.value = resource.kind
      projectId.input.value = resource.projectId
      repositoryId.input.value = resource.kind === 'repository' ? resource.repositoryId : ''
      name.input.value = resource.displayName
      state.select.value = resource.state
    })
    archive.type = 'button'
    archive.textContent = resource.state === 'archived'
      ? `${resource.kind === 'project' ? 'Project' : 'Repository'} archived`
      : `Archive ${resource.kind}`
    archive.dataset.wwcComponent = 'button'
    archive.dataset.variant = 'destructive'
    archive.disabled = disabled || resource.state === 'archived'
    archive.addEventListener('click', () => {
      void model.execute('projects', context => projectCommand(context, {
        kind: resource.kind,
        projectId: resource.projectId,
        repositoryId: resource.kind === 'repository' ? resource.repositoryId : null,
        displayName: resource.displayName,
        state: 'archived',
      }))
    })
    item.append(title, summary, edit, archive)
    return item
  }
  return {
    section,
    close() { panel.close() },
    render(projects, repositories, viewState, disabled) {
      areaStatus.textContent = areaLabel(viewState, 'projects')
      fieldset.disabled = disabled
      projectList.replaceChildren(...projects.map(item => row(item, disabled)))
      repositoryList.replaceChildren(...repositories.map(item => row(item, disabled)))
      if (projects.length === 0) {
        const empty = element(document, 'li', 'wwc-enterprise-project-empty')
        empty.dataset.state = 'empty'
        empty.textContent = viewState.areas.projects.permission === 'denied'
          ? 'Project data is not available for your current role.'
          : 'No projects in the current snapshot.'
        projectList.append(empty)
      }
      if (repositories.length === 0) {
        const empty = element(document, 'li', 'wwc-enterprise-repository-empty')
        empty.dataset.state = 'empty'
        empty.textContent = viewState.areas.projects.permission === 'denied'
          ? 'Repository data is not available for your current role.'
          : 'No repositories in the current snapshot.'
        repositoryList.append(empty)
      }
    },
  }
}
