import assert from 'node:assert/strict'
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
  STRONGFLOW_JOB_TRANSITIONS,
  StrongFlowTransitionError,
  applyStrongFlowJobEvent,
  assertStrongFlowJobEvent,
  createStrongFlowJobEvent,
  projectStrongFlowJob,
} from '../packages/contracts/dist/index.js'

const systemSource = Object.freeze({ kind: 'system', actorId: 'strongflow-controller' })
const humanSource = Object.freeze({
  kind: 'human',
  actorId: 'reviewer-1',
  channel: 'local-ui',
})

function roleSource(roleId) {
  return Object.freeze({
    kind: 'role',
    actorId: roleId,
    kernelSessionId: KernelSessionId(`kernel-${roleId}`),
  })
}

function jobFixture(name = 'main') {
  const jobId = JobId(`job-${name}`)
  const events = []
  let sequence = 0
  let occurredAtMillis = 1_800_000_000_000
  let snapshot

  function emit(kind, data, source = systemSource) {
    const nextSequence = sequence + 1
    const nextOccurredAtMillis = occurredAtMillis + 1
    const event = createStrongFlowJobEvent({
      jobId,
      sequence: String(nextSequence),
      occurredAtMillis: nextOccurredAtMillis,
      source,
      kind,
      data,
    })
    const nextSnapshot = applyStrongFlowJobEvent(snapshot, event)
    sequence = nextSequence
    occurredAtMillis = nextOccurredAtMillis
    snapshot = nextSnapshot
    events.push(event)
    return event
  }

  function current() {
    assert.ok(snapshot)
    return snapshot
  }

  return { emit, events, current, jobId }
}

function definition(name = 'v1') {
  return Object.freeze({
    requirementId: RequirementId(`requirement-${name}`),
    solutionId: SolutionId(`solution-${name}`),
    systemArchitectureDiagramId: DiagramId(`architecture-${name}`),
    processFlowDiagramId: DiagramId(`process-${name}`),
  })
}

function stageIdentity(name) {
  return {
    stageRunId: StageRunId(`run-${name}`),
    attemptId: AttemptId(`attempt-${name}`),
  }
}

function runStage(machine, stage, name, successData = {}, roleId = stage.toLowerCase()) {
  const identity = stageIdentity(name)
  const source = roleSource(roleId)
  machine.emit('stage.started', { stage, ...identity }, source)
  machine.emit('stage.succeeded', { stage, ...identity, ...successData }, source)
  return identity
}

function reachHumanReview(machine, currentDefinition = definition()) {
  machine.emit('job.created', { title: 'Fixture delivery' })
  runStage(machine, 'REQUIREMENTS', 'requirements', {
    requirementId: currentDefinition.requirementId,
  })
  runStage(machine, 'SOLUTION', 'solution', {
    requirementId: currentDefinition.requirementId,
    solutionId: currentDefinition.solutionId,
  })
  runStage(machine, 'DIAGRAMS', 'diagrams', { definition: currentDefinition })
  return currentDefinition
}

function approve(machine, currentDefinition, name = 'v1') {
  machine.emit('human-review.approved', {
    reviewId: HumanReviewId(`review-${name}`),
    reviewerId: humanSource.actorId,
    definition: currentDefinition,
    comment: 'Definition approved.',
  }, humanSource)
}

function expectTransitionError(callback, code) {
  assert.throws(
    callback,
    error => error instanceof StrongFlowTransitionError && error.code === code,
  )
}

