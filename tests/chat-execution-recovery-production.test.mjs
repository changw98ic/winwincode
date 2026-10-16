// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { globSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { isDeepStrictEqual } from 'node:util'
import { crc32, inflateRawSync } from 'node:zlib'

import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import { assertSuccessfulRetainedCommands, completedCommandScript, readRetainedCoreCommandResults,
  readRetainedOutcomes, readWorkerTransport } from './fixtures/chat-execution-transport-probe.mjs'

function sealedBinaries() {
  const installed = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  return installed === undefined ? { build: false } : {
    build: false,
    serverBinary: join(installed, 'winwincode-server'),
    wwcBinary: join(installed, 'wwc'),
  }
}

function retainedTerminal(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const databasePath = join(deviceData, path)
    const store = new DatabaseSync(databasePath, { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='execution_terminal_outcome'").get()) return []
      return store.prepare(`SELECT r.run_key, r.record_json, t.frame_digest, t.frame_json
        FROM codex_run r JOIN execution_terminal_outcome t ON t.run_key=r.run_key`).all().flatMap(row => {
        const run = JSON.parse(Buffer.from(row.record_json).toString('utf8'))
        if (run.job.jobId !== jobId) return []
        const bytes = Buffer.from(row.frame_json)
        const frame = JSON.parse(bytes.toString('utf8'))
        const delivery = store.prepare(`SELECT delivery_id, correlation_key, state, acknowledgement_required
          FROM execution_outbox WHERE family='outcome' AND delivery_id=?`).get(frame.messageId)
        const receipt = delivery ? null : store.prepare(`SELECT response_digest FROM execution_response_receipt
          WHERE family='outcome'`).all()
        return [{ databasePath, runKey: row.run_key, run, frame, bytes, digest: row.frame_digest, delivery, receipt }]
      })
    } finally { store.close() }
  })
}

function coreExecution(run, deviceData) {
  assert.ok(run.rolloutPath, 'the real Core rollout is retained')
  const events = readFileSync(run.rolloutPath, 'utf8').trim().split('\n')
    .map(line => JSON.parse(line)).filter(item => item.type === 'event_msg').map(item => item.payload)
  return {
    submissionId: run.submissionId,
    submissionDigest: run.submissionDigest,
    kernelSessionId: run.kernelSessionId,
    codexThreadId: run.canonicalThreadId,
    turns: events.filter(event => ['task_started', 'turn_started'].includes(event.type)).map(event => event.turn_id),
    users: events.filter(event => event.type === 'user_message'
      || (event.type === 'item_completed' && event.item?.type === 'UserMessage')).length,
    commands: readRetainedCoreCommandResults(deviceData, run.kernelSessionId).map(row => {
      const request = JSON.parse(row.request_json)
      return {
        requestSequence: row.sequence, logicalId: request.logical_id, turnId: request.turn_id,
        attemptId: row.attempt_id, operationDigest: row.operation_digest,
        resolution: row.resolution, execution: row.execution,
        disposition: row.disposition, delivery: row.delivery,
        executionResult: row.execution_result, acceptedResult: row.accepted_result,
        polls: row.polls,
      }
    }),
  }
}

function assertSuccessfulCoreCommand(command) {
  assertSuccessfulRetainedCommands([{ execution: command.execution, disposition: command.disposition,
    execution_result: command.executionResult, accepted_result: command.acceptedResult, polls: command.polls }], 1)
}

function leaseRecord(authority) {
  return Object.fromEntries(['job_id', 'lease_id', 'payload_digest', 'worker_id', 'worker_instance_id',
    'attempt', 'fencing_token', 'issued_at', 'expires_at'].map(field => [field, authority[field]]))
}

