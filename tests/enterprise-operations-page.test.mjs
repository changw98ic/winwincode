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
    'apps/client/tsconfig.enterprise-operations-page-tests.json',
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
  `Enterprise operations page did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cacheRoot = resolve(root, '.cache/enterprise-operations-page-tests')
const viewModelModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'enterprise-management-view-model.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'enterprise-operations-page.js',
)).href}`)
const facadeModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'control-plane-client.js',
)).href}`)
const { createEnterpriseManagementViewModel } = viewModelModule
const { mountEnterpriseOperationsPage } = pageModule
const { ControlPlaneClientError } = facadeModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = { kind: 'organization', organizationId: 'org_00000000000000000000000001' }
const subscriptionId = 'sub_00000000000000000000000001'
const sha = value => `sha256:${String(value).repeat(64).slice(0, 64)}`

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) { return canonicalId('req', value) }

function policy(overrides = {}) {
  return {
    id: canonicalId('pol', 1),
    policyKind: 'network',
    mode: 'enforce',
    state: 'active',
    version: 2,
    scope,
    source: { actor, requestId: requestId(90) },
    effectiveAt: '2026-08-27T00:00:00.000Z',
    inheritanceMode: 'tighten',
    baseVersion: null,
    relaxationAuthority: null,
    definitionSha256: sha('1'),
    effectiveDefinitionSha256: sha('1'),
    versionDigest: sha('2'),
    revision: 2,
    updatedAt: '2026-08-27T00:00:00.000Z',
    conditionBody: 'raw condition must not render',
    ...overrides,
  }
}

function fleet(overrides = {}) {
  return {
    id: canonicalId('wpl', 1),
    displayName: 'Remote Build Fleet',
    state: 'healthy',
    registeredWorkers: 5,
    activeLeases: 2,
    availableCapacity: 3,
    labels: ['region:test'],
    revision: 2,
    updatedAt: '2026-08-27T00:00:00.000Z',
    registrationToken: 'raw token must not render',
    ...overrides,
  }
}

function usage(value) {
  return {
    sourceKind: value === 1 ? 'provider' : 'worker',
    bucketStart: `2026-08-2${String(value)}T00:00:00.000Z`,
    bucketEnd: `2026-08-2${String(value + 1)}T00:00:00.000Z`,
    operationCount: value,
    inputTokens: value * 10,
    outputTokens: value * 5,
    runtimeMillis: value * 100,
    storageBytes: value * 1000,
    costMicros: value * 50,
    revision: 2,
  }
}

function auditRecord() {
  return {
    sequence: 7,
    occurredAt: '2026-08-27T01:00:00.000Z',
    category: 'administration',
    action: 'fleet.drain',
    outcome: 'completed',
    actor,
    revision: 2,
    recordSha256: sha('2'),
    payload: 'raw audit payload must not render or export',
  }
}

function integration(overrides = {}) {
  return {
    id: canonicalId('int', 1),
    kind: 'github',
    displayName: 'GitHub Enterprise',
    state: 'enabled',
    configurationSha256: sha('3'),
    lastSyncAt: '2026-08-27T01:30:00.000Z',
    revision: 2,
    updatedAt: '2026-08-27T01:30:00.000Z',
    secretMaterial: 'raw integration secret must not render',
    ...overrides,
  }
}

const areaByQuery = Object.freeze({
  'enterprise.organization.list': 'organization',
  'enterprise.membership.list': 'members',
  'enterprise.project.list': 'projects',
  'enterprise.policy.list': 'policy',
  'enterprise.fleet.list': 'fleet',
  'enterprise.usage.list': 'usage',
  'enterprise.audit.list': 'audit',
  'enterprise.integration.list': 'integration',
})

const kindByArea = Object.freeze({
  organization: 'enterprise_organization_page',
  members: 'enterprise_membership_page',
  projects: 'enterprise_project_repository_page',
  policy: 'enterprise_policy_page',
  fleet: 'enterprise_fleet_page',
  usage: 'enterprise_usage_page',
  audit: 'enterprise_audit_page',
  integration: 'enterprise_integration_page',
})

function contractFake() {
  const queries = []
  const commands = []
  const subscriptions = []
  const revisions = Object.fromEntries(Object.values(areaByQuery).map(area => [area, 2]))
  const allowed = Object.fromEntries(Object.values(areaByQuery).map(area => [area, true]))
  let policies = [policy()]
  let fleets = [fleet()]
  let integrations = [integration()]
  let commandFailure = null

  function items(area, cursor) {
    if (area === 'policy') return policies
    if (area === 'fleet') return fleets
    if (area === 'audit') return [auditRecord()]
    if (area === 'integration') return integrations
    if (area === 'usage') return cursor === null ? [usage(1)] : [usage(2)]
    return []
  }

  return {
    queries,
    commands,
    subscriptions,
    revisions,
    allowed,
    set commandFailure(value) { commandFailure = value },
    async query(request) {
      const area = areaByQuery[request.query]
      queries.push(structuredClone(request))
      if (!allowed[area]) throw new ControlPlaneClientError({
        kind: 'authorization',
        code: 'PERMISSION_DENIED',
        message: 'raw permission record',
        requestId: request.requestId,
        retryable: false,
      })
      const firstUsagePage = area === 'usage' && request.page.cursor === null
      return {
        schemaVersion,
        requestId: request.requestId,
        query: request.query,
        result: {
          kind: kindByArea[area],
          snapshotRevision: revisions[area],
          items: items(area, request.page.cursor),
        },
        page: firstUsagePage
          ? { hasMore: true, nextCursor: 'usage_second_page' }
          : { hasMore: false, nextCursor: null },
      }
    },
    async command(request) {
      commands.push(structuredClone(request))
      if (commandFailure !== null) throw commandFailure
      let area
      let result
      if (request.command === 'enterprise.policy.update') {
        area = 'policy'
        result = policy({
          id: request.payload.policyId,
          policyKind: request.payload.policyKind,
          mode: request.payload.mode,
          state: request.payload.state,
          effectiveAt: request.payload.effectiveAt,
          inheritanceMode: request.payload.inheritanceMode,
          baseVersion: request.payload.baseVersion,
          definitionSha256: request.payload.definitionSha256,
        })
        policies = [result]
      } else if (request.command === 'enterprise.fleet.update') {
        area = 'fleet'
        result = fleet({
          id: request.payload.workerPoolId,
          state: request.payload.action === 'drain' ? 'draining' : 'healthy',
        })
        fleets = [result]
      } else {
        assert.equal(request.command, 'enterprise.integration.update')
        area = 'integration'
        result = integration({
          id: request.payload.integrationId,
          kind: request.payload.kind,
          displayName: request.payload.displayName,
          state: request.payload.state,
          configurationSha256: request.payload.configurationSha256,
        })
        integrations = [result]
      }
      revisions[area] += 1
      result = { ...result, revision: revisions[area] }
      if (area === 'policy') policies = [result]
      if (area === 'fleet') fleets = [result]
      if (area === 'integration') integrations = [result]
      return {
        schemaVersion,
        requestId: request.requestId,
        command: request.command,
        outcome: 'completed',
        previousRevision: request.expectedRevision,
        currentRevision: revisions[area],
        result,
      }
    },
    subscribe(options) {
      subscriptions.push(options)
      return { cursor: null, resume() {}, reconnect() {}, close() {} }
    },
    close() {},
    serverUrl: 'https://control.example/enterprise',
  }
}

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  required = false
  type = ''
  value = ''
  #textContent = ''

  get textContent() { return this.#textContent }

  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }

  append(...children) { this.children.push(...children) }

  replaceChildren(...children) { this.children = [...children] }

  setAttribute(name, value) { this.attributes.set(name, String(value)) }

  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }

  dispatch(name) {
    const event = { preventDefault() {} }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) { return new FakeElement(this, tagName) }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function byClass(rootElement, className) {
  const match = descendants(rootElement).find(node => node.className === className)
  assert.notEqual(match, undefined, `missing .${className}`)
  return match
}

function visibleText(node) {
  return descendants(node).map(current => current.textContent).join(' ')
}

async function waitFor(predicate, label) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (predicate()) return
    await new Promise(resolvePromise => { setImmediate(resolvePromise) })
  }
  assert.fail(`timed out waiting for ${label}`)
}

function createMountedFixture(client = contractFake()) {
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
  const exports = []
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const mounted = mountEnterpriseOperationsPage({
    root: rootElement,
    model,
    onAuditExport(filename, content) { exports.push({ filename, content }) },
    now: () => '2026-08-27T12:00:00.000Z',
  })
  return { client, model, mounted, rootElement, exports }
}

test('browser fake renders bounded operations, pagination, permissions, and no sensitive fields', async () => {
  const client = contractFake()
  client.allowed.integration = false
  const fixture = createMountedFixture(client)
  await waitFor(() => fixture.model.state.status !== 'loading', 'initial operations snapshot')

  assert.equal(fixture.model.state.status, 'partial')
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-integration-fields').disabled, true)
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-policy-fields').disabled, false)
  assert.equal(
    client.queries.filter(request => request.query === 'enterprise.usage.list').length,
    2,
  )
  const text = visibleText(fixture.rootElement)
  for (const visible of [
    'Policy and dry-run',
    'Remote Worker fleets',
    '3 operations',
    'fleet.drain',
    'Integration data is not available for your current role.',
  ]) assert.equal(text.includes(visible), true, visible)
  for (const hidden of [
    'raw condition',
    'raw token',
    'raw audit payload',
    'raw integration secret',
    'raw permission record',
  ]) assert.equal(text.includes(hidden), false, hidden)
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-operations-status').attributes.get('role'), 'status')
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-operations-error').attributes.get('role'), 'alert')
  fixture.mounted.close()
})

test('browser fake applies Policy dry-run, fleet drain, usage refresh, audit export, and Integration status', async () => {
  const fixture = createMountedFixture()
  await waitFor(() => fixture.model.state.status === 'ready', 'initial operations snapshot')

  const setValue = (className, value) => { byClass(fixture.rootElement, className).value = value }
  setValue('wwc-enterprise-policy-id', canonicalId('pol', 1))
  setValue('wwc-enterprise-policy-kind', 'network')
  setValue('wwc-enterprise-policy-mode', 'audit')
  setValue('wwc-enterprise-policy-state', 'active')
  setValue('wwc-enterprise-policy-inheritance', 'tighten')
  setValue('wwc-enterprise-policy-child-override', 'tighten_only')
  setValue('wwc-enterprise-policy-default', 'deny')
  setValue('wwc-enterprise-policy-rule-kind', 'network')
  setValue('wwc-enterprise-policy-rule-effect', 'allow')
  setValue('wwc-enterprise-policy-resource', 'repository:*')
  setValue('wwc-enterprise-policy-condition-digest', sha('4'))
  setValue('wwc-enterprise-policy-definition-digest', sha('5'))
  byClass(fixture.rootElement, 'wwc-enterprise-policy-form').dispatch('submit')
  await waitFor(() => fixture.model.state.areas.policy.revision === 3, 'Policy dry-run update')
  assert.equal(fixture.client.commands[0].payload.mode, 'audit')
  assert.equal(fixture.client.commands[0].payload.effectiveAt, '2026-08-27T12:00:00.000Z')
  assert.equal(fixture.client.commands[0].payload.inheritanceMode, 'tighten')
  assert.equal(fixture.client.commands[0].payload.baseVersion, null)
  assert.equal(fixture.client.commands[0].payload.definition.childOverrideMode, 'tighten_only')
  assert.equal(fixture.client.commands[0].payload.definition.rules.length, 1)

  byClass(fixture.rootElement, 'wwc-enterprise-fleet-drain').dispatch('click')
  await waitFor(() => fixture.model.state.areas.fleet.revision === 3, 'fleet drain')
  assert.equal(fixture.client.commands[1].payload.action, 'drain')
  assert.equal(fixture.client.commands[1].expectedRevision, 2)

  const usageQueries = fixture.client.queries.filter(
    request => request.query === 'enterprise.usage.list',
  ).length
  byClass(fixture.rootElement, 'wwc-enterprise-usage-refresh').dispatch('click')
  await waitFor(() => fixture.client.queries.filter(
    request => request.query === 'enterprise.usage.list',
  ).length === usageQueries + 2, 'usage refresh')

  byClass(fixture.rootElement, 'wwc-enterprise-audit-export').dispatch('click')
  assert.equal(fixture.exports.length, 1)
  assert.equal(fixture.exports[0].filename, 'winwincode-enterprise-audit.csv')
  assert.equal(fixture.exports[0].content.includes('fleet.drain'), true)
  assert.equal(fixture.exports[0].content.includes('raw audit payload'), false)
  assert.equal(fixture.mounted.exportAuditCsv(), fixture.exports[0].content)

  setValue('wwc-enterprise-integration-id', canonicalId('int', 1))
  setValue('wwc-enterprise-integration-kind', 'github')
  setValue('wwc-enterprise-integration-name', 'GitHub Enterprise')
  setValue('wwc-enterprise-integration-state', 'disabled')
  setValue('wwc-enterprise-integration-endpoint', 'https://github.example')
  setValue('wwc-enterprise-integration-tenant', 'enterprise')
  setValue('wwc-enterprise-integration-repository', 'winwincode/core')
  setValue('wwc-enterprise-integration-audience', 'winwincode')
  setValue('wwc-enterprise-integration-digest', sha('6'))
  setValue('wwc-enterprise-integration-credential', canonicalId('crd', 1))
  byClass(fixture.rootElement, 'wwc-enterprise-integration-form').dispatch('submit')
  await waitFor(() => fixture.model.state.areas.integration.revision === 3, 'Integration update')
  assert.equal(fixture.client.commands[2].payload.state, 'disabled')
  assert.equal(fixture.client.commands[2].payload.credentialReferenceId, canonicalId('crd', 1))
  assert.equal(JSON.stringify(fixture.client.commands[2]).includes('secret'), false)

  fixture.client.commandFailure = new ControlPlaneClientError({
    kind: 'server',
    code: 'REVISION_CONFLICT',
    message: 'raw Integration configuration and token',
    requestId: null,
    retryable: false,
  })
  byClass(fixture.rootElement, 'wwc-enterprise-integration-form').dispatch('submit')
  await waitFor(
    () => fixture.model.state.interaction.status === 'revision-conflict',
    'Integration revision conflict',
  )
  assert.equal(
    byClass(fixture.rootElement, 'wwc-enterprise-operations-error-text').textContent,
    'This enterprise setting changed before the update was saved. Review the current snapshot and try again.',
  )
  assert.equal(visibleText(fixture.rootElement).includes('raw Integration'), false)
  fixture.mounted.close()
})

test('enterprise operations page has one view-model boundary and no raw transport or secret field', () => {
  const source = readFileSync(
    resolve(root, 'apps/client/src/enterprise-operations-page.ts'),
    'utf8',
  )
  assert.equal((source.match(/\.\/enterprise-management-view-model\.js/gu) ?? []).length, 1)
  assert.doesNotMatch(source, /\.\/control-plane-client\.js/u)
  assert.doesNotMatch(source, /\bfetch\s*\(|new\s+WebSocket/u)
  assert.doesNotMatch(source, /innerHTML|console\.|localStorage|sessionStorage/u)
  assert.doesNotMatch(source, /secretMaterial|accessToken|apiKey|credentialValue/u)
  assert.match(source, /model\.execute\('policy'/u)
  assert.match(source, /model\.execute\('fleet'/u)
  assert.match(source, /model\.execute\('integration'/u)
})
