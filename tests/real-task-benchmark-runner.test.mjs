import assert from 'node:assert/strict'
import { execFileSync, spawn, spawnSync } from 'node:child_process'
import { once } from 'node:events'
import { createHash } from 'node:crypto'
import { exportDeviceCandidate, exportDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import { access, chmod, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:https'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { assertDeviceBenchmarkRunning } from '../scripts/device-production-fixture.mjs'
import { driveDelivery } from '../scripts/run-api-production-vertical.mjs'
import { openBenchmarkLedger } from '../scripts/benchmark-ledger.mjs'
import { benchmarkAggregationInput, executeDeviceBenchmark, recoverBenchmarkDeviceCell,
  runBenchmarkDeviceAggregation, terminalBenchmarkDeviceFailure,
  terminalDeviceFailure } from '../scripts/benchmark-device-adapter.mjs'
import { runDeviceTaskVertical, fusionDeviceProviders, inspectUnresolvedDeviceTasks, benchmarkDeviceEnvironment,
  expiredCrashedDeviceWorkRun, expiredDeviceWorkRunLease,
  loadDeviceProviderEnvironment } from '../scripts/run-device-task-vertical.mjs'

import {
  aggregateBenchmarkReport,
  benchmarkAggregationDigest,
  benchmarkConfiguration,
  buildBenchmarkPlan,
  createToolRequestGuard,
  executeBenchmarkCell,
  executeFormalBenchmark,
  executeToolRequest,
  normalizeToolRequestIdentity,
  recoverBenchmarkCell,
  runBenchmarkPlan,
  validateFrozenTaskSource,
} from '../scripts/run-real-task-benchmark.mjs'

test('expired crashed Device Worker is diagnosed from both durable authorities', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-worker-crash-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await mkdir(resolve(directory, 'device-data'))
  await mkdir(resolve(directory, 'server-data'))
  const device = new DatabaseSync(resolve(directory, 'device-data/device-client.sqlite3'))
  const server = new DatabaseSync(resolve(directory, 'server-data/control-plane.sqlite3'))
  try {
    device.exec(`CREATE TABLE worker_process_registry (worker_session_id TEXT, worker_id TEXT,
      worker_instance_id TEXT, state TEXT, exit_code INTEGER, last_observed_at TEXT);
      INSERT INTO worker_process_registry VALUES ('wsn_one','wrk_one','wki_one','crashed',1,'2026-01-01T00:01:00Z');`)
    server.exec(`CREATE TABLE execution_leases (job_id TEXT, lease_id TEXT, worker_id TEXT,
      worker_instance_id TEXT, issued_at TEXT, expires_at TEXT);
      CREATE TABLE execution_lease_terminals (lease_id TEXT);
      INSERT INTO execution_leases VALUES ('job_one','lse_one','wrk_one','wki_one',
        '2026-01-01T00:00:00Z','2026-01-01T00:10:00Z');`)
    assert.equal(expiredCrashedDeviceWorkRun(directory, 'run_one', 'wsn_one',
      Date.parse('2026-01-01T00:09:00Z')), null)
    const crash = expiredCrashedDeviceWorkRun(directory, 'run_one', 'wsn_one',
      Date.parse('2026-01-01T00:11:00Z'))
    assert.equal(crash?.workRunId, 'run_one')
    assert.equal(crash?.leaseId, 'lse_one')
    device.exec("UPDATE worker_process_registry SET state = 'running', exit_code = NULL")
    assert.equal(expiredCrashedDeviceWorkRun(directory, 'run_one', 'wsn_one',
      Date.parse('2026-01-01T00:11:00Z')), null)
    const active = expiredDeviceWorkRunLease(directory, 'run_one', 'wsn_one',
      Date.parse('2026-01-01T00:11:00Z'), 'job_one')
    assert.equal(active?.workerState, 'running')
    assert.equal(active?.leaseId, 'lse_one')
    assert.equal(expiredDeviceWorkRunLease(directory, 'run_one', 'wsn_one',
      Date.parse('2026-01-01T00:11:00Z'), 'job_other'), null)
    server.exec("INSERT INTO execution_lease_terminals VALUES ('lse_one')")
    assert.equal(expiredDeviceWorkRunLease(directory, 'run_one', 'wsn_one',
      Date.parse('2026-01-01T00:11:00Z')), null)
    assert.equal(expiredCrashedDeviceWorkRun(directory, 'run_one', 'wsn_one',
      Date.parse('2026-01-01T00:11:00Z')), null)
  } finally { device.close(); server.close() }
})

test('device recovery settles only complete terminal failures and leaves product work unresolved', () => {
  const observation = { delivery: { status: 'failed', attention: [] },
    workRunAggregate: { items: [{ state: 'failed' }], runs: [{ state: 'failed' }] } }
  assert.deepEqual(terminalDeviceFailure(observation), { code: 'DEVICE_PRODUCT_FAILED', status: 'failed' })
  assert.deepEqual(terminalDeviceFailure({ delivery: { status: 'cancelled', attention: [] },
    workRunAggregate: { items: [{ state: 'done' }, { state: 'cancelled' }],
      runs: [{ state: 'settled' }, { state: 'cancelled' }] } }),
  { code: 'DEVICE_PRODUCT_CANCELLED', status: 'cancelled' })
  for (const changed of [
    { delivery: { status: 'failed', attention: [] }, workRunAggregate: { items: [{ state: 'failed' }, { state: 'ready' }], runs: [{ state: 'failed' }] } },
    { delivery: { status: 'failed', attention: [{ status: 'open' }] }, workRunAggregate: { items: [{ state: 'failed' }], runs: [{ state: 'failed' }] } },
    { delivery: { status: 'failed', attention: [] }, workRunAggregate: { items: [{ state: 'failed' }], runs: [{ state: 'running' }] } },
    { delivery: { status: 'failed', attention: [] }, workRunAggregate: { items: [{ state: 'failed' }], runs: [{ state: 'candidate_ready' }] } },
    { delivery: { status: 'candidate_ready', attention: [] }, workRunAggregate: { items: [{ state: 'candidate_ready' }], runs: [{ state: 'candidate_ready' }] } },
    { delivery: { status: 'candidate_ready', attention: [] }, workRunAggregate: { items: [{ state: 'candidate_ready' }], runs: [{ state: 'candidate_ready' }, { state: 'running' }] } },
  ]) assert.equal(terminalDeviceFailure(changed), null)
  assert.deepEqual(terminalDeviceFailure({ delivery: { status: 'candidate_ready', attention: [] },
    workRunAggregate: { items: [{ state: 'candidate_ready' }], runs: [{ state: 'candidate_ready' }, { state: 'failed' }] } }),
  { code: 'DEVICE_PRODUCT_STALLED', status: 'candidate_ready' })
  const waitingHuman = { delivery: { status: 'waiting_human', attention: [{ status: 'open', blocking: true }] },
    workRunAggregate: { items: [{ state: 'candidate_ready' }], runs: [{ state: 'settled' }] } }
  assert.equal(terminalDeviceFailure(waitingHuman), null)
  assert.deepEqual(terminalBenchmarkDeviceFailure(waitingHuman),
    { code: 'DEVICE_TASK_ATTENTION', status: 'waiting_human' })
  assert.equal(terminalBenchmarkDeviceFailure({ ...waitingHuman,
    workRunAggregate: { ...waitingHuman.workRunAggregate, runs: [{ state: 'running' }] } }), null)
})

test('a returned terminal Device failure remains a failed benchmark row', async () => {
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) }).cells[0]
  const failure = { code: 'DEVICE_PRODUCT_STALLED', status: 'candidate_ready' }
  const model = { status: 'failed', failure, productComplete: false, candidate: null,
    externalScore: null, externalVerdict: null }
  const result = await executeBenchmarkCell(cell, { runModel: async () => model })
  assert.equal(result.status, 'failed')
  assert.deepEqual(result.failure, failure)
  assert.equal(result.model, model)
})

test('a returned failed Fusion aggregation remains a failed cell after retaining all member outcomes', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) })
  const cell = plan.cells.find(value => value.configurationId === 'main-C' && value.comparison === 'fusion-4')
  const failure = { code: 'DEVICE_PRODUCT_FAILED', status: 'failed' }
  const result = await executeBenchmarkCell(cell, {
    runModel: async request => ({ status: 'completed', provider: request.provider }),
    aggregate: async () => ({ status: 'failed', failure }),
  })
  assert.equal(result.status, 'failed')
  assert.deepEqual(result.failure, failure)
  assert.equal(result.members.length, 4)
  assert.equal(result.aggregateReceipt.callCount, 1)
  assert.deepEqual(result.productOutcome, result.aggregate)
})

test('a settled attention failure retains its Fusion member and runs the remaining providers', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) })
  const cell = plan.cells.find(value => value.configurationId === 'main-C' && value.comparison === 'fusion-4')
  const called = []
  const calls = []
  const result = await executeBenchmarkCell(cell, {
    runModel: async request => {
      called.push(request.provider)
      if (called.length === 1) return { status: 'failed', provider: request.provider,
        failure: { code: 'DEVICE_TASK_ATTENTION', status: 'waiting_human' } }
      return { status: 'completed', provider: request.provider }
    },
    aggregate: async request => ({ status: 'failed', failure: { code: 'DEVICE_TASK_ATTENTION' },
      memberCount: request.members.length }),
  }, { recordCall: call => calls.push(call) })
  assert.equal(called.length, 4)
  assert.equal(result.members[0].failure.code, 'DEVICE_TASK_ATTENTION')
  assert.equal(result.aggregate.memberCount, 4)
  assert.equal(calls.length, 5)
  assert.equal(calls[0].status, 'returned')
})

test('the durable benchmark row keeps a returned product failure failed and never reexecutes it', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-returned-failure-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) }).cells[0]
  const plan = { cells: [cell] }
  const failure = { code: 'DEVICE_PRODUCT_FAILED', status: 'failed' }
  const model = { status: 'failed', failure, productComplete: false,
    externalScore: null, externalVerdict: null }
  const ledgerPath = resolve(directory, 'ledger.sqlite3')
  const options = { ledgerPath, experimentBinding: { experimentId: 'returned-product-failure' },
    executeCell: (value, runner) => executeBenchmarkCell(value, { runModel: async () => model }, runner) }
  const first = await runBenchmarkPlan(plan, options)
  assert.equal(first.records[0].status, 'failed')
  assert.deepEqual(first.records[0].failure, failure)
  assert.deepEqual(first.records[0].productOutcome, model)
  assert.equal(first.records[0].verdict, null)
  assert.equal(first.records[0].score, null)
  assert.equal(first.records[0].calls[0].status, 'returned')
  const reopened = await runBenchmarkPlan(plan, { ...options,
    executeCell: () => assert.fail('durable product failure must not be rerun') })
  assert.deepEqual(reopened.records[0], first.records[0])
})

