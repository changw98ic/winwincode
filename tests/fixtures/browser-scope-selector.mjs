import { mountWinWinCodeClient } from '/module/application.js'

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

const controlPlane = {
  serverUrl: 'https://control.localhost',
  async restore() { return structuredClone(session()) },
  async login() { return structuredClone(session()) },
  async logout() {},
  async command() { throw new Error('unexpected command') },
  async query(request) {
    queries.push(structuredClone(request))
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
  selector.focus()
  selector.value = value
  selector.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor(() => new URLSearchParams(location.hash.split('?')[1] ?? '').get(
    `${level}Id`,
  ) === value, `${level} selection`)
  return {
    activeId: document.activeElement?.id ?? null,
    bodyHasFocus: document.activeElement === document.body,
    connected: selector.isConnected,
    insideSelector: document.querySelector('.wwc-scope-selector')?.contains(
      document.activeElement,
    ) ?? false,
    sameNode: control(level) === selector,
  }
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
  const focusTransitions = {
    organization: await choose('organization', scope.organizationId),
    workspace: await choose('workspace', scope.workspaceId),
    project: await choose('project', scope.projectId),
    repository: await choose('repository', scope.repositoryId),
  }
  await waitFor(() => queries.some(query => (
    query.query === 'settings.get' && query.scope.repositoryId === scope.repositoryId
  )), 'repository settings')
  return focusTransitions
}

globalThis.scopeSelectorReady = () => true

globalThis.runScopeSelection = async () => {
  await waitFor(() => application.authSession.state.status === 'signed-in', 'session restore')
  await waitFor(() => control('organization') !== null, 'Scope selector')
  const initial = selectorState()
  const initialProductReads = queries.filter(query => query.query === 'settings.get').length
  const focusTransitions = await chooseRepository(repositoryTwo)
  await waitFor(() => subscriptions.length > 0, 'settings subscription')
  const selected = selectorState()
  return {
    hash: location.hash,
    initial,
    initialProductReads,
    focusTransitions,
    selected,
    selectedProductScopes: queries
      .filter(query => query.query === 'settings.get')
      .map(query => query.scope),
  }
}

globalThis.restoreSecondRepository = async () => {
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
