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
  RuntimeSessionLedger,
} from '../packages/dsh-profile/dist/index.js'
import {
  DeliveryRuntimeProjection,
  DeliveryRuntimeProjectionError,
} from '../packages/strongflow/dist/index.js'

const now = 2_300_000_000_000
const deliveryId = 'delivery-runtime-projection'
const dshSessionId = 'dsh-runtime-projection'
const codexSessionId = 'codex-runtime-projection'
const stageRunId = 'stage-runtime-executing'
const bindingId = 'binding-runtime-executing'

function kernelEvent(sequence, type, data = {}, submissionId = 'submission-runtime') {
  const payload = { id: submissionId, msg: { type, ...data } }
  return Object.freeze({
    sequence: BigInt(sequence),
    kind: type,
    payload,
    rawJson: JSON.stringify(payload),
  })
}

function deliveryFixture({
  boundDshSessionId = dshSessionId,
  boundCodexSessionId = codexSessionId,
} = {}) {
  const criterionId = 'criterion-runtime-projection'
  const taskId = 'delivery-task-runtime-projection'
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 8,
    status: 'executing',
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-spec-runtime-projection',
      deliveryId,
      revision: 1,
      title: 'Project Codex execution facts',
      goal: 'Rebuild the same StrongFlow execution view from the runtime ledger.',
      scope: ['Plan, agents, diff, evidence, failures, recovery, approvals, and usage'],
      outOfScope: ['A second execution scheduler'],
      constraints: ['RuntimeSessionLedger remains the raw source'],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: criterionId,
        description: 'Live and restarted projections are identical.',
        verificationMethod: 'Replay the durable RuntimeSessionLedger.',
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
      createdAtMillis: now,
    },
    tasks: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: taskId,
      deliveryId,
      title: 'Runtime semantics',
      goal: 'Expose execution facts without copying execution authority.',
      acceptanceCriterionIds: [criterionId],
      blockedByTaskIds: [],
      owner: null,
      status: 'active',
    }],
    stageRuns: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: stageRunId,
      deliveryId,
      deliveryTaskId: taskId,
      stage: 'executing',
      actorType: 'codex',
      role: 'implementation',
      status: 'running',
      attempt: 1,
      startedAtMillis: now + 1,
      finishedAtMillis: null,
    }],
    sessionBindings: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: bindingId,
      deliveryId,
      stageRunId,
      dshSessionId: boundDshSessionId,
      codexSessionId: boundCodexSessionId,
      boundAtMillis: now + 2,
    }],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 2,
  })
}

