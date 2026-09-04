// SPDX-License-Identifier: Apache-2.0

import { attentionCenterItemHash } from './attention-center-page.js'
import type { AttentionCenterOrigin } from './attention-center-view-model.js'
import {
  mountButton,
  mountEmptyState,
  mountPageHeader,
  mountStatusBadge,
  type StatusTone,
} from './components/index.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import { scopeHash, surfaceHash, type ScopeRouteSelection } from './core/scope-context.js'
import type { Instant, ProductSessionId } from './generated/contracts.js'
import type {
  HomeDashboardSource,
  HomeDashboardState,
  HomeDashboardStatus,
  HomeDashboardViewModel,
  HomeDecisionCard,
  HomeDeliveryCard,
  HomeVisitedCard,
} from './home-dashboard-view-model.js'
import { mountUsageHealthSummary, type UsageHealthSummary } from './usage-health-page.js'
import { strongFlowRouteHash, type StrongFlowRoute } from './strongflow-route.js'

export type HomeSectionId = 'decisions' | 'active' | 'failing' | 'completed' | 'visited'

/** One card as the dashboard renders it: a decision, a Delivery, or a visit. */
export type HomeCard = HomeDecisionCard | HomeDeliveryCard | HomeVisitedCard

export interface HomeDashboardPresentation {
  readonly eyebrow: string
  readonly description: string
  readonly refreshLabel: string
  readonly statusLabel: Readonly<Record<HomeDashboardStatus, string>>
  readonly partialNote: string
  readonly errorNote: string
  readonly unavailableLabel: string
  readonly sourceLabel: Readonly<Record<HomeDashboardSource, string>>
  readonly sectionHeading: Readonly<Record<HomeSectionId, string>>
  readonly sectionDescription: Readonly<Record<HomeSectionId, string>>
  readonly sectionEmpty: Readonly<Record<HomeSectionId, string>>
  readonly sectionLink: Readonly<
    Partial<Record<HomeSectionId, { readonly label: string; readonly path: string }>>
  >
  readonly decisionLabel: Readonly<Record<HomeDecisionCard['kind'], string>>
  readonly openDecisionLabel: string
  readonly openChatLabel: string
  readonly openDeliveryLabel: string
  readonly disabledLabel: string
  readonly unknownSessionLabel: string
  readonly firstUseTitle: string
  readonly firstUseDetail: string
  readonly firstUseDeliveryLabel: string
  readonly firstUseChatLabel: string
  readonly countLabel: (count: number) => string
  readonly updatedLabel: (at: Instant) => string
  readonly visitedLabel: (at: Instant) => string
  readonly taskLabel: (card: Pick<
    HomeDeliveryCard,
    'activeTasks' | 'verifyingTasks' | 'failedTasks' | 'blockedTasks' | 'completedTasks'
  >) => string
}

const PRESENTATION_SPEC: HomeDashboardPresentation = {
  eyebrow: 'Attention first',
  description: 'What needs you now, and which executions are moving in this repository Scope.',
  refreshLabel: 'Refresh now',
  statusLabel: Object.freeze({
    loading: 'Reading the dashboard…',
    ready: 'Ready',
    partial: 'Ready with gaps',
    error: 'The dashboard could not be read',
    closed: 'Dashboard closed',
  }),
  partialNote: 'Some projections are unavailable in this Scope.',
  errorNote: 'Retry the dashboard.',
  unavailableLabel: 'is unavailable',
  sourceLabel: Object.freeze({
    delivery: 'The Delivery list',
    attention: 'Attention',
    usage: 'Usage and health',
  }),
  sectionHeading: Object.freeze({
    decisions: 'Needs you now',
    active: 'In progress',
    failing: 'Failed or blocked',
    completed: 'Recently completed',
    visited: 'Recently opened',
  }),
  sectionDescription: Object.freeze({
    decisions: 'Every pending decision across the current repository Scope.',
    active: 'Deliveries whose work is moving or waiting on the next step.',
    failing: 'Deliveries with failures, blocked tasks, or open business Attention.',
    completed: 'Deliveries that reached their publication target.',
    visited: 'Deliveries you opened recently, remembered in this browser only.',
  }),
  sectionEmpty: Object.freeze({
    decisions: 'Nothing needs a decision right now.',
    active: 'No Delivery is in progress.',
    failing: 'Nothing is failing or blocked.',
    completed: 'Nothing has been delivered yet.',
    visited: 'You have not opened a Delivery from this browser yet.',
  }),
  sectionLink: Object.freeze({
    decisions: Object.freeze({ label: 'Open the Attention Center', path: '/attention' }),
    active: Object.freeze({ label: 'Open all Deliveries', path: '/strongflow' }),
  }),
  decisionLabel: Object.freeze({
    input: 'Input',
    approval: 'Tool approval',
    attention: 'Business Attention',
  }),
  openDecisionLabel: 'Open decisions',
  openChatLabel: 'Open chat',
  openDeliveryLabel: 'Open delivery',
  disabledLabel: 'This decision is closed. Refresh for the current state.',
  unknownSessionLabel: 'Session · not reported',
  firstUseTitle: 'Start your first Delivery',
  firstUseDetail:
    'This repository Scope has no Delivery and no pending decision yet. Create a Delivery to move a requirement through StrongFlow, or start a Chat to describe what you need.',
  firstUseDeliveryLabel: 'Create your first Delivery',
  firstUseChatLabel: 'Start your first Chat',
  countLabel: count => (count === 1 ? '1 entry' : `${String(count)} entries`),
  updatedLabel: at => `Updated ${at}`,
  visitedLabel: at => `Opened ${at}`,
  taskLabel: card => [
    `${String(card.activeTasks)} active`,
    `${String(card.verifyingTasks)} verifying`,
    `${String(card.completedTasks)} completed`,
    card.failedTasks > 0 ? `${String(card.failedTasks)} failed` : null,
    card.blockedTasks > 0 ? `${String(card.blockedTasks)} blocked` : null,
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
    counts.decisions === 1
      ? '1 item needs a decision'
      : `${String(counts.decisions)} items need a decision`,
    `${String(counts.active)} in progress`,
    `${String(counts.failing)} failed or blocked`,
    `${String(counts.completed)} completed`,
  ].join(' · ')
  return state.status === 'partial'
    ? `${PRESENTATION.statusLabel.partial} · ${summary} · ${PRESENTATION.partialNote}`
    : `${PRESENTATION.statusLabel.ready} · ${summary}`
}

