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
    'apps/client/tsconfig.attention-notifications-tests.json',
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
  `Attention notification modules did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const signalsModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/attention-notifications-tests/attention-signals.js',
)).href}`)
const monitorModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/attention-notifications-tests/attention-notifications.js',
)).href}`)
const {
  attentionSignalBadge,
  attentionSignalRouteHash,
  attentionSignals,
  attentionSignalsTitle,
  createAttentionSignalGate,
} = signalsModule
const {
  createAttentionNotificationMonitor,
} = monitorModule
const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const scopeSelection = {
  organizationId: scope.organizationId,
  workspaceId: scope.workspaceId,
  projectId: scope.projectId,
  repositoryId: scope.repositoryId,
}
const deliveryId = 'dlv_00000000000000000000000001'
const attentionDeliveryId = 'dlv_00000000000000000000000002'
const deliveredDeliveryId = 'dlv_00000000000000000000000003'
const workRunId = 'wrn_00000000000000000000000001'
const approvalSessionId = 'psn_00000000000000000000000002'
const approvalId = 'apr_00000000000000000000000001'
const workerSessionId = 'wss_00000000000000000000000001'
const executionJobId = 'job_00000000000000000000000001'
const codexThreadId = 'thr_00000000000000000000000001'
const subscriptionId = 'sub_00000000000000000000000001'
const now = Date.parse('2026-09-04T03:00:00.000Z')
// Secret-safe probes: none of these may reach a notification or a badge.
const approvalSubject = 'Allow raw-tool-payload=credential-secret on private-host'
const repositoryLocator = 'ssh://user:token@private-host/secret/repository/path'
const candidateDigest = 'sha256:current-candidate-must-not-enter-notifications'

function page() {
  return { hasMore: false, nextCursor: null }
}

function requestId(value) {
  return `req_${String(value).padStart(26, '0')}`
}

function deliverySummary(overrides = {}) {
  return {
    activeWorkRunId: workRunId,
    deliveryId,
    openAttentionCount: 0,
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    revision: 13,
    schemaVersion,
    status: 'in_progress',
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, inProgress: 1, waitingDependency: 0, done: 2, failed: 0, backlog: 1, total: 4, validating: 0 },
    title: 'Delivery under execution',
    updatedAt: '2026-09-04T02:58:00.000Z',
    ...overrides,
  }
}

function approval(overrides = {}) {
  return {
    id: approvalId,
    revision: 7,
    state: 'pending',
    requestedAt: '2026-09-04T02:59:00.000Z',
    expiresAt: '2026-09-04T05:00:00.000Z',
    subject: approvalSubject,
    binding: {
      productSessionId: approvalSessionId,
      executionJobId,
      workerSessionId,
      sessionIdentity: {
        productSessionId: approvalSessionId,
        workerSessionId,
        codexThreadId,
        workRunId: workRunId,
      },
    },
    ...overrides,
  }
}

test('attention signals derive approval, attention, failure, and completion facts from projections', () => {
  const signals = attentionSignals({
    approvals: [approval()],
    deliveries: [
      deliverySummary(),
      deliverySummary({
        deliveryId: attentionDeliveryId,
        activeWorkRunId: null,
        openAttentionCount: 3,
        status: 'waiting_human',
        title: 'Delivery waiting on a decision',
      }),
      deliverySummary({
        deliveryId: deliveredDeliveryId,
        activeWorkRunId: null,
        status: 'done',
        workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0,
          inProgress: 0, waitingDependency: 0, done: 4, failed: 2, backlog: 0, total: 6, validating: 0,
        },
        title: 'Delivery that finished',
      }),
    ],
    nowMillis: now,
  })

  assert.deepEqual(signals.map(signal => signal.kind), [
    'attention',
    'approval',
    'failure',
    'completion',
  ])
  const attention = signals[0]
  assert.equal(attention.id, attentionDeliveryId)
  assert.equal(attention.identity, `attention:${attentionDeliveryId}:3`)
  assert.equal(attention.weight, 3)
  assert.equal(attention.workRunId, null)
  assert.equal(attention.context, '交付 · Delivery waiting on a decision')
  const failure = signals[2]
  assert.equal(failure.identity, `failure:${deliveredDeliveryId}:2`)
  assert.equal(failure.weight, 2)
  assert.equal(failure.title, '多项任务失败')
  assert.equal(failure.workRunId, null)
  const completion = signals[3]
  assert.equal(completion.id, deliveredDeliveryId)
  assert.equal(completion.identity, `completion:${deliveredDeliveryId}:done`)
  assert.equal(completion.workRunId, null)
  assert.equal(completion.weight, 1)
  const approvalSignal = signals[1]
  assert.equal(approvalSignal.id, approvalId)
  assert.equal(approvalSignal.identity, `approval:${approvalId}:pending`)
  assert.equal(approvalSignal.productSessionId, approvalSessionId)
  assert.equal(approvalSignal.workRunId, workRunId)
  assert.equal(approvalSignal.context, '交付 · Delivery under execution')

  const badge = attentionSignalBadge(signals)
  assert.deepEqual(badge, {
    total: 7,
    attention: 3,
    approval: 1,
    completion: 1,
    failure: 2,
  })
  assert.equal(attentionSignalsTitle('WinWinCode', badge), '(7) WinWinCode')
  assert.equal(attentionSignalsTitle('WinWinCode', attentionSignalBadge([])), 'WinWinCode')
})

test('attention signals exclude expired approvals and silent deliveries', () => {
  const signals = attentionSignals({
    approvals: [
      approval(),
      approval({
        id: 'apr_00000000000000000000000002',
        state: 'expired',
        expiresAt: '2026-09-04T02:00:00.000Z',
      }),
      approval({
        id: 'apr_00000000000000000000000003',
        expiresAt: '2026-09-04T02:59:59.000Z',
      }),
    ],
    deliveries: [
      deliverySummary({ openAttentionCount: 2, status: 'in_progress' }),
      deliverySummary({ deliveryId: attentionDeliveryId, workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0,
        inProgress: 0, waitingDependency: 0, done: 0, failed: 0, backlog: 3, total: 3, validating: 0,
      } }),
    ],
    nowMillis: now,
  })

  assert.deepEqual(signals.map(signal => signal.kind), ['approval'])
  assert.deepEqual(attentionSignalBadge(signals), {
    total: 1,
    attention: 0,
    approval: 1,
    completion: 0,
    failure: 0,
  })
})

test('notification content stays secret-safe and drops non-canonical identities', () => {
  const signals = attentionSignals({
    approvals: [approval()],
    deliveries: [
      deliverySummary({
        deliveryId: attentionDeliveryId,
        activeWorkRunId: null,
        openAttentionCount: 1,
        status: 'waiting_human',
        title: 'Delivery waiting on a decision',
      }),
      deliverySummary({
        deliveryId: 'dlv_not-canonical',
        activeWorkRunId: 'wrn_00000000000000000000000009',
        openAttentionCount: 4,
        status: 'waiting_human',
      }),
    ],
    nowMillis: now,
  })
  const text = signals.map(signal => `${signal.title} ${signal.context}`).join(' ')
  for (const secret of [approvalSubject, repositoryLocator, candidateDigest]) {
    assert.equal(text.includes(secret), false, secret)
  }
  assert.equal(text.includes('dlv_not-canonical'), false)
  assert.deepEqual(
    signals.map(signal => signal.kind),
    ['attention', 'approval'],
    'a malformed Delivery identity never becomes a notification',
  )
  const attention = signals.find(signal => signal.kind === 'attention')
  assert.equal(attention.context, '交付 · Delivery waiting on a decision')
  const approvalSignal = signals.find(signal => signal.kind === 'approval')
  assert.equal(approvalSignal.title, '工具调用等待批准')
  assert.equal(approvalSignal.context, '打开会话决策')
})

test('one event identity notifies once and a changed state notifies again', () => {
  const gate = createAttentionSignalGate()
  const first = attentionSignals({
    approvals: [approval()],
    deliveries: [deliverySummary({ openAttentionCount: 1, status: 'waiting_human' })],
    nowMillis: now,
  })
  assert.deepEqual(gate.admit(first), first)
  assert.deepEqual(gate.admit(first), [], 'the same snapshot must not notify again')
  assert.deepEqual(gate.admit([...first].reverse()), [], 'order changes must not notify again')

  const progressed = attentionSignals({
    approvals: [approval()],
    deliveries: [deliverySummary({ openAttentionCount: 2, status: 'waiting_human' })],
    nowMillis: now,
  })
  assert.deepEqual(
    gate.admit(progressed).map(signal => signal.identity),
    [`attention:${deliveryId}:2`],
  )
  assert.deepEqual(
    gate.admit(attentionSignals({ approvals: [], deliveries: [deliverySummary()], nowMillis: now })),
    [],
  )
  gate.forget(`attention:${deliveryId}:2`)
  assert.deepEqual(
    gate.admit(progressed).map(signal => signal.identity),
    [`attention:${deliveryId}:2`],
  )
})

test('signals open the filtered task board', () => {
  const signals = attentionSignals({
    approvals: [approval()],
    deliveries: [
      deliverySummary({ openAttentionCount: 1, status: 'waiting_human' }),
      deliverySummary({
        deliveryId: deliveredDeliveryId,
        activeWorkRunId: null,
        status: 'done',
      }),
    ],
    nowMillis: now,
  })
  const expectedHash = `#/home?filter=attention`
    + `&organizationId=${scope.organizationId}&workspaceId=${scope.workspaceId}`
    + `&projectId=${scope.projectId}&repositoryId=${scope.repositoryId}`
  assert.equal(attentionSignalRouteHash(scopeSelection), expectedHash)
})

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
    this.attributes = new Map()
    this.children = []
    this.parentNode = null
    this.listeners = new Map()
    this.dataset = {}
    this.className = ''
    this.disabled = false
    this.hidden = false
    this.type = ''
    this.value = ''
    this.id = ''
    this.tabIndex = 0
    this.title = ''
    this.#textContent = ''
  }

  #textContent

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
  constructor() {
    this.title = 'WinWinCode'
  }

  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

