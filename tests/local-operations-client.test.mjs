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
    'apps/client/tsconfig.local-operations-tests.json',
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
  `Local operations client did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const viewModelModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/local-operations-tests/local-operations-view-model.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/local-operations-tests/local-operations-page.js',
)).href}`)

const { createLocalOperationsViewModel } = viewModelModule
const { localOperationsPagePresentation, mountLocalOperationsPage } = pageModule
const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const workerId = 'wrk_00000000000000000000000001'
const deliveryId = 'dlv_00000000000000000000000001'
const subscriptionId = 'sub_00000000000000000000000001'

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function worker(overrides = {}) {
  return {
    id: workerId,
    revision: 1,
    state: 'enabled',
    capacity: 0,
    lastHeartbeatAt: '2026-08-27T02:00:00.000Z',
    ...overrides,
  }
}

function deliverySummary() {
  return {
    schemaVersion,
    deliveryId,
    revision: 3,
    status: 'verifying',
    title: 'Local repository diagnostics',
    updatedAt: '2026-08-27T02:00:02.000Z',
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    activeStageRunId: null,
    openAttentionCount: 1,
    taskCounts: {
      total: 0,
      pending: 0,
      active: 0,
      blocked: 0,
      verifying: 0,
      completed: 0,
      failed: 0,
    },
  }
}

function detail(overrides = {}) {
  return {
    requirements: {
      repository: {
        kind: 'local-git',
        locator: 'ssh://user:token@private-host/secret/repository/path',
      },
      baseRevision: '0123456789abcdef0123456789abcdef01234567',
    },
    currentCandidate: {
      candidateRef: 'git-candidate:sha256:hidden',
    },
    attention: [{ status: 'open', blocking: true }],
    verdict: null,
    ...overrides,
  }
}

function queryResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: page(),
  }
}

function commandResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    command: request.command,
    outcome: 'completed',
    previousRevision: request.expectedRevision,
    currentRevision: result.revision,
    result,
  }
}

function contractFake() {
  const queries = []
  const commands = []
  const subscriptions = []
  let currentWorker = worker()
  let currentDetail = detail()
  return {
    queries,
    commands,
    subscriptions,
    get worker() { return currentWorker },
    set worker(value) { currentWorker = value },
    get detail() { return currentDetail },
    set detail(value) { currentDetail = value },
    async query(request) {
      queries.push(structuredClone(request))
      if (request.query === 'worker.list') {
        return queryResponse(request, { kind: 'worker_page', items: [currentWorker] })
      }
      if (request.query === 'delivery.list') {
        return queryResponse(request, { kind: 'delivery_page', items: [deliverySummary()] })
      }
      if (request.query === 'delivery.get') return queryResponse(request, currentDetail)
      throw new Error(`unexpected query ${request.query}`)
    },
    async command(request) {
      commands.push(structuredClone(request))
      if (request.command === 'worker.drain') {
        currentWorker = worker({
          ...currentWorker,
          revision: currentWorker.revision + 1,
          state: 'draining',
        })
        return commandResponse(request, currentWorker)
      }
      if (request.command === 'worker.enable') {
        currentWorker = worker({
          ...currentWorker,
          revision: currentWorker.revision + 1,
          state: 'enabled',
        })
        return commandResponse(request, currentWorker)
      }
      throw new Error(`unexpected command ${request.command}`)
    },
    subscribe(options) {
      subscriptions.push(options)
      return {
        cursor: null,
        resume() {},
        reconnect() {},
        close() {},
      }
    },
    close() {},
    serverUrl: 'https://control.example/local',
  }
}

