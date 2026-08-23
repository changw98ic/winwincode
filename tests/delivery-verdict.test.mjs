import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseDelivery,
} from '../packages/contracts/dist/index.js'
import { CodexRuntimeProjector } from '../packages/dsh-profile/dist/index.js'
import {
  DeliveryVerdictComputationError,
  computeDeliveryVerdict,
  freezeAcceptanceVerificationInput,
  freezeDeliveryCandidate,
} from '../packages/strongflow/dist/index.js'

const now = 2_700_000_000_000
const deliveryId = 'delivery-verdict-computation'
const specId = 'spec-verdict-computation'
const requiredCriterionId = 'criterion-required'
const optionalCriterionId = 'criterion-optional'
const taskId = 'task-verdict-computation'
const executorStageRunId = 'stage-verdict-executor'
const executorBindingId = 'binding-verdict-executor'

const roleIdentity = Object.freeze({
  reviewer: Object.freeze({
    stageRunId: 'stage-verdict-reviewer',
    bindingId: 'binding-verdict-reviewer',
    dshSessionId: 'dsh-verdict-reviewer',
    codexSessionId: 'codex-verdict-reviewer',
    startedAtMillis: now + 60,
  }),
  verifier: Object.freeze({
    stageRunId: 'stage-verdict-verifier',
    bindingId: 'binding-verdict-verifier',
    dshSessionId: 'dsh-verdict-verifier',
    codexSessionId: 'codex-verdict-verifier',
    startedAtMillis: now + 90,
  }),
})

function deliveryFixture({ includeReviewer = true, blockingAttention = false } = {}) {
  const roles = includeReviewer ? ['reviewer', 'verifier'] : ['verifier']
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 10,
    status: 'verifying',
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: specId,
      deliveryId,
      revision: 1,
      title: 'Compute a fail-closed verdict',
      goal: 'Use direct evidence from the current candidate and independent sessions.',
      scope: ['Deterministic acceptance'],
      outOfScope: ['A second test runner'],
      constraints: ['Codex remains the execution authority'],
      acceptanceCriteria: [
        {
          schemaVersion: DELIVERY_SCHEMA_VERSION,
          id: requiredCriterionId,
          description: 'The required behavior is verified.',
          verificationMethod: 'Run the declared check against the frozen candidate.',
          required: true,
        },
        {
          schemaVersion: DELIVERY_SCHEMA_VERSION,
          id: optionalCriterionId,
          description: 'An optional observation is recorded when available.',
          verificationMethod: 'Inspect the optional behavior.',
          required: false,
        },
      ],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/verdict-computation',
      },
      baseRevision: '1'.repeat(40),
      maxReworkAttempts: 2,
      createdAtMillis: now + 1,
    },
    tasks: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: taskId,
      deliveryId,
      title: 'Compute verdict',
      goal: 'Bind exact independent evidence.',
      acceptanceCriterionIds: [requiredCriterionId, optionalCriterionId],
      blockedByTaskIds: [],
      owner: null,
      status: 'verifying',
    }],
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
        startedAtMillis: now + 10,
        finishedAtMillis: now + 20,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: executorStageRunId,
        deliveryId,
        deliveryTaskId: taskId,
        stage: 'executing',
        actorType: 'codex',
        role: 'executor',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: now + 30,
        finishedAtMillis: now + 50,
      },
      ...roles.map((role) => {
        const identity = roleIdentity[role]
        return {
          schemaVersion: DELIVERY_SCHEMA_VERSION,
          id: identity.stageRunId,
          deliveryId,
          deliveryTaskId: taskId,
          stage: 'verifying',
          actorType: 'codex',
          role,
          status: role === 'reviewer' ? 'succeeded' : 'running',
          attempt: 1,
          startedAtMillis: identity.startedAtMillis,
          finishedAtMillis: role === 'reviewer' ? now + 80 : null,
        }
      }),
    ],
    sessionBindings: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-verdict-plan-review',
        deliveryId,
        stageRunId: 'stage-verdict-plan-review',
        dshSessionId: 'dsh-verdict-plan-review',
        codexSessionId: null,
        boundAtMillis: now + 11,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: executorBindingId,
        deliveryId,
        stageRunId: executorStageRunId,
        dshSessionId: 'dsh-verdict-executor',
        codexSessionId: 'codex-verdict-executor',
        boundAtMillis: now + 31,
      },
      ...roles.map((role) => {
        const identity = roleIdentity[role]
        return {
          schemaVersion: DELIVERY_SCHEMA_VERSION,
          id: identity.bindingId,
          deliveryId,
          stageRunId: identity.stageRunId,
          dshSessionId: identity.dshSessionId,
          codexSessionId: identity.codexSessionId,
          boundAtMillis: identity.startedAtMillis + 1,
        }
      }),
    ],
    attentionItems: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'attention-verdict-plan-review',
        deliveryId,
        deliverySpecId: specId,
        stageRunId: 'stage-verdict-plan-review',
        type: 'decision_required',
        title: 'Approve the acceptance definition',
        context: 'Approve the exact requirement and solution before execution.',
        options: [],
        assignedTo: 'human-reviewer',
        blocking: true,
        status: 'resolved',
        resolution: 'Approved.',
        resolvedBy: 'human-reviewer',
        createdAtMillis: now + 12,
        resolvedAtMillis: now + 19,
      },
      ...(blockingAttention ? [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'attention-verdict-current-blocker',
        deliveryId,
        deliverySpecId: specId,
        stageRunId: roleIdentity.verifier.stageRunId,
        type: 'verification_blocked',
        title: 'Verification decision remains open',
        context: 'A current blocking decision has not been resolved.',
        options: [],
        assignedTo: 'human-reviewer',
        blocking: true,
        status: 'open',
        resolution: null,
        resolvedBy: null,
        createdAtMillis: now + 92,
        resolvedAtMillis: null,
      }] : []),
    ],
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 92,
  })
}

