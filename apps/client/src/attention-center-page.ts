// SPDX-License-Identifier: Apache-2.0

import type { ControlPlaneClientError } from './community-control-plane-client.js'
import { formatInstant } from './format-instant.js'
import {
  mountButton,
  mountErrorState,
  mountPageHeader,
  mountStatusBadge,
  type StatusTone,
} from '@winwincode/browser-ui'
import { mountEmptyState, mountToolbar } from './components/index.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import { boundApprovalText } from './approval-risk-detail.js'
import { scopeHash, surfaceHash, type ScopeRouteSelection } from '@winwincode/browser-core/scope-context'
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
      '待办列表超出有界查询上限。请先解决待决策事项，再刷新重试。',
    ATTENTION_CENTER_QUERY_MISMATCH: '服务端返回了意外结果。请刷新后重试。',
    ATTENTION_CENTER_PAGE_INVALID: '服务端返回了不一致的分页。请刷新后重试。',
    ATTENTION_CENTER_CURSOR_INVALID: '服务端返回了无效的续读游标。请刷新后重试。',
    ATTENTION_CENTER_APPROVAL_BINDING_INVALID:
      '审批列表不一致。请在处理条目前先刷新。',
  })
  return labels[error.code] ?? null
}

function errorLabel(error: ControlPlaneClientError | null): string | null {
  if (error === null) return null
  const known = knownCenterError(error)
  if (known !== null) return known
  if (error.kind === 'authentication') return '请重新登录后查看待我处理。'
  if (error.kind === 'authorization') return '你没有访问此待我处理页面的权限。'
  if (error.kind === 'network') return '无法连接服务端。请检查网络后重试。'
  if (error.kind === 'version') return '客户端与服务端版本不一致。请更新客户端后重试。'
  if (error.kind === 'cancelled') return '待我处理的更新已取消。'
  if (error.kind === 'configuration') return '请检查服务端地址与 Scope 配置后重试。'
  return '待我处理无法更新。请重试，或查看服务端状态。'
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
    ? '正在加载待我处理…'
    : state.status === 'refreshing' || state.realtime === 'reloading'
      ? '正在更新待我处理…'
      : state.realtime === 'reconnecting'
        ? '正在重新连接…'
        : state.status === 'authentication-required'
          ? '访问已撤销 · 请重新登录后加载待我处理'
          : state.status === 'authorization-denied'
            ? '访问被拒绝'
            : state.status === 'cancelled'
              ? '更新已取消'
              : state.status === 'error'
                ? '待我处理不可用'
                : state.status === 'closed'
                  ? '待我处理已关闭'
                  : `就绪 · 待决策 ${String(counts.needDecision)} 项 · 阻塞 ${
                    String(counts.blocking)} 项 · 已过期 ${String(counts.expired)} 项 · 绑定失效 ${
                    String(counts.bindingInvalid)} 项`
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
  input: '输入请求',
  approval: '工具审批',
  attention: '业务注意点',
})

/**
 * The one status line of design page 06, from the item's real kind and
 * urgency: Delivery-bound Attention entries await the hand-over, decisions
 * await the user, and fail-closed states keep their explicit labels.
 */
function itemStatusText(item: AttentionCenterItem): string {
  if (item.urgency === 'expired') return '已过期 · 操作禁用'
  if (item.urgency === 'binding-invalid') return '绑定失效 · 操作禁用'
  if (item.urgency === 'blocking') return '阻塞 · 需要立即决策'
  return item.kind === 'attention' ? '交付待验收' : '待决策'
}

/** Decision-class entries open the review, Delivery-bound ones the hand-over. */
function itemActionLabel(item: AttentionCenterItem): string {
  return item.kind === 'attention' ? '验收交付' : '审核方案'
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
    // The run page is the one authoritative Delivery/StageRun surface, so a
    // business Attention deep link goes through the same typed StrongFlow
    // route boundary every other run-page entry uses (parse and format stay
    // one canonical shape), instead of a hand-built query string.
    return strongFlowRouteHash({
      deliveryId: item.deliveryId,
      productSessionId: null,
      stageRunId: item.stageRunId,
      candidatePath: null,
      candidateView: 'unified',
      comparison: { status: 'none' },
      evidenceTab: 'evidence',
      evidenceId: null,
    }, scopeSelection)
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
      statusText: '此浏览器不支持桌面通知。',
      buttonLabel: null,
    }
  }
  if (state.blocked) {
    return {
      statusText: '桌面通知被浏览器阻止。请先在浏览器中允许通知。',
      buttonLabel: null,
    }
  }
  if (state.enabled) {
    return {
      statusText: '桌面通知已开启，需要你处理的条目会通知你。',
      buttonLabel: '关闭桌面通知',
    }
  }
  return {
    statusText: '桌面通知当前关闭。开启后，阻塞条目会通知你。',
    buttonLabel: '开启桌面通知',
  }
}

