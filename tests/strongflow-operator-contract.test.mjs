import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import test from 'node:test'

import {
  AttemptId,
  DiagramId,
  HumanReviewId,
  JobId,
  RequirementId,
  SolutionId,
  StageRunId,
  STRONGFLOW_CLI_COMMANDS,
  STRONGFLOW_CLI_EXIT_CODES,
  STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION,
  STRONGFLOW_OPERATOR_ERROR_CODES,
  STRONGFLOW_OPERATOR_ERROR_DEFINITIONS,
  STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
  STRONGFLOW_OPERATOR_EXPORT_SCHEMA_VERSION,
  STRONGFLOW_OPERATOR_JOB_VIEW_SCHEMA_VERSION,
  STRONGFLOW_OPERATOR_OPERATIONS,
  STRONGFLOW_OPERATOR_SCHEMA_VERSION,
  StrongFlowOperatorValidationError,
  materializeStrongFlowArtifact,
  materializeStrongFlowOperatorFailure,
  materializeStrongFlowOperatorRequest,
  materializeStrongFlowOperatorSuccess,
  parseStrongFlowOperatorArtifactLink,
  parseStrongFlowOperatorEventCursor,
  parseStrongFlowOperatorRequest,
  parseStrongFlowOperatorResponse,
  parseStrongFlowOperatorResponseForRequest,
  renderStrongFlowCliHelp,
  strongFlowCliExitCode,
  strongFlowCliSignalExitCode,
  strongFlowOperatorEventCursor,
} from '../packages/contracts/dist/index.js'
import { generateStrongFlowDefinitionDiagrams } from '../packages/strongflow/dist/index.js'

const HASH_A = 'a'.repeat(64)

function ref(artifactKind, artifactId) {
  return Object.freeze({ artifactKind, artifactId })
}

function interval(suffix) {
  return Object.freeze({
    schemaVersion: 1,
    kernelSessionLineageId: `operator-lineage-${suffix}`,
    contextId: `operator-context-${suffix}`,
    generation: 1,
    kernelSessionId: `operator-kernel-${suffix}`,
    kernelStreamId: `operator-stream-${suffix}`,
    turnId: `operator-turn-${suffix}`,
    firstSequence: '10',
    lastSequence: '12',
    eventCount: 3,
  })
}

function expectOperatorError(code, path) {
  return error => (
    error instanceof StrongFlowOperatorValidationError
    && error.code === code
    && (path === undefined || error.path === path)
  )
}

function fixtures() {
  const jobId = JobId('operator-job')
  const ids = Object.freeze({
    user: 'operator-user-request',
    requirement: RequirementId('operator-requirement'),
    solution: SolutionId('operator-solution'),
    architecture: DiagramId('operator-system-diagram'),
    process: DiagramId('operator-process-diagram'),
  })
  const requirement = materializeStrongFlowArtifact('REQUIREMENT_SPEC', {
    artifactId: ids.requirement,
    jobId,
    sourceArtifacts: [ref('USER_REQUEST', ids.user)],
    producer: {
      kind: 'role',
      roleId: 'requirements',
      stageRunId: StageRunId('operator-requirements-run'),
      attemptId: AttemptId('operator-requirements-attempt'),
    },
    kernelEventInterval: interval('requirements'),
    createdAtMillis: 2_100_000_000_001,
  }, {
    title: '统一操作接口',
    summary: 'UI 与 CLI 使用同一个经过校验的接口。',
    goals: [{ id: 'goal-shared-interface', text: '所有操作保留同一身份。' }],
    nonGoals: [],
    constraints: [{ id: 'constraint-review', text: '执行前必须人工审核。' }],
    acceptanceCriteria: [{
      criterionId: 'criterion-stale',
      statement: '旧定义不能被批准。',
      verification: '提交旧定义并检查稳定错误代码。',
    }],
    repositoryFacts: [],
    risks: [],
    openQuestions: [],
  })
  const solution = materializeStrongFlowArtifact('SOLUTION_DESIGN', {
    artifactId: ids.solution,
    jobId,
    sourceArtifacts: [ref('REQUIREMENT_SPEC', ids.requirement)],
    producer: {
      kind: 'role',
      roleId: 'solution',
      stageRunId: StageRunId('operator-solution-run'),
      attemptId: AttemptId('operator-solution-attempt'),
    },
    kernelEventInterval: interval('solution'),
    createdAtMillis: 2_100_000_000_002,
  }, {
    requirementId: ids.requirement,
    summary: '用一个版本化调用接口隔开客户端和运行内核。',
    decisions: [{
      decisionId: 'decision-single-seam',
      title: '一个调用入口',
      decision: 'DSH UI 和 CLI 都提交同一种操作信封。',
      rationale: '客户端不需要知道 DSH 或 Codex 私有对象。',
      requirementItemIds: ['goal-shared-interface', 'criterion-stale'],
    }],
    components: [{
      componentId: 'component-operator-interface',
      name: 'Operator interface',
      kind: 'module',
      responsibility: '校验操作、结果、游标和错误。',
      trustBoundary: '本地客户端接口',
      sourcePaths: ['packages/contracts/src/strongflow-operator.ts'],
    }],
    connections: [],
    unresolvedFacts: [],
    risks: [],
  })
  const diagrams = generateStrongFlowDefinitionDiagrams({
    requirement,
    solution,
    systemArchitectureDiagramId: ids.architecture,
    processFlowDiagramId: ids.process,
    createdAtMillis: 2_100_000_000_003,
  })
  const definition = Object.freeze({
    requirementId: ids.requirement,
    solutionId: ids.solution,
    systemArchitectureDiagramId: ids.architecture,
    processFlowDiagramId: ids.process,
  })
  return Object.freeze({ jobId, ids, requirement, solution, diagrams, definition })
}

