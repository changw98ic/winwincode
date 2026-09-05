import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.contextual-decision-tests.json',
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
  `The contextual decision card did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cardModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/contextual-decision-tests/contextual-decision.js',
)).href}`)
const viewModel = await import(`${pathToFileURL(resolve(
  root,
  '.cache/contextual-decision-tests/contextual-decision-view-model.js',
)).href}`)

const { mountContextualDecisionCard } = cardModule
const {
  contextualDecisionPresentation,
  contextualDecisions,
} = viewModel

const productSessionId = 'psn_00000000000000000000000001'
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const now = Date.parse('2026-09-04T12:00:00.000Z')

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
  href = ''
  #textContent = ''

  get textContent() {
    return this.#textContent + this.children.map(child => child.textContent).join('')
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

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

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    this.listeners.set(name, listeners.filter(candidate => candidate !== listener))
  }

  emit(name, values = {}) {
    const event = { target: this, ...values }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
  }

  click() { this.emit('click') }
}

class FakeDocument {
  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

function findByClass(node, className) {
  if (node.className === className) return node
  for (const child of node.children) {
    const match = findByClass(child, className)
    if (match !== null) return match
  }
  return null
}

function findAllByClass(node, className, matches = []) {
  if (node.className === className) matches.push(node)
  for (const child of node.children) findAllByClass(child, className, matches)
  return matches
}

function binding(overrides = {}) {
  return {
    productSessionId,
    executionJobId: 'job_00000000000000000000000001',
    workerSessionId: 'wsn_00000000000000000000000001',
    sessionIdentity: {
      productSessionId,
      workerSessionId: 'wsn_00000000000000000000000001',
      codexThreadId: 'cdx_00000000000000000000000001',
      stageRunId,
    },
    ...overrides,
  }
}

function input(overrides = {}) {
  return {
    kind: 'input',
    inputRequestId: 'inp_00000000000000000000000001',
    revision: 3,
    state: 'pending',
    prompt: 'Select the next StageRun step.',
    binding: binding(),
    mode: 'text',
    options: [],
    allowEmpty: false,
    expiresAt: '2026-09-04T12:30:00.000Z',
    ...overrides,
  }
}

function approval(overrides = {}) {
  return {
    id: 'apr_00000000000000000000000001',
    requestedAt: '2026-09-04T11:00:00.000Z',
    expiresAt: '2026-09-04T12:10:00.000Z',
    revision: 5,
    state: 'pending',
    subject: 'Run the approved test command.',
    binding: binding(),
    ...overrides,
  }
}

function attention(overrides = {}) {
  return {
    projection: {
      id: 'atn_00000000000000000000000001',
      title: 'Verification blocked on the delivery criterion.',
      status: 'open',
      blocking: true,
      createdAt: '2026-09-04T11:30:00.000Z',
      stageRunId,
      type: 'verification_blocked',
      options: [],
      assignedTo: null,
      deliverySpecId: 'spec:1',
      resolutionSummary: null,
      resolvedAt: null,
      resolvedBy: null,
    },
    deliveryId,
    deliveryRevision: 9,
    ...overrides,
  }
}

function view(overrides = {}) {
  return contextualDecisions({
    inputs: [],
    approvals: [],
    attention: [],
    nowMillis: now,
    ...overrides,
  })
}

const readyPresentation = contextualDecisionPresentation(view())

function mount(update, overrides = {}) {
  const document = new FakeDocument()
  const root = document.createElement('div')
  const actions = []
  const card = mountContextualDecisionCard({
    root,
    id: 'wwc-test-decisions',
    actions: {
      provideInput: (item, value) => { actions.push(['provideInput', item.id, value]) },
      cancelInput: item => { actions.push(['cancelInput', item.id]) },
      decideApproval: (item, decision, reason) => {
        actions.push(['decideApproval', item.id, decision, reason])
      },
      resolveAttention: (item, decision, resolution) => {
        actions.push(['resolveAttention', item.id, decision, resolution])
      },
    },
    ...overrides,
  })
  card.update({
    view: view(),
    presentation: readyPresentation,
    note: null,
    ...update,
  })
  return { card, root, actions }
}

function rows(root) {
  return findAllByClass(root, 'wwc-contextual-decision-item')
}

test('an empty context hides the card instead of rendering an empty block', () => {
  const { root } = mount({ view: view() })
  assert.equal(findByClass(root, 'wwc-contextual-decision').hidden, true)
})

test('one row per decision carries the kind label and the bound producer text', () => {
  const { root } = mount({
    view: view({ approvals: [approval()], inputs: [input()] }),
  })
  const items = rows(root)
  assert.equal(items.length, 2)
  assert.equal(items[0].dataset.kind, 'approval')
  assert.equal(items[0].dataset.urgency, 'blocking')
  assert.equal(
    findByClass(items[0], 'wwc-contextual-decision-title').textContent,
    'Run the approved test command.',
  )
  const context = findByClass(items[0], 'wwc-contextual-decision-context')
  assert.match(context.textContent, /Tool approval/u)
  assert.match(context.textContent, /ProductSession and StageRun-bound/u)
  assert.match(context.textContent, /Expires 2026-09-04T12:10:00\.000Z/u)
  assert.equal(items[1].dataset.kind, 'input')
})

test('an approval is decided inline and keeps its reason until the server confirms', () => {
  const { root, actions } = mount({ view: view({ approvals: [approval()] }) })
  const row = rows(root)[0]
  const reason = findByClass(row, 'wwc-contextual-decision-response')
  reason.value = 'Reviewed the exact command.'
  findByClass(row, 'wwc-contextual-decision-submit').click()
  assert.deepEqual(actions, [['decideApproval', 'apr_00000000000000000000000001', 'approve', 'Reviewed the exact command.']])

  // The draft survives the submission: the row clears only when the server
  // stops listing this decision.
  assert.equal(reason.value, 'Reviewed the exact command.')
  assert.equal(findByClass(row, 'wwc-contextual-decision-secondary').textContent, 'Reject')

  findByClass(row, 'wwc-contextual-decision-secondary').click()
  assert.deepEqual(actions.at(-1), ['decideApproval', 'apr_00000000000000000000000001', 'reject', 'Reviewed the exact command.'])
})

test('an approval without its required reason is refused and keeps the card open', () => {
  const { root, actions } = mount({ view: view({ approvals: [approval()] }) })
  const row = rows(root)[0]
  findByClass(row, 'wwc-contextual-decision-submit').click()
  assert.deepEqual(actions, [])
  assert.equal(findByClass(row, 'wwc-contextual-decision-rejected').hidden, false)
})

test('a text input submits the exact interactive value the input asked for', () => {
  const { root, actions } = mount({ view: view({ inputs: [input()] }) })
  const row = rows(root)[0]
  const response = findByClass(row, 'wwc-contextual-decision-response')
  response.value = '  candidate workspace  '
  findByClass(row, 'wwc-contextual-decision-submit').click()
  assert.deepEqual(actions, [[
    'provideInput',
    'inp_00000000000000000000000001',
    { mode: 'text', value: 'candidate workspace' },
  ]])
})

test('a choice input renders its options and submits the chosen canonical value', () => {
  const { root, actions } = mount({
    view: view({
      inputs: [input({
        mode: 'single_choice',
        options: [
          { id: 'ich_00000000000000000000000001', value: 'candidate', label: 'Candidate workspace' },
        ],
      })],
    }),
  })
  const row = rows(root)[0]
  const option = findByClass(row, 'wwc-contextual-decision-option')
  assert.equal(option.textContent, 'Candidate workspace')
  option.click()
  assert.deepEqual(actions, [[
    'provideInput',
    'inp_00000000000000000000000001',
    { mode: 'single_choice', value: 'candidate' },
  ]])
})

test('a text input offers cancel instead of a destructive second decision', () => {
  const { root, actions } = mount({ view: view({ inputs: [input()] }) })
  const row = rows(root)[0]
  assert.equal(findByClass(row, 'wwc-contextual-decision-secondary').textContent, 'Cancel input')
  findByClass(row, 'wwc-contextual-decision-secondary').click()
  assert.deepEqual(actions, [['cancelInput', 'inp_00000000000000000000000001']])
})

test('an expired decision submits nothing and keeps the typed input', () => {
  const { root, actions } = mount({
    view: view({
      approvals: [approval({ expiresAt: '2026-09-04T06:00:00.000Z' })],
    }),
  })
  const row = rows(root)[0]
  const reason = findByClass(row, 'wwc-contextual-decision-response')
  reason.value = 'Approved before the deadline passed.'
  assert.equal(findByClass(row, 'wwc-contextual-decision-submit').disabled, true)
  findByClass(row, 'wwc-contextual-decision-submit').click()
  assert.deepEqual(actions, [])
  assert.equal(reason.value, 'Approved before the deadline passed.')
  assert.equal(findByClass(row, 'wwc-contextual-decision-rejected').textContent,
    'This decision is no longer current. Refresh for the current state.')
})

test('a busy page disables every row control while it replaces its snapshot', () => {
  const { root, actions } = mount({
    view: view({ approvals: [approval()] }),
    presentation: contextualDecisionPresentation(view({ approvals: [approval()] }), {
      busy: true,
    }),
  })
  const row = rows(root)[0]
  assert.equal(findByClass(row, 'wwc-contextual-decision-submit').disabled, true)
  findByClass(row, 'wwc-contextual-decision-submit').click()
  assert.deepEqual(actions, [])
})

test('a read-only card renders the decisions without any control', () => {
  const { root, actions } = mount({
    view: view({ approvals: [approval()] }),
    presentation: contextualDecisionPresentation(view({ approvals: [approval()] }), {
      readOnly: true,
    }),
  }, { readOnly: true })
  const row = rows(root)[0]
  assert.equal(findByClass(row, 'wwc-contextual-decision-submit').disabled, true)
  findByClass(row, 'wwc-contextual-decision-submit').click()
  assert.deepEqual(actions, [])
})

test('a row that this page does not decide links to the owning decision surface', () => {
  const { root, actions } = mount({
    view: view({ attention: [attention()] }),
  }, {
    actions: {},
    detailHref: item => `#/attention?session=${productSessionId}`
      + `&delivery=${item.deliveryId}&stageRun=${item.stageRunId}`,
  })
  const row = rows(root)[0]
  assert.equal(findByClass(row, 'wwc-contextual-decision-submit').hidden, true)
  assert.equal(findByClass(row, 'wwc-contextual-decision-response-label').hidden, true)
  assert.equal(findByClass(row, 'wwc-contextual-decision-options').hidden, true)
  const link = findByClass(row, 'wwc-contextual-decision-detail')
  assert.equal(link.hidden, false)
  assert.match(link.href, new RegExp(`^#/attention\\?session=${productSessionId}`, 'u'))
  assert.match(link.href, /delivery=dlv_00000000000000000000000001/u)
  assert.match(link.href, /stageRun=run_00000000000000000000000001/u)
  assert.deepEqual(actions, [])
})

test('an expired row loses its link even when an owning surface exists', () => {
  const { root } = mount({
    view: view({
      approvals: [approval({ expiresAt: '2026-09-04T06:00:00.000Z' })],
    }),
  }, {
    actions: {},
    detailHref: () => '#/attention',
  })
  const link = findByClass(rows(root)[0], 'wwc-contextual-decision-detail')
  assert.equal(link.hidden, true)
  assert.equal(link.getAttribute('href'), null)
})

test('an omitted decision is reported instead of silently dropped', () => {
  const { root } = mount({
    view: view({
      approvals: Array.from({ length: 6 }, (_, index) => approval({
        id: `apr_${String(index + 1).padStart(26, '0')}`,
        expiresAt: `2026-09-04T13:0${String(index)}:00.000Z`,
      })),
    }),
  })
  assert.equal(rows(root).length, 4)
  assert.match(
    findByClass(root, 'wwc-contextual-decision-omitted').textContent,
    /2 more decisions not shown/u,
  )
})

test('the card keeps the host note and falls back to its own count', () => {
  const document = new FakeDocument()
  const root = document.createElement('div')
  const card = mountContextualDecisionCard({ root, id: 'wwc-test-decisions' })
  card.update({
    view: view({ approvals: [approval()] }),
    presentation: readyPresentation,
    note: 'The Solution review is decided in the Delivery actions.',
  })
  const note = findByClass(root, 'wwc-contextual-decision-note')
  assert.equal(note.hidden, false)
  assert.equal(note.textContent, 'The Solution review is decided in the Delivery actions.')
  // Without a host note the card reports its own decision count.
  card.update({
    view: view({ approvals: [approval()] }),
    presentation: readyPresentation,
    note: null,
  })
  assert.equal(note.hidden, false)
  assert.equal(note.textContent, readyPresentation.statusText)
  card.close()
})

test('closing the card removes its rows and listeners from the host', () => {
  const { card, root, actions } = mount({ view: view({ approvals: [approval()] }) })
  const row = rows(root)[0]
  card.close()
  assert.equal(findByClass(root, 'wwc-contextual-decision-item'), null)
  assert.equal(row.parentNode, null)
  findByClass(root, 'wwc-contextual-decision-submit')?.click()
  assert.deepEqual(actions, [])
})

test('the card suite is registered once and its modules are inventoried', () => {
  const runner = readFileSync(resolve(root, 'scripts/run-ts-tests.mjs'), 'utf8')
  for (const path of [
    'tests/contextual-decision.test.mjs',
    'tests/contextual-decision-view-model.test.mjs',
  ]) {
    assert.equal(
      runner.split(`'${path}'`).length - 1,
      1,
      `${path} must be registered exactly once in the canonical TypeScript lane`,
    )
  }
  const inventory = JSON.parse(readFileSync(
    resolve(root, 'docs/decisions/0028-control-plane-worker-migration.inventory.json'),
    'utf8',
  ))
  const listed = inventory.surfaces
    .flatMap(surface => surface.sourcePaths)
  for (const path of [
    'apps/client/src/contextual-decision.ts',
    'apps/client/src/contextual-decision-view-model.ts',
  ]) {
    assert.equal(listed.filter(entry => entry === path).length, 1, `${path} is inventoried once`)
  }
})
