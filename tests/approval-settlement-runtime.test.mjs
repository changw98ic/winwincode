// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import test from 'node:test'
import { DatabaseSync } from 'node:sqlite'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { resolveDeviceTaskApprovals } from '../scripts/lib/device-production-fixture.mjs'
import { readDevicePendingApprovalIds } from '../scripts/acceptance/export-device-candidate.mjs'

const run = {
  id: 'wrn_01J00000000000000000000001', state: 'running',
  productSessionId: 'psn_01J00000000000000000000001',
  codexThreadId: 'cdx_01J00000000000000000000001',
  workerSessionId: 'wsn_01J00000000000000000000001',
  executionJobId: 'job_01J00000000000000000000001',
}
const expiredApproval = {
  id: 'apr_01J00000000000000000000001', state: 'expired', category: 'shell',
  expiresAt: '2030-01-01T00:15:00.000Z',
  binding: {
    productSessionId: run.productSessionId, workerSessionId: run.workerSessionId,
    executionJobId: run.executionJobId,
    sessionIdentity: {
      workRunId: run.id, codexThreadId: run.codexThreadId,
      productSessionId: run.productSessionId, workerSessionId: run.workerSessionId,
    },
  },
}

function approvalApi(items) {
  const calls = []
  return {
    calls,
    async query(kind, payload) {
      calls.push({ kind, payload })
      assert.equal(kind, 'approval.list')
      return { page: { hasMore: false }, result: {
        items: items.filter(item => payload.states.includes(item.state)),
      } }
    },
    async command() { assert.fail('observing approval expiry must not create a Control Plane decision') },
  }
}

test('expired approval remains visible while its exact Core operation is pending', async () => {
  const api = approvalApi([expiredApproval])
  const pending = []
  await resolveDeviceTaskApprovals({
    api, runs: [run], automaticTaskActions: true, approvalOwner: 'execution_port',
    corePendingApprovalIds: [expiredApproval.id],
    onPending: value => pending.push(...value),
  })
  assert.deepEqual(pending.map(item => item.id), [expiredApproval.id],
    'a derived expired projection must not hide an unresolved Core tool wait')
  assert.ok(api.calls[0].payload.states.includes('expired'))
})

test('expired approvals already settled in Core do not remain in the waiting set', async () => {
  const api = approvalApi([expiredApproval])
  const pending = []
  await resolveDeviceTaskApprovals({
    api, runs: [run], automaticTaskActions: true, approvalOwner: 'execution_port',
    corePendingApprovalIds: [], onPending: value => pending.push(...value),
  })
  assert.deepEqual(pending, [])
})

test('an expired approval for another exact execution is never attributed to this WorkRun', async () => {
  const foreign = structuredClone(expiredApproval)
  foreign.binding.executionJobId = 'job_01J00000000000000000000002'
  const api = approvalApi([foreign])
  const pending = []
  await resolveDeviceTaskApprovals({
    api, runs: [run], automaticTaskActions: true, approvalOwner: 'execution_port',
    corePendingApprovalIds: [foreign.id], onPending: value => pending.push(...value),
  })
  assert.deepEqual(pending, [])
})

