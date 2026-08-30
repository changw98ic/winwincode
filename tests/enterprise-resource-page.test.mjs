import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.enterprise-resource-page-tests.json',
    '--pretty',
    'false',
    '--incremental',
    'false',
  ],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `Enterprise resource page did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cacheRoot = resolve(root, '.cache/enterprise-resource-page-tests')
const viewModelModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'enterprise-management-view-model.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'enterprise-resource-page.js',
)).href}`)
const facadeModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'control-plane-client.js',
)).href}`)
const { createEnterpriseManagementViewModel } = viewModelModule
const { mountEnterpriseResourcePage } = pageModule
const { ControlPlaneClientError } = facadeModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'organization',
  organizationId: 'org_00000000000000000000000001',
}
const subscriptionId = 'sub_00000000000000000000000001'

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function organization(overrides = {}) {
  return {
    id: scope.organizationId,
    displayName: 'Example Organization',
    slug: 'example',
    state: 'active',
    revision: 2,
    updatedAt: '2026-08-27T00:00:00.000Z',
    ...overrides,
  }
}

function membership(value, overrides = {}) {
  const roleId = canonicalId('rol', value)
  return {
    id: canonicalId('mbr', value),
    organizationId: scope.organizationId,
    actorId: canonicalId('usr', value + 10),
    displayName: `Member ${String(value)}`,
    state: 'active',
    teamIds: [],
    roleAssignments: [{
      roleId,
      roleVersion: 1,
      scope,
      scopeMode: 'descendants',
      notBefore: null,
      expiresAt: null,
    }],
    revision: 2,
    updatedAt: '2026-08-27T00:00:00.000Z',
    ...overrides,
  }
}

function project(overrides = {}) {
  return {
    kind: 'project',
    projectId: canonicalId('prj', 1),
    displayName: 'Core Project',
    state: 'active',
    repositoryCount: 1,
    revision: 2,
    updatedAt: '2026-08-27T00:00:00.000Z',
    ...overrides,
  }
}

function repository(overrides = {}) {
  return {
    kind: 'repository',
    projectId: canonicalId('prj', 1),
    repositoryId: canonicalId('rep', 1),
    displayName: 'Core Repository',
    state: 'active',
    defaultBranch: 'main',
    revision: 2,
    updatedAt: '2026-08-27T00:00:00.000Z',
    ...overrides,
  }
}

const areaByQuery = Object.freeze({
  'enterprise.organization.list': 'organization',
  'enterprise.membership.list': 'members',
  'enterprise.project.list': 'projects',
  'enterprise.policy.list': 'policy',
  'enterprise.fleet.list': 'fleet',
  'enterprise.usage.list': 'usage',
  'enterprise.audit.list': 'audit',
  'enterprise.integration.list': 'integration',
})

const kindByArea = Object.freeze({
  organization: 'enterprise_organization_page',
  members: 'enterprise_membership_page',
  projects: 'enterprise_project_repository_page',
  policy: 'enterprise_policy_page',
  fleet: 'enterprise_fleet_page',
  usage: 'enterprise_usage_page',
  audit: 'enterprise_audit_page',
  integration: 'enterprise_integration_page',
})

function contractFake() {
  const queries = []
  const commands = []
  const subscriptions = []
  const revisions = Object.fromEntries(Object.values(areaByQuery).map(area => [area, 2]))
  const allowed = Object.fromEntries(Object.values(areaByQuery).map(area => [area, true]))
  let organizations = [organization()]
  let members = [membership(1), membership(2, {
    roleAssignments: [{
      roleId: canonicalId('rol', 1),
      roleVersion: 1,
      scope,
      scopeMode: 'descendants',
      notBefore: null,
      expiresAt: null,
    }],
  })]
  let projectResources = [project(), repository()]
  let commandFailure = null

  function items(area, cursor) {
    if (area === 'organization') return organizations
    if (area === 'projects') return projectResources
    if (area !== 'members') return []
    return cursor === null ? members.slice(0, 1) : members.slice(1)
  }

  return {
    queries,
    commands,
    subscriptions,
    revisions,
    allowed,
    set commandFailure(value) { commandFailure = value },
    async query(request) {
      const area = areaByQuery[request.query]
      assert.notEqual(area, undefined)
      queries.push(structuredClone(request))
      if (!allowed[area]) throw new ControlPlaneClientError({
        kind: 'authorization',
        code: 'PERMISSION_DENIED',
        message: 'raw server permission detail',
        requestId: request.requestId,
        retryable: false,
      })
      const pagedMembers = area === 'members'
      const firstPage = pagedMembers && request.page.cursor === null
      return {
        schemaVersion,
        requestId: request.requestId,
        query: request.query,
        result: {
          kind: kindByArea[area],
          snapshotRevision: revisions[area],
          items: items(area, request.page.cursor),
        },
        page: firstPage
          ? { hasMore: true, nextCursor: 'members_second_page' }
          : { hasMore: false, nextCursor: null },
      }
    },
    async command(request) {
      commands.push(structuredClone(request))
      if (commandFailure !== null) throw commandFailure
      let area
      let result
      if (request.command === 'enterprise.organization.update') {
        area = 'organization'
        result = organization({
          id: request.payload.organizationId,
          displayName: request.payload.displayName,
          slug: request.payload.slug,
          state: request.payload.state,
        })
        organizations = [
          ...organizations.filter(item => item.id !== result.id),
          result,
        ]
      } else if (request.command === 'enterprise.membership.update') {
        area = 'members'
        const prior = members.find(item => item.id === request.payload.membershipId)
        result = membership(9, {
          ...prior,
          id: request.payload.membershipId,
          actorId: request.payload.actorId,
          displayName: request.payload.displayName,
          teamIds: request.payload.teamIds,
          roleAssignments: request.payload.roleAssignments,
          state: request.payload.state,
        })
        members = [...members.filter(item => item.id !== result.id), result]
      } else {
        assert.equal(request.command, 'enterprise.project_repository.update')
        area = 'projects'
        result = request.payload.kind === 'project'
          ? project({
              projectId: request.payload.projectId,
              displayName: request.payload.displayName,
              state: request.payload.state,
            })
          : repository({
              projectId: request.payload.projectId,
              repositoryId: request.payload.repositoryId,
              displayName: request.payload.displayName,
              state: request.payload.state,
            })
        projectResources = [
          ...projectResources.filter(item => (
            item.kind !== result.kind
            || (item.kind === 'project' && item.projectId !== result.projectId)
            || (item.kind === 'repository' && item.repositoryId !== result.repositoryId)
          )),
          result,
        ]
      }
      revisions[area] += 1
      result = { ...result, revision: revisions[area] }
      if (area === 'organization') {
        organizations = organizations.map(item => item.id === result.id ? result : item)
      } else if (area === 'members') {
        members = members.map(item => item.id === result.id ? result : item)
      } else {
        projectResources = projectResources.map(item => (
          item.kind === result.kind
          && (item.kind === 'project'
            ? item.projectId === result.projectId
            : item.repositoryId === result.repositoryId)
            ? result
            : item
        ))
      }
      return {
        schemaVersion,
        requestId: request.requestId,
        command: request.command,
        outcome: 'completed',
        previousRevision: request.expectedRevision,
        currentRevision: revisions[area],
        result,
      }
    },
    subscribe(options) {
      subscriptions.push(options)
      return { cursor: null, resume() {}, reconnect() {}, close() {} }
    },
    close() {},
    serverUrl: 'https://control.example/enterprise',
  }
}

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  required = false
  type = ''
  value = ''
  #textContent = ''

  get textContent() { return this.#textContent }

  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }

  append(...children) { this.children.push(...children) }

  replaceChildren(...children) { this.children = [...children] }

  setAttribute(name, value) { this.attributes.set(name, String(value)) }

  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }

  dispatch(name) {
    const event = { preventDefault() {} }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) { return new FakeElement(this, tagName) }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function byClass(rootElement, className) {
  const match = descendants(rootElement).find(node => node.className === className)
  assert.notEqual(match, undefined, `missing .${className}`)
  return match
}

function visibleText(node) {
  return descendants(node).map(current => current.textContent).join(' ')
}

async function waitFor(predicate, label) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (predicate()) return
    await new Promise(resolvePromise => { setImmediate(resolvePromise) })
  }
  assert.fail(`timed out waiting for ${label}`)
}

function createMountedFixture(client = contractFake()) {
  let nextRequest = 0
  const model = createEnterpriseManagementViewModel({
    client,
    actor,
    scope,
    subscriptionId,
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
  })
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const mounted = mountEnterpriseResourcePage({ root: rootElement, model })
  return { client, model, mounted, rootElement }
}

test('browser fake renders paged resources, role groups, access state, and accessible regions', async () => {
  const client = contractFake()
  client.allowed.organization = false
  const fixture = createMountedFixture(client)
  await waitFor(() => fixture.model.state.status !== 'loading', 'initial enterprise snapshot')

  assert.equal(fixture.model.state.status, 'partial')
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-organization-fields').disabled, true)
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-member-fields').disabled, false)
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-project-fields').disabled, false)
  assert.equal(
    client.queries.filter(request => request.query === 'enterprise.membership.list').length,
    2,
  )
  const text = visibleText(fixture.rootElement)
  assert.equal(text.includes('Enterprise resources and access'), true)
  assert.equal(text.includes('Organization data is not available for your current role.'), true)
  assert.equal(text.includes('Member 1'), true)
  assert.equal(text.includes('Member 2'), true)
  assert.equal(text.includes('Role …000001 · 2 members'), true)
  assert.equal(text.includes('Core Project'), true)
  assert.equal(text.includes('Core Repository'), true)
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-resources-status').attributes.get('role'), 'status')
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-resources-error').attributes.get('role'), 'alert')
  fixture.mounted.close()
})

test('browser fake creates, updates, archives, disables, assigns roles, and retries conflicts', async () => {
  const fixture = createMountedFixture()
  await waitFor(() => fixture.model.state.status === 'ready', 'initial enterprise snapshot')

  byClass(fixture.rootElement, 'wwc-enterprise-member-edit').dispatch('click')
  const roles = byClass(fixture.rootElement, 'wwc-enterprise-member-roles')
  roles.value = `${canonicalId('rol', 1)}@1, ${canonicalId('rol', 2)}@2`
  byClass(fixture.rootElement, 'wwc-enterprise-member-form').dispatch('submit')
  await waitFor(() => fixture.client.commands.length === 1
    && fixture.model.state.areas.members.revision === 3
    && fixture.model.state.areas.members.status === 'ready', 'member role update')
  assert.deepEqual(
    fixture.client.commands[0].payload.roleAssignments.map(assignment => ({
      roleId: assignment.roleId,
      roleVersion: assignment.roleVersion,
    })),
    [
      { roleId: canonicalId('rol', 1), roleVersion: 1 },
      { roleId: canonicalId('rol', 2), roleVersion: 2 },
    ],
  )
  assert.equal(fixture.client.commands[0].expectedRevision, 2)

  fixture.client.commandFailure = new ControlPlaneClientError({
    kind: 'server',
    code: 'REVISION_CONFLICT',
    message: 'raw conflicting member and private diagnostics',
    requestId: null,
    retryable: false,
  })
  byClass(fixture.rootElement, 'wwc-enterprise-member-disable').dispatch('click')
  await waitFor(
    () => fixture.model.state.interaction.status === 'revision-conflict',
    'member revision conflict',
  )
  assert.equal(
    byClass(fixture.rootElement, 'wwc-enterprise-resources-error-text').textContent,
    'These enterprise resources changed before the update was saved. Review the current snapshot and try again.',
  )
  assert.equal(visibleText(fixture.rootElement).includes('private diagnostics'), false)

  fixture.client.commandFailure = null
  byClass(fixture.rootElement, 'wwc-enterprise-member-disable').dispatch('click')
  await waitFor(() => fixture.client.commands.length === 3
    && fixture.model.state.areas.members.status === 'ready', 'member disable retry')
  assert.equal(fixture.client.commands[2].payload.state, 'disabled')
  assert.equal(fixture.client.commands[2].expectedRevision, 3)

  byClass(fixture.rootElement, 'wwc-enterprise-organization-id').value = canonicalId('org', 2)
  byClass(fixture.rootElement, 'wwc-enterprise-organization-name').value = 'Second Organization'
  byClass(fixture.rootElement, 'wwc-enterprise-organization-slug').value = 'second'
  byClass(fixture.rootElement, 'wwc-enterprise-organization-state').value = 'active'
  byClass(fixture.rootElement, 'wwc-enterprise-organization-form').dispatch('submit')
  await waitFor(() => fixture.client.commands.length === 4
    && fixture.model.state.areas.organization.revision === 3
    && fixture.model.state.areas.organization.status === 'ready', 'organization create')
  assert.equal(fixture.client.commands[3].command, 'enterprise.organization.update')
  assert.equal(fixture.client.commands[3].payload.organizationId, canonicalId('org', 2))
  assert.equal(visibleText(fixture.rootElement).includes('Second Organization'), true)

  byClass(fixture.rootElement, 'wwc-enterprise-project-archive').dispatch('click')
  await waitFor(() => fixture.client.commands.length === 5
    && fixture.model.state.areas.projects.revision === 3
    && fixture.model.state.areas.projects.status === 'ready', 'project archive')
  assert.equal(fixture.client.commands[4].command, 'enterprise.project_repository.update')
  assert.equal(fixture.client.commands[4].payload.state, 'archived')
  fixture.mounted.close()
})

test('enterprise resource page has one view-model boundary and no raw transport or secret sink', () => {
  const source = readFileSync(
    resolve(root, 'apps/client/src/enterprise-resource-page.ts'),
    'utf8',
  )
  assert.equal((source.match(/\.\/enterprise-management-view-model\.js/gu) ?? []).length, 1)
  assert.doesNotMatch(source, /\.\/control-plane-client\.js/u)
  assert.doesNotMatch(source, /\bfetch\s*\(|new\s+WebSocket/u)
  assert.doesNotMatch(source, /innerHTML|console\.|localStorage|sessionStorage/u)
  assert.doesNotMatch(source, /secretMaterial|accessToken|apiKey|credentialValue/u)
  assert.match(source, /model\.execute\('organization'/u)
  assert.match(source, /model\.execute\('members'/u)
  assert.match(source, /model\.execute\('projects'/u)
})
