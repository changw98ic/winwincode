// SPDX-License-Identifier: Apache-2.0

import { formatInstant } from './format-instant.js'
import {
  mountPageHeader,
  mountStatusBadge,
  type StatusTone,
} from '@winwincode/browser-ui'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import { scopeHash, surfaceHash, type ScopeRouteSelection } from '@winwincode/browser-core/scope-context'
import type { Instant, ProductSessionId, WorkItemState } from './generated/contracts.js'
import { WorkItemState as WorkItemStateVocabulary } from './generated/contracts.js'
import type {
  HomeDashboardSource,
  HomeDashboardState,
  HomeDashboardStatus,
  HomeDashboardViewModel,
  HomeDecisionCard,
  HomeDeliveryCard,
  HomeVisitedCard,
} from './home-dashboard-view-model.js'

export type HomeSectionId =
  | 'decisions'
  | 'backlog'
  | 'running'
  | 'ready'
  | 'waiting'
  | 'validating'
  | 'failed'
  | 'completed'
  | 'visited'

/** One card as the dashboard renders it: a decision or a Delivery. */
export type HomeCard = HomeDecisionCard | HomeDeliveryCard | HomeVisitedCard

export interface HomeDashboardPresentation {
  readonly title: string
  readonly projectSelectLabel: string
  readonly projectSelectTitle: string
  readonly allProjectsLabel: string
  readonly currentScopeLabel: string
  readonly newTaskLabel: string
  readonly attentionOnlyLabel: string
  readonly statusLabel: Readonly<Record<HomeDashboardStatus, string>>
  readonly partialNote: string
  readonly errorNote: string
  readonly unavailableLabel: string
  readonly sourceLabel: Readonly<Record<HomeDashboardSource, string>>
  readonly sectionHeading: Readonly<Record<HomeSectionId, string>>
  readonly sectionEmpty: Readonly<Record<HomeSectionId, string>>
  /** The collapsed history rows of design page 04 (failing/completed). */
  readonly collapsibleSections: readonly HomeSectionId[]
  readonly expandLabel: string
  readonly collapseLabel: string
  readonly strongFlowLabel: string
  readonly deliveryStatusText: Readonly<Record<WorkItemState, string>>
  readonly planReviewPendingLabel: string
  readonly deliveryAcceptancePendingLabel: string
  readonly decisionUrgencyText: Readonly<Record<'expired' | 'binding-invalid', string>>
  readonly blockingDecisionLabel: string
  readonly reviewPlanLabel: string
  readonly acceptDeliveryLabel: string
  readonly openChatLabel: string
  readonly deliveryProgressLabel: string
  readonly disabledLabel: string
  readonly countLabel: (count: number) => string
  readonly updatedLabel: (at: Instant) => string
  readonly taskLabel: (card: Pick<
    HomeDeliveryCard,
    'activeTasks' | 'verifyingTasks' | 'failedTasks' | 'blockedTasks' | 'completedTasks'
  >) => string
  readonly visitedLabel: (at: string) => string
}

