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
    'apps/client/tsconfig.strongflow-evidence-tests.json',
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
  `StrongFlow Evidence did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const evidenceModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-evidence-tests/strongflow-evidence.js',
)).href}`)
const contracts = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-evidence-tests/generated/contracts.js',
)).href}`)

const generatedClient = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-evidence-tests/generated/control-plane-client.js',
)).href}`)

const {
  createStrongFlowEvidenceViewModel,
  mountStrongFlowEvidence,
  strongFlowEvidenceSummary,
  strongFlowEvidenceErrorText,
  strongFlowEvidenceOutcomePresentation,
  strongFlowEvidenceRowsForTab,
} = evidenceModule
const { EvidenceOutcome, QueryName } = contracts
const { matchesCanonicalSchema } = generatedClient

const schemaVersion = 'winwincode/v1'
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const sessionBindingId = 'sbd_00000000000000000000000001'
const productSessionId = 'psn_00000000000000000000000001'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const currentCandidateRef = 'git-candidate:sha256:' + 'a'.repeat(64)
const supersededCandidateRef = 'git-candidate:sha256:' + 'b'.repeat(64)
const foreignEvidenceId = 'evd_0000000000000000000000000Z'
const missingEvidenceId = 'evd_0000000000000000000000000Y'

function evidenceId(value) {
  return `evd_${String(value).padStart(26, '0')}`
}

function readCursor(overrides = {}) {
  return {
    deliveryId,
    deliveryRevision: 6,
    eventCursor: {
      eventId: 'evt_00000000000000000000000001',
      sequence: 12,
      scope,
      stream: { kind: 'delivery', deliveryId },
    },
    publicationRevision: 0,
    runtimeAcceptedSequence: 4,
    runtimeLedgerRevision: 5,
    scope,
    token: 'cursor-token-1',
    ...overrides,
  }
}

function evidenceRow(value) {
  return {
    candidateRef: currentCandidateRef,
    createdAt: '2026-09-02T01:00:00.000Z',
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 2,
    id: evidenceId(value),
    sessionBindingId,
    sourceRef: `artifact:source:${String(value)}`,
    stageRunId,
    type: 'test',
  }
}

function projection(overrides = {}) {
  return {
    delivery: {
      schemaVersion,
      deliveryId,
      deliveryRevision: 6,
      status: 'executing',
      ownership: {
        organizationId: scope.organizationId,
        workspaceId: scope.workspaceId,
        projectId: scope.projectId,
        repositoryId: scope.repositoryId,
      },
      requirements: {
        title: 'Evidence workbench',
        goal: 'Open exact Evidence detail without unbounded reads.',
      },
      tasks: [],
      stages: [{
        id: stageRunId,
        stage: 'executing',
        role: 'implementer',
        status: 'running',
      }],
      attention: [],
    },
    solutionReview: null,
    stage: { id: stageRunId },
    runtime: { stageRunId, sessions: [] },
    evidence: [
      evidenceRow(1),
      { ...evidenceRow(2), type: 'command' },
      { ...evidenceRow(3), type: 'runtime_event' },
      { ...evidenceRow(4), type: 'diff', candidateRef: supersededCandidateRef },
      { ...evidenceRow(5), type: 'commit' },
    ],
    verdict: null,
    attention: [],
    currentCandidate: {
      candidateRef: currentCandidateRef,
      candidateCommitId: '1'.repeat(40),
      candidateTreeId: '2'.repeat(40),
      diffSha256: `sha256:${'3'.repeat(64)}`,
      frozenAt: '2026-09-02T00:59:00.000Z',
    },
    publication: null,
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-09-02T01:00:00.000Z',
      revisions: { delivery: 6, deliverySpec: 2, runtime: 5, publication: 0 },
      readCursor: readCursor(),
    },
    ...overrides,
  }
}

function modelState(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    projection: projection(),
    interaction: { status: 'idle', error: null },
    error: null,
    ...overrides,
  }
}

class FakeStrongFlowModel {
  constructor(initialState = modelState()) {
    this.state = initialState
  }

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
}

function evidenceDetailResult(overrides = {}) {
  return {
    kind: 'evidence_detail',
    artifactAccess: {
      state: 'unavailable',
      reason: 'no_authoritative_link',
    },
    evidence: evidenceRow(1),
    outcome: EvidenceOutcome.Succeeded,
    readCursor: readCursor(),
    ...overrides,
  }
}

function evidenceGetResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: QueryName.EvidenceGet,
    result,
    page: { hasMore: false, nextCursor: null },
  }
}

function artifactDescriptor(overrides = {}) {
  return {
    artifactId: 'art_0000000000000000000000001A',
    digest: `sha256:${'c'.repeat(64)}`,
    fileName: 'run.log',
    kind: 'log',
    mediaType: 'text/plain',
    previewMode: 'inline_text',
    provenance: {
      candidateRef: currentCandidateRef,
      deliveryId,
      deliveryRevision: 6,
      evidenceId: evidenceId(2),
      sessionBindingId,
      stageRunId,
    },
    sizeBytes: 26,
    ...overrides,
  }
}

function artifactChunkResponse(request, overrides = {}) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: QueryName.EvidenceArtifactContentGet,
    result: {
      artifact: artifactDescriptor(),
      contentEncoding: 'utf-8',
      dataBase64: Buffer.from('log line one\nlog line two\n', 'utf8').toString('base64'),
      encoding: 'base64',
      evidence: { ...evidenceRow(2), type: 'command' },
      kind: 'evidence_artifact_content_chunk',
      nextOffset: null,
      offset: 0,
      previewMode: 'inline_text',
      readCursor: readCursor(),
      returnedBytes: 26,
      state: 'available',
      totalBytes: 26,
      truncated: false,
      ...overrides,
    },
    page: { hasMore: false, nextCursor: null },
  }
}

class FakeControlPlaneClient {
  constructor(handler = () => null) {
    this.handler = handler
    this.queries = []
  }