function runtimeSourceEvents() {
  return [
    kernelEvent(1, 'session_configured', {
      session_id: codexSessionId,
      thread_id: codexSessionId,
      model_provider_id: 'deepseek-compatible',
      model: 'fixture-coder',
    }, 'runtime-startup'),
    kernelEvent(2, 'task_started', {
      turn_id: 'turn-runtime-1',
      started_at: 100,
      model_context_window: 64_000,
    }),
    kernelEvent(3, 'plan_update', {
      explanation: 'Use the Codex-owned plan as a read-only projection.',
      plan: [
        { step: 'Inspect runtime facts', status: 'completed' },
        { step: 'Build the projection', status: 'in_progress' },
        { step: 'Replay from the ledger', status: 'pending' },
      ],
    }),
    kernelEvent(4, 'plan_delta', {
      thread_id: codexSessionId,
      turn_id: 'turn-runtime-1',
      item_id: 'plan-runtime-1',
      delta: '- preserve event identity\n',
    }),
    kernelEvent(5, 'item_completed', {
      thread_id: codexSessionId,
      turn_id: 'turn-runtime-1',
      completed_at_ms: 100_050,
      item: {
        type: 'Plan',
        id: 'plan-runtime-1',
        text: '- preserve event identity\n- rebuild after restart\n',
      },
    }),
    kernelEvent(6, 'collab_agent_spawn_end', {
      call_id: 'spawn-reviewer',
      completed_at_ms: 100_100,
      sender_thread_id: codexSessionId,
      new_thread_id: 'codex-subagent-reviewer',
      new_agent_nickname: 'reviewer',
      new_agent_role: 'review',
      prompt: 'Review the projection.',
      model: 'fixture-coder',
      reasoning_effort: 'medium',
      status: 'running',
    }),
    kernelEvent(7, 'sub_agent_activity', {
      event_id: 'agent-activity-reviewer',
      occurred_at_ms: 100_110,
      agent_thread_id: 'codex-subagent-reviewer',
      agent_path: '/root/reviewer',
      kind: 'started',
    }),
    kernelEvent(8, 'request_user_input', {
      call_id: 'input-runtime-scope',
      turn_id: 'turn-runtime-1',
      isBlocking: true,
      questions: [{
        id: 'runtime-scope',
        header: 'Projection scope',
        question: 'Keep this view read-only?',
        isOther: false,
        isSecret: false,
        options: [{
          label: 'Keep read-only',
          description: 'Do not add execution controls to the projection.',
        }],
      }],
    }),
    kernelEvent(9, 'exec_approval_request', {
      call_id: 'exec-runtime-tests',
      approval_id: 'approval-runtime-tests',
      turn_id: 'turn-runtime-1',
      command: ['pnpm', 'test'],
      cwd: '/workspace/repository',
      parsed_cmd: [],
    }),
    kernelEvent(10, 'item_started', {
      thread_id: codexSessionId,
      turn_id: 'turn-runtime-1',
      started_at_ms: 100_200,
      item: {
        type: 'CommandExecution',
        id: 'exec-runtime-tests',
        command: ['pnpm', 'test'],
        cwd: 'file:///workspace/repository',
        parsed_cmd: [],
        source: 'agent',
        status: 'in_progress',
      },
    }),
    kernelEvent(11, 'item_completed', {
      thread_id: codexSessionId,
      turn_id: 'turn-runtime-1',
      started_at_ms: 100_200,
      completed_at_ms: 100_300,
      item: {
        type: 'CommandExecution',
        id: 'exec-runtime-tests',
        command: ['pnpm', 'test'],
        cwd: 'file:///workspace/repository',
        parsed_cmd: [],
        source: 'agent',
        status: 'completed',
        stdout: 'all tests passed',
        stderr: '',
        aggregated_output: 'all tests passed',
        exit_code: 0,
        formatted_output: 'all tests passed',
      },
    }),
    kernelEvent(12, 'turn_diff', {
      unified_diff: [
        '--- a/packages/strongflow/src/view.ts',
        '+++ b/packages/strongflow/src/view.ts',
        '@@ -1 +1,2 @@',
        '-old',
        '+new',
        '+projection',
        '',
      ].join('\n'),
    }),
    kernelEvent(13, 'token_count', {
      info: {
        total_token_usage: { input_tokens: 20, output_tokens: 8, total_tokens: 28 },
        last_token_usage: { input_tokens: 20, output_tokens: 8, total_tokens: 28 },
      },
      rate_limits: null,
    }),
    kernelEvent(14, 'error', {
      message: 'The first runtime turn failed.',
      codex_error_info: 'response_stream_disconnected',
    }),
    kernelEvent(15, 'task_complete', {
      turn_id: 'turn-runtime-1',
      last_agent_message: 'Retry required.',
      error: { message: 'The first runtime turn failed.' },
      completed_at: 101,
    }),
    kernelEvent(16, 'task_started', {
      turn_id: 'turn-runtime-2',
      started_at: 102,
      model_context_window: 64_000,
    }, 'submission-runtime-recovery'),
    kernelEvent(17, 'task_complete', {
      turn_id: 'turn-runtime-2',
      last_agent_message: 'Projection recovered.',
      error: null,
      completed_at: 103,
    }, 'submission-runtime-recovery'),
    kernelEvent(18, 'collab_waiting_end', {
      sender_thread_id: codexSessionId,
      call_id: 'wait-runtime-reviewer',
      completed_at_ms: 103_100,
      agent_statuses: [{
        thread_id: 'codex-subagent-reviewer',
        agent_nickname: 'reviewer',
        agent_role: 'review',
        status: { completed: 'The runtime projection is consistent.' },
      }],
      statuses: {
        'codex-subagent-reviewer': { completed: 'The runtime projection is consistent.' },
      },
    }, 'submission-runtime-recovery'),
  ]
}

