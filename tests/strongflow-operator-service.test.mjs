import assert from 'node:assert/strict'
import { mkdir, mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { Context } from '@deepseek-ai/cordis'
import TypertGatewayService from '@deepseek-ai/dsh-api-gateway'
import { remoteMethods } from '@deepseek-ai/dsh-typert-protocol'
import { TypertRegistry } from '@deepseek-ai/dsh-typert-registry'

import {
  AttemptId,
  DiagramId,
  KernelSessionId,
  RequirementId,
  SolutionId,
  StageRunId,
  materializeStrongFlowArtifact,
  materializeStrongFlowOperatorRequest,
  createStrongFlowJobEvent,
} from '../packages/contracts/dist/index.js'
import {
  StrongFlowArtifactStore,
  StrongFlowJobStore,
  StrongFlowLocalJobService,
  StrongFlowOperatorRemoteService,
  createStrongFlowLocalProofAuthenticator,
  generateStrongFlowDefinitionDiagrams,
} from '../packages/strongflow/dist/index.js'
import { runStrongFlowCli } from '../apps/host/dist/index.js'

function proofAuthenticator() {
  return createStrongFlowLocalProofAuthenticator({
    localSessionProof: 'ui-proof',
    localPeerProof: 'cli-proof',
    localSessionReviewerId: 'reviewer-ui',
    localPeerReviewerId: 'reviewer-cli',
  })
}

async function operatorFixture(t, name, options = {}) {
  const root = await mkdtemp(join(tmpdir(), `winwincode-operator-${name}-`))
  const home = join(root, 'home')
  const repositoryPath = join(root, 'repository')
  await mkdir(repositoryPath, { recursive: true })
  t.after(() => rm(root, { recursive: true, force: true }))
  let now = 2_100_000_000_000
  const serviceOptions = {
    home,
    authenticator: proofAuthenticator(),
    clock: () => ++now,
    followPollMillis: 5,
    ...(options.scheduler === undefined ? {} : { scheduler: options.scheduler }),
  }
  return {
    home,
    repositoryPath,
    service: new StrongFlowLocalJobService(serviceOptions),
    serviceOptions,
  }
}

function createRequest(fixture, suffix, submittedFrom = 'local-ui') {
  return materializeStrongFlowOperatorRequest('job.create', `create-${suffix}`, {
    repositoryPath: fixture.repositoryPath,
    baseRevision: null,
    title: `Operator ${suffix}`,
    request: `Implement the ${suffix} StrongFlow fixture.`,
    submittedFrom,
  })
}

function jobRequest(operation, requestId, jobId, extra = {}) {
  return materializeStrongFlowOperatorRequest(operation, requestId, {
    jobId,
    ...extra,
  })
}

function interval(suffix, firstSequence) {
  return Object.freeze({
    schemaVersion: 1,
    kernelSessionLineageId: `operator-lineage-${suffix}`,
    contextId: `operator-context-${suffix}`,
    generation: 1,
    kernelSessionId: `operator-kernel-${suffix}`,
    kernelStreamId: `operator-stream-${suffix}`,
    turnId: `operator-turn-${suffix}`,
    firstSequence: String(firstSequence),
    lastSequence: String(firstSequence + 1),
    eventCount: 2,
  })
}

function roleMetadata(jobId, artifactId, roleId, stageName, sourceArtifacts, now) {
  return Object.freeze({
    artifactId,
    jobId,
    sourceArtifacts,
    producer: Object.freeze({
      kind: 'role',
      roleId,
      stageRunId: StageRunId(`operator-run-${stageName}`),
      attemptId: AttemptId(`operator-attempt-${stageName}`),
    }),
    kernelEventInterval: interval(stageName, roleId === 'requirements' ? 1 : 3),
    createdAtMillis: now,
  })
}

async function publishDefinition(fixture, jobId, suffix) {
  const artifacts = await StrongFlowArtifactStore.open(fixture.home, jobId)
  const firstPage = await artifacts.list({ limit: 10, entryKinds: ['artifact'] })
  const userRequestRecord = firstPage.records.find(record => (
    record.entryKind === 'artifact' && record.identity.artifactKind === 'USER_REQUEST'
  ))
  assert.ok(userRequestRecord)
  const requirementId = RequirementId(`operator-requirement-${suffix}`)
  const solutionId = SolutionId(`operator-solution-${suffix}`)
  const requirement = materializeStrongFlowArtifact(
    'REQUIREMENT_SPEC',
    roleMetadata(
      jobId,
      requirementId,
      'requirements',
      `requirements-${suffix}`,
      [{
        artifactKind: 'USER_REQUEST',
        artifactId: userRequestRecord.identity.artifactId,
      }],
      2_100_000_001_000,
    ),
    {
      title: '审核运行中的 Agent 变更',
      summary: '需求、方案和两张图必须形成固定身份并由人工审核。',
      goals: [{ id: 'goal-review', text: '人工只批准当前四个定义身份。' }],
      nonGoals: [],
      constraints: [{ id: 'constraint-durable', text: '重启后保留决定和事件游标。' }],
      acceptanceCriteria: [{
        criterionId: 'criterion-review',
        statement: '过期定义不能解锁执行。',
        verification: '提交旧图身份并检查返回的当前定义。',
      }],
      repositoryFacts: [],
      risks: [],
      openQuestions: [],
    },
  )
  const solution = materializeStrongFlowArtifact(
    'SOLUTION_DESIGN',
    roleMetadata(
      jobId,
      solutionId,
      'solution',
      `solution-${suffix}`,
      [{ artifactKind: 'REQUIREMENT_SPEC', artifactId: requirementId }],
      2_100_000_001_100,
    ),
    {
      requirementId,
      summary: '一个本地服务同时供 DSH Remote 和 CLI 调用。',
      decisions: [{
        decisionId: 'decision-shared-service',
        title: '共用本地服务',
        decision: '界面和 CLI 只做传输转换，不各自保存状态。',
        rationale: '两个入口看到同一作业身份、决定和事件序号。',
        requirementItemIds: ['goal-review', 'criterion-review'],
      }],
      components: [
        {
          componentId: 'component-operator',
          name: '本地作业服务',
          kind: 'module',
          responsibility: '保存作业、审核决定和事件。',
          trustBoundary: '本地进程',
          sourcePaths: ['packages/strongflow/src/operator-service.ts'],
        },
        {
          componentId: 'component-workbench',
          name: 'StrongFlow 工作台',
          kind: 'surface',
          responsibility: '显示需求、方案、两张图和审核入口。',
          trustBoundary: '本地 DSH 会话',
          sourcePaths: ['packages/strongflow/src/client.ts'],
        },
      ],
      connections: [{
        connectionId: 'connection-workbench-operator',
        fromComponentId: 'component-workbench',
        toComponentId: 'component-operator',
        label: '提交带身份的操作请求',
      }],
      unresolvedFacts: [],
      risks: [],
    },
  )
  const generated = generateStrongFlowDefinitionDiagrams({
    requirement,
    solution,
    systemArchitectureDiagramId: DiagramId(`operator-architecture-${suffix}`),
    processFlowDiagramId: DiagramId(`operator-process-${suffix}`),
    createdAtMillis: 2_100_000_001_200,
  })
  await artifacts.publishArtifact(requirement)
  await artifacts.publishArtifact(solution)
  await artifacts.publishArtifact(generated.systemArchitectureDiagram)
  await artifacts.publishArtifact(generated.processFlowDiagram)

  const definition = Object.freeze({
    requirementId,
    solutionId,
    systemArchitectureDiagramId: generated.systemArchitectureDiagram.artifactId,
    processFlowDiagramId: generated.processFlowDiagram.artifactId,
  })
  const store = await StrongFlowJobStore.open(fixture.home, jobId)
  async function append(kind, data, roleId, kernelSessionId) {
    const stored = await store.read()
    return store.append(createStrongFlowJobEvent({
      jobId,
      sequence: (BigInt(stored.snapshot.sequence) + 1n).toString(),
      occurredAtMillis: stored.snapshot.lastOccurredAtMillis + 1,
      source: {
        kind: 'role',
        actorId: roleId,
        kernelSessionId: KernelSessionId(kernelSessionId),
      },
      kind,
      data,
    }))
  }
  async function stage(stage, roleId, stageName, output) {
    const identity = {
      stageRunId: StageRunId(`operator-run-${stageName}-${suffix}`),
      attemptId: AttemptId(`operator-attempt-${stageName}-${suffix}`),
    }
    const kernelSessionId = `operator-stage-kernel-${stageName}-${suffix}`
    await append('stage.started', { stage, ...identity }, roleId, kernelSessionId)
    await append('stage.succeeded', { stage, ...identity, ...output }, roleId, kernelSessionId)
  }
  await stage('REQUIREMENTS', 'requirements', 'requirements', { requirementId })
  await stage('SOLUTION', 'solution', 'solution', { requirementId, solutionId })
  await stage('DIAGRAMS', 'solution', 'diagrams', { definition })
  assert.equal((await store.read()).snapshot.state, 'AWAITING_HUMAN_REVIEW')
  return { artifacts, definition, requirement, solution, store }
}

function reviewRequest(operation, requestId, jobId, definition, extra = {}) {
  return materializeStrongFlowOperatorRequest(operation, requestId, {
    jobId,
    definition,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof: 'ui-proof' },
    comment: null,
    ...extra,
  })
}

