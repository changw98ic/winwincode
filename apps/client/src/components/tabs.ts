// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export interface TabItem {
  readonly id: string
  readonly label: string
  readonly panelId: string
  readonly disabled?: boolean
}

export interface TabsProps {
  readonly id: string
  readonly label: string
  readonly tabs: readonly TabItem[]
  readonly selectedId: string
  readonly className?: string
  readonly onSelect: (id: string) => void
}

export interface TabsMountOptions {
  readonly document: Document
  readonly props: Readonly<TabsProps>
}

export interface TabsView extends MountedView<TabsProps> {
  readonly root: HTMLDivElement
  tab(id: string): HTMLButtonElement
}

interface MountedTab {
  readonly root: HTMLButtonElement
  readonly onClick: () => void
  readonly onKeyDown: (event: KeyboardEvent) => void
}

export function mountTabs(options: TabsMountOptions): TabsView {
  const tabsId = options.props.id
  const root = options.document.createElement('div')
  const mounted = new Map<string, MountedTab>()
  let current = options.props
  let open = true

  root.dataset.wwcComponent = 'tabs'
  root.setAttribute('role', 'tablist')

  function enabledTabs(): readonly TabItem[] {
    return current.tabs.filter(tab => tab.disabled !== true)
  }

  function selectAdjacent(id: string, key: string): void {
    const enabled = enabledTabs()
    if (enabled.length === 0) return
    const index = enabled.findIndex(tab => tab.id === id)
    let next: TabItem | undefined
    if (key === 'Home') next = enabled[0]
    else if (key === 'End') next = enabled.at(-1)
    else if (key === 'ArrowRight' || key === 'ArrowDown') {
      next = enabled[index < 0 ? 0 : (index + 1) % enabled.length]
    } else if (key === 'ArrowLeft' || key === 'ArrowUp') {
      next = enabled[index < 0 ? enabled.length - 1 : (index - 1 + enabled.length) % enabled.length]
    }
    if (next === undefined) return
    mounted.get(next.id)?.root.focus()
    current.onSelect(next.id)
  }

  function createTab(id: string): MountedTab {
    const button = options.document.createElement('button')
    button.type = 'button'
    button.setAttribute('role', 'tab')
    const onClick = () => {
      const tab = current.tabs.find(candidate => candidate.id === id)
      if (tab === undefined || tab.disabled === true) return
      current.onSelect(id)
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) return
      event.preventDefault()
      selectAdjacent(id, event.key)
    }
    button.addEventListener('click', onClick)
    button.addEventListener('keydown', onKeyDown)
    return { root: button, onClick, onKeyDown }
  }

  function update(props: Readonly<TabsProps>): void {
    assertMounted(open, 'Tabs')
    if (props.id !== tabsId) throw new Error('Tabs id cannot change after mount.')
    const identities = new Set(props.tabs.map(tab => tab.id))
    if (identities.size !== props.tabs.length) throw new Error('Tabs requires unique tab ids.')
    const selected = props.tabs.find(tab => tab.id === props.selectedId)
    if (selected === undefined || selected.disabled === true) {
      throw new Error('Tabs selectedId must identify one enabled tab.')
    }
    current = props
    root.className = props.className ?? 'wwc-tabs'
    root.setAttribute('aria-label', props.label)
    for (const [id, view] of mounted) {
      if (identities.has(id)) continue
      view.root.removeEventListener?.('click', view.onClick)
      view.root.removeEventListener?.('keydown', view.onKeyDown)
      view.root.remove?.()
      mounted.delete(id)
    }
    const ordered = props.tabs.map(tab => {
      const view = mounted.get(tab.id) ?? createTab(tab.id)
      mounted.set(tab.id, view)
      view.root.id = `${tabsId}-${tab.id}`
      view.root.textContent = tab.label
      view.root.disabled = tab.disabled === true
      view.root.tabIndex = tab.id === props.selectedId ? 0 : -1
      view.root.setAttribute('aria-controls', tab.panelId)
      view.root.setAttribute('aria-selected', tab.id === props.selectedId ? 'true' : 'false')
      return view.root
    })
    root.replaceChildren(...ordered)
  }

  update(current)

  return {
    root,
    update,
    tab(id) {
      assertMounted(open, 'Tabs')
      const view = mounted.get(id)
      if (view === undefined) throw new Error(`Unknown tab: ${id}`)
      return view.root
    },
    close() {
      if (!open) return
      open = false
      for (const view of mounted.values()) {
        view.root.removeEventListener?.('click', view.onClick)
        view.root.removeEventListener?.('keydown', view.onKeyDown)
      }
      mounted.clear()
      removeNode(root)
    },
  }
}
