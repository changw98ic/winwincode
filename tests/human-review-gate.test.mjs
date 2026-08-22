import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  AttemptId,
  DiagramId,
  HumanReviewId,
  JobId,
  KernelSessionId,
  RequirementId,
  SolutionId,
  StageRunId,
  StrongFlowTransitionError,
  createStrongFlowJobEvent,
} from '../packages/contracts/dist/index.js'
import {
  HumanReviewGateError,
  StrongFlowHumanReviewGate,
  StrongFlowJobStore,
} from '../packages/strongflow/dist/index.js'

const systemSource = Object.freeze({ kind: 'system', actorId: 'strongflow-controller' })

function roleSource(roleId) {
  return Object.freeze({
    kind: 'role',
    actorId: roleId,
    kernelSessionId: KernelSessionId(`kernel-${roleId}`),
  })
}

function stageIdentity(name) {
  return {
    stageRunId: StageRunId(`run-${name}`),
    attemptId: AttemptId(`attempt-${name}`),
  }
}

function definition(name) {
  return Object.freeze({
    requirementId: RequirementId(`review-requirement-${name}`),
    solutionId: SolutionId(`review-solution-${name}`),
    systemArchitectureDiagramId: DiagramId(`review-architecture-${name}`),
    processFlowDiagramId: DiagramId(`review-process-${name}`),
  })
}

async function reviewFixture(t, name = 'main') {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-review-gate-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const jobId = JobId(`review-job-${name}`)
  let occurredAtMillis = 1_800_000_100_000
  const created = createStrongFlowJobEvent({
    jobId,
    sequence: '1',
    occurredAtMillis,
    source: systemSource,
    kind: 'job.created',
    data: {},
  })
  const store = await StrongFlowJobStore.create({ home, event: created })

  async function append(kind, data, source = systemSource) {
    const stored = await store.read()
    const nextSequence = (BigInt(stored.snapshot.sequence) + 1n).toString()
    const nextTime = Math.max(occurredAtMillis + 1, stored.snapshot.lastOccurredAtMillis + 1)
    const event = createStrongFlowJobEvent({
      jobId,
      sequence: nextSequence,
      occurredAtMillis: nextTime,
      source,
      kind,
      data,
    })
    const snapshot = await store.append(event)
    occurredAtMillis = nextTime
    return { event, snapshot }
  }

  async function runStage(stage, stageName, successData) {
    const identity = stageIdentity(`${name}-${stageName}`)
    const source = roleSource(stage.toLowerCase())
    await append('stage.started', { stage, ...identity }, source)
    return append('stage.succeeded', { stage, ...identity, ...successData }, source)
  }

  async function reachReview(currentDefinition) {
    await runStage('REQUIREMENTS', 'requirements', {
      requirementId: currentDefinition.requirementId,
    })
    await runStage('SOLUTION', 'solution', {
      requirementId: currentDefinition.requirementId,
      solutionId: currentDefinition.solutionId,
    })
    await runStage('DIAGRAMS', 'diagrams', { definition: currentDefinition })
  }

  return {
    append,
    home,
    jobId,
    reachReview,
    runStage,
    store,
  }
}

function authenticator(calls, acceptedToken = 'valid-token') {
  return {
    async authenticate(request) {
      calls.push(request)
      if (request.authentication !== acceptedToken) return undefined
      return { reviewerId: 'reviewer-authenticated' }
    },
  }
}

function gate(fixture, calls, id = 'review-gate-1') {
  return new StrongFlowHumanReviewGate({
    store: fixture.store,
    authenticator: authenticator(calls),
    clock: () => 1_800_000_200_000,
    reviewIdFactory: () => HumanReviewId(id),
  })
}

function submission(decision, currentDefinition, extra = {}) {
  return {
    decision,
    channel: 'local-ui',
    authentication: 'valid-token',
    definition: currentDefinition,
    ...extra,
  }
}

function expectGateError(code) {
  return error => error instanceof HumanReviewGateError && error.code === code
}

