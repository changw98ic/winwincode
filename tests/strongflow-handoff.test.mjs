import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  AttemptId,
  CandidateId,
  DiagramId,
  ExecutionPlanId,
  HumanReviewId,
  JobId,
  PatchManifestId,
  RemediationRequestId,
  RequirementId,
  ReviewReportId,
  SolutionId,
  StageRunId,
  StrongFlowHandoffValidationError,
  UserRequestId,
  VerificationReportId,
  createStrongFlowJobEvent,
  materializeStrongFlowArtifact,
  parseStrongFlowHandoffManifest,
} from '../packages/contracts/dist/index.js'
import {
  StrongFlowArtifactStore,
  StrongFlowHandoffBuilder,
  StrongFlowHandoffError,
  StrongFlowJobStore,
  generateStrongFlowDefinitionDiagrams,
} from '../packages/strongflow/dist/index.js'

const HASH_A = 'a'.repeat(64)
const HASH_B = 'b'.repeat(64)

async function temporaryHome(t) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-handoff-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  return home
}

function interval(suffix) {
  return Object.freeze({
    schemaVersion: 1,
    kernelSessionLineageId: `handoff-lineage-${suffix}`,
    contextId: `handoff-context-${suffix}`,
    generation: 1,
    kernelSessionId: `handoff-kernel-${suffix}`,
    kernelStreamId: `handoff-stream-${suffix}`,
    turnId: `handoff-turn-${suffix}`,
    firstSequence: '10',
    lastSequence: '12',
    eventCount: 3,
  })
}

function roleMetadata(
  jobId,
  artifactId,
  sources,
  roleId,
  stageRunId,
  attemptId,
  time,
  eventInterval,
) {
  return Object.freeze({
    artifactId,
    jobId,
    sourceArtifacts: Object.freeze(sources),
    producer: Object.freeze({ kind: 'role', roleId, stageRunId, attemptId }),
    kernelEventInterval: eventInterval ?? interval(`${roleId}-${artifactId}`),
    createdAtMillis: time,
  })
}

function ref(artifactKind, artifactId) {
  return Object.freeze({ artifactKind, artifactId })
}

async function append(store, kind, data, source = { kind: 'system', actorId: 'fixture' }) {
  const stored = await store.read()
  return store.append(createStrongFlowJobEvent({
    jobId: stored.snapshot.jobId,
    sequence: (BigInt(stored.snapshot.sequence) + 1n).toString(),
    occurredAtMillis: stored.snapshot.lastOccurredAtMillis + 1,
    source,
    kind,
    data,
  }))
}

function roleSource(roleId, kernelSessionId) {
  return Object.freeze({
    kind: 'role',
    actorId: roleId,
    ...(kernelSessionId === undefined ? {} : { kernelSessionId }),
  })
}

function expectHandoffError(code) {
  return error => error instanceof StrongFlowHandoffError && error.code === code
}

function expectHandoffValidationError(code, path) {
  return error => (
    error instanceof StrongFlowHandoffValidationError
    && error.code === code
    && (path === undefined || error.path === path)
  )
}

