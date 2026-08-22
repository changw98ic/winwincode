import assert from 'node:assert/strict'
import test from 'node:test'

import {
  STRONGFLOW_ROLE_IDS,
  createStrongFlowRoleConfiguration,
} from '../packages/contracts/dist/index.js'
import {
  EMPTY_STRONGFLOW_ROLE_BUDGET_BASELINE,
  StrongFlowRoleRunner,
} from '../packages/strongflow/dist/index.js'

const HASH_A = 'a'.repeat(64)
const HASH_B = 'b'.repeat(64)

const modelCatalog = Object.freeze([
  Object.freeze({
    provider: 'fixture-provider',
    model: 'fixture-model',
    reasoningEfforts: Object.freeze(['medium']),
  }),
])

const roleConfiguration = createStrongFlowRoleConfiguration(
  Object.fromEntries(STRONGFLOW_ROLE_IDS.map(roleId => [roleId, {
    modelRoute: { provider: 'fixture-provider', model: 'fixture-model' },
    reasoningEffort: 'medium',
    budget: {
      maxTurns: 4,
      maxWallTimeMillis: 2_000,
      maxTotalTokens: 10_000,
      maxCostUsdMicros: 100_000,
    },
  }])),
  modelCatalog,
)

function deferred() {
  let resolvePromise = () => {}
  const promise = new Promise(resolve => {
    resolvePromise = resolve
  })
  return { promise, resolve: resolvePromise }
}

function rawEvent(sequence, type, data = {}, turnId = 'turn-role-fixture') {
  const payload = { id: turnId, msg: { type, ...data } }
  return Object.freeze({
    sequence: BigInt(sequence),
    kind: type,
    payload,
    rawJson: JSON.stringify(payload),
  })
}

function usageEvent(sequence, totalTokens = 14, turnId = 'turn-role-fixture') {
  return rawEvent(sequence, 'token_count', {
    info: {
      total_token_usage: {
        input_tokens: totalTokens - 4,
        cached_input_tokens: 2,
        cache_write_input_tokens: 0,
        output_tokens: 4,
        reasoning_output_tokens: 1,
        total_tokens: totalTokens,
      },
      last_token_usage: {
        input_tokens: totalTokens - 4,
        output_tokens: 4,
        total_tokens: totalTokens,
      },
    },
    rate_limits: null,
  }, turnId)
}

function outputEnvelope(roleSpec, overrides = {}) {
  const artifacts = roleSpec.requiredOutputArtifacts.map(kind => ({
    kind,
    artifact: { marker: kind, source: 'fixture' },
  }))
  return JSON.stringify({
    schemaVersion: 1,
    artifacts,
    ...overrides,
  })
}

function defaultEvents(roleSpec) {
  return [
    rawEvent(1, 'task_started', { turn_id: 'turn-role-fixture', started_at: 100 }),
    usageEvent(2),
    rawEvent(3, 'task_complete', {
      turn_id: 'turn-role-fixture',
      last_agent_message: outputEnvelope(roleSpec),
      error: null,
      completed_at: 101,
    }),
  ]
}

class FakeRoleSession {
  constructor(roleSpec, options = {}) {
    this.roleSpec = Object.freeze({
      ...roleSpec,
      ...(options.budget === undefined
        ? {}
        : { budget: Object.freeze({ ...roleSpec.budget, ...options.budget }) }),
    })
    this.options = options
    this.context = Object.freeze({
      schemaVersion: 1,
      kernelSessionLineageId: `kernel-lineage-sha256-${HASH_A}`,
      contextId: `role-context-sha256-${HASH_B}`,
      roleSpecId: `role-spec-sha256-${HASH_A}`,
      jobId: 'job-role-runner-fixture',
      stageRunId: 'stage-run-role-runner-fixture',
      attemptId: 'attempt-role-runner-fixture',
      roleSpec: this.roleSpec,
      workspace: Object.freeze({
        roleId: roleSpec.id,
        stageRunId: 'stage-run-role-runner-fixture',
        workspaceId: `workspace-sha256-${HASH_A}`,
        mode: roleSpec.workspaceMode,
        path: '/fixture/workspace',
        sourceSnapshotId: `source-sha256-${HASH_B}`,
      }),
    })
    this.kernel = Object.freeze({
      generation: 1,
      source: 'create',
      kernelSessionId: 'kernel-role-runner-fixture',
      kernelStreamId: 'kernel-stream-role-runner-fixture',
      rolloutPath: '/fixture/rollout.jsonl',
      acceptedAtMillis: 1,
    })
    this.closed = deferred()
  }

