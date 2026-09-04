// SPDX-License-Identifier: Apache-2.0

import { mountWinWinCodeClient } from '/module/application.js'
import { ControlPlaneClientError } from '/module/control-plane-client.js'
import {
  canAdvanceStrongFlowDelivery,
  hasUnmetStrongFlowRequiredCriterion,
} from '/module/strongflow-view-model.js'

// UI-607: the whole Delivery review vertical in one real browser. The injected
// Control Plane facade is the only server: it answers the exact typed queries,
// validates every command's expectedRevision the way the Control Plane does, and
// publishes real delivery.changed.v1 events, so the mounted page observes a
// faithful read model while the browser drives the review chain itself.
//
// `ui607AdvanceServer(name)` moves the read model to the next named Control
// Plane state, so the Node test steps the chain forward between scenarios.

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const projectScope = {
  kind: 'project',
  organizationId: scope.organizationId,
  workspaceId: scope.workspaceId,
  projectId: scope.projectId,
}

const deliveryId = 'dlv_00000000000000000000000002'
const taskId = 'task:export'
const nodeId = 'node:export-gateway'
const specId = 'spec:ui607'

const planningRunId = 'run_00000000000000000000000001'
const planReviewRunId = 'run_00000000000000000000000002'
const executingRunId = 'run_00000000000000000000000003'
const verifyingRunId = 'run_00000000000000000000000004'
const reworkRunId = 'run_00000000000000000000000005'
const reverifyingRunId = 'run_00000000000000000000000006'

/** Exact execution binding index per StageRun, so history reads the right one. */
const RUN_BINDING_INDEX = new Map([
  [planningRunId, 1],
  [executingRunId, 3],
  [verifyingRunId, 4],
  [reworkRunId, 5],
  [reverifyingRunId, 6],
])

const attentionId = 'att_0000000000000000000000000c'
const credentialReferenceId = 'crd_00000000000000000000000001'
const chatProductSessionId = 'psn_00000000000000000000000001'
const publicationId = 'pub_00000000000000000000000002'
const reviewSetDigest = `sha256:${'7'.repeat(64)}`
const publicationSetDigest = `sha256:${'9'.repeat(64)}`

const modelRoute = {
  providerId: 'browser-provider',
  modelId: 'browser-model',
  credentialReferenceId,
}

// criterion:2 fails on the business rule and criterion:3 reports an
// infrastructure error, so the workbench has to surface both shapes and keep
// both away from the final Delivery approval.
const CRITERIA = [
  { id: 'criterion:1', description: 'The rework stays inside the declared scope.', verificationMethod: 'unit test', required: true },
  { id: 'criterion:2', description: 'The export stays secret-safe.', verificationMethod: 'secret scan', required: true },
  { id: 'criterion:3', description: 'Verification ran to completion.', verificationMethod: 'focused run', required: true },
  { id: 'criterion:4', description: 'The changelog is updated.', verificationMethod: 'review', required: false },
]
const criterion = id => CRITERIA.find(entry => entry.id === id)

const calls = { commands: [], queries: [], queryErrors: [], subscriptions: [] }
const world = {
  state: 'verifying-failed',
  revision: 9,
  eventSequence: 14,
  candidateTag: 'a',
  frozenTags: ['a'],
  conflictOnceFor: null,
  receiptReads: 0,
  servedRevisions: [],
}
let realtimeOptions = null

function identifier(prefix, index) {
  return `${prefix}_${String(index).padStart(26, '0')}`
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function response(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: page(),
  }
}

function completed(request, previousRevision, currentRevision) {
  return {
    schemaVersion,
    requestId: request.requestId,
    command: request.command,
    outcome: 'completed',
    previousRevision,
    currentRevision,
    result: { deliveryId, revision: currentRevision },
  }
}

function accessFailure(request, kind, code, retryable) {
  return new ControlPlaneClientError({
    kind,
    code,
    message: `the Control Plane rejected this request with ${code}`,
    requestId: request?.requestId ?? null,
    retryable,
  })
}

function ownership() {
  return {
    organizationId: scope.organizationId,
    workspaceId: scope.workspaceId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
  }
}

function readCursor() {
  return {
    token: `cursor_${String(world.revision).padStart(32, '0')}`,
    scope,
    deliveryId,
    deliveryRevision: world.revision,
    runtimeLedgerRevision: world.revision + 100,
    runtimeAcceptedSequence: world.revision + 200,
    publicationRevision: publication() === null ? 0 : publication().revision,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: world.eventSequence,
      eventId: world.eventSequence === 0 ? null : identifier('evt', world.eventSequence),
    },
  }
}

function binding(index) {
  const suffix = String(index).padStart(2, '0')
  return {
    bindingId: `binding:ui607:${suffix}`,
    boundAt: '2026-09-03T01:00:00.000Z',
    executionJobId: identifier('job', index),
    productSessionId: identifier('psn', index),
    stageRunId: null,
    workerSessionId: identifier('wsn', index),
    codexThreadId: identifier('cdx', index),
    attempt: 1,
    fencingToken: `fence:${suffix}`,
    leaseId: identifier('lse', index),
    workerId: identifier('wrk', index),
    sourceIdentity: {
      kind: 'execution-worker',
      leaseId: identifier('lse', index),
      workerId: identifier('wrk', index),
      workerInstanceId: identifier('wki', index),
      workerSessionId: identifier('wsn', index),
    },
    sessionIdentity: {
      productSessionId: identifier('psn', index),
      workerSessionId: identifier('wsn', index),
      codexThreadId: identifier('cdx', index),
      stageRunId: null,
    },
  }
}

function stage(id, index, overrides = {}) {
  const sessionBinding = overrides.sessionBinding === undefined
    ? binding(index)
    : overrides.sessionBinding
  return {
    id,
    actorType: sessionBinding === null ? 'human' : 'codex',
    attempt: overrides.attempt ?? 1,
    deliveryTaskId: overrides.deliveryTaskId ?? null,
    finishedAt: overrides.finishedAt ?? '2026-09-03T01:05:00.000Z',
    role: overrides.role ?? 'implementer',
    sessionBinding: sessionBinding === null
      ? null
      : { ...sessionBinding, stageRunId: id },
    stage: overrides.stage ?? 'executing',
    startedAt: overrides.startedAt ?? '2026-09-03T01:00:00.000Z',
    status: overrides.status ?? 'succeeded',
  }
}

function candidateRecord(tag) {
  const suffix = tag.repeat(40)
  const attempt = tag === 'b' ? reworkRunId : executingRunId
  return {
    candidateRef: `git-candidate:sha256:${tag.repeat(64)}`,
    candidateCommitId: suffix,
    candidateTreeId: suffix,
    deliverySpecId: specId,
    deliverySpecRevision: 3,
    diffSha256: `sha256:${tag.repeat(64)}`,
    frozenAt: tag === 'a'
      ? '2026-09-03T01:04:00.000Z'
      : tag === 'c'
        ? '2026-09-03T01:08:00.000Z'
        : '2026-09-03T01:20:00.000Z',
    producerSessionBindingId: tag === 'b' ? 'binding:ui607:05' : 'binding:ui607:03',
    producerStageRunId: attempt,
  }
}