function reviewArtifact(fixture, decision, sequence) {
  return materializeStrongFlowArtifact('HUMAN_REVIEW_RECORD', {
    artifactId: HumanReviewId(`operator-review-${decision}-${sequence}`),
    jobId: fixture.jobId,
    sourceArtifacts: [
      ref('REQUIREMENT_SPEC', fixture.ids.requirement),
      ref('SOLUTION_DESIGN', fixture.ids.solution),
      ref('SYSTEM_ARCHITECTURE_DIAGRAM', fixture.ids.architecture),
      ref('PROCESS_FLOW_DIAGRAM', fixture.ids.process),
    ],
    producer: { kind: 'human', actorId: 'operator-reviewer', channel: 'local-ui' },
    kernelEventInterval: null,
    createdAtMillis: 2_100_000_000_100 + Number(sequence),
  }, {
    definition: fixture.definition,
    decision,
    comment: null,
    scope: decision === 'changes-requested' ? 'solution' : null,
  })
}

function jobView(fixture, options = {}) {
  const status = options.reviewStatus ?? 'pending'
  const record = options.review ?? null
  const state = options.state ?? 'AWAITING_HUMAN_REVIEW'
  const terminal = ['FAILED', 'REJECTED', 'CANCELLED', 'DELIVERED'].includes(state)
  const lockReason = status === 'pending'
    ? 'awaiting-human-review'
    : status === 'changes-requested'
      ? 'definition-revision-requested'
      : status === 'approved'
        ? 'job-active'
        : terminal
          ? 'job-terminal'
          : 'definition-incomplete'
  return {
    schemaVersion: STRONGFLOW_OPERATOR_JOB_VIEW_SCHEMA_VERSION,
    jobId: fixture.jobId,
    title: '操作接口作业',
    state,
    sequence: options.sequence ?? '10',
    updatedAtMillis: 2_100_000_000_100,
    definition: {
      revision: 1,
      requirementId: fixture.definition.requirementId,
      solutionId: fixture.definition.solutionId,
      systemArchitectureDiagramId: fixture.definition.systemArchitectureDiagramId,
      processFlowDiagramId: fixture.definition.processFlowDiagramId,
    },
    review: {
      status,
      definition: status === 'unavailable' ? null : fixture.definition,
      record,
    },
    activeStage: null,
    candidateId: null,
    interruption: options.interruption ?? null,
    lastStop: null,
    executionLock: {
      locked: !['definition-approved', 'job-active'].includes(lockReason),
      reason: lockReason,
      message: status === 'pending' ? '等待人工审核。' : '作业正在流转。',
    },
    allowedOperations: ['job.status', 'job.follow', 'job.artifacts', 'job.export'],
  }
}

