import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  RUNTIME_EVENT_SCHEMA_VERSION,
  deliveryIdForGitHubIssueSource,
  parseDelivery,
  parseStrongFlowGitHubPublicationContextText,
  parseStrongFlowPlanReviewContextText,
} from '../packages/contracts/dist/index.js'
import { CodexRuntimeProjector } from '../packages/dsh-profile/dist/index.js'
import {
  DeliveryStore,
  StrongFlowGitHubReviewPackageError,
  StrongFlowService,
  createStrongFlowGitHubPublicationAttention,
  createStrongFlowGitHubPublicationDecision,
  createStrongFlowPlanReviewAttention,
  createStrongFlowPlanReviewDecision,
  freezeDeliveryCandidate,
  generateStrongFlowGitHubReviewPackage,
  readStrongFlowGitHubReviewPackage,
  runStrongFlowGitHubPublication,
  verifyStrongFlowGitHubReviewPackage,
  writeStrongFlowGitHubReviewPackage,
} from '../packages/strongflow/dist/index.js'

const baseTime = 2_500_000_000_000
const sourceRef = Object.freeze({
  schemaVersion: DELIVERY_SCHEMA_VERSION,
  provider: 'github',
  kind: 'issue',
  repository: 'example/widget',
  number: 42,
})
const publicationTarget = Object.freeze({
  schemaVersion: DELIVERY_SCHEMA_VERSION,
  provider: 'github',
  kind: 'pull-request',
  repository: 'example/widget',
  baseBranch: 'main',
  headRepository: 'example/widget',
  headBranch: 'winwincode/issue-42',
})
const deliveryId = deliveryIdForGitHubIssueSource(sourceRef)
const specId = `${deliveryId}:spec:1`
const criterionId = `${deliveryId}:criterion:1`
const taskId = `${deliveryId}:task:1`
const planStageId = `${deliveryId}:stage:plan:1`
const planBindingId = `${deliveryId}:binding:plan:1`
const planReviewStageId = `${deliveryId}:stage:plan-review:1`
const planAttentionId = `${deliveryId}:attention:plan-review:1`
const executorStageId = `${deliveryId}:stage:execute:1`
const executorBindingId = `${deliveryId}:binding:execute:1`
const reviewerStageId = `${deliveryId}:stage:reviewer:1`
const reviewerBindingId = `${deliveryId}:binding:reviewer:1`
const verifierStageId = `${deliveryId}:stage:verifier:1`
const verifierBindingId = `${deliveryId}:binding:verifier:1`
const publicationReviewStageId = `${deliveryId}:stage:delivery-review:1`
const publicationAttentionId = `${deliveryId}:attention:publication:1`
const candidateDiff = [
  'diff --git a/src/value.ts b/src/value.ts',
  'index 1111111..5555555 100644',
  '--- a/src/value.ts',
  '+++ b/src/value.ts',
  '@@ -1 +1 @@',
  '-old',
  '+new',
  '',
].join('\n')

const authenticator = Object.freeze({
  async authenticate(request) {
    return request.channel === 'local-ui'
      && request.authentication.scheme === 'local-session'
      && request.authentication.proof === 'review-package-proof'
      ? Object.freeze({ actorId: 'human-reviewer' })
      : undefined
  },
})

function spec() {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: specId,
    deliveryId,
    revision: 1,
    title: 'Deliver one reviewed GitHub change',
    goal: 'Produce one frozen candidate whose direct checks pass.',
    scope: ['Change the bounded value module'],
    outOfScope: ['Generic issue tracking'],
    constraints: ['Codex Core remains the execution authority'],
    acceptanceCriteria: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: criterionId,
      description: 'The value module passes its direct check.',
      verificationMethod: 'Run the direct test and review the diff.',
      required: true,
    }],
    sourceRef,
    publicationTarget,
    repository: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      kind: 'github',
      locator: 'example/widget',
    },
    baseRevision: '1'.repeat(40),
    maxReworkAttempts: 2,
    createdAtMillis: baseTime,
  }
}

function task() {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: taskId,
    deliveryId,
    title: 'Value module change',
    goal: 'Produce one independently verifiable candidate.',
    acceptanceCriterionIds: [criterionId],
    blockedByTaskIds: [],
    owner: 'executor',
    status: 'pending',
  }
}

function solution() {
  return {
    id: `${deliveryId}:solution:1`,
    summary: 'Change the bounded value module and verify the frozen result.',
    approach: ['Change the value.', 'Review and test the frozen candidate.'],
    components: [{
      id: `${deliveryId}:component:value`,
      label: 'Value module',
      responsibility: 'Own the value covered by the acceptance criterion.',
      kind: 'component',
      trustBoundary: 'Repository application',
      unresolved: false,
      repositoryPathPrefixes: ['src'],
    }],
    connections: [{
      id: `${deliveryId}:connection:value`,
      from: 'platform:codex-core',
      to: `${deliveryId}:component:value`,
      label: 'Implements the approved change',
    }],
  }
}

