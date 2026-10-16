// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { globSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { isDeepStrictEqual } from 'node:util'
import test from 'node:test'
import { pauseManagedProcess } from './fixtures/managed-process-pause.mjs'
import { runApiProductionVertical, waitForDeviceWorkerRegistered } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import { assertSuccessfulRetainedCommands, readRetainedCoreCommandResults, readRetainedOutcomes,
  readRetainedWorkerHeartbeats, readWorkerTransport } from './fixtures/chat-execution-transport-probe.mjs'

function sealedBinaries() {
  const directory = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  assert.ok(directory, 'dispatch/prepare acceptance requires explicit freshly sealed production binaries')
  return { build: false, serverBinary: join(directory, 'winwincode-server'), wwcBinary: join(directory, 'wwc') }
}

function openDatabase(path, readOnly = true) {
  const store = new DatabaseSync(path, { readOnly })
  store.exec('PRAGMA busy_timeout=5000')
  return store
}

function runs(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = openDatabase(join(deviceData, path))
    try {
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) return []
      return store.prepare('SELECT record_json FROM codex_run').all().flatMap(row => {
        const bytes = Buffer.from(row.record_json)
        const run = JSON.parse(bytes.toString('utf8'))
        if (run.job?.jobId !== jobId) return []
        const bindings = store.prepare("SELECT frame_digest,frame_json FROM execution_outbox WHERE family='session_binding'")
          .all().flatMap(value => {
            const bindingBytes = Buffer.from(value.frame_json)
            const binding = JSON.parse(bindingBytes.toString('utf8'))
            if (binding.lease.jobId !== jobId) return []
            assert.equal(value.frame_digest, 'sha256:' + createHash('sha256').update(bindingBytes).digest('hex'))
            return [{ bytes: bindingBytes, binding }]
          })
        return [{ bytes, run, bindings }]
      })
    } finally { store.close() }
  })
}

function driveReceipts(database, jobId) {
  return database.prepare(`SELECT * FROM repository_scheduler_drive_receipts
    WHERE json_extract(response_json,'$.job.job_id')=? ORDER BY rowid`).all(jobId)
    .map(row => ({ ...row, receipt: JSON.parse(row.response_json) }))
}


function assertClaimPeriod(database, receipt) {
  const records = database.prepare(`SELECT response_json FROM execution_lease_request_receipts
    WHERE operation='claim' AND job_id=? AND request_id=?`).all(receipt.job.job_id, receipt.request_id)
  assert.ok(records.some(row => {
    const record = JSON.parse(row.response_json)
    return record.status === 'accepted' && isDeepStrictEqual(record.lease, receipt.lease)
  }), 'the original full Registry claim period is accepted by its exact durable receipt')
}

