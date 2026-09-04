// UI-608 component-state visual fixture.
//
// Mounts the real UI primitives — plus the Candidate Diff viewer — in every
// state the decision names, and reports one visual fingerprint per state.  The
// mounts are the product's own mount functions, so what the baseline pins is
// what ships.  Determinism comes from the harness, not from the components: a
// fixed font stack, no transitions or animations, no scrollbars, and fixture
// data with fixed identifiers and fixed timestamps.

import {
  captureVisualFingerprint,
  compareVisualFingerprints,
  renderVisualRegressionReport,
  VISUAL_REGRESSION_FONT_STACK,
} from '/module/visual-regression.js'

import { mountButton } from '/module/components/button.js'
import { mountStatusBadge } from '/module/components/status-badge.js'
import { mountPageHeader } from '/module/components/page-header.js'
import { mountPanel } from '/module/components/panel.js'
import { mountMetric } from '/module/components/metric.js'
import { mountFormField } from '/module/components/form-field.js'
import { mountEmptyState } from '/module/components/empty-state.js'
import { mountErrorState } from '/module/components/error-state.js'
import { mountTabs } from '/module/components/tabs.js'
import { mountToolbar } from '/module/components/toolbar.js'
import { mountDrawer } from '/module/components/drawer.js'
import { mountSplitPane } from '/module/components/split-pane.js'
import { mountActionBar } from '/module/components/action-bar.js'
import { mountConnectionBar } from '/module/components/connection-bar.js'
import { mountClientErrorBoundary } from '/module/components/client-error-boundary.js'
import { mountWindowedList } from '/module/components/windowed-list.js'
import { mountCandidateDiffViewer } from '/module/strongflow-diff-viewer.js'
import { mountStrongFlowDeliveryList } from '/module/strongflow-delivery-list-page.js'

const FONT_OVERRIDE = `
*, *::before, *::after {
  animation: none !important;
  transition: none !important;
  caret-color: transparent !important;
}
html { scrollbar-width: none; }
::-webkit-scrollbar { display: none; }
`

const VIEWPORT = Object.freeze({ width: 1280, height: 800 })

const schemaVersion = 'winwincode/v1'
const fixedScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const fixedTime = '2026-09-02T01:00:00.000Z'

function connectionState(status) {
  return {
    status,
    code: status === 'connected' ? null : `connection.${status}`,
    requestId: status === 'connected' ? null : 'req_00000000000000000000000001',
    lastSuccessfulAt: status === 'connected' ? fixedTime : null,
    revision: 1,
  }
}

