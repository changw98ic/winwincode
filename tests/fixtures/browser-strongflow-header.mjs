import { mountStrongFlowPage } from '/module/strongflow-page.js'

const root = document.querySelector('[data-winwincode-client-root]')
const deliveryId = 'dlv_00000000000000000000000007'
const currentRunId = 'run_00000000000000000000000007'
const productSessionId = 'psn_00000000000000000000000007'
const workerId = 'wrk_00000000000000000000000007'
const workerSessionId = 'wss_00000000000000000000000007'
const codexThreadId = 'cdx_00000000000000000000000007'
const executionJobId = 'job_00000000000000000000000007'
const leaseId = 'lease_00000000000000000000000007'
const candidateRef = 'refs/winwincode/candidate/browser-header'

function stage(status) {
  return {
    id: currentRunId,
    stage: 'executing',
    role: 'implementer',
    status,
    attempt: 3,
    actorType: 'codex',
    deliveryTaskId: null,
    startedAt: '2026-09-03T08:00:00.000Z',
    finishedAt: null,
    sessionBinding: {
      productSessionId,
      executionJobId,
      bindingId: 'bind:7',
      boundAt: '2026-09-03T08:00:00.000Z',
      codexThreadId,
      fencingToken: 'fence:7',
      leaseId,
      sessionIdentity: null,
      sourceIdentity: null,
      stageRunId: currentRunId,
      workerId,
      workerSessionId,
      attempt: 3,
    },
  }
}

function build(status, overrides = {}) {
  const deliveryStage = stage(status)
  const delivery = {
    schemaVersion: 'winwincode/v1',
    deliveryId,
    deliveryRevision: 4,
    status: 'executing',
    ownership: {
      organizationId: 'org_00000000000000000000000001',
      workspaceId: 'wsp_00000000000000000000000001',
      projectId: 'prj_00000000000000000000000001',
      repositoryId: 'rep_00000000000000000000000001',
    },
    requirements: {
      title: 'Real browser next-action header',
      goal: 'Answer what happens now and what to do next.',
    },
    tasks: [],
    stages: [deliveryStage],
    attention: [],
    evidence: [],
    solutionReview: null,
    currentCandidate: {
      candidateRef,
      candidateCommitId: '7'.repeat(40),
      candidateTreeId: '8'.repeat(40),
      diffSha256: `sha256:${'9'.repeat(64)}`,
      frozenAt: '2026-09-03T08:01:00.000Z',
    },
    verdict: null,
    publication: null,
    readCursor: {},
    ...overrides.delivery,
  }
  return {
    status: 'ready',
    realtime: 'subscribed',
    projection: {
      delivery,
      solutionReview: delivery.solutionReview,
      stage: deliveryStage,
      runtime: {
        stageRunId: currentRunId,
        sessions: [],
      },
      evidence: [],
      verdict: delivery.verdict,
      attention: delivery.attention,
      publication: delivery.publication,
      currentCandidate: delivery.currentCandidate,
      diagramExecution: null,
      metadata: {
        source: 'control-plane-snapshot',
        updatedAt: '2026-09-03T08:01:00.000Z',
        revisions: { delivery: 4, deliverySpec: 2, runtime: 7, publication: 0 },
        readCursor: {},
      },
    },
    candidateFiles: {
      status: 'idle',
      items: [],
      hasMore: false,
      previewLimited: false,
      selectedPath: null,
      diff: {
        status: 'idle',
        path: null,
        content: '',
        loadedBytes: 0,
        totalBytes: null,
        hasMore: false,
        previewLimited: false,
        fileDiffSha256: null,
        unavailableReason: null,
        error: null,
      },
      error: null,
    },
    interaction: { status: 'idle', error: null },
    error: null,
    ...overrides.state,
  }
}

function settled() {
  return new Promise(resolve => { requestAnimationFrame(() => { resolve() }) })
}

class BrowserHeaderModel {
  state = build('running')

  draftScope = 'scope:browser-header'

  calls = []

  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  async start() { this.calls.push(['start']) }
  async refresh() { this.calls.push(['refresh']) }
  async decideSolutionReview() { this.calls.push(['decideSolutionReview']) }
  async approveTaskBreakdown() { this.calls.push(['approveTaskBreakdown']) }
  async resolveAttention() { this.calls.push(['resolveAttention']) }
  async submitVerdict() { this.calls.push(['submitVerdict']) }
  async advanceDelivery() { this.calls.push(['advanceDelivery']) }
  async loadStageRunRuntime() { return null }
  async loadStageRunCandidates() { return [] }
  async loadCandidateHistoricalReview() { return null }
  async loadCandidateFiles() {}
  async loadMoreCandidateFiles() {}
  async selectCandidateFile() {}
  async loadMoreCandidateDiff() {}
  cancelPending() {}
  reconnect() {}
  close() {}

  publish(next) {
    this.state = next
    this.listener?.(next)
  }
}

