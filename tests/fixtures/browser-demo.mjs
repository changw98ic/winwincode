// UI-608 key-page visual fixture.
//
// Mounts the one browser shell over a deterministic Control Plane facade and
// reports a visual fingerprint of each key page, at whatever viewport the
// harness has set.  The route in the URL selects which slice of fixture data is
// served, so one fixture file covers the populated pages and the empty Chat
// without re-mounting mid-capture.
//
// Determinism: the shell receives a fixed clock, every served timestamp and
// identifier is a literal, the font stack is pinned, and transitions,
// animations, and scrollbars are switched off.

import {
  captureVisualFingerprint,
  compareVisualFingerprints,
  renderVisualRegressionReport,
  VISUAL_REGRESSION_FONT_STACK,
} from '/module/visual-regression.js'
import { mountWinWinCodeClient } from '/module/application.js'

const DETERMINISM_CSS = `
*, *::before, *::after {
  animation: none !important;
  transition: none !important;
  caret-color: transparent !important;
}
html { scrollbar-width: none; }
::-webkit-scrollbar { display: none; }
`

const DESKTOP_VIEWPORT = Object.freeze({ width: 1280, height: 900 })
const NARROW_VIEWPORT = Object.freeze({ width: 420, height: 900 })

const FIXED_NOW = '2026-09-02T01:00:00.000Z'

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const identity = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
}
const scope = {
  kind: 'repository',
  ...identity,
  repositoryId: 'rep_00000000000000000000000001',
}
const productSessionId = 'psn_00000000000000000000000001'
const credentialReferenceId = 'crd_00000000000000000000000001'
const modelRoute = {
  providerId: 'visual-provider',
  modelId: 'visual-model',
  credentialReferenceId,
}

/** Which slice of fixture data this page load serves, taken from the route. */
const route = location.hash.replace(/^#/u, '')
const MODE = route === '/chat-empty' ? 'empty-chat' : 'populated'
const emptyChat = MODE === 'empty-chat'

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function response(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: page(),
  }
}

function ownership() {
  return { ...identity, repositoryId: scope.repositoryId }
}

function deliverySummary(index, overrides = {}) {
  return {
    deliveryId: canonicalId('dlv', index),
    revision: 4,
    schemaVersion,
    status: 'executing',
    title: `Delivery ${String(index)}`,
    updatedAt: '2026-09-02T00:30:00.000Z',
    ownership: ownership(),
    activeStageRunId: canonicalId('run', index),
    openAttentionCount: 1,
    taskCounts: { total: 4, pending: 0, active: 1, blocked: 0, verifying: 0, completed: 1, failed: 2 },
    ...overrides,
  }
}

const deliveries = [
  deliverySummary(1, { status: 'draft', activeStageRunId: null, openAttentionCount: 0 }),
  deliverySummary(2, { status: 'clarifying' }),
  deliverySummary(3, { status: 'executing' }),
  deliverySummary(4, { status: 'plan-review', openAttentionCount: 0 }),
  deliverySummary(5, { status: 'verifying', openAttentionCount: 0 }),
  deliverySummary(6, { status: 'delivered', activeStageRunId: null, openAttentionCount: 0, taskCounts: {
    total: 4, pending: 0, active: 0, blocked: 0, verifying: 0, completed: 4, failed: 0,
  } }),
  deliverySummary(7, { status: 'needs-attention', activeStageRunId: null, taskCounts: {
    total: 4, pending: 0, active: 0, blocked: 0, verifying: 0, completed: 2, failed: 2,
  } }),
]

function chatSession() {
  return {
    id: productSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision: 3,
    state: 'idle',
    title: 'Visual fixture Chat',
    updatedAt: '2026-09-02T00:30:00.000Z',
  }
}

