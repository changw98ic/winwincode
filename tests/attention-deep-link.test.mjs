// [UI-100.3] Attention entry deep links: input and approval decisions open the
// Chat session that raised them; a Delivery-bound business Attention renders no
// action instead of a dead end, because the community client has no standalone
// delivery acceptance surface.
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.attention-center-tests.json',
    '--pretty',
    'false',
    '--incremental',
    'false',
  ],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `Attention deep-link area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/attention-center-tests')
// Plain module paths keep one module identity across the page, the view-model,
// and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const pageModule = await cachedModule('attention-center-page.js')

const {
  attentionCenterItemHash,
  mountAttentionCenterPage,
} = pageModule

const scope = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const scopeSelection = { ...scope }
const productSessionId = 'psn_00000000000000000000000001'
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'str_00000000000000000000000001'

function centerItem(overrides = {}) {
  return {
    kind: 'input',
    id: 'inp_00000000000000000000000001',
    title: 'Describe the exact local change',
    blocking: false,
    expired: false,
    bindingValid: true,
    urgency: 'pending',
    createdAt: null,
    expiresAt: '2026-09-03T04:00:00.000Z',
    productSessionId,
    sessionTitle: 'Session psn_00000000000000000000000001',
    stageRunId,
    executionJobId: 'job_00000000000000000000000001',
    deliveryId: null,
    deliveryTitle: null,
    candidateBound: false,
    revision: 4,
    ...overrides,
  }
}

function parametersOf(hash) {
  const query = hash.slice(hash.indexOf('?') + 1)
  return Object.fromEntries(new URLSearchParams(query))
}

test('a decision links the Chat session that raised it, with the exact Scope', () => {
  assert.equal(
    attentionCenterItemHash(centerItem(), scopeSelection),
    `#/chat?session=${productSessionId}`
      + `&organizationId=${scope.organizationId}&workspaceId=${scope.workspaceId}`
      + `&projectId=${scope.projectId}&repositoryId=${scope.repositoryId}`,
  )
})

test('a business Attention opens the run page for its acceptance (设计稿 06)', () => {
  assert.equal(attentionCenterItemHash(centerItem({
    kind: 'attention',
    id: 'att_00000000000000000000000001',
    productSessionId: null,
    deliveryId,
    deliveryTitle: 'Delivery under attention',
    candidateBound: true,
  }), scopeSelection), `#/home/task-run?organizationId=${scope.organizationId}`
    + `&workspaceId=${scope.workspaceId}&projectId=${scope.projectId}`
    + `&repositoryId=${scope.repositoryId}`)
})

test('a decision without a Session id links nothing instead of fabricating one', () => {
  assert.equal(attentionCenterItemHash(centerItem({
    productSessionId: null,
    sessionTitle: null,
  }), scopeSelection), null)
})

class FakeElement {
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
  type = ''
  value = ''
  id = ''
  tabIndex = 0
  title = ''
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

  get href() { return this.getAttribute('href') ?? '' }

  set href(value) { this.setAttribute('href', value) }

  append(...children) {
    for (const child of children) this.insertBefore(child, null)
  }

  replaceChildren(...children) {
    for (const child of [...this.children]) child.remove()
    for (const child of children) this.insertBefore(child, null)
  }

  insertBefore(child, reference) {
    child.remove?.()
    const index = reference === null ? this.children.length : this.children.indexOf(reference)
    this.children.splice(index < 0 ? this.children.length : index, 0, child)
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

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }

  removeEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    this.listeners.set(name, current.filter(candidate => candidate !== listener))
  }

  dispatch(name) {
    const event = { preventDefault() {} }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function allByClass(rootElement, className) {
  return descendants(rootElement).filter(node => node.className === className)
}

function byClass(rootElement, className) {
  const match = allByClass(rootElement, className)[0]
  assert.notEqual(match, undefined, `missing .${className}`)
  return match
}

function fakeModel(initialStateValue) {
  let state = initialStateValue
  const listeners = new Set()
  let closeCalls = 0
  return {
    get state() { return state },
    get closeCalls() { return closeCalls },
    subscribe(listener) {
      listeners.add(listener)
      listener(state)
      return () => { listeners.delete(listener) }
    },
    publish(next) {
      state = next
      for (const listener of listeners) listener(state)
    },
    async start() {},
    async refresh() {},
    cancelPending() {},
    reconnect() {},
    close() { closeCalls += 1 },
  }
}

function mountedCards(rootElement) {
  return [...byClass(rootElement, 'wwc-attention-center-list').children]
}

function cardAction(card) {
  return allByClass(card, 'wwc-attention-card-action')[0]
}

test('the mounted center links decisions to their Chat session and hides dead-end actions', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const state = {
    status: 'ready',
    realtime: 'subscribed',
    items: [
      // A decision raised inside one Chat session.
      centerItem(),
      // A business Attention bound to a Delivery: no acceptance surface exists.
      centerItem({
        kind: 'attention',
        id: 'att_00000000000000000000000001',
        title: 'Review the proposed delivery scope',
        blocking: true,
        urgency: 'blocking',
        createdAt: '2026-09-03T02:58:00.000Z',
        expiresAt: null,
        productSessionId: null,
        sessionTitle: null,
        executionJobId: null,
        deliveryId,
        deliveryTitle: 'Delivery under attention',
        candidateBound: true,
        revision: 12,
      }),
      // An expired decision: the entry fails closed with no href at all.
      centerItem({
        id: 'inp_00000000000000000000000002',
        title: 'Too late',
        expired: true,
        urgency: 'expired',
        expiresAt: '2026-09-03T02:00:00.000Z',
      }),
    ],
    origins: [],
    error: null,
  }
  const model = fakeModel(state)
  const mounted = mountAttentionCenterPage({
    root: rootElement,
    model,
    scopeSelection,
    ownsModel: true,
  })

  const cards = mountedCards(rootElement)
  assert.equal(cards.length, 3)
  const decisionCard = cards.find(node => node.dataset.kind === 'input')
  const attentionCard = cards.find(node => node.dataset.kind === 'attention')
  const expiredCard = cards.find(node => node.dataset.urgency === 'expired')

  const decisionParameters = parametersOf(cardAction(decisionCard).href)
  assert.equal(cardAction(decisionCard).href.startsWith('#/chat?'), true)
  assert.equal(decisionParameters.session, productSessionId)

  // 设计稿 06:待验收行保留「验收交付」动作,打开该交付的运行页。
  assert.equal(cardAction(attentionCard).hidden, false)
  assert.equal(cardAction(attentionCard).getAttribute('href')?.startsWith('#/home/task-run'), true)

  assert.equal(cardAction(expiredCard).hidden, true)
  assert.equal(cardAction(expiredCard).getAttribute('href'), null)

  // No execution-origin link exists without a delivery workbench surface.
  assert.equal(allByClass(decisionCard, 'wwc-attention-card-origin').length, 0)
  mounted.close()
  assert.equal(model.closeCalls, 1)
})
