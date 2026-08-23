import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseDelivery,
} from '../packages/contracts/dist/index.js'
import { CodexRuntimeProjector } from '../packages/dsh-profile/dist/index.js'
import {
  DeliveryCandidateEvidenceError,
  assertFrozenDeliveryCandidateCurrent,
  freezeAcceptanceVerificationInput,
  freezeDeliveryCandidate,
  resolveDeliveryEvidence,
} from '../packages/strongflow/dist/index.js'

const now = 2_500_000_000_000
const deliveryId = 'delivery-candidate-evidence'
const executorStageRunId = 'stage-candidate-executor'
const executorBindingId = 'binding-candidate-executor'
const verifierStageRunId = 'stage-candidate-verifier'
const verifierBindingId = 'binding-candidate-verifier'
const dshVerifierSessionId = 'dsh-candidate-verifier'
const codexVerifierSessionId = 'codex-candidate-verifier'
const exactDiff = [
  '--- a/src/result.ts',
  '+++ b/src/result.ts',
  '@@ -1 +1 @@',
  '-old',
  '+new',
  '',
].join('\n')

function deliveryFixture({ specRevision = 1, verifierRole = 'verifier' } = {}) {
  const specId = `delivery-spec-candidate-v${specRevision}`
  const criterionId = `criterion-candidate-v${specRevision}`
  const taskId = 'delivery-task-candidate'
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 12 + specRevision,
    status: 'verifying',
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: specId,
      deliveryId,
      revision: specRevision,
      title: 'Bind direct evidence',
      goal: 'Resolve existing Codex and Git facts without running another executor.',
      scope: ['Candidate identity and acceptance evidence'],
      outOfScope: ['A second command or Agent runtime'],
      constraints: ['Codex RuntimeSessionLedger remains the execution source'],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: criterionId,
        description: 'The exact frozen candidate passes its declared test.',
        verificationMethod: 'Run pnpm test in the read-only verifier session.',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/candidate',
      },
      baseRevision: '1'.repeat(40),
      maxReworkAttempts: 2,
      createdAtMillis: now + specRevision,
    },
    tasks: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: taskId,
      deliveryId,
      title: 'Candidate evidence',
      goal: 'Bind direct facts to one independently verifiable delivery unit.',
      acceptanceCriterionIds: [criterionId],
      blockedByTaskIds: [],
      owner: null,
      status: 'verifying',
    }],
    stageRuns: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-candidate-plan-review',
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
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: verifierStageRunId,
        deliveryId,
        deliveryTaskId: taskId,
        stage: 'verifying',
        actorType: 'codex',
        role: verifierRole,
        status: 'running',
        attempt: 1,
        startedAtMillis: now + 60,
        finishedAtMillis: null,
      },
    ],
    sessionBindings: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-candidate-plan-review',
        deliveryId,
        stageRunId: 'stage-candidate-plan-review',
        dshSessionId: 'dsh-candidate-plan-review',
        codexSessionId: null,
        boundAtMillis: now + 11,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: executorBindingId,
        deliveryId,
        stageRunId: executorStageRunId,
        dshSessionId: 'dsh-candidate-executor',
        codexSessionId: 'codex-candidate-executor',
        boundAtMillis: now + 31,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: verifierBindingId,
        deliveryId,
        stageRunId: verifierStageRunId,
        dshSessionId: dshVerifierSessionId,
        codexSessionId: codexVerifierSessionId,
        boundAtMillis: now + 61,
      },
    ],
    attentionItems: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'attention-candidate-plan-review',
      deliveryId,
      deliverySpecId: specId,
      stageRunId: 'stage-candidate-plan-review',
      type: 'decision_required',
      title: 'Approve the candidate acceptance definition',
      context: `Approve ${specId} before implementation and evidence collection.`,
      options: [],
      assignedTo: 'reviewer-candidate',
      blocking: true,
      status: 'resolved',
      resolution: `Approved ${specId}.`,
      resolvedBy: 'reviewer-candidate',
      createdAtMillis: now + 12,
      resolvedAtMillis: now + 19,
    }],
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 61,
  })
}