test('projects the complete approved delivery path and replays it exactly', () => {
  const machine = jobFixture('delivery')
  const currentDefinition = reachHumanReview(machine)
  approve(machine, currentDefinition)

  runStage(machine, 'PLANNING', 'planning')
  const candidateId = CandidateId('candidate-1')
  runStage(machine, 'EXECUTION', 'execution', { candidateId }, 'implementation')
  runStage(machine, 'VERIFICATION', 'verification', {
    candidateId,
    outcome: 'remediation-required',
  }, 'verification')
  runStage(machine, 'REMEDIATION', 'remediation', { candidateId }, 'remediation')
  runStage(machine, 'VERIFICATION', 'verification-2', {
    candidateId,
    outcome: 'passed',
  }, 'verification')

  machine.emit('completion-gate.passed', {
    stageRunId: StageRunId('gate-1'),
    candidateId,
  })
  runStage(machine, 'DELIVERY', 'delivery', { candidateId }, 'delivery')
  machine.emit('job.delivered', { candidateId })

  const snapshot = machine.current()
  assert.equal(snapshot.state, 'DELIVERED')
  assert.equal(snapshot.candidateId, candidateId)
  assert.equal(snapshot.approval.payload.decision, 'approved')
  assert.deepEqual(snapshot.approval.payload.definition, currentDefinition)
  assert.equal(snapshot.completionGate.outcome, 'passed')
  assert.equal(snapshot.deliveredAtMillis, machine.events.at(-1).occurredAtMillis)
  assert.deepEqual(projectStrongFlowJob(machine.events), snapshot)
  assert.deepEqual(JSON.parse(JSON.stringify(machine.events)), machine.events)
  assert.ok(Object.isFrozen(snapshot))
  assert.ok(Object.isFrozen(snapshot.definition))
})

test('planning and execution cannot start before a current human approval', () => {
  const machine = jobFixture('approval-gate')
  reachHumanReview(machine)
  const planning = stageIdentity('planning-without-approval')
  const event = createStrongFlowJobEvent({
    jobId: machine.jobId,
    sequence: String(machine.events.length + 1),
    occurredAtMillis: machine.events.at(-1).occurredAtMillis + 1,
    source: roleSource('planning'),
    kind: 'stage.started',
    data: { stage: 'PLANNING', ...planning },
  })

  expectTransitionError(
    () => applyStrongFlowJobEvent(machine.current(), event),
    'ILLEGAL_TRANSITION',
  )

  const approvedMachine = jobFixture('stale-approval')
  const approvedDefinition = reachHumanReview(approvedMachine)
  approve(approvedMachine, approvedDefinition)
  const staleSnapshot = Object.freeze({
    ...approvedMachine.current(),
    definition: Object.freeze({
      ...approvedMachine.current().definition,
      solutionId: SolutionId('solution-changed-out-of-band'),
    }),
  })
  const stalePlanning = stageIdentity('planning-stale-approval')
  const staleEvent = createStrongFlowJobEvent({
    jobId: approvedMachine.jobId,
    sequence: String(approvedMachine.events.length + 1),
    occurredAtMillis: approvedMachine.events.at(-1).occurredAtMillis + 1,
    source: roleSource('planning'),
    kind: 'stage.started',
    data: { stage: 'PLANNING', ...stalePlanning },
  })
  expectTransitionError(
    () => applyStrongFlowJobEvent(staleSnapshot, staleEvent),
    'APPROVAL_REQUIRED',
  )
})

test('a requested revision invalidates approval and stale definition decisions', () => {
  const machine = jobFixture('revision')
  const firstDefinition = reachHumanReview(machine, definition('v1'))
  approve(machine, firstDefinition)

  machine.emit('human-review.changes-requested', {
    reviewId: HumanReviewId('review-change-solution'),
    reviewerId: humanSource.actorId,
    definition: firstDefinition,
    scope: 'solution',
    comment: 'Use a different solution boundary.',
  }, humanSource)

  assert.equal(machine.current().state, 'DEFINING_SOLUTION')
  assert.equal(machine.current().definitionRevision, 2)
  assert.deepEqual(machine.current().definition, {
    requirementId: firstDefinition.requirementId,
  })
  assert.equal(machine.current().approval, undefined)
  assert.equal(machine.current().candidateId, undefined)

  const secondDefinition = definition('v2')
  runStage(machine, 'SOLUTION', 'solution-v2', {
    requirementId: firstDefinition.requirementId,
    solutionId: secondDefinition.solutionId,
  })
  const revisedDefinition = Object.freeze({
    ...secondDefinition,
    requirementId: firstDefinition.requirementId,
  })
  runStage(machine, 'DIAGRAMS', 'diagrams-v2', { definition: revisedDefinition })

  expectTransitionError(
    () => machine.emit('human-review.approved', {
      reviewId: HumanReviewId('review-stale'),
      reviewerId: humanSource.actorId,
      definition: firstDefinition,
    }, humanSource),
    'STALE_DEFINITION',
  )
  assert.equal(machine.events.at(-1).kind, 'stage.succeeded')

  approve(machine, revisedDefinition, 'v2')
  assert.equal(machine.current().state, 'PLANNING')
  assert.deepEqual(machine.current().approval.payload.definition, revisedDefinition)
})

