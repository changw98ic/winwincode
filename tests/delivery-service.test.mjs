import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  RUNTIME_EVENT_SCHEMA_VERSION,
  STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
  parseDelivery,
  parseStrongFlowPlanReviewContextText,
} from '../packages/contracts/dist/index.js'
import { CodexRuntimeProjector } from '../packages/dsh-profile/dist/index.js'
import {
  DeliveryStore,
  StrongFlowService,
  StrongFlowServiceError,
  createStrongFlowPlanReviewAttention,
  createStrongFlowPlanReviewDecision,
  freezeDeliveryCandidate,
} from '../packages/strongflow/dist/index.js'

const baseTime = 1_800_000_000_000
const candidateDiff = [
  'diff --git a/src/value.ts b/src/value.ts',
  'index 1111111..5555555 100644',
  '--- a/src/value.ts',
  '+++ b/src/value.ts',
  '@@ -1 +1 @@',
  '-old',
  '+new',
  '',
].join('\n')

const authenticator = Object.freeze({
  async authenticate(request) {
    return request.channel === 'local-ui'
      && request.authentication.scheme === 'local-session'
      && request.authentication.proof === 'fixture-proof-value'
      ? Object.freeze({ actorId: 'reviewer-1' })
      : undefined
  },
})

function spec(deliveryId, revision, suffix = `v${revision}`) {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `delivery-spec-${suffix}`,
    deliveryId,
    revision,
    title: `Delivery ${suffix}`,
    goal: `Produce the ${suffix} delivery outcome.`,
    scope: [`Scope ${suffix}`],
    outOfScope: ['General project management'],
    constraints: ['Codex remains the execution authority'],
    acceptanceCriteria: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `criterion-${suffix}`,
      description: `The ${suffix} observable outcome passes.`,
      verificationMethod: `Run the ${suffix} direct check.`,
      required: true,
    }],
    sourceRef: null,
    publicationTarget: null,
    repository: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      kind: 'local-git',
      locator: '/workspace/repository',
    },
    baseRevision: '0123456789012345678901234567890123456789',
    maxReworkAttempts: 2,
    createdAtMillis: baseTime + revision,
  }
}

function taskFor(deliveryId, criterionId, status = 'pending') {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'delivery-task-main',
    deliveryId,
    title: 'Independent delivery task',
    goal: 'Produce one independently verifiable result.',
    acceptanceCriterionIds: [criterionId],
    blockedByTaskIds: [],
    owner: 'owner-1',
    status,
  }
}

function openAttention({
  id,
  deliveryId,
  deliverySpecId,
  stageRunId,
  type,
  title,
  createdAtMillis,
}) {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id,
    deliveryId,
    deliverySpecId,
    stageRunId,
    type,
    title,
    context: `${title} before StrongFlow can continue.`,
    options: [],
    assignedTo: 'reviewer-1',
    blocking: true,
    status: 'open',
    resolution: null,
    resolvedBy: null,
    createdAtMillis,
    resolvedAtMillis: null,
  }
}

function planReviewSolution(suffix = 'main') {
  return {
    id: `solution-${suffix}`,
    summary: 'Use the approved DeliverySpec to implement one bounded repository change.',
    approach: ['Keep DSH as the product shell.', 'Run every code action through Codex Core.'],
    components: [{
      id: `component-${suffix}`,
      label: 'Delivery feature',
      responsibility: 'Produce the user-visible result covered by the acceptance criterion.',
      kind: 'component',
      trustBoundary: 'Repository application',
      unresolved: false,
      repositoryPathPrefixes: ['src'],
    }],
    connections: [{
      id: `connection-${suffix}`,
      from: 'platform:codex-core',
      to: `component-${suffix}`,
      label: 'Implements the approved solution',
    }],
  }
}

function planReviewDecision(attention, action = 'approve', options = {}) {
  return createStrongFlowPlanReviewDecision({
    context: parseStrongFlowPlanReviewContextText(attention.context),
    action,
    comments: options.comments ?? 'Reviewed the exact current plan-review set.',
    requestedChanges: options.requestedChanges ?? [],
  })
}

async function fixture(t, name) {
  const home = await mkdtemp(join(tmpdir(), `winwincode-delivery-service-${name}-`))
  t.after(() => rm(home, { recursive: true, force: true }))
  let now = baseTime + 100
  let diagramFacts = Object.freeze({ runtimeEvents: Object.freeze([]), candidate: null })
  return {
    home,
    service: new StrongFlowService({
      home,
      authenticator,
      clock: () => ++now,
      executionSource: {
        async read() { return diagramFacts },
      },
    }),
    setDiagramFacts(value) { diagramFacts = value },
    readDiagramFacts() { return diagramFacts },
  }
}

function expectServiceError(code) {
  return error => error instanceof StrongFlowServiceError && error.code === code
}

function candidateForDelivery(delivery, producerStageRunId, producerSessionBindingId) {
  return freezeDeliveryCandidate(delivery, {
    producerStageRunId,
    producerSessionBindingId,
    baseCommitId: delivery.spec.baseRevision,
    baseTreeId: '2'.repeat(40),
    candidateCommitId: '3'.repeat(40),
    candidateTreeId: '4'.repeat(40),
    diffSha256: createHash('sha256').update(candidateDiff).digest('hex'),
    changedPaths: [{
      path: 'src/value.ts',
      state: 'present',
      objectId: '5'.repeat(40),
    }],
  })
}

function candidateRuntimeEvent(delivery, candidate) {
  const binding = delivery.sessionBindings.find(entry => (
    entry.id === candidate.producerSessionBindingId
  ))
  const run = delivery.stageRuns.find(entry => entry.id === candidate.producerStageRunId)
  assert.notEqual(binding, undefined)
  assert.notEqual(binding.dshSessionId, null)
  assert.notEqual(binding.codexSessionId, null)
  assert.notEqual(run, undefined)
  return Object.freeze({
    schemaVersion: RUNTIME_EVENT_SCHEMA_VERSION,
    id: `${binding.dshSessionId}@1`,
    cursor: Object.freeze({ sessionId: binding.dshSessionId, sequence: '1' }),
    kind: 'diff.updated',
    source: Object.freeze({
      authority: 'codex-core',
      sessionId: binding.dshSessionId,
      kernelSessionId: binding.codexSessionId,
      roleId: run.role,
      kernelStreamId: `stream-${run.id}`,
      kernelSequence: '1',
      submissionId: `submission-${run.id}`,
      kernelKind: 'diff_updated',
    }),
    occurredAtMillis: run.finishedAtMillis,
    data: Object.freeze({
      unified_diff: candidateDiff,
      frozen_candidate: candidate,
    }),
  })
}

function kernelEvent(sequence, type, data = {}) {
  const payload = { id: `submission-${sequence}`, msg: { type, ...data } }
  return Object.freeze({
    sequence: BigInt(sequence),
    kind: type,
    payload,
    rawJson: JSON.stringify(payload),
  })
}

