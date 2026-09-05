import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.client-occupancy-tests.json',
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
  `Occupancy facade did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/client-occupancy-tests')
// Plain module paths keep one ControlPlaneClientError class identity across
// the facade and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')

const {
  ControlPlaneClientError,
  controlPlaneOccupancyFailure,
  createControlPlaneClientOccupancy,
} = facade

const schemaVersion = 'winwincode/v1'
const claimPath = 'https://control.example/api/v1/clients/occupancy'
const forceReleasePath = 'https://control.example/api/v1/clients/occupancy/force-release'
const validClientId = '123456789012'

function holderView(overrides = {}) {
  return {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'occupied',
    presence: 'online',
    holderUserId: 'usr_00000000000000000000000001',
    occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
    fencingToken: 7,
    claimedAt: '2026-09-04T00:00:00.000Z',
    acknowledgedAt: '2026-09-04T00:00:01.000Z',
    recoveryDeadlineAt: null,
    capacityUsed: 3,
    capacityTotal: 8,
    ...overrides,
  }
}

function response(status, payload = '') {
  return {
    ok: status >= 200 && status < 300,
    status,
    async text() {
      return typeof payload === 'string' ? payload : JSON.stringify(payload)
    },
  }
}

function errorPayload(code, message, retryable = false) {
  return {
    schemaVersion,
    requestId: 'req_00000000000000000000000001',
    error: { code, message, retryable, details: {} },
  }
}

function baseClient(overrides = {}) {
  return {
    serverUrl: 'https://control.example',
    async restore() { throw new Error('not used') },
    async login() { throw new Error('not used') },
    async loginWithPassword() { throw new Error('not used') },
    async initializationStatus() { throw new Error('not used') },
    async logout() {},
    async command() { throw new Error('not used') },
    async query() { throw new Error('not used') },
    subscribe() { throw new Error('not used') },
    close() {},
    ...overrides,
  }
}

function occupancyFixture(transport, baseOverrides = {}) {
  return createControlPlaneClientOccupancy({
    client: baseClient(baseOverrides),
    transport,
  })
}

test('facade claims occupancy with the digit identity and freezes the holder view', async () => {
  const requests = []
  const occupancy = occupancyFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), init: structuredClone(init) })
      return response(201, holderView())
    },
  })

  const view = await occupancy.claimOccupancy({ clientId: '1234 5678 9012' })

  assert.deepEqual(view, {
    occupancy: 'occupied',
    presence: 'online',
    holderUserId: 'usr_00000000000000000000000001',
    occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
    fencingToken: 7,
    claimedAt: '2026-09-04T00:00:00.000Z',
    acknowledgedAt: '2026-09-04T00:00:01.000Z',
    recoveryDeadlineAt: null,
    capacityUsed: 3,
    capacityTotal: 8,
  })
  assert.equal(Object.isFrozen(view), true)
  assert.deepEqual(requests.map(request => [request.input, request.init.method]), [
    [claimPath, 'POST'],
  ])
  const claimRequest = requests[0]
  assert.equal(claimRequest.init.credentials, 'include')
  assert.equal(claimRequest.init.redirect, 'error')
  assert.equal(claimRequest.init.cache, 'no-store')
  assert.equal(claimRequest.init.referrerPolicy, 'no-referrer')
  assert.deepEqual(JSON.parse(claimRequest.init.body), {
    schemaVersion,
    clientId: validClientId,
  })
})

test('facade validates occupancy input before any request exists', async () => {
  let requests = 0
  const occupancy = occupancyFixture({
    async fetch() {
      requests += 1
      return response(201, holderView())
    },
  })
  await assert.rejects(
    occupancy.claimOccupancy({ clientId: '12345678' }),
    error => error.code === 'CLIENT_OCCUPANCY_ID_INVALID',
  )
  await assert.rejects(
    occupancy.claimOccupancy({ clientId: '1234567890123' }),
    error => error.code === 'CLIENT_OCCUPANCY_ID_INVALID',
  )
  await assert.rejects(
    occupancy.occupancyStatus({ clientId: 'device-local' }),
    error => error.code === 'CLIENT_OCCUPANCY_ID_INVALID',
  )
  await assert.rejects(
    occupancy.releaseOccupancy({ clientId: '12 34', mode: 'release' }),
    error => error.code === 'CLIENT_OCCUPANCY_ID_INVALID',
  )
  await assert.rejects(
    occupancy.forceReleaseOccupancy({ clientId: '' }),
    error => error.code === 'CLIENT_OCCUPANCY_ID_INVALID',
  )
  assert.equal(requests, 0)
})

test('repeated claims for the same Client are idempotent at the facade seam', async () => {
  let requests = 0
  let releaseFirst
  const occupancy = occupancyFixture({
    async fetch() {
      requests += 1
      if (requests === 1) await new Promise(resolvePromise => { releaseFirst = resolvePromise })
      return response(201, holderView())
    },
  })
  const first = occupancy.claimOccupancy({ clientId: validClientId })
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  const second = occupancy.claimOccupancy({ clientId: '1234 5678 9012' })
  assert.equal(second, first, 'a second caller joins the in-flight claim')
  releaseFirst()
  const [firstView, secondView] = await Promise.all([first, second])
  assert.equal(requests, 1, 'an in-flight claim is never repeated')
  assert.equal(firstView, secondView, 'both callers get the one claim result')

  const replayed = await occupancy.claimOccupancy({ clientId: validClientId })
  assert.equal(requests, 2, 'a settled claim replays on the wire')
  assert.deepEqual(replayed, firstView, 'the replay resolves to the same holder view')
})

test('concurrent claims for different Clients never join each other', async () => {
  const requested = []
  const occupancy = occupancyFixture({
    async fetch(input, init) {
      const body = JSON.parse(init.body)
      requested.push(body.clientId)
      return response(201, holderView({
        clientId: body.clientId,
        occupancyLeaseId: body.clientId === '999999999999' ? 'ocl_9' : 'ocl_1',
      }))
    },
  })
  const [first, second] = await Promise.all([
    occupancy.claimOccupancy({ clientId: validClientId }),
    occupancy.claimOccupancy({ clientId: '999999999999' }),
  ])
  assert.equal(requested.length, 2)
  assert.equal(first.occupancyLeaseId, 'ocl_1')
  assert.equal(second.occupancyLeaseId, 'ocl_9')
})

test('facade reads all three occupancy projections for the signed-in user', async () => {
  let payload = { schemaVersion, clientId: validClientId, occupancy: 'available', presence: 'online' }
  const occupancy = occupancyFixture({
    async fetch(input, init) {
      assert.equal(init.method, 'GET')
      assert.equal(
        String(input),
        `https://control.example/api/v1/clients/${validClientId}/occupancy`,
      )
      assert.equal(init.body, undefined, 'a status read carries no body')
      return response(200, payload)
    },
  })

  const available = await occupancy.occupancyStatus({ clientId: validClientId })
  assert.deepEqual(available, { occupancy: 'available', presence: 'online' })
  assert.equal(Object.isFrozen(available), true)

  payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'occupied-by-other',
  }
  const projection = await occupancy.occupancyStatus({ clientId: validClientId })
  assert.deepEqual(projection, { occupancy: 'occupied-by-other' })
  assert.equal(Object.isFrozen(projection), true)
  assert.deepEqual(Object.keys(projection), ['occupancy'], 'the projection names the occupancy only')

  payload = holderView({ occupancy: 'reserving', acknowledgedAt: null })
  const reserving = await occupancy.occupancyStatus({ clientId: validClientId })
  assert.equal(reserving.occupancy, 'reserving')
  assert.equal(reserving.holderUserId, 'usr_00000000000000000000000001')
  assert.equal(reserving.acknowledgedAt, null)
})