test('DSH Remote and CLI create and inspect the same durable job identities', async t => {
  const fixture = await operatorFixture(t, 'adapters')
  const ctx = new Context()
  await ctx.plugin(TypertRegistry)
  await ctx.plugin(TypertGatewayService)
  let remote
  const plugin = pluginContext => {
    remote = new StrongFlowOperatorRemoteService(pluginContext, fixture.service)
  }
  await ctx.plugin(plugin)
  t.after(() => ctx.fiber.dispose())
  assert.ok(remote)
  assert.deepEqual(remoteMethods(remote), [{ method: 'invoke', invocation: { kind: 'direct' } }])
  const invalidRemote = await remote.invoke({}, new AbortController().signal)
  assert.equal(invalidRemote.ok, false)
  assert.equal(invalidRemote.error.code, 'INVALID_REQUEST')
  assert.equal(invalidRemote.requestId, null)

  const remoteCreate = await ctx.typertGateway.invoke({
    namespace: 'strongflow',
    method: 'invoke',
    args: { request: createRequest(fixture, 'remote') },
    signal: new AbortController().signal,
  })
  assert.equal(remoteCreate.ok, true, JSON.stringify(remoteCreate))

  const cliStdout = []
  const cliStderr = []
  const cliStatusCode = await runStrongFlowCli([
    'status',
    remoteCreate.result.job.jobId,
    '--request-id',
    'cli-status-remote-job',
    '--json',
  ], fixture.service, {
    stdout: text => cliStdout.push(text),
    stderr: text => cliStderr.push(text),
  })
  assert.equal(cliStatusCode, 0)
  assert.equal(cliStderr.length, 0)
  assert.equal(JSON.parse(cliStdout.join('')).result.job.jobId, remoteCreate.result.job.jobId)

  const cliCreateOutput = []
  const cliCreateCode = await runStrongFlowCli([
    'create',
    '--repo',
    fixture.repositoryPath,
    '--request',
    'Create a second job from the CLI adapter.',
    '--title',
    'CLI-created job',
    '--request-id',
    'cli-create-adapter-job',
    '--json',
  ], fixture.service, {
    stdout: text => cliCreateOutput.push(text),
    stderr: text => assert.fail(`unexpected CLI error: ${text}`),
  })
  assert.equal(cliCreateCode, 0)
  const cliCreated = JSON.parse(cliCreateOutput.join(''))
  const remoteStatus = await remote.invoke(
    jobRequest('job.status', 'remote-status-cli-job', cliCreated.result.job.jobId),
    new AbortController().signal,
  )
  assert.equal(remoteStatus.ok, true)
  assert.equal(remoteStatus.result.job.jobId, cliCreated.result.job.jobId)
  assert.notEqual(remoteStatus.result.job.jobId, remoteCreate.result.job.jobId)

  const invalidCliError = []
  const invalidCliCode = await runStrongFlowCli([
    'status',
    remoteCreate.result.job.jobId,
    '--request-id',
    'not a portable request id',
    '--json',
  ], fixture.service, {
    stdout: text => assert.fail(`unexpected CLI output: ${text}`),
    stderr: text => invalidCliError.push(text),
  })
  assert.equal(invalidCliCode, 2)
  const invalidCliResponse = JSON.parse(invalidCliError.join(''))
  assert.equal(invalidCliResponse.ok, false)
  assert.equal(invalidCliResponse.requestId, null)
  assert.equal(invalidCliResponse.error.code, 'INVALID_REQUEST')
})

