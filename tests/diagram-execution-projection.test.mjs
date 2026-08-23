import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  RUNTIME_EVENT_SCHEMA_VERSION,
  parseDelivery,
  parseStrongFlowDiagramExecutionProjection,
  parseStrongFlowPlanReviewContextText,
} from '../packages/contracts/dist/index.js'
import {
  createStrongFlowPlanReviewAttention,
  createStrongFlowPlanReviewDecision,
  diagramExecutionAnnotationExists,
  freezeDeliveryCandidate,
  projectStrongFlowDiagramExecution,
} from '../packages/strongflow/dist/index.js'

const baseTime = 2_400_000_000_000
const deliveryId = 'delivery-diagram-cycle'
const exactDiff = [
  'diff --git a/src/invitations/api.ts b/src/invitations/api.ts',
  'index 1111111..2222222 100644',
  '--- a/src/invitations/api.ts',
  '+++ b/src/invitations/api.ts',
  '@@ -1 +1,2 @@',
  '-export const consume = false',
  '+export const consume = true',
  '+export const atomic = true',
  'diff --git a/docs/invitations.md b/docs/invitations.md',
  'new file mode 100644',
  'index 0000000..3333333',
  '--- /dev/null',
  '+++ b/docs/invitations.md',
  '@@ -0,0 +1 @@',
  '+Invitation behavior',
  '',
].join('\n')

function spec() {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `${deliveryId}:spec:1`,
    deliveryId,
    revision: 1,
    title: '可审核邀请流程',
    goal: '在同一组图上显示执行前、执行中和执行结束状态。',
    scope: ['邀请接口'],
    outOfScope: ['通用任务系统'],
    constraints: ['Codex Core remains the execution authority'],
    acceptanceCriteria: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `${deliveryId}:criterion:1`,
      description: '邀请令牌只能消费一次。',
      verificationMethod: '运行并发测试。',
      required: true,
    }],
    sourceRef: null,
    publicationTarget: null,
    repository: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      kind: 'local-git',
      locator: '/workspace/invitations',
    },
    baseRevision: '1'.repeat(40),
    maxReworkAttempts: 2,
    createdAtMillis: baseTime,
  }
}

function reviewFixture() {
  const planningRun = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `${deliveryId}:stage:planning`,
    deliveryId,
    deliveryTaskId: null,
    stage: 'planning',
    actorType: 'codex',
    role: 'planner',
    status: 'running',
    attempt: 1,
    startedAtMillis: baseTime + 1,
    finishedAtMillis: null,
  }
  const planningBinding = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `${deliveryId}:binding:planning`,
    deliveryId,
    stageRunId: planningRun.id,
    dshSessionId: `${deliveryId}:dsh:planning`,
    codexSessionId: `${deliveryId}:codex:planning`,
    boundAtMillis: baseTime + 2,
  }
  const planning = parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 2,
    status: 'planning',
    spec: spec(),
    tasks: [],
    stageRuns: [planningRun],
    sessionBindings: [planningBinding],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 2,
  })
  const attention = createStrongFlowPlanReviewAttention({
    delivery: planning,
    attentionItemId: `${deliveryId}:attention:plan-review`,
    reviewStageRunId: `${deliveryId}:stage:plan-review`,
    assignedTo: 'reviewer',
    solution: {
      id: `${deliveryId}:solution:1`,
      summary: '为邀请接口增加原子消费并补充文档。',
      approach: ['修改接口。', '验证并发。'],
      components: [{
        id: `${deliveryId}:component:invitations`,
        label: '邀请模块',
        responsibility: '签发并消费邀请令牌。',
        kind: 'component',
        trustBoundary: 'Application boundary',
        unresolved: false,
        repositoryPathPrefixes: ['src/invitations'],
      }],
      connections: [{
        id: `${deliveryId}:connection:invitations`,
        from: 'platform:repository',
        to: `${deliveryId}:component:invitations`,
        label: '保存邀请实现',
      }],
    },
    risks: [],
    unresolvedItems: [],
    preparedAtMillis: baseTime + 3,
  })
  const review = parseDelivery({
    ...planning,
    revision: 3,
    status: 'needs-attention',
    stageRuns: [{
      ...planningRun,
      status: 'succeeded',
      finishedAtMillis: baseTime + 4,
    }, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `${deliveryId}:stage:plan-review`,
      deliveryId,
      deliveryTaskId: null,
      stage: 'plan-review',
      actorType: 'human',
      role: 'reviewer',
      status: 'waiting',
      attempt: 1,
      startedAtMillis: baseTime + 4,
      finishedAtMillis: null,
    }],
    sessionBindings: [planningBinding, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `${deliveryId}:binding:plan-review`,
      deliveryId,
      stageRunId: `${deliveryId}:stage:plan-review`,
      dshSessionId: `${deliveryId}:dsh:plan-review`,
      codexSessionId: null,
      boundAtMillis: baseTime + 4,
    }],
    attentionItems: [attention],
    updatedAtMillis: baseTime + 4,
  })
  return { review, attention }
}

