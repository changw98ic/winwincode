// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export type ButtonVariant = 'default' | 'primary' | 'destructive' | 'ghost'

export interface ButtonProps {
  readonly label: string
  readonly accessibleName?: string
  readonly busy?: boolean
  readonly busyLabel?: string
  readonly className?: string
  readonly disabled?: boolean
  readonly type?: 'button' | 'submit' | 'reset'
  readonly variant?: ButtonVariant
  readonly onActivate?: (event: MouseEvent) => void
}

export interface ButtonMountOptions {
  readonly document: Document
  readonly props: Readonly<ButtonProps>
}

export interface ButtonView extends MountedView<ButtonProps> {
  readonly root: HTMLButtonElement
}

export function mountButton(options: ButtonMountOptions): ButtonView {
  const root = options.document.createElement('button')
  let current = options.props
  let open = true

  root.dataset.wwcComponent = 'button'

  const onClick = (event: MouseEvent) => {
    if (current.busy === true || current.disabled === true) return
    current.onActivate?.(event)
  }
  root.addEventListener('click', onClick)

  function update(props: Readonly<ButtonProps>): void {
    assertMounted(open, 'Button')
    current = props
    root.className = props.className ?? 'wwc-button'
    root.type = props.type ?? 'button'
    root.dataset.variant = props.variant ?? 'default'
    root.disabled = props.disabled === true || props.busy === true
    root.textContent = props.busy === true
      ? (props.busyLabel ?? props.label)
      : props.label
    if (props.busy === true) root.setAttribute('aria-busy', 'true')
    else root.removeAttribute?.('aria-busy')
    if (props.accessibleName === undefined) root.removeAttribute?.('aria-label')
    else root.setAttribute('aria-label', props.accessibleName)
  }

  update(current)

  return {
    root,
    update,
    close() {
      if (!open) return
      open = false
      root.removeEventListener?.('click', onClick)
      removeNode(root)
    },
  }
}
