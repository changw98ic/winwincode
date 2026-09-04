import { mountStrongFlowPage } from '/module/strongflow-page.js'

// UI-305 browser regression: a real browser proves that realtime invalidation
// keeps StrongFlow selection, expansion, scroll, panel state, focus, and the
// roving tabindex, and that a Candidate change is reported instead of silently
// re-reading the file under review.

const root = document.querySelector('[data-winwincode-client-root]')
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const candidateRef = 'refs/winwincode/candidate/browser-state'
const FILE_COUNT = 30
const DIFF_LINES = 400

function diagram(kind) {
  return {
    id: `diagram:${kind}`,
    kind,
    title: `${kind} diagram`,
    nodes: [
      {
        id: `node:${kind}:1`,
        label: `${kind} entry`,
        description: 'Entry responsibility for the browser state fixture.',
        kind: 'component',
        trustBoundary: 'browser-boundary',
        unresolved: false,
      },
      {
        id: `node:${kind}:2`,
        label: `${kind} store`,
        description: 'Store responsibility for the browser state fixture.',
        kind: 'data-store',
        trustBoundary: null,
        unresolved: false,
      },
    ],
    edges: [{
      id: `edge:${kind}`,
      from: `node:${kind}:1`,
      to: `node:${kind}:2`,
      label: 'writes',
    }],
  }
}

function candidateFiles(selectedPath) {
  return {
    status: 'ready',
    items: Array.from({ length: FILE_COUNT }, (_, index) => ({
      path: `src/module-${String(index + 1).padStart(2, '0')}.ts`,
      oldPath: null,
      status: 'modified',
      additions: index + 1,
      deletions: 0,
      binary: false,
      encoding: 'utf-8',
    })),
    hasMore: false,
    previewLimited: false,
    selectedPath,
    diff: {
      status: 'ready',
      path: selectedPath,
      content: Array.from(
        { length: DIFF_LINES },
        (_, index) => `@@ line ${String(index + 1)} of the open Diff`,
      ).join('\n'),
      loadedBytes: 12_345,
      totalBytes: 12_345,
      hasMore: false,
      previewLimited: false,
      fileDiffSha256: `sha256:${'a'.repeat(64)}`,
      unavailableReason: null,
      error: null,
    },
    error: null,
  }
}

function createProjection({
  candidateDigest = '3',
  runtimeSequence = 1,
  deliveryRevision = 4,
} = {}) {
  return {
    delivery: {
      schemaVersion: 'winwincode/v1',
      deliveryId,
      deliveryRevision,
      status: 'executing',
      ownership: {
        organizationId: 'org_00000000000000000000000001',
        workspaceId: 'wsp_00000000000000000000000001',
        projectId: 'prj_00000000000000000000000001',
        repositoryId: 'rep_00000000000000000000000001',
      },
      requirements: {
        title: 'StrongFlow realtime state',
        goal: 'Keep review work while the canonical snapshot reloads.',
      },
      tasks: [
        { id: 'task:1', title: 'Expanded Task under review', status: 'active' },
        { id: 'task:2', title: 'Unrelated Task', status: 'pending' },
      ],
      stages: [
        { id: stageRunId, stage: 'executing', role: 'implementer', status: 'running' },
        {
          id: 'run_00000000000000000000000002',
          stage: 'verifying',
          role: 'implementer',
          status: 'waiting',
        },
      ],
      attention: [],
    },
    solutionReview: {
      reviewStatus: 'pending',
      architectureDiagram: diagram('system-architecture'),
      processDiagram: diagram('process-flow'),
    },
    diagramExecution: null,
    stage: { id: stageRunId },
    runtime: {
      stageRunId,
      sessions: [{
        productSessionId: 'psn_00000000000000000000000001',
        stageRunId,
        sessionBindingId: 'bind:1',
        codexThreadId: 'cdx_t0000000000000000000000001',
        deliveryTaskId: 'task:1',
        attempt: 1,
        asOfSequence: runtimeSequence,
        agents: [],
        agentEdges: [],
        activities: [],
        diffSummary: {
          changedFileCount: FILE_COUNT,
          additions: 20,
          deletions: 0,
          sourceRef: `runtime:diff:${String(runtimeSequence)}`,
        },
        usage: null,
        plan: null,
        recovery: {
          failureCount: 0,
          lastFailureSourceRef: null,
          latestRecoverySourceRef: null,
          recoveryCount: 0,
          state: 'none',
        },
      }],
    },
    evidence: [{
      id: 'evidence:1',
      type: 'test',
      sourceRef: 'artifact:test:browser-state',
      candidateRef,
    }],
    verdict: null,
    attention: [],
    currentCandidate: {
      candidateRef,
      candidateCommitId: '1111111111111111111111111111111111111111',
      candidateTreeId: '2222222222222222222222222222222222222222',
      diffSha256: `sha256:${candidateDigest.repeat(64)}`,
      frozenAt: '2026-09-02T09:00:00.000Z',
    },
    publication: null,
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-09-02T09:00:00.000Z',
      revisions: { delivery: deliveryRevision, deliverySpec: 3, runtime: 8, publication: 0 },
      readCursor: {},
    },
  }
}

