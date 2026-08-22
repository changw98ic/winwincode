import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  DurableStrongFlowSecurityAudit,
  StrongFlowSecurityAuditError,
  redactStrongFlowSecurityValue,
} from '../packages/strongflow/dist/index.js'

function event(overrides = {}) {
  return Object.freeze({
    schemaVersion: 1,
    type: 'strongflow.security.tool.requested',
    jobId: 'job-security-audit',
    stageRunId: 'stage-security-audit',
    attemptId: 'attempt-security-audit',
    roleId: 'executor',
    contextId: 'context-security-audit',
    source: Object.freeze({
      authority: 'codex-core',
      kernelSessionLineageId: 'lineage-security-audit',
      kernelSessionId: 'session-security-audit',
      kernelStreamId: 'stream-security-audit',
      kernelSequence: '7',
      turnId: 'turn-security-audit',
      operationId: 'call-security-audit',
    }),
    outcome: 'requested',
    facts: Object.freeze({ tool: 'command.run' }),
    ...overrides,
  })
}

async function fixture(t) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-security-audit-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  return home
}

test('durably chains concurrent security facts without retaining credential values', async t => {
  const home = await fixture(t)
  const secret = 'fixture-exact-sensitive-value'
  const audit = new DurableStrongFlowSecurityAudit({
    home,
    sensitiveValues: [secret],
  })
  await Promise.all([
    audit.append(event({
      facts: Object.freeze({
        authorization: `Bearer ${secret}`,
        argv: ['fixture', `TOKEN=${secret}`],
      }),
    })),
    audit.append(event({
      type: 'strongflow.security.tool.completed',
      outcome: 'completed',
      facts: Object.freeze({
        outputSha256: 'sha256:fixture',
        privateKey: secret,
      }),
    })),
  ])

  const records = await audit.read('job-security-audit')
  assert.equal(records.length, 2)
  assert.deepEqual(records.map(record => record.sequence), ['1', '2'])
  assert.equal(records[0].previousDigest, null)
  assert.equal(records[1].previousDigest, records[0].digest)
  assert.match(records[0].digest, /^sha256:[a-f0-9]{64}$/u)
  assert.equal(JSON.stringify(records).includes(secret), false)
  assert.equal(JSON.stringify(records).includes('Bearer fixture'), false)

  const key = createHash('sha256').update('job-security-audit').digest('hex')
  const path = join(home, 'strongflow-security-audit', key, 'security.jsonl')
  assert.equal((await stat(path)).mode & 0o777, 0o600)
  assert.equal((await readFile(path, 'utf8')).includes(secret), false)
})

test('strictly rejects a modified durable security record', async t => {
  const home = await fixture(t)
  const audit = new DurableStrongFlowSecurityAudit({ home })
  await audit.append(event())
  const key = createHash('sha256').update('job-security-audit').digest('hex')
  const path = join(home, 'strongflow-security-audit', key, 'security.jsonl')
  const stored = await readFile(path, 'utf8')
  await writeFile(path, stored.replace('"tool":"command.run"', '"tool":"test.run"'))
  await assert.rejects(
    audit.read('job-security-audit'),
    error => error instanceof StrongFlowSecurityAuditError && error.code === 'AUDIT_CORRUPT',
  )
})

test('redacts key names, bearer values, assignments, JWTs, private keys, and exact secrets', () => {
  const secret = 'fixture-sensitive-value'
  const redacted = redactStrongFlowSecurityValue({
    apiKey: secret,
    diagnostic: [
      `Bearer ${secret}`,
      `PASSWORD=${secret}`,
      'eyJheader.payload.signature',
      `-----BEGIN PRIVATE KEY-----\n${secret}\n-----END PRIVATE KEY-----`,
      `exact ${secret}`,
    ],
  }, [secret])
  const text = JSON.stringify(redacted)
  assert.equal(text.includes(secret), false)
  assert.equal(text.includes('eyJheader.payload.signature'), false)
  assert.equal(text.includes('BEGIN PRIVATE KEY'), false)
})
