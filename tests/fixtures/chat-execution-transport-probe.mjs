// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { globSync, readFileSync, realpathSync } from 'node:fs'
import { request } from 'node:http'
import { request as httpsRequest } from 'node:https'
import { join, relative } from 'node:path'
import { DatabaseSync } from 'node:sqlite'

export async function loseCommittedCommandResponse(api, command, expectedRevision, payload, requestId) {
  const endpoint = new URL('/api/v1/commands', api.baseUrl)
  const send = endpoint.protocol === 'https:' ? httpsRequest : request
  const serialized = JSON.stringify({
    schemaVersion: 'winwincode/v1', requestId, actor: api.actor, scope: api.scope,
    command, expectedRevision, payload,
  })
  return new Promise((resolveResponse, rejectResponse) => {
    let intentionallyClosed = false
    const request = send(endpoint, {
      method: 'POST', servername: 'control.localhost', timeout: 30_000,
      rejectUnauthorized: api.ca !== undefined,
      ...(api.ca === undefined ? {} : { ca: api.ca }),
      headers: {
        Origin: api.origin, Accept: 'application/json', Connection: 'close',
        'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(serialized),
        ...(api.cookie === null ? {} : { Cookie: api.cookie }),
      },
    }, response => {
      const status = response.statusCode
      const bodyComplete = response.complete
      intentionallyClosed = true
      response.destroy()
      resolveResponse({ status, bodyComplete, destroyed: response.destroyed })
    })
    request.on('error', error => { if (!intentionallyClosed) rejectResponse(error) })
    request.on('timeout', () => request.destroy(new Error('lost response fixture timed out')))
    request.end(serialized)
  })
}

export function signalIfPresent(pid, signal) {
  try {
    process.kill(pid, signal)
    return true
  } catch (error) {
    if (error.code === 'ESRCH') return false
    throw error
  }
}

function inside(root, path) {
  const resolved = realpathSync(path)
  const suffix = relative(realpathSync(root), resolved)
  assert.ok(suffix !== '' && !suffix.startsWith('..') && !suffix.startsWith('/'))
  return resolved
}

function acknowledgementId(message) {
  if (message.kind === 'worker.heartbeat') {
    const digest = createHash('sha256').update(JSON.stringify([
      'worker-heartbeat-receipt-v1', message.messageId, message.workerId,
      message.workerInstanceId, message.heartbeatSequence, 'accepted',
    ])).digest()
    const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'
    return `xmsg_${Array.from({ length: 26 }, (_, index) =>
      alphabet[(digest[index % digest.length] >> ((index % 5) * 2)) & 31]).join('')}`
  }
  const namespace = message.kind === 'runtime.event'
    ? 'winwincode.runtime-ack-message.v1' : 'winwincode.execution-port.outcome-ack.v1'
  const bytes = createHash('sha256').update(namespace).update(Buffer.from([0]))
    .update(message.messageId).digest()
  const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'
  if (message.kind === 'job.outcome') return `xmsg_${[...bytes.subarray(0, 26)]
    .map(byte => alphabet[byte & 31]).join('')}`
  let value = BigInt(`0x${bytes.subarray(0, 16).toString('hex')}`)
  const suffix = Array(26).fill('0')
  for (let index = 25; index >= 0; index--) {
    suffix[index] = alphabet[Number(value & 31n)]
    value >>= 5n
  }
  return `xmsg_${suffix.join('')}`
}

function sortedJson(value) {
  if (Array.isArray(value)) return value.map(sortedJson)
  if (value && typeof value === 'object') return Object.fromEntries(Object.keys(value).sort()
    .map(key => [key, sortedJson(value[key])]))
  return value
}

function expectedOutcomeResponseDigest(message) {
  const accepted = { kind: 'job.outcome_ack', lease: message.lease,
    schemaVersion: 'winwincode/v1', sessionIdentity: message.sessionIdentity,
    status: 'accepted', workerSessionId: message.workerSessionId }
  return `sha256:${createHash('sha256').update(JSON.stringify(sortedJson(accepted))).digest('hex')}`
}

