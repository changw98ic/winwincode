// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export type SplitPaneOrientation = 'horizontal' | 'vertical'

export interface SplitPaneProps {
  readonly primary: HTMLElement
  readonly primaryLabel: string
  readonly secondary: HTMLElement
  readonly secondaryLabel: string
  readonly orientation?: SplitPaneOrientation
  readonly secondaryHidden?: boolean
  readonly className?: string
}

export interface SplitPaneMountOptions {
  readonly document: Document
  readonly props: Readonly<SplitPaneProps>
}

export interface SplitPaneView extends MountedView<SplitPaneProps> {
  readonly root: HTMLDivElement
  readonly primary: HTMLElement
  readonly secondary: HTMLElement
}

export function mountSplitPane(options: SplitPaneMountOptions): SplitPaneView {
  const root = options.document.createElement('div')
  const primary = options.document.createElement('section')
  const secondary = options.document.createElement('section')
  let primaryContent: HTMLElement | null = null
  let secondaryContent: HTMLElement | null = null
  let open = true

  root.dataset.wwcComponent = 'split-pane'
  primary.className = 'wwc-split-pane-primary'
  primary.setAttribute('role', 'region')
  secondary.className = 'wwc-split-pane-secondary'
  secondary.setAttribute('role', 'region')
  root.append(primary, secondary)

  function update(props: Readonly<SplitPaneProps>): void {
    assertMounted(open, 'SplitPane')
    root.className = props.className ?? 'wwc-split-pane'
    root.dataset.orientation = props.orientation ?? 'horizontal'
    primary.setAttribute('aria-label', props.primaryLabel)
    secondary.setAttribute('aria-label', props.secondaryLabel)
    secondary.hidden = props.secondaryHidden === true
    if (primaryContent !== props.primary) {
      primaryContent = props.primary
      primary.replaceChildren(props.primary)
    }
    if (secondaryContent !== props.secondary) {
      secondaryContent = props.secondary
      secondary.replaceChildren(props.secondary)
    }
  }

  update(options.props)

  return {
    root,
    primary,
    secondary,
    update,
    close() {
      if (!open) return
      open = false
      removeNode(root)
    },
  }
}