function readOnlySessionConfiguration() {
  return {
    approval_policy: 'on-request',
    approvals_reviewer: 'user',
    permission_profile: {
      type: 'managed',
      file_system: {
        type: 'restricted',
        entries: [{ path: { type: 'special', value: { kind: 'root' } }, access: 'read' }],
      },
      network: 'restricted',
    },
  }
}

function verdictRuntimeEvents(delivery, candidate, verdict) {
  return delivery.stageRuns
    .filter(run => run.stage === 'verifying' && (run.role === 'reviewer' || run.role === 'verifier'))
    .flatMap((run) => {
      const binding = delivery.sessionBindings.find(entry => entry.stageRunId === run.id)
      assert.notEqual(binding, undefined)
      assert.notEqual(binding.dshSessionId, null)
      assert.notEqual(binding.codexSessionId, null)
      const type = run.role === 'reviewer' ? 'command' : 'test'
      const events = [
        kernelEvent(1, 'session_configured', {
          session_id: binding.codexSessionId,
          thread_id: binding.codexSessionId,
          occurred_at_ms: binding.boundAtMillis,
          ...readOnlySessionConfiguration(),
        }),
        kernelEvent(2, 'task_started', {
          turn_id: `turn-${run.role}`,
          started_at_ms: binding.boundAtMillis,
        }),
        kernelEvent(3, 'item_completed', {
          turn_id: `turn-${run.role}`,
          completed_at_ms: binding.boundAtMillis,
          item: {
            type: 'CommandExecution',
            id: `check-${run.role}`,
            command: run.role === 'reviewer' ? ['git', 'diff', '--check'] : ['pnpm', 'test'],
            status: verdict === 'fail' ? 'failed' : 'completed',
            exit_code: verdict === 'fail' ? 1 : 0,
          },
        }),
        kernelEvent(4, 'agent_message', {
          turn_id: `turn-${run.role}`,
          occurred_at_ms: binding.boundAtMillis,
          phase: 'final_answer',
          message: JSON.stringify({
            protocol: 'winwincode.independent-verification-result.v1',
            delivery_spec_id: delivery.spec.id,
            delivery_spec_revision: delivery.spec.revision,
            candidate_ref: candidate.candidateRef,
            findings: [{
              finding_id: `finding-${run.role}`,
              criterion_id: delivery.spec.acceptanceCriteria[0].id,
              verdict,
              explanation: `${run.role} evaluated the current candidate.`,
              evidence_sources: [{
                type,
                event_id: `${binding.dshSessionId}@3`,
              }],
            }],
          }),
        }),
        kernelEvent(5, 'task_complete', {
          turn_id: `turn-${run.role}`,
          completed_at_ms: binding.boundAtMillis,
          last_agent_message: `${run.role} complete`,
          error: null,
        }),
      ]
      return new CodexRuntimeProjector({
        sessionId: binding.dshSessionId,
        kernelSessionId: binding.codexSessionId,
        roleId: run.role,
        kernelStreamId: `stream-${run.role}`,
      }).replay(events)
    })
}

function verdictSubmission(delivery, producerStageRunId, producerSessionBindingId, verdict) {
  const candidate = candidateForDelivery(
    delivery,
    producerStageRunId,
    producerSessionBindingId,
  )
  return Object.freeze({
    candidate,
    runtimeEvents: verdictRuntimeEvents(delivery, candidate, verdict),
    requiredRoles: Object.freeze(['reviewer', 'verifier']),
  })
}

async function seedDelivery(home, name, snapshot) {
  return DeliveryStore.create({
    home,
    requestId: `seed-${name}`,
    requestDigest: 'c'.repeat(64),
    snapshot: parseDelivery(snapshot),
  })
}

test('StrongFlowService creates, revises, starts, binds, replays, and reopens Delivery', async t => {
  const fixtureValue = await fixture(t, 'lifecycle')
  const deliveryId = 'delivery-service-lifecycle'
  const firstSpec = spec(deliveryId, 1)
  const createInput = {
    requestId: 'create-lifecycle',
    spec: firstSpec,
    tasks: [taskFor(deliveryId, firstSpec.acceptanceCriteria[0].id)],
  }
  const created = await fixtureValue.service.createDelivery(createInput)
  assert.equal(created.status, 'draft')
  assert.equal(created.revision, 1)
  assert.deepEqual(await fixtureValue.service.createDelivery(createInput), created)

  await assert.rejects(
    fixtureValue.service.createDelivery({
      ...createInput,
      requestId: 'create-conflict',
      spec: { ...firstSpec, title: 'Changed title' },
    }),
    expectServiceError('DELIVERY_CONFLICT'),
  )

  const updateInput = {
    requestId: 'update-lifecycle-spec',
    deliveryId,
    expectedRevision: 1,
    spec: spec(deliveryId, 2),
  }
  const ready = await fixtureValue.service.updateDeliverySpec(updateInput)
  assert.equal(ready.status, 'ready')
  assert.equal(ready.revision, 2)
  assert.deepEqual(ready.tasks, [])
  assert.deepEqual(await fixtureValue.service.updateDeliverySpec(updateInput), ready)

  const planning = await fixtureValue.service.startStage({
    requestId: 'start-planning',
    deliveryId,
    expectedRevision: 2,
    stageRunId: 'stage-planning-1',
    deliveryTaskId: null,
    stage: 'planning',
    actorType: 'codex',
    role: 'planner',
    attention: null,
  })
  assert.equal(planning.status, 'planning')
  assert.equal(planning.stageRuns[0].attempt, 1)
  assert.equal(planning.stageRuns[0].status, 'running')

  const bound = await fixtureValue.service.bindSession({
    requestId: 'bind-planner',
    deliveryId,
    expectedRevision: 3,
    bindingId: 'binding-planner-1',
    stageRunId: 'stage-planning-1',
    dshSessionId: 'dsh-planner-1',
    codexSessionId: 'codex-planner-1',
  })
  assert.equal(bound.revision, 4)
  assert.equal(bound.sessionBindings[0].codexSessionId, 'codex-planner-1')
  await assert.rejects(
    fixtureValue.service.startStage({
      requestId: 'start-second-planner',
      deliveryId,
      expectedRevision: 4,
      stageRunId: 'stage-planning-2',
      deliveryTaskId: null,
      stage: 'planning',
      actorType: 'codex',
      role: 'planner',
      attention: null,
    }),
    expectServiceError('WRONG_DELIVERY_STATE'),
  )
  await assert.rejects(
    fixtureValue.service.bindSession({
      requestId: 'bind-stale',
      deliveryId,
      expectedRevision: 3,
      bindingId: 'binding-stale',
      stageRunId: 'stage-planning-1',
      dshSessionId: 'dsh-stale',
      codexSessionId: 'codex-stale',
    }),
    expectServiceError('REVISION_CONFLICT'),
  )

  const restarted = new StrongFlowService({ home: fixtureValue.home, authenticator })
  assert.deepEqual((await restarted.getDeliveryProjection(deliveryId)).delivery, bound)
})

