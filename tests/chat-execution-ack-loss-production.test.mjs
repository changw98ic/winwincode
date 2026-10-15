// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { globSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'

import { pauseManagedProcess } from './fixtures/managed-process-pause.mjs'
import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import {
  assertSuccessfulRetainedCommands, completedCommandScript, readRetainedCoreCommandResults,
  readRetainedOutcomes, readWorkerTransport,
} from './fixtures/chat-execution-transport-probe.mjs'

function sealedBinaries() {
  const directory = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  assert.ok(directory, 'production ACK loss acceptance requires explicit freshly sealed binaries')
  return { build: false, serverBinary: join(directory, 'winwincode-server'),
    wwcBinary: join(directory, 'wwc') }
}

function snapshot(database, jobId) {
  const row = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
    .get(`execution-outcome:${jobId}:1`)
  return {
    accepted: row ? { revision: row.revision, bytes: Buffer.from(row.payload),
      payload: JSON.parse(Buffer.from(row.payload).toString('utf8')) } : null,
    terminals: database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').all(jobId),
    reservation: database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId),
    sources: database.prepare('SELECT * FROM execution_admission_settlement_sources WHERE job_id=?').all(jobId),
    slot: database.prepare('SELECT * FROM worker_session_slots WHERE job_id=?').get(jobId),
    accounting: Object.fromEntries([
      'execution_admission_reconciliation', 'execution_admission_reconciliation_facts',
      'execution_attempt_accounting_pending', 'execution_attempt_accounting_manifests',
      'execution_attempt_provider_receipts', 'execution_attempt_accounting_facts',
      'execution_attempt_accounting_totals', 'execution_attempt_terminal_usage',
    ].map(table => [table, database.prepare(`SELECT * FROM ${table} WHERE job_id=? ORDER BY rowid`).all(jobId)])),
  }
}