test('recovery records an authoritative terminal product failure without replaying its model call', async () => {
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) }).cells[0]
  const launch = { callId: `${cell.runId}:model`, directory: '/private/retained',
    productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001' }
  let inspections = 0
  const result = await recoverBenchmarkCell(cell, { calls: [], launches: [launch] }, async (_request, target) => {
    inspections += 1
    assert.deepEqual(target, launch)
    return { status: 'failed', failure: { code: 'DEVICE_PRODUCT_FAILED', status: 'failed' },
      productComplete: false, candidate: null, externalScore: null, externalVerdict: null }
  })
  assert.equal(inspections, 1)
  assert.equal(result.status, 'failed')
  assert.deepEqual(result.failure, { code: 'DEVICE_PRODUCT_FAILED', status: 'failed' })
  assert.equal(result.productOutcome.productComplete, false)
  assert.equal(result.productOutcome.externalScore, null)
  assert.equal(result.calls[0].status, 'returned')
})

test('recovery uses the last cursor-matched terminal projection when the registered local Server has stopped', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-stopped-server-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) }).cells[0]
  const callId = `${cell.runId}:model`
  const launch = { callId, directory, deliveryId: 'dlv_01J00000000000000000000001',
    productSessionId: 'psn_01J00000000000000000000001' }
  const inputBytes = Buffer.from('{"task":"original"}\n')
  const digest = createHash('sha256').update(inputBytes).digest('hex')
  const cursor = { deliveryId: launch.deliveryId, deliveryRevision: 4, token: 'cursor-4' }
  const configuration = Object.fromEntries(['configurationId', 'track', 'fusion', 'jev', 'jevContext', 'jevJudge']
    .map(key => [key, cell[key]]))
  await mkdir(resolve(directory, 'device-data/providers'), { recursive: true })
  await mkdir(resolve(directory, 'device-data/worker-sessions'), { recursive: true })
  await writeFile(resolve(directory, 'task-input.json'), inputBytes, { mode: 0o600 })
  await writeFile(resolve(directory, 'task-source-binding.json'), JSON.stringify({ runId: cell.runId, callId,
    taskId: cell.taskId, taskInputSha256: digest }), { mode: 0o600 })
  await writeFile(resolve(directory, 'device-task-result.json'), JSON.stringify({ taskInputDigest: digest,
    productSessionId: launch.productSessionId, deliveryId: launch.deliveryId, modelRoute: { modelId: cell.comparison },
    benchmarkConfiguration: configuration, complete: false, delivery: {
      detail: { deliveryId: launch.deliveryId, status: 'failed', readCursor: cursor, attention: [], currentCandidate: null },
      workRunAggregate: { deliveryId: launch.deliveryId, readCursor: cursor,
        items: [{ state: 'failed' }], runs: [{ id: 'failed-executor', state: 'failed' }] },
    } }), { mode: 0o600 })
  const productSeal = Buffer.from('{"source":"frozen"}\n')
  await writeFile(resolve(directory, 'product-source-seal.json'), productSeal, { mode: 0o600 })
  const database = new DatabaseSync(resolve(directory, 'device-data/providers/providers.sqlite3'))
  database.exec('CREATE TABLE exchanges(exchange_id TEXT,request_open TEXT,chunks TEXT)')
  database.close()
  execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-days', '1',
    '-subj', '/CN=control.localhost', '-addext', 'subjectAltName=DNS:control.localhost',
    '-keyout', resolve(directory, 'fixture-key.pem'), '-out', resolve(directory, 'fixture-cert.pem')], { stdio: 'ignore' })
  await writeFile(resolve(directory, 'server-endpoint.json'), JSON.stringify({
    controlUrl: 'https://127.0.0.1:1', origin: 'https://api.localhost:1',
  }), { mode: 0o600 })

  const result = await recoverBenchmarkDeviceCell(cell, { calls: [], launches: [launch] }, {
    productSourceSealSha256: createHash('sha256').update(productSeal).digest('hex'),
  })
  assert.equal(result.status, 'failed')
  assert.equal(result.failure.code, 'DEVICE_PRODUCT_FAILED')
  assert.equal(result.productOutcome.externalScore, null)
  assert.equal(result.productOutcome.externalVerdict, null)
})

test('recovery finalizes retained calls after process death and fences stale writers without reexecution', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-recovery-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i}`) }).cells[0]
  const plan = { cells: [cell] }
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'recovery' } }
  const child = spawnSync(process.execPath, ['--input-type=module', '-e', `
    const { runBenchmarkPlan } = await import(process.argv[1]);
    await runBenchmarkPlan(JSON.parse(process.argv[2]), { ...JSON.parse(process.argv[3]),
      executeCell: (cell, context) => {
        context.recordCall({ callId: cell.runId + ':model', status: 'returned',
          result: { status: 'completed', candidate: 'retained' } });
        process.exit(86);
      }
    });
  `, new URL('../scripts/run-real-task-benchmark.mjs', import.meta.url).href, JSON.stringify(plan), JSON.stringify(options)],
  { encoding: 'utf8' })
  assert.equal(child.status, 86, child.stderr)
  const executeCell = () => assert.fail('recovery must not execute a model or product command')
  const result = await runBenchmarkPlan(plan, { ...options, executeCell, recoverCell: recoverBenchmarkCell })
  assert.equal(result.records[0].model.candidate, 'retained')
  assert.equal(result.records[0].score, null)
  assert.equal(result.records[0].recovery.originalCalls.length, 1)
  assert.deepEqual(await runBenchmarkPlan(plan, { ...options, executeCell }), result)

  const store = openBenchmarkLedger(resolve(directory, 'race.sqlite3'), 'race', [cell])
  try {
    const { token } = store.claim(0)
    let observed
    assert.throws(() => store.claim(0), error => { observed = error; return true })
    store.recordCall(0, token, { callId: `${cell.runId}:model`, status: 'returned', result: {} })
    assert.throws(() => store.recover(0, observed, { runId: cell.runId }), { code: 'LEDGER_RECOVERY_CONFLICT' })
    assert.throws(() => store.claim(0), error => { observed = error; return true })
    store.recover(0, observed, { runId: cell.runId, status: 'failed', score: null })
    assert.throws(() => store.finish(0, token, { runId: cell.runId }), { code: 'LEDGER_COMPLETION_CONFLICT' })
    assert.throws(() => store.recordCall(0, token, { callId: `${cell.runId}:late`, status: 'returned' }), { code: 'LEDGER_CLAIM_MISMATCH' })
  } finally { store.close() }
})

test('recovery leaves incomplete Fusion unresolved and propagates retained stops', async () => {
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i}`) }).cells[4]
  const calls = []
  const expected = await executeBenchmarkCell(cell, {
    runModel: request => ({ provider: request.provider }), aggregate: request => ({ digest: request.inputDigest }),
  }, { recordCall: call => calls.push(call) })
  const recovered = await recoverBenchmarkCell(cell, { calls, launches: [] })
  assert.deepEqual(recovered.aggregateReceipt, expected.aggregateReceipt)
  assert.deepEqual(recovered.members, expected.members)
  await assert.rejects(recoverBenchmarkCell(cell, { calls: calls.slice(0, 4), launches: [] }), { code: 'LEDGER_RUN_UNRESOLVED' })
  const stopped = await recoverBenchmarkCell(cell, { launches: [], calls: [{
    callId: calls[0].callId, status: 'failed', failure: { code: 'STUCK_TOOL_REPEAT_LIMIT' },
  }] })
  assert.equal(stopped.status, 'failed')
  assert.equal(stopped.termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  await assert.rejects(recoverBenchmarkCell(cell, { launches: [], calls: [{
    callId: calls[0].callId, status: 'failed', failure: { code: 'BENCHMARK_EVIDENCE_FAILED' },
  }] }), { code: 'LEDGER_RUN_UNRESOLVED' })
})

