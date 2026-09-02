import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import vm from 'node:vm'
import { fileURLToPath } from 'node:url'
import * as React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'

import {
  DELIVERY_SCHEMA_VERSION,
  DeliveryId,
  DeliveryValidationError,
  deliveryIdForGitHubIssueSource,
  generateDeliveryId,
  parseDelivery,
  parseDeliverySpec,
  parseGitHubIssueSourceRef,
  parseGitHubPullRequestTargetRef,
  parseStrongFlowGitHubPublicationContextText,
} from '../packages/contracts/dist/index.js'
import {
  DeliveryStore,
  StrongFlowGitHubPublicationError,
  StrongFlowService,
  StrongFlowServiceError,
  assertStrongFlowGitHubPublicationCurrent,
  createStrongFlowGitHubPublicationAttention,
  createStrongFlowGitHubPublicationDecision,
  freezeDeliveryCandidate,
} from '../packages/strongflow/dist/index.js'

const root = fileURLToPath(new URL('../', import.meta.url))

function loadStrongFlowClient() {
  let registration
  vm.runInNewContext(
    readFileSync(join(root, 'packages', 'strongflow', 'dist', 'client.js'), 'utf8'),
    {
      crypto: globalThis.crypto,
      Symbol,
      structuredClone,
      window: {
        __ModuleLoader__: {
          load(value) { registration = value },
        },
      },
    },
  )
  assert.equal(registration?.id, '@winwincode/strongflow')
  return registration.factory(id => {
    if (id === 'react') return React
    throw new Error(`unexpected StrongFlow client dependency: ${id}`)
  })
}

const now = 2_400_000_000_000
const proof = 'github-publication-fixture-proof'
const githubIssueIdentityNamespace = 'winwincode.github-issue-delivery-id.v1'

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

const deliveryId = DeliveryId('dlv_7TEPT1B6JF7W5SASWZMKTCC4KT')
const specId = `${deliveryId}:spec:1`
const criterionId = `${deliveryId}:criterion:1`
const taskId = `${deliveryId}:task:1`
const producerStageRunId = `${deliveryId}:stage:execute:1`
const producerBindingId = `${deliveryId}:binding:execute:1`
const verifierStageRunId = `${deliveryId}:stage:verify:1`
const verifierBindingId = `${deliveryId}:binding:verify:1`

function deliverySpec(overrides = {}) {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: specId,
    deliveryId,
    revision: 1,
    title: 'Deliver the GitHub issue result',
    goal: 'Bind one external issue to one reviewed pull request.',
    scope: ['Issue result'],
    outOfScope: ['GitHub comments and project boards'],
    constraints: ['Codex Core remains the execution authority'],
    acceptanceCriteria: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: criterionId,
      description: 'The current candidate passes its direct check.',
      verificationMethod: 'Run the direct check.',
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
    createdAtMillis: now,
    ...overrides,
  }
}

function stageRuns() {
  return [{
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: producerStageRunId,
    deliveryId,
    deliveryTaskId: taskId,
    stage: 'executing',
    actorType: 'codex',
    role: 'executor',
    status: 'succeeded',
    attempt: 1,
    startedAtMillis: now + 10,
    finishedAtMillis: now + 20,
  }, {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: verifierStageRunId,
    deliveryId,
    deliveryTaskId: taskId,
    stage: 'verifying',
    actorType: 'codex',
    role: 'verifier',
    status: 'succeeded',
    attempt: 1,
    startedAtMillis: now + 21,
    finishedAtMillis: now + 30,
  }]
}

function sessionBindings() {
  return [{
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: producerBindingId,
    deliveryId,
    stageRunId: producerStageRunId,
    dshSessionId: 'dsh-github-executor',
    codexSessionId: 'codex-github-executor',
    boundAtMillis: now + 11,
  }, {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: verifierBindingId,
    deliveryId,
    stageRunId: verifierStageRunId,
    dshSessionId: 'dsh-github-verifier',
    codexSessionId: 'codex-github-verifier',
    boundAtMillis: now + 22,
  }]
}

function interimDelivery() {
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'verifying',
    spec: deliverySpec(),
    tasks: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: taskId,
      deliveryId,
      title: 'GitHub issue result',
      goal: 'Produce one independently verifiable candidate.',
      acceptanceCriterionIds: [criterionId],
      blockedByTaskIds: [],
      owner: null,
      status: 'verifying',
    }],
    stageRuns: stageRuns(),
    sessionBindings: sessionBindings(),
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 30,
  })
}

