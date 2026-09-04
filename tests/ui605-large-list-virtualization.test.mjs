// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

import {
  LARGE_DATA_CORPUS,
  LARGE_DATA_PERFORMANCE_BASELINE,
  Ui605Document,
  deliveryIdFor,
  findByClass,
  findByClassName,
  findAllByClass,
  largeChangedFiles,
  largeDeliverySummaries,
  largeDeliveryTasks,
  largeEvidenceRows,
  largeLogText,
  largeRuntimeActivities,
  largeStageRuns,
  largeUnifiedDiff,
  treeNodeCount,
} from './fixtures/ui605-large-data.mjs'

const root = resolve(import.meta.dirname, '..')
const targetRoot = resolve(process.env.UI605_TARGET_ROOT ?? root)
const outputRoot = resolve(targetRoot, '.cache/ui605-large-data-tests')

const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.ui605-large-data-tests.json',
    '--pretty',
    'false',
    '--incremental',
    'false',
  ],
  { cwd: targetRoot, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `UI-605 modules did not compile in ${targetRoot}:\n${compiler.stdout}${compiler.stderr}`,
)

const imported = specifier => import(
  `${pathToFileURL(join(outputRoot, specifier)).href}?ui605=${String(Date.now())}`
)

const { windowBounds, mountWindowedList } = await imported('components/windowed-list.js')
const {
  boundedItems,
  DEFAULT_STRONGFLOW_RENDER_LIMITS,
  STRONGFLOW_WINDOW_OVERSCAN_ROWS,
  STRONGFLOW_WINDOW_ROW_HEIGHT_PX,
  STRONGFLOW_WINDOW_VIEWPORT_ROWS,
} = await imported('strongflow-rendering.js')
const { mountStrongFlowDeliveryList } = await imported('strongflow-delivery-list-page.js')
const { mountCandidateDiffViewer } = await imported('strongflow-diff-viewer.js')

const viewportRows = STRONGFLOW_WINDOW_VIEWPORT_ROWS
const overscanRows = STRONGFLOW_WINDOW_OVERSCAN_ROWS
const renderedRows = viewportRows + 2 * overscanRows

function assertWindow(bounds, start, end, total) {
  assert.deepEqual({ ...bounds }, { start, end, total })
}

test('window bounds keep a fixed DOM budget and never lose records', () => {
  const total = LARGE_DATA_CORPUS.deliveries
  assertWindow(windowBounds(0, viewportRows, 0, overscanRows), 0, 0, 0)
  assertWindow(windowBounds(1, viewportRows, 0, overscanRows), 0, 1, 1)
  assertWindow(windowBounds(total, viewportRows, 0, overscanRows), 0, renderedRows, total)
  assert.ok(
    renderedRows <= LARGE_DATA_PERFORMANCE_BASELINE.rendered.listRows,
    'the window cap must stay within the recorded baseline',
  )

  assertWindow(windowBounds(total, viewportRows, 2_400, overscanRows), 2_394, 2_430, total)

  const tail = windowBounds(total, viewportRows, total - 1, overscanRows)
  assert.equal(tail.end, total)
  assert.equal(tail.end - tail.start, renderedRows, 'the window never shrinks at the tail')

  assert.equal(windowBounds(total, viewportRows, -50, overscanRows).start, 0)
  assert.equal(windowBounds(total, viewportRows, total + 500, overscanRows).start, total - renderedRows)
})

test('window bounds reject budgets that would silently truncate or leak records', () => {
  assert.throws(() => windowBounds(-1, viewportRows, 0, overscanRows), RangeError)
  assert.throws(() => windowBounds(10, 0, 0, overscanRows), RangeError)
  assert.throws(() => windowBounds(10, viewportRows, 0, -1), RangeError)
  assert.throws(() => windowBounds(10, 1.5, 0, overscanRows), RangeError)
})

function windowState(items, overrides = {}) {
  return {
    status: 'ready',
    filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
    visible: items,
    loadedCount: items.length,
    hasMore: false,
    loadingMore: false,
    moreFailure: null,
    error: null,
    advance: { deliveryId: null, failure: null },
    ...overrides,
  }
}