function currentCandidate() {
  return candidateRecord(world.candidateTag)
}

function criterionResult(id, verdict, evidenceRefs) {
  return {
    criterionId: id,
    evaluatedAt: '2026-09-03T01:06:00.000Z',
    evidenceRefs,
    explanation: `${criterion(id).description} — result ${verdict}.`,
    resultId: `result:${id}`,
    verdict,
  }
}

function activeVerdict() {
  const candidate = currentCandidate()
  const base = {
    id: `verdict:candidate-${world.candidateTag}`,
    candidateRef: candidate.candidateRef,
    deliverySpecId: specId,
    deliverySpecRevision: 3,
    producedAt: '2026-09-03T01:06:00.000Z',
    unresolvedFindings: [],
  }
  if (world.state === 'verifying-b') return null
  if (world.candidateTag === 'b') {
    return {
      ...base,
      criteria: [
        criterionResult('criterion:1', 'pass', ['evd_00000000000000000000000004']),
        criterionResult('criterion:2', 'pass', ['evd_00000000000000000000000005']),
        criterionResult('criterion:3', 'pass', ['evd_00000000000000000000000006']),
      ],
      producedAt: '2026-09-03T01:26:00.000Z',
      status: 'pass',
    }
  }
  return {
    ...base,
    criteria: [
      criterionResult('criterion:1', 'pass', ['evd_00000000000000000000000001']),
      criterionResult('criterion:2', 'fail', ['evd_00000000000000000000000002']),
      criterionResult('criterion:3', 'infra_error', ['evd_00000000000000000000000003']),
    ],
    status: 'fail',
    unresolvedFindings: ['The export report still names one internal host.'],
  }
}

function evidenceRow(id, type, sourceRef, stageRunId, createdAt) {
  return {
    candidateRef: currentCandidate().candidateRef,
    createdAt,
    deliverySpecId: specId,
    deliverySpecRevision: 3,
    id,
    sessionBindingId: `binding:ui607:${world.candidateTag === 'b' ? '06' : '04'}`,
    sourceRef,
    stageRunId,
    type,
  }
}

function deliveryEvidence() {
  const preflight = world.candidateTag === 'b'
    ? {
      first: 'evd_00000000000000000000000004',
      scan: 'evd_00000000000000000000000005',
      run: 'evd_00000000000000000000000006',
      stageRunId: reverifyingRunId,
      session: 'binding:ui607:06',
    }
    : {
      first: 'evd_00000000000000000000000001',
      scan: 'evd_00000000000000000000000002',
      run: 'evd_00000000000000000000000003',
      stageRunId: verifyingRunId,
      session: 'binding:ui607:04',
    }
  return [
    evidenceRow(
      preflight.first,
      'test',
      `artifact:test:${world.candidateTag}`,
      preflight.stageRunId,
      '2026-09-03T01:05:30.000Z',
    ),
    evidenceRow(
      preflight.scan,
      'command',
      `artifact:scan:${world.candidateTag}`,
      preflight.stageRunId,
      '2026-09-03T01:05:40.000Z',
    ),
    evidenceRow(
      preflight.run,
      'runtime_event',
      `artifact:run:${world.candidateTag}`,
      preflight.stageRunId,
      '2026-09-03T01:05:50.000Z',
    ),
  ]
}

function solutionReview() {
  return {
    approach: ['Freeze one bounded candidate and verify it.'],
    architectureDiagram: {
      id: 'diagram:architecture',
      kind: 'system-architecture',
      title: 'Export gateway',
      nodes: [{
        id: nodeId,
        label: 'Export gateway',
        description: 'Redacts every export before it leaves the workspace.',
        kind: 'component',
        trustBoundary: null,
        unresolved: false,
      }],
      edges: [],
    },
    attentionItemId: attentionId,
    comments: 'Approved as one bounded plan.',
    components: [],
    connections: [],
    decision: 'approve',
    deliveryId,
    deliverySpecId: specId,
    deliverySpecRevision: 3,
    planningSessionBindingId: 'binding:ui607:01',
    planningStageRunId: planningRunId,
    processDiagram: {
      id: 'diagram:process',
      kind: 'process-flow',
      title: 'Verification flow',
      nodes: [{
        id: 'node:verify',
        label: 'Verify candidate',
        description: 'Runs the focused verification lane.',
        kind: 'process-step',
        trustBoundary: null,
        unresolved: false,
      }],
      edges: [],
    },
    requestedChanges: null,
    reviewedAt: '2026-09-03T01:02:00.000Z',
    reviewerId: actor.id,
    reviewSetSha256: reviewSetDigest,
    reviewStageRunId: planReviewRunId,
    reviewStatus: 'approved',
    risks: [],
    solutionId: 'solution:ui607',
    summary: 'One bounded export with a redaction gate.',
    taskProposals: [],
    unresolvedItems: [],
  }
}

function attentionRecord() {
  const open = ['verifying-failed', 'candidate-superseded', 'hostile-ready']
    .includes(world.state)
  return {
    assignedTo: actor.id,
    blocking: open,
    createdAt: '2026-09-03T01:06:30.000Z',
    deliverySpecId: specId,
    id: attentionId,
    options: [],
    resolutionSummary: open
      ? null
      : 'Bounded rework approved on the current candidate.',
    resolvedAt: open ? null : '2026-09-03T01:10:00.000Z',
    resolvedBy: open ? null : actor.id,
    stageRunId: verifyingRunId,
    status: open ? 'open' : 'resolved',
    title: 'Verification blocked: two required criteria did not pass',
    type: 'verification_blocked',
  }
}

function stages() {
  const base = [
    stage(planningRunId, 1, { role: 'planner', stage: 'planning' }),
    stage(planReviewRunId, 0, {
      role: 'reviewer',
      stage: 'plan-review',
      sessionBinding: null,
    }),
    stage(executingRunId, 3, { deliveryTaskId: taskId }),
    stage(verifyingRunId, 4, { role: 'verifier', stage: 'verifying', deliveryTaskId: taskId }),
  ]
  if (world.state === 'verifying-failed' || world.state === 'candidate-superseded') return base
  if (world.state === 'hostile-ready') return base
  if (world.state === 'reworking') {
    return [...base, stage(reworkRunId, 5, {
      attempt: 2,
      deliveryTaskId: taskId,
      finishedAt: null,
      stage: 'reworking',
      status: 'running',
    })]
  }
  return [...base,
    stage(reworkRunId, 5, { attempt: 2, deliveryTaskId: taskId, stage: 'reworking' }),
    stage(reverifyingRunId, 6, {
      role: 'verifier',
      stage: 'verifying',
      deliveryTaskId: taskId,
    }),
  ]
}

/** The Publication journal states the coordinator has reached, in order. */
function publicationStates() {
  if (world.state === 'delivered') return ['publishing']
  if (world.state === 'approval-expired') return ['publishing', 'failed']
  if (world.state === 'retry-failed') return ['publishing', 'failed', 'failed']
  return ['publishing', 'failed', 'failed', 'published']
}

