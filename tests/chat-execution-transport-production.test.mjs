// SPDX-License-Identifier: Apache-2.0

// Production acceptance replays original Worker bytes through real HTTPS.
import assert from 'node:assert/strict'
import { globSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'

import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import {
  assertSuccessfulRetainedCommands, readRetainedCoreCommandResults,
  readRetainedOutcomes, readRetainedRuntime, readWorkerTransport, runtimeLedgerSnapshot,
} from './fixtures/chat-execution-transport-probe.mjs'

function sealedBinaries() {
  const directory = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  assert.ok(directory, 'production acceptance requires explicit sealed binaries')
  return { build: false, serverBinary: join(directory, 'winwincode-server'),
    wwcBinary: join(directory, 'wwc') }
}

function terminalSnapshot(database, jobId) {
  const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId)
  return {
    job,
    sessionJobs: database.prepare('SELECT * FROM scheduler_execution_jobs WHERE product_session_id=? ORDER BY job_id')
      .all(job.product_session_id),
    sessionDispatches: database.prepare(`SELECT * FROM execution_dispatch_authorities WHERE job_id IN
      (SELECT job_id FROM scheduler_execution_jobs WHERE product_session_id=?) ORDER BY job_id`).all(job.product_session_id),
    terminals: database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').all(jobId),
    accepted: database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
      .get(`execution-outcome:${jobId}:1`),
    reservation: database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId),
  }
}

async function waitCompleted(database, jobId, deviceData) {
  return waitFor(() => {
    const snapshot = terminalSnapshot(database, jobId)
    const payload = snapshot.accepted && JSON.parse(Buffer.from(snapshot.accepted.payload).toString('utf8'))
    const [worker] = readRetainedOutcomes(deviceData, jobId)
    if (snapshot.job?.state !== 'completed' || snapshot.terminals.length !== 1
      || snapshot.accepted?.revision !== 2 || !payload?.acknowledgementReady || !worker?.workerAcknowledged) return false
    assert.deepEqual(payload.message, worker.message,
      'the accepted raw outcome equals the immutable original Worker terminal')
    return snapshot
  }, 'one original completed terminal and its business acknowledgement', 120_000)
}

function coreSnapshot(deviceData, jobId) {
  const runs = globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) return []
      return store.prepare('SELECT record_json FROM codex_run').all().flatMap(row => {
        const bytes = Buffer.from(row.record_json)
        const run = JSON.parse(bytes.toString('utf8'))
        return run.job.jobId === jobId ? [{ bytes, run, rollout: readFileSync(run.rolloutPath) }] : []
      })
    } finally { store.close() }
  })
  assert.equal(runs.length, 1, 'the original Job has one actual Core run')
  const [original] = runs
  const commands = readRetainedCoreCommandResults(deviceData, original.run.kernelSessionId)
  assertSuccessfulRetainedCommands(commands, 16)
  return { bytes: original.bytes, rollout: original.rollout, commands }
}

