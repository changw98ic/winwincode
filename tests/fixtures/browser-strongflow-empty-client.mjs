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
const persistedDelivery = JSON.parse(sessionStorage.getItem('strongflow-delivery') ?? 'null')
let deliveryId = persistedDelivery?.deliveryId ?? null
let deliveryRevision = persistedDelivery?.deliveryRevision ?? 0
let deliverySpec = persistedDelivery?.deliverySpec ?? null

function persistDelivery() {
  sessionStorage.setItem('strongflow-delivery', JSON.stringify({
    deliveryId,
    deliveryRevision,
    deliverySpec,
  }))
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
    status: 'enabled',
    reason: 'ready',
    items: [{
      route: modelRoute,
      providerDisplayName: 'Browser Provider',
      modelDisplayName: 'Browser Model',
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

function ownership() {
  return {
    organizationId: scope.organizationId,
    workspaceId: scope.workspaceId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
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

function summary() {
  return {
    schemaVersion,
    deliveryId,
    revision: deliveryRevision,
    status: deliveryRevision === 1 ? 'draft' : 'clarifying',
    title: deliverySpec.title,
    updatedAt: '2026-09-02T01:00:00.000Z',
    ownership: ownership(),
    activeStageRunId: deliveryRevision === 1 ? null : stageRunId,
    openAttentionCount: 0,
    taskCounts: taskCounts(),
  }
}

function readCursor() {
  return {
    token: 'cursor_00000000000000000000000000000002',
    scope,
    deliveryId,
    deliveryRevision: 2,
    runtimeLedgerRevision: 1,
    runtimeAcceptedSequence: 0,
    publicationRevision: 0,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 2,
      eventId: 'evt_00000000000000000000000002',
    },
  }
}

function binding() {
  return {
    bindingId: 'binding:strongflow:empty-browser',
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

function detail() {
  return {
    kind: 'delivery_detail',
    schemaVersion,
    deliveryId,
    deliveryRevision: 2,
    readCursor: readCursor(),
    ownership: ownership(),
    status: 'clarifying',
    requirements: {
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 1,
      title: deliverySpec.title,
      goal: deliverySpec.goal,
      scope: [],
      outOfScope: [],
      constraints: [],
      acceptanceCriteria: deliverySpec.acceptanceCriteria.map(criterion => ({
        id: criterion.id,
        description: criterion.title,
        verificationMethod: null,
        required: criterion.required,
      })),
      sourceRef: null,
      publicationTarget: null,
      repository: { kind: 'local-git', locator: 'workspace://repository' },
      baseRevision: deliverySpec.baseRevision,
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
      sessionBinding: binding(),
      stage: 'clarifying',
      startedAt: '2026-09-02T01:00:00.000Z',
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
    deliveryId,
    stageRunId,
    readCursor: cursor,
    eventCursor: cursor.eventCursor,
    lastProjectionSequence: 0,
    revision: 1,
    rebuiltAt: '2026-09-02T01:00:00.000Z',
    sessions: [],
  }
}

function chatSession() {
  return {
    id: chatProductSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision: 3,
    state: 'idle',
    title: 'Confirmed requirements Chat',
    updatedAt: '2026-09-02T00:30:00.000Z',
  }
}

function chatMessages() {
  return [{
    id: 'msg_00000000000000000000000001',
    productSessionId: chatProductSessionId,
    role: 'user',
    content: 'Build the requirement confirmed by this Chat.',
    sequence: 1,
    state: 'completed',
    createdAt: '2026-09-02T00:30:00.000Z',
    updatedAt: '2026-09-02T00:30:00.000Z',
  }, {
    id: 'msg_00000000000000000000000002',
    productSessionId: chatProductSessionId,
    role: 'assistant',
    content: 'The requirement is confirmed and ready for StrongFlow.',
    sequence: 2,
    state: 'completed',
    createdAt: '2026-09-02T00:31:00.000Z',
    updatedAt: '2026-09-02T00:31:00.000Z',
  }]
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

const controlPlane = {
  serverUrl: 'https://control.localhost',
  async restore() { return structuredClone(browserSession) },
  async login() { return structuredClone(browserSession) },
  async logout() {},
  async query(request) {
    calls.queries.push(structuredClone(request))
    if (request.query === 'delivery.list') {
      return response(request, {
        kind: 'delivery_page',
        items: deliveryRevision === 0 ? [] : [summary()],
      })
    }
    if (request.query === 'session.list') {
      return response(request, { kind: 'product_session_page', items: [chatSession()] })
    }
    if (request.query === 'model.route.availability.list') {
      return response(request, routeAvailability())
    }
    if (request.query === 'session.get') return response(request, chatSession())
    if (request.query === 'session.messages.list') {
      return response(request, { kind: 'chat_message_page', items: chatMessages() })
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
    if (deliveryRevision < 2) throw new Error(`unexpected pre-advance query: ${request.query}`)
    if (request.query === 'delivery.get') return response(request, detail())
    if (request.query === 'runtime.projection.get') return response(request, deliveryRuntime())
    throw new Error(`unexpected query: ${request.query}`)
  },
  async command(request) {
    calls.commands.push(structuredClone(request))
    if (request.command === 'delivery.create') {
      deliveryId = request.payload.deliveryId
      deliverySpec = structuredClone(request.payload.spec)
      deliveryRevision = 1
    } else if (request.command === 'delivery.advance') {
      deliveryRevision = 2
    } else throw new Error(`unexpected command: ${request.command}`)
    persistDelivery()
    return {
      schemaVersion,
      requestId: request.requestId,
      command: request.command,
      outcome: 'completed',
      previousRevision: request.expectedRevision,
      currentRevision: deliveryRevision,
      result: summary(),
    }
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

globalThis.runEmptyStrongFlowScenario = async () => {
  await waitFor(
    () => document.querySelector('.wwc-strongflow-create-submit')?.disabled === false,
    'empty StrongFlow creation form',
  )
  const empty = {
    hash: location.hash,
    submitDisabled: document.querySelector('.wwc-strongflow-create-submit').disabled,
    text: document.querySelector('.wwc-strongflow-create-content').textContent,
  }
  document.querySelector('.wwc-strongflow-create-title').value = 'First StrongFlow Delivery'
  document.querySelector('.wwc-strongflow-create-goal').value
    = 'Enter StrongFlow from an empty repository.'
  document.querySelector('.wwc-strongflow-create-baseline').value
    = '0123456789abcdef0123456789abcdef01234567'
  document.querySelector('.wwc-strongflow-create-criteria').value
    = 'The real Delivery snapshot opens.\nDelivery events are subscribed.'
  document.querySelector('.wwc-strongflow-create-submit').click()

  await waitFor(
    () => document.querySelector('.wwc-strongflow-heading')?.textContent
      === 'First StrongFlow Delivery',
    'created StrongFlow snapshot',
  )
  await waitFor(
    () => calls.subscriptions.some(call => call.subscription.stream.kind === 'delivery'),
    'created Delivery subscription',
  )
  return {
    empty,
    created: {
      hash: location.hash,
      heading: document.querySelector('.wwc-strongflow-heading').textContent,
      listDeliveryIds: [...document.querySelectorAll('[data-delivery-id]')]
        .map(node => node.dataset.deliveryId),
      status: document.querySelector('.wwc-strongflow-status').textContent,
    },
    calls,
    deliveryId,
    navigationEntryCount: performance.getEntriesByType('navigation').length,
    scope,
  }
}

globalThis.runChatConversionScenario = async () => {
  await waitFor(
    () => document.querySelector('.wwc-chat-heading')?.textContent
      === 'Confirmed requirements Chat',
    'confirmed Chat snapshot',
  )
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
    goal: document.querySelector('.wwc-chat-convert-goal').value,
    model: document.querySelector('.wwc-chat-convert-model').value,
    scope: document.querySelector('.wwc-chat-convert-scope').value,
    sourceSession: document.querySelector('.wwc-chat-convert-source-session').value,
    title: document.querySelector('.wwc-chat-convert-title').value,
  }
  document.querySelector('.wwc-chat-convert-baseline').value
    = '0123456789abcdef0123456789abcdef01234567'
  document.querySelector('.wwc-chat-convert-criteria').value
    = 'The confirmed requirement is delivered.'
  document.querySelector('.wwc-chat-convert-confirm').checked = true
  document.querySelector('.wwc-chat-convert-submit').click()

  await waitFor(
    () => document.querySelector('.wwc-strongflow-heading')?.textContent
      === 'Confirmed requirements Chat',
    'converted StrongFlow snapshot',
  )
  await waitFor(
    () => calls.subscriptions.some(call => call.subscription.stream.kind === 'delivery'),
    'converted Delivery subscription',
  )
  return {
    actor,
    calls,
    deliveryId,
    draft,
    scope,
    created: {
      hash: location.hash,
      heading: document.querySelector('.wwc-strongflow-heading').textContent,
      listDeliveryIds: [...document.querySelectorAll('[data-delivery-id]')]
        .map(node => node.dataset.deliveryId),
      status: document.querySelector('.wwc-strongflow-status').textContent,
    },
  }
}

globalThis.inspectConvertedAfterReload = async () => {
  await waitFor(
    () => document.querySelector('.wwc-strongflow-heading')?.textContent
      === 'Confirmed requirements Chat',
    'restored converted StrongFlow',
  )
  await waitFor(
    () => calls.subscriptions.some(call => call.subscription.stream.kind === 'delivery'),
    'restored Delivery subscription',
  )
  return {
    deliverySubscribed: calls.subscriptions.some(call => (
      call.subscription.stream.kind === 'delivery'
      && call.subscription.stream.deliveryId === deliveryId
    )),
    hash: location.hash,
    heading: document.querySelector('.wwc-strongflow-heading').textContent,
    status: document.querySelector('.wwc-strongflow-status').textContent,
  }
}
