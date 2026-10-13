// SPDX-License-Identifier: Apache-2.0

// Exact live-Worker cut. The explicit debugger sidecar is acceptance infrastructure.
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, globSync, mkdtempSync, readFileSync, readlinkSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { isDeepStrictEqual } from 'node:util'
import { fileURLToPath } from 'node:url'
import { runApiProductionVertical, verifyApiProductionSourceSeal,
  waitForDeviceWorkerRegistered } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import { assertSuccessfulRetainedCommands, readRetainedCoreCommandResults,
  readRetainedOutcomes, readWorkerTransport } from './fixtures/chat-execution-transport-probe.mjs'

const PROTOCOL = 'wwc.submission-intent-debugger.v1'
const POLL = 'winwincode_codex::adapter::ProductionCodexAdapter::submit_kernel_turn::{{closure}}'
const digest = bytes => `sha256:${createHash('sha256').update(bytes).digest('hex')}`
const json = path => JSON.parse(readFileSync(path, 'utf8'))
const processStartTicks = pid => {
  const value = readFileSync(`/proc/${pid}/stat`, 'utf8')
  return value.slice(value.lastIndexOf(') ') + 2).split(' ')[19]
}

function openDatabase(path) {
  const store = new DatabaseSync(path, { readOnly: true })
  store.exec('PRAGMA busy_timeout=5000')
  return store
}

function retainedRuns(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = openDatabase(join(deviceData, path))
    try {
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) return []
      return store.prepare('SELECT run_key,record_json FROM codex_run').all().flatMap(row => {
        const bytes = Buffer.from(row.record_json)
        const run = JSON.parse(bytes.toString('utf8'))
        if (run.job.jobId !== jobId) return []
        const modelOpens = store.prepare("SELECT frame_json FROM execution_outbox WHERE family='model_open'")
          .all().map(value => JSON.parse(Buffer.from(value.frame_json).toString('utf8')))
          .filter(value => value.lease.jobId === jobId)
        const bindings = store.prepare("SELECT frame_digest,frame_json FROM execution_outbox WHERE family='session_binding'")
          .all().flatMap(value => {
            const frameBytes = Buffer.from(value.frame_json)
            const binding = JSON.parse(frameBytes.toString('utf8'))
            if (binding.lease.jobId !== jobId) return []
            assert.equal(value.frame_digest, digest(frameBytes))
            return [{ bytes: frameBytes, binding }]
          })
        return [{ runKey: row.run_key, bytes, run, modelOpens, bindings }]
      })
    } finally { store.close() }
  })
}