export function readWorkerTransport(deviceData, authority, { launch = null } = {}) {
  assert.ok((launch === null && authority && typeof authority === 'object')
    || (launch !== null && authority === null), 'select the original CP authority or one actual pre-dispatch Device launch')
  const workerSessionId = launch === null ? authority.worker_session_id : null
  if (launch === null) assert.match(workerSessionId, /^wsn_[0-9A-HJKMNP-TV-Z]{26}$/u)
  const workerId = launch?.workerId ?? authority.worker_id
  const workerInstanceId = launch?.workerInstanceId ?? authority.worker_instance_id
  const store = new DatabaseSync(join(deviceData, 'device-client.sqlite3'), { readOnly: true })
  let row
  let profile
  try {
    store.exec('PRAGMA busy_timeout=5000')
    const rows = store.prepare('SELECT * FROM worker_process_registry WHERE worker_id=? AND worker_instance_id=?')
      .all(workerId, workerInstanceId)
    assert.equal(rows.length, 1, 'one actual Device-supervised process owns the exact dispatch Worker instance')
    row = rows[0]
    const profiles = store.prepare('SELECT base_url FROM server_profile').all()
    assert.equal(profiles.length, 1, 'the production fixture has one actual Device server profile')
    profile = profiles[0]
  } finally { store.close() }
  assert.ok(row, 'the actual Device-supervised Worker process is registered')
  if (launch !== null) assert.equal(row.worker_session_id, launch.workerSessionId,
    'pre-dispatch transport belongs to the exact actual Device launch WorkerSession')
  const dataDirectory = inside(deviceData, row.data_directory)
  const configurationPath = inside(deviceData, join(dataDirectory, 'managed-session.json'))
  const configuration = JSON.parse(readFileSync(configurationPath, 'utf8'))
  assert.equal(configuration.workerSessionId, row.worker_session_id)
  assert.equal(configuration.workerId, row.worker_id)
  assert.equal(configuration.workerInstanceId, row.worker_instance_id)
  const endpoint = new URL('/internal/v1/client/worker-exchange', profile.base_url)
  assert.equal(endpoint.protocol, 'http:')
  assert.ok(['127.0.0.1', 'localhost', 'control.localhost'].includes(endpoint.hostname))
  const replayAcknowledgements = new Set()
  return {
    workerId: row.worker_id, workerInstanceId: row.worker_instance_id,
    workerSessionId, launchWorkerSessionId: row.worker_session_id, pid: row.pid, dataDirectory,
    async postRetainedMessage(bytes, { timeoutMillis = 10_000, loseResponseAfterHeaders = false } = {}) {
      assert.equal(typeof loseResponseAfterHeaders, 'boolean')
      const message = JSON.parse(Buffer.from(bytes).toString('utf8'))
      assert.ok(['runtime.event', 'job.outcome', 'worker.heartbeat'].includes(message.kind))
      if (message.kind === 'worker.heartbeat') {
        assert.equal(message.workerId, row.worker_id)
        assert.equal(message.workerInstanceId, row.worker_instance_id)
        const retained = readRetainedWorkerHeartbeats(dataDirectory, row.worker_id, row.worker_instance_id)
          .filter(value => value.message.messageId === message.messageId)
        assert.equal(retained.length, 1, 'heartbeat forwarding requires one actual pending Adapter outbox row')
        assert.deepEqual(Buffer.from(bytes), retained[0].bytes, 'forward the immutable original heartbeat bytes')
      } else {
        assert.ok(workerSessionId, 'execution messages require the exact original CP dispatch authority')
        assert.equal(message.lease.workerId, row.worker_id)
        assert.equal(message.lease.workerInstanceId, row.worker_instance_id)
        assert.equal(message.workerSessionId, workerSessionId)
      }
      const frame = Buffer.concat([
        Buffer.from('{"frameType":"message","direction":"worker-to-control-plane","message":'),
        Buffer.from(bytes), Buffer.from('}'),
      ])
      assert.ok(frame.length > 0 && frame.length <= 256 * 1024,
        'the replay uses the production canonical frame bound')
      const exchange = JSON.stringify({
        schemaVersion: 'execution-port.remote-exchange.v2',
        workerId: row.worker_id, workerInstanceId: row.worker_instance_id,
        acknowledgements: [...replayAcknowledgements], frame: [...frame],
      })
      const body = Buffer.from(JSON.stringify({
        clientNodeId: configuration.clientNodeId,
        workerId: row.worker_id, workerInstanceId: row.worker_instance_id,
        request: { kind: 'exchange', body: exchange },
      }))
      return new Promise((resolveResponse, rejectResponse) => {
        const outgoing = request(endpoint, {
          method: 'POST',
          timeout: timeoutMillis,
          headers: { 'Content-Type': 'application/json',
            'Content-Length': body.length, Connection: 'close' },
        }, incoming => {
          if (loseResponseAfterHeaders) {
            try {
              assert.equal(incoming.statusCode, 200, 'the Device relay HTTP response is deliberately lost')
              resolveResponse({ httpStatus: incoming.statusCode, status: null, frameAccepted: null,
                acknowledgement: null, responseLostAfterHeaders: true })
            } catch { rejectResponse(new Error('the lost-response exchange was not successful')) }
            incoming.destroy()
            outgoing.destroy()
            return
          }
          const parts = []
          let length = 0
          incoming.on('data', part => {
            length += part.length
            if (length > 2 * 1024 * 1024) incoming.destroy(new Error('bounded transport response exceeded'))
            else parts.push(part)
          })
          incoming.on('error', () => rejectResponse(new Error('retained-frame response failed')))
          incoming.on('end', () => {
            try {
              const responseBytes = Buffer.concat(parts)
              assert.equal(incoming.statusCode, 200, 'the Device relay returns its Worker response envelope')
              const relay = JSON.parse(responseBytes.toString('utf8'))
              assert.ok(Number.isInteger(relay.status))
              assert.equal(typeof relay.body, 'string')
              const status = relay.status
              const response = status === 200 ? JSON.parse(relay.body) : null
              if (status === 200) {
                assert.equal(response.schemaVersion, 'execution-port.remote-exchange.v2')
                assert.equal(typeof response.frameAccepted, 'boolean')
                assert.ok(Array.isArray(response.deliveries) && response.deliveries.length <= 128)
              }
              const deliveries = response?.deliveries.map(delivery => {
                assert.ok(Array.isArray(delivery.frame) && delivery.frame.length <= 256 * 1024)
                const envelope = JSON.parse(Buffer.from(delivery.frame).toString('utf8'))
                assert.equal(envelope.frameType, 'message')
                assert.equal(envelope.direction, 'control-plane-to-worker')
                assert.equal(envelope.message.messageId, delivery.deliveryId)
                return { deliveryId: delivery.deliveryId, message: envelope.message }
              }) ?? []
              const received = deliveries.find(delivery =>
                delivery.message.messageId === acknowledgementId(message)
                && delivery.message.kind === (message.kind === 'runtime.event' ? 'runtime.ack'
                  : message.kind === 'worker.heartbeat' ? 'worker.heartbeat_ack' : 'job.outcome_ack'))
              const acknowledgement = received?.message
              if (acknowledgement) {
                if (message.kind === 'worker.heartbeat') {
                  assert.equal(acknowledgement.schemaVersion, 'winwincode/v1')
                  assert.equal(acknowledgement.workerId, message.workerId)
                  assert.equal(acknowledgement.workerInstanceId, message.workerInstanceId)
                  assert.equal(acknowledgement.heartbeatSequence, message.heartbeatSequence)
                  assert.ok(['accepted', 'duplicate'].includes(acknowledgement.status))
                  assert.equal(acknowledgement.error ?? null, null)
                } else {
                  assert.deepEqual(acknowledgement.lease, message.lease)
                  assert.equal(acknowledgement.workerSessionId, message.workerSessionId)
                  assert.deepEqual(acknowledgement.sessionIdentity, message.sessionIdentity)
                }
              }
              if (status === 200) {
                replayAcknowledgements.clear()
                if (received) replayAcknowledgements.add(received.deliveryId)
              }
              resolveResponse({ status, frameAccepted: response?.frameAccepted ?? null,
                acknowledgement: acknowledgement ?? null,
                deliveries,
                rejectionBoundary: status === 400 && relay.body === 'Worker route does not match its Device session' ? 'device-worker-route'
                  : status === 400 && relay.body === 'remote Worker frame rejected' ? 'control-plane-worker-instance'
                  : ['rejected_worker_instance', 'rejected_stale_fencing_token'].includes(acknowledgement?.status)
                    ? 'control-plane-authority'
                    : acknowledgement?.status.startsWith('rejected_') ? 'control-plane-message-rejection' : null })
            } catch { rejectResponse(new Error('retained-frame response violated the canonical envelope')) }
          })
        })
        outgoing.on('timeout', () => outgoing.destroy(new Error('retained-frame exchange timed out')))
        outgoing.on('error', () => rejectResponse(new Error('retained-frame Device relay failed')))
        outgoing.end(body)
      })
    },
  }
}

