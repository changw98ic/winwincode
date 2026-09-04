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
    'apps/client/tsconfig.strongflow-review-annotations-tests.json',
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
  `StrongFlow review annotations did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const annotationsModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-review-annotations-tests/strongflow-review-annotations.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-review-annotations-tests/strongflow-page.js',
)).href}`)

const {
  StrongFlowReviewDraftError,
  createStrongFlowReviewAnnotations,
  strongFlowReviewAnchorLabel,
  mountStrongFlowReviewPanel,
} = annotationsModule
const { mountStrongFlowPage } = pageModule

const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const candidateDigest = `sha256:${'3'.repeat(64)}`
const nextCandidateDigest = `sha256:${'4'.repeat(64)}`
const candidateRef = 'refs/winwincode/candidate/1'
const nextCandidateRef = 'refs/winwincode/candidate/2'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}

function many(count, create) {
  return Array.from({ length: count }, (_, index) => create(index + 1))
}

function diagram(kind) {
  return {
    id: `diagram:${kind}`,
    kind,
    title: `${kind} diagram`,
    nodes: many(3, value => ({
      id: `node:${String(value)}`,
      label: `Node ${String(value)}`,
      description: `Description ${String(value)}`,
      kind: 'component',
      trustBoundary: null,
      unresolved: false,
    })),
    edges: [{
      id: 'edge:1',
      from: 'node:1',
      to: 'node:2',
      label: 'Edge 1',
    }],
  }
}

/** A current, reviewable StrongFlow projection with every anchor target present. */
function projection(overrides = {}) {
  return {
    delivery: {
      schemaVersion: 'winwincode/v1',
      deliveryId,
      deliveryRevision: 4,
      status: 'executing',
      ownership: scope,
      requirements: {
        title: 'Bounded StrongFlow review',
        goal: 'Compose local review notes into one legal command.',
      },
      tasks: [
        { id: 'task:1', title: 'Task 1', status: 'active' },
        { id: 'task:2', title: 'Task 2', status: 'pending' },
      ],
      stages: [{ id: stageRunId, stage: 'reviewing', role: 'reviewer', status: 'waiting' }],
      attention: [
        { id: 'attention:1', title: 'Attention 1', status: 'open' },
        { id: 'attention:2', title: 'Attention 2', status: 'resolved' },
      ],
    },
    solutionReview: {
      reviewStatus: 'pending',
      attentionItemId: 'attention:1',
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      reviewStageRunId: stageRunId,
      reviewSetSha256: `sha256:${'7'.repeat(64)}`,
      architectureDiagram: diagram('system-architecture'),
      processDiagram: diagram('process-flow'),
    },
    diagramExecution: null,
    stage: { id: stageRunId },
    runtime: { stageRunId, sessions: [] },
    evidence: [],
    verdict: {
      id: 'verdict:1',
      status: 'pass',
      producedAt: '2026-09-02T01:00:05.000Z',
      criteria: [
        {
          criterionId: 'criterion:1',
          evaluatedAt: '2026-09-02T01:00:05.000Z',
          evidenceRefs: [],
          explanation: 'The exact check passed.',
          resultId: 'result:1',
          verdict: 'pass',
        },
        {
          criterionId: 'criterion:2',
          evaluatedAt: '2026-09-02T01:00:05.000Z',
          evidenceRefs: [],
          explanation: 'The other exact check passed.',
          resultId: 'result:2',
          verdict: 'pass',
        },
      ],
    },
    attention: [
      { id: 'attention:1', title: 'Attention 1', status: 'open' },
      { id: 'attention:2', title: 'Attention 2', status: 'resolved' },
    ],
    currentCandidate: {
      candidateRef,
      candidateCommitId: '1'.repeat(40),
      candidateTreeId: '2'.repeat(40),
      diffSha256: candidateDigest,
      frozenAt: '2026-09-02T01:00:04.000Z',
    },
    publication: { state: 'pending', revision: 1, updatedAt: '2026-09-02T01:00:06.000Z' },
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-09-02T01:00:06.000Z',
      revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 1 },
      readCursor: {},
    },
    ...overrides,
  }
}

