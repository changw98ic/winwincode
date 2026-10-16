// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { createHash, randomBytes } from 'node:crypto'
import { existsSync, globSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { createServer } from 'node:https'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { isDeepStrictEqual } from 'node:util'

import { assertCompletedDelivery, createCertificate, driveDelivery,
  runApiProductionVertical, workItemCreatePayload } from '../scripts/run-api-production-vertical.mjs'
import { configuredDeviceModelRoute, deviceFixtureCodeModeCommand,
  deterministicVerificationProduct, DETERMINISTIC_EXECUTOR_BEHAVIOR_MARKER,
  DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER, observeToolProcess,
  seedDeviceLocalProvider, waitFor, workInputFromRequest,
  writeDeviceTrustRoot } from '../scripts/device-production-fixture.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import { assertSuccessfulRetainedCommands, readRetainedCoreCommandResults,
  readRetainedOutcomes } from './fixtures/chat-execution-transport-probe.mjs'
import { fusionMemberProvider } from './fixtures/fusion-member-provider.mjs'

const CLAIM_KEY = 'claim:frozen-task'
const VERIFY = "git show HEAD:TASK.md | grep -Fx 'status: complete'"
const MEMBERS = ['a', 'b', 'c'].map(id => ({ id: 'member-' + id,
  provider: 'owned-fusion-' + id, model: 'owned-fusion-model-' + id, reasoning: 'high' }))
const digest = bytes => 'sha256:' + createHash('sha256').update(bytes).digest('hex')
const parsed = bytes => JSON.parse(Buffer.from(bytes).toString('utf8'))

function database(path) {
  const result = new DatabaseSync(path, { readOnly: true })
  result.exec('PRAGMA busy_timeout=5000')
  return result
}

function sealedBinaries() {
  const installed = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  return installed === undefined ? { build: false } : { build: false,
    serverBinary: join(installed, 'winwincode-server'),
    wwcBinary: join(installed, 'wwc') }
}

// This Provider returns deterministic Anthropic SSE. It chooses conclusions
// only after the original Core command output supplies a real sealed exit.
async function primaryProvider(certificate, apiKey) {
  const records = []
  const errors = []
  const server = createServer({ cert: readFileSync(certificate.cert), key: readFileSync(certificate.key) },
    async (incoming, outgoing) => {
      try {
        assert.equal(incoming.method, 'POST')
        assert.equal(incoming.url, '/v1/messages')
        assert.equal(incoming.headers.authorization, 'Bearer ' + apiKey)
        const parts = []
        let size = 0
        for await (const part of incoming) {
          size += part.length
          assert.ok(size <= 2 * 1024 * 1024)
          parts.push(part)
        }
        const bytes = Buffer.concat(parts)
        const request = parsed(bytes)
        const text = bytes.toString('utf8')
        const executor = text.includes(DETERMINISTIC_EXECUTOR_BEHAVIOR_MARKER)
        const verification = text.includes(DETERMINISTIC_VERIFICATION_BEHAVIOR_MARKER)
        const fusion = text.includes('Fusion independent analysis')
        const workInput = workInputFromRequest(request)
        const callId = executor ? 'owned-fusion-executor-command' : 'owned-fusion-verification-command'
        const observed = observeToolProcess(request, callId)
        const exec = request.tools?.find(tool => /(?:^|__)exec$/u.test(tool.name))
        const wait = request.tools?.find(tool => /(?:^|__)wait$/u.test(tool.name))
        let tool = null
        let input = null
        let toolId = callId
        let answer = 'The source Chat is ready for the authorized Delivery.'
        if (executor || verification) {
          assert.ok(workInput !== null)
          if (observed.cellId !== null) {
            tool = wait
            input = { cell_id: observed.cellId, yield_time_ms: 1000 }
            toolId = observed.nextCallId
          } else if (observed.sessionId !== null) {
            tool = exec
            input = { input: deviceFixtureCodeModeCommand({ session_id: observed.sessionId,
              chars: '', yield_time_ms: 1000 }) }
            toolId = observed.nextCallId
          } else if (!observed.hasToolOutput) {
            tool = exec
            input = { input: deviceFixtureCodeModeCommand({ workdir: '.', yield_time_ms: 1000,
              cmd: executor ? "printf '%s\\n' 'status: complete' > TASK.md; git status --porcelain=v1 --untracked-files=all"
                : workInput.verificationCommand }) }
          } else {
            assert.notEqual(observed.exitCode, null, 'a final model result needs the actual original command exit')
            if (executor) {
              assert.equal(observed.exitCode, 0)
              answer = 'The candidate stage command completed.'
            } else {
              const product = JSON.parse(deterministicVerificationProduct({
                passed: observed.exitCode === 0, workInput, evidenceSourceId: observed.evidenceSourceId }))
              if (fusion) product.fusion_investigations = [{ claim_key: CLAIM_KEY,
                status: 'investigated', evidence_sources: [{ source_id: observed.evidenceSourceId }] }]
              answer = JSON.stringify(product)
            }
          }
          if (input !== null) assert.ok(tool, 'the sealed Core exec/wait tool must be available')
        }
        records.push({ executor, verification, fusion, requestDigest: digest(bytes),
          workInput, sourceId: observed.evidenceSourceId, exitCode: observed.exitCode,
          final: input === null, finalProduct: verification && input === null ? JSON.parse(answer) : null })
        outgoing.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', connection: 'close' })
        const event = (type, value) => outgoing.write('event: ' + type + '\ndata: ' + JSON.stringify({ type, ...value }) + '\n\n')
        event('message_start', { message: { id: 'owned-main-' + records.length, type: 'message', role: 'assistant',
          model: request.model, content: [], stop_reason: null, stop_sequence: null,
          usage: { input_tokens: 10, output_tokens: 0 } } })
        event('content_block_start', { index: 0, content_block: input === null
          ? { type: 'text', text: '' } : { type: 'tool_use', id: toolId, name: tool.name, input: {} } })
        event('content_block_delta', { index: 0, delta: input === null
          ? { type: 'text_delta', text: answer } : { type: 'input_json_delta', partial_json: JSON.stringify(input) } })
        event('content_block_stop', { index: 0 })
        event('message_delta', { delta: { stop_reason: input === null ? 'end_turn' : 'tool_use', stop_sequence: null },
          usage: { output_tokens: 5 } })
        event('message_stop', {})
        outgoing.end()
      } catch (error) {
        errors.push(error.message)
        if (!outgoing.headersSent) outgoing.writeHead(500, { connection: 'close' })
        outgoing.end('Owned StrongFlow Provider failed\n')
      }
    })
  await new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolve)
  })
  return { endpoint: 'https://127.0.0.1:' + server.address().port + '/v1/messages', records, errors,
    async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)) } }
}

