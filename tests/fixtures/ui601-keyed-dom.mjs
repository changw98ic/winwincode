export class TrackedElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  parentNode = null
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  checked = false
  readOnly = false
  required = false
  type = ''
  id = ''
  htmlFor = ''
  rows = 0
  autocomplete = ''
  selectedIndex = -1
  selectionStart = 0
  selectionEnd = 0
  scrollTop = 0
  value = ''
  href = ''
  #textContent = ''

  get childNodes() {
    return this.children
  }

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  append(...children) {
    for (const child of children) this.insertBefore(child, null)
  }

  replaceChildren(...children) {
    for (const child of [...this.children]) child.remove()
    for (const child of children) this.insertBefore(child, null)
  }

  insertBefore(child, reference) {
    child.remove?.()
    const referenceIndex = reference === null
      ? this.children.length
      : this.children.indexOf(reference)
    const index = referenceIndex < 0 ? this.children.length : referenceIndex
    this.children.splice(index, 0, child)
    child.parentNode = this
    return child
  }

  remove() {
    if (this.parentNode === null) return
    const index = this.parentNode.children.indexOf(this)
    if (index >= 0) this.parentNode.children.splice(index, 1)
    this.parentNode = null
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value))
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? new Set()
    listeners.add(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    this.listeners.get(name)?.delete(listener)
  }

  emit(name, values = {}) {
    let prevented = false
    const event = {
      preventDefault() { prevented = true },
      ...values,
    }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
    return prevented
  }

  requestSubmit() {
    this.emit('submit')
  }
}

export class TrackedDocument {
  activeElement = null
  elements = []

  createElement(tagName) {
    const element = new TrackedElement(this, tagName)
    this.elements.push(element)
    return element
  }

  listenerCount() {
    return this.elements.reduce((total, element) => (
      total + [...element.listeners.values()].reduce(
        (elementTotal, listeners) => elementTotal + listeners.size,
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

export function treeNodeCount(node) {
  return 1 + node.children.reduce((total, child) => total + treeNodeCount(child), 0)
}
