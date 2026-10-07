import assert from 'node:assert/strict'
import { execFileSync, spawn, spawnSync } from 'node:child_process'
import { once } from 'node:events'
import { createHash } from 'node:crypto'
import { exportDeviceCandidate, exportDeviceExecutionReceipts } from '../scripts/acceptance/export-device-candidate.mjs'
import { deviceFailureWithModelCauses } from '../scripts/lib/device-model-failures.mjs'
import { access, chmod, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:https'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { assertDeviceBenchmarkRunning } from '../scripts/lib/device-production-fixture.mjs'
import { driveDelivery } from '../scripts/acceptance/run-api-production-vertical.mjs'
import { openBenchmarkLedger } from '../scripts/lib/benchmark-ledger.mjs'
import { benchmarkAggregationInput, deviceBenchmarkExperimentBinding, executeDeviceBenchmark, prepareBenchmarkDeviceTask, recoverBenchmarkDeviceCell,
  runBenchmarkDeviceAggregation, stoppedDeviceResult, assertRetainedDeviceTaskRunning, terminalBenchmarkDeviceFailure,
  terminalDeviceFailure, failedDispatchDeviceResult, resolveRegisteredDeviceTask } from '../scripts/lib/benchmark-device-adapter.mjs'
import { runDeviceTaskVertical, fusionDeviceProviders, inspectUnresolvedDeviceTasks, benchmarkDeviceEnvironment,
  expiredCrashedDeviceWorkRun, expiredDeviceWorkRunLease, failedDeviceDispatch,
  loadDeviceProviderEnvironment } from '../scripts/acceptance/run-device-task-vertical.mjs'

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
  verifyBenchmarkLedgerIdentity,
  runBenchmarkSchedule,
  validateBenchmarkDispatchPolicy,
  validateFrozenTaskSource,
} from '../scripts/benchmark/run-real-task-benchmark.mjs'

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

const dispatchLeaseSchema = `
  CREATE TABLE execution_leases (job_id TEXT, lease_id TEXT, payload_digest TEXT, worker_id TEXT,
    worker_instance_id TEXT, attempt INTEGER, fencing_token TEXT);
  CREATE TABLE execution_lease_terminals (lease_id TEXT, job_id TEXT, worker_id TEXT,
    worker_instance_id TEXT, attempt INTEGER, fencing_token TEXT, outcome TEXT);
`

test('a claimed failed job is an execution failure rather than a dispatch failure', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-claimed-failure-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await mkdir(resolve(directory, 'server-data'))
  const db = new DatabaseSync(resolve(directory, 'server-data/control-plane.sqlite3'))
  t.after(() => db.close())
  db.exec(`CREATE TABLE scheduler_execution_jobs (job_id TEXT, delivery_id TEXT,
    work_run_id TEXT, state TEXT, attempt INTEGER, revision INTEGER, updated_at TEXT, payload_digest TEXT);
    INSERT INTO scheduler_execution_jobs VALUES ('job','delivery','run','failed',2,5,'now','payload');
    ${dispatchLeaseSchema}`)
  assert.equal(failedDeviceDispatch(directory, 'run', 'delivery').failureOrigin, undefined)
  db.exec("INSERT INTO execution_leases VALUES ('job','lease','payload','worker','instance',2,'2')")
  assert.equal(failedDeviceDispatch(directory, 'run', 'delivery').failureOrigin, 'execution')
  db.exec("UPDATE execution_leases SET payload_digest='different'")
  assert.equal(failedDeviceDispatch(directory, 'run', 'delivery').failureOrigin, undefined)
})

test('current exact cancelled lease remains a product cancellation during recovery', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-cancelled-dispatch-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await mkdir(resolve(directory, 'server-data'))
  const db = new DatabaseSync(resolve(directory, 'server-data/control-plane.sqlite3'))
  t.after(() => db.close())
  db.exec(`CREATE TABLE scheduler_execution_jobs (job_id TEXT, delivery_id TEXT,
    work_run_id TEXT, state TEXT, attempt INTEGER, revision INTEGER, updated_at TEXT, payload_digest TEXT);
    INSERT INTO scheduler_execution_jobs VALUES ('job','delivery','run','failed',2,5,'now','payload');
    ${dispatchLeaseSchema}
    INSERT INTO execution_leases VALUES ('job','lease','payload','worker','instance',2,'2');
    INSERT INTO execution_lease_terminals VALUES ('lease','job','worker','instance',2,'2','cancelled');`)
  assert.equal(failedDeviceDispatch(directory, 'run', 'delivery'), null,
    'scheduler failed state must not replace its exact cancelled lease outcome')
  assert.deepEqual(terminalDeviceFailure({ delivery: { status: 'cancelled', attention: [] },
    workRunAggregate: { runs: [{ state: 'cancelled' }], items: [{ state: 'cancelled' }] } }),
  { code: 'DEVICE_PRODUCT_CANCELLED', status: 'cancelled' })
  for (const mutation of [
    "UPDATE execution_lease_terminals SET outcome='failed'",
    "UPDATE execution_lease_terminals SET lease_id='foreign'",
    "UPDATE execution_lease_terminals SET job_id='foreign'",
    "UPDATE execution_lease_terminals SET worker_id='foreign'",
    "UPDATE execution_lease_terminals SET worker_instance_id='foreign'",
    'UPDATE execution_lease_terminals SET attempt=1',
    "UPDATE execution_lease_terminals SET fencing_token='1'",
    'UPDATE execution_leases SET attempt=1',
    "UPDATE execution_leases SET payload_digest='foreign'",
    'DELETE FROM execution_lease_terminals',
  ]) {
    db.exec(mutation)
    assert.equal(failedDeviceDispatch(directory, 'run', 'delivery')?.jobId, 'job', mutation)
    // The production helper opens its own read-only connection, so each
    // mutation and restoration must be committed before observing it.
    db.exec(`DELETE FROM execution_leases; DELETE FROM execution_lease_terminals;
      INSERT INTO execution_leases VALUES ('job','lease','payload','worker','instance',2,'2');
      INSERT INTO execution_lease_terminals VALUES ('lease','job','worker','instance',2,'2','cancelled');`)
  }
})

test('queued cancellation recovery requires the exact current scheduler receipt and no lease', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-queued-cancel-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await mkdir(resolve(directory, 'server-data'))
  const db = new DatabaseSync(resolve(directory, 'server-data/control-plane.sqlite3'))
  t.after(() => db.close())
  db.exec(`CREATE TABLE scheduler_execution_jobs (job_id TEXT, organization_id TEXT,
    workspace_id TEXT, project_id TEXT, repository_id TEXT, product_session_id TEXT,
    delivery_id TEXT, work_run_id TEXT, submission_request_id TEXT, payload_digest TEXT,
    dispatch_payload BLOB, state TEXT, attempt INTEGER, revision INTEGER, submitted_at TEXT,
    updated_at TEXT, cancellation_request_id TEXT, cancellation_requested_at TEXT);
    CREATE TABLE repository_scheduler_cancel_receipts (scope_key TEXT, request_id TEXT,
      request_digest TEXT, response_json TEXT);
    ${dispatchLeaseSchema}`)
  db.prepare('INSERT INTO scheduler_execution_jobs VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)')
    .run('job', 'org', 'workspace', 'project', 'repository', 'session', 'delivery', 'run',
      'submit', 'payload', Buffer.from('{}'), 'failed', 1, 3, 'before', 'now', 'cancel', 'now')
  const receipt = { request_id: 'cancel', lease: null, worker_session_id: null,
    codex_thread_id: null, message_id: null, replayed: false,
    job: { scope: { organization_id: 'org', workspace_id: 'workspace', project_id: 'project',
      repository_id: 'repository', product_session_id: 'session', delivery_id: 'delivery' },
    job_id: 'job', submission_request_id: 'submit', payload_digest: 'payload',
    dispatch_payload: [...Buffer.from('{}')], state: 'failed', attempt: 1, revision: 3,
    dependencies: [], work_run_id: 'run', submitted_at: 'before', updated_at: 'now',
    cancellation: { request_id: 'cancel', requested_at: 'now' } } }
  const store = value => {
    db.exec('DELETE FROM repository_scheduler_cancel_receipts')
    db.prepare('INSERT INTO repository_scheduler_cancel_receipts VALUES (?,?,?,?)')
      .run(['org', 'workspace', 'project', 'repository'].join('\u001f'), 'cancel', 'digest', JSON.stringify(value))
  }
  store(receipt)
  assert.equal(failedDeviceDispatch(directory, 'run', 'delivery'), null,
    'the retained queued cancellation receipt must reach public product recovery')
  for (const mutate of [
    value => { value.request_id = 'foreign' },
    value => { value.job.job_id = 'foreign' },
    value => { value.job.scope.product_session_id = 'foreign' },
    value => { value.job.scope.repository_id = 'foreign' },
    value => { value.job.scope.delivery_id = 'foreign' },
    value => { value.job.work_run_id = 'foreign' },
    value => { value.job.payload_digest = 'foreign' },
    value => { value.job.dispatch_payload = [0] },
    value => { value.job.dispatch_payload = [123 + 256, 125] },
    value => { value.job.dispatch_payload = [123, '125'] },
    value => { value.job.attempt = 2 },
    value => { value.job.revision = 2 },
    value => { value.job.cancellation.request_id = 'foreign' },
    value => { value.job.cancellation.requested_at = 'before' },
    value => { value.lease = { lease_id: 'foreign' } },
    value => { value.worker_session_id = 'foreign' },
    value => { value.message_id = 'foreign' },
  ]) {
    const value = structuredClone(receipt)
    mutate(value)
    store(value)
    assert.equal(failedDeviceDispatch(directory, 'run', 'delivery')?.jobId, 'job')
  }
  store(receipt)
  db.exec("INSERT INTO execution_leases VALUES ('job','lease','payload','worker','instance',1,'1')")
  assert.equal(failedDeviceDispatch(directory, 'run', 'delivery')?.jobId, 'job',
    'a no-lease receipt cannot replace an actual current lease')
  db.exec('DELETE FROM execution_leases; DELETE FROM repository_scheduler_cancel_receipts')
  assert.equal(failedDeviceDispatch(directory, 'run', 'delivery')?.jobId, 'job')
  store(receipt)
  db.exec("UPDATE repository_scheduler_cancel_receipts SET response_json = '{'")
  assert.throws(() => failedDeviceDispatch(directory, 'run', 'delivery'), SyntaxError,
    'corrupt required cancellation evidence must not produce an invented terminal result')
})

