#!/usr/bin/env node

import { createHash } from 'node:crypto'
import { readFile, writeFile } from 'node:fs/promises'
import { isAbsolute, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const LEVELS = new Set(['simple', 'medium', 'complex'])
const RESULTS = new Set(['accepted', 'rejected', 'failed', 'timeout'])
const REQUIRED_CASES = ['recovery', 'regression', 'collaboration']
const SHA256 = /^[0-9a-f]{64}$/u
const COMMIT = /^[0-9a-f]{40}$/u
const WORK_ITEM = /^wit_[0-9A-HJKMNP-TV-Z]{26}$/u
const WORK_RUN = /^wrn_[0-9A-HJKMNP-TV-Z]{26}$/u
const REPLAY_MODES = ['baseline', 'jev', 'jev+memory', 'jev+rotation']
const REPLAY_MODE_SET = new Set(REPLAY_MODES)
const REPLAY_METRICS = Object.freeze({
  tokens: ['rawInput', 'cachedInput', 'uncachedInput', 'output', 'reasoning', 'jevInput'],
  context: ['active', 'removed', 'archived', 'bootstrap'],
  agent: ['turns', 'toolCalls', 'fileReads', 'greps', 'tests', 'repeatToolCalls'],
  perf: ['ttftMs', 'latencyMs', 'jevLatencyMs', 'rotations'],
})
const REPLAY_COSTS = ['inputUsd', 'cacheUsd', 'outputUsd', 'jevUsd', 'retryUsd']
const REPLAY_QUALITY = ['success', 'verificationPassed', 'regression', 'contextFailure', 'forgottenConstraint']

export class BenchmarkError extends Error {
  constructor(code, message) {
    super(`${code}: ${message}`)
    this.name = 'BenchmarkError'
    this.code = code
  }
}

function fail(code, message) {
  throw new BenchmarkError(code, message)
}

function record(value, label) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    fail('BENCHMARK_INVALID', `${label} must be an object`)
  }
  return value
}

function finiteNonNegative(value, label) {
  if (!Number.isFinite(value) || value < 0) fail('BENCHMARK_INVALID', `${label} must be non-negative`)
  return value
}

function ratio(numerator, denominator) {
  return denominator === 0 ? null : Number((numerator / denominator).toFixed(6))
}

function safeEvidencePath(path) {
  return typeof path === 'string'
    && path.length > 0
    && !isAbsolute(path)
    && path !== '..'
    && !path.startsWith('../')
    && !path.includes('/../')
    && !path.includes('\\')
}

async function validateEvidence(repositoryRoot, subject, label) {
  if (!Array.isArray(subject.evidence) || subject.evidence.length === 0) {
    fail('EVIDENCE_MISSING', `${label} has no evidence`)
  }
  for (const [evidenceIndex, evidenceValue] of subject.evidence.entries()) {
    const evidence = record(evidenceValue, `${label}.evidence[${evidenceIndex}]`)
    if (!safeEvidencePath(evidence.path) || !SHA256.test(evidence.sha256 ?? '')) {
      fail('EVIDENCE_INVALID', `${label}.evidence[${evidenceIndex}] identity is invalid`)
    }
    let bytes
    try {
      bytes = await readFile(resolve(repositoryRoot, evidence.path))
    } catch {
      fail('EVIDENCE_MISSING', `${label} evidence is unavailable: ${evidence.path}`)
    }
    const actual = createHash('sha256').update(bytes).digest('hex')
    if (actual !== evidence.sha256) {
      fail('EVIDENCE_MISMATCH', `${label} evidence digest changed: ${evidence.path}`)
    }
  }
}