function artifactLink(artifact, sequence) {
  const content = Buffer.from(JSON.stringify(artifact), 'utf8')
  const digest = createHash('sha256').update(content).digest('hex')
  const producer = artifact.producer.kind === 'role'
    ? {
      kind: 'role',
      roleId: artifact.producer.roleId,
      stageRunId: artifact.producer.stageRunId,
      attemptId: artifact.producer.attemptId,
      kernelSessionId: artifact.kernelEventInterval.kernelSessionId,
      firstKernelSequence: artifact.kernelEventInterval.firstSequence,
      lastKernelSequence: artifact.kernelEventInterval.lastSequence,
      kernelEventCount: artifact.kernelEventInterval.eventCount,
    }
    : artifact.producer
  return {
    schemaVersion: STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION,
    jobId: artifact.jobId,
    sequence,
    recordId: `operator-record-${sequence}`,
    artifactKind: artifact.artifactKind,
    artifactId: artifact.artifactId,
    blobId: `sha256-${digest}`,
    byteLength: content.byteLength,
    mediaType: 'application/vnd.winwincode.strongflow-artifact+json; charset=utf-8',
    createdAtMillis: artifact.createdAtMillis,
    producer,
    candidate: null,
  }
}

function eventView(fixture, options = {}) {
  const sequence = options.sequence ?? '10'
  const eventId = options.eventId ?? `operator-event-${sequence}`
  return {
    schemaVersion: STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
    eventId,
    cursor: strongFlowOperatorEventCursor(fixture.jobId, sequence, eventId),
    jobId: fixture.jobId,
    sequence,
    occurredAtMillis: 2_100_000_000_100 + Number(sequence),
    kind: options.kind ?? 'notice',
    state: options.state ?? 'AWAITING_HUMAN_REVIEW',
    source: options.source ?? { kind: 'system', actorId: 'operator-service' },
    stage: null,
    candidateId: options.candidateId ?? null,
    definition: options.definition ?? fixture.definition,
    artifactLinks: options.artifactLinks ?? [],
    change: options.change ?? null,
    message: options.message ?? '状态已更新。',
  }
}

function requestFixtures(fixture) {
  const afterCursor = strongFlowOperatorEventCursor(
    fixture.jobId,
    '2',
    'operator-event-2',
  )
  return [
    materializeStrongFlowOperatorRequest('job.create', 'request-create', {
      repositoryPath: '/tmp/operator-repository',
      baseRevision: null,
      title: '共享接口',
      request: '创建一个需要人工审核的作业。',
      submittedFrom: 'local-ui',
    }),
    materializeStrongFlowOperatorRequest('job.status', 'request-status', {
      jobId: fixture.jobId,
    }),
    materializeStrongFlowOperatorRequest('job.follow', 'request-follow', {
      jobId: fixture.jobId,
      afterCursor,
      limit: 100,
      waitMillis: 10_000,
    }),
    materializeStrongFlowOperatorRequest('definition.requirement', 'request-requirement', {
      jobId: fixture.jobId,
    }),
    materializeStrongFlowOperatorRequest('definition.solution', 'request-solution', {
      jobId: fixture.jobId,
    }),
    materializeStrongFlowOperatorRequest('definition.diagrams', 'request-diagrams', {
      jobId: fixture.jobId,
    }),
    materializeStrongFlowOperatorRequest('review.approve', 'request-approve-ui', {
      jobId: fixture.jobId,
      definition: fixture.definition,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof: 'ui-session-proof' },
      comment: null,
    }),
    materializeStrongFlowOperatorRequest('review.reject', 'request-reject-cli', {
      jobId: fixture.jobId,
      definition: fixture.definition,
      channel: 'cli',
      authentication: { scheme: 'local-peer', proof: 'cli-peer-proof' },
      comment: '拒绝。',
    }),
    materializeStrongFlowOperatorRequest('review.request-changes', 'request-changes-ui', {
      jobId: fixture.jobId,
      definition: fixture.definition,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof: 'ui-session-proof' },
      comment: '更新方案。',
      scope: 'solution',
    }),
    materializeStrongFlowOperatorRequest('job.cancel', 'request-cancel', {
      jobId: fixture.jobId,
      reason: '操作员取消。',
    }),
    materializeStrongFlowOperatorRequest('job.resume', 'request-resume', {
      jobId: fixture.jobId,
      interruptionSequence: '9',
    }),
    materializeStrongFlowOperatorRequest('job.artifacts', 'request-artifacts', {
      jobId: fixture.jobId,
      afterSequence: null,
      limit: 100,
      artifactKinds: ['REQUIREMENT_SPEC', 'SOLUTION_DESIGN'],
    }),
    materializeStrongFlowOperatorRequest('job.export', 'request-export', {
      jobId: fixture.jobId,
      format: 'manifest-json',
    }),
  ]
}

