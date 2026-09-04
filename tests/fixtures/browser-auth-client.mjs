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
const browserSockets = []
const transportFrameTypes = []
const transportFrames = []
const BrowserWebSocket = globalThis.WebSocket
globalThis.WebSocket = class TrackingWebSocket extends BrowserWebSocket {
  constructor(...arguments_) {
    super(...arguments_)
    browserSockets.push(this)
    this.addEventListener('message', event => {
      try {
        const frame = JSON.parse(event.data)
        transportFrames.push(frame)
        transportFrameTypes.push(frame.type)
      } catch {}
    })
  }
}
let activeSubscription = null
const receivedEvents = []
const capturedConsole = []
for (const method of ['debug', 'error', 'info', 'log', 'warn']) {
  const original = console[method].bind(console)
  console[method] = (...values) => {
    capturedConsole.push(values.map(value => String(value)).join(' '))
    original(...values)
  }
}

const productSessionId = 'psn_01J00000000000000000000000'
const credentialReferenceId = 'crd_01J00000000000000000000000'
const deliveryId = 'dlv_01J00000000000000000000000'
const publicationId = 'pub_01J00000000000000000000000'

function id(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function page(limit = 25) {
  return { cursor: null, limit }
}

function waitFor(predicate, label) {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + 15_000
    const poll = () => {
      if (predicate()) {
        resolve()
        return
      }
      if (Date.now() >= deadline) {
        reject(new Error(
          `timed out waiting for ${label}; frames=${JSON.stringify(transportFrames)}; events=${JSON.stringify(receivedEvents)}; console=${capturedConsole.join(' | ')}`,
        ))
        return
      }
      setTimeout(poll, 20)
    }
    poll()
  })
}

function submitProof(value) {
  const input = document.querySelector('.wwc-auth-session-proof')
  const form = document.querySelector('.wwc-auth-session-form')
  input.value = value
  form.requestSubmit()
  return input.value
}

function storageText(storage) {
  return Array.from({ length: storage.length }, (_, index) => {
    const key = storage.key(index)
    return `${key ?? ''}:${key === null ? '' : storage.getItem(key) ?? ''}`
  }).join('\n')
}

function authenticatedContext() {
  const session = application.authSession.state.session
  if (application.authSession.state.status !== 'signed-in' || session === null) {
    throw new Error('browser session context is unavailable')
  }
  const scope = session.authorizedScopes.find(candidate => candidate.kind === 'repository')
  if (scope === undefined) throw new Error('repository Scope is unavailable')
  return { actor: session.actor, scope }
}

function query(requestId, query, parameters, queryScope = null) {
  const context = authenticatedContext()
  return application.controlPlane.query({
    schemaVersion: 'winwincode/v1',
    requestId: id('req', requestId),
    actor: context.actor,
    scope: queryScope ?? context.scope,
    query,
    parameters,
    page: page(),
  })
}

function command(requestId, operation, expectedRevision, payload, commandScope = null) {
  const context = authenticatedContext()
  return application.controlPlane.command({
    schemaVersion: 'winwincode/v1',
    requestId: id('req', requestId),
    actor: context.actor,
    scope: commandScope ?? context.scope,
    command: operation,
    expectedRevision,
    payload,
  })
}

