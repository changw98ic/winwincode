// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import test from 'node:test'
import { isDeepStrictEqual } from 'node:util'
import { globSync, mkdtempSync, rmSync } from 'node:fs'
import { createHash, randomBytes } from 'node:crypto'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { createCertificate, runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import { configuredDeviceModelRoute, deterministicDeviceProvider, seedDeviceLocalProvider,
  startDeterministicDeviceModelServer, waitFor } from '../scripts/device-production-fixture.mjs'

const COMMAND_HISTORY_SIZE = 512
const LEASE_SECONDS = 30
const CANCEL_DEADLINE_MS = 10_000

function scenarioDirectory(t, prefix) {
  const directory = mkdtempSync(join(tmpdir(), prefix))
  let completed = false
  t.after(() => {
    if (completed) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed Chat production scenario retained at ${directory}`)
  })
  return { directory, complete() { completed = true } }
}

function sealedBinaries() {
  const installed = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  return installed === undefined ? { build: false } : { build: false,
    serverBinary: join(installed, 'winwincode-server'),
    wwcBinary: join(installed, 'wwc') }
}

function openDatabase(path) {
  const database = new DatabaseSync(path, { readOnly: true })
  database.exec('PRAGMA busy_timeout=5000')
  return database
}

async function createChat(context, productSessionId, title, modelRoute = context.modelRoute) {
  await context.api.command('session.create', 0, { productSessionId,
    projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
    title, modelRoute })
  return context.devicePath.launchAnchor({ productSessionId })
}

function jobStore(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).map(path => join(deviceData, path))
    .find(path => {
      const database = openDatabase(path)
      try {
        if (!database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) return false
        return database.prepare('SELECT record_json FROM codex_run').all()
          .some(row => JSON.parse(Buffer.from(row.record_json).toString('utf8')).job?.jobId === jobId)
      } finally { database.close() }
    })
}

function modelOpens(deviceData, jobId) {
  const path = jobStore(deviceData, jobId)
  if (!path) return []
  const database = openDatabase(path)
  try {
    return database.prepare("SELECT frame_json FROM execution_outbox WHERE family='model_open'").all()
      .map(row => JSON.parse(Buffer.from(row.frame_json).toString('utf8')))
      .filter(frame => frame.lease.jobId === jobId)
  } finally { database.close() }
}

function leaseRecord(authority) {
  return Object.fromEntries(['job_id', 'lease_id', 'payload_digest', 'worker_id', 'worker_instance_id',
    'attempt', 'fencing_token', 'issued_at', 'expires_at'].map(field => [field, authority[field]]))
}

function assertAcceptedLeasePeriod(database, period, operation) {
  const receipts = database.prepare(`SELECT response_json FROM execution_lease_request_receipts
    WHERE job_id=? AND operation=? AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(period.job_id, operation, period.expires_at)
  assert.ok(receipts.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, period)),
    'the exact live lease period has its real accepted claim or renewal receipt')
}

function currentExecutionLease(database, authority) {
  const current = database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(authority.job_id)
  if (!current) return false
  const original = leaseRecord(authority)
  for (const field of Object.keys(original).filter(field => field !== 'expires_at')) {
    assert.equal(current[field], original[field], `renewal preserves the original ${field}`)
  }
  assertAcceptedLeasePeriod(database, original, 'claim')
  assertAcceptedLeasePeriod(database, leaseRecord(current), current.expires_at === original.expires_at ? 'claim' : 'renew')
  assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(authority.job_id), authority,
    'heartbeat renewal leaves the accepted dispatch authority immutable')
  return current
}