function frozenCandidate() {
  return freezeDeliveryCandidate(interimDelivery(), {
    producerStageRunId,
    producerSessionBindingId: producerBindingId,
    baseCommitId: '1'.repeat(40),
    baseTreeId: '2'.repeat(40),
    candidateCommitId: '3'.repeat(40),
    candidateTreeId: '4'.repeat(40),
    diffSha256: '5'.repeat(64),
    changedPaths: [{
      path: 'src/result.ts',
      state: 'present',
      objectId: '6'.repeat(40),
    }],
  })
}

function readyDelivery() {
  const candidate = frozenCandidate()
  const evidenceId = `${deliveryId}:evidence:1`
  return parseDelivery({
    ...interimDelivery(),
    status: 'ready-to-deliver',
    tasks: [{
      ...interimDelivery().tasks[0],
      status: 'completed',
    }],
    evidence: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: evidenceId,
      deliveryId,
      deliverySpecId: specId,
      deliverySpecRevision: 1,
      stageRunId: verifierStageRunId,
      sessionBindingId: verifierBindingId,
      candidateRef: candidate.candidateRef,
      type: 'test',
      sourceRef: 'runtime_event:dsh-github-verifier@1',
      createdAtMillis: now + 30,
    }],
    verdict: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `${deliveryId}:verdict:1`,
      deliveryId,
      deliverySpecId: specId,
      candidateRef: candidate.candidateRef,
      status: 'pass',
      criteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: `${deliveryId}:criterion-result:1`,
        deliveryId,
        deliverySpecId: specId,
        criterionId,
        candidateRef: candidate.candidateRef,
        verdict: 'pass',
        evidenceRefs: [evidenceId],
        explanation: 'The direct check passed.',
        evaluatedAtMillis: now + 31,
      }],
      unresolvedFindings: [],
      producedAtMillis: now + 32,
    },
    updatedAtMillis: now + 32,
  })
}

function expectPublicationError(code) {
  return error => error instanceof StrongFlowGitHubPublicationError && error.code === code
}

test('GitHub references remain exact while Delivery keeps its canonical identity', () => {
  const parsedSource = parseGitHubIssueSourceRef({
    ...sourceRef,
    repository: 'Example/Widget',
  })
  assert.equal(parsedSource.repository, 'example/widget')
  assert.equal(deliveryIdForGitHubIssueSource(parsedSource), deliveryId)
  assert.throws(
    () => parseGitHubIssueSourceRef({ ...sourceRef, title: 'duplicated issue title' }),
    error => error instanceof DeliveryValidationError && error.code === 'INVALID_SHAPE',
  )
  assert.equal(parseDeliverySpec(deliverySpec()).deliveryId, deliveryId)
  assert.throws(
    () => parseDeliverySpec({
      ...deliverySpec(),
      deliveryId: 'dlv_01J00000000000000000000000',
    }),
    error => error instanceof DeliveryValidationError
      && error.code === 'RELATIONSHIP_MISMATCH',
  )
  assert.throws(() => DeliveryId('github-issue:example/widget:42'), error => (
    error instanceof DeliveryValidationError && error.code === 'INVALID_IDENTIFIER'
  ))
  assert.throws(
    () => parseGitHubPullRequestTargetRef({
      ...publicationTarget,
      headBranch: publicationTarget.baseBranch,
    }),
    error => error instanceof DeliveryValidationError && error.code === 'INVALID_VALUE',
  )
})