function kernelEvent(sequence, type, data = {}) {
  const payload = { id: `submission-${sequence}`, msg: { type, ...data } }
  return Object.freeze({
    sequence: BigInt(sequence),
    kind: type,
    payload,
    rawJson: JSON.stringify(payload),
  })
}

function readOnlySessionConfiguration() {
  return {
    approval_policy: 'on-request',
    approvals_reviewer: 'user',
    permission_profile: {
      type: 'managed',
      file_system: {
        type: 'restricted',
        entries: [{ path: { type: 'special', value: { kind: 'root' } }, access: 'read' }],
      },
      network: 'restricted',
    },
  }
}

function verificationEvents(delivery, candidate) {
  return delivery.stageRuns
    .filter(run => run.stage === 'verifying')
    .flatMap((run) => {
      const binding = delivery.sessionBindings.find(entry => entry.stageRunId === run.id)
      assert.notEqual(binding, undefined)
      assert.notEqual(binding.dshSessionId, null)
      assert.notEqual(binding.codexSessionId, null)
      const evidenceType = run.role === 'reviewer' ? 'command' : 'test'
      return new CodexRuntimeProjector({
        sessionId: binding.dshSessionId,
        kernelSessionId: binding.codexSessionId,
        roleId: run.role,
        kernelStreamId: `stream-${run.role}`,
      }).replay([
        kernelEvent(1, 'session_configured', {
          session_id: binding.codexSessionId,
          thread_id: binding.codexSessionId,
          occurred_at_ms: binding.boundAtMillis,
          ...readOnlySessionConfiguration(),
        }),
        kernelEvent(2, 'task_started', {
          turn_id: `turn-${run.role}`,
          started_at_ms: binding.boundAtMillis,
        }),
        kernelEvent(3, 'item_completed', {
          turn_id: `turn-${run.role}`,
          completed_at_ms: binding.boundAtMillis,
          item: {
            type: 'CommandExecution',
            id: `check-${run.role}`,
            command: run.role === 'reviewer'
              ? ['git', 'diff', '--check']
              : ['pnpm', 'test'],
            status: 'completed',
            exit_code: 0,
          },
        }),
        kernelEvent(4, 'agent_message', {
          turn_id: `turn-${run.role}`,
          occurred_at_ms: binding.boundAtMillis,
          phase: 'final_answer',
          message: JSON.stringify({
            protocol: 'winwincode.independent-verification-result.v1',
            delivery_spec_id: delivery.spec.id,
            delivery_spec_revision: delivery.spec.revision,
            candidate_ref: candidate.candidateRef,
            findings: [{
              finding_id: `finding-${run.role}`,
              criterion_id: criterionId,
              verdict: 'pass',
              explanation: `${run.role} checked the current frozen candidate.`,
              evidence_sources: [{
                type: evidenceType,
                event_id: `${binding.dshSessionId}@3`,
              }],
            }],
          }),
        }),
        kernelEvent(5, 'task_complete', {
          turn_id: `turn-${run.role}`,
          completed_at_ms: binding.boundAtMillis,
          last_agent_message: `${run.role} complete`,
          error: null,
        }),
      ])
    })
}

function diffEvent(delivery, candidate) {
  const run = delivery.stageRuns.find(entry => entry.id === executorStageId)
  const binding = delivery.sessionBindings.find(entry => entry.id === executorBindingId)
  assert.notEqual(run, undefined)
  assert.notEqual(binding, undefined)
  return Object.freeze({
    schemaVersion: RUNTIME_EVENT_SCHEMA_VERSION,
    id: `${binding.dshSessionId}@1`,
    cursor: Object.freeze({ sessionId: binding.dshSessionId, sequence: '1' }),
    kind: 'diff.updated',
    source: Object.freeze({
      authority: 'codex-core',
      sessionId: binding.dshSessionId,
      kernelSessionId: binding.codexSessionId,
      roleId: run.role,
      kernelStreamId: 'stream-executor',
      kernelSequence: '1',
      submissionId: 'submission-executor',
      kernelKind: 'diff_updated',
    }),
    occurredAtMillis: run.finishedAtMillis,
    data: Object.freeze({
      unified_diff: candidateDiff,
      frozen_candidate: candidate,
    }),
  })
}

