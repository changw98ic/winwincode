// SPDX-License-Identifier: Apache-2.0
// Offline fixture. The regression_runner owns execution.
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { execFileSync } from 'node:child_process'
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises'
import { createServer } from 'node:https'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { pathToFileURL } from 'node:url'

const sourceRoot = resolve(process.argv[2])
const out = resolve(process.argv[3])
const source = name => pathToFileURL(resolve(sourceRoot, 'scripts', name)).href
const { runBenchmarkSchedule, runBenchmarkPlan, buildBenchmarkPlan } = await import(source('run-real-task-benchmark.mjs'))
const { persistedBenchmarkDeviceFailure, resolveRegisteredDeviceTask, terminalBenchmarkDeviceFailure } = await import(source('benchmark-device-adapter.mjs'))
const { driveDelivery } = await import(source('run-api-production-vertical.mjs'))
const { readDevicePendingApprovalIds } = await import(source('export-device-candidate.mjs'))
const { resolveDeviceTaskApprovals } = await import(source('device-production-fixture.mjs'))
const digest = bytes => createHash('sha256').update(bytes).digest('hex')
const root = await mkdtemp(resolve(tmpdir(), 'wwc-scheduler-mechanism-'))
const receipt = { schemaVersion: 1, sourceRoot, fixture: import.meta.url, cases: [], providerRequests: 0,
  productionMutations: 0, boundary: 'actual JavaScript repository methods; not Rust scheduler execution' }