function frozenCandidate(delivery, overrides = {}) {
  return freezeDeliveryCandidate(delivery, {
    producerStageRunId: executorStageRunId,
    producerSessionBindingId: executorBindingId,
    baseCommitId: '1'.repeat(40),
    baseTreeId: '2'.repeat(40),
    candidateCommitId: '3'.repeat(40),
    candidateTreeId: '4'.repeat(40),
    diffSha256: createHash('sha256').update(exactDiff).digest('hex'),
    changedPaths: [
      { path: 'src/removed.ts', state: 'deleted', objectId: null },
      { path: 'src/result.ts', state: 'present', objectId: '5'.repeat(40) },
    ],
    ...overrides,
  })
}

function kernelEvent(sequence, type, data = {}) {
  const payload = { id: 'candidate-evidence-submission', msg: { type, ...data } }
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

function verifierEvents(candidate, {
  successfulWrite = false,
  roleId = 'verifier',
  sessionConfiguration = readOnlySessionConfiguration(),
} = {}) {
  const source = [
    kernelEvent(1, 'session_configured', {
      session_id: codexVerifierSessionId,
      thread_id: codexVerifierSessionId,
      model_provider_id: 'fixture-provider',
      model: 'fixture-model',
      ...sessionConfiguration,
    }),
    kernelEvent(2, 'item_completed', {
      thread_id: codexVerifierSessionId,
      completed_at_ms: now + 70,
      item: {
        type: 'CommandExecution',
        id: 'test-success',
        command: ['pnpm', 'test'],
        status: 'completed',
        exit_code: 0,
      },
    }),
    kernelEvent(3, 'item_completed', {
      thread_id: codexVerifierSessionId,
      completed_at_ms: now + 71,
      item: {
        type: 'CommandExecution',
        id: 'test-task-failure',
        command: ['pnpm', 'test'],
        status: 'failed',
        exit_code: 1,
      },
    }),
    kernelEvent(4, 'item_completed', {
      thread_id: codexVerifierSessionId,
      completed_at_ms: now + 72,
      item: {
        type: 'CommandExecution',
        id: 'test-timeout',
        command: ['pnpm', 'test'],
        status: 'failed',
        exit_code: 124,
        formatted_output: 'command timed out after 30000 milliseconds',
      },
    }),
    kernelEvent(5, 'item_completed', {
      thread_id: codexVerifierSessionId,
      completed_at_ms: now + 73,
      item: {
        type: 'CommandExecution',
        id: 'test-policy-denial',
        command: ['pnpm', 'test'],
        status: 'sandbox-denied',
      },
    }),
    kernelEvent(6, 'error', {
      occurred_at_ms: now + 74,
      message: 'The model transport closed before verification settled.',
      codex_error_info: 'response_stream_disconnected',
    }),
    kernelEvent(7, 'turn_diff', {
      occurred_at_ms: now + 75,
      unified_diff: exactDiff,
    }),
    kernelEvent(8, 'item_completed', {
      thread_id: codexVerifierSessionId,
      completed_at_ms: now + 76,
      evidence_type: 'file',
      candidate_ref: candidate.candidateRef,
      path: 'src/result.ts',
      object_id: '5'.repeat(40),
      item: { type: 'DynamicToolCall', id: 'file-fact', status: 'completed' },
    }),
    kernelEvent(9, 'item_completed', {
      thread_id: codexVerifierSessionId,
      completed_at_ms: now + 77,
      evidence_type: 'commit',
      candidate_ref: candidate.candidateRef,
      candidate_commit_id: candidate.candidateCommitId,
      item: { type: 'DynamicToolCall', id: 'commit-fact', status: 'completed' },
    }),
    kernelEvent(10, 'agent_message', {
      occurred_at_ms: now + 78,
      phase: 'final_answer',
      message: JSON.stringify({
        protocol: 'winwincode.independent-verification-result.v1',
        delivery_spec_id: candidate.deliverySpecId,
        delivery_spec_revision: candidate.deliverySpecRevision,
        candidate_ref: candidate.candidateRef,
        findings: [{
          finding_id: 'finding-candidate-one',
          criterion_id: 'criterion-candidate-v1',
          verdict: 'pass',
          explanation: 'The exact candidate satisfies the inspected boundary.',
          evidence_sources: [{
            type: 'test',
            event_id: `${dshVerifierSessionId}@2`,
          }],
        }],
      }),
    }),
  ]
  if (successfulWrite) {
    source.push(kernelEvent(11, 'item_completed', {
      thread_id: codexVerifierSessionId,
      completed_at_ms: now + 79,
      item: {
        type: 'FileChange',
        id: 'forbidden-verifier-write',
        status: 'completed',
      },
    }))
  }
  return new CodexRuntimeProjector({
    sessionId: dshVerifierSessionId,
    kernelSessionId: codexVerifierSessionId,
    roleId,
    kernelStreamId: 'stream-candidate-verifier',
  }).replay(source)
}

function evidenceInput(delivery, candidate, events, overrides = {}) {
  return {
    delivery,
    acceptance: freezeAcceptanceVerificationInput(delivery),
    candidate,
    evidenceId: 'evidence-candidate-runtime',
    stageRunId: verifierStageRunId,
    sessionBindingId: verifierBindingId,
    source: { kind: 'runtime-event', type: 'test', eventId: `${dshVerifierSessionId}@2` },
    runtimeEvents: events,
    createdAtMillis: now + 90,
    ...overrides,
  }
}

function expectEvidenceError(code) {
  return error => error instanceof DeliveryCandidateEvidenceError && error.code === code
}

test('freezes exact producer Git facts without creating another persisted domain object', () => {
  const delivery = deliveryFixture()
  const candidate = frozenCandidate(delivery)

  assert.match(candidate.candidateRef, /^git-candidate:sha256:[0-9a-f]{64}$/u)
  assert.equal(candidate.deliverySpecRevision, delivery.spec.revision)
  assert.equal(candidate.producerStageRunId, executorStageRunId)
  assert.equal(candidate.producerSessionBindingId, executorBindingId)
  assert.deepEqual(candidate.changedPaths.map(entry => entry.path), [
    'src/removed.ts',
    'src/result.ts',
  ])
  assert.equal(Object.isFrozen(candidate), true)
  assert.equal(Object.isFrozen(candidate.changedPaths), true)
  assert.deepEqual(assertFrozenDeliveryCandidateCurrent(delivery, candidate), candidate)
  assert.equal(frozenCandidate(delivery, {
    changedPaths: [...candidate.changedPaths].reverse(),
  }).candidateRef, candidate.candidateRef)
})

test('rejects malformed or internally inconsistent candidate facts', () => {
  const delivery = deliveryFixture()
  assert.throws(
    () => frozenCandidate(delivery, { baseCommitId: 'a'.repeat(40) }),
    expectEvidenceError('INVALID_CANDIDATE'),
  )
  assert.throws(
    () => frozenCandidate(delivery, { candidateTreeId: '4'.repeat(64) }),
    expectEvidenceError('INVALID_CANDIDATE'),
  )
  assert.throws(
    () => frozenCandidate(delivery, { unexpected: true }),
    expectEvidenceError('INVALID_CANDIDATE'),
  )
})

test('resolves runtime and Git facts to exact spec, candidate, stage, and session identities', () => {
  const delivery = deliveryFixture()
  const candidate = frozenCandidate(delivery)
  const events = verifierEvents(candidate)
  const runtime = resolveDeliveryEvidence(evidenceInput(delivery, candidate, events))

  assert.deepEqual(runtime, {
    schemaVersion: 1,
    evidence: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'evidence-candidate-runtime',
      deliveryId,
      deliverySpecId: delivery.spec.id,
      deliverySpecRevision: delivery.spec.revision,
      stageRunId: verifierStageRunId,
      sessionBindingId: verifierBindingId,
      candidateRef: candidate.candidateRef,
      type: 'test',
      sourceRef: `runtime_event:${dshVerifierSessionId}@2`,
      createdAtMillis: now + 90,
    },
    outcome: 'succeeded',
    eventId: `${dshVerifierSessionId}@2`,
  })

  const directSources = [
    ['candidate-commit', 'commit', `git_commit:${candidate.candidateCommitId}`],
    ['candidate-diff', 'diff', `git_diff:sha256:${candidate.diffSha256}`],
    [
      'candidate-file',
      'file',
      `git_file:${candidate.candidateTreeId}:src%2Fresult.ts@${'5'.repeat(40)}`,
    ],
  ]
  for (const [kind, type, sourceRef] of directSources) {
    const source = kind === 'candidate-file'
      ? { kind, path: 'src/result.ts' }
      : { kind }
    const resolved = resolveDeliveryEvidence(evidenceInput(delivery, candidate, [], {
      evidenceId: `evidence-${kind}`,
      stageRunId: executorStageRunId,
      sessionBindingId: executorBindingId,
      source,
    }))
    assert.equal(resolved.evidence.type, type)
    assert.equal(resolved.evidence.sourceRef, sourceRef)
    assert.equal(resolved.evidence.sessionBindingId, executorBindingId)
    assert.equal(resolved.outcome, 'observed')
    assert.equal(resolved.eventId, null)
  }

  for (const [sequence, type] of [[7, 'diff'], [8, 'file'], [9, 'commit'], [10, 'review_finding']]) {
    const resolved = resolveDeliveryEvidence(evidenceInput(delivery, candidate, events, {
      evidenceId: `evidence-runtime-${type}`,
      source: { kind: 'runtime-event', type, eventId: `${dshVerifierSessionId}@${sequence}` },
    }))
    assert.equal(resolved.evidence.type, type)
    assert.equal(resolved.evidence.candidateRef, candidate.candidateRef)
  }
})

