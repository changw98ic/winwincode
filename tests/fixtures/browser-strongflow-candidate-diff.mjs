import {
  strongFlowCandidateViewFromHash,
  strongFlowRouteHash,
} from '/module/application.js'
import { scopeSelectionFromHash } from '/module/core/scope-context.js'
import { strongFlowHistorySelectionFromHash } from '/module/strongflow-history-selection.js'
import { mountStrongFlowPage } from '/module/strongflow-page.js'

const deliveryId = 'dlv_00000000000000000000000001'
const productSessionId = 'psn_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const candidateRef = `git-candidate:sha256:${'1'.repeat(64)}`
const candidateTreeId = '2'.repeat(40)
const candidateDiffSha256 = `sha256:${'3'.repeat(64)}`
const fileDiffSha256 = `sha256:${'4'.repeat(64)}`
const ownership = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}

const files = [{
  path: 'src/app.ts', oldPath: null, status: 'modified', additions: 3, deletions: 1,
  binary: false, encoding: 'utf-8',
}, {
  path: 'src/renamed.ts', oldPath: 'src/old.ts', status: 'renamed', additions: 1, deletions: 1,
  binary: false, encoding: 'utf-8',
}, {
  path: 'public/logo.png', oldPath: null, status: 'modified', additions: null, deletions: null,
  binary: true, encoding: 'binary',
}]

const projection = {
  delivery: {
    schemaVersion: 'winwincode/v1',
    deliveryId,
    deliveryRevision: 4,
    status: 'executing',
    ownership,
    requirements: {
      title: 'Candidate Diff viewer',
      goal: 'Review one exact frozen Candidate Diff.',
    },
    tasks: [],
    stages: [],
    attention: [],
  },
  solutionReview: null,
  stage: { id: stageRunId },
  runtime: { stageRunId, sessions: [] },
  evidence: [],
  verdict: null,
  attention: [],
  currentCandidate: {
    candidateRef,
    candidateCommitId: '1'.repeat(40),
    candidateTreeId,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 3,
    diffSha256: candidateDiffSha256,
    frozenAt: '2026-09-03T01:00:00.000Z',
    producerSessionBindingId: 'binding:strongflow:1',
    producerStageRunId: stageRunId,
  },
  publication: null,
  metadata: {
    source: 'control-plane-snapshot',
    updatedAt: '2026-09-03T01:00:00.000Z',
    revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 0 },
    readCursor: {},
  },
}

function fileDiff(path) {
  if (path === 'src/renamed.ts') {
    return [
      'diff --git a/src/old.ts b/src/renamed.ts',
      '@@ -10,7 +10,7 @@ function before() {',
      ' const alpha = 1',
      '-const beta = 2',
      '+const beta = 22',
      ' const gamma = 3',
      ' const delta = 4',
      ' const epsilon = 5',
      ' const zeta = 6',
      ' const eta = 7',
      '@@ -40,3 +40,4 @@ function later() {',
      ' const theta = 1',
      ' const iota = 2',
      '+const kappa = 3',
      ' const lambda = 4',
      '',
    ].join('\n')
  }
  return [
    `diff --git a/${path} b/${path}`,
    '@@ -1,3 +1,5 @@',
    ' const one = 1',
    '-const two = 2',
    '+const two = 22',
    '+const three = 3',
    ' const four = 4',
  ].join('\n')
}

function diffState(path, overrides = {}) {
  return {
    status: 'ready',
    path,
    content: fileDiff(path),
    loadedBytes: 320,
    totalBytes: 640,
    hasMore: true,
    previewLimited: false,
    fileDiffSha256,
    unavailableReason: null,
    error: null,
    ...overrides,
  }
}

let state = {
  status: 'ready',
  realtime: 'subscribed',
  projection,
  candidateFiles: {
    status: 'ready',
    items: files,
    hasMore: false,
    previewLimited: false,
    selectedPath: 'src/renamed.ts',
    diff: diffState('src/renamed.ts'),
    error: null,
  },
  interaction: { status: 'idle', error: null },
  error: null,
}
let listener = null
const calls = []
function publish(candidateFilesState) {
  state = { ...state, candidateFiles: candidateFilesState }
  listener?.(state)
}

function candidateRoute(path, mode = strongFlowCandidateViewFromHash(location.hash) ?? 'unified') {
  return strongFlowRouteHash(
    deliveryId,
    productSessionId,
    stageRunId,
    path,
    mode,
    scopeSelectionFromHash(location.hash),
    strongFlowHistorySelectionFromHash(location.hash),
  )
}

