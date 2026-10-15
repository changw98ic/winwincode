// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { createServer } from 'node:http'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { assertSuccessfulRetainedCommands, readRetainedCoreCommandResults, readWorkerTransport } from './fixtures/chat-execution-transport-probe.mjs'

function receipt(result, input = {}) {
  const bytes = JSON.stringify({ code_mode_result: result,
    effective_input: { kind: 'function', input: JSON.stringify(input) } })
  return { execution: 'completed', disposition: 'accepted',
    execution_result: bytes, accepted_result: bytes }
}

test('command evidence accepts its original direct successful exit', () => {
  assertSuccessfulRetainedCommands([receipt({ exit_code: 0 })], 1)
})

test('command evidence accepts the retained successful poll of its exact running session', () => {
  const command = { ...receipt({ session_id: 42 }), polls: [receipt({ exit_code: 0 }, { session_id: 42 })] }
  assertSuccessfulRetainedCommands([command], 1)
})

test('handler completion with a running command is insufficient OS evidence', () => {
  assert.throws(() => assertSuccessfulRetainedCommands([receipt({ session_id: 42 })], 1),
    /retained completion poll/u)
})

test('a successful poll of a different session cannot prove command completion', () => {
  const command = { ...receipt({ session_id: 42 }), polls: [receipt({ exit_code: 0 }, { session_id: 43 })] }
  assert.throws(() => assertSuccessfulRetainedCommands([command], 1), /exact original command session/u)
})

test('a pending poll cannot prove command completion', () => {
  const command = { ...receipt({ session_id: 42 }), polls: [receipt({ session_id: 42 }, { session_id: 42 })] }
  assert.throws(() => assertSuccessfulRetainedCommands([command], 1), /successful OS command/u)
})

test('both execution and accepted results must prove the same successful exit', () => {
  const poll = receipt({ exit_code: 0 }, { session_id: 42 })
  poll.accepted_result = receipt({ exit_code: 7 }, { session_id: 42 }).accepted_result
  const command = { ...receipt({ session_id: 42 }), polls: [poll] }
  assert.throws(() => assertSuccessfulRetainedCommands([command], 1), /successful OS command/u)
})

test('retained command evidence preserves a rejected error beside a successful continuation', t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-command-evidence-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const store = new DatabaseSync(join(directory, 'state_5.sqlite'))
  const error = 'exec_command failed: INTERACTION_DEADLINE_EXPIRED'
  try {
    store.exec(`CREATE TABLE tool_requests(sequence INTEGER,thread_id TEXT,request_json TEXT,resolution TEXT);
      CREATE TABLE tool_attempts(request_sequence INTEGER,attempt_id TEXT,operation_digest TEXT,
        execution TEXT,disposition TEXT,delivery TEXT,execution_result TEXT,accepted_result TEXT);`)
    for (const [sequence, result] of [[1, { execution: 'uncertain', disposition: 'rejected',
      execution_result: error, accepted_result: error }], [2, receipt({ exit_code: 0 })]]) {
      store.prepare('INSERT INTO tool_requests VALUES(?,?,?,?)').run(sequence, 'thread',
        JSON.stringify({ tool_name: 'exec_command', turn_id: 'turn' }), 'resolved')
      store.prepare('INSERT INTO tool_attempts VALUES(?,?,?,?,?,?,?,?)').run(sequence, `attempt-${sequence}`,
        `digest-${sequence}`, result.execution, result.disposition, 'offered',
        result.execution_result, result.accepted_result)
    }
  } finally { store.close() }
  const rows = readRetainedCoreCommandResults(directory, 'thread')
  assert.equal(rows.length, 2)
  assert.equal(rows[0].execution_result, error)
  assert.deepEqual(rows[0].polls, [])
  assert.throws(() => assertSuccessfulRetainedCommands([rows[0]], 1))
  assertSuccessfulRetainedCommands([rows[1]], 1)
})