function assertAcceptedLeasePeriod(database, period) {
  const receipts = database.prepare(`SELECT response_json FROM execution_lease_request_receipts
    WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(period.job_id, period.expires_at)
  assert.ok(receipts.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, period)),
    'the exact immutable lease period has a real accepted claim or renewal receipt')
}

function currentExecutionLease(database, authority, stamp) {
  const current = database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(authority.job_id)
  if (!current) return false
  const original = leaseRecord(authority)
  for (const field of Object.keys(original).filter(field => field !== 'expires_at')) assert.equal(current[field], original[field])
  assertAcceptedLeasePeriod(database, original)
  assertAcceptedLeasePeriod(database, leaseRecord(current))
  if (stamp) {
    const period = { job_id: stamp.jobId, lease_id: stamp.leaseId, payload_digest: authority.payload_digest,
      worker_id: stamp.workerId, worker_instance_id: stamp.workerInstanceId, attempt: stamp.attempt,
      fencing_token: stamp.fencingToken, issued_at: stamp.issuedAt, expires_at: stamp.expiresAt }
    for (const field of Object.keys(period).filter(field => field !== 'expires_at')) assert.equal(period[field], original[field])
    assertAcceptedLeasePeriod(database, period)
    assert.ok(Date.parse(current.expires_at) >= Date.parse(period.expires_at))
  }
  assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(authority.job_id), authority,
    'real heartbeat renewal preserves the immutable dispatch authority')
  return current
}

function settlementSources(database, jobId) {
  return database.prepare('SELECT * FROM execution_admission_settlement_sources WHERE job_id=?')
    .all(jobId)
}

function acceptedOutcome(database, jobId, attempt = 1) {
  const row = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
    .get(`execution-outcome:${jobId}:${attempt}`)
  return row ? { revision: row.revision, payload: JSON.parse(Buffer.from(row.payload).toString('utf8')) } : null
}

function retainedRuns(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const databasePath = join(deviceData, path)
    const store = new DatabaseSync(databasePath, { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) return []
      return store.prepare('SELECT run_key,record_json FROM codex_run').all().flatMap(row => {
        const bytes = Buffer.from(row.record_json)
        const run = JSON.parse(bytes.toString('utf8'))
        if (run.job.jobId !== jobId) return []
        const requests = store.prepare("SELECT frame_json FROM execution_outbox WHERE family='model_open'")
          .all().map(frame => JSON.parse(Buffer.from(frame.frame_json).toString('utf8')))
          .filter(frame => frame.lease.jobId === jobId && frame.lease.attempt === run.job.attempt)
        return [{ databasePath, runKey: row.run_key, bytes, run, requests }]
      })
    } finally { store.close() }
  })
}

function providerExchangeFacts(deviceData, exchangeId) {
  const paths = globSync('**/provider-runtime.sqlite3', { cwd: deviceData })
  assert.ok(paths.length > 0, 'the real Device Provider store exists')
  return paths.flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      const exchange = store.prepare('SELECT * FROM exchanges WHERE exchange_id=?').get(exchangeId)
      const attempts = store.prepare('SELECT * FROM model_invocation_attempts WHERE exchange_id=? ORDER BY attempt_number')
        .all(exchangeId)
      return exchange || attempts.length > 0 ? [{ path, exchange, attempts }] : []
    } finally { store.close() }
  })
}

function privateInputCalls(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const databasePath = join(deviceData, path)
    const store = new DatabaseSync(databasePath, { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='input_call'").get()) return []
      return store.prepare(`SELECT i.run_key,i.request_digest,i.record_json,r.record_json AS run_json
        FROM input_call i JOIN codex_run r ON r.run_key=i.run_key`).all().flatMap(row => {
        const run = JSON.parse(Buffer.from(row.run_json).toString('utf8'))
        if (run.job.jobId !== jobId) return []
        const bytes = Buffer.from(row.record_json)
        return [{ databasePath, runKey: row.run_key, requestDigest: row.request_digest, bytes,
          call: JSON.parse(bytes.toString('utf8')) }]
      })
    } finally { store.close() }
  })
}

function privateApprovalOperations(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='approval_operation'").get()) return []
      return store.prepare(`SELECT a.*,r.record_json AS run_json FROM approval_operation a
        JOIN codex_run r ON r.run_key=a.run_key`).all().flatMap(row => {
        const run = JSON.parse(Buffer.from(row.run_json).toString('utf8'))
        if (run.job.jobId !== jobId) return []
        const { run_json, ...operation } = row
        return [operation]
      })
    } finally { store.close() }
  })
}

function actualWorkerProcess(device, authority, { terminalAcknowledged = false } = {}) {
  const rows = device.prepare('SELECT * FROM worker_process_registry WHERE worker_id=? AND worker_instance_id=?')
    .all(authority.worker_id, authority.worker_instance_id)
  assert.equal(rows.length, 1, 'one actual Device process has this exact Worker and instance identity')
  if (terminalAcknowledged && rows[0].state === 'exited') {
    assert.equal(rows[0].exit_code, 0, 'the original managed Worker exits successfully after its exact business ACK')
  } else {
    assert.equal(rows[0].state, 'running')
  }
  return rows[0]
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
  assert.equal(entries.length, 1, 'the frozen project ZIP contains one exact expected file')
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

function assertFrozenChatFile(deviceData, jobId, fileName, content) {
  const candidates = globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='candidate_artifact_content'").get()) return []
      return store.prepare(`SELECT u.record_json,c.bytes FROM candidate_artifact_upload u
        JOIN candidate_artifact_content c ON c.authority_key=u.authority_key`).all()
        .map(row => ({ record: JSON.parse(Buffer.from(row.record_json).toString('utf8')), bytes: Buffer.from(row.bytes) }))
        .filter(value => value.record.open_message.lease.jobId === jobId)
    } finally { store.close() }
  })
  assert.equal(candidates.length, 1, 'the original Job retains one frozen candidate ZIP')
  const { record, bytes } = candidates[0]
  assert.equal(record.descriptor.fileName, 'project.zip')
  assert.equal(record.descriptor.mediaType, 'application/zip')
  assert.equal(record.descriptor.digest, `sha256:${createHash('sha256').update(bytes).digest('hex')}`)
  assert.equal(record.final_ack?.status, 'accepted')
  assert.equal(record.final_ack.artifactId, record.descriptor.artifactId)
  assert.equal(record.ack_sequence, record.chunk_count)
  assert.equal(record.final_ack.ackSequence, record.chunk_count)
  assert.deepEqual(frozenZipFile(bytes, fileName), Buffer.from(content))
}

function retainedInteractionRequests(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='execution_outbox'").get()) return []
      return store.prepare(`SELECT delivery_id,frame_json,frame_digest FROM execution_outbox
        WHERE family IN ('input_request','approval_request') ORDER BY position`).all().flatMap(row => {
        const bytes = Buffer.from(row.frame_json)
        const message = JSON.parse(bytes.toString('utf8'))
        if (message.lease.jobId !== jobId) return []
        assert.equal(message.schemaVersion, 'winwincode/v1')
        assert.equal(row.delivery_id, message.messageId)
        assert.equal(row.frame_digest, `sha256:${createHash('sha256').update(bytes).digest('hex')}`)
        return [{ bytes, digest: row.frame_digest, message }]
      })
    } finally { store.close() }
  })
}

function coreInputRequests(deviceData, kernelSessionId) {
  return globSync('**/state_5.sqlite', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      return store.prepare('SELECT request_json FROM tool_requests WHERE thread_id=? ORDER BY sequence').all(kernelSessionId)
        .map(row => JSON.parse(row.request_json)).filter(request => request.tool_name.endsWith('request_user_input'))
    } finally { store.close() }
  })
}

function assertOriginalInteraction(request, authority, run, productSessionId) {
  assert.equal(request.workerSessionId, authority.worker_session_id)
  assert.deepEqual(request.sessionIdentity, { productSessionId,
    workerSessionId: authority.worker_session_id, codexThreadId: run.canonicalThreadId })
  assert.equal(request.lease.jobId, authority.job_id)
  assert.equal(request.lease.attempt, 1)
}

async function renewOriginalInteraction(database, authority, request) {
  const previous = currentExecutionLease(database, authority, request.lease)
  const expiry = Date.parse(request.expiresAt)
  const renewed = await waitFor(() => {
    assert.ok(Date.now() < expiry, 'success uses the original unexpired interaction deadline')
    const current = currentExecutionLease(database, authority, request.lease)
    return Date.parse(current.expires_at) > Date.parse(previous.expires_at) ? current : false
  }, 'a real accepted heartbeat renews the original interaction execution before its unchanged deadline', 30_000, 50)
  assert.ok(Date.parse(renewed.expires_at) > Date.parse(request.lease.expiresAt))
  return renewed
}

function assertExactInteractionOutcome(database, deviceData, jobId, authority, raw) {
  const outcomes = readRetainedOutcomes(deviceData, jobId)
  assert.equal(outcomes.length, 1)
  const [outcome] = outcomes
  assert.equal(outcome.workerAcknowledged, true, 'the exact normalized original outcome ACK exists and its outbox row is consumed')
  assert.equal(outcome.message.outcome.status, 'succeeded')
  assert.deepEqual(outcome.message, raw)
  currentExecutionLease(database, authority, outcome.message.lease)
  assert.equal(database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId).attempt, 1)
  assert.equal(database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId).state, 'completed')
  assert.equal(database.prepare('SELECT * FROM worker_session_slots WHERE worker_session_id=?').get(authority.worker_session_id).state, 'completed')
  assert.equal(settlementSources(database, jobId).length, 1)
}

test('Chat replaces a crashed real Worker and embedded Core through trusted Device recovery while three Provider slots remain occupied', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-worker-core-recovery-'))
  let complete = false
  t.after(() => {
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed Chat Worker/Core recovery retained at ${directory}`)
  })
  const sessionIds = ['psn_01J00000000000000000000091', 'psn_01J00000000000000000000092',
    'psn_01J00000000000000000000093', 'psn_01J00000000000000000000094']
  const requestIds = ['req_01J00000000000000000000091', 'req_01J00000000000000000000092',
    'req_01J00000000000000000000093', 'req_01J00000000000000000000094']
  const markers = ['worker-recovery-first', 'worker-recovery-second', 'worker-recovery-third',
    'worker-recovery-slot-waiter']
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: markers, nativeCellScripts: {
      'native-cell-A': 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command tool missing"); text(await tools[command.name]({cmd:"printf \'once\\n\' >> CHAT-RECOVERY-ONCE.txt",yield_time_ms:1000}));',
    } },
    scenario: { async run({ api, modelRoute, devicePath }) {
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      const device = new DatabaseSync(join(devicePath.deviceData, 'device-client.sqlite3'), { readOnly: true })
      database.exec('PRAGMA busy_timeout=5000')
      device.exec('PRAGMA busy_timeout=5000')
      const server = devicePath.modelServer
      const submitted = []
      const jobs = index => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?')
        .all(requestIds[index])
      try {
        for (let index = 0; index < sessionIds.length; index++) {
          await api.command('session.create', 0, { productSessionId: sessionIds[index],
            projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
            title: `Worker Core recovery ${index}`, modelRoute })
          await devicePath.launchAnchor({ productSessionId: sessionIds[index] })
          const payload = { productSessionId: sessionIds[index],
            message: index === 3 ? `native-cell-A ${markers[index]}` : markers[index] }
          const receipt = await api.command('chat.submit', 1, payload, requestIds[index])
          const [job] = jobs(index)
          assert.equal(jobs(index).length, 1)
          submitted[index] = { payload, receipt, job }
          if (index < 3) await waitFor(() => server.requests.some(request => request.gateMarker === markers[index]
            && request.active), 'the original real HTTPS request owns its physical Provider slot', 60_000)
        }
        assert.equal(server.activeRequests, 3)
        assert.equal(server.peakActiveRequests, 3)
        const originalJob = submitted[3].job
        const original = await waitFor(() => retainedRuns(devicePath.deviceData, originalJob.job_id)
          .find(value => value.run.job.attempt === 1 && value.run.phase === 'runtime_started'
            && value.requests.length === 1), 'the fourth real Core ModelOpen waits durably before physical invocation', 60_000)
        const oldCore = coreExecution(original.run, devicePath.deviceData)
        assert.deepEqual(oldCore.turns, [oldCore.submissionId])
        assert.equal(oldCore.users, 1, 'the old Core incarnation accepted the original turn exactly once')
        assert.equal(oldCore.commands.length, 0, 'the gated old Core incarnation has performed no shell side effect')
        assert.equal(server.requests.filter(request => request.gateMarker === markers[3]).length, 0)
        const oldExchangeId = original.requests[0].modelExchangeId
        assert.deepEqual(providerExchangeFacts(devicePath.deviceData, oldExchangeId), [],
          'the Deferred slot waiter has no Device Provider exchange or physical start fact')
        const oldAuthority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?')
          .get(originalJob.job_id)
        const oldFacts = database.prepare('SELECT * FROM device_execution_current_facts WHERE job_id=?')
          .get(originalJob.job_id)
        const process = actualWorkerProcess(device, oldAuthority)
        assert.equal(process.state, 'running')
        assert.equal(process.worker_id, oldAuthority.worker_id)
        assert.equal(process.worker_instance_id, oldAuthority.worker_instance_id)
        assert.ok(process.data_directory.startsWith(`${devicePath.deviceData}/`), 'only this fixture owns the target process')
        assert.equal(Number(execFileSync('ps', ['-p', String(process.pid), '-o', 'pgid='], { encoding: 'utf8' }).trim()), process.pid,
          'the Device-supervised Worker owns the process group which contains its embedded Core')
        const killedAt = Date.now()
        globalThis.process.kill(-process.pid, 'SIGKILL')
        const replacement = await waitFor(() => {
          const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?')
            .get(originalJob.job_id)
          const current = jobs(3)[0]
          const run = retainedRuns(devicePath.deviceData, originalJob.job_id)
            .find(value => value.run.job.attempt === 2 && value.run.phase === 'runtime_started'
              && value.requests.length === 1)
          return current?.attempt === 2 && authority?.attempt === 2 && run ? { authority, current, run } : false
        }, 'the official trusted Device exit and successor launch replace the dead Worker/Core attempt', 120_000)
        assert.equal(jobs(3).length, 1, 'replacement retains the original Chat Job and command intent')
        assert.equal(replacement.current.submission_request_id, originalJob.submission_request_id)
        assert.notEqual(replacement.authority.worker_instance_id, oldAuthority.worker_instance_id)
        assert.notEqual(replacement.authority.worker_session_id, oldAuthority.worker_session_id)
        assert.equal(replacement.authority.worker_id, oldAuthority.worker_id)
        assert.ok(BigInt(replacement.authority.fencing_token) > BigInt(oldAuthority.fencing_token))
        assert.notEqual(replacement.run.run.canonicalThreadId, original.run.canonicalThreadId,
          'the new attempt has its own Core thread, as required by the execution contract')
        const successor = database.prepare(`SELECT r.successor_grant_id,g.worker_instance_id,g.worker_session_id
          FROM worker_replacement_launches r JOIN worker_launch_grants g ON g.worker_launch_grant_id=r.successor_grant_id
          WHERE r.predecessor_grant_id=?`).get(oldFacts.worker_launch_grant_id)
        assert.equal(successor.worker_instance_id, replacement.authority.worker_instance_id)
        const replacementProcess = actualWorkerProcess(device, replacement.authority)
        assert.equal(successor.worker_session_id, replacementProcess.worker_session_id,
          'the successor grant names its actual Device launch session')
        assert.ok(database.prepare('SELECT 1 FROM worker_launch_grant_exits WHERE worker_launch_grant_id=?')
          .get(oldFacts.worker_launch_grant_id), 'a real Device process exit authorizes this successor')
        assert.ok(database.prepare(`SELECT 1 FROM device_execution_recovery_intents
          WHERE job_id=? AND predecessor_attempt=1 AND successor_grant_id=? AND applied_at IS NOT NULL`)
          .get(originalJob.job_id, successor.successor_grant_id))
        assert.ok(Date.now() >= Date.parse(oldAuthority.expires_at), 'the replacement cannot take over an unexpired original lease')
        const predecessorPending = database.prepare(`SELECT * FROM execution_attempt_accounting_pending
          WHERE job_id=? AND attempt=1`).get(originalJob.job_id)
        assert.ok(predecessorPending, 'official recovery retains unresolved predecessor accounting')
        const predecessorLease = JSON.parse(predecessorPending.lease_json)
        assert.deepEqual(Object.keys(predecessorLease).sort(), Object.keys(leaseRecord(oldAuthority)).sort(),
          'unresolved accounting retains the complete nine-field storage lease')
        for (const field of Object.keys(predecessorLease).filter(field => field !== 'expires_at')) {
          assert.equal(predecessorLease[field], oldAuthority[field],
            `the unresolved predecessor retains its original ${field}`)
        }
        for (const [stored, transport] of [['job_id', 'jobId'], ['lease_id', 'leaseId'], ['worker_id', 'workerId'],
          ['worker_instance_id', 'workerInstanceId'], ['attempt', 'attempt'], ['fencing_token', 'fencingToken'], ['issued_at', 'issuedAt']]) {
          assert.equal(predecessorLease[stored], original.requests[0].lease[transport])
        }
        assertAcceptedLeasePeriod(database, predecessorLease)
        assert.ok(Date.parse(predecessorLease.expires_at) >= Date.parse(original.requests[0].lease.expiresAt),
          'the retained predecessor deadline comes from its real accepted lease period')
        const oldAfterReplacement = retainedRuns(devicePath.deviceData, originalJob.job_id)
          .find(value => value.run.job.attempt === 1)
        assert.deepEqual(oldAfterReplacement.bytes, original.bytes, 'the dead attempt remains zero-write')
        assert.deepEqual(coreExecution(oldAfterReplacement.run, devicePath.deviceData), oldCore)
        assert.equal(acceptedOutcome(database, originalJob.job_id, 1), null)
        assert.equal(server.activeRequests, 3)
        assert.equal(server.requests.filter(request => request.gateMarker === markers[3]).length, 0,
          'a restored execution must reacquire a real slot before it can invoke HTTP')
        server.releaseResponseGate(markers[0])
        await waitFor(() => server.requests.some(request => request.gateMarker === markers[3] && request.active),
          'the replacement Core ModelOpen invokes only after a real Provider slot is released', 60_000)
        assert.ok(server.activeRequests <= 3)
        assert.equal(server.peakActiveRequests, 3)
        for (const marker of markers) server.releaseResponseGate(marker)
        const settled = await waitFor(async () => {
          const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=? AND attempt=2')
            .get(originalJob.job_id)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?')
            .get(originalJob.job_id)
          const accepted = acceptedOutcome(database, originalJob.job_id, 2)
          const reconciliation = database.prepare(`SELECT * FROM execution_admission_reconciliation WHERE job_id=?`)
            .get(originalJob.job_id)
          const [worker] = retainedTerminal(devicePath.deviceData, originalJob.job_id)
            .filter(value => value.run.job.attempt === 2)
          const messages = (await api.query('session.messages.list', { productSessionId: sessionIds[3] })).result.items
          return jobs(3)[0]?.state === 'completed' && terminal?.outcome === 'completed'
            && reservation?.state === 'released' && reconciliation?.accounting_status === 'pending'
            && accepted?.revision === 2 && accepted.payload.acknowledgementReady
            && worker && !worker.delivery && worker.receipt?.length === 1
            && messages.some(message => message.role === 'assistant' && message.state === 'completed')
            ? { terminal, reservation, reconciliation, accepted, worker, messages } : false
        }, 'the replacement retains one completed terminal and exact business ACK while predecessor accounting stays unknown', 120_000)
        assert.deepEqual(settled.accepted.payload.message, settled.worker.frame,
          'the Control Plane accepts the exact immutable terminal offered by this replacement Worker')
        const finalAuthority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?')
          .get(originalJob.job_id)
        for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token']) {
          assert.equal(settled.terminal[field], replacement.authority[field],
            `the final terminal retains its exact successor authority ${field}`)
        }
        currentExecutionLease(database, finalAuthority, settled.worker.frame.lease)
        const finalCore = coreExecution(settled.worker.run, devicePath.deviceData)
        assert.deepEqual(finalCore.turns, [finalCore.submissionId])
        assert.equal(finalCore.users, 1, 'the replacement Core incarnation accepts one frozen user input')
        assert.equal(finalCore.commands.length, 1, 'only the admitted replacement executes the real shell command')
        assert.equal(finalCore.commands[0].execution, 'completed')
        assert.equal(finalCore.commands[0].disposition, 'accepted')
        assert.equal(finalCore.commands[0].delivery, 'offered')
        assertSuccessfulCoreCommand(finalCore.commands[0])
        assertFrozenChatFile(devicePath.deviceData, originalJob.job_id, 'CHAT-RECOVERY-ONCE.txt', 'once\n')
        const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === originalJob.job_id)
        const oldPhysical = calls.filter(call => call.workerSessionId === oldAuthority.worker_session_id)
          .flatMap(call => call.providerAttempts).filter(attempt => !['prepared', 'not_sent'].includes(attempt.state))
        assert.equal(oldPhysical.length, 0, 'the killed slot waiter did not send a physical attempt')
        const newPhysical = calls.filter(call => call.workerSessionId === replacement.authority.worker_session_id)
          .flatMap(call => call.providerAttempts)
        assert.ok(newPhysical.length >= 2, 'the real replacement retains its model/tool/model Provider loop')
        assert.ok(newPhysical.every(attempt => attempt.state === 'completed' && attempt.usage !== null))
        assert.deepEqual(providerExchangeFacts(devicePath.deviceData, oldExchangeId), [],
          'the obsolete slot waiter creates no Provider rows after its owner dies')
        assert.deepEqual(database.prepare(`SELECT * FROM execution_attempt_accounting_pending
          WHERE job_id=? AND attempt=1`).get(originalJob.job_id), predecessorPending)
        assert.equal(database.prepare(`SELECT COUNT(*) AS n FROM execution_attempt_accounting_facts
          WHERE job_id=? AND attempt=1`).get(originalJob.job_id).n, 0,
          'observing zero HTTP does not manufacture a predecessor accounting statement')
        assert.equal(database.prepare(`SELECT COUNT(*) AS n FROM execution_attempt_accounting_totals
          WHERE job_id=? AND attempt=1`).get(originalJob.job_id).n, 0)
        assert.equal(settled.reservation.release_reason, 'failed')
        assert.equal(settled.reservation.actual_tokens, null)
        assert.equal(settled.reservation.actual_cost_microunits, null)
        assert.equal(settled.reconciliation.settlement_request_id, null)
        assert.equal(settled.reconciliation.settlement_usage_json, null)
        const knownTokens = newPhysical.reduce((total, attempt) => total + attempt.usage.totalTokens, 0)
        const latest = database.prepare(`SELECT usage_json FROM execution_admission_reconciliation_facts
          WHERE job_id=? ORDER BY sequence DESC LIMIT 1`).get(originalJob.job_id)
        for (const usage of [JSON.parse(settled.reconciliation.terminal_usage_json),
          ...(latest ? [JSON.parse(latest.usage_json)] : [])]) {
          assert.equal(usage.accountingStatus, 'unknown')
          assert.equal(usage.tokens ?? null, null)
          assert.equal(usage.costMicrounits ?? null, null)
          assert.equal(usage.knownTokens, knownTokens,
            'only measured successor usage forms a lower bound while its predecessor lacks a statement')
        }
        assert.equal(settled.messages.filter(message => message.role === 'user').length, 1)
        assert.equal(settled.messages.filter(message => message.role === 'assistant' && message.state === 'completed').length, 1)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=? AND attempt=2')
          .get(originalJob.job_id).n, 1)
        const oldAfterCompletion = retainedRuns(devicePath.deviceData, originalJob.job_id)
          .find(value => value.run.job.attempt === 1)
        assert.deepEqual(oldAfterCompletion.bytes, original.bytes)
        assert.deepEqual(coreExecution(oldAfterCompletion.run, devicePath.deviceData), oldCore)
        const httpCount = server.requests.length
        assert.deepEqual((await api.command('chat.submit', 1, submitted[3].payload, requestIds[3])).result,
          submitted[3].receipt.result)
        assert.equal(server.requests.length, httpCount, 'completed command replay starts no new physical attempt')
        await waitFor(() => server.activeRequests === 0, 'every real Provider slot is released after completion', 30_000)
        return { jobId: originalJob.job_id, originalAttempt: 1, replacementAttempt: 2,
          originalCoreTurnId: oldCore.submissionId, replacementCoreTurnId: finalCore.submissionId,
          oldPhysicalAttempts: oldPhysical.length, replacementPhysicalAttempts: newPhysical.length,
          realCommands: finalCore.commands.length, peakHttp: server.peakActiveRequests,
          recoveryMillis: Date.now() - killedAt, trustedExit: true, businessAcknowledged: true,
          predecessorAccounting: 'pending', aggregateAccounting: 'unknown', knownTokens,
          reservationReleased: settled.reservation.state === 'released' }
      } finally {
        for (const marker of markers) server.releaseResponseGate(marker)
        device.close()
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.peakHttp, 3)
  assert.equal(report.flow.scenario.oldPhysicalAttempts, 0)
  assert.equal(report.flow.scenario.realCommands, 1)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})

