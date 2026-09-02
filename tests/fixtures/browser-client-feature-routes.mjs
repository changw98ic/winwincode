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
const productSessionId = 'psn_00000000000000000000000001'
const browserSession = {
  schemaVersion,
  expiresAt: '2099-09-02T00:00:00.000Z',
  actor,
  authorizedScopes: [scope],
}
const calls = {
  abortedQueries: [],
  commands: [],
  copiedDiagnostics: [],
  queries: [],
  subscriptions: [],
}
let failureMode = null
let blockedQuery = null
const subscriptionCallbacks = []

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

function session() {
  return {
    id: productSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision: 1,
    state: 'idle',
    title: 'Route fixture Chat',
    updatedAt: '2026-09-02T00:00:00.000Z',
  }
}

function failure(request) {
  if (failureMode === null) return null
  const kinds = {
    authentication: 'authentication',
    authorization: 'authorization',
    network: 'network',
    version: 'version',
  }
  const kind = kinds[failureMode] ?? 'network'
  return new ControlPlaneClientError({
    kind,
    code: kind === 'authentication'
      ? 'AUTHENTICATION_REQUIRED'
      : kind === 'authorization'
        ? 'PERMISSION_DENIED'
        : kind === 'version'
          ? 'SCHEMA_VERSION_UNSUPPORTED'
          : 'NETWORK_ERROR',
    message: 'private route fixture diagnostic SECRET_TOKEN /private/repository',
    requestId: request.requestId,
    retryable: kind === 'network',
  })
}

const controlPlane = {
  serverUrl: 'https://control.localhost',
  async restore() { return structuredClone(browserSession) },
  async login() { return structuredClone(browserSession) },
  async logout() {},
  async query(request, options = {}) {
    calls.queries.push(structuredClone(request))
    const currentFailure = failure(request)
    if (currentFailure !== null) throw currentFailure
    if (request.query === blockedQuery) {
      blockedQuery = null
      return new Promise((resolve, reject) => {
        const abort = () => {
          calls.abortedQueries.push(request.query)
          reject(new ControlPlaneClientError({
            kind: 'cancelled',
            code: 'REQUEST_CANCELLED',
            message: 'route changed',
            requestId: request.requestId,
            retryable: false,
          }))
        }
        if (options.signal?.aborted === true) abort()
        else options.signal?.addEventListener('abort', abort, { once: true })
      })
    }
    if (request.query === 'settings.get') {
      return response(request, {
        revision: 1,
        defaultModelRoute: null,
        workerConcurrencyLimit: 2,
      })
    }
    if (request.query === 'credential.reference.list') {
      return response(request, { kind: 'credential_reference_page', items: [] })
    }
    if (request.query === 'worker.list') {
      return response(request, { kind: 'worker_page', items: [] })
    }
    if (request.query === 'delivery.list') {
      return response(request, { kind: 'delivery_page', items: [] })
    }
    if (request.query === 'session.get') return response(request, session())
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
    throw new Error(`unexpected command: ${request.command}`)
  },
  subscribe(options) {
    subscriptionCallbacks.push(options)
    const record = {
      closed: false,
      reconnects: 0,
      subscriptionId: options.subscriptionId,
      subscription: structuredClone(options.subscription),
    }
    calls.subscriptions.push(record)
    return {
      cursor: null,
      resume() {},
      reconnect() { record.reconnects += 1 },
      close() { record.closed = true },
    }
  },
  close() {},
}

const root = document.querySelector('[data-winwincode-client-root]')
const application = mountWinWinCodeClient({
  root,
  serverUrl: controlPlane.serverUrl,
  controlPlane,
  copyText(value) { calls.copiedDiagnostics.push(value) },
})

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise(resolve => { setTimeout(resolve, 20) })
  }
  throw new Error(`timed out waiting for ${label}: ${document.body.textContent}`)
}

async function settled(selector, statusSelector) {
  await waitFor(() => document.querySelector(selector) !== null, selector)
  await waitFor(() => {
    const statusRoot = document.querySelector(statusSelector)
    const status = statusRoot?.querySelector('.wwc-status-badge-label')?.textContent
      ?? statusRoot?.textContent
      ?? ''
    return !status.startsWith('Loading') && !status.startsWith('Updating')
  }, `${selector} snapshot`)
  const statusRoot = document.querySelector(statusSelector)
  return {
    hash: location.hash,
    status: statusRoot?.querySelector('.wwc-status-badge-label')?.textContent
      ?? statusRoot?.textContent
      ?? '',
    text: document.querySelector(selector)?.textContent ?? '',
  }
}