  queryOptions = []

  async query(request, options) {
    this.queries.push(structuredClone(request))
    this.queryOptions.push(options ?? null)
    const response = this.handler(request, this.queries.length)
    if (response === null) throw new Error('unexpected query')
    return response
  }
}

test('fixture Evidence identities satisfy the generated canonical schema', () => {
  for (const value of [
    ...Array.from({ length: 5 }, (_, index) => evidenceId(index + 1)),
    foreignEvidenceId,
    missingEvidenceId,
  ]) {
    assert.equal(matchesCanonicalSchema('EvidenceId', value), true, value)
  }
})

test('summary joins only verdict-owned criterion evidence and bounds key failures', () => {
  const rows = [evidenceRow(1), evidenceRow(2), evidenceRow(3)]
  const value = projection({
    evidence: rows,
    verdict: {
      status: 'fail',
      producedAt: '2026-09-02T02:00:00.000Z',
      criteria: [
        { criterionId: 'criterion:pass', resultId: 'result:1', verdict: 'pass', evidenceRefs: [rows[0].id], explanation: 'passed', evaluatedAt: '2026-09-02T02:00:00.000Z' },
        { criterionId: 'criterion:fail-a', resultId: 'result:2', verdict: 'fail', evidenceRefs: [rows[1].id], explanation: 'failed a', evaluatedAt: '2026-09-02T02:00:00.000Z' },
        { criterionId: 'criterion:infra', resultId: 'result:3', verdict: 'infra_error', evidenceRefs: [rows[1].id], explanation: 'infra', evaluatedAt: '2026-09-02T02:00:00.000Z' },
        { criterionId: 'criterion:fail-b', resultId: 'result:4', verdict: 'fail', evidenceRefs: [foreignEvidenceId], explanation: 'failed b', evaluatedAt: '2026-09-02T02:00:00.000Z' },
      ],
    },
  })
  const summary = strongFlowEvidenceSummary(value, 2)
  assert.deepEqual(summary.counts, { total: 4, pass: 1, fail: 2, inconclusive: 0, infraError: 1 })
  assert.deepEqual(summary.failures.map(failure => failure.criterionId), [
    'criterion:fail-a',
    'criterion:infra',
  ])
  assert.equal(summary.omittedFailures, 1)
  assert.deepEqual(summary.criterionIdsByEvidence.get(rows[1].id), [
    'criterion:fail-a',
    'criterion:infra',
  ])
  assert.equal(summary.criterionIdsByEvidence.has(foreignEvidenceId), false)
})

test('detail identity mismatch is rejected and every facade read receives a cancellable signal', async () => {
  const client = new FakeControlPlaneClient(request => evidenceGetResponse(
    request,
    evidenceDetailResult({ evidence: { ...evidenceRow(1), stageRunId: 'run_foreign' } }),
  ))
  const { created } = viewModel({ client })
  await created.openEvidence(evidenceId(1))
  assert.equal(created.state.detail.status, 'error')
  assert.equal(created.state.detail.error.code, 'STRONGFLOW_EVIDENCE_IDENTITY_MISMATCH')
  assert.equal(client.queryOptions[0].signal instanceof AbortSignal, true)
})

test('detail rejects foreign Evidence, Candidate, StageRun, SessionBinding, cursor, and Artifact provenance', async () => {
  const baseDescriptor = artifactDescriptor({
    previewMode: 'download_only',
    provenance: {
      ...artifactDescriptor().provenance,
      evidenceId: evidenceId(1),
    },
  })
  const cases = [
    { evidence: { ...evidenceRow(1), id: foreignEvidenceId } },
    { evidence: { ...evidenceRow(1), candidateRef: supersededCandidateRef } },
    { evidence: { ...evidenceRow(1), stageRunId: 'run_foreign' } },
    { evidence: { ...evidenceRow(1), sessionBindingId: 'binding:foreign' } },
    { readCursor: readCursor({ token: 'cursor-token-foreign' }) },
    {
      artifactAccess: {
        state: 'available',
        items: [{
          ...baseDescriptor,
          provenance: { ...baseDescriptor.provenance, deliveryId: 'delivery:foreign' },
        }],
      },
    },
    {
      artifactAccess: {
        state: 'available',
        items: [{
          ...baseDescriptor,
          provenance: { ...baseDescriptor.provenance, evidenceId: foreignEvidenceId },
        }],
      },
    },
  ]
  for (const overrides of cases) {
    const client = new FakeControlPlaneClient(request => evidenceGetResponse(
      request,
      evidenceDetailResult(overrides),
    ))
    const { created } = viewModel({ client })
    await created.openEvidence(evidenceId(1))
    assert.equal(created.state.detail.status, 'error')
    assert.equal(created.state.detail.error.code, 'STRONGFLOW_EVIDENCE_IDENTITY_MISMATCH')
  }
})

test('a changed snapshot identity clears stale bytes synchronously, aborts the old read, and reloads', async () => {
  let resolveFirst
  const first = new Promise(resolve => { resolveFirst = resolve })
  const client = new FakeControlPlaneClient((request, count) => {
    if (count === 1) return first
    return evidenceGetResponse(request, evidenceDetailResult({
      readCursor: readCursor({ token: 'cursor-token-2', deliveryRevision: 7 }),
    }))
  })
  const { created, model } = viewModel({ client })
  const opening = created.openEvidence(evidenceId(1))
  await new Promise(resolve => { setImmediate(resolve) })
  const firstSignal = client.queryOptions[0].signal
  model.publish(modelState({
    projection: projection({
      delivery: { ...projection().delivery, deliveryRevision: 7 },
      metadata: {
        ...projection().metadata,
        revisions: { ...projection().metadata.revisions, delivery: 7 },
        readCursor: readCursor({ token: 'cursor-token-2', deliveryRevision: 7 }),
      },
    }),
  }))
  assert.equal(firstSignal.aborted, true)
  assert.equal(created.state.detail.status, 'loading')
  assert.equal(created.state.content, null)
  resolveFirst(evidenceGetResponse(client.queries[0], evidenceDetailResult()))
  await opening
  await new Promise(resolve => { setImmediate(resolve) })
  assert.equal(client.queries.length, 2)
  assert.equal(created.state.detail.status, 'ready')
})

