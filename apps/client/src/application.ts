// SPDX-License-Identifier: Apache-2.0
//
// Community product boundary: WinWinCode Community is a single local Owner
// product. This application graph mounts DSH, StrongFlow, local Owner
// login/session, Device Client, and Repository surfaces only. Cloud/Enterprise
// management routes and organization/member administration navigation are not
// Community product surface. Scope query keys that still carry organizationId
// use LocalDefaultScope singleton values.

import { mountPreferences } from './preferences.js'
import { REPOSITORY_NAME_CHANGED_EVENT } from './display-labels.js'

import {
  ControlPlaneClientError,
  createControlPlaneClient,
  createControlPlaneClientCandidates,
  createControlPlaneClientDirectory,
  createControlPlaneRepositoryRegistration,
  createControlPlaneManagedAppTemplatePort,
  createControlPlaneTaskPort,
  createControlPlaneTaskFake,
  createControlPlaneWorkerSessionPort,
  createControlPlaneRunIdentityPort,
  createControlPlaneRunIdentityFake,
  queryWorkRunAggregate,
  queryDeliveryDetail,
  type ControlPlaneClient,
  type ControlPlaneClientTransport,
  type ControlPlaneTaskAnchor,
} from './community-control-plane-client.js'
import { QueryName, type WorkItemId } from './generated/contracts.js'
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
import { mountSessionBrowser } from './dsh-ui.js'
import { CommandName } from './generated/contracts.js'
import type { ProductSessionProjection, OpaqueCursor } from './generated/contracts.js'
import type {
  CollaborationPageAnnotationProjection,
  CollaborationPageAnnotationSubmitCompletedResponse,
} from './generated/contracts.js'
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
import { matchesCanonicalSchema } from './generated/control-plane-client.js'
import { browserHomeVisitStorage, createHomeRecentVisitStore, homeDeliveryVisitFromHash, type HomeRecentVisitStore } from './home-recent-visits.js'
import { mountClientsPage, type ClientsPage } from './clients-page.js'
import { mountOnboardingPage, type OnboardingPage } from './onboarding-page.js'
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
import type {
  ControlPlaneWebSocketSubscriptionId,
  DeliveryId,
  ProductSessionId,
  RepositoryScope,
  RequestId,
} from './generated/contracts.js'
import {
  projectionForSession,
  surfaceCapabilityForHash,
  type NavigationCapabilityFacts,
  type NavigationCapabilityProjection,
  type SurfaceCapability,
} from './navigation-capability.js'
import type {
  AttentionNotificationMonitor,
} from './attention-notifications.js'

