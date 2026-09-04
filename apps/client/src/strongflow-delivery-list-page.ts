// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryProjection,
  DeliveryStatus,
} from './generated/contracts.js'
import { DeliveryStatus as DeliveryStatusVocabulary } from './generated/contracts.js'
import { scopeHash, type ScopeRouteSelection } from './core/scope-context.js'
import { mountKeyedCollection, type KeyedCollectionView } from './components/keyed-collection.js'
import {
  mountWindowedList,
  type WindowedListView,
} from './components/windowed-list.js'
import type {
  StrongFlowDeliveryListState,
  StrongFlowDeliveryListViewModel,
} from './strongflow-delivery-list-view-model.js'
import {
  boundedItems,
  DEFAULT_STRONGFLOW_RENDER_LIMITS,
  strongFlowElement,
  STRONGFLOW_WINDOW_OVERSCAN_ROWS,
  STRONGFLOW_WINDOW_ROW_HEIGHT_PX,
  STRONGFLOW_WINDOW_VIEWPORT_ROWS,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'

export type StrongFlowDeliveryListView = 'list' | 'kanban'

export interface StrongFlowDeliveryListPageOptions {
  readonly root: HTMLElement
  readonly model: StrongFlowDeliveryListViewModel
  /** The exact Scope path prefixed onto every Delivery route. */
  readonly routeScope?: ScopeRouteSelection
  readonly view?: StrongFlowDeliveryListView
  readonly onViewChange?: (view: StrongFlowDeliveryListView) => void
  /** Presentation-only capability; Server authorization remains authoritative. */
  readonly readOnly?: boolean
  /** Render caps; defaults keep the list to one bounded window. */
  readonly limits?: StrongFlowRenderLimits
}

export interface StrongFlowDeliveryListPage {
  /**
   * Merges the exact active Delivery projection into the rendered rows so the
   * current selection survives searches, filters, and unloaded windows.
   */
  setActive(delivery: DeliveryProjection | null): void
  close(): void
}

interface DragCard {
  readonly deliveryId: string
  readonly revision: number
  readonly status: DeliveryStatus
}

interface KanbanColumn {
  readonly status: DeliveryStatus
  readonly deliveries: readonly DeliveryProjection[]
}

function humanizeStatus(status: string): string {
  return status.split('-').map(part => part.charAt(0).toUpperCase() + part.slice(1)).join(' ')
}

function deliveryRoute(deliveryId: string, routeScope?: ScopeRouteSelection): string {
  const route = `#/strongflow?delivery=${encodeURIComponent(deliveryId)}`
  return routeScope === undefined ? route : scopeHash(route, routeScope)
}

function statusText(delivery: DeliveryProjection): string {
  const base = `${delivery.status} · r${String(delivery.revision)}`
  return delivery.openAttentionCount === 0
    ? base
    : `${base} · needs attention (${String(delivery.openAttentionCount)})`
}

interface AlertAction {
  readonly label: string
  readonly action: 'refresh' | 'retry'
  readonly onActivate: () => void
}

export function mountStrongFlowDeliveryList(
  options: StrongFlowDeliveryListPageOptions,
): StrongFlowDeliveryListPage {
  const document = options.root.ownerDocument
  const readOnly = options.readOnly === true
  const limits: StrongFlowRenderLimits = options.limits ?? DEFAULT_STRONGFLOW_RENDER_LIMITS
  let view: StrongFlowDeliveryListView = options.view ?? 'list'
  let activeDelivery: DeliveryProjection | null = null
  let currentState: StrongFlowDeliveryListState | null = null
  let dragCard: DragCard | null = null
  /** Identity the window last revealed, so deep links scroll only once. */
  let revealedDeliveryId: DeliveryProjection['deliveryId'] | null = null
  let kanbanOmittedCount = 0

  const heading = document.createElement('h2')
  heading.className = 'wwc-strongflow-deliveries-heading'
  heading.textContent = 'Deliveries'

  const toolbar = document.createElement('div')
  toolbar.className = 'wwc-delivery-toolbar'

  const searchLabel = document.createElement('label')
  searchLabel.className = 'wwc-delivery-field'
  const search = document.createElement('input')
  search.className = 'wwc-delivery-search'
  search.type = 'text'
  const searchText = document.createElement('span')
  searchText.textContent = 'Search'
  searchLabel.append(searchText, search)

  const statusLabel = document.createElement('label')
  statusLabel.className = 'wwc-delivery-field'
  const statusFilter = document.createElement('select')
  statusFilter.className = 'wwc-delivery-status-filter'
  const allStatuses = document.createElement('option')
  allStatuses.value = ''
  allStatuses.textContent = 'All statuses'
  statusFilter.append(allStatuses)
  for (const status of Object.values(DeliveryStatusVocabulary)) {
    const option = document.createElement('option')
    option.value = status
    option.textContent = humanizeStatus(status)
    statusFilter.append(option)
  }
  const statusLabelText = document.createElement('span')
  statusLabelText.textContent = 'Status'
  statusLabel.append(statusLabelText, statusFilter)

  const attentionLabel = document.createElement('label')
  attentionLabel.className = 'wwc-delivery-field'
  const attentionFilter = document.createElement('input')
  attentionFilter.className = 'wwc-delivery-attention-filter'
  attentionFilter.type = 'checkbox'
  const attentionText = document.createElement('span')
  attentionText.textContent = 'Needs attention'
  attentionLabel.append(attentionText, attentionFilter)

  const orderLabel = document.createElement('label')
  orderLabel.className = 'wwc-delivery-field'
  const order = document.createElement('select')
  order.className = 'wwc-delivery-order'
  const recentOption = document.createElement('option')
  recentOption.value = 'recent'
  recentOption.textContent = 'Recently updated'
  const titleOption = document.createElement('option')
  titleOption.value = 'title'
  titleOption.textContent = 'Title'
  order.append(recentOption, titleOption)
  const orderText = document.createElement('span')
  orderText.textContent = 'Order'
  orderLabel.append(orderText, order)

  const viewSwitch = document.createElement('div')
  viewSwitch.className = 'wwc-delivery-view-switch'
  const listButton = document.createElement('button')
  listButton.className = 'wwc-delivery-view-list'
  listButton.type = 'button'
  const kanbanButton = document.createElement('button')
  kanbanButton.className = 'wwc-delivery-view-kanban'
  kanbanButton.type = 'button'
  listButton.textContent = 'List'
  kanbanButton.textContent = 'Kanban'
  viewSwitch.append(listButton, kanbanButton)

  const refresh = document.createElement('button')
  refresh.className = 'wwc-delivery-refresh'
  refresh.type = 'button'
  refresh.textContent = 'Refresh'

  toolbar.append(searchLabel, statusLabel, attentionLabel, orderLabel, viewSwitch, refresh)

  const feedback = document.createElement('p')
  feedback.className = 'wwc-delivery-feedback'
  feedback.setAttribute('role', 'status')

  const alert = document.createElement('div')
  alert.className = 'wwc-delivery-alert'
  alert.setAttribute('role', 'alert')
  alert.hidden = true

  const empty = document.createElement('p')
  empty.className = 'wwc-delivery-empty'
  empty.setAttribute('role', 'status')
  empty.hidden = true

  const listView = document.createElement('div')
  listView.className = 'wwc-delivery-list-view'
  const list = document.createElement('ul')
  list.className = 'wwc-strongflow-delivery-list'

  const rows = new WeakMap<HTMLLIElement, {
    readonly link: HTMLAnchorElement
    readonly status: HTMLElement
  }>()

  function renderRow(item: HTMLLIElement, delivery: DeliveryProjection): void {
    const row = rows.get(item)
    if (row === undefined) return
    row.link.href = deliveryRoute(delivery.deliveryId, options.routeScope)
    row.link.textContent = delivery.title
    row.link.dataset.deliveryId = delivery.deliveryId
    row.status.textContent = statusText(delivery)
    if (activeDelivery?.deliveryId === delivery.deliveryId) {
      row.link.setAttribute('aria-current', 'page')
    } else {
      row.link.removeAttribute('aria-current')
    }
  }

  // The list itself is the keyed window host; the scroller around it hosts the
  // spacers that keep native scrolling exact for the records left out of the DOM.
  const listCollection = mountWindowedList<DeliveryProjection, string, HTMLLIElement>({
    document,
    scroller: strongFlowElement(document, 'div', 'wwc-delivery-list-scroll'),
    content: list,
    key: delivery => delivery.deliveryId,
    create() {
      const item = document.createElement('li')
      const link = document.createElement('a')
      const deliveryStatus = document.createElement('span')
      item.append(link, deliveryStatus)
      rows.set(item, { link, status: deliveryStatus })
      return item
    },
    update(item, delivery) { renderRow(item, delivery) },
    remove(item) { rows.delete(item) },
    rowHeight: STRONGFLOW_WINDOW_ROW_HEIGHT_PX,
    viewportRows: STRONGFLOW_WINDOW_VIEWPORT_ROWS,
    overscan: STRONGFLOW_WINDOW_OVERSCAN_ROWS,
  })
  listView.append(listCollection.root)

  const kanbanView = document.createElement('div')
  kanbanView.className = 'wwc-delivery-kanban-view'

  const kanbanOmitted = document.createElement('p')
  kanbanOmitted.className = 'wwc-delivery-kanban-omitted wwc-strongflow-omitted'
  kanbanOmitted.hidden = true

  const loadedNote = document.createElement('p')
  loadedNote.className = 'wwc-delivery-loaded-note'

  const loadMore = document.createElement('button') as HTMLButtonElement
  loadMore.className = 'wwc-delivery-load-more'
  loadMore.type = 'button'
  loadMore.textContent = 'Load more'

  options.root.replaceChildren(
    heading,
    toolbar,
    feedback,
    alert,
    empty,
    listView,
    kanbanView,
    kanbanOmitted,
    loadedNote,
    loadMore,
  )

  const columns = new WeakMap<HTMLElement, {
    readonly cards: HTMLElement
    readonly collection: KeyedCollectionView<DeliveryProjection, string, HTMLElement>
  }>()
  const kanbanCollection = mountKeyedCollection({
    parent: kanbanView,
    key: (entry: KanbanColumn) => entry.status,
    create(entry) {
      const column = document.createElement('section')
      column.className = 'wwc-delivery-kanban-column'
      column.dataset.status = entry.status
      const columnHeading = document.createElement('h3')
      columnHeading.className = 'wwc-delivery-kanban-heading'
      columnHeading.textContent = humanizeStatus(entry.status)
      const cards = document.createElement('ul')
      cards.className = 'wwc-delivery-kanban-column-list'
      column.append(columnHeading, cards)
      const collection = mountKeyedCollection({
        parent: cards,
        key: (delivery: DeliveryProjection) => delivery.deliveryId,
        create() {
          const item = document.createElement('li')
          item.className = 'wwc-delivery-kanban-card'
          const link = document.createElement('a')
          const deliveryStatus = document.createElement('span')
          // UI-604: drag and drop is the only way a Kanban card can move today, which
          // locks every pointer-free user out of advancing a Delivery.  The button is
          // the keyboard equivalent of the same drop handler and stays in the card so
          // the two paths can never diverge.
          const advance = document.createElement('button') as HTMLButtonElement
          advance.type = 'button'
          advance.className = 'wwc-delivery-kanban-advance'
          advance.textContent = 'Advance'
          advance.addEventListener('click', () => {
            if (readOnly) return
            const deliveryId = item.dataset.deliveryId
            const revision = Number(item.dataset.revision ?? '0')
            if (deliveryId === undefined || deliveryId.length === 0) return
            void options.model.advanceDelivery(
              deliveryId as DeliveryProjection['deliveryId'],
              revision,
            )
          })
          item.append(link, deliveryStatus, advance)
          item.addEventListener('dragstart', () => {
            dragCard = {
              deliveryId: item.dataset.deliveryId ?? '',
              revision: Number(item.dataset.revision ?? '0'),
              status: (item.dataset.status ?? entry.status) as DeliveryStatus,
            }
          })
          item.addEventListener('dragend', () => { dragCard = null })
          return item
        },
        update(item, delivery: DeliveryProjection) {
          item.dataset.deliveryId = delivery.deliveryId
          item.dataset.revision = String(delivery.revision)
          item.dataset.status = delivery.status
          item.draggable = !readOnly
          const advance = item.children[2] as HTMLButtonElement
          advance.hidden = readOnly
          advance.disabled = readOnly
          advance.setAttribute('aria-label', `Advance ${delivery.title}`)
          const card = item.children
          const link = card[0] as HTMLAnchorElement
          link.className = 'wwc-delivery-kanban-card-link'
          link.href = deliveryRoute(delivery.deliveryId, options.routeScope)
          link.textContent = delivery.title
          const deliveryStatus = card[1] as HTMLElement
          deliveryStatus.textContent = statusText(delivery)
        },
      })
      columns.set(column, { cards, collection })
      column.addEventListener('dragover', event => { event.preventDefault() })
      column.addEventListener('drop', () => {
        const dragged = dragCard
        dragCard = null
        if (dragged === null || dragged.status === entry.status) return
        void options.model.advanceDelivery(
          dragged.deliveryId as DeliveryProjection['deliveryId'],
          dragged.revision,
        )
      })
      return column
    },
    update(column, entry) {
      const view2 = columns.get(column)
      if (view2 === undefined) return
      view2.collection.update(entry.deliveries)
    },
    remove(column) {
      columns.delete(column)
    },
  })

  search.addEventListener('input', () => { options.model.setSearch(search.value) })
  statusFilter.addEventListener('change', () => {
    void options.model.setStatusFilter(
      statusFilter.value === '' ? null : statusFilter.value as DeliveryStatus,
    )
  })
  attentionFilter.addEventListener('change', () => {
    options.model.setAttentionOnly(attentionFilter.checked)
  })
  order.addEventListener('change', () => {
    options.model.setOrder(order.value as 'recent' | 'title')
  })
  listButton.addEventListener('click', () => { setView('list') })
  kanbanButton.addEventListener('click', () => { setView('kanban') })
  refresh.addEventListener('click', () => { void options.model.refresh() })
  loadMore.addEventListener('click', () => { void options.model.loadMore() })

  function setView(next: StrongFlowDeliveryListView): void {
    view = next
    options.onViewChange?.(next)
    renderViewSwitch()
    // Only the active view mounts rows, so the hidden view costs no DOM.
    if (currentState !== null) render(currentState)
  }

  function renderViewSwitch(): void {
    listButton.setAttribute('aria-pressed', String(view === 'list'))
    kanbanButton.setAttribute('aria-pressed', String(view === 'kanban'))
    listView.hidden = view !== 'list'
    kanbanView.hidden = view !== 'kanban'
  }

  function mergedVisible(state: StrongFlowDeliveryListState): readonly DeliveryProjection[] {
    if (activeDelivery === null) return state.visible
    const existing = state.visible.find(
      delivery => delivery.deliveryId === activeDelivery?.deliveryId,
    )
    if (existing === undefined) return [...state.visible, activeDelivery]
    if (existing.revision >= activeDelivery.revision) return state.visible
    return state.visible.map(delivery => (
      delivery.deliveryId === activeDelivery?.deliveryId ? activeDelivery : delivery
    ))
  }

  function capacityText(
    rendered: number,
    matches: number,
    state: StrongFlowDeliveryListState,
  ): string {
    const parts = [
      `Rendered ${String(rendered)} of ${String(matches)} matching deliveries · ${String(
        state.loadedCount,
      )} loaded · ${String(Math.max(0, matches - rendered))}`
        + ' loaded Deliveries are not rendered in this window',
    ]
    parts.push(state.hasMore
      ? ' · more Deliveries are available on the server'
      : ' · every loaded Delivery is already on the client')
    return parts.join('')
  }

  function alertSection(
    message: string,
    actions: readonly AlertAction[],
  ): HTMLElement {
    const section = document.createElement('div')
    section.className = 'wwc-delivery-alert-section'
    const text = document.createElement('p')
    text.textContent = message
    section.append(text)
    for (const action of actions) {
      const button = document.createElement('button') as HTMLButtonElement
      button.className = 'wwc-delivery-alert-action'
      button.type = 'button'
      button.dataset.action = action.action
      button.textContent = action.label
      button.addEventListener('click', action.onActivate)
      section.append(button)
    }
    return section
  }

  function renderAlert(state: StrongFlowDeliveryListState): void {
    const sections: HTMLElement[] = []
    if (state.error !== null) {
      const denied = state.error.kind === 'authorization' || state.error.kind === 'authentication'
      sections.push(alertSection(
        denied
          ? 'This account is not authorized to read Deliveries in the current repository.'
          : `Deliveries could not be loaded (${state.error.code}).`,
        [{
          label: 'Try again',
          action: 'refresh',
          onActivate: () => { void options.model.refresh() },
        }],
      ))
    }
    if (state.moreFailure !== null) {
      const expired = state.moreFailure.code === 'READ_CURSOR_EXPIRED'
      sections.push(alertSection(
        expired
          ? 'The next page is no longer current. Refresh the list from the start.'
          : `The next page could not be loaded (${state.moreFailure.code}).`,
        [
          {
            label: 'Retry',
            action: 'retry',
            onActivate: () => { void options.model.loadMore() },
          },
          {
            label: 'Refresh from start',
            action: 'refresh',
            onActivate: () => { void options.model.refresh() },
          },
        ],
      ))
    }
    if (state.advance.failure !== null) {
      sections.push(alertSection(
        `The move was rejected (${state.advance.failure.code}). `
          + 'Open the Delivery in StrongFlow to use the review or advance actions.',
        [],
      ))
    }
    alert.replaceChildren(...sections)
    alert.hidden = sections.length === 0
  }

  function render(state: StrongFlowDeliveryListState): void {
    currentState = state
    kanbanOmittedCount = 0
    feedback.textContent = state.status === 'loading'
      ? 'Loading Deliveries…'
      : state.status === 'refreshing'
        ? 'Refreshing Deliveries…'
        : ''
    feedback.hidden = feedback.textContent.length === 0

    renderAlert(state)

    const visible = mergedVisible(state)
    const byStatus = new Map<DeliveryStatus, DeliveryProjection[]>()
    for (const delivery of visible) {
      const bucket = byStatus.get(delivery.status) ?? []
      bucket.push(delivery)
      byStatus.set(delivery.status, bucket)
    }
    const vocabulary = Object.values(DeliveryStatusVocabulary)
    let kanbanRenderedCount = 0
    const kanbanEntries = [...byStatus.entries()]
      .sort((left, right) => vocabulary.indexOf(left[0]) - vocabulary.indexOf(right[0]))
      .map(([status, deliveries]) => {
        const bounded = boundedItems(deliveries, limits.deliveries)
        kanbanRenderedCount += bounded.items.length
        kanbanOmittedCount += bounded.omitted
        return { status, deliveries: bounded.items }
      })

    // Only the active view mounts rows: the Delivery corpus stays bounded no
    // matter which presentation the user picked.
    let renderedCount = 0
    if (view === 'list') {
      listCollection.update(visible)
      kanbanCollection.update([])
      // A deep link to a Delivery outside the rendered window must still be
      // visible, so the window scrolls to it once per active Delivery identity.
      if (activeDelivery !== null && activeDelivery.deliveryId !== revealedDeliveryId) {
        revealedDeliveryId = activeDelivery.deliveryId
        listCollection.reveal(activeDelivery.deliveryId)
      }
      renderedCount = listCollection.window().end - listCollection.window().start
    } else {
      listCollection.update([])
      kanbanCollection.update(kanbanEntries)
      renderedCount = kanbanRenderedCount
    }

    if (visible.length > 0) {
      empty.hidden = true
    } else if (state.status === 'loading') {
      empty.hidden = true
    } else if (state.loadedCount === 0) {
      empty.textContent = 'There are no Deliveries in this repository yet.'
      empty.hidden = false
    } else {
      empty.textContent = `No loaded Delivery matches the current filters (loaded ${String(
        state.loadedCount,
      )}).`
      empty.hidden = false
    }

    loadedNote.hidden = state.loadedCount === 0
    if (state.loadedCount !== 0) {
      loadedNote.textContent = capacityText(renderedCount, visible.length, state)
    }
    kanbanOmitted.hidden = kanbanOmittedCount === 0
    if (kanbanOmittedCount > 0) {
      kanbanOmitted.textContent = `${String(kanbanOmittedCount)} kanban cards not rendered.`
    }

    loadMore.hidden = !state.hasMore
    loadMore.disabled = state.loadingMore
    loadMore.textContent = state.loadingMore ? 'Loading…' : 'Load more'

    renderViewSwitch()
  }

  const unsubscribe = options.model.subscribe(render)

  return {
    setActive(delivery) {
      activeDelivery = delivery
      // Closing the Delivery clears the reveal marker so reopening the same one
      // scrolls back to it instead of being swallowed by the earlier reveal.
      if (delivery === null) revealedDeliveryId = null
      if (currentState !== null) render(currentState)
    },
    close() {
      unsubscribe()
      listCollection.close()
      kanbanCollection.close()
    },
  }
}