test('a transient StrongFlow refresh reopens the selected Evidence only under the new cursor', async () => {
  const nextCursor = readCursor({ token: 'cursor-token-2', deliveryRevision: 7 })
  const client = new FakeControlPlaneClient((request, count) => evidenceGetResponse(
    request,
    evidenceDetailResult(count === 1 ? {} : { readCursor: nextCursor }),
  ))
  const { created, model } = viewModel({ client })
  await created.openEvidence(evidenceId(1))

  model.publish(modelState({ status: 'refreshing', projection: null }))
  assert.equal(created.state.selected, null)
  assert.equal(created.state.detail, null)
  assert.equal(created.state.content, null)

  model.publish(modelState({
    projection: projection({
      delivery: { ...projection().delivery, deliveryRevision: 7 },
      metadata: {
        ...projection().metadata,
        revisions: { ...projection().metadata.revisions, delivery: 7 },
        readCursor: nextCursor,
      },
    }),
  }))
  await new Promise(resolve => { setImmediate(resolve) })

  assert.equal(client.queries.length, 2)
  assert.equal(created.state.selected.binding.atCursor.token, nextCursor.token)
  assert.equal(created.state.detail.status, 'ready')
})

test('content rejects foreign Evidence, cursor, Artifact id, digest, and provenance', async () => {
  const descriptor = artifactDescriptor({ sizeBytes: 26 })
  const cases = [
    { evidence: { ...evidenceRow(2), type: 'command', id: foreignEvidenceId } },
    { readCursor: readCursor({ token: 'cursor-token-foreign' }) },
    { artifact: { ...descriptor, artifactId: 'artifact:foreign' } },
    { artifact: { ...descriptor, digest: `sha256:${'d'.repeat(64)}` } },
    {
      artifact: {
        ...descriptor,
        provenance: { ...descriptor.provenance, stageRunId: 'run_foreign' },
      },
    },
  ]
  for (const overrides of cases) {
    const client = new FakeControlPlaneClient(request => {
      if (request.query === QueryName.EvidenceGet) {
        return evidenceGetResponse(request, evidenceDetailResult({
          evidence: { ...evidenceRow(2), type: 'command' },
          artifactAccess: { state: 'available', items: [descriptor] },
        }))
      }
      return artifactChunkResponse(request, { artifact: descriptor, ...overrides })
    })
    const { created } = viewModel({ client })
    await created.openEvidence(evidenceId(2))
    assert.equal(created.state.content.status, 'error')
    assert.equal(created.state.content.error.code, 'STRONGFLOW_EVIDENCE_CONTENT_IDENTITY_MISMATCH')
    assert.equal(client.queryOptions.at(-1).signal instanceof AbortSignal, true)
  }
})

test('content rejects repeated, backward, oversized, and inconsistent continuation ranges', async () => {
  const cases = [
    { offset: 1 },
    { nextOffset: 0 },
    { nextOffset: 30 },
    { totalBytes: 27 },
    { returnedBytes: 25 },
  ]
  for (const overrides of cases) {
    const descriptor = artifactDescriptor({ sizeBytes: 26 })
    const client = new FakeControlPlaneClient(request => {
      if (request.query === QueryName.EvidenceGet) return evidenceGetResponse(
        request,
        evidenceDetailResult({
          evidence: { ...evidenceRow(2), type: 'command' },
          artifactAccess: { state: 'available', items: [descriptor] },
        }),
      )
      return artifactChunkResponse(request, { artifact: descriptor, ...overrides })
    })
    const { created } = viewModel({ client })
    await created.openEvidence(evidenceId(2))
    assert.equal(created.state.content.status, 'error')
    assert.equal(created.state.content.error.code, 'STRONGFLOW_EVIDENCE_CONTENT_RANGE_INVALID')
  }
})

test('opening a replacement and closing the Drawer abort their in-flight facade reads', async () => {
  const pending = []
  const client = new FakeControlPlaneClient(request => new Promise(resolve => {
    pending.push({ request, resolve })
  }))
  const { created } = viewModel({ client })
  const first = created.openEvidence(evidenceId(1))
  await new Promise(resolve => { setImmediate(resolve) })
  const firstSignal = client.queryOptions[0].signal
  const second = created.openEvidence(evidenceId(2))
  await new Promise(resolve => { setImmediate(resolve) })
  const secondSignal = client.queryOptions[1].signal
  assert.equal(firstSignal.aborted, true)
  created.closeEvidence()
  assert.equal(secondSignal.aborted, true)
  pending[0].resolve(evidenceGetResponse(
    pending[0].request,
    evidenceDetailResult(),
  ))
  pending[1].resolve(evidenceGetResponse(
    pending[1].request,
    evidenceDetailResult({ evidence: { ...evidenceRow(2), type: 'command' } }),
  ))
  await Promise.all([first, second])
  assert.equal(created.state.selected, null)
  assert.equal(created.state.detail, null)
})

test('streaming UTF-8 preserves a code point split across bounded chunks', async () => {
  const descriptor = artifactDescriptor({ sizeBytes: 4 })
  const bytes = Buffer.from('🙂', 'utf8')
  const client = new FakeControlPlaneClient(request => {
    if (request.query === QueryName.EvidenceGet) {
      return evidenceGetResponse(request, evidenceDetailResult({
        evidence: { ...evidenceRow(2), type: 'command' },
        artifactAccess: { state: 'available', items: [descriptor] },
      }))
    }
    const offset = request.parameters.offset
    const part = offset === 0 ? bytes.subarray(0, 2) : bytes.subarray(2)
    return artifactChunkResponse(request, {
      artifact: descriptor,
      dataBase64: part.toString('base64'),
      offset,
      returnedBytes: 2,
      totalBytes: 4,
      nextOffset: offset === 0 ? 2 : null,
    })
  })
  const { created } = viewModel({ client })
  await created.openEvidence(evidenceId(2))
  assert.equal(created.state.content.text, '')
  await created.loadNextChunk()
  assert.equal(created.state.content.text, '🙂')
  assert.equal(created.state.content.complete, true)
})

