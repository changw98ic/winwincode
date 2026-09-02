import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseDelivery,
} from '../packages/contracts/dist/index.js'
import { CodexRuntimeProjector } from '../packages/dsh-profile/dist/index.js'
import {
  IndependentVerificationError,
  createIndependentVerificationAssignment,
  freezeAcceptanceVerificationInput,
  freezeDeliveryCandidate,
  projectIndependentVerification,
  serializeIndependentVerificationSessionInput,
} from '../packages/strongflow/dist/index.js'

const now = 2_600_000_000_000
const deliveryId = 'dlv_07PBAQ2TGNJV2KWF4JCS6960Z3'
const specId = 'delivery-spec-independent-verification'
const criterionId = 'criterion-independent-verification'
const taskId = 'delivery-task-independent-verification'
const executorStageRunId = 'stage-independent-executor'
const executorBindingId = 'binding-independent-executor'
const reviewerStageRunId = 'stage-independent-reviewer'
const reviewerBindingId = 'binding-independent-reviewer'
const verifierStageRunId = 'stage-independent-verifier'
const verifierBindingId = 'binding-independent-verifier'

function deliveryFixture({ includeReviewer = true } = {}) {
  const reviewerRun = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: reviewerStageRunId,
    deliveryId,
    deliveryTaskId: taskId,
    stage: 'verifying',
    actorType: 'codex',
    role: 'reviewer',
    status: 'succeeded',
    attempt: 1,
    startedAtMillis: now + 60,
    finishedAtMillis: now + 80,
  }
  const reviewerBinding = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: reviewerBindingId,
    deliveryId,
    stageRunId: reviewerStageRunId,
    dshSessionId: 'dsh-independent-reviewer',
    codexSessionId: 'codex-independent-reviewer',
    boundAtMillis: now + 61,
  }
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
      title: 'Independent review and verification',
      goal: 'Keep independent Codex results tied to one approved candidate.',
      scope: ['Reviewer and verifier evidence'],
      outOfScope: ['A second Agent scheduler or mailbox'],
      constraints: ['Codex owns Agent lifecycle and graph state'],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: criterionId,
        description: 'The frozen candidate passes the declared behavior check.',
        verificationMethod: 'Run the declared test against the read-only candidate.',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/independent-verification',
      },
      baseRevision: '1'.repeat(40),
      maxReworkAttempts: 2,
      createdAtMillis: now + 1,
    },
    tasks: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: taskId,
      deliveryId,
      title: 'Independent verification',
      goal: 'Verify one independently deliverable candidate.',
      acceptanceCriterionIds: [criterionId],
      blockedByTaskIds: [],
      owner: null,
      status: 'verifying',
    }],
    stageRuns: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-independent-plan-review',
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
      ...(includeReviewer ? [reviewerRun] : []),
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: verifierStageRunId,
        deliveryId,
        deliveryTaskId: taskId,
        stage: 'verifying',
        actorType: 'codex',
        role: 'verifier',
        status: 'running',
        attempt: 1,
        startedAtMillis: now + 90,
        finishedAtMillis: null,
      },
    ],
    sessionBindings: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-independent-plan-review',
        deliveryId,
        stageRunId: 'stage-independent-plan-review',
        dshSessionId: 'dsh-independent-plan-review',
        codexSessionId: null,
        boundAtMillis: now + 11,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: executorBindingId,
        deliveryId,
        stageRunId: executorStageRunId,
        dshSessionId: 'dsh-independent-executor',
        codexSessionId: 'codex-independent-executor',
        boundAtMillis: now + 31,
      },
      ...(includeReviewer ? [reviewerBinding] : []),
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: verifierBindingId,
        deliveryId,
        stageRunId: verifierStageRunId,
        dshSessionId: 'dsh-independent-verifier',
        codexSessionId: 'codex-independent-verifier',
        boundAtMillis: now + 91,
      },
    ],
    attentionItems: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'attention-independent-plan-review',
      deliveryId,
      deliverySpecId: specId,
      stageRunId: 'stage-independent-plan-review',
      type: 'decision_required',
      title: 'Approve independent verification inputs',
      context: 'Approve the exact spec before implementation and verification.',
      options: [],
      assignedTo: 'human-reviewer',
      blocking: true,
      status: 'resolved',
      resolution: 'Approved the exact spec and checks.',
      resolvedBy: 'human-reviewer',
      createdAtMillis: now + 12,
      resolvedAtMillis: now + 19,
    }],
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 91,
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
  verdict,
  complete = true,
  candidateRef = candidate.candidateRef,
  successfulWrite = false,
}) {
  const dshSessionId = `dsh-independent-${role}`
  const codexSessionId = `codex-independent-${role}`
  const command = role === 'reviewer' ? ['git', 'diff', '--check'] : ['pnpm', 'test']
  const events = [
    kernelEvent(1, 'session_configured', {
      session_id: codexSessionId,
      thread_id: codexSessionId,
      occurred_at_ms: now + (role === 'reviewer' ? 62 : 92),
      ...readOnlySessionConfiguration(),
    }),
    kernelEvent(2, 'task_started', {
      turn_id: `turn-${role}`,
      started_at_ms: now + (role === 'reviewer' ? 63 : 93),
    }),
    kernelEvent(3, 'item_completed', {
      turn_id: `turn-${role}`,
      completed_at_ms: now + (role === 'reviewer' ? 64 : 94),
      item: {
        type: 'CommandExecution',
        id: `check-${role}`,
        command,
        status: 'completed',
        exit_code: 0,
      },
    }),
  ]
  if (role === 'reviewer') {
    events.push(kernelEvent(4, 'collab_agent_spawn_end', {
      turn_id: `turn-${role}`,
      sender_thread_id: codexSessionId,
      new_thread_id: 'codex-independent-reviewer-child',
      new_agent_nickname: 'boundary-checker',
      new_agent_role: 'reviewer',
      status: 'running',
    }))
  }
  if (complete) {
    const findingSequence = events.length + 1
    events.push(
      kernelEvent(findingSequence, 'agent_message', {
        turn_id: `turn-${role}`,
        occurred_at_ms: now + (role === 'reviewer' ? 65 : 95),
        phase: 'final_answer',
        message: JSON.stringify({
          protocol: 'winwincode.independent-verification-result.v1',
          delivery_spec_id: specId,
          delivery_spec_revision: 1,
          candidate_ref: candidateRef,
          findings: [{
            finding_id: `finding-${role}`,
            criterion_id: criterionId,
            verdict,
            explanation: `${role} independently evaluated the declared criterion.`,
            evidence_sources: [{
              type: role === 'reviewer' ? 'command' : 'test',
              event_id: `${dshSessionId}@3`,
            }],
          }],
        }),
      }),
      kernelEvent(findingSequence + 1, 'task_complete', {
        turn_id: `turn-${role}`,
        completed_at_ms: now + (role === 'reviewer' ? 66 : 96),
        last_agent_message: `${role} complete`,
        error: null,
      }),
    )
  }
  if (successfulWrite) {
    events.push(kernelEvent(events.length + 1, 'item_completed', {
      turn_id: `turn-${role}`,
      completed_at_ms: now + (role === 'reviewer' ? 67 : 97),
      item: {
        type: 'FileChange',
        id: `write-${role}`,
        status: 'completed',
      },
    }))
  }
  return new CodexRuntimeProjector({
    sessionId: dshSessionId,
    kernelSessionId: codexSessionId,
    roleId: role,
    kernelStreamId: `stream-${role}`,
  }).replay(events)
}

