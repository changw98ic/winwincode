import { mountWinWinCodeClient } from '/module/application.js'
import { ControlPlaneClientError } from '/module/control-plane-client.js'

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const repositoryOne = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const repositoryTwo = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000002',
  workspaceId: 'wsp_00000000000000000000000002',
  projectId: 'prj_00000000000000000000000002',
  repositoryId: 'rep_00000000000000000000000002',
}
let authorizedScopes = [repositoryOne, repositoryTwo]
let metadataFailure = null
const queries = []
const subscriptions = []

function session() {
  return {
    schemaVersion,
    expiresAt: '2099-09-02T00:00:00.000Z',
    actor,
    authorizedScopes,
  }
}

function response(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: { hasMore: false, nextCursor: null },
  }
}

function metadataError(kind) {
  return new ControlPlaneClientError({
    kind,
    code: kind === 'authorization' ? 'PERMISSION_DENIED' : 'NETWORK_ERROR',
    message: 'private metadata diagnostics',
    requestId: null,
    retryable: kind === 'network',
  })
}

const controlPlane = {
  serverUrl: 'https://control.localhost',
  async restore() { return structuredClone(session()) },
  async login() { return structuredClone(session()) },
  async logout() {},
  async command() { throw new Error('unexpected command') },
  async query(request) {
    queries.push(structuredClone(request))
    if (
      metadataFailure !== null
      && (request.query === 'enterprise.organization.list'
        || request.query === 'enterprise.project.list')
    ) throw metadataError(metadataFailure)
    if (request.query === 'enterprise.organization.list') return response(request, {
      kind: 'enterprise_organization_page',
      snapshotRevision: 1,
      items: [{
        id: repositoryOne.organizationId,
        displayName: 'Acme',
        slug: 'acme',
        state: 'active',
        revision: 1,
        updatedAt: '2026-09-02T00:00:00.000Z',
      }, {
        id: repositoryTwo.organizationId,
        displayName: 'Beta',
        slug: 'beta',
        state: 'active',
        revision: 1,
        updatedAt: '2026-09-02T00:00:00.000Z',
      }],
    })
    if (request.query === 'enterprise.project.list') {
      const selected = request.scope.organizationId === repositoryOne.organizationId
        ? repositoryOne
        : repositoryTwo
      return response(request, {
        kind: 'enterprise_project_repository_page',
        snapshotRevision: 1,
        items: [{
          kind: 'project',
          projectId: selected.projectId,
          displayName: selected === repositoryOne ? 'Core' : 'Workbench',
          repositoryCount: 1,
          state: 'active',
          revision: 1,
          updatedAt: '2026-09-02T00:00:00.000Z',
        }, {
          kind: 'repository',
          projectId: selected.projectId,
          repositoryId: selected.repositoryId,
          displayName: selected === repositoryOne ? 'Server' : 'Client',
          defaultBranch: 'main',
          state: 'active',
          revision: 1,
          updatedAt: '2026-09-02T00:00:00.000Z',
        }],
      })
    }
    if (request.query === 'settings.get') return response(request, {
      revision: 1,
      defaultModelRoute: null,
      workerConcurrencyLimit: 2,
    })
    if (request.query === 'credential.reference.list') return response(request, {
      kind: 'credential_reference_page',
      items: [],
    })
    throw new Error(`unexpected query: ${request.query}`)
  },
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

const root = document.querySelector('[data-winwincode-client-root]')
const application = mountWinWinCodeClient({
  root,
  serverUrl: controlPlane.serverUrl,
  controlPlane,
})

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise(resolve => { setTimeout(resolve, 20) })
  }
  throw new Error(`timed out waiting for ${label}`)
}

function control(level) {
  return document.querySelector(`#wwc-scope-${level}`)
}

async function choose(level, value) {
  const selector = control(level)
  selector.value = value
  selector.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor(() => new URLSearchParams(location.hash.split('?')[1] ?? '').get(
    `${level}Id`,
  ) === value, `${level} selection`)
}