/** The exact StrongFlow route of one Delivery card and its active StageRun. */
export function homeDeliveryHash(
  card: Pick<HomeDeliveryCard, 'deliveryId' | 'activeStageRunId'>,
  scopeSelection: ScopeRouteSelection,
): string {
  const route: StrongFlowRoute = {
    deliveryId: card.deliveryId,
    productSessionId: null,
    stageRunId: card.activeStageRunId,
    candidatePath: null,
    candidateView: 'unified',
    comparison: { status: 'none' },
    evidenceTab: 'evidence',
    evidenceId: null,
  }
  return strongFlowRouteHash(route, scopeSelection)
}

/** The exact Chat session one decision came from. */
export function homeChatHash(
  productSessionId: ProductSessionId,
  scopeSelection: ScopeRouteSelection,
): string {
  return scopeHash(`#/chat?session=${encodeURIComponent(productSessionId)}`, scopeSelection)
}

/**
 * The authoritative target of one decision card: a Delivery-bound Attention
 * opens its execution context, every other decision opens the decision surface
 * and carries the exact origin with it.
 */
export function homeDecisionHash(
  card: HomeDecisionCard,
  scopeSelection: ScopeRouteSelection,
  origins?: readonly AttentionCenterOrigin[],
): string {
  return attentionCenterItemHash({
    kind: card.kind,
    id: card.id,
    productSessionId: card.productSessionId,
    stageRunId: card.stageRunId,
    deliveryId: card.deliveryId,
  }, scopeSelection, origins)
}

