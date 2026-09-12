// SPDX-License-Identifier: Apache-2.0

/**
 * UI-605 large-data fixtures and the recorded performance baseline.
 *
 * The fixture records the DOM, interaction, and scroll budgets used by the
 * shared windowed-list checks.
 */

export const LARGE_DELIVERY_COUNT = 5000

export const LARGE_DATA_CORPUS = Object.freeze({
  deliveries: LARGE_DELIVERY_COUNT,
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
