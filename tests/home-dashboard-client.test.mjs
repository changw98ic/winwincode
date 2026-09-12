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
    'apps/client/tsconfig.home-dashboard-tests.json',
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
  `Home dashboard boundary did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cacheRoot = resolve(root, '.cache/home-dashboard-tests')
const load = name => import(`${pathToFileURL(resolve(cacheRoot, name)).href}`)

const [
  { ControlPlaneClientError },
  viewModelModule,
  pageModule,
  surfaceModule,
  contractsModule,
] = await Promise.all([
  load('community-control-plane-client.js'),
  load('home-dashboard-view-model.js'),
  load('home-dashboard-page.js'),
  load('client-surface.js'),
  load('generated/contracts.js'),
])
const { WorkItemState } = contractsModule

const {
  DEFAULT_HOME_DASHBOARD_LIMITS,
  createHomeDashboardViewModel,
  homeDashboardState,
  homeDeliveryCards,
  homeDeliverySection,
  orderedHomeBacklogCards,
  orderedHomeRunningCards,
  orderedHomeReadyCards,
  orderedHomeWaitingCards,
  orderedHomeValidatingCards,
  orderedHomeCompletedCards,
  orderedHomeFailedCards,
} = viewModelModule
const {
  homeChatHash,
  homeDashboardAnnouncement,
  homeDashboardPresentation,
  homeDecisionHash,
  mobileWebCapabilityText,
  mountHomeDashboardPage,
} = pageModule
const { CLIENT_SURFACES, clientSurfaceFromHash } = surfaceModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const otherScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000002',
}
const scopeSelection = {
  organizationId: scope.organizationId,
  workspaceId: scope.workspaceId,
  projectId: scope.projectId,
  repositoryId: scope.repositoryId,
}
const otherScopeSelection = () => ({
  organizationId: otherScope.organizationId,
  workspaceId: otherScope.workspaceId,
  projectId: otherScope.projectId,
  repositoryId: otherScope.repositoryId,
})
const workRunId = 'wrn_00000000000000000000000001'
const deliveryId = 'dlv_00000000000000000000000001'
const executingDeliveryId = 'dlv_00000000000000000000000002'
const deliveredDeliveryId = 'dlv_00000000000000000000000003'
const blockedDeliveryId = 'dlv_00000000000000000000000004'
const productSessionId = 'psn_00000000000000000000000001'
const approvalId = 'apr_00000000000000000000000001'
const inputRequestId = 'inp_00000000000000000000000001'
const attentionItemId = 'att_00000000000000000000000001'
const subscriptionId = 'sub_00000000000000000000000001'
const NOW = Date.parse('2026-09-03T09:00:00.000Z')

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function scopeIds(identity) {
  return {
    organizationId: identity.organizationId,
    workspaceId: identity.workspaceId,
    projectId: identity.projectId,
    repositoryId: identity.repositoryId,
  }
}

function deliverySummary(overrides = {}) {
  return {
    activeWorkRunId: null,
    deliveryId,
    openAttentionCount: 0,
    ownership: scopeIds(scope),
    revision: 3,
    schemaVersion,
    status: 'in_progress',
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, inProgress: 1, waitingDependency: 0, done: 0, failed: 0, backlog: 0, total: 1, validating: 0 },
    title: 'Delivery',
    updatedAt: '2026-09-03T08:00:00.000Z',
    ...overrides,
  }
}

function productSession(overrides = {}) {
  return {
    id: productSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision: 2,
    state: 'waiting_for_approval',
    title: 'First Chat',
    updatedAt: '2026-09-03T08:30:00.000Z',
    ...overrides,
  }
}

function approval(overrides = {}) {
  return {
    id: approvalId,
    revision: 5,
    state: 'pending',
    requestedAt: '2026-09-03T08:40:00.000Z',
    expiresAt: '2026-09-03T10:00:00.000Z',
    subject: 'Allow the projected repository action',
    binding: {
      productSessionId,
      executionJobId: canonicalId('job', 1),
      workerSessionId: canonicalId('wss', 1),
      sessionIdentity: {
        productSessionId,
        workerSessionId: canonicalId('wss', 1),
        codexThreadId: canonicalId('thr', 1),
        workRunId,
      },
    },
    ...overrides,
  }
}

function deliveryDetail(delivery) {
  return {
    deliveryId: delivery.deliveryId,
    deliveryRevision: delivery.revision,
    ownership: scopeIds(scope),
    attention: delivery.openAttentionCount > 0
      ? [{
          assignedTo: null,
          blocking: true,
          createdAt: '2026-09-03T08:45:00.000Z',
          deliverySpecId: 'spec-1',
          id: attentionItemId,
          options: [],
          resolutionSummary: null,
          resolvedAt: null,
          resolvedBy: null,
          workRunId: delivery.activeWorkRunId,
          status: 'open',
          title: 'Review the proposed delivery scope',
          type: 'scope_change',
        }]
      : [],
    currentCandidate: null,
    requirements: { repository: { kind: 'local-git', locator: '/tmp/repository' } },
    internalToolPayload: 'internal-tool-payload-marker',
  }
}

function respond(request, state) {
  const result = () => {
    if (request.query === 'delivery.list') {
      return { kind: 'delivery_page', items: state.deliveries }
    }
    if (request.query === 'delivery.get') {
      const delivery = state.deliveries.find(
        item => item.deliveryId === request.parameters.deliveryId,
      )
      const detail = state.deliveryDetails.get(request.parameters.deliveryId)
        ?? (delivery === undefined ? null : deliveryDetail(delivery))
      if (detail === null) throw new Error('unexpected delivery.get')
      return detail
    }
    if (request.query === 'session.list') {
      return { kind: 'product_session_page', items: state.sessions }
    }
    if (request.query === 'session.interactions.list') {
      return { kind: 'chat_interaction_page', items: state.interactions }
    }
    if (request.query === 'approval.list') {
      return { kind: 'approval_page', items: state.approvals }
    }
    if (request.query === 'worker.list') {
      return { kind: 'worker_page', items: state.workers }
    }
    if (request.query === 'credential.reference.list') {
      return { kind: 'credential_reference_page', items: [] }
    }
    if (request.query === 'settings.get') {
      return { revision: 1, defaultModelRoute: null, workerConcurrencyLimit: 2 }
    }
    if (request.query === 'model.route.availability.list') {
      return {
        kind: 'model_route_availability_page',
        scope: request.scope,
        requestPoolSource: request.scope,
        requestPoolRevision: 1,
        settingsRevision: 1,
        settingsSource: request.scope,
        defaultProviderId: null,
        defaultModelId: null,
        status: 'disabled',
        reason: 'no_provider',
        items: [],
      }
    }
    throw new Error(`unexpected query ${request.query}`)
  }
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result: result(),
    page: page(),
  }
}

function contractFake(state = {}) {
  const queries = []
  const subscriptions = []
  const subscriptionOptions = []
  const current = {
    deliveries: [],
    deliveryDetails: new Map(),
    sessions: [],
    interactions: [],
    approvals: [],
    workers: [],
    ...state,
  }
  return {
    queries,
    subscriptions,
    subscriptionOptions,
    get deliveries() { return current.deliveries },
    set deliveries(value) { current.deliveries = value },
    async query(request) {
      queries.push(structuredClone(request))
      return respond(request, current)
    },
    subscribe(options) {
      subscriptionOptions.push(options)
      const handle = { cursor: null, resume() {}, reconnect() {}, closed: false }
      handle.close = () => { handle.closed = true }
      subscriptions.push(handle)
      return handle
    },
    close() {},
    serverUrl: 'https://control.example/home',
  }
}


function homeModel(client, options = {}) {
  let nextRequest = 0
  return createHomeDashboardViewModel({
    client,
    actor,
    scope,
    subscriptionId,
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
    ...options,
  })
}

function deliveryState(deliveries, overrides = {}) {
  return {
    status: 'ready',
    filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
    visible: deliveries,
    loadedCount: deliveries.length,
    hasMore: false,
    loadingMore: false,
    moreFailure: null,
    error: null,
    advance: { deliveryId: null, failure: null },
    ...overrides,
  }
}

function attentionState(items, overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    items,
    origins: [],
    error: null,
    ...overrides,
  }
}

function usageState(overrides = {}) {
  return {
    status: 'ready',
    generatedAt: '2026-09-03T09:00:00.000Z',
    timeWindow: null,
    truncated: false,
    byDelivery: [],
    byWorkRun: [],
    byRole: [],
    byModel: [],
    byProvider: [],
    workers: [],
    capacity: null,
    credentials: [],
    errors: [],
    sources: {
      delivery: 'ok',
      usage: 'ok',
      provider: 'ok',
      credential: 'ok',
      worker: 'ok',
      settings: 'ok',
    },
    unavailable: [],
    error: null,
    ...overrides,
  }
}

function decisionCard(overrides = {}) {
  return {
    kind: 'input',
    id: inputRequestId,
    title: 'Describe the exact local change',
    urgency: 'pending',
    createdAt: null,
    expiresAt: '2026-09-03T10:00:00.000Z',
    actionDisabled: false,
    productSessionId,
    sessionTitle: 'First Chat',
    deliveryId: null,
    deliveryTitle: null,
    workRunId: null,
    ...overrides,
  }
}

test('Chat is the canonical default surface and every product entry stays reachable', () => {
  // 设计稿侧栏:新对话排首位并是默认落地页,任务看板紧随其后。
  assert.equal(CLIENT_SURFACES[0]?.id, 'chat')
  assert.deepEqual(CLIENT_SURFACES.map(surface => surface.id), [
    'chat',
    'home',
    'projects',
    'extensions',
    'device',
    'settings',
    'onboarding',
  ])
  assert.equal(clientSurfaceFromHash('').id, 'chat')
  assert.equal(clientSurfaceFromHash('#/home?x=1').id, 'home')
  assert.equal(clientSurfaceFromHash('#/chat').id, 'chat')
  for (const surface of CLIENT_SURFACES) {
    assert.equal(surface.default, surface.id === 'chat', surface.id)
  }
})




test('the dashboard groups Delivery projections into bounded, ordered sections', () => {
  const running = deliverySummary({
    deliveryId: executingDeliveryId,
    title: 'Running delivery',
    status: 'in_progress',
    activeWorkRunId: workRunId,
    updatedAt: '2026-09-03T08:50:00.000Z',
  })
  const verifying = deliverySummary({
    deliveryId: canonicalId('dlv', 5),
    title: 'Verifying delivery',
    status: 'validating',
    updatedAt: '2026-09-03T08:55:00.000Z',
  })
  const planReview = deliverySummary({
    deliveryId: canonicalId('dlv', 6),
    title: 'Plan review delivery',
    status: 'waiting_human',
    updatedAt: '2026-09-03T08:10:00.000Z',
  })
  const readyToStart = deliverySummary({
    deliveryId: canonicalId('dlv', 8),
    title: 'Ready delivery',
    status: 'ready',
    updatedAt: '2026-09-03T08:15:00.000Z',
  })
  const failed = deliverySummary({
    deliveryId: blockedDeliveryId,
    title: 'Blocked delivery',
    status: 'waiting_human',
    openAttentionCount: 2,
    updatedAt: '2026-09-03T08:20:00.000Z',
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, inProgress: 0, waitingDependency: 1, done: 1, failed: 3, backlog: 0, total: 5, validating: 0 },
  })
  const olderCompleted = deliverySummary({
    deliveryId: deliveredDeliveryId,
    title: 'Older delivered',
    status: 'done',
    updatedAt: '2026-09-03T06:00:00.000Z',
  })
  const newerCompleted = deliverySummary({
    deliveryId: canonicalId('dlv', 7),
    title: 'Newer delivered',
    status: 'done',
    updatedAt: '2026-09-03T07:30:00.000Z',
  })
  const cards = homeDashboardState({
    visits: [],
    deliveries: deliveryState([
      planReview,
      olderCompleted,
      failed,
      verifying,
      newerCompleted,
      readyToStart,
      running,
    ]),
    attention: attentionState([]),
    usage: usageState(),
  })

  assert.equal(cards.status, 'ready')
  assert.equal(cards.firstUse, false)
  assert.deepEqual(cards.sources, { delivery: 'ok', attention: 'ok', usage: 'ok' })
  assert.deepEqual(cards.running.map(card => card.deliveryId), [
    executingDeliveryId,
  ])
  assert.deepEqual(cards.validating.map(card => card.deliveryId), [canonicalId('dlv', 5)])
  assert.deepEqual(cards.ready.map(card => card.deliveryId), [canonicalId('dlv', 8)])
  assert.deepEqual(cards.waiting.map(card => card.deliveryId), [canonicalId('dlv', 6)])
  assert.deepEqual(cards.failed.map(card => card.deliveryId), [blockedDeliveryId])
  assert.deepEqual(cards.completed.map(card => card.deliveryId), [
    canonicalId('dlv', 7),
    deliveredDeliveryId,
  ])
  assert.deepEqual(cards.counts, {
    decisions: 0,
    backlog: 0,
    running: 1,
    ready: 1,
    waiting: 1,
    validating: 1,
    failed: 1,
    completed: 2,
    visited: 0,
  })
  assert.equal(cards.running[0]?.failedTasks, 0)
  assert.equal(cards.running[0]?.activeWorkRunId, workRunId)
  assert.equal(DEFAULT_HOME_DASHBOARD_LIMITS.deliveries, 4)

  // Section limits stay bounded even when the Scope holds many Deliveries.
  const bounded = homeDashboardState({
    visits: [],
    deliveries: deliveryState([
      planReview,
      olderCompleted,
      failed,
      verifying,
      newerCompleted,
      readyToStart,
      running,
    ]),
    attention: attentionState([]),
    usage: usageState(),
    limits: { decisions: 1, deliveries: 1 },
  })
  assert.equal(bounded.running.length, 1)
  assert.equal(bounded.validating.length, 1)
  assert.equal(bounded.ready.length, 1)
  assert.equal(bounded.completed.length, 1)
  assert.equal(bounded.failed.length, 1)
  assert.equal(bounded.counts.running, 1)
  assert.equal(bounded.counts.validating, 1)
  assert.equal(bounded.counts.completed, 2)
})

test('every canonical WorkItem state lands in exactly one board section', () => {
  const statuses = Object.values(WorkItemState)
  assert.equal(statuses.length, 11, 'the Controller publishes exactly these statuses')
  for (const status of statuses) {
    const card = homeDeliveryCards([deliverySummary({ status })])[0]
    const section = homeDeliverySection(card)
    assert.equal(
      ['backlog', 'ready', 'running', 'waiting', 'validating', 'failed', 'completed'].includes(section),
      true,
      `${status} must project into one canonical section (got ${section})`,
    )
  }
  // The WWC-ER-1001 mapping, section by section, from the served status alone.
  assert.equal(homeDeliverySection(homeDeliveryCards([deliverySummary({
    status: 'in_progress',
  })])[0]), 'running')
  assert.equal(homeDeliverySection(homeDeliveryCards([deliverySummary({
    status: 'rework',
  })])[0]), 'running', '设计稿 04:验证未通过正在修复的交付在 正在运行 列')
  assert.equal(homeDeliverySection(homeDeliveryCards([deliverySummary({
    status: 'ready',
  })])[0]), 'ready')
  assert.equal(homeDeliverySection(homeDeliveryCards([deliverySummary({
    status: 'candidate_ready',
  })])[0]), 'validating')
  assert.equal(homeDeliverySection(homeDeliveryCards([deliverySummary({
    status: 'validating',
  })])[0]), 'validating')
  assert.equal(homeDeliverySection(homeDeliveryCards([deliverySummary({
    status: 'backlog',
  })])[0]), 'backlog')
  assert.equal(homeDeliverySection(homeDeliveryCards([deliverySummary({
    status: 'waiting_human',
  })])[0]), 'waiting')
  assert.equal(homeDeliverySection(homeDeliveryCards([deliverySummary({
    status: 'done',
  })])[0]), 'completed')
  // Failure signals outrank motion: a partial failure surfaces as failed.
  assert.equal(homeDeliverySection(homeDeliveryCards([deliverySummary({
    status: 'in_progress',
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, inProgress: 1, waitingDependency: 0, done: 0, failed: 2, backlog: 0, total: 3, validating: 0 },
  })])[0]), 'failed')
})

test('failed Deliveries order by failures, then blocks, then recency; reworking runs', () => {
  const manyFailures = deliverySummary({
    deliveryId: canonicalId('dlv', 11),
    title: 'Three failures',
    status: 'in_progress',
    updatedAt: '2026-09-03T05:00:00.000Z',
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, inProgress: 1, waitingDependency: 0, done: 0, failed: 3, backlog: 0, total: 4, validating: 0 },
  })
  const blocked = deliverySummary({
    deliveryId: canonicalId('dlv', 12),
    title: 'Two blocked',
    status: 'validating',
    updatedAt: '2026-09-03T08:00:00.000Z',
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, inProgress: 0, waitingDependency: 2, done: 0, failed: 0, backlog: 0, total: 2, validating: 0 },
  })
  const reworking = deliverySummary({
    deliveryId: canonicalId('dlv', 13),
    title: 'Reworking',
    status: 'rework',
    updatedAt: '2026-09-03T08:59:00.000Z',
  })
  const cards = homeDeliveryCards([reworking, blocked, manyFailures])
  assert.deepEqual(orderedHomeFailedCards(cards).map(card => card.deliveryId), [
    canonicalId('dlv', 11),
    canonicalId('dlv', 12),
  ])
  assert.deepEqual(orderedHomeRunningCards(cards).map(card => card.deliveryId), [
    canonicalId('dlv', 13),
  ])
  const readyCards = homeDeliveryCards([
    deliverySummary({
      deliveryId: canonicalId('dlv', 16),
      status: 'candidate_ready',
      updatedAt: '2026-09-03T04:00:00.000Z',
    }),
    deliverySummary({
      deliveryId: canonicalId('dlv', 17),
      status: 'ready',
      updatedAt: '2026-09-03T07:30:00.000Z',
    }),
  ])
  const waitingCards = homeDeliveryCards([
    deliverySummary({
      deliveryId: canonicalId('dlv', 18),
      status: 'backlog',
      updatedAt: '2026-09-03T03:00:00.000Z',
    }),
    deliverySummary({
      deliveryId: canonicalId('dlv', 19),
      status: 'waiting_human',
      updatedAt: '2026-09-03T07:45:00.000Z',
    }),
  ])
  assert.deepEqual(orderedHomeReadyCards(readyCards).map(card => card.deliveryId), [
    canonicalId('dlv', 17),
  ])
  assert.deepEqual(orderedHomeValidatingCards(readyCards).map(card => card.deliveryId), [
    canonicalId('dlv', 16),
  ])
  assert.deepEqual(orderedHomeWaitingCards(waitingCards).map(card => card.deliveryId), [
    canonicalId('dlv', 19),
  ])
  assert.deepEqual(orderedHomeBacklogCards(waitingCards).map(card => card.deliveryId), [
    canonicalId('dlv', 18),
  ])
  const completedCards = homeDeliveryCards([
    deliverySummary({
      deliveryId: canonicalId('dlv', 14),
      status: 'done',
      updatedAt: '2026-09-03T04:00:00.000Z',
    }),
    deliverySummary({
      deliveryId: canonicalId('dlv', 15),
      status: 'done',
      updatedAt: '2026-09-03T07:00:00.000Z',
    }),
  ])
  assert.deepEqual(
    orderedHomeCompletedCards(completedCards).map(card => card.deliveryId),
    [canonicalId('dlv', 15), canonicalId('dlv', 14)],
  )
})

test('an empty first-use Scope reports an explicit empty dashboard', () => {
  const state = homeDashboardState({
    visits: [],
    deliveries: deliveryState([]),
    attention: attentionState([]),
    usage: usageState(),
  })
  assert.equal(state.firstUse, true)
  assert.equal(state.status, 'ready')
  assert.deepEqual(state.counts, {
    decisions: 0,
    backlog: 0,
    running: 0,
    ready: 0,
    waiting: 0,
    validating: 0,
    failed: 0,
    completed: 0,
    visited: 0,
  })

  // A failed Attention read hides the first-use claim: the dashboard cannot
  // prove nothing needs the user when one projection is unreadable.
  const uncertain = homeDashboardState({
    visits: [],
    deliveries: deliveryState([]),
    attention: attentionState([], { status: 'error' }),
    usage: usageState(),
  })
  assert.equal(uncertain.firstUse, false)
  assert.equal(uncertain.status, 'partial')
  assert.deepEqual(uncertain.sources, {
    delivery: 'ok',
    attention: 'unavailable',
    usage: 'ok',
  })

  const failed = homeDashboardState({
    visits: [],
    deliveries: deliveryState([], { status: 'error' }),
    attention: attentionState([], { status: 'error' }),
    usage: usageState({ status: 'error' }),
  })
  assert.equal(failed.status, 'error')
})


test('the composed view model reads every existing projection once and publishes one bounded dashboard', async () => {
  const client = contractFake({
    deliveries: [
      deliverySummary({
        deliveryId: executingDeliveryId,
        title: 'Running delivery',
        status: 'in_progress',
        activeWorkRunId: workRunId,
      }),
      deliverySummary({
        title: 'Delivery under attention',
        status: 'waiting_human',
        openAttentionCount: 1,
      }),
    ],
    sessions: [productSession()],
    approvals: [approval()],
    workers: [{
      id: canonicalId('wrk', 1),
      state: 'enabled',
      capacity: 2,
      lastHeartbeatAt: '2026-09-03T08:59:00.000Z',
      revision: 1,
    }],
  })
  const model = homeModel(client)
  let publications = 0
  const unsubscribe = model.subscribe(() => { publications += 1 })
  await model.start()

  assert.deepEqual([...new Set(client.queries.map(request => request.query))].sort(), [
    'approval.list',
    'credential.reference.list',
    'delivery.get',
    'delivery.list',
    'model.route.availability.list',
    'runtime.projection.get',
    'session.list',
    'settings.get',
    'worker.list',
  ])
  for (const request of client.queries) {
    assert.deepEqual(request.scope, scope)
    assert.deepEqual(request.actor, actor)
  }
  assert.equal(model.state.status, 'ready')
  assert.deepEqual(model.state.counts, {
    decisions: 2,
    backlog: 0,
    running: 1,
    ready: 0,
    waiting: 0,
    validating: 0,
    failed: 1,
    completed: 0,
    visited: 0,
  })
  assert.deepEqual(model.state.decisions.map(card => card.kind), ['attention', 'approval'])
  assert.equal(model.state.decisions[0]?.title, 'Review the proposed delivery scope')
  assert.equal(model.state.running[0]?.title, 'Running delivery')
  assert.equal(model.state.failed[0]?.openAttentionCount, 1)
  assert.equal(model.usage.state.workers.length, 1)
  assert.ok(publications > 1, 'the dashboard published while its projections loaded')

  await model.refresh()
  assert.equal(model.state.status, 'ready')
  unsubscribe()
  model.close()
  assert.equal(model.state.status, 'closed')
  assert.deepEqual(model.state.decisions, [])
  assert.equal(client.subscriptions.length, 1)
  assert.equal(client.subscriptions[0].closed, true, 'closing Home closes the scope subscription')
})

test('a decision card links to the exact decision surface and the exact Chat session', () => {
  const attentionCard = decisionCard({
    kind: 'attention',
    id: attentionItemId,
    title: 'Review the proposed delivery scope',
    urgency: 'blocking',
    createdAt: '2026-09-03T08:45:00.000Z',
    expiresAt: null,
    productSessionId: null,
    sessionTitle: null,
    deliveryId,
    deliveryTitle: 'Delivery under attention',
    workRunId,
  })
  const inputCard = decisionCard({ workRunId })
  assert.equal(
    homeDecisionHash(attentionCard, scopeSelection),
    `#/home/review?delivery=${deliveryId}&organizationId=${scope.organizationId}`
      + `&workspaceId=${scope.workspaceId}&projectId=${scope.projectId}`
      + `&repositoryId=${scope.repositoryId}`,
    'a Delivery-bound Attention opens its exact StrongFlow review',
  )
  assert.equal(
    homeDecisionHash(inputCard, scopeSelection),
    '#/chat?session=psn_00000000000000000000000001'
      + '&organizationId=org_00000000000000000000000001'
      + '&workspaceId=wsp_00000000000000000000000001'
      + '&projectId=prj_00000000000000000000000001'
      + '&repositoryId=rep_00000000000000000000000001',
  )
  assert.equal(
    homeChatHash(productSessionId, scopeSelection),
    '#/chat?session=psn_00000000000000000000000001'
      + '&organizationId=org_00000000000000000000000001'
      + '&workspaceId=wsp_00000000000000000000000001'
      + '&projectId=prj_00000000000000000000000001'
      + '&repositoryId=rep_00000000000000000000000001',
  )
})