async function definitionFixture(t) {
  const home = await temporaryHome(t)
  const jobId = JobId('handoff-job')
  const ids = Object.freeze({
    user: UserRequestId('handoff-user-request'),
    requirement: RequirementId('handoff-requirement'),
    solution: SolutionId('handoff-solution'),
    architecture: DiagramId('handoff-system-diagram'),
    process: DiagramId('handoff-process-diagram'),
    review: HumanReviewId('handoff-human-review'),
    plan: ExecutionPlanId('handoff-plan'),
    patch: PatchManifestId('handoff-patch'),
    reviewReport: ReviewReportId('handoff-review-report'),
    verification: VerificationReportId('handoff-verification'),
    remediation: RemediationRequestId('handoff-remediation-request'),
  })
  const runs = Object.freeze({
    requirements: Object.freeze({
      stageRunId: StageRunId('handoff-run-requirements'),
      attemptId: AttemptId('handoff-attempt-requirements'),
    }),
    solution: Object.freeze({
      stageRunId: StageRunId('handoff-run-solution'),
      attemptId: AttemptId('handoff-attempt-solution'),
    }),
    diagrams: Object.freeze({
      stageRunId: StageRunId('handoff-run-diagrams'),
      attemptId: AttemptId('handoff-attempt-diagrams'),
    }),
    planning: Object.freeze({
      stageRunId: StageRunId('handoff-run-planning'),
      attemptId: AttemptId('handoff-attempt-planning'),
    }),
    execution: Object.freeze({
      stageRunId: StageRunId('handoff-run-execution'),
      attemptId: AttemptId('handoff-attempt-execution'),
    }),
    verification: Object.freeze({
      stageRunId: StageRunId('handoff-run-verification'),
      attemptId: AttemptId('handoff-attempt-verification'),
    }),
    remediation: Object.freeze({
      stageRunId: StageRunId('handoff-run-remediation'),
      attemptId: AttemptId('handoff-attempt-remediation'),
    }),
  })
  const created = createStrongFlowJobEvent({
    jobId,
    sequence: '1',
    occurredAtMillis: 2_000_000_000_000,
    source: { kind: 'system', actorId: 'fixture-controller' },
    kind: 'job.created',
    data: { title: 'Controlled handoffs' },
  })
  const jobStore = await StrongFlowJobStore.create({ home, event: created })
  const artifactStore = await StrongFlowArtifactStore.create({
    home,
    jobId,
    createdAtMillis: created.occurredAtMillis,
  })
  const user = materializeStrongFlowArtifact('USER_REQUEST', {
    artifactId: ids.user,
    jobId,
    sourceArtifacts: [],
    producer: { kind: 'human', actorId: 'requester', channel: 'local-ui' },
    kernelEventInterval: null,
    createdAtMillis: created.occurredAtMillis,
  }, {
    request: 'Build exact, durable, bounded role handoffs.',
    submittedFrom: 'strongflow-workbench',
  })
  await artifactStore.publishArtifact(user)
  await append(jobStore, 'stage.started', {
    stage: 'REQUIREMENTS',
    ...runs.requirements,
  }, roleSource('requirements'))

  const requirement = materializeStrongFlowArtifact(
    'REQUIREMENT_SPEC',
    roleMetadata(
      jobId,
      ids.requirement,
      [ref('USER_REQUEST', ids.user)],
      'requirements',
      runs.requirements.stageRunId,
      runs.requirements.attemptId,
      2_000_000_000_002,
    ),
    {
      title: 'Controlled role handoffs',
      summary: 'Only exact approved artifacts enter each role context.',
      goals: [{ id: 'goal-handoff', text: 'Rebuild every handoff after restart.' }],
      nonGoals: [],
      constraints: [{ id: 'constraint-context', text: 'Bound model-visible context.' }],
      acceptanceCriteria: [{
        criterionId: 'criterion-replay',
        statement: 'A stored handoff replays the same inputs.',
        verification: 'Reopen both stores and reconstruct by handoff id.',
      }],
      repositoryFacts: [],
      risks: [],
      openQuestions: [],
    },
  )
  await artifactStore.publishArtifact(requirement)
  await append(jobStore, 'stage.succeeded', {
    stage: 'REQUIREMENTS',
    ...runs.requirements,
    requirementId: ids.requirement,
  }, roleSource('requirements'))
  await append(jobStore, 'stage.started', {
    stage: 'SOLUTION',
    ...runs.solution,
  }, roleSource('solution'))

  const solution = materializeStrongFlowArtifact(
    'SOLUTION_DESIGN',
    roleMetadata(
      jobId,
      ids.solution,
      [ref('REQUIREMENT_SPEC', ids.requirement)],
      'solution',
      runs.solution.stageRunId,
      runs.solution.attemptId,
      2_000_000_000_004,
    ),
    {
      requirementId: ids.requirement,
      summary: 'Resolve exact stored artifacts at one program-owned handoff seam.',
      decisions: [{
        decisionId: 'decision-pinned-inputs',
        title: 'Pin record identities',
        decision: 'Each handoff records the exact artifact record and blob identity.',
        rationale: 'Later duplicate artifacts cannot change a published handoff.',
        requirementItemIds: ['goal-handoff', 'criterion-replay'],
      }],
      components: [{
        componentId: 'component-handoff',
        name: 'Handoff builder',
        kind: 'module',
        responsibility: 'Select, validate, persist, and reconstruct role inputs.',
        trustBoundary: 'Local StrongFlow process',
        sourcePaths: ['packages/strongflow/src/handoff.ts'],
      }],
      connections: [],
      unresolvedFacts: [],
      risks: [],
    },
  )
  await artifactStore.publishArtifact(solution)
  const diagrams = generateStrongFlowDefinitionDiagrams({
    requirement,
    solution,
    systemArchitectureDiagramId: ids.architecture,
    processFlowDiagramId: ids.process,
    createdAtMillis: 2_000_000_000_005,
  })
  await artifactStore.publishArtifact(diagrams.systemArchitectureDiagram)
  await artifactStore.publishArtifact(diagrams.processFlowDiagram)
  await append(jobStore, 'stage.succeeded', {
    stage: 'SOLUTION',
    ...runs.solution,
    requirementId: ids.requirement,
    solutionId: ids.solution,
  }, roleSource('solution'))
  await append(jobStore, 'stage.started', {
    stage: 'DIAGRAMS',
    ...runs.diagrams,
  }, { kind: 'system', actorId: 'diagram-generator' })
  const definition = Object.freeze({
    requirementId: ids.requirement,
    solutionId: ids.solution,
    systemArchitectureDiagramId: ids.architecture,
    processFlowDiagramId: ids.process,
  })
  await append(jobStore, 'stage.succeeded', {
    stage: 'DIAGRAMS',
    ...runs.diagrams,
    definition,
  }, { kind: 'system', actorId: 'diagram-generator' })
  return {
    home,
    jobId,
    ids,
    runs,
    jobStore,
    artifactStore,
    user,
    requirement,
    solution,
    diagrams,
    definition,
  }
}

