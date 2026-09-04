import { mountWinWinCodeClient } from '/module/application.js'
import { ControlPlaneClientError } from '/module/control-plane-client.js'

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
const modelRoute = {
  providerId: 'browser-provider',
  modelId: 'browser-model',
  credentialReferenceId: 'crd_00000000000000000000000001',
}
const secondModelRoute = {
  providerId: 'browser-provider-two',
  modelId: 'browser-model-two',
  credentialReferenceId: 'crd_00000000000000000000000002',
}
const browserSession = {
  schemaVersion,
  expiresAt: '2099-09-02T00:00:00.000Z',
  actor,
  authorizedScopes: [scope],
}
const calls = { commands: [], queries: [], subscriptions: [] }
let createdSession = null
let routeMode = 'available'

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

function runtime(productSessionId) {
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
    rebuiltAt: '2026-09-02T00:00:00.000Z',
    sessions: [],
  }
}

function availabilityItem(route, overrides = {}) {
  return {
    route,
    providerDisplayName: route === modelRoute ? 'Browser Provider' : 'Browser Provider Two',
    modelDisplayName: route === modelRoute ? 'Browser Model' : 'Browser Model Two',
    catalogSource: scope,
    catalogVersion: 1,
    providerVersion: 1,
    modelVersion: 1,
    contextWindowTokens: 128_000,
    maxOutputTokens: 16_000,
    toolSupport: 'parallel',
    reasoningEfforts: ['medium', 'high'],
    credentialRotationVersion: 1,
    isDefault: route === modelRoute,
    status: 'enabled',
    reason: 'ready',
    ...overrides,
  }
}

function routeAvailability() {
  const requestPoolSource = routeMode === 'cross-project'
    ? {
        ...projectScope,
        projectId: 'prj_00000000000000000000000099',
      }
    : projectScope
  if (routeMode === 'not-configured') return {
    kind: 'model_route_availability_page',
    scope,
    settingsSource: null,
    settingsRevision: null,
    requestPoolSource,
    requestPoolRevision: 5,
    defaultProviderId: null,
    defaultModelId: null,
    status: 'disabled',
    reason: 'no_provider',
    items: [],
  }
  const disabled = routeMode === 'revoked'
  return {
    kind: 'model_route_availability_page',
    scope,
    settingsSource: scope,
    settingsRevision: 1,
    requestPoolSource,
    requestPoolRevision: 5,
    defaultProviderId: modelRoute.providerId,
    defaultModelId: modelRoute.modelId,
    status: disabled ? 'disabled' : 'enabled',
    reason: disabled ? 'credential_missing_or_revoked' : 'ready',
    items: [
      availabilityItem(modelRoute, disabled ? {
        credentialRotationVersion: null,
        status: 'disabled',
        reason: 'credential_missing_or_revoked',
      } : {}),
      ...(disabled ? [] : [availabilityItem(secondModelRoute, { isDefault: false })]),
    ],
  }
}

