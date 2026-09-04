// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  type ControlPlaneClient,
} from './control-plane-client.js'
import type {
  ApprovalListResultResponse,
  ApprovalProjection,
  Actor,
  DeliveryListResultResponse,
  DeliveryProjection,
  OpaqueCursor,
  QueryResultResponse,
  RepositoryScope,
  RequestId,
} from './generated/contracts.js'
import { QueryName } from './generated/contracts.js'
import {
  attentionSignalBadge,
  attentionSignals,
  attentionSignalsTitle,
  attentionSignalRouteHash,
  createAttentionSignalGate,
  type AttentionSignal,
  type AttentionSignalBadge,
} from './attention-signals.js'

const SCHEMA_VERSION = 'winwincode/v1' as const
const PAGE_SIZE = 200
const MAX_PAGES = 10
const DEFAULT_TICK_MILLIS = 30_000
const TITLE_BADGE_PATTERN = /^\(\d+\)\s*/u

export type AttentionNotificationStatus =
  | 'idle'
  | 'loading'
  | 'ready'
  | 'refreshing'
  | 'error'
  | 'closed'

export interface AttentionDesktopNotificationContent {
  /** The notification identity: one operating-system tag per event identity. */
  readonly tag: string
  readonly title: string
  readonly body: string
}

/**
 * The single seam to the browser notification permission and surface.  The
 * browser only grants it after an explicit user action, and the Client only
 * hands it secret-safe content.
 */
export interface AttentionDesktopNotifications {
  readonly supported: boolean
  permission(): 'default' | 'granted' | 'denied'
  requestPermission(): Promise<'default' | 'granted' | 'denied'>
  show(content: AttentionDesktopNotificationContent, onClick: () => void): void
  close(tag: string): void
}

export interface AttentionNotificationDesktopState {
  readonly supported: boolean
  readonly permission: 'default' | 'granted' | 'denied'
  readonly enabled: boolean
  readonly blocked: boolean
}

export interface AttentionNotificationMonitorState {
  readonly status: AttentionNotificationStatus
  readonly signals: readonly AttentionSignal[]
  readonly badge: AttentionSignalBadge
  /** The exact document title to present, including the count when one exists. */
  readonly titleText: string
  readonly desktop: AttentionNotificationDesktopState
}

export interface AttentionNotificationMonitor {
  readonly state: AttentionNotificationMonitorState
  subscribe(listener: (state: AttentionNotificationMonitorState) => void): () => void
  start(): Promise<void>
  refresh(): Promise<void>
  /** Re-apply the navigation badge after the shell rebuilt its navigation labels. */
  applyBadge(): void
  setDesktopEnabled(enabled: boolean): Promise<void>
  close(): void
}

export interface AttentionNotificationControl {
  readonly state: AttentionNotificationMonitorState
  subscribe(listener: (state: AttentionNotificationMonitorState) => void): () => void
  setDesktopEnabled(enabled: boolean): Promise<void>
}

/** Browser adapter for the one desktop notification seam. */
export function browserAttentionDesktopNotifications(
  browser: Window,
): AttentionDesktopNotifications {
  const source = browser as unknown as {
    readonly Notification?: {
      readonly permission: 'default' | 'granted' | 'denied'
      requestPermission(): Promise<unknown>
      new (title: string, options?: {
        readonly body?: string
        readonly tag?: string
      }): {
        close(): void
        set onclick(handler: (() => void) | null)
      }
    }
  }
  const notification = source.Notification
  const supported = typeof notification === 'function'
  const shown = new Map<string, { close(): void }>()
  const adapter: AttentionDesktopNotifications = {
    supported,
    permission() {
      if (!supported) return 'denied'
      return notification.permission
    },
    async requestPermission() {
      if (!supported) return 'denied'
      const granted = await notification.requestPermission()
      return granted === 'granted' ? 'granted' : 'denied'
    },
    show(content, onClick) {
      if (!supported) return
      const instance = new notification(content.title, {
        ...(content.body === undefined ? {} : { body: content.body }),
        ...(content.tag === undefined ? {} : { tag: content.tag }),
      })
      // One tag per event identity keeps at most one notification per event.
      if (content.tag !== undefined) shown.set(content.tag, instance)
      instance.onclick = () => {
        instance.close()
        if (content.tag !== undefined) shown.delete(content.tag)
        browser.focus()
        onClick()
      }
    },
    close(tag) {
      shown.get(tag)?.close()
      shown.delete(tag)
    },
  }
  return Object.freeze(adapter)
}