function routeFacts() {
  const parameters = new URLSearchParams(location.hash.split('?')[1] ?? '')
  return Object.fromEntries([
    'file',
    'view',
    'organizationId',
    'workspaceId',
    'projectId',
    'repositoryId',
    'task',
    'run',
  ].map(name => [name, parameters.get(name)]))
}

const model = {
  get state() { return state },
  subscribe(next) {
    listener = next
    next(state)
    return () => { listener = null }
  },
  async start() { calls.push(['start']) },
  async refresh() {},
  async loadCandidateFiles() { calls.push(['loadCandidateFiles']) },
  async loadMoreCandidateFiles() { calls.push(['loadMoreCandidateFiles']) },
  async selectCandidateFile(path) {
    calls.push(['selectCandidateFile', path])
    const file = files.find(item => item.path === path)
    if (file === undefined) return
    location.hash = candidateRoute(path)
    publish({
      ...state.candidateFiles,
      selectedPath: path,
      diff: file.binary || file.encoding !== 'utf-8'
        ? {
            status: 'unavailable', path, content: '', loadedBytes: 0, totalBytes: null,
            hasMore: false, previewLimited: false, fileDiffSha256: null,
            unavailableReason: file.binary ? 'binary' : 'unsupported-encoding', error: null,
          }
        : diffState(path),
    })
  },
  async loadMoreCandidateDiff() {
    calls.push(['loadMoreCandidateDiff'])
    publish({
      ...state.candidateFiles,
      diff: {
        ...state.candidateFiles.diff,
        loadedBytes: 640,
        hasMore: false,
      },
    })
  },
  async decideSolutionReview() {},
  async approveTaskBreakdown() {},
  async resolveAttention() {},
  async submitVerdict() {},
  async advanceDelivery() {},
  cancelPending() {},
  reconnect() {},
  close() { calls.push(['close']) },
}

const root = document.querySelector('[data-winwincode-client-root]')
const mounted = mountStrongFlowPage({
  root,
  model,
  candidateView: 'unified',
  historyLocation: null,
  onCandidateViewModeChange(mode) {
    calls.push(['viewMode', mode])
    const parameters = new URLSearchParams(location.hash.split('?')[1] ?? '')
    location.hash = candidateRoute(parameters.get('file'), mode)
  },
})

function viewer() { return document.querySelector('.wwc-candidate-diff-viewer') }
function rows() { return [...document.querySelectorAll('.wwc-candidate-diff-row')] }
function lineRows() { return rows().filter(row => row.dataset.kind === 'line') }
function toggles() {
  return [...document.querySelectorAll('.wwc-candidate-diff-hunk-toggle')]
}
function query(selector) { return document.querySelector(selector) }
function key(node, value, init = {}) {
  node.dispatchEvent(new KeyboardEvent('keydown', { key: value, bubbles: true, ...init }))
}

