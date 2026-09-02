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
  `Query cache did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const { createQueryCache, queryCacheKey } = await import(`${pathToFileURL(resolve(
  root,
  '.cache/ui-components-tests/core/query-cache.js',
)).href}`)

const actor = Object.freeze({ kind: 'user', id: 'usr_00000000000000000000000001' })
const otherActor = Object.freeze({ kind: 'user', id: 'usr_00000000000000000000000002' })
const scope = Object.freeze({
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
})
const otherScope = Object.freeze({ ...scope, repositoryId: 'rep_00000000000000000000000002' })

function request({
  requestId = 'req_00000000000000000000000001',
  requestActor = actor,
  requestScope = scope,
  parameters = { states: [] },
  page = { cursor: null, limit: 50 },
  query = 'session.list',
} = {}) {
  return Object.freeze({
    schemaVersion: 'winwincode/v1',
    requestId,
    actor: requestActor,
    scope: requestScope,
    query,
    parameters,
    page,
  })
}

function response(value, queryRequest) {
  return Object.freeze({
    schemaVersion: 'winwincode/v1',
    requestId: queryRequest.requestId,
    query: queryRequest.query,
    result: Object.freeze({ kind: 'product_session_page', items: Object.freeze([value]) }),
    page: Object.freeze({ nextCursor: null, hasMore: false }),
  })
}

function deferred() {
  let resolvePromise
  let rejectPromise
  const promise = new Promise((resolve, reject) => {
    resolvePromise = resolve
    rejectPromise = reject
  })
  return { promise, resolve: resolvePromise, reject: rejectPromise }
}

function fakeClient(queryImplementation) {
  let subscriptionOptions = null
  let reconnects = 0
  let resumes = 0
  let closes = 0
  return {
    client: {
      serverUrl: 'https://control.localhost',
      async restore() { return { actor, authorizedScopes: [scope] } },
      async login() { return { actor, authorizedScopes: [scope] } },
      async logout() {},
      async command(commandRequest) {
        return { requestId: commandRequest.requestId, outcome: 'completed' }
      },
      query: queryImplementation,
      subscribe(options) {
        subscriptionOptions = options
        return {
          cursor: null,
          resume() { resumes += 1 },
          reconnect() { reconnects += 1 },
          close() { closes += 1 },
        }
      },
      close() {},
    },
    get options() { return subscriptionOptions },
    get reconnects() { return reconnects },
    get resumes() { return resumes },
    get closes() { return closes },
  }
}

test('cache key isolates Actor, Scope, query discriminator, parameters, and page', () => {
  const base = request({ parameters: { states: ['idle'], nested: { b: 2, a: 1 } } })
  assert.equal(
    queryCacheKey(base),
    queryCacheKey(request({
      requestId: 'req_00000000000000000000000002',
      parameters: { nested: { a: 1, b: 2 }, states: ['idle'] },
    })),
  )
  assert.notEqual(queryCacheKey(base), queryCacheKey(request({ requestActor: otherActor })))
  assert.notEqual(queryCacheKey(base), queryCacheKey(request({ requestScope: otherScope })))
  assert.notEqual(queryCacheKey(base), queryCacheKey(request({ query: 'approval.list' })))
  assert.notEqual(queryCacheKey(base), queryCacheKey(request({ parameters: { states: ['running'] } })))
  assert.notEqual(queryCacheKey(base), queryCacheKey(request({ page: { cursor: null, limit: 1 } })))
})

test('cached snapshots keep each generated request correlation and coalesce concurrent reads', async () => {
  const pending = deferred()
  const calls = []
  const fake = fakeClient(async queryRequest => {
    calls.push(queryRequest)
    await pending.promise
    return response('snapshot-1', queryRequest)
  })
  const cache = createQueryCache({ client: fake.client })
  const firstRequest = request()
  const secondRequest = request({ requestId: 'req_00000000000000000000000002' })
  const first = cache.client.query(firstRequest)
  const second = cache.client.query(secondRequest)
  assert.equal(calls.length, 1)
  pending.resolve()
  const [firstResponse, secondResponse] = await Promise.all([first, second])
  assert.equal(firstResponse.requestId, firstRequest.requestId)
  assert.equal(secondResponse.requestId, secondRequest.requestId)
  assert.equal(firstResponse.result, secondResponse.result)
  assert.equal(calls.length, 1)

  const thirdRequest = request({ requestId: 'req_00000000000000000000000003' })
  const thirdResponse = await cache.client.query(thirdRequest)
  assert.equal(thirdResponse.requestId, thirdRequest.requestId)
  assert.equal(calls.length, 1)
  cache.close()
})

test('snapshot handoff retains stale data while repeated invalidations produce one trailing reload', async () => {
  const second = deferred()
  const third = deferred()
  const calls = []
  const fake = fakeClient(async queryRequest => {
    calls.push(queryRequest)
    if (calls.length === 1) return response('snapshot-1', queryRequest)
    if (calls.length === 2) {
      await second.promise
      return response('snapshot-2', queryRequest)
    }
    await third.promise
    return response('snapshot-3', queryRequest)
  })
  const cache = createQueryCache({ client: fake.client })
  const base = request()
  await cache.client.query(base)
  cache.invalidate({ scope, queries: ['session.list'], reason: 'event' })
  const duringFirstReload = cache.client.query(request({
    requestId: 'req_00000000000000000000000002',
  }))
  assert.deepEqual(cache.peek(base), {
    key: queryCacheKey(base),
    response: response('snapshot-1', base),
    status: 'stale',
  })

  cache.invalidate({ scope, queries: ['session.list'], reason: 'event' })
  cache.invalidate({ scope, queries: ['session.list'], reason: 'event' })
  const afterRepeatedInvalidation = cache.client.query(request({
    requestId: 'req_00000000000000000000000003',
  }))
  assert.equal(calls.length, 2)
  second.resolve()
  await Promise.resolve()
  await Promise.resolve()
  assert.equal(calls.length, 3)
  third.resolve()
  const [firstReload, trailingReload] = await Promise.all([
    duringFirstReload,
    afterRepeatedInvalidation,
  ])
  assert.equal(firstReload.result.items[0], 'snapshot-3')
  assert.equal(trailingReload.result.items[0], 'snapshot-3')
  assert.equal(calls.length, 3)
  assert.equal(cache.peek(base).status, 'fresh')
  cache.close()
})

test('retention reset, authorization epoch change, revocation, resume, and reconnect discard snapshots', async () => {
  const calls = []
  const fake = fakeClient(async queryRequest => {
    calls.push(queryRequest)
    return response(`snapshot-${String(calls.length)}`, queryRequest)
  })
  const cache = createQueryCache({ client: fake.client })
  const base = request()
  await cache.client.query(base)
  let reloads = 0
  let resets = 0
  let revocations = 0
  const subscription = cache.client.subscribe({
    subscriptionId: 'sub_00000000000000000000000001',
    subscription: { scope, stream: { kind: 'scope' }, eventTypes: ['activity.recorded.v1'] },
    async onEvent() {
      reloads += 1
      await cache.client.query(request({
        requestId: `req_${String(reloads + 1).padStart(26, '0')}`,
      }))
    },
    async onResetRequired(frame) {
      resets += 1
      assert.equal(cache.peek(base), null)
      return frame?.earliestAvailable ?? {
        scope,
        stream: { kind: 'scope' },
        sequence: 0,
        eventId: null,
      }
    },
    onAuthorizationRevoked() {
      revocations += 1
      assert.equal(cache.peek(base), null)
    },
  })
  const event = authorizationEpoch => ({
    type: 'event.v1',
    subscriptionId: 'sub_00000000000000000000000001',
    authorizationEpoch,
    scope,
    stream: { kind: 'scope' },
    event: { type: 'activity.recorded.v1' },
  })
  await fake.options.onEvent(event(4))
  assert.equal(reloads, 1)
  assert.equal(calls.length, 2)
  await fake.options.onEvent(event(5))
  assert.equal(reloads, 2)
  assert.equal(calls.length, 3)
  assert.equal(cache.peek(base).status, 'fresh')

  const earliestAvailable = { scope, stream: { kind: 'scope' }, sequence: 10, eventId: null }
  assert.equal(await fake.options.onResetRequired({ earliestAvailable }), earliestAvailable)
  assert.equal(resets, 1)
  await cache.client.query(request({ requestId: 'req_00000000000000000000000010' }))
  const after4409 = await fake.options.onResetRequired(null)
  assert.equal(after4409.sequence, 0)
  assert.equal(resets, 2)
  await cache.client.query(request({ requestId: 'req_00000000000000000000000013' }))
  await fake.options.onAuthorizationRevoked({ authorizationEpoch: 6 })
  assert.equal(revocations, 1)

  await cache.client.query(request({ requestId: 'req_00000000000000000000000011' }))
  subscription.resume()
  assert.equal(cache.peek(base), null)
  assert.equal(fake.resumes, 1)
  await cache.client.query(request({ requestId: 'req_00000000000000000000000012' }))
  subscription.reconnect()
  assert.equal(cache.peek(base), null)
  assert.equal(fake.reconnects, 1)
  subscription.close()
  assert.equal(fake.closes, 1)
  cache.close()
})

test('WebSocket payloads only invalidate; the next snapshot still comes from HTTP', async () => {
  let serverValue = 'server-1'
  let capturedPayload = null
  const fake = fakeClient(async queryRequest => response(serverValue, queryRequest))
  const cache = createQueryCache({ client: fake.client })
  const base = request()
  await cache.client.query(base)
  cache.client.subscribe({
    subscriptionId: 'sub_00000000000000000000000001',
    subscription: { scope, stream: { kind: 'scope' }, eventTypes: ['activity.recorded.v1'] },
    onEvent(frame) { capturedPayload = frame.event.value },
  })
  serverValue = 'server-2'
  await fake.options.onEvent({
    type: 'event.v1',
    subscriptionId: 'sub_00000000000000000000000001',
    authorizationEpoch: 1,
    scope,
    stream: { kind: 'scope' },
    event: { type: 'activity.recorded.v1', value: 'untrusted-ws-value' },
  })
  assert.equal(capturedPayload, 'untrusted-ws-value')
  assert.equal(cache.peek(base).status, 'stale')
  const refreshed = await cache.client.query(request({
    requestId: 'req_00000000000000000000000002',
  }))
  assert.equal(refreshed.result.items[0], 'server-2')
  assert.notEqual(refreshed.result.items[0], capturedPayload)
  cache.close()
})

test('discarded in-flight work cannot remove the replacement snapshot', async () => {
  const obsolete = deferred()
  let calls = 0
  const fake = fakeClient(async queryRequest => {
    calls += 1
    if (calls === 1) {
      await obsolete.promise
      return response('obsolete', queryRequest)
    }
    return response('replacement', queryRequest)
  })
  const cache = createQueryCache({ client: fake.client })
  const base = request()
  const first = cache.client.query(base)
  const firstFailure = assert.rejects(first, error => error.code === 'REQUEST_CANCELLED')
  cache.invalidate({ scope, reason: 'authorization-epoch', discard: true })
  const replacement = await cache.client.query(request({
    requestId: 'req_00000000000000000000000002',
  }))
  assert.equal(replacement.result.items[0], 'replacement')
  obsolete.resolve()
  await firstFailure
  assert.equal(cache.peek(base).response.result.items[0], 'replacement')
  cache.close()
})

test('a 4409 reset without a cursor-producing reload fails closed after discarding cache', async () => {
  const fake = fakeClient(async queryRequest => response('snapshot', queryRequest))
  const cache = createQueryCache({ client: fake.client })
  const base = request()
  await cache.client.query(base)
  cache.client.subscribe({
    subscriptionId: 'sub_00000000000000000000000001',
    subscription: { scope, stream: { kind: 'scope' }, eventTypes: ['activity.recorded.v1'] },
    onEvent() {},
  })
  await assert.rejects(
    fake.options.onResetRequired(null),
    error => error.code === 'RESET_REQUIRED',
  )
  assert.equal(cache.peek(base), null)
  cache.close()
})