test('TypeScript GitHub Delivery identities match fixed Node SHA-256 vectors', () => {
  const vectors = [
    {
      repository: 'Example/Widget',
      number: 42,
      sha256: 'fa75b4159a4f3f0b95679fa4f4c6127a14a3c8287161099b492db3cc44c43df2',
      deliveryId: 'dlv_7TEPT1B6JF7W5SASWZMKTCC4KT',
    },
    {
      repository: 'example/widget',
      number: 43,
      sha256: 'b6eeb6858632327056507c5fa8555b30f2dcb8944cf67d88a8f5e1353d0e2e73',
      deliveryId: 'dlv_5PXTV8B1HJ69R5CM3WBYM5APSG',
    },
    {
      repository: 'contributor/widget',
      number: 42,
      sha256: 'dcd30d46815fe94efdf852d0c578262f6c0766f023794bb5bc8c6ba816693328',
      deliveryId: 'dlv_6WTC6MD0AZX57FVY2JT32QG9HF',
    },
  ]
  for (const vector of vectors) {
    const parsed = parseGitHubIssueSourceRef({
      ...sourceRef,
      repository: vector.repository,
      number: vector.number,
    })
    const canonicalBytes = [
      githubIssueIdentityNamespace,
      'github',
      'issue',
      parsed.repository,
      String(parsed.number),
    ].join('\0')
    assert.equal(createHash('sha256').update(canonicalBytes).digest('hex'), vector.sha256)
    assert.equal(deliveryIdForGitHubIssueSource(parsed), vector.deliveryId)
  }
})

test('source-less creation can generate a canonical ULID identity', () => {
  const generated = generateDeliveryId(now)
  assert.match(generated, /^dlv_[0-9A-HJKMNP-TV-Z]{26}$/u)

  const request = loadStrongFlowClient().createDeliveryRequestFromDraft({
    deliveryId: '',
    title: 'Source-less delivery',
    goal: 'Create one local Delivery without an external source.',
    scope: 'Local result',
    outOfScope: 'External publication',
    constraints: 'Keep one canonical identity',
    criteria: 'Local check passes | Run local check',
    repositoryKind: 'local-git',
    repositoryLocator: '/workspace/example',
    baseRevision: '1'.repeat(40),
    maxReworkAttempts: '2',
    githubIssue: '',
    githubBaseBranch: '',
    githubHeadRepository: '',
    githubHeadBranch: '',
  }, 'ui:create:source-less', now)
  assert.match(request.payload.spec.deliveryId, /^dlv_[0-9A-HJKMNP-TV-Z]{26}$/u)
  assert.equal(request.payload.spec.sourceRef, null)
  assert.equal(request.payload.spec.publicationTarget, null)
})

test('StrongFlow create form materializes the typed GitHub source and single PR target', () => {
  const client = loadStrongFlowClient()
  const draft = {
    deliveryId,
    title: 'GitHub delivery',
    goal: 'Deliver one issue through one reviewed pull request.',
    scope: 'Issue result',
    outOfScope: 'GitHub project board',
    constraints: 'Codex Core remains the execution authority',
    criteria: 'Direct check passes | Run direct check',
    repositoryKind: 'github',
    repositoryLocator: 'example/widget',
    baseRevision: '1'.repeat(40),
    maxReworkAttempts: '2',
    githubIssue: 'Example/Widget#42',
    githubBaseBranch: 'main',
    githubHeadRepository: '',
    githubHeadBranch: 'winwincode/issue-42',
  }
  const request = client.createDeliveryRequestFromDraft(
    draft,
    'ui:create:github-issue:42',
    now,
  )
  assert.equal(request.payload.spec.deliveryId, deliveryId)
  assert.equal(JSON.stringify(request.payload.spec.sourceRef), JSON.stringify(sourceRef))
  assert.equal(
    JSON.stringify(request.payload.spec.publicationTarget),
    JSON.stringify(publicationTarget),
  )
  assert.throws(() => client.createDeliveryRequestFromDraft({
    ...draft,
    deliveryId: 'dlv_01J00000000000000000000000',
  }, 'ui:create:github-issue:42:mismatch', now), error => (
    error?.code === 'RELATIONSHIP_MISMATCH'
  ))
})

test('different create requests for the same GitHub issue keep one Delivery identity', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-github-create-identity-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  let clock = now + 80
  const service = new StrongFlowService({
    home,
    clock: () => ++clock,
    authenticator: { async authenticate() { return undefined } },
  })
  const specification = deliverySpec()

  const created = await service.createDelivery({
    requestId: 'github-create-first-request',
    spec: specification,
    tasks: [],
  })
  assert.equal(created.id, deliveryId)
  await assert.rejects(service.createDelivery({
    requestId: 'github-create-second-request',
    spec: specification,
    tasks: [],
  }), error => (
    error instanceof StrongFlowServiceError && error.code === 'DELIVERY_CONFLICT'
  ))

  const stored = await (await DeliveryStore.open(home, deliveryId)).read()
  assert.equal(stored.records.length, 1)
  assert.equal(stored.snapshot.id, deliveryId)
})

