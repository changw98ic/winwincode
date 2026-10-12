// SPDX-License-Identifier: Apache-2.0

// Real production error boundaries. Use freshly sealed binaries after the Chat gate passes.
// This file exercises real ChatAPI -> CP -> Device Worker -> Adapter -> Core.
// SQL cuts only refuse real writes. They do not insert receipts or change time.
// A file-directory cut covers Adapter has_consumed reads, not claim writes.
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import {
  existsSync, globSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { isDeepStrictEqual } from 'node:util'

import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { deterministicDeviceProvider, waitFor } from '../scripts/device-production-fixture.mjs'
import { readRetainedOutcomes } from './fixtures/chat-execution-transport-probe.mjs'

const SECRET = 'fixture-secret-chat-error-boundaries-792615'
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex')
const jsonBytes = bytes => JSON.parse(Buffer.from(bytes).toString('utf8'))

function sealedBinaries() {
  const directory = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  assert.ok(directory, 'production tests require explicit freshly sealed binaries')
  return { build: false, serverBinary: join(directory, 'winwincode-server'),
    wwcBinary: join(directory, 'wwc') }
}

function database(path, readOnly = true) {
  const store = new DatabaseSync(path, { readOnly })
  store.exec('PRAGMA busy_timeout=5000')
  return store
}

function readStore(path, inspect) {
  const store = database(path)
  try { return inspect(store) } finally { store.close() }
}

function storeRows(deviceData, fileName, inspect) {
  return globSync(`**/${fileName}`, { cwd: deviceData })
    .flatMap(path => readStore(join(deviceData, path), store => inspect(store, join(deviceData, path))))
}

function retainedRuns(deviceData, jobId, attempt = 1) {
  return storeRows(deviceData, 'worker-codex.sqlite3', (store, path) => {
    if (!store.prepare("SELECT 1 FROM sqlite_master WHERE name='codex_run'").get()) return []
    return store.prepare('SELECT record_json FROM codex_run').all().flatMap(row => {
      const run = jsonBytes(row.record_json)
      return run.job.jobId === jobId && run.job.attempt === attempt ? [{ path, run }] : []
    })
  })
}

function retainedRequests(deviceData, jobId, family, attempt = 1) {
  return storeRows(deviceData, 'worker-codex.sqlite3', (store, path) => {
    if (!store.prepare("SELECT 1 FROM sqlite_master WHERE name='execution_outbox'").get()) return []
    return store.prepare('SELECT * FROM execution_outbox WHERE family=? ORDER BY position').all(family)
      .flatMap(row => {
        const bytes = Buffer.from(row.frame_json)
        const frame = jsonBytes(bytes)
        return frame.lease?.jobId === jobId && frame.lease.attempt === attempt
          ? [{ path, row, bytes, frame }] : []
      })
  })
}

function signedActionReceipt(cp, request) {
  const suffix = sha256(request.requestId)
  const row = cp.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
    .get(`action-enforcement-receipt:${suffix}`)
  if (!row) return null
  const bytes = Buffer.from(row.payload)
  const receipt = jsonBytes(bytes)
  for (const field of ['requestId', 'jobId', 'workerSessionId', 'sessionIdentity', 'lease']) {
    assert.deepEqual(receipt[field], request[field], `the signed action receipt keeps original ${field}`)
  }
  return { revision: row.revision, bytes, receipt }
}

function responseProof(request) {
  return readStore(request.path, store => store.prepare(`SELECT response_digest FROM execution_response_receipt
    WHERE family='action_request' AND correlation_key=?`).get(request.row.correlation_key) ?? null)
}

function actionClaimPath(request) {
  return join(dirname(request.path), 'action-enforcement-receipts', `${sha256(request.frame.requestId)}.json`)
}

function workerSurface(deviceData, authority) {
  const rows = readStore(join(deviceData, 'device-client.sqlite3'), store => store.prepare(`SELECT *
    FROM worker_process_registry WHERE worker_id=? AND worker_instance_id=?`)
    .all(authority.worker_id, authority.worker_instance_id))
  assert.equal(rows.length, 1, 'one real Device-managed process owns the original Worker instance')
  return { dataDirectory: rows[0].data_directory,
    logPath: join(deviceData, 'worker-logs', rows[0].worker_session_id, 'stderr.log') }
}

function workerLog(surface) {
  return existsSync(surface.logPath) ? readFileSync(surface.logPath, 'utf8') : ''
}

function transportAttempts(surface, request) {
  const operation = sha256(`${request.frame.lease.workerId}:${request.frame.lease.workerInstanceId}:${request.frame.messageId}`)
  return globSync('**/network-requests.sqlite3', { cwd: surface.dataDirectory })
    .flatMap(path => readStore(join(surface.dataDirectory, path), store => store.prepare(`SELECT *
      FROM network_request_attempts WHERE operation_hash=? ORDER BY sequence`).all(operation)))
}

function coreCommands(deviceData, jobId, attempt = 1) {
  const runs = retainedRuns(deviceData, jobId, attempt)
  assert.equal(runs.length, 1)
  return storeRows(deviceData, 'state_5.sqlite', store => store.prepare(`SELECT r.request_json,
    r.original_input,r.call_status,r.call_result,
    a.attempt_id,a.execution,a.disposition,a.effective_input,a.execution_result,a.accepted_result,a.rejection
    FROM tool_requests r JOIN tool_attempts a ON a.request_sequence=r.sequence WHERE r.thread_id=?`)
    .all(runs[0].run.kernelSessionId).filter(row => JSON.parse(row.request_json).tool_name.endsWith('exec_command'))
    .map(row => ({ ...row, command: row.effective_input
      ? JSON.parse(JSON.parse(row.effective_input).input).cmd : null })))
}

function assertOneSuccessfulCommand(deviceData, jobId, command) {
  const rows = coreCommands(deviceData, jobId).filter(row => row.command === command)
  assert.equal(rows.length, 1, 'the original command has one actual Core handler attempt')
  assert.equal(rows[0].execution, 'completed')
  assert.equal(rows[0].disposition, 'accepted')
  for (const field of ['execution_result', 'accepted_result']) {
    assert.equal(JSON.parse(rows[0][field]).code_mode_result?.exit_code, 0,
      `the original ${field} records actual OS completion`)
  }
}

function requiredStoreCoreCauseReference(core, run, observed, source, command) {
  assert.ok(run.rolloutPath, 'the original Core thread retains its private canonical rollout')
  const bytes = readFileSync(run.rolloutPath)
  const lines = bytes.toString('utf8').split('\n').flatMap((line, index) => line.trim()
    ? [{ lineIndex: index + 1, bytes: Buffer.from(line), item: JSON.parse(line) }] : [])
  const header = lines.find(line => line.item.type === 'session_meta')
  assert.equal(header?.item.payload.id, run.kernelSessionId,
    'the private rollout header binds the original actual Core thread')
  const request = JSON.parse(observed.request_json)
  assert.equal(request.logical_id, observed.logical_id,
    'the private reference retains the original database logical request identity')
  const argumentsBytes = JSON.stringify({ cmd: command, yield_time_ms: 1000 })
  assert.equal(request.binding, sha256(JSON.stringify({ input: argumentsBytes, kind: 'function' })),
    'the refused nested claim binds the exact first command function payload')
  assert.equal(request.thread_id, run.kernelSessionId)
  assert.equal(request.turn_id, run.submissionId)
  assert.equal(request.source, 'code_mode', 'the refused claim is an actual nested Code Mode tool request')
  assert.ok(request.cell_id && request.parent_call_id && request.scope_id)
  assert.equal(request.scope_id, sha256(JSON.stringify([run.submissionId, request.parent_call_id])),
    'the nested request retains the exact original Core parent scope')
  const starts = lines.filter(line => line.item.type === 'event_msg'
    && ['task_started', 'turn_started'].includes(line.item.payload.type))
  assert.deepEqual(starts.map(line => line.item.payload.turn_id), [run.submissionId],
    'the source cause belongs to the one original accepted Core turn')
  const parentCall = lines.filter(line => line.item.type === 'response_item'
    && line.item.payload.type === 'custom_tool_call'
    && line.item.payload.call_id === request.parent_call_id)
  assert.equal(parentCall.length, 1)
  assert.equal(parentCall[0].item.payload.name, 'exec')
  assert.equal(parentCall[0].item.payload.input, source,
    'the original model-authored JS includes the real nested try/catch path')
  const parentLogicalId = `direct:${sha256(JSON.stringify([run.submissionId, request.parent_call_id]))}`
  const parent = core.prepare('SELECT request_json FROM tool_requests WHERE thread_id=? AND logical_id=?')
    .get(run.kernelSessionId, parentLogicalId)
  assert.ok(parent, 'the real Core observed the parent cell request for this exact nested call')
  const parentRequest = JSON.parse(parent.request_json)
  assert.equal(parentRequest.turn_id, request.turn_id)
  assert.equal(parentRequest.binding, sha256(JSON.stringify({ input: source, kind: 'custom' })),
    'the original parent receipt binds the exact JS source bytes')
  const expectedError = {
    type: 'error', codex_error_info: 'internal_server_error',
    message: 'Fatal error: tool_fact_store_unavailable: execution and recovery require durable Core facts',
  }
  const errors = lines.filter(line => line.item.type === 'event_msg'
    && isDeepStrictEqual(line.item.payload, expectedError))
  assert.equal(errors.length, 1, 'the original safe required-store Fatal survives in private durable Core history')
  const error = errors[0]
  assert.ok(error.lineIndex > starts[0].lineIndex && error.lineIndex > parentCall[0].lineIndex,
    'the durable Error follows the original turn and the exact parent tool request')
  return { kind: 'core_rollout_error', kernelThreadId: request.thread_id, coreTurnId: request.turn_id,
    originalRequestSequence: observed.sequence, logicalRequestId: request.logical_id,
    originalRequestDigest: sha256(observed.request_json), cellId: request.cell_id,
    parentCallId: request.parent_call_id, rolloutDigest: sha256(bytes),
    errorLineIndex: error.lineIndex, errorOrdinal: error.item.ordinal ?? null,
    errorLineDigest: sha256(error.bytes), sourceCause: 'tool_fact_store_unavailable' }
}

function expiredModelSourceReference(run) {
  assert.ok(run.rolloutPath, 'the original rejected model reply belongs to its actual private Core rollout')
  const bytes = readFileSync(run.rolloutPath)
  const lines = bytes.toString('utf8').split('\n').flatMap((line, index) => line.trim()
    ? [{ lineIndex: index + 1, bytes: Buffer.from(line), item: JSON.parse(line) }] : [])
  assert.equal(lines.find(row => row.item.type === 'session_meta')?.item.payload.id, run.kernelSessionId)
  const starts = lines.filter(row => row.item.type === 'event_msg'
    && ['task_started', 'turn_started'].includes(row.item.payload.type))
  assert.deepEqual(starts.map(row => row.item.payload.turn_id), [run.submissionId])
  const errors = lines.filter(row => row.item.type === 'event_msg'
    && row.item.payload.type === 'error'
    && row.item.payload.codex_error_info === 'model_authority_expired')
  assert.equal(errors.length, 1, 'the original Core terminal preserves the typed execution lease failure')
  assert.ok(errors[0].lineIndex > starts[0].lineIndex)
  assert.deepEqual(run.failureDiagnostic, { stage: 'core_event', cause: 'MODEL_AUTHORITY_EXPIRED' })
  return { scope: 'execution_lease', cause: 'lease_expired', sourceCode: 'MODEL_AUTHORITY_EXPIRED',
    kernelThreadId: run.kernelSessionId, coreTurnId: run.submissionId,
    rolloutDigest: sha256(bytes), errorLineIndex: errors[0].lineIndex,
    errorLineDigest: sha256(errors[0].bytes) }
}

function providerFacts(deviceData, jobId, attempt = 1) {
  return storeRows(deviceData, 'providers.sqlite3', (store, path) => store.prepare('SELECT * FROM exchanges WHERE request_open IS NOT NULL').all()
    .flatMap(exchange => {
      const open = JSON.parse(exchange.request_open)
      if (open.lease.jobId !== jobId || open.lease.attempt !== attempt) return []
      return [{ path, open, exchange,
        attempts: store.prepare('SELECT * FROM model_invocation_attempts WHERE exchange_id=? ORDER BY attempt_number')
          .all(exchange.exchange_id),
        diagnostics: store.prepare('SELECT * FROM model_attempt_diagnostics WHERE exchange_id=? ORDER BY sequence')
          .all(exchange.exchange_id) }]
    }))
}

function leasePeriod(row) {
  return Object.fromEntries(['job_id', 'lease_id', 'payload_digest', 'worker_id', 'worker_instance_id',
    'attempt', 'fencing_token', 'issued_at', 'expires_at'].map(field => [field, row[field]]))
}

function currentLease(cp, authority) {
  const row = cp.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(authority.job_id)
  if (!row) return null
  for (const field of Object.keys(leasePeriod(authority)).filter(field => field !== 'expires_at')) {
    assert.equal(row[field], authority[field])
  }
  for (const period of [leasePeriod(authority), leasePeriod(row)]) {
    const receipts = cp.prepare(`SELECT response_json FROM execution_lease_request_receipts
      WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(period.job_id, period.expires_at)
    assert.ok(receipts.some(receipt => isDeepStrictEqual(JSON.parse(receipt.response_json).lease, period)),
      'every observed lease period has its real original accepted claim or renewal receipt')
  }
  return row
}

async function assertPublicSafe(api, productSessionId) {
  const views = await Promise.all([
    api.query('session.get', { productSessionId }),
    api.query('session.messages.list', { productSessionId }),
    api.query('session.interactions.list', { productSessionId, states: ['pending', 'resolved', 'expired', 'cancelled'] }),
  ])
  const bytes = JSON.stringify(views)
  assert.equal(bytes.includes(SECRET), false, 'public projections exclude the actual Device-only Provider secret')
  assert.equal(bytes.includes('1f'.repeat(32)), false, 'public projections exclude the fixture action signing key')
  return views
}

function assertRetainedOutcomeAuthority(cp, authority, productSessionId, run, worker) {
  const message = worker.message
  assert.equal(message.workerSessionId, authority.worker_session_id)
  assert.deepEqual(message.sessionIdentity, {
    codexThreadId: run.canonicalThreadId, productSessionId,
    workerSessionId: authority.worker_session_id,
  }, 'the immutable raw Worker terminal retains the original three Chat session identities')
  assert.equal(message.outcome.codexThreadId, run.canonicalThreadId)
  const lease = message.lease
  const period = { job_id: lease.jobId, lease_id: lease.leaseId, payload_digest: authority.payload_digest,
    worker_id: lease.workerId, worker_instance_id: lease.workerInstanceId, attempt: lease.attempt,
    fencing_token: lease.fencingToken, issued_at: lease.issuedAt, expires_at: lease.expiresAt }
  for (const field of Object.keys(period).filter(field => field !== 'expires_at')) {
    assert.equal(period[field], authority[field], `the immutable outcome retains original authority ${field}`)
  }
  const receipts = cp.prepare(`SELECT response_json FROM execution_lease_request_receipts
    WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(authority.job_id, period.expires_at)
  assert.ok(receipts.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, period)),
    'the full immutable outcome lease period comes from its actual accepted claim or renewal')
  assert.deepEqual(cp.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?')
    .get(authority.job_id), authority, 'renewal and reliable delivery keep the original dispatch byte facts')
  return { sessionIdentity: message.sessionIdentity, period, immutableDigest: worker.digest,
    businessAcknowledged: worker.workerAcknowledged }
}

function terminalAccountingReady(cp, jobId, reservation, usage, reconciledUsage = usage) {
  if (!reservation || !usage) return false
  if (usage.accountingStatus === 'known') {
    return reservation.state === 'settled' && reservation.release_reason === null
      && reservation.actual_tokens === usage.tokens
      && reservation.actual_cost_microunits === usage.costMicrounits
      && reservation.actual_runtime_millis === usage.runtimeMillis
  }
  if (usage.accountingStatus !== 'unknown') return false
  const reconciliation = cp.prepare('SELECT * FROM execution_admission_reconciliation WHERE job_id=?').get(jobId)
  const latest = cp.prepare(`SELECT usage_json FROM execution_admission_reconciliation_facts
    WHERE job_id=? ORDER BY sequence DESC LIMIT 1`).get(jobId)
  const retainedUsage = reconciliation && jsonBytes(reconciliation.terminal_usage_json)
  const latestUsage = latest && jsonBytes(latest.usage_json)
  return reservation.state === 'released' && reservation.release_reason === 'failed'
    && reservation.actual_tokens === null && reservation.actual_cost_microunits === null
    && reservation.actual_runtime_millis === null
    && reconciliation?.accounting_status === 'pending' && reconciliation.settlement_request_id === null
    && reconciliation.settlement_usage_json === null && usage.tokens === null && usage.costMicrounits === null
    && isDeepStrictEqual(retainedUsage, usage)
    && isDeepStrictEqual(latestUsage ?? retainedUsage, reconciledUsage)
}

async function waitAcknowledgedTerminal(cp, api, deviceData, productSessionId, jobId, authority,
  expectedStatus = 'succeeded', { reconciliationEvidence = null } = {}) {
  const expectedOutcome = expectedStatus === 'succeeded' ? 'completed' : 'failed'
  const settled = await waitFor(async () => {
    const job = cp.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId)
    const terminal = cp.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(jobId)
    const slot = cp.prepare('SELECT * FROM worker_session_slots WHERE worker_session_id=?')
      .get(authority.worker_session_id)
    const reservation = cp.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId)
    const accepted = cp.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
      .get(`execution-outcome:${jobId}:${authority.attempt}`)
    const raw = accepted && jsonBytes(accepted.payload)
    const outcomes = readRetainedOutcomes(deviceData, jobId).filter(row => row.message.lease.attempt === authority.attempt)
    const messages = (await api.query('session.messages.list', { productSessionId })).result.items
    const runs = retainedRuns(deviceData, jobId, authority.attempt)
    const run = runs.length === 1 ? runs[0].run : null
    const accountingEvidence = reconciliationEvidence?.(outcomes[0]?.message.outcome.usage)
    return job?.state === expectedOutcome && job.attempt === authority.attempt
      && terminal?.outcome === expectedOutcome && slot?.state === expectedOutcome
      && (!reconciliationEvidence || accountingEvidence)
      && terminalAccountingReady(cp, jobId, reservation, outcomes[0]?.message.outcome.usage,
        accountingEvidence?.usage ?? outcomes[0]?.message.outcome.usage)
      && accepted?.revision === 2 && raw?.acknowledgementReady
      && outcomes.length === 1 && outcomes[0].workerAcknowledged && run?.terminal
      && messages.filter(message => message.role === 'assistant' && message.state === expectedOutcome).length === 1
      ? { job, terminal, slot, reservation, raw, worker: outcomes[0], run, accountingEvidence } : false
  }, `the original immutable raw ${expectedStatus} terminal receives its real normalized business ACK and resource settlement`, 120_000)
  assert.deepEqual(settled.raw.message, settled.worker.message)
  assert.equal(settled.worker.message.outcome.status, expectedStatus)
  assert.equal(settled.slot.codex_thread_id, settled.run.canonicalThreadId)
  for (const field of ['job_id', 'lease_id', 'worker_id', 'worker_instance_id', 'attempt', 'fencing_token']) {
    assert.equal(settled.terminal[field], authority[field])
    assert.equal(settled.slot[field], authority[field])
  }
  settled.authority = assertRetainedOutcomeAuthority(cp, authority, productSessionId, settled.run, settled.worker)
  return settled
}