export interface HomeDashboardPageOptions {
  readonly root: HTMLElement
  readonly model: HomeDashboardViewModel
  /** The exact Scope path prefixed onto every deep link on the dashboard. */
  readonly scopeSelection: ScopeRouteSelection
  /** Execution origins used to link a decision back to its StageRun. */
  readonly origins?: readonly AttentionCenterOrigin[]
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

function isVisited(card: HomeCard): card is HomeVisitedCard {
  return 'visitedAt' in card
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
  readonly kind: HTMLElement
  readonly title: HTMLElement
  readonly context: HTMLUListElement
  readonly chat: HTMLAnchorElement
  readonly action: HTMLAnchorElement
}

/** Mount the Attention-first Home dashboard: one bounded first screen per Scope. */
export function mountHomeDashboardPage(
  options: HomeDashboardPageOptions,
): HomeDashboardPage {
  const document = options.root.ownerDocument
  const presentation = PRESENTATION
  const origins = options.origins ?? []

  const layout = element(document, 'section', 'wwc-home')
  layout.dataset.wwcPage = 'home'
  const pageHeader = mountPageHeader({
    document,
    props: {
      title: 'Home',
      eyebrow: presentation.eyebrow,
      description: presentation.description,
      headingLevel: 2,
      className: 'wwc-home-heading',
    },
  })
  // The dashboard keeps exactly one polite live region: the Usage summary it
  // composes announces nothing, and the first-use state is a status region.
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

  function decisionContextEntries(card: HomeDecisionCard): readonly string[] {
    return Object.freeze([
      card.urgency === 'blocking'
        ? 'Blocking · needs a decision now'
        : card.urgency === 'pending'
          ? 'Needs a decision'
          : card.urgency === 'expired'
            ? 'Expired · action disabled'
            : 'Binding invalid · action disabled',
      card.sessionTitle === null
        ? (card.productSessionId === null
          ? presentation.unknownSessionLabel
          : `Session · ${card.productSessionId}`)
        : `Session · ${card.sessionTitle}`,
      card.deliveryTitle === null ? 'No Delivery context' : `Delivery · ${card.deliveryTitle}`,
      card.expiresAt === null ? 'No expiry deadline' : `Expires ${card.expiresAt}`,
    ])
  }

  function deliveryContextEntries(card: HomeDeliveryCard): readonly string[] {
    return Object.freeze([
      `Status ${card.status} · r${String(card.revision)}`,
      presentation.taskLabel(card),
      card.openAttentionCount === 0
        ? 'No open Attention'
        : `${String(card.openAttentionCount)} open Attention`,
      presentation.updatedLabel(card.updatedAt),
    ])
  }

  function setAction(parts: CardParts, href: string, label: string): void {
    parts.action.href = href
    parts.action.textContent = label
    parts.action.removeAttribute('aria-disabled')
    parts.action.tabIndex = 0
    parts.action.title = ''
  }

  function disableAction(parts: CardParts, label: string): void {
    parts.action.removeAttribute('href')
    parts.action.setAttribute('aria-disabled', 'true')
    parts.action.tabIndex = -1
    parts.action.title = presentation.disabledLabel
    parts.action.textContent = label
  }

  function fillDecisionCard(parts: CardParts, card: HomeDecisionCard): void {
    parts.node.dataset.kind = 'decision'
    parts.node.dataset.urgency = card.urgency
    parts.node.dataset.disabled = String(card.actionDisabled)
    parts.kind.textContent = presentation.decisionLabel[card.kind]
    parts.title.textContent = card.title
    parts.chat.hidden = card.actionDisabled || card.productSessionId === null
    if (card.productSessionId !== null && !card.actionDisabled) {
      parts.chat.href = homeChatHash(card.productSessionId, options.scopeSelection)
      parts.chat.textContent = presentation.openChatLabel
    } else {
      parts.chat.removeAttribute('href')
      parts.chat.textContent = ''
    }
    updateContextList(parts.context, decisionContextEntries(card))
    if (card.actionDisabled) disableAction(parts, presentation.openDecisionLabel)
    else {
      setAction(
        parts,
        homeDecisionHash(card, options.scopeSelection, origins),
        card.kind === 'attention'
          ? presentation.openDeliveryLabel
          : presentation.openDecisionLabel,
      )
    }
  }

  function fillDeliveryCard(parts: CardParts, card: HomeDeliveryCard): void {
    parts.node.dataset.kind = 'delivery'
    parts.node.dataset.status = card.status
    parts.node.dataset.urgency = ''
    delete parts.node.dataset.disabled
    parts.kind.textContent = card.status
    parts.title.textContent = card.title
    parts.chat.hidden = true
    parts.chat.removeAttribute('href')
    parts.chat.textContent = ''
    updateContextList(parts.context, deliveryContextEntries(card))
    setAction(
      parts,
      homeDeliveryHash(card, options.scopeSelection),
      presentation.openDeliveryLabel,
    )
  }

  function fillVisitedCard(parts: CardParts, card: HomeVisitedCard): void {
    parts.node.dataset.kind = 'visited'
    parts.node.dataset.status = card.status
    parts.node.dataset.urgency = ''
    delete parts.node.dataset.disabled
    parts.kind.textContent = card.status
    parts.title.textContent = card.title
    parts.chat.hidden = true
    parts.chat.removeAttribute('href')
    parts.chat.textContent = ''
    updateContextList(parts.context, [
      ...deliveryContextEntries(card),
      presentation.visitedLabel(card.visitedAt),
    ])
    setAction(
      parts,
      homeDeliveryHash(card, options.scopeSelection),
      presentation.openDeliveryLabel,
    )
  }

  function createCard(): HTMLLIElement {
    const node = element(document, 'li', 'wwc-home-card')
    const kind = element(document, 'span', 'wwc-home-card-kind')
    const title = element(document, 'h4', 'wwc-home-card-title')
    const context = element(document, 'ul', 'wwc-home-card-context')
    for (let index = 0; index < 5; index += 1) context.append(document.createElement('li'))
    const chat = element(document, 'a', 'wwc-home-card-chat')
    chat.hidden = true
    const action = element(document, 'a', 'wwc-home-card-action')
    node.append(kind, title, context, chat, action)
    cardParts.set(node, { node, kind, title, context, chat, action })
    return node
  }

  function updateCard(node: HTMLLIElement, card: HomeCard): void {
    const parts = cardParts.get(node)
    if (parts === undefined) return
    if (isVisited(card)) fillVisitedCard(parts, card)
    else if (isDecision(card)) fillDecisionCard(parts, card)
    else fillDeliveryCard(parts, card)
  }

  interface SectionParts {
    readonly count: HTMLElement
    readonly empty: HTMLElement
    readonly collection: KeyedCollectionView<HomeCard, string, HTMLLIElement>
  }
  const sections = new Map<HomeSectionId, SectionParts>()
  const sectionsRoot = element(document, 'div', 'wwc-home-sections')

  for (const id of ['decisions', 'active', 'failing', 'completed', 'visited'] as const) {
    const headingRow = element(document, 'header', 'wwc-home-section-header')
    const heading = element(document, 'h3', 'wwc-home-section-heading')
    heading.textContent = presentation.sectionHeading[id]
    const count = element(document, 'span', 'wwc-home-section-count')
    headingRow.append(heading, count)
    const description = element(document, 'p', 'wwc-home-section-description')
    description.textContent = presentation.sectionDescription[id]
    const empty = element(document, 'p', 'wwc-home-section-empty')
    empty.hidden = true
    empty.textContent = presentation.sectionEmpty[id]
    const cards = element(document, 'ul', 'wwc-home-cards')
    const root = element(document, 'section', 'wwc-home-section')
    root.dataset.section = id
    root.append(headingRow, description, empty, cards)
    const link = presentation.sectionLink[id]
    if (link !== undefined) {
      const sectionLink = element(document, 'a', 'wwc-home-section-link')
      sectionLink.href = surfaceHash(link.path, options.scopeSelection)
      sectionLink.textContent = link.label
      root.append(sectionLink)
    }
    sectionsRoot.append(root)
    sections.set(id, {
      count,
      empty,
      collection: mountKeyedCollection<HomeCard, string, HTMLLIElement>({
        parent: cards,
        key: cardKey,
        create: createCard,
        update: updateCard,
      }),
    })
  }

  const usageRoot = element(document, 'div', 'wwc-home-usage-root')
  layout.append(
    pageHeader.root,
    statusBadge.root,
    refreshButton.root,
    unavailable,
    sectionsRoot,
    usageRoot,
  )
  options.root.replaceChildren(layout)

  // The Usage, Provider and Worker health summary is the existing read-only
  // projection panel; it owns this root and opens no second live region.
  const usagePanel: UsageHealthSummary = mountUsageHealthSummary({
    root: usageRoot,
    model: options.model.usage,
  })

  const firstUseDelivery = element(document, 'a', 'wwc-home-first-use-delivery')
  firstUseDelivery.href = surfaceHash('/strongflow', options.scopeSelection)
  firstUseDelivery.textContent = presentation.firstUseDeliveryLabel
  const firstUseChat = element(document, 'a', 'wwc-home-first-use-chat')
  firstUseChat.href = surfaceHash('/chat', options.scopeSelection)
  firstUseChat.textContent = presentation.firstUseChatLabel
  const firstUseActions = element(document, 'div', 'wwc-home-first-use-actions')
  firstUseActions.append(firstUseDelivery, firstUseChat)
  const firstUse = mountEmptyState({
    document,
    props: {
      title: presentation.firstUseTitle,
      detail: presentation.firstUseDetail,
      headingLevel: 3,
      className: 'wwc-home-first-use',
      action: firstUseActions,
    },
  })
  firstUse.root.hidden = true
  layout.append(firstUse.root)

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
    sections.get('visited')?.collection.update(state.visited)
    for (const [id, section] of sections) {
      const rendered = id === 'decisions'
        ? state.decisions.length
        : id === 'active'
          ? state.active.length
          : id === 'failing'
            ? state.failing.length
            : id === 'completed'
              ? state.completed.length
              : state.visited.length
      const total = id === 'decisions'
        ? state.counts.decisions
        : id === 'active'
          ? state.counts.active
          : id === 'failing'
            ? state.counts.failing
            : id === 'completed'
              ? state.counts.completed
              : state.counts.visited
      section.count.textContent = presentation.countLabel(total)
      section.empty.hidden = rendered > 0
    }
    firstUse.root.hidden = !state.firstUse
  }

  const unsubscribe = options.model.subscribe(render)
  void options.model.start()

  return {
    close() {
      if (closed) return
      closed = true
      unsubscribe()
      for (const section of sections.values()) section.collection.close()
      firstUse.close()
      usagePanel.close()
      refreshButton.close()
      statusBadge.close()
      pageHeader.close()
      options.root.replaceChildren()
      if (options.ownsModel !== false) options.model.close()
    },
  }
}