test('UI and CLI operations round-trip through one strict versioned request interface', () => {
  const fixture = fixtures()
  const requests = requestFixtures(fixture)
  assert.deepEqual(
    new Set(requests.map(request => request.operation)),
    new Set(STRONGFLOW_OPERATOR_OPERATIONS),
  )
  for (const request of requests) {
    const parsed = parseStrongFlowOperatorRequest(JSON.parse(JSON.stringify(request)))
    assert.deepEqual(parsed, request)
    assert.ok(Object.isFrozen(parsed))
    assert.ok(Object.isFrozen(parsed.payload))
  }
  assert.equal(requests.some(request => (
    request.operation === 'review.approve' && request.payload.channel === 'local-ui'
  )), true)
  assert.equal(requests.some(request => (
    request.operation === 'review.reject' && request.payload.channel === 'cli'
  )), true)
})

test('request validation rejects stale shapes, mismatched authentication, cursors, and limits', () => {
  const fixture = fixtures()
  const approve = requestFixtures(fixture).find(request => request.operation === 'review.approve')
  assert.ok(approve)
  assert.throws(
    () => parseStrongFlowOperatorRequest({ ...approve, schemaVersion: 2 }),
    expectOperatorError('UNSUPPORTED_SCHEMA_VERSION', 'request.schemaVersion'),
  )
  const missingDefinitionIdentity = structuredClone(approve)
  delete missingDefinitionIdentity.payload.definition.processFlowDiagramId
  assert.throws(
    () => parseStrongFlowOperatorRequest(missingDefinitionIdentity),
    expectOperatorError('INVALID_REQUEST', 'request.payload.definition'),
  )
  assert.throws(
    () => parseStrongFlowOperatorRequest({
      ...approve,
      payload: {
        ...approve.payload,
        authentication: { scheme: 'local-peer', proof: 'wrong-channel-proof' },
      },
    }),
    expectOperatorError('INVALID_REQUEST', 'request.payload.authentication.scheme'),
  )
  const otherJobCursor = strongFlowOperatorEventCursor(
    JobId('operator-other-job'),
    '2',
    'operator-event-2',
  )
  assert.throws(
    () => materializeStrongFlowOperatorRequest('job.follow', 'request-bad-cursor', {
      jobId: fixture.jobId,
      afterCursor: otherJobCursor,
      limit: 10,
      waitMillis: 0,
    }),
    expectOperatorError('INVALID_CURSOR', 'request.payload.afterCursor'),
  )
  assert.throws(
    () => materializeStrongFlowOperatorRequest('job.follow', 'request-bad-limit', {
      jobId: fixture.jobId,
      afterCursor: null,
      limit: 501,
      waitMillis: 0,
    }),
    expectOperatorError('LIMIT_EXCEEDED', 'request.payload.limit'),
  )
})

test('cursor pages are job-bound, strictly ordered, and resume from the exact last event', () => {
  const fixture = fixtures()
  const first = eventView(fixture, { sequence: '11' })
  const second = eventView(fixture, { sequence: '12' })
  const afterCursor = strongFlowOperatorEventCursor(
    fixture.jobId,
    '10',
    'operator-event-10',
  )
  const request = materializeStrongFlowOperatorRequest('job.follow', 'request-page', {
    jobId: fixture.jobId,
    afterCursor,
    limit: 100,
    waitMillis: 0,
  })
  const response = materializeStrongFlowOperatorSuccess(request, {
    schemaVersion: STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
    jobId: fixture.jobId,
    afterCursor,
    events: [first, second],
    nextCursor: second.cursor,
    caughtUp: true,
  })
  assert.deepEqual(
    parseStrongFlowOperatorResponseForRequest(request, JSON.parse(JSON.stringify(response))),
    response,
  )
  assert.deepEqual(parseStrongFlowOperatorEventCursor(response.result.nextCursor), {
    jobId: fixture.jobId,
    sequence: '12',
    eventId: 'operator-event-12',
  })

  const reversed = structuredClone(response)
  reversed.result.events.reverse()
  reversed.result.nextCursor = reversed.result.events.at(-1).cursor
  assert.throws(
    () => parseStrongFlowOperatorResponse(reversed),
    expectOperatorError('RELATIONSHIP_MISMATCH'),
  )
})

