import assert from 'node:assert/strict'
import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises'
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
  createStrongFlowJobEvent,
} from '../packages/contracts/dist/index.js'
import {
  StrongFlowJobStore,
  StrongFlowJobStoreError,
} from '../packages/strongflow/dist/index.js'

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

async function storeFixture(t, name = 'main') {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-job-store-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const jobId = JobId(`job-store-${name}`)
  let sequence = 1
  let occurredAtMillis = 1_800_000_000_000
  const created = createStrongFlowJobEvent({
    jobId,
    sequence: String(sequence),
    occurredAtMillis,
    source: systemSource,
    kind: 'job.created',
    data: { title: `Stored fixture ${name}` },
  })
  const store = await StrongFlowJobStore.create({ home, event: created })

  async function append(kind, data, source = systemSource) {
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
    const snapshot = await store.append(event)
    sequence = nextSequence
    occurredAtMillis = nextOccurredAtMillis
    return { event, snapshot }
  }

  return {
    append,
    created,
    home,
    jobId,
    store,
    nextSequence: () => String(sequence + 1),
    nextTime: () => occurredAtMillis + 1,
  }
}

function stageIdentity(name) {
  return {
    stageRunId: StageRunId(`run-${name}`),
    attemptId: AttemptId(`attempt-${name}`),
  }
}

async function runStage(fixture, stage, name, successData, roleId = stage.toLowerCase()) {
  const identity = stageIdentity(name)
  const source = roleSource(roleId)
  await fixture.append('stage.started', { stage, ...identity }, source)
  return fixture.append('stage.succeeded', { stage, ...identity, ...successData }, source)
}

function expectStoreError(code) {
  return error => error instanceof StrongFlowJobStoreError && error.code === code
}

test('creates, appends, lists, reopens, and projects the exact approved definition', async t => {
  const fixture = await storeFixture(t, 'approved')
  const currentDefinition = Object.freeze({
    requirementId: RequirementId('stored-requirement-v1'),
    solutionId: SolutionId('stored-solution-v1'),
    systemArchitectureDiagramId: DiagramId('stored-architecture-v1'),
    processFlowDiagramId: DiagramId('stored-process-v1'),
  })

  await runStage(fixture, 'REQUIREMENTS', 'requirements', {
    requirementId: currentDefinition.requirementId,
  })
  await runStage(fixture, 'SOLUTION', 'solution', {
    requirementId: currentDefinition.requirementId,
    solutionId: currentDefinition.solutionId,
  })
  await runStage(fixture, 'DIAGRAMS', 'diagrams', { definition: currentDefinition })
  const approved = await fixture.append('human-review.approved', {
    reviewId: HumanReviewId('stored-review-v1'),
    reviewerId: humanSource.actorId,
    definition: currentDefinition,
    comment: 'Approved from the local review surface.',
  }, humanSource)

  assert.equal(approved.snapshot.state, 'PLANNING')
  assert.deepEqual(approved.snapshot.definition, currentDefinition)
  assert.equal(approved.snapshot.approval.payload.decision, 'approved')

  const live = await fixture.store.read()
  const reopened = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
  const replayed = await reopened.read()
  assert.deepEqual(replayed, live)
  assert.equal(replayed.events.length, 8)
  assert.deepEqual(replayed.snapshot.approval.payload.definition, currentDefinition)
  assert.ok(Object.isFrozen(replayed.events))
  assert.ok(Object.isFrozen(replayed.events[0]))

  const jobs = await StrongFlowJobStore.list(fixture.home)
  assert.deepEqual(jobs, [{
    manifest: live.manifest,
    sequence: '8',
    state: 'PLANNING',
    definitionRevision: 1,
    definition: currentDefinition,
    approved: true,
  }])
  assert.ok(Object.isFrozen(jobs))
  assert.ok(Object.isFrozen(jobs[0]))

  const rootEntries = await readdir(join(fixture.home, 'strongflow-jobs'))
  assert.equal(rootEntries.length, 1)
  assert.match(rootEntries[0], /^[a-f0-9]{64}$/u)
  const firstEventText = await readFile(join(fixture.store.eventsDirectory, '1.json'), 'utf8')
  assert.ok(firstEventText.endsWith('\n'))
})