async function reviewFixture(t) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-github-review-package-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  await DeliveryStore.create({
    home,
    requestId: 'seed-review-package',
    requestDigest: '7'.repeat(64),
    snapshot: parseDelivery({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: deliveryId,
      revision: 1,
      status: 'ready',
      spec: spec(),
      tasks: [task()],
      stageRuns: [],
      sessionBindings: [],
      attentionItems: [],
      evidence: [],
      verdict: null,
      createdAtMillis: baseTime,
      updatedAtMillis: baseTime,
    }),
  })
  let now = baseTime + 100
  const service = new StrongFlowService({
    home,
    authenticator,
    clock: () => ++now,
  })
  const planning = await service.startStage({
    requestId: 'start-planning',
    deliveryId,
    expectedRevision: 1,
    stageRunId: planStageId,
    deliveryTaskId: null,
    stage: 'planning',
    actorType: 'codex',
    role: 'planner',
    attention: null,
  })
  const boundPlanning = await service.bindSession({
    requestId: 'bind-planning',
    deliveryId,
    expectedRevision: planning.revision,
    bindingId: planBindingId,
    stageRunId: planStageId,
    dshSessionId: 'dsh-plan',
    codexSessionId: 'codex-plan',
  })
  const planAttention = createStrongFlowPlanReviewAttention({
    delivery: boundPlanning,
    attentionItemId: planAttentionId,
    reviewStageRunId: planReviewStageId,
    assignedTo: 'human-reviewer',
    solution: solution(),
    risks: ['The package must remain local until publication approval.'],
    unresolvedItems: [],
    preparedAtMillis: boundPlanning.updatedAtMillis,
  })
  const planReview = await service.startStage({
    requestId: 'start-plan-review',
    deliveryId,
    expectedRevision: boundPlanning.revision,
    stageRunId: planReviewStageId,
    deliveryTaskId: null,
    stage: 'plan-review',
    actorType: 'human',
    role: 'reviewer',
    attention: planAttention,
  })
  const boundPlanReview = await service.bindSession({
    requestId: 'bind-plan-review',
    deliveryId,
    expectedRevision: planReview.revision,
    bindingId: `${deliveryId}:binding:plan-review:1`,
    stageRunId: planReviewStageId,
    dshSessionId: 'dsh-plan-review',
    codexSessionId: null,
  })
  const planDecision = createStrongFlowPlanReviewDecision({
    context: parseStrongFlowPlanReviewContextText(planAttention.context),
    action: 'approve',
    comments: 'Approved the exact solution and diagram set.',
    requestedChanges: [],
  })
  const approvedPlan = await service.resolveAttention({
    requestId: 'approve-plan-review',
    deliveryId,
    expectedRevision: boundPlanReview.revision,
    attentionItemId: planAttention.id,
    status: 'resolved',
    resolution: JSON.stringify(planDecision),
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'review-package-proof' },
  })
  const executing = await service.startStage({
    requestId: 'start-execution',
    deliveryId,
    expectedRevision: approvedPlan.revision,
    stageRunId: executorStageId,
    deliveryTaskId: taskId,
    stage: 'executing',
    actorType: 'codex',
    role: 'executor',
    attention: null,
  })
  const boundExecutor = await service.bindSession({
    requestId: 'bind-execution',
    deliveryId,
    expectedRevision: executing.revision,
    bindingId: executorBindingId,
    stageRunId: executorStageId,
    dshSessionId: 'dsh-executor',
    codexSessionId: 'codex-executor',
  })
  const reviewing = await service.startStage({
    requestId: 'start-reviewer',
    deliveryId,
    expectedRevision: boundExecutor.revision,
    stageRunId: reviewerStageId,
    deliveryTaskId: taskId,
    stage: 'verifying',
    actorType: 'codex',
    role: 'reviewer',
    attention: null,
  })
  const boundReviewer = await service.bindSession({
    requestId: 'bind-reviewer',
    deliveryId,
    expectedRevision: reviewing.revision,
    bindingId: reviewerBindingId,
    stageRunId: reviewerStageId,
    dshSessionId: 'dsh-reviewer',
    codexSessionId: 'codex-reviewer',
  })
  const verifying = await service.startStage({
    requestId: 'start-verifier',
    deliveryId,
    expectedRevision: boundReviewer.revision,
    stageRunId: verifierStageId,
    deliveryTaskId: taskId,
    stage: 'verifying',
    actorType: 'codex',
    role: 'verifier',
    attention: null,
  })
  const boundVerifier = await service.bindSession({
    requestId: 'bind-verifier',
    deliveryId,
    expectedRevision: verifying.revision,
    bindingId: verifierBindingId,
    stageRunId: verifierStageId,
    dshSessionId: 'dsh-verifier',
    codexSessionId: 'codex-verifier',
  })
  const candidate = freezeDeliveryCandidate(boundVerifier, {
    producerStageRunId: executorStageId,
    producerSessionBindingId: executorBindingId,
    baseCommitId: boundVerifier.spec.baseRevision,
    baseTreeId: '2'.repeat(40),
    candidateCommitId: '3'.repeat(40),
    candidateTreeId: '4'.repeat(40),
    diffSha256: createHash('sha256').update(candidateDiff).digest('hex'),
    changedPaths: [{
      path: 'src/value.ts',
      state: 'present',
      objectId: '5'.repeat(40),
    }],
  })
  const evidenceEvents = verificationEvents(boundVerifier, candidate)
  const readyToDeliver = await service.submitVerdict({
    requestId: 'submit-verdict',
    deliveryId,
    expectedRevision: boundVerifier.revision,
    candidate,
    runtimeEvents: evidenceEvents,
    requiredRoles: ['reviewer', 'verifier'],
  })
  const publicationAttention = createStrongFlowGitHubPublicationAttention({
    delivery: readyToDeliver,
    candidate,
    attentionItemId: publicationAttentionId,
    reviewStageRunId: publicationReviewStageId,
    assignedTo: 'human-reviewer',
    preparedAtMillis: readyToDeliver.updatedAtMillis,
  })
  const publicationReview = await service.startStage({
    requestId: 'start-publication-review',
    deliveryId,
    expectedRevision: readyToDeliver.revision,
    stageRunId: publicationReviewStageId,
    deliveryTaskId: null,
    stage: 'delivery-review',
    actorType: 'human',
    role: 'approver',
    attention: publicationAttention,
  })
  const delivery = await service.bindSession({
    requestId: 'bind-publication-review',
    deliveryId,
    expectedRevision: publicationReview.revision,
    bindingId: `${deliveryId}:binding:delivery-review:1`,
    stageRunId: publicationReviewStageId,
    dshSessionId: 'dsh-publication-review',
    codexSessionId: null,
  })
  return Object.freeze({
    home,
    service,
    delivery,
    candidate,
    runtimeEvents: Object.freeze([diffEvent(delivery, candidate), ...evidenceEvents]),
  })
}