async function approve(fixture) {
  const snapshot = await append(fixture.jobStore, 'human-review.approved', {
    reviewId: fixture.ids.review,
    reviewerId: 'human-reviewer',
    definition: fixture.definition,
    comment: 'Proceed with this exact definition.',
  }, { kind: 'human', actorId: 'human-reviewer', channel: 'local-ui' })
  assert.ok(snapshot.approval)
  await fixture.artifactStore.publishArtifact(snapshot.approval)
  return snapshot.approval
}

async function startPlanning(fixture) {
  await append(fixture.jobStore, 'stage.started', {
    stage: 'PLANNING',
    ...fixture.runs.planning,
  }, roleSource('planner'))
}

function planArtifact(fixture, options = {}) {
  const approval = options.approval
  const ids = fixture.ids
  const sources = [
    ref('REQUIREMENT_SPEC', ids.requirement),
    ref('SOLUTION_DESIGN', ids.solution),
    ref('SYSTEM_ARCHITECTURE_DIAGRAM', ids.architecture),
    ref('PROCESS_FLOW_DIAGRAM', ids.process),
    ref('HUMAN_REVIEW_RECORD', approval.artifactId),
  ]
  return materializeStrongFlowArtifact(
    'EXECUTION_PLAN',
    roleMetadata(
      fixture.jobId,
      options.artifactId ?? ids.plan,
      sources,
      'planner',
      options.stageRunId ?? fixture.runs.planning.stageRunId,
      options.attemptId ?? fixture.runs.planning.attemptId,
      options.time ?? 2_000_000_000_020,
      options.eventInterval,
    ),
    {
      definition: fixture.definition,
      humanReviewId: approval.artifactId,
      summary: 'Implement the approved handoff module.',
      steps: [{
        stepId: 'step-handoff',
        title: 'Build the handoff',
        instructions: 'Resolve exact durable source records and reject stale inputs.',
        dependsOn: [],
        paths: ['packages/strongflow/src/handoff.ts'],
        commands: ['corepack pnpm typecheck'],
        checks: ['The handoff can be reconstructed after restart.'],
      }],
    },
  )
}

function candidate() {
  return Object.freeze({
    candidateId: CandidateId('handoff-candidate'),
    sourceSnapshotId: `source-sha256-${HASH_A}`,
    baseCommitId: '1'.repeat(40),
    baseTreeId: '2'.repeat(40),
    candidateCommitId: '3'.repeat(40),
    candidateTreeId: '4'.repeat(40),
    diffId: HASH_B,
  })
}

