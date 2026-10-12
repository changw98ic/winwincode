// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { DatabaseSync } from 'node:sqlite'
import { isDeepStrictEqual } from 'node:util'
import { crc32, inflateRawSync } from 'node:zlib'

import { runApiProductionVertical, waitForDeviceWorkerRegistered } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import { assertSuccessfulRetainedCommands, readRetainedCoreCommandResults,
  readRetainedOutcomes, readRetainedRuntime } from './fixtures/chat-execution-transport-probe.mjs'

const LEASE_SECONDS = 30
const CONTROL_DEADLINE_MS = 10_000
const COMMAND = "printf '%s\\n' 'auxiliary-once' >> CHAT-AUXILIARY-ONCE.txt"
const digest = bytes => `sha256:${createHash('sha256').update(bytes).digest('hex')}`

function sealedBinaries() {
  const installed = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  return installed === undefined ? { build: false } : { build: false,
    serverBinary: join(installed, 'winwincode-server'), wwcBinary: join(installed, 'wwc') }
}

function openDatabase(path, readOnly = true) {
  const database = new DatabaseSync(path, { readOnly })
  database.exec('PRAGMA busy_timeout=5000')
  return database
}

function leaseRecord(authority) {
  return Object.fromEntries(['job_id', 'lease_id', 'payload_digest', 'worker_id', 'worker_instance_id',
    'attempt', 'fencing_token', 'issued_at', 'expires_at'].map(field => [field, authority[field]]))
}

