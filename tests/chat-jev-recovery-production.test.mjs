// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { globSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { createServer } from 'node:https'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'

import { createCertificate, runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'
import { waitFor, writeDeviceTrustRoot } from '../scripts/device-production-fixture.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'

const digest = bytes => `sha256:${createHash('sha256').update(bytes).digest('hex')}`

function canonicalJson(value) {
  if (Array.isArray(value)) return value.map(canonicalJson)
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(Object.keys(value).sort().map(key => [key, canonicalJson(value[key])]))
  }
  return value
}

function sealedBinaries() {
  const installed = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  return installed === undefined ? { build: false } : {
    build: false,
    serverBinary: join(installed, 'winwincode-server'),
    wwcBinary: join(installed, 'wwc'),
  }
}

// Real HTTPS Responses and SystemOne protocols. Only these owned endpoints
// produce deterministic model output; Worker, Core, preparation and accounting
// use their production paths and retain their original inputs and receipts.
async function startJevChatProvider(certificate) {
  const requests = []
  const errors = []
  const server = createServer({ key: readFileSync(certificate.key), cert: readFileSync(certificate.cert) },
    async (request, response) => {
      try {
        assert.equal(request.method, 'POST')
        const parts = []
        let size = 0
        for await (const part of request) {
          size += part.length
          assert.ok(size <= 2 * 1024 * 1024, 'owned HTTPS request exceeds bound')
          parts.push(part)
        }
        const bytes = Buffer.concat(parts)
        const payload = JSON.parse(bytes)
        requests.push({ path: request.url, bytes, payload })
        if (request.url === '/jev') {
          assert.ok(Object.keys(payload.questions).length > 0)
          const answers = Object.fromEntries(Object.keys(payload.questions).map(key => [key, {
            type: 'choice', choice: 'neutral', confidence: 0.8,
            probabilities: { entailment: 0.1, contradiction: 0.1, neutral: 0.8 },
          }]))
          response.writeHead(200, { 'content-type': 'application/json', connection: 'close' })
          response.end(JSON.stringify({ model: payload.model, answers,
            usage: { input_tokens: 7, output_tokens: 2 } }))
          return
        }
        assert.equal(request.url, '/v1/responses')
        assert.ok(Array.isArray(payload.input))
        assert.deepEqual(payload.tools.map(tool => tool.name).sort(), ['exec', 'wait'])
        const outputs = payload.input.filter(item => ['custom_tool_call_output', 'function_call_output'].includes(item.type))
        const index = outputs.length
        assert.ok(index <= 2, 'the production Core must preserve both original tool outputs')
        const responseId = `resp_jev_chat_${index}`
        const items = index < 2 ? [
          { type: 'message', id: `msg_jev_chat_${index}`, role: 'assistant', phase: 'commentary',
            content: [{ type: 'output_text', text: index === 0
              ? 'I will check the retained repository revision with executable evidence.'
              : 'The first revision check completed. I will check the same immutable revision again.' }] },
          { type: 'custom_tool_call', id: `ct_jev_chat_${index}`, call_id: `call_jev_chat_${index}`, name: 'exec',
            input: 'const command = ALL_TOOLS.find(tool => tool.name.endsWith("exec_command")); if (!command) throw new Error("command tool missing"); text(await tools[command.name]({cmd:"git rev-parse --verify HEAD",yield_time_ms:1000}));' },
        ] : [
          { type: 'message', id: 'msg_jev_chat_final', role: 'assistant', phase: 'final_answer',
            content: [{ type: 'output_text', text: 'Both executable repository revision checks completed.' }] },
        ]
        const frames = [
          { type: 'response.created', response: { id: responseId, model: payload.model } },
          ...items.flatMap(item => [
            { type: 'response.output_item.added', item: { ...item,
              ...(item.type === 'message' ? { content: [] } : { input: '' }) } },
            { type: 'response.output_item.done', item },
          ]),
          { type: 'response.completed', response: { id: responseId, model: payload.model,
            status: 'completed', error: null, end_turn: index === 2,
            usage: { input_tokens: 11, output_tokens: 2, total_tokens: 13 } } },
        ]
        response.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', connection: 'close' })
        response.end(frames.map(frame => `data: ${JSON.stringify(frame)}\n\n`).join(''))
      } catch (error) {
        errors.push(error.message)
        if (!response.headersSent) response.writeHead(500, { 'content-type': 'text/plain', connection: 'close' })
        response.end('owned JEv Chat provider failed\n')
      }
    })
  await new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolve)
  })
  const origin = `https://127.0.0.1:${server.address().port}`
  return { origin, requests, errors, async close() {
    server.closeAllConnections()
    await new Promise(resolve => server.close(resolve))
  } }
}

