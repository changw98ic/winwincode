import assert from 'node:assert/strict'
import test from 'node:test'

import {
  CodexRuntimeProjector,
  DshProjectionError,
  DshRuntimeProjection,
  RuntimeApprovalRouter,
  RuntimeProjectionError,
  streamRuntimeEvents,
} from '../packages/dsh-profile/dist/index.js'

const sessionId = 'session-runtime-fixture'
const kernelSessionId = 'kernel-session-runtime-fixture'
const roleId = 'implementation'
const kernelStreamId = 'kernel-stream-1'

function kernelEvent(sequence, type, data = {}, submissionId = 'submission-1') {
  const payload = { id: submissionId, msg: { type, ...data } }
  return Object.freeze({
    sequence: BigInt(sequence),
    kind: type === 'task_started'
      ? 'turn_started'
      : type === 'task_complete'
        ? 'turn_complete'
        : type,
    payload,
    rawJson: JSON.stringify(payload),
  })
}

function fixtureEvents() {
  return [
    kernelEvent(1, 'session_configured', {
      session_id: sessionId,
      thread_id: sessionId,
      model_provider_id: 'deepseek-compatible',
      model: 'fixture-coder',
    }, 'startup'),
    kernelEvent(2, 'task_started', {
      turn_id: 'turn-1',
      started_at: 100,
      model_context_window: 64_000,
    }),
    kernelEvent(3, 'user_message', {
      message: 'Implement the projection.',
      images: null,
      local_images: [],
      audio: null,
      local_audio: [],
      text_elements: [],
    }),
    kernelEvent(4, 'agent_message_content_delta', {
      thread_id: sessionId,
      turn_id: 'turn-1',
      item_id: 'message-1',
      delta: 'Projection ',
    }),
    kernelEvent(5, 'agent_message', { message: 'Projection ready.' }),
    kernelEvent(6, 'exec_approval_request', {
      call_id: 'exec-1',
      approval_id: 'approval-1',
      turn_id: 'turn-1',
      started_at_ms: 100_100,
      command: ['printf', 'ok'],
      cwd: '/workspace',
      parsed_cmd: [],
    }),
    kernelEvent(7, 'item_started', {
      thread_id: sessionId,
      turn_id: 'turn-1',
      started_at_ms: 100_200,
      item: {
        type: 'CommandExecution',
        id: 'exec-1',
        command: ['printf', 'ok'],
        cwd: 'file:///workspace',
        parsed_cmd: [],
        source: 'agent',
        status: 'in_progress',
      },
    }),
    kernelEvent(8, 'item_completed', {
      thread_id: sessionId,
      turn_id: 'turn-1',
      started_at_ms: 100_200,
      completed_at_ms: 100_300,
      item: {
        type: 'CommandExecution',
        id: 'exec-1',
        command: ['printf', 'ok'],
        cwd: 'file:///workspace',
        parsed_cmd: [],
        source: 'agent',
        status: 'completed',
        stdout: 'ok',
        stderr: '',
        aggregated_output: 'ok',
        exit_code: 0,
        formatted_output: 'ok',
      },
    }),
    kernelEvent(9, 'turn_diff', { unified_diff: '--- a\n+++ b\n' }),
    kernelEvent(10, 'token_count', {
      info: {
        total_token_usage: { input_tokens: 10, output_tokens: 4, total_tokens: 14 },
        last_token_usage: { input_tokens: 10, output_tokens: 4, total_tokens: 14 },
      },
      rate_limits: null,
    }),
    kernelEvent(11, 'collab_agent_spawn_begin', {
      call_id: 'spawn-1',
      started_at_ms: 100_400,
      sender_thread_id: sessionId,
      prompt: 'Review the implementation.',
      model: 'fixture-coder',
      reasoning_effort: 'medium',
    }),
    kernelEvent(12, 'collab_agent_spawn_end', {
      call_id: 'spawn-1',
      completed_at_ms: 100_500,
      sender_thread_id: sessionId,
      new_thread_id: 'agent-review-1',
      new_agent_nickname: 'reviewer',
      new_agent_role: 'review',
      prompt: 'Review the implementation.',
      model: 'fixture-coder',
      reasoning_effort: 'medium',
      status: 'completed',
    }),
    kernelEvent(13, 'error', {
      message: 'fixture failure',
      codex_error_info: { type: 'other' },
    }),
    kernelEvent(14, 'task_complete', {
      turn_id: 'turn-1',
      last_agent_message: 'Projection ready.',
      error: { message: 'fixture failure', codex_error_info: { type: 'other' } },
      started_at: 100,
      completed_at: 101,
      duration_ms: 1_000,
    }),
  ]
}

