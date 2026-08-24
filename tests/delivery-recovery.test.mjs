import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseDelivery,
} from '../packages/contracts/dist/index.js'
import {
  CodexRuntimeProjector,
  DeliveryRecoveryError,
  RuntimeSessionLedger,
  reconcileDeliveryAfterRestart,
} from '../packages/dsh-profile/dist/index.js'
import {
  DeliveryStore,
  freezeDeliveryCandidate,
} from '../packages/strongflow/dist/index.js'

const now = 2_500_000_000_000
const baseRevision = 'a'.repeat(40)

function kernelEvent(sequence, type, data = {}, submissionId = 'recovery-submission') {
  const payload = { id: submissionId, msg: { type, ...data } }
  return Object.freeze({
    sequence: BigInt(sequence),
    kind: type,
    payload,
    rawJson: JSON.stringify(payload),
  })
}

function stageRun({
  id,
  deliveryId,
  taskId = 'task-recovery',
  stage = 'executing',
  actorType = 'codex',
  role = 'executor',
  status = 'running',
  startedAtMillis = now + 10,
  finishedAtMillis = null,
}) {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id,
    deliveryId,
    deliveryTaskId: taskId,
    stage,
    actorType,
    role,
    status,
    attempt: 1,
    startedAtMillis,
    finishedAtMillis,
  }
}

function sessionBinding({
  id,
  deliveryId,
  stageRunId,
  dshSessionId,
  codexSessionId,
  boundAtMillis = now + 11,
}) {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id,
    deliveryId,
    stageRunId,
    dshSessionId,
    codexSessionId,
    boundAtMillis,
  }
}

function deliveryFixture({
  id = 'delivery-recovery',
  status = 'executing',
  taskStatus = 'active',
  stageRuns,
  sessionBindings,
  attentionItems = [],
}) {
  const criterionId = `criterion-${id}`
  const taskId = `task-${id}`
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id,
    revision: 1,
    status,
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `spec-${id}`,
      deliveryId: id,
      revision: 1,
      title: 'Recover one delivery',
      goal: 'Rebuild DSH and StrongFlow projections without replaying Codex execution.',
      scope: ['Delivery owner records and bound runtime ledgers'],
      outOfScope: ['A second execution runtime'],
      constraints: ['Select exactly one delivery-level next action'],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: criterionId,
        description: 'Restart recovery preserves owner state and blockers.',
        verificationMethod: 'Reopen owner records and compare projections.',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/recovery',
      },
      baseRevision,
      maxReworkAttempts: 2,
      createdAtMillis: now,
    },
    tasks: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: taskId,
      deliveryId: id,
      title: 'Recovery task',
      goal: 'Keep the current delivery stage authoritative.',
      acceptanceCriterionIds: [criterionId],
      blockedByTaskIds: [],
      owner: null,
      status: taskStatus,
    }],
    stageRuns: stageRuns({ deliveryId: id, taskId }),
    sessionBindings: sessionBindings({ deliveryId: id, taskId }),
    attentionItems: attentionItems.map(item => ({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      deliveryId: id,
      deliverySpecId: `spec-${id}`,
      options: [],
      assignedTo: 'reviewer@example.test',
      blocking: true,
      status: 'open',
      resolution: null,
      resolvedBy: null,
      createdAtMillis: now + 12,
      resolvedAtMillis: null,
      ...item,
    })),
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 40,
  })
}

async function createDeliveryStore(home, delivery) {
  await DeliveryStore.create({
    home,
    requestId: `create-${delivery.id}`,
    requestDigest: '0'.repeat(64),
    snapshot: delivery,
  })
}

async function createRuntimeLedger(home, {
  dshSessionId,
  codexSessionId,
  role,
  events,
}) {
  const streamId = `stream-${dshSessionId}`
  const ledger = await RuntimeSessionLedger.create({
    home,
    dshSessionId,
    roleId: role,
    cwd: '/workspace/recovery',
    kernelSessionId: codexSessionId,
    kernelStreamId: streamId,
    rolloutPath: `/runtime/${dshSessionId}.jsonl`,
    provider: 'deepseek-compatible',
    model: 'fixture-coder',
  })
  const projector = new CodexRuntimeProjector({
    sessionId: dshSessionId,
    kernelSessionId: codexSessionId,
    roleId: role,
    kernelStreamId: streamId,
  })
  for (const sourceEvent of events) {
    const event = projector.ingest(sourceEvent)
    assert.ok(event)
    await ledger.appendEvent(event)
  }
}

