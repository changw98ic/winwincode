import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
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
  'control-plane-client.js',
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
const workspaceScope = {
  kind: 'workspace',
  organizationId: 'org_00000000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
}
const projectScope = {
  kind: 'project',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
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

test('signed-out and restoring sessions hide every navigation entry', () => {
  for (const status of ['signed-out', 'restoring']) {
    const capabilities = capabilityMap(status)
    for (const surface of ['home', 'chat', 'strongflow', 'settings', 'attention', 'enterprise']) {
      assert.equal(
        capabilities[surface].capability,
        'hidden',
        `${status} ${surface}`,
      )
    }
    assert.equal(projectionForSession({ ...SIGNED_OUT, status }).deployment, 'unknown')
  }
})

test('a personal repository-only session trims Enterprise and keeps product areas available', () => {
  const capabilities = capabilityMap('signed-in', sessionWith([repositoryScope]))
  assert.equal(capabilities.chat.capability, 'available')
  assert.equal(capabilities.strongflow.capability, 'available')
  assert.equal(capabilities.settings.capability, 'available')
  assert.equal(capabilities.attention.capability, 'available')
  assert.equal(capabilities.enterprise.capability, 'hidden')
  assert.equal(capabilities.chat.reason, 'authorized-scope')
  assert.equal(capabilities.enterprise.reason, 'no-enterprise-scope')
})

test('an enterprise-hierarchy scope makes Enterprise enterable', () => {
  for (const scope of [organizationScope, workspaceScope, projectScope]) {
    const capabilities = capabilityMap('signed-in', sessionWith([scope, repositoryScope]))
    assert.equal(capabilities.enterprise.capability, 'available', scope.kind)
    assert.equal(capabilities.enterprise.reason, 'enterprise-scope')
    assert.equal(capabilities.chat.capability, 'available')
  }
})

test('known enterprise deployment keeps missing permissions visible as disabled', () => {
  const capabilities = capabilityMap(
    'signed-in',
    sessionWith([repositoryScope]),
    null,
    { deployment: 'enterprise' },
  )
  assert.equal(capabilities.chat.capability, 'available')
  assert.equal(capabilities.enterprise.capability, 'disabled')
  assert.equal(capabilities.enterprise.reason, 'no-enterprise-scope')
})

test('query capability facts distinguish read-only and denied entries', () => {
  const session = sessionWith([organizationScope, repositoryScope])
  const readOnly = capabilityMap('signed-in', session, null, {
    surfaceAccess: { enterprise: 'read-only' },
  })
  const denied = capabilityMap('signed-in', session, null, {
    surfaceAccess: { enterprise: 'denied' },
  })
  assert.equal(readOnly.enterprise.capability, 'read-only')
  assert.equal(readOnly.enterprise.reason, 'read-only-capability')
  assert.equal(denied.enterprise.capability, 'disabled')
  assert.equal(denied.enterprise.reason, 'capability-denied')
})

test('deployment projection distinguishes personal from enterprise sessions', () => {
  assert.equal(
    projection('signed-in', sessionWith([repositoryScope])).deployment,
    'personal',
  )
  assert.equal(
    projection('signed-in', sessionWith([organizationScope])).deployment,
    'enterprise',
  )
  assert.equal(
    projection('signed-in', sessionWith([workspaceScope, repositoryScope])).deployment,
    'enterprise',
  )
  assert.equal(projection('signed-out').deployment, 'unknown')
})

test('a session without any scope hides product areas without crashing', () => {
  const capabilities = capabilityMap('signed-in', sessionWith([]))
  for (const surface of ['home', 'chat', 'strongflow', 'settings', 'attention', 'enterprise']) {
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
    assert.equal(capabilities.enterprise.capability, 'hidden', status)
    assert.equal(capabilities.chat.capability, 'hidden', status)
  }
})

test('surfaceCapabilityForHash resolves the exact surface a URL will enter', () => {
  assert.equal(surfaceCapabilityForHash('#/enterprise/resources', {
    status: 'signed-in',
    session: sessionWith([organizationScope]),
    error: null,
  }).surface.id, 'enterprise')
  assert.equal(surfaceCapabilityForHash('#/chat?session=psn_1', {
    status: 'signed-in',
    session: sessionWith([repositoryScope]),
    error: null,
  }).surface.id, 'chat')
  // UI-504: an address without a product path, and an unknown path, both enter
  // the canonical Home dashboard instead of an arbitrary product area.
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