test('a non-holder projection drops holder fields even when the wire drifted', async () => {
  const occupancy = occupancyFixture({
    async fetch() {
      return response(200, {
        schemaVersion,
        clientId: validClientId,
        occupancy: 'occupied-by-other',
        holderUserId: 'usr_99999999999999999999999999',
        occupancyLeaseId: 'ocl_leaked',
        fencingToken: 99,
        capacityTotal: 8,
      })
    },
  })
  const projection = await occupancy.occupancyStatus({ clientId: validClientId })
  assert.equal(projection.occupancy, 'occupied-by-other')
  assert.equal('holderUserId' in projection, false)
  assert.equal('occupancyLeaseId' in projection, false)
  assert.equal('fencingToken' in projection, false)
  assert.equal('capacityTotal' in projection, false)
  assert.equal(JSON.stringify(projection).includes('usr_99999999999999999999999999'), false)
})

test('facade releases with the three modes and parses both outcomes', async () => {
  const requests = []
  let payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'released',
    occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
    mode: 'release',
  }
  const occupancy = occupancyFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), init: structuredClone(init) })
      return response(200, payload)
    },
  })

  const released = await occupancy.releaseOccupancy({ clientId: validClientId, mode: 'release' })
  assert.deepEqual(released, {
    occupancy: 'released',
    occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
    mode: 'release',
  })
  assert.equal(Object.isFrozen(released), true)

  payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'draining',
    occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
    mode: 'drain',
  }
  assert.deepEqual(
    await occupancy.releaseOccupancy({ clientId: validClientId, mode: 'drain' }),
    { occupancy: 'draining', occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV', mode: 'drain' },
  )

  payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'draining',
    occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
    mode: 'cancel_and_release',
  }
  await occupancy.releaseOccupancy({
    clientId: validClientId,
    mode: 'cancel_and_release',
    confirm: true,
  })
  const cancelRequest = requests.at(-1)
  assert.equal(cancelRequest.init.method, 'DELETE')
  assert.deepEqual(JSON.parse(cancelRequest.init.body), {
    schemaVersion,
    clientId: validClientId,
    mode: 'cancel_and_release',
    confirm: true,
  })
  assert.deepEqual(requests.map(request => request.input), [claimPath, claimPath, claimPath])
})