function publication() {
  // The coordinator creates the Publication only once the approved Delivery has
  // been advanced; every earlier state still reads as "not created".
  if (['verifying-failed', 'candidate-superseded', 'reworking', 'hostile-ready',
    'verifying-b', 'ready-to-deliver'].includes(world.state)) return null
  const states = publicationStates()
  const revision = states.length
  const state = states.at(-1)
  return {
    approvalAttentionItemId: attentionId,
    approvedAt: '2026-09-03T01:28:00.000Z',
    approvedBy: actor.id,
    candidateRef: candidateRecord('b').candidateRef,
    deliveryId,
    deliverySpecId: specId,
    deliverySpecRevision: 3,
    deliveryVerdictId: 'verdict:candidate-b',
    id: publicationId,
    publicationSetSha256: publicationSetDigest,
    resourceRef: state === 'published'
      ? { kind: 'github_pull_request', number: 21, repository: 'winwincode/browser-fixture' }
      : null,
    revision,
    state,
    target: {
      baseBranch: 'main',
      headBranch: 'winwincode/candidate-b',
      headRepository: 'winwincode/browser-fixture',
      provider: 'github',
      repository: 'winwincode/browser-fixture',
    },
    updatedAt: `2026-09-03T01:${String(30 + revision).padStart(2, '0')}:00.000Z`,
    verdictStatus: 'pass',
  }
}

function status() {
  switch (world.state) {
    case 'verifying-failed':
    case 'candidate-superseded': return 'needs-attention'
    case 'reworking': return 'reworking'
    case 'verifying-b': return 'verifying'
    case 'ready-to-deliver':
    case 'hostile-ready': return 'ready-to-deliver'
    default: return 'delivered'
  }
}

function deliveryDetail() {
  const reworked = world.candidateTag === 'b'
  return {
    kind: 'delivery_detail',
    schemaVersion,
    deliveryId,
    deliveryRevision: world.revision,
    readCursor: readCursor(),
    ownership: ownership(),
    status: status(),
    requirements: {
      deliverySpecId: specId,
      deliverySpecRevision: 3,
      title: 'UI-607 bounded review vertical',
      goal: 'Review a candidate, approve bounded rework, and publish one receipt.',
      scope: ['Export gateway redaction'],
      outOfScope: ['Any new provider integration'],
      constraints: ['No credential material may reach an export'],
      acceptanceCriteria: CRITERIA.map(entry => ({
        id: entry.id,
        description: entry.description,
        verificationMethod: entry.verificationMethod,
        required: entry.required,
      })),
      sourceRef: null,
      publicationTarget: {
        baseBranch: 'main',
        headBranch: 'winwincode/candidate-b',
        headRepository: 'winwincode/browser-fixture',
        provider: 'github',
        repository: 'winwincode/browser-fixture',
      },
      repository: { kind: 'local-git', locator: 'workspace://repository' },
      baseRevision: '0123456789012345678901234567890123456789',
      maxReworkAttempts: 2,
    },
    solutionReview: solutionReview(),
    diagramExecution: null,
    stages: stages(),
    tasks: [{
      acceptanceCriterionIds: ['criterion:1', 'criterion:2', 'criterion:3'],
      blockedByTaskIds: [],
      evidenceRefs: [],
      goal: 'Keep the rework inside the declared scope.',
      id: taskId,
      owner: null,
      stageRunIds: [
        executingRunId,
        verifyingRunId,
        reworkRunId,
        reverifyingRunId,
      ],
      status: 'completed',
      title: 'Ship the redacted export',
    }],
    attention: [attentionRecord()],
    evidence: deliveryEvidence(),
    currentCandidate: currentCandidate(),
    verdict: activeVerdict(),
    publication: publication(),
  }
}

function deliveryRuntime() {
  const cursor = readCursor()
  const active = stages().filter(item => item.sessionBinding !== null).at(-1)
  return {
    kind: 'runtime_projection',
    productSessionId: active.sessionBinding.productSessionId,
    deliveryId,
    stageRunId: active.id,
    readCursor: cursor,
    eventCursor: cursor.eventCursor,
    lastProjectionSequence: cursor.runtimeAcceptedSequence,
    revision: cursor.runtimeLedgerRevision,
    rebuiltAt: '2026-09-03T01:26:00.000Z',
    sessions: [],
  }
}

function historicalRuntime(stageRunId, index) {
  const cursor = readCursor()
  const productSessionId = identifier('psn', index)
  const bindingId = `binding:ui607:${String(index).padStart(2, '0')}`
  return {
    kind: 'runtime_projection',
    productSessionId,
    deliveryId,
    stageRunId,
    readCursor: cursor,
    eventCursor: cursor.eventCursor,
    lastProjectionSequence: cursor.runtimeAcceptedSequence,
    revision: cursor.runtimeLedgerRevision,
    rebuiltAt: '2026-09-03T01:05:00.000Z',
    sessions: [{
      productSessionId,
      stageRunId,
      sessionBindingId: bindingId,
      executionJobId: identifier('job', index),
      workerSessionId: identifier('wsn', index),
      codexThreadId: identifier('cdx', index),
      fencingToken: 'fence:03',
      leaseId: identifier('lse', index),
      attempt: 1,
      deliveryTaskId: taskId,
      asOfSequence: cursor.runtimeAcceptedSequence,
      diffSummary: null,
      plan: null,
      usage: null,
      recovery: {
        failureCount: 0,
        lastFailureSourceRef: null,
        latestRecoverySourceRef: null,
        recoveryCount: 0,
        state: 'none',
      },
      agents: [],
      agentEdges: [],
      activities: [{
        activityType: 'shell_command',
        callId: 'call:ui607:1',
        command: 'corepack pnpm test --scope export',
        outcome: 'succeeded',
        exitCode: 0,
        sourceRef: 'artifact:runtime:scope',
        status: 'succeeded',
      }],
    }],
  }
}

function candidateFiles() {
  const added = world.candidateTag === 'a'
  return [
    {
      path: 'src/export/gateway.ts',
      oldPath: null,
      status: 'modified',
      additions: added ? 18 : 24,
      deletions: 4,
      binary: false,
      encoding: 'utf-8',
    },
    {
      path: 'src/export/redact.ts',
      oldPath: null,
      status: added ? 'added' : 'modified',
      additions: added ? 40 : 52,
      deletions: added ? 0 : 6,
      binary: false,
      encoding: 'utf-8',
    },
    {
      path: 'docs/changelog.md',
      oldPath: null,
      status: 'added',
      additions: 6,
      deletions: 0,
      binary: false,
      encoding: 'utf-8',
    },
  ]
}

function diffBody(file) {
  return [
    `diff --git a/${file.path} b/${file.path}`,
    `--- a/${file.path}`,
    `+++ b/${file.path}`,
    '@@ -1,4 +1,9 @@',
    '+// UI-607 browser fixture diff for a real review read.',
    ' export function redact(input) {',
    '-  return input',
    '+  return input.replace(/internal-[a-z]+/u, "[redacted]")',
    ' }',
    '',
  ].join('\n')
}

