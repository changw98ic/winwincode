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

async function validateEvidence(repositoryRoot, task, index) {
  if (!Array.isArray(task.evidence) || task.evidence.length === 0) {
    fail('EVIDENCE_MISSING', `tasks[${index}] has no evidence`)
  }
  for (const [evidenceIndex, evidenceValue] of task.evidence.entries()) {
    const evidence = record(evidenceValue, `tasks[${index}].evidence[${evidenceIndex}]`)
    if (!safeEvidencePath(evidence.path) || !SHA256.test(evidence.sha256 ?? '')) {
      fail('EVIDENCE_INVALID', `tasks[${index}].evidence[${evidenceIndex}] identity is invalid`)
    }
    let bytes
    try {
      bytes = await readFile(resolve(repositoryRoot, evidence.path))
    } catch {
      fail('EVIDENCE_MISSING', `tasks[${index}] evidence is unavailable: ${evidence.path}`)
    }
    const actual = createHash('sha256').update(bytes).digest('hex')
    if (actual !== evidence.sha256) {
      fail('EVIDENCE_MISMATCH', `tasks[${index}] evidence digest changed: ${evidence.path}`)
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
  await Promise.all(tasks.map((task, index) => validateEvidence(repositoryRoot, task, index)))

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

function parseArguments(arguments_) {
  const options = {}
  for (let index = 0; index < arguments_.length; index += 1) {
    const name = arguments_[index]
    const value = arguments_[index += 1]
    if (!['--input', '--output', '--repository-root'].includes(name) || value === undefined) {
      fail('ARGUMENT_INVALID', 'usage: evaluate-real-task-benchmark --input FILE [--output FILE] [--repository-root DIR]')
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
    const report = await evaluateRealTaskBenchmark(dataset, resolve(options['repository-root'] ?? '.'))
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
