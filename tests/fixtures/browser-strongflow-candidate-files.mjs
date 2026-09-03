import { strongFlowRouteHash } from '/module/application.js'
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

const specialFiles = [{
  path: 'docs/removed.md', oldPath: null, status: 'deleted', additions: 0, deletions: 12,
  binary: false, encoding: 'utf-8',
}, {
  path: 'public/logo.png', oldPath: null, status: 'modified', additions: null, deletions: null,
  binary: true, encoding: 'binary',
}, {
  path: 'src/copied.ts', oldPath: 'src/source.ts', status: 'copied', additions: 1, deletions: 0,
  binary: false, encoding: 'utf-8',
}, {
  path: 'src/current.ts', oldPath: 'src/legacy.ts', status: 'renamed', additions: 2, deletions: 1,
  binary: false, encoding: 'utf-8',
}, {
  path: 'src/added.ts', oldPath: null, status: 'added', additions: 9, deletions: 0,
  binary: false, encoding: 'utf-8',
}, {
  path: 'src/changed-kind.ts', oldPath: null, status: 'type_changed', additions: null, deletions: null,
  binary: false, encoding: 'unknown-8bit',
}]
const candidateFiles = [
  ...specialFiles,
  ...Array.from({ length: 224 }, (_, index) => ({
    path: `src/feature-${String(index).padStart(3, '0')}.ts`,
    oldPath: null,
    status: 'modified',
    additions: index + 1,
    deletions: 1,
    binary: false,
    encoding: 'utf-8',
  })),
]

const projection = {
  delivery: {
    schemaVersion: 'winwincode/v1',
    deliveryId,
    deliveryRevision: 4,
    status: 'executing',
    ownership,
    requirements: {
      title: 'Candidate changed files',
      goal: 'Review one exact frozen Candidate.',
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
    frozenAt: '2026-09-02T01:00:00.000Z',
    producerSessionBindingId: 'binding:strongflow:1',
    producerStageRunId: stageRunId,
  },
  publication: null,
  metadata: {
    source: 'control-plane-snapshot',
    updatedAt: '2026-09-02T01:00:00.000Z',
    revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 0 },
    readCursor: {},
  },
}

function diffState(path = 'src/current.ts') {
  return {
    status: 'ready',
    path,
    content: [
      `diff --git a/${path} b/${path}`,
      '@@ -1,2 +1,2 @@',
      `-selected ${path}`,
      `+selected ${path} (changed)`,
    ].join('\n'),
    loadedBytes: 64,
    totalBytes: 128,
    hasMore: true,
    previewLimited: false,
    fileDiffSha256,
    unavailableReason: null,
    error: null,
  }
}

let state = {
  status: 'ready',
  realtime: 'subscribed',
  projection,
  candidateFiles: {
    status: 'ready',
    items: candidateFiles,
    hasMore: true,
    previewLimited: false,
    selectedPath: 'src/current.ts',
    diff: diffState(),
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
    const file = candidateFiles.find(item => item.path === path)
    if (file === undefined) return
    location.hash = strongFlowRouteHash(deliveryId, productSessionId, stageRunId, path)
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
        content: `${state.candidateFiles.diff.content}\n+more`,
        loadedBytes: 128,
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
const mounted = mountStrongFlowPage({ root, model })

function visibleRows() {
  return [...document.querySelectorAll('.wwc-candidate-file-row')]
}

function input(selector, value, eventName) {
  const node = document.querySelector(selector)
  node.value = value
  node.dispatchEvent(new Event(eventName, { bubbles: true }))
}

function clickRow(path) {
  const row = visibleRows().find(node => node.dataset.path === path)
  if (row === undefined) throw new Error(`missing Candidate row: ${path}`)
  row.click()
}

function key(node, value) {
  node.dispatchEvent(new KeyboardEvent('keydown', { key: value, bubbles: true }))
}

function rowPaths() {
  return visibleRows().filter(node => node.dataset.kind === 'file').map(node => node.dataset.path)
}

function statusLabels() {
  return [...document.querySelectorAll('.wwc-candidate-file-state')]
    .map(node => node.textContent.trim())
}

globalThis.runCandidateFilesScenario = async () => {
  await new Promise(resolve => { setTimeout(resolve, 0) })
  const initial = {
    rowCount: visibleRows().length,
    summary: document.querySelector('.wwc-candidate-file-summary').textContent,
    renamed: document.querySelector('.wwc-candidate-file-renamed').textContent,
    unavailable: document.querySelector('.wwc-candidate-file-preview-unavailable').textContent,
    statusLabels: statusLabels(),
    selectedPath: document.querySelector(
      '.wwc-candidate-file-row[aria-selected="true"]',
    )?.dataset.path ?? null,
    candidateSummaryHasTechnicalIdentity: /sha256:|git-candidate:/u.test(
      document.querySelector('.wwc-candidate-file-summary').textContent,
    ),
    technicalOpen: document.querySelector('.wwc-candidate-technical-details').open,
    technicalText: document.querySelector('.wwc-candidate-technical-details').textContent,
  }

  const srcDirectory = visibleRows().find(node => node.dataset.path === 'src')
  srcDirectory.click()
  const collapsed = {
    expanded: srcDirectory.getAttribute('aria-expanded'),
    containsCurrent: rowPaths().includes('src/current.ts'),
  }
  srcDirectory.click()

  const current = visibleRows().find(node => node.dataset.path === 'src/current.ts')
  current.focus()
  key(current, 'ArrowDown')
  const keyboardTarget = document.activeElement?.dataset.path ?? null
  key(document.activeElement, 'Enter')
  await new Promise(resolve => { setTimeout(resolve, 0) })
  const keyboard = {
    target: keyboardTarget,
    selectedPath: document.querySelector(
      '.wwc-candidate-file-row[aria-selected="true"]',
    )?.dataset.path ?? null,
    hash: location.hash,
    diff: document.querySelector('.wwc-candidate-diff-content').textContent,
    activePath: document.activeElement?.dataset.path ?? null,
  }
  document.querySelector('.wwc-candidate-load-more-diff').click()

  input('.wwc-candidate-file-search', 'legacy', 'input')
  input('.wwc-candidate-file-status-filter', 'renamed', 'change')
  const filtered = { paths: rowPaths(), count: visibleRows().length }

  input('.wwc-candidate-file-search', 'logo', 'input')
  input('.wwc-candidate-file-status-filter', 'all', 'change')
  clickRow('public/logo.png')
  await new Promise(resolve => { setTimeout(resolve, 0) })
  const binary = {
    hash: location.hash,
    status: document.querySelector('.wwc-candidate-diff-status').textContent,
  }

  document.querySelector('.wwc-candidate-load-more-files').click()

  return {
    initial,
    collapsed,
    keyboard,
    filtered,
    binary,
    calls,
    mainCount: document.querySelectorAll('main').length,
  }
}

globalThis.closeCandidateFilesScenario = () => { mounted.close() }
