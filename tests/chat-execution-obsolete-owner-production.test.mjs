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
import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { assertSuccessfulRetainedCommands, readRetainedCoreCommandResults,
  readRetainedOutcomes, readRetainedRuntime, readWorkerTransport,
  runtimeLedgerSnapshot } from './fixtures/chat-execution-transport-probe.mjs'

function sealedBinaries() {
  const directory = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  assert.ok(directory, 'obsolete-owner acceptance requires explicitly sealed production binaries')
  return { build: false, serverBinary: join(directory, 'winwincode-server'),
    wwcBinary: join(directory, 'wwc') }
}

function runsFor(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) return []
      return store.prepare('SELECT record_json FROM codex_run').all().flatMap(row => {
        const bytes = Buffer.from(row.record_json)
        const run = JSON.parse(bytes.toString('utf8'))
        return run.job.jobId === jobId ? [{ bytes, run }] : []
      })
    } finally { store.close() }
  })
}

function originalAttemptSnapshot(deviceData, jobId) {
  const [original] = runsFor(deviceData, jobId).filter(value => value.run.job.attempt === 1)
  assert.ok(original?.run.rolloutPath)
  return { run: original.bytes, rollout: readFileSync(original.run.rolloutPath),
    commands: readRetainedCoreCommandResults(deviceData, original.run.kernelSessionId),
    runtime: readRetainedRuntime(deviceData, jobId).filter(value => value.message.lease.attempt === 1)
      .map(value => ({ sequence: value.sequence, digest: value.digest, bytes: value.bytes,
        ackSequence: value.ack_sequence, highestSequence: value.highest_sequence })),
    outcomes: readRetainedOutcomes(deviceData, jobId).filter(value => value.message.lease.attempt === 1)
      .map(value => ({ bytes: value.bytes, digest: value.digest, delivery: value.delivery, receipt: value.receipt })) }
}

function businessSnapshot(database, productSessionId, jobId) {
  return { queue: database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId),
    sessionJobs: database.prepare('SELECT * FROM scheduler_execution_jobs WHERE product_session_id=? ORDER BY job_id').all(productSessionId),
    sessionDispatches: database.prepare(`SELECT * FROM execution_dispatch_authorities WHERE job_id IN
      (SELECT job_id FROM scheduler_execution_jobs WHERE product_session_id=?) ORDER BY job_id`).all(productSessionId),
    terminal: database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=? ORDER BY attempt').all(jobId),
    accepted: database.prepare('SELECT stream_id,revision,payload FROM product_state WHERE stream_id IN (?,?) ORDER BY stream_id')
      .all(`execution-outcome:${jobId}:1`, `execution-outcome:${jobId}:2`),
    runtime: runtimeLedgerSnapshot(database, productSessionId),
    reservation: database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId),
    pendingAccounting: database.prepare('SELECT * FROM execution_attempt_accounting_pending WHERE job_id=? ORDER BY attempt').all(jobId),
    accountingFacts: database.prepare('SELECT * FROM execution_attempt_accounting_facts WHERE job_id=? ORDER BY attempt,statement_digest').all(jobId),
    accountingTotals: database.prepare('SELECT * FROM execution_attempt_accounting_totals WHERE job_id=? ORDER BY attempt').all(jobId) }
}

function assertObsoleteWorkerRejected(response) {
  assert.equal(response.status, 400, 'an exited Worker has no active Device route')
  assert.equal(response.rejectionBoundary, 'device-worker-route')
  assert.equal(response.acknowledgement, null, 'an obsolete Worker receives no business receipt')
  return 'device-worker-route'
}