test('all operation result variants use public StrongFlow views and exact artifact identities', () => {
  const fixture = fixtures()
  const requests = new Map(requestFixtures(fixture).map(request => [request.operation, request]))
  const pendingJob = jobView(fixture)
  const requirementLink = artifactLink(fixture.requirement, '1')
  const solutionLink = artifactLink(fixture.solution, '2')
  const architectureLink = artifactLink(fixture.diagrams.systemArchitectureDiagram, '3')
  const processLink = artifactLink(fixture.diagrams.processFlowDiagram, '4')
  const followEvent = eventView(fixture, { sequence: '10' })
  const approval = reviewArtifact(fixture, 'approved', '11')
  const rejection = reviewArtifact(fixture, 'rejected', '12')
  const changes = reviewArtifact(fixture, 'changes-requested', '13')

  const mutation = (operation, sequence, kind, state, review) => ({
    job: jobView(fixture, {
      state,
      sequence,
      reviewStatus: review?.payload.decision ?? 'unavailable',
      review: review ?? null,
    }),
    event: eventView(fixture, { sequence, kind, state }),
    review: review ?? null,
  })
  const results = new Map([
    ['job.create', { job: pendingJob }],
    ['job.status', { job: pendingJob }],
    ['job.follow', {
      schemaVersion: STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
      jobId: fixture.jobId,
      afterCursor: null,
      events: [followEvent],
      nextCursor: followEvent.cursor,
      caughtUp: true,
    }],
    ['definition.requirement', {
      job: pendingJob,
      link: requirementLink,
      artifact: fixture.requirement,
    }],
    ['definition.solution', {
      job: pendingJob,
      link: solutionLink,
      artifact: fixture.solution,
    }],
    ['definition.diagrams', {
      job: pendingJob,
      definition: fixture.definition,
      systemArchitecture: {
        link: architectureLink,
        artifact: fixture.diagrams.systemArchitectureDiagram,
      },
      processFlow: {
        link: processLink,
        artifact: fixture.diagrams.processFlowDiagram,
      },
    }],
    ['review.approve', mutation(
      'review.approve',
      '11',
      'human-review.approved',
      'PLANNING',
      approval,
    )],
    ['review.reject', mutation(
      'review.reject',
      '12',
      'human-review.rejected',
      'REJECTED',
      rejection,
    )],
    ['review.request-changes', mutation(
      'review.request-changes',
      '13',
      'human-review.changes-requested',
      'DEFINING_SOLUTION',
      changes,
    )],
    ['job.cancel', mutation('job.cancel', '14', 'job.cancelled', 'CANCELLED', null)],
    ['job.resume', mutation('job.resume', '15', 'job.resumed', 'DEFINING_REQUIREMENTS', null)],
    ['job.artifacts', {
      schemaVersion: STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION,
      jobId: fixture.jobId,
      afterSequence: null,
      artifacts: [requirementLink, solutionLink, architectureLink, processLink],
      nextAfterSequence: '4',
    }],
    ['job.export', {
      schemaVersion: STRONGFLOW_OPERATOR_EXPORT_SCHEMA_VERSION,
      format: 'manifest-json',
      exportedAtMillis: 2_100_000_000_200,
      job: pendingJob,
      events: [followEvent],
      artifacts: [requirementLink, solutionLink, architectureLink, processLink],
    }],
  ])

  for (const operation of STRONGFLOW_OPERATOR_OPERATIONS) {
    const request = requests.get(operation)
    const result = results.get(operation)
    assert.ok(request, operation)
    assert.ok(result, operation)
    const response = materializeStrongFlowOperatorSuccess(request, result)
    const transported = parseStrongFlowOperatorResponseForRequest(
      request,
      JSON.parse(JSON.stringify(response)),
    )
    assert.deepEqual(transported, response, operation)
    const publicJson = JSON.stringify(transported)
    for (const privateName of [
      'authentication',
      'proof',
      'threadId',
      'modelProvider',
      'dshContext',
      'codexThread',
    ]) assert.equal(publicJson.includes(privateName), false, `${operation}: ${privateName}`)
  }
})