test('Chat Worker replacement preserves the original sent Provider exchange without reissuing its physical request', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-provider-worker-restart-'))
  const productSessionId = 'psn_01J00000000000000000000231'
  const requestId = 'req_01J00000000000000000000231'
  const marker = 'provider-boundary-worker-restart'
  let complete = false
  t.after(() => {
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed Provider Worker restart evidence retained at ${directory}`)
  })
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '10' },
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: [marker] },
    scenario: { async run({ api, modelRoute, devicePath }) {
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      const device = new DatabaseSync(join(devicePath.deviceData, 'device-client.sqlite3'), { readOnly: true })
      for (const store of [database, device]) store.exec('PRAGMA busy_timeout=5000')
      const server = devicePath.modelServer
      try {
        await api.command('session.create', 0, { productSessionId,
          projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
          title: 'Provider sent request survives Worker replacement', modelRoute })
        await devicePath.launchAnchor({ productSessionId })
        const payload = { productSessionId, message: marker }
        const submitted = await api.command('chat.submit', 1, payload, requestId)
        const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').all(requestId)
        assert.equal(jobs().length, 1)
        const job = jobs()[0]
        const original = await waitFor(() => {
          const run = retainedRuns(devicePath.deviceData, job.job_id).find(value => value.run.job.attempt === 1)
          if (!run?.requests.length) return false
          const [fact] = providerExchangeFacts(devicePath.deviceData, run.requests[0].modelExchangeId)
          const attempt = fact?.attempts[0]
          const physical = attempt && server.requests.find(request => request.idempotencyKey === attempt.adapter_request_id && request.active)
          return fact?.attempts.length === 1 && attempt.state === 'invoking' && physical ? { run, fact, attempt } : false
        }, 'the original exchange has one actual accepted HTTPS request', 60_000)
        const originalExchangeId = original.run.requests[0].modelExchangeId
        const originalKey = original.attempt.adapter_request_id
        const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
        const worker = actualWorkerProcess(device, authority)
        assert.equal(worker.state, 'running')
        assert.ok(worker.data_directory.startsWith(`${devicePath.deviceData}/`))
        process.kill(-worker.pid, 'SIGKILL')
        const interrupted = await waitFor(() => {
          const [fact] = providerExchangeFacts(devicePath.deviceData, originalExchangeId)
          return fact?.attempts.length === 1 && fact.attempts[0].state === 'interrupted_unknown' && fact.exchange.chunks ? fact : false
        }, 'trusted Device recovery closes the original physical attempt in the runtime ledger', 30_000)
        assert.equal(interrupted.exchange.request_open, original.fact.exchange.request_open)
        assert.equal(interrupted.attempts[0].adapter_request_id, originalKey)
        assert.equal(interrupted.attempts[0].accounting_chunks, null)
        const replacement = await waitFor(() => {
          const current = jobs()[0]
          const next = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
          const run = retainedRuns(devicePath.deviceData, job.job_id).find(value => value.run.job.attempt === 2)
          const physical = server.requests.find(request => request.gateMarker === marker && request.idempotencyKey !== originalKey && request.active)
          return current?.attempt === 2 && next?.attempt === 2 && run?.requests.length === 1 && physical
            ? { authority: next, run, physical } : false
        }, 'the official successor starts its own exchange after trusted Worker exit', 90_000)
        assert.equal(jobs().length, 1)
        assert.notEqual(replacement.authority.worker_instance_id, authority.worker_instance_id)
        assert.notEqual(replacement.run.requests[0].modelExchangeId, originalExchangeId)
        assert.equal(server.requests.filter(request => request.idempotencyKey === originalKey).length, 1)
        const assertOriginalExchange = () => {
          const facts = providerExchangeFacts(devicePath.deviceData, originalExchangeId)
          assert.equal(facts.length, 1)
          assert.equal(facts[0].exchange.request_open, interrupted.exchange.request_open)
          assert.equal(facts[0].exchange.chunks, interrupted.exchange.chunks)
          assert.deepEqual(facts[0].attempts, interrupted.attempts,
            'new credential resolution leaves the original interrupted physical attempt unchanged')
        }
        assertOriginalExchange()
        const session = (await api.query('session.get', { productSessionId })).result
        await api.command('session.cancel', session.revision, { productSessionId,
          reason: 'Cancel the observed successor while every response gate remains held' })
        await waitFor(() => server.activeRequests === 0,
          'cancellation closes the successor HTTPS request without releasing its response gate', 10_000)
        server.releaseResponseGate(marker)
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        assert.equal(server.requests.filter(request => request.idempotencyKey === originalKey).length, 1)
        assertOriginalExchange()
        return { originalExchangeId, originalPhysicalRequests: 1,
          successorExchangeId: replacement.run.requests[0].modelExchangeId,
          originalWorkerInstanceId: authority.worker_instance_id,
          successorWorkerInstanceId: replacement.authority.worker_instance_id }
      } finally { server.releaseResponseGate(marker); device.close(); database.close() }
    } },
  })
  assert.equal(report.flow.scenario.originalPhysicalRequests, 1)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})

function installCutPoint(database, point, productSessionId) {
  assert.match(productSessionId, /^psn_[0-9A-HJKMNP-TV-Z]{26}$/u)
  const selectedJob = `SELECT job_id FROM scheduler_execution_jobs WHERE product_session_id='${productSessionId}'`
  if (point === 'terminal-retained') {
    database.exec(`CREATE TRIGGER pause_chat_recovery BEFORE INSERT ON execution_lease_terminals
      WHEN NEW.job_id IN (${selectedJob}) BEGIN SELECT RAISE(ABORT,'injected before CP terminal commit'); END`)
  } else {
    database.exec(`CREATE TRIGGER pause_chat_recovery BEFORE UPDATE ON worker_session_slots
      WHEN NEW.state='completed' AND NEW.job_id IN (${selectedJob})
      BEGIN SELECT RAISE(ABORT,'injected after CP terminal commit before business ACK'); END`)
    database.exec(`CREATE TRIGGER pause_chat_recovery_settlement BEFORE INSERT ON execution_admission_settlement_sources
      WHEN NEW.job_id IN (${selectedJob})
      BEGIN SELECT RAISE(ABORT,'injected before Chat settlement commit'); END`)
  }
}

test('Chat recovers its committed original Job before dispatch after a real Server crash and never repeats the user input or command', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-job-commit-recovery-'))
  let complete = false
  t.after(() => {
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed Chat Job commit recovery retained at ${directory}`)
  })
  const productSessionId = 'psn_01J00000000000000000000063'
  const requestId = 'req_01J00000000000000000000063'
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: {
      'native-cell-A': completedCommandScript('git rev-parse --verify HEAD'),
    } },
    scenario: { async run({ api, modelRoute, devicePath, restartServer }) {
      await api.command('session.create', 0, { productSessionId,
        projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
        title: 'Chat committed Job crash recovery', modelRoute })
      await devicePath.launchAnchor({ productSessionId })
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
      database.exec('PRAGMA busy_timeout=5000')
      try {
        database.exec(`CREATE TRIGGER pause_chat_claim BEFORE INSERT ON execution_leases
          WHEN NEW.job_id IN (SELECT job_id FROM scheduler_execution_jobs WHERE product_session_id='${productSessionId}')
          BEGIN SELECT RAISE(ABORT,'injected after Chat Job commit before dispatch'); END`)
        const payload = { productSessionId, message: 'native-cell-A' }
        const submitted = await api.command('chat.submit', 1, payload, requestId)
        const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE product_session_id=?')
          .all(productSessionId)
        const [original] = jobs()
        assert.equal(jobs().length, 1)
        assert.equal(original.attempt, 1)
        assert.equal(original.state, 'queued')
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_leases WHERE job_id=?').get(original.job_id).n, 0)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_dispatch_authorities WHERE job_id=?').get(original.job_id).n, 0)
        assert.equal(retainedRuns(devicePath.deviceData, original.job_id).length, 0)
        assert.equal(devicePath.modelServer.requests.length, 0)
        const before = (await api.query('session.messages.list', { productSessionId })).result.items
        assert.equal(before.filter(message => message.role === 'user').length, 1)
        const reopened = await restartServer({ crash: true })
        assert.equal(reopened.signal, 'SIGKILL')
        assert.notEqual(reopened.previousPid, reopened.restartedPid)
        assert.deepEqual(jobs(), [original], 'the real Server reopen preserves the original sealed Job bytes and attempt')
        assert.deepEqual((await api.query('session.messages.list', { productSessionId })).result.items, before)
        assert.equal(retainedRuns(devicePath.deviceData, original.job_id).length, 0)
        assert.equal(devicePath.modelServer.requests.length, 0)
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        assert.equal(jobs().length, 1)
        database.exec('DROP TRIGGER pause_chat_claim')
        const settled = await waitFor(async () => {
          const accepted = acceptedOutcome(database, original.job_id)
          const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(original.job_id)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(original.job_id)
          const [worker] = retainedTerminal(devicePath.deviceData, original.job_id)
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          return accepted?.revision === 2 && accepted.payload.acknowledgementReady && terminal?.outcome === 'completed'
            && reservation?.state === 'settled' && worker && !worker.delivery && worker.receipt?.length === 1
            && messages.some(message => message.role === 'assistant' && message.state === 'completed')
            ? { accepted, terminal, reservation, worker, messages } : false
        }, 'the original committed Job dispatches once and retains its exact business ACK after reopening', 120_000)
        const core = coreExecution(settled.worker.run, devicePath.deviceData)
        assert.deepEqual(core.turns, [core.submissionId])
        assert.equal(core.users, 1)
        assert.equal(core.commands.length, 1)
        assert.equal(core.commands[0].execution, 'completed')
        assert.equal(core.commands[0].disposition, 'accepted')
        assert.equal(core.commands[0].delivery, 'offered')
        assertSuccessfulCoreCommand(core.commands[0])
        assert.deepEqual(settled.accepted.payload.message, settled.worker.frame)
        assert.equal(jobs().length, 1)
        assert.equal(jobs()[0].attempt, 1)
        assert.equal(settled.messages.filter(message => message.role === 'user').length, 1)
        assert.equal(settled.messages.filter(message => message.role === 'assistant' && message.state === 'completed').length, 1)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(original.job_id).n, 1)
        assert.equal(settlementSources(database, original.job_id).length, 1)
        const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === original.job_id)
        assert.ok(calls.length >= 2)
        assert.ok(calls.every(call => call.providerAttempts.length === 1 && call.providerAttempts[0].state === 'completed'))
        const httpCount = devicePath.modelServer.requests.length
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        return { point: 'job-commit-before-dispatch', jobId: original.job_id, attempt: 1,
          turnId: core.submissionId, toolCalls: core.commands.length, physicalAttempts: httpCount,
          businessAcknowledged: true, signal: reopened.signal }
      } finally {
        database.exec('DROP TRIGGER IF EXISTS pause_chat_claim')
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.businessAcknowledged, true)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})