export interface WinWinCodeClientApplicationOptions {
  readonly serverUrl: string
  readonly root: HTMLElement
  readonly window?: Window
  /** Canonical facade injection used by deterministic browser fixtures and host composition. */
  readonly controlPlane?: ControlPlaneClient
  /** Secret-safe clipboard seam for browser hosts and deterministic fixtures. */
  readonly copyText?: (value: string) => Promise<void> | void
  /** Session history renderer seam for hosts with a non-DOM test document. */
  readonly mountSessionBrowser?: (root: HTMLElement) => ReturnType<typeof mountSessionBrowser>
  readonly now?: () => string
  /** Server/deployment facts projected into navigation presentation only. */
  readonly navigationCapabilities?: Readonly<NavigationCapabilityFacts>
  /** 假优先任务锚点种子(演示 fixture 用,设计稿 05 的任务详情内容)。 */
  readonly taskSeed?: readonly ControlPlaneTaskAnchor[]
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
  prefix: 'req' | 'sub' | 'psn' | 'dlv' | 'wit' | 'crt',
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
  const closePreferences = mountPreferences(browser, document)
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
  const workerSessions = createControlPlaneWorkerSessionPort({
    client: rawControlPlane,
    transport: browserTransport,
  })
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
  const clientsModel: ClientsViewModel = createClientsViewModel({
    client: clientDirectory,
  })
  const clientCandidates = createControlPlaneClientCandidates({
    client: rawControlPlane,
    transport: browserTransport,
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
  // REPO-100.3: the repository list talks to the same directory facade; the
  // list is a Server snapshot read, so expected failures stay inside the area.
  const repositoriesModel: RepositoriesViewModel = createRepositoriesViewModel({
    client: clientDirectory,
  })
  // UI-504: 浏览器本地的最近打开交付历史,看板以「最近访问」折叠行渲染。
  const homeVisits: HomeRecentVisitStore = createHomeRecentVisitStore({
    storage: browserHomeVisitStorage(browser),
  })

  /** 记录一次交付访问;非交付深链路由不写入。 */
  function recordHomeVisit(hash: string): void {
    const path = hash.slice(0, hash.indexOf('?') < 0 ? hash.length : hash.indexOf('?'))
    if (path !== '#/home/task-run') return
    const deliveryId = routeParameters(hash).get('delivery') as import('./generated/contracts.js').DeliveryId
    if (deliveryId === null) return
    homeVisits.record(deliveryId, scopeSelectionFromHash(hash), Date.now())
  }

  const taskPort = options.taskSeed === undefined
    ? createControlPlaneTaskPort({
        client: controlPlane,
        actor: () => authenticatedRouteContext()?.actor ?? null,
        scope: () => authenticatedRouteContext()?.scope ?? null,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        nextWorkItemId: () => contractId('wit', browser.crypto) as WorkItemId,
        workerSessions,
        async deliveryContext() {
          const context = authenticatedRouteContext()
          const deliveryId = routeParameters(browser.location.hash).get('delivery') as DeliveryId | null
          if (context === null || deliveryId === null) return null
          const aggregate = await queryWorkRunAggregate(controlPlane, {
            actor: context.actor,
            scope: context.scope,
            requestId: contractId('req', browser.crypto) as RequestId,
            workItemId: null,
            deliveryId,
          })
          const criterion = aggregate.contract.criteria.find(candidate => candidate.required)
            ?? aggregate.contract.criteria[0]
          if (criterion === undefined) return null
          return {
            deliveryId,
            expectedRevision: aggregate.readCursor.deliveryRevision,
            contractRevision: aggregate.contract.revision,
            criterionId: criterion.id,
          }
        },
      })
    : createControlPlaneTaskFake({
        nextTaskId: () => contractId('wit', browser.crypto) as WorkItemId,
        seed: options.taskSeed,
      })
  const fixtureRunIdentity = options.taskSeed === undefined
    ? null
    : createControlPlaneRunIdentityFake()
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
  // Community binds one repository at Server startup; there is no multi-tenant
  // Scope switcher. Notices for revoked/empty scope use a plain alert region.
  const scopeNotice = element(document, 'p', 'wwc-scope-notice')
  const readinessRoot = element(document, 'div', 'wwc-readiness-root')
  // The shell supplies the accessible page title for every routed surface.
  const title = element(document, 'h1', 'wwc-surface-title')
  const description = element(document, 'p', 'wwc-surface-description')
  const readOnlyNotice = element(document, 'p', 'wwc-surface-read-only')
  const slot = element(document, 'section', 'wwc-surface-slot')
  const links = new Map<ClientSurfaceId, HTMLAnchorElement>()
  let activeSurface = clientSurfaceFromHash(browser.location.hash)
  let activeFeature: MountedClientFeature | null = null
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
  function selectionIdentity(selection: ScopeRouteSelection): string {
    return [
      selection.organizationId ?? '',
      selection.workspaceId ?? '',
      selection.projectId ?? '',
      selection.repositoryId ?? '',
    ].join('\u0000')
  }

  function diagnosticScope(): unknown {
    const session = authSession.state.session
    if (session === null) return lastKnownDiagnosticScope
    const resolution = resolveScopeContext(
      session.authorizedScopes,
      browser.location.hash,
      'repository',
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
    if (clipboard === undefined) return Promise.reject(new Error('剪贴板不可用。'))
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
    chat: '<path d="M11 4H5a1 1 0 0 0-1 1v13a1 1 0 0 0 1 1h13a1 1 0 0 0 1-1v-6"/><path d="M17.5 3.5a2.1 2.1 0 0 1 3 3L12 15l-4 1 1-4Z"/>',
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
    link.setAttribute('aria-label', surface.label)
    link.hidden = true
    link.addEventListener('click', event => {
      if (link.getAttribute('aria-disabled') !== 'true') return
      event.preventDefault()
    })
    links.set(surface.id, link)
    navigation.append(link)
  }
  // Design sidebar footer: 设置 sits in the footer under 「Client 已连接」,
  // not inside the top navigation group.
  connectionBar.root.append(links.get('settings')!)

  const recentChatsRoot = element(document, 'div', 'wwc-recent-chats')
  navigation.id = 'wwc-navigation'
  recentChatsRoot.id = 'wwc-recent-chats'
  const navigationToggle = element(document, 'button', 'wwc-navigation-toggle')
  navigationToggle.type = 'button'
  navigationToggle.textContent = '导航'
  navigationToggle.setAttribute('aria-label', '导航与历史')
  navigationToggle.setAttribute('aria-controls', `${navigation.id} ${recentChatsRoot.id}`)
  navigationToggle.setAttribute('aria-expanded', 'false')
  navigationToggle.addEventListener('click', () => {
    const expanded = header.classList.toggle('wwc-header-expanded')
    navigationToggle.setAttribute('aria-expanded', String(expanded))
  })
  const sessionBrowser = (options.mountSessionBrowser ?? mountSessionBrowser)(recentChatsRoot)
  let historySessions: readonly ProductSessionProjection[] = []
  let historyScope = ''
  let historyLoading = false
  let historyError = ''
  let historyGeneration = 0
  function renderRecentChats(): void {
    sessionBrowser.update({
      sessions: historySessions, loading: historyLoading, error: historyError,
      currentId: routeParameters(browser.location.hash).get('session'),
      href: session => {
        const params = new URLSearchParams({ session: session.id })
        if (session.deviceContext !== undefined) {
          params.set('client', session.deviceContext.clientId)
          params.set('repositoryBinding', session.deviceContext.repositoryBindingId)
        }
        return surfaceHash(`/chat?${params}`, scopeSelectionFromHash(browser.location.hash))
      },
      refresh: () => { void refreshRecentChats(true) },
      async update(session, title, archived) {
        const context = authenticatedRouteContext()
        if (context === null) throw new Error('请先登录')
        await controlPlane.command({
          schemaVersion: 'winwincode/v1', ...context,
          requestId: contractId('req', browser.crypto) as RequestId,
          command: CommandName.SessionUpdate, expectedRevision: session.revision,
          payload: { productSessionId: session.id, title, archived },
        })
        await refreshRecentChats(true)
      },
    })
  }
  async function refreshRecentChats(force = false): Promise<void> {
    const session = authSession.state.session
    const resolution = currentScopeResolution
    const scope = resolution?.status === 'selected' && resolution.scope.kind === 'repository' ? resolution.scope : null
    if (session === null || scope === null) {
      historyGeneration += 1; historySessions = []; historyScope = ''; historyLoading = false
      recentChatsRoot.hidden = true
      renderRecentChats()
      return
    }
    recentChatsRoot.hidden = false
    const identity = JSON.stringify([session.actor, scope])
    if (identity === historyScope && (!force || historyLoading)) { renderRecentChats(); return }
    if (identity !== historyScope) historySessions = []
    historyScope = identity
    const own = ++historyGeneration
    historyLoading = true; historyError = ''; renderRecentChats()
    try {
      const rows: ProductSessionProjection[] = []
      let cursor: OpaqueCursor | null = null
      do {
        const response = await controlPlane.query({
          schemaVersion: 'winwincode/v1', actor: session.actor, scope,
          requestId: contractId('req', browser.crypto) as RequestId,
          query: QueryName.SessionList, parameters: { states: [] }, page: { cursor, limit: 200 },
        })
        if (response.query !== QueryName.SessionList) throw new Error('对话读取失败')
        rows.push(...response.result.items)
        cursor = response.page.nextCursor
      } while (cursor !== null && own === historyGeneration)
      if (own === historyGeneration) historySessions = rows
    } catch { if (own === historyGeneration) historyError = '读取对话失败，请点击刷新。' }
    finally { if (own === historyGeneration) { historyLoading = false; renderRecentChats() } }
  }
  const historyTimer = browser.setInterval(() => { void refreshRecentChats(true) }, 30_000)
  browser.addEventListener(REPOSITORY_NAME_CHANGED_EVENT, renderRecentChats)


  header.append(skipLink, brand, navigationToggle, navigation, recentChatsRoot, authRoot, connectionBar.root)
  authRoot.hidden = true
  main.append(
    scopeNotice,
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
  shell.append(header, main)
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
        href: surfaceHash('/settings', selection),
        label: '打开运行诊断',
      }
    }
    if (item.id === 'first-chat-delivery') {
      return item.reason === 'no-delivery'
        ? {
            href: surfaceHash('/home/new-task', selection),
            label: '创建你的第一个任务',
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
    const linkText = new Map<HTMLAnchorElement, HTMLElement>()
    for (const entry of projection.surfaces) {
      const link = links.get(entry.surface.id)
      if (link === undefined) continue
      link.href = surfaceHash(entry.surface.path, scopeSelectionFromHash(browser.location.hash))
      link.dataset.capability = entry.capability
      link.hidden = entry.capability === 'hidden'
      // The icon span from mount time must survive label updates, so only the
      // dedicated text node is rewritten here.
      let text = linkText.get(link)
      if (text === undefined) {
        text = link.lastElementChild as HTMLElement | null ?? link
        linkText.set(link, text)
      }
      text.textContent = entry.capability === 'read-only'
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
    // The 设置 link lives in the sidebar footer (moved at mount), so the
    // navigation group rebuild keeps only the top group.
    navigation.replaceChildren(...visible.filter(link => link.dataset.surface !== 'settings'))
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
    currentScopeResolution = null
    closeAttentionMonitor()
    scopeNotice.setAttribute('role', 'alert')
    scopeNotice.textContent = '此范围的授权已被撤销。请返回安全入口并恢复访问。'
    scopeNotice.hidden = false
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
    const badgeTarget = links.get('home')
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
      let selectedClientId = parameters.get('client')
      let selectedBindingId = parameters.get('repositoryBinding')
      if (productSessionId !== null) {
        const session = await controlPlane.query({
          schemaVersion: 'winwincode/v1', requestId: contractId('req', browser.crypto) as RequestId,
          actor: context.actor, scope: context.scope, query: QueryName.SessionGet,
          parameters: { productSessionId }, page: { cursor: null, limit: 1 },
        }, { signal: controller.signal }) as import('./generated/contracts.js').SessionGetResultResponse
        if (closed || generation !== renderGeneration || controller.signal.aborted) return
        if (session.result.deviceContext !== undefined) {
          selectedClientId = session.result.deviceContext.clientId
          selectedBindingId = session.result.deviceContext.repositoryBindingId
        } else if (session.result.state !== 'idle') {
          selectedClientId = null
          selectedBindingId = null
        }
      }
      let project: import('./chat-page.js').ChatPageOptions['project']
      if (selectedClientId !== null || selectedBindingId !== null) {
        if (selectedClientId === null || selectedBindingId === null) {
          routeUnavailable('项目链接缺少设备或仓库信息，请从项目页重新打开。')
          return
        }
        const [devices, repositories] = await Promise.all([
          clientDirectory.listClients({ signal: controller.signal }),
          clientDirectory.listRepositories({ clientId: selectedClientId }, { signal: controller.signal }),
        ])
        if (closed || generation !== renderGeneration || controller.signal.aborted) return
        const device = devices.find(item => item.clientId === selectedClientId)
        const repository = repositories.find(item => item.repositoryBindingId === selectedBindingId)
        if (device === undefined || repository === undefined) {
          routeUnavailable('该设备或项目已不可用，请从项目页重新选择。')
          return
        }
        project = { clientId: selectedClientId, deviceName: device.displayName, repository }
      }
      const [
        { createChatViewModel },
        { mountChatPage },
        { createChatDeliveryCreator },
        { createHomeDeliveryListViewModel },
      ] = await Promise.all([
        import('./chat-view-model.js'),
        import('./chat-page.js'),
        import('./chat-delivery-creator.js'),
        import('./home-dashboard-view-model.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const model = createChatViewModel({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        productSessionId,
        ...(project === undefined ? {} : { clientId: project.clientId }),
        async launchDeviceSession(input) {
          await clientOccupancy.claim({ clientId: input.clientId })
          await workerSessions.launch({ clientId: input.clientId,
            repositoryBindingId: input.repositoryBindingId, productSession: { id: input.productSessionId, scope: context.scope } })
        },
        nextSubscriptionId: () => contractId(
          'sub',
          browser.crypto,
        ) as ControlPlaneWebSocketSubscriptionId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        onActiveSessionChange(nextProductSessionId) {
          if (closed || generation !== renderGeneration || controller.signal.aborted) return
          const parameters = routeParameters(browser.location.hash)
          parameters.set('session', nextProductSessionId)
          replaceHash(`#/chat?${parameters.toString()}`)
        },
      })
      // Design page 03b: a confirmed draft becomes the first Delivery; the
      // receipt line inside the conversation carries the follow-up link, so
      // creation itself never navigates away from the session.
      const deliveryCreator = createChatDeliveryCreator({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        nextDeliveryId: () => contractId('dlv', browser.crypto) as DeliveryId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        onCreated() {
          // The page re-renders from the creator subscription; no navigation.
        },
      })
      activeFeature = mountChatPage({
        root: slot,
        model,
        listDeviceRepositories: clientId => clientDirectory.listRepositories({ clientId }),
        ...(project === undefined ? {} : { project }),
        onProjectChange(clientId, repositoryBindingId) {
          const parameters = routeParameters(browser.location.hash)
          parameters.set('client', clientId)
          parameters.set('repositoryBinding', repositoryBindingId)
          replaceHash(`#/chat?${parameters.toString()}`)
        },
        deliveryCreator,
        deliveries: createHomeDeliveryListViewModel({
          client: controlPlane, actor: context.actor, scope: context.scope,
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        }),
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
      let lastHistoryRevision = ''
      const unsubscribeRecentChats = model.subscribe(state => {
        const session = state.session
        if (session === null) return
        const identity = `${session.id}:${session.revision}`
        if (lastHistoryRevision === identity) return
        lastHistoryRevision = identity
        historySessions = [session, ...historySessions.filter(item => item.id !== session.id)]
        renderRecentChats()
        void refreshRecentChats(true)
      })
      const chatPage = activeFeature
      activeFeature = {
        close() {
          unsubscribeRecentChats()
          chatPage?.close()
        },
      }
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
        registration: createControlPlaneRepositoryRegistration({ serverUrl: controlPlane.serverUrl, transport: browserTransport }),
        managedAppTemplate: createControlPlaneManagedAppTemplatePort({ serverUrl: controlPlane.serverUrl, transport: browserTransport }),
        newChatHref: (clientId, repository) => scopeHash(`#/chat?${new URLSearchParams({
          client: clientId,
          repositoryBinding: repository.repositoryBindingId,
        }).toString()}`, scopeSelectionFromHash(browser.location.hash)),
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
        homeHref: '#/onboarding',
        projectsHref: '#/projects',
        requestOptions: () => undefined,
      })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'DEVICE_ROUTE_FAILURE')
    }
  }

  /** Device-owned extension configuration and confirmed execution capabilities. */
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
      activeFeature = mountExtensionsPage({ root: slot, serverUrl: controlPlane.serverUrl, fetch: browserTransport.fetch })
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
    routeLoading('正在加载设置…')
    try {
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
      const page = mountSettingsPage({
        root: slot,
        serverUrl: controlPlane.serverUrl,
        fetch: browserTransport.fetch,
        model,
        readOnly: activeRouteReadOnly,
        // Design page 15: the 用量 tab lazily mounts the live Usage/Provider/
        // Worker health summary, so the settings route only pays for the usage
        // projection when the tab is opened.
        mountUsagePanel: async usageRoot => {
          const [{ createUsageHealthViewModel }, { mountUsageHealthSummary }] = await Promise.all([
            import('./usage-health-view-model.js'),
            import('./usage-health-page.js'),
          ])
          const healthModel = createUsageHealthViewModel({
            client: controlPlane,
            actor: context.actor,
            scope: context.scope,
            nextRequestId: () => contractId('req', browser.crypto) as RequestId,
          })
          const healthSummary = mountUsageHealthSummary({
            root: usageRoot,
            model: healthModel,
          })
          void healthModel.start().catch(() => {})
          return {
            close() {
              healthSummary.close()
              healthModel.close()
            },
          }
        },
      })
      activeFeature = {
        close() {
          page.close()
          model.close()
        },
      }
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'SETTINGS_ROUTE_FAILURE')
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
        visits: homeVisits,
        // The Clients zone reuses the one shell-owned Clients area model, so
        // the first screen never grows a second device-list state.
        clients: clientsModel,
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
  function homeSubRoute(): 'my-work' | 'task-entry' | 'task-run' | 'review' | 'candidate-preview' | 'clarification' {
    const path = browser.location.hash.replace(/^#/u, '').replace(/\?.*$/u, '')
    if (path === '/home/new-task') return 'task-entry'
    // `/home/run` is the canonical sub-route; `/home/task-run` is the same
    // task-detail page under its design-page name.
    if (path === '/home/run' || path === '/home/task-run') return 'task-run'
    if (path === '/home/preview') return 'candidate-preview'
    if (path === '/home/clarify') return 'clarification'
    if (path === '/home/review') return 'review'
    return 'my-work'
  }

  /** The anchor facts a deep-linked run route must carry to be actionable. */
  function taskRunRouteAnchor(): {
    readonly taskId: WorkItemId
    readonly deliveryId: DeliveryId
  } | null {
    const parameters = routeParameters(browser.location.hash)
    const taskId = parameters.get('task')
    const deliveryId = parameters.get('delivery')
    if (
      taskId === null || !matchesCanonicalSchema('WorkItemId', taskId)
      || deliveryId === null || !matchesCanonicalSchema('DeliveryId', deliveryId)
    ) {
      return null
    }
    return { taskId: taskId as WorkItemId, deliveryId: deliveryId as DeliveryId }
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
          })
          const deliveryId = routeParameters(browser.location.hash).get('delivery')
          if (deliveryId !== null) parameters.set('delivery', deliveryId)
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
   * UI-100.2: the §16.7 run page. Client, Occupancy, and Repository rows are
   * live shell facts; execution identity comes only from WorkRun.
   */
  async function renderTaskRun(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const anchorFacts = taskRunRouteAnchor()
    if (anchorFacts === null) {
      routeUnavailable('任务链接不完整。请从任务看板发起新任务，或在对话中委托任务。')
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
        taskId: anchorFacts.taskId,
        clientId: '',
        repositoryBindingId: '',
        baseBranch: '',
        description: '',
        modelRouteId: '',
      })
      const model = createTaskRunViewModel({
        anchor,
        taskDescription: knownAnchor?.description ?? null,
        clients: clientsModel,
        repositories: repositoriesModel,
        identity: fixtureRunIdentity ?? createControlPlaneRunIdentityPort({
          client: controlPlane,
          candidates: clientCandidates,
          actor: () => context.actor,
          scope: () => context.scope,
          deliveryId: () => anchorFacts.deliveryId,
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        }),
        candidatePreviewHref: () => scopeHash(
          `#/home/preview?${new URLSearchParams({
            delivery: anchorFacts.deliveryId,
            task: anchorFacts.taskId,
          }).toString()}`,
          scopeSelectionFromHash(browser.location.hash),
        ),
      })
      activeFeature = mountTaskRunPage({
        root: slot,
        model,
        homeHref: surfaceHash('/home', scopeSelectionFromHash(browser.location.hash)),
        reviewHref: scopeHash(
          `#/home/review?delivery=${encodeURIComponent(anchorFacts.deliveryId)}`,
          scopeSelectionFromHash(browser.location.hash),
        ),
        clarificationHref: scopeHash(
          `#/home/clarify?delivery=${encodeURIComponent(anchorFacts.deliveryId)}`,
          scopeSelectionFromHash(browser.location.hash),
        ),
        ...(knownAnchor === null ? {} : { anchor: knownAnchor }),
      })
      await model.start()
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'TASK_RUN_ROUTE_FAILURE')
    }
  }

