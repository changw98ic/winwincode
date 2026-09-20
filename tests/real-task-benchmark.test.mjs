import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import test from 'node:test'

import {
  BenchmarkError,
  evaluateRealTaskBenchmark,
  evaluateSessionReplayBenchmark,
} from '../scripts/evaluate-real-task-benchmark.mjs'

const sha256 = bytes => createHash('sha256').update(bytes).digest('hex')
const canonicalId = (prefix, index) => `${prefix}_${index.toString(10).padStart(26, '0')}`

async function fixture(t) {
  const root = await mkdtemp(resolve(tmpdir(), 'wwc-real-benchmark-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const evidence = Buffer.from('real-task-fixture-evidence\n')
  await writeFile(resolve(root, 'evidence.json'), evidence)
  const tasks = Array.from({ length: 20 }, (_, index) => ({
    id: `task-${index + 1}`,
    provenance: 'production',
    level: ['simple', 'medium', 'complex'][index % 3],
    cases: index === 0 ? ['recovery'] : index === 1 ? ['regression'] : index === 2 ? ['collaboration'] : [],
    sourceCommit: 'a'.repeat(40),
    configurationSha256: 'b'.repeat(64),
    workItemId: canonicalId('wit', index + 1),
    workRunId: canonicalId('wrn', index + 1),
    result: index === 19 ? 'failed' : 'accepted',
    humanActiveMinutes: 10,
    attention: { opportunities: 1, raised: 1, false: index === 3 ? 1 : 0, missed: index === 4 ? 1 : 0 },
    verification: { failures: index === 19 ? 1 : 0, escaped: 0 },
    recovery: { attempted: index === 0, succeeded: index === 0 },
    reworkCount: index === 5 ? 1 : 0,
    cost: { modelUsd: index === 6 ? null : 1, verificationUsd: 0.25 },
    peer: { used: index === 2, baselineHumanMinutes: index === 2 ? 20 : null, assistedHumanMinutes: index === 2 ? 10 : null },
    evidence: [{ path: 'evidence.json', sha256: sha256(evidence) }],
  }))
  return { root, dataset: { schemaVersion: 1, kind: 'winwincode.real-task-benchmark.v1', tasks } }
}

test('20 evidence-bound tasks produce all nine product metrics', async t => {
  const { root, dataset } = await fixture(t)
  const report = await evaluateRealTaskBenchmark(dataset, root)
  assert.equal(report.taskCount, 20)
  assert.equal(report.metrics.acceptedTaskRate, 0.95)
  assert.equal(report.metrics.humanMinutesPerTask, 10)
  assert.deepEqual(report.metrics.recoverySuccess, { rate: 1, successes: 1, attempts: 1 })
  assert.equal(report.metrics.falseAttention.rate, 0.05)
  assert.equal(report.metrics.missedAttention.rate, 0.05)
  assert.equal(report.metrics.verificationFailureEscape.rate, 0)
  assert.equal(report.metrics.reworkRate, 0.05)
  assert.deepEqual(report.metrics.cost, {
    modelUsd: 19,
    verificationUsd: 5,
    knownModelTasks: 19,
    knownVerificationTasks: 20,
    unknownModelTasks: 1,
    unknownVerificationTasks: 0,
  })
  assert.deepEqual(report.metrics.peerCollaborationBenefit, {
    rate: 0.5,
    pairedTasks: 1,
    baselineHumanMinutes: 20,
    assistedHumanMinutes: 10,
  })
})

test('synthetic tasks, missing coverage, and evidence drift fail closed', async t => {
  const { root, dataset } = await fixture(t)
  dataset.tasks[0].provenance = 'synthetic'
  await assert.rejects(evaluateRealTaskBenchmark(dataset, root), error => (
    error instanceof BenchmarkError && error.code === 'SYNTHETIC_TASK'
  ))

  dataset.tasks[0].provenance = 'production'
  dataset.tasks[0].evidence[0].sha256 = 'c'.repeat(64)
  await assert.rejects(evaluateRealTaskBenchmark(dataset, root), error => (
    error instanceof BenchmarkError && error.code === 'EVIDENCE_MISMATCH'
  ))
})

test('session replay report is reproducible and never passes unavailable modes', async t => {
  const root = await mkdtemp(resolve(tmpdir(), 'wwc-session-replay-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const evidence = Buffer.from('production-session-replay-evidence\n')
  await writeFile(resolve(root, 'replay.json'), evidence)
  const modes = ['baseline', 'jev', 'jev+memory', 'jev+rotation']
  const runs = modes.map((mode, index) => ({
    mode,
    status: 'available',
    provenance: 'production',
    sourceCommit: 'a'.repeat(40),
    configurationSha256: 'b'.repeat(64),
    workItemId: canonicalId('wit', 100),
    workRunId: canonicalId('wrn', 100 + index),
    measurements: {
      tokens: {
        rawInput: mode === 'baseline' ? 100 : 80,
        cachedInput: 0,
        uncachedInput: mode === 'baseline' ? 100 : 80,
        output: 20,
        reasoning: 5,
        jevInput: mode === 'baseline' ? 0 : 10,
      },
      context: { active: 80, removed: 20, archived: 0, bootstrap: 10 },
      agent: { turns: 2, toolCalls: 10, fileReads: 2, greps: 1, tests: 1, repeatToolCalls: mode === 'baseline' ? 10 : 8 },
      perf: { ttftMs: 100, latencyMs: 1_000, jevLatencyMs: mode === 'baseline' ? null : 10, rotations: 0 },
      quality: {
        success: true,
        verificationPassed: true,
        regression: false,
        contextFailure: false,
        forgottenConstraint: false,
        criticalRecall: mode === 'baseline' ? null : { recalled: 99, total: 100 },
      },
      cost: [{
        provider: 'fixture-provider',
        inputUsd: mode === 'baseline' ? 1 : 0.8,
        cacheUsd: 0,
        outputUsd: 0,
        jevUsd: mode === 'baseline' ? 0 : 0.02,
        retryUsd: 0,
      }],
    },
    evidence: [{ path: 'replay.json', sha256: sha256(evidence) }],
  }))
  const dataset = {
    schemaVersion: 1,
    kind: 'winwincode.session-replay-benchmark.v1',
    tasks: [{ id: 'production-task-1', runs }],
  }

  const report = await evaluateSessionReplayBenchmark(dataset, root)
  assert.equal(report.modes.baseline.metrics.tokens.uncachedInput.total, 100)
  assert.equal(report.modes.jev.metrics.cost['fixture-provider'].jevUsd.total, 0.02)
  assert.equal(report.gate.status, 'pass')
  assert.deepEqual(Object.fromEntries(Object.entries(report.gate.checks).map(([name, check]) => [name, check.status])), {
    selectedModesAvailable: 'pass',
    billedInputReduction: 'pass',
    repeatToolReduction: 'pass',
    jevCostShareOfSavings: 'pass',
    taskSuccessNotBelowBaseline: 'pass',
    verificationNotBelowBaseline: 'pass',
    memoryRegression: 'pass',
    criticalRecall: 'pass',
  })

  dataset.tasks[0].runs[1] = {
    mode: 'jev',
    status: 'unavailable',
    unavailable: { code: 'DEPENDENCY_UNAVAILABLE', detail: 'BD-01 is not ready' },
  }
  const unavailableReport = await evaluateSessionReplayBenchmark(dataset, root)
  assert.equal(unavailableReport.modes.jev.unavailableRuns, 1)
  assert.equal(unavailableReport.gate.status, 'unknown')
  assert.equal(unavailableReport.gate.checks.billedInputReduction.status, 'unknown')
})