for (const variant of ['completed-replay', 'genuine-gap']) {
  test(`Chat transport ${variant} uses real retained Worker bytes and rejects changed runtime content`, async t => {
    const directory = mkdtempSync(join(tmpdir(), `wwc-chat-transport-${variant}-`))
    const productSessionId = variant === 'completed-replay'
      ? 'psn_01J00000000000000000000131' : 'psn_01J00000000000000000000132'
    const requestId = variant === 'completed-replay'
      ? 'req_01J00000000000000000000131' : 'req_01J00000000000000000000132'
    const marker = `transport-${variant}`
    let complete = false
    let paused = null
    t.after(() => {
      if (paused !== null) { try { process.kill(paused, 'SIGCONT') } catch { /* cleanup owns the process */ } }
      if (complete) rmSync(directory, { recursive: true, force: true })
      else t.diagnostic(`Failed transport evidence retained at ${directory}`)
    })
    const report = await runApiProductionVertical({
      directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
      serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
      deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: [marker], nativeCellScripts: {
        'native-cell-A': 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command tool missing"); for(let index=0;index<16;index++) { const result=await tools[command.name]({cmd:"true",yield_time_ms:1000}); if(result.exit_code!==0) throw new Error("original command did not exit successfully"); } text("sixteen real commands completed");',
      } },
      scenario: { async run({ api, modelRoute, devicePath }) {
        const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
        database.exec('PRAGMA busy_timeout=5000')
        let probe
        try {
          await api.command('session.create', 0, { productSessionId,
            projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
            title: `Chat retained transport ${variant}`, modelRoute })
          await devicePath.launchAnchor({ productSessionId })
          await api.command('chat.submit', 1, { productSessionId, message: `native-cell-A ${marker}` }, requestId)
          const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').get(requestId)
          await waitFor(() => devicePath.modelServer.requests.some(value => value.gateMarker === marker && value.active),
            'the original physical model request is held before it returns genuine tool calls', 60_000)
          const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
          probe = readWorkerTransport(devicePath.deviceData, authority)
          const ca = readFileSync(join(directory, 'fixture-cert.pem'))
          let gap = null
          if (variant === 'genuine-gap') {
            const stream = `product-session:${productSessionId}`
            // Refuse a real commit. Do not insert events, receipts, heads or ACKs.
            database.exec(`CREATE TRIGGER pause_chat_runtime_insert BEFORE INSERT ON product_state
              WHEN NEW.stream_id='${stream}' BEGIN SELECT RAISE(ABORT,'pause genuine runtime commit'); END`)
            database.exec(`CREATE TRIGGER pause_chat_runtime_update BEFORE UPDATE ON product_state
              WHEN NEW.stream_id='${stream}' BEGIN SELECT RAISE(ABORT,'pause genuine runtime commit'); END`)
            devicePath.modelServer.releaseResponseGate(marker)
            const pending = await waitFor(() => {
              const rows = readRetainedRuntime(devicePath.deviceData, job.job_id)
                .filter(row => row.sequence > row.ack_sequence)
              return rows.length >= 2 ? rows : false
            }, 'at least two original Core runtime frames are retained without business ACK', 60_000)
            // Freeze the actual Worker, including its embedded Core, so its
            // automatic retry cannot race the deliberate transport order.
            process.kill(probe.pid, 'SIGSTOP')
            paused = probe.pid
            const before = runtimeLedgerSnapshot(database, productSessionId)
            const expected = before ? JSON.parse(before.bytes.toString('utf8')).highestSequence + 1 : 1
            const first = pending.find(row => row.message.event.sequence === expected)
            const later = pending.find(row => row.message.event.sequence === expected + 1)
            assert.ok(first && later, 'the gap uses two real consecutive frames, never invented sequence values')
            database.exec('DROP TRIGGER pause_chat_runtime_insert')
            database.exec('DROP TRIGGER pause_chat_runtime_update')
            const rejected = await probe.postRetainedMessage(later.bytes, { ca })
            assert.equal(rejected.status, 200, 'the genuine gap reaches the authenticated shared Core')
            assert.equal(rejected.acknowledgement?.status, 'gap')
            assert.equal(rejected.acknowledgement.replayFromSequence, expected)
            assert.equal(rejected.acknowledgement.ackSequence, expected - 1)
            assert.deepEqual(runtimeLedgerSnapshot(database, productSessionId), before,
              'an out-of-order frame does not advance the accepted business ledger')
            for (const row of [first, later]) {
              const accepted = await probe.postRetainedMessage(row.bytes, { ca })
              assert.equal(accepted.status, 200)
              assert.equal(accepted.acknowledgement?.status, 'accepted')
              assert.equal(accepted.acknowledgement.ackSequence, row.message.event.sequence)
            }
            gap = { expected, observed: later.message.event.sequence, rejected: 'gap', replayed: 2 }
            process.kill(probe.pid, 'SIGCONT')
            paused = null
          } else devicePath.modelServer.releaseResponseGate(marker)
          await waitCompleted(database, job.job_id, devicePath.deviceData)
          process.kill(probe.pid, 'SIGSTOP')
          paused = probe.pid
          const before = { terminal: terminalSnapshot(database, job.job_id),
            runtime: runtimeLedgerSnapshot(database, productSessionId), core: coreSnapshot(devicePath.deviceData, job.job_id) }
          const runtime = readRetainedRuntime(devicePath.deviceData, job.job_id)
          assert.ok(runtime.length > 0)
          assert.ok(runtime.every(row => row.sequence <= row.ack_sequence), 'the original runtime history has real ACKs')
          const [outcome] = readRetainedOutcomes(devicePath.deviceData, job.job_id)
          const count = devicePath.modelServer.requests.length
          const duplicate = await probe.postRetainedMessage(runtime[0].bytes, { ca })
          assert.equal(duplicate.status, 200)
          assert.equal(duplicate.acknowledgement?.status, 'duplicate')
          const changed = structuredClone(runtime[0].message)
          changed.event.summary = 'changed payload under the original accepted identity'
          const conflict = await probe.postRetainedMessage(Buffer.from(JSON.stringify(changed)), { ca })
          assert.equal(conflict.status, 200, 'a content conflict reaches the authenticated shared Core')
          assert.equal(conflict.acknowledgement?.status, 'rejected_conflict')
          let terminalReplay = await probe.postRetainedMessage(outcome.bytes, { ca })
          assert.equal(terminalReplay.status, 200)
          let originalAckTransportPending = false
          if (terminalReplay.acknowledgement?.status === 'accepted') {
            // The real Worker has applied its original business ACK, but may
            // not yet have returned that delivery's transport confirmation.
            // The probe confirms only the exact ACK it actually received.
            originalAckTransportPending = true
            terminalReplay = await probe.postRetainedMessage(outcome.bytes, { ca })
            assert.equal(terminalReplay.status, 200)
          }
          assert.equal(terminalReplay.acknowledgement?.status, 'duplicate')
          assert.deepEqual({ terminal: terminalSnapshot(database, job.job_id),
            runtime: runtimeLedgerSnapshot(database, productSessionId), core: coreSnapshot(devicePath.deviceData, job.job_id) }, before)
          assert.equal(devicePath.modelServer.requests.length, count)
          const heartbeat = database.prepare('SELECT heartbeat_sequence FROM execution_workers WHERE worker_id=?')
            .get(probe.workerId)
          process.kill(probe.pid, 'SIGCONT')
          paused = null
          const workerPostProbe = await waitFor(() => {
            const current = database.prepare('SELECT worker_instance_id,heartbeat_sequence FROM execution_workers WHERE worker_id=?')
              .get(probe.workerId)
            if (current?.worker_instance_id === probe.workerInstanceId
                && current.heartbeat_sequence >= heartbeat.heartbeat_sequence + 2) {
              return { kind: 'heartbeats', count: current.heartbeat_sequence - heartbeat.heartbeat_sequence }
            }
            const exits = database.prepare('SELECT * FROM worker_launch_grant_exits WHERE worker_session_id=? AND worker_instance_id=?')
              .all(probe.launchWorkerSessionId, probe.workerInstanceId)
            assert.ok(exits.length <= 1, 'the original managed launch has one actual exit receipt')
            if (exits.length === 0) return false
            const [exit] = exits
            assert.ok(Date.parse(exit.observed_at) >= Date.parse(before.terminal.terminals[0].terminal_at))
            const device = new DatabaseSync(join(devicePath.deviceData, 'device-client.sqlite3'), { readOnly: true })
            try {
              device.exec('PRAGMA busy_timeout=5000')
              const process = device.prepare('SELECT state,exit_code,pid FROM worker_process_registry WHERE worker_session_id=? AND worker_instance_id=?')
                .get(probe.launchWorkerSessionId, probe.workerInstanceId)
              assert.equal(process?.pid, probe.pid)
              assert.equal(process.state, 'exited')
              assert.equal(process.exit_code, 0, 'the acknowledged managed Worker drains and exits successfully')
            } finally { device.close() }
            return { kind: 'process-exit', observedAt: exit.observed_at, exitCode: 0 }
          }, 'the actual original Worker resumes formal heartbeats or completes its acknowledged managed execution', 30_000)
          assert.deepEqual({ terminal: terminalSnapshot(database, job.job_id),
            runtime: runtimeLedgerSnapshot(database, productSessionId), core: coreSnapshot(devicePath.deviceData, job.job_id) }, before)
          assert.equal(devicePath.modelServer.requests.length, count)
          return { jobId: job.job_id, workerInstanceId: probe.workerInstanceId, gap, originalAckTransportPending,
            runtimeDuplicate: 'duplicate', changedPayload: 'rejected_conflict', terminalDuplicate: 'duplicate',
            businessStateUnchanged: true, originalCoreHistoryUnchanged: true, realCommands: 16,
            workerPostProbe, resumedWorkerHeartbeatObserved: workerPostProbe.kind === 'heartbeats', extraHttp: 0 }
        } finally {
          database.exec('DROP TRIGGER IF EXISTS pause_chat_runtime_insert')
          database.exec('DROP TRIGGER IF EXISTS pause_chat_runtime_update')
          devicePath.modelServer.releaseResponseGate(marker)
          if (paused !== null) { process.kill(paused, 'SIGCONT'); paused = null }
          database.close()
        }
      } },
    })
    t.diagnostic(JSON.stringify(report.flow.scenario))
    complete = true
  })
}
