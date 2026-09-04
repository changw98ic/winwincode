// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientError } from './control-plane-client.js'
import {
  mountButton,
  mountEmptyState,
  mountErrorState,
  mountPageHeader,
  mountPanel,
  mountStatusBadge,
  mountToolbar,
  type StatusTone,
} from './components/index.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import { scopeHash, type ScopeRouteSelection } from './core/scope-context.js'
import type { StageRunId } from './generated/contracts.js'
import type {
  AttentionNotificationControl,
  AttentionNotificationDesktopState,
} from './attention-notifications.js'
import type {
  AttentionCenterItem,
  AttentionCenterItemKind,
  AttentionCenterOrigin,
  AttentionCenterViewModel,
  AttentionCenterViewModelState,
} from './attention-center-view-model.js'
import { orderedAttentionCenterItems } from './attention-center-view-model.js'
import { strongFlowRouteHash, type StrongFlowRoute } from './strongflow-route.js'

export interface AttentionCenterPageOptions {
  readonly root: HTMLElement
  readonly model: AttentionCenterViewModel
  /** Current exact Scope path used to build every source-context entry link. */
  readonly scopeSelection: ScopeRouteSelection
  /**
   * Lifecycle ownership: `true` (the default composition) lets the page close
   * the model it mounted; a host that shares this model passes `false` and
   * closes it itself.
   */
  readonly ownsModel: boolean
  /** Shell-owned desktop notification control; absent when the shell is closed. */
  readonly notifications?: AttentionNotificationControl
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
}

export interface AttentionCenterPage {
  close(): void
}

export type AttentionCenterKindFilter = 'all' | AttentionCenterItemKind
export type AttentionCenterSort = 'urgency' | 'newest' | 'expiry'

export interface AttentionCenterSelection {
  readonly kind: AttentionCenterKindFilter
  readonly sort: AttentionCenterSort
}

export interface AttentionCenterPresentation {
  readonly statusText: string
  readonly errorText: string | null
  readonly busy: boolean
  readonly retryVisible: boolean
  readonly reconnectVisible: boolean
  readonly actionsDisabled: boolean
  readonly counts: {
    readonly needDecision: number
    readonly blocking: number
    readonly expired: number
    readonly bindingInvalid: number
  }
}

function knownCenterError(error: ControlPlaneClientError): string | null {
  const labels: Readonly<Record<string, string>> = Object.freeze({
    ATTENTION_CENTER_PAGE_LIMIT_EXCEEDED:
      'The pending list exceeded the bounded query limit. Resolve open decisions and refresh.',
    ATTENTION_CENTER_QUERY_MISMATCH: 'The server returned an unexpected answer. Refresh and retry.',
    ATTENTION_CENTER_PAGE_INVALID: 'The server returned an inconsistent page. Refresh and retry.',
    ATTENTION_CENTER_CURSOR_INVALID: 'The server returned an invalid continuation. Refresh and retry.',
    ATTENTION_CENTER_APPROVAL_BINDING_INVALID:
      'The Approval list is inconsistent. Refresh before acting on an entry.',
  })
  return labels[error.code] ?? null
}

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
  const known = knownCenterError(error)
  if (known !== null) return known
  if (error.kind === 'authentication') return 'Sign in again to review the Attention Center.'
  if (error.kind === 'authorization') return 'You do not have access to this Attention Center.'
  if (error.kind === 'network') return 'The server could not be reached. Check the connection and retry.'
  if (error.kind === 'version') return 'The Client and Server versions differ. Update the Client and retry.'
  if (error.kind === 'cancelled') return 'The Attention Center update was cancelled.'
  if (error.kind === 'configuration') return 'Check the server URL and Scope configuration, then retry.'
  return 'The Attention Center could not be updated. Retry, or review the server status.'
}

function centerCounts(items: readonly AttentionCenterItem[]): AttentionCenterPresentation['counts'] {
  return Object.freeze({
    needDecision: items.filter(item => item.kind !== 'attention' && item.urgency === 'pending').length,
    blocking: items.filter(item => item.urgency === 'blocking').length,
    expired: items.filter(item => item.urgency === 'expired').length,
    bindingInvalid: items.filter(item => item.urgency === 'binding-invalid').length,
  })
}