function monitoringClient() {
  const queries = []
  let currentApprovals = []
  let currentDeliveries = []
  const subscriptions = []
  return {
    queries,
    subscriptions,
    get approvals() { return currentApprovals },
    set approvals(value) { currentApprovals = value },
    get deliveries() { return currentDeliveries },
    set deliveries(value) { currentDeliveries = value },
    failNext: null,
    async query(request) {
      queries.push(structuredClone(request))
      if (this.failNext !== null) throw this.failNext
      if (request.query === 'approval.list') {
        return {
          schemaVersion,
          requestId: request.requestId,
          query: request.query,
          result: { kind: 'approval_page', items: currentApprovals },
          page: page(),
        }
      }
      if (request.query === 'delivery.list') {
        return {
          schemaVersion,
          requestId: request.requestId,
          query: request.query,
          result: { kind: 'delivery_page', items: currentDeliveries },
          page: page(),
        }
      }
      throw new Error(`unexpected query ${request.query}`)
    },
    subscribe() {
      subscriptions.push('subscribed')
      return { cursor: null, resume() {}, reconnect() {}, close() {} }
    },
    close() {},
    serverUrl: 'https://control.example/local',
  }
}

function manualTicker() {
  const pending = []
  return {
    pending,
    schedule(handler) {
      pending.push(handler)
      return () => {
        const index = pending.indexOf(handler)
        if (index >= 0) pending.splice(index, 1)
      }
    },
    tick() {
      for (const handler of [...pending]) handler()
    },
  }
}