function workerFacts(deviceData) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const db = database(join(deviceData, path))
    try {
      if (!db.prepare("SELECT 1 FROM sqlite_master WHERE name='codex_run'").get()) return []
      return db.prepare('SELECT run_key,record_json FROM codex_run').all().map(row => {
        const record = parsed(row.record_json)
        const panels = db.prepare('SELECT panel_id,input_json,result_json FROM fusion_panel WHERE run_key=?')
          .all(row.run_key).map(panel => ({ id: panel.panel_id, input: parsed(panel.input_json),
            inputDigest: digest(panel.input_json), result: panel.result_json === null ? null : parsed(panel.result_json) }))
        const opens = db.prepare("SELECT frame_json FROM execution_outbox WHERE family='model_open'")
          .all().map(item => parsed(item.frame_json)).filter(frame => frame.lease.jobId === record.job.jobId)
        const terminal = db.prepare('SELECT frame_json FROM execution_terminal_outcome WHERE run_key=?').get(row.run_key)
        const runtime = db.prepare('SELECT frame FROM runtime_replay_frame ORDER BY stream_key,sequence')
          .all().map(item => parsed(item.frame)).filter(message => message.lease.jobId === record.job.jobId)
          .map(message => {
            const payload = message.event.payload
            if (payload === null || payload === undefined) return { message, payload: null }
            const bytes = Buffer.from(payload.dataBase64, 'base64')
            assert.equal(digest(bytes), payload.payloadDigest)
            return { message, payload: payload.contentType === 'application/json' ? parsed(bytes) : null }
          })
        return { path: join(deviceData, path), runKey: row.run_key, record, panels, opens, runtime,
          terminal: terminal ? parsed(terminal.frame_json) : null }
      })
    } finally { db.close() }
  })
}

function memberExchanges(deviceData, jobId) {
  return globSync('**/providers.sqlite3', { cwd: deviceData }).flatMap(path => {
    const db = database(join(deviceData, path))
    try {
      return db.prepare('SELECT exchange_id,request_open,chunks FROM exchanges WHERE request_open IS NOT NULL')
        .all().flatMap(row => {
          const open = JSON.parse(row.request_open)
          if (open.lease.jobId !== jobId) return []
          const request = parsed(Buffer.from(open.request.dataBase64, 'base64'))
          if (!MEMBERS.some(member => member.provider === request.provider)) return []
          return [{ open, request, chunks: row.chunks,
            attempts: db.prepare('SELECT * FROM model_invocation_attempts WHERE exchange_id=? ORDER BY attempt_number')
              .all(row.exchange_id) }]
        })
    } finally { db.close() }
  })
}

