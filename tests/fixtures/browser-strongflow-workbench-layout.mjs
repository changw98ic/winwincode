import { mountStrongFlowPage } from '/module/strongflow-page.js'

const root = document.querySelector('[data-winwincode-client-root]')
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const candidateRef = 'refs/winwincode/candidate/browser-layout'

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
    deliveryRevision: 4,
    status: 'executing',
    ownership: {
      organizationId: 'org_00000000000000000000000001',
      workspaceId: 'wsp_00000000000000000000000001',
      projectId: 'prj_00000000000000000000000001',
      repositoryId: 'rep_00000000000000000000000001',
    },
    requirements: {
      title: 'Real browser StrongFlow workbench',
      goal: 'Keep the complete review workflow reachable at every supported width.',
    },
    tasks: [{ id: 'task:browser', title: 'Verify responsive layout', status: 'active' }],
    stages: [{ id: stageRunId, stage: 'executing', role: 'implementer', status: 'running' }],
    attention: [{ id: 'attention:browser', title: 'Review the responsive proof', status: 'open' }],
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
      agents: [{
        threadId: 'cdx_00000000000000000000000001',
        nickname: 'Browser worker',
        role: 'worker',
        status: 'running',
      }],
      agentEdges: [],
      activities: [{ activityType: 'test', status: 'running', outcome: 'observed' }],
      diffSummary: {
        changedFileCount: 3,
        additions: 20,
        deletions: 5,
        sourceRef: 'runtime:diff:browser-layout',
      },
    }],
  },
  evidence: [{
    id: 'evidence:browser',
    type: 'test',
    sourceRef: 'artifact:test:browser-layout',
    candidateRef,
  }],
  verdict: { id: 'verdict:browser', status: 'pass', producedAt: '2026-09-02T08:00:00.000Z' },
  attention: [{
    id: 'attention:browser',
    title: 'Review the responsive proof',
    status: 'open',
    type: 'decision_required',
  }],
  currentCandidate: {
    candidateRef,
    candidateCommitId: '1111111111111111111111111111111111111111',
    candidateTreeId: '2222222222222222222222222222222222222222',
    diffSha256: `sha256:${'3'.repeat(64)}`,
    frozenAt: '2026-09-02T08:00:00.000Z',
  },
  publication: { state: 'pending', revision: 1, updatedAt: '2026-09-02T08:00:00.000Z' },
  metadata: {
    source: 'control-plane-snapshot',
    updatedAt: '2026-09-02T08:00:00.000Z',
    revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 1 },
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
const mounted = mountStrongFlowPage({
  root,
  model,
  deliveries: [{
    schemaVersion: 'winwincode/v1',
    deliveryId,
    title: 'Real browser StrongFlow workbench',
    revision: 4,
    status: 'executing',
  }],
})

function visible(element) {
  if (element === null || element.hidden) return false
  const style = getComputedStyle(element)
  const rectangle = element.getBoundingClientRect()
  return style.display !== 'none' && style.visibility !== 'hidden'
    && rectangle.width > 0 && rectangle.height > 0
}

function regionSnapshot() {
  return ['navigation', 'main-region', 'context'].map(name => {
    const element = document.querySelector(`.wwc-strongflow-${name}`)
    const rectangle = element.getBoundingClientRect()
    return {
      ariaLabel: element.getAttribute('aria-label'),
      height: Math.round(rectangle.height),
      tag: element.tagName,
      visible: visible(element),
      width: Math.round(rectangle.width),
      x: Math.round(rectangle.x),
      y: Math.round(rectangle.y),
    }
  })
}

function selectedArtifact() {
  return document.querySelector('.wwc-strongflow-artifact-tab[aria-selected="true"]')
    ?.dataset.artifactTab ?? null
}

function layoutSnapshot() {
  const workspace = document.querySelector('.wwc-strongflow-workspace')
  return {
    artifact: selectedArtifact(),
    candidateVisible: visible(document.querySelector(
      '[data-artifact-tab="candidate"] .wwc-strongflow-view-candidate',
    )),
    contextCollapsed: workspace.dataset.contextCollapsed,
    contextDrawerOpen: !document.querySelector('.wwc-strongflow-context-drawer').hidden,
    contextWidth: workspace.dataset.contextWidth,
    navigationCollapsed: workspace.dataset.navigationCollapsed,
    navigationDrawerOpen: !document.querySelector('.wwc-strongflow-navigation-drawer').hidden,
    navigationWidth: workspace.dataset.navigationWidth,
    stored: JSON.parse(localStorage.getItem('winwincode.strongflow.layout.v1') ?? 'null'),
    viewport: workspace.dataset.viewport,
  }
}