function desktopFake({ permission = 'default', supported = true } = {}) {
  return {
    supported,
    shown: [],
    closed: [],
    clickHandlers: [],
    permissionValue: permission,
    requested: 0,
    permission() { return this.permissionValue },
    async requestPermission() {
      this.requested += 1
      this.permissionValue = this.nextPermission ?? 'granted'
      return this.permissionValue
    },
    show(notification, onClick) {
      this.shown.push(notification)
      this.clickHandlers.push(onClick)
    },
    close(tag) { this.closed.push(tag) },
  }
}

function monitorFor(client, overrides = {}) {
  const ticker = overrides.ticker ?? manualTicker()
  const document = overrides.document ?? new FakeDocument()
  const badgeTarget = overrides.badgeTarget ?? new FakeElement(document, 'a')
  const desktop = overrides.desktop ?? null
  const opened = []
  const monitor = createAttentionNotificationMonitor({
    client,
    actor,
    scope,
    nextRequestId: (() => {
      let next = 0
      return () => requestId(++next)
    })(),
    nowMillis: () => now,
    document,
    badgeTarget,
    ...(desktop === null ? {} : { notifications: desktop }),
    onOpenTarget(hash) { opened.push(hash) },
    scheduleTick: (handler, millis) => ticker.schedule(handler, millis),
    ...overrides.options,
  })
  return { monitor, ticker, document, badgeTarget, desktop, opened }
}

