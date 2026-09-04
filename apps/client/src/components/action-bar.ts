// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export type ActionBarAlignment = 'start' | 'end' | 'space-between'

export interface ActionBarProps {
  readonly label: string
  readonly items: readonly HTMLElement[]
  readonly align?: ActionBarAlignment
  readonly className?: string
}

export interface ActionBarMountOptions {
  readonly document: Document
  readonly props: Readonly<ActionBarProps>
}

export interface ActionBarView extends MountedView<ActionBarProps> {
  readonly root: HTMLDivElement
}

export function mountActionBar(options: ActionBarMountOptions): ActionBarView {
  const root = options.document.createElement('div')
  let open = true

  root.dataset.wwcComponent = 'action-bar'
  root.setAttribute('role', 'group')

  function update(props: Readonly<ActionBarProps>): void {
    assertMounted(open, 'ActionBar')
    root.className = props.className ?? 'wwc-action-bar'
    root.dataset.align = props.align ?? 'start'
    root.setAttribute('aria-label', props.label)
    root.replaceChildren(...props.items)
  }

  update(options.props)

  return {
    root,
    update,
    close() {
      if (!open) return
      open = false
      removeNode(root)
    },
  }
}