const red = (id, title, observed, predicate) => {
  let error = null
  try { predicate() } catch (caught) { error = { name: caught.name, code: caught.code, message: caught.message } }
  receipt.cases.push({ id, title, observed, desiredBehaviorPassed: error === null, regression: error ? 'red' : 'green', assertion: error })
}
try {
  // M18: real schedule and durable local ledger. No task 3 admission is permitted.
  let release
  let settled = false
  let admitted = 0
  const blocked = new Promise(resolvePromise => { release = resolvePromise })
  const events = []
  const failure = Object.assign(new Error('fixture unresolved original execution'), {
    code: 'DEVICE_EXECUTION_LEASE_EXPIRED', unresolvedDeviceExecution: true })
  const cells = [0, 1, 2].map(index => ({ runId: `offline-schedule-${index}` }))
  const schedule = runBenchmarkSchedule(cells, { concurrency: 2,
    onState: state => events.push(state),
    executeCell: (cell, index) => runBenchmarkPlan({ cells: [cell] }, {
      ledgerPath: resolve(root, `m18-${index}.sqlite3`), experimentBinding: { offline: true, index },
      executeCell: async (_cell, runner) => {
        admitted += 1
        if (index === 0) {
          runner.recordCall({ callId: `${cell.runId}:model`, status: 'failed',
            failure: { code: failure.code, executionUnresolved: true } })
          throw failure
        }
        await blocked
        return { status: 'completed' }
      },
    }),
  }).then(() => { settled = true }, error => { settled = true; return error })
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  const before = { admitted, settled, events: structuredClone(events) }
  const ledger = new DatabaseSync(resolve(root, 'm18-0.sqlite3'), { readOnly: true })
  before.claimedUnresolved = ledger.prepare('SELECT COUNT(*) AS n FROM benchmark_cell WHERE token IS NOT NULL AND record IS NULL').get().n
  before.retainedCalls = ledger.prepare('SELECT COUNT(*) AS n FROM benchmark_call').get().n
  ledger.close()
  release()
  const propagated = await schedule
  assert.equal(admitted, 2)
  assert.equal(propagated, failure)
  red('M18', 'Immediate admissionStopped/draining/firstFailure projection while a sibling drains', before, () => {
    assert.ok(before.events.some(event => event.admissionStopped === true && event.firstFailure?.code === failure.code),
      'runBenchmarkSchedule emitted no immediate stopped-admission state before the blocked sibling finished')
  })

  // M19: old report is active; the original launch has since become terminal.
  // Exercise the actual adapter classifier and the actual read-only recovery API.
  // The production catch calls these in this order. The first call throws,
  // preventing the second. No product invocation or second launch exists here.
  const directory = resolve(root, 'm19')
  await mkdir(resolve(directory, 'device-data/providers'), { recursive: true })
  await mkdir(resolve(directory, 'device-data/worker-sessions'), { recursive: true })
  const cell = buildBenchmarkPlan({ taskIds: Array.from({ length: 20 }, (_, index) => `offline-task-${index}`) }).cells[0]
  const request = { ...cell, callId: `${cell.runId}:model`, provider: cell.comparison }
  const launch = { callId: request.callId, directory,
    deliveryId: 'dlv_01J00000000000000000000001', productSessionId: 'psn_01J00000000000000000000001' }
  const oldCursor = { deliveryId: launch.deliveryId, deliveryRevision: 6, token: 'old-active-cursor' }
  const currentCursor = { deliveryId: launch.deliveryId, deliveryRevision: 7, token: 'current-terminal-cursor' }
  const identities = { id: 'wrn_01J00000000000000000000001', executionJobId: 'job_01J00000000000000000000001',
    workerSessionId: 'wsn_01J00000000000000000000001', attempt: 1 }
  const original = { taskInputDigest: digest('offline-input'), productSessionId: launch.productSessionId,
    deliveryId: launch.deliveryId, modelRoute: { modelId: cell.comparison },
    benchmarkConfiguration: Object.fromEntries(['configurationId', 'track', 'fusion', 'jev', 'jevContext', 'jevJudge'].map(key => [key, cell[key]])),
    complete: false, errorCode: 'DEVICE_EXECUTION_LEASE_EXPIRED', delivery: {
      detail: { deliveryId: launch.deliveryId, status: 'in_progress', readCursor: oldCursor, attention: [], currentCandidate: null },
      workRunAggregate: { readCursor: oldCursor, items: [{ state: 'running' }], runs: [{ ...identities, state: 'running' }] } } }
  const current = { registeredLaunch: launch, productSession: { id: launch.productSessionId },
    delivery: { ...original.delivery.detail, status: 'failed', readCursor: currentCursor },
    workRunAggregate: { readCursor: currentCursor, items: [{ state: 'failed' }], runs: [{ ...identities, state: 'failed' }] } }
  const originalBytes = `${JSON.stringify(original)}\n`
  await writeFile(resolve(directory, 'device-task-result.json'), originalBytes)
  await writeFile(resolve(directory, 'task-source-binding.json'), JSON.stringify({ runId: request.runId,
    callId: request.callId, taskId: request.taskId, taskInputSha256: original.taskInputDigest }))
  await writeFile(resolve(directory, 'product-source-seal.json'), '{}')
  const provider = new DatabaseSync(resolve(directory, 'device-data/providers/providers.sqlite3'))
  provider.exec('CREATE TABLE exchanges(exchange_id TEXT,request_open TEXT,chunks TEXT)')
  provider.close()
  execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-days', '1',
    '-subj', '/CN=control.localhost', '-addext', 'subjectAltName=DNS:control.localhost',
    '-keyout', resolve(directory, 'key.pem'), '-out', resolve(directory, 'fixture-cert.pem')], { stdio: 'ignore' })
  const queries = []
  const server = createServer({ key: await readFile(resolve(directory, 'key.pem')),
    cert: await readFile(resolve(directory, 'fixture-cert.pem')) }, async (req, res) => {
    res.setHeader('Content-Type', 'application/json')
    if (req.method === 'GET' && req.url === '/api/v1/auth/session') {
      res.end(JSON.stringify({ schemaVersion: 'winwincode/v1', actor: { kind: 'user' } })); return
    }
    assert.equal(req.url, '/api/v1/queries', 'offline observer must not issue product commands')
    const chunks = []
    for await (const chunk of req) chunks.push(chunk)
    const body = JSON.parse(Buffer.concat(chunks))
    queries.push(body.query)
    res.end(JSON.stringify({ schemaVersion: 'winwincode/v1', requestId: body.requestId, query: body.query,
      result: { 'delivery.get': current.delivery, 'workrun.get': current.workRunAggregate, 'session.get': current.productSession }[body.query] }))
  })
  try {
    await new Promise(resolvePromise => server.listen(0, '127.0.0.1', resolvePromise))
    const port = server.address().port
    await writeFile(resolve(directory, 'server-endpoint.json'), JSON.stringify({ controlUrl: `https://127.0.0.1:${port}`, origin: `https://api.localhost:${port}` }))
    let classifierError = null
    let normalPathResult = null
    try {
      const observation = persistedBenchmarkDeviceFailure(original, launch,
        Object.assign(new Error('lease expiry observed before terminal'), { code: original.errorCode, report: original }))
      normalPathResult = await resolveRegisteredDeviceTask(request, launch, {}, { report: original, reportBytes: Buffer.from(originalBytes), observation })
    } catch (error) { classifierError = { code: error.code, unresolvedDeviceExecution: error.unresolvedDeviceExecution } }
    const normalPathQueries = [...queries]
    const explicitRecovery = await resolveRegisteredDeviceTask(request, launch)
    assert.deepEqual(terminalBenchmarkDeviceFailure(current), { code: 'DEVICE_PRODUCT_FAILED', status: 'failed' })
    assert.equal(explicitRecovery.status, 'failed')
    assert.equal(explicitRecovery.failure.code, 'DEVICE_PRODUCT_FAILED')
    assert.equal(await readFile(resolve(directory, 'device-task-result.json'), 'utf8'), originalBytes)
    red('M19', 'Adapter normal path observes late terminal state of the same original launch', {
      classifierError, normalPathResult, normalPathQueries, explicitRecovery: { status: explicitRecovery.status,
        code: explicitRecovery.failure.code, queries, originalReportPreserved: true }, launchCount: 0,
      integrationBoundary: 'actual adapter classifier and resolver; normal production ordering; no runDeviceTaskVertical invocation' }, () => {
      assert.equal(normalPathResult?.failure?.code, 'DEVICE_PRODUCT_FAILED', 'stale active report aborts before current terminal observation')
    })
  } finally {
    server.closeAllConnections()
    await new Promise(resolvePromise => server.close(resolvePromise))
  }

  // M21: actual active driveDelivery loop + actual no-approval registry reads.
  // Ten terminal Core databases are historical. No model data is required.
  const pollDir = resolve(root, 'm21')
  const history = 10
  for (let index = 0; index < history; index += 1) {
    const dir = resolve(pollDir, 'device-data', `history-${index}`)
    await mkdir(dir, { recursive: true })
    const db = new DatabaseSync(resolve(dir, 'worker-codex.sqlite3'))
    db.exec('CREATE TABLE codex_run(run_key TEXT,record_json BLOB); CREATE TABLE approval_operation(approval_id TEXT,run_key TEXT,state TEXT)')
    db.close()
  }
  const run = { ...identities, state: 'running', codexThreadId: 'cdx_01J00000000000000000000001' }
  const counts = { 'delivery.get': 0, 'workrun.get': 0, 'approval.list': 0, onProjection: 0, onActive: 0, coreScans: 0 }
  const targetPolls = 5
  const clock = () => counts.onActive * 50
  const client = { query: async name => {
    counts[name] += 1
    if (name === 'approval.list') return { page: { hasMore: false }, result: { items: [] } }
    if (name === 'delivery.get') return { result: { deliveryId: launch.deliveryId, readCursor: currentCursor,
      deliveryRevision: 7, status: 'in_progress', attention: [] } }
    return { result: { readCursor: currentCursor, runs: [run] } }
  } }
  const stop = Object.assign(new Error('offline polling budget reached'), { code: 'OFFLINE_POLL_DONE' })
  const started = performance.now()
  await assert.rejects(driveDelivery(client, null, undefined, clock, {
    deliveryId: launch.deliveryId, strictLaunchAnchor: true,
    assertRunning: () => { if (counts.onActive >= targetPolls) throw stop },
    onProjection: () => { counts.onProjection += 1 },
    onActiveWorkRuns: async runs => {
      counts.onActive += 1
      const pending = readDevicePendingApprovalIds(pollDir, runs)
      counts.coreScans += history
      assert.deepEqual(pending, [])
      await resolveDeviceTaskApprovals({ api: client, runs, approvalOwner: 'execution_port', corePendingApprovalIds: pending })
    },
  }), error => error === stop)
  assert.equal(counts['delivery.get'], targetPolls)
  assert.equal(counts['workrun.get'], targetPolls)
  assert.equal(counts['approval.list'], targetPolls)
  receipt.cases.push({ id: 'M21', title: 'Unchanged cursor repeats full projection, approval and Core history reads',
    classification: 'mechanism-measured; no formal-batch latency attribution', historyDatabases: history,
    polls: targetPolls, counts, elapsedMs: performance.now() - started,
    coreScansMethod: 'one actual readDevicePendingApprovalIds invocation per poll × known fixture database count; not a patched counter',
    pollDelayMillis: 50, status: 'confirmed' })
} finally {
  await mkdir(out, { recursive: true })
  await writeFile(resolve(out, 'scheduler-js-receipt.json'), `${JSON.stringify(receipt, null, 2)}\n`)
  process.stdout.write(`${JSON.stringify({ cases: receipt.cases.map(({ id, regression, status }) => ({ id, regression, status })) })}\n`)
  if (process.argv[4] !== '--observe' && receipt.cases.some(value => value.regression === 'red')) process.exitCode = 1
  await rm(root, { recursive: true, force: true })
}