function packageInput(fixture) {
  return {
    delivery: fixture.delivery,
    candidate: fixture.candidate,
    publicationAttentionItemId: publicationAttentionId,
    runtimeEvents: fixture.runtimeEvents,
  }
}

async function approvedPublicationFixture(t) {
  const fixture = await reviewFixture(t)
  const reviewPackage = generateStrongFlowGitHubReviewPackage(packageInput(fixture))
  const attention = fixture.delivery.attentionItems.find(item => (
    item.id === publicationAttentionId
  ))
  assert.notEqual(attention, undefined)
  const decision = createStrongFlowGitHubPublicationDecision({
    context: parseStrongFlowGitHubPublicationContextText(attention.context),
    comments: 'Approved the exact package, candidate, verdict, and destination.',
  })
  const delivery = await fixture.service.resolveAttention({
    requestId: 'approve-publication',
    deliveryId,
    expectedRevision: fixture.delivery.revision,
    attentionItemId: attention.id,
    status: 'resolved',
    resolution: JSON.stringify(decision),
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'review-package-proof' },
  })
  return Object.freeze({ ...fixture, delivery, reviewPackage })
}

class FixtureGitHubProvider {
  resources = new Map()
  lookupCalls = []
  applyCalls = []
  remoteWrites = []
  lookupUnknown = new Set()
  lookupThrowsOnce = new Set()
  applyThrowsAfterWriteOnce = new Set()
  rejected = new Set()

  async lookup(operation) {
    this.lookupCalls.push(operation.kind)
    if (this.lookupThrowsOnce.delete(operation.kind)) throw new Error('fixture lookup interruption')
    if (this.lookupUnknown.has(operation.kind)) {
      return {
        state: 'unknown',
        operationKey: operation.operationKey,
        code: 'fixture-lookup-unknown',
      }
    }
    const resource = this.resources.get(operation.operationKey)
    return resource === undefined
      ? { state: 'absent', operationKey: operation.operationKey }
      : {
          state: 'found',
          operationKey: operation.operationKey,
          requestSha256: resource.requestSha256,
          resourceRef: resource.resourceRef,
        }
  }

  async apply(operation) {
    this.applyCalls.push(operation.kind)
    if (this.rejected.has(operation.kind)) {
      return {
        state: 'rejected',
        operationKey: operation.operationKey,
        code: 'fixture-rejected',
      }
    }
    const existing = this.resources.get(operation.operationKey)
    const resource = {
      requestSha256: operation.requestSha256,
      resourceRef: `github:${operation.kind}:${operation.operationKey}`,
    }
    this.resources.set(operation.operationKey, resource)
    if (existing?.requestSha256 !== operation.requestSha256) {
      this.remoteWrites.push(operation.kind)
    }
    if (this.applyThrowsAfterWriteOnce.delete(operation.kind)) {
      throw new Error('fixture response lost after write')
    }
    return {
      state: 'applied',
      operationKey: operation.operationKey,
      requestSha256: operation.requestSha256,
      resourceRef: resource.resourceRef,
      remoteWritePerformed: existing?.requestSha256 !== operation.requestSha256,
    }
  }
}

