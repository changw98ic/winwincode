import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, globSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
import { join, relative } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { retainedModelFailure } from './device-model-failures.mjs'

const digest = bytes => createHash('sha256').update(bytes).digest('hex')
const objectId = /^[0-9a-f]{40}$/u

function retain(path, bytes) {
  try {
    writeFileSync(path, bytes, { flag: 'wx', mode: 0o600 })
  } catch (error) {
    if (error.code !== 'EEXIST') throw error
    assert.deepEqual(readFileSync(path), Buffer.from(bytes), 'candidate export must not replace prior evidence')
  }
}

function decodeReceipt(payload) {
  const bytes = Buffer.from(payload.dataBase64, 'base64')
  assert.equal(bytes.toString('base64'), payload.dataBase64)
  assert.equal(`sha256:${digest(bytes)}`, payload.payloadDigest, 'model receipt digest mismatch')
  return JSON.parse(bytes)
}

function measuredUsage(usage) {
  if (usage == null) return null
  const counter = (camel, snake, { optional = false } = {}) => {
    const value = usage[camel] ?? usage[snake] ?? null
    assert.ok((optional && value === null) || (Number.isSafeInteger(value) && value >= 0),
      'invalid measured usage')
    return value
  }
  const inputTokens = counter('inputTokens', 'input_tokens')
  const outputTokens = counter('outputTokens', 'output_tokens')
  const measuredTotal = inputTokens + outputTokens
  assert.ok(Number.isSafeInteger(measuredTotal), 'invalid measured usage total')
  const totalTokens = counter('totalTokens', 'total_tokens', { optional: true }) ?? measuredTotal
  assert.equal(totalTokens, measuredTotal, 'measured usage total does not match input and output')
  return { inputTokens, outputTokens, totalTokens,
    cachedTokens: counter('cachedInputTokens', 'cached_input_tokens', { optional: true }),
    cacheWriteTokens: counter('cacheWriteInputTokens', 'cache_write_input_tokens', { optional: true }),
    reasoningTokens: counter('reasoningOutputTokens', 'reasoning_output_tokens', { optional: true }) }
}

function providerAttemptChunks(chunks, opened) {
  if (chunks === null) return null
  const values = JSON.parse(chunks)
  assert.ok(Array.isArray(values), 'invalid Provider attempt chunks')
  const decoded = []
  let terminalSeen = false
  for (const chunk of values) {
    assert.equal(chunk.modelExchangeId, opened.modelExchangeId, 'Provider attempt exchange mismatch')
    assert.deepEqual(chunk.lease, opened.lease, 'Provider attempt lease mismatch')
    assert.equal(chunk.workerSessionId, opened.workerSessionId, 'Provider attempt Worker Session mismatch')
    assert.deepEqual(chunk.sessionIdentity, opened.sessionIdentity, 'Provider attempt Session binding mismatch')
    assert.ok(Number.isSafeInteger(chunk.sequence) && chunk.sequence > 0, 'invalid Provider attempt sequence')
    assert.equal(terminalSeen, false, 'Provider attempt has chunks after its terminal')
    if (chunk.isFinal === true) terminalSeen = true
    if (chunk.error) assert.equal(chunk.isFinal, true, 'Provider attempt failure must be terminal')
    decoded.push({ chunk, frame: chunk.payload ? decodeReceipt(chunk.payload) : null })
  }
  return decoded
}