function evidenceDetail(request) {
  const parameters = request.parameters
  const row = deliveryEvidence().find(entry => entry.id === parameters.evidenceId)
  if (row === undefined) throw new Error(`unknown evidence ${parameters.evidenceId}`)
  if (
    parameters.candidateRef !== row.candidateRef
    || parameters.stageRunId !== row.stageRunId
    || parameters.sessionBindingId !== row.sessionBindingId
    || parameters.sourceRef !== row.sourceRef
    || parameters.type !== row.type
    || parameters.deliveryId !== deliveryId
    || parameters.readPageLimit !== 1
    || parameters.atCursor.token !== readCursor().token
  ) {
    throw new Error('evidence.get binding does not match the snapshot row')
  }
  return response(request, {
    kind: 'evidence_detail',
    artifactAccess: {
      state: 'available',
      items: [{
        artifactId: `art_${row.id.slice(-8)}`,
        digest: `sha256:${'c'.repeat(64)}`,
        fileName: `${row.type}-report.txt`,
        kind: 'report',
        mediaType: 'text/plain',
        previewMode: 'inline_text',
        provenance: {
          candidateRef: row.candidateRef,
          deliveryId,
          deliveryRevision: world.revision,
          evidenceId: row.id,
          sessionBindingId: row.sessionBindingId,
          stageRunId: row.stageRunId,
        },
        sizeBytes: 64,
      }],
    },
    evidence: row,
    outcome: row.id.endsWith('02') || row.id.endsWith('05') ? 'failed' : 'succeeded',
    readCursor: readCursor(),
  })
}

function historyFor(states) {
  return states.map((state, index) => ({
    cancellable: state === 'publishing',
    retryable: state === 'failed',
    revision: index + 1,
    state,
    stepStates: [
      { kind: 'branch', state: 'succeeded' },
      {
        kind: 'pull_request',
        state: state === 'failed'
          ? 'rejected'
          : state === 'publishing' ? 'applying' : 'succeeded',
      },
    ],
    updatedAt: `2026-09-03T01:${String(30 + index + 1).padStart(2, '0')}:00.000Z`,
  }))
}

function stepsFor(summary) {
  const branch = {
    kind: 'branch',
    outcomeCode: null,
    remoteWritePerformed: true,
    resourceRef: null,
    retryable: false,
    state: 'succeeded',
  }
  if (summary.state === 'publishing') {
    return [branch, {
      kind: 'pull_request',
      outcomeCode: null,
      remoteWritePerformed: false,
      resourceRef: null,
      retryable: false,
      state: 'applying',
    }]
  }
  if (summary.state === 'failed') {
    return [branch, {
      kind: 'pull_request',
      outcomeCode: world.state === 'approval-expired'
        ? 'publication.approval.expired'
        : 'RESOURCE_CONFLICT',
      remoteWritePerformed: false,
      resourceRef: null,
      retryable: world.state !== 'approval-expired',
      state: 'rejected',
    }]
  }
  return [branch, {
    kind: 'pull_request',
    outcomeCode: null,
    remoteWritePerformed: true,
    resourceRef: { kind: 'github_pull_request', number: 21, repository: 'winwincode/browser-fixture' },
    retryable: false,
    state: 'succeeded',
  }]
}

function deliverySummary() {
  return {
    schemaVersion,
    deliveryId,
    revision: world.revision,
    status: status(),
    title: 'UI-607 bounded review vertical',
    updatedAt: '2026-09-03T01:26:00.000Z',
    ownership: ownership(),
    activeStageRunId: stages().at(-1).id,
    openAttentionCount: attentionRecord().status === 'open' ? 1 : 0,
    taskCounts: {
      total: 1, pending: 0, active: 0, blocked: 0, verifying: 0, completed: 1, failed: 0,
    },
  }
}

function chatSession() {
  return {
    id: chatProductSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision: 1,
    state: 'idle',
    title: 'UI-607 fixture Chat',
    updatedAt: '2026-09-03T00:30:00.000Z',
  }
}

function chatRuntime() {
  return {
    kind: 'runtime_projection',
    productSessionId: chatProductSessionId,
    deliveryId: null,
    stageRunId: null,
    readCursor: null,
    eventCursor: {
      eventId: null,
      sequence: 0,
      scope,
      stream: { kind: 'product-session', productSessionId: chatProductSessionId },
    },
    lastProjectionSequence: 0,
    revision: 1,
    rebuiltAt: '2026-09-03T00:31:00.000Z',
    sessions: [],
  }
}

function browserSession() {
  return {
    schemaVersion,
    expiresAt: '2099-09-03T00:00:00.000Z',
    actor,
    authorizedScopes: [scope],
  }
}

function acceptCommand(request) {
  calls.commands.push(structuredClone(request))
  if (world.conflictOnceFor === request.command) {
    world.conflictOnceFor = null
    throw accessFailure(request, 'conflict', 'REVISION_CONFLICT', false)
  }
  if (request.expectedRevision !== world.revision) {
    throw accessFailure(request, 'conflict', 'REVISION_CONFLICT', false)
  }
}

async function publishChanged() {
  if (realtimeOptions === null) throw new Error('the StrongFlow subscription is not active')
  await realtimeOptions.onEvent({
    sequence: world.eventSequence,
    event: {
      type: 'delivery.changed.v1',
      deliveryId,
      revision: world.revision,
      changeKind: 'advanced',
    },
  })
}

