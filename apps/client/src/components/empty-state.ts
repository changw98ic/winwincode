// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export interface EmptyStateProps {
  readonly title: string
  readonly detail: string
  readonly action?: HTMLElement
  readonly className?: string
}

export interface EmptyStateMountOptions {
  readonly document: Document
  readonly props: Readonly<EmptyStateProps>
}

export interface EmptyStateView extends MountedView<EmptyStateProps> {
  readonly root: HTMLElement
  readonly title: HTMLHeadingElement
  readonly detail: HTMLParagraphElement
  readonly actions: HTMLDivElement
}

export function mountEmptyState(options: EmptyStateMountOptions): EmptyStateView {
  const root = options.document.createElement('section')
  const title = options.document.createElement('h2')
  const detail = options.document.createElement('p')
  const actions = options.document.createElement('div')
  let open = true

  root.dataset.wwcComponent = 'empty-state'
  root.setAttribute('role', 'status')
  title.className = 'wwc-empty-state-title'
  detail.className = 'wwc-empty-state-detail'
  actions.className = 'wwc-empty-state-actions'
  root.append(title, detail, actions)

  function update(props: Readonly<EmptyStateProps>): void {
    assertMounted(open, 'EmptyState')
    root.className = props.className ?? 'wwc-empty-state'
    title.textContent = props.title
    detail.textContent = props.detail
    actions.replaceChildren(...(props.action === undefined ? [] : [props.action]))
    actions.hidden = props.action === undefined
  }

  update(options.props)

  return {
    root,
    title,
    detail,
    actions,
    update,
    close() {
      if (!open) return
      open = false
      removeNode(root)
    },
  }
}
