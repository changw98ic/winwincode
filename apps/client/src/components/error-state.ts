// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export type ErrorStateHeadingLevel = 2 | 3 | 4

export interface ErrorStateProps {
  readonly title: string
  readonly message: string
  readonly detail?: string
  readonly actions?: readonly HTMLElement[]
  readonly visible?: boolean
  readonly className?: string
  /** UI-604: follow the level of the panel the state nests inside. */
  readonly headingLevel?: ErrorStateHeadingLevel
}

export interface ErrorStateMountOptions {
  readonly document: Document
  readonly props: Readonly<ErrorStateProps>
}

export interface ErrorStateView extends MountedView<ErrorStateProps> {
  readonly root: HTMLElement
  readonly icon: HTMLSpanElement
  readonly title: HTMLHeadingElement
  readonly message: HTMLParagraphElement
  readonly detail: HTMLParagraphElement
  readonly actions: HTMLDivElement
}

export function mountErrorState(options: ErrorStateMountOptions): ErrorStateView {
  const root = options.document.createElement('section')
  const icon = options.document.createElement('span')
  const content = options.document.createElement('div')
  const headingLevel = options.props.headingLevel ?? 2
  const title = options.document.createElement(`h${String(headingLevel)}`) as HTMLHeadingElement
  const message = options.document.createElement('p')
  const detail = options.document.createElement('p')
  const actions = options.document.createElement('div')
  let open = true

  root.dataset.wwcComponent = 'error-state'
  root.dataset.tone = 'danger'
  root.setAttribute('role', 'alert')
  icon.className = 'wwc-error-state-icon'
  icon.setAttribute('aria-hidden', 'true')
  icon.textContent = '×'
  content.className = 'wwc-error-state-content'
  title.className = 'wwc-error-state-title'
  message.className = 'wwc-error-state-message'
  detail.className = 'wwc-error-state-detail'
  actions.className = 'wwc-error-state-actions'
  content.append(title, message, detail, actions)
  root.append(icon, content)

  function update(props: Readonly<ErrorStateProps>): void {
    assertMounted(open, 'ErrorState')
    if ((props.headingLevel ?? 2) !== headingLevel) {
      throw new Error('ErrorState headingLevel cannot change after mount.')
    }
    root.className = props.className ?? 'wwc-error-state'
    root.hidden = props.visible === false
    title.textContent = props.title
    message.textContent = props.message
    detail.textContent = props.detail ?? ''
    detail.hidden = props.detail === undefined
    actions.replaceChildren(...(props.actions ?? []))
    actions.hidden = props.actions === undefined || props.actions.length === 0
  }

  update(options.props)

  return {
    root,
    icon,
    title,
    message,
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
