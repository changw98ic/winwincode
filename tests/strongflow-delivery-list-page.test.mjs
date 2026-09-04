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
    'apps/client/tsconfig.strongflow-page-tests.json',
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
  `StrongFlow delivery list page did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const module = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-delivery-list-page.js',
)).href}`)
const { mountStrongFlowDeliveryList } = module

const scopeSelection = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}

function summary(index, overrides = {}) {
  return {
    schemaVersion: 'winwincode/v1',
    deliveryId: `dlv_${String(index).padStart(26, '0')}`,
    revision: 1,
    status: 'draft',
    title: `Delivery ${String(index)}`,
    updatedAt: `2026-01-0${String((index % 9) + 1)}T00:00:00Z`,
    openAttentionCount: 0,
    activeStageRunId: null,
    ownership: { ...scopeSelection },
    taskCounts: {
      total: 0,
      pending: 0,
      active: 0,
      blocked: 0,
      verifying: 0,
      completed: 0,
      failed: 0,
    },
    ...overrides,
  }
}

function listState(overrides = {}) {
  return {
    status: 'ready',
    filters: {
      search: '',
      status: null,
      attentionOnly: false,
      order: 'recent',
    },
    visible: [],
    loadedCount: 0,
    hasMore: false,
    loadingMore: false,
    moreFailure: null,
    error: null,
    advance: { deliveryId: null, failure: null },
    ...overrides,
  }
}

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
  checked = false
  draggable = false
  href = ''
  #textContent = ''

  get textContent() {
    return this.#textContent + this.children.map(child => child.textContent).join('')
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

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

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    this.listeners.set(
      name,
      (this.listeners.get(name) ?? []).filter(candidate => candidate !== listener),
    )
  }

  emit(name, values = {}) {
    const event = {
      target: this,
      preventDefault() {},
      ...values,
    }
    let current = this
    while (current !== null) {
      for (const listener of current.listeners.get(name) ?? []) listener(event)
      current = current.parentNode
    }
  }

  focus() { this.ownerDocument.activeElement = this }

  click() { this.emit('click') }
}

class FakeDocument {
  activeElement = null

  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

class FakeListModel {
  constructor(initialState) {
    this.state = initialState
  }

  calls = []
  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }

  async start() { this.calls.push(['start']) }
  async refresh() { this.calls.push(['refresh']) }
  async loadMore() { this.calls.push(['loadMore']) }
  setSearch(value) { this.calls.push(['setSearch', value]) }
  async setStatusFilter(value) { this.calls.push(['setStatusFilter', value]) }
  setAttentionOnly(value) { this.calls.push(['setAttentionOnly', value]) }
  setOrder(value) { this.calls.push(['setOrder', value]) }
  async advanceDelivery(id, revision) {
    this.calls.push(['advanceDelivery', id, revision])
  }
  close() { this.calls.push(['close']) }
}

function findByClass(node, className) {
  if (node.className === className) return node
  for (const child of node.children) {
    const match = findByClass(child, className)
    if (match !== null) return match
  }
  return null
}

function findAllByClass(node, className, matches = []) {
  if (node.className === className) matches.push(node)
  for (const child of node.children) findAllByClass(child, className, matches)
  return matches
}

function mounted(state, options = {}) {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const model = new FakeListModel(state)
  const page = mountStrongFlowDeliveryList({
    root: rootElement,
    model,
    view: 'list',
    ...options,
  })
  return { document, rootElement, model, page }
}

test('list rows carry the scoped route, exact title, status, and the active marker', () => {
  const { rootElement, page } = mounted(listState({
    visible: [
      summary(1),
      summary(2, { status: 'executing', title: 'Other effort' }),
    ],
    loadedCount: 2,
  }), { routeScope: scopeSelection })
  page.setActive(summary(2, { status: 'executing', title: 'Other effort', revision: 3 }))

  const rows = findAllByClass(rootElement, 'wwc-strongflow-delivery-list')[0].children
  assert.equal(rows.length, 2)
  const [first, second] = rows
  assert.equal(first.children[0].textContent, 'Delivery 1')
  assert.equal(
    first.children[0].href,
    '#/strongflow?delivery=dlv_00000000000000000000000001'
      + `&organizationId=${scopeSelection.organizationId}`
      + `&workspaceId=${scopeSelection.workspaceId}`
      + `&projectId=${scopeSelection.projectId}`
      + `&repositoryId=${scopeSelection.repositoryId}`,
  )
  assert.equal(second.children[0].getAttribute('aria-current'), 'page')
  assert.equal(first.children[0].getAttribute('aria-current'), null)
  assert.match(second.children[1].textContent, /executing/u)
})

