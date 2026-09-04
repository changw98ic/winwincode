import { createQueryCache } from '/module/core/query-cache.js'
import { mountSettingsPage } from '/module/settings-page.js'
import { createSettingsViewModel } from '/module/settings-view-model.js'

const root = document.querySelector('[data-winwincode-client-root]')
const actor = Object.freeze({ kind: 'user', id: 'usr_00000000000000000000000001' })
const scope = Object.freeze({
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
})
const queryCounts = new Map()
let settingsRevision = 1
let reloadGate = null
let subscriptionOptions = null
let requestSequence = 0

function deferred() {
  let resolvePromise
  const promise = new Promise(resolve => { resolvePromise = resolve })
  return { promise, resolve: resolvePromise }
}

function queryResponse(request, result) {
  return Object.freeze({
    schemaVersion: request.schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result: Object.freeze(result),
    page: Object.freeze({ hasMore: false, nextCursor: null }),
  })
}

const rawClient = {
  serverUrl: 'https://control.localhost',
  async restore() { throw new Error('not used') },
  async login() { throw new Error('not used') },
  async logout() {},
  async command() { throw new Error('not used') },
  async query(request) {
    queryCounts.set(request.query, (queryCounts.get(request.query) ?? 0) + 1)
    if (request.query === 'settings.get' && reloadGate !== null) await reloadGate.promise
    if (request.query === 'settings.get') return queryResponse(request, {
      revision: settingsRevision,
      defaultModelRoute: null,
      workerConcurrencyLimit: 2,
    })
    return queryResponse(request, { kind: 'credential_reference_page', items: [] })
  },
  subscribe(options) {
    subscriptionOptions = options
    return { cursor: null, resume() {}, reconnect() {}, close() {} }
  },
  close() {},
}

const cache = createQueryCache({ client: rawClient })
const model = createSettingsViewModel({
  client: cache.client,
  actor,
  scope,
  subscriptionId: 'sub_00000000000000000000000001',
  nextRequestId() {
    requestSequence += 1
    return `req_${String(requestSequence).padStart(26, '0')}`
  },
})
const mounted = mountSettingsPage({ root, model })

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error(`Timed out waiting for ${label}.`)
    await new Promise(resolve => { setTimeout(resolve, 5) })
  }
}

globalThis.runQueryCacheScenario = async () => {
  await waitFor(() => model.state.status === 'ready' && subscriptionOptions !== null, 'initial Settings snapshot')
  const provider = document.querySelector('.wwc-settings-provider')
  provider.value = 'draft-provider'
  provider.dispatchEvent(new Event('input', { bubbles: true }))
  provider.selectionStart = 5
  provider.selectionEnd = 5
  provider.focus()
  const settingsQueriesBefore = queryCounts.get('settings.get') ?? 0
  const credentialQueriesBefore = queryCounts.get('credential.reference.list') ?? 0

  settingsRevision = 2
  reloadGate = deferred()
  const frames = Array.from({ length: 100 }, (_, index) => ({
    type: 'event.v1',
    subscriptionId: 'sub_00000000000000000000000001',
    authorizationEpoch: 7,
    sequence: index + 1,
    scope,
    stream: { kind: 'scope' },
    event: { type: 'activity.recorded.v1' },
  }))
  subscriptionOptions.onEventQueued?.(frames[0])
  const firstInvalidation = subscriptionOptions.onEvent(frames[0])
  for (const frame of frames.slice(1)) subscriptionOptions.onEventQueued?.(frame)
  const duringReload = {
    ariaBusy: document.querySelector('[data-wwc-page="management"]')?.getAttribute('aria-busy'),
    draft: provider.value,
    focused: document.activeElement === provider,
    revision: model.state.settings?.revision ?? null,
  }
  reloadGate.resolve()
  await firstInvalidation
  for (const frame of frames.slice(1)) await subscriptionOptions.onEvent(frame)
  reloadGate = null
  await waitFor(() => model.state.status === 'ready' && model.state.settings?.revision === 2, 'coalesced snapshot')
  return {
    afterReload: {
      draft: provider.value,
      focused: document.activeElement === provider,
      realtime: model.state.realtime,
      revision: model.state.settings?.revision ?? null,
      selectionStart: provider.selectionStart,
      status: model.state.status,
    },
    duringReload,
    queryDelta: {
      credentials: (queryCounts.get('credential.reference.list') ?? 0) - credentialQueriesBefore,
      settings: (queryCounts.get('settings.get') ?? 0) - settingsQueriesBefore,
    },
  }
}

globalThis.closeQueryCacheFixture = () => {
  mounted.close()
  cache.close()
}
