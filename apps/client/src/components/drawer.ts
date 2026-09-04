// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export interface DrawerProps {
  readonly id: string
  readonly title: string
  readonly open: boolean
  readonly content: HTMLElement
  readonly closeLabel?: string
  readonly className?: string
  readonly onClose: () => void
}

export interface DrawerMountOptions {
  readonly document: Document
  readonly props: Readonly<DrawerProps>
}

export interface DrawerView extends MountedView<DrawerProps> {
  readonly root: HTMLElement
  readonly title: HTMLHeadingElement
  readonly closeButton: HTMLButtonElement
  readonly content: HTMLDivElement
}

function focusElement(value: Element | null): value is HTMLElement {
  return value !== null && typeof Reflect.get(value, 'focus') === 'function'
}

export function mountDrawer(options: DrawerMountOptions): DrawerView {
  const drawerId = options.props.id
  const root = options.document.createElement('aside')
  const header = options.document.createElement('header')
  const title = options.document.createElement('h2')
  const closeButton = options.document.createElement('button')
  const content = options.document.createElement('div')
  let current = options.props
  let mountedContent: HTMLElement | null = null
  let previouslyFocused: HTMLElement | null = null
  let wasOpen = false
  let open = true

  root.dataset.wwcComponent = 'drawer'
  root.setAttribute('role', 'dialog')
  root.setAttribute('aria-modal', 'false')
  title.id = `${drawerId}-title`
  title.className = 'wwc-drawer-title'
  closeButton.type = 'button'
  closeButton.className = 'wwc-drawer-close'
  closeButton.textContent = '×'
  content.className = 'wwc-drawer-content'
  header.className = 'wwc-drawer-header'
  header.append(title, closeButton)
  root.append(header, content)
  root.setAttribute('aria-labelledby', title.id)

  const requestClose = () => { current.onClose() }
  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key !== 'Escape' || current.open !== true) return
    event.preventDefault()
    requestClose()
  }
  closeButton.addEventListener('click', requestClose)
  root.addEventListener('keydown', onKeyDown)

  function update(props: Readonly<DrawerProps>): void {
    assertMounted(open, 'Drawer')
    if (props.id !== drawerId) throw new Error('Drawer id cannot change after mount.')
    current = props
    root.className = props.className ?? 'wwc-drawer'
    root.hidden = !props.open
    title.textContent = props.title
    closeButton.setAttribute('aria-label', props.closeLabel ?? 'Close drawer')
    if (mountedContent !== props.content) {
      mountedContent = props.content
      content.replaceChildren(props.content)
    }
    if (props.open && !wasOpen) {
      previouslyFocused = focusElement(options.document.activeElement)
        ? options.document.activeElement
        : null
      closeButton.focus()
    } else if (!props.open && wasOpen) {
      previouslyFocused?.focus()
      previouslyFocused = null
    }
    wasOpen = props.open
  }

  update(current)

  return {
    root,
    title,
    closeButton,
    content,
    update,
    close() {
      if (!open) return
      open = false
      closeButton.removeEventListener?.('click', requestClose)
      root.removeEventListener?.('keydown', onKeyDown)
      if (wasOpen) previouslyFocused?.focus()
      previouslyFocused = null
      removeNode(root)
    },
  }
}