test('keeps timeout, policy denial, task failure, and infrastructure failure distinct', () => {
  const delivery = deliveryFixture()
  const candidate = frozenCandidate(delivery)
  const events = verifierEvents(candidate)
  const cases = [
    [3, 'test', 'task-failed'],
    [4, 'test', 'timed-out'],
    [5, 'test', 'policy-denied'],
    [6, 'runtime_event', 'infrastructure-failed'],
  ]

  for (const [sequence, type, outcome] of cases) {
    const resolved = resolveDeliveryEvidence(evidenceInput(delivery, candidate, events, {
      evidenceId: `evidence-outcome-${outcome}`,
      source: { kind: 'runtime-event', type, eventId: `${dshVerifierSessionId}@${sequence}` },
    }))
    assert.equal(resolved.outcome, outcome)
  }
})

test('rejects missing, foreign, type-mismatched, and candidate-mismatched source facts', () => {
  const delivery = deliveryFixture()
  const candidate = frozenCandidate(delivery)
  const events = verifierEvents(candidate)

  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(delivery, candidate, events, {
      source: { kind: 'runtime-event', type: 'test', eventId: `${dshVerifierSessionId}@99` },
    })),
    expectEvidenceError('EVIDENCE_SOURCE_MISSING'),
  )
  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(delivery, candidate, events, {
      source: { kind: 'runtime-event', type: 'command', eventId: `${dshVerifierSessionId}@2` },
    })),
    expectEvidenceError('EVIDENCE_TYPE_MISMATCH'),
  )
  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(delivery, candidate, events, {
      sessionBindingId: executorBindingId,
    })),
    expectEvidenceError('EVIDENCE_SESSION_MISMATCH'),
  )
  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(delivery, candidate, events, {
      evidenceId: undefined,
    })),
    expectEvidenceError('INVALID_EVIDENCE'),
  )
  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(delivery, candidate, events, {
      source: { kind: 'invented-source' },
    })),
    expectEvidenceError('INVALID_EVIDENCE'),
  )

  const wrongDiff = events.map(event => event.id === `${dshVerifierSessionId}@7`
    ? Object.freeze({ ...event, data: Object.freeze({ ...event.data, unified_diff: 'changed' }) })
    : event)
  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(delivery, candidate, wrongDiff, {
      source: { kind: 'runtime-event', type: 'diff', eventId: `${dshVerifierSessionId}@7` },
    })),
    expectEvidenceError('EVIDENCE_CANDIDATE_MISMATCH'),
  )
})