test('definition reads, exact review, idempotency, export, and change routing survive restart',
  async t => {
    const schedulerCalls = []
    const fixture = await operatorFixture(t, 'review', {
      scheduler: {
        jobReady(job) {
          schedulerCalls.push(job.configuration.jobId)
        },
      },
    })
    const created = await fixture.service.invoke(createRequest(fixture, 'review'))
    assert.equal(created.ok, true)
    const jobId = created.result.job.jobId
    const published = await publishDefinition(fixture, jobId, 'review')

    const requirement = await fixture.service.invoke(jobRequest(
      'definition.requirement',
      'read-requirement-review',
      jobId,
    ))
    const solution = await fixture.service.invoke(jobRequest(
      'definition.solution',
      'read-solution-review',
      jobId,
    ))
    const diagrams = await fixture.service.invoke(jobRequest(
      'definition.diagrams',
      'read-diagrams-review',
      jobId,
    ))
    assert.equal(requirement.ok, true)
    assert.equal(solution.ok, true)
    assert.equal(diagrams.ok, true)
    assert.equal(requirement.result.link.artifactId, published.definition.requirementId)
    assert.equal(solution.result.link.artifactId, published.definition.solutionId)
    assert.equal(
      diagrams.result.systemArchitecture.link.artifactId,
      published.definition.systemArchitectureDiagramId,
    )
    assert.equal(
      diagrams.result.processFlow.link.artifactId,
      published.definition.processFlowDiagramId,
    )

    const staleDefinition = {
      ...published.definition,
      processFlowDiagramId: DiagramId('operator-process-stale'),
    }
    const stale = await fixture.service.invoke(reviewRequest(
      'review.approve',
      'approve-stale-review',
      jobId,
      staleDefinition,
    ))
    assert.equal(stale.ok, false)
    assert.equal(stale.error.code, 'STALE_DEFINITION')
    assert.deepEqual(stale.error.currentDefinition, published.definition)

    const approvalRequest = reviewRequest(
      'review.approve',
      'approve-exact-review',
      jobId,
      published.definition,
      { comment: 'Approve these exact four identities.' },
    )
    const competingService = new StrongFlowLocalJobService(fixture.serviceOptions)
    const [approved, competingApproval] = await Promise.all([
      fixture.service.invoke(approvalRequest),
      competingService.invoke(approvalRequest),
    ])
    assert.equal(approved.ok, true, JSON.stringify(approved))
    assert.deepEqual(competingApproval, approved)
    assert.equal(approved.result.job.state, 'PLANNING')
    assert.equal(approved.result.review.payload.decision, 'approved')
    assert.equal(approved.result.event.artifactLinks.length, 1)
    assert.equal(approved.result.event.artifactLinks[0].artifactKind, 'HUMAN_REVIEW_RECORD')
    assert.doesNotMatch(JSON.stringify(approved), /ui-proof/u)

    const restarted = new StrongFlowLocalJobService(fixture.serviceOptions)
    assert.deepEqual(await restarted.invoke(approvalRequest), approved)
    const conflicting = await restarted.invoke(reviewRequest(
      'review.approve',
      'approve-exact-review',
      jobId,
      published.definition,
      { comment: 'Reuse the same request identity with changed content.' },
    ))
    assert.equal(conflicting.ok, false)
    assert.equal(conflicting.error.code, 'JOB_CONFLICT')

    const exported = await restarted.invoke(jobRequest(
      'job.export',
      'export-reviewed-job',
      jobId,
      { format: 'manifest-json' },
    ))
    assert.equal(exported.ok, true)
    assert.ok(exported.result.artifacts.some(link => (
      link.artifactKind === 'HUMAN_REVIEW_RECORD'
      && link.artifactId === approved.result.review.artifactId
    )))
    assert.doesNotMatch(JSON.stringify(exported), /ui-proof/u)
    await new Promise(resolve => setImmediate(resolve))
    assert.ok(schedulerCalls.filter(id => id === jobId).length >= 2)

    const revisionFixture = await operatorFixture(t, 'changes', {
      scheduler: fixture.serviceOptions.scheduler,
    })
    const revisionCreated = await revisionFixture.service.invoke(
      createRequest(revisionFixture, 'changes'),
    )
    assert.equal(revisionCreated.ok, true)
    const revisionJobId = revisionCreated.result.job.jobId
    const revisionDefinition = await publishDefinition(
      revisionFixture,
      revisionJobId,
      'changes',
    )
    const changes = await revisionFixture.service.invoke(reviewRequest(
      'review.request-changes',
      'request-diagram-changes',
      revisionJobId,
      revisionDefinition.definition,
      { scope: 'diagrams', comment: 'Update the diagram details.' },
    ))
    assert.equal(changes.ok, true)
    assert.equal(changes.result.job.state, 'DEFINING_DIAGRAMS')
    assert.equal(changes.result.review.payload.scope, 'diagrams')

    const rejectFixture = await operatorFixture(t, 'reject')
    const rejectCreated = await rejectFixture.service.invoke(
      createRequest(rejectFixture, 'reject'),
    )
    assert.equal(rejectCreated.ok, true)
    const rejectJobId = rejectCreated.result.job.jobId
    const rejectDefinition = await publishDefinition(
      rejectFixture,
      rejectJobId,
      'reject',
    )
    const rejectedAuthentication = await rejectFixture.service.invoke(
      materializeStrongFlowOperatorRequest('review.reject', 'reject-wrong-proof', {
        jobId: rejectJobId,
        definition: rejectDefinition.definition,
        channel: 'local-ui',
        authentication: { scheme: 'local-session', proof: 'wrong-proof' },
        comment: null,
      }),
    )
    assert.equal(rejectedAuthentication.ok, false)
    assert.equal(rejectedAuthentication.error.code, 'AUTHENTICATION_REQUIRED')
    const rejected = await rejectFixture.service.invoke(reviewRequest(
      'review.reject',
      'reject-exact-definition',
      rejectJobId,
      rejectDefinition.definition,
      { comment: 'Reject this exact definition.' },
    ))
    assert.equal(rejected.ok, true)
    assert.equal(rejected.result.job.state, 'REJECTED')
    assert.equal(rejected.result.review.payload.decision, 'rejected')
  })