class WindowedRecorderModel {
  constructor(state) { this.state = state }

  listener = null
  calls = []

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }

  async start() {}
  async refresh() { this.calls.push(['refresh']) }
  async loadMore() { this.calls.push(['loadMore']) }
  setSearch(value) { this.calls.push(['setSearch', value]) }
  async setStatusFilter(value) { this.calls.push(['setStatusFilter', value]) }
  setAttentionOnly(value) { this.calls.push(['setAttentionOnly', value]) }
  setOrder(value) { this.calls.push(['setOrder', value]) }
  async advanceDelivery() {}
  close() {}
}

function mountLargeList(count = LARGE_DATA_CORPUS.deliveries, overrides = {}, view = 'list') {
  const document = new Ui605Document()
  const rootElement = document.createElement('main')
  const model = new WindowedRecorderModel(windowState(largeDeliverySummaries(count), overrides))
  const page = mountStrongFlowDeliveryList({ root: rootElement, model, view })
  return { document, rootElement, model, page }
}

const listRows = rootElement => findAllByClass(rootElement, 'wwc-strongflow-delivery-list')[0]
const capacityNote = rootElement => findByClass(rootElement, 'wwc-delivery-loaded-note')

test('a 5 000 Delivery corpus renders one bounded window, not the corpus', () => {
  const { rootElement, page } = mountLargeList()
  const rows = listRows(rootElement).children

  assert.equal(rows.length, renderedRows)
  assert.ok(rows.length < LARGE_DATA_CORPUS.deliveries, 'every loaded Delivery must not render')
  assert.ok(
    treeNodeCount(rootElement) <= LARGE_DATA_PERFORMANCE_BASELINE.rendered.pageDomNodes,
    `the mounted page allocated ${String(treeNodeCount(rootElement))} DOM nodes`,
  )
  assert.equal(rows[0].children[0].textContent, 'Enterprise delivery 1 — kernel workstream')
  assert.equal(rows[0].children[0].dataset.deliveryId, deliveryIdFor(1))
  assert.equal(rows[renderedRows - 1].children[0].dataset.deliveryId, deliveryIdFor(renderedRows))
  page.close()
  assert.deepEqual(listRows(rootElement).children, [])
})

test('the capacity note reports rendered, matched, loaded, omitted and continue states', () => {
  const { rootElement, model, page } = mountLargeList(200, { hasMore: true })
  const note = capacityNote(rootElement)

  assert.match(note.textContent, /Rendered 36 of 200 matching deliveries/u)
  assert.match(note.textContent, /200 loaded/u)
  assert.match(note.textContent, /164 loaded Deliveries are not rendered in this window/u)
  assert.match(note.textContent, /more Deliveries are available on the server/u)
  assert.equal(findByClass(rootElement, 'wwc-delivery-load-more').hidden, false)

  model.publish(windowState(largeDeliverySummaries(200), { hasMore: false }))
  assert.match(note.textContent, /every loaded Delivery is already on the client/u)
  assert.equal(findByClass(rootElement, 'wwc-delivery-load-more').hidden, true)
  page.close()
})

test('scrolling swaps the window without growing the DOM', () => {
  const { document, rootElement, page } = mountLargeList()
  const scroller = findByClassName(rootElement, 'wwc-window-scroller')
  const nodesAtMount = treeNodeCount(rootElement)
  const listenersAtMount = document.listenerCount()

  scroller.scrollTop = 2_400 * STRONGFLOW_WINDOW_ROW_HEIGHT_PX
  scroller.emit('scroll')

  const rows = listRows(rootElement).children
  assert.equal(rows.length, renderedRows)
  assert.equal(rows[0].children[0].dataset.deliveryId, deliveryIdFor(2_401 - overscanRows))
  assert.equal(treeNodeCount(rootElement), nodesAtMount, 'scrolling allocated nodes')
  assert.equal(document.listenerCount(), listenersAtMount, 'scrolling added listeners')

  scroller.scrollTop = 0
  scroller.emit('scroll')
  assert.equal(listRows(rootElement).children[0].children[0].dataset.deliveryId, deliveryIdFor(1))
  assert.equal(treeNodeCount(rootElement), nodesAtMount)
  page.close()
})

