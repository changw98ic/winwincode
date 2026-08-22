import assert from 'node:assert/strict'
import { randomUUID } from 'node:crypto'
import {
  mkdtemp,
  mkdir,
  readFile,
  readdir,
  rm,
  unlink,
  writeFile,
} from 'node:fs/promises'
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
  RequirementId,
  SolutionId,
  StageRunId,
  UserRequestId,
  materializeStrongFlowArtifact,
} from '../packages/contracts/dist/index.js'
import {
  StrongFlowArtifactStore,
  StrongFlowArtifactStoreError,
  isPendingStrongFlowArtifactBlobFile,
} from '../packages/strongflow/dist/index.js'

const HASH_A = 'a'.repeat(64)
const HASH_B = 'b'.repeat(64)
const HASH_C = 'c'.repeat(64)

async function temporaryHome(t, name) {
  const home = await mkdtemp(join(tmpdir(), `winwincode-artifact-${name}-`))
  t.after(() => rm(home, { recursive: true, force: true }))
  return home
}

function interval(suffix) {
  return Object.freeze({
    schemaVersion: 1,
    kernelSessionLineageId: `artifact-store-lineage-${suffix}`,
    contextId: `artifact-store-context-${suffix}`,
    generation: 1,
    kernelSessionId: `artifact-store-kernel-${suffix}`,
    kernelStreamId: `artifact-store-stream-${suffix}`,
    turnId: `artifact-store-turn-${suffix}`,
    firstSequence: '31',
    lastSequence: '33',
    eventCount: 3,
  })
}

function requirementArtifact(jobId, suffix = 'one') {
  const artifactId = RequirementId(`stored-requirement-${suffix}`)
  const event = interval(`requirements-${suffix}`)
  return materializeStrongFlowArtifact('REQUIREMENT_SPEC', {
    artifactId,
    jobId,
    sourceArtifacts: [{
      artifactKind: 'USER_REQUEST',
      artifactId: UserRequestId(`stored-user-request-${suffix}`),
    }],
    producer: {
      kind: 'role',
      roleId: 'requirements',
      stageRunId: StageRunId(`stored-stage-requirements-${suffix}`),
      attemptId: AttemptId(`stored-attempt-requirements-${suffix}`),
    },
    kernelEventInterval: event,
    createdAtMillis: 1_930_000_000_000,
  }, {
    title: 'Persist exact artifacts',
    summary: 'Store immutable content and append-only metadata.',
    goals: [{ id: 'goal-store', text: 'A later stage reads exact prior facts.' }],
    nonGoals: [],
    constraints: [],
    acceptanceCriteria: [{
      criterionId: 'criterion-digest',
      statement: 'Changed content fails digest verification.',
      verification: 'Tamper with a stored blob and read it again.',
    }],
    repositoryFacts: [],
    risks: [],
    openQuestions: [],
  })
}

function candidate(suffix = 'one') {
  return Object.freeze({
    candidateId: CandidateId(`stored-candidate-${suffix}`),
    sourceSnapshotId: `source-sha256-${HASH_A}`,
    baseCommitId: '1'.repeat(40),
    baseTreeId: '2'.repeat(40),
    candidateCommitId: '3'.repeat(40),
    candidateTreeId: '4'.repeat(40),
    diffId: HASH_B,
  })
}