const model = new BrowserHeaderModel()
const deliveryList = {
  state: {
    status: 'ready',
    filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
    visible: [{
      schemaVersion: 'winwincode/v1',
      deliveryId,
      title: 'Real browser next-action header',
      revision: 4,
      status: 'executing',
      updatedAt: '2026-09-03T08:01:00.000Z',
      openAttentionCount: 0,
      activeStageRunId: null,
      ownership: {
        organizationId: 'org_00000000000000000000000001',
        workspaceId: 'wsp_00000000000000000000000001',
        projectId: 'prj_00000000000000000000000001',
        repositoryId: 'rep_00000000000000000000000001',
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
    }],
    loadedCount: 1,
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
history.replaceState(null, '', `#/strongflow?delivery=${deliveryId}&session=${productSessionId}&stageRun=${currentRunId}`)
mountStrongFlowPage({ root, model, deliveryList })

function trimmed(node) {
  return (node?.innerText ?? '').replace(/\s+/gu, ' ').trim()
}

globalThis.headerPrimary = () => {
  const header = document.querySelector('.wwc-strongflow-header')
  const status = header?.querySelector('.wwc-strongflow-header-status')
  const run = header?.querySelector('.wwc-strongflow-header-run')
  const reason = header?.querySelector('.wwc-strongflow-header-reason')
  const next = header?.querySelector('.wwc-strongflow-header-next')
  const primary = `${status?.textContent ?? ''} ${run?.textContent ?? ''} ${
    reason?.textContent ?? ''
  } ${next?.textContent ?? ''}`
  return {
    hidden: header === null || header.hidden,
    status: status?.textContent ?? null,
    run: run?.textContent ?? null,
    reason: reason?.textContent ?? null,
    next: next?.textContent ?? null,
    hasTechnicalId: /dlv_|psn_|run_|cdx_|wrk_|wss_|job_|lease_/u.test(primary),
  }
}

globalThis.headerToggleIdentity = async () => {
  const toggle = document.querySelector('.wwc-strongflow-identity-toggle')
  toggle?.click()
  await settled()
  const list = document.querySelector('.wwc-strongflow-identity-list')
  const rows = [...(list?.querySelectorAll('.wwc-strongflow-identity-row dt') ?? [])]
    .map(node => node.textContent)
  const values = [...(list?.querySelectorAll('.wwc-strongflow-identity-row dd') ?? [])]
    .map(node => node.textContent)
  return {
    expanded: toggle?.getAttribute('aria-expanded') ?? null,
    controls: toggle?.getAttribute('aria-controls') ?? null,
    listId: list?.id ?? null,
    listHidden: list?.hidden ?? null,
    toggleTag: toggle?.tagName ?? null,
    terms: rows,
    values,
  }
}

globalThis.headerEquivalentRepublish = async () => {
  const header = document.querySelector('.wwc-strongflow-header')
  const status = header?.querySelector('.wwc-strongflow-header-status')
  const list = document.querySelector('.wwc-strongflow-identity-list')
  const firstRow = list?.querySelector('.wwc-strongflow-identity-row')
  model.publish(build('running'))
  await settled()
  return {
    sameStatus: document.querySelector('.wwc-strongflow-header-status') === status,
    sameList: document.querySelector('.wwc-strongflow-identity-list') === list,
    sameRow: list?.querySelector('.wwc-strongflow-identity-row') === firstRow,
    stillOpen: list?.hidden === false,
    expanded: document.querySelector('.wwc-strongflow-identity-toggle')
      ?.getAttribute('aria-expanded') ?? null,
  }
}

globalThis.headerStateChanges = async () => {
  model.publish(build('running', {
    delivery: {
      attention: [{
        blocking: true,
        createdAt: '2026-09-03T08:02:00.000Z',
        deliverySpecId: 'spec:7',
        id: 'attention:7',
        options: [],
        resolutionSummary: null,
        resolvedAt: null,
        resolvedBy: null,
        stageRunId: null,
        status: 'open',
        title: 'Which repository should receive the result?',
        type: 'decision_required',
      }],
    },
  }))
  await settled()
  const waiting = globalThis.headerPrimary()
  const waitingValues = [
    ...document.querySelectorAll('.wwc-strongflow-identity-list .wwc-strongflow-identity-row dd'),
  ].map(node => node.textContent)
  model.publish(build('failed', {
    delivery: {
      stages: [stage('failed')],
    },
  }))
  await settled()
  const failed = globalThis.headerPrimary()
  const failedValues = [
    ...document.querySelectorAll('.wwc-strongflow-identity-list .wwc-strongflow-identity-row dd'),
  ].map(node => node.textContent)
  return {
    waiting,
    waitingIdentity: waitingValues,
    failed,
    failedIdentity: failedValues,
  }
}

globalThis.headerText = () => trimmed(document.querySelector('.wwc-strongflow-header'))