function reviewState(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    projection: projection(),
    candidateFiles: {
      status: 'ready',
      items: [
        {
          path: 'src/current.ts',
          oldPath: null,
          status: 'modified',
          additions: 4,
          deletions: 1,
          binary: false,
          encoding: 'utf-8',
        },
        {
          path: 'src/other.ts',
          oldPath: null,
          status: 'added',
          additions: 9,
          deletions: null,
          binary: false,
          encoding: 'utf-8',
        },
      ],
      hasMore: false,
      previewLimited: false,
      selectedPath: 'src/current.ts',
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

function draftWith(ids = ['a', 'b', 'c', 'd', 'e', 'f']) {
  const queue = [...ids]
  return createStrongFlowReviewAnnotations({
    nextId: () => queue.shift() ?? 'overflow',
    nowMillis: () => 1_000,
  })
}

function fileLine(path = 'src/current.ts', line = 12) {
  return { kind: 'file-line', path, line }
}

test('anchor labels name the exact reviewed target', () => {
  assert.equal(strongFlowReviewAnchorLabel(fileLine('src/a.ts', 3)), 'src/a.ts:3')
  assert.equal(strongFlowReviewAnchorLabel({ kind: 'task', deliveryTaskId: 'task:1' }), 'task:1')
  assert.equal(
    strongFlowReviewAnchorLabel({ kind: 'solution-node', nodeId: 'node:2' }),
    'node:2',
  )
  assert.equal(
    strongFlowReviewAnchorLabel({ kind: 'criterion', criterionId: 'criterion:1' }),
    'criterion:1',
  )
})

test('local annotations can be added, modified, deleted and summarized', () => {
  const draft = draftWith()
  draft.synchronize(projection())

  const first = draft.add({ anchor: fileLine(), note: 'Guard the narrowed loop.' })
  const second = draft.add({ anchor: { kind: 'task', deliveryTaskId: 'task:1' }, note: 'Split this task.' })
  const third = draft.add({ anchor: { kind: 'solution-node', nodeId: 'node:1' }, note: 'Cache lives here.' })
  const fourth = draft.add({ anchor: { kind: 'criterion', criterionId: 'criterion:1' }, note: 'Not covered.' })

  assert.equal(draft.state.annotations.length, 4)
  assert.equal(draft.state.annotations[0].id, first)
  assert.deepEqual(draft.state.annotations[0].anchor, fileLine())
  assert.equal(draft.state.annotations[0].identity.candidateDigest, candidateDigest)
  assert.equal(draft.state.annotations[0].identity.deliveryRevision, 4)

  draft.update(second, 'Split this task and pin its owner.')
  assert.equal(draft.state.annotations[1].note, 'Split this task and pin its owner.')

  draft.reanchor(third, { kind: 'solution-node', nodeId: 'node:2' })
  assert.equal(draft.state.annotations[2].anchor.nodeId, 'node:2')

  draft.remove(fourth)
  assert.deepEqual(draft.state.annotations.map(annotation => annotation.id), [first, second, third])

  const summary = draft.summarize()
  assert.deepEqual(summary, [
    'src/current.ts:12 — Guard the narrowed loop.',
    'task:1 — Split this task and pin its owner.',
    'node:2 — Cache lives here.',
  ])

  draft.remove(first)
  draft.remove(second)
  draft.remove(third)
  assert.equal(draft.state.annotations.length, 0)
  assert.deepEqual(draft.summarize(), [])
})

test('annotation notes must be bounded, non-empty text', () => {
  const draft = draftWith()
  draft.synchronize(projection())

  assert.throws(() => draft.add({ anchor: fileLine(), note: '   ' }), errorWithCode(
    'STRONGFLOW_REVIEW_DRAFT_INVALID',
  ))
  assert.throws(() => draft.add({ anchor: fileLine(), note: 'x'.repeat(2_001) }), errorWithCode(
    'STRONGFLOW_REVIEW_DRAFT_INVALID',
  ))
  assert.throws(() => draft.add({ anchor: { kind: 'file-line', path: 'src/a.ts', line: 0 } }), errorWithCode(
    'STRONGFLOW_REVIEW_DRAFT_INVALID',
  ))
  const id = draft.add({ anchor: fileLine(), note: 'ok' })
  assert.equal(draft.state.annotations.length, 1)
  assert.throws(() => draft.update(id, ''), errorWithCode('STRONGFLOW_REVIEW_DRAFT_INVALID'))
  assert.equal(draft.state.annotations[0].note, 'ok')
})

test('annotations stay bound to one Delivery and are dropped when it changes', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  const id = draft.add({ anchor: fileLine(), note: 'note' })

  const otherDelivery = projection({
    delivery: {
      ...projection().delivery,
      deliveryId: 'dlv_00000000000000000000000002',
    },
  })
  draft.synchronize(otherDelivery)
  assert.deepEqual(draft.state.annotations, [])
  assert.throws(() => draft.update(id, 'moved'), errorWithCode('STRONGFLOW_REVIEW_DRAFT_INVALID'))
})

test('a changed candidate keeps every draft and asks the reviewer to re-confirm it', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  const fileNote = draft.add({ anchor: fileLine(), note: 'still true' })
  const taskNote = draft.add({ anchor: { kind: 'task', deliveryTaskId: 'task:1' }, note: 'still scoped' })

  draft.synchronize(projection({
    currentCandidate: {
      candidateRef: nextCandidateRef,
      candidateCommitId: '9'.repeat(40),
      candidateTreeId: '8'.repeat(40),
      diffSha256: nextCandidateDigest,
      frozenAt: '2026-09-02T02:00:04.000Z',
    },
  }))

  // Drafts are never silently dropped.
  assert.deepEqual(draft.state.annotations.map(annotation => annotation.id), [fileNote, taskNote])
  assert.deepEqual(draft.state.staleness.map(entry => entry.id), [fileNote, taskNote])
  assert.equal(draft.state.staleness[0].reason, 'candidate-changed')
  assert.equal(draft.state.staleness[0].captured.candidateDigest, candidateDigest)
  assert.equal(draft.state.staleness[0].current.candidateDigest, nextCandidateDigest)

  // Composition fails closed while a stale note is unresolved.
  assert.throws(
    () => draft.compose({ target: 'attention-resolution', attentionItemId: 'attention:1' }),
    errorWithCode('STRONGFLOW_REVIEW_DRAFT_STALE'),
  )

  // Re-confirming re-pins the note onto the current candidate without losing text.
  draft.confirm(fileNote)
  assert.deepEqual(draft.state.staleness.map(entry => entry.id), [taskNote])
  assert.equal(draft.state.annotations[0].identity.candidateDigest, nextCandidateDigest)
  assert.equal(draft.state.annotations[0].note, 'still true')
  draft.confirm(taskNote)
  assert.deepEqual(draft.state.staleness, [])

  // Discarding is the explicit other path; it removes only the chosen note.
  draft.synchronize(projection({
    currentCandidate: {
      candidateRef: nextCandidateRef,
      candidateCommitId: '9'.repeat(40),
      candidateTreeId: '8'.repeat(40),
      diffSha256: nextCandidateDigest,
      frozenAt: '2026-09-02T02:00:04.000Z',
    },
  }))
  draft.discard(fileNote)
  assert.deepEqual(draft.state.annotations.map(annotation => annotation.id), [taskNote])
})