function activeExecutionFixture(id = 'delivery-recovery-active') {
  const runId = `stage-${id}-executor`
  const bindingId = `binding-${id}-executor`
  const dshSessionId = `dsh-${id}-executor`
  const codexSessionId = `codex-${id}-executor`
  const delivery = deliveryFixture({
    id,
    stageRuns: ({ deliveryId, taskId }) => [stageRun({
      id: runId,
      deliveryId,
      taskId,
    })],
    sessionBindings: ({ deliveryId }) => [sessionBinding({
      id: bindingId,
      deliveryId,
      stageRunId: runId,
      dshSessionId,
      codexSessionId,
    })],
  })
  return { delivery, runId, bindingId, dshSessionId, codexSessionId }
}

test('restart recovery rebuilds the same DSH and StrongFlow views and continues a live stage', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-live-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const fixture = activeExecutionFixture()
  await createDeliveryStore(home, fixture.delivery)
  await createRuntimeLedger(home, {
    ...fixture,
    role: 'executor',
    events: [
      kernelEvent(1, 'task_started', { turn_id: 'turn-recovery-live' }),
      kernelEvent(2, 'plan_update', {
        explanation: 'Rebuild only projections.',
        plan: [{ step: 'Keep owner state', status: 'in_progress' }],
      }),
      kernelEvent(3, 'turn_diff', {
        unified_diff: [
          '--- a/packages/example.ts',
          '+++ b/packages/example.ts',
          '@@ -1 +1 @@',
          '-before',
          '+after',
          '',
        ].join('\n'),
      }),
    ],
  })
  const codex = {
    calls: 0,
    async listSessions() {
      this.calls += 1
      return [fixture.codexSessionId, 'unrelated-codex-session']
    },
  }

  const first = await reconcileDeliveryAfterRestart({
    home,
    deliveryId: fixture.delivery.id,
    codex,
  })
  const restarted = await reconcileDeliveryAfterRestart({
    home,
    deliveryId: fixture.delivery.id,
    codex,
  })

  assert.equal(codex.calls, 2)
  assert.deepEqual(restarted, first)
  assert.deepEqual(first.liveBoundCodexSessionIds, [fixture.codexSessionId])
  assert.equal(first.deliveryRecordSequence, '1')
  assert.equal(first.sessions[0].dsh.status, 'running')
  assert.equal(first.sessions[0].dsh.asOfSequence, '3')
  assert.equal(first.strongFlow.stages[0].sessions[0].plan.items[0].step, 'Keep owner state')
  assert.deepEqual(first.strongFlow.stages[0].changedFiles, ['packages/example.ts'])
  assert.deepEqual(first.nextAction, {
    kind: 'continue-stage',
    stageRunId: fixture.runId,
    sessionBindingId: fixture.bindingId,
    dshSessionId: fixture.dshSessionId,
    codexSessionId: fixture.codexSessionId,
  })
  assert.equal(Object.isFrozen(first), true)
})

test('restart recovery recreates only the missing session boundary for an active StageRun', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-unbound-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const deliveryId = 'delivery-recovery-unbound'
  const runId = 'stage-delivery-recovery-unbound'
  const delivery = deliveryFixture({
    id: deliveryId,
    stageRuns: ({ deliveryId: ownerId, taskId }) => [stageRun({
      id: runId,
      deliveryId: ownerId,
      taskId,
    })],
    sessionBindings: () => [],
  })
  await createDeliveryStore(home, delivery)
  const recovered = await reconcileDeliveryAfterRestart({
    home,
    deliveryId,
    codex: { async listSessions() { return [] } },
  })
  assert.deepEqual(recovered.nextAction, {
    kind: 'create-stage-session',
    stageRunId: runId,
    stage: 'executing',
    actorType: 'codex',
    role: 'executor',
  })
  assert.deepEqual(recovered.sessions, [])
})

