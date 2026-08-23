import assert from 'node:assert/strict'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseDelivery,
} from '../packages/contracts/dist/index.js'
import {
  DELIVERY_MEASURES_SCHEMA_VERSION,
  DeliveryMeasuresError,
  createDeliveryMeasuresProjection,
  groupDeliveryMeasuresByRunKind,
} from '../packages/strongflow/dist/index.js'

const now = 2_500_000_000_000
const deliveryId = 'delivery-measures-fixture'
const specId = 'spec-measures-fixture'
const criterionId = 'criterion-measures-fixture'
const taskId = 'task-measures-fixture'
const candidateRef = 'candidate-measures-fixture'

function eventLink(eventId, sequence) {
  return Object.freeze({
    eventId,
    sourceRef: `runtime_event:${eventId}`,
    sequence: String(sequence),
  })
}

function stageRun(id, stage, actorType, role, started, finished, status = 'succeeded') {
  return Object.freeze({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id,
    deliveryId,
    deliveryTaskId: taskId,
    stage,
    actorType,
    role,
    status,
    attempt: 1,
    startedAtMillis: now + started,
    finishedAtMillis: finished === null ? null : now + finished,
  })
}

function binding(id, stageRunId, codex = true) {
  return Object.freeze({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id,
    deliveryId,
    stageRunId,
    dshSessionId: `dsh-${id}`,
    codexSessionId: codex ? `codex-${id}` : null,
    boundAtMillis: now + 10,
  })
}

function evidence(id, stageRunId, sessionBindingId, type, sourceRef) {
  return Object.freeze({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id,
    deliveryId,
    deliverySpecId: specId,
    deliverySpecRevision: 1,
    stageRunId,
    sessionBindingId,
    candidateRef,
    type,
    sourceRef,
    createdAtMillis: now + 400,
  })
}

function deliveryFixture({
  criterionVerdict = 'pass',
  deliveryStatus = 'delivered',
  includeVerdict = true,
  stageFailure = false,
} = {}) {
  const stages = [
    stageRun(
      'stage-measures-executing',
      'executing',
      'codex',
      'executor',
      100,
      200,
      stageFailure ? 'failed' : 'succeeded',
    ),
    stageRun('stage-measures-reviewer', 'verifying', 'codex', 'reviewer', 210, 310),
    stageRun('stage-measures-verifier', 'verifying', 'codex', 'verifier', 220, 320),
    stageRun('stage-measures-approval', 'delivery-review', 'human', 'approver', 330, 350),
  ]
  const bindings = [
    binding('binding-measures-executing', 'stage-measures-executing'),
    binding('binding-measures-reviewer', 'stage-measures-reviewer'),
    binding('binding-measures-verifier', 'stage-measures-verifier'),
    binding('binding-measures-approval', 'stage-measures-approval', false),
  ]
  const evidenceFacts = includeVerdict ? [
    evidence(
      'evidence-measures-reviewer-finding',
      'stage-measures-reviewer',
      'binding-measures-reviewer',
      'review_finding',
      'runtime_event:finding-reviewer',
    ),
    evidence(
      'evidence-measures-reviewer-test',
      'stage-measures-reviewer',
      'binding-measures-reviewer',
      'test',
      'runtime_event:test-reviewer',
    ),
    evidence(
      'evidence-measures-verifier-finding',
      'stage-measures-verifier',
      'binding-measures-verifier',
      'review_finding',
      'runtime_event:finding-verifier',
    ),
    evidence(
      'evidence-measures-verifier-test',
      'stage-measures-verifier',
      'binding-measures-verifier',
      'test',
      'runtime_event:test-verifier',
    ),
  ] : []
  const evidenceIds = evidenceFacts.map(reference => reference.id)
  const verdict = includeVerdict ? {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `verdict-measures-${criterionVerdict}`,
    deliveryId,
    deliverySpecId: specId,
    candidateRef,
    status: criterionVerdict,
    criteria: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `criterion-result-measures-${criterionVerdict}`,
      deliveryId,
      deliverySpecId: specId,
      criterionId,
      candidateRef,
      verdict: criterionVerdict,
      evidenceRefs: evidenceIds,
      explanation: `Independent roles reported ${criterionVerdict}.`,
      evaluatedAtMillis: now + 500,
    }],
    unresolvedFindings: [],
    producedAtMillis: now + 510,
  } : null
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 9,
    status: deliveryStatus,
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: specId,
      deliveryId,
      revision: 1,
      title: 'Measure one Delivery',
      goal: 'Derive source-linked facts without one opaque score.',
      scope: ['Five explainable dimensions'],
      outOfScope: ['A second runtime'],
      constraints: ['Keep run kinds separate'],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: criterionId,
        description: 'The exact Delivery result is independently verified.',
        verificationMethod: 'Two independent role findings with direct test evidence.',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/fixture/repository',
      },
      baseRevision: '0123456789012345678901234567890123456789',
      maxReworkAttempts: 2,
      createdAtMillis: now,
    },
    tasks: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: taskId,
      deliveryId,
      title: 'Measured work',
      goal: 'Produce the independently verified candidate.',
      acceptanceCriterionIds: [criterionId],
      blockedByTaskIds: [],
      owner: null,
      status: includeVerdict ? 'completed' : 'active',
    }],
    stageRuns: stages,
    sessionBindings: bindings,
    attentionItems: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'attention-measures-delivery-review',
      deliveryId,
      deliverySpecId: specId,
      stageRunId: 'stage-measures-approval',
      type: 'delivery_approval',
      title: 'Approve the evaluated candidate',
      context: 'Review the exact Verdict and evidence.',
      options: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'approve-delivery',
        label: 'Approve',
        description: 'Approve this exact candidate.',
      }],
      assignedTo: 'fixture-human',
      blocking: true,
      status: 'resolved',
      resolution: 'Approved.',
      resolvedBy: 'fixture-human',
      createdAtMillis: now + 330,
      resolvedAtMillis: now + 350,
    }],
    evidence: evidenceFacts,
    verdict,
    createdAtMillis: now,
    updatedAtMillis: now + 700,
  })
}