  submissions = []
  cancellations = []
  failures = []
  disposals = 0

  get state() {
    return 'ready'
  }

  get eventFailure() {
    return undefined
  }

  get summary() {
    return Object.freeze({
      kernelSessionLineageId: this.context.kernelSessionLineageId,
      contextId: this.context.contextId,
      jobId: this.context.jobId,
      stageRunId: this.context.stageRunId,
      attemptId: this.context.attemptId,
      roleId: this.roleSpec.id,
      kernelSessionId: this.kernel.kernelSessionId,
      kernelStreamId: this.kernel.kernelStreamId,
      generation: 1,
      state: 'ready',
    })
  }

  async submitTurn(text) {
    this.submissions.push(text)
    if (this.options.submissionError !== undefined) throw this.options.submissionError
    return this.options.submission ?? Object.freeze({
      status: 'started',
      turnId: 'turn-role-fixture',
    })
  }

  async *events() {
    if (this.options.streamError !== undefined) throw this.options.streamError
    const events = this.options.events ?? defaultEvents(this.roleSpec)
    for (const event of events) {
      yield Object.freeze({
        kernelSessionLineageId: this.context.kernelSessionLineageId,
        contextId: this.context.contextId,
        generation: this.kernel.generation,
        kernelSessionId: this.kernel.kernelSessionId,
        kernelStreamId: this.kernel.kernelStreamId,
        event,
      })
    }
    if (this.options.keepOpen === true) await this.closed.promise
  }

  async cancel(reason) {
    this.cancellations.push(reason)
    this.closed.resolve()
  }

  async fail(reason, options = {}) {
    this.failures.push({ reason, interrupt: options.interrupt ?? false })
    this.closed.resolve()
    if (this.options.teardownError !== undefined) throw this.options.teardownError
  }

  async dispose() {
    this.disposals += 1
    this.closed.resolve()
    if (this.options.teardownError !== undefined) throw this.options.teardownError
  }
}

class RecordingRunRecorder {
  constructor(options = {}) {
    this.options = options
  }

  events = []
  results = []
  flushes = 0

  async appendKernelEvent(event) {
    if (this.options.appendError !== undefined) throw this.options.appendError
    this.events.push(event)
  }

  async finish(result) {
    if (this.options.finishError !== undefined) throw this.options.finishError
    this.results.push(result)
  }

  async flush() {
    this.flushes += 1
    if (this.options.flushError !== undefined) throw this.options.flushError
  }
}

function roleSpec(roleId) {
  const result = roleConfiguration.roles.find(role => role.id === roleId)
  assert.ok(result)
  return result
}

function inputsFor(spec) {
  return spec.acceptedInputArtifacts.map((kind, index) => Object.freeze({
    artifactId: `artifact-${kind.toLowerCase().replaceAll('_', '-')}-${index}`,
    kind,
    value: Object.freeze({ marker: kind, index }),
  }))
}