test('all bounded Artifact descriptors remain selectable and switch exact content authority', async () => {
  const first = artifactDescriptor({ artifactId: 'artifact:first', sizeBytes: 26 })
  const second = artifactDescriptor({
    artifactId: 'artifact:second',
    digest: `sha256:${'e'.repeat(64)}`,
    fileName: 'second.log',
    sizeBytes: 26,
  })
  const client = new FakeControlPlaneClient(request => {
    if (request.query === QueryName.EvidenceGet) {
      return evidenceGetResponse(request, evidenceDetailResult({
        evidence: { ...evidenceRow(2), type: 'command' },
        artifactAccess: { state: 'available', items: [first, second] },
      }))
    }
    const descriptor = request.parameters.artifactId === first.artifactId ? first : second
    return artifactChunkResponse(request, { artifact: descriptor })
  })
  const { created } = viewModel({ client })
  await created.openEvidence(evidenceId(2))
  assert.deepEqual(created.state.detail.artifactAccess.items.map(item => item.artifactId), [
    first.artifactId,
    second.artifactId,
  ])
  await created.selectArtifact(second.artifactId)
  assert.equal(created.state.content.artifact.artifactId, second.artifactId)
  assert.equal(client.queries.at(-1).parameters.artifactId, second.artifactId)
})

let requestSequence = 0
function nextRequestId() {
  requestSequence += 1
  return `req_${String(requestSequence).padStart(26, '0')}`
}

function viewModel(overrides = {}) {
  const model = new FakeStrongFlowModel()
  const client = overrides.client ?? new FakeControlPlaneClient()
  const link = overrides.deepLink ?? deepLink()
  const created = createStrongFlowEvidenceViewModel({
    client,
    actor,
    scope,
    nextRequestId,
    model,
    route: link.route,
    onRouteChange: link.onRouteChange,
    ...(overrides.options ?? {}),
  })
  return { created, client, model }
}

test('outcome presentation covers exactly the canonical outcomes and never invents skipped', () => {
  assert.deepEqual(
    [...strongFlowEvidenceOutcomePresentation.keys()].sort(),
    [...Object.values(EvidenceOutcome)].sort(),
  )
  const failed = strongFlowEvidenceOutcomePresentation.get(EvidenceOutcome.Failed)
  const infra = strongFlowEvidenceOutcomePresentation.get(EvidenceOutcome.InfrastructureFailed)
  const passed = strongFlowEvidenceOutcomePresentation.get(EvidenceOutcome.Succeeded)
  assert.notEqual(failed.label, infra.label)
  assert.notEqual(failed.tone, infra.tone)
  assert.equal(passed.tone, 'pass')
  assert.equal(failed.tone, 'business-fail')
  assert.equal(infra.tone, 'infra')
  assert.equal(
    strongFlowEvidenceOutcomePresentation.get(EvidenceOutcome.Observed).tone,
    'neutral',
  )
})

test('tab rows keep every Evidence type in the Evidence tab and split tests and logs exactly', () => {
  const rows = projection().evidence
  assert.deepEqual(strongFlowEvidenceRowsForTab(rows, 'evidence').map(row => row.type), [
    'test',
    'command',
    'runtime_event',
    'diff',
    'commit',
  ])
  assert.deepEqual(strongFlowEvidenceRowsForTab(rows, 'tests').map(row => row.type), ['test'])
  assert.deepEqual(
    strongFlowEvidenceRowsForTab(rows, 'logs').map(row => row.type),
    ['command', 'runtime_event'],
  )
})

test('opening Evidence issues one exact evidence.get with the full read binding and no cursor page', async () => {
  let seen = null
  const client = new FakeControlPlaneClient(request => {
    if (request.query === QueryName.EvidenceGet) {
      seen = request
      return evidenceGetResponse(request, evidenceDetailResult())
    }
    return null
  })
  const { created } = viewModel({ client })
  await created.openEvidence(evidenceId(1))
  assert.equal(client.queries.length, 1)
  assert.equal(seen.schemaVersion, schemaVersion)
  assert.equal(seen.actor, actor)
  assert.equal(seen.scope, scope)
  assert.equal(seen.query, QueryName.EvidenceGet)
  assert.deepEqual(seen.page, { cursor: null, limit: 1 })
  assert.equal(typeof seen.requestId, 'string')
  assert.equal(seen.requestId.length > 0, true)
  assert.deepEqual(seen.parameters, {
    atCursor: readCursor(),
    candidateRef: currentCandidateRef,
    deliveryId,
    evidenceId: evidenceId(1),
    readPageLimit: 1,
    sessionBindingId,
    sourceRef: 'artifact:source:1',
    stageRunId,
    type: 'test',
  })
  const detail = created.state.detail
  assert.equal(detail.status, 'ready')
  assert.equal(detail.outcome, EvidenceOutcome.Succeeded)
  assert.equal(detail.artifactAccess.state, 'unavailable')
})

