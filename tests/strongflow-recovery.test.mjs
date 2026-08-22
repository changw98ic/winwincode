import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve, join } from 'node:path'
import test from 'node:test'

import {
  AttemptId,
  CandidateId,
  DiagramId,
  JobId,
  RequirementId,
  SolutionId,
  StageRunId,
  createStrongFlowJobEvent,
} from '../packages/contracts/dist/index.js'
import {
  StrongFlowController,
  StrongFlowJobStore,
  StrongFlowJobStoreError,
} from '../packages/strongflow/dist/index.js'

const root = resolve(import.meta.dirname, '..')
const processFixture = resolve(root, 'tests/fixtures/strongflow-process-step.mjs')
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

function encoded(value) {
  return Buffer.from(JSON.stringify(value), 'utf8').toString('base64url')
}

function runProcess(command, home, jobId, options = {}) {
  const child = spawnSync(process.execPath, [
    processFixture,
    command,
    home,
    jobId,
    ...(options.payload === undefined ? [] : [encoded(options.payload)]),
  ], {
    cwd: root,
    encoding: 'utf8',
    timeout: 20_000,
  })
  assert.equal(
    child.signal,
    null,
    `StrongFlow child ended with ${child.signal ?? 'no signal'}: ${child.stderr}`,
  )
  assert.equal(child.status, options.status ?? 0, child.stderr || child.stdout)
  const line = child.stdout.trim().split('\n').at(-1)
  return line === undefined || line.length === 0 ? undefined : JSON.parse(line)
}

async function processJob(t, name) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-process-recovery-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const jobId = JobId(`process-job-${name}`)
  const event = createStrongFlowJobEvent({
    jobId,
    sequence: '1',
    occurredAtMillis: 1_910_000_000_000,
    source: { kind: 'system', actorId: 'process-test' },
    kind: 'job.created',
    data: { title: `Process recovery ${name}` },
  })
  const store = await StrongFlowJobStore.create({ home, event })
  return { home, jobId, store }
}

async function reopenedSnapshot(fixture) {
  const store = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
  return (await store.read()).snapshot
}

test('each committed boundary resumes in a fresh process at the next owed action', async t => {
  const fixture = await processJob(t, 'complete')
  const definitionSteps = [
    ['REQUIREMENTS', 'DEFINING_SOLUTION'],
    ['SOLUTION', 'DEFINING_DIAGRAMS'],
    ['DIAGRAMS', 'AWAITING_HUMAN_REVIEW'],
  ]

  for (const [stage, state] of definitionSteps) {
    const report = runProcess('advance', fixture.home, fixture.jobId)
    assert.equal(report.ok, true)
    assert.equal(report.providerCalls, 1)
    assert.equal(report.result.kind, 'stage-succeeded')
    assert.equal(report.result.stage, stage)
    assert.equal(report.result.snapshot.state, state)
    assert.equal((await reopenedSnapshot(fixture)).state, state)
  }

  const paused = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
  const pausedBefore = await paused.read()
  const stillWaiting = runProcess('advance', fixture.home, fixture.jobId)
  assert.equal(stillWaiting.result.kind, 'waiting-for-human-review')
  assert.equal(stillWaiting.providerCalls, 0)
  assert.equal((await paused.read()).events.length, pausedBefore.events.length)

  const approval = runProcess('review', fixture.home, fixture.jobId, {
    payload: {
      decision: 'approved',
      definition: pausedBefore.snapshot.definition,
      comment: 'Approve the exact process fixture definition.',
    },
  })
  assert.equal(approval.receipt.snapshot.state, 'PLANNING')
  assert.equal(approval.receipt.decision.producer.channel, 'cli')

  const executionSteps = [
    ['stage-succeeded', 'PLANNING', 'EXECUTING', 1],
    ['stage-succeeded', 'EXECUTION', 'VERIFYING', 1],
    ['stage-succeeded', 'VERIFICATION', 'AWAITING_COMPLETION_GATE', 1],
    ['completion-gate-passed', undefined, 'DELIVERING', 0],
    ['stage-succeeded', 'DELIVERY', 'READY_TO_DELIVER', 1],
    ['delivered', undefined, 'DELIVERED', 0],
  ]
  for (const [kind, stage, state, providerCalls] of executionSteps) {
    const report = runProcess('advance', fixture.home, fixture.jobId)
    assert.equal(report.result.kind, kind)
    assert.equal(report.providerCalls, providerCalls)
    assert.equal(report.result.snapshot.state, state)
    if (stage !== undefined) assert.equal(report.result.stage, stage)
    assert.equal((await reopenedSnapshot(fixture)).state, state)
  }

  const delivered = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
  const deliveredBefore = await delivered.read()
  const terminal = runProcess('advance', fixture.home, fixture.jobId)
  assert.equal(terminal.result.kind, 'terminal')
  assert.equal(terminal.providerCalls, 0)
  assert.equal((await delivered.read()).events.length, deliveredBefore.events.length)
})

