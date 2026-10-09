// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import test from 'node:test'
import { DatabaseSync } from 'node:sqlite'
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { readDeviceExecutionReceipts } from '../scripts/acceptance/export-device-candidate.mjs'

test('interaction timeout receipts export exact request identity and deadline without private request contents', t => {
  const directory = mkdtempSync(join(tmpdir(), 'approval-timeout-export-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const data = join(directory, 'device-data', 'codex-runtime')
  mkdirSync(data, { recursive: true })
  const db = new DatabaseSync(join(data, 'worker-codex.sqlite3'))
  db.exec('CREATE TABLE codex_run (run_key TEXT PRIMARY KEY, record_json BLOB NOT NULL)')
  const expiresAt = '2030-01-01T00:15:00.000Z'
  const observedAt = '2030-01-01T00:15:01.000Z'
  const requestDigest = `sha256:${'a'.repeat(64)}`
  db.prepare('INSERT INTO codex_run VALUES (?, ?)').run('fixture-run', Buffer.from(JSON.stringify({
    job: { jobId: 'fixture-job' },
    interactionTimeouts: [
      { request: { kind: 'approval.request', approvalId: 'fixture-approval', expiresAt,
        action: { command: 'SYNTHETIC_PRIVATE_COMMAND', credential: 'SYNTHETIC_PRIVATE_CREDENTIAL' } },
      observedAt, requestDigest, appliedKernelSessionId: 'fixture-kernel' },
      { request: { kind: 'input.request', inputRequestId: 'fixture-input', expiresAt,
        question: 'SYNTHETIC_PRIVATE_QUESTION' }, observedAt, requestDigest,
      appliedKernelSessionId: null },
    ],
  })))
  db.close()
  const evidence = readDeviceExecutionReceipts(directory)
  assert.deepEqual(evidence.interactionTimeouts.map(({ source, ...receipt }) => receipt), [
    { runKey: 'fixture-run', jobId: 'fixture-job', kind: 'approval', id: 'fixture-approval',
      causeCode: 'INTERACTION_DEADLINE_EXPIRED', expiresAt, observedAt,
      requestSha256: requestDigest, responseSubmitted: true },
    { runKey: 'fixture-run', jobId: 'fixture-job', kind: 'input', id: 'fixture-input',
      causeCode: 'INTERACTION_DEADLINE_EXPIRED', expiresAt, observedAt,
      requestSha256: requestDigest, responseSubmitted: false },
  ])
  assert.equal(JSON.stringify(evidence).includes('SYNTHETIC_PRIVATE'), false)
})