function acceptedLeasePeriod(database, authority, period) {
  assert.deepEqual(Object.keys(period).sort(), Object.keys(leaseRecord(authority)).sort())
  for (const field of Object.keys(period).filter(field => field !== 'expires_at')) assert.equal(period[field], authority[field])
  const rows = database.prepare(`SELECT response_json FROM execution_lease_request_receipts
    WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(authority.job_id, period.expires_at)
  assert.ok(rows.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, period)),
    'the complete current lease has an actual accepted claim or renewal receipt')
  assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(authority.job_id), authority)
  return period
}

function currentExecutionLease(database, authority) {
  const row = database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(authority.job_id)
  return row && acceptedLeasePeriod(database, authority, leaseRecord(row))
}

function retainedRun(worker, jobId) {
  const rows = worker.prepare('SELECT run_key,record_json FROM codex_run').all().flatMap(row => {
    const run = JSON.parse(Buffer.from(row.record_json).toString('utf8'))
    return run.job.jobId === jobId ? [{ key: row.run_key, run }] : []
  })
  assert.equal(rows.length, 1, 'the actual Worker retains one original Chat execution')
  return rows[0]
}

function coreIdentity(run) {
  assert.ok(run.rolloutPath, 'the original embedded Core retains its actual rollout')
  const events = readFileSync(run.rolloutPath, 'utf8').trim().split('\n')
    .map(line => JSON.parse(line)).filter(row => row.type === 'event_msg').map(row => row.payload)
  return { kernelSessionId: run.kernelSessionId, canonicalThreadId: run.canonicalThreadId,
    submissionId: run.submissionId, submissionDigest: run.submissionDigest,
    turns: events.filter(event => ['task_started', 'turn_started'].includes(event.type)).map(event => event.turn_id),
    users: events.filter(event => event.type === 'user_message'
      || (event.type === 'item_completed' && event.item?.type === 'UserMessage')).length }
}

function installAuxiliaryFailure(worker) {
  // Primary model/Observer performance rows carry usage accounting. Only
  // turn timing and the optional terminal projection reservation can fail here.
  worker.exec(`CREATE TABLE auxiliary_failure_observation (
      position INTEGER PRIMARY KEY AUTOINCREMENT, run_key TEXT NOT NULL,
      target TEXT NOT NULL, observed_at TEXT NOT NULL);
    CREATE TRIGGER refuse_auxiliary_turn_metric BEFORE INSERT ON performance_operation
      WHEN NEW.operation_kind='turn' AND NEW.completed=0
      BEGIN
        INSERT INTO auxiliary_failure_observation(run_key,target,observed_at)
          VALUES(NEW.run_key,'turn-metric',strftime('%Y-%m-%dT%H:%M:%fZ','now'));
        SELECT RAISE(FAIL,'injected auxiliary turn timing failure');
      END;
    CREATE TRIGGER refuse_auxiliary_projection_queue BEFORE INSERT ON performance_projection
      BEGIN
        INSERT INTO auxiliary_failure_observation(run_key,target,observed_at)
          VALUES(NEW.run_key,'projection-queue',strftime('%Y-%m-%dT%H:%M:%fZ','now'));
        SELECT RAISE(FAIL,'injected auxiliary projection queue failure');
      END;`)
}

function auxiliaryFailures(worker, runKey) {
  return worker.prepare(`SELECT position,run_key,target,observed_at
    FROM auxiliary_failure_observation WHERE run_key=? ORDER BY position`).all(runKey)
}

async function settled(context, database, item, state, startedAt) {
  const { api, devicePath } = context
  const result = await waitFor(async () => {
    assert.ok(performance.now() - startedAt < CONTROL_DEADLINE_MS,
      'auxiliary storage failure preserves the original ten-second control completion budget')
    const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(item.job.job_id)
    const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(item.job.job_id)
    const slot = database.prepare('SELECT * FROM worker_session_slots WHERE worker_session_id=?').get(item.authority.worker_session_id)
    const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(item.job.job_id)
    const accepted = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
      .get(`execution-outcome:${item.job.job_id}:1`)
    const raw = accepted && JSON.parse(Buffer.from(accepted.payload).toString('utf8'))
    const outcomes = readRetainedOutcomes(devicePath.deviceData, item.job.job_id)
    const messages = (await api.query('session.messages.list', { productSessionId: item.productSessionId })).result.items
    return job?.state === (state === 'cancelled' ? 'failed' : 'completed') && job.attempt === 1
      && terminal?.outcome === state && slot?.state === state
      && reservation?.state === (state === 'cancelled' ? 'released' : 'settled')
      && accepted?.revision === 2 && raw?.acknowledgementReady
      && outcomes.length === 1 && outcomes[0].workerAcknowledged
      && messages.some(message => message.role === 'assistant' && message.state === state)
      ? { job, terminal, slot, reservation, raw, worker: outcomes[0], messages } : false
  }, `the original ${state} Chat settles its exact Worker outcome and business ACK despite auxiliary queue failure`, CONTROL_DEADLINE_MS, 50)
  assert.ok(performance.now() - startedAt < CONTROL_DEADLINE_MS)
  assert.deepEqual(result.raw.message, result.worker.message)
  assert.equal(result.worker.message.outcome.status, state === 'completed' ? 'succeeded' : 'cancelled')
  assert.equal(result.worker.message.workerSessionId, item.authority.worker_session_id)
  assert.deepEqual(result.worker.message.sessionIdentity, { productSessionId: item.productSessionId,
    workerSessionId: item.authority.worker_session_id, codexThreadId: item.original.run.canonicalThreadId })
  const stamp = result.worker.message.lease
  acceptedLeasePeriod(database, item.authority, { job_id: stamp.jobId, lease_id: stamp.leaseId,
    payload_digest: item.authority.payload_digest, worker_id: stamp.workerId, worker_instance_id: stamp.workerInstanceId,
    attempt: stamp.attempt, fencing_token: stamp.fencingToken, issued_at: stamp.issuedAt, expires_at: stamp.expiresAt })
  for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token']) {
    assert.equal(result.terminal[field], item.authority[field])
    assert.equal(result.slot[field], item.authority[field])
  }
  assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(item.job.job_id).n, 1)
  assert.equal(database.prepare('SELECT COUNT(*) AS n FROM scheduler_execution_jobs WHERE product_session_id=?').get(item.productSessionId).n, 1)
  assert.equal(result.messages.filter(message => message.role === 'user').length, 1)
  assert.equal(result.messages.filter(message => message.role === 'assistant'
    && ['completed', 'failed', 'cancelled'].includes(message.state)).length, 1)
  assert.deepEqual(coreIdentity(retainedRun(item.worker, item.job.job_id).run), item.core)
  const failures = auxiliaryFailures(item.worker, item.original.key)
  assert.ok(failures.some(failure => failure.target === 'turn-metric'), 'the actual original Core turn attempted the failing auxiliary metric write')
  assert.ok(failures.some(failure => failure.target === 'projection-queue'), 'the actual original terminal attempted the failing auxiliary queue reservation')
  assert.equal(item.worker.prepare('SELECT COUNT(*) AS n FROM performance_projection WHERE run_key=?').get(item.original.key).n, 0)
  assert.equal(item.worker.prepare(`SELECT COUNT(*) AS n FROM sqlite_master WHERE type='trigger'
    AND name IN ('refuse_auxiliary_turn_metric','refuse_auxiliary_projection_queue')`).get().n, 2,
  'the real auxiliary failures remain enabled through the complete original outcome and ACK')
  return { ...result, failures, elapsedMs: performance.now() - startedAt }
}

function frozenZipFile(bytes, fileName) {
  const end = bytes.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]))
  assert.ok(end >= 0 && end + 22 <= bytes.length)
  assert.equal(end + 22 + bytes.readUInt16LE(end + 20), bytes.length)
  assert.equal(bytes.readUInt16LE(end + 4), 0)
  assert.equal(bytes.readUInt16LE(end + 6), 0)
  const count = bytes.readUInt16LE(end + 10)
  assert.equal(bytes.readUInt16LE(end + 8), count)
  let offset = bytes.readUInt32LE(end + 16)
  const entries = []
  for (let index = 0; index < count; index++) {
    assert.equal(bytes.readUInt32LE(offset), 0x02014b50)
    const length = bytes.readUInt16LE(offset + 28)
    const name = bytes.subarray(offset + 46, offset + 46 + length).toString('utf8')
    if (name === fileName) entries.push(offset)
    offset += 46 + length + bytes.readUInt16LE(offset + 30) + bytes.readUInt16LE(offset + 32)
  }
  assert.equal(offset, end)
  assert.equal(entries.length, 1, 'the original frozen project ZIP contains the exact side effect once')
  const entry = entries[0]
  assert.equal(bytes.readUInt16LE(entry + 8) & 1, 0)
  const method = bytes.readUInt16LE(entry + 10)
  assert.ok(method === 0 || method === 8)
  const local = bytes.readUInt32LE(entry + 42)
  assert.equal(bytes.readUInt32LE(local), 0x04034b50)
  const start = local + 30 + bytes.readUInt16LE(local + 26) + bytes.readUInt16LE(local + 28)
  const compressed = bytes.subarray(start, start + bytes.readUInt32LE(entry + 20))
  const result = method === 0 ? compressed : inflateRawSync(compressed, { maxOutputLength: 1024 * 1024 })
  assert.equal(result.length, bytes.readUInt32LE(entry + 24))
  assert.equal(crc32(result), bytes.readUInt32LE(entry + 16))
  return result
}

async function assertRequiredCandidate(api, item, terminal) {
  const candidates = item.worker.prepare(`SELECT u.record_json,c.bytes FROM candidate_artifact_upload u
    JOIN candidate_artifact_content c ON c.authority_key=u.authority_key`).all()
    .map(row => ({ record: JSON.parse(Buffer.from(row.record_json).toString('utf8')), bytes: Buffer.from(row.bytes) }))
    .filter(value => value.record.open_message.lease.jobId === item.job.job_id)
  assert.equal(candidates.length, 1, 'one original immutable candidate remains required')
  const { record, bytes } = candidates[0]
  assert.equal(record.descriptor.kind, 'candidate')
  assert.equal(record.descriptor.fileName, 'project.zip')
  assert.equal(record.descriptor.mediaType, 'application/zip')
  assert.equal(record.descriptor.digest, digest(bytes))
  assert.equal(record.descriptor.sizeBytes, bytes.length)
  assert.equal(record.final_ack?.status, 'accepted')
  assert.equal(record.final_ack.artifactId, record.descriptor.artifactId)
  assert.deepEqual(record.final_ack.lease, record.open_message.lease)
  assert.deepEqual(record.final_ack.sessionIdentity, record.open_message.sessionIdentity)
  assert.equal(record.ack_sequence, record.chunk_count)
  assert.equal(record.final_ack.ackSequence, record.chunk_count)
  assert.deepEqual(frozenZipFile(bytes, 'CHAT-AUXILIARY-ONCE.txt'), Buffer.from('auxiliary-once\n'))
  const assistant = terminal.messages.find(message => message.role === 'assistant' && message.state === 'completed')
  assert.deepEqual(assistant.artifactRefs, [{ artifactId: record.descriptor.artifactId, digest: record.descriptor.digest }])
  const downloaded = (await api.query('session.artifact.get', { productSessionId: item.productSessionId,
    artifactId: record.descriptor.artifactId, offset: 0, length: 262144 })).result
  assert.equal(downloaded.totalSize, bytes.length)
  assert.equal(downloaded.digest, record.descriptor.digest)
  assert.deepEqual(Buffer.from(downloaded.dataBase64, 'base64'), bytes)
  return { artifactId: record.descriptor.artifactId, digest: record.descriptor.digest }
}

test('Chat heartbeat, cancellation and completion stay timely while actual auxiliary turn timing and terminal projection queue writes fail', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-auxiliary-queue-'))
  let complete = false
  t.after(() => {
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed Chat auxiliary queue evidence retained at ${directory}`)
  })
  const cases = [
    { productSessionId: 'psn_01J00000000000000000000151', requestId: 'req_01J00000000000000000000151', marker: 'auxiliary-complete-hold', outcome: 'completed' },
    { productSessionId: 'psn_01J00000000000000000000152', requestId: 'req_01J00000000000000000000152', marker: 'auxiliary-cancel-hold', outcome: 'cancelled' },
  ]
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: String(LEASE_SECONDS) },
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: cases.map(item => item.marker),
      nativeCellScripts: { 'native-cell-A': `const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));
        if(!command) throw new Error("command tool missing");
        const result=await tools[command.name]({cmd:${JSON.stringify(COMMAND)},yield_time_ms:1000});
        if(result.exit_code!==0) throw new Error("original auxiliary isolation command failed"); text(result);` } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      const database = openDatabase(join(directory, 'server-data/control-plane.sqlite3'))
      const device = openDatabase(join(devicePath.deviceData, 'device-client.sqlite3'))
      const items = []
      try {
        for (const item of cases) {
          await api.command('session.create', 0, { productSessionId: item.productSessionId,
            projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
            title: `Auxiliary queue ${item.outcome}`, modelRoute: context.modelRoute })
          const launched = await devicePath.launchAnchor({ productSessionId: item.productSessionId })
          await waitForDeviceWorkerRegistered(api, launched, 60_000)
          const process = device.prepare('SELECT * FROM worker_process_registry WHERE worker_id=? AND worker_instance_id=?')
            .get(launched.workerId, launched.workerInstanceId)
          assert.equal(process.state, 'running')
          assert.equal(process.worker_session_id, launched.workerSessionId)
          assert.ok(process.data_directory.startsWith(`${devicePath.deviceData}/`))
          const path = join(process.data_directory, 'codex-runtime/worker-codex.sqlite3')
          await waitFor(() => existsSync(path), 'the actual Device Worker Adapter store exists before public submit', 30_000)
          const worker = openDatabase(path, false)
          const current = { ...item, worker, process }
          items.push(current)
          installAuxiliaryFailure(worker)
          const payload = { productSessionId: item.productSessionId, message: `native-cell-A ${item.marker}` }
          current.payload = payload
          current.receipt = await api.command('chat.submit', 1, payload, item.requestId)
          current.job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').get(item.requestId)
          await waitFor(() => devicePath.modelServer.requests.some(request => request.gateMarker === item.marker && request.active),
            'the original real Chat Core has an actual held HTTPS invocation', 60_000)
          current.authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(current.job.job_id)
          assert.equal(current.authority.worker_id, process.worker_id)
          assert.equal(current.authority.worker_instance_id, process.worker_instance_id)
          assert.equal(current.authority.attempt, 1)
          const claimed = leaseRecord(current.authority)
          acceptedLeasePeriod(database, current.authority, claimed)
          assert.equal(Date.parse(claimed.expires_at) - Date.parse(claimed.issued_at), LEASE_SECONDS * 1000)
          current.observed = [{ expiresAt: claimed.expires_at, observedAt: Date.now() }]
          current.original = retainedRun(worker, current.job.job_id)
          current.core = coreIdentity(current.original.run)
          assert.equal(current.core.users, 1)
          assert.deepEqual(current.core.turns, [current.core.submissionId])
          assert.ok(auxiliaryFailures(worker, current.original.key).some(failure => failure.target === 'turn-metric'),
            'an actual failing auxiliary write is observed before heartbeat measurement')
        }
        assert.equal(devicePath.modelServer.activeRequests, 2)
        await waitFor(() => {
          let crossed = true
          for (const item of items) {
            const current = currentExecutionLease(database, item.authority)
            assert.ok(current)
            const previous = item.observed.at(-1)
            if (current.expires_at !== previous.expiresAt) {
              assert.ok(Date.now() < Date.parse(previous.expiresAt), 'the real heartbeat renews before the prior accepted deadline')
              assert.ok(Date.parse(current.expires_at) > Date.parse(previous.expiresAt))
              item.observed.push({ expiresAt: current.expires_at, observedAt: Date.now() })
            }
            crossed &&= Date.now() > Date.parse(item.authority.expires_at) + 500 && item.observed.length >= 3
              && Date.now() < Date.parse(current.expires_at)
          }
          return crossed
        }, 'both real Chat executions cross the original thirty-second deadline under observed auxiliary write failure', 60_000, 100)
        const cancelled = items.find(item => item.outcome === 'cancelled')
        const session = (await api.query('session.get', { productSessionId: cancelled.productSessionId })).result
        const cancelPayload = { productSessionId: cancelled.productSessionId, reason: 'Cancel the original real Chat while auxiliary queue writes fail' }
        const cancelRequestId = 'req_01J00000000000000000000153'
        const cancelStarted = performance.now()
        const cancelReceipt = await api.command('session.cancel', session.revision, cancelPayload, cancelRequestId)
        const cancelTerminal = await settled(context, database, cancelled, 'cancelled', cancelStarted)
        assert.equal(cancelTerminal.reservation.release_reason, 'cancelled')
        assert.equal(cancelTerminal.reservation.actual_tokens, null)
        assert.equal(cancelTerminal.reservation.actual_cost_microunits, null)
        assert.equal(cancelTerminal.raw.message.outcome.usage.accountingStatus, 'unknown')
        assert.equal(cancelTerminal.raw.message.outcome.usage.tokens ?? null, null)
        assert.equal(cancelTerminal.raw.message.outcome.usage.costMicrounits ?? null, null)
        assert.equal(cancelTerminal.raw.message.outcome.usage.knownTokens, 0)
        assert.equal(readRetainedCoreCommandResults(devicePath.deviceData, cancelled.core.kernelSessionId).length, 0)
        const cancelledHttpCount = devicePath.modelServer.requests.filter(request => request.gateMarker === cancelled.marker).length
        assert.equal(cancelledHttpCount, 1)
        devicePath.modelServer.releaseResponseGate(cancelled.marker)
        await waitFor(() => !devicePath.modelServer.requests.some(request => request.gateMarker === cancelled.marker && request.active),
          'the original cancelled HTTPS invocation releases its actual Device slot', CONTROL_DEADLINE_MS)

        const completed = items.find(item => item.outcome === 'completed')
        const completeStarted = performance.now()
        devicePath.modelServer.releaseResponseGate(completed.marker)
        const completeTerminal = await settled(context, database, completed, 'completed', completeStarted)
        const commands = readRetainedCoreCommandResults(devicePath.deviceData, completed.core.kernelSessionId)
        assertSuccessfulRetainedCommands(commands, 1)
        assert.equal(commands[0].delivery, 'offered')
        for (const field of ['execution_result', 'accepted_result']) {
          assert.equal(JSON.parse(JSON.parse(commands[0][field]).effective_input.input).cmd, COMMAND)
        }
        const facts = readRetainedRuntime(devicePath.deviceData, completed.job.job_id)
          .filter(value => value.message.event.payload?.contentType === 'application/vnd.winwincode.core-tool-fact+json')
        assert.ok(facts.length > 0)
        assert.ok(facts.every(value => value.sequence <= value.ack_sequence), 'the real original tool facts retain their exact business ACK watermarks')
        const candidate = await assertRequiredCandidate(api, completed, completeTerminal)
        const calls = readDeviceExecutionReceipts(directory).calls
        const measured = calls.filter(call => call.jobId === completed.job.job_id)
        assert.ok(measured.length >= 2)
        assert.ok(measured.every(call => call.providerAttempts.length === 1
          && call.providerAttempts[0].state === 'completed' && call.providerAttempts[0].usage !== null))
        const tokens = measured.reduce((total, call) => total + call.providerAttempts[0].usage.totalTokens, 0)
        assert.equal(completeTerminal.raw.message.outcome.usage.accountingStatus, 'known')
        assert.equal(completeTerminal.raw.message.outcome.usage.tokens, tokens)
        assert.equal(completeTerminal.raw.message.outcome.usage.knownTokens, tokens)
        assert.equal(completeTerminal.reservation.actual_tokens, tokens)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_admission_settlement_sources WHERE job_id=?')
          .get(completed.job.job_id).n, 1)
        const cancelledCalls = calls.filter(call => call.jobId === cancelled.job.job_id)
        assert.equal(cancelledCalls.length, 1)
        assert.equal(cancelledCalls[0].providerAttempts.length, 1)
        assert.equal(cancelledCalls[0].providerAttempts[0].usage, null)
        assert.notEqual(cancelledCalls[0].providerAttempts[0].state, 'completed')
        const httpCount = devicePath.modelServer.requests.length
        assert.deepEqual((await api.command('session.cancel', session.revision, cancelPayload, cancelRequestId)).result, cancelReceipt.result)
        for (const item of items) {
          assert.deepEqual((await api.command('chat.submit', 1, item.payload, item.requestId)).result, item.receipt.result)
          assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(item.job.job_id), item.authority)
          assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(item.job.job_id).n, 1)
        }
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        assert.equal(devicePath.modelServer.requests.filter(request => request.gateMarker === cancelled.marker).length, cancelledHttpCount)
        assert.equal(readRetainedCoreCommandResults(devicePath.deviceData, cancelled.core.kernelSessionId).length, 0)
        return { cases: items.map(item => ({ jobId: item.job.job_id, outcome: item.outcome,
          originalDeadline: item.authority.expires_at, heartbeatRenewals: item.observed.length - 1,
          auxiliaryFailureCount: auxiliaryFailures(item.worker, item.original.key).length,
          auxiliaryFailureDigest: digest(JSON.stringify(auxiliaryFailures(item.worker, item.original.key))) })),
        leaseSeconds: LEASE_SECONDS, controlDeadlineMs: CONTROL_DEADLINE_MS,
        cancelElapsedMs: cancelTerminal.elapsedMs, completeElapsedMs: completeTerminal.elapsedMs,
        completedCommands: commands.length, measuredTokens: tokens, candidate, businessAcknowledged: true }
      } finally {
        for (const item of items) {
          devicePath.modelServer.releaseResponseGate(item.marker)
          item.worker.exec('DROP TRIGGER IF EXISTS refuse_auxiliary_turn_metric; DROP TRIGGER IF EXISTS refuse_auxiliary_projection_queue')
          item.worker.close()
        }
        device.close()
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.businessAcknowledged, true)
  assert.equal(report.flow.scenario.completedCommands, 1)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})