test('waits without model work, then persists authenticated approval before resuming', async t => {
  const fixture = await reviewFixture(t, 'approval')
  const currentDefinition = definition('approval-v1')
  await fixture.reachReview(currentDefinition)
  const authenticationCalls = []
  const reviewGate = gate(fixture, authenticationCalls)

  let settled = false
  let modelCalls = 0
  const waiting = reviewGate.waitForDecision().then(receipt => {
    settled = true
    return receipt
  })
  await new Promise(resolve => setImmediate(resolve))
  assert.equal(settled, false)
  assert.equal(modelCalls, 0)

  const submitted = await reviewGate.submit(submission('approved', currentDefinition, {
    comment: 'The exact definition may proceed.',
  }))
  const resumed = await waiting
  assert.deepEqual(resumed, submitted)
  assert.equal(submitted.snapshot.state, 'PLANNING')
  assert.deepEqual(submitted.decision, {
    schemaVersion: 1,
    artifactKind: 'HUMAN_REVIEW_RECORD',
    artifactId: HumanReviewId('review-gate-1'),
    jobId: fixture.jobId,
    sourceArtifacts: [
      { artifactKind: 'REQUIREMENT_SPEC', artifactId: currentDefinition.requirementId },
      { artifactKind: 'SOLUTION_DESIGN', artifactId: currentDefinition.solutionId },
      {
        artifactKind: 'SYSTEM_ARCHITECTURE_DIAGRAM',
        artifactId: currentDefinition.systemArchitectureDiagramId,
      },
      {
        artifactKind: 'PROCESS_FLOW_DIAGRAM',
        artifactId: currentDefinition.processFlowDiagramId,
      },
    ],
    producer: {
      kind: 'human',
      actorId: 'reviewer-authenticated',
      channel: 'local-ui',
    },
    kernelEventInterval: null,
    createdAtMillis: 1_800_000_200_000,
    payload: {
      decision: 'approved',
      definition: currentDefinition,
      scope: null,
      comment: 'The exact definition may proceed.',
    },
  })
  assert.deepEqual(authenticationCalls, [{
    channel: 'local-ui',
    authentication: 'valid-token',
  }])
  assert.equal(modelCalls, 0)

  const reopened = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
  const replayed = await reopened.read()
  assert.deepEqual(replayed.snapshot.lastHumanReview, submitted.decision)
  assert.equal(replayed.snapshot.approval.artifactId, submitted.decision.artifactId)
  assert.doesNotMatch(JSON.stringify(replayed.events), /valid-token/u)
})

test('rejects missing authentication, role channels, and model-authored decisions', async t => {
  const fixture = await reviewFixture(t, 'authentication')
  const currentDefinition = definition('authentication-v1')
  await fixture.reachReview(currentDefinition)
  const authenticationCalls = []
  const reviewGate = gate(fixture, authenticationCalls)

  await assert.rejects(
    reviewGate.submit({
      ...submission('approved', currentDefinition),
      authentication: 'wrong-token',
    }),
    expectGateError('AUTHENTICATION_REQUIRED'),
  )
  await assert.rejects(
    reviewGate.submit({
      ...submission('approved', currentDefinition),
      channel: 'role',
    }),
    expectGateError('INVALID_REVIEW_REQUEST'),
  )

  const stored = await fixture.store.read()
  const modelEvent = createStrongFlowJobEvent({
    jobId: fixture.jobId,
    sequence: (BigInt(stored.snapshot.sequence) + 1n).toString(),
    occurredAtMillis: stored.snapshot.lastOccurredAtMillis + 1,
    source: roleSource('requirements'),
    kind: 'human-review.approved',
    data: {
      reviewId: HumanReviewId('model-review'),
      reviewerId: 'requirements',
      definition: currentDefinition,
    },
  })
  await assert.rejects(
    fixture.store.append(modelEvent),
    error => error instanceof StrongFlowTransitionError && error.code === 'INVALID_EVENT',
  )
  assert.equal((await fixture.store.read()).snapshot.state, 'AWAITING_HUMAN_REVIEW')
  assert.equal(authenticationCalls.length, 1)
})

