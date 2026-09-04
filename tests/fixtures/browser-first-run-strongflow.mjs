// SPDX-License-Identifier: Apache-2.0

import { mountWinWinCodeClient } from '/module/application.js'
import { ControlPlaneClientError } from '/module/control-plane-client.js'

// Deterministic first-run workspace.  The injected Control Plane facade starts
// completely empty - no browser session, no default model route, no Chat session
// and no Delivery - so the whole first-use vertical runs without history and
// without a real model key.  BOOTSTRAP_PROOF is the only credential the run ever
// handles and SECRET_MARKER is planted inside one served credential reference,
// which lets the suite prove that neither value reaches the DOM, the URL, browser
// storage, the console, or a diagnostic artifact.
//
// Workspace facts live in sessionStorage because a full page reload must observe
// the same Server state a real Control Plane would keep: the browser session
// restores, and the Chat and Delivery facts survive.

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const updated = '2026-09-02T08:00:00.000Z'
const BASELINE_REVISION = '0123456789abcdef0123456789abcdef01234567'
const BOOTSTRAP_PROOF = 'first-run-browser-bootstrap-proof'
const REJECTED_PROOF = 'rejected-first-run-bootstrap-proof'
const SECRET_MARKER = 'vault-locator-secret-marker'
const REQUIREMENT = 'Deliver the deterministic first-run vertical.'
const DELIVERY_TITLE = 'First-run Delivery'
const CRITERION = 'The first-run vertical reaches StrongFlow.'
const STORAGE_KEY = 'winwincode-first-run-browser-state'

function identifier(prefix, index) {
  return `${prefix}_${String(index).padStart(26, '0')}`
}

function workspaceIdentity(index) {
  return {
    organizationId: identifier('org', index),
    workspaceId: identifier('wsp', index),
    projectId: identifier('prj', index),
    repositoryId: identifier('rep', index),
  }
}

function repositoryScope(identity) {
  return {
    kind: 'repository',
    organizationId: identity.organizationId,
    workspaceId: identity.workspaceId,
    projectId: identity.projectId,
    repositoryId: identity.repositoryId,
  }
}

const chosenIdentity = workspaceIdentity(1)
const alternativeIdentity = workspaceIdentity(2)
const chosenScope = repositoryScope(chosenIdentity)
const alternativeScope = repositoryScope(alternativeIdentity)
const chosenNames = { organizationName: 'Acme', projectName: 'Platform', repositoryName: 'Server' }
const alternativeNames = {
  organizationName: 'Beta',
  projectName: 'Sandbox',
  repositoryName: 'Client',
}

const primaryCredentialReferenceId = identifier('crd', 1)
const alternateCredentialReferenceId = identifier('crd', 2)
const primaryModelRoute = {
  providerId: 'primary-provider',
  modelId: 'primary-model',
  credentialReferenceId: primaryCredentialReferenceId,
}
const alternateModelRoute = {
  providerId: 'alternate-provider',
  modelId: 'alternate-model',
  credentialReferenceId: alternateCredentialReferenceId,
}

const stageProductSessionId = identifier('psn', 2)
const stageRunId = identifier('run', 1)
const workerId = identifier('wrk', 1)

function loadState() {
  const stored = JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? 'null')
  if (stored !== null && typeof stored === 'object') return stored
  return {
    authenticated: false,
    availabilityReads: 0,
    settings: { revision: 1, workerConcurrencyLimit: 2, defaultModelRoute: null },
    sessions: [],
    messages: [],
    delivery: null,
    submittedRequirements: 0,
  }
}

let state = loadState()
const calls = {
  commands: [],
  queries: [],
  subscriptions: [],
  console: [],
  submittedProofs: [],
}

function save() {
  sessionStorage.setItem(STORAGE_KEY, JSON.stringify(state))
}

for (const method of ['debug', 'error', 'info', 'log', 'warn']) {
  const original = console[method].bind(console)
  console[method] = (...values) => {
    calls.console.push(values.map(value => String(value)).join(' '))
    original(...values)
  }
}

window.addEventListener('error', event => {
  calls.console.push(`unhandled error ${event.error?.code ?? ''} ${event.error?.message ?? ''}`)
})
window.addEventListener('unhandledrejection', event => {
  calls.console.push(`unhandled rejection ${event.reason?.code ?? ''} ${event.reason?.message ?? ''}`)
})