test('failed dispatch is detected from the bound terminal scheduler job and retained as a failed cell', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-dispatch-failed-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await mkdir(resolve(directory, 'server-data'))
  const database = new DatabaseSync(resolve(directory, 'server-data/control-plane.sqlite3'))
  const workRunId = 'wrn_01J00000000000000000000001'
  const deliveryId = 'dlv_01J00000000000000000000001'
  const productSessionId = 'psn_01J00000000000000000000001'
  const runId = 'main-A:rust-001:glm-5.3-flash'
  const callId = `${runId}:model`
  const jobId = 'job_01J00000000000000000000001'
  try {
    database.exec(`CREATE TABLE scheduler_execution_jobs (job_id TEXT, delivery_id TEXT,
      work_run_id TEXT, state TEXT, attempt INTEGER, revision INTEGER, updated_at TEXT);
      INSERT INTO scheduler_execution_jobs VALUES ('${jobId}', '${deliveryId}', '${workRunId}',
        'queued', 1, 1, '2026-01-01T00:00:00Z')`)
    database.exec(`ALTER TABLE scheduler_execution_jobs ADD COLUMN payload_digest TEXT DEFAULT 'payload';
      ${dispatchLeaseSchema}`)
    assert.equal(failedDeviceDispatch(directory, workRunId, deliveryId), null)
    database.exec(`UPDATE scheduler_execution_jobs SET state='failed', revision=2,
      updated_at='2026-01-01T00:01:00Z'`)
    assert.equal(failedDeviceDispatch(directory, workRunId, 'dlv_wrong'), null)
    assert.equal(failedDeviceDispatch(directory, 'wrn_wrong', deliveryId), null)
    const failure = failedDeviceDispatch(directory, workRunId, deliveryId)
    assert.equal(failure?.jobId, jobId)
    assert.equal(failure?.state, 'failed')
    database.exec(`INSERT INTO scheduler_execution_jobs (job_id, delivery_id, work_run_id, state, attempt, revision, updated_at) VALUES ('job_retry', '${deliveryId}',
      '${workRunId}', 'queued', 2, 1, '2026-01-01T00:02:00Z')`)
    assert.equal(failedDeviceDispatch(directory, workRunId, deliveryId), null,
      'a newer retry must not be called terminal')
    database.exec("DELETE FROM scheduler_execution_jobs WHERE job_id='job_retry'")
    const report = { complete: false, productSessionId, deliveryId, workRunId,
      errorCode: 'DEVICE_DISPATCH_FAILED', dispatchFailure: failure,
      taskInputDigest: 'frozen-input', modelRoute: { modelId: 'glm-5.3-flash' },
      benchmarkConfiguration: benchmarkConfiguration('main-A') }
    await writeFile(resolve(directory, 'device-task-result.json'), JSON.stringify(report))
    await writeFile(resolve(directory, 'task-source-binding.json'), JSON.stringify({ runId,
      callId, taskId: 'rust-001', taskInputSha256: 'frozen-input' }))
    await writeFile(resolve(directory, 'product-source-seal.json'), '{}')
    const launch = { callId, directory, productSessionId, deliveryId }
    const result = failedDispatchDeviceResult({ runId, callId, taskId: 'rust-001',
      provider: 'glm-5.3-flash' }, launch, {})
    assert.equal(result.status, 'failed')
    assert.equal(result.failure.code, 'DEVICE_DISPATCH_FAILED')
    assert.equal(result.productComplete, false)
    assert.equal(result.candidate, null)
    assert.equal(result.externalScore, null)
    assert.deepEqual(result.dispatchFailure, failure)
    const cell = buildBenchmarkPlan({ taskIds: ['rust-001',
      ...Array.from({ length: 19 }, (_, index) => `other-${index}`)] })
      .cells.find(item => item.runId === runId)
    const plan = { cells: [cell] }
    const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'),
      experimentBinding: { experimentId: 'dispatch-failure-recovery' } }
    await assert.rejects(runBenchmarkPlan(plan, { ...options,
      executeCell: (_cell, runner) => {
        runner.registerLaunch(launch)
        throw Object.assign(new Error('driver interrupted'), { code: 'BENCHMARK_EVIDENCE_FAILED' })
      },
    }), { code: 'BENCHMARK_EVIDENCE_FAILED' })
    const recovered = await runBenchmarkPlan(plan, { ...options,
      executeCell: () => assert.fail('terminal dispatch must not execute again'),
      recoverCell: (value, observed) => recoverBenchmarkDeviceCell(value, observed, {}),
    })
    assert.equal(recovered.records[0].status, 'failed')
    assert.equal(recovered.records[0].failure.code, 'DEVICE_DISPATCH_FAILED')
    assert.deepEqual(recovered.records[0].calls[0].result.dispatchFailure, failure)
    const client = { query: async name => ({ result: name === 'delivery.get'
      ? { deliveryRevision: 3, status: 'ready', attention: [] }
      : { runs: [] } }) }
    await assert.rejects(driveDelivery(client, null, undefined, Date.now, {
      expectDeviceWorkRun: true, strictLaunchAnchor: true,
      pendingDeviceWorkRunIds: () => [workRunId],
      onPendingDeviceWorkRuns: ids => {
        const found = failedDeviceDispatch(directory, ids[0], deliveryId)
        if (found) throw Object.assign(new Error('terminal dispatch'), { code: 'DEVICE_DISPATCH_FAILED' })
      },
    }), { code: 'DEVICE_DISPATCH_FAILED' })
  } finally { database.close() }
})

test('dispatch failure reports preserve a model cause only from the failed job', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-model-dispatch-failure-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await mkdir(resolve(directory, 'server-data'))
  await mkdir(resolve(directory, 'device-data/providers'), { recursive: true })
  const server = new DatabaseSync(resolve(directory, 'server-data/control-plane.sqlite3'))
  const providers = new DatabaseSync(resolve(directory, 'device-data/providers/providers.sqlite3'))
  t.after(() => { server.close(); providers.close() })
  server.exec(`CREATE TABLE scheduler_execution_jobs (job_id TEXT, delivery_id TEXT,
    work_run_id TEXT, state TEXT, attempt INTEGER, revision INTEGER, updated_at TEXT);
    INSERT INTO scheduler_execution_jobs VALUES ('failed-job', 'delivery', 'work-run', 'failed', 1, 1,
      '2026-01-01T00:00:00Z');`)
  server.exec(`ALTER TABLE scheduler_execution_jobs ADD COLUMN payload_digest TEXT DEFAULT 'payload';
    ${dispatchLeaseSchema}`)
  providers.exec('CREATE TABLE exchanges(exchange_id TEXT,request_open TEXT,chunks TEXT)')
  for (const [exchangeId, jobId, code] of [
    ['failed-exchange', 'failed-job', 'DEVICE_PROVIDER_SSE_EVENT_INVALID'],
    ['other-exchange', 'other-job', 'DEVICE_PROVIDER_CONNECTION_FAILED'],
  ]) {
    const bytes = Buffer.from(JSON.stringify({ provider: 'deepseek', request: { model: 'deepseek-flash' } }))
    const opened = { modelExchangeId: exchangeId, lease: { jobId }, workerSessionId: 'session',
      request: { dataBase64: bytes.toString('base64'),
        payloadDigest: `sha256:${createHash('sha256').update(bytes).digest('hex')}` } }
    providers.prepare('INSERT INTO exchanges VALUES (?,?,?)').run(exchangeId, JSON.stringify(opened),
      JSON.stringify([{ modelExchangeId: exchangeId, isFinal: true,
        error: { code, message: 'private diagnostic', retryable: false } }]))
  }
  await writeFile(resolve(directory, 'task-source-binding.json'), JSON.stringify({
    runId: 'run', callId: 'run:model', taskId: 'rust-001' }))
  await writeFile(resolve(directory, 'product-source-seal.json'), '{}')
  const report = { complete: false, productSessionId: 'product-session', deliveryId: 'delivery',
    workRunId: 'work-run', errorCode: 'DEVICE_DISPATCH_FAILED',
    dispatchFailure: failedDeviceDispatch(directory, 'work-run', 'delivery') }
  const request = { runId: 'run', callId: 'run:model', taskId: 'rust-001', provider: 'deepseek-flash' }
  const launch = { callId: request.callId, directory, productSessionId: 'product-session', deliveryId: 'delivery' }
  for (const errorCode of ['DEVICE_DISPATCH_FAILED']) {
    await writeFile(resolve(directory, 'device-task-result.json'), JSON.stringify({ ...report, errorCode }))
    const result = failedDispatchDeviceResult(request, launch, {})
    assert.deepEqual(result.failure, { code: 'DEVICE_DISPATCH_FAILED', status: 'failed',
      observedProviderFailures: [{ exchangeId: 'failed-exchange', jobId: 'failed-job',
        code: 'DEVICE_PROVIDER_SSE_EVENT_INVALID', retryable: false }] })
    assert.equal(result.executionReceipts.calls.length, 2)
    assert.equal(JSON.stringify(result).includes('private diagnostic'), false)
  }
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
  `, new URL('../scripts/benchmark/run-real-task-benchmark.mjs', import.meta.url).href, JSON.stringify(plan), JSON.stringify(options)],
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

test('recovered product calls are committed to the call table with the final record', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-recovered-call-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i}`) }).cells[0]
  const plan = { cells: [cell] }
  const ledgerPath = resolve(directory, 'ledger.sqlite3')
  const options = { ledgerPath, experimentBinding: { experimentId: 'recovered-call' } }
  const launch = { callId: `${cell.runId}:model`, directory: resolve(directory, 'product'),
    productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001' }
  const child = spawnSync(process.execPath, ['--input-type=module', '-e', `
    const { runBenchmarkPlan } = await import(process.argv[1]);
    await runBenchmarkPlan(JSON.parse(process.argv[2]), { ...JSON.parse(process.argv[3]),
      executeCell: (_cell, context) => {
        context.registerLaunch(JSON.parse(process.argv[4]));
        process.exit(86);
      },
    });
  `, new URL('../scripts/benchmark/run-real-task-benchmark.mjs', import.meta.url).href,
  JSON.stringify(plan), JSON.stringify(options), JSON.stringify(launch)], { encoding: 'utf8' })
  assert.equal(child.status, 86, child.stderr)

  const result = await runBenchmarkPlan(plan, { ...options,
    executeCell: () => assert.fail('product must not be executed again'),
    recoverCell: (value, observed) => recoverBenchmarkCell(value, observed, async (_request, target) => {
      assert.deepEqual(target, launch)
      return { status: 'failed', failure: { code: 'DEVICE_PRODUCT_FAILED' }, productComplete: false }
    }),
  })
  const database = new DatabaseSync(ledgerPath, { readOnly: true })
  try {
    const calls = database.prepare('SELECT record FROM benchmark_call ORDER BY rowid').all()
      .map(row => JSON.parse(row.record))
    assert.deepEqual(calls, result.records[0].calls)
    assert.equal(calls.length, 1)
    assert.equal(calls[0].status, 'returned')
  } finally { database.close() }
})

test('a prelaunch crash becomes an explicit failed row without starting a product', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-prelaunch-crash-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, i) => `task-${i}`) }).cells[0]
  const plan = { cells: [cell] }
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'),
    experimentBinding: { experimentId: 'prelaunch-crash' } }
  const child = spawnSync(process.execPath, ['--input-type=module', '-e', `
    const { runBenchmarkPlan } = await import(process.argv[1]);
    await runBenchmarkPlan(JSON.parse(process.argv[2]), { ...JSON.parse(process.argv[3]),
      executeCell: () => process.exit(86),
    });
  `, new URL('../scripts/benchmark/run-real-task-benchmark.mjs', import.meta.url).href,
  JSON.stringify(plan), JSON.stringify(options)], { encoding: 'utf8' })
  assert.equal(child.status, 86, child.stderr)
  const result = await runBenchmarkPlan(plan, { ...options,
    executeCell: () => assert.fail('claimed cell must not be retried'),
    recoverCell: recoverBenchmarkCell })
  assert.equal(result.records[0].status, 'failed')
  assert.equal(result.records[0].failure.code, 'BENCHMARK_UNLAUNCHED_CLAIM')
  assert.equal(result.records[0].recovery.kind, 'unlaunched-claim')
  assert.deepEqual(result.records[0].launches, [])
  assert.deepEqual(result.records[0].calls, [])
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
  const stoppedCall = {
    callId: calls[0].callId, status: 'failed', failure: { code: 'STUCK_TOOL_REPEAT_LIMIT' },
  }
  const stopped = await recoverBenchmarkCell(cell, { launches: [], calls: [stoppedCall, ...calls.slice(1, 4)] })
  assert.equal(stopped.status, 'failed')
  assert.equal(stopped.termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  await assert.rejects(recoverBenchmarkCell(cell, { launches: [], calls: [stoppedCall] }), { code: 'LEDGER_RUN_UNRESOLVED' })
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
  assert.equal(attempts, 4)
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
    providerEnvironment: {},
    agentSettings: { jevContext: { provider: 'context', policy: { version: 'frozen' } },
      jevJudge: 'judge', jevSettingsFile: resolve(directory, 'private-settings.json') } }),
  { code: 'BENCHMARK_CONFIGURATION_UNAVAILABLE' })
  await assert.rejects(access(evidenceRoot), { code: 'ENOENT' })
})

test('formal Device entry validates automatic approval mode before reading inputs or opening a runtime', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-approval-preflight-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  for (const automaticTaskActions of [null, 'true', 1, {}]) {
    await assert.rejects(executeDeviceBenchmark({ automaticTaskActions,
      preparedInputsDirectory: resolve(directory, 'missing-inputs'), evidenceRoot: resolve(directory, 'runtime') }),
    /task action authorization must be explicit/u)
  }
  await assert.rejects(access(resolve(directory, 'runtime')), { code: 'ENOENT' })
})

