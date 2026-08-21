import assert from 'node:assert/strict'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  CodexRuntimeProjector,
  RuntimeSessionLedger,
  RuntimeSessionLedgerError,
} from '../packages/dsh-profile/dist/index.js'

function kernelEvent(sequence, type, data = {}) {
  const payload = { id: `submission-${sequence}`, msg: { type, ...data } }
  return {
    sequence: BigInt(sequence),
    kind: type,
    payload,
    rawJson: JSON.stringify(payload),
  }
}

async function fixtureLedger(t) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-ledger-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const lifecycle = {
    kernelSessionId: 'kernel-session-1',
    kernelStreamId: 'kernel-stream-1',
    rolloutPath: join(home, 'rollout-1.jsonl'),
    provider: 'deepseek',
    model: 'deepseek-chat',
  }
  const ledger = await RuntimeSessionLedger.create({
    home,
    dshSessionId: 'dsh-session-1',
    roleId: 'chat',
    cwd: '/workspace',
    ...lifecycle,
  })
  return { home, ledger, lifecycle }
}

test('stores DSH and kernel identities separately and reopens append-only runtime history', async t => {
  const { home, ledger } = await fixtureLedger(t)
  const projector = new CodexRuntimeProjector({
    sessionId: 'dsh-session-1',
    kernelSessionId: 'kernel-session-1',
    roleId: 'chat',
    kernelStreamId: 'kernel-stream-1',
  })
  const first = projector.ingest(kernelEvent(1, 'task_started', { turn_id: 'turn-1' }))
  const second = projector.ingest(kernelEvent(2, 'agent_message', { message: 'done' }))
  assert.ok(first)
  assert.ok(second)
  await Promise.all([ledger.appendEvent(first), ledger.appendEvent(second)])

  const reopened = await RuntimeSessionLedger.open(home, 'dsh-session-1')
  const snapshot = await reopened.read()
  assert.equal(snapshot.manifest.dshSessionId, 'dsh-session-1')
  assert.equal(snapshot.manifest.kernelSessionId, 'kernel-session-1')
  assert.deepEqual(snapshot.events, [first, second])
  assert.equal(snapshot.records[0].recordType, 'kernel.lifecycle')
  assert.equal(snapshot.records.at(-1).recordType, 'runtime.event')
})

test('records a new kernel stream and repairs a stale manifest from the durable lifecycle record', async t => {
  const { home, ledger, lifecycle } = await fixtureLedger(t)
  const next = {
    kernelSessionId: 'kernel-session-2',
    kernelStreamId: 'kernel-stream-2',
    rolloutPath: lifecycle.rolloutPath,
    provider: 'anthropic',
    model: 'claude-sonnet-4-6',
  }
  await ledger.appendLifecycle(next)
  const stale = { ...ledger.manifest, ...lifecycle }
  await writeFile(ledger.manifestPath, `${JSON.stringify(stale, null, 2)}\n`, 'utf8')

  const reopened = await RuntimeSessionLedger.open(home, 'dsh-session-1')
  assert.deepEqual(reopened.manifest, {
    schemaVersion: 1,
    dshSessionId: 'dsh-session-1',
    roleId: 'chat',
    cwd: '/workspace',
    ...next,
  })
  assert.deepEqual(JSON.parse(await readFile(reopened.manifestPath, 'utf8')), reopened.manifest)
})

test('rejects duplicate ledgers, wrong sessions, gaps, and corrupt JSONL', async t => {
  const { home, ledger, lifecycle } = await fixtureLedger(t)
  await assert.rejects(
    RuntimeSessionLedger.create({
      home,
      dshSessionId: 'dsh-session-1',
      roleId: 'chat',
      cwd: '/workspace',
      ...lifecycle,
    }),
    error => error instanceof RuntimeSessionLedgerError
      && error.code === 'LEDGER_ALREADY_EXISTS',
  )
  const projector = new CodexRuntimeProjector({
    sessionId: 'dsh-session-1',
    kernelSessionId: 'kernel-session-1',
    roleId: 'chat',
    kernelStreamId: 'kernel-stream-1',
    startAfterSequence: 1,
  })
  const gap = projector.ingest(kernelEvent(1, 'agent_message', { message: 'gap' }))
  assert.ok(gap)
  await assert.rejects(
    ledger.appendEvent(gap),
    error => error instanceof RuntimeSessionLedgerError
      && error.code === 'LEDGER_SEQUENCE_MISMATCH',
  )
  await writeFile(ledger.recordsPath, '{bad json}\n', { flag: 'a' })
  await assert.rejects(
    RuntimeSessionLedger.open(home, 'dsh-session-1'),
    error => error instanceof RuntimeSessionLedgerError && error.code === 'LEDGER_CORRUPT',
  )
})