test('repository and Worker view-model redacts paths and separates resource shortage from code failure', async () => {
  const client = contractFake()
  let nextRequest = 0
  const model = createLocalOperationsViewModel({
    client,
    actor,
    scope,
    subscriptionId,
    nextRequestId() {
      nextRequest += 1
      return requestId(nextRequest)
    },
  })
  await model.start()
  assert.deepEqual(client.queries.map(request => request.query), [
    'worker.list',
    'delivery.list',
    'delivery.get',
  ])
  assert.deepEqual(client.queries[0].parameters.states, ['enabled', 'draining', 'offline'])
  assert.equal(model.state.repository.repositoryIdentity, '…000001')
  assert.equal(model.state.repository.baselineRevision, '0123456789ab')
  assert.equal(model.state.repository.worktreeState, 'candidate-frozen')
  assert.equal(model.state.repository.gitRisk, 'attention-required')
  assert.equal(model.state.repository.pathsHidden, true)
  assert.equal(JSON.stringify(model.state).includes('private-host'), false)
  assert.equal(JSON.stringify(model.state).includes('/secret/repository/path'), false)
  assert.equal(model.state.resources.reportedCapacitySlots, 0)
  assert.equal(model.state.resources.failureClassification, 'resource-shortage')
  assert.equal(model.state.resources.cpu, 'not-reported')
  assert.equal(model.state.resources.memory, 'not-reported')
  assert.equal(model.state.resources.disk, 'not-reported')
  assert.equal(model.state.resources.cleanup, 'not-reported')
  assert.deepEqual(client.subscriptions[0].subscription, {
    scope,
    stream: { kind: 'scope' },
    eventTypes: ['activity.recorded.v1'],
  })

  client.worker = worker({ capacity: 2 })
  client.detail = detail({
    attention: [],
    verdict: { status: 'fail' },
  })
  await client.subscriptions[0].onEvent({})
  assert.equal(model.state.resources.reportedCapacitySlots, 2)
  assert.equal(model.state.resources.failureClassification, 'code-failure')
  assert.equal(model.state.repository.gitRisk, 'code-failure')

  await model.drainWorker(workerId)
  assert.equal(client.commands.length, 1)
  assert.equal(client.commands[0].command, 'worker.drain')
  assert.equal(client.commands[0].expectedRevision, 1)
  assert.equal(client.commands[0].payload.reason, 'Requested from the local operations page.')
  assert.equal(model.state.workers[0].state, 'draining')
  assert.equal(model.state.resources.failureClassification, 'code-failure')
  await model.drainWorker(workerId)
  assert.equal(client.commands.length, 1, 'draining an already draining Worker is idempotent')

  await model.enableWorker(workerId)
  assert.equal(client.commands.length, 2)
  assert.equal(client.commands[1].command, 'worker.enable')
  assert.equal(client.commands[1].expectedRevision, 2)
  assert.equal(model.state.workers[0].state, 'enabled')
  await model.enableWorker(workerId)
  assert.equal(client.commands.length, 2, 'enabling an already enabled Worker is idempotent')
  model.close()
})

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
  type = ''
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }

  append(...children) {
    this.children.push(...children)
  }

  replaceChildren(...children) {
    this.children = [...children]
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value))
  }

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
  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
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

function pageState(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    workers: [worker({ capacity: 2 })],
    repository: {
      available: true,
      repositoryIdentity: '…000001',
      repositoryKind: 'local-git',
      baselineRevision: '0123456789ab',
      worktreeState: 'candidate-frozen',
      gitRisk: 'clear',
      openAttentionCount: 0,
      pathsHidden: true,
    },
    resources: {
      reportedWorkerCount: 1,
      enabledWorkerCount: 1,
      reportedCapacitySlots: 2,
      cpu: 'not-reported',
      memory: 'not-reported',
      disk: 'not-reported',
      cleanup: 'not-reported',
      failureClassification: 'none',
    },
    interaction: { status: 'idle', operation: null, workerId: null, error: null },
    error: null,
    ...overrides,
  }
}