test('StrongFlowService completes the reviewed Delivery lifecycle through atomic stage handoffs', async t => {
  const current = await fixture(t, 'reviewed-lifecycle')
  const deliveryId = 'delivery-service-reviewed-lifecycle'
  await current.service.createDelivery({
    requestId: 'reviewed-create',
    spec: spec(deliveryId, 1, 'reviewed-draft'),
    tasks: [],
  })
  const ready = await current.service.updateDeliverySpec({
    requestId: 'reviewed-spec',
    deliveryId,
    expectedRevision: 1,
    spec: spec(deliveryId, 2, 'reviewed-approved'),
  })
  assert.equal(ready.status, 'ready')

  const planning = await current.service.startStage({
    requestId: 'reviewed-start-planning',
    deliveryId,
    expectedRevision: 2,
    stageRunId: 'reviewed-stage-planning',
    deliveryTaskId: null,
    stage: 'planning',
    actorType: 'codex',
    role: 'planner',
    attention: null,
  })
  assert.equal(planning.status, 'planning')
  const boundPlanning = await current.service.bindSession({
    requestId: 'reviewed-bind-planning',
    deliveryId,
    expectedRevision: 3,
    bindingId: 'reviewed-binding-planning',
    stageRunId: 'reviewed-stage-planning',
    dshSessionId: 'reviewed-dsh-planning',
    codexSessionId: 'reviewed-codex-planning',
  })

  const planReviewAttention = createStrongFlowPlanReviewAttention({
    delivery: boundPlanning,
    attentionItemId: 'reviewed-attention-plan',
    reviewStageRunId: 'reviewed-stage-plan-review',
    assignedTo: 'reviewer-1',
    solution: planReviewSolution('reviewed'),
    risks: ['The candidate must keep the current repository boundary.'],
    unresolvedItems: [],
    preparedAtMillis: boundPlanning.updatedAtMillis,
  })
  await assert.rejects(
    current.service.startStage({
      requestId: 'reviewed-start-plan-review-without-attention',
      deliveryId,
      expectedRevision: 4,
      stageRunId: 'reviewed-stage-plan-review-missing-attention',
      deliveryTaskId: null,
      stage: 'plan-review',
      actorType: 'human',
      role: 'reviewer',
      attention: null,
    }),
    expectServiceError('ATTENTION_REQUIRED'),
  )
  const planReview = await current.service.startStage({
    requestId: 'reviewed-start-plan-review',
    deliveryId,
    expectedRevision: 4,
    stageRunId: 'reviewed-stage-plan-review',
    deliveryTaskId: null,
    stage: 'plan-review',
    actorType: 'human',
    role: 'reviewer',
    attention: planReviewAttention,
  })
  assert.equal(planReview.status, 'needs-attention')
  assert.equal(planReview.stageRuns[0].status, 'succeeded')
  assert.equal(planReview.stageRuns[1].status, 'waiting')
  assert.equal(planReview.attentionItems[0].status, 'open')
  await current.service.bindSession({
    requestId: 'reviewed-bind-plan-review',
    deliveryId,
    expectedRevision: 5,
    bindingId: 'reviewed-binding-plan-review',
    stageRunId: 'reviewed-stage-plan-review',
    dshSessionId: 'reviewed-dsh-human-review',
    codexSessionId: null,
  })
  const approvedPlan = await current.service.resolveAttention({
    requestId: 'reviewed-approve-plan',
    deliveryId,
    expectedRevision: 6,
    attentionItemId: planReviewAttention.id,
    status: 'resolved',
    resolution: JSON.stringify(planReviewDecision(planReviewAttention)),
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
  })
  assert.equal(approvedPlan.status, 'executing')
  assert.equal(approvedPlan.stageRuns[1].status, 'succeeded')

  await current.service.startStage({
    requestId: 'reviewed-start-execution',
    deliveryId,
    expectedRevision: 7,
    stageRunId: 'reviewed-stage-executing',
    deliveryTaskId: null,
    stage: 'executing',
    actorType: 'codex',
    role: 'executor',
    attention: null,
  })
  await current.service.bindSession({
    requestId: 'reviewed-bind-execution',
    deliveryId,
    expectedRevision: 8,
    bindingId: 'reviewed-binding-execution',
    stageRunId: 'reviewed-stage-executing',
    dshSessionId: 'reviewed-dsh-execution',
    codexSessionId: 'reviewed-codex-execution',
  })
  await assert.rejects(
    current.service.startStage({
      requestId: 'reviewed-start-invalid-verification',
      deliveryId,
      expectedRevision: 9,
      stageRunId: 'reviewed-stage-invalid-verifying',
      deliveryTaskId: null,
      stage: 'verifying',
      actorType: 'codex',
      role: 'executor',
      attention: null,
    }),
    expectServiceError('INVALID_REQUEST'),
  )
  await current.service.startStage({
    requestId: 'reviewed-start-review',
    deliveryId,
    expectedRevision: 9,
    stageRunId: 'reviewed-stage-reviewing',
    deliveryTaskId: null,
    stage: 'verifying',
    actorType: 'codex',
    role: 'reviewer',
    attention: null,
  })
  await current.service.bindSession({
    requestId: 'reviewed-bind-review',
    deliveryId,
    expectedRevision: 10,
    bindingId: 'reviewed-binding-review',
    stageRunId: 'reviewed-stage-reviewing',
    dshSessionId: 'reviewed-dsh-review',
    codexSessionId: 'reviewed-codex-review',
  })
  const verifying = await current.service.startStage({
    requestId: 'reviewed-start-verification',
    deliveryId,
    expectedRevision: 11,
    stageRunId: 'reviewed-stage-verifying',
    deliveryTaskId: null,
    stage: 'verifying',
    actorType: 'codex',
    role: 'verifier',
    attention: null,
  })
  assert.equal(verifying.status, 'verifying')
  assert.equal(verifying.stageRuns[2].status, 'succeeded')
  assert.equal(verifying.stageRuns[3].status, 'succeeded')
  assert.equal(verifying.stageRuns[4].status, 'running')
  assert.equal(verifying.stageRuns[3].attempt, 1)
  assert.equal(verifying.stageRuns[4].attempt, 1)
  const verificationBound = await current.service.bindSession({
    requestId: 'reviewed-bind-verification',
    deliveryId,
    expectedRevision: 12,
    bindingId: 'reviewed-binding-verification',
    stageRunId: 'reviewed-stage-verifying',
    dshSessionId: 'reviewed-dsh-verification',
    codexSessionId: 'reviewed-codex-verification',
  })

  const currentSpec = ready.spec
  const submission = verdictSubmission(
    verificationBound,
    'reviewed-stage-executing',
    'reviewed-binding-execution',
    'pass',
  )
  const readyToDeliver = await current.service.submitVerdict({
    requestId: 'reviewed-submit-verdict',
    deliveryId,
    expectedRevision: 13,
    ...submission,
  })
  assert.equal(readyToDeliver.status, 'ready-to-deliver')

  const deliveryAttention = openAttention({
    id: 'reviewed-attention-delivery',
    deliveryId,
    deliverySpecId: currentSpec.id,
    stageRunId: 'reviewed-stage-delivery-review',
    type: 'delivery_approval',
    title: 'Approve the verified delivery',
    createdAtMillis: baseTime + 90,
  })
  await assert.rejects(
    current.service.startStage({
      requestId: 'reviewed-bypass-delivery-review',
      deliveryId,
      expectedRevision: 14,
      stageRunId: 'reviewed-stage-bypass-delivery-review',
      deliveryTaskId: null,
      stage: 'executing',
      actorType: 'codex',
      role: 'executor',
      attention: null,
    }),
    expectServiceError('WRONG_DELIVERY_STATE'),
  )
  const deliveryReview = await current.service.startStage({
    requestId: 'reviewed-start-delivery-review',
    deliveryId,
    expectedRevision: 14,
    stageRunId: 'reviewed-stage-delivery-review',
    deliveryTaskId: null,
    stage: 'delivery-review',
    actorType: 'human',
    role: 'approver',
    attention: deliveryAttention,
  })
  assert.equal(deliveryReview.status, 'needs-attention')
  assert.equal(deliveryReview.verdict.status, 'pass')
  await current.service.bindSession({
    requestId: 'reviewed-bind-delivery-review',
    deliveryId,
    expectedRevision: 15,
    bindingId: 'reviewed-binding-delivery-review',
    stageRunId: 'reviewed-stage-delivery-review',
    dshSessionId: 'reviewed-dsh-human-review',
    codexSessionId: null,
  })
  const delivered = await current.service.resolveAttention({
    requestId: 'reviewed-approve-delivery',
    deliveryId,
    expectedRevision: 16,
    attentionItemId: deliveryAttention.id,
    status: 'resolved',
    resolution: 'The verified candidate is approved for delivery.',
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
  })
  assert.equal(delivered.revision, 17)
  assert.equal(delivered.status, 'delivered')
  assert.equal(delivered.verdict.status, 'pass')
  assert.equal(delivered.stageRuns.every(run => run.status === 'succeeded'), true)
  assert.equal(delivered.attentionItems.every(item => item.status === 'resolved'), true)
  assert.deepEqual(
    delivered.sessionBindings
      .filter(binding => binding.codexSessionId === null)
      .map(binding => binding.dshSessionId),
    ['reviewed-dsh-human-review', 'reviewed-dsh-human-review'],
  )

  const restarted = new StrongFlowService({ home: current.home, authenticator })
  assert.deepEqual((await restarted.getDeliveryProjection(deliveryId)).delivery, delivered)
})