async function enterVerification(fixture, approval, plan) {
  await fixture.jobStore.append(createStrongFlowJobEvent({
    jobId: fixture.jobId,
    sequence: (BigInt((await fixture.jobStore.read()).snapshot.sequence) + 1n).toString(),
    occurredAtMillis: (await fixture.jobStore.read()).snapshot.lastOccurredAtMillis + 1,
    source: roleSource('planner', plan.kernelEventInterval.kernelSessionId),
    kind: 'stage.succeeded',
    data: { stage: 'PLANNING', ...fixture.runs.planning },
  }))
  await append(fixture.jobStore, 'stage.started', {
    stage: 'EXECUTION',
    ...fixture.runs.execution,
  }, roleSource('executor'))
  const approvedSources = [
    ref('REQUIREMENT_SPEC', fixture.ids.requirement),
    ref('SOLUTION_DESIGN', fixture.ids.solution),
    ref('SYSTEM_ARCHITECTURE_DIAGRAM', fixture.ids.architecture),
    ref('PROCESS_FLOW_DIAGRAM', fixture.ids.process),
    ref('HUMAN_REVIEW_RECORD', approval.artifactId),
  ]
  const patch = materializeStrongFlowArtifact(
    'PATCH_MANIFEST',
    roleMetadata(
      fixture.jobId,
      fixture.ids.patch,
      [...approvedSources, ref('EXECUTION_PLAN', plan.artifactId)],
      'executor',
      fixture.runs.execution.stageRunId,
      fixture.runs.execution.attemptId,
      2_000_000_000_030,
    ),
    {
      executionPlanId: plan.artifactId,
      candidate: candidate(),
      remediationRequestId: null,
      changedFiles: [{
        path: 'packages/strongflow/src/handoff.ts',
        changeType: 'added',
        previousPath: null,
        hunks: [{
          hunkId: 'handoff-hunk',
          oldStart: 0,
          oldLines: 0,
          newStart: 1,
          newLines: 10,
          summary: 'Add controlled handoffs.',
          diagramNodeIds: [],
        }],
      }],
      commands: [],
      tests: [],
    },
  )
  await fixture.artifactStore.publishArtifact(patch)
  await append(fixture.jobStore, 'stage.succeeded', {
    stage: 'EXECUTION',
    ...fixture.runs.execution,
    candidateId: patch.payload.candidate.candidateId,
  }, roleSource('executor', patch.kernelEventInterval.kernelSessionId))
  await append(fixture.jobStore, 'stage.started', {
    stage: 'VERIFICATION',
    ...fixture.runs.verification,
  }, roleSource('reviewer'))
  return { patch, approvedSources }
}

test('human and role handoffs pin separate exact inputs and replay after restart', async t => {
  const fixture = await definitionFixture(t)
  const builder = new StrongFlowHandoffBuilder({
    artifactStore: fixture.artifactStore,
    jobStore: fixture.jobStore,
  })
  const human = await builder.buildHumanReview()
  assert.deepEqual(human.inputs.map(input => input.kind), [
    'REQUIREMENT_SPEC',
    'SOLUTION_DESIGN',
    'SYSTEM_ARCHITECTURE_DIAGRAM',
    'PROCESS_FLOW_DIAGRAM',
  ])
  assert.equal(human.handoff.target.kind, 'human-review')
  assert.equal(human.record.entryKind, 'handoff')

  const reopenedArtifacts = await StrongFlowArtifactStore.open(fixture.home, fixture.jobId)
  const reopenedJobs = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
  const replay = await new StrongFlowHandoffBuilder({
    artifactStore: reopenedArtifacts,
    jobStore: reopenedJobs,
  }).reconstruct(human.handoff.handoffId)
  assert.deepEqual(replay.handoff, human.handoff)
  assert.deepEqual(replay.inputs, human.inputs)

  const approval = await approve(fixture)
  await startPlanning(fixture)
  const planner = await builder.buildRole({ roleId: 'planner', ...fixture.runs.planning })
  assert.deepEqual(planner.inputs.map(input => input.kind), [
    'REQUIREMENT_SPEC',
    'SOLUTION_DESIGN',
    'SYSTEM_ARCHITECTURE_DIAGRAM',
    'PROCESS_FLOW_DIAGRAM',
    'HUMAN_REVIEW_RECORD',
  ])
  assert.equal(planner.inputs[4].artifactId, approval.artifactId)

  const replayBuilder = new StrongFlowHandoffBuilder({
    artifactStore: await StrongFlowArtifactStore.open(fixture.home, fixture.jobId),
    jobStore: await StrongFlowJobStore.open(fixture.home, fixture.jobId),
  })
  for (const original of [human, planner]) {
    const rebuilt = await replayBuilder.reconstruct(original.handoff.handoffId)
    assert.deepEqual(rebuilt.handoff, original.handoff)
    assert.deepEqual(rebuilt.inputs, original.inputs)
  }
})