test('live delivery and replay build the same normalized DSH state', async () => {
  const source = fixtureEvents()
  const liveNormalizer = new CodexRuntimeProjector({
    sessionId,
    kernelSessionId,
    roleId,
    kernelStreamId,
    rememberedEventLimit: 32,
  })
  const liveProjection = new DshRuntimeProjection({
    sessionId,
    roleId,
    rowLimit: 32,
    deduplicationLimit: 32,
  })
  const runtimeEvents = []
  const liveAppends = []

  for (const raw of source.slice(0, 6)) {
    const event = liveNormalizer.ingest(raw)
    assert.ok(event)
    runtimeEvents.push(event)
    liveAppends.push(...liveProjection.apply(event).sessionAppends)
  }

  const approval = liveProjection.pendingApproval('approval-1')
  assert.deepEqual(approval, {
    id: 'approval-1',
    kind: 'exec',
    operationId: 'approval-1',
    source: runtimeEvents[5].source,
    payload: runtimeEvents[5].data,
  })
  const approvalCalls = []
  const router = new RuntimeApprovalRouter({
    async resolveApproval(response) {
      approvalCalls.push(response)
      return 'approval-submission-1'
    },
  }, liveProjection)
  assert.equal(await router.resolve({
    approvalId: 'approval-1',
    decision: { kind: 'approved' },
  }), 'approval-submission-1')
  assert.deepEqual(approvalCalls, [{
    sessionId: kernelSessionId,
    kind: 'exec',
    operationId: 'approval-1',
    turnId: 'turn-1',
    decision: { kind: 'approved' },
  }])
  await assert.rejects(
    router.resolve({ approvalId: 'approval-1', decision: { kind: 'abort' } }),
    error => error instanceof DshProjectionError
      && error.code === 'APPROVAL_ALREADY_SUBMITTED',
  )

  for (const raw of source.slice(6)) {
    const event = liveNormalizer.ingest(raw)
    assert.ok(event)
    runtimeEvents.push(event)
    liveAppends.push(...liveProjection.apply(event).sessionAppends)
  }

  const replayNormalizer = new CodexRuntimeProjector({
    sessionId,
    kernelSessionId,
    roleId,
    kernelStreamId,
  })
  const replayEvents = replayNormalizer.replay(source)
  const replayProjection = new DshRuntimeProjection({ sessionId, roleId, rowLimit: 32 })
  const replayAppends = replayProjection.replay(replayEvents)

  assert.deepEqual(replayEvents, runtimeEvents)
  assert.deepEqual(replayAppends, liveAppends)
  assert.deepEqual(replayProjection.snapshot, liveProjection.snapshot)
  assert.equal(liveProjection.snapshot.status, 'failed')
  assert.equal(liveProjection.snapshot.pendingApprovals.length, 0)
  assert.equal(liveProjection.snapshot.latestDiff.unified_diff, '--- a\n+++ b\n')
  assert.equal(liveProjection.snapshot.latestUsage.type, 'token_count')
  assert.ok(liveProjection.snapshot.rows.some(row => (
    row.id === 'tool:exec-1'
    && row.status === 'completed'
    && row.source.roleId === roleId
  )))
  assert.ok(liveProjection.snapshot.rows.some(row => (
    row.id === 'subagent:agent-review-1'
    && row.source.agentThreadId === 'agent-review-1'
  )))
  assert.ok(liveProjection.snapshot.rows.some(row => (
    row.kind === 'failure'
    && row.source.sessionId === sessionId
    && row.source.roleId === roleId
  )))
  assert.ok(liveAppends.some(event => event.type === 'turn/start'))
  assert.ok(liveAppends.some(event => event.type === 'assistant/chunk'))
  assert.ok(liveAppends.some(event => event.type === 'assistant/message'))
  assert.ok(liveAppends.some(event => event.type === 'tool/call'))
  assert.ok(liveAppends.some(event => event.type === 'tool/result'))
  assert.ok(liveAppends.some(event => event.type === 'turn/end'))
  assert.ok(liveAppends.every(event => event.sourceEventId.startsWith(`${sessionId}@`)))

  assert.equal(liveNormalizer.ingest(source[13]), undefined)
  assert.deepEqual(liveProjection.apply(runtimeEvents[13]), {
    changed: false,
    sessionAppends: [],
  })
})