function assertReplacementTerminal(database, oldAuthority, oldFacts, successorGrantId) {
  const jobId = oldAuthority.job_id
  const rows = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=? AND attempt=1').all(jobId)
  assert.equal(rows.length, 1, 'official replacement closes exactly one predecessor lease')
  const [terminal] = rows
  for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token']) {
    assert.equal(terminal[field], oldAuthority[field], `replacement closes the original ${field}`)
  }
  const recovery = database.prepare(`SELECT * FROM device_execution_recovery_intents
    WHERE job_id=? AND predecessor_attempt=1 AND successor_grant_id=?`).get(jobId, successorGrantId)
  const exit = database.prepare('SELECT * FROM worker_launch_grant_exits WHERE worker_launch_grant_id=?')
    .get(oldFacts.worker_launch_grant_id)
  const grant = database.prepare('SELECT worker_session_id,worker_instance_id FROM worker_launch_grants WHERE worker_launch_grant_id=?')
    .get(oldFacts.worker_launch_grant_id)
  assert.equal(exit.worker_session_id, grant.worker_session_id)
  assert.equal(exit.worker_instance_id, grant.worker_instance_id)
  assert.equal(exit.worker_instance_id, oldAuthority.worker_instance_id)
  assert.ok(Date.parse(exit.observed_at) <= Date.parse(recovery.applied_at))
  assert.equal(terminal.outcome, 'failed')
  assert.equal(terminal.terminal_at, recovery.applied_at)
  const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'
  const suffix = [...createHash('sha256')
    .update('winwincode.repository-scheduler.replacement-lease-terminal.v1').update(Buffer.from([0]))
    .update(`${jobId}\u001f2`).digest().subarray(0, 26)].map(byte => alphabet[byte & 31]).join('')
  assert.equal(terminal.request_id, `req_${suffix}`, 'the terminal uses the official replacement request identity')
  const request = { job_id: terminal.job_id, lease_id: terminal.lease_id, worker_id: terminal.worker_id,
    worker_instance_id: terminal.worker_instance_id, attempt: terminal.attempt, fencing_token: terminal.fencing_token,
    outcome: terminal.outcome, terminal_at: terminal.terminal_at, request_id: terminal.request_id }
  assert.equal(terminal.request_digest, `sha256:${createHash('sha256').update(JSON.stringify(request)).digest('hex')}`)
  const submitted = database.prepare(`SELECT response_json FROM scheduler_execution_job_receipts
    WHERE operation='submit' AND job_id=?`).all(jobId)
  assert.equal(submitted.length, 1, 'the failed predecessor retains exactly one original submission')
  const original = JSON.parse(submitted[0].response_json).job
  assert.equal(original.job_id, terminal.job_id)
  assert.equal(original.attempt, terminal.attempt)
  const queue = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId)
  assert.equal(queue.submission_request_id, original.submission_request_id,
    'the successor retries the original request after its predecessor attempt failed')
  assert.equal(database.prepare('SELECT 1 FROM product_state WHERE stream_id=?').get(`execution-outcome:${jobId}:1`), undefined,
    'replacement lease closure is not an accepted predecessor Worker outcome')
  return terminal
}