test('publication Attention freezes the exact source, target, candidate, verdict, and stable PR key', () => {
  const delivery = readyDelivery()
  const candidate = frozenCandidate()
  const first = createStrongFlowGitHubPublicationAttention({
    delivery,
    candidate,
    attentionItemId: `${deliveryId}:attention:publish:1`,
    reviewStageRunId: `${deliveryId}:stage:delivery-review:1`,
    assignedTo: 'approver-1',
    preparedAtMillis: now + 40,
  })
  const replay = createStrongFlowGitHubPublicationAttention({
    delivery,
    candidate,
    attentionItemId: `${deliveryId}:attention:publish:1`,
    reviewStageRunId: `${deliveryId}:stage:delivery-review:1`,
    assignedTo: 'approver-1',
    preparedAtMillis: now + 40,
  })
  assert.deepEqual(replay, first)
  const context = parseStrongFlowGitHubPublicationContextText(first.context)
  assert.deepEqual(context.sourceRef, sourceRef)
  assert.deepEqual(context.publicationTarget, publicationTarget)
  assert.equal(context.candidateRef, candidate.candidateRef)
  assert.equal(context.deliveryVerdictId, delivery.verdict.id)
  assert.match(context.providerIdempotencyKey, /^github:pull-request:sha256:[a-f0-9]{64}$/u)
  assert.equal(first.context.includes('GitHub comments'), false)
})

test('DeliverySpec keeps one source issue and one immutable intended pull request', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-github-binding-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  let clock = now + 100
  const service = new StrongFlowService({
    home,
    clock: () => ++clock,
    authenticator: { async authenticate() { return undefined } },
  })
  const created = await service.createDelivery({
    requestId: 'github-binding-create',
    spec: deliverySpec(),
    tasks: [],
  })
  await assert.rejects(service.updateDeliverySpec({
    requestId: 'github-binding-change-target',
    deliveryId,
    expectedRevision: created.revision,
    spec: deliverySpec({
      id: `${deliveryId}:spec:2`,
      revision: 2,
      publicationTarget: {
        ...publicationTarget,
        baseBranch: 'release',
      },
      createdAtMillis: now + 1,
    }),
  }), error => error instanceof StrongFlowServiceError && error.code === 'DELIVERY_CONFLICT')
  const reopened = (await new StrongFlowService({
    home,
    authenticator: { async authenticate() { return undefined } },
  }).getDeliveryProjection(deliveryId)).delivery
  assert.deepEqual(reopened.spec.sourceRef, sourceRef)
  assert.deepEqual(reopened.spec.publicationTarget, publicationTarget)
})