function acknowledgedCoreHistory(database, jobId) {
  const calls = new Map()
  const streams = new Set()
  const retained = database.prepare(`SELECT f.stream_key, f.sequence, f.event_id, f.digest, f.frame,
    h.ack_sequence FROM runtime_replay_frame f JOIN runtime_replay_head h ON h.stream_key=f.stream_key
    WHERE f.sequence<=h.ack_sequence ORDER BY f.stream_key,f.sequence`).all().filter(row => {
    const frame = JSON.parse(Buffer.from(row.frame).toString('utf8'))
    if (frame.lease.jobId !== jobId) return false
    streams.add(row.stream_key)
    const payload = frame.event.payload
    if (payload?.contentType !== 'application/vnd.winwincode.core-tool-fact+json') return false
    const source = JSON.parse(Buffer.from(payload.dataBase64, 'base64').toString('utf8'))
    const value = JSON.parse(source.factJson)
    const fact = value.fact
    const sequence = fact.request_sequence ?? fact.source_request_sequence
    const key = `${source.sourceThreadId}:${sequence}`
    if (value.kind === 'request') {
      const attempt = fact.attempt
      calls.set(key, { ...calls.get(key), toolName: fact.request.tool_name,
        ...(attempt === null ? {} : { execution: attempt.execution,
          disposition: attempt.disposition, delivery: attempt.delivery }),
        cellId: fact.request.cell_id })
    } else if (value.kind === 'sharing' && calls.has(key)) {
      calls.set(key, { ...calls.get(key), disposition: fact.disposition, delivery: fact.delivery })
    }
    return true
  })
  const completedCommands = [...calls.values()].filter(call => call.toolName.endsWith('exec_command')
    && call.execution === 'completed' && call.disposition === 'accepted' && call.delivery === 'offered'
    && typeof call.cellId === 'string' && call.cellId.length > 0)
  return { retained, streamKeys: [...streams], completedCommands }
}

function historyFingerprint(database, streamKeys) {
  const rows = streamKeys.flatMap(streamKey => database.prepare(`SELECT sequence,event_id,digest
    FROM runtime_replay_frame WHERE stream_key=? ORDER BY sequence`).all(streamKey))
  const heads = streamKeys.map(streamKey => database.prepare(`SELECT stream_key,ack_sequence,highest_sequence
    FROM runtime_replay_head WHERE stream_key=?`).get(streamKey))
  return { rows: rows.length, heads, digest: createHash('sha256').update(JSON.stringify(rows)).digest('hex') }
}

// Recover the exact renewal request only when its durable receipt digest matches.
// issued_at remains the original claim time; sent_at starts the renewed period.
function acceptedRenewalPeriod(database, period) {
  const rows = database.prepare(`SELECT operation,request_id,request_digest,response_json
    FROM execution_lease_request_receipts WHERE job_id=? AND operation IN ('claim','renew')
      AND json_extract(response_json,'$.status')='accepted' ORDER BY rowid`).all(period.job_id)
  let previous
  for (const row of rows) {
    const lease = JSON.parse(row.response_json).lease
    if (row.operation === 'renew' && isDeepStrictEqual(lease, period)) {
      assert.ok(previous, 'a real renewal follows its original accepted lease period')
      const sentAt = new Date(Date.parse(lease.expires_at) - LEASE_SECONDS * 1000).toISOString()
      // Field order is ExecutionLeaseRenewal's canonical serde serialization.
      const request = { expires_at: lease.expires_at, fencing_token: lease.fencing_token,
        job_id: lease.job_id, lease_id: lease.lease_id, message_id: `xmsg_${row.request_id.slice(4)}`,
        prior_expires_at: previous.expires_at, request_id: row.request_id, sent_at: sentAt,
        worker_id: lease.worker_id, worker_instance_id: lease.worker_instance_id, attempt: lease.attempt }
      const digest = `sha256:${createHash('sha256').update(JSON.stringify(request)).digest('hex')}`
      assert.equal(digest, row.request_digest,
        'the exact accepted renewal request proves a thirty second period from its real sent_at')
      assert.ok(Date.parse(sentAt) < Date.parse(previous.expires_at),
        'the accepted renewal request was sent before the preceding real deadline')
      return { requestId: row.request_id, requestDigest: digest, sentAt,
        priorExpiresAt: previous.expires_at, expiresAt: lease.expires_at }
    }
    previous = lease
  }
  assert.fail('the measured period has no exact accepted renewal receipt')
}