function candidateFor(delivery) {
  const diff = '--- a/src/value.ts\n+++ b/src/value.ts\n@@ -1 +1 @@\n-old\n+new\n'
  return freezeDeliveryCandidate(delivery, {
    producerStageRunId: executorStageRunId,
    producerSessionBindingId: executorBindingId,
    baseCommitId: '1'.repeat(40),
    baseTreeId: '2'.repeat(40),
    candidateCommitId: '3'.repeat(40),
    candidateTreeId: '4'.repeat(40),
    diffSha256: createHash('sha256').update(diff).digest('hex'),
    changedPaths: [{
      path: 'src/value.ts',
      state: 'present',
      objectId: '5'.repeat(40),
    }],
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

function roleEvents({
  role,
  candidate,
  verdict = 'pass',
  evidence = true,
  outcome = 'succeeded',
  criterionId = requiredCriterionId,
  complete = true,
  candidateRef = candidate.candidateRef,
}) {
  const identity = roleIdentity[role]
  const commandType = role === 'reviewer' ? 'command' : 'test'
  const status = outcome === 'task-failed'
    ? 'failed'
    : outcome === 'timed-out'
      ? 'timed-out'
      : 'completed'
  const events = [
    kernelEvent(1, 'session_configured', {
      session_id: identity.codexSessionId,
      thread_id: identity.codexSessionId,
      occurred_at_ms: identity.startedAtMillis + 2,
      ...readOnlySessionConfiguration(),
    }),
    kernelEvent(2, 'task_started', {
      turn_id: `turn-${role}`,
      started_at_ms: identity.startedAtMillis + 3,
    }),
    kernelEvent(3, 'item_completed', {
      turn_id: `turn-${role}`,
      completed_at_ms: identity.startedAtMillis + 4,
      item: {
        type: 'CommandExecution',
        id: `check-${role}`,
        command: role === 'reviewer' ? ['git', 'diff', '--check'] : ['pnpm', 'test'],
        status,
        exit_code: outcome === 'task-failed' ? 1 : 0,
        timed_out: outcome === 'timed-out',
      },
    }),
  ]
  if (complete) {
    events.push(
      kernelEvent(4, 'agent_message', {
        turn_id: `turn-${role}`,
        occurred_at_ms: identity.startedAtMillis + 5,
        phase: 'final_answer',
        message: JSON.stringify({
          protocol: 'winwincode.independent-verification-result.v1',
          delivery_spec_id: specId,
          delivery_spec_revision: 1,
          candidate_ref: candidateRef,
          findings: [{
            finding_id: `finding-${role}-${criterionId ?? 'unscoped'}`,
            criterion_id: criterionId,
            verdict,
            explanation: `${role} evaluated ${criterionId ?? 'an unscoped risk'}.`,
            evidence_sources: evidence ? [{
              type: commandType,
              event_id: `${identity.dshSessionId}@3`,
            }] : [],
          }],
        }),
      }),
      kernelEvent(5, 'task_complete', {
        turn_id: `turn-${role}`,
        completed_at_ms: identity.startedAtMillis + 6,
        last_agent_message: `${role} complete`,
        error: null,
      }),
    )
  }
  return new CodexRuntimeProjector({
    sessionId: identity.dshSessionId,
    kernelSessionId: identity.codexSessionId,
    roleId: role,
    kernelStreamId: `stream-${role}`,
  }).replay(events)
}

function failedRoleEvents(role) {
  const identity = roleIdentity[role]
  return new CodexRuntimeProjector({
    sessionId: identity.dshSessionId,
    kernelSessionId: identity.codexSessionId,
    roleId: role,
    kernelStreamId: `stream-${role}`,
  }).replay([
    kernelEvent(1, 'session_configured', {
      session_id: identity.codexSessionId,
      thread_id: identity.codexSessionId,
      occurred_at_ms: identity.startedAtMillis + 2,
      ...readOnlySessionConfiguration(),
    }),
    kernelEvent(2, 'task_started', {
      turn_id: `turn-${role}`,
      started_at_ms: identity.startedAtMillis + 3,
    }),
    kernelEvent(3, 'error', {
      turn_id: `turn-${role}`,
      occurred_at_ms: identity.startedAtMillis + 4,
      message: 'fixture transport failure',
      code: 'transport',
    }),
  ])
}

function roleEventsWithSupersededFailure(role, candidate) {
  const identity = roleIdentity[role]
  const type = role === 'reviewer' ? 'command' : 'test'
  const resultMessage = (verdict, evidenceSequence) => JSON.stringify({
    protocol: 'winwincode.independent-verification-result.v1',
    delivery_spec_id: specId,
    delivery_spec_revision: 1,
    candidate_ref: candidate.candidateRef,
    findings: [{
      finding_id: `finding-${role}-${verdict}`,
      criterion_id: requiredCriterionId,
      verdict,
      explanation: `${role} produced the ${verdict} result.`,
      evidence_sources: [{
        type,
        event_id: `${identity.dshSessionId}@${String(evidenceSequence)}`,
      }],
    }],
  })
  return new CodexRuntimeProjector({
    sessionId: identity.dshSessionId,
    kernelSessionId: identity.codexSessionId,
    roleId: role,
    kernelStreamId: `stream-${role}`,
  }).replay([
    kernelEvent(1, 'session_configured', {
      session_id: identity.codexSessionId,
      thread_id: identity.codexSessionId,
      occurred_at_ms: identity.startedAtMillis + 2,
      ...readOnlySessionConfiguration(),
    }),
    kernelEvent(2, 'task_started', { turn_id: `turn-${role}-old` }),
    kernelEvent(3, 'item_completed', {
      turn_id: `turn-${role}-old`,
      item: {
        type: 'CommandExecution',
        id: `check-${role}-old`,
        command: role === 'reviewer' ? ['git', 'diff', '--check'] : ['pnpm', 'test'],
        status: 'failed',
        exit_code: 1,
      },
    }),
    kernelEvent(4, 'agent_message', {
      turn_id: `turn-${role}-old`,
      phase: 'final_answer',
      message: resultMessage('fail', 3),
    }),
    kernelEvent(5, 'task_complete', {
      turn_id: `turn-${role}-old`,
      last_agent_message: 'old result',
      error: null,
    }),
    kernelEvent(6, 'task_started', { turn_id: `turn-${role}-current` }),
    kernelEvent(7, 'item_completed', {
      turn_id: `turn-${role}-current`,
      item: {
        type: 'CommandExecution',
        id: `check-${role}-current`,
        command: role === 'reviewer' ? ['git', 'diff', '--check'] : ['pnpm', 'test'],
        status: 'completed',
        exit_code: 0,
      },
    }),
    kernelEvent(8, 'agent_message', {
      turn_id: `turn-${role}-current`,
      phase: 'final_answer',
      message: resultMessage('pass', 7),
    }),
    kernelEvent(9, 'task_complete', {
      turn_id: `turn-${role}-current`,
      last_agent_message: 'current result',
      error: null,
    }),
  ])
}

function compute(delivery, candidate, runtimeEvents) {
  return computeDeliveryVerdict({
    delivery,
    acceptance: freezeAcceptanceVerificationInput(delivery),
    candidate,
    runtimeEvents,
    producedAtMillis: now + 200,
  })
}

function completeEvents(candidate, overrides = {}) {
  return [
    ...roleEvents({ role: 'reviewer', candidate, ...overrides.reviewer }),
    ...roleEvents({ role: 'verifier', candidate, ...overrides.verifier }),
  ]
}

function expectComputationError(code) {
  return error => error instanceof DeliveryVerdictComputationError && error.code === code
}

test('computes one deterministic passing verdict from current direct evidence', () => {
  const delivery = deliveryFixture()
  const candidate = candidateFor(delivery)
  const events = completeEvents(candidate)
  const first = compute(delivery, candidate, events)
  const replay = compute(delivery, candidate, structuredClone(events))

  assert.deepEqual(replay, first)
  assert.equal(first.verdict.status, 'pass')
  assert.deepEqual(first.verdict.criteria.map(result => [result.criterionId, result.verdict]), [
    [requiredCriterionId, 'pass'],
    [optionalCriterionId, 'inconclusive'],
  ])
  assert.equal(first.verdict.unresolvedFindings.length, 0)
  assert.equal(first.evidence.length, 4)
  assert.equal(first.verdict.criteria[0].evidenceRefs.length, 4)
  assert.equal(Object.isFrozen(first.verdict.criteria), true)
})

test('uses only the latest settled final response from each verification session', () => {
  const delivery = deliveryFixture()
  const candidate = candidateFor(delivery)
  const result = compute(delivery, candidate, [
    ...roleEventsWithSupersededFailure('reviewer', candidate),
    ...roleEvents({ role: 'verifier', candidate }),
  ])

  assert.equal(result.verdict.status, 'pass')
  assert.equal(result.verdict.unresolvedFindings.length, 0)
  assert.match(result.verdict.criteria[0].explanation, /finding-reviewer-pass/u)
  assert.doesNotMatch(result.verdict.criteria[0].explanation, /finding-reviewer-fail/u)
})

test('fails closed for each non-passing verification condition', async (t) => {
  const cases = [
    {
      name: 'missing required reviewer',
      delivery: deliveryFixture({ includeReviewer: false }),
      events: candidate => roleEvents({ role: 'verifier', candidate }),
      status: 'inconclusive',
    },
    {
      name: 'running verifier',
      delivery: deliveryFixture(),
      events: candidate => [
        ...roleEvents({ role: 'reviewer', candidate }),
        ...roleEvents({ role: 'verifier', candidate, complete: false }),
      ],
      status: 'inconclusive',
    },
    {
      name: 'failed verification session',
      delivery: deliveryFixture(),
      events: candidate => [
        ...roleEvents({ role: 'reviewer', candidate }),
        ...failedRoleEvents('verifier'),
      ],
      status: 'infra_error',
    },
    {
      name: 'explicit failed criterion',
      delivery: deliveryFixture(),
      events: candidate => completeEvents(candidate, {
        reviewer: { verdict: 'fail', outcome: 'task-failed' },
        verifier: { verdict: 'fail', outcome: 'task-failed' },
      }),
      status: 'fail',
    },
    {
      name: 'explicit inconclusive criterion',
      delivery: deliveryFixture(),
      events: candidate => completeEvents(candidate, {
        reviewer: { verdict: 'inconclusive', evidence: false },
        verifier: { verdict: 'inconclusive', evidence: false },
      }),
      status: 'inconclusive',
    },
    {
      name: 'infrastructure evidence outcome',
      delivery: deliveryFixture(),
      events: candidate => completeEvents(candidate, {
        reviewer: { verdict: 'infra_error', outcome: 'timed-out' },
        verifier: { verdict: 'infra_error', outcome: 'timed-out' },
      }),
      status: 'infra_error',
    },
    {
      name: 'missing direct evidence',
      delivery: deliveryFixture(),
      events: candidate => completeEvents(candidate, {
        reviewer: { evidence: false },
        verifier: { evidence: false },
      }),
      status: 'inconclusive',
    },
    {
      name: 'contradictory independent findings',
      delivery: deliveryFixture(),
      events: candidate => completeEvents(candidate, {
        reviewer: { verdict: 'fail', outcome: 'task-failed' },
        verifier: { verdict: 'pass' },
      }),
      status: 'inconclusive',
      unresolved: 'contradiction:',
    },
    {
      name: 'pass finding cites failed evidence',
      delivery: deliveryFixture(),
      events: candidate => completeEvents(candidate, {
        reviewer: { verdict: 'pass', outcome: 'task-failed' },
      }),
      status: 'inconclusive',
      unresolved: 'evidence-mismatch:',
    },
    {
      name: 'open blocking Attention',
      delivery: deliveryFixture({ blockingAttention: true }),
      events: candidate => completeEvents(candidate),
      status: 'inconclusive',
      unresolved: 'blocking-attention:',
    },
    {
      name: 'unscoped finding',
      delivery: deliveryFixture(),
      events: candidate => [
        ...roleEvents({ role: 'reviewer', candidate, criterionId: null, verdict: 'fail' }),
        ...roleEvents({ role: 'verifier', candidate }),
      ],
      status: 'inconclusive',
      unresolved: 'unscoped-finding:',
    },
  ]

  for (const scenario of cases) {
    await t.test(scenario.name, () => {
      const candidate = candidateFor(scenario.delivery)
      const result = compute(scenario.delivery, candidate, scenario.events(candidate))
      assert.equal(result.verdict.status, scenario.status)
      assert.notEqual(result.verdict.status, 'pass')
      if (scenario.unresolved !== undefined) {
        assert.equal(
          result.verdict.unresolvedFindings.some(value => value.startsWith(scenario.unresolved)),
          true,
        )
      }
    })
  }
})

test('rejects stale acceptance, stale candidate, and foreign result identities', async (t) => {
  const delivery = deliveryFixture()
  const acceptance = freezeAcceptanceVerificationInput(delivery)
  const candidate = candidateFor(delivery)
  const events = completeEvents(candidate)

  await t.test('stale acceptance', () => {
    const changed = parseDelivery({
      ...delivery,
      spec: { ...delivery.spec, revision: 2 },
      updatedAtMillis: now + 100,
    })
    assert.throws(
      () => computeDeliveryVerdict({
        delivery: changed,
        acceptance,
        candidate,
        runtimeEvents: events,
        producedAtMillis: now + 200,
      }),
      expectComputationError('ACCEPTANCE_STALE'),
    )
  })

  await t.test('stale candidate', () => {
    assert.throws(
      () => computeDeliveryVerdict({
        delivery,
        acceptance,
        candidate: { ...candidate, candidateTreeId: '6'.repeat(40) },
        runtimeEvents: events,
        producedAtMillis: now + 200,
      }),
      expectComputationError('CANDIDATE_STALE'),
    )
  })

  await t.test('foreign result candidate', () => {
    assert.throws(
      () => computeDeliveryVerdict({
        delivery,
        acceptance,
        candidate,
        runtimeEvents: completeEvents(candidate, {
          verifier: { candidateRef: `git-candidate:sha256:${'f'.repeat(64)}` },
        }),
        producedAtMillis: now + 200,
      }),
      expectComputationError('VERIFICATION_INVALID'),
    )
  })
})
