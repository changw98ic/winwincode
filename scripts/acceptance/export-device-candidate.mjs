import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, globSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
import { join, relative } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { retainedModelFailure, retainedNetworkFailure, retainedJevFailure, retainedStopReason } from '../lib/device-model-failures.mjs'

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

// Only exact active jobs can contribute pending Core approvals. Return IDs,
// never the private StoredRun payload, prompts, or command details.
export function readDevicePendingApprovalIds(directory, runs) {
  const active = runs.filter(run => ['leased', 'running'].includes(run.state))
  if (active.length === 0 || !existsSync(join(directory, 'device-data'))) return []
  const deviceDirectory = realpathSync(join(directory, 'device-data'))
  const pending = new Set()
  for (const path of globSync('**/worker-codex.sqlite3', { cwd: deviceDirectory }).sort()) {
    const database = new DatabaseSync(join(deviceDirectory, path), { readOnly: true })
    try {
      database.exec('PRAGMA busy_timeout=5000')
      for (const row of database.prepare(`SELECT a.approval_id, r.record_json
        FROM approval_operation a JOIN codex_run r ON r.run_key = a.run_key
        WHERE a.state = 'pending'`).all()) {
        const record = JSON.parse(Buffer.from(row.record_json).toString('utf8'))
        if (record.terminal != null || ['terminal', 'outcomeRetained'].includes(record.phase)) continue
        if (active.some(run => run.executionJobId === record.job?.jobId
          && run.codexThreadId === record.canonicalThreadId)) pending.add(row.approval_id)
      }
    } finally { database.close() }
  }
  return [...pending].sort()
}

// Export identity/accounting facts only; prompts, tool output and credentials stay private.
export function readDeviceExecutionReceipts(directory, scope) {
  const selectedJobs = taskReceiptJobs(directory, scope)
  const deviceDirectory = existsSync(join(directory, 'device-data')) ? realpathSync(join(directory, 'device-data')) : null
  const calls = []
  const jev = []
  const performance = []
  const interactionTimeouts = []
  const identities = new Set()
  for (const path of (deviceDirectory === null ? [] : globSync('**/providers.sqlite3', { cwd: deviceDirectory }).map(path => join('device-data', path))).sort()) {
    const database = new DatabaseSync(join(directory, path), { readOnly: true })
    try {
      database.exec('PRAGMA busy_timeout=5000')
      const exchanges = new Map()
      const hasModelDiagnostics = !!database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='model_attempt_diagnostics'").get()
      const hasJevDiagnostics = !!database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='jev_attempt_diagnostics'").get()
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
          if (frame.type === 'server_model') actualModels.push(frame.model)
          if (frame.type === 'completed' || frame.type === 'error') {
            assert.equal(terminal, null, 'multiple terminal model receipts')
            terminal = frame
            if (frame.type === 'error') failure = retainedModelFailure(frame.error, { canonical: true })
          }
        }
        const usage = terminal?.tokenUsage ?? null
        if (usage !== null) for (const key of ['input_tokens', 'output_tokens', 'total_tokens']) {
          assert.ok(Number.isSafeInteger(usage[key]) && usage[key] >= 0, 'invalid measured usage')
        }
        const attempts = hasModelDiagnostics ? database.prepare(`SELECT sequence,policy_attempt,started_ms,finished_ms,outcome,failure_json,stop_reason
          FROM model_attempt_diagnostics WHERE exchange_id=? ORDER BY sequence`).all(row.exchange_id).map(attempt => ({
            diagnosticRef: `model-attempt:${digest(row.exchange_id)}:${attempt.sequence}`,
            sequence: attempt.sequence, policyAttempt: attempt.policy_attempt,
            startedMs: attempt.started_ms, finishedMs: attempt.finished_ms,
            outcome: ['in_flight', 'accepted', 'failed'].includes(attempt.outcome) ? attempt.outcome : null,
            network: attempt.failure_json === null ? null : retainedNetworkFailure(JSON.parse(attempt.failure_json)),
            stopReason: retainedStopReason(attempt.stop_reason),
          })) : null
        if (failure !== null && attempts !== null) failure = { ...failure, attempts,
          stopReason: attempts.at(-1)?.stopReason ?? null }
        const call = { exchangeId: row.exchange_id, jobId: opened.lease.jobId,
          workerSessionId: opened.workerSessionId, provider: request.provider,
          requestedModel: request.request.model, reasoningEffort: request.request.reasoning?.effort ?? null,
          actualModels, terminalType: terminal?.type ?? null, attempts,
          ...(failure === null ? {} : { failure }),
          usage: usage === null ? null : {
            inputTokens: usage.input_tokens, outputTokens: usage.output_tokens,
            totalTokens: usage.total_tokens,
            cachedTokens: usage.cached_input_tokens ?? null,
            cacheWriteTokens: usage.cache_write_input_tokens ?? null,
            reasoningTokens: usage.reasoning_output_tokens ?? null,
          },
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
          const retainedFailures = run?.failures ?? (hasJevDiagnostics
            ? database.prepare('SELECT failure_json FROM jev_attempt_diagnostics WHERE operation_id=? AND role=? ORDER BY sequence')
              .all(row.operation_id, kind).map(attempt => JSON.parse(attempt.failure_json)) : null)
          const failures = retainedFailures === null ? null : retainedFailures.map((failure, index) => ({
            diagnosticRef: `jev-attempt:${digest(row.operation_id)}:${index + 1}`, ...retainedJevFailure(failure),
          }))
          jev.push({ operationId: row.operation_id, exchangeId, jobId: call.jobId, kind,
            provider: observation?.providerId ?? null, requestedModel: observation?.modelId ?? null,
            actualModel: observation?.resolvedModelId ?? null,
            inputTokens: observation?.inputTokens ?? null, outputTokens: observation?.outputTokens ?? null,
            failureCount: failures?.length ?? null, failures, completed: run !== null,
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
      if (database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) {
        for (const row of database.prepare('SELECT run_key, record_json FROM codex_run ORDER BY run_key').all()) {
          const record = JSON.parse(Buffer.from(row.record_json).toString('utf8'))
          if (selectedJobs !== null && !selectedJobs.has(record.job.jobId)) continue
          for (const timeout of record.interactionTimeouts ?? []) {
            const request = timeout.request
            const kind = request.kind === 'approval.request' ? 'approval' : 'input'
            const id = kind === 'approval' ? request.approvalId : request.inputRequestId
            assert.ok(typeof id === 'string', 'interaction timeout must retain its exact request identity')
            interactionTimeouts.push({ runKey: row.run_key, jobId: record.job.jobId,
              kind, id, causeCode: 'INTERACTION_DEADLINE_EXPIRED',
              expiresAt: request.expiresAt, observedAt: timeout.observedAt,
              requestSha256: timeout.requestDigest,
              responseSubmitted: typeof timeout.appliedKernelSessionId === 'string', source: path })
          }
        }
      }
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
  const completeUsage = calls.length > 0 && calls.every(call => call.usage !== null)
    && jev.every(call => call.inputTokens !== null && call.outputTokens !== null && call.failureCount === 0)
  const evidence = { calls, jev, performance, interactionTimeouts, completeUsage,
    totalTokens: completeUsage ? calls.reduce((sum, call) => sum + call.usage.totalTokens, 0)
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