/** Mount the unified Attention Center: one filtered, ordered view of every pending decision. */
export function mountAttentionCenterPage(options: AttentionCenterPageOptions): AttentionCenterPage {
  const document = options.root.ownerDocument
  const layout = element(document, 'section', 'wwc-attention-center')
  layout.dataset.wwcPage = 'management'
  // Design page 06: back to the task board, then the counted title.
  const back = element(document, 'a', 'wwc-attention-center-back')
  back.href = surfaceHash('/home', options.scopeSelection)
  back.textContent = '返回任务看板'
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: '待我处理',
      headingLevel: 2,
      className: 'wwc-attention-center-heading',
    },
  })
  const heading = pageHeader.root
  const statusBadge = mountStatusBadge({
    document,
    props: {
      label: '正在加载待我处理…',
      tone: 'info',
      live: 'polite',
      className: 'wwc-attention-center-status',
    },
  })
  const status = statusBadge.root
  const refreshButton = mountButton({
    document,
    props: {
      label: '立即刷新',
      className: 'wwc-attention-center-refresh',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const refresh = refreshButton.root
  const retryButton = mountButton({
    document,
    props: {
      label: '重试快照',
      className: 'wwc-attention-center-retry',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const retry = retryButton.root
  const reconnectButton = mountButton({
    document,
    props: {
      label: '重新连接事件流',
      className: 'wwc-attention-center-reconnect',
      onActivate: () => { options.model.reconnect() },
    },
  })
  const reconnect = reconnectButton.root
  const errorState = mountErrorState({
    document,
    props: {
      title: '待我处理不可用',
      message: '',
      actions: [retry, reconnect],
      visible: false,
      className: 'wwc-attention-center-error',
    },
  })
  const error = errorState.root
  errorState.message.className = 'wwc-attention-center-error-text'

  // Design page 06: the Browse panel collapses into one compact control row
  // (type + sort) with the desktop-notification consent beside it.
  const controlsSection = element(document, 'div', 'wwc-attention-center-controls')
  const kindLabel = element(document, 'label', 'wwc-attention-center-control-label')
  const kindSelect = element(document, 'select', 'wwc-attention-center-kind')
  const sortLabel = element(document, 'label', 'wwc-attention-center-control-label')
  const sortSelect = element(document, 'select', 'wwc-attention-center-sort')
  kindSelect.id = 'wwc-attention-center-kind'
  kindLabel.htmlFor = kindSelect.id
  kindLabel.textContent = '类型'
  for (const [value, label] of [
    ['all', '全部待办'],
    ['input', '输入请求'],
    ['approval', '工具审批'],
    ['attention', '业务注意点'],
  ] as const) {
    const option = document.createElement('option')
    option.value = value
    option.textContent = label
    kindSelect.append(option)
  }
  sortSelect.id = 'wwc-attention-center-sort'
  sortLabel.htmlFor = sortSelect.id
  sortLabel.textContent = '排序'
  for (const [value, label] of [
    ['urgency', '按紧急度'],
    ['newest', '最新优先'],
    ['expiry', '最先到期'],
  ] as const) {
    const option = document.createElement('option')
    option.value = value
    option.textContent = label
    sortSelect.append(option)
  }
  const toolbar = mountToolbar({
    document,
    props: {
      label: '待我处理浏览控件',
      items: [kindLabel, kindSelect, sortLabel, sortSelect],
      className: 'wwc-attention-center-toolbar',
    },
  })
  controlsSection.append(toolbar.root)

  // UI-506: the browser notification permission is only ever requested from this
  // explicit control, and the state text stays a plain paragraph so the page
  // keeps exactly one polite live region.
  const desktopStatus = element(document, 'p', 'wwc-attention-center-desktop-status')
  desktopStatus.hidden = true
  const desktopToggle = mountButton({
    document,
    props: {
      label: '开启桌面通知',
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
  controlsSection.append(desktopStatus, desktopButton)

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
      label: presentation.buttonLabel ?? '开启桌面通知',
      className: 'wwc-attention-center-desktop-toggle',
      variant: 'default',
      onActivate: () => {
        void control.setDesktopEnabled(control.state.desktop.enabled !== true)
      },
    })
  }
  renderDesktopNotifications()
  const unsubscribeDesktop = options.notifications?.subscribe(renderDesktopNotifications) ?? null

  const itemsRoot = element(document, 'div', 'wwc-attention-center-items')
  const cards = element(document, 'ul', 'wwc-attention-center-list')
  const empty = mountEmptyState({
    document,
    props: {
      title: '暂无待处理事项',
      detail: '新的输入请求、工具审批与业务注意点会出现在这里。',
      className: 'wwc-attention-center-empty',
      headingLevel: 3,
    },
  })
  itemsRoot.append(cards, empty.root)

  // Design page 06: the handled archive collapses into one hairline row.  The
  // loaded snapshot carries only open and closed (expired/invalid) entries, so
  // the count stays at the honest zero until the Server exposes the archive.
  const handled = element(document, 'button', 'wwc-attention-center-handled')
  handled.type = 'button'
  handled.setAttribute('aria-expanded', 'false')
  handled.setAttribute('aria-controls', 'wwc-attention-center-handled-detail')
  const handledLabel = element(document, 'span', 'wwc-attention-center-handled-label')
  handledLabel.textContent = '已处理'
  const handledCount = element(document, 'span', 'wwc-attention-center-handled-count')
  handledCount.textContent = '0'
  handled.append(handledLabel, handledCount)
  const handledDetail = element(document, 'p', 'wwc-attention-center-handled-detail')
  handledDetail.id = 'wwc-attention-center-handled-detail'
  handledDetail.hidden = true
  handledDetail.textContent =
    '已处理条目由服务端归档；当前待办快照只包含待处理与已关闭（过期/绑定失效）条目。'
  handled.addEventListener('click', () => {
    const expanded = handled.getAttribute('aria-expanded') === 'true'
    handled.setAttribute('aria-expanded', expanded ? 'false' : 'true')
    handledDetail.hidden = expanded
  })

  layout.append(back, heading, status, refresh, error, controlsSection, itemsRoot, handled, handledDetail)
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
    readonly status: HTMLElement
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
      const status = element(document, 'p', 'wwc-attention-card-status')
      const context = cardContext(document, ['', '', '', ''])
      const origin = element(document, 'a', 'wwc-attention-card-origin')
      origin.hidden = true
      const action = element(document, 'a', 'wwc-attention-card-action')
      row.append(kind, title, status, context, origin, action)
      cardParts.set(row, { kind, title, status, context, origin, action })
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
      // Producer summaries are free-form, so the card never renders one raw.
      parts.title.textContent = boundApprovalText(item.title).text
      parts.status.textContent = itemStatusText(item)
      parts.origin.hidden = item.kind === 'attention' || origin === undefined || disabled
      if (origin !== undefined && cardStageRunId !== null && item.kind !== 'attention' && !disabled) {
        parts.origin.href = attentionCenterOriginHash(
          origin,
          cardStageRunId,
          options.scopeSelection,
        )
        parts.origin.textContent = '打开执行上下文'
      } else {
        parts.origin.removeAttribute('href')
        parts.origin.textContent = ''
      }
      // Absent facts and internal binding bookkeeping are omitted; the row
      // shows the status line, human times, and its source context only.
      updateCardContext(parts.context, [
        item.createdAt === null ? null : `创建于 ${formatInstant(item.createdAt)}`,
        item.expiresAt === null ? null : `过期于 ${formatInstant(item.expiresAt)}`,
        item.kind === 'attention'
          ? (item.deliveryTitle === null ? null : `交付 · ${item.deliveryTitle}`)
          : (item.sessionTitle === null ? null : `会话 · ${item.sessionTitle}`),
      ].filter((entry): entry is string => entry !== null))
      parts.action.textContent = itemActionLabel(item)
      if (disabled) {
        parts.action.removeAttribute('href')
        parts.action.setAttribute('aria-disabled', 'true')
        parts.action.tabIndex = -1
        parts.action.title = '该条目已禁用。请刷新查看当前状态。'
      } else {
        parts.action.href = attentionCenterItemHash(item, options.scopeSelection, origins())
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
    pageHeader.update({
      title: `待我处理 ${String(state.items.length)} 项`,
      headingLevel: 2,
      className: 'wwc-attention-center-heading',
    })
    statusBadge.update({
      label: presentation.statusText,
      tone,
      live: 'polite',
      className: 'wwc-attention-center-status',
    })
    layout.setAttribute('aria-busy', String(presentation.busy))
    errorState.update({
      title: '待我处理不可用',
      message: presentation.errorText ?? '',
      actions: [retry, reconnect],
      visible: presentation.errorText !== null,
      className: 'wwc-attention-center-error',
    })
    retry.hidden = !presentation.retryVisible
    reconnect.hidden = !presentation.reconnectVisible
    refreshButton.update({
      label: '立即刷新',
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