function historyPrefix(database, streamKeys, boundaries = undefined) {
  const heads = boundaries ?? streamKeys.map(streamKey => database.prepare(`SELECT stream_key,
    ack_sequence,highest_sequence FROM runtime_replay_head WHERE stream_key=?`).get(streamKey))
  const rows = heads.flatMap(head => database.prepare(`SELECT stream_key,sequence,event_id,digest,frame
    FROM runtime_replay_frame WHERE stream_key=? AND sequence<=? ORDER BY sequence`)
    .all(head.stream_key, head.highest_sequence).map(row => ({ ...row,
      frame: Buffer.from(row.frame).toString('base64') })))
  return { heads, rows: rows.length, digest: createHash('sha256').update(JSON.stringify(rows)).digest('hex') }
}

function assertHistoryPrefix(database, streamKeys, original) {
  assert.deepEqual(historyPrefix(database, streamKeys, original.heads), original,
    'the original acknowledged history retains every sequence, event identity, digest and exact frame byte')
  for (const originalHead of original.heads) {
    const current = database.prepare('SELECT * FROM runtime_replay_head WHERE stream_key=?').get(originalHead.stream_key)
    assert.ok(current.ack_sequence >= originalHead.ack_sequence)
    assert.ok(current.highest_sequence >= originalHead.highest_sequence)
  }
}

function retainedOutcome(deviceData, jobId, attempt) {
  const path = jobStore(deviceData, jobId)
  if (!path) return null
  const database = openDatabase(path)
  try {
    const rows = database.prepare('SELECT frame_json FROM execution_terminal_outcome').all()
      .map(row => JSON.parse(Buffer.from(row.frame_json).toString('utf8')))
      .filter(frame => frame.lease.jobId === jobId && frame.lease.attempt === attempt)
    assert.ok(rows.length <= 1, 'one attempt retains at most one immutable Worker outcome')
    return rows[0] ?? null
  } finally { database.close() }
}

async function terminal(api, database, productSessionId, jobId, outcome, deviceData, timeout = 120_000) {
  const queueState = outcome === 'cancelled' ? 'failed' : outcome
  return waitFor(async () => {
    const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId)
    const result = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(jobId)
    const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId)
    const messages = (await api.query('session.messages.list', { productSessionId })).result.items
    const accepted = job && database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
      .get(`execution-outcome:${jobId}:${job.attempt}`)
    const raw = accepted && JSON.parse(Buffer.from(accepted.payload).toString('utf8'))
    const workerFrame = job && retainedOutcome(deviceData, jobId, job.attempt)
    const rawStatus = outcome === 'completed' ? 'succeeded' : outcome
    if (accepted?.revision === 2 && raw?.acknowledgementReady && workerFrame) {
      assert.deepEqual(raw.message, workerFrame, 'the accepted raw outcome is the exact immutable Worker frame')
      assert.equal(raw.message.outcome.status, rawStatus, 'raw outcome preserves cancellation and execution semantics')
    }
    return job?.state === queueState && result?.outcome === outcome
      && accepted?.revision === 2 && raw?.acknowledgementReady && workerFrame
      && reservation?.state === (outcome === 'cancelled' ? 'released' : 'settled')
      && messages.some(message => message.role === 'assistant' && message.state === outcome)
      ? { job, result, reservation, raw: raw.message } : false
  }, `the original ${outcome} execution settles its terminal and reservation`, timeout)
}