test('terminal delivery cannot be reached without the completion-gate event', () => {
  const machine = jobFixture('completion-gate')
  const currentDefinition = reachHumanReview(machine)
  approve(machine, currentDefinition)
  runStage(machine, 'PLANNING', 'planning')
  const candidateId = CandidateId('candidate-gate')
  runStage(machine, 'EXECUTION', 'execution', { candidateId })
  runStage(machine, 'VERIFICATION', 'verification', {
    candidateId,
    outcome: 'passed',
  })

  expectTransitionError(
    () => machine.emit('job.delivered', { candidateId }),
    'ILLEGAL_TRANSITION',
  )
  assert.equal(machine.current().state, 'AWAITING_COMPLETION_GATE')
  assert.equal(machine.events.at(-1).kind, 'stage.succeeded')

  machine.emit('completion-gate.failed', {
    stageRunId: StageRunId('gate-failed'),
    candidateId,
    reason: 'Acceptance probe failed.',
  })
  assert.equal(machine.current().state, 'REMEDIATING')
  assert.equal(machine.current().completionGate.outcome, 'failed')
})

test('interruption, rejection, cancellation, and failure categories remain distinct', () => {
  const interrupted = jobFixture('interrupted')
  interrupted.emit('job.created', {})
  const running = stageIdentity('running-requirements')
  const source = roleSource('requirements')
  interrupted.emit('stage.started', { stage: 'REQUIREMENTS', ...running }, source)
  const interruptEvent = interrupted.emit('job.interrupted', {
    reason: 'Host is shutting down.',
    stageRunId: running.stageRunId,
  })
  assert.equal(interrupted.current().state, 'INTERRUPTED')
  assert.equal(interrupted.current().lastStop.kind, 'interruption')
  assert.equal(interrupted.current().activeStage, undefined)
  interrupted.emit('job.resumed', { interruptionSequence: interruptEvent.sequence })
  assert.equal(interrupted.current().state, 'DEFINING_REQUIREMENTS')
  assert.equal(interrupted.current().interruption, undefined)

  const rejected = jobFixture('rejected')
  const rejectedDefinition = reachHumanReview(rejected)
  rejected.emit('human-review.rejected', {
    reviewId: HumanReviewId('review-rejected'),
    reviewerId: humanSource.actorId,
    definition: rejectedDefinition,
    comment: 'Definition is not accepted.',
  }, humanSource)
  assert.equal(rejected.current().state, 'REJECTED')
  assert.equal(rejected.current().lastStop.kind, 'human-rejection')

  const cancelled = jobFixture('cancelled')
  cancelled.emit('job.created', {})
  cancelled.emit('job.cancelled', { reason: 'Cancelled by operator.' }, humanSource)
  assert.equal(cancelled.current().state, 'CANCELLED')
  assert.equal(cancelled.current().lastStop.kind, 'cancellation')

  for (const category of ['task', 'infrastructure']) {
    const failed = jobFixture(`failed-${category}`)
    failed.emit('job.created', {})
    const failedRun = stageIdentity(`failed-${category}`)
    const failedSource = roleSource('requirements')
    failed.emit('stage.started', {
      stage: 'REQUIREMENTS',
      ...failedRun,
    }, failedSource)
    failed.emit('stage.failed', {
      stage: 'REQUIREMENTS',
      ...failedRun,
      category,
      code: `${category.toUpperCase()}_FAILURE`,
      message: `${category} failure fixture`,
      retryable: category === 'infrastructure',
    }, failedSource)
    assert.equal(failed.current().state, 'FAILED')
    assert.equal(
      failed.current().lastStop.kind,
      category === 'task' ? 'task-failure' : 'infrastructure-failure',
    )
  }
})