test('restart recovery resumes an absent Codex thread but does not accept final model output', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-resume-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const fixture = activeExecutionFixture('delivery-recovery-resume')
  await createDeliveryStore(home, fixture.delivery)
  await createRuntimeLedger(home, {
    ...fixture,
    role: 'executor',
    events: [kernelEvent(1, 'task_started', { turn_id: 'turn-recovery-resume' })],
  })

  const missing = await reconcileDeliveryAfterRestart({
    home,
    deliveryId: fixture.delivery.id,
    codex: { async listSessions() { return [] } },
  })
  assert.equal(missing.nextAction.kind, 'resume-stage-session')
  assert.equal(missing.nextAction.rolloutPath, `/runtime/${fixture.dshSessionId}.jsonl`)
  assert.deepEqual(missing.nextAction.pendingInteractionIds, [])

  await rm(home, { recursive: true, force: true })
  const terminalHome = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-terminal-'))
  t.after(() => rm(terminalHome, { recursive: true, force: true }))
  await createDeliveryStore(terminalHome, fixture.delivery)
  await createRuntimeLedger(terminalHome, {
    ...fixture,
    role: 'executor',
    events: [
      kernelEvent(1, 'task_started', { turn_id: 'turn-recovery-terminal' }),
      kernelEvent(2, 'agent_message', { message: 'Implementation is complete.' }),
      kernelEvent(3, 'task_complete', {
        turn_id: 'turn-recovery-terminal',
        last_agent_message: 'Implementation is complete.',
        error: null,
      }),
    ],
  })
  const terminal = await reconcileDeliveryAfterRestart({
    home: terminalHome,
    deliveryId: fixture.delivery.id,
    codex: { async listSessions() { return [] } },
  })
  assert.equal(terminal.delivery.status, 'executing')
  assert.equal(terminal.delivery.stageRuns[0].status, 'running')
  assert.equal(terminal.delivery.tasks[0].status, 'active')
  assert.deepEqual(terminal.nextAction, {
    kind: 'review-stage-output',
    stageRunId: fixture.runId,
    sessionBindingId: fixture.bindingId,
    dshSessionId: fixture.dshSessionId,
    codexSessionId: fixture.codexSessionId,
    runtimeStatus: 'completed',
  })
})

test('unresolved Codex approval and business Attention remain the one blocking action', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-blockers-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const fixture = activeExecutionFixture('delivery-recovery-approval')
  await createDeliveryStore(home, fixture.delivery)
  await createRuntimeLedger(home, {
    ...fixture,
    role: 'executor',
    events: [
      kernelEvent(1, 'task_started', { turn_id: 'turn-recovery-approval' }),
      kernelEvent(2, 'exec_approval_request', {
        call_id: 'call-recovery-test',
        approval_id: 'approval-recovery-test',
        turn_id: 'turn-recovery-approval',
        command: ['pnpm', 'test'],
        cwd: '/workspace/recovery',
        parsed_cmd: [],
      }),
    ],
  })
  const approval = await reconcileDeliveryAfterRestart({
    home,
    deliveryId: fixture.delivery.id,
    codex: { async listSessions() { return [fixture.codexSessionId] } },
  })
  assert.deepEqual(approval.nextAction, {
    kind: 'resolve-runtime-interaction',
    stageRunId: fixture.runId,
    sessionBindingId: fixture.bindingId,
    dshSessionId: fixture.dshSessionId,
    codexSessionId: fixture.codexSessionId,
    interactionIds: ['approval-recovery-test'],
  })

  const attentionHome = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-attention-'))
  t.after(() => rm(attentionHome, { recursive: true, force: true }))
  const attentionId = 'attention-recovery-delivery-review'
  const reviewRunId = 'stage-recovery-delivery-review'
  const reviewDelivery = deliveryFixture({
    id: 'delivery-recovery-attention',
    status: 'needs-attention',
    taskStatus: 'completed',
    stageRuns: ({ deliveryId }) => [stageRun({
      id: reviewRunId,
      deliveryId,
      taskId: null,
      stage: 'delivery-review',
      actorType: 'human',
      role: 'approver',
      status: 'waiting',
    })],
    sessionBindings: ({ deliveryId }) => [sessionBinding({
      id: 'binding-recovery-delivery-review',
      deliveryId,
      stageRunId: reviewRunId,
      dshSessionId: 'dsh-recovery-delivery-review',
      codexSessionId: null,
    })],
    attentionItems: [{
      id: attentionId,
      stageRunId: reviewRunId,
      type: 'delivery_approval',
      title: 'Approve the reviewed delivery',
      context: 'The exact delivery candidate is ready for a human decision.',
    }],
  })
  await createDeliveryStore(attentionHome, reviewDelivery)
  const attention = await reconcileDeliveryAfterRestart({
    home: attentionHome,
    deliveryId: reviewDelivery.id,
    codex: { async listSessions() { return [] } },
  })
  assert.deepEqual(attention.nextAction, {
    kind: 'resolve-delivery-attention',
    attentionItemIds: [attentionId],
  })
})