function patchArtifact(jobId, suffix = 'one') {
  const requirementId = RequirementId(`stored-patch-requirement-${suffix}`)
  const solutionId = SolutionId(`stored-patch-solution-${suffix}`)
  const architectureId = DiagramId(`stored-patch-architecture-${suffix}`)
  const processId = DiagramId(`stored-patch-process-${suffix}`)
  const reviewId = HumanReviewId(`stored-patch-review-${suffix}`)
  const planId = ExecutionPlanId(`stored-patch-plan-${suffix}`)
  return materializeStrongFlowArtifact('PATCH_MANIFEST', {
    artifactId: PatchManifestId(`stored-patch-${suffix}`),
    jobId,
    sourceArtifacts: [
      { artifactKind: 'REQUIREMENT_SPEC', artifactId: requirementId },
      { artifactKind: 'SOLUTION_DESIGN', artifactId: solutionId },
      { artifactKind: 'SYSTEM_ARCHITECTURE_DIAGRAM', artifactId: architectureId },
      { artifactKind: 'PROCESS_FLOW_DIAGRAM', artifactId: processId },
      { artifactKind: 'HUMAN_REVIEW_RECORD', artifactId: reviewId },
      { artifactKind: 'EXECUTION_PLAN', artifactId: planId },
    ],
    producer: {
      kind: 'role',
      roleId: 'executor',
      stageRunId: StageRunId(`stored-stage-executor-${suffix}`),
      attemptId: AttemptId(`stored-attempt-executor-${suffix}`),
    },
    kernelEventInterval: interval(`executor-${suffix}`),
    createdAtMillis: 1_930_000_000_100,
  }, {
    executionPlanId: planId,
    candidate: candidate(suffix),
    remediationRequestId: null,
    changedFiles: [],
    commands: [],
    tests: [],
  })
}

function evidenceProducer(artifact) {
  assert.equal(artifact.producer.kind, 'role')
  return Object.freeze({
    roleId: artifact.producer.roleId,
    stageRunId: artifact.producer.stageRunId,
    attemptId: artifact.producer.attemptId,
    eventInterval: artifact.kernelEventInterval,
  })
}

function directEvidence(jobId, evidenceId, producer, options = {}) {
  return {
    jobId,
    evidenceId,
    evidenceKind: options.evidenceKind ?? 'command',
    content: options.content ?? Buffer.from(`direct evidence ${evidenceId}`, 'utf8'),
    mediaType: options.mediaType ?? 'text/plain; charset=utf-8',
    producer,
    candidate: options.candidate ?? null,
    createdAtMillis: options.createdAtMillis ?? 1_930_000_000_200,
    command: options.command === undefined
      ? { commandId: `command-${evidenceId}`, exitCode: 0 }
      : options.command,
  }
}

function expectStoreError(code) {
  return error => error instanceof StrongFlowArtifactStoreError && error.code === code
}

function blobFile(store, blobId) {
  const digest = blobId.slice('sha256-'.length)
  return join(store.blobsDirectory, digest.slice(0, 2), `${digest}.blob`)
}

async function countPublishedBlobs(store) {
  let count = 0
  for (const shard of await readdir(store.blobsDirectory, { withFileTypes: true })) {
    if (!shard.isDirectory()) continue
    for (const entry of await readdir(join(store.blobsDirectory, shard.name), {
      withFileTypes: true,
    })) {
      if (entry.isFile() && entry.name.endsWith('.blob') && !entry.name.startsWith('.')) count += 1
    }
  }
  return count
}

async function treeContainsBytes(root, wanted) {
  for (const entry of await readdir(root, { withFileTypes: true })) {
    const path = join(root, entry.name)
    if (entry.isDirectory()) {
      if (await treeContainsBytes(path, wanted)) return true
    } else if (entry.isFile() && (await readFile(path)).includes(wanted)) {
      return true
    }
  }
  return false
}

test('publishes, reopens, and idempotently reads one canonical artifact', async t => {
  const home = await temporaryHome(t, 'artifact')
  const jobId = JobId('artifact-store-job-artifact')
  const store = await StrongFlowArtifactStore.create({
    home,
    jobId,
    createdAtMillis: 1_930_000_000_000,
  })
  const artifact = requirementArtifact(jobId)

  const first = await store.publishArtifact(artifact)
  const duplicate = await store.publishArtifact(structuredClone(artifact))
  assert.equal(first.outcome, 'published')
  assert.equal(first.blobReused, false)
  assert.equal(duplicate.outcome, 'already-published')
  assert.equal(duplicate.blobReused, true)
  assert.equal(duplicate.record.recordId, first.record.recordId)
  assert.equal(first.record.producer.kind, 'role')
  assert.equal(
    first.record.producer.eventInterval.kernelSessionId,
    artifact.kernelEventInterval.kernelSessionId,
  )
  assert.equal(first.record.candidate, null)

  const read = await store.read(first.record.recordId)
  assert.equal(read.record.entryKind, 'artifact')
  assert.deepEqual(read.artifact, artifact)
  assert.ok(Object.isFrozen(read.artifact))
  assert.equal((await store.list({ limit: 10 })).records.length, 1)
  assert.deepEqual(
    (await store.findArtifact('REQUIREMENT_SPEC', artifact.artifactId))?.artifact,
    artifact,
  )

  const reopened = await StrongFlowArtifactStore.open(home, jobId)
  assert.deepEqual(await reopened.read(first.record.recordId), read)
  assert.deepEqual(reopened.manifest, store.manifest)
})

