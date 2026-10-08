// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import test from 'node:test'
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { cancellationEvidence } from './fixtures/code-mode-cancellation-evidence.mjs'

test('cancellation failure evidence distinguishes Core closure from a stalled Worker cursor', t => {
  const root = mkdtempSync(join(tmpdir(), 'wwc-cancel-evidence-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  const runtime = join(root, 'worker/codex-runtime'), home = join(runtime, 'kernel-home')
  mkdirSync(home, { recursive: true })
  const device = new DatabaseSync(join(root, 'device-client.sqlite3'))
  device.exec(`CREATE TABLE worker_process_registry (
    worker_session_id TEXT, state TEXT, exit_code INTEGER, data_directory TEXT)`)
  device.prepare('INSERT INTO worker_process_registry VALUES (?, ?, ?, ?)')
    .run('wss-fixture', 'running', null, join(root, 'worker'))
  device.close()
  const adapter = new DatabaseSync(join(runtime, 'worker-codex.sqlite3'))
  adapter.exec(`CREATE TABLE codex_run (run_key TEXT, record_json BLOB);
    CREATE TABLE execution_outbox (family TEXT, state TEXT, frame_json BLOB)`)
  adapter.prepare('INSERT INTO codex_run VALUES (?, ?)').run('run', Buffer.from(JSON.stringify({
    kernelSessionId: 'kernel', canonicalThreadId: 'thread', phase: 'running',
    terminal: { kind: 'cancelled', secret: 'SECRET-CANARY' },
    coreToolCursor: 7, coreToolFinalCursor: null, coreToolPending: null,
    lastAgentMessage: 'SECRET-CANARY', providerApiKey: 'SECRET-CANARY',
  })))
  adapter.prepare('INSERT INTO execution_outbox VALUES (?, ?, ?)')
    .run('runtime-event', 'sent_attempt', Buffer.from('SECRET-CANARY'))
  adapter.close()
  const core = new DatabaseSync(join(home, 'state_5.sqlite'))
  core.exec(`CREATE TABLE tool_runtime_cells (
    sequence INTEGER, thread_id TEXT, cell_id TEXT, lifecycle TEXT, revision INTEGER);
    CREATE TABLE tool_fact_events (sequence INTEGER, thread_id TEXT, fact_json TEXT);
    INSERT INTO tool_runtime_cells VALUES (1, 'thread', 'cell', 'closed', 2);
    INSERT INTO tool_fact_events VALUES (9, 'thread', 'SECRET-CANARY')`)
  core.close()
  const evidence = cancellationEvidence(root)
  assert.equal(evidence[0].processState, 'running')
  assert.equal(evidence[0].adapter.runs[0].coreToolCursor, 7)
  assert.equal(evidence[0].core[0].cells[0].lifecycle, 'closed')
  assert.equal(evidence[0].core[0].facts[0].sourceSequence, 9)
  assert.deepEqual(evidence[0].adapter.outbox.map(row => ({ ...row })), [
    { family: 'runtime-event', state: 'sent_attempt', count: 1 },
  ])
  assert.ok(!JSON.stringify(evidence).includes('SECRET-CANARY'))
  assert.ok(!JSON.stringify(evidence).includes(root))
})

test('unavailable cancellation diagnostics do not look like an empty receipt stream', t => {
  const root = mkdtempSync(join(tmpdir(), 'wwc-cancel-evidence-missing-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  assert.deepEqual(cancellationEvidence(root), { unavailable: 'missing' })
  new DatabaseSync(join(root, 'device-client.sqlite3')).close()
  assert.deepEqual(cancellationEvidence(root), { unavailable: 'read-failed' })
})
