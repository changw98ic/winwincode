// SPDX-License-Identifier: Apache-2.0

/**
 * UI-605 large-data fixtures and the recorded performance baseline.
 *
 * The fixtures are deterministic and use only canonical generated-schema
 * identities, so an enterprise-sized Delivery, Task, StageRun, Runtime
 * activity, changed-file, Diff, Evidence, and log corpus can be mounted
 * without a Control Plane.  The baseline records the DOM, interaction, and
 * scroll budgets the virtualized surfaces must meet; the UI-605 suites assert
 * against these numbers instead of restating them.
 */

export const LARGE_DELIVERY_COUNT = 5000

export const LARGE_DATA_CORPUS = Object.freeze({
  deliveries: LARGE_DELIVERY_COUNT,
  deliveryTasks: 600,
  stageRuns: 400,
  runtimeActivities: 2400,
  changedFiles: 1800,
  diffLines: 24_000,
  evidenceRows: 900,
  logLines: 60_000,
})

/**
 * Recorded UI-605 budgets.  `rendered.listRows` is a hard DOM cap: a windowed
 * list renders exactly this many rows no matter how many records are loaded.
 * The millisecond budgets are intentionally wide so they stay stable on
 * shared runners while still failing a page that rebuilds the whole corpus.
 */
export const LARGE_DATA_PERFORMANCE_BASELINE = Object.freeze({
  rendered: Object.freeze({
    listRows: 36,
    pageDomNodes: 500,
    scrollSteps: 40,
  }),
  millis: Object.freeze({
    firstInteraction: 2_500,
    scroll: 5_000,
  }),
})

/** One canonical Delivery identity: `dlv_` plus 22 lowercase digits. */
export function deliveryIdFor(index) {
  return `dlv_${String(index).padStart(22, '0')}`
}

const STATUSES = Object.freeze([
  'draft',
  'clarifying',
  'ready',
  'planning',
  'plan-review',
  'executing',
  'verifying',
  'reworking',
  'ready-to-deliver',
  'delivered',
])

/** Enterprise-sized Delivery projections with stable identities and titles. */
export function largeDeliverySummaries(count = LARGE_DELIVERY_COUNT) {
  return Array.from({ length: count }, (_, index) => ({
    deliveryId: deliveryIdFor(index + 1),
    revision: (index % 7) + 1,
    status: STATUSES[index % STATUSES.length],
    title: `Enterprise delivery ${String(index + 1)} — ${index % 3 === 0 ? 'kernel' : index % 3 === 1 ? 'control plane' : 'client'} workstream`,
    openAttentionCount: index % 11 === 0 ? 1 : 0,
  }))
}

/** Enterprise-sized Delivery tasks, StageRuns, and Runtime activity frames. */
export function largeDeliveryTasks(count = LARGE_DATA_CORPUS.deliveryTasks) {
  return Array.from({ length: count }, (_, index) => ({
    id: `task:${String(index + 1).padStart(4, '0')}`,
    title: `Enterprise task ${String(index + 1)}`,
    status: index % 4 === 0 ? 'done' : index % 4 === 1 ? 'active' : 'pending',
  }))
}

export function largeStageRuns(count = LARGE_DATA_CORPUS.stageRuns) {
  return Array.from({ length: count }, (_, index) => ({
    id: `run_${String(index + 1).padStart(22, '0')}`,
    stage: STATUSES[(index + 5) % STATUSES.length],
    role: index % 2 === 0 ? 'implementer' : 'reviewer',
    status: index % 5 === 0 ? 'succeeded' : 'running',
  }))
}

export function largeRuntimeActivities(count = LARGE_DATA_CORPUS.runtimeActivities) {
  return Array.from({ length: count }, (_, index) => ({
    id: `activity:${String(index + 1).padStart(6, '0')}`,
    kind: index % 3 === 0 ? 'command' : index % 3 === 1 ? 'observation' : 'evidence',
    label: `Runtime activity ${String(index + 1)}`,
    sequence: index + 1,
  }))
}

/** Enterprise-sized changed-file corpus for the Candidate file tree. */
export function largeChangedFiles(count = LARGE_DATA_CORPUS.changedFiles) {
  const statuses = ['added', 'modified', 'deleted', 'renamed', 'copied', 'type_changed']
  return Array.from({ length: count }, (_, index) => ({
    path: `apps/module-${String(index % 40)}/src/area-${String(index % 7)}/file-${String(index + 1)}.ts`,
    previousPath: index % 9 === 0 ? `apps/legacy/file-${String(index + 1)}.ts` : null,
    status: statuses[index % statuses.length],
    additions: (index % 90) + 1,
    deletions: index % 40,
    binary: false,
    encoding: 'utf-8',
  }))
}

/** One large unified Git Diff with `lines` body lines across many hunks. */
export function largeUnifiedDiff(lines = LARGE_DATA_CORPUS.diffLines) {
  const parts = ['--- a/apps/client/src/large-sample.ts', '+++ b/apps/client/src/large-sample.ts']
  let remaining = lines
  let hunk = 0
  while (remaining > 0) {
    hunk += 1
    const body = Math.min(remaining, 400)
    parts.push(`@@ -${String(hunk * 400)},${String(body)} +${String(hunk * 400)},${String(body)} @@ export function range${String(hunk)}()`)
    for (let index = 0; index < body; index += 1) {
      if (index % 5 === 0) parts.push(`-const before${String(index)} = ${String(index)}`)
      else if (index % 5 === 1) parts.push(`+const after${String(index)} = ${String(index)}`)
      else parts.push(` const shared${String(index)} = ${String(index)}`)
    }
    remaining -= body
  }
  return `${parts.join('\n')}\n`
}

