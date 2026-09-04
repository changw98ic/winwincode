import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
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
    'apps/client/tsconfig.usage-health-tests.json',
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
  `Usage and health summary boundary did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cacheRoot = resolve(root, '.cache/usage-health-tests')
const viewModelModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'usage-health-view-model.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'usage-health-page.js',
)).href}`)

const { ControlPlaneClientError } = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'control-plane-client.js',
)).href}`)

const { createUsageHealthViewModel } = viewModelModule
const { mountUsageHealthSummary, usageHealthPresentation } = pageModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const deliveryOne = 'dlv_00000000000000000000000001'
const deliveryTwo = 'dlv_00000000000000000000000002'
const productSessionOne = 'psn_00000000000000000000000001'
const productSessionTwo = 'psn_00000000000000000000000002'
const productSessionOutside = 'psn_00000000000000000000000009'
const stageRunOne = 'str_00000000000000000000000001'
const stageRunTwo = 'str_00000000000000000000000002'
const stageRunThree = 'str_00000000000000000000000003'
// Planted marker: if any secret-unsafe field reaches the panel this string becomes
// readable page text and the assertion below fails.
const SECRET_MARKER = 'vault-locator-secret-marker'
const FRESH = '2026-09-03T08:29:30.000Z'
const STALE = '2026-09-03T07:00:00.000Z'
const ROTATED = '2026-09-01T00:00:00.000Z'
const GENERATED = Date.parse('2026-09-03T08:30:00.000Z')

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function response(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: { hasMore: false, nextCursor: null },
  }
}

function usage(...entries) {
  return { sourceRef: 'runtime:usage', totals: entries.map(([name, value]) => ({ name, value })) }
}

function agent(role, index) {
  return {
    nickname: role === null ? null : `${role}-${index}`,
    parentThreadId: null,
    path: null,
    role,
    sourceRef: `runtime:agent-${index}`,
    status: 'completed',
    threadId: canonicalId('thr', index + 1),
  }
}

function runtimeSession(stageRunId, overrides = {}) {
  return {
    activities: [],
    agentEdges: [],
    agents: [agent('planner', 1)],
    asOfSequence: 12,
    attempt: 1,
    codexThreadId: canonicalId('thr', 1),
    deliveryTaskId: null,
    diffSummary: null,
    executionJobId: canonicalId('job', 1),
    fencingToken: '1',
    leaseId: canonicalId('lse', 1),
    plan: null,
    productSessionId: productSessionOne,
    recovery: {
      failureCount: 0,
      lastFailureSourceRef: null,
      latestRecoverySourceRef: null,
      recoveryCount: 0,
      state: 'none',
    },
    sessionBindingId: 'binding-1',
    stageRunId,
    usage: usage(['input_tokens', 100], ['cached_input_tokens', 20], ['output_tokens', 50], [
      'total_tokens',
      170,
    ]),
    workerSessionId: canonicalId('wss', 1),
    ...overrides,
  }
}

function runtimeSnapshot(productSessionId, deliveryId, stageRunId, sessions, rebuiltAt) {
  return {
    deliveryId,
    eventCursor: {
      deliveryId,
      eventId: canonicalId('evt', 1),
      kind: 'delivery',
      sequence: 12,
      stageRunId,
      stream: { kind: 'delivery' },
    },
    kind: 'runtime_projection',
    lastProjectionSequence: 12,
    productSessionId,
    readCursor: null,
    rebuiltAt,
    revision: 3,
    sessions,
    stageRunId,
  }
}

function delivery(overrides = {}) {
  return {
    activeStageRunId: stageRunOne,
    deliveryId: deliveryOne,
    openAttentionCount: 0,
    ownership: {
      organizationId: scope.organizationId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
      workspaceId: scope.workspaceId,
    },
    revision: 5,
    schemaVersion,
    status: 'executing',
    taskCounts: { active: 1, blocked: 0, completed: 0, failed: 0, pending: 0, total: 1, verifying: 0 },
    title: 'First Delivery',
    updatedAt: '2026-09-03T08:30:00.000Z',
    ...overrides,
  }
}

function modelRoute(providerId, modelId, overrides = {}) {
  return {
    route: {
      providerId,
      modelId,
      credentialReferenceId: canonicalId('crd', 1),
    },
    status: 'enabled',
    reason: 'ready',
    isDefault: false,
    providerDisplayName: `${providerId} display`,
    modelDisplayName: `${modelId} display`,
    contextWindowTokens: 400000,
    maxOutputTokens: 128000,
    reasoningEfforts: [],
    toolSupport: 'parallel',
    catalogSource: scope,
    catalogVersion: 1,
    credentialRotationVersion: 1,
    providerVersion: 1,
    modelVersion: 1,
    ...overrides,
  }
}

function credential(overrides = {}) {
  return {
    displayName: 'Primary provider key',
    id: canonicalId('crd', 1),
    lastRotatedAt: ROTATED,
    providerId: 'openai',
    revokedAt: null,
    rotationVersion: 3,
    secretState: 'available',
    updatedAt: ROTATED,
    [SECRET_MARKER]: true,
    ...overrides,
  }
}

function worker(overrides = {}) {
  return {
    capacity: 2,
    id: canonicalId('wrk', 1),
    lastHeartbeatAt: FRESH,
    revision: 2,
    state: 'enabled',
    ...overrides,
  }
}

/**
 * Full projection fixture. Every field the summary reads is present so the
 * deterministic suite observes the same facts the Control Plane publishes.
 */
function baselineFixtures() {
  return {
    'settings.get': { revision: 4, defaultModelRoute: null, workerConcurrencyLimit: 4 },
    'session.list': {
      kind: 'product_session_page',
      items: [
        {
          id: productSessionOne,
          projectId: scope.projectId,
          repositoryId: scope.repositoryId,
          revision: 2,
          state: 'running',
          title: 'First Chat',
          updatedAt: '2026-09-03T08:30:00.000Z',
        },
        {
          id: productSessionTwo,
          projectId: scope.projectId,
          repositoryId: scope.repositoryId,
          revision: 2,
          state: 'running',
          title: 'Second Chat',
          updatedAt: '2026-09-03T08:20:00.000Z',
        },
        {
          id: productSessionOutside,
          projectId: scope.projectId,
          repositoryId: 'rep_00000000000000000000000002',
          revision: 2,
          state: 'running',
          title: 'Another repository chat',
          updatedAt: '2026-09-03T08:40:00.000Z',
        },
      ],
    },
    'runtime.projection.get': (request) => {
      const id = request.parameters.productSessionId
      if (id === productSessionOne) {
        return runtimeSnapshot(productSessionOne, deliveryOne, stageRunOne, [
          runtimeSession(stageRunOne, {
            agents: [agent('planner', 1), agent('implementer', 2)],
            recovery: {
              failureCount: 2,
              lastFailureSourceRef: 'runtime:failure-2',
              latestRecoverySourceRef: 'runtime:recovery-3',
              recoveryCount: 1,
              state: 'recovered',
            },
          }),
          runtimeSession(stageRunTwo, {
            agents: [agent(null, 3)],
            executionJobId: canonicalId('job', 2),
            usage: usage(['input_tokens', 10], ['output_tokens', 5], ['schema_version', 1]),
            workerSessionId: canonicalId('wss', 2),
          }),
        ], '2026-09-03T08:30:00.000Z')
      }
      if (id === productSessionTwo) {
        return runtimeSnapshot(productSessionTwo, deliveryTwo, stageRunThree, [
          runtimeSession(stageRunThree, {
            agents: [agent('reviewer', 4)],
            executionJobId: canonicalId('job', 3),
            productSessionId: productSessionTwo,
            usage: null,
            workerSessionId: canonicalId('wss', 3),
          }),
        ], '2026-09-03T08:20:00.000Z')
      }
      throw new Error(`unexpected runtime projection read: ${id}`)
    },
    'delivery.list': {
      kind: 'delivery_page',
      items: [
        delivery(),
        delivery({
          activeStageRunId: null,
          deliveryId: deliveryTwo,
          openAttentionCount: 3,
          status: 'needs-attention',
          title: 'Second Delivery',
          updatedAt: '2026-09-03T07:00:00.000Z',
        }),
      ],
    },
    'model.route.availability.list': {
      kind: 'model_route_availability_page',
      scope,
      requestPoolSource: {
        kind: 'project',
        organizationId: scope.organizationId,
        workspaceId: scope.workspaceId,
        projectId: scope.projectId,
      },
      requestPoolRevision: 1,
      settingsRevision: 4,
      settingsSource: scope,
      defaultProviderId: 'openai',
      defaultModelId: 'gpt-5',
      status: 'enabled',
      reason: 'ready',
      items: [
        modelRoute('openai', 'gpt-5', { isDefault: true }),
        modelRoute('anthropic', 'claude', {
          status: 'disabled',
          reason: 'provider_or_model_disabled',
        }),
        modelRoute('mistral', 'mistral-large', {
          reason: 'request_pool_unavailable',
        }),
      ],
    },
    'credential.reference.list': {
      kind: 'credential_reference_page',
      items: [
        credential(),
        credential({
          displayName: 'Backup provider key',
          id: canonicalId('crd', 2),
          providerId: 'anthropic',
          revokedAt: '2026-09-02T00:00:00.000Z',
          rotationVersion: 1,
          secretState: 'revoked',
          updatedAt: '2026-09-02T00:00:00.000Z',
          [SECRET_MARKER]: true,
        }),
      ],
    },
    'worker.list': {
      kind: 'worker_page',
      items: [
        worker(),
        worker({ capacity: 0, id: canonicalId('wrk', 2) }),
        worker({ capacity: 3, id: canonicalId('wrk', 3), state: 'draining' }),
        worker({
          capacity: 0,
          id: canonicalId('wrk', 4),
          lastHeartbeatAt: null,
          state: 'offline',
        }),
        worker({ capacity: 1, id: canonicalId('wrk', 5), lastHeartbeatAt: STALE }),
        worker({ capacity: 1, id: canonicalId('wrk', 6), lastHeartbeatAt: null }),
      ],
    },
  }
}

function fixtureClient(fixtures, options = {}) {
  const queries = []
  let failures = 0
  return {
    queries,
    serverUrl: 'https://control.example/usage-health',
    failures: () => failures,
    async restore() {
      return {
        schemaVersion,
        expiresAt: '2099-09-03T00:00:00.000Z',
        actor,
        authorizedScopes: [scope],
      }
    },
    async login() { throw new Error('unexpected login') },
    async logout() {},
    async command() { throw new Error('unexpected command') },
    async query(request, requestOptions) {
      queries.push(structuredClone(request))
      if (options.throwOnQuery === request.query) {
        failures += 1
        throw new ControlPlaneClientError({
          kind: options.errorKind === 'authentication' ? 'authentication' : 'protocol',
          code: options.errorCode ?? 'CONTROL_PLANE_UNAVAILABLE',
          message: 'The Control Plane could not answer the read.',
          requestId: request.requestId,
          retryable: options.errorKind !== 'authentication',
        })
      }
      if (request.query === 'runtime.projection.get') {
        if (request.parameters.kind !== 'product-session') {
          throw new Error('the summary must read product-session runtime projections only')
        }
        return response(request, fixtures['runtime.projection.get'](request))
      }
      const fixture = fixtures[request.query]
      if (fixture === undefined) throw new Error(`unexpected query: ${request.query}`)
      return response(request, structuredClone(fixture))
    },
    subscribe() {
      return { cursor: null, resume() {}, reconnect() {}, close() {} }
    },
    close() {},
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
  id = ''
  type = ''
  value = ''
  #ownText = ''

  get textContent() {
    return this.#ownText + this.children.map(child => child.textContent).join('')
  }
  set textContent(value) {
    this.#ownText = String(value)
    this.children = []
  }
  append(...children) { this.children.push(...children) }
  replaceChildren(...children) { this.children = [...children] }
  insertBefore(node, reference) {
    if (reference === null || reference === node || !this.children.includes(reference)) {
      if (!this.children.includes(node)) this.children.push(node)
      return node
    }
    this.children.splice(this.children.indexOf(reference), 0, node)
    return node
  }
  get childNodes() { return this.children }
  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }
  remove() { this.children = [] }
  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }
  removeEventListener(name, listener) {
    this.listeners.set(
      name,
      (this.listeners.get(name) ?? []).filter(candidate => candidate !== listener),
    )
  }
  dispatchEvent(event) {
    for (const listener of [...(this.listeners.get(event.type) ?? [])]) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) { return new FakeElement(this, tagName) }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

// Real DOM class matching is token based, so one node can carry the shared row
// class and its dimension class at the same time.
function hasClass(node, className) {
  return typeof node.className === 'string' && node.className.split(/\s+/u).includes(className)
}

function findByClass(node, className) {
  return descendants(node).find(candidate => hasClass(candidate, className)) ?? null
}

function findAllByClass(node, className) {
  return descendants(node).filter(candidate => hasClass(candidate, className))
}

let requestCounter = 0

function mount(fixtures = baselineFixtures(), options = {}) {
  const client = fixtureClient(fixtures, options.client ?? {})
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'section')
  const model = createUsageHealthViewModel({
    client,
    actor,
    scope,
    nextRequestId: () => requestId((requestCounter += 1).toString()),
    nowMillis: options.nowMillis ?? (() => GENERATED),
    ...(options.staleAfterMillis === undefined ? {} : { staleAfterMillis: options.staleAfterMillis }),
    ...(options.sessionLimit === undefined ? {} : { sessionLimit: options.sessionLimit }),
  })
  const view = mountUsageHealthSummary({ root: rootElement, model })
  return { client, document, model, rootElement, view }
}

async function started(options = {}) {
  const mounted = mount(options.fixtures, options)
  await mounted.model.start()
  return mounted
}

function rows(rootElement, className) {
  return findAllByClass(rootElement, className)
}

function rowText(row) {
  return descendants(row).map(node => node.textContent).join(' ')
}

test('usage is aggregated per Delivery from runtime token totals', async () => {
  const { model } = await started()
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.byDelivery.length, 2)
  const first = model.state.byDelivery.find(row => row.key === deliveryOne)
  assert.deepEqual({
    dimension: first.dimension,
    label: first.label,
    sessionCount: first.sessionCount,
    inputTokens: first.inputTokens,
    cachedInputTokens: first.cachedInputTokens,
    outputTokens: first.outputTokens,
    totalTokens: first.totalTokens,
    tokensKnown: first.tokensKnown,
    attribution: first.attribution,
    durationMillis: first.durationMillis,
    durationKnown: first.durationKnown,
  }, {
    dimension: 'delivery',
    label: 'First Delivery',
    sessionCount: 2,
    inputTokens: 110,
    cachedInputTokens: 20,
    outputTokens: 55,
    totalTokens: 170,
    tokensKnown: true,
    attribution: 'session',
    durationMillis: null,
    durationKnown: false,
  })
  const second = model.state.byDelivery.find(row => row.key === deliveryTwo)
  assert.equal(second.tokensKnown, false)
  assert.equal(second.sessionCount, 1)
})

test('usage is aggregated per StageRun including the session that reported no usage', async () => {
  const { model } = await started()
  const keys = model.state.byStageRun.map(row => row.key)
  assert.deepEqual(keys, [stageRunOne, stageRunTwo, stageRunThree])
  const first = model.state.byStageRun[0]
  assert.equal(first.inputTokens, 100)
  assert.equal(first.totalTokens, 170)
  const reported = model.state.byStageRun[1]
  assert.equal(reported.inputTokens, 10)
  assert.deepEqual(reported.unknownMetrics, ['schema_version'])
  const missing = model.state.byStageRun[2]
  assert.equal(missing.tokensKnown, false)
  assert.equal(missing.durationKnown, false)
})

test('usage per Role overlaps StageRun usage and marks unlabelled agents as unknown', async () => {
  const { model } = await started()
  const planner = model.state.byRole.find(row => row.key === 'planner')
  assert.equal(planner.overlaps, true)
  assert.equal(planner.inputTokens, 100)
  const unlabelled = model.state.byRole.find(row => row.key === 'role-unknown')
  assert.equal(unlabelled.label, 'Role not reported')
  assert.equal(unlabelled.inputTokens, 10)
  const reviewer = model.state.byRole.find(row => row.key === 'reviewer')
  assert.equal(reviewer.tokensKnown, false)
})

test('Model and Provider rows keep routing facts and mark token usage as unattributed', async () => {
  const { model } = await started()
  assert.deepEqual(model.state.byModel.map(row => row.key), [
    'anthropic/claude',
    'mistral/mistral-large',
    'openai/gpt-5',
  ])
  for (const row of model.state.byModel) {
    assert.equal(row.attribution, 'unattributed')
    assert.equal(row.tokensKnown, false)
    assert.equal(row.asOfKnown, false)
  }
  const byProvider = new Map(model.state.byProvider.map(row => [row.providerId, row]))
  assert.equal(byProvider.get('openai').state, 'ready')
  assert.equal(byProvider.get('openai').isDefault, true)
  assert.equal(byProvider.get('anthropic').state, 'disabled')
  assert.equal(byProvider.get('anthropic').reason, 'provider_or_model_disabled')
  assert.equal(byProvider.get('mistral').state, 'unavailable')
  assert.equal(byProvider.get('mistral').reason, 'request_pool_unavailable')
  for (const row of byProvider.values()) assert.equal(row.usageAttribution, 'unattributed')
})

test('Worker offline, draining, capacity-short and healthy states stay distinct', async () => {
  const { model, rootElement } = await started()
  const states = model.state.workers.map(row => row.state)
  assert.deepEqual(states, [
    'online',
    'no-capacity',
    'draining',
    'offline',
    'heartbeat-stale',
    'heartbeat-unknown',
  ])
  assert.equal(new Set(states).size, states.length)
  const labels = rows(rootElement, 'wwc-usage-health-worker').map(node =>
    findByClass(node, 'wwc-usage-health-worker-state').textContent,
  )
  assert.equal(new Set(labels).size, states.length)
  assert.deepEqual(
    rows(rootElement, 'wwc-usage-health-worker').map(node => node.dataset.workerState),
    states,
  )
})

test('reported Worker capacity is compared with the configured concurrency limit', async () => {
  const { model, rootElement } = await started()
  assert.deepEqual(model.state.capacity, {
    reportedCapacity: 4,
    drainingCapacity: 3,
    limit: 4,
    sufficient: true,
  })
  assert.equal(
    findByClass(rootElement, 'wwc-usage-health-capacity').dataset.capacityState,
    'sufficient',
  )

  const short = await started({
    fixtures: {
      ...baselineFixtures(),
      'settings.get': { revision: 4, defaultModelRoute: null, workerConcurrencyLimit: 9 },
    },
  })
  assert.deepEqual(short.model.state.capacity, {
    reportedCapacity: 4,
    drainingCapacity: 3,
    limit: 9,
    sufficient: false,
  })
  assert.equal(
    findByClass(short.rootElement, 'wwc-usage-health-capacity').dataset.capacityState,
    'short',
  )
})

test('Provider unavailability is reported separately from Worker capacity and reachability', async () => {
  const { model, rootElement } = await started()
  const providerStates = new Set(model.state.byProvider.map(row => row.state))
  const workerStates = new Set(model.state.workers.map(row => row.state))
  for (const state of workerStates) assert.equal(providerStates.has(state), false)
  assert.notEqual(findByClass(rootElement, 'wwc-usage-health-providers'), null)
  assert.notEqual(findByClass(rootElement, 'wwc-usage-health-worker-list'), null)
  for (const row of rows(rootElement, 'wwc-usage-health-provider')) {
    assert.equal(typeof row.dataset.providerState, 'string')
  }
})

test('the observed data window, coverage and last update are presented', async () => {
  const { model, rootElement } = await started()
  assert.deepEqual(model.state.timeWindow, {
    from: ROTATED,
    to: '2026-09-03T08:30:00.000Z',
    observedSessions: 2,
    availableSessions: 2,
  })
  assert.equal(model.state.generatedAt, '2026-09-03T08:30:00.000Z')
  assert.equal(model.state.truncated, false)
  const window = findByClass(rootElement, 'wwc-usage-health-window')
  assert.match(window.textContent, /2026-09-01T00:00:00\.000Z/u)
  assert.match(window.textContent, /2026-09-03T08:30:00\.000Z/u)
  assert.match(
    findByClass(rootElement, 'wwc-usage-health-updated').textContent,
    /2026-09-03T08:30:00\.000Z/u,
  )

  const bounded = await started({ sessionLimit: 1 })
  assert.equal(bounded.model.state.truncated, true)
  assert.match(
    findByClass(bounded.rootElement, 'wwc-usage-health-window').textContent,
    /1 of 2/u,
  )
})

test('unknown and stale facts carry explicit non-color markers', async () => {
  const { model, rootElement } = await started()
  const stale = model.state.workers.find(row => row.state === 'heartbeat-stale')
  assert.equal(stale.heartbeatAgeMillis, 5_400_000)
  const unknown = model.state.workers.find(row => row.state === 'heartbeat-unknown')
  assert.equal(unknown.lastHeartbeatAt, null)
  assert.equal(unknown.heartbeatKnown, false)

  const marked = rows(rootElement, 'wwc-usage-health-unknown').map(node => node.dataset.unknown)
  assert.equal(marked.length > 0, true)
  for (const value of marked) assert.equal(value, 'true')

  const missingUsage = rows(rootElement, 'wwc-usage-health-stage-run')
    .find(node => node.dataset.key === stageRunThree)
  assert.equal(missingUsage.dataset.unknown, 'true')

  const noUsageNote = rows(rootElement, 'wwc-usage-health-note')
    .every(node => node.textContent.length > 0)
  assert.equal(noUsageNote, true)
})

test('credential lifecycle rows carry rotation facts and never credential identifiers', async () => {
  const { model, rootElement } = await started()
  assert.deepEqual(model.state.credentials.map(row => row.secretState), [
    'available',
    'revoked',
  ])
  assert.equal(model.state.credentials[0].rotationVersion, 3)
  const text = rootElement.textContent
  assert.match(text, /Primary provider key/u)
  assert.match(text, /revoked/u)
  assert.equal(text.includes('crd_'), false)
  assert.equal(text.includes(SECRET_MARKER), false)
})

test('recent execution errors and open Delivery attention are listed separately', async () => {
  const { model, rootElement } = await started()
  assert.deepEqual(model.state.errors.map(row => row.key), [
    `${deliveryOne}/${stageRunOne}`,
    deliveryTwo,
  ])
  const recovered = model.state.errors[0]
  assert.equal(recovered.failureCount, 2)
  assert.equal(recovered.recovered, true)
  assert.equal(recovered.sourceRef, 'runtime:failure-2')
  assert.equal(model.state.errors[1].attentionCount, 3)
  const listed = rows(rootElement, 'wwc-usage-health-error')
  assert.equal(listed.length, 2)
})

test('cost is never presented as an exact amount without a price source', async () => {
  const { rootElement } = await started()
  const text = rootElement.textContent
  assert.equal(/(?:\$|€|£)\s?\d/u.test(text), false)
  assert.match(text, /unit prices are not published/u)
})

test('the summary opens no second live region and reuses row identity across equivalent reads', async () => {
  const { model, rootElement } = await started()
  // The host page owns the one polite channel, so this panel must stay silent.
  const liveRegions = descendants(rootElement)
    .filter(node => node.getAttribute('aria-live') !== null)
  assert.deepEqual(liveRegions, [])
  assert.match(
    findByClass(rootElement, 'wwc-usage-health-updated').textContent,
    /Last updated/u,
  )

  const before = rows(rootElement, 'wwc-usage-health-delivery')
  await model.refresh()
  const after = rows(rootElement, 'wwc-usage-health-delivery')
  assert.deepEqual(after.map(node => node.dataset.key), before.map(node => node.dataset.key))
  assert.deepEqual(after, before)
})

test('refresh asks the shared query cache to discard this Scope reads before reloading', async () => {
  const { client, model } = await started()
  const before = client.queries.length
  await model.refresh()
  assert.ok(client.queries.length > before)
  const names = new Set(client.queries.map(request => request.query))
  for (const name of [
    'session.list',
    'runtime.projection.get',
    'delivery.list',
    'model.route.availability.list',
    'credential.reference.list',
    'worker.list',
    'settings.get',
  ]) assert.equal(names.has(name), true, name)
  const runtimeRead = client.queries.find(request => request.query === 'runtime.projection.get')
  assert.equal(runtimeRead.parameters.kind, 'product-session')
})

test('one unavailable projection marks only its own section', async () => {
  const { model, rootElement, client } = await started()
  const originalQuery = client.query
  client.query = async (request, requestOptions) => {
    if (request.query === 'worker.list') {
      throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'CONTROL_PLANE_UNAVAILABLE',
        message: 'The Control Plane could not answer the read.',
        requestId: request.requestId,
        retryable: true,
      })
    }
    return originalQuery.call(client, request, requestOptions)
  }
  await model.refresh()
  assert.equal(model.state.status, 'ready')
  assert.deepEqual(model.state.sources.worker, 'unavailable')
  assert.deepEqual(model.state.sources.delivery, 'ok')
  assert.deepEqual(
    model.state.unavailable.map(entry => entry.source),
    ['worker'],
  )
  assert.equal(model.state.workers.length, 0)
  const workerSection = findByClass(rootElement, 'wwc-usage-health-sections')
  const notes = rows(rootElement, 'wwc-usage-health-unavailable')
  assert.equal(notes.length, 7, 'one unavailable note per section')
  const visible = notes.filter(node => node.hidden === false)
  assert.equal(visible.length, 1)
  assert.match(visible[0].textContent, /CONTROL_PLANE_UNAVAILABLE/u)
  assert.match(visible[0].textContent, /This section is unavailable/u)
  assert.equal(rows(rootElement, 'wwc-usage-health-delivery').length > 0, true)
  assert.equal(findByClass(rootElement, 'wwc-usage-health-capacity').dataset.capacityState, 'unknown')
})

test('a runtime projection failure marks the usage sections without losing health facts', async () => {
  const { model, rootElement, client } = await started()
  const originalQuery = client.query
  client.query = async (request, requestOptions) => {
    if (request.query === 'runtime.projection.get') {
      throw new ControlPlaneClientError({
        kind: 'protocol',
        code: 'RUNTIME_PROJECTION_UNAVAILABLE',
        message: 'The runtime projection is not available.',
        requestId: request.requestId,
        retryable: true,
      })
    }
    return originalQuery.call(client, request, requestOptions)
  }
  await model.refresh()
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.sources.usage, 'unavailable')
  assert.equal(model.state.sources.provider, 'ok')
  assert.equal(model.state.sources.worker, 'ok')
  assert.equal(model.state.sources.credential, 'ok')
  assert.deepEqual(model.state.byDelivery, [])
  assert.equal(model.state.workers.length, 6)
  const visible = rows(rootElement, 'wwc-usage-health-unavailable')
    .filter(node => node.hidden === false)
  assert.equal(
    visible.length,
    4,
    'the three usage sections and the Recent errors section share the runtime failure',
  )
  for (const note of visible) assert.match(note.textContent, /RUNTIME_PROJECTION_UNAVAILABLE/u)
  assert.equal(rows(rootElement, 'wwc-usage-health-worker').length, 6)
})

test('every projection failing at once reports one closed error instead of an empty summary', async () => {
  const mounted = mount(baselineFixtures())
  const { model, rootElement, client } = mounted
  const originalQuery = client.query
  client.query = async () => {
    throw new ControlPlaneClientError({
      kind: 'protocol',
      code: 'CONTROL_PLANE_UNAVAILABLE',
      message: 'The Control Plane could not answer the read.',
      requestId: requestId('999'),
      retryable: true,
    })
  }
  await model.start()
  void originalQuery
  assert.equal(model.state.status, 'error')
  assert.equal(model.state.error.code, 'CONTROL_PLANE_UNAVAILABLE')
  assert.equal(findByClass(rootElement, 'wwc-usage-health-error-banner').hidden, false)
  assert.match(
    findByClass(rootElement, 'wwc-usage-health-error-banner').textContent,
    /CONTROL_PLANE_UNAVAILABLE/u,
  )
})

test('authentication and authorization failures fail closed without rendering facts', async () => {
  const denied = await started({ client: { throwOnQuery: 'worker.list', errorKind: 'authentication' } })
  assert.equal(denied.model.state.status, 'authentication-required')
  assert.equal(denied.model.state.workers.length, 0)
  assert.equal(findByClass(denied.rootElement, 'wwc-usage-health-worker-list').children.length, 0)
})

test('cancelPending stops an in-flight read and reports the cancelled state', async () => {
  const { client, model } = mount(baselineFixtures())
  const originalQuery = client.query
  let release
  const gate = new Promise(resolvePromise => { release = resolvePromise })
  client.query = async (request, requestOptions) => {
    if (request.query === 'worker.list') {
      await gate
      return originalQuery.call(client, request, requestOptions)
    }
    return originalQuery.call(client, request, requestOptions)
  }
  const pending = model.start()
  await new Promise(resolvePromise => { setTimeout(resolvePromise, 0) })
  model.cancelPending()
  release()
  await pending
  assert.equal(model.state.status, 'cancelled')
  assert.equal(model.state.error.code, 'REQUEST_CANCELLED')
})

test('close stops the view model and the mounted panel tears every listener down', async () => {
  const { model, rootElement, view } = await started()
  view.close()
  assert.equal(rootElement.children.length, 0)
  await model.refresh()
  assert.equal(rootElement.children.length, 0, 'a closed panel must not re-render')
  model.close()
  await assert.rejects(model.refresh())
  assert.equal(model.state.status, 'closed')
  assert.equal(rootElement.children.length, 0)
})

test('presentation derives distinct labels for every worker, provider and credential state', () => {
  const presentation = usageHealthPresentation()
  const workerStates = [
    'online',
    'no-capacity',
    'draining',
    'offline',
    'heartbeat-stale',
    'heartbeat-unknown',
  ]
  const workerLabels = workerStates.map(state => presentation.workerStateLabel[state])
  for (const label of workerLabels) assert.equal(typeof label, 'string')
  assert.equal(new Set(workerLabels).size, workerStates.length)
  const providerStates = ['ready', 'disabled', 'unavailable', 'unknown']
  const providerLabels = providerStates.map(state => presentation.providerStateLabel[state])
  for (const label of providerLabels) assert.equal(typeof label, 'string')
  assert.equal(new Set(providerLabels).size, providerStates.length)
  const credentialStates = ['available', 'missing', 'revoked']
  const credentialLabels = credentialStates.map(state => presentation.credentialStateLabel[state])
  assert.equal(new Set(credentialLabels).size, credentialStates.length)
})

test('the summary suite is registered once in the canonical lane and the decision inventory', () => {
  const runner = readFileSync(resolve(root, 'scripts/run-ts-tests.mjs'), 'utf8')
  for (const path of [
    'tests/usage-health-client.test.mjs',
    'tests/usage-health-browser.test.mjs',
  ]) {
    assert.equal(
      runner.split(`'${path}'`).length - 1,
      1,
      `${path} must be registered exactly once in the canonical TypeScript lane`,
    )
  }
  const inventory = JSON.parse(readFileSync(
    resolve(root, 'docs/decisions/0028-control-plane-worker-migration.inventory.json'),
    'utf8',
  ))
  const listed = new Set(inventory.surfaces.flatMap(surface => surface.sourcePaths))
  for (const path of [
    'apps/client/src/usage-health-view-model.ts',
    'apps/client/src/usage-health-page.ts',
  ]) assert.equal(listed.has(path), true, `${path} must be listed in the decision inventory`)
})
