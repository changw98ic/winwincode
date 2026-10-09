// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtempSync, mkdirSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { exportDeviceExecutionReceipts, readDeviceExecutionReceipts } from '../scripts/acceptance/export-device-candidate.mjs'

const privateText = 'SYNTHETIC_PRIVATE_PAYLOAD'
const sha = text => createHash('sha256').update(text).digest('hex')
const logId = `sse-${'a'.repeat(64)}.log`

function fixture(t, { diagnostics = true } = {}) {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-device-diagnostic-export-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  mkdirSync(join(directory, 'device-data/providers'), { recursive: true })
  const db = new DatabaseSync(join(directory, 'device-data/providers/providers.sqlite3'))
  t.after(() => db.close())
  db.exec(`CREATE TABLE exchanges(exchange_id TEXT,request_open TEXT,chunks TEXT);
    CREATE TABLE jev_context_exchanges(operation_id TEXT,request_json TEXT,result TEXT);
    CREATE TABLE jev_judge_exchanges(operation_id TEXT,request_json TEXT,result TEXT);`)
  if (diagnostics) db.exec(`CREATE TABLE model_attempt_diagnostics(exchange_id TEXT,sequence INTEGER,
    policy_attempt INTEGER,started_ms INTEGER,finished_ms INTEGER,outcome TEXT,failure_json TEXT,stop_reason TEXT);
    CREATE TABLE jev_attempt_diagnostics(operation_id TEXT,sequence INTEGER,role TEXT,recorded_ms INTEGER,failure_json TEXT);`)
  const bytes = Buffer.from(JSON.stringify({ provider: 'fixture-provider',
    request: { model: 'fixture-model', input: privateText, apiKey: 'SYNTHETIC_PRIVATE_CREDENTIAL' } }))
  const opened = { modelExchangeId: 'fixture-exchange', lease: { jobId: 'fixture-job' },
    workerSessionId: 'fixture-session', request: { dataBase64: bytes.toString('base64'), payloadDigest: `sha256:${sha(bytes)}` } }
  const chunks = [{ modelExchangeId: 'fixture-exchange', isFinal: true,
    error: { code: 'DEVICE_PROVIDER_SSE_EVENT_INVALID', retryable: false,
      message: privateText, url: 'https://private.invalid/?token=SYNTHETIC_PRIVATE_CREDENTIAL' } }]
  db.prepare('INSERT INTO exchanges VALUES (?,?,?)').run('fixture-exchange', JSON.stringify(opened), JSON.stringify(chunks))
  return { directory, db }
}

function network(diagnostic = { code: 'http_status' }) {
  return { kind: 'server_transient', acceptance: 'response_received', phase: 'response_headers',
    httpStatus: 503, retryAfterMs: 2000, diagnostic,
    message: privateText, url: 'https://private.invalid/?token=SYNTHETIC_PRIVATE_CREDENTIAL' }
}

function jevFailure(overrides = {}) {
  return { providerId: 'fixture-provider', kind: 'invalidResponse',
    latency: { secs: 0, nanos: 10 }, attempt: 1, connectionWait: false,
    network: network({ code: 'json_schema', field: 'answers', line: 2, column: 19,
      message: privateText, prompt: privateText }), rawResponse: privateText, ...overrides }
}

test('SQLite export preserves four model attempts and JEV diagnostics using safe references', t => {
  const { directory, db } = fixture(t)
  const insert = db.prepare('INSERT INTO model_attempt_diagnostics VALUES (?,?,?,?,?,?,?,?)')
  for (let attempt = 1; attempt <= 4; attempt += 1) insert.run('fixture-exchange', attempt, attempt,
    attempt * 100, attempt * 100 + 50, 'failed', JSON.stringify(network(attempt === 2
      ? { code: 'sse_event', responseLog: logId, responseLogStatus: 'retained', rawResponse: privateText }
      : { code: 'http_status', message: privateText })), attempt === 4 ? 'retry_budget_exhausted' : null)
  const failure = jevFailure()
  db.prepare('INSERT INTO jev_context_exchanges VALUES (?,?,?)').run('jev:fixture-exchange:0', privateText,
    JSON.stringify({ value: null, observation: null, failures: [failure] }))
  db.prepare('INSERT INTO jev_attempt_diagnostics VALUES (?,?,?,?,?)').run('jev:fixture-exchange:0', 1,
    'context', 1000, JSON.stringify(failure))
  const { evidence } = exportDeviceExecutionReceipts(directory)
  const call = evidence.calls[0]
  assert.equal(call.failure.code, 'DEVICE_PROVIDER_SSE_EVENT_INVALID')
  assert.equal(call.failure.stopReason, 'retry_budget_exhausted')
  assert.deepEqual(call.attempts.map(attempt => attempt.sequence), [1, 2, 3, 4])
  assert.deepEqual(call.attempts.map(attempt => attempt.policyAttempt), [1, 2, 3, 4])
  for (const attempt of call.attempts) {
    assert.equal(attempt.diagnosticRef, `model-attempt:${sha('fixture-exchange')}:${attempt.sequence}`)
    assert.equal(attempt.network.httpStatus, 503)
    assert.equal(attempt.network.retryAfterMs, 2000)
  }
  assert.equal(call.attempts[1].network.diagnostic.responseLog, logId)
  assert.equal(call.attempts[1].network.diagnostic.responseLogStatus, 'retained')
  assert.deepEqual(evidence.jev[0].failures[0].network.diagnostic,
    { code: 'json_schema', field: 'answers', line: 2, column: 19 })
  assert.equal(evidence.jev[0].failures[0].diagnosticRef, `jev-attempt:${sha('jev:fixture-exchange:0')}:1`)
  const retained = readFileSync(join(directory, 'execution-receipts.json'), 'utf8')
  assert.deepEqual(JSON.parse(retained), evidence)
  assert.equal(retained.includes('SYNTHETIC_PRIVATE'), false)
  assert.equal(retained.includes('private.invalid'), false)
})

