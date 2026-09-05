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
const facade = await cachedModule('control-plane-client.js')
const clientsViewModelModule = await cachedModule('clients-view-model.js')
const repositoriesViewModelModule = await cachedModule('repositories-view-model.js')
const taskEntryViewModelModule = await cachedModule('task-entry-view-model.js')
const taskEntryPageModule = await cachedModule('task-entry-page.js')
const taskRunViewModelModule = await cachedModule('task-run-view-model.js')
const taskRunPageModule = await cachedModule('task-run-page.js')

const {
  controlPlaneTaskModelRouteOptions,
  createControlPlaneRunIdentityFake,
  createControlPlaneTaskFake,
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
  assert.equal(section.getAttribute('aria-label'), 'New task')
  const notice = byClass(rootElement, 'wwc-task-entry-occupied-notice')
  assert.equal(notice.hidden, false)
  assert.match(notice.textContent, /No Client is occupied by you/u)
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
  assert.match(failure.textContent, /base branch/u)
  const baseInput = byClass(rootElement, 'wwc-task-entry-base')
  assert.equal(baseInput.getAttribute('aria-invalid'), 'true')

  model.close()
  page.close()
})

function identityFake() {
  return createControlPlaneRunIdentityFake({ now: FIXED_NOW })
}

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
  assert.match(model.state.client.stateText, /occupied by you/u)
  assert.equal(model.state.occupancy.capacityText, 'Capacity 1 / 8')
  assert.equal(model.state.repository.displayName, 'WinWinCode')
  assert.equal(model.state.repository.defaultBranch, 'main')
  assert.equal(model.state.identityStatus, 'ready')
  assert.deepEqual(repositoryDirectory.calls, ['123456789012'])

  const identity = model.state.identity
  assert.equal(identity.workerSessions.length, 1)
  assert.equal(identity.workerSessions[0].state, 'running')
  assert.equal(identity.workerSessions[0].workerSessionId, 'wss_00000000000000000000000042')
  assert.equal(identity.candidate.candidateRef, 'cand_00000000000000000000000042')
  assert.equal(identity.candidate.branchName, 'winwincode/task/tsk_00000000000000000000000042')
  assert.equal(identity.apply.result, 'branch_created')
  assert.equal(identity.apply.targetBranch, 'winwincode/task/tsk_00000000000000000000000042')
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

test('the run page renders the six §16.7 identity rows from served facts', async () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const fixture = runFixture()
  const page = mountTaskRunPage({
    root: rootElement,
    model: fixture.model,
    homeHref: '#/home?organizationId=org_00000000000000000000000001',
  })
  await fixture.model.start()

  const rows = allByClass(rootElement, 'wwc-task-run-row')
  assert.equal(rows.length, 6)
  assert.deepEqual(rows.map(row => row.dataset.taskRunRow), [
    'Client',
    'Repository',
    'Occupancy',
    'Worker sessions',
    'Candidate',
    'Apply',
  ])
  const values = new Map(rows.map(row => [row.dataset.taskRunRow, row]))
  assert.match(visibleText(values.get('Client')), /Wenjie MacBook Pro/u)
  assert.match(visibleText(values.get('Client')), /occupied by you/u)
  assert.match(visibleText(values.get('Repository')), /WinWinCode · base main/u)
  assert.match(visibleText(values.get('Occupancy')), /Capacity 1 \/ 8/u)
  assert.match(visibleText(values.get('Worker sessions')), /Running/u)
  assert.match(visibleText(values.get('Candidate')), /cand_00000000000000000000000042/u)
  assert.match(visibleText(values.get('Candidate')), /Local branch created/u)
  assert.match(visibleText(values.get('Apply')), /Local branch created/u)
  assert.match(visibleText(values.get('Apply')), /Target winwincode\/task\/tsk_/u)
  assert.equal(
    byClass(rootElement, 'wwc-task-run-identity-notice').hidden,
    true,
    'a served identity zone shows no gap notice',
  )
  assert.equal(
    byClass(rootElement, 'wwc-task-run-back').href,
    '#/home?organizationId=org_00000000000000000000000001',
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
  assert.match(notice.textContent, /unreachable right now/u)
  const rows = allByClass(rootElement, 'wwc-task-run-row')
  const values = new Map(rows.map(row => [row.dataset.taskRunRow, row]))
  assert.match(visibleText(values.get('Client')), /Wenjie MacBook Pro/u)
  assert.match(visibleText(values.get('Worker sessions')), /loading/u)

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
  assert.equal(projection.workerSessions.length, 1)
  assert.equal(projection.candidate.state, 'branch_created')
  assert.equal(projection.candidate.history.length, 1)
  assert.equal(projection.candidate.history[0].result, 'branch_created')
})

test('the run presentation helpers keep one tone and commit vocabulary', () => {
  assert.equal(runWorkerSessionStateText('running'), 'Running')
  assert.equal(runWorkerSessionStateText('draining'), 'Finishing current work')
  assert.equal(taskRunCommitText('abc1234567890'), 'abc1234')
  assert.equal(taskRunCommitText(null), null)
})
