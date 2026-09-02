// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export interface ToolbarProps {
  readonly label: string
  readonly items: readonly HTMLElement[]
  readonly className?: string
}

export interface ToolbarMountOptions {
  readonly document: Document
  readonly props: Readonly<ToolbarProps>
}

export interface ToolbarView extends MountedView<ToolbarProps> {
  readonly root: HTMLDivElement
}

function available(item: HTMLElement): boolean {
  return item.hidden !== true && Reflect.get(item, 'disabled') !== true
}

export function mountToolbar(options: ToolbarMountOptions): ToolbarView {
  const root = options.document.createElement('div')
  let current = options.props
  let open = true

  root.dataset.wwcComponent = 'toolbar'
  root.setAttribute('role', 'toolbar')

  function eligibleItems(): readonly HTMLElement[] {
    return current.items.filter(available)
  }

  function moveFocus(key: string): boolean {
    const items = eligibleItems()
    if (items.length === 0) return false
    const active = root.ownerDocument.activeElement
    const activeIndex = items.findIndex(item => item === active)
    let nextIndex: number
    if (key === 'Home') nextIndex = 0
    else if (key === 'End') nextIndex = items.length - 1
    else if (key === 'ArrowRight' || key === 'ArrowDown') {
      nextIndex = activeIndex < 0 ? 0 : (activeIndex + 1) % items.length
    } else if (key === 'ArrowLeft' || key === 'ArrowUp') {
      nextIndex = activeIndex < 0 ? items.length - 1 : (activeIndex - 1 + items.length) % items.length
    } else return false
    for (const item of current.items) item.tabIndex = item === items[nextIndex] ? 0 : -1
    items[nextIndex]?.focus()
    return true
  }

  const onKeyDown = (event: KeyboardEvent) => {
    if (!moveFocus(event.key)) return
    event.preventDefault()
  }
  root.addEventListener('keydown', onKeyDown)

  function update(props: Readonly<ToolbarProps>): void {
    assertMounted(open, 'Toolbar')
    current = props
    root.className = props.className ?? 'wwc-toolbar'
    root.setAttribute('aria-label', props.label)
    root.replaceChildren(...props.items)
    const active = root.ownerDocument.activeElement
    const tabStop = props.items.find(item => item === active && available(item))
      ?? props.items.find(available)
    for (const item of props.items) item.tabIndex = item === tabStop ? 0 : -1
  }

  update(current)

  return {
    root,
    update,
    close() {
      if (!open) return
      open = false
      root.removeEventListener?.('keydown', onKeyDown)
      removeNode(root)
    },
  }
}