function focusSnapshot() {
  const active = document.activeElement
  let region = null
  if (active instanceof Element) {
    if (active.closest('.wwc-strongflow-navigation') !== null) region = 'navigation'
    else if (active.classList.contains('wwc-strongflow-resize-navigation')) region = 'navigation'
    else if (active.closest('.wwc-strongflow-main-region') !== null) region = 'main'
    else if (active.classList.contains('wwc-strongflow-resize-context')) region = 'context'
    else if (active.closest('.wwc-strongflow-context') !== null) region = 'context'
    else if (active.closest('.wwc-strongflow-artifacts') !== null) region = 'artifacts'
  }
  return {
    className: active?.className ?? null,
    focusVisible: active?.matches(':focus-visible') ?? false,
    region,
    role: active?.getAttribute('role') ?? null,
    tag: active?.tagName ?? null,
  }
}

function waitForViewport(mode) {
  return new Promise((resolvePromise, reject) => {
    const deadline = Date.now() + 5_000
    const check = () => {
      if (layoutSnapshot().viewport === mode) {
        resolvePromise()
        return
      }
      if (Date.now() >= deadline) {
        reject(new Error(`timed out waiting for ${mode} viewport`))
        return
      }
      setTimeout(check, 20)
    }
    check()
  })
}

globalThis.runStrongFlowWideLayoutScenario = () => {
  const outerSplit = document.querySelector('.wwc-strongflow-navigation-split')
  const innerSplit = document.querySelector('.wwc-strongflow-main-context-split')
  const before = {
    layout: layoutSnapshot(),
    landmarks: {
      mainCount: document.querySelectorAll('main').length,
      workspaceTag: document.querySelector('.wwc-strongflow-workspace').tagName,
    },
    media: {
      innerWidth,
      max64: matchMedia('(max-width: 64rem)').matches,
      supported: CSS.supports(
        'grid-template-columns',
        'minmax(0, 22%) auto minmax(0, 1fr)',
      ),
      variables: [
        getComputedStyle(document.querySelector('.wwc-strongflow-workspace'))
          .getPropertyValue('--wwc-strongflow-navigation-width'),
        getComputedStyle(document.querySelector('.wwc-strongflow-workspace'))
          .getPropertyValue('--wwc-strongflow-context-width'),
      ],
    },
    regions: regionSnapshot(),
    splits: [outerSplit, innerSplit].map(element => ({
      className: element.className,
      display: getComputedStyle(element).display,
      gridTemplateColumns: getComputedStyle(element).gridTemplateColumns,
      orientation: element.dataset.orientation,
      width: Math.round(element.getBoundingClientRect().width),
    })),
  }
  const navigationResize = document.querySelector('.wwc-strongflow-resize-navigation')
  navigationResize.focus()
  navigationResize.dispatchEvent(new KeyboardEvent('keydown', {
    key: 'ArrowRight',
    bubbles: true,
    cancelable: true,
  }))
  const candidateTab = document.querySelector('[data-artifact-tab="candidate"][role="tab"]')
  candidateTab.click()
  document.querySelector('.wwc-strongflow-collapse-navigation').click()
  return {
    before,
    after: layoutSnapshot(),
    navigationHandle: {
      ariaControls: navigationResize.getAttribute('aria-controls'),
      ariaOrientation: navigationResize.getAttribute('aria-orientation'),
      ariaValueNow: navigationResize.getAttribute('aria-valuenow'),
      role: navigationResize.getAttribute('role'),
    },
  }
}