function readProviderAttempts(database, opened) {
  return database.prepare(`SELECT exchange_id, attempt_number, adapter_request_id, state,
    failure_chunks, accounting_chunks, response_bytes FROM model_invocation_attempts
    WHERE exchange_id = ? ORDER BY attempt_number`).all(opened.modelExchangeId).map((row, index) => {
    assert.equal(row.exchange_id, opened.modelExchangeId, 'Provider attempt exchange mismatch')
    assert.equal(row.attempt_number, index + 1, 'Provider attempt numbers are incomplete')
    assert.equal(row.adapter_request_id, `device-${opened.modelExchangeId}:attempt:${row.attempt_number}`,
      'Provider attempt request identity mismatch')
    assert.ok(['prepared', 'invoking', 'completed', 'failed', 'not_sent', 'interrupted_unknown'].includes(row.state),
      'invalid Provider attempt state')
    const failures = providerAttemptChunks(row.failure_chunks, opened)
    const accounting = providerAttemptChunks(row.accounting_chunks, opened)
    const retainedFailures = (failures ?? []).flatMap(({ chunk, frame }) =>
      chunk.error ? [retainedModelFailure(chunk.error, { metadata: frame })]
        : frame?.type === 'error' ? [retainedModelFailure(frame.error,
          { canonical: true, metadata: { ...frame, ...frame.error } })] : [])
    assert.ok(retainedFailures.length <= 1, 'multiple Provider attempt failures')
    const terminal = (accounting ?? []).filter(({ chunk }) => chunk.isFinal === true)
    assert.ok(terminal.length <= 1, 'multiple Provider attempt accounting receipts')
    if (row.state === 'not_sent' || row.state === 'prepared') {
      assert.equal(accounting, null, 'unsent Provider attempt has accounting')
      assert.equal(row.response_bytes, null, 'unsent Provider attempt has an upstream response')
    }
    const actualModels = [...new Set([...(failures ?? []), ...(accounting ?? [])]
      .filter(({ frame }) => frame?.type === 'server_model').map(({ frame }) => frame.model))]
    assert.ok(actualModels.every(model => typeof model === 'string'), 'invalid observed Provider model')
    const usage = measuredUsage(terminal[0]?.frame?.tokenUsage ?? terminal[0]?.frame?.token_usage ?? null)
    if (row.response_bytes !== null) assert.ok(row.response_bytes instanceof Uint8Array,
      'invalid retained Provider response bytes')
    return { exchangeId: row.exchange_id, attemptNumber: row.attempt_number,
      adapterRequestId: row.adapter_request_id, state: row.state, actualModels,
      ...(retainedFailures.length === 0 ? {} : { failure: retainedFailures[0] }),
      usage,
      failureChunksSha256: row.failure_chunks === null ? null : digest(row.failure_chunks),
      accountingChunksSha256: row.accounting_chunks === null ? null : digest(row.accounting_chunks),
      responseSha256: row.response_bytes === null ? null : digest(row.response_bytes) }
  })
}

function actualUsage(call) {
  if (!call.providerAttempts?.length) return { complete: call.usage !== null, rows: [call.usage] }
  const attempts = call.providerAttempts.filter(attempt => !['not_sent', 'prepared'].includes(attempt.state))
  return { complete: !call.providerAttempts.some(attempt => attempt.state === 'prepared')
      && attempts.every(attempt => ['completed', 'failed'].includes(attempt.state) && attempt.usage !== null),
    rows: attempts.map(attempt => attempt.usage) }
}

// Shared runtimes retain all task jobs. Select this Delivery's durable jobs,
// including Controller role Sessions, before decoding or aggregating receipts.
function taskReceiptJobs(directory, scope) {
  if (scope === undefined && existsSync(join(directory, 'runtime-binding.json'))) {
    const report = JSON.parse(readFileSync(join(directory, 'device-task-result.json')))
    scope = { deliveryId: report.deliveryId, productSessionId: report.productSessionId }
  }
  if (scope === undefined) return null
  assert.match(scope.deliveryId, /^dlv_[0-9A-HJKMNP-TV-Z]{26}$/u)
  assert.match(scope.productSessionId, /^psn_[0-9A-HJKMNP-TV-Z]{26}$/u)
  const server = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'), { readOnly: true })
  try {
    server.exec('PRAGMA busy_timeout=5000')
    return new Set(server.prepare(`SELECT job_id FROM scheduler_execution_jobs
      WHERE delivery_id = ? OR product_session_id = ?`).all(scope.deliveryId, scope.productSessionId)
      .map(row => row.job_id))
  } finally { server.close() }
}

