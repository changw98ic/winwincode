import assert from 'node:assert/strict'
import { execFile } from 'node:child_process'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { promisify } from 'node:util'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
  materializeStrongFlowDeliveryAdvanceRequest,
  parseStrongFlowPlanReviewContextText,
} from '../packages/contracts/dist/index.js'
import {
  CodexRuntimeProjector,
} from '../packages/dsh-profile/dist/index.js'
import {
  LocalGitDeliveryWorkspace,
  StrongFlowDeliveryStageCoordinator,
  StrongFlowService,
  createStrongFlowDeliveryLocalProofAuthenticator,
  createStrongFlowPlanReviewDecision,
} from '../packages/strongflow/dist/index.js'

const exec = promisify(execFile)
const baseTime = 3_200_000_000_000
const localProof = 'coordinator-local-session-proof'

async function git(repository, ...args) {
  return (await exec('git', ['-C', repository, ...args], { encoding: 'utf8' })).stdout.trim()
}

function kernelEvent(sequence, type, data, submissionId) {
  const payload = { id: submissionId, msg: { type, ...data } }
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
        entries: [{
          path: { type: 'special', value: { kind: 'root' } },
          access: 'read',
        }],
      },
      network: 'restricted',
    },
  }
}

function planSolution() {
  return {
    id: 'solution-browser-loop',
    summary: 'Change one repository file and verify its exact frozen candidate.',
    approach: ['Edit the bounded file.', 'Run independent review and verification.'],
    components: [{
      id: 'component-browser-loop',
      label: 'Candidate file',
      responsibility: 'Expose the approved changed value.',
      kind: 'component',
      trustBoundary: 'Local repository',
      unresolved: false,
      repositoryPathPrefixes: ['src'],
    }],
    connections: [{
      id: 'connection-browser-loop',
      from: 'platform:codex-core',
      to: 'component-browser-loop',
      label: 'Implements the approved change',
    }],
  }
}

class FakeRoleSession {
  #runtime
  #projector
  #sequence = 0
  #assignment = null