/** Answers one typed query from the current read model. */
async function respond(request) {
    if (request.query === 'delivery.list') {
      return response(request, { kind: 'delivery_page', items: [deliverySummary()] })
    }
    if (request.query === 'session.list') {
      return response(request, { kind: 'product_session_page', items: [chatSession()] })
    }
    if (request.query === 'model.route.availability.list') {
      return response(request, {
        kind: 'model_route_availability_page',
        scope,
        settingsSource: scope,
        settingsRevision: 1,
        requestPoolSource: projectScope,
        requestPoolRevision: 1,
        defaultProviderId: modelRoute.providerId,
        defaultModelId: modelRoute.modelId,
        routes: [{
          providerId: modelRoute.providerId,
          modelId: modelRoute.modelId,
          displayName: 'Browser fixture model',
          contextWindowTokens: 128_000,
          maxOutputTokens: 16_000,
          toolSupport: 'parallel',
          reasoningEfforts: ['medium'],
          credentialRotationVersion: 1,
          isDefault: true,
          status: 'enabled',
          reason: 'ready',
        }],
      })
    }
    if (request.query === 'session.get') return response(request, chatSession())
    if (request.query === 'session.messages.list') {
      return response(request, { kind: 'chat_message_page', items: [] })
    }
    if (request.query === 'session.interactions.list') {
      return response(request, { kind: 'chat_interaction_page', items: [] })
    }
    if (request.query === 'approval.list') {
      return response(request, { kind: 'approval_page', items: [] })
    }
    if (request.query === 'settings.get') {
      return response(request, {
        revision: 1,
        workerConcurrencyLimit: 1,
        defaultModelRoute: modelRoute,
      })
    }
    if (request.query === 'credential.reference.list') {
      return response(request, {
        kind: 'credential_reference_page',
        items: [{
          id: credentialReferenceId,
          providerId: modelRoute.providerId,
          displayName: 'Browser model credential',
          secretState: 'available',
          rotationVersion: 1,
          lastRotatedAt: '2026-09-03T00:00:00.000Z',
          revokedAt: null,
          revision: 1,
          updatedAt: '2026-09-03T00:00:00.000Z',
        }],
      })
    }
    if (request.query === 'runtime.projection.get'
      && request.parameters.kind === 'product-session') {
      return response(request, chatRuntime())
    }
    if (request.query === 'delivery.get') {
      world.servedRevisions.push(world.revision)
      return response(request, deliveryDetail())
    }
    if (request.query === 'runtime.projection.get') {
      const active = stages().filter(entry => entry.sessionBinding !== null).at(-1)
      const requested = request.parameters.stageRunId
      if (request.parameters.kind === 'delivery-stage' && requested !== active.id) {
        const index = RUN_BINDING_INDEX.get(requested)
        if (index === undefined) throw new Error(`no runtime binding for ${requested}`)
        return response(request, historicalRuntime(requested, index))
      }
      return response(request, deliveryRuntime())
    }
    if (request.query === 'evidence.get') return evidenceDetail(request)
    if (request.query === 'candidate.list') {
      return response(request, {
        kind: 'candidate_history_page',
        items: world.frozenTags.map(tag => ({
          availability: 'available',
          candidate: candidateRecord(tag),
          firstSeenDeliveryRevision: tag === 'a' ? 7 : 12,
          isCurrentAtReadCursor: tag === world.candidateTag,
          lastSeenDeliveryRevision: world.revision,
          reviewDeliveryRevision: null,
        })),
        readCursor: readCursor(),
      })
    }
    if (request.query === 'candidate.review.get') {
      const parameters = request.parameters
      const tag = world.frozenTags.find(entry => (
        candidateRecord(entry).candidateRef === parameters.candidateRef
      ))
      const candidate = candidateRecord(tag)
      if (
        tag === undefined
        || parameters.candidateTreeId !== candidate.candidateTreeId
        || parameters.diffSha256 !== candidate.diffSha256
        || parameters.deliveryId !== deliveryId
        || parameters.atCursor.token !== readCursor().token
      ) throw new Error('candidate.review.get does not match the requested Candidate')
      return response(request, {
        availability: 'available',
        candidate,
        currentAuthorization: false,
        displayOnly: true,
        evidence: deliveryEvidence().filter(entry => entry.candidateRef === candidate.candidateRef),
        firstSeenDeliveryRevision: tag === 'a' ? 7 : 12,
        kind: 'candidate_historical_review',
        lastSeenDeliveryRevision: world.revision,
        readCursor: readCursor(),
        reviewDeliveryRevision: null,
        verdict: tag === world.candidateTag ? activeVerdict() : null,
      })
    }
    if (request.query === 'candidate.files.list') {
      const candidate = currentCandidate()
      if (
        request.parameters.candidateRef !== candidate.candidateRef
        || request.parameters.candidateTreeId !== candidate.candidateTreeId
        || request.parameters.diffSha256 !== candidate.diffSha256
        || request.parameters.deliveryId !== deliveryId
        || request.parameters.atCursor.token !== readCursor().token
      ) throw new Error('candidate.files.list does not match the current Candidate')
      return response(request, {
        kind: 'candidate_file_page',
        items: candidateFiles(),
        candidate,
        readCursor: readCursor(),
      })
    }
    if (request.query === 'candidate.diff.get') {
      const candidate = currentCandidate()
      const file = candidateFiles().find(entry => entry.path === request.parameters.path)
      if (file === undefined) throw new Error('unknown diff path')
      const bytes = new TextEncoder().encode(diffBody(file))
      return response(request, {
        kind: 'candidate_diff_chunk',
        binary: false,
        candidate,
        contentEncoding: 'utf-8',
        dataBase64: btoa(String.fromCharCode(...bytes)),
        encoding: 'base64',
        fileDiffSha256: `sha256:${'d'.repeat(64)}`,
        mediaType: 'application/vnd.winwincode.git-diff',
        nextOffset: null,
        offset: 0,
        oldPath: file.oldPath,
        path: file.path,
        readCursor: readCursor(),
        returnedBytes: bytes.byteLength,
        status: file.status,
        totalBytes: bytes.byteLength,
      })
    }
    if (request.query === 'publication.get') {
      world.receiptReads += 1
      const summary = publication()
      if (request.parameters.publicationId !== summary.id) {
        throw new Error('publication.get asked for another Publication')
      }
      return response(request, {
        cancellable: summary.state === 'publishing',
        cancellation: null,
        history: historyFor(publicationStates()),
        historyTruncated: false,
        kind: 'publication_detail',
        retryable: summary.state === 'failed' && world.state !== 'approval-expired',
        steps: stepsFor(summary),
        summary,
      })
    }
    throw new Error(`unexpected query: ${request.query}`)
}

const controlPlane = {
  serverUrl: 'https://control.localhost',
  async restore() { return structuredClone(browserSession()) },
  async login() { return structuredClone(browserSession()) },
  async logout() {},
  async command(request) {
    acceptCommand(request)
    const previous = world.revision
    world.revision += 1
    world.eventSequence += 1
    if (request.command === 'delivery.resolve_attention') {
      if (request.payload.remediation !== null) world.state = 'reworking'
      else world.state = 'candidate-superseded'
    } else if (request.command === 'delivery.submit_verdict') {
      world.state = 'ready-to-deliver'
    } else if (request.command === 'delivery.advance') {
      world.state = 'delivered'
    } else {
      throw new Error(`unexpected command: ${request.command}`)
    }
    await publishChanged()
    return completed(request, previous, world.revision)
  },
  async query(request) {
    calls.queries.push(structuredClone(request))
    try {
      return await respond(request)
    } catch (error) {
      calls.queryErrors.push(`${request.query}: ${error?.message ?? String(error)}`)
      throw error
    }
  },
  subscribe(options) {
    realtimeOptions = options
    calls.subscriptions.push(structuredClone({
      subscriptionId: options.subscriptionId,
      subscription: options.subscription,
      startAt: options.startAt,
    }))
    return {
      cursor: options.startAt,
      resume() {},
      reconnect() {},
      close() {},
    }
  },
  close() {},
}

const root = document.querySelector('[data-winwincode-client-root]')
mountWinWinCodeClient({ root, serverUrl: controlPlane.serverUrl, controlPlane })

async function waitFor(predicate, label) {
  const deadline = Date.now() + 15_000
  while (Date.now() < deadline) {
    let result = false
    try {
      result = await predicate()
    } catch {}
    if (result) return
    await new Promise(resolve => { setTimeout(resolve, 20) })
  }
  throw new Error(`timed out waiting for ${label}: ${diagnostic()}`)
}