test('a delivery revision change alone marks drafts stale but keeps them', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  const id = draft.add({ anchor: fileLine(), note: 'note' })

  draft.synchronize(projection({
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-09-02T01:00:07.000Z',
      revisions: { delivery: 5, deliverySpec: 3, runtime: 8, publication: 1 },
      readCursor: {},
    },
  }))

  assert.deepEqual(draft.state.annotations.map(annotation => annotation.id), [id])
  assert.equal(draft.state.staleness.length, 1)
  assert.equal(draft.state.staleness[0].reason, 'delivery-revision-changed')
  assert.equal(draft.state.staleness[0].captured.deliveryRevision, 4)
  assert.equal(draft.state.staleness[0].current.deliveryRevision, 5)
})

test('bounded rework composes into exactly one legal command scope', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  draft.add({ anchor: fileLine('src/current.ts', 12), note: 'Guard the narrowed loop.' })
  draft.add({ anchor: { kind: 'task', deliveryTaskId: 'task:1' }, note: 'Split this task.' })
  draft.add({ anchor: { kind: 'solution-node', nodeId: 'node:1' }, note: 'Cache lives here.' })

  const plan = draft.compose({
    target: 'bounded-rework',
    attentionItemId: 'attention:1',
    nodeId: 'node:1',
    deliveryTaskId: 'task:1',
  })

  assert.equal(plan.command, 'delivery.resolve_attention')
  assert.equal(plan.target, 'bounded-rework')
  assert.deepEqual(plan.solutionReview, null)
  assert.notEqual(plan.attention, null)
  assert.equal(plan.attention.decision, 'resolve')
  assert.equal(plan.attention.attentionItemId, 'attention:1')
  assert.equal(plan.attention.remediation.nodeId, 'node:1')
  assert.equal(plan.attention.remediation.deliveryTaskId, 'task:1')
  // The view model wraps plain instructions into the rework protocol JSON, so
  // the composed text stays human readable here.
  assert.match(plan.attention.remediation.instructions, /Guard the narrowed loop\./u)
  assert.doesNotMatch(plan.attention.remediation.instructions, /winwincode\.client-rework-instructions/u)
  assert.deepEqual(plan.annotationIds, ['a', 'b', 'c'])

  // The final bounded rework scope is shown before submit and names every axis.
  assert.equal(plan.summary.length > 0, true)
  const scopeText = plan.summary.join('\n')
  assert.match(scopeText, /node:1/u)
  assert.match(scopeText, /task:1/u)
  assert.match(scopeText, /sha256:3{64}/u)
  assert.match(scopeText, /src\/current\.ts:12/u)
  assert.match(scopeText, /3 notes staged/u)
})