function assertFrozenPanel(fact, detail) {
  assert.equal(fact.record.job.executionProfile, 'reviewer')
  assert.equal(fact.record.job.workspace.writeMode, 'read-only')
  assert.ok(fact.record.snapshotId)
  assert.equal(fact.record.job.workInput.candidateRef, detail.currentCandidate.candidateRef)
  assert.equal(fact.panels.length, 1)
  const [binding, input] = fact.panels[0].input
  assert.equal(binding.snapshot, fact.record.agentConfig.snapshotDigest)
  assert.equal(binding.thread, fact.record.canonicalThreadId)
  assert.equal(binding.session, fact.record.kernelSessionId)
  assert.equal(binding.turn, fact.record.submissionId)
  const context = input.canonicalContext
  assert.equal(context.snapshotId, fact.record.snapshotId)
  assert.deepEqual(context.workInput, fact.record.job.workInput)
  assert.equal(context.candidate.commit, detail.currentCandidate.candidateCommitId)
  assert.equal(context.candidate.tree, detail.currentCandidate.candidateTreeId)
  assert.equal(fact.record.workspaceRevision, 'git-tree:' + context.candidate.tree)
  const task = context.candidate.files.find(file => file.path === 'TASK.md')
  assert.equal(task?.content, 'status: complete\n')
  assert.match(task.objectId, /^[0-9a-f]{40}$/u)
  assert.deepEqual(fact.record.agentConfig.profile.source.settings.fusion.members, MEMBERS)
  return context
}

async function startDelivery(context, productSessionId, deliveryId) {
  const { api, devicePath, modelRoute, baseline } = context
  await api.command('session.create', 0, { productSessionId,
    projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
    title: 'StrongFlow sealed Fusion source', modelRoute })
  await devicePath.launchAnchor({ productSessionId })
  const created = await api.command('delivery.create', 0, { deliveryId, spec: {
    acceptanceCriteria: [{ id: 'frozen-task-criterion', required: true, title: 'The frozen TASK.md contains status: complete' }],
    baseRevision: baseline, constraints: ['Use the immutable frozen candidate'], goal: 'Complete TASK.md and verify its frozen Git object',
    outOfScope: [], publicationTarget: null, repositoryId: 'rep_01J00000000000000000000000',
    scope: ['TASK.md'], sourceProductSessionId: productSessionId, verificationCommand: VERIFY,
    title: 'StrongFlow sealed reviewer Fusion' } })
  assert.equal(created.outcome, 'completed')
  const aggregate = (await api.query('workrun.get', { deliveryId, workItemId: null, atCursor: null })).result
  assert.ok(aggregate.contract.criteria.every(criterion => criterion.required && criterion.requiredEvidenceClass === 'machine'))
  const items = workItemCreatePayload(aggregate, created.currentRevision)
  if (items !== null) await api.command('workitems.create', created.currentRevision, items)
  const detail = (await api.query('delivery.get', { deliveryId })).result
  const started = await api.command('workrun.start', detail.deliveryRevision, { deliveryId, dispatchProfile: 'executor' })
  assert.equal(started.outcome, 'completed')
  const anchored = new Set()
  const anchor = async id => {
    assert.equal(typeof id, 'string')
    if (anchored.has(id)) return
    await devicePath.forProductSession(productSessionId).launchAnchor({ workRunId: id, deliveryId })
    anchored.add(id)
  }
  await anchor(started.result.activeWorkRunId ?? started.result.workRunId)
  return { anchor, anchored }
}

function queuedRunIds(cp, deliveryId) {
  return cp.prepare("SELECT work_run_id FROM scheduler_execution_jobs WHERE delivery_id=? AND work_run_id IS NOT NULL AND state IN ('queued','leased','running') ORDER BY job_id")
    .all(deliveryId).map(row => row.work_run_id)
}

async function advanceToReviewer(context, deliveryId, launches, cp) {
  return waitFor(async () => {
    const detail = (await context.api.query('delivery.get', { deliveryId })).result
    const aggregate = (await context.api.query('workrun.get', { deliveryId, workItemId: null, atCursor: null })).result
    // Queued Device jobs need a Client launch grant before their leased
    // WorkRuns appear in workrun.get. Observe the real Controller dispatch.
    for (const id of queuedRunIds(cp, deliveryId)) await launches.anchor(id)
    for (const run of aggregate.runs.filter(run => ['queued', 'leased', 'running'].includes(run.state))) await launches.anchor(run.id)
    const runIds = new Set(aggregate.runs.map(run => run.id))
    const reviewer = workerFacts(context.devicePath.deviceData)
      .find(fact => runIds.has(fact.record.job.scope?.workRunId) && fact.record.job.executionProfile === 'reviewer')
    return reviewer?.panels.length === 1 ? { reviewer, detail, aggregate } : false
  }, 'Controller executor freezes its candidate and dispatches one sealed reviewer panel', 120_000)
}