function chatMessages() {
  return [{
    id: canonicalId('msg', 1),
    productSessionId,
    role: 'user',
    content: 'Deliver the visual fixture requirement.',
    sequence: 1,
    state: 'completed',
    createdAt: '2026-09-02T00:30:00.000Z',
    updatedAt: '2026-09-02T00:30:00.000Z',
  }, {
    id: canonicalId('msg', 2),
    productSessionId,
    role: 'assistant',
    content: 'The requirement is confirmed and ready for StrongFlow.',
    sequence: 2,
    state: 'completed',
    createdAt: '2026-09-02T00:31:00.000Z',
    updatedAt: '2026-09-02T00:31:00.000Z',
  }]
}


function approval(index) {
  return {
    id: canonicalId('apr', index),
    revision: 5,
    state: 'pending',
    requestedAt: '2026-09-02T00:40:00.000Z',
    expiresAt: '2099-09-02T00:00:00.000Z',
    subject: `Allow the projected repository action ${String(index)}`,
    category: 'shell',
    effectiveDecisionScope: 'once',
    sanitizedDetail: { kind: 'unavailable', reason: 'source_not_recorded' },
    binding: {
      productSessionId,
      executionJobId: canonicalId('job', index),
      workerSessionId: canonicalId('wsn', index),
      sessionIdentity: {
        productSessionId,
        workerSessionId: canonicalId('wsn', index),
        codexThreadId: canonicalId('cdx', index),
        stageRunId: canonicalId('run', index),
      },
    },
  }
}

function worker() {
  return {
    id: canonicalId('wrk', 1),
    state: 'enabled',
    capacity: 2,
    lastHeartbeatAt: '2026-09-02T00:59:00.000Z',
    revision: 1,
  }
}

function credentialReference() {
  return {
    id: credentialReferenceId,
    providerId: modelRoute.providerId,
    displayName: 'Visual model credential',
    secretState: 'available',
    rotationVersion: 1,
    lastRotatedAt: '2026-09-02T00:00:00.000Z',
    revokedAt: null,
    revision: 1,
    updatedAt: '2026-09-02T00:00:00.000Z',
  }
}

function routeAvailability() {
  return {
    kind: 'model_route_availability_page',
    scope,
    settingsSource: scope,
    settingsRevision: 1,
    requestPoolSource: { kind: 'project', ...identity },
    requestPoolRevision: 1,
    defaultProviderId: modelRoute.providerId,
    defaultModelId: modelRoute.modelId,
    status: 'enabled',
    reason: 'ready',
    items: [{
      route: modelRoute,
      providerDisplayName: 'Visual Provider',
      modelDisplayName: 'Visual Model',
      catalogSource: scope,
      catalogVersion: 1,
      providerVersion: 1,
      modelVersion: 1,
      contextWindowTokens: 128_000,
      maxOutputTokens: 16_000,
      toolSupport: 'parallel',
      reasoningEfforts: ['medium', 'high'],
      credentialRotationVersion: 1,
      isDefault: true,
      status: 'enabled',
      reason: 'ready',
    }],
  }
}

function chatRuntime() {
  return {
    kind: 'runtime_projection',
    productSessionId,
    deliveryId: null,
    stageRunId: null,
    readCursor: null,
    eventCursor: {
      eventId: null,
      sequence: 0,
      scope,
      stream: { kind: 'product-session', productSessionId },
    },
    lastProjectionSequence: 0,
    revision: 1,
    rebuiltAt: '2026-09-02T00:31:00.000Z',
    sessions: [],
  }
}

function attentionItem(delivery) {
  const index = Number.parseInt(delivery.deliveryId.slice(-2), 10)
  return {
    id: canonicalId('att', index),
    deliverySpecId: `demo/spec-${String(index)}`,
    stageRunId: canonicalId('run', index),
    type: 'blocking_note',
    title: `Delivery ${String(index)} needs a review decision`,
    options: [],
    assignedTo: null,
    blocking: true,
    status: 'open',
    resolutionSummary: null,
    resolvedBy: null,
    createdAt: '2026-09-02T00:35:00.000Z',
    resolvedAt: null,
  }
}

