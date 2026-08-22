import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  AttemptId,
  CandidateId,
  DiagramId,
  HumanReviewId,
  JobId,
  KernelSessionId,
  RequirementId,
  SolutionId,
  StageRunId,
  createStrongFlowJobEvent,
} from '../packages/contracts/dist/index.js'
import {
  StrongFlowController,
  StrongFlowControllerError,
  StrongFlowHumanReviewGate,
  StrongFlowJobStore,
  StrongFlowStageProviderFailure,
} from '../packages/strongflow/dist/index.js'

const STAGES = [
  'REQUIREMENTS',
  'SOLUTION',
  'DIAGRAMS',
  'PLANNING',
  'EXECUTION',
  'VERIFICATION',
  'REMEDIATION',
  'DELIVERY',
]

function expectedControllerError(code) {
  return error => error instanceof StrongFlowControllerError && error.code === code
}

async function controllerFixture(t, name, options = {}) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-controller-'))
  t.after(() => rm(home, { recursive: true, force: true }))

  const jobId = JobId(`controller-job-${name}`)
  let now = 1_900_000_000_000
  const created = createStrongFlowJobEvent({
    jobId,
    sequence: '1',
    occurredAtMillis: now,
    source: { kind: 'system', actorId: 'fixture' },
    kind: 'job.created',
    data: { title: `Controller fixture ${name}` },
  })
  const store = await StrongFlowJobStore.create({ home, event: created })
  const providerCalls = []
  const completionGateCalls = []
  const completionGateOutcomes = [...(options.completionGateOutcomes ?? ['passed'])]
  let stageRunCounter = 0
  let attemptCounter = 0
  let reviewCounter = 0

  function defaultOutput(stage, context) {
    const revision = context.snapshot.definitionRevision
    const requirementId = context.snapshot.definition.requirementId
      ?? RequirementId(`requirement-${name}-r${revision}`)
    const solutionId = context.snapshot.definition.solutionId
      ?? SolutionId(`solution-${name}-r${revision}`)
    const candidateId = context.snapshot.candidateId
      ?? CandidateId(`candidate-${name}-r${revision}`)

    switch (stage) {
      case 'REQUIREMENTS':
        return { requirementId }
      case 'SOLUTION':
        return { requirementId, solutionId }
      case 'DIAGRAMS':
        return {
          definition: {
            requirementId,
            solutionId,
            systemArchitectureDiagramId: DiagramId(`architecture-${name}-r${revision}`),
            processFlowDiagramId: DiagramId(`process-${name}-r${revision}`),
          },
        }
      case 'PLANNING':
        return {}
      case 'EXECUTION':
        return { candidateId }
      case 'VERIFICATION':
        return { candidateId, outcome: 'passed' }
      case 'REMEDIATION':
      case 'DELIVERY':
        return { candidateId }
      default:
        assert.fail(`unexpected stage ${stage}`)
    }
  }

  const providers = STAGES.map(stage => ({
    stage,
    roleId: `role-${stage.toLowerCase()}`,
    async run(context) {
      providerCalls.push({ stage, context })
      const override = options.providerRuns?.[stage]
      if (override !== undefined) return override(context)
      return {
        output: defaultOutput(stage, context),
        kernelSessionId: KernelSessionId(
          `kernel-${stage.toLowerCase()}-${context.attemptId}`,
        ),
      }
    },
  }))

  const completionGate = {
    authority: 'program',
    async evaluate(context) {
      completionGateCalls.push(context)
      const outcome = completionGateOutcomes.shift() ?? 'passed'
      return outcome === 'passed'
        ? { outcome: 'passed' }
        : { outcome: 'failed', reason: String(outcome.reason ?? outcome) }
    },
  }

  function controller(controllerName = 'main', selectedStore = store) {
    return new StrongFlowController({
      store: selectedStore,
      providers,
      completionGate,
      controllerId: `controller-${name}-${controllerName}`,
      clock: () => ++now,
      stageRunIdFactory: operation => {
        stageRunCounter += 1
        return StageRunId(
          `run-${name}-${operation.toLowerCase()}-${stageRunCounter}`,
        )
      },
      attemptIdFactory: stage => {
        attemptCounter += 1
        return AttemptId(`attempt-${name}-${stage.toLowerCase()}-${attemptCounter}`)
      },
    })
  }

  async function submitReview(decision, definition, extra = {}) {
    reviewCounter += 1
    const gate = new StrongFlowHumanReviewGate({
      store,
      authenticator: {
        async authenticate(request) {
          assert.deepEqual(request, {
            channel: 'local-ui',
            authentication: 'fixture-authentication',
          })
          return { reviewerId: 'fixture-reviewer' }
        },
      },
      clock: () => ++now,
      reviewIdFactory: () => HumanReviewId(`review-${name}-${reviewCounter}`),
    })
    return gate.submit({
      decision,
      channel: 'local-ui',
      authentication: 'fixture-authentication',
      definition,
      ...extra,
    })
  }

  return {
    completionGateCalls,
    controller,
    home,
    jobId,
    providerCalls,
    store,
    submitReview,
  }
}

