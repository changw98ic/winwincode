// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'
import type { StatusTone } from './status-badge.js'

export interface MetricProps {
  readonly label: string
  readonly value: string
  readonly hint?: string
  readonly tone?: StatusTone
  readonly className?: string
}

export interface MetricMountOptions {
  readonly document: Document
  readonly props: Readonly<MetricProps>
}

export interface MetricView extends MountedView<MetricProps> {
  readonly root: HTMLDListElement
  readonly label: HTMLElement
  readonly value: HTMLElement
  readonly hint: HTMLElement
}

export function mountMetric(options: MetricMountOptions): MetricView {
  const root = options.document.createElement('dl')
  const label = options.document.createElement('dt')
  const value = options.document.createElement('dd')
  const hint = options.document.createElement('dd')
  let open = true

  root.dataset.wwcComponent = 'metric'
  label.className = 'wwc-metric-label'
  value.className = 'wwc-metric-value'
  hint.className = 'wwc-metric-hint'
  root.append(label, value, hint)

  function update(props: Readonly<MetricProps>): void {
    assertMounted(open, 'Metric')
    root.className = props.className ?? 'wwc-metric'
    root.dataset.tone = props.tone ?? 'neutral'
    label.textContent = props.label
    value.textContent = props.value
    hint.textContent = props.hint ?? ''
    hint.hidden = props.hint === undefined
  }

  update(options.props)

  return {
    root,
    label,
    value,
    hint,
    update,
    close() {
      if (!open) return
      open = false
      removeNode(root)
    },
  }
}