/** Enterprise-sized Evidence rows covering every generated Evidence type. */
export function largeEvidenceRows(count = LARGE_DATA_CORPUS.evidenceRows) {
  const types = ['test', 'command', 'runtime_event', 'diff']
  const outcomes = ['succeeded', 'failed', 'infrastructure_failed', 'observed']
  return Array.from({ length: count }, (_, index) => ({
    id: `evd_${String(index + 1).padStart(22, '0')}`,
    type: types[index % types.length],
    title: `Enterprise evidence ${String(index + 1)}`,
    sourceRef: `artifact://${String(index + 1)}`,
    outcome: outcomes[index % outcomes.length],
  }))
}

/** One large runtime log payload: `lines` lines of bounded-width text. */
export function largeLogText(lines = LARGE_DATA_CORPUS.logLines) {
  const parts = []
  for (let index = 0; index < lines; index += 1) {
    parts.push(`${String(index + 1).padStart(6, '0')} runtime frame ${String(index + 1)} status=ok worker=worker-1 stage=executing`)
  }
  return `${parts.join('\n')}\n`
}

/** DOM-counter element that records every allocated node and listener. */
class Ui605Element {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
    this.attributes = new Map()
    this.children = []
    this.parentNode = null
    this.listeners = new Map()
    this.dataset = {}
    this.className = ''
    this.disabled = false
    this.hidden = false
    this.checked = false
    this.draggable = false
    this.tabIndex = -1
    this.type = ''
    this.value = ''
    this.href = ''
    this.scrollTop = 0
    this.scrollHeight = 0
    this.clientHeight = 0
    this.style = {
      values: new Map(),
      setProperty(name, value) { this.values.set(name, String(value)) },
      getPropertyValue(name) { return this.values.get(name) ?? '' },
    }
  }

  #textContent = ''

  get childNodes() { return this.children }

  get textContent() {
    return this.#textContent + this.children.map(child => child.textContent).join('')
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get firstChild() { return this.children[0] ?? null }

  append(...children) {
    for (const child of children) this.insertBefore(child, null)
  }

  replaceChildren(...children) {
    for (const child of [...this.children]) child.remove()
    for (const child of children) this.insertBefore(child, null)
  }

  insertBefore(child, reference) {
    child.remove?.()
    const index = reference === null || reference === undefined
      ? this.children.length
      : this.children.indexOf(reference)
    this.children.splice(index < 0 ? this.children.length : index, 0, child)
    child.parentNode = this
    this.ownerDocument.created += 1
    return child
  }

  remove() {
    if (this.parentNode === null) return
    const index = this.parentNode.children.indexOf(this)
    if (index >= 0) this.parentNode.children.splice(index, 1)
    this.parentNode = null
  }

  setAttribute(name, value) { this.attributes.set(name, String(value)) }

  getAttribute(name) { return this.attributes.get(name) ?? null }

  removeAttribute(name) { this.attributes.delete(name) }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    this.listeners.set(
      name,
      (this.listeners.get(name) ?? []).filter(candidate => candidate !== listener),
    )
  }

  /** Dispatch one DOM event to this node and every ancestor. */
  emit(name, values = {}) {
    const event = { target: this, preventDefault() {}, ...values }
    let current = this
    while (current !== null) {
      for (const listener of current.listeners.get(name) ?? []) listener(event)
      current = current.parentNode
    }
  }

  click() { this.emit('click') }

  focus() { this.ownerDocument.activeElement = this }
}

/** Counting document: every created element is tracked for DOM budgets. */
export class Ui605Document {
  activeElement = null
  elements = []
  created = 0

  createElement(tagName) {
    const element = new Ui605Element(this, tagName)
    this.elements.push(element)
    return element
  }

  listenerCount() {
    return this.elements.reduce((total, element) => (
      total + [...element.listeners.values()].reduce(
        (elementTotal, listeners) => elementTotal + listeners.length,
        0,
      )
    ), 0)
  }
}

export function findByClass(node, className) {
  if (node.className === className) return node
  for (const child of node.children) {
    const match = findByClass(child, className)
    if (match !== null) return match
  }
  return null
}

/** Depth-first search for one token of a space-separated class attribute. */
export function findByClassName(node, className) {
  if (node.className.split(/\s+/u).includes(className)) return node
  for (const child of node.children) {
    const match = findByClassName(child, className)
    if (match !== null) return match
  }
  return null
}

export function findAllByClass(node, className, matches = []) {
  if (node.className === className) matches.push(node)
  for (const child of node.children) findAllByClass(child, className, matches)
  return matches
}

/** Count every element reachable from `node`, including `node`. */
export function treeNodeCount(node) {
  return 1 + node.children.reduce((total, child) => total + treeNodeCount(child), 0)
}