test('Evidence detail answers must be single-page evidence.get results or become sanitized errors', async () => {
  const client = new FakeControlPlaneClient(request => evidenceGetResponse(request, evidenceDetailResult()))
  const { created } = viewModel({ client })
  await created.openEvidence(evidenceId(1))
  const wrongQuery = new FakeControlPlaneClient(request => ({
    ...evidenceGetResponse(request, evidenceDetailResult()),
    query: QueryName.DeliveryGet,
  }))
  const mismatched = viewModel({ client: wrongQuery })
  await mismatched.created.openEvidence(evidenceId(1))
  assert.equal(mismatched.created.state.detail.status, 'error')
  assert.equal(
    mismatched.created.state.detail.error.code,
    'STRONGFLOW_EVIDENCE_QUERY_MISMATCH',
  )
  const paged = new FakeControlPlaneClient(request => ({
    ...evidenceGetResponse(request, evidenceDetailResult()),
    page: { hasMore: true, nextCursor: 'next' },
  }))
  const pagedCase = viewModel({ client: paged })
  await pagedCase.created.openEvidence(evidenceId(1))
  assert.equal(pagedCase.created.state.detail.status, 'error')
  assert.equal(pagedCase.created.state.detail.error.code, 'STRONGFLOW_EVIDENCE_PAGE_INVALID')
})

test('opening Evidence without a current projection or outside the snapshot never queries', async () => {
  const client = new FakeControlPlaneClient(() => null)
  const empty = viewModel({ client })
  empty.model.publish(modelState({ projection: null }))
  await empty.created.openEvidence(evidenceId(1))
  assert.equal(empty.created.state.detail.status, 'error')
  assert.equal(empty.created.state.detail.error.code, 'STRONGFLOW_EVIDENCE_SNAPSHOT_REQUIRED')
  const present = viewModel({ client })
  await present.created.openEvidence(missingEvidenceId)
  assert.equal(present.created.state.detail.status, 'error')
  assert.equal(present.created.state.detail.error.code, 'STRONGFLOW_EVIDENCE_NOT_IN_SNAPSHOT')
  assert.equal(client.queries.length, 0)
})

test('unavailable artifact access keeps an explicit unavailable state and never reads content', async () => {
  const client = new FakeControlPlaneClient(request => evidenceGetResponse(
    request,
    evidenceDetailResult({ artifactAccess: { state: 'unavailable', reason: 'no_authoritative_link' } }),
  ))
  const { created } = viewModel({ client })
  await created.openEvidence(evidenceId(1))
  assert.equal(created.state.content.status, 'unavailable')
  await created.loadNextChunk()
  assert.equal(client.queries.filter(
    query => query.query === QueryName.EvidenceArtifactContentGet,
  ).length, 0)
})

test('inline text artifacts load bounded chunks with continuation and dedupe repeat loads', async () => {
  const descriptor = artifactDescriptor({ sizeBytes: 512 * 1024 })
  const client = new FakeControlPlaneClient((request, count) => {
    if (request.query === QueryName.EvidenceGet) {
      return evidenceGetResponse(request, evidenceDetailResult({
        evidence: { ...evidenceRow(2), type: 'command' },
        artifactAccess: { state: 'available', items: [descriptor] },
      }))
    }
    if (request.query === QueryName.EvidenceArtifactContentGet) {
      assert.equal(request.parameters.length <= 256 * 1024, true)
      assert.equal(request.parameters.offset < descriptor.sizeBytes, true)
      assert.deepEqual(request.parameters.evidence, {
        atCursor: readCursor(),
        candidateRef: currentCandidateRef,
        deliveryId,
        evidenceId: evidenceId(2),
        readPageLimit: 1,
        sessionBindingId,
        sourceRef: 'artifact:source:2',
        stageRunId,
        type: 'command',
      })
      return artifactChunkResponse(request, {
        artifact: descriptor,
        dataBase64: Buffer.from('x'.repeat(request.parameters.length)).toString('base64'),
        offset: request.parameters.offset,
        nextOffset: count === 2 ? 256 * 1024 : null,
        returnedBytes: request.parameters.length,
        totalBytes: descriptor.sizeBytes,
      })
    }
    return null
  })
  const { created } = viewModel({ client })
  await created.openEvidence(evidenceId(2))
  assert.equal(created.state.content.status, 'ready')
  await created.loadNextChunk()
  const contentQueries = client.queries.filter(
    query => query.query === QueryName.EvidenceArtifactContentGet,
  )
  assert.equal(contentQueries.length, 2)
  assert.equal(contentQueries[1].parameters.offset, 256 * 1024)
  assert.equal(created.state.content.loadedBytes, 512 * 1024)
  assert.equal(created.state.content.complete, true)
  await created.loadNextChunk()
  assert.equal(client.queries.filter(
    query => query.query === QueryName.EvidenceArtifactContentGet,
  ).length, 2)
})

test('binary, unknown-8bit, and download-only artifacts stay download-only without inline text', async () => {
  for (const overrides of [
    { contentEncoding: 'binary' },
    { contentEncoding: 'unknown-8bit' },
  ]) {
    const descriptor = artifactDescriptor()
    const client = new FakeControlPlaneClient(request => {
      if (request.query === QueryName.EvidenceGet) {
        return evidenceGetResponse(request, evidenceDetailResult({
          evidence: { ...evidenceRow(2), type: 'command' },
          artifactAccess: { state: 'available', items: [descriptor] },
        }))
      }
      return artifactChunkResponse(request, { ...overrides, artifact: descriptor })
    })
    const { created } = viewModel({ client })
    await created.openEvidence(evidenceId(2))
    assert.equal(created.state.content.status, 'download-only')
    assert.equal(created.state.content.text, null)
  }
  const descriptor = artifactDescriptor({ previewMode: 'download_only' })
  const client = new FakeControlPlaneClient(request => {
    if (request.query === QueryName.EvidenceGet) {
      return evidenceGetResponse(request, evidenceDetailResult({
        evidence: { ...evidenceRow(2), type: 'command' },
        artifactAccess: { state: 'available', items: [descriptor] },
      }))
    }
    return artifactChunkResponse(request, { artifact: descriptor })
  })
  const downloadOnly = viewModel({ client })
  await downloadOnly.created.openEvidence(evidenceId(2))
  assert.equal(downloadOnly.created.state.content.status, 'download-only')
})