function executionFixture() {
  const { review, attention } = reviewFixture()
  const context = parseStrongFlowPlanReviewContextText(attention.context)
  const decision = createStrongFlowPlanReviewDecision({
    context,
    action: 'approve',
    comments: '批准。',
    requestedChanges: [],
  })
  const task = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `${deliveryId}:task:invitations`,
    deliveryId,
    title: '邀请模块',
    goal: '交付可独立验证的邀请接口。',
    acceptanceCriterionIds: [`${deliveryId}:criterion:1`],
    blockedByTaskIds: [],
    owner: 'executor',
    status: 'active',
  }
  const writer = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `${deliveryId}:stage:executing`,
    deliveryId,
    deliveryTaskId: task.id,
    stage: 'executing',
    actorType: 'codex',
    role: 'executor',
    status: 'running',
    attempt: 1,
    startedAtMillis: baseTime + 6,
    finishedAtMillis: null,
  }
  const writerBinding = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `${deliveryId}:binding:executing`,
    deliveryId,
    stageRunId: writer.id,
    dshSessionId: `${deliveryId}:dsh:executing`,
    codexSessionId: `${deliveryId}:codex:executing`,
    boundAtMillis: baseTime + 7,
  }
  const live = parseDelivery({
    ...review,
    revision: 5,
    status: 'executing',
    tasks: [task],
    stageRuns: [
      review.stageRuns[0],
      { ...review.stageRuns[1], status: 'succeeded', finishedAtMillis: baseTime + 5 },
      writer,
    ],
    sessionBindings: [...review.sessionBindings, writerBinding],
    attentionItems: [{
      ...attention,
      status: 'resolved',
      resolution: JSON.stringify(decision),
      resolvedBy: 'reviewer',
      resolvedAtMillis: baseTime + 5,
    }],
    updatedAtMillis: baseTime + 7,
  })
  return { live, writer, writerBinding, context, task }
}

function diffEvent(delivery, binding, role, extraData = {}) {
  return Object.freeze({
    schemaVersion: RUNTIME_EVENT_SCHEMA_VERSION,
    id: `${binding.dshSessionId}@1`,
    cursor: Object.freeze({ sessionId: binding.dshSessionId, sequence: '1' }),
    kind: 'diff.updated',
    source: Object.freeze({
      authority: 'codex-core',
      sessionId: binding.dshSessionId,
      kernelSessionId: binding.codexSessionId,
      roleId: role,
      kernelStreamId: `${deliveryId}:stream:execute`,
      kernelSequence: '1',
      submissionId: `${deliveryId}:submission:execute`,
      kernelKind: 'diff_updated',
    }),
    occurredAtMillis: delivery.updatedAtMillis,
    data: Object.freeze({ unified_diff: exactDiff, ...extraData }),
  })
}

function finishedFixture() {
  const { live, writer, writerBinding, context, task } = executionFixture()
  const finished = parseDelivery({
    ...live,
    revision: 6,
    status: 'verifying',
    tasks: [{ ...task, status: 'verifying' }],
    stageRuns: [
      ...live.stageRuns.slice(0, -1),
      { ...writer, status: 'succeeded', finishedAtMillis: baseTime + 10 },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: `${deliveryId}:stage:verifying`,
        deliveryId,
        deliveryTaskId: task.id,
        stage: 'verifying',
        actorType: 'codex',
        role: 'verifier',
        status: 'running',
        attempt: 1,
        startedAtMillis: baseTime + 11,
        finishedAtMillis: null,
      },
    ],
    sessionBindings: [...live.sessionBindings, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `${deliveryId}:binding:verifying`,
      deliveryId,
      stageRunId: `${deliveryId}:stage:verifying`,
      dshSessionId: `${deliveryId}:dsh:verifying`,
      codexSessionId: `${deliveryId}:codex:verifying`,
      boundAtMillis: baseTime + 11,
    }],
    updatedAtMillis: baseTime + 11,
  })
  const candidate = freezeDeliveryCandidate(finished, {
    producerStageRunId: writer.id,
    producerSessionBindingId: writerBinding.id,
    baseCommitId: finished.spec.baseRevision,
    baseTreeId: '2'.repeat(40),
    candidateCommitId: '3'.repeat(40),
    candidateTreeId: '4'.repeat(40),
    diffSha256: createHash('sha256').update(exactDiff).digest('hex'),
    changedPaths: [{
      path: 'src/invitations/api.ts',
      state: 'present',
      objectId: '5'.repeat(40),
    }, {
      path: 'docs/invitations.md',
      state: 'present',
      objectId: '6'.repeat(40),
    }],
  })
  return {
    finished,
    candidate,
    context,
    event: diffEvent(finished, writerBinding, writer.role, { frozen_candidate: candidate }),
  }
}