  /** Edit the current Delivery Spec through the canonical delivery contract. */
  async function renderDecisionClarification(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const deliveryId = routeParameters(browser.location.hash).get('delivery')
    if (deliveryId === null || !matchesCanonicalSchema('DeliveryId', deliveryId)) {
      routeUnavailable('需求编辑链接缺少有效的交付标识。请从任务页面重新打开。')
      return
    }
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载需求与验收…')
    try {
      const [{ createClarificationViewModel, createControlPlaneClarificationPort }, { mountDecisionClarificationPage }] =
        await Promise.all([
          import('./decision-clarification-view-model.js'),
          import('./decision-clarification-page.js'),
        ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const model = createClarificationViewModel({
        port: createControlPlaneClarificationPort({
          client: controlPlane,
          actor: context.actor,
          scope: context.scope,
          deliveryId: deliveryId as DeliveryId,
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        }),
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        nextCriterionId: () => contractId('crt', browser.crypto),
      })
      activeFeature = mountDecisionClarificationPage({
        root: slot,
        model,
        backHref: surfaceHash('/home', scopeSelectionFromHash(browser.location.hash)),
        window: browser,
      })
      await model.start()
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'DECISION_CLARIFICATION_ROUTE_FAILURE')
    }
  }

  /**
   * WWX-RUN-04: the candidate run preview main surface under `#/home/preview`.
   * The preview source comes from the canonical WorkRun projection; query
   * parameters only identify the Delivery and WorkItem anchor.
   */
  async function renderCandidateRunPreview(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const parameters = routeParameters(browser.location.hash)
    const allowedParameters = new Set([
      'delivery',
      'task',
      'organizationId',
      'workspaceId',
      'projectId',
      'repositoryId',
    ])
    for (const key of parameters.keys()) {
      if (!allowedParameters.has(key)) {
        routeUnavailable('预览链接只接受交付和任务锚点，请从运行中的任务重新打开。')
        return
      }
    }
    const taskId = parameters.get('task')
    const deliveryId = parameters.get('delivery')
    if (
      taskId === null || !matchesCanonicalSchema('WorkItemId', taskId)
      || deliveryId === null || !matchesCanonicalSchema('DeliveryId', deliveryId)
    ) {
      routeUnavailable('预览链接缺少有效的交付或任务锚点。请从运行中的任务重新打开。')
      return
    }
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载候选运行预览…')
    try {
      const [
        { createCandidatePreviewViewModel },
        { mountCandidateRunPreviewPage },
        { candidatePreviewFileContextFromIdentity, createControlPlaneCandidatePreviewPort },
        { createPageAnnotationViewModel },
      ] = await Promise.all([
        import('./candidate-run-preview-view-model.js'),
        import('./candidate-run-preview-page.js'),
        import('./candidate-run-preview-control-plane.js'),
        import('./page-annotation-view-model.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const knownAnchor = taskPort.describe(taskId as WorkItemId)
      const anchor: ControlPlaneTaskAnchor = knownAnchor ?? Object.freeze({
        taskId: taskId as WorkItemId,
        clientId: '',
        repositoryBindingId: '',
        baseBranch: '',
        description: '',
        modelRouteId: '',
      })
      const identityPort = createControlPlaneRunIdentityPort({
        client: controlPlane,
        candidates: clientCandidates,
        actor: () => context.actor,
        scope: () => context.scope,
        deliveryId: () => deliveryId as DeliveryId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
      })
      const projection = await identityPort.read(anchor)
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      if (projection.candidate === null) {
        routeUnavailable('当前任务没有可预览的候选结果。请等待候选生成后重试。')
        return
      }
      const fileContext = candidatePreviewFileContextFromIdentity(projection)
      if (fileContext === null) {
        routeUnavailable('当前候选缺少规范文件读取上下文，暂不能打开预览。')
        return
      }
      const previewRun = projection.managedAppSourceRun ?? projection.workRun
      const identity = Object.freeze({
        mode: 'frozen-candidate' as const,
        clientId: projection.clientId,
        sourceId: previewRun.executionJobId,
        workRunId: previewRun.id,
        repositoryBindingId: projection.repositoryBindingId,
        taskId: projection.taskId,
        attempt: previewRun.attempt,
        candidateCommit: projection.candidate.candidateCommitId,
        candidateTreeId: projection.candidate.candidateTreeId,
        runConfigVersion: projection.managedAppRunConfig?.schemaVersion ?? null,
      })
      const model = createCandidatePreviewViewModel({
        port: createControlPlaneCandidatePreviewPort({
          serverUrl: controlPlane.serverUrl,
          fetch: browserTransport.fetch,
          identity,
          client: controlPlane,
          actor: context.actor,
          scope: context.scope,
          fileContext,
          runIdentityProjection: () => identityPort.read(anchor),
          occupancyStatus: clientId => clientOccupancy.getStatus({ clientId }),
          ...(projection.managedAppRunConfig === null
            ? {}
            : { managedAppRunConfig: projection.managedAppRunConfig }),
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        }),
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
      })
      const routeContext = context
      function persistedAnnotation(item: CollaborationPageAnnotationProjection) {
        const targetElement = item.target.element ?? null
        return {
          id: item.id,
          body: item.body,
          candidateRef: item.candidate.candidateRef,
          workRunId: item.candidate.workRunId,
          target: {
            pagePath: item.target.pagePath,
            viewport: item.target.viewport,
            element: targetElement === null ? null : {
              tagName: targetElement.tagName,
              role: targetElement.role ?? null,
              accessibleName: targetElement.accessibleName ?? null,
              locator: targetElement.locator,
              bounds: {
                x: targetElement.x,
                y: targetElement.y,
                width: targetElement.width,
                height: targetElement.height,
              },
            },
            region: item.target.region,
          },
          updatedAt: new Date(item.updatedAt).toISOString(),
        }
      }
      async function annotationFacts() {
        const current = await identityPort.read(anchor)
        if (current.deliveryId !== deliveryId || current.candidate === null || current.contract === null) {
          throw new Error('当前候选身份已变化，请刷新预览后重试。')
        }
        const delivery = await queryDeliveryDetail(controlPlane, {
          actor: routeContext.actor,
          scope: routeContext.scope,
          deliveryId: deliveryId as DeliveryId,
          requestId: contractId('req', browser.crypto) as RequestId,
        })
        const attention = delivery.attention.find(item =>
          item.status === 'open' && item.workRunId === current.workRun.id)
        if (attention === undefined) throw new Error('当前任务没有可接收页面批注的处理项。')
        const contract = current.contract
        const candidate = current.candidate
        const evidence = current.evidence.find(item =>
          item.workRunId === current.workRun.id && item.candidateRef === candidate.candidateRef)
        const candidateDigest = candidate.candidateRef.replace(/^git-candidate:/u, '')
        return { current, contract, candidate, attention, evidence, candidateDigest }
      }
      const annotationTransport = {
        async list(input: { readonly deliveryId: string }) {
          const items: CollaborationPageAnnotationProjection[] = []
          let cursor: OpaqueCursor | null = null
          let catalogRevision = 0
          do {
            const response = await controlPlane.query({
              schemaVersion: 'winwincode/v1',
              actor: routeContext.actor,
              scope: routeContext.scope,
              requestId: contractId('req', browser.crypto) as RequestId,
              query: QueryName.CollaborationPageAnnotationList,
              parameters: { deliveryId: input.deliveryId as DeliveryId },
              page: { cursor, limit: 200 },
            })
            if (response.query !== QueryName.CollaborationPageAnnotationList) throw new Error('页面批注读取失败。')
            items.push(...response.result.items)
            catalogRevision = Number(response.result.catalogRevision)
            cursor = response.page.nextCursor
          } while (cursor !== null)
          return {
            items: items.map(persistedAnnotation),
            catalogRevision,
          }
        },
        async submit(input: { readonly draft: import('./page-annotation-view-model.js').PageAnnotationDraft; readonly expectedRevision: number }) {
          const { current, contract, candidate, attention, evidence, candidateDigest } = await annotationFacts()
          const draft = input.draft
          const target = {
            pagePath: draft.pagePath,
            viewport: draft.viewport,
            element: draft.element === null ? null : {
              tagName: draft.element.tagName,
              role: draft.element.role,
              accessibleName: draft.element.accessibleName,
              locator: draft.element.locator,
              x: draft.element.bounds.x,
              y: draft.element.bounds.y,
              width: draft.element.bounds.width,
              height: draft.element.bounds.height,
            },
            region: draft.region ?? {
              x: 0,
              y: 0,
              width: draft.viewport.width,
              height: draft.viewport.height,
            },
          }
          const response = await controlPlane.command({
            schemaVersion: 'winwincode/v1',
            actor: routeContext.actor,
            scope: routeContext.scope,
            requestId: contractId('req', browser.crypto) as RequestId,
            command: CommandName.CollaborationPageAnnotationSubmit,
            expectedRevision: input.expectedRevision,
            payload: {
              annotationId: draft.id,
              itemId: { id: attention.id, kind: 'delivery_attention' },
              candidate: {
                deliveryId: deliveryId as DeliveryId,
                deliverySpecId: contract.id,
                deliverySpecRevision: Number(contract.revision),
                candidateRef: candidate.candidateRef,
                candidateDigest,
                candidateTreeId: candidate.candidateTreeId,
                diffSha256: candidate.diffSha256,
                workRunId: current.workRun.id,
                attempt: current.workRun.attempt,
                sessionBindingId: evidence?.sessionBindingId ?? current.workRun.workerSessionId,
              },
              target,
              body: draft.comment,
              screenshotArtifact: null,
            },
          })
          if (response.command !== CommandName.CollaborationPageAnnotationSubmit
            || !('result' in response)) throw new Error('页面批注提交失败。')
          const completed = response as unknown as CollaborationPageAnnotationSubmitCompletedResponse
          return {
            annotation: persistedAnnotation(completed.result.annotation),
            catalogRevision: Number(completed.result.catalogRevision),
          }
        },
      }
      const annotations = createPageAnnotationViewModel({
        appOrigin: browser.location.origin,
        transport: annotationTransport,
      })
      const page = mountCandidateRunPreviewPage({
        root: slot,
        model,
          annotations,
        appOrigin: browser.location.origin,
        devicePixelRatio: browser.devicePixelRatio,
        deliveryId,
        taskHref: scopeHash(
          `#/home/run?${new URLSearchParams({
            delivery: deliveryId,
            task: taskId,
          }).toString()}`,
          scopeSelectionFromHash(browser.location.hash),
        ),
      })
      activeFeature = { close() { page.close(); annotations.close(); model.close() } }
      await model.start()
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'CANDIDATE_PREVIEW_ROUTE_FAILURE')
    }
  }

  /** Authorized artifacts plus the Controller-owned solution-review decision. */
  async function renderReview(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const deliveryId = routeParameters(browser.location.hash).get('delivery')
    if (deliveryId === null || !matchesCanonicalSchema('DeliveryId', deliveryId)) {
      routeUnavailable('审核链接缺少有效的交付标识。请从任务看板或对话中打开审核。')
      return
    }
    const controller = new AbortController()
    featureController = controller
    routeLoading('正在加载审核面板…')
    try {
      const [{ createStrongFlowReviewViewModel }, { mountStrongFlowReviewDetail }, { createStrongFlowReviewAnnotations }] =
        await Promise.all([
          import('./strongflow-review-view-model.js'),
          import('./strongflow-review-detail.js'),
          import('./strongflow-review-annotations.js'),
        ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const annotations = createStrongFlowReviewAnnotations()
      const model = createStrongFlowReviewViewModel({
        client: controlPlane,
        actor: context.actor,
        scope: context.scope,
        deliveryId,
        nextRequestId: () => contractId('req', browser.crypto) as RequestId,
      })
      const page = mountStrongFlowReviewDetail({
        root: slot,
        model,
        annotations,
        candidateActions: { candidates: clientCandidates, directory: clientDirectory,
          claim: clientId => clientOccupancy.claim({ clientId }),
        },
        onDownload(fileName, bytes, mediaType) {
          const url = URL.createObjectURL(new Blob([Uint8Array.from(bytes)], { type: mediaType }))
          const anchor = element(document, 'a', 'wwc-review-download-anchor')
          anchor.href = url
          anchor.download = fileName
          anchor.click()
          URL.revokeObjectURL(url)
        },
      })
      activeFeature = { close() { page.close(); annotations.close(); model.close() } }
      await model.start()
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'STRONGFLOW_REVIEW_ROUTE_FAILURE')
    }
  }

  function performRender(): void {
    renderGeneration += 1
    const generation = renderGeneration
    featureController?.abort()
    featureController = null
    activeFeature?.close()
    activeFeature = null
    currentScopeResolution = null
    activeSurface = clientSurfaceFromHash(browser.location.hash)
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
    title.textContent = browser.location.hash.startsWith('#/home/new-task')
      ? '新任务' : activeSurface.label
    title.hidden = activeSurface.id === 'onboarding'
    main.dataset.surface = activeSurface.id
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
        'repository',
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
      // Community: resolve the single authorized repository without a Scope
      // switcher. Empty selection auto-picks the only compatible repository.
      if (resolution.status === 'selected') {
        scopeNotice.hidden = true
        scopeNotice.textContent = ''
      } else if (resolution.status === 'denied') {
        scopeNotice.setAttribute('role', 'alert')
        scopeNotice.textContent = 'URL 中的范围已不再被授权，或当前账号没有可用仓库。'
        scopeNotice.hidden = false
      } else if (resolution.status === 'empty') {
        scopeNotice.setAttribute('role', 'status')
        scopeNotice.textContent = '此区域没有兼容的授权范围。'
        scopeNotice.hidden = false
      } else {
        scopeNotice.setAttribute('role', 'alert')
        scopeNotice.textContent = '当前账号有多个授权仓库；社区版仅支持启动时绑定的单一仓库。'
        scopeNotice.hidden = false
      }
    } else {
      currentScopeResolution = null
      scopeNotice.hidden = true
      scopeNotice.textContent = ''
    }
    // 设计稿 16 页:没有任何页面在内联位置渲染就绪检查清单(它属于独立的
    // 首次设置流程),因此外壳不再把这个区块压在内容上方;模型照常更新,
    // 保证独立的就绪页与连接健康状态保持准确。
    readinessRoot.hidden = true
    {
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
    recordHomeVisit(browser.location.hash)
    const chatSessionActive = activeSurface.id === 'chat'
      && browser.location.hash.includes('session=')
    void refreshRecentChats()
    for (const [id, link] of links) {
      const current = id === activeSurface.id
      if (current) link.setAttribute('aria-current', 'page')
      else link.removeAttribute('aria-current')
      // 设计稿 03b:会话打开时高亮的是会话行,不是「新对话」导航项。
      link.classList.toggle(
        'wwc-navigation-link-in-session',
        id === 'chat' && chatSessionActive,
      )
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
      } else if (homeRoute === 'candidate-preview') {
        launchRoute(renderCandidateRunPreview(generation), generation, 'CANDIDATE_PREVIEW_ROUTE_FAILURE')
      } else if (homeRoute === 'clarification') {
        launchRoute(renderDecisionClarification(generation), generation, 'DECISION_CLARIFICATION_ROUTE_FAILURE')
      } else if (homeRoute === 'review') {
        launchRoute(renderReview(generation), generation, 'STRONGFLOW_REVIEW_ROUTE_FAILURE')
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
    } else if (activeSurface.id === 'settings') {
      launchRoute(renderSettings(generation), generation, 'SETTINGS_ROUTE_FAILURE')
    } else if (activeSurface.id === 'onboarding') {
      launchRoute(Promise.resolve(renderOnboarding(generation)), generation, 'ONBOARDING_ROUTE_FAILURE')
    }
  }

  /** 设计稿 02:首次设置第 1 步——连接执行设备(裸画布流程页)。 */
  function renderOnboarding(generation: number): void {
    featureController?.abort()
    activeFeature?.close()
    activeFeature = null
    if (closed || generation !== renderGeneration) return
    let page: OnboardingPage | null = null
    try {
      page = mountOnboardingPage({
        root: slot,
        connect: async (clientId, connectionCode) => {
          await clientDirectory.addClient({
            clientId,
            connectionCode,
          })
          browser.location.hash = `#/device?${new URLSearchParams({
            organizationId: currentScopeResolution?.status === 'selected'
              && currentScopeResolution.scope.kind === 'repository'
              ? currentScopeResolution.scope.organizationId
              : '',
            workspaceId: currentScopeResolution?.status === 'selected'
              && currentScopeResolution.scope.kind === 'repository'
              ? currentScopeResolution.scope.workspaceId
              : '',
            projectId: currentScopeResolution?.status === 'selected'
              && currentScopeResolution.scope.kind === 'repository'
                ? currentScopeResolution.scope.projectId
                : '',
            repositoryId: currentScopeResolution?.status === 'selected'
              && currentScopeResolution.scope.kind === 'repository'
              ? currentScopeResolution.scope.repositoryId
              : '',
          }).toString()}`
        },
        onSignOut: () => { void authSession.logout() },
      })
      activeFeature = page
    } catch (error) {
      showRouteFailure(error, 'ONBOARDING_ROUTE_FAILURE')
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

  function render(): void {
    try {
      performRender()
    } catch (error) {
      showRouteFailure(error, 'CLIENT_RENDER_FAILURE')
    }
  }

  const onHashChange = () => {
    if (header.classList.contains('wwc-header-expanded')) {
      header.classList.remove('wwc-header-expanded')
      navigationToggle.setAttribute('aria-expanded', 'false')
      main.focus()
    }
    render()
  }
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
      || activeSurface.id === 'settings'
      || activeSurface.id === 'onboarding'
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
      closePreferences()
      historyGeneration += 1
      browser.clearInterval(historyTimer)
      browser.removeEventListener(REPOSITORY_NAME_CHANGED_EVENT, renderRecentChats)
      sessionBrowser.close()
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