for (const [point, suffix] of [['terminal-retained', '61'], ['cp-commit-before-ack', '62'], ['cp-commit-worker-exit', '64']]) {
  test(`Chat recovers ${point} after a real Server crash without duplicate Core execution or accounting`, async t => {
    const directory = mkdtempSync(join(tmpdir(), `wwc-chat-recovery-${point}-`))
    let complete = false
    t.after(() => {
      if (complete) rmSync(directory, { recursive: true, force: true })
      else t.diagnostic(`Failed Chat recovery scenario retained at ${directory}`)
    })
    const productSessionId = `psn_01J000000000000000000000${suffix}`
    const requestId = `req_01J000000000000000000000${suffix}`
    const report = await runApiProductionVertical({
      directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
      deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: {
      'native-cell-A': completedCommandScript('git rev-parse --verify HEAD'),
      } },
      scenario: { async run({ api, modelRoute, devicePath, fixtureDirectory, restartServer }) {
        assert.equal(fixtureDirectory, directory)
        await api.command('session.create', 0, {
          productSessionId, projectId: 'prj_01J00000000000000000000000',
          repositoryId: 'rep_01J00000000000000000000000', title: `Chat recovery ${point}`, modelRoute,
        })
        await devicePath.launchAnchor({ productSessionId })
        const database = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'))
        database.exec('PRAGMA busy_timeout=5000')
        try {
          await waitFor(() => database.prepare(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='execution_admission_settlement_sources'").get(),
          'the real Worker lifecycle prepares the settlement schema before fault injection', 30_000)
          assert.equal(database.prepare('SELECT COUNT(*) AS n FROM scheduler_execution_jobs WHERE product_session_id=?')
            .get(productSessionId).n, 0, 'the selected Chat Job still starts only after the real crash cut is armed')
          installCutPoint(database, point, productSessionId)
          const payload = { productSessionId, message: 'native-cell-A' }
          const submitted = await api.command('chat.submit', 1, payload, requestId)
          assert.equal(submitted.outcome, 'completed')
          const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE product_session_id=?')
            .all(productSessionId)
          const job = jobs()[0]
          assert.equal(jobs().length, 1)
          assert.equal(job.attempt, 1)
          const cut = await waitFor(() => {
            const terminals = retainedTerminal(devicePath.deviceData, job.job_id)
            const accepted = acceptedOutcome(database, job.job_id)
            const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(job.job_id)
            if (terminals.length !== 1 || terminals[0].run.phase !== 'outcome_retained'
              || terminals[0].delivery?.acknowledgement_required !== 1) return false
            return point === 'terminal-retained'
              ? !accepted && !terminal && { worker: terminals[0], accepted, terminal }
              : accepted?.revision === 1 && !accepted.payload.acknowledgementReady
                && terminal?.outcome === 'completed' && { worker: terminals[0], accepted, terminal }
          }, `real ${point} cut point and pending Worker business ACK`, 120_000, 50)
          assert.equal(cut.worker.frame.outcome.status, 'succeeded')
          assert.equal(cut.worker.digest, `sha256:${createHash('sha256').update(cut.worker.bytes).digest('hex')}`)
          const originalCore = coreExecution(cut.worker.run, devicePath.deviceData)
          assert.deepEqual(originalCore.turns, [originalCore.submissionId], 'one real Core turn was accepted')
          assert.equal(originalCore.users, 1, 'one real Core user input was recorded')
          assert.equal(originalCore.commands.length, 1, 'the real command executed once before the crash')
          assert.equal(originalCore.commands[0].turnId, originalCore.submissionId)
          assert.equal(originalCore.commands[0].execution, 'completed')
          assert.equal(originalCore.commands[0].disposition, 'accepted')
          assert.equal(originalCore.commands[0].delivery, 'offered')
          assertSuccessfulCoreCommand(originalCore.commands[0])
          const originalSettlementSources = settlementSources(database, job.job_id)
          assert.equal(originalSettlementSources.length, 0, 'the real crash cut precedes settlement')
          const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
          assert.ok(calls.length >= 2, 'the real model/tool/model loop retained physical Provider calls')
          assert.ok(calls.every(call => call.providerAttempts.length === 1 && call.providerAttempts[0].state === 'completed'))
          const httpCount = devicePath.modelServer.requests.length
          const endpoint = api.baseUrl
          const deadWorker = point === 'cp-commit-worker-exit'
          const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
          const probe = deadWorker ? readWorkerTransport(devicePath.deviceData, authority) : null
          const restarted = await restartServer({ crash: true })
          assert.equal(restarted.signal, 'SIGKILL')
          assert.notEqual(restarted.previousPid, restarted.restartedPid)
          assert.equal(restarted.health, 'ready')
          assert.equal(api.baseUrl, endpoint, 'Device transport reconnects to the original Server origin')
          assert.equal(devicePath.api, api, 'the Device launch closure retains the reauthenticated API object')
          const afterCrash = retainedTerminal(devicePath.deviceData, job.job_id)
          assert.equal(afterCrash.length, 1)
          assert.deepEqual(afterCrash[0].bytes, cut.worker.bytes, 'the original Worker terminal remains immutable')
          assert.deepEqual(coreExecution(afterCrash[0].run, devicePath.deviceData), originalCore)
          assert.deepEqual(acceptedOutcome(database, job.job_id), cut.accepted)
          assert.deepEqual(settlementSources(database, job.job_id), originalSettlementSources)
          assert.equal(devicePath.modelServer.requests.length, httpCount)
          let exitedRelayStatus = null
          if (deadWorker) {
            assert.equal(Number(execFileSync('ps', ['-p', String(probe.pid), '-o', 'pgid='], { encoding: 'utf8' }).trim()), probe.pid)
            globalThis.process.kill(-probe.pid, 'SIGKILL')
            await waitFor(() => database.prepare('SELECT 1 FROM worker_launch_grant_exits WHERE worker_session_id=? AND worker_instance_id=?')
              .get(probe.launchWorkerSessionId, probe.workerInstanceId), 'the actual Device reaps the old Worker and publishes its trusted exit', 30_000)
            const pending = acceptedOutcome(database, job.job_id)
            assert.equal(pending.revision, 1)
            assert.equal(pending.payload.acknowledgementReady, false)
            assert.equal(database.prepare('SELECT state FROM worker_session_slots WHERE job_id=?').get(job.job_id).state, 'running')
            const denied = await probe.postRetainedMessage(cut.worker.bytes)
            assert.equal(denied.status, 503, 'the original accepted terminal still awaits the blocked CP cleanup')
            assert.equal(denied.acknowledgement, null)
            exitedRelayStatus = denied.status
            assert.deepEqual(acceptedOutcome(database, job.job_id), pending, 'Device relay replay cannot bypass the blocked terminal cleanup')
          }
          database.exec('DROP TRIGGER pause_chat_recovery')
          database.exec('DROP TRIGGER IF EXISTS pause_chat_recovery_settlement')
          const settled = await waitFor(async () => {
            const accepted = acceptedOutcome(database, job.job_id)
            const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(job.job_id)
            const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
            const slot = database.prepare('SELECT * FROM worker_session_slots WHERE job_id=?').get(job.job_id)
            const [worker] = retainedTerminal(devicePath.deviceData, job.job_id)
            const messages = (await api.query('session.messages.list', { productSessionId })).result.items
            return accepted?.revision === 2 && accepted.payload.acknowledgementReady
              && terminal?.outcome === 'completed' && reservation?.state === 'settled' && slot?.state === 'completed'
              && worker && (deadWorker ? worker.delivery?.acknowledgement_required === 1 && worker.receipt === null
                : !worker.delivery && worker.receipt?.length === 1)
              && messages.some(message => message.role === 'assistant' && message.state === 'completed')
              ? { accepted, terminal, reservation, slot, worker, messages } : false
          }, deadWorker ? 'Server-owned recovery finishes accepted cleanup after the original Worker exits'
            : 'exact terminal replay completes cleanup and durable Worker business ACK', 120_000, 50)
          assert.deepEqual(settled.accepted.payload.message, cut.worker.frame)
          if (cut.accepted) assert.deepEqual(settled.accepted.payload.decision, cut.accepted.payload.decision)
          assert.deepEqual(settled.worker.bytes, cut.worker.bytes)
          assert.deepEqual(coreExecution(settled.worker.run, devicePath.deviceData), originalCore)
          const sources = settlementSources(database, job.job_id)
          assert.equal(sources.length, 1, 'the Control Plane retained one immutable settlement source')
          const source = JSON.parse(sources[0].fact_json)
          assert.equal(source.actualTokens, cut.worker.frame.outcome.usage.tokens)
          assert.equal(source.actualRuntimeMillis, cut.worker.frame.outcome.usage.runtimeMillis)
          assert.equal(source.actualCostMicrounits, cut.worker.frame.outcome.usage.costMicrounits)
          if (originalSettlementSources.length) assert.deepEqual(sources, originalSettlementSources)
          assert.equal(jobs().length, 1)
          assert.equal(jobs()[0].attempt, 1)
          assert.equal(jobs()[0].state, 'completed')
          assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(job.job_id).n, 1)
          assert.equal(settled.messages.filter(message => message.role === 'user').length, 1)
          assert.equal(settled.messages.filter(message => message.role === 'assistant' && message.state === 'completed').length, 1)
          assert.equal((await api.query('session.get', { productSessionId })).result.state, 'idle')
          assert.deepEqual(readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id), calls)
          assert.equal(devicePath.modelServer.requests.length, httpCount, 'recovery emits no duplicate HTTP accounting attempt')
          const replay = await api.command('chat.submit', 1, payload, requestId)
          assert.deepEqual(replay.result, submitted.result, 'lost submit response retains its original command receipt')
          assert.equal(jobs().length, 1)
          assert.equal(devicePath.modelServer.requests.length, httpCount)
          return { point, jobId: job.job_id, attempt: 1, codexThreadId: originalCore.codexThreadId,
            turnId: originalCore.submissionId, toolCalls: originalCore.commands.length, physicalAttempts: httpCount,
            terminalDigest: cut.worker.digest, beforeRevision: cut.accepted?.revision ?? 0, afterRevision: 2,
            signal: restarted.signal, businessAcknowledged: !deadWorker, serverCleanupRecovered: true,
            exitedRelayStatus }
        } finally {
          database.exec('DROP TRIGGER IF EXISTS pause_chat_recovery')
          database.exec('DROP TRIGGER IF EXISTS pause_chat_recovery_settlement')
          database.close()
        }
      } },
    })
    assert.equal(report.health.afterRestart, 'ready')
    assert.equal(report.flow.scenario.businessAcknowledged, point !== 'cp-commit-worker-exit')
    assert.equal(report.flow.scenario.serverCleanupRecovered, true)
    t.diagnostic(JSON.stringify(report.flow.scenario))
    complete = true
  })
}