async function runRealApplicationFlows() {
  const { actor, scope } = authenticatedContext()
  const delivery = await query(10, 'delivery.list', { states: [] })
  const workers = await query(11, 'worker.list', { states: [] })
  const approvals = await query(12, 'approval.list', { states: [] })
  const settingsBefore = await query(13, 'settings.get', {})
  const settingsUpdate = await command(14, 'settings.update', 0, {
    patch: {
      defaultModelRoute: null,
      workerConcurrencyLimit: 2,
    },
  })
  const settingsAfter = await query(15, 'settings.get', {})
  const createdDelivery = await command(16, 'delivery.create', 0, {
    deliveryId,
    spec: {
      acceptanceCriteria: [{
        id: 'browser-criterion',
        required: true,
        title: 'Browser production route is accepted',
      }],
      baseRevision: serverConfiguration.repositoryBaseline,
      goal: 'Verify the standalone StrongFlow command path',
      publicationTarget: null,
      repositoryId: scope.repositoryId,
      title: 'Cross-origin browser Delivery',
    },
    tasks: [],
  })
  const advancedDelivery = await command(17, 'delivery.advance', 1, { deliveryId })
  const deliveriesAfterAdvance = await query(18, 'delivery.list', { states: [] })
  const deliveryDetail = await query(19, 'delivery.get', { deliveryId })
  let publicationPublishCode = null
  try {
    await command(25, 'publication.publish', 0, {
      publicationId,
      deliveryId,
      candidateDigest: `sha256:${'a'.repeat(64)}`,
      target: {
        provider: 'github',
        repository: 'winwincode/browser-fixture',
        baseBranch: 'main',
        headRepository: 'winwincode/browser-fixture',
        headBranch: 'winwincode/browser-fixture',
      },
    })
  } catch (error) {
    publicationPublishCode = error.code
  }
  const publications = await query(26, 'publication.list', {
    deliveryId: null,
    states: [],
  })

  activeSubscription = application.controlPlane.subscribe({
    subscriptionId: id('sub', 1),
    subscription: {
      scope,
      stream: { kind: 'product-session', productSessionId },
      eventTypes: [
        'product-session.changed.v1',
        'product-session.message.appended.v1',
      ],
    },
    onEvent(frame) {
      receivedEvents.push({
        eventId: frame.eventId,
        sequence: frame.sequence,
        type: frame.event.type,
      })
    },
    onError(error) {
      capturedConsole.push(`subscription error ${error.code}: ${error.message}`)
    },
  })
  await waitFor(
    () => transportFrameTypes.includes('transport.subscription-accepted.v1'),
    'subscription acceptance',
  )

  const created = await command(20, 'session.create', 0, {
    productSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    title: 'Cross-origin browser Chat',
    modelSelection: {
      providerId: 'fixture-provider',
      modelId: 'fixture-model',
      accountSource: { kind: 'system_default' },
    },
  })
  await waitFor(() => receivedEvents.length === 1, 'ProductSession create event')
  await waitFor(() => activeSubscription.cursor?.sequence === 1, 'ProductSession event ack')
  const firstCursor = activeSubscription.cursor
  activeSubscription.reconnect()
  const submitted = await command(21, 'chat.submit', 1, {
    productSessionId,
    message: 'Cross-origin browser message',
  })
  await waitFor(() => receivedEvents.length === 3, 'resumed Chat events')
  const messages = await query(22, 'session.messages.list', { productSessionId })

  let approvalDecisionCode = null
  try {
    await command(23, 'approval.decide', 0, {
      approvalId: id('apr', 1),
      decision: 'approve',
      reason: 'Browser verified decision route',
      binding: {
        productSessionId,
        executionJobId: id('job', 1),
        workerSessionId: id('wsn', 1),
        sessionIdentity: {
          productSessionId,
          workerSessionId: id('wsn', 1),
          codexThreadId: id('cdx', 1),
        },
      },
    })
  } catch (error) {
    approvalDecisionCode = error.code
  }

  const versionResponse = await fetch(`${serverConfiguration.serverUrl}/api/v1/queries`, {
    method: 'POST',
    credentials: 'include',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      schemaVersion: 'winwincode/v0',
      requestId: id('req', 24),
      actor,
      scope,
      query: 'delivery.list',
      parameters: { states: [] },
      page: page(),
    }),
  })
  const versionError = await versionResponse.json()

  return {
    approvalDecisionCode,
    chatCommand: submitted.command,
    chatMessage: messages.result.items[0]?.content ?? null,
    corsResponseType: versionResponse.type,
    deliveryAdvanceCommand: advancedDelivery.command,
    deliveryCreateCommand: createdDelivery.command,
    deliveryCount: delivery.result.items.length,
    deliveryDetailRevision: deliveryDetail.result.deliveryRevision,
    deliveryRevision: deliveriesAfterAdvance.result.items[0]?.revision ?? null,
    eventSequences: receivedEvents.map(event => event.sequence),
    eventTypes: receivedEvents.map(event => event.type),
    firstCursorSequence: firstCursor?.sequence ?? null,
    publicationCount: publications.result.items.length,
    publicationPublishCode,
    sessionCommand: created.command,
    settingsConcurrency: settingsAfter.result.workerConcurrencyLimit,
    settingsPreviousConcurrency: settingsBefore.result.workerConcurrencyLimit,
    settingsRevision: settingsUpdate.currentRevision,
    versionCode: versionError.error.code,
    versionReason: versionError.error.details.reason,
    versionSupportedSchema: versionError.error.details.supportedSchemaVersion,
    versionStatus: versionResponse.status,
    workerCount: workers.result.items.length,
    approvalCount: approvals.result.items.length,
  }
}