const PRESENTATION_SPEC: HomeDashboardPresentation = {
  title: '任务看板',
  projectSelectLabel: '项目范围（展示）',
  projectSelectTitle: '看板已限定当前仓库 Scope;项目筛选为展示控件。',
  allProjectsLabel: '全部项目',
  currentScopeLabel: '当前仓库',
  newTaskLabel: '新建任务',
  attentionOnlyLabel: '仅看待处理',
  statusLabel: Object.freeze({
    loading: '正在读取看板…',
    ready: '就绪',
    partial: '就绪（部分缺省）',
    error: '看板读取失败',
    closed: '看板已关闭',
  }),
  partialNote: '此范围内部分投影不可用。',
  errorNote: '重试看板。',
  unavailableLabel: '不可用',
  sourceLabel: Object.freeze({
    delivery: '交付列表',
    attention: '待办',
    usage: '用量与健康',
  }),
  sectionHeading: Object.freeze({
    decisions: '待我处理',
    backlog: '待拆分（Backlog）',
    ready: '待启动（Ready）',
    running: '运行中（Running）',
    waiting: '等待中（Waiting）',
    validating: '验证中（Validating）',
    failed: '失败或阻塞',
    completed: '已完成（Done）',
    visited: '最近访问',
  }),
  sectionEmpty: Object.freeze({
    decisions: '现在没有需要决策的事项。',
    backlog: '没有待拆分的交付。',
    running: '没有进行中的交付。',
    ready: '没有待启动的交付。',
    waiting: '没有等待中的交付。',
    validating: '没有待验证或验证中的交付。',
    failed: '没有失败或阻塞的交付。',
    completed: '还没有已完成的交付。',
    visited: '此浏览器还没有打开过交付。',
  }),
  // 设计稿 04:两列实时卡(正在运行/待我处理),其余区块是折叠的单行历史。
  collapsibleSections: Object.freeze([
    'backlog',
    'ready',
    'waiting',
    'validating',
    'failed',
    'completed',
    'visited',
  ]),
  expandLabel: '展开',
  collapseLabel: '收起',
  strongFlowLabel: '强流程',
  deliveryStatusText: Object.freeze({
    [WorkItemStateVocabulary.Backlog]: '待拆分',
    [WorkItemStateVocabulary.Ready]: '待启动',
    [WorkItemStateVocabulary.InProgress]: '正在执行',
    [WorkItemStateVocabulary.WaitingDependency]: '等待依赖',
    [WorkItemStateVocabulary.WaitingHuman]: '等待处理',
    [WorkItemStateVocabulary.CandidateReady]: '候选结果已就绪',
    [WorkItemStateVocabulary.Validating]: '正在验证',
    [WorkItemStateVocabulary.Rework]: '正在修复',
    [WorkItemStateVocabulary.Done]: '已完成',
    [WorkItemStateVocabulary.Failed]: '失败',
    [WorkItemStateVocabulary.Cancelled]: '已取消',
  }),
  planReviewPendingLabel: '方案待审核',
  deliveryAcceptancePendingLabel: '交付待验收',
  decisionUrgencyText: Object.freeze({
    expired: '已过期 · 操作禁用',
    'binding-invalid': '绑定失效 · 操作禁用',
  }),
  blockingDecisionLabel: '阻塞 · 需要立即决策',
  reviewPlanLabel: '审核方案',
  acceptDeliveryLabel: '验收交付',
  openChatLabel: '打开对话',
  deliveryProgressLabel: '查看进度',
  disabledLabel: '该决策已关闭。请刷新查看当前状态。',
  countLabel: count => `${String(count)}`,
  updatedLabel: at => `更新于 ${formatInstant(at)}`,
  visitedLabel: (at: string) => `访问于 ${at}`,
  taskLabel: card => [
    card.failedTasks > 0 ? `${String(card.failedTasks)} 个失败` : null,
    card.blockedTasks > 0 ? `${String(card.blockedTasks)} 个阻塞` : null,
  ].filter((entry): entry is string => entry !== null).join(' · '),
}

const PRESENTATION: HomeDashboardPresentation = Object.freeze(PRESENTATION_SPEC)

export function homeDashboardPresentation(): HomeDashboardPresentation {
  return PRESENTATION
}

/** The one polite announcement for the whole dashboard: counts first, then gaps. */
export function homeDashboardAnnouncement(state: HomeDashboardState): string {
  if (state.status === 'error') {
    return `${PRESENTATION.statusLabel.error} · ${PRESENTATION.errorNote}`
  }
  if (state.status === 'loading' || state.status === 'closed') {
    return PRESENTATION.statusLabel[state.status]
  }
  const counts = state.counts
  const summary = [
    `${String(counts.decisions)} 项待决策`,
    `${String(counts.backlog)} 个待拆分`,
    `${String(counts.running)} 个运行中`,
    `${String(counts.ready)} 个待启动`,
    `${String(counts.waiting)} 个等待中`,
    `${String(counts.validating)} 个验证中`,
    `${String(counts.failed)} 个失败或阻塞`,
    `${String(counts.completed)} 个已完成`,
  ].join(' · ')
  return state.status === 'partial'
    ? `${PRESENTATION.statusLabel.partial} · ${summary} · ${PRESENTATION.partialNote}`
    : `${PRESENTATION.statusLabel.ready} · ${summary}`
}