test('bounded rework refuses anchors that are no longer in the current snapshot', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  draft.add({ anchor: fileLine(), note: 'note' })

  assert.throws(
    () => draft.compose({
      target: 'bounded-rework',
      attentionItemId: 'attention:1',
      nodeId: 'node:missing',
      deliveryTaskId: 'task:1',
    }),
    errorWithCode('STRONGFLOW_REVIEW_DRAFT_ANCHOR_STALE'),
  )
  assert.throws(
    () => draft.compose({
      target: 'bounded-rework',
      attentionItemId: 'attention:1',
      nodeId: 'node:1',
      deliveryTaskId: 'task:missing',
    }),
    errorWithCode('STRONGFLOW_REVIEW_DRAFT_ANCHOR_STALE'),
  )
  assert.throws(
    () => draft.compose({
      target: 'attention-resolution',
      attentionItemId: 'attention:resolved-already',
    }),
    errorWithCode('STRONGFLOW_REVIEW_DRAFT_TARGET_UNAVAILABLE'),
  )
  assert.throws(
    () => draft.compose({
      target: 'bounded-rework',
      attentionItemId: 'attention:resolved-already',
      nodeId: 'node:1',
    }),
    errorWithCode('STRONGFLOW_REVIEW_DRAFT_TARGET_UNAVAILABLE'),
  )
})

test('bounded rework keeps the existing optional Task scope of the legal command', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  draft.add({ anchor: { kind: 'solution-node', nodeId: 'node:1' }, note: 'Cache lives here.' })

  const plan = draft.compose({
    target: 'bounded-rework',
    attentionItemId: 'attention:1',
    nodeId: 'node:1',
  })

  assert.equal(plan.attention.remediation.deliveryTaskId, null)
  assert.match(plan.summary.join('\n'), /Delivery task · none/u)
  assert.deepEqual(plan.annotationIds, ['a'])
})

test('requested changes compose into the existing solution review decision input', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  draft.add({ anchor: fileLine('src/current.ts', 12), note: 'Guard the narrowed loop.' })
  draft.add({ anchor: { kind: 'criterion', criterionId: 'criterion:1' }, note: 'Add a failing test first.' })

  const plan = draft.compose({
    target: 'requested-changes',
    comments: 'Please tighten the implementation.',
  })

  assert.equal(plan.command, 'delivery.resolve_attention')
  assert.deepEqual(plan.attention, null)
  assert.deepEqual(plan.solutionReview, {
    action: 'request_changes',
    comments: 'Please tighten the implementation.',
    requestedChanges: [
      'src/current.ts:12 — Guard the narrowed loop.',
      'criterion:1 — Add a failing test first.',
    ],
  })
  assert.deepEqual(plan.annotationIds, ['a', 'b'])
  assert.equal(plan.summary.join('\n').includes('Guard the narrowed loop.'), true)
})

