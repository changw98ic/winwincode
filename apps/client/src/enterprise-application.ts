// SPDX-License-Identifier: Apache-2.0

import {
  createEnterpriseManagementViewModel,
  type EnterpriseManagementArea,
  type EnterpriseManagementViewModel,
} from './enterprise-management-view-model.js'
import type { ControlPlaneClient } from './control-plane-client.js'
import type {
  Actor,
  ControlPlaneWebSocketSubscriptionId,
  RequestId,
  Scope,
} from './generated/contracts.js'

export type EnterpriseRouteId = 'resources' | 'operations'

export interface EnterpriseRoute {
  readonly id: EnterpriseRouteId
  readonly path: `/enterprise/${EnterpriseRouteId}`
  readonly label: string
  readonly description: string
  readonly areas: readonly EnterpriseManagementArea[]
}

export const ENTERPRISE_ROUTES: readonly EnterpriseRoute[] = Object.freeze([
  Object.freeze({
    id: 'resources',
    path: '/enterprise/resources',
    label: 'Resources and access',
    description: 'Organizations, members, teams, roles, projects, and repositories',
    areas: Object.freeze<EnterpriseManagementArea[]>([
      'organization',
      'members',
      'projects',
    ]),
  }),
  Object.freeze({
    id: 'operations',
    path: '/enterprise/operations',
    label: 'Governance and operations',
    description: 'Policy, remote Workers, usage, audit, and integrations',
    areas: Object.freeze<EnterpriseManagementArea[]>([
      'policy',
      'fleet',
      'usage',
      'audit',
      'integration',
    ]),
  }),
])

const DEFAULT_ENTERPRISE_ROUTE = ENTERPRISE_ROUTES[0] as EnterpriseRoute

export function enterpriseRouteFromHash(hash: string): EnterpriseRoute {
  const path = hash.replace(/^#/u, '').replace(/[?#].*$/u, '').replace(/\/$/u, '')
  return ENTERPRISE_ROUTES.find(route => route.path === path) ?? DEFAULT_ENTERPRISE_ROUTE
}

export interface EnterpriseClientContext {
  /** Identity established by the browser-session composition boundary. */
  readonly actor: Actor
  /** Authorized tenant boundary selected by that same session composition. */
  readonly scope: Scope
  readonly subscriptionId: ControlPlaneWebSocketSubscriptionId
  readonly nextRequestId: () => RequestId
  readonly onAuditExport?: (filename: string, content: string) => void
}

export interface EnterpriseApplicationOptions extends EnterpriseClientContext {
  readonly root: HTMLElement
  readonly client: ControlPlaneClient
  readonly hash: string
  readonly signal?: AbortSignal
}

export interface EnterpriseApplication {
  readonly route: EnterpriseRoute
  readonly model: EnterpriseManagementViewModel
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

function isRouteDenied(
  model: EnterpriseManagementViewModel,
  route: EnterpriseRoute,
): boolean {
  return route.areas.every(area => model.state.areas[area].permission === 'denied')
}

/**
 * Load one enterprise route into the canonical Client shell. Page modules stay in route chunks;
 * every route shares this single generated-facade view-model boundary.
 */
export async function mountEnterpriseApplication(
  options: EnterpriseApplicationOptions,
): Promise<EnterpriseApplication | null> {
  const route = enterpriseRouteFromHash(options.hash)
  const mountPage = route.id === 'resources'
    ? await import('./enterprise-resource-page.js').then(module => (
        (root: HTMLElement, model: EnterpriseManagementViewModel) => (
          module.mountEnterpriseResourcePage({ root, model })
        )
      ))
    : await import('./enterprise-operations-page.js').then(module => (
        (root: HTMLElement, model: EnterpriseManagementViewModel) => (
          module.mountEnterpriseOperationsPage({
            root,
            model,
            ...(options.onAuditExport === undefined
              ? {}
              : { onAuditExport: options.onAuditExport }),
          })
        )
      ))
  if (options.signal?.aborted === true) return null

  const document = options.root.ownerDocument
  const layout = element(document, 'div', 'wwc-enterprise-application')
  const navigation = element(document, 'nav', 'wwc-enterprise-navigation')
  const routeStatus = element(document, 'p', 'wwc-enterprise-route-status')
  const pageRoot = element(document, 'div', 'wwc-enterprise-route-root')
  const links = new Map<EnterpriseRouteId, HTMLAnchorElement>()
  const model = createEnterpriseManagementViewModel({
    client: options.client,
    actor: options.actor,
    scope: options.scope,
    subscriptionId: options.subscriptionId,
    nextRequestId: options.nextRequestId,
  })
  let closed = false

  navigation.setAttribute('aria-label', 'Enterprise management')
  routeStatus.setAttribute('role', 'status')
  routeStatus.setAttribute('aria-live', 'polite')
  pageRoot.dataset.enterpriseRoute = route.id
  for (const candidate of ENTERPRISE_ROUTES) {
    const link = element(document, 'a', 'wwc-enterprise-navigation-link')
    link.href = `#${candidate.path}`
    link.textContent = candidate.label
    link.title = candidate.description
    link.dataset.enterpriseRoute = candidate.id
    if (candidate.id === route.id) link.setAttribute('aria-current', 'page')
    link.addEventListener('click', event => {
      if (link.getAttribute('aria-disabled') !== 'true') return
      event.preventDefault()
    })
    links.set(candidate.id, link)
    navigation.append(link)
  }
  layout.append(navigation, routeStatus, pageRoot)
  options.root.replaceChildren(layout)

  const unsubscribe = model.subscribe(state => {
    if (closed) return
    for (const candidate of ENTERPRISE_ROUTES) {
      const link = links.get(candidate.id)
      if (link === undefined) continue
      const denied = isRouteDenied(model, candidate)
      link.setAttribute('aria-disabled', String(denied))
      link.tabIndex = denied ? -1 : 0
      link.dataset.permission = denied ? 'denied' : 'available'
    }
    routeStatus.textContent = state.realtime === 'reloading'
      ? `${route.label} is refreshing from an enterprise event.`
      : state.realtime === 'reconnecting'
        ? `${route.label} is reconnecting to enterprise events.`
        : isRouteDenied(model, route)
          ? `${route.label} is not available to the current identity.`
          : `${route.label} route is active.`
  })

  let page: { close(): void }
  try {
    page = mountPage(pageRoot, model)
  } catch (error) {
    closed = true
    unsubscribe()
    model.close()
    options.root.replaceChildren()
    throw error
  }

  return Object.freeze({
    route,
    model,
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      page.close()
      options.root.replaceChildren()
    },
  })
}