export function readRetainedWorkerHeartbeats(dataDirectory, workerId, workerInstanceId) {
  return globSync('**/worker-codex.sqlite3', { cwd: dataDirectory }).flatMap(path => {
    const store = new DatabaseSync(join(dataDirectory, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='execution_outbox'").get()) return []
      return store.prepare(`SELECT delivery_id,correlation_key,state,acknowledgement_required,frame_json,frame_digest
        FROM execution_outbox WHERE family='worker_heartbeat' ORDER BY position`).all().flatMap(row => {
        const bytes = Buffer.from(row.frame_json)
        const message = JSON.parse(bytes.toString('utf8'))
        if (message.workerId !== workerId || message.workerInstanceId !== workerInstanceId) return []
        assert.equal(message.schemaVersion, 'winwincode/v1')
        assert.equal(message.kind, 'worker.heartbeat')
        assert.equal(row.delivery_id, message.messageId)
        assert.ok(Number.isSafeInteger(message.heartbeatSequence) && message.heartbeatSequence > 0)
        assert.equal(row.frame_digest, `sha256:${createHash('sha256').update(bytes).digest('hex')}`)
        assert.equal(row.correlation_key, `sha256:${createHash('sha256').update(JSON.stringify([
          message.heartbeatSequence, message.workerId, message.workerInstanceId,
        ])).digest('hex')}`)
        return [{ bytes, message, delivery: { deliveryId: row.delivery_id, state: row.state,
          correlationKey: row.correlation_key, acknowledgementRequired: row.acknowledgement_required } }]
      })
    } finally { store.close() }
  })
}

export function readRetainedRuntime(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_replay_frame'").get()) return []
      return store.prepare(`SELECT f.sequence,f.event_id,f.digest,f.frame,h.ack_sequence,h.highest_sequence
        FROM runtime_replay_frame f JOIN runtime_replay_head h ON h.stream_key=f.stream_key
        ORDER BY f.sequence`).all().flatMap(row => {
        const bytes = Buffer.from(row.frame)
        const message = JSON.parse(bytes.toString('utf8'))
        if (message.lease.jobId !== jobId) return []
        assert.equal(message.kind, 'runtime.event')
        assert.equal(message.schemaVersion, 'winwincode/v1')
        assert.equal(message.event.eventId, row.event_id)
        assert.equal(message.event.sequence, row.sequence)
        assert.equal(row.digest, `sha256:${createHash('sha256')
          .update(JSON.stringify(message.event)).digest('hex')}`,
        'the real retained event bytes match their immutable digest')
        return [{ ...row, bytes, message }]
      })
    } finally { store.close() }
  })
}