test('runs the fixed stage order, pauses without model work, and resumes from stored approval',
  async t => {
    const fixture = await controllerFixture(t, 'delivery')
    const definingController = fixture.controller('definition')

    const waiting = await definingController.runUntilPause({ maxTransitions: 3 })
    assert.equal(waiting.transitions, 3)
    assert.equal(waiting.result.kind, 'waiting-for-human-review')
    assert.deepEqual(
      fixture.providerCalls.map(call => call.stage),
      ['REQUIREMENTS', 'SOLUTION', 'DIAGRAMS'],
    )

    const eventCountAtPause = (await fixture.store.read()).events.length
    assert.equal((await definingController.advance()).kind, 'waiting-for-human-review')
    assert.equal((await definingController.advance()).kind, 'waiting-for-human-review')
    assert.equal(fixture.providerCalls.length, 3)
    assert.equal((await fixture.store.read()).events.length, eventCountAtPause)
    assert.equal(fixture.completionGateCalls.length, 0)

    const definition = waiting.result.snapshot.definition
    await fixture.submitReview('approved', definition)
    const reopened = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
    const executionController = fixture.controller('reopened', reopened)
    const delivered = await executionController.runUntilPause()

    assert.equal(delivered.transitions, 6)
    assert.equal(delivered.result.kind, 'delivered')
    assert.equal(delivered.result.snapshot.state, 'DELIVERED')
    assert.deepEqual(
      fixture.providerCalls.map(call => call.stage),
      [
        'REQUIREMENTS',
        'SOLUTION',
        'DIAGRAMS',
        'PLANNING',
        'EXECUTION',
        'VERIFICATION',
        'DELIVERY',
      ],
    )
    assert.equal(fixture.completionGateCalls.length, 1)

    const replayed = await reopened.read()
    assert.deepEqual(
      replayed.events
        .filter(event => event.kind === 'stage.succeeded')
        .map(event => event.data.stage),
      [
        'REQUIREMENTS',
        'SOLUTION',
        'DIAGRAMS',
        'PLANNING',
        'EXECUTION',
        'VERIFICATION',
        'DELIVERY',
      ],
    )
    assert.equal(replayed.snapshot.approval.producer.actorId, 'fixture-reviewer')
  })

test('a requested solution revision reruns only solution and both diagrams', async t => {
  const fixture = await controllerFixture(t, 'revision')
  const controller = fixture.controller()
  const firstPause = await controller.runUntilPause({ maxTransitions: 3 })
  const firstDefinition = firstPause.result.snapshot.definition

  const revision = await fixture.submitReview(
    'changes-requested',
    firstDefinition,
    { scope: 'solution', comment: 'Revise the solution boundary.' },
  )
  assert.equal(revision.snapshot.state, 'DEFINING_SOLUTION')
  assert.equal(revision.snapshot.definition.requirementId, firstDefinition.requirementId)
  assert.equal(revision.snapshot.definition.solutionId, undefined)

  const secondPause = await controller.runUntilPause({ maxTransitions: 2 })
  assert.equal(secondPause.transitions, 2)
  assert.equal(secondPause.result.kind, 'waiting-for-human-review')
  assert.deepEqual(
    fixture.providerCalls.map(call => call.stage),
    ['REQUIREMENTS', 'SOLUTION', 'DIAGRAMS', 'SOLUTION', 'DIAGRAMS'],
  )
  assert.equal(
    secondPause.result.snapshot.definition.requirementId,
    firstDefinition.requirementId,
  )
  assert.notEqual(
    secondPause.result.snapshot.definition.solutionId,
    firstDefinition.solutionId,
  )
})

