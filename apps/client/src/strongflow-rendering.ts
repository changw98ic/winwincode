// SPDX-License-Identifier: Apache-2.0

export interface StrongFlowRenderLimits {
  readonly deliveries: number
  readonly tasks: number
  readonly stages: number
  readonly attention: number
  readonly evidence: number
  readonly runtimeSessions: number
  readonly graphNodes: number
  readonly graphEdges: number
  readonly activities: number
}

export const DEFAULT_STRONGFLOW_RENDER_LIMITS: StrongFlowRenderLimits = Object.freeze({
  deliveries: 50,
  tasks: 100,
  stages: 50,
  attention: 50,
  evidence: 100,
  runtimeSessions: 50,
  graphNodes: 100,
  graphEdges: 200,
  activities: 100,
})

/**
 * Exact rendered height of one windowed list row in CSS pixels.  It is also
 * written onto the scroller as the `--wwc-window-row-height` custom property,
 * so the scroll math and the painted row height cannot diverge.  `tokens.css`
 * declares the same value as the design token fallback and the UI-605 suite
 * asserts the two agree.
 */
export const STRONGFLOW_WINDOW_ROW_HEIGHT_PX = 60

/** Rows a windowed list always renders for the visible viewport. */
export const STRONGFLOW_WINDOW_VIEWPORT_ROWS = 24

/** Extra rows rendered above and below the viewport to hide scroll work. */
export const STRONGFLOW_WINDOW_OVERSCAN_ROWS = 6

export interface BoundedItems<Value> {
  readonly items: readonly Value[]
  readonly omitted: number
}

export function boundedItems<Value>(
  values: readonly Value[],
  limit: number,
): BoundedItems<Value> {
  if (!Number.isInteger(limit) || limit < 1 || limit > 500) {
    throw new RangeError('StrongFlow render limits must be integers between 1 and 500.')
  }
  return Object.freeze({
    items: Object.freeze(values.slice(0, limit)),
    omitted: Math.max(0, values.length - limit),
  })
}

export function boundedEdgesForNodes<Value extends {
  readonly from: string
  readonly to: string
}>(
  edges: readonly Value[],
  retainedNodeIds: ReadonlySet<string>,
  limit: number,
): BoundedItems<Value> {
  const joinable = edges.filter(
    edge => retainedNodeIds.has(edge.from) && retainedNodeIds.has(edge.to),
  )
  const bounded = boundedItems(joinable, limit)
  return Object.freeze({
    items: bounded.items,
    omitted: edges.length - joinable.length + bounded.omitted,
  })
}

export function strongFlowElement<K extends keyof HTMLElementTagNameMap>(
  document: Document,
  tag: K,
  className: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  node.className = className
  return node
}

export function appendOmittedCount(
  document: Document,
  root: HTMLElement,
  omitted: number,
  label: string,
): void {
  if (omitted === 0) return
  const note = strongFlowElement(document, 'p', 'wwc-strongflow-omitted')
  note.textContent = `${String(omitted)} more ${label} not rendered.`
  root.append(note)
}