function validateTask(taskValue, index) {
  const task = record(taskValue, `tasks[${index}]`)
  if (task.provenance !== 'production') fail('SYNTHETIC_TASK', `tasks[${index}] is not production evidence`)
  if (!LEVELS.has(task.level)) fail('BENCHMARK_INVALID', `tasks[${index}].level is invalid`)
  if (!RESULTS.has(task.result)) fail('BENCHMARK_INVALID', `tasks[${index}].result is invalid`)
  if (!COMMIT.test(task.sourceCommit ?? '') || !SHA256.test(task.configurationSha256 ?? '')) {
    fail('BENCHMARK_INVALID', `tasks[${index}] lacks an exact code/configuration identity`)
  }
  if (!WORK_ITEM.test(task.workItemId ?? '') || !WORK_RUN.test(task.workRunId ?? '')) {
    fail('BENCHMARK_INVALID', `tasks[${index}] lacks a canonical WorkItem/WorkRun identity`)
  }
  if (!Array.isArray(task.cases) || task.cases.some(value => !REQUIRED_CASES.includes(value))) {
    fail('BENCHMARK_INVALID', `tasks[${index}].cases is invalid`)
  }
  finiteNonNegative(task.humanActiveMinutes, `tasks[${index}].humanActiveMinutes`)
  finiteNonNegative(task.reworkCount, `tasks[${index}].reworkCount`)
  if (!Number.isSafeInteger(task.reworkCount)) fail('BENCHMARK_INVALID', `tasks[${index}].reworkCount must be an integer`)
  for (const name of ['attention', 'verification', 'recovery', 'cost', 'peer']) record(task[name], `tasks[${index}].${name}`)
  finiteNonNegative(task.attention.opportunities, `tasks[${index}].attention.opportunities`)
  finiteNonNegative(task.attention.raised, `tasks[${index}].attention.raised`)
  finiteNonNegative(task.attention.false, `tasks[${index}].attention.false`)
  finiteNonNegative(task.attention.missed, `tasks[${index}].attention.missed`)
  if (task.attention.false > task.attention.raised || task.attention.missed > task.attention.opportunities) {
    fail('BENCHMARK_INVALID', `tasks[${index}].attention counters are inconsistent`)
  }
  for (const field of ['failures', 'escaped']) finiteNonNegative(task.verification[field], `tasks[${index}].verification.${field}`)
  if (task.verification.escaped > task.verification.failures) {
    fail('BENCHMARK_INVALID', `tasks[${index}].verification counters are inconsistent`)
  }
  if (typeof task.recovery.attempted !== 'boolean' || typeof task.recovery.succeeded !== 'boolean') {
    fail('BENCHMARK_INVALID', `tasks[${index}].recovery flags are invalid`)
  }
  if (!task.recovery.attempted && task.recovery.succeeded) {
    fail('BENCHMARK_INVALID', `tasks[${index}] cannot recover without an attempt`)
  }
  for (const field of ['modelUsd', 'verificationUsd']) {
    if (task.cost[field] !== null) finiteNonNegative(task.cost[field], `tasks[${index}].cost.${field}`)
  }
  if (typeof task.peer.used !== 'boolean') fail('BENCHMARK_INVALID', `tasks[${index}].peer.used is invalid`)
  if (task.peer.used) {
    finiteNonNegative(task.peer.baselineHumanMinutes, `tasks[${index}].peer.baselineHumanMinutes`)
    finiteNonNegative(task.peer.assistedHumanMinutes, `tasks[${index}].peer.assistedHumanMinutes`)
    if (task.peer.baselineHumanMinutes === 0) fail('BENCHMARK_INVALID', `tasks[${index}] peer baseline must be positive`)
  }
  return task
}

