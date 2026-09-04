import { mountWinWinCodeClient } from '/module/application.js'

const serverConfiguration = await fetch('/fixture/server-url.json', {
  cache: 'no-store',
}).then(async response => {
  if (!response.ok) throw new Error('fixture server URL is unavailable')
  return response.json()
})
const root = document.querySelector('[data-winwincode-client-root]')
const application = mountWinWinCodeClient({
  root,
  serverUrl: serverConfiguration.serverUrl,
})
const transportFailures = []
const subscriptionFailures = []
const eventHandlerFailures = []
for (const operation of ['command', 'query']) {
  const invoke = application.controlPlane[operation].bind(application.controlPlane)
  application.controlPlane[operation] = async (request, options) => {
    try {
      return await invoke(request, options)
    } catch (error) {
      transportFailures.push({
        code: error?.code ?? 'UNKNOWN',
        kind: error?.kind ?? 'unknown',
        name: request[operation],
        operation,
        requestId: request.requestId,
      })
      throw error
    }
  }
}
const subscribe = application.controlPlane.subscribe.bind(application.controlPlane)
application.controlPlane.subscribe = options => subscribe({
  ...options,
  async onEvent(frame) {
    try {
      return await options.onEvent(frame)
    } catch (error) {
      eventHandlerFailures.push({
        code: error?.code ?? 'UNKNOWN',
        kind: error?.kind ?? 'unknown',
        type: frame?.event?.type ?? 'unknown',
      })
      throw error
    }
  },
  onError(error) {
    subscriptionFailures.push({
      code: error?.code ?? 'UNKNOWN',
      kind: error?.kind ?? 'unknown',
      reason: error?.details?.reason ?? null,
    })
    options.onError(error)
  },
})
const productSessionId = 'psn_01J00000000000000000000001'
const deliveryId = 'dlv_01J00000000000000000000001'
const credentialReferenceId = 'crd_01J00000000000000000000001'
let requestSequence = 0
const WORKFLOW_TIMEOUT_MILLIS = 240_000
const MAX_STRONGFLOW_TRANSITIONS = 64
const HUMAN_ACTION_SETTLE_MILLIS = 1_000
const forbiddenRequestPaths = serverConfiguration.forbiddenRequestPathPatterns
  .map(pattern => new RegExp(pattern, 'u'))

function id(prefix) {
  requestSequence += 1
  return `${prefix}_${String(requestSequence).padStart(26, '0')}`
}

function page(limit = 50) {
  return { cursor: null, limit }
}

async function waitFor(predicate, label, timeoutMillis = 20_000) {
  const deadline = Date.now() + timeoutMillis
  for (;;) {
    if (await predicate()) return
    if (Date.now() >= deadline) {
      throw new Error(`timed out waiting for ${label}: ${document.body.textContent}`)
    }
    await new Promise(resolve => { setTimeout(resolve, 20) })
  }
}

function submitProof(value) {
  const input = document.querySelector('.wwc-auth-session-proof')
  const form = document.querySelector('.wwc-auth-session-form')
  input.value = value
  form.requestSubmit()
}

function context() {
  const session = application.authSession.state.session
  if (application.authSession.state.status !== 'signed-in' || session === null) {
    throw new Error('signed-in context is unavailable')
  }
  const scope = session.authorizedScopes.find(candidate => candidate.kind === 'repository')
  if (scope === undefined) throw new Error('repository Scope is unavailable')
  return { actor: session.actor, scope }
}

function query(queryName, parameters, limit = 50) {
  const { actor, scope } = context()
  return application.controlPlane.query({
    schemaVersion: 'winwincode/v1',
    requestId: id('req'),
    actor,
    scope,
    query: queryName,
    parameters,
    page: page(limit),
  })
}

function command(commandName, expectedRevision, payload) {
  const { actor, scope } = context()
  return application.controlPlane.command({
    schemaVersion: 'winwincode/v1',
    requestId: id('req'),
    actor,
    scope,
    command: commandName,
    expectedRevision,
    payload,
  })
}

