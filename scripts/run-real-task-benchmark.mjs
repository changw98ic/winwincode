#!/usr/bin/env node

import { createHash } from 'node:crypto'
import { execFile } from 'node:child_process'
import { readFile, readdir } from 'node:fs/promises'
import { resolve } from 'node:path'
import { promisify } from 'node:util'

import { openBenchmarkLedger } from './benchmark-ledger.mjs'
import { assertBenchmarkExecutionReceipts } from './benchmark-execution-receipts.mjs'

const runFile = promisify(execFile)

const PROVIDERS = Object.freeze(['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash'])
const SHA256_PATTERN = /^[0-9a-f]{64}$/u

const CONFIGURATIONS = Object.freeze([
  ['main-A', 'main', false, false, false],
  ['main-B', 'main', false, true, true],
  ['main-C', 'main', true, false, false],
  ['main-D', 'main', true, true, true],
  ['jev-context-only', 'jev-ablation', false, true, false],
  ['jev-judge-only', 'jev-ablation', false, false, true],
  ['jev-full', 'jev-ablation', false, true, true],
].map(([configurationId, track, fusion, jevContext, jevJudge]) => Object.freeze({
  configurationId, track, fusion, jev: jevContext || jevJudge, jevContext, jevJudge,
})))

export function benchmarkConfiguration(configurationId) {
  const configuration = CONFIGURATIONS.find(value => value.configurationId === configurationId)
  if (!configuration) fail('BENCHMARK_CONFIGURATION_INVALID', 'unknown or missing experimental configuration')
  return configuration
}

export function validateBenchmarkConfiguration(value) {
  const configuration = benchmarkConfiguration(value.configurationId)
  if (Object.entries(configuration).some(([key, expected]) => value[key] !== expected)) {
    fail('BENCHMARK_CONFIGURATION_INVALID', 'experimental switches do not match the frozen configuration')
  }
  return configuration
}

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

function runnerFailureCode(error) {
  return typeof error?.code === 'string' && /^[A-Z][A-Z0-9_]{0,127}$/u.test(error.code)
    ? error.code : 'RUNNER_UNEXPECTED'
}