async function launch(context, cp, productSessionId, marker) {
  const { api, modelRoute, devicePath } = context
  await api.command('session.create', 0, { productSessionId,
    projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
    title: `Real Chat error boundary ${marker}`, modelRoute })
  await devicePath.launchAnchor({ productSessionId })
  await api.command('chat.submit', 1, { productSessionId, message: `native-cell-A ${marker}` })
  const job = await waitFor(() => cp.prepare(`SELECT * FROM scheduler_execution_jobs
    WHERE product_session_id=? ORDER BY rowid DESC LIMIT 1`).get(productSessionId), 'the original Chat Job exists', 30_000)
  const authority = await waitFor(() => cp.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?')
    .get(job.job_id), 'the original real dispatch is accepted', 30_000)
  return { job, authority }
}

function nativeCommand(command, { escalated = false, continuation = null, expectedRejection = null } = {}) {
  const invocation = `text(await tools[command.name]({cmd:${JSON.stringify(command)},yield_time_ms:1000${escalated
    ? ',sandbox_permissions:"require_escalated",justification:"Exercise the original fixture approval."' : ''}}));`
  return `const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));
    if(!command) throw new Error("command tool missing");
    ${expectedRejection === null ? invocation : `try { ${invocation} } catch(error) {
      if(!String(error).includes(${JSON.stringify(expectedRejection)})) throw error;
      text(String(error));
    }`}
    ${continuation === null ? '' : `text(await tools[command.name]({cmd:${JSON.stringify(continuation)},yield_time_ms:1000}));`}`
}