test('event validation rejects lossy JSON, unknown fields, and sequence gaps', () => {
  const jobId = JobId('job-validation')
  const base = createStrongFlowJobEvent({
    jobId,
    sequence: '1',
    occurredAtMillis: 1,
    source: systemSource,
    kind: 'job.created',
    data: {},
  })

  for (const invalid of [
    { ...base, schemaVersion: 2 },
    { ...base, extra: true },
    { ...base, occurredAtMillis: Number.NaN },
    { ...base, data: { title: undefined } },
    { ...base, data: { title: new Date(0) } },
  ]) {
    expectTransitionError(() => assertStrongFlowJobEvent(invalid), 'INVALID_EVENT')
  }

  const created = applyStrongFlowJobEvent(undefined, base)
  const gap = createStrongFlowJobEvent({
    jobId,
    sequence: '3',
    occurredAtMillis: 2,
    source: roleSource('requirements'),
    kind: 'stage.started',
    data: {
      stage: 'REQUIREMENTS',
      stageRunId: StageRunId('run-gap'),
      attemptId: AttemptId('attempt-gap'),
    },
  })
  expectTransitionError(() => applyStrongFlowJobEvent(created, gap), 'INVALID_SEQUENCE')
})

test('the public transition table is immutable and terminal states accept no events', () => {
  assert.ok(Object.isFrozen(STRONGFLOW_JOB_TRANSITIONS))
  for (const state of ['FAILED', 'REJECTED', 'CANCELLED', 'DELIVERED']) {
    assert.deepEqual(STRONGFLOW_JOB_TRANSITIONS[state], [])
    assert.ok(Object.isFrozen(STRONGFLOW_JOB_TRANSITIONS[state]))
  }
  assert.deepEqual(STRONGFLOW_JOB_TRANSITIONS.AWAITING_HUMAN_REVIEW, [
    'human-review.approved',
    'human-review.changes-requested',
    'human-review.rejected',
    'job.interrupted',
    'job.cancelled',
  ])
})

