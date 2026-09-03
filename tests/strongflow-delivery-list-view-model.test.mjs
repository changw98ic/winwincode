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
  `StrongFlow delivery list did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const module = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-delivery-list-view-model.js',
)).href}`)
const { createStrongFlowDeliveryListViewModel } = module

const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
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
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
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

class FakeClient {
  constructor() {
    this.queries = []
    this.commands = []
    this.handlers = new Map()
    this.commandHandlers = new Map()
  }

  onQuery(name, handler) {
    this.handlers.set(name, handler)
    return this
  }

  onCommand(name, handler) {
    this.commandHandlers.set(name, handler)
    return this
  }

  async query(request, options) {
    this.queries.push({ request: structuredClone(request) })
    const handler = this.handlers.get(request.query)
    if (handler === undefined) throw new Error(`unexpected query ${request.query}`)
    return handler(request, options)
  }

  async command(command, options) {
    this.commands.push({ command: structuredClone(command) })
    const handler = this.commandHandlers.get(command.command)
    if (handler === undefined) throw new Error(`unexpected command ${command.command}`)
    return handler(command, options)
  }

  subscribe() {
    throw new Error('The delivery list view model must not open subscriptions.')
  }

  close() {}
}

function listPage(items, page = { hasMore: false, nextCursor: null }) {
  return {
    schemaVersion: 'winwincode/v1',
    requestId: 'req_test',
    query: 'delivery.list',
    page,
    result: { kind: 'delivery_page', items },
  }
}

function deferred() {
  let resolvePromise
  let rejectPromise
  const promise = new Promise((resolveValue, rejectValue) => {
    resolvePromise = resolveValue
    rejectPromise = rejectValue
  })
  return { promise, resolve: resolvePromise, reject: rejectPromise }
}

function view(client, options = {}) {
  let requestSequence = 0
  const model = createStrongFlowDeliveryListViewModel({
    client,
    actor,
    scope,
    nextRequestId() {
      requestSequence += 1
      return `req_${String(requestSequence).padStart(26, '0')}`
    },
    ...options,
  })
  return { model }
}

test('initial load issues one exact repository-scoped first page and publishes the visible list', async () => {
  const client = new FakeClient().onQuery('delivery.list', () => listPage([
    summary(2, { updatedAt: '2026-01-02T00:00:00Z' }),
    summary(1, { updatedAt: '2026-01-05T00:00:00Z' }),
  ]))
  const { model } = view(client)
  await model.start()

  assert.equal(model.state.status, 'ready')
  assert.equal(client.queries.length, 1)
  assert.deepEqual(client.queries[0].request, {
    schemaVersion: 'winwincode/v1',
    requestId: 'req_00000000000000000000000001',
    actor,
    scope,
    query: 'delivery.list',
    parameters: { states: [] },
    page: { cursor: null, limit: 50 },
  })
  assert.equal(model.state.hasMore, false)
  assert.equal(model.state.loadedCount, 2)
  assert.deepEqual(
    model.state.visible.map(item => item.deliveryId),
    [
      'dlv_00000000000000000000000001',
      'dlv_00000000000000000000000002',
    ],
  )
  assert.equal(model.state.error, null)
  assert.equal(model.state.moreFailure, null)
  model.close()
})

test('recent ordering is the default and broken ties stay deterministic', async () => {
  const client = new FakeClient().onQuery('delivery.list', () => listPage([
    summary(1, { updatedAt: '2026-01-05T00:00:00Z' }),
    summary(2, { updatedAt: '2026-01-05T00:00:00Z' }),
    summary(3, { updatedAt: '2026-01-01T00:00:00Z' }),
  ]))
  const { model } = view(client)
  await model.start()
  assert.deepEqual(
    model.state.visible.map(item => item.deliveryId),
    [
      'dlv_00000000000000000000000002',
      'dlv_00000000000000000000000001',
      'dlv_00000000000000000000000003',
    ],
  )
  model.close()
})