function deliveryDetail(delivery) {
  return {
    deliveryId: delivery.deliveryId,
    deliveryRevision: delivery.revision,
    ownership: delivery.ownership,
    attention: delivery.openAttentionCount > 0 ? [attentionItem(delivery)] : [],
    currentCandidate: null,
    requirements: {
      repository: { kind: 'local-git', locator: 'workspace://repository' },
    },
    internalToolPayload: null,
  }
}

/** Every query a failing route needs in order to fail. */
function serve(request) {
  console.info('[demo] query', request.query)
  const result = () => {
    if (request.query === 'delivery.list') {
      return { kind: 'delivery_page', items: deliveries }
    }
    if (request.query === 'delivery.get') {
      const delivery = deliveries.find(
        item => item.deliveryId === request.parameters.deliveryId,
      )
      return delivery === undefined ? null : deliveryDetail(delivery)
    }
    if (request.query === 'enterprise.organization.list') {
      return {
        kind: 'enterprise_organization_page',
        snapshotRevision: 1,
        items: [{
          id: identity.organizationId,
          slug: 'visual-organization',
          displayName: 'Visual Organization',
          state: 'active',
          revision: 1,
          updatedAt: FIXED_NOW,
        }],
      }
    }
    if (request.query === 'enterprise.project.list') {
      return {
        kind: 'enterprise_project_repository_page',
        snapshotRevision: 1,
        items: [
          {
            kind: 'project',
            projectId: identity.projectId,
            displayName: 'Visual Project',
            state: 'active',
            repositoryCount: 1,
            revision: 1,
            updatedAt: FIXED_NOW,
          },
          {
            kind: 'repository',
            repositoryId: scope.repositoryId,
            projectId: identity.projectId,
            displayName: 'Visual Repository',
            state: 'active',
            defaultBranch: 'main',
            revision: 1,
            updatedAt: FIXED_NOW,
          },
        ],
      }
    }
    if (request.query.startsWith('enterprise.') && request.query.endsWith('.list')) {
      return { kind: 'enterprise_projection_page', items: [], snapshotRevision: 1 }
    }
    if (request.query === 'session.list') {
      return { kind: 'product_session_page', items: emptyChat ? [] : [chatSession()] }
    }
    if (request.query === 'session.get') return chatSession()
    if (request.query === 'session.messages.list') {
      return { kind: 'chat_message_page', items: chatMessages() }
    }
    if (request.query === 'session.interactions.list') {
      return { kind: 'chat_interaction_page', items: [] }
    }
    if (request.query === 'approval.list') {
      return { kind: 'approval_page', items: [approval(1), approval(2)] }
    }
    if (request.query === 'worker.list') {
      return { kind: 'worker_page', items: [worker()] }
    }
    if (request.query === 'credential.reference.list') {
      return { kind: 'credential_reference_page', items: [credentialReference()] }
    }
    if (request.query === 'settings.get') {
      return {
        revision: 1,
        defaultModelRoute: modelRoute,
        workerConcurrencyLimit: 2,
      }
    }
    if (request.query === 'model.route.availability.list') return routeAvailability()
    if (request.query === 'runtime.projection.get') return chatRuntime()
    throw new Error(`unexpected query: ${request.query}`)
  }
  return response(request, result())
}