test('Chat reoffers its original committed dispatch receipt after a Server crash before actual Adapter preparation', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-dispatch-before-prepare-'))
  const productSessionId = 'psn_01J00000000000000000000142'
  const requestId = 'req_01J00000000000000000000142'
  const marker = 'dispatch-prepare-recovery'
  let completed = false
  let paused = null
  t.after(() => {
    if (paused !== null) { try { process.kill(paused, 'SIGCONT') } catch { /* the Device owns process cleanup */ } }
    if (completed) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed dispatch/prepare evidence retained at ${directory}`)
  })
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: [marker], nativeCellScripts: {
      'native-cell-A': 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command tool missing"); const result=await tools[command.name]({cmd:"git rev-parse --verify HEAD",yield_time_ms:1000}); if(result.exit_code!==0) throw new Error("original command failed"); text(result);',
    } },
    scenario: { async run({ api, modelRoute, devicePath, restartServer }) {
      await api.command('session.create', 0, { productSessionId,
        projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
        title: 'Chat dispatch receipt before Adapter preparation', modelRoute })
      const launched = await devicePath.launchAnchor({ productSessionId })
      await waitForDeviceWorkerRegistered(api, launched, 60_000)
      const probe = readWorkerTransport(devicePath.deviceData, null, { launch: launched })
      assert.equal(probe.workerSessionId, null, 'a launch WorkerSession is not an execution WorkerSession')
      const database = openDatabase(join(directory, 'server-data/control-plane.sqlite3'), false)
      const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE product_session_id=? ORDER BY job_id').all(productSessionId)
      const notPrepared = jobId => {
        assert.equal(runs(devicePath.deviceData, jobId).length, 0, 'the actual Adapter has not prepared this Job')
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_dispatch_authorities WHERE job_id=?').get(jobId).n, 0)
        assert.equal(database.prepare(`SELECT COUNT(*) AS n FROM execution_lease_request_receipts
          WHERE operation='dispatch_result' AND job_id=?`).get(jobId).n, 0,
        'Worker prepare has not produced its accepted dispatch-result receipt')
      }
      try {
        const registered = database.prepare('SELECT * FROM execution_workers WHERE worker_id=?').get(probe.workerId)
        assert.equal(registered.worker_instance_id, probe.workerInstanceId)
        database.exec(`CREATE TRIGGER hold_dispatch_prepare_heartbeat BEFORE INSERT ON execution_heartbeats
          WHEN NEW.worker_id='${probe.workerId}' AND NEW.worker_instance_id='${probe.workerInstanceId}'
          BEGIN SELECT RAISE(ABORT,'hold actual pre-dispatch Worker heartbeat'); END`)
        const previousSequence = database.prepare('SELECT heartbeat_sequence FROM execution_workers WHERE worker_id=?')
          .get(probe.workerId).heartbeat_sequence
        const originalHeartbeat = await pauseManagedProcess(probe.pid, () => {
          const pending = readRetainedWorkerHeartbeats(probe.dataDirectory, probe.workerId, probe.workerInstanceId)
            .find(value => value.message.heartbeatSequence === previousSequence + 1)
          return pending?.delivery.acknowledgementRequired === 1 ? pending : false
        }, { timeoutMillis: 30_000 })
        paused = probe.pid
        await new Promise(resolve => setTimeout(resolve, 10_100))
        assert.deepEqual(originalHeartbeat.message.activeLeases, [])
        assert.equal(originalHeartbeat.message.capacity.runningJobs, 0)
        assert.ok(originalHeartbeat.message.capacity.availableSlots > 0)
        assert.equal(database.prepare(`SELECT COUNT(*) AS n FROM execution_heartbeats
          WHERE worker_id=? AND worker_instance_id=? AND heartbeat_sequence=?`)
          .get(probe.workerId, probe.workerInstanceId, originalHeartbeat.message.heartbeatSequence).n, 0)
        const modelCountBefore = devicePath.modelServer.requests.length
        const payload = { productSessionId, message: `native-cell-A ${marker}` }
        const submitted = await api.command('chat.submit', 1, payload, requestId)
        assert.equal(jobs().length, 1)
        const [queued] = jobs()
        assert.equal(queued.state, 'queued')
        assert.equal(queued.attempt, 1)
        notPrepared(queued.job_id)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_leases WHERE job_id=?').get(queued.job_id).n, 0)
        database.exec('DROP TRIGGER hold_dispatch_prepare_heartbeat')
        const forwarded = await probe.postRetainedMessage(originalHeartbeat.bytes, {
        })
        assert.equal(forwarded.status, 200)
        assert.equal(forwarded.frameAccepted, true)
        assert.equal(forwarded.acknowledgement?.status, 'accepted')
        const dispatches = forwarded.deliveries.filter(value => value.message.kind === 'job.dispatch'
          && value.message.job.jobId === queued.job_id)
        assert.equal(dispatches.length, 1, 'the real authenticated exchange offers the original dispatch control')
        const dispatch = dispatches[0].message
        const initial = await waitFor(() => {
          const receipts = driveReceipts(database, queued.job_id)
          const lease = database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(queued.job_id)
          const slot = database.prepare('SELECT * FROM worker_session_slots WHERE job_id=?').get(queued.job_id)
          return jobs()[0]?.state === 'leased' && receipts.length === 1 && lease && slot
            ? { job: jobs()[0], row: receipts[0], lease, slot } : false
        }, 'the original scheduler drive receipt, full claim period and leased Job are durable before Worker preparation', 10_000)
        assert.equal(initial.row.receipt.recovered, false)
        assert.equal(initial.row.receipt.replayed, false)
        assert.deepEqual(initial.row.receipt.lease, { ...initial.lease })
        assertClaimPeriod(database, initial.row.receipt)
        assert.equal(dispatch.messageId, initial.row.receipt.message_id)
        assert.equal(dispatch.requestId, initial.row.receipt.request_id)
        for (const [camel, snake] of [['jobId', 'job_id'], ['leaseId', 'lease_id'], ['workerId', 'worker_id'],
          ['workerInstanceId', 'worker_instance_id'], ['attempt', 'attempt'], ['fencingToken', 'fencing_token'],
          ['issuedAt', 'issued_at'], ['expiresAt', 'expires_at']]) assert.equal(dispatch.lease[camel], initial.lease[snake])
        assert.deepEqual(dispatch.job, JSON.parse(Buffer.from(initial.job.dispatch_payload).toString('utf8')))
        assert.equal(dispatch.sentAt, initial.lease.issued_at)
        const executionWorkerSessionId = initial.slot.worker_session_id
        for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token']) {
          assert.equal(initial.slot[field], initial.lease[field])
        }
        notPrepared(queued.job_id)
        assert.equal(devicePath.modelServer.requests.length, modelCountBefore)
        const pendingHeartbeat = readRetainedWorkerHeartbeats(probe.dataDirectory, probe.workerId, probe.workerInstanceId)
          .find(value => value.message.messageId === originalHeartbeat.message.messageId)
        assert.deepEqual(pendingHeartbeat.bytes, originalHeartbeat.bytes,
          'the paused actual Worker has not consumed even its original heartbeat ACK')
        const offersBefore = database.prepare('SELECT * FROM repository_scheduler_offers WHERE job_id=?').all(queued.job_id)
        assert.equal(offersBefore.length, 1)
        const messagesBefore = (await api.query('session.messages.list', { productSessionId })).result.items
        const restarted = await restartServer({ crash: true })
        assert.equal(restarted.signal, 'SIGKILL')
        assert.notEqual(restarted.previousPid, restarted.restartedPid)
        assert.equal(restarted.health, 'ready')
        assert.deepEqual(jobs(), [initial.job])
        assert.deepEqual(database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(queued.job_id), initial.lease)
        assert.deepEqual(driveReceipts(database, queued.job_id), [initial.row])
        assert.deepEqual((await api.query('session.messages.list', { productSessionId })).result.items, messagesBefore)
        notPrepared(queued.job_id)
        assert.equal(devicePath.modelServer.requests.length, modelCountBefore)
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        process.kill(probe.pid, 'SIGCONT')
        paused = null
        await waitFor(() => devicePath.modelServer.requests.some(value => value.gateMarker === marker && value.active),
          'the same resumed Worker prepares the original dispatch once and its first Core model request is held', 60_000)
        const recovered = driveReceipts(database, queued.job_id).filter(value => value.receipt.recovered)
        assert.equal(recovered.length, 1)
        assert.deepEqual(recovered[0].receipt.lease, { ...initial.lease })
        assert.equal(recovered[0].receipt.message_id, initial.row.receipt.message_id)
        assert.equal(recovered[0].receipt.request_id, initial.row.receipt.request_id)
        assert.equal(recovered[0].receipt.job.attempt, initial.job.attempt)
        assert.deepEqual(recovered[0].receipt.job.dispatch_payload, initial.row.receipt.job.dispatch_payload)
        const offers = database.prepare('SELECT * FROM repository_scheduler_offers WHERE job_id=? ORDER BY rowid').all(queued.job_id)
        assert.equal(offers.length, 2)
        assert.notEqual(offers[0].scheduler_generation, offers[1].scheduler_generation)
        assert.equal(offers[0].message_id, offers[1].message_id)
        const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(queued.job_id)
        assert.equal(authority.worker_id, probe.workerId)
        assert.equal(authority.worker_instance_id, probe.workerInstanceId)
        assert.equal(authority.dispatch_request_id, initial.row.receipt.request_id)
        const prepared = runs(devicePath.deviceData, queued.job_id)
        assert.equal(prepared.length, 1)
        assert.equal(prepared[0].bindings.length, 1)
        const originalBinding = prepared[0].bindings[0]
        assert.equal(originalBinding.binding.workerSessionId, executionWorkerSessionId)
        assert.deepEqual(originalBinding.binding.sessionIdentity, { productSessionId,
          workerSessionId: executionWorkerSessionId, codexThreadId: prepared[0].run.canonicalThreadId })
        assert.equal(prepared[0].run.job.jobId, queued.job_id)
        devicePath.modelServer.releaseResponseGate(marker)
        const settled = await waitFor(async () => {
          const accepted = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
            .get(`execution-outcome:${queued.job_id}:1`)
          const raw = accepted && JSON.parse(Buffer.from(accepted.payload).toString('utf8'))
          const outcomes = readRetainedOutcomes(devicePath.deviceData, queued.job_id)
          const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(queued.job_id)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(queued.job_id)
          const slot = database.prepare('SELECT * FROM worker_session_slots WHERE worker_session_id=?').get(executionWorkerSessionId)
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          return jobs()[0]?.state === 'completed' && accepted?.revision === 2 && raw?.acknowledgementReady
            && outcomes.length === 1 && outcomes[0].workerAcknowledged && terminal?.outcome === 'completed'
            && reservation?.state === 'settled' && slot?.state === 'completed'
            && messages.some(value => value.role === 'assistant' && value.state === 'completed')
            ? { raw, worker: outcomes[0], terminal, reservation, slot, messages } : false
        }, 'the original attempt completes one real Core turn, terminal and actual Worker business ACK', 120_000)
        assert.deepEqual(settled.raw.message, settled.worker.message)
        assert.equal(settled.worker.message.outcome.status, 'succeeded')
        assert.equal(settled.worker.message.workerSessionId, executionWorkerSessionId)
        assert.equal(settled.worker.message.sessionIdentity.workerSessionId, executionWorkerSessionId)
        assert.equal(settled.worker.message.sessionIdentity.productSessionId, productSessionId)
        for (const [camel, snake] of [['jobId', 'job_id'], ['leaseId', 'lease_id'], ['workerId', 'worker_id'],
          ['workerInstanceId', 'worker_instance_id'], ['attempt', 'attempt'], ['fencingToken', 'fencing_token'], ['issuedAt', 'issued_at']]) {
          assert.equal(settled.worker.message.lease[camel], initial.lease[snake])
        }
        for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token']) {
          assert.equal(settled.terminal[field], initial.lease[field])
          assert.equal(settled.slot[field], initial.lease[field])
        }
        const terminalLease = settled.worker.message.lease
        const terminalPeriod = { job_id: terminalLease.jobId, lease_id: terminalLease.leaseId,
          payload_digest: initial.lease.payload_digest, worker_id: terminalLease.workerId,
          worker_instance_id: terminalLease.workerInstanceId, attempt: terminalLease.attempt,
          fencing_token: terminalLease.fencingToken, issued_at: terminalLease.issuedAt, expires_at: terminalLease.expiresAt }
        const periods = database.prepare(`SELECT response_json FROM execution_lease_request_receipts
          WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
            AND json_extract(response_json,'$.lease.expires_at')=?`).all(queued.job_id, terminalPeriod.expires_at)
        assert.ok(periods.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, terminalPeriod)),
          'the raw terminal expiry belongs to the exact real accepted claim or renewal period')
        const currentLease = database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(queued.job_id)
        for (const field of Object.keys(terminalPeriod).filter(field => field !== 'expires_at')) {
          assert.equal(currentLease[field], terminalPeriod[field])
        }
        assert.ok(Date.parse(currentLease.expires_at) >= Date.parse(terminalPeriod.expires_at))
        const finalRuns = runs(devicePath.deviceData, queued.job_id)
        assert.equal(finalRuns.length, 1)
        const finalRun = finalRuns[0].run
        assert.equal(settled.worker.message.sessionIdentity.codexThreadId, finalRun.canonicalThreadId)
        assert.equal(settled.slot.codex_thread_id, finalRun.canonicalThreadId)
        assert.equal(initial.slot.codex_thread_id, finalRun.canonicalThreadId)
        assert.deepEqual(finalRuns[0].bindings, [originalBinding],
          'the actual Adapter retains the same immutable original session.binding bytes')
        assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(queued.job_id), authority)
        assert.equal(finalRun.kernelSessionId, prepared[0].run.kernelSessionId)
        assert.equal(finalRun.submissionId, prepared[0].run.submissionId)
        const events = readFileSync(finalRun.rolloutPath, 'utf8').trim().split('\n').map(line => JSON.parse(line))
          .filter(event => event.type === 'event_msg').map(event => event.payload)
        assert.deepEqual(events.filter(event => ['task_started', 'turn_started'].includes(event.type))
          .map(event => event.turn_id), [finalRun.submissionId])
        assert.equal(events.filter(event => event.type === 'user_message'
          || (event.type === 'item_completed' && event.item?.type === 'UserMessage')).length, 1)
        const commands = readRetainedCoreCommandResults(devicePath.deviceData, finalRun.kernelSessionId)
        assertSuccessfulRetainedCommands(commands, 1)
        assert.equal(settled.messages.filter(value => value.role === 'user').length, 1)
        assert.equal(settled.messages.filter(value => value.role === 'assistant' && value.state === 'completed').length, 1)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(queued.job_id).n, 1)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_admission_settlement_sources WHERE job_id=?').get(queued.job_id).n, 1)
        const calls = readDeviceExecutionReceipts(directory).calls.filter(value => value.jobId === queued.job_id)
        assert.ok(calls.length >= 2)
        assert.ok(calls.every(value => value.providerAttempts.length === 1 && value.providerAttempts[0].state === 'completed'))
        const httpCount = devicePath.modelServer.requests.length
        assert.equal(httpCount - modelCountBefore, calls.reduce((sum, value) => sum + value.providerAttempts.length, 0))
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        assert.deepEqual(readRetainedCoreCommandResults(devicePath.deviceData, finalRun.kernelSessionId), commands)
        assert.equal(jobs().length, 1)
        assert.equal(jobs()[0].attempt, 1)
        return { point: 'dispatch-receipt-before-adapter-prepare', jobId: queued.job_id, attempt: 1,
          messageId: initial.row.receipt.message_id, dispatchRequestId: initial.row.receipt.request_id,
          workerId: probe.workerId, workerInstanceId: probe.workerInstanceId,
          workerSessionId: executionWorkerSessionId, launchWorkerSessionId: probe.launchWorkerSessionId,
          fencingToken: initial.lease.fencing_token, turns: 1, commands: commands.length,
          physicalAttempts: httpCount - modelCountBefore, signal: restarted.signal,
          businessAcknowledged: settled.worker.workerAcknowledged }
      } finally {
        database.exec('DROP TRIGGER IF EXISTS hold_dispatch_prepare_heartbeat')
        devicePath.modelServer.releaseResponseGate(marker)
        if (paused !== null) { process.kill(paused, 'SIGCONT'); paused = null }
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.point, 'dispatch-receipt-before-adapter-prepare')
  t.diagnostic(JSON.stringify(report.flow.scenario))
  completed = true
})