test('a restart cannot approve stale diagrams or bypass the repeated review pause', async t => {
  const fixture = await processJob(t, 'stale-diagrams')
  for (let index = 0; index < 3; index += 1) {
    runProcess('advance', fixture.home, fixture.jobId)
  }
  const first = await reopenedSnapshot(fixture)
  assert.equal(first.state, 'AWAITING_HUMAN_REVIEW')

  const requested = runProcess('review', fixture.home, fixture.jobId, {
    payload: {
      decision: 'changes-requested',
      definition: first.definition,
      scope: 'diagrams',
      comment: 'Regenerate both diagrams.',
    },
  })
  assert.equal(requested.receipt.snapshot.state, 'DEFINING_DIAGRAMS')
  runProcess('advance', fixture.home, fixture.jobId)
  const second = await reopenedSnapshot(fixture)
  assert.equal(second.state, 'AWAITING_HUMAN_REVIEW')
  assert.equal(second.definition.requirementId, first.definition.requirementId)
  assert.equal(second.definition.solutionId, first.definition.solutionId)
  assert.notEqual(
    second.definition.systemArchitectureDiagramId,
    first.definition.systemArchitectureDiagramId,
  )
  assert.notEqual(
    second.definition.processFlowDiagramId,
    first.definition.processFlowDiagramId,
  )

  const staleSequence = second.sequence
  const stale = runProcess('review', fixture.home, fixture.jobId, {
    status: 2,
    payload: {
      decision: 'approved',
      definition: first.definition,
    },
  })
  assert.equal(stale.ok, false)
  assert.equal(stale.error.code, 'STALE_DEFINITION')
  assert.equal((await reopenedSnapshot(fixture)).sequence, staleSequence)

  const waiting = runProcess('advance', fixture.home, fixture.jobId)
  assert.equal(waiting.result.kind, 'waiting-for-human-review')
  assert.equal(waiting.providerCalls, 0)
  runProcess('review', fixture.home, fixture.jobId, {
    payload: { decision: 'approved', definition: second.definition },
  })
  const planning = runProcess('advance', fixture.home, fixture.jobId)
  assert.equal(planning.result.stage, 'PLANNING')
  assert.equal(planning.result.snapshot.state, 'EXECUTING')
})

test('a process exit after stage start stays incomplete until explicit interruption and resume',
  async t => {
    const fixture = await processJob(t, 'interruption')
    runProcess('crash-during-stage', fixture.home, fixture.jobId, { status: 23 })

    const crashed = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
    const afterCrash = await crashed.read()
    assert.equal(afterCrash.snapshot.state, 'DEFINING_REQUIREMENTS')
    assert.equal(afterCrash.snapshot.activeStage.stage, 'REQUIREMENTS')
    assert.deepEqual(
      afterCrash.events.map(event => event.kind),
      ['job.created', 'stage.started'],
    )
    const abandonedRunId = afterCrash.snapshot.activeStage.stageRunId

    const inactive = runProcess('advance', fixture.home, fixture.jobId)
    assert.equal(inactive.result.kind, 'active-stage')
    assert.equal(inactive.providerCalls, 0)
    const interrupted = runProcess('interrupt-active', fixture.home, fixture.jobId)
    assert.equal(interrupted.result.state, 'INTERRUPTED')

    const stopped = runProcess('advance', fixture.home, fixture.jobId)
    assert.equal(stopped.result.kind, 'interrupted')
    assert.equal(stopped.providerCalls, 0)
    const resumed = runProcess('resume', fixture.home, fixture.jobId)
    assert.equal(resumed.result.state, 'DEFINING_REQUIREMENTS')
    const completed = runProcess('advance', fixture.home, fixture.jobId)
    assert.equal(completed.result.kind, 'stage-succeeded')
    assert.equal(completed.result.snapshot.state, 'DEFINING_SOLUTION')

    const recovered = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
    const events = (await recovered.read()).events
    const starts = events.filter(event => event.kind === 'stage.started')
    const successes = events.filter(event => event.kind === 'stage.succeeded')
    assert.equal(starts.length, 2)
    assert.equal(successes.length, 1)
    assert.equal(starts[0].data.stageRunId, abandonedRunId)
    assert.notEqual(starts[1].data.stageRunId, abandonedRunId)
    assert.equal(successes[0].data.stageRunId, starts[1].data.stageRunId)
  })