function runtimeSession(bindingFact, {
  outcome = 'succeeded',
  parallel = false,
  interaction = false,
  failure = false,
  recovery = false,
} = {}) {
  const rootAgent = {
    threadId: `agent-${bindingFact.id}`,
    parentThreadId: null,
    status: failure ? 'failed' : 'completed',
    firstEvent: eventLink(`agent-start-${bindingFact.id}`, 1),
    latestEvent: eventLink(`agent-end-${bindingFact.id}`, 10),
  }
  return Object.freeze({
    binding: bindingFact,
    agents: parallel ? [rootAgent, {
      threadId: `subagent-${bindingFact.id}`,
      parentThreadId: rootAgent.threadId,
      status: 'completed',
      firstEvent: eventLink(`subagent-start-${bindingFact.id}`, 3),
      latestEvent: eventLink(`subagent-end-${bindingFact.id}`, 7),
    }] : [rootAgent],
    activities: [{
      outcome,
      firstEvent: eventLink(`activity-start-${bindingFact.id}`, 4),
      latestEvent: eventLink(`activity-end-${bindingFact.id}`, 6),
    }],
    interactions: interaction ? [{
      interactionType: 'execution-approval',
      blocking: true,
      status: 'resolved',
      requestedEvent: eventLink(`approval-request-${bindingFact.id}`, 2),
      resolvedEvent: eventLink(`approval-resolved-${bindingFact.id}`, 3),
    }] : [],
    failures: failure ? [{
      event: eventLink(`failure-${bindingFact.id}`, 8),
    }] : [],
    recovery: {
      failureCount: failure ? 1 : 0,
      recoveryCount: recovery ? 1 : 0,
      lastFailureEvent: failure ? eventLink(`failure-${bindingFact.id}`, 8) : null,
      latestRecoveryEvent: recovery ? eventLink(`recovery-${bindingFact.id}`, 9) : null,
    },
  })
}

function runtimeFixture(delivery, {
  outcome = 'succeeded',
  parallel = true,
  interaction = true,
  failure = false,
  recovery = false,
} = {}) {
  const stages = delivery.stageRuns
    .filter(stage => stage.actorType === 'codex')
    .map(stage => {
      const bindingFact = delivery.sessionBindings.find(entry => entry.stageRunId === stage.id)
      return Object.freeze({
        stageRun: stage,
        sessions: [runtimeSession(bindingFact, {
          outcome: stage.role === 'executor' ? outcome : 'succeeded',
          parallel: stage.role === 'executor' && parallel,
          interaction: stage.role === 'executor' && interaction,
          failure: stage.role === 'executor' && failure,
          recovery: stage.role === 'executor' && recovery,
        })],
      })
    })
  return Object.freeze({
    deliveryId: delivery.id,
    deliveryRevision: delivery.revision,
    stages,
  })
}

