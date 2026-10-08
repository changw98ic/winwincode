// SPDX-License-Identifier: Apache-2.0
import { closeSync, existsSync, fstatSync, openSync, readSync, readdirSync } from 'node:fs'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'

function readDatabase(path, read) {
  if (!existsSync(path)) return { unavailable: 'missing' }
  let database
  try {
    database = new DatabaseSync(path, { readOnly: true })
    database.exec('PRAGMA busy_timeout = 1000')
    return read(database)
  } catch {
    // Keep a failed diagnostic distinct from empty evidence. Never expose SQL,
    // paths or raw records, which can contain credentials and model content.
    return { unavailable: 'read-failed' }
  } finally { database?.close() }
}

function intakeFailures(path) {
  if (!existsSync(path)) return { unavailable: 'missing' }
  const allowed = new Set([
    'model_bridge:resolve:MODEL_BRIDGE_UNAVAILABLE', 'model_bridge:resolve:MODEL_BRIDGE_CONFLICT',
    'model_bridge:resolve:MODEL_EXCHANGE_UNKNOWN', 'model_bridge:resolve:MODEL_AUTHORITY_STALE',
    'model_bridge:authority:MODEL_AUTHORITY_STALE', 'model_bridge:authority:MODEL_BRIDGE_UNAVAILABLE',
    'model_bridge:retention:MODEL_PAYLOAD_INVALID', 'model_bridge:retention:MODEL_BRIDGE_CONFLICT',
    'model_bridge:retention:MODEL_BRIDGE_UNAVAILABLE',
    'worker:accept_chunk:MODEL_CHUNK_ACCEPT_FAILED',
    'device_models:accept_chunk:MODEL_CHUNK_ACCEPT_FAILED',
    'device_models:poll_codex:FOREIGN_OR_UNOWNED_EXCHANGE',
    'device_models:recover_skip:FOREIGN_EXCHANGE',
  ])
  let descriptor
  try {
    descriptor = openSync(path, 'r')
    const size = fstatSync(descriptor).size, start = Math.max(0, size - 65_536)
    const bytes = Buffer.alloc(size - start)
    const count = readSync(descriptor, bytes, 0, bytes.length, start)
    const rows = bytes.subarray(0, count).toString('utf8').split('\n')
    if (start > 0) rows.shift()
    const totals = new Map()
    for (const line of rows) {
      const match = /\bcomponent=(\w+) stage=(\w+) code=(\w+) /u.exec(line)
      if (!match) continue
      const [, component, stage, code] = match, key = `${component}:${stage}:${code}`
      if (!allowed.has(key)) continue
      totals.set(key, { component, stage, code, count: (totals.get(key)?.count ?? 0) + 1 })
    }
    return [...totals.values()].slice(-16)
  } catch { return { unavailable: 'read-failed' } }
  finally { if (descriptor !== undefined) closeSync(descriptor) }
}

/** Read bounded metadata at the Device -> Worker -> Core cancellation seams. */
export function cancellationEvidence(deviceData) {
  return readDatabase(join(deviceData, 'device-client.sqlite3'), device => {
    const workers = device.prepare(`SELECT worker_session_id, state, exit_code, data_directory
      FROM worker_process_registry ORDER BY worker_session_id LIMIT 16`).all()
    return workers.map(worker => {
      const runtime = join(worker.data_directory, 'codex-runtime')
      const home = join(runtime, 'kernel-home')
      return {
        workerSessionId: worker.worker_session_id,
        processState: worker.state,
        exitCode: worker.exit_code,
        intakeFailures: intakeFailures(join(runtime, 'model-intake.log')),
        adapter: readDatabase(join(runtime, 'worker-codex.sqlite3'), adapter => ({
          runs: adapter.prepare(`SELECT
            json_extract(record_json, '$.kernelSessionId') AS kernelSessionId,
            json_extract(record_json, '$.canonicalThreadId') AS canonicalThreadId,
            json_extract(record_json, '$.phase') AS phase,
            json_type(record_json, '$.terminal') AS terminalType,
            json_extract(record_json, '$.pendingCompletion.kind') AS pendingCompletion,
            json_extract(record_json, '$.coreToolCursor') AS coreToolCursor,
            json_extract(record_json, '$.coreToolFinalCursor') AS coreToolFinalCursor,
            json_type(record_json, '$.coreToolPending') AS pendingToolFact
            FROM codex_run ORDER BY run_key LIMIT 16`).all(),
          outbox: adapter.prepare(`SELECT family, state, count(*) AS count
            FROM execution_outbox GROUP BY family, state ORDER BY family, state LIMIT 16`).all(),
        })),
        core: existsSync(home) ? readdirSync(home).filter(name => /^state_\d+\.sqlite$/u.test(name))
          .sort().slice(0, 4).map(name => readDatabase(join(home, name), core => ({
            cells: core.prepare(`SELECT thread_id AS threadId, cell_id AS cellId,
              lifecycle, revision FROM tool_runtime_cells ORDER BY sequence DESC LIMIT 16`).all(),
            facts: core.prepare(`SELECT thread_id AS threadId, max(sequence) AS sourceSequence,
              count(*) AS count FROM tool_fact_events GROUP BY thread_id LIMIT 16`).all(),
          }))) : { unavailable: 'missing' },
      }
    })
  })
}
