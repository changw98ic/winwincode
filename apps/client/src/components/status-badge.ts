// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export type StatusTone = 'neutral' | 'info' | 'success' | 'warning' | 'danger'
export type LiveMode = 'off' | 'polite' | 'assertive'

export interface StatusBadgeProps {
  readonly label: string
  readonly tone?: StatusTone
  readonly live?: LiveMode
  readonly className?: string
}

export interface StatusBadgeMountOptions {
  readonly document: Document
  readonly props: Readonly<StatusBadgeProps>
}

export interface StatusBadgeView extends MountedView<StatusBadgeProps> {
  readonly root: HTMLSpanElement
  readonly icon: HTMLSpanElement
  readonly label: HTMLSpanElement
}

const TONE_ICONS: Readonly<Record<StatusTone, string>> = Object.freeze({
  neutral: '•',
  info: 'ⓘ',
  success: '✓',
  warning: '!',
  danger: '×',
})

export function mountStatusBadge(options: StatusBadgeMountOptions): StatusBadgeView {
  const root = options.document.createElement('span')
  const icon = options.document.createElement('span')
  const label = options.document.createElement('span')
  let open = true

  root.dataset.wwcComponent = 'status-badge'
  icon.className = 'wwc-status-badge-icon'
  icon.setAttribute('aria-hidden', 'true')
  label.className = 'wwc-status-badge-label'
  root.append(icon, label)

  function update(props: Readonly<StatusBadgeProps>): void {
    assertMounted(open, 'StatusBadge')
    const tone = props.tone ?? 'neutral'
    const live = props.live ?? 'off'
    root.className = props.className ?? 'wwc-status-badge'
    root.dataset.tone = tone
    icon.textContent = TONE_ICONS[tone]
    label.textContent = props.label
    if (live === 'off') {
      root.removeAttribute?.('role')
      root.removeAttribute?.('aria-live')
    } else {
      root.setAttribute('role', live === 'assertive' ? 'alert' : 'status')
      root.setAttribute('aria-live', live)
    }
  }

  update(options.props)

  return {
    root,
    icon,
    label,
    update,
    close() {
      if (!open) return
      open = false
      removeNode(root)
    },
  }
}
