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
// the facade, the wire seam, the view-model, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facadeModule = await cachedModule('client-occupancy-facade.js')
const wireModule = await cachedModule('community-control-plane-client.js')
const occupancyViewModelModule = await cachedModule('client-occupancy-view-model.js')

const {
  createClientOccupancyFacade,
  clientOccupancyErrorCategory,
} = facadeModule
const { ControlPlaneClientError } = wireModule
const { clientOccupancyPortFromFacade } = occupancyViewModelModule

const schemaVersion = 'winwincode/v1'
const claimPath = 'https://control.example/api/v1/clients/occupancy'
const statusPath = 'https://control.example/api/v1/clients/123456789012/occupancy'
const forceReleasePath = 'https://control.example/api/v1/clients/occupancy/force-release'
const validClientId = '123456789012'
const foreignUserId = 'usr_99999999999999999999999999'
const foreignLeaseId = 'ocl_FOREIGNHOLDERLEAK000000000'
const ownUserId = 'usr_00000000000000000000000001'
const ownLeaseId = 'ocl_0123456789ABCDEFGHJKMNPQRSV'

function holderView(overrides = {}) {
  return {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'occupied',
    presence: 'online',
    holderUserId: ownUserId,
    occupancyLeaseId: ownLeaseId,
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

function errorPayload(code, message, retryable = false, details = {}) {
  return {
    schemaVersion,
    requestId: 'req_00000000000000000000000001',
    error: { code, message, retryable, details },
  }
}

function baseClient(overrides = {}) {
  return {
    serverUrl: 'https://control.example',
    async restore() { throw new Error('not used') },
    async initializeOwner() { throw new Error('not used') },
    async login() { throw new Error('not used') },
    async initializationStatus() { throw new Error('not used') },
    async logout() {},
    async command() { throw new Error('not used') },
    async query() { throw new Error('not used') },
    subscribe() { throw new Error('not used') },
    close() {},
    ...overrides,
  }
}

function facadeFixture(transport, baseOverrides = {}) {
  return createClientOccupancyFacade({
    client: baseClient(baseOverrides),
    transport,
  })
}

async function rejectionOf(run) {
  try {
    await run()
  } catch (error) {
    return error
  }
  throw new Error('the facade call was expected to reject')
}

test('claim resolves with the frozen holder view over one hardened request', async () => {
  const requests = []
  const occupancy = facadeFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), init: structuredClone(init) })
      return response(201, holderView())
    },
  })

  const view = await occupancy.claim({ clientId: '1234 5678 9012' })

  assert.deepEqual(view, {
    occupancy: 'occupied',
    presence: 'online',
    holderUserId: ownUserId,
    occupancyLeaseId: ownLeaseId,
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

test('getStatus returns the Server projection and never a derived state', async () => {
  let payload = { schemaVersion, clientId: validClientId, occupancy: 'available', presence: 'online' }
  const occupancy = facadeFixture({
    async fetch(input, init) {
      assert.equal(init.method, 'GET')
      assert.equal(String(input), statusPath)
      assert.equal(init.body, undefined, 'a status read carries no body')
      return response(200, payload)
    },
  })

  const available = await occupancy.getStatus({ clientId: validClientId })
  assert.deepEqual(available, { occupancy: 'available', presence: 'online' })
  assert.equal(Object.isFrozen(available), true)

  payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'occupied-by-other',
  }
  const projection = await occupancy.getStatus({ clientId: validClientId })
  assert.deepEqual(projection, { occupancy: 'occupied-by-other' })
  assert.equal(Object.isFrozen(projection), true)
  assert.deepEqual(Object.keys(projection), ['occupancy'], 'the projection names the occupancy only')
})

test('a non-holder projection drops holder fields even when the wire drifted', async () => {
  const occupancy = facadeFixture({
    async fetch() {
      return response(200, {
        schemaVersion,
        clientId: validClientId,
        occupancy: 'occupied-by-other',
        holderUserId: foreignUserId,
        occupancyLeaseId: foreignLeaseId,
        fencingToken: 99,
        capacityTotal: 8,
      })
    },
  })
  const projection = await occupancy.getStatus({ clientId: validClientId })
  assert.equal(projection.occupancy, 'occupied-by-other')
  assert.equal('holderUserId' in projection, false)
  assert.equal('occupancyLeaseId' in projection, false)
  assert.equal('fencingToken' in projection, false)
  assert.equal('capacityTotal' in projection, false)
  assert.equal(JSON.stringify(projection).includes(foreignUserId), false)
})

test('the holder projection of the caller carries only the caller identity', async () => {
  let payload = holderView({ occupancy: 'reserving', acknowledgedAt: null })
  const occupancy = facadeFixture({
    async fetch() {
      return response(200, payload)
    },
  })
  const reserving = await occupancy.getStatus({ clientId: validClientId })
  assert.equal(reserving.occupancy, 'reserving')
  assert.equal(reserving.holderUserId, ownUserId)
  assert.equal(reserving.acknowledgedAt, null)
  assert.equal(JSON.stringify(reserving).includes(foreignUserId), false)
})

test('release defaults to the immediate mode and parses both outcomes', async () => {
  const requests = []
  let payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'released',
    occupancyLeaseId: ownLeaseId,
    mode: 'release',
  }
  const occupancy = facadeFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), init: structuredClone(init) })
      return response(200, payload)
    },
  })

  const released = await occupancy.release({ clientId: validClientId })
  assert.deepEqual(released, {
    occupancy: 'released',
    occupancyLeaseId: ownLeaseId,
    mode: 'release',
  })
  assert.equal(Object.isFrozen(released), true)

  payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'draining',
    occupancyLeaseId: ownLeaseId,
    mode: 'drain',
  }
  const draining = await occupancy.release({ clientId: validClientId, mode: 'drain' })
  assert.deepEqual(draining, {
    occupancy: 'draining',
    occupancyLeaseId: ownLeaseId,
    mode: 'drain',
  })
  assert.deepEqual(requests.map(request => [request.input, request.init.method]), [
    [claimPath, 'DELETE'],
    [claimPath, 'DELETE'],
  ])
  assert.deepEqual(JSON.parse(requests[0].init.body), {
    schemaVersion,
    clientId: validClientId,
    mode: 'release',
  })
  assert.deepEqual(JSON.parse(requests[1].init.body), {
    schemaVersion,
    clientId: validClientId,
    mode: 'drain',
  })
})