function retainedTerminal(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const database = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      database.exec('PRAGMA busy_timeout=5000')
      if (!database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='execution_terminal_outcome'").get()) return []
      return database.prepare(`SELECT r.record_json,t.frame_digest,t.frame_json FROM codex_run r
        JOIN execution_terminal_outcome t ON t.run_key=r.run_key`).all().flatMap(row => {
        const run = JSON.parse(Buffer.from(row.record_json).toString('utf8'))
        if (run.job.jobId !== jobId) return []
        const bytes = Buffer.from(row.frame_json)
        const frame = JSON.parse(bytes.toString('utf8'))
        const pending = database.prepare("SELECT acknowledgement_required FROM execution_outbox WHERE family='outcome' AND delivery_id=?")
          .get(frame.messageId)
        const acknowledged = database.prepare("SELECT correlation_key,response_digest FROM execution_response_receipt WHERE family='outcome'")
          .all().map(receipt => ({ ...receipt }))
        return [{ run, bytes, frame, digest: row.frame_digest, pending, acknowledged }]
      })
    } finally { database.close() }
  })
}

function coreFacts(run, deviceData) {
  const events = readFileSync(run.rolloutPath, 'utf8').trim().split('\n').map(line => JSON.parse(line))
    .filter(item => item.type === 'event_msg').map(item => item.payload)
  const tools = globSync('**/state_5.sqlite', { cwd: deviceData }).flatMap(path => {
    const database = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      database.exec('PRAGMA busy_timeout=5000')
      return database.prepare(`SELECT r.sequence,r.request_json,r.resolution,
        a.attempt_id,a.operation_digest,a.execution,a.disposition,a.delivery
        FROM tool_requests r LEFT JOIN tool_attempts a ON a.request_sequence=r.sequence
        WHERE r.thread_id=? ORDER BY r.sequence`).all(run.kernelSessionId)
        .filter(row => JSON.parse(row.request_json).tool_name.endsWith('exec_command'))
    } finally { database.close() }
  })
  return { submissionId: run.submissionId, submissionDigest: run.submissionDigest,
    kernelSessionId: run.kernelSessionId, threadId: run.canonicalThreadId,
    turns: events.filter(event => ['task_started', 'turn_started'].includes(event.type)).map(event => event.turn_id),
    users: events.filter(event => event.type === 'user_message'
      || (event.type === 'item_completed' && event.item?.type === 'UserMessage')).length, tools }
}

function preparedModelRequests(deviceData, jobId) {
  return globSync('**/providers.sqlite3', { cwd: deviceData }).sort().flatMap(path => {
    const database = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      database.exec('PRAGMA busy_timeout=5000')
      return database.prepare(`SELECT exchange_id,request_open,prepared_payload,prepared_payload_digest
        FROM exchanges WHERE request_open IS NOT NULL ORDER BY exchange_id`).all().flatMap(row => {
        if (JSON.parse(row.request_open).lease.jobId !== jobId) return []
        assert.ok(row.prepared_payload !== null, 'JEv must retain each exact prepared request before invoking HTTPS')
        const bytes = Buffer.from(row.prepared_payload)
        assert.equal(row.prepared_payload_digest, digest(bytes))
        const payload = JSON.parse(bytes)
        assert.equal(payload.winwincodeJevContext, undefined)
        assert.equal(payload.winwincodeJevTask, undefined)
        assert.equal(payload.winwincodeJevJudge, undefined)
        return [{ path, exchangeId: row.exchange_id, requestOpen: row.request_open, bytes,
          digest: row.prepared_payload_digest }]
      })
    } finally { database.close() }
  })
}