test('the toolbar edits exactly one model call per control', async () => {
  const onViewChangeCalls = []
  const { rootElement, model, page } = mounted(listState(), {
    routeScope: scopeSelection,
    onViewChange(view) {
      onViewChangeCalls.push(view)
    },
  })

  const search = findByClass(rootElement, 'wwc-delivery-search')
  search.value = ' kernel '
  search.emit('input')

  const statusFilter = findByClass(rootElement, 'wwc-delivery-status-filter')
  statusFilter.value = 'executing'
  statusFilter.emit('change')

  const attention = findByClass(rootElement, 'wwc-delivery-attention-filter')
  attention.checked = true
  attention.emit('change')

  const order = findByClass(rootElement, 'wwc-delivery-order')
  order.value = 'title'
  order.emit('change')

  findByClass(rootElement, 'wwc-delivery-refresh').click()
  await new Promise(resolveTick => setTimeout(resolveTick, 0))

  assert.deepEqual(model.calls, [
    ['setSearch', ' kernel '],
    ['setStatusFilter', 'executing'],
    ['setAttentionOnly', true],
    ['setOrder', 'title'],
    ['refresh'],
  ])

  const listButton = findByClass(rootElement, 'wwc-delivery-view-list')
  const kanbanButton = findByClass(rootElement, 'wwc-delivery-view-kanban')
  assert.equal(listButton.getAttribute('aria-pressed'), 'true')
  kanbanButton.click()
  assert.deepEqual(onViewChangeCalls, ['kanban'])
  assert.equal(kanbanButton.getAttribute('aria-pressed'), 'true')
  assert.equal(listButton.getAttribute('aria-pressed'), 'false')
  assert.equal(findByClass(rootElement, 'wwc-delivery-kanban-view')?.hidden, false)
  page.close()
})

test('status options cover the whole server projection vocabulary plus all', () => {
  const { rootElement, page } = mounted(listState())
  const statusFilter = findByClass(rootElement, 'wwc-delivery-status-filter')
  const values = statusFilter.children.map(option => option.value)
  assert.deepEqual(values, [
    '',
    'draft',
    'clarifying',
    'ready',
    'planning',
    'plan-review',
    'executing',
    'verifying',
    'reworking',
    'needs-attention',
    'ready-to-deliver',
    'delivered',
  ])
  page.close()
})

test('loading, refreshing, and empty states give bounded, honest feedback', () => {
  const { rootElement: loadingRoot, page: loadingPage } = mounted(listState({ status: 'loading' }))
  const loading = findByClass(loadingRoot, 'wwc-delivery-feedback')
  assert.equal(loading.getAttribute('role'), 'status')
  assert.match(loading.textContent, /Loading Deliveries/u)
  assert.equal(findByClass(loadingRoot, 'wwc-delivery-empty').hidden, true)
  loadingPage.close()

  const { rootElement: emptyRoot, page: emptyPage } = mounted(listState())
  assert.match(findByClass(emptyRoot, 'wwc-delivery-empty').textContent, /no Deliveries/iu)
  emptyPage.close()

  const filteredRoot = mounted(listState({
    visible: [],
    loadedCount: 5,
    filters: {
      search: 'kernel',
      status: null,
      attentionOnly: false,
      order: 'recent',
    },
  })).rootElement
  assert.match(findByClass(filteredRoot, 'wwc-delivery-empty').textContent, /loaded 5/u)
})