test('equivalent snapshots reuse row identity and allocate nothing', () => {
  const { rootElement, page } = mountLargeList()
  const first = listRows(rootElement).children[0]
  const nodesAtMount = treeNodeCount(rootElement)

  for (let round = 0; round < 25; round += 1) page.setActive(null)

  assert.ok(listRows(rootElement).children[0] === first, 'row identity changed')
  assert.equal(treeNodeCount(rootElement), nodesAtMount, 'equivalent snapshots allocated nodes')
  page.close()
})

test('search narrows the corpus and the window follows the filtered order', () => {
  const { rootElement, model, page } = mountLargeList()
  const search = findByClass(rootElement, 'wwc-delivery-search')
  search.value = 'control plane'
  search.emit('input')
  assert.deepEqual(model.calls.at(-1), ['setSearch', 'control plane'])

  const matches = largeDeliverySummaries().filter(row => row.title.includes('control plane'))
  model.publish(windowState(matches, { loadedCount: LARGE_DATA_CORPUS.deliveries }))
  const rows = listRows(rootElement).children
  assert.equal(rows.length, renderedRows)
  assert.ok(rows[0].children[0].textContent.includes('control plane'))
  assert.match(
    capacityNote(rootElement).textContent,
    new RegExp(`Rendered ${String(renderedRows)} of ${String(matches.length)} matching`, 'u'),
  )
  page.close()
})

test('the status filter narrows the corpus without hiding the loaded count', () => {
  const { rootElement, model, page } = mountLargeList()
  const statusFilter = findByClass(rootElement, 'wwc-delivery-status-filter')
  statusFilter.value = 'executing'
  statusFilter.emit('change')
  assert.deepEqual(model.calls.at(-1), ['setStatusFilter', 'executing'])

  const matches = largeDeliverySummaries().filter(row => row.status === 'executing')
  model.publish(windowState(matches, {
    loadedCount: LARGE_DATA_CORPUS.deliveries,
    filters: { search: '', status: 'executing', attentionOnly: false, order: 'recent' },
  }))
  const rows = listRows(rootElement).children
  assert.equal(rows.length, renderedRows)
  assert.match(rows[0].children[1].textContent, /executing/u)
  const note = capacityNote(rootElement).textContent
  assert.match(note, new RegExp(`of ${String(matches.length)} matching`, 'u'))
  assert.match(note, new RegExp(`${String(LARGE_DATA_CORPUS.deliveries)} loaded`, 'u'))
  page.close()
})

test('a deep link to a Delivery outside the window reveals its row', () => {
  const { rootElement, page } = mountLargeList()
  const scroller = findByClassName(rootElement, 'wwc-window-scroller')
  const deepTarget = 4_500

  page.setActive({
    deliveryId: deliveryIdFor(deepTarget),
    revision: 1,
    status: 'executing',
    title: `Enterprise delivery ${String(deepTarget)} — client workstream`,
    openAttentionCount: 0,
  })

  const rows = listRows(rootElement).children
  const target = rows.find(row => row.children[0].dataset.deliveryId === deliveryIdFor(deepTarget))
  assert.notEqual(target, undefined, 'the deep-linked Delivery must render inside the window')
  assert.ok(scroller.scrollTop > 0, 'the window must scroll to reveal the deep link')
  assert.equal(target.children[0].getAttribute('aria-current'), 'page')
  page.close()
})