test('Device approval policy is frozen before effects and same-mode recovery preserves original claims', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-approval-binding-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = { cells: [{ runId: 'original' }, { runId: 'next' }] }
  const providerEvidence = ['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash'].map(model => ({
    requestedModelId: model, observedModelId: model, endpoint: 'https://provider.invalid/messages',
    credentialPresent: true, supportsReasoningEffort: 'max',
  }))
  const bindingOptions = { experimentId: 'approval-policy', agentSettings: {}, providerEvidence,
    frozenSourceIdentity: {}, productSourceSealSha256: 'fixture-seal' }
  const binding = automaticTaskActions => deviceBenchmarkExperimentBinding(
    { ...bindingOptions, automaticTaskActions }, { revision: 'fixture-revision' }, { tasks: [] })
  assert.deepEqual(binding(undefined), binding(false), 'omitted mode means explicit false')
  for (const originalMode of [false, true]) {
    const options = { ledgerPath: resolve(directory, `${originalMode}.sqlite3`),
      providerEvidence, experimentBinding: binding(originalMode) }
    const target = { callId: 'original:model', directory: resolve(directory, `product-${originalMode}`),
      productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001' }
    const child = spawnSync(process.execPath, ['--input-type=module', '-e', `
      const { runBenchmarkPlan } = await import(process.argv[1]);
      await runBenchmarkPlan(JSON.parse(process.argv[2]), { ...JSON.parse(process.argv[3]),
        executeCell: (_cell, context) => {
          context.registerLaunch(JSON.parse(process.argv[4]));
          process.exit(86);
        },
      });
    `, new URL('../scripts/benchmark/run-real-task-benchmark.mjs', import.meta.url).href,
    JSON.stringify(plan), JSON.stringify(options), JSON.stringify(target)], { encoding: 'utf8' })
    assert.equal(child.status, 86, child.stderr)
    const snapshot = () => {
      const database = new DatabaseSync(options.ledgerPath, { readOnly: true })
      try {
        return { cells: database.prepare('SELECT * FROM benchmark_cell ORDER BY ordinal').all(),
          launches: database.prepare('SELECT * FROM benchmark_launch').all(),
          calls: database.prepare('SELECT * FROM benchmark_call').all() }
      } finally { database.close() }
    }
    const before = snapshot()
    const changed = { ...options, experimentBinding: binding(!originalMode) }
    assert.throws(() => verifyBenchmarkLedgerIdentity(plan, changed), { code: 'LEDGER_IDENTITY_MISMATCH' })
    let effects = 0
    await assert.rejects(runBenchmarkPlan(plan, { ...changed,
      executeCell: () => { effects += 1 }, recoverCell: () => { effects += 1 } }),
    { code: 'LEDGER_IDENTITY_MISMATCH' })
    assert.equal(effects, 0, 'policy changes cannot recover, approve or execute any cell')
    assert.deepEqual(snapshot(), before, 'original claim, launch and unopened next cell remain unchanged')
    verifyBenchmarkLedgerIdentity(plan, options)
    await assert.rejects(runBenchmarkPlan(plan, { ...options, executeCell: () => assert.fail('unknown call cannot restart') }),
    error => {
      assert.equal(error.code, 'LEDGER_RUN_UNRESOLVED')
      assert.deepEqual(error.launches, [target])
      return true
    })
    const result = await runBenchmarkPlan(plan, { ...options,
      recoverCell: (_cell, observed) => {
        assert.deepEqual(observed.launches, [target])
        return { status: 'completed', recovery: { kind: 'fixture-original-result' } }
      },
      executeCell: cell => { assert.equal(cell.runId, 'next'); effects += 1; return { status: 'completed' } },
    })
    assert.equal(effects, 1)
    assert.deepEqual(result.records[0].launches, [target])
    assert.deepEqual(await runBenchmarkPlan(plan, { ...options,
      executeCell: () => assert.fail('completed calls cannot restart') }), result)
  }
})

test('rejected formal preflight leaves no frozen identity and corrected configuration can use the same directory', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-invalid-first-config-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = { cells: [{ runId: 'first' }] }
  const providers = ['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash', 'qwen3.8-flash'].map(model => ({
    requestedModelId: model, observedModelId: model, endpoint: 'https://provider.invalid/messages',
    credentialPresent: true, supportsReasoningEffort: 'max',
  }))
  const cases = [
    { providerEvidence: providers.map((row, i) => i === 1 ? { ...row, observedModelId: 'wrong-model' } : row),
      experimentId: 'same-experiment', code: 'MODEL_IDENTITY_MISMATCH' },
    { providerEvidence: null, experimentId: 'same-experiment', code: 'MODEL_IDENTITY_MISSING' },
    { providerEvidence: providers, experimentId: undefined, code: 'LEDGER_REQUIRED' },
  ]
  await writeFile(resolve(directory, 'prepared-inputs.json'), JSON.stringify({
    tasks: Array.from({ length: 20 }, (_, i) => ({ taskId: `task-${i}` })),
  }))
  const providerEnvironment = Object.fromEntries(['ZHIPU', 'XIAOMI', 'DEEPSEEK', 'OPENCODE'].flatMap((prefix, index) => [
    [`${prefix}_API_KEY`, 'fixture-only'], [`${prefix}_BASE_URL`, 'https://provider.invalid'],
    [`${prefix}_MODEL`, providers[index].requestedModelId],
  ]))
  for (const [index, invalid] of cases.entries()) {
    const evidenceRoot = resolve(directory, `device-${index}`)
    await assert.rejects(executeDeviceBenchmark({ ...invalid, automaticTaskActions: true, evidenceRoot,
      preparedInputsDirectory: directory, sourceRoot: resolve(directory, 'missing-source'), providerEnvironment,
      agentSettings: { jevSettingsFile: resolve(directory, 'unused-settings'),
        jevContext: { provider: 'fixture-context', policy: {} }, jevJudge: 'fixture-judge' },
    }), { code: invalid.code })
    await assert.rejects(access(evidenceRoot), { code: 'ENOENT' })
    const options = evidence => ({ providerEvidence: evidence.providerEvidence,
      ledgerPath: resolve(directory, `${index}.sqlite3`),
      experimentBinding: deviceBenchmarkExperimentBinding({ ...evidence, automaticTaskActions: true,
        agentSettings: {}, frozenSourceIdentity: {}, productSourceSealSha256: 'fixture-seal' }, {}, {}),
    })
    const refused = options(invalid)
    assert.throws(() => verifyBenchmarkLedgerIdentity(plan, refused), { code: invalid.code })
    await assert.rejects(executeFormalBenchmark(plan, { ...refused,
      executeCell: () => assert.fail('invalid evidence cannot claim or execute a cell') }), { code: invalid.code })
    await assert.rejects(access(refused.ledgerPath), { code: 'ENOENT' })
    const corrected = options({ providerEvidence: providers, experimentId: 'same-experiment' })
    verifyBenchmarkLedgerIdentity(plan, corrected)
    let executed = 0
    const result = await executeFormalBenchmark(plan, { ...corrected,
      executeCell: cell => { executed += 1; assert.equal(cell.runId, 'first'); return { status: 'completed' } },
    })
    assert.equal(executed, 1)
    assert.equal(result.records[0].status, 'completed')
  }
  await assert.rejects(executeDeviceBenchmark({ preparedInputsDirectory: directory,
    evidenceRoot: resolve(directory, 'missing-evidence'), providerEnvironment,
    agentSettings: { jevSettingsFile: resolve(directory, 'unused-settings'),
      jevContext: { provider: 'fixture-context', policy: {} }, jevJudge: 'fixture-judge' },
  }), { code: 'MODEL_IDENTITY_MISSING' })
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

test('Fusion members overlap and aggregation joins every result in provider order', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) })
  const cell = plan.cells[4]
  const providers = plan.cells.slice(0, 4).map(value => value.comparison)
  const releases = new Map()
  const receipts = new Map()
  const started = []
  const calls = []
  let aggregations = 0
  const execution = executeBenchmarkCell(cell, {
    runModel: async request => {
      started.push(request.provider)
      await new Promise(resolve => releases.set(request.provider, resolve))
      return { provider: request.provider, answer: request.provider }
    },
    aggregate: async request => {
      aggregations += 1
      assert.deepEqual(request.members.map(member => member.provider), providers)
      assert.equal(calls.length, 4)
      assert.equal(request.inputDigest, benchmarkAggregationDigest(cell.runId, cell.taskId, request.members))
      return { answer: 'aggregated' }
    },
  }, { recordCall: call => {
    calls.push(call)
    receipts.get(call.callId)?.()
  } })
  try {
    assert.deepEqual(started, providers, 'all four members must start before any member finishes')
    for (const provider of [providers[3], providers[1], providers[2]]) {
      const receipt = new Promise(resolve => receipts.set(`${cell.runId}:member:${provider}`, resolve))
      releases.get(provider)()
      await receipt
      assert.equal(aggregations, 0, 'aggregation must wait for the slowest member')
    }
    releases.get(providers[0])()
    const result = await execution
    assert.equal(aggregations, 1)
    assert.deepEqual(result.members.map(member => member.provider), providers)
    assert.deepEqual(calls.map(call => call.callId), [
      ...[providers[3], providers[1], providers[2], providers[0]].map(provider => `${cell.runId}:member:${provider}`),
      `${cell.runId}:aggregation`,
    ])
  } finally {
    for (const release of releases.values()) release()
    await execution.catch(() => {})
  }
})

test('a Fusion member stop waits for launched siblings to retain their results', async () => {
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) }).cells[4]
  const releases = []
  const calls = []
  let started = 0
  let settled = false
  const stop = { termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' } }
  const execution = executeBenchmarkCell(cell, {
    runModel: async request => {
      const index = started++
      if (index === 0) return stop
      await new Promise(resolve => releases.push(resolve))
      return { status: 'completed', provider: request.provider }
    },
    aggregate: () => assert.fail('a task stop must prevent aggregation'),
  }, { recordCall: call => calls.push(call) })
  const observed = execution.then(() => { settled = true }, error => { settled = true; return error })
  try {
    await new Promise(resolve => setImmediate(resolve))
    assert.equal(started, 4)
    assert.equal(settled, false, 'a stop must not close the cell while siblings still run')
    assert.equal(calls.length, 1)
    for (const release of releases) release()
    const error = await observed
    assert.equal(error.code, 'STUCK_TOOL_REPEAT_LIMIT')
    assert.equal(calls.length, 4)
    assert.deepEqual(calls[0].result, stop)
    assert.equal(calls.slice(1).every(call => call.status === 'returned'), true)
  } finally {
    for (const release of releases) release()
    await observed
  }
})

test('Fusion recovery preserves out-of-order durable member receipts', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-parallel-recovery-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) })
  const cell = plan.cells[4]
  const providers = plan.cells.slice(0, 4).map(value => value.comparison)
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'),
    experimentBinding: { experimentId: 'parallel-recovery' } }
  const releases = new Map()
  const receipts = new Map()
  const calls = []
  const execution = runBenchmarkPlan({ cells: [cell] }, { ...options,
    executeCell: (value, context) => executeBenchmarkCell(value, {
      runModel: async request => {
        await new Promise(resolve => releases.set(request.provider, resolve))
        return request.provider === providers[0]
          ? { termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' } }
          : { status: 'completed', provider: request.provider }
      },
      aggregate: () => assert.fail('a stopped member must not aggregate'),
    }, { ...context, recordCall: call => {
      context.recordCall(call)
      calls.push(call)
      receipts.get(call.callId)?.()
    } }).catch(error => {
      assert.equal(error.code, 'STUCK_TOOL_REPEAT_LIMIT')
      // Preserve the unfinished claim as if the coordinator stopped before
      // committing the cell, after every member receipt was already durable.
      throw Object.assign(new Error('coordinator interrupted'), { code: 'BENCHMARK_EVIDENCE_FAILED' })
    }),
  }).catch(error => error)
  try {
    assert.equal(releases.size, 4)
    for (const provider of [...providers].reverse()) {
      const receipt = new Promise(resolve => receipts.set(`${cell.runId}:member:${provider}`, resolve))
      releases.get(provider)()
      await receipt
    }
    assert.equal((await execution).code, 'BENCHMARK_EVIDENCE_FAILED')
    const recovered = await runBenchmarkPlan({ cells: [cell] }, { ...options,
      executeCell: () => assert.fail('recovery must not launch new model work'),
      recoverCell: recoverBenchmarkCell,
    })
    assert.equal(recovered.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
    assert.deepEqual(recovered.records[0].calls, calls)
    assert.deepEqual(recovered.records[0].calls.map(call => call.callId),
      [...providers].reverse().map(provider => `${cell.runId}:member:${provider}`))
  } finally {
    for (const release of releases.values()) release()
    await execution
  }
})

