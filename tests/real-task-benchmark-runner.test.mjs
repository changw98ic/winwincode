import assert from 'node:assert/strict'
import { readdir } from 'node:fs/promises'
import { resolve } from 'node:path'
import test from 'node:test'

import {
  aggregateBenchmarkReport,
  buildBenchmarkPlan,
  createToolRequestGuard,
  executeBenchmarkCell,
  executeFormalBenchmark,
  executeToolRequest,
  normalizeToolRequestIdentity,
  runBenchmarkPlan,
  validateFrozenTaskSource,
} from '../scripts/run-real-task-benchmark.mjs'

test('frozen task catalog produces exactly 700 unique benchmark cells', () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })

  assert.equal(plan.cells.length, 700)
  assert.equal(new Set(plan.cells.map(cell => cell.runId)).size, 700)
})

test('an independent fusion cell calls each member once and aggregates once', async () => {
  const calls = []
  const cell = {
    runId: 'main-C:task-0001:fusion-4',
    taskId: 'task-0001',
    comparison: 'fusion-4',
    fusionKind: 'independent-aggregate',
    reasoningEffort: 'max',
    budgetLimits: null,
  }
  const result = await executeBenchmarkCell(cell, {
    runModel: async request => {
      calls.push({ type: 'model', request })
      return { provider: request.provider, answer: request.provider }
    },
    aggregate: async request => {
      calls.push({ type: 'aggregate', request })
      return { answer: 'aggregated' }
    },
  })

  assert.deepEqual(calls.map(call => call.type), ['model', 'model', 'model', 'model', 'aggregate'])
  assert.deepEqual(calls.slice(0, 4).map(call => call.request.provider), [
    'glm5.1flash',
    'mimov2.6pro',
    'ds4.1flash',
    'qwen3.8flash',
  ])
  assert.ok(calls.slice(0, 4).every(call => call.request.reasoningEffort === 'max'))
  assert.equal(result.aggregate.answer, 'aggregated')
  assert.equal(calls[4].request.engine, 'fusion-engine')
  assert.equal(calls[4].request.algorithmVersion, 'fusion-4-v1')
  assert.equal('provider' in calls[4].request, false)
  assert.equal('modelId' in calls[4].request, false)
  assert.equal('reasoningEffort' in calls[4].request, false)
  assert.equal(result.aggregateReceipt.callKind, 'fusion-engine')
  assert.equal(result.aggregateReceipt.inputDigest, calls[4].request.inputDigest)
  assert.match(result.aggregateReceipt.outputDigest, /^[0-9a-f]{64}$/u)
})

test('the sixth identical normalized tool request is intercepted before execution', async () => {
  const gate = new Map()
  let executions = 0
  const identity = normalizeToolRequestIdentity({
    tool: 'read_file',
    target: 'src/main.rs',
    params: { range: [1, 20] },
    contentDigest: 'a'.repeat(64),
    requestId: 'request-1',
    timestamp: '2026-09-24T00:00:00Z',
    progress: { elapsedMs: 10 },
  })

  for (let occurrence = 1; occurrence <= 5; occurrence += 1) {
    await executeToolRequest(
      {
        tool: 'read_file',
        target: 'src/main.rs',
        params: { range: [1, 20] },
        contentDigest: 'a'.repeat(64),
        requestId: `request-${occurrence}`,
        timestamp: `2026-09-24T00:00:0${occurrence}Z`,
        progress: { elapsedMs: occurrence },
      },
      gate,
      async () => ({ executions: executions += 1 }),
    )
  }
  const blocked = await executeToolRequest(
    { tool: 'read_file', target: 'src/main.rs', params: { range: [1, 20] }, contentDigest: 'a'.repeat(64) },
    gate,
    async () => ({ executions: executions += 1 }),
  )

  assert.equal(identity, normalizeToolRequestIdentity({
    tool: 'read_file',
    target: 'src/main.rs',
    params: { range: [1, 20] },
    contentDigest: 'a'.repeat(64),
  }))
  assert.equal(executions, 5)
  assert.equal(blocked.status, 'terminated')
  assert.equal(blocked.reason, 'STUCK_TOOL_REPEAT_LIMIT')
})