test('requested changes refuse a snapshot whose solution review is not pending', () => {
  const draft = draftWith()
  draft.synchronize(projection({
    solutionReview: {
      reviewStatus: 'approved',
      attentionItemId: 'attention:1',
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      reviewStageRunId: stageRunId,
      reviewSetSha256: `sha256:${'7'.repeat(64)}`,
      architectureDiagram: diagram('system-architecture'),
      processDiagram: diagram('process-flow'),
    },
  }))
  draft.add({ anchor: fileLine(), note: 'Guard the narrowed loop.' })

  assert.throws(
    () => draft.compose({ target: 'requested-changes' }),
    errorWithCode('STRONGFLOW_REVIEW_DRAFT_TARGET_UNAVAILABLE'),
  )
})

test('attention resolution composes into the existing attention decision input', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  draft.add({ anchor: fileLine(), note: 'Explain the rollback path.' })

  const plan = draft.compose({ target: 'attention-resolution', attentionItemId: 'attention:1' })

  assert.equal(plan.command, 'delivery.resolve_attention')
  assert.deepEqual(plan.solutionReview, null)
  assert.equal(plan.attention.remediation, null)
  assert.equal(plan.attention.attentionItemId, 'attention:1')
  assert.equal(plan.attention.decision, 'resolve')
  assert.match(plan.attention.resolution, /Explain the rollback path\./u)
  assert.deepEqual(plan.annotationIds, ['a'])
})

test('a settled submission clears only the annotations it submitted', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  draft.add({ anchor: fileLine(), note: 'one' })
  draft.add({ anchor: { kind: 'task', deliveryTaskId: 'task:1' }, note: 'two' })

  const plan = draft.compose({ target: 'attention-resolution', attentionItemId: 'attention:1' })
  draft.begin(plan)
  assert.deepEqual(draft.state.submission, plan.annotationIds)
  assert.throws(() => draft.add({ anchor: fileLine(), note: 'three' }), errorWithCode(
    'STRONGFLOW_REVIEW_DRAFT_IN_FLIGHT',
  ))

  draft.settle('failure')
  assert.equal(draft.state.submission, null)
  assert.equal(draft.state.annotations.length, 2)

  const retry = draft.compose({ target: 'attention-resolution', attentionItemId: 'attention:1' })
  draft.begin(retry)
  draft.settle('cancelled')
  assert.equal(draft.state.annotations.length, 2)

  const last = draft.compose({ target: 'attention-resolution', attentionItemId: 'attention:1' })
  draft.begin(last)
  draft.settle('success')
  assert.equal(draft.state.submission, null)
  assert.deepEqual(draft.state.annotations, [])
})

test('nothing is cleared while a submission is unresolved', () => {
  const draft = draftWith()
  draft.synchronize(projection())
  draft.add({ anchor: fileLine(), note: 'one' })
  const plan = draft.compose({ target: 'attention-resolution', attentionItemId: 'attention:1' })
  draft.begin(plan)
  draft.settle('cancelled')
  assert.equal(draft.state.annotations.length, 1)
  assert.equal(draft.state.submission, null)
})

