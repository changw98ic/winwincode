import assert from 'node:assert/strict'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseAttentionItem,
  parseDelivery,
} from '../packages/contracts/dist/index.js'
import {
  DeliveryAttentionClassificationError,
  assertDeliveryVerdictAttentionCurrent,
  deliveryVerdictAttentionNextStatus,
  deriveDeliveryVerdictAttention,
} from '../packages/strongflow/dist/index.js'

const now = 2_300_000_000_000
const deliveryId = 'dlv_5PQMH8V1RF6S6VEGYMZQQ4QWAV'
const specId = 'delivery-spec-attention-v1'
const candidateRef = `git-candidate:sha256:${'a'.repeat(64)}`
const verificationStageRunId = 'stage-attention-verifier'
const verificationBindingId = 'binding-attention-verifier'

function fixture({
  verdicts,
  unresolvedFindings = [],
  maxReworkAttempts = 2,
  reworkAttemptsUsed = 0,
  priorFailureCriterionIds = [],
}) {
  const criteria = verdicts.map((_, index) => ({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `criterion-attention-${index + 1}`,
    description: `Criterion ${index + 1} is directly verifiable.`,
    verificationMethod: `Run check ${index + 1}.`,
    required: true,
  }))
  const evidence = verdicts.flatMap((verdict, index) => (
    verdict === 'pass' || verdict === 'fail'
      ? [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: `evidence-attention-${index + 1}`,
        deliveryId,
        deliverySpecId: specId,
        deliverySpecRevision: 1,
        stageRunId: verificationStageRunId,
        sessionBindingId: verificationBindingId,
        candidateRef,
        type: 'test',
        sourceRef: `dsh-attention-verifier@${index + 1}`,
        createdAtMillis: now + 20,
      }]
      : []
  ))
  const results = verdicts.map((verdict, index) => ({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `criterion-result-attention-${index + 1}`,
    deliveryId,
    deliverySpecId: specId,
    criterionId: criteria[index].id,
    candidateRef,
    verdict,
    evidenceRefs: verdict === 'pass' || verdict === 'fail'
      ? [`evidence-attention-${index + 1}`]
      : [],
    explanation: `Independent verification result ${index + 1}.`,
    evaluatedAtMillis: now + 20,
  }))
  const requiredStatus = verdicts.includes('fail')
    ? 'fail'
    : verdicts.includes('infra_error')
      ? 'infra_error'
      : verdicts.includes('inconclusive') || unresolvedFindings.length > 0
        ? 'inconclusive'
        : 'pass'
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'verifying',
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: specId,
      deliveryId,
      revision: 1,
      title: 'Delivery Attention fixture',
      goal: 'Classify delivery outcomes without copying runtime logs.',
      scope: ['Delivery outcome classification'],
      outOfScope: ['Execution retries'],
      constraints: ['Codex remains the execution authority'],
      acceptanceCriteria: criteria,
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/repository',
      },
      baseRevision: '1'.repeat(40),
      maxReworkAttempts,
      createdAtMillis: now,
    },
    tasks: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-task-attention',
      deliveryId,
      title: 'Classified work',
      goal: 'Produce the candidate under verification.',
      acceptanceCriterionIds: criteria.map(criterion => criterion.id),
      blockedByTaskIds: [],
      owner: null,
      status: 'verifying',
    }],
    stageRuns: [
      ...Array.from({ length: reworkAttemptsUsed }, (_, index) => ({
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: `stage-attention-rework-${index + 1}`,
        deliveryId,
        deliveryTaskId: 'delivery-task-attention',
        stage: 'reworking',
        actorType: 'codex',
        role: 'remediator',
        status: 'succeeded',
        attempt: index + 1,
        startedAtMillis: now + 2 + (index * 2),
        finishedAtMillis: now + 3 + (index * 2),
      })),
      {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: verificationStageRunId,
      deliveryId,
      deliveryTaskId: 'delivery-task-attention',
      stage: 'verifying',
      actorType: 'codex',
      role: 'verifier',
      status: 'succeeded',
      attempt: 1,
      startedAtMillis: now + 10,
      finishedAtMillis: now + 20,
      },
    ],
    sessionBindings: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: verificationBindingId,
      deliveryId,
      stageRunId: verificationStageRunId,
      dshSessionId: 'dsh-attention-verifier',
      codexSessionId: 'codex-attention-verifier',
      boundAtMillis: now + 11,
    }],
    attentionItems: priorFailureCriterionIds.map((criterionId, index) => ({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `attention-prior-failure-${index + 1}`,
      deliveryId,
      deliverySpecId: specId,
      stageRunId: verificationStageRunId,
      type: 'decision_required',
      title: 'Prior acceptance criterion rework',
      context: JSON.stringify({
        protocol: 'winwincode.delivery-attention.v1',
        verdictId: `delivery-verdict-prior-${index + 1}`,
        candidateRef: `git-candidate:sha256:${String(index + 1).repeat(64).slice(0, 64)}`,
        stageRunId: verificationStageRunId,
        action: 'start-rework',
        criterionResultId: `criterion-result-prior-${index + 1}`,
        criterionId,
        evidenceRefIds: [],
        evidenceRefCount: 0,
        evidenceSetSha256: '4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e8f4b5f1b4c4d8bba3dbda7',
        unresolvedFindings: [],
        unresolvedFindingCount: 0,
        unresolvedFindingSetSha256: '4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e8f4b5f1b4c4d8bba3dbda7',
        reworkAttemptsUsed: index,
        reworkAttemptsLimit: maxReworkAttempts,
        repeatedCriterionFailure: false,
      }),
      options: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'start-rework',
        label: 'Start rework',
        description: 'Open a bounded rework StageRun for the failed criterion.',
      }],
      assignedTo: null,
      blocking: true,
      status: 'resolved',
      resolution: 'Prior rework was approved.',
      resolvedBy: 'reviewer-1',
      createdAtMillis: now + 4 + index,
      resolvedAtMillis: now + 5 + index,
    })),
    evidence,
    verdict: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `delivery-verdict-attention-${requiredStatus}`,
      deliveryId,
      deliverySpecId: specId,
      candidateRef,
      status: requiredStatus,
      criteria: results,
      unresolvedFindings,
      producedAtMillis: now + 20,
    },
    createdAtMillis: now,
    updatedAtMillis: now + 20,
  })
}