test('load more continues with the verbatim server cursor and appends the page', async () => {
  const cursor = 'opaque-cursor-1'
  let firstRequest = true
  const client = new FakeClient().onQuery('delivery.list', request => {
    if (firstRequest) {
      firstRequest = false
      assert.equal(request.page.cursor, null)
      return listPage([summary(1)], { hasMore: true, nextCursor: cursor })
    }
    assert.equal(request.page.cursor, cursor, 'the client must send the server cursor verbatim')
    return listPage([summary(2, { updatedAt: '2026-02-01T00:00:00Z' })], {
      hasMore: false,
      nextCursor: null,
    })
  })
  const { model } = view(client)
  await model.start()
  assert.equal(model.state.hasMore, true)

  await model.loadMore()

  assert.equal(client.queries.length, 2)
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.loadedCount, 2)
  assert.equal(model.state.hasMore, false)
  assert.deepEqual(
    model.state.visible.map(item => item.deliveryId),
    [
      'dlv_00000000000000000000000002',
      'dlv_00000000000000000000000001',
    ],
  )
  model.close()
})

test('load more is refused without a server cursor and concurrent calls share one flight', async () => {
  const pending = deferred()
  let calls = 0
  const client = new FakeClient().onQuery('delivery.list', () => {
    calls += 1
    return pending.promise
  })
  const { model } = view(client)
  const started = model.start()
  pending.resolve(listPage([summary(1)]))
  await started
  assert.equal(calls, 1)
  assert.equal(model.state.hasMore, false)

  await model.loadMore()
  assert.equal(calls, 1, 'no cursor means nothing further to load')
  model.close()

  const finished = deferred()
  const cursor = 'opaque-cursor-2'
  let serverCalls = 0
  client.onQuery('delivery.list', () => {
    serverCalls += 1
    return serverCalls === 1
      ? listPage([summary(1)], { hasMore: true, nextCursor: cursor })
      : finished.promise
  })
  const secondModel = view(client).model
  await secondModel.start()
  assert.equal(secondModel.state.hasMore, true)

  const first = secondModel.loadMore()
  const second = secondModel.loadMore()
  finished.resolve(listPage([summary(2)], { hasMore: false, nextCursor: null }))
  await Promise.all([first, second])

  assert.equal(serverCalls, 2, 'concurrent load-more calls must share one request')
  assert.equal(secondModel.state.loadedCount, 2)
  secondModel.close()
})

test('a failed load more keeps the loaded list and retries the same continuation', async () => {
  const cursor = 'opaque-cursor-1'
  let firstRequest = true
  let failNext = true
  const client = new FakeClient().onQuery('delivery.list', () => {
    if (firstRequest) {
      firstRequest = false
      return listPage([summary(1)], { hasMore: true, nextCursor: cursor })
    }
    if (failNext) {
      failNext = false
      throw Object.assign(new Error('boom'), {
        kind: 'network',
        code: 'NETWORK_ERROR',
        requestId: 'req_failed',
        retryable: true,
      })
    }
    return listPage([summary(2)], { hasMore: false, nextCursor: null })
  })
  const { model } = view(client)
  await model.start()
  const loadedBefore = model.state.visible.length

  await model.loadMore()
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.visible.length, loadedBefore, 'failure must not drop loaded items')
  assert.equal(model.state.loadingMore, false)
  assert.equal(model.state.hasMore, true)
  assert.equal(model.state.moreFailure?.code, 'NETWORK_ERROR')

  await model.loadMore()
  assert.equal(model.state.moreFailure, null)
  assert.equal(model.state.loadedCount, 2)
  model.close()
})

test('load more of a loading list waits for the initial load to finish', async () => {
  const pending = deferred()
  let calls = 0
  const client = new FakeClient().onQuery('delivery.list', () => {
    calls += 1
    return calls === 1 ? pending.promise : listPage([summary(9)])
  })
  const { model } = view(client)
  const started = model.start()
  const more = model.loadMore()
  pending.resolve(listPage([summary(1)], { hasMore: true, nextCursor: 'opaque-cursor-3' }))
  await Promise.all([started, more])

  assert.equal(calls, 2)
  assert.equal(model.state.loadedCount, 2)
  model.close()
})

