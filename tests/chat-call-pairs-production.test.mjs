// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { globSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { readRetainedOutcomes, readRetainedRuntime, readWorkerTransport } from './fixtures/chat-execution-transport-probe.mjs'

function readStores(deviceData, pattern, table, read) {
  return globSync(pattern, { cwd: deviceData }).sort().flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?").get(table)) return []
      return read(store)
    } finally { store.close() }
  })
}

function calls(deviceData, jobId, attempt) {
  const runs = readStores(deviceData, '**/worker-codex.sqlite3', 'codex_run', store =>
    store.prepare('SELECT record_json FROM codex_run').all().map(row => JSON.parse(Buffer.from(row.record_json).toString()))
      .filter(run => run.job.jobId === jobId && run.job.attempt === attempt))
  const threads = new Set(runs.map(run => run.kernelSessionId))
  const model = readStores(deviceData, '**/providers.sqlite3', 'exchanges', store =>
    store.prepare(`SELECT exchange_id,request_open,call_status,chunks,accounting_chunks FROM exchanges
      WHERE json_extract(request_open,'$.lease.jobId')=? AND json_extract(request_open,'$.lease.attempt')=? ORDER BY exchange_id`).all(jobId, attempt))
  const physical = readStores(deviceData, '**/providers.sqlite3', 'model_invocation_attempts', store =>
    store.prepare(`SELECT exchange_id,attempt_number,request_json,request_payload,state,call_status,failure_chunks,accounting_chunks
      FROM model_invocation_attempts WHERE json_extract(request_json,'$.lease.jobId')=? AND json_extract(request_json,'$.lease.attempt')=?
      ORDER BY exchange_id,attempt_number`).all(jobId, attempt))
  const tool = readStores(deviceData, '**/state_5.sqlite', 'tool_requests', store =>
    store.prepare(`SELECT r.sequence,r.thread_id,r.request_json,r.original_input,r.call_status,r.call_result,
      a.effective_input,a.execution,a.disposition,a.execution_result,a.accepted_result,a.rejection
      FROM tool_requests r LEFT JOIN tool_attempts a ON a.request_sequence=r.sequence ORDER BY r.sequence`).all()
      .filter(row => threads.has(row.thread_id)))
  return { runs, model, physical, tool }
}

function assertPairs(records) {
  for (const row of records.model) {
    assert.ok(row.request_open)
    assert.ok(['succeeded', 'failed', 'cancelled'].includes(row.call_status))
    assert.ok(row.chunks, 'every terminal model call has its result body')
    assert.equal(JSON.parse(row.request_open).modelExchangeId, row.exchange_id)
    assert.equal(JSON.parse(row.chunks).at(-1).modelExchangeId, row.exchange_id)
  }
  for (const row of records.physical) {
    assert.ok(row.request_json && row.request_payload.byteLength > 0)
    assert.ok(['succeeded', 'failed', 'cancelled'].includes(row.call_status))
    assert.ok(row.failure_chunks || row.accounting_chunks, 'each physical invocation has a result body')
    assert.equal(JSON.parse(row.request_json).modelExchangeId, row.exchange_id)
  }
  for (const row of records.tool) {
    assert.ok(row.original_input)
    assert.ok(['succeeded', 'failed', 'cancelled'].includes(row.call_status))
    assert.ok(row.call_result, 'every terminal tool call has its result body')
    if (row.execution) {
      assert.ok(row.effective_input)
      assert.ok(row.execution_result)
      assert.notEqual(row.execution, 'running')
      assert.notEqual(row.disposition, 'pending')
    }
  }
}