test('the workbench panel stages, summarizes and submits through the existing view model', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(reviewState())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    evidence: pageEvidenceOptions(),
  })

  const panel = findByClass(rootElement, 'wwc-strongflow-review-draft')
  assert.notEqual(panel, null)
  const list = findByClass(rootElement, 'wwc-strongflow-review-draft-list')
  assert.equal(list.children.length, 0)

  const anchorKind = findByClass(rootElement, 'wwc-strongflow-review-draft-kind')
  const anchorValue = findByClass(rootElement, 'wwc-strongflow-review-draft-anchor')
  const anchorLine = findByClass(rootElement, 'wwc-strongflow-review-draft-line')
  const note = findByClass(rootElement, 'wwc-strongflow-review-draft-note')
  const add = findByClass(rootElement, 'wwc-strongflow-review-draft-add')

  anchorKind.value = 'file-line'
  anchorValue.value = 'src/current.ts'
  anchorLine.value = '12'
  note.value = 'Guard the narrowed loop.'
  add.emit('click')
  assert.equal(list.children.length, 1)
  assert.match(list.children[0].textContent, /src\/current\.ts:12/u)
  assert.match(list.children[0].textContent, /Guard the narrowed loop\./u)

  // Modify the staged note in place.
  const edit = findByClass(list.children[0], 'wwc-strongflow-review-draft-edit')
  edit.emit('click')
  const editor = findByClass(list.children[0], 'wwc-strongflow-review-draft-note-input')
  editor.value = 'Guard the narrowed loop with a bounds check.'
  findByClass(list.children[0], 'wwc-strongflow-review-draft-save').emit('click')
  assert.match(list.textContent, /Guard the narrowed loop with a bounds check\./u)

  // Summarize before submitting.
  findByClass(rootElement, 'wwc-strongflow-review-draft-summary-button').emit('click')
  const summary = findByClass(rootElement, 'wwc-strongflow-review-draft-summary')
  assert.match(summary.textContent, /src\/current\.ts:12/u)

  // One submit reaches the view model through one existing legal command input.
  const target = findByClass(rootElement, 'wwc-strongflow-review-draft-target')
  target.value = 'attention-resolution'
  const attentionItem = findByClass(rootElement, 'wwc-strongflow-review-draft-attention')
  attentionItem.value = 'attention:1'
  findByClass(rootElement, 'wwc-strongflow-review-draft-submit').emit('click')
  await flush()

  assert.equal(model.calls.filter(([name]) => name === 'resolveAttention').length, 1)
  const call = model.calls.find(([name]) => name === 'resolveAttention')
  assert.equal(call[1].attentionItemId, 'attention:1')
  assert.equal(call[1].decision, 'resolve')
  assert.equal(call[1].remediation, null)
  assert.match(call[1].resolution, /Guard the narrowed loop with a bounds check\./u)
  assert.equal(list.children.length, 0)

  mounted.close()
  assert.deepEqual(model.calls.at(-1), ['close'])
  assert.deepEqual(rootElement.children, [])
})

test('the panel keeps a stale draft and surfaces the re-confirm choice', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(reviewState())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    evidence: pageEvidenceOptions(),
  })

  const list = findByClass(rootElement, 'wwc-strongflow-review-draft-list')
  findByClass(rootElement, 'wwc-strongflow-review-draft-kind').value = 'file-line'
  findByClass(rootElement, 'wwc-strongflow-review-draft-anchor').value = 'src/current.ts'
  findByClass(rootElement, 'wwc-strongflow-review-draft-line').value = '12'
  findByClass(rootElement, 'wwc-strongflow-review-draft-note').value = 'still true'
  findByClass(rootElement, 'wwc-strongflow-review-draft-add').emit('click')
  assert.equal(list.children.length, 1)

  const nextProjection = reviewState()
  nextProjection.projection.currentCandidate = {
    candidateRef: nextCandidateRef,
    candidateCommitId: '9'.repeat(40),
    candidateTreeId: '8'.repeat(40),
    diffSha256: nextCandidateDigest,
    frozenAt: '2026-09-02T02:00:04.000Z',
  }
  model.publish(nextProjection)

  assert.equal(list.children.length, 1)
  const stale = findByClass(rootElement, 'wwc-strongflow-review-draft-stale')
  assert.notEqual(stale, null)
  assert.match(stale.textContent, /candidate changed/u)

  // Submit stays disabled while a note is stale; the draft is preserved.
  const submit = findByClass(rootElement, 'wwc-strongflow-review-draft-submit')
  assert.equal(submit.disabled, true)
  assert.equal(list.children.length, 1)

  findByClass(rootElement, 'wwc-strongflow-review-draft-confirm').emit('click')
  assert.equal(findByClass(rootElement, 'wwc-strongflow-review-draft-stale').hidden, true)
  assert.equal(list.children.length, 1)
  assert.equal(submit.disabled, false)

  mounted.close()
  assert.deepEqual(rootElement.children, [])
})