async function driveStrongFlowScheduler(expectedRevision) {
  try {
    const response = await command('delivery.advance', expectedRevision, { deliveryId })
    if (
      response.outcome !== 'completed'
      || response.command !== 'delivery.advance'
      || response.previousRevision !== expectedRevision
      || response.currentRevision <= expectedRevision
    ) {
      throw new Error(`delivery.advance returned an invalid scheduler transition: ${JSON.stringify({
        command: response.command,
        currentRevision: response.currentRevision,
        outcome: response.outcome,
        previousRevision: response.previousRevision,
      })}`)
    }
    return response
  } catch (error) {
    if ([
      'REVISION_CONFLICT',
      'TRUSTED_FACTS_UNAVAILABLE',
      'WRONG_STATE',
    ].includes(error?.code)) return null
    throw error
  }
}

async function navigate(hash, selector) {
  location.hash = hash
  await waitFor(() => document.querySelector(selector) !== null, selector)
}

function visibleText(selector) {
  return document.querySelector(selector)?.textContent?.replace(/\s+/gu, ' ').trim() ?? null
}

function chatMessages() {
  return [...document.querySelectorAll('.wwc-chat-messages article')].map(node => ({
    content: node.querySelector('p')?.textContent ?? '',
    role: node.dataset.role ?? '',
    state: node.dataset.state ?? '',
  }))
}

function chatTurnIsTerminal() {
  return ['Ready', 'Completed'].includes(visibleText('.wwc-chat-status'))
    && chatMessages().some(message => (
      message.role === 'assistant'
      && message.state === 'completed'
      && message.content.length > 0
    ))
}

function strongFlowDomSnapshot() {
  const statusText = visibleText('.wwc-strongflow-status')
  const readyStatus = statusText?.match(/^([a-z-]+) · revision (\d+)$/u) ?? null
  const metadata = visibleText('.wwc-strongflow-metadata')
  const metadataRevision = metadata?.match(/^Delivery r(\d+) ·/u) ?? null
  return {
    attention: [...document.querySelectorAll('.wwc-strongflow-attention-list > li')]
      .map(node => node.dataset.status ?? ''),
    deliveryHeading: visibleText('.wwc-strongflow-heading'),
    error: visibleText('.wwc-strongflow-error-text'),
    revision: metadataRevision === null
      ? readyStatus === null ? null : Number(readyStatus[2])
      : Number(metadataRevision[1]),
    stageCount: document.querySelectorAll('.wwc-strongflow-stage-list > li').length,
    status: readyStatus?.[1] ?? null,
    statusText,
    tasks: [...document.querySelectorAll('.wwc-strongflow-task-list > li')]
      .map(node => node.dataset.status ?? ''),
    ...browserEvidence(),
  }
}

function summarizedTransportFailures() {
  const counts = new Map()
  for (const { code, kind, name, operation } of transportFailures) {
    const identity = JSON.stringify({ code, kind, name, operation })
    counts.set(identity, (counts.get(identity) ?? 0) + 1)
  }
  return [...counts.entries()]
    .map(([identity, count]) => ({ ...JSON.parse(identity), count }))
    .sort((left, right) => JSON.stringify(left).localeCompare(JSON.stringify(right)))
}

function strongFlowFailure(snapshot) {
  if (snapshot.error !== null && snapshot.error.length > 0) {
    const last = transportFailures.slice(-5).map(({ code, kind, name, operation }) => ({
      code,
      kind,
      name,
      operation,
    }))
    return new Error(`StrongFlow reported an error: ${snapshot.error}; transport=${JSON.stringify({
      last,
      eventHandler: eventHandlerFailures.slice(-5),
      summary: summarizedTransportFailures(),
      subscription: subscriptionFailures.slice(-5),
    })}`)
  }
  if (snapshot.statusText === 'StrongFlow unavailable') {
    return new Error(`StrongFlow became unavailable: ${document.body.textContent}`)
  }
  return null
}

function deliveryDiagnostic(detail) {
  return {
    attentionStatuses: detail.attention.map(item => item.status),
    candidatePresent: detail.currentCandidate !== null,
    revision: detail.deliveryRevision,
    solutionReviewStatus: detail.solutionReview?.reviewStatus ?? null,
    stages: detail.stages.map(stage => ({
      attempt: stage.attempt,
      role: stage.role,
      status: stage.status,
    })),
    status: detail.status,
    taskStatuses: detail.tasks.map(task => task.status),
    verdictStatus: detail.verdict?.status ?? null,
  }
}