function rehashPackage(reviewPackage) {
  const files = reviewPackage.files.toSorted((left, right) => left.path.localeCompare(right.path))
  const metadata = files.map(entry => ({
    path: entry.path,
    mediaType: entry.mediaType,
    sha256: createHash('sha256').update(entry.content).digest('hex'),
    bytes: Buffer.byteLength(entry.content),
  }))
  const { packageId: _oldPackageId, ...oldUnsigned } = reviewPackage.manifest
  const unsigned = { ...oldUnsigned, files: metadata }
  const packageId = `github-review-package:sha256:${createHash('sha256')
    .update(`${JSON.stringify(unsigned, null, 2)}\n`)
    .digest('hex')}`
  return {
    ...reviewPackage,
    manifest: { ...unsigned, packageId },
    files,
  }
}

test('review package generation is deterministic, separated, hash-bound, and zero-write', async t => {
  const fixture = await reviewFixture(t)
  let remoteWrites = 0
  const input = {
    ...packageInput(fixture),
    publish() { remoteWrites += 1 },
  }
  const first = generateStrongFlowGitHubReviewPackage(input)
  const second = generateStrongFlowGitHubReviewPackage(input)
  assert.deepEqual(second, first)
  assert.equal(remoteWrites, 0)
  assert.equal(first.manifest.dryRun.publicationOccurred, false)
  assert.equal(first.manifest.dryRun.remoteWriteCount, 0)
  assert.equal(first.files.some(entry => entry.path === 'requirements/delivery-spec.json'), true)
  assert.equal(first.files.some(entry => entry.path === 'solution/solution.json'), true)
  assert.equal(first.files.some(entry => entry.path === 'diagrams/system-architecture.json'), true)
  assert.equal(first.files.some(entry => entry.path === 'diagrams/process-flow.json'), true)
  assert.equal(first.files.some(entry => entry.path === 'candidate/candidate.diff'), true)
  assert.equal(first.files.some(entry => entry.path === 'verification/evidence-refs.json'), true)
  assert.equal(first.files.some(entry => entry.path === 'verification/delivery-verdict.json'), true)
  assert.equal(first.files.some(entry => entry.path === 'github/pull-request-preview.md'), true)
  assert.match(first.preview.body, /Closes https:\/\/github\.com\/example\/widget\/issues\/42/u)
  assert.match(first.preview.body, /winwincode-publication:github:pull-request:sha256:/u)
  assert.equal(JSON.stringify(first).includes('rawJson'), false)
  for (const metadata of first.manifest.files) {
    const entry = first.files.find(candidate => candidate.path === metadata.path)
    assert.notEqual(entry, undefined)
    assert.equal(metadata.sha256, createHash('sha256').update(entry.content).digest('hex'))
    assert.equal(metadata.bytes, Buffer.byteLength(entry.content))
  }
  assert.deepEqual(verifyStrongFlowGitHubReviewPackage(first), first)
})

test('review package generation rejects missing owning Diff and verification facts', async t => {
  const fixture = await reviewFixture(t)
  const withoutDiff = fixture.runtimeEvents.filter(event => event.kind !== 'diff.updated')
  assert.throws(
    () => generateStrongFlowGitHubReviewPackage({
      ...packageInput(fixture),
      runtimeEvents: withoutDiff,
    }),
    error => error instanceof StrongFlowGitHubReviewPackageError
      && error.code === 'CANDIDATE_DIFF_INVALID',
  )
  const referencedEvidenceEventId = fixture.delivery.evidence[0].sourceRef
    .slice('runtime_event:'.length)
  const referencedSessionId = referencedEvidenceEventId.slice(
    0,
    referencedEvidenceEventId.lastIndexOf('@'),
  )
  assert.throws(
    () => generateStrongFlowGitHubReviewPackage({
      ...packageInput(fixture),
      runtimeEvents: fixture.runtimeEvents.filter(event => (
        event.source.sessionId !== referencedSessionId
      )),
    }),
    error => error instanceof StrongFlowGitHubReviewPackageError
      && error.code === 'EVIDENCE_UNRESOLVED',
  )
})

test('review package generation rejects stale plan and publication review sets', async t => {
  const fixture = await reviewFixture(t)
  const stalePlan = parseDelivery({
    ...fixture.delivery,
    attentionItems: fixture.delivery.attentionItems.map((attention) => {
      if (attention.id !== planAttentionId) return attention
      const context = JSON.parse(attention.context)
      return {
        ...attention,
        context: JSON.stringify({ ...context, risks: [...context.risks, 'Late risk'] }),
      }
    }),
  })
  assert.throws(
    () => generateStrongFlowGitHubReviewPackage({
      ...packageInput(fixture),
      delivery: stalePlan,
    }),
    error => error instanceof StrongFlowGitHubReviewPackageError
      && error.code === 'PLAN_REVIEW_STALE',
  )

  const stalePublication = parseDelivery({
    ...fixture.delivery,
    attentionItems: fixture.delivery.attentionItems.map((attention) => {
      if (attention.id !== publicationAttentionId) return attention
      const context = JSON.parse(attention.context)
      return {
        ...attention,
        context: JSON.stringify({ ...context, publicationSetSha256: '0'.repeat(64) }),
      }
    }),
  })
  assert.throws(
    () => generateStrongFlowGitHubReviewPackage({
      ...packageInput(fixture),
      delivery: stalePublication,
    }),
    error => error instanceof StrongFlowGitHubReviewPackageError
      && error.code === 'PUBLICATION_REVIEW_STALE',
  )
})