function validatorsFor(spec, options = {}) {
  return spec.requiredOutputArtifacts.map(kind => Object.freeze({
    kind,
    validate(value, context) {
      if (options.failureKind === kind) throw new Error('fixture validator rejected artifact')
      assert.equal(context.artifactKind, kind)
      assert.equal(context.roleSession.roleSpec.id, spec.id)
      assert.deepEqual(value, { marker: kind, source: 'fixture' })
      return Object.freeze({
        kind,
        marker: value.marker,
        eventCount: context.eventInterval.eventCount,
      })
    },
  }))
}

function runner(recorder = new RecordingRunRecorder(), cost = usage => usage.totalTokens * 2) {
  return {
    recorder,
    value: new StrongFlowRoleRunner({
      recorder,
      costAccountant: {
        costUsdMicros(request) {
          return cost(request.tokenUsage)
        },
      },
    }),
  }
}

function requestFor(session, validators = validatorsFor(session.roleSpec), overrides = {}) {
  return {
    session,
    inputs: inputsFor(session.roleSpec),
    validators,
    budgetBaseline: EMPTY_STRONGFLOW_ROLE_BUDGET_BASELINE,
    ...overrides,
  }
}

test('extracts the exact typed artifacts and usage owed by every governed role', async () => {
  for (const roleId of STRONGFLOW_ROLE_IDS) {
    const session = new FakeRoleSession(roleSpec(roleId))
    const { recorder, value } = runner()
    const result = await value.run(requestFor(session))

    assert.equal(result.outcome, 'succeeded', roleId)
    assert.deepEqual(
      Object.keys(result.artifacts),
      session.roleSpec.requiredOutputArtifacts,
      roleId,
    )
    for (const kind of session.roleSpec.requiredOutputArtifacts) {
      assert.deepEqual(result.artifacts[kind], { kind, marker: kind, eventCount: 3 })
    }
    assert.equal(result.usage.turnsStarted, 1)
    assert.equal(result.usage.tokenUsage.totalTokens, 14)
    assert.equal(result.usage.costUsdMicros, 28)
    assert.equal(result.usage.usageEvents, 1)
    assert.deepEqual(
      result.inputArtifacts,
      inputsFor(session.roleSpec).map(({ artifactId, kind }) => ({ artifactId, kind })),
    )
    assert.deepEqual(result.eventInterval, {
      schemaVersion: 1,
      contextId: session.context.contextId,
      generation: 1,
      kernelSessionId: session.kernel.kernelSessionId,
      kernelStreamId: session.kernel.kernelStreamId,
      turnId: 'turn-role-fixture',
      firstSequence: '1',
      lastSequence: '3',
      eventCount: 3,
    })
    assert.equal(session.submissions.length, 1)
    assert.match(session.submissions[0], /Your final answer must be exactly one JSON object/u)
    assert.equal(session.disposals, 1)
    assert.equal(session.failures.length, 0)
    assert.equal(recorder.events.length, 3)
    assert.equal(recorder.events[0].sequence, '1')
    assert.equal(recorder.events[0].schemaVersion, 1)
    assert.doesNotThrow(() => JSON.stringify(recorder.events))
    assert.doesNotThrow(() => JSON.stringify(result))
    assert.equal(recorder.results.length, 1)
    assert.equal(recorder.flushes, 1)
  }
})

test('rejects an oversized model-visible handoff before submitting a kernel turn', async () => {
  const spec = roleSpec('requirements')
  const session = new FakeRoleSession(spec)
  const recorder = new RecordingRunRecorder()
  const value = new StrongFlowRoleRunner({
    recorder,
    costAccountant: {
      costUsdMicros(request) {
        return request.tokenUsage.totalTokens * 2
      },
    },
    maxInputBytes: 64,
  })
  const result = await value.run(requestFor(session))
  assert.equal(result.outcome, 'failed')
  assert.equal(result.failure.code, 'INPUT_CONTEXT_LIMIT_EXCEEDED')
  assert.equal(result.failure.category, 'input')
  assert.equal(session.submissions.length, 0)
  assert.equal(recorder.results.length, 1)
})