test('an unfinished JEV operation exports its durable connection wait facts', t => {
  const { directory, db } = fixture(t)
  db.prepare('INSERT INTO jev_judge_exchanges VALUES (?,?,NULL)').run('jev:fixture-exchange:judge', privateText)
  const failure = jevFailure({ kind: 'unavailable', connectionWait: true,
    network: { kind: 'connection_unavailable', acceptance: 'not_sent', phase: 'connect', httpStatus: null,
      retryAfterMs: null, diagnostic: { code: 'io', ioKind: 'connection_refused', osCode: 61,
        responseLog: '../../private.log', responseLogStatus: 'SYNTHETIC_PRIVATE_ERROR', text: privateText } } })
  db.prepare('INSERT INTO jev_attempt_diagnostics VALUES (?,?,?,?,?)').run('jev:fixture-exchange:judge', 1,
    'judge', 1000, JSON.stringify(failure))
  const evidence = readDeviceExecutionReceipts(directory)
  const jev = evidence.jev[0]
  assert.equal(jev.completed, false)
  assert.equal(jev.failureCount, 1)
  assert.equal(jev.failures[0].kind, 'unavailable')
  assert.equal(jev.failures[0].attempt, 1)
  assert.equal(jev.failures[0].connectionWait, true)
  assert.deepEqual(jev.failures[0].network.diagnostic, { code: 'io', ioKind: 'connection_refused', osCode: 61 })
  assert.equal(JSON.stringify(evidence).includes('SYNTHETIC_PRIVATE'), false)
  assert.equal(JSON.stringify(evidence).includes('../../private.log'), false)
})

test('legacy SQLite receipts retain the terminal code without inventing attempt history', t => {
  const { directory, db } = fixture(t, { diagnostics: false })
  db.prepare('INSERT INTO jev_context_exchanges VALUES (?,?,?)').run('jev:fixture-exchange:0', privateText,
    JSON.stringify({ value: null, observation: null, failures: [{ providerId: 'fixture', kind: 'unavailable',
      latency: { secs: 0, nanos: 0 } }] }))
  const evidence = readDeviceExecutionReceipts(directory)
  assert.equal(evidence.calls[0].failure.code, 'DEVICE_PROVIDER_SSE_EVENT_INVALID')
  assert.equal(evidence.calls[0].attempts, null)
  assert.equal(evidence.jev[0].failureCount, 1)
  assert.equal(evidence.jev[0].failures[0].network, null)
  assert.equal(evidence.jev[0].failures[0].attempt, null)
  assert.equal(JSON.stringify(evidence).includes('SYNTHETIC_PRIVATE'), false)
})

test('a recovered model call retains retry history without declaring a final failure', t => {
  const { directory, db } = fixture(t)
  const completed = Buffer.from(JSON.stringify({ type: 'completed',
    tokenUsage: { input_tokens: 1, output_tokens: 2, total_tokens: 3 } }))
  const chunks = [{ modelExchangeId: 'fixture-exchange', isFinal: true,
    payload: { dataBase64: completed.toString('base64'), payloadDigest: `sha256:${sha(completed)}` } }]
  db.prepare('UPDATE exchanges SET chunks=?').run(JSON.stringify(chunks))
  const insert = db.prepare('INSERT INTO model_attempt_diagnostics VALUES (?,?,?,?,?,?,?,?)')
  insert.run('fixture-exchange', 1, 1, 100, 150, 'failed', JSON.stringify(network()), null)
  insert.run('fixture-exchange', 2, 2, 200, 250, 'accepted', null, 'response_completed')
  const call = readDeviceExecutionReceipts(directory).calls[0]
  assert.equal(call.terminalType, 'completed')
  assert.equal(call.failure, undefined)
  assert.equal(call.attempts.length, 2)
  assert.equal(call.attempts[0].network.httpStatus, 503)
  assert.equal(call.attempts[1].network, null)
  assert.equal(call.attempts[1].stopReason, 'response_completed')
})