  constructor(runtime, input) {
    this.#runtime = runtime
    this.input = input
    this.dshSessionId = input.dshSessionId
    this.codexSessionId = `codex-${input.dshSessionId}`
    this.#projector = new CodexRuntimeProjector({
      sessionId: this.dshSessionId,
      kernelSessionId: this.codexSessionId,
      roleId: input.role,
      kernelStreamId: `stream-${input.dshSessionId}`,
    })
    this.#emit('session_configured', {
      session_id: this.codexSessionId,
      thread_id: this.codexSessionId,
      ...(input.role === 'reviewer' || input.role === 'verifier'
        ? readOnlySessionConfiguration()
        : {}),
    }, 'startup')
  }

  #emit(type, data = {}, submissionId = `turn-${this.#sequence + 1}`) {
    this.#sequence += 1
    const event = this.#projector.ingest(kernelEvent(
      this.#sequence,
      type,
      data,
      submissionId,
    ))
    assert.ok(event)
    this.#runtime.events.get(this.dshSessionId).push(event)
    return event
  }

  async turn(prompt, signal) {
    const turnId = `turn-${this.input.role}-${this.#runtime.turns.length + 1}`
    this.#runtime.turns.push({ role: this.input.role, prompt, cwd: this.input.cwd })
    this.#emit('task_started', { turn_id: turnId }, turnId)
    if (this.#runtime.abortRoleOnce === this.input.role) {
      this.#runtime.abortRoleOnce = null
      this.#emit('turn_aborted', { turn_id: turnId }, turnId)
      this.#runtime.abortController?.abort()
      signal?.throwIfAborted()
      throw new Error('fixture turn was aborted')
    }
    if (this.input.role === 'planner') {
      this.#emit('plan_update', {
        explanation: 'Plan the exact approved delivery.',
        plan: [
          { step: 'Change the bounded file', status: 'completed' },
          { step: 'Run independent verification', status: 'completed' },
        ],
      }, turnId)
      this.#emit('agent_message', {
        turn_id: turnId,
        message: JSON.stringify({
          protocol: 'winwincode.planning-result.v1',
          solution: planSolution(),
          risks: [],
          unresolvedItems: [],
        }),
      }, turnId)
    } else if (this.input.role === 'executor' || this.input.role === 'remediator') {
      await writeFile(
        join(this.input.cwd, 'src', 'value.txt'),
        this.input.role === 'executor' ? 'after\n' : 'after-remediation\n',
      )
      assert.match(this.#runtime.baseRevision, /^[a-f0-9]{40,64}$/u)
      const unifiedDiff = (await exec('git', [
        '-C',
        this.input.cwd,
        'diff',
        '--no-ext-diff',
        '--binary',
        '--full-index',
        this.#runtime.baseRevision,
      ], { encoding: 'utf8' })).stdout
      this.#emit('turn_diff', {
        turn_id: turnId,
        unified_diff: unifiedDiff,
      }, turnId)
      this.#emit('agent_message', {
        turn_id: turnId,
        message: this.input.role === 'executor'
          ? 'Implemented the approved bounded change.'
          : 'Applied only the approved diagram annotations.',
      }, turnId)
    } else {
      if (prompt.startsWith('{')) {
        this.#assignment = JSON.parse(prompt.split('\n', 1)[0])
        const evidence = this.#emit('item_completed', {
          turn_id: turnId,
          item: {
            type: 'CommandExecution',
            id: `check-${this.input.role}`,
            command: ['git', 'diff', '--check'],
            status: 'completed',
            exit_code: 0,
          },
        }, turnId)
        this.#emit('agent_message', {
          turn_id: turnId,
          message: `${this.input.role} observed ${evidence.id}.`,
        }, turnId)
      } else {
        assert.ok(this.#assignment)
        const evidence = this.#runtime.events.get(this.dshSessionId)
          .find(event => event.kind === 'tool.completed')
        assert.ok(evidence)
        this.#emit('agent_message', {
          turn_id: turnId,
          message: JSON.stringify({
            protocol: 'winwincode.independent-verification-result.v1',
            delivery_spec_id: this.#assignment.deliverySpec.id,
            delivery_spec_revision: this.#assignment.deliverySpec.revision,
            candidate_ref: this.#assignment.candidate.candidateRef,
            findings: this.#assignment.deliverySpec.acceptanceCriteria.map((criterion, index) => ({
              finding_id: `finding-${this.input.role}-${String(index + 1)}`,
              criterion_id: criterion.id,
              verdict: 'pass',
              explanation: `${this.input.role} observed the required candidate behavior.`,
              evidence_sources: [{ type: 'command', event_id: evidence.id }],
            })),
          }),
        }, turnId)
      }
    }
    if (this.#runtime.pauseRoleOnce === this.input.role) {
      this.#runtime.pauseRoleOnce = null
      this.#runtime.markPauseStarted?.()
      await this.#runtime.pauseRelease
    }
    this.#emit('task_complete', {
      turn_id: turnId,
      last_agent_message: `${this.input.role} turn completed`,
      error: null,
    }, turnId)
  }

  async dispose() {}
}

class FakeStageRuntime {
  events = new Map()
  sessions = new Map()
  turns = []
  abortRoleOnce = null
  abortController = null
  baseRevision = null
  pauseRoleOnce = null
  pauseRelease = null
  markPauseStarted = null

  constructor(readDelivery) {
    this.readDelivery = readDelivery
  }

  async openRoleSession(input) {
    let session = this.sessions.get(input.dshSessionId)
    if (session === undefined) {
      this.events.set(input.dshSessionId, [])
      session = new FakeRoleSession(this, input)
      this.sessions.set(input.dshSessionId, session)
    }
    return session
  }

  async readRuntimeSessionEvents(dshSessionId) {
    return Object.freeze([...(this.events.get(dshSessionId) ?? [])])
  }

  allEvents() {
    return Object.freeze([...this.events.values()].flat())
  }