test('the deep-linked selection survives scrolling away and coming back', () => {
  const { rootElement, page } = mountLargeList()
  const scroller = findByClassName(rootElement, 'wwc-window-scroller')
  const target = deliveryIdFor(2_000)

  page.setActive({
    deliveryId: target,
    revision: 1,
    status: 'executing',
    title: 'Enterprise delivery 2000 — client workstream',
    openAttentionCount: 0,
  })
  assert.notEqual(
    listRows(rootElement).children.find(row => row.children[0].dataset.deliveryId === target),
    undefined,
  )

  // Scrolling is user work: the window follows the viewport and the deep-linked
  // row legitimately leaves the DOM instead of being pinned.
  scroller.scrollTop = 3_000 * STRONGFLOW_WINDOW_ROW_HEIGHT_PX
  scroller.emit('scroll')
  assert.equal(
    listRows(rootElement).children.find(row => row.children[0].dataset.deliveryId === target),
    undefined,
  )

  // Returning to it re-renders the same node with its selection marker intact.
  scroller.scrollTop = 2_000 * STRONGFLOW_WINDOW_ROW_HEIGHT_PX
  scroller.emit('scroll')
  const back = listRows(rootElement).children.find(
    row => row.children[0].dataset.deliveryId === target,
  )
  assert.notEqual(back, undefined)
  assert.equal(back.children[0].getAttribute('aria-current'), 'page')

  // A different deep link re-reveals its own row.
  page.setActive({
    deliveryId: deliveryIdFor(900),
    revision: 1,
    status: 'ready',
    title: 'Enterprise delivery 900 — kernel workstream',
    openAttentionCount: 0,
  })
  assert.notEqual(
    listRows(rootElement).children.find(
      row => row.children[0].dataset.deliveryId === deliveryIdFor(900),
    ),
    undefined,
  )
  page.close()
})

test('reopening the same deep link reveals it again after the selection closes', () => {
  const { rootElement, page } = mountLargeList()
  const scroller = findByClassName(rootElement, 'wwc-window-scroller')
  const target = deliveryIdFor(4_500)
  const row = () => ({
    deliveryId: target,
    revision: 1,
    status: 'executing',
    title: `Enterprise delivery 4500 — client workstream`,
    openAttentionCount: 0,
  })

  page.setActive(row())
  assert.notEqual(
    listRows(rootElement).children.find(item => item.children[0].dataset.deliveryId === target),
    undefined,
  )

  page.setActive(null)
  scroller.scrollTop = 0
  scroller.emit('scroll')
  page.setActive(row())

  assert.notEqual(
    listRows(rootElement).children.find(item => item.children[0].dataset.deliveryId === target),
    undefined,
    'reopening the same deep link was swallowed by the earlier reveal',
  )
  assert.ok(scroller.scrollTop > 0, 'the window did not scroll back to the deep link')
  page.close()
})

test('the kanban view stays bounded and reports its omitted cards', () => {
  const { rootElement, page } = mountLargeList(LARGE_DATA_CORPUS.deliveries, {}, 'kanban')
  const columns = findAllByClass(rootElement, 'wwc-delivery-kanban-column')
  const perColumn = DEFAULT_STRONGFLOW_RENDER_LIMITS.deliveries

  assert.ok(columns.length > 1)
  for (const column of columns) {
    const cards = column.children[1].children
    assert.ok(
      cards.length <= perColumn,
      `a kanban column rendered ${String(cards.length)} cards`,
    )
  }
  const note = findByClassName(rootElement, 'wwc-delivery-kanban-omitted')
  assert.equal(note.hidden, false)
  assert.match(note.textContent, /kanban cards not rendered/u)
  page.close()
})

function makeWindowView(document) {
  return mountWindowedList({
    document,
    scroller: document.createElement('div'),
    content: document.createElement('ul'),
    key: row => row,
    create: row => {
      const item = document.createElement('li')
      item.dataset.row = row
      return item
    },
    update() {},
    rowHeight: STRONGFLOW_WINDOW_ROW_HEIGHT_PX,
    viewportRows,
    overscan: overscanRows,
  })
}

