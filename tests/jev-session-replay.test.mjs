import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'
import test from 'node:test'
import { fileURLToPath } from 'node:url'

import {
  DATASET_KIND,
  JevReplayError,
  MISSING_REAL_SESSION_DATA,
  PHASE1_MODES,
  REPORT_KIND,
  evaluateJevSessionReplay,
} from '../scripts/evaluate-jev-session-replay.mjs'
import {
  BenchmarkError,
  evaluateRealTaskBenchmark,
} from '../scripts/evaluate-real-task-benchmark.mjs'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const skeletonPath = resolve(root, 'tests/fixtures/jev-session-replay/phase1.skeleton.json')
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex')

async function loadSkeleton() {
  return JSON.parse(await readFile(skeletonPath, 'utf8'))
}

function cloneSkeleton(skeleton) {
  return structuredClone(skeleton)
}

function availableRun(mode, overrides = {}) {
  return {
    mode,
    status: 'available',
    provenance: 'fixture',
    sourceCommit: 'fixture-commit-jev04-phase1',
    configurationSha256: 'b'.repeat(64),
    workItemId: 'fixture-wit-jev04-task-001',
    workRunId: `fixture-wrn-${mode}`,
    measurements: {
      tokens: {
        rawInput: 100,
        cachedInput: 0,
        uncachedInput: 100,
        output: 10,
        reasoning: 1,
        jevInput: 0,
      },
      context: { active: 80, removed: 20, archived: 0, bootstrap: 5 },
      agent: { turns: 2, toolCalls: 8, fileReads: 2, greps: 1, tests: 1, repeatToolCalls: 4 },
      perf: { ttftMs: 100, latencyMs: 1000, jevLatencyMs: null, rotations: 0 },
      quality: {
        success: true,
        verificationPassed: true,
        regression: false,
        contextFailure: false,
        forgottenConstraint: false,
        criticalRecall: null,
      },
      cost: [{
        provider: 'fixture-provider',
        inputUsd: 0.1,
        cacheUsd: 0,
        outputUsd: 0,
        jevUsd: 0,
        retryUsd: 0,
      }],
    },
    evidence: [{
      path: 'tests/fixtures/jev-session-replay/fixture-evidence.txt',
      sha256: '7432b5aac43f4ffd8e42d9cb766c0a6d765780708dc2d19ac2e63154a84903a1',
    }],
    ...overrides,
  }
}

function fixtureDataset(runs, extra = {}) {
  return {
    schemaVersion: 1,
    kind: DATASET_KIND,
    track: 'jev-session-replay',
    phase: 'phase1',
    evidenceClass: 'fixture',
    datasetLabel: 'unit-fixture-not-production',
    tasks: [{ id: 'fixture-task-1', runs }],
    ...extra,
  }
}

test('labeled fixture skeleton produces a Phase1 comparative report without production claims', async () => {
  const skeleton = await loadSkeleton()
  const report = await evaluateJevSessionReplay(skeleton, root)

  assert.equal(report.kind, REPORT_KIND)
  assert.equal(report.track, 'jev-session-replay')
  assert.equal(report.phase, 'phase1')
  assert.equal(report.evidenceClass, 'fixture')
  assert.equal(report.productionClaims, false)
  assert.equal(report.separateFromE13ProductionGate, true)
  assert.equal(report.phase1Gate.eligibleAsProductionEvidence, false)
  assert.deepEqual(report.selectedModes, [...PHASE1_MODES])
  assert.deepEqual(report.phase1Modes, ['baseline', 'deterministic-gc', 'gc+jev'])

  assert.equal(report.modes.baseline.metrics.tokens.uncachedInput.total, 1000)
  assert.equal(report.modes['deterministic-gc'].metrics.tokens.uncachedInput.total, 750)
  assert.equal(report.modes['gc+jev'].metrics.tokens.cachedInput.total, 200)

  const vsBaseline = report.comparison.arms['gc+jev'].vsBaseline
  assert.equal(vsBaseline.billedInputTokens.baseline, 1000)
  assert.equal(vsBaseline.billedInputTokens.candidate, 700)
  assert.equal(vsBaseline.billedInputTokens.reduction, 0.3)
  assert.equal(vsBaseline.repeatToolCalls.reduction, 0.4)
  assert.equal(vsBaseline.taskSuccess.delta, 0)
  assert.equal(vsBaseline.criticalRecall.candidate, 0.99)

  const detGc = report.comparison.arms['deterministic-gc'].vsBaseline
  assert.equal(detGc.billedInputTokens.reduction, 0.15)
  assert.equal(report.comparison.arms['gc+jev'].vsDeterministicGc.billedInputTokens.reduction, 0.176471)

  assert.equal(report.phase1Gate.hardGate.checks.criticalRecall.status, 'pass')
  assert.equal(report.phase1Gate.hardGate.checks.taskSuccessNotBelowBaseline.status, 'pass')
  assert.equal(report.phase1Gate.expandedGate.checks.billedInputReduction.status, 'pass')
  assert.equal(report.phase1Gate.expandedGate.checks.repeatToolReduction.status, 'pass')
  assert.equal(report.phase1Gate.expandedGate.checks.jevCostShareOfSavings.status, 'pass')
  assert.equal(report.phase1Gate.status, 'pass')
  assert.equal(report.phase1Gate.productionClaims, false)

  const missing = report.phase1Gate.missingRealSessionData
  assert.ok(missing.includes(MISSING_REAL_SESSION_DATA[0]))
  assert.ok(missing.some(item => item.includes('live BD-01 OpenJev scores')))
  assert.ok(missing.length >= MISSING_REAL_SESSION_DATA.length)
})