test('invalidates prior acceptance and candidate identities after definition or writer changes', () => {
  const delivery = deliveryFixture()
  const acceptance = freezeAcceptanceVerificationInput(delivery)
  const candidate = frozenCandidate(delivery)
  const revised = deliveryFixture({ specRevision: 2 })

  assert.throws(
    () => resolveDeliveryEvidence({
      ...evidenceInput(revised, frozenCandidate(revised), [], {
        stageRunId: executorStageRunId,
        sessionBindingId: executorBindingId,
        source: { kind: 'candidate-commit' },
      }),
      acceptance,
    }),
    expectEvidenceError('ACCEPTANCE_STALE'),
  )

  const laterWriter = parseDelivery({
    ...delivery,
    revision: delivery.revision + 1,
    stageRuns: [
      ...delivery.stageRuns,
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-candidate-remediator',
        deliveryId,
        deliveryTaskId: 'delivery-task-candidate',
        stage: 'reworking',
        actorType: 'codex',
        role: 'remediator',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: now + 100,
        finishedAtMillis: now + 110,
      },
    ],
    sessionBindings: [
      ...delivery.sessionBindings,
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-candidate-remediator',
        deliveryId,
        stageRunId: 'stage-candidate-remediator',
        dshSessionId: 'dsh-candidate-remediator',
        codexSessionId: 'codex-candidate-remediator',
        boundAtMillis: now + 101,
      },
    ],
    updatedAtMillis: now + 110,
  })
  assert.throws(
    () => assertFrozenDeliveryCandidateCurrent(laterWriter, candidate),
    expectEvidenceError('CANDIDATE_STALE'),
  )

  const runningWriter = parseDelivery({
    ...delivery,
    revision: delivery.revision + 1,
    stageRuns: [
      ...delivery.stageRuns,
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-candidate-running-remediator',
        deliveryId,
        deliveryTaskId: 'delivery-task-candidate',
        stage: 'reworking',
        actorType: 'codex',
        role: 'remediator',
        status: 'running',
        attempt: 2,
        startedAtMillis: now + 100,
        finishedAtMillis: null,
      },
    ],
    sessionBindings: [
      ...delivery.sessionBindings,
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-candidate-running-remediator',
        deliveryId,
        stageRunId: 'stage-candidate-running-remediator',
        dshSessionId: 'dsh-candidate-running-remediator',
        codexSessionId: 'codex-candidate-running-remediator',
        boundAtMillis: now + 101,
      },
    ],
    updatedAtMillis: now + 101,
  })
  assert.throws(
    () => assertFrozenDeliveryCandidateCurrent(runningWriter, candidate),
    expectEvidenceError('CANDIDATE_STALE'),
  )
})

