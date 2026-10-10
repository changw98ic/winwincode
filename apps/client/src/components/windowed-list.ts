import { assertMounted } from '@winwincode/browser-ui'
import {
  mountKeyedCollection,
  type KeyedCollectionKey,
  type KeyedCollectionView,
} from './keyed-collection.js'

export interface WindowedListWindow {
  readonly start: number
  readonly end: number
  readonly total: number
}

const WINDOW_OVERSCAN_LIMIT = 100

function requireNonNegativeInteger(name: string, value: number): void {
  if (!Number.isInteger(value) || value < 0) {
    throw new RangeError(`${name} must be a non-negative integer.`)
  }
}

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
  readonly scroller: HTMLElement
  readonly content: HTMLElement
  readonly key: (item: Item) => Key
  readonly create: (item: Item) => Node
  readonly update: (node: Node, item: Item) => void
  readonly remove?: (node: Node) => void
  readonly rowHeight: number
  readonly viewportRows: number
  readonly overscan?: number
  readonly onWindowChange?: (window: WindowedListWindow) => void
}

export interface WindowedListView<Item, Key extends KeyedCollectionKey, Node> {
  readonly root: HTMLElement
  readonly content: HTMLElement
  update(items: readonly Item[]): void
  node(key: Key): Node | null
  window(): WindowedListWindow
  refresh(): void
  reveal(key: Key): boolean
  close(): void
}

function setCustomProperty(node: HTMLElement, name: string, value: string): void {
  node.style?.setProperty?.(name, value)
}

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