function modelCalls() {
  return Object.freeze([{
    sourceRef: 'model_call:fixture:1',
    status: 'completed',
    startedAtMillis: now + 100,
    finishedAtMillis: now + 200,
    inputTokens: 10,
    outputTokens: 5,
    cacheReadTokens: 2,
    cacheWriteTokens: 3,
    costUsdMicros: 100,
  }, {
    sourceRef: 'model_call:fixture:2',
    status: 'completed',
    startedAtMillis: now + 300,
    finishedAtMillis: now + 500,
    inputTokens: 7,
    outputTokens: 4,
    cacheReadTokens: 0,
    cacheWriteTokens: 1,
    costUsdMicros: 80,
  }])
}

function measures({
  runKind = 'deterministic',
  runId = 'measures-fixture-run',
  runState = 'completed',
  delivery = deliveryFixture(),
  runtimeProjection = runtimeFixture(delivery),
  historicalVerdicts = delivery.verdict === null ? [] : [delivery.verdict],
} = {}) {
  return createDeliveryMeasuresProjection({
    schemaVersion: DELIVERY_MEASURES_SCHEMA_VERSION,
    runKind,
    runId,
    runState,
    startedAtMillis: now,
    finishedAtMillis: runState === 'running' ? null : now + 1_000,
    delivery,
    runtimeProjection,
    requiredVerificationRoles: ['reviewer', 'verifier'],
    modelCalls: modelCalls(),
    pricingSource: 'fixture pricing revision 1',
    historicalVerdicts,
  })
}

function assertEveryMeasureHasSources(value) {
  if (Array.isArray(value)) {
    value.forEach(assertEveryMeasureHasSources)
    return
  }
  if (typeof value !== 'object' || value === null) return
  if (Object.hasOwn(value, 'value')) {
    assert.equal(Array.isArray(value.sourceRefs), true)
    assert.equal(value.sourceRefs.length > 0, true)
  }
  Object.values(value).forEach(assertEveryMeasureHasSources)
}

test('derives a frozen, reproducible, source-linked report without an overall score', () => {
  const input = {
    runKind: 'deterministic',
    runId: 'measures-repeatable-run',
  }
  const first = measures(input)
  const second = measures(input)
  assert.deepEqual(first, second)
  assert.equal(Object.isFrozen(first), true)
  assert.equal(Object.isFrozen(first.dimensions.efficiency.totalTokens), true)
  assert.equal(first.outcome.classification.value, 'proven-success')
  assert.equal(first.outcome.falseSuccessRisk.value, false)
  assert.equal(first.outcome.falseFailureRisk.value, false)
  assert.equal(first.dimensions.completeness.status.value, 'complete')
  assert.equal(first.dimensions.confidence.status.value, 'independently-supported')
  assert.equal(Object.hasOwn(first, 'score'), false)
  assert.equal(JSON.stringify(first).includes('overallScore'), false)
  assertEveryMeasureHasSources(first)
})

test('separates confidence in evidence from whether the candidate passed', () => {
  const delivery = deliveryFixture({
    criterionVerdict: 'fail',
    deliveryStatus: 'verifying',
  })
  const report = measures({
    runState: 'failed',
    delivery,
    runtimeProjection: runtimeFixture(delivery, { outcome: 'task-failed' }),
  })
  assert.equal(report.dimensions.completeness.status.value, 'failed')
  assert.equal(report.dimensions.confidence.status.value, 'independently-supported')
  assert.equal(report.dimensions.stability.status.value, 'failed')
  assert.equal(report.outcome.completionProofPresent.value, false)
})

test('keeps infrastructure failure separate from candidate task failure', () => {
  const delivery = deliveryFixture({
    criterionVerdict: 'infra_error',
    deliveryStatus: 'verifying',
  })
  const report = measures({
    runState: 'failed',
    delivery,
    runtimeProjection: runtimeFixture(delivery, {
      outcome: 'infrastructure-failed',
      failure: true,
    }),
  })
  assert.equal(report.dimensions.completeness.status.value, 'blocked-by-infrastructure')
  assert.equal(report.dimensions.stability.status.value, 'infrastructure-affected')
  assert.equal(report.dimensions.stability.taskFailureCount.value, 0)
  assert.equal(report.dimensions.stability.infrastructureFailureCount.value, 1)
  assert.equal(report.dimensions.stability.runtimeFailureEventCount.value, 1)
})