test('cursor follow, disconnection, interruption, resume, cancel, and restart stay distinct',
  async t => {
    const fixture = await operatorFixture(t, 'lifecycle')
    const created = await fixture.service.invoke(createRequest(fixture, 'lifecycle'))
    assert.equal(created.ok, true)
    const jobId = created.result.job.jobId
    const firstPage = await fixture.service.invoke(jobRequest(
      'job.follow',
      'follow-initial-lifecycle',
      jobId,
      { afterCursor: null, limit: 10, waitMillis: 0 },
    ))
    assert.equal(firstPage.ok, true)
    assert.equal(firstPage.result.events.length, 1)
    const createdCursor = firstPage.result.nextCursor
    assert.ok(createdCursor)

    const store = await StrongFlowJobStore.open(fixture.home, jobId)
    const startedIdentity = {
      stageRunId: StageRunId('operator-run-interrupted-lifecycle'),
      attemptId: AttemptId('operator-attempt-interrupted-lifecycle'),
    }
    const waiting = fixture.service.invoke(jobRequest(
      'job.follow',
      'follow-live-lifecycle',
      jobId,
      { afterCursor: createdCursor, limit: 10, waitMillis: 500 },
    ))
    await new Promise(resolve => setTimeout(resolve, 20))
    let stored = await store.read()
    await store.append(createStrongFlowJobEvent({
      jobId,
      sequence: (BigInt(stored.snapshot.sequence) + 1n).toString(),
      occurredAtMillis: stored.snapshot.lastOccurredAtMillis + 1,
      source: {
        kind: 'role',
        actorId: 'requirements',
        kernelSessionId: KernelSessionId('operator-kernel-interrupted-lifecycle'),
      },
      kind: 'stage.started',
      data: { stage: 'REQUIREMENTS', ...startedIdentity },
    }))
    const livePage = await waiting
    assert.equal(livePage.ok, true)
    assert.deepEqual(livePage.result.events.map(event => event.sequence), ['2'])
    assert.equal(livePage.result.events[0].kind, 'stage.started')

    const abort = new AbortController()
    const disconnected = fixture.service.invoke(jobRequest(
      'job.follow',
      'follow-aborted-lifecycle',
      jobId,
      { afterCursor: livePage.result.nextCursor, limit: 10, waitMillis: 500 },
    ), { signal: abort.signal })
    setTimeout(() => abort.abort(), 20)
    const aborted = await disconnected
    assert.equal(aborted.ok, false)
    assert.equal(aborted.error.code, 'OPERATION_ABORTED')
    assert.equal((await store.read()).snapshot.state, 'DEFINING_REQUIREMENTS')

    stored = await store.read()
    const interruption = createStrongFlowJobEvent({
      jobId,
      sequence: (BigInt(stored.snapshot.sequence) + 1n).toString(),
      occurredAtMillis: stored.snapshot.lastOccurredAtMillis + 1,
      source: { kind: 'system', actorId: 'operator-runtime-lifecycle' },
      kind: 'job.interrupted',
      data: {
        reason: 'Host restart fixture.',
        stageRunId: startedIdentity.stageRunId,
      },
    })
    await store.append(interruption)
    const restarted = new StrongFlowLocalJobService(fixture.serviceOptions)
    const interruptedStatus = await restarted.invoke(jobRequest(
      'job.status',
      'status-interrupted-lifecycle',
      jobId,
    ))
    assert.equal(interruptedStatus.ok, true)
    assert.equal(interruptedStatus.result.job.state, 'INTERRUPTED')
    assert.equal(interruptedStatus.result.job.interruption.sequence, interruption.sequence)
    const resumed = await restarted.invoke(jobRequest(
      'job.resume',
      'resume-interrupted-lifecycle',
      jobId,
      { interruptionSequence: interruption.sequence },
    ))
    assert.equal(resumed.ok, true)
    assert.equal(resumed.result.job.state, 'DEFINING_REQUIREMENTS')
    assert.equal(resumed.result.event.kind, 'job.resumed')

    const cancelNotifications = []
    const cancelFixture = await operatorFixture(t, 'cancel', {
      scheduler: {
        jobReady() {},
        jobCancelled(cancelledJobId, reason) {
          cancelNotifications.push({ cancelledJobId, reason })
        },
      },
    })
    const cancelCreated = await cancelFixture.service.invoke(
      createRequest(cancelFixture, 'cancel'),
    )
    assert.equal(cancelCreated.ok, true)
    const cancelled = await cancelFixture.service.invoke(jobRequest(
      'job.cancel',
      'cancel-explicit-lifecycle',
      cancelCreated.result.job.jobId,
      { reason: 'Explicit operator cancellation.' },
    ))
    assert.equal(cancelled.ok, true)
    assert.equal(cancelled.result.job.state, 'CANCELLED')
    await new Promise(resolve => setImmediate(resolve))
    assert.deepEqual(cancelNotifications, [{
      cancelledJobId: cancelCreated.result.job.jobId,
      reason: 'Explicit operator cancellation.',
    }])
    const cancelRestart = new StrongFlowLocalJobService(cancelFixture.serviceOptions)
    const cancelStatus = await cancelRestart.invoke(jobRequest(
      'job.status',
      'status-cancelled-lifecycle',
      cancelCreated.result.job.jobId,
    ))
    assert.equal(cancelStatus.ok, true)
    assert.equal(cancelStatus.result.job.state, 'CANCELLED')
  })