export async function evaluateRealTaskBenchmark(datasetValue, repositoryRoot) {
  const dataset = record(datasetValue, 'dataset')
  if (dataset.schemaVersion !== 1 || dataset.kind !== 'winwincode.real-task-benchmark.v1') {
    fail('BENCHMARK_INVALID', 'dataset identity is invalid')
  }
  if (!Array.isArray(dataset.tasks) || dataset.tasks.length < 20) {
    fail('TASK_COUNT_INSUFFICIENT', 'at least 20 real tasks are required')
  }
  const tasks = dataset.tasks.map(validateTask)
  if (new Set(tasks.map(task => task.id)).size !== tasks.length) fail('BENCHMARK_INVALID', 'task ids must be unique')
  if (new Set(tasks.map(task => task.workRunId)).size !== tasks.length) fail('BENCHMARK_INVALID', 'WorkRun ids must be unique')
  for (const level of LEVELS) {
    if (!tasks.some(task => task.level === level)) fail('COVERAGE_MISSING', `task level is missing: ${level}`)
  }
  for (const requiredCase of REQUIRED_CASES) {
    if (!tasks.some(task => task.cases.includes(requiredCase))) fail('COVERAGE_MISSING', `case is missing: ${requiredCase}`)
  }
  if (!tasks.some(task => task.result === 'accepted') || !tasks.some(task => task.result !== 'accepted')) {
    fail('COVERAGE_MISSING', 'both successful and unsuccessful results are required')
  }
  await Promise.all(tasks.map((task, index) => validateEvidence(repositoryRoot, task, `tasks[${index}]`)))

  const total = tasks.length
  const accepted = tasks.filter(task => task.result === 'accepted').length
  const humanMinutes = tasks.reduce((sum, task) => sum + task.humanActiveMinutes, 0)
  const recoveryAttempts = tasks.filter(task => task.recovery.attempted).length
  const recoverySuccesses = tasks.filter(task => task.recovery.attempted && task.recovery.succeeded).length
  const attention = tasks.reduce((sum, task) => ({
    opportunities: sum.opportunities + task.attention.opportunities,
    raised: sum.raised + task.attention.raised,
    false: sum.false + task.attention.false,
    missed: sum.missed + task.attention.missed,
  }), { opportunities: 0, raised: 0, false: 0, missed: 0 })
  const verification = tasks.reduce((sum, task) => ({
    failures: sum.failures + task.verification.failures,
    escaped: sum.escaped + task.verification.escaped,
  }), { failures: 0, escaped: 0 })
  const peerTasks = tasks.filter(task => task.peer.used)
  const peerBaseline = peerTasks.reduce((sum, task) => sum + task.peer.baselineHumanMinutes, 0)
  const peerAssisted = peerTasks.reduce((sum, task) => sum + task.peer.assistedHumanMinutes, 0)
  const knownModelCosts = tasks.flatMap(task => task.cost.modelUsd === null ? [] : [task.cost.modelUsd])
  const knownVerificationCosts = tasks.flatMap(task => task.cost.verificationUsd === null ? [] : [task.cost.verificationUsd])

  return Object.freeze({
    schemaVersion: 1,
    kind: 'winwincode.real-task-benchmark-report.v1',
    datasetSha256: createHash('sha256').update(JSON.stringify(dataset)).digest('hex'),
    taskCount: total,
    coverage: Object.freeze({
      levels: Object.fromEntries([...LEVELS].map(level => [level, tasks.filter(task => task.level === level).length])),
      cases: Object.fromEntries(REQUIRED_CASES.map(name => [name, tasks.filter(task => task.cases.includes(name)).length])),
      accepted,
      unsuccessful: total - accepted,
    }),
    metrics: Object.freeze({
      acceptedTaskRate: ratio(accepted, total),
      humanMinutesPerTask: ratio(humanMinutes, total),
      recoverySuccess: Object.freeze({ rate: ratio(recoverySuccesses, recoveryAttempts), successes: recoverySuccesses, attempts: recoveryAttempts }),
      falseAttention: Object.freeze({ rate: ratio(attention.false, attention.raised), false: attention.false, raised: attention.raised }),
      missedAttention: Object.freeze({ rate: ratio(attention.missed, attention.opportunities), missed: attention.missed, opportunities: attention.opportunities }),
      verificationFailureEscape: Object.freeze({ rate: ratio(verification.escaped, verification.failures), escaped: verification.escaped, failures: verification.failures }),
      reworkRate: ratio(tasks.filter(task => task.reworkCount > 0).length, total),
      cost: Object.freeze({
        modelUsd: Number(knownModelCosts.reduce((sum, value) => sum + value, 0).toFixed(6)),
        verificationUsd: Number(knownVerificationCosts.reduce((sum, value) => sum + value, 0).toFixed(6)),
        knownModelTasks: knownModelCosts.length,
        knownVerificationTasks: knownVerificationCosts.length,
        unknownModelTasks: total - knownModelCosts.length,
        unknownVerificationTasks: total - knownVerificationCosts.length,
      }),
      peerCollaborationBenefit: Object.freeze({
        rate: peerTasks.length === 0 ? null : ratio(peerBaseline - peerAssisted, peerBaseline),
        pairedTasks: peerTasks.length,
        baselineHumanMinutes: peerBaseline,
        assistedHumanMinutes: peerAssisted,
      }),
    }),
  })
}

