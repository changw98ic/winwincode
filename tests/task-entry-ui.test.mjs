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
    'apps/client/tsconfig.task-entry-tests.json',
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
  `Task entry area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/task-entry-tests')
// Plain module paths keep one ControlPlaneClientError class identity across
// the facade, the view-models, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('community-control-plane-client.js')
const clientsViewModelModule = await cachedModule('clients-view-model.js')
const repositoriesViewModelModule = await cachedModule('repositories-view-model.js')
const taskEntryViewModelModule = await cachedModule('task-entry-view-model.js')
const taskEntryPageModule = await cachedModule('task-entry-page.js')
const taskRunViewModelModule = await cachedModule('task-run-view-model.js')
const taskRunPageModule = await cachedModule('task-run-page.js')

const {
  controlPlaneTaskModelRouteOptions,
  createControlPlaneRunIdentityFake,
  createControlPlaneRunIdentityPort,
  createControlPlaneTaskFake,
  createControlPlaneTaskPort,
  createControlPlaneWorkerSessionPort,
} = facade
const { createClientsViewModel } = clientsViewModelModule
const { createRepositoriesViewModel } = repositoriesViewModelModule
const {
  createTaskEntryViewModel,
  deviceSupportsTaskStart,
  repositorySupportsTaskStart,
} = taskEntryViewModelModule
const { mountTaskEntryPage } = taskEntryPageModule
const {
  createTaskRunViewModel,
  runWorkGraphStateTone,
  runWorkerSessionStateText,
  taskRunCommitText,
} = taskRunViewModelModule
const { mountTaskRunPage } = taskRunPageModule

const FIXED_NOW = () => '2026-09-04T00:02:00.000Z'

/** Settle one async model read (repository list, task create, identity read). */
function flush() {
  return new Promise(resolvePromise => setTimeout(resolvePromise, 0))
}

function device(overrides = {}) {
  return {
    clientId: '123456789012',
    displayName: 'Wenjie MacBook Pro',
    presence: 'online',
    occupancy: 'available',
    capacityUsed: 0,
    capacityTotal: 8,
    lastHeartbeatAt: '2026-09-04T00:00:00.000Z',
    version: '1.2.3',
    ...overrides,
  }
}

const OCCUPIED = { occupancy: 'occupied-by-me', capacityUsed: 1, capacityTotal: 8 }

function repository(overrides = {}) {
  return {
    repositoryBindingId: 'rb_10000000000000000000000001',
    displayName: 'WinWinCode',
    defaultBranch: 'main',
    headCommit: 'abc1234567890abcdef1234567890abcdef1234',
    dirtyState: 'clean',
    availability: 'available',
    ...overrides,
  }
}

function clientsFake(devices) {
  let current = devices
  return {
    get devices() { return current },
    set devices(next) { current = next },
    async addClient() { return current },
    async listClients() { return current },
  }
}

function repositoriesFake(byDevice) {
  const calls = []
  let failNext = false
  return {
    calls,
    failNextList() { failNext = true },
    async listRepositories(input) {
      calls.push(input.clientId)
      if (failNext) {
        failNext = false
        throw new Error('unreachable')
      }
      return byDevice[input.clientId] ?? []
    },
  }
}

/** One deterministic task port: every create records and can fail once. */
function taskPortFake() {
  const calls = []
  let sequence = 0
  let failNext = false
  const anchors = new Map()
  return {
    calls,
    anchors,
    failNextCreate() { failNext = true },
    async create(input) {
      calls.push({ ...input })
      if (failNext) {
        failNext = false
        throw new Error('rejected')
      }
      sequence += 1
      const anchor = {
        taskId: `tsk_${String(sequence).padStart(26, '0')}`,
        ...input,
      }
      anchors.set(anchor.taskId, anchor)
      return anchor
    },
    describe(taskId) {
      return anchors.get(taskId) ?? null
    },
  }
}

function entryFixture({
  devices = [device({ ...OCCUPIED })],
  byDevice,
} = {}) {
  const directory = clientsFake(devices)
  const repositoryDirectory = repositoriesFake(
    byDevice ?? {
      '123456789012': [
        repository(),
        repository({
          repositoryBindingId: 'rb_10000000000000000000000002',
          displayName: 'n0vel',
          defaultBranch: 'develop',
        }),
      ],
    },
  )
  const clients = createClientsViewModel({ client: directory })
  const repositories = createRepositoriesViewModel({ client: repositoryDirectory })
  const port = taskPortFake()
  const model = createTaskEntryViewModel({
    clients,
    repositories,
    port,
  })
  return { directory, repositoryDirectory, clients, repositories, port, model }
}

test('only online Clients occupied by the current user may start a task', () => {
  const candidates = [
    device({ clientId: '100000000001' }),
    device({ clientId: '100000000002', ...OCCUPIED }),
    device({ clientId: '100000000003', ...OCCUPIED, presence: 'offline' }),
    device({ clientId: '100000000004', occupancy: 'occupied-by-other' }),
    device({ clientId: '100000000005', occupancy: 'draining', capacityUsed: 1 }),
  ]
  assert.deepEqual(
    candidates.filter(deviceSupportsTaskStart).map(entry => entry.clientId),
    ['100000000002'],
  )
  assert.equal(repositorySupportsTaskStart(repository()), true)
  assert.equal(repositorySupportsTaskStart(repository({ availability: 'dirty' })), false)
})

test('an empty form fails honestly and never calls the task port', async () => {
  const { port, model } = entryFixture()
  await model.start()
  assert.equal(model.state.occupiedDevices.length, 1)
  assert.equal(
    model.state.selection.modelRouteId,
    'route_default',
    'the first route option is the default',
  )

  model.submit()
  assert.equal(model.state.status, 'editing')
  assert.equal(model.state.failure, 'no-occupied-client')
  assert.equal(port.calls.length, 0)
  model.close()
})

test('a form without an occupied Client explains the §16.6 gate', async () => {
  const { model } = entryFixture({ devices: [device()] })
  await model.start()
  assert.equal(model.state.occupiedDevices.length, 0)
  model.submit()
  assert.equal(model.state.failure, 'no-occupied-client')
  model.close()
})

test('choosing a Client reads the shared repository list and defaults the base branch', async () => {
  const { repositoryDirectory, repositories, model } = entryFixture()
  await model.start()
  model.selectClient('123456789012')
  await flush()

  assert.deepEqual(repositoryDirectory.calls, ['123456789012'])
  assert.equal(model.state.selection.clientId, '123456789012')
  assert.equal(
    model.state.selection.repositoryBindingId,
    'rb_10000000000000000000000001',
    'the first usable binding is preselected',
  )
  assert.equal(
    model.state.selection.baseBranch,
    'main',
    'the base branch defaults to the repository default',
  )
  assert.equal(repositories.state.clientId, '123456789012')
  model.close()
})

test('a failed repository read leaves the choice empty and the form names the gap', async () => {
  const { repositoryDirectory, model } = entryFixture()
  await model.start()
  repositoryDirectory.failNextList()
  model.selectClient('123456789012')
  await flush()

  assert.equal(model.state.repositoriesStatus, 'unavailable')
  assert.equal(model.state.selection.repositoryBindingId, null)
  assert.equal(model.state.selection.baseBranch, '')
  model.submit()
  assert.equal(model.state.failure, 'no-repository')
  model.close()
})

test('base-branch drafts survive a refresh and different repositories reset the draft', async () => {
  const { repositoryDirectory, model } = entryFixture()
  await model.start()
  model.selectClient('123456789012')
  await flush()
  model.setBaseBranch('feature/task-42')
  assert.equal(model.state.selection.baseBranch, 'feature/task-42')

  await model.refresh()
  assert.equal(
    model.state.selection.baseBranch,
    'feature/task-42',
    'a repository refresh keeps the user draft',
  )

  const readsBeforeSwitch = repositoryDirectory.calls.length
  model.selectRepository('rb_10000000000000000000000002')
  assert.equal(model.state.selection.baseBranch, 'develop', 'a new repository restarts the draft on its default')
  assert.equal(
    repositoryDirectory.calls.length,
    readsBeforeSwitch,
    'a repository switch never re-reads the device list',
  )
  model.close()
})

test('submit validates every field in order before the port is called', async () => {
  const { port, model } = entryFixture()
  await model.start()
  model.selectClient('123456789012')
  await flush()
  model.setBaseBranch('')
  model.submit()
  assert.equal(model.state.failure, 'missing-base-branch')

  model.setBaseBranch('main')
  model.submit()
  assert.equal(model.state.failure, 'missing-description')

  model.setDescription('Ship the occupancy gate')
  model.selectModelRoute(null)
  model.submit()
  assert.equal(model.state.failure, 'missing-model-route')
  assert.equal(port.calls.length, 0, 'an invalid form never reaches the port')

  model.selectModelRoute('route_fast')
  model.submit()
  assert.deepEqual(port.calls, [{
    clientId: '123456789012',
    repositoryBindingId: 'rb_10000000000000000000000001',
    baseBranch: 'main',
    description: 'Ship the occupancy gate',
    modelRouteId: 'route_fast',
  }])
  await flush()
  assert.equal(model.state.status, 'started')
  assert.equal(model.state.anchor.taskId, 'tsk_00000000000000000000000001')
  model.close()
})

test('a rejected creation keeps every draft and the same submit retries', async () => {
  const { port, model } = entryFixture()
  await model.start()
  model.selectClient('123456789012')
  await flush()
  model.setDescription('Ship the occupancy gate')

  port.failNextCreate()
  model.submit()
  await flush()
  assert.equal(model.state.status, 'editing')
  assert.equal(model.state.failure, 'unavailable')
  assert.equal(
    model.state.selection.description,
    'Ship the occupancy gate',
    'a rejected creation keeps the drafts',
  )

  model.submit()
  await flush()
  assert.equal(port.calls.length, 2)
  assert.equal(model.state.status, 'started')
  assert.notEqual(model.state.anchor, null)
  model.close()
})

test('a Client that leaves the occupied set clears the selection and the repository list', async () => {
  const { directory, repositories, model } = entryFixture()
  await model.start()
  model.selectClient('123456789012')
  await flush()
  assert.notEqual(model.state.selection.repositoryBindingId, null)

  directory.devices = [device({ ...OCCUPIED, occupancy: 'occupied-by-other' })]
  await model.refresh()

  assert.equal(model.state.selection.clientId, null)
  assert.equal(model.state.selection.repositoryBindingId, null)
  assert.equal(model.state.selection.baseBranch, '')
  assert.equal(repositories.state.clientId, null, 'the shared repository list clears with the form')
  model.close()
})

test('the model route options come from the fake §16.6 catalog', () => {
  const options = controlPlaneTaskModelRouteOptions()
  assert.deepEqual(
    options.map(option => option.routeId),
    ['route_default', 'route_long_context', 'route_fast'],
  )
  assert.equal(Object.isFrozen(options), true)
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
    this.id = ''
    this.rows = 0
    this.htmlFor = ''
    this.name = ''
    this.tabIndex = 0
    this.#textContent = ''
  }

  #textContent = ''

  get textContent() { return this.#textContent }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

  get href() { return this.getAttribute('href') ?? '' }

  set href(value) { this.setAttribute('href', value) }

  get options() { return this.children }

  set value(next) {
    if (this.tagName === 'SELECT') {
      // A select keeps only values one of its options names, like the DOM.
      const match = this.children.some(option => option.getAttribute('value') === next)
      this.attributes.set('value', match ? next : '')
      return
    }
    this.attributes.set('value', String(next))
  }

  get value() { return this.getAttribute('value') ?? '' }

  get classList() {
    const self = this
    return {
      add(...names) {
        const set = new Set(self.className.split(/\s+/u).filter(Boolean))
        for (const name of names) set.add(name)
        self.className = [...set].join(' ')
      },
      remove(...names) {
        const set = new Set(self.className.split(/\s+/u).filter(Boolean))
        for (const name of names) set.delete(name)
        self.className = [...set].join(' ')
      },
      contains(name) {
        return self.className.split(/\s+/u).includes(name)
      },
    }
  }

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
    this.listeners.set(
      name,
      (this.listeners.get(name) ?? []).filter(candidate => candidate !== listener),
    )
  }

  dispatch(name, event = {}) {
    const payload = { preventDefault() {}, ...event }
    for (const listener of this.listeners.get(name) ?? []) listener(payload)
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
  return descendants(rootElement).filter(node => node.className.split(/\s+/u).includes(className))
}

function byClass(rootElement, className) {
  const match = allByClass(rootElement, className)[0]
  assert.notEqual(match, undefined, `missing .${className}`)
  return match
}

function visibleText(node) {
  return descendants(node).map(current => current.textContent).join(' ')
}

function optionValues(select) {
  return select.options.map(option => option.getAttribute('value'))
}

function pageEntryFixture({ devices, byDevice } = {}) {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const fixture = entryFixture({ devices, byDevice })
  const started = []
  const page = mountTaskEntryPage({
    root: rootElement,
    model: fixture.model,
    onStarted: anchor => started.push(anchor),
  })
  return { document, rootElement, started, page, ...fixture }
}

test('the form page renders the occupied-Client gate and disables the submit', async () => {
  const { rootElement, model, page } = pageEntryFixture({ devices: [device()] })
  await model.start()

  const section = byClass(rootElement, 'wwc-task-entry')
  assert.equal(section.getAttribute('aria-label'), '新任务')
  const notice = byClass(rootElement, 'wwc-task-entry-occupied-notice')
  assert.equal(notice.hidden, false)
  assert.match(notice.textContent, /没有占用执行设备/u)
  assert.equal(byClass(rootElement, 'wwc-task-entry-submit').disabled, true)

  model.close()
  page.close()
})

test('the form page offers the occupied devices, repositories, and routes', async () => {
  const { rootElement, model, page } = pageEntryFixture()
  await model.start()
  model.selectClient('123456789012')
  await flush()

  const clientSelect = byClass(rootElement, 'wwc-task-entry-client')
  assert.deepEqual(optionValues(clientSelect), ['', '123456789012'])
  assert.equal(clientSelect.value, '123456789012')
  const repositorySelect = byClass(rootElement, 'wwc-task-entry-repository')
  assert.deepEqual(
    optionValues(repositorySelect),
    ['', 'rb_10000000000000000000000001', 'rb_10000000000000000000000002'],
  )
  assert.equal(repositorySelect.value, 'rb_10000000000000000000000001')
  const baseInput = byClass(rootElement, 'wwc-task-entry-base')
  assert.equal(baseInput.value, 'main')
  const routeSelect = byClass(rootElement, 'wwc-task-entry-route')
  assert.deepEqual(
    optionValues(routeSelect),
    ['', 'route_default', 'route_long_context', 'route_fast'],
  )
  assert.equal(routeSelect.value, 'route_default')

  model.close()
  page.close()
})

test('submitting the page fires onStarted exactly once with the anchor', async () => {
  const { rootElement, started, model, page } = pageEntryFixture()
  await model.start()
  model.selectClient('123456789012')
  await flush()
  model.setDescription('Ship the occupancy gate')

  const form = byClass(rootElement, 'wwc-task-entry-form')
  form.dispatch('submit')
  await flush()

  assert.equal(started.length, 1)
  assert.equal(started[0].taskId, 'tsk_00000000000000000000000001')
  assert.equal(started[0].clientId, '123456789012')
  assert.equal(started[0].baseBranch, 'main')
  assert.equal(started[0].modelRouteId, 'route_default')
  assert.equal(
    byClass(rootElement, 'wwc-task-entry-status').hidden,
    true,
    'the busy status line clears once the anchor landed',
  )

  model.close()
  page.close()
})

test('a form failure reaches the alert line and marks its field', async () => {
  const { rootElement, model, page } = pageEntryFixture()
  await model.start()
  model.selectClient('123456789012')
  await flush()
  model.setBaseBranch('')

  const form = byClass(rootElement, 'wwc-task-entry-form')
  form.dispatch('submit')

  const failure = byClass(rootElement, 'wwc-task-entry-error')
  assert.equal(failure.getAttribute('role'), 'alert')
  assert.match(failure.textContent, /基准分支/u)
  const baseInput = byClass(rootElement, 'wwc-task-entry-base')
  assert.equal(baseInput.getAttribute('aria-invalid'), 'true')

  model.close()
  page.close()
})

function identityFake() {
  return createControlPlaneRunIdentityFake({ now: FIXED_NOW })
}

function detailedIdentity() {
  const base = identityFake()
  return {
    async read(anchor) {
      const projection = await base.read(anchor)
      const contractId = 'wct_00000000000000000000000042'
      const criterionId = 'crt_00000000000000000000000042'
      const dependencyId = 'wit_00000000000000000000000041'
      return {
        ...projection,
        contract: {
          schemaVersion: 'winwincode/v1',
          id: contractId,
          revision: 2,
          scope: ['登录回跳'],
          objective: '修复登录后的回跳逻辑。',
          constraints: [],
          protectedScope: [],
          requiredHumanAuthority: 'approval',
          criteria: [{
            id: criterionId,
            description: '成功登录返回原页面。',
            required: true,
            requiredEvidenceClass: 'machine',
            verificationMethod: 'node --test',
          }],
          createdAt: FIXED_NOW(),
        },
        item: {
          schemaVersion: 'winwincode/v1',
          id: anchor.taskId,
          revision: 1,
          title: '修复登录回跳',
          goal: '登录成功后返回原页面。',
          state: 'running',
          criterionIds: [criterionId],
          dependsOn: [dependencyId],
          workContractId: contractId,
          workContractRevision: 2,
        },
        graphItem: {
          workItemId: anchor.taskId,
          state: 'blocked',
          dependencies: [dependencyId],
          blockers: [dependencyId],
        },
        owner: 'chengwen',
        evidence: [{
          id: 'evd_00000000000000000000000042',
          deliverySpecId: 'spec-login',
          deliverySpecRevision: 1,
          workRunId: projection.workRun.id,
          sessionBindingId: 'binding-1',
          candidateRef: `git-candidate:sha256:${'c'.repeat(64)}`,
          type: 'command',
          sourceRef: 'cmd:node --test',
          createdAt: FIXED_NOW(),
        }],
      }
    },
  }
}

test('the production run identity port joins the canonical WorkItem detail cut', async () => {
  const deliveryId = 'dlv_00000000000000000000000042'
  const taskId = 'wit_00000000000000000000000042'
  const contractId = 'wct_00000000000000000000000042'
  const runId = 'wrn_00000000000000000000000042'
  const scope = {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001',
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
  }
  const readCursor = {
    token: `sfread_${'1'.padStart(32, '0')}`,
    scope,
    deliveryId,
    deliveryRevision: 1,
    runtimeLedgerRevision: 1,
    runtimeAcceptedSequence: 1,
    publicationRevision: 0,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 0,
      eventId: null,
    },
  }
  const contract = {
    schemaVersion: 'winwincode/v1',
    id: contractId,
    revision: 1,
    scope: ['登录回跳'],
    objective: '修复登录后的回跳逻辑。',
    constraints: [],
    protectedScope: [],
    requiredHumanAuthority: 'approval',
    criteria: [{
      id: 'crt_00000000000000000000000042',
      description: '成功登录返回原页面。',
      required: true,
      requiredEvidenceClass: 'machine',
      verificationMethod: 'node --test',
    }],
    createdAt: FIXED_NOW(),
  }
  const item = {
    schemaVersion: 'winwincode/v1',
    id: taskId,
    revision: 1,
    title: '修复登录回跳',
    goal: '登录成功后返回原页面。',
    state: 'in_progress',
    criterionIds: ['crt_00000000000000000000000042'],
    dependsOn: [],
    workContractId: contractId,
    workContractRevision: 1,
  }
  const workRun = {
    schemaVersion: 'winwincode/v1',
    id: runId,
    workContractId: contractId,
    contractRevision: 1,
    workItemId: taskId,
    workItemRevision: 1,
    revision: 1,
    state: 'running',
    executionJobId: 'job_00000000000000000000000042',
    attempt: 1,
    workerId: 'wrk_00000000000000000000000042',
    workerInstanceId: 'wki_00000000000000000000000042',
    workerSessionId: 'wsn_00000000000000000000000042',
    leaseId: 'lse_00000000000000000000000042',
    fencingToken: '1',
    productSessionId: 'psn_00000000000000000000000042',
    codexThreadId: null,
    candidateDigest: null,
  }
  const evidence = {
    id: 'evd_00000000000000000000000042',
    deliverySpecId: 'spec-login',
    deliverySpecRevision: 1,
    workRunId: runId,
    sessionBindingId: 'binding-1',
    candidateRef: `git-candidate:sha256:${'c'.repeat(64)}`,
    type: 'command',
    sourceRef: 'cmd:node --test',
    createdAt: FIXED_NOW(),
  }
  const calls = []
  const client = {
    async query(request) {
      calls.push(request.query)
      if (request.query === 'workrun.get') {
        if (request.parameters.atCursor !== null) {
          assert.equal(request.parameters.atCursor.token, readCursor.token)
        }
        return {
          query: 'workrun.get',
          result: {
            schemaVersion: 'winwincode/v1',
            readCursor,
            contract,
            items: [item],
            graphItems: [{ workItemId: taskId, state: 'running', dependencies: [], blockers: [] }],
            runs: [workRun],
            deviceBindings: [{
              workRunId: runId,
              clientId: '123456789012',
              repositoryBindingId: 'rbd_00000000000000000000000042',
            }],
          },
        }
      }
      return {
        query: 'delivery.get',
        result: {
          schemaVersion: 'winwincode/v1',
          kind: 'delivery_detail',
          deliveryId,
          deliveryRevision: 1,
          readCursor,
          ownership: {
            organizationId: scope.organizationId,
            workspaceId: scope.workspaceId,
            projectId: scope.projectId,
            repositoryId: scope.repositoryId,
          },
          status: 'in_progress',
          requirements: {
            deliverySpecId: 'spec-login',
            deliverySpecRevision: 1,
            title: '修复登录回跳',
            goal: '登录成功后返回原页面。',
            scope: ['登录回跳'],
            outOfScope: [],
            constraints: [],
            acceptanceCriteria: [{
              id: 'AC-1',
              description: '成功登录返回原页面。',
              verificationMethod: 'node --test',
              required: true,
            }],
            sourceProductSessionId: null,
            sourceRef: null,
            publicationTarget: null,
            repository: { kind: 'local-git', locator: 'workspace://repository' },
            baseRevision: '0123456789abcdef0123456789abcdef01234567',
            maxReworkAttempts: 2,
          },
          solutionReview: null,
          diagramExecution: null,
          attention: [],
          evidence: [evidence],
          currentCandidate: null,
          verdict: null,
          publication: null,
        },
      }
    },
  }
  const port = createControlPlaneRunIdentityPort({
    client,
    candidates: { async listDeviceCandidates() { throw new Error('unexpected read') } },
    actor: () => ({ kind: 'human', id: 'hum_00000000000000000000000001' }),
    scope: () => scope,
    deliveryId: () => deliveryId,
    nextRequestId: () => 'req_00000000000000000000000042',
  })

  const projection = await port.read({
    taskId,
    clientId: '000000000000',
    repositoryBindingId: 'rbd_00000000000000000000000000',
    baseBranch: 'main',
    description: 'stale route values',
    modelRouteId: 'route_default',
  })

  assert.deepEqual(calls, ['delivery.get', 'workrun.get'])
  assert.equal(projection.clientId, '123456789012')
  assert.equal(projection.contract.id, contractId)
  assert.equal(projection.item.id, taskId)
  assert.equal(projection.graphItem.state, 'running')
  assert.deepEqual(projection.evidence.map(entry => entry.id), [evidence.id])

  const chain = []
  const taskClient = {
    serverUrl: 'https://control.example',
    async command(request) {
      chain.push(request.command)
      if (request.command === 'workitems.create') {
        return {
          command: request.command,
          outcome: 'completed',
          currentRevision: 2,
          result: { items: [item] },
        }
      }
      return {
        command: request.command,
        outcome: 'completed',
        currentRevision: 3,
        result: { activeWorkRunId: runId },
      }
    },
    async query(request) {
      chain.push(request.query)
      return client.query(request)
    },
  }
  const workerSessions = createControlPlaneWorkerSessionPort({
    client: taskClient,
    transport: {
      async fetch(_url, init) {
        chain.push('client.worker.launch')
        const body = JSON.parse(init.body)
        assert.equal(body.workRunId, runId)
        return {
          ok: true,
          status: 201,
          async text() {
            return JSON.stringify(body)
          },
        }
      },
    },
  })
  const tasks = createControlPlaneTaskPort({
    client: taskClient,
    actor: () => ({ kind: 'human', id: 'hum_00000000000000000000000001' }),
    scope: () => scope,
    nextRequestId: () => 'req_00000000000000000000000042',
    nextWorkItemId: () => taskId,
    workerSessions,
    deliveryContext: async () => ({
      deliveryId,
      expectedRevision: 1,
      contractRevision: 1,
      criterionId: contract.criteria[0].id,
    }),
  })
  await tasks.create({
    clientId: '123456789012',
    repositoryBindingId: 'rbd_00000000000000000000000042',
    baseBranch: 'main',
    description: '修复登录回跳',
    modelRouteId: 'route_default',
  })
  assert.deepEqual(chain, [
    'workitems.create',
    'workrun.start',
    'client.worker.launch',
    'workrun.get',
  ])
})

function runFixture({
  devices = [device({ ...OCCUPIED })],
  byDevice,
  identity = identityFake(),
  anchor = {
    taskId: 'tsk_00000000000000000000000042',
    clientId: '123456789012',
    repositoryBindingId: 'rb_10000000000000000000000001',
    baseBranch: 'main',
    description: 'Ship the occupancy gate',
    modelRouteId: 'route_default',
  },
} = {}) {
  const directory = clientsFake(devices)
  const repositoryDirectory = repositoriesFake(
    byDevice ?? {
      '123456789012': [repository()],
    },
  )
  const clients = createClientsViewModel({ client: directory })
  const repositories = createRepositoriesViewModel({ client: repositoryDirectory })
  const model = createTaskRunViewModel({
    anchor,
    taskDescription: anchor.description,
    clients,
    repositories,
    identity,
  })
  return { directory, repositoryDirectory, clients, repositories, identity, anchor, model }
}

test('the run projection composes live facts with the fake identity zone', async () => {
  const { repositoryDirectory, model } = runFixture()
  await model.start()

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.taskDescription, 'Ship the occupancy gate')
  assert.equal(model.state.client.displayName, 'Wenjie MacBook Pro')
  assert.match(model.state.client.stateText, /已由你占用/u)
  assert.equal(model.state.occupancy.capacityText, '容量 1 / 8')
  assert.equal(model.state.repository.displayName, 'WinWinCode')
  assert.equal(model.state.repository.defaultBranch, 'main')
  assert.equal(model.state.identityStatus, 'ready')
  assert.deepEqual(repositoryDirectory.calls, ['123456789012'])

  const identity = model.state.identity
  assert.equal(identity.workerSessions.length, 1)
  assert.equal(identity.workerSessions[0].state, 'running')
  assert.equal(identity.workerSessions[0].workerSessionId, 'wsn_00000000000000000000000042')
  assert.equal(identity.candidate, null)
  assert.equal(identity.apply, null)
  model.close()
})

test('closing the run page only detaches the view and never stops its Worker', async () => {
  const operations = []
  const base = identityFake()
  const identity = {
    async read(anchor) {
      operations.push('read')
      return base.read(anchor)
    },
    async cancel() {
      operations.push('cancel')
    },
  }
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const { model } = runFixture({ identity })
  const page = mountTaskRunPage({ root: rootElement, model })
  await model.start()
  assert.equal(model.state.identity.workerSessions[0].state, 'running')

  page.close()

  assert.deepEqual(operations, ['read'])
  assert.equal(model.state.identity.workerSessions[0].state, 'running')
  model.close()
})

test('the run projection follows the served device binding and Candidate apply history', async () => {
  const servedClientId = '987654321012'
  const servedBindingId = 'rbd_20000000000000000000000001'
  const fake = identityFake()
  const identity = {
    async read(anchor) {
      const projection = await fake.read(anchor)
      return {
        ...projection,
        clientId: servedClientId,
        repositoryBindingId: servedBindingId,
        candidate: {
          candidateRef: `git-candidate:sha256:${'a'.repeat(64)}`,
          state: 'branch_created',
          branchName: 'winwincode/task/candidate',
          history: [{
            localApplyReceiptId: 'lap_00000000000000000000000001',
            candidateRef: `git-candidate:sha256:${'a'.repeat(64)}`,
            repositoryBindingId: servedBindingId,
            targetBranch: 'winwincode/task/candidate',
            expectedHead: 'abc1234',
            strategy: 'create_branch',
            result: 'branch_created',
            resultingCommit: 'abc1234567890',
            conflictArtifactRef: null,
            createdAt: FIXED_NOW(),
            revision: 1,
          }],
        },
      }
    },
  }
  const { model } = runFixture({
    devices: [
      device({ ...OCCUPIED }),
      device({ ...OCCUPIED, clientId: servedClientId, displayName: '执行设备 B' }),
    ],
    byDevice: {
      [servedClientId]: [repository({
        repositoryBindingId: servedBindingId,
        displayName: '绑定仓库 B',
      })],
    },
    identity,
  })

  await model.start()

  assert.equal(model.state.client.displayName, '执行设备 B')
  assert.equal(model.state.repository.displayName, '绑定仓库 B')
  assert.equal(model.state.identity.candidate.branchName, 'winwincode/task/candidate')
  assert.equal(model.state.identity.apply.result, 'branch_created')
  assert.equal(model.state.identity.apply.resultingCommit, 'abc1234567890')
  model.close()
})

test('an unavailable identity read keeps the live rows and names the gap', async () => {
  const identity = {
    async read() {
      throw new Error('unreachable')
    },
  }
  const { model } = runFixture({ identity })
  await model.start()

  assert.equal(model.state.status, 'partial')
  assert.equal(model.state.identity, null)
  assert.equal(model.state.identityStatus, 'unavailable')
  assert.equal(
    model.state.client.displayName,
    'Wenjie MacBook Pro',
    'the live rows survive the identity failure',
  )
  model.close()
})

test('the run page renders the twelve identity rows and seven WorkItem facts', async () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const fixture = runFixture({ identity: detailedIdentity() })
  const page = mountTaskRunPage({
    root: rootElement,
    model: fixture.model,
    homeHref: '#/home?organizationId=org_00000000000000000000000001',
    clarificationHref: '#/home/clarify?delivery=dlv_00000000000000000000000001',
  })
  await fixture.model.start()

  // Design page 05: back link left, 流程与记录 display slot right.
  const back = byClass(rootElement, 'wwc-task-run-back')
  assert.equal(back.textContent, '返回看板')
  assert.equal(
    back.href,
    '#/home?organizationId=org_00000000000000000000000001',
  )
  const topbarActions = byClass(rootElement, 'wwc-task-run-topbar-actions')
  assert.match(visibleText(topbarActions), /流程与记录/u)
  assert.equal(
    byClass(rootElement, 'wwc-task-run-clarification-link').href,
    '#/home/clarify?delivery=dlv_00000000000000000000000001',
  )
  assert.match(visibleText(topbarActions), /更多/u)

  assert.match(visibleText(byClass(rootElement, 'wwc-task-run-heading')), /运行中的任务/u)
  assert.equal(
    byClass(rootElement, 'wwc-task-run-status').textContent,
    '强流程 · 运行中',
  )
  assert.match(visibleText(byClass(rootElement, 'wwc-task-run-description')), /Ship the occupancy gate/u)

  // The StrongFlow-owned actions stay visible but disabled: no fake success.
  const approve = byClass(rootElement, 'wwc-task-run-approve')
  assert.equal(approve.textContent, '批准方案并执行')
  assert.equal(approve.disabled, true)
  assert.match(approve.title, /StrongFlow/u)
  const requestChange = byClass(rootElement, 'wwc-task-run-request-change')
  assert.equal(requestChange.textContent, '提出修改')
  assert.equal(requestChange.disabled, true)

  // The identity table stays collapsed behind the design-05 row.
  const identityToggle = byClass(rootElement, 'wwc-task-run-identity-toggle')
  assert.equal(identityToggle.textContent, '展开完整运行身份 · 12 行')
  const rowsContainer = byClass(rootElement, 'wwc-task-run-rows')
  assert.equal(rowsContainer.hidden, true)
  identityToggle.dispatch('click')
  assert.equal(identityToggle.textContent, '收起完整运行身份 · 12 行')
  assert.equal(rowsContainer.hidden, false)

  const rows = allByClass(rootElement, 'wwc-task-run-row')
  assert.equal(rows.length, 12)
  assert.deepEqual(rows.map(row => row.dataset.taskRunRow), [
    '执行设备',
    '仓库',
    '占用状态',
    '工作契约',
    '验收条件',
    '负责人',
    '依赖项',
    '阻塞项',
    '证据',
    '执行会话',
    '候选结果',
    '应用结果',
  ])
  const values = new Map(rows.map(row => [row.dataset.taskRunRow, row]))
  assert.match(visibleText(values.get('执行设备')), /Wenjie MacBook Pro/u)
  assert.match(visibleText(values.get('执行设备')), /已由你占用/u)
  assert.match(visibleText(values.get('仓库')), /WinWinCode · 基准分支 main/u)
  assert.match(visibleText(values.get('占用状态')), /容量 1 \/ 8/u)
  assert.match(visibleText(values.get('工作契约')), /修复登录后的回跳逻辑/u)
  assert.match(visibleText(values.get('验收条件')), /成功登录返回原页面/u)
  assert.match(visibleText(values.get('负责人')), /chengwen/u)
  assert.match(visibleText(values.get('依赖项')), /wit_00000000000000000000000041/u)
  assert.match(visibleText(values.get('阻塞项')), /wit_00000000000000000000000041/u)
  assert.match(visibleText(values.get('证据')), /cmd:node --test/u)
  assert.match(visibleText(values.get('执行会话')), /运行中/u)
  assert.match(visibleText(values.get('候选结果')), /尚无候选结果/u)
  assert.match(visibleText(values.get('应用结果')), /尚无应用记录/u)
  assert.equal(
    byClass(rootElement, 'wwc-task-run-identity-notice').hidden,
    true,
    'a served identity zone shows no gap notice',
  )

  fixture.model.close()
  page.close()
})

test('the run page keeps the identity rows honest when the zone is unreachable', async () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const identity = {
    async read() {
      throw new Error('unreachable')
    },
  }
  const fixture = runFixture({ identity })
  const page = mountTaskRunPage({ root: rootElement, model: fixture.model })
  await fixture.model.start()

  const notice = byClass(rootElement, 'wwc-task-run-identity-notice')
  assert.equal(notice.hidden, false)
  assert.match(notice.textContent, /不可达/u)
  const rows = allByClass(rootElement, 'wwc-task-run-row')
  const values = new Map(rows.map(row => [row.dataset.taskRunRow, row]))
  assert.match(visibleText(values.get('执行设备')), /Wenjie MacBook Pro/u)
  assert.match(visibleText(values.get('执行会话')), /正在加载/u)
  assert.equal(
    byClass(rootElement, 'wwc-task-run-status').textContent,
    '强流程 · 部分身份信息不可用',
  )

  fixture.model.close()
  page.close()
})

test('the fake task port issues stable ids and answers describe', async () => {
  const port = createControlPlaneTaskFake()
  const first = await port.create({
    clientId: '123456789012',
    repositoryBindingId: 'rb_10000000000000000000000001',
    baseBranch: 'main',
    description: 'first',
    modelRouteId: 'route_default',
  })
  const second = await port.create({
    clientId: '123456789012',
    repositoryBindingId: 'rb_10000000000000000000000001',
    baseBranch: 'main',
    description: 'second',
    modelRouteId: 'route_fast',
  })
  assert.notEqual(first.taskId, second.taskId)
  assert.equal(port.describe(first.taskId).description, 'first')
  assert.equal(port.describe('tsk_unknown'), null)

  const identity = createControlPlaneRunIdentityFake({ now: FIXED_NOW })
  const projection = await identity.read(first)
  assert.equal(projection.taskId, first.taskId)
  assert.equal(projection.workRun.workItemId, first.taskId)
  assert.equal(projection.workRun.state, 'running')
  assert.equal(projection.candidate, null)
})

test('the run presentation helpers keep one tone and commit vocabulary', () => {
  assert.equal(runWorkerSessionStateText('running'), '运行中')
  assert.equal(runWorkerSessionStateText('draining'), '正在完成当前工作')
  assert.equal(runWorkGraphStateTone('blocked'), 'warning')
  assert.equal(taskRunCommitText('abc1234567890'), 'abc1234')
  assert.equal(taskRunCommitText(null), null)
})