/**
 * The exact Chat session one decision came from.
 */
export function homeChatHash(
  productSessionId: ProductSessionId,
  scopeSelection: ScopeRouteSelection,
): string {
  return scopeHash(`#/chat?session=${encodeURIComponent(productSessionId)}`, scopeSelection)
}

/**
 * The authoritative target of one decision card: an input or approval opens
 * its Chat session; Delivery Attention opens the existing StrongFlow review.
 */
export function homeDecisionHash(
  card: HomeDecisionCard,
  scopeSelection: ScopeRouteSelection,
): string | null {
  if (card.kind === 'attention') {
    return card.deliveryId === null
      ? null
      : scopeHash(`#/home/review?delivery=${encodeURIComponent(card.deliveryId)}`, scopeSelection)
  }
  if (card.productSessionId === null) return null
  return homeChatHash(card.productSessionId, scopeSelection)
}

/** Native capability notice; the station inbox remains usable without Push or PWA. */
export function mobileWebCapabilityText(view: Window | null): string {
  const navigator = view?.navigator as (Navigator & { readonly standalone?: boolean }) | undefined
  const installed = navigator?.standalone === true
    || view?.matchMedia?.('(display-mode: standalone)').matches === true
  const pwa = installed
    ? '已在独立 PWA 窗口中运行。'
    : navigator !== undefined && 'serviceWorker' in navigator
      ? '浏览器支持 PWA 基础能力；当前会话仍需联网。'
      : '此浏览器不支持 PWA；仍可使用移动网页。'
  const permission = (view as (Window & {
    readonly Notification?: { readonly permission?: NotificationPermission }
  }) | null)?.Notification?.permission
  const notification = permission === 'granted'
    ? '系统通知已启用，站内待处理入口仍是权威入口。'
    : permission === 'denied'
      ? '系统通知已被阻止；站内待处理入口仍可处理任务。'
      : permission === 'default'
        ? '系统通知尚未授权；站内待处理入口仍可处理任务。'
        : '系统通知不可用；站内待处理入口仍可处理任务。'
  return `${pwa} ${notification}`
}

/** The one status line of a pending-decision card, from its real kind/urgency. */
export function homeDecisionStatusText(card: HomeDecisionCard): string {
  if (card.actionDisabled) {
    return PRESENTATION.decisionUrgencyText[card.urgency === 'expired' ? 'expired' : 'binding-invalid']
  }
  if (card.urgency === 'blocking') return PRESENTATION.blockingDecisionLabel
  return card.kind === 'attention'
    ? PRESENTATION.deliveryAcceptancePendingLabel
    : PRESENTATION.planReviewPendingLabel
}

/** Decision-class entries open the plan review, Delivery-bound ones the hand-over. */
export function homeDecisionActionLabel(card: HomeDecisionCard): string {
  return card.kind === 'attention'
    ? PRESENTATION.acceptDeliveryLabel
    : PRESENTATION.reviewPlanLabel
}

export interface HomeDashboardPageOptions {
  readonly root: HTMLElement
  readonly model: HomeDashboardViewModel
  /** The exact Scope path prefixed onto every deep link on the dashboard. */
  readonly scopeSelection: ScopeRouteSelection
  /**
   * Lifecycle ownership: `true` (the default composition) lets the page close
   * the model it mounted; a host that shares this model passes `false` and
   * closes it itself.
   */
  readonly ownsModel?: boolean
}

