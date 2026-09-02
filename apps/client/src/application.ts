// SPDX-License-Identifier: Apache-2.0

import {
  createControlPlaneClient,
  type ControlPlaneClient,
  type ControlPlaneClientTransport,
} from './control-plane-client.js'
import { mountClientErrorBoundary } from './components/client-error-boundary.js'
import { mountConnectionBar } from './components/connection-bar.js'
import {
  classifyClientFailure,
  createConnectionMonitor,
  createSafeDiagnostic,
  observeControlPlaneClient,
  type ClientFailure,
  type ConnectionMonitor,
  type ConnectionSnapshot,
} from './core/connection-state.js'
import { mountAuthSessionPage, type AuthSessionPage } from './auth-page.js'
import {
  createAuthSessionViewModel,
  type AuthSessionViewModel,
} from './auth-view-model.js'
import type { EnterpriseApplication } from './enterprise-application.js'
import type {
  ControlPlaneWebSocketSubscriptionId,
  DeliveryGetResultResponse,
  DeliveryId,
  DeliveryListResultResponse,
  ProductSessionId,
  RepositoryScope,
  RequestId,
  StageRunId,
} from './generated/contracts.js'
import { QueryName } from './generated/contracts.js'

export type ClientSurfaceId =
  | 'chat'
  | 'strongflow'
  | 'settings'
  | 'approvals'
  | 'enterprise'

export interface ClientSurface {
  readonly id: ClientSurfaceId
  readonly path: `/${ClientSurfaceId}`
  readonly label: string
  readonly description: string
  readonly default: boolean
}

export const CLIENT_SURFACES: readonly ClientSurface[] = Object.freeze([
  Object.freeze({
    id: 'chat',
    path: '/chat',
    label: 'Chat',
    description: 'Conversation workspace',
    default: true,
  }),
  Object.freeze({
    id: 'strongflow',
    path: '/strongflow',
    label: 'StrongFlow',
    description: 'Advanced delivery workspace',
    default: false,
  }),
  Object.freeze({
    id: 'settings',
    path: '/settings',
    label: 'Settings',
    description: 'Personal and workspace settings',
    default: false,
  }),
  Object.freeze({
    id: 'approvals',
    path: '/approvals',
    label: 'Approvals',
    description: 'Human decisions awaiting review',
    default: false,
  }),
  Object.freeze({
    id: 'enterprise',
    path: '/enterprise',
    label: 'Enterprise',
    description: 'Organization administration',
    default: false,
  }),
])

const DEFAULT_SURFACE = CLIENT_SURFACES[0] as ClientSurface