test('downloading an artifact walks bounded ranges once and hands one base64 payload to the downloader', async () => {
  const descriptor = artifactDescriptor({ sizeBytes: 300 * 1024 })
  const downloads = []
  const client = new FakeControlPlaneClient((request, count) => {
    if (request.query === QueryName.EvidenceGet) {
      return evidenceGetResponse(request, evidenceDetailResult({
        evidence: { ...evidenceRow(2), type: 'command' },
        artifactAccess: { state: 'available', items: [descriptor] },
      }))
    }
    return artifactChunkResponse(request, {
      artifact: descriptor,
      offset: request.parameters.offset,
      nextOffset: count === 2 || count === 3 ? 256 * 1024 : null,
      returnedBytes: request.parameters.length,
      totalBytes: descriptor.sizeBytes,
      dataBase64: Buffer.from('x'.repeat(request.parameters.length), 'utf8').toString('base64'),
    })
  })
  const { created } = viewModel({
    client,
    options: {
      downloader: download => { downloads.push(download) },
    },
  })
  await created.openEvidence(evidenceId(2))
  await created.downloadArtifact()
  assert.equal(downloads.length, 1)
  assert.equal(downloads[0].mediaType, 'text/plain')
  assert.equal(downloads[0].fileName, 'run.log')
  assert.equal(
    Buffer.from(downloads[0].bytes).length,
    300 * 1024,
  )
  const contentQueries = client.queries.filter(
    query => query.query === QueryName.EvidenceArtifactContentGet,
  )
  assert.deepEqual(contentQueries.map(query => query.parameters.offset), [0, 0, 256 * 1024])
  assert.equal(contentQueries.every(query => query.parameters.length <= 256 * 1024), true)
})

test('Evidence errors are sanitized by code and kind and never expose server details', () => {
  const snapshotMoved = strongFlowEvidenceErrorText({
    kind: 'protocol',
    code: 'CANDIDATE_STALE',
    message: 'internal path /var/secret',
    requestId: 'req_1',
    retryable: false,
    details: { workerPath: '/worker' },
  })
  assert.match(snapshotMoved, /moved with its Delivery snapshot/u)
  assert.equal(snapshotMoved.includes('/var/secret'), false)
  assert.equal(snapshotMoved.includes('/worker'), false)
  assert.match(
    strongFlowEvidenceErrorText({
      kind: 'server',
      code: 'TRUSTED_FACTS_UNAVAILABLE',
      message: 'x',
      requestId: null,
      retryable: true,
      details: {},
    }),
    /temporarily unavailable/u,
  )
  assert.match(
    strongFlowEvidenceErrorText({
      kind: 'protocol',
      code: 'RESOURCE_NOT_FOUND',
      message: 'x',
      requestId: null,
      retryable: false,
      details: {},
    }),
    /not part of the current Delivery snapshot/u,
  )
  assert.match(
    strongFlowEvidenceErrorText({
      kind: 'network',
      code: 'NETWORK_ERROR',
      message: 'x',
      requestId: null,
      retryable: true,
      details: {},
    }),
    /could not be reached/u,
  )
  assert.match(
    strongFlowEvidenceErrorText({
      kind: 'protocol',
      code: 'UNEXPECTED_SERVER_CODE',
      message: 'raw leak',
      requestId: null,
      retryable: false,
      details: {},
    }),
    /could not be opened/u,
  )
  assert.equal(
    strongFlowEvidenceErrorText({
      kind: 'protocol',
      code: 'UNEXPECTED_SERVER_CODE',
      message: 'raw leak',
      requestId: null,
      retryable: false,
      details: {},
    }).includes('raw leak'),
    false,
  )
})