test('the windowed list sizes spacers instead of rendering hidden rows', () => {
  const document = new Ui605Document()
  const view = makeWindowView(document)
  const scroller = view.root
  const content = view.content

  view.update(Array.from({ length: 1_000 }, (_, index) => `row-${String(index)}`))
  assert.equal(content.children.length, renderedRows)
  assert.deepEqual({ ...view.window() }, { start: 0, end: renderedRows, total: 1_000 })
  assert.equal(scroller.children[0].style.getPropertyValue('--wwc-window-spacer-height'), '0px')
  assert.equal(
    scroller.children[2].style.getPropertyValue('--wwc-window-spacer-height'),
    `${String((1_000 - renderedRows) * STRONGFLOW_WINDOW_ROW_HEIGHT_PX)}px`,
  )
  assert.equal(
    scroller.style.getPropertyValue('--wwc-window-row-height'),
    `${String(STRONGFLOW_WINDOW_ROW_HEIGHT_PX)}px`,
  )

  scroller.scrollTop = 900 * STRONGFLOW_WINDOW_ROW_HEIGHT_PX
  view.refresh()
  assert.equal(content.children.length, renderedRows)
  assert.equal(
    scroller.children[0].style.getPropertyValue('--wwc-window-spacer-height'),
    `${String((900 - overscanRows) * STRONGFLOW_WINDOW_ROW_HEIGHT_PX)}px`,
  )
  // The window only pins to the tail once the scroll position enters the last
  // window-sized block, so the hidden tail here is still spacer-sized.
  const hiddenTailRows = 1_000 - (900 - overscanRows) - renderedRows
  assert.equal(
    scroller.children[2].style.getPropertyValue('--wwc-window-spacer-height'),
    `${String(hiddenTailRows * STRONGFLOW_WINDOW_ROW_HEIGHT_PX)}px`,
  )

  assert.equal(view.reveal('row-missing'), false)
  assert.equal(view.reveal('row-0'), true)
  assert.equal(scroller.scrollTop, 0)
  view.close()
  assert.deepEqual(content.children, [])
})

test('the windowed list closes exactly once and releases its listeners', () => {
  const document = new Ui605Document()
  const view = makeWindowView(document)
  view.update(['a', 'b'])
  assert.ok(document.listenerCount() > 0)

  view.close()
  view.close()
  assert.equal(document.listenerCount(), 0)
  assert.deepEqual(view.content.children, [])
  assert.throws(() => view.update(['c']))
})

test('the canonical row height token and the scroll math never diverge', () => {
  const tokens = readFileSync(join(root, 'apps/client/src/styles/tokens.css'), 'utf8')
  assert.match(tokens, /--wwc-window-row-height:\s*60px;/u)
  assert.equal(STRONGFLOW_WINDOW_ROW_HEIGHT_PX, 60)

  const features = readFileSync(
    join(root, 'apps/client/src/styles/features/strongflow.css'),
    'utf8',
  )
  assert.match(
    features,
    /\.wwc-delivery-list-scroll \{[^}]*--wwc-window-viewport-rows[^}]*--wwc-window-row-height[^}]*\}/u,
    'the scroll box must be sized from the window tokens, not a magic size',
  )
  assert.match(features, /\.wwc-window-scroller > \* > \* \{[^}]*--wwc-window-row-height/u)
  assert.equal(
    DEFAULT_STRONGFLOW_RENDER_LIMITS.deliveries > 0,
    true,
    'bounded render limits stay in force alongside the window cap',
  )
})

test('a 24 000-line Diff renders a bounded window and grows only on request', () => {
  const document = new Ui605Document()
  const diffViewer = mountCandidateDiffViewer({
    document,
    rowLimit: 300,
    onLoadMoreDiff() {},
    onViewModeChange() {},
  })
  const content = largeUnifiedDiff()
  const path = 'apps/client/src/large-sample.ts'

  diffViewer.update({
    diff: {
      status: 'ready',
      path,
      content,
      loadedBytes: content.length,
      totalBytes: content.length,
      hasMore: false,
      previewLimited: false,
      fileDiffSha256: 'sha256-large',
      unavailableReason: null,
      error: null,
    },
    selectedPath: path,
    viewMode: 'unified',
    candidateDigest: 'sha256-candidate',
    selectedLine: null,
  })

  const body = findByClass(diffViewer.root, 'wwc-candidate-diff-body')
  const initialRows = body.children.length
  assert.ok(initialRows > 0)
  assert.ok(initialRows <= 300, `the Diff viewer rendered ${String(initialRows)} rows`)
  assert.ok(
    initialRows < LARGE_DATA_CORPUS.diffLines,
    'the Diff viewer must not render every line',
  )
  const renderMore = findByClass(diffViewer.root, 'wwc-candidate-diff-render-more')
  assert.equal(renderMore.hidden, false, 'a larger window must stay reachable')

  renderMore.click()
  const grown = findByClass(diffViewer.root, 'wwc-candidate-diff-body').children.length
  assert.ok(grown > initialRows, 'render more added rows')
  assert.ok(grown <= 500, `render more exceeded the Diff row cap: ${String(grown)}`)
  assert.match(
    findByClass(diffViewer.root, 'wwc-candidate-diff-status').textContent,
    /Diff lines shown/u,
  )
  diffViewer.close()
})