test('handoff boundary rejects unsupported versions, source swaps, and changed diagram identities', async t => {
  const fixture = await definitionFixture(t)
  const builder = new StrongFlowHandoffBuilder({
    artifactStore: fixture.artifactStore,
    jobStore: fixture.jobStore,
  })
  const built = await builder.buildHumanReview()

  assert.throws(
    () => parseStrongFlowHandoffManifest({ ...built.handoff, schemaVersion: 2 }),
    expectHandoffValidationError('UNSUPPORTED_SCHEMA_VERSION', 'handoff.schemaVersion'),
  )

  const swapped = structuredClone(built.handoff)
  const firstInput = swapped.inputs[0]
  swapped.inputs[0] = { ...swapped.inputs[1], position: 0 }
  swapped.inputs[1] = { ...firstInput, position: 1 }
  assert.throws(
    () => parseStrongFlowHandoffManifest(swapped),
    expectHandoffValidationError('INPUT_SET_MISMATCH', 'handoff.inputs'),
  )

  const changedDiagram = structuredClone(built.handoff)
  changedDiagram.inputs[2].artifactId = 'handoff-foreign-system-diagram'
  assert.throws(
    () => parseStrongFlowHandoffManifest(changedDiagram),
    error => {
      assert.ok(error instanceof StrongFlowHandoffValidationError)
      assert.equal(error.code, 'INVALID_RELATIONSHIP')
      assert.equal(error.path, 'handoff.definition')
      assert.doesNotMatch(error.message, new RegExp(fixture.user.payload.request, 'u'))
      return true
    },
  )
})

test('requirements never receive solution data and handoff context limits fail before a turn', async t => {
  const fixture = await definitionFixture(t)
  const freshHome = await temporaryHome(t)
  const freshJobId = JobId('handoff-requirements-job')
  const created = createStrongFlowJobEvent({
    jobId: freshJobId,
    sequence: '1',
    occurredAtMillis: 1,
    source: { kind: 'system', actorId: 'fixture-controller' },
    kind: 'job.created',
    data: {},
  })
  const jobStore = await StrongFlowJobStore.create({ home: freshHome, event: created })
  const artifactStore = await StrongFlowArtifactStore.create({
    home: freshHome,
    jobId: freshJobId,
    createdAtMillis: 1,
  })
  const user = materializeStrongFlowArtifact('USER_REQUEST', {
    artifactId: UserRequestId('handoff-requirements-user'),
    jobId: freshJobId,
    sourceArtifacts: [],
    producer: { kind: 'human', actorId: 'requester', channel: 'local-ui' },
    kernelEventInterval: null,
    createdAtMillis: 1,
  }, { request: 'Keep requirements separate.', submittedFrom: 'chat' })
  await artifactStore.publishArtifact(user)
  const run = {
    stageRunId: StageRunId('handoff-requirements-run'),
    attemptId: AttemptId('handoff-requirements-attempt'),
  }
  await append(jobStore, 'stage.started', { stage: 'REQUIREMENTS', ...run }, roleSource('requirements'))
  const builder = new StrongFlowHandoffBuilder({ artifactStore, jobStore })
  const handoff = await builder.buildRole({ roleId: 'requirements', ...run })
  assert.deepEqual(handoff.inputs.map(input => input.kind), ['USER_REQUEST'])
  assert.equal(JSON.stringify(handoff.inputs).includes(fixture.solution.payload.summary), false)

  const bounded = new StrongFlowHandoffBuilder({
    artifactStore,
    jobStore,
    contextLimitBytes: 1,
  })
  await assert.rejects(
    bounded.buildRole({ roleId: 'requirements', ...run }),
    expectHandoffError('CONTEXT_LIMIT_EXCEEDED'),
  )
})