function readEvents(record) {
  if (!record.rolloutPath || !existsSync(record.rolloutPath)) return []
  return readFileSync(record.rolloutPath, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse)
    .filter(event => event.type === 'event_msg').map(event => event.payload)
}

function assertAcceptedReviewEvidence(deviceData, reviewer, finalReview, detail) {
  const commands = reviewer.runtime.filter(event => ['command', 'test'].includes(event.message.event.category)
    && event.payload?.source_id === finalReview.sourceId)
  assert.equal(commands.length, 1, 'the cited source belongs to one original retained reviewer command')
  const command = commands[0]
  assert.equal(command.payload.exit_code, 0)
  assert.equal(command.payload.status, 'completed')
  const original = readRetainedCoreCommandResults(deviceData, reviewer.record.kernelSessionId)
    .filter(receipt => parsed(receipt.accepted_result).response.call_id === finalReview.sourceId)
  assertSuccessfulRetainedCommands(original, 1)
  assert.equal(original[0].delivery, 'offered')
  for (const field of ['execution_result', 'accepted_result']) {
    const result = parsed(original[0][field])
    assert.equal(result.response.call_id, finalReview.sourceId,
      'the runtime command traces to the original embedded Core command receipt')
    assert.equal(parsed(result.effective_input.input).cmd, VERIFY,
      'the Core receipt records the assigned exact verification method')
  }
  const results = reviewer.runtime.filter(event => event.message.event.category === 'activity'
    && event.payload?.protocol === 'winwincode.independent-verification-result.v1')
  assert.equal(results.length, 1, 'the original reviewer binds one canonical verification result')
  assert.equal(results[0].payload.candidate_ref, detail.currentCandidate.candidateRef)
  const sources = [{ type: command.message.event.category, event_id: command.message.event.eventId }]
  assert.deepEqual(results[0].payload.fusion_investigations, [{ claim_key: CLAIM_KEY,
    status: 'investigated', evidence_sources: sources }])
  assert.ok(results[0].payload.findings.some(finding => finding.evidence_sources.some(source =>
    source.event_id === command.message.event.eventId)), 'the accepted finding cites the same direct command')
  const evidence = detail.evidence.filter(item => item.sourceRef === 'runtime_event:' + command.message.event.eventId)
  assert.ok(evidence.length > 0, 'server Evidence resolves the exact retained original reviewer command event')
  assert.ok(evidence.every(item => item.type === command.message.event.category
    && item.workRunId === reviewer.record.job.scope.workRunId
    && item.snapshotId === reviewer.record.snapshotId
    && item.candidateId === detail.currentCandidate.candidateId),
  'the server Evidence retains the original direct type, reviewer, snapshot and candidate')
  return command.message.event.eventId
}

function accountingStatement(cp, jobId, exchanges) {
  const expectedSlots = exchanges.map(exchange => 'primary:' + exchange.open.modelExchangeId).sort()
  return cp.prepare('SELECT statement_digest,statement_json FROM execution_attempt_accounting_facts WHERE job_id=? AND attempt=1')
    .all(jobId).map(row => {
      assert.equal(digest(row.statement_json), row.statement_digest)
      return { statement: JSON.parse(row.statement_json), digest: row.statement_digest }
    }).find(record => JSON.stringify(record.statement.manifest) === JSON.stringify(expectedSlots)) ?? null
}

function leasePeriod(authority) {
  return Object.fromEntries(['job_id', 'lease_id', 'payload_digest', 'worker_id', 'worker_instance_id',
    'attempt', 'fencing_token', 'issued_at', 'expires_at'].map(field => [field, authority[field]]))
}

