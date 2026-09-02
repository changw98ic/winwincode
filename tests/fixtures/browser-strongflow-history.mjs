import { mountStrongFlowPage } from '/module/strongflow-page.js'

const root = document.querySelector('[data-winwincode-client-root]')
const deliveryId = 'dlv_00000000000000000000000001'
const currentRunId = 'run_00000000000000000000000002'
const failedRunId = 'run_00000000000000000000000001'
const planningRunId = 'run_00000000000000000000000003'
const reviewRunId = 'run_00000000000000000000000004'
const candidateRef = 'refs/winwincode/candidate/browser-history'

function diagram(kind) {
  return {
    id: `diagram:${kind}`,
    kind,
    title: `${kind} diagram`,
    nodes: [{
      id: `node:${kind}`,
      label: `${kind} node`,
      description: `Browser ${kind} description`,
      kind: 'component',
      trustBoundary: null,
      unresolved: false,
    }],
    edges: [],
  }
}

const projection = {
  delivery: {
    schemaVersion: 'winwincode/v1',
    deliveryId,
    deliveryRevision: 6,
    status: 'executing',
    ownership: {
      organizationId: 'org_00000000000000000000000001',
      workspaceId: 'wsp_00000000000000000000000001',
      projectId: 'prj_00000000000000000000000001',
      repositoryId: 'rep_00000000000000000000000001',
    },
    requirements: {
      title: 'Real browser history navigation',
      goal: 'Review every historical StageRun attempt in a real browser.',
    },
    tasks: [{
      id: 'task:browser',
      title: 'Verify history navigation',
      goal: 'Every attempt stays reachable.',
      status: 'active',
      owner: null,
      acceptanceCriterionIds: [],
      blockedByTaskIds: [],
      evidenceRefs: [],
      stageRunIds: [failedRunId, currentRunId],
    }],
    stages: [
      {
        id: failedRunId,
        stage: 'executing',
        role: 'implementer',
        status: 'failed',
        attempt: 1,
        actorType: 'codex',
        deliveryTaskId: 'task:browser',
        startedAt: '2026-09-02T08:00:00.000Z',
        finishedAt: '2026-09-02T08:10:00.000Z',
        sessionBinding: {
          productSessionId: 'psn_00000000000000000000000001',
          executionJobId: 'job_00000000000000000000000001',
          bindingId: 'bind:1',
          boundAt: '2026-09-02T08:00:00.000Z',
          codexThreadId: 'cdx_00000000000000000000000001',
          fencingToken: 'fence:1',
          leaseId: 'lease:1',
          sessionIdentity: null,
          sourceIdentity: null,
          stageRunId: failedRunId,
          workerId: 'wrk_00000000000000000000000001',
          workerSessionId: 'wsn_00000000000000000000000001',
          attempt: 1,
        },
      },
      {
        id: currentRunId,
        stage: 'executing',
        role: 'implementer',
        status: 'running',
        attempt: 2,
        actorType: 'codex',
        deliveryTaskId: 'task:browser',
        startedAt: '2026-09-02T08:11:00.000Z',
        finishedAt: null,
        sessionBinding: {
          productSessionId: 'psn_00000000000000000000000002',
          executionJobId: 'job_00000000000000000000000002',
          bindingId: 'bind:2',
          boundAt: '2026-09-02T08:11:00.000Z',
          codexThreadId: 'cdx_00000000000000000000000002',
          fencingToken: 'fence:2',
          leaseId: 'lease:2',
          sessionIdentity: null,
          sourceIdentity: null,
          stageRunId: currentRunId,
          workerId: 'wrk_00000000000000000000000002',
          workerSessionId: 'wsn_00000000000000000000000002',
          attempt: 2,
        },
      },
      {
        id: planningRunId,
        stage: 'planning',
        role: 'planner',
        status: 'succeeded',
        attempt: 1,
        actorType: 'codex',
        deliveryTaskId: null,
        startedAt: '2026-09-02T07:50:00.000Z',
        finishedAt: '2026-09-02T07:55:00.000Z',
        sessionBinding: {
          productSessionId: 'psn_00000000000000000000000003',
          executionJobId: 'job_00000000000000000000000003',
          bindingId: 'bind:3',
          boundAt: '2026-09-02T07:50:00.000Z',
          codexThreadId: 'cdx_00000000000000000000000003',
          fencingToken: 'fence:3',
          leaseId: 'lease:3',
          sessionIdentity: null,
          sourceIdentity: null,
          stageRunId: planningRunId,
          workerId: 'wrk_00000000000000000000000003',
          workerSessionId: 'wsn_00000000000000000000000003',
          attempt: 1,
        },
      },
      {
        id: reviewRunId,
        stage: 'plan-review',
        role: 'reviewer',
        status: 'succeeded',
        attempt: 1,
        actorType: 'human',
        deliveryTaskId: null,
        startedAt: '2026-09-02T07:56:00.000Z',
        finishedAt: '2026-09-02T07:59:00.000Z',
        sessionBinding: null,
      },
    ],
    attention: [],
    evidence: [
      {
        id: 'evidence:failed',
        type: 'command',
        sourceRef: 'artifact:command:attempt-1',
        candidateRef: 'refs/winwincode/candidate/attempt-1',
        stageRunId: failedRunId,
        sessionBindingId: 'bind:1',
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 2,
        createdAt: '2026-09-02T08:09:00.000Z',
      },
      {
        id: 'evidence:current',
        type: 'test',
        sourceRef: 'artifact:test:attempt-2',
        candidateRef,
        stageRunId: currentRunId,
        sessionBindingId: 'bind:2',
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 2,
        createdAt: '2026-09-02T08:12:00.000Z',
      },
    ],
    solutionReview: null,
    currentCandidate: {
      candidateRef,
      candidateCommitId: '1111111111111111111111111111111111111111',
      candidateTreeId: '2222222222222222222222222222222222222222',
      diffSha256: `sha256:${'3'.repeat(64)}`,
      frozenAt: '2026-09-02T08:12:30.000Z',
    },
    verdict: null,
    publication: null,
    readCursor: {},
  },
  solutionReview: null,
  stage: { id: currentRunId },
  runtime: {
    stageRunId: currentRunId,
    sessions: [],
  },
  evidence: [
    {
      id: 'evidence:failed',
      type: 'command',
      sourceRef: 'artifact:command:attempt-1',
      candidateRef: 'refs/winwincode/candidate/attempt-1',
      stageRunId: failedRunId,
    },
    {
      id: 'evidence:current',
      type: 'test',
      sourceRef: 'artifact:test:attempt-2',
      candidateRef,
      stageRunId: currentRunId,
    },
  ],
  verdict: null,
  attention: [],
  publication: null,
  currentCandidate: {
    candidateRef,
    candidateCommitId: '1111111111111111111111111111111111111111',
    candidateTreeId: '2222222222222222222222222222222222222222',
    diffSha256: `sha256:${'3'.repeat(64)}`,
    frozenAt: '2026-09-02T08:12:30.000Z',
  },
  metadata: {
    source: 'control-plane-snapshot',
    updatedAt: '2026-09-02T08:12:30.000Z',
    revisions: { delivery: 6, deliverySpec: 2, runtime: 7, publication: 0 },
    readCursor: {},
  },
}