test('one failed Fusion member does not omit siblings and recovery reproduces the same aggregation input', async () => {
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i}`) }).cells[4]
  const providers = []
  const calls = []
  let aggregations = 0
  const result = await executeBenchmarkCell(cell, {
    runModel: request => {
      providers.push(request.provider)
      if (providers.length === 1) throw Object.assign(new Error('retained provider failure'), { code: 'MODEL_FAILED' })
      return { provider: request.provider, status: 'completed' }
    },
    aggregate: request => {
      aggregations += 1
      assert.equal(request.members.length, 4)
      assert.equal(request.members[0].failure.code, 'MODEL_FAILED')
      return { status: 'completed', inputDigest: request.inputDigest }
    },
  }, { recordCall: call => calls.push(call) })
  assert.equal(providers.length, 4)
  assert.equal(new Set(providers).size, 4)
  assert.equal(aggregations, 1)
  assert.deepEqual(calls.map(call => call.status), ['failed', 'returned', 'returned', 'returned', 'returned'])
  const recovered = await recoverBenchmarkCell(cell, { calls, launches: [] })
  assert.deepEqual(recovered.members, result.members)
  assert.deepEqual(recovered.aggregateReceipt, result.aggregateReceipt)
  let attempts = 0
  await assert.rejects(executeBenchmarkCell(cell, {
    runModel: async (_request, runner) => {
      attempts += 1
      try { await runner.registerLaunch({}) } catch { /* May not swallow persistence failure. */ }
      return { status: 'completed' }
    },
    aggregate: () => assert.fail('storage failure must prevent aggregation'),
  }, { registerLaunch: () => { throw Object.assign(new Error('disk unavailable'), { code: 'ENOSPC' }) } }), { code: 'ENOSPC' })
  assert.equal(attempts, 1)
})

test('product aggregation binds four independent frozen candidates and rejects reused or changed member inputs', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-aggregation-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i}`) })
  const cell = plan.cells[4]
  const providers = plan.cells.slice(0, 4).map(item => item.comparison)
  const sha = bytes => createHash('sha256').update(bytes).digest('hex')
  const members = []
  for (const [index, provider] of providers.entries()) {
    const root = resolve(directory, String(index))
    await mkdir(root)
    const candidate = { candidateCommitId: String(index + 1).repeat(40), candidateTreeId: 'a'.repeat(40) }
    const files = JSON.stringify({ commit: candidate.candidateCommitId, tree: candidate.candidateTreeId,
      files: [{ path: 'main.py', encoding: 'utf8', content: `print(${index})\n` }] })
    const binding = JSON.stringify({ runId: cell.runId, taskId: cell.taskId, callId: `${cell.runId}:member:${provider}`,
      taskInputSha256: 'b'.repeat(64), source: { sourceDigest: 'c'.repeat(64) } })
    const manifest = JSON.stringify({ candidate, productComplete: true, taskInputSha256: 'b'.repeat(64),
      modelRoute: { modelId: provider }, configuration: benchmarkConfiguration(cell.configurationId),
      candidateFilesSha256: sha(files), productSourceSealSha256: 'd'.repeat(64) })
    await writeFile(resolve(root, 'candidate-files.json'), files)
    await writeFile(resolve(root, 'manifest.json'), manifest)
    await writeFile(resolve(root, 'task-source-binding.json'), binding)
    members.push({ status: 'completed', directory: root, candidate, taskSourceBindingSha256: sha(binding),
      submissionManifest: { path: resolve(root, 'manifest.json'), sha256: sha(manifest) } })
  }
  const request = { ...cell, engine: 'fusion-engine', algorithmVersion: 'fusion-4-v1',
    callId: `${cell.runId}:aggregation`, members,
    inputDigest: benchmarkAggregationDigest(cell.runId, cell.taskId, members) }
  const input = benchmarkAggregationInput(request)
  assert.equal(input.members.length, 4)
  assert.equal(input.members[0].candidate.files[0].content, 'print(0)\n')
  assert.throws(() => benchmarkAggregationInput({ ...request, inputDigest: '0'.repeat(64) }))
  const bindingPath = resolve(members[0].directory, 'task-source-binding.json')
  const originalBinding = await readFile(bindingPath)
  const foreign = JSON.parse(originalBinding)
  foreign.runId = plan.cells[0].runId
  const foreignBytes = JSON.stringify(foreign)
  await writeFile(bindingPath, foreignBytes)
  members[0].taskSourceBindingSha256 = sha(foreignBytes)
  assert.throws(() => benchmarkAggregationInput({ ...request,
    inputDigest: benchmarkAggregationDigest(cell.runId, cell.taskId, members) }), /cannot reuse a standalone candidate/u)
  await writeFile(bindingPath, originalBinding)
  members[0].taskSourceBindingSha256 = sha(originalBinding)
  await writeFile(resolve(members[0].directory, 'candidate-files.json'), '{}')
  assert.throws(() => benchmarkAggregationInput({ ...request,
    inputDigest: benchmarkAggregationDigest(cell.runId, cell.taskId, members) }))
  await assert.rejects(runBenchmarkDeviceAggregation({ ...request,
    inputDigest: benchmarkAggregationDigest(cell.runId, cell.taskId, members) }, {}, {}), { code: 'BENCHMARK_EVIDENCE_FAILED' })
  const failures = providers.map(provider => ({ provider, callId: `${cell.runId}:member:${provider}`,
    status: 'failed', failure: { code: 'MODEL_FAILED' } }))
  await assert.rejects(runBenchmarkDeviceAggregation({ ...request, members: failures,
    inputDigest: benchmarkAggregationDigest(cell.runId, cell.taskId, failures) }, {}, {}), { code: 'FUSION_NO_SUCCESSFUL_MEMBERS' })
})

test('formal Device entry refuses an unavailable arm before creating the ledger or preparing any launch', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-preflight-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await writeFile(resolve(directory, 'prepared-inputs.json'), JSON.stringify({
    tasks: Array.from({ length: 20 }, (_, i) => ({ taskId: `task-${i}` })),
  }))
  const evidenceRoot = resolve(directory, 'must-not-exist')
  await assert.rejects(executeDeviceBenchmark({ preparedInputsDirectory: directory, evidenceRoot,
    agentSettings: { jevContext: { provider: 'context', policy: { version: 'frozen' } },
      jevJudge: 'judge', jevSettingsFile: resolve(directory, 'private-settings.json') } }),
  { code: 'BENCHMARK_CONFIGURATION_UNAVAILABLE' })
  await assert.rejects(access(evidenceRoot), { code: 'ENOENT' })
})

test('frozen task catalog produces exactly 700 unique benchmark cells', () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })

  assert.equal(plan.cells.length, 700)
  assert.equal(new Set(plan.cells.map(cell => cell.runId)).size, 700)
})

test('every experimental arm reaches both member execution and aggregation unchanged', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const cells = plan.cells.filter(cell => cell.taskId === 'task-1')
  assert.equal(cells.length, 35)
  for (const cell of cells) {
    const calls = []
    const check = async request => {
      calls.push(request)
      for (const key of ['configurationId', 'track', 'fusion', 'jev', 'jevContext', 'jevJudge']) {
        assert.equal(request[key], cell[key], `${cell.runId}: ${key}`)
      }
      return { answer: 'fixture' }
    }
    await executeBenchmarkCell(cell, { runModel: check, aggregate: check })
    assert.equal(calls.length, cell.comparison === 'fusion-4' ? 5 : 1)
  }
})

test('experimental arms distinguish context and judge and reject contradictory switches', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const arms = plan.cells.filter(cell => cell.taskId === 'task-1' && cell.comparison === 'glm-5.3-flash')
  assert.deepEqual(arms.map(({ configurationId, fusion, jevContext, jevJudge }) =>
    [configurationId, fusion, jevContext, jevJudge]), [
    ['main-A', false, false, false], ['main-B', false, true, true],
    ['main-C', true, false, false], ['main-D', true, true, true],
    ['jev-context-only', false, true, false], ['jev-judge-only', false, false, true],
    ['jev-full', false, true, true],
  ])
  for (const cell of arms) {
    for (const key of ['fusion', 'jev', 'jevContext', 'jevJudge']) {
      await assert.rejects(executeBenchmarkCell({ ...cell, [key]: !cell[key] }, {
        runModel: () => assert.fail('contradictory arm reached model execution'),
      }), error => error.code === 'BENCHMARK_CONFIGURATION_INVALID')
    }
  }
})

test('Device rejects missing experimental configuration before launch side effects', async t => {
  const root = await mkdtemp(resolve(tmpdir(), 'wwc-device-arms-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const directory = resolve(root, 'product')
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  for (const cell of plan.cells.filter(cell => cell.taskId === 'task-1'
    && ['main-B', 'main-D', 'jev-context-only', 'jev-judge-only', 'jev-full'].includes(cell.configurationId))) {
    await assert.rejects(runDeviceTaskVertical({ ...cell, directory,
      registerLaunch: () => assert.fail('unsupported arm registered a product launch'),
    }), error => error.code === 'BENCHMARK_CONFIGURATION_UNAVAILABLE')
  }
  await assert.rejects(runDeviceTaskVertical({ directory,
    registerLaunch: () => assert.fail('lost configuration registered a product launch'),
  }), error => error.code === 'BENCHMARK_CONFIGURATION_INVALID')
  for (const key of ['WWC_WORKER_FUSION', 'WWC_WORKER_JEV_CONTEXT', 'WWC_WORKER_JEV_JUDGE', 'WWC_DEVICE_JEV_SETTINGS_FILE']) {
    const previous = process.env[key]
    try {
      process.env[key] = ''
      await assert.rejects(runDeviceTaskVertical({
        ...plan.cells.find(cell => cell.configurationId === 'main-A'), directory,
        registerLaunch: () => assert.fail('ambient JEV settings registered a baseline launch'),
      }), error => error.code === 'BENCHMARK_CONFIGURATION_INVALID')
    } finally {
      if (previous === undefined) delete process.env[key]
      else process.env[key] = previous
    }
  }
  await assert.rejects(access(directory), error => error.code === 'ENOENT')
})

test('a successfully executed tool may return no value', async () => {
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) }).cells[0]
  let executions = 0
  await executeBenchmarkCell(cell, {
    runModel: async (_request, runner) => {
      await runner.requestTool({
        tool: 'write_file', target: 'answer.txt', args: {}, contentDigest: 'a'.repeat(64),
      }, async () => { executions += 1 })
      return { answer: 'written' }
    },
  })
  assert.equal(executions, 1)
})