test('StrongFlowService rejects a blocked DeliveryTask before changing durable state', async t => {
  const current = await fixture(t, 'blocked-task')
  const deliveryId = 'delivery-service-blocked-task'
  const currentSpec = spec(deliveryId, 1, 'blocked-task')
  const prerequisite = {
    ...taskFor(deliveryId, currentSpec.acceptanceCriteria[0].id),
    id: 'delivery-task-prerequisite',
    title: 'Prerequisite task',
  }
  const blocked = {
    ...taskFor(deliveryId, currentSpec.acceptanceCriteria[0].id),
    id: 'delivery-task-blocked',
    title: 'Blocked task',
    blockedByTaskIds: [prerequisite.id],
  }
  await seedDelivery(current.home, 'blocked-task', {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'executing',
    spec: currentSpec,
    tasks: [prerequisite, blocked],
    stageRuns: [],
    sessionBindings: [],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 10,
  })

  await assert.rejects(
    current.service.startStage({
      requestId: 'start-blocked-task',
      deliveryId,
      expectedRevision: 1,
      stageRunId: 'stage-blocked-task',
      deliveryTaskId: blocked.id,
      stage: 'executing',
      actorType: 'codex',
      role: 'executor',
      attention: null,
    }),
    expectServiceError('WRONG_DELIVERY_STATE'),
  )
  const unchanged = await current.service.getDeliveryProjection(deliveryId)
  assert.equal(unchanged.delivery.revision, 1)
  assert.deepEqual(unchanged.delivery.stageRuns, [])
  assert.equal(unchanged.delivery.tasks[1].status, 'pending')
})

test('StrongFlowService resolves only current business Attention and settles its human StageRun', async t => {
  const fixtureValue = await fixture(t, 'attention')
  const deliveryId = 'delivery-service-attention'
  const currentSpec = spec(deliveryId, 1, 'attention')
  const attentionId = 'attention-plan-decision'
  const planningRun = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'stage-planning-before-review-1',
    deliveryId,
    deliveryTaskId: null,
    stage: 'planning',
    actorType: 'codex',
    role: 'planner',
    status: 'running',
    attempt: 1,
    startedAtMillis: baseTime + 5,
    finishedAtMillis: null,
  }
  const planningBinding = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'binding-planning-before-review-1',
    deliveryId,
    stageRunId: planningRun.id,
    dshSessionId: 'dsh-planning-before-review-1',
    codexSessionId: 'codex-planning-before-review-1',
    boundAtMillis: baseTime + 6,
  }
  const planningDelivery = parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'planning',
    spec: currentSpec,
    tasks: [],
    stageRuns: [planningRun],
    sessionBindings: [planningBinding],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 6,
  })
  const currentAttention = createStrongFlowPlanReviewAttention({
    delivery: planningDelivery,
    attentionItemId: attentionId,
    reviewStageRunId: 'stage-plan-review-1',
    assignedTo: 'reviewer-1',
    solution: planReviewSolution('attention'),
    risks: [],
    unresolvedItems: ['Confirm the final rollout window.'],
    preparedAtMillis: baseTime + 7,
  })
  await seedDelivery(fixtureValue.home, 'attention', {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'needs-attention',
    spec: currentSpec,
    tasks: [],
    stageRuns: [{
      ...planningRun,
      status: 'succeeded',
      finishedAtMillis: baseTime + 8,
    }, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'stage-plan-review-1',
      deliveryId,
      deliveryTaskId: null,
      stage: 'plan-review',
      actorType: 'human',
      role: 'reviewer',
      status: 'waiting',
      attempt: 1,
      startedAtMillis: baseTime + 8,
      finishedAtMillis: null,
    }],
    sessionBindings: [planningBinding, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'binding-reviewer-1',
      deliveryId,
      stageRunId: 'stage-plan-review-1',
      dshSessionId: 'dsh-reviewer-1',
      codexSessionId: null,
      boundAtMillis: baseTime + 11,
    }],
    attentionItems: [currentAttention],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 11,
  })

  const resolved = await fixtureValue.service.resolveAttention({
    requestId: 'resolve-plan-decision',
    deliveryId,
    expectedRevision: 1,
    attentionItemId: attentionId,
    status: 'resolved',
    resolution: JSON.stringify(planReviewDecision(currentAttention)),
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
  })
  assert.equal(resolved.status, 'executing')
  assert.equal(resolved.attentionItems[0].status, 'resolved')
  assert.equal(resolved.stageRuns[1].status, 'succeeded')
  assert.ok(resolved.stageRuns[1].finishedAtMillis > baseTime + 11)
  await assert.rejects(
    fixtureValue.service.resolveAttention({
      requestId: 'resolve-plan-again',
      deliveryId,
      expectedRevision: 2,
      attentionItemId: attentionId,
      status: 'resolved',
      resolution: 'Approve again.',
      remediation: null,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
    }),
    expectServiceError('DELIVERY_CONFLICT'),
  )
})