test('service requires the exact structured human publication decision', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-github-publication-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const delivery = readyDelivery()
  const candidate = frozenCandidate()
  await DeliveryStore.create({
    home,
    requestId: 'github-publication-seed',
    requestDigest: '7'.repeat(64),
    snapshot: delivery,
  })
  let clock = now + 40
  const service = new StrongFlowService({
    home,
    clock: () => ++clock,
    authenticator: {
      async authenticate(request) {
        return request.authentication.proof === proof
          ? Object.freeze({ actorId: 'approver-1' })
          : undefined
      },
    },
  })
  const reviewStageRunId = `${deliveryId}:stage:delivery-review:1`
  const attention = createStrongFlowGitHubPublicationAttention({
    delivery,
    candidate,
    attentionItemId: `${deliveryId}:attention:publish:1`,
    reviewStageRunId,
    assignedTo: 'approver-1',
    preparedAtMillis: now + 40,
  })
  await assert.rejects(service.startStage({
    requestId: 'github-publication-generic-attention',
    deliveryId,
    expectedRevision: 1,
    stageRunId: `${deliveryId}:stage:delivery-review:generic`,
    deliveryTaskId: null,
    stage: 'delivery-review',
    actorType: 'human',
    role: 'approver',
    attention: {
      ...attention,
      id: `${deliveryId}:attention:generic`,
      stageRunId: `${deliveryId}:stage:delivery-review:generic`,
      context: 'Approve this candidate.',
    },
  }), error => error instanceof StrongFlowServiceError && error.code === 'INVALID_REQUEST')

  const reviewing = await service.startStage({
    requestId: 'github-publication-start-review',
    deliveryId,
    expectedRevision: 1,
    stageRunId: reviewStageRunId,
    deliveryTaskId: null,
    stage: 'delivery-review',
    actorType: 'human',
    role: 'approver',
    attention,
  })
  const bound = await service.bindSession({
    requestId: 'github-publication-bind-review',
    deliveryId,
    expectedRevision: reviewing.revision,
    bindingId: `${deliveryId}:binding:delivery-review:1`,
    stageRunId: reviewStageRunId,
    dshSessionId: 'dsh-github-publication-review',
    codexSessionId: null,
  })
  await assert.rejects(service.resolveAttention({
    requestId: 'github-publication-unstructured-decision',
    deliveryId,
    expectedRevision: bound.revision,
    attentionItemId: attention.id,
    status: 'resolved',
    resolution: 'Looks good.',
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof },
  }), error => error instanceof StrongFlowServiceError && error.code === 'INVALID_REQUEST')

  const context = parseStrongFlowGitHubPublicationContextText(attention.context)
  const decision = createStrongFlowGitHubPublicationDecision({
    context,
    comments: 'Reviewed the exact candidate, verdict, source issue, and PR destination.',
  })
  const delivered = await service.resolveAttention({
    requestId: 'github-publication-approve',
    deliveryId,
    expectedRevision: bound.revision,
    attentionItemId: attention.id,
    status: 'resolved',
    resolution: JSON.stringify(decision),
    remediation: null,
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof },
  })
  assert.equal(delivered.status, 'delivered')
  const resolved = delivered.attentionItems.find(item => item.id === attention.id)
  assert.notEqual(resolved, undefined)
  const current = assertStrongFlowGitHubPublicationCurrent(delivered, candidate, resolved)
  assert.equal(current.context.providerIdempotencyKey, context.providerIdempotencyKey)
  assert.equal(current.approvedBy, 'approver-1')
})

test('browser approval request carries the exact visible publication identities', () => {
  const client = loadStrongFlowClient()
  const delivery = readyDelivery()
  const candidate = frozenCandidate()
  const reviewStageRunId = `${deliveryId}:stage:delivery-review:browser`
  const attention = createStrongFlowGitHubPublicationAttention({
    delivery,
    candidate,
    attentionItemId: `${deliveryId}:attention:publish:browser`,
    reviewStageRunId,
    assignedTo: 'approver-1',
    preparedAtMillis: now + 40,
  })
  const reviewing = parseDelivery({
    ...delivery,
    revision: 2,
    status: 'needs-attention',
    stageRuns: [...delivery.stageRuns, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: reviewStageRunId,
      deliveryId,
      deliveryTaskId: null,
      stage: 'delivery-review',
      actorType: 'human',
      role: 'approver',
      status: 'waiting',
      attempt: 1,
      startedAtMillis: now + 41,
      finishedAtMillis: null,
    }],
    sessionBindings: [...delivery.sessionBindings, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `${deliveryId}:binding:delivery-review:browser`,
      deliveryId,
      stageRunId: reviewStageRunId,
      dshSessionId: 'dsh-github-browser-review',
      codexSessionId: null,
      boundAtMillis: now + 42,
    }],
    attentionItems: [attention],
    updatedAtMillis: now + 42,
  })
  const request = client.createGitHubPublicationDecisionRequest({
    delivery: reviewing,
    attentionItemId: attention.id,
    comments: 'Reviewed the exact visible publication set.',
    requestId: 'ui:github-publication:approve:42',
  })
  const decision = JSON.parse(request.payload.resolution)
  const context = parseStrongFlowGitHubPublicationContextText(attention.context)
  assert.equal(request.operation, 'resolveAttention')
  assert.equal(request.payload.expectedRevision, reviewing.revision)
  assert.equal(decision.candidateRef, context.candidateRef)
  assert.equal(decision.deliveryVerdictId, context.deliveryVerdictId)
  assert.equal(decision.providerIdempotencyKey, context.providerIdempotencyKey)
  assert.equal(decision.publicationSetSha256, context.publicationSetSha256)
  const markup = renderToStaticMarkup(React.createElement(
    client.StrongFlowDeliveryProjection,
    {
      delivery: reviewing,
      diagramExecution: null,
      sessionId: 'dsh-github-browser-review',
      refreshing: false,
      onRefresh() {},
      onClose() {},
      openSession() {},
      async onPlanReviewDecision() {},
    },
  ))
  assert.match(markup, /GitHub 交付审核/u)
  assert.match(markup, /example\/widget#42/u)
  assert.match(markup, /批准当前发布集合/u)
  assert.equal(markup.includes(attention.context), false)
})

