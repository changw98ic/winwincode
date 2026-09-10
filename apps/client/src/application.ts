// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  createControlPlaneClient,
  createControlPlaneClientDirectory,
  createControlPlaneClientUsers,
  createControlPlaneRunIdentityFake,
  createControlPlaneTaskFake,
  type ControlPlaneClient,
  type ControlPlaneClientTransport,
  type ControlPlaneTaskAnchor,
} from './community-control-plane-client.js'
import { createClientOccupancyFacade } from './client-occupancy-facade.js'
import { mountClientErrorBoundary } from './components/client-error-boundary.js'
import { mountConnectionBar } from './components/connection-bar.js'
import {
  CLIENT_SURFACES,
  clientSurfaceFromHash,
  type ClientSurface,
  type ClientSurfaceId,
} from './client-surface.js'
import {
  classifyClientFailure,
  createConnectionMonitor,
  createSafeDiagnostic,
  observeControlPlaneClient,
  type ClientFailure,
  type ConnectionMonitor,
  type ConnectionSnapshot,
} from './core/connection-state.js'
import {
  loadRecentChats,
  type RecentChatEntry,
} from './recent-chats.js'
import { createQueryCache } from '@winwincode/browser-core/query-cache'
import {
  resolveScopeContext,
  scopeHash,
  scopeSelectionFromHash,
  surfaceHash,
  type ScopeContextResolution,
  type ScopeRouteSelection,
} from '@winwincode/browser-core/scope-context'
import { mountAuthSessionPage, type AuthSessionPage } from './auth-page.js'
import {
  createAuthSessionViewModel,
  type AuthSessionViewModel,
} from './auth-view-model.js'
import {
  createLoginViewModel,
  type LoginViewModel,
} from './login-view-model.js'
import { mountLoginPage, type LoginPage } from './login-page.js'
import {
  createClientsViewModel,
  type ClientsViewModel,
} from './clients-view-model.js'
import { mountClientsPage, type ClientsPage } from './clients-page.js'
import {
  createUserManagementViewModel,
  type UserManagementViewModel,
} from './user-management-view-model.js'
import { mountUsersPage, type UsersPage } from './users-page.js'
import {
  clientOccupancyPortFromFacade,
  createClientOccupancyViewModel,
  type ClientOccupancyViewModel,
} from './client-occupancy-view-model.js'
import {
  createRepositoriesViewModel,
  type RepositoriesViewModel,
} from './repositories-view-model.js'
import { mountRepositoriesPage, type RepositoriesPage } from './repositories-page.js'
import {
  createReadinessViewModel,
  type ReadinessContext,
  type ReadinessItemState,
} from './readiness-view-model.js'
import {
  mountReadinessPage,
  type ReadinessFixTarget,
} from './readiness-page.js'
import type { EnterpriseApplication } from './enterprise-application.js'
import {
  mountScopeSelectorPage,
  type ScopeSelectorPage,
} from './scope-selector-page.js'
import { createScopeSelectorViewModel } from './scope-selector-view-model.js'
import type {
  CandidateComparisonRouteSelection,
  CandidateDiffViewMode,
} from './strongflow-diff-model.js'
import {
  strongFlowHistorySelectionFromHash,
} from './strongflow-history-selection.js'
import {
  parseStrongFlowRouteHash,
  strongFlowCandidateViewFromHash,
  strongFlowRawCandidateFileFromHash,
  strongFlowRouteHash,
  type StrongFlowEvidenceRouteState,
  type StrongFlowRoute,
} from './strongflow-route.js'
import type {
  ControlPlaneWebSocketSubscriptionId,
  DeliveryGetResultResponse,
  DeliveryId,
  ProductSessionId,
  RepositoryScope,
  RequestId,
  Scope,
  StageRunId,
} from './generated/contracts.js'
import { matchesCanonicalSchema } from './generated/control-plane-client.js'
import { QueryName } from './generated/contracts.js'
import {
  projectionForSession,
  surfaceCapabilityForHash,
  type NavigationCapabilityFacts,
  type NavigationCapabilityProjection,
  type SurfaceCapability,
} from './navigation-capability.js'
import type {
  AttentionNotificationControl,
  AttentionNotificationMonitor,
} from './attention-notifications.js'
import {
  browserHomeVisitStorage,
  createHomeRecentVisitStore,
  homeDeliveryVisitFromHash,
  type HomeRecentVisitStore,
} from './home-recent-visits.js'

export interface WinWinCodeClientApplicationOptions {
  readonly serverUrl: string
  readonly root: HTMLElement
  readonly window?: Window
  /** Canonical facade injection used by deterministic browser fixtures and host composition. */
  readonly controlPlane?: ControlPlaneClient
  /** Secret-safe clipboard seam for browser hosts and deterministic fixtures. */
  readonly copyText?: (value: string) => Promise<void> | void
  readonly now?: () => string
  /** Server/deployment facts projected into navigation presentation only. */
  readonly navigationCapabilities?: Readonly<NavigationCapabilityFacts>
}

export interface WinWinCodeClientApplication {
  readonly controlPlane: ControlPlaneClient
  readonly authSession: AuthSessionViewModel
  readonly connection: ConnectionMonitor
  readonly surfaces: readonly ClientSurface[]
  readonly activeSurface: ClientSurface
  navigate(surface: ClientSurfaceId): void
  close(): void
}

interface MountedClientFeature {
  close(): void
}

type ScopeSelectorRenderMode = 'replace' | 'preserve'

function element<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

const CONTRACT_ID_ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'

function contractId(
  prefix: 'req' | 'sub' | 'psn' | 'dlv' | 'tsk',
  crypto: Crypto,
): string {
  const entropy = crypto.getRandomValues(new Uint8Array(26))
  const value = [...entropy]
    .map(byte => CONTRACT_ID_ALPHABET[byte & 31])
    .join('')
  return `${prefix}_${value}`
}

function routeParameters(hash: string): URLSearchParams {
  const query = hash.indexOf('?')
  return new URLSearchParams(query < 0 ? '' : hash.slice(query + 1))
}

/** A route stage-run identity that survives the canonical StageRun schema. */
function canonicalStageRunParameter(parameters: URLSearchParams): StageRunId | null {
  const values = parameters.getAll('stageRun')
  const value = values.length === 1 ? values[0] : null
  return value !== null && matchesCanonicalSchema('StageRunId', value)
    ? value as StageRunId
    : null
}

export {
  parseStrongFlowRouteHash,
  strongFlowCandidateViewFromHash,
  strongFlowRawCandidateFileFromHash,
  strongFlowRouteHash,
}
export type { StrongFlowEvidenceRouteState, StrongFlowRoute }

function browserControlPlaneTransport(browser: Window): ControlPlaneClientTransport {
  const nativeFetch = browser.fetch
  if (typeof nativeFetch !== 'function') return Object.freeze({})
  const fetch: NonNullable<ControlPlaneClientTransport['fetch']> = (input, init) => (
    nativeFetch.call(browser, input, init)
  )
  return Object.freeze({ fetch })
}