function ready(projection = createProjection(), files = candidateFiles(null)) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    projection,
    candidateFiles: files,
    interaction: { status: 'idle', error: null },
    error: null,
  }
}

class RealtimeStateModel {
  draftScope = '["browser-realtime-state-actor","browser-realtime-state-scope"]'
  state = ready()
  calls = []

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(state) {
    this.state = state
    this.listener?.(state)
  }

  async start() {}
  async refresh() {}
  async loadCandidateFiles() { this.calls.push(['loadCandidateFiles']) }
  async loadMoreCandidateFiles() { this.calls.push(['loadMoreCandidateFiles']) }
  async selectCandidateFile(path) {
    this.calls.push(['selectCandidateFile', path])
    this.publish(ready(this.state.projection, candidateFiles(path)))
  }
  async loadMoreCandidateDiff() { this.calls.push(['loadMoreCandidateDiff']) }
  async loadStageRunRuntime() { return null }
  async loadStageRunCandidates() { return [] }
  async loadCandidateHistoricalReview() { return null }
  async decideSolutionReview() {}
  async approveTaskBreakdown() {}
  async resolveAttention() {}
  async submitVerdict() {}
  async advanceDelivery() {}
  cancelPending() {}
  reconnect() {}
  close() {}
}

const model = new RealtimeStateModel()
const deliveryList = {
  state: {
    status: 'ready',
    filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
    visible: [],
    loadedCount: 0,
    hasMore: false,
    loadingMore: false,
    moreFailure: null,
    error: null,
    advance: { deliveryId: null, failure: null },
  },
  subscribe(listener) {
    listener(this.state)
    return () => {}
  },
  async start() {},
  async refresh() {},
  async loadMore() {},
  setSearch() {},
  async setStatusFilter() {},
  setAttentionOnly() {},
  setOrder() {},
  async advanceDelivery() {},
  close() {},
}

// A wide viewport keeps the artifacts panels in the desktop split, so the
// assertions observe the panel the user is actually reading.
const mounted = mountStrongFlowPage({
  root,
  model,
  deliveryList,
  viewport: { width: 1_680 },
})

globalThis.closeStrongFlowRealtimeStateFixture = () => { mounted.close() }

const query = selector => document.querySelector(selector)
const queryAll = selector => Array.from(document.querySelectorAll(selector))
const fileRow = path => queryAll('.wwc-candidate-file-row')
  .find(row => row.dataset.path === path) ?? null
const artifactTab = tab => queryAll('.wwc-strongflow-artifact-tab')
  .find(button => button.dataset.artifactTab === tab) ?? null
const activePath = () => document.activeElement instanceof HTMLElement
  ? document.activeElement.dataset.path ?? null
  : null