test('cancelAndRelease submits the confirmed destructive mode', async () => {
  const requests = []
  const occupancy = facadeFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), init: structuredClone(init) })
      return response(200, {
        schemaVersion,
        clientId: validClientId,
        occupancy: 'draining',
        occupancyLeaseId: ownLeaseId,
        mode: 'cancel_and_release',
      })
    },
  })
  const outcome = await occupancy.cancelAndRelease({ clientId: validClientId })
  assert.deepEqual(outcome, {
    occupancy: 'draining',
    occupancyLeaseId: ownLeaseId,
    mode: 'cancel_and_release',
  })
  assert.equal(Object.isFrozen(outcome), true)
  assert.deepEqual(requests.map(request => [request.input, request.init.method]), [
    [claimPath, 'DELETE'],
  ])
  assert.deepEqual(JSON.parse(requests[0].init.body), {
    schemaVersion,
    clientId: validClientId,
    mode: 'cancel_and_release',
    confirm: true,
  })
})

test('forceRelease resolves with the strictly higher fence token', async () => {
  const requests = []
  const occupancy = facadeFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), init: structuredClone(init) })
      return response(200, {
        schemaVersion,
        clientId: validClientId,
        released: true,
        occupancyLeaseId: ownLeaseId,
        forceFenceToken: 41,
      })
    },
  })
  const outcome = await occupancy.forceRelease({ clientId: validClientId })
  assert.deepEqual(outcome, {
    released: true,
    occupancyLeaseId: ownLeaseId,
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

test('repeated claims for the same Client join one request and replay idempotently', async () => {
  let requests = 0
  let releaseFirst
  const occupancy = facadeFixture({
    async fetch() {
      requests += 1
      if (requests === 1) await new Promise(resolvePromise => { releaseFirst = resolvePromise })
      return response(201, holderView())
    },
  })
  const first = occupancy.claim({ clientId: validClientId })
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  const second = occupancy.claim({ clientId: '1234 5678 9012' })
  releaseFirst()
  const [firstView, secondView] = await Promise.all([first, second])
  assert.equal(requests, 1, 'an in-flight claim is never repeated')
  assert.deepEqual(firstView, secondView, 'both callers get the one claim result')

  const replayed = await occupancy.claim({ clientId: validClientId })
  assert.equal(requests, 2, 'a settled claim replays on the wire')
  assert.deepEqual(replayed, firstView, 'the replay resolves to the same holder view')
})

test('concurrent claims for different Clients never join each other', async () => {
  const requested = []
  const occupancy = facadeFixture({
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
    occupancy.claim({ clientId: validClientId }),
    occupancy.claim({ clientId: '999999999999' }),
  ])
  assert.equal(requested.length, 2)
  assert.equal(first.occupancyLeaseId, 'ocl_1')
  assert.equal(second.occupancyLeaseId, 'ocl_9')
})

test('an in-flight failed claim unblocks the next attempt for the same Client', async () => {
  let requests = 0
  let failFirst = true
  let releaseFirst
  const occupancy = facadeFixture({
    async fetch() {
      requests += 1
      if (requests === 1) await new Promise(resolvePromise => { releaseFirst = resolvePromise })
      if (failFirst) {
        return response(409, errorPayload('OCCUPIED_BY_OTHER', 'taken'))
      }
      return response(201, holderView())
    },
  })
  const first = occupancy.claim({ clientId: validClientId })
  const joined = occupancy.claim({ clientId: validClientId })
  releaseFirst()
  await assert.rejects(first)
  await assert.rejects(joined, error => {
    return clientOccupancyErrorCategory(error) === 'occupied-by-other'
  })
  assert.equal(requests, 1)

  failFirst = false
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  const retry = await occupancy.claim({ clientId: validClientId })
  assert.equal(requests, 2, 'the failed claim freed the seam for the retry')
  assert.equal(retry.occupancy, 'occupied')
})

test('the facade validates the digit identity before a request exists', async () => {
  let requests = 0
  const occupancy = facadeFixture({
    async fetch() {
      requests += 1
      return response(201, holderView())
    },
  })
  for (const run of [
    () => occupancy.claim({ clientId: '12345678' }),
    () => occupancy.claim({ clientId: '1234567890123' }),
    () => occupancy.getStatus({ clientId: 'device-local' }),
    () => occupancy.release({ clientId: '12 34' }),
    () => occupancy.cancelAndRelease({ clientId: '' }),
    () => occupancy.forceRelease({ clientId: 'abc' }),
  ]) {
    const error = await rejectionOf(run)
    assert.equal(error instanceof ControlPlaneClientError, true)
    assert.equal(error.code, 'CLIENT_OCCUPANCY_ID_INVALID')
    assert.equal(
      error.message,
      'Select a Client to manage its occupancy.',
      'client-minted rejections keep their stable copy',
    )
    assert.equal(clientOccupancyErrorCategory(error), 'unavailable')
  }
  assert.equal(requests, 0)
})

test('every server occupancy code maps onto the finite stable category union', async () => {
  const cases = [
    { status: 400, code: 'INVALID_REQUEST', category: 'invalid-request' },
    { status: 400, code: 'CONFIRMATION_REQUIRED', category: 'confirmation-required' },
    { status: 404, code: 'CLIENT_NOT_FOUND', category: 'client-not-found' },
    { status: 409, code: 'CLIENT_OFFLINE', category: 'client-offline' },
    { status: 409, code: 'CLIENT_LOCKED', category: 'client-locked' },
    { status: 409, code: 'CLIENT_CONNECTIONS_FORBIDDEN', category: 'new-connections-forbidden' },
    { status: 403, code: 'ACCESS_DENIED', category: 'access-denied' },
    { status: 409, code: 'OCCUPIED_BY_OTHER', category: 'occupied-by-other' },
    { status: 409, code: 'CAPACITY_EXHAUSTED', category: 'capacity-exhausted' },
    { status: 409, code: 'OCCUPANCY_REJECTED', category: 'occupancy-rejected' },
    { status: 504, code: 'OCCUPANCY_ACK_TIMEOUT', category: 'occupancy-ack-timeout' },
    { status: 409, code: 'OCCUPANCY_RECOVERY_PENDING', category: 'recovery-pending' },
    { status: 403, code: 'PERMISSION_DENIED', category: 'permission-denied' },
    { status: 404, code: 'RESOURCE_NOT_FOUND', category: 'no-active-occupancy' },
    { status: 409, code: 'WRONG_STATE', category: 'wrong-state' },
    { status: 429, code: 'RATE_LIMITED', category: 'rate-limited', retryable: true },
    { status: 503, code: 'SERVICE_UNAVAILABLE', category: 'unavailable', retryable: true },
  ]
  for (const candidate of cases) {
    const rawMessage = `raw ${candidate.code} text`
    const occupancy = facadeFixture({
      async fetch() {
        return response(candidate.status, errorPayload(
          candidate.code,
          rawMessage,
          candidate.retryable === true,
        ))
      },
    })
    const error = await rejectionOf(() => occupancy.claim({ clientId: validClientId }))
    assert.equal(error instanceof ControlPlaneClientError, true, candidate.code)
    assert.equal(error.code, candidate.code)
    assert.equal(error.retryable, candidate.retryable === true, candidate.code)
    assert.equal(
      clientOccupancyErrorCategory(error),
      candidate.category,
      candidate.code,
    )
  }
})

test('every rejection keeps the stable shape over network, auth, and unknown codes', async () => {
  const offline = facadeFixture({
    async fetch() {
      throw new TypeError('network unreachable')
    },
  })
  const network = await rejectionOf(() =>
    offline.release({ clientId: validClientId, mode: 'release' }))
  assert.equal(network instanceof ControlPlaneClientError, true)
  assert.equal(network.kind, 'network')
  assert.equal(network.code, 'NETWORK_ERROR')
  assert.equal(
    network.message,
    'The Control Plane server could not be reached.',
    'client-minted rejections keep their stable copy',
  )
  assert.equal(clientOccupancyErrorCategory(network), 'unavailable')

  const unknown = facadeFixture({
    async fetch() {
      return response(409, errorPayload('SOMETHING_ELSE', 'unmapped'))
    },
  })
  const drifted = await rejectionOf(() => unknown.getStatus({ clientId: validClientId }))
  assert.equal(drifted.code, 'SOMETHING_ELSE')
  assert.equal(clientOccupancyErrorCategory(drifted), 'unavailable')

  const expired = facadeFixture({
    async fetch() {
      return response(401, errorPayload('AUTHENTICATION_REQUIRED', 'sign in again'))
    },
  })
  const authentication = await rejectionOf(() =>
    expired.forceRelease({ clientId: validClientId }))
  assert.equal(authentication.kind, 'authentication')
  assert.equal(clientOccupancyErrorCategory(authentication), 'unavailable')

  // A rejection outside the one error identity can only arrive through the
  // injected-client seam; the facade still collapses it into the honest
  // unavailable failure instead of passing it through.
  const alien = createClientOccupancyFacade({
    client: baseClient({
      async occupancyStatus() {
        throw `foreign text ${foreignUserId}`
      },
    }),
    transport: {
      async fetch() {
        throw new Error('the injected seam must stay untouched by the transport')
      },
    },
  })
  const collapsed = await rejectionOf(() => alien.getStatus({ clientId: validClientId }))
  assert.equal(collapsed.message.includes(foreignUserId), false)
  assert.equal(collapsed instanceof ControlPlaneClientError, true)
  assert.equal(collapsed.code, 'CLIENT_OCCUPANCY_FAILED')
  assert.equal(clientOccupancyErrorCategory(collapsed), 'unavailable')

  const controller = new AbortController()
  controller.abort()
  const aborted = facadeFixture({
    async fetch() {
      throw new Error('an aborted request never reaches the transport')
    },
  })
  const cancelled = await rejectionOf(() =>
    aborted.getStatus({ clientId: validClientId }, { signal: controller.signal }))
  assert.equal(cancelled.code, 'REQUEST_CANCELLED')
  assert.equal(cancelled.kind, 'cancelled')
  assert.equal(clientOccupancyErrorCategory(cancelled), 'unavailable')
})

test('server error text and foreign identity never leak through a rejection', async () => {
  const occupancy = facadeFixture({
    async fetch() {
      return response(409, errorPayload(
        'OCCUPIED_BY_OTHER',
        `Holder ${foreignUserId} holds this device (${foreignLeaseId})`,
        false,
        { holderUserId: foreignUserId, occupancyLeaseId: foreignLeaseId },
      ))
    },
  })
  const error = await rejectionOf(() => occupancy.claim({ clientId: validClientId }))
  assert.equal(error instanceof ControlPlaneClientError, true)
  assert.equal(error.code, 'OCCUPIED_BY_OTHER', 'the stable code survives')
  assert.equal(error.retryable, false)
  assert.notEqual(error.message, `Holder ${foreignUserId} holds this device (${foreignLeaseId})`)
  assert.equal(error.message.includes(foreignUserId), false)
  assert.equal(error.message.includes(foreignLeaseId), false)
  assert.deepEqual(error.details, {}, 'server-supplied details are dropped')
  const printed = JSON.stringify(error)
  assert.equal(printed.includes(foreignUserId), false)
  assert.equal(printed.includes(foreignLeaseId), false)
  assert.equal(error.message.length > 0, true, 'the stable category copy stays')
})

test('malformed payloads and schema drift reject with the stable client codes', async () => {
  let payload = { schemaVersion, clientId: validClientId, occupancy: 'available', presence: 'online' }
  let status = 200
  const occupancy = facadeFixture({
    async fetch() {
      return response(status, payload)
    },
  })
  payload = { schemaVersion, clientId: validClientId, occupancy: 'available', presence: 'sleeping' }
  const malformed = await rejectionOf(() => occupancy.getStatus({ clientId: validClientId }))
  assert.equal(malformed.code, 'INVALID_CLIENT_OCCUPANCY_RESPONSE')
  assert.equal(
    malformed.message,
    'The Control Plane server returned an invalid occupancy response.',
  )
  assert.equal(clientOccupancyErrorCategory(malformed), 'unavailable')

  status = 201
  payload = holderView({ claimedAt: 'not-an-instant' })
  await assert.rejects(
    occupancy.claim({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )

  status = 200
  payload = {
    schemaVersion,
    clientId: validClientId,
    occupancy: 'released',
    occupancyLeaseId: ownLeaseId,
    mode: 'teleport',
  }
  await assert.rejects(
    occupancy.release({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )
  payload = { schemaVersion, clientId: validClientId, released: 'yes', forceFenceToken: 3 }
  await assert.rejects(
    occupancy.forceRelease({ clientId: validClientId }),
    error => error.code === 'INVALID_CLIENT_OCCUPANCY_RESPONSE',
  )

  status = 201
  payload = { schemaVersion: 'winwincode/v0', clientId: validClientId, occupancy: 'occupied' }
  const drifted = await rejectionOf(() => occupancy.claim({ clientId: validClientId }))
  assert.equal(drifted.kind, 'version')
  assert.equal(drifted.code, 'SCHEMA_VERSION_MISMATCH')
  assert.equal(drifted.message, 'The Control Plane server must use winwincode/v1.')
})

test('the facade reuses an injected client that already implements occupancy', async () => {
  const attempts = []
  const occupancy = createClientOccupancyFacade({
    client: baseClient({
      async claimOccupancy(input) {
        attempts.push(['claim', input])
        return holderView({ occupancyLeaseId: 'ocl_injected' })
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
  assert.equal((await occupancy.claim({ clientId: validClientId })).occupancyLeaseId,
    'ocl_injected')
  assert.deepEqual(await occupancy.getStatus({ clientId: ' 123456789012 ' }),
    { occupancy: 'occupied-by-other' })
  assert.deepEqual(await occupancy.release({ clientId: validClientId, mode: 'drain' }), {
    occupancy: 'released',
    occupancyLeaseId: 'ocl_injected',
    mode: 'drain',
  })
  assert.deepEqual(await occupancy.forceRelease({ clientId: validClientId }), {
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

test('the view-model port composes the facade as its one occupancy seam', async () => {
  const requests = []
  const occupancy = facadeFixture({
    async fetch(input, init) {
      requests.push({ input: String(input), body: init.body, method: init.method })
      if (init.method === 'POST' && String(input) === claimPath) {
        return response(201, holderView())
      }
      if (init.method === 'DELETE') {
        const body = JSON.parse(init.body)
        return response(200, {
          schemaVersion,
          clientId: validClientId,
          occupancy: 'draining',
          occupancyLeaseId: ownLeaseId,
          mode: body.mode,
        })
      }
      return response(200, {
        schemaVersion,
        clientId: validClientId,
        released: true,
        occupancyLeaseId: ownLeaseId,
        forceFenceToken: 12,
      })
    },
  })

  const port = clientOccupancyPortFromFacade(occupancy)
  assert.notEqual(port, null, 'the facade satisfies the port seam')
  assert.notEqual(port.forceRelease, undefined, 'the Owner entry stays available')

  await port.claim({ clientId: validClientId })
  await port.release({ clientId: validClientId })
  await port.cancelAndRelease({ clientId: validClientId })
  await port.forceRelease({ clientId: validClientId })
  assert.deepEqual(requests.map(request => [request.method, request.input]), [
    ['POST', claimPath],
    ['DELETE', claimPath],
    ['DELETE', claimPath],
    ['POST', forceReleasePath],
  ])
  assert.deepEqual(JSON.parse(requests[1].body).mode, 'release')
  assert.deepEqual(JSON.parse(requests[2].body).mode, 'cancel_and_release')
  assert.equal(JSON.parse(requests[2].body).confirm, true)
})

test('port rejections keep the stable code for the view-model classifier', async () => {
  const occupancy = facadeFixture({
    async fetch() {
      return response(409, errorPayload('OCCUPIED_BY_OTHER', 'raw server text'))
    },
  })
  const port = clientOccupancyPortFromFacade(occupancy)
  const error = await rejectionOf(() => port.claim({ clientId: validClientId }))
  assert.equal(error instanceof ControlPlaneClientError, true)
  assert.equal(error.code, 'OCCUPIED_BY_OTHER')
  assert.equal(
    clientOccupancyErrorCategory(error),
    'occupied-by-other',
    'the facade classifier agrees with the view-model seam',
  )
})
