import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync('corepack', [
  'pnpm',
  'exec',
  'tsc',
  '-p',
  'apps/client/tsconfig.ui-components-tests.json',
  '--pretty',
  'false',
  '--incremental',
  'false',
], { cwd: root, encoding: 'utf8' })
assert.equal(
  compiler.status,
  0,
  `Query cache ViewModel seam did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cacheModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/ui-components-tests/core/query-cache.js',
)).href}`)
const settingsModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/ui-components-tests/settings-view-model.js',
)).href}`)

const { createQueryCache } = cacheModule
const { createSettingsViewModel } = settingsModule
const actor = Object.freeze({ kind: 'user', id: 'usr_00000000000000000000000001' })
const scope = Object.freeze({
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
})

function deferred() {
  let resolvePromise
  const promise = new Promise(resolve => { resolvePromise = resolve })
  return { promise, resolve: resolvePromise }
}

async function waitFor(predicate, label) {
  const deadline = Date.now() + 2_000
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error(`Timed out waiting for ${label}.`)
    await new Promise(resolvePromise => { setTimeout(resolvePromise, 5) })
  }
}

test('Settings public lifecycle coalesces invalidations and reloads after reconnect', async () => {
  const queryCounts = new Map()
  let settingsRevision = 1
  let eventGate = null
  let subscriptionOptions = null
  let reconnects = 0
  let subscriptionCloses = 0
  const rawClient = {
    serverUrl: 'https://control.localhost',
    async restore() { throw new Error('not used') },
    async login() { throw new Error('not used') },
    async logout() {},
    async command() { throw new Error('not used') },
    async query(request) {
      queryCounts.set(request.query, (queryCounts.get(request.query) ?? 0) + 1)
      if (request.query === 'settings.get' && eventGate !== null) await eventGate.promise
      const result = request.query === 'settings.get'
        ? {
            revision: settingsRevision,
            defaultModelRoute: null,
            workerConcurrencyLimit: 2,
          }
        : { kind: 'credential_reference_page', items: [] }
      return Object.freeze({
        schemaVersion: request.schemaVersion,
        requestId: request.requestId,
        query: request.query,
        result: Object.freeze(result),
        page: Object.freeze({ hasMore: false, nextCursor: null }),
      })
    },
    subscribe(options) {
      subscriptionOptions = options
      return {
        cursor: null,
        resume() {},
        reconnect() { reconnects += 1 },
        close() { subscriptionCloses += 1 },
      }
    },
    close() {},
  }
  const cache = createQueryCache({ client: rawClient })
  let requestSequence = 0
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
  const observedStates = []
  const unsubscribe = model.subscribe(state => { observedStates.push(state) })

  await model.start()
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.settings.revision, 1)
  assert.deepEqual(Object.fromEntries(queryCounts), {
    'credential.reference.list': 1,
    'settings.get': 1,
  })

  settingsRevision = 2
  eventGate = deferred()
  const frame = {
    type: 'event.v1',
    subscriptionId: 'sub_00000000000000000000000001',
    authorizationEpoch: 3,
    scope,
    stream: { kind: 'scope' },
    event: { type: 'activity.recorded.v1' },
  }
  const invalidations = Array.from({ length: 40 }, async () => subscriptionOptions.onEvent(frame))
  assert.equal(model.state.status, 'refreshing')
  assert.equal(model.state.settings.revision, 1)
  assert.equal(queryCounts.get('settings.get'), 2)
  eventGate.resolve()
  await Promise.all(invalidations)
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'subscribed')
  assert.equal(model.state.settings.revision, 2)
  assert.ok(queryCounts.get('settings.get') <= 3)
  assert.ok(queryCounts.get('credential.reference.list') <= 2)

  eventGate = null
  settingsRevision = 3
  await model.refresh()
  assert.equal(model.state.settings.revision, 3)

  settingsRevision = 4
  model.reconnect()
  assert.equal(reconnects, 1)
  await waitFor(() => model.state.status === 'ready' && model.state.settings?.revision === 4, 'reconnect reload')
  assert.equal(model.state.realtime, 'subscribed')
  assert.ok(observedStates.some(state => state.realtime === 'reconnecting'))

  unsubscribe()
  model.close()
  assert.equal(model.state.status, 'closed')
  assert.equal(subscriptionCloses, 1)
  cache.close()
})