test('offline verification rejects changed bytes and internally rehashed cross-file drift', async t => {
  const fixture = await reviewFixture(t)
  const generated = generateStrongFlowGitHubReviewPackage(packageInput(fixture))
  const changedDiff = {
    ...generated,
    files: generated.files.map(entry => entry.path === 'candidate/candidate.diff'
      ? { ...entry, content: `${entry.content}+unreviewed\n` }
      : entry),
  }
  assert.throws(
    () => verifyStrongFlowGitHubReviewPackage(changedDiff),
    error => error instanceof StrongFlowGitHubReviewPackageError
      && error.code === 'PACKAGE_INVALID',
  )

  const changedSolution = rehashPackage({
    ...generated,
    files: generated.files.map((entry) => {
      if (entry.path !== 'solution/solution.json') return entry
      const value = JSON.parse(entry.content)
      return {
        ...entry,
        content: `${JSON.stringify({ ...value, summary: 'Changed after approval.' }, null, 2)}\n`,
      }
    }),
  })
  assert.throws(
    () => verifyStrongFlowGitHubReviewPackage(changedSolution),
    error => error instanceof StrongFlowGitHubReviewPackageError
      && error.code === 'PACKAGE_INVALID',
  )
})

test('local package writes atomically, reuses identical output, and rejects conflicts', async t => {
  const fixture = await reviewFixture(t)
  const generated = generateStrongFlowGitHubReviewPackage(packageInput(fixture))
  const output = join(fixture.home, 'review-packages', generated.manifest.packageId)
  const first = await writeStrongFlowGitHubReviewPackage({
    outputDirectory: output,
    reviewPackage: generated,
  })
  assert.equal(first.reused, false)
  assert.deepEqual(await readStrongFlowGitHubReviewPackage(output), generated)
  const second = await writeStrongFlowGitHubReviewPackage({
    outputDirectory: output,
    reviewPackage: generated,
  })
  assert.equal(second.reused, true)

  const conflict = join(fixture.home, 'review-packages', 'occupied')
  await mkdir(conflict, { recursive: true })
  await writeFile(join(conflict, 'unrelated.txt'), 'occupied\n')
  await assert.rejects(
    writeStrongFlowGitHubReviewPackage({
      outputDirectory: conflict,
      reviewPackage: generated,
    }),
    error => error instanceof StrongFlowGitHubReviewPackageError
      && error.code === 'OUTPUT_CONFLICT',
  )
  const siblings = await readdir(dirname(output))
  assert.equal(siblings.some(name => name.includes('.pending-')), false)
})

test('publication defaults to a provider-free zero-write dry run', async t => {
  const fixture = await reviewFixture(t)
  const reviewPackage = generateStrongFlowGitHubReviewPackage(packageInput(fixture))
  const provider = new FixtureGitHubProvider()
  const result = await runStrongFlowGitHubPublication({
    home: fixture.home,
    delivery: fixture.delivery,
    candidate: fixture.candidate,
    reviewPackage,
    publicationAttentionItemId: publicationAttentionId,
    provider,
  })
  assert.deepEqual(result, {
    schemaVersion: 1,
    mode: 'dry-run',
    status: 'dry-run',
    reviewPackageId: reviewPackage.manifest.packageId,
    providerIdempotencyKey: reviewPackage.manifest.providerIdempotencyKey,
    publicationSetSha256: reviewPackage.manifest.publicationSetSha256,
    remoteWriteCount: 0,
  })
  assert.deepEqual(provider.lookupCalls, [])
  assert.deepEqual(provider.applyCalls, [])
  assert.equal((await readdir(fixture.home)).includes('github-publications'), false)
})

test('live publication requires the exact resolved human approval and a provider', async t => {
  const openFixture = await reviewFixture(t)
  const openPackage = generateStrongFlowGitHubReviewPackage(packageInput(openFixture))
  const openProvider = new FixtureGitHubProvider()
  await assert.rejects(
    runStrongFlowGitHubPublication({
      home: openFixture.home,
      mode: 'live',
      delivery: openFixture.delivery,
      candidate: openFixture.candidate,
      reviewPackage: openPackage,
      publicationAttentionItemId: publicationAttentionId,
      provider: openProvider,
    }),
    error => error.code === 'LIVE_APPROVAL_REQUIRED',
  )
  assert.deepEqual(openProvider.lookupCalls, [])
  assert.deepEqual(openProvider.applyCalls, [])

  const approved = await approvedPublicationFixture(t)
  await assert.rejects(
    runStrongFlowGitHubPublication({
      home: approved.home,
      mode: 'live',
      delivery: approved.delivery,
      candidate: approved.candidate,
      reviewPackage: approved.reviewPackage,
      publicationAttentionItemId: publicationAttentionId,
    }),
    error => error.code === 'PROVIDER_REQUIRED',
  )
})