  async reconcileDelivery(deliveryId) {
    const delivery = await this.readDelivery(deliveryId)
    const active = delivery.stageRuns.find(run => (
      run.status === 'running' || run.status === 'waiting'
    ))
    const blocking = delivery.attentionItems.filter(item => item.blocking && item.status === 'open')
    if (active?.actorType === 'human' && delivery.sessionBindings.every(
      binding => binding.stageRunId !== active.id,
    )) {
      return {
        delivery,
        nextAction: {
          kind: 'create-stage-session',
          stageRunId: active.id,
          stage: active.stage,
          actorType: active.actorType,
          role: active.role,
        },
      }
    }
    if (blocking.length > 0) {
      return {
        delivery,
        nextAction: {
          kind: 'resolve-delivery-attention',
          attentionItemIds: blocking.map(item => item.id),
        },
      }
    }
    if (active !== undefined) {
      const binding = delivery.sessionBindings.find(entry => entry.stageRunId === active.id)
      if (binding === undefined) {
        return {
          delivery,
          nextAction: {
            kind: 'create-stage-session',
            stageRunId: active.id,
            stage: active.stage,
            actorType: active.actorType,
            role: active.role,
          },
        }
      }
      const events = this.events.get(binding.dshSessionId) ?? []
      const terminal = events.findLast(event => (
        event.kind === 'turn.completed' || event.kind === 'turn.aborted'
      ))
      return {
        delivery,
        nextAction: terminal === undefined
          ? {
              kind: 'continue-stage',
              stageRunId: active.id,
              sessionBindingId: binding.id,
              dshSessionId: binding.dshSessionId,
              codexSessionId: binding.codexSessionId,
            }
          : {
              kind: 'review-stage-output',
              stageRunId: active.id,
              sessionBindingId: binding.id,
              dshSessionId: binding.dshSessionId,
              codexSessionId: binding.codexSessionId,
              runtimeStatus: terminal.kind === 'turn.completed' ? 'completed' : 'aborted',
            },
      }
    }
    const stage = delivery.status === 'ready' || delivery.status === 'planning'
      ? 'planning'
      : delivery.status === 'executing'
        ? 'executing'
        : delivery.status === 'reworking'
          ? 'reworking'
        : delivery.status === 'verifying'
          ? 'verifying'
          : delivery.status === 'ready-to-deliver'
            ? 'delivery-review'
            : null
    return {
      delivery,
      nextAction: stage === null
        ? { kind: 'delivery-complete' }
        : { kind: 'start-stage', stage, deliveryTaskId: null },
    }
  }
}

function draftSpec(deliveryId, repository, baseRevision, revision, suffix) {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `spec-browser-loop-${suffix}`,
    deliveryId,
    revision,
    title: 'Run the browser delivery loop',
    goal: 'Change one file and prove the frozen result.',
    scope: ['src/value.txt'],
    outOfScope: ['unrelated files'],
    constraints: ['Codex remains the execution authority'],
    acceptanceCriteria: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `criterion-browser-loop-${suffix}`,
      description: 'The candidate changes src/value.txt to after.',
      verificationMethod: 'Inspect the frozen file and run git diff --check.',
      required: true,
    }],
    sourceRef: null,
    publicationTarget: null,
    repository: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      kind: 'local-git',
      locator: repository,
    },
    baseRevision,
    maxReworkAttempts: 2,
    createdAtMillis: baseTime + revision,
  }
}