class BrowserStrongFlowModel {
  state = {
    status: 'ready',
    realtime: 'subscribed',
    projection,
    interaction: { status: 'idle', error: null },
    error: null,
  }

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  async start() {}
  async refresh() {}
  async decideSolutionReview() {}
  async approveTaskBreakdown() {}
  async resolveAttention() {}
  async submitVerdict() {}
  async advanceDelivery() {}
  cancelPending() {}
  reconnect() {}
  close() {}

  publish(next) {
    this.state = next
    this.listener?.(next)
  }
}

const model = new BrowserStrongFlowModel()
const baseHash = `#/strongflow?delivery=${deliveryId}&session=psn_00000000000000000000000002&stageRun=${currentRunId}`
history.replaceState(null, '', `/${baseHash}&task=task%3Abrowser&run=${failedRunId}`)
const mounted = mountStrongFlowPage({
  root,
  model,
  deliveries: [{
    schemaVersion: 'winwincode/v1',
    deliveryId,
    title: 'Real browser history navigation',
    revision: 6,
    status: 'executing',
  }],
})

function detailElement() {
  return document.querySelector('.wwc-strongflow-history')
}

function historySnapshot() {
  const detail = detailElement()
  const rawText = detail?.innerText ?? detail?.textContent ?? ''
  return {
    detailText: rawText.replace(/\s+/gu, ' ').trim(),
    detailVisible: detail !== null && !detail.hidden,
    hash: location.hash,
    pressedRun: document.querySelector('.wwc-strongflow-run-button[aria-pressed="true"]')
      ?.dataset.stageRunId ?? null,
    taskExpanded: document.querySelector('.wwc-strongflow-history-toggle')
      ?.getAttribute('aria-expanded') ?? null,
  }
}

globalThis.historyDeepLinkSnapshot = () => historySnapshot()

globalThis.historySelectTimelineRun = () => {
  document.querySelector(
    `.wwc-strongflow-run-button[data-stage-run-id="${planningRunId}"]`,
  )?.click()
  return historySnapshot()
}

globalThis.historySelectHumanReviewRun = () => {
  document.querySelector(
    `.wwc-strongflow-run-button[data-stage-run-id="${reviewRunId}"]`,
  )?.click()
  return historySnapshot()
}

globalThis.historyReturnToCurrent = () => {
  document.querySelector('.wwc-strongflow-current-run')?.click()
  return historySnapshot()
}

globalThis.historyKeyboardFlow = () => {
  const toggle = document.querySelector('.wwc-strongflow-history-toggle')
  toggle?.focus()
  const before = {
    expanded: toggle?.getAttribute('aria-expanded') ?? null,
    focusClass: document.activeElement?.className ?? null,
  }
  toggle?.dispatchEvent(new KeyboardEvent('keydown', {
    key: 'ArrowRight',
    bubbles: true,
    cancelable: true,
  }))
  const afterExpand = {
    expanded: toggle?.getAttribute('aria-expanded') ?? null,
    focusClass: document.activeElement?.className ?? null,
    focusRun: document.activeElement?.dataset?.stageRunId ?? null,
  }
  document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', {
    key: 'ArrowDown',
    bubbles: true,
    cancelable: true,
  }))
  const afterDown = {
    focusClass: document.activeElement?.className ?? null,
    focusRun: document.activeElement?.dataset?.stageRunId ?? null,
  }
  return { before, afterExpand, afterDown }
}

globalThis.historyRefreshRestore = () => {
  // A full reload keeps the URL as the single source of selection truth.
  return { hash: location.hash }
}