export function readRetainedCoreCommandResults(deviceData, kernelSessionId) {
  return globSync('**/state_5.sqlite', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      const rows = store.prepare(`SELECT r.sequence,r.request_json,r.resolution,
        a.attempt_id,a.operation_digest,a.execution,a.disposition,a.delivery,a.execution_result,a.accepted_result
        FROM tool_requests r LEFT JOIN tool_attempts a ON a.request_sequence=r.sequence
        WHERE r.thread_id=? ORDER BY r.sequence`).all(kernelSessionId)
      return rows.filter(row => JSON.parse(row.request_json).tool_name.endsWith('exec_command'))
        .map(row => {
          const session = row.execution === 'completed' && row.disposition === 'accepted' && row.execution_result
            ? JSON.parse(row.execution_result).code_mode_result?.session_id : undefined
          const turn = JSON.parse(row.request_json).turn_id
          const polls = session === undefined ? [] : rows.filter(candidate => {
            const request = JSON.parse(candidate.request_json)
            if (candidate.sequence <= row.sequence || request.turn_id !== turn
              || !request.tool_name.endsWith('write_stdin') || !candidate.execution_result) return false
            const input = JSON.parse(candidate.execution_result).effective_input?.input
            return input !== undefined && JSON.parse(input).session_id === session
          })
          return { ...row, polls }
        })
    } finally { store.close() }
  })
}