test('an independent fusion cell calls each member once and aggregates once', async () => {
  const calls = []
  const cell = {
    ...benchmarkConfiguration('main-C'),
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
    'glm-5.3-flash',
    'mimo-v2.6-pro',
    'deepseek-flash',
    'qwen3.8-flash',
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
      ...benchmarkConfiguration('main-A'),
      runId: 'main-A:task-0001:glm-5.3-flash',
      taskId: 'task-0001',
      comparison: 'glm-5.3-flash',
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
  assert.equal(ledger.records[0].score, null)
  assert.equal(ledger.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(ledger.records[1].status, 'not_run_runner_terminated')
  assert.equal(ledger.records[1].verdict, null)
  assert.equal(ledger.records[1].score, null)
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
        ...benchmarkConfiguration('main-A'),
        runId: cell.runId,
        taskId: 'task-0001',
        comparison: 'glm-5.3-flash',
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
    ['glm-5.3-flash', 'glm-5.3-flash'],
    ['mimo-v2.6-pro', 'mimo-v2.6-pro[1m]'],
    ['deepseek-flash', 'deepseek-flash'],
    ['qwen3.8-flash', 'qwen3.8-flash'],
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
    error => error.code === 'MODEL_IDENTITY_MISMATCH' && error.message.includes('mimo-v2.6-pro'),
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
      requestedModelId: 'glm-5.3-flash',
      observedModelId: 'glm-5.3-flash',
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

test('frozen source validation binds repository, exact revision and 20 task directories', async t => {
  const repositoryRoot = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-source-'))
  t.after(() => rm(repositoryRoot, { recursive: true, force: true }))
  const taskIds = Array.from({ length: 20 }, (_, index) => `task-${index + 1}`)
  await writeFile(resolve(repositoryRoot, 'catalog.json'), JSON.stringify(taskIds.map(id => ({ id }))))
  for (const id of taskIds) {
    await mkdir(resolve(repositoryRoot, 'tasks', id), { recursive: true })
    for (const name of ['task.json', 'examples.json', 'task.md']) {
      await writeFile(resolve(repositoryRoot, 'tasks', id, name), '{}\n')
    }
  }
  await mkdir(resolve(repositoryRoot, 'tasks', 'task-1', 'starter'))
  await writeFile(resolve(repositoryRoot, 'tasks', 'task-1', 'starter', 'main.py'), 'original starter\n')
  await mkdir(resolve(repositoryRoot, 'tools'))
  await writeFile(resolve(repositoryRoot, 'tools', 'runner.py'), 'original runner\n')
  const git = (...args) => execFileSync('git', ['-C', repositoryRoot, ...args], { encoding: 'utf8' }).trim()
  git('init', '--quiet')
  git('remote', 'add', 'origin', 'https://example.invalid/benchmark-tasks')
  git('add', '.')
  git('-c', 'user.name=Benchmark test', '-c', 'user.email=benchmark@example.invalid',
    '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'fixture')
  const revision = git('rev-parse', 'HEAD')
  const source = await validateFrozenTaskSource({
    repositoryRoot,
    repositoryUrl: 'https://example.invalid/benchmark-tasks',
    revision,
  })

  assert.equal(source.taskCount, 20)
  assert.deepEqual(source.taskIds, source.catalogTaskIds)
  assert.equal(source.revision, revision)
  const validate = () => validateFrozenTaskSource({ repositoryRoot, repositoryUrl: source.repositoryUrl, revision })
  for (const path of ['tasks/task-1/starter/main.py', 'tools/runner.py']) {
    await writeFile(resolve(repositoryRoot, path), 'changed source\n')
    await assert.rejects(validate(), error => error.code === 'SOURCE_WORKTREE_DIRTY')
    git('add', path)
    await assert.rejects(validate(), error => error.code === 'SOURCE_WORKTREE_DIRTY')
    git('restore', '--source=HEAD', '--staged', '--worktree', path)
  }
  await writeFile(resolve(repositoryRoot, 'tasks/task-1/starter/extra.py'), 'extra source\n')
  await assert.rejects(validate(), error => error.code === 'SOURCE_WORKTREE_DIRTY')
  await rm(resolve(repositoryRoot, 'tasks/task-1/starter/extra.py'))
  assert.equal((await validate()).sourceDigest, source.sourceDigest)
  await writeFile(resolve(repositoryRoot, 'tools/runner.py'), 'new frozen runner\n')
  git('add', '.')
  git('-c', 'user.name=Benchmark test', '-c', 'user.email=benchmark@example.invalid',
    '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'changed fixture')
  const changed = await validateFrozenTaskSource({ repositoryRoot, repositoryUrl: source.repositoryUrl, revision: git('rev-parse', 'HEAD') })
  assert.notEqual(changed.sourceDigest, source.sourceDigest, 'execution tooling contributes to frozen source digest')
  await assert.rejects(validateFrozenTaskSource({
    repositoryRoot, repositoryUrl: source.repositoryUrl, revision: '0'.repeat(40),
  }), error => error.code === 'SOURCE_REVISION_MISMATCH')
  await assert.rejects(validateFrozenTaskSource({
    repositoryRoot, repositoryUrl: 'https://example.invalid/other', revision,
  }), error => error.code === 'SOURCE_REPOSITORY_MISMATCH')
})


test('durable runner resumes after export failure without replaying completed work and rejects changed identity', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-ledger-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = { cells: [{ runId: 'first' }, { runId: 'second' }] }
  const options = {
    ledgerPath: resolve(directory, 'ledger.sqlite3'),
    experimentBinding: { experimentId: 'test-only', sourceDigest: 'a'.repeat(64) },
  }
  const calls = []
  const executeCell = async cell => {
    calls.push(cell.runId)
    return { status: 'completed', score: 1 }
  }
  await assert.rejects(runBenchmarkPlan(plan, {
    ...options, executeCell, onRecord: () => { throw new Error('export unavailable') },
  }), /export unavailable/u)
  assert.deepEqual(calls, ['first'])
  const ledger = await runBenchmarkPlan(plan, { ...options, executeCell })
  assert.deepEqual(calls, ['first', 'second'])
  assert.equal(ledger.records.every(record => record.status === 'completed'), true)
  assert.equal(ledger.records.every(record => record.score === null && record.verdict === null), true)
  await runBenchmarkPlan(plan, { ...options, executeCell })
  assert.deepEqual(calls, ['first', 'second'])
  await assert.rejects(runBenchmarkPlan(plan, {
    ...options, executeCell, experimentBinding: { ...options.experimentBinding, sourceDigest: 'b'.repeat(64) },
  }), error => error.code === 'LEDGER_IDENTITY_MISMATCH')
  await assert.rejects(runBenchmarkPlan({ cells: [...plan.cells].reverse() }, {
    ...options, executeCell,
  }), error => error.code === 'LEDGER_IDENTITY_MISMATCH')
})

test('process death after durable claim prevents an automatic second execution', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-crash-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const ledgerPath = resolve(directory, 'ledger.sqlite3')
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', `
    const { runBenchmarkPlan } = await import(process.argv[2]);
    await runBenchmarkPlan({ cells: [{ runId: 'claimed' }] }, {
      ledgerPath: process.argv[1], experimentBinding: { experimentId: 'crash-test' },
      executeCell: (_cell, { registerLaunch }) => {
        registerLaunch({ callId: 'claimed:model', directory: process.argv[3],
          productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001' });
        process.exit(86);
      },
    });
  `, ledgerPath, new URL('../scripts/run-real-task-benchmark.mjs', import.meta.url).href, directory], { encoding: 'utf8' })
  assert.equal(result.status, 86, result.stderr)
  let executions = 0
  await assert.rejects(runBenchmarkPlan({ cells: [{ runId: 'claimed' }] }, {
    ledgerPath, experimentBinding: { experimentId: 'crash-test' },
    executeCell: () => { executions += 1 },
  }), error => {
    assert.equal(error.code, 'LEDGER_RUN_UNRESOLVED')
    assert.deepEqual(error.launches, [{ callId: 'claimed:model', directory,
      productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001' }])
    return true
  })
  assert.equal(executions, 0)
})

test('product launch identity is immutable, claim-scoped and retained with the result', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-launch-binding-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const path = resolve(directory, 'ledger.sqlite3')
  const plan = { cells: [{ runId: 'one' }, { runId: 'two' }] }
  const target = { callId: 'one:model', directory: resolve(directory, 'model'),
    productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001' }
  const store = openBenchmarkLedger(path, 'test-only', plan.cells)
  try {
    const { token } = store.claim(0)
    assert.throws(() => store.registerLaunch(0, 'wrong', target), { code: 'LEDGER_CLAIM_MISMATCH' })
    assert.throws(() => store.registerLaunch(1, token, target), { code: 'LEDGER_CLAIM_MISMATCH' })
    for (const patch of [{ callId: 'foreign:model' }, { directory: 'relative' },
      { productSessionId: 'invalid' }, { apiKey: 'must-never-persist' }]) {
      assert.throws(() => store.registerLaunch(0, token, { ...target, ...patch }), { code: 'LEDGER_LAUNCH_INVALID' })
    }
    store.registerLaunch(0, token, target)
    store.registerLaunch(0, token, { ...target })
    assert.throws(() => store.registerLaunch(0, token, { ...target, directory }), { code: 'LEDGER_LAUNCH_CONFLICT' })
    const second = store.claim(1)
    assert.throws(() => store.registerLaunch(1, second.token, { ...target, callId: 'two:model' }), { code: 'LEDGER_LAUNCH_CONFLICT' })
    store.finish(0, token, { runId: 'one', status: 'completed', launches: store.launches(0) })
    assert.throws(() => store.registerLaunch(0, token, target), { code: 'LEDGER_CLAIM_MISMATCH' })
  } finally { store.close() }
  const reopened = openBenchmarkLedger(path, 'test-only', plan.cells)
  try { assert.deepEqual(reopened.claim(0).record.launches, [target]) } finally { reopened.close() }
})

test('launch persistence failure stops the batch even when an adapter catches the error', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-launch-failure-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'test-only' } }
  const plan = { cells: [{ runId: 'one' }, { runId: 'two' }] }
  const calls = []
  await assert.rejects(runBenchmarkPlan(plan, { ...options, executeCell: (cell, { registerLaunch }) => {
    calls.push(cell.runId)
    const database = new DatabaseSync(options.ledgerPath)
    database.exec("CREATE TRIGGER fail_launch BEFORE INSERT ON benchmark_launch BEGIN SELECT RAISE(ABORT, 'disk failure fixture'); END;")
    database.close()
    try {
      registerLaunch({ callId: 'one:model', directory,
        productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001' })
    } catch { /* An adapter cannot turn a storage failure into a task result. */ }
    return { status: 'completed', score: 1 }
  } }), /disk failure fixture/u)
  assert.deepEqual(calls, ['one'])
  await assert.rejects(runBenchmarkPlan(plan, { ...options,
    executeCell: () => assert.fail('unresolved task must not restart'),
  }), { code: 'LEDGER_RUN_UNRESOLVED' })
})

test('model adapter registers separate standalone and Fusion launch targets before execution', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-adapter-launch-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const full = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const plan = { cells: [full.cells[0], full.cells.find(cell => cell.fusionKind === 'independent-aggregate')] }
  const ledgerPath = resolve(directory, 'ledger.sqlite3')
  let launches = 0
  const adapter = {
    runModel(request, runner) {
      launches += 1
      runner.registerLaunch({ callId: request.callId, directory: resolve(directory, String(launches)),
        productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001' })
      const database = new DatabaseSync(ledgerPath, { readOnly: true })
      try {
        assert.equal(database.prepare('SELECT count(*) AS count FROM benchmark_launch').get().count, launches)
      } finally { database.close() }
      return { provider: request.provider }
    },
    aggregate: () => ({ claims: [] }),
  }
  const result = await runBenchmarkPlan(plan, { ledgerPath, experimentBinding: { experimentId: 'test-only' },
    executeCell: (cell, context) => executeBenchmarkCell(cell, adapter, context),
  })
  assert.equal(launches, 5)
  assert.equal(result.records[0].launches.length, 1)
  assert.equal(result.records[1].launches.length, 4)
})

test('actual Device launcher commits its product address before starting and survives process death', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-device-launch-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const ledgerPath = resolve(directory, 'ledger.sqlite3')
  const taskInputPath = resolve(directory, 'task.json')
  await writeFile(taskInputPath, JSON.stringify({ title: 'test only', goal: 'test only',
    scope: ['TASK.md'], constraints: [], outOfScope: [], verificationCommand: 'true',
    acceptanceCriteria: [{ id: 'fixture', title: 'test only', required: true }], files: { 'TASK.md': 'fixture\n' },
  }))
  const cell = { ...benchmarkConfiguration('main-A'), runId: 'device-call', taskId: 'fixture', comparison: 'glm-5.3-flash',
    fusionKind: 'standalone', reasoningEffort: 'max', budgetLimits: null }
  const options = { ledgerPath, experimentBinding: { experimentId: 'test-only' } }
  const productDirectory = resolve(directory, 'product')
  const wwcBinary = resolve(directory, 'wwc-fixture')
  await writeFile(wwcBinary, '')
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', `
    import assert from 'node:assert/strict';
    const { runBenchmarkPlan, executeBenchmarkCell } = await import(process.argv[1]);
    const { runDeviceTaskVertical } = await import(process.argv[2]);
    const [cell, options, directory, taskInputPath] = JSON.parse(process.argv[3]);
    await assert.rejects(runDeviceTaskVertical({ ...cell, directory, taskInputPath, providerName: 'glm',
      callId: 'device-call:model', requestedModel: 'mimo-v2.6-pro',
      registerLaunch: () => assert.fail('model mismatch reached launch registration'),
    }), /configured model must match/u);
    const cli = process.env.WWC_CLI_BINARY;
    process.env.WWC_CLI_BINARY = directory + '/missing-wwc';
    await assert.rejects(runDeviceTaskVertical({ ...cell, directory, taskInputPath, providerName: 'glm',
      callId: 'device-call:model', requestedModel: 'glm-5.3-flash',
      registerLaunch: () => assert.fail('missing CLI reached launch registration'),
    }), { code: 'DEVICE_CLI_MISSING' });
    process.env.WWC_CLI_BINARY = cli;
    await runBenchmarkPlan({ cells: [cell] }, { ...options,
      executeCell: (cell, context) => executeBenchmarkCell(cell, {
        runModel: (request, runner) => runDeviceTaskVertical({ ...request, directory, taskInputPath, providerName: 'glm',
          callId: request.callId, requestedModel: request.provider,
          registerLaunch: target => { runner.registerLaunch(target); process.exit(86); },
        }),
      }, context),
    });
  `, new URL('../scripts/run-real-task-benchmark.mjs', import.meta.url).href,
  new URL('../scripts/run-device-task-vertical.mjs', import.meta.url).href,
  JSON.stringify([cell, options, productDirectory, taskInputPath])], {
    encoding: 'utf8', env: {
      PATH: process.env.PATH, HOME: process.env.HOME,
      ZHIPU_API_KEY: 'synthetic-fixture-key', ZHIPU_BASE_URL: 'https://model.invalid', ZHIPU_MODEL: 'glm-5.3-flash',
      WWC_WORKER_MODEL_REASONING_EFFORT: 'max', WWC_BENCHMARK_TOOL_REPEAT_GUARD: '1',
      WWC_CLI_BINARY: wwcBinary,
    },
  })
  assert.equal(result.status, 86, result.stderr)
  await assert.rejects(runBenchmarkPlan({ cells: [cell] }, { ...options,
    executeCell: () => assert.fail('original product target must be reconciled first'),
  }), error => {
    assert.equal(error.code, 'LEDGER_RUN_UNRESOLVED')
    assert.deepEqual(error.launches, [{ callId: 'device-call:model', directory: productDirectory,
      productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001' }])
    return true
  })
})

test('concurrent runner cannot execute an already claimed cell', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-concurrent-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = { cells: [{ runId: 'one' }] }
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'concurrent' } }
  let release
  const pending = new Promise(resolve => { release = resolve })
  const running = runBenchmarkPlan(plan, { ...options, executeCell: () => pending })
  try {
    await assert.rejects(runBenchmarkPlan(plan, {
      ...options, executeCell: () => assert.fail('duplicate execution'),
    }), error => error.code === 'LEDGER_RUN_UNRESOLVED')
  } finally {
    release({ status: 'completed', score: 1 })
    await running
  }
})

test('Core repeat stop code survives restart and keeps every pending cell in the denominator', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-stop-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'stop-test' } }
  await assert.rejects(runBenchmarkPlan(plan, {
    ...options,
    executeCell: () => { throw Object.assign(new Error('Core stopped'), { code: 'STUCK_TOOL_REPEAT_LIMIT' }) },
    onRecord: () => { throw new Error('export interrupted') },
  }), /export interrupted/u)
  const ledger = await runBenchmarkPlan(plan, {
    ...options, executeCell: () => assert.fail('stopped batch must not execute'),
  })
  assert.equal(ledger.denominator, 700)
  assert.equal(ledger.records[0].status, 'failed')
  assert.equal(ledger.records[0].failure.code, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(ledger.records.slice(1).every(record => record.status === 'not_run_runner_terminated'), true)
  assert.equal(ledger.records.every(record => record.score === null && record.verdict === null), true)
})


test('Device Core stop check waits for a concurrent database migration', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-device-core-lock-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const session = 'wsn_01J00000000000000000000001'
  const runtime = resolve(directory, 'worker-sessions', session, 'data', 'codex-runtime')
  await mkdir(runtime, { recursive: true })
  const child = spawn(process.execPath, ['--input-type=module', '-e', `
    import { DatabaseSync } from 'node:sqlite';
    const db = new DatabaseSync(process.argv[1]);
    db.exec("BEGIN EXCLUSIVE; CREATE TABLE tool_repeat_run (run_key TEXT, stopped INTEGER); INSERT INTO tool_repeat_run VALUES ('stopped-run', 1)");
    process.stdout.write('locked');
    setTimeout(() => { db.exec('COMMIT'); db.close(); }, 200);
  `, resolve(runtime, 'worker-codex.sqlite3')], { stdio: ['ignore', 'pipe', 'ignore'] })
  t.after(() => child.kill())
  const exited = once(child, 'exit')
  await once(child.stdout, 'data')
  assert.throws(() => assertDeviceBenchmarkRunning(directory, [session]), error =>
    error.code === 'STUCK_TOOL_REPEAT_LIMIT' && error.runKey === 'stopped-run')
  assert.equal((await exited)[0], 0)
})

test('Device Core stop ledger terminates the delivery driver and all remaining benchmark cells', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-device-core-stop-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const workerSessionId = 'wsn_01J00000000000000000000001'
  const sessions = new Set([workerSessionId])
  assertDeviceBenchmarkRunning(directory, sessions) // Worker has not started yet.
  assert.throws(() => assertDeviceBenchmarkRunning(directory, ['../escape']))
  const runtime = resolve(directory, 'worker-sessions', workerSessionId, 'data', 'codex-runtime')
  await mkdir(runtime, { recursive: true })
  const database = new DatabaseSync(resolve(runtime, 'worker-codex.sqlite3'))
  t.after(() => database.close())
  assertDeviceBenchmarkRunning(directory, sessions) // Database exists before its schema is initialized.
  database.exec(`PRAGMA journal_mode=WAL;
    CREATE TABLE tool_repeat_run (run_key TEXT PRIMARY KEY, stopped INTEGER NOT NULL);
    INSERT INTO tool_repeat_run VALUES ('owned-core-run', 0)`)
  assertDeviceBenchmarkRunning(directory, sessions)
  database.exec("UPDATE tool_repeat_run SET stopped = 1 WHERE run_key = 'owned-core-run'")
  assert.throws(() => assertDeviceBenchmarkRunning(directory, sessions), error =>
    error.code === 'STUCK_TOOL_REPEAT_LIMIT' && error.workerSessionId === workerSessionId
      && error.runKey === 'owned-core-run')
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  let calls = 0
  const options = {
    ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'device-stop' },
    executeCell: async () => {
      calls += 1
      await driveDelivery({ query: () => assert.fail('must stop before querying or launching more work') },
        null, undefined, Date.now, { assertRunning: () => assertDeviceBenchmarkRunning(directory, sessions) })
    },
  }
  const ledger = await runBenchmarkPlan(plan, options)
  assert.equal(calls, 1)
  assert.equal(ledger.records[0].failure.code, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(ledger.records.slice(1).every(record => record.status === 'not_run_runner_terminated'), true)
  assert.equal(ledger.denominator, 700)
  await runBenchmarkPlan(plan, { ...options, executeCell: () => assert.fail('restart cannot execute') })
  assert.equal(database.prepare('SELECT stopped FROM tool_repeat_run').get().stopped, 1)
})

test('execution completion leaves external grading pending and never fabricates zero scores', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const ledger = await runBenchmarkPlan(plan, {
    executeCell: async () => ({ status: 'completed', score: 1, verdict: 'pass' }),
  })
  assert.equal(ledger.records.every(record => record.score === null && record.verdict === null), true)
  const report = aggregateBenchmarkReport(plan, { records: ledger.records, experimentId: 'pending-grading' })
  assert.equal(report.failureAccounting.completed, 700)
  assert.equal(report.failureAccounting.unsuccessful, 0)
  assert.deepEqual(report.grading, { status: 'pending', scored: 0, pending: 700 })
  assert.equal(report.quality.meanScore, null)
  assert.equal(report.quality.bindingErrors, null)
  assert.equal(report.strategyReferences.every(reference => reference.regrets === null), true)
  assert.equal(Object.values(report.gates).every(gate => gate.status === 'insufficient_evidence'), true)

  const partial = ledger.records.map((record, index) => ({ ...record, score: index === 0 ? null : 0.8 }))
  const partialReport = aggregateBenchmarkReport(plan, { records: partial, experimentId: 'partial-grading' })
  assert.deepEqual(partialReport.grading, { status: 'pending', scored: 699, pending: 1 })
  assert.equal(partialReport.quality.meanScore, null)
  assert.equal(partialReport.strategyReferences[0].regrets, null)
  const graded = partial.map(record => ({ ...record, score: 0.8 }))
  const gradedReport = aggregateBenchmarkReport(plan, { records: graded, experimentId: 'external-scores' })
  assert.equal(gradedReport.grading.status, 'complete')
  assert.equal(gradedReport.quality.meanScore, 0.8)
  assert.equal(gradedReport.gates.bindingError.status, 'insufficient_evidence')
  const incompleteQuality = graded.map((record, index) => ({
    ...record, quality: index === 0 ? { minorityRetained: 1, minorityTotal: 1, bindingErrors: 0 } : null,
  }))
  const incompleteReport = aggregateBenchmarkReport(plan, { records: incompleteQuality, experimentId: 'partial-quality' })
  assert.equal(incompleteReport.quality.minority.observedRuns, 1)
  assert.equal(incompleteReport.gates.minority.status, 'insufficient_evidence')
  assert.equal(incompleteReport.gates.bindingError.status, 'insufficient_evidence')
  for (const score of [Number.NaN, Infinity, -1, 1.1, '0.8']) {
    assert.throws(() => aggregateBenchmarkReport(plan, {
      records: graded.map((record, index) => index === 0 ? { ...record, score } : record),
      experimentId: 'invalid-external-score',
    }), error => error.code === 'SCORE_INVALID')
  }
})

test('Fusion member receipts survive aggregation failure and unfinished-ledger reopen', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-member-receipts-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const full = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i + 1}`) })
  const cell = full.cells.find(item => item.fusionKind === 'independent-aggregate')
  const path = resolve(directory, 'ledger.sqlite3')
  const result = await runBenchmarkPlan({ cells: [cell] }, {
    ledgerPath: path, experimentBinding: { experimentId: 'receipt-test' },
    executeCell: (item, context) => executeBenchmarkCell(item, {
      runModel: request => ({ provider: request.provider, candidateCommit: 'fixture' }),
      aggregate: () => {
        const db = new DatabaseSync(path, { readOnly: true })
        try { assert.equal(db.prepare('SELECT count(*) AS n FROM benchmark_call').get().n, 4) }
        finally { db.close() }
        throw Object.assign(new Error('fixture aggregation failure'), { code: 'AGGREGATION_FAILED' })
      },
    }, context),
  })
  assert.equal(result.records[0].status, 'failed')
  assert.deepEqual(result.records[0].calls.map(call => call.status), ['returned', 'returned', 'returned', 'returned', 'failed'])
  assert.equal(result.records[0].calls[4].failure.code, 'AGGREGATION_FAILED')
  assert.equal(result.records[0].score, null)
  const interruptedPath = resolve(directory, 'interrupted.sqlite3')
  let ledger = openBenchmarkLedger(interruptedPath, 'identity', [cell])
  const { token } = ledger.claim(0)
  const receipt = result.records[0].calls[0]
  ledger.recordCall(0, token, receipt)
  ledger.recordCall(0, token, receipt)
  assert.throws(() => ledger.recordCall(0, token, { ...receipt, result: 'changed' }), { code: 'LEDGER_CALL_CONFLICT' })
  assert.throws(() => ledger.recordCall(0, 'wrong-token', receipt), { code: 'LEDGER_CLAIM_MISMATCH' })
  ledger.close()
  ledger = openBenchmarkLedger(interruptedPath, 'identity', [cell])
  try {
    assert.throws(() => ledger.claim(0), error => {
      assert.equal(error.code, 'LEDGER_RUN_UNRESOLVED')
      assert.deepEqual(error.calls, [receipt])
      return true
    })
  } finally { ledger.close() }
})

test('member receipt write failure stops execution and leaves the cell unresolved', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-member-write-failure-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const full = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i + 1}`) })
  const plan = { cells: [full.cells.find(cell => cell.fusionKind === 'independent-aggregate')] }
  const ledgerPath = resolve(directory, 'ledger.sqlite3')
  const options = { ledgerPath, experimentBinding: { experimentId: 'write-failure' } }
  let executions = 0
  await assert.rejects(runBenchmarkPlan(plan, { ...options,
    executeCell: (cell, context) => executeBenchmarkCell(cell, {
      runModel: () => {
        executions += 1
        const db = new DatabaseSync(ledgerPath)
        try { db.exec("CREATE TRIGGER reject_call BEFORE INSERT ON benchmark_call BEGIN SELECT RAISE(ABORT, 'receipt disk failure'); END;") }
        finally { db.close() }
        return { candidateCommit: 'fixture' }
      },
      aggregate: () => assert.fail('aggregation must not start'),
    }, context),
  }), /receipt disk failure/u)
  assert.equal(executions, 1)
  await assert.rejects(runBenchmarkPlan(plan, { ...options,
    executeCell: () => assert.fail('unresolved member must not rerun'),
  }), { code: 'LEDGER_RUN_UNRESOLVED' })
})

test('returned product stop halts standalone, Fusion members and aggregation without another call', async t => {
  const root = await mkdtemp(resolve(tmpdir(), 'wwc-returned-stop-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const full = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i + 1}`) })
  const fusion = full.cells.find(cell => cell.fusionKind === 'independent-aggregate')
  for (const stage of ['standalone', 'member', 'aggregation']) {
    const first = stage === 'standalone' ? full.cells[0] : fusion
    const next = full.cells.find(cell => cell.runId !== first.runId)
    let calls = 0
    const stop = { failure: { code: 'STUCK_TOOL_REPEAT_LIMIT' }, claims: [{ id: 'claim:retained' }] }
    const adapter = {
      runModel: () => { calls += 1; return stage === 'aggregation' ? { answer: 'member' } : stop },
      aggregate: () => { calls += 1; assert.equal(stage, 'aggregation'); return stop },
    }
    const ledger = await runBenchmarkPlan({ cells: [first, next] }, {
      ledgerPath: resolve(root, `${stage}.sqlite3`), experimentBinding: { experimentId: stage },
      executeCell: (cell, context) => executeBenchmarkCell(cell, adapter, context),
    })
    assert.equal(calls, stage === 'aggregation' ? 5 : 1)
    assert.equal(ledger.records[0].status, 'failed')
    assert.equal(ledger.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
    assert.deepEqual(ledger.records[0].claims, stop.claims)
    assert.deepEqual(ledger.records[0].calls.at(-1).result, stop)
    assert.equal(ledger.records[1].status, 'not_run_runner_terminated')
    assert.equal(ledger.records[1].calls.length, 0)
    assert.equal(ledger.records[0].score, null)
    assert.equal(ledger.records[1].score, null)
  }
})


