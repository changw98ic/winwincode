import { mountWinWinCodeClient } from '/module/application.js'
import { ControlPlaneClientError } from '/module/control-plane-client.js'

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const repositoryScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const organizationScope = {
  kind: 'organization',
  organizationId: repositoryScope.organizationId,
}
const mode = new URL(location.href).searchParams.get('mode') ?? 'personal'
const enterpriseDeployment = mode !== 'personal'
const queries = []
const subscriptions = []
const scopes = enterpriseDeployment
  ? [organizationScope, repositoryScope]
  : [repositoryScope]
const session = {
  schemaVersion,
  expiresAt: '2099-09-02T00:00:00.000Z',
  actor,
  authorizedScopes: scopes,
}
const areaByQuery = Object.freeze({
  'enterprise.organization.list': 'enterprise_organization_page',
  'enterprise.membership.list': 'enterprise_membership_page',
  'enterprise.project.list': 'enterprise_project_repository_page',
  'enterprise.policy.list': 'enterprise_policy_page',
  'enterprise.fleet.list': 'enterprise_fleet_page',
  'enterprise.usage.list': 'enterprise_usage_page',
  'enterprise.audit.list': 'enterprise_audit_page',
  'enterprise.integration.list': 'enterprise_integration_page',
})

function response(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: { hasMore: false, nextCursor: null },
  }
}

const controlPlane = {
  serverUrl: 'https://control.localhost',
  async restore() { return structuredClone(session) },
  async login() { return structuredClone(session) },
  async logout() {},
  async query(request) {
    queries.push(structuredClone(request))
    if (request.query === 'session.list') {
      return response(request, { kind: 'product_session_page', items: [] })
    }
    const kind = areaByQuery[request.query]
    if (kind !== undefined) {
      return response(request, { kind, snapshotRevision: 1, items: [] })
    }
    throw new Error(`unexpected query: ${request.query}`)
  },
  async command() { throw new Error('unexpected command') },
  subscribe(options) {
    const handle = {
      cursor: null,
      closed: false,
      resume() {},
      reconnect() {},
      close() { this.closed = true },
    }
    subscriptions.push({ options, handle })
    return handle
  },
  close() {},
}

const navigationCapabilities = mode === 'disabled'
  ? { deployment: 'enterprise', surfaceAccess: { enterprise: 'denied' } }
  : mode === 'read-only'
    ? { deployment: 'enterprise', surfaceAccess: { enterprise: 'read-only' } }
    : {}
const root = document.querySelector('[data-winwincode-client-root]')
const application = mountWinWinCodeClient({
  root,
  serverUrl: controlPlane.serverUrl,
  controlPlane,
  navigationCapabilities,
})

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise(resolve => { setTimeout(resolve, 20) })
  }
  throw new Error(`timed out waiting for ${label}`)
}

function navigationState() {
  const entries = [...document.querySelectorAll('.wwc-navigation-link')]
  return {
    deployment: document.querySelector('.wwc-navigation')?.dataset.deployment ?? null,
    entries: Object.fromEntries(entries.map(entry => [entry.dataset.surface, {
      ariaDisabled: entry.getAttribute('aria-disabled'),
      capability: entry.dataset.capability,
      label: entry.textContent,
      tabIndex: entry.tabIndex,
    }])),
    mode,
  }
}

globalThis.navigationMode = mode
globalThis.inspectNavigationCapability = async () => {
  await waitFor(() => application.authSession.state.status === 'signed-in', 'session restore')
  await waitFor(() => document.querySelector('.wwc-navigation')?.dataset.deployment !== undefined,
    'navigation projection')
  return navigationState()
}

globalThis.openDeniedEnterpriseRoute = async () => {
  const enterpriseQueriesBefore = queries.filter(query => query.query.startsWith('enterprise.')).length
  location.hash = '#/enterprise/resources'
  await waitFor(() => document.querySelector('.wwc-surface-route-denied') !== null,
    'Enterprise route denial')
  const denial = document.querySelector('.wwc-surface-route-denied')
  const safeEntry = denial.querySelector('.wwc-surface-route-safe-entry')
  safeEntry.focus()
  return {
    alertRole: denial.getAttribute('role'),
    enterpriseQueries: queries.filter(query => query.query.startsWith('enterprise.')).length
      - enterpriseQueriesBefore,
    focused: document.activeElement === safeEntry,
    safeHref: safeEntry.getAttribute('href'),
    text: denial.textContent,
  }
}

globalThis.tryDisabledEnterpriseEntry = async () => {
  const entry = document.querySelector('[data-surface="enterprise"]')
  const before = location.hash
  entry.click()
  await Promise.resolve()
  return { after: location.hash, before, state: navigationState() }
}

globalThis.revokeEnterpriseRoute = async () => {
  location.hash = '#/enterprise/resources'
  await waitFor(() => subscriptions.length > 0, 'Enterprise subscription')
  const subscription = subscriptions.at(-1).handle
  application.authSession.authenticationRequired(new ControlPlaneClientError({
    kind: 'authentication',
    code: 'AUTHENTICATION_REQUIRED',
    message: 'private revoked navigation payload',
    requestId: null,
    retryable: false,
  }))
  await waitFor(() => subscription.closed, 'revoked subscription cleanup')
  await waitFor(() => document.querySelectorAll('.wwc-navigation-link').length === 0,
    'revoked navigation cleanup')
  return {
    routeText: document.querySelector('.wwc-enterprise-context-required')?.textContent ?? '',
    subscriptionClosed: subscription.closed,
    visibleEntries: document.querySelectorAll('.wwc-navigation-link').length,
  }
}

globalThis.revokeEnterpriseSubscription = async () => {
  location.hash = '#/enterprise/resources'
  await waitFor(() => subscriptions.length > 0, 'Enterprise subscription')
  const subscription = subscriptions.at(-1)
  await subscription.options.onAuthorizationRevoked(null)
  await waitFor(() => subscription.handle.closed, 'revoked subscription cleanup')
  await waitFor(() => document.querySelector('.wwc-surface-route-safe-entry') !== null,
    'shell safe entry')
  const entry = document.querySelector('[data-surface="enterprise"]')
  return {
    capability: entry?.dataset.capability ?? null,
    routeAccess: entry?.dataset.routeAccess ?? null,
    safeHref: document.querySelector('.wwc-surface-route-safe-entry')?.getAttribute('href') ?? null,
    subscriptionClosed: subscription.handle.closed,
  }
}
