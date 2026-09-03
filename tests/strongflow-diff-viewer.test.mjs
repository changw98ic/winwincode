// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync('corepack', [
  'pnpm', 'exec', 'tsc',
  '-p', 'apps/client/tsconfig.strongflow-page-tests.json',
  '--pretty', 'false',
  '--incremental', 'false',
], { cwd: root, encoding: 'utf8' })
assert.equal(
  compiler.status,
  0,
  `StrongFlow Diff viewer modules did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const modelModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-diff-model.js',
)).href}`)

const viewerModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-diff-viewer.js',
)).href}`)

const { parseCandidateDiff } = modelModule
const { mountCandidateDiffViewer } = viewerModule

const selectedFileDiff = [
  'diff --git a/src/app.ts b/src/app.ts',
  'index 1111111..2222222 100644',
  '--- a/src/app.ts',
  '+++ b/src/app.ts',
  '@@ -1,4 +1,5 @@',
  ' const one = 1',
  '-const two = 2',
  '+const two = 22',
  '+const three = 3',
  ' const four = 4',
  '',
].join('\n')

test('Candidate Diff parsing exposes file headers, hunk ranges, and exact line numbers', () => {
  const parsed = parseCandidateDiff(selectedFileDiff)
  assert.deepEqual(parsed.fileHeaders, [
    'diff --git a/src/app.ts b/src/app.ts',
    'index 1111111..2222222 100644',
    '--- a/src/app.ts',
    '+++ b/src/app.ts',
  ])
  assert.equal(parsed.hunks.length, 1)
  assert.deepEqual(parsed.hunks[0], {
    key: 'hunk:1',
    header: '@@ -1,4 +1,5 @@',
    oldStart: 1,
    newStart: 1,
    lines: [
      { kind: 'context', text: 'const one = 1', oldLine: 1, newLine: 1 },
      { kind: 'deletion', text: 'const two = 2', oldLine: 2, newLine: null },
      { kind: 'addition', text: 'const two = 22', oldLine: null, newLine: 2 },
      { kind: 'addition', text: 'const three = 3', oldLine: null, newLine: 3 },
      { kind: 'context', text: 'const four = 4', oldLine: 3, newLine: 4 },
    ],
  })
  assert.equal(parsed.lineCount, 5)
  assert.equal(parsed.truncatedTail, false)
})

test('Candidate Diff parsing keeps exact line numbers across several hunks', () => {
  const parsed = parseCandidateDiff([
    '@@ -10,3 +10,3 @@',
    ' const keep = 1',
    '-const drop = 2',
    '+const kept = 2',
    ' const more = 3',
    '@@ -40,2 +40,3 @@',
    ' const later = 1',
    '+const inserted = 2',
    ' const final = 3',
    '',
  ].join('\n'))
  assert.deepEqual(parsed.hunks.map(hunk => [
    hunk.key,
    hunk.oldStart,
    hunk.newStart,
    hunk.lines.map(line => [line.kind, line.oldLine, line.newLine]),
  ]), [
    ['hunk:1', 10, 10, [
      ['context', 10, 10],
      ['deletion', 11, null],
      ['addition', null, 11],
      ['context', 12, 12],
    ]],
    ['hunk:2', 40, 40, [
      ['context', 40, 40],
      ['addition', null, 41],
      ['context', 41, 42],
    ]],
  ])
  assert.equal(parsed.lineCount, 7)
})

test('Candidate Diff parsing treats Git no-newline markers as metadata instead of content', () => {
  const parsed = parseCandidateDiff([
    '@@ -1,2 +1,2 @@',
    '-const last = 1',
    '\\ No newline at end of file',
    '+const last = 2',
    '',
  ].join('\n'))
  assert.deepEqual(parsed.hunks[0].lines.map(line => [line.kind, line.text]), [
    ['deletion', 'const last = 1'],
    ['addition', 'const last = 2'],
  ])
  assert.equal(parsed.lineCount, 2)
})

test('Candidate Diff parsing reports a Diff that ends inside a line as truncated', () => {
  const truncated = parseCandidateDiff([
    '@@ -1,2 +1,2 @@',
    ' const one = 1',
    '+const two = 2',
  ].join('\n'))
  assert.equal(truncated.truncatedTail, true)
  assert.deepEqual(truncated.hunks[0].lines.map(line => line.text), [
    'const one = 1',
    'const two = 2',
  ])
})

const pairedDiff = parseCandidateDiff([
  'diff --git a/src/app.ts b/src/app.ts',
  'index 1111111..2222222 100644',
  '--- a/src/app.ts',
  '+++ b/src/app.ts',
  '@@ -1,5 +1,6 @@',
  ' const one = 1',
  '-const two = 2',
  '-const three = 3',
  '+const two = 22',
  '+const three = 33',
  '+const four = 4',
  ' const five = 5',
  ' const six = 6',
  '',
].join('\n'))

test('Candidate Diff rows keep one row per changed or context line in unified layout', () => {
  const unified = modelModule.candidateDiffRows(pairedDiff, {
    mode: 'unified',
    collapsedContextHunks: new Set(),
    limit: 200,
  })
  assert.equal(unified.totalRows, 13)
  assert.equal(unified.omittedRows, 0)
  assert.deepEqual(unified.rows.map(row => [row.kind, row.key]), [
    ['file-header', 'file-header:0'],
    ['file-header', 'file-header:1'],
    ['file-header', 'file-header:2'],
    ['file-header', 'file-header:3'],
    ['hunk-header', 'hunk-header:hunk:1'],
    ['line', 'hunk:1:line:0'],
    ['line', 'hunk:1:line:1'],
    ['line', 'hunk:1:line:2'],
    ['line', 'hunk:1:line:3'],
    ['line', 'hunk:1:line:4'],
    ['line', 'hunk:1:line:5'],
    ['line', 'hunk:1:line:6'],
    ['line', 'hunk:1:line:7'],
  ])
  assert.deepEqual(unified.rows[5], {
    kind: 'line',
    key: 'hunk:1:line:0',
    hunkKey: 'hunk:1',
    type: 'context',
    oldLine: 1,
    newLine: 1,
    oldText: 'const one = 1',
    newText: 'const one = 1',
  })
  assert.deepEqual(unified.rows[6], {
    kind: 'line',
    key: 'hunk:1:line:1',
    hunkKey: 'hunk:1',
    type: 'deletion',
    oldLine: 2,
    newLine: null,
    oldText: 'const two = 2',
    newText: 'const two = 2',
  })
})

test('Candidate Diff rows pair deletions with additions in side-by-side layout', () => {
  const sideBySide = modelModule.candidateDiffRows(pairedDiff, {
    mode: 'side-by-side',
    collapsedContextHunks: new Set(),
    limit: 200,
  })
  assert.equal(sideBySide.totalRows, 11)
  assert.deepEqual(sideBySide.rows.map(row => [row.kind, row.key]), [
    ['file-header', 'file-header:0'],
    ['file-header', 'file-header:1'],
    ['file-header', 'file-header:2'],
    ['file-header', 'file-header:3'],
    ['hunk-header', 'hunk-header:hunk:1'],
    ['line', 'hunk:1:pair:0'],
    ['line', 'hunk:1:pair:1'],
    ['line', 'hunk:1:pair:2'],
    ['line', 'hunk:1:pair:3'],
    ['line', 'hunk:1:pair:4'],
    ['line', 'hunk:1:pair:5'],
  ])
  assert.deepEqual(sideBySide.rows[5], {
    kind: 'line',
    key: 'hunk:1:pair:0',
    hunkKey: 'hunk:1',
    type: 'context',
    oldLine: 1,
    newLine: 1,
    oldText: 'const one = 1',
    newText: 'const one = 1',
  })
  assert.deepEqual(sideBySide.rows[6], {
    kind: 'line',
    key: 'hunk:1:pair:1',
    hunkKey: 'hunk:1',
    type: 'modified',
    oldLine: 2,
    newLine: 2,
    oldText: 'const two = 2',
    newText: 'const two = 22',
  })
  assert.deepEqual(sideBySide.rows[8], {
    kind: 'line',
    key: 'hunk:1:pair:3',
    hunkKey: 'hunk:1',
    type: 'addition',
    oldLine: null,
    newLine: 4,
    oldText: '',
    newText: 'const four = 4',
  })
  assert.deepEqual(sideBySide.rows[9], {
    kind: 'line',
    key: 'hunk:1:pair:4',
    hunkKey: 'hunk:1',
    type: 'context',
    oldLine: 4,
    newLine: 5,
    oldText: 'const five = 5',
    newText: 'const five = 5',
  })
})

test('Candidate Diff rows render a bounded window and add more rows without repeating keys', () => {
  const wideDiff = parseCandidateDiff([
    `@@ -1,30 +1,30 @@`,
    ...Array.from({ length: 30 }, (_, index) => ` const line ${String(index + 1)} = ${String(index + 1)}`),
    '',
  ].join('\n'))
  const first = modelModule.candidateDiffRows(wideDiff, {
    mode: 'unified',
    collapsedContextHunks: new Set(),
    limit: 10,
  })
  assert.equal(first.rows.length, 10)
  assert.equal(first.totalRows, 31)
  assert.equal(first.omittedRows, 21)
  const wider = modelModule.candidateDiffRows(wideDiff, {
    mode: 'unified',
    collapsedContextHunks: new Set(),
    limit: 20,
  })
  const firstKeys = new Set(first.rows.map(row => row.key))
  assert.equal(wider.rows.slice(0, 10).every(row => firstKeys.has(row.key)), true)
  assert.equal(new Set(wider.rows.map(row => row.key)).size, wider.rows.length)
  assert.throws(() => modelModule.candidateDiffRows(wideDiff, {
    mode: 'unified',
    collapsedContextHunks: new Set(),
    limit: 501,
  }), /between 1 and 500/u)
})

test('Candidate Diff rows collapse unchanged context per hunk and report the hidden count', () => {
  const collapsed = modelModule.candidateDiffRows(pairedDiff, {
    mode: 'unified',
    collapsedContextHunks: new Set(['hunk:1']),
    limit: 200,
  })
  assert.deepEqual(collapsed.rows.map(row => [row.kind, row.type ?? null]), [
    ['file-header', null],
    ['file-header', null],
    ['file-header', null],
    ['file-header', null],
    ['hunk-header', null],
    ['line', 'deletion'],
    ['line', 'deletion'],
    ['line', 'addition'],
    ['line', 'addition'],
    ['line', 'addition'],
  ])
  assert.deepEqual(
    {
      expanded: collapsed.rows[4].expanded,
      hiddenContext: collapsed.rows[4].hiddenContext,
    },
    { expanded: false, hiddenContext: 3 },
  )
  assert.equal(collapsed.totalRows, 10)
  const expanded = modelModule.candidateDiffRows(pairedDiff, {
    mode: 'unified',
    collapsedContextHunks: new Set(),
    limit: 200,
  })
  assert.deepEqual(
    { expanded: expanded.rows[4].expanded, hiddenContext: expanded.rows[4].hiddenContext },
    { expanded: true, hiddenContext: 0 },
  )
})

test('Candidate Diff search matches rendered rows on either side without unbounded work', () => {
  const rows = modelModule.candidateDiffRows(pairedDiff, {
    mode: 'side-by-side',
    collapsedContextHunks: new Set(),
    limit: 200,
  }).rows
  assert.deepEqual(
    modelModule.candidateDiffMatchKeys(rows, 'FIVE', 200),
    ['hunk:1:pair:4'],
  )
  assert.deepEqual(
    modelModule.candidateDiffMatchKeys(rows, 'const two', 200),
    ['hunk:1:pair:1'],
  )
  assert.deepEqual(
    modelModule.candidateDiffMatchKeys(rows, 'const three', 200),
    ['hunk:1:pair:2'],
  )
  assert.deepEqual(modelModule.candidateDiffMatchKeys(rows, 'missing-token', 200), [])
  const wideRows = modelModule.candidateDiffRows(parseCandidateDiff([
    '@@ -1,30 +1,30 @@',
    ...Array.from({ length: 30 }, (_, index) => ` const line ${String(index + 1)}`),
    '',
  ].join('\n')), { mode: 'unified', collapsedContextHunks: new Set(), limit: 200 }).rows
  assert.equal(modelModule.candidateDiffMatchKeys(wideRows, 'const line', 5).length, 5)
})

test('narrow viewports fall back to the unified layout while wide ones keep the choice', () => {
  assert.equal(modelModule.effectiveCandidateDiffView('side-by-side', true), 'unified')
  assert.equal(modelModule.effectiveCandidateDiffView('unified', true), 'unified')
  assert.equal(modelModule.effectiveCandidateDiffView('side-by-side', false), 'side-by-side')
  assert.equal(modelModule.effectiveCandidateDiffView('unified', false), 'unified')
})

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
    this.attributes = new Map()
    this.children = []
    this.listeners = new Map()
    this.dataset = {}
    this.className = ''
    this.disabled = false
    this.hidden = false
    this.tabIndex = -1
    this.scrollTop = 0
    this.value = ''
    this.parentNode = null
    this.#textContent = ''
  }

  #textContent

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

  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    this.listeners.set(name, (this.listeners.get(name) ?? []).filter(item => item !== listener))
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

  closest(selector) {
    if (selector.startsWith('.') && this.className.split(' ').includes(selector.slice(1))) return this
    return this.parentNode?.closest?.(selector) ?? null
  }

  focus() { this.ownerDocument.activeElement = this }

  click() { this.emit('click') }
}

class FakeDocument {
  activeElement = null
  createElement(tagName) { return new FakeElement(this, tagName) }
}

function findAllByClass(node, className, matches = []) {
  if (node.className === className) matches.push(node)
  for (const child of node.children) findAllByClass(child, className, matches)
  return matches
}

function findByClass(node, className) {
  return findAllByClass(node, className)[0] ?? null
}

const selectedDiffContent = [
  'diff --git a/src/app.ts b/src/app.ts',
  'index 1111111..2222222 100644',
  '--- a/src/app.ts',
  '+++ b/src/app.ts',
  '@@ -1,4 +1,5 @@',
  ' const one = 1',
  '-const two = 2',
  '+const two = 22',
  '+const three = 3',
  ' const four = 4',
  '',
].join('\n')

function diffState(overrides = {}) {
  return {
    status: 'ready',
    path: 'src/app.ts',
    content: selectedDiffContent,
    loadedBytes: 220,
    totalBytes: 220,
    hasMore: false,
    previewLimited: false,
    fileDiffSha256: `sha256:${'4'.repeat(64)}`,
    unavailableReason: null,
    error: null,
    ...overrides,
  }
}

function mountViewer(stateOverrides = {}, optionsOverrides = {}) {
  const document = new FakeDocument()
  const viewer = mountCandidateDiffViewer({
    document,
    onLoadMoreDiff() {},
    onViewModeChange() {},
    ...optionsOverrides,
  })
  document.activeElement = viewer.root
  viewer.update({
    diff: diffState(stateOverrides),
    selectedPath: stateOverrides.selectedPath ?? 'src/app.ts',
    viewMode: stateOverrides.viewMode ?? 'unified',
    candidateDigest: `sha256:${'3'.repeat(64)}`,
  })
  return { document, viewer }
}

function rowNodes(viewer) {
  return [...findByClass(viewer.root, 'wwc-candidate-diff-body').children]
}

test('the viewer renders hunk headers, line numbers, and added or removed markers', () => {
  const { viewer } = mountViewer()
  const rows = rowNodes(viewer)
  assert.equal(rows.length, 10)
  assert.equal(rows[0].dataset.kind, 'file-header')
  assert.equal(rows[4].dataset.kind, 'hunk-header')
  assert.match(rows[4].textContent, /@@ -1,4 \+1,5 @@/u)
  assert.deepEqual(rows.slice(5).map(row => [
    row.dataset.type,
    row.children[0].textContent,
    row.children[1].textContent,
    row.children[2].textContent,
  ]), [
    ['context', '1', '1', ' const one = 1'],
    ['deletion', '2', '', '-const two = 2'],
    ['addition', '', '2', '+const two = 22'],
    ['addition', '', '3', '+const three = 3'],
    ['context', '3', '4', ' const four = 4'],
  ])
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-table').getAttribute('data-columns'), '3')
})

test('the viewer renders one pair per row with four columns in side-by-side layout', () => {
  const { viewer } = mountViewer({ viewMode: 'side-by-side' })
  const rows = rowNodes(viewer)
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-table').getAttribute('data-columns'), '4')
  assert.deepEqual(rows.slice(5).map(row => [
    row.dataset.type,
    row.children[0].textContent,
    row.children[1].textContent,
    row.children[2].textContent,
    row.children[3].textContent,
  ]), [
    ['context', '1', ' const one = 1', '1', ' const one = 1'],
    ['modified', '2', '-const two = 2', '2', '+const two = 22'],
    ['addition', '', '', '3', '+const three = 3'],
    ['context', '3', ' const four = 4', '4', ' const four = 4'],
  ])
})

test('the viewer never renders a Diff that belongs to another file or Candidate', () => {
  const document = new FakeDocument()
  const viewer = mountCandidateDiffViewer({
    document,
    onLoadMoreDiff() {},
    onViewModeChange() {},
  })
  const state = { diff: diffState(), selectedPath: 'src/app.ts', viewMode: 'unified' }
  viewer.update({ ...state, candidateDigest: `sha256:${'3'.repeat(64)}` })
  assert.equal(rowNodes(viewer).length, 10)

  viewer.update({ ...state, selectedPath: 'src/other.ts', candidateDigest: `sha256:${'3'.repeat(64)}` })
  assert.equal(rowNodes(viewer).length, 0)
  assert.equal(viewer.root.textContent.includes('const two = 2'), false)

  const narrowSearch = findByClass(viewer.root, 'wwc-candidate-diff-search')
  narrowSearch.value = 'const two'
  viewer.update({ ...state, candidateDigest: `sha256:${'9'.repeat(64)}` })
  assert.equal(rowNodes(viewer).length, 10)
  assert.equal(narrowSearch.value, '', 'a new Candidate must not inherit the search draft')
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-table').getAttribute('data-columns'), '3')
})

test('the viewer degrades binary, encoding, error, truncation, and empty states clearly', () => {
  const binary = mountViewer({
    status: 'unavailable',
    unavailableReason: 'binary',
    content: '',
    fileDiffSha256: null,
  })
  assert.match(findByClass(binary.viewer.root, 'wwc-candidate-diff-status').textContent,
    /Binary file preview is unavailable\./u)
  assert.equal(rowNodes(binary.viewer).length, 0)

  const encoded = mountViewer({
    status: 'unavailable',
    unavailableReason: 'unsupported-encoding',
    content: '',
    fileDiffSha256: null,
  })
  assert.match(findByClass(encoded.viewer.root, 'wwc-candidate-diff-status').textContent,
    /encoding is not previewable/u)

  const failure = mountViewer({ status: 'error' })
  assert.match(findByClass(failure.viewer.root, 'wwc-candidate-diff-status').textContent,
    /could not be loaded\./u)
  assert.equal(findByClass(failure.viewer.root, 'wwc-candidate-load-more-diff').hidden, false)
  assert.equal(findByClass(failure.viewer.root, 'wwc-candidate-load-more-diff').textContent,
    'Retry Diff')

  const partial = mountViewer({ loadedBytes: 220, totalBytes: 400, hasMore: true })
  assert.match(findByClass(partial.viewer.root, 'wwc-candidate-diff-status').textContent,
    /first 220 of 400 Diff bytes/u)
  assert.equal(findByClass(partial.viewer.root, 'wwc-candidate-load-more-diff').hidden, false)

  const limited = mountViewer({ previewLimited: true, hasMore: false })
  assert.match(findByClass(limited.viewer.root, 'wwc-candidate-diff-status').textContent,
    /Diff preview limit reached\./u)
  assert.equal(findByClass(limited.viewer.root, 'wwc-candidate-load-more-diff').hidden, true)

  const tail = mountViewer({ content: `${selectedDiffContent}+const five = 5` })
  assert.match(findByClass(tail.viewer.root, 'wwc-candidate-diff-status').textContent,
    /ends inside a line/u)

  const headerOnly = mountViewer({ content: [
    'diff --git a/src/app.ts b/src/app.ts',
    '--- a/src/app.ts',
    '+++ b/src/app.ts',
    '',
  ].join('\n') })
  assert.match(findByClass(headerOnly.viewer.root, 'wwc-candidate-diff-status').textContent,
    /no line changes to display\./u)
})

test('very long Diff lines are capped for rendering with a visible note', () => {
  const longLine = `+const long = '${'x'.repeat(3_000)}'`
  const { viewer } = mountViewer({ content: [
    'diff --git a/src/app.ts b/src/app.ts',
    '@@ -1,2 +1,3 @@',
    ' const one = 1',
    longLine,
    ' const four = 4',
    '',
  ].join('\n') })
  const rows = rowNodes(viewer)
  const longRow = rows.find(row => row.dataset.type === 'addition')
  assert.notEqual(longRow, undefined)
  const text = longRow.textContent
  assert.equal(text.length < 3_000, true)
  assert.match(text, /line truncated for rendering/u)
})

const twoHunkContent = [
  'diff --git a/src/app.ts b/src/app.ts',
  '@@ -1,3 +1,3 @@',
  ' const one = 1',
  '-const two = 2',
  '+const two = 22',
  ' const three = 3',
  '@@ -20,3 +20,4 @@',
  ' const twenty = 20',
  '+const twentyOne = 21',
  ' const twentyTwo = 22',
  ' const twentyThree = 23',
  '',
].join('\n')

function mountWindowedViewer() {
  const document = new FakeDocument()
  const requested = []
  const modeChanges = []
  const viewer = mountCandidateDiffViewer({
    document,
    rowLimit: 6,
    onLoadMoreDiff() { requested.push('load') },
    onViewModeChange(mode) { modeChanges.push(mode) },
  })
  document.activeElement = viewer.root
  viewer.update({
    diff: diffState({ content: twoHunkContent }),
    selectedPath: 'src/app.ts',
    viewMode: 'unified',
    candidateDigest: `sha256:${'3'.repeat(64)}`,
  })
  return { document, viewer, requested, modeChanges }
}

test('large Diffs render a bounded window and extend it without rebuilding rendered rows', () => {
  const { viewer } = mountWindowedViewer()
  const rows = rowNodes(viewer)
  assert.equal(rows.length, 6)
  const renderMore = findByClass(viewer.root, 'wwc-candidate-diff-render-more')
  assert.equal(renderMore.hidden, false)
  assert.match(renderMore.textContent, /more Diff rows/u)

  const firstNode = rows[0]
  const search = findByClass(viewer.root, 'wwc-candidate-diff-search')
  search.value = 'const twenty'
  renderMore.click()
  const extended = rowNodes(viewer)
  assert.equal(extended.length, 11, 'the second window holds the remaining rows')
  assert.equal(extended[0] === firstNode, true, 'rendered rows must be kept, not rebuilt')
  assert.equal(new Set(extended.map(row => row.dataset.key)).size, extended.length)
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-search').value, 'const twenty')
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-render-more').hidden, true,
    'nothing is left to render once every row is present')
})

function mountTwoHunkViewer() {
  const document = new FakeDocument()
  const modeChanges = []
  const viewer = mountCandidateDiffViewer({
    document,
    onLoadMoreDiff() {},
    onViewModeChange(mode) { modeChanges.push(mode) },
  })
  document.activeElement = viewer.root
  viewer.update({
    diff: diffState({ content: twoHunkContent }),
    selectedPath: 'src/app.ts',
    viewMode: 'unified',
    candidateDigest: `sha256:${'3'.repeat(64)}`,
  })
  return { document, viewer, modeChanges }
}

test('keyboard navigation moves between hunks, toggles context, and requests layouts', () => {
  const { document, viewer, modeChanges } = mountTwoHunkViewer()
  const scroll = findByClass(viewer.root, 'wwc-candidate-diff-content')
  const toggles = () => [...rowNodes(viewer)]
    .filter(row => row.dataset.kind === 'hunk-header')
    .map(row => row.children[0].children[0])

  scroll.emit('keydown', { key: 'j' })
  assert.equal(document.activeElement, toggles()[0])
  scroll.emit('keydown', { key: 'j' })
  assert.equal(document.activeElement, toggles()[1])
  scroll.emit('keydown', { key: 'k' })
  assert.equal(document.activeElement, toggles()[0])
  scroll.emit('keydown', { key: 's' })
  assert.deepEqual(modeChanges, ['side-by-side'])
  scroll.emit('keydown', { key: 'u' })
  assert.deepEqual(modeChanges, ['side-by-side', 'unified'])

  toggles()[0].emit('click')
  const collapsedRows = rowNodes(viewer)
  assert.equal(collapsedRows
    .filter(row => row.dataset.hunkKey === 'hunk:1')
    .some(row => row.dataset.type === 'context'), false)
  assert.equal(collapsedRows
    .filter(row => row.dataset.hunkKey === 'hunk:2')
    .some(row => row.dataset.type === 'context'), true)
  assert.match(
    collapsedRows.find(row => row.dataset.kind === 'hunk-header').textContent,
    /2 unchanged lines hidden/u,
  )
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-context-toggle').textContent,
    'Show unchanged lines')
  assert.equal(document.activeElement.dataset.hunkKey, 'hunk:1')

  findByClass(viewer.root, 'wwc-candidate-diff-context-toggle').click()
  assert.equal(rowNodes(viewer).some(row => row.dataset.type === 'context'), true)
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-context-toggle').textContent,
    'Hide unchanged lines')
})

test('search jumps between rendered matches by keyboard', () => {
  const { document, viewer } = mountTwoHunkViewer()
  const search = findByClass(viewer.root, 'wwc-candidate-diff-search')
  search.value = 'const t'
  search.emit('input')
  const matchStatus = findByClass(viewer.root, 'wwc-candidate-diff-match-status')
  assert.match(matchStatus.textContent, /Match 1 of \d+/u)
  const matchedRows = rowNodes(viewer).filter(row => row.dataset.match === 'true')
  assert.equal(matchedRows.length > 0, true)

  search.emit('keydown', { key: 'Enter' })
  assert.match(matchStatus.textContent, /Match 2 of \d+/u)
  search.emit('keydown', { key: 'Enter', shiftKey: true })
  assert.match(matchStatus.textContent, /Match 1 of \d+/u)
  assert.equal(document.activeElement.dataset.match, 'true')

  search.value = 'nothing-matches-this'
  search.emit('input')
  assert.match(matchStatus.textContent, /No matching lines/u)
})

test('switching layout keeps the file, search draft, scroll position, and row focus', () => {
  const { document, viewer } = mountWindowedViewer()
  const search = findByClass(viewer.root, 'wwc-candidate-diff-search')
  search.value = 'const one'
  const scroll = findByClass(viewer.root, 'wwc-candidate-diff-content')
  scroll.scrollTop = 240
  const rows = rowNodes(viewer)
  rows[3].focus()

  viewer.update({
    diff: diffState({ content: twoHunkContent }),
    selectedPath: 'src/app.ts',
    viewMode: 'side-by-side',
    candidateDigest: `sha256:${'3'.repeat(64)}`,
  })
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-search').value, 'const one')
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-content').scrollTop, 240)
  assert.equal(document.activeElement.dataset.kind, 'line')
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-table').getAttribute('data-columns'), '4')
})

test('narrow viewports force the unified layout and disable the side-by-side option', () => {
  const document = new FakeDocument()
  const viewer = mountCandidateDiffViewer({
    document,
    narrowViewport: () => true,
    onLoadMoreDiff() {},
    onViewModeChange() {},
  })
  viewer.update({
    diff: diffState({ content: twoHunkContent }),
    selectedPath: 'src/app.ts',
    viewMode: 'side-by-side',
    candidateDigest: `sha256:${'3'.repeat(64)}`,
  })
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-table').getAttribute('data-columns'), '3')
  const sideBySideOption = [...viewer.root.children[0].children[0].children]
    .find(node => node.dataset.mode === 'side-by-side')
  assert.equal(sideBySideOption.disabled, true)
  assert.equal(sideBySideOption.getAttribute('aria-pressed'), 'true')
  assert.equal(findByClass(viewer.root, 'wwc-candidate-diff-view-toggle').dataset.narrow, 'true')
})

test('the viewer builds text with DOM nodes only and never touches innerHTML', () => {
  const { viewer } = mountWindowedViewer()
  findByClass(viewer.root, 'wwc-candidate-diff-render-more').click()
  viewer.update({
    diff: diffState({ content: twoHunkContent, hasMore: true, totalBytes: 900 }),
    selectedPath: 'src/app.ts',
    viewMode: 'side-by-side',
    candidateDigest: `sha256:${'3'.repeat(64)}`,
  })
  const stack = [viewer.root]
  let count = 0
  while (stack.length > 0) {
    const node = stack.pop()
    count += 1
    assert.equal(Object.hasOwn(node, 'innerHTML'), false)
    for (const child of node.children) stack.push(child)
  }
  assert.equal(count > 20, true)
})