/** Browse the loaded snapshot: filter by kind, then order by the chosen ranking. */
export function selectAttentionCenterItems(
  state: AttentionCenterViewModelState,
  selection: AttentionCenterSelection,
): readonly AttentionCenterItem[] {
  const filtered = selection.kind === 'all'
    ? state.items
    : state.items.filter(item => item.kind === selection.kind)
  if (selection.sort === 'newest') {
    return Object.freeze([...filtered].sort((left, right) => {
      const leftCreated = left.createdAt === null
        ? Number.NEGATIVE_INFINITY
        : Date.parse(left.createdAt)
      const rightCreated = right.createdAt === null
        ? Number.NEGATIVE_INFINITY
        : Date.parse(right.createdAt)
      if (leftCreated !== rightCreated) return rightCreated - leftCreated
      return left.id.localeCompare(right.id)
    }))
  }
  if (selection.sort === 'expiry') {
    return Object.freeze([...filtered].sort((left, right) => {
      const leftExpiry = left.expiresAt === null ? Number.POSITIVE_INFINITY : Date.parse(left.expiresAt)
      const rightExpiry = right.expiresAt === null
        ? Number.POSITIVE_INFINITY
        : Date.parse(right.expiresAt)
      if (leftExpiry !== rightExpiry) return leftExpiry - rightExpiry
      return left.id.localeCompare(right.id)
    }))
  }
  return orderedAttentionCenterItems(filtered)
}

export function attentionCenterPresentation(
  state: AttentionCenterViewModelState,
  selection: AttentionCenterSelection,
): AttentionCenterPresentation {
  const counts = centerCounts(state.items)
  const statusText = state.status === 'loading'
    ? 'Loading the Attention Center…'
    : state.status === 'refreshing' || state.realtime === 'reloading'
      ? 'Updating the Attention Center…'
      : state.realtime === 'reconnecting'
        ? 'Reconnecting…'
        : state.status === 'authentication-required'
          ? 'Access revoked · sign in to load the Attention Center'
          : state.status === 'authorization-denied'
            ? 'Access denied'
            : state.status === 'cancelled'
              ? 'Update cancelled'
              : state.status === 'error'
                ? 'Attention Center unavailable'
                : state.status === 'closed'
                  ? 'Attention Center closed'
                  : `Ready · ${String(counts.needDecision)} need a decision · ${
                    String(counts.blocking)} blocking · ${String(counts.expired)} expired · ${
                    String(counts.bindingInvalid)} binding invalid`
  const busy = state.status === 'loading'
    || state.status === 'refreshing'
    || state.realtime === 'reloading'
    || state.realtime === 'reconnecting'
  const errorText = errorLabel(state.error)
  const actionsDisabled = state.status === 'authentication-required'
    || state.status === 'authorization-denied'
    || state.status === 'closed'
  return Object.freeze({
    statusText,
    errorText,
    busy,
    // Revoked access permits no further read from page controls until remount.
    retryVisible: errorText !== null && !actionsDisabled,
    reconnectVisible: state.realtime === 'reconnecting',
    actionsDisabled,
    counts,
  })
}

const KIND_LABELS: Readonly<Record<AttentionCenterItemKind, string>> = Object.freeze({
  input: 'Input',
  approval: 'Tool approval',
  attention: 'Business Attention',
})

function urgencyLabel(urgency: AttentionCenterItem['urgency']): string {
  if (urgency === 'blocking') return 'Blocking · needs a decision now'
  if (urgency === 'pending') return 'Needs a decision'
  if (urgency === 'expired') return 'Expired · action disabled'
  return 'Binding invalid · action disabled'
}

/**
 * The route facts of one card.  The full Attention item satisfies this subset,
 * so the Attention Center and each composed surface (UI-504 Home) build the same
 * authoritative decision link from the same function.
 */