export interface HomeDashboardPage {
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

function isDecision(card: HomeCard): card is HomeDecisionCard {
  return 'kind' in card
}

function cardKey(card: HomeCard): string {
  if (isDecision(card)) return `decision:${card.kind}:${card.id}`
  if ('visitedAt' in card) return `visited:${card.deliveryId}`
  return `delivery:${card.deliveryId}`
}

function updateContextList(list: HTMLUListElement, entries: readonly string[]): void {
  entries.forEach((entry, index) => {
    const item = list.children[index]
    if (item !== undefined && item.textContent !== entry) item.textContent = entry
  })
  for (let index = entries.length; index < list.children.length; index += 1) {
    const item = list.children[index]
    if (item !== undefined && item.textContent !== '') item.textContent = ''
  }
}

interface CardParts {
  readonly node: HTMLLIElement
  readonly title: HTMLElement
  readonly status: HTMLElement
  readonly context: HTMLUListElement
  readonly chat: HTMLAnchorElement
  readonly action: HTMLAnchorElement
}

/** Mount the design-04 task board: two live columns over collapsed history rows. */
export function mountHomeDashboardPage(
  options: HomeDashboardPageOptions,
): HomeDashboardPage {
  const document = options.root.ownerDocument
  const presentation = PRESENTATION

  const layout = element(document, 'section', 'wwc-home')
  layout.dataset.wwcPage = 'home'

  // Design page 04: the big title shares one row with the display-only Scope
  // select and the new-task entry.
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: presentation.title,
      headingLevel: 2,
      className: 'wwc-home-heading',
    },
  })
  const projectSelect = element(document, 'select', 'wwc-home-project-select')
  projectSelect.disabled = true
  projectSelect.setAttribute('aria-label', presentation.projectSelectLabel)
  projectSelect.title = presentation.projectSelectTitle
  for (const label of [presentation.allProjectsLabel, presentation.currentScopeLabel]) {
    const option = document.createElement('option')
    option.textContent = label
    projectSelect.append(option)
  }
  const newTask = element(document, 'a', 'wwc-home-new-task')
  newTask.href = surfaceHash('/home/new-task', options.scopeSelection)
  newTask.textContent = presentation.newTaskLabel
  // 任务看板是待处理事项的统一入口，筛选开关只保留需要用户操作的卡片。
  const attentionOnly = element(document, 'button', 'wwc-home-attention-only')
  attentionOnly.type = 'button'
  attentionOnly.textContent = presentation.attentionOnlyLabel
  attentionOnly.setAttribute('aria-pressed', 'false')
  const topActions = element(document, 'div', 'wwc-home-actions')
  topActions.append(projectSelect, attentionOnly, newTask)
  const topbar = element(document, 'div', 'wwc-home-topbar')
  topbar.append(pageHeader.root, topActions)

  // Design page 04: the status line is the one polite live region; it stays in
  // the DOM for announcements while the stylesheet keeps it off the canvas.
  const statusBadge = mountStatusBadge({
    document,
    props: {
      label: presentation.statusLabel.loading,
      tone: 'info',
      live: 'polite',
      className: 'wwc-home-status',
    },
  })
  const unavailable = element(document, 'p', 'wwc-home-unavailable')
  unavailable.hidden = true
  const mobileCapability = element(document, 'p', 'wwc-home-mobile-capability')
  mobileCapability.textContent = mobileWebCapabilityText(document.defaultView)

  const cardParts = new WeakMap<HTMLLIElement, CardParts>()

  function deliveryContextEntries(card: HomeDeliveryCard): readonly string[] {
    return Object.freeze([
      presentation.taskLabel(card),
      card.openAttentionCount === 0
        ? null
        : `${String(card.openAttentionCount)} 个待处理`,
      presentation.updatedLabel(card.updatedAt),
    ].filter((entry): entry is string => entry !== null && entry !== ''))
  }

  function setAction(parts: CardParts, href: string, label: string): void {
    parts.action.href = href
    parts.action.textContent = label
    parts.action.removeAttribute('aria-disabled')
    parts.action.tabIndex = 0
    parts.action.title = ''
  }

  function disableAction(parts: CardParts, label: string, title?: string): void {
    parts.action.removeAttribute('href')
    parts.action.setAttribute('aria-disabled', 'true')
    parts.action.tabIndex = -1
    parts.action.title = title ?? presentation.disabledLabel
    parts.action.textContent = label
  }

  function fillDecisionCard(parts: CardParts, card: HomeDecisionCard): void {
    parts.node.dataset.kind = 'decision'
    parts.node.dataset.urgency = card.urgency
    parts.node.dataset.disabled = String(card.actionDisabled)
    parts.title.textContent = card.title
    parts.status.textContent = homeDecisionStatusText(card)
    // Design page 04: the pending card face carries the name and the status
    // line only; the Chat session stays reachable through the quiet link.
    updateContextList(parts.context, [])
    parts.chat.hidden = card.actionDisabled || card.productSessionId === null
    if (card.productSessionId !== null && !card.actionDisabled) {
      parts.chat.href = homeChatHash(card.productSessionId, options.scopeSelection)
      parts.chat.textContent = presentation.openChatLabel
    } else {
      parts.chat.removeAttribute('href')
      parts.chat.textContent = ''
    }
    if (card.actionDisabled) disableAction(parts, homeDecisionActionLabel(card))
    else {
      const hash = homeDecisionHash(card, options.scopeSelection)
      if (hash === null) {
        disableAction(
          parts,
          homeDecisionActionLabel(card),
          '验收在交付流程中处理；当前版本没有独立验收入口。',
        )
      } else setAction(parts, hash, homeDecisionActionLabel(card))
    }
  }

  function clearAction(parts: CardParts): void {
    parts.action.removeAttribute('href')
    parts.action.removeAttribute('aria-disabled')
    parts.action.tabIndex = -1
    parts.action.title = ''
    parts.action.textContent = ''
  }

  function fillVisitedCard(parts: CardParts, card: HomeVisitedCard): void {
    fillDeliveryCard(parts, card)
    // 设计稿 04 之外:最近访问行的附加时间标注。
    parts.context.querySelectorAll('li').forEach((li, idx, all) => {
      if (idx === all.length - 1) li.textContent = `访问于 ${card.visitedAt}`
    })
  }

  function fillDeliveryCard(parts: CardParts, card: HomeDeliveryCard): void {
    parts.node.dataset.kind = 'delivery'
    parts.node.dataset.status = card.status
    parts.node.dataset.urgency = ''
    delete parts.node.dataset.disabled
    parts.title.textContent = card.title
    parts.status.textContent = `${presentation.strongFlowLabel} · ${
      presentation.deliveryStatusText[card.status]}`
    parts.chat.hidden = true
    parts.chat.removeAttribute('href')
    parts.chat.textContent = ''
    updateContextList(parts.context, deliveryContextEntries(card))
    // Design page 04: the running card's action opens the run page (查看进度).
    setAction(
      parts,
      surfaceHash('/home/task-run', options.scopeSelection),
      presentation.deliveryProgressLabel,
    )
  }


  function createCard(): HTMLLIElement {
    const node = element(document, 'li', 'wwc-home-card')
    const main = element(document, 'div', 'wwc-home-card-main')
    const title = element(document, 'h4', 'wwc-home-card-title')
    const status = element(document, 'p', 'wwc-home-card-status')
    const context = element(document, 'ul', 'wwc-home-card-context')
    for (let index = 0; index < 4; index += 1) context.append(document.createElement('li'))
    const chat = element(document, 'a', 'wwc-home-card-chat')
    chat.hidden = true
    main.append(title, status, context, chat)
    const action = element(document, 'a', 'wwc-home-card-action')
    node.append(main, action)
    cardParts.set(node, { node, title, status, context, chat, action })
    return node
  }

  function updateCard(node: HTMLLIElement, card: HomeCard): void {
    const parts = cardParts.get(node)
    if (parts === undefined) return
    if (isDecision(card)) fillDecisionCard(parts, card)
    else if ('visitedAt' in card) fillVisitedCard(parts, card)
    else fillDeliveryCard(parts, card)
  }

  interface SectionParts {
    readonly root: HTMLElement
    readonly heading: HTMLElement
    readonly count: HTMLElement
    readonly empty: HTMLElement
    readonly cards: HTMLUListElement
    readonly toggle: HTMLButtonElement | null
    readonly collection: KeyedCollectionView<HomeCard, string, HTMLLIElement>
  }
  const sections = new Map<HomeSectionId, SectionParts>()
  const sectionsRoot = element(document, 'div', 'wwc-home-sections')

  // The canonical WWC-ER-1001 board order: the two live columns first, then
  // the collapsed history rows of design page 04.
  const SECTION_ORDER: readonly HomeSectionId[] = Object.freeze([
    'running',
    'decisions',
    'backlog',
    'ready',
    'waiting',
    'validating',
    'failed',
    'completed',
    'visited',
  ])

  for (const id of SECTION_ORDER) {
    const headingRow = element(document, 'header', 'wwc-home-section-header')
    const heading = element(document, 'h3', 'wwc-home-section-heading')
    heading.textContent = presentation.sectionHeading[id]
    const count = element(document, 'span', 'wwc-home-section-count')
    headingRow.append(heading, count)
    const empty = element(document, 'p', 'wwc-home-section-empty')
    empty.hidden = true
    empty.textContent = presentation.sectionEmpty[id]
    const cards = element(document, 'ul', 'wwc-home-cards')
    const root = element(document, 'section', 'wwc-home-section')
    root.dataset.section = id
    // Design page 04: history groups render as collapsed single hairline rows.
    const collapsible = presentation.collapsibleSections.includes(id)
    let toggle: HTMLButtonElement | null = null
    if (collapsible) {
      const toggleButton = element(document, 'button', 'wwc-home-section-toggle')
      toggleButton.type = 'button'
      cards.id = `wwc-home-cards-${id}`
      toggleButton.setAttribute('aria-controls', cards.id)
      toggleButton.setAttribute('aria-expanded', 'false')
      toggleButton.setAttribute(
        'aria-label',
        `${presentation.sectionHeading[id]} · ${presentation.expandLabel}`,
      )
      toggleButton.addEventListener('click', () => {
        const expanded = toggleButton.getAttribute('aria-expanded') === 'true'
        toggleButton.setAttribute('aria-expanded', expanded ? 'false' : 'true')
        toggleButton.setAttribute(
          'aria-label',
          `${presentation.sectionHeading[id]} · ${expanded ? presentation.expandLabel : presentation.collapseLabel}`,
        )
        const liveTotal = options.model.state.counts[id === 'visited' ? 'visited' : id]
        toggleButton.textContent = expanded
          ? `${presentation.sectionHeading[id]} · ${presentation.collapseLabel}`
          : `${presentation.sectionHeading[id]} · ${presentation.countLabel(liveTotal)}`
        cards.hidden = expanded
        empty.hidden = expanded ? true : !renderedEmpty(id)
      })
      headingRow.append(toggleButton)
      cards.hidden = true
      toggle = toggleButton
    }
    root.append(headingRow, empty, cards)
    sectionsRoot.append(root)
    sections.set(id, {
      root,
      heading,
      count,
      empty,
      cards,
      toggle,
      collection: mountKeyedCollection<HomeCard, string, HTMLLIElement>({
        parent: cards,
        key: cardKey,
        create: createCard,
        update: updateCard,
      }),
    })
  }

  function renderedEmpty(id: HomeSectionId): boolean {
    const state = options.model.state
    const rendered = id === 'decisions'
      ? state.decisions.length
      : id === 'backlog' ? state.backlog.length
        : id === 'running' ? state.running.length
          : id === 'ready' ? state.ready.length
            : id === 'waiting' ? state.waiting.length
              : id === 'validating' ? state.validating.length
                : id === 'failed' ? state.failed.length
                  : id === 'completed' ? state.completed.length
                    : state.visited.length
    return rendered === 0
  }

  layout.append(topbar, statusBadge.root, unavailable, mobileCapability, sectionsRoot)
  options.root.replaceChildren(layout)

  let closed = false
  // 通知打开看板时携带筛选参数，直接展示需要用户操作的卡片。
  let attentionOnlyEnabled = globalThis.location?.hash?.includes('filter=attention') === true

  attentionOnly.setAttribute('aria-pressed', String(attentionOnlyEnabled))
  attentionOnly.dataset.active = String(attentionOnlyEnabled)
  layout.dataset.attentionOnly = String(attentionOnlyEnabled)

  attentionOnly.addEventListener('click', () => {
    attentionOnlyEnabled = !attentionOnlyEnabled
    attentionOnly.setAttribute('aria-pressed', String(attentionOnlyEnabled))
    attentionOnly.dataset.active = String(attentionOnlyEnabled)
    layout.dataset.attentionOnly = String(attentionOnlyEnabled)
    for (const [id, section] of sections) {
      if (id === 'decisions') continue
      section.root.hidden = attentionOnlyEnabled
    }
    const decisions = sections.get('decisions')
    if (decisions !== undefined) {
      decisions.root.hidden = false
      const expanded = decisions.toggle === null
        || decisions.toggle.getAttribute('aria-expanded') === 'true'
      decisions.empty.hidden = expanded ? !renderedEmpty('decisions') : true
    }
  })

  function render(state: HomeDashboardState): void {
    if (closed) return
    const tone: StatusTone = state.status === 'error'
      ? 'danger'
      : state.status === 'partial'
        ? 'warning'
        : state.status === 'loading'
          ? 'info'
          : state.status === 'ready'
            ? 'success'
            : 'neutral'
    statusBadge.update({
      label: homeDashboardAnnouncement(state),
      tone,
      live: 'polite',
      className: 'wwc-home-status',
    })
    layout.setAttribute('aria-busy', String(state.status === 'loading'))
    const missing = (Object.keys(state.sources) as readonly HomeDashboardSource[]).filter(
      source => state.sources[source] === 'unavailable',
    )
    unavailable.hidden = missing.length === 0
    unavailable.textContent = missing.length === 0
      ? ''
      : missing.map(source => `${presentation.sourceLabel[source]} ${
        presentation.unavailableLabel}`).join(' · ')
    sections.get('decisions')?.collection.update(state.decisions)
    sections.get('backlog')?.collection.update(state.backlog)
    sections.get('running')?.collection.update(state.running)
    sections.get('ready')?.collection.update(state.ready)
    sections.get('waiting')?.collection.update(state.waiting)
    sections.get('validating')?.collection.update(state.validating)
    sections.get('failed')?.collection.update(state.failed)
    sections.get('completed')?.collection.update(state.completed)
    sections.get('visited')?.collection.update(state.visited)
    for (const [id, section] of sections) {
      const total = id === 'decisions'
        ? state.counts.decisions
        : id === 'backlog' ? state.counts.backlog
          : id === 'running' ? state.counts.running
            : id === 'ready' ? state.counts.ready
              : id === 'waiting' ? state.counts.waiting
                : id === 'validating' ? state.counts.validating
                  : id === 'failed' ? state.counts.failed
                    : id === 'completed' ? state.counts.completed
                      : state.counts.visited
      section.count.textContent = presentation.countLabel(total)
      // Design 04: the collapsed hairline is a count row driven by the home
      // projection — never a silent empty strip.
      if (section.toggle !== null) {
        const expandedNow = section.toggle.getAttribute('aria-expanded') === 'true'
        section.toggle.textContent = expandedNow
          ? `${presentation.sectionHeading[id]} · ${presentation.collapseLabel}`
          : `${presentation.sectionHeading[id]} · ${presentation.countLabel(total)}`
      }
      if (attentionOnlyEnabled && id !== 'decisions') {
        section.root.hidden = true
        continue
      }
      section.root.hidden = false
      // A collapsed row keeps its empty note hidden with its cards; an open
      // column shows the honest empty state.
      const expanded = section.toggle === null
        || section.toggle.getAttribute('aria-expanded') === 'true'
      section.empty.hidden = expanded ? !renderedEmpty(id) : true
    }
  }

  const unsubscribe = options.model.subscribe(render)
  void options.model.start()

  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      for (const section of sections.values()) section.collection.close()
      statusBadge.close()
      pageHeader.close()
      options.root.replaceChildren()
      if (options.ownsModel !== false) options.model.close()
    },
  }
}
