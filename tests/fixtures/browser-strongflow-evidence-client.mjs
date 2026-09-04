import { mountWinWinCodeClient } from '/module/application.js'

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const projectScope = {
  kind: 'project',
  organizationId: scope.organizationId,
  workspaceId: scope.workspaceId,
  projectId: scope.projectId,
}
const chatProductSessionId = 'psn_00000000000000000000000001'
const stageProductSessionId = 'psn_00000000000000000000000002'
const stageRunId = 'run_00000000000000000000000001'
const credentialReferenceId = 'crd_00000000000000000000000001'
const deliveryId = 'dlv_00000000000000000000000002'
const sessionBindingId = 'binding:strongflow:evidence-browser'
const candidateRef = `git-candidate:sha256:${'a'.repeat(64)}`

const evidenceId = 'evd_00000000000000000000000001'

function evidenceIdFor(value) {
  return `evd_${String(value).padStart(26, '0')}`
}
const modelRoute = {
  providerId: 'browser-provider',
  modelId: 'browser-model',
  credentialReferenceId,
}
const browserSession = {
  schemaVersion,
  expiresAt: '2099-09-02T00:00:00.000Z',
  actor,
  authorizedScopes: [scope],
}
const calls = { commands: [], queries: [], subscriptions: [] }
let deliveryRevision = 2
let realtimeOptions = null

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