test('free-form, mismatched, and schema-invalid model output are explicit failures', async t => {
  const spec = roleSpec('solution')
  const cases = [
    {
      name: 'free-form output',
      code: 'OUTPUT_MALFORMED',
      text: 'The solution and diagrams are complete.',
    },
    {
      name: 'missing diagram',
      code: 'OUTPUT_MISMATCH',
      text: JSON.stringify({
        schemaVersion: 1,
        artifacts: [{
          kind: 'SOLUTION_DESIGN',
          artifact: { marker: 'SOLUTION_DESIGN', source: 'fixture' },
        }],
      }),
    },
    {
      name: 'wrong artifact order',
      code: 'OUTPUT_MISMATCH',
      text: JSON.stringify({
        schemaVersion: 1,
        artifacts: [...JSON.parse(outputEnvelope(spec)).artifacts].reverse(),
      }),
    },
  ]
  for (const fixtureCase of cases) {
    await t.test(fixtureCase.name, async () => {
      const events = [
        rawEvent(1, 'task_started', { turn_id: 'turn-role-fixture' }),
        usageEvent(2),
        rawEvent(3, 'task_complete', {
          turn_id: 'turn-role-fixture',
          last_agent_message: fixtureCase.text,
          error: null,
        }),
      ]
      const session = new FakeRoleSession(spec, { events })
      const { value } = runner()
      const result = await value.run(requestFor(session))
      assert.notEqual(result.outcome, 'succeeded')
      assert.equal(result.failure.code, fixtureCase.code)
      assert.equal(result.eventInterval.firstSequence, '1')
      assert.equal(result.eventInterval.lastSequence, '3')
      assert.deepEqual(session.failures, [{
        reason: `Governed role run failed: ${fixtureCase.code}`,
        interrupt: false,
      }])
    })
  }

  await t.test('artifact validator rejection', async () => {
    const session = new FakeRoleSession(spec)
    const { value } = runner()
    const result = await value.run(requestFor(
      session,
      validatorsFor(spec, { failureKind: 'SYSTEM_ARCHITECTURE_DIAGRAM' }),
    ))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'ARTIFACT_INVALID')
  })
})

test('input identities and output validators must match the immutable role specification', async t => {
  const spec = roleSpec('solution')

  await t.test('missing identified input', async () => {
    const session = new FakeRoleSession(spec)
    const { value } = runner()
    const result = await value.run(requestFor(session, undefined, { inputs: [] }))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'INPUT_ARTIFACT_MISMATCH')
    assert.equal(result.eventInterval.eventCount, 0)
    assert.equal(session.submissions.length, 0)
  })

  await t.test('missing artifact validator', async () => {
    const session = new FakeRoleSession(spec)
    const { value } = runner()
    const result = await value.run(requestFor(session, []))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'VALIDATOR_MISMATCH')
    assert.equal(result.eventInterval.eventCount, 0)
    assert.equal(session.submissions.length, 0)
  })
})

test('model, tool, and sandbox failures stop the exact role turn', async t => {
  const spec = roleSpec('executor')
  const cases = [
    {
      name: 'model failure',
      code: 'MODEL_FAILED',
      event: rawEvent(2, 'error', { message: 'fixture provider failed' }),
    },
    {
      name: 'tool failure',
      code: 'TOOL_FAILED',
      event: rawEvent(2, 'item_completed', {
        turn_id: 'turn-role-fixture',
        item: {
          type: 'CommandExecution',
          id: 'tool-1',
          status: 'failed',
          exit_code: 1,
          stderr: 'fixture command failed',
        },
      }),
    },
    {
      name: 'sandbox denial',
      code: 'SANDBOX_DENIED',
      event: rawEvent(2, 'request_permissions', {
        turn_id: 'turn-role-fixture',
        call_id: 'approval-1',
      }),
    },
  ]
  for (const fixtureCase of cases) {
    await t.test(fixtureCase.name, async () => {
      const session = new FakeRoleSession(spec, {
        events: [
          rawEvent(1, 'task_started', { turn_id: 'turn-role-fixture' }),
          fixtureCase.event,
        ],
      })
      const { value } = runner()
      const result = await value.run(requestFor(session))
      assert.notEqual(result.outcome, 'succeeded')
      assert.equal(result.failure.code, fixtureCase.code)
      assert.equal(result.eventInterval.eventCount, 2)
      assert.deepEqual(session.failures, [{
        reason: `Governed role run failed: ${fixtureCase.code}`,
        interrupt: true,
      }])
    })
  }
})