test('two controllers publish one stage start and invoke one provider', async t => {
  let enterProvider
  let releaseProvider
  const entered = new Promise(resolve => {
    enterProvider = resolve
  })
  const released = new Promise(resolve => {
    releaseProvider = resolve
  })
  const fixture = await controllerFixture(t, 'competition', {
    providerRuns: {
      async REQUIREMENTS(context) {
        enterProvider()
        await released
        return {
          output: { requirementId: RequirementId('requirement-competition') },
          kernelSessionId: KernelSessionId(`kernel-${context.attemptId}`),
        }
      },
    },
  })
  const first = fixture.controller('first')
  const second = fixture.controller('second')

  const outcomesPromise = Promise.allSettled([first.advance(), second.advance()])
  await entered
  releaseProvider()
  const outcomes = await outcomesPromise

  assert.equal(
    fixture.providerCalls.filter(call => call.stage === 'REQUIREMENTS').length,
    1,
  )
  const stored = await fixture.store.read()
  assert.equal(
    stored.events.filter(event => event.kind === 'stage.started').length,
    1,
  )
  assert.equal(
    stored.events.filter(event => event.kind === 'stage.succeeded').length,
    1,
  )
  assert.equal(
    outcomes.filter(outcome => (
      outcome.status === 'fulfilled' && outcome.value.kind === 'stage-succeeded'
    )).length,
    1,
  )
  const observer = outcomes.find(outcome => !(
    outcome.status === 'fulfilled' && outcome.value.kind === 'stage-succeeded'
  ))
  assert.ok(observer)
  if (observer.status === 'fulfilled') {
    assert.equal(observer.value.kind, 'active-stage')
  } else {
    assert.ok(expectedControllerError('CONTROLLER_CONFLICT')(observer.reason))
  }
})

test('a typed provider failure becomes terminal failure without a success event', async t => {
  const fixture = await controllerFixture(t, 'provider-failure', {
    providerRuns: {
      async REQUIREMENTS() {
        throw new StrongFlowStageProviderFailure({
          category: 'task',
          code: 'REQUIREMENT_INVALID',
          message: 'The requirement could not be validated.',
          retryable: false,
        })
      },
    },
  })

  const result = await fixture.controller().advance()
  assert.equal(result.kind, 'stage-failed')
  assert.equal(result.snapshot.state, 'FAILED')
  assert.deepEqual(result.snapshot.lastStop, {
    kind: 'task-failure',
    occurredAtMillis: result.snapshot.lastOccurredAtMillis,
    message: 'The requirement could not be validated.',
    code: 'REQUIREMENT_INVALID',
    retryable: false,
    stage: 'REQUIREMENTS',
    stageRunId: result.snapshot.lastStop.stageRunId,
  })
  const stored = await fixture.store.read()
  assert.equal(stored.events.some(event => event.kind === 'stage.succeeded'), false)
})

test('a provider cannot choose workflow state through its result', async t => {
  const fixture = await controllerFixture(t, 'model-state', {
    providerRuns: {
      async REQUIREMENTS() {
        return {
          output: {
            requirementId: RequirementId('requirement-model-state'),
            nextState: 'DELIVERED',
          },
        }
      },
    },
  })

  const result = await fixture.controller().advance()
  assert.equal(result.kind, 'stage-failed')
  assert.equal(result.snapshot.state, 'FAILED')
  assert.equal(result.snapshot.lastStop.code, 'INVALID_STAGE_RESULT')
  assert.equal(result.snapshot.candidateId, undefined)
  assert.equal(
    (await fixture.store.read()).events.some(event => event.kind === 'stage.succeeded'),
    false,
  )
})