for (const kind of ['model', 'tool']) {
  test(`Chat ${kind} pending call fails after real Worker death and a new attempt retains complete call pairs`, async t => {
    const binaries = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
    assert.ok(binaries, 'call-pair acceptance requires sealed production binaries')
    const directory = mkdtempSync(join(tmpdir(), `wwc-chat-call-pairs-${kind}-`))
    let complete = false
    t.after(() => {
      if (complete) rmSync(directory, { recursive: true, force: true })
      else t.diagnostic(`Failed call-pair evidence retained at ${directory}`)
    })
    const suffix = kind === 'model' ? '161' : '162'
    const productSessionId = `psn_01J00000000000000000000${suffix}`
    const requestId = `req_01J00000000000000000000${suffix}`
    const marker = `call-pairs-${kind}`
    const script = 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command missing"); const result=await tools[command.name]({cmd:"sleep 5; printf call-pair-completed",yield_time_ms:10000}); if(result.exit_code!==0) throw new Error("command failed"); text("call-pair-completed");'
    const report = await runApiProductionVertical({
      directory, restart: false, repeat: false, includeStrongFlow: false, build: false,
      serverBinary: join(binaries, 'winwincode-server'), wwcBinary: join(binaries, 'wwc'),
      serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
      deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: kind === 'model' ? [marker] : [],
        nativeCellScripts: { 'native-cell-A': script } },
      scenario: { async run({ api, modelRoute, devicePath }) {
        const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
        database.exec('PRAGMA busy_timeout=5000')
        try {
          await api.command('session.create', 0, { productSessionId,
            projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
            title: `Call pair ${kind} recovery`, modelRoute })
          await devicePath.launchAnchor({ productSessionId })
          await api.command('chat.submit', 1, { productSessionId, message: `native-cell-A ${marker}` }, requestId)
          const [job] = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').all(requestId)
          assert.ok(job)
          const pending = await waitFor(() => {
            const saved = calls(devicePath.deviceData, job.job_id, 1)
            return (kind === 'model'
              ? saved.physical.some(row => row.state === 'invoking' && row.call_status === 'pending')
              : saved.tool.some(row => row.execution === 'running' && row.call_status === 'pending'
                && JSON.parse(row.request_json).tool_name.endsWith('exec_command'))) && saved
          }, 'the real model or tool invocation has durably saved its request and pending state', 60_000)
          const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
          if (kind === 'model') await waitFor(() => devicePath.modelServer.requests.some(request => request.gateMarker === marker),
            'the actual Provider has received the pending model request', 30_000)
          const current = database.prepare('SELECT * FROM device_execution_current_facts WHERE job_id=?').get(job.job_id)
          const probe = readWorkerTransport(devicePath.deviceData, authority)
          const retained = await waitFor(() => readRetainedRuntime(devicePath.deviceData, job.job_id).find(row => row.message.lease.attempt === 1),
            'an original Worker runtime frame is durably retained', 30_000)
          process.kill(-probe.pid, 'SIGKILL')
          await waitFor(() => database.prepare('SELECT 1 FROM worker_launch_grant_exits WHERE worker_launch_grant_id=?')
            .get(current.worker_launch_grant_id), 'the Device supervisor recovers calls and reports the actual process death', 60_000)
          const failed = calls(devicePath.deviceData, job.job_id, 1)
          assertPairs(failed)
          for (const before of pending.model) assert.equal(failed.model.find(row => row.exchange_id === before.exchange_id).request_open, before.request_open)
          for (const before of pending.tool) assert.equal(failed.tool.find(row => row.sequence === before.sequence).original_input, before.original_input)
          if (kind === 'model') {
            const interrupted = failed.physical.find(row => pending.physical.some(old => old.exchange_id === row.exchange_id && old.state === 'invoking'))
            assert.equal(interrupted.state, 'interrupted_unknown')
            assert.equal(interrupted.accounting_chunks, null, 'a killed in-flight response has unknown usage')
            assert.ok(interrupted.failure_chunks.includes('DEVICE_MODEL_INTERRUPTED'))
            devicePath.modelServer.releaseResponseGate(marker)
          } else {
            const interrupted = failed.tool.find(row => pending.tool.some(old => old.sequence === row.sequence && old.execution === 'running'))
            assert.equal(interrupted.call_status, 'failed')
            assert.equal(interrupted.execution, 'uncertain')
            assert.ok(interrupted.call_result.includes('unknown'))
          }
          const successor = await waitFor(() => {
            const jobs = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').all(requestId)
            const outcome = readRetainedOutcomes(devicePath.deviceData, job.job_id).find(row => row.message.lease.attempt === 2 && row.workerAcknowledged)
            return jobs.length === 1 && jobs[0].attempt === 2 && jobs[0].state === 'completed'
              && outcome?.message.outcome.status === 'succeeded' && outcome
          }, 'the official new attempt completes the original request with its actual Worker ACK', 120_000)
          const recovered = calls(devicePath.deviceData, job.job_id, 2)
          assertPairs(recovered)
          assert.ok(recovered.physical.some(row => row.call_status === 'succeeded'))
          assert.ok(recovered.tool.some(row => row.call_status === 'succeeded' && JSON.parse(row.request_json).tool_name.endsWith('exec_command')))
          assert.notEqual(recovered.runs[0].canonicalThreadId, failed.runs[0].canonicalThreadId)
          const oldTerminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=? AND attempt=1').all(job.job_id)
          assert.equal(oldTerminal.length, 1)
          assert.equal(oldTerminal[0].outcome, 'failed')
          for (let replay = 0; replay < 2; replay += 1) {
            assert.equal((await probe.postRetainedMessage(retained.bytes)).status, 400)
            assert.deepEqual(calls(devicePath.deviceData, job.job_id, 1), failed, 'late old messages cannot change any original call pair')
          }
          assert.equal(successor.message.workerSessionId, database.prepare('SELECT worker_session_id FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id).worker_session_id)
          return { kind, jobId: job.job_id, predecessorFailed: true, successorAttempt: 2,
            modelPairs: failed.model.length + recovered.model.length,
            physicalModelPairs: failed.physical.length + recovered.physical.length,
            toolPairs: failed.tool.length + recovered.tool.length, lateMessagesRejected: 2 }
        } finally {
          if (kind === 'model') devicePath.modelServer.releaseResponseGate(marker)
          database.close()
        }
      } },
    })
    t.diagnostic(JSON.stringify(report.flow.scenario))
    complete = true
  })
}