globalThis.runCandidateDiffScenario = async () => {
  await new Promise(resolve => { setTimeout(resolve, 0) })
  const initial = {
    columns: query('.wwc-candidate-diff-table').getAttribute('data-columns'),
    rowCount: rows().length,
    hunkHeaders: toggles().map(toggle => toggle.textContent),
    firstLine: lineRows()[0] === undefined
      ? null
      : lineRows()[0].textContent,
    deletionLine: lineRows().find(row => row.dataset.type === 'deletion')?.textContent ?? null,
    additionLine: lineRows().find(row => row.dataset.type === 'addition')?.textContent ?? null,
    pressed: [...document.querySelectorAll('.wwc-candidate-diff-view-option')]
      .map(node => `${node.dataset.mode}:${node.getAttribute('aria-pressed')}`),
    status: query('.wwc-candidate-diff-status').textContent,
    fileSummary: query('.wwc-candidate-file-summary').textContent,
    selectedPath: query('.wwc-candidate-file-row[aria-selected="true"]')?.dataset.path ?? null,
    hash: location.hash,
    route: routeFacts(),
  }

  query('.wwc-candidate-diff-search').value = 'kappa'
  query('.wwc-candidate-diff-search').dispatchEvent(new Event('input', { bubbles: true }))
  key(query('.wwc-candidate-diff-search'), 'Enter')
  const search = {
    matchStatus: query('.wwc-candidate-diff-match-status').textContent,
    activeText: document.activeElement?.textContent ?? null,
  }

  key(query('.wwc-candidate-diff-content'), 'j')
  // A focused hunk toggle is activated with Enter or Space; synthetic key events do not
  // trigger the browser's default activation, so the focused button is clicked directly.
  document.activeElement.click()
  const collapsed = {
    focusedHunk: document.activeElement?.dataset.hunkKey ?? null,
    hiddenNote: query('.wwc-candidate-diff-hunk-hidden')?.textContent ?? null,
    contextRowsInFirstHunk: lineRows()
      .filter(row => row.dataset.hunkKey === 'hunk:1' && row.dataset.type === 'context').length,
    contextToggle: query('.wwc-candidate-diff-context-toggle').textContent,
  }

  const stableHeaderBeforeSwitch = rows()[0]
  const diffScroll = query('.wwc-candidate-diff-content')
  diffScroll.style.maxHeight = '4rem'
  diffScroll.scrollTop = 48
  const scrollTopBeforeSwitch = diffScroll.scrollTop
  const focusedHunkBeforeSwitch = document.activeElement?.dataset.hunkKey ?? null
  key(document.activeElement, 's')
  await new Promise(resolve => { setTimeout(resolve, 0) })
  const switched = {
    columns: query('.wwc-candidate-diff-table').getAttribute('data-columns'),
    rowCount: rows().length,
    modifiedLine: lineRows().find(row => row.dataset.type === 'modified')?.textContent ?? null,
    searchDraft: query('.wwc-candidate-diff-search').value,
    hash: location.hash,
    route: routeFacts(),
    selectedPath: query('.wwc-candidate-file-row[aria-selected="true"]')?.dataset.path ?? null,
    scrollTop: query('.wwc-candidate-diff-content').scrollTop,
    pressed: [...document.querySelectorAll('.wwc-candidate-diff-view-option')]
      .map(node => `${node.dataset.mode}:${node.getAttribute('aria-pressed')}`),
    calls: calls.filter(call => call[0] === 'viewMode'),
    stableHeaderPreserved: rows()[0] === stableHeaderBeforeSwitch,
    scrollTopBeforeSwitch,
    focusedHunkBeforeSwitch,
    focusedHunkAfterSwitch: document.activeElement?.dataset.hunkKey ?? null,
    focusedClassAfterSwitch: document.activeElement?.className ?? null,
  }

  key(query('.wwc-candidate-diff-content'), 'u')
  await new Promise(resolve => { setTimeout(resolve, 0) })
  const backToUnified = {
    columns: query('.wwc-candidate-diff-table').getAttribute('data-columns'),
  }

  query('.wwc-candidate-load-more-diff').click()
  await new Promise(resolve => { setTimeout(resolve, 0) })
  const loadedMore = {
    status: query('.wwc-candidate-diff-status').textContent,
    loadMoreHidden: query('.wwc-candidate-load-more-diff').hidden,
  }

  const binaryRow = [...document.querySelectorAll('.wwc-candidate-file-row')]
    .find(node => node.dataset.path === 'public/logo.png')
  binaryRow.click()
  await new Promise(resolve => { setTimeout(resolve, 0) })
  const binary = {
    status: query('.wwc-candidate-diff-status').textContent,
    rowCount: rows().length,
    hash: location.hash,
    route: routeFacts(),
  }

  return {
    initial,
    search,
    collapsed,
    switched,
    backToUnified,
    loadedMore,
    binary,
    narrow: {},
    mainRegionCount: document.querySelectorAll('.wwc-strongflow-main-region').length,
  }
}

globalThis.runCandidateDiffNarrowScenario = async () => {
  await new Promise(resolve => { setTimeout(resolve, 0) })
  const sideBySide = [...document.querySelectorAll('.wwc-candidate-diff-view-option')]
    .find(node => node.dataset.mode === 'side-by-side')
  sideBySide.click()
  await new Promise(resolve => { setTimeout(resolve, 0) })
  return {
    columns: query('.wwc-candidate-diff-table').getAttribute('data-columns'),
    disabled: sideBySide.disabled,
    narrow: query('.wwc-candidate-diff-view-toggle').dataset.narrow,
    hash: location.hash,
    route: routeFacts(),
  }
}

globalThis.closeCandidateDiffScenario = () => { mounted.close() }
