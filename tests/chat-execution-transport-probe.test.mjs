// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { assertSuccessfulRetainedCommands, readRetainedCoreCommandResults } from './fixtures/chat-execution-transport-probe.mjs'

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