test('close stops publication and further work', async () => {
  const client = new FakeClient().onQuery('delivery.list', () => listPage([summary(1)]))
  const { model } = view(client)
  await model.start()
  let publications = 0
  model.subscribe(() => {
    publications += 1
  })
  model.close()
  const before = publications
  await model.loadMore()
  await model.refresh()
  assert.equal(publications, before, 'a closed list must not publish again')
})

test('a READ_CURSOR_EXPIRED continuation fails closed and a fresh refresh restarts at the first page', async () => {
  const cursor = 'opaque-cursor-1'
  let requests = 0
  let expired = false
  const client = new FakeClient().onQuery('delivery.list', request => {
    requests += 1
    if (requests === 1) {
      return listPage([summary(1)], { hasMore: true, nextCursor: cursor })
    }
    if (expired) {
      assert.equal(request.page.cursor, null, 'the refresh must restart without the dead cursor')
      return listPage([
        summary(1, { revision: 2 }),
        summary(3),
      ], { hasMore: false, nextCursor: null })
    }
    expired = true
    throw Object.assign(new Error('stale'), {
      kind: 'server',
      code: 'READ_CURSOR_EXPIRED',
      requestId: 'req_stale',
      retryable: false,
    })
  })
  const { model } = view(client)
  await model.start()
  assert.equal(model.state.hasMore, true)

  await model.loadMore()
  assert.equal(model.state.moreFailure?.code, 'READ_CURSOR_EXPIRED')
  assert.equal(model.state.loadedCount, 1, 'the loaded page survives the dead continuation')
  assert.equal(model.state.hasMore, true)

  await model.refresh()
  assert.equal(model.state.moreFailure, null)
  assert.equal(model.state.error, null)
  assert.equal(model.state.loadedCount, 2)
  assert.equal(
    model.state.visible.find(item => item.deliveryId === 'dlv_00000000000000000000000001')?.revision,
    2,
  )
  model.close()
})

test('a permission failure on first load fails closed with an explicit denied state', async () => {
  const client = new FakeClient().onQuery('delivery.list', () => {
    throw Object.assign(new Error('denied'), {
      kind: 'authorization',
      code: 'PERMISSION_DENIED',
      requestId: 'req_denied',
      retryable: false,
    })
  })
  const { model } = view(client)
  await model.start()
  assert.equal(model.state.status, 'error')
  assert.equal(model.state.loadedCount, 0)
  assert.equal(model.state.error?.kind, 'authorization')
  assert.equal(model.state.error?.code, 'PERMISSION_DENIED')
  assert.equal(model.state.visible.length, 0)
  model.close()
})

test('the status filter changes the server states parameter and rebuilds from the first page', async () => {
  const requests = []
  const client = new FakeClient().onQuery('delivery.list', request => {
    requests.push({ states: [...request.parameters.states], cursor: request.page.cursor })
    if (requests.length === 1) {
      return listPage([summary(1)], { hasMore: true, nextCursor: 'opaque-cursor-9' })
    }
    return listPage([summary(2, { status: 'executing' })], {
      hasMore: false,
      nextCursor: null,
    })
  })
  const { model } = view(client)
  await model.start()
  assert.equal(model.state.hasMore, true)

  await model.setStatusFilter('executing')

  assert.deepEqual(requests, [
    { states: [], cursor: null },
    { states: ['executing'], cursor: null },
  ])
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.loadedCount, 1)
  assert.equal(model.state.filters.status, 'executing')
  assert.equal(model.state.hasMore, false)
  model.close()
})