test('load more appears only with a server cursor and reports its failure with a restart', async () => {
  const { rootElement, model, page } = mounted(listState({
    visible: [summary(1)],
    loadedCount: 1,
    hasMore: true,
  }))
  const loadMore = findByClass(rootElement, 'wwc-delivery-load-more')
  assert.equal(loadMore.hidden, false)
  loadMore.click()
  await new Promise(resolveTick => setTimeout(resolveTick, 0))
  assert.deepEqual(model.calls.at(-1), ['loadMore'])

  model.publish(listState({
    visible: [summary(1)],
    loadedCount: 1,
    hasMore: true,
    loadingMore: true,
  }))
  assert.equal(findByClass(rootElement, 'wwc-delivery-load-more').disabled, true)

  model.publish(listState({
    visible: [summary(1)],
    loadedCount: 1,
    hasMore: true,
    moreFailure: {
      kind: 'server',
      code: 'READ_CURSOR_EXPIRED',
      message: 'stale',
      requestId: null,
    },
  }))
  const alert = findByClass(rootElement, 'wwc-delivery-alert')
  assert.equal(alert.hidden, false)
  assert.equal(alert.getAttribute('role'), 'alert')
  assert.match(alert.textContent, /no longer current|expired|refresh/iu)
  const restart = findAllByClass(rootElement, 'wwc-delivery-alert-action')
    .find(button => button.dataset.action === 'refresh')
  restart.click()
  await new Promise(resolveTick => setTimeout(resolveTick, 0))
  assert.deepEqual(model.calls.at(-1), ['refresh'])

  model.publish(listState({ visible: [summary(1)], loadedCount: 1 }))
  assert.equal(findByClass(rootElement, 'wwc-delivery-load-more').hidden, true)
  assert.equal(findByClass(rootElement, 'wwc-delivery-alert').hidden, true)
  page.close()
})

test('a first-load permission failure is a denial with an explicit retry action', async () => {
  const { rootElement, model, page } = mounted(listState({
    status: 'error',
    error: {
      kind: 'authorization',
      code: 'PERMISSION_DENIED',
      message: 'denied',
      requestId: null,
    },
  }))
  const alert = findByClass(rootElement, 'wwc-delivery-alert')
  assert.equal(alert.hidden, false)
  assert.match(alert.textContent, /not authorized|permission/iu)
  const retry = findAllByClass(rootElement, 'wwc-delivery-alert-action')
    .find(button => button.dataset.action === 'refresh')
  retry.click()
  await new Promise(resolveTick => setTimeout(resolveTick, 0))
  assert.deepEqual(model.calls.at(-1), ['refresh'])
  page.close()
})

test('a first-load transport failure shows the code and a retry action', async () => {
  const { rootElement, model, page } = mounted(listState({
    status: 'error',
    error: {
      kind: 'network',
      code: 'NETWORK_ERROR',
      message: 'offline',
      requestId: null,
    },
  }))
  const alert = findByClass(rootElement, 'wwc-delivery-alert')
  assert.match(alert.textContent, /NETWORK_ERROR/u)
  findAllByClass(rootElement, 'wwc-delivery-alert-action')
    .find(button => button.dataset.action === 'refresh')
    .click()
  await new Promise(resolveTick => setTimeout(resolveTick, 0))
  assert.deepEqual(model.calls.at(-1), ['refresh'])
  page.close()
})

test('the loaded note stays honest about visible and loaded counts', () => {
  const { rootElement, page } = mounted(listState({
    visible: [summary(1)],
    loadedCount: 3,
    hasMore: true,
  }))
  const note = findByClass(rootElement, 'wwc-delivery-loaded-note')
  assert.match(note.textContent, /1 of 3/u)
  assert.match(note.textContent, /3/u)
  page.close()
})

test('kanban columns follow the server projection and never invent columns', () => {
  const { rootElement, page } = mounted(listState({
    visible: [
      summary(1, { status: 'executing' }),
      summary(2, { status: 'draft' }),
      summary(3, { status: 'executing', title: 'Second effort' }),
    ],
    loadedCount: 3,
  }), { view: 'kanban' })

  const columns = findAllByClass(rootElement, 'wwc-delivery-kanban-column')
  assert.deepEqual(columns.map(column => column.dataset.status), ['draft', 'executing'])
  const executingCards = columns[1].children[1].children
  assert.equal(executingCards.length, 2)
  assert.equal(columns[0].children[1].children.length, 1)
  page.close()
})

