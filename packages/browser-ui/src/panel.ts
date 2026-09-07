// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export type PanelHeadingLevel = 2 | 3

export interface PanelProps {
  readonly id: string
  readonly title: string
  readonly description?: string
  readonly busy?: boolean
  readonly className?: string
  /**
   * UI-604: the shell owns the one page heading, so a panel that nests below a
   * page's own section title moves down a level instead of sitting beside it.
   */
  readonly headingLevel?: PanelHeadingLevel
}

export interface PanelMountOptions {
  readonly document: Document
  readonly props: Readonly<PanelProps>
}

export interface PanelView extends MountedView<PanelProps> {
  readonly root: HTMLElement
  readonly title: HTMLHeadingElement
  readonly description: HTMLParagraphElement
  readonly content: HTMLDivElement
}

export function mountPanel(options: PanelMountOptions): PanelView {
  const panelId = options.props.id
  const headingLevel = options.props.headingLevel ?? 2
  const root = options.document.createElement('section')
  const header = options.document.createElement('header')
  const title = options.document.createElement(`h${String(headingLevel)}`) as HTMLHeadingElement
  const description = options.document.createElement('p')
  const content = options.document.createElement('div')
  let open = true

  root.dataset.wwcComponent = 'panel'
  header.className = 'wwc-panel-header'
  title.id = `${panelId}-title`
  title.className = 'wwc-panel-title'
  description.className = 'wwc-panel-description'
  content.className = 'wwc-panel-content'
  header.append(title, description)
  root.append(header, content)
  root.setAttribute('aria-labelledby', title.id)

  function update(props: Readonly<PanelProps>): void {
    assertMounted(open, 'Panel')
    if (props.id !== panelId) throw new Error('Panel id cannot change after mount.')
    if ((props.headingLevel ?? 2) !== headingLevel) {
      throw new Error('Panel headingLevel cannot change after mount.')
    }
    root.className = props.className ?? 'wwc-panel'
    title.textContent = props.title
    description.textContent = props.description ?? ''
    description.hidden = props.description === undefined
    if (props.busy === true) root.setAttribute('aria-busy', 'true')
    else root.removeAttribute?.('aria-busy')
  }

  update(options.props)

  return {
    root,
    title,
    description,
    content,
    update,
    close() {
      if (!open) return
      open = false
      removeNode(root)
    },
  }
}
