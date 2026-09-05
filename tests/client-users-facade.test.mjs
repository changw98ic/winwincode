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
    'apps/client/tsconfig.client-users-tests.json',
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
  `Users facade did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/client-users-tests')
// Plain module paths keep one ControlPlaneClientError class identity across
// the facade, the view-models, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')

const {
  ControlPlaneClientError,
  controlPlaneUserCreateFailure,
  controlPlaneUserPasswordFailure,
  controlPlaneUserStateFailure,
  createControlPlaneClientUsers,
} = facade

const schemaVersion = 'winwincode/v1'
const usersPath = 'https://control.example/api/v1/users'
const usersStatePath = 'https://control.example/api/v1/users/state'
const usersPasswordPath = 'https://control.example/api/v1/users/password'

function account(overrides = {}) {
  return {
    userId: 'usr_00000000000000000000000002',
    username: 'ada',
    normalizedUsername: 'ada',
    role: 'member',
    state: 'active',
    createdAt: '2026-09-04T00:00:00.000Z',
    updatedAt: '2026-09-04T00:00:00.000Z',
    revision: 1,
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

function usersFacade(transport, baseOverrides = {}) {
  return createControlPlaneClientUsers({
    client: baseClient(baseOverrides),
    transport,
  })
}

function recordedTransport(requests, responder) {
  return {
    async fetch(input, init) {
      requests.push({
        input: String(input),
        method: init.method,
        body: typeof init.body === 'string' ? JSON.parse(init.body) : null,
      })
      return responder(requests[requests.length - 1])
    },
  }
}

test('creating a user posts the real wire shape and returns the one-time password', async () => {
  const requests = []
  const users = usersFacade(recordedTransport(requests, () => response(201, {
    schemaVersion,
    user: account(),
    temporaryPassword: 'one-time-secret-1',
  })))

  const outcome = await users.createUser({ username: '  ada  ', role: 'member' })

  assert.deepEqual(requests, [{
    input: usersPath,
    method: 'POST',
    body: { schemaVersion, username: 'ada', role: 'member' },
  }])
  assert.deepEqual(outcome.user, account())
  assert.equal(outcome.temporaryPassword, 'one-time-secret-1')
  assert.ok(Object.isFrozen(outcome))
  assert.ok(Object.isFrozen(outcome.user))
})

test('creation input is validated before a request exists', async () => {
  const requests = []
  const users = usersFacade(recordedTransport(requests, () => response(201, {
    schemaVersion,
    user: account(),
    temporaryPassword: 'one-time-secret-1',
  })))

  await assert.rejects(
    users.createUser({ username: '   ', role: 'member' }),
    error => error instanceof ControlPlaneClientError
      && error.code === 'USERS_CREATE_INPUT_INVALID',
  )
  await assert.rejects(
    users.createUser({ username: 'has space', role: 'member' }),
    error => error.code === 'USERS_CREATE_INPUT_INVALID',
  )
  await assert.rejects(
    users.createUser({ username: 'x'.repeat(97), role: 'member' }),
    error => error.code === 'USERS_CREATE_INPUT_INVALID',
  )
  await assert.rejects(
    users.createUser({ username: 'ada', role: 'admin' }),
    error => error.code === 'USERS_CREATE_INPUT_INVALID',
  )
  assert.equal(requests.length, 0, 'an invalid draft never reaches the wire')
})

test('state changes post the compare-and-swap shape and freeze the account', async () => {
  const requests = []
  const users = usersFacade(recordedTransport(requests, () => response(200, {
    schemaVersion,
    user: account({ state: 'disabled', revision: 2, updatedAt: '2026-09-04T01:00:00.000Z' }),
  })))

  const updated = await users.setUserState({
    userId: 'usr_00000000000000000000000002',
    expectedRevision: 1,
    state: 'disabled',
  })

  assert.deepEqual(requests, [{
    input: usersStatePath,
    method: 'POST',
    body: {
      schemaVersion,
      userId: 'usr_00000000000000000000000002',
      expectedRevision: 1,
      state: 'disabled',
    },
  }])
  assert.equal(updated.state, 'disabled')
  assert.equal(updated.revision, 2)
  assert.ok(Object.isFrozen(updated))

  await assert.rejects(
    users.setUserState({ userId: '', expectedRevision: 1, state: 'active' }),
    error => error.code === 'USERS_STATE_INPUT_INVALID',
  )
  await assert.rejects(
    users.setUserState({ userId: 'usr_x', expectedRevision: 1.5, state: 'active' }),
    error => error.code === 'USERS_STATE_INPUT_INVALID',
  )
})

test('the Owner reset returns the one-time secret; the self form never does', async () => {
  const requests = []
  const users = usersFacade(recordedTransport(requests, ({ body }) => {
    if (body.currentPassword !== undefined) {
      return response(200, { schemaVersion, user: account({ revision: 3 }) })
    }
    return response(200, {
      schemaVersion,
      user: account({ revision: 2 }),
      temporaryPassword: 'one-time-secret-2',
    })
  }))

  const ownerReset = await users.resetUserPassword({
    userId: 'usr_00000000000000000000000002',
    expectedRevision: 1,
  })
  assert.deepEqual(requests.at(-1).body, {
    schemaVersion,
    userId: 'usr_00000000000000000000000002',
    expectedRevision: 1,
  })
  assert.equal(ownerReset.temporaryPassword, 'one-time-secret-2')

  const selfReset = await users.resetUserPassword({
    userId: 'usr_00000000000000000000000002',
    expectedRevision: 2,
    currentPassword: 'one-time-secret-2',
    newPassword: 'rotated-password-9',
  })
  assert.deepEqual(requests.at(-1).body, {
    schemaVersion,
    userId: 'usr_00000000000000000000000002',
    expectedRevision: 2,
    currentPassword: 'one-time-secret-2',
    newPassword: 'rotated-password-9',
  })
  assert.equal(selfReset.temporaryPassword, null, 'a self rotation carries no secret')

  // A wire payload that tried to hand a secret back on the self form is
  // dropped, so the page keeps one secret channel.
  const leaking = usersFacade(recordedTransport([], () => response(200, {
    schemaVersion,
    user: account({ revision: 3 }),
    temporaryPassword: 'should-not-surface',
  })))
  const guarded = await leaking.resetUserPassword({
    userId: 'usr_00000000000000000000000002',
    expectedRevision: 2,
    currentPassword: 'x-current-1',
    newPassword: 'x-new-password-1',
  })
  assert.equal(guarded.temporaryPassword, null)

  await assert.rejects(
    users.resetUserPassword({ userId: 'usr_x', expectedRevision: 1, newPassword: 'only-new' }),
    error => error.code === 'USERS_PASSWORD_INPUT_INVALID',
  )
})

test('the account list read validates and freezes every row', async () => {
  const requests = []
  const users = usersFacade(recordedTransport(requests, () => response(200, {
    schemaVersion,
    users: [
      account(),
      account({
        userId: 'usr_00000000000000000000000001',
        username: 'owner',
        normalizedUsername: 'owner',
        role: 'owner',
      }),
    ],
  })))

  const list = await users.listUsers()

  assert.deepEqual(requests, [{ input: usersPath, method: 'GET', body: null }])
  assert.equal(list.length, 2)
  assert.deepEqual(list[1], {
    userId: 'usr_00000000000000000000000001',
    username: 'owner',
    role: 'owner',
    state: 'active',
    createdAt: '2026-09-04T00:00:00.000Z',
    revision: 1,
  })
  assert.ok(Object.isFrozen(list))
  assert.ok(Object.isFrozen(list[0]))

  const malformed = usersFacade(recordedTransport([], () => response(200, {
    schemaVersion,
    users: [{ userId: 'usr_x', username: 'x', role: 'admin', state: 'active' }],
  })))
  await assert.rejects(
    malformed.listUsers(),
    error => error.code === 'INVALID_USER_ACCOUNT_RESPONSE',
  )
})

test('every stable wire code lands in its presentation failure', async () => {
  async function rejectionOf(status, payload) {
    const users = usersFacade(recordedTransport([], () => response(status, payload)))
    try {
      await users.createUser({ username: 'ada', role: 'member' })
    } catch (error) {
      return error
    }
    return null
  }

  const wrongState = await rejectionOf(409, errorPayload('WRONG_STATE', 'conflict'))
  assert.equal(controlPlaneUserCreateFailure(wrongState), 'username-conflict')
  assert.equal(controlPlaneUserStateFailure(wrongState), 'wrong-state')
  assert.equal(controlPlaneUserPasswordFailure(wrongState), 'wrong-state')

  const notFound = await rejectionOf(404, errorPayload('RESOURCE_NOT_FOUND', 'missing'))
  assert.equal(controlPlaneUserStateFailure(notFound), 'user-not-found')
  assert.equal(controlPlaneUserCreateFailure(notFound), 'user-not-found')

  const permission = await rejectionOf(403, errorPayload('PERMISSION_DENIED', 'owner required'))
  assert.equal(permission.kind, 'authorization')
  assert.equal(controlPlaneUserCreateFailure(permission), 'permission-denied')

  const revision = await rejectionOf(409, errorPayload('REVISION_CONFLICT', 'moved'))
  assert.equal(controlPlaneUserStateFailure(revision), 'revision-conflict')

  const invalid = await rejectionOf(400, errorPayload('INVALID_REQUEST', 'bad'))
  assert.equal(controlPlaneUserCreateFailure(invalid), 'invalid-request')

  const authentication = await rejectionOf(
    401,
    errorPayload('AUTHENTICATION_REQUIRED', 'sign in'),
  )
  assert.equal(authentication.kind, 'authentication')
  assert.equal(controlPlaneUserStateFailure(authentication), 'authentication-required')
  assert.equal(
    controlPlaneUserPasswordFailure(authentication),
    'current-password-wrong',
    'the self form presents 401 as the wrong current password',
  )

  assert.equal(controlPlaneUserCreateFailure(new Error('boom')), 'unavailable')
  assert.equal(controlPlaneUserStateFailure(new Error('boom')), 'unavailable')
  assert.equal(controlPlaneUserPasswordFailure(new Error('boom')), 'unavailable')
})

test('server rejections carry the wire identity and unavailable servers stay retryable', async () => {
  const users = usersFacade(recordedTransport([], () => response(409, errorPayload(
    'WRONG_STATE',
    'username already belongs to another account',
  ))))
  await assert.rejects(
    users.createUser({ username: 'ada', role: 'member' }),
    error => {
      assert.ok(error instanceof ControlPlaneClientError)
      assert.equal(error.code, 'WRONG_STATE')
      assert.equal(error.requestId, 'req_00000000000000000000000001')
      assert.equal(error.retryable, false)
      return true
    },
  )

  const down = usersFacade({
    async fetch() {
      throw new Error('connection refused')
    },
  })
  await assert.rejects(
    down.listUsers(),
    error => error.kind === 'network' && error.retryable,
  )
})

test('an injected users implementation is reused verbatim with normalized input', async () => {
  const calls = []
  const injected = usersFacade({
    async fetch() {
      throw new Error('the injected seam is never on the wire')
    },
  }, {
    async listUsers() { calls.push(['listUsers']); return [] },
    async createUser(input) { calls.push(['createUser', input]); return {} },
    async setUserState(input) { calls.push(['setUserState', input]); return {} },
    async resetUserPassword(input) { calls.push(['resetUserPassword', input]); return {} },
  })

  await injected.listUsers()
  await injected.createUser({ username: '  ada  ', role: 'member' })
  await injected.setUserState({
    userId: 'usr_1',
    expectedRevision: 1,
    state: 'active',
  })
  await injected.resetUserPassword({ userId: 'usr_1', expectedRevision: 2 })

  assert.deepEqual(calls, [
    ['listUsers'],
    ['createUser', { username: 'ada', role: 'member' }],
    ['setUserState', { userId: 'usr_1', expectedRevision: 1, state: 'active' }],
    ['resetUserPassword', { userId: 'usr_1', expectedRevision: 2 }],
  ])
})

test('a schema version drift is presented as the version kind', async () => {
  const users = usersFacade(recordedTransport([], () => response(201, {
    schemaVersion: 'winwincode/v2',
    user: account(),
    temporaryPassword: 'one-time-secret-1',
  })))
  await assert.rejects(
    users.createUser({ username: 'ada', role: 'member' }),
    error => error.kind === 'version' && error.code === 'SCHEMA_VERSION_MISMATCH',
  )
})