function withAttention(delivery, attentionItems) {
  return parseDelivery({
    ...delivery,
    status: 'needs-attention',
    attentionItems,
    updatedAtMillis: now + 21,
  })
}

test('classifies each delivery outcome into a focused next action', async t => {
  const cases = [
    {
      name: 'failed criterion',
      verdicts: ['fail'],
      type: 'decision_required',
      action: 'start-rework',
      nextStatus: 'reworking',
    },
    {
      name: 'infrastructure error',
      verdicts: ['infra_error'],
      type: 'verification_blocked',
      action: 'retry-verification',
      nextStatus: 'verifying',
    },
    {
      name: 'incomplete verification',
      verdicts: ['inconclusive'],
      type: 'verification_blocked',
      action: 'complete-verification',
      nextStatus: 'verifying',
    },
    {
      name: 'contradictory verification',
      verdicts: ['inconclusive'],
      unresolvedFindings: ['contradiction:criterion-attention-1:fail,pass'],
      type: 'decision_required',
      action: 'resolve-verification-conflict',
      nextStatus: 'verifying',
    },
    {
      name: 'unscoped finding',
      verdicts: ['pass'],
      unresolvedFindings: ['unscoped-finding:verifier:finding-outside:fail'],
      type: 'scope_change',
      action: 'clarify-scope',
      nextStatus: 'clarifying',
    },
  ]

  for (const current of cases) {
    await t.test(current.name, () => {
      const delivery = fixture(current)
      const first = deriveDeliveryVerdictAttention({
        delivery,
        verificationStageRunId,
        createdAtMillis: now + 21,
      })
      const replay = deriveDeliveryVerdictAttention({
        delivery,
        verificationStageRunId,
        createdAtMillis: now + 21,
      })
      assert.deepEqual(replay, first)
      assert.equal(first.attentionItems.length, 1)
      const item = first.attentionItems[0]
      assert.equal(item.type, current.type)
      assert.equal(item.stageRunId, verificationStageRunId)
      assert.equal(item.options.length, 1)
      assert.equal(item.options[0].id, current.action)
      const context = JSON.parse(item.context)
      assert.equal(context.verdictId, delivery.verdict.id)
      assert.equal(context.candidateRef, candidateRef)
      assert.equal(context.stageRunId, verificationStageRunId)
      assert.equal(context.action, current.action)
      assert.equal(item.context.includes('Independent verification result'), false)
      const persisted = withAttention(delivery, first.attentionItems)
      assert.deepEqual(assertDeliveryVerdictAttentionCurrent(persisted, item), item)
      assert.equal(
        deliveryVerdictAttentionNextStatus(persisted, item, 'resolved'),
        current.nextStatus,
      )
    })
  }
})

test('a passing verdict opens no delivery outcome Attention', () => {
  const delivery = fixture({ verdicts: ['pass'] })
  const classification = deriveDeliveryVerdictAttention({
    delivery,
    verificationStageRunId,
    createdAtMillis: now + 21,
  })
  assert.deepEqual(classification.attentionItems, [])
})

