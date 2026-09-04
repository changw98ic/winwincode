import { mountStrongFlowPage } from '/module/strongflow-page.js'

const root = document.querySelector('[data-winwincode-client-root]')
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const candidateRef = 'refs/winwincode/candidate/browser-event'

function diagram(kind) {
  return {
    id: `diagram:${kind}`,
    kind,
    title: `${kind} diagram`,
    nodes: [{
      id: `node:${kind}`,
      label: `${kind} node`,
      description: 'Browser event reload proof',
      kind: 'component',
      trustBoundary: null,
      unresolved: false,
    }],
    edges: [],
  }
}

function createProjection({ candidateDigest = '3', runtimeSource = '1' } = {}) {
  return {
    delivery: {
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
        title: 'StrongFlow event reload',
        goal: 'Retain review work while the canonical snapshot reloads.',
      },
      tasks: [{ id: 'task:browser', title: 'Retain the draft', status: 'active' }],
      stages: [{ id: stageRunId, stage: 'executing', role: 'implementer', status: 'running' }],
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
        deliveryTaskId: 'task:browser',
        attempt: 1,
        asOfSequence: 1,
        agents: [],
        agentEdges: [],
        activities: [],
        diffSummary: {
          changedFileCount: 1,
          additions: 2,
          deletions: 0,
          sourceRef: `runtime:diff:${runtimeSource}`,
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
      id: 'evidence:browser',
      type: 'test',
      sourceRef: 'artifact:test:browser-event',
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
      revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 0 },
      readCursor: {},
    },
  }
}

function ready(projection = createProjection()) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    projection,
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
  }
}

class BrowserStrongFlowModel {
  draftScope = '["browser-strongflow-actor","browser-strongflow-scope"]'
  state = ready()

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
  async loadCandidateFiles() {}
  async loadMoreCandidateFiles() {}
  async selectCandidateFile() {}
  async loadMoreCandidateDiff() {}
  async decideSolutionReview(input) {
    this.calls.push(['decideSolutionReview', structuredClone(input)])
  }
  async approveTaskBreakdown() {}
  async resolveAttention() {}
  async submitVerdict() {}
  async advanceDelivery() {}
  cancelPending() {}
  reconnect() {}
  close() {}

  calls = []
}

const model = new BrowserStrongFlowModel()
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
const mounted = mountStrongFlowPage({ root, model, deliveryList })

globalThis.runStrongFlowEventReloadScenario = () => {
  const comments = document.querySelector('.wwc-strongflow-solution-actions textarea')
  const changes = document.querySelectorAll('.wwc-strongflow-solution-actions textarea')[1]
  const candidateBefore = document.querySelector('.wwc-strongflow-view-candidate')
  const diagramsBefore = document.querySelector('.wwc-strongflow-diagrams')
  comments.value = 'Review draft retained in Chrome'
  changes.value = 'Requested changes retained in Chrome'
  comments.selectionStart = 7
  comments.selectionEnd = 7
  comments.focus()

  model.publish({
    status: 'refreshing',
    realtime: 'reloading',
    projection: model.state.projection,
    candidateFiles: model.state.candidateFiles,
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const duringReload = {
    candidateRetained: document.querySelector('.wwc-strongflow-view-candidate') === candidateBefore,
    changesDraft: changes.value,
    currentDeliveryRetained: document.querySelector('[data-delivery-id]')
      ?.getAttribute('aria-current') === 'page',
    diagramsRetained: document.querySelector('.wwc-strongflow-diagrams') === diagramsBefore,
    draft: comments.value,
    reviewDisabled: document.querySelector('.wwc-strongflow-approve-solution').disabled,
  }
  for (let index = 0; index < 50; index += 1) model.publish(ready())
  const afterEquivalentEvents = {
    candidateRetained: document.querySelector('.wwc-strongflow-view-candidate') === candidateBefore,
    changesDraft: changes.value,
    diagramsRetained: document.querySelector('.wwc-strongflow-diagrams') === diagramsBefore,
    draft: comments.value,
    focused: document.activeElement === comments,
    selectionStart: comments.selectionStart,
  }

  model.publish(ready(createProjection({ candidateDigest: '4' })))
  const candidateAfterChange = document.querySelector('.wwc-strongflow-view-candidate')
  const diagramsAfterCandidate = document.querySelector('.wwc-strongflow-diagrams')
  const executionAfterCandidate = document.querySelector('.wwc-strongflow-execution-session')
  model.publish(ready(createProjection({ candidateDigest: '4', runtimeSource: '2' })))
  const runtimeChange = {
    candidateRetained: document.querySelector('.wwc-strongflow-view-candidate')
      === candidateAfterChange,
    diagramsRetained: document.querySelector('.wwc-strongflow-diagrams')
      === diagramsAfterCandidate,
    executionSessionRetained: document.querySelector('.wwc-strongflow-execution-session')
      === executionAfterCandidate,
  }

  comments.value = 'conflicted review draft'
  comments.dispatchEvent(new Event('input', { bubbles: true }))
  const conflicted = structuredClone(createProjection({ candidateDigest: '4', runtimeSource: '2' }))
  conflicted.metadata.revisions.delivery = 5
  conflicted.delivery.deliveryRevision = 5
  model.publish(ready(conflicted))
  const revisionConflict = {
    icon: document.querySelector('.wwc-strongflow-review-conflict [aria-hidden="true"]')
      !== null,
    visible: !document.querySelector('.wwc-strongflow-review-conflict').hidden,
  }
  document.querySelector('.wwc-strongflow-review-keep-draft').click()

  comments.value = 'accepted review decision'
  comments.dispatchEvent(new Event('input', { bubbles: true }))
  document.querySelector('.wwc-strongflow-approve-solution').click()
  const acceptedCall = model.calls.at(-1)
  model.publish({
    status: 'ready',
    realtime: 'subscribed',
    projection: conflicted,
    candidateFiles: model.state.candidateFiles,
    interaction: { status: 'waiting', error: null },
    error: null,
  })
  model.publish({
    status: 'refreshing',
    realtime: 'reloading',
    projection: conflicted,
    candidateFiles: model.state.candidateFiles,
    interaction: { status: 'idle', error: null },
    error: null,
  })
  model.publish(ready(conflicted))
  const acceptedReview = {
    approveDisabled: document.querySelector('.wwc-strongflow-approve-solution').disabled,
    decision: acceptedCall,
    submittedOnce: model.calls
      .filter(([name]) => name === 'decideSolutionReview').length === 1,
  }

  comments.value = 'draft for the next candidate'
  comments.dispatchEvent(new Event('input', { bubbles: true }))
  const sameDigestNewIdentity = createProjection({ candidateDigest: '4', runtimeSource: '2' })
  sameDigestNewIdentity.currentCandidate.candidateRef = 'refs/winwincode/candidate/browser-event-2'
  sameDigestNewIdentity.currentCandidate.candidateCommitId = '9999999999999999999999999999999999999999'
  model.publish(ready(sameDigestNewIdentity))
  const identityChange = { draftReset: comments.value === '' }

  return {
    acceptedReview,
    afterEquivalentEvents,
    candidateChange: {
      candidateRetained: candidateAfterChange === candidateBefore,
      diagramsRetained: diagramsAfterCandidate === diagramsBefore,
    },
    duringReload,
    identityChange,
    revisionConflict,
    runtimeChange,
  }
}

globalThis.closeStrongFlowEventReloadFixture = () => { mounted.close() }