test('request-changes invalidates approval inputs and reroutes the named definition stage', async t => {
  for (const [scope, state, expectedDefinition] of [
    ['requirements', 'DEFINING_REQUIREMENTS', {}],
    ['solution', 'DEFINING_SOLUTION', { requirementId: 'keep' }],
    ['diagrams', 'DEFINING_DIAGRAMS', { requirementId: 'keep', solutionId: 'keep' }],
  ]) {
    const fixture = await reviewFixture(t, `changes-${scope}`)
    const currentDefinition = definition(`changes-${scope}-v1`)
    await fixture.reachReview(currentDefinition)
    const reviewGate = gate(fixture, [], `review-changes-${scope}`)
    const receipt = await reviewGate.submit(submission(
      'changes-requested',
      currentDefinition,
      { scope, comment: `Revise ${scope}.` },
    ))

    assert.equal(receipt.snapshot.state, state)
    assert.equal(receipt.snapshot.definitionRevision, 2)
    assert.equal(receipt.snapshot.approval, undefined)
    assert.equal(receipt.decision.payload.decision, 'changes-requested')
    assert.equal(receipt.decision.payload.scope, scope)
    assert.deepEqual(receipt.snapshot.definition, {
      ...(expectedDefinition.requirementId === undefined
        ? {}
        : { requirementId: currentDefinition.requirementId }),
      ...(expectedDefinition.solutionId === undefined
        ? {}
        : { solutionId: currentDefinition.solutionId }),
    })
  }
})

test('a changed diagram set requires a new human decision', async t => {
  const fixture = await reviewFixture(t, 'stale')
  const firstDefinition = definition('stale-v1')
  await fixture.reachReview(firstDefinition)
  const firstGate = gate(fixture, [], 'review-change-diagrams')
  await firstGate.submit(submission('changes-requested', firstDefinition, {
    scope: 'diagrams',
  }))

  const secondDefinition = Object.freeze({
    ...firstDefinition,
    systemArchitectureDiagramId: DiagramId('review-architecture-stale-v2'),
    processFlowDiagramId: DiagramId('review-process-stale-v2'),
  })
  await fixture.runStage('DIAGRAMS', 'diagrams-v2', { definition: secondDefinition })
  const secondGate = gate(fixture, [], 'review-stale-v2')
  await assert.rejects(
    secondGate.submit(submission('approved', firstDefinition)),
    expectGateError('STALE_DEFINITION'),
  )
  const approved = await secondGate.submit(submission('approved', secondDefinition))
  assert.equal(approved.snapshot.state, 'PLANNING')
  assert.deepEqual(approved.snapshot.approval.payload.definition, secondDefinition)
})

test('reject is terminal and a pending definition accepts only one human decision', async t => {
  const rejectedFixture = await reviewFixture(t, 'rejected')
  const rejectedDefinition = definition('rejected-v1')
  await rejectedFixture.reachReview(rejectedDefinition)
  const rejectedGate = gate(rejectedFixture, [], 'review-rejected')
  const rejected = await rejectedGate.submit({
    ...submission('rejected', rejectedDefinition, {
      comment: 'The definition is not accepted.',
    }),
    channel: 'cli',
  })
  assert.equal(rejected.snapshot.state, 'REJECTED')
  assert.equal(rejected.decision.payload.decision, 'rejected')
  assert.equal(rejected.decision.producer.channel, 'cli')

  const fixture = await reviewFixture(t, 'single-use')
  const currentDefinition = definition('single-use-v1')
  await fixture.reachReview(currentDefinition)
  let reviewNumber = 0
  const reviewGate = new StrongFlowHumanReviewGate({
    store: fixture.store,
    authenticator: authenticator([]),
    clock: () => 1_800_000_200_000,
    reviewIdFactory: () => HumanReviewId(`review-single-${++reviewNumber}`),
  })
  const results = await Promise.allSettled([
    reviewGate.submit(submission('approved', currentDefinition)),
    reviewGate.submit(submission('approved', currentDefinition)),
  ])
  assert.equal(results.filter(result => result.status === 'fulfilled').length, 1)
  const failed = results.find(result => result.status === 'rejected')
  assert.ok(failed)
  assert.ok(expectGateError('REVIEW_ALREADY_DECIDED')(failed.reason))
  assert.equal((await fixture.store.read()).events.length, 8)
})

test('an aborted wait releases its listener without changing durable state', async t => {
  const fixture = await reviewFixture(t, 'abort')
  const currentDefinition = definition('abort-v1')
  await fixture.reachReview(currentDefinition)
  const reviewGate = gate(fixture, [])
  const controller = new AbortController()
  const waiting = reviewGate.waitForDecision(controller.signal)
  controller.abort()
  await assert.rejects(waiting, error => error instanceof Error && error.name === 'AbortError')
  assert.equal((await fixture.store.read()).snapshot.state, 'AWAITING_HUMAN_REVIEW')
})