test('local operations page shows safe diagnostics and delegates Worker commands', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const calls = []
  let state = pageState()
  let listener = () => {}
  const model = {
    get state() { return state },
    subscribe(next) {
      listener = next
      next(state)
      return () => { listener = () => {} }
    },
    async start() {},
    async refresh() { calls.push({ operation: 'refresh' }) },
    async drainWorker(id) { calls.push({ operation: 'drain', id }) },
    async enableWorker(id) { calls.push({ operation: 'enable', id }) },
    cancelPending() {},
    reconnect() { calls.push({ operation: 'reconnect' }) },
    close() {},
  }
  const mounted = mountLocalOperationsPage({ root: rootElement, model })
  assert.equal(byClass(rootElement, 'wwc-local-operations').dataset.wwcPage, 'management')
  assert.equal(byClass(rootElement, 'wwc-local-operations-heading').dataset.wwcComponent, 'page-header')
  assert.equal(byClass(rootElement, 'wwc-local-operations-status').dataset.wwcComponent, 'status-badge')
  assert.equal(byClass(rootElement, 'wwc-local-operations-retry').dataset.wwcComponent, 'button')
  assert.equal(byClass(rootElement, 'wwc-local-repository').dataset.wwcComponent, 'panel')
  assert.equal(byClass(rootElement, 'wwc-local-resources').dataset.wwcComponent, 'panel')
  assert.equal(byClass(rootElement, 'wwc-local-workers').dataset.wwcComponent, 'panel')
  const text = visibleText(rootElement)
  assert.equal(text.includes('Repository paths hidden'), true)
  assert.equal(text.includes('0123456789ab'), true)
  assert.equal(text.includes('Not reported by Control Plane'), true)
  assert.equal(text.includes(workerId), false)
  assert.equal(text.includes('private-host'), false)
  assert.equal(text.includes('/secret/'), false)
  assert.equal(byClass(rootElement, 'wwc-local-resources-content').dataset.failureClassification, 'none')
  byClass(rootElement, 'wwc-local-worker-drain').dispatch('click')
  assert.deepEqual(calls[0], { operation: 'drain', id: workerId })

  state = pageState({
    workers: [worker({ state: 'draining', revision: 2, capacity: 2 })],
    resources: {
      ...pageState().resources,
      enabledWorkerCount: 0,
      reportedCapacitySlots: 0,
      failureClassification: 'resource-shortage',
    },
  })
  listener(state)
  assert.equal(visibleText(rootElement).includes('Resource shortage · no enabled capacity'), true)
  assert.equal(byClass(rootElement, 'wwc-local-worker-drain').disabled, true)
  byClass(rootElement, 'wwc-local-worker-enable').dispatch('click')
  assert.deepEqual(calls[1], { operation: 'enable', id: workerId })

  state = pageState({ workers: [] })
  listener(state)
  assert.equal(byClass(rootElement, 'wwc-local-worker-empty').dataset.wwcComponent, 'empty-state')
  mounted.close()
})

test('read-only local operations keeps Worker state visible and blocks commands', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const calls = []
  const state = pageState()
  const model = {
    state,
    subscribe(next) { next(state); return () => {} },
    async start() {},
    async refresh() {},
    async drainWorker() { calls.push('drain') },
    async enableWorker() { calls.push('enable') },
    cancelPending() {},
    reconnect() {},
    close() {},
  }
  const mounted = mountLocalOperationsPage({ root: rootElement, model, readOnly: true })
  const drain = byClass(rootElement, 'wwc-local-worker-drain')
  const enable = byClass(rootElement, 'wwc-local-worker-enable')
  assert.equal(drain.disabled, true)
  assert.equal(enable.disabled, true)
  drain.dispatch('click')
  enable.dispatch('click')
  assert.deepEqual(calls, [])
  assert.match(visibleText(rootElement), /Reported capacity slots/iu)
  mounted.close()
})

test('local operations source has one facade and no local process, filesystem, or raw transport path', () => {
  const viewModelSource = readFileSync(
    resolve(root, 'apps/client/src/local-operations-view-model.ts'),
    'utf8',
  )
  const pageSource = readFileSync(
    resolve(root, 'apps/client/src/local-operations-page.ts'),
    'utf8',
  )
  for (const source of [viewModelSource, pageSource]) {
    assert.doesNotMatch(source, /\bfetch\s*\(/u)
    assert.doesNotMatch(source, /new\s+WebSocket/u)
    assert.doesNotMatch(source, /innerHTML/u)
    assert.doesNotMatch(source, /console\./u)
    assert.doesNotMatch(source, /node:fs|child_process|\bprocess\.|localStorage|sessionStorage/u)
    assert.doesNotMatch(source, /navigator\.|performance\./u)
  }
  assert.equal((viewModelSource.match(/\.\/control-plane-client\.js/gu) ?? []).length, 1)
  assert.equal((pageSource.match(/\.\/local-operations-view-model\.js/gu) ?? []).length, 1)

  const codeFailure = pageState({
    resources: { ...pageState().resources, failureClassification: 'code-failure' },
  })
  assert.equal(localOperationsPagePresentation(codeFailure).errorText, null)
  const stale = pageState({
    interaction: {
      status: 'error',
      operation: 'worker.drain',
      workerId,
      error: {
        kind: 'server',
        code: 'REVISION_CONFLICT',
        message: 'raw path /private/repository must stay hidden',
        requestId: null,
        retryable: false,
      },
    },
  })
  assert.equal(
    localOperationsPagePresentation(stale).errorText,
    'This Worker changed before the command was saved. Review the current state and try again.',
  )
})