export interface AttentionNotificationMonitorOptions {
  readonly client: ControlPlaneClient
  readonly actor: Actor
  readonly scope: RepositoryScope
  readonly nextRequestId: () => RequestId
  readonly nowMillis?: () => number
  /** Presentation host that carries the page title count. */
  readonly document: Document
  /** The navigation entry that carries the badge. */
  readonly badgeTarget: HTMLElement
  /** Base page title without a count prefix; defaults to the current title. */
  readonly titleBase?: string
  readonly notifications?: AttentionDesktopNotifications | null
  readonly onOpenTarget?: (hash: string) => void
  /** Bounded revalidation clock; the monitor owns no event subscription. */
  readonly scheduleTick?: (handler: () => void, millis: number) => () => void
  readonly tickMillis?: number
}

function clientFailure(message: string, cause?: unknown): ControlPlaneClientError {
  return new ControlPlaneClientError({
    kind: 'protocol',
    code: 'ATTENTION_NOTIFICATION_FAILURE',
    message,
    requestId: null,
    retryable: false,
    ...(cause === undefined ? {} : { cause }),
  })
}

type PagedResponse = {
  readonly page: { readonly hasMore: boolean; readonly nextCursor: OpaqueCursor | null }
}

function expectQuery<Query extends QueryResultResponse['query']>(
  response: QueryResultResponse,
  query: Query,
): Extract<QueryResultResponse, { readonly query: Query }> {
  if (response.query !== query) throw clientFailure(
    'The notification monitor received another query result.',
  )
  return response as Extract<QueryResultResponse, { readonly query: Query }>
}

function cursorAfter(response: PagedResponse, seen: Set<OpaqueCursor>): OpaqueCursor | null {
  if (!response.page.hasMore) return null
  const next = response.page.nextCursor
  if (next === null || seen.has(next)) throw clientFailure(
    'The notification monitor received an invalid continuation cursor.',
  )
  seen.add(next)
  return next
}

function itemsOf<T>(response: { readonly result: unknown }, kind: string): readonly T[] {
  const result = response.result
  if (typeof result !== 'object' || result === null) throw clientFailure(
    'The notification monitor received an unexpected result.',
  )
  const items = (result as { readonly items?: unknown }).items
  if (!Array.isArray(items)) throw clientFailure(`The ${kind} result carried no item list.`)
  return items as readonly T[]
}

/** Strip an already rendered count prefix so the base title never accumulates. */
function baseTitleOf(document: Document, explicit: string | undefined): string {
  if (explicit !== undefined) return explicit
  return document.title.replace(TITLE_BADGE_PATTERN, '')
}

function browserTick(browser: Window | undefined): (
  handler: () => void,
  millis: number,
) => () => void {
  return (handler, millis) => {
    if (typeof browser?.setInterval !== 'function') return () => {}
    const handle = browser.setInterval(handler, millis)
    return () => { browser.clearInterval(handle) }
  }
}

/**
 * UI-506 shell monitor.  It keeps the navigation badge, the page title count,
 * and the optional desktop notifications aligned with the authoritative
 * projections, notifies one event identity exactly once, and clears resolved or
 * expired entries on the next bounded tick.  It opens no event subscription, so
 * the mounted feature routes keep their single event stream.
 */