test('live publication persists intent and replays all four provider operations once', async t => {
  const fixture = await approvedPublicationFixture(t)
  const provider = new FixtureGitHubProvider()
  let now = baseTime + 1_000
  const input = {
    home: fixture.home,
    mode: 'live',
    delivery: fixture.delivery,
    candidate: fixture.candidate,
    reviewPackage: fixture.reviewPackage,
    publicationAttentionItemId: publicationAttentionId,
    provider,
    clock: () => ++now,
  }
  const first = await runStrongFlowGitHubPublication(input)
  assert.equal(first.status, 'succeeded')
  assert.deepEqual(provider.remoteWrites, [
    'branch',
    'pull-request',
    'issue-comment',
    'commit-status',
  ])
  assert.equal(first.confirmedRemoteWriteCount, 4)
  assert.equal(first.journal.steps.every(step => step.state === 'succeeded'), true)
  assert.equal(first.journal.events[0].outcome, 'lookup-absent')
  assert.equal(first.journal.events[1].outcome, 'apply-intent')
  const callsAfterFirst = provider.lookupCalls.length + provider.applyCalls.length

  const replay = await runStrongFlowGitHubPublication(input)
  assert.equal(replay.status, 'succeeded')
  assert.equal(provider.lookupCalls.length + provider.applyCalls.length, callsAfterFirst)
  assert.deepEqual(provider.remoteWrites, [
    'branch',
    'pull-request',
    'issue-comment',
    'commit-status',
  ])
  assert.equal(replay.journal.intent.reviewPackageId, fixture.reviewPackage.manifest.packageId)
})

test('unknown apply outcomes reconcile by lookup without duplicate remote resources', async t => {
  for (const kind of ['branch', 'pull-request', 'issue-comment', 'commit-status']) {
    await t.test(kind, async () => {
      const fixture = await approvedPublicationFixture(t)
      const provider = new FixtureGitHubProvider()
      provider.applyThrowsAfterWriteOnce.add(kind)
      let now = baseTime + 2_000
      const input = {
        home: fixture.home,
        mode: 'live',
        delivery: fixture.delivery,
        candidate: fixture.candidate,
        reviewPackage: fixture.reviewPackage,
        publicationAttentionItemId: publicationAttentionId,
        provider,
        clock: () => ++now,
      }
      const uncertain = await runStrongFlowGitHubPublication(input)
      assert.equal(uncertain.status, 'pending')
      assert.equal(
        uncertain.journal.steps.find(step => step.kind === kind).state,
        'unknown',
      )
      const reconciled = await runStrongFlowGitHubPublication(input)
      assert.equal(reconciled.status, 'succeeded')
      assert.equal(provider.remoteWrites.filter(entry => entry === kind).length, 1)
      assert.equal(provider.resources.size, 4)
    })
  }
})

test('interrupted lookup at each provider boundary performs no speculative write', async t => {
  for (const kind of ['branch', 'pull-request', 'issue-comment', 'commit-status']) {
    await t.test(kind, async () => {
      const fixture = await approvedPublicationFixture(t)
      const provider = new FixtureGitHubProvider()
      provider.lookupThrowsOnce.add(kind)
      let now = baseTime + 2_500
      const input = {
        home: fixture.home,
        mode: 'live',
        delivery: fixture.delivery,
        candidate: fixture.candidate,
        reviewPackage: fixture.reviewPackage,
        publicationAttentionItemId: publicationAttentionId,
        provider,
        clock: () => ++now,
      }
      const interrupted = await runStrongFlowGitHubPublication(input)
      assert.equal(interrupted.status, 'pending')
      assert.equal(provider.applyCalls.includes(kind), false)
      const reconciled = await runStrongFlowGitHubPublication(input)
      assert.equal(reconciled.status, 'succeeded')
      assert.equal(provider.remoteWrites.filter(entry => entry === kind).length, 1)
    })
  }
})

test('unknown lookups remain pending until an authoritative observation is available', async t => {
  const fixture = await approvedPublicationFixture(t)
  const provider = new FixtureGitHubProvider()
  provider.lookupUnknown.add('pull-request')
  let now = baseTime + 3_000
  const input = {
    home: fixture.home,
    mode: 'live',
    delivery: fixture.delivery,
    candidate: fixture.candidate,
    reviewPackage: fixture.reviewPackage,
    publicationAttentionItemId: publicationAttentionId,
    provider,
    clock: () => ++now,
  }
  assert.equal((await runStrongFlowGitHubPublication(input)).status, 'pending')
  assert.equal((await runStrongFlowGitHubPublication(input)).status, 'pending')
  assert.equal(provider.applyCalls.includes('pull-request'), false)
  provider.lookupUnknown.delete('pull-request')
  const reconciled = await runStrongFlowGitHubPublication(input)
  assert.equal(reconciled.status, 'succeeded')
  assert.equal(provider.remoteWrites.filter(kind => kind === 'pull-request').length, 1)
})