test('tool identity recursively canonicalizes arguments and excludes request progress metadata', () => {
  const first = normalizeToolRequestIdentity({
    tool: 'search',
    target: 'src',
    args: {
      query: { all: ['race', 'lock'], any: { second: 2, first: 1 } },
      requestId: 'nested-request-1',
      timestamp: '2026-09-25T00:00:00Z',
      progress: { elapsedMs: 10 },
    },
    contentDigest: 'd'.repeat(64),
    requestId: 'request-1',
    timestamp: '2026-09-25T00:00:01Z',
    progress: { elapsedMs: 11 },
  })
  const reordered = normalizeToolRequestIdentity({
    progress: { elapsedMs: 99 },
    contentDigest: 'd'.repeat(64),
    target: 'src',
    timestamp: '2026-09-25T00:00:02Z',
    args: {
      progress: { elapsedMs: 98 },
      query: { any: { first: 1, second: 2 }, all: ['race', 'lock'] },
      timestamp: '2026-09-25T00:00:03Z',
      requestId: 'nested-request-2',
    },
    requestId: 'request-2',
    tool: 'search',
  })
  const changed = normalizeToolRequestIdentity({
    tool: 'search',
    target: 'src',
    args: { query: { all: ['race', 'lock'], any: { second: 2, first: 3 } } },
    contentDigest: 'd'.repeat(64),
  })

  assert.equal(first, reordered)
  assert.notEqual(first, changed)
})

