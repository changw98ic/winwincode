import { randomUUID } from 'node:crypto'
import { DatabaseSync } from 'node:sqlite'
import { isAbsolute, resolve } from 'node:path'

// A claimed cell is never automatically retried: its product run must be
// reconciled first, even when the runner died before receiving a result.
export function openBenchmarkLedger(path, identity, cells) {
  const database = new DatabaseSync(path)
  const transaction = action => {
    database.exec('BEGIN IMMEDIATE')
    try {
      const result = action()
      database.exec('COMMIT')
      return result
    } catch (error) {
      database.exec('ROLLBACK')
      throw error
    }
  }
  const reject = code => {
    throw Object.assign(new Error(code), { code })
  }
  const launches = index => database.prepare(
    'SELECT target FROM benchmark_launch WHERE ordinal = ? ORDER BY call_id',
  ).all(index).map(row => JSON.parse(row.target))
  const calls = index => database.prepare(
    'SELECT record FROM benchmark_call WHERE ordinal = ? ORDER BY rowid',
  ).all(index).map(row => JSON.parse(row.record))
  try {
    database.exec(`
      PRAGMA busy_timeout = 5000;
      PRAGMA synchronous = FULL;
      PRAGMA foreign_keys = ON;
      CREATE TABLE IF NOT EXISTS benchmark_identity (id INTEGER PRIMARY KEY CHECK (id = 1), identity TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS benchmark_cell (
        ordinal INTEGER PRIMARY KEY,
        run_id TEXT NOT NULL UNIQUE,
        token TEXT,
        record TEXT,
        CHECK (record IS NULL OR token IS NOT NULL)
      );
      CREATE TABLE IF NOT EXISTS benchmark_call (
        ordinal INTEGER NOT NULL REFERENCES benchmark_cell(ordinal),
        call_id TEXT NOT NULL,
        record TEXT NOT NULL,
        PRIMARY KEY (ordinal, call_id)
      );
      CREATE TABLE IF NOT EXISTS benchmark_launch (
        ordinal INTEGER NOT NULL REFERENCES benchmark_cell(ordinal),
        call_id TEXT NOT NULL,
        directory TEXT NOT NULL UNIQUE,
        target TEXT NOT NULL,
        PRIMARY KEY (ordinal, call_id)
      );
    `)
    transaction(() => {
      const existing = database.prepare('SELECT identity FROM benchmark_identity WHERE id = 1').get()
      if (existing) {
        if (existing.identity !== identity) reject('LEDGER_IDENTITY_MISMATCH')
        return
      }
      database.prepare('INSERT INTO benchmark_identity VALUES (1, ?)').run(identity)
      const insert = database.prepare('INSERT INTO benchmark_cell (ordinal, run_id) VALUES (?, ?)')
      cells.forEach((cell, index) => insert.run(index, cell.runId))
    })
  } catch (error) {
    database.close()
    throw error
  }
  return {
    // Read retained results without claiming or recovering any pending cell.
    records() {
      const rows = database.prepare('SELECT ordinal, record FROM benchmark_cell ORDER BY ordinal').all()
      if (rows.length !== cells.length) reject('LEDGER_CELL_MISSING')
      return rows.map((row, index) => {
        if (row.ordinal !== index) reject('LEDGER_CELL_MISSING')
        return row.record === null ? null : JSON.parse(row.record)
      })
    },
    claim(index) {
      return transaction(() => {
        const row = database.prepare('SELECT token, record FROM benchmark_cell WHERE ordinal = ?').get(index)
        if (!row) reject('LEDGER_CELL_MISSING')
        if (row.record !== null) return { record: JSON.parse(row.record) }
        if (row.token !== null) {
          throw Object.assign(new Error('LEDGER_RUN_UNRESOLVED'), {
            code: 'LEDGER_RUN_UNRESOLVED', claimToken: row.token, launches: launches(index), calls: calls(index),
          })
        }
        const token = randomUUID()
        database.prepare('UPDATE benchmark_cell SET token = ? WHERE ordinal = ?').run(token, index)
        return { token }
      })
    },
    registerLaunch(index, token, target) {
      return transaction(() => {
        const row = database.prepare('SELECT run_id, token, record FROM benchmark_cell WHERE ordinal = ?').get(index)
        if (!row || row.token !== token || token === null || row.record !== null) reject('LEDGER_CLAIM_MISMATCH')
        if (!target || Object.keys(target).sort().join(',') !== 'callId,deliveryId,directory,productSessionId'
          || typeof target.callId !== 'string' || !target.callId.startsWith(`${row.run_id}:`)
          || !/^psn_[0-9A-HJKMNP-TV-Z]{26}$/u.test(target.productSessionId)
          || !/^dlv_[0-9A-HJKMNP-TV-Z]{26}$/u.test(target.deliveryId)
          || typeof target.directory !== 'string' || !isAbsolute(target.directory)
          || resolve(target.directory) !== target.directory) reject('LEDGER_LAUNCH_INVALID')
        const encoded = JSON.stringify({
          callId: target.callId, directory: target.directory,
          productSessionId: target.productSessionId, deliveryId: target.deliveryId,
        })
        const existing = database.prepare('SELECT target FROM benchmark_launch WHERE ordinal = ? AND call_id = ?').get(index, target.callId)
        if (existing) {
          if (existing.target !== encoded) reject('LEDGER_LAUNCH_CONFLICT')
          return
        }
        if (database.prepare('SELECT 1 FROM benchmark_launch WHERE directory = ?').get(target.directory)) reject('LEDGER_LAUNCH_CONFLICT')
        database.prepare('INSERT INTO benchmark_launch VALUES (?, ?, ?, ?)').run(index, target.callId, target.directory, encoded)
      })
    },
    launches,
    calls,
    recordCall(index, token, record) {
      return transaction(() => {
        const row = database.prepare('SELECT run_id, token, record FROM benchmark_cell WHERE ordinal = ?').get(index)
        if (!row || row.token !== token || token === null || row.record !== null) reject('LEDGER_CLAIM_MISMATCH')
        if (!record || typeof record.callId !== 'string' || !record.callId.startsWith(`${row.run_id}:`)
          || !['returned', 'failed'].includes(record.status)) reject('LEDGER_CALL_INVALID')
        const encoded = JSON.stringify(record)
        const existing = database.prepare('SELECT record FROM benchmark_call WHERE ordinal = ? AND call_id = ?').get(index, record.callId)
        if (existing) {
          if (existing.record !== encoded) reject('LEDGER_CALL_CONFLICT')
          return
        }
        database.prepare('INSERT INTO benchmark_call VALUES (?, ?, ?)').run(index, record.callId, encoded)
      })
    },
    finish(index, token, record) {
      const result = database.prepare(`
        UPDATE benchmark_cell SET record = ?
        WHERE ordinal = ? AND token = ? AND record IS NULL AND run_id = ?
      `).run(JSON.stringify(record), index, token, record.runId)
      if (result.changes !== 1) reject('LEDGER_COMPLETION_CONFLICT')
    },
    recover(index, observed, record) {
      return transaction(() => {
        const row = database.prepare('SELECT run_id, token, record FROM benchmark_cell WHERE ordinal = ?').get(index)
        if (!row || row.record !== null || row.token === null || row.token !== observed.claimToken
          || row.run_id !== record.runId
          || JSON.stringify(launches(index)) !== JSON.stringify(observed.launches)
          || JSON.stringify(calls(index)) !== JSON.stringify(observed.calls)) reject('LEDGER_RECOVERY_CONFLICT')
        if (record.calls !== undefined) {
          if (!Array.isArray(record.calls) || record.calls.length < observed.calls.length) {
            reject('LEDGER_RECOVERY_CONFLICT')
          }
          for (const [position, prior] of observed.calls.entries()) {
            const resolved = record.calls[position]
            if (JSON.stringify(resolved) === JSON.stringify(prior)) continue
            const launch = observed.launches.find(target => target.callId === prior.callId)
            // An export or unresolved observation error is not a Provider outcome. Resolve only that
            // exact registered call from retained product evidence, and keep
            // its original failure in the same atomic recovery record.
            if (prior.status !== 'failed' || (prior.failure?.code !== 'BENCHMARK_EVIDENCE_FAILED'
                && prior.unresolvedDeviceExecution !== true)
              || resolved?.status !== 'returned' || resolved.callId !== prior.callId
              || !launch || resolved.result?.directory !== launch.directory || !resolved.result.recovery
              || record.recovery?.kind !== 'retained-product-result'
              || JSON.stringify(record.recovery.originalCalls) !== JSON.stringify(observed.calls)) {
              reject('LEDGER_RECOVERY_CONFLICT')
            }
            database.prepare('UPDATE benchmark_call SET record = ? WHERE ordinal = ? AND call_id = ?')
              .run(JSON.stringify(resolved), index, prior.callId)
          }
          const insert = database.prepare('INSERT INTO benchmark_call VALUES (?, ?, ?)')
          for (const call of record.calls.slice(observed.calls.length)) {
            if (!call || typeof call.callId !== 'string' || !call.callId.startsWith(`${row.run_id}:`)
              || !['returned', 'failed'].includes(call.status)
              || database.prepare('SELECT 1 FROM benchmark_call WHERE ordinal = ? AND call_id = ?').get(index, call.callId)) {
              reject('LEDGER_RECOVERY_CONFLICT')
            }
            insert.run(index, call.callId, JSON.stringify(call))
          }
        }
        // Fence the old driver and commit the recovered result together. Never clear a claim.
        database.prepare('UPDATE benchmark_cell SET token = ?, record = ? WHERE ordinal = ?')
          .run(randomUUID(), JSON.stringify(record), index)
      })
    },
    close() { database.close() },
  }
}