function coreFacts(run, deviceData) {
  assert.ok(run.rolloutPath, 'the original opened Core session has its exact allocated rollout path')
  const materialized = existsSync(run.rolloutPath)
  if (!materialized) {
    assert.equal(run.phase, 'submission_intent', 'only the observed pre-acceptance cut permits a deferred rollout')
    assert.deepEqual(globSync('**/rollout-*-' + run.kernelSessionId + '.jsonl', { cwd: deviceData }), [],
      'the exact original Core session has no materialized rollout elsewhere')
  }
  const rollout = materialized ? readFileSync(run.rolloutPath) : null
  const rows = rollout === null ? []
    : rollout.toString('utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line))
  const events = rows.filter(row => row.type === 'event_msg').map(row => row.payload)
  const databases = globSync('**/state_5.sqlite', { cwd: deviceData })
  assert.equal(databases.length, 1, 'the actual original Core state database is already open at the first-poll cut')
  const toolRequests = databases.flatMap(path => {
    const store = openDatabase(join(deviceData, path))
    try { return store.prepare('SELECT * FROM tool_requests WHERE thread_id=? ORDER BY sequence').all(run.kernelSessionId) }
    finally { store.close() }
  })
  return { materialized, rollout, toolRequests,
    turns: events.filter(event => ['task_started', 'turn_started'].includes(event.type)).map(event => event.turn_id),
    users: events.filter(event => event.type === 'user_message'
      || (event.type === 'item_completed' && event.item?.type === 'UserMessage')).length }
}

function acceptedLeasePeriod(database, stamp, authority) {
  const period = { job_id: stamp.jobId, lease_id: stamp.leaseId, payload_digest: authority.payload_digest,
    worker_id: stamp.workerId, worker_instance_id: stamp.workerInstanceId, attempt: stamp.attempt,
    fencing_token: stamp.fencingToken, issued_at: stamp.issuedAt, expires_at: stamp.expiresAt }
  for (const field of Object.keys(period).filter(field => field !== 'expires_at')) assert.equal(period[field], authority[field])
  const receipts = database.prepare(`SELECT response_json FROM execution_lease_request_receipts
    WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(period.job_id, period.expires_at)
  assert.ok(receipts.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, period)),
    'the original full terminal lease period has an actual accepted claim or renewal receipt')
  return period
}

function debuggerControl(directory) {
  assert.ok(directory?.startsWith('/build/'), 'the explicitly started sidecar needs one shared /build directory')
  assert.equal(statSync(directory).mode & 0o777, 0o700)
  const write = (name, value) => {
    const path = join(directory, `${name}.tmp`)
    writeFileSync(path, `${JSON.stringify({ protocol: PROTOCOL, ...value }, null, 2)}\n`, { mode: 0o600 })
    renameSync(path, join(directory, name))
  }
  const read = name => {
    if (existsSync(join(directory, 'error.json'))) assert.fail(json(join(directory, 'error.json')).error)
    const path = join(directory, name)
    if (!existsSync(path)) return false
    assert.equal(statSync(path).mode & 0o777, 0o600)
    const value = json(path)
    assert.equal(value.protocol, PROTOCOL)
    return value
  }
  return { write, read }
}

test('Chat keeps its durable SubmissionIntent across a Server crash at the first Adapter submission poll before Core acceptance', async t => {
  const installed = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  assert.ok(installed, 'submission-intent acceptance requires explicit freshly sealed production binaries')
  const verified = verifyApiProductionSourceSeal({ root: fileURLToPath(new URL('../', import.meta.url)),
    serverBinary: join(installed, 'winwincode-server'), helperExecutable: join(installed, 'winwincode-kernel-helper') })
  const control = debuggerControl(process.env.WWC_SUBMISSION_DEBUGGER_DIRECTORY)
  const ready = await waitFor(() => control.read('ready.json'), 'the explicit debugger sidecar has resolved the fresh ELF poll symbol', 30_000)
  const seal = json(join(installed, 'winwincode-api-production.source.json'))
  assert.deepEqual(ready.officialSourceSeal, verified.seal, 'the sidecar verified the official current source seal before resolving the fresh poll')
  assert.equal(ready.sourceSealSha256, digest(readFileSync(verified.path)))
  assert.equal(ready.workerSha256, seal.workerBinarySha256)
  assert.equal(ready.workerSha256, digest(readFileSync(join(installed, 'winwincode-worker'))))
  assert.equal(ready.pollName, POLL)
  assert.equal(ready.symbolicEntryVerified, true)
  assert.ok(Number.isSafeInteger(ready.symbolSize) && ready.symbolSize > 0)
  assert.equal(ready.pidNamespace, readlinkSync('/proc/self/ns/pid'))
  assert.equal(ready.networkNamespace, readlinkSync('/proc/self/ns/net'))
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-submission-before-core-'))
  const productSessionId = 'psn_01J00000000000000000000143'
  const requestId = 'req_01J00000000000000000000143'
  let completed = false
  let detached = false
  t.after(() => {
    if (!detached) control.write('abort.json', { requestId })
    if (completed) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed SubmissionIntent evidence retained at ${directory}`)
  })
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, build: false,
    serverBinary: join(installed, 'winwincode-server'), wwcBinary: join(installed, 'wwc'),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: {
      'native-cell-A': 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command tool missing"); const result=await tools[command.name]({cmd:"git rev-parse --verify HEAD",yield_time_ms:1000}); if(result.exit_code!==0) throw new Error("original command failed"); text(result);',
    } },
    scenario: { async run({ api, modelRoute, devicePath, restartServer }) {
      const database = openDatabase(join(directory, 'server-data/control-plane.sqlite3'))
      try {
        await api.command('session.create', 0, { productSessionId,
          projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
          title: 'Live Worker SubmissionIntent before Core submission', modelRoute })
        const launched = await devicePath.launchAnchor({ productSessionId })
        await waitForDeviceWorkerRegistered(api, launched, 60_000)
        const probe = readWorkerTransport(devicePath.deviceData, null, { launch: launched })
        assert.equal(digest(readFileSync(`/proc/${probe.pid}/exe`)), ready.workerSha256)
        const identity = execFileSync('ps', ['-p', String(probe.pid), '-o', 'lstart='], { encoding: 'utf8' }).trim()
        const startTicks = processStartTicks(probe.pid)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM scheduler_execution_jobs WHERE product_session_id=?').get(productSessionId).n, 0)
        const baselineHttp = devicePath.modelServer.requests.length
        control.write('arm.json', { requestId, pid: probe.pid, workerSha256: ready.workerSha256,
          workerId: probe.workerId, workerInstanceId: probe.workerInstanceId })
        const armed = await waitFor(() => control.read('armed.json'), 'the fresh symbolic first-poll breakpoint is armed on the actual original Worker before submit', 30_000)
        assert.equal(armed.pid, probe.pid)
        assert.equal(armed.requestId, requestId)
        assert.equal(armed.workerSha256, ready.workerSha256)
        assert.equal(armed.rawPollSymbol, ready.rawPollSymbol)
        assert.equal(armed.allStopMode, true)
        const payload = { productSessionId, message: 'native-cell-A' }
        const submitted = await api.command('chat.submit', 1, payload, requestId)
        const stopped = await waitFor(() => control.read('stopped.json'), 'the first actual async poll stops before reconcile_turn_exact executes', 30_000)
        assert.equal(stopped.pid, probe.pid)
        assert.equal(stopped.requestId, requestId)
        assert.equal(stopped.rawPollSymbol, ready.rawPollSymbol)
        assert.equal(stopped.workerSha256, ready.workerSha256)
        assert.equal(stopped.pc, armed.relocatedEntry)
        assert.equal(stopped.relocatedEntry, armed.relocatedEntry)
        assert.equal(stopped.pollHits, 1)
        assert.equal(stopped.allThreadsStopped, true)
        assert.ok(stopped.threads.length > 0 && stopped.threads.every(thread => ['t', 'T'].includes(thread.state)))
        assert.deepEqual(globSync('*', { cwd: `/proc/${probe.pid}/task` }).map(Number).sort((a, b) => a - b),
          stopped.threads.map(thread => thread.tid).sort((a, b) => a - b),
          'the independent OS thread inventory equals the complete stopped inferior')
        for (const thread of stopped.threads) {
          const status = readFileSync(`/proc/${probe.pid}/task/${thread.tid}/status`, 'utf8')
          assert.match(status, /^State:\s+[tT]\s/mu, 'the independent OS observation confirms each original thread is stopped')
          assert.match(status, new RegExp(`^TracerPid:\\s+${stopped.debuggerPid}$`, 'mu'))
        }
        const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').all(requestId)
        assert.equal(jobs().length, 1)
        const [job] = jobs()
        assert.equal(job.attempt, 1)
        const originals = retainedRuns(devicePath.deviceData, job.job_id)
        assert.equal(originals.length, 1)
        const [original] = originals
        assert.equal(original.run.phase, 'submission_intent')
        assert.match(original.run.submissionDigest, /^sha256:[a-f0-9]{64}$/u)
        assert.ok(original.run.submissionId && original.run.kernelSessionId && original.run.canonicalThreadId)
        assert.equal(original.run.currentTurnId ?? null, null)
        assert.equal(original.modelOpens.length, 0)
        const coreBefore = coreFacts(original.run, devicePath.deviceData)
        assert.deepEqual(coreBefore.turns, [])
        assert.equal(coreBefore.users, 0)
        assert.equal(coreBefore.toolRequests.length, 0)
        assert.equal(devicePath.modelServer.requests.length, baselineHttp)
        assert.deepEqual(readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id), [])
        const acceptedBefore = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id) ?? null
        assert.equal(acceptedBefore, null,
          'the first submission poll precedes forwarding the original retained dispatch acceptance to CP')
        const lease = database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(job.job_id)
        assert.equal(lease.worker_id, probe.workerId)
        assert.equal(lease.worker_instance_id, probe.workerInstanceId)
        assert.equal(original.bindings.length, 1)
        const originalBinding = original.bindings[0]
        assert.equal(originalBinding.binding.workerId, probe.workerId)
        assert.deepEqual(originalBinding.binding.sessionIdentity, { productSessionId,
          workerSessionId: originalBinding.binding.workerSessionId,
          codexThreadId: original.run.canonicalThreadId })
        assert.equal(originalBinding.binding.lease.workerId, lease.worker_id)
        assert.equal(originalBinding.binding.lease.workerInstanceId, lease.worker_instance_id)
        assert.equal(originalBinding.binding.lease.leaseId, lease.lease_id)
        assert.equal(originalBinding.binding.lease.attempt, lease.attempt)
        assert.equal(originalBinding.binding.lease.fencingToken, lease.fencing_token)
        const currentLeaseReceipts = database.prepare(`SELECT response_json FROM execution_lease_request_receipts
          WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
            AND json_extract(response_json,'$.lease.expires_at')=?`).all(job.job_id, lease.expires_at)
        assert.ok(currentLeaseReceipts.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, { ...lease })),
          'the original current full lease at the cut has its own accepted claim or renewal receipt')
        const expiry = Date.parse(lease.expires_at)
        const claim = JSON.parse(database.prepare(`SELECT response_json FROM execution_lease_request_receipts
          WHERE job_id=? AND operation='claim' AND json_extract(response_json,'$.status')='accepted' ORDER BY rowid LIMIT 1`).get(job.job_id).response_json).lease
        assert.equal(Date.parse(claim.expires_at) - Date.parse(claim.issued_at), 30_000)
        assert.ok(expiry - Date.now() > 5_000, 'the actual original period leaves room for Server restart without extending its deadline')
        const restarted = await restartServer({ crash: true })
        assert.equal(restarted.signal, 'SIGKILL')
        assert.notEqual(restarted.previousPid, restarted.restartedPid)
        assert.equal(restarted.health, 'ready')
        assert.deepEqual(jobs(), [job])
        assert.equal(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id) ?? null, acceptedBefore)
        assert.deepEqual(database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(job.job_id), lease)
        assert.deepEqual(retainedRuns(devicePath.deviceData, job.job_id), [original])
        assert.deepEqual(coreFacts(original.run, devicePath.deviceData), coreBefore)
        assert.equal(devicePath.modelServer.requests.length, baselineHttp)
        assert.equal(execFileSync('ps', ['-p', String(probe.pid), '-o', 'lstart='], { encoding: 'utf8' }).trim(), identity)
        assert.equal(processStartTicks(probe.pid), startTicks)
        assert.ok(Date.now() < expiry - 1_000, 'the stopped original Worker resumes before its actual unchanged lease expires')
        control.write('resume.json', { requestId })
        const resumed = await waitFor(() => control.read('resumed.json'), 'the sidecar detaches and continues the exact same live Worker', 5_000)
        detached = true
        assert.equal(resumed.pid, probe.pid)
        assert.equal(resumed.requestId, requestId)
        assert.equal(resumed.pollHits, 1)
        assert.equal(processStartTicks(probe.pid), startTicks)
        assert.ok(resumed.detachedAtMillis < expiry)
        assert.ok(resumed.detachedAtMillis - stopped.stoppedAtMillis < 15_000)
        const final = await waitFor(() => {
          const accepted = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?').get(`execution-outcome:${job.job_id}:1`)
          const raw = accepted && JSON.parse(Buffer.from(accepted.payload).toString('utf8'))
          const [worker] = readRetainedOutcomes(devicePath.deviceData, job.job_id)
          const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(job.job_id)
          const slot = database.prepare('SELECT * FROM worker_session_slots WHERE worker_session_id=?').get(originalBinding.binding.workerSessionId)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
          return jobs()[0]?.state === 'completed' && terminal?.outcome === 'completed'
            && accepted?.revision === 2 && raw?.acknowledgementReady && worker?.workerAcknowledged
            && slot?.state === 'completed' && reservation?.state === 'settled'
            ? { raw, worker, terminal, slot, reservation } : false
        }, 'the original submission completes one raw terminal and its real Worker business ACK', 120_000)
        assert.deepEqual(final.raw.message, final.worker.message)
        assert.equal(final.worker.message.outcome.status, 'succeeded')
        assert.equal(final.worker.message.sessionIdentity.productSessionId, productSessionId)
        assert.equal(final.worker.message.sessionIdentity.codexThreadId, original.run.canonicalThreadId)
        const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id)
        assert.ok(authority, 'the original actual Worker acceptance is committed by CP after resume')
        for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token', 'issued_at', 'payload_digest']) {
          assert.equal(authority[field], lease[field])
        }
        assert.equal(authority.worker_session_id, originalBinding.binding.workerSessionId)
        assert.equal(final.worker.message.workerSessionId, originalBinding.binding.workerSessionId)
        acceptedLeasePeriod(database, final.worker.message.lease, authority)
        for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token']) {
          assert.equal(final.terminal[field], authority[field])
          assert.equal(final.slot[field], authority[field])
        }
        assert.equal(final.slot.codex_thread_id, original.run.canonicalThreadId)
        const [after] = retainedRuns(devicePath.deviceData, job.job_id)
        assert.equal(retainedRuns(devicePath.deviceData, job.job_id).length, 1)
        assert.equal(readRetainedOutcomes(devicePath.deviceData, job.job_id).length, 1)
        assert.equal(after.runKey, original.runKey)
        for (const field of ['submissionId', 'submissionDigest', 'kernelSessionId', 'canonicalThreadId', 'jobDigest']) assert.equal(after.run[field], original.run[field])
        assert.deepEqual(after.run.job, original.run.job)
        assert.deepEqual(after.bindings, [originalBinding], 'the real original Worker binding remains immutable')
        const coreAfter = coreFacts(after.run, devicePath.deviceData)
        assert.deepEqual(coreAfter.turns, [original.run.submissionId])
        assert.equal(coreAfter.users, 1)
        const commands = readRetainedCoreCommandResults(devicePath.deviceData, after.run.kernelSessionId)
        assertSuccessfulRetainedCommands(commands, 1)
        for (const field of ['execution_result', 'accepted_result']) {
          assert.equal(JSON.parse(JSON.parse(commands[0][field]).effective_input.input).cmd, 'git rev-parse --verify HEAD')
        }
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?').get(job.job_id).n, 1)
        assert.equal(database.prepare('SELECT COUNT(*) AS n FROM execution_admission_settlement_sources WHERE job_id=?').get(job.job_id).n, 1)
        const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
        assert.ok(calls.length >= 2)
        for (const call of calls) assert.deepEqual(call.providerAttempts.map(attempt => attempt.state), ['completed'])
        const httpCount = devicePath.modelServer.requests.length
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        assert.equal(jobs().length, 1)
        assert.equal(jobs()[0].attempt, 1)
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        assert.deepEqual(readRetainedCoreCommandResults(devicePath.deviceData, after.run.kernelSessionId), commands)
        assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(job.job_id), authority)
        return { jobId: job.job_id, attempt: 1, workerPid: probe.pid, workerInstanceId: probe.workerInstanceId,
          submissionId: original.run.submissionId, submissionDigest: original.run.submissionDigest,
          kernelSessionId: original.run.kernelSessionId, workerSha256: ready.workerSha256,
          rawPollSymbol: ready.rawPollSymbol, relocatedEntry: stopped.pc, allStoppedThreads: stopped.threads.length,
          stoppedAtMillis: stopped.stoppedAtMillis, resumedAtMillis: resumed.detachedAtMillis,
          unchangedLeaseDeadline: lease.expires_at, initialCoreUsers: 0, initialCoreTurns: 0,
          initialCoreTools: 0, initialProviderHttpDelta: 0, initialProviderCalls: 0,
          finalCoreUsers: 1, finalCoreTurns: 1, finalCommands: 1,
          rawStatus: 'succeeded', businessAcknowledged: final.worker.workerAcknowledged,
          recoveryScope: 'same live Worker and embedded Core across Server restart' }
      } finally {
        if (!detached) control.write('abort.json', { requestId })
        database.close()
      }
    } },
  })
  t.diagnostic(JSON.stringify(report.flow.scenario))
  completed = true
})
