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
const MAX_CAPTURED_FAILURES = 64

function recordFailure(target, failure) {
  if (target.length >= MAX_CAPTURED_FAILURES) target.shift()
  target.push(failure)
}
for (const operation of ['command', 'query']) {
  const invoke = application.controlPlane[operation].bind(application.controlPlane)
  application.controlPlane[operation] = async (request, options) => {
    try {
      return await invoke(request, options)
    } catch (error) {
      recordFailure(transportFailures, {
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
      recordFailure(eventHandlerFailures, {
        code: error?.code ?? 'UNKNOWN',
        kind: error?.kind ?? 'unknown',
        type: frame?.event?.type ?? 'unknown',
      })
      throw error
    }
  },
  onError(error) {
    recordFailure(subscriptionFailures, {
      code: error?.code ?? 'UNKNOWN',
      kind: error?.kind ?? 'unknown',
      reason: error?.details?.reason ?? null,
    })
    options.onError(error)
  },
})
const productSessionId = 'psn_01J00000000000000000000001'
const credentialReferenceId = 'crd_01J00000000000000000000001'
let requestSequence = 0
const WORKFLOW_TIMEOUT_MILLIS = 240_000
const forbiddenRequestPaths = serverConfiguration.forbiddenRequestPathPatterns
  .map(pattern => new RegExp(pattern, 'u'))

function id(prefix) {
  requestSequence += 1
  return `${prefix}_${'B'.repeat(20)}${String(requestSequence).padStart(6, '0')}`
}

function page(limit = 50) {
  return { cursor: null, limit }
}

async function waitFor(predicate, label, timeoutMillis = 20_000) {
  const deadline = Date.now() + timeoutMillis
  for (;;) {
    if (await predicate()) return
    if (Date.now() >= deadline) {
      throw new Error(`timed out waiting for ${label}: ${document.body.textContent.slice(0, 2_000)}`)
    }
    await new Promise(resolve => { setTimeout(resolve, 20) })
  }
}

async function initializeOwner(value) {
  await waitFor(
    () => document.querySelector('.wwc-login-initialization')?.hidden === false,
    'owner initialization form',
  )
  const username = document.querySelector('.wwc-login-initialization-username')
  const password = document.querySelector('.wwc-login-initialization-password')
  const proof = document.querySelector('.wwc-login-initialization-proof')
  username.value = 'owner'
  password.value = `${value}-owner-password`
  proof.value = value
  document.querySelector('.wwc-login-initialization-form').requestSubmit()
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
  return ['就绪', '已完成'].includes(visibleText('.wwc-chat-status'))
    && chatMessages().some(message => (
      message.role === 'assistant'
      && message.state === 'completed'
      && message.content.length > 0
    ))
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

function authSessionBytes() {
  const session = application.authSession.state.session
  if (application.authSession.state.status !== 'signed-in' || session === null) {
    throw new Error('the authenticated browser session is unavailable')
  }
  return JSON.stringify(session)
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

globalThis.runChatProductionSetup = async proof => {
  await waitFor(
    () => ['signed-out', 'authentication-required'].includes(
      application.authSession.state.status,
    ),
    'initial unauthenticated session restore',
  )
  await initializeOwner(proof)
  await waitFor(() => application.authSession.state.status === 'signed-in', 'signed-in session')
  const { scope } = context()
  await command('session.create', 0, {
    productSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    title: 'Browser production Chat',
    modelRoute: {
      providerId: 'winwincode-loopback',
      modelId: 'loopback-model',
      credentialReferenceId,
    },
  })
  await navigate(`#/chat?session=${productSessionId}`, '.wwc-chat')
  await waitFor(
    () => !['正在加载对话…', '正在更新对话…'].includes(visibleText('.wwc-chat-status')),
    'initial Chat snapshot',
  )
  await command('chat.submit', 1, {
    productSessionId,
    message: 'Run the deterministic local browser workflow.',
  })
  await waitFor(
    chatTurnIsTerminal,
    'completed Chat turn',
    WORKFLOW_TIMEOUT_MILLIS,
  )

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
    chatError: visibleText('.wwc-chat-error-text'),
    chatMessages: chatMessages(),
    chatStatus: visibleText('.wwc-chat-status'),
    subscriptionFailures: subscriptionFailures.slice(-5),
    transportFailures: transportFailures.slice(-5),
    ...browserEvidence(),
  }
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
