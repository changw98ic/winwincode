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
    stage: { id: stageRunId },
    runtime: {
      stageRunId,
      sessions: [{
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
    interaction: { status: 'idle', error: null },
    error: null,
  }
}

class BrowserStrongFlowModel {
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
  async decideSolutionReview() {}
  async approveTaskBreakdown() {}
  async resolveAttention() {}
  async submitVerdict() {}
  async advanceDelivery() {}
  cancelPending() {}
  reconnect() {}
  close() {}
}

const model = new BrowserStrongFlowModel()
const mounted = mountStrongFlowPage({ root, model })

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
  model.publish(ready(createProjection({ candidateDigest: '4', runtimeSource: '2' })))
  return {
    afterEquivalentEvents,
    candidateChange: {
      candidateRebuilt: candidateAfterChange !== candidateBefore,
      diagramsRetained: diagramsAfterCandidate === diagramsBefore,
    },
    duringReload,
    runtimeChange: {
      candidateRetained: document.querySelector('.wwc-strongflow-view-candidate')
        === candidateAfterChange,
      diagramsRebuilt: document.querySelector('.wwc-strongflow-diagrams')
        !== diagramsAfterCandidate,
    },
  }
}

globalThis.closeStrongFlowEventReloadFixture = () => { mounted.close() }
