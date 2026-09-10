// SPDX-License-Identifier: Apache-2.0

import { formatInstant } from './format-instant.js'
import { attentionCenterItemHash } from './attention-center-page.js'
import {
  mountButton,
  mountPageHeader,
  mountStatusBadge,
  type StatusTone,
} from '@winwincode/browser-ui'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import { scopeHash, surfaceHash, type ScopeRouteSelection } from '@winwincode/browser-core/scope-context'
import type { DeliveryStatus, Instant, ProductSessionId } from './generated/contracts.js'
import { DeliveryStatus as DeliveryStatusVocabulary } from './generated/contracts.js'
import type {
  HomeDashboardSource,
  HomeDashboardState,
  HomeDashboardStatus,
  HomeDashboardViewModel,
  HomeDecisionCard,
  HomeDeliveryCard,
} from './home-dashboard-view-model.js'

export type HomeSectionId = 'decisions' | 'active' | 'failing' | 'completed'

/** One card as the dashboard renders it: a decision or a Delivery. */
export type HomeCard = HomeDecisionCard | HomeDeliveryCard

export interface HomeDashboardPresentation {
  readonly title: string
  readonly refreshLabel: string
  readonly projectSelectLabel: string
  readonly projectSelectTitle: string
  readonly allProjectsLabel: string
  readonly currentScopeLabel: string
  readonly newTaskLabel: string
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
  readonly deliveryStatusText: Readonly<Record<DeliveryStatus, string>>
  readonly planReviewPendingLabel: string
  readonly deliveryAcceptancePendingLabel: string
  readonly decisionUrgencyText: Readonly<Record<'expired' | 'binding-invalid', string>>
  readonly blockingDecisionLabel: string
  readonly reviewPlanLabel: string
  readonly acceptDeliveryLabel: string
  readonly openChatLabel: string
  readonly disabledLabel: string
  readonly countLabel: (count: number) => string
  readonly updatedLabel: (at: Instant) => string
  readonly taskLabel: (card: Pick<
    HomeDeliveryCard,
    'activeTasks' | 'verifyingTasks' | 'failedTasks' | 'blockedTasks' | 'completedTasks'
  >) => string
}