// Export identity/accounting facts only; prompts, tool output and credentials stay private.
export function readDeviceExecutionReceipts(directory, scope) {
  const selectedJobs = taskReceiptJobs(directory, scope)
  const deviceDirectory = existsSync(join(directory, 'device-data')) ? realpathSync(join(directory, 'device-data')) : null
  const calls = []
  const jev = []
  const performance = []
  const identities = new Set()
  for (const path of (deviceDirectory === null ? [] : globSync('**/providers.sqlite3', { cwd: deviceDirectory }).map(path => join('device-data', path))).sort()) {
    const database = new DatabaseSync(join(directory, path), { readOnly: true })
    try {
      database.exec('PRAGMA busy_timeout=5000')
      const hasProviderAttempts = Boolean(database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='model_invocation_attempts'").get())
      const exchanges = new Map()
      for (const row of database.prepare('SELECT exchange_id, request_open, chunks FROM exchanges WHERE request_open IS NOT NULL ORDER BY exchange_id').all()) {
        const opened = JSON.parse(row.request_open)
        if (selectedJobs !== null && !selectedJobs.has(opened.lease.jobId)) continue
        assert.ok(!identities.has(row.exchange_id), 'duplicate model exchange across Device stores')
        identities.add(row.exchange_id)
        assert.equal(opened.modelExchangeId, row.exchange_id)
        const request = decodeReceipt(opened.request)
        const actualModels = []
        let terminal = null
        let failure = null
        for (const chunk of JSON.parse(row.chunks ?? '[]')) {
          assert.equal(chunk.modelExchangeId, row.exchange_id)
          if (chunk.error) {
            assert.equal(chunk.isFinal, true, 'model failure must be terminal')
            assert.equal(terminal, null, 'multiple terminal model receipts')
            failure = retainedModelFailure(chunk.error)
            terminal = { type: 'error' }
          }
          if (!chunk.payload) continue
          const frame = decodeReceipt(chunk.payload)
          if (chunk.error) failure = retainedModelFailure(chunk.error, { metadata: frame })
          if (frame.type === 'server_model') actualModels.push(frame.model)
          if (frame.type === 'completed' || frame.type === 'error') {
            assert.equal(terminal, null, 'multiple terminal model receipts')
            terminal = frame
            if (frame.type === 'error') failure = retainedModelFailure(frame.error,
              { canonical: true, metadata: { ...frame, ...frame.error } })
          }
        }
        const usage = measuredUsage(terminal?.tokenUsage ?? null)
        const call = { exchangeId: row.exchange_id, jobId: opened.lease.jobId,
          workerSessionId: opened.workerSessionId, provider: request.provider,
          requestedModel: request.request.model, reasoningEffort: request.request.reasoning?.effort ?? null,
          actualModels, terminalType: terminal?.type ?? null,
          ...(failure === null ? {} : { failure }),
          usage,
          ...(hasProviderAttempts ? { providerAttempts: readProviderAttempts(database, opened) } : {}),
          requestSha256: digest(row.request_open), chunksSha256: digest(row.chunks ?? '[]'), source: path }
        exchanges.set(row.exchange_id, call)
        calls.push(call)
      }
      for (const kind of ['context', 'judge']) {
        const table = `jev_${kind}_exchanges`
        if (!database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?").get(table)) continue
        for (const row of database.prepare(`SELECT operation_id, request_json, result FROM ${table} ORDER BY operation_id`).all()) {
          const exchangeId = row.operation_id.split(':')[1]
          const call = exchanges.get(exchangeId)
          if (selectedJobs !== null && !call) continue
          assert.ok(call, 'JEV receipt must belong to a retained model exchange')
          const run = row.result === null ? null : JSON.parse(row.result)
          const observation = run?.observation
          jev.push({ operationId: row.operation_id, exchangeId, jobId: call.jobId, kind,
            provider: observation?.providerId ?? null, requestedModel: observation?.modelId ?? null,
            actualModel: observation?.resolvedModelId ?? null,
            inputTokens: observation?.inputTokens ?? null, outputTokens: observation?.outputTokens ?? null,
            failureCount: run?.failures?.length ?? null,
            requestSha256: row.request_json === null ? null : digest(row.request_json),
            resultSha256: row.result === null ? null : digest(row.result), source: path })
        }
      }
    } finally {
      database.close()
    }
  }
  for (const path of (deviceDirectory === null ? [] : globSync('**/worker-codex.sqlite3', { cwd: deviceDirectory }).map(path => join('device-data', path))).sort()) {
    const database = new DatabaseSync(join(directory, path), { readOnly: true })
    try {
      database.exec('PRAGMA busy_timeout=5000')
      if (!database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='performance_projection'").get()) continue
      const projections = database.prepare('SELECT run_key, record_json FROM performance_projection ORDER BY run_key').all()
      const operationsTable = database.prepare("SELECT 1 FROM sqlite_master WHERE type='view' AND name='performance_operation_accounted'").get()
        ? 'performance_operation_accounted' : 'performance_operation'
      const operations = database.prepare(`SELECT run_key, operation_kind, completed, duration_millis,
        actual_cost_microunits FROM ${operationsTable} ORDER BY run_key, operation_kind, operation_id`).all()
      for (const projection of projections) {
        if (selectedJobs !== null) {
          if (!database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) continue
          const core = database.prepare('SELECT record_json FROM codex_run WHERE run_key = ?').get(projection.run_key)
          if (!core || !selectedJobs.has(JSON.parse(Buffer.from(core.record_json).toString('utf8')).job.jobId)) continue
        }
        const stored = JSON.parse(Buffer.from(projection.record_json).toString('utf8'))
        const report = stored.report
        for (const key of ['primaryModelWaitMs', 'totalRuntimeMs']) {
          assert.ok(Number.isSafeInteger(report?.[key]) && report[key] >= 0, 'invalid retained performance time')
        }
        const selected = operations.filter(operation => operation.run_key === projection.run_key)
        const duration = kind => {
          const matching = selected.filter(operation => operation.operation_kind === kind)
          return matching.every(operation => Number.isSafeInteger(operation.duration_millis)
            && operation.duration_millis >= 0)
            ? matching.reduce((sum, operation) => sum + operation.duration_millis, 0) : null
        }
        const modelOperations = selected.filter(operation => operation.operation_kind === 'primary_model')
        const costComplete = modelOperations.length > 0 && modelOperations.every(operation =>
          operation.completed === 1 && Number.isSafeInteger(operation.actual_cost_microunits)
            && operation.actual_cost_microunits >= 0)
        performance.push({ runKey: projection.run_key, source: path,
          projectionSha256: digest(Buffer.from(projection.record_json)),
          modelCalls: modelOperations.length,
          totalRuntimeMs: report.totalRuntimeMs,
          modelWaitMs: report.primaryModelWaitMs,
          toolMs: duration('tool'),
          actualCostMicros: costComplete
            ? modelOperations.reduce((sum, operation) => sum + operation.actual_cost_microunits, 0) : null })
      }
    } finally {
      database.close()
    }
  }
  const measuredCalls = calls.map(actualUsage)
  const completeUsage = calls.length > 0 && measuredCalls.every(call => call.complete)
    && jev.every(call => call.inputTokens !== null && call.outputTokens !== null && call.failureCount === 0)
  const evidence = { calls, jev, performance, completeUsage,
    totalTokens: completeUsage ? measuredCalls.flatMap(call => call.rows).reduce((sum, usage) => sum + usage.totalTokens, 0)
      + jev.reduce((sum, call) => sum + call.inputTokens + call.outputTokens, 0) : null,
    actualCost: null, externalVerdict: null, externalScore: null }
  return evidence
}

export function exportDeviceExecutionReceipts(directory, evidenceDirectory = directory, scope) {
  const evidence = readDeviceExecutionReceipts(directory, scope)
  const bytes = `${JSON.stringify(evidence, null, 2)}\n`
  retain(join(evidenceDirectory, 'execution-receipts.json'), bytes)
  return { evidence, sha256: digest(bytes) }
}

// Export retained Git objects, never the mutable Worker checkout or model text.
export function exportDeviceCandidate(directory, taskInputPath, evidenceDirectory = directory) {
  const reportBytes = readFileSync(join(evidenceDirectory, 'device-task-result.json'))
  const report = JSON.parse(reportBytes)
  const candidate = report.delivery?.detail?.currentCandidate
  assert.ok(candidate && objectId.test(candidate.candidateCommitId) && objectId.test(candidate.candidateTreeId))
  const commit = candidate.candidateCommitId
  assert.equal(report.candidateRef, `refs/winwincode/candidates/${commit}`)
  assert.equal(candidate.candidateRef, report.candidateRef)
  const task = readFileSync(taskInputPath)
  assert.equal(digest(task), report.taskInputDigest, 'task input changed after execution')
  const seal = readFileSync(join(directory, 'product-source-seal.json'))
  const objects = join(directory, 'server-data/artifacts/objects/sha256')
  let artifactBytes
  let artifactPath
  let bundle
  for (const path of globSync('*/*', { cwd: objects })) {
    const bytes = readFileSync(join(objects, path))
    if (bytes[0] !== 123) continue
    let artifact
    try { artifact = JSON.parse(bytes) } catch { continue }
    if (artifact.candidateCommitId !== commit) continue
    assert.equal(digest(bytes), path.split('/').join(''), 'retained artifact digest mismatch')
    assert.equal(artifact.schemaVersion, 2)
    const decoded = Buffer.from(artifact.bundleBase64, 'base64')
    assert.equal(decoded.toString('base64'), artifact.bundleBase64, 'invalid candidate bundle encoding')
    assert.equal(`sha256:${digest(decoded)}`, artifact.bundleDigest)
    if (bundle) assert.deepEqual(decoded, bundle, 'conflicting candidate artifacts')
    bundle = decoded
    artifactBytes = bytes
    artifactPath = join(objects, path)
  }
  assert.ok(bundle, 'candidate has no retained Git bundle')
  const output = join(evidenceDirectory, 'submission-evidence', commit)
  mkdirSync(output, { recursive: true, mode: 0o700 })
  const scratch = mkdtempSync(join(output, '.verify-'))
  let parents
  const files = []
  try {
    const gitBytes = (...args) => execFileSync('git', args, { stdio: ['ignore', 'pipe', 'pipe'],
      env: { PATH: process.env.PATH, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null' } })
    const git = (...args) => gitBytes(...args).toString('utf8').trim()
    const repository = join(scratch, 'candidate.git')
    const bundlePath = join(scratch, 'candidate.bundle')
    writeFileSync(bundlePath, bundle, { mode: 0o600 })
    git('init', '--bare', '--quiet', repository)
    git('--git-dir', repository, 'bundle', 'verify', bundlePath)
    git('--git-dir', repository, 'bundle', 'unbundle', bundlePath)
    assert.equal(git('--git-dir', repository, 'rev-parse', `${commit}^{commit}`), commit)
    assert.equal(git('--git-dir', repository, 'rev-parse', `${commit}^{tree}`), candidate.candidateTreeId)
    parents = git('--git-dir', repository, 'show', '-s', '--format=%P', commit).split(' ').filter(Boolean)
    for (const row of gitBytes('--git-dir', repository, 'ls-tree', '-r', '-z', commit).toString('utf8').split('\0').filter(Boolean)) {
      const separator = row.indexOf('\t')
      assert.ok(separator > 0)
      const [mode, kind, objectId] = row.slice(0, separator).split(' ')
      const file = { path: row.slice(separator + 1), mode, kind, objectId }
      if (kind === 'blob') {
        const bytes = gitBytes('--git-dir', repository, 'cat-file', 'blob', objectId)
        const utf8 = bytes.toString('utf8')
        Object.assign(file, Buffer.from(utf8).equals(bytes) ? { encoding: 'utf8', content: utf8 }
          : { encoding: 'base64', content: bytes.toString('base64') })
      }
      files.push(file)
    }
  } finally {
    rmSync(scratch, { recursive: true, force: true })
  }
  const receipts = exportDeviceExecutionReceipts(directory, evidenceDirectory)
  const candidateFiles = `${JSON.stringify({ commit, tree: candidate.candidateTreeId, files }, null, 2)}\n`
  const manifest = {
    candidateFilesSha256: digest(candidateFiles),
    executionReceiptsSha256: receipts.sha256,
    candidate, parents, productComplete: report.complete === true,
    productSessionId: report.productSessionId, deliveryId: report.deliveryId,
    configuration: report.benchmarkConfiguration, modelRoute: report.modelRoute,
    externalVerdict: null, externalScore: null,
    bundleSha256: digest(bundle), taskInputSha256: digest(task), productSourceSealSha256: digest(seal),
    originalReportSha256: digest(reportBytes), artifactSha256: digest(artifactBytes),
    artifactPath: relative(directory, artifactPath),
  }
  retain(join(output, 'execution-receipts.json'), readFileSync(join(evidenceDirectory, 'execution-receipts.json')))
  retain(join(output, 'candidate.bundle'), bundle)
  retain(join(output, 'candidate-files.json'), candidateFiles)
  retain(join(output, 'task-input.json'), task)
  retain(join(output, 'product-source-seal.json'), seal)
  retain(join(output, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`)
  return manifest
}
