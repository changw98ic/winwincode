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
    'apps/client/tsconfig.enterprise-management-tests.json',
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
  `Enterprise management view-model did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const module = await import(`${pathToFileURL(resolve(
  root,
  '.cache/enterprise-management-tests/enterprise-management-view-model.js',
)).href}`)
const { ControlPlaneClientError } = await import(`${pathToFileURL(resolve(
  root,
  '.cache/enterprise-management-tests/control-plane-client.js',
)).href}`)
const {
  ENTERPRISE_MANAGEMENT_AREAS,
  createEnterpriseManagementSources,
  createEnterpriseManagementViewModel,
} = module

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'organization',
  organizationId: 'org_00000000000000000000000001',
}
const subscriptionId = 'sub_00000000000000000000000001'

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

const events = Object.freeze({
  organization: 'enterprise-organization.invalidated.v1',
  members: 'enterprise-membership.invalidated.v1',
  projects: 'enterprise-project.invalidated.v1',
  policy: 'enterprise-policy.invalidated.v1',
  fleet: 'enterprise-fleet.invalidated.v1',
  usage: 'enterprise-usage.invalidated.v1',
  audit: 'enterprise-audit.invalidated.v1',
  integration: 'enterprise-integration.invalidated.v1',
})

const queryAreas = Object.freeze({
  'enterprise.organization.list': 'organization',
  'enterprise.membership.list': 'members',
  'enterprise.project.list': 'projects',
  'enterprise.policy.list': 'policy',
  'enterprise.fleet.list': 'fleet',
  'enterprise.usage.list': 'usage',
  'enterprise.audit.list': 'audit',
  'enterprise.integration.list': 'integration',
})

const pageKinds = Object.freeze({
  organization: 'enterprise_organization_page',
  members: 'enterprise_membership_page',
  projects: 'enterprise_project_repository_page',
  policy: 'enterprise_policy_page',
  fleet: 'enterprise_fleet_page',
  usage: 'enterprise_usage_page',
  audit: 'enterprise_audit_page',
  integration: 'enterprise_integration_page',
})

function sources() {
  return [...createEnterpriseManagementSources()]
}

function contractFake() {
  const queryCalls = []
  const commandCalls = []
  const subscriptions = []
  const subscriptionHandles = []
  const revisions = Object.fromEntries(ENTERPRISE_MANAGEMENT_AREAS.map(area => [area, 4]))
  const allowed = Object.fromEntries(ENTERPRISE_MANAGEMENT_AREAS.map(area => [area, true]))
  let commandFailure = null
  let queryHook = null

  return {
    queryCalls,
    commandCalls,
    subscriptions,
    subscriptionHandles,
    revisions,
    allowed,
    set commandFailure(value) { commandFailure = value },
    set queryHook(value) { queryHook = value },
    async query(request) {
      const area = queryAreas[request.query]
      assert.notEqual(area, undefined, 'generated query must bind to one enterprise area')
      queryCalls.push({ area, request: structuredClone(request) })
      await queryHook?.({ area, request })
      if (!allowed[area]) throw new ControlPlaneClientError({
        kind: 'authorization',
        code: 'PERMISSION_DENIED',
        message: 'Permission denied.',
        requestId: request.requestId,
        retryable: false,
      })
      const firstFleetPage = area === 'fleet' && request.page.cursor === null
      return {
        schemaVersion,
        requestId: request.requestId,
        query: request.query,
        result: {
          kind: pageKinds[area],
          snapshotRevision: revisions[area],
          items: [],
        },
        page: firstFleetPage
          ? { hasMore: true, nextCursor: 'cursor_fleet_second' }
          : { hasMore: false, nextCursor: null },
      }
    },
    async command(request) {
      commandCalls.push(structuredClone(request))
      if (commandFailure !== null) throw commandFailure
      const area = 'fleet'
      revisions[area] += 1
      return {
        schemaVersion,
        requestId: request.requestId,
        command: request.command,
        outcome: 'completed',
        previousRevision: request.expectedRevision,
        currentRevision: revisions[area],
        result: {
          id: request.payload.workerPoolId,
          displayName: 'Enterprise Fleet',
          revision: revisions[area],
          state: 'draining',
          registeredWorkers: 2,
          activeLeases: 1,
          availableCapacity: 0,
          labels: ['region:local'],
          updatedAt: '2026-08-27T00:00:00.000Z',
        },
      }
    },
    subscribe(options) {
      subscriptions.push(options)
      const handle = {
        cursor: null,
        resume() {},
        reconnect() { this.reconnected = true },
        close() { this.closed = true },
        reconnected: false,
        closed: false,
      }
      subscriptionHandles.push(handle)
      return handle
    },
    close() {},
    serverUrl: 'https://control.example/enterprise',
  }
}

function createFixture(client = contractFake()) {
  let nextRequest = 0
  const model = createEnterpriseManagementViewModel({
    client,
    actor,
    scope,
    subscriptionId,
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
  })
  return { client, model }
}

function fleetDrain(context) {
  return {
    schemaVersion,
    actor: context.actor,
    scope: context.scope,
    requestId: context.requestId,
    command: 'enterprise.fleet.update',
    expectedRevision: context.expectedRevision,
    payload: {
      workerPoolId: 'wpl_00000000000000000000000001',
      action: 'drain',
      reason: 'Enterprise fleet maintenance.',
    },
  }
}

function fleetInvalidationFrame(sequence = 1) {
  return {
    type: 'event.v1',
    subscriptionId,
    eventId: canonicalId('evt', sequence),
    authorizationEpoch: 1,
    occurredAt: '2026-08-27T00:00:00.000Z',
    scope,
    sequence,
    stream: { kind: 'scope' },
    source: { kind: 'control-plane', actor, component: 'enterprise-management' },
    event: {
      type: 'enterprise-fleet.invalidated.v1',
      snapshotRevision: sequence,
      reloadQueries: ['enterprise.fleet.list'],
    },
  }
}

test('all eight enterprise areas load independently with bounded pagination and permission state', async () => {
  const client = contractFake()
  client.allowed.policy = false
  const { model } = createFixture(client)
  await model.start()

  assert.deepEqual(Object.keys(model.state.areas), ENTERPRISE_MANAGEMENT_AREAS)
  assert.equal(model.state.status, 'partial')
  assert.equal(model.state.areas.policy.status, 'permission-denied')
  assert.equal(model.state.areas.policy.permission, 'denied')
  assert.deepEqual(model.state.areas.policy.pages, [])
  for (const area of ENTERPRISE_MANAGEMENT_AREAS.filter(value => value !== 'policy')) {
    assert.equal(model.state.areas[area].status, 'ready')
    assert.equal(model.state.areas[area].permission, 'allowed')
    assert.equal(model.state.areas[area].revision, 4)
  }
  assert.equal(model.state.areas.fleet.pages.length, 2)
  const fleetQueries = client.queryCalls.filter(call => call.area === 'fleet')
  assert.deepEqual(fleetQueries.map(call => call.request.page), [
    { cursor: null, limit: 100 },
    { cursor: 'cursor_fleet_second', limit: 100 },
  ])
  assert.equal(client.subscriptions.length, 1)
  assert.deepEqual(client.subscriptions[0].subscription, {
    scope,
    stream: { kind: 'scope' },
    eventTypes: [
      'enterprise-organization.invalidated.v1',
      'enterprise-membership.invalidated.v1',
      'enterprise-project.invalidated.v1',
      'enterprise-policy.invalidated.v1',
      'enterprise-fleet.invalidated.v1',
      'enterprise-usage.invalidated.v1',
      'enterprise-audit.invalidated.v1',
      'enterprise-integration.invalidated.v1',
    ],
  })

  client.allowed.policy = true
  await model.refresh('policy')
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.areas.policy.permission, 'allowed')
  model.close()
})

test('revision conflicts keep the last complete page until an explicit retry succeeds', async () => {
  const { client, model } = createFixture()
  await model.start()
  const previousPages = model.state.areas.fleet.pages
  client.commandFailure = new ControlPlaneClientError({
    kind: 'server',
    code: 'REVISION_CONFLICT',
    message: 'Revision conflict.',
    requestId: null,
    retryable: false,
  })
  await model.execute('fleet', fleetDrain)
  assert.equal(model.state.areas.fleet.status, 'revision-conflict')
  assert.equal(model.state.areas.fleet.revision, 4)
  assert.equal(model.state.areas.fleet.pages, previousPages)
  assert.equal(model.state.interaction.status, 'revision-conflict')
  assert.equal(client.commandCalls[0].expectedRevision, 4)

  client.commandFailure = null
  await model.execute('fleet', fleetDrain)
  assert.equal(model.state.areas.fleet.status, 'ready')
  assert.equal(model.state.areas.fleet.revision, 5)
  assert.equal(model.state.interaction.status, 'idle')
  assert.equal(client.commandCalls[1].expectedRevision, 4)
  model.close()
})

test('a completed mutation keeps the prior snapshot until its revision is observable', async () => {
  const { client, model } = createFixture()
  await model.start()
  const previousPages = model.state.areas.fleet.pages
  client.queryHook = ({ area }) => {
    if (area === 'fleet') client.revisions.fleet = 4
  }

  await model.execute('fleet', fleetDrain)

  assert.equal(model.state.areas.fleet.status, 'error')
  assert.equal(model.state.areas.fleet.revision, 4)
  assert.equal(model.state.areas.fleet.pages, previousPages)
  assert.equal(
    model.state.areas.fleet.error.code,
    'ENTERPRISE_MANAGEMENT_SNAPSHOT_STALE',
  )

  client.queryHook = null
  client.revisions.fleet = 5
  await model.refresh('fleet')
  assert.equal(model.state.areas.fleet.status, 'ready')
  assert.equal(model.state.areas.fleet.revision, 5)
  model.close()
})

test('generated event invalidation refreshes only affected areas and access revocation clears data', async () => {
  const { client, model } = createFixture()
  await model.start()
  const subscription = client.subscriptions[0]
  const before = Object.fromEntries(ENTERPRISE_MANAGEMENT_AREAS.map(area => [
    area,
    client.queryCalls.filter(call => call.area === area).length,
  ]))
  client.revisions.fleet = 8
  await subscription.onEvent(fleetInvalidationFrame())
  assert.equal(model.state.realtime, 'subscribed')
  assert.equal(model.state.areas.fleet.revision, 8)
  for (const area of ENTERPRISE_MANAGEMENT_AREAS) {
    const increment = area === 'fleet' ? 2 : 0
    assert.equal(
      client.queryCalls.filter(call => call.area === area).length,
      before[area] + increment,
    )
  }

  let releaseStaleRefresh
  let staleRefreshStarted = false
  client.queryHook = ({ area, request }) => {
    if (area !== 'fleet' || request.page.cursor !== null || staleRefreshStarted) return undefined
    staleRefreshStarted = true
    return new Promise(resolve => { releaseStaleRefresh = resolve })
  }
  const staleRefresh = subscription.onEvent(fleetInvalidationFrame(2))
  while (!staleRefreshStarted) await new Promise(resolve => { setImmediate(resolve) })
  await subscription.onEvent(fleetInvalidationFrame(3))
  assert.equal(model.state.realtime, 'subscribed')
  releaseStaleRefresh()
  await staleRefresh
  assert.equal(model.state.realtime, 'subscribed')
  client.queryHook = null

  subscription.onAuthorizationRevoked()
  assert.equal(model.state.status, 'authentication-required')
  assert.equal(model.state.realtime, 'access-revoked')
  assert.equal(client.subscriptionHandles[0].closed, true)
  for (const area of ENTERPRISE_MANAGEMENT_AREAS) {
    assert.equal(model.state.areas[area].permission, 'denied')
    assert.deepEqual(model.state.areas[area].pages, [])
  }
  model.close()
})

test('source and transport invariants fail closed without raw browser networking', async () => {
  assert.throws(
    () => createEnterpriseManagementViewModel({
      client: contractFake(),
      actor,
      scope,
      subscriptionId,
      nextRequestId: () => requestId(1),
      sources: sources().slice(0, -1),
    }),
    /Every enterprise management area requires one generated query source/u,
  )

  const malformedSources = sources()
  malformedSources[0] = {
    ...malformedSources[0],
    query(context) {
      return {
        ...sources()[0].query(context),
        scope: { ...scope, organizationId: canonicalId('org', 2) },
      }
    },
  }
  const malformed = createEnterpriseManagementViewModel({
    client: contractFake(),
    actor,
    scope,
    subscriptionId,
    nextRequestId: () => requestId(99),
    sources: malformedSources,
  })
  await malformed.start()
  assert.equal(malformed.state.areas.organization.status, 'error')
  assert.equal(malformed.state.areas.organization.error.code, 'ENTERPRISE_MANAGEMENT_QUERY_INVALID')
  malformed.close()

  const source = readFileSync(
    resolve(root, 'apps/client/src/enterprise-management-view-model.ts'),
    'utf8',
  )
  assert.doesNotMatch(source, /\bfetch\s*\(/u)
  assert.doesNotMatch(source, /new\s+WebSocket/u)
  assert.doesNotMatch(source, /innerHTML|console\.|localStorage|sessionStorage/u)
  assert.equal((source.match(/\.\/control-plane-client\.js/gu) ?? []).length, 1)
  assert.match(source, /import type \{[\s\S]*QueryRequest,[\s\S]*\} from '\.\/generated\/contracts\.js'/u)
  assert.match(source, /import type \{[\s\S]*QueryResultResponse,[\s\S]*\} from '\.\/generated\/contracts\.js'/u)
  assert.match(source, /import type \{[\s\S]*CommandRequest,[\s\S]*\} from '\.\/generated\/contracts\.js'/u)
})