test('StrongFlowService binds current evidence and computes the next state from DeliveryVerdict', async t => {
  const fixtureValue = await fixture(t, 'verdict')
  const deliveryId = 'delivery-service-verdict'
  const currentSpec = spec(deliveryId, 1, 'verdict')
  const seeded = parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'verifying',
    spec: currentSpec,
    tasks: [taskFor(deliveryId, currentSpec.acceptanceCriteria[0].id, 'verifying')],
    stageRuns: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-verdict-plan-review',
        deliveryId,
        deliveryTaskId: null,
        stage: 'plan-review',
        actorType: 'human',
        role: 'reviewer',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: baseTime + 5,
        finishedAtMillis: baseTime + 8,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-verdict-executor',
        deliveryId,
        deliveryTaskId: 'delivery-task-main',
        stage: 'executing',
        actorType: 'codex',
        role: 'executor',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: baseTime + 10,
        finishedAtMillis: baseTime + 14,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-verdict-reviewer',
        deliveryId,
        deliveryTaskId: 'delivery-task-main',
        stage: 'verifying',
        actorType: 'codex',
        role: 'reviewer',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: baseTime + 15,
        finishedAtMillis: baseTime + 19,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-verifying-1',
        deliveryId,
        deliveryTaskId: 'delivery-task-main',
        stage: 'verifying',
        actorType: 'codex',
        role: 'verifier',
        status: 'running',
        attempt: 1,
        startedAtMillis: baseTime + 20,
        finishedAtMillis: null,
      },
    ],
    sessionBindings: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-verdict-plan-review',
        deliveryId,
        stageRunId: 'stage-verdict-plan-review',
        dshSessionId: 'dsh-verdict-plan-review',
        codexSessionId: null,
        boundAtMillis: baseTime + 6,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-verdict-executor',
        deliveryId,
        stageRunId: 'stage-verdict-executor',
        dshSessionId: 'dsh-verdict-executor',
        codexSessionId: 'codex-verdict-executor',
        boundAtMillis: baseTime + 11,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-verdict-reviewer',
        deliveryId,
        stageRunId: 'stage-verdict-reviewer',
        dshSessionId: 'dsh-verdict-reviewer',
        codexSessionId: 'codex-verdict-reviewer',
        boundAtMillis: baseTime + 16,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-verifier-1',
        deliveryId,
        stageRunId: 'stage-verifying-1',
        dshSessionId: 'dsh-verifier-1',
        codexSessionId: 'codex-verifier-1',
        boundAtMillis: baseTime + 21,
      },
    ],
    attentionItems: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'attention-verdict-plan-review',
      deliveryId,
      deliverySpecId: currentSpec.id,
      stageRunId: 'stage-verdict-plan-review',
      type: 'decision_required',
      title: 'Approve the exact verification definition',
      context: 'Approve the current DeliverySpec before execution.',
      options: [],
      assignedTo: 'reviewer-1',
      blocking: true,
      status: 'resolved',
      resolution: 'Approved.',
      resolvedBy: 'reviewer-1',
      createdAtMillis: baseTime + 6,
      resolvedAtMillis: baseTime + 7,
    }],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 21,
  })
  await seedDelivery(fixtureValue.home, 'verdict', seeded)
  const submission = verdictSubmission(
    seeded,
    'stage-verdict-executor',
    'binding-verdict-executor',
    'pass',
  )
  const submitted = await fixtureValue.service.submitVerdict({
    requestId: 'submit-verdict-pass',
    deliveryId,
    expectedRevision: 1,
    ...submission,
  })
  assert.equal(submitted.status, 'ready-to-deliver')
  assert.equal(submitted.stageRuns[0].status, 'succeeded')
  assert.equal(submitted.tasks[0].status, 'completed')
  assert.equal(submitted.evidence.some(entry => entry.type === 'test'), true)
  assert.equal(submitted.verdict.status, 'pass')
  assert.deepEqual(await fixtureValue.service.submitVerdict({
    requestId: 'submit-verdict-pass',
    deliveryId,
    expectedRevision: 1,
    ...submission,
  }), submitted)
})

