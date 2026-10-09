// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import test from 'node:test'
import { createHash } from 'node:crypto'
import { DatabaseSync } from 'node:sqlite'
import { openBenchmarkLedger } from '../scripts/lib/benchmark-ledger.mjs'
import { execFileSync, spawn } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { acquireDeviceTaskSupervisor } from '../scripts/lib/device-task-supervisor.mjs'
import { resumeRegisteredDeviceTask } from '../scripts/acceptance/run-device-task-vertical.mjs'
import { setTimeout as delay } from 'node:timers/promises'
import { resolveDeviceTaskApprovals, stopUnusedDeviceTaskAnchor } from '../scripts/lib/device-production-fixture.mjs'

const identity = { productSessionId: 'psn_01J00000000000000000000001', deliveryId: 'dlv_01J00000000000000000000001', callId: 'fixture-call' }
const workRunId = 'wrn_01J00000000000000000000001'
const fixture = t => {
  const directory = mkdtempSync(join(tmpdir(), 'device-task-runtime-unified-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  return directory
}
const journal = directory => JSON.parse(readFileSync(join(directory, 'task-supervision.json'), 'utf8'))

test('supervisor rejects a live duplicate and changed durable identity', t => {
  const directory = fixture(t)
  const first = acquireDeviceTaskSupervisor({ directory, identity })
  try {
    first.checkpoint('dispatched', { workRunId })
    assert.throws(() => acquireDeviceTaskSupervisor({ directory, identity }), { code: 'DEVICE_TASK_SUPERVISOR_OWNED' })
    assert.throws(() => acquireDeviceTaskSupervisor({ directory, identity: { ...identity, callId: 'other-call' } }), /identity changed/u)
    assert.equal(journal(directory).workRunId, workRunId)
  } finally { first.close() }
})

test('supervisor fences an obsolete owner before any journal update', t => {
  const directory = fixture(t)
  const supervisor = acquireDeviceTaskSupervisor({ directory, identity })
  const ownerPath = join(directory, 'task-supervision.owner', 'owner.json')
  const originalOwner = readFileSync(ownerPath, 'utf8')
  const before = readFileSync(join(directory, 'task-supervision.json'), 'utf8')
  try {
    writeFileSync(ownerPath, JSON.stringify({ ...JSON.parse(originalOwner), id: 'different-owner' }))
    assert.throws(() => supervisor.checkpoint('completed'), { code: 'DEVICE_TASK_SUPERVISOR_FENCED' })
    assert.equal(readFileSync(join(directory, 'task-supervision.json'), 'utf8'), before)
  } finally {
    writeFileSync(ownerPath, originalOwner)
    supervisor.close()
  }
})

test('OS-confirmed dead owner takeover preserves the registered WorkRun', t => {
  const directory = fixture(t)
  const moduleUrl = new URL('../scripts/lib/device-task-supervisor.mjs', import.meta.url).href
  execFileSync(process.execPath, ['--input-type=module', '-e', `
    import { acquireDeviceTaskSupervisor } from ${JSON.stringify(moduleUrl)};
    const supervisor = acquireDeviceTaskSupervisor({directory: process.argv[1], identity: JSON.parse(process.argv[2])});
    supervisor.checkpoint('dispatched', {workRunId: process.argv[3]});
    process.exit(0);
  `, directory, JSON.stringify(identity), workRunId], { stdio: 'pipe' })
  const previous = journal(directory)
  const replacement = acquireDeviceTaskSupervisor({ directory, identity })
  try {
    const current = replacement.snapshot()
    assert.equal(current.phase, 'recovering')
    assert.equal(current.workRunId, workRunId)
    assert.equal(current.generation, previous.generation + 1)
    assert.notEqual(current.owner.id, previous.owner.id)
  } finally { replacement.close() }
})

test('supervisor retries observation and persists a bounded blocked result', async t => {
  const directory = fixture(t)
  const supervisor = acquireDeviceTaskSupervisor({ directory, identity })
  let observations = 0
  try {
    supervisor.checkpoint('dispatched', { workRunId })
    await assert.rejects(supervisor.drive(async () => {
      observations++
      throw Object.assign(new Error('private-response https://private.invalid token=secret'), { code: 'ECONNRESET' })
    }, { retryMillis: 0, recoveryLimit: 2 }), error => {
      assert.equal(error.unresolvedDeviceExecution, true)
      assert.equal(error.supervision.phase, 'blocked')
      return true
    })
    assert.equal(observations, 3)
    const saved = journal(directory)
    assert.equal(saved.workRunId, workRunId)
    assert.equal(saved.failures.length, 3)
    assert.equal(saved.blockedReason, 'ECONNRESET')
    assert.equal(JSON.stringify(saved).includes('private-response'), false)
    assert.equal(JSON.stringify(saved).includes('private.invalid'), false)
    assert.equal(JSON.stringify(saved).includes('token=secret'), false)
  } finally { supervisor.close() }
})

test('terminal execution failure is recorded once and is never retried', async t => {
  const supervisor = acquireDeviceTaskSupervisor({ directory: fixture(t), identity })
  let observations = 0
  try {
    await assert.rejects(supervisor.drive(async () => {
      observations++
      throw Object.assign(new Error('execution failed'), { code: 'DEVICE_EXECUTION_FAILED' })
    }, { retryMillis: 0, terminalError: error => error.code === 'DEVICE_EXECUTION_FAILED' }), { code: 'DEVICE_EXECUTION_FAILED' })
    assert.equal(observations, 1)
    assert.equal(supervisor.snapshot().failures.length, 1)
  } finally { supervisor.close() }
})

test('registered recovery observes the existing terminal task without submitting commands', async t => {
  const directory = fixture(t)
  writeFileSync(join(directory, 'device-task-result.json'), JSON.stringify({
    ...identity, workRunId, workerSessionId: 'wsn_01J00000000000000000000001', steps: [], workRuns: [],
    modelRoute: { providerId: 'local-fixture', modelId: 'local-fixture' },
  }))
  const calls = []
  const runtime = {
    api: {
      async query(kind) {
        calls.push(kind)
        if (kind === 'delivery.get') return { result: { status: 'done', deliveryRevision: 1,
          readCursor: { token: 'fixture-cursor' }, currentCandidate: { candidateRef: 'fixture-candidate' },
          verdict: { status: 'pass', criteria: [{ verdict: 'pass' }] }, attention: [], evidence: [{}] } }
        if (kind === 'workrun.get') return { result: { items: [{ state: 'done' }], runs: [{ id: workRunId, state: 'completed', workItemId: 'wit_fixture', executionJobId: 'job_fixture' }] } }
        throw new Error(`Unexpected query ${kind}`)
      },
      async command(kind) { calls.push(kind); throw new Error(`Recovery must not submit ${kind}`) },
    },
    devicePath: { forProductSession: () => ({}), assertBenchmarkRunning() {},
      async launchAnchor() { throw new Error('terminal WorkRun cannot require a new anchor') } },
  }
  const result = await resumeRegisteredDeviceTask({ launch: { ...identity, directory }, runtime })
  assert.equal(result.detail.status, 'done')
  assert.deepEqual(calls, ['delivery.get', 'workrun.get', 'workrun.get'])
  assert.equal(journal(directory).phase, 'completed')
  assert.equal(JSON.parse(readFileSync(join(directory, 'device-task-result.json'))).workRunId, workRunId)
})

test('execution-port approval ownership exposes pending approvals without a second decision', async () => {
  const decisions = []
  const pending = []
  const api = {
    async query(kind) {
      assert.equal(kind, 'approval.list')
      return { page: { hasMore: false }, result: { items: [{ id: 'apr_fixture', state: 'pending', category: 'shell',
        binding: { productSessionId: identity.productSessionId, workerSessionId: 'wsn_fixture', executionJobId: 'job_fixture',
          sessionIdentity: { workRunId, codexThreadId: 'thread-fixture', productSessionId: identity.productSessionId, workerSessionId: 'wsn_fixture' } } }] } }
    },
    async command(...args) { decisions.push(args); throw new Error('host owns the action decision') },
  }
  await resolveDeviceTaskApprovals({ api, runs: [{ id: workRunId, state: 'running', productSessionId: identity.productSessionId,
    codexThreadId: 'thread-fixture', workerSessionId: 'wsn_fixture', executionJobId: 'job_fixture' }], automaticTaskActions: true,
    approvalOwner: 'execution_port', onPending: value => pending.push(...value) })
  assert.equal(pending.length, 1)
  assert.equal(decisions.length, 0)
})

const benchmarkCells = () => Array.from({ length: 35 }, (_, index) => ({
  runId: `run-${index}`, taskId: 'rust-001', configurationId: `configuration-${Math.floor(index / 5)}`,
  comparison: index % 5 === 4 ? 'fusion' : `model-${index % 5}`, fusionKind: index % 5 === 4 ? 'parallel' : null,
}))
const retainedEntry = (cell, index) => {
  const record = { ...cell, status: 'completed', finalScore: 1 }
  return { index, record, provenance: { sourceLedger: '/fixture/old-benchmark.sqlite3',
    sourceLedgerSha256: 'a'.repeat(64), recordSha256: createHash('sha256').update(JSON.stringify(record)).digest('hex') } }
}

test('ledger imports seven completed facts and leaves twenty-eight tasks unclaimed', t => {
  const directory = fixture(t)
  const path = join(directory, 'benchmark.sqlite3')
  const cells = benchmarkCells()
  const entries = cells.slice(0, 7).map(retainedEntry)
  const ledger = openBenchmarkLedger(path, 'new-experiment', cells)
  try {
    ledger.retainCompleted(entries)
    ledger.retainCompleted(entries)
    const records = ledger.records()
    assert.equal(records.filter(record => record?.status === 'completed').length, 7)
    assert.equal(records.filter(record => record === null).length, 28)
    const database = new DatabaseSync(path, { readOnly: true })
    try {
      assert.equal(database.prepare('SELECT COUNT(*) AS n FROM benchmark_cell WHERE token IS NULL AND record IS NULL').get().n, 28)
      assert.equal(database.prepare('SELECT COUNT(*) AS n FROM benchmark_call').get().n, 0)
      assert.equal(database.prepare('SELECT COUNT(*) AS n FROM benchmark_launch').get().n, 0)
    } finally { database.close() }
    assert.equal(ledger.claim(0).record.retainedCompletion.sourceLedgerSha256, 'a'.repeat(64))
  } finally { ledger.close() }
})

test('ledger rejects failed, changed, and conflicting retained completion imports atomically', t => {
  const cells = benchmarkCells()
  for (const mutation of [
    entry => { entry.record.status = 'failed' },
    entry => { entry.record.configurationId = 'wrong-configuration' },
    entry => { entry.provenance.recordSha256 = 'b'.repeat(64) },
    entry => { entry.provenance.sourceLedger = 'relative.sqlite3' },
  ]) {
    const directory = fixture(t)
    const ledger = openBenchmarkLedger(join(directory, 'benchmark.sqlite3'), 'new-experiment', cells)
    try {
      const invalid = retainedEntry(cells[1], 1)
      mutation(invalid)
      refreshRecordHash(invalid)
      assert.throws(() => ledger.retainCompleted([retainedEntry(cells[0], 0), invalid]), { code: 'LEDGER_RETAINED_COMPLETION_INVALID' })
      assert.equal(ledger.records().every(record => record === null), true, 'import transaction must roll back earlier entries')
      ledger.claim(0)
      assert.throws(() => ledger.retainCompleted([retainedEntry(cells[0], 0)]), { code: 'LEDGER_RETAINED_COMPLETION_CONFLICT' })
    } finally { ledger.close() }
  }
})

function refreshRecordHash(entry) {
  // Keep the real invalid-record hash when isolating status/identity boundaries.
  if (entry.provenance.recordSha256 !== 'b'.repeat(64)) {
    entry.provenance.recordSha256 = createHash('sha256').update(JSON.stringify(entry.record)).digest('hex')
  }
}


test('supervisor rejects missing owner metadata and leaves its authority directory intact', t => {
  const directory = fixture(t)
  const lock = join(directory, 'task-supervision.owner')
  mkdirSync(lock)
  assert.throws(() => acquireDeviceTaskSupervisor({ directory, identity }), { code: 'DEVICE_TASK_SUPERVISOR_OWNER_UNCONFIRMED' })
  assert.equal(existsSync(lock), true)
})

test('fenced close preserves successor owner and retained journal', t => {
  const directory = fixture(t)
  const supervisor = acquireDeviceTaskSupervisor({ directory, identity })
  const ownerPath = join(directory, 'task-supervision.owner', 'owner.json')
  const successor = { ...JSON.parse(readFileSync(ownerPath)), id: 'successor-owner' }
  writeFileSync(ownerPath, JSON.stringify(successor))
  const before = readFileSync(join(directory, 'task-supervision.json'), 'utf8')
  supervisor.close()
  assert.deepEqual(JSON.parse(readFileSync(ownerPath)), successor)
  assert.equal(readFileSync(join(directory, 'task-supervision.json'), 'utf8'), before)
})

test('heartbeat authority failure becomes an explicit blocked observation', async t => {
  const directory = fixture(t)
  const supervisor = acquireDeviceTaskSupervisor({ directory, identity })
  const ownerPath = join(directory, 'task-supervision.owner', 'owner.json')
  try {
    writeFileSync(ownerPath, JSON.stringify({ ...JSON.parse(readFileSync(ownerPath)), id: 'successor-owner' }))
    await delay(5100)
    assert.equal(supervisor.snapshot().phase, 'blocked')
    assert.equal(supervisor.snapshot().blockedReason, 'DEVICE_TASK_SUPERVISOR_FENCED')
    assert.throws(() => supervisor.checkpoint('executing'), error => error.code === 'DEVICE_TASK_SUPERVISOR_FENCED' && error.unresolvedDeviceExecution === true)
  } finally { supervisor.close() }
})

function terminalRecoveryRuntime(calls, launchAnchor) {
  return { api: {
    async query(kind) {
      calls.push(kind)
      if (kind === 'worker.get') return { result: { id: 'worker-source', state: 'drained', revision: 1 } }
      if (kind === 'delivery.get') return { result: { status: 'done', deliveryRevision: 1,
        readCursor: { token: 'fixture-cursor' }, currentCandidate: { candidateRef: 'fixture-candidate' },
        verdict: { status: 'pass', criteria: [{ verdict: 'pass' }] }, attention: [], evidence: [{}] } }
      if (kind === 'workrun.get') return { result: { items: [{ state: 'done' }], runs: [{ id: workRunId, state: 'completed', workItemId: 'wit_fixture', executionJobId: 'job_fixture' }] } }
      throw new Error(`Unexpected query ${kind}`)
    },
    async command(kind) { calls.push(kind); throw new Error(`Recovery must not submit ${kind}`) },
  }, devicePath: { forProductSession: () => ({}), assertBenchmarkRunning() {}, launchAnchor } }
}

function writeRecoveryReport(directory, extra = {}) {
  writeFileSync(join(directory, 'device-task-result.json'), JSON.stringify({ ...identity, workRunId,
    steps: [], workRuns: [], modelRoute: { providerId: 'local-fixture', modelId: 'local-fixture' }, ...extra }))
}

test('post-dispatch anchor failure retries supervision on the same WorkRun', async t => {
  const directory = fixture(t)
  writeRecoveryReport(directory)
  const calls = [], launchedIds = []
  const runtime = terminalRecoveryRuntime(calls, async ({ workRunId: id }) => {
    launchedIds.push(id)
    if (launchedIds.length === 1) throw Object.assign(new Error('local transport interrupted'), { code: 'ECONNRESET' })
    return { workerSessionId: 'wsn_fixture' }
  })
  const result = await resumeRegisteredDeviceTask({ launch: { ...identity, directory }, runtime })
  assert.equal(result.detail.status, 'done')
  assert.deepEqual(launchedIds, [workRunId, workRunId])
  assert.equal(calls.some(kind => kind.includes('start') || kind.includes('launch')), false)
  assert.equal(journal(directory).failures.length, 1)
  assert.equal(journal(directory).failures[0].code, 'ECONNRESET')
  assert.equal(journal(directory).workRunId, workRunId)
  assert.equal(JSON.parse(readFileSync(join(directory, 'device-task-result.json'))).workRunId, workRunId)
})

function anchorRegistry(directory, pid, boot, state = 'running') {
  const deviceData = join(directory, 'device-data')
  mkdirSync(deviceData)
  mkdirSync(join(directory, 'server-data'))
  const server = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'))
  server.exec(`CREATE TABLE scheduler_execution_jobs (product_session_id TEXT, work_run_id TEXT, dispatch_payload BLOB, delivery_id TEXT, state TEXT, submitted_at TEXT, job_id TEXT);
    CREATE TABLE execution_leases (worker_id TEXT, worker_instance_id TEXT, lease_id TEXT);
    CREATE TABLE execution_lease_terminals (lease_id TEXT);`)
  server.close()
  const device = new DatabaseSync(join(deviceData, 'device-client.sqlite3'))
  device.exec('CREATE TABLE worker_process_registry (worker_session_id TEXT, worker_id TEXT, worker_instance_id TEXT, pid INTEGER, process_start_identity TEXT, state TEXT)')
  device.prepare('INSERT INTO worker_process_registry VALUES (?, ?, ?, ?, ?, ?)').run('source-session', 'worker-source', 'instance-source', pid, boot, state)
  return { deviceData, device, launched: { workerSessionId: 'source-session', workerId: 'worker-source', workerInstanceId: 'instance-source' } }
}

test('source-anchor exit projection can resume after its process has already exited', async t => {
  const directory = fixture(t)
  const anchor = anchorRegistry(directory, 2147483647, 'nonexistent-boot')
  try {
    const calls = []
    const runtime = terminalRecoveryRuntime(calls, async () => { throw new Error('already registered role') })
    writeRecoveryReport(directory, { workerSessionId: 'wsn_fixture', sourceAuthorityAnchor: anchor.launched })
    runtime.devicePath.deviceData = anchor.deviceData
    const projection = setTimeout(() => anchor.device.prepare("UPDATE worker_process_registry SET state='exited'").run(), 100)
    try { await resumeRegisteredDeviceTask({ launch: { ...identity, directory }, runtime }) }
    finally { clearTimeout(projection) }
    const report = JSON.parse(readFileSync(join(directory, 'device-task-result.json')))
    assert.equal(report.sourceAnchor.state, 'drained')
    assert.deepEqual(report.sourceAuthorityAnchor, anchor.launched)
    assert.equal(report.complete, true)
    assert.equal(calls.filter(kind => kind === 'worker.get').length, 1)
  } finally { anchor.device.close() }
})

test('source-anchor cleanup preserves a live PID with a different boot identity', async t => {
  const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' })
  const directory = fixture(t)
  const anchor = anchorRegistry(directory, child.pid, 'different-boot', 'exited')
  try {
    await delay(50)
    const runtime = terminalRecoveryRuntime([], async () => {})
    const result = await stopUnusedDeviceTaskAnchor({ api: runtime.api, directory, deviceData: anchor.deviceData,
      launched: anchor.launched, productSessionId: identity.productSessionId })
    assert.equal(result.state, 'drained')
    assert.equal(child.exitCode, null)
    assert.equal(child.signalCode, null)
    process.kill(child.pid, 0)
  } finally {
    anchor.device.close()
    child.kill('SIGKILL')
    await new Promise(resolve => child.once('exit', resolve))
  }
})


test('supervision binds one WorkRun and protects immutable ownership fields', t => {
  const directory = fixture(t)
  const inputIdentity = { ...identity }
  const supervisor = acquireDeviceTaskSupervisor({ directory, identity: inputIdentity })
  try {
    inputIdentity.callId = 'mutated-caller'
    supervisor.checkpoint('dispatched', { workRunId })
    for (const details of [{ workRunId: 'different-workrun' }, { identity: {} }, { owner: {} }, { generation: 99 }, { revision: 99 }]) {
      assert.throws(() => supervisor.checkpoint('executing', details))
      assert.equal(journal(directory).workRunId, workRunId)
      assert.deepEqual(journal(directory).identity, identity)
    }
  } finally { supervisor.close() }
})

test('supervision heartbeat advances liveness without inventing business progress', async t => {
  const directory = fixture(t)
  const supervisor = acquireDeviceTaskSupervisor({ directory, identity })
  try {
    supervisor.checkpoint('dispatched', { workRunId })
    const initial = supervisor.snapshot()
    await delay(5100)
    const heartbeat = supervisor.snapshot()
    assert.notEqual(heartbeat.updatedAt, initial.updatedAt)
    assert.equal(heartbeat.lastProgressAt, initial.lastProgressAt)
    supervisor.checkpoint('executing', { activeWorkRunIds: [workRunId] })
    assert.notEqual(supervisor.snapshot().lastProgressAt, initial.lastProgressAt)
  } finally { supervisor.close() }
})

test('concurrent stale takeover keeps exactly one live owner and fences the losing process', async t => {
  const directory = fixture(t)
  const moduleUrl = process.env.WWC_TEST_SUPERVISOR_MODULE ?? new URL('../scripts/lib/device-task-supervisor.mjs', import.meta.url).href
  execFileSync(process.execPath, ['--input-type=module', '-e', `
    const { acquireDeviceTaskSupervisor } = await import(${JSON.stringify(moduleUrl)});
    const owner = acquireDeviceTaskSupervisor({directory: process.argv[1], identity: JSON.parse(process.argv[2])});
    owner.checkpoint('dispatched', {workRunId: process.argv[3]});
    process.exit(0);
  `, directory, JSON.stringify(identity), workRunId], { stdio: 'pipe' })
  const before = readFileSync(join(directory, 'task-supervision.json'), 'utf8')
  const staleId = JSON.parse(before).owner.id
  const childCode = `
    import fs from 'node:fs';
    import {join} from 'node:path';
    import {syncBuiltinESMExports} from 'node:module';
    const [directory, side, moduleUrl, identityJson, staleId] = process.argv.slice(1);
    const path = name => join(directory, name);
    const lock = path('task-supervision.owner');
    const read = fs.readFileSync, rename = fs.renameSync;
    const wait = target => {
      const deadline = Date.now() + 10_000;
      while (!fs.existsSync(target)) {
        if (Date.now() > deadline) throw new Error('fixture barrier expired');
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 5);
      }
    };
    let paused = false;
    fs.readFileSync = function(target, ...options) {
      const value = read(target, ...options);
      if (!paused && String(target) === join(lock, 'owner.json') && JSON.parse(value).id === staleId) {
        paused = true;
        fs.writeFileSync(path(side + '-read-stale'), 'ready');
        wait(path('release-stale-read'));
      }
      return value;
    };
    fs.renameSync = function(source, target) {
      if (side === 'B' && String(source) === lock) wait(path('A-result.json'));
      return rename(source, target);
    };
    syncBuiltinESMExports();
    const {acquireDeviceTaskSupervisor} = await import(moduleUrl);
    let supervisor;
    try {
      supervisor = acquireDeviceTaskSupervisor({directory, identity: JSON.parse(identityJson)});
      fs.writeFileSync(path(side + '-result.json'), JSON.stringify({status: 'acquired', owner: supervisor.snapshot().owner}));
      wait(path('release-child'));
    } catch (error) {
      fs.writeFileSync(path(side + '-result.json'), JSON.stringify({status: 'blocked', code: error.code ?? 'FIXTURE_FAILED'}));
    } finally { supervisor?.close(); }
  `
  const children = []
  const start = side => {
    const child = spawn(process.execPath, ['--input-type=module', '-e', childCode, directory, side, moduleUrl, JSON.stringify(identity), staleId], { stdio: 'pipe' })
    children.push(child)
    return child
  }
  const waitFor = async condition => {
    const deadline = Date.now() + 10_000
    while (!condition()) {
      assert.ok(Date.now() < deadline, 'real child process did not reach its fixture barrier')
      await delay(5)
    }
  }
  try {
    start('A')
    await waitFor(() => existsSync(join(directory, 'A-read-stale')))
    start('B')
    await waitFor(() => existsSync(join(directory, 'B-read-stale')) || existsSync(join(directory, 'B-result.json')))
    assert.equal(readFileSync(join(directory, 'task-supervision.json'), 'utf8'), before, 'a blocked claimant cannot write the journal')
    writeFileSync(join(directory, 'release-stale-read'), 'go')
    await waitFor(() => existsSync(join(directory, 'A-result.json')) && existsSync(join(directory, 'B-result.json')))
    const first = JSON.parse(readFileSync(join(directory, 'A-result.json'), 'utf8'))
    const second = JSON.parse(readFileSync(join(directory, 'B-result.json'), 'utf8'))
    assert.equal([first, second].filter(result => result.status === 'acquired').length, 1, 'two stale claimants must never acquire concurrently')
    assert.equal(first.status, 'acquired')
    assert.deepEqual(second, { status: 'blocked', code: 'DEVICE_TASK_SUPERVISOR_OWNER_UNCONFIRMED' })
    assert.deepEqual(JSON.parse(readFileSync(join(directory, 'task-supervision.owner', 'owner.json'), 'utf8')), first.owner)
    assert.equal(journal(directory).owner.id, first.owner.id)
    assert.equal(journal(directory).generation, JSON.parse(before).generation + 1)
    assert.equal(journal(directory).workRunId, workRunId)
  } finally {
    writeFileSync(join(directory, 'release-child'), 'go')
    writeFileSync(join(directory, 'release-stale-read'), 'go')
    await delay(20)
    for (const child of children) if (child.exitCode === null) child.kill('SIGKILL')
  }
})

test('an orphaned takeover guard blocks acquisition before a new owner directory exists', t => {
  const directory = fixture(t)
  const guard = join(directory, 'task-supervision.owner.takeover')
  mkdirSync(guard)
  writeFileSync(join(guard, 'fixture-crash-witness'), 'retained')
  assert.throws(() => acquireDeviceTaskSupervisor({ directory, identity }), { code: 'DEVICE_TASK_SUPERVISOR_OWNER_UNCONFIRMED' })
  assert.equal(readFileSync(join(guard, 'fixture-crash-witness'), 'utf8'), 'retained')
  assert.equal(existsSync(join(directory, 'task-supervision.owner')), false)
  assert.equal(existsSync(join(directory, 'task-supervision.json')), false)
})
