// UI-307 review red light for winwincode-plc: a contract-valid review diagram
// (120 nodes / 150 edges, canonical bound is 200/200) must render at the
// default limits without throwing. Current source throws inside the page
// render because boundedItems truncates nodes (100) and edges (200)
// independently, leaving edges that join nodes the graph never received.
// Review-lane file: expected to turn green with winwincode-plc and be folded
// into tests/strongflow-diagram-graph.test.mjs by the fixing lane.
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
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
    'apps/client/tsconfig.strongflow-page-tests.json',
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
  `StrongFlow page did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const diagramsModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-diagrams.js',
)).href}`)
const { mountStrongFlowDiagrams } = diagramsModule

const stageRunId = 'run_00000000000000000000000001'

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
  hidden = false
  type = ''
  #text = ''

  get textContent() {
    return this.#text
  }

  set textContent(value) {
    this.#text = String(value)
    this.replaceChildren()
  }

  get childNodes() {
    return this.children
  }

  contains(candidate) {
    if (candidate === this) return true
    return this.children.some(child => child.contains(candidate))
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
    const listeners = (this.listeners.get(name) ?? []).filter(candidate => candidate !== listener)
    if (listeners.length === 0) this.listeners.delete(name)
    else this.listeners.set(name, listeners)
  }

  emit(name, values = {}) {
    for (const listener of this.listeners.get(name) ?? []) listener(values)
  }

  focus() {
    this.ownerDocument.activeElement = this
  }
}

class FakeDocument {
  activeElement = null
  elements = []

  createElement(tagName) {
    const element = new FakeElement(this, tagName)
    this.elements.push(element)
    return element
  }
}

function largeDiagram(kind) {
  return {
    id: `diagram:${kind}`,
    kind,
    title: `${kind} diagram`,
    nodes: Array.from({ length: 120 }, (_, index) => ({
      id: `node:${String(index + 1)}`,
      label: `Node ${String(index + 1)}`,
      description: `Description ${String(index + 1)}`,
      kind: 'component',
      trustBoundary: null,
      unresolved: false,
    })),
    edges: Array.from({ length: 150 }, (_, index) => ({
      id: `edge:${String(index + 1)}`,
      from: 'node:1',
      to: `node:${String(index + 1)}`,
      label: `Edge ${String(index + 1)}`,
    })),
  }
}

function largeProjection() {
  return {
    delivery: {
      schemaVersion: 'winwincode/v1',
      deliveryId: 'dlv_00000000000000000000000001',
      deliveryRevision: 4,
      status: 'executing',
      ownership: {
        organizationId: 'org_00000000000000000000000001',
        workspaceId: 'wsp_00000000000000000000000001',
        projectId: 'prj_00000000000000000000000001',
        repositoryId: 'rep_00000000000000000000000001',
      },
      requirements: { title: 'Large valid diagram', goal: 'Contract-bounded diagram.' },
      tasks: [],
      stages: [],
      attention: [],
    },
    solutionReview: {
      deliveryId: 'dlv_00000000000000000000000001',
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      reviewSetSha256: `sha256:${'a'.repeat(64)}`,
      attentionItemId: 'attention:1',
      reviewStatus: 'pending',
      architectureDiagram: largeDiagram('system-architecture'),
      processDiagram: largeDiagram('process-flow'),
    },
    stage: { id: stageRunId },
    runtime: { stageRunId, sessions: [] },
    evidence: [],
    verdict: null,
    attention: [],
    currentCandidate: null,
    diagramExecution: null,
    publication: null,
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-09-02T08:00:00.000Z',
      revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 1 },
      readCursor: {},
    },
  }
}

const DEFAULT_LIMITS = {
  deliveries: 50,
  tasks: 100,
  stages: 50,
  attention: 50,
  evidence: 100,
  runtimeSessions: 50,
  graphNodes: 100,
  graphEdges: 200,
  activities: 100,
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

test('winwincode-plc: a contract-valid 120-node 150-edge review renders within default limits', () => {
  const document = new FakeDocument()
  const view = mountStrongFlowDiagrams({ document, limits: DEFAULT_LIMITS })
  const projection = largeProjection()
  view.update({ projection, narrow: false })

  const architecture = findAllByClass(view.root, 'wwc-strongflow-graph')[0]
  const renderedNodes = findAllByClass(architecture, 'wwc-strongflow-graph-node')
    .map(node => node.dataset.id)
  assert.equal(
    renderedNodes.length,
    DEFAULT_LIMITS.graphNodes,
    'bounded render keeps exactly the graph node limit',
  )
  const renderedNodeIds = new Set(renderedNodes)
  for (const edge of findAllByClass(architecture, 'wwc-strongflow-graph-edge')) {
    const from = edge.getAttribute('data-from')
    const to = edge.getAttribute('data-to')
    assert.equal(
      renderedNodeIds.has(from) && renderedNodeIds.has(to),
      true,
      `rendered edge ${edge.dataset.id} must join rendered nodes, got ${String(from)} → ${String(to)}`,
    )
  }
  assert.equal(
    findByClass(architecture, 'wwc-strongflow-omitted') === null
      || findAllByClass(architecture, 'wwc-strongflow-omitted')
        .some(note => /connections/u.test(note.textContent)),
    true,
    'dropped connections must surface through the omitted count',
  )
  view.close()
})