async function waitForStrongFlowSnapshot(label, predicate = () => true, timeoutMillis = 20_000) {
  let result = null
  try {
    await waitFor(async () => {
      const snapshot = strongFlowDomSnapshot()
      const failure = strongFlowFailure(snapshot)
      if (failure !== null) {
        try {
          failure.message += `; delivery=${JSON.stringify(deliveryDiagnostic(await deliveryDetail()))}`
        } catch (error) {
          failure.message += `; deliveryDiagnostic=${JSON.stringify({
            code: error?.code ?? 'UNKNOWN',
            kind: error?.kind ?? 'unknown',
          })}`
        }
        throw failure
      }
      if (snapshot.status === null || snapshot.revision === null || !predicate(snapshot)) return false
      result = snapshot
      return true
    }, label, timeoutMillis)
  } catch (error) {
    let delivery
    try {
      delivery = deliveryDiagnostic(await deliveryDetail())
    } catch (diagnosticError) {
      delivery = {
        code: diagnosticError?.code ?? 'UNKNOWN',
        kind: diagnosticError?.kind ?? 'unknown',
      }
    }
    if (error instanceof Error) {
      error.message += `; strongFlowDiagnostic=${JSON.stringify({
        delivery,
        eventHandler: eventHandlerFailures.slice(-5),
        summary: summarizedTransportFailures(),
        subscription: subscriptionFailures.slice(-5),
      })}`
    }
    throw error
  }
  return result
}

function strongFlowAction() {
  const actions = [
    ['approve-solution', '.wwc-strongflow-approve-solution'],
    ['approve-tasks', '.wwc-strongflow-approve-tasks'],
    ['submit-verdict', '.wwc-strongflow-submit-verdict'],
    ['resolve-attention', '.wwc-strongflow-resolve-attention'],
    ['advance-delivery', '.wwc-strongflow-advance-delivery'],
  ]
  for (const [name, selector] of actions) {
    const button = document.querySelector(selector)
    if (button !== null && !button.disabled) return { button, name }
  }
  return null
}

function clickStrongFlowAction(action) {
  if (action.name === 'approve-solution') {
    const group = action.button.closest('.wwc-strongflow-solution-actions')
    const comments = group?.querySelector('textarea')
    if (comments !== null && comments !== undefined) {
      comments.value = 'Approve the current sealed solution review.'
    }
  }
  if (action.name === 'resolve-attention') {
    const group = action.button.closest('.wwc-strongflow-attention-actions')
    const resolution = group?.querySelector('textarea')
    if (resolution === null || resolution === undefined) {
      throw new Error('the current Attention action has no decision note input')
    }
    resolution.value = 'Approve the current bounded StrongFlow decision.'
  }
  action.button.click()
}

async function deliveryDetail() {
  const response = await query('delivery.get', { deliveryId }, 1)
  return response.result
}

async function canonicalChatMessagesBytes() {
  const response = await query('session.messages.list', { productSessionId })
  if (response.query !== 'session.messages.list') {
    throw new Error('the Chat message query returned another projection')
  }
  return JSON.stringify(response.result.items)
}

async function chatRuntimeSessions() {
  const response = await query('runtime.projection.get', {
    kind: 'product-session',
    productSessionId,
  }, 1)
  if (response.query !== 'runtime.projection.get') {
    throw new Error('the Chat runtime query returned another projection')
  }
  return response.result.sessions
}

function canonicalDeliveryBytes(detail) {
  return JSON.stringify(detail)
}

function authSessionBytes() {
  const session = application.authSession.state.session
  if (application.authSession.state.status !== 'signed-in' || session === null) {
    throw new Error('the authenticated browser session is unavailable')
  }
  return JSON.stringify(session)
}

