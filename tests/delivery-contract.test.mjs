import assert from 'node:assert/strict'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  DeliveryId,
  DeliveryValidationError,
  StageRunId,
  parseDelivery,
  parseDeliverySpec,
} from '../packages/contracts/dist/index.js'

const now = 1_800_000_000_000

function deliveryFixture() {
  const deliveryId = 'dlv_01J00000000000000000000000'
  const specId = 'delivery-spec-v1'
  const candidateRef = 'git-tree:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
  const requiredCriterionId = 'criterion-required'
  const optionalCriterionId = 'criterion-optional'
  const stageRunId = 'stage-verification-1'
  const evidenceId = 'evidence-test-1'
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 7,
    status: 'ready-to-deliver',
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: specId,
      deliveryId,
      revision: 1,
      title: 'Implement invitation flow',
      goal: 'Users can invite a teammate and verify the result.',
      scope: ['Invitation data and API'],
      outOfScope: ['Organization billing'],
      constraints: ['Keep the existing authentication boundary'],
      acceptanceCriteria: [
        {
          schemaVersion: DELIVERY_SCHEMA_VERSION,
          id: requiredCriterionId,
          description: 'A valid invitation can be accepted exactly once.',
          verificationMethod: 'Run the invitation integration test.',
          required: true,
        },
        {
          schemaVersion: DELIVERY_SCHEMA_VERSION,
          id: optionalCriterionId,
          description: 'The empty state remains readable at narrow widths.',
          verificationMethod: null,
          required: false,
        },
      ],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/repository',
      },
      baseRevision: '0123456789012345678901234567890123456789',
      maxReworkAttempts: 2,
      createdAtMillis: now,
    },
    tasks: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'delivery-task-api',
        deliveryId,
        title: 'Invitation API',
        goal: 'Deliver the independently verifiable invitation endpoint.',
        acceptanceCriterionIds: [requiredCriterionId],
        blockedByTaskIds: [],
        owner: 'backend-owner',
        status: 'completed',
      },
    ],
    stageRuns: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: stageRunId,
        deliveryId,
        deliveryTaskId: 'delivery-task-api',
        stage: 'verifying',
        actorType: 'codex',
        role: 'verifier',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: now + 10,
        finishedAtMillis: now + 20,
      },
    ],
    sessionBindings: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-verifier-1',
        deliveryId,
        stageRunId,
        dshSessionId: 'dsh-session-verifier',
        codexSessionId: 'codex-session-verifier',
        boundAtMillis: now + 11,
      },
    ],
    attentionItems: [],
    evidence: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: evidenceId,
        deliveryId,
        deliverySpecId: specId,
        deliverySpecRevision: 1,
        stageRunId,
        sessionBindingId: 'binding-verifier-1',
        candidateRef,
        type: 'test',
        sourceRef: 'runtime-event:codex-session-verifier/42',
        createdAtMillis: now + 19,
      },
    ],
    verdict: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-verdict-1',
      deliveryId,
      deliverySpecId: specId,
      candidateRef,
      status: 'pass',
      criteria: [
        {
          schemaVersion: DELIVERY_SCHEMA_VERSION,
          id: 'criterion-result-required',
          deliveryId,
          deliverySpecId: specId,
          criterionId: requiredCriterionId,
          candidateRef,
          verdict: 'pass',
          evidenceRefs: [evidenceId],
          explanation: 'The direct integration test passed.',
          evaluatedAtMillis: now + 21,
        },
        {
          schemaVersion: DELIVERY_SCHEMA_VERSION,
          id: 'criterion-result-optional',
          deliveryId,
          deliverySpecId: specId,
          criterionId: optionalCriterionId,
          candidateRef,
          verdict: 'inconclusive',
          evidenceRefs: [],
          explanation: 'No required visual probe was defined.',
          evaluatedAtMillis: now + 21,
        },
      ],
      unresolvedFindings: [],
      producedAtMillis: now + 22,
    },
    createdAtMillis: now,
    updatedAtMillis: now + 23,
  }
}

function expectDeliveryError(code, action) {
  assert.throws(action, error => (
    error instanceof DeliveryValidationError && error.code === code
  ))
}