test('the approved diagram pair keeps stable nodes through all three execution states', () => {
  const { review } = reviewFixture()
  const before = projectStrongFlowDiagramExecution(review, {
    runtimeEvents: [],
    candidate: null,
  })
  const { live, writerBinding, writer } = executionFixture()
  const liveEvent = diffEvent(live, writerBinding, writer.role)
  const executing = projectStrongFlowDiagramExecution(live, {
    runtimeEvents: [liveEvent],
    candidate: null,
  })
  const { finished, candidate, event } = finishedFixture()
  const completed = projectStrongFlowDiagramExecution(finished, {
    runtimeEvents: [event],
    candidate,
  })

  assert.equal(before.state, 'before-execution')
  assert.ok([...before.architecture.nodes, ...before.process.nodes].every(node => (
    node.state === 'normal' && node.fileIds.length === 0
  )))
  assert.equal(executing.state, 'executing')
  assert.equal(executing.details, null)
  assert.equal(
    executing.architecture.nodes.find(node => node.nodeId.endsWith(':component:invitations')).state,
    'affected-live',
  )
  assert.equal(
    executing.process.nodes.find(node => node.nodeId === 'process:executing').state,
    'affected-live',
  )
  assert.doesNotMatch(JSON.stringify(executing), /src\/invitations\/api\.ts|Invitation behavior/u)
  assert.equal(completed.state, 'execution-finished')
  assert.deepEqual(
    completed.architecture.nodes.map(node => node.nodeId),
    before.architecture.nodes.map(node => node.nodeId),
  )
  assert.deepEqual(
    completed.process.nodes.map(node => node.nodeId),
    before.process.nodes.map(node => node.nodeId),
  )
  assert.equal(
    completed.architecture.nodes.find(node => node.nodeId.endsWith(':component:invitations')).state,
    'affected-finished',
  )
})

test('finished diagram details match the exact candidate files, hunks, totals, and provenance', () => {
  const { finished, candidate, context, event } = finishedFixture()
  const projection = projectStrongFlowDiagramExecution(finished, {
    runtimeEvents: [event],
    candidate,
  })
  assert.notEqual(projection.details, null)
  assert.equal(projection.affectedFileCount, 2)
  assert.deepEqual(projection.details.files.map(file => file.path), [
    'docs/invitations.md',
    'src/invitations/api.ts',
  ])
  assert.equal(projection.details.additions, 3)
  assert.equal(projection.details.deletions, 1)
  assert.equal(projection.details.hunks.length, 2)
  assert.match(projection.details.hunks[0].content, /^@@ -1 \+1,2 @@/u)
  assert.equal(projection.details.provenance.role, 'executor')
  assert.equal(projection.details.provenance.stage, 'executing')
  assert.equal(projection.details.provenance.attempt, 1)
  assert.equal(projection.details.provenance.codexSessionId, `${deliveryId}:codex:executing`)
  const component = projection.architecture.nodes.find(node => (
    node.nodeId.endsWith(':component:invitations')
  ))
  const mappedFile = projection.details.files.find(file => file.path === 'src/invitations/api.ts')
  const mappedHunk = projection.details.hunks.find(hunk => hunk.fileId === mappedFile.id)
  assert.equal(diagramExecutionAnnotationExists(projection, {
    diagramKind: 'system-architecture',
    diagramId: context.architectureDiagram.id,
    nodeId: component.nodeId,
    filePath: mappedFile.path,
    hunkSha256: mappedHunk.sha256,
  }), true)
  assert.equal(diagramExecutionAnnotationExists(projection, {
    diagramKind: 'system-architecture',
    diagramId: context.architectureDiagram.id,
    nodeId: component.nodeId,
    filePath: mappedFile.path,
    hunkSha256: '0'.repeat(64),
  }), false)
  assert.deepEqual(parseStrongFlowDiagramExecutionProjection(projection), projection)
})