function diagnostic() {
  return JSON.stringify({
    text: document.body.textContent.replace(/\s+/gu, ' ').trim().slice(0, 200),
    candidateTreeStatus: text('.wwc-candidate-file-tree-status'),
    candidateSummary: text('.wwc-candidate-file-summary'),
    candidateStatus: text('.wwc-candidate-file-status'),
    diffStatus: text('.wwc-candidate-diff-status'),
    historyStatus: text('.wwc-strongflow-history-runtime-status'),
    historyError: text('.wwc-strongflow-history-runtime-error'),
    candidateHistoryStatus: text('.wwc-strongflow-history-candidates-status'),
    reviewHistoryStatus: text('.wwc-strongflow-history-review-status'),
    reviewStatus: text('.wwc-strongflow-review-detail-receipt-status'),
    error: text('.wwc-strongflow-error-text'),
    queryErrors: calls.queryErrors.slice(-4),
    historyHidden: document.querySelector('.wwc-strongflow-history')?.hidden ?? null,
    historyPressed: document.querySelector('.wwc-strongflow-run-button[aria-pressed="true"]')?.dataset.stageRunId ?? null,
    tabs: [...document.querySelectorAll('.wwc-strongflow-artifact-tab')]
      .map(entry => `${entry.textContent}:${entry.getAttribute('aria-selected')}`),
    queries: calls.queries.slice(-8).map(entry => ({
      query: entry.query,
      kind: entry.parameters?.kind ?? null,
      stageRunId: entry.parameters?.stageRunId ?? null,
    })),
    commands: calls.commands.map(entry => entry.command),
  })
}

function text(selector) {
  return (document.querySelector(selector)?.textContent ?? '').replace(/\s+/gu, ' ').trim()
}

function selectArtifactTab(label) {
  const tab = [...document.querySelectorAll('.wwc-strongflow-artifact-tab')]
    .find(entry => entry.textContent === label)
  if (tab === undefined) throw new Error(`no ${label} artifact tab`)
  tab.click()
}

function reviewSection(id) {
  return [...document.querySelectorAll('.wwc-strongflow-review-detail-section')]
    .find(entry => entry.dataset.section === id)
}

function openReviewSection(id) {
  const section = reviewSection(id)
  if (section === undefined) throw new Error(`no ${id} review section`)
  const toggle = section.querySelector('.wwc-strongflow-review-detail-toggle')
  if (toggle === null) throw new Error(`no ${id} review section toggle`)
  if (toggle.getAttribute('aria-expanded') !== 'true') toggle.click()
  return section
}

function advanceControl() {
  return document.querySelector('.wwc-strongflow-advance-delivery')
}

function stagedNotes() {
  return [...document.querySelectorAll('.wwc-strongflow-review-draft-row')]
    .map(row => ({
      anchor: row.querySelector('.wwc-strongflow-review-draft-anchor-label')?.textContent ?? '',
      note: row.querySelector('.wwc-strongflow-review-draft-note-text')?.textContent ?? '',
      stale: row.querySelector('.wwc-strongflow-review-draft-stale-note')?.hidden === false,
      staleText: row.querySelector('.wwc-strongflow-review-draft-stale-note')?.textContent ?? '',
    }))
}

function stageNote(kindValue, anchorValue, noteText) {
  const kind = document.querySelector('.wwc-strongflow-review-draft-kind')
  kind.value = kindValue
  kind.dispatchEvent(new Event('change', { bubbles: true }))
  document.querySelector('.wwc-strongflow-review-draft-anchor').value = anchorValue
  document.querySelector('.wwc-strongflow-review-draft-note').value = noteText
  document.querySelector('.wwc-strongflow-review-draft-add').click()
}

function commandLog(name) {
  return calls.commands.filter(entry => entry.command === name)
}

function receiptFlag(label) {
  const match = publicationText()
    .match(new RegExp(`${label}\\s*(yes|no)`, 'u'))
  return match === null ? null : match[1]
}

function publicationText() {
  return text('.wwc-strongflow-review-detail-section[data-section="publication"]')
}

function criterionOutcomes() {
  return [...document.querySelectorAll(
    '.wwc-strongflow-review-detail-criteria > li',
  )].map(entry => entry.dataset.outcome)
}

function queryCount(name) {
  return calls.queries.filter(entry => entry.query === name).length
}

globalThis.ui607Ready = () => true

/**
 * A read model that claims `ready-to-deliver` while a required Criterion has not
 * passed. The Server never produces this, so it isolates the Client gate: the
 * workbench must offer no final approval and the command path must refuse even
 * when the hidden control is activated programmatically.
 */
globalThis.ui607HostileReadyClaim = async () => {
  const restoreState = world.state
  const restoreTag = world.candidateTag
  const restoreFrozen = [...world.frozenTags]
  const restoreRevisions = world.servedRevisions.length
  const claim = async () => {
    world.revision += 1
    world.eventSequence += 1
    await publishChanged()
    await waitFor(
      () => world.servedRevisions.length > restoreRevisions
        && world.servedRevisions.at(-1) >= world.revision,
      'the hostile read model snapshot',
    )
  }
  world.state = 'hostile-ready'
  await claim()
  const observed = {
    deliveryStatus: deliverySummary().status,
    advancePresent: advanceControl() !== null,
    advanceHidden: advanceControl()?.hidden ?? null,
    advanceDisabled: advanceControl()?.disabled ?? null,
    blockedHidden: document.querySelector('.wwc-strongflow-advance-blocked')?.hidden ?? null,
    blockedText: text('.wwc-strongflow-advance-blocked'),
    predicateUnmet: hasUnmetStrongFlowRequiredCriterion(
      { verdict: activeVerdict(), delivery: deliveryDetail() },
    ),
    predicateAdvance: canAdvanceStrongFlowDelivery(
      { verdict: activeVerdict(), delivery: deliveryDetail() },
    ),
    candidate: currentCandidate().candidateRef,
    advanceCommands: commandLog('delivery.advance').length,
  }
  // A hidden control still dispatches to a programmatic click, so this proves
  // the command path refuses on its own rather than relying on the DOM.
  advanceControl()?.click()
  await new Promise(resolve => { setTimeout(resolve, 60) })
  observed.advanceCommandsAfterClick = commandLog('delivery.advance').length
  observed.errorText = text('.wwc-strongflow-error-text')

  world.state = restoreState
  world.candidateTag = restoreTag
  world.frozenTags = restoreFrozen
  await claim()
  observed.restoredStatus = deliverySummary().status
  observed.restoredCandidate = currentCandidate().candidateRef
  return observed
}

/** Moves the read model to the next named Control Plane state. */
globalThis.ui607AdvanceServer = async name => {
  world.state = name
  if (name === 'candidate-superseded' && !world.frozenTags.includes('c')) {
    world.frozenTags.push('c')
    world.candidateTag = 'c'
  }
  if (name === 'verifying-b') {
    world.candidateTag = 'b'
    if (!world.frozenTags.includes('b')) world.frozenTags.push('b')
  }
  world.revision += 1
  world.eventSequence += 1
  await publishChanged()
  // The page has re-read the read model at (or past) the announced revision.
  await waitFor(
    () => world.servedRevisions.at(-1) >= world.revision,
    'the snapshot reload after the state change',
  )
  return { state: world.state, revision: world.revision, candidate: world.candidateTag }
}

