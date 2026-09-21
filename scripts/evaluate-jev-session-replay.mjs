#!/usr/bin/env node

/**
 * JEV-04 Phase1 session-replay harness.
 *
 * This evaluator is intentionally separate from the E13 production gate in
 * `scripts/evaluate-real-task-benchmark.mjs`. It compares the Phase1 arms
 * Baseline vs Deterministic GC vs GC+Jev on clearly labeled fixture/replay or
 * production session evidence. Fixture and replay datasets must declare an
 * honest `evidenceClass` and must never be presented as E13 production claims.
 */

import { createHash } from 'node:crypto'
import { readFile, writeFile } from 'node:fs/promises'
import { isAbsolute, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

export const DATASET_KIND = 'winwincode.jev-session-replay.v1'
export const REPORT_KIND = 'winwincode.jev-session-replay-report.v1'
export const TRACK = 'jev-session-replay'
export const PHASE1_MODES = Object.freeze(['baseline', 'deterministic-gc', 'gc+jev'])
export const FUTURE_MODES = Object.freeze(['jev+memory', 'jev+rotation'])
export const REPLAY_MODES = Object.freeze([...PHASE1_MODES, ...FUTURE_MODES])
export const REPLAY_MODE_SET = new Set(REPLAY_MODES)
export const EVIDENCE_CLASSES = Object.freeze(['production', 'fixture', 'replay'])
export const EVIDENCE_CLASS_SET = new Set(EVIDENCE_CLASSES)

/** Real session inputs still required before Phase1 can claim production evidence. */
export const MISSING_REAL_SESSION_DATA = Object.freeze([
  'production Codex session rollouts with per-turn raw/cached/uncached/output/reasoning token usage',
  'tool-call traces under each Phase1 arm so tool duplication can be measured, not assumed',
  'critical-constraint inventories per task for Critical Recall scoring',
  'provider cost traces (input/cache/output/jev/retry) for baseline, deterministic-gc, and gc+jev',
  'live OpenJev (BD-01) KEEP/DROP scores wired into the replay loop',
  'decision-engine (BD-02) auditable ContextRetention decisions from real sessions',
  'deterministic GC (BD-03) applied histories replayed side-by-side with baseline',
  'TTFT and end-to-end latency instrumentation for every selected mode',
  'a real multi-task corpus with WorkItem/WorkRun identities large enough for Phase1',
  'independent verification outcomes so Task Success is comparable to baseline',
])

const REPLAY_METRICS = Object.freeze({
  tokens: ['rawInput', 'cachedInput', 'uncachedInput', 'output', 'reasoning', 'jevInput'],
  context: ['active', 'removed', 'archived', 'bootstrap'],
  agent: ['turns', 'toolCalls', 'fileReads', 'greps', 'tests', 'repeatToolCalls'],
  perf: ['ttftMs', 'latencyMs', 'jevLatencyMs', 'rotations'],
})
const REPLAY_COSTS = ['inputUsd', 'cacheUsd', 'outputUsd', 'jevUsd', 'retryUsd']
const REPLAY_QUALITY = ['success', 'verificationPassed', 'regression', 'contextFailure', 'forgottenConstraint']
const SHA256 = /^[0-9a-f]{64}$/u
const PRODUCTION_CLAIM_PATTERN = /production|e13|真实生产|生产证据/iu
const PRODUCTION_NEGATION_PATTERN = /(?:not|non)[-\s_]?(?:production|e13)|(?:not|非)[-\s_]?(?:真实生产|生产证据)/iu
const POSITIVE_LABEL_PATTERN = /fixture|replay/iu

export class JevReplayError extends Error {
  constructor(code, message) {
    super(`${code}: ${message}`)
    this.name = 'JevReplayError'
    this.code = code
  }
}

function fail(code, message) {
  throw new JevReplayError(code, message)
}

function record(value, label) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    fail('REPLAY_INVALID', `${label} must be an object`)
  }
  return value
}

function finiteNonNegative(value, label) {
  if (!Number.isFinite(value) || value < 0) fail('REPLAY_INVALID', `${label} must be non-negative`)
  return value
}