test('missing, conflicting, and old kernel events fail with stable codes', () => {
  const source = fixtureEvents()
  const missing = new CodexRuntimeProjector({ sessionId, roleId, kernelStreamId })
  assert.throws(
    () => missing.ingest(source[1]),
    error => error instanceof RuntimeProjectionError
      && error.code === 'EVENT_SEQUENCE_MISSING'
      && error.expectedSequence === '1'
      && error.actualSequence === '2',
  )

  const conflicting = new CodexRuntimeProjector({ sessionId, roleId, kernelStreamId })
  conflicting.ingest(source[0])
  const changed = kernelEvent(1, 'session_configured', {
    session_id: sessionId,
    model_provider_id: 'changed',
    model: 'changed',
  }, 'startup')
  assert.throws(
    () => conflicting.ingest(changed),
    error => error instanceof RuntimeProjectionError
      && error.code === 'EVENT_SEQUENCE_CONFLICT',
  )

  const bounded = new CodexRuntimeProjector({
    sessionId,
    roleId,
    kernelStreamId,
    rememberedEventLimit: 2,
  })
  bounded.replay(source.slice(0, 3))
  assert.throws(
    () => bounded.ingest(source[0]),
    error => error instanceof RuntimeProjectionError
      && error.code === 'EVENT_SEQUENCE_OUT_OF_ORDER',
  )
})

test('normalized cursors remain monotonic when a resumed native stream restarts at one', () => {
  const initial = new CodexRuntimeProjector({ sessionId, roleId, kernelStreamId: 'lifecycle-1' })
  const prefix = initial.replay([
    kernelEvent(1, 'session_configured', {
      session_id: sessionId,
      model_provider_id: 'deepseek-compatible',
      model: 'fixture-coder',
    }, 'startup-1'),
    kernelEvent(2, 'task_started', { turn_id: 'turn-before-restart' }, 'turn-before-restart'),
  ])
  assert.deepEqual(initial.checkpoint, {
    cursor: { sessionId, sequence: '2' },
    kernelStreamId: 'lifecycle-1',
    kernelSequence: '2',
  })

  const resumed = new CodexRuntimeProjector({
    sessionId,
    roleId,
    kernelStreamId: 'lifecycle-2',
    startAfterSequence: initial.cursor.sequence,
  })
  const suffix = resumed.replay([
    kernelEvent(1, 'task_complete', {
      turn_id: 'turn-before-restart',
      last_agent_message: 'resumed',
      error: null,
    }, 'turn-before-restart'),
  ])

  assert.equal(suffix[0].id, `${sessionId}@3`)
  assert.equal(suffix[0].cursor.sequence, '3')
  assert.equal(suffix[0].source.kernelStreamId, 'lifecycle-2')
  assert.equal(suffix[0].source.kernelSequence, '1')
  const projection = new DshRuntimeProjection({ sessionId, roleId })
  projection.replay([...prefix, ...suffix])
  assert.equal(projection.snapshot.asOfSequence, '3')
  assert.equal(projection.snapshot.status, 'completed')
})

test('live runtime stream uses the same normalizer without adding an unbounded queue', async () => {
  const sourceEvents = fixtureEvents().slice(0, 3)
  const received = []
  const source = {
    async *events(requestedSessionId, options) {
      received.push({ requestedSessionId, options })
      yield* sourceEvents
    },
  }
  const stream = streamRuntimeEvents(source, {
    sessionId,
    kernelSessionId,
    roleId,
    kernelStreamId,
    timeoutMillis: 25,
  })
  const projected = []
  while (true) {
    const next = await stream.next()
    if (next.done) {
      assert.deepEqual(next.value, {
        cursor: { sessionId, sequence: '3' },
        kernelStreamId,
        kernelSequence: '3',
      })
      break
    }
    projected.push(next.value)
  }
  assert.equal(projected.length, 3)
  assert.deepEqual(received, [{
    requestedSessionId: kernelSessionId,
    options: { timeoutMillis: 25 },
  }])
})

test('DSH projection keeps active state bounded and fails before advancing its cursor', () => {
  const normalizer = new CodexRuntimeProjector({ sessionId, roleId, kernelStreamId })
  const projection = new DshRuntimeProjection({ sessionId, roleId, rowLimit: 1 })
  const turn = normalizer.ingest(kernelEvent(1, 'task_started', { turn_id: 'turn-1' }))
  const approval = normalizer.ingest(kernelEvent(2, 'exec_approval_request', {
    call_id: 'exec-1',
    turn_id: 'turn-1',
    command: ['true'],
    cwd: '/workspace',
    started_at_ms: 1,
    parsed_cmd: [],
  }))
  assert.ok(turn)
  assert.ok(approval)
  projection.apply(turn)
  assert.throws(
    () => projection.apply(approval),
    error => error instanceof DshProjectionError
      && error.code === 'PROJECTION_CAPACITY_EXCEEDED',
  )
  assert.equal(projection.snapshot.asOfSequence, '1')
  assert.equal(projection.snapshot.pendingApprovals.length, 0)
})
