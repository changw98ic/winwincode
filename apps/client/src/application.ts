// SPDX-License-Identifier: Apache-2.0

import {
  createControlPlaneClient,
  type ControlPlaneClient,
  type ControlPlaneClientTransport,
} from './control-plane-client.js'
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
  SessionListResultResponse,
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
}

export interface WinWinCodeClientApplication {
  readonly controlPlane: ControlPlaneClient
  readonly authSession: AuthSessionViewModel
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
  prefix: 'req' | 'sub' | 'psn',
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
  let accessFailureSession: AuthSessionViewModel | null = null
  const controlPlane = options.controlPlane ?? createControlPlaneClient({
      serverUrl: options.serverUrl,
      transport: browserControlPlaneTransport(browser),
      onAccessFailure(error) {
        accessFailureSession?.authenticationRequired(error)
      },
    })
  const authSession = createAuthSessionViewModel(controlPlane)
  accessFailureSession = authSession
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
  let closed = false

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
  main.append(title, description, slot)
  shell.append(header, main)
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
      let productSessionId = parameters.get('session') as ProductSessionId | null
      if (productSessionId === null) {
        const response = await controlPlane.query({
          schemaVersion: 'winwincode/v1',
          requestId: contractId('req', browser.crypto) as RequestId,
          actor: context.actor,
          scope: context.scope,
          query: QueryName.SessionList,
          parameters: { states: [] },
          page: { cursor: null, limit: 1 },
        }, { signal: controller.signal })
        if (response.query !== QueryName.SessionList) {
          throw new Error('The Chat route received another query response.')
        }
        productSessionId = (response as SessionListResultResponse).result.items[0]?.id ?? null
      }
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      if (productSessionId === null) {
        routeUnavailable('No Chat session exists in this repository yet.')
        return
      }
      if (parameters.get('session') === null) {
        replaceHash(`#/chat?session=${encodeURIComponent(productSessionId)}`)
      }
      const [{ createChatViewModel }, { mountChatPage }] = await Promise.all([
        import('./chat-view-model.js'),
        import('./chat-page.js'),
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
      })
      activeFeature = mountChatPage({
        root: slot,
        model,
        nextProductSessionId: () => contractId(
          'psn',
          browser.crypto,
        ) as ProductSessionId,
      })
    } catch {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      routeUnavailable('Chat could not be opened. Reload this route to retry.')
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
        routeUnavailable('No Delivery exists in this repository yet.')
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
      activeFeature = mountStrongFlowPage({ root: slot, model, deliveries })
    } catch {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      routeUnavailable('StrongFlow could not be opened. Reload this route to retry.')
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
    } catch {
      if (closed || generation !== renderGeneration || controller.signal.aborted) return
      const failure = element(document, 'p', 'wwc-enterprise-route-failure')
      failure.setAttribute('role', 'alert')
      failure.textContent = 'Enterprise management could not be loaded. Reload this route to retry.'
      slot.replaceChildren(failure)
    }
  }

  function render(): void {
    renderGeneration += 1
    const generation = renderGeneration
    featureController?.abort()
    featureController = null
    activeFeature?.close()
    activeFeature = null
    activeSurface = clientSurfaceFromHash(browser.location.hash)
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
    if (activeSurface.id === 'chat') void renderChat(generation)
    else if (activeSurface.id === 'strongflow') void renderStrongFlow(generation)
    else if (activeSurface.id === 'enterprise') void renderEnterprise(generation)
  }

  const onHashChange = () => { render() }
  browser.addEventListener('hashchange', onHashChange)
  const unsubscribeAuthSession = authSession.subscribe(() => {
    if (
      activeSurface.id === 'chat'
      || activeSurface.id === 'strongflow'
      || activeSurface.id === 'enterprise'
    ) render()
  })
  render()
  void authSession.restore()

  return {
    controlPlane,
    authSession,
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
      unsubscribeAuthSession()
      featureController?.abort()
      featureController = null
      activeFeature?.close()
      activeFeature = null
      authPage.close()
      authSession.close()
      accessFailureSession = null
      controlPlane.close()
      options.root.replaceChildren()
    },
  }
}
