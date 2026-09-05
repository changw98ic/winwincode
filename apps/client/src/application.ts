// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  createControlPlaneClient,
  createControlPlaneClientDirectory,
  createControlPlaneClientOccupancy,
  createControlPlaneClientUsers,
  type ControlPlaneClient,
  type ControlPlaneClientTransport,
} from './control-plane-client.js'
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
import { createQueryCache } from './core/query-cache.js'
import {
  resolveScopeContext,
  scopeHash,
  scopeSelectionFromHash,
  surfaceHash,
  type ScopeContextResolution,
  type ScopeRouteSelection,
} from './core/scope-context.js'
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
  prefix: 'req' | 'sub' | 'psn' | 'dlv',
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
  return Object.freeze({
    fetch: nativeFetch.bind(browser) as NonNullable<ControlPlaneClientTransport['fetch']>,
  })
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
  const clientOccupancy = createControlPlaneClientOccupancy({
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
  navigation.setAttribute('aria-label', 'Product areas')
  readOnlyNotice.setAttribute('role', 'status')
  readOnlyNotice.textContent = 'This product area is read-only. Write actions are unavailable, and Server authorization still applies to every request.'
  readOnlyNotice.hidden = true
  // UI-604: the surface slot holds the whole mounted page.  Marking it as a live
  // region queued every realtime DOM change for announcement and nested inside
  // the page's own status regions, so the shell stays silent and each page keeps
  // exactly one polite channel for its own status line.
  skipLink.href = '#wwc-main'
  skipLink.textContent = 'Skip to main content'
  skipLink.addEventListener('click', event => {
    event.preventDefault()
    main.focus()
  })
  main.tabIndex = -1
  main.id = 'wwc-main'

  for (const surface of CLIENT_SURFACES) {
    const link = element(document, 'a', 'wwc-navigation-link')
    link.href = `#${surface.path}`
    link.textContent = surface.label
    link.dataset.surface = surface.id
    link.hidden = true
    link.addEventListener('click', event => {
      if (link.getAttribute('aria-disabled') !== 'true') return
      event.preventDefault()
    })
    links.set(surface.id, link)
    navigation.append(link)
  }

  header.append(skipLink, brand, navigation, authRoot)
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
      return { href: surfaceHash('/settings', selection), label: 'Open Settings' }
    }
    if (item.id === 'server-worker-health' || item.id === 'helper-availability') {
      return {
        href: surfaceHash('/settings/runtime', selection),
        label: 'Open local diagnostics',
      }
    }
    if (item.id === 'first-chat-delivery') {
      return item.reason === 'no-delivery'
        ? {
            href: surfaceHash('/strongflow', selection),
            label: 'Create your first Delivery',
          }
        : { href: surfaceHash('/chat', selection), label: 'Start your first Chat' }
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
        ? `${entry.surface.label} (read only)`
        : entry.capability === 'disabled'
          ? `${entry.surface.label} (unavailable)`
          : entry.surface.label
      if (entry.capability === 'disabled') {
        link.setAttribute('aria-disabled', 'true')
        link.tabIndex = -1
        link.title = `${entry.surface.description}. Not available to the current identity.`
      } else {
        link.removeAttribute('aria-disabled')
        link.tabIndex = 0
        link.title = entry.capability === 'read-only'
          ? `${entry.surface.description}. Read-only access.`
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
    message.textContent = `${capability.surface.label} is not available to the current identity.`
    safeEntry.href = surfaceHash('/chat', scopeSelectionFromHash(browser.location.hash))
    safeEntry.textContent = 'Return to Chat'
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
    scopeRevoked.textContent = 'The current Scope authorization was revoked. Return to a safe entry and restore access.'
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
        ? 'Restoring your signed-in workspace…'
        : resolution?.status === 'denied'
          ? 'The repository Scope in this URL is not authorized. Choose another Scope.'
          : resolution?.status === 'selection-required'
            ? 'Choose an authorized repository Scope to open this workspace.'
            : authSession.state.status === 'signed-in' && session !== null
              ? 'Your account does not have access to a repository workspace.'
          : 'Sign in to open this workspace.'
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
    routeLoading('Loading Chat…')
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
        subscriptionId: contractId(
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

  async function renderSettings(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const controller = new AbortController()
    featureController = controller
    const operationsRoute = browser.location.hash
      .replace(/^#/u, '')
      .replace(/\?.*$/u, '') === '/settings/runtime'
    routeLoading(operationsRoute ? 'Loading local operations…' : 'Loading Settings…')
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
    routeLoading('Loading Home…')
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

  async function renderAttention(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const parameters = routeParameters(browser.location.hash)
    const productSessionId = parameters.get('session') as ProductSessionId | null
    if (productSessionId !== null) {
      const controller = new AbortController()
      featureController = controller
      routeLoading('Loading session decisions…')
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
    routeLoading('Loading the Attention Center…')
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
    routeLoading('Loading StrongFlow…')
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
    for (const link of links.values()) link.removeAttribute('data-route-access')
    delete slot.dataset.routeAccess
    clearRouteFailure()
    const navigationProjection = updateNavigation()
    const capability = navigationProjection.surfaces.find(entry => (
      entry.surface.id === activeSurface.id
    )) as SurfaceCapability
    activeRouteReadOnly = capability.capability === 'read-only'
    title.textContent = activeSurface.label
    description.textContent = activeSurface.description
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
    if (activeSurface.id === 'home') launchRoute(renderHome(generation), generation, 'HOME_ROUTE_FAILURE')
    else if (activeSurface.id === 'chat') launchRoute(renderChat(generation), generation, 'CHAT_ROUTE_FAILURE')
    else if (activeSurface.id === 'strongflow') {
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