test('records full candidate links and keeps direct evidence distinct from model observations', async t => {
  const home = await temporaryHome(t, 'trust')
  const jobId = JobId('artifact-store-job-trust')
  const store = await StrongFlowArtifactStore.create({
    home,
    jobId,
    createdAtMillis: 1,
  })
  const requirement = requirementArtifact(jobId, 'trust')
  const patch = patchArtifact(jobId, 'trust')
  await store.publishArtifact(requirement)
  const patchReceipt = await store.publishArtifact(patch)
  assert.equal(patchReceipt.record.candidate.kind, 'complete')
  assert.deepEqual(patchReceipt.record.candidate.identity, patch.payload.candidate)

  const direct = await store.publishDirectEvidence(directEvidence(
    jobId,
    'evidence-direct-trust',
    evidenceProducer(patch),
    {
      candidate: patch.payload.candidate,
      content: Buffer.from('{"exitCode":0,"output":"ok"}', 'utf8'),
      mediaType: 'application/json; charset=utf-8',
    },
  ))
  const observationBytes = Buffer.from('The model observed one requirement risk.', 'utf8')
  const observation = await store.publishModelObservation({
    jobId,
    evidenceId: 'evidence-model-observation',
    evidenceKind: 'other',
    content: observationBytes,
    mediaType: 'text/plain; charset=utf-8',
    producer: evidenceProducer(requirement),
    candidate: null,
    createdAtMillis: 1_930_000_000_201,
    sourceArtifact: {
      kind: 'artifact',
      artifactKind: 'REQUIREMENT_SPEC',
      artifactId: requirement.artifactId,
    },
  })

  assert.equal(direct.record.entryKind, 'direct-command-evidence')
  assert.equal(direct.record.identity.trust, 'trusted-direct-command')
  assert.equal(direct.record.identity.sourceArtifact, null)
  assert.equal(direct.record.identity.command.exitCode, 0)
  assert.equal(observation.record.entryKind, 'model-observation')
  assert.equal(observation.record.identity.trust, 'model-observation')
  assert.deepEqual(observation.record.identity.sourceArtifact, {
    kind: 'artifact',
    artifactKind: 'REQUIREMENT_SPEC',
    artifactId: requirement.artifactId,
  })
  assert.equal(observation.record.identity.command, null)

  const firstRead = await store.read(observation.record.recordId)
  assert.deepEqual(Buffer.from(firstRead.content), observationBytes)
  firstRead.content[0] = 0
  const secondRead = await store.read(observation.record.recordId)
  assert.deepEqual(Buffer.from(secondRead.content), observationBytes)

  await assert.rejects(
    store.publishModelObservation({
      jobId,
      evidenceId: 'evidence-missing-source',
      evidenceKind: 'other',
      content: Buffer.from('unlinked observation'),
      mediaType: 'text/plain; charset=utf-8',
      producer: evidenceProducer(requirement),
      candidate: null,
      createdAtMillis: 1_930_000_000_202,
      sourceArtifact: {
        kind: 'artifact',
        artifactKind: 'REQUIREMENT_SPEC',
        artifactId: 'not-published',
      },
    }),
    expectStoreError('ENTRY_INVALID'),
  )
})