test('search, attention, and order are client-side projections and never query the server', async () => {
  const client = new FakeClient().onQuery('delivery.list', () => listPage([
    summary(1, {
      title: 'alpha service',
      openAttentionCount: 2,
      updatedAt: '2026-01-05T00:00:00Z',
    }),
    summary(2, {
      title: 'beta service',
      openAttentionCount: 0,
      updatedAt: '2026-01-04T00:00:00Z',
    }),
    summary(3, {
      title: 'gamma service',
      openAttentionCount: 0,
      updatedAt: '2026-01-03T00:00:00Z',
    }),
  ]))
  const { model } = view(client)
  await model.start()
  assert.equal(client.queries.length, 1)

  model.setSearch('SERVICE')
  assert.equal(client.queries.length, 1)
  assert.deepEqual(model.state.visible.map(item => item.deliveryId), [
    'dlv_00000000000000000000000001',
    'dlv_00000000000000000000000002',
    'dlv_00000000000000000000000003',
  ])

  model.setAttentionOnly(true)
  assert.equal(client.queries.length, 1)
  assert.deepEqual(model.state.visible.map(item => item.deliveryId), [
    'dlv_00000000000000000000000001',
  ])

  model.setAttentionOnly(false)
  model.setSearch('')
  assert.equal(client.queries.length, 1)

  model.setOrder('title')
  assert.equal(client.queries.length, 1)
  assert.deepEqual(model.state.visible.map(item => item.deliveryId), [
    'dlv_00000000000000000000000001',
    'dlv_00000000000000000000000002',
    'dlv_00000000000000000000000003',
  ])
  assert.equal(model.state.loadedCount, 3)
  model.close()
})

test('refresh swaps in one complete snapshot and keeps the old window when the rebuild fails', async () => {
  const cursor = 'opaque-cursor-1'
  let requests = 0
  const inFlight = deferred()
  let released = false
  const client = new FakeClient().onQuery('delivery.list', () => {
    requests += 1
    if (requests === 1) {
      return listPage([summary(1)], { hasMore: true, nextCursor: cursor })
    }
    if (requests === 2) {
      return listPage([summary(2)], { hasMore: true, nextCursor: 'opaque-cursor-2' })
    }
    if (requests === 3) {
      return listPage([summary(3)], { hasMore: false, nextCursor: null })
    }
    if (requests === 4) {
      return inFlight.promise.then(() => {
        released = true
        throw Object.assign(new Error('offline'), {
          kind: 'network',
          code: 'NETWORK_ERROR',
          requestId: 'req_offline',
          retryable: true,
        })
      })
    }
    if (requests === 5) {
      assert.equal(released, true)
      return listPage([summary(1, { revision: 7 })], { hasMore: false, nextCursor: null })
    }
    throw new Error('unexpected request')
  })
  const { model } = view(client)
  await model.start()
  await model.loadMore()
  await model.loadMore()
  assert.equal(model.state.loadedCount, 3)

  const refreshing = model.refresh()
  assert.equal(model.state.status, 'refreshing')
  assert.equal(model.state.loadedCount, 3, 'the old window stays visible while rebuilding')
  inFlight.resolve(undefined)
  await refreshing

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.error?.code, 'NETWORK_ERROR')
  assert.equal(model.state.loadedCount, 3, 'a failed refresh keeps the previous snapshot')

  await model.refresh()
  assert.equal(model.state.error, null)
  assert.equal(model.state.loadedCount, 1)
  assert.equal(model.state.visible[0]?.revision, 7)
  model.close()
})

test('a refresh that the server keeps extending beyond the page bound fails closed', async () => {
  let requests = 0
  let expectedCursor = null
  const client = new FakeClient().onQuery('delivery.list', request => {
    requests += 1
    if (requests === 5) expectedCursor = null
    assert.equal(request.page.cursor, expectedCursor, 'only server cursors are ever sent')
    if (requests === 1) {
      expectedCursor = 'opaque-cursor-1'
      return listPage([summary(1)], { hasMore: true, nextCursor: expectedCursor })
    }
    const nextCursor = `opaque-cursor-${String(requests)}`
    expectedCursor = nextCursor
    return listPage([summary(requests)], { hasMore: true, nextCursor })
  })
  const { model } = view(client, { maxPages: 2 })
  await model.start()
  await model.loadMore()
  await model.loadMore()
  await model.loadMore()
  assert.equal(model.state.loadedCount, 4)
  assert.equal(model.state.hasMore, true)

  await model.refresh()
  assert.equal(model.state.error?.code, 'STRONGFLOW_DELIVERY_LIST_PAGE_LIMIT')
  assert.equal(model.state.loadedCount, 4, 'the bound failure keeps the loaded window')
  model.close()
})