function nullableNonNegative(value, label, integer = false) {
  if (value === null) return null
  finiteNonNegative(value, label)
  if (integer && !Number.isSafeInteger(value)) fail('BENCHMARK_INVALID', `${label} must be an integer or null`)
  return value
}

function validateReplayMeasurements(value, label) {
  const measurements = record(value, label)
  for (const [group, fields] of Object.entries(REPLAY_METRICS)) {
    const metrics = record(measurements[group], `${label}.${group}`)
    for (const field of fields) {
      nullableNonNegative(metrics[field], `${label}.${group}.${field}`, group !== 'perf')
    }
  }
  const quality = record(measurements.quality, `${label}.quality`)
  for (const field of REPLAY_QUALITY) {
    if (quality[field] !== null && typeof quality[field] !== 'boolean') {
      fail('BENCHMARK_INVALID', `${label}.quality.${field} must be a boolean or null`)
    }
  }
  if (quality.criticalRecall !== null) {
    const recall = record(quality.criticalRecall, `${label}.quality.criticalRecall`)
    finiteNonNegative(recall.recalled, `${label}.quality.criticalRecall.recalled`)
    finiteNonNegative(recall.total, `${label}.quality.criticalRecall.total`)
    if (!Number.isSafeInteger(recall.recalled) || !Number.isSafeInteger(recall.total)
      || recall.total === 0 || recall.recalled > recall.total) {
      fail('BENCHMARK_INVALID', `${label}.quality.criticalRecall is inconsistent`)
    }
  }
  if (!Array.isArray(measurements.cost) || measurements.cost.length === 0) {
    fail('BENCHMARK_INVALID', `${label}.cost must contain at least one provider`)
  }
  const providers = new Set()
  for (const [index, costValue] of measurements.cost.entries()) {
    const cost = record(costValue, `${label}.cost[${index}]`)
    if (typeof cost.provider !== 'string' || cost.provider.length === 0 || providers.has(cost.provider)) {
      fail('BENCHMARK_INVALID', `${label}.cost[${index}].provider is invalid or duplicated`)
    }
    providers.add(cost.provider)
    for (const field of REPLAY_COSTS) nullableNonNegative(cost[field], `${label}.cost[${index}].${field}`)
  }
  return measurements
}

async function validateReplayRun(runValue, label, repositoryRoot) {
  const run = record(runValue, label)
  if (!REPLAY_MODE_SET.has(run.mode)) fail('BENCHMARK_INVALID', `${label}.mode is invalid`)
  if (run.status === 'unavailable') {
    const unavailable = record(run.unavailable, `${label}.unavailable`)
    if (typeof unavailable.code !== 'string' || unavailable.code.length === 0) {
      fail('BENCHMARK_INVALID', `${label}.unavailable.code is required`)
    }
    if (typeof unavailable.detail !== 'string' || unavailable.detail.length === 0) {
      fail('BENCHMARK_INVALID', `${label}.unavailable.detail is required`)
    }
    if (run.measurements !== undefined || run.evidence !== undefined) {
      fail('SYNTHETIC_TASK', `${label} cannot attach results while unavailable`)
    }
    return run
  }
  if (run.status !== 'available') fail('BENCHMARK_INVALID', `${label}.status is invalid`)
  if (run.provenance !== 'production') fail('SYNTHETIC_TASK', `${label} is not production evidence`)
  if (!COMMIT.test(run.sourceCommit ?? '') || !SHA256.test(run.configurationSha256 ?? '')) {
    fail('BENCHMARK_INVALID', `${label} lacks an exact code/configuration identity`)
  }
  if (!WORK_ITEM.test(run.workItemId ?? '') || !WORK_RUN.test(run.workRunId ?? '')) {
    fail('BENCHMARK_INVALID', `${label} lacks a canonical WorkItem/WorkRun identity`)
  }
  validateReplayMeasurements(run.measurements, `${label}.measurements`)
  await validateEvidence(repositoryRoot, run, label)
  return run
}

function selectedReplayModes(value) {
  const modes = value ?? REPLAY_MODES
  if (!Array.isArray(modes) || modes.length === 0 || new Set(modes).size !== modes.length) {
    fail('BENCHMARK_INVALID', 'selected modes must be a non-empty unique list')
  }
  for (const mode of modes) {
    if (!REPLAY_MODE_SET.has(mode)) fail('BENCHMARK_INVALID', `unknown replay mode: ${mode}`)
  }
  return modes
}