function correctedRoleEvents({ role, candidate, verdict }) {
  const dshSessionId = `dsh-independent-${role}`
  const codexSessionId = `codex-independent-${role}`
  const command = role === 'reviewer' ? ['git', 'diff', '--check'] : ['pnpm', 'test']
  return new CodexRuntimeProjector({
    sessionId: dshSessionId,
    kernelSessionId: codexSessionId,
    roleId: role,
    kernelStreamId: `stream-${role}`,
  }).replay([
    kernelEvent(1, 'session_configured', {
      session_id: codexSessionId,
      thread_id: codexSessionId,
      occurred_at_ms: now + 62,
      ...readOnlySessionConfiguration(),
    }),
    kernelEvent(2, 'task_started', {
      turn_id: `turn-${role}-check`,
      started_at_ms: now + 63,
    }),
    kernelEvent(3, 'item_completed', {
      turn_id: `turn-${role}-check`,
      completed_at_ms: now + 64,
      item: {
        type: 'CommandExecution',
        id: `check-${role}`,
        command,
        status: 'completed',
        exit_code: 0,
      },
    }),
    kernelEvent(4, 'agent_message', {
      turn_id: `turn-${role}-check`,
      occurred_at_ms: now + 65,
      phase: 'final_answer',
      message: JSON.stringify({
        protocol: 'winwincode.independent-verification-result.v1',
        candidate_ref: candidate.candidateRef,
      }),
    }),
    kernelEvent(5, 'task_complete', {
      turn_id: `turn-${role}-check`,
      completed_at_ms: now + 66,
      last_agent_message: `${role} malformed result`,
      error: null,
    }),
    kernelEvent(6, 'task_started', {
      turn_id: `turn-${role}-correction`,
      started_at_ms: now + 67,
    }),
    kernelEvent(7, 'agent_message', {
      turn_id: `turn-${role}-correction`,
      occurred_at_ms: now + 68,
      phase: 'final_answer',
      message: JSON.stringify({
        protocol: 'winwincode.independent-verification-result.v1',
        delivery_spec_id: specId,
        delivery_spec_revision: 1,
        candidate_ref: candidate.candidateRef,
        findings: [{
          finding_id: `finding-${role}-corrected`,
          criterion_id: criterionId,
          verdict,
          explanation: `${role} corrected the result without changing the observed evidence.`,
          evidence_sources: [{
            type: role === 'reviewer' ? 'command' : 'test',
            event_id: `${dshSessionId}@3`,
          }],
        }],
      }),
    }),
    kernelEvent(8, 'task_complete', {
      turn_id: `turn-${role}-correction`,
      completed_at_ms: now + 69,
      last_agent_message: `${role} corrected result`,
      error: null,
    }),
  ])
}