test('kernel command approvals remain in-band after the host submits a human decision', async () => {
  const spec = roleSpec('executor')
  const session = new FakeRoleSession(spec, {
    events: [
      rawEvent(1, 'task_started', { turn_id: 'turn-role-fixture' }),
      rawEvent(2, 'exec_approval_request', {
        turn_id: 'turn-role-fixture',
        call_id: 'approval-1',
      }),
      usageEvent(3),
      rawEvent(4, 'task_complete', {
        turn_id: 'turn-role-fixture',
        last_agent_message: outputEnvelope(spec),
        error: null,
      }),
    ],
  })
  const { value } = runner()

  const result = await value.run(requestFor(session))

  assert.equal(result.outcome, 'succeeded')
  assert.equal(result.eventInterval.eventCount, 4)
  assert.deepEqual(session.failures, [])
})

test('cancellation, timeout, and each budget limit return bounded non-success results', async t => {
  const spec = roleSpec('requirements')

  await t.test('external cancellation', async () => {
    const controller = new AbortController()
    const session = new FakeRoleSession(spec, { events: [], keepOpen: true })
    const { value } = runner()
    setImmediate(() => controller.abort())
    const result = await value.run(requestFor(session, undefined, { signal: controller.signal }))
    assert.equal(result.outcome, 'cancelled')
    assert.equal(result.failure.code, 'CANCELLED')
    assert.equal(result.eventInterval.eventCount, 0)
    assert.deepEqual(session.cancellations, ['Governed role run cancelled'])
  })

  await t.test('wall-time timeout', async () => {
    const session = new FakeRoleSession(spec, {
      events: [],
      keepOpen: true,
      budget: { maxWallTimeMillis: 20 },
    })
    const { value } = runner()
    const result = await value.run(requestFor(session))
    assert.equal(result.outcome, 'timed-out')
    assert.equal(result.failure.code, 'TIMEOUT')
    assert.deepEqual(session.failures, [{
      reason: 'Governed role run failed: TIMEOUT',
      interrupt: true,
    }])
  })

  await t.test('turn budget', async () => {
    const session = new FakeRoleSession(spec, { budget: { maxTurns: 1 } })
    const { value } = runner()
    const result = await value.run(requestFor(session, undefined, {
      budgetBaseline: Object.freeze({
        ...EMPTY_STRONGFLOW_ROLE_BUDGET_BASELINE,
        turnsStarted: 1,
      }),
    }))
    assert.equal(result.outcome, 'budget-exceeded')
    assert.equal(result.failure.code, 'TURN_BUDGET_EXCEEDED')
    assert.equal(session.submissions.length, 0)
  })

  await t.test('token budget', async () => {
    const session = new FakeRoleSession(spec, {
      budget: { maxTotalTokens: 10 },
      events: [
        rawEvent(1, 'task_started', { turn_id: 'turn-role-fixture' }),
        usageEvent(2, 14),
      ],
    })
    const { value } = runner()
    const result = await value.run(requestFor(session))
    assert.equal(result.outcome, 'budget-exceeded')
    assert.equal(result.failure.code, 'TOKEN_BUDGET_EXCEEDED')
    assert.equal(result.usage.tokenUsage.totalTokens, 14)
  })

  await t.test('cost budget', async () => {
    const session = new FakeRoleSession(spec, {
      budget: { maxCostUsdMicros: 20 },
      events: [
        rawEvent(1, 'task_started', { turn_id: 'turn-role-fixture' }),
        usageEvent(2, 14),
      ],
    })
    const { value } = runner()
    const result = await value.run(requestFor(session))
    assert.equal(result.outcome, 'budget-exceeded')
    assert.equal(result.failure.code, 'COST_BUDGET_EXCEEDED')
    assert.equal(result.usage.costUsdMicros, 28)
  })
})

