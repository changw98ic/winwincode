import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.navigation-capability-tests.json',
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
  `Navigation capability module did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cacheRoot = resolve(root, '.cache/navigation-capability-tests')
const navigationModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'navigation-capability.js',
)).href}`)
const facadeModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'community-control-plane-client.js',
)).href}`)
const { projectionForSession, surfaceCapabilityForHash } = navigationModule
const { ControlPlaneClientError } = facadeModule

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
  organizationId: 'org_00000000000000000000000001',
}

function sessionWith(scopes, sessionActor = actor) {
  return {
    schemaVersion,
    expiresAt: '2099-09-02T00:00:00.000Z',
    actor: sessionActor,
    authorizedScopes: scopes,
  }
}

function projection(status, session = null, error = null, facts = {}) {
  return projectionForSession(Object.freeze({ status, session, error }), facts)
}

function capabilityMap(status, session = null, error = null, facts = {}) {
  const projectionValue = projection(status, session, error, facts)
  return Object.fromEntries(projectionValue.surfaces.map(surface => [
    surface.surface.id,
    surface,
  ]))
}

const SIGNED_OUT = { status: 'signed-out', session: null, error: null }
const PRODUCT_SURFACES = ['home', 'chat', 'strongflow', 'settings', 'attention']

test('signed-out and restoring sessions hide every navigation entry', () => {
  for (const status of ['signed-out', 'restoring']) {
    const capabilities = capabilityMap(status)
    for (const surface of PRODUCT_SURFACES) {
      assert.equal(
        capabilities[surface].capability,
        'hidden',
        `${status} ${surface}`,
      )
    }
    assert.equal(projectionForSession({ ...SIGNED_OUT, status }).deployment, 'unknown')
  }
})

test('a personal repository-only session keeps every product area available', () => {
  const capabilities = capabilityMap('signed-in', sessionWith([repositoryScope]))
  assert.equal(capabilities.home.capability, 'available')
  assert.equal(capabilities.chat.capability, 'available')
  assert.equal(capabilities.strongflow.capability, 'available')
  assert.equal(capabilities.settings.capability, 'available')
  assert.equal(capabilities.attention.capability, 'available')
  assert.equal(capabilities.chat.reason, 'authorized-scope')
})

test('organization-only sessions still require a repository Scope for product surfaces', () => {
  const capabilities = capabilityMap('signed-in', sessionWith([organizationScope]))
  for (const surface of PRODUCT_SURFACES) {
    assert.equal(capabilities[surface].capability, 'hidden', surface)
    assert.equal(capabilities[surface].reason, 'no-repository-scope', surface)
  }
})

test('query capability facts distinguish read-only and denied entries', () => {
  const session = sessionWith([repositoryScope])
  const readOnly = capabilityMap('signed-in', session, null, {
    surfaceAccess: { chat: 'read-only' },
  })
  const denied = capabilityMap('signed-in', session, null, {
    surfaceAccess: { chat: 'denied' },
  })
  assert.equal(readOnly.chat.capability, 'read-only')
  assert.equal(readOnly.chat.reason, 'read-only-capability')
  assert.equal(denied.chat.capability, 'disabled')
  assert.equal(denied.chat.reason, 'capability-denied')
})

test('deployment projection is personal only when a repository Scope exists', () => {
  assert.equal(
    projection('signed-in', sessionWith([repositoryScope])).deployment,
    'personal',
  )
  assert.equal(
    projection('signed-in', sessionWith([organizationScope])).deployment,
    'unknown',
  )
  assert.equal(projection('signed-out').deployment, 'unknown')
})

test('a session without any scope hides product areas without crashing', () => {
  const capabilities = capabilityMap('signed-in', sessionWith([]))
  for (const surface of PRODUCT_SURFACES) {
    assert.equal(capabilities[surface].capability, 'hidden', surface)
  }
})

test('runtime revocation moves every entry back to hidden', () => {
  const revoked = new ControlPlaneClientError({
    kind: 'authentication',
    code: 'AUTHENTICATION_REQUIRED',
    message: 'private revoked-session diagnostics',
    requestId: null,
    retryable: false,
  })
  for (const status of ['authentication-required', 'signed-out', 'error']) {
    const capabilities = capabilityMap(status, null, revoked)
    assert.equal(capabilities.chat.capability, 'hidden', status)
    assert.equal(capabilities.home.capability, 'hidden', status)
  }
})

test('surfaceCapabilityForHash resolves the exact surface a URL will enter', () => {
  assert.equal(surfaceCapabilityForHash('#/chat?session=psn_1', {
    status: 'signed-in',
    session: sessionWith([repositoryScope]),
    error: null,
  }).surface.id, 'chat')
  // An unknown path falls back to Home instead of an Enterprise surface.
  assert.equal(surfaceCapabilityForHash('#/enterprise/resources', {
    status: 'signed-in',
    session: sessionWith([repositoryScope]),
    error: null,
  }).surface.id, 'home')
  assert.equal(surfaceCapabilityForHash('', {
    status: 'signed-in',
    session: sessionWith([repositoryScope]),
    error: null,
  }).surface.id, 'home')
  assert.equal(surfaceCapabilityForHash('#/unknown-route', {
    status: 'signed-in',
    session: sessionWith([repositoryScope]),
    error: null,
  }).surface.id, 'home')
})

test('projection is a read-only view that never mutates the session', () => {
  const frozen = Object.freeze(sessionWith([repositoryScope]))
  const state = Object.freeze({ status: 'signed-in', session: frozen, error: null })
  const first = projectionForSession(state)
  const second = projectionForSession(state)
  assert.deepEqual(first, second)
  assert.equal(Object.isFrozen(first), true)
  assert.equal(Object.isFrozen(first.surfaces), true)
  assert.equal(Object.isFrozen(first.surfaces[0]), true)
  assert.equal(frozen.authorizedScopes.length, 1)
})
