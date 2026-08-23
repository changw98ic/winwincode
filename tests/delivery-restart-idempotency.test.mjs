import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseDelivery,
} from '../packages/contracts/dist/index.js'
import { CodexRuntimeProjector } from '../packages/dsh-profile/dist/index.js'
import {
  DeliveryStore,
  StrongFlowService,
  freezeDeliveryCandidate,
} from '../packages/strongflow/dist/index.js'

const baseTime = 2_600_000_000_000
const serviceTime = baseTime + 1_000
const baseRevision = '1'.repeat(40)

const authenticator = Object.freeze({
  async authenticate(request) {
    return request.channel === 'local-ui'
      && request.authentication.scheme === 'local-session'
      && request.authentication.proof === 'restart-proof-value'
      ? Object.freeze({ actorId: 'restart-reviewer' })
      : undefined
  },
})

function service(home) {
  return new StrongFlowService({ home, authenticator, clock: () => serviceTime })
}

function spec(deliveryId, revision, suffix) {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `spec-${deliveryId}-${suffix}`,
    deliveryId,
    revision,
    title: `Restart delivery ${suffix}`,
    goal: `Keep the ${suffix} delivery facts durable across host restart.`,
    scope: [`Restart scope ${suffix}`],
    outOfScope: ['Codex tool and subagent recovery'],
    constraints: ['Codex remains the execution authority'],
    acceptanceCriteria: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `criterion-${deliveryId}-${suffix}`,
      description: `The ${suffix} candidate passes its direct check.`,
      verificationMethod: 'Run the exact read-only verifier command.',
      required: true,
    }],
    sourceRef: null,
    publicationTarget: null,
    repository: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      kind: 'local-git',
      locator: '/workspace/restart-idempotency',
    },
    baseRevision,
    maxReworkAttempts: 1,
    createdAtMillis: baseTime + revision,
  }
}

function deliveryTask(deliveryId, criterionId, status = 'pending') {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `task-${deliveryId}`,
    deliveryId,
    title: 'Restart-safe delivery task',
    goal: 'Produce one independently verified candidate.',
    acceptanceCriterionIds: [criterionId],
    blockedByTaskIds: [],
    owner: 'restart-owner',
    status,
  }
}

function kernelEvent(sequence, type, data = {}) {
  const payload = { id: `restart-submission-${sequence}`, msg: { type, ...data } }
  return Object.freeze({
    sequence: BigInt(sequence),
    kind: type,
    payload,
    rawJson: JSON.stringify(payload),
  })
}