test('Core pending lookup requires exact job and thread and excludes terminal or resolved operations', t => {
  const directory = mkdtempSync(join(tmpdir(), 'approval-core-lookup-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const data = join(directory, 'device-data', 'codex-runtime')
  mkdirSync(data, { recursive: true })
  const db = new DatabaseSync(join(data, 'worker-codex.sqlite3'))
  db.exec(`CREATE TABLE codex_run (run_key TEXT PRIMARY KEY, record_json BLOB NOT NULL);
    CREATE TABLE approval_operation (approval_id TEXT PRIMARY KEY, run_key TEXT NOT NULL, state TEXT NOT NULL)`)
  const insert = (key, approval, job, thread, terminal, state = 'pending') => {
    db.prepare('INSERT INTO codex_run VALUES (?, ?)').run(key, Buffer.from(JSON.stringify({
      job: { jobId: job }, canonicalThreadId: thread, terminal,
      privateSentinel: 'never-export-this-model-content',
    })))
    db.prepare('INSERT INTO approval_operation VALUES (?, ?, ?)').run(approval, key, state)
  }
  insert('exact-z', 'apr_z', run.executionJobId, run.codexThreadId, null)
  insert('exact-a', 'apr_a', run.executionJobId, run.codexThreadId, null)
  insert('foreign-job', 'apr_foreign_job', 'job_other', run.codexThreadId, null)
  insert('foreign-thread', 'apr_foreign_thread', run.executionJobId, 'cdx_other', null)
  insert('terminal', 'apr_terminal', run.executionJobId, run.codexThreadId, { kind: 'failed' })
  insert('resolved', 'apr_resolved', run.executionJobId, run.codexThreadId, null, 'resolved')
  db.close()
  assert.deepEqual(readDevicePendingApprovalIds(directory, [run, run]), ['apr_a', 'apr_z'])
})

test('registered driver preserves expiry blockage across projections and clears it only after Core settlement', async t => {
  const directory = mkdtempSync(join(tmpdir(), 'approval-expiry-driver-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const data = join(directory, 'device-data', 'codex-runtime')
  mkdirSync(data, { recursive: true })
  const db = new DatabaseSync(join(data, 'worker-codex.sqlite3'))
  t.after(() => db.close())
  db.exec(`CREATE TABLE codex_run (run_key TEXT PRIMARY KEY, record_json BLOB NOT NULL);
    CREATE TABLE approval_operation (approval_id TEXT PRIMARY KEY, run_key TEXT NOT NULL, state TEXT NOT NULL)`)
  db.prepare('INSERT INTO codex_run VALUES (?, ?)').run('fixture-run', Buffer.from(JSON.stringify({
    job: { jobId: run.executionJobId }, canonicalThreadId: run.codexThreadId, terminal: null,
  })))
  db.prepare('INSERT INTO approval_operation VALUES (?, ?, ?)')
    .run(expiredApproval.id, 'fixture-run', 'pending')
  const launch = { productSessionId: run.productSessionId, deliveryId: 'dlv_01J00000000000000000000001',
    callId: 'approval-expiry-fixture', directory }
  writeFileSync(join(directory, 'device-task-result.json'), JSON.stringify({
    ...launch, workRunId: run.id, workerSessionId: run.workerSessionId,
    steps: [], workRuns: [run], modelRoute: { providerId: 'offline', modelId: 'offline' },
  }))
  const journal = () => JSON.parse(readFileSync(join(directory, 'task-supervision.json'), 'utf8'))
  const samples = []
  const apiUrl = new URL('../scripts/acceptance/run-api-production-vertical.mjs', import.meta.url)
  const originalApi = await import(apiUrl.href)
  t.mock.module(apiUrl, { namedExports: {
    ...originalApi,
    async driveDelivery(_api, _timeout, _route, _clock, options) {
      const aggregate = { runs: [run] }
      options.onProjection({ detail: { readCursor: { token: 'fixture-first' } }, workRunAggregate: aggregate })
      await options.onActiveWorkRuns([run])
      samples.push(journal())
      options.onProjection({ detail: { readCursor: { token: 'fixture-second' } }, workRunAggregate: aggregate })
      samples.push(journal())
      db.prepare("UPDATE approval_operation SET state='resolved' WHERE approval_id=?").run(expiredApproval.id)
      await options.onActiveWorkRuns([run])
      samples.push(journal())
      return { detail: { status: 'done', currentCandidate: { candidateRef: 'fixture-candidate' } } }
    },
  } })
  const { resumeRegisteredDeviceTask } = await import(
    new URL('../scripts/acceptance/run-device-task-vertical.mjs?approval-expiry-test', import.meta.url).href)
  const api = approvalApi([expiredApproval])
  await resumeRegisteredDeviceTask({ launch, automaticTaskActions: true, runtime: {
    api, devicePath: {
      forProductSession: () => ({}), assertBenchmarkRunning() {},
      async launchAnchor() { assert.fail('recovery must keep the existing execution authority') },
    },
  } })
  for (const sample of samples.slice(0, 2)) {
    assert.equal(sample.phase, 'blocked', 'expired Core wait cannot be reported as executing')
    assert.equal(sample.blockedReason, 'DEVICE_APPROVAL_EXPIRY_UNSETTLED')
    assert.deepEqual(sample.expiredApprovalIds, [expiredApproval.id])
    assert.deepEqual(sample.pendingApprovalIds, [expiredApproval.id])
  }
  assert.equal(samples[2].phase, 'executing')
  assert.equal(samples[2].blockedReason, null)
  assert.deepEqual(samples[2].expiredApprovalIds, [])
  assert.deepEqual(samples[2].pendingApprovalIds, [])
  assert.equal(journal().phase, 'completed')
})
