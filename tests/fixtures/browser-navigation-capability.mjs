import { mountWinWinCodeClient } from '/module/application.js'

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const repositoryScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const mode = new URL(location.href).searchParams.get('mode') ?? 'personal'
const session = {
  schemaVersion,
  expiresAt: '2099-09-02T00:00:00.000Z',
  actor,
  authorizedScopes: [repositoryScope],
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
  async restore() { return structuredClone(session) },
  async login() { return structuredClone(session) },
  async logout() {},
  async query(request) {
    if (request.query === 'session.list') {
      return response(request, { kind: 'product_session_page', items: [] })
    }
    throw new Error(`unexpected query: ${request.query}`)
  },
  async command() { throw new Error('unexpected command') },
  subscribe() {
    throw new Error('unexpected subscribe')
  },
  close() {},
}

const navigationCapabilities = mode === 'disabled'
  ? { surfaceAccess: { chat: 'denied' } }
  : mode === 'read-only'
    ? { surfaceAccess: { chat: 'read-only' } }
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

globalThis.tryDisabledChatEntry = async () => {
  const entry = document.querySelector('[data-surface="chat"]')
  const before = location.hash
  entry.click()
  await Promise.resolve()
  return { after: location.hash, before, state: navigationState() }
}
