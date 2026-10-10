// SPDX-License-Identifier: Apache-2.0
// Read-only witness for the fresh, owned mechanism audit directory.
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { existsSync, realpathSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import { DatabaseSync } from 'node:sqlite'

const sha256 = bytes => createHash('sha256').update(bytes).digest('hex')

function ownedPath(path) {
  const ownedRoot = realpathSync(resolve(process.env.WWC_MECHANISM_AUDIT_OUTPUT ?? join(tmpdir(), 'winwincode-mechanism-audit')))
  const absolute = resolve(path)
  assert.ok(absolute.startsWith(`${ownedRoot}/`), 'witness requires owned artifact data')
  if (existsSync(absolute)) {
    assert.ok(realpathSync(absolute).startsWith(`${realpathSync(ownedRoot)}/`), 'owned data cannot escape through a symlink')
  }
  return absolute
}

function readDatabase(path, read) {
  const database = new DatabaseSync(ownedPath(path), { readOnly: true })
  try {
    database.exec('PRAGMA query_only=ON')
    database.exec('PRAGMA busy_timeout=1000')
    return read(database)
  } finally { database.close() }
}

export const witnessQueries = Object.freeze({
  registry: `SELECT worker_session_id,worker_id,worker_instance_id,data_directory,state
    FROM worker_process_registry WHERE worker_instance_id=?`,
  opens: `SELECT o.position,o.delivery_id,o.state,o.frame_json,d.rejected
    FROM execution_outbox o LEFT JOIN execution_delivery_disposition d ON d.delivery_id=o.delivery_id
    WHERE json_extract(o.frame_json,'$.kind')='model.open'
      AND json_extract(o.frame_json,'$.lease.workerInstanceId')=?
      AND json_extract(o.frame_json,'$.lease.jobId')=? ORDER BY o.position`,
  diagnostics: `SELECT sequence,policy_attempt,started_ms,finished_ms,outcome,stop_reason
    FROM model_attempt_diagnostics WHERE exchange_id=? ORDER BY sequence`,
  invocations: `SELECT attempt_number,adapter_request_id,state,accounting_chunks IS NULL AS accounting_is_null,
    response_bytes IS NULL AS response_is_null FROM model_invocation_attempts
    WHERE exchange_id=? ORDER BY attempt_number`,
  exchanges: `SELECT digest,cancelled,length(chunks) AS chunks_json_bytes,
    length(prepared_payload) AS prepared_payload_bytes,prepared_payload FROM exchanges WHERE exchange_id=?`,
})

/** Accepts (deviceData, workerInstanceId, jobId), or (deviceData, identity). */
export function snapshotPendingModelOpen(deviceData, identity, exactJobId) {
  const context = typeof identity === 'string' ? { workerInstanceId: identity, jobId: exactJobId } : identity
  assert.equal(typeof context?.workerInstanceId, 'string')
  assert.equal(typeof context?.jobId, 'string')
  const data = ownedPath(deviceData)
  const registryPath = join(data, 'device-client.sqlite3')
  if (!existsSync(registryPath)) return { status: 'registry_not_available', observedAtUtc: new Date().toISOString() }
  const rows = readDatabase(registryPath, db => db.prepare(witnessQueries.registry).all(context.workerInstanceId))
  assert.ok(rows.length <= 1, 'exact Worker instance must resolve one physical registry row')
  if (rows.length === 0) return { status: 'worker_not_registered', observedAtUtc: new Date().toISOString() }
  const worker = rows[0]
  if (context.workerId) assert.equal(worker.worker_id, context.workerId)
  const adapterPath = ownedPath(join(worker.data_directory, 'codex-runtime', 'worker-codex.sqlite3'))
  if (!existsSync(adapterPath)) return { status: 'adapter_not_available', worker, observedAtUtc: new Date().toISOString() }
  const opens = readDatabase(adapterPath, db => db.prepare(witnessQueries.opens).all(context.workerInstanceId, context.jobId))
  const providerPath = ownedPath(join(data, 'providers', 'providers.sqlite3'))
  const candidates = opens.map(row => {
    const raw = Buffer.from(row.frame_json)
    const open = JSON.parse(raw)
    if (context.leaseId) assert.equal(open.lease.leaseId, context.leaseId)
    if (context.attempt !== undefined) assert.equal(open.lease.attempt, context.attempt)
    if (context.fencingToken !== undefined) assert.equal(open.lease.fencingToken, context.fencingToken)
    const payload = Buffer.from(open.request.dataBase64, 'base64')
    const decoded = JSON.parse(payload)
    const digest = sha256(payload)
    assert.equal(open.request.payloadDigest, `sha256:${digest}`)
    const provider = existsSync(providerPath) ? readDatabase(providerPath, db => {
      const diagnostics = db.prepare(witnessQueries.diagnostics).all(open.modelExchangeId)
      const invocations = db.prepare(witnessQueries.invocations).all(open.modelExchangeId)
      const records = db.prepare(witnessQueries.exchanges).all(open.modelExchangeId)
      return { diagnostics, invocations, diagnosticCount: diagnostics.length,
        invocationCount: invocations.length, exchangeCount: records.length,
        exchanges: records.map(record => ({ digest: record.digest, cancelled: record.cancelled,
          chunksJsonBytes: record.chunks_json_bytes, preparedPayloadBytes: record.prepared_payload_bytes,
          preparedPayloadSha256: record.prepared_payload == null ? null : sha256(Buffer.from(record.prepared_payload)) })) }
    }) : null
    return { position: row.position, deliveryId: row.delivery_id, state: row.state,
      rejectedDisposition: row.rejected ?? null, modelExchangeId: open.modelExchangeId,
      requestId: open.requestId, logicalWorkerSessionId: open.workerSessionId,
      codexThreadId: open.sessionIdentity.codexThreadId,
      originalOpenFrameSha256: sha256(raw), originalPayloadSha256: digest,
      nativeThreadSha256: typeof decoded.threadId === 'string' ? sha256(decoded.threadId) : null,
      coreThreadSha256: typeof decoded.request?.client_metadata?.thread_id === 'string'
        ? sha256(decoded.request.client_metadata.thread_id)
        : typeof decoded.threadId === 'string' ? sha256(decoded.threadId) : null,
      nativeSessionSha256: typeof decoded.sessionId === 'string' ? sha256(decoded.sessionId) : null,
      expectedFirstHttpIdempotencyKey: `device-${open.modelExchangeId}:attempt:1`, provider }
  })
  const pendingCandidates = candidates.filter(value => value.state === 'pending' && value.rejectedDisposition !== 1)
  const scoped = context.modelExchangeId ? candidates.filter(value => value.modelExchangeId === context.modelExchangeId)
    : pendingCandidates.length ? pendingCandidates : candidates
  const selected = scoped.length === 1 ? scoped[0] : null
  return { status: candidates.length ? 'exact_model_open_found' : 'model_open_not_available',
    observedAtUtc: new Date().toISOString(), jobId: context.jobId, workerInstanceId: context.workerInstanceId,
    physicalWorkerSessionId: worker.worker_session_id, workerState: worker.state,
    adapterPath, providerPath, candidates,
    exactPendingOpenCount: pendingCandidates.length,
    modelExchangeId: selected?.modelExchangeId ?? null,
    coreThreadSha256: selected?.coreThreadSha256 ?? null,
    nativeThreadSha256: selected?.nativeThreadSha256 ?? null,
    expectedFirstHttpIdempotencyKey: selected?.expectedFirstHttpIdempotencyKey ?? null,
    physicalAttemptCount: selected?.provider?.diagnosticCount ?? null,
    attemptCountBoundary: 'Provider diagnostic starts before adapter.open; actual HTTP mapping remains separately required',
    invocationRecordCount: selected?.provider?.invocationCount ?? null,
    providerExchangeCount: selected?.provider?.exchangeCount ?? null,
    pendingCandidates,
    clockBoundary: 'snapshot UTC is separate from copied ModelOpen sentAt and Provider diagnostic wallclock' }
}

/** Requests must record actual Idempotency-Key plus actual thread-id hash. */
export function lookupAfterRequest(snapshot, requests, exactExchangeId) {
  const candidates = snapshot.candidates.filter(value => !exactExchangeId || value.modelExchangeId === exactExchangeId)
  assert.equal(candidates.length, 1, 'mapping requires one exact exchange witness')
  const witness = candidates[0]
  const prefix = `device-${witness.modelExchangeId}:attempt:`
  const matches = requests.filter(request => typeof request.idempotencyKey === 'string'
    && request.idempotencyKey.startsWith(prefix)).map(request => ({
      requestIndex: request.index, idempotencyKey: request.idempotencyKey,
      firstAttempt: request.idempotencyKey === witness.expectedFirstHttpIdempotencyKey,
      requestSha256: request.requestSha256,
      threadSha256Matches: request.threadIdSha256 === witness.nativeThreadSha256,
      nativeThreadSha256: request.threadIdSha256 ?? null,
    }))
  return { modelExchangeId: witness.modelExchangeId, exactPhysicalRequestCount: matches.length,
    firstAttemptRequestCount: matches.filter(value => value.firstAttempt).length,
    matches, identityMethod: 'actual HTTP Idempotency-Key contains the exact durable exchange and attempt; actual thread-id header hash is an independent cross-check',
    payloadBoundary: 'raw ModelOpen payload digest differs from translated HTTP body digest; do not compare them as equal' }
}