test('StrongFlowService can hand a failed verdict through rework into a new verification run', async t => {
  const current = await fixture(t, 'rework')
  const deliveryId = 'delivery-service-rework'
  const currentSpec = spec(deliveryId, 1, 'rework')
  const task = taskFor(deliveryId, currentSpec.acceptanceCriteria[0].id, 'verifying')
  const planningRun = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'stage-rework-planning',
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
    id: 'binding-rework-planning',
    deliveryId,
    stageRunId: planningRun.id,
    dshSessionId: 'dsh-rework-planning',
    codexSessionId: 'codex-rework-planning',
    boundAtMillis: baseTime + 2,
  }
  const planning = parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'planning',
    spec: currentSpec,
    tasks: [],
    stageRuns: [planningRun],
    sessionBindings: [planningBinding],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 2,
  })
  const planAttention = createStrongFlowPlanReviewAttention({
    delivery: planning,
    attentionItemId: 'attention-rework-plan-review',
    reviewStageRunId: 'stage-rework-plan-review',
    assignedTo: 'reviewer-1',
    solution: planReviewSolution('rework'),
    risks: [],
    unresolvedItems: [],
    preparedAtMillis: baseTime + 4,
  })
  const seeded = parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'verifying',
    spec: currentSpec,
    tasks: [task],
    stageRuns: [
      {
        ...planningRun,
        status: 'succeeded',
        finishedAtMillis: baseTime + 5,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-rework-plan-review',
        deliveryId,
        deliveryTaskId: null,
        stage: 'plan-review',
        actorType: 'human',
        role: 'reviewer',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: baseTime + 5,
        finishedAtMillis: baseTime + 8,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-rework-original-executor',
        deliveryId,
        deliveryTaskId: task.id,
        stage: 'executing',
        actorType: 'codex',
        role: 'executor',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: baseTime + 10,
        finishedAtMillis: baseTime + 14,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-rework-reviewer',
        deliveryId,
        deliveryTaskId: task.id,
        stage: 'verifying',
        actorType: 'codex',
        role: 'reviewer',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: baseTime + 15,
        finishedAtMillis: baseTime + 19,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-rework-initial-verification',
        deliveryId,
        deliveryTaskId: task.id,
        stage: 'verifying',
        actorType: 'codex',
        role: 'verifier',
        status: 'running',
        attempt: 1,
        startedAtMillis: baseTime + 20,
        finishedAtMillis: null,
      },
    ],
    sessionBindings: [
      planningBinding,
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-rework-plan-review',
        deliveryId,
        stageRunId: 'stage-rework-plan-review',
        dshSessionId: 'dsh-rework-plan-review',
        codexSessionId: null,
        boundAtMillis: baseTime + 6,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-rework-original-executor',
        deliveryId,
        stageRunId: 'stage-rework-original-executor',
        dshSessionId: 'dsh-rework-original-executor',
        codexSessionId: 'codex-rework-original-executor',
        boundAtMillis: baseTime + 11,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-rework-reviewer',
        deliveryId,
        stageRunId: 'stage-rework-reviewer',
        dshSessionId: 'dsh-rework-reviewer',
        codexSessionId: 'codex-rework-reviewer',
        boundAtMillis: baseTime + 16,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-rework-initial-verification',
        deliveryId,
        stageRunId: 'stage-rework-initial-verification',
        dshSessionId: 'dsh-rework-initial-verification',
        codexSessionId: 'codex-rework-initial-verification',
        boundAtMillis: baseTime + 21,
      },
    ],
    attentionItems: [{
      ...planAttention,
      status: 'resolved',
      resolution: JSON.stringify(planReviewDecision(planAttention)),
      resolvedBy: 'reviewer-1',
      resolvedAtMillis: baseTime + 7,
    }],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 21,
  })
  await seedDelivery(current.home, 'rework', seeded)
  const submission = verdictSubmission(
    seeded,
    'stage-rework-original-executor',
    'binding-rework-original-executor',
    'fail',
  )
  const failed = await current.service.submitVerdict({
    requestId: 'submit-rework-failure',
    deliveryId,
    expectedRevision: 1,
    ...submission,
  })
  assert.equal(failed.status, 'needs-attention')
  assert.equal(failed.tasks[0].status, 'failed')
  assert.equal(failed.attentionItems.length, 2)
  const failureAttention = failed.attentionItems.find(item => (
    item.options.some(option => option.id === 'start-rework')
  ))
  assert.notEqual(failureAttention, undefined)
  assert.equal(failureAttention.stageRunId, 'stage-rework-initial-verification')
  assert.equal(JSON.parse(failureAttention.context).verdictId, failed.verdict.id)

  const reworking = await current.service.resolveAttention({
    requestId: 'resolve-rework-failure',
    deliveryId,
    expectedRevision: 2,
    attentionItemId: failureAttention.id,
    status: 'resolved',
    resolution: 'Start bounded code rework for the failed acceptance criterion.',
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
  })
  assert.equal(reworking.status, 'reworking')
  assert.equal(reworking.attentionItems.at(-1).resolvedBy, 'reviewer-1')

  await assert.rejects(
    current.service.startStage({
      requestId: 'start-rework-with-executor',
      deliveryId,
      expectedRevision: 3,
      stageRunId: 'stage-rework-wrong-role',
      deliveryTaskId: task.id,
      stage: 'reworking',
      actorType: 'codex',
      role: 'executor',
      attention: null,
    }),
    expectServiceError('INVALID_REQUEST'),
  )
  const startedRework = await current.service.startStage({
    requestId: 'start-rework',
    deliveryId,
    expectedRevision: 3,
    stageRunId: 'stage-rework-execution',
    deliveryTaskId: task.id,
    stage: 'reworking',
    actorType: 'codex',
    role: 'remediator',
    attention: null,
  })
  assert.deepEqual(startedRework.spec, currentSpec)
  assert.equal(
    startedRework.stageRuns.find(run => run.id === 'stage-rework-execution').attempt,
    1,
  )
  await assert.rejects(
    current.service.updateDeliverySpec({
      requestId: 'change-spec-during-rework',
      deliveryId,
      expectedRevision: 4,
      spec: spec(deliveryId, 2, 'rework-definition-change'),
    }),
    expectServiceError('WRONG_DELIVERY_STATE'),
  )
  await current.service.bindSession({
    requestId: 'bind-rework',
    deliveryId,
    expectedRevision: 4,
    bindingId: 'binding-rework-execution',
    stageRunId: 'stage-rework-execution',
    dshSessionId: 'dsh-rework-execution',
    codexSessionId: 'codex-rework-execution',
  })
  await current.service.startStage({
    requestId: 'start-rework-review',
    deliveryId,
    expectedRevision: 5,
    stageRunId: 'stage-rework-reviewer-2',
    deliveryTaskId: task.id,
    stage: 'verifying',
    actorType: 'codex',
    role: 'reviewer',
    attention: null,
  })
  await current.service.bindSession({
    requestId: 'bind-rework-review',
    deliveryId,
    expectedRevision: 6,
    bindingId: 'binding-rework-reviewer-2',
    stageRunId: 'stage-rework-reviewer-2',
    dshSessionId: 'dsh-rework-reviewer-2',
    codexSessionId: 'codex-rework-reviewer-2',
  })
  const verification = await current.service.startStage({
    requestId: 'start-rework-verification',
    deliveryId,
    expectedRevision: 7,
    stageRunId: 'stage-rework-verification',
    deliveryTaskId: task.id,
    stage: 'verifying',
    actorType: 'codex',
    role: 'verifier',
    attention: null,
  })
  const readyForVerdict = await current.service.bindSession({
    requestId: 'bind-rework-verification',
    deliveryId,
    expectedRevision: 8,
    bindingId: 'binding-rework-verification',
    stageRunId: 'stage-rework-verification',
    dshSessionId: 'dsh-rework-verification',
    codexSessionId: 'codex-rework-verification',
  })
  assert.equal(verification.status, 'verifying')
  assert.equal(verification.tasks[0].status, 'verifying')
  assert.equal(
    verification.stageRuns.find(run => run.id === 'stage-rework-execution').status,
    'succeeded',
  )
  assert.equal(
    verification.stageRuns.find(run => run.id === 'stage-rework-verification').status,
    'running',
  )
  await assert.rejects(
    current.service.submitVerdict({
      requestId: 'submit-stale-pre-rework-candidate',
      deliveryId,
      expectedRevision: 9,
      candidate: submission.candidate,
      runtimeEvents: verdictRuntimeEvents(readyForVerdict, submission.candidate, 'pass'),
      requiredRoles: ['reviewer', 'verifier'],
    }),
    expectServiceError('INVALID_REQUEST'),
  )
  const freshSubmission = verdictSubmission(
    readyForVerdict,
    'stage-rework-execution',
    'binding-rework-execution',
    'pass',
  )
  const passed = await current.service.submitVerdict({
    requestId: 'submit-reworked-candidate',
    deliveryId,
    expectedRevision: 9,
    ...freshSubmission,
  })
  assert.equal(passed.status, 'ready-to-deliver')
  assert.notEqual(passed.verdict.candidateRef, failed.verdict.candidateRef)
  assert.notEqual(passed.verdict.criteria[0].id, failed.verdict.criteria[0].id)
  assert.ok(passed.evidence.some(entry => (
    entry.candidateRef === passed.verdict.candidateRef
  )))
  current.setDiagramFacts(Object.freeze({
    runtimeEvents: Object.freeze([candidateRuntimeEvent(passed, freshSubmission.candidate)]),
    candidate: freshSubmission.candidate,
  }))

  const deliveryReviewAttention = openAttention({
    id: 'attention-rework-delivery-review',
    deliveryId,
    deliverySpecId: currentSpec.id,
    stageRunId: 'stage-rework-delivery-review',
    type: 'delivery_approval',
    title: 'Review the reworked candidate diagrams',
    createdAtMillis: baseTime + 90,
  })
  await current.service.startStage({
    requestId: 'start-rework-delivery-review',
    deliveryId,
    expectedRevision: 10,
    stageRunId: 'stage-rework-delivery-review',
    deliveryTaskId: null,
    stage: 'delivery-review',
    actorType: 'human',
    role: 'approver',
    attention: deliveryReviewAttention,
  })
  await current.service.bindSession({
    requestId: 'bind-rework-delivery-review',
    deliveryId,
    expectedRevision: 11,
    bindingId: 'binding-rework-delivery-review',
    stageRunId: 'stage-rework-delivery-review',
    dshSessionId: 'dsh-rework-delivery-review',
    codexSessionId: null,
  })
  await assert.rejects(
    current.service.resolveAttention({
      requestId: 'dismiss-review-without-annotations',
      deliveryId,
      expectedRevision: 12,
      attentionItemId: deliveryReviewAttention.id,
      status: 'dismissed',
      resolution: 'The reviewed diagram node needs a targeted change.',
      remediation: null,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
    }),
    expectServiceError('ATTENTION_REQUIRED'),
  )
  const currentEvidence = passed.evidence.find(entry => (
    entry.candidateRef === freshSubmission.candidate.candidateRef
  ))
  assert.notEqual(currentEvidence, undefined)
  const reviewContext = parseStrongFlowPlanReviewContextText(planAttention.context)
  const exactHunkSha256 = createHash('sha256')
    .update('@@ -1 +1 @@\n-old\n+new\n')
    .digest('hex')
  const remediation = {
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    protocol: 'winwincode.delivery-remediation.v1',
    deliveryTaskId: task.id,
    candidate: freshSubmission.candidate,
    annotations: [{
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      id: 'annotation-rework-value-hunk',
      diagramKind: 'system-architecture',
      diagramId: reviewContext.architectureDiagram.id,
      nodeId: 'component-rework',
      filePath: 'src/value.ts',
      hunkSha256: exactHunkSha256,
      evidenceRefIds: [currentEvidence.id],
      note: 'Keep the reviewed behavior but correct this exact changed hunk.',
    }],
  }
  await assert.rejects(
    current.service.resolveAttention({
      requestId: 'dismiss-review-with-broadened-task-scope',
      deliveryId,
      expectedRevision: 12,
      attentionItemId: deliveryReviewAttention.id,
      status: 'dismissed',
      resolution: 'This annotation removes the reviewed DeliveryTask boundary.',
      remediation: { ...remediation, deliveryTaskId: null },
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
    }),
    expectServiceError('DELIVERY_CONFLICT'),
  )
  await assert.rejects(
    current.service.resolveAttention({
      requestId: 'dismiss-review-with-stale-candidate',
      deliveryId,
      expectedRevision: 12,
      attentionItemId: deliveryReviewAttention.id,
      status: 'dismissed',
      resolution: 'This annotation cites the superseded candidate.',
      remediation: { ...remediation, candidate: submission.candidate },
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
    }),
    expectServiceError('INVALID_REQUEST'),
  )
  await assert.rejects(
    current.service.resolveAttention({
      requestId: 'dismiss-review-with-foreign-path',
      deliveryId,
      expectedRevision: 12,
      attentionItemId: deliveryReviewAttention.id,
      status: 'dismissed',
      resolution: 'This annotation cites a path outside the frozen diff.',
      remediation: {
        ...remediation,
        annotations: [{
          ...remediation.annotations[0],
          filePath: 'src/foreign.ts',
        }],
      },
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
    }),
    expectServiceError('DELIVERY_CONFLICT'),
  )
  for (const [requestId, annotation] of [
    ['dismiss-review-with-stale-diagram-node', {
      ...remediation.annotations[0],
      nodeId: 'component-rework-stale',
    }],
    ['dismiss-review-with-stale-hunk', {
      ...remediation.annotations[0],
      hunkSha256: '0'.repeat(64),
    }],
  ]) {
    await assert.rejects(
      current.service.resolveAttention({
        requestId,
        deliveryId,
        expectedRevision: 12,
        attentionItemId: deliveryReviewAttention.id,
        status: 'dismissed',
        resolution: 'This annotation does not exist in the current yellow diagram node.',
        remediation: {
          ...remediation,
          annotations: [annotation],
        },
        channel: 'local-ui',
        authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
      }),
      expectServiceError('DELIVERY_CONFLICT'),
    )
  }
  const annotated = await current.service.resolveAttention({
    requestId: 'dismiss-review-with-annotations',
    deliveryId,
    expectedRevision: 12,
    attentionItemId: deliveryReviewAttention.id,
    status: 'dismissed',
    resolution: 'Apply the selected diagram annotation only.',
    remediation,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
  })
  assert.equal(annotated.status, 'reworking')
  assert.equal(annotated.tasks[0].status, 'failed')
  assert.equal(annotated.verdict, null)
  const persistedRemediation = JSON.parse(annotated.attentionItems.at(-1).resolution)
  assert.equal(persistedRemediation.candidateRef, freshSubmission.candidate.candidateRef)
  assert.equal(persistedRemediation.diffSha256, freshSubmission.candidate.diffSha256)
  assert.equal(persistedRemediation.annotations[0].nodeId, 'component-rework')
  assert.equal(Object.hasOwn(persistedRemediation, 'candidate'), false)
  const secondRework = await current.service.startStage({
    requestId: 'start-annotated-rework',
    deliveryId,
    expectedRevision: 13,
    stageRunId: 'stage-annotated-rework',
    deliveryTaskId: task.id,
    stage: 'reworking',
    actorType: 'codex',
    role: 'remediator',
    attention: null,
  })
  assert.equal(
    secondRework.stageRuns.find(run => run.id === 'stage-annotated-rework').attempt,
    2,
  )
  const executingDiagram = await current.service.getDeliveryProjection(deliveryId)
  assert.equal(executingDiagram.diagramExecution.state, 'executing')
  assert.equal(executingDiagram.diagramExecution.details, null)
  assert.equal(executingDiagram.runtimeExecution.deliveryId, deliveryId)
  assert.equal(executingDiagram.runtimeExecution.deliveryRevision, executingDiagram.delivery.revision)
  assert.equal(executingDiagram.runtimeExecution.sessions.some(session => (
    session.sessionBindingId === freshSubmission.candidate.producerSessionBindingId
      && session.diffSummary?.detailsVisible === false
  )), true)
  assert.equal(
    executingDiagram.diagramExecution.architecture.nodes.find(node => (
      node.nodeId === remediation.annotations[0].nodeId
    )).state,
    'affected-live',
  )
  const restartedService = new StrongFlowService({
    home: current.home,
    authenticator,
    executionSource: {
      async read() { return current.readDiagramFacts() },
    },
  })
  const restartedDiagram = await restartedService.getDeliveryProjection(deliveryId)
  assert.deepEqual(restartedDiagram.diagramExecution, executingDiagram.diagramExecution)
  assert.deepEqual(restartedDiagram.runtimeExecution, executingDiagram.runtimeExecution)
  assert.equal(
    JSON.parse(restartedDiagram.delivery.attentionItems.at(-1).resolution)
      .annotations[0].nodeId,
    remediation.annotations[0].nodeId,
  )
  assert.throws(
    () => freezeDeliveryCandidate(secondRework, {
      producerStageRunId: freshSubmission.candidate.producerStageRunId,
      producerSessionBindingId: freshSubmission.candidate.producerSessionBindingId,
      baseCommitId: freshSubmission.candidate.baseCommitId,
      baseTreeId: freshSubmission.candidate.baseTreeId,
      candidateCommitId: freshSubmission.candidate.candidateCommitId,
      candidateTreeId: freshSubmission.candidate.candidateTreeId,
      diffSha256: freshSubmission.candidate.diffSha256,
      changedPaths: freshSubmission.candidate.changedPaths,
    }),
  )
  await current.service.bindSession({
    requestId: 'bind-annotated-rework',
    deliveryId,
    expectedRevision: 14,
    bindingId: 'binding-annotated-rework',
    stageRunId: 'stage-annotated-rework',
    dshSessionId: 'dsh-annotated-rework',
    codexSessionId: 'codex-annotated-rework',
  })
  await current.service.startStage({
    requestId: 'start-annotated-rework-review',
    deliveryId,
    expectedRevision: 15,
    stageRunId: 'stage-annotated-rework-reviewer',
    deliveryTaskId: task.id,
    stage: 'verifying',
    actorType: 'codex',
    role: 'reviewer',
    attention: null,
  })
  await current.service.bindSession({
    requestId: 'bind-annotated-rework-review',
    deliveryId,
    expectedRevision: 16,
    bindingId: 'binding-annotated-rework-reviewer',
    stageRunId: 'stage-annotated-rework-reviewer',
    dshSessionId: 'dsh-annotated-rework-reviewer',
    codexSessionId: 'codex-annotated-rework-reviewer',
  })
  await current.service.startStage({
    requestId: 'start-annotated-rework-verification',
    deliveryId,
    expectedRevision: 17,
    stageRunId: 'stage-annotated-rework-verifier',
    deliveryTaskId: task.id,
    stage: 'verifying',
    actorType: 'codex',
    role: 'verifier',
    attention: null,
  })
  const secondReadyForVerdict = await current.service.bindSession({
    requestId: 'bind-annotated-rework-verification',
    deliveryId,
    expectedRevision: 18,
    bindingId: 'binding-annotated-rework-verifier',
    stageRunId: 'stage-annotated-rework-verifier',
    dshSessionId: 'dsh-annotated-rework-verifier',
    codexSessionId: 'codex-annotated-rework-verifier',
  })
  const repeatedFailureSubmission = verdictSubmission(
    secondReadyForVerdict,
    'stage-annotated-rework',
    'binding-annotated-rework',
    'fail',
  )
  const stopped = await current.service.submitVerdict({
    requestId: 'submit-repeated-rework-failure',
    deliveryId,
    expectedRevision: 19,
    ...repeatedFailureSubmission,
  })
  const definitionAttention = stopped.attentionItems.find(item => (
    item.status === 'open' && item.options.some(option => option.id === 'clarify-scope')
  ))
  assert.notEqual(definitionAttention, undefined)
  assert.equal(definitionAttention.type, 'scope_change')
  assert.equal(
    stopped.attentionItems.some(item => (
      item.status === 'open' && item.options.some(option => option.id === 'start-rework')
    )),
    false,
  )
  const clarifying = await current.service.resolveAttention({
    requestId: 'resolve-repeated-rework-failure',
    deliveryId,
    expectedRevision: 20,
    attentionItemId: definitionAttention.id,
    status: 'resolved',
    resolution: 'Review the delivery definition before any additional code change.',
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
  })
  assert.equal(clarifying.status, 'clarifying')
  await assert.rejects(
    current.service.startStage({
      requestId: 'start-exhausted-rework',
      deliveryId,
      expectedRevision: 21,
      stageRunId: 'stage-exhausted-rework',
      deliveryTaskId: task.id,
      stage: 'reworking',
      actorType: 'codex',
      role: 'remediator',
      attention: null,
    }),
    expectServiceError('WRONG_DELIVERY_STATE'),
  )

  const infrastructure = await fixture(t, 'verification-infrastructure')
  await seedDelivery(infrastructure.home, 'verification-infrastructure', seeded)
  const infrastructureSubmission = verdictSubmission(
    seeded,
    'stage-rework-original-executor',
    'binding-rework-original-executor',
    'infra_error',
  )
  const blocked = await infrastructure.service.submitVerdict({
    requestId: 'submit-verification-infrastructure-error',
    deliveryId,
    expectedRevision: 1,
    ...infrastructureSubmission,
  })
  assert.equal(blocked.status, 'needs-attention')
  assert.equal(blocked.tasks[0].status, 'verifying')
  const retryAttention = blocked.attentionItems.find(item => (
    item.options.some(option => option.id === 'retry-verification')
  ))
  assert.notEqual(retryAttention, undefined)
  assert.equal(retryAttention.type, 'verification_blocked')
  const retrying = await infrastructure.service.resolveAttention({
    requestId: 'resolve-verification-infrastructure-error',
    deliveryId,
    expectedRevision: 2,
    attentionItemId: retryAttention.id,
    status: 'resolved',
    resolution: 'Retry verification on the unchanged candidate.',
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'fixture-proof-value' },
  })
  assert.equal(retrying.status, 'verifying')
  assert.equal(retrying.tasks[0].status, 'verifying')
  assert.equal(retrying.attentionItems.at(-1).resolvedBy, 'reviewer-1')
})

test('StrongFlowService fails before storage on malformed or credential-bearing input', async t => {
  const fixtureValue = await fixture(t, 'invalid')
  const deliveryId = 'delivery-service-invalid'
  const currentSpec = spec(deliveryId, 1, 'invalid')
  await assert.rejects(
    fixtureValue.service.createDelivery({
      requestId: 'create-secret',
      spec: {
        ...currentSpec,
        goal: 'Use Authorization: Bearer fixture-secret-value',
      },
      tasks: [],
    }),
    expectServiceError('INVALID_REQUEST'),
  )
  await assert.rejects(
    fixtureValue.service.createDelivery({
      requestId: 'create-extra-field',
      spec: currentSpec,
      tasks: [],
      codexPlan: [],
    }),
    expectServiceError('INVALID_REQUEST'),
  )
  await assert.rejects(
    fixtureValue.service.getDeliveryProjection(deliveryId),
    expectServiceError('DELIVERY_NOT_FOUND'),
  )
})