function stageProvenance(detail) {
  return detail.stages.map(stage => ({
    actorType: stage.actorType,
    id: stage.id,
    role: stage.role.toLowerCase(),
    status: stage.status,
    bindingReady: stage.sessionBinding !== null
      && stage.sessionBinding.workerSessionId !== null
      && stage.sessionBinding.codexThreadId !== null
      && stage.sessionBinding.sessionIdentity !== null
      && (
        stage.sessionBinding.stageRunId === null
        || stage.sessionBinding.stageRunId === stage.id
      )
      && stage.sessionBinding.sessionIdentity.stageRunId === stage.id
      && stage.sessionBinding.sessionIdentity.productSessionId
        === stage.sessionBinding.productSessionId
      && stage.sessionBinding.sessionIdentity.workerSessionId
        === stage.sessionBinding.workerSessionId
      && stage.sessionBinding.sessionIdentity.codexThreadId
        === stage.sessionBinding.codexThreadId,
  }))
}

function strongFlowHash(detail) {
  const stage = [...detail.stages].reverse().find(candidate => candidate.sessionBinding !== null)
  if (stage?.sessionBinding === null || stage?.sessionBinding === undefined) {
    throw new Error('the Delivery has no canonical Codex StageRun binding')
  }
  return `#/strongflow?delivery=${deliveryId}`
    + `&session=${stage.sessionBinding.productSessionId}&stageRun=${stage.id}`
}

function browserEvidence() {
  const resources = performance.getEntriesByType('resource').map(entry => entry.name)
  return {
    hash: location.hash,
    navigation: [...document.querySelectorAll('.wwc-navigation-link')].map(link => ({
      current: link.getAttribute('aria-current'),
      label: link.textContent,
    })),
    resources,
    legacyBackendRequests: resources.filter(forbiddenRequestUrl),
  }
}

function forbiddenRequestUrl(value) {
  try {
    const path = decodeURIComponent(new URL(value, location.href).pathname)
    return forbiddenRequestPaths.some(pattern => pattern.test(path))
  } catch {
    return true
  }
}

async function assertChatSnapshotQueries() {
  const probes = [
    ['session.list', { states: [] }],
    ['session.get', { productSessionId }],
    ['session.messages.list', { productSessionId }],
    ['settings.get', {}],
    ['runtime.projection.get', { kind: 'product-session', productSessionId }],
    ['session.interactions.list', { productSessionId, states: ['pending'] }],
    ['approval.list', { states: ['pending'] }],
  ]
  const failures = []
  for (const [queryName, parameters] of probes) {
    try {
      await query(queryName, parameters)
    } catch (error) {
      failures.push({
        query: queryName,
        code: error.code,
        kind: error.kind,
        message: error.message,
      })
    }
  }
  if (failures.length > 0) {
    throw new Error(`Chat snapshot probes failed: ${JSON.stringify(failures)}`)
  }
}

globalThis.runChatStrongFlowSetup = async proof => {
  await waitFor(
    () => ['signed-out', 'authentication-required'].includes(
      application.authSession.state.status,
    ),
    'initial unauthenticated session restore',
  )
  submitProof(proof)
  await waitFor(() => application.authSession.state.status === 'signed-in', 'signed-in session')
  const { scope } = context()
  await command('session.create', 0, {
    productSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    title: 'Browser production Chat',
    modelSelection: {
      providerId: 'winwincode-loopback',
      modelId: 'loopback-model',
      accountSource: { kind: 'system_default' },
    },
  })
  await navigate(`#/chat?session=${productSessionId}`, '.wwc-chat')
  await waitFor(
    () => !['Loading Chat…', 'Updating Chat…'].includes(visibleText('.wwc-chat-status')),
    'initial Chat snapshot',
  )
  await command('chat.submit', 1, {
    productSessionId,
    message: 'Run the deterministic local browser workflow.',
  })
  await waitFor(
    chatTurnIsTerminal,
    'completed Chat before StrongFlow admission',
    WORKFLOW_TIMEOUT_MILLIS,
  )
  await command('delivery.create', 0, {
    deliveryId,
    spec: {
      acceptanceCriteria: [{
        id: 'browser-terminal-criterion',
        required: true,
        title: 'The local production workflow reaches one terminal projection',
      }],
      baseRevision: serverConfiguration.repositoryBaseline,
      goal: 'Verify Chat and StrongFlow through the canonical local backend',
      publicationTarget: null,
      repositoryId: scope.repositoryId,
      title: 'Browser production StrongFlow',
    },
    tasks: [],
  })
  const advance = await command('delivery.advance', 1, { deliveryId })
  if (advance.outcome !== 'completed') {
    throw new Error('the initial delivery.advance did not complete synchronously')
  }
  const detail = await query('delivery.get', { deliveryId }, 1)
  const stage = [...detail.result.stages].reverse()
    .find(candidate => candidate.sessionBinding !== null)
  if (stage?.sessionBinding === null || stage?.sessionBinding === undefined) {
    throw new Error('delivery.advance did not create the canonical StageRun binding')
  }

  await assertChatSnapshotQueries()
  await waitFor(
    () => [...document.querySelectorAll('.wwc-chat-messages article p')]
      .some(node => node.textContent === 'Run the deterministic local browser workflow.'),
    'Chat message projection',
  )
  return {
    authSessionBytes: authSessionBytes(),
    chatHash: location.hash,
    chatHeading: visibleText('.wwc-chat-heading'),
    chatMessages: chatMessages(),
    chatStatus: visibleText('.wwc-chat-status'),
    deliveryRevision: advance.currentRevision,
    deliveryStatus: advance.result.status,
    deliveryId,
    productSessionId: stage.sessionBinding.productSessionId,
    stageRunId: stage.id,
    ...browserEvidence(),
  }
}