async function currentSettings(requestId) {
  const settings = await query(requestId, 'settings.get', {})
  return {
    concurrency: settings.result.workerConcurrencyLimit,
    revision: settings.result.revision,
  }
}

globalThis.runAuthBrowserFixture = async proof => {
  const failedInputValue = submitProof('incorrect-bootstrap-proof')
  await waitFor(
    () => application.authSession.state.status === 'authentication-required',
    'failed login',
  )
  const inputAfterFailedLogin = document.querySelector('.wwc-auth-session-proof').value
  const submittedInputValue = submitProof(proof)
  await waitFor(() => application.authSession.state.status === 'signed-in', 'successful login')
  const inputAfterSuccessfulLogin = document.querySelector('.wwc-auth-session-proof').value
  const flows = await runRealApplicationFlows()
  const context = authenticatedContext()
  const sessionState = JSON.stringify(application.authSession.state)
  const resources = performance.getEntriesByType('resource')
  const scans = {
    url: location.href,
    dom: document.documentElement.outerHTML,
    localStorage: storageText(localStorage),
    sessionStorage: storageText(sessionStorage),
    console: capturedConsole.join('\n'),
    network: resources.map(entry => entry.name).join('\n'),
    sessionState,
  }
  return {
    ...flows,
    contentSecurityPolicy: document.querySelector(
      'meta[http-equiv="Content-Security-Policy"]',
    )?.content ?? null,
    failedInputValue,
    inputAfterFailedLogin,
    submittedInputValue,
    inputAfterSuccessfulLogin,
    sessionActor: context.actor,
    sessionScope: context.scope,
    proofFound: Object.values(scans).some(value => value.includes(proof)),
    redirectedResources: resources.filter(entry => entry.redirectEnd > entry.redirectStart).length,
    cookieVisibleToScript: document.cookie,
  }
}

globalThis.runPostServerRestartFixture = async () => {
  const cancelled = await command(30, 'session.cancel', 2, {
    productSessionId,
    reason: 'browser restart cancellation',
  })
  await waitFor(() => receivedEvents.length === 4, 'post-restart ProductSession event')
  return {
    command: cancelled.command,
    eventSequences: receivedEvents.map(event => event.sequence),
    eventTypes: receivedEvents.map(event => event.type),
    settings: await currentSettings(31),
  }
}

globalThis.runWhileClientServerStoppedFixture = async () => currentSettings(32)

globalThis.runExistingSessionFixture = async () => {
  await waitFor(() => application.authSession.state.status === 'signed-in', 'session restore')
  const context = authenticatedContext()
  return {
    settings: await currentSettings(33),
    actor: context.actor,
    scope: context.scope,
  }
}

globalThis.finishAuthBrowserFixture = async () => {
  await application.controlPlane.logout()
  await waitFor(
    () => browserSockets.every(socket => socket.readyState === WebSocket.CLOSED),
    'WebSocket close',
  )
  let revokedKind = null
  try {
    await query(40, 'delivery.list', { states: [] })
  } catch (error) {
    revokedKind = error.kind
  }
  return {
    revokedKind,
    webSocketClosed: browserSockets.every(socket => socket.readyState === WebSocket.CLOSED),
  }
}