test('recovery starts the next stored stage and never repeats a settled executor', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-next-stage-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const deliveryId = 'delivery-recovery-next-stage'
  const executorRunId = 'stage-recovery-settled-executor'
  const executorBindingId = 'binding-recovery-settled-executor'
  const dshSessionId = 'dsh-recovery-settled-executor'
  const codexSessionId = 'codex-recovery-settled-executor'
  const delivery = deliveryFixture({
    id: deliveryId,
    status: 'verifying',
    taskStatus: 'verifying',
    stageRuns: ({ deliveryId: ownerId, taskId }) => [stageRun({
      id: executorRunId,
      deliveryId: ownerId,
      taskId,
      status: 'succeeded',
      finishedAtMillis: now + 20,
    })],
    sessionBindings: ({ deliveryId: ownerId }) => [sessionBinding({
      id: executorBindingId,
      deliveryId: ownerId,
      stageRunId: executorRunId,
      dshSessionId,
      codexSessionId,
    })],
  })
  await createDeliveryStore(home, delivery)
  await createRuntimeLedger(home, {
    dshSessionId,
    codexSessionId,
    role: 'executor',
    events: [
      kernelEvent(1, 'task_started', { turn_id: 'turn-recovery-settled' }),
      kernelEvent(2, 'task_complete', {
        turn_id: 'turn-recovery-settled',
        last_agent_message: 'Candidate produced.',
        error: null,
      }),
    ],
  })
  const recovered = await reconcileDeliveryAfterRestart({
    home,
    deliveryId,
    codex: { async listSessions() { return [] } },
  })
  assert.deepEqual(recovered.nextAction, {
    kind: 'start-stage',
    stage: 'verifying',
    deliveryTaskId: `task-${deliveryId}`,
  })
  assert.equal(recovered.nextAction.stage === 'executing', false)
})

test('recovery validates a rebuilt candidate and rejects stale candidate identity', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-candidate-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const deliveryId = 'delivery-recovery-candidate'
  const executorRunId = 'stage-recovery-candidate-executor'
  const verifierRunId = 'stage-recovery-candidate-verifier'
  const executorBindingId = 'binding-recovery-candidate-executor'
  const verifierBindingId = 'binding-recovery-candidate-verifier'
  const delivery = deliveryFixture({
    id: deliveryId,
    status: 'verifying',
    taskStatus: 'verifying',
    stageRuns: ({ deliveryId: ownerId, taskId }) => [
      stageRun({
        id: executorRunId,
        deliveryId: ownerId,
        taskId,
        status: 'succeeded',
        finishedAtMillis: now + 20,
      }),
      stageRun({
        id: verifierRunId,
        deliveryId: ownerId,
        taskId,
        stage: 'verifying',
        role: 'verifier',
        startedAtMillis: now + 30,
      }),
    ],
    sessionBindings: ({ deliveryId: ownerId }) => [
      sessionBinding({
        id: executorBindingId,
        deliveryId: ownerId,
        stageRunId: executorRunId,
        dshSessionId: 'dsh-recovery-candidate-executor',
        codexSessionId: 'codex-recovery-candidate-executor',
      }),
      sessionBinding({
        id: verifierBindingId,
        deliveryId: ownerId,
        stageRunId: verifierRunId,
        dshSessionId: 'dsh-recovery-candidate-verifier',
        codexSessionId: 'codex-recovery-candidate-verifier',
        boundAtMillis: now + 31,
      }),
    ],
  })
  await createDeliveryStore(home, delivery)
  await createRuntimeLedger(home, {
    dshSessionId: 'dsh-recovery-candidate-executor',
    codexSessionId: 'codex-recovery-candidate-executor',
    role: 'executor',
    events: [
      kernelEvent(1, 'task_started', { turn_id: 'turn-recovery-candidate-executor' }),
      kernelEvent(2, 'task_complete', {
        turn_id: 'turn-recovery-candidate-executor',
        last_agent_message: 'Candidate produced.',
        error: null,
      }),
    ],
  })
  await createRuntimeLedger(home, {
    dshSessionId: 'dsh-recovery-candidate-verifier',
    codexSessionId: 'codex-recovery-candidate-verifier',
    role: 'verifier',
    events: [kernelEvent(1, 'task_started', { turn_id: 'turn-recovery-candidate-verifier' })],
  })
  const candidate = freezeDeliveryCandidate(delivery, {
    producerStageRunId: executorRunId,
    producerSessionBindingId: executorBindingId,
    baseCommitId: baseRevision,
    baseTreeId: 'b'.repeat(40),
    candidateCommitId: 'c'.repeat(40),
    candidateTreeId: 'd'.repeat(40),
    diffSha256: 'e'.repeat(64),
    changedPaths: [{ path: 'packages/example.ts', state: 'present', objectId: 'f'.repeat(40) }],
  })
  const recovered = await reconcileDeliveryAfterRestart({
    home,
    deliveryId,
    candidate,
    codex: { async listSessions() { return ['codex-recovery-candidate-verifier'] } },
  })
  assert.equal(recovered.candidateRef, candidate.candidateRef)
  assert.equal(recovered.nextAction.kind, 'continue-stage')

  await assert.rejects(
    reconcileDeliveryAfterRestart({
      home,
      deliveryId,
      candidate: { ...candidate, diffSha256: '0'.repeat(64) },
      codex: { async listSessions() { return [] } },
    }),
    error => error instanceof DeliveryRecoveryError && error.code === 'CANDIDATE_CONFLICT',
  )
})