export function clientSurfaceFromHash(hash: string): ClientSurface {
  const path = hash.replace(/^#/u, '').replace(/\?.*$/u, '')
  return CLIENT_SURFACES.find(surface => (
    surface.path === path || path.startsWith(`${surface.path}/`)
  )) ?? DEFAULT_SURFACE
}

export interface WinWinCodeClientApplicationOptions {
  readonly serverUrl: string
  readonly root: HTMLElement
  readonly window?: Window
  /** Canonical facade injection used by deterministic browser fixtures and host composition. */
  readonly controlPlane?: ControlPlaneClient
  /** Secret-safe clipboard seam for browser hosts and deterministic fixtures. */
  readonly copyText?: (value: string) => Promise<void> | void
  readonly now?: () => string
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

export function strongFlowRouteHash(
  deliveryId: DeliveryId,
  productSessionId: ProductSessionId,
  stageRunId: StageRunId,
): string {
  return `#/strongflow?delivery=${encodeURIComponent(deliveryId)}`
    + `&session=${encodeURIComponent(productSessionId)}`
    + `&stageRun=${encodeURIComponent(stageRunId)}`
}

function repositoryScope(
  session: AuthSessionViewModel['state']['session'],
): RepositoryScope | null {
  if (session === null) return null
  return session.authorizedScopes.find(scope => scope.kind === 'repository') ?? null
}

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
  const rawControlPlane = options.controlPlane ?? createControlPlaneClient({
      serverUrl: options.serverUrl,
      transport: browserControlPlaneTransport(browser),
      onAccessFailure(error) {
        accessFailureSession?.authenticationRequired(error)
      },
    })
  const observedControlPlane = observeControlPlaneClient({
    client: rawControlPlane,
    monitor: connection,
    online: browserIsOnline,
  })
  const controlPlane = observedControlPlane.client
  const authSession = createAuthSessionViewModel(controlPlane)
  accessFailureSession = authSession
  let lastKnownDiagnosticScope: unknown = null
  const shell = element(document, 'div', 'wwc-shell')
  const header = element(document, 'header', 'wwc-header')
  const brand = element(document, 'strong', 'wwc-brand')
  const navigation = element(document, 'nav', 'wwc-navigation')
  const authRoot = element(document, 'div', 'wwc-auth-session-root')
  const main = element(document, 'main', 'wwc-main')
  const title = element(document, 'h1', 'wwc-surface-title')
  const description = element(document, 'p', 'wwc-surface-description')
  const slot = element(document, 'section', 'wwc-surface-slot')
  const links = new Map<ClientSurfaceId, HTMLAnchorElement>()
  let activeSurface = clientSurfaceFromHash(browser.location.hash)
  let activeFeature: EnterpriseApplication | MountedClientFeature | null = null
  let featureController: AbortController | null = null
  let renderGeneration = 0
  let currentFailure: ClientFailure | null = null
  let closed = false

  function diagnosticScope(): unknown {
    const session = authSession.state.session
    if (session === null) return lastKnownDiagnosticScope
    const scope = activeSurface.id === 'enterprise'
      ? (session.authorizedScopes[0] ?? null)
      : (repositoryScope(session) ?? session.authorizedScopes[0] ?? null)
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
    replaceHash('#/chat')
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
  slot.setAttribute('aria-live', 'polite')

  for (const surface of CLIENT_SURFACES) {
    const link = element(document, 'a', 'wwc-navigation-link')
    link.href = `#${surface.path}`
    link.textContent = surface.label
    link.dataset.surface = surface.id
    links.set(surface.id, link)
    navigation.append(link)
  }

  header.append(brand, navigation, authRoot)
  main.append(title, description, errorBoundary.root, slot)
  shell.append(header, connectionBar.root, main)
  options.root.replaceChildren(shell)
  const authPage: AuthSessionPage = mountAuthSessionPage({
    root: authRoot,
    model: authSession,
  })

  function authenticatedRouteContext(): {
    readonly actor: NonNullable<AuthSessionViewModel['state']['session']>['actor']
    readonly scope: RepositoryScope
  } | null {
    const session = authSession.state.session
    const scope = repositoryScope(session)
    if (authSession.state.status !== 'signed-in' || session === null || scope === null) {
      const unavailable = element(document, 'p', 'wwc-authenticated-context-required')
      unavailable.setAttribute('role', 'status')
      unavailable.textContent = authSession.state.status === 'restoring'
        ? 'Restoring your signed-in workspace…'
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
          replaceHash(`#/chat?session=${encodeURIComponent(nextProductSessionId)}`)
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
          replaceHash(`#/strongflow?delivery=${encodeURIComponent(deliveryId)}`)
          render()
        },
      })
      activeFeature = mountChatPage({
        root: slot,
        model,
        deliveryCreator,
        scope: context.scope,
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
        const [{ createLocalOperationsViewModel }, { mountLocalOperationsPage }] = await Promise.all([
          import('./local-operations-view-model.js'),
          import('./local-operations-page.js'),
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
        activeFeature = mountLocalOperationsPage({ root: slot, model })
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
        localOperationsHref: '#/settings/runtime',
      })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, operationsRoute
        ? 'LOCAL_OPERATIONS_ROUTE_FAILURE'
        : 'SETTINGS_ROUTE_FAILURE')
    }
  }

  async function renderApprovals(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const parameters = routeParameters(browser.location.hash)
    const productSessionId = parameters.get('session') as ProductSessionId | null
    if (productSessionId === null) {
      routeUnavailable('Choose a Chat session before opening Approvals.')
      return
    }
    const controller = new AbortController()
    featureController = controller
    routeLoading('Loading Approvals…')
    try {
      const deliveryId = parameters.get('delivery') as DeliveryId | null
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
      activeFeature = mountLocalDecisionsPage({ root: slot, model })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'APPROVALS_ROUTE_FAILURE')
    }
  }

  async function renderStrongFlow(generation: number): Promise<void> {
    const context = authenticatedRouteContext()
    if (context === null) return
    const controller = new AbortController()
    featureController = controller
    routeLoading('Loading StrongFlow…')
    try {
      const parameters = routeParameters(browser.location.hash)
      const deliveriesValue = await controlPlane.query({
        schemaVersion: 'winwincode/v1',
        requestId: contractId('req', browser.crypto) as RequestId,
        actor: context.actor,
        scope: context.scope,
        query: QueryName.DeliveryList,
        parameters: { states: [] },
        page: { cursor: null, limit: 50 },
      }, { signal: controller.signal })
      if (deliveriesValue.query !== QueryName.DeliveryList) {
        throw new Error('The StrongFlow route received another list response.')
      }
      const deliveries = (deliveriesValue as DeliveryListResultResponse).result.items
      const deliveryId = (
        parameters.get('delivery') as DeliveryId | null
      ) ?? deliveries[0]?.deliveryId ?? null
      if (deliveryId === null) {
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
            replaceHash(`#/strongflow?delivery=${encodeURIComponent(createdDeliveryId)}`)
            render()
          },
        })
        activeFeature = mountStrongFlowCreatePage({
          root: slot,
          model,
          scope: context.scope,
        })
        return
      }
      const detailValue = await controlPlane.query({
        schemaVersion: 'winwincode/v1',
        requestId: contractId('req', browser.crypto) as RequestId,
        actor: context.actor,
        scope: context.scope,
        query: QueryName.DeliveryGet,
        parameters: { deliveryId },
        page: { cursor: null, limit: 1 },
      }, { signal: controller.signal })
      if (detailValue.query !== QueryName.DeliveryGet) {
        throw new Error('The StrongFlow route received another detail response.')
      }
      const detail = (detailValue as DeliveryGetResultResponse).result
      const requestedStageRunId = parameters.get('stageRun') as StageRunId | null
      const stage = requestedStageRunId === null
        ? [...detail.stages].reverse().find(candidate => candidate.sessionBinding !== null)
        : detail.stages.find(candidate => candidate.id === requestedStageRunId)
      const productSessionId = (
        parameters.get('session') as ProductSessionId | null
      ) ?? stage?.sessionBinding?.productSessionId ?? null
      if (stage === undefined || stage.sessionBinding === null || productSessionId === null) {
        routeUnavailable('This Delivery does not have an executable StrongFlow stage yet.')
        return
      }
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      if (
        parameters.get('delivery') === null
        || parameters.get('session') === null
        || parameters.get('stageRun') === null
      ) {
        replaceHash(strongFlowRouteHash(deliveryId, productSessionId, stage.id))
      }
      const [{ createStrongFlowViewModel }, { mountStrongFlowPage }] = await Promise.all([
        import('./strongflow-view-model.js'),
        import('./strongflow-page.js'),
      ])
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
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
        onStageBindingChange(binding) {
          if (closed || generation !== renderGeneration || controller.signal.aborted) return
          replaceHash(strongFlowRouteHash(
            deliveryId,
            binding.productSessionId,
            binding.stageRunId,
          ))
        },
      })
      activeFeature = mountStrongFlowPage({
        root: slot,
        model,
        deliveries,
        evidence: {
          client: controlPlane,
          actor: context.actor,
          scope: context.scope,
          nextRequestId: () => contractId('req', browser.crypto) as RequestId,
        },
      })
    } catch (error) {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      showRouteFailure(error, 'STRONGFLOW_ROUTE_FAILURE')
    }
  }

  async function renderEnterprise(generation: number): Promise<void> {
    const session = authSession.state.session
    if (authSession.state.status !== 'signed-in' || session === null) {
      const unavailable = element(document, 'p', 'wwc-enterprise-context-required')
      unavailable.setAttribute('role', 'status')
      unavailable.textContent = authSession.state.status === 'restoring'
        ? 'Restoring enterprise identity and organization access…'
        : 'Sign in to load enterprise management.'
      slot.replaceChildren(unavailable)
      return
    }
    const scope = session.authorizedScopes[0]
    if (scope === undefined) throw new Error('Authenticated session has no authorized Scope.')
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

  function performRender(): void {
    renderGeneration += 1
    const generation = renderGeneration
    featureController?.abort()
    featureController = null
    activeFeature?.close()
    activeFeature = null
    activeSurface = clientSurfaceFromHash(browser.location.hash)
    clearRouteFailure()
    title.textContent = activeSurface.label
    description.textContent = activeSurface.description
    slot.dataset.winwincodeSurface = activeSurface.id
    slot.replaceChildren()
    for (const [id, link] of links) {
      if (id === activeSurface.id) link.setAttribute('aria-current', 'page')
      else link.removeAttribute('aria-current')
    }
    options.root.dispatchEvent(new CustomEvent('winwincode:surface-change', {
      detail: Object.freeze({
        surface: activeSurface,
        controlPlane,
      }),
    }))
    if (activeSurface.id === 'chat') launchRoute(renderChat(generation), generation, 'CHAT_ROUTE_FAILURE')
    else if (activeSurface.id === 'strongflow') {
      launchRoute(renderStrongFlow(generation), generation, 'STRONGFLOW_ROUTE_FAILURE')
    } else if (activeSurface.id === 'settings') {
      launchRoute(renderSettings(generation), generation, 'SETTINGS_ROUTE_FAILURE')
    } else if (activeSurface.id === 'approvals') {
      launchRoute(renderApprovals(generation), generation, 'APPROVALS_ROUTE_FAILURE')
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

  function render(): void {
    try {
      performRender()
    } catch (error) {
      showRouteFailure(error, 'CLIENT_RENDER_FAILURE')
    }
  }

  const onHashChange = () => { render() }
  const onOffline = () => { connection.offline() }
  const onOnline = () => { observedControlPlane.reconnectAll() }
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
  browser.addEventListener('error', onWindowError)
  browser.addEventListener('unhandledrejection', onUnhandledRejection)
  const unsubscribeConnection = connection.subscribe(updateReliabilityViews)
  const unsubscribeAuthSession = authSession.subscribe(state => {
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
    if ((state.status === 'signed-in' || state.status === 'signed-out') && (
      activeSurface.id === 'chat'
      || activeSurface.id === 'strongflow'
      || activeSurface.id === 'settings'
      || activeSurface.id === 'approvals'
      || activeSurface.id === 'enterprise'
    )) render()
  })
  render()
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
      browser.location.hash = surface.path
      render()
    },
    close() {
      if (closed) return
      closed = true
      browser.removeEventListener('hashchange', onHashChange)
      browser.removeEventListener('offline', onOffline)
      browser.removeEventListener('online', onOnline)
      browser.removeEventListener('error', onWindowError)
      browser.removeEventListener('unhandledrejection', onUnhandledRejection)
      unsubscribeAuthSession()
      unsubscribeConnection()
      featureController?.abort()
      featureController = null
      activeFeature?.close()
      activeFeature = null
      authPage.close()
      authSession.close()
      accessFailureSession = null
      controlPlane.close()
      errorBoundary.close()
      connectionBar.close()
      connection.close()
      options.root.replaceChildren()
    },
  }
}
