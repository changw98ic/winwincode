import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test, { mock } from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.strongflow-tests.json',
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
  `StrongFlow view-model did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const module = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-view-model-tests/strongflow-view-model.js',
)).href}`)
const facade = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-view-model-tests/control-plane-client.js',
)).href}`)

const { createStrongFlowCreateViewModel, createStrongFlowViewModel } = module
const { ControlPlaneClientError } = facade
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
const nextProductSessionId = 'psn_00000000000000000000000002'
const nextStageRunId = 'run_00000000000000000000000003'
const subscriptionId = 'sub_00000000000000000000000001'

function requestId(value) {
  return `req_${String(value).padStart(26, '0')}`
}

function eventId(value) {
  return value === 0 ? null : `evt_${String(value).padStart(26, '0')}`
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function readCursor(revision = 1, eventSequence = revision) {
  return {
    token: `cursor_${String(revision).padStart(32, '0')}`,
    scope,
    deliveryId,
    deliveryRevision: revision,
    runtimeLedgerRevision: revision + 10,
    runtimeAcceptedSequence: revision + 20,
    publicationRevision: 1,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: eventSequence,
      eventId: eventId(eventSequence),
    },
  }
}

function sessionBinding(
  selectedProductSessionId = productSessionId,
  selectedStageRunId = stageRunId,
  suffix = '01',
) {
  return {
    bindingId: `binding:strongflow:${suffix}`,
    boundAt: '2026-08-27T01:00:00.000Z',
    executionJobId: `job_000000000000000000000000${suffix}`,
    productSessionId: selectedProductSessionId,
    stageRunId: selectedStageRunId,
    workerSessionId: `wsn_000000000000000000000000${suffix}`,
    codexThreadId: `cdx_000000000000000000000000${suffix}`,
    attempt: 1,
    fencingToken: '1',
    leaseId: `lse_000000000000000000000000${suffix}`,
    workerId: `wrk_000000000000000000000000${suffix}`,
    sourceIdentity: {
      kind: 'execution-worker',
      leaseId: `lse_000000000000000000000000${suffix}`,
      workerId: `wrk_000000000000000000000000${suffix}`,
      workerInstanceId: `wki_000000000000000000000000${suffix}`,
      workerSessionId: `wsn_000000000000000000000000${suffix}`,
    },
    sessionIdentity: {
      productSessionId: selectedProductSessionId,
      workerSessionId: `wsn_000000000000000000000000${suffix}`,
      codexThreadId: `cdx_000000000000000000000000${suffix}`,
      stageRunId: selectedStageRunId,
    },
  }
}

function diagram(id, kind) {
  return {
    id,
    kind,
    title: `${kind} diagram`,
    nodes: [{
      id: 'node:1',
      label: 'Control Plane',
      description: 'Owns the bounded projection.',
      kind: 'delivery-control',
      trustBoundary: null,
      unresolved: false,
    }],
    edges: [],
  }
}

function solutionReview() {
  return {
    deliveryId,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 3,
    planningStageRunId: stageRunId,
    planningSessionBindingId: 'binding:strongflow:1',
    reviewStageRunId: 'run_00000000000000000000000002',
    attentionItemId: 'att_00000000000000000000000001',
    reviewSetSha256: `sha256:${'1'.repeat(64)}`,
    reviewStatus: 'approved',
    decision: 'approve',
    comments: 'Approved as one bounded plan.',
    requestedChanges: null,
    reviewerId: actor.id,
    reviewedAt: '2026-08-27T01:00:02.000Z',
    solutionId: 'solution:1',
    summary: 'Use the canonical Control Plane projection.',
    approach: ['Load one exact Delivery and Runtime pair.'],
    components: [],
    connections: [],
    architectureDiagram: diagram('diagram:architecture', 'system-architecture'),
    processDiagram: diagram('diagram:process', 'process-flow'),
    risks: [],
    unresolvedItems: [],
    taskProposals: [{
      id: 'task:1',
      title: 'Build projection',
      goal: 'Keep the UI exact.',
      blockedByTaskIds: [],
      acceptanceCriterionIds: ['criterion:1'],
    }],
  }
}

function delivery(revision = 1, candidateRef = 'refs/winwincode/candidate/1') {
  const cursor = readCursor(revision)
  const verdict = {
    id: 'verdict:1',
    candidateRef,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 3,
    producedAt: '2026-08-27T01:00:05.000Z',
    status: 'pass',
    criteria: [{
      criterionId: 'criterion:1',
      evaluatedAt: '2026-08-27T01:00:05.000Z',
      evidenceRefs: ['evd_00000000000000000000000001'],
      explanation: 'The exact snapshot passed.',
      resultId: 'result:1',
      verdict: 'pass',
    }],
    unresolvedFindings: [],
  }
  return {
    kind: 'delivery_detail',
    schemaVersion,
    deliveryId,
    deliveryRevision: revision,
    readCursor: cursor,
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    status: 'ready-to-deliver',
    requirements: {
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      title: 'StrongFlow projection',
      goal: 'Show one current bounded read model.',
      scope: ['Client projection'],
      outOfScope: [],
      constraints: [],
      acceptanceCriteria: [{
        id: 'criterion:1',
        description: 'Snapshot stays current.',
        verificationMethod: 'Focused test',
        required: true,
      }],
      sourceRef: {
        kind: 'issue',
        provider: 'github',
        repository: 'winwincode/winwincode',
        number: 1,
      },
      publicationTarget: null,
      repository: { kind: 'local-git', locator: 'workspace://repository' },
      baseRevision: '0123456789abcdef0123456789abcdef01234567',
      maxReworkAttempts: 2,
    },
    solutionReview: solutionReview(),
    stages: [{
      id: stageRunId,
      actorType: 'codex',
      attempt: 1,
      deliveryTaskId: 'tsk_00000000000000000000000001',
      finishedAt: null,
      role: 'implementer',
      sessionBinding: sessionBinding(),
      stage: 'executing',
      startedAt: '2026-08-27T01:00:00.000Z',
      status: 'running',
    }],
    tasks: [{
      id: 'tsk_00000000000000000000000001',
      title: 'Build projection',
      goal: 'Keep the UI exact.',
      owner: null,
      status: 'active',
      blockedByTaskIds: [],
      acceptanceCriterionIds: ['criterion:1'],
      stageRunIds: [stageRunId],
      evidenceRefs: ['evd_00000000000000000000000001'],
    }],
    attention: [{
      id: 'att_00000000000000000000000001',
      deliverySpecId: 'spec:1',
      stageRunId,
      type: 'delivery_approval',
      title: 'Approve publication',
      options: [],
      blocking: false,
      status: 'resolved',
      assignedTo: actor.id,
      createdAt: '2026-08-27T01:00:01.000Z',
      resolvedAt: '2026-08-27T01:00:04.000Z',
      resolvedBy: actor.id,
      resolutionSummary: 'Approved.',
    }],
    evidence: [{
      id: 'evd_00000000000000000000000001',
      candidateRef,
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      sessionBindingId: 'binding:strongflow:1',
      sourceRef: 'artifact:test:1',
      stageRunId,
      type: 'test',
      createdAt: '2026-08-27T01:00:04.000Z',
    }],
    currentCandidate: {
      candidateCommitId: '1111111111111111111111111111111111111111',
      candidateTreeId: '2222222222222222222222222222222222222222',
      candidateRef,
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      diffSha256: `sha256:${'2'.repeat(64)}`,
      frozenAt: '2026-08-27T01:00:03.000Z',
      producerSessionBindingId: 'binding:strongflow:1',
      producerStageRunId: stageRunId,
    },
    verdict,
    publication: {
      id: 'pub_00000000000000000000000001',
      revision: 1,
      deliveryId,
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      candidateRef,
      deliveryVerdictId: verdict.id,
      verdictStatus: 'pass',
      approvalAttentionItemId: 'att_00000000000000000000000001',
      approvedBy: actor.id,
      approvedAt: '2026-08-27T01:00:04.000Z',
      publicationSetSha256: `sha256:${'3'.repeat(64)}`,
      target: {
        schemaVersion,
        scope,
        headRepository: 'winwincode/winwincode',
        headBranch: 'candidate',
        baseBranch: 'main',
      },
      state: 'pending',
      resourceRef: null,
      updatedAt: '2026-08-27T01:00:06.000Z',
    },
  }
}

function deliveryWithNextActiveStage(revision = 2) {
  const value = delivery(revision)
  value.stages[0] = {
    ...value.stages[0],
    finishedAt: '2026-08-27T01:00:10.000Z',
    status: 'succeeded',
  }
  value.stages.push({
    ...value.stages[0],
    id: nextStageRunId,
    finishedAt: null,
    role: 'reviewer',
    sessionBinding: sessionBinding(nextProductSessionId, nextStageRunId, '02'),
    stage: 'reviewing',
    startedAt: '2026-08-27T01:00:11.000Z',
    status: 'running',
  })
  value.tasks[0].stageRunIds.push(nextStageRunId)
  return value
}

function runtime(
  deliveryValue,
  selectedProductSessionId = productSessionId,
  selectedStageRunId = stageRunId,
) {
  const cursor = deliveryValue.readCursor
  return {
    kind: 'runtime_projection',
    productSessionId: selectedProductSessionId,
    deliveryId,
    stageRunId: selectedStageRunId,
    readCursor: cursor,
    eventCursor: cursor.eventCursor,
    lastProjectionSequence: cursor.runtimeAcceptedSequence,
    revision: cursor.runtimeLedgerRevision,
    rebuiltAt: '2026-08-27T01:00:05.000Z',
    sessions: [],
  }
}

function response(query, result) {
  return {
    schemaVersion,
    requestId: requestId(90),
    query,
    result,
    page: page(),
  }
}

class FakeClient {
  constructor() {
    const deliveryValue = delivery()
    this.responses = new Map([
      ['delivery.get', response('delivery.get', deliveryValue)],
      ['runtime.projection.get', response('runtime.projection.get', runtime(deliveryValue))],
    ])
  }

  calls = []
  queues = new Map()
  subscription = null
  subscriptionClosed = false
  reconnects = 0

  enqueue(query, value) {
    const queue = this.queues.get(query) ?? []
    queue.push(value)
    this.queues.set(query, queue)
  }

  async query(request) {
    this.calls.push(structuredClone(request))
    const queue = this.queues.get(request.query)
    const value = queue?.shift() ?? this.responses.get(request.query)
    if (value instanceof Error) throw value
    return structuredClone(value)
  }

  async command() {
    throw new Error('StrongFlow read model does not send commands.')
  }

  subscribe(options) {
    this.subscription = options
    this.subscriptionClosed = false
    return {
      cursor: null,
      resume() {},
      reconnect: () => { this.reconnects += 1 },
      close: () => { this.subscriptionClosed = true },
    }
  }

  close() {}
}

function view(client = new FakeClient(), options = {}) {
  let requestSequence = 0
  return {
    client,
    model: createStrongFlowViewModel({
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
      ...options,
    }),
  }
}

test('initial bounded pair composes the complete StrongFlow projection and source metadata', async () => {
  const { client, model } = view()
  await model.start()

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'subscribed')
  assert.equal(model.state.projection.delivery.deliveryId, deliveryId)
  assert.equal(model.state.projection.solutionReview.reviewStatus, 'approved')
  assert.equal(model.state.projection.stage.id, stageRunId)
  assert.equal(model.state.projection.runtime.stageRunId, stageRunId)
  assert.deepEqual(model.state.projection.evidence.map(item => item.sourceRef), ['artifact:test:1'])
  assert.equal(model.state.projection.verdict.status, 'pass')
  assert.equal(model.state.projection.attention[0].status, 'resolved')
  assert.equal(model.state.projection.publication.state, 'pending')
  assert.deepEqual(model.state.projection.metadata.revisions, {
    delivery: 1,
    deliverySpec: 3,
    runtime: 11,
    publication: 1,
  })
  assert.equal(model.state.projection.metadata.source, 'control-plane-snapshot')
  assert.equal(model.state.projection.metadata.updatedAt, '2026-08-27T01:00:06.000Z')
  assert.deepEqual(client.calls.map(call => call.query), [
    'delivery.get',
    'runtime.projection.get',
  ])
  assert.deepEqual(client.calls[1].parameters.atCursor, client.calls[0].parameters.atCursor
    ?? model.state.projection.delivery.readCursor)
  assert.deepEqual(client.subscription.startAt, readCursor().eventCursor)
  assert.deepEqual(client.subscription.subscription.eventTypes, [
    'delivery.changed.v1',
    'delivery-task.changed.v1',
    'attention.changed.v1',
    'runtime-projection.invalidated.v1',
  ])
})

test('empty StrongFlow creates and advances one Delivery with exact authority and revisions', async () => {
  const calls = []
  const created = {
    schemaVersion,
    deliveryId,
    revision: 1,
    status: 'draft',
    title: 'First StrongFlow Delivery',
    updatedAt: '2026-09-02T01:00:00.000Z',
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    activeStageRunId: null,
    openAttentionCount: 0,
    taskCounts: {
      total: 0,
      pending: 0,
      active: 0,
      blocked: 0,
      verifying: 0,
      completed: 0,
      failed: 0,
    },
  }
  const advanced = {
    ...created,
    revision: 2,
    status: 'clarifying',
    activeStageRunId: stageRunId,
  }
  const client = {
    async command(request) {
      calls.push(structuredClone(request))
      const result = request.command === 'delivery.create' ? created : advanced
      return {
        schemaVersion,
        requestId: request.requestId,
        command: request.command,
        outcome: 'completed',
        previousRevision: request.expectedRevision,
        currentRevision: result.revision,
        result,
      }
    },
    close() {},
  }
  const createdIds = []
  let requestSequence = 0
  const model = createStrongFlowCreateViewModel({
    client,
    actor,
    scope,
    nextDeliveryId: () => deliveryId,
    nextRequestId: () => requestId(++requestSequence),
    onCreated(value) { createdIds.push(value) },
  })

  await model.create({
    title: '  First StrongFlow Delivery  ',
    goal: '  Enter the advanced workspace without a reload.  ',
    baseRevision: '  0123456789abcdef0123456789abcdef01234567  ',
    scope: ['  Open the advanced workspace.  '],
    outOfScope: ['  Replace the default Chat surface.  '],
    constraints: ['  Keep the exact repository binding.  '],
    sourceProductSessionId: null,
    acceptanceCriteria: [
      '  The created Delivery is loaded from the Control Plane.  ',
      'The Delivery event stream is subscribed.',
    ],
  })

  assert.deepEqual(calls, [{
    schemaVersion,
    requestId: requestId(1),
    actor,
    scope,
    command: 'delivery.create',
    expectedRevision: 0,
    payload: {
      deliveryId,
      spec: {
        acceptanceCriteria: [{
          id: 'criterion:1',
          required: true,
          title: 'The created Delivery is loaded from the Control Plane.',
        }, {
          id: 'criterion:2',
          required: true,
          title: 'The Delivery event stream is subscribed.',
        }],
        baseRevision: '0123456789abcdef0123456789abcdef01234567',
        constraints: ['Keep the exact repository binding.'],
        goal: 'Enter the advanced workspace without a reload.',
        outOfScope: ['Replace the default Chat surface.'],
        publicationTarget: null,
        repositoryId: scope.repositoryId,
        scope: ['Open the advanced workspace.'],
        sourceProductSessionId: null,
        title: 'First StrongFlow Delivery',
      },
      tasks: [],
    },
  }, {
    schemaVersion,
    requestId: requestId(2),
    actor,
    scope,
    command: 'delivery.advance',
    expectedRevision: 1,
    payload: { deliveryId },
  }])
  assert.deepEqual(createdIds, [deliveryId])
  assert.equal(model.state.status, 'created')
  assert.equal(model.state.error, null)
})

test('Delivery conversion retries the same create and advance requests without duplicate creation', async () => {
  const calls = []
  const created = {
    schemaVersion,
    deliveryId,
    revision: 1,
    status: 'draft',
    title: 'Confirmed Chat requirement',
    updatedAt: '2026-09-02T01:00:00.000Z',
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    activeStageRunId: null,
    openAttentionCount: 0,
    taskCounts: {
      total: 0,
      pending: 0,
      active: 0,
      blocked: 0,
      verifying: 0,
      completed: 0,
      failed: 0,
    },
  }
  const advanced = {
    ...created,
    revision: 2,
    status: 'clarifying',
    activeStageRunId: stageRunId,
  }
  let createCalls = 0
  let advanceCalls = 0
  const client = {
    async command(request) {
      calls.push(structuredClone(request))
      if (request.command === 'delivery.create') {
        createCalls += 1
        if (createCalls === 1) throw new ControlPlaneClientError({
          kind: 'authorization',
          code: 'PERMISSION_DENIED',
          message: 'private permission detail',
          requestId: request.requestId,
          retryable: false,
        })
      } else {
        advanceCalls += 1
        if (advanceCalls === 1) throw new ControlPlaneClientError({
          kind: 'network',
          code: 'NETWORK_ERROR',
          message: 'response was lost',
          requestId: request.requestId,
          retryable: true,
        })
      }
      const result = request.command === 'delivery.create' ? created : advanced
      return {
        schemaVersion,
        requestId: request.requestId,
        command: request.command,
        outcome: 'completed',
        previousRevision: request.expectedRevision,
        currentRevision: result.revision,
        result,
      }
    },
  }
  let requestSequence = 0
  const opened = []
  const model = createStrongFlowCreateViewModel({
    client,
    actor,
    scope,
    nextDeliveryId: () => deliveryId,
    nextRequestId: () => requestId(++requestSequence),
    onCreated(value) { opened.push(value) },
  })
  const input = {
    title: 'Confirmed Chat requirement',
    goal: 'Implement the requirement confirmed in Chat.',
    baseRevision: '0123456789abcdef0123456789abcdef01234567',
    scope: ['Implement the confirmed requirement.'],
    outOfScope: [],
    constraints: ['Keep the source session binding.'],
    sourceProductSessionId: productSessionId,
    acceptanceCriteria: ['The confirmed result is delivered.'],
  }

  await model.create(input)
  assert.equal(model.state.error.code, 'PERMISSION_DENIED')
  await model.create(input)
  assert.equal(model.state.error.code, 'NETWORK_ERROR')
  await model.create(input)

  assert.equal(createCalls, 2)
  assert.equal(advanceCalls, 2)
  assert.deepEqual(calls[1], calls[0], 'permission retry must reuse the create idempotency key')
  assert.deepEqual(calls[3], calls[2], 'response-loss retry must reuse the advance idempotency key')
  assert.equal(calls[2].expectedRevision, 1)
  assert.deepEqual(opened, [deliveryId])
  assert.equal(model.state.status, 'created')
})

test('cancelling and duplicate activation retain one Delivery attempt for exact retry', async () => {
  const calls = []
  let first = true
  const created = {
    schemaVersion,
    deliveryId,
    revision: 1,
    status: 'draft',
    title: 'Cancelable conversion',
    updatedAt: '2026-09-02T01:00:00.000Z',
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    activeStageRunId: null,
    openAttentionCount: 0,
    taskCounts: {
      total: 0,
      pending: 0,
      active: 0,
      blocked: 0,
      verifying: 0,
      completed: 0,
      failed: 0,
    },
  }
  const advanced = { ...created, revision: 2, status: 'clarifying', activeStageRunId: stageRunId }
  const client = {
    async command(request, options) {
      calls.push(structuredClone(request))
      if (first) {
        first = false
        return new Promise((resolve, reject) => {
          options.signal.addEventListener('abort', () => {
            reject(new ControlPlaneClientError({
              kind: 'cancelled',
              code: 'REQUEST_CANCELLED',
              message: 'cancelled locally',
              requestId: request.requestId,
              retryable: false,
            }))
          }, { once: true })
        })
      }
      const result = request.command === 'delivery.create' ? created : advanced
      return {
        schemaVersion,
        requestId: request.requestId,
        command: request.command,
        outcome: 'completed',
        previousRevision: request.expectedRevision,
        currentRevision: result.revision,
        result,
      }
    },
  }
  let requestSequence = 0
  const model = createStrongFlowCreateViewModel({
    client,
    actor,
    scope,
    nextDeliveryId: () => deliveryId,
    nextRequestId: () => requestId(++requestSequence),
    onCreated() {},
  })
  const input = {
    title: 'Cancelable conversion',
    goal: 'Retry the exact request.',
    baseRevision: '0123456789abcdef0123456789abcdef01234567',
    scope: ['Retry the submitted conversion.'],
    outOfScope: [],
    constraints: [],
    sourceProductSessionId: null,
    acceptanceCriteria: ['Only one Delivery is created.'],
  }

  const pending = model.create(input)
  await Promise.resolve()
  void model.create(input)
  assert.equal(calls.length, 1)
  assert.equal(model.state.status, 'submitting')
  model.cancelPending()
  await pending
  assert.equal(model.state.error.code, 'REQUEST_CANCELLED')
  await model.create(input)

  assert.deepEqual(calls[1], calls[0], 'cancel retry must reuse the original request')
  assert.equal(calls[2].command, 'delivery.advance')
  assert.equal(model.state.status, 'created')
})

test('an accepted Delivery command can be cancelled and retried with its exact request', async () => {
  const calls = []
  const created = {
    schemaVersion,
    deliveryId,
    revision: 1,
    status: 'draft',
    title: 'Accepted conversion',
    updatedAt: '2026-09-02T01:00:00.000Z',
    ownership: {
      organizationId: scope.organizationId,
      workspaceId: scope.workspaceId,
      projectId: scope.projectId,
      repositoryId: scope.repositoryId,
    },
    activeStageRunId: null,
    openAttentionCount: 0,
    taskCounts: {
      total: 0,
      pending: 0,
      active: 0,
      blocked: 0,
      verifying: 0,
      completed: 0,
      failed: 0,
    },
  }
  const advanced = { ...created, revision: 2, status: 'clarifying', activeStageRunId: stageRunId }
  let accepted = true
  const client = {
    async command(request) {
      calls.push(structuredClone(request))
      if (accepted) {
        accepted = false
        return {
          schemaVersion,
          requestId: request.requestId,
          command: request.command,
          outcome: 'accepted',
          acceptedAt: '2026-09-02T01:00:00.000Z',
        }
      }
      const result = request.command === 'delivery.create' ? created : advanced
      return {
        schemaVersion,
        requestId: request.requestId,
        command: request.command,
        outcome: 'completed',
        previousRevision: request.expectedRevision,
        currentRevision: result.revision,
        result,
      }
    },
  }
  let requestSequence = 0
  const model = createStrongFlowCreateViewModel({
    client,
    actor,
    scope,
    nextDeliveryId: () => deliveryId,
    nextRequestId: () => requestId(++requestSequence),
    onCreated() {},
  })
  const input = {
    title: 'Accepted conversion',
    goal: 'Retry a command accepted by the server.',
    baseRevision: '0123456789abcdef0123456789abcdef01234567',
    scope: ['Retry the accepted command.'],
    outOfScope: [],
    constraints: [],
    sourceProductSessionId: null,
    acceptanceCriteria: ['The exact accepted request is retried.'],
  }

  await model.create(input)
  assert.equal(model.state.status, 'waiting')
  model.cancelPending()
  assert.equal(model.state.error.code, 'REQUEST_CANCELLED')
  await model.create(input)

  assert.deepEqual(calls[1], calls[0])
  assert.equal(calls[2].command, 'delivery.advance')
  assert.equal(model.state.status, 'created')
})

test('pending Worker attachment keeps the exact Delivery StageRun readable', async () => {
  const client = new FakeClient()
  const pending = delivery()
  pending.stages[0].sessionBinding = {
    ...pending.stages[0].sessionBinding,
    attempt: null,
    codexThreadId: null,
    fencingToken: null,
    leaseId: null,
    sessionIdentity: null,
    sourceIdentity: null,
    stageRunId: null,
    workerId: null,
    workerSessionId: null,
  }
  client.responses.set('delivery.get', response('delivery.get', pending))
  client.responses.set(
    'runtime.projection.get',
    response('runtime.projection.get', runtime(pending)),
  )
  const { model } = view(client)

  await model.start()

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.projection.stage.id, stageRunId)
  assert.equal(model.state.projection.stage.sessionBinding.stageRunId, null)
  assert.deepEqual(model.state.projection.runtime.sessions, [])
})

test('Delivery events reload one complete pair without publishing a partial revision', async () => {
  const { client, model } = view()
  const observed = []
  model.subscribe(state => observed.push(state))
  await model.start()
  const nextDelivery = delivery(2, 'refs/winwincode/candidate/2')
  client.enqueue('delivery.get', response('delivery.get', nextDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(nextDelivery),
  ))

  await client.subscription.onEvent({
    sequence: 2,
    event: {
      type: 'delivery.changed.v1',
      deliveryId,
      revision: 2,
      changeKind: 'advanced',
    },
  })

  assert.equal(observed.some(state => (
    state.status === 'refreshing'
    && state.realtime === 'reloading'
    && state.projection === null
  )), true)
  assert.equal(model.state.projection.metadata.revisions.delivery, 2)
  assert.equal(model.state.projection.currentCandidate.candidateRef, 'refs/winwincode/candidate/2')
  assert.equal(model.state.projection.verdict.candidateRef, 'refs/winwincode/candidate/2')
})

test('a newer canonical Codex StageRun atomically rebinds Runtime, projection, and route owner', async () => {
  const client = new FakeClient()
  const bindingChanges = []
  const { model } = view(client, {
    onStageBindingChange(binding) {
      bindingChanges.push(structuredClone(binding))
    },
  })
  await model.start()
  const nextDelivery = deliveryWithNextActiveStage(2)
  client.enqueue('delivery.get', response('delivery.get', nextDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(nextDelivery, nextProductSessionId, nextStageRunId),
  ))

  await client.subscription.onEvent({
    sequence: 2,
    event: {
      type: 'runtime-projection.invalidated.v1',
      scopeKind: 'delivery-stage',
      productSessionId: nextProductSessionId,
      deliveryId,
      stageRunId: nextStageRunId,
      projectionRevision: 12,
      lastProjectionSequence: 22,
      reloadQueries: ['delivery.get', 'runtime.projection.get'],
    },
  })

  assert.deepEqual(bindingChanges, [{
    productSessionId: nextProductSessionId,
    stageRunId: nextStageRunId,
  }])
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.projection.stage.id, nextStageRunId)
  assert.equal(model.state.projection.runtime.productSessionId, nextProductSessionId)
  assert.equal(model.state.projection.runtime.stageRunId, nextStageRunId)
  assert.deepEqual(client.calls.at(-1).parameters, {
    kind: 'delivery-stage',
    productSessionId: nextProductSessionId,
    deliveryId,
    stageRunId: nextStageRunId,
    atCursor: nextDelivery.readCursor,
  })
})

test('an older binding invalidation is absorbed by a newer Delivery read cut', async () => {
  const client = new FakeClient()
  const { model } = view(client)
  await model.start()
  const nextDelivery = deliveryWithNextActiveStage(2)
  client.enqueue('delivery.get', response('delivery.get', nextDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(nextDelivery, nextProductSessionId, nextStageRunId),
  ))

  await client.subscription.onEvent({
    sequence: 1,
    event: {
      type: 'runtime-projection.invalidated.v1',
      scopeKind: 'delivery-stage',
      productSessionId,
      deliveryId,
      stageRunId,
      projectionRevision: 1,
      lastProjectionSequence: 1,
      reloadQueries: ['delivery.get', 'runtime.projection.get'],
    },
  })

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.error, null)
  assert.equal(model.state.projection.stage.id, nextStageRunId)
  assert.equal(model.state.projection.runtime.stageRunId, nextStageRunId)
})

test('a superseded invalidation for a future StageRun remains fail-closed', async () => {
  const client = new FakeClient()
  const { model } = view(client)
  await model.start()
  const nextDelivery = deliveryWithNextActiveStage(2)
  client.enqueue('delivery.get', response('delivery.get', nextDelivery))

  await assert.rejects(client.subscription.onEvent({
    sequence: 1,
    event: {
      type: 'runtime-projection.invalidated.v1',
      scopeKind: 'delivery-stage',
      productSessionId: nextProductSessionId,
      deliveryId,
      stageRunId: 'run_00000000000000000000000099',
      projectionRevision: 1,
      lastProjectionSequence: 1,
      reloadQueries: ['delivery.get', 'runtime.projection.get'],
    },
  }), error => error.code === 'STRONGFLOW_RUNTIME_EVENT_MISMATCH')
  assert.equal(model.state.projection, null)
  assert.equal(model.state.error.code, 'STRONGFLOW_RUNTIME_EVENT_MISMATCH')
})

test('old binding events and lower Delivery revisions fail closed after a StageRun rebind', async () => {
  const client = new FakeClient()
  const { model } = view(client)
  await model.start()
  const nextDelivery = deliveryWithNextActiveStage(2)
  client.enqueue('delivery.get', response('delivery.get', nextDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(nextDelivery, nextProductSessionId, nextStageRunId),
  ))
  await client.subscription.onEvent({
    event: {
      type: 'delivery.changed.v1',
      deliveryId,
      revision: 2,
      changeKind: 'advanced',
    },
  })

  client.enqueue('delivery.get', response('delivery.get', nextDelivery))
  await assert.rejects(client.subscription.onEvent({
    event: {
      type: 'runtime-projection.invalidated.v1',
      scopeKind: 'delivery-stage',
      productSessionId,
      deliveryId,
      stageRunId,
      projectionRevision: 13,
      lastProjectionSequence: 23,
      reloadQueries: ['delivery.get', 'runtime.projection.get'],
    },
  }), error => error.code === 'STRONGFLOW_RUNTIME_EVENT_MISMATCH')
  assert.equal(model.state.projection, null)

  const lowerRevision = deliveryWithNextActiveStage(1)
  client.enqueue('delivery.get', response('delivery.get', lowerRevision))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(lowerRevision, nextProductSessionId, nextStageRunId),
  ))
  await model.refresh()
  assert.equal(model.state.projection, null)
  assert.equal(model.state.error.code, 'STRONGFLOW_SNAPSHOT_STALE')

  const sameRevisionRebind = deliveryWithNextActiveStage(2)
  sameRevisionRebind.stages.push({
    ...sameRevisionRebind.stages.at(-1),
    id: 'run_00000000000000000000000004',
    sessionBinding: sessionBinding(
      'psn_00000000000000000000000004',
      'run_00000000000000000000000004',
      '04',
    ),
  })
  client.enqueue('delivery.get', response('delivery.get', sameRevisionRebind))
  await model.refresh()
  assert.equal(model.state.projection, null)
  assert.equal(model.state.error.code, 'STRONGFLOW_STAGE_BINDING_ROLLBACK')
})

test('cross-Delivery invalidations fail before any reload query', async () => {
  const { client, model } = view()
  await model.start()
  const callCount = client.calls.length

  await assert.rejects(client.subscription.onEvent({
    event: {
      type: 'delivery.changed.v1',
      deliveryId: 'dlv_00000000000000000000000099',
      revision: 2,
      changeKind: 'advanced',
    },
  }), error => error.code === 'STRONGFLOW_EVENT_DELIVERY_MISMATCH')
  assert.equal(client.calls.length, callCount)
  assert.equal(model.state.projection.metadata.revisions.delivery, 1)
})

test('an invalidation cannot acknowledge a snapshot older than its announced revision', async () => {
  const { client, model } = view()
  await model.start()
  const staleDelivery = delivery(1)
  client.enqueue('delivery.get', response('delivery.get', staleDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(staleDelivery),
  ))

  await assert.rejects(client.subscription.onEvent({
    event: {
      type: 'delivery.changed.v1',
      deliveryId,
      revision: 2,
      changeKind: 'advanced',
    },
  }), error => error.code === 'STRONGFLOW_SNAPSHOT_STALE')
  assert.equal(model.state.projection, null)
  assert.equal(model.state.error.code, 'STRONGFLOW_SNAPSHOT_STALE')
})

test('a transiently stale Delivery invalidation retries within the same generation', async () => {
  const { client, model } = view()
  await model.start()
  const staleDelivery = delivery(1)
  const currentDelivery = delivery(2)
  client.enqueue('delivery.get', response('delivery.get', staleDelivery))
  client.enqueue('delivery.get', response('delivery.get', currentDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(currentDelivery),
  ))

  await client.subscription.onEvent({
    sequence: 2,
    event: {
      type: 'delivery.changed.v1',
      deliveryId,
      revision: 2,
      changeKind: 'advanced',
    },
  })

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.error, null)
  assert.equal(model.state.projection.metadata.revisions.delivery, 2)
  assert.deepEqual(client.calls.map(call => call.query), [
    'delivery.get',
    'runtime.projection.get',
    'delivery.get',
    'delivery.get',
    'runtime.projection.get',
  ])
})

test('a StageRun rebind invalidation uses its zero runtime sequence as the Delivery minimum', async () => {
  const { client, model } = view()
  await model.start()
  const staleDelivery = delivery(1)
  const currentDelivery = deliveryWithNextActiveStage(2)
  client.enqueue('delivery.get', response('delivery.get', staleDelivery))
  client.enqueue('delivery.get', response('delivery.get', currentDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(currentDelivery, nextProductSessionId, nextStageRunId),
  ))

  await client.subscription.onEvent({
    sequence: 2,
    event: {
      type: 'runtime-projection.invalidated.v1',
      scopeKind: 'delivery-stage',
      productSessionId: nextProductSessionId,
      deliveryId,
      stageRunId: nextStageRunId,
      projectionRevision: 2,
      lastProjectionSequence: 0,
      reloadQueries: ['delivery.get', 'runtime.projection.get'],
    },
  })

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.projection.stage.id, nextStageRunId)
  assert.equal(model.state.projection.runtime.stageRunId, nextStageRunId)
})

test('runtime invalidation validates the exact stage and reload query set', async () => {
  const { client, model } = view()
  await model.start()
  const nextDelivery = delivery(2)
  client.enqueue('delivery.get', response('delivery.get', nextDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(nextDelivery),
  ))

  await client.subscription.onEvent({
    event: {
      type: 'runtime-projection.invalidated.v1',
      scopeKind: 'delivery-stage',
      productSessionId,
      deliveryId,
      stageRunId,
      projectionRevision: 12,
      lastProjectionSequence: 22,
      reloadQueries: ['delivery.get', 'runtime.projection.get'],
    },
  })
  assert.equal(model.state.projection.runtime.revision, 12)

  await assert.rejects(client.subscription.onEvent({
    event: {
      type: 'runtime-projection.invalidated.v1',
      scopeKind: 'delivery-stage',
      productSessionId,
      deliveryId,
      stageRunId: 'run_00000000000000000000000099',
      projectionRevision: 13,
      lastProjectionSequence: 23,
      reloadQueries: ['delivery.get', 'runtime.projection.get'],
    },
  }), error => error.code === 'STRONGFLOW_RUNTIME_EVENT_MISMATCH')
  assert.equal(model.state.projection, null)
  assert.equal(model.state.error.code, 'STRONGFLOW_RUNTIME_EVENT_MISMATCH')
})

test('expired bounded read restarts Delivery before Runtime and never mixes cuts', async () => {
  const { client, model } = view()
  const firstDelivery = delivery(1)
  const secondDelivery = delivery(2)
  client.enqueue('delivery.get', response('delivery.get', firstDelivery))
  client.enqueue('runtime.projection.get', new ControlPlaneClientError({
    kind: 'server',
    code: 'READ_CURSOR_EXPIRED',
    message: 'The bounded read expired.',
    requestId: null,
    retryable: true,
  }))
  client.enqueue('delivery.get', response('delivery.get', secondDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(secondDelivery),
  ))

  await model.start()
  assert.deepEqual(client.calls.map(call => call.query), [
    'delivery.get',
    'runtime.projection.get',
    'delivery.get',
    'runtime.projection.get',
  ])
  assert.equal(model.state.projection.metadata.revisions.delivery, 2)
  assert.deepEqual(client.calls[3].parameters.atCursor, secondDelivery.readCursor)
})

test('a retryable trusted-facts command keeps its shape while rotating request keys', async () => {
  const client = new FakeClient()
  const requests = []
  let attempts = 0
  client.command = async request => {
    requests.push(request)
    attempts += 1
    if (attempts < 3) throw new ControlPlaneClientError({
      kind: 'server',
      code: 'TRUSTED_FACTS_UNAVAILABLE',
      message: 'Trusted facts are still catching up.',
      requestId: request.requestId,
      retryable: true,
    })
    return {
      schemaVersion,
      requestId: request.requestId,
      command: request.command,
      outcome: 'accepted',
      currentRevision: request.expectedRevision,
      acceptedAt: '2026-08-27T01:00:00.000Z',
    }
  }
  const { model } = view(client)
  await model.start()

  await model.advanceDelivery()

  assert.equal(attempts, 3)
  assert.deepEqual(requests.map(request => request.requestId), [
    requestId(3),
    requestId(4),
    requestId(5),
  ])
  const { requestId: _firstRequestId, ...firstRequest } = requests[0]
  assert.deepEqual(requests.map(({ requestId: _requestId, ...command }) => command), [
    firstRequest,
    firstRequest,
    firstRequest,
  ])
  assert.equal(requests[0].expectedRevision, 1)
  assert.deepEqual(requests[0].payload, { deliveryId })
  assert.equal(model.state.interaction.status, 'waiting')
  assert.equal(model.state.interaction.error, null)
})

test('submitVerdict sends the candidate reference digest, not the diff digest', async () => {
  const client = new FakeClient()
  const candidateDigest = `sha256:${'a'.repeat(64)}`
  const currentDelivery = delivery(1, `git-candidate:${candidateDigest}`)
  currentDelivery.stages[0].status = 'succeeded'
  currentDelivery.stages[0].finishedAt = '2026-08-27T01:00:06.000Z'
  currentDelivery.verdict = null
  currentDelivery.publication = null
  currentDelivery.readCursor.publicationRevision = 0
  client.responses.set('delivery.get', response('delivery.get', currentDelivery))
  client.responses.set(
    'runtime.projection.get',
    response('runtime.projection.get', runtime(currentDelivery)),
  )
  let commandRequest = null
  client.command = async request => {
    commandRequest = request
    return {
      schemaVersion,
      requestId: request.requestId,
      command: request.command,
      outcome: 'accepted',
      currentRevision: request.expectedRevision,
      acceptedAt: '2026-08-27T01:00:00.000Z',
    }
  }
  const { model } = view(client)
  await model.start()

  await model.submitVerdict()

  assert.equal(commandRequest.payload.candidateDigest, candidateDigest)
  assert.notEqual(
    commandRequest.payload.candidateDigest,
    model.state.projection.currentCandidate.diffSha256,
  )
  assert.equal(model.state.interaction.status, 'waiting')
})

test('submitVerdict waits for active StageRuns and does not send early', async () => {
  const client = new FakeClient()
  const currentDelivery = delivery(1, `git-candidate:sha256:${'a'.repeat(64)}`)
  currentDelivery.verdict = null
  currentDelivery.publication = null
  currentDelivery.readCursor.publicationRevision = 0
  client.responses.set('delivery.get', response('delivery.get', currentDelivery))
  client.responses.set(
    'runtime.projection.get',
    response('runtime.projection.get', runtime(currentDelivery)),
  )
  const { model } = view(client)
  await model.start()

  await model.submitVerdict()

  assert.equal(client.calls.filter(call => 'command' in call).length, 0)
  assert.equal(model.state.interaction.status, 'error')
  assert.equal(model.state.interaction.error.code, 'STRONGFLOW_VERDICT_STAGES_ACTIVE')
})

test('a higher Delivery event absorbs self-event cancellation after publishing the new cut', async () => {
  const client = new FakeClient()
  let commandStartedResolve
  const commandStarted = new Promise(resolve => { commandStartedResolve = resolve })
  client.command = async (_request, options) => {
    commandStartedResolve()
    return new Promise((_resolve, reject) => {
      options.signal.addEventListener('abort', () => reject(new ControlPlaneClientError({
        kind: 'cancelled',
        code: 'REQUEST_CANCELLED',
        message: 'The command request was superseded by the Delivery event.',
        requestId: null,
        retryable: false,
      })), { once: true })
    })
  }
  const { model } = view(client)
  await model.start()
  let cancellationReported = false
  model.subscribe(state => {
    if (
      cancellationReported
      || state.projection?.metadata.revisions.delivery !== 2
    ) return
    cancellationReported = true
    client.subscription.onError(new ControlPlaneClientError({
      kind: 'server',
      code: 'REQUEST_CANCELLED',
      message: 'The event handler observed the superseded command request.',
      requestId: null,
      retryable: false,
    }))
  })

  const commandPending = model.advanceDelivery()
  await commandStarted
  const nextDelivery = delivery(2, 'refs/winwincode/candidate/2')
  client.enqueue('delivery.get', response('delivery.get', nextDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(nextDelivery),
  ))
  await client.subscription.onEvent({
    sequence: 2,
    event: {
      type: 'delivery.changed.v1',
      deliveryId,
      revision: 2,
      changeKind: 'advanced',
    },
  })
  await commandPending

  assert.equal(cancellationReported, true)
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'subscribed')
  assert.equal(model.state.projection.metadata.revisions.delivery, 2)
  assert.equal(model.state.error, null)
  assert.equal(model.state.interaction.error, null)
})

test('a higher Delivery event absorbs a microtask-late self-event cancellation', async () => {
  const client = new FakeClient()
  let cancellationReportedResolve
  const cancellationReported = new Promise(resolve => {
    cancellationReportedResolve = resolve
  })
  const { model } = view(client)
  await model.start()
  let cancellationScheduled = false
  model.subscribe(state => {
    if (
      cancellationScheduled
      || state.projection?.metadata.revisions.delivery !== 2
    ) return
    cancellationScheduled = true
    queueMicrotask(() => {
      client.subscription.onError(new ControlPlaneClientError({
        kind: 'server',
        code: 'REQUEST_CANCELLED',
        message: 'The event handler reported the superseded request late.',
        requestId: null,
        retryable: false,
      }))
      cancellationReportedResolve()
    })
  })

  const nextDelivery = delivery(2, 'refs/winwincode/candidate/2')
  client.enqueue('delivery.get', response('delivery.get', nextDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(nextDelivery),
  ))
  await client.subscription.onEvent({
    sequence: 2,
    event: {
      type: 'delivery.changed.v1',
      deliveryId,
      revision: 2,
      changeKind: 'advanced',
    },
  })
  await cancellationReported

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'subscribed')
  assert.equal(model.state.projection.metadata.revisions.delivery, 2)
  assert.equal(model.state.error, null)
  assert.equal(model.state.interaction.error, null)
})

test('a same-generation subscription cancellation remains visible', async () => {
  const { client, model } = view()
  await model.start()
  const cancellation = new ControlPlaneClientError({
    kind: 'server',
    code: 'REQUEST_CANCELLED',
    message: 'The active subscription was cancelled.',
    requestId: null,
    retryable: false,
  })

  client.subscription.onError(cancellation)

  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.realtime, 'reconnecting')
  assert.equal(model.state.error, cancellation)
})

test('non-retryable command failures are exposed without replay', async () => {
  const client = new FakeClient()
  let attempts = 0
  const failure = new ControlPlaneClientError({
    kind: 'terminal',
    code: 'REVISION_CONFLICT',
    message: 'The Delivery revision changed.',
    requestId: null,
    retryable: false,
  })
  client.command = async () => {
    attempts += 1
    throw failure
  }
  const { model } = view(client)
  await model.start()

  await model.advanceDelivery()

  assert.equal(attempts, 1)
  assert.equal(model.state.interaction.status, 'error')
  assert.equal(model.state.interaction.error.code, 'REVISION_CONFLICT')
})

test('an exhausted trusted-facts retry reports one terminal interaction error', async () => {
  const client = new FakeClient()
  const requests = []
  let attempts = 0
  client.command = async request => {
    requests.push(request)
    attempts += 1
    throw new ControlPlaneClientError({
      kind: 'server',
      code: 'TRUSTED_FACTS_UNAVAILABLE',
      message: 'Trusted facts remain unavailable.',
      requestId: request.requestId,
      retryable: true,
    })
  }
  const { model } = view(client)
  const observed = []
  model.subscribe(state => observed.push(state))
  await model.start()
  observed.length = 0

  const startTime = Date.now()
  mock.timers.enable({ apis: ['Date', 'setTimeout'], now: startTime })
  try {
    const pending = model.advanceDelivery()
    for (let tick = 0; tick < 450 && attempts < 401; tick += 1) {
      await Promise.resolve()
      mock.timers.tick(50)
      await Promise.resolve()
      await Promise.resolve()
      await Promise.resolve()
      await Promise.resolve()
      await Promise.resolve()
    }
    await pending
  } finally {
    mock.timers.reset()
  }

  assert.equal(attempts, 401)
  assert.equal(new Set(requests.map(request => request.requestId)).size, attempts)
  assert.equal(observed.filter(state => state.interaction.status === 'error').length, 1)
  assert.equal(model.state.interaction.error.code, 'TRUSTED_FACTS_UNAVAILABLE')
})

test('candidate mismatch clears the previous projection and rejects an old Verdict', async () => {
  const { client, model } = view()
  await model.start()
  const inconsistent = delivery(2)
  inconsistent.verdict = {
    ...inconsistent.verdict,
    candidateRef: 'refs/winwincode/candidate/old',
  }
  client.enqueue('delivery.get', response('delivery.get', inconsistent))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(inconsistent),
  ))

  await model.refresh()
  assert.equal(model.state.status, 'error')
  assert.equal(model.state.projection, null)
  assert.equal(model.state.error.code, 'STRONGFLOW_VERDICT_CANDIDATE_MISMATCH')
})

test('reset, reconnect, authorization, cancellation, and close have explicit empty states', async () => {
  const { client, model } = view()
  await model.start()
  const nextDelivery = delivery(3)
  client.enqueue('delivery.get', response('delivery.get', nextDelivery))
  client.enqueue('runtime.projection.get', response(
    'runtime.projection.get',
    runtime(nextDelivery),
  ))
  const cursor = await client.subscription.onResetRequired()
  assert.deepEqual(cursor, nextDelivery.readCursor.eventCursor)
  assert.equal(model.state.projection.metadata.revisions.delivery, 3)

  const network = new ControlPlaneClientError({
    kind: 'network',
    code: 'NETWORK_ERROR',
    message: 'Disconnected.',
    requestId: null,
    retryable: true,
  })
  client.subscription.onError(network)
  assert.equal(model.state.realtime, 'reconnecting')
  model.reconnect()
  assert.equal(client.reconnects, 1)

  await client.subscription.onAuthorizationRevoked()
  assert.equal(model.state.status, 'authentication-required')
  assert.equal(model.state.realtime, 'access-revoked')
  assert.equal(model.state.projection, null)

  const cancelledView = view()
  await cancelledView.model.start()
  cancelledView.model.cancelPending()
  assert.equal(cancelledView.model.state.status, 'cancelled')
  assert.equal(cancelledView.model.state.projection, null)
  cancelledView.client.subscription.onError(new ControlPlaneClientError({
    kind: 'server',
    code: 'REQUEST_CANCELLED',
    message: 'The closed subscription reported its local cancellation.',
    requestId: null,
    retryable: false,
  }))
  assert.equal(cancelledView.model.state.status, 'cancelled')
  assert.equal(cancelledView.model.state.realtime, 'inactive')
  cancelledView.model.close()
  assert.equal(cancelledView.model.state.status, 'closed')
  assert.equal(cancelledView.model.state.realtime, 'closed')
})

test('StrongFlow view-model uses only the facade and never infers state from logs', () => {
  const source = readFileSync(resolve(root, 'apps/client/src/strongflow-view-model.ts'), 'utf8')
  assert.match(source, /options\.client\.command/u)
  assert.match(source, /options\.client\.query/u)
  assert.match(source, /options\.client\.subscribe/u)
  assert.doesNotMatch(
    source,
    /\bfetch\s*\(|new\s+WebSocket|@deepseek-ai|dsh-typert|remote\.|console\.|readFile|log\b/iu,
  )
})
