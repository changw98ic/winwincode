// SPDX-License-Identifier: Apache-2.0

// Real Gap control consumption and replay of the original Worker-owned range.
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { globSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { isDeepStrictEqual } from 'node:util'

import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import {
  assertSuccessfulRetainedCommands, readRetainedCoreCommandResults,
  readRetainedOutcomes, readRetainedRuntime, readWorkerTransport, runtimeLedgerSnapshot,
} from './fixtures/chat-execution-transport-probe.mjs'

const COMMAND_COUNT = 96
const sha = bytes => createHash('sha256').update(bytes).digest('hex')
const decode = bytes => JSON.parse(Buffer.from(bytes).toString('utf8'))

function sealedBinaries() {
  const directory = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  assert.ok(directory, 'bounded replay acceptance requires explicit sealed production binaries')
  return { build: false, serverBinary: join(directory, 'winwincode-server'),
    wwcBinary: join(directory, 'wwc') }
}

function workerFrame(bytes) {
  return Buffer.concat([Buffer.from('{"frameType":"message","direction":"worker-to-control-plane","message":'),
    bytes, Buffer.from('}')])
}

function runtimeTransport(probe, runtime) {
  const store = new DatabaseSync(probe.networkJournalPath, { readOnly: true })
  try {
    store.exec('PRAGMA busy_timeout=5000')
    return runtime.map(row => {
      const operation = probe.workerId + ':' + probe.workerInstanceId + ':' + row.message.messageId
      const hash = sha(operation)
      const cursor = store.prepare('SELECT cursor_json FROM network_envelope_cursors WHERE operation_hash=?').get(hash)
      const state = store.prepare('SELECT state_json FROM network_queue_state WHERE operation_hash=?').get(hash)
      const attempts = store.prepare('SELECT * FROM network_request_attempts WHERE operation_hash=? ORDER BY sequence').all(hash)
      const wire = workerFrame(row.bytes)
      if (cursor) assert.equal(JSON.parse(cursor.cursor_json).input_digest, sha(wire),
        'the actual Worker HTTPS envelope is bound to the complete immutable original frame bytes')
      return { sequence: row.sequence, messageId: row.message.messageId, hash,
        originalBytesDigest: sha(row.bytes), wireBytes: wire.length,
        cursor: cursor ? JSON.parse(cursor.cursor_json) : null,
        state: state ? JSON.parse(state.state_json) : null, attempts }
    })
  } finally { store.close() }
}

function gapConfirmations(probe, messageId) {
  const store = new DatabaseSync(probe.networkJournalPath, { readOnly: true })
  try {
    store.exec('PRAGMA busy_timeout=5000')
    return store.prepare('SELECT operation_hash,sequence,cursor_json FROM acceptance_completed_envelopes').all().flatMap(row => {
      const cursor = JSON.parse(row.cursor_json)
      if (!cursor.ack_ids.includes(messageId)) return []
      const attempts = store.prepare('SELECT * FROM network_request_attempts WHERE operation_hash=? AND sequence=?')
        .all(row.operation_hash, row.sequence)
      return attempts.length === 1 && attempts[0].outcome === 'completed'
        ? [{ hash: row.operation_hash, inputDigest: cursor.input_digest, attempts }] : []
    })
  } finally { store.close() }
}

function observeCompletedEnvelopes(probe) {
  const store = new DatabaseSync(probe.networkJournalPath)
  try {
    store.exec(`PRAGMA busy_timeout=5000;
      CREATE TABLE acceptance_completed_envelopes (
        operation_hash TEXT NOT NULL, sequence INTEGER NOT NULL, cursor_json TEXT NOT NULL,
        PRIMARY KEY(operation_hash,sequence));
      CREATE TRIGGER observe_completed_envelope BEFORE DELETE ON network_envelope_cursors
      BEGIN
        INSERT INTO acceptance_completed_envelopes(operation_hash,sequence,cursor_json)
          SELECT OLD.operation_hash,sequence,OLD.cursor_json FROM network_request_attempts
          WHERE operation_hash=OLD.operation_hash AND outcome='completed'
          ORDER BY sequence DESC LIMIT 1;
      END;`)
  } finally { store.close() }
  // NetworkJournal::finish removes a successful envelope in the same transaction
  // that completes its real HTTP attempt. Observe that exact preimage instead of
  // racing a transient cursor. This trace does not modify any execution fact.
  return () => {
    const cleanup = new DatabaseSync(probe.networkJournalPath)
    try { cleanup.exec('PRAGMA busy_timeout=5000; DROP TRIGGER IF EXISTS observe_completed_envelope') }
    finally { cleanup.close() }
  }
}

function coreRun(deviceData, jobId) {
  const runs = globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) return []
      return store.prepare('SELECT record_json FROM codex_run').all().map(row => decode(row.record_json))
        .filter(run => run.job.jobId === jobId)
    } finally { store.close() }
  })
  assert.equal(runs.length, 1, 'the same original Job retains one actual embedded Core run')
  return runs[0]
}