const controlPlane = {
  serverUrl: 'http://127.0.0.1:8080',
  async restore() {
    return {
      schemaVersion,
      expiresAt: '2099-09-02T00:00:00.000Z',
      actor,
      authorizedScopes: [scope],
    }
  },
  async login() {
    return {
      schemaVersion,
      expiresAt: '2099-09-02T00:00:00.000Z',
      actor,
      authorizedScopes: [scope],
    }
  },
  async logout() {},
  async command() { throw new Error('unexpected command') },
  // serve() already returns the full query envelope; wrapping it again here
  // would make every view-model see a malformed nested result.
  async query(request) {
    const res = serve(request)
    try {
      const mod = await import('/module/generated/control-plane-client.js')
      const names = {
        'approval.list': 'ApprovalListResultResponse',
        'session.list': 'SessionListResultResponse',
        'delivery.list': 'DeliveryListResultResponse',
        'session.get': 'SessionGetResultResponse',
        'delivery.get': 'DeliveryGetResultResponse',
        'session.interactions.list': 'SessionInteractionListResultResponse',
        'session.messages.list': 'SessionMessageListResultResponse',
        'worker.list': 'WorkerListResultResponse',
        'credential.reference.list': 'CredentialReferenceListResultResponse',
        'model.route.availability.list': 'ModelRouteAvailabilityListResultResponse',
        'enterprise.organization.list': 'EnterpriseOrganizationListResultResponse',
        'enterprise.project.list': 'EnterpriseProjectListResultResponse',
        'settings.get': 'SettingsGetResultResponse',
        'runtime.projection.get': 'RuntimeProjectionGetResultResponse',
      }
      const name = names[request.query]
      if (name !== undefined && mod.matchesCanonicalSchema(name, res) === false) {
        if (request.query === 'approval.list') {
          const item = res.result.items[0]
          const checks = {
            whole: mod.matchesCanonicalSchema(name, res),
            projection: mod.matchesCanonicalSchema('ApprovalProjection', item),
            binding: mod.matchesCanonicalSchema('ChatInteractionBindingProjection', item.binding),
            sanitized: mod.matchesCanonicalSchema('ApprovalSanitizedDetailProjection', item.sanitizedDetail),
            category: mod.matchesCanonicalSchema('ApprovalProjectionCategory', item.category),
            scopeField: mod.matchesCanonicalSchema('ApprovalEffectiveDecisionScope', item.effectiveDecisionScope),
            keys: Object.keys(item).join(','),
          }
          console.warn('[demo] approval checks', JSON.stringify(checks))
        } else {
          console.warn('[demo] schema mismatch:', request.query)
        }
      }
    } catch { /* debug-only */ }
    return res
  },
  subscribe() {
    return { cursor: null, resume() {}, reconnect() {}, close() {} }
  },
  close() {},
}

// Determinism controls are installed through CSSOM because the harness serves a
// `style-src 'self'` policy, and the clock is pinned on the shell itself.
const determinism = new CSSStyleSheet()
determinism.replaceSync(DETERMINISM_CSS)
document.adoptedStyleSheets = [...document.adoptedStyleSheets, determinism]

const application = mountWinWinCodeClient({
  root: document.querySelector('[data-winwincode-client-root]'),
  serverUrl: controlPlane.serverUrl,
  controlPlane,
  now: () => FIXED_NOW,
})
globalThis.__demo = { controlPlane, response, serve, FIXED_NOW, schemaVersion, actor, scope, identity }

// Demo convenience: collapse the first-run readiness checklist once it renders,
// so the routed page is the first thing on screen. The toggle stays available.
function collapseReadinessChecklist() {
  const toggle = [...document.querySelectorAll('button')].find(
    candidate => candidate.textContent === 'Hide checklist',
  )
  if (toggle === undefined) {
    setTimeout(collapseReadinessChecklist, 120)
    return
  }
  toggle.click()
  if (toggle.getAttribute('aria-expanded') === 'true') setTimeout(collapseReadinessChecklist, 200)
}
setTimeout(collapseReadinessChecklist, 300)

/** Read at capture time: the harness applies its viewport override after mount. */
function currentViewport() {
  return document.documentElement.clientWidth <= NARROW_VIEWPORT.width
    ? NARROW_VIEWPORT
    : DESKTOP_VIEWPORT
}

function waitFor(predicate, label) {
  const deadline = Date.now() + 10_000
  return (async () => {
    while (!predicate()) {
      if (Date.now() >= deadline) {
        const visible = document.querySelector('main.wwc-main')?.textContent.slice(0, 300)
        throw new Error(`timed out waiting for ${label}: ${JSON.stringify(visible)}`)
      }
      await new Promise(resolve_ => { setTimeout(resolve_, 20) })
    }
  })()
}