test('tool repeat termination is an absorbing runner state', async () => {
  const guard = createToolRequestGuard()
  const request = {
    tool: 'read_file',
    target: 'src/main.rs',
    params: { range: [1, 20] },
    contentDigest: 'a'.repeat(64),
  }
  let executions = 0

  for (let occurrence = 1; occurrence <= 5; occurrence += 1) {
    await executeToolRequest(request, guard, async () => {
      executions += 1
    })
  }
  const firstTermination = await executeToolRequest(request, guard, async () => {
    executions += 1
  })
  const laterTermination = await executeToolRequest({
    tool: 'search',
    target: 'src',
    params: {},
    contentDigest: 'b'.repeat(64),
  }, guard, async () => {
    executions += 1
  })

  assert.equal(executions, 5)
  assert.equal(firstTermination.status, 'terminated')
  assert.equal(firstTermination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.deepEqual(laterTermination, firstTermination)
})

test('the model adapter tool boundary stops the cell at the sixth repeat before execution', async () => {
  let executions = 0
  await assert.rejects(
    executeBenchmarkCell({
      runId: 'main-A:task-0001:glm5.1flash',
      taskId: 'task-0001',
      comparison: 'glm5.1flash',
      fusionKind: null,
      reasoningEffort: 'max',
      budgetLimits: null,
    }, {
      runModel: async (_request, runner) => {
        for (let occurrence = 1; occurrence <= 6; occurrence += 1) {
          await runner.requestTool({
            tool: 'read_file',
            target: 'src/lib.rs',
            params: {},
            contentDigest: 'c'.repeat(64),
          }, async () => ({ executions: executions += 1 }))
        }
      },
    }),
    error => error.code === 'STUCK_TOOL_REPEAT_LIMIT',
  )
  assert.equal(executions, 5)
})

test('a stuck tool request terminates the local batch without dropping fixed-denominator rows', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  let cellCalls = 0
  const ledger = await runBenchmarkPlan(plan, {
    executeCell: async () => {
      cellCalls += 1
      return { status: 'failed', termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' } }
    },
  })

  assert.equal(cellCalls, 1)
  assert.equal(ledger.records.length, 700)
  assert.equal(ledger.records[0].status, 'failed')
  assert.equal(ledger.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(ledger.records.slice(1).every(record => record.status === 'not_run_runner_terminated'), true)
})

test('a record carrying runner termination cannot be published as completed or pass', async () => {
  const plan = {
    cells: [
      { runId: 'run-terminated', claims: [{ id: 'claim:kept', state: 'disputed' }] },
      { runId: 'run-pending', claims: [] },
    ],
  }
  const ledger = await runBenchmarkPlan(plan, {
    executeCell: async () => ({
      status: 'completed',
      verdict: 'pass',
      claims: [{ id: 'claim:kept', state: 'disputed' }],
      termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' },
    }),
  })

  assert.equal(ledger.records[0].status, 'failed')
  assert.equal(ledger.records[0].verdict, null)
  assert.equal(ledger.records[0].score, 0)
  assert.equal(ledger.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(ledger.records[1].status, 'not_run_runner_terminated')
  assert.equal(ledger.records[1].verdict, null)
  assert.equal(ledger.records[1].score, 0)
})

test('report aggregation accepts terminated zero scores but rejects surviving success markers', () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const records = plan.cells.map((cell, index) => ({
    ...cell,
    status: index === 0 ? 'failed' : 'not_run_runner_terminated',
    verdict: null,
    score: 0,
    termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' },
  }))
  const report = aggregateBenchmarkReport(plan, {
    records,
    experimentId: 'terminated-ledger-v1',
  })
  assert.equal(report.failureAccounting.completed, 0)
  assert.equal(report.failureAccounting.unsuccessful, 700)
  assert.equal(report.failureAccounting.stuckToolRepeatLimit, 700)

  const invalid = records.map((record, index) => index === 0 ? { ...record, verdict: 'pass' } : record)
  assert.throws(
    () => aggregateBenchmarkReport(plan, { records: invalid, experimentId: 'invalid-terminated-ledger' }),
    error => error.code === 'TERMINATION_STATE_INVALID',
  )
})

test('runner termination from tool admission preserves fixed-denominator claims', async () => {
  const firstClaims = [{ id: 'claim:one', state: 'disputed' }]
  const pendingClaims = [{ id: 'claim:two', state: 'disputed' }]
  const plan = {
    cells: [
      { runId: 'run-1', claims: firstClaims },
      { runId: 'run-2', claims: pendingClaims },
    ],
  }
  const toolRequest = {
    tool: 'read_file',
    target: 'src/main.rs',
    args: { range: [1, 20] },
    requestedContentDigest: 'a'.repeat(64),
  }
  let cellCalls = 0

  const ledger = await runBenchmarkPlan(plan, {
    executeCell: async (cell, { toolGate }) => {
      cellCalls += 1
      assert.equal(cell.runId, 'run-1')
      return executeBenchmarkCell({
        runId: cell.runId,
        taskId: 'task-0001',
        comparison: 'glm5.1flash',
        fusionKind: null,
        reasoningEffort: 'max',
        budgetLimits: null,
      }, {
        runModel: async (_request, runner) => {
          for (let occurrence = 1; occurrence <= 6; occurrence += 1) {
            await runner.requestTool(toolRequest, async () => ({ occurrence }))
          }
        },
      }, { toolGate })
    },
  })

  assert.equal(cellCalls, 1)
  assert.equal(ledger.denominator, 2)
  assert.equal(ledger.records.length, 2)
  assert.equal(ledger.records[0].status, 'failed')
  assert.equal(ledger.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.deepEqual(ledger.records[0].claims, firstClaims)
  assert.equal(ledger.records[1].status, 'not_run_runner_terminated')
  assert.equal(ledger.records[1].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.deepEqual(ledger.records[1].claims, pendingClaims)
})

test('formal execution rejects a model substitution before starting any benchmark cell', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const providerEvidence = [
    ['glm5.1flash', 'glm5.1flash'],
    ['mimov2.6pro', 'mimo-v2.6-pro[1m]'],
    ['ds4.1flash', 'ds4.1flash'],
    ['qwen3.8flash', 'qwen3.8flash'],
  ].map(([requestedModelId, observedModelId]) => ({
    requestedModelId,
    observedModelId,
    endpoint: 'https://provider.invalid/messages',
    credentialPresent: true,
    supportsReasoningEffort: 'max',
  }))
  let cellCalls = 0

  await assert.rejects(
    executeFormalBenchmark(plan, { providerEvidence, executeCell: async () => { cellCalls += 1 } }),
    error => error.code === 'MODEL_IDENTITY_MISMATCH' && error.message.includes('mimov2.6pro'),
  )
  assert.equal(cellCalls, 0)
})

test('benchmark report recomputes fixed denominators and keeps usage dimensions separate', () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const records = plan.cells.map((cell, index) => ({
    ...cell,
    status: index === 0 ? 'completed' : 'failed',
    score: index === 0 ? 0.8 : 0,
    wallMs: index === 0 ? 150 : 0,
    context: index === 0 ? {
      rebuildCount: 1,
      rebuildIntervalMs: 1000,
      compressionRatio: 0.4,
      badEvictions: 0,
      staleResidue: 0,
      forgettingEvents: 0,
      postRebuildAttempted: 1,
      postRebuildSucceeded: 1,
    } : null,
    quality: index === 0 ? {
      minorityRetained: 9,
      minorityTotal: 10,
      captureRetained: 8,
      captureTotal: 10,
      constraintsRetained: 5,
      constraintsTotal: 5,
      confirmedRetained: 4,
      confirmedTotal: 4,
      bindingErrors: 0,
      stateRegressions: 0,
      hallucinated: 0,
    } : null,
    usage: index === 0 ? [{
      callKind: 'model',
      callRole: 'model',
      cacheScenario: 'cold',
      requestedModelId: 'glm5.1flash',
      observedModelId: 'glm5.1flash',
      reasoningEffort: 'max',
      inputTokens: 10,
      outputTokens: 4,
      cachedTokens: 2,
      cacheHits: 1,
      costUsd: 0.25,
      modelWaitMs: 100,
      toolMs: 20,
      rebuildMs: 0,
      wallMs: 150,
    }, {
      callKind: 'fusion-engine',
      cacheScenario: 'reusable',
      engine: 'fusion-engine',
      algorithmVersion: 'fusion-4-v1',
      inputDigest: 'a'.repeat(64),
      outputDigest: 'b'.repeat(64),
      callCount: 1,
      costUsd: 0,
      wallMs: 10,
    }] : [],
  }))
  const report = aggregateBenchmarkReport(plan, { records, experimentId: 'experiment-v1' })

  assert.equal(report.failureAccounting.denominator, 700)
  assert.equal(report.failureAccounting.completed, 1)
  assert.equal(report.failureAccounting.unsuccessful, 699)
  assert.equal(report.quality.minority.numerator, 9)
  assert.equal(report.quality.minority.denominator, 10)
  assert.equal(report.quality.minority.rate, 0.9)
  assert.ok(report.quality.minority.interval95.lower < 0.9)
  assert.ok(report.quality.minority.interval95.upper > 0.9)
  assert.equal(report.tokenAndCache.inputTokens, 10)
  assert.equal(report.tokenAndCache.cacheScenarios.cold.inputTokens, 10)
  assert.equal(report.cost.costUsd, 0.25)
  assert.equal(report.time.callWallMs, 160)
  assert.equal(report.time.endToEnd.totalWallMs, 150)
  assert.equal(report.time.endToEnd.p95Ms, 150)
  assert.equal(report.contextEffects.rebuildCount, 1)
  assert.equal(report.contextEffects.evidenceStatus, 'insufficient_evidence')
  assert.deepEqual(report.callAccounting, { modelCalls: 1, fusionEngineCalls: 1 })
  assert.equal(report.gates.regret.status, 'insufficient_evidence')
  assert.equal(report.gates.minority.status, 'insufficient_evidence')
})

test('frozen source validation binds the public repository, exact revision and 20 task directories', async () => {
  const repositoryRoot = resolve('fusion-benchmark-tasks/agent-benchmark-tasks')
  const source = await validateFrozenTaskSource({
    repositoryRoot,
    repositoryUrl: 'https://github.com/changw9813/agent-benchmark-tasks',
    revision: 'fa9da301e493fb88d48c86cb8954ed46d9cd2ffe',
  })

  assert.equal(source.taskCount, 20)
  assert.deepEqual(source.taskIds, source.catalogTaskIds)
  assert.equal(source.revision, 'fa9da301e493fb88d48c86cb8954ed46d9cd2ffe')
  assert.deepEqual((await readdir(repositoryRoot)).includes('tasks'), true)
})