test('a response cannot switch the requested job or turn a stale review target into success', () => {
  const fixture = fixtures()
  const statusRequest = materializeStrongFlowOperatorRequest('job.status', 'request-bound-status', {
    jobId: fixture.jobId,
  })
  const otherJob = structuredClone(jobView(fixture))
  otherJob.jobId = JobId('operator-other-job')
  assert.throws(
    () => materializeStrongFlowOperatorSuccess(statusRequest, { job: otherJob }),
    expectOperatorError('RELATIONSHIP_MISMATCH', 'response.result'),
  )

  const staleDefinition = Object.freeze({
    ...fixture.definition,
    processFlowDiagramId: DiagramId('operator-stale-process-diagram'),
  })
  const reviewRequest = materializeStrongFlowOperatorRequest(
    'review.approve',
    'request-stale-review-success',
    {
      jobId: fixture.jobId,
      definition: staleDefinition,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof: 'ui-session-proof' },
      comment: null,
    },
  )
  const approval = reviewArtifact(fixture, 'approved', '21')
  assert.throws(
    () => materializeStrongFlowOperatorSuccess(reviewRequest, {
      job: jobView(fixture, {
        state: 'PLANNING',
        sequence: '21',
        reviewStatus: 'approved',
        review: approval,
      }),
      event: eventView(fixture, {
        sequence: '21',
        kind: 'human-review.approved',
        state: 'PLANNING',
      }),
      review: approval,
    }),
    expectOperatorError('RELATIONSHIP_MISMATCH', 'response.result.review'),
  )
})

test('running diff events deny details while frozen candidate events require exact identity', () => {
  const fixture = fixtures()
  const running = eventView(fixture, {
    sequence: '20',
    state: 'EXECUTING',
    kind: 'diff.updated',
    definition: fixture.definition,
    change: {
      state: 'executing',
      detailAccess: 'denied',
      changedPaths: ['packages/contracts/src/strongflow-operator.ts'],
      affectedNodes: [{
        diagramId: fixture.ids.architecture,
        nodeId: 'component:operator-interface',
      }],
    },
  })
  const request = materializeStrongFlowOperatorRequest('job.follow', 'request-running-diff', {
    jobId: fixture.jobId,
    afterCursor: null,
    limit: 10,
    waitMillis: 0,
  })
  const response = materializeStrongFlowOperatorSuccess(request, {
    schemaVersion: STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
    jobId: fixture.jobId,
    afterCursor: null,
    events: [running],
    nextCursor: running.cursor,
    caughtUp: false,
  })
  assert.equal(response.result.events[0].change.detailAccess, 'denied')

  const exposed = structuredClone(response)
  exposed.result.events[0].change.detailAccess = 'available'
  assert.throws(
    () => parseStrongFlowOperatorResponse(exposed),
    expectOperatorError('RELATIONSHIP_MISMATCH', 'response.result.event.change.detailAccess'),
  )

  const finishedWithoutCandidate = structuredClone(response)
  finishedWithoutCandidate.result.events[0].change.state = 'execution-finished'
  finishedWithoutCandidate.result.events[0].change.detailAccess = 'available'
  assert.throws(
    () => parseStrongFlowOperatorResponse(finishedWithoutCandidate),
    expectOperatorError('RELATIONSHIP_MISMATCH', 'response.result.event.candidateId'),
  )

  const annotationLink = {
    ...artifactLink(fixture.requirement, '30'),
    artifactKind: 'EXECUTION_CHANGE_ANNOTATION',
    artifactId: 'operator-annotation',
    producer: { kind: 'human', actorId: 'operator-reviewer', channel: 'local-ui' },
    candidate: {
      kind: 'diff',
      candidateId: 'operator-candidate',
      diffId: HASH_A,
    },
  }
  assert.deepEqual(
    parseStrongFlowOperatorArtifactLink(annotationLink).candidate,
    annotationLink.candidate,
  )
  const missingPatchCandidate = {
    ...artifactLink(fixture.requirement, '31'),
    artifactKind: 'PATCH_MANIFEST',
    artifactId: 'operator-patch',
    producer: {
      kind: 'role',
      roleId: 'executor',
      stageRunId: 'operator-execution-run',
      attemptId: 'operator-execution-attempt',
      kernelSessionId: 'operator-execution-kernel',
      firstKernelSequence: '20',
      lastKernelSequence: '22',
      kernelEventCount: 3,
    },
    candidate: null,
  }
  assert.throws(
    () => parseStrongFlowOperatorArtifactLink(missingPatchCandidate),
    expectOperatorError('RELATIONSHIP_MISMATCH', 'response.result.link.candidate'),
  )
})