function nullableNonNegative(value, label, integer = false) {
  if (value === null) return null
  finiteNonNegative(value, label)
  if (integer && !Number.isSafeInteger(value)) fail('REPLAY_INVALID', `${label} must be an integer or null`)
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
      fail('REPLAY_INVALID', `${label}.quality.${field} must be a boolean or null`)
    }
  }
  if (quality.criticalRecall !== null) {
    const recall = record(quality.criticalRecall, `${label}.quality.criticalRecall`)
    finiteNonNegative(recall.recalled, `${label}.quality.criticalRecall.recalled`)
    finiteNonNegative(recall.total, `${label}.quality.criticalRecall.total`)
    if (!Number.isSafeInteger(recall.recalled) || !Number.isSafeInteger(recall.total)
      || recall.total === 0 || recall.recalled > recall.total) {
      fail('REPLAY_INVALID', `${label}.quality.criticalRecall is inconsistent`)
    }
  }
  if (!Array.isArray(measurements.cost) || measurements.cost.length === 0) {
    fail('REPLAY_INVALID', `${label}.cost must contain at least one provider`)
  }
  const providers = new Set()
  for (const [index, costValue] of measurements.cost.entries()) {
    const cost = record(costValue, `${label}.cost[${index}]`)
    if (typeof cost.provider !== 'string' || cost.provider.length === 0 || providers.has(cost.provider)) {
      fail('REPLAY_INVALID', `${label}.cost[${index}].provider is invalid or duplicated`)
    }
    providers.add(cost.provider)
    for (const field of REPLAY_COSTS) nullableNonNegative(cost[field], `${label}.cost[${index}].${field}`)
  }
  return measurements
}

function validateRunIdentity(run, label, evidenceClass) {
  if (typeof run.provenance !== 'string' || run.provenance.length === 0) {
    fail('PROVENANCE_UNLABELED', `${label}.provenance is required`)
  }
  if (!EVIDENCE_CLASS_SET.has(run.provenance)) {
    fail('PROVENANCE_UNLABELED', `${label}.provenance must be production, fixture, or replay`)
  }
  if (run.provenance !== evidenceClass) {
    fail('PROVENANCE_MISMATCH', `${label}.provenance must match dataset evidenceClass ${evidenceClass}`)
  }
  if (!SHA256.test(run.configurationSha256 ?? '')) {
    fail('REPLAY_INVALID', `${label} lacks a configuration digest`)
  }
  if (evidenceClass === 'production') {
    if (!/^[0-9a-f]{40}$/u.test(run.sourceCommit ?? '')) {
      fail('REPLAY_INVALID', `${label} lacks an exact production source commit`)
    }
    if (!/^wit_[0-9A-HJKMNP-TV-Z]{26}$/u.test(run.workItemId ?? '')
      || !/^wrn_[0-9A-HJKMNP-TV-Z]{26}$/u.test(run.workRunId ?? '')) {
      fail('REPLAY_INVALID', `${label} lacks a canonical WorkItem/WorkRun identity`)
    }
    return
  }
  if (typeof run.sourceCommit !== 'string' || run.sourceCommit.length === 0) {
    fail('REPLAY_INVALID', `${label}.sourceCommit is required for labeled ${evidenceClass} evidence`)
  }
  if (typeof run.workItemId !== 'string' || run.workItemId.length === 0
    || typeof run.workRunId !== 'string' || run.workRunId.length === 0) {
    fail('REPLAY_INVALID', `${label} needs labeled WorkItem/WorkRun ids for ${evidenceClass} evidence`)
  }
}

async function validateReplayRun(runValue, label, repositoryRoot, evidenceClass) {
  const run = record(runValue, label)
  if (!REPLAY_MODE_SET.has(run.mode)) fail('REPLAY_INVALID', `${label}.mode is invalid`)
  if (run.status === 'unavailable') {
    const unavailable = record(run.unavailable, `${label}.unavailable`)
    if (typeof unavailable.code !== 'string' || unavailable.code.length === 0) {
      fail('REPLAY_INVALID', `${label}.unavailable.code is required`)
    }
    if (typeof unavailable.detail !== 'string' || unavailable.detail.length === 0) {
      fail('REPLAY_INVALID', `${label}.unavailable.detail is required`)
    }
    if (run.measurements !== undefined || run.evidence !== undefined || run.provenance !== undefined) {
      fail('REPLAY_INVALID', `${label} cannot attach results while unavailable`)
    }
    return run
  }
  if (run.status !== 'available') fail('REPLAY_INVALID', `${label}.status is invalid`)
  validateRunIdentity(run, label, evidenceClass)
  validateReplayMeasurements(run.measurements, `${label}.measurements`)
  await validateEvidence(repositoryRoot, run, label)
  return run
}