test('one browser action per role drives approved spec through both human review gates', async t => {
  const root = await mkdtemp(join(tmpdir(), 'winwincode-stage-coordinator-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const repository = join(root, 'repository')
  const home = join(root, 'home')
  await exec('git', ['init', repository])
  await git(repository, 'config', 'user.name', 'Fixture')
  await git(repository, 'config', 'user.email', 'fixture@example.test')
  await exec('mkdir', ['-p', join(repository, 'src')])
  await writeFile(join(repository, 'src', 'value.txt'), 'before\n')
  await git(repository, 'add', 'src/value.txt')
  await git(repository, 'commit', '-m', 'base')
  const baseRevision = await git(repository, 'rev-parse', 'HEAD')
  let clock = baseTime + 100
  const workspace = new LocalGitDeliveryWorkspace({ home })
  let runtime
  const service = new StrongFlowService({
    home,
    clock: () => ++clock,
    authenticator: createStrongFlowDeliveryLocalProofAuthenticator({
      localSessionProof: localProof,
      localSessionActorId: 'browser-human',
    }),
    executionSource: {
      async read(currentDelivery) {
        const candidate = await workspace.currentCandidateSnapshot(currentDelivery)
        return {
          runtimeEvents: runtime?.allEvents() ?? [],
          candidate: candidate?.candidate ?? null,
          candidateDiff: candidate?.unifiedDiff ?? null,
        }
      },
    },
  })
  runtime = new FakeStageRuntime(async deliveryId => (
    await service.getDeliveryProjection(deliveryId)
  ).delivery)
  runtime.baseRevision = baseRevision
  const coordinator = new StrongFlowDeliveryStageCoordinator({
    service,
    runtime,
    workspace,
  })
  const deliveryId = 'dlv_1GSSFJ2FJ4N06K1R41D8XYP6GB'
  const created = await service.createDelivery({
    requestId: 'create-browser-loop',
    spec: draftSpec(deliveryId, repository, baseRevision, 1, 'draft'),
    tasks: [],
  })
  let delivery = await service.updateDeliverySpec({
    requestId: 'approve-browser-loop-spec',
    deliveryId,
    expectedRevision: created.revision,
    spec: draftSpec(deliveryId, repository, baseRevision, 2, 'approved'),
  })
  const caller = {
    dshSessionId: 'dsh-browser-human-review',
    modelRoute: { provider: 'fixture', model: 'fixture-coder', maxTokens: 4_096 },
  }
  let advanceNumber = 0
  const advance = async () => {
    advanceNumber += 1
    const result = await coordinator.advance(
      materializeStrongFlowDeliveryAdvanceRequest(
        `advance-browser-loop-${String(advanceNumber)}`,
        delivery.id,
        delivery.revision,
      ),
      caller,
    )
    delivery = result.delivery
    return result
  }

  const plan = await advance()
  assert.equal(plan.outcome.kind, 'plan-review-ready')
  assert.equal(delivery.status, 'needs-attention')
  const planAttention = delivery.attentionItems.find(item => item.status === 'open')
  assert.ok(planAttention)
  const context = parseStrongFlowPlanReviewContextText(planAttention.context)
  const planDecision = createStrongFlowPlanReviewDecision({
    context,
    action: 'approve',
    comments: 'The exact solution and both diagrams are approved.',
    requestedChanges: [],
  })
  delivery = await service.resolveAttention({
    requestId: 'resolve-browser-plan-review',
    deliveryId,
    expectedRevision: delivery.revision,
    attentionItemId: planAttention.id,
    status: 'resolved',
    resolution: JSON.stringify(planDecision),
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: localProof },
  })

  const execution = await advance()
  assert.equal(execution.outcome.kind, 'candidate-ready-for-review')
  assert.equal(delivery.status, 'verifying')
  assert.equal(delivery.stageRuns.at(-1).role, 'reviewer')

  const review = await advance()
  assert.equal(review.outcome.kind, 'reviewer-complete')
  assert.equal(delivery.stageRuns.at(-1).role, 'verifier')

  const verification = await advance()
  assert.equal(verification.outcome.kind, 'delivery-review-ready')
  assert.equal(delivery.status, 'needs-attention')
  assert.equal(delivery.verdict.status, 'pass')
  const firstDeliveryReview = delivery.attentionItems.find(item => (
    item.type === 'delivery_approval' && item.status === 'open'
  ))
  assert.ok(firstDeliveryReview)
  const finishedProjection = await service.getDeliveryProjection(deliveryId)
  assert.equal(finishedProjection.diagramExecution.state, 'execution-finished')
  assert.ok(finishedProjection.diagramExecution.details)
  const details = finishedProjection.diagramExecution.details
  const file = details.files.find(entry => entry.path === 'src/value.txt')
  assert.ok(file)
  const hunk = details.hunks.find(entry => entry.fileId === file.id)
  assert.ok(hunk)
  assert.equal(details.provenance.deliveryTaskId, null)
  assert.ok(details.provenance.evidenceRefIds.length > 0)
  const oldCandidateRef = details.candidate.candidateRef
  delivery = await service.resolveAttention({
    requestId: 'resolve-browser-delivery-remediation',
    deliveryId,
    expectedRevision: delivery.revision,
    attentionItemId: firstDeliveryReview.id,
    status: 'dismissed',
    resolution: 'Apply only the selected candidate-bound annotation.',
    remediation: {
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      protocol: 'winwincode.delivery-remediation.v1',
      deliveryTaskId: null,
      candidate: details.candidate,
      annotations: [{
        schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
        id: 'annotation-browser-remediation',
        diagramKind: 'system-architecture',
        diagramId: finishedProjection.diagramExecution.architecture.diagramId,
        nodeId: 'component-browser-loop',
        filePath: file.path,
        hunkSha256: hunk.sha256,
        evidenceRefIds: details.provenance.evidenceRefIds,
        note: 'Change this exact reviewed hunk from after to after-remediation.',
      }],
    },
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: localProof },
  })
  assert.equal(delivery.status, 'reworking')
  assert.equal(delivery.verdict, null)
  const liveProjection = await service.getDeliveryProjection(deliveryId)
  assert.equal(liveProjection.diagramExecution.state, 'executing')
  assert.equal(liveProjection.diagramExecution.details, null)
  assert.equal(
    liveProjection.diagramExecution.architecture.nodes.find(node => (
      node.nodeId === 'component-browser-loop'
    )).state,
    'affected-live',
  )

  runtime.pauseRoleOnce = 'remediator'
  const pauseStarted = new Promise(resolve => { runtime.markPauseStarted = resolve })
  let releasePausedTurn
  runtime.pauseRelease = new Promise(resolve => { releasePausedTurn = resolve })
  const remediationPromise = advance()
  await pauseStarted
  const duringRemediation = await service.getDeliveryProjection(deliveryId)
  assert.equal(duringRemediation.delivery.status, 'reworking')
  assert.equal(duringRemediation.diagramExecution.state, 'executing')
  assert.equal(duringRemediation.diagramExecution.details, null)
  assert.equal(
    duringRemediation.diagramExecution.architecture.nodes.find(node => (
      node.nodeId === 'component-browser-loop'
    )).state,
    'affected-live',
  )
  releasePausedTurn()
  const remediation = await remediationPromise
  assert.equal(remediation.outcome.kind, 'candidate-ready-for-review')
  assert.equal(delivery.status, 'verifying')
  const remediationRun = delivery.stageRuns.find(run => run.role === 'remediator')
  assert.ok(remediationRun)
  assert.equal(remediationRun.status, 'succeeded')
  const remediationBindings = delivery.sessionBindings.filter(binding => (
    binding.stageRunId === remediationRun.id
  ))
  assert.equal(remediationBindings.length, 1)
  assert.notEqual(remediationBindings[0].codexSessionId, null)
  const remediatedCandidate = await workspace.currentCandidate(delivery)
  assert.ok(remediatedCandidate)
  assert.notEqual(remediatedCandidate.candidateRef, oldCandidateRef)
  const remediatorTurn = runtime.turns.find(turn => turn.role === 'remediator')
  assert.ok(remediatorTurn)
  assert.equal(
    await readFile(join(remediatorTurn.cwd, 'src', 'value.txt'), 'utf8'),
    'after-remediation\n',
  )
  const remediationPromptValue = JSON.parse(remediatorTurn.prompt)
  assert.deepEqual(Object.keys(remediationPromptValue).sort(), [
    'approvedAnnotations',
    'deliverySpec',
    'instruction',
    'protocol',
  ])
  assert.equal(remediationPromptValue.protocol, 'winwincode.remediation.v1')
  assert.equal(remediationPromptValue.approvedAnnotations.length, 1)
  assert.equal(remediationPromptValue.approvedAnnotations[0].filePath, 'src/value.txt')
  assert.doesNotMatch(JSON.stringify(remediationPromptValue), /approvedPlanReview|risks|unresolvedItems/u)

  const reworkReview = await advance()
  assert.equal(reworkReview.outcome.kind, 'reviewer-complete')
  const reworkVerification = await advance()
  assert.equal(reworkVerification.outcome.kind, 'delivery-review-ready')
  assert.equal(delivery.status, 'needs-attention')
  assert.equal(delivery.verdict.status, 'pass')
  const remediatedProjection = await service.getDeliveryProjection(deliveryId)
  assert.equal(remediatedProjection.diagramExecution.state, 'execution-finished')
  assert.equal(
    remediatedProjection.diagramExecution.details.candidate.candidateRef,
    remediatedCandidate.candidateRef,
  )
  assert.match(
    remediatedProjection.diagramExecution.details.hunks.map(entry => entry.content).join('\n'),
    /after-remediation/u,
  )
  const finalAttention = delivery.attentionItems.findLast(item => (
    item.type === 'delivery_approval' && item.status === 'open'
  ))
  assert.ok(finalAttention)
  delivery = await service.resolveAttention({
    requestId: 'resolve-browser-delivery-review',
    deliveryId,
    expectedRevision: delivery.revision,
    attentionItemId: finalAttention.id,
    status: 'resolved',
    resolution: 'The exact frozen candidate and passing evidence are approved.',
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: localProof },
  })

  assert.equal(delivery.status, 'delivered')
  assert.deepEqual(runtime.turns.map(turn => turn.role), [
    'planner',
    'executor',
    'reviewer',
    'reviewer',
    'verifier',
    'verifier',
    'remediator',
    'reviewer',
    'reviewer',
    'verifier',
    'verifier',
  ])
  assert.deepEqual(
    delivery.sessionBindings
      .filter(binding => binding.codexSessionId === null)
      .map(binding => binding.dshSessionId),
    [
      'dsh-browser-human-review',
      'dsh-browser-human-review',
      'dsh-browser-human-review',
    ],
  )
  assert.equal(await git(repository, 'rev-parse', 'HEAD'), baseRevision)
  assert.equal(await git(repository, 'status', '--porcelain=v1'), '')
})