test('cancellation interrupts an active provider and cannot record stage success', async t => {
  let enterProvider
  const entered = new Promise(resolve => {
    enterProvider = resolve
  })
  const fixture = await controllerFixture(t, 'cancellation', {
    providerRuns: {
      REQUIREMENTS(context) {
        enterProvider()
        return new Promise((resolve, reject) => {
          context.signal.addEventListener(
            'abort',
            () => reject(new Error('fixture provider aborted')),
            { once: true },
          )
        })
      },
    },
  })
  const controller = fixture.controller()

  const running = controller.advance()
  await entered
  const cancelling = controller.cancel('The operator cancelled the job.')
  const [interrupted, cancelled] = await Promise.all([running, cancelling])

  assert.equal(interrupted.kind, 'interrupted')
  assert.equal(interrupted.snapshot.state, 'INTERRUPTED')
  assert.equal(cancelled.state, 'CANCELLED')
  const stored = await fixture.store.read()
  assert.deepEqual(
    stored.events.map(event => event.kind),
    ['job.created', 'stage.started', 'job.interrupted', 'job.cancelled'],
  )
})

test('a failed program gate remediates and re-verifies before delivery', async t => {
  const fixture = await controllerFixture(t, 'remediation', {
    completionGateOutcomes: [{ reason: 'Required evidence is missing.' }, 'passed'],
  })
  const controller = fixture.controller()
  const waiting = await controller.runUntilPause({ maxTransitions: 3 })
  await fixture.submitReview('approved', waiting.result.snapshot.definition)

  const delivered = await controller.runUntilPause()
  assert.equal(delivered.result.kind, 'delivered')
  assert.equal(delivered.result.snapshot.state, 'DELIVERED')
  assert.equal(fixture.completionGateCalls.length, 2)
  assert.deepEqual(
    fixture.providerCalls.map(call => call.stage),
    [
      'REQUIREMENTS',
      'SOLUTION',
      'DIAGRAMS',
      'PLANNING',
      'EXECUTION',
      'VERIFICATION',
      'REMEDIATION',
      'VERIFICATION',
      'DELIVERY',
    ],
  )
})

test('the transition limit stops before extra work but returns an exact-limit human pause',
  async t => {
    const fixture = await controllerFixture(t, 'transition-limit')
    const controller = fixture.controller()

    await assert.rejects(
      controller.runUntilPause({ maxTransitions: 2 }),
      expectedControllerError('STEP_LIMIT_REACHED'),
    )
    assert.equal((await fixture.store.read()).snapshot.state, 'DEFINING_DIAGRAMS')
    assert.deepEqual(
      fixture.providerCalls.map(call => call.stage),
      ['REQUIREMENTS', 'SOLUTION'],
    )

    const waiting = await controller.runUntilPause({ maxTransitions: 1 })
    assert.equal(waiting.transitions, 1)
    assert.equal(waiting.result.kind, 'waiting-for-human-review')
    assert.equal(waiting.result.snapshot.state, 'AWAITING_HUMAN_REVIEW')
    assert.deepEqual(
      fixture.providerCalls.map(call => call.stage),
      ['REQUIREMENTS', 'SOLUTION', 'DIAGRAMS'],
    )
  })

test('constructor rejects non-program gates and incomplete provider rosters', async t => {
  const fixture = await controllerFixture(t, 'invalid-options')
  const completeController = fixture.controller()
  assert.ok(completeController instanceof StrongFlowController)

  assert.throws(
    () => new StrongFlowController({
      store: fixture.store,
      providers: [],
      completionGate: { authority: 'program', async evaluate() {} },
    }),
    expectedControllerError('MISSING_STAGE_PROVIDER'),
  )
  assert.throws(
    () => new StrongFlowController({
      store: fixture.store,
      providers: STAGES.map(stage => ({
        stage,
        roleId: `role-${stage.toLowerCase()}`,
        async run() {},
      })),
      completionGate: { authority: 'model', async evaluate() {} },
    }),
    expectedControllerError('INVALID_CONTROLLER_OPTIONS'),
  )
})
