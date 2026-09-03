import { mountStrongFlowPage } from '/module/strongflow-page.js'

const root = document.querySelector('[data-winwincode-client-root]')
const deliveryId = 'dlv_00000000000000000000000001'
const currentRunId = 'run_00000000000000000000000002'
const failedRunId = 'run_00000000000000000000000001'
const planningRunId = 'run_00000000000000000000000003'
const reviewRunId = 'run_00000000000000000000000004'
const candidateRef = 'refs/winwincode/candidate/browser-history'
const historicalCandidateRef = 'refs/winwincode/candidate/attempt-1'
const RUNTIME_EVENT_COUNT = 200

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
    status: 'ready-to-deliver',
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
    tasks: [
      {
        id: 'task:browser',
        title: 'Verify history navigation',
        goal: 'Every attempt stays reachable.',
        status: 'active',
        owner: null,
        acceptanceCriterionIds: [],
        blockedByTaskIds: [],
        evidenceRefs: [],
        stageRunIds: [failedRunId, currentRunId],
      },
      {
        id: 'task:browser2',
        title: 'Crossed deep-link guard',
        goal: 'A crossed task/run link normalizes onto the run association.',
        status: 'pending',
        owner: null,
        acceptanceCriterionIds: [],
        blockedByTaskIds: [],
        evidenceRefs: [],
        stageRunIds: [],
      },
    ],
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
        candidateRef: historicalCandidateRef,
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
      candidateRef: historicalCandidateRef,
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

const RUNTIME_BINDING_BY_RUN = {
  [failedRunId]: ['psn_00000000000000000000000001', 'cdx_00000000000000000000000001'],
  [planningRunId]: ['psn_00000000000000000000000003', 'cdx_00000000000000000000000003'],
}

function historicalRuntimeSnapshot(stageRunId, binding) {
  return {
    kind: 'runtime_projection',
    productSessionId: binding[0],
    deliveryId,
    stageRunId,
    revision: 7,
    lastProjectionSequence: 200,
    rebuiltAt: '2026-09-02T08:09:30.000Z',
    readCursor: {},
    eventCursor: {},
    sessions: [{
      productSessionId: binding[0],
      stageRunId,
      sessionBindingId: 'bind:1',
      executionJobId: 'job_00000000000000000000000001',
      workerSessionId: 'wsn_00000000000000000000000001',
      codexThreadId: binding[1],
      fencingToken: 'fence:1',
      leaseId: 'lease:1',
      attempt: 1,
      deliveryTaskId: 'task:browser',
      asOfSequence: 200,
      diffSummary: null,
      plan: null,
      usage: null,
      recovery: {
        failureCount: 0,
        lastFailureSourceRef: null,
        latestRecoverySourceRef: null,
        recoveryCount: 0,
        state: 'none',
      },
      agents: [],
      agentEdges: [],
      activities: Array.from({ length: RUNTIME_EVENT_COUNT }, (_, index) => ({
        activityType: 'shell_command',
        callId: `call:${String(index + 1)}`,
        command: `cargo test --event ${String(index + 1)}`,
        outcome: 'succeeded',
        exitCode: 0,
        sourceRef: `artifact:runtime:${String(index + 1)}`,
        status: 'succeeded',
      })),
    }],
  }
}

const historicalCandidateSummary = {
  candidateCommitId: '4444444444444444444444444444444444444444',
  candidateRef: historicalCandidateRef,
  candidateTreeId: '5555555555555555555555555555555555555555',
  deliverySpecId: 'spec:1',
  deliverySpecRevision: 2,
  diffSha256: `sha256:${'4'.repeat(64)}`,
  frozenAt: '2026-09-02T08:09:30.000Z',
  producerSessionBindingId: 'bind:1',
  producerStageRunId: failedRunId,
}

function settled() {
  return new Promise(resolve => { requestAnimationFrame(() => { resolve() }) })
}

function emptyCandidateFilesState() {
  return {
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
  }
}

class BrowserStrongFlowModel {
  draftScope = '["browser-strongflow-history-actor","browser-strongflow-history-scope"]'
  state = {
    status: 'ready',
    realtime: 'subscribed',
    projection,
    candidateFiles: emptyCandidateFilesState(),
    interaction: { status: 'idle', error: null },
    error: null,
  }