test('provider rejection stops later writes and concurrent retries converge', async t => {
  const rejectedFixture = await approvedPublicationFixture(t)
  const rejectedProvider = new FixtureGitHubProvider()
  rejectedProvider.rejected.add('issue-comment')
  let rejectedNow = baseTime + 4_000
  const rejected = await runStrongFlowGitHubPublication({
    home: rejectedFixture.home,
    mode: 'live',
    delivery: rejectedFixture.delivery,
    candidate: rejectedFixture.candidate,
    reviewPackage: rejectedFixture.reviewPackage,
    publicationAttentionItemId: publicationAttentionId,
    provider: rejectedProvider,
    clock: () => ++rejectedNow,
  })
  assert.equal(rejected.status, 'failed')
  assert.equal(rejectedProvider.applyCalls.includes('commit-status'), false)
  assert.equal(rejectedProvider.remoteWrites.includes('issue-comment'), false)

  const concurrentFixture = await approvedPublicationFixture(t)
  const concurrentProvider = new FixtureGitHubProvider()
  let concurrentNow = baseTime + 5_000
  const input = {
    home: concurrentFixture.home,
    mode: 'live',
    delivery: concurrentFixture.delivery,
    candidate: concurrentFixture.candidate,
    reviewPackage: concurrentFixture.reviewPackage,
    publicationAttentionItemId: publicationAttentionId,
    provider: concurrentProvider,
    clock: () => ++concurrentNow,
  }
  const results = await Promise.all([
    runStrongFlowGitHubPublication(input),
    runStrongFlowGitHubPublication(input),
  ])
  assert.equal(results.every(result => result.status === 'succeeded'), true)
  assert.deepEqual(
    [...new Set(concurrentProvider.remoteWrites)].sort(),
    ['branch', 'commit-status', 'issue-comment', 'pull-request'],
  )
  assert.equal(concurrentProvider.remoteWrites.length, 4)
  assert.equal(concurrentProvider.resources.size, 4)
})

test('provider diagnostics and credential-bearing resource references never enter the journal', async t => {
  const fixture = await approvedPublicationFixture(t)
  const provider = new FixtureGitHubProvider()
  provider.apply = async (operation) => ({
    state: 'applied',
    operationKey: operation.operationKey,
    requestSha256: operation.requestSha256,
    resourceRef: 'https://fixture-user:fixture-password@github.com/example/widget',
    remoteWritePerformed: true,
  })
  let now = baseTime + 6_000
  const result = await runStrongFlowGitHubPublication({
    home: fixture.home,
    mode: 'live',
    delivery: fixture.delivery,
    candidate: fixture.candidate,
    reviewPackage: fixture.reviewPackage,
    publicationAttentionItemId: publicationAttentionId,
    provider,
    clock: () => ++now,
  })
  assert.equal(result.status, 'pending')
  assert.equal(result.journal.steps[0].lastCode, 'apply-result-invalid')
  const root = join(fixture.home, 'github-publications')
  const files = await readdir(root, { recursive: true, withFileTypes: true })
  const contents = await Promise.all(files
    .filter(entry => entry.isFile())
    .map(entry => readFile(join(entry.parentPath, entry.name), 'utf8')))
  assert.equal(contents.some(content => content.includes('fixture-password')), false)
})

test('publication retry fails closed when its durable intent changes on disk', async t => {
  const fixture = await approvedPublicationFixture(t)
  const provider = new FixtureGitHubProvider()
  let now = baseTime + 7_000
  const input = {
    home: fixture.home,
    mode: 'live',
    delivery: fixture.delivery,
    candidate: fixture.candidate,
    reviewPackage: fixture.reviewPackage,
    publicationAttentionItemId: publicationAttentionId,
    provider,
    clock: () => ++now,
  }
  assert.equal((await runStrongFlowGitHubPublication(input)).status, 'succeeded')
  const providerDirectory = createHash('sha256')
    .update(fixture.reviewPackage.manifest.providerIdempotencyKey)
    .digest('hex')
  const packageDirectory = createHash('sha256')
    .update(fixture.reviewPackage.manifest.packageId)
    .digest('hex')
  const intentPath = join(
    fixture.home,
    'github-publications',
    providerDirectory,
    packageDirectory,
    'intent.json',
  )
  const intent = JSON.parse(await readFile(intentPath, 'utf8'))
  await writeFile(intentPath, `${JSON.stringify({ ...intent, candidateRef: `git-candidate:sha256:${'0'.repeat(64)}` }, null, 2)}\n`)
  await assert.rejects(
    runStrongFlowGitHubPublication(input),
    error => error.code === 'JOURNAL_ERROR',
  )
})
