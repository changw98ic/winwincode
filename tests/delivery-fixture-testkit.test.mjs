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
  assertForeignDeliveryProjection,
  assertMalformedFixtureProjection,
  exerciseFixturePolicyDenial,
  keylessFixtureEnvironment,
} from './fixtures/delivery-service-testkit.mjs'
import { RuntimeSessionLedger } from './fixtures/dsh-profile/index.mjs'

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

test('testkit drives a scripted planner role through the repository runtime-event ledger', async t => {
  const kit = await DeliveryServiceFixtureTestkit.create({
    deliveryId: 'dlv_6PGC8PSH5XNSGV6Q4ASXTH9AEX',
  })
  t.after(() => kit.cleanup())
  const review = await kit.preparePlanReview()

  const stored = await RuntimeSessionLedger.open(kit.home, 'dsh-fixture-planner')
    .then(ledger => ledger.read())
  assert.equal(stored.manifest.roleId, 'planner')
  assert.equal(stored.manifest.kernelSessionId, 'codex-fixture-planner')
  assert.equal(stored.manifest.provider, 'fixture')
  assert.equal(stored.manifest.model, 'fixture-coder')
  assert.match(stored.manifest.rolloutPath, /fixture-rollouts\/dsh-fixture-planner\.jsonl$/u)
  assert.deepEqual(stored.events.map(event => event.kind), [
    'turn.started',
    'plan.updated',
    'turn.completed',
  ])
  assert.deepEqual(stored.events.map(event => event.source.roleId), [
    'planner',
    'planner',
    'planner',
  ])
  assert.equal(review.delivery.sessionBindings.some(binding => (
    binding.stageRunId === 'stage-fixture-planning'
    && binding.dshSessionId === 'dsh-fixture-planner'
    && binding.codexSessionId === 'codex-fixture-planner'
  )), true)
  assert.deepEqual(Object.keys(process.env).filter(name => (
    /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu.test(name)
    && Object.hasOwn(keylessFixtureEnvironment(), name)
  )), [])

  const source = await readFile(
    resolve(root, 'tests/fixtures/delivery-service-testkit.mjs'),
    'utf8',
  )
  assert.doesNotMatch(source, /packages\/(?:contracts|dsh-profile|native|strongflow)\/src\//u)
  assert.doesNotMatch(source, /@deepseek-ai\//u)
  const ownedRoot = kit.root
  await kit.cleanup()
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

  const reviewContext = parseStrongFlowPlanReviewContextText(review.attention.context)
  assert.equal(reviewContext.deliverySpecId, review.delivery.spec.id)
  assert.ok(reviewContext.solution.summary.length > 0)
  assert.equal(reviewContext.architectureDiagram.title, '系统架构图')
  assert.equal(reviewContext.processDiagram.title, '交付流程图')
  assert.equal(review.attention.options.some(option => (
    option.id === 'approve' && option.label === '批准执行'
  )), true)

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
  const executingDiagram = prepared.executingProjection.diagramExecution
  assert.equal(executingDiagram.state, 'executing')
  assert.equal(executingDiagram.details, null)
  assert.equal(executingDiagram.architecture.nodes.some(node => (
    node.state === 'affected-live'
  )), true)
  assert.equal(executingDiagram.process.nodes.some(node => (
    node.state === 'affected-live'
  )), true)
  assert.doesNotMatch(JSON.stringify(executingDiagram), /src\/value\.mjs/u)

  const finishedDiagram = prepared.finishedProjection.diagramExecution
  assert.equal(finishedDiagram.state, 'execution-finished')
  assert.equal(finishedDiagram.details.candidate.candidateRef, prepared.candidate.candidateRef)
  assert.equal(finishedDiagram.details.diffSha256, prepared.candidate.diffSha256)
  assert.equal(finishedDiagram.architecture.nodes.some(node => (
    node.state === 'affected-finished' && node.fileIds.length > 0
  )), true)
  assert.equal(finishedDiagram.details.files.some(file => (
    file.path === 'src/value.mjs'
  )), true)

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