function routeAvailability() {
  return {
    kind: 'model_route_availability_page',
    scope,
    settingsSource: scope,
    settingsRevision: 1,
    requestPoolSource: projectScope,
    requestPoolRevision: 1,
    defaultProviderId: modelRoute.providerId,
    defaultModelId: modelRoute.modelId,
    routes: [{
      providerId: modelRoute.providerId,
      modelId: modelRoute.modelId,
      displayName: 'Browser fixture model',
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

function ownership() {
  return {
    organizationId: scope.organizationId,
    workspaceId: scope.workspaceId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
  }
}

function readCursor() {
  return {
    token: `cursor_${String(deliveryRevision).padStart(32, '0')}`,
    scope,
    deliveryId,
    deliveryRevision,
    runtimeLedgerRevision: 1,
    runtimeAcceptedSequence: 0,
    publicationRevision: 0,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: deliveryRevision,
      eventId: `evt_${String(deliveryRevision).padStart(26, '0')}`,
    },
  }
}

function binding() {
  return {
    bindingId: sessionBindingId,
    boundAt: '2026-09-02T01:00:00.000Z',
    executionJobId: 'job_00000000000000000000000001',
    productSessionId: stageProductSessionId,
    stageRunId: null,
    workerSessionId: null,
    codexThreadId: null,
    attempt: null,
    fencingToken: null,
    leaseId: null,
    workerId: null,
    sourceIdentity: null,
    sessionIdentity: null,
  }
}

const screenshotEvidenceId = evidenceIdFor(5)
// One real 1x1 PNG so the browser actually decodes the screenshot.
const SCREENSHOT_PNG_BASE64
  = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII='
const screenshotBytes = Uint8Array.from(atob(SCREENSHOT_PNG_BASE64), character =>
  character.charCodeAt(0))
const screenshotDataBase64 = btoa(String.fromCharCode(...screenshotBytes))

function evidenceRows() {
  const rows = [1, 2, 3, 4].map(value => ({
    candidateRef,
    createdAt: '2026-09-02T01:00:00.000Z',
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 1,
    id: evidenceIdFor(value),
    sessionBindingId,
    sourceRef: `artifact:source:${String(value)}`,
    stageRunId,
    type: value === 1 ? 'test' : value === 2 ? 'command' : value === 3 ? 'runtime_event' : 'diff',
  }))
  rows.push({
    candidateRef,
    createdAt: '2026-09-02T01:00:02.000Z',
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 1,
    id: screenshotEvidenceId,
    sessionBindingId,
    sourceRef: 'artifact:source:screenshot',
    stageRunId,
    type: 'runtime_event',
  })
  return rows
}

function screenshotDescriptor(row) {
  return {
    artifactId: 'art_screenshot_0000000000000001',
    digest: `sha256:${'7'.repeat(64)}`,
    fileName: 'candidate.png',
    kind: 'report',
    mediaType: 'image/png',
    previewMode: 'inline_text',
    provenance: {
      candidateRef: row.candidateRef,
      deliveryId,
      deliveryRevision,
      evidenceId: row.id,
      sessionBindingId: row.sessionBindingId,
      stageRunId: row.stageRunId,
    },
    sizeBytes: screenshotBytes.length,
  }
}

function summary() {
  return {
    schemaVersion,
    deliveryId,
    revision: deliveryRevision,
    status: 'clarifying',
    title: 'Evidence workbench browser fixture',
    updatedAt: '2026-09-02T01:00:00.000Z',
    ownership: ownership(),
    activeStageRunId: stageRunId,
    openAttentionCount: 0,
    taskCounts: {
      total: 0,
      pending: 0,
      active: 0,
      blocked: 0,
      verifying: 0,
      completed: 0,
      failed: 0,
    },
  }
}

function detail() {
  return {
    kind: 'delivery_detail',
    schemaVersion,
    deliveryId,
    deliveryRevision,
    readCursor: readCursor(),
    ownership: ownership(),
    status: 'clarifying',
    requirements: {
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 1,
      title: 'Evidence workbench browser fixture',
      goal: 'Open exact Evidence detail in a real browser.',
      scope: [],
      outOfScope: [],
      constraints: [],
      acceptanceCriteria: [],
      sourceRef: null,
      publicationTarget: null,
      repository: { kind: 'local-git', locator: 'workspace://repository' },
      baseRevision: '0123456789012345678901234567890123456789',
      maxReworkAttempts: 2,
    },
    solutionReview: null,
    stages: [{
      id: stageRunId,
      actorType: 'codex',
      attempt: 1,
      deliveryTaskId: null,
      finishedAt: null,
      role: 'clarifier',
      sessionBinding: binding(),
      stage: 'clarifying',
      startedAt: '2026-09-02T01:00:00.000Z',
      status: 'running',
    }],
    tasks: [],
    attention: [],
    evidence: evidenceRows(),
    currentCandidate: {
      candidateRef,
      candidateCommitId: '1'.repeat(40),
      candidateTreeId: '2'.repeat(40),
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 1,
      diffSha256: `sha256:${'3'.repeat(64)}`,
      frozenAt: '2026-09-02T01:00:01.000Z',
      producerSessionBindingId: sessionBindingId,
      producerStageRunId: stageRunId,
    },
    verdict: {
      id: 'verdict:evidence-browser',
      candidateRef,
      criteria: [{
        criterionId: 'criterion:browser',
        evaluatedAt: '2026-09-02T01:00:03.000Z',
        evidenceRefs: [evidenceId],
        explanation: 'The browser evidence check failed.',
        resultId: 'result:browser',
        verdict: 'fail',
      }],
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 1,
      producedAt: '2026-09-02T01:00:03.000Z',
      status: 'fail',
      unresolvedFindings: [],
    },
    publication: null,
  }
}

function deliveryRuntime() {
  const cursor = readCursor()
  return {
    kind: 'runtime_projection',
    productSessionId: stageProductSessionId,
    deliveryId,
    stageRunId,
    readCursor: cursor,
    eventCursor: cursor.eventCursor,
    lastProjectionSequence: 0,
    revision: 1,
    rebuiltAt: '2026-09-02T01:00:02.000Z',
    sessions: [],
  }
}

function chatSession() {
  return {
    id: chatProductSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision: 1,
    state: 'idle',
    title: 'Evidence fixture Chat',
    updatedAt: '2026-09-02T00:30:00.000Z',
  }
}

function chatRuntime() {
  const eventCursor = {
    eventId: null,
    sequence: 0,
    scope,
    stream: { kind: 'product-session', productSessionId: chatProductSessionId },
  }
  return {
    kind: 'runtime_projection',
    productSessionId: chatProductSessionId,
    deliveryId: null,
    stageRunId: null,
    readCursor: null,
    eventCursor,
    lastProjectionSequence: 0,
    revision: 1,
    rebuiltAt: '2026-09-02T00:31:00.000Z',
    sessions: [],
  }
}

function evidenceDetail(request) {
  const bindingParameters = request.parameters
  const row = evidenceRows().find(candidate => candidate.id === bindingParameters.evidenceId)
  if (row === undefined) {
    throw new Error(`unexpected evidence.get binding: ${bindingParameters.evidenceId}`)
  }
  if (
    bindingParameters.candidateRef !== row.candidateRef
    || bindingParameters.stageRunId !== row.stageRunId
    || bindingParameters.sessionBindingId !== row.sessionBindingId
    || bindingParameters.sourceRef !== row.sourceRef
    || bindingParameters.type !== row.type
    || bindingParameters.deliveryId !== deliveryId
    || bindingParameters.readPageLimit !== 1
    || bindingParameters.atCursor.token !== readCursor().token
  ) {
    throw new Error('evidence.get binding does not match the snapshot row')
  }
  if (row.id === screenshotEvidenceId) {
    return response(request, {
      kind: 'evidence_detail',
      artifactAccess: { state: 'available', items: [screenshotDescriptor(row)] },
      evidence: row,
      outcome: 'succeeded',
      readCursor: readCursor(),
    })
  }
  const artifacts = [1, 2].map(value => ({
    artifactId: `art_${String(value).padStart(26, '0')}`,
    digest: `sha256:${String(value).repeat(64)}`,
    fileName: `evidence-${String(value)}.bin`,
    kind: 'report',
    mediaType: 'application/octet-stream',
    previewMode: 'download_only',
    provenance: {
      candidateRef: row.candidateRef,
      deliveryId,
      deliveryRevision,
      evidenceId: row.id,
      sessionBindingId: row.sessionBindingId,
      stageRunId: row.stageRunId,
    },
    sizeBytes: 26,
  }))
  return response(request, {
    kind: 'evidence_detail',
    artifactAccess: { state: 'available', items: artifacts },
    evidence: row,
    outcome: row.type === 'test' ? 'failed' : 'succeeded',
    readCursor: readCursor(),
  })
}

const controlPlane = {
  serverUrl: 'https://control.localhost',
  async restore() { return structuredClone(browserSession) },
  async login() { return structuredClone(browserSession) },
  async logout() {},
  async query(request) {
    calls.queries.push(structuredClone(request))
    if (request.query === 'delivery.list') {
      return response(request, { kind: 'delivery_page', items: [summary()] })
    }
    if (request.query === 'session.list') {
      return response(request, { kind: 'product_session_page', items: [chatSession()] })
    }
    if (request.query === 'model.route.availability.list') {
      return response(request, routeAvailability())
    }
    if (request.query === 'session.get') return response(request, chatSession())
    if (request.query === 'session.messages.list') {
      return response(request, { kind: 'chat_message_page', items: [] })
    }
    if (request.query === 'settings.get') {
      return response(request, {
        revision: 1,
        workerConcurrencyLimit: 1,
        defaultModelRoute: modelRoute,
      })
    }
    if (request.query === 'credential.reference.list') {
      return response(request, {
        kind: 'credential_reference_page',
        items: [{
          id: credentialReferenceId,
          providerId: modelRoute.providerId,
          displayName: 'Browser model credential',
          secretState: 'available',
          rotationVersion: 1,
          lastRotatedAt: '2026-09-02T00:00:00.000Z',
          revokedAt: null,
          revision: 1,
          updatedAt: '2026-09-02T00:00:00.000Z',
        }],
      })
    }
    if (request.query === 'session.interactions.list') {
      return response(request, { kind: 'chat_interaction_page', items: [] })
    }
    if (request.query === 'approval.list') {
      return response(request, { kind: 'approval_page', items: [] })
    }
    if (request.query === 'runtime.projection.get'
      && request.parameters.kind === 'product-session') return response(request, chatRuntime())
    if (request.query === 'delivery.get') return response(request, detail())
    if (request.query === 'evidence.get') return evidenceDetail(request)
    if (request.query === 'evidence.artifact.content.get') {
      const parameters = request.parameters
      const row = evidenceRows().find(candidate => candidate.id === screenshotEvidenceId)
      const descriptor = screenshotDescriptor(row)
      if (
        parameters.evidence.evidenceId !== screenshotEvidenceId
        || parameters.artifactId !== descriptor.artifactId
        || parameters.offset !== 0
        || parameters.length !== screenshotBytes.length
      ) {
        throw new Error('unsafe Artifact content read must not be answered')
      }
      return response(request, {
        artifact: descriptor,
        contentEncoding: 'binary',
        dataBase64: screenshotDataBase64,
        encoding: 'base64',
        evidence: row,
        kind: 'evidence_artifact_content_chunk',
        nextOffset: null,
        offset: 0,
        previewMode: 'inline_text',
        readCursor: readCursor(),
        returnedBytes: screenshotBytes.length,
        state: 'available',
        totalBytes: screenshotBytes.length,
        truncated: false,
      })
    }
    if (request.query === 'runtime.projection.get') return response(request, deliveryRuntime())
    throw new Error(`unexpected query: ${request.query}`)
  },
  async command(request) {
    throw new Error(`unexpected command: ${request.command}`)
  },
  subscribe(options) {
    realtimeOptions = options
    calls.subscriptions.push(structuredClone({
      subscriptionId: options.subscriptionId,
      subscription: options.subscription,
      startAt: options.startAt,
    }))
    return {
      cursor: options.startAt,
      resume() {},
      reconnect() {},
      close() {},
    }
  },
  close() {},
}

async function advanceDeliverySnapshot() {
  if (realtimeOptions === null) throw new Error('StrongFlow subscription is not active')
  deliveryRevision += 1
  await realtimeOptions.onEvent({
    sequence: deliveryRevision,
    event: {
      type: 'delivery.changed.v1',
      deliveryId,
      revision: deliveryRevision,
      changeKind: 'advanced',
    },
  })
}

const root = document.querySelector('[data-winwincode-client-root]')
mountWinWinCodeClient({
  root,
  serverUrl: controlPlane.serverUrl,
  controlPlane,
})

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise(resolve => { setTimeout(resolve, 20) })
  }
  throw new Error(`timed out waiting for ${label}: ${document.body.textContent}`)
}

globalThis.runEvidenceWorkbenchScenario = async () => {
  await waitFor(
    () => document.querySelector('.wwc-strongflow-heading')?.textContent
      === 'Evidence workbench browser fixture',
    'StrongFlow snapshot with evidence',
  )
  await waitFor(
    () => document.querySelectorAll('.wwc-strongflow-evidence-row').length === 5,
    'evidence rows',
  )
  const tabs = [...document.querySelectorAll('.wwc-strongflow-evidence-tabs [role="tab"]')]
  const entryPointNodes = {
    stage: document.querySelector('.wwc-strongflow-stage-evidence-open'),
    candidate: document.querySelector('.wwc-strongflow-candidate-evidence-open'),
    criterion: document.querySelector('.wwc-strongflow-criterion-evidence-open'),
  }
  const initial = {
    hash: location.hash,
    tabLabels: tabs.map(tab => tab.textContent),
    selected: tabs.find(tab => tab.getAttribute('aria-selected') === 'true').textContent,
    rowTypes: [...document.querySelectorAll('.wwc-strongflow-evidence-row')]
      .map(row => row.dataset.evidenceType),
    candidateStates: [...new Set(
      [...document.querySelectorAll('.wwc-strongflow-evidence-row')]
        .map(row => row.dataset.candidateState),
    )],
    panels: [...document.querySelectorAll('.wwc-strongflow-evidence-panel')].map(panel => ({
      id: panel.id,
      role: panel.getAttribute('role'),
      labelledBy: panel.getAttribute('aria-labelledby'),
      hidden: panel.hidden,
    })),
    entryPoints: {
      stage: document.querySelectorAll('.wwc-strongflow-stage-evidence-open').length,
      candidate: document.querySelectorAll('.wwc-strongflow-candidate-evidence-open').length,
      criterion: document.querySelectorAll('.wwc-strongflow-criterion-evidence-open').length,
    },
    summary: document.querySelector('.wwc-strongflow-evidence-summary-counts').textContent,
    criterionJoin: document.querySelector(
      `[data-evidence-id="${evidenceId}"] .wwc-strongflow-evidence-criteria`,
    ).textContent,
  }

  tabs.find(tab => tab.textContent === 'Tests').click()
  await waitFor(
    () => document.querySelectorAll('.wwc-strongflow-evidence-row').length === 1,
    'Tests tab filter',
  )
  const testsView = {
    rowTypes: [...document.querySelectorAll('.wwc-strongflow-evidence-row')]
      .map(row => row.dataset.evidenceType),
    hash: location.hash,
  }

  const row = document.querySelector('.wwc-strongflow-evidence-row .wwc-strongflow-evidence-open')
  row.focus()
  row.click()
  const detailDuringLoad = document.querySelector('.wwc-strongflow-evidence-detail')
  const closeFocusedDuringLoad = document.activeElement
    === document.querySelector('.wwc-drawer-close')
  await waitFor(
    () => document.querySelector('.wwc-strongflow-evidence-detail')?.dataset.status === 'ready',
    'evidence detail drawer',
  )
  const detail = {
    outcome: document.querySelector(
      '.wwc-strongflow-evidence-detail-outcome .wwc-status-badge-label',
    ).textContent,
    tone: document.querySelector('.wwc-strongflow-evidence-detail-outcome').dataset.tone,
    statusIcon: document.querySelector(
      '.wwc-strongflow-evidence-detail-outcome .wwc-status-badge-icon',
    ).textContent,
    statusIconHidden: document.querySelector(
      '.wwc-strongflow-evidence-detail-outcome .wwc-status-badge-icon',
    ).getAttribute('aria-hidden'),
    candidate: document.querySelector('.wwc-strongflow-evidence-detail-candidate').textContent,
    artifact: document.querySelector('.wwc-strongflow-evidence-artifact').textContent,
    hash: location.hash,
    stableNode: detailDuringLoad === document.querySelector('.wwc-strongflow-evidence-detail'),
    closeFocusedDuringLoad,
    busy: document.querySelector('.wwc-strongflow-evidence-detail').getAttribute('aria-busy'),
    artifactSelectors: document.querySelectorAll(
      '.wwc-strongflow-evidence-artifact-select',
    ).length,
  }
  const artifactSelectors = [...document.querySelectorAll(
    '.wwc-strongflow-evidence-artifact-select',
  )]
  artifactSelectors[1].click()
  detail.selectedArtifact = artifactSelectors[1].getAttribute('aria-pressed')

  const closeEvents = []
  const drawer = document.querySelector('.wwc-strongflow-evidence-drawer')
  drawer.addEventListener('keydown', event => {
    if (event.key === 'Escape') closeEvents.push('escape')
  })
  drawer.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  await waitFor(
    () => document.querySelector('.wwc-strongflow-evidence-drawer').hidden,
    'closed evidence detail',
  )
  const closed = {
    hash: location.hash,
    detailRetained: document.querySelector('.wwc-strongflow-evidence-detail') === detailDuringLoad,
    detailHidden: document.querySelector('.wwc-strongflow-evidence-detail').hidden,
    openerFocused: document.activeElement === row,
  }

  const selectedTab = document.querySelector('.wwc-strongflow-evidence-tabs [aria-selected="true"]')
  selectedTab.focus()
  // Tests -> Preview: the keyboard walk crosses the Preview tab.
  selectedTab.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft', bubbles: true }))
  await waitFor(
    () => document.querySelector('.wwc-strongflow-evidence-tabs [aria-selected="true"]')
      .textContent === 'Preview'
      && document.querySelectorAll('.wwc-strongflow-preview-row').length === 3,
    'keyboard tab navigation to Preview',
  )
  const selectedPreviewTab = document.querySelector(
    '.wwc-strongflow-evidence-tabs [aria-selected="true"]',
  )
  selectedPreviewTab.dispatchEvent(
    new KeyboardEvent('keydown', { key: 'ArrowLeft', bubbles: true }),
  )
  await waitFor(
    () => [...document.querySelectorAll('.wwc-strongflow-evidence-row')]
      .map(row => row.dataset.evidenceType)
      .join(',') === 'test,command,runtime_event,diff,runtime_event'
      && document.querySelector('.wwc-strongflow-evidence-tabs [aria-selected="true"]')
        .textContent === 'Evidence',
    'keyboard tab navigation back to Evidence',
  )

  row.focus()
  row.click()
  await waitFor(
    () => document.querySelector('.wwc-strongflow-evidence-detail')?.dataset.status === 'ready',
    'second Evidence detail open',
  )
  const staleDetail = document.querySelector('.wwc-strongflow-evidence-detail')
  const refresh = advanceDeliverySnapshot()
  const staleClearedDuringRefresh = staleDetail.hidden
    && document.querySelector('.wwc-strongflow-evidence-drawer').hidden
  await refresh
  await waitFor(
    () => document.querySelector('.wwc-strongflow-evidence-detail')?.dataset.status === 'ready'
      && !document.querySelector('.wwc-strongflow-evidence-drawer').hidden,
    'Evidence detail reopened under the new cursor',
  )
  const refreshed = {
    staleClearedDuringRefresh,
    stableNode: staleDetail === document.querySelector('.wwc-strongflow-evidence-detail'),
    stableEntryPoints: {
      stage: entryPointNodes.stage === document.querySelector('.wwc-strongflow-stage-evidence-open'),
      candidate: entryPointNodes.candidate
        === document.querySelector('.wwc-strongflow-candidate-evidence-open'),
      criterion: entryPointNodes.criterion
        === document.querySelector('.wwc-strongflow-criterion-evidence-open'),
    },
    binding: document.querySelector('.wwc-strongflow-evidence-detail-summary dd').textContent,
  }

  // UI-407: the Preview tab reports health, joins Criteria, and opens the one
  // raster screenshot Artifact inside a sandboxed image node.
  const previewTab = tabs.find(tab => tab.textContent === 'Preview')
  previewTab.click()
  await waitFor(
    () => document.querySelectorAll('.wwc-strongflow-preview-row').length === 3,
    'Preview rows',
  )
  const previewHealth = document.querySelector('.wwc-strongflow-preview-health')
  const previewReason = document.querySelector('.wwc-strongflow-preview-reason')
  const previewRows = [...document.querySelectorAll('.wwc-strongflow-preview-row')]
  const previewRow = previewRows.find(row => row.dataset.evidenceId === screenshotEvidenceId)
  previewRow.querySelector('.wwc-strongflow-evidence-open').click()
  await waitFor(
    () => document.querySelector('.wwc-strongflow-evidence-detail')?.dataset.status === 'ready',
    'Preview screenshot detail',
  )
  await waitFor(
    () => {
      const image = document.querySelector('.wwc-strongflow-evidence-image-content')
      return image !== null && image.complete && image.naturalWidth > 0
    },
    'screenshot decoded',
  )
  const screenshot = document.querySelector('.wwc-strongflow-evidence-image-content')
  const screenshotContentReads = () => calls.queries.filter(
    query => query.query === 'evidence.artifact.content.get',
  ).length
  const readsAfterScreenshot = screenshotContentReads()
  const preview = {
    hash: location.hash,
    health: previewHealth.dataset.previewHealth,
    healthTone: previewHealth.dataset.tone,
    reason: previewReason.textContent,
    kinds: previewRows.map(row => row.dataset.previewKind),
    screenshotCaption: document.querySelector(
      '.wwc-strongflow-evidence-image-caption',
    ).textContent,
    screenshotSandboxed: screenshot.getAttribute('data-preview-sandbox'),
    screenshotDecoded: screenshot.complete && screenshot.naturalWidth > 0,
    screenshotReferrerPolicy: screenshot.getAttribute('referrerpolicy'),
    textViewerHidden: document.querySelector('.wwc-strongflow-evidence-content-text').hidden,
    screenshotReads: readsAfterScreenshot,
  }
  // The screenshot bytes come from exactly one bounded Artifact read.
  const revokedNaturally = screenshot.src.startsWith('blob:')

  return {
    initial,
    testsView,
    detail,
    closed,
    refreshed,
    preview,
    screenshotUrlIsBlob: revokedNaturally,
    evidenceQueries: calls.queries
      .filter(query => query.query === 'evidence.get')
      .map(query => ({
        evidenceId: query.parameters.evidenceId,
        readPageLimit: query.parameters.readPageLimit,
        cursorToken: query.parameters.atCursor.token,
        page: query.page,
      })),
    contentQueries: calls.queries
      .filter(query => query.query === 'evidence.artifact.content.get').length,
    navigationEntryCount: performance.getEntriesByType('navigation').length,
    scope,
  }
}

globalThis.runEvidenceDeepLinkReloadScenario = async () => {
  await waitFor(
    () => document.querySelector('.wwc-strongflow-heading')?.textContent
      === 'Evidence workbench browser fixture',
    'reloaded StrongFlow snapshot',
  )
  await waitFor(
    () => document.querySelector('.wwc-strongflow-evidence-detail')?.dataset.status === 'ready'
      && !document.querySelector('.wwc-strongflow-evidence-drawer')?.hidden,
    'Evidence detail restored from the typed deep link',
  )
  const parameters = new URLSearchParams(location.hash.split('?')[1] ?? '')
  const evidenceQueries = calls.queries.filter(query => query.query === 'evidence.get')
  const evidenceQuery = evidenceQueries.at(-1)
  return {
    hash: location.hash,
    selectedTab: document.querySelector(
      '.wwc-strongflow-evidence-tabs [aria-selected="true"]',
    )?.textContent,
    detailEvidenceId: document.querySelector(
      '.wwc-strongflow-evidence-detail',
    )?.dataset.evidenceId,
    route: {
      deliveryId: parameters.get('delivery'),
      productSessionId: parameters.get('session'),
      stageRunId: parameters.get('stageRun'),
      evidenceId: parameters.get('evidence'),
    },
    binding: evidenceQuery === undefined ? null : {
      deliveryId: evidenceQuery.parameters.deliveryId,
      sessionBindingId: evidenceQuery.parameters.sessionBindingId,
      stageRunId: evidenceQuery.parameters.stageRunId,
      evidenceId: evidenceQuery.parameters.evidenceId,
    },
    evidenceQueryCount: evidenceQueries.length,
  }
}