test('kanban drops route through delivery.advance and never move the card themselves', async () => {
  const { rootElement, model, page } = mounted(listState({
    visible: [
      summary(1, { status: 'ready-to-deliver', revision: 4 }),
      summary(2, { status: 'draft', revision: 1 }),
    ],
    loadedCount: 2,
  }), { view: 'kanban' })

  const columnByStatus = status => findAllByClass(rootElement, 'wwc-delivery-kanban-column')
    .find(column => column.dataset.status === status)
  const readyColumn = columnByStatus('ready-to-deliver')
  const draftColumn = columnByStatus('draft')
  const card = findAllByClass(rootElement, 'wwc-delivery-kanban-card')
    .find(candidate => candidate.dataset.deliveryId === 'dlv_00000000000000000000000001')

  card.emit('dragstart')
  draftColumn.emit('drop')
  await new Promise(resolveTick => setTimeout(resolveTick, 0))
  assert.deepEqual(model.calls.at(-1), [
    'advanceDelivery',
    'dlv_00000000000000000000000001',
    4,
  ])

  draftColumn.emit('drop')
  await new Promise(resolveTick => setTimeout(resolveTick, 0))
  assert.equal(model.calls.length, 1, 'a drop without a drag payload does nothing')

  card.emit('dragstart')
  readyColumn.emit('drop')
  await new Promise(resolveTick => setTimeout(resolveTick, 0))
  assert.equal(model.calls.length, 1, 'a drop on the current column does nothing')
  page.close()
})

test('an advance rejection surfaces the server code and keeps the cards unchanged', async () => {
  const state = listState({
    visible: [summary(1, { status: 'ready-to-deliver', revision: 4 })],
    loadedCount: 1,
    advance: {
      deliveryId: 'dlv_00000000000000000000000001',
      failure: {
        kind: 'server',
        code: 'WRONG_STATE',
        message: 'illegal',
        requestId: null,
      },
    },
  })
  const { rootElement, page } = mounted(state, { view: 'kanban' })
  const alert = findByClass(rootElement, 'wwc-delivery-alert')
  assert.equal(alert.hidden, false)
  assert.match(alert.textContent, /WRONG_STATE/u)
  assert.match(alert.textContent, /StrongFlow/iu)
  const column = findAllByClass(rootElement, 'wwc-delivery-kanban-column')
    .find(candidate => candidate.dataset.status === 'ready-to-deliver')
  assert.equal(column.children[1].children.length, 1, 'the card stays in its server column')
  page.close()
})

test('read-only StrongFlow disables drag moves while keeping navigation', () => {
  const { rootElement, page } = mounted(listState({
    visible: [summary(1, { status: 'ready-to-deliver', revision: 4 })],
    loadedCount: 1,
  }), { view: 'kanban', readOnly: true })

  const card = findByClass(rootElement, 'wwc-delivery-kanban-card')
  assert.equal(card.draggable, false)
  assert.notEqual(card.children[0].href, '')
  page.close()
})

test('setActive keeps row identity so focus and scroll survive updates', () => {
  const { document, rootElement, model, page } = mounted(listState({
    visible: [summary(1), summary(2)],
    loadedCount: 2,
  }))
  const list = findByClass(rootElement, 'wwc-strongflow-delivery-list')
  const originalRow = list.children[1]
  const link = originalRow.children[0]
  link.focus()
  assert.equal(document.activeElement, link)

  model.publish(listState({
    visible: [summary(1), summary(2, { revision: 9 })],
    loadedCount: 2,
  }))
  page.setActive(summary(2, { revision: 9 }))

  assert.equal(list.children[1], originalRow, 'keyed rows must keep their node identity')
  assert.equal(document.activeElement, link, 'focus must survive a data-only update')
  assert.equal(list.children[1].children[0].getAttribute('aria-current'), 'page')
  page.close()
})

test('an active delivery outside the loaded window still appears with its exact identity', () => {
  const { rootElement, page } = mounted(listState({
    visible: [summary(1)],
    loadedCount: 1,
  }))
  page.setActive(summary(7, { status: 'verifying', title: 'Routed delivery' }))

  const rows = findByClass(rootElement, 'wwc-strongflow-delivery-list').children
  assert.equal(rows.length, 2)
  assert.equal(rows[1].children[0].textContent, 'Routed delivery')
  assert.equal(rows[1].children[0].getAttribute('aria-current'), 'page')
  page.close()
})

test('close unsubscribes and stops further model interaction', () => {
  const { model, page } = mounted(listState())
  page.close()
  model.publish(listState({ visible: [summary(1)], loadedCount: 1 }))
  assert.deepEqual(model.calls, [], 'a closed list page must not touch its model')
})