test('facade force-releases and parses the strictly higher fence token', async () => {
  const requests = []
  const occupancy = occupancyFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), init: structuredClone(init) })
      return response(200, {
        schemaVersion,
        clientId: validClientId,
        released: true,
        occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
        forceFenceToken: 41,
      })
    },
  })
  const outcome = await occupancy.forceReleaseOccupancy({ clientId: validClientId })
  assert.deepEqual(outcome, {
    released: true,
    occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
    forceFenceToken: 41,
  })
  assert.equal(Object.isFrozen(outcome), true)
  assert.deepEqual(requests.map(request => [request.input, request.init.method]), [
    [forceReleasePath, 'POST'],
  ])
  assert.deepEqual(JSON.parse(requests[0].init.body), {
    schemaVersion,
    clientId: validClientId,
  })
})

test('every occupancy wire code maps onto the finite presentation taxonomy', async () => {
  const cases = [
    { status: 400, code: 'INVALID_REQUEST', failure: 'invalid-request' },
    { status: 400, code: 'CONFIRMATION_REQUIRED', failure: 'confirmation-required' },
    { status: 404, code: 'CLIENT_NOT_FOUND', failure: 'client-not-found' },
    { status: 409, code: 'CLIENT_OFFLINE', failure: 'client-offline' },
    { status: 409, code: 'CLIENT_LOCKED', failure: 'client-locked' },
    { status: 409, code: 'CLIENT_CONNECTIONS_FORBIDDEN', failure: 'new-connections-forbidden' },
    { status: 403, code: 'ACCESS_DENIED', failure: 'access-denied' },
    { status: 409, code: 'OCCUPIED_BY_OTHER', failure: 'occupied-by-other' },
    { status: 409, code: 'CAPACITY_EXHAUSTED', failure: 'capacity-exhausted' },
    { status: 409, code: 'OCCUPANCY_REJECTED', failure: 'occupancy-rejected' },
    { status: 504, code: 'OCCUPANCY_ACK_TIMEOUT', failure: 'occupancy-ack-timeout' },
    { status: 409, code: 'OCCUPANCY_RECOVERY_PENDING', failure: 'recovery-pending' },
    { status: 403, code: 'PERMISSION_DENIED', failure: 'permission-denied' },
    { status: 404, code: 'RESOURCE_NOT_FOUND', failure: 'no-active-occupancy' },
    { status: 409, code: 'WRONG_STATE', failure: 'wrong-state' },
    { status: 429, code: 'RATE_LIMITED', failure: 'rate-limited', retryable: true },
    { status: 503, code: 'SERVICE_UNAVAILABLE', failure: 'unavailable', retryable: true },
  ]
  for (const candidate of cases) {
    const occupancy = occupancyFixture({
      async fetch() {
        return response(candidate.status, errorPayload(
          candidate.code,
          'occupancy rejected',
          candidate.retryable === true,
        ))
      },
    })
    await assert.rejects(
      occupancy.claimOccupancy({ clientId: validClientId }),
      error => {
        assert.equal(error instanceof ControlPlaneClientError, true)
        assert.equal(error.code, candidate.code)
        assert.equal(error.retryable, candidate.retryable === true)
        assert.equal(controlPlaneOccupancyFailure(error), candidate.failure)
        return true
      },
    )
  }
  const offline = occupancyFixture({
    async fetch() {
      throw new TypeError('network unreachable')
    },
  })
  await assert.rejects(
    offline.releaseOccupancy({ clientId: validClientId, mode: 'release' }),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'network'
      && controlPlaneOccupancyFailure(error) === 'unavailable',
  )
  const unknown = occupancyFixture({
    async fetch() {
      return response(409, errorPayload('SOMETHING_ELSE', 'unmapped'))
    },
  })
  await assert.rejects(
    unknown.occupancyStatus({ clientId: validClientId }),
    error => controlPlaneOccupancyFailure(error) === 'unavailable',
  )
  const expired = occupancyFixture({
    async fetch() {
      return response(401, errorPayload('AUTHENTICATION_REQUIRED', 'sign in again'))
    },
  })
  await assert.rejects(
    expired.forceReleaseOccupancy({ clientId: validClientId }),
    error => error.kind === 'authentication'
      && controlPlaneOccupancyFailure(error) === 'unavailable',
  )
})