test('a Fusion member stop cannot hide a sibling evidence or unresolved-ledger failure', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-mixed-stop-failure-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const full = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) })
  const cell = full.cells[4]
  const providers = full.cells.slice(0, 4).map(value => value.comparison)
  const plan = { cells: [cell, full.cells[0]] }
  for (const code of ['BENCHMARK_EVIDENCE_FAILED', 'LEDGER_RUN_UNRESOLVED']) {
    await t.test(code, async () => {
      const options = { ledgerPath: resolve(directory, `${code}.sqlite3`),
        experimentBinding: { experimentId: code } }
      let models = 0
      let nextCellStarted = false
      await assert.rejects(runBenchmarkPlan(plan, { ...options,
        executeCell: (value, context) => {
          if (value.runId !== cell.runId) { nextCellStarted = true; assert.fail('a recovery error must stop the batch') }
          return executeBenchmarkCell(value, {
            runModel: async request => {
              models += 1
              if (request.provider === providers[0]) return { termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' } }
              if (request.provider === providers[1]) throw Object.assign(new Error('member recovery required'), { code })
              return { status: 'completed', provider: request.provider }
            },
            aggregate: () => assert.fail('recovery errors must prevent aggregation'),
          }, context)
        },
      }), { code })
      assert.equal(models, 4)
      assert.equal(nextCellStarted, false)
      await assert.rejects(runBenchmarkPlan(plan, { ...options,
        executeCell: () => assert.fail('the original claim must remain unresolved'),
      }), error => {
        assert.equal(error.code, 'LEDGER_RUN_UNRESOLVED')
        assert.equal(error.calls.length, 4)
        assert.equal(error.calls.find(call => call.callId.endsWith(providers[1])).failure.code, code)
        return true
      })
    })
  }
})

test('one Fusion launch persistence failure preserves its siblings returned receipts', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) })
  const cell = plan.cells[4]
  const providers = plan.cells.slice(0, 4).map(value => value.comparison)
  const calls = []
  await assert.rejects(executeBenchmarkCell(cell, {
    runModel: async (request, runner) => {
      try { await runner.registerLaunch({ callId: request.callId }) } catch { /* Own launch failure must remain fatal. */ }
      return { status: 'completed', provider: request.provider, answer: `answer-${request.provider}` }
    },
    aggregate: () => assert.fail('a launch persistence failure must prevent aggregation'),
  }, {
    registerLaunch: target => {
      if (target.callId.endsWith(providers[0])) throw Object.assign(new Error('launch disk failure'), { code: 'ENOSPC' })
    },
    recordCall: call => calls.push(call),
  }), { code: 'ENOSPC' })
  assert.equal(calls.length, 4)
  assert.equal(calls.find(call => call.callId.endsWith(providers[0])).status, 'failed')
  for (const provider of providers.slice(1)) {
    const call = calls.find(value => value.callId.endsWith(provider))
    assert.equal(call.status, 'returned')
    assert.deepEqual(call.result, { status: 'completed', provider, answer: `answer-${provider}` })
  }
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

test('a stuck tool request terminates only its task and retains all 700 rows', async () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  let cellCalls = 0
  const ledger = await runBenchmarkPlan(plan, {
    executeCell: async () => {
      cellCalls += 1
      return cellCalls === 1
        ? { status: 'failed', termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' } }
        : { status: 'completed' }
    },
  })

  assert.equal(cellCalls, 700)
  assert.equal(ledger.records.length, 700)
  assert.equal(ledger.records[0].status, 'failed')
  assert.equal(ledger.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(ledger.records.slice(1).every(record => record.status === 'completed' && record.termination === null), true)
})

test('a task termination cannot be published as success or inherited by its neighbor', async () => {
  const plan = {
    cells: [
      { runId: 'run-terminated', claims: [{ id: 'claim:kept', state: 'disputed' }] },
      { runId: 'run-pending', claims: [] },
    ],
  }
  const ledger = await runBenchmarkPlan(plan, {
    executeCell: async cell => cell.runId === 'run-pending' ? { status: 'completed' } : ({
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
  assert.equal(ledger.records[1].status, 'completed')
  assert.equal(ledger.records[1].termination, null)
  assert.equal(ledger.records[1].verdict, null)
  assert.equal(ledger.records[1].score, null)
})

test('parallel coordinator continues after a durable task stop and drains admitted tasks on storage failure', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-coordinator-stop-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const cells = Array.from({ length: 4 }, (_, index) => ({ runId: `task-${index}` }))
  const executed = []
  const results = await runBenchmarkSchedule(cells, {
    concurrency: 2,
    executeCell: (cell, index) => runBenchmarkPlan({ cells: [cell] }, {
      ledgerPath: resolve(directory, `${index}.sqlite3`), experimentBinding: { experimentId: 'task-stop' },
      executeCell: () => {
        executed.push(cell.runId)
        return index === 0 ? { termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' } } : { status: 'completed' }
      },
    }),
  })
  assert.deepEqual(executed.toSorted(), cells.map(cell => cell.runId))
  assert.deepEqual(results.map(result => result.records[0].status), ['failed', 'completed', 'completed', 'completed'])
  await runBenchmarkSchedule(cells, { concurrency: 2, executeCell: (cell, index) => runBenchmarkPlan({ cells: [cell] }, {
    ledgerPath: resolve(directory, `${index}.sqlite3`), experimentBinding: { experimentId: 'task-stop' },
    executeCell: () => assert.fail('coordinator restart must reuse retained task results'),
  }) })

  let release
  let admitted = 0
  let drained = false
  const pending = new Promise(resolve => { release = resolve })
  const storageFailure = Object.assign(new Error('evidence disk failure'), { code: 'BENCHMARK_EVIDENCE_FAILED' })
  const admissionFailures = []
  const failed = runBenchmarkSchedule(cells, { concurrency: 2,
    onAdmissionFailure: failure => admissionFailures.push(failure),
    executeCell: async (_cell, index) => {
    admitted += 1
    if (index === 0) throw storageFailure
    await pending
    drained = true
    return { status: 'completed' }
  } })
  await new Promise(resolve => setImmediate(resolve))
  assert.equal(admitted, 2)
  assert.equal(drained, false)
  const rejected = assert.rejects(failed, error => error === storageFailure && drained)
  const failureBeforeDrain = admissionFailures[0]
  release()
  await rejected
  assert.equal(admissionFailures.length, 1)
  assert.deepEqual(failureBeforeDrain, { error: storageFailure, cell: cells[0], index: 0 })
  assert.equal(admitted, 2)
  let releaseAfterObserverFailure
  let observerFailureDrained = false
  const observerPending = new Promise(resolve => { releaseAfterObserverFailure = resolve })
  const observerFailure = Object.assign(new Error('progress export failed'), { code: 'ENOSPC' })
  let failureNotification
  const planFailure = runBenchmarkPlan({ cells }, {
    concurrency: 2,
    executeCell: async (_cell, _runner) => {
      if (!failureNotification) {
        failureNotification = 'started'
        throw storageFailure
      }
      await observerPending
      observerFailureDrained = true
      return { status: 'completed' }
    },
    onAdmissionFailure: failure => {
      failureNotification = failure
      throw observerFailure
    },
  })
  const rejectedObserver = assert.rejects(planFailure,
    error => error === observerFailure && observerFailureDrained)
  await new Promise(resolve => setImmediate(resolve))
  assert.equal(observerFailureDrained, false)
  assert.equal(failureNotification.error, storageFailure, 'plan must forward the admission failure hook')
  releaseAfterObserverFailure()
  await rejectedObserver
  await assert.rejects(runBenchmarkSchedule(cells, { concurrency: 0, executeCell: () => assert.fail() }),
    { code: 'BENCHMARK_CONCURRENCY_INVALID' })
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

test('tool admission stops only the offending task and preserves each task claims', async () => {
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
  const executions = []

  const ledger = await runBenchmarkPlan(plan, {
    executeCell: async (cell, { toolGate }) => {
      cellCalls += 1
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
          for (let occurrence = 1; occurrence <= (cell.runId === 'run-1' ? 6 : 1); occurrence += 1) {
            await runner.requestTool(toolRequest, async () => { executions.push(cell.runId); return { occurrence } })
          }
          return { answer: 'done' }
        },
      }, { toolGate })
    },
  })

  assert.equal(cellCalls, 2)
  assert.deepEqual(executions, ['run-1', 'run-1', 'run-1', 'run-1', 'run-1', 'run-2'])
  assert.equal(ledger.denominator, 2)
  assert.equal(ledger.records.length, 2)
  assert.equal(ledger.records[0].status, 'failed')
  assert.equal(ledger.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.deepEqual(ledger.records[0].claims, firstClaims)
  assert.equal(ledger.records[1].status, 'completed')
  assert.equal(ledger.records[1].termination, null)
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
  assert.equal(report.time.execution.totalWallMs, 150)
  assert.equal(report.contextEffects.rebuildCount, 1)
  assert.equal(report.contextEffects.evidenceStatus, 'insufficient_evidence')
  assert.deepEqual(report.callAccounting, { modelCalls: 1, fusionEngineCalls: 1,
    jevModelCalls: 0, unmeasuredProductCalls: 0 })
  assert.equal(report.gates.regret.status, 'fail')
  assert.equal(report.gates.minority.status, 'insufficient_evidence')
})

test('formal report recomputes actual nested receipts and leaves missing cost or tokens unknown', () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const records = plan.cells.map((cell, index) => ({ ...cell,
    status: index === 0 ? 'completed' : 'failed', verdict: null, score: null,
    wallMs: index === 0 ? 2000 : 0, calls: index === 0 ? [{
      callId: `${cell.runId}:model`, status: 'returned', result: { executionReceipts: {
        calls: [{ exchangeId: 'observed-1', requestedModel: 'glm-5.3-flash',
          actualModels: ['glm-5.3-flash'], reasoningEffort: 'max', terminalType: 'completed',
          usage: { inputTokens: 11, outputTokens: 4, totalTokens: 15, cachedTokens: 3 } },
        { exchangeId: 'observed-2', requestedModel: 'glm-5.3-flash',
          actualModels: [], reasoningEffort: 'max', terminalType: 'error', usage: null }],
        jev: [{ exchangeId: 'observed-1', requestedModel: 'jev-latest', actualModel: 'jev-1.13.0',
          inputTokens: 2, outputTokens: 1, failureCount: 0 }],
        performance: [{ modelCalls: 2, totalRuntimeMs: 1500, modelWaitMs: 800,
          toolMs: 200, actualCostMicros: null }],
      } },
    }] : [] }))
  const report = aggregateBenchmarkReport(plan, { records, experimentId: 'actual-receipts' })
  assert.equal(report.callAccounting.modelCalls, 3)
  assert.equal(report.callAccounting.jevModelCalls, 1)
  assert.equal(report.tokenAndCache.inputTokens, null)
  assert.equal(report.tokenAndCache.measured.inputTokens, 13)
  assert.equal(report.tokenAndCache.unknownUsageExchanges, 1)
  assert.equal(report.cost.costUsd, null)
  assert.ok(report.cost.unknownCostSources > 0)
  assert.equal(report.time.callWallMs, 1500)
  assert.equal(report.time.modelWaitMs, 800)
  assert.equal(report.time.toolMs, 200)
  assert.equal(report.time.endToEnd.totalWallMs, 2000)
  assert.equal(report.time.execution.totalWallMs, 2000)
  const failedReport = aggregateBenchmarkReport(plan, { records: records.map((record, index) =>
    index === 0 ? { ...record, status: 'failed' } : record), experimentId: 'actual-failed-receipts' })
  assert.equal(failedReport.time.endToEnd.totalWallMs, null)
  assert.equal(failedReport.time.execution.totalWallMs, 2000)
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
  const preparedInputsDirectory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-prepared-'))
  t.after(() => rm(preparedInputsDirectory, { recursive: true, force: true }))
  const evidenceRoot = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-aggregation-goal-'))
  t.after(() => rm(evidenceRoot, { recursive: true, force: true }))
  const artifact = { taskId: 'task-1', sourceRevision: revision,
    sourceDigest: source.sourceDigest, imageId: `sha256:${'a'.repeat(64)}`,
    spec: { id: 'task-1', title: 'Fixture', entry: 'main.py', allowed_suffixes: ['.py'],
      max_submission_files: 10, max_submission_bytes: 1024 },
    files: { 'main.py': 'print(1)\n', 'TASK.md': 'Implement fixture\n', 'PROTOCOL.md': 'JSONL\n' } }
  const artifactBytes = JSON.stringify(artifact)
  await writeFile(resolve(preparedInputsDirectory, 'task-1.json'), artifactBytes)
  await writeFile(resolve(preparedInputsDirectory, 'prepared-inputs.json'), JSON.stringify({
    source, tasks: [{ taskId: 'task-1', sha256: createHash('sha256').update(artifactBytes).digest('hex'),
      imageId: artifact.imageId }],
  }))
  const aggregation = { inputDigest: 'b'.repeat(64), members: [
    ...['glm-5.3-flash', 'mimo-v2.6-pro', 'deepseek-flash'].map(provider => ({
      provider, callId: `main-A:task-1:fusion-4:member:${provider}`, status: 'completed',
      candidate: { files: [{ path: 'main.py', content: `${provider}\n${'源码'.repeat(6000)}\0` }] },
    })),
    { provider: 'qwen3.8-flash', callId: 'main-A:task-1:fusion-4:member:qwen3.8-flash',
      status: 'failed', failure: { code: 'FIXTURE_FAILURE' } },
  ] }
  const prepared = await prepareBenchmarkDeviceTask({ taskId: 'task-1',
    runId: 'main-A:task-1:fusion-4', callId: 'main-A:task-1:fusion-4:aggregation',
    comparison: 'fusion-4' }, { preparedInputsDirectory, sourceRoot: repositoryRoot,
    evidenceRoot, aggregation, publicSmokeExecutionLock: resolve(evidenceRoot, 'scoring.lock') })
  const aggregationTask = JSON.parse(await readFile(prepared.taskInputPath, 'utf8'))
  assert.deepEqual(prepared.mcpConfiguration.args.slice(-2),
    ['--execution-lock', resolve(evidenceRoot, 'scoring.lock')])
  assert.ok(aggregationTask.verificationCommand.includes("'--execution-lock'"))
  assert.ok(aggregationTask.verificationCommand.includes(resolve(evidenceRoot, 'scoring.lock')))
  assert.match(aggregationTask.verificationCommand, /'--verify-source' '\.'/u,
    'independent verification must read host receipts without executing Docker or writing evidence')
  assert.ok(!aggregationTask.verificationCommand.includes('--run-source'))
  assert.equal([...aggregationTask.goal].some(character => character.codePointAt(0) < 32), false,
    'aggregation WorkRun goal must satisfy the Worker prompt contract')
  assert.ok(aggregationTask.goal.includes('独立四模型候选的一次聚合'))
  assert.ok([...aggregationTask.goal].length < 20000, 'large candidates must stay outside the goal')
  const aggregationPath = 'benchmark-inputs/aggregation.json'
  const aggregationBytes = aggregationTask.files[aggregationPath]
  assert.equal(typeof aggregationBytes, 'string', 'input must reach the seeded task repository')
  assert.deepEqual(JSON.parse(aggregationBytes), aggregation, 'all four outcomes and source bytes survive')
  assert.ok(aggregationTask.goal.includes(aggregationPath))
  assert.ok(!aggregationTask.goal.includes('FIXTURE_FAILURE'))
  const binding = JSON.parse(await readFile(resolve(prepared.directory, 'task-source-binding.json'), 'utf8'))
  assert.deepEqual(binding.aggregationInputFile, { path: aggregationPath,
    sha256: createHash('sha256').update(aggregationBytes).digest('hex') })
  assert.deepEqual(aggregationTask.files['TASK.md'], artifact.files['TASK.md'])
  assert.ok(aggregationTask.outOfScope.includes(aggregationPath))
  assert.ok(aggregationTask.constraints.some(value => value.includes(aggregationPath)))
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


test('one 700-cell ledger admits twelve cells together and retains plan order after out-of-order completion', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-parallel-ledger-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'parallel' } }
  const releases = []
  const executed = []
  const returned = []
  const running = runBenchmarkPlan(plan, { ...options, concurrency: 12,
    executeCell: cell => {
      const index = executed.length
      executed.push(cell.runId)
      const result = index === 0 ? { termination: { reason: 'STUCK_TOOL_REPEAT_LIMIT' } } : { status: 'completed' }
      return index < 12 ? new Promise(resolvePromise => { releases[index] = () => resolvePromise(result) }) : result
    },
    onRecord: record => { returned.push(record.runId) },
  })
  let result
  try {
    await new Promise(resolvePromise => setImmediate(resolvePromise))
    assert.equal(executed.length, 12, 'every slot must start before the first answer')
    assert.deepEqual(returned, [])
    const database = new DatabaseSync(options.ledgerPath, { readOnly: true })
    try {
      assert.deepEqual({ ...database.prepare(`SELECT count(*) AS total,
        sum(token IS NOT NULL) AS claimed, sum(record IS NOT NULL) AS finished FROM benchmark_cell`).get() },
      { total: 700, claimed: 12, finished: 0 })
    } finally { database.close() }
    for (const release of releases.toReversed()) {
      release()
      await new Promise(resolvePromise => setImmediate(resolvePromise))
    }
    result = await running
  } finally {
    for (const release of releases) release()
    await running
  }
  assert.equal(result.denominator, 700)
  assert.deepEqual(result.records.map(record => record.runId), plan.cells.map(cell => cell.runId))
  assert.equal(returned[0], plan.cells[11].runId)
  assert.equal(returned.at(-1), plan.cells[0].runId)
  assert.equal(result.records[0].status, 'failed')
  assert.equal(result.records.slice(1).every(record => record.status === 'completed'), true)
  assert.equal(new Set(executed).size, 700)
  const retained = await runBenchmarkPlan(plan, { ...options, concurrency: 3,
    executeCell: () => assert.fail('retained parallel results cannot execute again'),
  })
  assert.deepEqual(retained, result)
})

test('parallel ledger stops new claims on a hard evidence error and drains its already running cells', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-parallel-drain-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'drain' } }
  const releases = []
  const executed = []
  const retained = []
  const failure = Object.assign(new Error('evidence unavailable'), { code: 'BENCHMARK_EVIDENCE_FAILED' })
  let settled = false
  const running = runBenchmarkPlan(plan, { ...options, concurrency: 3,
    executeCell: cell => {
      const index = executed.length
      executed.push(cell.runId)
      if (index === 0) throw failure
      return new Promise(resolvePromise => { releases.push(() => resolvePromise({ status: 'completed' })) })
    },
    onRecord: record => { retained.push(record.runId) },
  })
  const rejected = assert.rejects(running, error => error === failure && retained.length === 2)
    .then(() => { settled = true })
  try {
    await new Promise(resolvePromise => setImmediate(resolvePromise))
    assert.equal(executed.length, 3)
    assert.equal(settled, false, 'the store must remain open for the two pending cells')
  } finally {
    for (const release of releases) release()
    await rejected
  }
  const database = new DatabaseSync(options.ledgerPath, { readOnly: true })
  try {
    const rows = database.prepare('SELECT ordinal, token, record FROM benchmark_cell ORDER BY ordinal').all()
    assert.equal(rows.length, 700)
    assert.notEqual(rows[0].token, null)
    assert.equal(rows[0].record, null, 'the unresolved evidence failure must not become a model result')
    assert.equal(rows.slice(1, 3).every(row => JSON.parse(row.record).status === 'completed'), true)
    assert.equal(rows.slice(3).every(row => row.token === null && row.record === null), true)
  } finally { database.close() }
  const resumed = await runBenchmarkPlan(plan, { ...options, concurrency: 4,
    recoverCell: cell => {
      assert.equal(cell.runId, plan.cells[0].runId)
      return { status: 'failed', failure: { code: 'DEVICE_PRODUCT_FAILED' } }
    },
    executeCell: cell => {
      assert.equal(executed.includes(cell.runId), false, 'the original three executions cannot replay')
      executed.push(cell.runId)
      return { status: 'completed' }
    },
  })
  assert.equal(resumed.denominator, 700)
  assert.equal(new Set(executed).size, 700)
})