function coreModelCalls(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      return store.prepare('SELECT run_key,record_json FROM codex_run').all().flatMap(row => {
        if (decode(row.record_json).job.jobId !== jobId) return []
        return store.prepare('SELECT * FROM model_call_ledger WHERE run_key=? ORDER BY ordinal')
          .all(row.run_key).map(call => ({ ...call, frames: store.prepare(`
            SELECT sequence,frame_digest FROM model_call_frame
            WHERE run_key=? AND model_call_id=? ORDER BY sequence`).all(row.run_key, call.model_call_id) }))
      })
    } finally { store.close() }
  })
}

function leasePeriod(authority) {
  return Object.fromEntries(['job_id', 'lease_id', 'payload_digest', 'worker_id', 'worker_instance_id',
    'attempt', 'fencing_token', 'issued_at', 'expires_at'].map(field => [field, authority[field]]))
}

function assertAcceptedLeasePeriod(database, period) {
  const receipts = database.prepare(`SELECT response_json FROM execution_lease_request_receipts
    WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(period.job_id, period.expires_at)
  assert.ok(receipts.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, period)),
    'the complete nine-field original lease period belongs to a real accepted claim or renewal receipt')
}

function assertOriginalAdmittedLease(database, originalDispatch, job, stamp) {
  assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id),
    originalDispatch, 'the complete original dispatch authority remains immutable across Gap consumption and replay')
  const original = leasePeriod(originalDispatch)
  const current = leasePeriod(database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(job.job_id))
  const terminal = { job_id: stamp.jobId, lease_id: stamp.leaseId, payload_digest: job.payload_digest,
    worker_id: stamp.workerId, worker_instance_id: stamp.workerInstanceId, attempt: stamp.attempt,
    fencing_token: stamp.fencingToken, issued_at: stamp.issuedAt, expires_at: stamp.expiresAt }
  assert.equal(job.payload_digest, original.payload_digest)
  for (const field of Object.keys(original).filter(field => field !== 'expires_at')) {
    assert.equal(current[field], original[field])
    assert.equal(terminal[field], original[field])
  }
  for (const period of [original, current, terminal]) assertAcceptedLeasePeriod(database, period)
  assert.ok(Date.parse(terminal.expires_at) >= Date.parse(original.expires_at))
  assert.ok(Date.parse(current.expires_at) >= Date.parse(terminal.expires_at))
}

function acceptedTerminal(database, deviceData, jobId, originalDispatch) {
  const jobs = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').all(jobId)
  const raw = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
    .get('execution-outcome:' + jobId + ':1')
  const accepted = raw ? decode(raw.payload) : null
  const outcomes = readRetainedOutcomes(deviceData, jobId)
  const terminals = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').all(jobId)
  const slot = database.prepare('SELECT * FROM worker_session_slots WHERE job_id=?').get(jobId)
  const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId)
  if (jobs.length !== 1 || jobs[0].state !== 'completed' || raw?.revision !== 2 || !accepted?.acknowledgementReady
    || outcomes.length !== 1 || !outcomes[0].workerAcknowledged || terminals.length !== 1
    || terminals[0].outcome !== 'completed' || slot?.state !== 'completed' || reservation?.state !== 'settled') return false
  const [outcome] = outcomes
  assert.deepEqual(accepted.message, outcome.message)
  assert.equal(outcome.message.outcome.status, 'succeeded')
  assert.equal(jobs[0].attempt, 1)
  assert.equal(outcome.message.lease.attempt, 1)
  assert.equal(outcome.message.sessionIdentity.productSessionId, jobs[0].product_session_id)
  assert.equal(outcome.message.sessionIdentity.workerSessionId, outcome.message.workerSessionId)
  assert.equal(outcome.message.sessionIdentity.codexThreadId, slot.codex_thread_id)
  assert.equal(outcome.message.sessionIdentity.workRunId ?? null, null)
  assertOriginalAdmittedLease(database, originalDispatch, jobs[0], outcome.message.lease)
  for (const [column, key] of [['job_id', 'jobId'], ['lease_id', 'leaseId'], ['worker_id', 'workerId'],
    ['worker_instance_id', 'workerInstanceId'], ['attempt', 'attempt'], ['fencing_token', 'fencingToken']]) {
    assert.equal(terminals[0][column], outcome.message.lease[key])
    assert.equal(slot[column], outcome.message.lease[key])
  }
  assert.equal(slot.worker_session_id, outcome.message.workerSessionId)
  assert.equal(originalDispatch.worker_session_id, outcome.message.workerSessionId)
  const usage = outcome.message.outcome.usage
  assert.equal(usage.accountingStatus, 'known')
  assert.equal(reservation.actual_tokens, usage.tokens)
  assert.equal(reservation.actual_cost_microunits, usage.costMicrounits)
  assert.equal(reservation.actual_runtime_millis, usage.runtimeMillis)
  assert.equal(reservation.terminal_at, outcome.message.outcome.finishedAt)
  const retainedUsage = database.prepare('SELECT usage_json FROM execution_attempt_terminal_usage WHERE job_id=? AND attempt=1')
    .get(jobId)
  assert.ok(retainedUsage, 'the actual original raw terminal usage remains durably attributable')
  assert.deepEqual(JSON.parse(retainedUsage.usage_json), usage)
  return { job: jobs[0], raw, accepted, outcome, terminals, slot, reservation }
}

function physicalAccounting(database, jobId, calls, usage) {
  const manifest = calls.map(call => 'primary:' + call.exchangeId).sort()
  const record = database.prepare('SELECT statement_digest,statement_json FROM execution_attempt_accounting_facts WHERE job_id=? AND attempt=1')
    .all(jobId).map(row => {
      assert.equal('sha256:' + sha(row.statement_json), row.statement_digest)
      return { statement: JSON.parse(row.statement_json), digest: row.statement_digest }
    }).find(row => isDeepStrictEqual(row.statement.manifest, manifest))
  if (!record) return false
  const total = database.prepare('SELECT * FROM execution_attempt_accounting_totals WHERE job_id=? AND attempt=1').get(jobId)
  if (!total) return false
  const tokens = calls.reduce((sum, call) => sum + call.providerAttempts[0].usage.totalTokens, 0)
  assert.equal(total.tokens, tokens)
  assert.equal(total.known_tokens, tokens)
  assert.equal(total.cost, null, 'unpriced physical receipts retain unavailable cost')
  assert.equal(usage.tokens, total.tokens)
  assert.equal(usage.knownTokens, total.known_tokens)
  assert.equal(usage.costMicrounits, total.cost)
  return { manifest, tokens, knownTokens: total.known_tokens, costMicrounits: total.cost,
    statementDigest: record.digest }
}

test('Chat consumes its real Gap control and automatically replays only the finite original unacknowledged range', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-bounded-gap-'))
  const productSessionId = 'psn_01J00000000000000000000201'
  const requestId = 'req_01J00000000000000000000201'
  const marker = 'bounded-real-gap'
  const markerCodes = [...marker].map(character => character.charCodeAt(0)).join(',')
  let complete = false
  let paused = null
  let stopObservation
  t.after(() => {
    if (paused !== null) { try { process.kill(paused, 'SIGCONT') } catch { /* fixture owns process cleanup */ } }
    stopObservation?.()
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic('Unverified real bounded Gap fixture retained at ' + directory)
  })
  const commandScript = 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); '
    + 'if(!command) throw new Error("command tool missing"); '
    + 'const results=await Promise.allSettled(Array.from({length:' + COMMAND_COUNT + '},'
    + '()=>tools[command.name]({cmd:"true",yield_time_ms:1000}))); '
    + 'for(const result of results) { if(result.status!=="fulfilled" || result.value.exit_code!==0) '
    + 'throw new Error("original command failed"); } '
    + 'text({completedCommands:' + COMMAND_COUNT + ',holdMarker:String.fromCharCode(' + markerCodes + ')});'
  assert.equal(commandScript.includes(marker), false,
    'the final Provider hold marker is produced only after every original real command has completed')
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: [marker],
      nativeCellScripts: { 'native-cell-A': commandScript } },
    scenario: { async run({ api, modelRoute, devicePath }) {
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
      database.exec('PRAGMA busy_timeout=5000')
      let probe
      try {
        await api.command('session.create', 0, { productSessionId,
          projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
          title: 'Real bounded Worker Gap replay', modelRoute })
        await devicePath.launchAnchor({ productSessionId })
        const payload = { productSessionId, message: 'native-cell-A' }
        const submitted = await api.command('chat.submit', 1, payload, requestId)
        const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').all(requestId)
        assert.equal(jobs().length, 1)
        const [job] = jobs()
        await waitFor(() => devicePath.modelServer.requests.some(request => request.gateMarker === marker && request.active),
          'the final original real HTTPS model request is held after its actual command loop', 60_000)
        const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
        probe = readWorkerTransport(devicePath.deviceData, authority)
        // Install observation DDL while the actual Worker is stopped and its
        // journal has no held write transaction. Execution resumes immediately.
        await waitFor(() => {
          process.kill(probe.pid, 'SIGSTOP')
          paused = probe.pid
          const status = readFileSync(`/proc/${probe.pid}/status`, 'utf8')
          if (!/^State:\s+T/mu.test(status)) {
            process.kill(probe.pid, 'SIGCONT')
            paused = null
            return false
          }
          const checkpoint = new DatabaseSync(probe.networkJournalPath)
          try {
            checkpoint.exec('PRAGMA busy_timeout=0; BEGIN IMMEDIATE; COMMIT')
          } catch (error) {
            if (![5, 6].includes(error.errcode & 0xff)) throw error
            process.kill(probe.pid, 'SIGCONT')
            paused = null
            return false
          } finally { checkpoint.close() }
          return true
        }, 'the stopped original Worker has no held journal writer transaction before observation DDL', 3000, 10)
        stopObservation = observeCompletedEnvelopes(probe)
        process.kill(probe.pid, 'SIGCONT')
        paused = null
        await waitFor(() => {
          const rows = readRetainedRuntime(devicePath.deviceData, job.job_id)
          return rows.some(row => row.sequence <= row.ack_sequence) ? rows : false
        }, 'the same original execution has genuine previously acknowledged runtime history', 30_000)
        const run = coreRun(devicePath.deviceData, job.job_id)
        const originalCommands = readRetainedCoreCommandResults(devicePath.deviceData, run.kernelSessionId)
        assertSuccessfulRetainedCommands(originalCommands, COMMAND_COUNT)
        t.diagnostic(JSON.stringify({ cut: 'all-commands-completed-final-provider-open',
          realCommands: originalCommands.length, coreToolCursor: run.coreToolCursor,
          retainedRuntime: readRetainedRuntime(devicePath.deviceData, job.job_id).length }))
        const stream = 'product-session:' + productSessionId
        // Fault injection refuses real runtime writes. No runtime, receipt,
        // accounting, sequence, identity or ACK fact is inserted or edited.
        database.exec("CREATE TRIGGER pause_bounded_gap_insert BEFORE INSERT ON product_state WHEN NEW.stream_id='"
          + stream + "' BEGIN SELECT RAISE(ABORT,'hold real runtime commit'); END")
        database.exec("CREATE TRIGGER pause_bounded_gap_update BEFORE UPDATE ON product_state WHEN NEW.stream_id='"
          + stream + "' BEGIN SELECT RAISE(ABORT,'hold real runtime commit'); END")
        devicePath.modelServer.releaseResponseGate(marker)
        await waitFor(() => {
          const rows = readRetainedRuntime(devicePath.deviceData, job.job_id)
          return rows.filter(row => row.sequence > row.ack_sequence).length > 64 ? rows : false
        }, 'more than one production page of actual original Core runtime frames is retained without business ACK', 120_000)
        const completedBeforeGap = await waitFor(() => {
          const original = coreRun(devicePath.deviceData, job.job_id)
          const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
          const modelCalls = coreModelCalls(devicePath.deviceData, job.job_id)
          if (original.terminal?.kind !== 'completed' || calls.length === 0 || modelCalls.length === 0
            || calls.some(call => call.terminalType !== 'completed' || call.providerAttempts.length !== 1
              || call.providerAttempts[0].state !== 'completed' || call.providerAttempts[0].usage === null)
            || modelCalls.some(call => call.completed !== 1 || call.provider_final !== 1 || call.core_committed !== 1)
            || devicePath.modelServer.requests.some(request => request.active)) return false
          assert.deepEqual(modelCalls.map(call => call.model_exchange_id).sort(), calls.map(call => call.exchangeId).sort(),
            'every original completed Provider request belongs to the actual Core model call identity')
          return { run: original, calls, modelCalls }
        }, 'the actual original Core terminal and all nonempty physical Provider receipts exist before Gap injection', 120_000)
        const stopped = await waitFor(() => {
          const ledger = runtimeLedgerSnapshot(database, productSessionId)
          if (!ledger) return false
          const expected = decode(ledger.bytes).highestSequence + 1
          const rows = readRetainedRuntime(devicePath.deviceData, job.job_id)
          const first = rows.find(row => row.sequence === expected)
          if (!first) return false
          const transport = runtimeTransport(probe, [first])[0]
          if (!transport.state || transport.state.in_flight || transport.attempts.at(-1)?.outcome !== 'failed') return false
          process.kill(probe.pid, 'SIGSTOP')
          paused = probe.pid
          const status = execFileSync('ps', ['-p', String(probe.pid), '-o', 'stat='], { encoding: 'utf8' }).trim()
          const frozen = runtimeTransport(probe, [first])[0]
          if (!status.startsWith('T') || frozen.state.in_flight || frozen.attempts.at(-1)?.outcome !== 'failed') {
            process.kill(probe.pid, 'SIGCONT')
            paused = null
            return false
          }
          return { ledger, expected, rows: readRetainedRuntime(devicePath.deviceData, job.job_id) }
        }, 'the original Worker is stopped after its real failed runtime exchange has completely ended', 30_000, 10)
        const history = stopped.rows.filter(row => row.sequence < stopped.expected)
        const pending = stopped.rows.filter(row => row.sequence >= stopped.expected)
        assert.ok(history.length > 0)
        assert.ok(pending.length > 64)
        assert.ok(history.every(row => row.sequence <= row.ack_sequence))
        assert.ok(pending.every((row, index) => row.sequence === stopped.expected + index))
        const historyBefore = runtimeTransport(probe, history)
        assert.ok(historyBefore.every(row => row.attempts.at(-1)?.outcome === 'completed'))
        const pendingBefore = runtimeTransport(probe, pending)
        const callsBefore = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
        assert.ok(callsBefore.length > 0, 'the frozen original paid call vector is nonempty')
        assert.deepEqual(callsBefore, completedBeforeGap.calls)
        const modelCallsBefore = coreModelCalls(devicePath.deviceData, job.job_id)
        assert.ok(modelCallsBefore.length > 0, 'the frozen original Core model identity vector is nonempty')
        assert.deepEqual(modelCallsBefore, completedBeforeGap.modelCalls)
        assert.ok(modelCallsBefore.every(call => call.frames.length > 0))
        const physicalBefore = structuredClone(devicePath.modelServer.requests)
        assert.equal(physicalBefore.length, callsBefore.reduce((count, call) => count + call.providerAttempts.length, 0),
          'all real HTTPS requests correspond to the frozen original physical attempt identities')
        const later = pending.at(-1)
        database.exec('DROP TRIGGER pause_bounded_gap_insert')
        database.exec('DROP TRIGGER pause_bounded_gap_update')
        // Send one genuine later frame. Its real Gap delivery is left in the
        // Server queue. No missing frame or confirmation is sent by the probe.
        const gap = await probe.postRetainedMessage(later.bytes, { ca: readFileSync(join(directory, 'fixture-cert.pem')) })
        assert.equal(gap.status, 200)
        assert.equal(gap.acknowledgement?.kind, 'runtime.ack')
        assert.equal(gap.acknowledgement.status, 'gap')
        assert.equal(gap.acknowledgement.ackSequence, stopped.expected - 1)
        assert.equal(gap.acknowledgement.replayFromSequence, stopped.expected)
        assert.deepEqual(gap.acknowledgement.lease, later.message.lease)
        assert.deepEqual(gap.acknowledgement.sessionIdentity, later.message.sessionIdentity)
        assert.deepEqual(runtimeLedgerSnapshot(database, productSessionId), stopped.ledger)
        assert.equal(gapConfirmations(probe, gap.acknowledgement.messageId).length, 0)
        process.kill(probe.pid, 'SIGCONT')
        paused = null
        const confirmedGap = await waitFor(() => {
          const confirmations = gapConfirmations(probe, gap.acknowledgement.messageId)
          return confirmations.length > 0 ? confirmations : false
        }, 'the actual original Worker consumes and confirms its real Gap through its own authenticated HTTPS exchange', 60_000)
        const terminal = await waitFor(() => acceptedTerminal(database, devicePath.deviceData, job.job_id, authority),
          'the actual resumed Worker drains the original range and receives its normalized terminal business ACK', 120_000)
        const finalRows = readRetainedRuntime(devicePath.deviceData, job.job_id)
        assert.ok(finalRows.every(row => row.sequence <= row.ack_sequence))
        assert.equal(decode(runtimeLedgerSnapshot(database, productSessionId).bytes).highestSequence, finalRows.at(-1).sequence)
        for (const original of stopped.rows) {
          const retained = finalRows.find(row => row.sequence === original.sequence)
          assert.ok(retained)
          assert.equal(retained.event_id, original.event_id)
          assert.equal(retained.digest, original.digest)
          assert.deepEqual(retained.bytes, original.bytes)
        }
        assert.deepEqual(runtimeTransport(probe, history), historyBefore,
          'the real Worker does not retransmit the previously acknowledged original prefix')
        const pendingAfter = runtimeTransport(probe, pending)
        for (const original of pendingBefore) {
          const replayed = pendingAfter.find(row => row.sequence === original.sequence)
          assert.equal(replayed.originalBytesDigest, original.originalBytesDigest)
          assert.equal(replayed.wireBytes, original.wireBytes)
          assert.ok(replayed.attempts.length > original.attempts.length,
            'each requested original runtime frame is retransmitted by the actual Worker, including the originally out-of-order last frame')
          assert.equal(replayed.attempts.at(-1).outcome, 'completed')
          assert.ok(replayed.wireBytes <= 256 * 1024, 'each actual replayed HTTPS frame meets the canonical transport bound')
        }
        const completedRun = coreRun(devicePath.deviceData, job.job_id)
        assert.equal(completedRun.kernelSessionId, run.kernelSessionId)
        assert.equal(completedRun.canonicalThreadId, run.canonicalThreadId)
        assert.equal(completedRun.submissionId, run.submissionId)
        assert.deepEqual(completedRun.terminal, completedBeforeGap.run.terminal)
        assert.deepEqual(readRetainedCoreCommandResults(devicePath.deviceData, run.kernelSessionId), originalCommands)
        const events = readFileSync(completedRun.rolloutPath, 'utf8').trim().split('\n').map(JSON.parse)
          .filter(event => event.type === 'event_msg').map(event => event.payload)
        assert.deepEqual(events.filter(event => ['task_started', 'turn_started'].includes(event.type))
          .map(event => event.turn_id), [run.submissionId])
        assert.equal(events.filter(event => event.type === 'user_message'
          || (event.type === 'item_completed' && event.item?.type === 'UserMessage')).length, 1)
        assert.equal(jobs().length, 1)
        assert.equal(jobs()[0].attempt, 1)
        const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
        assert.ok(calls.length > 0)
        assert.deepEqual(calls, callsBefore, 'automatic Gap replay preserves every original paid request, physical attempt and usage receipt')
        assert.deepEqual(coreModelCalls(devicePath.deviceData, job.job_id), modelCallsBefore,
          'automatic Gap replay preserves the original Core model call identities and exact retained receipt digests')
        assert.deepEqual(devicePath.modelServer.requests, physicalBefore, 'automatic Gap replay starts no physical Provider HTTP request')
        assert.ok(calls.every(call => call.providerAttempts.length === 1 && call.providerAttempts[0].state === 'completed'))
        const accounting = await waitFor(() => physicalAccounting(database, job.job_id, calls, terminal.outcome.message.outcome.usage),
          'the signed original physical attempt manifest and precise raw usage settle without another paid call', 30_000)
        const physicalCount = devicePath.modelServer.requests.length
        const messages = (await api.query('session.messages.list', { productSessionId })).result.items
        assert.equal(messages.filter(message => message.role === 'assistant' && message.state === 'completed').length, 1)
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        assert.equal(devicePath.modelServer.requests.length, physicalCount)
        assert.deepEqual(readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id), calls)
        assert.deepEqual(readRetainedCoreCommandResults(devicePath.deviceData, run.kernelSessionId), originalCommands)
        return { jobId: job.job_id, workerInstanceId: probe.workerInstanceId, status: terminal.outcome.message.outcome.status,
          gapMessageId: gap.acknowledgement.messageId, originalAckedPrefix: history.length,
          originalReplayFrom: stopped.expected, originalReplayThrough: later.sequence,
          actualWorkerReplayedFrames: pending.length, actualGapConfirmingEnvelopes: confirmedGap.length,
          maxActualFrameBytes: Math.max(...pendingAfter.map(row => row.wireBytes)), realCommands: COMMAND_COUNT,
          immutableOriginalRange: true, previouslyAcknowledgedPrefixRetransmissions: 0,
          normalizedBusinessAcknowledged: true,
          originalPaidCalls: calls.length, originalPhysicalAttempts: physicalBefore.length,
          unchangedOriginalModelIdentities: true, physicalAccounting: accounting,
          batchObservation: 'HTTPS journal has no flush batch ID; count/bytes/time flush budgets require production source and load evidence' }
      } finally {
        database.exec('DROP TRIGGER IF EXISTS pause_bounded_gap_insert')
        database.exec('DROP TRIGGER IF EXISTS pause_bounded_gap_update')
        devicePath.modelServer.releaseResponseGate(marker)
        if (paused !== null) { process.kill(paused, 'SIGCONT'); paused = null }
        database.close()
      }
    } },
  })
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})