test('Chat honors live heartbeat and cancellation deadlines after real acknowledged Core tool history in the same Worker store', async t => {
  const fixture = scenarioDirectory(t, 'wwc-chat-real-history-load-')
  const { directory } = fixture
  const productSessionId = 'psn_01J00000000000000000000071'
  const historyRequestId = 'req_01J00000000000000000000071'
  const holdMarker = 'real-history-live-hold'
  const markerCodes = [...holdMarker].map(character => character.charCodeAt(0)).join(',')
  const historyScript = `// @exec: {"yield_time_ms":1000}
    const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));
    if(!command) throw new Error("command tool missing");
    for(let index=0;index<${COMMAND_HISTORY_SIZE};index++) {
      const result=await tools[command.name]({cmd:"true",yield_time_ms:1000});
      if(result.exit_code!==0) throw new Error("history command failed");
    }
    text({completedCommands:${COMMAND_HISTORY_SIZE},holdMarker:String.fromCharCode(${markerCodes})});`
  assert.equal(historyScript.includes(holdMarker), false,
    'the Provider hold marker is produced only by the completed real tool cell')
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, timeoutMillis: 300_000, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: String(LEASE_SECONDS) },
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: [holdMarker],
      nativeCellScripts: { 'native-cell-A': historyScript } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      const database = openDatabase(join(directory, 'server-data/control-plane.sqlite3'))
      let worker
      try {
        await createChat(context, productSessionId, 'Real acknowledged tool history load')
        await api.command('chat.submit', 1, { productSessionId, message: 'native-cell-A' }, historyRequestId)
        const live = await waitFor(() => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?')
          .get(historyRequestId), 'the historical execution exists', 60_000)
        const historicalStore = await waitFor(() => jobStore(devicePath.deviceData, live.job_id),
          'the real Worker retains the active historical execution', 60_000)
        worker = openDatabase(historicalStore)
        const originalAuthority = await waitFor(() => database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?')
          .get(live.job_id), 'the active historical execution has its original accepted authority', 60_000)
        const claimed = JSON.parse(database.prepare(`SELECT response_json FROM execution_lease_request_receipts
          WHERE job_id=? AND operation='claim' AND json_extract(response_json,'$.status')='accepted'
          ORDER BY rowid LIMIT 1`).get(live.job_id).response_json).lease
        assert.equal(Date.parse(claimed.expires_at) - Date.parse(claimed.issued_at), LEASE_SECONDS * 1000,
          'the real first accepted lease uses the unchanged thirty second production configuration')
        assert.deepEqual(claimed, leaseRecord(originalAuthority))
        const heldRequest = await waitFor(() => devicePath.modelServer.requests.find(request => request.gateMarker === holdMarker && request.active),
          'the same execution has its next actual HTTPS response held after the real command cell', 300_000)
        assert.equal(devicePath.modelServer.requests[0].gateMarker, null,
          'the first real Provider request is not held before the command history starts')
        assert.equal(devicePath.modelServer.requests.filter(request => request.gateMarker === holdMarker).length, 1)
        assert.ok(heldRequest.hasToolOutput, 'the hold follows real tool output from the completed command cell')
        const history = await waitFor(() => {
          const current = acknowledgedCoreHistory(worker, live.job_id)
          if (current.completedCommands.length !== COMMAND_HISTORY_SIZE) return false
          const heads = historyFingerprint(worker, current.streamKeys).heads
          const pending = current.streamKeys.reduce((count, streamKey) => count + worker.prepare(`SELECT COUNT(*) AS count
            FROM execution_outbox WHERE family='runtime' AND runtime_stream_key=? AND acknowledgement_required=1`)
            .get(streamKey).count, 0)
          return heads.length > 0 && heads.every(head => head.ack_sequence === head.highest_sequence)
            && pending === 0 ? current : false
        }, 'all 512 real completed commands are business acknowledged and compacted before measurement', 60_000, 250)
        assert.equal(history.completedCommands.length, COMMAND_HISTORY_SIZE,
          'all historical commands are actual completed nested Core operations with business ACK')
        assert.ok(history.retained.length >= COMMAND_HISTORY_SIZE)
        const original = historyPrefix(worker, history.streamKeys)
        assert.ok(original.heads.every(head => head.ack_sequence === head.highest_sequence))
        assert.equal(jobStore(devicePath.deviceData, live.job_id), historicalStore,
          'the measured execution uses the exact Worker database containing the historical facts')
        assert.equal(database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(live.job_id).state, 'running')
        const historyReadyAt = Date.now()
        const beforeMeasurement = currentExecutionLease(database, originalAuthority)
        assert.ok(beforeMeasurement)
        const starting = await waitFor(() => {
          const current = currentExecutionLease(database, originalAuthority)
          if (!current || current.expires_at === beforeMeasurement.expires_at) return false
          const proof = acceptedRenewalPeriod(database, leaseRecord(current))
          return Date.parse(proof.sentAt) >= historyReadyAt ? { lease: current, proof } : false
        }, 'a new real thirty second lease period begins after the acknowledged load is retained', 30_000, 250)
        const observed = [{ ...starting.proof, observedAt: Date.now() }]
        await waitFor(() => {
          const current = currentExecutionLease(database, originalAuthority)
          if (!current) return false
          const previous = observed.at(-1)
          if (current.expires_at !== previous.expiresAt) {
            assert.ok(Date.now() < Date.parse(previous.expiresAt), 'a real heartbeat renews before the previously accepted deadline')
            assert.ok(Date.parse(current.expires_at) > Date.parse(previous.expiresAt))
            observed.push({ ...acceptedRenewalPeriod(database, leaseRecord(current)), observedAt: Date.now() })
          }
          return Date.now() > Date.parse(starting.lease.expires_at) + 500 && observed.length >= 3
            && Date.parse(current.expires_at) > Date.now() ? current : false
        }, 'live heartbeats preserve the loaded execution across its current real thirty second deadline', 60_000, 250)
        assert.equal(jobStore(devicePath.deviceData, live.job_id), historicalStore)
        assert.equal(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(live.job_id).worker_instance_id,
          originalAuthority.worker_instance_id, 'measurement keeps the exact original Worker instance')
        assertHistoryPrefix(worker, history.streamKeys, original)
        const cancellingSession = (await api.query('session.get', { productSessionId })).result
        const cancelStarted = performance.now()
        await api.command('session.cancel', cancellingSession.revision, {
          productSessionId, reason: 'Cancel the observed execution after retained historical ACK load',
        })
        const cancelled = await terminal(api, database, productSessionId, live.job_id, 'cancelled', devicePath.deviceData, CANCEL_DEADLINE_MS)
        const cancelElapsedMs = performance.now() - cancelStarted
        assert.ok(cancelElapsedMs < CANCEL_DEADLINE_MS, 'control settlement completes within ten seconds under a thirty second lease')
        assert.equal(cancelled.reservation.release_reason, 'cancelled')
        assert.equal(cancelled.result.worker_instance_id, originalAuthority.worker_instance_id)
        assert.equal(jobStore(devicePath.deviceData, live.job_id), historicalStore)
        assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(live.job_id), originalAuthority,
          'cancellation preserves the exact original accepted dispatch authority')
        const httpCount = devicePath.modelServer.requests.length
        devicePath.modelServer.releaseResponseGate(holdMarker)
        await waitFor(() => devicePath.modelServer.activeRequests === 0, 'cancelled HTTPS attempt releases its physical slot', 10_000)
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        assertHistoryPrefix(worker, history.streamKeys, original)
        assert.equal(acknowledgedCoreHistory(worker, live.job_id).completedCommands.length, COMMAND_HISTORY_SIZE,
          'the late Provider response cannot add another command after cancellation')
        assert.equal(database.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id=?').get(live.job_id).count, 1)
        const calls = readDeviceExecutionReceipts(directory).calls
        assert.equal(new Set(calls.map(call => call.source)).size, 1, 'all loaded execution calls use the same Device Provider store')
        return { historyJobId: live.job_id, liveJobId: live.job_id, workerStore: historicalStore,
          workerInstanceId: originalAuthority.worker_instance_id,
          acknowledgedCommands: history.completedCommands.length, retainedCoreFacts: history.retained.length,
          retainedHistoryFrames: original.rows, historyDigest: original.digest, historyReadyAt,
          leaseSeconds: LEASE_SECONDS, heartbeatRenewals: observed.length - 1, measuredPeriods: observed,
          originalDeadline: claimed.expires_at, measuredDeadline: starting.lease.expires_at,
          lastDeadline: observed.at(-1).expiresAt, cancelDeadlineMs: CANCEL_DEADLINE_MS, cancelElapsedMs }
      } finally {
        devicePath.modelServer.releaseResponseGate(holdMarker)
        worker?.close()
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.acknowledgedCommands, COMMAND_HISTORY_SIZE)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})