globalThis.openStrongFlowBrowserFixture = async () => {
  const detail = await query('delivery.get', { deliveryId }, 1)
  await navigate(strongFlowHash(detail.result), '.wwc-strongflow')
  await waitFor(
    () => !['Loading StrongFlow…', 'Updating StrongFlow…'].includes(
      visibleText('.wwc-strongflow-status'),
    ),
    'StrongFlow snapshot',
  )
  const snapshot = await waitForStrongFlowSnapshot('initial StrongFlow projection')
  return { ...snapshot, deliveryStatus: detail.result.status }
}

globalThis.waitForTerminalChatBrowserFixture = async () => {
  await navigate(`#/chat?session=${productSessionId}`, '.wwc-chat')
  await waitFor(
    chatTurnIsTerminal,
    'completed Chat with a non-empty assistant message',
    WORKFLOW_TIMEOUT_MILLIS,
  )
  return {
    authSessionBytes: authSessionBytes(),
    canonicalMessagesBytes: await canonicalChatMessagesBytes(),
    runtimeSessions: await chatRuntimeSessions(),
    heading: visibleText('.wwc-chat-heading'),
    messages: chatMessages(),
    status: visibleText('.wwc-chat-status'),
    ...browserEvidence(),
  }
}

globalThis.runStrongFlowToDelivered = async () => {
  const initialDetail = await deliveryDetail()
  await navigate(strongFlowHash(initialDetail), '.wwc-strongflow')
  let snapshot = await waitForStrongFlowSnapshot(
    'StrongFlow workflow projection',
    () => true,
    WORKFLOW_TIMEOUT_MILLIS,
  )
  const observations = []
  const actions = []
  const deadline = Date.now() + WORKFLOW_TIMEOUT_MILLIS

  function observe(value) {
    const observation = {
      revision: value.revision,
      status: value.status,
    }
    const previous = observations.at(-1)
    if (previous?.revision === observation.revision && previous.status === observation.status) return
    if (observations.length >= MAX_STRONGFLOW_TRANSITIONS) {
      throw new Error('StrongFlow exceeded the bounded transition trace')
    }
    observations.push(observation)
  }

  for (;;) {
    observe(snapshot)
    if (snapshot.status === 'delivered') break
    const remaining = deadline - Date.now()
    if (remaining <= 0) {
      throw new Error(`StrongFlow did not reach delivered: ${JSON.stringify(observations)}`)
    }
    const action = strongFlowAction()
    if (action !== null) {
      const before = snapshot
      await new Promise(resolve => { setTimeout(resolve, HUMAN_ACTION_SETTLE_MILLIS) })
      const settled = await waitForStrongFlowSnapshot(
        `stable StrongFlow ${action.name} action`,
        () => true,
        Math.min(20_000, remaining),
      )
      const settledAction = strongFlowAction()
      if (
        settled.revision !== before.revision
        || settled.status !== before.status
        || settledAction?.name !== action.name
      ) {
        snapshot = settled
        continue
      }
      clickStrongFlowAction(settledAction)
      snapshot = await waitForStrongFlowSnapshot(
        `StrongFlow ${action.name} event refresh`,
        candidate => candidate.revision > before.revision,
        remaining,
      )
      actions.push({
        action: action.name,
        fromRevision: before.revision,
        fromStatus: before.status,
        toRevision: snapshot.revision,
        toStatus: snapshot.status,
      })
      if (actions.length > MAX_STRONGFLOW_TRANSITIONS) {
        throw new Error('StrongFlow exceeded the bounded human-action trace')
      }
      continue
    }
    const beforeRevision = snapshot.revision
    const schedulerTransition = await driveStrongFlowScheduler(beforeRevision)
    if (schedulerTransition === null) {
      await new Promise(resolve => { setTimeout(resolve, 50) })
      snapshot = await waitForStrongFlowSnapshot(
        'the local scheduler retry projection',
        () => true,
        Math.min(20_000, remaining),
      )
      continue
    }
    snapshot = await waitForStrongFlowSnapshot(
      'the local scheduler Delivery event refresh',
      candidate => candidate.revision >= schedulerTransition.currentRevision,
      remaining,
    )
  }

  const detail = await deliveryDetail()
  if (detail.status !== 'delivered') {
    throw new Error(`the terminal DOM disagrees with delivery.get: ${detail.status}`)
  }
  const terminalHash = strongFlowHash(detail)
  if (location.hash !== terminalHash) {
    await navigate(terminalHash, '.wwc-strongflow')
    snapshot = await waitForStrongFlowSnapshot(
      'terminal StrongFlow deep link',
      candidate => candidate.status === 'delivered',
      20_000,
    )
  }
  return {
    ...snapshot,
    actions,
    candidatePresent: detail.currentCandidate !== null,
    candidateProducerStageRunId: detail.currentCandidate?.producerStageRunId ?? null,
    canonicalDeliveryBytes: canonicalDeliveryBytes(detail),
    deliveryStatus: detail.status,
    openAttentionCount: detail.attention.filter(item => item.status === 'open').length,
    observations,
    stageRoles: [...new Set(detail.stages.map(stage => stage.role.toLowerCase()))].sort(),
    stageProvenance: stageProvenance(detail),
    taskStatuses: detail.tasks.map(task => task.status),
    transportFailures: transportFailures.map(({ code, kind, name, operation }) => ({
      code,
      kind,
      name,
      operation,
    })),
    verdictCriteria: detail.verdict?.criteria.map(criterion => criterion.verdict) ?? [],
    verdictStatus: detail.verdict?.status ?? null,
  }
}