test('a moved snapshot invalidates the open Evidence instead of rendering stale detail', async () => {
  let calls = 0
  const client = new FakeControlPlaneClient(request => {
    calls += 1
    return evidenceGetResponse(request, evidenceDetailResult())
  })
  const { created, model } = viewModel({ client })
  await created.openEvidence(evidenceId(1))
  assert.equal(created.state.detail.status, 'ready')
  model.publish(modelState({ projection: projection({ evidence: [] }) }))
  assert.equal(created.state.detail, null)
  assert.equal(created.state.selected, null)
  assert.equal(calls, 1)
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
  #textContent = ''

  get textContent() {
    return this.#textContent
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
    for (const listener of this.listeners.get(name) ?? []) listener(values)
  }

  focus() {}

  blur() {}
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

function findAllByClass(node, className, matches = []) {
  if (node.className === className) matches.push(node)
  for (const child of node.children) findAllByClass(child, className, matches)
  return matches
}

function deepLink() {
  const state = { hash: '#/strongflow?delivery=' + deliveryId, replaced: [] }
  const link = {
    get route() {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      const tab = parameters.get('tab')
      return {
        tab: tab === 'tests' || tab === 'logs' ? tab : 'evidence',
        evidenceId: parameters.get('evidence'),
      }
    },
    onRouteChange: route => {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      parameters.set('tab', route.tab)
      if (route.evidenceId === null) parameters.delete('evidence')
      else parameters.set('evidence', route.evidenceId)
      state.hash = `#/strongflow?${parameters.toString()}`
      state.replaced.push(state.hash)
    },
    state,
  }
  return link
}

function mountedWorkbench(overrides = {}) {
  const document = new FakeDocument()
  const rootElement = document.createElement('section')
  const model = overrides.model ?? new FakeStrongFlowModel()
  const client = overrides.client ?? new FakeControlPlaneClient(request => evidenceGetResponse(
    request,
    evidenceDetailResult(),
  ))
  const link = overrides.deepLink ?? deepLink()
  const mounted = mountStrongFlowEvidence({
    root: rootElement,
    model,
    client,
    actor,
    scope,
    nextRequestId,
    route: link.route,
    onRouteChange: link.onRouteChange,
    ...(overrides.limits === undefined ? {} : { limits: overrides.limits }),
    ...(overrides.downloader === undefined ? {} : { downloader: overrides.downloader }),
  })
  return { document, rootElement, model, client, link, mounted }
}

test('the workbench renders tab navigation, bounded rows, and candidate binding state', () => {
  const { rootElement } = mountedWorkbench({ limits: { evidence: 2 } })
  const tablist = findByClass(rootElement, 'wwc-strongflow-evidence-tabs')
  assert.equal(tablist.getAttribute('role'), 'tablist')
  const tabs = tablist.children
  assert.deepEqual(tabs.map(tab => tab.textContent), ['Evidence', 'Tests', 'Logs'])
  assert.deepEqual(tabs.map(tab => tab.getAttribute('aria-selected')), ['true', 'false', 'false'])
  const rows = findAllByClass(rootElement, 'wwc-strongflow-evidence-row')
  assert.equal(rows.length, 2)
  const omitted = findAllByClass(rootElement, 'wwc-strongflow-omitted')
  assert.equal(omitted.length > 0, true)
  assert.equal(omitted.some(node => /3 more evidence records/u.test(node.textContent)), true)
})

test('tabs own real panels and the keyed Drawer keeps one detail node with accessible state', async () => {
  const client = new FakeControlPlaneClient(request => evidenceGetResponse(
    request,
    evidenceDetailResult({ evidence: { ...evidenceRow(1), stageRunId: 'run_foreign' } }),
  ))
  const { rootElement, mounted } = mountedWorkbench({ client })
  const tabs = findByClass(rootElement, 'wwc-strongflow-evidence-tabs').children
  const panels = findAllByClass(rootElement, 'wwc-strongflow-evidence-panel')
  assert.equal(panels.length, 3)
  assert.deepEqual(panels.map(panel => panel.getAttribute('role')), ['tabpanel', 'tabpanel', 'tabpanel'])
  assert.deepEqual(tabs.map(tab => tab.getAttribute('aria-controls')), panels.map(panel => panel.id))
  assert.equal(panels.filter(panel => !panel.hidden).length, 1)
  const row = findAllByClass(rootElement, 'wwc-strongflow-evidence-row')[0]
  row.emit('click')
  const detailBefore = findByClass(rootElement, 'wwc-strongflow-evidence-detail')
  assert.equal(detailBefore.getAttribute('aria-busy'), 'true')
  await new Promise(resolve => { setImmediate(resolve) })
  const detailAfter = findByClass(rootElement, 'wwc-strongflow-evidence-detail')
  assert.equal(detailAfter, detailBefore)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-evidence-error').getAttribute('role'), 'alert')
  mounted.close()
})

test('workbench shows bounded Verdict failures, criterion joins, and every Artifact selector', async () => {
  const descriptorA = artifactDescriptor({ artifactId: 'artifact:a' })
  const descriptorB = artifactDescriptor({ artifactId: 'artifact:b', digest: `sha256:${'e'.repeat(64)}` })
  const currentProjection = projection({
    verdict: {
      criteria: [{
        criterionId: 'criterion:failure',
        resultId: 'result:1',
        verdict: 'fail',
        evidenceRefs: [evidenceId(2)],
        explanation: 'failed exact check',
        evaluatedAt: '2026-09-02T02:00:00.000Z',
      }],
    },
  })
  const client = new FakeControlPlaneClient(request => {
    if (request.query === QueryName.EvidenceGet) return evidenceGetResponse(request, evidenceDetailResult({
      evidence: { ...evidenceRow(2), type: 'command' },
      artifactAccess: { state: 'available', items: [descriptorA, descriptorB] },
    }))
    return artifactChunkResponse(request, {
      artifact: request.parameters.artifactId === descriptorA.artifactId ? descriptorA : descriptorB,
    })
  })
  const { rootElement, mounted } = mountedWorkbench({
    client,
    model: new FakeStrongFlowModel(modelState({ projection: currentProjection })),
  })
  assert.match(findByClass(rootElement, 'wwc-strongflow-evidence-summary-counts').textContent, /1 failed/u)
  const row = findAllByClass(rootElement, 'wwc-strongflow-evidence-row')
    .find(candidate => candidate.dataset.evidenceId === evidenceId(2))
  assert.match(findByClass(row, 'wwc-strongflow-evidence-criteria').textContent, /criterion:failure/u)
  row.emit('click')
  await new Promise(resolve => { setImmediate(resolve) })
  assert.equal(findAllByClass(rootElement, 'wwc-strongflow-evidence-artifact-select').length, 2)
  mounted.close()
})

test('tab selection moves with the keyboard, filters rows, and writes a bounded deep link', () => {
  const { rootElement, link, mounted } = mountedWorkbench()
  const tabs = findByClass(rootElement, 'wwc-strongflow-evidence-tabs').children
  tabs[0].emit('keydown', { key: 'ArrowRight', preventDefault() {} })
  assert.deepEqual(
    findAllByClass(rootElement, 'wwc-strongflow-evidence-row').map(row => row.dataset.evidenceType),
    ['test'],
  )
  assert.equal(tabs[1].getAttribute('aria-selected'), 'true')
  assert.equal(link.state.hash.includes('tab=tests'), true)
  tabs[1].emit('keydown', { key: 'ArrowRight', preventDefault() {} })
  assert.deepEqual(
    findAllByClass(rootElement, 'wwc-strongflow-evidence-row').map(row => row.dataset.evidenceType),
    ['command', 'runtime_event'],
  )
  assert.equal(link.state.hash.includes('tab=logs'), true)
  assert.equal(link.state.hash.includes(`delivery=${deliveryId}`), true)
  tabs[2].emit('keydown', { key: 'Home', preventDefault() {} })
  assert.equal(tabs[0].getAttribute('aria-selected'), 'true')
  mounted.close()
})

test('opening a row from the list opens the detail drawer with sanitized provenance and Escape closes', async () => {
  const { rootElement, client, link, mounted } = mountedWorkbench()
  const rows = findAllByClass(rootElement, 'wwc-strongflow-evidence-row')
  rows[0].emit('click')
  await new Promise(resolve => { setImmediate(resolve) })
  const drawer = findByClass(rootElement, 'wwc-strongflow-evidence-drawer')
  assert.equal(drawer.getAttribute('role'), 'dialog')
  const detail = findByClass(rootElement, 'wwc-strongflow-evidence-detail')
  assert.equal(detail.dataset.status, 'ready')
  assert.equal(detail.dataset.evidenceId, evidenceId(1))
  const outcome = findByClass(rootElement, 'wwc-strongflow-evidence-detail-outcome')
  assert.equal(findByClass(outcome, 'wwc-status-badge-label').textContent, 'Passed')
  const outcomeIcon = findByClass(outcome, 'wwc-status-badge-icon')
  assert.notEqual(outcomeIcon, null)
  assert.equal(outcomeIcon.getAttribute('aria-hidden'), 'true')
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-evidence-detail-candidate').textContent,
    /current candidate/u,
  )
  assert.equal(link.state.hash.includes(`evidence=${evidenceId(1)}`), true)
  const artifact = findByClass(rootElement, 'wwc-strongflow-evidence-artifact')
  assert.match(artifact.textContent, /not available/u)
  assert.equal(client.queries.length, 1)
  findByClass(rootElement, 'wwc-drawer-close').emit('click')
  assert.equal(findByClass(rootElement, 'wwc-strongflow-evidence-detail').hidden, true)
  assert.equal(link.state.hash.includes('evidence='), false)
  mounted.close()
})

test('superseded candidates are labeled clearly in rows and detail', async () => {
  const client = new FakeControlPlaneClient(request => evidenceGetResponse(
    request,
    evidenceDetailResult({ evidence: { ...evidenceRow(4), type: 'diff', candidateRef: supersededCandidateRef } }),
  ))
  const { rootElement, mounted } = mountedWorkbench({ client })
  const rows = findAllByClass(rootElement, 'wwc-strongflow-evidence-row')
  const superseded = rows.find(row => row.dataset.evidenceId === evidenceId(4))
  assert.equal(superseded.dataset.candidateState, 'superseded')
  superseded.emit('click')
  await new Promise(resolve => { setImmediate(resolve) })
  const detail = findByClass(rootElement, 'wwc-strongflow-evidence-detail')
  assert.equal(detail.dataset.candidateState, 'superseded')
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-evidence-detail-candidate').textContent,
    /superseded candidate/u,
  )
  mounted.close()
})