test('read-only workbench stages nothing and keeps the panel inert', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(reviewState())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    evidence: pageEvidenceOptions(),
    readOnly: true,
  })

  assert.notEqual(findByClass(rootElement, 'wwc-strongflow-review-draft'), null)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-review-draft-add').disabled, true)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-review-draft-submit').disabled, true)
  const panel = mountStrongFlowReviewPanel({
    document,
    root: document.createElement('section'),
    draft: createStrongFlowReviewAnnotations(),
    model,
    readOnly: true,
  })
  assert.equal(panel.root.getAttribute('aria-label'), 'Staged review notes')
  panel.close()

  mounted.close()
  assert.deepEqual(rootElement.children, [])
})

test('the panel shows the final bounded rework scope before any submit', () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(reviewState())
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    evidence: pageEvidenceOptions(),
  })

  const list = findByClass(rootElement, 'wwc-strongflow-review-draft-list')
  const blocker = findByClass(rootElement, 'wwc-strongflow-review-draft-scope-blocker')
  assert.equal(blocker.hidden, false)
  assert.match(blocker.textContent, /Stage a note/u)

  findByClass(rootElement, 'wwc-strongflow-review-draft-kind').value = 'solution-node'
  findByClass(rootElement, 'wwc-strongflow-review-draft-anchor').value = 'node:1'
  findByClass(rootElement, 'wwc-strongflow-review-draft-note').value = 'Cache lives here.'
  findByClass(rootElement, 'wwc-strongflow-review-draft-add').emit('click')
  assert.equal(list.children.length, 1)

  findByClass(rootElement, 'wwc-strongflow-review-draft-target').value = 'bounded-rework'
  findByClass(rootElement, 'wwc-strongflow-review-draft-node').value = 'node:2'
  const taskSelect = findByClass(rootElement, 'wwc-strongflow-review-draft-task')
  taskSelect.value = 'task:1'
  taskSelect.emit('change')

  const scope = findByClass(rootElement, 'wwc-strongflow-review-draft-scope')
  assert.equal(scope.hidden, false)
  const scopeText = scope.textContent
  assert.match(scopeText, /Bounded rework scope · node:2/u)
  assert.match(scopeText, /Delivery task · task:1/u)
  assert.match(scopeText, new RegExp(`Candidate · ${candidateDigest.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`))
  assert.match(scopeText, /node:1 — Cache lives here\./u)
  assert.match(scopeText, /1 note staged/u)

  // Showing the scope is a preview only: no command has been sent.
  assert.deepEqual(model.calls.filter(([name]) => (
    name === 'resolveAttention' || name === 'decideSolutionReview'
  )), [])

  mounted.close()
  assert.deepEqual(rootElement.children, [])
})

test('the standalone panel mounts one labelled region and closes cleanly', () => {
  const document = new FakeDocument()
  const root = document.createElement('section')
  const model = new FakeStrongFlowViewModel(reviewState())
  const panel = mountStrongFlowReviewPanel({
    document,
    root,
    draft: createStrongFlowReviewAnnotations(),
    model,
  })
  assert.equal(panel.root.getAttribute('aria-label'), 'Staged review notes')
  panel.update()
  panel.close()
  assert.deepEqual(root.children, [])
})

function errorWithCode(code) {
  return error => {
    assert.ok(error instanceof StrongFlowReviewDraftError, `expected ${code}, got ${error}`)
    assert.equal(error.code, code)
    return true
  }
}

async function flush() {
  for (let index = 0; index < 8; index += 1) await new Promise(resolveQueue => setImmediate(resolveQueue))
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
  readOnly = false
  required = false
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
    const event = { target: this, preventDefault() {}, ...values }
    let current = this
    while (current !== null) {
      for (const listener of current.listeners.get(name) ?? []) listener(event)
      current = current.parentNode
    }
  }

  closest(selector) {
    if (selector.startsWith('.') && this.className.split(' ').includes(selector.slice(1))) return this
    return this.parentNode?.closest?.(selector) ?? null
  }

  focus() { this.ownerDocument.activeElement = this }

  blur() {
    if (this.ownerDocument.activeElement === this) this.ownerDocument.activeElement = null
  }
}