/**
 * Every key page: the route the shell serves it on, the element that says it
 * has mounted, and the status line that says its first snapshot has landed.
 * `decisions` is the Attention item's own route, which is where the shell puts
 * the repository's decision list.
 */
const PAGES = Object.freeze({
  home: { route: '#/home', selector: '.wwc-home', status: '.wwc-home-status' },
  chat: { route: '#/chat', selector: '.wwc-chat', status: '.wwc-chat-status' },
  settings: {
    route: '#/settings',
    selector: '.wwc-settings',
    status: '.wwc-settings-status .wwc-status-badge-label',
  },
  attention: {
    route: '#/attention',
    selector: '.wwc-attention-center',
    status: '.wwc-attention-center-status .wwc-status-badge-label',
  },
  decisions: {
    route: `#/attention?session=${productSessionId}`,
    selector: '.wwc-local-decisions',
    status: '.wwc-local-decisions-status .wwc-status-badge-label',
  },
  operations: {
    route: '#/settings/runtime',
    selector: '.wwc-local-operations',
    status: '.wwc-local-operations-status .wwc-status-badge-label',
  },
})

function fingerprintOf(id, kind, root) {
  return captureVisualFingerprint({
    document,
    root,
    id,
    kind,
    viewport: { ...currentViewport() },
    fontStack: VISUAL_REGRESSION_FONT_STACK,
  })
}

function viewportLabel() {
  return currentViewport().width <= NARROW_VIEWPORT.width ? 'narrow' : 'desktop'
}

async function capturePage(name, captureId) {
  const page_ = PAGES[name]
  if (page_ === undefined) throw new Error(`unknown visual page: ${name}`)
  location.hash = page_.route
  await waitFor(() => document.querySelector(page_.selector) !== null, page_.selector)
  await waitFor(() => {
    const label = document.querySelector(page_.status)?.textContent ?? ''
    return label.length > 0 && !/^Loading|^Updating/u.test(label)
  }, `${page_.selector} status`)
  await new Promise(resolve_ => { setTimeout(resolve_, 60) })
  // The page is rooted at its own surface, not at `main`: the Scope selector and
  // the readiness section are shell-level and already baselined with the shell.
  return fingerprintOf(
    `page/${captureId ?? name}@${viewportLabel()}`,
    'page',
    document.querySelector(page_.selector),
  )
}

globalThis.captureVisualPage = capturePage

globalThis.captureVisualShell = async () => {
  await waitFor(() => document.querySelector('header.wwc-header') !== null, 'shell header')
  const captures = [
    fingerprintOf(`shell/header@${viewportLabel()}`, 'shell', document.querySelector('header.wwc-header')),
  ]
  const connection = document.querySelector('.wwc-connection-bar')
  if (connection !== null) {
    captures.push(fingerprintOf(
      `shell/connection-connected@${viewportLabel()}`,
      'shell',
      connection,
    ))
  }
  return captures
}

globalThis.captureVisualOffline = async () => {
  application.connection.offline()
  await waitFor(() => {
    const badge = document.querySelector('.wwc-connection-status .wwc-status-badge-label')
    return badge !== null && badge.textContent === 'Offline'
  }, 'the offline connection presentation')
  await new Promise(resolve_ => { setTimeout(resolve_, 60) })
  const connection = document.querySelector('.wwc-connection-bar')
  if (connection === null) throw new Error('the shell has no connection bar')
  return [fingerprintOf(`shell/connection-offline@${viewportLabel()}`, 'shell', connection)]
}



globalThis.describeVisualViewport = () => ({ ...currentViewport(), mode: MODE })

/** Compares one captured page against the committed baseline and renders a report. */
globalThis.compareVisualPages = (expected, actual) => {
  const differences = compareVisualFingerprints(expected, actual)
  return {
    differences,
    report: renderVisualRegressionReport(differences, { id: actual.id }),
  }
}