function selectedReplayModes(value) {
  const modes = value ?? PHASE1_MODES
  if (!Array.isArray(modes) || modes.length === 0 || new Set(modes).size !== modes.length) {
    fail('REPLAY_INVALID', 'selected modes must be a non-empty unique list')
  }
  for (const mode of modes) {
    if (!REPLAY_MODE_SET.has(mode)) fail('REPLAY_INVALID', `unknown replay mode: ${mode}`)
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
    unavailable: unavailable.map(run => ({ taskId: run.taskId, mode: run.mode, ...run.unavailable })),
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

function delta(candidate, baseline) {
  return baseline === null || candidate === null ? null : Number((candidate - baseline).toFixed(6))
}

function billedInputTokens(modeReport) {
  const cached = modeReport?.metrics.tokens.cachedInput.total ?? null
  const uncached = modeReport?.metrics.tokens.uncachedInput.total ?? null
  return cached === null || uncached === null ? null : cached + uncached
}

function cacheHitRate(modeReport) {
  const cached = modeReport?.metrics.tokens.cachedInput.total ?? null
  const uncached = modeReport?.metrics.tokens.uncachedInput.total ?? null
  if (cached === null || uncached === null) return null
  return ratio(cached, cached + uncached)
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

function armDelta(modeReports, runsByMode, candidateMode, baselineMode) {
  const candidate = modeReports[candidateMode]
  const baseline = modeReports[baselineMode]
  const candidateRuns = runsByMode[candidateMode] ?? []
  const baselineRuns = runsByMode[baselineMode] ?? []
  const candidateCost = completeCost(candidateRuns, REPLAY_COSTS)
  const baselineCost = completeCost(baselineRuns, REPLAY_COSTS)
  return Object.freeze({
    candidate: candidateMode,
    baseline: baselineMode,
    billedInputTokens: Object.freeze({
      candidate: billedInputTokens(candidate),
      baseline: billedInputTokens(baseline),
      reduction: reduction(billedInputTokens(baseline), billedInputTokens(candidate)),
    }),
    cacheHitRate: Object.freeze({
      candidate: cacheHitRate(candidate),
      baseline: cacheHitRate(baseline),
    }),
    repeatToolCalls: Object.freeze({
      candidate: completeMetric(modeReports, candidateMode, 'agent', 'repeatToolCalls'),
      baseline: completeMetric(modeReports, baselineMode, 'agent', 'repeatToolCalls'),
      reduction: reduction(
        completeMetric(modeReports, baselineMode, 'agent', 'repeatToolCalls'),
        completeMetric(modeReports, candidateMode, 'agent', 'repeatToolCalls'),
      ),
    }),
    taskSuccess: Object.freeze({
      candidate: candidate?.metrics.quality.success.rate ?? null,
      baseline: baseline?.metrics.quality.success.rate ?? null,
      delta: delta(candidate?.metrics.quality.success.rate ?? null, baseline?.metrics.quality.success.rate ?? null),
    }),
    verificationPassed: Object.freeze({
      candidate: candidate?.metrics.quality.verificationPassed.rate ?? null,
      baseline: baseline?.metrics.quality.verificationPassed.rate ?? null,
      delta: delta(
        candidate?.metrics.quality.verificationPassed.rate ?? null,
        baseline?.metrics.quality.verificationPassed.rate ?? null,
      ),
    }),
    criticalRecall: Object.freeze({
      candidate: candidate?.metrics.quality.criticalRecall.rate ?? null,
      baseline: baseline?.metrics.quality.criticalRecall.rate ?? null,
    }),
    latencyMs: Object.freeze({
      candidate: completeMetric(modeReports, candidateMode, 'perf', 'latencyMs'),
      baseline: completeMetric(modeReports, baselineMode, 'perf', 'latencyMs'),
    }),
    providerCostUsd: Object.freeze({
      candidate: candidateCost,
      baseline: baselineCost,
      savings: baselineCost === null || candidateCost === null
        ? null
        : Number((baselineCost - candidateCost).toFixed(6)),
    }),
  })
}

export function buildComparativeSkeleton(modeReports, runsByMode, selectedModes) {
  const arms = {}
  if (selectedModes.includes('deterministic-gc')) {
    arms['deterministic-gc'] = Object.freeze({
      vsBaseline: armDelta(modeReports, runsByMode, 'deterministic-gc', 'baseline'),
    })
  }
  if (selectedModes.includes('gc+jev')) {
    const gcJev = { vsBaseline: armDelta(modeReports, runsByMode, 'gc+jev', 'baseline') }
    if (selectedModes.includes('deterministic-gc')) {
      gcJev.vsDeterministicGc = armDelta(modeReports, runsByMode, 'gc+jev', 'deterministic-gc')
    }
    arms['gc+jev'] = Object.freeze(gcJev)
  }
  for (const mode of selectedModes) {
    if (mode in arms || mode === 'baseline') continue
    arms[mode] = Object.freeze({
      vsBaseline: armDelta(modeReports, runsByMode, mode, 'baseline'),
    })
  }
  return Object.freeze({
    phase: 'phase1',
    baselineMode: 'baseline',
    arms: Object.freeze(arms),
  })
}

function phase1Gate(modeReports, runsByMode, selectedModes, evidenceClass, datasetMissing) {
  const baselineSuccess = modeReports.baseline?.metrics.quality.success.rate ?? null
  const gcJevSuccess = modeReports['gc+jev']?.metrics.quality.success.rate ?? null
  const gcJevRecall = modeReports['gc+jev']?.metrics.quality.criticalRecall.rate ?? null
  const detGcRecall = modeReports['deterministic-gc']?.metrics.quality.criticalRecall.rate ?? null

  const baselineCost = completeCost(runsByMode.baseline ?? [], ['inputUsd', 'cacheUsd', 'outputUsd', 'retryUsd'])
  const gcJevCostWithoutJev = completeCost(runsByMode['gc+jev'] ?? [], ['inputUsd', 'cacheUsd', 'outputUsd', 'retryUsd'])
  const gcJevEvaluationCost = completeCost(runsByMode['gc+jev'] ?? [], ['jevUsd'])
  const savings = baselineCost === null || gcJevCostWithoutJev === null
    ? null
    : Number((baselineCost - gcJevCostWithoutJev).toFixed(6))
  const jevCostShare = savings === null || savings <= 0 || gcJevEvaluationCost === null
    ? null
    : ratio(gcJevEvaluationCost, savings)

  const phase1Checks = {
    criticalRecall: gateCheck(
      gcJevRecall,
      0.99,
      value => value >= 0.99,
    ),
    deterministicGcCriticalRecall: gateCheck(
      detGcRecall,
      0.99,
      value => value >= 0.99,
    ),
    taskSuccessNotBelowBaseline: gateCheck(
      baselineSuccess === null || gcJevSuccess === null ? null : gcJevSuccess - baselineSuccess,
      0,
      value => value >= 0,
    ),
  }
  Object.freeze(phase1Checks)

  const expandedChecks = {
    selectedModesAvailable: gateCheck(
      selectedModes.every(mode => modeReports[mode]?.unavailableRuns === 0) ? 1 : null,
      1,
      value => value === 1,
    ),
    billedInputReduction: gateCheck(
      reduction(billedInputTokens(modeReports.baseline), billedInputTokens(modeReports['gc+jev'])),
      0.15,
      value => value >= 0.15,
    ),
    repeatToolReduction: gateCheck(
      reduction(
        completeMetric(modeReports, 'baseline', 'agent', 'repeatToolCalls'),
        completeMetric(modeReports, 'gc+jev', 'agent', 'repeatToolCalls'),
      ),
      0.15,
      value => value >= 0.15,
    ),
    jevCostShareOfSavings: Object.freeze({
      ...gateCheck(jevCostShare, 0.1, value => value <= 0.1),
      status: savings !== null && savings <= 0 ? 'fail' : gateCheck(jevCostShare, 0.1, value => value <= 0.1).status,
      savingsUsd: savings,
      jevCostUsd: gcJevEvaluationCost,
    }),
    verificationNotBelowBaseline: gateCheck(
      delta(
        modeReports['gc+jev']?.metrics.quality.verificationPassed.rate ?? null,
        modeReports.baseline?.metrics.quality.verificationPassed.rate ?? null,
      ),
      0,
      value => value >= 0,
    ),
  }
  if (selectedModes.includes('jev+memory')) {
    expandedChecks.memoryRegression = gateCheck(
      modeReports['jev+memory']?.metrics.quality.regression.rate ?? null,
      0.02,
      value => value < 0.02,
    )
  }
  Object.freeze(expandedChecks)

  const statuses = [...Object.values(phase1Checks), ...Object.values(expandedChecks)].map(check => check.status)
  const status = statuses.includes('fail') ? 'fail' : statuses.includes('unknown') ? 'unknown' : 'pass'
  const productionClaims = evidenceClass === 'production'
  const eligibleAsProductionEvidence = productionClaims
    && status === 'pass'
    && Object.values(phase1Checks).every(check => check.status === 'pass')
  const missing = [...new Set([...MISSING_REAL_SESSION_DATA, ...datasetMissing])]

  return Object.freeze({
    status,
    productionClaims,
    eligibleAsProductionEvidence,
    separateFromE13ProductionGate: true,
    hardGate: Object.freeze({
      rule: 'Critical Recall >= 99% AND Task Success >= baseline before expanding Jev Runtime',
      checks: phase1Checks,
    }),
    expandedGate: Object.freeze({
      rule: 'billed input -15%, repeated tool -15%, Jev cost <= 10% of savings, success/verification not below baseline',
      checks: expandedChecks,
    }),
    missingRealSessionData: Object.freeze(missing),
  })
}

function validateDatasetHeader(dataset) {
  if (dataset.schemaVersion !== 1 || dataset.kind !== DATASET_KIND) {
    fail('REPLAY_INVALID', 'dataset identity must be winwincode.jev-session-replay.v1 schemaVersion 1')
  }
  if (dataset.track !== TRACK) {
    fail('REPLAY_INVALID', `dataset.track must be ${TRACK}`)
  }
  if (dataset.phase !== 'phase1') {
    fail('REPLAY_INVALID', 'dataset.phase must be phase1 for this harness')
  }
  if (!EVIDENCE_CLASS_SET.has(dataset.evidenceClass)) {
    fail('PROVENANCE_UNLABELED', 'dataset.evidenceClass must be production, fixture, or replay')
  }
  if (typeof dataset.datasetLabel !== 'string' || dataset.datasetLabel.trim().length === 0) {
    fail('PROVENANCE_UNLABELED', 'dataset.datasetLabel is required so fixture/replay data stays labeled')
  }
  if (dataset.evidenceClass !== 'production') {
    if (!POSITIVE_LABEL_PATTERN.test(dataset.datasetLabel)) {
      fail('PROVENANCE_UNLABELED', 'non-production datasetLabel must include fixture or replay')
    }
    if (PRODUCTION_CLAIM_PATTERN.test(dataset.datasetLabel)
      && !PRODUCTION_NEGATION_PATTERN.test(dataset.datasetLabel)) {
      fail('PROVENANCE_MISMATCH', 'non-production datasetLabel must not claim production or E13 evidence')
    }
  }
  if (!Array.isArray(dataset.tasks) || dataset.tasks.length === 0) {
    fail('TASK_COUNT_INSUFFICIENT', 'at least one replay task is required')
  }
  if (dataset.missingRealData !== undefined && !Array.isArray(dataset.missingRealData)) {
    fail('REPLAY_INVALID', 'dataset.missingRealData must be an array of strings')
  }
  for (const [index, item] of (dataset.missingRealData ?? []).entries()) {
    if (typeof item !== 'string' || item.length === 0) {
      fail('REPLAY_INVALID', `dataset.missingRealData[${index}] must be a non-empty string`)
    }
  }
}

export async function evaluateJevSessionReplay(datasetValue, repositoryRoot, requestedModes) {
  const dataset = record(datasetValue, 'dataset')
  validateDatasetHeader(dataset)
  const evidenceClass = dataset.evidenceClass
  const modes = selectedReplayModes(requestedModes)
  const taskIds = new Set()
  const runsByMode = Object.fromEntries(modes.map(mode => [mode, []]))
  const runIds = new Set()
  for (const [taskIndex, taskValue] of dataset.tasks.entries()) {
    const task = record(taskValue, `tasks[${taskIndex}]`)
    if (typeof task.id !== 'string' || task.id.length === 0 || taskIds.has(task.id)) {
      fail('REPLAY_INVALID', `tasks[${taskIndex}].id is invalid or duplicated`)
    }
    taskIds.add(task.id)
    if (!Array.isArray(task.runs)) fail('REPLAY_INVALID', `tasks[${taskIndex}].runs must be an array`)
    for (const mode of modes) {
      const matches = task.runs.filter(run => run?.mode === mode)
      if (matches.length !== 1) fail('COVERAGE_MISSING', `tasks[${taskIndex}] must contain one ${mode} run`)
      const run = await validateReplayRun(
        matches[0],
        `tasks[${taskIndex}].runs.${mode}`,
        repositoryRoot,
        evidenceClass,
      )
      if (run.status === 'available') {
        if (runIds.has(run.workRunId)) fail('REPLAY_INVALID', `duplicate WorkRun id: ${run.workRunId}`)
        runIds.add(run.workRunId)
      }
      runsByMode[mode].push(Object.freeze({ ...run, taskId: task.id }))
    }
    const workItems = new Set(modes.flatMap(mode => runsByMode[mode]
      .filter(run => run.taskId === task.id && run.status === 'available')
      .map(run => run.workItemId)))
    if (workItems.size > 1) {
      fail('REPLAY_INVALID', `tasks[${taskIndex}] modes do not replay the same WorkItem`)
    }
  }
  const modeReports = Object.fromEntries(modes.map(mode => [mode, summarizeMode(runsByMode[mode])]))
  const datasetMissing = (dataset.missingRealData ?? []).map(String)
  return Object.freeze({
    schemaVersion: 1,
    kind: REPORT_KIND,
    track: TRACK,
    phase: 'phase1',
    evidenceClass,
    productionClaims: evidenceClass === 'production',
    separateFromE13ProductionGate: true,
    datasetLabel: dataset.datasetLabel,
    datasetSha256: createHash('sha256').update(JSON.stringify(dataset)).digest('hex'),
    taskCount: dataset.tasks.length,
    selectedModes: modes,
    phase1Modes: PHASE1_MODES,
    modes: Object.freeze(modeReports),
    comparison: buildComparativeSkeleton(modeReports, runsByMode, modes),
    phase1Gate: phase1Gate(modeReports, runsByMode, modes, evidenceClass, datasetMissing),
  })
}

function parseArguments(arguments_) {
  const options = {}
  for (let index = 0; index < arguments_.length; index += 1) {
    const name = arguments_[index]
    const value = arguments_[index += 1]
    if (!['--input', '--output', '--repository-root', '--modes'].includes(name) || value === undefined) {
      fail(
        'ARGUMENT_INVALID',
        'usage: evaluate-jev-session-replay --input FILE [--output FILE] [--repository-root DIR] [--modes MODE,...]',
      )
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
    const report = await evaluateJevSessionReplay(dataset, repositoryRoot, options.modes?.split(','))
    const bytes = `${JSON.stringify(report, null, 2)}\n`
    if (options.output) await writeFile(resolve(options.output), bytes, { flag: 'wx' })
    else process.stdout.write(bytes)
    return 0
  } catch (error) {
    const code = error instanceof JevReplayError ? error.code : 'REPLAY_UNEXPECTED'
    process.stderr.write(`${code}: ${error.message}\n`)
    return 1
  }
}

const direct = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)
if (direct) process.exitCode = await runCli()