test('retained Worker bytes use the Device relay envelope and its inner response status', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-worker-relay-probe-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const fixture = JSON.parse(readFileSync(new URL('./fixtures/contracts/execution-port.valid.json', import.meta.url), 'utf8'))
  const message = fixture.messages.find(value => value.kind === 'runtime.event')
  const bytes = Buffer.from(JSON.stringify(message))
  const workerId = message.lease.workerId
  const workerInstanceId = message.lease.workerInstanceId
  const workerSessionId = 'wss_00000000000000000000000001'
  const clientNodeId = 'cnd_00000000000000000000000001'
  const received = []
  const server = createServer(async (incoming, outgoing) => {
    const parts = []
    for await (const part of incoming) parts.push(part)
    received.push({ path: incoming.url, headers: incoming.headers,
      body: JSON.parse(Buffer.concat(parts).toString('utf8')) })
    outgoing.writeHead(200, { 'Content-Type': 'application/json' })
    outgoing.end(JSON.stringify(received.length === 1
      ? { status: 200, body: JSON.stringify({ schemaVersion: 'execution-port.remote-exchange.v2',
        frameAccepted: true, deliveries: [] }) }
      : { status: 400, body: 'Worker route does not match its Device session' }))
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  t.after(() => new Promise(resolve => server.close(resolve)))
  const dataDirectory = join(directory, 'worker')
  mkdirSync(dataDirectory)
  writeFileSync(join(dataDirectory, 'managed-session.json'), JSON.stringify({
    clientNodeId, workerSessionId, workerId, workerInstanceId,
  }))
  const store = new DatabaseSync(join(directory, 'device-client.sqlite3'))
  try {
    store.exec(`CREATE TABLE worker_process_registry(worker_session_id TEXT,worker_id TEXT,
      worker_instance_id TEXT,pid INTEGER,data_directory TEXT);
      CREATE TABLE server_profile(base_url TEXT);`)
    store.prepare('INSERT INTO worker_process_registry VALUES(?,?,?,?,?)')
      .run(workerSessionId, workerId, workerInstanceId, process.pid, dataDirectory)
    store.prepare('INSERT INTO server_profile VALUES(?)')
      .run(`http://127.0.0.1:${server.address().port}/internal/v1/client/exchange`)
  } finally { store.close() }
  const probe = readWorkerTransport(directory, { worker_id: workerId,
    worker_instance_id: workerInstanceId, worker_session_id: message.workerSessionId })
  const accepted = await probe.postRetainedMessage(bytes)
  assert.equal(accepted.status, 200)
  assert.equal(accepted.frameAccepted, true)
  const rejected = await probe.postRetainedMessage(bytes)
  assert.equal(rejected.status, 400)
  assert.equal(rejected.rejectionBoundary, 'device-worker-route')
  const lost = await probe.postRetainedMessage(bytes, { loseResponseAfterHeaders: true })
  assert.equal(lost.httpStatus, 200)
  assert.equal(lost.status, null)
  assert.equal(lost.responseLostAfterHeaders, true)
  for (const request of received) {
    assert.equal(request.path, '/internal/v1/client/worker-exchange')
    assert.equal(request.headers.authorization, undefined)
    assert.equal(request.body.clientNodeId, clientNodeId)
    assert.equal(request.body.workerId, workerId)
    assert.equal(request.body.workerInstanceId, workerInstanceId)
    assert.equal(request.body.request.kind, 'exchange')
    const exchange = JSON.parse(request.body.request.body)
    assert.equal(exchange.workerId, workerId)
    assert.equal(exchange.workerInstanceId, workerInstanceId)
    const frame = Buffer.from(exchange.frame)
    const prefix = Buffer.from('{"frameType":"message","direction":"worker-to-control-plane","message":')
    assert.deepEqual(frame.subarray(prefix.length, -1), bytes)
  }
})