test('an aborted role StageRun resumes after coordinator restart without a second binding', async t => {
  const root = await mkdtemp(join(tmpdir(), 'winwincode-stage-restart-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const repository = join(root, 'repository')
  const home = join(root, 'home')
  await exec('git', ['init', repository])
  await git(repository, 'config', 'user.name', 'Fixture')
  await git(repository, 'config', 'user.email', 'fixture@example.test')
  await exec('mkdir', ['-p', join(repository, 'src')])
  await writeFile(join(repository, 'src', 'value.txt'), 'before\n')
  await git(repository, 'add', 'src/value.txt')
  await git(repository, 'commit', '-m', 'base')
  const baseRevision = await git(repository, 'rev-parse', 'HEAD')
  let clock = baseTime + 2_000
  const service = new StrongFlowService({
    home,
    clock: () => ++clock,
    authenticator: createStrongFlowDeliveryLocalProofAuthenticator({
      localSessionProof: localProof,
      localSessionActorId: 'restart-human',
    }),
  })
  const runtime = new FakeStageRuntime(async deliveryId => (
    await service.getDeliveryProjection(deliveryId)
  ).delivery)
  runtime.baseRevision = baseRevision
  const workspace = new LocalGitDeliveryWorkspace({ home })
  const deliveryId = 'dlv_3N0AMNQQX75367AGFTG98Q6WVC'
  const created = await service.createDelivery({
    requestId: 'create-restart-loop',
    spec: draftSpec(deliveryId, repository, baseRevision, 1, 'restart-draft'),
    tasks: [],
  })
  let delivery = await service.updateDeliverySpec({
    requestId: 'approve-restart-loop-spec',
    deliveryId,
    expectedRevision: created.revision,
    spec: draftSpec(deliveryId, repository, baseRevision, 2, 'restart-approved'),
  })
  const caller = {
    dshSessionId: 'dsh-restart-human-review',
    modelRoute: { provider: 'fixture', model: 'fixture-coder' },
  }
  runtime.abortRoleOnce = 'planner'
  runtime.abortController = new AbortController()
  const firstCoordinator = new StrongFlowDeliveryStageCoordinator({ service, runtime, workspace })
  await assert.rejects(
    firstCoordinator.advance(
      materializeStrongFlowDeliveryAdvanceRequest(
        'advance-restart-aborted',
        delivery.id,
        delivery.revision,
      ),
      caller,
      { signal: runtime.abortController.signal },
    ),
    error => error?.code === 'OPERATION_ABORTED',
  )

  delivery = (await service.getDeliveryProjection(delivery.id)).delivery
  assert.equal(delivery.status, 'planning')
  assert.equal(delivery.stageRuns.filter(run => run.status === 'running').length, 1)
  assert.equal(delivery.sessionBindings.length, 1)
  const originalBinding = delivery.sessionBindings[0]

  runtime.abortController = null
  const restartedCoordinator = new StrongFlowDeliveryStageCoordinator({ service, runtime, workspace })
  const resumed = await restartedCoordinator.advance(
    materializeStrongFlowDeliveryAdvanceRequest(
      'advance-restart-resumed',
      delivery.id,
      delivery.revision,
    ),
    caller,
  )
  assert.equal(resumed.outcome.kind, 'plan-review-ready')
  assert.equal(resumed.delivery.status, 'needs-attention')
  assert.equal(
    resumed.delivery.sessionBindings.filter(binding => binding.stageRunId === originalBinding.stageRunId).length,
    1,
  )
  assert.deepEqual(
    resumed.delivery.sessionBindings.find(binding => binding.stageRunId === originalBinding.stageRunId),
    originalBinding,
  )
  assert.deepEqual(runtime.turns.map(turn => turn.role), ['planner', 'planner'])
})
