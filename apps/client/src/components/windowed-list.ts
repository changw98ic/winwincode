// SPDX-License-Identifier: Apache-2.0

import { assertMounted } from './mounted-view.js'
import {
  mountKeyedCollection,
  type KeyedCollectionKey,
  type KeyedCollectionView,
} from './keyed-collection.js'

/**
 * Half-open `[start, end)` index range of the one rendered window inside the
 * full loaded list.  `total` is the complete loaded count, so callers can
 * report how many records stay out of the DOM instead of hiding them.
 */
export interface WindowedListWindow {
  readonly start: number
  readonly end: number
  readonly total: number
}

/** Largest accepted overscan, so a bad budget cannot enlarge the DOM cap. */
const WINDOW_OVERSCAN_LIMIT = 100

function requireNonNegativeInteger(name: string, value: number): void {
  if (!Number.isInteger(value) || value < 0) {
    throw new RangeError(`${name} must be a non-negative integer.`)
  }
}

/**
 * Resolve the rendered window for one scroll position.  The window keeps a
 * constant size whenever the list is longer than the window, so the DOM budget
 * never depends on how far the user has scrolled and no loaded record is ever
 * dropped: records outside the window stay reachable through the spacers.
 */
export function windowBounds(
  total: number,
  viewportRows: number,
  scrollTopRows: number,
  overscan: number,
): WindowedListWindow {
  requireNonNegativeInteger('total', total)
  requireNonNegativeInteger('overscan', overscan)
  requireNonNegativeInteger('viewportRows', viewportRows)
  if (viewportRows < 1) throw new RangeError('viewportRows must be at least 1.')
  if (overscan > WINDOW_OVERSCAN_LIMIT) {
    throw new RangeError(`overscan must not exceed ${String(WINDOW_OVERSCAN_LIMIT)}.`)
  }
  const windowRows = viewportRows + 2 * overscan
  // A host that cannot measure its scroll position yet still renders the head
  // of the list instead of an empty window.
  const measuredRows = Number.isFinite(scrollTopRows) ? Math.max(0, scrollTopRows) : 0
  const requestedStart = Math.floor(measuredRows) - overscan
  const start = Math.min(Math.max(requestedStart, 0), Math.max(0, total - windowRows))
  return { start, end: Math.min(total, start + windowRows), total }
}

export interface WindowedListMountOptions<
  Item,
  Key extends KeyedCollectionKey,
  Node extends globalThis.Node & ChildNode,
> {
  readonly document: Document
  /** The scroll container. It becomes `root` and hosts both spacers. */
  readonly scroller: HTMLElement
  /** The keyed row host. It holds exactly the rendered window. */
  readonly content: HTMLElement
  readonly key: (item: Item) => Key
  readonly create: (item: Item) => Node
  readonly update: (node: Node, item: Item) => void
  readonly remove?: (node: Node) => void
  /** Exact rendered row height in CSS pixels; the scroll math depends on it. */
  readonly rowHeight: number
  /** Rows rendered for the viewport, independent of the real viewport height. */
  readonly viewportRows: number
  readonly overscan?: number
  readonly onWindowChange?: (window: WindowedListWindow) => void
}

export interface WindowedListView<Item, Key extends KeyedCollectionKey, Node> {
  readonly root: HTMLElement
  readonly content: HTMLElement
  update(items: readonly Item[]): void
  node(key: Key): Node | null
  /** The currently rendered window, including how many records stay hidden. */
  window(): WindowedListWindow
  /** Re-read the scroll position and re-render the window. */
  refresh(): void
  /** Scroll the row with this key into the window; false when it is absent. */
  reveal(key: Key): boolean
  close(): void
}

function setCustomProperty(node: HTMLElement, name: string, value: string): void {
  node.style?.setProperty?.(name, value)
}

/**
 * One keyed, windowed list.  Rows keep their DOM identity across equivalent
 * snapshots because the window is rendered through `mountKeyedCollection`; only
 * the rows entering or leaving the window are created or removed.  Sizing the
 * two spacers to the hidden record count keeps native scrolling exact without
 * ever mounting the whole list.
 */
export function mountWindowedList<
  Item,
  Key extends KeyedCollectionKey,
  Node extends globalThis.Node & ChildNode,
>(
  options: WindowedListMountOptions<Item, Key, Node>,
): WindowedListView<Item, Key, Node> {
  const { document, scroller, content } = options
  requireNonNegativeInteger('rowHeight', options.rowHeight)
  if (options.rowHeight < 1) throw new RangeError('rowHeight must be at least 1.')

  const lead = document.createElement('div')
  const trail = document.createElement('div')
  lead.className = 'wwc-window-spacer wwc-window-spacer-lead'
  trail.className = 'wwc-window-spacer wwc-window-spacer-trail'
  scroller.className = `${scroller.className} wwc-window-scroller`.trim()
  setCustomProperty(scroller, '--wwc-window-row-height', `${String(options.rowHeight)}px`)
  setCustomProperty(
    scroller,
    '--wwc-window-viewport-rows',
    String(options.viewportRows + 2 * (options.overscan ?? 0)),
  )
  scroller.append(lead, content, trail)

  const rows = mountKeyedCollection({
    parent: content,
    key: options.key,
    create: options.create,
    update: options.update,
    ...(options.remove === undefined ? {} : { remove: options.remove }),
  })

  let open = true
  let items: readonly Item[] = []
  let currentWindow: WindowedListWindow = { start: 0, end: 0, total: 0 }

  function render(): void {
    const next = windowBounds(
      items.length,
      options.viewportRows,
      scroller.scrollTop / options.rowHeight,
      options.overscan ?? 0,
    )
    currentWindow = next
    rows.update(items.slice(next.start, next.end))
    setCustomProperty(
      lead,
      '--wwc-window-spacer-height',
      `${String(next.start * options.rowHeight)}px`,
    )
    setCustomProperty(
      trail,
      '--wwc-window-spacer-height',
      `${String((next.total - next.end) * options.rowHeight)}px`,
    )
    options.onWindowChange?.(next)
  }

  const onScroll = () => { render() }
  scroller.addEventListener('scroll', onScroll)

  return {
    root: scroller,
    content,
    update(nextItems) {
      assertMounted(open, 'WindowedList')
      items = nextItems
      render()
    },
    node(key) {
      assertMounted(open, 'WindowedList')
      return rows.node(key)
    },
    window() {
      assertMounted(open, 'WindowedList')
      return currentWindow
    },
    refresh() {
      assertMounted(open, 'WindowedList')
      render()
    },
    reveal(key) {
      assertMounted(open, 'WindowedList')
      // Only this control pays the full-key scan, and only when an active
      // identity changes; the hot update path stays independent of list length.
      let index = -1
      for (let position = 0; position < items.length; position += 1) {
        const item = items[position]
        if (item === undefined) continue
        if (options.key(item) === key) {
          index = position
          break
        }
      }
      if (index < 0) return false
      const top = index * options.rowHeight
      const viewport = options.viewportRows * options.rowHeight
      if (top < scroller.scrollTop || top + options.rowHeight > scroller.scrollTop + viewport) {
        scroller.scrollTop = Math.max(0, top)
      }
      render()
      return true
    },
    close() {
      if (!open) return
      open = false
      scroller.removeEventListener('scroll', onScroll)
      rows.close()
      lead.remove()
      trail.remove()
    },
  }
}