function assertAcceptedTerminalLeasePeriod(database, lease, authority, currentLease) {
  const period = { job_id: lease.jobId, lease_id: lease.leaseId,
    payload_digest: authority.payload_digest, worker_id: lease.workerId,
    worker_instance_id: lease.workerInstanceId, attempt: lease.attempt,
    fencing_token: lease.fencingToken, issued_at: lease.issuedAt, expires_at: lease.expiresAt }
  for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token', 'issued_at']) {
    assert.equal(period[field], authority[field], `the immutable terminal lease has the current stable ${field}`)
    assert.equal(period[field], currentLease[field], `the current Registry lease has the same stable ${field}`)
  }
  assert.equal(currentLease.payload_digest, period.payload_digest)
  const expiry = Date.parse(period.expires_at)
  const currentExpiry = Date.parse(currentLease.expires_at)
  assert.ok(Number.isFinite(expiry) && Number.isFinite(currentExpiry) && currentExpiry >= expiry)
  const accepted = database.prepare(`SELECT operation,request_id,response_json FROM execution_lease_request_receipts
    WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(lease.jobId, lease.expiresAt)
    .find(row => {
      const receipt = JSON.parse(row.response_json)
      assert.equal(receipt.status, 'accepted')
      return isDeepStrictEqual(receipt.lease, period)
    })
  assert.ok(accepted, 'the immutable terminal lease exactly matches a real accepted claim or renewal period')
  assert.deepEqual(JSON.parse(accepted.response_json).lease, period)
  return { operation: accepted.operation, requestId: accepted.request_id, expiresAt: lease.expiresAt }
}

for (const kind of ['runtime', 'outcome']) {
  test(`Chat obsolete owner ${kind} resends genuinely retained bytes through its original revoked transport identity`, async t => {
    const directory = mkdtempSync(join(tmpdir(), `wwc-chat-obsolete-owner-${kind}-`))
    const suffix = kind === 'runtime' ? '151' : '152'
    const productSessionId = `psn_01J00000000000000000000${suffix}`
    const requestId = `req_01J00000000000000000000${suffix}`
    const marker = `obsolete-owner-${kind}`
    let complete = false
    let paused = null
    t.after(() => {
      if (paused !== null) { try { process.kill(paused, 'SIGCONT') } catch { /* runner owns shutdown */ } }
      if (complete) rmSync(directory, { recursive: true, force: true })
      else t.diagnostic(`Failed obsolete-owner evidence retained at ${directory}`)
    })
    const report = await runApiProductionVertical({
      directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
      serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
      deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: [marker], nativeCellScripts: {
        'native-cell-A': 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command tool missing"); for(let index=0;index<16;index++) { const result=await tools[command.name]({cmd:"true",yield_time_ms:1000}); if(result.exit_code!==0) throw new Error("original command did not exit successfully"); await new Promise(resolve=>setTimeout(resolve,100)); } text("original retained owner calls completed");',
      } },
      scenario: { async run({ api, modelRoute, devicePath }) {
        const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
        database.exec('PRAGMA busy_timeout=5000')
        const stream = `product-session:${productSessionId}`
        try {
          await api.command('session.create', 0, { productSessionId,
            projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
            title: `Obsolete original ${kind} owner`, modelRoute })
          await devicePath.launchAnchor({ productSessionId })
          await api.command('chat.submit', 1, { productSessionId, message: `native-cell-A ${marker}` }, requestId)
          const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').all(requestId)
          const [job] = jobs()
          assert.equal(jobs().length, 1)
          await waitFor(() => devicePath.modelServer.requests.some(value => value.gateMarker === marker && value.active),
            'the original physical Provider response is held before the owner-cut setup', 60_000)
          const oldAuthority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
          const oldFacts = database.prepare('SELECT * FROM device_execution_current_facts WHERE job_id=?').get(job.job_id)
          const probe = readWorkerTransport(devicePath.deviceData, oldAuthority)
          assert.equal(Number(execFileSync('ps', ['-p', String(probe.pid), '-o', 'pgid='], { encoding: 'utf8' }).trim()), probe.pid)

          const acceptedOriginal = await waitFor(() => readRetainedRuntime(devicePath.deviceData, job.job_id)
            .find(value => value.message.lease.attempt === 1 && value.sequence <= value.ack_sequence),
          'one original runtime frame already has its real Worker business ACK', 60_000)
          if (kind === 'runtime') {
            database.exec(`CREATE TRIGGER pause_obsolete_runtime_insert BEFORE INSERT ON product_state
              WHEN NEW.stream_id='${stream}' BEGIN SELECT RAISE(ABORT,'refuse original owner runtime commit'); END`)
            database.exec(`CREATE TRIGGER pause_obsolete_runtime_update BEFORE UPDATE ON product_state
              WHEN NEW.stream_id='${stream}' BEGIN SELECT RAISE(ABORT,'refuse original owner runtime commit'); END`)
          } else database.exec(`CREATE TRIGGER pause_obsolete_terminal BEFORE INSERT ON execution_lease_terminals
            WHEN NEW.job_id='${job.job_id}' AND NEW.attempt=1 BEGIN SELECT RAISE(ABORT,'refuse original owner terminal commit'); END`)
          devicePath.modelServer.releaseResponseGate(marker)
          const newOriginal = await waitFor(() => kind === 'runtime'
            ? readRetainedRuntime(devicePath.deviceData, job.job_id)
              .find(value => value.message.lease.attempt === 1 && value.sequence > value.ack_sequence)
            : readRetainedOutcomes(devicePath.deviceData, job.job_id)
              .find(value => value.message.lease.attempt === 1 && value.delivery && !value.receipt),
          `a genuine new original ${kind} frame is retained before CP acceptance`, 120_000)
          process.kill(probe.pid, 'SIGSTOP')
          paused = probe.pid
          assert.notEqual(newOriginal.message.messageId, acceptedOriginal.message.messageId)
          assert.equal(database.prepare('SELECT 1 FROM product_state WHERE stream_id=?')
            .get(`execution-outcome:${job.job_id}:1`), undefined)
          process.kill(-probe.pid, 'SIGKILL')
          paused = null
          await waitFor(() => database.prepare('SELECT 1 FROM worker_launch_grant_exits WHERE worker_launch_grant_id=?')
            .get(oldFacts.worker_launch_grant_id), 'the Device supervisor observes the original process death', 60_000)
          const oldAfterDeath = originalAttemptSnapshot(devicePath.deviceData, job.job_id)
          if (kind === 'outcome') assertSuccessfulRetainedCommands(oldAfterDeath.commands, 16)
          database.exec('DROP TRIGGER IF EXISTS pause_obsolete_runtime_insert')
          database.exec('DROP TRIGGER IF EXISTS pause_obsolete_runtime_update')
          database.exec('DROP TRIGGER IF EXISTS pause_obsolete_terminal')
          const successor = await waitFor(() => {
            const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
            const current = jobs()[0]
            const run = runsFor(devicePath.deviceData, job.job_id).find(value => value.run.job.attempt === 2)
            return current?.attempt === 2 && authority?.attempt === 2 && run ? { authority, current, run } : false
          }, 'the trusted real process exit and original lease expiry authorize the successor attempt', 120_000)
          assert.equal(jobs().length, 1)
          assert.equal(successor.current.submission_request_id, job.submission_request_id)
          assert.notEqual(successor.authority.worker_session_id, oldAuthority.worker_session_id)
          assert.notEqual(successor.authority.worker_instance_id, oldAuthority.worker_instance_id)
          assert.equal(successor.authority.worker_id, oldAuthority.worker_id)
          assert.ok(BigInt(successor.authority.fencing_token) > BigInt(oldAuthority.fencing_token))
          const [oldRun] = runsFor(devicePath.deviceData, job.job_id).filter(value => value.run.job.attempt === 1)
          assert.notEqual(successor.run.run.canonicalThreadId, oldRun.run.canonicalThreadId)
          assert.ok(Date.now() >= Date.parse(oldAuthority.expires_at))
          const replacement = database.prepare(`SELECT r.successor_grant_id,g.worker_session_id,g.worker_instance_id
            FROM worker_replacement_launches r JOIN worker_launch_grants g ON g.worker_launch_grant_id=r.successor_grant_id
            WHERE r.predecessor_grant_id=?`).get(oldFacts.worker_launch_grant_id)
          const successorProbe = readWorkerTransport(devicePath.deviceData, successor.authority)
          assert.equal(replacement.worker_session_id, successorProbe.launchWorkerSessionId)
          assert.equal(replacement.worker_instance_id, successor.authority.worker_instance_id)
          assert.ok(database.prepare('SELECT 1 FROM worker_launch_grant_exits WHERE worker_launch_grant_id=?')
            .get(oldFacts.worker_launch_grant_id))
          assert.ok(database.prepare(`SELECT 1 FROM device_execution_recovery_intents
            WHERE job_id=? AND predecessor_attempt=1 AND successor_grant_id=? AND applied_at IS NOT NULL`)
            .get(job.job_id, replacement.successor_grant_id))
          const settled = await waitFor(async () => {
            const accepted = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
              .get(`execution-outcome:${job.job_id}:2`)
            const raw = accepted && JSON.parse(Buffer.from(accepted.payload).toString('utf8'))
            const worker = readRetainedOutcomes(devicePath.deviceData, job.job_id)
              .find(value => value.message.lease.attempt === 2)
            const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=? AND attempt=2').get(job.job_id)
            const slot = database.prepare('SELECT * FROM worker_session_slots WHERE worker_session_id=?')
              .get(successor.authority.worker_session_id)
            const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
            const reconciliation = database.prepare('SELECT * FROM execution_admission_reconciliation WHERE job_id=?').get(job.job_id)
            const messages = (await api.query('session.messages.list', { productSessionId })).result.items
            return jobs()[0]?.state === 'completed' && terminal?.outcome === 'completed'
              && slot?.state === 'completed' && ['settled', 'released'].includes(reservation?.state)
              && accepted?.revision === 2 && raw?.acknowledgementReady && worker?.workerAcknowledged
              && messages.some(value => value.role === 'assistant' && value.state === 'completed')
              ? { raw, worker, terminal, slot, reservation, reconciliation } : false
          }, 'the official successor completes its exact raw terminal and actual Worker ACK', 120_000)
          assert.deepEqual(settled.raw.message, settled.worker.message)
          assert.equal(settled.worker.message.outcome.status, 'succeeded')
          assert.equal(settled.worker.message.workerSessionId, successor.authority.worker_session_id)
          assert.equal(settled.worker.message.sessionIdentity.codexThreadId, successor.run.run.canonicalThreadId)
          for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token']) {
            assert.equal(settled.terminal[field], successor.authority[field],
              `the accepted terminal belongs to the exact official successor ${field}`)
          }
          const finalAuthority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
          const currentLease = database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(job.job_id)
          const acceptedTerminalLeasePeriod = assertAcceptedTerminalLeasePeriod(database,
            settled.worker.message.lease, finalAuthority, currentLease)
          assert.equal(settled.slot.worker_session_id, successor.authority.worker_session_id)
          assert.equal(settled.slot.codex_thread_id, successor.run.run.canonicalThreadId)
          for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token']) {
            assert.equal(settled.slot[field], successor.authority[field])
          }
          if (settled.reservation.state === 'released') {
            assert.equal(settled.reservation.release_reason, 'failed')
            assert.equal(settled.reservation.actual_tokens, null)
            assert.equal(settled.reservation.actual_cost_microunits, null)
            assert.equal(settled.reconciliation?.accounting_status, 'pending')
            assert.equal(settled.reconciliation.settlement_request_id, null)
            assert.equal(settled.reconciliation.settlement_usage_json, null)
            const usage = JSON.parse(settled.reconciliation.terminal_usage_json)
            assert.equal(usage.accountingStatus, 'unknown')
            assert.equal(usage.tokens ?? null, null)
            assert.equal(usage.costMicrounits ?? null, null)
          }
          process.kill(successorProbe.pid, 'SIGSTOP')
          paused = successorProbe.pid
          const before = businessSnapshot(database, productSessionId, job.job_id)
          const predecessorTerminal = assertReplacementTerminal(database, oldAuthority, oldFacts, replacement.successor_grant_id)
          assert.equal(before.sessionJobs.length, 1)
          const successorCoreBefore = runsFor(devicePath.deviceData, job.job_id).find(value => value.run.job.attempt === 2)
          const successorCommandsBefore = readRetainedCoreCommandResults(devicePath.deviceData, successorCoreBefore.run.kernelSessionId)
          assertSuccessfulRetainedCommands(successorCommandsBefore, 16)
          const successorRolloutBefore = readFileSync(successorCoreBefore.run.rolloutPath)
          const httpCount = devicePath.modelServer.requests.length
          const exactReceipt = await probe.postRetainedMessage(acceptedOriginal.bytes)
          const duplicateBoundary = assertObsoleteWorkerRejected(exactReceipt)
          const rejected = await probe.postRetainedMessage(newOriginal.bytes)
          const newWriteBoundary = assertObsoleteWorkerRejected(rejected)
          assert.deepEqual(businessSnapshot(database, productSessionId, job.job_id), before)
          assert.deepEqual(originalAttemptSnapshot(devicePath.deviceData, job.job_id), oldAfterDeath)
          assert.equal(devicePath.modelServer.requests.length, httpCount)
          assert.deepEqual(assertReplacementTerminal(database, oldAuthority, oldFacts, replacement.successor_grant_id), predecessorTerminal)
          const heartbeat = database.prepare('SELECT heartbeat_sequence FROM execution_workers WHERE worker_id=?')
            .get(successorProbe.workerId)
          process.kill(successorProbe.pid, 'SIGCONT')
          paused = null
          const successorPostProbe = await waitFor(() => {
            const current = database.prepare('SELECT worker_instance_id,heartbeat_sequence FROM execution_workers WHERE worker_id=?')
              .get(successorProbe.workerId)
            if (current?.worker_instance_id === successorProbe.workerInstanceId
                && current.heartbeat_sequence >= heartbeat.heartbeat_sequence + 2) {
              return { kind: 'heartbeats', count: current.heartbeat_sequence - heartbeat.heartbeat_sequence }
            }
            const exit = database.prepare('SELECT * FROM worker_launch_grant_exits WHERE worker_launch_grant_id=?')
              .get(replacement.successor_grant_id)
            if (!exit) return false
            assert.equal(exit.worker_instance_id, successorProbe.workerInstanceId)
            assert.equal(exit.worker_session_id, successorProbe.launchWorkerSessionId)
            assert.ok(Date.parse(exit.observed_at) >= Date.parse(settled.terminal.terminal_at))
            return { kind: 'process-exit', observedAt: exit.observed_at }
          }, 'the actual successor resumes formal heartbeats or completes its acknowledged managed execution', 30_000)
          assert.deepEqual(businessSnapshot(database, productSessionId, job.job_id), before)
          assert.deepEqual(originalAttemptSnapshot(devicePath.deviceData, job.job_id), oldAfterDeath)
          assert.deepEqual(assertReplacementTerminal(database, oldAuthority, oldFacts, replacement.successor_grant_id), predecessorTerminal)
          const successorCoreAfter = runsFor(devicePath.deviceData, job.job_id).find(value => value.run.job.attempt === 2)
          assert.deepEqual(successorCoreAfter.bytes, successorCoreBefore.bytes)
          assert.deepEqual(readFileSync(successorCoreAfter.run.rolloutPath), successorRolloutBefore)
          assert.deepEqual(readRetainedCoreCommandResults(devicePath.deviceData, successorCoreAfter.run.kernelSessionId),
            successorCommandsBefore)
          assert.equal(devicePath.modelServer.requests.length, httpCount)
          return { kind, jobId: job.job_id, predecessorAttempt: 1, successorAttempt: 2,
            newWriteBoundary, duplicateBoundary, oldWorkerCoreZeroWrite: true, successorCoreUnchanged: true,
            businessStateUnchanged: true, successorPostProbe,
            resumedSuccessorHeartbeatObserved: successorPostProbe.kind === 'heartbeats',
            acceptedTerminalLeasePeriod,
            predecessorReplacementTerminalUnchanged: true,
            slotState: settled.slot.state, reservationState: settled.reservation.state,
            aggregateAccountingStatus: settled.reconciliation?.accounting_status ?? null,
            extraHttp: 0, obsoleteWorkerRejected: true, predecessorAttemptFailed: true,
            originalRequestRetained: true, successorRetriesOriginalRequest: true }
        } finally {
          database.exec('DROP TRIGGER IF EXISTS pause_obsolete_runtime_insert')
          database.exec('DROP TRIGGER IF EXISTS pause_obsolete_runtime_update')
          database.exec('DROP TRIGGER IF EXISTS pause_obsolete_terminal')
          devicePath.modelServer.releaseResponseGate(marker)
          if (paused !== null) { try { process.kill(paused, 'SIGCONT') } catch { /* runner owns shutdown */ } paused = null }
          database.close()
        }
      } },
    })
    t.diagnostic(JSON.stringify(report.flow.scenario))
    complete = true
  })
}