  calls = []

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  async start() { this.calls.push(['start']) }
  async refresh() { this.calls.push(['refresh']) }
  async loadCandidateFiles() { this.calls.push(['loadCandidateFiles']) }
  async loadMoreCandidateFiles() { this.calls.push(['loadMoreCandidateFiles']) }
  async selectCandidateFile(path) { this.calls.push(['selectCandidateFile', path]) }
  async loadMoreCandidateDiff() { this.calls.push(['loadMoreCandidateDiff']) }
  async decideSolutionReview() { this.calls.push(['decideSolutionReview']) }
  async approveTaskBreakdown() { this.calls.push(['approveTaskBreakdown']) }
  async resolveAttention() { this.calls.push(['resolveAttention']) }
  async submitVerdict() { this.calls.push(['submitVerdict']) }
  async advanceDelivery() { this.calls.push(['advanceDelivery']) }
  async loadStageRunRuntime(stageRunId) {
    this.calls.push(['loadStageRunRuntime', stageRunId])
    const binding = RUNTIME_BINDING_BY_RUN[stageRunId]
    return binding === undefined ? null : historicalRuntimeSnapshot(stageRunId, binding)
  }
  async loadStageRunCandidates(stageRunId) {
    this.calls.push(['loadStageRunCandidates', stageRunId])
    return stageRunId === failedRunId
      ? [{
        availability: 'available',
        candidate: historicalCandidateSummary,
        firstSeenDeliveryRevision: 5,
        isCurrentAtReadCursor: false,
        lastSeenDeliveryRevision: 6,
        reviewDeliveryRevision: null,
      }]
      : []
  }
  async loadCandidateHistoricalReview(candidate) {
    this.calls.push(['loadCandidateHistoricalReview', candidate.candidateRef])
    return {
      availability: 'available',
      candidate: historicalCandidateSummary,
      currentAuthorization: false,
      displayOnly: true,
      evidence: [{
        id: 'evidence:failed',
        type: 'command',
        sourceRef: 'artifact:command:attempt-1',
        candidateRef: candidate.candidateRef,
        stageRunId: failedRunId,
        sessionBindingId: 'bind:1',
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 2,
        createdAt: '2026-09-02T08:09:00.000Z',
      }],
      firstSeenDeliveryRevision: 5,
      kind: 'candidate_historical_review',
      lastSeenDeliveryRevision: 6,
      readCursor: {},
      reviewDeliveryRevision: null,
      verdict: null,
    }
  }
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
// Start from the exact Task association owned by the historical StageRun.
history.replaceState(null, '', `/${baseHash}&task=task%3Abrowser&run=${failedRunId}`)
const mounted = mountStrongFlowPage({
  root,
  model,
  deliveries: [{
    schemaVersion: 'winwincode/v1',
    deliveryId,
    title: 'Real browser history navigation',
    revision: 6,
    status: 'ready-to-deliver',
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

globalThis.historyMutationGate = () => {
  const advance = document.querySelector('.wwc-strongflow-advance-delivery')
  advance?.click()
  const note = document.querySelector('.wwc-strongflow-history-blocked')
  return {
    advanceDisabled: advance?.disabled ?? null,
    noteVisible: note !== null && !note.hidden,
    advanceCalls: model.calls
      .filter(([name]) => name === 'advanceDelivery').length,
  }
}

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

globalThis.historyRuntimeProbe = async () => {
  await settled()
  const sessions = document.querySelector('.wwc-strongflow-history-runtime-sessions')
  const activities = document.querySelector('.wwc-strongflow-history-runtime-activities')
  return {
    runtimeText: (document.querySelector('.wwc-strongflow-history-runtime')?.innerText ?? '')
      .replace(/\s+/gu, ' ').trim(),
    sessionText: (sessions?.children[0]?.innerText ?? '').replace(/\s+/gu, ' ').trim(),
    sessionCount: sessions?.children.length ?? 0,
    activityCount: activities?.children.length ?? 0,
    activityText: (activities?.children[0]?.innerText ?? '').replace(/\s+/gu, ' ').trim(),
    omittedText: (sessions?.querySelector('.wwc-strongflow-omitted')?.innerText ?? '')
      .replace(/\s+/gu, ' ').trim(),
  }
}

globalThis.historyOpenCandidate = async () => {
  document.querySelector('.wwc-strongflow-history-candidate')?.click()
  await settled()
  const review = document.querySelector('.wwc-strongflow-history-review')
  return {
    reviewText: (review?.innerText ?? '').replace(/\s+/gu, ' ').trim(),
    noteText: (document.querySelector('.wwc-strongflow-history-review-note')?.innerText ?? '')
      .replace(/\s+/gu, ' ').trim(),
    expanded: document.querySelector('.wwc-strongflow-history-candidate')
      ?.getAttribute('aria-expanded') ?? null,
  }
}

globalThis.historyIdentityProbe = () => {
  const detail = detailElement()
  const candidate = document.querySelector('.wwc-strongflow-history-candidate')
  candidate?.focus()
  globalThis.__historyIdentity = {
    detail,
    evidenceFirst: document.querySelector('.wwc-strongflow-history-evidence li'),
    activityFirst: document.querySelector('.wwc-strongflow-history-runtime-activities li'),
    focus: document.activeElement,
    scrollTop: detail?.scrollTop ?? 0,
  }
  return { focusedClass: document.activeElement?.className ?? null }
}

globalThis.historyEquivalentRepublish = () => {
  const before = globalThis.__historyIdentity
  const detail = detailElement()
  if (detail !== null && detail.scrollHeight > detail.clientHeight) {
    detail.scrollTop = 24
    before.scrollTop = detail.scrollTop
  }
  // A fresh, content-equal projection object: equivalent snapshot by value.
  model.publish({ ...model.state, projection: structuredClone(projection) })
  const after = globalThis.__historyIdentity
  return {
    sameDetail: detailElement() === after.detail,
    sameEvidence: document.querySelector('.wwc-strongflow-history-evidence li') === after.evidenceFirst,
    sameActivity: document.querySelector('.wwc-strongflow-history-runtime-activities li') === after.activityFirst,
    focusPreserved: document.activeElement === after.focus,
    scrollPreserved: (detailElement()?.scrollTop ?? 0) === after.scrollTop,
    activityCount: document.querySelector('.wwc-strongflow-history-runtime-activities')?.children.length ?? 0,
  }
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

globalThis.historyTimelineArrowLeft = () => {
  const toggle = document.querySelector('.wwc-strongflow-history-toggle')
  const timelineButton = document.querySelector(
    `.wwc-strongflow-stage-list .wwc-strongflow-run-button[data-stage-run-id="${failedRunId}"]`,
  )
  timelineButton?.focus()
  timelineButton?.dispatchEvent(new KeyboardEvent('keydown', {
    key: 'ArrowLeft',
    bubbles: true,
    cancelable: true,
  }))
  return {
    expanded: toggle?.getAttribute('aria-expanded') ?? null,
    focusRun: document.activeElement?.dataset?.stageRunId ?? null,
  }
}

globalThis.historyRestoreAfterReload = () => historySnapshot()