function deterministicProviders(calls) {
  return STAGES.map(stage => ({
    stage,
    roleId: `recovery-role-${stage.toLowerCase()}`,
    async run(context) {
      calls.push(stage)
      const requirementId = context.snapshot.definition.requirementId
        ?? RequirementId('recovery-requirement')
      const solutionId = context.snapshot.definition.solutionId
        ?? SolutionId('recovery-solution')
      const candidateId = context.snapshot.candidateId
        ?? CandidateId('recovery-candidate')
      if (stage === 'REQUIREMENTS') return { output: { requirementId } }
      if (stage === 'SOLUTION') return { output: { requirementId, solutionId } }
      if (stage === 'DIAGRAMS') {
        return {
          output: {
            definition: {
              requirementId,
              solutionId,
              systemArchitectureDiagramId: DiagramId('recovery-architecture'),
              processFlowDiagramId: DiagramId('recovery-process'),
            },
          },
        }
      }
      if (stage === 'PLANNING') return { output: {} }
      if (stage === 'VERIFICATION') {
        return { output: { candidateId, outcome: 'passed' } }
      }
      return { output: { candidateId } }
    },
  }))
}

function deterministicController(store, calls, name) {
  let now = store.manifest.createdAtMillis
  return new StrongFlowController({
    store,
    providers: deterministicProviders(calls),
    completionGate: {
      authority: 'program',
      async evaluate() {
        return { outcome: 'passed' }
      },
    },
    controllerId: `recovery-controller-${name}`,
    clock: () => ++now,
    stageRunIdFactory: (operation, snapshot) => StageRunId(
      `recovery-run-${operation.toLowerCase()}-${snapshot.sequence}`,
    ),
    attemptIdFactory: (stage, snapshot) => AttemptId(
      `recovery-attempt-${stage.toLowerCase()}-${snapshot.sequence}`,
    ),
  })
}

test('publication failures distinguish uncommitted work from durable committed work', async t => {
  const beforeFixture = await processJob(t, 'before-publication')
  const beforeOriginalAppend = beforeFixture.store.append.bind(beforeFixture.store)
  beforeFixture.store.append = async event => {
    if (event.kind === 'stage.succeeded') {
      throw new StrongFlowJobStoreError(
        'STORE_IO_ERROR',
        'fixture failed before publishing stage success',
      )
    }
    return beforeOriginalAppend(event)
  }
  const beforeCalls = []
  await assert.rejects(
    deterministicController(beforeFixture.store, beforeCalls, 'before').advance(),
    error => error instanceof StrongFlowJobStoreError && error.code === 'STORE_IO_ERROR',
  )
  assert.deepEqual(beforeCalls, ['REQUIREMENTS'])
  const beforeReopened = await StrongFlowJobStore.open(
    beforeFixture.home,
    beforeFixture.jobId,
  )
  const beforeStored = await beforeReopened.read()
  assert.equal(beforeStored.snapshot.activeStage.stage, 'REQUIREMENTS')
  assert.equal(beforeStored.events.some(event => event.kind === 'stage.succeeded'), false)
  const beforeRestartCalls = []
  const beforeRestart = await deterministicController(
    beforeReopened,
    beforeRestartCalls,
    'before-restart',
  ).advance()
  assert.equal(beforeRestart.kind, 'active-stage')
  assert.deepEqual(beforeRestartCalls, [])

  const afterFixture = await processJob(t, 'after-publication')
  const afterOriginalAppend = afterFixture.store.append.bind(afterFixture.store)
  afterFixture.store.append = async event => {
    const snapshot = await afterOriginalAppend(event)
    if (event.kind === 'stage.succeeded') {
      throw new StrongFlowJobStoreError(
        'STORE_IO_ERROR',
        'fixture lost the acknowledgement after durable publication',
      )
    }
    return snapshot
  }
  const afterCalls = []
  await assert.rejects(
    deterministicController(afterFixture.store, afterCalls, 'after').advance(),
    error => error instanceof StrongFlowJobStoreError && error.code === 'STORE_IO_ERROR',
  )
  assert.deepEqual(afterCalls, ['REQUIREMENTS'])
  const afterReopened = await StrongFlowJobStore.open(
    afterFixture.home,
    afterFixture.jobId,
  )
  const afterStored = await afterReopened.read()
  assert.equal(afterStored.snapshot.state, 'DEFINING_SOLUTION')
  assert.equal(afterStored.snapshot.activeStage, undefined)
  assert.equal(afterStored.events.filter(event => event.kind === 'stage.succeeded').length, 1)

  const afterRestartCalls = []
  const continued = await deterministicController(
    afterReopened,
    afterRestartCalls,
    'after-restart',
  ).advance()
  assert.equal(continued.kind, 'stage-succeeded')
  assert.equal(continued.stage, 'SOLUTION')
  assert.deepEqual(afterRestartCalls, ['SOLUTION'])
})
