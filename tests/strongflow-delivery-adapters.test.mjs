import assert from 'node:assert/strict'
import { mkdtemp, readFile, readdir, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { Context } from '@deepseek-ai/cordis'
import TypertGatewayService from '@deepseek-ai/dsh-api-gateway'
import { remoteMethods } from '@deepseek-ai/dsh-typert-protocol'
import { TypertRegistry } from '@deepseek-ai/dsh-typert-registry'

import {
  DELIVERY_SCHEMA_VERSION,
  materializeStrongFlowDeliveryAdvanceRequest,
  materializeStrongFlowDeliveryRequest,
  parseDelivery,
  parseStrongFlowPlanReviewContextText,
} from '../packages/contracts/dist/index.js'
import {
  DeliveryStore,
  StrongFlowDeliveryRemoteService,
  StrongFlowService,
  StrongFlowServiceInvoker,
  createStrongFlowPlanReviewAttention,
  createStrongFlowPlanReviewDecision,
  createStrongFlowDeliveryLocalProofAuthenticator,
} from '../packages/strongflow/dist/index.js'
import { runStrongFlowCli } from '../apps/host/dist/index.js'

const baseTime = 2_200_000_000_000
const uiProof = 'ui-proof-for-delivery-fixture'
const cliProof = 'cli-proof-for-delivery-fixture'

function spec(deliveryId, revision = 1) {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `delivery-spec-${deliveryId}-v${revision}`,
    deliveryId,
    revision,
    title: `Delivery ${deliveryId}`,
    goal: 'Use one durable Delivery service from DSH and CLI.',
    scope: ['Delivery service adapters'],
    outOfScope: ['Generic project management'],
    constraints: ['Codex remains the execution authority'],
    acceptanceCriteria: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `criterion-${deliveryId}`,
      description: 'DSH and CLI observe the same Delivery revision.',
      verificationMethod: 'Run the adapter process test.',
      required: true,
    }],
    sourceRef: null,
    publicationTarget: null,
    repository: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      kind: 'local-git',
      locator: '/workspace/repository',
    },
    baseRevision: '0123456789012345678901234567890123456789',
    maxReworkAttempts: 2,
    createdAtMillis: baseTime + revision,
  }
}

function authenticator() {
  return createStrongFlowDeliveryLocalProofAuthenticator({
    localSessionProof: uiProof,
    localPeerProof: cliProof,
    localSessionActorId: 'reviewer-ui',
    localPeerActorId: 'reviewer-cli',
  })
}

function unusedCoordinator() {
  return Object.freeze({
    async advance() {
      throw new Error('stage coordinator is not used by this adapter fixture')
    },
  })
}

function planReviewSolution() {
  return {
    id: 'solution-adapter-review',
    summary: 'Route the exact reviewed delivery through the one StrongFlow service.',
    approach: ['Keep transport adapters stateless.', 'Authenticate only the human decision.'],
    components: [{
      id: 'component-adapter-review',
      label: 'Delivery adapter',
      responsibility: 'Carry the canonical request without owning Delivery state.',
      kind: 'component',
      trustBoundary: 'Transport boundary',
      unresolved: false,
      repositoryPathPrefixes: ['packages/strongflow'],
    }],
    connections: [{
      id: 'connection-adapter-review',
      from: 'platform:dsh',
      to: 'component-adapter-review',
      label: 'Sends the reviewed human action',
    }],
  }
}