globalThis.runStrongFlowLifecycleScenario = () => {
  const comments = document.querySelector('.wwc-strongflow-solution-actions textarea')
  const attentionDraft = document.querySelector('.wwc-strongflow-attention-actions textarea')
  const taskRow = document.querySelector('.wwc-strongflow-task-list li')
  const candidate = document.querySelector('.wwc-strongflow-view-candidate')
  const diagrams = document.querySelector('.wwc-strongflow-diagrams')
  const selectedTab = document.querySelector(
    '.wwc-strongflow-artifact-tab[aria-selected="true"]',
  )
  comments.value = 'Keep this review draft'
  attentionDraft.value = 'Keep this Attention draft'
  comments.focus()
  comments.setSelectionRange(5, 11)
  window.scrollTo(0, document.documentElement.scrollHeight)
  const pageScroll = scrollY

  for (let index = 0; index < 200; index += 1) {
    model.publish({
      ...structuredClone(model.state),
      status: index % 2 === 0 ? 'refreshing' : 'ready',
    })
  }

  return {
    attentionDraft: attentionDraft.value,
    attentionIdentity: document.querySelector(
      '.wwc-strongflow-attention-actions textarea',
    ) === attentionDraft,
    candidateIdentity: document.querySelector('.wwc-strongflow-view-candidate') === candidate,
    commentsDraft: comments.value,
    commentsFocus: document.activeElement === comments,
    commentsIdentity: document.querySelector(
      '.wwc-strongflow-solution-actions textarea',
    ) === comments,
    commentsSelection: [comments.selectionStart, comments.selectionEnd],
    diagramsIdentity: document.querySelector('.wwc-strongflow-diagrams') === diagrams,
    pageScroll: [pageScroll, scrollY],
    selectedTabIdentity: document.querySelector(
      '.wwc-strongflow-artifact-tab[aria-selected="true"]',
    ) === selectedTab,
    taskIdentity: document.querySelector('.wwc-strongflow-task-list li') === taskRow,
  }
}

globalThis.startStrongFlowTabSequence = () => {
  document.querySelector('.wwc-strongflow-collapse-navigation').focus()
  return focusSnapshot()
}

globalThis.strongFlowFocusSnapshot = () => focusSnapshot()

globalThis.runStrongFlowRestoredLayoutScenario = () => layoutSnapshot()

globalThis.runStrongFlowBreakpointScenario = async () => {
  await waitForViewport('narrow')
  return {
    layout: layoutSnapshot(),
    media: {
      innerWidth,
      max64: matchMedia('(max-width: 64rem)').matches,
    },
    navigationInDrawer: document.querySelector(
      '.wwc-strongflow-navigation-drawer .wwc-strongflow-delivery-list',
    ) !== null,
    contextInDrawer: document.querySelector(
      '.wwc-strongflow-context-drawer .wwc-strongflow-attention-list',
    ) !== null,
    resizeHandlesHidden: [
      document.querySelector('.wwc-strongflow-resize-navigation').hidden,
      document.querySelector('.wwc-strongflow-resize-context').hidden,
    ],
  }
}

globalThis.runStrongFlowNarrowLayoutScenario = async () => {
  await waitForViewport('narrow')
  const openNavigation = document.querySelector('.wwc-strongflow-open-navigation')
  const openContext = document.querySelector('.wwc-strongflow-open-context')
  openNavigation.focus()
  openNavigation.click()
  const navigationDrawer = document.querySelector('.wwc-strongflow-navigation-drawer')
  const navigationOpen = {
    activeClass: document.activeElement?.className ?? null,
    drawerVisible: visible(navigationDrawer),
    role: navigationDrawer.getAttribute('role'),
    taskVisible: visible(navigationDrawer.querySelector('.wwc-strongflow-task-list')),
  }
  navigationDrawer.dispatchEvent(new KeyboardEvent('keydown', {
    key: 'Escape',
    bubbles: true,
    cancelable: true,
  }))
  const afterEscape = {
    drawerHidden: navigationDrawer.hidden,
    focusReturned: document.activeElement === openNavigation,
  }
  openContext.click()
  const contextDrawer = document.querySelector('.wwc-strongflow-context-drawer')
  return {
    afterEscape,
    approvalVisible: visible(document.querySelector('.wwc-strongflow-approve-solution')),
    candidateVisible: visible(document.querySelector(
      '[data-artifact-tab="candidate"] .wwc-strongflow-view-candidate',
    )),
    contextOpen: {
      attentionVisible: visible(contextDrawer.querySelector('.wwc-strongflow-attention-list')),
      drawerVisible: visible(contextDrawer),
      evidenceVisible: visible(contextDrawer.querySelector('.wwc-strongflow-evidence')),
    },
    layout: layoutSnapshot(),
    navigationOpen,
    resizeHandlesHidden: [
      document.querySelector('.wwc-strongflow-resize-navigation').hidden,
      document.querySelector('.wwc-strongflow-resize-context').hidden,
    ],
  }
}

globalThis.closeStrongFlowWorkbenchFixture = () => { mounted.close() }