export type AttentionCenterItemRoute = Pick<
  AttentionCenterItem,
  'kind' | 'id' | 'productSessionId' | 'stageRunId' | 'deliveryId'
>

/** Real entry point for one card: the authoritative decision or Delivery surface. */
export function attentionCenterItemHash(
  item: AttentionCenterItemRoute,
  scopeSelection: ScopeRouteSelection,
  origins?: readonly AttentionCenterOrigin[],
): string {
  if (item.kind === 'attention') {
    const hash = `#/strongflow?delivery=${encodeURIComponent(item.deliveryId ?? '')}`
      + (item.stageRunId === null ? '' : `&stageRun=${encodeURIComponent(item.stageRunId)}`)
    return scopeHash(hash, scopeSelection)
  }
  // The decision link carries the exact execution origin so the decision
  // surface can return to the Task/StageRun that raised the decision.
  const stageRunId = item.stageRunId
  const origin = stageRunId === null
    ? undefined
    : origins?.find(candidate => candidate.activeStageRunId === stageRunId)
  const parameters = [`session=${encodeURIComponent(item.productSessionId ?? '')}`]
  if (origin !== undefined && stageRunId !== null) {
    parameters.push(
      `delivery=${encodeURIComponent(origin.deliveryId)}`,
      `stageRun=${encodeURIComponent(stageRunId)}`,
    )
  }
  return scopeHash(`#/attention?${parameters.join('&')}`, scopeSelection)
}