function requiresBenchmarkRecovery(error) {
  return error?.benchmarkPersistenceFailure === true
    || error?.unresolvedDeviceExecution === true
    || ['BENCHMARK_EVIDENCE_FAILED', 'LEDGER_RUN_UNRESOLVED'].includes(error?.code)
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
  if (await gitOutput(root, 'status', '--porcelain=v1', '--untracked-files=all')) {
    fail('SOURCE_WORKTREE_DIRTY', 'frozen task repository has modified or untracked files')
  }

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
  const digestPaths = (await gitOutput(root, 'ls-files', '-z')).split('\0').filter(Boolean).sort()
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

export function benchmarkAggregationDigest(runId, taskId, members) {
  return createHash('sha256').update(canonicalJson({ runId, taskId, members })).digest('hex')
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
  const cells = []
  for (const configuration of CONFIGURATIONS) {
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

export async function executeBenchmarkCell(cell, adapter, {
  toolGate = createToolRequestGuard(),
  recordCall = () => {},
  registerLaunch = () => fail('LEDGER_REQUIRED', 'product launch registration requires a durable ledger'),
} = {}) {
  if (cell.budgetLimits !== null) throw new TypeError('formal benchmark cells cannot set budget limits')
  if (cell.reasoningEffort !== 'max') throw new TypeError('formal benchmark model calls require max reasoning effort')
  const configuration = validateBenchmarkConfiguration(cell)

  const requestFor = (provider, callId) => ({
    runId: cell.runId,
    taskId: cell.taskId,
    ...configuration,
    callId,
    provider,
    comparison: cell.comparison,
    fusionKind: cell.fusionKind,
    reasoningEffort: cell.reasoningEffort,
    budgetLimits: cell.budgetLimits,
  })
  const persistCall = call => {
    try { recordCall(call) } catch (error) {
      error.benchmarkPersistenceFailure = true
      throw error
    }
  }

  const executeCall = async (request, execute) => {
    let persistenceFailure = null
    const runner = Object.freeze({
      registerLaunch: async target => {
        try { await registerLaunch(target) } catch (error) {
          persistenceFailure = error
          error.benchmarkPersistenceFailure = true
          throw error
        }
      },
      requestTool: async (toolRequest, executor) => {
        const result = await executeToolRequest(toolRequest, toolGate, executor)
        if (result?.status === 'terminated') {
          const error = new Error(result.reason)
          error.code = result.reason
          error.termination = result
          throw error
        }
        return result
      },
    })
    let result
    try {
      result = await execute(request, runner)
      if (persistenceFailure) throw persistenceFailure
    } catch (error) {
      const code = runnerFailureCode(error)
      persistCall({ callId: request.callId, status: 'failed', failure: { code },
        ...(error?.unresolvedDeviceExecution === true ? { unresolvedDeviceExecution: true } : {}) })
      throw error
    }
    persistCall({ callId: request.callId, status: 'returned', result })
    const termination = runnerTermination(result)
    if (termination) {
      throw Object.assign(new Error('product execution terminated the current benchmark task'), {
        code: termination.reason, termination,
        productOutcome: result,
        ...(result.claims !== undefined ? { claims: result.claims } : {}),
      })
    }
    return result
  }

  if (cell.fusionKind === 'independent-aggregate') {
    const outcomes = await Promise.allSettled(PROVIDERS.map(async provider => {
      const request = requestFor(provider, `${cell.runId}:member:${provider}`)
      try {
        const member = await executeCall(request, (input, context) => adapter.runModel(input, context))
        return member?.status === 'failed'
          ? { status: 'failed', provider, callId: request.callId, failure: member.failure }
          : member
      } catch (error) {
        if (runnerTermination(error) || requiresBenchmarkRecovery(error)) throw error
        // A failed model is one retained member outcome, not permission to
        // omit the remaining independent members or reuse another cell.
        return { status: 'failed', provider, callId: request.callId,
          failure: { code: runnerFailureCode(error) } }
      }
    }))
    // Keep the ledger open until every launched member has retained its result,
    // including when a sibling stops the task. Aggregation preserves provider
    // order independently of completion order and starts only after this join.
    const failure = outcomes.find(outcome => outcome.status === 'rejected' && requiresBenchmarkRecovery(outcome.reason))
      ?? outcomes.find(outcome => outcome.status === 'rejected')
    if (failure) throw failure.reason
    const members = outcomes.map(outcome => outcome.value)
    const inputDigest = benchmarkAggregationDigest(cell.runId, cell.taskId, members)
    const aggregate = await executeCall({
      runId: cell.runId,
      taskId: cell.taskId,
      callId: `${cell.runId}:aggregation`,
      ...configuration,
      comparison: cell.comparison,
      engine: 'fusion-engine',
      algorithmVersion: 'fusion-4-v1',
      inputDigest,
      members,
    }, (request, context) => adapter.aggregate(request, context))
    const result = {
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
    return aggregate?.status === 'failed'
      ? { ...result, status: 'failed', failure: aggregate.failure, productOutcome: aggregate }
      : result
  }

  const model = await executeCall(requestFor(cell.comparison, `${cell.runId}:model`), (request, context) => adapter.runModel(request, context))
  return model?.status === 'failed'
    ? { model, status: 'failed', failure: model.failure, productOutcome: model }
    : { model }
}

function runnerTermination(value) {
  if (value?.termination) return value.termination
  if ((value?.code ?? value?.failure?.code) === 'STUCK_TOOL_REPEAT_LIMIT') {
    return { reason: 'STUCK_TOOL_REPEAT_LIMIT' }
  }
  return null
}

// Reconstruct the same cell from durable calls. Missing model results may only
// come from the registered product; aggregation is never dispatched again.
export async function recoverBenchmarkCell(cell, observed, resolveModel) {
  // registerLaunch is committed before the first product command. A claimed
  // cell with no launch or call therefore never entered the product loop.
  // Preserve it as an explicit failed denominator row instead of retrying it.
  if (observed.launches.length === 0 && observed.calls.length === 0) {
    return { status: 'failed', failure: { code: 'BENCHMARK_UNLAUNCHED_CLAIM' }, calls: [],
      recovery: { kind: 'unlaunched-claim', originalCalls: [] } }
  }
  const recoveredCalls = []
  const replay = async request => {
    const retained = observed.calls.find(call => call.callId === request.callId)
    if (retained?.status === 'returned') return retained.result
    if (retained?.status === 'failed' && retained.failure.code !== 'BENCHMARK_EVIDENCE_FAILED'
        && retained.unresolvedDeviceExecution !== true) {
      throw Object.assign(new Error(retained.failure.code), { code: retained.failure.code, retainedFailure: true })
    }
    const launch = observed.launches.find(target => target.callId === request.callId)
    if ((request.provider || request.engine === 'fusion-engine') && launch && resolveModel) return resolveModel(request, launch)
    fail('LEDGER_RUN_UNRESOLVED', 'registered execution has no final result')
  }
  let result
  try {
    result = await executeBenchmarkCell(cell, { runModel: replay, aggregate: replay }, {
      recordCall: call => recoveredCalls.push(call),
    })
    const terminalProductCall = cell.fusionKind === 'independent-aggregate' ? result.aggregate : result.model
    if (terminalProductCall?.status === 'failed') {
      result = { ...result, status: 'failed', failure: terminalProductCall.failure,
        productOutcome: terminalProductCall }
    }
  } catch (error) {
    if (requiresBenchmarkRecovery(error)) throw error
    if (!error.retainedFailure && !runnerTermination(error)) throw error
    const code = runnerFailureCode(error)
    result = { status: 'failed', termination: runnerTermination(error), failure: { code, message: code } }
  }
  // Existing receipts retain their durable order for the recovery transaction;
  // parallel member replay may finish in a different order from the first run.
  const retainedCallIds = new Set(observed.calls.map(call => call.callId))
  const recoveredByCallId = new Map(recoveredCalls.map(call => [call.callId, call]))
  const calls = [
    ...observed.calls.map(call => recoveredByCallId.get(call.callId) ?? call),
    ...recoveredCalls.filter(call => !retainedCallIds.has(call.callId)),
  ]
  return { ...result, calls, recovery: { kind: 'retained-product-result',
    originalCalls: observed.calls } }
}

// Coordinate independent durable task runners. Their returned task outcomes
// never stop admission; a thrown ledger/evidence error stops new admission and
// waits for already running tasks to retain their results before propagating.
export async function runBenchmarkSchedule(cells, { concurrency = 1, executeCell }) {
  validateBenchmarkConcurrency(concurrency)
  const results = Array(cells.length)
  let cursor = 0
  let failure = null
  const worker = async () => {
    while (cursor < cells.length && !failure) {
      const index = cursor++
      try { results[index] = await executeCell(cells[index], index) }
      catch (error) { failure ??= { error } }
    }
  }
  await Promise.all(Array.from({ length: Math.min(concurrency, cells.length) }, worker))
  if (failure) throw failure.error
  return results
}

function validateBenchmarkConcurrency(concurrency) {
  if (!Number.isInteger(concurrency) || concurrency < 1) {
    fail('BENCHMARK_CONCURRENCY_INVALID', 'benchmark concurrency must be a positive integer')
  }
}

// Admission policy can expand between runs without changing the frozen plan.
// Selecting profiles never creates a smaller experiment or claims another profile.
export function validateBenchmarkDispatchPolicy(plan, { concurrency = 1, selectedConfigurationIds } = {}) {
  validateBenchmarkConcurrency(concurrency)
  if (selectedConfigurationIds === undefined) return
  const configurations = new Set(plan.cells.map(cell => cell.configurationId))
  if (!Array.isArray(selectedConfigurationIds) || selectedConfigurationIds.length === 0
      || new Set(selectedConfigurationIds).size !== selectedConfigurationIds.length
      || selectedConfigurationIds.some(id => typeof id !== 'string' || !configurations.has(id))) {
    fail('BENCHMARK_CONFIGURATION_SELECTION_INVALID', 'selected configurations must be unique profiles from the frozen plan')
  }
}

// Validate formal configuration before initializing or checking the immutable
// identity. This may create an unclaimed ledger, but never starts product work.
export function verifyBenchmarkLedgerIdentity(plan, options) {
  validateFormalBenchmarkOptions(options)
  validateBenchmarkDispatchPolicy(plan, options)
  const { ledgerPath, experimentBinding } = options
  const store = openBenchmarkLedger(ledgerPath, canonicalJson({ plan, experimentBinding }), plan.cells)
  store.close()
}

export async function runBenchmarkPlan(plan, {
  executeCell,
  createToolGate = () => createToolRequestGuard(),
  onRecord = () => {},
  ledgerPath,
  experimentBinding,
  recoverCell,
  concurrency = 1,
  selectedConfigurationIds,
}) {
  validateBenchmarkDispatchPolicy(plan, { concurrency, selectedConfigurationIds })
  const selected = selectedConfigurationIds === undefined ? null : new Set(selectedConfigurationIds)
  const store = ledgerPath
    ? openBenchmarkLedger(ledgerPath, canonicalJson({ plan, experimentBinding }), plan.cells)
    : null
  const records = plan.cells.map(cell => ({ ...cell, status: 'planned', verdict: null, score: null, termination: null }))
  try {
    for (const [index, record] of (store?.records() ?? []).entries()) {
      if (record !== null) records[index] = record
    }
    const execute = async (cell, index) => {
      if (selected && !selected.has(cell.configurationId)) return records[index]
      let claim
      try {
        claim = store?.claim(index)
      } catch (error) {
        if (error.code !== 'LEDGER_RUN_UNRESOLVED' || !recoverCell) throw error
        const result = await recoverCell(cell, { launches: error.launches, calls: error.calls })
        const termination = runnerTermination(result)
        const recovered = { ...cell, ...result, runId: cell.runId,
          status: termination ? 'failed' : result.status ?? 'completed',
          verdict: null, score: null, termination, launches: error.launches }
        store.recover(index, error, recovered)
        claim = { record: recovered }
      }
      let record = claim?.record
      if (!record) {
        const startedAtMs = Date.now()
        let persistenceError = null
        const registerLaunch = target => {
          try {
            if (!store) fail('LEDGER_REQUIRED', 'product launch registration requires a durable ledger')
            store.registerLaunch(index, claim.token, target)
          } catch (error) {
            persistenceError = error
            throw error
          }
        }
        const recordCall = call => {
          try {
            store?.recordCall(index, claim.token, call)
          } catch (error) {
            persistenceError = error
            throw error
          }
        }
        record = { ...cell, status: 'planned', termination: null }
        try {
          const result = await executeCell(cell, { toolGate: createToolGate(cell), registerLaunch, recordCall })
          if (persistenceError) throw persistenceError
          const termination = runnerTermination(result)
          record = {
            ...record,
            ...result,
            runId: cell.runId,
            status: termination ? 'failed' : result.status ?? 'completed',
            // Benchmark grades belong to the user's independent grading machine.
            verdict: null,
            score: null,
            termination,
          }
        } catch (error) {
          if (persistenceError) throw persistenceError
          if (requiresBenchmarkRecovery(error)) throw error
          const termination = runnerTermination(error)
          record = {
            ...record,
            ...(error?.claims !== undefined ? { claims: error.claims } : {}),
            ...(error?.productOutcome !== undefined ? { productOutcome: error.productOutcome } : {}),
            status: 'failed',
            verdict: null,
            score: null,
            termination,
            failure: {
              code: termination?.reason ?? runnerFailureCode(error),
              // Raw adapter errors can contain request headers or credentials.
              // Detailed diagnostics remain in the sanitized product evidence.
              message: termination?.reason ?? runnerFailureCode(error),
            },
          }
        }
        // Storage/export failures must escape. They are not model failures and
        // must never overwrite a completed task or allow the next task to start.
        if (store) {
          record.launches = store.launches(index)
          record.calls = store.calls(index)
        }
        record.wallMs ??= Date.now() - startedAtMs
        store?.finish(index, claim.token, record)
      }
      records[index] = record
      // A retained termination belongs to this task. Only hard persistence or
      // evidence errors escape the loop and prevent admission of another task.
      await onRecord(record, index)
      return record
    }
    await runBenchmarkSchedule(plan.cells, { concurrency, executeCell: execute })
  } finally {
    store?.close()
  }
  return Object.freeze({
    schemaVersion: 1,
    kind: 'winwincode.real-task-benchmark-ledger.v1',
    denominator: records.length,
    records: Object.freeze(records.map(record => Object.freeze(record))),
  })
}

export function validateFormalBenchmarkOptions({ providerEvidence, ...options }) {
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
  if (!options.ledgerPath || !options.experimentBinding?.experimentId) {
    fail('LEDGER_REQUIRED', 'formal execution requires a durable ledger and experiment identity')
  }
}

export async function executeFormalBenchmark(plan, { providerEvidence, ...options }) {
  validateFormalBenchmarkOptions({ providerEvidence, ...options })
  return runBenchmarkPlan(plan, options)
}

function metricRatio(records, numeratorField, denominatorField) {
  let numerator = 0
  let denominator = 0
  let observedRuns = 0
  for (const record of records) {
    const top = record.quality?.[numeratorField]
    const bottom = record.quality?.[denominatorField]
    if (top == null || bottom == null) continue
    if (!Number.isSafeInteger(top) || !Number.isSafeInteger(bottom) || top < 0 || bottom < top) {
      fail('QUALITY_INVALID', `${record.runId} has invalid ${numeratorField}/${denominatorField}`)
    }
    observedRuns += 1
    numerator += top
    denominator += bottom
  }
  if (denominator === 0) {
    return Object.freeze({
      numerator,
      denominator,
      observedRuns,
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
    observedRuns,
    rate: Number(proportion.toFixed(6)),
    interval95: Object.freeze({
      lower: Number(Math.max(0, center - halfWidth).toFixed(6)),
      upper: Number(Math.min(1, center + halfWidth).toFixed(6)),
    }),
  })
}

function retainedUsage(record) {
  if (!Array.isArray(record.calls)) return { rows: record.usage ?? [], performance: [],
    unmeasuredProductCalls: 0, jevCalls: 0, real: false }
  const rows = []
  const performance = []
  let unmeasuredProductCalls = 0
  let jevCalls = 0
  for (const call of record.calls) {
    const receipt = call.result?.executionReceipts
    if (!receipt || !Array.isArray(receipt.calls) || receipt.calls.length === 0) {
      unmeasuredProductCalls += 1
      continue
    }
    assertBenchmarkExecutionReceipts(receipt)
    for (const exchange of receipt.calls) {
      const tracked = exchange.providerAttempts?.length > 0
      const attempts = tracked ? exchange.providerAttempts.filter(attempt =>
        !['prepared', 'not_sent'].includes(attempt.state)) : null
      const provenUnsent = tracked && exchange.providerAttempts.every(attempt => attempt.state === 'not_sent')
      const measured = attempts?.length ? [...attempts] : [{ usage: provenUnsent ? {
        inputTokens: 0, outputTokens: 0, cachedTokens: 0,
      } : tracked ? null : exchange.usage }]
      if (attempts?.length && exchange.providerAttempts.some(attempt => attempt.state === 'prepared')) {
        measured.push({ usage: null, state: 'prepared' })
      }
      for (const [index, attempt] of measured.entries()) {
        const cached = attempt.usage?.cachedTokens ?? null
        const invoked = attempt.state !== undefined && !['prepared', 'not_sent'].includes(attempt.state)
        rows.push({ callKind: 'model', callCount: index === 0 ? 1 : 0,
          requestedModelId: exchange.requestedModel,
          observedModelId: tracked ? attempt.actualModels?.at(-1) ?? null : exchange.actualModels.at(-1) ?? null,
          reasoningEffort: exchange.reasoningEffort,
          status: tracked && invoked ? (attempt.state === 'completed' ? 'completed' : 'failed')
            : exchange.terminalType === 'completed' ? 'completed' : 'failed',
          logicalCallFailed: exchange.terminalType !== 'completed',
          retry: tracked && invoked && index > 0,
          ...(tracked ? { providerInvocation: invoked ? 1 : 0,
            providerAccountingPending: attempt.state === 'prepared',
            providerChargeUnknown: invoked && (attempt.usage == null
              || ['invoking', 'interrupted_unknown'].includes(attempt.state)),
            failedProviderInvocation: ['failed', 'interrupted_unknown'].includes(attempt.state) } : {}),
          cacheScenario: provenUnsent ? 'not-sent' : cached === null ? 'unknown' : cached > 0 ? 'cached' : 'uncached',
          inputTokens: attempt.usage?.inputTokens ?? null,
          outputTokens: attempt.usage?.outputTokens ?? null,
          cachedTokens: cached,
          cacheHits: provenUnsent ? 0 : cached === null ? null : Number(cached > 0),
          cacheMisses: provenUnsent ? 0 : cached === null ? null : Number(cached === 0),
          sourceExchangeId: exchange.exchangeId,
          ...(attempt.attemptNumber === undefined ? {} : { providerAttemptNumber: attempt.attemptNumber }) })
      }
    }
    for (const jev of receipt.jev ?? []) {
      jevCalls += 1
      rows.push({ callKind: 'jev-model', requestedModelId: jev.requestedModel,
        observedModelId: jev.actualModel,
        status: jev.failureCount === 0 && jev.actualModel ? 'completed' : 'failed',
        cacheScenario: 'jev', inputTokens: jev.inputTokens, outputTokens: jev.outputTokens,
        cachedTokens: null, cacheHits: null, cacheMisses: null,
        sourceExchangeId: jev.exchangeId })
    }
    performance.push(...(receipt.performance ?? []))
  }
  if (record.aggregateReceipt) rows.push({ ...record.aggregateReceipt,
    cacheScenario: 'fusion-engine', costUsd: 0 })
  return { rows, performance, unmeasuredProductCalls, jevCalls, real: true }
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
  const missing = { inputTokens: 0, outputTokens: 0, cachedTokens: 0,
    rebuildTokens: 0, toolTokens: 0, cacheHits: 0, cacheMisses: 0,
    cost: 0, wallMs: 0, modelWaitMs: 0, toolMs: 0,
    rebuildMs: 0, productCalls: 0 }
  let jevModelCalls = 0
  let providerInvocations = 0
  let failedProviderInvocations = 0
  let trackedProviderInvocations = false
  let untrackedProviderInvocations = false
  for (const record of records) {
    const evidence = retainedUsage(record)
    missing.productCalls += evidence.unmeasuredProductCalls
    jevModelCalls += evidence.jevCalls
    if (evidence.unmeasuredProductCalls > 0) {
      for (const field of ['inputTokens', 'outputTokens', 'cachedTokens', 'rebuildTokens',
        'toolTokens', 'cacheHits', 'cacheMisses']) {
        missing[field] += evidence.unmeasuredProductCalls
      }
      missing.cost += evidence.unmeasuredProductCalls
      for (const field of Object.keys(time)) missing[field] += evidence.unmeasuredProductCalls
    }
    for (const call of evidence.rows) {
      totals.calls += call.callCount ?? 1
      totals[call.callKind === 'fusion-engine' ? 'fusionEngineCalls' : 'modelCalls'] += call.callCount ?? 1
      totals.failedCalls += (call.logicalCallFailed ?? (call.status === 'failed')) ? call.callCount ?? 1 : 0
      totals.retryCalls += call.retry === true ? 1 : 0
      if (call.callKind === 'model' && evidence.real) {
        if (call.providerInvocation === undefined) untrackedProviderInvocations = true
        else {
          trackedProviderInvocations = true
          providerInvocations += call.providerInvocation
          failedProviderInvocations += call.failedProviderInvocation ? 1 : 0
        }
      }
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
        costUnknown: 0,
      }
      scenario.calls += call.callCount ?? 1
      scenario[call.callKind === 'fusion-engine' ? 'fusionEngineCalls' : 'modelCalls'] += call.callCount ?? 1
      if (call.providerInvocation !== undefined) {
        scenario.providerInvocations = (scenario.providerInvocations ?? 0) + call.providerInvocation
      }
      for (const field of ['inputTokens', 'outputTokens', 'cachedTokens', 'rebuildTokens', 'toolTokens', 'cacheHits', 'cacheMisses']) {
        if (call[field] == null) {
          if (field in missing && call.callKind !== 'fusion-engine') missing[field] += 1
        } else {
          if (!Number.isSafeInteger(call[field]) || call[field] < 0) fail('USAGE_INVALID', `${record.runId} has invalid ${field}`)
          totals[field] += call[field]
          scenario[field] += call[field]
        }
      }
      if (call.costUsd != null) {
        if (!Number.isFinite(call.costUsd) || call.costUsd < 0) fail('USAGE_INVALID', `${record.runId} has invalid cost`)
        costUsd += call.costUsd
        scenario.costUsd += call.costUsd
      } else if (!evidence.real && call.callKind !== 'fusion-engine') {
        missing.cost += 1
        scenario.costUnknown += 1
      } else if (evidence.real && call.callKind !== 'fusion-engine') {
        scenario.costUnknown += 1
      }
      if (!evidence.real) for (const field of Object.keys(time)) {
        if (call[field] != null) time[field] += call[field]
        else if (call.callKind !== 'fusion-engine') missing[field] += 1
      }
      cacheScenarios.set(cacheScenario, scenario)
    }
    if (evidence.real && record.calls.length > 0) {
      const primaryCalls = evidence.rows.filter(call => call.callKind === 'model')
        .reduce((sum, call) => sum + (call.callCount ?? 1), 0)
      const measuredCalls = evidence.performance.reduce((sum, run) => sum + run.modelCalls, 0)
      const retriedProviderCalls = evidence.rows.some(call => call.providerInvocation === 1 && call.retry)
      const pendingProviderAccounting = evidence.rows.some(call => call.providerAccountingPending)
      const unknownProviderCharge = evidence.rows.some(call => call.providerChargeUnknown)
      if (evidence.performance.length === 0 || measuredCalls !== primaryCalls || evidence.jevCalls > 0
          || retriedProviderCalls || pendingProviderAccounting || unknownProviderCharge
          || evidence.performance.some(run => run.actualCostMicros == null)) {
        missing.cost += 1
      } else {
        costUsd += evidence.performance.reduce((sum, run) => sum + run.actualCostMicros, 0) / 1_000_000
      }
      if (evidence.performance.length === 0) {
        for (const field of Object.keys(time)) missing[field] += 1
      } else {
        for (const run of evidence.performance) {
          for (const [field, value] of [['wallMs', run.totalRuntimeMs], ['modelWaitMs', run.modelWaitMs],
            ['toolMs', run.toolMs], ['rebuildMs', run.rebuildMs]]) {
            if (value == null) missing[field] += 1
            else time[field] += value
          }
        }
      }
    }
  }
  const reported = field => missing[field] === 0 ? totals[field] : null
  return {
    tokenAndCache: Object.freeze({
      ...totals,
      inputTokens: reported('inputTokens'), outputTokens: reported('outputTokens'),
      cachedTokens: reported('cachedTokens'), rebuildTokens: reported('rebuildTokens'),
      toolTokens: reported('toolTokens'), cacheHits: reported('cacheHits'),
      cacheMisses: reported('cacheMisses'),
      measured: Object.freeze({ inputTokens: totals.inputTokens, outputTokens: totals.outputTokens,
        cachedTokens: totals.cachedTokens }),
      unknownUsageExchanges: Math.max(missing.inputTokens, missing.outputTokens),
      cacheScenarios: Object.freeze(Object.fromEntries(
        [...cacheScenarios.entries()].sort(([left], [right]) => left.localeCompare(right))
          .map(([scenario, values]) => [scenario, Object.freeze({
            ...Object.fromEntries(Object.entries(values).filter(([key]) => key !== 'costUnknown')),
            costUsd: values.costUnknown === 0 ? Number(values.costUsd.toFixed(6)) : null,
          })]),
      )),
    }),
    callAccounting: Object.freeze({
      modelCalls: totals.modelCalls,
      fusionEngineCalls: totals.fusionEngineCalls,
      jevModelCalls,
      ...(trackedProviderInvocations ? {
        providerInvocations: untrackedProviderInvocations ? null : providerInvocations,
        failedProviderInvocations: untrackedProviderInvocations ? null : failedProviderInvocations,
      } : {}),
      unmeasuredProductCalls: missing.productCalls,
    }),
    cost: Object.freeze({ costUsd: missing.cost === 0 ? Number(costUsd.toFixed(6)) : null,
      measuredCostUsd: Number(costUsd.toFixed(6)), unknownCostSources: missing.cost }),
    time: Object.freeze(Object.fromEntries(Object.entries(time).map(([key, value]) =>
      [key, missing[key] === 0 ? Math.round(value) : null]))),
    coverage: Object.freeze(missing),
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

function benchmarkGates(records, quality, references) {
  const complete = records.length > 0 && records.every(record => record.score != null)
  const fusionRegrets = references.map(reference => reference.regrets?.['fusion-4']?.againstBatchBest)
  const regret = complete && fusionRegrets.every(value => Number.isFinite(value))
    ? (Math.max(...fusionRegrets) <= 0.01 ? 'pass' : 'fail') : 'insufficient_evidence'
  const status = (metric, threshold, direction) => {
    if (!complete || metric.observedRuns !== records.length || metric.rate === 'insufficient_evidence') return 'insufficient_evidence'
    return direction === 'min' ? (metric.rate >= threshold ? 'pass' : 'fail')
      : (metric.rate <= threshold ? 'pass' : 'fail')
  }
  return Object.freeze({
    regret: Object.freeze({ threshold: 0.01, direction: 'max', status: regret }),
    minority: Object.freeze({ threshold: 0.95, direction: 'min', status: status(quality.minority, 0.95, 'min') }),
    capture: Object.freeze({ threshold: 0.95, direction: 'min', status: status(quality.capture, 0.95, 'min') }),
    constraintRecall: Object.freeze({ threshold: 1, direction: 'min', status: status(quality.constraintRecall, 1, 'min') }),
    confirmedRecall: Object.freeze({ threshold: 1, direction: 'min', status: status(quality.confirmedRecall, 1, 'min') }),
    bindingError: Object.freeze({ threshold: 0, direction: 'max', status: complete && quality.bindingErrors != null ? (quality.bindingErrors === 0 ? 'pass' : 'fail') : 'insufficient_evidence' }),
    stateRegression: Object.freeze({ threshold: 0, direction: 'max', status: complete && quality.stateRegressions != null ? (quality.stateRegressions === 0 ? 'pass' : 'fail') : 'insufficient_evidence' }),
    hallucinated: Object.freeze({ threshold: 0, direction: 'max', status: complete && quality.hallucinated != null ? (quality.hallucinated === 0 ? 'pass' : 'fail') : 'insufficient_evidence' }),
  })
}

function strategyReferences(plan, records) {
  const byCell = new Map(records.map(record => [record.runId, record.score]))
  const taskIds = [...new Set(plan.cells.map(cell => cell.taskId))]
  const configurations = [...new Set(plan.cells.map(cell => cell.configurationId))]
  const comparisons = [...PROVIDERS, 'fusion-4']
  return configurations.map(configurationId => {
    if (plan.cells.some(cell => cell.configurationId === configurationId && byCell.get(cell.runId) == null)) {
      return Object.freeze({
        configurationId, gradingStatus: 'pending', providerMeans: null,
        batchBestSingleModels: null, postHocPerTaskBestIsRetrospective: true, regrets: null,
      })
    }
    const scoreFor = (comparison, taskId) => byCell.get(
      plan.cells.find(cell => cell.configurationId === configurationId
        && cell.comparison === comparison && cell.taskId === taskId).runId,
    )
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
      gradingStatus: 'complete',
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
    if (record.score != null && (!Number.isFinite(record.score) || record.score < 0 || record.score > 1)) {
      fail('SCORE_INVALID', `${record.runId} has an invalid normalized score`)
    }
    for (const call of retainedUsage(record).rows) {
      if (call.callKind === 'fusion-engine') {
        const masqueradesAsModel = ['provider', 'requestedModelId', 'observedModelId', 'reasoningEffort']
          .some(field => field in call)
        if (masqueradesAsModel || call.engine !== 'fusion-engine'
          || call.algorithmVersion !== 'fusion-4-v1' || call.callCount !== 1
          || !SHA256_PATTERN.test(call.inputDigest ?? '') || !SHA256_PATTERN.test(call.outputDigest ?? '')) {
          fail('USAGE_IDENTITY_INVALID', `${record.runId} has an invalid fusion-engine call`)
        }
      } else if (call.callKind === 'jev-model') {
        if (call.status === 'completed' && (typeof call.requestedModelId !== 'string'
          || !call.requestedModelId || typeof call.observedModelId !== 'string'
          || !call.observedModelId)) {
          fail('USAGE_IDENTITY_INVALID', `${record.runId} has an unproven JEV call`)
        }
      } else if (call.callKind !== 'model' || !PROVIDERS.includes(call.requestedModelId)
        || (call.observedModelId !== null && call.requestedModelId !== call.observedModelId)
        || (call.status !== 'failed' && call.observedModelId === null)
        || call.reasoningEffort !== 'max') {
        fail('USAGE_IDENTITY_INVALID', `${record.runId} has an unproven model call`)
      }
    }
  }

  const completed = records.filter(record => record.status === 'completed')
  const scored = records.filter(record => record.score != null).length
  const qualityCounts = Object.fromEntries(['bindingErrors', 'stateRegressions', 'hallucinated'].map(field => {
    const values = records.map(record => record.quality?.[field])
    if (values.some(value => value != null && (!Number.isSafeInteger(value) || value < 0))) {
      fail('QUALITY_INVALID', `invalid ${field} count`)
    }
    return [field, values.some(value => value == null) ? null : values.reduce((sum, value) => sum + value, 0)]
  }))
  const usage = sumUsage(records)
  const references = strategyReferences(plan, records)
  const quality = Object.freeze({
    meanScore: scored === records.length
      ? Number((records.reduce((sum, record) => sum + record.score, 0) / records.length).toFixed(6)) : null,
    minority: metricRatio(records, 'minorityRetained', 'minorityTotal'),
    capture: metricRatio(records, 'captureRetained', 'captureTotal'),
    constraintRecall: metricRatio(records, 'constraintsRetained', 'constraintsTotal'),
    confirmedRecall: metricRatio(records, 'confirmedRetained', 'confirmedTotal'),
    ...Object.fromEntries(Object.entries(qualityCounts).map(([field, count]) => [field, count])),
  })
  const timedRecords = completed
  const endToEndComplete = timedRecords.every(record => Number.isSafeInteger(record.wallMs) && record.wallMs >= 0)
  const endToEndWallMs = timedRecords.map(record => record.wallMs).filter(value => value != null)
    .sort((left, right) => left - right)
  const executedRecords = records.filter(record => record.status === 'completed' || record.status === 'failed')
  const executionComplete = executedRecords.every(record => Number.isSafeInteger(record.wallMs) && record.wallMs >= 0)
  const executionWallMs = executedRecords.map(record => record.wallMs).filter(value => value != null)
    .sort((left, right) => left - right)
  const p95Index = endToEndWallMs.length === 0 ? 0 : Math.min(
    endToEndWallMs.length - 1,
    Math.ceil(endToEndWallMs.length * 0.95) - 1,
  )
  return Object.freeze({
    schemaVersion: 1,
    kind: 'winwincode.real-task-benchmark-report.v1',
    experimentId,
    binding: Object.freeze(binding),
    grading: Object.freeze({ status: scored === records.length ? 'complete' : 'pending', scored, pending: records.length - scored }),
    failureAccounting: Object.freeze({
      denominator: records.length,
      completed: completed.length,
      unsuccessful: records.length - completed.length,
      stuckToolRepeatLimit: records.filter(record => record.termination?.reason === 'STUCK_TOOL_REPEAT_LIMIT').length,
      runnerTerminatedNotRun: records.filter(record => record.status === 'not_run_runner_terminated').length,
    }),
    quality,
    strategyReferences: Object.freeze(references),
    tokenAndCache: usage.tokenAndCache,
    callAccounting: usage.callAccounting,
    accountingCoverage: usage.coverage,
    cost: usage.cost,
    time: Object.freeze({
      ...usage.time,
      callWallMs: usage.time.wallMs,
      endToEnd: Object.freeze({
        totalWallMs: endToEndComplete && endToEndWallMs.length > 0
          ? endToEndWallMs.reduce((total, wallMs) => total + wallMs, 0) : null,
        p95Ms: endToEndComplete ? endToEndWallMs[p95Index] ?? null : null,
        observedRuns: endToEndWallMs.length, missingRuns: timedRecords.length - endToEndWallMs.length,
      }),
      execution: Object.freeze({
        totalWallMs: executionComplete && executionWallMs.length > 0
          ? executionWallMs.reduce((total, wallMs) => total + wallMs, 0) : null,
        p95Ms: executionComplete && executionWallMs.length > 0
          ? executionWallMs[Math.ceil(executionWallMs.length * 0.95) - 1] : null,
        observedRuns: executionWallMs.length,
        missingRuns: executedRecords.length - executionWallMs.length,
      }),
    }),
    contextEffects: contextEffects(records),
    gates: benchmarkGates(records, quality, references),
  })
}