function acceptedOutcome(database, jobId) {
  const row = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
    .get(`execution-outcome:${jobId}:1`)
  return row ? { revision: row.revision, payload: JSON.parse(Buffer.from(row.payload).toString('utf8')) } : null
}

// Crash point: the Server terminal commit, after paid preparation and Worker
// terminal retention. Preparation-in-flight death is a separate recovery case.
test('JEv-enabled Chat preserves completed paid preparation after Worker terminal retention and a Server crash', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-chat-jev-recovery-'))
  const certificate = createCertificate(directory)
  writeDeviceTrustRoot({ certificatePath: certificate.cert,
    destination: join(directory, 'device-provider-trust-root.der') })
  const provider = await startJevChatProvider(certificate)
  const settingsPath = join(directory, 'jev-settings.toml')
  writeFileSync(settingsPath, `providerId = "jev-chat-acceptance"
endpoint = "${provider.origin}/jev"
apiKey = "owned-jev-chat-secret"
modelId = "owned-jev-chat-model"
timeoutMs = 5000
retries = 0
maxBatchSize = 8
devices = ["remote"]
dtypes = ["auto"]
`, { mode: 0o600 })
  const originalTrustRoot = process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE
  process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE = join(directory, 'device-provider-trust-root.der')
  let complete = false
  t.after(async () => {
    await provider.close()
    if (originalTrustRoot === undefined) delete process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE
    else process.env.WWC_DEVICE_PROVIDER_TLS_ROOT_DER_FILE = originalTrustRoot
    if (complete) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed real JEv Chat recovery retained at ${directory}`)
  })
  const productSessionId = 'psn_01J00000000000000000000081'
  const requestId = 'req_01J00000000000000000000081'
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, includeStrongFlow: false, ...sealedBinaries(),
    deviceProvider: { providerId: 'owned-chat-jev', modelId: 'owned-chat-model',
      displayName: 'Owned real Chat and JEv acceptance Provider',
      endpoint: `${provider.origin}/v1/responses`, protocol: 'openai_responses', responsesStructuredOutput: 'text' },
    deviceProviderSecrets: ['owned-chat-model-secret'],
    deviceAgentEnvironment: { WWC_WORKER_JEV_CONTEXT: JSON.stringify({ provider: 'jev-chat-acceptance', policy: {
      version: 'chat-jev-acceptance-v1', minimumConfidence: 0.6, pinThreshold: 0.8,
      keepThreshold: 0.8, compactThreshold: 0.8, dropThreshold: 0.8,
      taskMemoryThreshold: 0.5, projectMemoryThreshold: 0.7, longTermMemoryThreshold: 0.9,
    } }), WWC_WORKER_JEV_JUDGE: 'jev-chat-acceptance', WWC_DEVICE_JEV_SETTINGS_FILE: settingsPath },
    scenario: { async run({ api, modelRoute, devicePath, restartServer }) {
      await api.command('session.create', 0, { productSessionId,
        projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
        title: 'Real JEv Chat preparation recovery', modelRoute })
      await devicePath.launchAnchor({ productSessionId })
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
      database.exec('PRAGMA busy_timeout=5000')
      try {
        database.exec(`CREATE TRIGGER pause_jev_chat_recovery BEFORE INSERT ON execution_lease_terminals
          WHEN NEW.job_id IN (SELECT job_id FROM scheduler_execution_jobs WHERE product_session_id='${productSessionId}')
          BEGIN SELECT RAISE(ABORT,'injected before CP terminal commit'); END`)
        const payload = { productSessionId, message: 'Check the retained repository revision twice with executable evidence.' }
        const submitted = await api.command('chat.submit', 1, payload, requestId)
        const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE product_session_id=?').all(productSessionId)
        assert.equal(jobs().length, 1)
        const job = jobs()[0]
        const original = await waitFor(() => retainedTerminal(devicePath.deviceData, job.job_id)
          .find(terminal => terminal.run.phase === 'outcome_retained' && terminal.pending?.acknowledgement_required === 1),
        'JEv-enabled real Core retains its original terminal before the Control Plane commit', 120_000)
        assert.equal(original.frame.outcome.status, 'succeeded', 'ordinary Chat must execute with its original nullable WorkInput and formal JEv config')
        assert.equal(Object.hasOwn(original.run.job, 'workInput'), false,
          'this acceptance must retain the real ordinary Chat job scope')
        assert.equal(original.digest, digest(original.bytes))
        assert.equal(acceptedOutcome(database, job.job_id), null)
        const originalCore = coreFacts(original.run, devicePath.deviceData)
        assert.deepEqual(originalCore.turns, [originalCore.submissionId])
        assert.equal(originalCore.users, 1)
        assert.equal(originalCore.tools.length, 2)
        assert.ok(originalCore.tools.every(tool => tool.execution === 'completed' && tool.disposition === 'accepted'))
        const evidence = readDeviceExecutionReceipts(directory)
        const calls = evidence.calls.filter(call => call.jobId === job.job_id)
        const jev = evidence.jev.filter(call => call.jobId === job.job_id)
        assert.equal(calls.length, 3)
        assert.ok(calls.every(call => call.providerAttempts.length === 1 && call.providerAttempts[0].state === 'completed'))
        assert.ok(jev.some(call => call.kind === 'context'), 'older real Core commentary must incur inline context preparation')
        assert.ok(jev.some(call => call.kind === 'judge'), 'new real Core command evidence must incur semantic review')
        assert.ok(jev.every(call => call.completed && call.inputTokens > 0 && call.outputTokens > 0 && call.failureCount === 0))
        const physical = provider.requests.map(({ path, bytes }) => ({ path, bytes: Buffer.from(bytes) }))
        assert.equal(physical.filter(request => request.path === '/v1/responses').length, calls.length)
        assert.equal(physical.filter(request => request.path === '/jev').length, jev.length)
        const prepared = preparedModelRequests(devicePath.deviceData, job.job_id)
        assert.equal(prepared.length, calls.length)
        for (const retained of prepared) {
          const request = JSON.parse(retained.bytes).request
          const outputs = request.input.filter(item => ['custom_tool_call_output', 'function_call_output'].includes(item.type)).length
          const wire = provider.requests.filter(physical => physical.path === '/v1/responses'
            && physical.payload.input.filter(item => ['custom_tool_call_output', 'function_call_output'].includes(item.type)).length === outputs)
          assert.equal(wire.length, 1)
          // The Responses adapter adds only its documented API transport fields
          // and uses the text mode configured through encrypted Device apply.
          const expected = { ...request, stream: true, store: false, max_output_tokens: 32_768 }
          expected.instructions ??= ''
          delete expected.text
          assert.deepEqual(wire[0].payload, expected,
            'the physical HTTPS request must use the exact retained JEv-prepared input, instructions and tools')
        }
        const sourcesBefore = database.prepare('SELECT * FROM execution_admission_settlement_sources WHERE job_id=?').all(job.job_id)
        const restarted = await restartServer({ crash: true })
        assert.equal(restarted.signal, 'SIGKILL')
        assert.notEqual(restarted.previousPid, restarted.restartedPid)
        const [reopened] = retainedTerminal(devicePath.deviceData, job.job_id)
        assert.deepEqual(reopened.bytes, original.bytes)
        assert.deepEqual(coreFacts(reopened.run, devicePath.deviceData), originalCore)
        assert.deepEqual(preparedModelRequests(devicePath.deviceData, job.job_id), prepared)
        assert.deepEqual(provider.requests.map(({ path, bytes }) => ({ path, bytes: Buffer.from(bytes) })), physical)
        assert.deepEqual(database.prepare('SELECT * FROM execution_admission_settlement_sources WHERE job_id=?').all(job.job_id), sourcesBefore)
        database.exec('DROP TRIGGER pause_jev_chat_recovery')
        const settled = await waitFor(async () => {
          const accepted = acceptedOutcome(database, job.job_id)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
          const terminal = retainedTerminal(devicePath.deviceData, job.job_id)[0]
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          return accepted?.revision === 2 && accepted.payload.acknowledgementReady && reservation?.state === 'settled'
            && terminal && !terminal.pending && terminal.acknowledged.length === 1
            && messages.some(message => message.role === 'assistant' && message.state === 'completed')
            ? { accepted, reservation, terminal, messages } : false
        }, 'the original JEv-prepared result receives its durable business ACK after real Server recovery', 120_000)
        assert.deepEqual(settled.accepted.payload.message, original.frame)
        assert.deepEqual(settled.terminal.bytes, original.bytes)
        assert.deepEqual(settled.terminal.acknowledged, [{
          correlation_key: digest(JSON.stringify(canonicalJson([
            original.frame.lease, original.frame.workerSessionId, original.frame.sessionIdentity,
          ]))),
          response_digest: digest(JSON.stringify(canonicalJson({
            kind: 'job.outcome_ack', schemaVersion: 'winwincode/v1', status: 'accepted',
            lease: original.frame.lease, workerSessionId: original.frame.workerSessionId,
            sessionIdentity: original.frame.sessionIdentity,
          }))),
        }], 'business ACK proof must bind the complete original lease and Session identity')
        assert.deepEqual(coreFacts(settled.terminal.run, devicePath.deviceData), originalCore)
        assert.deepEqual(preparedModelRequests(devicePath.deviceData, job.job_id), prepared)
        const after = readDeviceExecutionReceipts(directory)
        assert.deepEqual(after.calls.filter(call => call.jobId === job.job_id), calls)
        assert.deepEqual(after.jev.filter(call => call.jobId === job.job_id), jev)
        const measuredTokens = calls.reduce((sum, call) => sum + call.providerAttempts[0].usage.totalTokens, 0)
          + jev.reduce((sum, call) => sum + call.inputTokens + call.outputTokens, 0)
        assert.equal(settled.reservation.actual_tokens, measuredTokens, 'settlement includes each real primary and JEv receipt once')
        assert.equal(settled.messages.filter(message => message.role === 'user').length, 1)
        assert.equal(settled.messages.filter(message => message.role === 'assistant' && message.state === 'completed').length, 1)
        const sources = database.prepare('SELECT * FROM execution_admission_settlement_sources WHERE job_id=?').all(job.job_id)
        assert.equal(sources.length, 1)
        assert.equal(JSON.parse(sources[0].fact_json).actualTokens, measuredTokens)
        assert.equal(jobs().length, 1)
        assert.equal(jobs()[0].attempt, 1)
        assert.deepEqual((await api.command('chat.submit', 1, payload, requestId)).result, submitted.result)
        assert.deepEqual(provider.requests.map(({ path, bytes }) => ({ path, bytes: Buffer.from(bytes) })), physical,
          'recovery and completed command replay must add no physical model or paid JEv request')
        assert.deepEqual(provider.errors, [])
        return { jobId: job.job_id, coreTurnId: originalCore.submissionId, modelAttempts: calls.length,
          jevContext: jev.filter(call => call.kind === 'context').length,
          jevJudge: jev.filter(call => call.kind === 'judge').length, measuredTokens, businessAcknowledged: true }
      } finally {
        database.exec('DROP TRIGGER IF EXISTS pause_jev_chat_recovery')
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.businessAcknowledged, true)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  complete = true
})
