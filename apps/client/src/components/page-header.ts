// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export type PageHeadingLevel = 1 | 2 | 3

export interface PageHeaderProps {
  readonly title: string
  readonly description?: string
  readonly eyebrow?: string
  /** Fixed for the lifetime of one mounted header. */
  readonly headingLevel?: PageHeadingLevel
  readonly className?: string
}

export interface PageHeaderMountOptions {
  readonly document: Document
  readonly props: Readonly<PageHeaderProps>
}

export interface PageHeaderView extends MountedView<PageHeaderProps> {
  readonly root: HTMLElement
  readonly title: HTMLHeadingElement
  readonly description: HTMLParagraphElement
  readonly eyebrow: HTMLParagraphElement
}

export function mountPageHeader(options: PageHeaderMountOptions): PageHeaderView {
  const headingLevel = options.props.headingLevel ?? 1
  const root = options.document.createElement('header')
  const eyebrow = options.document.createElement('p')
  const title = options.document.createElement(`h${String(headingLevel)}`) as HTMLHeadingElement
  const description = options.document.createElement('p')
  let open = true

  root.dataset.wwcComponent = 'page-header'
  eyebrow.className = 'wwc-page-header-eyebrow'
  title.className = 'wwc-page-header-title'
  description.className = 'wwc-page-header-description'
  root.append(eyebrow, title, description)

  function update(props: Readonly<PageHeaderProps>): void {
    assertMounted(open, 'PageHeader')
    if ((props.headingLevel ?? 1) !== headingLevel) {
      throw new Error('PageHeader headingLevel cannot change after mount.')
    }
    root.className = props.className ?? 'wwc-page-header'
    title.textContent = props.title
    eyebrow.textContent = props.eyebrow ?? ''
    eyebrow.hidden = props.eyebrow === undefined
    description.textContent = props.description ?? ''
    description.hidden = props.description === undefined
  }

  update(options.props)

  return {
    root,
    title,
    description,
    eyebrow,
    update,
    close() {
      if (!open) return
      open = false
      removeNode(root)
    },
  }
}