test('configuration selection keeps all 700 cells and preserves completed and unresolved unselected rows', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-selected-profiles-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'profiles' } }
  const execute = expected => cell => {
    assert.equal(cell.configurationId, expected)
    return { status: 'completed' }
  }
  const first = await runBenchmarkPlan(plan, { ...options, concurrency: 4,
    selectedConfigurationIds: ['main-A'], executeCell: execute('main-A'),
  })
  assert.equal(first.denominator, 700)
  assert.equal(first.records.filter(record => record.status === 'completed').length, 100)
  assert.equal(first.records.filter(record => record.status === 'planned').length, 600)
  const database = new DatabaseSync(options.ledgerPath)
  let identity
  let original
  try {
    identity = database.prepare('SELECT identity FROM benchmark_identity').get().identity
    original = database.prepare('SELECT * FROM benchmark_cell ORDER BY ordinal').all()
    assert.equal(original.slice(100).every(row => row.token === null && row.record === null), true)
    database.prepare('UPDATE benchmark_cell SET token = ? WHERE ordinal = 100').run('crashed-unselected-claim')
  } finally { database.close() }
  const second = await runBenchmarkPlan(plan, { ...options, concurrency: 6,
    selectedConfigurationIds: ['main-C'], executeCell: execute('main-C'),
    recoverCell: () => assert.fail('an unselected claim must not be recovered'),
  })
  assert.equal(second.denominator, 700)
  assert.deepEqual(second.records.slice(0, 100), first.records.slice(0, 100))
  assert.equal(second.records.filter(record => record.status === 'completed').length, 200)
  const reopened = new DatabaseSync(options.ledgerPath, { readOnly: true })
  try {
    assert.equal(reopened.prepare('SELECT identity FROM benchmark_identity').get().identity, identity)
    const rows = reopened.prepare('SELECT * FROM benchmark_cell ORDER BY ordinal').all()
    assert.deepEqual(rows.slice(0, 100), original.slice(0, 100))
    assert.equal(rows[100].token, 'crashed-unselected-claim')
    assert.equal(rows[100].record, null)
    assert.deepEqual(rows.slice(101, 200), original.slice(101, 200))
    assert.deepEqual(rows.slice(300), original.slice(300))
  } finally { reopened.close() }
  let newExecutions = 0
  const expanded = await runBenchmarkPlan(plan, { ...options, concurrency: 5,
    selectedConfigurationIds: ['main-A', 'main-B', 'main-C'],
    recoverCell: cell => {
      assert.equal(cell.runId, plan.cells[100].runId)
      return { status: 'completed' }
    },
    executeCell: cell => {
      assert.equal(cell.configurationId, 'main-B')
      newExecutions += 1
      return { status: 'completed' }
    },
  })
  assert.equal(newExecutions, 99)
  assert.equal(expanded.denominator, 700)
  assert.equal(expanded.records.filter(record => record.status === 'completed').length, 300)
})

test('invalid concurrency and configuration selection are rejected before creating the ledger', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-dispatch-policy-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'invalid-policy' } }
  for (const concurrency of [0, -1, 1.5, '3', null]) {
    await assert.rejects(runBenchmarkPlan(plan, { ...options, concurrency, executeCell: () => assert.fail() }),
      { code: 'BENCHMARK_CONCURRENCY_INVALID' })
  }
  for (const selectedConfigurationIds of [null, 'main-A', [], ['main-A', 'main-A'], ['unknown'], [undefined]]) {
    await assert.rejects(runBenchmarkPlan(plan, { ...options, selectedConfigurationIds, executeCell: () => assert.fail() }),
      { code: 'BENCHMARK_CONFIGURATION_SELECTION_INVALID' })
  }
  await assert.rejects(access(options.ledgerPath), { code: 'ENOENT' })
  assert.doesNotThrow(() => validateBenchmarkDispatchPolicy(plan, { concurrency: 12, selectedConfigurationIds: ['main-A', 'main-C'] }))
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
  `, ledgerPath, new URL('../scripts/benchmark/run-real-task-benchmark.mjs', import.meta.url).href, directory], { encoding: 'utf8' })
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
  `, new URL('../scripts/benchmark/run-real-task-benchmark.mjs', import.meta.url).href,
  new URL('../scripts/acceptance/run-device-task-vertical.mjs', import.meta.url).href,
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