test('live and reopened ledgers rebuild the same read-only Delivery runtime view', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-delivery-runtime-projection-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const delivery = deliveryFixture()
  const ledger = await RuntimeSessionLedger.create({
    home,
    dshSessionId,
    roleId: 'implementation',
    cwd: '/workspace/repository',
    kernelSessionId: codexSessionId,
    kernelStreamId: 'kernel-stream-runtime-projection',
    rolloutPath: '/workspace/rollout.jsonl',
    provider: 'deepseek-compatible',
    model: 'fixture-coder',
  })
  const normalizer = new CodexRuntimeProjector({
    sessionId: dshSessionId,
    kernelSessionId: codexSessionId,
    roleId: 'implementation',
    kernelStreamId: 'kernel-stream-runtime-projection',
  })
  const live = new DeliveryRuntimeProjection({ delivery })
  for (const sourceEvent of runtimeSourceEvents()) {
    const event = normalizer.ingest(sourceEvent)
    assert.ok(event)
    live.apply(event)
    await ledger.appendEvent(event)
  }

  const liveSnapshot = live.snapshot
  const reopened = await RuntimeSessionLedger.open(home, dshSessionId)
  const restarted = new DeliveryRuntimeProjection({ delivery })
  const restartedSnapshot = restarted.replay((await reopened.read()).events)
  assert.deepEqual(restartedSnapshot, liveSnapshot)

  const stage = liveSnapshot.stages[0]
  const session = stage.sessions[0]
  assert.equal(session.binding.id, bindingId)
  assert.equal(session.asOfSequence, '18')
  assert.deepEqual(session.plan.items.map(item => item.status), [
    'completed',
    'in_progress',
    'pending',
  ])
  assert.equal(session.plan.text, '- preserve event identity\n- rebuild after restart\n')
  assert.equal(session.agents.some(agent => (
    agent.threadId === 'codex-subagent-reviewer'
      && agent.path === '/root/reviewer'
      && agent.role === 'review'
      && agent.status === 'completed'
  )), true)
  assert.deepEqual(session.agentEdges, [{
    parentThreadId: codexSessionId,
    childThreadId: 'codex-subagent-reviewer',
  }])
  assert.equal(session.activities[0].activityType, 'test')
  assert.equal(session.activities[0].status, 'completed')
  assert.equal(session.interactions.some(interaction => (
    interaction.interactionType === 'user-input'
      && interaction.requestedEvent.kind === 'input.requested'
  )), true)
  assert.equal(session.interactions.some(interaction => (
    interaction.interactionType === 'execution-approval'
      && interaction.operationId === 'exec-runtime-tests'
      && interaction.status === 'resolved'
  )), true)
  assert.equal(session.attentionCandidates[0].type, 'decision_required')
  assert.equal(session.attentionCandidates[0].status, 'resolved')
  assert.deepEqual(session.diff.changedFiles, ['packages/strongflow/src/view.ts'])
  assert.equal(session.diff.additions, 2)
  assert.equal(session.diff.deletions, 1)
  assert.deepEqual(session.usage.totals, {
    input_tokens: 20,
    output_tokens: 8,
    total_tokens: 28,
  })
  assert.equal(session.failures.length, 2)
  assert.equal(session.failures[0].code, 'response_stream_disconnected')
  assert.equal(session.recovery.state, 'recovered')
  assert.equal(session.recovery.failureCount, 2)
  assert.equal(session.recovery.recoveryCount, 1)
  assert.equal(stage.changedFiles[0], 'packages/strongflow/src/view.ts')
  assert.equal(liveSnapshot.tasks[0].stageRunIds[0], stageRunId)
  assert.equal(stage.evidenceLinks.some(link => link.type === 'test'), true)
  assert.equal(stage.evidenceLinks.some(link => link.type === 'diff'), true)

  assert.deepEqual(live.apply((await reopened.read()).events.at(-1)), { changed: false })
})

