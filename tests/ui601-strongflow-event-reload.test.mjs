// SPDX-License-Identifier: Apache-2.0

// Reproduce a realtime invalidation reload: a refreshing notification followed
// by a newly decoded snapshot object with the same rendered artifact content.

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const outDir = mkdtempSync(join(tmpdir(), 'ui601-strongflow-event-'))
test.after(() => { rmSync(outDir, { recursive: true, force: true }) })

const compiler = spawnSync('corepack', [
  'pnpm',
  'exec',
  'tsc',
  '-p',
  'apps/client/tsconfig.strongflow-page-tests.json',
  '--pretty',
  'false',
  '--incremental',
  'false',
  `--outDir`,
  outDir,
], { cwd: root, encoding: 'utf8' })
assert.equal(
  compiler.status,
  0,
  `Candidate StrongFlow page did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const page = await import(`${pathToFileURL(join(outDir, 'strongflow-page.js')).href}`)

const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'

function many(count, create) {
  return Array.from({ length: count }, (_, index) => create(index + 1))
}

function diagram(kind) {
  return {
    id: `diagram:${kind}`,
    kind,
    title: `${kind} diagram`,
    nodes: many(5, value => ({
      id: `node:${String(value)}`,
      label: `Node ${String(value)}`,
      description: `Description ${String(value)}`,
      kind: 'component',
      trustBoundary: null,
      unresolved: false,
    })),
    edges: many(5, value => ({
      id: `edge:${String(value)}`,
      from: 'node:1',
      to: `node:${String(value)}`,
      label: `Edge ${String(value)}`,
    })),
  }
}

function projection() {
  const candidateRef = 'refs/winwincode/candidate/1'
  return {
    delivery: {
      schemaVersion: 'winwincode/v1',
      deliveryId,
      deliveryRevision: 4,
      status: 'executing',
      ownership: {
        organizationId: 'org_00000000000000000000000001',
        workspaceId: 'wsp_00000000000000000000000001',
        projectId: 'prj_00000000000000000000000001',
        repositoryId: 'rep_00000000000000000000000001',
      },
      requirements: {
        title: 'Bounded StrongFlow workspace',
        goal: 'Render without unbounded DOM growth.',
      },
      tasks: many(5, value => ({
        id: `task:${String(value)}`,
        title: `Task ${String(value)}`,
        status: value === 1 ? 'active' : 'pending',
      })),
      stages: many(5, value => ({
        id: value === 1 ? stageRunId : `run_${String(value).padStart(26, '0')}`,
        stage: value === 1 ? 'executing' : 'verifying',
        role: 'implementer',
        status: value === 1 ? 'running' : 'waiting',
      })),
      attention: many(5, value => ({
        id: `attention:${String(value)}`,
        title: `Attention ${String(value)}`,
        status: value === 1 ? 'open' : 'resolved',
      })),
    },
    solutionReview: {
      reviewStatus: 'pending',
      architectureDiagram: diagram('system-architecture'),
      processDiagram: diagram('process-flow'),
    },
    stage: { id: stageRunId },
    runtime: {
      stageRunId,
      sessions: many(3, sessionValue => ({
        deliveryTaskId: `task:${String(sessionValue)}`,
        attempt: sessionValue,
        asOfSequence: sessionValue,
        agents: many(5, agentValue => ({
          threadId: `cdx_${String(agentValue).padStart(26, '0')}`,
          nickname: `Agent ${String(agentValue)}`,
          role: 'worker',
          status: agentValue === 1 ? 'running' : 'waiting',
        })),
        agentEdges: many(5, edgeValue => ({
          parentThreadId: 'cdx_00000000000000000000000001',
          childThreadId: `cdx_${String(edgeValue).padStart(26, '0')}`,
        })),
        activities: many(5, activityValue => ({
          activityType: 'test',
          status: activityValue === 1 ? 'running' : 'completed',
          outcome: activityValue === 1 ? 'observed' : 'succeeded',
        })),
        diffSummary: {
          changedFileCount: 3,
          additions: 20,
          deletions: 5,
          sourceRef: 'runtime:diff:1',
        },
      })),
    },
    evidence: many(5, value => ({
      id: `evidence:${String(value)}`,
      type: 'test',
      sourceRef: `artifact:test:${String(value)}`,
      candidateRef,
    })),
    verdict: null,
    attention: many(5, value => ({
      id: `attention:${String(value)}`,
      title: `Attention ${String(value)}`,
      status: value === 1 ? 'open' : 'resolved',
    })),
    currentCandidate: {
      candidateRef,
      candidateCommitId: '1111111111111111111111111111111111111111',
      candidateTreeId: '2222222222222222222222222222222222222222',
      diffSha256: `sha256:${'3'.repeat(64)}`,
      frozenAt: '2026-08-27T01:00:04.000Z',
    },
    publication: null,
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-08-27T01:00:06.000Z',
      revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 0 },
      readCursor: {},
    },
  }
}

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
  #textContent = ''

  get textContent() { return this.#textContent }
  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

  append(...children) { for (const child of children) this.insertBefore(child, null) }

  replaceChildren(...children) {
    for (const child of [...this.children]) child.remove()
    for (const child of children) this.insertBefore(child, null)
  }

  insertBefore(child, reference) {
    child.remove?.()
    const index = reference === null
      ? this.children.length
      : this.children.indexOf(reference)
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

  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }

  addEventListener(name, listener) {
    const existing = this.listeners.get(name) ?? []
    existing.push(listener)
    this.listeners.set(name, existing)
  }

  removeEventListener(name, listener) {
    const existing = this.listeners.get(name) ?? []
    this.listeners.set(name, existing.filter(candidate => candidate !== listener))
  }
}

class FakeDocument {
  createElement(tagName) { return new FakeElement(this, tagName) }
}

function findByClass(node, className) {
  if (node.className === className) return node
  for (const child of node.children) {
    const match = findByClass(child, className)
    if (match !== null) return match
  }
  return null
}

class FakeStrongFlowViewModel {
  constructor(initialState) { this.state = initialState }

  draftScope = '["ui601-test-actor","ui601-test-scope"]'
  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) { this.state = next; this.listener?.(next) }

  async start() {}
  async refresh() {}
  async loadCandidateFiles() {}
  async loadMoreCandidateFiles() {}
  async selectCandidateFile() {}
  async loadMoreCandidateDiff() {}
  async decideSolutionReview() {}
  async approveTaskBreakdown() {}
  async resolveAttention() {}
  async submitVerdict() {}
  async advanceDelivery() {}
  cancelPending() {}
  reconnect() {}
  close() {}
}

const limits = {
  deliveries: 2, tasks: 2, stages: 2, attention: 2, evidence: 2,
  runtimeSessions: 2, graphNodes: 2, graphEdges: 2, activities: 2,
}

function state(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    projection: projection(),
    candidateFiles: {
      status: 'idle',
      items: [],
      hasMore: false,
      previewLimited: false,
      selectedPath: null,
      diff: {
        status: 'idle',
        path: null,
        content: '',
        loadedBytes: 0,
        totalBytes: null,
        hasMore: false,
        previewLimited: false,
        fileDiffSha256: null,
        unavailableReason: null,
        error: null,
      },
      error: null,
    },
    interaction: { status: 'idle', error: null },
    error: null,
    ...overrides,
  }
}

test('UI-601: realtime event reload keeps the solution review draft and Diff Viewer identity', () => {
  const document = new FakeDocument()
  const root = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = page.mountStrongFlowPage({ root, model, limits })
  const actions = findByClass(root, 'wwc-strongflow-solution-actions')
  const comments = actions.children[0].children[0]
  const changes = actions.children[1].children[0]
  const candidateBefore = findByClass(root, 'wwc-strongflow-view-candidate')
  const diagramsBefore = findByClass(root, 'wwc-strongflow-diagrams')
  assert.notEqual(candidateBefore, null)
  assert.notEqual(diagramsBefore, null)
  comments.value = 'about to approve'
  changes.value = 'please tighten edge cases'

  // One DeliveryTaskChangedV1 event: retained interim, then a fresh snapshot.
  model.publish({
    status: 'refreshing',
    realtime: 'reloading',
    projection: model.state.projection,
    candidateFiles: model.state.candidateFiles,
    interaction: { status: 'idle', error: null },
    error: null,
  })
  const activeDelivery = findByClass(root, 'wwc-strongflow-delivery-list').children[0].children[0]
  assert.equal(activeDelivery.getAttribute('aria-current'), 'page')
  model.publish(state())

  assert.equal(
    comments.value,
    'about to approve',
    'UI-601 acceptance: an unrelated task event must not clear the review draft',
  )
  assert.equal(
    changes.value,
    'please tighten edge cases',
    'UI-601 acceptance: an unrelated task event must not clear the requested-changes draft',
  )
  assert.equal(
    findByClass(root, 'wwc-strongflow-view-candidate'),
    candidateBefore,
    'UI-601 acceptance: an unrelated task event must not rebuild the Diff Viewer',
  )
  assert.equal(
    findByClass(root, 'wwc-strongflow-diagrams'),
    diagramsBefore,
    'UI-601 acceptance: an unrelated task event must not rebuild the diagrams',
  )
  mounted.close()
})

test('UI-601: repeated realtime events keep DOM node identity bounded to one rebuild per change', () => {
  const document = new FakeDocument()
  const root = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = page.mountStrongFlowPage({ root, model, limits })
  const distinctCandidates = new Set()
  const distinctDiagrams = new Set()
  for (let index = 0; index < 20; index += 1) {
    model.publish({
      status: 'refreshing',
      realtime: 'reloading',
      projection: model.state.projection,
      candidateFiles: model.state.candidateFiles,
      interaction: { status: 'idle', error: null },
      error: null,
    })
    model.publish(state())
    distinctCandidates.add(findByClass(root, 'wwc-strongflow-view-candidate'))
    distinctDiagrams.add(findByClass(root, 'wwc-strongflow-diagrams'))
  }
  assert.equal(
    distinctCandidates.size,
    1,
    `unrelated reloads rebuilt the Diff Viewer ${distinctCandidates.size} times`,
  )
  assert.equal(
    distinctDiagrams.size,
    1,
    `unrelated reloads rebuilt the diagrams ${distinctDiagrams.size} times`,
  )
  mounted.close()
})

test('UI-601: mounted artifact views retain identity while their content changes', () => {
  const document = new FakeDocument()
  const root = document.createElement('main')
  const model = new FakeStrongFlowViewModel(state())
  const mounted = page.mountStrongFlowPage({ root, model, limits })
  const actions = findByClass(root, 'wwc-strongflow-solution-actions')
  const comments = actions.children[0].children[0]
  const candidateBefore = findByClass(root, 'wwc-strongflow-view-candidate')
  const diagramsBefore = findByClass(root, 'wwc-strongflow-diagrams')
  comments.value = 'draft for the current candidate'

  const taskOnly = projection()
  taskOnly.delivery.tasks[0].title = 'Updated task title'
  model.publish(state({ projection: taskOnly }))
  assert.equal(findByClass(root, 'wwc-strongflow-view-candidate'), candidateBefore)
  assert.equal(findByClass(root, 'wwc-strongflow-diagrams'), diagramsBefore)
  assert.equal(comments.value, 'draft for the current candidate')

  const runtimeChanged = projection()
  runtimeChanged.runtime.sessions[0].diffSummary.sourceRef = 'runtime:diff:2'
  model.publish(state({ projection: runtimeChanged }))
  const diagramsAfterRuntime = findByClass(root, 'wwc-strongflow-diagrams')
  assert.notEqual(diagramsAfterRuntime, diagramsBefore)
  assert.equal(findByClass(root, 'wwc-strongflow-view-candidate'), candidateBefore)
  assert.equal(comments.value, 'draft for the current candidate')

  const candidateChanged = projection()
  candidateChanged.runtime.sessions[0].diffSummary.sourceRef = 'runtime:diff:2'
  candidateChanged.currentCandidate.diffSha256 = `sha256:${'4'.repeat(64)}`
  model.publish(state({ projection: candidateChanged }))
  assert.equal(findByClass(root, 'wwc-strongflow-diagrams'), diagramsAfterRuntime)
  assert.equal(findByClass(root, 'wwc-strongflow-view-candidate'), candidateBefore)
  assert.equal(comments.value, '')
  mounted.close()
})
