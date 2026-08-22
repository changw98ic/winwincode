import assert from 'node:assert/strict'
import test from 'node:test'

import { Context } from '@deepseek-ai/cordis'

import {
  DshStrongFlowApprovalInteraction,
} from '../packages/dsh-profile/dist/index.js'

function request(signal = new AbortController().signal) {
  return Object.freeze({
    jobId: 'job-approval-fixture',
    stageRunId: 'stage-run-approval-fixture',
    attemptId: 'attempt-approval-fixture',
    roleId: 'executor',
    contextId: `role-context-sha256-${'a'.repeat(64)}`,
    operationKind: 'exec',
    operationId: 'approval-fixture-1',
    requestedScope: Object.freeze({ command: Object.freeze(['git', 'status']) }),
    source: Object.freeze({
      authority: 'codex-core',
      kernelSessionLineageId: `kernel-lineage-sha256-${'b'.repeat(64)}`,
      kernelSessionId: 'kernel-session-approval-fixture',
      kernelStreamId: `kernel-stream-sha256-${'c'.repeat(64)}`,
      kernelSequence: '17',
      turnId: 'turn-approval-fixture',
    }),
    signal,
  })
}

test('DSH approval interaction passes the exact source request to its UI answerer', async t => {
  const ctx = new Context()
  t.after(() => ctx.fiber.dispose())
  const seen = []
  ctx.on('winwincode/strongflow/approval/request', async (approvalRequest, _next) => {
    seen.push(approvalRequest)
    return 'approved'
  })

  const approvalRequest = request()
  const outcome = await new DshStrongFlowApprovalInteraction(ctx).request(approvalRequest)

  assert.equal(outcome, 'approved')
  assert.equal(seen.length, 1)
  assert.equal(seen[0], approvalRequest)
  assert.equal(seen[0].source.authority, 'codex-core')
  assert.equal(seen[0].source.kernelSequence, '17')
})

test('DSH approval interaction fails closed without a valid answerer', async t => {
  const noAnswerer = new Context()
  t.after(() => noAnswerer.fiber.dispose())
  assert.equal(
    await new DshStrongFlowApprovalInteraction(noAnswerer).request(request()),
    'unavailable',
  )

  const invalidAnswerer = new Context()
  t.after(() => invalidAnswerer.fiber.dispose())
  invalidAnswerer.on('winwincode/strongflow/approval/request', async () => 'allow-everything')
  assert.equal(
    await new DshStrongFlowApprovalInteraction(invalidAnswerer).request(request()),
    'unavailable',
  )

  const throwingAnswerer = new Context()
  t.after(() => throwingAnswerer.fiber.dispose())
  throwingAnswerer.on('winwincode/strongflow/approval/request', async () => {
    throw new Error('fixture UI failed')
  })
  assert.equal(
    await new DshStrongFlowApprovalInteraction(throwingAnswerer).request(request()),
    'unavailable',
  )
})

test('DSH approval interaction returns cancelled when its role session is aborted', async t => {
  const alreadyAborted = new Context()
  t.after(() => alreadyAborted.fiber.dispose())
  const first = new AbortController()
  first.abort('fixture cancelled')
  assert.equal(
    await new DshStrongFlowApprovalInteraction(alreadyAborted).request(request(first.signal)),
    'cancelled',
  )

  const waiting = new Context()
  t.after(() => waiting.fiber.dispose())
  let answererStarted
  const started = new Promise(resolve => { answererStarted = resolve })
  waiting.on('winwincode/strongflow/approval/request', async () => {
    answererStarted()
    return new Promise(() => {})
  })
  const second = new AbortController()
  const decision = new DshStrongFlowApprovalInteraction(waiting).request(request(second.signal))
  await started
  second.abort('role session settled')
  assert.equal(await decision, 'cancelled')
})
