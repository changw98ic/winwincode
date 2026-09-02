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
    'apps/client/tsconfig.strongflow-workflow-tests.json',
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
  `StrongFlow workflow integration did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cacheRoot = resolve(root, '.cache/strongflow-workflow-tests')
const facade = await import(pathToFileURL(resolve(cacheRoot, 'control-plane-client.js')).href)
const strongflow = await import(pathToFileURL(resolve(cacheRoot, 'strongflow-view-model.js')).href)
const page = await import(pathToFileURL(resolve(cacheRoot, 'strongflow-page.js')).href)
const { createControlPlaneClient } = facade
const { createStrongFlowViewModel } = strongflow
const { strongFlowPagePresentation } = page

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const deliveryId = 'dlv_00000000000000000000000001'
const productSessionId = 'psn_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const reviewStageRunId = 'run_00000000000000000000000002'
const taskId = 'dtk_00000000000000000000000001'
const reviewAttentionId = 'att_00000000000000000000000001'
const reworkAttentionId = 'att_00000000000000000000000002'
const finalAttentionId = 'att_00000000000000000000000003'
const subscriptionId = 'sub_00000000000000000000000001'
const candidateDigest1 = `sha256:${'a'.repeat(64)}`
const candidateDigest2 = `sha256:${'b'.repeat(64)}`
const candidateDiffDigest1 = `sha256:${'c'.repeat(64)}`
const candidateDiffDigest2 = `sha256:${'d'.repeat(64)}`

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function eventId(value) {
  return canonicalId('evt', value)
}

function eventCursor(sequence) {
  return {
    scope,
    stream: { kind: 'delivery', deliveryId },
    sequence,
    eventId: sequence === 0 ? null : eventId(sequence),
  }
}

function pageInfo() {
  return { hasMore: false, nextCursor: null }
}

function sessionBinding() {
  const workerSessionId = 'wsn_00000000000000000000000001'
  const codexThreadId = 'cdx_00000000000000000000000001'
  const leaseId = 'lse_00000000000000000000000001'
  const workerId = 'wrk_00000000000000000000000001'
  return {
    bindingId: 'binding:strongflow:1',
    boundAt: '2026-08-27T01:00:00.000Z',
    executionJobId: 'job_00000000000000000000000001',
    productSessionId,
    stageRunId,
    workerSessionId,
    codexThreadId,
    attempt: 1,
    fencingToken: '1',
    leaseId,
    workerId,
    sourceIdentity: {
      kind: 'execution-worker',
      leaseId,
      workerId,
      workerInstanceId: 'wki_00000000000000000000000001',
      workerSessionId,
    },
    sessionIdentity: { productSessionId, workerSessionId, codexThreadId, stageRunId },
  }
}

function diagram(id, kind) {
  return {
    id,
    kind,
    title: `${kind} diagram`,
    nodes: [{
      id: 'node:1',
      label: 'Current node',
      description: 'The exact node selected for bounded rework.',
      kind: kind === 'system-architecture' ? 'component' : 'stage',
      trustBoundary: null,
      unresolved: false,
    }],
    edges: [],
  }
}

function review(state) {
  const common = {
    deliveryId,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 1,
    planningStageRunId: stageRunId,
    planningSessionBindingId: 'binding:strongflow:1',
    reviewStageRunId,
    attentionItemId: reviewAttentionId,
    reviewSetSha256: state.reviewDigest,
    solutionId: 'solution:1',
    summary: 'Use the canonical Control Plane workflow.',
    approach: ['Keep every decision on the selected Delivery revision.'],
    components: [{
      id: 'node:1',
      label: 'Current node',
      responsibility: 'Own the bounded rework target.',
      kind: 'component',
      trustBoundary: null,
      unresolved: false,
      repositoryPathPrefixes: ['apps/client'],
    }],
    connections: [],
    architectureDiagram: diagram('diagram:architecture', 'system-architecture'),
    processDiagram: diagram('diagram:process', 'process-flow'),
    risks: [],
    unresolvedItems: [],
    taskProposals: [{
      id: taskId,
      title: 'Complete the current candidate',
      goal: 'Produce evidence for the exact reviewed change.',
      blockedByTaskIds: [],
      acceptanceCriterionIds: ['criterion:1'],
    }],
  }
  if (state.reviewStatus === 'pending') return {
    ...common,
    reviewStatus: 'pending',
    decision: null,
    comments: null,
    requestedChanges: null,
    reviewerId: null,
    reviewedAt: null,
  }
  if (state.reviewStatus === 'changes_requested') return {
    ...common,
    reviewStatus: 'changes_requested',
    decision: 'request_changes',
    comments: state.reviewComments,
    requestedChanges: state.requestedChanges,
    reviewerId: actor.id,
    reviewedAt: '2026-08-27T01:00:02.000Z',
  }
  return {
    ...common,
    reviewStatus: 'approved',
    decision: 'approve',
    comments: state.reviewComments,
    requestedChanges: null,
    reviewerId: actor.id,
    reviewedAt: '2026-08-27T01:00:03.000Z',
  }
}

function attention(id, type, title, status, revision) {
  const resolved = status !== 'open'
  return {
    id,
    deliverySpecId: 'spec:1',
    stageRunId: type === 'decision_required' ? reviewStageRunId : stageRunId,
    type,
    title,
    options: [],
    blocking: status === 'open',
    status,
    assignedTo: actor.id,
    createdAt: '2026-08-27T01:00:01.000Z',
    resolvedAt: resolved ? `2026-08-27T01:00:${String(10 + revision).padStart(2, '0')}.000Z` : null,
    resolvedBy: resolved ? actor.id : null,
    resolutionSummary: resolved ? 'Resolved through the current Control Plane decision.' : null,
  }
}

function candidate(digest) {
  const suffix = digest === candidateDigest1 ? '1' : '2'
  const diffSha256 = digest === candidateDigest1 ? candidateDiffDigest1 : candidateDiffDigest2
  return {
    candidateCommitId: suffix.repeat(40),
    candidateTreeId: suffix.repeat(40),
    candidateRef: `git-candidate:${digest}`,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 1,
    diffSha256,
    frozenAt: `2026-08-27T01:00:0${suffix}.000Z`,
    producerSessionBindingId: 'binding:strongflow:1',
    producerStageRunId: stageRunId,
  }
}

function criterionVerdict(status, candidateRef) {
  return {
    id: status === 'pass' ? 'verdict:pass' : 'verdict:fail',
    candidateRef,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 1,
    producedAt: '2026-08-27T01:00:20.000Z',
    status,
    criteria: [{
      criterionId: 'criterion:1',
      evaluatedAt: '2026-08-27T01:00:20.000Z',
      evidenceRefs: ['evd_00000000000000000000000001'],
      explanation: status === 'pass' ? 'The candidate passed.' : 'The candidate needs rework.',
      resultId: status === 'pass' ? 'result:pass' : 'result:fail',
      verdict: status,
    }],
    unresolvedFindings: status === 'pass' ? [] : ['Update node:1.'],
  }
}

function deliveryDetail(state) {
  const readCursor = {
    token: `cursor_${String(state.revision).padStart(32, '0')}`,
    scope,
    deliveryId,
    deliveryRevision: state.revision,
    runtimeLedgerRevision: state.revision + 100,
    runtimeAcceptedSequence: state.revision + 200,
    publicationRevision: 0,
    eventCursor: eventCursor(state.eventSequence),
  }
  const attentionItems = [attention(
    reviewAttentionId,
    'decision_required',
    'Review the current solution',
    state.reviewAttentionStatus,
    state.revision,
  )]
  if (state.reworkAttentionStatus !== null) attentionItems.push(attention(
    reworkAttentionId,
    'verification_blocked',
    'Approve bounded rework',
    state.reworkAttentionStatus,
    state.revision,
  ))
  if (state.finalAttentionStatus !== null) attentionItems.push(attention(
    finalAttentionId,
    'delivery_approval',
    'Approve the final Delivery',
    state.finalAttentionStatus,
    state.revision,
  ))
  const currentCandidate = state.candidateDigest === null ? null : candidate(state.candidateDigest)
  return {
    kind: 'delivery_detail',
    schemaVersion,
    deliveryId,
    deliveryRevision: state.revision,
    readCursor,
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    status: state.status,
    requirements: {
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 1,
      title: 'StrongFlow workflow contract',
      goal: 'Keep decisions bound to the current Delivery and candidate.',
      scope: ['apps/client'],
      outOfScope: [],
      constraints: [],
      sourceProductSessionId: null,
      acceptanceCriteria: [{
        id: 'criterion:1',
        description: 'All decisions retain current identities.',
        verificationMethod: 'Browser contract fake',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: { kind: 'local-git', locator: 'workspace://repository' },
      baseRevision: '0123456789abcdef0123456789abcdef01234567',
      maxReworkAttempts: 2,
    },
    solutionReview: review(state),
    stages: [{
      id: stageRunId,
      actorType: 'codex',
      attempt: 1,
      deliveryTaskId: state.tasksApproved ? taskId : null,
      finishedAt: null,
      role: 'implementer',
      sessionBinding: sessionBinding(),
      stage: state.status === 'reworking' ? 'reworking' : 'executing',
      startedAt: '2026-08-27T01:00:00.000Z',
      status: state.tasksApproved ? 'succeeded' : 'running',
    }, {
      id: reviewStageRunId,
      actorType: 'human',
      attempt: 1,
      deliveryTaskId: null,
      finishedAt: state.reviewStatus === 'pending' ? null : '2026-08-27T01:00:03.000Z',
      role: 'reviewer',
      sessionBinding: null,
      stage: 'plan-review',
      startedAt: '2026-08-27T01:00:01.000Z',
      status: state.reviewStatus === 'pending' ? 'waiting' : 'succeeded',
    }],
    tasks: state.tasksApproved ? [{
      id: taskId,
      title: 'Complete the current candidate',
      goal: 'Produce evidence for the exact reviewed change.',
      owner: null,
      status: state.status === 'reworking' ? 'active' : 'verifying',
      blockedByTaskIds: [],
      acceptanceCriterionIds: ['criterion:1'],
      stageRunIds: [stageRunId],
      evidenceRefs: state.verdictStatus === null ? [] : ['evd_00000000000000000000000001'],
    }] : [],
    attention: attentionItems,
    evidence: state.verdictStatus === null || currentCandidate === null ? [] : [{
      id: 'evd_00000000000000000000000001',
      candidateRef: currentCandidate.candidateRef,
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 1,
      sessionBindingId: 'binding:strongflow:1',
      sourceRef: 'artifact:test:1',
      stageRunId,
      type: 'test',
      createdAt: '2026-08-27T01:00:19.000Z',
    }],
    currentCandidate,
    verdict: state.verdictStatus === null || currentCandidate === null
      ? null
      : criterionVerdict(state.verdictStatus, currentCandidate.candidateRef),
    publication: null,
  }
}

function runtimeProjection(detail) {
  return {
    kind: 'runtime_projection',
    productSessionId,
    deliveryId,
    stageRunId,
    readCursor: detail.readCursor,
    eventCursor: detail.readCursor.eventCursor,
    lastProjectionSequence: detail.readCursor.runtimeAcceptedSequence,
    revision: detail.readCursor.runtimeLedgerRevision,
    rebuiltAt: '2026-08-27T01:00:21.000Z',
    sessions: [],
  }
}

function response(status, payload) {
  return {
    ok: status >= 200 && status < 300,
    status,
    async text() { return JSON.stringify(payload) },
  }
}

function queryResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: pageInfo(),
  }
}

function deliverySummary(state) {
  const detail = deliveryDetail(state)
  return {
    schemaVersion,
    deliveryId,
    revision: state.revision,
    status: state.status,
    title: detail.requirements.title,
    updatedAt: '2026-08-27T01:00:21.000Z',
    ownership: detail.ownership,
    activeStageRunId: stageRunId,
    openAttentionCount: detail.attention.filter(item => item.status === 'open').length,
    taskCounts: {
      total: detail.tasks.length,
      pending: detail.tasks.filter(item => item.status === 'pending').length,
      active: detail.tasks.filter(item => item.status === 'active').length,
      blocked: detail.tasks.filter(item => item.status === 'blocked').length,
      verifying: detail.tasks.filter(item => item.status === 'verifying').length,
      completed: detail.tasks.filter(item => item.status === 'completed').length,
      failed: detail.tasks.filter(item => item.status === 'failed').length,
    },
  }
}

function commandResponse(request, state) {
  return {
    schemaVersion,
    requestId: request.requestId,
    command: request.command,
    outcome: 'completed',
    previousRevision: request.expectedRevision,
    currentRevision: state.revision,
    result: deliverySummary(state),
  }
}

function terminalError(request, code, message) {
  return {
    schemaVersion,
    requestId: request.requestId,
    error: { code, message, retryable: false, details: {} },
  }
}

function transportLimits() {
  return {
    maxUnackedEvents: 256,
    hardUnackedEvents: 1024,
    ackDeadlineMillis: 30_000,
    backpressureCloseCode: 4408,
  }
}

class FakeWebSocket {
  readyState = 0
  onopen = null
  onmessage = null
  onclose = null
  onerror = null
  sent = []

  send(source) {
    assert.equal(this.readyState, 1)
    this.sent.push(JSON.parse(source))
  }

  close() { this.readyState = 3 }
  open() { this.readyState = 1; this.onopen?.({}) }
  receive(frame) { this.onmessage?.({ data: JSON.stringify(frame) }) }
  serverClose(code) { this.readyState = 3; this.onclose?.({ code }) }
}

function sockets() {
  const values = []
  return {
    values,
    createSocket() {
      const socket = new FakeWebSocket()
      values.push(socket)
      return socket
    },
  }
}

function acceptedFrame(cursor) {
  return {
    type: 'transport.subscription-accepted.v1',
    subscriptionId,
    cursor,
    authorizationEpoch: 1,
    limits: transportLimits(),
  }
}

function resumedFrame(after) {
  return {
    type: 'transport.resume-accepted.v1',
    subscriptionId,
    after,
    replayThrough: after,
    authorizationEpoch: 1,
  }
}

function deliveryEvent(sequence, revision) {
  return {
    type: 'event.v1',
    subscriptionId,
    eventId: eventId(sequence),
    scope,
    stream: { kind: 'delivery', deliveryId },
    sequence,
    occurredAt: '2026-08-27T01:00:22.000Z',
    authorizationEpoch: 1,
    source: { kind: 'control-plane', component: 'strongflow-contract-fake', actor },
    event: {
      type: 'delivery.changed.v1',
      deliveryId,
      revision,
      changeKind: 'reworked',
    },
  }
}

function contractFake() {
  const socketFactory = sockets()
  const requests = []
  const state = {
    revision: 1,
    eventSequence: 0,
    status: 'plan-review',
    reviewStatus: 'pending',
    reviewDigest: `sha256:${'1'.repeat(64)}`,
    reviewComments: null,
    requestedChanges: null,
    reviewAttentionStatus: 'open',
    tasksApproved: false,
    candidateDigest: null,
    verdictStatus: null,
    reworkAttentionStatus: null,
    finalAttentionStatus: null,
  }
  let verdictCount = 0

  function advanceRevision() {
    state.revision += 1
  }

  async function fetch(input, init) {
    const request = JSON.parse(init.body)
    requests.push({ input, request, init })
    if (input.endsWith('/api/v1/queries')) {
      const detail = deliveryDetail(state)
      if (request.query === 'delivery.get') {
        return response(200, queryResponse(request, detail))
      }
      if (request.query === 'runtime.projection.get') {
        assert.deepEqual(request.parameters.atCursor, detail.readCursor)
        return response(200, queryResponse(request, runtimeProjection(detail)))
      }
    }
    if (request.expectedRevision !== state.revision) {
      return response(409, terminalError(request, 'REVISION_CONFLICT', 'Delivery changed.'))
    }
    if (request.command === 'delivery.resolve_attention') {
      if (request.payload.attentionItemId === reviewAttentionId) {
        const decision = JSON.parse(request.payload.resolution)
        assert.equal(decision.schemaVersion, 1)
        assert.equal(decision.protocol, 'winwincode.solution-review-decision.v1')
        assert.equal(decision.deliveryId, deliveryId)
        assert.equal(decision.deliverySpecRevision, 1)
        assert.equal(decision.reviewStageRunId, reviewStageRunId)
        assert.equal(decision.attentionItemId, reviewAttentionId)
        assert.equal(decision.reviewSetSha256, state.reviewDigest.replace(/^sha256:/u, ''))
        assert.equal(request.payload.remediation, null)
        state.reviewStatus = decision.action === 'request_changes'
          ? 'changes_requested'
          : decision.action === 'approve' ? 'approved' : 'rejected'
        state.reviewComments = decision.comments
        state.requestedChanges = decision.action === 'request_changes'
          ? decision.requestedChanges
          : null
        if (decision.action === 'request_changes') {
          assert.ok(Array.isArray(decision.requestedChanges))
          assert.ok(decision.requestedChanges.length > 0)
        } else {
          assert.equal(decision.requestedChanges, null)
        }
        state.reviewAttentionStatus = 'resolved'
        state.status = decision.action === 'approve' ? 'ready' : 'planning'
      } else if (request.payload.attentionItemId === reworkAttentionId) {
        const instructions = JSON.parse(request.payload.remediation.instructions)
        assert.equal(
          request.payload.remediation.candidateDigest,
          candidate(state.candidateDigest).diffSha256,
        )
        assert.equal(request.payload.remediation.deliveryTaskId, taskId)
        assert.equal(instructions.candidateDigest, candidate(state.candidateDigest).diffSha256)
        assert.equal(instructions.deliveryTaskId, taskId)
        assert.equal(instructions.nodeId, 'node:1')
        assert.equal(instructions.instructions, 'Fix only the reviewed node.')
        state.reworkAttentionStatus = 'resolved'
        state.status = 'reworking'
      } else if (request.payload.attentionItemId === finalAttentionId) {
        assert.equal(request.payload.remediation, null)
        state.finalAttentionStatus = 'resolved'
      } else {
        return response(409, terminalError(request, 'WRONG_STATE', 'Attention is stale.'))
      }
      advanceRevision()
      return response(200, commandResponse(request, state))
    }
    if (request.command === 'delivery.approve_task_breakdown') {
      assert.equal(state.reviewStatus, 'approved')
      assert.equal(request.payload.reviewSetSha256, state.reviewDigest)
      state.tasksApproved = true
      state.candidateDigest = candidateDigest1
      state.status = 'verifying'
      advanceRevision()
      return response(200, commandResponse(request, state))
    }
    if (request.command === 'delivery.submit_verdict') {
      assert.equal(request.payload.candidateDigest, state.candidateDigest)
      verdictCount += 1
      state.verdictStatus = verdictCount === 1 ? 'fail' : 'pass'
      if (state.verdictStatus === 'fail') {
        state.status = 'needs-attention'
        state.reworkAttentionStatus = 'open'
      } else {
        state.status = 'ready-to-deliver'
        state.finalAttentionStatus = 'open'
      }
      advanceRevision()
      return response(200, commandResponse(request, state))
    }
    if (request.command === 'delivery.advance') {
      assert.equal(state.status, 'ready-to-deliver')
      assert.equal(state.finalAttentionStatus, 'resolved')
      state.status = 'delivered'
      advanceRevision()
      return response(200, commandResponse(request, state))
    }
    return response(400, terminalError(request, 'INVALID_REQUEST', 'Unsupported fake request.'))
  }

  return {
    fetch,
    requests,
    socketFactory,
    state,
    prepareReplacementReview() {
      advanceRevision()
      state.status = 'plan-review'
      state.reviewStatus = 'pending'
      state.reviewDigest = `sha256:${'2'.repeat(64)}`
      state.reviewComments = null
      state.requestedChanges = null
      state.reviewAttentionStatus = 'open'
    },
    createUnannouncedRevision() { advanceRevision() },
    finishRework() {
      advanceRevision()
      state.eventSequence += 1
      state.status = 'verifying'
      state.candidateDigest = candidateDigest2
      state.verdictStatus = null
      state.reworkAttentionStatus = 'resolved'
    },
  }
}

async function flush() {
  await new Promise(resolvePromise => setTimeout(resolvePromise, 2))
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  await new Promise(resolvePromise => setImmediate(resolvePromise))
}

test('StrongFlow browser contract fake covers review, stale decisions, rework, final review, and cursor recovery', async () => {
  const fake = contractFake()
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/root',
    maxNetworkRetries: 0,
    reconnectDelayMillis: 0,
    transport: { fetch: fake.fetch, createSocket: fake.socketFactory.createSocket },
  })
  let requestSequence = 0
  const model = createStrongFlowViewModel({
    client,
    actor,
    scope,
    deliveryId,
    productSessionId,
    stageRunId,
    subscriptionId,
    nextRequestId() {
      requestSequence += 1
      return requestId(requestSequence)
    },
  })

  await model.start()
  assert.ok(model.state.projection, JSON.stringify({
    status: model.state.status,
    code: model.state.error?.code,
    message: model.state.error?.message,
    details: model.state.error?.details,
    cause: String(model.state.error?.cause?.stack ?? model.state.error?.cause ?? ''),
  }))
  assert.equal(model.state.projection.solutionReview.reviewStatus, 'pending')
  const firstSocket = fake.socketFactory.values[0]
  firstSocket.open()
  const subscribe = firstSocket.sent[0]
  assert.equal(subscribe.type, 'transport.subscribe.v1')
  assert.deepEqual(subscribe.startAt, eventCursor(0))
  firstSocket.receive(acceptedFrame(subscribe.startAt))

  await model.decideSolutionReview({
    action: 'request_changes',
    comments: 'Return this exact review set.',
    requestedChanges: ['Clarify the current node.'],
  })
  let command = fake.requests.at(-1).request
  assert.equal(command.command, 'delivery.resolve_attention')
  assert.equal(command.expectedRevision, 1)
  assert.match(command.requestId, /^req_/u)
  assert.equal(command.payload.resolution, JSON.stringify({
    schemaVersion: 1,
    protocol: 'winwincode.solution-review-decision.v1',
    deliveryId,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 1,
    reviewStageRunId,
    attentionItemId: reviewAttentionId,
    reviewSetSha256: '1'.repeat(64),
    action: 'request_changes',
    comments: 'Return this exact review set.',
    requestedChanges: ['Clarify the current node.'],
  }))
  assert.deepEqual(Object.keys(JSON.parse(command.payload.resolution)), [
    'schemaVersion',
    'protocol',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'reviewStageRunId',
    'attentionItemId',
    'reviewSetSha256',
    'action',
    'comments',
    'requestedChanges',
  ])
  await model.refresh()
  assert.equal(model.state.projection.solutionReview.reviewStatus, 'changes_requested')
  assert.deepEqual(model.state.projection.solutionReview.requestedChanges, [
    'Clarify the current node.',
  ])

  fake.prepareReplacementReview()
  await model.refresh()
  assert.equal(model.state.projection.metadata.revisions.delivery, 3)
  fake.createUnannouncedRevision()
  await model.decideSolutionReview({ action: 'approve', comments: 'Approve.', requestedChanges: [] })
  assert.equal(model.state.interaction.error.code, 'REVISION_CONFLICT')
  assert.match(strongFlowPagePresentation(model.state).errorText, /Delivery changed/u)
  assert.equal(model.state.projection.metadata.revisions.delivery, 3)

  await model.refresh()
  assert.equal(model.state.projection.metadata.revisions.delivery, 4)
  await model.decideSolutionReview({ action: 'approve', comments: 'Approve.', requestedChanges: [] })
  command = fake.requests.at(-1).request
  assert.equal(command.expectedRevision, 4)
  assert.equal(command.payload.resolution, JSON.stringify({
    schemaVersion: 1,
    protocol: 'winwincode.solution-review-decision.v1',
    deliveryId,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 1,
    reviewStageRunId,
    attentionItemId: reviewAttentionId,
    reviewSetSha256: '2'.repeat(64),
    action: 'approve',
    comments: 'Approve.',
    requestedChanges: null,
  }))
  assert.deepEqual(Object.keys(JSON.parse(command.payload.resolution)), [
    'schemaVersion',
    'protocol',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'reviewStageRunId',
    'attentionItemId',
    'reviewSetSha256',
    'action',
    'comments',
    'requestedChanges',
  ])
  await model.refresh()
  assert.equal(model.state.projection.solutionReview.reviewStatus, 'approved')

  await model.approveTaskBreakdown()
  command = fake.requests.at(-1).request
  assert.equal(command.command, 'delivery.approve_task_breakdown')
  assert.equal(command.expectedRevision, 5)
  assert.equal(command.payload.reviewSetSha256, `sha256:${'2'.repeat(64)}`)
  await model.refresh()
  assert.equal(model.state.projection.delivery.tasks[0].id, taskId)
  assert.equal(model.state.projection.currentCandidate.diffSha256, candidateDiffDigest1)

  await model.submitVerdict()
  command = fake.requests.at(-1).request
  assert.equal(command.command, 'delivery.submit_verdict')
  assert.equal(command.payload.candidateDigest, candidateDigest1)
  await model.refresh()
  assert.equal(model.state.projection.verdict.status, 'fail')
  assert.equal(model.state.projection.attention.find(item => item.id === reworkAttentionId).status, 'open')

  await model.resolveAttention({
    attentionItemId: reworkAttentionId,
    decision: 'resolve',
    resolution: 'Approve one bounded rework.',
    remediation: {
      deliveryTaskId: taskId,
      nodeId: 'node:1',
      instructions: 'Fix only the reviewed node.',
    },
  })
  command = fake.requests.at(-1).request
  assert.equal(command.command, 'delivery.resolve_attention')
  assert.equal(command.expectedRevision, 7)
  assert.equal(command.payload.remediation.candidateDigest, candidateDiffDigest1)
  await model.refresh()
  assert.equal(model.state.projection.delivery.status, 'reworking')

  fake.finishRework()
  firstSocket.receive(deliveryEvent(1, fake.state.revision))
  await flush()
  assert.equal(model.state.projection.currentCandidate.diffSha256, candidateDiffDigest2)
  assert.equal(firstSocket.sent.at(-1).type, 'transport.ack.v1')
  assert.equal(firstSocket.sent.at(-1).cursor.sequence, 1)

  firstSocket.serverClose(1006)
  await flush()
  const resumedSocket = fake.socketFactory.values[1]
  resumedSocket.open()
  assert.equal(resumedSocket.sent[0].type, 'transport.resume.v1')
  assert.deepEqual(resumedSocket.sent[0].after, eventCursor(1))
  resumedSocket.receive(resumedFrame(resumedSocket.sent[0].after))

  await model.submitVerdict()
  command = fake.requests.at(-1).request
  assert.equal(command.payload.candidateDigest, candidateDigest2)
  assert.equal(command.expectedRevision, 9)
  await model.refresh()
  assert.equal(model.state.projection.verdict.status, 'pass')
  assert.equal(model.state.projection.delivery.status, 'ready-to-deliver')

  await model.resolveAttention({
    attentionItemId: finalAttentionId,
    decision: 'resolve',
    resolution: 'Approve the exact current candidate and verdict.',
    remediation: null,
  })
  command = fake.requests.at(-1).request
  assert.equal(command.expectedRevision, 10)
  await model.refresh()
  assert.equal(model.state.projection.attention.find(item => item.id === finalAttentionId).status, 'resolved')

  await model.advanceDelivery()
  command = fake.requests.at(-1).request
  assert.equal(command.command, 'delivery.advance')
  assert.equal(command.expectedRevision, 11)
  await model.refresh()
  assert.equal(model.state.projection.delivery.status, 'delivered')

  const commands = fake.requests.filter(({ request }) => 'command' in request)
  assert.equal(commands.every(({ request }) => request.requestId.startsWith('req_')), true)
  assert.equal(commands.every(({ request }) => Number.isInteger(request.expectedRevision)), true)
  assert.equal(commands.every(({ input }) => input.endsWith('/api/v1/commands')), true)

  model.close()
  client.close()
})