test('Core repeat stop survives restart while unclaimed tasks continue exactly once', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-benchmark-stop-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'), experimentBinding: { experimentId: 'stop-test' } }
  await assert.rejects(runBenchmarkPlan(plan, {
    ...options,
    executeCell: () => { throw Object.assign(new Error('Core stopped'), { code: 'STUCK_TOOL_REPEAT_LIMIT' }) },
    onRecord: () => { throw new Error('export interrupted') },
  }), /export interrupted/u)
  const resumed = []
  const ledger = await runBenchmarkPlan(plan, {
    ...options, executeCell: cell => { resumed.push(cell.runId); return { status: 'completed' } },
  })
  assert.equal(ledger.denominator, 700)
  assert.equal(ledger.records[0].status, 'failed')
  assert.equal(ledger.records[0].failure.code, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.deepEqual(resumed, plan.cells.slice(1).map(cell => cell.runId))
  assert.equal(ledger.records.slice(1).every(record => record.status === 'completed'), true)
  assert.equal(ledger.records.every(record => record.score === null && record.verdict === null), true)
  await runBenchmarkPlan(plan, { ...options, executeCell: () => assert.fail('completed or stopped tasks must not replay') })
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

test('Device Core stop ledger terminates its delivery driver while other benchmark tasks continue', async t => {
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
      if (calls > 1) return { status: 'completed' }
      await driveDelivery({ query: () => assert.fail('must stop before querying or launching more work') },
        null, undefined, Date.now, { assertRunning: () => assertDeviceBenchmarkRunning(directory, sessions) })
    },
  }
  const ledger = await runBenchmarkPlan(plan, options)
  assert.equal(calls, 700)
  assert.equal(ledger.records[0].failure.code, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(ledger.records.slice(1).every(record => record.status === 'completed'), true)
  assert.equal(ledger.denominator, 700)
  await runBenchmarkPlan(plan, { ...options, executeCell: () => assert.fail('restart cannot execute') })
  assert.equal(database.prepare('SELECT stopped FROM tool_repeat_run').get().stopped, 1)
})

test('a reused physical Worker stop is scoped to its ProductSession', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-session-stop-scope-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const workerSessionId = 'wsn_01J00000000000000000000001'
  const firstId = 'psn_01J00000000000000000000003'
  const secondId = 'psn_01J00000000000000000000004'
  const runtime = resolve(directory, 'worker-sessions', workerSessionId, 'data', 'codex-runtime')
  await mkdir(runtime, { recursive: true })
  const database = new DatabaseSync(resolve(runtime, 'worker-codex.sqlite3'))
  t.after(() => database.close())
  database.exec(`CREATE TABLE tool_repeat_run (run_key TEXT PRIMARY KEY, stopped INTEGER);
    CREATE TABLE codex_run (run_key TEXT PRIMARY KEY, record_json TEXT)`)
  for (const [runKey, productSessionId, stopped] of [['first', firstId, 1], ['second', secondId, 0]]) {
    database.prepare('INSERT INTO tool_repeat_run VALUES (?, ?)').run(runKey, stopped)
    database.prepare('INSERT INTO codex_run VALUES (?, ?)').run(runKey,
      JSON.stringify({ job: { scope: { productSessionId } } }))
  }
  assertDeviceBenchmarkRunning(directory, [workerSessionId], [], secondId)
  assert.throws(() => assertDeviceBenchmarkRunning(directory, [workerSessionId], [], firstId),
    error => error.code === 'STUCK_TOOL_REPEAT_LIMIT' && error.runKey === 'first')
  assert.equal(database.prepare('SELECT stopped FROM tool_repeat_run WHERE run_key = ?').get('first').stopped, 1)
})

test('a persisted Core stop settles a claimed product call before Delivery terminal projection', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-core-stop-product-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const workerSessionId = 'wsn_01J00000000000000000000001'
  const productSessionId = 'psn_01J00000000000000000000001'
  const deliveryId = 'dlv_01J00000000000000000000001'
  const runId = 'main-A:task-0:glm-5.3-flash'
  const callId = `${runId}:model`
  const runtime = resolve(directory, 'device-data/worker-sessions', workerSessionId, 'data/codex-runtime')
  await mkdir(runtime, { recursive: true })
  const database = new DatabaseSync(resolve(runtime, 'worker-codex.sqlite3'))
  database.exec("CREATE TABLE tool_repeat_run (run_key TEXT PRIMARY KEY, stopped INTEGER NOT NULL); INSERT INTO tool_repeat_run VALUES ('owned-core-run', 1)")
  database.close()
  const launch = { callId, directory, productSessionId, deliveryId }
  const report = { errorCode: 'STUCK_TOOL_REPEAT_LIMIT', complete: false, productSessionId, deliveryId,
    delivery: { detail: { status: 'in_progress' },
      workRunAggregate: { runs: [{ id: 'wrn_01J00000000000000000000001', workerSessionId, state: 'running' }] } } }
  await writeFile(resolve(directory, 'device-task-result.json'), JSON.stringify(report))
  await writeFile(resolve(directory, 'task-source-binding.json'), JSON.stringify({ runId, callId, taskId: 'task-0' }))
  await writeFile(resolve(directory, 'execution-receipts.json'), JSON.stringify({ calls: [], jev: [] }))
  const result = stoppedDeviceResult({ runId, callId, taskId: 'task-0', provider: 'glm-5.3-flash' },
    launch, report)
  assert.equal(result.failure.code, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(result.termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(result.productComplete, false)
  assert.equal(result.delivery.detail.status, 'in_progress')
  assert.equal(result.stopProof.workerSessionId, workerSessionId)
  assert.throws(() => stoppedDeviceResult({ runId, callId, taskId: 'other', provider: 'glm-5.3-flash' },
    launch, report))
})

test('a Core stop follows the exact WorkRun into its physical Worker directory', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-core-stop-physical-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const workerSessionId = 'wsn_01J00000000000000000000001'
  const physicalSessionId = 'wsn_01J00000000000000000000002'
  const productSessionId = 'psn_01J00000000000000000000001'
  const deliveryId = 'dlv_01J00000000000000000000001'
  const runId = 'main-A:task-0:glm-5.3-flash'
  const callId = `${runId}:model`
  const run = { id: 'wrn_owned', workerSessionId, workerId: 'wrk_owned',
    workerInstanceId: 'wki_owned', executionJobId: 'job_owned', codexThreadId: 'cdx_owned',
    productSessionId, attempt: 1, state: 'running' }
  const runtime = resolve(directory, 'device-data/worker-sessions', physicalSessionId, 'data/codex-runtime')
  await mkdir(runtime, { recursive: true })
  const device = new DatabaseSync(resolve(directory, 'device-data/device-client.sqlite3'))
  device.exec(`CREATE TABLE worker_process_registry (worker_session_id TEXT, worker_id TEXT, worker_instance_id TEXT)`)
  device.prepare('INSERT INTO worker_process_registry VALUES (?, ?, ?)')
    .run(physicalSessionId, run.workerId, run.workerInstanceId)
  device.close()
  const database = new DatabaseSync(resolve(runtime, 'worker-codex.sqlite3'))
  database.exec(`CREATE TABLE tool_repeat_run (run_key TEXT PRIMARY KEY, stopped INTEGER NOT NULL);
    CREATE TABLE codex_run (run_key TEXT PRIMARY KEY, record_json BLOB);
    INSERT INTO tool_repeat_run VALUES ('owned-core-run', 1)`)
  const record = { canonicalThreadId: run.codexThreadId, job: { jobId: run.executionJobId,
    attempt: run.attempt, scope: { workRunId: run.id, productSessionId } } }
  database.prepare('INSERT INTO codex_run VALUES (?, ?)').run('owned-core-run', JSON.stringify(record))
  t.after(() => database.close())
  const launch = { callId, directory, productSessionId, deliveryId }
  const report = { errorCode: 'STUCK_TOOL_REPEAT_LIMIT', complete: false, productSessionId, deliveryId,
    delivery: { detail: { status: 'in_progress' }, workRunAggregate: { runs: [run] } } }
  await writeFile(resolve(directory, 'device-task-result.json'), JSON.stringify(report))
  await writeFile(resolve(directory, 'task-source-binding.json'), JSON.stringify({ runId, callId, taskId: 'task-0' }))
  await writeFile(resolve(directory, 'execution-receipts.json'), JSON.stringify({ calls: [], jev: [] }))
  const result = stoppedDeviceResult({ runId, callId, taskId: 'task-0', provider: 'glm-5.3-flash' }, launch, report)
  assert.equal(result.failure.code, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(result.stopProof.workerSessionId, physicalSessionId)
  assert.equal(result.stopProof.logicalWorkerSessionId, workerSessionId)
  assert.equal(result.stopProof.runKey, 'owned-core-run')
  for (const [field, changed] of [['executionJobId', 'job_other'], ['codexThreadId', 'cdx_other'],
    ['attempt', 2], ['id', 'wrn_other'], ['productSessionId', 'psn_other'],
    ['workerId', 'wrk_other'], ['workerInstanceId', 'wki_other']]) {
    const foreign = { ...report, delivery: { ...report.delivery,
      workRunAggregate: { runs: [{ ...run, [field]: changed }] } } }
    await writeFile(resolve(directory, 'device-task-result.json'), JSON.stringify(foreign))
    assert.throws(() => stoppedDeviceResult({ runId, callId, taskId: 'task-0', provider: 'glm-5.3-flash' },
      launch, foreign), `a different ${field} must not inherit the stop`)
  }
  await writeFile(resolve(directory, 'device-task-result.json'), JSON.stringify({ ...report,
    modelRoute: { modelId: 'glm-5.3-flash' }, benchmarkConfiguration: benchmarkConfiguration('main-A') }))
  await writeFile(resolve(directory, 'product-source-seal.json'), '{}')
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) }).cells[0]
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'),
    experimentBinding: { experimentId: 'physical-stop-recovery' } }
  let productStarts = 0
  await assert.rejects(runBenchmarkPlan({ cells: [cell] }, { ...options,
    executeCell: (value, runner) => executeBenchmarkCell(value, {
      runModel: async (_request, callRunner) => {
        productStarts += 1
        await callRunner.registerLaunch(launch)
        throw Object.assign(new Error('Core stop export was unavailable'), { code: 'BENCHMARK_EVIDENCE_FAILED' })
      },
    }, runner) }), { code: 'BENCHMARK_EVIDENCE_FAILED' })
  const metadata = new DatabaseSync(options.ledgerPath, { readOnly: true })
  const identity = metadata.prepare('SELECT identity FROM benchmark_identity').get().identity
  metadata.close()
  const retained = openBenchmarkLedger(options.ledgerPath, identity, [cell])
  try {
    let observed
    try { retained.claim(0) } catch (error) { observed = error }
    assert.equal(observed.code, 'LEDGER_RUN_UNRESOLVED')
    const result = await recoverBenchmarkDeviceCell(cell, observed, {})
    const record = { ...cell, ...result, launches: observed.launches }
    const foreign = structuredClone(record)
    foreign.calls[0].result.directory = `${directory}/other-product`
    assert.throws(() => retained.recover(0, observed, foreign), { code: 'LEDGER_RECOVERY_CONFLICT' })
    const unproved = structuredClone(record)
    delete unproved.recovery.originalCalls
    assert.throws(() => retained.recover(0, observed, unproved), { code: 'LEDGER_RECOVERY_CONFLICT' })
    assert.equal(retained.calls(0)[0].failure.code, 'BENCHMARK_EVIDENCE_FAILED')
  } finally { retained.close() }
  const ledger = await runBenchmarkPlan({ cells: [cell] }, { ...options,
    executeCell: () => assert.fail('recovery cannot start another product call'),
    recoverCell: (value, observed) => recoverBenchmarkDeviceCell(value, observed, {}) })
  assert.equal(productStarts, 1)
  assert.equal(ledger.records[0].failure.code, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(ledger.records[0].calls[0].status, 'returned')
  assert.equal(ledger.records[0].recovery.originalCalls[0].failure.code, 'BENCHMARK_EVIDENCE_FAILED')
  assert.deepEqual(await runBenchmarkPlan({ cells: [cell] }, { ...options,
    executeCell: () => assert.fail('settled stop cannot restart') }), ledger)
})