test('a changed candidate diff fails before any finished detail is exposed', () => {
  const { finished, candidate, event } = finishedFixture()
  assert.throws(
    () => projectStrongFlowDiagramExecution(finished, {
      runtimeEvents: [{
        ...event,
        data: { ...event.data, unified_diff: `${exactDiff}+tampered\n` },
      }],
      candidate,
    }),
    /matching authoritative runtime diff/u,
  )
})

test('finished projection keeps identical hunk bodies distinct across files', () => {
  const duplicateDiff = [
    'diff --git a/src/one.ts b/src/one.ts',
    '--- a/src/one.ts',
    '+++ b/src/one.ts',
    '@@ -1 +1 @@',
    '-same',
    '+changed',
    'diff --git a/src/two.ts b/src/two.ts',
    '--- a/src/two.ts',
    '+++ b/src/two.ts',
    '@@ -1 +1 @@',
    '-same',
    '+changed',
    '',
  ].join('\n')
  const { finished, candidate: original } = finishedFixture()
  const candidate = freezeDeliveryCandidate(finished, {
    producerStageRunId: original.producerStageRunId,
    producerSessionBindingId: original.producerSessionBindingId,
    baseCommitId: original.baseCommitId,
    baseTreeId: original.baseTreeId,
    candidateCommitId: '7'.repeat(40),
    candidateTreeId: '8'.repeat(40),
    diffSha256: createHash('sha256').update(duplicateDiff).digest('hex'),
    changedPaths: [{
      path: 'src/one.ts',
      state: 'present',
      objectId: '9'.repeat(40),
    }, {
      path: 'src/two.ts',
      state: 'present',
      objectId: 'a'.repeat(40),
    }],
  })
  const binding = finished.sessionBindings.find(entry => (
    entry.id === candidate.producerSessionBindingId
  ))
  const projection = projectStrongFlowDiagramExecution(finished, {
    runtimeEvents: [diffEvent(finished, binding, 'executor', {
      unified_diff: duplicateDiff,
      frozen_candidate: candidate,
    })],
    candidate,
  })

  assert.equal(projection.details.hunks.length, 2)
  assert.equal(projection.details.hunks[0].sha256, projection.details.hunks[1].sha256)
  assert.notEqual(projection.details.hunks[0].id, projection.details.hunks[1].id)
})

test('finished projection decodes Git UTF-8 quoted paths exactly', () => {
  const unicodeDiff = [
    'diff --git "a/src/\\351\\202\\200\\350\\257\\267/api.ts" "b/src/\\351\\202\\200\\350\\257\\267/api.ts"',
    '--- "a/src/\\351\\202\\200\\350\\257\\267/api.ts"',
    '+++ "b/src/\\351\\202\\200\\350\\257\\267/api.ts"',
    '@@ -1 +1 @@',
    '-false',
    '+true',
    '',
  ].join('\n')
  const { finished, candidate: original } = finishedFixture()
  const candidate = freezeDeliveryCandidate(finished, {
    producerStageRunId: original.producerStageRunId,
    producerSessionBindingId: original.producerSessionBindingId,
    baseCommitId: original.baseCommitId,
    baseTreeId: original.baseTreeId,
    candidateCommitId: 'b'.repeat(40),
    candidateTreeId: 'c'.repeat(40),
    diffSha256: createHash('sha256').update(unicodeDiff).digest('hex'),
    changedPaths: [{
      path: 'src/邀请/api.ts',
      state: 'present',
      objectId: 'd'.repeat(40),
    }],
  })
  const binding = finished.sessionBindings.find(entry => (
    entry.id === candidate.producerSessionBindingId
  ))
  const projection = projectStrongFlowDiagramExecution(finished, {
    runtimeEvents: [diffEvent(finished, binding, 'executor', {
      unified_diff: unicodeDiff,
      frozen_candidate: candidate,
    })],
    candidate,
  })

  assert.equal(projection.details.files[0].path, 'src/邀请/api.ts')
})