test('rejects credential material before artifact or evidence bytes become durable', async t => {
  const home = await temporaryHome(t, 'credential-gate')
  const jobId = JobId('artifact-store-job-credential-gate')
  const store = await StrongFlowArtifactStore.create({ home, jobId, createdAtMillis: 1 })
  const requirement = requirementArtifact(jobId, 'credential-gate')
  const secret = 'fixture-secret-that-must-never-be-published'
  const credentialArtifact = structuredClone(requirement)
  credentialArtifact.payload.summary = `Authorization: Bearer ${secret}`

  await assert.rejects(
    store.publishArtifact(credentialArtifact),
    error => {
      assert.ok(error instanceof StrongFlowArtifactStoreError)
      assert.equal(error.code, 'CREDENTIAL_MATERIAL_DENIED')
      assert.equal(error.message.includes(secret), false)
      return true
    },
  )
  assert.equal((await store.list({ limit: 10 })).records.length, 0)
  assert.equal(await countPublishedBlobs(store), 0)

  await store.publishArtifact(requirement)
  await assert.rejects(
    store.publishDirectEvidence(directEvidence(
      jobId,
      'evidence-credential-gate',
      evidenceProducer(requirement),
      { content: Buffer.from(`{"apiKey":"${secret}"}`, 'utf8') },
    )),
    expectStoreError('CREDENTIAL_MATERIAL_DENIED'),
  )
  await assert.rejects(
    store.publishModelObservation({
      jobId,
      evidenceId: 'observation-credential-gate',
      evidenceKind: 'other',
      content: Buffer.from(`PASSWORD=${secret}`, 'utf8'),
      mediaType: 'text/plain; charset=utf-8',
      producer: evidenceProducer(requirement),
      candidate: null,
      createdAtMillis: 1_930_000_000_203,
      sourceArtifact: {
        kind: 'artifact',
        artifactKind: 'REQUIREMENT_SPEC',
        artifactId: requirement.artifactId,
      },
    }),
    expectStoreError('CREDENTIAL_MATERIAL_DENIED'),
  )
  assert.equal((await store.list({ limit: 10 })).records.length, 1)
  assert.equal(await countPublishedBlobs(store), 1)
  assert.equal(await treeContainsBytes(home, Buffer.from(secret, 'utf8')), false)

  const redacted = await store.publishDirectEvidence(directEvidence(
    jobId,
    'evidence-redacted-credential',
    evidenceProducer(requirement),
    { content: Buffer.from('TOKEN=[REDACTED]', 'utf8') },
  ))
  assert.equal(redacted.outcome, 'published')
})

test('missing direct evidence fails closed without copying evidence bytes into diagnostics', async t => {
  const home = await temporaryHome(t, 'missing-direct-evidence')
  const jobId = JobId('artifact-store-job-missing-direct-evidence')
  const store = await StrongFlowArtifactStore.create({ home, jobId, createdAtMillis: 1 })
  const requirement = requirementArtifact(jobId, 'missing-direct-evidence')
  await store.publishArtifact(requirement)
  const evidenceBytes = 'fixture evidence bytes that must not enter an error'
  const receipt = await store.publishDirectEvidence(directEvidence(
    jobId,
    'evidence-missing-direct',
    evidenceProducer(requirement),
    { content: Buffer.from(evidenceBytes, 'utf8') },
  ))

  await unlink(blobFile(store, receipt.record.blob.blobId))
  const reopened = await StrongFlowArtifactStore.open(home, jobId)
  await assert.rejects(
    reopened.read(receipt.record.recordId),
    error => {
      assert.ok(error instanceof StrongFlowArtifactStoreError)
      assert.equal(error.code, 'CONTENT_MISSING')
      assert.doesNotMatch(error.message, new RegExp(evidenceBytes, 'u'))
      return true
    },
  )
})

