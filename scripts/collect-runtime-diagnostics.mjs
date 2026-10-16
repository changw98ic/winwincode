// SPDX-License-Identifier: Apache-2.0

import { existsSync, globSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { fileURLToPath } from 'node:url'
import { scanCredentialLeakBytes } from './credential-leak-gate.mjs'

export function collectRuntimeDiagnostics(temporary, transcript, output) {
  mkdirSync(output, { recursive: true })
  const report = { fixture: true, logs: [], databases: [] }
  const logs = globSync('**/device-process*.log', { cwd: temporary }).map(path => join(temporary, path))
  if (existsSync(transcript)) logs.unshift(transcript)
  for (const [index, path] of logs.entries()) {
    const bytes = readFileSync(path)
    const scan = scanCredentialLeakBytes({ bytes, label: `scenario-${index}.log` })
    report.logs.push({ file: relative(temporary, path), ...scan })
    if (scan.status === 'passed') writeFileSync(join(output, scan.label), bytes)
  }
  for (const path of globSync('**/*.sqlite3', { cwd: temporary })) {
    const entry = { file: path, tables: {} }
    let database
    try {
      database = new DatabaseSync(join(temporary, path), { readOnly: true })
      database.exec('PRAGMA busy_timeout=1000')
      for (const table of ['execution_leases', 'execution_dispatch_authorities', 'execution_queue_slots',
        'execution_terminal_outcomes', 'runtime_replay_frame', 'runtime_replay_head',
        'execution_outbox', 'codex_run', 'model_invocation_attempts', 'model_attempt_diagnostics',
        'jev_attempt_diagnostics']) {
        if (database.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?").get(table)) {
          entry.tables[table] = database.prepare(`SELECT count(*) AS rows FROM "${table}"`).get().rows
        }
      }
    } catch (error) { entry.error = error.code ?? 'SQLITE_DIAGNOSTIC_FAILED' }
    finally { database?.close() }
    report.databases.push(entry)
  }
  writeFileSync(join(output, 'diagnostics.json'), `${JSON.stringify(report, null, 2)}\n`)
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  collectRuntimeDiagnostics(process.env.TMPDIR, join(process.env.RUNNER_TEMP, 'runtime-sessions.log'),
    resolve(import.meta.dirname, '../.cache/runtime-diagnostics'))
}