test('multiple failures remain blocking and resolve to the strongest current action', () => {
  const delivery = fixture({ verdicts: ['fail', 'infra_error'] })
  const classification = deriveDeliveryVerdictAttention({
    delivery,
    verificationStageRunId,
    createdAtMillis: now + 21,
  })
  assert.equal(classification.attentionItems.length, 2)
  const persisted = withAttention(delivery, classification.attentionItems)
  const failureItem = classification.attentionItems.find(item => (
    item.options[0].id === 'start-rework'
  ))
  const infrastructureItem = classification.attentionItems.find(item => (
    item.options[0].id === 'retry-verification'
  ))
  assert.notEqual(failureItem, undefined)
  assert.notEqual(infrastructureItem, undefined)
  assert.equal(
    deliveryVerdictAttentionNextStatus(
      persisted,
      failureItem,
      'resolved',
    ),
    'needs-attention',
  )
  const firstResolved = parseAttentionItem({
    ...failureItem,
    status: 'resolved',
    resolution: 'Acknowledge the classified next action.',
    resolvedBy: 'reviewer-1',
    resolvedAtMillis: now + 22,
  })
  const remaining = withAttention(delivery, [firstResolved, infrastructureItem])
  assert.equal(
    deliveryVerdictAttentionNextStatus(
      remaining,
      infrastructureItem,
      'resolved',
    ),
    'reworking',
  )
})

test('exhausted and repeated failures stop automatic code rework', () => {
  const exhausted = fixture({
    verdicts: ['fail'],
    maxReworkAttempts: 1,
    reworkAttemptsUsed: 1,
  })
  const [exhaustedAttention] = deriveDeliveryVerdictAttention({
    delivery: exhausted,
    verificationStageRunId,
    createdAtMillis: now + 21,
  }).attentionItems
  assert.equal(exhaustedAttention.type, 'scope_change')
  assert.equal(exhaustedAttention.options[0].id, 'clarify-scope')
  assert.deepEqual(
    Object.fromEntries(
      ['reworkAttemptsUsed', 'reworkAttemptsLimit', 'repeatedCriterionFailure']
        .map(key => [key, JSON.parse(exhaustedAttention.context)[key]]),
    ),
    {
      reworkAttemptsUsed: 1,
      reworkAttemptsLimit: 1,
      repeatedCriterionFailure: false,
    },
  )

  const repeated = fixture({
    verdicts: ['fail'],
    maxReworkAttempts: 3,
    reworkAttemptsUsed: 1,
    priorFailureCriterionIds: ['criterion-attention-1'],
  })
  const [repeatedAttention] = deriveDeliveryVerdictAttention({
    delivery: repeated,
    verificationStageRunId,
    createdAtMillis: now + 21,
  }).attentionItems
  assert.equal(repeatedAttention.type, 'scope_change')
  assert.equal(repeatedAttention.options[0].id, 'clarify-scope')
  assert.equal(JSON.parse(repeatedAttention.context).repeatedCriterionFailure, true)
})

test('stale, mutated, dismissed, and unbound delivery outcome Attention fails closed', () => {
  const delivery = fixture({ verdicts: ['fail'] })
  const [item] = deriveDeliveryVerdictAttention({
    delivery,
    verificationStageRunId,
    createdAtMillis: now + 21,
  }).attentionItems
  const persisted = withAttention(delivery, [item])
  assert.throws(
    () => assertDeliveryVerdictAttentionCurrent(persisted, {
      ...item,
      title: 'Caller replaced the classified action.',
    }),
    error => error instanceof DeliveryAttentionClassificationError
      && error.code === 'ATTENTION_STALE',
  )
  assert.throws(
    () => deliveryVerdictAttentionNextStatus(persisted, item, 'dismissed'),
    error => error instanceof DeliveryAttentionClassificationError
      && error.code === 'ATTENTION_NON_ACTIONABLE',
  )
  assert.throws(
    () => deriveDeliveryVerdictAttention({
      delivery,
      verificationStageRunId: 'stage-foreign',
      createdAtMillis: now + 21,
    }),
    error => error instanceof DeliveryAttentionClassificationError
      && error.code === 'VERIFICATION_STAGE_MISMATCH',
  )

  const multiple = fixture({ verdicts: ['fail', 'infra_error'] })
  const classified = deriveDeliveryVerdictAttention({
    delivery: multiple,
    verificationStageRunId,
    createdAtMillis: now + 21,
  })
  const incompleteSet = withAttention(multiple, [classified.attentionItems[0]])
  assert.throws(
    () => assertDeliveryVerdictAttentionCurrent(
      incompleteSet,
      classified.attentionItems[0],
    ),
    error => error instanceof DeliveryAttentionClassificationError
      && error.code === 'ATTENTION_STALE',
  )
})