test('Fusion Device provisioning binds four exact models and preserves private settings in memory', () => {
  const entries = [
    ['ZHIPU', 'zhipu-glm', 'glm-5.3-flash'], ['XIAOMI', 'xiaomi-mimo', 'mimo-v2.6-pro'],
    ['DEEPSEEK', 'deepseek', 'deepseek-flash'], ['OPENCODE', 'opencode', 'qwen3.8-flash'],
  ]
  const environment = Object.fromEntries(entries.flatMap(([prefix, , model]) => [
    [`${prefix}_MODEL`, model], [`${prefix}_BASE_URL`, 'https://provider.invalid'], [`${prefix}_API_KEY`, 'private-key'],
  ]))
  const profile = { members: entries.map(([, provider, model]) => ({ id: provider, provider, model, reasoning: 'max' })) }
  assert.deepEqual(fusionDeviceProviders(profile, environment).map(row => row.modelId), entries.map(row => row[2]))
  const changed = structuredClone(profile)
  changed.members[0].model = 'wrong-model'
  assert.throws(() => fusionDeviceProviders(changed, environment))
  changed.members[0] = changed.members[1]
  assert.throws(() => fusionDeviceProviders(changed, environment))
  assert.throws(() => fusionDeviceProviders(profile, {}), error => !error.message.includes('private-key'))
})