test('submission, usage, recording, and teardown failures never become success', async t => {
  const spec = roleSpec('planner')

  await t.test('submission rejected', async () => {
    const session = new FakeRoleSession(spec, {
      submission: Object.freeze({ status: 'not_submitted', reason: 'fixture busy' }),
    })
    const { value } = runner()
    const result = await value.run(requestFor(session))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'SUBMISSION_REJECTED')
    assert.equal(result.eventInterval.eventCount, 0)
  })

  await t.test('submission call failed', async () => {
    const session = new FakeRoleSession(spec, {
      submissionError: new Error('fixture native submit failed'),
    })
    const { value } = runner()
    const result = await value.run(requestFor(session))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'SUBMISSION_FAILED')
    assert.equal(result.eventInterval.eventCount, 0)
  })

  await t.test('event stream failed', async () => {
    const session = new FakeRoleSession(spec, {
      streamError: new Error('fixture event stream failed'),
    })
    const { value } = runner()
    const result = await value.run(requestFor(session))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'EVENT_STREAM_FAILED')
    assert.equal(result.eventInterval.eventCount, 0)
  })

  await t.test('usage missing', async () => {
    const session = new FakeRoleSession(spec, {
      events: [
        rawEvent(1, 'task_started', { turn_id: 'turn-role-fixture' }),
        rawEvent(2, 'task_complete', {
          turn_id: 'turn-role-fixture',
          last_agent_message: outputEnvelope(spec),
          error: null,
        }),
      ],
    })
    const { value } = runner()
    const result = await value.run(requestFor(session))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'USAGE_MISSING')
  })

  await t.test('final output missing', async () => {
    const session = new FakeRoleSession(spec, {
      events: [
        rawEvent(1, 'task_started', { turn_id: 'turn-role-fixture' }),
        usageEvent(2),
        rawEvent(3, 'task_complete', {
          turn_id: 'turn-role-fixture',
          error: null,
        }),
      ],
    })
    const { value } = runner()
    const result = await value.run(requestFor(session))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'OUTPUT_MISSING')
  })

  await t.test('record append failure', async () => {
    const recorder = new RecordingRunRecorder({
      appendError: new Error('fixture append failed'),
    })
    const session = new FakeRoleSession(spec)
    const { value } = runner(recorder)
    const result = await value.run(requestFor(session))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'RECORDING_FAILED')
    assert.deepEqual(session.failures, [{
      reason: 'Governed role run failed: RECORDING_FAILED',
      interrupt: true,
    }])
  })

  await t.test('record flush failure', async () => {
    const recorder = new RecordingRunRecorder({ flushError: new Error('fixture flush failed') })
    const session = new FakeRoleSession(spec)
    const { value } = runner(recorder)
    const result = await value.run(requestFor(session))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'RECORDING_FAILED')
    assert.equal(recorder.results[0].outcome, 'succeeded')
  })

  await t.test('session teardown failure', async () => {
    const session = new FakeRoleSession(spec, {
      teardownError: new Error('fixture close failed'),
    })
    const { recorder, value } = runner()
    const result = await value.run(requestFor(session))
    assert.notEqual(result.outcome, 'succeeded')
    assert.equal(result.failure.code, 'TEARDOWN_FAILED')
    assert.equal(recorder.results[0].failure.code, 'TEARDOWN_FAILED')
  })
})