test('requires a read-only verification role and rejects any successful verifier write', () => {
  const delivery = deliveryFixture()
  const candidate = frozenCandidate(delivery)
  const eventsWithWrite = verifierEvents(candidate, { successfulWrite: true })
  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(delivery, candidate, eventsWithWrite)),
    expectEvidenceError('VERIFIER_WRITE_OBSERVED'),
  )

  const writableConfiguration = readOnlySessionConfiguration()
  writableConfiguration.permission_profile.file_system.entries.push({
    path: { type: 'special', value: { kind: 'project_roots' } },
    access: 'write',
  })
  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(
      delivery,
      candidate,
      verifierEvents(candidate, { sessionConfiguration: writableConfiguration }),
    )),
    expectEvidenceError('VERIFIER_POLICY_MISMATCH'),
  )

  const writerVerifier = deliveryFixture({ verifierRole: 'executor' })
  const writerCandidate = frozenCandidate(writerVerifier)
  const writerEvents = verifierEvents(writerCandidate, { roleId: 'executor' })
  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(writerVerifier, writerCandidate, writerEvents)),
    expectEvidenceError('VERIFIER_POLICY_MISMATCH'),
  )

  const wrongReadOnlyRole = deliveryFixture({ verifierRole: 'requirement-analyst' })
  const wrongRoleCandidate = frozenCandidate(wrongReadOnlyRole)
  const wrongRoleEvents = verifierEvents(wrongRoleCandidate, { roleId: 'requirement-analyst' })
  assert.throws(
    () => resolveDeliveryEvidence(evidenceInput(
      wrongReadOnlyRole,
      wrongRoleCandidate,
      wrongRoleEvents,
    )),
    expectEvidenceError('VERIFIER_POLICY_MISMATCH'),
  )
})