test('durable failures retain safe codes but never raw adapter errors', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-failure-redaction-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const secret = 'private-provider-key-should-not-be-exported'
  const plan = { cells: [{ runId: 'error-code' }, { runId: 'unknown-error' }] }
  const options = {
    ledgerPath: resolve(directory, 'ledger.sqlite'),
    experimentBinding: { experimentId: 'failure-redaction' },
    executeCell: async cell => {
      throw Object.assign(new Error(`Authorization: Bearer ${secret}`), {
        code: cell.runId === 'error-code' ? 'PROVIDER_UNAVAILABLE' : secret,
      })
    },
  }
  const ledger = await runBenchmarkPlan(plan, options)
  assert.deepEqual(ledger.records.map(row => row.failure), [
    { code: 'PROVIDER_UNAVAILABLE', message: 'PROVIDER_UNAVAILABLE' },
    { code: 'RUNNER_UNEXPECTED', message: 'RUNNER_UNEXPECTED' },
  ])
  assert.ok(ledger.records.every(row => row.status === 'failed' && row.score === null))
  assert.equal(JSON.stringify(ledger).includes(secret), false)
  const restored = await runBenchmarkPlan(plan, {
    ...options, executeCell: async () => assert.fail('saved failures must not execute again'),
  })
  assert.deepEqual(restored, ledger)
  const database = new DatabaseSync(options.ledgerPath, { readOnly: true })
  try {
    assert.equal(database.prepare('SELECT record FROM benchmark_cell').all()
      .some(row => row.record.includes(secret)), false)
  } finally { database.close() }
})


test('unresolved Device inspection queries the original TLS endpoint without changing claims or launching work', async () => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-inspection-'))
  const path = resolve(directory, 'ledger.sqlite3')
  const keyPath = resolve(directory, 'fixture-key.pem')
  const certPath = resolve(directory, 'fixture-cert.pem')
  execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-days', '1',
    '-subj', '/CN=control.localhost', '-addext', 'subjectAltName=DNS:control.localhost',
    '-keyout', keyPath, '-out', certPath], { stdio: 'ignore' })
  const target = { callId: 'one:model', directory,
    deliveryId: 'dlv_01J00000000000000000000001', productSessionId: 'psn_01J00000000000000000000001' }
  const cursor = { deliveryId: target.deliveryId, token: 'frozen-cursor' }
  const requests = []
  let wrongCursor = false
  const server = createServer({ key: await readFile(keyPath), cert: await readFile(certPath) }, async (request, response) => {
    const chunks = []
    for await (const chunk of request) chunks.push(chunk)
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : null
    requests.push({ method: request.method, path: request.url, body })
    response.setHeader('Content-Type', 'application/json')
    if (request.url === '/api/v1/auth/session' && request.method === 'GET') {
      response.end(JSON.stringify({ schemaVersion: 'winwincode/v1', actor: { kind: 'user' } }))
      return
    }
    if (request.url !== '/api/v1/queries') {
      response.writeHead(405).end('{}')
      return
    }
    const results = {
      'delivery.get': { deliveryId: target.deliveryId, readCursor: cursor, status: 'in_progress' },
      'workrun.get': { readCursor: wrongCursor ? { ...cursor, token: 'newer' } : cursor,
        runs: [{ id: 'original-run', state: 'running', attempt: 1 }] },
      'session.get': { id: target.productSessionId },
    }
    response.end(JSON.stringify({ schemaVersion: 'winwincode/v1', requestId: body.requestId,
      query: body.query, result: results[body.query] }))
  })
  try {
    await new Promise(resolvePromise => server.listen(0, '127.0.0.1', resolvePromise))
    const port = server.address().port
    const endpointPath = resolve(directory, 'server-endpoint.json')
    const endpoint = { controlUrl: `https://127.0.0.1:${port}`, origin: `https://api.localhost:${port}` }
    await writeFile(endpointPath, JSON.stringify(endpoint))
    const cells = ['one', 'unregistered', 'finished'].map(runId => ({ runId }))
    const ledger = openBenchmarkLedger(path, 'inspection', cells)
    ledger.registerLaunch(0, ledger.claim(0).token, target)
    ledger.claim(1)
    ledger.finish(2, ledger.claim(2).token, { runId: 'finished', status: 'failed' })
    ledger.close()
    const before = await readFile(path)
    const observations = await inspectUnresolvedDeviceTasks(path)
    assert.equal(observations.length, 2)
    assert.equal(observations[0].observation, 'queried')
    assert.equal(observations[0].workRunAggregate.runs[0].id, 'original-run')
    assert.equal(observations[1].observation, 'launch_not_registered')
    assert.deepEqual(requests.map(r => r.body?.query ?? r.path),
      ['/api/v1/auth/session', 'delivery.get', 'workrun.get', 'session.get'])
    assert.deepEqual(requests[2].body.parameters.atCursor, cursor)
    wrongCursor = true
    assert.equal((await inspectUnresolvedDeviceTasks(path))[0].observation, 'unavailable')
    const requestCount = requests.length
    await writeFile(endpointPath, JSON.stringify({ ...endpoint, controlUrl: 'https://example.com' }))
    assert.equal((await inspectUnresolvedDeviceTasks(path))[0].observation, 'unavailable')
    assert.equal(requests.length, requestCount, 'invalid endpoints must not receive requests')
    await writeFile(endpointPath, JSON.stringify(endpoint))
    await new Promise(resolvePromise => server.close(resolvePromise))
    assert.equal((await inspectUnresolvedDeviceTasks(path))[0].observation, 'unavailable')
    assert.deepEqual(await readFile(path), before, 'inspection must not mutate the ledger')
    const reopened = openBenchmarkLedger(path, 'inspection', cells)
    assert.throws(() => reopened.claim(0), { code: 'LEDGER_RUN_UNRESOLVED' })
    reopened.close()
  } finally {
    server.closeAllConnections()
    if (server.listening) await new Promise(resolvePromise => server.close(resolvePromise))
    await rm(directory, { recursive: true, force: true })
  }
})