test('executor selection is pinned to the successful planning attempt and ignores later duplicates', async t => {
  const fixture = await definitionFixture(t)
  const approval = await approve(fixture)
  await startPlanning(fixture)
  const plan = planArtifact(fixture, { approval })
  await fixture.artifactStore.publishArtifact(plan)
  await append(fixture.jobStore, 'stage.succeeded', {
    stage: 'PLANNING',
    ...fixture.runs.planning,
  }, roleSource('planner', plan.kernelEventInterval.kernelSessionId))
  await append(fixture.jobStore, 'stage.started', {
    stage: 'EXECUTION',
    ...fixture.runs.execution,
  }, roleSource('executor'))
  const foreignSessionPlan = planArtifact(fixture, {
    approval,
    artifactId: ExecutionPlanId('handoff-foreign-session-plan'),
    time: 2_000_000_000_098,
  })
  assert.equal(foreignSessionPlan.producer.stageRunId, plan.producer.stageRunId)
  assert.equal(foreignSessionPlan.producer.attemptId, plan.producer.attemptId)
  assert.notEqual(
    foreignSessionPlan.kernelEventInterval.kernelSessionId,
    plan.kernelEventInterval.kernelSessionId,
  )
  await fixture.artifactStore.publishArtifact(foreignSessionPlan)
  const builder = new StrongFlowHandoffBuilder({
    artifactStore: fixture.artifactStore,
    jobStore: fixture.jobStore,
  })
  const first = await builder.buildRole({ roleId: 'executor', ...fixture.runs.execution })
  assert.deepEqual(first.inputs.map(input => input.kind), ['EXECUTION_PLAN'])
  assert.equal(first.inputs.at(-1).artifactId, plan.artifactId)

  const duplicate = planArtifact(fixture, {
    approval,
    artifactId: ExecutionPlanId('handoff-unselected-plan'),
    time: 2_000_000_000_099,
    eventInterval: plan.kernelEventInterval,
  })
  assert.equal(duplicate.producer.stageRunId, plan.producer.stageRunId)
  assert.equal(duplicate.producer.attemptId, plan.producer.attemptId)
  assert.equal(
    duplicate.kernelEventInterval.kernelSessionId,
    plan.kernelEventInterval.kernelSessionId,
  )
  await fixture.artifactStore.publishArtifact(duplicate)
  const second = await builder.buildRole({ roleId: 'executor', ...fixture.runs.execution })
  assert.equal(second.handoff.handoffId, first.handoff.handoffId)
  assert.equal(second.record.recordId, first.record.recordId)
  assert.equal(second.inputs.at(-1).artifactId, plan.artifactId)

  const restarted = new StrongFlowHandoffBuilder({
    artifactStore: await StrongFlowArtifactStore.open(fixture.home, fixture.jobId),
    jobStore: await StrongFlowJobStore.open(fixture.home, fixture.jobId),
  })
  const replayed = await restarted.buildRole({
    roleId: 'executor',
    ...fixture.runs.execution,
  })
  assert.deepEqual(replayed.handoff, first.handoff)
  assert.deepEqual(replayed.inputs, first.inputs)
})

test('duplicate results from one accepted role attempt fail before the first handoff is selected', async t => {
  const fixture = await definitionFixture(t)
  const approval = await approve(fixture)
  await startPlanning(fixture)
  const first = planArtifact(fixture, { approval })
  const duplicate = planArtifact(fixture, {
    approval,
    artifactId: ExecutionPlanId('handoff-plan-duplicate-before-selection'),
    time: 2_000_000_000_021,
    eventInterval: first.kernelEventInterval,
  })
  await fixture.artifactStore.publishArtifact(first)
  await fixture.artifactStore.publishArtifact(duplicate)
  await append(fixture.jobStore, 'stage.succeeded', {
    stage: 'PLANNING',
    ...fixture.runs.planning,
  }, roleSource('planner', first.kernelEventInterval.kernelSessionId))
  await append(fixture.jobStore, 'stage.started', {
    stage: 'EXECUTION',
    ...fixture.runs.execution,
  }, roleSource('executor'))

  const builder = new StrongFlowHandoffBuilder({
    artifactStore: fixture.artifactStore,
    jobStore: fixture.jobStore,
  })
  await assert.rejects(
    builder.buildRole({ roleId: 'executor', ...fixture.runs.execution }),
    expectHandoffError('ARTIFACT_AMBIGUOUS'),
  )
})