async function runFixture(t, suffix, marker, script, run, { gated = true } = {}) {
  const directory = mkdtempSync(join(tmpdir(), `wwc-chat-errors-${suffix}-`))
  let completed = false
  t.after(() => {
    if (completed) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed error-boundary evidence retained at ${directory}`)
  })
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProviderSecrets: [SECRET],
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: {
      'native-cell-A': typeof script === 'function' ? script(directory) : script },
      ...(gated ? { responseGateMarkers: [marker] } : {}) },
    scenario: { async run(context) {
      const cp = database(join(directory, 'server-data/control-plane.sqlite3'), false)
      try { return await run({ ...context, cp, directory }) }
      finally { if (gated) context.devicePath.modelServer.releaseResponseGate(marker); cp.close() }
    } },
  })
  t.diagnostic(JSON.stringify(report.flow.scenario))
  completed = true
}

test('Chat formal user rejection consumes the original approval and never runs the denied command', async t => {
  const marker = 'formal-user-reject'
  const productSessionId = 'psn_01J00000000000000000000171'
  const continuation = "printf '%s' 'continued' > CHAT-USER-REJECT-CONTINUED.txt"
  await runFixture(t, 'user-reject', marker, directory => nativeCommand(
    `printf '%s' 'denied' > '${join(directory, 'CHAT-USER-DENIED.txt').replaceAll("'", "'\\''")}'`,
    { escalated: true, continuation, expectedRejection: 'approval denied by Control Plane' }),
    async ({ cp, api, devicePath, ...context }) => {
      const { job, authority } = await launch({ ...context, api, devicePath }, cp, productSessionId, marker)
      const approval = await waitFor(async () => (await api.query('approval.list', { states: ['pending'] })).result.items
        .find(item => item.binding.productSessionId === productSessionId), 'the real Core approval is formally pending', 60_000)
      assert.equal(approval.category, 'shell')
      assert.equal(approval.decisionEnabled, true)
      assert.equal(existsSync(join(context.directory, 'CHAT-USER-DENIED.txt')), false)
      const payload = { approvalId: approval.id, binding: approval.binding, decision: 'reject',
        reason: 'Reject the original temporary fixture shell.' }
      const requestId = 'req_01J00000000000000000000171'
      const accepted = await api.command('approval.decide', approval.revision, payload, requestId)
      assert.equal(accepted.outcome, 'completed')
      await waitAcknowledgedTerminal(cp, api, devicePath.deviceData, productSessionId, job.job_id, authority)
      assert.equal((await api.query('approval.get', { approvalId: approval.id })).result.state, 'rejected')
      assert.equal(existsSync(join(context.directory, 'CHAT-USER-DENIED.txt')), false)
      const denied = coreCommands(devicePath.deviceData, job.job_id).filter(row => row.command?.includes('CHAT-USER-DENIED.txt'))
      assert.equal(denied.length, 1, 'the original denied command retains one actual Core handler attempt')
      assert.equal(denied[0].disposition, 'rejected')
      assert.equal(denied[0].call_status, 'failed')
      assert.equal(JSON.parse(JSON.parse(denied[0].original_input).input).cmd, denied[0].command,
        'the failed logical call retains its exact original command request')
      assert.match(denied[0].call_result, /approval denied by Control Plane/,
        'the failed logical call retains the actual handler rejection body')
      assert.equal(denied[0].rejection, denied[0].call_result)
      assert.equal(denied[0].execution, 'uncertain')
      assert.equal(denied[0].execution_result, denied[0].call_result,
        'the physical handler failure and logical call retain the same actual error body')
      assert.equal(denied[0].accepted_result, null)
      assertOneSuccessfulCommand(devicePath.deviceData, job.job_id, continuation)
      const before = devicePath.modelServer.requests.length
      const replay = await api.command('approval.decide', approval.revision, payload, requestId)
      assert.deepEqual(replay.result, accepted.result)
      assert.equal(devicePath.modelServer.requests.length, before)
      await assertPublicSafe(api, productSessionId)
      return { jobId: job.job_id, approvalId: approval.id, scope: 'user_approval', decision: 'reject',
        originalDecisionReplayed: true, deniedEffectOccurred: false }
    }, { gated: false })
})

test('Chat signed action receipt write failure keeps the original pending action instead of fabricating user denial', async t => {
  const marker = 'action-receipt-write'
  const productSessionId = 'psn_01J00000000000000000000172'
  const command = "printf '%s' 'written' > CHAT-ACTION-WRITE-RECOVERED.txt"
  await runFixture(t, 'action-write', marker, nativeCommand(command), async context => {
    const { cp, api, devicePath } = context
    const { job, authority } = await launch(context, cp, productSessionId, marker)
    await waitFor(() => devicePath.modelServer.requests.some(value => value.gateMarker === marker && value.active),
      'the original Provider response is held before its real command', 60_000)
    assert.match(job.job_id, /^job_[0-9A-HJKMNP-TV-Z]{26}$/u)
    cp.exec(`CREATE TRIGGER refuse_original_action_receipt BEFORE INSERT ON product_state
      WHEN NEW.stream_id LIKE 'action-enforcement-receipt:%'
        AND json_extract(CAST(NEW.payload AS TEXT),'$.jobId')='${job.job_id}'
      BEGIN SELECT RAISE(ABORT,'refuse original required action receipt'); END`)
    try {
      devicePath.modelServer.releaseResponseGate(marker)
      const request = await waitFor(() => retainedRequests(devicePath.deviceData, job.job_id, 'action_request')[0],
        'the actual Core action retains its original authority request', 60_000)
      const surface = workerSurface(devicePath.deviceData, authority)
      const failed = await waitFor(() => transportAttempts(surface, request)
        .find(row => row.outcome === 'failed' && row.failure_json
          && JSON.parse(row.failure_json).httpStatus === 503),
      'the original authenticated action request actually receives a failed CP write response', 30_000)
      const frozen = cp.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
        .get(`action-enforcement-evaluation-time:${sha256(request.frame.requestId)}`)
      assert.ok(frozen, 'the original signed evaluation time is independently retained')
      assert.equal(signedActionReceipt(cp, request.frame), null)
      assert.equal(responseProof(request), null)
      assert.equal(existsSync(actionClaimPath(request)), false)
      assert.equal(globSync('**/CHAT-ACTION-WRITE-RECOVERED.txt', { cwd: context.directory }).length, 0)
      assert.deepEqual(retainedRequests(devicePath.deviceData, job.job_id, 'action_request')[0].bytes, request.bytes)
      assert.equal((await api.query('approval.list', { states: ['rejected'] })).result.items
        .filter(item => item.binding.executionJobId === job.job_id).length, 0)
      cp.exec('DROP TRIGGER refuse_original_action_receipt')
      const receipt = await waitFor(() => signedActionReceipt(cp, request.frame),
        'the same original request obtains its signed receipt after storage recovers', 60_000)
      assert.equal(receipt.receipt.decision, 'permit')
      assert.equal(receipt.receipt.evaluatedAt, jsonBytes(frozen.payload).evaluatedAt)
      await waitAcknowledgedTerminal(cp, api, devicePath.deviceData, productSessionId, job.job_id, authority)
      assertOneSuccessfulCommand(devicePath.deviceData, job.job_id, command)
      assert.ok(responseProof(request))
      assert.deepEqual(signedActionReceipt(cp, request.frame).bytes, receipt.bytes)
      await assertPublicSafe(api, productSessionId)
      return { jobId: job.job_id, requestId: request.frame.requestId, scope: 'control_plane_action_receipt',
        source: 'required_receipt_storage', failedTransportAttempt: failed.sequence,
        originalRequestSha256: sha256(request.bytes), recoveredReceiptSha256: sha256(receipt.bytes), userDenied: false }
    } finally { cp.exec('DROP TRIGGER IF EXISTS refuse_original_action_receipt') }
  })
})

for (const fault of ['action-store-read', 'applied-response-write']) {
  test(`Chat ${fault} keeps original signed authority and repairs its reliable delivery once`, async t => {
    const marker = fault
    const productSessionId = fault === 'action-store-read'
      ? 'psn_01J00000000000000000000173' : 'psn_01J00000000000000000000174'
    const command = `printf '%s' 'once' > CHAT-${fault.toUpperCase()}.txt`
    await runFixture(t, fault, marker, nativeCommand(command), async context => {
      const { cp, api, devicePath } = context
      const { job, authority } = await launch(context, cp, productSessionId, marker)
      await waitFor(() => devicePath.modelServer.requests.some(value => value.gateMarker === marker && value.active),
        'the original physical response is held before the actual action', 60_000)
      const [runtime] = retainedRuns(devicePath.deviceData, job.job_id)
      assert.ok(runtime, 'the actual Adapter store exists before installing the cut')
      const store = database(runtime.path, false)
      const surface = workerSurface(devicePath.deviceData, authority)
      const beforeLog = workerLog(surface)
      assert.equal(beforeLog.includes('stage=control_consume code=CodexDurableState'), false,
        'this original Worker has no previous generic acceptance failure')
      const receiptDirectory = join(dirname(runtime.path), 'action-enforcement-receipts')
      const savedDirectory = `${receiptDirectory}.original-fixture-save`
      let directoryMoved = false
      try {
        if (fault === 'action-store-read') {
          renameSync(receiptDirectory, savedDirectory)
          directoryMoved = true
          writeFileSync(receiptDirectory, 'unavailable original receipt directory', { mode: 0o600, flag: 'wx' })
        } else {
          store.exec(`CREATE TRIGGER refuse_original_applied_action BEFORE INSERT ON execution_response_receipt
            WHEN NEW.family='action_request'
              AND EXISTS(SELECT 1 FROM execution_outbox e WHERE e.family=NEW.family
                AND e.correlation_key=NEW.correlation_key
                AND json_extract(CAST(e.frame_json AS TEXT),'$.lease.jobId')='${job.job_id}')
            BEGIN SELECT RAISE(ABORT,'refuse original required response proof'); END`)
        }
        devicePath.modelServer.releaseResponseGate(marker)
        const request = await waitFor(() => retainedRequests(devicePath.deviceData, job.job_id, 'action_request')[0],
          'the original Core action reaches its durable request', 60_000)
        const signed = await waitFor(() => signedActionReceipt(cp, request.frame),
          'the actual CP signs and saves the original action receipt', 30_000)
        assert.equal(signed.receipt.decision, 'permit')
        await waitFor(() => workerLog(surface).slice(beforeLog.length).includes('stage=control_consume code=CodexDurableState'),
          'the original Adapter acceptance actually fails at the installed required-storage cut', 30_000)
        assert.equal(responseProof(request), null, 'a missing required response proof is never presented as applied')
        assert.deepEqual(retainedRequests(devicePath.deviceData, job.job_id, 'action_request')[0].bytes, request.bytes)
        assert.equal((await api.query('approval.list', { states: ['rejected'] })).result.items
          .filter(item => item.binding.executionJobId === job.job_id).length, 0)
        let originalClaim = null
        if (fault === 'action-store-read') {
          assert.equal(globSync('*.json', { cwd: savedDirectory }).length, 0,
            'Adapter has_consumed failed before the actual file claim')
          assert.equal(globSync(`**/CHAT-${fault.toUpperCase()}.txt`, { cwd: context.directory }).length, 0)
          rmSync(receiptDirectory, { force: true })
          renameSync(savedDirectory, receiptDirectory)
          directoryMoved = false
        } else {
          originalClaim = readFileSync(actionClaimPath(request))
          const claim = jsonBytes(originalClaim)
          assert.equal(claim.requestId, signed.receipt.requestId)
          assert.equal(claim.receiptSignature, signed.receipt.receiptSignature)
          store.exec('DROP TRIGGER refuse_original_applied_action')
        }
        const proof = await waitFor(() => responseProof(request),
          'the exact original signed receipt repairs its required response proof', 60_000)
        await waitAcknowledgedTerminal(cp, api, devicePath.deviceData, productSessionId, job.job_id, authority)
        assertOneSuccessfulCommand(devicePath.deviceData, job.job_id, command)
        assert.deepEqual(signedActionReceipt(cp, request.frame).bytes, signed.bytes)
        if (originalClaim) assert.deepEqual(readFileSync(actionClaimPath(request)), originalClaim,
          'reliable-delivery repair never creates a second action claim')
        assert.equal(store.prepare(`SELECT COUNT(*) AS count FROM execution_response_receipt
          WHERE family='action_request' AND correlation_key=?`).get(request.row.correlation_key).count, 1)
        await assertPublicSafe(api, productSessionId)
        return { jobId: job.job_id, requestId: request.frame.requestId,
          scope: fault === 'action-store-read' ? 'adapter_consumed_receipt_read' : 'adapter_applied_response_save',
          originalRequestSha256: sha256(request.bytes), originalReceiptSha256: sha256(signed.bytes),
          responseProofDigest: proof.response_digest, repaired: true, realCommandAttempts: 1, userDenied: false }
      } finally {
        store.exec('DROP TRIGGER IF EXISTS refuse_original_applied_action')
        store.close()
        if (directoryMoved) {
          rmSync(receiptDirectory, { force: true })
          renameSync(savedDirectory, receiptDirectory)
        }
      }
    })
  })
}

test('Chat a refused real Provider connection retains not_sent proof and its signed zero-charge accounting', async t => {
  const marker = 'provider-connect-not-sent'
  const productSessionId = 'psn_01J00000000000000000000180'
  await runFixture(t, 'connect-not-sent', marker, nativeCommand('true'), async context => {
    const { cp, api, devicePath } = context
    const beforeHttp = devicePath.modelServer.requests.length
    await devicePath.modelServer.close()
    const { job, authority } = await launch(context, cp, productSessionId, marker)
    const failed = await waitFor(() => providerFacts(devicePath.deviceData, job.job_id).flatMap(value =>
      value.attempts.filter(attempt => attempt.state === 'not_sent' && attempt.failure_chunks !== null)
        .flatMap(attempt => JSON.parse(attempt.failure_chunks).flatMap(chunk => {
          const metadata = chunk.payload && JSON.parse(Buffer.from(chunk.payload.dataBase64, 'base64').toString('utf8'))
          return metadata?.facts?.acceptance === 'not_sent'
            ? [{ ...value, attempt, chunk, facts: metadata.facts }] : []
        })))[0], 'the real refused TCP connection retains its explicit not_sent failure proof', 30_000)
    assert.equal(failed.facts.scope, 'model_attempt')
    assert.equal(failed.facts.phase, 'connect')
    assert.equal(failed.facts.attemptId, failed.attempt.adapter_request_id)
    assert.equal(failed.facts.sourceCode, failed.chunk.error.code)
    assert.equal(devicePath.modelServer.requests.length, beforeHttp)
    assert.equal(cp.prepare('SELECT COUNT(*) AS n FROM execution_lease_terminals WHERE job_id=?')
      .get(job.job_id).n, 0, 'a not_sent connection retry remains waiting under the original execution')
    const revision = (await api.query('session.get', { productSessionId })).result.revision
    await api.command('session.cancel', revision, { productSessionId, reason: 'Close the original refused connection retry.' })
    const settled = await waitFor(() => {
      const terminal = cp.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(job.job_id)
      const worker = readRetainedOutcomes(devicePath.deviceData, job.job_id)
        .find(row => row.message.lease.attempt === authority.attempt)
      const accepted = cp.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
        .get(`execution-outcome:${job.job_id}:${authority.attempt}`)
      const raw = accepted && jsonBytes(accepted.payload)
      const statement = cp.prepare(`SELECT statement_digest,statement_json FROM execution_attempt_accounting_facts
        WHERE job_id=? AND attempt=?`).get(job.job_id, authority.attempt)
      return terminal?.outcome === 'cancelled' && worker?.workerAcknowledged
        && accepted?.revision === 2 && raw?.acknowledgementReady && statement
        ? { terminal, worker, raw, statement } : false
    }, 'the original cancellation receives its actual business ACK and signed accounting statement', 30_000)
    assert.deepEqual(settled.raw.message, settled.worker.message)
    assert.equal(settled.worker.message.outcome.status, 'cancelled')
    const statement = jsonBytes(settled.statement.statement_json)
    const slot = `primary:${failed.exchange.exchange_id}`
    assert.deepEqual(statement.manifest, [slot])
    assert.equal(statement.receipts.length, 1)
    const receipt = statement.receipts[0]
    assert.equal(receipt.slot, slot)
    assert.equal(receipt.modelExchangeId, failed.exchange.exchange_id)
    assert.equal(receipt.tokens, 0)
    assert.equal(receipt.knownTokens, undefined, 'the signed receipt omits its zero lower bound')
    assert.equal(receipt.costMicrounits, 0)
    assert.match(statement.signature, /^sha256:[0-9a-f]{64}$/u)
    assert.match(receipt.sourceDigest, /^sha256:[0-9a-f]{64}$/u)
    const final = providerFacts(devicePath.deviceData, job.job_id)
    assert.equal(final.length, 1)
    assert.ok(final[0].attempts.length > 0)
    assert.ok(final[0].attempts.every(attempt => attempt.state === 'not_sent'))
    assert.equal(devicePath.modelServer.requests.length, beforeHttp)
    assert.equal(coreCommands(devicePath.deviceData, job.job_id).length, 0)
    await assertPublicSafe(api, productSessionId)
    return { jobId: job.job_id, modelExchangeId: failed.exchange.exchange_id,
      sourceCode: failed.facts.sourceCode, acceptance: failed.facts.acceptance,
      physicalAttempts: final[0].attempts.length, extraHttp: 0,
      tokens: receipt.tokens, costMicrounits: receipt.costMicrounits,
      statementDigest: settled.statement.statement_digest, businessAcknowledged: true }
  }, { gated: false })
})

test('Chat real Provider opening timeout retains model-attempt unknown facts while its original lease renews', async t => {
  const marker = 'provider-opening-timeout'
  const productSessionId = 'psn_01J00000000000000000000175'
  const command = "printf '%s' 'continued' > CHAT-HTTP-TIMEOUT-RECOVERED.txt"
  await runFixture(t, 'http-timeout', marker, nativeCommand(command), async context => {
    const { cp, api, devicePath } = context
    const { job, authority } = await launch(context, cp, productSessionId, marker)
    await waitFor(() => devicePath.modelServer.requests.some(value => value.gateMarker === marker && value.active),
      'the first real physical HTTP request waits for response headers', 60_000)
    // Production opening stages use connect 15s + idle 60s. No deadline is
    // changed by this fixture. The first actual failure can take about 75s.
    const failed = await waitFor(() => providerFacts(devicePath.deviceData, job.job_id).flatMap(value =>
      value.attempts.filter(attempt => attempt.failure_chunks !== null).flatMap(attempt => {
        const chunks = JSON.parse(attempt.failure_chunks)
        return chunks.flatMap(chunk => {
          const metadata = chunk.payload && JSON.parse(Buffer.from(chunk.payload.dataBase64, 'base64').toString('utf8'))
          return metadata?.facts?.cause === 'timeout' ? [{ ...value, attempt, chunk, facts: metadata.facts }] : []
        })
      }))[0], 'the original physical HTTP request reaches its real transport timeout', 110_000)
    assert.equal(failed.attempt.state, 'failed')
    assert.equal(failed.facts.scope, 'model_attempt')
    assert.equal(failed.facts.acceptance, 'unknown')
    assert.equal(failed.facts.phase, 'response_headers')
    assert.equal(failed.facts.attemptId, failed.attempt.adapter_request_id)
    assert.equal(failed.facts.sourceCode, failed.chunk.error.code)
    const diagnostic = await waitFor(() => providerFacts(devicePath.deviceData, job.job_id)
      .find(value => value.open.modelExchangeId === failed.open.modelExchangeId)?.diagnostics
      .find(value => value.policy_attempt === failed.attempt.attempt_number && value.outcome === 'failed'),
    'the actual finite timeout diagnostic closes after its required attempt receipt', 10_000)
    assert.equal(JSON.parse(diagnostic.failure_json).kind, 'timeout')
    const renewed = currentLease(cp, authority)
    assert.ok(Date.parse(renewed.expires_at) > diagnostic.finished_ms,
      'real heartbeat authority stays live beyond the HTTP failure')
    assert.ok(Date.parse(renewed.expires_at) > Date.parse(authority.expires_at))
    assert.equal(cp.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id=?')
      .get(job.job_id).count, 0, 'one HTTP timeout does not stand for an expired execution or terminal')
    devicePath.modelServer.releaseResponseGate(marker)
    await waitAcknowledgedTerminal(cp, api, devicePath.deviceData, productSessionId, job.job_id, authority)
    assertOneSuccessfulCommand(devicePath.deviceData, job.job_id, command)
    const reconciliation = await waitFor(() => cp.prepare('SELECT * FROM execution_admission_reconciliation WHERE job_id=?')
      .get(job.job_id), 'the original unknown physical attempt reaches authoritative accounting', 30_000)
    const reservation = cp.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
    assert.equal(reservation.actual_tokens, null)
    assert.equal(reservation.actual_cost_microunits, null)
    assert.equal(reconciliation.accounting_status, 'pending')
    const usage = JSON.parse(reconciliation.terminal_usage_json)
    assert.equal(usage.accountingStatus, 'unknown')
    assert.equal(usage.tokens ?? null, null)
    assert.equal(usage.costMicrounits ?? null, null)
    assert.ok(usage.knownTokens > 0, 'the later actual success remains a measured lower bound')
    await assertPublicSafe(api, productSessionId)
    return { jobId: job.job_id, modelExchangeId: failed.open.modelExchangeId,
      adapterRequestId: failed.attempt.adapter_request_id, scope: failed.facts.scope,
      cause: failed.facts.cause, acceptance: failed.facts.acceptance,
      renewedLeaseExpiresAt: renewed.expires_at, originalUsageStatus: usage.accountingStatus,
      knownTokens: usage.knownTokens }
  })
})

function originalProviderReconciliation(cp, deviceData, authority, usage, providerId) {
  if (!usage) return false
  const [original] = providerFacts(deviceData, authority.job_id, authority.attempt)
  if (!original || original.attempts.length !== 1 || original.attempts[0].state !== 'completed'
    || !original.attempts[0].accounting_chunks) return false
  const frames = jsonBytes(original.attempts[0].accounting_chunks)
  const completions = frames.flatMap(frame => {
    assert.equal(frame.modelExchangeId, original.exchange.exchange_id)
    assert.deepEqual(frame.lease, original.open.lease)
    assert.deepEqual(frame.sessionIdentity, original.open.sessionIdentity)
    if (!frame.payload) return []
    const bytes = Buffer.from(frame.payload.dataBase64, 'base64')
    assert.equal(frame.payload.payloadDigest, `sha256:${sha256(bytes)}`)
    const item = JSON.parse(bytes.toString('utf8'))
    return item.type === 'completed' ? [item] : []
  })
  assert.equal(completions.length, 1, 'one actual sent Provider attempt supplies its original measured completion')
  const tokens = completions[0].tokenUsage.total_tokens
  assert.ok(Number.isSafeInteger(tokens) && tokens > 0)
  assert.equal(usage.knownTokens, tokens, 'the immutable unknown terminal retains the measured lower bound')
  const facts = cp.prepare(`SELECT statement_digest,statement_json FROM execution_attempt_accounting_facts
    WHERE job_id=? AND attempt=?`).all(authority.job_id, authority.attempt)
  if (facts.length === 0) return false
  assert.equal(facts.length, 1)
  const statement = jsonBytes(facts[0].statement_json)
  assert.deepEqual(statement.lease, original.open.lease)
  const slot = `primary:${original.exchange.exchange_id}`
  assert.deepEqual(statement.manifest, [slot])
  assert.equal(statement.receipts.length, 1)
  const receipt = statement.receipts[0]
  assert.equal(receipt.slot, slot)
  assert.equal(receipt.modelExchangeId, original.exchange.exchange_id)
  assert.equal(receipt.providerId, providerId)
  assert.equal(receipt.tokens, tokens)
  assert.equal(receipt.knownTokens, tokens)
  assert.equal(receipt.costMicrounits, null, 'the actual Provider did not supply a measured cost')
  assert.match(receipt.sourceDigest, /^sha256:[0-9a-f]{64}$/u)
  assert.equal(receipt.providerReceiptId, `device-attempt-set:${receipt.sourceDigest}`)
  assert.match(statement.signature, /^sha256:[0-9a-f]{64}$/u)
  return { usage: { accountingStatus: 'known', tokens, knownTokens: tokens,
    costMicrounits: null, runtimeMillis: usage.runtimeMillis },
    source: { statementDigest: facts[0].statement_digest, providerReceiptId: receipt.providerReceiptId,
      modelExchangeId: receipt.modelExchangeId,
      completionDigest: sha256(JSON.stringify(completions[0])) } }
}

test('Chat original execution lease expires naturally while an already sent model request remains unknown', async t => {
  const marker = 'execution-lease-expiry'
  const productSessionId = 'psn_01J00000000000000000000176'
  const command = "printf '%s' 'forbidden' > CHAT-OLD-LEASE-EFFECT.txt"
  await runFixture(t, 'lease-expiry', marker, nativeCommand(command), async context => {
    const { cp, api, devicePath } = context
    const { job, authority } = await launch(context, cp, productSessionId, marker)
    await waitFor(() => devicePath.modelServer.requests.some(value => value.gateMarker === marker && value.active),
      'the original lease owns one real sent HTTP request', 60_000)
    assert.match(authority.lease_id, /^lse_[0-9A-HJKMNP-TV-Z]{26}$/u)
    assert.match(authority.fencing_token, /^[1-9][0-9]{0,19}$/u)
    cp.exec(`CREATE TRIGGER refuse_original_lease_renewal BEFORE UPDATE ON execution_leases
      WHEN OLD.job_id='${job.job_id}' AND OLD.lease_id='${authority.lease_id}'
        AND OLD.attempt=${authority.attempt} AND OLD.fencing_token='${authority.fencing_token}'
      BEGIN SELECT RAISE(ABORT,'refuse original lease renewal'); END`)
    try {
      // Re-read after the refusal is installed. A real concurrent renewal
      // committed before this cut owns its actual later expiry period.
      const period = currentLease(cp, authority)
      assert.ok(period)
      assert.deepEqual(cp.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?')
        .get(job.job_id), authority)
      await waitFor(() => Date.now() >= Date.parse(period.expires_at), 'the actual unmodified lease deadline arrives', 45_000)
      const current = cp.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(job.job_id)
      if (current && current.lease_id === period.lease_id && current.attempt === period.attempt) {
        assert.deepEqual(leasePeriod(current), leasePeriod(period))
      }
      const original = providerFacts(devicePath.deviceData, job.job_id, period.attempt)
      assert.equal(original.length, 1, 'inspect only the original fenced attempt, not a possible replacement')
      assert.equal(original[0].attempts.length, 1)
      assert.equal(original[0].attempts[0].state, 'invoking',
        'the original sent request is still awaiting its actual physical response at the expiry cut')
      assert.equal(original[0].attempts[0].accounting_chunks, null,
        'a sent request with no receipt retains unknown charge certainty')
      assert.equal(original[0].attempts[0].failure_chunks, null)
      assert.equal(original[0].exchange.accounting_chunks, null)
      assert.equal(original[0].exchange.chunks, null)
      assert.equal(globSync('**/CHAT-OLD-LEASE-EFFECT.txt', { cwd: context.directory }).length, 0)
      devicePath.modelServer.releaseResponseGate(marker)
      const worker = await waitFor(() => {
        const outcomes = readRetainedOutcomes(devicePath.deviceData, job.job_id)
          .filter(row => row.message.lease.attempt === period.attempt)
        return outcomes.length === 1 ? outcomes[0] : false
      }, 'the original expired execution forms one immutable raw Worker terminal', 60_000)
      const [runtime] = retainedRuns(devicePath.deviceData, job.job_id, period.attempt)
      const retained = assertRetainedOutcomeAuthority(cp, authority, productSessionId, runtime.run, worker)
      assert.deepEqual(leasePeriod(retained.period), leasePeriod(period))
      assert.equal(worker.message.outcome.status, 'infrastructure_error')
      assert.equal(worker.message.outcome.error.code, 'INFRASTRUCTURE_ERROR')
      assert.match(worker.message.outcome.error.message, /cause=MODEL_AUTHORITY_EXPIRED/u)
      const usage = worker.message.outcome.usage
      assert.equal(usage.accountingStatus, 'unknown')
      assert.equal(usage.tokens, null)
      assert.equal(usage.costMicrounits, null)
      const settled = await waitAcknowledgedTerminal(cp, api, devicePath.deviceData, productSessionId,
        job.job_id, authority, 'infrastructure_error', {
          reconciliationEvidence: actualUsage => originalProviderReconciliation(cp, devicePath.deviceData,
            authority, actualUsage, context.modelRoute.providerId),
        })
      assert.deepEqual(settled.worker.bytes, worker.bytes)
      assert.equal(settled.run.kernelSessionId, runtime.run.kernelSessionId)
      assert.equal(settled.run.submissionId, runtime.run.submissionId)
      assert.equal(settled.run.terminal.kind, 'infrastructure_failed')
      const sourceCause = expiredModelSourceReference(settled.run)
      const commands = coreCommands(devicePath.deviceData, job.job_id, period.attempt)
      assert.equal(commands.filter(row => row.command === command).length, 0,
        'the original expired execution does not start the late reply command')
      assert.equal(providerFacts(devicePath.deviceData, job.job_id, period.attempt)[0].attempts.length, 1,
        'the original expired owner cannot start a second physical Provider attempt')
      await assertPublicSafe(api, productSessionId)
      return { jobId: job.job_id, attempt: period.attempt, leaseId: period.lease_id,
        fencingToken: period.fencing_token, scope: 'execution_lease', expiresAt: period.expires_at,
        originalPhysicalAttempts: 1, originalLateCommandAttempts: 0,
        chargeCertaintyAtExpiry: 'unknown', expiryRejectionCauseVerified: true, sourceCause,
        terminalCauseVerified: true, immutableOutcomeDigest: worker.digest,
        businessAcknowledged: settled.worker.workerAcknowledged,
        accountingEvidence: settled.accountingEvidence }
    } finally { cp.exec('DROP TRIGGER IF EXISTS refuse_original_lease_renewal') }
  })
})

test('Chat original input deadline expires independently of its legitimately renewed execution lease', async t => {
  const marker = 'input-scope-expiry'
  const productSessionId = 'psn_01J00000000000000000000177'
  const command = "printf '%s' 'consumed' > CHAT-ERROR-INPUT-EXPIRED.txt"
  const script = `const input=ALL_TOOLS.find(tool=>tool.name.endsWith("request_user_input"));
    if(!input) throw new Error("input tool missing");
    text(await tools[input.name]({questions:[{id:"original_deadline",header:"Deadline",
      question:"Wait for the original deadline?",options:[{label:"continue",description:"Continue."},
      {label:"revise",description:"Revise."}]}]})); ${nativeCommand(command)}`
  await runFixture(t, 'input-expiry', marker, script, async context => {
    const { cp, api, devicePath } = context
    const { job, authority } = await launch(context, cp, productSessionId, marker)
    const interactions = () => api.query('session.interactions.list', {
      productSessionId, states: ['pending', 'expired', 'resolved', 'cancelled'],
    })
    const original = await waitFor(async () => (await interactions()).result.items
      .find(item => item.kind === 'input' && item.state === 'pending'), 'the real Core input has its original public deadline', 60_000)
    const request = retainedRequests(devicePath.deviceData, job.job_id, 'input_request')[0]
    assert.equal(request.frame.inputRequestId, original.inputRequestId)
    assert.equal(request.frame.expiresAt, original.expiresAt)
    const renewed = await waitFor(() => {
      const current = currentLease(cp, authority)
      return current && Date.parse(current.expires_at) > Date.parse(original.expiresAt) ? current : false
    }, 'real heartbeats extend the execution authority beyond the original input deadline', 30_000)
    const timeout = await waitFor(() => retainedRuns(devicePath.deviceData, job.job_id)[0]?.run.interactionTimeouts
      ?.find(value => value.request.inputRequestId === original.inputRequestId && value.appliedKernelSessionId),
    'the real original input timeout settles its exact original Core waiter', 120_000)
    const [retained] = retainedRuns(devicePath.deviceData, job.job_id)
    assert.equal(timeout.causeCode, 'INTERACTION_DEADLINE_EXPIRED')
    assert.deepEqual(timeout.request, request.frame)
    const digest = createHash('sha256').update('winwincode.interaction-timeout-request.v1')
      .update(Buffer.from([0])).update(request.bytes).digest('hex')
    assert.equal(timeout.requestDigest, `sha256:${digest}`)
    assert.equal(timeout.appliedKernelSessionId, retained.run.kernelSessionId)
    assert.ok(Date.parse(timeout.observedAt) >= Date.parse(original.expiresAt))
    assert.equal(retained.run.interactionTimeouts.length, 1)
    await waitAcknowledgedTerminal(cp, api, devicePath.deviceData, productSessionId, job.job_id, authority)
    assertOneSuccessfulCommand(devicePath.deviceData, job.job_id, command)
    const expired = (await interactions()).result.items.find(item => item.inputRequestId === original.inputRequestId)
    assert.equal(expired.state, 'expired')
    assert.equal(expired.expiresAt, original.expiresAt)
    await assert.rejects(api.command('input.respond', expired.revision, {
      ...original.binding, inputRequestId: original.inputRequestId, status: 'provided',
      value: { mode: 'single_choice', value: 'continue' },
    }), error => Number.isInteger(error.status) && error.status >= 400 && error.status < 500)
    await assertPublicSafe(api, productSessionId)
    return { jobId: job.job_id, inputRequestId: original.inputRequestId, scope: 'input_interaction',
      cause: timeout.causeCode, expiresAt: original.expiresAt,
      renewedLeaseExpiresAt: renewed.expires_at, originalWaiterApplied: true }
  }, { gated: false })
})

test('Chat real command exit failure remains tool feedback and Core executes its repair once', async t => {
  const marker = 'core-tool-repair'
  const productSessionId = 'psn_01J00000000000000000000178'
  const failedCommand = "printf '%s' 'observed original failure'; exit 7"
  const repairedCommand = "printf '%s' 'repaired' > CHAT-CORE-REPAIRED.txt"
  const script = `const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));
    if(!command) throw new Error("command tool missing");
    const original=await tools[command.name]({cmd:${JSON.stringify(failedCommand)},yield_time_ms:1000});
    if(original.exit_code!==7) throw new Error("original OS failure was not observed");
    text(original);
    text(await tools[command.name]({cmd:${JSON.stringify(repairedCommand)},yield_time_ms:1000}));`
  await runFixture(t, 'core-tool-repair', marker, script, async context => {
    const { cp, api, devicePath } = context
    const { job, authority } = await launch(context, cp, productSessionId, marker)
    const completed = await waitAcknowledgedTerminal(cp, api, devicePath.deviceData, productSessionId, job.job_id, authority)
    const attempts = coreCommands(devicePath.deviceData, job.job_id)
    const failed = attempts.filter(value => value.command === failedCommand)
    assert.equal(failed.length, 1, 'the failed command is one actual Core handler attempt')
    assert.equal(failed[0].execution, 'completed')
    assert.equal(failed[0].disposition, 'accepted', 'the failure is accepted factual tool feedback')
    for (const field of ['execution_result', 'accepted_result']) {
      assert.equal(JSON.parse(failed[0][field]).code_mode_result?.exit_code, 7)
    }
    assertOneSuccessfulCommand(devicePath.deviceData, job.job_id, repairedCommand)
    assert.equal(completed.run.failureDiagnostic ?? null, null)
    assert.equal(cp.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id=?')
      .get(job.job_id).count, 1)
    await assertPublicSafe(api, productSessionId)
    return { jobId: job.job_id, scope: 'core_tool_handler', originalExitCode: 7,
      originalAttemptId: failed[0].attempt_id, repairAttemptCount: 1, effectiveOutcome: 'completed' }
  }, { gated: false })
})

// Direct storage_error Fatal mapping and Code Mode nested Fatal retention
// are separate required boundaries.
// It requires typed infrastructure classification and the original private
// canonical Core cause. The public Worker summary uses its finite safe class.
test('Chat required Core tool claim save failure retains infrastructure cause before any OS effect', async t => {
  const marker = 'core-required-claim-write'
  const productSessionId = 'psn_01J00000000000000000000179'
  const command = "printf '%s' 'forbidden' > CHAT-CORE-CLAIM-UNSAVED.txt"
  const source = `const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));
    if(!command) throw new Error("command tool missing");
    try { text(await tools[command.name]({cmd:${JSON.stringify(command)},yield_time_ms:1000})); }
    catch(error) { text("caught nested failure"); }
    text("JS success after caught failure");
    text(await tools[command.name]({cmd:"printf '%s' 'forbidden' > CHAT-CORE-AFTER-FATAL.txt",yield_time_ms:1000}));`
  await runFixture(t, 'core-required-claim', marker, source, async context => {
    const { cp, api, devicePath } = context
    const { job, authority } = await launch(context, cp, productSessionId, marker)
    await waitFor(() => devicePath.modelServer.requests.some(value => value.gateMarker === marker && value.active),
      'the real original model reply is held before its required Core command claim', 60_000)
    const [runtime] = retainedRuns(devicePath.deviceData, job.job_id)
    const corePaths = globSync('**/state_5.sqlite', { cwd: devicePath.deviceData })
      .map(path => join(devicePath.deviceData, path)).filter(path => readStore(path, store =>
        Boolean(store.prepare('SELECT 1 FROM threads WHERE id=?').get(runtime.run.kernelSessionId))))
    assert.equal(corePaths.length, 1, 'the actual original thread owns one real Core state store')
    const core = database(corePaths[0], false)
    const threadId = runtime.run.kernelSessionId
    assert.match(threadId, /^[a-zA-Z0-9_-]{1,200}$/u)
    try {
      core.exec(`CREATE TRIGGER refuse_original_core_tool_claim BEFORE INSERT ON tool_attempts
        WHEN EXISTS(SELECT 1 FROM tool_requests r WHERE r.sequence=NEW.request_sequence
          AND r.thread_id='${threadId}'
          AND json_extract(r.request_json,'$.tool_name') LIKE '%exec_command')
        BEGIN SELECT RAISE(ABORT,'refuse original required Core execution claim'); END`)
      devicePath.modelServer.releaseResponseGate(marker)
      const worker = await waitFor(() => {
        const outcomes = readRetainedOutcomes(devicePath.deviceData, job.job_id)
          .filter(row => row.message.lease.attempt === authority.attempt)
        return outcomes.length === 1 ? outcomes[0] : false
      }, 'the actual Core required-store failure reaches the original immutable raw Worker terminal', 60_000)
      assertRetainedOutcomeAuthority(cp, authority, productSessionId, runtime.run, worker)
      const raw = worker.message
      assert.equal(raw.outcome.status, 'infrastructure_error')
      assert.equal(raw.outcome.error.code, 'INFRASTRUCTURE_ERROR')
      const settled = await waitAcknowledgedTerminal(cp, api, devicePath.deviceData, productSessionId,
        job.job_id, authority, 'infrastructure_error')
      assert.deepEqual(settled.worker.bytes, worker.bytes, 'business delivery keeps the immutable original failure frame')
      assert.equal(settled.run.terminal.kind, 'infrastructure_failed')
      const observed = core.prepare(`SELECT r.sequence,r.logical_id,r.request_json FROM tool_requests r
        WHERE r.thread_id=? AND json_extract(r.request_json,'$.tool_name') LIKE '%exec_command'`).all(threadId)
      assert.equal(observed.length, 1, 'the real Core observed its original command request before the rejected save')
      assert.equal(core.prepare('SELECT COUNT(*) AS count FROM tool_attempts WHERE request_sequence=?')
        .get(observed[0].sequence).count, 0, 'the rejected required claim never becomes a dispatched effect')
      assert.equal(settled.run.kernelSessionId, runtime.run.kernelSessionId)
      assert.equal(settled.run.submissionId, runtime.run.submissionId)
      const privateCause = requiredStoreCoreCauseReference(core, settled.run, observed[0], source, command)
      assert.equal(coreCommands(devicePath.deviceData, job.job_id).length, 0,
        'catching a nested host error cannot authorize a later effect in the same fatal cell')
      assert.equal(cp.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id=? AND outcome=?')
        .get(job.job_id, 'completed').count, 0)
      await assertPublicSafe(api, productSessionId)
      return { jobId: job.job_id, scope: 'core_required_tool_claim', originalRequestSequence: observed[0].sequence,
        rawOutcome: raw.outcome.status, privateCause,
        immutableOutcomeDigest: worker.digest, businessAcknowledged: settled.worker.workerAcknowledged,
        originalCommandAttempts: 0 }
    } finally { core.exec('DROP TRIGGER IF EXISTS refuse_original_core_tool_claim'); core.close() }
  })
})