test('log search filters only the loaded text and never claims the whole file', async () => {
  const descriptor = artifactDescriptor({ sizeBytes: 512 * 1024 })
  const client = new FakeControlPlaneClient((request, count) => {
    if (request.query === QueryName.EvidenceGet) {
      return evidenceGetResponse(request, evidenceDetailResult({
        evidence: { ...evidenceRow(2), type: 'command' },
        artifactAccess: { state: 'available', items: [descriptor] },
      }))
    }
    return artifactChunkResponse(request, {
      artifact: descriptor,
      dataBase64: Buffer.from(
        'log line one\nlog line two\n'.padEnd(request.parameters.length, 'x'),
      ).toString('base64'),
      offset: request.parameters.offset,
      nextOffset: count === 2 ? 256 * 1024 : null,
      returnedBytes: request.parameters.length,
      totalBytes: descriptor.sizeBytes,
    })
  })
  const { rootElement, mounted } = mountedWorkbench({ client })
  const logsTab = findByClass(rootElement, 'wwc-strongflow-evidence-tabs').children[2]
  logsTab.emit('click')
  const rows = findAllByClass(rootElement, 'wwc-strongflow-evidence-row')
  rows.find(row => row.dataset.evidenceId === evidenceId(2)).emit('click')
  await new Promise(resolve => { setImmediate(resolve) })
  findByClass(rootElement, 'wwc-strongflow-evidence-load-more').emit('click')
  await new Promise(resolve => { setImmediate(resolve) })
  const search = findByClass(rootElement, 'wwc-strongflow-evidence-search')
  search.value = 'line two'
  search.emit('input', {})
  const contentText = findByClass(rootElement, 'wwc-strongflow-evidence-content-text')
  assert.match(contentText.textContent, /log line two/u)
  assert.equal(contentText.textContent.includes('log line one'), false)
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-evidence-content-scope').textContent,
    /512 KiB of 512 KiB/u,
  )
  search.value = ''
  search.emit('input', {})
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-evidence-content-text').textContent,
    /log line one/u,
  )
  mounted.close()
})

test('a deep link opens the requested tab and Evidence without extra clicks', async () => {
  const link = deepLink()
  link.state.hash = `#/strongflow?delivery=${deliveryId}&tab=tests&evidence=${evidenceId(1)}`
  const { rootElement, mounted } = mountedWorkbench({ deepLink: link })
  const tabs = findByClass(rootElement, 'wwc-strongflow-evidence-tabs').children
  assert.equal(tabs[1].getAttribute('aria-selected'), 'true')
  await new Promise(resolve => { setImmediate(resolve) })
  assert.notEqual(findByClass(rootElement, 'wwc-strongflow-evidence-detail'), null)
  mounted.close()
})

test('evidence sources keep the page boundary and never open transports or render unsafe HTML', () => {
  const pageSource = readFileSync(
    resolve(root, 'apps/client/src/strongflow-page.ts'),
    'utf8',
  )
  const evidenceSource = readFileSync(
    resolve(root, 'apps/client/src/strongflow-evidence.ts'),
    'utf8',
  )
  assert.doesNotMatch(pageSource, /\.query\s*\(|\.command\s*\(/u)
  assert.match(evidenceSource, /QueryName\.EvidenceGet/u)
  assert.match(evidenceSource, /QueryName\.EvidenceArtifactContentGet/u)
  assert.doesNotMatch(evidenceSource, /\bfetch\s*\(|new\s+WebSocket|innerHTML/iu)
})