test('shared bytes reuse one blob while records and listing stay inside each job', async t => {
  const home = await temporaryHome(t, 'ownership')
  const jobA = JobId('artifact-store-job-a')
  const jobB = JobId('artifact-store-job-b')
  const storeA = await StrongFlowArtifactStore.create({ home, jobId: jobA, createdAtMillis: 1 })
  const storeB = await StrongFlowArtifactStore.create({ home, jobId: jobB, createdAtMillis: 2 })
  const producerA = evidenceProducer(requirementArtifact(jobA, 'owner-a'))
  const producerB = evidenceProducer(requirementArtifact(jobB, 'owner-b'))
  const shared = Buffer.from('same trusted direct evidence bytes', 'utf8')
  const first = await storeA.publishDirectEvidence(directEvidence(
    jobA,
    'evidence-owner-a',
    producerA,
    { content: shared, evidenceKind: 'diff', command: null },
  ))
  const second = await storeB.publishDirectEvidence(directEvidence(
    jobB,
    'evidence-owner-b',
    producerB,
    { content: shared, evidenceKind: 'diff', command: null },
  ))
  assert.equal(first.record.blob.blobId, second.record.blob.blobId)
  assert.equal(first.blobReused, false)
  assert.equal(second.blobReused, true)
  assert.equal(await countPublishedBlobs(storeA), 1)
  assert.deepEqual(
    (await storeA.list({ limit: 10 })).records.map(record => record.identity.evidenceId),
    ['evidence-owner-a'],
  )
  assert.deepEqual(
    (await storeB.list({ limit: 10 })).records.map(record => record.identity.evidenceId),
    ['evidence-owner-b'],
  )
  await assert.rejects(
    storeB.read(first.record.recordId),
    expectStoreError('RECORD_NOT_FOUND'),
  )
})

test('digest changes, missing blobs, and metadata edits fail closed', async t => {
  const home = await temporaryHome(t, 'tamper')
  const jobId = JobId('artifact-store-job-tamper')
  const store = await StrongFlowArtifactStore.create({ home, jobId, createdAtMillis: 1 })
  const artifact = requirementArtifact(jobId, 'tamper')
  const receipt = await store.publishArtifact(artifact)
  const path = blobFile(store, receipt.record.blob.blobId)
  const original = await readFile(path)
  await writeFile(path, Buffer.alloc(original.length, 0x78))
  await assert.rejects(
    store.read(receipt.record.recordId),
    expectStoreError('CONTENT_DIGEST_MISMATCH'),
  )
  await writeFile(path, original)
  await unlink(path)
  await assert.rejects(
    store.read(receipt.record.recordId),
    expectStoreError('CONTENT_MISSING'),
  )

  const metadataHome = await temporaryHome(t, 'metadata-tamper')
  const metadataJob = JobId('artifact-store-job-metadata-tamper')
  const metadataStore = await StrongFlowArtifactStore.create({
    home: metadataHome,
    jobId: metadataJob,
    createdAtMillis: 1,
  })
  await metadataStore.publishArtifact(requirementArtifact(metadataJob, 'metadata-tamper'))
  const recordPath = join(metadataStore.recordsDirectory, '1.json')
  const recordValue = JSON.parse(await readFile(recordPath, 'utf8'))
  recordValue.createdAtMillis += 1
  await writeFile(recordPath, `${JSON.stringify(recordValue)}\n`)
  await assert.rejects(
    metadataStore.list({ limit: 10 }),
    expectStoreError('STORE_CORRUPT'),
  )
})

test('pending interrupted files are ignored and a published partial record is corruption', async t => {
  const home = await temporaryHome(t, 'interrupted')
  const jobId = JobId('artifact-store-job-interrupted')
  const store = await StrongFlowArtifactStore.create({ home, jobId, createdAtMillis: 1 })
  await store.publishArtifact(requirementArtifact(jobId, 'interrupted'))
  await writeFile(
    join(store.recordsDirectory, `.pending-2-${randomUUID()}.json`),
    '{"partial":',
  )
  const pendingDigest = HASH_C
  const pendingName = `.pending-${pendingDigest}-${randomUUID()}.blob`
  assert.equal(isPendingStrongFlowArtifactBlobFile(pendingName), true)
  const pendingShard = join(store.blobsDirectory, pendingDigest.slice(0, 2))
  await mkdir(pendingShard, { recursive: true })
  await writeFile(join(pendingShard, pendingName), Buffer.from('partial'))
  assert.equal((await store.list({ limit: 10 })).records.length, 1)

  await writeFile(join(store.recordsDirectory, '2.json'), '{"partial":')
  await assert.rejects(
    store.list({ limit: 10 }),
    expectStoreError('STORE_CORRUPT'),
  )
})