test('mobile capability text keeps the station inbox available under every browser capability', () => {
  assert.match(mobileWebCapabilityText(null), /不支持 PWA.*系统通知不可用.*站内待处理/u)
  assert.match(mobileWebCapabilityText({
    navigator: { serviceWorker: {} },
    matchMedia: () => ({ matches: false }),
    Notification: { permission: 'default' },
  }), /支持 PWA 基础能力.*尚未授权.*站内待处理/u)
  assert.match(mobileWebCapabilityText({
    navigator: { standalone: true },
    matchMedia: () => ({ matches: false }),
    Notification: { permission: 'denied' },
  }), /独立 PWA.*已被阻止.*站内待处理/u)
})


test('the dashboard announcement names every section count and its gaps', () => {
  assert.ok(homeDashboardPresentation().sectionHeading.decisions.length > 0)
  assert.match(homeDashboardAnnouncement(homeDashboardState({
    visits: [],
    deliveries: deliveryState([]),
    attention: attentionState([]),
    usage: usageState(),
  })), /^就绪 · 0 项待决策/u)
  assert.match(homeDashboardAnnouncement(homeDashboardState({
    visits: [],
    deliveries: deliveryState([deliverySummary()]),
    attention: attentionState([decisionCard()]),
    usage: usageState(),
  })), /1 项待决策 · 0 个待拆分 · 1 个运行中 · 0 个待启动 · 0 个等待中 · 0 个验证中 · 0 个失败或阻塞 · 0 个已完成/u)
  assert.match(homeDashboardAnnouncement(homeDashboardState({
    visits: [],
    deliveries: deliveryState([]),
    attention: attentionState([], { status: 'error' }),
    usage: usageState({ status: 'error' }),
  })), /^就绪（部分缺省）/u)
  assert.match(homeDashboardAnnouncement(homeDashboardState({
    visits: [],
    deliveries: deliveryState([], { status: 'loading' }),
    attention: attentionState([], { status: 'loading' }),
    usage: usageState({ status: 'loading' }),
  })), /^正在读取看板/u)
  assert.match(homeDashboardAnnouncement(homeDashboardState({
    visits: [],
    deliveries: deliveryState([], { status: 'error' }),
    attention: attentionState([], { status: 'error' }),
    usage: usageState({ status: 'error' }),
  })), /^看板读取失败/u)
})

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  parentNode = null
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  type = ''
  value = ''
  id = ''
  tabIndex = 0
  title = ''
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

  get href() { return this.getAttribute('href') ?? '' }

  set href(value) { this.setAttribute('href', value) }

  append(...children) {
    for (const child of children) this.insertBefore(child, null)
  }

  replaceChildren(...children) {
    for (const child of [...this.children]) child.remove()
    for (const child of children) this.insertBefore(child, null)
  }

  insertBefore(child, reference) {
    child.remove?.()
    const index = reference === null ? this.children.length : this.children.indexOf(reference)
    this.children.splice(index < 0 ? this.children.length : index, 0, child)
    child.parentNode = this
    return child
  }

  remove() {
    if (this.parentNode === null) return
    const index = this.parentNode.children.indexOf(this)
    if (index >= 0) this.parentNode.children.splice(index, 1)
    this.parentNode = null
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value))
  }

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }

  removeEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    this.listeners.set(name, current.filter(candidate => candidate !== listener))
  }

  dispatch(name) {
    const event = { preventDefault() {} }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function allByClass(rootElement, className) {
  return descendants(rootElement).filter(node => node.className === className)
}

function byClass(rootElement, className) {
  const match = allByClass(rootElement, className)[0]
  assert.notEqual(match, undefined, `missing .${className}`)
  return match
}

function visibleText(node) {
  return descendants(node).map(current => current.textContent).join(' ')
}

function fakeUsageModel(state) {
  const listeners = new Set()
  return {
    state,
    subscribe(listener) {
      listeners.add(listener)
      listener(state)
      return () => { listeners.delete(listener) }
    },
    refresh() {},
    close() {},
  }
}

function fakeHomeModel(states) {
  let index = 0
  const listeners = new Set()
  const model = {
    usage: fakeUsageModel(usageState()),
    state: states[0],
    refreshes: 0,
    starts: 0,
    closed: false,
    start() {
      model.starts += 1
      return Promise.resolve()
    },
    subscribe(listener) {
      listeners.add(listener)
      listener(model.state)
      return () => { listeners.delete(listener) }
    },
    refresh() {
      model.refreshes += 1
      index = Math.min(index + 1, states.length - 1)
      model.state = states[index]
      for (const listener of listeners) listener(model.state)
      return Promise.resolve()
    },
    close() {
      model.closed = true
    },
  }
  return model
}

test('the Home page mounts the task board chrome, one polite live region, and exact deep links', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const state = homeDashboardState({
    visits: [],
    deliveries: deliveryState([
      deliverySummary({
        deliveryId: executingDeliveryId,
        title: 'Running delivery',
        status: 'in_progress',
        activeWorkRunId: workRunId,
      }),
      deliverySummary({
        deliveryId: deliveredDeliveryId,
        title: 'Delivered delivery',
        status: 'done',
        updatedAt: '2026-09-03T07:00:00.000Z',
      }),
    ]),
    attention: attentionState([
      decisionCard(),
      decisionCard({
        kind: 'attention',
        id: attentionItemId,
        title: 'Review the proposed delivery scope',
        urgency: 'blocking',
        createdAt: '2026-09-03T08:45:00.000Z',
        expiresAt: null,
        productSessionId: null,
        sessionTitle: null,
        deliveryId,
        deliveryTitle: 'Delivery under attention',
        workRunId,
      }),
      decisionCard({
        id: 'inp_00000000000000000000000009',
        title: 'Too late',
        urgency: 'expired',
        expiresAt: '2026-09-03T02:00:00.000Z',
        actionDisabled: true,
      }),
    ]),
    usage: usageState(),
  })
  const model = fakeHomeModel([state])
  const mountedPage = mountHomeDashboardPage({ root: rootElement, model, scopeSelection })

  const page = byClass(rootElement, 'wwc-home')
  assert.equal(page.dataset.wwcPage, 'home')

  // Design page 04: big title, display-only Scope select, and the new-task
  // entry that deep links the §16.6 form.
  assert.match(visibleText(byClass(rootElement, 'wwc-home-heading')), /任务看板/u)
  const projectSelect = byClass(rootElement, 'wwc-home-project-select')
  assert.equal(projectSelect.disabled, true)
  assert.match(visibleText(projectSelect), /全部项目/u)
  assert.match(visibleText(projectSelect), /当前仓库/u)
  const newTask = byClass(rootElement, 'wwc-home-new-task')
  assert.equal(newTask.textContent, '新建任务')
  assert.equal(
    newTask.href,
    `#/home/new-task?organizationId=${scope.organizationId}`
      + `&workspaceId=${scope.workspaceId}&projectId=${scope.projectId}`
      + `&repositoryId=${scope.repositoryId}`,
  )

  const liveRegions = descendants(page).filter(
    node => node.getAttribute('aria-live') === 'polite',
  )
  assert.equal(liveRegions.length, 1, 'the Home page keeps exactly one polite live region')
  assert.match(
    visibleText(liveRegions[0]),
    /就绪 · 3 项待决策 · 0 个待拆分 · 1 个运行中 · 0 个待启动 · 0 个等待中 · 0 个验证中 · 0 个失败或阻塞 · 1 个已完成/u,
  )

  // The two live columns: Running left, Needs-you right, bold + gray count.
  const runningSection = descendants(page).find(node => node.dataset?.section === 'running')
  assert.notEqual(runningSection, undefined)
  assert.equal(byClass(runningSection, 'wwc-home-section-heading').textContent, '运行中（Running）')
  assert.equal(byClass(runningSection, 'wwc-home-section-count').textContent, '1')
  const decisionsSection = descendants(page).find(node => node.dataset?.section === 'decisions')
  assert.notEqual(decisionsSection, undefined)
  assert.equal(byClass(decisionsSection, 'wwc-home-section-heading').textContent, '待我处理')
  assert.equal(byClass(decisionsSection, 'wwc-home-section-count').textContent, '3')

  const decisionCards = allByClass(rootElement, 'wwc-home-card')
    .filter(card => card.dataset.kind === 'decision')
  assert.equal(decisionCards.length, 3)
  const disabled = decisionCards.find(card => card.dataset.disabled === 'true')
  assert.notEqual(disabled, undefined)
  const disabledAction = byClass(disabled, 'wwc-home-card-action')
  assert.equal(disabledAction.getAttribute('href'), null)
  assert.equal(disabledAction.getAttribute('aria-disabled'), 'true')
  assert.equal(disabledAction.tabIndex, -1)
  assert.match(visibleText(disabled), /已过期 · 操作禁用/u)

  // Pending cards: status line by kind, one accent action per decision class.
  const inputCard = decisionCards.find(card => card.dataset.urgency === 'pending')
  assert.equal(byClass(inputCard, 'wwc-home-card-status').textContent, '方案待审核')
  assert.equal(byClass(inputCard, 'wwc-home-card-action').textContent, '审核方案')
  const attentionCard = decisionCards.find(card => card.dataset.urgency === 'blocking')
  assert.equal(byClass(attentionCard, 'wwc-home-card-status').textContent, '阻塞 · 需要立即决策')
  assert.equal(byClass(attentionCard, 'wwc-home-card-action').textContent, '验收交付')

  // Running cards: 强流程 + the mapped status text; the action opens the run
  // page (设计稿 04 的「查看进度」).
  const runningCard = allByClass(rootElement, 'wwc-home-card')
    .find(card => card.dataset.kind === 'delivery')
  assert.notEqual(runningCard, undefined)
  assert.equal(byClass(runningCard, 'wwc-home-card-status').textContent, '强流程 · 正在执行')
  assert.equal(byClass(runningCard, 'wwc-home-card-action').textContent, '查看进度')

  const actions = descendants(rootElement)
    .filter(node => node.className === 'wwc-home-card-action')
    .map(node => node.href)
  const scoped = `organizationId=${scope.organizationId}&workspaceId=${scope.workspaceId}`
    + `&projectId=${scope.projectId}&repositoryId=${scope.repositoryId}`
  assert.equal(
    actions.filter(href => href === `#/chat?session=${productSessionId}&${scoped}`).length,
    1,
    'the input decision opens its exact Chat session',
  )
  assert.equal(
    actions.filter(href => href.startsWith('#/')).length,
    4,
    'delivery cards open the run page; decisions open their exact surfaces',
  )
  const chatLinks = descendants(rootElement)
    .filter(node => node.className === 'wwc-home-card-chat' && node.hidden !== true)
    .map(node => node.href)
  assert.deepEqual(
    descendants(rootElement)
      .filter(node => node.className === 'wwc-home-card-chat' && node.hidden === true)
      .map(node => node.href),
    ['', '', '', ''],
    'cards without a Chat session expose no chat link at all',
  )
  assert.deepEqual(chatLinks, [homeChatHash(productSessionId, scopeSelection)])

  // Design page 04: the Usage panel and the first-use block are gone; the
  // canonical history groups render as collapsed hairline rows.
  assert.equal(allByClass(rootElement, 'wwc-usage-health').length, 0)
  assert.equal(allByClass(rootElement, 'wwc-home-first-use').length, 0)
  for (const id of ['backlog', 'ready', 'waiting', 'validating', 'failed', 'completed', 'visited']) {
    const section = descendants(page).find(node => node.dataset?.section === id)
    assert.notEqual(section, undefined)
    const toggle = byClass(section, 'wwc-home-section-toggle')
    assert.equal(toggle.getAttribute('aria-expanded'), 'false')
    const cards = byClass(section, 'wwc-home-cards')
    assert.equal(cards.hidden, true, `${id} stays collapsed`)
    if (id === 'completed') {
      assert.equal(byClass(section, 'wwc-home-section-heading').textContent, '已完成（Done）')
      toggle.dispatch('click')
      assert.equal(toggle.getAttribute('aria-expanded'), 'true')
      assert.equal(cards.hidden, false, 'the completed row expands in place')
      assert.equal(allByClass(section, 'wwc-home-card').length, 1)
      toggle.dispatch('click')
      assert.equal(toggle.getAttribute('aria-expanded'), 'false')
      assert.equal(cards.hidden, true)
    }
  }
  assert.match(visibleText(byClass(rootElement, 'wwc-home-sections')), /Running delivery/u)

  for (const card of allByClass(rootElement, 'wwc-home-card')) {
    assert.equal(card.getAttribute('draggable'), null)
    assert.equal(card.listeners.has('dragstart'), false)
    assert.equal(card.listeners.has('drop'), false)
  }

  mountedPage.close()
  assert.equal(rootElement.children.length, 0)
  assert.equal(model.closed, true)
})