test('rejects duplicate jobs, duplicate events, gaps, and wrong job identities', async t => {
  const fixture = await storeFixture(t, 'identity')

  await assert.rejects(
    StrongFlowJobStore.create({ home: fixture.home, event: fixture.created }),
    expectStoreError('JOB_ALREADY_EXISTS'),
  )
  await assert.rejects(
    fixture.store.append(fixture.created),
    expectStoreError('EVENT_ALREADY_EXISTS'),
  )

  const gap = createStrongFlowJobEvent({
    jobId: fixture.jobId,
    sequence: '3',
    occurredAtMillis: fixture.nextTime(),
    source: roleSource('requirements'),
    kind: 'stage.started',
    data: { stage: 'REQUIREMENTS', ...stageIdentity('gap') },
  })
  await assert.rejects(
    fixture.store.append(gap),
    expectStoreError('EVENT_SEQUENCE_MISMATCH'),
  )

  const wrongJob = createStrongFlowJobEvent({
    jobId: JobId('job-store-other'),
    sequence: fixture.nextSequence(),
    occurredAtMillis: fixture.nextTime(),
    source: roleSource('requirements'),
    kind: 'stage.started',
    data: { stage: 'REQUIREMENTS', ...stageIdentity('wrong-job') },
  })
  await assert.rejects(
    fixture.store.append(wrongJob),
    expectStoreError('JOB_ID_MISMATCH'),
  )

  const stored = await fixture.store.read()
  assert.equal(stored.events.length, 1)
  assert.equal(stored.snapshot.state, 'DEFINING_REQUIREMENTS')
})

test('competing writers atomically publish only one event for a sequence', async t => {
  const fixture = await storeFixture(t, 'concurrency')
  const first = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
  const second = await StrongFlowJobStore.open(fixture.home, fixture.jobId)
  const eventTime = fixture.nextTime()
  const firstEvent = createStrongFlowJobEvent({
    jobId: fixture.jobId,
    sequence: fixture.nextSequence(),
    occurredAtMillis: eventTime,
    source: roleSource('requirements-a'),
    kind: 'stage.started',
    data: { stage: 'REQUIREMENTS', ...stageIdentity('concurrent-a') },
  })
  const secondEvent = createStrongFlowJobEvent({
    jobId: fixture.jobId,
    sequence: fixture.nextSequence(),
    occurredAtMillis: eventTime,
    source: roleSource('requirements-b'),
    kind: 'stage.started',
    data: { stage: 'REQUIREMENTS', ...stageIdentity('concurrent-b') },
  })

  const results = await Promise.allSettled([first.append(firstEvent), second.append(secondEvent)])
  assert.equal(results.filter(result => result.status === 'fulfilled').length, 1)
  const rejected = results.find(result => result.status === 'rejected')
  assert.ok(rejected)
  assert.ok(expectStoreError('EVENT_ALREADY_EXISTS')(rejected.reason))

  const stored = await fixture.store.read()
  assert.equal(stored.events.length, 2)
  assert.equal(stored.snapshot.state, 'DEFINING_REQUIREMENTS')
  assert.ok(stored.snapshot.activeStage)
  assert.ok([firstEvent.id, secondEvent.id].includes(stored.events[1].id))
})

test('ignores unpublished pending files but treats a partial published event as corruption', async t => {
  const fixture = await storeFixture(t, 'partial')
  await writeFile(
    join(fixture.store.eventsDirectory, '.pending-2-deadbeef.json'),
    '{"partial":',
    'utf8',
  )
  const recovered = await fixture.store.read()
  assert.equal(recovered.events.length, 1)

  await writeFile(join(fixture.store.eventsDirectory, '2.json'), '{"partial":', 'utf8')
  await assert.rejects(
    fixture.store.read(),
    expectStoreError('STORE_CORRUPT'),
  )
})

test('detects changed manifests and missing jobs with stable errors', async t => {
  const fixture = await storeFixture(t, 'corruption')
  await assert.rejects(
    StrongFlowJobStore.open(fixture.home, JobId('job-store-missing')),
    expectStoreError('JOB_NOT_FOUND'),
  )

  const creating = join(
    fixture.home,
    'strongflow-jobs',
    `.creating-${'a'.repeat(64)}-deadbeef`,
  )
  await mkdir(creating)
  assert.equal((await StrongFlowJobStore.list(fixture.home)).length, 1)

  const manifest = JSON.parse(await readFile(fixture.store.manifestPath, 'utf8'))
  await writeFile(
    fixture.store.manifestPath,
    `${JSON.stringify({ ...manifest, unexpected: true }, null, 2)}\n`,
    'utf8',
  )
  await assert.rejects(
    StrongFlowJobStore.open(fixture.home, fixture.jobId),
    expectStoreError('STORE_CORRUPT'),
  )
  await assert.rejects(
    StrongFlowJobStore.list(fixture.home),
    expectStoreError('STORE_CORRUPT'),
  )
})