function assertAcceptedLeasePeriod(cp, period) {
  const receipts = cp.prepare(`SELECT response_json FROM execution_lease_request_receipts
    WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(period.job_id, period.expires_at)
  assert.ok(receipts.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, period)),
    'the complete original lease period belongs to a real accepted claim or renewal receipt')
}

function assertOriginalAdmittedLease(cp, authority, job, stamp) {
  const original = leasePeriod(authority)
  const current = leasePeriod(cp.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(authority.job_id))
  const terminal = { job_id: stamp.jobId, lease_id: stamp.leaseId, payload_digest: job.payload_digest,
    worker_id: stamp.workerId, worker_instance_id: stamp.workerInstanceId, attempt: stamp.attempt,
    fencing_token: stamp.fencingToken, issued_at: stamp.issuedAt, expires_at: stamp.expiresAt }
  assert.equal(job.payload_digest, original.payload_digest)
  for (const field of Object.keys(original).filter(field => field !== 'expires_at')) {
    assert.equal(current[field], original[field])
    assert.equal(terminal[field], original[field])
  }
  for (const period of [original, current, terminal]) assertAcceptedLeasePeriod(cp, period)
  assert.ok(Date.parse(terminal.expires_at) >= Date.parse(original.expires_at))
  assert.ok(Date.parse(current.expires_at) >= Date.parse(terminal.expires_at))
}

async function waitOriginalTerminal(cp, deviceData, jobId, outcome) {
  const expectedQueueState = outcome === 'cancelled' ? 'failed' : outcome
  return waitFor(() => {
    const workers = readRetainedOutcomes(deviceData, jobId)
    const raw = cp.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
      .get('execution-outcome:' + jobId + ':1')
    const accepted = raw ? parsed(raw.payload) : null
    const terminals = cp.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').all(jobId)
    const slot = cp.prepare('SELECT * FROM worker_session_slots WHERE job_id=?').get(jobId)
    const job = cp.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId)
    const authority = cp.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(jobId)
    const reservation = cp.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId)
    if (workers.length !== 1 || !workers[0].workerAcknowledged || raw?.revision !== 2
      || !accepted?.acknowledgementReady || terminals.length !== 1 || terminals[0].outcome !== outcome
      || slot?.state !== outcome || job?.state !== expectedQueueState
      || reservation?.state !== (outcome === 'cancelled' ? 'released' : 'settled')) return false
    const worker = workers[0]
    assert.deepEqual(accepted.message, worker.message, 'the server accepts the exact immutable original Worker terminal')
    assert.equal(worker.message.outcome.status, outcome === 'completed' ? 'succeeded' : outcome)
    for (const [column, key] of [['job_id', 'jobId'], ['lease_id', 'leaseId'], ['worker_id', 'workerId'],
      ['worker_instance_id', 'workerInstanceId'], ['attempt', 'attempt'], ['fencing_token', 'fencingToken']]) {
      assert.equal(terminals[0][column], worker.message.lease[key])
      assert.equal(authority[column], worker.message.lease[key])
      assert.equal(slot[column], worker.message.lease[key])
    }
    assertOriginalAdmittedLease(cp, authority, job, worker.message.lease)
    assert.equal(worker.message.lease.attempt, 1)
    assert.equal(worker.message.sessionIdentity.workerSessionId, worker.message.workerSessionId)
    assert.equal(slot.worker_session_id, worker.message.workerSessionId)
    assert.equal(authority.worker_session_id, worker.message.workerSessionId)
    assert.equal(slot.codex_thread_id, worker.message.sessionIdentity.codexThreadId)
    assert.equal(job.product_session_id, worker.message.sessionIdentity.productSessionId)
    assert.equal(job.work_run_id, worker.message.sessionIdentity.workRunId)
    return { worker, accepted, reservation, slot, terminal: terminals[0] }
  }, 'the original ' + outcome + ' terminal reaches its exact business ACK and releases its own Worker slot', 120_000)
}

for (const mode of ['complete-after-shared-slot-contention', 'cancel-in-flight-panel']) {
  test('StrongFlow Fusion production: ' + mode, async t => {
    const directory = mkdtempSync(join(tmpdir(), 'wwc-strongflow-fusion-'))
    const certificate = createCertificate(directory)
    writeDeviceTrustRoot({ certificatePath: certificate.cert,
      destination: join(directory, 'device-provider-trust-root.der') })
    const apiKey = 'owned-fusion-secret-' + randomBytes(24).toString('hex')
    const primary = await primaryProvider(certificate, apiKey)
    const responses = Object.fromEntries(MEMBERS.map((member, index) => [member.model, { claims: [{
      claimKey: CLAIM_KEY, summary: index === 2 ? 'TASK.md may contain an incomplete status; inspect the frozen object.'
        : 'TASK.md may contain a complete status; inspect the frozen object.',
      position: index === 2 ? 'opposes' : 'supports', evidence: [], requiredEvidence: ['command', 'runtime_event'] }] }]))
    const members = fusionMemberProvider({ certificatePath: certificate.cert, privateKeyPath: certificate.key,
      apiKey, responsesByModel: responses, gateModels: MEMBERS.map(member => member.model) })
    const memberEndpoint = await members.listen()
    const originalTrust = process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE
    process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE = join(directory, 'device-provider-trust-root.der')
    let completed = false
    t.after(async () => {
      await primary.close()
      await members.close()
      if (originalTrust === undefined) delete process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE
      else process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE = originalTrust
      if (completed) rmSync(directory, { recursive: true, force: true })
      else t.diagnostic('Unverified StrongFlow Fusion production fixture retained at ' + directory)
    })
    const productSessionId = 'psn_01J00000000000000000000091'
    const deliveryId = 'dlv_01J00000000000000000000091'
    const report = await runApiProductionVertical({ directory, restart: false, repeat: false,
      includeStrongFlow: false, timeoutMillis: 300_000, ...sealedBinaries(),
      deviceAgentEnvironment: { WWC_WORKER_FUSION: JSON.stringify({ members: MEMBERS }) },
      deviceProvider: { providerId: 'owned-fusion-primary', modelId: 'owned-fusion-primary-model',
        endpoint: primary.endpoint, protocol: 'anthropic_messages', displayName: 'Owned StrongFlow Core Provider' },
      deviceProviderSecrets: [apiKey],
      scenario: { files: { 'TASK.md': 'status: pending\n' }, verificationCommand: VERIFY, async run(context) {
        const { api, devicePath } = context
        const cp = database(join(directory, 'server-data/control-plane.sqlite3'))
        try {
          const routes = []
          for (const member of MEMBERS) {
            const saved = await seedDeviceLocalProvider({ api, publicClientId: devicePath.publicClientId,
              providerId: member.provider, modelId: member.model, endpoint: memberEndpoint.endpoint,
              protocol: memberEndpoint.protocol, apiKey })
            assert.equal(saved.receiptOutcome, 'saved')
            routes.push(configuredDeviceModelRoute({ clientNodeId: saved.clientNodeId,
              providerId: member.provider, modelId: member.model }))
          }
          if (mode === 'complete-after-shared-slot-contention') {
            for (const suffix of ['92', '93', '94']) {
              const id = 'psn_01J000000000000000000000' + suffix
              await api.command('session.create', 0, { productSessionId: id,
                projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
                title: 'Occupy the real shared member A Provider slot', modelRoute: routes[0] })
              await devicePath.launchAnchor({ productSessionId: id })
              await api.command('chat.submit', 1, { productSessionId: id, message: 'Hold this real Provider response for the shared slot test.' })
            }
            await waitFor(() => members.records.filter(record => record.model === MEMBERS[0].model && record.active).length === 3,
              'three ordinary Chat attempts hold every Device-wide member A Provider slot', 60_000)
          }
          const launches = await startDelivery(context, productSessionId, deliveryId)
          const active = await advanceToReviewer(context, deliveryId, launches, cp)
          const frozenContext = assertFrozenPanel(active.reviewer, active.detail)
          await waitFor(() => members.records.filter(record => record.model !== MEMBERS[0].model && record.active).length === 2,
            'the same reviewer opens B and C before either member responds', 60_000)
          const panelOwner = await waitFor(() => {
            const fact = workerFacts(devicePath.deviceData).find(item => item.record.job.jobId === active.reviewer.record.job.jobId)
            const opens = fact?.opens.filter(open => MEMBERS.some(member => member.provider ===
              parsed(Buffer.from(open.request.dataBase64, 'base64')).provider)) ?? []
            return opens.length === 3 ? fact : false
          }, 'the same original panel retains all three member ModelOpen messages before release', 30_000)
          assert.equal(panelOwner.panels[0].inputDigest, active.reviewer.panels[0].inputDigest)
          assert.deepEqual(assertFrozenPanel(panelOwner, active.detail), frozenContext)
          assert.equal(primary.records.filter(record => record.fusion).length, 0,
            'the main reviewer Core waits for the original panel')
          const jobId = active.reviewer.record.job.jobId
          if (mode === 'complete-after-shared-slot-contention') {
            assert.equal(members.records.filter(record => record.model === MEMBERS[0].model).length, 3,
              'the reviewer A request cannot become a fourth real physical invocation')
            const unstarted = panelOwner.opens.map(open => ({ open,
              request: parsed(Buffer.from(open.request.dataBase64, 'base64')) }))
              .find(item => item.request.provider === MEMBERS[0].provider)
            assert.ok(unstarted, 'the Worker retains the original deferred member A ModelOpen')
            assert.ok(!memberExchanges(devicePath.deviceData, jobId).some(exchange => exchange.request.provider === MEMBERS[0].provider),
              'Provider slot deferral records no first-start exchange or paid physical attempt')
            members.release(MEMBERS[0].model)
            await waitFor(() => members.records.filter(record => record.model === MEMBERS[0].model).length === 4,
              'the original member A request starts after an ordinary Chat returns a shared slot', 60_000)
            for (const member of MEMBERS.slice(1)) members.release(member.model)
            const terminal = await driveDelivery(api, 180_000, context.modelRoute, Date.now, {
              deliveryId, expectDeviceWorkRun: true, strictLaunchAnchor: true,
              resolveAttention: 'verified-candidate',
              pendingDeviceWorkRunIds: () => [...new Set([...launches.anchored, ...queuedRunIds(cp, deliveryId)])],
              onActiveWorkRuns: async runs => { for (const run of runs) await launches.anchor(run.id) },
              onPendingDeviceWorkRuns: async ids => { for (const id of ids) await launches.anchor(id) },
            })
            assertCompletedDelivery(terminal.detail, terminal.workRunAggregate)
            const runIds = new Set(terminal.workRunAggregate.runs.map(run => run.id))
            const allFacts = workerFacts(devicePath.deviceData).filter(fact => runIds.has(fact.record.job.scope?.workRunId))
            assert.equal(allFacts.length, 3, 'the enclosing Delivery retains one original run for each of its three roles')
            assert.deepEqual([...new Set(allFacts.map(fact => fact.record.job.executionProfile))].sort(), ['executor', 'reviewer', 'verifier'])
            for (const fact of allFacts) {
              const settled = await waitOriginalTerminal(cp, devicePath.deviceData, fact.record.job.jobId, 'completed')
              assert.deepEqual(settled.worker.message, fact.terminal)
            }
            const reviewer = allFacts.find(fact => fact.record.job.jobId === jobId)
            assert.equal(reviewer.panels[0].inputDigest, active.reviewer.panels[0].inputDigest)
            assert.equal(reviewer.panels[0].result.Ok.candidates.length, 3)
            const positions = reviewer.panels[0].result.Ok.candidates.map(candidate => candidate.answer.claims[0].position)
            assert.deepEqual(positions.sort(), ['opposes', 'supports', 'supports'])
            const exchanges = memberExchanges(devicePath.deviceData, jobId)
            assert.equal(exchanges.length, 3)
            for (const exchange of exchanges) {
              assert.deepEqual(exchange.open.sessionIdentity, unstarted.open.sessionIdentity)
              assert.deepEqual(exchange.open.lease, unstarted.open.lease)
              assert.equal(exchange.open.sessionIdentity.workRunId, reviewer.record.job.scope.workRunId)
              if (exchange.request.provider === MEMBERS[0].provider) assert.deepEqual(exchange.open, unstarted.open,
                'the released shared slot starts the exact original deferred member A request')
              assert.equal(exchange.attempts.length, 1)
              assert.equal(exchange.attempts[0].state, 'completed')
              const prompt = JSON.parse(exchange.request.request.input[0].content[0].text)
              assert.deepEqual(prompt.canonicalContext, frozenContext)
            }
            const finalReview = primary.records.find(record => record.fusion && record.final)
            assert.equal(finalReview?.exitCode, 0)
            assert.deepEqual(finalReview.finalProduct.fusion_investigations, [{ claim_key: CLAIM_KEY,
              status: 'investigated', evidence_sources: [{ source_id: finalReview.sourceId }] }])
            const commandEventId = assertAcceptedReviewEvidence(devicePath.deviceData, reviewer, finalReview, terminal.detail)
            const receipts = readDeviceExecutionReceipts(directory, { deliveryId, productSessionId })
            assert.equal(receipts.calls.filter(call => call.jobId === jobId && MEMBERS.some(member => member.provider === call.provider)).length, 3)
            const reviewerCalls = receipts.calls.filter(call => call.jobId === jobId)
            assert.equal(reviewerCalls.length, 3 + primary.records.filter(record => record.fusion).length)
            const totalTokens = 15 * reviewerCalls.length
            assert.equal(reviewer.terminal.outcome.usage.accountingStatus, 'known')
            assert.equal(reviewer.terminal.outcome.usage.tokens, totalTokens)
            assert.equal(reviewer.terminal.outcome.usage.knownTokens, totalTokens)
            assert.equal(reviewer.terminal.outcome.usage.costMicrounits, null)
            const accounted = await waitFor(() => cp.prepare('SELECT * FROM execution_attempt_accounting_totals WHERE job_id=? AND attempt=1').get(jobId),
              'the original reviewer accounting includes all physical member and main Core calls', 30_000)
            assert.equal(accounted.tokens, totalTokens)
            assert.equal(accounted.known_tokens, totalTokens)
            assert.equal(accounted.cost, null)
            assert.equal(members.peakActiveRequests, 5)
            assert.equal(primary.errors.length, 0)
            return { mode, deliveryId, reviewerJobId: jobId, candidateRef: terminal.detail.currentCandidate.candidateRef,
              memberCalls: exchanges.length, reviewerCommandEventId: commandEventId,
              reviewerKnownTokens: totalTokens,
              requiredMachineEvidence: terminal.detail.evidence.length,
              roles: allFacts.map(fact => fact.record.job.executionProfile), status: terminal.detail.status }
          }
          await waitFor(() => members.activeRequests === 3, 'all sealed reviewer members enter real HTTPS before cancellation', 60_000)
          const beforeCount = members.records.length
          const beforeFacts = memberExchanges(devicePath.deviceData, jobId)
          assert.equal(beforeFacts.length, 3)
          assert.ok(beforeFacts.every(exchange => exchange.attempts.length === 1 && exchange.attempts[0].state === 'invoking'),
            'each observed HTTPS request has its original real invoking physical attempt')
          const current = (await api.query('delivery.get', { deliveryId })).result
          const workRunId = beforeFacts[0].open.sessionIdentity.workRunId
          await api.command('workrun.cancel', current.deliveryRevision, { deliveryId, workRunId })
          const cancelled = await waitFor(() => {
            const fact = workerFacts(devicePath.deviceData).find(item => item.record.job.jobId === jobId)
            const leaseTerminal = cp.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(jobId)
            const accepted = cp.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
              .get('execution-outcome:' + jobId + ':1')
            const reservation = cp.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId)
            return fact?.terminal && leaseTerminal?.outcome === 'cancelled' && accepted?.revision === 2
              && parsed(accepted.payload).acknowledgementReady && reservation?.state === 'released'
              ? { fact, reservation, outcome: parsed(accepted.payload) } : false
          }, 'the same reviewer terminal and reservation settle cancelled through the real API', 30_000)
          assert.deepEqual(cancelled.outcome.message, cancelled.fact.terminal)
          assert.equal(cancelled.fact.terminal.outcome.status, 'cancelled')
          const usage = cancelled.fact.terminal.outcome.usage
          assert.equal(usage?.accountingStatus, 'unknown')
          assert.equal(usage.tokens, null)
          assert.equal(usage.knownTokens, 0)
          assert.equal(usage.costMicrounits, null)
          assert.equal(cancelled.reservation.release_reason, 'cancelled')
          const acknowledged = await waitOriginalTerminal(cp, devicePath.deviceData, jobId, 'cancelled')
          assert.deepEqual(acknowledged.worker.message, cancelled.fact.terminal)
          assert.equal(cancelled.fact.panels[0].inputDigest, active.reviewer.panels[0].inputDigest)
          const originalTerminal = cancelled.fact.terminal
          for (const member of MEMBERS) members.release(member.model)
          await waitFor(() => members.activeRequests === 0, 'cancelled physical member attempts release every shared Provider slot', 10_000)
          const latest = workerFacts(devicePath.deviceData).find(fact => fact.record.job.jobId === jobId)
          assert.deepEqual(latest.terminal, originalTerminal)
          assert.equal(latest.record.currentTurnId, null, 'the cancelled panel leaves the original main Core turn unstarted')
          assert.equal(primary.records.filter(record => record.fusion).length, 0)
          assert.equal(readEvents(latest.record).filter(event => ['task_started', 'turn_started', 'exec_command_begin'].includes(event.type)).length, 0,
            'late member results cannot start the original main reviewer turn or a tool')
          assert.equal(members.records.length, beforeCount)
          assert.equal(cp.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id=?').get(jobId).count, 1)
          const totals = await waitFor(() => cp.prepare('SELECT * FROM execution_attempt_accounting_totals WHERE job_id=? AND attempt=1').get(jobId),
            'the signed late member accounting statement is independently retained', 30_000)
          assert.equal(totals.tokens, null)
          assert.equal(totals.known_tokens, 0)
          assert.equal(totals.cost, null)
          const accountedStatement = await waitFor(() => accountingStatement(cp, jobId, beforeFacts),
            'the server accepts the complete original three-member accounting manifest', 30_000)
          assert.match(accountedStatement.digest, /^sha256:[0-9a-f]{64}$/u)
          for (const key of ['jobId', 'leaseId', 'workerId', 'workerInstanceId', 'attempt', 'fencingToken', 'issuedAt']) {
            assert.equal(accountedStatement.statement.lease[key], originalTerminal.lease[key])
          }
          assert.equal(accountedStatement.statement.receipts.length, 3)
          for (const receipt of accountedStatement.statement.receipts) {
            const exchange = beforeFacts.find(item => item.open.modelExchangeId === receipt.modelExchangeId)
            assert.ok(exchange, 'the receipt belongs to an original retained member exchange')
            assert.equal(receipt.providerId, exchange.request.provider)
            assert.equal(receipt.tokens, null)
            assert.equal(receipt.knownTokens ?? 0, 0)
            assert.equal(receipt.costMicrounits, null)
          }
          assert.equal(primary.errors.length, 0)
          return { mode, deliveryId, reviewerJobId: jobId, workRunId, memberCalls: beforeCount,
            status: 'cancelled', accountingStatus: usage.accountingStatus, knownTokens: totals.known_tokens,
            accountingStatementDigest: accountedStatement.digest }
        } finally { cp.close() }
      } },
    })
    assert.equal(report.flow.scenario.mode, mode)
    t.diagnostic(JSON.stringify(report.flow.scenario))
    completed = true
  })
}