function selectorState() {
  const labels = [...document.querySelectorAll('.wwc-scope-selector-label')]
  return {
    accessRole: document.querySelector('.wwc-scope-selector-access')?.getAttribute('role') ?? null,
    accessText: document.querySelector('.wwc-scope-selector-access')?.textContent ?? '',
    ariaBusy: document.querySelector('.wwc-scope-selector')?.getAttribute('aria-busy') ?? null,
    labels: labels.map(label => label.textContent),
    values: {
      organization: control('organization')?.value ?? null,
      workspace: control('workspace')?.value ?? null,
      project: control('project')?.value ?? null,
      repository: control('repository')?.value ?? null,
    },
    disabled: {
      organization: control('organization')?.disabled ?? null,
      workspace: control('workspace')?.disabled ?? null,
      project: control('project')?.disabled ?? null,
      repository: control('repository')?.disabled ?? null,
    },
    status: document.querySelector('.wwc-scope-selector-status')?.textContent ?? '',
    retryVisible: document.querySelector('.wwc-scope-selector-retry')?.hidden === false,
  }
}

async function chooseRepository(scope) {
  await choose('organization', scope.organizationId)
  await choose('workspace', scope.workspaceId)
  await choose('project', scope.projectId)
  await choose('repository', scope.repositoryId)
  await waitFor(() => queries.some(query => (
    query.query === 'settings.get' && query.scope.repositoryId === scope.repositoryId
  )), 'repository settings')
}

globalThis.scopeSelectorReady = () => true

globalThis.runScopeSelection = async () => {
  await waitFor(() => application.authSession.state.status === 'signed-in', 'session restore')
  await waitFor(() => control('organization') !== null, 'Scope selector')
  const initial = selectorState()
  const initialProductReads = queries.filter(query => query.query === 'settings.get').length
  await chooseRepository(repositoryTwo)
  await waitFor(() => subscriptions.length > 0, 'settings subscription')
  const selected = selectorState()
  return {
    hash: location.hash,
    initial,
    initialProductReads,
    selected,
    selectedProductScopes: queries
      .filter(query => query.query === 'settings.get')
      .map(query => query.scope),
  }
}

globalThis.switchScopeWithNetworkFailure = async () => {
  const oldSubscription = subscriptions.at(-1).handle
  metadataFailure = 'network'
  await choose('organization', repositoryOne.organizationId)
  await waitFor(() => oldSubscription.closed, 'old Scope subscription close')
  await waitFor(() => selectorState().retryVisible, 'network retry state')
  return {
    featureVisible: document.querySelector('.wwc-settings') !== null,
    oldSubscriptionClosed: oldSubscription.closed,
    state: selectorState(),
  }
}

globalThis.restoreSecondRepository = async () => {
  metadataFailure = null
  await chooseRepository(repositoryTwo)
  return { hash: location.hash }
}

globalThis.inspectRestoredScope = async () => {
  await waitFor(() => application.authSession.state.status === 'signed-in', 'restored session')
  await waitFor(() => subscriptions.length > 0, 'restored subscription')
  return {
    hash: location.hash,
    selected: selectorState(),
    settingsScope: queries.find(query => query.query === 'settings.get')?.scope ?? null,
  }
}

globalThis.revokeRestoredScope = async () => {
  const oldSubscription = subscriptions.at(-1).handle
  const before = queries.filter(query => query.query === 'settings.get').length
  authorizedScopes = [repositoryOne]
  await application.authSession.restore()
  await waitFor(() => oldSubscription.closed, 'revoked subscription close')
  await waitFor(() => selectorState().accessRole === 'alert', 'revoked Scope alert')
  return {
    afterProductReads: queries.filter(query => query.query === 'settings.get').length,
    beforeProductReads: before,
    oldSubscriptionClosed: oldSubscription.closed,
    state: selectorState(),
  }
}