test('publication guard rejects stale candidate, verdict, destination, and approval', () => {
  const delivery = readyDelivery()
  const candidate = frozenCandidate()
  const reviewStageRunId = `${deliveryId}:stage:delivery-review:stale`
  const attention = createStrongFlowGitHubPublicationAttention({
    delivery,
    candidate,
    attentionItemId: `${deliveryId}:attention:publish:stale`,
    reviewStageRunId,
    assignedTo: 'approver-1',
    preparedAtMillis: now + 40,
  })
  const context = parseStrongFlowGitHubPublicationContextText(attention.context)
  const decision = createStrongFlowGitHubPublicationDecision({
    context,
    comments: 'Approved exact current publication facts.',
  })
  const reviewRun = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: reviewStageRunId,
    deliveryId,
    deliveryTaskId: null,
    stage: 'delivery-review',
    actorType: 'human',
    role: 'approver',
    status: 'succeeded',
    attempt: 1,
    startedAtMillis: now + 41,
    finishedAtMillis: now + 43,
  }
  const resolved = {
    ...attention,
    status: 'resolved',
    resolution: JSON.stringify(decision),
    resolvedBy: 'approver-1',
    resolvedAtMillis: now + 43,
  }
  const delivered = parseDelivery({
    ...delivery,
    revision: 4,
    status: 'delivered',
    stageRuns: [...delivery.stageRuns, reviewRun],
    sessionBindings: [...delivery.sessionBindings, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `${deliveryId}:binding:delivery-review:stale`,
      deliveryId,
      stageRunId: reviewStageRunId,
      dshSessionId: 'dsh-github-stale-review',
      codexSessionId: null,
      boundAtMillis: now + 42,
    }],
    attentionItems: [resolved],
    updatedAtMillis: now + 43,
  })
  assert.equal(
    assertStrongFlowGitHubPublicationCurrent(delivered, candidate, delivered.attentionItems[0])
      .candidate.candidateRef,
    candidate.candidateRef,
  )

  assert.throws(() => assertStrongFlowGitHubPublicationCurrent(
    delivered,
    { ...candidate, candidateTreeId: '9'.repeat(40) },
    delivered.attentionItems[0],
  ), expectPublicationError('STALE_PUBLICATION_SET'))

  const staleVerdict = parseDelivery({
    ...delivered,
    verdict: { ...delivered.verdict, id: `${deliveryId}:verdict:changed` },
  })
  assert.throws(() => assertStrongFlowGitHubPublicationCurrent(
    staleVerdict,
    candidate,
    staleVerdict.attentionItems[0],
  ), expectPublicationError('STALE_PUBLICATION_SET'))

  const staleDestination = parseDelivery({
    ...delivered,
    spec: {
      ...delivered.spec,
      publicationTarget: {
        ...delivered.spec.publicationTarget,
        baseBranch: 'release',
      },
    },
  })
  assert.throws(() => assertStrongFlowGitHubPublicationCurrent(
    staleDestination,
    candidate,
    staleDestination.attentionItems[0],
  ), expectPublicationError('STALE_PUBLICATION_SET'))

  const staleApproval = parseDelivery({
    ...delivered,
    attentionItems: [{
      ...delivered.attentionItems[0],
      resolution: JSON.stringify({ ...decision, publicationSetSha256: '0'.repeat(64) }),
    }],
  })
  assert.throws(() => assertStrongFlowGitHubPublicationCurrent(
    staleApproval,
    candidate,
    staleApproval.attentionItems[0],
  ), expectPublicationError('STALE_PUBLICATION_SET'))

  assert.throws(() => assertStrongFlowGitHubPublicationCurrent(
    delivered,
    candidate,
    { ...delivered.attentionItems[0], resolvedBy: 'different-approver' },
  ), expectPublicationError('STALE_PUBLICATION_SET'))
})
