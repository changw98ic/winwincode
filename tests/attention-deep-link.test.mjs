// [UI-100.3] Attention entry deep links: every card opens the authoritative
// run page / StageRun context through the canonical typed route boundary, and
// a decision link carries the exact execution origin with it.
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
  attentionCenterOriginHash,
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
const otherDeliveryId = 'dlv_00000000000000000000000009'
const otherStageRunId = 'str_00000000000000000000000009'

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

function origin(overrides = {}) {
  return {
    deliveryId,
    deliveryTitle: 'Delivery under attention',
    deliveryRevision: 12,
    activeStageRunId: stageRunId,
    ...overrides,
  }
}

function parametersOf(hash) {
  const query = hash.slice(hash.indexOf('?') + 1)
  return Object.fromEntries(new URLSearchParams(query))
}

test('a business Attention opens the run page through the typed StrongFlow route', () => {
  const hash = attentionCenterItemHash(centerItem({
    kind: 'attention',
    id: 'att_00000000000000000000000001',
    productSessionId: null,
    deliveryId,
    deliveryTitle: 'Delivery under attention',
    candidateBound: true,
  }), scopeSelection)
  assert.match(hash, /^#\/strongflow\?/u)
  const parameters = parametersOf(hash)
  assert.equal(parameters.delivery, deliveryId, 'the Delivery identity is present')
  assert.equal(parameters.stageRun, stageRunId, 'the StageRun identity is present')
  assert.equal(parameters.view, 'unified', 'the canonical route formats the view itself')
  assert.equal(parameters.repositoryId, scope.repositoryId, 'the exact Scope is preserved')
  assert.equal(parameters.session, undefined, 'a business Attention fabricates no Session id')
})

test('a StageRun-bound Attention links the run page; a Delivery-bound one stays Delivery-level', () => {
  const withStageRun = parametersOf(attentionCenterItemHash(centerItem({
    kind: 'attention',
    deliveryId,
    stageRunId,
  }), scopeSelection))
  assert.equal(withStageRun.stageRun, stageRunId)
  const deliveryBound = parametersOf(attentionCenterItemHash(centerItem({
    kind: 'attention',
    deliveryId,
    stageRunId: null,
  }), scopeSelection))
  assert.equal(deliveryBound.delivery, deliveryId)
  assert.equal(deliveryBound.stageRun, undefined, 'no StageRun identity is invented')
})

test('a fail-closed Attention carries no fabricated Delivery or StageRun identity', () => {
  const parameters = parametersOf(attentionCenterItemHash(centerItem({
    kind: 'attention',
    bindingValid: false,
    urgency: 'binding-invalid',
    deliveryId: null,
    deliveryTitle: null,
    stageRunId: null,
  }), scopeSelection))
  assert.equal(parameters.delivery, undefined)
  assert.equal(parameters.stageRun, undefined)
})

test('decision links carry the exact StageRun origin and survive a missing origin honestly', () => {
  const origins = [origin()]
  assert.equal(
    attentionCenterItemHash(centerItem(), scopeSelection, origins),
    `#/attention?session=${productSessionId}&delivery=${deliveryId}&stageRun=${stageRunId}`
      + `&organizationId=${scope.organizationId}&workspaceId=${scope.workspaceId}`
      + `&projectId=${scope.projectId}&repositoryId=${scope.repositoryId}`,
  )
  const unmapped = centerItem({ stageRunId: otherStageRunId })
  assert.equal(
    attentionCenterItemHash(unmapped, scopeSelection, origins),
    `#/attention?session=${productSessionId}`
      + `&organizationId=${scope.organizationId}&workspaceId=${scope.workspaceId}`
      + `&projectId=${scope.projectId}&repositoryId=${scope.repositoryId}`,
    'a StageRun with no loaded Delivery origin links the decision surface only',
  )
})

test('the execution-origin link stays the exact typed run-page route', () => {
  assert.equal(
    attentionCenterOriginHash(origin(), stageRunId, scopeSelection),
    `#/strongflow?delivery=${deliveryId}&stageRun=${stageRunId}&view=unified`
      + `&organizationId=${scope.organizationId}&workspaceId=${scope.workspaceId}`
      + `&projectId=${scope.projectId}&repositoryId=${scope.repositoryId}`,
  )
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

test('the mounted center links every actionable card to its run-page context', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const state = {
    status: 'ready',
    realtime: 'subscribed',
    items: [
      // A decision bound to the StageRun that is active in the one origin.
      centerItem(),
      // A business Attention bound to the same Delivery and StageRun.
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
    origins: [origin()],
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

  // UI-100.3 wiring: the card action passes the loaded origins, so the decision
  // link returns to the Task/StageRun that raised it.
  const decisionParameters = parametersOf(cardAction(decisionCard).href)
  assert.equal(decisionParameters.session, productSessionId)
  assert.equal(decisionParameters.delivery, deliveryId)
  assert.equal(decisionParameters.stageRun, stageRunId)

  const attentionParameters = parametersOf(cardAction(attentionCard).href)
  assert.match(cardAction(attentionCard).href, /^#\/strongflow\?/u)
  assert.equal(attentionParameters.delivery, deliveryId)
  assert.equal(attentionParameters.stageRun, stageRunId)
  assert.equal(attentionParameters.view, 'unified')

  assert.equal(cardAction(expiredCard).getAttribute('href'), null)
  assert.equal(cardAction(expiredCard).getAttribute('aria-disabled'), 'true')

  // The execution-context entry stays the typed run-page route.
  const originLink = allByClass(decisionCard, 'wwc-attention-card-origin')[0]
  assert.notEqual(originLink, undefined)
  assert.equal(
    originLink.href,
    attentionCenterOriginHash(origin(), stageRunId, scopeSelection),
  )
  mounted.close()
  assert.equal(model.closeCalls, 1)
})

test('a live origins update rewrites the decision links without recreating the cards', () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const state = {
    status: 'ready',
    realtime: 'subscribed',
    items: [centerItem()],
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
  const card = mountedCards(rootElement)[0]
  assert.equal(parametersOf(cardAction(card).href).stageRun, undefined)

  model.publish({
    ...state,
    origins: [origin({ deliveryId: otherDeliveryId })],
  })
  assert.equal(mountedCards(rootElement)[0], card, 'the update keeps the node identity')
  const parameters = parametersOf(cardAction(card).href)
  assert.equal(parameters.delivery, otherDeliveryId)
  assert.equal(parameters.stageRun, stageRunId)
  mounted.close()
})