/** Mount the one browser shell. Feature modules attach to its named surface slot. */
export function mountWinWinCodeClient(
  options: WinWinCodeClientApplicationOptions,
): WinWinCodeClientApplication {
  const browser = options.window ?? window
  const document = options.root.ownerDocument
  const now = options.now ?? (() => new Date().toISOString())
  const connection = createConnectionMonitor({ now })
  const browserIsOnline = () => {
    const navigatorValue = Reflect.get(browser, 'navigator') as Navigator | undefined
    return navigatorValue?.onLine !== false
  }
  let accessFailureSession: AuthSessionViewModel | null = null
  let onRouteAuthorizationRevoked: (() => void) | null = null
  const browserTransport = browserControlPlaneTransport(browser)
  const rawControlPlane = options.controlPlane ?? createControlPlaneClient({
      serverUrl: options.serverUrl,
      transport: browserTransport,
      onAccessFailure(error) {
        accessFailureSession?.authenticationRequired(error)
      },
    })
  const observedControlPlane = observeControlPlaneClient({
    client: rawControlPlane,
    monitor: connection,
    online: browserIsOnline,
    onAuthorizationRevoked() { onRouteAuthorizationRevoked?.() },
  })
  const queryCache = createQueryCache({ client: observedControlPlane.client })
  const controlPlane = queryCache.client
  const authSession = createAuthSessionViewModel(controlPlane)
  accessFailureSession = authSession
  // AUTH-100.2: the username + password login page talks to the one Control
  // Plane facade directly. Expected sign-in failures stay in the form instead
  // of polluting feature connection health, mirroring the Scope selector seam.
  const loginModel: LoginViewModel = createLoginViewModel({ client: rawControlPlane })
  // CLIENT-200.4: the Clients area talks to the one Control Plane facade
  // through its directory extension. Expected add-Client failures stay in the
  // form instead of polluting feature connection health, mirroring the login
  // page seam above.
  const clientDirectory = createControlPlaneClientDirectory({
    client: rawControlPlane,
    transport: browserTransport,
  })
  const clientUsers = createControlPlaneClientUsers({
    client: rawControlPlane,
    transport: browserTransport,
  })
  const clientsModel: ClientsViewModel = createClientsViewModel({
    client: clientDirectory,
  })
  // CLIENT-300.5: the device card occupancy interactions run against the
  // frozen occupancy facade (claim / drain-aware release / cancel-and-release
  // with confirmation); without a Client surface the model mounts with a null
  // port and reports the honest unavailable failure instead of pretending the
  // actions landed.
  const clientOccupancy = createClientOccupancyFacade({
    client: rawControlPlane,
    transport: browserTransport,
  })
  const occupancyModel: ClientOccupancyViewModel = createClientOccupancyViewModel({
    port: clientOccupancyPortFromFacade(clientOccupancy),
    clients: clientsModel,
  })
  // UI-100.1: the Owner user management area talks to the real user
  // endpoints; the self-service password form stays disabled until the
  // signed-in account id is exposed by the session model.
  const usersRoot = element(document, 'div', 'wwc-users-root')
  const usersModel: UserManagementViewModel = createUserManagementViewModel({
    port: {
      listUsers: () => clientUsers.listUsers(),
      create: input => clientUsers.createUser(input),
      setState: input => clientUsers.setUserState(input),
      resetPassword: input => clientUsers.resetUserPassword(input),
    },
  })
  // REPO-100.3: the repository list talks to the same directory facade; the
  // list is a Server snapshot read, so expected failures stay inside the area.
  const repositoriesModel: RepositoriesViewModel = createRepositoriesViewModel({
    client: clientDirectory,
  })
  // UI-100.2 (fake-first): the §16.6 task creation seam and the §16.7 run
  // identity zone run on the local fakes from the one facade block until the
  // FLOW scheduler and worker/candidate routing land and replace the ports.
  const taskPort = createControlPlaneTaskFake({
    nextTaskId: () => contractId('tsk', browser.crypto),
  })
  const runIdentityPort = createControlPlaneRunIdentityFake()
  let lastKnownDiagnosticScope: unknown = null
  const shell = element(document, 'div', 'wwc-shell')
  const header = element(document, 'header', 'wwc-header')
  const skipLink = element(document, 'a', 'wwc-skip-link')
  const brand = element(document, 'strong', 'wwc-brand')
  const navigation = element(document, 'nav', 'wwc-navigation')
  const authRoot = element(document, 'div', 'wwc-auth-session-root')
  const loginRoot = element(document, 'div', 'wwc-login-root')
  const clientsRoot = element(document, 'div', 'wwc-clients-root')
  const repositoriesRoot = element(document, 'div', 'wwc-repositories-root')
  const main = element(document, 'main', 'wwc-main')
  const scopeRoot = element(document, 'div', 'wwc-scope-selector-root')
  const readinessRoot = element(document, 'div', 'wwc-readiness-root')
  // Pages own their page headers (design); the shell title elements stay in
  // the DOM for ARIA but render empty.
  const title = element(document, 'h1', 'wwc-surface-title')
  const description = element(document, 'p', 'wwc-surface-description')
  const readOnlyNotice = element(document, 'p', 'wwc-surface-read-only')
  const slot = element(document, 'section', 'wwc-surface-slot')
  const links = new Map<ClientSurfaceId, HTMLAnchorElement>()
  let activeSurface = clientSurfaceFromHash(browser.location.hash)
  let activeFeature: EnterpriseApplication | MountedClientFeature | null = null
  let scopeSelectorPage: ScopeSelectorPage | null = null
  let currentScopeResolution: ScopeContextResolution | null = null
  let featureController: AbortController | null = null
  let renderGeneration = 0
  let currentFailure: ClientFailure | null = null
  let activeRouteReadOnly = false
  let revokedScopeIdentity: string | null = null
  let closed = false
  // UI-506: one shell-owned notification monitor for the selected repository Scope.
  let attentionMonitor: AttentionNotificationMonitor | null = null
  let attentionMonitorScope: string | null = null
  // UI-504: one browser-local history of opened Deliveries, keyed by Scope, that
  // the Home dashboard renders as its "recently opened" section.
  const homeVisits: HomeRecentVisitStore = createHomeRecentVisitStore({
    storage: browserHomeVisitStorage(browser),
  })

  /** Records one Delivery visit; every other route leaves the history alone. */
  function recordHomeVisit(hash: string): void {
    const deliveryId = homeDeliveryVisitFromHash(hash)
    if (deliveryId === null) return
    homeVisits.record(deliveryId, scopeSelectionFromHash(hash), Date.now())
  }

  function selectionIdentity(selection: ScopeRouteSelection): string {
    return [
      selection.organizationId ?? '',
      selection.workspaceId ?? '',
      selection.projectId ?? '',
      selection.repositoryId ?? '',
    ].join('\u0000')
  }

  function selectionLeavesRevokedScope(selection: ScopeRouteSelection): boolean {
    if (revokedScopeIdentity === null) return false
    const revoked = revokedScopeIdentity.split('\u0000')
    return [
      selection.organizationId,
      selection.workspaceId,
      selection.projectId,
      selection.repositoryId,
    ].some((value, index) => value !== null && value !== revoked[index])
  }

  function diagnosticScope(): unknown {
    const session = authSession.state.session
    if (session === null) return lastKnownDiagnosticScope
    const resolution = resolveScopeContext(
      session.authorizedScopes,
      browser.location.hash,
      activeSurface.id === 'enterprise' ? 'scope' : 'repository',
    )
    const scope = resolution.status === 'selected' ? resolution.scope : null
    lastKnownDiagnosticScope = scope
    return scope
  }

  function diagnosticText(
    state: ConnectionSnapshot = connection.state,
    failure: ClientFailure | null = currentFailure,
  ): string {
    return createSafeDiagnostic({
      connection: state,
      failure,
      scope: diagnosticScope(),
      surface: activeSurface.id,
      generatedAt: now(),
    })
  }

  function copyDiagnostic(value: string): Promise<void> | void {
    if (options.copyText !== undefined) return options.copyText(value)
    const navigatorValue = Reflect.get(browser, 'navigator') as Navigator | undefined
    const clipboard = navigatorValue?.clipboard
    if (clipboard === undefined) return Promise.reject(new Error('Clipboard is unavailable.'))
    return clipboard.writeText(value)
  }

  function recoverConnection(): void {
    const status = connection.state.status
    if (status === 'authentication-required') {
      connection.reset()
      void authSession.restore()
      return
    }
    if (status === 'permission-denied' || status === 'version-mismatch') {
      returnToSafeEntry()
      return
    }
    if (status === 'refresh-required') {
      connection.reset()
      render()
      return
    }
    if (!browserIsOnline()) {
      connection.offline()
      return
    }
    queryCache.clear('reconnect')
    observedControlPlane.reconnectAll()
  }

  function retryFailedRoute(): void {
    currentFailure = null
    connection.reset()
    render()
  }

  function returnToSafeEntry(): void {
    currentFailure = null
    connection.reset()
    replaceHash(surfaceHash('/chat', scopeSelectionFromHash(browser.location.hash)))
    render()
  }

  const connectionBar = mountConnectionBar({
    document,
    props: {
      state: connection.state,
      diagnostic: diagnosticText(),
      onRecover: recoverConnection,
      onCopy: copyDiagnostic,
    },
  })
  const errorBoundary = mountClientErrorBoundary({
    document,
    props: {
      failure: null,
      diagnostic: diagnosticText(),
      onRetry: retryFailedRoute,
      onSafeEntry: returnToSafeEntry,
      onCopy: copyDiagnostic,
    },
  })

  function updateReliabilityViews(state: ConnectionSnapshot): void {
    connectionBar.update({
      state,
      diagnostic: diagnosticText(state),
      onRecover: recoverConnection,
      onCopy: copyDiagnostic,
    })
    errorBoundary.update({
      failure: currentFailure,
      diagnostic: diagnosticText(state),
      onRetry: retryFailedRoute,
      onSafeEntry: returnToSafeEntry,
      onCopy: copyDiagnostic,
    })
  }

  function applyFailureStatus(failure: ClientFailure): void {
    if (failure.connectionStatus === 'authentication-required') {
      connection.authenticationRequired(failure.code, failure.requestId)
    } else if (failure.connectionStatus === 'permission-denied') {
      connection.permissionDenied(failure.code, failure.requestId)
    } else if (failure.connectionStatus === 'version-mismatch') {
      connection.versionMismatch(failure.code, failure.requestId)
    } else if (failure.connectionStatus === 'offline') {
      connection.offline(failure.code, failure.requestId)
    } else if (failure.connectionStatus === 'reconnecting') {
      connection.reconnecting(failure.code, failure.requestId)
    } else {
      connection.refreshRequired(failure.code, failure.requestId)
    }
  }

  function showRouteFailure(error: unknown, fallbackCode: string): void {
    if (closed) return
    const failure = classifyClientFailure(
      error,
      fallbackCode,
      browserIsOnline(),
    )
    if (failure.category === 'cancelled') return
    currentFailure = failure
    applyFailureStatus(currentFailure)
    slot.hidden = true
    updateReliabilityViews(connection.state)
  }

  function clearRouteFailure(): void {
    currentFailure = null
    slot.hidden = false
    updateReliabilityViews(connection.state)
  }

  brand.textContent = 'WinWinCode'
  const brandEdition = element(document, 'span', 'wwc-brand-edition')
  brandEdition.textContent = '社区版'
  brand.append(brandEdition)
  navigation.setAttribute('aria-label', '产品导航')
  readOnlyNotice.setAttribute('role', 'status')
  readOnlyNotice.textContent = '此区域为只读。写入操作不可用，服务端授权仍然生效。'
  readOnlyNotice.hidden = true
  // UI-604: the surface slot holds the whole mounted page.  Marking it as a live
  // region queued every realtime DOM change for announcement and nested inside
  // the page's own status regions, so the shell stays silent and each page keeps
  // exactly one polite channel for its own status line.
  skipLink.href = '#wwc-main'
  skipLink.textContent = '跳到主内容'
  skipLink.addEventListener('click', event => {
    event.preventDefault()
    main.focus()
  })
  main.tabIndex = -1
  main.id = 'wwc-main'

  const NAV_ICONS: Partial<Record<ClientSurfaceId, string>> = {
    chat: '<path d="M4 5h16v11H8l-4 4z"/>',
    home: '<rect x="4" y="4" width="7" height="7"/><rect x="13" y="4" width="7" height="7"/><rect x="4" y="13" width="7" height="7"/><rect x="13" y="13" width="7" height="7"/>',
    projects: '<path d="M3 6h6l2 2h10v11H3z"/>',
    extensions: '<path d="M10 4h4v3a2 2 0 1 0 4 0h3v4h-3a2 2 0 1 0 0 4h3v4h-4v-3a2 2 0 1 0-4 0v3H6v-4H4v-4h3a2 2 0 1 0 0-4H4V4h6z"/>',
    device: '<rect x="3" y="4" width="18" height="12"/><path d="M9 20h6M12 16v4"/>',
    settings: '<circle cx="12" cy="12" r="3"/><path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1"/>',
  }
  for (const surface of CLIENT_SURFACES) {
    if (!surface.nav) continue
    const link = element(document, 'a', 'wwc-navigation-link')
    link.href = `#${surface.path}`
    const icon = element(document, 'span', 'wwc-navigation-icon')
    icon.innerHTML = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" width="18" height="18">${NAV_ICONS[surface.id] ?? ''}</svg>`
    const text = element(document, 'span', 'wwc-navigation-text')
    text.textContent = surface.label
    link.append(icon, text)
    link.dataset.surface = surface.id
    link.hidden = true
    link.addEventListener('click', event => {
      if (link.getAttribute('aria-disabled') !== 'true') return
      event.preventDefault()
    })
    links.set(surface.id, link)
    navigation.append(link)
  }

  // Design sidebar 「最近对话」: browser-local session titles written by the
  // Chat page; the shell only renders the titles.
  const recentChatsLabel = element(document, 'p', 'wwc-sidebar-section-label')
  recentChatsLabel.textContent = '最近对话'
  const recentChatsList = element(document, 'ul', 'wwc-sidebar-recent')
  const recentChatsRoot = element(document, 'div', 'wwc-recent-chats')
  recentChatsRoot.append(recentChatsLabel, recentChatsList)
  function renderRecentChats(): void {
    const entries = loadRecentChats(browser.localStorage ?? null)
    recentChatsList.replaceChildren(...entries.map(entry => {
      const row = element(document, 'li', 'wwc-sidebar-recent-item')
      row.textContent = entry.title
      row.dataset.sessionKey = entry.sessionKey
      return row
    }))
    recentChatsRoot.hidden = entries.length === 0
  }
  renderRecentChats()
  if (typeof window !== 'undefined') {
    window.addEventListener('wwc:recent-chats-changed', renderRecentChats)
  }


  header.append(skipLink, brand, navigation, recentChatsRoot, authRoot)
  authRoot.hidden = true
  main.append(
    scopeRoot,
    readinessRoot,
    title,
    description,
    readOnlyNotice,
    loginRoot,
    clientsRoot,
    repositoriesRoot,
    errorBoundary.root,
    slot,
  )
  shell.append(header, connectionBar.root, main)
  options.root.replaceChildren(shell)
  const authPage: AuthSessionPage = mountAuthSessionPage({
    root: authRoot,
    model: authSession,
  })
  const loginPage: LoginPage = mountLoginPage({
    root: loginRoot,
    model: loginModel,
  })
  const clientsPage: ClientsPage = mountClientsPage({
    root: clientsRoot,
    model: clientsModel,
    occupancy: occupancyModel,
    now: () => Date.parse(now()),
    onDeviceSelect: clientId => {
      void repositoriesModel.showDevice(clientId)
    },
  })
  const repositoriesPage: RepositoriesPage = mountRepositoriesPage({
    root: repositoriesRoot,
    model: repositoriesModel,
  })
  const usersPage: UsersPage = mountUsersPage({
    root: usersRoot,
    model: usersModel,
  })

  /**
   * The login page is the unauthenticated surface. It appears on sign-out and
   * on session expiry, and the URL hash never changes, so a successful
   * sign-in re-renders the originally requested route.
   */
  let loginVisible: boolean | null = null
  let clientsVisible: boolean | null = null
  function updateClientsVisibility(status: AuthSessionViewModel['state']['status']): void {
    const visible = status === 'signed-in'
    if (clientsVisible === visible) return
    clientsVisible = visible
    clientsPage.setVisible(visible)
    if (visible) void clientsModel.refresh()
    // REPO-100.3: the repository area follows the Clients area's signed-in
    // visibility; its content is driven by the device card selection.
    repositoriesPage.setVisible(visible)
    if (visible && repositoriesModel.state.clientId !== null) {
      void repositoriesModel.refresh()
    }
  }
  function updateLoginVisibility(): void {
    const status = authSession.state.status
    // CLIENT-200.4: the Clients area is the signed-in device directory, so its
    // visibility is decided independently of the login page's early return.
    updateClientsVisibility(status)
    const visible = status === 'signed-out'
      || status === 'restoring'
      || status === 'authentication-required'
    if (loginVisible === visible) return
    loginVisible = visible
    loginPage.setVisible(visible)
    if (visible) {
      // Each unauthenticated episode arms a fresh form; the previous
      // submission outcome must never re-trigger a session restore.
      loginModel.reset()
      void loginModel.refreshInitialization()
    }
  }
  const unsubscribeLoginModel = loginModel.subscribe(state => {
    if (state.status === 'succeeded' && authSession.state.status !== 'signed-in') {
      void authSession.restore()
    }
  })

  function readinessFixTarget(item: ReadinessItemState): ReadinessFixTarget | null {
    const resolution = currentScopeResolution
    const selection = resolution !== null && resolution.status === 'selected'
      ? resolution.selection
      : scopeSelectionFromHash(browser.location.hash)
    if (item.id === 'model-route' || item.id === 'credential-reference') {
      return { href: surfaceHash('/settings', selection), label: '打开设置' }
    }
    if (item.id === 'server-worker-health' || item.id === 'helper-availability') {
      return {
        href: surfaceHash('/settings/runtime', selection),
        label: '打开本地运维诊断',
      }
    }
    if (item.id === 'first-chat-delivery') {
      return item.reason === 'no-delivery'
        ? {
            href: surfaceHash('/strongflow', selection),
            label: '创建你的第一个交付',
          }
        : { href: surfaceHash('/chat', selection), label: '开始你的第一次对话' }
    }
    return null
  }

  const readiness = createReadinessViewModel({
    client: controlPlane,
    serverStatus: () => connection.state.status,
    now,
    nextRequestId: () => contractId('req', browser.crypto) as RequestId,
  })
  const readinessPage = mountReadinessPage({
    root: readinessRoot,
    model: readiness,
    fixTarget: readinessFixTarget,
  })

  function updateNavigation(): NavigationCapabilityProjection {
    const projection = projectionForSession(
      authSession.state,
      options.navigationCapabilities,
    )
    const visible: HTMLAnchorElement[] = []
    for (const entry of projection.surfaces) {
      const link = links.get(entry.surface.id)
      if (link === undefined) continue
      link.href = surfaceHash(entry.surface.path, scopeSelectionFromHash(browser.location.hash))
      link.dataset.capability = entry.capability
      link.hidden = entry.capability === 'hidden'
      link.textContent = entry.capability === 'read-only'
        ? `${entry.surface.label}（只读）`
        : entry.capability === 'disabled'
          ? `${entry.surface.label}（不可用）`
          : entry.surface.label
      if (entry.capability === 'disabled') {
        link.setAttribute('aria-disabled', 'true')
        link.tabIndex = -1
        link.title = `${entry.surface.description}。当前身份不可用。`
      } else {
        link.removeAttribute('aria-disabled')
        link.tabIndex = 0
        link.title = entry.capability === 'read-only'
          ? `${entry.surface.description}。只读访问。`
          : entry.surface.description
      }
      if (!link.hidden) visible.push(link)
    }
    navigation.replaceChildren(...visible)
    navigation.dataset.deployment = projection.deployment
    // Rebuilding the labels also rebuilds the badge, so it is re-applied here.
    attentionMonitor?.applyBadge()
    return projection
  }

  function routeDenied(capability: SurfaceCapability): void {
    const denied = element(document, 'div', 'wwc-surface-route-denied')
    const message = element(document, 'p', 'wwc-surface-route-denied-message')
    const safeEntry = element(document, 'a', 'wwc-surface-route-safe-entry')
    denied.setAttribute('role', 'alert')
    denied.dataset.capability = capability.capability
    message.textContent = `当前身份无法使用${capability.surface.label}。`
    safeEntry.href = surfaceHash('/chat', scopeSelectionFromHash(browser.location.hash))
    safeEntry.textContent = '返回新对话'
    denied.append(message, safeEntry)
    slot.replaceChildren(denied)
  }

  onRouteAuthorizationRevoked = () => {
    if (closed) return
    if (currentScopeResolution?.status === 'selected') {
      revokedScopeIdentity = selectionIdentity(currentScopeResolution.selection)
    }
    renderGeneration += 1
    featureController?.abort()
    featureController = null
    activeFeature?.close()
    activeFeature = null
    scopeSelectorPage?.close()
    scopeSelectorPage = null
    currentScopeResolution = null
    closeAttentionMonitor()
    const scopeRevoked = element(document, 'p', 'wwc-scope-selector-access')
    scopeRevoked.setAttribute('role', 'alert')
    scopeRevoked.textContent = '此范围的授权已被撤销。请返回安全入口并恢复访问。'
    scopeRoot.replaceChildren(scopeRevoked)
    clearRouteFailure()
    const link = links.get(activeSurface.id)
    link?.setAttribute('data-route-access', 'denied')
    slot.dataset.routeAccess = 'denied'
    const current = surfaceCapabilityForHash(
      browser.location.hash,
      authSession.state,
      options.navigationCapabilities,
    )
    routeDenied({
      ...current,
      capability: 'disabled',
      reason: 'capability-denied',
    })
  }

  function closeAttentionMonitor(): void {
    attentionMonitor?.close()
    attentionMonitor = null
    attentionMonitorScope = null
  }

  /** The shell-owned control the Attention Center consent button binds to. */
  function notificationsControl(): { readonly notifications: AttentionNotificationControl } | {} {
    return attentionMonitor === null ? {} : { notifications: attentionMonitor }
  }

  /**
   * Keep exactly one shell-owned notification monitor for the selected
   * repository Scope.  It opens no event subscription of its own, so the
   * mounted feature routes keep their single event stream.
   */
  async function ensureAttentionMonitor(
    generation: number,
    actor: NonNullable<AuthSessionViewModel['state']['session']>['actor'],
    scope: RepositoryScope,
  ): Promise<void> {
    const identity = selectionIdentity({
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    })
    if (attentionMonitor !== null && attentionMonitorScope === identity) return
    closeAttentionMonitor()
    const badgeTarget = links.get('attention')
    if (badgeTarget === undefined) return
    const notifications = await import('./attention-notifications.js')
    if (closed || generation !== renderGeneration) return
    const monitor = notifications.createAttentionNotificationMonitor({
      client: controlPlane,
      actor,
      scope,
      nextRequestId: () => contractId('req', browser.crypto) as RequestId,
      document: browser.document,
      badgeTarget,
      notifications: notifications.browserAttentionDesktopNotifications(browser),
      onOpenTarget(hash) {
        if (closed) return
        browser.location.hash = hash
        render()
      },
    })
    monitor.subscribe(() => { updateNavigation() })
    attentionMonitor = monitor
    attentionMonitorScope = identity
    await monitor.start()
  }

  function authenticatedRouteContext(): {
    readonly actor: NonNullable<AuthSessionViewModel['state']['session']>['actor']
    readonly scope: RepositoryScope
  } | null {
    const session = authSession.state.session
    const resolution = currentScopeResolution
    const scope = resolution?.status === 'selected' && resolution.scope.kind === 'repository'
      ? resolution.scope
      : null
    if (authSession.state.status !== 'signed-in' || session === null || scope === null) {
      const unavailable = element(document, 'p', 'wwc-authenticated-context-required')
      unavailable.setAttribute(
        'role',
        resolution?.status === 'denied' ? 'alert' : 'status',
      )
      unavailable.textContent = authSession.state.status === 'restoring'
        ? '正在恢复登录状态…'
        : resolution?.status === 'denied'
          ? 'URL 中的仓库范围未获授权，请选择其他范围。'
          : resolution?.status === 'selection-required'
            ? '选择一个已授权的仓库范围以打开工作区。'
            : authSession.state.status === 'signed-in' && session !== null
              ? '当前账号没有可用的仓库工作区。'
          : '登录后打开工作区。'
      slot.replaceChildren(unavailable)
      return null
    }
    return { actor: session.actor, scope }
  }

  function routeLoading(message: string): void {
    const loading = element(document, 'p', 'wwc-feature-route-loading')
    loading.setAttribute('role', 'status')
    loading.textContent = message
    slot.replaceChildren(loading)
  }

  function routeUnavailable(message: string): void {
    const unavailable = element(document, 'p', 'wwc-feature-route-unavailable')
    unavailable.setAttribute('role', 'status')
    unavailable.textContent = message
    slot.replaceChildren(unavailable)
  }

  function replaceHash(hash: string): void {
    browser.history.replaceState(null, '', `${browser.location.pathname}${browser.location.search}${hash}`)
  }

  async function renderChat(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载新对话…')
    try {
      const parameters = routeParameters(browser.location.hash)
      const productSessionId = parameters.get('session') as ProductSessionId | null
      const [
        { createChatViewModel },
        { mountChatPage },
        { createStrongFlowCreateViewModel },
      ] = await Promise.all([
        import('./chat-view-model.js'),
        import('./chat-page.js'),
        import('./strongflow-view-model.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const model = createChatViewModel({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        productSessionId,
        nextSubscriptionId: () => contractId(
          'sub',
          browser.crypto,
        ) as ControlPlaneWebSocketSubscriptionId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        onActiveSessionChange(nextProductSessionId) {
          if (closed || generation !== renderGeneration || controller.signal.aborted) return
          replaceHash(scopeHash(
            `#/chat?session=${encodeURIComponent(nextProductSessionId)}`,
            scopeSelectionFromHash(browser.location.hash),
          ))
        },
      })
      const deliveryCreator = createStrongFlowCreateViewModel({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        nextDeliveryId: () => contractId('dlv', browser.crypto) as DeliveryId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        onCreated(deliveryId) {
          if (closed || generation !== renderGeneration || controller.signal.aborted) return
          replaceHash(scopeHash(
            `#/strongflow?delivery=${encodeURIComponent(deliveryId)}`,
            scopeSelectionFromHash(browser.location.hash),
          ))
          render()
        },
      })
      activeFeature = mountChatPage({
        root: slot,
        model,
        deliveryCreator,
        scope: context.scope,
        settingsHref: scopeHash(
          '#/settings',
          scopeSelectionFromHash(browser.location.hash),
        ),
        readOnly: activeRouteReadOnly,
        nextProductSessionId: () => contractId(
          'psn',
          browser.crypto,
        ) as ProductSessionId,
      })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'CHAT_ROUTE_FAILURE')
    }
  }

  /** Design page 07: 项目与仓库 list. Backed by the directory facades the shell
   *  already owns; the page module owns DOM and presentation only. */
  async function renderProjects(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载项目…')
    try {
      const [{ renderProjectsPage }] = await Promise.all([
        import('./projects-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      activeFeature = renderProjectsPage({
        root: slot,
        clientDirectory,
        newChatHref: '#/chat',
        deviceHref: '#/device',
        requestOptions: () => undefined,
      })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'PROJECTS_ROUTE_FAILURE')
    }
  }

  /** Design page 08: 执行设备. Reuses the shell-owned Clients model. */
  async function renderDevice(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载执行设备…')
    try {
      const [{ renderDevicePage }] = await Promise.all([
        import('./device-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      activeFeature = renderDevicePage({
        root: slot,
        clientDirectory,
        homeHref: '#/home',
        projectsHref: '#/projects',
        requestOptions: () => undefined,
      })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'DEVICE_ROUTE_FAILURE')
    }
  }

  /** UI-EXT-100: the extensions hub is presentation-only until the control
   *  plane exposes plugin, skill, and MCP inventories; no model is mounted. */
  async function renderExtensions(generation: number): Promise<void> {
    if (authenticatedRouteContext() === null) return
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载扩展…')
    try {
      const [{ mountExtensionsPage }] = await Promise.all([
        import('./extensions-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      activeFeature = mountExtensionsPage({ root: slot })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'EXTENSIONS_ROUTE_FAILURE')
    }
  }

  async function renderSettings(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const controller = new AbortController()
    featureController = controller
    const operationsRoute = browser.location.hash
      .replace(/^#/u, '')
      .replace(/\?.*$/u, '') === '/settings/runtime'
    routeLoading(operationsRoute ? '正在加载本地运维…' : '正在加载设置…')
    try {
      if (operationsRoute) {
        const [
          { createLocalOperationsViewModel },
          { mountLocalOperationsPage },
          { createUsageHealthViewModel },
          { mountUsageHealthSummary },
        ] = await Promise.all([
          import('./local-operations-view-model.js'),
          import('./local-operations-page.js'),
          import('./usage-health-view-model.js'),
          import('./usage-health-page.js'),
        ])
        if (closed || generation !== renderGeneration || controller.signal.aborted) return
        const model = createLocalOperationsViewModel({
          client: controlPlane,
          actor: context.actor,
          scope: context.scope,
          subscriptionId: contractId(
            'sub',
            browser.crypto,
          ) as ControlPlaneWebSocketSubscriptionId,
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        })
        const operationsPage = mountLocalOperationsPage({
          root: slot,
          model,
          readOnly: activeRouteReadOnly,
          onOpenReadiness() {
            if (!closed) readiness.setCollapsed(false)
          },
        })
        // UI-505: the diagnostics route also carries the read-only Usage, Provider,
        // Credential and Worker health summary next to the local operations panel.
        let mountedHealth: { close(): void } | null = null
        try {
          const healthRoot = element(document, 'div', 'wwc-usage-health-root')
          slot.append(healthRoot)
          const healthModel = createUsageHealthViewModel({
            client: controlPlane,
            actor: context.actor,
            scope: context.scope,
            nextRequestId: () => contractId('req', browser.crypto) as RequestId,
          })
          const healthSummary = mountUsageHealthSummary({
            root: healthRoot,
            model: healthModel,
          })
          void healthModel.start().catch(() => {})
          mountedHealth = {
            close() {
              healthSummary.close()
              healthModel.close()
            },
          }
        } catch {
          // The local operations panel stays usable when only this summary fails to mount.
        }
        activeFeature = {
          close() {
            mountedHealth?.close()
            operationsPage.close()
          },
        }
        return
      }
      const [{ createSettingsViewModel }, { mountSettingsPage }] = await Promise.all([
        import('./settings-view-model.js'),
        import('./settings-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const model = createSettingsViewModel({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        subscriptionId: contractId(
          'sub',
          browser.crypto,
        ) as ControlPlaneWebSocketSubscriptionId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
      })
      activeFeature = mountSettingsPage({
        root: slot,
        model,
        localOperationsHref: scopeHash(
          '#/settings/runtime',
          scopeSelectionFromHash(browser.location.hash),
        ),
        readOnly: activeRouteReadOnly,
      })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, operationsRoute
        ? 'LOCAL_OPERATIONS_ROUTE_FAILURE'
        : 'SETTINGS_ROUTE_FAILURE')
    }
  }

  /**
   * UX-100.1: My Work is the converged post-login first screen.  It reuses the
   * existing Home dashboard projection (Attention, Delivery, Usage) as its work
   * sections, adds the §16.2 start-task entry and the Clients status zone from
   * the shell-owned Clients area model, and keeps every Home deep link.
   */
  async function renderHome(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载任务看板…')
    try {
      const [{ createMyWorkViewModel }, { mountMyWorkPage }] = await Promise.all([
        import('./my-work-view-model.js'),
        import('./my-work-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const model = createMyWorkViewModel({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        subscriptionId: contractId(
          'sub',
          browser.crypto,
        ) as ControlPlaneWebSocketSubscriptionId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        // The Clients zone reuses the one shell-owned Clients area model, so
        // the first screen never grows a second device-list state.
        clients: clientsModel,
        visits: homeVisits,
      })
      // The page owns the composed model: its close chain also closes the
      // Attention, Delivery and Usage projections it mounted.
      activeFeature = mountMyWorkPage({
        root: slot,
        model,
        scopeSelection: scopeSelectionFromHash(browser.location.hash),
        ownsModel: true,
      })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'HOME_ROUTE_FAILURE')
    }
  }

  /** The Home surface carries the §16.6/§16.7 sub-routes under its path. */
  function homeSubRoute(): 'my-work' | 'task-entry' | 'task-run' {
    const path = browser.location.hash.replace(/^#/u, '').replace(/\?.*$/u, '')
    if (path === '/home/new-task') return 'task-entry'
    if (path === '/home/run') return 'task-run'
    return 'my-work'
  }

  /** The anchor facts a deep-linked run route must carry to be actionable. */
  function taskRunRouteAnchor(): {
    readonly taskId: string
    readonly clientId: string
    readonly repositoryBindingId: string
  } | null {
    const parameters = routeParameters(browser.location.hash)
    const taskId = parameters.get('task')
    const clientId = parameters.get('client')
    const repositoryBindingId = parameters.get('repository')
    if (
      taskId === null || clientId === null || repositoryBindingId === null
      || taskId.length === 0 || clientId.length === 0 || repositoryBindingId.length === 0
    ) {
      return null
    }
    return { taskId, clientId, repositoryBindingId }
  }

  /**
   * UI-100.2 (fake-first): the §16.6 new-task form.  It reuses the shell-owned
   * Clients and Repositories models for its options, creates the task through
   * the fake-first task port, and lands on the run route with the task anchor
   * in the URL (form values like the description never enter the URL).
   */
  async function renderTaskEntry(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载新任务表单…')
    try {
      const [{ createTaskEntryViewModel }, { mountTaskEntryPage }] = await Promise.all([
        import('./task-entry-view-model.js'),
        import('./task-entry-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const model = createTaskEntryViewModel({
        clients: clientsModel,
        repositories: repositoriesModel,
        port: taskPort,
      })
      activeFeature = mountTaskEntryPage({
        root: slot,
        model,
        onStarted(anchor: ControlPlaneTaskAnchor) {
          if (closed || generation !== renderGeneration || controller.signal.aborted) return
          const parameters = new URLSearchParams({
            task: anchor.taskId,
            client: anchor.clientId,
            repository: anchor.repositoryBindingId,
          })
          replaceHash(scopeHash(
            `#/home/run?${parameters.toString()}`,
            scopeSelectionFromHash(browser.location.hash),
          ))
          render()
        },
      })
      await model.start()
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'TASK_ENTRY_ROUTE_FAILURE')
    }
  }

  /**
   * UI-100.2 (fake-first): the §16.7 run page.  The Client, Occupancy, and
   * Repository rows project the live shell-owned models; the WorkerSession
   * and Candidate/Apply rows come from the fake-first identity port.
   */
  async function renderTaskRun(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const anchorFacts = taskRunRouteAnchor()
    if (anchorFacts === null) {
      routeUnavailable('This task link is incomplete. Start a task from My Work.')
      return
    }
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载运行中任务…')
    try {
      const [{ createTaskRunViewModel }, { mountTaskRunPage }] = await Promise.all([
        import('./task-run-view-model.js'),
        import('./task-run-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const knownAnchor = taskPort.describe(anchorFacts.taskId)
      const anchor: ControlPlaneTaskAnchor = knownAnchor ?? Object.freeze({
        ...anchorFacts,
        baseBranch: '',
        description: '',
        modelRouteId: '',
      })
      const model = createTaskRunViewModel({
        anchor,
        taskDescription: knownAnchor?.description ?? null,
        clients: clientsModel,
        repositories: repositoriesModel,
        identity: runIdentityPort,
      })
      activeFeature = mountTaskRunPage({
        root: slot,
        model,
        homeHref: surfaceHash('/home', scopeSelectionFromHash(browser.location.hash)),
      })
      await model.start()
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'TASK_RUN_ROUTE_FAILURE')
    }
  }

  async function renderAttention(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const parameters = routeParameters(browser.location.hash)
    const productSessionId = parameters.get('session') as ProductSessionId | null
    if (productSessionId !== null) {
      const controller = new AbortController()
      featureController = controller
      routeLoading('正在加载会话决策…')
      try {
        const deliveryId = parameters.get('delivery') as DeliveryId | null
        const stageRunId = canonicalStageRunParameter(parameters)
        const [{ createLocalDecisionsViewModel }, { mountLocalDecisionsPage }] = await Promise.all([
          import('./local-decisions-view-model.js'),
          import('./local-decisions-page.js'),
        ])
        if (closed || generation !== renderGeneration || controller.signal.aborted) return
        const model = createLocalDecisionsViewModel({
          client: controlPlane,
          actor: context.actor,
          scope: context.scope,
          productSessionId,
          interactionSubscriptionId: contractId(
            'sub',
            browser.crypto,
          ) as ControlPlaneWebSocketSubscriptionId,
          ...(deliveryId === null
            ? {}
            : {
                delivery: {
                  deliveryId,
                  subscriptionId: contractId(
                    'sub',
                    browser.crypto,
                  ) as ControlPlaneWebSocketSubscriptionId,
                },
              }),
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        })
        activeFeature = mountLocalDecisionsPage({
          root: slot,
          model,
          readOnly: activeRouteReadOnly,
          // The exact execution origin this decision came from, so handling the
          // decision can return to the Task/StageRun that raised it.
          ...(deliveryId === null || stageRunId === null
            ? {}
            : {
                returnTarget: {
                  hash: strongFlowRouteHash({
                    deliveryId,
                    productSessionId,
                    stageRunId,
                    candidatePath: null,
                    candidateView: 'unified',
                    comparison: { status: 'none' },
                    evidenceTab: 'evidence',
                    evidenceId: null,
                  }, scopeSelectionFromHash(browser.location.hash)),
                  label: 'Return to execution context',
                },
              }),
        })
      } catch (error) {
        if (closed || generation !== renderGeneration || controller.signal.aborted) return
        showRouteFailure(error, 'ATTENTION_ROUTE_FAILURE')
      }
      return
    }
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载待我处理…')
    try {
      const [{ createAttentionCenterViewModel }, { mountAttentionCenterPage }] = await Promise.all([
        import('./attention-center-view-model.js'),
        import('./attention-center-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const model = createAttentionCenterViewModel({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        subscriptionId: contractId(
          'sub',
          browser.crypto,
        ) as ControlPlaneWebSocketSubscriptionId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
      })
      activeFeature = mountAttentionCenterPage({
        root: slot,
        model,
        scopeSelection: scopeSelectionFromHash(browser.location.hash),
        // The page owns the Attention Center snapshot; the shell notification
        // monitor is a separate, lighter projection for badges and alerts.
        ownsModel: true,
        ...notificationsControl(),
        readOnly: activeRouteReadOnly,
      })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'ATTENTION_ROUTE_FAILURE')
    }
  }

  async function renderStrongFlow(generation: number): Promise<void> {
    const routeContext = authenticatedRouteContext()
    if (routeContext === null) return
    const context = routeContext
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载 StrongFlow…')
    let deliveryList: Awaited<ReturnType<typeof createStrongFlowDeliveryList>> | null = null
    async function createStrongFlowDeliveryList() {
      const { createStrongFlowDeliveryListViewModel } = await import(
        './strongflow-delivery-list-view-model.js'
      )
      const list = createStrongFlowDeliveryListViewModel({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        signal: controller.signal,
      })
      await list.start()
      return list
    }
    try {
      const route = parseStrongFlowRouteHash(browser.location.hash)
      deliveryList = await createStrongFlowDeliveryList()
      if (closed || generation !== renderGeneration || controller.signal.aborted) {
        deliveryList.close()
        deliveryList = null
        return
      }
      // A failed first page leaves no honest fallback selection: fail the route
      // instead of showing the empty-repository create surface.
      if (route.deliveryId === null && deliveryList.state.error !== null) {
        throw new Error('The Delivery list could not be loaded for this repository.')
      }
      const deliveries = deliveryList.state.visible
      const deliveryId = route.deliveryId ?? deliveries[0]?.deliveryId ?? null
      if (deliveryId === null) {
        deliveryList.close()
        deliveryList = null
        const [{ createStrongFlowCreateViewModel }, { mountStrongFlowCreatePage }] = await Promise.all([
          import('./strongflow-view-model.js'),
          import('./strongflow-page.js'),
        ])
        if (closed || generation !== renderGeneration || controller.signal.aborted) return
        const model = createStrongFlowCreateViewModel({
          client: controlPlane,
          actor: context.actor,
          scope: context.scope,
          nextDeliveryId: () => contractId('dlv', browser.crypto) as DeliveryId,
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
          onCreated(createdDeliveryId) {
            if (closed || generation !== renderGeneration || controller.signal.aborted) return
            replaceHash(strongFlowRouteHash({
              deliveryId: createdDeliveryId,
              productSessionId: null,
              stageRunId: null,
              candidatePath: null,
              candidateView: 'unified',
              comparison: { status: 'none' },
              evidenceTab: 'evidence',
              evidenceId: null,
            }, scopeSelectionFromHash(browser.location.hash)))
            render()
          },
        })
        activeFeature = mountStrongFlowCreatePage({
          root: slot,
          model,
          scope: context.scope,
          readOnly: activeRouteReadOnly,
        })
        return
      }
      let detailValue
      try {
        detailValue = await controlPlane.query({
          schemaVersion: 'winwincode/v1',
          requestId: contractId('req', browser.crypto) as RequestId,
          actor: context.actor,
          scope: context.scope,
          query: QueryName.DeliveryGet,
          parameters: { deliveryId },
          page: { cursor: null, limit: 1 },
        }, { signal: controller.signal })
      } catch (error) {
        if (error instanceof ControlPlaneClientError && error.code === 'RESOURCE_NOT_FOUND') {
          const unavailable = element(document, 'p', 'wwc-feature-route-unavailable')
          unavailable.setAttribute('role', 'alert')
          unavailable.textContent = 'This StrongFlow link no longer names an available Delivery.'
          slot.replaceChildren(unavailable)
          return
        }
        throw error
      }
      if (detailValue.query !== QueryName.DeliveryGet) {
        throw new Error('The StrongFlow route received another detail response.')
      }
      const detail = (detailValue as DeliveryGetResultResponse).result
      const requestedStageRunId = route.stageRunId
      let selectedCandidatePath = route.candidatePath
      const routeCandidateView = strongFlowCandidateViewFromHash(browser.location.hash)
      const routeCandidateFile = strongFlowRawCandidateFileFromHash(browser.location.hash)
      let candidateView: CandidateDiffViewMode = route.candidateView
      const stage = requestedStageRunId === null
        ? [...detail.stages].reverse().find(candidate => candidate.sessionBinding !== null)
        : detail.stages.find(candidate => candidate.id === requestedStageRunId)
      const productSessionId = route.productSessionId
        ?? stage?.sessionBinding?.productSessionId
        ?? null
      if (stage === undefined || stage.sessionBinding === null || productSessionId === null) {
        deliveryList.close()
        deliveryList = null
        routeUnavailable('This Delivery does not have an executable StrongFlow stage yet.')
        return
      }
      if (closed || generation !== renderGeneration || controller.signal.aborted) {
        deliveryList.close()
        deliveryList = null
        return
      }
      let currentRoute: StrongFlowRoute = Object.freeze({
        ...route,
        deliveryId,
        productSessionId,
        stageRunId: stage.id,
      })
      if (
        route.deliveryId === null
        || route.productSessionId === null
        || route.stageRunId === null
        || routeCandidateView === null
        // An illegal Candidate file deep link was dropped at parse time, so the
        // URL is rewritten to the canonical route without it.
        || routeCandidateFile !== currentRoute.candidatePath
      ) {
        replaceHash(strongFlowRouteHash(
          currentRoute,
          scopeSelectionFromHash(browser.location.hash),
          strongFlowHistorySelectionFromHash(browser.location.hash),
        ))
      }
      const [{ createStrongFlowViewModel }, { mountStrongFlowPage }] = await Promise.all([
        import('./strongflow-view-model.js'),
        import('./strongflow-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) {
        deliveryList.close()
        deliveryList = null
        return
      }
      const model = createStrongFlowViewModel({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        deliveryId,
        productSessionId,
        stageRunId: stage.id,
        subscriptionId: contractId(
          'sub',
          browser.crypto,
        ) as ControlPlaneWebSocketSubscriptionId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        selectedCandidatePath,
        onCandidatePathChange(path) {
          if (closed || generation !== renderGeneration || controller.signal.aborted) return
          selectedCandidatePath = path
          currentRoute = Object.freeze({ ...currentRoute, candidatePath: path })
          replaceHash(strongFlowRouteHash(
            currentRoute,
            scopeSelectionFromHash(browser.location.hash),
            strongFlowHistorySelectionFromHash(browser.location.hash),
          ))
        },
        onStageBindingChange(binding) {
          if (closed || generation !== renderGeneration || controller.signal.aborted) return
          currentRoute = Object.freeze({
            ...currentRoute,
            productSessionId: binding.productSessionId,
            stageRunId: binding.stageRunId,
            evidenceId: null,
          })
          replaceHash(strongFlowRouteHash(
            currentRoute,
            scopeSelectionFromHash(browser.location.hash),
            strongFlowHistorySelectionFromHash(browser.location.hash),
          ))
        },
      })
      activeFeature = mountStrongFlowPage({
        root: slot,
        model,
        deliveryList,
        candidateView,
        comparison: currentRoute.comparison,
        onComparisonSelectionChange(request) {
          if (closed || generation !== renderGeneration || controller.signal.aborted) return
          const comparison: CandidateComparisonRouteSelection = {
            status: 'requested',
            request,
          }
          currentRoute = Object.freeze({ ...currentRoute, comparison })
          replaceHash(strongFlowRouteHash(
            currentRoute,
            scopeSelectionFromHash(browser.location.hash),
            strongFlowHistorySelectionFromHash(browser.location.hash),
          ))
        },
        onCandidateViewModeChange(mode) {
          if (closed || generation !== renderGeneration || controller.signal.aborted) return
          candidateView = mode
          currentRoute = Object.freeze({ ...currentRoute, candidateView: mode })
          replaceHash(strongFlowRouteHash(
            currentRoute,
            scopeSelectionFromHash(browser.location.hash),
            strongFlowHistorySelectionFromHash(browser.location.hash),
          ))
        },
        evidence: {
          client: controlPlane,
          actor: context.actor,
          scope: context.scope,
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
          route: {
            tab: currentRoute.evidenceTab,
            evidenceId: currentRoute.evidenceId,
          },
          onRouteChange(next) {
            if (closed || generation !== renderGeneration || controller.signal.aborted) return
            currentRoute = Object.freeze({
              ...currentRoute,
              evidenceTab: next.tab,
              evidenceId: next.evidenceId,
            })
            replaceHash(strongFlowRouteHash(
              currentRoute,
              scopeSelectionFromHash(browser.location.hash),
              strongFlowHistorySelectionFromHash(browser.location.hash),
            ))
          },
        },
        routeScope: scopeSelectionFromHash(browser.location.hash),
        readOnly: activeRouteReadOnly,
      })
      deliveryList = null
    } catch (error) {
      deliveryList?.close()
      deliveryList = null
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'STRONGFLOW_ROUTE_FAILURE')
    }
  }

  async function renderEnterprise(generation: number): Promise<void> {
    const session = authSession.state.session
    const resolution = currentScopeResolution
    if (
      authSession.state.status !== 'signed-in'
      || session === null
      || resolution?.status !== 'selected'
    ) {
      const unavailable = element(document, 'p', 'wwc-enterprise-context-required')
      unavailable.setAttribute('role', resolution?.status === 'denied' ? 'alert' : 'status')
      unavailable.textContent = authSession.state.status === 'restoring'
        ? 'Restoring enterprise identity and organization access…'
        : resolution?.status === 'denied'
          ? 'The enterprise Scope in this URL is not authorized. Choose another Scope.'
          : resolution?.status === 'selection-required'
            ? 'Choose an authorized Scope to load enterprise management.'
        : 'Sign in to load enterprise management.'
      slot.replaceChildren(unavailable)
      return
    }
    const scope: Scope = resolution.scope
    const loading = element(document, 'p', 'wwc-enterprise-route-loading')
    loading.setAttribute('role', 'status')
    loading.textContent = 'Loading enterprise management…'
    slot.replaceChildren(loading)
    const controller = new AbortController()
    featureController = controller
    try {
      const enterprise = await import('./enterprise-application.js')
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const mounted = await enterprise.mountEnterpriseApplication({
        root: slot,
        client: controlPlane,
        hash: browser.location.hash,
        signal: controller.signal,
        actor: session.actor,
        scope,
        subscriptionId: contractId(
          'sub',
          browser.crypto,
        ) as ControlPlaneWebSocketSubscriptionId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        readOnly: activeRouteReadOnly,
      })
      if (mounted === null) return
      if (closed || generation !== renderGeneration || controller.signal.aborted) {
        mounted.close()
        return
      }
      activeFeature = mounted
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'ENTERPRISE_ROUTE_FAILURE')
    }
  }

  function performRender(scopeSelectorMode: ScopeSelectorRenderMode = 'replace'): void {
    renderGeneration += 1
    const generation = renderGeneration
    featureController?.abort()
    featureController = null
    activeFeature?.close()
    activeFeature = null
    if (scopeSelectorMode === 'replace') {
      scopeSelectorPage?.close()
      scopeSelectorPage = null
    }
    currentScopeResolution = null
    activeSurface = clientSurfaceFromHash(browser.location.hash)
    recordHomeVisit(browser.location.hash)
    // Design pages 07/08: device onboarding and repository lists live on their
    // own pages, never as panels above work surfaces.
    clientsRoot.hidden = true
    repositoriesRoot.hidden = true
    for (const link of links.values()) link.removeAttribute('data-route-access')
    delete slot.dataset.routeAccess
    clearRouteFailure()
    const navigationProjection = updateNavigation()
    const capability = navigationProjection.surfaces.find(entry => (
      entry.surface.id === activeSurface.id
    )) as SurfaceCapability
    activeRouteReadOnly = capability.capability === 'read-only'
    title.textContent = ''
    description.textContent = ''
    readOnlyNotice.hidden = !activeRouteReadOnly
    slot.dataset.winwincodeSurface = activeSurface.id
    slot.dataset.navigationCapability = capability.capability
    slot.replaceChildren()
    const routeAccessDenied = authSession.state.status === 'signed-in'
      && (capability.capability === 'hidden' || capability.capability === 'disabled')
    const session = authSession.state.session
    if (authSession.state.status === 'signed-in' && session !== null) {
      const resolved = resolveScopeContext(
        session.authorizedScopes,
        browser.location.hash,
        activeSurface.id === 'enterprise' ? 'scope' : 'repository',
      )
      const resolution: ScopeContextResolution = resolved.status === 'selected'
        && selectionIdentity(resolved.selection) === revokedScopeIdentity
        ? Object.freeze({
            status: 'denied',
            reason: 'not-authorized',
            selection: resolved.selection,
            options: resolved.options,
          })
        : resolved
      currentScopeResolution = resolution
      if (scopeSelectorPage === null) {
        const model = createScopeSelectorViewModel({
          client: rawControlPlane,
          actor: session.actor,
          authorizedScopes: session.authorizedScopes,
          selection: resolution.selection,
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
          onSelectionChange(nextSelection) {
            if (closed || scopeSelectorPage === null) return
            if (selectionLeavesRevokedScope(nextSelection)) revokedScopeIdentity = null
            replaceHash(scopeHash(browser.location.hash, nextSelection))
            render('preserve')
          },
        })
        scopeSelectorPage = mountScopeSelectorPage({
          root: scopeRoot,
          model,
          contextStatus: resolution.status,
        })
        if (!routeAccessDenied && resolution.status !== 'denied') void model.start()
      } else {
        scopeSelectorPage.updateContextStatus(resolution.status)
      }
      scopeRoot.hidden = false
    } else {
      scopeSelectorPage?.close()
      scopeSelectorPage = null
      scopeRoot.hidden = true
      scopeRoot.replaceChildren()
    }
    readinessRoot.hidden = !(
      authSession.state.status === 'signed-in'
      && session !== null
      && activeSurface.id !== 'enterprise'
    )
    if (!readinessRoot.hidden) {
      const resolution = currentScopeResolution
      const context: ReadinessContext = authSession.state.status !== 'signed-in' || session === null
        ? { status: 'signed-out' }
        : resolution?.status === 'selected' && resolution.scope.kind === 'repository'
          ? { status: 'ready', actor: session.actor, scope: resolution.scope }
          : resolution?.status === 'denied'
            ? { status: 'no-scope', reason: 'denied' }
            : resolution?.status === 'empty'
              ? { status: 'no-scope', reason: 'empty' }
              : { status: 'no-scope', reason: 'selection-required' }
      void readiness.updateContext(context)
    }
    for (const [id, link] of links) {
      if (id === activeSurface.id) link.setAttribute('aria-current', 'page')
      else link.removeAttribute('aria-current')
    }
    if (routeAccessDenied) {
      closeAttentionMonitor()
      routeDenied(capability)
      return
    }
    const monitorContext = currentScopeResolution
    if (
      authSession.state.status === 'signed-in'
      && session !== null
      && monitorContext?.status === 'selected'
      && monitorContext.scope.kind === 'repository'
    ) {
      void ensureAttentionMonitor(generation, session.actor, monitorContext.scope).catch(() => {})
    } else {
      closeAttentionMonitor()
    }
    options.root.dispatchEvent(new CustomEvent('winwincode:surface-change', {
      detail: Object.freeze({
        surface: activeSurface,
        controlPlane,
      }),
    }))
    if (activeSurface.id === 'home') {
      // UI-100.2: the Home surface carries the §16.6 new-task form and the
      // §16.7 run page as sub-routes of the My Work first screen.
      const homeRoute = homeSubRoute()
      if (homeRoute === 'task-entry') {
        launchRoute(renderTaskEntry(generation), generation, 'TASK_ENTRY_ROUTE_FAILURE')
      } else if (homeRoute === 'task-run') {
        launchRoute(renderTaskRun(generation), generation, 'TASK_RUN_ROUTE_FAILURE')
      } else {
        launchRoute(renderHome(generation), generation, 'HOME_ROUTE_FAILURE')
      }
    }
    else if (activeSurface.id === 'chat') launchRoute(renderChat(generation), generation, 'CHAT_ROUTE_FAILURE')
    else if (activeSurface.id === 'extensions') {
      launchRoute(renderExtensions(generation), generation, 'EXTENSIONS_ROUTE_FAILURE')
    } else if (activeSurface.id === 'projects') {
      launchRoute(renderProjects(generation), generation, 'PROJECTS_ROUTE_FAILURE')
    } else if (activeSurface.id === 'device') {
      launchRoute(renderDevice(generation), generation, 'DEVICE_ROUTE_FAILURE')
    } else if (activeSurface.id === 'strongflow') {
      launchRoute(renderStrongFlow(generation), generation, 'STRONGFLOW_ROUTE_FAILURE')
    } else if (activeSurface.id === 'settings') {
      launchRoute(renderSettings(generation), generation, 'SETTINGS_ROUTE_FAILURE')
    } else if (activeSurface.id === 'attention') {
      launchRoute(renderAttention(generation), generation, 'ATTENTION_ROUTE_FAILURE')
    } else if (activeSurface.id === 'enterprise') {
      launchRoute(renderEnterprise(generation), generation, 'ENTERPRISE_ROUTE_FAILURE')
    }
  }

  function launchRoute(
    operation: Promise<void>,
    generation: number,
    fallbackCode: string,
  ): void {
    void operation.catch(error => {
      if (closed || generation !== renderGeneration) return
      showRouteFailure(error, fallbackCode)
    })
  }

  function render(scopeSelectorMode: ScopeSelectorRenderMode = 'replace'): void {
    try {
      performRender(scopeSelectorMode)
    } catch (error) {
      showRouteFailure(error, 'CLIENT_RENDER_FAILURE')
    }
  }

  const onHashChange = () => { render() }
  const onOffline = () => { connection.offline() }
  // Returning to the tab revalidates the badge so it never shows stale counts.
  const onFocus = () => { void attentionMonitor?.refresh().catch(() => {}) }
  const onOnline = () => {
    queryCache.clear('reconnect')
    observedControlPlane.reconnectAll()
  }
  const onWindowError = (event: ErrorEvent) => {
    event.preventDefault()
    showRouteFailure(event.error, 'CLIENT_RENDER_FAILURE')
  }
  const onUnhandledRejection = (event: PromiseRejectionEvent) => {
    event.preventDefault()
    showRouteFailure(event.reason, 'CLIENT_ASYNC_FAILURE')
  }
  browser.addEventListener('hashchange', onHashChange)
  browser.addEventListener('offline', onOffline)
  browser.addEventListener('online', onOnline)
  browser.addEventListener('focus', onFocus)
  browser.addEventListener('error', onWindowError)
  browser.addEventListener('unhandledrejection', onUnhandledRejection)
  const unsubscribeConnection = connection.subscribe(updateReliabilityViews)
  const unsubscribeAuthSession = authSession.subscribe(state => {
    updateLoginVisibility()
    if (state.status === 'authentication-required') {
      connection.authenticationRequired(state.error?.code, state.error?.requestId)
    } else if (state.status === 'signed-in'
      && connection.state.status === 'authentication-required') {
      connection.reset()
      connection.connected()
    } else if (state.status === 'error' && state.error !== null) {
      connection.failure(state.error, browserIsOnline())
    }
    if (state.status === 'signed-in') diagnosticScope()
    updateNavigation()
    if ((state.status === 'signed-in'
      || state.status === 'signed-out'
      || state.status === 'authentication-required'
      || state.status === 'error') && (
      activeSurface.id === 'home'
      || activeSurface.id === 'chat'
      || activeSurface.id === 'projects'
      || activeSurface.id === 'device'
      || activeSurface.id === 'extensions'
      || activeSurface.id === 'strongflow'
      || activeSurface.id === 'settings'
      || activeSurface.id === 'attention'
      || activeSurface.id === 'enterprise'
    )) render()
  })
  render()
  updateLoginVisibility()
  void authSession.restore()

  return {
    controlPlane,
    authSession,
    connection,
    surfaces: CLIENT_SURFACES,
    get activeSurface() {
      return activeSurface
    },
    navigate(surfaceId) {
      if (closed) throw new Error('WinWinCode Client is closed.')
      const surface = CLIENT_SURFACES.find(candidate => candidate.id === surfaceId)
      if (surface === undefined) throw new Error(`Unknown Client surface: ${surfaceId}`)
      browser.location.hash = surfaceHash(
        surface.path,
        scopeSelectionFromHash(browser.location.hash),
      )
      render()
    },
    close() {
      if (closed) return
      closed = true
      browser.removeEventListener('hashchange', onHashChange)
      browser.removeEventListener('offline', onOffline)
      browser.removeEventListener('online', onOnline)
      browser.removeEventListener('focus', onFocus)
      browser.removeEventListener('error', onWindowError)
      browser.removeEventListener('unhandledrejection', onUnhandledRejection)
      unsubscribeAuthSession()
      unsubscribeConnection()
      unsubscribeLoginModel()
      featureController?.abort()
      featureController = null
      activeFeature?.close()
      activeFeature = null
      scopeSelectorPage?.close()
      scopeSelectorPage = null
      currentScopeResolution = null
      closeAttentionMonitor()
      readinessPage.close()
      readiness.close()
      loginPage.close()
      loginModel.close()
      clientsPage.close()
      clientsModel.close()
      occupancyModel.close()
      repositoriesPage.close()
      repositoriesModel.close()
      authPage.close()
      authSession.close()
      accessFailureSession = null
      revokedScopeIdentity = null
      controlPlane.close()
      errorBoundary.close()
      connectionBar.close()
      connection.close()
      options.root.replaceChildren()
    },
  }
}