test('the monitor badges approval, attention, failure, and completion counts without a second subscription', async () => {
  const client = monitoringClient()
  client.approvals = [approval()]
  client.deliveries = [
    deliverySummary({ openAttentionCount: 2, status: 'waiting_human' }),
  ]
  const { monitor, ticker, badgeTarget, document } = monitorFor(client)
  badgeTarget.textContent = '任务看板'
  await monitor.start()

  assert.deepEqual(client.queries.map(request => request.query), [
    'approval.list',
    'delivery.list',
  ])
  assert.deepEqual(client.queries[0].parameters, { states: ['pending'] })
  assert.deepEqual(client.subscriptions, [], 'the monitor never opens a second event queue')
  assert.equal(monitor.state.status, 'ready')
  assert.deepEqual(monitor.state.badge, {
    total: 3,
    attention: 2,
    approval: 1,
    completion: 0,
    failure: 0,
  })
  assert.equal(badgeTarget.dataset.wwcBadge, '3')
  assert.equal(badgeTarget.getAttribute('aria-label'), '任务看板 · 3 项待处理')
  assert.equal(badgeTarget.children.length, 1)
  assert.equal(badgeTarget.children[0].textContent, '3')
  assert.equal(document.title, '(3) WinWinCode')
  assert.match(badgeTarget.textContent, /任务看板/u, 'the entry keeps its own label')

  // The shell rebuilds navigation labels; the badge must survive the rebuild.
  badgeTarget.textContent = '任务看板'
  monitor.applyBadge()
  assert.equal(badgeTarget.children.length, 1)
  assert.match(badgeTarget.textContent, /任务看板/u)

  client.deliveries = [deliverySummary()]
  client.approvals = []
  await monitor.refresh()
  assert.deepEqual(monitor.state.badge, {
    total: 0, attention: 0, approval: 0, completion: 0, failure: 0,
  })
  assert.equal(badgeTarget.children.length, 0)
  assert.equal(badgeTarget.getAttribute('aria-label'), null)
  assert.equal(badgeTarget.dataset.wwcBadge, undefined)
  assert.equal(document.title, 'WinWinCode')
  assert.match(badgeTarget.textContent, /任务看板/u, 'clearing the badge keeps the label')
  ticker.tick()
  await new Promise(resolve => { setImmediate(resolve) })
  assert.deepEqual(client.queries.map(request => request.query), [
    'approval.list',
    'delivery.list',
    'approval.list',
    'delivery.list',
    'approval.list',
    'delivery.list',
  ])
  monitor.close()
})

test('resolved and expired entries leave the badge on the next bounded tick', async () => {
  const client = monitoringClient()
  let clock = now
  client.approvals = [approval()]
  client.deliveries = [deliverySummary()]
  const { monitor, ticker, document } = monitorFor(client, {
    options: { nowMillis: () => clock },
  })
  await monitor.start()
  assert.equal(monitor.state.badge.total, 1)

  clock = Date.parse('2026-09-04T05:00:01.000Z')
  client.approvals = []
  ticker.tick()
  await new Promise(resolve => { setImmediate(resolve) })
  assert.equal(monitor.state.badge.total, 0)
  assert.equal(document.title, 'WinWinCode')
  monitor.close()
})

test('monitor failures clear the badge instead of presenting stale counts', async () => {
  const client = monitoringClient()
  client.deliveries = [deliverySummary({ openAttentionCount: 1, status: 'waiting_human' })]
  const { monitor, badgeTarget, document } = monitorFor(client)
  await monitor.start()
  assert.equal(monitor.state.badge.total, 1)

  client.failNext = new Error('control plane unavailable')
  await monitor.refresh()
  assert.equal(monitor.state.status, 'error')
  assert.deepEqual(monitor.state.badge, {
    total: 0, attention: 0, approval: 0, completion: 0, failure: 0,
  })
  assert.equal(badgeTarget.children.length, 0)
  assert.equal(document.title, 'WinWinCode')
  monitor.close()
})