globalThis.inspectFeatureRoute = async name => {
  if (name === 'settings') return settled('.wwc-settings', '.wwc-settings-status')
  if (name === 'operations') {
    return settled('.wwc-local-operations', '.wwc-local-operations-status')
  }
  if (name === 'approvals') {
    return settled('.wwc-local-decisions', '.wwc-local-decisions-status')
  }
  throw new Error(`unknown feature route: ${name}`)
}

globalThis.inspectManagementPresentation = name => {
  const selector = name === 'settings'
    ? '.wwc-settings'
    : name === 'operations'
      ? '.wwc-local-operations'
      : name === 'approvals'
        ? '.wwc-local-decisions'
        : null
  if (selector === null) throw new Error(`unknown management page: ${name}`)
  const pageRoot = document.querySelector(selector)
  const status = pageRoot?.querySelector('[data-wwc-component="status-badge"]')
  const panel = pageRoot?.querySelector('[data-wwc-component="panel"]')
  const pageRect = pageRoot?.getBoundingClientRect()
  const panelRect = panel?.getBoundingClientRect()
  return {
    page: pageRoot?.dataset.wwcPage ?? null,
    panelCount: pageRoot?.querySelectorAll('[data-wwc-component="panel"]').length ?? 0,
    emptyCount: pageRoot?.querySelectorAll('[data-wwc-component="empty-state"]:not([hidden])').length ?? 0,
    statusIcon: status?.querySelector('.wwc-status-badge-icon')?.textContent ?? '',
    statusIconHidden: status?.querySelector('.wwc-status-badge-icon')?.getAttribute('aria-hidden'),
    statusRole: status?.getAttribute('role'),
    noHorizontalOverflow: document.documentElement.scrollWidth <= document.documentElement.clientWidth,
    panelWithinPage: pageRect !== undefined
      && panelRect !== undefined
      && panelRect.left >= pageRect.left
      && panelRect.right <= pageRect.right,
  }
}

globalThis.inspectManagementFocus = selector => {
  const control = document.querySelector(selector)
  control.focus()
  const style = getComputedStyle(control)
  return {
    active: document.activeElement === control,
    outlineStyle: style.outlineStyle,
    outlineWidth: style.outlineWidth,
  }
}

function connectionSnapshot() {
  const bar = document.querySelector('.wwc-connection-bar')
  return {
    status: bar?.dataset.connectionStatus ?? null,
    label: bar?.querySelector('.wwc-connection-status .wwc-status-badge-label')?.textContent ?? '',
    route: location.hash,
  }
}