test('unlabeled provenance and production-claim fixture labels fail closed', async () => {
  const skeleton = await loadSkeleton()
  const unlabeled = cloneSkeleton(skeleton)
  delete unlabeled.evidenceClass
  await assert.rejects(evaluateJevSessionReplay(unlabeled, root), error => (
    error instanceof JevReplayError && error.code === 'PROVENANCE_UNLABELED'
  ))

  const claimLabel = cloneSkeleton(skeleton)
  claimLabel.datasetLabel = 'jev-phase1-fixture-production-evidence'
  await assert.rejects(evaluateJevSessionReplay(claimLabel, root), error => (
    error instanceof JevReplayError && error.code === 'PROVENANCE_MISMATCH'
  ))

  const missingMarker = cloneSkeleton(skeleton)
  missingMarker.datasetLabel = 'phase1-skeleton-only'
  await assert.rejects(evaluateJevSessionReplay(missingMarker, root), error => (
    error instanceof JevReplayError && error.code === 'PROVENANCE_UNLABELED'
  ))

  const mismatchedRun = cloneSkeleton(skeleton)
  mismatchedRun.tasks[0].runs[0].provenance = 'production'
  await assert.rejects(evaluateJevSessionReplay(mismatchedRun, root), error => (
    error instanceof JevReplayError && error.code === 'PROVENANCE_MISMATCH'
  ))

  const missingRunLabel = cloneSkeleton(skeleton)
  delete missingRunLabel.tasks[0].runs[1].provenance
  await assert.rejects(evaluateJevSessionReplay(missingRunLabel, root), error => (
    error instanceof JevReplayError && error.code === 'PROVENANCE_UNLABELED'
  ))
})

test('production evidenceClass still requires production identities', async () => {
  const production = fixtureDataset([
    availableRun('baseline', {
      provenance: 'production',
      sourceCommit: 'a'.repeat(40),
      workItemId: 'wit_00000000000000000000000001',
      workRunId: 'wrn_00000000000000000000000001',
    }),
    availableRun('deterministic-gc', {
      provenance: 'production',
      sourceCommit: 'a'.repeat(40),
      workItemId: 'wit_00000000000000000000000001',
      workRunId: 'wrn_00000000000000000000000002',
    }),
    availableRun('gc+jev', {
      provenance: 'production',
      sourceCommit: 'a'.repeat(40),
      workItemId: 'wit_00000000000000000000000001',
      workRunId: 'wrn_00000000000000000000000003',
    }),
  ], {
    evidenceClass: 'production',
    datasetLabel: 'jev-phase1-production-session-replay',
  })

  const report = await evaluateJevSessionReplay(production, root)
  assert.equal(report.productionClaims, true)
  assert.equal(report.phase1Gate.productionClaims, true)
  assert.equal(report.phase1Gate.eligibleAsProductionEvidence, false)

  const weakIdentity = cloneSkeleton(production)
  weakIdentity.tasks[0].runs[0].sourceCommit = 'fixture-commit-jev04-phase1'
  await assert.rejects(evaluateJevSessionReplay(weakIdentity, root), error => (
    error instanceof JevReplayError && error.code === 'REPLAY_INVALID'
  ))
})

test('unavailable gc+jev never yields a passing Phase1 gate', async () => {
  const dataset = fixtureDataset([
    availableRun('baseline'),
    availableRun('deterministic-gc', { workRunId: 'fixture-wrn-detgc' }),
    {
      mode: 'gc+jev',
      status: 'unavailable',
      unavailable: { code: 'DEPENDENCY_UNAVAILABLE', detail: 'BD-01 OpenJev adapter not wired into replay' },
    },
  ])

  const report = await evaluateJevSessionReplay(dataset, root)
  assert.equal(report.modes['gc+jev'].unavailableRuns, 1)
  assert.equal(report.phase1Gate.hardGate.checks.criticalRecall.status, 'unknown')
  assert.equal(report.phase1Gate.hardGate.checks.taskSuccessNotBelowBaseline.status, 'unknown')
  assert.equal(report.phase1Gate.expandedGate.checks.billedInputReduction.status, 'unknown')
  assert.equal(report.phase1Gate.status, 'unknown')
  assert.equal(report.phase1Gate.eligibleAsProductionEvidence, false)
  assert.equal(report.comparison.arms['gc+jev'].vsBaseline.billedInputTokens.reduction, null)
})