function readOnlyConfiguration() {
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

function candidateFor(delivery, producerStageRunId, producerSessionBindingId, generation) {
  const diff = [
    '--- a/src/restart.ts',
    '+++ b/src/restart.ts',
    '@@ -1 +1 @@',
    '-before',
    `+after-${generation}`,
    '',
  ].join('\n')
  const digits = generation === 'original'
    ? { commit: '3', tree: '4', object: '5' }
    : { commit: '6', tree: '7', object: '8' }
  return freezeDeliveryCandidate(delivery, {
    producerStageRunId,
    producerSessionBindingId,
    baseCommitId: delivery.spec.baseRevision,
    baseTreeId: '2'.repeat(40),
    candidateCommitId: digits.commit.repeat(40),
    candidateTreeId: digits.tree.repeat(40),
    diffSha256: createHash('sha256').update(diff).digest('hex'),
    changedPaths: [{
      path: 'src/restart.ts',
      state: 'present',
      objectId: digits.object.repeat(40),
    }],
  })
}

function verdictRuntimeEvents(delivery, candidate, verdict) {
  return ['reviewer', 'verifier'].flatMap((role) => {
    const run = delivery.stageRuns.findLast(entry => (
      entry.stage === 'verifying' && entry.role === role
    ))
    assert.notEqual(run, undefined)
    const binding = delivery.sessionBindings.find(entry => entry.stageRunId === run.id)
    assert.notEqual(binding, undefined)
    assert.notEqual(binding.dshSessionId, null)
    assert.notEqual(binding.codexSessionId, null)
    const evidenceType = role === 'reviewer' ? 'command' : 'test'
    return new CodexRuntimeProjector({
      sessionId: binding.dshSessionId,
      kernelSessionId: binding.codexSessionId,
      roleId: role,
      kernelStreamId: `stream-${binding.id}`,
    }).replay([
      kernelEvent(1, 'session_configured', {
        session_id: binding.codexSessionId,
        thread_id: binding.codexSessionId,
        occurred_at_ms: binding.boundAtMillis,
        ...readOnlyConfiguration(),
      }),
      kernelEvent(2, 'task_started', {
        turn_id: `turn-${binding.id}`,
        started_at_ms: binding.boundAtMillis,
      }),
      kernelEvent(3, 'item_completed', {
        turn_id: `turn-${binding.id}`,
        completed_at_ms: binding.boundAtMillis,
        item: {
          type: 'CommandExecution',
          id: `check-${binding.id}`,
          command: role === 'reviewer' ? ['git', 'diff', '--check'] : ['pnpm', 'test'],
          status: verdict === 'fail' ? 'failed' : 'completed',
          exit_code: verdict === 'fail' ? 1 : 0,
        },
      }),
      kernelEvent(4, 'agent_message', {
        turn_id: `turn-${binding.id}`,
        occurred_at_ms: binding.boundAtMillis,
        phase: 'final_answer',
        message: JSON.stringify({
          protocol: 'winwincode.independent-verification-result.v1',
          delivery_spec_id: delivery.spec.id,
          delivery_spec_revision: delivery.spec.revision,
          candidate_ref: candidate.candidateRef,
          findings: [{
            finding_id: `finding-${binding.id}`,
            criterion_id: delivery.spec.acceptanceCriteria[0].id,
            verdict,
            explanation: `${role} checked the exact restart candidate.`,
            evidence_sources: [{
              type: evidenceType,
              event_id: `${binding.dshSessionId}@3`,
            }],
          }],
        }),
      }),
      kernelEvent(5, 'task_complete', {
        turn_id: `turn-${binding.id}`,
        completed_at_ms: binding.boundAtMillis,
        last_agent_message: `${role} finished`,
        error: null,
      }),
    ])
  })
}

function verdictSubmission(
  delivery,
  producerStageRunId,
  producerSessionBindingId,
  generation,
  verdict,
) {
  const candidate = candidateFor(
    delivery,
    producerStageRunId,
    producerSessionBindingId,
    generation,
  )
  return {
    candidate,
    runtimeEvents: verdictRuntimeEvents(delivery, candidate, verdict),
    requiredRoles: ['reviewer', 'verifier'],
  }
}

function createMutationRunner(home, restartEveryMutation) {
  return async function mutate({ deliveryId, requestId, before, invoke }) {
    if (restartEveryMutation && before !== null) {
      const recoveredBefore = await service(home).getDeliveryProjection(deliveryId)
      assert.deepEqual(recoveredBefore.delivery, before)
    }
    const first = await invoke(service(home))
    if (!restartEveryMutation) return first

    const recoveredAfter = await service(home).getDeliveryProjection(deliveryId)
    assert.deepEqual(recoveredAfter.delivery, first)
    const replay = await invoke(service(home))
    assert.deepEqual(replay, first)
    const stored = await DeliveryStore.open(home, deliveryId).then(store => store.read())
    assert.equal(stored.records.filter(record => record.requestId === requestId).length, 1)
    assert.deepEqual(stored.snapshot, first)
    return replay
  }
}

async function runDefinitionScenario(home, restartEveryMutation) {
  const deliveryId = 'delivery-restart-definition'
  const mutate = createMutationRunner(home, restartEveryMutation)
  const firstSpec = spec(deliveryId, 1, 'definition-v1')
  let current = await mutate({
    deliveryId,
    requestId: 'restart-create-delivery',
    before: null,
    invoke: currentService => currentService.createDelivery({
      requestId: 'restart-create-delivery',
      spec: firstSpec,
      tasks: [deliveryTask(deliveryId, firstSpec.acceptanceCriteria[0].id)],
    }),
  })
  const secondSpec = spec(deliveryId, 2, 'definition-v2')
  current = await mutate({
    deliveryId,
    requestId: 'restart-update-spec',
    before: current,
    invoke: currentService => currentService.updateDeliverySpec({
      requestId: 'restart-update-spec',
      deliveryId,
      expectedRevision: 1,
      spec: secondSpec,
    }),
  })
  return current
}

function seededVerification(deliveryId) {
  const currentSpec = spec(deliveryId, 1, 'rework')
  const task = deliveryTask(deliveryId, currentSpec.acceptanceCriteria[0].id, 'verifying')
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'verifying',
    spec: currentSpec,
    tasks: [task],
    stageRuns: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-restart-plan-review',
        deliveryId,
        deliveryTaskId: null,
        stage: 'plan-review',
        actorType: 'human',
        role: 'reviewer',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: baseTime + 10,
        finishedAtMillis: baseTime + 20,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-restart-original-executor',
        deliveryId,
        deliveryTaskId: task.id,
        stage: 'executing',
        actorType: 'codex',
        role: 'executor',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: baseTime + 30,
        finishedAtMillis: baseTime + 40,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-restart-original-reviewer',
        deliveryId,
        deliveryTaskId: task.id,
        stage: 'verifying',
        actorType: 'codex',
        role: 'reviewer',
        status: 'succeeded',
        attempt: 1,
        startedAtMillis: baseTime + 50,
        finishedAtMillis: baseTime + 60,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-restart-original-verifier',
        deliveryId,
        deliveryTaskId: task.id,
        stage: 'verifying',
        actorType: 'codex',
        role: 'verifier',
        status: 'running',
        attempt: 1,
        startedAtMillis: baseTime + 70,
        finishedAtMillis: null,
      },
    ],
    sessionBindings: [
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-restart-plan-review',
        deliveryId,
        stageRunId: 'stage-restart-plan-review',
        dshSessionId: 'dsh-restart-plan-review',
        codexSessionId: null,
        boundAtMillis: baseTime + 15,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-restart-original-executor',
        deliveryId,
        stageRunId: 'stage-restart-original-executor',
        dshSessionId: 'dsh-restart-original-executor',
        codexSessionId: 'codex-restart-original-executor',
        boundAtMillis: baseTime + 35,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-restart-original-reviewer',
        deliveryId,
        stageRunId: 'stage-restart-original-reviewer',
        dshSessionId: 'dsh-restart-original-reviewer',
        codexSessionId: 'codex-restart-original-reviewer',
        boundAtMillis: baseTime + 55,
      },
      {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-restart-original-verifier',
        deliveryId,
        stageRunId: 'stage-restart-original-verifier',
        dshSessionId: 'dsh-restart-original-verifier',
        codexSessionId: 'codex-restart-original-verifier',
        boundAtMillis: baseTime + 75,
      },
    ],
    attentionItems: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'attention-restart-plan-review',
      deliveryId,
      deliverySpecId: currentSpec.id,
      stageRunId: 'stage-restart-plan-review',
      type: 'decision_required',
      title: 'Approve the restart verification definition',
      context: 'Approve the exact current DeliverySpec before candidate execution.',
      options: [],
      assignedTo: 'restart-reviewer',
      blocking: true,
      status: 'resolved',
      resolution: 'Approved.',
      resolvedBy: 'restart-reviewer',
      createdAtMillis: baseTime + 15,
      resolvedAtMillis: baseTime + 18,
    }],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 75,
  })
}