function numericSummary(runs, unavailableRuns, read) {
  const values = runs.map(read)
  const known = values.filter(value => value !== null)
  const unknownRuns = unavailableRuns + values.length - known.length
  const measuredTotal = Number(known.reduce((sum, value) => sum + value, 0).toFixed(6))
  return Object.freeze({
    total: unknownRuns === 0 && known.length > 0 ? measuredTotal : null,
    measuredTotal,
    knownRuns: known.length,
    unknownRuns,
  })
}

function qualitySummary(runs, unavailableRuns, field) {
  const values = runs.map(run => run.measurements.quality[field])
  const known = values.filter(value => value !== null)
  const trueCount = known.filter(Boolean).length
  const unknownRuns = unavailableRuns + values.length - known.length
  return Object.freeze({
    rate: unknownRuns === 0 && known.length > 0 ? ratio(trueCount, known.length) : null,
    trueCount,
    knownRuns: known.length,
    unknownRuns,
  })
}

function summarizeMode(runs) {
  const available = runs.filter(run => run.status === 'available')
  const unavailable = runs.filter(run => run.status === 'unavailable')
  const metrics = {}
  for (const [group, fields] of Object.entries(REPLAY_METRICS)) {
    metrics[group] = Object.fromEntries(fields.map(field => [
      field,
      numericSummary(available, unavailable.length, run => run.measurements[group][field]),
    ]))
  }
  const quality = Object.fromEntries(REPLAY_QUALITY.map(field => [
    field,
    qualitySummary(available, unavailable.length, field),
  ]))
  const recalls = available.map(run => run.measurements.quality.criticalRecall)
  const knownRecalls = recalls.filter(value => value !== null)
  const recalled = knownRecalls.reduce((sum, value) => sum + value.recalled, 0)
  const total = knownRecalls.reduce((sum, value) => sum + value.total, 0)
  const recallUnknown = unavailable.length + recalls.length - knownRecalls.length
  quality.criticalRecall = Object.freeze({
    rate: recallUnknown === 0 && total > 0 ? ratio(recalled, total) : null,
    recalled,
    total,
    knownRuns: knownRecalls.length,
    unknownRuns: recallUnknown,
  })
  const providers = [...new Set(available.flatMap(run => run.measurements.cost.map(cost => cost.provider)))].toSorted()
  const cost = Object.fromEntries(providers.map(provider => [provider, Object.fromEntries(REPLAY_COSTS.map(field => [
    field,
    numericSummary(available, unavailable.length, run => (
      run.measurements.cost.find(value => value.provider === provider)?.[field] ?? null
    )),
  ]))]))
  return Object.freeze({
    availableRuns: available.length,
    unavailableRuns: unavailable.length,
    unavailable: unavailable.map(run => ({ taskId: run.taskId, ...run.unavailable })),
    metrics: Object.freeze({ ...metrics, quality: Object.freeze(quality), cost: Object.freeze(cost) }),
  })
}

function gateCheck(value, threshold, passes) {
  return Object.freeze({
    status: value === null ? 'unknown' : passes(value) ? 'pass' : 'fail',
    value,
    threshold,
  })
}

function completeMetric(report, mode, group, field) {
  return report[mode]?.metrics[group][field].total ?? null
}

function reduction(baseline, candidate) {
  return baseline === null || candidate === null || baseline === 0 ? null : ratio(baseline - candidate, baseline)
}

function completeCost(runs, fields) {
  if (runs.length === 0) return null
  let total = 0
  for (const run of runs) {
    if (run.status !== 'available') return null
    for (const cost of run.measurements.cost) {
      for (const field of fields) {
        if (cost[field] === null) return null
        total += cost[field]
      }
    }
  }
  return Number(total.toFixed(6))
}