test('a partially streamed large Diff reports its bytes instead of waiting for all of them', () => {
  const document = new Ui605Document()
  const diffViewer = mountCandidateDiffViewer({
    document,
    rowLimit: 300,
    onLoadMoreDiff() {},
    onViewModeChange() {},
  })
  const content = largeUnifiedDiff(4_000)
  const path = 'apps/client/src/large-sample.ts'
  const loadedBytes = Math.floor(content.length / 4)

  diffViewer.update({
    diff: {
      status: 'ready',
      path,
      content: content.slice(0, loadedBytes),
      loadedBytes,
      totalBytes: content.length,
      hasMore: true,
      previewLimited: false,
      fileDiffSha256: 'sha256-large',
      unavailableReason: null,
      error: null,
    },
    selectedPath: path,
    viewMode: 'unified',
    candidateDigest: 'sha256-candidate',
    selectedLine: null,
  })

  assert.equal(
    findByClass(diffViewer.root, 'wwc-candidate-load-more-diff').hidden,
    false,
    'a partially streamed Diff must offer the next server range',
  )
  assert.match(
    findByClass(diffViewer.root, 'wwc-candidate-diff-status').textContent,
    new RegExp(
      `Showing the first ${loadedBytes.toLocaleString('en-US')} of ${
        content.length.toLocaleString('en-US')
      } Diff bytes`,
      'u',
    ),
  )
  const rendered = findByClass(diffViewer.root, 'wwc-candidate-diff-body').children.length
  assert.ok(
    rendered < LARGE_DATA_CORPUS.diffLines,
    'a partial Diff must not render the whole corpus',
  )
  diffViewer.close()
})

test('large corpus fixtures cover every UI-605 surface at enterprise size', () => {
  assert.equal(largeDeliverySummaries().length, LARGE_DATA_CORPUS.deliveries)
  assert.equal(largeDeliveryTasks().length, LARGE_DATA_CORPUS.deliveryTasks)
  assert.equal(largeStageRuns().length, LARGE_DATA_CORPUS.stageRuns)
  assert.equal(largeRuntimeActivities().length, LARGE_DATA_CORPUS.runtimeActivities)
  assert.equal(largeChangedFiles().length, LARGE_DATA_CORPUS.changedFiles)
  assert.equal(largeEvidenceRows().length, LARGE_DATA_CORPUS.evidenceRows)

  const logLines = largeLogText().split('\n').filter(line => line.length > 0)
  assert.equal(logLines.length, LARGE_DATA_CORPUS.logLines)
  assert.ok(logLines.join('\n').length > 2_000_000, 'the log fixture is genuinely large')

  const diffLines = largeUnifiedDiff().split('\n')
  assert.ok(diffLines.length >= LARGE_DATA_CORPUS.diffLines)

  for (const row of largeDeliverySummaries(3)) {
    assert.match(row.deliveryId, /^dlv_[0-9]{22}$/u)
  }
})

test('enterprise corpora exceed every shipped render cap without losing records', () => {
  const limits = DEFAULT_STRONGFLOW_RENDER_LIMITS
  for (const [label, rows, cap] of [
    ['Delivery tasks', largeDeliveryTasks(), limits.tasks],
    ['StageRuns', largeStageRuns(), limits.stages],
    ['Runtime activities', largeRuntimeActivities(), limits.activities],
    ['Evidence rows', largeEvidenceRows(), limits.evidence],
  ]) {
    assert.ok(rows.length > cap, `${label} fixture is smaller than its render cap`)
    const bounded = boundedItems(rows, cap)
    assert.equal(bounded.items.length, cap, `${label} rendered past its cap`)
    assert.equal(bounded.omitted, rows.length - cap, `${label} dropped its omitted count`)
  }
})