/**
 * Scenario 1: the reviewer reads the real Candidate Diff and Evidence and sees
 * that two required criteria did not pass, with no final approval offered.
 */
globalThis.ui607ReviewCandidate = async () => {
  await waitFor(
    () => document.querySelector('.wwc-strongflow-heading')?.textContent
      === 'UI-607 bounded review vertical',
    'the StrongFlow workbench',
  )

  selectArtifactTab('Candidate')
  await waitFor(
    () => document.querySelectorAll('.wwc-candidate-file-row[data-kind="file"]').length === 3,
    'the Candidate changed-file inventory',
  )
  const filesBefore = queryCount('candidate.diff.get')
  document.querySelector(
    '.wwc-candidate-file-row[data-path="src/export/gateway.ts"]',
  ).click()
  await waitFor(
    () => text('.wwc-candidate-diff-content').includes('diff --git a/src/export/gateway.ts'),
    'the real unified Diff',
  )

  selectArtifactTab('Evidence')
  await waitFor(
    () => document.querySelectorAll('.wwc-strongflow-evidence-row').length === 3,
    'the Candidate Evidence rows',
  )
  document.querySelector(
    '[data-evidence-id="evd_00000000000000000000000002"] .wwc-strongflow-evidence-open',
  ).click()
  await waitFor(
    () => document.querySelector('.wwc-strongflow-evidence-detail')?.dataset.status === 'ready',
    'the Evidence detail drawer',
  )
  const detail = {
    outcome: text('.wwc-strongflow-evidence-detail-outcome'),
    candidate: text('.wwc-strongflow-evidence-detail-candidate'),
    evidenceId: document.querySelector('.wwc-strongflow-evidence-detail')?.dataset.evidenceId,
    hash: location.hash,
  }
  document.querySelector('.wwc-drawer-close')?.click()
  await waitFor(
    () => document.querySelector('.wwc-strongflow-evidence-drawer').hidden,
    'the Evidence drawer closing',
  )

  openReviewSection('criteria')
  const blocked = document.querySelector('.wwc-strongflow-advance-blocked')
  return {
    diff: {
      content: text('.wwc-candidate-diff-content'),
      summary: text('.wwc-candidate-file-summary'),
      diffQueries: queryCount('candidate.diff.get') - filesBefore,
      fileQueries: queryCount('candidate.files.list'),
      candidateRef: currentCandidate().candidateRef,
    },
    evidence: {
      ...detail,
      evidenceQueries: queryCount('evidence.get'),
    },
    gate: {
      deliveryStatus: deliverySummary().status,
      advancePresent: advanceControl() !== null,
      advanceHidden: advanceControl()?.hidden ?? null,
      blockedHidden: blocked?.hidden ?? null,
      blockedText: blocked?.textContent.replace(/\s+/gu, ' ').trim() ?? '',
      predicateUnmet: hasUnmetStrongFlowRequiredCriterion(
        { verdict: activeVerdict(), delivery: deliveryDetail() },
      ),
      predicateAdvance: canAdvanceStrongFlowDelivery(
        { verdict: activeVerdict(), delivery: deliveryDetail() },
      ),
      criteriaText: reviewSection('criteria').textContent.replace(/\s+/gu, ' ').trim(),
      criterionOutcomes: criterionOutcomes(),
      conclusion: text('.wwc-strongflow-review-detail-conclusion'),
      receiptReads: world.receiptReads,
    },
    commands: calls.commands.map(entry => entry.command),
  }
}

/**
 * Scenario 3: a Candidate that is superseded under the reviewer marks the note
 * stale, keeps the draft, and offers no submission onto the new Candidate.
 */
globalThis.ui607StaleCandidate = async () => {
  stageNote(
    'criterion',
    'criterion:2',
    'The redaction gate drops this host from every export.',
  )
  await waitFor(() => stagedNotes().length === 1, 'the staged note')
  const before = {
    notes: stagedNotes(),
    submitDisabled: document.querySelector('.wwc-strongflow-review-draft-submit').disabled,
    candidate: currentCandidate().candidateRef,
  }

  await globalThis.ui607AdvanceServer('candidate-superseded')
  await waitFor(() => stagedNotes()[0]?.stale === true, 'the stale note')
  const submit = document.querySelector('.wwc-strongflow-review-draft-submit')
  const resolveCommands = commandLog('delivery.resolve_attention').length
  const stale = {
    notes: stagedNotes(),
    submitDisabled: submit.disabled,
    banner: text('.wwc-strongflow-review-draft-stale-text'),
    candidateBefore: before.candidate,
    candidateNow: currentCandidate().candidateRef,
    resolveCommands,
  }
  submit.click()
  await new Promise(resolve => { setTimeout(resolve, 40) })

  // The reviewer explicitly drops the stale note; nothing was ever submitted.
  document.querySelector('.wwc-strongflow-review-draft-row-discard')?.click()
  await waitFor(() => stagedNotes().length === 0, 'the discarded stale note')
  return {
    before,
    stale,
    afterDiscard: {
      notes: stagedNotes(),
      resolveCommands: commandLog('delivery.resolve_attention').length,
    },
  }
}

/**
 * Scenario 4: staged review notes compose into exactly one bounded rework
 * command, survive a revision conflict, and land on the Candidate read.
 */
globalThis.ui607ApproveBoundedRework = async () => {
  stageNote(
    'criterion',
    'criterion:3',
    'Re-run the focused verification lane to completion.',
  )
  await waitFor(() => stagedNotes().length === 1, 'the staged verification note')

  const target = document.querySelector('.wwc-strongflow-review-draft-target')
  target.value = 'bounded-rework'
  target.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor(
    () => document.querySelector('.wwc-strongflow-review-draft-rework')?.hidden === false,
    'the bounded rework fields',
  )
  // The reviewer picks the exact bounded scope: one solution node and one Task.
  const attentionSelect = document.querySelector('.wwc-strongflow-review-draft-attention')
  const nodeSelect = document.querySelector('.wwc-strongflow-review-draft-node')
  const taskSelect = document.querySelector('.wwc-strongflow-review-draft-task')
  await waitFor(
    () => nodeSelect.options.length > 0 && taskSelect.options.length > 1,
    'the bounded scope choices',
  )
  nodeSelect.value = nodeId
  nodeSelect.dispatchEvent(new Event('change', { bubbles: true }))
  taskSelect.value = taskId
  taskSelect.dispatchEvent(new Event('change', { bubbles: true }))
  await waitFor(
    () => document.querySelector('.wwc-strongflow-review-draft-submit').disabled === false,
    'the bounded scope summary',
  )
  const scope = {
    attention: attentionSelect.selectedOptions[0]?.textContent ?? '',
    attentionId: attentionSelect.value,
    node: nodeSelect.selectedOptions[0]?.textContent ?? '',
    task: taskSelect.selectedOptions[0]?.textContent ?? '',
  }
  const stagedBeforeSubmit = stagedNotes()
  const finalScope = [...document.querySelectorAll('.wwc-strongflow-review-draft-scope li')]
    .map(entry => entry.textContent)

  // One transport revision conflict: the draft must survive it untouched.
  world.conflictOnceFor = 'delivery.resolve_attention'
  document.querySelector('.wwc-strongflow-review-draft-submit').click()
  await waitFor(
    () => text('.wwc-strongflow-error-text').includes('changed before the decision was saved'),
    'the revision conflict message',
  )
  const conflict = {
    errorText: text('.wwc-strongflow-error-text'),
    notes: stagedNotes(),
    stagedBeforeSubmit,
    commands: commandLog('delivery.resolve_attention').length,
  }

  const attemptsBefore = commandLog('delivery.resolve_attention').length
  document.querySelector('.wwc-strongflow-review-draft-submit').click()
  await waitFor(
    () => commandLog('delivery.resolve_attention').length === attemptsBefore + 1
      && stagedNotes().length === 0,
    'the accepted bounded rework command',
  )
  const sent = commandLog('delivery.resolve_attention').at(-1)
  const remediation = sent.payload.remediation
  const instructions = remediation === null ? null : JSON.parse(remediation.instructions)
  return {
    scope,
    finalScope,
    conflict,
    command: {
      name: sent.command,
      attentionItemId: sent.payload.attentionItemId,
      decision: sent.payload.decision,
      expectedRevision: sent.expectedRevision,
      remediationNode: instructions?.nodeId ?? null,
      remediationTask: remediation?.deliveryTaskId ?? null,
      remediationDigest: instructions?.candidateDigest ?? null,
      carriesNotes: (remediation?.instructions ?? '').includes('verification lane'),
      readCandidate: currentCandidate().diffSha256,
    },
    notesAfterSuccess: stagedNotes().length,
    commands: calls.commands.map(entry => entry.command),
  }
}