test('Chat replays its original ready outcome after actual Device relay business ACK response loss and a Server crash', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-ready-ack-loss-'))
  const productSessionId = 'psn_01J00000000000000000000141'
  const requestId = 'req_01J00000000000000000000141'
  let complete = false
  let paused = null
  t.after(() => {
    if (paused !== null) { try { process.kill(paused, 'SIGCONT') } catch { /* fixture cleanup owns this process */ } }
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed ready ACK loss evidence retained at ${directory}`)
  })
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: {
      'native-cell-A': completedCommandScript('git rev-parse --verify HEAD'),
    } },
    scenario: { async run({ api, modelRoute, devicePath, restartServer }) {
      await api.command('session.create', 0, { productSessionId,
        projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
        title: 'Chat actual business ACK response loss', modelRoute })
      await devicePath.launchAnchor({ productSessionId })
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
      database.exec('PRAGMA busy_timeout=5000')
      try {
        database.exec(`CREATE TRIGGER pause_ready_ack_loss BEFORE INSERT ON outbox
          WHEN NEW.topic='execution-outcome.acknowledgement-ready.v1'
            AND json_extract(CAST(NEW.payload AS TEXT),'$.jobId') IN
              (SELECT job_id FROM scheduler_execution_jobs WHERE product_session_id='${productSessionId}')
          BEGIN SELECT RAISE(ABORT,'hold original acknowledgement readiness after real cleanup'); END`)
        const payload = { productSessionId, message: 'native-cell-A' }
        const submitted = await api.command('chat.submit', 1, payload, requestId)
        const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').all(requestId)
        const [job] = jobs()
        assert.equal(jobs().length, 1)
        assert.equal(job.attempt, 1)
        const original = await waitFor(() => {
          const outcomes = readRetainedOutcomes(devicePath.deviceData, job.job_id)
          const state = snapshot(database, job.job_id)
          return outcomes.length === 1 && outcomes[0].delivery?.acknowledgement_required === 1
            && state.accepted?.revision === 1 && !state.accepted.payload.acknowledgementReady
            && state.terminals.length === 1 && state.terminals[0].outcome === 'completed'
            && state.reservation?.state === 'settled' && state.sources.length === 1
            && state.slot?.state === 'completed'
            && state.accounting.execution_attempt_accounting_manifests.length === 1
            && state.accounting.execution_attempt_accounting_totals.length === 1
            && state.accounting.execution_attempt_terminal_usage.length === 1
            ? outcomes[0] : false
        }, 'the actual Worker completes cleanup and accounting while its original terminal still awaits business ACK', 120_000)
        assert.equal(original.message.outcome.status, 'succeeded')
        const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
        const probe = readWorkerTransport(devicePath.deviceData, authority)
        const runs = globSync('**/worker-codex.sqlite3', { cwd: devicePath.deviceData }).flatMap(path => {
          const adapter = new DatabaseSync(join(devicePath.deviceData, path), { readOnly: true })
          try {
            adapter.exec('PRAGMA busy_timeout=5000')
            if (!adapter.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) return []
            return adapter.prepare('SELECT record_json FROM codex_run').all().map(row =>
              JSON.parse(Buffer.from(row.record_json).toString('utf8'))).filter(value => value.job.jobId === job.job_id)
          } finally { adapter.close() }
        })
        assert.equal(runs.length, 1)
        const [run] = runs
        assert.equal(run.canonicalThreadId, original.message.sessionIdentity.codexThreadId)
        const commands = readRetainedCoreCommandResults(devicePath.deviceData, run.kernelSessionId)
        assertSuccessfulRetainedCommands(commands, 1)
        const rollout = readFileSync(run.rolloutPath)
        const events = rollout.toString('utf8').trim().split('\n').map(line => JSON.parse(line))
          .filter(item => item.type === 'event_msg').map(item => item.payload)
        assert.deepEqual(events.filter(event => ['task_started', 'turn_started'].includes(event.type))
          .map(event => event.turn_id), [run.submissionId])
        assert.equal(events.filter(event => event.type === 'user_message'
          || (event.type === 'item_completed' && event.item?.type === 'UserMessage')).length, 1)
        const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
        assert.ok(calls.length >= 2, 'the original model/tool/model loop has actual physical Provider receipts')
        const physicalReceipts = calls.reduce((sum, call) => sum + call.providerAttempts.length, 0)
        const beforeLoss = snapshot(database, job.job_id)
        assert.deepEqual(beforeLoss.accepted.payload.message, original.message)
        assert.equal(beforeLoss.accounting.execution_attempt_provider_receipts.length, physicalReceipts,
          'all actual physical receipts reach CP through the live accounting reconciler before it is frozen')
        const httpCount = devicePath.modelServer.requests.length
        await pauseManagedProcess(probe.pid, () => {
          const [outcome] = readRetainedOutcomes(devicePath.deviceData, job.job_id)
          return outcome?.delivery && !outcome.workerAcknowledged
        }, { timeoutMillis: 30_000 })
        paused = probe.pid
        await new Promise(resolve => setTimeout(resolve, 10_100))
        assert.deepEqual(snapshot(database, job.job_id), beforeLoss,
          'freezing the actual Worker leaves its accepted result and completed cleanup unchanged')
        const [stoppedOutcome] = readRetainedOutcomes(devicePath.deviceData, job.job_id)
        assert.deepEqual(stoppedOutcome.bytes, original.bytes)
        assert.ok(stoppedOutcome.delivery)
        assert.equal(stoppedOutcome.workerAcknowledged, false)
        database.exec('DROP TRIGGER pause_ready_ack_loss')
        const lost = await probe.postRetainedMessage(original.bytes, { loseResponseAfterHeaders: true })
        assert.equal(lost.httpStatus, 200)
        assert.equal(lost.status, null)
        assert.equal(lost.responseLostAfterHeaders, true)
        assert.equal(lost.acknowledgement, null)
        const committed = await waitFor(() => {
          const state = snapshot(database, job.job_id)
          return state.accepted?.revision === 2 && state.accepted.payload.acknowledgementReady
            && state.terminals.length === 1 && state.terminals[0].outcome === 'completed'
            && state.reservation?.state === 'settled' && state.sources.length === 1
            && state.slot?.state === 'completed'
            && state.accounting.execution_attempt_provider_receipts.length === physicalReceipts
            && state.accounting.execution_attempt_accounting_manifests.length === 1
            && state.accounting.execution_attempt_accounting_totals.length === 1
            && state.accounting.execution_attempt_terminal_usage.length === 1
            ? state : false
        }, 'CP persisted the ready original result and all cleanup before its actual response was lost', 10_000)
        assert.deepEqual(committed.accepted.payload.message, original.message)
        const [stillPending] = readRetainedOutcomes(devicePath.deviceData, job.job_id)
        assert.deepEqual(stillPending.bytes, original.bytes)
        assert.ok(stillPending.delivery)
        assert.equal(stillPending.workerAcknowledged, false, 'the paused actual Worker did not apply the lost business ACK')
        const restarted = await restartServer({ crash: true })
        assert.equal(restarted.signal, 'SIGKILL')
        assert.notEqual(restarted.previousPid, restarted.restartedPid)
        assert.equal(restarted.health, 'ready')
        assert.deepEqual(snapshot(database, job.job_id), committed,
          'Server reopening preserves the original ready receipt and resource facts')
        process.kill(probe.pid, 'SIGCONT')
        paused = null
        const acknowledged = await waitFor(() => {
          const [outcome] = readRetainedOutcomes(devicePath.deviceData, job.job_id)
          return outcome?.workerAcknowledged ? outcome : false
        }, 'the real resumed Worker applies its exact retained outcome business ACK', 60_000)
        assert.deepEqual(acknowledged.bytes, original.bytes)
        assert.deepEqual(snapshot(database, job.job_id), committed,
          'exact duplicate delivery creates no new terminal, settlement or accepted-result revision')
        assert.deepEqual(readRetainedCoreCommandResults(devicePath.deviceData, run.kernelSessionId), commands)
        assert.deepEqual(readFileSync(run.rolloutPath), rollout)
        assert.deepEqual(readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id), calls)
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        assert.equal(jobs().length, 1)
        assert.equal(jobs()[0].attempt, 1)
        const messages = (await api.query('session.messages.list', { productSessionId })).result.items
        assert.equal(messages.filter(message => message.role === 'user').length, 1)
        assert.equal(messages.filter(message => message.role === 'assistant' && message.state === 'completed').length, 1)
        assert.equal((await api.query('session.get', { productSessionId })).result.state, 'idle')
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        assert.deepEqual(snapshot(database, job.job_id), committed)
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        return { jobId: job.job_id, attempt: 1, workerInstanceId: probe.workerInstanceId,
          terminalDigest: original.digest, lostResponseAfterHeaders: true, committedRevision: 2,
          signal: restarted.signal, businessAcknowledged: true, toolCalls: 1, extraHttp: 0 }
      } finally {
        database.exec('DROP TRIGGER IF EXISTS pause_ready_ack_loss')
        if (paused !== null) { process.kill(paused, 'SIGCONT'); paused = null }
        database.close()
      }
    } },
  })
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})