test('Chat preserves its accepted real Core turn and original physical model request across a Server crash and legal renewal beyond the initial lease deadline', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-core-accepted-recovery-'))
  let complete = false
  t.after(() => {
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed Chat accepted Core recovery retained at ${directory}`)
  })
  const productSessionId = 'psn_01J00000000000000000000064'
  const requestId = 'req_01J00000000000000000000064'
  const marker = 'core-accepted-crash-renewal'
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: [marker], nativeCellScripts: {
      'native-cell-A': completedCommandScript('git rev-parse --verify HEAD'),
    } },
    scenario: { async run({ api, modelRoute, devicePath, restartServer }) {
      await api.command('session.create', 0, { productSessionId,
        projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
        title: 'Accepted Core and model renewal continuity', modelRoute })
      await devicePath.launchAnchor({ productSessionId })
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      const device = new DatabaseSync(join(devicePath.deviceData, 'device-client.sqlite3'), { readOnly: true })
      database.exec('PRAGMA busy_timeout=5000')
      device.exec('PRAGMA busy_timeout=5000')
      try {
        const payload = { productSessionId, message: `native-cell-A ${marker}` }
        const submitted = await api.command('chat.submit', 1, payload, requestId)
        const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').get(requestId)
        const original = await waitFor(() => retainedRuns(devicePath.deviceData, job.job_id)
          .find(value => value.run.phase === 'runtime_started' && value.requests.length === 1
            && devicePath.modelServer.requests.some(request => request.gateMarker === marker && request.active)),
        'the real Core turn is accepted and its original HTTPS attempt is held before any outcome', 60_000)
        const coreBefore = coreExecution(original.run, devicePath.deviceData)
        assert.deepEqual(coreBefore.turns, [coreBefore.submissionId])
        assert.equal(coreBefore.users, 1)
        assert.equal(coreBefore.commands.length, 0)
        const originalAuthority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
        const worker = actualWorkerProcess(device, originalAuthority)
        const claim = JSON.parse(database.prepare(`SELECT response_json FROM execution_lease_request_receipts
          WHERE job_id=? AND operation='claim' AND json_extract(response_json,'$.status')='accepted'
          ORDER BY rowid LIMIT 1`).get(job.job_id).response_json).lease
        assert.equal(Date.parse(claim.expires_at) - Date.parse(claim.issued_at), 30_000)
        const [firstCall] = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
        assert.equal(firstCall.providerAttempts.length, 1)
        assert.equal(firstCall.providerAttempts[0].state, 'invoking')
        assert.equal(devicePath.modelServer.requests.length, 1)
        assert.equal(acceptedOutcome(database, job.job_id), null)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(job.job_id).n, 0)
        const restarted = await restartServer({ crash: true })
        assert.equal(restarted.signal, 'SIGKILL')
        assert.notEqual(restarted.previousPid, restarted.restartedPid)
        assert.equal(actualWorkerProcess(device, originalAuthority).pid, worker.pid,
          'the exact real Worker and embedded Core survive this Server crash')
        assert.equal(devicePath.modelServer.requests.length, 1, 'reopening Server does not repeat the in-flight physical request')
        const afterCrash = retainedRuns(devicePath.deviceData, job.job_id)[0]
        assert.equal(afterCrash.run.kernelSessionId, original.run.kernelSessionId)
        assert.equal(afterCrash.run.submissionId, original.run.submissionId)
        assert.deepEqual(coreExecution(afterCrash.run, devicePath.deviceData), coreBefore)
        assert.deepEqual(afterCrash.requests, original.requests, 'the original model receipt retains its identity, body and original lease stamp')
        const originalLease = currentExecutionLease(database, originalAuthority, original.requests[0].lease)
        const observed = [{ expiresAt: originalLease.expires_at, observedAt: Date.now() }]
        const renewed = await waitFor(() => {
          const current = currentExecutionLease(database, originalAuthority, original.requests[0].lease)
          const previous = observed.at(-1)
          if (current.expires_at !== previous.expiresAt) {
            assert.ok(Date.now() < Date.parse(previous.expiresAt), 'the actual heartbeat renews before its previous authorized deadline')
            assert.ok(Date.parse(current.expires_at) > Date.parse(previous.expiresAt))
            observed.push({ expiresAt: current.expires_at, observedAt: Date.now() })
          }
          return Date.now() > Date.parse(claim.expires_at) + 500 && observed.length >= 3
            && Date.parse(current.expires_at) > Date.now() ? current : false
        }, 'the surviving execution completes real heartbeat renewal across the initial thirty second lease deadline', 60_000, 250)
        for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token', 'issued_at']) {
          assert.equal(renewed[field], originalAuthority[field], `legal renewal preserves the original authority ${field}`)
        }
        assert.deepEqual(retainedRuns(devicePath.deviceData, job.job_id)[0].requests, original.requests)
        assert.equal(devicePath.modelServer.requests.length, 1)
        devicePath.modelServer.releaseResponseGate(marker)
        const settled = await waitFor(async () => {
          const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(job.job_id)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
          const accepted = acceptedOutcome(database, job.job_id)
          const [retained] = retainedTerminal(devicePath.deviceData, job.job_id)
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          return terminal?.outcome === 'completed' && reservation?.state === 'settled'
            && accepted?.revision === 2 && accepted.payload.acknowledgementReady && retained && !retained.delivery
            && retained.receipt?.length === 1 && messages.some(message => message.role === 'assistant' && message.state === 'completed')
            ? { terminal, reservation, accepted, retained, messages } : false
        }, 'the original held physical model response completes the same renewed Core execution and exact business ACK', 120_000)
        const finalCore = coreExecution(settled.retained.run, devicePath.deviceData)
        assert.equal(finalCore.kernelSessionId, coreBefore.kernelSessionId)
        assert.equal(finalCore.codexThreadId, coreBefore.codexThreadId)
        assert.equal(finalCore.submissionId, coreBefore.submissionId)
        assert.equal(finalCore.submissionDigest, coreBefore.submissionDigest)
        assert.deepEqual(finalCore.turns, coreBefore.turns)
        assert.equal(finalCore.users, 1)
        assert.equal(finalCore.commands.length, 1)
        assert.equal(finalCore.commands[0].execution, 'completed')
        assert.equal(finalCore.commands[0].disposition, 'accepted')
        assert.equal(finalCore.commands[0].delivery, 'offered')
        assertSuccessfulCoreCommand(finalCore.commands[0])
        assert.deepEqual(settled.accepted.payload.message, settled.retained.frame)
        const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
        const completedOriginal = calls.find(call => call.exchangeId === firstCall.exchangeId)
        assert.equal(completedOriginal.requestSha256, firstCall.requestSha256)
        assert.equal(completedOriginal.providerAttempts.length, 1)
        assert.equal(completedOriginal.providerAttempts[0].adapterRequestId, firstCall.providerAttempts[0].adapterRequestId)
        assert.equal(completedOriginal.providerAttempts[0].state, 'completed')
        assert.ok(calls.every(call => call.providerAttempts.length === 1 && call.providerAttempts[0].state === 'completed'))
        assert.equal(settled.reservation.actual_tokens, calls.reduce((total, call) => total + call.providerAttempts[0].usage.totalTokens, 0))
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM scheduler_execution_jobs WHERE product_session_id=?').get(productSessionId).n, 1)
        assert.equal(database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(job.job_id).attempt, 1)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(job.job_id).n, 1)
        assert.equal(settled.messages.filter(message => message.role === 'user').length, 1)
        assert.equal(settled.messages.filter(message => message.role === 'assistant' && message.state === 'completed').length, 1)
        const httpCount = devicePath.modelServer.requests.length
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        assert.equal(devicePath.modelServer.activeRequests, 0)
        return { point: 'core-accepted-before-outcome', jobId: job.job_id, attempt: 1,
          coreTurnId: finalCore.submissionId, heartbeatRenewals: observed.length - 1,
          initialDeadline: claim.expires_at, renewedDeadline: renewed.expires_at,
          toolCalls: finalCore.commands.length, physicalAttempts: httpCount, businessAcknowledged: true }
      } finally {
        devicePath.modelServer.releaseResponseGate(marker)
        device.close()
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.businessAcknowledged, true)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})

test('Chat preserves its original partial multiquestion answer set and exact live Core waiter across a real Server crash', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-input-crash-recovery-'))
  let complete = false
  t.after(() => {
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed Chat multiquestion crash recovery retained at ${directory}`)
  })
  const productSessionId = 'psn_01J00000000000000000000065'
  const requestId = 'req_01J00000000000000000000065'
  const answerRequestIds = ['req_01J00000000000000000000066', 'req_01J00000000000000000000067']
  const privateValues = ['recovery-private-first-3847', 'recovery-private-second-1926']
  const script = `const input=ALL_TOOLS.find(tool=>tool.name.endsWith("request_user_input"));
    if(!input) throw new Error("input tool missing");
    await tools[input.name]({questions:[
      {id:"recovery_first",header:"First",question:"First recovery choice?",options:[{label:"continue",description:"Continue."},{label:"revise",description:"Revise."}]},
      {id:"recovery_second",header:"Second",question:"Second recovery choice?",options:[{label:"continue",description:"Continue."},{label:"revise",description:"Revise."}]}
    ]});
    const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));
    if(!command) throw new Error("command tool missing");
    text(await tools[command.name]({cmd:"printf 'once\\n' >> CHAT-INPUT-RECOVERY-ONCE.txt",yield_time_ms:1000}));`
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: { 'native-cell-A': script } },
    scenario: { async run({ api, modelRoute, devicePath, restartServer }) {
      await api.command('session.create', 0, { productSessionId,
        projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
        title: 'Partial exact multiquestion Server recovery', modelRoute })
      await devicePath.launchAnchor({ productSessionId })
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      const device = new DatabaseSync(join(devicePath.deviceData, 'device-client.sqlite3'), { readOnly: true })
      database.exec('PRAGMA busy_timeout=5000')
      device.exec('PRAGMA busy_timeout=5000')
      try {
        await api.command('chat.submit', 1, { productSessionId, message: 'native-cell-A' }, requestId)
        const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').get(requestId)
        const inputs = async () => (await api.query('session.interactions.list', { productSessionId,
          states: ['pending', 'resolved'] })).result.items.filter(item => item.kind === 'input')
        const questions = await waitFor(async () => {
          const values = await inputs()
          return values.length === 2 && values.every(value => value.state === 'pending') ? values : false
        }, 'both real Core questions exist as original public input projections', 60_000)
        const ordered = ['First recovery choice?', 'Second recovery choice?']
          .map(prompt => questions.find(question => question.prompt === prompt))
        assert.ok(ordered.every(Boolean))
        assert.equal(new Set(ordered.map(question => question.inputRequestId)).size, 2)
        assert.ok(ordered.every(question => question.binding.executionJobId === job.job_id))
        const firstPayload = { ...ordered[0].binding, inputRequestId: ordered[0].inputRequestId,
          status: 'provided', value: { mode: 'single_choice', value: privateValues[0] } }
        const firstReceipt = await api.command('input.respond', ordered[0].revision, firstPayload, answerRequestIds[0])
        const partial = await waitFor(() => {
          const values = privateInputCalls(devicePath.deviceData, job.job_id)
          return values.length === 1 && Object.keys(values[0].call.answers).length === 1
            && values[0].call.appliedIncarnationId === null ? values[0] : false
        }, 'the first private value is durably retained and does not settle the entire two-question Core waiter', 30_000)
        assert.deepEqual(partial.call.answers[ordered[0].inputRequestId].values, [privateValues[0]])
        assert.equal(partial.call.questions.length, 2)
        const publicBefore = await inputs()
        assert.equal(publicBefore.filter(question => question.state === 'provided').length, 1)
        assert.equal(publicBefore.filter(question => question.state === 'pending').length, 1)
        assert.ok(privateValues.every(value => !JSON.stringify(publicBefore).includes(value)))
        const originalRun = retainedRuns(devicePath.deviceData, job.job_id)[0]
        const originalCore = coreExecution(originalRun.run, devicePath.deviceData)
        assert.deepEqual(originalCore.turns, [originalCore.submissionId])
        assert.equal(originalCore.users, 1)
        assert.equal(originalCore.commands.length, 0)
        const httpCountBeforeCrash = devicePath.modelServer.requests.length
        const authorityBefore = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
        const originalRequests = retainedInteractionRequests(devicePath.deviceData, job.job_id)
        assert.equal(originalRequests.length, 2)
        assert.equal(partial.call.turnId, originalCore.submissionId)
        const inputRequests = coreInputRequests(devicePath.deviceData, originalCore.kernelSessionId)
        assert.equal(inputRequests.length, 1)
        assert.equal(inputRequests[0].turn_id, partial.call.turnId)
        assert.equal(inputRequests[0].logical_id, partial.call.callId)
        assert.deepEqual(partial.call.questions.map(question => question.inputRequestId), ordered.map(question => question.inputRequestId))
        assert.deepEqual(partial.call.questions.map(question => question.questionId), ['recovery_first', 'recovery_second'])
        for (const question of ordered) {
          const retained = originalRequests.find(value => value.message.inputRequestId === question.inputRequestId)
          assert.ok(retained)
          assert.equal(retained.message.expiresAt, question.expiresAt)
          assert.equal(question.expiresAt, partial.call.expiresAt)
          assertOriginalInteraction(retained.message, authorityBefore, originalRun.run, productSessionId)
          currentExecutionLease(database, authorityBefore, retained.message.lease)
        }
        const workerBefore = actualWorkerProcess(device, authorityBefore)
        const renewed = await renewOriginalInteraction(database, authorityBefore, originalRequests[0].message)
        assert.deepEqual(retainedInteractionRequests(devicePath.deviceData, job.job_id), originalRequests)
        assert.deepEqual(privateInputCalls(devicePath.deviceData, job.job_id)[0].bytes, partial.bytes)
        assert.deepEqual(await inputs(), publicBefore, 'legal renewal preserves original question identities and deadlines')
        const restarted = await restartServer({ crash: true })
        assert.equal(restarted.signal, 'SIGKILL')
        assert.notEqual(restarted.previousPid, restarted.restartedPid)
        assert.equal(actualWorkerProcess(device, authorityBefore).pid, workerBefore.pid)
        assert.deepEqual(retainedInteractionRequests(devicePath.deviceData, job.job_id), originalRequests)
        assert.deepEqual(coreInputRequests(devicePath.deviceData, originalCore.kernelSessionId), inputRequests)
        const afterLease = currentExecutionLease(database, authorityBefore, originalRequests[0].message.lease)
        assert.ok(Date.parse(afterLease.expires_at) >= Date.parse(renewed.expires_at))
        assert.deepEqual(await inputs(), publicBefore, 'Server recovery retains both exact question identities, deadlines and original decisions')
        const [afterCrash] = privateInputCalls(devicePath.deviceData, job.job_id)
        assert.deepEqual(afterCrash.bytes, partial.bytes, 'Server recovery preserves the complete private partial answer set')
        const runAfterCrash = retainedRuns(devicePath.deviceData, job.job_id)[0]
        assert.equal(runAfterCrash.run.kernelSessionId, originalRun.run.kernelSessionId)
        assert.equal(runAfterCrash.run.submissionId, originalRun.run.submissionId)
        assert.deepEqual(coreExecution(runAfterCrash.run, devicePath.deviceData), originalCore)
        assert.equal(devicePath.modelServer.requests.length, httpCountBeforeCrash)
        const authorityAfter = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
        for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'worker_session_id', 'attempt', 'fencing_token']) {
          assert.equal(authorityAfter[field], authorityBefore[field])
        }
        assert.deepEqual((await api.command('input.respond', ordered[0].revision, firstPayload, answerRequestIds[0])).result,
          firstReceipt.result, 'the accepted first response only replays its original receipt after reopening')
        assert.equal(privateInputCalls(devicePath.deviceData, job.job_id)[0].call.appliedIncarnationId, null)
        await assert.rejects(api.command('input.respond', ordered[0].revision, {
          ...firstPayload, value: { mode: 'single_choice', value: 'changed-recovery-private-value' },
        }, answerRequestIds[0]), error => error.status === 409)
        const secondPayload = { ...ordered[1].binding, inputRequestId: ordered[1].inputRequestId,
          status: 'provided', value: { mode: 'single_choice', value: privateValues[1] } }
        assert.ok(Date.now() < Date.parse(ordered[1].expiresAt), 'the second original answer is submitted before the unchanged deadline')
        const secondReceipt = await api.command('input.respond', ordered[1].revision, secondPayload, answerRequestIds[1])
        const settled = await waitFor(async () => {
          const [privateCall] = privateInputCalls(devicePath.deviceData, job.job_id)
          const [worker] = retainedTerminal(devicePath.deviceData, job.job_id)
          const outcomes = readRetainedOutcomes(devicePath.deviceData, job.job_id)
          const acknowledged = outcomes.length === 1 && outcomes[0].workerAcknowledged
          const accepted = acceptedOutcome(database, job.job_id)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
          const projections = await inputs()
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          return privateCall?.call.appliedIncarnationId && Object.keys(privateCall.call.answers).length === 2
            && accepted?.revision === 2 && accepted.payload.acknowledgementReady && reservation?.state === 'settled'
            && acknowledged && worker && !worker.delivery && worker.receipt?.length === 1
            && projections.filter(question => question.state === 'provided').length === 2
            && messages.some(message => message.role === 'assistant' && message.state === 'completed')
            ? { privateCall, worker, accepted, projections, messages } : false
        }, 'both retained original answers settle the exact surviving Core waiter once and complete its real command', 120_000)
        assert.equal(settled.privateCall.requestDigest, partial.requestDigest)
        assert.equal(settled.privateCall.runKey, partial.runKey)
        assert.equal(settled.privateCall.call.appliedIncarnationId, settled.privateCall.call.readyIncarnationId)
        assert.equal(settled.privateCall.call.readyIncarnationId, partial.call.readyIncarnationId)
        for (const [index, question] of ordered.entries()) {
          assert.deepEqual(settled.privateCall.call.answers[question.inputRequestId].values, [privateValues[index]])
          const current = settled.projections.find(value => value.inputRequestId === question.inputRequestId)
          assert.deepEqual(current.binding, question.binding)
          assert.equal(current.expiresAt, question.expiresAt)
        }
        const finalCore = coreExecution(settled.worker.run, devicePath.deviceData)
        assert.equal(finalCore.kernelSessionId, originalCore.kernelSessionId)
        assert.equal(finalCore.submissionId, originalCore.submissionId)
        assert.deepEqual(finalCore.turns, originalCore.turns)
        assert.equal(finalCore.users, 1)
        assert.equal(finalCore.commands.length, 1)
        assert.equal(finalCore.commands[0].execution, 'completed')
        assert.equal(finalCore.commands[0].disposition, 'accepted')
        assert.equal(finalCore.commands[0].delivery, 'offered')
        assertSuccessfulCoreCommand(finalCore.commands[0])
        for (const field of ['executionResult', 'acceptedResult']) {
          assert.equal(JSON.parse(JSON.parse(finalCore.commands[0][field]).effective_input.input).cmd, "printf 'once\n' >> CHAT-INPUT-RECOVERY-ONCE.txt")
        }
        assert.deepEqual(coreInputRequests(devicePath.deviceData, originalCore.kernelSessionId), inputRequests)
        assert.deepEqual(retainedInteractionRequests(devicePath.deviceData, job.job_id), originalRequests)
        assertExactInteractionOutcome(database, devicePath.deviceData, job.job_id, authorityBefore, settled.worker.frame)
        assert.deepEqual(settled.accepted.payload.message, settled.worker.frame)
        const publicResult = JSON.stringify({ messages: settled.messages, interactions: settled.projections,
          runtime: (await api.query('runtime.projection.get', { kind: 'product-session', productSessionId })).result })
        assert.ok(privateValues.every(value => !publicResult.includes(value)), 'retained private answers never enter public projections')
        assertFrozenChatFile(devicePath.deviceData, job.job_id, 'CHAT-INPUT-RECOVERY-ONCE.txt', 'once\n')
        const httpCount = devicePath.modelServer.requests.length
        for (const [index, question] of ordered.entries()) {
          const payload = index === 0 ? firstPayload : secondPayload
          const receipt = index === 0 ? firstReceipt : secondReceipt
          assert.deepEqual((await api.command('input.respond', question.revision, payload, answerRequestIds[index])).result,
            receipt.result)
        }
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        assert.equal(privateInputCalls(devicePath.deviceData, job.job_id).length, 1)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM scheduler_execution_jobs WHERE product_session_id=?').get(productSessionId).n, 1)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(job.job_id).n, 1)
        return { jobId: job.job_id, point: 'partial-input-answer-before-complete-waiter', questionCount: 2,
          retainedAnswersBeforeCrash: 1, retainedAnswersAfterCompletion: 2,
          sameCoreTurnId: finalCore.submissionId, appliedExactIncarnation: true, toolCalls: finalCore.commands.length,
          businessAcknowledged: true, sensitiveValuesPublic: false, originalInteractionDeadline: partial.call.expiresAt,
          renewedLeaseDeadline: renewed.expires_at, successfulAfterRealRenewal: true }
      } finally {
        device.close()
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.businessAcknowledged, true)
  assert.equal(report.flow.scenario.appliedExactIncarnation, true)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})