test('stable public errors, exit codes, signals, and generated help cover every command', () => {
  const fixture = fixtures()
  const stale = materializeStrongFlowOperatorFailure({
    requestId: 'request-stale',
    operation: 'review.approve',
    code: 'STALE_DEFINITION',
    message: 'The reviewed definition is stale.',
    field: 'payload.definition',
    currentDefinition: fixture.definition,
  })
  assert.equal(stale.error.status, 409)
  assert.equal(stale.error.category, 'conflict')
  assert.equal(stale.error.retryable, false)
  assert.equal(strongFlowCliExitCode(stale), STRONGFLOW_CLI_EXIT_CODES.conflict)
  assert.deepEqual(parseStrongFlowOperatorResponse(JSON.parse(JSON.stringify(stale))), stale)
  assert.throws(
    () => materializeStrongFlowOperatorFailure({
      requestId: 'request-invalid-stale',
      operation: 'review.approve',
      code: 'STALE_DEFINITION',
      message: 'The reviewed definition is stale.',
    }),
    expectOperatorError('RELATIONSHIP_MISMATCH', 'response.error.currentDefinition'),
  )
  const reviewRequest = requestFixtures(fixture).find(request => (
    request.operation === 'review.approve'
  ))
  assert.ok(reviewRequest)
  const leakedAuthentication = materializeStrongFlowOperatorFailure({
    requestId: reviewRequest.requestId,
    operation: reviewRequest.operation,
    code: 'AUTHENTICATION_FAILED',
    message: `Rejected ${reviewRequest.payload.authentication.proof}`,
  })
  assert.throws(
    () => parseStrongFlowOperatorResponseForRequest(reviewRequest, leakedAuthentication),
    expectOperatorError('INVALID_RESPONSE', 'response'),
  )
  const overlappingIdentity = materializeStrongFlowOperatorRequest(
    'review.approve',
    'shared-proof-and-request-id',
    {
      ...reviewRequest.payload,
      authentication: {
        scheme: 'local-session',
        proof: 'shared-proof-and-request-id',
      },
    },
  )
  const overlappingFailure = materializeStrongFlowOperatorFailure({
    requestId: overlappingIdentity.requestId,
    operation: overlappingIdentity.operation,
    code: 'AUTHENTICATION_REQUIRED',
    message: 'The local session proof was not accepted.',
  })
  assert.deepEqual(
    parseStrongFlowOperatorResponseForRequest(overlappingIdentity, overlappingFailure),
    overlappingFailure,
  )

  assert.deepEqual(
    new Set(STRONGFLOW_CLI_COMMANDS.map(command => command.operation)),
    new Set(STRONGFLOW_OPERATOR_OPERATIONS),
  )
  assert.equal(STRONGFLOW_CLI_COMMANDS.length, STRONGFLOW_OPERATOR_OPERATIONS.length)
  const help = renderStrongFlowCliHelp()
  for (const command of STRONGFLOW_CLI_COMMANDS) {
    assert.match(help, new RegExp(`winwincode ${command.command}(?: |$)`, 'u'))
  }
  assert.match(help, /--request-id/u)
  assert.match(help, /SIGINT exits 130/u)
  assert.match(help, /SIGTERM exits 143/u)
  assert.match(help, /cancel command/u)
  assert.equal(STRONGFLOW_CLI_EXIT_CODES.sigint, 130)
  assert.equal(STRONGFLOW_CLI_EXIT_CODES.sigterm, 143)
  assert.equal(strongFlowCliSignalExitCode('SIGINT'), 130)
  assert.equal(strongFlowCliSignalExitCode('SIGTERM'), 143)
  assert.deepEqual(new Set(Object.keys(STRONGFLOW_OPERATOR_ERROR_DEFINITIONS)), new Set(
    STRONGFLOW_OPERATOR_ERROR_CODES,
  ))
  assert.equal(STRONGFLOW_OPERATOR_SCHEMA_VERSION, 1)
})

test('the published operator declaration has no DSH, Codex, or native private dependency', () => {
  const declaration = readFileSync(new URL(
    '../packages/contracts/dist/strongflow-operator.d.ts',
    import.meta.url,
  ), 'utf8')
  for (const privateDependency of [
    '@deepseek-ai/',
    '@winwincode/native',
    'RuntimeSourceIdentity',
    'CodexThread',
    'DSHContext',
  ]) assert.equal(declaration.includes(privateDependency), false, privateDependency)
})