function accessFailure(request, kind, code, retryable) {
  return new ControlPlaneClientError({
    kind,
    code,
    message: 'private first-run diagnostics',
    requestId: request?.requestId ?? null,
    retryable,
  })
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

function completed(request, result, previousRevision, currentRevision) {
  return {
    schemaVersion,
    requestId: request.requestId,
    command: request.command,
    outcome: 'completed',
    previousRevision,
    currentRevision,
    result,
  }
}

function browserSession() {
  return {
    schemaVersion,
    expiresAt: '2099-09-02T00:00:00.000Z',
    actor,
    authorizedScopes: [chosenScope, alternativeScope],
  }
}

function availabilityItem(route, providerDisplayName, modelDisplayName) {
  return {
    route,
    providerDisplayName,
    modelDisplayName,
    catalogSource: chosenScope,
    catalogVersion: 1,
    providerVersion: 1,
    modelVersion: 1,
    contextWindowTokens: 128_000,
    maxOutputTokens: 16_000,
    toolSupport: 'parallel',
    reasoningEfforts: ['medium', 'high'],
    credentialRotationVersion: 1,
    isDefault: false,
    status: 'enabled',
    reason: 'ready',
  }
}

function availability(scope) {
  return {
    kind: 'model_route_availability_page',
    scope,
    settingsSource: scope,
    settingsRevision: state.settings.revision,
    requestPoolSource: {
      kind: 'project',
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
    },
    requestPoolRevision: 1,
    defaultProviderId: state.settings.defaultModelRoute?.providerId ?? null,
    defaultModelId: state.settings.defaultModelRoute?.modelId ?? null,
    status: 'enabled',
    reason: 'ready',
    items: [
      availabilityItem(primaryModelRoute, 'Primary Provider', 'Primary Model'),
      availabilityItem(alternateModelRoute, 'Alternate Provider', 'Alternate Model'),
    ],
  }
}

function ownership() {
  return {
    organizationId: chosenScope.organizationId,
    workspaceId: chosenScope.workspaceId,
    projectId: chosenScope.projectId,
    repositoryId: chosenScope.repositoryId,
  }
}

function taskCounts() {
  return {
    total: 0,
    pending: 0,
    active: 0,
    blocked: 0,
    verifying: 0,
    completed: 0,
    failed: 0,
  }
}

function deliverySummary() {
  if (state.delivery === null) return null
  return {
    schemaVersion,
    deliveryId: state.delivery.deliveryId,
    revision: state.delivery.revision,
    status: state.delivery.revision === 1 ? 'draft' : 'clarifying',
    title: state.delivery.spec.title,
    updatedAt: updated,
    ownership: ownership(),
    activeStageRunId: state.delivery.revision === 1 ? null : stageRunId,
    openAttentionCount: 0,
    taskCounts: taskCounts(),
  }
}

function readCursor() {
  return {
    token: identifier('cur', 2),
    scope: chosenScope,
    deliveryId: state.delivery.deliveryId,
    deliveryRevision: 2,
    runtimeLedgerRevision: 1,
    runtimeAcceptedSequence: 0,
    publicationRevision: 0,
    eventCursor: {
      scope: chosenScope,
      stream: { kind: 'delivery', deliveryId: state.delivery.deliveryId },
      sequence: 2,
      eventId: identifier('evt', 2),
    },
  }
}

function stageBinding() {
  return {
    bindingId: 'binding:first-run:browser',
    boundAt: updated,
    executionJobId: identifier('job', 1),
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

function deliveryDetail() {
  return {
    kind: 'delivery_detail',
    schemaVersion,
    deliveryId: state.delivery.deliveryId,
    deliveryRevision: 2,
    readCursor: readCursor(),
    ownership: ownership(),
    status: 'clarifying',
    requirements: {
      deliverySpecId: 'spec:first-run',
      deliverySpecRevision: 1,
      title: state.delivery.spec.title,
      goal: state.delivery.spec.goal,
      scope: state.delivery.spec.scope,
      outOfScope: state.delivery.spec.outOfScope,
      constraints: state.delivery.spec.constraints,
      acceptanceCriteria: state.delivery.spec.acceptanceCriteria.map(criterion => ({
        id: criterion.id,
        description: criterion.title,
        verificationMethod: null,
        required: criterion.required,
      })),
      sourceRef: null,
      publicationTarget: null,
      repository: { kind: 'local-git', locator: 'workspace://repository' },
      baseRevision: state.delivery.spec.baseRevision,
      maxReworkAttempts: 2,
    },
    solutionReview: null,
    diagramExecution: null,
    stages: [{
      id: stageRunId,
      actorType: 'codex',
      attempt: 1,
      deliveryTaskId: null,
      finishedAt: null,
      role: 'clarifier',
      sessionBinding: stageBinding(),
      stage: 'clarifying',
      startedAt: updated,
      status: 'running',
    }],
    tasks: [],
    attention: [],
    evidence: [],
    currentCandidate: null,
    verdict: null,
    publication: null,
  }
}

function deliveryRuntime() {
  const cursor = readCursor()
  return {
    kind: 'runtime_projection',
    productSessionId: stageProductSessionId,
    deliveryId: state.delivery.deliveryId,
    stageRunId,
    readCursor: cursor,
    eventCursor: cursor.eventCursor,
    lastProjectionSequence: 0,
    revision: 1,
    rebuiltAt: updated,
    sessions: [],
  }
}

function chatRuntime(productSessionId) {
  return {
    kind: 'runtime_projection',
    productSessionId,
    deliveryId: null,
    stageRunId: null,
    readCursor: null,
    eventCursor: {
      eventId: null,
      sequence: 0,
      scope: chosenScope,
      stream: { kind: 'product-session', productSessionId },
    },
    lastProjectionSequence: 0,
    revision: 1,
    rebuiltAt: updated,
    sessions: [],
  }
}

const controlPlane = {
  serverUrl: 'https://control.localhost/first-run',
  async restore() {
    if (!state.authenticated) throw accessFailure(null, 'authentication', 'AUTH_SESSION_MISSING', false)
    return structuredClone(browserSession())
  },
  async login(bootstrapProof) {
    calls.submittedProofs.push(bootstrapProof)
    if (bootstrapProof !== BOOTSTRAP_PROOF) {
      throw accessFailure(null, 'authentication', 'BOOTSTRAP_PROOF_REJECTED', false)
    }
    state.authenticated = true
    save()
    return structuredClone(browserSession())
  },
  async logout() {
    state.authenticated = false
    save()
  },
  async command(request) {
    calls.commands.push(structuredClone(request))
    if (!state.authenticated) {
      throw accessFailure(request, 'authentication', 'AUTH_SESSION_MISSING', false)
    }
    if (request.command === 'session.create') {
      state.sessions.push({
        id: request.payload.productSessionId,
        projectId: request.payload.projectId,
        repositoryId: request.payload.repositoryId,
        revision: 1,
        state: 'idle',
        title: request.payload.title,
        updatedAt: updated,
      })
      save()
      return completed(request, structuredClone(state.sessions.at(-1)), 0, 1)
    }
    if (request.command === 'chat.submit') {
      // The first requirement submission fails transiently so the vertical can
      // exercise the Chat retry entry on the write path that carries the
      // requirement into the workspace.
      const firstAttempt = state.submittedRequirements === 0
      state.submittedRequirements += 1
      save()
      if (firstAttempt) throw accessFailure(request, 'network', 'NETWORK_ERROR', true)
      const sessionProjection = state.sessions.find(item => item.id === request.payload.productSessionId)
      if (sessionProjection === undefined) {
        throw accessFailure(request, 'protocol', 'RESOURCE_NOT_FOUND', false)
      }
      if (request.expectedRevision !== sessionProjection.revision) {
        throw accessFailure(request, 'conflict', 'REVISION_CONFLICT', false)
      }
      sessionProjection.revision += 1
      state.messages.push({
        id: identifier('msg', state.messages.length + 1),
        productSessionId: sessionProjection.id,
        role: 'user',
        content: request.payload.message,
        sequence: state.messages.length + 1,
        state: 'completed',
        createdAt: updated,
        updatedAt: updated,
      })
      save()
      return completed(
        request,
        structuredClone(sessionProjection),
        sessionProjection.revision - 1,
        sessionProjection.revision,
      )
    }
    if (request.command === 'delivery.create') {
      state.delivery = {
        deliveryId: request.payload.deliveryId,
        revision: 1,
        spec: structuredClone(request.payload.spec),
      }
      save()
      return completed(request, structuredClone(deliverySummary()), 0, 1)
    }
    if (request.command === 'delivery.advance') {
      if (state.delivery === null || state.delivery.deliveryId !== request.payload.deliveryId) {
        throw accessFailure(request, 'protocol', 'RESOURCE_NOT_FOUND', false)
      }
      if (request.expectedRevision !== state.delivery.revision) {
        throw accessFailure(request, 'conflict', 'REVISION_CONFLICT', false)
      }
      state.delivery.revision += 1
      save()
      return completed(
        request,
        structuredClone(deliverySummary()),
        state.delivery.revision - 1,
        state.delivery.revision,
      )
    }
    throw new Error(`unexpected command: ${request.command}`)
  },
  async query(request) {
    calls.queries.push(structuredClone(request))
    if (!state.authenticated) {
      throw accessFailure(request, 'authentication', 'AUTH_SESSION_MISSING', false)
    }
    if (request.query === 'enterprise.organization.list') {
      return response(request, {
        kind: 'enterprise_organization_page',
        snapshotRevision: 1,
        items: [{
          id: chosenIdentity.organizationId,
          displayName: chosenNames.organizationName,
          slug: 'acme',
          state: 'active',
          revision: 1,
          updatedAt: updated,
        }, {
          id: alternativeIdentity.organizationId,
          displayName: alternativeNames.organizationName,
          slug: 'beta',
          state: 'active',
          revision: 1,
          updatedAt: updated,
        }],
      })
    }
    if (request.query === 'enterprise.project.list') {
      const selected = request.scope.organizationId === alternativeIdentity.organizationId
        ? { identity: alternativeIdentity, names: alternativeNames }
        : { identity: chosenIdentity, names: chosenNames }
      return response(request, {
        kind: 'enterprise_project_repository_page',
        snapshotRevision: 1,
        items: [{
          kind: 'project',
          projectId: selected.identity.projectId,
          displayName: selected.names.projectName,
          repositoryCount: 1,
          state: 'active',
          revision: 1,
          updatedAt: updated,
        }, {
          kind: 'repository',
          projectId: selected.identity.projectId,
          repositoryId: selected.identity.repositoryId,
          displayName: selected.names.repositoryName,
          defaultBranch: 'main',
          state: 'active',
          revision: 1,
          updatedAt: updated,
        }],
      })
    }
    if (request.query === 'settings.get') {
      return response(request, {
        revision: state.settings.revision,
        workerConcurrencyLimit: state.settings.workerConcurrencyLimit,
        defaultModelRoute: state.settings.defaultModelRoute,
      })
    }
    if (request.query === 'credential.reference.list') {
      return response(request, {
        kind: 'credential_reference_page',
        items: [{
          id: primaryCredentialReferenceId,
          providerId: primaryModelRoute.providerId,
          displayName: 'Primary provider key',
          secretState: 'available',
          rotationVersion: 1,
          lastRotatedAt: updated,
          revokedAt: null,
          revision: 1,
          updatedAt: updated,
        }, {
          id: alternateCredentialReferenceId,
          providerId: alternateModelRoute.providerId,
          displayName: 'Alternate provider key',
          secretState: 'available',
          rotationVersion: 1,
          lastRotatedAt: updated,
          revokedAt: null,
          revision: 1,
          updatedAt: updated,
          vaultLocator: SECRET_MARKER,
        }],
      })
    }
    if (request.query === 'worker.list') {
      return response(request, {
        kind: 'worker_page',
        items: [{
          id: workerId,
          state: 'enabled',
          capacity: 2,
          lastHeartbeatAt: updated,
          revision: 1,
        }],
      })
    }
    if (request.query === 'model.route.availability.list') {
      state.availabilityReads += 1
      save()
      return response(request, availability(request.scope))
    }
    if (request.query === 'session.list') {
      const items = state.sessions
        .filter(item => item.repositoryId === request.scope.repositoryId)
        .map(item => structuredClone(item))
      return response(request, { kind: 'product_session_page', items })
    }
    if (request.query === 'session.get') {
      const sessionProjection = state.sessions.find(
        item => item.id === request.parameters.productSessionId,
      )
      if (sessionProjection === undefined) {
        throw accessFailure(request, 'protocol', 'RESOURCE_NOT_FOUND', false)
      }
      return response(request, structuredClone(sessionProjection))
    }
    if (request.query === 'session.messages.list') {
      const history = state.messages.filter(
        item => item.productSessionId === request.parameters.productSessionId,
      )
      return response(request, { kind: 'chat_message_page', items: structuredClone(history) })
    }
    if (request.query === 'session.interactions.list') {
      return response(request, { kind: 'chat_interaction_page', items: [] })
    }
    if (request.query === 'approval.list') {
      return response(request, { kind: 'approval_page', items: [] })
    }
    if (request.query === 'runtime.projection.get'
      && request.parameters.kind === 'product-session') {
      return response(request, chatRuntime(request.parameters.productSessionId))
    }
    if (request.query === 'delivery.list') {
      return response(request, {
        kind: 'delivery_page',
        items: state.delivery === null ? [] : [deliverySummary()],
      })
    }
    if (state.delivery === null) throw new Error(`unexpected pre-delivery query: ${request.query}`)
    if (request.query === 'delivery.get') {
      if (request.parameters.deliveryId !== state.delivery.deliveryId) {
        throw accessFailure(request, 'protocol', 'RESOURCE_NOT_FOUND', false)
      }
      return response(request, deliveryDetail())
    }
    if (request.query === 'runtime.projection.get') {
      return response(request, deliveryRuntime())
    }
    throw new Error(`unexpected query: ${request.query}`)
  },
  subscribe(options) {
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

const root = document.querySelector('[data-winwincode-client-root]')
const application = mountWinWinCodeClient({
  root,
  serverUrl: controlPlane.serverUrl,
  controlPlane,
})

function dumpState() {
  return JSON.stringify({
    hash: location.hash,
    chat: chatState(),
    checklist: checklist(),
    slotHidden: document.querySelector('.wwc-surface-slot')?.hidden ?? null,
    boundary: document.querySelector('.wwc-client-error-boundary')?.textContent ?? null,
    connection: document.querySelector('.wwc-connection-status')?.textContent ?? null,
    availabilityReads: state.availabilityReads,
    queries: calls.queries.slice(-10).map(call => `${call.query}:${call.requestId}`),
    rejections: calls.console.filter(line => line.startsWith('unhandled ')),
  })
}

async function waitFor(predicate, label) {
  const deadline = Date.now() + 15_000
  while (Date.now() < deadline) {
    let result = false
    try {
      result = await predicate()
    } catch {}
    if (result) return
    await new Promise(resolve => { setTimeout(resolve, 20) })
  }
  throw new Error(`timed out waiting for ${label}: ${document.body.textContent.slice(0, 200)} ${dumpState()}`)
}

function hashParameter(name) {
  return new URLSearchParams(location.hash.split('?')[1] ?? '').get(name)
}

function signedIn() {
  return application.authSession.state.status === 'signed-in'
    && application.authSession.state.session !== null
}

function secretScan() {
  const storage = []
  for (const store of [localStorage, sessionStorage]) {
    for (let index = 0; index < store.length; index += 1) {
      const key = store.key(index)
      storage.push(`${key}=${store.getItem(key) ?? ''}`)
    }
  }
  const leaked = value => value.includes(BOOTSTRAP_PROOF) || value.includes(SECRET_MARKER)
  return {
    dom: leaked(document.body.textContent),
    url: leaked(location.href),
    storage: storage.some(leaked),
    console: calls.console.some(leaked),
  }
}

function checklist() {
  const section = document.querySelector('.wwc-readiness')
  if (section === null) return { present: false }
  return {
    present: true,
    hidden: section.closest('.wwc-readiness-root')?.hidden ?? null,
    summary: section.querySelector('.wwc-readiness-summary')?.textContent ?? '',
    expanded: section.querySelector('.wwc-readiness-toggle')
      ?.getAttribute('aria-expanded') === 'true',
    items: [...section.querySelectorAll('.wwc-readiness-item')].map(item => ({
      id: item.dataset.itemId,
      status: item.dataset.status,
      reason: item.querySelector('.wwc-readiness-item-reason')?.textContent ?? '',
      fix: item.querySelector('.wwc-readiness-fix')?.textContent ?? null,
    })),
  }
}

async function recheckUntil(predicate, label) {
  document.querySelector('.wwc-readiness-recheck').click()
  await waitFor(() => {
    const state = checklist()
    return state.present && state.hidden === false && predicate(state) ? state : false
  }, label)
  return checklist()
}

function chatState() {
  const articles = [...document.querySelectorAll('.wwc-chat-messages article')]
  return {
    hash: location.hash,
    heading: document.querySelector('.wwc-chat-heading')?.textContent ?? null,
    status: document.querySelector('.wwc-chat-status')?.textContent ?? null,
    empty: document.querySelector('.wwc-chat-empty')?.hidden === false
      ? document.querySelector('.wwc-chat-empty').textContent
      : null,
    error: document.querySelector('.wwc-chat-error')?.hidden === false
      ? document.querySelector('.wwc-chat-error-text').textContent
      : null,
    retryHidden: document.querySelector('.wwc-chat-retry')?.hidden ?? null,
    model: document.querySelector('.wwc-chat-model')?.selectedOptions[0]?.textContent ?? null,
    modelDisabled: document.querySelector('.wwc-chat-model')?.disabled ?? null,
    newSessionDisabled: document.querySelector('.wwc-chat-new-session')?.disabled ?? null,
    messages: articles.map(article => ({
      role: article.dataset.role,
      state: article.dataset.state,
      content: article.querySelector('p')?.textContent ?? '',
    })),
  }
}

async function chooseScope() {
  await waitFor(() => document.querySelector('#wwc-scope-organization') !== null, 'Scope selector')
  const before = {
    hash: location.hash,
    chatMounted: document.querySelector('.wwc-chat') !== null,
    slot: document.querySelector('.wwc-surface-slot')?.textContent ?? '',
    checklist: checklist(),
    secrets: secretScan(),
  }
  for (const level of ['organization', 'workspace', 'project', 'repository']) {
    const control = document.querySelector(`#wwc-scope-${level}`)
    const value = chosenIdentity[`${level}Id`]
    control.focus()
    control.value = value
    control.dispatchEvent(new Event('change', { bubbles: true }))
    await waitFor(() => hashParameter(`${level}Id`) === value, `${level} Scope selection`)
  }
  await waitFor(() => document.querySelector('.wwc-chat') !== null, 'first Chat shell')
  await waitFor(
    () => (document.querySelector('.wwc-chat-status')?.textContent?.length ?? 0) > 0,
    'Chat status',
  )
  const after = {
    hash: location.hash,
    scopeParameters: Object.fromEntries([
      'organizationId',
      'workspaceId',
      'projectId',
      'repositoryId',
    ].map(name => [name, hashParameter(name)])),
    repositoryScopeItem: checklist().items.find(item => item.id === 'repository-scope') ?? null,
    secrets: secretScan(),
  }
  return { before, after }
}

async function checklistAfterScope() {
  return recheckUntil(
    state => state.summary === 'First-run setup · 5 of 6 complete'
      && state.items.every(item => (item.id === 'first-chat-delivery'
        ? item.status === 'attention' && /No Chat session exists yet/u.test(item.reason)
        : item.status === 'ready')),
    'first-run checklist after the Scope choice',
  )
}

async function chooseModelRoute() {
  const select = document.querySelector('.wwc-chat-model')
  const before = chatState()
  select.selectedIndex = 2
  select.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor(
    () => document.querySelector('.wwc-chat-status')?.textContent === 'Ready for a new Chat',
    'chosen model route',
  )
  return { before, chosen: chatState() }
}

async function createFirstChat() {
  const before = chatState()
  document.querySelector('.wwc-chat-new-session').click()
  await waitFor(
    () => document.querySelector('.wwc-chat-heading')?.textContent === 'New Chat',
    'created first Chat',
  )
  await waitFor(() => hashParameter('session') !== null, 'first Chat URL')
  const created = chatState()
  const composer = document.querySelector('.wwc-chat-composer-input')
  composer.value = REQUIREMENT
  composer.dispatchEvent(new Event('input', { bubbles: true }))
  await waitFor(() => document.querySelector('.wwc-chat-send')?.disabled === false, 'composer')
  document.querySelector('.wwc-chat-send').click()
  await waitFor(
    () => document.querySelector('.wwc-chat-error')?.hidden === false
      && document.querySelector('.wwc-chat-retry')?.hidden === false,
    'transient requirement submission failure',
  )
  const failed = chatState()
  document.querySelector('.wwc-chat-retry').click()
  await waitFor(
    () => document.querySelector('.wwc-chat-status')?.textContent === 'Ready'
      && document.querySelector('.wwc-chat-retry')?.hidden === true
      && document.querySelector('.wwc-chat-composer-input')?.value === REQUIREMENT,
    'Chat recovery keeps the requirement draft',
  )
  const recovered = chatState()
  document.querySelector('.wwc-chat-send').click()
  await waitFor(
    () => [...document.querySelectorAll('.wwc-chat-messages article')].some(article => (
      article.dataset.role === 'user'
      && article.dataset.state === 'completed'
      && article.querySelector('p')?.textContent === REQUIREMENT
    )),
    'submitted requirement',
  )
  return { before, created, failed, recovered, delivered: chatState() }
}

async function restoreChatAfterReload() {
  await waitFor(
    () => document.querySelector('.wwc-chat-heading')?.textContent === 'New Chat',
    'restored first Chat',
  )
  await waitFor(
    () => [...document.querySelectorAll('.wwc-chat-messages article')].some(article => (
      article.querySelector('p')?.textContent === REQUIREMENT
    )),
    'restored requirement message',
  )
  return chatState()
}

async function continueRestoredChat() {
  const select = document.querySelector('.wwc-chat-model')
  select.selectedIndex = 2
  select.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor(
    () => document.querySelector('.wwc-chat-status')?.textContent === 'Ready',
    'restored Chat ready to continue',
  )
  return chatState()
}

async function convertChatToDelivery() {
  await waitFor(
    () => document.querySelector('.wwc-chat-convert-delivery')?.disabled === false,
    'Chat conversion action',
  )
  document.querySelector('.wwc-chat-convert-delivery').click()
  await waitFor(
    () => document.querySelector('.wwc-chat-convert')?.hidden === false
      && document.querySelector('.wwc-chat-convert-submit')?.disabled === false,
    'Chat conversion confirmation',
  )
  const draft = {
    title: document.querySelector('.wwc-chat-convert-title').value,
    goal: document.querySelector('.wwc-chat-convert-goal').value,
    sourceSession: document.querySelector('.wwc-chat-convert-source-session').value,
    scope: document.querySelector('.wwc-chat-convert-scope').value,
    model: document.querySelector('.wwc-chat-convert-model').value,
  }
  document.querySelector('.wwc-chat-convert-title').value = DELIVERY_TITLE
  document.querySelector('.wwc-chat-convert-baseline').value = BASELINE_REVISION
  document.querySelector('.wwc-chat-convert-criteria').value = CRITERION
  document.querySelector('.wwc-chat-convert-confirm').checked = true
  document.querySelector('.wwc-chat-convert-submit').click()
  await waitFor(
    () => document.querySelector('.wwc-strongflow-heading')?.textContent === DELIVERY_TITLE,
    'converted StrongFlow workbench',
  )
  await waitFor(
    () => calls.subscriptions.some(call => call.subscription.stream.kind === 'delivery'),
    'converted Delivery subscription',
  )
  await waitFor(
    () => [...document.querySelectorAll('.wwc-strongflow-delivery-list [data-delivery-id]')].length
      === 1,
    'converted Delivery list entry',
  )
  return {
    draft,
    strongflow: {
      hash: location.hash,
      heading: document.querySelector('.wwc-strongflow-heading').textContent,
      status: document.querySelector('.wwc-strongflow-header-status')?.textContent ?? null,
      deliveryIds: [...document.querySelectorAll(
        '.wwc-strongflow-delivery-list [data-delivery-id]',
      )].map(node => node.dataset.deliveryId),
    },
  }
}

async function restoreStrongFlowAfterReload() {
  await waitFor(
    () => document.querySelector('.wwc-strongflow-heading')?.textContent === DELIVERY_TITLE,
    'restored StrongFlow workbench',
  )
  await waitFor(
    () => calls.subscriptions.some(call => (
      call.subscription.stream.kind === 'delivery'
      && call.subscription.stream.deliveryId === hashParameter('delivery')
    )),
    'restored Delivery subscription',
  )
  return {
    hash: location.hash,
    deliveryParameter: hashParameter('delivery'),
    sessionParameter: hashParameter('session'),
    stageRunParameter: hashParameter('stageRun'),
    heading: document.querySelector('.wwc-strongflow-heading').textContent,
    status: document.querySelector('.wwc-strongflow-header-status')?.textContent ?? null,
    deliveryIds: [...document.querySelectorAll(
      '.wwc-strongflow-delivery-list [data-delivery-id]',
    )].map(node => node.dataset.deliveryId),
  }
}

globalThis.firstRunReady = () => true

globalThis.firstRunSignIn = async () => {
  await waitFor(
    () => application.authSession.state.status === 'authentication-required',
    'missing browser session',
  )
  await waitFor(
    () => document.querySelector('.wwc-auth-session-form')?.hidden === false,
    'first-run sign-in form',
  )
  const unsigned = {
    status: document.querySelector('.wwc-auth-session-status')?.textContent ?? '',
    slot: document.querySelector('.wwc-surface-slot')?.textContent ?? '',
    chatMounted: document.querySelector('.wwc-chat') !== null,
    scopeSelectorMounted: document.querySelector('#wwc-scope-organization') !== null,
    checklistHidden: document.querySelector('.wwc-readiness-root')?.hidden ?? null,
    secrets: secretScan(),
  }
  const input = document.querySelector('.wwc-auth-session-proof')
  input.value = REJECTED_PROOF
  document.querySelector('.wwc-auth-session-form').requestSubmit()
  await waitFor(
    () => document.querySelector('.wwc-auth-session-error')?.hidden === false,
    'rejected bootstrap proof',
  )
  const rejected = {
    status: document.querySelector('.wwc-auth-session-status')?.textContent ?? '',
    error: document.querySelector('.wwc-auth-session-error').textContent,
    diagnosticLeak: document.body.textContent.includes('private first-run diagnostics'),
    secrets: secretScan(),
  }
  input.value = BOOTSTRAP_PROOF
  document.querySelector('.wwc-auth-session-form').requestSubmit()
  await waitFor(() => signedIn(), 'first sign-in')
  await waitFor(
    () => document.querySelector('.wwc-auth-session-form')?.hidden === true,
    'hidden sign-in form',
  )
  return {
    unsigned,
    rejected,
    signedIn: {
      status: application.authSession.state.status,
      actor: application.authSession.state.session?.actor ?? null,
      authorizedScopeCount: application.authSession.state.session?.authorizedScopes.length ?? 0,
      secrets: secretScan(),
    },
  }
}

globalThis.firstRunChooseScope = chooseScope
globalThis.firstRunChecklistAfterScope = checklistAfterScope
globalThis.firstRunChooseModelRoute = chooseModelRoute
globalThis.firstRunCreateChat = createFirstChat
globalThis.firstRunRestoreChat = restoreChatAfterReload
globalThis.firstRunContinueRestoredChat = continueRestoredChat
globalThis.firstRunConvertDelivery = convertChatToDelivery
globalThis.firstRunRestoreStrongFlow = restoreStrongFlowAfterReload
globalThis.firstRunFinalChecklist = () => recheckUntil(
  state => state.summary === 'First-run setup complete · 6 of 6 complete'
    && state.items.every(item => item.status === 'ready' && item.fix === null),
  'completed first-run checklist',
)
globalThis.firstRunSecretScan = secretScan
globalThis.firstRunObservation = () => ({
  page: {
    url: location.href,
    hash: location.hash,
    title: document.title,
  },
  identity: {
    status: application.authSession.state.status,
    actor: application.authSession.state.session?.actor ?? null,
    authorizedScopes: application.authSession.state.session?.authorizedScopes ?? [],
  },
  workspace: {
    sessionCount: state.sessions.length,
    messageCount: state.messages.length,
    submittedRequirements: state.submittedRequirements,
    deliveryId: state.delivery?.deliveryId ?? null,
    deliveryRevision: state.delivery?.revision ?? null,
    deliveryStatus: deliverySummary()?.status ?? null,
    defaultModelRoute: state.settings.defaultModelRoute,
    availabilityReads: state.availabilityReads,
  },
  commands: calls.commands,
  queries: calls.queries,
  subscriptions: calls.subscriptions,
  console: calls.console,
  secrets: {
    bootstrapProof: BOOTSTRAP_PROOF,
    secretMarker: SECRET_MARKER,
    submittedProofs: calls.submittedProofs,
  },
})