/** The exact StrongFlow context of the StageRun that raised one decision. */
export function attentionCenterOriginHash(
  origin: AttentionCenterOrigin,
  stageRunId: StageRunId,
  scopeSelection: ScopeRouteSelection,
): string {
  const route: StrongFlowRoute = {
    deliveryId: origin.deliveryId,
    productSessionId: null,
    stageRunId,
    candidatePath: null,
    candidateView: 'unified',
    comparison: { status: 'none' },
    evidenceTab: 'evidence',
    evidenceId: null,
  }
  return strongFlowRouteHash(route, scopeSelection)
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

function cardContext(document: Document, entries: readonly string[]): HTMLUListElement {
  const list = element(document, 'ul', 'wwc-attention-card-context')
  for (const entry of entries) {
    const item = document.createElement('li')
    item.textContent = entry
    list.append(item)
  }
  return list
}

function updateCardContext(list: HTMLUListElement, entries: readonly string[]): void {
  entries.forEach((entry, index) => {
    const item = list.children[index]
    if (item !== undefined && item.textContent !== entry) item.textContent = entry
  })
}

interface DesktopPresentation {
  readonly statusText: string
  readonly buttonLabel: string | null
}

/**
 * UI-506 desktop notifications stay off until the user turns them on here, so
 * the browser permission prompt is always an explicit user action.
 */
function desktopPresentation(
  state: AttentionNotificationDesktopState,
): DesktopPresentation {
  if (!state.supported) {
    return {
      statusText: 'Desktop notifications are not available in this browser.',
      buttonLabel: null,
    }
  }
  if (state.blocked) {
    return {
      statusText: 'Desktop notifications are blocked. Allow them in the browser to enable.',
      buttonLabel: null,
    }
  }
  if (state.enabled) {
    return {
      statusText: 'Desktop notifications are on for entries that need you.',
      buttonLabel: 'Turn off desktop notifications',
    }
  }
  return {
    statusText: 'Desktop notifications are off. Turn them on to hear about blocking entries.',
    buttonLabel: 'Turn on desktop notifications',
  }
}

/** Mount the unified Attention Center: one filtered, ordered view of every pending decision. */
export function mountAttentionCenterPage(options: AttentionCenterPageOptions): AttentionCenterPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'section', 'wwc-attention-center')
  layout.dataset.wwcPage = 'management'
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: 'Attention Center',
      eyebrow: 'Every pending decision',
      description: 'Inputs, tool approvals, and business Attention across the current repository Scope.',
      headingLevel: 2,
      className: 'wwc-attention-center-heading',
    },
  })
  const heading = pageHeader.root
  const statusBadge = mountStatusBadge({
    document,
    props: {
      label: 'Loading the Attention Center…',
      tone: 'info',
      live: 'polite',
      className: 'wwc-attention-center-status',
    },
  })
  const status = statusBadge.root
  const refreshButton = mountButton({
    document,
    props: {
      label: 'Refresh now',
      className: 'wwc-attention-center-refresh',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const refresh = refreshButton.root
  const retryButton = mountButton({
    document,
    props: {
      label: 'Retry snapshot',
      className: 'wwc-attention-center-retry',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const retry = retryButton.root
  const reconnectButton = mountButton({
    document,
    props: {
      label: 'Reconnect events',
      className: 'wwc-attention-center-reconnect',
      onActivate: () => { options.model.reconnect() },
    },
  })
  const reconnect = reconnectButton.root
  const errorState = mountErrorState({
    document,
    props: {
      title: 'Attention Center unavailable',
      message: '',
      actions: [retry, reconnect],
      visible: false,
      className: 'wwc-attention-center-error',
    },
  })
  const error = errorState.root
  errorState.message.className = 'wwc-attention-center-error-text'

  const controlsPanel = mountPanel({
    document,
    props: {
      id: 'wwc-attention-center-controls',
      headingLevel: 3,
      title: 'Browse',
      description: 'Filter by type and order by urgency, newest, or soonest expiry.',
      className: 'wwc-attention-center-controls',
    },
  })
  const controlsSection = controlsPanel.root
  const kindLabel = element(document, 'label', 'wwc-attention-center-control-label')
  const kindSelect = element(document, 'select', 'wwc-attention-center-kind')
  const sortLabel = element(document, 'label', 'wwc-attention-center-control-label')
  const sortSelect = element(document, 'select', 'wwc-attention-center-sort')
  kindSelect.id = 'wwc-attention-center-kind'
  kindLabel.htmlFor = kindSelect.id
  kindLabel.textContent = 'Type'
  for (const [value, label] of [
    ['all', 'All pending items'],
    ['input', 'Inputs'],
    ['approval', 'Tool approvals'],
    ['attention', 'Business Attention'],
  ] as const) {
    const option = document.createElement('option')
    option.value = value
    option.textContent = label
    kindSelect.append(option)
  }
  sortSelect.id = 'wwc-attention-center-sort'
  sortLabel.htmlFor = sortSelect.id
  sortLabel.textContent = 'Order'
  for (const [value, label] of [
    ['urgency', 'Urgency'],
    ['newest', 'Newest first'],
    ['expiry', 'Soonest expiry'],
  ] as const) {
    const option = document.createElement('option')
    option.value = value
    option.textContent = label
    sortSelect.append(option)
  }
  const toolbar = mountToolbar({
    document,
    props: {
      label: 'Attention Center browsing controls',
      items: [kindLabel, kindSelect, sortLabel, sortSelect],
      className: 'wwc-attention-center-toolbar',
    },
  })
  controlsPanel.content.append(toolbar.root)

  // UI-506: the browser notification permission is only ever requested from this
  // explicit control, and the state text stays a plain paragraph so the page
  // keeps exactly one polite live region.
  const desktopStatus = element(document, 'p', 'wwc-attention-center-desktop-status')
  desktopStatus.hidden = true
  const desktopToggle = mountButton({
    document,
    props: {
      label: 'Turn on desktop notifications',
      className: 'wwc-attention-center-desktop-toggle',
      variant: 'default',
      onActivate: () => {
        const control = options.notifications
        if (control === undefined) return
        void control.setDesktopEnabled(control.state.desktop.enabled !== true)
      },
    },
  })
  const desktopButton = desktopToggle.root
  desktopButton.hidden = true
  controlsPanel.content.append(desktopStatus, desktopButton)

  function renderDesktopNotifications(): void {
    const control = options.notifications
    if (control === undefined) {
      desktopStatus.hidden = true
      desktopButton.hidden = true
      return
    }
    const presentation = desktopPresentation(control.state.desktop)
    desktopStatus.hidden = false
    desktopStatus.textContent = presentation.statusText
    desktopButton.hidden = presentation.buttonLabel === null
    desktopToggle.update({
      label: presentation.buttonLabel ?? 'Turn on desktop notifications',
      className: 'wwc-attention-center-desktop-toggle',
      variant: 'default',
      onActivate: () => {
        void control.setDesktopEnabled(control.state.desktop.enabled !== true)
      },
    })
  }
  renderDesktopNotifications()
  const unsubscribeDesktop = options.notifications?.subscribe(renderDesktopNotifications) ?? null

  const listPanel = mountPanel({
    document,
    props: {
      id: 'wwc-attention-center-items',
      headingLevel: 3,
      title: 'Pending items',
      description: 'Each card opens its authoritative decision or Delivery context.',
      className: 'wwc-attention-center-items',
    },
  })
  const listSection = listPanel.root
  const cards = element(document, 'ul', 'wwc-attention-center-list')
  const empty = mountEmptyState({
    document,
    props: {
      title: 'Nothing needs a decision',
      detail: 'New inputs, tool approvals, and business Attention will appear here.',
      className: 'wwc-attention-center-empty',
      headingLevel: 3,
    },
  })
  listPanel.content.append(cards, empty.root)

  layout.append(heading, status, refresh, error, controlsSection, listSection)
  options.root.replaceChildren(layout)

  let closed = false
  let selection: AttentionCenterSelection = Object.freeze({ kind: 'all', sort: 'urgency' })
  const onKindChange = () => {
    const value = kindSelect.value as AttentionCenterKindFilter
    selection = Object.freeze({ ...selection, kind: value })
    render(options.model.state)
  }
  const onSortChange = () => {
    const value = sortSelect.value as AttentionCenterSort
    selection = Object.freeze({ kind: selection.kind, sort: value })
    render(options.model.state)
  }
  kindSelect.addEventListener('change', onKindChange)
  sortSelect.addEventListener('change', onSortChange)

  interface CardParts {
    readonly kind: HTMLElement
    readonly title: HTMLElement
    readonly context: HTMLUListElement
    readonly origin: HTMLAnchorElement
    readonly action: HTMLAnchorElement
  }
  const cardParts = new WeakMap<HTMLLIElement, CardParts>()
  const origins = (): readonly AttentionCenterOrigin[] => options.model.state.origins
  const cardCollection: KeyedCollectionView<AttentionCenterItem, string, HTMLLIElement> = mountKeyedCollection({
    parent: cards,
    key: item => `${item.kind}:${item.id}`,
    create(item: AttentionCenterItem) {
      const row = element(document, 'li', 'wwc-attention-card')
      const kind = element(document, 'span', 'wwc-attention-card-kind')
      const title = element(document, 'h4', 'wwc-attention-card-title')
      const context = cardContext(document, ['', '', '', '', '', '', '', ''])
      const origin = element(document, 'a', 'wwc-attention-card-origin')
      origin.hidden = true
      const action = element(document, 'a', 'wwc-attention-card-action')
      row.append(kind, title, context, origin, action)
      cardParts.set(row, { kind, title, context, origin, action })
      return row
    },
    update(row, item) {
      const parts = cardParts.get(row)
      if (parts === undefined) return
      const disabled = options.readOnly === true
        || attentionCenterPresentation(options.model.state, selection).actionsDisabled
        || item.urgency === 'expired'
        || item.urgency === 'binding-invalid'
      const cardStageRunId = item.stageRunId
      const origin = cardStageRunId === null
        ? undefined
        : origins().find(candidate => candidate.activeStageRunId === cardStageRunId)
      row.dataset.kind = item.kind
      row.dataset.urgency = item.urgency
      parts.kind.textContent = KIND_LABELS[item.kind]
      parts.title.textContent = item.title
      parts.origin.hidden = item.kind === 'attention' || origin === undefined || disabled
      if (origin !== undefined && cardStageRunId !== null && item.kind !== 'attention' && !disabled) {
        parts.origin.href = attentionCenterOriginHash(
          origin,
          cardStageRunId,
          options.scopeSelection,
        )
        parts.origin.textContent = 'Open execution context'
      } else {
        parts.origin.removeAttribute('href')
        parts.origin.textContent = ''
      }
      updateCardContext(parts.context, [
        urgencyLabel(item.urgency),
        item.createdAt === null ? 'Created · not reported' : `Created ${item.createdAt}`,
        item.expiresAt === null ? 'No expiry deadline' : `Expires ${item.expiresAt}`,
        item.kind === 'attention'
          ? `Delivery · ${item.deliveryTitle ?? 'unknown'}`
          : `Session · ${item.sessionTitle ?? 'unknown'}`,
        item.kind === 'attention'
          ? (item.stageRunId === null ? 'Delivery-bound' : 'StageRun-bound')
          : (item.stageRunId === null ? 'ProductSession-bound' : 'ProductSession and StageRun-bound'),
        item.executionJobId === null ? 'No execution job' : 'Execution job-bound',
        // The Attention snapshot carries no authoritative DeliveryTask mapping, so the
        // Task field is explicitly unavailable; an ExecutionJob is never shown as a Task.
        'Task · Unavailable',
        item.kind === 'attention'
          ? (item.candidateBound ? 'Candidate · bound' : 'Candidate · not bound')
          : 'Candidate · not reported',
      ])
      parts.action.textContent = item.kind === 'attention'
        ? 'Open delivery context'
        : 'Open decisions'
      if (disabled) {
        parts.action.removeAttribute('href')
        parts.action.setAttribute('aria-disabled', 'true')
        parts.action.tabIndex = -1
        parts.action.title = 'This entry is disabled. Refresh for the current state.'
      } else {
        parts.action.href = attentionCenterItemHash(item, options.scopeSelection)
        parts.action.removeAttribute('aria-disabled')
        parts.action.tabIndex = 0
        parts.action.title = ''
      }
    },
    remove(row) {
      const parts = cardParts.get(row)
      if (parts !== undefined) {
        parts.action.removeAttribute('href')
        parts.origin.removeAttribute('href')
        cardParts.delete(row)
      }
    },
  })

  function render(state: AttentionCenterViewModelState): void {
    if (closed) return
    const presentation = attentionCenterPresentation(state, selection)
    const tone: StatusTone = presentation.errorText !== null
      ? 'danger'
      : state.realtime === 'reconnecting'
        ? 'warning'
        : presentation.busy
          ? 'info'
          : state.status === 'ready'
            ? 'success'
            : 'neutral'
    statusBadge.update({
      label: presentation.statusText,
      tone,
      live: 'polite',
      className: 'wwc-attention-center-status',
    })
    layout.setAttribute('aria-busy', String(presentation.busy))
    errorState.update({
      title: 'Attention Center unavailable',
      message: presentation.errorText ?? '',
      actions: [retry, reconnect],
      visible: presentation.errorText !== null,
      className: 'wwc-attention-center-error',
    })
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    refreshButton.update({
      label: 'Refresh now',
      className: 'wwc-attention-center-refresh',
      onActivate: () => { void options.model.refresh() },
      disabled: presentation.actionsDisabled,
    })
    const visible = selectAttentionCenterItems(state, selection)
    cardCollection.update(visible)
    cards.hidden = visible.length === 0
    empty.root.hidden = visible.length !== 0
  }

  const unsubscribe = options.model.subscribe(render)
  void options.model.start()
  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      unsubscribeDesktop?.()
      kindSelect.removeEventListener('change', onKindChange)
      sortSelect.removeEventListener('change', onSortChange)
      cardCollection.close()
      empty.close()
      listPanel.close()
      controlsPanel.close()
      toolbar.close()
      errorState.close()
      refreshButton.close()
      reconnectButton.close()
      retryButton.close()
      statusBadge.close()
      pageHeader.close()
      options.root.replaceChildren()
      if (options.ownsModel) options.model.close()
    },
  }
}