test('makes false-success and false-failure risks explicit', () => {
  const unverified = deliveryFixture({
    deliveryStatus: 'executing',
    includeVerdict: false,
  })
  const falseSuccess = measures({
    runState: 'completed',
    delivery: unverified,
    runtimeProjection: runtimeFixture(unverified),
  })
  assert.equal(falseSuccess.outcome.falseSuccessRisk.value, true)
  assert.equal(falseSuccess.outcome.classification.value, 'claimed-without-proof')

  const provenButUnclaimed = deliveryFixture({ deliveryStatus: 'verifying' })
  const falseFailure = measures({
    runState: 'failed',
    delivery: provenButUnclaimed,
    runtimeProjection: runtimeFixture(provenButUnclaimed),
  })
  assert.equal(falseFailure.outcome.falseFailureRisk.value, true)
  assert.equal(falseFailure.outcome.classification.value, 'proof-not-claimed')
})

test('reconciles elapsed time, usage, interventions, and observed parallelism', () => {
  const report = measures()
  const efficiency = report.dimensions.efficiency
  const human = report.dimensions.humanDependence
  assert.equal(efficiency.runElapsedMillis.value, 1_000)
  assert.equal(efficiency.settledStageMillis.value, 320)
  assert.equal(efficiency.modelElapsedMillis.value, 300)
  assert.equal(efficiency.inputTokens.value, 17)
  assert.equal(efficiency.outputTokens.value, 9)
  assert.equal(efficiency.cacheReadTokens.value, 2)
  assert.equal(efficiency.cacheWriteTokens.value, 4)
  assert.equal(efficiency.totalTokens.value, 32)
  assert.equal(efficiency.costUsdMicros.value, 180)
  assert.equal(efficiency.missingUsageCallCount.value, 0)
  assert.equal(efficiency.maxConcurrentAgents.value, 2)
  assert.equal(efficiency.parallelExecutionObserved.value, true)
  assert.equal(human.attentionCount.value, 1)
  assert.equal(human.resolvedAttentionCount.value, 1)
  assert.equal(human.executionApprovalRequestCount.value, 1)
  assert.equal(human.executionApprovalResolutionCount.value, 1)
})

test('keeps deterministic and live reports in separate lanes', () => {
  const deterministic = measures({ runId: 'deterministic-a' })
  const live = measures({ runKind: 'live', runId: 'live-a' })
  const grouped = groupDeliveryMeasuresByRunKind([live, deterministic])
  assert.deepEqual(grouped.deterministic.map(report => report.runId), ['deterministic-a'])
  assert.deepEqual(grouped.live.map(report => report.runId), ['live-a'])
  assert.equal(Object.hasOwn(grouped, 'combined'), false)
})

test('reports unavailable Delivery facts without inventing zero completion', () => {
  const report = createDeliveryMeasuresProjection({
    schemaVersion: DELIVERY_MEASURES_SCHEMA_VERSION,
    runKind: 'live',
    runId: 'failed-before-delivery',
    runState: 'failed',
    startedAtMillis: now,
    finishedAtMillis: now + 25,
    delivery: null,
    runtimeProjection: null,
    requiredVerificationRoles: ['reviewer', 'verifier'],
    modelCalls: [],
    pricingSource: 'fixture pricing revision 1',
    historicalVerdicts: [],
  })
  assert.equal(report.dimensions.completeness.status.value, 'not-available')
  assert.equal(report.dimensions.completeness.requiredPassRate.value, null)
  assert.equal(report.dimensions.confidence.status.value, 'not-evaluated')
  assert.equal(report.dimensions.efficiency.parallelismObservationAvailable.value, false)
  assert.equal(report.dimensions.efficiency.maxConcurrentAgents.value, null)
  assert.equal(JSON.stringify(report).includes('NaN'), false)
})

test('rejects mismatched runtime revisions and duplicate model-call sources', () => {
  const delivery = deliveryFixture()
  assert.throws(
    () => measures({
      delivery,
      runtimeProjection: {
        ...runtimeFixture(delivery),
        deliveryRevision: delivery.revision + 1,
      },
    }),
    error => error instanceof DeliveryMeasuresError
      && error.code === 'RELATIONSHIP_MISMATCH',
  )
  assert.throws(
    () => createDeliveryMeasuresProjection({
      schemaVersion: DELIVERY_MEASURES_SCHEMA_VERSION,
      runKind: 'live',
      runId: 'duplicate-call-run',
      runState: 'completed',
      startedAtMillis: now,
      finishedAtMillis: now + 1,
      delivery,
      runtimeProjection: runtimeFixture(delivery),
      requiredVerificationRoles: ['reviewer', 'verifier'],
      modelCalls: [modelCalls()[0], modelCalls()[0]],
      pricingSource: null,
      historicalVerdicts: [delivery.verdict],
    }),
    error => error instanceof DeliveryMeasuresError && error.code === 'DUPLICATE_FACT',
  )
})