test('every state rejects each event omitted from its public transition table', () => {
  const snapshots = new Map()
  const main = jobFixture('transition-matrix')
  main.emit('job.created', {})
  snapshots.set(main.current().state, main.current())
  const currentDefinition = definition('transition-matrix')
  runStage(main, 'REQUIREMENTS', 'matrix-requirements', {
    requirementId: currentDefinition.requirementId,
  })
  snapshots.set(main.current().state, main.current())
  runStage(main, 'SOLUTION', 'matrix-solution', {
    requirementId: currentDefinition.requirementId,
    solutionId: currentDefinition.solutionId,
  })
  snapshots.set(main.current().state, main.current())
  runStage(main, 'DIAGRAMS', 'matrix-diagrams', { definition: currentDefinition })
  snapshots.set(main.current().state, main.current())
  const reviewSnapshot = main.current()
  approve(main, currentDefinition, 'matrix')
  snapshots.set(main.current().state, main.current())
  runStage(main, 'PLANNING', 'matrix-planning')
  snapshots.set(main.current().state, main.current())
  const candidateId = CandidateId('candidate-transition-matrix')
  runStage(main, 'EXECUTION', 'matrix-execution', { candidateId })
  snapshots.set(main.current().state, main.current())
  runStage(main, 'VERIFICATION', 'matrix-verification-remediate', {
    candidateId,
    outcome: 'remediation-required',
  })
  snapshots.set(main.current().state, main.current())
  runStage(main, 'REMEDIATION', 'matrix-remediation', { candidateId })
  snapshots.set(main.current().state, main.current())
  runStage(main, 'VERIFICATION', 'matrix-verification-pass', {
    candidateId,
    outcome: 'passed',
  })
  snapshots.set(main.current().state, main.current())
  main.emit('completion-gate.passed', {
    stageRunId: StageRunId('matrix-completion-gate'),
    candidateId,
  })
  snapshots.set(main.current().state, main.current())
  runStage(main, 'DELIVERY', 'matrix-delivery', { candidateId })
  snapshots.set(main.current().state, main.current())
  main.emit('job.delivered', { candidateId })
  snapshots.set(main.current().state, main.current())

  const interrupted = jobFixture('transition-matrix-interrupted')
  interrupted.emit('job.created', {})
  const interruptedRun = stageIdentity('matrix-interrupted')
  interrupted.emit('stage.started', {
    stage: 'REQUIREMENTS',
    ...interruptedRun,
  }, roleSource('matrix-interrupted'))
  interrupted.emit('job.interrupted', {
    reason: 'Matrix interruption.',
    stageRunId: interruptedRun.stageRunId,
  })
  snapshots.set(interrupted.current().state, interrupted.current())

  const failed = jobFixture('transition-matrix-failed')
  failed.emit('job.created', {})
  const failedRun = stageIdentity('matrix-failed')
  const failedSource = roleSource('matrix-failed')
  failed.emit('stage.started', { stage: 'REQUIREMENTS', ...failedRun }, failedSource)
  failed.emit('stage.failed', {
    stage: 'REQUIREMENTS',
    ...failedRun,
    category: 'task',
    code: 'MATRIX_FAILURE',
    message: 'Matrix failure.',
    retryable: false,
  }, failedSource)
  snapshots.set(failed.current().state, failed.current())

  const rejected = jobFixture('transition-matrix-rejected')
  const rejectedDefinition = reachHumanReview(rejected, definition('matrix-rejected'))
  rejected.emit('human-review.rejected', {
    reviewId: HumanReviewId('review-matrix-rejected'),
    reviewerId: humanSource.actorId,
    definition: rejectedDefinition,
  }, humanSource)
  snapshots.set(rejected.current().state, rejected.current())

  const cancelled = jobFixture('transition-matrix-cancelled')
  cancelled.emit('job.created', {})
  cancelled.emit('job.cancelled', { reason: 'Matrix cancellation.' })
  snapshots.set(cancelled.current().state, cancelled.current())

  assert.deepEqual([...snapshots.keys()].sort(), [
    'AWAITING_COMPLETION_GATE',
    'AWAITING_HUMAN_REVIEW',
    'CANCELLED',
    'DEFINING_DIAGRAMS',
    'DEFINING_REQUIREMENTS',
    'DEFINING_SOLUTION',
    'DELIVERED',
    'DELIVERING',
    'EXECUTING',
    'FAILED',
    'INTERRUPTED',
    'PLANNING',
    'READY_TO_DELIVER',
    'REJECTED',
    'REMEDIATING',
    'VERIFYING',
  ])

  const eventKinds = [
    'job.created',
    'stage.started',
    'stage.succeeded',
    'stage.failed',
    'human-review.approved',
    'human-review.changes-requested',
    'human-review.rejected',
    'job.interrupted',
    'job.resumed',
    'job.cancelled',
    'completion-gate.passed',
    'completion-gate.failed',
    'job.delivered',
  ]
  const eventDefinition = definition('matrix-event')

  function eventFor(snapshot, kind) {
    const common = {
      jobId: snapshot.jobId,
      sequence: (BigInt(snapshot.sequence) + 1n).toString(),
      occurredAtMillis: snapshot.lastOccurredAtMillis + 1,
      source: systemSource,
    }
    const identity = {
      stageRunId: StageRunId(`matrix-run-${snapshot.state.toLowerCase()}`),
      attemptId: AttemptId(`matrix-attempt-${snapshot.state.toLowerCase()}`),
    }
    switch (kind) {
      case 'job.created':
        return createStrongFlowJobEvent({ ...common, kind, data: {} })
      case 'stage.started':
        return createStrongFlowJobEvent({
          ...common,
          source: roleSource('matrix-event'),
          kind,
          data: { stage: 'REQUIREMENTS', ...identity },
        })
      case 'stage.succeeded':
        return createStrongFlowJobEvent({
          ...common,
          source: roleSource('matrix-event'),
          kind,
          data: {
            stage: 'REQUIREMENTS',
            ...identity,
            requirementId: eventDefinition.requirementId,
          },
        })
      case 'stage.failed':
        return createStrongFlowJobEvent({
          ...common,
          source: roleSource('matrix-event'),
          kind,
          data: {
            stage: 'REQUIREMENTS',
            ...identity,
            category: 'task',
            code: 'MATRIX_EVENT_FAILURE',
            message: 'Matrix event failure.',
            retryable: false,
          },
        })
      case 'human-review.approved':
      case 'human-review.rejected':
        return createStrongFlowJobEvent({
          ...common,
          source: humanSource,
          kind,
          data: {
            reviewId: HumanReviewId(`review-${kind}-${snapshot.state.toLowerCase()}`),
            reviewerId: humanSource.actorId,
            definition: eventDefinition,
          },
        })
      case 'human-review.changes-requested':
        return createStrongFlowJobEvent({
          ...common,
          source: humanSource,
          kind,
          data: {
            reviewId: HumanReviewId(`review-changes-${snapshot.state.toLowerCase()}`),
            reviewerId: humanSource.actorId,
            definition: eventDefinition,
            scope: 'diagrams',
          },
        })
      case 'job.interrupted':
        return createStrongFlowJobEvent({
          ...common,
          kind,
          data: { reason: 'Matrix event interruption.' },
        })
      case 'job.resumed':
        return createStrongFlowJobEvent({
          ...common,
          kind,
          data: { interruptionSequence: '1' },
        })
      case 'job.cancelled':
        return createStrongFlowJobEvent({
          ...common,
          kind,
          data: { reason: 'Matrix event cancellation.' },
        })
      case 'completion-gate.passed':
        return createStrongFlowJobEvent({
          ...common,
          kind,
          data: {
            stageRunId: identity.stageRunId,
            candidateId: CandidateId('matrix-event-candidate'),
          },
        })
      case 'completion-gate.failed':
        return createStrongFlowJobEvent({
          ...common,
          kind,
          data: {
            stageRunId: identity.stageRunId,
            candidateId: CandidateId('matrix-event-candidate'),
            reason: 'Matrix completion failure.',
          },
        })
      case 'job.delivered':
        return createStrongFlowJobEvent({
          ...common,
          kind,
          data: { candidateId: CandidateId('matrix-event-candidate') },
        })
      default:
        assert.fail(`unexpected matrix event ${kind}`)
    }
  }

  for (const [state, snapshot] of snapshots) {
    for (const kind of eventKinds) {
      if (STRONGFLOW_JOB_TRANSITIONS[state].includes(kind)) continue
      const before = structuredClone(snapshot)
      expectTransitionError(
        () => applyStrongFlowJobEvent(snapshot, eventFor(snapshot, kind)),
        ['FAILED', 'REJECTED', 'CANCELLED', 'DELIVERED'].includes(state)
          ? 'TERMINAL_JOB'
          : 'ILLEGAL_TRANSITION',
      )
      assert.deepEqual(snapshot, before)
    }
  }

  const uncreated = {
    jobId: JobId('job-transition-matrix-uncreated'),
    sequence: '0',
    lastOccurredAtMillis: 0,
    state: 'DEFINING_REQUIREMENTS',
  }
  for (const kind of eventKinds.filter(kind => kind !== 'job.created')) {
    expectTransitionError(
      () => applyStrongFlowJobEvent(undefined, eventFor(uncreated, kind)),
      'ILLEGAL_TRANSITION',
    )
  }
})
