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
  findByClassName,
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

// The windowed list ships as the shared large-list facility.  Its canonical
// budget (design page 04/06 rows at one 60px row height) stays fixed even
// though the community client no longer mounts a delivery workbench.
const rowHeightPx = 60
const viewportRows = 24
const overscanRows = 6
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
    rowHeight: rowHeightPx,
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
    `${String((1_000 - renderedRows) * rowHeightPx)}px`,
  )
  assert.equal(
    scroller.style.getPropertyValue('--wwc-window-row-height'),
    `${String(rowHeightPx)}px`,
  )

  scroller.scrollTop = 900 * rowHeightPx
  view.refresh()
  assert.equal(content.children.length, renderedRows)
  assert.equal(
    scroller.children[0].style.getPropertyValue('--wwc-window-spacer-height'),
    `${String((900 - overscanRows) * rowHeightPx)}px`,
  )
  // The window only pins to the tail once the scroll position enters the last
  // window-sized block, so the hidden tail here is still spacer-sized.
  const hiddenTailRows = 1_000 - (900 - overscanRows) - renderedRows
  assert.equal(
    scroller.children[2].style.getPropertyValue('--wwc-window-spacer-height'),
    `${String(hiddenTailRows * rowHeightPx)}px`,
  )

  assert.equal(view.reveal('row-missing'), false)
  assert.equal(view.reveal('row-0'), true)
  assert.equal(scroller.scrollTop, 0)
  view.close()
  assert.deepEqual(content.children, [])
})

test('scrolling swaps the window without growing the DOM', () => {
  const document = new Ui605Document()
  const view = makeWindowView(document)
  const scroller = view.root
  const content = view.content
  view.update(Array.from({ length: LARGE_DATA_CORPUS.deliveries }, (_, index) => `row-${String(index)}`))
  const nodesAtMount = content.children.length
  assert.equal(nodesAtMount, renderedRows)

  scroller.scrollTop = 2_400 * rowHeightPx
  view.refresh()
  assert.equal(content.children.length, renderedRows)
  assert.equal(
    scroller.children[0].style.getPropertyValue('--wwc-window-spacer-height'),
    `${String((2_400 - overscanRows) * rowHeightPx)}px`,
  )

  scroller.scrollTop = 0
  view.refresh()
  assert.equal(content.children.length, renderedRows)
  assert.equal(
    scroller.children[0].style.getPropertyValue('--wwc-window-spacer-height'),
    '0px',
  )
  view.close()
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
  assert.equal(rowHeightPx, 60)
})

test('the reveal seam scrolls a deep-linked row into the window', () => {
  const document = new Ui605Document()
  const view = makeWindowView(document)
  const scroller = view.root
  const rows = Array.from({ length: 5_000 }, (_, index) => `row-${String(index)}`)
  view.update(rows)

  assert.equal(view.reveal('row-4500'), true)
  assert.ok(scroller.scrollTop > 0, 'the window must scroll to reveal the deep link')
  const windowed = view.window()
  assert.ok(
    windowed.start <= 4_500 && 4_500 < windowed.end,
    `the deep-linked row must render inside the window: ${JSON.stringify(windowed)}`,
  )
  view.close()
})