async function runReworkScenario(home, restartEveryMutation) {
  const deliveryId = 'delivery-restart-rework'
  let current = seededVerification(deliveryId)
  await DeliveryStore.create({
    home,
    requestId: 'restart-seed-verification',
    requestDigest: 'a'.repeat(64),
    snapshot: current,
  })
  const mutate = createMutationRunner(home, restartEveryMutation)
  const originalSubmission = verdictSubmission(
    current,
    'stage-restart-original-executor',
    'binding-restart-original-executor',
    'original',
    'fail',
  )
  current = await mutate({
    deliveryId,
    requestId: 'restart-submit-failure',
    before: current,
    invoke: currentService => currentService.submitVerdict({
      requestId: 'restart-submit-failure',
      deliveryId,
      expectedRevision: 1,
      ...originalSubmission,
    }),
  })
  const failureAttention = current.attentionItems.find(item => (
    item.status === 'open' && item.options.some(option => option.id === 'start-rework')
  ))
  assert.notEqual(failureAttention, undefined)
  current = await mutate({
    deliveryId,
    requestId: 'restart-resolve-failure',
    before: current,
    invoke: currentService => currentService.resolveAttention({
      requestId: 'restart-resolve-failure',
      deliveryId,
      expectedRevision: 2,
      attentionItemId: failureAttention.id,
      status: 'resolved',
      resolution: 'Start the one approved bounded rework attempt.',
      remediation: null,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof: 'restart-proof-value' },
    }),
  })
  current = await mutate({
    deliveryId,
    requestId: 'restart-start-rework',
    before: current,
    invoke: currentService => currentService.startStage({
      requestId: 'restart-start-rework',
      deliveryId,
      expectedRevision: 3,
      stageRunId: 'stage-restart-rework',
      deliveryTaskId: `task-${deliveryId}`,
      stage: 'reworking',
      actorType: 'codex',
      role: 'remediator',
      attention: null,
    }),
  })
  current = await mutate({
    deliveryId,
    requestId: 'restart-bind-rework',
    before: current,
    invoke: currentService => currentService.bindSession({
      requestId: 'restart-bind-rework',
      deliveryId,
      expectedRevision: 4,
      bindingId: 'binding-restart-rework',
      stageRunId: 'stage-restart-rework',
      dshSessionId: 'dsh-restart-rework',
      codexSessionId: 'codex-restart-rework',
    }),
  })
  current = await mutate({
    deliveryId,
    requestId: 'restart-start-reviewer',
    before: current,
    invoke: currentService => currentService.startStage({
      requestId: 'restart-start-reviewer',
      deliveryId,
      expectedRevision: 5,
      stageRunId: 'stage-restart-reviewer-2',
      deliveryTaskId: `task-${deliveryId}`,
      stage: 'verifying',
      actorType: 'codex',
      role: 'reviewer',
      attention: null,
    }),
  })
  current = await mutate({
    deliveryId,
    requestId: 'restart-bind-reviewer',
    before: current,
    invoke: currentService => currentService.bindSession({
      requestId: 'restart-bind-reviewer',
      deliveryId,
      expectedRevision: 6,
      bindingId: 'binding-restart-reviewer-2',
      stageRunId: 'stage-restart-reviewer-2',
      dshSessionId: 'dsh-restart-reviewer-2',
      codexSessionId: 'codex-restart-reviewer-2',
    }),
  })
  current = await mutate({
    deliveryId,
    requestId: 'restart-start-verifier',
    before: current,
    invoke: currentService => currentService.startStage({
      requestId: 'restart-start-verifier',
      deliveryId,
      expectedRevision: 7,
      stageRunId: 'stage-restart-verifier-2',
      deliveryTaskId: `task-${deliveryId}`,
      stage: 'verifying',
      actorType: 'codex',
      role: 'verifier',
      attention: null,
    }),
  })
  current = await mutate({
    deliveryId,
    requestId: 'restart-bind-verifier',
    before: current,
    invoke: currentService => currentService.bindSession({
      requestId: 'restart-bind-verifier',
      deliveryId,
      expectedRevision: 8,
      bindingId: 'binding-restart-verifier-2',
      stageRunId: 'stage-restart-verifier-2',
      dshSessionId: 'dsh-restart-verifier-2',
      codexSessionId: 'codex-restart-verifier-2',
    }),
  })
  const freshSubmission = verdictSubmission(
    current,
    'stage-restart-rework',
    'binding-restart-rework',
    'reworked',
    'pass',
  )
  current = await mutate({
    deliveryId,
    requestId: 'restart-submit-pass',
    before: current,
    invoke: currentService => currentService.submitVerdict({
      requestId: 'restart-submit-pass',
      deliveryId,
      expectedRevision: 9,
      ...freshSubmission,
    }),
  })
  return current
}