test('conflicting SessionBindings fail visibly instead of selecting an action', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-conflict-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const deliveryId = 'delivery-recovery-conflict'
  const runId = 'stage-recovery-conflict'
  const delivery = deliveryFixture({
    id: deliveryId,
    stageRuns: ({ deliveryId: ownerId, taskId }) => [stageRun({
      id: runId,
      deliveryId: ownerId,
      taskId,
    })],
    sessionBindings: ({ deliveryId: ownerId }) => [
      sessionBinding({
        id: 'binding-recovery-conflict-a',
        deliveryId: ownerId,
        stageRunId: runId,
        dshSessionId: 'dsh-recovery-conflict-a',
        codexSessionId: 'codex-recovery-conflict-a',
      }),
      sessionBinding({
        id: 'binding-recovery-conflict-b',
        deliveryId: ownerId,
        stageRunId: runId,
        dshSessionId: 'dsh-recovery-conflict-b',
        codexSessionId: 'codex-recovery-conflict-b',
      }),
    ],
  })
  await createDeliveryStore(home, delivery)
  await assert.rejects(
    reconcileDeliveryAfterRestart({
      home,
      deliveryId,
      codex: { async listSessions() { return [] } },
    }),
    error => error instanceof DeliveryRecoveryError
      && error.code === 'SESSION_BINDING_CONFLICT'
      && error.message.includes(runId),
  )
})

test('restart recovery accepts one DSH review Session reused by settled human stages', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-delivery-recovery-human-reuse-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const deliveryId = 'delivery-recovery-human-reuse'
  const sharedSessionId = 'dsh-recovery-human-review'
  const delivery = deliveryFixture({
    id: deliveryId,
    status: 'ready',
    taskStatus: 'pending',
    stageRuns: ({ deliveryId: ownerId }) => [
      stageRun({
        id: 'stage-recovery-human-plan-review',
        deliveryId: ownerId,
        taskId: null,
        stage: 'plan-review',
        actorType: 'human',
        role: 'reviewer',
        status: 'succeeded',
        startedAtMillis: now + 10,
        finishedAtMillis: now + 20,
      }),
      stageRun({
        id: 'stage-recovery-human-delivery-review',
        deliveryId: ownerId,
        taskId: null,
        stage: 'delivery-review',
        actorType: 'human',
        role: 'approver',
        status: 'succeeded',
        startedAtMillis: now + 30,
        finishedAtMillis: now + 40,
      }),
    ],
    sessionBindings: ({ deliveryId: ownerId }) => [
      sessionBinding({
        id: 'binding-recovery-human-plan-review',
        deliveryId: ownerId,
        stageRunId: 'stage-recovery-human-plan-review',
        dshSessionId: sharedSessionId,
        codexSessionId: null,
      }),
      sessionBinding({
        id: 'binding-recovery-human-delivery-review',
        deliveryId: ownerId,
        stageRunId: 'stage-recovery-human-delivery-review',
        dshSessionId: sharedSessionId,
        codexSessionId: null,
        boundAtMillis: now + 31,
      }),
    ],
  })
  await createDeliveryStore(home, delivery)

  const recovered = await reconcileDeliveryAfterRestart({
    home,
    deliveryId,
    codex: { async listSessions() { return [] } },
  })

  assert.deepEqual(recovered.nextAction, {
    kind: 'start-stage',
    stage: 'planning',
    deliveryTaskId: null,
  })
  assert.deepEqual(recovered.sessions, [])
  assert.equal(recovered.strongFlow.stages.length, 2)
})