test('Delivery runtime projection rejects events outside its SessionBindings', () => {
  const normalizer = new CodexRuntimeProjector({
    sessionId: 'foreign-dsh-session',
    kernelSessionId: 'foreign-codex-session',
    roleId: 'implementation',
    kernelStreamId: 'foreign-stream',
  })
  const event = normalizer.ingest(kernelEvent(1, 'task_started', { turn_id: 'foreign-turn' }))
  assert.ok(event)
  const projection = new DeliveryRuntimeProjection({ delivery: deliveryFixture() })
  assert.throws(
    () => projection.apply(event),
    error => error instanceof DeliveryRuntimeProjectionError
      && error.code === 'RUNTIME_SESSION_UNBOUND',
  )
})

test('Delivery runtime projection requires the complete DSH and Codex session binding', () => {
  const normalizer = new CodexRuntimeProjector({
    sessionId: dshSessionId,
    kernelSessionId: 'foreign-codex-session',
    roleId: 'implementation',
    kernelStreamId: 'foreign-stream',
  })
  const event = normalizer.ingest(kernelEvent(1, 'task_started', { turn_id: 'foreign-turn' }))
  assert.ok(event)
  const projection = new DeliveryRuntimeProjection({ delivery: deliveryFixture() })
  assert.throws(
    () => projection.apply(event),
    error => error instanceof DeliveryRuntimeProjectionError
      && error.code === 'RUNTIME_SESSION_UNBOUND',
  )

  const sharedSessionId = 'shared-direct-session'
  const directNormalizer = new CodexRuntimeProjector({
    sessionId: sharedSessionId,
    kernelSessionId: sharedSessionId,
    roleId: 'implementation',
    kernelStreamId: 'direct-stream',
  })
  const directEvent = directNormalizer.ingest(kernelEvent(
    1,
    'task_started',
    { turn_id: 'direct-turn' },
  ))
  assert.ok(directEvent)
  const directProjection = new DeliveryRuntimeProjection({
    delivery: deliveryFixture({
      boundDshSessionId: sharedSessionId,
      boundCodexSessionId: sharedSessionId,
    }),
  })
  assert.deepEqual(directProjection.apply(directEvent), { changed: true })
})

test('Delivery runtime projection bounds duplicate recognition without retaining an event log', () => {
  const normalizer = new CodexRuntimeProjector({
    sessionId: dshSessionId,
    kernelSessionId: codexSessionId,
    roleId: 'implementation',
    kernelStreamId: 'bounded-stream',
  })
  const events = normalizer.replay([
    kernelEvent(1, 'task_started', { turn_id: 'bounded-turn' }),
    kernelEvent(2, 'token_count', {
      info: { total_token_usage: { total_tokens: 1 } },
    }),
    kernelEvent(3, 'task_complete', {
      turn_id: 'bounded-turn',
      last_agent_message: 'Done.',
      error: null,
    }),
  ])
  const projection = new DeliveryRuntimeProjection({
    delivery: deliveryFixture(),
    rememberedEventLimit: 2,
  })
  projection.replay(events)
  assert.deepEqual(projection.apply(events[2]), { changed: false })
  assert.throws(
    () => projection.apply(events[0]),
    error => error instanceof DeliveryRuntimeProjectionError
      && error.code === 'RUNTIME_SEQUENCE_OUT_OF_ORDER',
  )
})