test('facade rejects malformed occupancy payloads and schema drift', async () => {
  let payload = { schemaVersion, clientId: validClientId, occupancy: 'available', presence: 'online' }
  let status = 200
  const occupancy = occupancyFixture({
    async fetch() {
      return response(status, payload)
    },
  })
  payload = { schemaVersion, clientId: validClientId, occupancy: 'available', presence: 'sleeping' }
  await assert.rejects(
    occupancy.occupancyStatus({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  payload = { schemaVersion, clientId: validClientId, occupancy: 'haunted' }
  await assert.rejects(
    occupancy.occupancyStatus({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  payload = holderView({ fencingToken: 0 })
  await assert.rejects(
    occupancy.occupancyStatus({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  payload = holderView({ capacityUsed: 9 })
  await assert.rejects(
    occupancy.occupancyStatus({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  status = 201
  payload = holderView({ claimedAt: 'not-an-instant' })
  await assert.rejects(
    occupancy.claimOccupancy({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  status = 200
  payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'released',
    occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
    mode: 'teleport',
  }
  await assert.rejects(
    occupancy.releaseOccupancy({ clientId: validClientId, mode: 'release' }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'reserving',
    occupancyLeaseId: 'ocl_0123456789ABCDEFGHJKMNPQRSV',
    mode: 'release',
  }
  await assert.rejects(
    occupancy.releaseOccupancy({ clientId: validClientId, mode: 'release' }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  payload = { schemaVersion, clientId: validClientId, released: 'yes', forceFenceToken: 3 }
  await assert.rejects(
    occupancy.forceReleaseOccupancy({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  payload = { schemaVersion, clientId: validClientId, released: true, forceFenceToken: 0 }
  await assert.rejects(
    occupancy.forceReleaseOccupancy({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  status = 201
  payload = { schemaVersion: 'winwincode/v0', clientId: validClientId, occupancy: 'occupied' }
  await assert.rejects(
    occupancy.claimOccupancy({ clientId: validClientId }),
    error => error.kind === 'version' && error.code === 'SCHEMA_VERSION_MISMATCH',
  )
})

test('the decorator reuses an injected facade that already implements occupancy', async () => {
  const attempts = []
  const occupancy = createControlPlaneClientOccupancy({
    client: baseClient({
      async claimOccupancy(input) {
        attempts.push(['claim', input])
        return {
          occupancy: 'occupied',
          presence: 'online',
          holderUserId: 'usr_00000000000000000000000001',
          occupancyLeaseId: 'ocl_injected',
          fencingToken: 2,
          claimedAt: null,
          acknowledgedAt: null,
          recoveryDeadlineAt: null,
          capacityUsed: 0,
          capacityTotal: 4,
        }
      },
      async occupancyStatus(input) {
        attempts.push(['status', input])
        return { occupancy: 'occupied-by-other' }
      },
      async releaseOccupancy(input) {
        attempts.push(['release', input])
        return { occupancy: 'released', occupancyLeaseId: 'ocl_injected', mode: input.mode }
      },
      async forceReleaseOccupancy(input) {
        attempts.push(['force', input])
        return { released: true, occupancyLeaseId: 'ocl_injected', forceFenceToken: 9 }
      },
    }),
    transport: {
      async fetch() {
        throw new Error('the occupancy facade must stay on the injected seam')
      },
    },
  })
  assert.equal((await occupancy.claimOccupancy({ clientId: validClientId })).occupancyLeaseId,
    'ocl_injected')
  assert.deepEqual(await occupancy.occupancyStatus({ clientId: ' 123456789012 ' }),
    { occupancy: 'occupied-by-other' })
  assert.deepEqual(
    await occupancy.releaseOccupancy({ clientId: validClientId, mode: 'drain' }),
    { occupancy: 'released', occupancyLeaseId: 'ocl_injected', mode: 'drain' },
  )
  assert.deepEqual(await occupancy.forceReleaseOccupancy({ clientId: validClientId }), {
    released: true,
    occupancyLeaseId: 'ocl_injected',
    forceFenceToken: 9,
  })
  assert.deepEqual(attempts, [
    ['claim', { clientId: validClientId }],
    ['status', { clientId: validClientId }],
    ['release', { clientId: validClientId, mode: 'drain' }],
    ['force', { clientId: validClientId }],
  ])
})

test('an in-flight failed claim unblocks the next attempt for the same Client', async () => {
  let requests = 0
  let failFirst = true
  let releaseFirst
  const occupancy = occupancyFixture({
    async fetch() {
      requests += 1
      if (requests === 1) await new Promise(resolvePromise => { releaseFirst = resolvePromise })
      if (failFirst) return response(409, errorPayload('OCCUPIED_BY_OTHER', 'taken'))
      return response(201, holderView())
    },
  })
  const first = occupancy.claimOccupancy({ clientId: validClientId })
  const joined = occupancy.claimOccupancy({ clientId: validClientId })
  releaseFirst()
  await assert.rejects(first)
  await assert.rejects(joined, error => controlPlaneOccupancyFailure(error) === 'occupied-by-other')
  assert.equal(requests, 1)

  failFirst = false
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  const retry = await occupancy.claimOccupancy({ clientId: validClientId })
  assert.equal(requests, 2, 'the failed claim freed the seam for the retry')
  assert.equal(retry.occupancy, 'occupied')
})