test('identity conflicts fail and bounded attempt listing pages an append-only chain', async t => {
  const home = await temporaryHome(t, 'listing')
  const jobId = JobId('artifact-store-job-listing')
  const store = await StrongFlowArtifactStore.create({ home, jobId, createdAtMillis: 1 })
  const requirement = requirementArtifact(jobId, 'listing')
  await store.publishArtifact(requirement)
  const changed = structuredClone(requirement)
  changed.payload.summary = 'Different content under the same immutable artifact id.'
  await assert.rejects(
    store.publishArtifact(changed),
    expectStoreError('IDENTITY_CONFLICT'),
  )

  const producer = evidenceProducer(requirement)
  for (let index = 0; index < 5; index += 1) {
    const attemptId = AttemptId(`stored-list-attempt-${index % 2}`)
    await store.publishDirectEvidence(directEvidence(
      jobId,
      `stored-list-evidence-${index}`,
      { ...producer, attemptId },
      { createdAtMillis: 1_930_000_000_300 + index },
    ))
  }
  await assert.rejects(
    store.publishDirectEvidence(directEvidence(
      jobId,
      'stored-list-evidence-0',
      producer,
      { content: Buffer.from('different bytes') },
    )),
    expectStoreError('IDENTITY_CONFLICT'),
  )

  const firstPage = await store.list({ limit: 2 })
  assert.equal(firstPage.records.length, 2)
  assert.equal(firstPage.nextAfterSequence, '2')
  const secondPage = await store.list({ limit: 2, afterSequence: '2' })
  assert.deepEqual(secondPage.records.map(record => record.sequence), ['3', '4'])
  const filtered = await store.list({
    limit: 10,
    attemptId: AttemptId('stored-list-attempt-1'),
    entryKinds: ['direct-command-evidence'],
  })
  assert.deepEqual(
    filtered.records.map(record => record.identity.evidenceId),
    ['stored-list-evidence-1', 'stored-list-evidence-3'],
  )
  await assert.rejects(store.list({ limit: 0 }), expectStoreError('ENTRY_INVALID'))
  await assert.rejects(
    store.list({ limit: 10, extra: true }),
    expectStoreError('ENTRY_INVALID'),
  )
})

test('competing store instances publish complete consecutive metadata records', async t => {
  const home = await temporaryHome(t, 'concurrent')
  const jobId = JobId('artifact-store-job-concurrent')
  const first = await StrongFlowArtifactStore.create({ home, jobId, createdAtMillis: 1 })
  const second = await StrongFlowArtifactStore.open(home, jobId)
  const producer = evidenceProducer(requirementArtifact(jobId, 'concurrent'))
  const shared = Buffer.from('concurrent shared content', 'utf8')
  const receipts = await Promise.all(Array.from({ length: 12 }, (_, index) => {
    const store = index % 2 === 0 ? first : second
    return store.publishDirectEvidence(directEvidence(
      jobId,
      `concurrent-evidence-${index}`,
      producer,
      {
        content: shared,
        evidenceKind: 'log',
        command: null,
        createdAtMillis: 1_930_000_001_000 + index,
      },
    ))
  }))
  assert.equal(receipts.length, 12)
  const reopened = await StrongFlowArtifactStore.open(home, jobId)
  const listed = await reopened.list({ limit: 100 })
  assert.deepEqual(
    listed.records.map(record => record.sequence),
    Array.from({ length: 12 }, (_, index) => String(index + 1)),
  )
  assert.equal(new Set(listed.records.map(record => record.identity.evidenceId)).size, 12)
  assert.equal(await countPublishedBlobs(reopened), 1)
})