test('advance sends the exact delivery.advance command and applies the returned projection', async () => {
  const client = new FakeClient()
    .onQuery('delivery.list', () => listPage([
      summary(1, { status: 'ready-to-deliver', revision: 4 }),
    ]))
    .onCommand('delivery.advance', command => {
      assert.equal(command.command, 'delivery.advance')
      assert.equal(command.expectedRevision, 4)
      assert.deepEqual(command.payload, { deliveryId: 'dlv_00000000000000000000000001' })
      assert.equal(command.scope, scope)
      return {
        schemaVersion: 'winwincode/v1',
        requestId: command.requestId,
        command: 'delivery.advance',
        outcome: 'completed',
        previousRevision: 4,
        currentRevision: 5,
        result: summary(1, { status: 'delivered', revision: 5 }),
      }
    })
  const { model } = view(client)
  await model.start()

  await model.advanceDelivery('dlv_00000000000000000000000001', 4)

  assert.equal(client.commands.length, 1)
  assert.equal(model.state.advance.deliveryId, null)
  assert.equal(model.state.advance.failure, null)
  const advanced = model.state.visible.find(item => item.deliveryId === 'dlv_00000000000000000000000001')
  assert.equal(advanced?.status, 'delivered')
  assert.equal(advanced?.revision, 5)
  model.close()
})

test('an accepted advance keeps calm and rebuilds for the authoritative projection', async () => {
  let queries = 0
  const client = new FakeClient()
    .onQuery('delivery.list', () => {
      queries += 1
      return queries === 1
        ? listPage([summary(1, { status: 'ready-to-deliver', revision: 4 })])
        : listPage([summary(1, { status: 'delivered', revision: 5 })])
    })
    .onCommand('delivery.advance', command => ({
      schemaVersion: 'winwincode/v1',
      requestId: command.requestId,
      command: 'delivery.advance',
      outcome: 'accepted',
    }))
  const { model } = view(client)
  await model.start()

  await model.advanceDelivery('dlv_00000000000000000000000001', 4)

  assert.equal(model.state.advance.deliveryId, null)
  assert.equal(model.state.advance.failure, null)
  await model.refresh()
  assert.equal(queries >= 2, true, 'the accepted advance schedules a rebuild')
  assert.equal(model.state.visible[0]?.status, 'delivered')
  model.close()
})

test('a rejected advance fails closed and leaves the card in its server column', async () => {
  const client = new FakeClient()
    .onQuery('delivery.list', () => listPage([
      summary(1, { status: 'draft', revision: 1 }),
    ]))
    .onCommand('delivery.advance', () => {
      throw Object.assign(new Error('illegal transition'), {
        kind: 'server',
        code: 'WRONG_STATE',
        requestId: 'req_wrong_state',
        retryable: false,
      })
    })
  const { model } = view(client)
  await model.start()

  await model.advanceDelivery('dlv_00000000000000000000000001', 1)

  assert.equal(model.state.advance.deliveryId, 'dlv_00000000000000000000000001')
  assert.equal(model.state.advance.failure?.code, 'WRONG_STATE')
  assert.equal(model.state.visible[0]?.status, 'draft', 'the card never moves on rejection')
  model.close()
})

test('a superseded rebuild never publishes an older generation over a newer one', async () => {
  const firstPage = deferred()
  const secondPage = deferred()
  let requests = 0
  const client = new FakeClient().onQuery('delivery.list', () => {
    requests += 1
    if (requests === 1) return firstPage.promise
    if (requests === 2) return secondPage.promise
    return listPage([summary(2)], { hasMore: false, nextCursor: null })
  })
  const { model } = view(client)
  const started = model.start()
  await new Promise(resolveTick => setImmediate(resolveTick))
  assert.equal(client.queries.length, 1, 'the first rebuild is in flight')
  const refreshed = model.refresh()
  firstPage.resolve(listPage([summary(9)], { hasMore: true, nextCursor: 'opaque-cursor-0' }))
  await started
  secondPage.resolve(listPage([summary(1)], { hasMore: false, nextCursor: null }))
  await refreshed

  assert.equal(client.queries.length, 2, 'only the surviving generation reaches the server')
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.loadedCount, 1)
  assert.deepEqual(model.state.visible.map(item => item.deliveryId), [
    'dlv_00000000000000000000000001',
  ])
  model.close()
})