/**
 * Scenario 5: the reworked Candidate compares against its rework baseline, the
 * Verdict is computed, the final approval opens, and the Publication receipt is
 * read while the browser never writes to the provider.
 */
globalThis.ui607VerdictAndReceipt = async () => {
  await globalThis.ui607AdvanceServer('verifying-b')

  selectArtifactTab('Candidate')
  const comparisonFrom = document.querySelector('.wwc-strongflow-candidate-comparison-from')
  await waitFor(
    () => [...(comparisonFrom?.options ?? [])].length >= 2
      && document.querySelector('.wwc-strongflow-candidate-comparison-to') !== null,
    'the comparison choices',
  )
  const comparison = {
    from: comparisonFrom.value,
    candidateChoices: [...comparisonFrom.options].map(entry => entry.value),
    to: document.querySelector('.wwc-strongflow-candidate-comparison-to').value,
    files: text('.wwc-strongflow-candidate-comparison-files-summary'),
    verdict: text('.wwc-strongflow-candidate-comparison-verdict'),
    alertHidden: document.querySelector('.wwc-strongflow-candidate-comparison-alert').hidden,
    candidateListQueries: queryCount('candidate.list'),
    reviewQueries: queryCount('candidate.review.get'),
  }
  await waitFor(
    () => document.querySelector('.wwc-strongflow-submit-verdict') !== null,
    'the Verdict control after every StageRun settled',
  )
  const verdictsBefore = commandLog('delivery.submit_verdict').length
  document.querySelector('.wwc-strongflow-submit-verdict').click()
  await waitFor(
    () => commandLog('delivery.submit_verdict').length === verdictsBefore + 1,
    'the Verdict command',
  )
  const verdictCommand = commandLog('delivery.submit_verdict').at(-1)
  await waitFor(
    () => advanceControl()?.hidden === false,
    'the final Delivery approval after a passing Verdict',
  )
  const gate = {
    advanceHidden: advanceControl().hidden,
    blockedHidden: document.querySelector('.wwc-strongflow-advance-blocked').hidden,
    conclusion: text('.wwc-strongflow-review-detail-conclusion'),
    deliveryStatus: deliverySummary().status,
    predicateAdvance: canAdvanceStrongFlowDelivery(
      { verdict: activeVerdict(), delivery: deliveryDetail() },
    ),
  }
  const advancesBefore = commandLog('delivery.advance').length
  advanceControl().click()
  await waitFor(
    () => commandLog('delivery.advance').length === advancesBefore + 1,
    'the final Delivery approval command',
  )
  const advanceCommand = commandLog('delivery.advance').at(-1)

  openReviewSection('publication')
  await waitFor(
    () => publicationText().includes(publicationId),
    'the Publication receipt',
  )
  const readsAfterFirst = world.receiptReads
  const queriesAfterFirst = queryCount('publication.get')
  openReviewSection('publication')
  const receipt = {
    publicationId,
    text: publicationText(),
    queries: queryCount('publication.get'),
    reads: readsAfterFirst,
  }

  // The Publication coordinator denies the write on an expired approval and then
  // retries. The receipt is replayed from the first read until the reviewer
  // refreshes it, so each step below is an explicit refresh of the same receipt.
  const refreshReceipt = async label => {
    document.querySelector('.wwc-strongflow-review-detail-receipt-refresh')?.click()
    await waitFor(label, 'the refreshed Publication receipt')
  }
  await globalThis.ui607AdvanceServer('approval-expired')
  openReviewSection('publication')
  await refreshReceipt(
    () => publicationText().includes('approval.expired'),
  )
  const expired = {
    text: publicationText(),
    history: text('.wwc-strongflow-review-detail-receipt-history'),
    steps: text('.wwc-strongflow-review-detail-receipt-steps'),
    retryable: receiptFlag('Retryable'),
  }
  await globalThis.ui607AdvanceServer('retry-failed')
  await refreshReceipt(
    () => publicationText().includes('RESOURCE_CONFLICT'),
  )
  const retried = {
    text: publicationText(),
    retryable: receiptFlag('Retryable'),
  }
  await globalThis.ui607AdvanceServer('published')
  await refreshReceipt(
    () => publicationText().includes('winwincode/browser-fixture #21'),
  )
  return {
    comparison,
    verdictCommand: {
      name: verdictCommand.command,
      candidateDigest: verdictCommand.payload.candidateDigest,
      expectedRevision: verdictCommand.expectedRevision,
    },
    advanceCommand: {
      name: advanceCommand.command,
      expectedRevision: advanceCommand.expectedRevision,
    },
    gate,
    receipt,
    receiptStableAfterToggle: queryCount('publication.get') === queriesAfterFirst,
    expired,
    retried,
    published: {
      text: publicationText(),
      history: text('.wwc-strongflow-review-detail-receipt-history'),
      conclusion: text('.wwc-strongflow-review-detail-conclusion'),
      publicationQueries: queryCount('publication.get'),
    },
    writeCommands: calls.commands
      .filter(entry => entry.command.startsWith('publication.'))
      .map(entry => entry.command),
    commands: calls.commands.map(entry => entry.command),
    collectionLiveRegions: document.querySelectorAll(
      '.wwc-strongflow-review-detail [aria-live],'
        + '.wwc-strongflow-candidate-comparison [aria-live],'
        + '.wwc-strongflow-candidate-diff [aria-live],'
        + '.wwc-strongflow-evidence [aria-live],'
        + '.wwc-strongflow-review-draft [aria-live]',
    ).length,
  }
}