function assertUniqueIdentities(delivery) {
  for (const values of [
    delivery.stageRuns,
    delivery.sessionBindings,
    delivery.attentionItems,
    delivery.evidence,
  ]) {
    assert.equal(new Set(values.map(value => value.id)).size, values.length)
  }
}

test('every Delivery mutation converges across pre/post restart and exact request replay', async t => {
  const controlDefinitionHome = await mkdtemp(join(tmpdir(), 'winwincode-restart-control-def-'))
  const restartDefinitionHome = await mkdtemp(join(tmpdir(), 'winwincode-restart-case-def-'))
  const controlReworkHome = await mkdtemp(join(tmpdir(), 'winwincode-restart-control-flow-'))
  const restartReworkHome = await mkdtemp(join(tmpdir(), 'winwincode-restart-case-flow-'))
  t.after(() => Promise.all([
    rm(controlDefinitionHome, { recursive: true, force: true }),
    rm(restartDefinitionHome, { recursive: true, force: true }),
    rm(controlReworkHome, { recursive: true, force: true }),
    rm(restartReworkHome, { recursive: true, force: true }),
  ]))

  const controlDefinition = await runDefinitionScenario(controlDefinitionHome, false)
  const restartedDefinition = await runDefinitionScenario(restartDefinitionHome, true)
  assert.deepEqual(restartedDefinition, controlDefinition)
  assert.deepEqual(restartedDefinition.attentionItems, [])
  assert.deepEqual(restartedDefinition.evidence, [])
  assert.equal(restartedDefinition.verdict, null)

  const control = await runReworkScenario(controlReworkHome, false)
  const restarted = await runReworkScenario(restartReworkHome, true)
  assert.deepEqual(restarted, control)
  assert.equal(restarted.status, 'ready-to-deliver')
  assert.equal(restarted.verdict.status, 'pass')
  assert.equal(restarted.tasks[0].status, 'completed')
  assert.equal(restarted.stageRuns.filter(run => run.stage === 'reworking').length, 1)
  assert.equal(restarted.stageRuns.find(run => run.stage === 'reworking').attempt, 1)
  assertUniqueIdentities(restarted)

  const stored = await DeliveryStore.open(restartReworkHome, restarted.id).then(store => store.read())
  assert.equal(stored.records.length, restarted.revision)
  assert.equal(new Set(stored.records.map(record => record.requestId)).size, stored.records.length)
  const reworkCounts = stored.records.map(record => (
    record.snapshot.stageRuns.filter(run => run.stage === 'reworking').length
  ))
  assert.deepEqual(reworkCounts, [...reworkCounts].sort((left, right) => left - right))
  assert.equal(Math.max(...reworkCounts), restarted.spec.maxReworkAttempts)
  const firstVerdictRecord = stored.records.find(record => (
    record.requestId === 'restart-submit-failure'
  ))
  assert.notEqual(firstVerdictRecord, undefined)
  assert.equal(firstVerdictRecord.snapshot.verdict.status, 'fail')
  assert.ok(firstVerdictRecord.snapshot.evidence.length > 0)
  assert.notEqual(firstVerdictRecord.snapshot.verdict.candidateRef, restarted.verdict.candidateRef)
})