test('recovery retains a Core stop and receipts when the driver dies before writing its stop report', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-core-stop-recovery-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index}`) }).cells[0]
  const launch = { callId: `${cell.runId}:model`, directory,
    productSessionId: 'psn_01J00000000000000000000001',
    deliveryId: 'dlv_01J00000000000000000000001' }
  const workerSessionId = 'wsn_01J00000000000000000000001'
  const runtime = resolve(directory, 'device-data/worker-sessions', workerSessionId, 'data/codex-runtime')
  await mkdir(runtime, { recursive: true })
  const database = new DatabaseSync(resolve(runtime, 'worker-codex.sqlite3'))
  database.exec("CREATE TABLE tool_repeat_run (run_key TEXT PRIMARY KEY, stopped INTEGER NOT NULL); INSERT INTO tool_repeat_run VALUES ('owned-core-run', 1)")
  database.close()
  await writeFile(resolve(directory, 'device-task-result.json'), JSON.stringify({ complete: false,
    productSessionId: launch.productSessionId, deliveryId: launch.deliveryId,
    modelRoute: { modelId: cell.comparison }, benchmarkConfiguration: benchmarkConfiguration(cell.configurationId),
    delivery: { detail: { status: 'in_progress' }, workRunAggregate: { runs: [{ workerSessionId,
      state: 'running' }] } } }))
  await writeFile(resolve(directory, 'task-source-binding.json'), JSON.stringify({ runId: cell.runId,
    callId: launch.callId, taskId: cell.taskId }))
  await writeFile(resolve(directory, 'product-source-seal.json'), '{}')
  const options = { ledgerPath: resolve(directory, 'ledger.sqlite3'),
    experimentBinding: { experimentId: 'retained-stop' } }
  await assert.rejects(runBenchmarkPlan({ cells: [cell] }, { ...options,
    executeCell: (_cell, runner) => {
      runner.registerLaunch(launch)
      throw Object.assign(new Error('driver died'), { code: 'BENCHMARK_EVIDENCE_FAILED' })
    } }), { code: 'BENCHMARK_EVIDENCE_FAILED' })
  const restored = await runBenchmarkPlan({ cells: [cell] }, { ...options,
    executeCell: () => assert.fail('recovery must not restart the product'),
    recoverCell: (value, observed) => recoverBenchmarkDeviceCell(value, observed, {}) })
  assert.equal(restored.records[0].status, 'failed')
  assert.equal(restored.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
  assert.equal(restored.records[0].calls[0].result.stopProof.workerSessionId, workerSessionId)
  assert.equal(restored.records[0].calls[0].result.recovery.kind, 'retained-core-stop')
  assert.deepEqual(await runBenchmarkPlan({ cells: [cell] }, { ...options,
    executeCell: () => assert.fail('completed recovery cannot restart') }), restored)
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

test('externally scored failures remain in the denominator and Fusion regret is gated', () => {
  const plan = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `task-${index + 1}`) })
  const quality = { minorityRetained: 1, minorityTotal: 1, captureRetained: 1, captureTotal: 1,
    constraintsRetained: 1, constraintsTotal: 1, confirmedRetained: 1, confirmedTotal: 1,
    bindingErrors: 0, stateRegressions: 0, hallucinated: 0 }
  const records = plan.cells.map(cell => ({ ...cell, status: 'completed', score: 0.8,
    verdict: 'pass', quality, launches: [], calls: [], wallMs: 0 }))
  const failed = records.map((record, index) => index === 0
    ? { ...record, status: 'failed', score: 0, verdict: null } : record)
  const report = aggregateBenchmarkReport(plan, { records: failed, experimentId: 'graded-failure' })
  assert.equal(report.grading.status, 'complete')
  assert.equal(report.failureAccounting.unsuccessful, 1)
  assert.equal(report.quality.meanScore, 0.798857)
  assert.equal(Object.values(report.gates).every(gate => gate.status === 'pass'), true)

  const underperforming = failed.map(record => record.runId === 'main-D:task-1:fusion-4'
    ? { ...record, status: 'failed', score: 0, verdict: null } : record)
  const regretReport = aggregateBenchmarkReport(plan, {
    records: underperforming, experimentId: 'graded-fusion-loss',
  })
  assert.equal(regretReport.strategyReferences.find(reference =>
    reference.configurationId === 'main-D').regrets['fusion-4'].againstBatchBest, 0.04)
  assert.equal(regretReport.gates.regret.status, 'fail')
  assert.equal(regretReport.gates.minority.status, 'pass')
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
        if (executions !== 1) return { candidateCommit: 'fixture' }
        const db = new DatabaseSync(ledgerPath)
        try { db.exec("CREATE TRIGGER reject_call BEFORE INSERT ON benchmark_call BEGIN SELECT RAISE(ABORT, 'receipt disk failure'); END;") }
        finally { db.close() }
        return { candidateCommit: 'fixture' }
      },
      aggregate: () => assert.fail('aggregation must not start'),
    }, context),
  }), /receipt disk failure/u)
  assert.equal(executions, 4)
  await assert.rejects(runBenchmarkPlan(plan, { ...options,
    executeCell: () => assert.fail('unresolved member must not rerun'),
  }), { code: 'LEDGER_RUN_UNRESOLVED' })
})

test('returned product stop halts its standalone or Fusion task while the next task runs', async t => {
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
      executeCell: (cell, context) => executeBenchmarkCell(cell, cell.runId === first.runId
        ? adapter : { runModel: () => { calls += 1; return { answer: 'next task' } } }, context),
    })
    assert.equal(calls, stage === 'aggregation' ? 6 : stage === 'member' ? 5 : 2)
    assert.equal(ledger.records[0].status, 'failed')
    assert.equal(ledger.records[0].termination.reason, 'STUCK_TOOL_REPEAT_LIMIT')
    assert.deepEqual(ledger.records[0].claims, stop.claims)
    assert.deepEqual(ledger.records[0].calls.at(-1).result, stop)
    assert.equal(ledger.records[1].status, 'completed')
    assert.equal(ledger.records[1].termination, null)
    assert.equal(ledger.records[1].calls.length, 1)
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
  const interruptions = []
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
    if (body.query === 'delivery.get' && interruptions.length > 0) {
      const interruption = interruptions.shift()
      if (interruption === 'reset') {
        request.socket.destroy()
        return
      }
      if (interruption === 'truncated') {
        response.writeHead(200, { 'Content-Length': 1000 })
        response.write('{"schemaVersion":')
        setImmediate(() => response.destroy())
        return
      }
      response.setHeader('Retry-After', '0')
      response.writeHead(interruption === 'unavailable' ? 503 : 403)
      response.end(JSON.stringify({ error: { code: interruption === 'unavailable'
        ? 'TRUSTED_FACTS_UNAVAILABLE' : 'PERMISSION_DENIED' } }))
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
    interruptions.push('reset', 'unavailable')
    const retryStart = requests.length
    const recovered = await inspectUnresolvedDeviceTasks(path)
    assert.equal(recovered[0].observation, 'queried', 'transient queries must retain the native task')
    const attempts = requests.slice(retryStart).filter(row => row.body?.query === 'delivery.get')
    assert.equal(attempts.length, 3)
    assert.deepEqual(attempts[1], attempts[0], 'retry must preserve the exact query request')
    assert.deepEqual(attempts[2], attempts[0])
    interruptions.push('truncated')
    const truncatedStart = requests.length
    assert.equal((await inspectUnresolvedDeviceTasks(path))[0].observation, 'queried')
    const truncatedAttempts = requests.slice(truncatedStart).filter(row => row.body?.query === 'delivery.get')
    assert.equal(truncatedAttempts.length, 2)
    assert.deepEqual(truncatedAttempts[1], truncatedAttempts[0])
    interruptions.push('forbidden')
    const forbiddenStart = requests.length
    assert.equal((await inspectUnresolvedDeviceTasks(path))[0].observation, 'unavailable')
    assert.equal(requests.slice(forbiddenStart).filter(row => row.body?.query === 'delivery.get').length, 1,
      'permission errors must not retry')
    interruptions.push(...Array(4).fill('unavailable'))
    const exhaustedStart = requests.length
    assert.equal((await inspectUnresolvedDeviceTasks(path))[0].observation, 'unavailable')
    assert.equal(requests.slice(exhaustedStart).filter(row => row.body?.query === 'delivery.get').length, 4,
      'query retries must stop after four attempts')
    assert.equal(interruptions.length, 0)
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
  const providerDirectory = resolve(directory, 'device-data', 'providers')
  await mkdir(providerDirectory, { recursive: true })
  const providerDatabase = new DatabaseSync(resolve(providerDirectory, 'providers.sqlite3'))
  providerDatabase.exec('CREATE TABLE exchanges(exchange_id TEXT,request_open TEXT,chunks TEXT)')
  const encode = object => {
    const bytes = Buffer.from(JSON.stringify(object))
    return { dataBase64: bytes.toString('base64'), payloadDigest: `sha256:${sha(bytes)}` }
  }
  providerDatabase.prepare('INSERT INTO exchanges VALUES (?,?,?)').run('fixture-exchange',
    JSON.stringify({ modelExchangeId: 'fixture-exchange', lease: { jobId: 'fixture-job' },
      workerSessionId: 'fixture-worker', request: encode({ provider: 'fixture',
        request: { model: cell.comparison, reasoning: { effort: 'max' } } }) }),
    JSON.stringify([{ modelExchangeId: 'fixture-exchange',
      payload: encode({ type: 'server_model', model: cell.comparison }) },
    { modelExchangeId: 'fixture-exchange', payload: encode({ type: 'completed',
      tokenUsage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 } }) }]))
  providerDatabase.close()
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
  let cancelled = false
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
        status: cancelled ? 'cancelled' : done ? 'done' : 'in_progress',
        verdict: cancelled ? null : { status: 'pass', criteria: [{ verdict: 'pass' }] },
        attention: [], evidence: [{ id: 'canonical-test-evidence' }] },
      'workrun.get': { readCursor: cursor, items: [{ state: cancelled ? 'cancelled' : 'done' }],
        runs: [{ id: 'original-run', state: cancelled ? 'cancelled' : 'completed',
          workItemId: 'original-item', executionJobId: 'original-job' }] },
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
    let productStarts = 0
    await assert.rejects(runBenchmarkPlan(plan, { ...options,
      executeCell: (value, runner) => executeBenchmarkCell(value, {
        runModel: async (_request, callRunner) => {
          productStarts += 1
          await callRunner.registerLaunch(launch)
          throw Object.assign(new Error('interrupted export'), { code: 'BENCHMARK_EVIDENCE_FAILED' })
        },
      }, runner) }), { code: 'BENCHMARK_EVIDENCE_FAILED' })
    const before = new DatabaseSync(options.ledgerPath, { readOnly: true })
    const originalCalls = before.prepare('SELECT record FROM benchmark_call ORDER BY rowid').all()
      .map(row => JSON.parse(row.record))
    assert.equal(originalCalls.length, 1)
    assert.equal(originalCalls[0].failure.code, 'BENCHMARK_EVIDENCE_FAILED')
    assert.equal(before.prepare('SELECT record FROM benchmark_cell').get().record, null)
    before.close()
    const recovery = { ...options, executeCell: () => assert.fail('must not rerun task'), recoverCell: recoverBenchmarkDeviceCell }
    await assert.rejects(runBenchmarkPlan(plan, recovery))
    done = true
    const restored = await runBenchmarkPlan(plan, recovery)
    assert.equal(productStarts, 1, 'recovery cannot start another product invocation')
    assert.equal(restored.records[0].calls[0].status, 'returned')
    assert.deepEqual(restored.records[0].recovery.originalCalls, originalCalls)
    assert.equal(restored.records[0].model.recovery.originalReportSha256, sha(await readFile(reportPath)))
    assert.equal(restored.records[0].model.recovery.observation.delivery.status, 'done')
    const recoveredManifest = JSON.parse(await readFile(restored.records[0].model.submissionManifest.path))
    assert.equal(recoveredManifest.productComplete, true)
    assert.equal(recoveredManifest.externalScore, null)
    assert.equal(JSON.parse(await readFile(reportPath)).complete, false, 'original observation remains unchanged')
    assert.equal(JSON.parse(await readFile(resolve(directory, 'submission-evidence', commit, 'manifest.json'))).productComplete, false)
    assert.deepEqual(await runBenchmarkPlan(plan, recovery), restored)
    await t.test('current execution failure receipt recovers from its exact terminal lease', async () => {
      await mkdir(resolve(directory, 'server-data'), { recursive: true })
      const scheduler = new DatabaseSync(resolve(directory, 'server-data/control-plane.sqlite3'))
      try {
        scheduler.exec(`CREATE TABLE scheduler_execution_jobs (job_id TEXT, delivery_id TEXT,
          work_run_id TEXT, state TEXT, attempt INTEGER, revision INTEGER, updated_at TEXT, payload_digest TEXT);
          ${dispatchLeaseSchema}
          INSERT INTO execution_leases VALUES ('original-job','lease','payload','worker','instance',1,'1');
          INSERT INTO execution_lease_terminals VALUES ('lease','original-job','worker','instance',1,'1','failed');`)
        scheduler.prepare('INSERT INTO scheduler_execution_jobs VALUES (?,?,?,?,?,?,?,?)')
          .run('original-job', launch.deliveryId, 'original-run', 'failed', 1, 5, 'now', 'payload')
        const currentReport = { ...report, errorCode: 'DEVICE_EXECUTION_FAILED', workRunId: 'original-run',
          dispatchFailure: failedDeviceDispatch(directory, 'original-run', launch.deliveryId) }
        const currentBytes = JSON.stringify(currentReport)
        await writeFile(reportPath, currentBytes)
        const originalProviderBytes = await readFile(resolve(providerDirectory, 'providers.sqlite3'))
        const request = { ...cell, callId: launch.callId, provider: cell.comparison }
        const result = await resolveRegisteredDeviceTask(request, launch)
        assert.equal(result.failure.code, 'DEVICE_EXECUTION_FAILED')
        assert.equal(result.productComplete, false)
        assert.equal(result.externalScore, null)
        assert.deepEqual(result.dispatchFailure, currentReport.dispatchFailure)
        assert.equal(await readFile(reportPath, 'utf8'), currentBytes)
        assert.deepEqual(await readFile(resolve(providerDirectory, 'providers.sqlite3')), originalProviderBytes)
        assert.equal(productStarts, 1)
        const misclassifiedReport = { ...currentReport, errorCode: 'DEVICE_DISPATCH_FAILED' }
        await writeFile(reportPath, JSON.stringify(misclassifiedReport))
        await assert.rejects(resolveRegisteredDeviceTask(request, launch), { code: 'ERR_ASSERTION' })
        const incompleteReceipt = structuredClone(currentReport)
        delete incompleteReceipt.dispatchFailure.failureOrigin
        await writeFile(reportPath, JSON.stringify(incompleteReceipt))
        await assert.rejects(resolveRegisteredDeviceTask(request, launch), { code: 'ERR_ASSERTION' })
        scheduler.exec("UPDATE execution_lease_terminals SET outcome='cancelled'")
        await writeFile(reportPath, currentBytes)
        await assert.rejects(resolveRegisteredDeviceTask(request, launch), { code: 'ERR_ASSERTION' })
        await writeFile(reportPath, JSON.stringify(report))
        cancelled = true
        const cancellation = await resolveRegisteredDeviceTask(request, launch)
        assert.equal(cancellation.failure.code, 'DEVICE_PRODUCT_CANCELLED')
        assert.equal(await readFile(reportPath, 'utf8'), JSON.stringify(report))
        assert.deepEqual(await readFile(resolve(providerDirectory, 'providers.sqlite3')), originalProviderBytes)
        assert.equal(productStarts, 1)
      } finally { scheduler.close() }
    })
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

test('execution receipt export retains typed model failures without copying upstream text', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'execution-failure-receipts-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const device = resolve(directory, 'device-data', 'providers')
  await mkdir(device, { recursive: true })
  const database = new DatabaseSync(resolve(device, 'providers.sqlite3'))
  t.after(() => database.close())
  database.exec('CREATE TABLE exchanges(exchange_id TEXT,request_open TEXT,chunks TEXT)')
  const requestBytes = Buffer.from(JSON.stringify({ provider: 'deepseek', request: { model: 'deepseek-flash' } }))
  for (const [exchange, code, message, expected] of [
    ['typed', 'DEVICE_PROVIDER_SSE_EVENT_INVALID', 'private upstream text', 'DEVICE_PROVIDER_SSE_EVENT_INVALID'],
    ['untyped', 'MODEL_STREAM_FAILED', 'DEVICE_PROVIDER_SSE_EVENT_INVALID: private upstream text', 'MODEL_STREAM_FAILED'],
    ['unknown', 'MODEL_STREAM_FAILED', 'private unrecognized diagnostic', 'MODEL_STREAM_FAILED'],
  ]) {
    const opened = { modelExchangeId: exchange, lease: { jobId: `job-${exchange}` }, workerSessionId: 'session',
      request: { dataBase64: requestBytes.toString('base64'),
        payloadDigest: `sha256:${createHash('sha256').update(requestBytes).digest('hex')}` } }
    const chunks = [{ modelExchangeId: exchange, isFinal: true,
      error: { code, message, retryable: false } }]
    database.prepare('INSERT INTO exchanges VALUES (?,?,?)')
      .run(exchange, JSON.stringify(opened), JSON.stringify(chunks))
    const evidenceDirectory = resolve(directory, exchange)
    await mkdir(evidenceDirectory)
    const { evidence } = exportDeviceExecutionReceipts(directory, evidenceDirectory)
    const call = evidence.calls.find(value => value.exchangeId === exchange)
    assert.deepEqual(call.failure, { code: expected, retryable: false })
    assert.equal(call.terminalType, 'error')
    assert.equal(call.usage, null)
    assert.equal((await readFile(resolve(evidenceDirectory, 'execution-receipts.json'), 'utf8')).includes('private'), false)
  }
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
  const runtime = resolve(directory, 'device-data', 'worker-sessions', 'fixture-worker',
    'data', 'codex-runtime')
  await mkdir(runtime, { recursive: true })
  const performanceDatabase = new DatabaseSync(resolve(runtime, 'worker-codex.sqlite3'))
  t.after(() => performanceDatabase.close())
  performanceDatabase.exec(`CREATE TABLE performance_projection(run_key TEXT, record_json BLOB);
    CREATE TABLE performance_operation(run_key TEXT, operation_kind TEXT, operation_id TEXT,
      completed INTEGER, duration_millis INTEGER, actual_cost_microunits INTEGER);`)
  performanceDatabase.prepare('INSERT INTO performance_projection VALUES (?,?)').run('sha256:fixture',
    Buffer.from(JSON.stringify({ report: { primaryModelWaitMs: 42, totalRuntimeMs: 60 } })))
  performanceDatabase.prepare('INSERT INTO performance_operation VALUES (?,?,?,?,?,?)')
    .run('sha256:fixture', 'primary_model', 'model-1', 1, 42, null)
  performanceDatabase.prepare('INSERT INTO performance_operation VALUES (?,?,?,?,?,?)')
    .run('sha256:fixture', 'tool', 'tool-1', 1, 10, null)
  const { evidence } = exportDeviceExecutionReceipts(directory)
  assert.equal(evidence.calls.length, 2)
  assert.equal(evidence.completeUsage, false)
  assert.equal(evidence.totalTokens, null)
  assert.equal(evidence.calls[0].usage.totalTokens, 7)
  assert.equal(evidence.calls[1].usage, null)
  assert.equal(evidence.performance[0].totalRuntimeMs, 60)
  assert.equal(evidence.performance[0].modelWaitMs, 42)
  assert.equal(evidence.performance[0].toolMs, 10)
  assert.equal(evidence.performance[0].actualCostMicros, null)
  const saved = await readFile(resolve(directory, 'execution-receipts.json'), 'utf8')
  assert.equal(saved.includes('private-'), false)
  const opened = JSON.parse(database.prepare('SELECT request_open FROM exchanges LIMIT 1').get().request_open)
  opened.request.payloadDigest = `sha256:${'0'.repeat(64)}`
  database.prepare('UPDATE exchanges SET request_open=? WHERE exchange_id=?').run(JSON.stringify(opened), 'known')
  assert.throws(() => exportDeviceExecutionReceipts(directory), /receipt digest mismatch/u)
  assert.equal(await readFile(resolve(directory, 'execution-receipts.json'), 'utf8'), saved)
})

test('shared Device receipts select durable Delivery jobs including role Sessions', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'shared-device-receipts-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  await mkdir(resolve(directory, 'device-data', 'providers'), { recursive: true })
  await mkdir(resolve(directory, 'server-data'))
  const server = new DatabaseSync(resolve(directory, 'server-data', 'control-plane.sqlite3'))
  const providers = new DatabaseSync(resolve(directory, 'device-data', 'providers', 'providers.sqlite3'))
  t.after(() => { server.close(); providers.close() })
  server.exec('CREATE TABLE scheduler_execution_jobs(job_id TEXT,delivery_id TEXT,product_session_id TEXT)')
  providers.exec('CREATE TABLE exchanges(exchange_id TEXT,request_open TEXT,chunks TEXT)')
  const deliveryId = 'dlv_01J00000000000000000000001'
  const productSessionId = 'psn_01J00000000000000000000001'
  for (const [job, delivery, session] of [
    ['root', deliveryId, productSessionId],
    ['role', deliveryId, 'psn_01J00000000000000000000002'],
    ['other', 'dlv_01J00000000000000000000002', 'psn_01J00000000000000000000003'],
  ]) server.prepare('INSERT INTO scheduler_execution_jobs VALUES (?,?,?)').run(job, delivery, session)
  const encode = object => {
    const bytes = Buffer.from(JSON.stringify(object))
    return { dataBase64: bytes.toString('base64'), payloadDigest: `sha256:${createHash('sha256').update(bytes).digest('hex')}` }
  }
  for (const job of ['root', 'role', 'other']) {
    const opened = { modelExchangeId: job, lease: { jobId: job }, workerSessionId: 'shared-worker',
      request: encode({ provider: 'glm', request: { model: 'glm-5.3-flash' } }) }
    if (job === 'other') opened.request.payloadDigest = 'sha256:invalid'
    providers.prepare('INSERT INTO exchanges VALUES (?,?,?)').run(job, JSON.stringify(opened), JSON.stringify([
      { modelExchangeId: job, isFinal: true, payload: encode({ type: 'completed', tokenUsage: {
        input_tokens: 2, output_tokens: 3, total_tokens: 5,
      } }) },
    ]))
  }
  const { evidence } = exportDeviceExecutionReceipts(directory, directory, { deliveryId, productSessionId })
  assert.deepEqual(evidence.calls.map(call => call.jobId), ['role', 'root'])
  assert.equal(evidence.totalTokens, 10)
  assert.equal(evidence.calls.some(call => call.jobId === 'other'), false)
  await assert.rejects(async () => exportDeviceExecutionReceipts(directory, directory, {
    deliveryId: 'dlv_01J00000000000000000000002', productSessionId: 'psn_01J00000000000000000000003',
  }), /receipt digest mismatch/u)
})


test('shared recovery ignores a neighboring stop and retains its own unprojected Controller stop', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'wwc-shared-recovery-stop-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const source = 'psn_01J00000000000000000000001'
  const role = 'psn_01J00000000000000000000002'
  const neighbor = 'psn_01J00000000000000000000003'
  const worker = 'wsn_01J00000000000000000000001'
  const launch = { directory, productSessionId: source, deliveryId: 'delivery-a' }
  await mkdir(resolve(directory, 'server-data'), { recursive: true })
  await mkdir(resolve(directory, 'device-data/worker-sessions', worker, 'data/codex-runtime'), { recursive: true })
  await writeFile(resolve(directory, 'runtime-binding.json'), '{}')
  const server = new DatabaseSync(resolve(directory, 'server-data/control-plane.sqlite3'))
  t.after(() => server.close())
  server.exec('CREATE TABLE scheduler_execution_jobs (delivery_id TEXT, work_run_id TEXT, dispatch_payload BLOB)')
  server.prepare('INSERT INTO scheduler_execution_jobs VALUES (?,?,?)').run('delivery-a', 'run-a',
    JSON.stringify({ scope: { kind: 'work-run', workRunId: 'run-a', productSessionId: role } }))
  server.prepare('INSERT INTO scheduler_execution_jobs VALUES (?,?,?)').run('delivery-b', 'run-b', 'invalid foreign proof')
  const core = new DatabaseSync(resolve(directory, 'device-data/worker-sessions', worker,
    'data/codex-runtime/worker-codex.sqlite3'))
  t.after(() => core.close())
  core.exec('CREATE TABLE tool_repeat_run (run_key TEXT, stopped INTEGER); CREATE TABLE codex_run (run_key TEXT, record_json BLOB)')
  for (const [key, session, stopped] of [['own', role, 0], ['foreign', neighbor, 1]]) {
    core.prepare('INSERT INTO tool_repeat_run VALUES (?,?)').run(key, stopped)
    core.prepare('INSERT INTO codex_run VALUES (?,?)').run(key, JSON.stringify({ job: { scope: { productSessionId: session } } }))
  }
  assert.deepEqual(assertRetainedDeviceTaskRunning(launch), [worker])
  core.prepare('UPDATE tool_repeat_run SET stopped = 1 WHERE run_key = ?').run('own')
  assert.throws(() => assertRetainedDeviceTaskRunning(launch), error => error.code === 'STUCK_TOOL_REPEAT_LIMIT' && error.runKey === 'own')
})
