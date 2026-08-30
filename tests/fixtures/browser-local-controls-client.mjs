import { mountWinWinCodeClient } from '/module/application.js'

const serverConfiguration = await fetch('/fixture/server-url.json', {
  cache: 'no-store',
}).then(async response => {
  if (!response.ok) throw new Error('fixture server URL is unavailable')
  return response.json()
})
const root = document.querySelector('[data-winwincode-client-root]')
const browserSockets = []
const transportFrames = []
const BrowserWebSocket = globalThis.WebSocket
globalThis.WebSocket = class TrackingWebSocket extends BrowserWebSocket {
  constructor(...arguments_) {
    super(...arguments_)
    browserSockets.push(this)
    this.addEventListener('message', event => {
      try {
        transportFrames.push(JSON.parse(event.data))
      } catch {}
    })
  }
}
const application = mountWinWinCodeClient({
  root,
  serverUrl: serverConfiguration.serverUrl,
})
const capturedConsole = []
for (const method of ['debug', 'error', 'info', 'log', 'warn']) {
  const original = console[method].bind(console)
  console[method] = (...values) => {
    capturedConsole.push(values.map(value => String(value)).join(' '))
    original(...values)
  }
}

const productSessionId = id('psn', 1)
const credentialReferenceId = id('crd', 2)
const receivedEvents = []
const authorizationRevocations = []
let subscription = null
let authorizedContext = null
let requestSequence = 1_000

function id(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function nextRequestId() {
  requestSequence += 1
  return id('req', requestSequence)
}

function page(limit = 100) {
  return { cursor: null, limit }
}

function waitFor(predicate, label) {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + 20_000
    const poll = () => {
      if (predicate()) {
        resolve()
        return
      }
      if (Date.now() >= deadline) {
        reject(new Error(
          `timed out waiting for ${label}; auth=${JSON.stringify(application.authSession.state)}; frames=${JSON.stringify(transportFrames)}; events=${JSON.stringify(receivedEvents)}; console=${capturedConsole.join(' | ')}`,
        ))
        return
      }
      setTimeout(poll, 20)
    }
    poll()
  })
}

function context() {
  const session = application.authSession.state.session
  if (application.authSession.state.status !== 'signed-in' || session === null) {
    throw new Error('browser session context is unavailable')
  }
  const scope = session.authorizedScopes.find(candidate => candidate.kind === 'repository')
  if (scope === undefined) throw new Error('repository Scope is unavailable')
  return { actor: session.actor, scope }
}

function command(operation, expectedRevision, payload) {
  const current = context()
  return application.controlPlane.command({
    schemaVersion: 'winwincode/v1',
    requestId: nextRequestId(),
    actor: current.actor,
    scope: current.scope,
    command: operation,
    expectedRevision,
    payload,
  })
}

function queryWithContext(current, operation, parameters, limit = 100) {
  return application.controlPlane.query({
    schemaVersion: 'winwincode/v1',
    requestId: nextRequestId(),
    actor: current.actor,
    scope: current.scope,
    query: operation,
    parameters,
    page: page(limit),
  })
}

function query(operation, parameters, limit = 100) {
  return queryWithContext(context(), operation, parameters, limit)
}

function submitProof(value) {
  const input = document.querySelector('.wwc-auth-session-proof')
  input.value = value
  document.querySelector('.wwc-auth-session-form').requestSubmit()
  return input.value
}

function subscribe() {
  const current = context()
  subscription = application.controlPlane.subscribe({
    subscriptionId: id('sub', 52),
    startAt: 'earliest-available',
    subscription: {
      scope: current.scope,
      stream: { kind: 'product-session', productSessionId },
      eventTypes: [
        'approval.changed.v1',
        'chat-interactions.invalidated.v1',
        'product-session.changed.v1',
      ],
    },
    onEvent(frame) {
      receivedEvents.push({
        eventId: frame.eventId,
        sequence: frame.sequence,
        type: frame.event.type,
      })
    },
    onAuthorizationRevoked(frame) {
      const value = frame ?? transportFrames.findLast(candidate => (
        candidate.type === 'transport.authorization-revoked.v1'
      ))
      if (value === undefined) return
      authorizationRevocations.push({
        authorizationEpoch: value.authorizationEpoch,
        subscriptionId: value.subscriptionId,
        type: value.type,
      })
    },
    onError(error) {
      capturedConsole.push(`subscription error ${error.code}: ${error.message}`)
    },
  })
}

function acceptedFrameCount() {
  return transportFrames.filter(frame => (
    frame.type === 'transport.subscription-accepted.v1'
    || frame.type === 'transport.resume-accepted.v1'
  )).length
}

function uniqueEvents() {
  return new Set(receivedEvents.map(event => event.eventId)).size
}

function storageText(storage) {
  return Array.from({ length: storage.length }, (_, index) => {
    const key = storage.key(index)
    return `${key ?? ''}:${key === null ? '' : storage.getItem(key) ?? ''}`
  }).join('\n')
}

function browserSurfaceContains(...secrets) {
  const surface = [
    document.documentElement.outerHTML,
    storageText(localStorage),
    storageText(sessionStorage),
    capturedConsole.join('\n'),
    performance.getEntriesByType('resource').map(entry => entry.name).join('\n'),
    JSON.stringify(application.authSession.state),
    JSON.stringify(receivedEvents),
    JSON.stringify(transportFrames),
  ].join('\n')
  return secrets.some(secret => surface.includes(secret))
}