async function fixture(t, name) {
  const home = await mkdtemp(join(tmpdir(), `winwincode-delivery-adapter-${name}-`))
  t.after(() => rm(home, { recursive: true, force: true }))
  let now = baseTime + 100
  const service = new StrongFlowService({
    home,
    authenticator: authenticator(),
    clock: () => ++now,
  })
  return { home, service, invoker: new StrongFlowServiceInvoker(service) }
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

test('DSH Remote and CLI read and create through one durable Delivery service', async t => {
  const current = await fixture(t, 'shared')
  const ctx = new Context()
  await ctx.plugin(TypertRegistry)
  await ctx.plugin(TypertGatewayService)
  const remoteAgent = Object.freeze({
    id: 'dsh-remote-fixture',
    options: Object.freeze({
      provider: 'fixture-provider',
      model: 'fixture-model',
      maxTokens: 4_096,
    }),
  })
  ctx.typert.lookups.register('agent', {
    parameter: 'agent',
    wire: 'agentId',
    hostTypeSymbol: '@deepseek-ai/dsh-agent#Agent',
    wireTypeSymbol: '@deepseek-ai/dsh-session/types#SessionId',
    resolve: sessionId => sessionId === remoteAgent.id ? remoteAgent : undefined,
  })
  let remote
  const advanceCalls = []
  const coordinator = {
    async advance(request, caller) {
      advanceCalls.push({ request, caller })
      return {
        delivery: (await current.service.getDeliveryProjection(request.deliveryId)).delivery,
        outcome: {
          kind: 'stage-busy',
          message: 'fixture stage remains active',
          stageRunId: null,
          dshSessionId: caller.dshSessionId,
        },
      }
    },
  }
  const plugin = pluginContext => {
    remote = new StrongFlowDeliveryRemoteService(pluginContext, current.invoker, {
      localSessionProof: uiProof,
      coordinator,
    })
  }
  await ctx.plugin(plugin)
  t.after(() => ctx.fiber.dispose())
  assert.ok(remote)
  assert.deepEqual(remoteMethods(remote), [
    { method: 'advance', invocation: { kind: 'direct' } },
    { method: 'invoke', invocation: { kind: 'direct' } },
  ])

  const invalidRemote = await remote.invoke(
    remoteAgent,
    {},
    new AbortController().signal,
  )
  assert.equal(invalidRemote.ok, false)
  assert.equal(invalidRemote.error.code, 'INVALID_REQUEST')
  assert.equal(invalidRemote.requestId, null)

  const remoteDeliveryId = 'delivery-created-from-remote'
  const remoteCreate = await ctx.typertGateway.invoke({
    namespace: 'strongflow',
    method: 'invoke',
    args: {
      agentId: 'dsh-remote-fixture',
      request: materializeStrongFlowDeliveryRequest('createDelivery', 'remote-create', {
        spec: spec(remoteDeliveryId),
        tasks: [],
      }),
    },
    signal: new AbortController().signal,
  })
  assert.equal(remoteCreate.ok, true, JSON.stringify(remoteCreate))

  const advanced = await ctx.typertGateway.invoke({
    namespace: 'strongflow',
    method: 'advance',
    args: {
      agentId: remoteAgent.id,
      request: materializeStrongFlowDeliveryAdvanceRequest(
        'remote-advance',
        remoteDeliveryId,
        remoteCreate.result.delivery.revision,
      ),
    },
    signal: new AbortController().signal,
  })
  assert.equal(advanced.ok, true, JSON.stringify(advanced))
  assert.equal(advanced.result.outcome.kind, 'stage-busy')
  assert.deepEqual(advanceCalls[0].caller, {
    dshSessionId: remoteAgent.id,
    modelRoute: {
      provider: 'fixture-provider',
      model: 'fixture-model',
      maxTokens: 4_096,
    },
  })

  const cliStdout = []
  const cliStderr = []
  const cliShowCode = await runStrongFlowCli([
    'delivery',
    'show',
    remoteDeliveryId,
    '--request-id',
    'cli-show-remote-delivery',
    '--json',
  ], current.invoker, {
    stdout: text => cliStdout.push(text),
    stderr: text => cliStderr.push(text),
  })
  assert.equal(cliShowCode, 0)
  assert.equal(cliStderr.length, 0)
  assert.equal(JSON.parse(cliStdout.join('')).result.delivery.id, remoteDeliveryId)

  const cliDeliveryId = 'delivery-created-from-cli'
  const cliCreateOutput = []
  const cliCreateCode = await runStrongFlowCli([
    'delivery',
    'create',
    '--spec',
    'spec.json',
    '--request-id',
    'cli-create-delivery',
    '--json',
  ], current.invoker, {
    readTextFile: async path => {
      assert.equal(path, 'spec.json')
      return JSON.stringify(spec(cliDeliveryId))
    },
    stdout: text => cliCreateOutput.push(text),
    stderr: text => assert.fail(`unexpected CLI error: ${text}`),
  })
  assert.equal(cliCreateCode, 0)
  assert.equal(JSON.parse(cliCreateOutput.join('')).result.delivery.id, cliDeliveryId)

  const remoteShow = await remote.invoke(
    remoteAgent,
    materializeStrongFlowDeliveryRequest('getDeliveryProjection', 'remote-show-cli', {
      deliveryId: cliDeliveryId,
    }),
    new AbortController().signal,
  )
  assert.equal(remoteShow.ok, true)
  assert.equal(remoteShow.result.delivery.id, cliDeliveryId)
})

test('Attention resolution authenticates the human and never stores the proof', async t => {
  const current = await fixture(t, 'attention')
  const deliveryId = 'delivery-adapter-attention'
  const currentSpec = spec(deliveryId)
  const planningRun = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'stage-adapter-planning',
    deliveryId,
    deliveryTaskId: null,
    stage: 'planning',
    actorType: 'codex',
    role: 'planner',
    status: 'running',
    attempt: 1,
    startedAtMillis: baseTime + 5,
    finishedAtMillis: null,
  }
  const planningBinding = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'binding-adapter-planning',
    deliveryId,
    stageRunId: planningRun.id,
    dshSessionId: 'dsh-adapter-planning',
    codexSessionId: 'codex-adapter-planning',
    boundAtMillis: baseTime + 6,
  }
  const planningDelivery = parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'planning',
    spec: currentSpec,
    tasks: [],
    stageRuns: [planningRun],
    sessionBindings: [planningBinding],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 6,
  })
  const reviewAttention = createStrongFlowPlanReviewAttention({
    delivery: planningDelivery,
    attentionItemId: 'attention-adapter-review',
    reviewStageRunId: 'stage-adapter-review',
    assignedTo: 'reviewer-cli',
    solution: planReviewSolution(),
    risks: [],
    unresolvedItems: [],
    preparedAtMillis: baseTime + 7,
  })
  const approvedReview = createStrongFlowPlanReviewDecision({
    context: parseStrongFlowPlanReviewContextText(reviewAttention.context),
    action: 'approve',
    comments: 'Approve the exact adapter review set.',
    requestedChanges: [],
  })
  await DeliveryStore.create({
    home: current.home,
    requestId: 'seed-attention',
    requestDigest: 'd'.repeat(64),
    snapshot: parseDelivery({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: deliveryId,
      revision: 1,
      status: 'needs-attention',
      spec: currentSpec,
      tasks: [],
      stageRuns: [{
        ...planningRun,
        status: 'succeeded',
        finishedAtMillis: baseTime + 8,
      }, {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-adapter-review',
        deliveryId,
        deliveryTaskId: null,
        stage: 'plan-review',
        actorType: 'human',
        role: 'reviewer',
        status: 'waiting',
        attempt: 1,
        startedAtMillis: baseTime + 10,
        finishedAtMillis: null,
      }],
      sessionBindings: [planningBinding, {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-adapter-review',
        deliveryId,
        stageRunId: 'stage-adapter-review',
        dshSessionId: 'dsh-adapter-review',
        codexSessionId: null,
        boundAtMillis: baseTime + 11,
      }],
      attentionItems: [reviewAttention],
      evidence: [],
      verdict: null,
      createdAtMillis: baseTime,
      updatedAtMillis: baseTime + 11,
    }),
  })

  const rejected = await current.invoker.invoke(materializeStrongFlowDeliveryRequest(
    'resolveAttention',
    'resolve-wrong-proof',
    {
      deliveryId,
      expectedRevision: 1,
      attentionItemId: 'attention-adapter-review',
      status: 'resolved',
      resolution: JSON.stringify(approvedReview),
      remediation: null,
      channel: 'local-ui',
      authentication: {
        scheme: 'local-session',
        proof: 'wrong-proof-for-delivery-fixture',
      },
    },
  ))
  assert.equal(rejected.ok, false)
  assert.equal(rejected.error.code, 'AUTHENTICATION_FAILED')
  assert.equal((await current.service.getDeliveryProjection(deliveryId)).delivery.revision, 1)

  const stdout = []
  const code = await runStrongFlowCli([
    'delivery',
    'resolve-attention',
    deliveryId,
    '--expected-revision',
    '1',
    '--attention-id',
    'attention-adapter-review',
    '--decision',
    'resolved',
    '--resolution',
    JSON.stringify(approvedReview),
    '--auth',
    cliProof,
    '--request-id',
    'resolve-correct-proof',
    '--json',
  ], current.invoker, {
    stdout: text => stdout.push(text),
    stderr: text => assert.fail(`unexpected CLI error: ${text}`),
  })
  assert.equal(code, 0)
  const response = JSON.parse(stdout.join(''))
  assert.equal(response.result.delivery.status, 'executing')
  assert.equal(response.result.delivery.attentionItems[0].resolvedBy, 'reviewer-cli')
  assert.equal(await treeContainsBytes(current.home, Buffer.from(cliProof, 'utf8')), false)
})