function inputs(delivery, candidate, runtimeEvents, overrides = {}) {
  return {
    delivery,
    acceptance: freezeAcceptanceVerificationInput(delivery),
    candidate,
    runtimeEvents,
    ...overrides,
  }
}

function expectVerificationError(code) {
  return error => error instanceof IndependentVerificationError && error.code === code
}

test('binds exact approved inputs to separate existing reviewer and verifier sessions', () => {
  const delivery = deliveryFixture()
  const candidate = candidateFor(delivery)
  const acceptance = freezeAcceptanceVerificationInput(delivery)
  const reviewer = createIndependentVerificationAssignment({
    delivery,
    acceptance,
    candidate,
    stageRunId: reviewerStageRunId,
    sessionBindingId: reviewerBindingId,
  })
  const verifier = createIndependentVerificationAssignment({
    delivery,
    acceptance,
    candidate,
    stageRunId: verifierStageRunId,
    sessionBindingId: verifierBindingId,
  })

  assert.notEqual(reviewer.assignmentRef, verifier.assignmentRef)
  assert.notEqual(reviewer.sessionBindingId, executorBindingId)
  assert.notEqual(verifier.sessionBindingId, executorBindingId)
  assert.equal(reviewer.role, 'reviewer')
  assert.equal(verifier.role, 'verifier')
  assert.equal(reviewer.sessionInput.deliverySpec.id, specId)
  assert.equal(reviewer.sessionInput.acceptance.freezeId, acceptance.freezeId)
  assert.equal(reviewer.sessionInput.candidate.candidateRef, candidate.candidateRef)
  assert.equal(reviewer.sessionInput.resultContract.channel, 'codex-final-response')
  assert.equal(
    reviewer.sessionInput.resultContract.protocol,
    'winwincode.independent-verification-result.v1',
  )
  assert.deepEqual(
    reviewer.sessionInput.resultContract.requiredEvidenceSourceFields,
    ['type', 'event_id'],
  )
  assert.deepEqual(
    JSON.parse(serializeIndependentVerificationSessionInput(reviewer)),
    reviewer.sessionInput,
  )
  assert.equal(Object.isFrozen(reviewer.sessionInput.deliverySpec), true)

  assert.throws(
    () => createIndependentVerificationAssignment({
      delivery,
      acceptance,
      candidate,
      stageRunId: executorStageRunId,
      sessionBindingId: executorBindingId,
    }),
    expectVerificationError('VERIFICATION_STAGE_MISMATCH'),
  )
})

test('projects independent settlements and preserves contradictory findings', () => {
  const delivery = deliveryFixture()
  const candidate = candidateFor(delivery)
  const events = [
    ...roleEvents({ role: 'reviewer', candidate, verdict: 'fail' }),
    ...roleEvents({ role: 'verifier', candidate, verdict: 'pass' }),
  ]
  const projection = projectIndependentVerification(inputs(delivery, candidate, events))

  assert.deepEqual(projection.requiredSettlements.map(entry => [entry.role, entry.state]), [
    ['reviewer', 'settled'],
    ['verifier', 'settled'],
  ])
  assert.deepEqual(projection.findings.map(finding => [
    finding.role,
    finding.verdict,
    finding.deliverySpecId,
    finding.deliverySpecRevision,
    finding.candidateRef,
  ]), [
    ['reviewer', 'fail', specId, 1, candidate.candidateRef],
    ['verifier', 'pass', specId, 1, candidate.candidateRef],
  ])
  assert.deepEqual(projection.contradictions, [{
    criterionId,
    verdicts: ['pass', 'fail'],
    findingEventIds: [
      'dsh-independent-reviewer@5',
      'dsh-independent-verifier@4',
    ],
  }])
  assert.equal(projection.sessions[0].runtimeSession.agents[0].threadId, 'codex-independent-reviewer')
  assert.equal(
    projection.sessions[0].runtimeSession.agents[1].threadId,
    'codex-independent-reviewer-child',
  )
  assert.equal(projection.sessions[1].runtimeSession.agents[0].threadId, 'codex-independent-verifier')
  assert.deepEqual(projection.sessions.flatMap(session => session.findings).map(finding => (
    finding.supportingEvidence[0].outcome
  )), ['succeeded', 'succeeded'])
})