function failure(category, code, title, message, connectionStatus) {
  return {
    category,
    code,
    requestId: 'req_00000000000000000000000002',
    retryable: true,
    connectionStatus,
    title,
    message,
    recoveryLabel: 'Retry route',
  }
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

// ---------------------------------------------------------------------------
// Gallery entries.  Every entry mounts the production component and returns the
// element the fingerprint is taken from.
// ---------------------------------------------------------------------------

const buttonVariants = Object.freeze([
  ['default', {}],
  ['primary', { variant: 'primary' }],
  ['destructive', { variant: 'destructive' }],
  ['ghost', { variant: 'ghost' }],
  ['busy', { variant: 'primary', busy: true, busyLabel: 'Publishing…' }],
  ['disabled', { variant: 'primary', disabled: true }],
])

const badgeTones = Object.freeze([
  ['neutral', 'Draft'],
  ['info', 'Clarifying'],
  ['success', 'Published'],
  ['warning', 'Attention'],
  ['danger', 'Failed'],
])

const connectionStates = Object.freeze([
  ['connected', 'connected'],
  ['reconnecting', 'reconnecting'],
  ['offline', 'offline'],
  ['authentication-required', 'authentication-required'],
])

const ENTRIES = Object.freeze([

  // Buttons: default, busy, disabled, and the destructive variant.
  ...buttonVariants.map(([name, variant]) => [`button/${name}`, document => {
    const view = mountButton({
      document,
      props: {
        label: name === 'busy' ? 'Publish candidate' : 'Publish candidate',
        ...variant,
      },
    })
    register(view)
    return view.root
  }]),

  // Status badges: the delivery tones every stage and state maps onto.
  ...badgeTones.map(([tone, label]) => [`status-badge/${tone}`, document => {
    const view = mountStatusBadge({ document, props: { label, tone } })
    register(view)
    return view.root
  }]),

  ['page-header/full', document => {
    const view = mountPageHeader({
      document,
      props: {
        eyebrow: 'Repository scope',
        title: 'Provider settings',
        description: 'One model route for this repository.',
      },
    })
    register(view)
    return view.root
  }],

  ['panel/default', document => {
    const view = mountPanel({
      document,
      props: { id: 'visual-panel', title: 'Credential references', description: 'Reference only.' },
    })
    view.update({
      id: 'visual-panel',
      title: 'Credential references',
      description: 'Reference only.',
      content: textParagraph(document, 'No Credential references.'),
    })
    register(view)
    return view.root
  }],

  ['panel/busy', document => {
    const view = mountPanel({
      document,
      props: { id: 'visual-panel-busy', title: 'Credential references', busy: true },
    })
    view.update({
      id: 'visual-panel-busy',
      title: 'Credential references',
      busy: true,
      content: textParagraph(document, 'Loading Credential references.'),
    })
    register(view)
    return view.root
  }],

  ['metric/default', document => {
    const view = mountMetric({
      document,
      props: { label: 'Open attention', value: '3', hint: 'Across this repository' },
    })
    register(view)
    return view.root
  }],

  ['metric/error', document => {
    const view = mountMetric({
      document,
      props: { label: 'Failed stages', value: '1', tone: 'danger' },
    })
    register(view)
    return view.root
  }],

  ['form-field/default', document => {
    const control = document.createElement('input')
    control.type = 'text'
    control.value = 'Browser model'
    const view = mountFormField({
      document,
      props: { id: 'visual-field', label: 'Display name', control, help: 'Shown in the route list.' },
    })
    register(view)
    return view.root
  }],

  ['form-field/error', document => {
    const control = document.createElement('input')
    control.type = 'text'
    control.value = ''
    const view = mountFormField({
      document,
      props: {
        id: 'visual-field-error',
        label: 'Display name',
        control,
        error: 'A display name is required.',
        required: true,
      },
    })
    register(view)
    return view.root
  }],

  ['empty-state/default', document => {
    const view = mountEmptyState({
      document,
      props: { title: 'No Credential references', detail: 'Add one to route this model.' },
    })
    register(view)
    return view.root
  }],

  ['error-state/default', document => {
    const view = mountErrorState({
      document,
      props: {
        title: 'Provider settings unavailable',
        message: 'The Server rejected settings.get.',
        detail: 'Error code: settings.read denied · Request ID: req_00000000000000000000000003',
      },
    })
    register(view)
    return view.root
  }],

  ['client-error-boundary/error', document => {
    const view = mountClientErrorBoundary({
      document,
      props: {
        failure: failure(
          'server',
          'delivery.read_failed',
          'StrongFlow stopped unexpectedly',
          'Retry this route or return to Chat.',
          'refresh-required',
        ),
        diagnostic: 'WWC diagnostic fixture text with no credential material.',
        onRetry() {},
        onSafeEntry() {},
        onCopy() {},
      },
    })
    register(view)
    return view.root
  }],

  ['tabs/selected-disabled-and-unselected', document => {
    const view = mountTabs({
      document,
      props: {
        id: 'visual-tabs',
        label: 'Diff layout',
        tabs: [
          { id: 'unified', label: 'Unified', panelId: 'visual-panel-unified' },
          { id: 'split', label: 'Side by side', panelId: 'visual-panel-split' },
          { id: 'images', label: 'Images', panelId: 'visual-panel-images', disabled: true },
        ],
        selectedId: 'unified',
        onSelect() {},
      },
    })
    register(view)
    return view.root
  }],

  ['toolbar/with-disabled-item', document => {
    const first = mountButton({
      document,
      props: { label: 'Expand all', variant: 'ghost', className: 'wwc-toolbar-item' },
    })
    const second = mountButton({
      document,
      props: { label: 'Collapse all', variant: 'ghost', className: 'wwc-toolbar-item', disabled: true },
    })
    const view = mountToolbar({ document, props: { label: 'Diff tools', items: [first.root, second.root] } })
    register(first, second, view)
    return view.root
  }],

  ['action-bar/space-between', document => {
    const cancel = mountButton({
      document,
      props: { label: 'Cancel', variant: 'ghost', className: 'wwc-action-item' },
    })
    const confirm = mountButton({
      document,
      props: { label: 'Approve rework', variant: 'primary', className: 'wwc-action-item' },
    })
    const view = mountActionBar({
      document,
      props: { label: 'Rework decision', items: [cancel.root, confirm.root], align: 'space-between' },
    })
    register(cancel, confirm, view)
    return view.root
  }],

  ['drawer/open', document => {
    const view = mountDrawer({
      document,
      props: {
        id: 'visual-drawer',
        title: 'Evidence',
        open: true,
        content: textParagraph(document, 'Execution log for the selected stage run.'),
        onClose() {},
      },
    })
    register(view)
    return view.root
  }],

  ['drawer/closed', document => {
    const view = mountDrawer({
      document,
      props: {
        id: 'visual-drawer-closed',
        title: 'Evidence',
        open: false,
        content: textParagraph(document, 'Execution log for the selected stage run.'),
        onClose() {},
      },
    })
    register(view)
    return view.root
  }],

  ['split-pane/both-panes', document => {
    const view = mountSplitPane({
      document,
      props: {
        primary: textSection(document, 'Diff'),
        primaryLabel: 'Candidate diff',
        secondary: textSection(document, 'Evidence'),
        secondaryLabel: 'Stage evidence',
      },
    })
    register(view)
    return view.root
  }],

  ['split-pane/secondary-hidden', document => {
    const view = mountSplitPane({
      document,
      props: {
        primary: textSection(document, 'Diff only'),
        primaryLabel: 'Candidate diff',
        secondary: textSection(document, 'Evidence'),
        secondaryLabel: 'Stage evidence',
        secondaryHidden: true,
      },
    })
    register(view)
    return view.root
  }],

  // Connection states: the offline and stale presentations the shell shows.
  ...connectionStates.map(([name, status]) => [`connection-bar/${name}`, document => {
    const view = mountConnectionBar({
      document,
      props: {
        state: connectionState(status),
        diagnostic: `WWC connection fixture ${status}; no credential material.`,
        onRecover() {},
        onCopy() {},
      },
    })
    register(view)
    return view.root
  }]),

  // The Candidate Diff viewer: unified, empty, and error presentations.
  ['strongflow-diff/unified', document => {
    const view = mountCandidateDiffViewer({
      document,
      onLoadMoreDiff() {},
      onViewModeChange() {},
    })
    view.update({
      diff: diffState(),
      selectedPath: 'src/app.ts',
      viewMode: 'unified',
      candidateDigest: `sha256:${'3'.repeat(64)}`,
      selectedLine: null,
    })
    register(view)
    return view.root
  }],

  ['strongflow-diff/no-selection', document => {
    const view = mountCandidateDiffViewer({
      document,
      onLoadMoreDiff() {},
      onViewModeChange() {},
    })
    view.update({
      diff: diffState({ status: 'idle', path: null, content: null, fileDiffSha256: null }),
      selectedPath: null,
      viewMode: 'unified',
      candidateDigest: `sha256:${'3'.repeat(64)}`,
      selectedLine: null,
    })
    register(view)
    return view.root
  }],

  ['strongflow-diff/error', document => {
    const view = mountCandidateDiffViewer({
      document,
      onLoadMoreDiff() {},
      onViewModeChange() {},
    })
    view.update({
      diff: diffState({
        status: 'error',
        path: 'src/app.ts',
        content: null,
        fileDiffSha256: null,
        error: { code: 'candidate.diff_unavailable', message: 'The Candidate Diff is unavailable.' },
      }),
      selectedPath: 'src/app.ts',
      viewMode: 'unified',
      candidateDigest: `sha256:${'3'.repeat(64)}`,
      selectedLine: null,
    })
    register(view)
    return view.root
  }],

  ['strongflow-delivery-list/list', document => {
    const root = document.createElement('section')
    const page = mountStrongFlowDeliveryList({ root, model: deliveryListModel(), view: 'list' })
    register(page)
    return root
  }],

  ['strongflow-delivery-list/kanban', document => {
    const root = document.createElement('section')
    const page = mountStrongFlowDeliveryList({ root, model: deliveryListModel(), view: 'kanban' })
    register(page)
    return root
  }],

  ['strongflow-delivery-list/error', document => {
    const root = document.createElement('section')
    const page = mountStrongFlowDeliveryList({
      root,
      model: deliveryListModel({
        status: 'error',
        visible: [],
        error: {
          kind: 'server',
          code: 'delivery.list_failed',
          message: 'The Delivery list could not be loaded.',
          requestId: 'req_00000000000000000000000004',
        },
      }),
      view: 'list',
    })
    register(page)
    return root
  }],

  ['windowed-list/window', document => {
    const scroller = document.createElement('div')
    scroller.className = 'wwc-visual-scroller'
    const content = document.createElement('div')
    content.className = 'wwc-visual-rows'
    const view = mountWindowedList({
      document,
      scroller,
      content,
      key: item => item.id,
      create: () => document.createElement('div'),
      update: (node, item) => {
        node.className = 'wwc-visual-row'
        node.textContent = item.label
      },
      rowHeight: 60,
      viewportRows: 3,
      overscan: 1,
    })
    view.update(Array.from({ length: 40 }, (_, index) => ({
      id: `row_${String(index + 1).padStart(2, '0')}`,
      label: `Delivery ${String(index + 1)}`,
    })))
    register(view)
    return view.root
  }],
])

// ---------------------------------------------------------------------------
// StrongFlow Delivery list.  The stages a Delivery moves through are pinned
// through the real list and Kanban views, driven by a ready model state.
// ---------------------------------------------------------------------------

const STAGES = Object.freeze(['draft', 'clarifying', 'executing', 'reviewing', 'verifying', 'delivered', 'failed'])

const stageDeliveries = STAGES.map((status, index) => ({
  deliveryId: `dlv_${String(index + 1).padStart(26, '0')}`,
  revision: 4,
  schemaVersion,
  status,
  title: `Delivery ${String(index + 1)}`,
  updatedAt: fixedTime,
  ownership: fixedScope,
  activeStageRunId: status === 'draft' || status === 'delivered' || status === 'failed'
    ? null
    : `str_${String(index + 1).padStart(26, '0')}`,
  openAttentionCount: status === 'clarifying' || status === 'executing' ? 1 : 0,
  taskCounts: {
    total: 4,
    pending: 1,
    active: status === 'executing' ? 1 : 0,
    blocked: 0,
    verifying: status === 'verifying' ? 1 : 0,
    completed: 1,
    failed: status === 'failed' ? 2 : 0,
  },
}))

/** A ready list model.  The gallery changes no state, so nothing else is needed. */
function deliveryListModel(overrides = {}) {
  return {
    state: {
      status: 'ready',
      filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
      visible: stageDeliveries,
      loadedCount: stageDeliveries.length,
      hasMore: false,
      loadingMore: false,
      moreFailure: null,
      error: null,
      advance: { deliveryId: null, failure: null },
      ...overrides,
    },
    subscribe() { return () => {} },
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
}

function textParagraph(document, text) {  const node = document.createElement('p')
  node.textContent = text
  return node
}

function textSection(document, text) {
  const section = document.createElement('section')
  const heading = document.createElement('h3')
  heading.textContent = text
  section.append(heading, textParagraph(document, `${text} content.`))
  return section
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

const mountedViews = []
function register(...views) {
  mountedViews.push(...views)
}

// The harness serves a `style-src 'self'` policy, so the determinism controls
// are installed through CSSOM, which that policy does not block.
const determinism = new CSSStyleSheet()
determinism.replaceSync(FONT_OVERRIDE)
document.adoptedStyleSheets = [...document.adoptedStyleSheets, determinism]
for (const token of ['--wwc-font-family', '--wwc-font-family-mono']) {
  document.documentElement.style.setProperty(token, VISUAL_REGRESSION_FONT_STACK)
}

const shell = document.createElement('div')
shell.className = 'wwc-shell'
const main = document.createElement('main')
main.className = 'wwc-main'
main.id = 'wwc-visual-gallery'
shell.append(main)
document.body.replaceChildren(shell)

async function captureEntry(id, mount) {
  const host = document.createElement('section')
  host.className = 'wwc-visual-host'
  main.replaceChildren(host)
  const root = mount(document)
  host.append(root)
  // Force one layout so boxes are measured after the mount, not during it.
  void host.getBoundingClientRect()
  const fingerprint = captureVisualFingerprint({
    document,
    root,
    id,
    kind: 'component',
    viewport: { ...VIEWPORT },
    fontStack: VISUAL_REGRESSION_FONT_STACK,
  })
  return fingerprint
}

globalThis.captureComponentStates = async () => {
  await document.fonts.ready
  const fingerprints = []
  for (const [id, mount] of ENTRIES) {
    fingerprints.push(await captureEntry(id, mount))
  }
  for (const view of mountedViews.splice(0)) view.close()
  main.replaceChildren()
  return {
    schemaVersion: 1,
    viewport: { ...VIEWPORT },
    fontStack: VISUAL_REGRESSION_FONT_STACK,
    fingerprints,
    // One JSON document, so the credential gate can scan the capture directly.
    capturedText: JSON.stringify(fingerprints),
    entryIds: ENTRIES.map(([id]) => id),
  }
}

globalThis.inspectGalleryReadiness = () => ({
  entries: ENTRIES.length,
  stylesheetRules: document.styleSheets.length,
  fontOverride: getComputedStyle(document.documentElement)
    .getPropertyValue('--wwc-font-family')
    .trim(),
})

/** Compares one capture against the committed baseline and renders the report. */
globalThis.compareComponentStates = (baseline, fingerprints) => {
  const committed = new Map(baseline.map(entry => [entry.id, entry]))
  const differences = fingerprints.flatMap(fingerprint => {
    const expected = committed.get(fingerprint.id)
    return expected === undefined
      ? [{
          reason: 'unexpected-node',
          path: fingerprint.id,
          property: 'presence',
          baseline: null,
          actual: fingerprint.id,
        }]
      : compareVisualFingerprints(expected, fingerprint)
  })
  return {
    differences,
    report: renderVisualRegressionReport(differences, { id: 'component-states' }),
  }
}