test('desktop notifications stay off until the user grants them and never repeat one event', async () => {
  const client = monitoringClient()
  client.deliveries = [
    deliverySummary({ openAttentionCount: 1, status: 'waiting_human' }),
    deliverySummary({ deliveryId: attentionDeliveryId, status: 'done' }),
  ]
  const desktop = desktopFake({ permission: 'default' })
  const { monitor, opened } = monitorFor(client, { desktop })
  await monitor.start()
  assert.equal(monitor.state.desktop.enabled, false)
  assert.deepEqual(desktop.shown, [], 'no notification without explicit consent')

  await monitor.setDesktopEnabled(true)
  assert.equal(desktop.requested, 1)
  assert.equal(monitor.state.desktop.enabled, true)
  assert.equal(monitor.state.desktop.permission, 'granted')
  assert.deepEqual(desktop.shown, [], 'consent must not replay entries that already existed')

  client.deliveries = [
    deliverySummary({ openAttentionCount: 2, status: 'waiting_human' }),
    deliverySummary({ deliveryId: attentionDeliveryId, status: 'done' }),
  ]
  await monitor.refresh()
  assert.deepEqual(desktop.shown.map(notification => notification.tag), [
    `attention:${deliveryId}:2`,
  ])
  assert.equal(desktop.shown[0].title, '交付需要处理')
  assert.equal(desktop.shown[0].body, '交付 · Delivery under execution')
  await monitor.refresh()
  assert.equal(desktop.shown.length, 1, 'one event identity never notifies twice')

  desktop.clickHandlers[0]()
  assert.deepEqual(opened, [
    `#/home?filter=attention`
      + `&organizationId=${scope.organizationId}&workspaceId=${scope.workspaceId}`
      + `&projectId=${scope.projectId}&repositoryId=${scope.repositoryId}`,
  ])
  monitor.close()
})

test('denied desktop permission stays explicit and resolved entries close their notification', async () => {
  const client = monitoringClient()
  client.deliveries = [deliverySummary({ openAttentionCount: 1, status: 'waiting_human' })]
  const desktop = desktopFake({ permission: 'default' })
  desktop.nextPermission = 'denied'
  const { monitor } = monitorFor(client, { desktop })
  await monitor.start()
  await monitor.setDesktopEnabled(true)
  assert.equal(desktop.requested, 1)
  assert.equal(monitor.state.desktop.enabled, false)
  assert.equal(monitor.state.desktop.blocked, true)
  assert.deepEqual(desktop.shown, [])

  desktop.permissionValue = 'granted'
  await monitor.setDesktopEnabled(true)
  assert.equal(monitor.state.desktop.enabled, true)
  assert.equal(desktop.shown.length, 0, 'enabling later never replays known entries')

  client.deliveries = [
    deliverySummary({ openAttentionCount: 1, status: 'waiting_human' }),
    deliverySummary({ deliveryId: attentionDeliveryId, openAttentionCount: 1, status: 'waiting_human' }),
  ]
  await monitor.refresh()
  assert.equal(desktop.shown.length, 1)
  client.deliveries = [deliverySummary()]
  await monitor.refresh()
  assert.deepEqual(desktop.closed, [desktop.shown[0].tag], 'the notification clears with its entry')
  monitor.close()
})

test('closing the monitor stops the bounded tick and clears the badge', async () => {
  const client = monitoringClient()
  client.deliveries = [deliverySummary({ openAttentionCount: 1, status: 'waiting_human' })]
  const { monitor, ticker, badgeTarget, document } = monitorFor(client)
  await monitor.start()
  monitor.close()
  assert.equal(monitor.state.status, 'closed')
  assert.equal(ticker.pending.length, 0)
  assert.equal(badgeTarget.children.length, 0)
  assert.equal(document.title, 'WinWinCode')
  const queriesBefore = client.queries.length
  await monitor.refresh()
  assert.equal(client.queries.length, queriesBefore)
})