test('keeps missing, running, and optional adversarial settlements visible', () => {
  const delivery = deliveryFixture({ includeReviewer: false })
  const candidate = candidateFor(delivery)
  const events = roleEvents({
    role: 'verifier',
    candidate,
    verdict: 'pass',
    complete: false,
  })
  const projection = projectIndependentVerification(inputs(delivery, candidate, events, {
    requiredRoles: ['reviewer', 'verifier', 'adversarial-verifier'],
  }))

  assert.deepEqual(projection.requiredSettlements.map(entry => [entry.role, entry.state]), [
    ['reviewer', 'missing'],
    ['verifier', 'running'],
    ['adversarial-verifier', 'missing'],
  ])
  assert.equal(projection.findings.length, 0)
})

test('rejects foreign result identities, role drift, and verifier writes', () => {
  const delivery = deliveryFixture()
  const candidate = candidateFor(delivery)
  const reviewer = roleEvents({ role: 'reviewer', candidate, verdict: 'pass' })

  assert.throws(
    () => projectIndependentVerification(inputs(delivery, candidate, [
      ...reviewer,
      ...roleEvents({
        role: 'verifier',
        candidate,
        verdict: 'pass',
        candidateRef: 'git-candidate:sha256:'.concat('f'.repeat(64)),
      }),
    ])),
    expectVerificationError('RESULT_IDENTITY_MISMATCH'),
  )

  const wrongRoleEvents = new CodexRuntimeProjector({
    sessionId: 'dsh-independent-verifier',
    kernelSessionId: 'codex-independent-verifier',
    roleId: 'reviewer',
    kernelStreamId: 'stream-wrong-role',
  }).replay([
    kernelEvent(1, 'session_configured'),
  ])
  assert.throws(
    () => projectIndependentVerification(inputs(delivery, candidate, [
      ...reviewer,
      ...wrongRoleEvents,
    ])),
    expectVerificationError('VERIFICATION_SESSION_MISMATCH'),
  )

  assert.throws(
    () => projectIndependentVerification(inputs(delivery, candidate, [
      ...reviewer,
      ...roleEvents({
        role: 'verifier',
        candidate,
        verdict: 'pass',
        successfulWrite: true,
      }),
    ])),
    expectVerificationError('VERIFICATION_POLICY_MISMATCH'),
  )
})

test('rejects malformed finding payloads and caller attempts to omit required roles', () => {
  const delivery = deliveryFixture()
  const candidate = candidateFor(delivery)
  const malformed = roleEvents({ role: 'reviewer', candidate, verdict: 'pass' }).map(event => (
    event.id === 'dsh-independent-reviewer@5'
      ? Object.freeze({
          ...event,
          semantic: undefined,
          data: Object.freeze({
            ...event.data,
            message: JSON.stringify({
              protocol: 'winwincode.independent-verification-result.v1',
              candidate_ref: candidate.candidateRef,
            }),
          }),
        })
      : event
  ))
  assert.throws(
    () => projectIndependentVerification(inputs(delivery, candidate, malformed)),
    expectVerificationError('RESULT_INVALID'),
  )
  assert.throws(
    () => projectIndependentVerification(inputs(delivery, candidate, [], {
      requiredRoles: ['verifier'],
    })),
    expectVerificationError('INVALID_INPUT'),
  )
})

test('keeps malformed attempts in the ledger but accepts findings only from the latest corrected turn', () => {
  const delivery = deliveryFixture()
  const candidate = candidateFor(delivery)
  const reviewer = correctedRoleEvents({ role: 'reviewer', candidate, verdict: 'pass' })
  const projection = projectIndependentVerification(inputs(delivery, candidate, [
    ...reviewer,
    ...roleEvents({ role: 'verifier', candidate, verdict: 'pass' }),
  ]))

  assert.equal(reviewer.some(event => event.id === 'dsh-independent-reviewer@4'), true)
  assert.deepEqual(
    projection.sessions.find(session => session.role === 'reviewer').findings.map(finding => (
      finding.event.eventId
    )),
    ['dsh-independent-reviewer@7'],
  )
  assert.equal(
    projection.findings.some(finding => finding.event.eventId === 'dsh-independent-reviewer@4'),
    false,
  )
})