export function assertSuccessfulRetainedCommands(commands, expectedCount) {
  assert.equal(commands.length, expectedCount, 'the original Core has the expected exact command request count')
  for (const row of commands) {
    const receipts = [row, ...(row.polls ?? [])]
    for (const receipt of receipts) {
      assert.equal(receipt.execution, 'completed')
      assert.equal(receipt.disposition, 'accepted')
    }
    for (const field of ['execution_result', 'accepted_result']) {
      assert.ok(row[field], `the exact command has its real retained ${field}`)
      const initial = JSON.parse(row[field]).code_mode_result
      if (initial?.exit_code === undefined) {
        assert.ok(Number.isInteger(initial?.session_id), 'a running command has its original session identity')
        assert.ok(row.polls?.length > 0, 'the running command has a retained completion poll')
        for (const poll of row.polls) {
          assert.equal(JSON.parse(JSON.parse(poll[field]).effective_input.input).session_id, initial.session_id,
            'the completion poll belongs to the exact original command session')
        }
      }
      assert.ok(receipts.at(-1)[field], `the exact command has its real retained ${field}`)
      assert.equal(JSON.parse(receipts.at(-1)[field]).code_mode_result?.exit_code, 0,
        `the real retained ${field} proves successful OS command completion`)
    }
  }
}

export function completedCommandScript(command, count = 1, options = {}) {
  return `const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command"));
const input=ALL_TOOLS.find(tool=>tool.name.endsWith("write_stdin"));
if(!command || !input) throw new Error("command tools missing");
for(let index=0;index<${count};index++) {
  let result=await tools[command.name](${JSON.stringify({ ...options, cmd: command, yield_time_ms: 1000 })});
  while(result.exit_code===undefined && Number.isInteger(result.session_id)) {
    result=await tools[input.name]({session_id:result.session_id,chars:"",yield_time_ms:1000});
  }
  if(result.exit_code!==0) throw new Error("original command did not exit successfully");
  text(result);
}`
}

export function readRetainedOutcomes(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='execution_terminal_outcome'").get()) return []
      return store.prepare('SELECT frame_json,frame_digest FROM execution_terminal_outcome').all().flatMap(row => {
        const bytes = Buffer.from(row.frame_json)
        const message = JSON.parse(bytes.toString('utf8'))
        if (message.lease.jobId !== jobId) return []
        assert.equal(message.kind, 'job.outcome')
        assert.equal(message.schemaVersion, 'winwincode/v1')
        assert.equal(row.frame_digest, `sha256:${createHash('sha256').update(bytes).digest('hex')}`)
        const correlation = `sha256:${createHash('sha256').update(JSON.stringify([
          message.lease, message.workerSessionId, message.sessionIdentity,
        ])).digest('hex')}`
        const delivery = store.prepare(`SELECT delivery_id,correlation_key,state,acknowledgement_required
          FROM execution_outbox WHERE family='outcome' AND delivery_id=?`).get(message.messageId)
        if (delivery) assert.equal(delivery.correlation_key, correlation)
        const receipt = store.prepare(`SELECT response_digest FROM execution_response_receipt
          WHERE family='outcome' AND correlation_key=?`).get(correlation)
        if (receipt) assert.equal(receipt.response_digest, expectedOutcomeResponseDigest(message),
          'the Worker proof contains the exact normalized original terminal ACK')
        return [{ bytes, message, digest: row.frame_digest, delivery, receipt,
          workerAcknowledged: !delivery && Boolean(receipt) }]
      })
    } finally { store.close() }
  })
}

export function runtimeLedgerSnapshot(database, productSessionId) {
  const row = database.prepare('SELECT revision,payload FROM product_state WHERE stream_id=?')
    .get(`product-session:${productSessionId}`)
  return row ? { revision: row.revision, bytes: Buffer.from(row.payload) } : null
}