function replayGate(modeReports, runsByMode, selectedModes) {
  const baselineInput = completeMetric(modeReports, 'baseline', 'tokens', 'cachedInput')
  const baselineUncached = completeMetric(modeReports, 'baseline', 'tokens', 'uncachedInput')
  const jevInput = completeMetric(modeReports, 'jev', 'tokens', 'cachedInput')
  const jevUncached = completeMetric(modeReports, 'jev', 'tokens', 'uncachedInput')
  const billedReduction = reduction(
    baselineInput === null || baselineUncached === null ? null : baselineInput + baselineUncached,
    jevInput === null || jevUncached === null ? null : jevInput + jevUncached,
  )
  const baseCost = completeCost(runsByMode.baseline ?? [], ['inputUsd', 'cacheUsd', 'outputUsd', 'retryUsd'])
  const jevCostWithoutJev = completeCost(runsByMode.jev ?? [], ['inputUsd', 'cacheUsd', 'outputUsd', 'retryUsd'])
  const jevEvaluationCost = completeCost(runsByMode.jev ?? [], ['jevUsd'])
  const savings = baseCost === null || jevCostWithoutJev === null ? null : Number((baseCost - jevCostWithoutJev).toFixed(6))
  const jevCostShare = savings === null || savings <= 0 || jevEvaluationCost === null
    ? null
    : ratio(jevEvaluationCost, savings)
  const selectedJevModes = selectedModes.filter(mode => mode !== 'baseline')
  const recallReports = selectedJevModes.map(mode => modeReports[mode]?.metrics.quality.criticalRecall)
  const recallUnknown = recallReports.length === 0 || recallReports.some(value => value === undefined || value.unknownRuns > 0)
  const recalled = recallReports.reduce((sum, value) => sum + (value?.recalled ?? 0), 0)
  const recallTotal = recallReports.reduce((sum, value) => sum + (value?.total ?? 0), 0)
  const criticalRecall = recallUnknown || recallTotal === 0 ? null : ratio(recalled, recallTotal)
  const baselineSuccess = modeReports.baseline?.metrics.quality.success.rate ?? null
  const jevSuccess = modeReports.jev?.metrics.quality.success.rate ?? null
  const baselineVerification = modeReports.baseline?.metrics.quality.verificationPassed.rate ?? null
  const jevVerification = modeReports.jev?.metrics.quality.verificationPassed.rate ?? null
  const allAvailable = selectedModes.every(mode => modeReports[mode]?.unavailableRuns === 0)
  const jevCostCheck = gateCheck(jevCostShare, 0.1, value => value <= 0.1)
  const checks = {
    selectedModesAvailable: gateCheck(allAvailable ? 1 : null, 1, value => value === 1),
    billedInputReduction: gateCheck(billedReduction, 0.15, value => value >= 0.15),
    repeatToolReduction: gateCheck(reduction(
      completeMetric(modeReports, 'baseline', 'agent', 'repeatToolCalls'),
      completeMetric(modeReports, 'jev', 'agent', 'repeatToolCalls'),
    ), 0.15, value => value >= 0.15),
    jevCostShareOfSavings: Object.freeze({
      ...jevCostCheck,
      status: savings !== null && savings <= 0 ? 'fail' : jevCostCheck.status,
      savingsUsd: savings,
      jevCostUsd: jevEvaluationCost,
    }),
    taskSuccessNotBelowBaseline: gateCheck(
      baselineSuccess === null || jevSuccess === null ? null : jevSuccess - baselineSuccess,
      0,
      value => value >= 0,
    ),
    verificationNotBelowBaseline: gateCheck(
      baselineVerification === null || jevVerification === null ? null : jevVerification - baselineVerification,
      0,
      value => value >= 0,
    ),
    criticalRecall: gateCheck(criticalRecall, 0.99, value => value >= 0.99),
  }
  if (selectedModes.includes('jev+memory')) {
    checks.memoryRegression = gateCheck(modeReports['jev+memory'].metrics.quality.regression.rate, 0.02, value => value < 0.02)
  }
  Object.freeze(checks)
  const statuses = Object.values(checks).map(check => check.status)
  return Object.freeze({
    status: statuses.includes('fail') ? 'fail' : statuses.includes('unknown') ? 'unknown' : 'pass',
    checks,
  })
}