test('canonical Delivery round-trips and freezes every owned fact', () => {
  const parsed = parseDelivery(JSON.parse(JSON.stringify(deliveryFixture())))
  assert.equal(parsed.id, DeliveryId('dlv_01J00000000000000000000000'))
  assert.equal(parsed.stageRuns[0].id, StageRunId('stage-verification-1'))
  assert.equal(parsed.verdict.status, 'pass')
  assert.equal(parsed.verdict.criteria[1].verdict, 'inconclusive')
  assert.ok(Object.isFrozen(parsed))
  assert.ok(Object.isFrozen(parsed.spec))
  assert.ok(Object.isFrozen(parsed.spec.acceptanceCriteria))
  assert.ok(Object.isFrozen(parsed.tasks))
  assert.ok(Object.isFrozen(parsed.stageRuns))
  assert.ok(Object.isFrozen(parsed.sessionBindings))
  assert.ok(Object.isFrozen(parsed.evidence))
  assert.ok(Object.isFrozen(parsed.verdict.criteria))
})

test('DeliveryId accepts only the canonical dlv_ identity', () => {
  assert.equal(
    DeliveryId('dlv_01J00000000000000000000000'),
    'dlv_01J00000000000000000000000',
  )
  for (const value of [
    'github-issue:example/widget:42',
    'delivery-main',
    'dlv_01I00000000000000000000000',
    'dlv_01j00000000000000000000000',
  ]) {
    expectDeliveryError('INVALID_IDENTIFIER', () => DeliveryId(value))
  }
})

test('DeliverySpec requires delivery boundaries and a bounded rework limit', () => {
  const fixture = deliveryFixture().spec
  expectDeliveryError('INVALID_VALUE', () => parseDeliverySpec({
    ...fixture,
    acceptanceCriteria: fixture.acceptanceCriteria.map(criterion => ({
      ...criterion,
      required: false,
    })),
  }))
  expectDeliveryError('INVALID_SHAPE', () => parseDeliverySpec({
    ...fixture,
    repository: undefined,
  }))
  expectDeliveryError('INVALID_VALUE', () => parseDeliverySpec({
    ...fixture,
    baseRevision: '',
  }))
  expectDeliveryError('INVALID_VALUE', () => parseDeliverySpec({
    ...fixture,
    scope: [],
  }))
  expectDeliveryError('INVALID_VALUE', () => parseDeliverySpec({
    ...fixture,
    maxReworkAttempts: -1,
  }))
  expectDeliveryError('INVALID_VALUE', () => parseDeliverySpec({
    ...fixture,
    maxReworkAttempts: 101,
  }))
  assert.equal(parseDeliverySpec({
    ...fixture,
    maxReworkAttempts: 0,
  }).maxReworkAttempts, 0)
})

test('strict contracts reject extra execution-owned fields', () => {
  const fixture = deliveryFixture()
  expectDeliveryError('INVALID_SHAPE', () => parseDelivery({
    ...fixture,
    codexPlan: [{ step: 'This belongs to Codex' }],
  }))
  expectDeliveryError('INVALID_SHAPE', () => parseDelivery({
    ...fixture,
    stageRuns: [{
      ...fixture.stageRuns[0],
      subagents: ['agent-1'],
    }],
  }))
})

test('Delivery accepts Delivery- or task-scoped Codex remediators within the approved limit', () => {
  const fixture = deliveryFixture()
  const reworkRun = (index, role = 'remediator', deliveryTaskId = fixture.tasks[0].id) => ({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `stage-rework-${index}`,
    deliveryId: fixture.id,
    deliveryTaskId,
    stage: 'reworking',
    actorType: 'codex',
    role,
    status: 'succeeded',
    attempt: index,
    startedAtMillis: now + 30 + index,
    finishedAtMillis: now + 40 + index,
  })
  assert.doesNotThrow(() => parseDelivery({
    ...fixture,
    stageRuns: [...fixture.stageRuns, reworkRun(1, 'remediator', null)],
  }))
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    stageRuns: [...fixture.stageRuns, reworkRun(1, 'executor')],
  }))
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    stageRuns: [
      ...fixture.stageRuns,
      reworkRun(1),
      reworkRun(2),
      reworkRun(3),
    ],
  }))
})