test('Chat preserves its original pending real Core approval across a Server crash and applies the formal decision to one shell operation', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-approval-crash-recovery-'))
  let complete = false
  t.after(() => {
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed Chat approval crash recovery retained at ${directory}`)
  })
  const productSessionId = 'psn_01J00000000000000000000068'
  const requestId = 'req_01J00000000000000000000068'
  const decisionRequestId = 'req_01J00000000000000000000069'
  const script = `const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));
    if(!command) throw new Error("command tool missing");
    text(await tools[command.name]({cmd:"printf 'once\\n' >> CHAT-APPROVAL-RECOVERY-ONCE.txt",
      sandbox_permissions:"require_escalated",justification:"Exercise the temporary recovered approval fixture.",yield_time_ms:1000}));`
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: { 'native-cell-A': script } },
    scenario: { async run({ api, modelRoute, devicePath, restartServer }) {
      await api.command('session.create', 0, { productSessionId,
        projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
        title: 'Pending real Core approval Server recovery', modelRoute })
      await devicePath.launchAnchor({ productSessionId })
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      const device = new DatabaseSync(join(devicePath.deviceData, 'device-client.sqlite3'), { readOnly: true })
      database.exec('PRAGMA busy_timeout=5000')
      device.exec('PRAGMA busy_timeout=5000')
      const files = () => globSync('**/CHAT-APPROVAL-RECOVERY-ONCE.txt', { cwd: devicePath.deviceData })
        .map(path => join(devicePath.deviceData, path))
      try {
        await api.command('chat.submit', 1, { productSessionId, message: 'native-cell-A' }, requestId)
        const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').get(requestId)
        const original = await waitFor(async () => (await api.query('approval.list', { states: ['pending'] })).result.items
          .find(approval => approval.binding.executionJobId === job.job_id && approval.binding.productSessionId === productSessionId),
        'the original real escalated shell reaches its public pending approval', 60_000)
        assert.equal(original.category, 'shell')
        assert.equal(original.decisionEnabled, true)
        assert.equal(files().length, 0, 'pending approval has performed no shell side effect')
        const privateOriginal = await waitFor(() => privateApprovalOperations(devicePath.deviceData, job.job_id)
          .find(operation => operation.approval_id === original.id && operation.state === 'pending'),
        'the exact original Core approval operation is durably retained', 30_000)
        assert.equal(privateOriginal.resolution_digest, null)
        const [originalRun] = retainedRuns(devicePath.deviceData, job.job_id)
        const coreBefore = coreExecution(originalRun.run, devicePath.deviceData)
        assert.equal(privateOriginal.kernel_session_id, coreBefore.kernelSessionId)
        assert.equal(privateOriginal.turn_id, coreBefore.submissionId)
        assert.deepEqual(coreBefore.turns, [coreBefore.submissionId])
        assert.equal(coreBefore.users, 1)
        const httpBeforeCrash = devicePath.modelServer.requests.length
        const authorityBefore = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
        const originalRequests = retainedInteractionRequests(devicePath.deviceData, job.job_id)
        assert.equal(originalRequests.length, 1)
        const request = originalRequests[0].message
        assert.equal(request.approvalId, original.id)
        assert.equal(request.expiresAt, original.expiresAt)
        assertOriginalInteraction(request, authorityBefore, originalRun.run, productSessionId)
        const workerBefore = actualWorkerProcess(device, authorityBefore)
        const renewed = await renewOriginalInteraction(database, authorityBefore, request)
        assert.deepEqual(retainedInteractionRequests(devicePath.deviceData, job.job_id), originalRequests)
        assert.deepEqual(privateApprovalOperations(devicePath.deviceData, job.job_id), [privateOriginal])
        assert.deepEqual((await api.query('approval.get', { approvalId: original.id })).result, original,
          'legal renewal leaves the original pending approval deadline and identity unchanged')
        const restarted = await restartServer({ crash: true })
        assert.equal(restarted.signal, 'SIGKILL')
        assert.notEqual(restarted.previousPid, restarted.restartedPid)
        assert.equal(actualWorkerProcess(device, authorityBefore).pid, workerBefore.pid)
        assert.deepEqual(retainedInteractionRequests(devicePath.deviceData, job.job_id), originalRequests)
        const afterLease = currentExecutionLease(database, authorityBefore, request.lease)
        assert.ok(Date.parse(afterLease.expires_at) >= Date.parse(renewed.expires_at))
        assert.deepEqual((await api.query('approval.get', { approvalId: original.id })).result, original,
          'Server reopen retains the original safe action, binding, deadline and approval revision')
        assert.deepEqual(privateApprovalOperations(devicePath.deviceData, job.job_id), [privateOriginal],
          'the pending exact Core approval operation is preserved without a fabricated decision')
        assert.deepEqual(coreExecution(retainedRuns(devicePath.deviceData, job.job_id)[0].run, devicePath.deviceData), coreBefore)
        assert.equal(devicePath.modelServer.requests.length, httpBeforeCrash)
        assert.equal(files().length, 0)
        const authorityAfter = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
        for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'worker_session_id', 'attempt', 'fencing_token']) {
          assert.equal(authorityAfter[field], authorityBefore[field])
        }
        const payload = { approvalId: original.id, binding: original.binding, decision: 'approve',
          reason: 'Approve the original temporary shell operation after Server recovery.' }
        assert.ok(Date.now() < Date.parse(original.expiresAt), 'the original approval succeeds before its unchanged deadline')
        const receipt = await api.command('approval.decide', original.revision, payload, decisionRequestId)
        const settled = await waitFor(async () => {
          const operation = privateApprovalOperations(devicePath.deviceData, job.job_id)
            .find(value => value.approval_id === original.id)
          const [worker] = retainedTerminal(devicePath.deviceData, job.job_id)
          const outcomes = readRetainedOutcomes(devicePath.deviceData, job.job_id)
          const acknowledged = outcomes.length === 1 && outcomes[0].workerAcknowledged
          const accepted = acceptedOutcome(database, job.job_id)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
          const approval = (await api.query('approval.get', { approvalId: original.id })).result
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          return operation?.state === 'resolved' && operation.resolution_digest && approval.state === 'approved'
            && accepted?.revision === 2 && accepted.payload.acknowledgementReady && reservation?.state === 'settled'
            && acknowledged && worker && !worker.delivery && worker.receipt?.length === 1
            && messages.some(message => message.role === 'assistant' && message.state === 'completed')
            ? { operation, worker, accepted, approval, messages } : false
        }, 'the formal decision applies to the exact original operation and completes one measured execution and business ACK', 120_000)
        for (const field of ['approval_id', 'run_key', 'kernel_session_id', 'operation_kind', 'operation_id', 'turn_id', 'request_digest']) {
          assert.equal(settled.operation[field], privateOriginal[field], `approval recovery preserves original operation ${field}`)
        }
        assert.deepEqual(settled.operation.detail_json, privateOriginal.detail_json)
        assert.deepEqual(settled.approval.binding, original.binding)
        assert.equal(settled.approval.expiresAt, original.expiresAt)
        const coreAfter = coreExecution(settled.worker.run, devicePath.deviceData)
        assert.equal(coreAfter.kernelSessionId, coreBefore.kernelSessionId)
        assert.equal(coreAfter.submissionId, coreBefore.submissionId)
        assert.deepEqual(coreAfter.turns, coreBefore.turns)
        assert.equal(coreAfter.users, 1)
        assert.equal(coreAfter.commands.length, 1)
        assert.equal(coreAfter.commands[0].execution, 'completed')
        assert.equal(coreAfter.commands[0].disposition, 'accepted')
        assert.equal(coreAfter.commands[0].delivery, 'offered')
        assertSuccessfulCoreCommand(coreAfter.commands[0])
        for (const field of ['executionResult', 'acceptedResult']) {
          assert.equal(JSON.parse(JSON.parse(coreAfter.commands[0][field]).effective_input.input).cmd, "printf 'once\n' >> CHAT-APPROVAL-RECOVERY-ONCE.txt")
        }
        assert.deepEqual(retainedInteractionRequests(devicePath.deviceData, job.job_id), originalRequests)
        assertExactInteractionOutcome(database, devicePath.deviceData, job.job_id, authorityBefore, settled.worker.frame)
        assertFrozenChatFile(devicePath.deviceData, job.job_id, 'CHAT-APPROVAL-RECOVERY-ONCE.txt', 'once\n')
        assert.deepEqual(settled.accepted.payload.message, settled.worker.frame)
        const httpCount = devicePath.modelServer.requests.length
        assert.deepEqual((await api.command('approval.decide', original.revision, payload, decisionRequestId)).result, receipt.result)
        await assert.rejects(api.command('approval.decide', original.revision, { ...payload, decision: 'reject' }, decisionRequestId),
          error => error.status === 409)
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        assert.deepEqual(privateApprovalOperations(devicePath.deviceData, job.job_id), [settled.operation])
        assertFrozenChatFile(devicePath.deviceData, job.job_id, 'CHAT-APPROVAL-RECOVERY-ONCE.txt', 'once\n')
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM scheduler_execution_jobs WHERE product_session_id=?').get(productSessionId).n, 1)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(job.job_id).n, 1)
        return { jobId: job.job_id, point: 'pending-approval-before-decision', approvalId: original.id,
          sameCoreTurnId: coreAfter.submissionId, operationRequestDigest: settled.operation.request_digest,
          toolCalls: coreAfter.commands.length, originalDecisionApplied: true, businessAcknowledged: true,
          originalInteractionDeadline: original.expiresAt, renewedLeaseDeadline: renewed.expires_at, successfulAfterRealRenewal: true }
      } finally {
        device.close()
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.originalDecisionApplied, true)
  assert.equal(report.flow.scenario.businessAcknowledged, true)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})

function retryStoreSnapshot(provider, jobId) {
  const rows = provider.prepare(`SELECT r.*,e.digest,e.request_open,e.prepared_payload,e.prepared_payload_digest,
    e.chunks,e.cancelled FROM model_retry_state r JOIN exchanges e ON e.exchange_id=r.exchange_id`).all()
    .filter(row => JSON.parse(row.request_open).lease.jobId === jobId)
  assert.ok(rows.length <= 1, 'one original Core exchange owns this pending retry')
  if (!rows.length) return false
  const row = rows[0]
  const open = JSON.parse(row.request_open)
  assert.equal(row.digest, createHash('sha256').update(row.request_open).digest('hex'))
  assert.ok(row.prepared_payload?.length > 0, 'the exact original prepared input is durable')
  assert.equal(row.prepared_payload_digest, 'sha256:' + createHash('sha256').update(row.prepared_payload).digest('hex'))
  const attempts = provider.prepare('SELECT * FROM model_invocation_attempts WHERE exchange_id=? ORDER BY attempt_number')
    .all(row.exchange_id)
  return { row, open, attempts }
}

function providerEntryLines(deviceData, open) {
  return globSync('**/model-intake.log', { cwd: deviceData }).flatMap(path => readFileSync(join(deviceData, path), 'utf8')
    .split('\n').filter(line => line.includes(`exchange=${open.modelExchangeId} `)
      && line.includes(`request_id=${open.requestId} `)))
}

function assertOriginalRetryStillPending(provider, original) {
  const actual = retryStoreSnapshot(provider, original.open.lease.jobId)
  assert.ok(actual)
  assert.deepEqual(actual.row, original.row, 'reopened entry preserves original wake, policy, request and prepared bytes')
  assert.deepEqual(actual.attempts, original.attempts, 'full slots create no physical attempt or accounting rows')
}

async function originalTerminalAcknowledged(database, deviceData, api, submission, expectedStatus) {
  return waitFor(async () => {
    const jobId = submission.job.job_id
    const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(jobId)
    const queue = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId)
    const slot = database.prepare('SELECT * FROM worker_session_slots WHERE job_id=?').get(jobId)
    const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId)
    const accepted = acceptedOutcome(database, jobId)
    const [worker] = retainedTerminal(deviceData, jobId)
    const outcomes = readRetainedOutcomes(deviceData, jobId)
    const acknowledged = outcomes.length === 1 && outcomes[0].workerAcknowledged
    const messages = (await api.query('session.messages.list', { productSessionId: submission.productSessionId })).result.items
    const expectedQueue = expectedStatus === 'cancelled' ? 'failed' : expectedStatus
    return queue?.state === expectedQueue && terminal?.outcome === expectedStatus && slot?.state === expectedStatus
      && reservation?.state === 'released' && accepted?.revision === 2 && accepted.payload.acknowledgementReady
      && acknowledged && worker && !worker.delivery
      && messages.some(message => message.role === 'assistant' && message.state === expectedStatus)
      ? { terminal, queue, slot, reservation, accepted, worker, messages, outcome: outcomes[0] } : false
  }, `original ${expectedStatus} retry execution has real terminal, cleanup and business ACK`, 120_000)
}

function assertOriginalUnknownAccounting(database, terminal, calls) {
  const jobId = terminal.queue.job_id
  const usage = terminal.worker.frame.outcome.usage
  const knownTokens = calls.flatMap(call => call.providerAttempts).reduce((sum, attempt) => sum + (attempt.usage?.totalTokens ?? 0), 0)
  assert.equal(usage.accountingStatus, 'unknown', 'an unmeasured original HTTP failure remains unknown after recovery')
  assert.equal(usage.tokens ?? null, null)
  assert.equal(usage.costMicrounits ?? null, null)
  assert.equal(usage.knownTokens, knownTokens)
  assert.equal(terminal.reservation.actual_tokens, null)
  assert.equal(terminal.reservation.actual_cost_microunits, null)
  assert.equal(terminal.reservation.release_reason, terminal.terminal.outcome === 'cancelled' ? 'cancelled' : 'failed')
  const retainedUsage = database.prepare('SELECT usage_json FROM execution_attempt_terminal_usage WHERE job_id=? AND attempt=1').get(jobId)
  if (!retainedUsage) return false
  assert.deepEqual(JSON.parse(retainedUsage.usage_json), usage)
  const reconciliation = database.prepare('SELECT * FROM execution_admission_reconciliation WHERE job_id=?').get(jobId)
  if (!reconciliation) return false
  assert.equal(reconciliation.accounting_status, 'pending')
  if (!reconciliation.terminal_usage_json) return false
  assert.deepEqual(JSON.parse(reconciliation.terminal_usage_json), usage)
  assert.equal(reconciliation.settlement_usage_json, null)
  const manifest = calls.map(call => 'primary:' + call.exchangeId).sort()
  const statement = database.prepare('SELECT statement_digest,statement_json FROM execution_attempt_accounting_facts WHERE job_id=? AND attempt=1')
    .all(jobId).map(row => {
      assert.equal(row.statement_digest, 'sha256:' + createHash('sha256').update(row.statement_json).digest('hex'))
      return { ...row, decoded: JSON.parse(row.statement_json) }
    }).find(row => isDeepStrictEqual(row.decoded.manifest, manifest))
  if (!statement) return false
  const totals = database.prepare('SELECT * FROM execution_attempt_accounting_totals WHERE job_id=? AND attempt=1').get(jobId)
  if (!totals) return false
  assert.equal(totals.tokens, null)
  assert.equal(totals.known_tokens, knownTokens)
  assert.equal(totals.cost, null)
  return { knownTokens, manifest, statementDigest: statement.statement_digest, aggregateAccounting: 'unknown' }
}

test('Chat reopens the same durable Provider retry in a fresh Store and thread while its original live Core waits under three real slots', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-same-exchange-reopened-retry-'))
  let complete = false
  t.after(() => {
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed same-exchange Provider recovery retained at ${directory}`)
  })
  const markers = ['reopened-retry-success', 'reopened-retry-cancel', 'reopened-held-one', 'reopened-held-two', 'reopened-held-three']
  const ids = markers.map((_, index) => `psn_${String(110 + index).padStart(26, '0')}`)
  const requestIds = markers.map((_, index) => `req_${String(110 + index).padStart(26, '0')}`)
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '10' },
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: markers,
      httpFailures: Object.fromEntries(markers.slice(0, 2).map(marker => [marker, { status: 429, count: 1, retryAfterSeconds: 30 }])),
      nativeCellScripts: { 'native-cell-A': 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command tool missing"); text(await tools[command.name]({cmd:"printf \'once\\n\' >> CHAT-REOPENED-ONCE.txt",yield_time_ms:1000}));' },
    },
    scenario: { async run({ api, modelRoute, devicePath }) {
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      const device = new DatabaseSync(join(devicePath.deviceData, 'device-client.sqlite3'), { readOnly: true })
      const provider = new DatabaseSync(join(devicePath.deviceData, 'providers/runtime/provider-runtime.sqlite3'), { readOnly: true })
      for (const store of [database, device, provider]) store.exec('PRAGMA busy_timeout=5000')
      const server = devicePath.modelServer
      const submitted = []
      const originals = []
      const submitOriginal = async index => {
        const payload = { productSessionId: ids[index], message: index === 0 ? `native-cell-A ${markers[index]}` : markers[index] }
        const receipt = await api.command('chat.submit', 1, payload, requestIds[index])
        const jobs = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').all(requestIds[index])
        assert.equal(jobs.length, 1)
        submitted[index] = { productSessionId: ids[index], payload, receipt, job: jobs[0] }
      }
      try {
        for (let index = 0; index < markers.length; index++) {
          await api.command('session.create', 0, { productSessionId: ids[index],
            projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
            title: `Fresh Provider retry ${index}`, modelRoute })
          await devicePath.launchAnchor({ productSessionId: ids[index] })
        }
        for (let index = 0; index < 2; index++) await submitOriginal(index)
        for (let index = 0; index < 2; index++) {
            const job = submitted[index].job
            const retry = await waitFor(() => {
              const value = retryStoreSnapshot(provider, job.job_id)
              return value && value.row.wake_ms > Date.now() + 5000
                && providerEntryLines(devicePath.deviceData, value.open).some(line => line.includes('stage=provider_retry_deferred')
                  && line.includes('old_thread_joined=true old_store_closed=true')) ? value : false
            }, 'original real HTTP failure yields its durable retry and closes its old thread/Store', 60_000)
            assert.deepEqual(retry.attempts.map(row => row.state), ['failed'])
            assert.equal(retry.row.chunks, null)
            assert.equal(retry.row.policy_attempt, 2)
            const run = await waitFor(() => retainedRuns(devicePath.deviceData, job.job_id)[0], 'original live Core run', 30_000)
            const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
            originals[index] = { ...retry, run: run.run, core: coreExecution(run.run, devicePath.deviceData),
              authority, process: actualWorkerProcess(device, authority) }
            assert.equal(originals[index].core.users, 1)
            assert.deepEqual(originals[index].core.turns, [originals[index].core.submissionId])
            assert.equal(originals[index].core.commands.length, 0)
            assert.equal(server.requests.filter(request => request.gateMarker === markers[index]).length, 1)
        }
        for (let index = 2; index < markers.length; index++) await submitOriginal(index)
        for (let index = 2; index < markers.length; index++) {
            await waitFor(() => server.requests.some(request => request.gateMarker === markers[index] && request.active),
              'actual Provider request retains one physical slot', 60_000)
        }
        assert.equal(server.activeRequests, 3)
        assert.equal(server.peakActiveRequests, 3)
        for (let index = 0; index < 2; index++) {
          const original = originals[index]
          await waitFor(() => providerEntryLines(devicePath.deviceData, original.open)
            .filter(line => line.includes('stage=provider_store_opened')).some(line => line.includes('retry_entry=true')),
          'the original durable retry enters a new actual ProviderStore/thread while three HTTP slots stay full', 60_000)
          assert.ok(Date.now() >= original.row.wake_ms)
          assertOriginalRetryStillPending(provider, original)
          const entries = providerEntryLines(devicePath.deviceData, original.open).filter(line => line.includes('stage=provider_store_opened'))
          assert.equal(entries.length, 2)
          const providerThread = line => line.match(/provider_thread=(\S+)/u)[1]
          assert.notEqual(providerThread(entries[0]), providerThread(entries[1]))
          assert.equal(server.requests.filter(request => request.gateMarker === markers[index]).length, 1,
            'existing exchange reopened entry performs no second HTTP while three slots are occupied')
          const current = currentExecutionLease(database, original.authority, original.open.lease)
          assert.ok(Date.parse(current.expires_at) > Date.parse(original.authority.expires_at), 'real heartbeats renewed original authority during durable retry')
          const live = actualWorkerProcess(device, original.authority)
          for (const field of ['worker_session_id', 'worker_id', 'worker_instance_id', 'pid', 'data_directory']) assert.equal(live[field], original.process[field])
          assert.deepEqual(coreExecution(retainedRuns(devicePath.deviceData, original.open.lease.jobId)[0].run, devicePath.deviceData), original.core)
        }
        assert.equal(server.activeRequests, 3)
        const session = (await api.query('session.get', { productSessionId: ids[1] })).result
        const cancelPayload = { productSessionId: ids[1], reason: 'Cancel the original observed reopened Provider slot waiter' }
        const cancelId = 'req_00000000000000000000000120'
        const cancelled = await api.command('session.cancel', session.revision, cancelPayload, cancelId)
        const cancelledTerminal = await originalTerminalAcknowledged(database, devicePath.deviceData, api, submitted[1], 'cancelled')
        assert.equal(server.requests.filter(request => request.gateMarker === markers[1]).length, 1)
        assertOriginalRetryStillPending(provider, originals[0])
        server.releaseResponseGate(markers[2])
        await waitFor(() => server.requests.some(request => request.gateMarker === markers[0] && request.status === 200 && request.active),
          'same original exchange reacquires a real released slot after current authority and cancellation checks', 60_000)
        const physical = server.requests.filter(request => request.gateMarker === markers[0])
        assert.equal(physical.length, 2)
        assert.equal(physical[0].idempotencyKey, `device-${originals[0].open.modelExchangeId}:attempt:1`)
        assert.equal(physical[1].idempotencyKey, `device-${originals[0].open.modelExchangeId}:attempt:2`)
        assert.equal(physical[0].requestSha256, physical[1].requestSha256)
        const preparedThreadId = JSON.parse(Buffer.from(originals[0].row.prepared_payload).toString('utf8')).threadId
        assert.ok(typeof preparedThreadId === 'string' && preparedThreadId.length > 0)
        assert.ok(physical[0].threadIdSha256)
        assert.equal(physical[0].threadIdSha256, createHash('sha256').update(preparedThreadId).digest('hex'))
        assert.equal(physical[0].threadIdSha256, physical[1].threadIdSha256)
        const exchange = provider.prepare('SELECT * FROM exchanges WHERE exchange_id=?').get(originals[0].open.modelExchangeId)
        for (const field of ['digest', 'request_open', 'prepared_payload', 'prepared_payload_digest']) assert.deepEqual(exchange[field], originals[0].row[field])
        for (const marker of markers) server.releaseResponseGate(marker)
        const completedTerminal = await originalTerminalAcknowledged(database, devicePath.deviceData, api, submitted[0], 'completed')
        const cases = []
        for (const [index, terminal] of [[0, completedTerminal], [1, cancelledTerminal]]) {
          const original = originals[index]
          assert.equal(terminal.outcome.workerAcknowledged, true,
            'the consumed outbox has the exact normalized original terminal ACK')
          assert.deepEqual(terminal.outcome.bytes, terminal.worker.bytes)
          assert.equal(terminal.outcome.digest, terminal.worker.digest)
          assert.deepEqual(terminal.outcome.message, terminal.worker.frame)
          assert.equal(terminal.outcome.message.outcome.status, index === 0 ? 'succeeded' : 'cancelled')
          assert.deepEqual(terminal.accepted.payload.message, terminal.worker.frame)
          currentExecutionLease(database, original.authority, terminal.worker.frame.lease)
          assert.equal(terminal.queue.attempt, 1)
          assert.equal(terminal.slot.attempt, 1)
          assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(original.open.lease.jobId).n, 1)
          const finalProcess = actualWorkerProcess(device, original.authority, { terminalAcknowledged: true })
          for (const field of ['worker_session_id', 'worker_id', 'worker_instance_id', 'pid', 'data_directory']) assert.equal(finalProcess[field], original.process[field])
          const finalCore = coreExecution(terminal.worker.run, devicePath.deviceData)
          for (const field of ['submissionId', 'submissionDigest', 'kernelSessionId', 'codexThreadId']) assert.equal(finalCore[field], original.core[field])
          assert.equal(finalCore.users, 1)
          assert.deepEqual(finalCore.turns, original.core.turns)
          assert.equal(finalCore.commands.length, index === 0 ? 1 : 0)
          if (index === 0) {
            assertSuccessfulCoreCommand(finalCore.commands[0])
            for (const field of ['executionResult', 'acceptedResult']) {
              assert.equal(JSON.parse(JSON.parse(finalCore.commands[0][field]).effective_input.input).cmd,
                "printf 'once\n' >> CHAT-REOPENED-ONCE.txt")
            }
          }
          assert.equal(terminal.messages.filter(message => message.role === 'user').length, 1)
          assert.equal(terminal.messages.filter(message => message.role === 'assistant' && message.state === (index === 0 ? 'completed' : 'cancelled')).length, 1)
          const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === original.open.lease.jobId)
          const first = calls.find(call => call.exchangeId === original.open.modelExchangeId)
          assert.ok(first)
          assert.deepEqual(first.providerAttempts.map(attempt => attempt.state), index === 0 ? ['failed', 'completed'] : ['failed'])
          assert.equal(first.providerAttempts[0].usage, null)
          if (index === 0) assert.ok(first.providerAttempts[1].usage?.totalTokens > 0)
          assert.ok(calls.filter(call => call.exchangeId !== original.open.modelExchangeId).every(call => call.providerAttempts.length === 1 && call.providerAttempts[0].state === 'completed'))
          const accounting = await waitFor(() => assertOriginalUnknownAccounting(database, terminal, calls),
            'original physical manifest retains every attempt and exact unknown-accounting lower bound', 30_000)
          cases.push({ jobId: original.open.lease.jobId, modelExchangeId: original.open.modelExchangeId,
            status: terminal.terminal.outcome, originalWorkerPid: original.process.pid, originalCoreThread: finalCore.codexThreadId,
            reopenedProviderEntries: 2, firstExchangePhysicalAttempts: first.providerAttempts.length, ...accounting })
        }
        assertFrozenChatFile(devicePath.deviceData, originals[0].open.lease.jobId, 'CHAT-REOPENED-ONCE.txt', 'once\n')
        const count = server.requests.length
        assert.deepEqual((await api.command('chat.submit', 1, submitted[0].payload, requestIds[0])).result, submitted[0].receipt.result)
        assert.deepEqual((await api.command('session.cancel', session.revision, cancelPayload, cancelId)).result, cancelled.result)
        assert.equal(server.requests.length, count)
        assert.equal(server.requests.filter(request => request.gateMarker === markers[1]).length, 1)
        await waitFor(() => server.activeRequests === 0, 'all real HTTP slots release after original execution completion', 30_000)
        assert.equal(server.peakActiveRequests, 3)
        return { cases, peakHttp: server.peakActiveRequests, sameLiveWorkerAndCore: true,
          recoveryBoundary: 'old Provider invocation/thread/Store closed; fresh ProviderStore reopened original exchange; whole-process death not exercised' }
      } finally {
        for (const marker of markers) server.releaseResponseGate(marker)
        provider.close(); device.close(); database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.sameLiveWorkerAndCore, true)
  assert.equal(report.flow.scenario.peakHttp, 3)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})