const PRESENTATION_SPEC: HomeDashboardPresentation = {
  title: '任务看板',
  refreshLabel: '立即刷新',
  projectSelectLabel: '项目范围（展示）',
  projectSelectTitle: '看板已限定当前仓库 Scope;项目筛选为展示控件。',
  allProjectsLabel: '全部项目',
  currentScopeLabel: '当前仓库',
  newTaskLabel: '新建任务',
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
    active: '正在运行',
    failing: '失败或阻塞',
    completed: '已完成',
  }),
  sectionEmpty: Object.freeze({
    decisions: '现在没有需要决策的事项。',
    active: '没有进行中的交付。',
    failing: '没有失败或阻塞的交付。',
    completed: '还没有已完成的交付。',
  }),
  collapsibleSections: Object.freeze(['failing', 'completed']),
  expandLabel: '展开',
  collapseLabel: '收起',
  strongFlowLabel: '强流程',
  deliveryStatusText: Object.freeze({
    [DeliveryStatusVocabulary.Draft]: '草稿',
    [DeliveryStatusVocabulary.Clarifying]: '正在澄清',
    [DeliveryStatusVocabulary.Ready]: '待启动',
    [DeliveryStatusVocabulary.Planning]: '正在规划',
    [DeliveryStatusVocabulary.PlanReview]: '等你审核方案',
    [DeliveryStatusVocabulary.Executing]: '正在执行',
    [DeliveryStatusVocabulary.Verifying]: '正在验证',
    [DeliveryStatusVocabulary.Reworking]: '验证未通过，正在修复',
    [DeliveryStatusVocabulary.NeedsAttention]: '阻塞 · 需要处理',
    [DeliveryStatusVocabulary.ReadyToDeliver]: '待验收',
    [DeliveryStatusVocabulary.Delivered]: '已交付',
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
  disabledLabel: '该决策已关闭。请刷新查看当前状态。',
  countLabel: count => String(count),
  updatedLabel: at => `更新于 ${formatInstant(at)}`,
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
    `${String(counts.active)} 个运行中`,
    `${String(counts.failing)} 个失败或阻塞`,
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
 * the Chat session that raised it.  A Delivery-bound Attention has no
 * standalone acceptance surface in the community client, so it renders no
 * action (`null`) instead of a dead end.
 */
export function homeDecisionHash(
  card: HomeDecisionCard,
  scopeSelection: ScopeRouteSelection,
): string | null {
  return attentionCenterItemHash({
    kind: card.kind,
    id: card.id,
    productSessionId: card.productSessionId,
    stageRunId: card.stageRunId,
    deliveryId: card.deliveryId,
  }, scopeSelection)
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
  return isDecision(card)
    ? `decision:${card.kind}:${card.id}`
    : `delivery:${card.deliveryId}`
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
  const topActions = element(document, 'div', 'wwc-home-actions')
  topActions.append(projectSelect, newTask)
  const topbar = element(document, 'div', 'wwc-home-topbar')
  topbar.append(pageHeader.root, topActions)

  // The dashboard keeps exactly one polite live region: the status row.
  const statusBadge = mountStatusBadge({
    document,
    props: {
      label: presentation.statusLabel.loading,
      tone: 'info',
      live: 'polite',
      className: 'wwc-home-status',
    },
  })
  const refreshButton = mountButton({
    document,
    props: {
      label: presentation.refreshLabel,
      className: 'wwc-home-refresh',
      onActivate: () => { void options.model.refresh() },
    },
  })
  const unavailable = element(document, 'p', 'wwc-home-unavailable')
  unavailable.hidden = true

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
      // A Delivery-bound Attention has no standalone acceptance surface; the
      // card keeps its status line instead of a dead-end action.
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
    // A Delivery card is informational: the community client has no delivery
    // workbench route, so the card carries no dead-end action link.
    clearAction(parts)
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
    else fillDeliveryCard(parts, card)
  }

  interface SectionParts {
    readonly heading: HTMLElement
    readonly count: HTMLElement
    readonly empty: HTMLElement
    readonly cards: HTMLUListElement
    readonly toggle: HTMLButtonElement | null
    readonly collection: KeyedCollectionView<HomeCard, string, HTMLLIElement>
  }
  const sections = new Map<HomeSectionId, SectionParts>()
  const sectionsRoot = element(document, 'div', 'wwc-home-sections')

  for (const id of ['decisions', 'active', 'failing', 'completed'] as const) {
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
      : id === 'active'
        ? state.active.length
        : id === 'failing'
          ? state.failing.length
          : state.completed.length
    return rendered === 0
  }

  layout.append(topbar, statusBadge.root, refreshButton.root, unavailable, sectionsRoot)
  options.root.replaceChildren(layout)

  let closed = false

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
    refreshButton.update({
      label: presentation.refreshLabel,
      className: 'wwc-home-refresh',
      onActivate: () => { void options.model.refresh() },
      disabled: state.status === 'loading' || state.status === 'closed',
    })
    const missing = (Object.keys(state.sources) as readonly HomeDashboardSource[]).filter(
      source => state.sources[source] === 'unavailable',
    )
    unavailable.hidden = missing.length === 0
    unavailable.textContent = missing.length === 0
      ? ''
      : missing.map(source => `${presentation.sourceLabel[source]} ${
        presentation.unavailableLabel}`).join(' · ')
    sections.get('decisions')?.collection.update(state.decisions)
    sections.get('active')?.collection.update(state.active)
    sections.get('failing')?.collection.update(state.failing)
    sections.get('completed')?.collection.update(state.completed)
    for (const [id, section] of sections) {
      const total = id === 'decisions'
        ? state.counts.decisions
        : id === 'active'
          ? state.counts.active
          : id === 'failing'
            ? state.counts.failing
            : id === 'completed'
              ? state.counts.completed
              : state.counts.completed
      section.count.textContent = presentation.countLabel(total)
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
      refreshButton.close()
      statusBadge.close()
      pageHeader.close()
      options.root.replaceChildren()
      if (options.ownsModel !== false) options.model.close()
    },
  }
}