test('Chat isolates two real Device Providers with three physical HTTPS slots each and an independently cancelled fourth waiter', async t => {
  const fixture = scenarioDirectory(t, 'wwc-chat-provider-isolation-')
  const { directory } = fixture
  const markers = { A: ['isolation-a-first', 'isolation-a-second', 'isolation-a-third', 'isolation-a-fourth'],
    B: ['isolation-b-first', 'isolation-b-second', 'isolation-b-third', 'isolation-b-fourth'] }
  const ids = { A: ['psn_01J00000000000000000000081', 'psn_01J00000000000000000000082',
    'psn_01J00000000000000000000083', 'psn_01J00000000000000000000084'],
  B: ['psn_01J00000000000000000000085', 'psn_01J00000000000000000000086',
    'psn_01J00000000000000000000087', 'psn_01J00000000000000000000088'] }
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...sealedBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: markers.A },
    scenario: { async run(context) {
      const { api, devicePath } = context
      const certificate = createCertificate(directory)
      const second = await startDeterministicDeviceModelServer({ certificatePath: certificate.cert,
        privateKeyPath: certificate.key, responseGateMarkers: markers.B }).listen()
      const database = openDatabase(join(directory, 'server-data/control-plane.sqlite3'))
      const first = devicePath.modelServer
      const servers = { A: first, B: second }
      const jobs = { A: [], B: [] }
      const receipts = { A: [], B: [] }
      try {
        const seeded = await seedDeviceLocalProvider({ api, publicClientId: devicePath.publicClientId,
          providerId: 'winwincode-device-isolation-second', modelId: 'device-isolation-second-model',
          endpoint: second.endpoint, apiKey: `device-local-${randomBytes(24).toString('hex')}` })
        assert.equal(seeded.receiptOutcome, 'saved')
        const routes = { A: context.modelRoute, B: configuredDeviceModelRoute({
          clientNodeId: seeded.clientNodeId, providerId: seeded.providerId, modelId: seeded.modelId }) }
        assert.notEqual(routes.A.providerId, routes.B.providerId)
        assert.notEqual(routes.A.credentialReferenceId, routes.B.credentialReferenceId)
        const launch = async (group, index) => {
          await createChat(context, ids[group][index], markers[group][index], routes[group])
          const payload = { productSessionId: ids[group][index], message: markers[group][index] }
          const requestId = `req_${ids[group][index].slice(4)}`
          const receipt = await api.command('chat.submit', 1, payload, requestId)
          jobs[group][index] = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?').get(requestId)
          assert.ok(jobs[group][index])
          receipts[group][index] = { payload, requestId, receipt }
        }
        for (const group of ['A', 'B']) {
          for (const index of [0, 1, 2]) await launch(group, index)
          await waitFor(() => servers[group].activeRequests === 3,
            `Provider ${group} holds exactly three real HTTPS responses`, 60_000)
          assert.equal(servers[group].peakActiveRequests, 3)
        }
        assert.equal(first.activeRequests + second.activeRequests, 6,
          'the second authenticated Provider advances while all three first-Provider slots stay occupied')
        for (const group of ['A', 'B']) {
          await launch(group, 3)
          await waitFor(() => modelOpens(devicePath.deviceData, jobs[group][3].job_id).length === 1,
            `Provider ${group} fourth Core request waits in its real durable Worker outbox`, 60_000)
          assert.equal(servers[group].requests.filter(request => request.gateMarker === markers[group][3]).length, 0)
        }
        const current = (await api.query('session.get', { productSessionId: ids.A[3] })).result
        await api.command('session.cancel', current.revision, { productSessionId: ids.A[3],
          reason: 'Cancel only the first Provider fourth waiter' })
        await terminal(api, database, ids.A[3], jobs.A[3].job_id, 'cancelled', devicePath.deviceData)
        assert.equal(first.activeRequests, 3)
        assert.equal(second.activeRequests, 3)
        first.releaseResponseGate(markers.A[0])
        await terminal(api, database, ids.A[0], jobs.A[0].job_id, 'completed', devicePath.deviceData)
        assert.equal(second.activeRequests, 3)
        assert.equal(second.requests.filter(request => request.gateMarker === markers.B[3]).length, 0,
          'a released first-Provider slot cannot admit a fourth second-Provider attempt')
        second.releaseResponseGate(markers.B[0])
        await waitFor(() => second.requests.some(request => request.gateMarker === markers.B[3] && request.active),
          'the second Provider independently admits its fourth waiter when its own slot is released', 60_000)
        assert.equal(second.activeRequests, 3)
        assert.equal(first.requests.filter(request => request.gateMarker === markers.A[3]).length, 0)
        for (const group of ['A', 'B']) for (const marker of markers[group]) servers[group].releaseResponseGate(marker)
        for (const group of ['A', 'B']) for (const index of group === 'A' ? [0, 1, 2] : [0, 1, 2, 3]) {
          await terminal(api, database, ids[group][index], jobs[group][index].job_id, 'completed', devicePath.deviceData)
        }
        const calls = readDeviceExecutionReceipts(directory).calls
        for (const group of ['A', 'B']) {
          for (const index of group === 'A' ? [0, 1, 2] : [0, 1, 2, 3]) {
            const original = calls.filter(call => call.jobId === jobs[group][index].job_id)
            assert.equal(original.length, 1)
            assert.equal(original[0].provider, routes[group].providerId)
            assert.deepEqual(original[0].providerAttempts.map(attempt => attempt.state), ['completed'])
          }
          assert.equal(servers[group].activeRequests, 0)
          assert.equal(servers[group].peakActiveRequests, 3)
        }
        assert.equal(new Set(calls.map(call => call.source)).size, 1,
          'both authenticated Provider limits are enforced in the same real Device Provider store')
        assert.equal(calls.filter(call => call.jobId === jobs.A[3].job_id).flatMap(call => call.providerAttempts)
          .filter(attempt => !['prepared', 'not_sent'].includes(attempt.state)).length, 0)
        const httpBeforeReplay = { A: first.requests.length, B: second.requests.length }
        for (const [group, index] of [['A', 0], ['B', 3]]) {
          const saved = receipts[group][index]
          const replay = await api.command('chat.submit', 1, saved.payload, saved.requestId)
          assert.deepEqual(replay.result, saved.receipt.result)
        }
        assert.deepEqual({ A: first.requests.length, B: second.requests.length }, httpBeforeReplay)
        return { providerIds: [routes.A.providerId, routes.B.providerId], peakHttpPerProvider: [first.peakActiveRequests, second.peakActiveRequests],
          simultaneouslyInFlight: 6, physicalRequests: httpBeforeReplay, cancelledJobId: jobs.A[3].job_id,
          independentFourthJobId: jobs.B[3].job_id, sharedProviderStore: true }
      } finally {
        for (const group of ['A', 'B']) for (const marker of markers[group]) servers[group].releaseResponseGate(marker)
        await second.close()
        database.close()
      }
    } },
  })
  assert.deepEqual(report.flow.scenario.peakHttpPerProvider, [3, 3])
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})
test('Chat cancels the current turn after a completed historical turn without changing the old acknowledged execution', async t => {
  const fixture = scenarioDirectory(t, 'wwc-chat-completed-history-cancel-')
  const { directory } = fixture
  const productSessionId = 'psn_01J00000000000000000000072'
  const historyRequestId = 'req_01J00000000000000000000073'
  const liveRequestId = 'req_01J00000000000000000000074'
  const holdMarker = 'completed-history-current-hold'
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: String(LEASE_SECONDS) },
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: [holdMarker], nativeCellScripts: {
      'native-cell-B': `const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));
        if(!command) throw new Error("command tool missing");
        const result=await tools[command.name]({cmd:"true",yield_time_ms:1000});
        if(result.exit_code!==0) throw new Error("history command failed");
        text({completedCommands:1});`,
    } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      const database = openDatabase(join(directory, 'server-data/control-plane.sqlite3'))
      let worker
      let currentWorker
      try {
        await createChat(context, productSessionId, 'Cancel the current turn after a completed turn')
        await api.command('chat.submit', 1, { productSessionId, message: 'native-cell-B' }, historyRequestId)
        const historical = await waitFor(() => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?')
          .get(historyRequestId), 'the historical Chat execution exists', 60_000)
        const completed = await terminal(api, database, productSessionId, historical.job_id, 'completed', devicePath.deviceData)
        const historicalStore = jobStore(devicePath.deviceData, historical.job_id)
        assert.ok(historicalStore)
        worker = openDatabase(historicalStore)
        const history = acknowledgedCoreHistory(worker, historical.job_id)
        assert.equal(history.completedCommands.length, 1, 'the completed turn retains its actual acknowledged Core command')
        const original = historyFingerprint(worker, history.streamKeys)
        const originalBytes = historyPrefix(worker, history.streamKeys)
        assert.ok(original.heads.every(head => head.ack_sequence === head.highest_sequence))
        const current = (await api.query('session.get', { productSessionId })).result
        await api.command('chat.submit', current.revision, { productSessionId, message: holdMarker }, liveRequestId)
        const live = await waitFor(() => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id=?')
          .get(liveRequestId), 'the next Chat turn creates its real current execution', 60_000)
        assert.notEqual(live.job_id, historical.job_id)
        await waitFor(() => devicePath.modelServer.requests.some(request => request.gateMarker === holdMarker && request.active),
          'the current turn has its actual HTTPS request in flight', 60_000)
        const originalAuthority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(live.job_id)
        assert.ok(originalAuthority)
        const currentStore = jobStore(devicePath.deviceData, live.job_id)
        assert.ok(currentStore)
        currentWorker = openDatabase(currentStore)
        const cancellingSession = (await api.query('session.get', { productSessionId })).result
        const cancelStarted = performance.now()
        await api.command('session.cancel', cancellingSession.revision, { productSessionId,
          reason: 'Cancel the current turn while retaining the completed historical turn' })
        const cancelled = await terminal(api, database, productSessionId, live.job_id, 'cancelled', devicePath.deviceData, CANCEL_DEADLINE_MS)
        const cancelElapsedMs = performance.now() - cancelStarted
        assert.ok(cancelElapsedMs < CANCEL_DEADLINE_MS)
        assert.equal(cancelled.reservation.release_reason, 'cancelled')
        assert.equal(cancelled.result.worker_instance_id, originalAuthority.worker_instance_id)
        assert.equal(database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(historical.job_id).state, 'completed')
        assert.deepEqual(database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(historical.job_id), completed.result)
        assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(live.job_id), originalAuthority)
        const httpCount = devicePath.modelServer.requests.length
        devicePath.modelServer.releaseResponseGate(holdMarker)
        await waitFor(() => devicePath.modelServer.activeRequests === 0, 'the cancelled current turn releases its real HTTPS request', 10_000)
        assert.equal(devicePath.modelServer.requests.length, httpCount)
        assert.deepEqual(historyFingerprint(worker, history.streamKeys), original)
        assertHistoryPrefix(worker, history.streamKeys, originalBytes)
        assert.equal(acknowledgedCoreHistory(worker, historical.job_id).completedCommands.length, history.completedCommands.length)
        assert.equal(acknowledgedCoreHistory(currentWorker, live.job_id).completedCommands.length, 0,
          'a late response cannot start a command in the cancelled current turn')
        for (const jobId of [historical.job_id, live.job_id]) {
          assert.equal(database.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id=?').get(jobId).count, 1)
        }
        const calls = readDeviceExecutionReceipts(directory).calls
        assert.equal(new Set(calls.map(call => call.source)).size, 1)
        return { historyJobId: historical.job_id, liveJobId: live.job_id,
          historyWorkerInstanceId: completed.result.worker_instance_id,
          currentWorkerInstanceId: cancelled.result.worker_instance_id,
          historyDigest: original.digest, cancelElapsedMs, cancelDeadlineMs: CANCEL_DEADLINE_MS }
      } finally {
        devicePath.modelServer.releaseResponseGate(holdMarker)
        currentWorker?.close()
        worker?.close()
        database.close()
      }
    } },
  })
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})