const controlPlane = {
  serverUrl: 'https://control.localhost',
  async restore() { return structuredClone(browserSession) },
  async login() { return structuredClone(browserSession) },
  async logout() {},
  async query(request) {
    calls.queries.push(structuredClone(request))
    if (request.query === 'session.list') {
      return response(request, {
        kind: 'product_session_page',
        items: createdSession === null ? [] : [createdSession],
      })
    }
    if (request.query === 'model.route.availability.list') {
      if (routeMode === 'permission-denied') throw new ControlPlaneClientError({
        kind: 'authorization',
        code: 'PERMISSION_DENIED',
        message: 'private browser credential diagnostic',
        requestId: request.requestId,
        retryable: false,
      })
      return response(request, routeAvailability())
    }
    if (createdSession === null) throw new Error(`unexpected empty query: ${request.query}`)
    if (request.query === 'session.get') return response(request, createdSession)
    if (request.query === 'session.messages.list') {
      return response(request, { kind: 'chat_message_page', items: [] })
    }
    if (request.query === 'runtime.projection.get') {
      return response(request, runtime(createdSession.id))
    }
    if (request.query === 'session.interactions.list') {
      return response(request, { kind: 'chat_interaction_page', items: [] })
    }
    if (request.query === 'approval.list') {
      return response(request, { kind: 'approval_page', items: [] })
    }
    throw new Error(`unexpected query: ${request.query}`)
  },
  async command(request) {
    calls.commands.push(structuredClone(request))
    if (request.command !== 'session.create') throw new Error(`unexpected command: ${request.command}`)
    createdSession = {
      id: request.payload.productSessionId,
      projectId: request.payload.projectId,
      repositoryId: request.payload.repositoryId,
      revision: 1,
      state: 'idle',
      title: request.payload.title,
      updatedAt: '2026-09-02T00:00:00.000Z',
    }
    return {
      schemaVersion,
      requestId: request.requestId,
      command: request.command,
      outcome: 'completed',
      previousRevision: 0,
      currentRevision: 1,
      result: createdSession,
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

globalThis.runEmptyChatScenario = async () => {
  await waitFor(() => document.querySelector('.wwc-chat') !== null, 'empty Chat shell')
  await waitFor(
    () => document.querySelector('.wwc-chat-status')?.textContent === 'Ready for a new Chat'
      && document.querySelector('.wwc-chat-new-session')?.disabled === false,
    'empty Chat snapshot',
  )
  const empty = {
    hash: location.hash,
    model: document.querySelector('.wwc-chat-model').selectedOptions[0].textContent,
    newChatDisabled: document.querySelector('.wwc-chat-new-session').disabled,
    text: document.querySelector('.wwc-chat-empty').textContent,
  }

  routeMode = 'not-configured'
  location.hash = '#/chat?model=not-configured'
  await waitFor(
    () => document.querySelector('.wwc-chat-status')?.textContent === 'Model setup required',
    'unconfigured model route',
  )
  const notConfigured = {
    settingsHref: document.querySelector('.wwc-chat-model-settings').getAttribute('href'),
    text: document.querySelector('.wwc-chat-empty').textContent,
  }

  routeMode = 'permission-denied'
  location.hash = '#/chat?model=permission-denied'
  await waitFor(
    () => document.querySelector('.wwc-chat-status')?.textContent === 'Access denied',
    'model route permission failure',
  )
  const denied = {
    newChatDisabled: document.querySelector('.wwc-chat-new-session').disabled,
    text: document.querySelector('.wwc-chat-error-text').textContent,
  }

  routeMode = 'revoked'
  location.hash = '#/chat?model=revoked'
  await waitFor(
    () => document.querySelector('.wwc-chat-status')?.textContent === 'Model route unavailable',
    'revoked model route',
  )
  const revoked = {
    model: document.querySelector('.wwc-chat-model').options[0].textContent,
    modelDisabled: document.querySelector('.wwc-chat-model').options[0].disabled,
    newChatDisabled: document.querySelector('.wwc-chat-new-session').disabled,
  }

  routeMode = 'cross-project'
  location.hash = '#/chat?model=cross-project'
  await waitFor(
    () => document.querySelector('.wwc-chat-status')?.textContent === 'Chat unavailable',
    'cross-Project request-pool source rejection',
  )
  const crossProject = {
    newChatDisabled: document.querySelector('.wwc-chat-new-session').disabled,
    text: document.querySelector('.wwc-chat-error-text').textContent,
  }

  routeMode = 'available'
  location.hash = '#/chat?model=available'
  await waitFor(
    () => document.querySelector('.wwc-chat-status')?.textContent === 'Ready for a new Chat'
      && document.querySelector('.wwc-chat-new-session')?.disabled === false,
    'restored model route',
  )
  const modelSelect = document.querySelector('.wwc-chat-model')
  modelSelect.selectedIndex = 1
  modelSelect.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor(
    () => document.querySelector('.wwc-chat-model').selectedOptions[0].textContent
      .includes('Browser Model Two'),
    'second model route selection',
  )
  document.querySelector('.wwc-chat-new-session').click()
  await waitFor(() => createdSession !== null, 'first session command')
  await waitFor(
    () => document.querySelector('.wwc-chat-heading')?.textContent === 'New Chat',
    'created Chat snapshot',
  )
  await waitFor(() => new URLSearchParams(location.hash.split('?')[1]).has('session'), 'Chat URL')
  const composer = document.querySelector('.wwc-chat-composer-input')
  composer.value = 'First message'
  composer.dispatchEvent(new Event('input', { bubbles: true }))
  return {
    empty,
    denied,
    notConfigured,
    revoked,
    crossProject,
    created: {
      hash: location.hash,
      heading: document.querySelector('.wwc-chat-heading').textContent,
      sendDisabled: document.querySelector('.wwc-chat-send').disabled,
      status: document.querySelector('.wwc-chat-status').textContent,
    },
    calls,
  }
}
