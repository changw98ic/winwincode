// SPDX-License-Identifier: Apache-2.0

// Production acceptance helper. Reads a real Device launch and retained Worker
// messages. It never registers a Worker or writes state. It acknowledges only
// its own received replay ACK deliveries, never unrelated Worker controls.
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { globSync, readFileSync, realpathSync, statSync } from 'node:fs'
import { request } from 'node:https'
import { dirname, join, relative } from 'node:path'
import { DatabaseSync } from 'node:sqlite'

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
    // RemoteDeliveryQueue separates positive and retryable-gap receipts.
    // Match its accepted tuple and runtime::crockford_26 at the wire boundary.
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
  // ExecutionOutbox::response_replay_digest removes messageId/sentAt and
  // normalizes duplicate to accepted. serde_json::Value serializes sorted keys.
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
  try {
    store.exec('PRAGMA busy_timeout=5000')
    const rows = store.prepare('SELECT * FROM worker_process_registry WHERE worker_id=? AND worker_instance_id=?')
      .all(workerId, workerInstanceId)
    assert.equal(rows.length, 1, 'one actual Device-supervised process owns the exact dispatch Worker instance')
    row = rows[0]
  } finally { store.close() }
  assert.ok(row, 'the actual Device-supervised Worker process is registered')
  if (launch !== null) assert.equal(row.worker_session_id, launch.workerSessionId,
    'pre-dispatch transport belongs to the exact actual Device launch WorkerSession')
  const dataDirectory = inside(deviceData, row.data_directory)
  const configurationPath = inside(deviceData, join(dataDirectory, 'managed-session.json'))
  assert.equal(statSync(configurationPath).mode & 0o777, 0o600)
  const configuration = JSON.parse(readFileSync(configurationPath, 'utf8'))
  // A managed launch has its own WorkerSession. Each accepted Job receives
  // the execution WorkerSession frozen in its CP dispatch authority.
  assert.equal(configuration.workerSessionId, row.worker_session_id)
  assert.equal(configuration.workerId, row.worker_id)
  assert.equal(configuration.workerInstanceId, row.worker_instance_id)
  const credentialPath = inside(deviceData, configuration.workerCredentialPath)
  assert.equal(statSync(credentialPath).mode & 0o777, 0o600)
  const credential = readFileSync(credentialPath, 'utf8').trim()
  assert.ok(credential.length >= 16 && credential.length <= 512)
  const networkJournalPath = inside(deviceData, join(dirname(credentialPath), 'network-requests.sqlite3'))
  const endpoint = new URL('/internal/v1/execution-port/exchange', configuration.serverOrigin)
  assert.equal(endpoint.protocol, 'https:')
  assert.ok(['127.0.0.1', 'localhost', 'control.localhost'].includes(endpoint.hostname),
    'the private credential can only be sent to this local production fixture')
  const replayAcknowledgements = new Set()
  // Keep secret material in a closure. Evidence output contains only identities.
  return {
    workerId: row.worker_id, workerInstanceId: row.worker_instance_id,
    workerSessionId, launchWorkerSessionId: row.worker_session_id, pid: row.pid, dataDirectory, networkJournalPath,
    async postRetainedMessage(bytes, { ca, timeoutMillis = 10_000, loseResponseAfterHeaders = false } = {}) {
      assert.ok(ca, 'the real fixture TLS root is required')
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
      const body = Buffer.from(JSON.stringify({
        schemaVersion: 'execution-port.remote-exchange.v2',
        workerId: row.worker_id, workerInstanceId: row.worker_instance_id,
        acknowledgements: [...replayAcknowledgements], frame: [...frame],
      }))
      return new Promise((resolveResponse, rejectResponse) => {
        const outgoing = request(endpoint, {
          method: 'POST', ca, servername: 'control.localhost', rejectUnauthorized: true,
          timeout: timeoutMillis,
          headers: { Authorization: `Bearer ${credential}`, 'Content-Type': 'application/json',
            'Content-Length': body.length, Connection: 'close' },
        }, incoming => {
          if (loseResponseAfterHeaders) {
            try {
              assert.equal(incoming.statusCode, 200, 'only a successful real exchange response is deliberately lost')
              // No response body or delivery is read. In particular, this
              // client cannot confirm the business ACK it deliberately loses.
              resolveResponse({ status: incoming.statusCode, frameAccepted: null,
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
              const status = incoming.statusCode
              const response = status === 200 ? JSON.parse(responseBytes.toString('utf8')) : null
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
                // A 401 proves transport rejection. It does not prove CP fencing.
                rejectionBoundary: status === 401 ? 'http-authentication'
                  : ['rejected_worker_instance', 'rejected_stale_fencing_token'].includes(acknowledgement?.status)
                    ? 'control-plane-authority'
                    : acknowledgement?.status.startsWith('rejected_') ? 'control-plane-message-rejection' : null })
            } catch { rejectResponse(new Error('retained-frame response violated the canonical envelope')) }
          })
        })
        outgoing.on('timeout', () => outgoing.destroy(new Error('retained-frame exchange timed out')))
        outgoing.on('error', () => rejectResponse(new Error('retained-frame HTTPS exchange failed')))
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
      return store.prepare(`SELECT r.sequence,r.request_json,r.resolution,
        a.attempt_id,a.operation_digest,a.execution,a.disposition,a.delivery,a.execution_result,a.accepted_result
        FROM tool_requests r LEFT JOIN tool_attempts a ON a.request_sequence=r.sequence
        WHERE r.thread_id=? ORDER BY r.sequence`).all(kernelSessionId)
        .filter(row => JSON.parse(row.request_json).tool_name.endsWith('exec_command'))
    } finally { store.close() }
  })
}

export function assertSuccessfulRetainedCommands(commands, expectedCount) {
  assert.equal(commands.length, expectedCount, 'the original Core has the expected exact command request count')
  for (const row of commands) {
    assert.equal(row.execution, 'completed')
    assert.equal(row.disposition, 'accepted')
    // A completed/accepted handler receipt can carry a failed OS command.
    // The original retained result is the authority for the actual exit code.
    for (const field of ['execution_result', 'accepted_result']) {
      assert.ok(row[field], `the exact command has its real retained ${field}`)
      assert.equal(JSON.parse(row[field]).code_mode_result?.exit_code, 0,
        `the real retained ${field} proves successful OS command completion`)
    }
  }
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
        // The retained Rust-serialized authority objects preserve the exact
        // field order used by ExecutionOutbox's tuple correlation. This lookup
        // reads the actual Worker proof; it never creates a response receipt.
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