globalThis.runReliabilityScenario = async () => {
  await globalThis.inspectFeatureRoute('settings')
  const provider = document.querySelector('.wwc-settings-provider')
  provider.value = 'draft-provider'
  const connected = connectionSnapshot()

  dispatchEvent(new Event('offline'))
  const offline = connectionSnapshot()
  const offlineDraft = provider.value
  dispatchEvent(new Event('online'))
  const reconnecting = connectionSnapshot()
  const reconnectCount = calls.subscriptions.reduce((sum, item) => sum + item.reconnects, 0)
  await application.controlPlane.restore()
  const reconnected = connectionSnapshot()

  application.authSession.authenticationRequired(new ControlPlaneClientError({
    kind: 'authentication',
    code: 'AUTHENTICATION_REQUIRED',
    message: 'SECRET_TOKEN /private/repository expired session payload',
    requestId: 'req_00000000000000000000000001',
    retryable: false,
  }))
  await waitFor(
    () => document.querySelector('.wwc-connection-bar')?.dataset.connectionStatus
      === 'authentication-required',
    'expired authentication status',
  )
  const authenticationDraft = provider.value
  document.querySelector('.wwc-connection-copy').click()
  await Promise.resolve()
  const authenticationDiagnostic = calls.copiedDiagnostics.at(-1) ?? ''
  await application.authSession.restore()
  await globalThis.inspectFeatureRoute('settings')

  await subscriptionCallbacks.at(-1).onAuthorizationRevoked({
    secret: 'SECRET_TOKEN',
    path: '/private/repository',
  })
  await waitFor(
    () => document.querySelector('.wwc-connection-bar')?.dataset.connectionStatus
      === 'permission-denied',
    'revoked WebSocket permission status',
  )
  const revokedStatus = connectionSnapshot()
  document.querySelector('.wwc-connection-copy').click()
  await Promise.resolve()
  const revokedDiagnostic = calls.copiedDiagnostics.at(-1) ?? ''
  application.connection.reset()
  await application.controlPlane.restore()

  async function failureState(mode, expectedStatus) {
    failureMode = mode
    location.hash = `#/settings/runtime?fixture=${mode}`
    await waitFor(
      () => document.querySelector('.wwc-connection-bar')?.dataset.connectionStatus
        === expectedStatus,
      `${mode} global status`,
    )
    return connectionSnapshot()
  }

  const network = await failureState('network', 'reconnecting')
  const permission = await failureState('authorization', 'permission-denied')
  const version = await failureState('version', 'version-mismatch')
  const authentication = await failureState('authentication', 'authentication-required')

  failureMode = null
  const rawFailure = new Error('SECRET_TOKEN /private/repository raw render payload')
  dispatchEvent(new ErrorEvent('error', { error: rawFailure, message: rawFailure.message }))
  await waitFor(
    () => document.querySelector('.wwc-client-error-boundary:not([hidden])') !== null,
    'global Client Error Boundary',
  )
  const boundary = document.querySelector('.wwc-client-error-boundary')
  const boundaryText = boundary.textContent
  boundary.querySelector('.wwc-client-error-copy').click()
  await Promise.resolve()
  const copied = calls.copiedDiagnostics.at(-1) ?? ''
  const copyFeedback = boundary.querySelector('.wwc-client-error-copy-feedback').textContent
  const copyButton = boundary.querySelector('.wwc-client-error-copy')
  copyButton.focus()
  const focusStyle = getComputedStyle(copyButton)
  const focus = {
    active: document.activeElement === copyButton,
    outlineStyle: focusStyle.outlineStyle,
    outlineWidth: focusStyle.outlineWidth,
  }
  boundary.querySelector('.wwc-client-error-safe-entry').click()
  await waitFor(() => location.hash === '#/chat', 'safe Chat entry')

  return {
    authentication,
    authenticationDiagnostic,
    authenticationDraft,
    boundaryText,
    connected,
    copied,
    copyFeedback,
    focus,
    network,
    offline,
    offlineDraft,
    permission,
    reconnected,
    reconnectCount,
    reconnecting,
    revokedDiagnostic,
    revokedStatus,
    safeHash: location.hash,
    version,
  }
}

globalThis.runFeatureNavigationScenario = async () => {
  const settings = await globalThis.inspectFeatureRoute('settings')
  document.querySelector('.wwc-settings-local-operations-link').click()
  const operations = await globalThis.inspectFeatureRoute('operations')
  const settingsSubscriptionClosed = calls.subscriptions[0]?.closed ?? false

  document.querySelector('[data-surface="approvals"]').click()
  await waitFor(
    () => document.querySelector('.wwc-feature-route-unavailable') !== null,
    'unconfigured Approvals route',
  )
  const unconfigured = document.querySelector('.wwc-feature-route-unavailable').textContent

  failureMode = 'authorization'
  location.hash = '#/settings?fixture=denied'
  await waitFor(
    () => document.querySelector('.wwc-settings-status .wwc-status-badge-label')?.textContent
      === 'Access denied',
    'Settings permission failure',
  )
  const denied = document.querySelector('.wwc-settings-error-text').textContent

  failureMode = 'network'
  location.hash = '#/settings/runtime?fixture=network'
  await waitFor(
    () => document.querySelector(
      '.wwc-local-operations-status .wwc-status-badge-label',
    )?.textContent
      === 'Reconnecting…',
    'Local Operations network failure',
  )
  const network = document.querySelector('.wwc-local-operations-error-text').textContent

  failureMode = null
  blockedQuery = 'settings.get'
  location.hash = '#/settings?fixture=cancel'
  await waitFor(
    () => calls.queries.at(-1)?.query === 'settings.get',
    'pending Settings query',
  )
  location.hash = '#/settings/runtime?fixture=after-cancel'
  const afterCancellation = await globalThis.inspectFeatureRoute('operations')
  await waitFor(() => calls.abortedQueries.includes('settings.get'), 'cancelled Settings query')

  return {
    afterCancellation,
    denied,
    network,
    operations,
    settings,
    settingsSubscriptionClosed,
    unconfigured,
    calls,
  }
}