test('Delivery rejects duplicate and cyclic delivery-significant tasks', () => {
  const fixture = deliveryFixture()
  expectDeliveryError('DUPLICATE_ID', () => parseDelivery({
    ...fixture,
    tasks: [fixture.tasks[0], fixture.tasks[0]],
  }))
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    tasks: [
      { ...fixture.tasks[0], blockedByTaskIds: ['delivery-task-ui'] },
      {
        ...fixture.tasks[0],
        id: 'delivery-task-ui',
        blockedByTaskIds: ['delivery-task-api'],
      },
    ],
  }))
})

test('Codex StageRuns require a Codex session binding and current relationships', () => {
  const fixture = deliveryFixture()
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    sessionBindings: [{
      ...fixture.sessionBindings[0],
      codexSessionId: null,
    }],
  }))
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    evidence: [{
      ...fixture.evidence[0],
      stageRunId: 'stage-run-foreign',
    }],
  }))
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    evidence: [{
      ...fixture.evidence[0],
      deliverySpecRevision: fixture.spec.revision + 1,
    }],
  }))
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    evidence: [{
      ...fixture.evidence[0],
      sessionBindingId: 'binding-verifier-foreign',
    }],
  }))
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    evidence: [{
      ...fixture.evidence[0],
      createdAtMillis: fixture.sessionBindings[0].boundAtMillis - 1,
    }],
  }))
})

test('passing and delivered states fail closed on stale or failed evidence', () => {
  const fixture = deliveryFixture()
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    evidence: [{
      ...fixture.evidence[0],
      candidateRef: 'git-tree:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
    }],
  }))
  expectDeliveryError('INVALID_VERDICT', () => parseDelivery({
    ...fixture,
    verdict: {
      ...fixture.verdict,
      criteria: fixture.verdict.criteria.map((criterion, index) => (
        index === 0 ? { ...criterion, verdict: 'fail' } : criterion
      )),
    },
  }))
  expectDeliveryError('INVALID_VERDICT', () => parseDelivery({
    ...fixture,
    verdict: {
      ...fixture.verdict,
      status: 'fail',
    },
  }))
  expectDeliveryError('INVALID_VERDICT', () => parseDelivery({
    ...fixture,
    status: 'delivered',
    verdict: {
      ...fixture.verdict,
      status: 'inconclusive',
    },
  }))
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    verdict: {
      ...fixture.verdict,
      criteria: fixture.verdict.criteria.map((criterion, index) => (
        index === 0
          ? { ...criterion, evaluatedAtMillis: fixture.verdict.producedAtMillis + 1 }
          : criterion
      )),
    },
  }))
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    evidence: [{
      ...fixture.evidence[0],
      createdAtMillis: fixture.verdict.criteria[0].evaluatedAtMillis + 1,
    }],
  }))
  expectDeliveryError('INVALID_VERDICT', () => parseDelivery({
    ...fixture,
    attentionItems: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'attention-open-verdict-blocker',
      deliveryId: fixture.id,
      deliverySpecId: fixture.spec.id,
      stageRunId: fixture.stageRuns[0].id,
      type: 'verification_blocked',
      title: 'Resolve the current verification blocker',
      context: 'The current verdict still depends on an open decision.',
      options: [],
      assignedTo: 'reviewer-1',
      blocking: true,
      status: 'open',
      resolution: null,
      resolvedBy: null,
      createdAtMillis: now + 20,
      resolvedAtMillis: null,
    }],
  }))
})

test('needs-attention requires a durable open blocking item', () => {
  const fixture = deliveryFixture()
  expectDeliveryError('RELATIONSHIP_MISMATCH', () => parseDelivery({
    ...fixture,
    status: 'needs-attention',
    verdict: null,
  }))
  const parsed = parseDelivery({
    ...fixture,
    status: 'needs-attention',
    verdict: null,
    attentionItems: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'attention-scope-choice',
      deliveryId: fixture.id,
      deliverySpecId: fixture.spec.id,
      stageRunId: fixture.stageRuns[0].id,
      type: 'scope_change',
      title: 'Choose the supported invitation lifetime',
      context: 'The approved scope does not define an expiry duration.',
      options: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'seven-days',
        label: 'Seven days',
        description: 'Expire unused invitations after seven days.',
      }],
      assignedTo: 'requester-1',
      blocking: true,
      status: 'open',
      resolution: null,
      resolvedBy: null,
      createdAtMillis: now + 22,
      resolvedAtMillis: null,
    }],
  })
  assert.equal(parsed.attentionItems[0].status, 'open')
})