export async function evaluateSessionReplayBenchmark(datasetValue, repositoryRoot, requestedModes) {
  const dataset = record(datasetValue, 'dataset')
  if (dataset.schemaVersion !== 1 || dataset.kind !== 'winwincode.session-replay-benchmark.v1') {
    fail('BENCHMARK_INVALID', 'session replay dataset identity is invalid')
  }
  if (!Array.isArray(dataset.tasks) || dataset.tasks.length === 0) {
    fail('TASK_COUNT_INSUFFICIENT', 'at least one replay task is required')
  }
  const modes = selectedReplayModes(requestedModes)
  const taskIds = new Set()
  const runsByMode = Object.fromEntries(modes.map(mode => [mode, []]))
  const runIds = new Set()
  for (const [taskIndex, taskValue] of dataset.tasks.entries()) {
    const task = record(taskValue, `tasks[${taskIndex}]`)
    if (typeof task.id !== 'string' || task.id.length === 0 || taskIds.has(task.id)) {
      fail('BENCHMARK_INVALID', `tasks[${taskIndex}].id is invalid or duplicated`)
    }
    taskIds.add(task.id)
    if (!Array.isArray(task.runs)) fail('BENCHMARK_INVALID', `tasks[${taskIndex}].runs must be an array`)
    for (const mode of modes) {
      const matches = task.runs.filter(run => run?.mode === mode)
      if (matches.length !== 1) fail('COVERAGE_MISSING', `tasks[${taskIndex}] must contain one ${mode} run`)
      const run = await validateReplayRun(matches[0], `tasks[${taskIndex}].runs.${mode}`, repositoryRoot)
      if (run.status === 'available') {
        if (runIds.has(run.workRunId)) fail('BENCHMARK_INVALID', `duplicate WorkRun id: ${run.workRunId}`)
        runIds.add(run.workRunId)
      }
      runsByMode[mode].push(Object.freeze({ ...run, taskId: task.id }))
    }
    const workItems = new Set(modes.flatMap(mode => runsByMode[mode]
      .filter(run => run.taskId === task.id && run.status === 'available')
      .map(run => run.workItemId)))
    if (workItems.size > 1) fail('BENCHMARK_INVALID', `tasks[${taskIndex}] modes do not replay the same WorkItem`)
  }
  const modeReports = Object.fromEntries(modes.map(mode => [mode, summarizeMode(runsByMode[mode])]))
  return Object.freeze({
    schemaVersion: 1,
    kind: 'winwincode.session-replay-benchmark-report.v1',
    datasetSha256: createHash('sha256').update(JSON.stringify(dataset)).digest('hex'),
    taskCount: dataset.tasks.length,
    selectedModes: modes,
    modes: Object.freeze(modeReports),
    gate: replayGate(modeReports, runsByMode, modes),
  })
}

function parseArguments(arguments_) {
  const options = {}
  for (let index = 0; index < arguments_.length; index += 1) {
    const name = arguments_[index]
    const value = arguments_[index += 1]
    if (!['--input', '--output', '--repository-root', '--modes'].includes(name) || value === undefined) {
      fail('ARGUMENT_INVALID', 'usage: evaluate-real-task-benchmark --input FILE [--output FILE] [--repository-root DIR] [--modes MODE,...]')
    }
    options[name.slice(2)] = value
  }
  if (!options.input) fail('ARGUMENT_INVALID', '--input is required')
  return options
}

export async function runCli(arguments_ = process.argv.slice(2)) {
  try {
    const options = parseArguments(arguments_)
    const input = resolve(options.input)
    const dataset = JSON.parse(await readFile(input, 'utf8'))
    const repositoryRoot = resolve(options['repository-root'] ?? '.')
    const report = dataset.kind === 'winwincode.session-replay-benchmark.v1'
      ? await evaluateSessionReplayBenchmark(dataset, repositoryRoot, options.modes?.split(','))
      : await evaluateRealTaskBenchmark(dataset, repositoryRoot)
    const bytes = `${JSON.stringify(report, null, 2)}\n`
    if (options.output) await writeFile(resolve(options.output), bytes, { flag: 'wx' })
    else process.stdout.write(bytes)
    return 0
  } catch (error) {
    const code = error instanceof BenchmarkError ? error.code : 'BENCHMARK_UNEXPECTED'
    process.stderr.write(`${code}: ${error.message}\n`)
    return 1
  }
}

const direct = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)
if (direct) process.exitCode = await runCli()