globalThis.runStrongFlowRealtimeStateScenario = () => {
  const report = {}

  // --- establish the review state the refresh must survive ---
  const comments = query('.wwc-strongflow-solution-actions textarea')
  comments.value = 'Review draft kept in Chrome'
  comments.dispatchEvent(new Event('input', { bubbles: true }))
  comments.selectionStart = 7
  comments.selectionEnd = 7
  comments.focus()

  artifactTab('solution')?.click()
  query('.wwc-strongflow-graph-boundary')?.click()
  query('.wwc-strongflow-graph-zoom-in')?.click()
  query('.wwc-strongflow-history-toggle')?.click()
  fileRow('src/module-02.ts')?.click()
  artifactTab('candidate')?.click()

  const tree = query('.wwc-candidate-file-tree')
  const diff = query('.wwc-candidate-diff-content')
  const viewport = query('.wwc-strongflow-graph-viewport')
  const workspace = query('.wwc-strongflow-workspace')
  const candidateHostBefore = query('.wwc-strongflow-candidate-host')
  const taskRowBefore = query('.wwc-strongflow-task-list li')
  const stageRowBefore = queryAll('.wwc-strongflow-stage-list li')[1]
  const historicalRun = queryAll('.wwc-strongflow-stage-list li')[1]
    ?.querySelector('.wwc-strongflow-run-button')
  historicalRun?.click()

  // Keyboard navigation lands the roving anchor on a row that is not selected.
  const selectedRow = fileRow('src/module-02.ts')
  selectedRow?.focus()
  selectedRow?.dispatchEvent(new KeyboardEvent('keydown', {
    key: 'ArrowDown',
    bubbles: true,
  }))
  const rovingRowPath = activePath()

  tree.scrollTop = 240
  diff.scrollTop = 300
  viewport.scrollTop = 60
  workspace.scrollTop = 96

  const before = {
    candidateHost: candidateHostBefore,
    rovingRowPath,
    rovingTabIndex: fileRow(rovingRowPath)?.getAttribute('tabindex') ?? null,
    focusedPath: activePath(),
    treeScrollTop: tree.scrollTop,
    diffScrollTop: diff.scrollTop,
    viewportScrollTop: viewport.scrollTop,
    workspaceScrollTop: workspace.scrollTop,
    selectedTab: query('.wwc-strongflow-artifact-tab[aria-selected="true"]')?.dataset.artifactTab,
    selectedPath: query('.wwc-candidate-file-row[aria-selected="true"]')?.dataset.path,
    expandedTask: query('.wwc-strongflow-history-toggle')?.getAttribute('aria-expanded'),
    historicalStagePressed: queryAll('.wwc-strongflow-run-button')
      .find(button => button.dataset.stageRunId === 'run_00000000000000000000000002')
      ?.getAttribute('aria-pressed'),
    zoom: viewport.getAttribute('data-zoom'),
    boundaryExpanded: query('.wwc-strongflow-graph-boundary')?.getAttribute('aria-expanded'),
    draft: comments.value,
    caret: comments.selectionStart,
    staleNoticeHidden: query('.wwc-strongflow-candidate-stale')?.hidden,
  }

  // --- high-frequency runtime invalidation ---
  model.publish({
    status: 'refreshing',
    realtime: 'reloading',
    projection: model.state.projection,
    candidateFiles: model.state.candidateFiles,
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const runtimeProjection = createProjection({ runtimeSequence: 9 })
  for (let index = 0; index < 25; index += 1) {
    model.publish(ready(runtimeProjection, model.state.candidateFiles))
  }

  report.runtimeBatch = {
    candidateRetained: query('.wwc-strongflow-candidate-host') === candidateHostBefore,
    taskRowRetained: query('.wwc-strongflow-task-list li') === taskRowBefore,
    stageRowRetained: queryAll('.wwc-strongflow-stage-list li')[1] === stageRowBefore,
    taskStillExpanded: query('.wwc-strongflow-history-toggle')?.getAttribute('aria-expanded'),
    historicalStageStillPressed: queryAll('.wwc-strongflow-run-button')
      .find(button => button.dataset.stageRunId === 'run_00000000000000000000000002')
      ?.getAttribute('aria-pressed'),
    selectedTab: query('.wwc-strongflow-artifact-tab[aria-selected="true"]')?.dataset.artifactTab,
    selectedPath: query('.wwc-candidate-file-row[aria-selected="true"]')?.dataset.path,
    focusedStillRoving: activePath(),
    rovingTabIndex: fileRow(rovingRowPath)?.getAttribute('tabindex') ?? null,
    treeScrollTop: query('.wwc-candidate-file-tree').scrollTop,
    diffScrollTop: query('.wwc-candidate-diff-content').scrollTop,
    viewportScrollTop: query('.wwc-strongflow-graph-viewport').scrollTop,
    workspaceScrollTop: workspace.scrollTop,
    zoom: query('.wwc-strongflow-graph-viewport').getAttribute('data-zoom'),
    boundaryExpanded: query('.wwc-strongflow-graph-boundary')?.getAttribute('aria-expanded'),
    draft: query('.wwc-strongflow-solution-actions textarea').value,
    caret: query('.wwc-strongflow-solution-actions textarea').selectionStart,
    staleNoticeHidden: query('.wwc-strongflow-candidate-stale')?.hidden,
    before,
  }

  // --- Candidate change under the open review context ---
  model.publish(ready(
    createProjection({ candidateDigest: '4', runtimeSequence: 9, deliveryRevision: 5 }),
    candidateFiles('src/module-02.ts'),
  ))
  const staleNotice = query('.wwc-strongflow-candidate-stale')
  report.candidateChange = {
    staleNoticeVisible: staleNotice !== null && staleNotice.hidden === false,
    staleNoticeRole: staleNotice?.getAttribute('role') ?? null,
    staleNoticeIconAriaHidden: staleNotice
      ?.querySelector('.wwc-strongflow-candidate-stale-icon')?.getAttribute('aria-hidden') ?? null,
    staleNoticeText: staleNotice?.querySelector('.wwc-strongflow-candidate-stale-text')
      ?.textContent ?? null,
    selectedPathStill: query('.wwc-candidate-file-row[aria-selected="true"]')?.dataset.path,
    focusDroppedToBody: document.activeElement === document.body,
    focusedPath: activePath(),
    candidateRetained: query('.wwc-strongflow-candidate-host') === candidateHostBefore,
    selectedTab: query('.wwc-strongflow-artifact-tab[aria-selected="true"]')?.dataset.artifactTab,
    expandedTask: query('.wwc-strongflow-history-toggle')?.getAttribute('aria-expanded'),
    treeScrollTop: query('.wwc-candidate-file-tree').scrollTop,
    diffScrollTop: query('.wwc-candidate-diff-content').scrollTop,
  }

  // --- re-selecting a file is the explicit re-confirmation ---
  fileRow('src/module-05.ts')?.click()
  model.publish(ready(
    createProjection({ candidateDigest: '4', runtimeSequence: 9, deliveryRevision: 5 }),
    candidateFiles('src/module-05.ts'),
  ))
  report.reconfirmed = {
    noticeHidden: query('.wwc-strongflow-candidate-stale')?.hidden,
    selectedPath: query('.wwc-candidate-file-row[aria-selected="true"]')?.dataset.path,
    selectCalls: model.calls.filter(([name]) => name === 'selectCandidateFile').length,
  }

  // --- a Candidate change with no open Diff has nothing to re-confirm ---
  model.publish(ready(
    createProjection({ candidateDigest: '5', runtimeSequence: 9, deliveryRevision: 6 }),
    candidateFiles(null),
  ))
  model.publish(ready(
    createProjection({ candidateDigest: '6', runtimeSequence: 9, deliveryRevision: 7 }),
    candidateFiles(null),
  ))
  report.withoutReviewContext = {
    noticeHidden: query('.wwc-strongflow-candidate-stale')?.hidden,
  }

  return report
}
