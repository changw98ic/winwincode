#!/usr/bin/env node

import { createHash } from 'node:crypto'
import { execFile } from 'node:child_process'
import { readFile, readdir } from 'node:fs/promises'
import { resolve } from 'node:path'
import { promisify } from 'node:util'

const runFile = promisify(execFile)

const PROVIDERS = Object.freeze(['glm5.1flash', 'mimov2.6pro', 'ds4.1flash', 'qwen3.8flash'])
const SHA256_PATTERN = /^[0-9a-f]{64}$/u

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

function normalizeRepositoryUrl(value) {
  return String(value).replace(/\.git$/u, '').replace(/\/$/u, '')
}

async function gitOutput(repositoryRoot, ...arguments_) {
  try {
    const { stdout } = await runFile('git', ['-C', repositoryRoot, ...arguments_], { encoding: 'utf8' })
    return stdout.trim()
  } catch {
    fail('SOURCE_IDENTITY_MISSING', `cannot read Git identity for ${repositoryRoot}`)
  }
}

export async function validateFrozenTaskSource({ repositoryRoot, repositoryUrl, revision }) {
  const root = resolve(repositoryRoot)
  const [actualUrl, actualRevision] = await Promise.all([
    gitOutput(root, 'remote', 'get-url', 'origin'),
    gitOutput(root, 'rev-parse', 'HEAD'),
  ])
  if (normalizeRepositoryUrl(actualUrl) !== normalizeRepositoryUrl(repositoryUrl)) {
    fail('SOURCE_REPOSITORY_MISMATCH', `${actualUrl} is not ${repositoryUrl}`)
  }
  if (actualRevision !== revision) fail('SOURCE_REVISION_MISMATCH', `${actualRevision} is not ${revision}`)

  const catalog = JSON.parse(await readFile(resolve(root, 'catalog.json'), 'utf8'))
  if (!Array.isArray(catalog) || catalog.length !== 20) fail('TASK_COUNT_INVALID', 'catalog must contain exactly 20 tasks')
  const catalogTaskIds = catalog.map(task => task.id)
  if (new Set(catalogTaskIds).size !== 20) fail('TASK_IDENTITY_INVALID', 'catalog task ids must be unique')
  const entries = await readdir(resolve(root, 'tasks'), { withFileTypes: true })
  const directoryTaskIds = entries.filter(entry => entry.isDirectory()).map(entry => entry.name).sort()
  if (directoryTaskIds.length !== 20 || directoryTaskIds.some((taskId, index) => taskId !== [...catalogTaskIds].sort()[index])) {
    fail('TASK_IDENTITY_INVALID', 'task directories must exactly match catalog.json')
  }
  const digest = createHash('sha256')
  const digestPaths = ['catalog.json', ...catalogTaskIds.flatMap(taskId => [
    `tasks/${taskId}/examples.json`,
    `tasks/${taskId}/task.json`,
    `tasks/${taskId}/task.md`,
  ])].sort()
  for (const path of digestPaths) {
    const bytes = await readFile(resolve(root, path))
    digest.update(`${path}\0${createHash('sha256').update(bytes).digest('hex')}\n`)
  }
  return Object.freeze({
    repositoryUrl: normalizeRepositoryUrl(repositoryUrl),
    revision,
    sourceDigest: digest.digest('hex'),
    taskCount: directoryTaskIds.length,
    taskIds: Object.freeze(catalogTaskIds),
    catalogTaskIds: Object.freeze(catalogTaskIds),
  })
}

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  if (value === null || typeof value !== 'object') return JSON.stringify(value)
  return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(',')}}`
}

const TOOL_METADATA_KEYS = new Set([
  'requestid',
  'timestamp',
  'timestampms',
  'progress',
  'progressmetadata',
])

function canonicalToolArgs(value) {
  if (Array.isArray(value)) return value.map(canonicalToolArgs)
  if (value === null || typeof value !== 'object') return value
  return Object.fromEntries(Object.entries(value)
    .filter(([key]) => !TOOL_METADATA_KEYS.has(key.toLowerCase().replace(/[^a-z0-9]/gu, '')))
    .map(([key, nested]) => [key, canonicalToolArgs(nested)]))
}

export function normalizeToolRequestIdentity(request) {
  if (!request || typeof request !== 'object' || Array.isArray(request)) {
    throw new TypeError('tool request must be an object')
  }
  const { tool, target } = request
  const args = request.args ?? request.params
  const requestedContentDigest = request.requestedContentDigest ?? request.contentDigest
  if (typeof tool !== 'string' || tool.length === 0 || typeof target !== 'string' || target.length === 0) {
    throw new TypeError('tool request requires tool and target identities')
  }
  if (args === undefined || typeof requestedContentDigest !== 'string'
    || !/^[0-9a-f]{64}$/u.test(requestedContentDigest)) {
    throw new TypeError('tool request requires canonical args and a SHA-256 requested-content digest')
  }
  return createHash('sha256')
    .update(canonicalJson({ tool, target, args: canonicalToolArgs(args), requestedContentDigest }))
    .digest('hex')
}

const LEGACY_GATE_TERMINATION = Symbol('tool-repeat-termination')

export function createToolRequestGuard() {
  const occurrences = new Map()
  let termination = null
  return Object.freeze({
    admit(request) {
      if (termination) return termination
      const identity = normalizeToolRequestIdentity(request)
      const occurrence = (occurrences.get(identity) ?? 0) + 1
      occurrences.set(identity, occurrence)
      if (occurrence >= 6) {
        termination = Object.freeze({
          status: 'terminated',
          reason: 'STUCK_TOOL_REPEAT_LIMIT',
          identity,
          occurrence: 6,
        })
        return termination
      }
      return Object.freeze({ status: 'admitted', identity, occurrence })
    },
  })
}

export async function executeToolRequest(request, gate, executor) {
  if (gate && typeof gate.admit === 'function') {
    const admission = gate.admit(request)
    if (admission.status === 'terminated') return admission
    return executor(request, { identity: admission.identity, occurrence: admission.occurrence })
  }
  if (!(gate instanceof Map)) throw new TypeError('tool gate must be a Map or tool request guard')
  const termination = gate.get(LEGACY_GATE_TERMINATION)
  if (termination) return termination
  const identity = normalizeToolRequestIdentity(request)
  const occurrence = (gate.get(identity) ?? 0) + 1
  gate.set(identity, occurrence)
  if (occurrence >= 6) {
    const nextTermination = Object.freeze({
      status: 'terminated',
      reason: 'STUCK_TOOL_REPEAT_LIMIT',
      identity,
      occurrence: 6,
    })
    gate.set(LEGACY_GATE_TERMINATION, nextTermination)
    return nextTermination
  }
  return executor(request, { identity, occurrence })
}

function taskIdentities(taskIds) {
  if (!Array.isArray(taskIds) || taskIds.length !== 20 || new Set(taskIds).size !== 20) {
    throw new TypeError('taskIds must contain exactly 20 unique task identities')
  }
  return [...taskIds]
}

export function buildBenchmarkPlan({ taskIds }) {
  const tasks = taskIdentities(taskIds)
  const configurations = [
    { configurationId: 'main-A', track: 'main', fusion: false, jev: false },
    { configurationId: 'main-B', track: 'main', fusion: false, jev: true },
    { configurationId: 'main-C', track: 'main', fusion: true, jev: false },
    { configurationId: 'main-D', track: 'main', fusion: true, jev: true },
    { configurationId: 'jev-context-only', track: 'jev-ablation', fusion: false, jev: true },
    { configurationId: 'jev-judge-only', track: 'jev-ablation', fusion: false, jev: true },
    { configurationId: 'jev-full', track: 'jev-ablation', fusion: false, jev: true },
  ]
  const cells = []
  for (const configuration of configurations) {
    for (const taskId of tasks) {
      for (const comparison of [...PROVIDERS, 'fusion-4']) {
        cells.push({
          runId: `${configuration.configurationId}:${taskId}:${comparison}`,
          taskId,
          ...configuration,
          comparison,
          fusionKind: comparison === 'fusion-4' ? 'independent-aggregate' : null,
          reasoningEffort: 'max',
          budgetLimits: null,
          status: 'planned',
        })
      }
    }
  }
  return Object.freeze({ schemaVersion: 1, kind: 'winwincode.real-task-benchmark-plan.v1', cells: Object.freeze(cells) })
}

export async function executeBenchmarkCell(cell, adapter, { toolGate = createToolRequestGuard() } = {}) {
  if (cell.budgetLimits !== null) throw new TypeError('formal benchmark cells cannot set budget limits')
  if (cell.reasoningEffort !== 'max') throw new TypeError('formal benchmark model calls require max reasoning effort')

  const requestFor = (provider, callId) => ({
    runId: cell.runId,
    taskId: cell.taskId,
    callId,
    provider,
    comparison: cell.comparison,
    fusionKind: cell.fusionKind,
    reasoningEffort: cell.reasoningEffort,
    budgetLimits: cell.budgetLimits,
  })
  const runner = Object.freeze({
    requestTool: async (request, executor) => {
      const result = await executeToolRequest(request, toolGate, executor)
      if (result.status === 'terminated') {
        const error = new Error(result.reason)
        error.code = result.reason
        error.termination = result
        throw error
      }
      return result
    },
  })

  if (cell.fusionKind === 'independent-aggregate') {
    const members = []
    for (const provider of PROVIDERS) {
      members.push(await adapter.runModel(requestFor(provider, `${cell.runId}:member:${provider}`), runner))
    }
    const inputDigest = createHash('sha256').update(canonicalJson({ runId: cell.runId, taskId: cell.taskId, members })).digest('hex')
    const aggregate = await adapter.aggregate({
      runId: cell.runId,
      taskId: cell.taskId,
      callId: `${cell.runId}:aggregation`,
      comparison: cell.comparison,
      engine: 'fusion-engine',
      algorithmVersion: 'fusion-4-v1',
      inputDigest,
      members,
    }, runner)
    return {
      members,
      aggregate,
      aggregateReceipt: {
        callKind: 'fusion-engine',
        engine: 'fusion-engine',
        algorithmVersion: 'fusion-4-v1',
        inputDigest,
        outputDigest: createHash('sha256').update(canonicalJson(aggregate)).digest('hex'),
        callCount: 1,
      },
    }
  }

  const model = await adapter.runModel(requestFor(cell.comparison, `${cell.runId}:model`), runner)
  return { model }
}

export async function runBenchmarkPlan(plan, {
  executeCell,
  createToolGate = () => createToolRequestGuard(),
  onRecord = () => {},
}) {
  const records = plan.cells.map(cell => ({ ...cell, status: 'planned', termination: null }))
  for (const [index, cell] of plan.cells.entries()) {
    try {
      const result = await executeCell(cell, { toolGate: createToolGate(cell) })
      const termination = result.termination ?? null
      records[index] = {
        ...records[index],
        ...result,
        status: termination ? 'failed' : result.status ?? 'completed',
        verdict: termination ? null : result.verdict ?? null,
        score: termination ? 0 : result.score,
        termination,
      }
      await onRecord(records[index], index)
      if (termination) {
        for (let pending = index + 1; pending < records.length; pending += 1) {
          records[pending] = {
            ...records[pending],
            status: 'not_run_runner_terminated',
            verdict: null,
            score: 0,
            termination,
          }
          await onRecord(records[pending], pending)
        }
        break
      }
    } catch (error) {
      const termination = error && typeof error === 'object' ? error.termination : null
      records[index] = {
        ...records[index],
        ...(error && typeof error === 'object' && error.claims !== undefined ? { claims: error.claims } : {}),
        status: 'failed',
        verdict: null,
        score: 0,
        termination: termination ?? null,
        failure: {
          code: termination?.reason ?? (error && typeof error === 'object' && error.code) ?? 'RUNNER_UNEXPECTED',
          message: error instanceof Error ? error.message : String(error),
        },
      }
      await onRecord(records[index], index)
      if (termination) {
        for (let pending = index + 1; pending < records.length; pending += 1) {
          records[pending] = {
            ...records[pending],
            status: 'not_run_runner_terminated',
            verdict: null,
            score: 0,
            termination,
          }
          await onRecord(records[pending], pending)
        }
        break
      }
    }
  }
  return Object.freeze({
    schemaVersion: 1,
    kind: 'winwincode.real-task-benchmark-ledger.v1',
    denominator: records.length,
    records: Object.freeze(records.map(record => Object.freeze(record))),
  })
}

export async function executeFormalBenchmark(plan, { providerEvidence, ...options }) {
  if (!Array.isArray(providerEvidence)) fail('MODEL_IDENTITY_MISSING', 'providerEvidence is required before execution')
  const byProvider = new Map(providerEvidence.map(evidence => [evidence.requestedModelId, evidence]))
  if (byProvider.size !== PROVIDERS.length || PROVIDERS.some(provider => !byProvider.has(provider))) {
    fail('MODEL_IDENTITY_MISSING', 'all four frozen provider identities require preflight evidence')
  }
  for (const provider of PROVIDERS) {
    const evidence = byProvider.get(provider)
    if (evidence.observedModelId !== provider) {
      fail('MODEL_IDENTITY_MISMATCH', `${provider} was resolved to ${String(evidence.observedModelId)}`)
    }
    let endpoint
    try {
      endpoint = new URL(evidence.endpoint)
    } catch {
      fail('MODEL_IDENTITY_MISSING', `${provider} endpoint is invalid`)
    }
    if (endpoint.protocol !== 'https:' || evidence.credentialPresent !== true) {
      fail('MODEL_IDENTITY_MISSING', `${provider} requires an HTTPS endpoint and present credential`)
    }
    if (evidence.supportsReasoningEffort !== 'max') {
      fail('REASONING_EFFORT_UNSUPPORTED', `${provider} cannot prove max reasoning effort`)
    }
  }
  return runBenchmarkPlan(plan, options)
}

function metricRatio(records, numeratorField, denominatorField) {
  let numerator = 0
  let denominator = 0
  for (const record of records) {
    if (!record.quality) continue
    numerator += record.quality[numeratorField]
    denominator += record.quality[denominatorField]
  }
  if (denominator === 0) {
    return Object.freeze({
      numerator,
      denominator,
      rate: 'insufficient_evidence',
      interval95: null,
    })
  }

  const proportion = numerator / denominator
  const z = 1.959963984540054
  const zSquared = z * z
  const center = (numerator + zSquared / 2) / (denominator + zSquared)
  const halfWidth = z * Math.sqrt(
    (proportion * (1 - proportion) + zSquared / (4 * denominator)) / denominator,
  ) / (1 + zSquared / denominator)
  return Object.freeze({
    numerator,
    denominator,
    rate: Number(proportion.toFixed(6)),
    interval95: Object.freeze({
      lower: Number(Math.max(0, center - halfWidth).toFixed(6)),
      upper: Number(Math.min(1, center + halfWidth).toFixed(6)),
    }),
  })
}

function sumUsage(records) {
  const totals = {
    calls: 0,
    modelCalls: 0,
    fusionEngineCalls: 0,
    failedCalls: 0,
    retryCalls: 0,
    inputTokens: 0,
    outputTokens: 0,
    cachedTokens: 0,
    rebuildTokens: 0,
    toolTokens: 0,
    cacheHits: 0,
    cacheMisses: 0,
  }
  const cacheScenarios = new Map()
  let costUsd = 0
  const time = { wallMs: 0, modelWaitMs: 0, toolMs: 0, rebuildMs: 0 }
  for (const record of records) {
    for (const call of record.usage ?? []) {
      totals.calls += call.callCount ?? 1
      totals[call.callKind === 'fusion-engine' ? 'fusionEngineCalls' : 'modelCalls'] += call.callCount ?? 1
      totals.failedCalls += call.status === 'failed' ? 1 : 0
      totals.retryCalls += call.retry === true ? 1 : 0
      const cacheScenario = call.cacheScenario ?? 'unspecified'
      const scenario = cacheScenarios.get(cacheScenario) ?? {
        calls: 0,
        modelCalls: 0,
        fusionEngineCalls: 0,
        inputTokens: 0,
        outputTokens: 0,
        cachedTokens: 0,
        rebuildTokens: 0,
        toolTokens: 0,
        cacheHits: 0,
        cacheMisses: 0,
        costUsd: 0,
      }
      scenario.calls += call.callCount ?? 1
      scenario[call.callKind === 'fusion-engine' ? 'fusionEngineCalls' : 'modelCalls'] += call.callCount ?? 1
      for (const field of ['inputTokens', 'outputTokens', 'cachedTokens', 'rebuildTokens', 'toolTokens', 'cacheHits', 'cacheMisses']) {
        totals[field] += call[field] ?? 0
        scenario[field] += call[field] ?? 0
      }
      costUsd += call.costUsd ?? 0
      scenario.costUsd += call.costUsd ?? 0
      for (const field of Object.keys(time)) time[field] += call[field] ?? 0
      cacheScenarios.set(cacheScenario, scenario)
    }
  }
  return {
    tokenAndCache: Object.freeze({
      ...totals,
      cacheScenarios: Object.freeze(Object.fromEntries(
        [...cacheScenarios.entries()].sort(([left], [right]) => left.localeCompare(right))
          .map(([scenario, values]) => [scenario, Object.freeze({
            ...values,
            costUsd: Number(values.costUsd.toFixed(6)),
          })]),
      )),
    }),
    callAccounting: Object.freeze({
      modelCalls: totals.modelCalls,
      fusionEngineCalls: totals.fusionEngineCalls,
    }),
    cost: Object.freeze({ costUsd: Number(costUsd.toFixed(6)) }),
    time: Object.freeze(Object.fromEntries(Object.entries(time).map(([key, value]) => [key, Math.round(value)]))),
  }
}

function contextEffects(records) {
  const contexts = records.map(record => record.context).filter(Boolean)
  const sum = (field) => contexts.reduce((total, context) => total + (context[field] ?? 0), 0)
  const rebuildCount = sum('rebuildCount')
  return Object.freeze({
    rebuildCount,
    rebuildIntervalMs: contexts.length === 0 ? null : Math.round(
      contexts.reduce((total, context) => total + (context.rebuildIntervalMs ?? 0), 0) / contexts.length,
    ),
    compressionRatio: contexts.length === 0 ? null : Number((
      contexts.reduce((total, context) => total + (context.compressionRatio ?? 0), 0) / contexts.length
    ).toFixed(6)),
    badEvictions: sum('badEvictions'),
    staleResidue: sum('staleResidue'),
    forgettingEvents: sum('forgettingEvents'),
    postRebuildAttempted: sum('postRebuildAttempted'),
    postRebuildSucceeded: sum('postRebuildSucceeded'),
    evidenceStatus: contexts.length === 0 ? 'insufficient_evidence' : 'insufficient_evidence',
  })
}

function benchmarkGates(records, quality) {
  const complete = records.length > 0 && records.every(record => record.status === 'completed')
  const status = (metric, threshold, direction) => {
    if (!complete || metric.rate === 'insufficient_evidence') return 'insufficient_evidence'
    return direction === 'min' ? (metric.rate >= threshold ? 'pass' : 'fail')
      : (metric.rate <= threshold ? 'pass' : 'fail')
  }
  return Object.freeze({
    regret: Object.freeze({ threshold: 0.01, direction: 'max', status: complete ? 'insufficient_evidence' : 'insufficient_evidence' }),
    minority: Object.freeze({ threshold: 0.95, direction: 'min', status: status(quality.minority, 0.95, 'min') }),
    capture: Object.freeze({ threshold: 0.95, direction: 'min', status: status(quality.capture, 0.95, 'min') }),
    constraintRecall: Object.freeze({ threshold: 1, direction: 'min', status: status(quality.constraintRecall, 1, 'min') }),
    confirmedRecall: Object.freeze({ threshold: 1, direction: 'min', status: status(quality.confirmedRecall, 1, 'min') }),
    bindingError: Object.freeze({ threshold: 0, direction: 'max', status: complete ? (quality.bindingErrors === 0 ? 'pass' : 'fail') : 'insufficient_evidence' }),
    stateRegression: Object.freeze({ threshold: 0, direction: 'max', status: complete ? (quality.stateRegressions === 0 ? 'pass' : 'fail') : 'insufficient_evidence' }),
    hallucinated: Object.freeze({ threshold: 0, direction: 'max', status: complete ? (quality.hallucinated === 0 ? 'pass' : 'fail') : 'insufficient_evidence' }),
  })
}

function strategyReferences(plan, records) {
  const byCell = new Map(records.map(record => [record.runId, record.score]))
  const taskIds = [...new Set(plan.cells.map(cell => cell.taskId))]
  const configurations = [...new Set(plan.cells.map(cell => cell.configurationId))]
  const comparisons = [...PROVIDERS, 'fusion-4']
  return configurations.map(configurationId => {
    const scoreFor = (comparison, taskId) => byCell.get(
      plan.cells.find(cell => cell.configurationId === configurationId
        && cell.comparison === comparison && cell.taskId === taskId).runId,
    ) ?? 0
    const providerMeans = Object.fromEntries(PROVIDERS.map(provider => [provider, Number(
      (taskIds.reduce((sum, taskId) => sum + scoreFor(provider, taskId), 0) / taskIds.length).toFixed(6),
    )]))
    const bestMean = Math.max(...Object.values(providerMeans))
    const tiedBatchBest = PROVIDERS.filter(provider => providerMeans[provider] === bestMean)
    const regrets = Object.fromEntries(comparisons.map(comparison => {
      const againstBatchBest = Math.max(...tiedBatchBest.map(baseline => Number((
        taskIds.reduce((sum, taskId) => sum + Math.max(0, scoreFor(baseline, taskId) - scoreFor(comparison, taskId)), 0)
        / taskIds.length
      ).toFixed(6))))
      const againstPerTaskBest = Number((
        taskIds.reduce((sum, taskId) => sum + Math.max(
          0,
          Math.max(...PROVIDERS.map(provider => scoreFor(provider, taskId))) - scoreFor(comparison, taskId),
        ), 0) / taskIds.length
      ).toFixed(6))
      return [comparison, Object.freeze({ againstBatchBest, againstPerTaskBest })]
    }))
    return Object.freeze({
      configurationId,
      providerMeans: Object.freeze(providerMeans),
      batchBestSingleModels: Object.freeze(tiedBatchBest),
      postHocPerTaskBestIsRetrospective: true,
      regrets: Object.freeze(regrets),
    })
  })
}

export function aggregateBenchmarkReport(plan, { records, experimentId, ...binding }) {
  if (!plan || plan.kind !== 'winwincode.real-task-benchmark-plan.v1' || plan.cells.length !== 700) {
    fail('PLAN_INVALID', 'formal report requires the frozen 700-cell plan')
  }
  if (!Array.isArray(records) || records.length !== plan.cells.length
    || records.some((record, index) => record.runId !== plan.cells[index].runId)) {
    fail('LEDGER_INVALID', 'every frozen cell requires exactly one ordered run record')
  }
  for (const record of records) {
    const terminatedScoreIsSuccess = record.score != null && record.score !== 0
    if (record.termination && (record.status !== 'failed' && record.status !== 'not_run_runner_terminated'
      || record.verdict != null || terminatedScoreIsSuccess)) {
      fail('TERMINATION_STATE_INVALID', `${record.runId} has a termination recorded as success`)
    }
    if (record.status === 'completed' && (!Number.isFinite(record.score) || record.score < 0 || record.score > 1)) {
      fail('SCORE_INVALID', `${record.runId} completed without a normalized score`)
    }
    for (const call of record.usage ?? []) {
      if (call.callKind === 'fusion-engine') {
        const masqueradesAsModel = ['provider', 'requestedModelId', 'observedModelId', 'reasoningEffort']
          .some(field => field in call)
        if (masqueradesAsModel || call.engine !== 'fusion-engine'
          || call.algorithmVersion !== 'fusion-4-v1' || call.callCount !== 1
          || !SHA256_PATTERN.test(call.inputDigest ?? '') || !SHA256_PATTERN.test(call.outputDigest ?? '')) {
          fail('USAGE_IDENTITY_INVALID', `${record.runId} has an invalid fusion-engine call`)
        }
      } else if (call.callKind !== 'model' || !PROVIDERS.includes(call.requestedModelId)
        || call.requestedModelId !== call.observedModelId || call.reasoningEffort !== 'max') {
        fail('USAGE_IDENTITY_INVALID', `${record.runId} has an unproven model call`)
      }
    }
  }

  const completed = records.filter(record => record.status === 'completed')
  const qualityCounts = ['bindingErrors', 'stateRegressions', 'hallucinated'].reduce((counts, field) => ({
    ...counts,
    [field]: records.reduce((sum, record) => sum + (record.quality?.[field] ?? 0), 0),
  }), {})
  const usage = sumUsage(records)
  const quality = Object.freeze({
    meanScore: Number((records.reduce((sum, record) => sum + (record.score ?? 0), 0) / records.length).toFixed(6)),
    minority: metricRatio(records, 'minorityRetained', 'minorityTotal'),
    capture: metricRatio(records, 'captureRetained', 'captureTotal'),
    constraintRecall: metricRatio(records, 'constraintsRetained', 'constraintsTotal'),
    confirmedRecall: metricRatio(records, 'confirmedRetained', 'confirmedTotal'),
    ...Object.fromEntries(Object.entries(qualityCounts).map(([field, count]) => [field, count])),
  })
  const endToEndWallMs = completed.map(record => record.wallMs ?? 0).sort((left, right) => left - right)
  const p95Index = endToEndWallMs.length === 0 ? 0 : Math.min(
    endToEndWallMs.length - 1,
    Math.ceil(endToEndWallMs.length * 0.95) - 1,
  )
  return Object.freeze({
    schemaVersion: 1,
    kind: 'winwincode.real-task-benchmark-report.v1',
    experimentId,
    binding: Object.freeze(binding),
    failureAccounting: Object.freeze({
      denominator: records.length,
      completed: completed.length,
      unsuccessful: records.length - completed.length,
      stuckToolRepeatLimit: records.filter(record => record.termination?.reason === 'STUCK_TOOL_REPEAT_LIMIT').length,
      runnerTerminatedNotRun: records.filter(record => record.status === 'not_run_runner_terminated').length,
    }),
    quality,
    strategyReferences: Object.freeze(strategyReferences(plan, records)),
    tokenAndCache: usage.tokenAndCache,
    callAccounting: usage.callAccounting,
    cost: usage.cost,
    time: Object.freeze({
      ...usage.time,
      callWallMs: usage.time.wallMs,
      endToEnd: Object.freeze({
        totalWallMs: endToEndWallMs.reduce((total, wallMs) => total + wallMs, 0),
        p95Ms: endToEndWallMs[p95Index] ?? 0,
      }),
    }),
    contextEffects: contextEffects(records),
    gates: benchmarkGates(records, quality),
  })
}