test('candidate reports cannot swap in a different frozen candidate identity', async t => {
  const fixture = await definitionFixture(t)
  const approval = await approve(fixture)
  await startPlanning(fixture)
  const plan = planArtifact(fixture, { approval })
  await fixture.artifactStore.publishArtifact(plan)
  const { patch, approvedSources } = await enterVerification(fixture, approval, plan)
  const foreignCandidate = Object.freeze({
    ...patch.payload.candidate,
    candidateId: CandidateId('handoff-foreign-candidate'),
  })
  const review = materializeStrongFlowArtifact(
    'REVIEW_REPORT',
    roleMetadata(
      fixture.jobId,
      ReviewReportId('handoff-foreign-candidate-review'),
      [
        ...approvedSources,
        ref('EXECUTION_PLAN', plan.artifactId),
        ref('PATCH_MANIFEST', patch.artifactId),
      ],
      'reviewer',
      fixture.runs.verification.stageRunId,
      fixture.runs.verification.attemptId,
      2_000_000_000_040,
    ),
    {
      patchManifestId: patch.artifactId,
      candidate: foreignCandidate,
      outcome: 'accepted',
      summary: 'This report attempts to replace the frozen candidate identity.',
      findings: [],
    },
  )
  await fixture.artifactStore.publishArtifact(review)

  const builder = new StrongFlowHandoffBuilder({
    artifactStore: fixture.artifactStore,
    jobStore: fixture.jobStore,
  })
  await assert.rejects(
    builder.buildRole({ roleId: 'verifier', ...fixture.runs.verification }),
    expectHandoffError('STALE_CANDIDATE'),
  )
})

