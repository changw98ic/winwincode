import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { access, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'
import { mkdtemp } from 'node:fs/promises'

import {
  parseStrongFlowPlanReviewContextText,
} from '../packages/contracts/dist/index.js'
import {
  createStrongFlowPlanReviewDecision,
} from '../packages/strongflow/dist/index.js'
import {
  DELIVERY_FIXTURE_BASE_TIME,
  DELIVERY_FIXTURE_UI_PROOF,
  DeliveryServiceFixtureTestkit,
  ScriptedDshFixtureRuntime,
  assertForeignDeliveryProjection,
  assertMalformedFixtureProjection,
  exerciseFixturePolicyDenial,
  keylessFixtureEnvironment,
  renderFixtureDeliveryProjection,
} from './fixtures/delivery-service-testkit.mjs'

const root = resolve(import.meta.dirname, '..')

function assertFailure(response, code) {
  assert.equal(response.ok, false)
  assert.equal(response.error.code, code)
}

function childAtCheckpoint(directory) {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(process.execPath, [
      resolve(root, 'tests/fixtures/delivery-service-checkpoint.mjs'),
      directory,
      '--hold',
    ], {
      cwd: root,
      env: keylessFixtureEnvironment(),
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stdout = ''
    let stderr = ''
    let settled = false
    const fail = (error) => {
      if (settled) return
      settled = true
      child.kill('SIGKILL')
      rejectPromise(error)
    }
    const timer = setTimeout(() => {
      fail(new Error(`checkpoint process timed out\n${stderr}\n${stdout}`))
    }, 45_000)
    child.on('error', fail)
    child.stderr.on('data', chunk => { stderr += chunk })
    child.stdout.on('data', (chunk) => {
      stdout += chunk
      const line = stdout.split('\n').find(entry => entry.trim().startsWith('{'))
      if (line === undefined || settled) return
      let checkpoint
      try {
        checkpoint = JSON.parse(line)
      } catch (error) {
        fail(error)
        return
      }
      child.kill('SIGTERM')
      child.once('close', (code, signal) => {
        if (settled) return
        settled = true
        clearTimeout(timer)
        resolvePromise({ checkpoint, code, signal, stderr })
      })
    })
  })
}

test('testkit drives a scripted DSH role through the real embedded Codex kernel', async t => {
  const kit = await DeliveryServiceFixtureTestkit.create({
    deliveryId: 'dlv_6PGC8PSH5XNSGV6Q4ASXTH9AEX',
  })
  t.after(() => kit.cleanup())
  const runtime = await ScriptedDshFixtureRuntime.create({
    owner: kit,
    home: kit.home,
    workspace: kit.repository,
    script: [{
      text: 'The fixture requirements remain separate from the solution.',
      usage: { inputTokens: 16, outputTokens: 9 },
    }],
  })
  const result = await runtime.runRole({
    sessionId: 'dsh-testkit-requirements',
    roleId: 'requirements',
    prompt: 'Record the deterministic Delivery requirements.',
    maxTokens: 96,
  })

  assert.equal(result.roleId, 'requirements')
  assert.match(result.codexSessionId, /^[A-Za-z0-9-]+$/u)
  assert.deepEqual(result.assistantMessages, [
    'The fixture requirements remain separate from the solution.',
  ])
  assert.ok(result.events.some(event => event.kind === 'turn.started'))
  assert.ok(result.events.some(event => event.kind === 'message.completed'))
  assert.ok(result.events.some(event => event.kind === 'turn.completed'))
  assert.equal(result.configuredMaxTokens, 96)
  assert.deepEqual(runtime.calls.map(call => ({
    provider: call.provider,
    model: call.model,
    maxTokens: call.maxTokens,
  })), [{ provider: 'fixture', model: 'fixture-coder', maxTokens: null }])
  assert.equal(runtime.remainingResponses, 0)
  assert.deepEqual(Object.keys(process.env).filter(name => (
    /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu.test(name)
    && Object.hasOwn(keylessFixtureEnvironment(), name)
  )), [])

  const source = await readFile(
    resolve(root, 'tests/fixtures/delivery-service-testkit.mjs'),
    'utf8',
  )
  assert.doesNotMatch(source, /packages\/(?:contracts|dsh-profile|native|strongflow)\/src\//u)
  const ownedRoot = kit.root
  await kit.cleanup()
  assert.equal(runtime.closed, true)
  await assert.rejects(access(ownedRoot), error => error?.code === 'ENOENT')
})

test('testkit covers every Delivery mutation, human gate, projection, evidence, and verdict', async t => {
  const kit = await DeliveryServiceFixtureTestkit.create({
    deliveryId: 'dlv_6X828BCTWC881956V7XW1F7P3H',
  })
  t.after(() => kit.cleanup())

  const review = await kit.preparePlanReview()
  assert.equal(review.delivery.status, 'needs-attention')
  assert.equal(review.delivery.attentionItems[0].status, 'open')
  assert.equal(review.delivery.stageRuns.at(-1).status, 'waiting')
  assert.equal(review.delivery.stageRuns.some(run => run.stage === 'executing'), false)

  const reviewProjection = await kit.service.getDeliveryProjection(kit.deliveryId)
  const reviewMarkup = await renderFixtureDeliveryProjection({
    ...reviewProjection,
    sessionId: 'dsh-fixture-plan-review',
  })
  assert.ok(reviewMarkup.indexOf('DeliverySpec') < reviewMarkup.indexOf('Solution Review Set'))
  assert.equal((reviewMarkup.match(/<figure /gu) ?? []).length, 2)
  assert.match(reviewMarkup, /批准执行/u)
  assert.match(reviewMarkup, /系统架构图/u)
  assert.match(reviewMarkup, /交付流程图/u)

  const staleDecision = {
    ...review.decision,
    deliverySpecRevision: review.decision.deliverySpecRevision - 1,
  }
  const staleApproval = await kit.approvePlan(review, {
    requestId: 'fixture:approve:stale',
    decision: staleDecision,
  })
  assertFailure(staleApproval, 'DELIVERY_CONFLICT')
  assert.equal(
    (await kit.service.getDeliveryProjection(kit.deliveryId)).delivery.revision,
    review.delivery.revision,
  )

  const approval = await kit.approvePlan(review)
  assert.equal(approval.ok, true)
  assert.equal(approval.result.delivery.status, 'executing')
  const prepared = await kit.prepareVerification(approval.result.delivery)
  assert.equal(prepared.executingProjection.diagramExecution.state, 'executing')
  assert.equal(prepared.executingProjection.diagramExecution.details, null)
  assert.equal(prepared.finishedProjection.diagramExecution.state, 'execution-finished')
  assert.equal(prepared.finishedProjection.diagramExecution.details.candidate.candidateRef,
    prepared.candidate.candidateRef)

  const executingMarkup = await renderFixtureDeliveryProjection({
    ...prepared.executingProjection,
    sessionId: 'dsh-fixture-executor',
  })
  assert.match(executingMarkup, /执行中状态/u)
  assert.match(executingMarkup, /data-execution-state="affected-live"/u)
  assert.doesNotMatch(executingMarkup, /src\/value\.mjs/u)

  const finishedMarkup = await renderFixtureDeliveryProjection({
    ...prepared.finishedProjection,
    sessionId: 'dsh-fixture-plan-review',
  })
  assert.match(finishedMarkup, /执行结束状态/u)
  assert.match(finishedMarkup, /data-execution-state="affected-finished"/u)
  assert.match(finishedMarkup, /Frozen Candidate Diff/u)

  const runtimeEvents = await kit.verificationEvents(prepared)
  const invalidEvidence = runtimeEvents.filter(event => (
    event.id !== 'dsh-fixture-verifier@3'
  ))
  const invalidVerdict = await kit.submitVerdict(prepared, invalidEvidence, {
    requestId: 'fixture:submit:invalid-evidence',
  })
  assertFailure(invalidVerdict, 'INVALID_REQUEST')
  assert.equal(
    (await kit.service.getDeliveryProjection(kit.deliveryId)).delivery.revision,
    prepared.delivery.revision,
  )

  const submitted = await kit.submitVerdict(prepared, runtimeEvents)
  assert.equal(submitted.ok, true)
  assert.equal(submitted.result.delivery.status, 'ready-to-deliver')
  assert.equal(submitted.result.delivery.verdict.status, 'pass')
  assert.ok(submitted.result.delivery.evidence.some(entry => entry.type === 'test'))
  assert.ok(submitted.result.delivery.evidence.some(entry => entry.type === 'command'))

  const runtimeProjection = await kit.runtimeProjection(submitted.result.delivery)
  const executor = runtimeProjection.stages
    .find(stage => stage.stageRun.id === 'stage-fixture-executor')
    .sessions[0]
  assert.deepEqual(executor.plan.items.map(item => item.status), [
    'completed',
    'completed',
    'in_progress',
  ])
  assert.equal(executor.agents.some(agent => (
    agent.threadId === 'codex-fixture-subagent-executor-1'
  )), true)
  assert.deepEqual(executor.diff.changedFiles, ['src/value.mjs'])
  assert.equal(executor.usage.totals.total_tokens, 42)

  const beforeRestart = await kit.service.getDeliveryProjection(kit.deliveryId)
  kit.restart()
  const afterRestart = await kit.service.getDeliveryProjection(kit.deliveryId)
  assert.deepEqual(afterRestart, beforeRestart)

  const mutationOperations = [
    'createDelivery',
    'updateDeliverySpec',
    'startStage',
    'bindSession',
    'resolveAttention',
    'submitVerdict',
  ]
  for (const operation of mutationOperations) {
    assert.equal(kit.mutationTrace.some(entry => entry.operation === operation && entry.ok), true)
    assert.equal(kit.mutationTrace.some(entry => entry.operation === operation && !entry.ok), true)
  }
  const stored = await kit.stored()
  assert.equal(stored.records.length, submitted.result.delivery.revision)
  assert.equal(new Set(stored.records.map(record => record.requestId)).size, stored.records.length)
})

test('policy denial and infrastructure failure remain explicit and cannot pass', async t => {
  const routedDenial = await exerciseFixturePolicyDenial()
  assert.equal(routedDenial.submissionId, 'submission-fixture-policy-denied')
  assert.deepEqual(routedDenial.responses[0].decision, {
    kind: 'denied',
    rejection: 'Fixture policy denied this operation.',
  })

  const kit = await DeliveryServiceFixtureTestkit.create({
    deliveryId: 'dlv_00P419PPDHSRT3KGEKV544SH12',
  })
  t.after(() => kit.cleanup())
  const review = await kit.preparePlanReview()
  const approved = await kit.approvePlan(review)
  assert.equal(approved.ok, true)
  const prepared = await kit.prepareVerification(approved.result.delivery)
  const runtimeEvents = await kit.verificationEvents(prepared, {
    reviewer: 'timed-out',
    verifier: 'policy-denied',
  })
  const submitted = await kit.submitVerdict(prepared, runtimeEvents)
  assert.equal(submitted.ok, true)
  assert.equal(submitted.result.delivery.status, 'needs-attention')
  assert.equal(submitted.result.delivery.verdict.status, 'infra_error')
  assert.notEqual(submitted.result.delivery.verdict.status, 'pass')
  const retry = submitted.result.delivery.attentionItems.find(item => (
    item.status === 'open'
    && item.options.some(option => option.id === 'retry-verification')
  ))
  assert.notEqual(retry, undefined)
  assert.equal(retry.type, 'verification_blocked')

  const resumed = await kit.requireSuccess('resolveAttention', 'fixture:resume:verification', {
    deliveryId: kit.deliveryId,
    expectedRevision: submitted.result.delivery.revision,
    attentionItemId: retry.id,
    status: 'resolved',
    resolution: 'Retry the unchanged candidate after the fixture infrastructure is restored.',
    remediation: null,
    channel: 'local-ui',
    authentication: {
      scheme: 'local-session',
      proof: DELIVERY_FIXTURE_UI_PROOF,
    },
  })
  assert.equal(resumed.status, 'verifying')
  assert.equal(resumed.attentionItems.at(-1).resolvedBy, 'fixture-ui-reviewer')
})

test('malformed and foreign Codex projections fail with stable source-bound errors', async t => {
  assert.equal(assertMalformedFixtureProjection(), 'EVENT_SEQUENCE_MISSING')
  const kit = await DeliveryServiceFixtureTestkit.create({
    deliveryId: 'dlv_1MT8NEWX94TWQ7NRFEXE5RR3T9',
  })
  t.after(() => kit.cleanup())
  const review = await kit.preparePlanReview()
  assert.equal(assertForeignDeliveryProjection(review.delivery), 'RUNTIME_SESSION_UNBOUND')
})

test('a terminated host resumes from the durable human checkpoint without replaying mutation', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'winwincode-delivery-process-testkit-'))
  t.after(() => rm(directory, { recursive: true, force: true }))

  const child = await childAtCheckpoint(directory)
  assert.equal(child.code, null)
  assert.equal(child.signal, 'SIGTERM')
  assert.equal(child.stderr, '')
  assert.equal(child.checkpoint.status, 'needs-attention')

  const kit = await DeliveryServiceFixtureTestkit.create({
    root: directory,
    deliveryId: child.checkpoint.deliveryId,
    clockStart: DELIVERY_FIXTURE_BASE_TIME + 10_000,
  })
  const before = await kit.service.getDeliveryProjection(child.checkpoint.deliveryId)
  assert.equal(before.delivery.revision, child.checkpoint.revision)
  assert.equal(before.delivery.status, 'needs-attention')
  assert.equal(before.delivery.attentionItems[0].status, 'open')
  assert.equal(before.delivery.sessionBindings.at(-1).dshSessionId,
    child.checkpoint.reviewSessionId)

  const recovery = await kit.recover([])
  assert.deepEqual(recovery.nextAction, {
    kind: 'resolve-delivery-attention',
    attentionItemIds: [child.checkpoint.attentionItemId],
  })
  const attention = before.delivery.attentionItems.find(entry => (
    entry.id === child.checkpoint.attentionItemId
  ))
  const decision = createStrongFlowPlanReviewDecision({
    context: parseStrongFlowPlanReviewContextText(attention.context),
    action: 'approve',
    comments: 'Approve the exact checkpoint after host restart.',
    requestedChanges: [],
  })
  const requestId = 'fixture:process:resume-plan-review'
  const payload = {
    deliveryId: child.checkpoint.deliveryId,
    expectedRevision: before.delivery.revision,
    attentionItemId: attention.id,
    status: 'resolved',
    resolution: JSON.stringify(decision),
    remediation: null,
    channel: 'local-ui',
    authentication: {
      scheme: 'local-session',
      proof: DELIVERY_FIXTURE_UI_PROOF,
    },
  }
  const resumed = await kit.request('resolveAttention', requestId, payload)
  assert.equal(resumed.ok, true)
  assert.equal(resumed.result.delivery.status, 'executing')
  const replayed = await kit.request('resolveAttention', requestId, payload)
  assert.deepEqual(replayed, resumed)
  const stored = await kit.stored()
  assert.equal(stored.records.length, resumed.result.delivery.revision)
  assert.equal(stored.records.filter(record => record.requestId === requestId).length, 1)
})