test('Device driver preserves candidate projections before failure and propagates evidence write failures', async () => {
  const detail = { status: 'candidate_ready', deliveryRevision: 7, readCursor: { token: 'cursor-7' },
    currentCandidate: { candidateRef: 'candidate/original' }, attention: [{ id: 'needs-input', status: 'open' }] }
  const workRunAggregate = { runs: [{ id: 'failed-verifier', state: 'failed' }] }
  const client = {
    query: async name => ({ result: name === 'delivery.get' ? detail : workRunAggregate }),
    command: () => assert.fail('failure observation must not create another product command'),
  }
  const saved = []
  await assert.rejects(driveDelivery(client, null, undefined, Date.now, {
    resolveAttention: false, onProjection: projection => saved.push(structuredClone(projection)),
  }), { code: 'DEVICE_TASK_ATTENTION' })
  assert.deepEqual(saved, [{ detail, workRunAggregate }])
  const writeFailure = Object.assign(new Error('evidence storage unavailable'), { code: 'ENOSPC' })
  await assert.rejects(driveDelivery(client, null, undefined, Date.now, {
    onProjection: () => { throw writeFailure },
  }), error => error === writeFailure)
  detail.status = 'done'
  await assert.rejects(driveDelivery(client, null, undefined, Date.now, {
    onProjection: projection => saved.push(projection),
  }), /Done projection must contain a passing verdict/u)
  assert.equal(saved.at(-1).detail.status, 'done', 'final projection must also be preserved')
})


test('candidate export verifies a self-contained Git bundle and preserves failed product status', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'candidate-export-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const repository = resolve(directory, 'source')
  await mkdir(repository)
  const git = (...args) => execFileSync('git', ['-C', repository, ...args], { encoding: 'utf8' }).trim()
  git('init', '--quiet')
  await writeFile(resolve(repository, 'answer.txt'), 'candidate answer\n')
  git('add', 'answer.txt')
  git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', '-c', 'commit.gpgSign=false', 'commit', '--quiet', '-m', 'candidate')
  const commit = git('rev-parse', 'HEAD')
  const tree = git('rev-parse', 'HEAD^{tree}')
  const bundlePath = resolve(directory, 'input.bundle')
  git('bundle', 'create', bundlePath, 'HEAD')
  const bundle = await readFile(bundlePath)
  const sha = bytes => createHash('sha256').update(bytes).digest('hex')
  const artifact = Buffer.from(JSON.stringify({ schemaVersion: 2, candidateCommitId: commit,
    bundleBase64: bundle.toString('base64'), bundleDigest: `sha256:${sha(bundle)}` }))
  const artifactDigest = sha(artifact)
  const objectDirectory = resolve(directory, 'server-data/artifacts/objects/sha256', artifactDigest.slice(0, 2))
  await mkdir(objectDirectory, { recursive: true })
  const artifactPath = resolve(objectDirectory, artifactDigest.slice(2))
  await writeFile(artifactPath, artifact)
  const taskPath = resolve(directory, 'task-input.json')
  await writeFile(taskPath, '{"task":"original"}')
  await writeFile(resolve(directory, 'product-source-seal.json'), '{"source":"frozen"}')
  const candidateRef = `refs/winwincode/candidates/${commit}`
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i}`) }).cells[0]
  const launch = { callId: `${cell.runId}:model`, directory,
    deliveryId: 'dlv_01J00000000000000000000001', productSessionId: 'psn_01J00000000000000000000001' }
  const report = { complete: false, candidateRef, taskInputDigest: sha(await readFile(taskPath)),
    productSessionId: launch.productSessionId, deliveryId: launch.deliveryId,
    modelRoute: { modelId: cell.comparison }, benchmarkConfiguration: benchmarkConfiguration('main-A'),
    delivery: { detail: { currentCandidate: {
      candidateRef, candidateCommitId: commit, candidateTreeId: tree,
    } } } }
  const reportPath = resolve(directory, 'device-task-result.json')
  await writeFile(reportPath, JSON.stringify(report))
  const manifest = exportDeviceCandidate(directory, taskPath)
  assert.equal(manifest.productComplete, false)
  assert.equal(manifest.externalScore, null)
  assert.equal(manifest.externalVerdict, null)
  assert.deepEqual(await readFile(resolve(directory, 'submission-evidence', commit, 'candidate.bundle')), bundle)
  const candidateFiles = await readFile(resolve(directory, 'submission-evidence', commit, 'candidate-files.json'))
  assert.equal(sha(candidateFiles), manifest.candidateFilesSha256)
  assert.equal(JSON.parse(candidateFiles).files[0].content, 'candidate answer\n')
  assert.deepEqual(exportDeviceCandidate(directory, taskPath), manifest)

  await writeFile(resolve(directory, 'task-source-binding.json'), JSON.stringify({ runId: cell.runId,
    taskId: cell.taskId, callId: launch.callId, taskInputSha256: report.taskInputDigest }))
  execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-days', '1',
    '-subj', '/CN=control.localhost', '-addext', 'subjectAltName=DNS:control.localhost',
    '-keyout', resolve(directory, 'fixture-key.pem'), '-out', resolve(directory, 'fixture-cert.pem')], { stdio: 'ignore' })
  let done = false
  const cursor = { deliveryId: launch.deliveryId, token: 'terminal-cursor' }
  const server = createServer({ key: await readFile(resolve(directory, 'fixture-key.pem')),
    cert: await readFile(resolve(directory, 'fixture-cert.pem')) }, async (request, response) => {
    response.setHeader('Content-Type', 'application/json')
    if (request.method === 'GET' && request.url === '/api/v1/auth/session') {
      response.end(JSON.stringify({ schemaVersion: 'winwincode/v1', actor: { kind: 'user' } }))
      return
    }
    assert.equal(request.url, '/api/v1/queries', 'recovery may not send product commands')
    const chunks = []
    for await (const chunk of request) chunks.push(chunk)
    const body = JSON.parse(Buffer.concat(chunks))
    const values = {
      'session.get': { id: launch.productSessionId },
      'delivery.get': { ...report.delivery.detail, deliveryId: launch.deliveryId, readCursor: cursor,
        status: done ? 'done' : 'in_progress', verdict: { status: 'pass', criteria: [{ verdict: 'pass' }] },
        attention: [], evidence: [{ id: 'canonical-test-evidence' }] },
      'workrun.get': { readCursor: cursor, items: [{ state: 'done' }],
        runs: [{ id: 'original-run', state: 'completed', workItemId: 'original-item', executionJobId: 'original-job' }] },
    }
    response.end(JSON.stringify({ schemaVersion: 'winwincode/v1', requestId: body.requestId,
      query: body.query, result: values[body.query] }))
  })
  try {
    await new Promise(resolvePromise => server.listen(0, '127.0.0.1', resolvePromise))
    const port = server.address().port
    await writeFile(resolve(directory, 'server-endpoint.json'), JSON.stringify({
      controlUrl: `https://127.0.0.1:${port}`, origin: `https://api.localhost:${port}` }))
    const plan = { cells: [cell] }
    const options = { ledgerPath: resolve(directory, 'recovery.sqlite3'), experimentBinding: { experimentId: 'original-product' } }
    await assert.rejects(runBenchmarkPlan(plan, { ...options, executeCell: (_cell, context) => {
      context.registerLaunch(launch)
      throw Object.assign(new Error('interrupted export'), { code: 'BENCHMARK_EVIDENCE_FAILED' })
    } }), { code: 'BENCHMARK_EVIDENCE_FAILED' })
    const recovery = { ...options, executeCell: () => assert.fail('must not rerun task'), recoverCell: recoverBenchmarkDeviceCell }
    await assert.rejects(runBenchmarkPlan(plan, recovery))
    done = true
    const restored = await runBenchmarkPlan(plan, recovery)
    const recoveredManifest = JSON.parse(await readFile(restored.records[0].model.submissionManifest.path))
    assert.equal(recoveredManifest.productComplete, true)
    assert.equal(recoveredManifest.externalScore, null)
    assert.equal(JSON.parse(await readFile(reportPath)).complete, false, 'original observation remains unchanged')
    assert.equal(JSON.parse(await readFile(resolve(directory, 'submission-evidence', commit, 'manifest.json'))).productComplete, false)
    assert.deepEqual(await runBenchmarkPlan(plan, recovery), restored)
  } finally {
    server.closeAllConnections()
    await new Promise(resolvePromise => server.close(resolvePromise))
  }
  report.complete = true
  await writeFile(reportPath, JSON.stringify(report))
  assert.throws(() => exportDeviceCandidate(directory, taskPath), /must not replace prior evidence/u)
  report.delivery.detail.currentCandidate.candidateTreeId = '0'.repeat(40)
  await writeFile(reportPath, JSON.stringify(report))
  assert.throws(() => exportDeviceCandidate(directory, taskPath))
  await writeFile(taskPath, 'changed task')
  assert.throws(() => exportDeviceCandidate(directory, taskPath), /task input changed/u)
})

test('candidate evidence failure leaves the claim unresolved and stops the batch', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'candidate-export-failure-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = { cells: [{ runId: 'one' }, { runId: 'two' }] }
  let launches = 0
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'export-failure' },
    executeCell: () => { launches += 1; throw Object.assign(new Error('export failed'), { code: 'BENCHMARK_EVIDENCE_FAILED' }) } }
  await assert.rejects(runBenchmarkPlan(plan, options), { code: 'BENCHMARK_EVIDENCE_FAILED' })
  await assert.rejects(runBenchmarkPlan(plan, options), { code: 'LEDGER_RUN_UNRESOLVED' })
  assert.equal(launches, 1)
})