globalThis.inspectTerminalStrongFlowAfterReload = async () => {
  await waitFor(() => application.authSession.state.status === 'signed-in', 'restored session')
  await waitFor(() => document.querySelector('.wwc-strongflow') !== null, 'reloaded StrongFlow')
  const snapshot = await waitForStrongFlowSnapshot(
    'reloaded terminal StrongFlow snapshot',
    candidate => candidate.status === 'delivered',
  )
  const detail = await deliveryDetail()
  return {
    ...snapshot,
    authSessionBytes: authSessionBytes(),
    canonicalDeliveryBytes: canonicalDeliveryBytes(detail),
    deliveryStatus: detail.status,
  }
}

globalThis.openTerminalChatAfterStrongFlowReload = async () => {
  await navigate(`#/chat?session=${productSessionId}`, '.wwc-chat')
  await waitFor(chatTurnIsTerminal, 'restored terminal Chat')
  return {
    authSessionBytes: authSessionBytes(),
    canonicalMessagesBytes: await canonicalChatMessagesBytes(),
    messages: chatMessages(),
    status: visibleText('.wwc-chat-status'),
    ...browserEvidence(),
  }
}

globalThis.inspectTerminalChatAfterReload = async () => {
  await waitFor(() => application.authSession.state.status === 'signed-in', 'restored session')
  await waitFor(() => document.querySelector('.wwc-chat') !== null, 'reloaded Chat')
  await waitFor(chatTurnIsTerminal, 'reloaded terminal Chat')
  return {
    authSessionBytes: authSessionBytes(),
    canonicalMessagesBytes: await canonicalChatMessagesBytes(),
    messages: chatMessages(),
    status: visibleText('.wwc-chat-status'),
    ...browserEvidence(),
  }
}