test('local UI plan review accepts only the bound DSH Session and current revision', async t => {
  const current = await fixture(t, 'local-ui-review')
  const deliveryId = 'delivery-adapter-local-ui-review'
  const currentSpec = spec(deliveryId)
  const planningRun = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'stage-local-ui-planning',
    deliveryId,
    deliveryTaskId: null,
    stage: 'planning',
    actorType: 'codex',
    role: 'planner',
    status: 'running',
    attempt: 1,
    startedAtMillis: baseTime + 5,
    finishedAtMillis: null,
  }
  const planningBinding = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'binding-local-ui-planning',
    deliveryId,
    stageRunId: planningRun.id,
    dshSessionId: 'dsh-local-ui-planning',
    codexSessionId: 'codex-local-ui-planning',
    boundAtMillis: baseTime + 6,
  }
  const planningDelivery = parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'planning',
    spec: currentSpec,
    tasks: [],
    stageRuns: [planningRun],
    sessionBindings: [planningBinding],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: baseTime,
    updatedAtMillis: baseTime + 6,
  })
  const reviewAttention = createStrongFlowPlanReviewAttention({
    delivery: planningDelivery,
    attentionItemId: 'attention-local-ui-review',
    reviewStageRunId: 'stage-local-ui-review',
    assignedTo: 'reviewer-ui',
    solution: planReviewSolution(),
    risks: [],
    unresolvedItems: [],
    preparedAtMillis: baseTime + 7,
  })
  const approvedReview = createStrongFlowPlanReviewDecision({
    context: parseStrongFlowPlanReviewContextText(reviewAttention.context),
    action: 'approve',
    comments: 'Approve the current browser review set.',
    requestedChanges: [],
  })
  await DeliveryStore.create({
    home: current.home,
    requestId: 'seed-local-ui-review',
    requestDigest: 'e'.repeat(64),
    snapshot: parseDelivery({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: deliveryId,
      revision: 1,
      status: 'needs-attention',
      spec: currentSpec,
      tasks: [],
      stageRuns: [{
        ...planningRun,
        status: 'succeeded',
        finishedAtMillis: baseTime + 8,
      }, {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'stage-local-ui-review',
        deliveryId,
        deliveryTaskId: null,
        stage: 'plan-review',
        actorType: 'human',
        role: 'reviewer',
        status: 'waiting',
        attempt: 1,
        startedAtMillis: baseTime + 9,
        finishedAtMillis: null,
      }],
      sessionBindings: [planningBinding, {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'binding-local-ui-review',
        deliveryId,
        stageRunId: 'stage-local-ui-review',
        dshSessionId: 'dsh-local-ui-reviewer',
        codexSessionId: null,
        boundAtMillis: baseTime + 10,
      }],
      attentionItems: [reviewAttention],
      evidence: [],
      verdict: null,
      createdAtMillis: baseTime,
      updatedAtMillis: baseTime + 10,
    }),
  })

  const ctx = new Context()
  await ctx.plugin(TypertRegistry)
  await ctx.plugin(TypertGatewayService)
  const agents = new Map([
    ['dsh-local-ui-reviewer', Object.freeze({ id: 'dsh-local-ui-reviewer' })],
    ['dsh-local-ui-observer', Object.freeze({ id: 'dsh-local-ui-observer' })],
  ])
  ctx.typert.lookups.register('agent', {
    parameter: 'agent',
    wire: 'agentId',
    hostTypeSymbol: '@deepseek-ai/dsh-agent#Agent',
    wireTypeSymbol: '@deepseek-ai/dsh-session/types#SessionId',
    resolve: sessionId => agents.get(sessionId),
  })
  const plugin = pluginContext => {
    new StrongFlowDeliveryRemoteService(pluginContext, current.invoker, {
      localSessionProof: uiProof,
      coordinator: unusedCoordinator(),
    })
  }
  await ctx.plugin(plugin)
  t.after(() => ctx.fiber.dispose())

  const decisionRequest = (requestId, expectedRevision) => (
    materializeStrongFlowDeliveryRequest('resolveAttention', requestId, {
      deliveryId,
      expectedRevision,
      attentionItemId: reviewAttention.id,
      status: 'resolved',
      resolution: JSON.stringify(approvedReview),
      remediation: null,
      channel: 'local-ui',
      authentication: {
        scheme: 'local-session',
        proof: 'dsh-reference-only',
      },
    })
  )
  const invokeAs = (agentId, request) => ctx.typertGateway.invoke({
    namespace: 'strongflow',
    method: 'invoke',
    args: { agentId, request },
    signal: new AbortController().signal,
  })

  const wrongSession = await invokeAs(
    'dsh-local-ui-observer',
    decisionRequest('local-ui-wrong-session', 1),
  )
  assert.equal(wrongSession.ok, false)
  assert.equal(wrongSession.error.code, 'AUTHENTICATION_FAILED')
  assert.equal(
    (await current.service.getDeliveryProjection(deliveryId)).delivery.status,
    'needs-attention',
  )

  const stale = await invokeAs(
    'dsh-local-ui-reviewer',
    decisionRequest('local-ui-stale-screen', 2),
  )
  assert.equal(stale.ok, false)
  assert.equal(stale.error.code, 'REVISION_CONFLICT')
  assert.equal(stale.error.currentRevision, 1)
  assert.equal(
    (await current.service.getDeliveryProjection(deliveryId)).delivery.status,
    'needs-attention',
  )

  const approved = await invokeAs(
    'dsh-local-ui-reviewer',
    decisionRequest('local-ui-current-review', 1),
  )
  assert.equal(approved.ok, true)
  assert.equal(approved.result.delivery.status, 'executing')
  assert.equal(approved.result.delivery.attentionItems[0].resolvedBy, 'reviewer-ui')
  assert.equal(await treeContainsBytes(current.home, Buffer.from(uiProof, 'utf8')), false)
})

test('invoker reports current revision and aborts only before dispatch', async t => {
  const current = await fixture(t, 'failures')
  const deliveryId = 'delivery-adapter-failures'
  await current.invoker.invoke(materializeStrongFlowDeliveryRequest(
    'createDelivery',
    'create-for-conflict',
    { spec: spec(deliveryId), tasks: [] },
  ))
  const stale = await current.invoker.invoke(materializeStrongFlowDeliveryRequest(
    'updateDeliverySpec',
    'update-stale',
    {
      deliveryId,
      expectedRevision: 2,
      spec: spec(deliveryId, 2),
    },
  ))
  assert.equal(stale.ok, false)
  assert.equal(stale.error.code, 'REVISION_CONFLICT')
  assert.equal(stale.error.currentRevision, 1)

  const abort = new AbortController()
  abort.abort()
  const aborted = await current.invoker.invoke(materializeStrongFlowDeliveryRequest(
    'getDeliveryProjection',
    'show-aborted',
    { deliveryId },
  ), { signal: abort.signal })
  assert.equal(aborted.ok, false)
  assert.equal(aborted.error.code, 'OPERATION_ABORTED')
})