test('incomplete measurements keep comparative deltas null instead of zero', async () => {
  const skeleton = await loadSkeleton()
  const incomplete = cloneSkeleton(skeleton)
  incomplete.tasks[0].runs[2].measurements.tokens.cachedInput = null
  incomplete.tasks[0].runs[2].measurements.tokens.uncachedInput = null
  incomplete.tasks[0].runs[2].measurements.quality.criticalRecall = null

  const report = await evaluateJevSessionReplay(incomplete, root)
  const vsBaseline = report.comparison.arms['gc+jev'].vsBaseline
  assert.equal(vsBaseline.billedInputTokens.candidate, null)
  assert.equal(vsBaseline.billedInputTokens.reduction, null)
  assert.equal(report.phase1Gate.hardGate.checks.criticalRecall.status, 'unknown')
  assert.equal(report.phase1Gate.expandedGate.checks.billedInputReduction.status, 'unknown')
  assert.equal(report.phase1Gate.status, 'unknown')
})

test('critical recall below 99 percent fails the Phase1 hard gate on labeled fixture data', async () => {
  const skeleton = await loadSkeleton()
  const weakRecall = cloneSkeleton(skeleton)
  weakRecall.tasks[0].runs[2].measurements.quality.criticalRecall = { recalled: 95, total: 100 }

  const report = await evaluateJevSessionReplay(weakRecall, root)
  assert.equal(report.phase1Gate.hardGate.checks.criticalRecall.status, 'fail')
  assert.equal(report.phase1Gate.hardGate.checks.criticalRecall.value, 0.95)
  assert.equal(report.phase1Gate.status, 'fail')
  assert.equal(report.phase1Gate.eligibleAsProductionEvidence, false)
})

test('task success below baseline fails the Phase1 hard gate', async () => {
  const skeleton = await loadSkeleton()
  const regression = cloneSkeleton(skeleton)
  regression.tasks[0].runs[2].measurements.quality.success = false

  const report = await evaluateJevSessionReplay(regression, root)
  assert.equal(report.phase1Gate.hardGate.checks.taskSuccessNotBelowBaseline.status, 'fail')
  assert.equal(report.phase1Gate.status, 'fail')
})

test('E13 production gate still rejects fixture tasks and session-replay kinds', async t => {
  const evidence = await readFile(resolve(root, 'tests/fixtures/jev-session-replay/fixture-evidence.txt'))
  const e13Task = {
    id: 'task-1',
    provenance: 'fixture',
    level: 'simple',
    cases: ['recovery'],
    sourceCommit: 'a'.repeat(40),
    configurationSha256: 'b'.repeat(64),
    workItemId: 'wit_00000000000000000000000001',
    workRunId: 'wrn_00000000000000000000000001',
    result: 'accepted',
    humanActiveMinutes: 1,
    attention: { opportunities: 1, raised: 1, false: 0, missed: 0 },
    verification: { failures: 0, escaped: 0 },
    recovery: { attempted: false, succeeded: false },
    reworkCount: 0,
    cost: { modelUsd: 1, verificationUsd: 0 },
    peer: { used: false },
    evidence: [{
      path: 'tests/fixtures/jev-session-replay/fixture-evidence.txt',
      sha256: sha256(evidence),
    }],
  }
  const tasks = Array.from({ length: 20 }, (_, index) => ({
    ...e13Task,
    id: `task-${index + 1}`,
    workRunId: `wrn_${String(index + 1).padStart(26, '0')}`,
  }))
  await assert.rejects(
    evaluateRealTaskBenchmark({ schemaVersion: 1, kind: 'winwincode.real-task-benchmark.v1', tasks }, root),
    error => error instanceof BenchmarkError && error.code === 'SYNTHETIC_TASK',
  )

  await assert.rejects(
    evaluateRealTaskBenchmark(await loadSkeleton(), root),
    error => error instanceof BenchmarkError
      && error.code === 'BENCHMARK_INVALID'
      && error.message.includes('evaluate-jev-session-replay.mjs'),
  )
})

test('comparative skeleton shape is stable for report consumers', async () => {
  const skeleton = await loadSkeleton()
  const report = await evaluateJevSessionReplay(skeleton, root, ['baseline', 'deterministic-gc', 'gc+jev'])
  assert.deepEqual(Object.keys(report.comparison.arms).toSorted(), ['deterministic-gc', 'gc+jev'])
  for (const arm of Object.values(report.comparison.arms)) {
    const delta = arm.vsBaseline
    for (const key of [
      'billedInputTokens',
      'cacheHitRate',
      'repeatToolCalls',
      'taskSuccess',
      'verificationPassed',
      'criticalRecall',
      'latencyMs',
      'providerCostUsd',
    ]) {
      assert.ok(key in delta, key)
    }
  }
  assert.equal(typeof report.datasetSha256, 'string')
  assert.equal(report.datasetSha256.length, 64)
})