test('the Home page stays usable when one projection is unavailable and closes its model once', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const ready = homeDashboardState({
    visits: [],
    deliveries: deliveryState([deliverySummary({ title: 'Running delivery' })]),
    attention: attentionState([]),
    usage: usageState(),
  })
  const partial = homeDashboardState({
    visits: [],
    deliveries: deliveryState([deliverySummary({ title: 'Running delivery' })]),
    attention: attentionState([], { status: 'error' }),
    usage: usageState({ status: 'error' }),
  })
  const model = fakeHomeModel([ready, partial])
  const mountedPage = mountHomeDashboardPage({
    root: rootElement,
    model,
    scopeSelection,
    ownsModel: false,
  })
  model.refresh()
  assert.match(visibleText(byClass(rootElement, 'wwc-home')), /待办 不可用/u)
  assert.match(visibleText(byClass(rootElement, 'wwc-home')), /用量与健康 不可用/u)
  assert.match(visibleText(byClass(rootElement, 'wwc-home')), /Running delivery/u)
  // 设计稿 04:看板画布没有刷新控件——模型刷新只能来自生命周期,不是按钮。
  assert.equal(model.refreshes, 1)

  mountedPage.close()
  assert.equal(rootElement.children.length, 0)
  assert.equal(model.closed, false, 'a host that owns the model closes it itself')
})