class FakeDocument {
  activeElement = null

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

class FakeStrongFlowViewModel {
  constructor(initialState) {
    this.state = initialState
  }

  draftScope = '["review-annotation-test-actor","review-annotation-test-scope"]'
  calls = []
  loaderCalls = []
  candidateHistory = []
  candidateReviews = {}
  listeners = new Set()

  subscribe(listener) {
    this.listeners.add(listener)
    listener(this.state)
    return () => { this.listeners.delete(listener) }
  }

  publish(next) {
    this.state = next
    for (const listener of this.listeners) listener(next)
  }

  async start() { this.calls.push(['start']) }
  async refresh() { this.calls.push(['refresh']) }
  async loadCandidateFiles() { this.calls.push(['loadCandidateFiles']) }
  async loadMoreCandidateFiles() { this.calls.push(['loadMoreCandidateFiles']) }
  async selectCandidateFile(path) { this.calls.push(['selectCandidateFile', path]) }
  async loadMoreCandidateDiff() { this.calls.push(['loadMoreCandidateDiff']) }
  async decideSolutionReview(input) { this.calls.push(['decideSolutionReview', input]) }
  async approveTaskBreakdown() { this.calls.push(['approveTaskBreakdown']) }
  async resolveAttention(input) { this.calls.push(['resolveAttention', input]) }
  async loadDeliveryCandidates() {
    this.loaderCalls.push(['loadDeliveryCandidates'])
    return this.candidateHistory
  }
  async loadCandidateHistoricalReview(candidate) {
    this.loaderCalls.push(['loadCandidateHistoricalReview', candidate.candidateRef])
    return this.candidateReviews?.[candidate.candidateRef] ?? null
  }
  async submitVerdict() { this.calls.push(['submitVerdict']) }
  async advanceDelivery() { this.calls.push(['advanceDelivery']) }
  cancelPending() { this.calls.push(['cancelPending']) }
  reconnect() { this.calls.push(['reconnect']) }
  close() { this.calls.push(['close']) }
}

class FakeDeliveryListModel {
  constructor(visible) {
    this.state = {
      status: 'ready',
      filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
      visible,
      loadedCount: visible.length,
      hasMore: false,
      loadingMore: false,
      moreFailure: null,
      error: null,
      advance: { deliveryId: null, failure: null },
    }
  }

  calls = []
  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }

  async refresh() { this.calls.push(['refresh']) }
  async loadMore() { this.calls.push(['loadMore']) }
  setSearch(value) { this.calls.push(['setSearch', value]) }
  async setStatusFilter(value) { this.calls.push(['setStatusFilter', value]) }
  setAttentionOnly(value) { this.calls.push(['setAttentionOnly', value]) }
  setOrder(value) { this.calls.push(['setOrder', value]) }
  async advanceDelivery(id, revision) { this.calls.push(['advanceDelivery', id, revision]) }
  close() { this.calls.push(['close']) }
}

function fakeDeliveryList(visible) {
  return new FakeDeliveryListModel(visible)
}

function pageEvidenceDeepLink() {
  const state = { hash: `#/strongflow?delivery=${deliveryId}` }
  const link = {
    get route() {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      const tab = parameters.get('tab')
      return {
        tab: tab === 'preview' || tab === 'tests' || tab === 'logs' ? tab : 'evidence',
        evidenceId: parameters.get('evidence'),
      }
    },
    onRouteChange(route) {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      parameters.set('tab', route.tab)
      if (route.evidenceId === null) parameters.delete('evidence')
      else parameters.set('evidence', route.evidenceId)
      state.hash = `#/strongflow?${parameters.toString()}`
    },
    state,
  }
  return link
}

function pageEvidenceOptions(overrides = {}) {
  const link = pageEvidenceDeepLink()
  return {
    client: {
      queries: [],
      async query() {
        throw new Error('unexpected evidence query')
      },
    },
    actor,
    scope,
    nextRequestId: () => 'req_00000000000000000000000001',
    route: link.route,
    onRouteChange: link.onRouteChange,
    ...overrides,
  }
}