test('review, verification, adversarial, and remediation handoffs keep one frozen candidate', async t => {
  const fixture = await definitionFixture(t)
  const approval = await approve(fixture)
  await startPlanning(fixture)
  const plan = planArtifact(fixture, { approval })
  await fixture.artifactStore.publishArtifact(plan)
  const { patch, approvedSources } = await enterVerification(fixture, approval, plan)
  const builder = new StrongFlowHandoffBuilder({
    artifactStore: fixture.artifactStore,
    jobStore: fixture.jobStore,
  })
  const reviewer = await builder.buildRole({ roleId: 'reviewer', ...fixture.runs.verification })
  assert.equal(reviewer.inputs.at(-1).artifactId, patch.artifactId)
  assert.deepEqual(reviewer.handoff.candidate, patch.payload.candidate)

  const candidateSources = [
    ...approvedSources,
    ref('EXECUTION_PLAN', plan.artifactId),
    ref('PATCH_MANIFEST', patch.artifactId),
  ]
  const review = materializeStrongFlowArtifact(
    'REVIEW_REPORT',
    roleMetadata(
      fixture.jobId,
      fixture.ids.reviewReport,
      candidateSources,
      'reviewer',
      fixture.runs.verification.stageRunId,
      fixture.runs.verification.attemptId,
      2_000_000_000_040,
    ),
    {
      patchManifestId: patch.artifactId,
      candidate: patch.payload.candidate,
      outcome: 'changes-required',
      summary: 'One failed check requires remediation.',
      findings: [{
        findingId: 'finding-handoff',
        severity: 'major',
        title: 'Reject stale inputs',
        message: 'Handoff selection must remain pinned.',
        location: { path: 'packages/strongflow/src/handoff.ts', hunkId: 'handoff-hunk' },
        diagramNodeIds: [],
        disposition: 'open',
      }],
    },
  )
  await fixture.artifactStore.publishArtifact(review)
  const verifier = await builder.buildRole({ roleId: 'verifier', ...fixture.runs.verification })
  assert.equal(verifier.inputs.at(-1).artifactId, review.artifactId)

  const verification = materializeStrongFlowArtifact(
    'VERIFICATION_REPORT',
    roleMetadata(
      fixture.jobId,
      fixture.ids.verification,
      [...candidateSources, ref('REVIEW_REPORT', review.artifactId)],
      'verifier',
      fixture.runs.verification.stageRunId,
      fixture.runs.verification.attemptId,
      2_000_000_000_041,
    ),
    {
      patchManifestId: patch.artifactId,
      candidate: patch.payload.candidate,
      mode: 'standard',
      outcome: 'failed',
      summary: 'The negative handoff test failed.',
      checks: [{
        checkId: 'check-handoff-negative',
        title: 'Reject stale handoff',
        command: 'node --test tests/strongflow-handoff.test.mjs',
        outcome: 'failed',
        evidence: 'The exact failed command result is retained in this immutable report.',
        relatedFindingIds: ['finding-handoff'],
      }],
    },
  )
  await fixture.artifactStore.publishArtifact(verification)
  const adversarial = await builder.buildRole({
    roleId: 'adversarial-verifier',
    ...fixture.runs.verification,
  })
  assert.equal(adversarial.inputs.at(-1).artifactId, verification.artifactId)
  assert.deepEqual(adversarial.handoff.candidate, patch.payload.candidate)

  const request = materializeStrongFlowArtifact(
    'REMEDIATION_REQUEST',
    {
      artifactId: fixture.ids.remediation,
      jobId: fixture.jobId,
      sourceArtifacts: [
        ...candidateSources,
        ref('REVIEW_REPORT', review.artifactId),
        ref('VERIFICATION_REPORT', verification.artifactId),
      ],
      producer: { kind: 'system', actorId: 'remediation-gate' },
      kernelEventInterval: null,
      createdAtMillis: 2_000_000_000_042,
    },
    {
      candidate: patch.payload.candidate,
      patchManifestId: patch.artifactId,
      reason: 'Fix the exact failed negative check.',
      findings: [{
        sourceArtifactKind: 'VERIFICATION_REPORT',
        sourceArtifactId: verification.artifactId,
        findingId: 'check-handoff-negative',
        instruction: 'Make the stale-handoff negative test pass.',
        diagramNodeIds: [],
      }],
      annotationIds: [],
      boundedPaths: ['packages/strongflow/src/handoff.ts'],
    },
  )
  await fixture.artifactStore.publishArtifact(request)
  await append(fixture.jobStore, 'stage.succeeded', {
    stage: 'VERIFICATION',
    ...fixture.runs.verification,
    candidateId: patch.payload.candidate.candidateId,
    outcome: 'remediation-required',
  }, roleSource('reviewer'))
  await append(fixture.jobStore, 'stage.started', {
    stage: 'REMEDIATION',
    ...fixture.runs.remediation,
  }, roleSource('remediator'))
  const remediation = await builder.buildRole({ roleId: 'remediator', ...fixture.runs.remediation })
  assert.equal(remediation.inputs.at(-1).artifactId, request.artifactId)
  assert.equal(
    remediation.inputs.at(-1).value.payload.findings[0].instruction,
    'Make the stale-handoff negative test pass.',
  )
  assert.ok(remediation.inputs.every(input => (
    !('payload' in input.value)
    || input.value.payload.candidate === undefined
    || input.value.payload.candidate.candidateId === patch.payload.candidate.candidateId
  )))
  const restarted = new StrongFlowHandoffBuilder({
    artifactStore: await StrongFlowArtifactStore.open(fixture.home, fixture.jobId),
    jobStore: await StrongFlowJobStore.open(fixture.home, fixture.jobId),
  })
  for (const original of [reviewer, verifier, adversarial, remediation]) {
    const replay = await restarted.reconstruct(original.handoff.handoffId)
    assert.deepEqual(replay.handoff, original.handoff)
    assert.deepEqual(replay.inputs, original.inputs)
  }
})

test('wrong stage attempts and absent approval fail before publishing a role handoff', async t => {
  const fixture = await definitionFixture(t)
  const builder = new StrongFlowHandoffBuilder({
    artifactStore: fixture.artifactStore,
    jobStore: fixture.jobStore,
  })
  await assert.rejects(
    builder.buildRole({ roleId: 'planner', ...fixture.runs.planning }),
    expectHandoffError('WRONG_JOB_STATE'),
  )
  await approve(fixture)
  await startPlanning(fixture)
  await assert.rejects(
    builder.buildRole({
      roleId: 'planner',
      stageRunId: fixture.runs.planning.stageRunId,
      attemptId: AttemptId('handoff-wrong-attempt'),
    }),
    expectHandoffError('STAGE_RUN_MISMATCH'),
  )
})