test('an empty Scope stays honest without the first-use block or usage panel', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const empty = homeDashboardState({
    visits: [],
    deliveries: deliveryState([]),
    attention: attentionState([]),
    usage: usageState(),
  })
  const model = fakeHomeModel([empty])
  const mountedPage = mountHomeDashboardPage({
    root: rootElement,
    model,
    scopeSelection,
    ownsModel: false,
  })
  // Design page 04 removes the first-use block and the Usage panel; the empty
  // columns keep their explicit notes and the new-task entry stays reachable.
  assert.equal(allByClass(rootElement, 'wwc-home-first-use').length, 0)
  assert.equal(allByClass(rootElement, 'wwc-usage-health').length, 0)
  const decisionsSection = descendants(rootElement)
    .find(node => node.dataset?.section === 'decisions')
  assert.equal(byClass(decisionsSection, 'wwc-home-section-empty').hidden, false)
  assert.match(
    byClass(decisionsSection, 'wwc-home-section-empty').textContent,
    /现在没有需要决策的事项/u,
  )
  const runningSection = descendants(rootElement)
    .find(node => node.dataset?.section === 'running')
  assert.equal(byClass(runningSection, 'wwc-home-section-empty').hidden, false)
  assert.equal(byClass(rootElement, 'wwc-home-new-task').textContent, '新建任务')
  mountedPage.close()
})

test('the composed view model stays partial when pending-decision data is unavailable', async () => {
  const client = contractFake()
  client.query = async request => {
    if (request.query === 'delivery.list') {
      return respond(request, { deliveries: [], sessions: [], approvals: [], workers: [] })
    }
    throw new ControlPlaneClientError({
      kind: 'network',
      code: 'NETWORK_UNREACHABLE',
      message: 'offline',
      requestId: null,
      retryable: true,
    })
  }
  const model = homeModel(client)
  await model.start()
  assert.equal(model.state.status, 'partial')
  assert.equal(model.state.sources.delivery, 'ok')
  assert.equal(model.state.sources.attention, 'unavailable')
  assert.equal(model.state.decisions.length, 0)
  // A broken projection hides the first-use claim: the dashboard cannot prove
  // the Scope is unused while Attention is unreadable.
  assert.equal(model.state.firstUse, false)
  model.close()
})