export function createAttentionNotificationMonitor(
  options: AttentionNotificationMonitorOptions,
): AttentionNotificationMonitor {
  const nowMillis = options.nowMillis ?? Date.now
  const titleBase = baseTitleOf(options.document, options.titleBase)
  const desktop = options.notifications ?? null
  const gate = createAttentionSignalGate()
  const scheduleTick = options.scheduleTick
    ?? browserTick(options.document.defaultView ?? undefined)
  const tickMillis = options.tickMillis ?? DEFAULT_TICK_MILLIS
  const shownTags = new Set<string>()
  const listeners = new Set<(state: AttentionNotificationMonitorState) => void>()
  let badgeNode: HTMLElement | null = null
  let stopTick: (() => void) | null = null
  let closed = false
  let desktopEnabled = false
  let currentState: AttentionNotificationMonitorState = initial()

  function initial(): AttentionNotificationMonitorState {
    return Object.freeze({
      status: 'idle' as const,
      signals: Object.freeze([]) as readonly AttentionSignal[],
      badge: attentionSignalBadge([]),
      titleText: titleBase,
      desktop: desktopState(),
    })
  }

  function publish(state: AttentionNotificationMonitorState): void {
    currentState = Object.freeze(state)
    for (const listener of listeners) listener(currentState)
  }

  function patch(update: Partial<AttentionNotificationMonitorState>): void {
    publish({ ...currentState, ...update })
  }

  function desktopState(
    overrides: Partial<AttentionNotificationDesktopState> = {},
  ): AttentionNotificationDesktopState {
    const permission = overrides.permission ?? desktop?.permission() ?? 'default'
    return Object.freeze({
      supported: overrides.supported ?? desktop?.supported ?? false,
      permission,
      enabled: overrides.enabled ?? desktopEnabled,
      blocked: overrides.blocked ?? permission === 'denied',
    })
  }

  function applyBadge(): void {
    options.document.title = currentState.titleText
    const target = options.badgeTarget
    const total = currentState.badge.total
    // The shell rewrites the navigation label on its own, so only the badge
    // node is ever touched here and the entry keeps its own name.
    if (badgeNode !== null && badgeNode.parentNode !== target) {
      badgeNode.remove()
      badgeNode = null
    }
    if (total === 0) {
      if (badgeNode !== null) {
        badgeNode.remove()
        badgeNode = null
      }
      delete target.dataset.wwcBadge
      target.removeAttribute('aria-label')
      return
    }
    target.dataset.wwcBadge = String(total)
    target.setAttribute(
      'aria-label',
      `Attention · ${String(total)} ${total === 1 ? 'entry' : 'entries'} need you`,
    )
    if (badgeNode === null) {
      badgeNode = target.ownerDocument.createElement('span')
      target.append(badgeNode)
    }
    badgeNode.className = 'wwc-navigation-badge'
    badgeNode.setAttribute('aria-hidden', 'true')
    badgeNode.textContent = String(total)
  }

  function notify(admitted: readonly AttentionSignal[]): void {
    if (desktop === null || !desktopEnabled || desktop.permission() !== 'granted') return
    for (const signal of admitted) {
      shownTags.add(signal.identity)
      desktop.show({
        tag: signal.identity,
        title: signal.title,
        body: signal.context,
      }, () => {
        options.onOpenTarget?.(attentionSignalRouteHash(signal, {
          organizationId: options.scope.organizationId,
          workspaceId: options.scope.workspaceId,
          projectId: options.scope.projectId,
          repositoryId: options.scope.repositoryId,
        }))
      })
    }
  }

  function retractResolved(signals: readonly AttentionSignal[]): void {
    if (desktop === null || shownTags.size === 0) return
    const current = new Set(signals.map(signal => signal.identity))
    for (const tag of [...shownTags]) {
      if (current.has(tag)) continue
      shownTags.delete(tag)
      desktop.close(tag)
    }
  }

  async function load(loadingStatus: 'loading' | 'refreshing'): Promise<void> {
    if (closed) throw clientFailure('The notification monitor is closed.')
    patch({ status: loadingStatus })
    const approvals: ApprovalProjection[] = []
    const deliveries: DeliveryProjection[] = []
    const clock = nowMillis()
    try {
      let cursor: OpaqueCursor | null = null
      const approvalCursors = new Set<OpaqueCursor>()
      for (let index = 0; index < MAX_PAGES; index += 1) {
        const response = expectQuery(await options.client.query({
          schemaVersion: SCHEMA_VERSION,
          requestId: options.nextRequestId(),
          actor: options.actor,
          scope: options.scope,
          query: QueryName.ApprovalList,
          parameters: { states: ['pending'] },
          page: { cursor, limit: PAGE_SIZE },
        }), QueryName.ApprovalList) as ApprovalListResultResponse
        approvals.push(...itemsOf<ApprovalProjection>(response, 'Approval list'))
        cursor = cursorAfter(response, approvalCursors)
        if (cursor === null) break
      }
      if (cursor !== null) throw clientFailure('The Approval list exceeded the bounded page limit.')
      let deliveryCursor: OpaqueCursor | null = null
      const deliveryCursors = new Set<OpaqueCursor>()
      for (let index = 0; index < MAX_PAGES; index += 1) {
        const response = expectQuery(await options.client.query({
          schemaVersion: SCHEMA_VERSION,
          requestId: options.nextRequestId(),
          actor: options.actor,
          scope: options.scope,
          query: QueryName.DeliveryList,
          parameters: { states: [] },
          page: { cursor: deliveryCursor, limit: PAGE_SIZE },
        }), QueryName.DeliveryList) as DeliveryListResultResponse
        deliveries.push(...itemsOf<DeliveryProjection>(response, 'Delivery list'))
        deliveryCursor = cursorAfter(response, deliveryCursors)
        if (deliveryCursor === null) break
      }
      if (deliveryCursor !== null) {
        throw clientFailure('The Delivery list exceeded the bounded page limit.')
      }
    } catch {
      // A failed revalidation never presents stale counts and never notifies.
      gate.close()
      shownTags.clear()
      patch({
        status: 'error',
        signals: Object.freeze([]) as readonly AttentionSignal[],
        badge: attentionSignalBadge([]),
        titleText: titleBase,
      })
      applyBadge()
      return
    }
    const signals = attentionSignals({ approvals, deliveries, nowMillis: clock })
    const admitted = gate.admit(signals)
    const badge = attentionSignalBadge(signals)
    retractResolved(signals)
    notify(admitted)
    publish({
      status: 'ready',
      signals,
      badge,
      titleText: attentionSignalsTitle(titleBase, badge),
      desktop: desktopState(),
    })
    applyBadge()
  }

  function schedule(): void {
    stopTick?.()
    stopTick = scheduleTick(() => {
      if (closed) return
      void load('refreshing').catch(() => {})
    }, tickMillis)
  }

  return {
    get state() { return currentState },
    subscribe(listener) {
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async start() {
      await load('loading')
      if (!closed) schedule()
    },
    async refresh() {
      if (closed) return
      await load('refreshing')
    },
    applyBadge,
    async setDesktopEnabled(enabled) {
      if (closed || desktop === null) return
      if (!enabled) {
        desktopEnabled = false
        for (const tag of [...shownTags]) {
          shownTags.delete(tag)
          desktop.close(tag)
        }
        patch({ desktop: desktopState({ enabled: false }) })
        return
      }
      if (desktop.permission() !== 'granted') {
        const permission = await desktop.requestPermission()
        if (permission !== 'granted') {
          desktopEnabled = false
          patch({ desktop: desktopState({ permission, enabled: false, blocked: true }) })
          return
        }
      }
      // Consent never replays entries that already existed: only events that
      // arrive after the user opted in may notify.
      gate.admit(currentState.signals)
      desktopEnabled = true
      patch({ desktop: desktopState({ enabled: true, blocked: false }) })
    },
    close() {
      if (closed) return
      closed = true
      stopTick?.()
      stopTick = null
      gate.close()
      if (desktop !== null) {
        for (const tag of [...shownTags]) desktop.close(tag)
      }
      shownTags.clear()
      desktopEnabled = false
      publish({
        status: 'closed',
        signals: Object.freeze([]) as readonly AttentionSignal[],
        badge: attentionSignalBadge([]),
        titleText: titleBase,
        desktop: desktopState({ enabled: false }),
      })
      applyBadge()
      listeners.clear()
    },
  }
}