test('Device JEV arms select only their explicit component profiles and keep baseline clear', () => {
  const settings = { jevContext: { provider: 'context-provider', policy: { version: 'frozen' } },
    jevJudge: 'judge-provider', jevSettingsFile: resolve(tmpdir(), 'private-jev.json') }
  for (const id of ['main-A', 'main-B', 'jev-context-only', 'jev-judge-only', 'jev-full']) {
    const configuration = benchmarkConfiguration(id)
    const env = benchmarkDeviceEnvironment(configuration, settings)
    assert.equal(env.PYTHONDONTWRITEBYTECODE, '1')
    assert.equal(env.WWC_WORKER_MODEL_REASONING_EFFORT, 'max')
    assert.equal(env.WWC_BENCHMARK_TOOL_REPEAT_GUARD, '1')
    assert.equal(env.WWC_WORKER_FUSION, undefined)
    assert.equal(env.WWC_WORKER_JEV_CONTEXT, configuration.jevContext ? JSON.stringify(settings.jevContext) : undefined)
    assert.equal(env.WWC_WORKER_JEV_JUDGE, configuration.jevJudge ? settings.jevJudge : undefined)
    assert.equal(env.WWC_DEVICE_JEV_SETTINGS_FILE, configuration.jev ? settings.jevSettingsFile : undefined)
  }
  for (const id of ['main-B', 'jev-context-only', 'jev-judge-only', 'jev-full']) {
    assert.throws(() => benchmarkDeviceEnvironment(benchmarkConfiguration(id)), { code: 'BENCHMARK_CONFIGURATION_UNAVAILABLE' })
  }
  const fusionEnvironment = {
    ZHIPU_API_KEY: 'zhipu-secret', ZHIPU_BASE_URL: 'https://glm.example.invalid', ZHIPU_MODEL: 'glm-5.3-flash',
    XIAOMI_API_KEY: 'mimo-secret', XIAOMI_BASE_URL: 'https://mimo.example.invalid', XIAOMI_MODEL: 'mimo-v2.6-pro',
    DEEPSEEK_API_KEY: 'deepseek-secret', DEEPSEEK_BASE_URL: 'https://deepseek.example.invalid', DEEPSEEK_MODEL: 'deepseek-flash',
    OPENCODE_API_KEY: 'qwen-secret', OPENCODE_BASE_URL: 'https://qwen.example.invalid/v1/chat/completions',
    OPENCODE_MODEL: 'qwen3.8-flash', OPENCODE_SESSION_VALUE: 'private-session',
  }
  for (const id of ['main-C', 'main-D']) {
    const env = benchmarkDeviceEnvironment(benchmarkConfiguration(id), settings, fusionEnvironment)
    assert.equal(env.PYTHONDONTWRITEBYTECODE, '1')
    const profile = JSON.parse(env.WWC_WORKER_FUSION)
    assert.deepEqual(profile.members.map(({ id: member, provider, model, reasoning }) =>
      [member, provider, model, reasoning]), [
      ['fusion-glm', 'zhipu-glm', 'glm-5.3-flash', 'max'],
      ['fusion-mimo', 'xiaomi-mimo', 'mimo-v2.6-pro', 'max'],
      ['fusion-deepseek', 'deepseek', 'deepseek-flash', 'max'],
      ['fusion-qwen', 'opencode', 'qwen3.8-flash', 'max'],
    ])
    assert.equal(env.WWC_WORKER_FUSION.includes('secret'), false)
    assert.equal(env.WWC_WORKER_FUSION.includes('private-session'), false)
    if (id === 'main-C') {
      assert.equal(env.WWC_WORKER_JEV_CONTEXT, undefined)
      assert.equal(env.WWC_WORKER_JEV_JUDGE, undefined)
      assert.equal(env.WWC_DEVICE_JEV_SETTINGS_FILE, undefined)
    } else {
      assert.equal(env.WWC_WORKER_JEV_CONTEXT, JSON.stringify(settings.jevContext))
      assert.equal(env.WWC_WORKER_JEV_JUDGE, settings.jevJudge)
      assert.equal(env.WWC_DEVICE_JEV_SETTINGS_FILE, settings.jevSettingsFile)
    }
  }
  assert.throws(() => benchmarkDeviceEnvironment(benchmarkConfiguration('main-C'), {}, {}),
    { code: 'BENCHMARK_CONFIGURATION_UNAVAILABLE' })
  const withoutQwen = { ...fusionEnvironment }
  delete withoutQwen.OPENCODE_API_KEY
  delete withoutQwen.OPENCODE_BASE_URL
  delete withoutQwen.OPENCODE_MODEL
  assert.throws(() => benchmarkDeviceEnvironment(benchmarkConfiguration('main-C'), {}, withoutQwen), error => {
    assert.equal(error.code, 'BENCHMARK_CONFIGURATION_UNAVAILABLE')
    assert.match(error.message, /OPENCODE_API_KEY.*OPENCODE_BASE_URL.*OPENCODE_MODEL/u)
    assert.doesNotMatch(error.message, /secret|example\.invalid/u)
    return true
  })
  const staleModelRoutes = { ...fusionEnvironment,
    XIAOMI_MODEL: 'mimo-v2.6-pro[1m]', DEEPSEEK_MODEL: 'deepseek-v4-flash' }
  assert.throws(() => benchmarkDeviceEnvironment(benchmarkConfiguration('main-C'), {}, staleModelRoutes), error => {
    assert.equal(error.code, 'BENCHMARK_CONFIGURATION_UNAVAILABLE')
    assert.match(error.message, /XIAOMI_MODEL.*mimo-v2.6-pro/u)
    assert.match(error.message, /DEEPSEEK_MODEL.*deepseek-flash/u)
    assert.doesNotMatch(error.message, /secret|example\.invalid/u)
    return true
  })
})


test('Device benchmark loads private routes and pins the four frozen model selectors', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-device-env-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const envFile = resolve(directory, '.env')
  await writeFile(envFile, [
    'ZHIPU_API_KEY=file-glm-key', 'ZHIPU_BASE_URL=https://glm.invalid', 'ZHIPU_MODEL=glm-5.3-flash',
    'XIAOMI_API_KEY=file-mimo-key', 'XIAOMI_BASE_URL=https://mimo.invalid', 'XIAOMI_MODEL=mimo-v2.6-pro',
    'DEEPSEEK_API_KEY=file-deepseek-key', 'DEEPSEEK_BASE_URL=https://deepseek.invalid', 'DEEPSEEK_MODEL=deepseek-flash',
    'OPENCODE_API_KEY=file-qwen-key', 'OPENCODE_BASE_URL=https://qwen.invalid/v1/chat/completions',
    'OPENCODE_MODEL=qwen3.8-flash',
  ].join('\n') + '\n', { mode: 0o600 })
  const ambient = {
    XIAOMI_API_KEY: 'ambient-mimo-key', XIAOMI_MODEL: 'mimo-v2.6-pro[1m]',
    DEEPSEEK_MODEL: 'deepseek-v4-flash',
  }
  const environment = loadDeviceProviderEnvironment({ envFile, environment: ambient })
  assert.equal(environment.OPENCODE_API_KEY, 'file-qwen-key')
  assert.equal(environment.XIAOMI_API_KEY, 'ambient-mimo-key')
  assert.equal(environment.XIAOMI_MODEL, 'mimo-v2.6-pro')
  assert.equal(environment.DEEPSEEK_MODEL, 'deepseek-flash')
  const profile = JSON.parse(benchmarkDeviceEnvironment(benchmarkConfiguration('main-C'), {}, environment).WWC_WORKER_FUSION)
  assert.deepEqual(profile.members.map(member => member.model), [
    'glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash',
  ])
  assert.equal(ambient.XIAOMI_MODEL, 'mimo-v2.6-pro[1m]')

  await chmod(envFile, 0o644)
  assert.throws(() => loadDeviceProviderEnvironment({ envFile, environment: ambient }), error => {
    assert.equal(error.code, 'BENCHMARK_CONFIGURATION_UNAVAILABLE')
    assert.doesNotMatch(error.message, /file-qwen-key|mimo\.invalid/u)
    return true
  })
})

test('execution receipt export validates payloads, omits private text and preserves unknown failed usage', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'execution-receipts-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const device = resolve(directory, 'device-data', 'providers')
  await mkdir(device, { recursive: true })
  const database = new DatabaseSync(resolve(device, 'providers.sqlite3'))
  t.after(() => database.close())
  database.exec('CREATE TABLE exchanges(exchange_id TEXT,request_open TEXT,chunks TEXT)')
  const encode = object => {
    const bytes = Buffer.from(JSON.stringify(object))
    return { dataBase64: bytes.toString('base64'), payloadDigest: `sha256:${createHash('sha256').update(bytes).digest('hex')}` }
  }
  for (const [exchange, usage] of [['known', { input_tokens: 4, output_tokens: 3, total_tokens: 7 }], ['unknown', null]]) {
    const opened = { modelExchangeId: exchange, lease: { jobId: 'job' }, workerSessionId: 'session',
      request: encode({ provider: 'deepseek', request: { model: 'deepseek-flash', reasoning: { effort: 'max' },
        input: 'private-tool-content' } }) }
    const chunks = [
      { modelExchangeId: exchange, payload: encode({ type: 'server_model', model: 'deepseek-flash' }) },
      { modelExchangeId: exchange, payload: encode({ type: 'error', error: { message: 'private-provider-error' }, tokenUsage: usage }) },
    ]
    database.prepare('INSERT INTO exchanges VALUES (?,?,?)').run(exchange, JSON.stringify(opened), JSON.stringify(chunks))
  }
  const { evidence } = exportDeviceExecutionReceipts(directory)
  assert.equal(evidence.calls.length, 2)
  assert.equal(evidence.completeUsage, false)
  assert.equal(evidence.totalTokens, null)
  assert.equal(evidence.calls[0].usage.totalTokens, 7)
  assert.equal(evidence.calls[1].usage, null)
  const saved = await readFile(resolve(directory, 'execution-receipts.json'), 'utf8')
  assert.equal(saved.includes('private-'), false)
  const opened = JSON.parse(database.prepare('SELECT request_open FROM exchanges LIMIT 1').get().request_open)
  opened.request.payloadDigest = `sha256:${'0'.repeat(64)}`
  database.prepare('UPDATE exchanges SET request_open=? WHERE exchange_id=?').run(JSON.stringify(opened), 'known')
  assert.throws(() => exportDeviceExecutionReceipts(directory), /receipt digest mismatch/u)
  assert.equal(await readFile(resolve(directory, 'execution-receipts.json'), 'utf8'), saved)
})