globalThis.runLocalControlsFixture = async (proof, firstLocator, rotatedLocator) => {
  const submittedProof = submitProof(proof)
  await waitFor(() => application.authSession.state.status === 'signed-in', 'browser sign-in')
  authorizedContext = context()
  const clearedProof = document.querySelector('.wwc-auth-session-proof').value

  subscribe()
  await waitFor(() => acceptedFrameCount() === 1, 'initial subscription acceptance')
  await waitFor(() => receivedEvents.length >= 4, 'seeded durable event replay')
  await waitFor(() => subscription.cursor?.sequence === receivedEvents.at(-1)?.sequence, 'initial acknowledgement')
  const beforeReconnect = receivedEvents.length
  const firstCursor = subscription.cursor
  subscription.reconnect()
  await waitFor(() => acceptedFrameCount() === 2, 'explicit cursor resume')
  await new Promise(resolve => setTimeout(resolve, 250))
  const afterReconnect = receivedEvents.length

  const settingsBefore = await query('settings.get', {}, 1)
  const created = await command('credential.reference.create', 0, {
    credentialReferenceId,
    displayName: 'Browser credential reference',
    providerId: 'browser-provider',
    vaultLocator: firstLocator,
  })
  let settings
  try {
    settings = await command('settings.update', settingsBefore.result.revision, {
      patch: {
        defaultModelRoute: {
          providerId: 'browser-provider',
          modelId: 'browser-model',
          credentialReferenceId: id('crd', 1),
        },
        workerConcurrencyLimit: 3,
      },
    })
  } catch (error) {
    throw new Error(`settings update failed: ${JSON.stringify({
      code: error.code,
      details: error.details,
      kind: error.kind,
      message: error.message,
    })}`)
  }
  const rotated = await command('credential.reference.rotate', created.result.revision, {
    credentialReferenceId,
    vaultLocator: rotatedLocator,
  })
  const revoked = await command('credential.reference.revoke', rotated.result.revision, {
    credentialReferenceId,
  })

  const approvals = await query('approval.list', { states: ['pending'] })
  const approval = approvals.result.items.find(item => item.id === id('apr', 1))
  if (approval === undefined) throw new Error('seeded Approval is unavailable')
  const beforeDecision = receivedEvents.length
  const decided = await command('approval.decide', approval.revision, {
    approvalId: approval.id,
    binding: approval.binding,
    decision: 'approve',
    reason: 'Real browser approved the durable request',
  })
  await waitFor(() => receivedEvents.length > beforeDecision, 'approval public events')
  await waitFor(() => subscription.cursor?.sequence === receivedEvents.at(-1)?.sequence, 'approval acknowledgement')

  return {
    afterReconnect,
    approval: { id: decided.result.id, state: decided.result.state },
    beforeReconnect,
    browserSecretFound: browserSurfaceContains(proof, firstLocator, rotatedLocator),
    credentialRevisions: [created.result.revision, rotated.result.revision, revoked.result.revision],
    credentialState: revoked.result.secretState,
    cursorSequence: subscription.cursor?.sequence ?? null,
    eventCount: receivedEvents.length,
    eventSequences: receivedEvents.map(event => event.sequence),
    eventTypes: receivedEvents.map(event => event.type),
    firstCursorSequence: firstCursor?.sequence ?? null,
    settingsConcurrency: settings.result.workerConcurrencyLimit,
    settingsRevision: settings.result.revision,
    submittedProof,
    clearedProof,
    uniqueEventCount: uniqueEvents(),
  }
}

globalThis.runLocalControlsAfterRestart = async () => {
  const priorAccepted = acceptedFrameCount()
  subscription.reconnect()
  await waitFor(() => acceptedFrameCount() > priorAccepted, 'post-restart cursor resume')
  const beforeCancel = receivedEvents.length
  const cancelled = await command('session.cancel', 2, {
    productSessionId,
    reason: 'real browser restart cancellation',
  })
  await waitFor(() => receivedEvents.length > beforeCancel, 'post-restart cancellation event')
  await waitFor(() => subscription.cursor?.sequence === receivedEvents.at(-1)?.sequence, 'cancellation acknowledgement')
  const settings = await query('settings.get', {}, 1)
  const credential = await query('credential.reference.get', { credentialReferenceId }, 1)
  const approval = await query('approval.get', { approvalId: id('apr', 1) }, 1)
  return {
    approvalState: approval.result.state,
    cancellationState: cancelled.result.state,
    credentialRevision: credential.result.revision,
    credentialState: credential.result.secretState,
    eventCount: receivedEvents.length,
    eventSequences: receivedEvents.map(event => event.sequence),
    eventTypes: receivedEvents.map(event => event.type),
    settingsConcurrency: settings.result.workerConcurrencyLimit,
    settingsRevision: settings.result.revision,
    uniqueEventCount: uniqueEvents(),
  }
}

globalThis.runLocalControlsAfterPermissionRevocation = async () => {
  await waitFor(
    () => authorizationRevocations.length === 1 || transportFrames.some(frame => (
      frame.type === 'transport.authorization-revoked.v1'
    )),
    'generated authorization revocation',
  )
  const revocation = authorizationRevocations[0] ?? (() => {
    const frame = transportFrames.findLast(candidate => (
      candidate.type === 'transport.authorization-revoked.v1'
    ))
    return {
      authorizationEpoch: frame.authorizationEpoch,
      subscriptionId: frame.subscriptionId,
      type: frame.type,
    }
  })()
  await waitFor(
    () => browserSockets.every(socket => socket.readyState === WebSocket.CLOSED),
    'revoked WebSocket close',
  )
  let queryError = null
  try {
    await queryWithContext(authorizedContext, 'settings.get', {}, 1)
  } catch (error) {
    queryError = { code: error.code, kind: error.kind }
  }
  return {
    queryError,
    revocation,
    webSocketClosed: browserSockets.every(socket => socket.readyState === WebSocket.CLOSED),
  }
}
