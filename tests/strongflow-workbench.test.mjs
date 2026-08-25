import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import test from 'node:test'
import vm from 'node:vm'
import { fileURLToPath } from 'node:url'

import {
  DELIVERY_SCHEMA_VERSION,
  STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
  parseDelivery,
  parseStrongFlowPlanReviewContextText,
  parseStrongFlowPlanReviewDecisionText,
} from '../packages/contracts/dist/index.js'
import { createStrongFlowPlanReviewAttention } from '../packages/strongflow/dist/index.js'
import * as React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'

const root = fileURLToPath(new URL('../', import.meta.url))

function loadStrongFlowClient() {
  let registration
  vm.runInNewContext(
    readFileSync(join(root, 'packages', 'strongflow', 'dist', 'client.js'), 'utf8'),
    {
      Symbol,
      structuredClone,
      window: {
        __ModuleLoader__: {
          load(value) {
            registration = value
          },
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

function deliveryFixture() {
  const now = 2_300_000_000_000
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'dlv_5SCBC9KW8YEWPQ8BS08VMA5CVX',
    revision: 4,
    status: 'executing',
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-workbench:spec:1',
      deliveryId: 'dlv_5SCBC9KW8YEWPQ8BS08VMA5CVX',
      revision: 1,
      title: '实现可审核的邀请流程',
      goal: '把用户目标、交付阶段和验收依据放在同一个只读视图中。',
      scope: ['邀请 API', '邀请页面'],
      outOfScope: ['通用项目管理'],
      constraints: ['Codex Core remains the execution authority'],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'delivery-workbench:criterion:1',
        description: '邀请链接只能使用一次。',
        verificationMethod: '运行并发集成测试。',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/invitations',
      },
      baseRevision: '0123456789012345678901234567890123456789',
      maxReworkAttempts: 2,
      createdAtMillis: now,
    },
    tasks: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-workbench:task:api',
      deliveryId: 'dlv_5SCBC9KW8YEWPQ8BS08VMA5CVX',
      title: '邀请 API',
      goal: '交付可以独立验收的邀请接口。',
      acceptanceCriterionIds: ['delivery-workbench:criterion:1'],
      blockedByTaskIds: [],
      owner: 'executor',
      status: 'active',
    }],
    stageRuns: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-workbench:stage:execute',
      deliveryId: 'dlv_5SCBC9KW8YEWPQ8BS08VMA5CVX',
      deliveryTaskId: 'delivery-workbench:task:api',
      stage: 'executing',
      actorType: 'codex',
      role: 'executor',
      status: 'running',
      attempt: 1,
      startedAtMillis: now + 10,
      finishedAtMillis: null,
    }],
    sessionBindings: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-workbench:binding:execute',
      deliveryId: 'dlv_5SCBC9KW8YEWPQ8BS08VMA5CVX',
      stageRunId: 'delivery-workbench:stage:execute',
      dshSessionId: 'dsh-workbench-executor',
      codexSessionId: 'codex-workbench-executor',
      boundAtMillis: now + 11,
    }],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 11,
  })
}

function planReviewDeliveryFixture() {
  const base = deliveryFixture()
  const now = base.createdAtMillis
  const planningRun = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'delivery-workbench:stage:planning',
    deliveryId: base.id,
    deliveryTaskId: null,
    stage: 'planning',
    actorType: 'codex',
    role: 'planner',
    status: 'running',
    attempt: 1,
    startedAtMillis: now + 1,
    finishedAtMillis: null,
  }
  const planningBinding = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'delivery-workbench:binding:planning',
    deliveryId: base.id,
    stageRunId: planningRun.id,
    dshSessionId: 'dsh-workbench-planning',
    codexSessionId: 'codex-workbench-planning',
    boundAtMillis: now + 2,
  }
  const planning = parseDelivery({
    ...base,
    revision: 4,
    status: 'planning',
    tasks: [],
    stageRuns: [planningRun],
    sessionBindings: [planningBinding],
    updatedAtMillis: now + 2,
  })
  const attention = createStrongFlowPlanReviewAttention({
    delivery: planning,
    attentionItemId: 'delivery-workbench:attention:plan-review',
    reviewStageRunId: 'delivery-workbench:stage:plan-review',
    assignedTo: 'reviewer',
    solution: {
      id: 'delivery-workbench:solution:1',
      summary: '在现有邀请模块中增加一次性令牌和并发消费保护。',
      approach: ['增加邀请令牌存储。', '实现原子消费 API。', '补充并发验证。'],
      components: [{
        id: 'delivery-workbench:component:invitation',
        label: '邀请模块',
        responsibility: '签发并原子消费一次性邀请令牌。',
        kind: 'component',
        trustBoundary: 'Application boundary',
        unresolved: false,
        repositoryPathPrefixes: ['src/invitations'],
      }],
      connections: [{
        id: 'delivery-workbench:connection:invitation',
        from: 'platform:strongflow',
        to: 'delivery-workbench:component:invitation',
        label: '传递已批准交付定义',
      }],
    },
    risks: ['并发请求可能重复消费同一个邀请令牌。'],
    unresolvedItems: ['确认邀请链接的默认有效期。'],
    preparedAtMillis: now + 3,
  })
  return parseDelivery({
    ...planning,
    revision: 6,
    status: 'needs-attention',
    stageRuns: [{
      ...planningRun,
      status: 'succeeded',
      finishedAtMillis: now + 4,
    }, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-workbench:stage:plan-review',
      deliveryId: base.id,
      deliveryTaskId: null,
      stage: 'plan-review',
      actorType: 'human',
      role: 'reviewer',
      status: 'waiting',
      attempt: 1,
      startedAtMillis: now + 4,
      finishedAtMillis: null,
    }],
    sessionBindings: [planningBinding, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-workbench:binding:plan-review',
      deliveryId: base.id,
      stageRunId: 'delivery-workbench:stage:plan-review',
      dshSessionId: 'dsh-workbench-review',
      codexSessionId: null,
      boundAtMillis: now + 5,
    }],
    attentionItems: [attention],
    updatedAtMillis: now + 5,
  })
}

function diagramExecutionFixture(delivery, state) {
  const context = parseStrongFlowPlanReviewContextText(delivery.attentionItems[0].context)
  const fileId = `diagram-file:sha256:${'1'.repeat(64)}`
  const hunkId = `diagram-hunk:sha256:${'2'.repeat(64)}`
  const componentId = 'delivery-workbench:component:invitation'
  const node = entry => ({
    nodeId: entry.id,
    state: entry.id === componentId || entry.id === 'platform:repository'
      || entry.id === 'process:executing'
      ? state === 'executing' ? 'affected-live' : 'affected-finished'
      : 'normal',
    affectedFileCount: entry.id === componentId || entry.id === 'platform:repository'
      || entry.id === 'process:executing'
      ? 1
      : 0,
    fileIds: state === 'execution-finished'
      && (entry.id === componentId || entry.id === 'platform:repository'
        || entry.id === 'process:executing')
      ? [fileId]
      : [],
  })
  const candidate = {
    schemaVersion: 1,
    candidateRef: `git-candidate:sha256:${'3'.repeat(64)}`,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    deliverySpecRevision: delivery.spec.revision,
    repositoryKind: delivery.spec.repository.kind,
    repositoryLocator: delivery.spec.repository.locator,
    baseRevision: delivery.spec.baseRevision,
    producerStageRunId: 'delivery-workbench:stage:execute',
    producerSessionBindingId: 'delivery-workbench:binding:execute',
    baseCommitId: delivery.spec.baseRevision,
    baseTreeId: '4'.repeat(40),
    candidateCommitId: '5'.repeat(40),
    candidateTreeId: '6'.repeat(40),
    diffSha256: '7'.repeat(64),
    changedPaths: [{
      path: 'src/invitations/api.ts',
      state: 'present',
      objectId: '8'.repeat(40),
    }],
  }
  return {
    schemaVersion: 1,
    protocol: 'winwincode.diagram-execution-projection.v1',
    deliveryId: delivery.id,
    deliveryRevision: delivery.revision,
    reviewSetSha256: context.reviewSetSha256,
    state,
    architecture: {
      diagramId: context.architectureDiagram.id,
      kind: 'system-architecture',
      nodes: context.architectureDiagram.nodes.map(entry => node(entry)),
    },
    process: {
      diagramId: context.processDiagram.id,
      kind: 'process-flow',
      nodes: context.processDiagram.nodes.map(entry => node(entry)),
    },
    affectedFileCount: 1,
    details: state === 'execution-finished'
      ? {
        candidate,
        diffSha256: candidate.diffSha256,
        files: [{
          id: fileId,
          path: 'src/invitations/api.ts',
          previousPath: null,
          state: 'present',
          additions: 1,
          deletions: 1,
          hunkIds: [hunkId],
          nodeIds: [componentId, 'platform:repository', 'process:executing'],
        }],
        hunks: [{
          id: hunkId,
          fileId,
          sha256: '2'.repeat(64),
          header: '@@ -1 +1 @@',
          content: '@@ -1 +1 @@\n-unsafe\n+<script>alert(1)</script>\n',
          additions: 1,
          deletions: 1,
        }],
        additions: 1,
        deletions: 1,
        provenance: {
          stageRunId: candidate.producerStageRunId,
          sessionBindingId: candidate.producerSessionBindingId,
          deliveryTaskId: 'delivery-workbench:task:api',
          stage: 'executing',
          role: 'executor',
          attempt: 1,
          dshSessionId: 'dsh-workbench-executor',
          codexSessionId: 'codex-workbench-executor',
          startedAtMillis: delivery.createdAtMillis + 10,
          finishedAtMillis: delivery.createdAtMillis + 20,
          agents: [],
          activities: [{
            callId: 'call-test',
            type: 'test',
            command: 'pnpm test',
            status: 'completed',
            outcome: 'succeeded',
            exitCode: 0,
            occurredAtMillis: delivery.createdAtMillis + 19,
          }],
          evidenceRefIds: [`evidence:${'9'.repeat(64)}`],
        },
      }
      : null,
    updatedAtMillis: delivery.updatedAtMillis,
  }
}

function runtimeExecutionFixture(delivery) {
  const event = (sequence, kind) => ({
    eventId: `dsh-workbench-executor@${String(sequence)}`,
    sourceRef: `runtime_event:dsh-workbench-executor@${String(sequence)}`,
    sequence: String(sequence),
    kind,
  })
  return {
    schemaVersion: 1,
    protocol: 'winwincode.runtime-execution-projection.v1',
    deliveryId: delivery.id,
    deliveryRevision: delivery.revision,
    sessions: [{
      stageRunId: 'delivery-workbench:stage:execute',
      sessionBindingId: 'delivery-workbench:binding:execute',
      dshSessionId: 'dsh-workbench-executor',
      codexSessionId: 'codex-workbench-executor',
      asOfSequence: '9',
      plan: {
        itemId: 'plan-workbench-executor',
        explanation: '按已批准范围完成邀请流程。',
        items: [
          { step: '实现原子消费 API', status: 'completed' },
          { step: '运行并发集成测试', status: 'in_progress' },
          { step: '整理验收依据', status: 'pending' },
        ],
        text: null,
        complete: false,
        latestEvent: event(2, 'plan.updated'),
      },
      agents: [{
        threadId: 'codex-workbench-executor',
        path: '/root',
        parentThreadId: null,
        nickname: null,
        role: 'executor',
        status: 'running',
        latestEvent: event(1, 'turn.started'),
      }, {
        threadId: 'codex-workbench-reviewer',
        path: '/root/reviewer',
        parentThreadId: 'codex-workbench-executor',
        nickname: 'reviewer',
        role: 'review',
        status: 'waiting',
        latestEvent: event(3, 'subagent.started'),
      }],
      agentEdges: [{
        parentThreadId: 'codex-workbench-executor',
        childThreadId: 'codex-workbench-reviewer',
      }],
      activities: [{
        callId: 'call-workbench-tests',
        activityType: 'test',
        command: 'pnpm test:integration',
        status: 'completed',
        outcome: 'succeeded',
        exitCode: 0,
        latestEvent: event(4, 'tool.completed'),
      }],
      interactions: [{
        id: 'input-workbench-expiry',
        interactionType: 'user-input',
        blocking: true,
        status: 'pending',
        questions: [{
          id: 'question-workbench-expiry',
          header: '邀请有效期',
          question: '默认有效期使用 24 小时吗？',
          isSecret: false,
        }],
        requestedEvent: event(5, 'input.requested'),
        resolvedEvent: null,
      }],
      failures: [{
        message: '第一次测试进程退出。',
        code: 'process-exited',
        event: event(6, 'failure'),
      }],
      recovery: {
        state: 'recovered',
        failureCount: 1,
        recoveryCount: 1,
        lastFailureEvent: event(6, 'failure'),
        latestRecoveryEvent: event(7, 'turn.completed'),
      },
      diffSummary: {
        changedFileCount: 2,
        additions: 18,
        deletions: 4,
        detailsVisible: false,
        event: event(8, 'diff.updated'),
      },
      usage: {
        totals: { input_tokens: 120, output_tokens: 80, total_tokens: 200 },
        event: event(9, 'usage.updated'),
      },
      evidence: [{
        type: 'test',
        outcome: 'succeeded',
        sourceRef: 'runtime_event:dsh-workbench-executor@4',
        eventId: 'dsh-workbench-executor@4',
      }],
    }],
  }
}

test('StrongFlow create form produces one canonical DeliverySpec without promoting plan steps', () => {
  const client = loadStrongFlowClient()
  const request = client.createDeliveryRequestFromDraft({
    deliveryId: 'dlv_7N3Y3ASK5SWBB9TT46PCV6M9E4',
    title: '创建交付',
    goal: '固定目标后再进入方案和执行。',
    scope: '交付定义\n人工审核',
    outOfScope: '通用任务系统',
    constraints: 'Codex Core remains the execution authority',
    criteria: '定义可审核 | 读取 DeliverySpec\n执行前必须人工批准',
    repositoryKind: 'local-git',
    repositoryLocator: '/workspace/repository',
    baseRevision: 'HEAD',
    maxReworkAttempts: '2',
    githubIssue: '',
    githubBaseBranch: 'main',
    githubHeadRepository: '',
    githubHeadBranch: '',
  }, 'ui:create:delivery-created-in-workbench:fixture', 2_300_000_000_000)

  assert.equal(request.operation, 'createDelivery')
  assert.equal(request.payload.spec.id, 'dlv_7N3Y3ASK5SWBB9TT46PCV6M9E4:spec:1')
  assert.deepEqual(Array.from(request.payload.spec.scope), ['交付定义', '人工审核'])
  assert.deepEqual(Array.from(request.payload.spec.acceptanceCriteria, criterion => ({
    id: criterion.id,
    method: criterion.verificationMethod,
  })), [
    {
      id: 'dlv_7N3Y3ASK5SWBB9TT46PCV6M9E4:criterion:1',
      method: '读取 DeliverySpec',
    },
    {
      id: 'dlv_7N3Y3ASK5SWBB9TT46PCV6M9E4:criterion:2',
      method: null,
    },
  ])
  assert.equal(request.payload.tasks.length, 0)
})

test('StrongFlow confirms one exact draft as a new approved DeliverySpec revision', () => {
  const client = loadStrongFlowClient()
  const base = deliveryFixture()
  const draft = parseDelivery({
    ...base,
    revision: 1,
    status: 'draft',
    tasks: [],
    stageRuns: [],
    sessionBindings: [],
    updatedAtMillis: base.createdAtMillis,
  })
  const request = client.createRequirementsApprovalRequest(
    draft,
    'ui:approve-requirements:fixture',
  )
  assert.equal(request.operation, 'updateDeliverySpec')
  assert.equal(request.payload.expectedRevision, draft.revision)
  assert.equal(request.payload.spec.revision, draft.spec.revision + 1)
  assert.notEqual(request.payload.spec.id, draft.spec.id)
  assert.deepEqual(
    structuredClone(request.payload.spec.acceptanceCriteria),
    structuredClone(draft.spec.acceptanceCriteria),
  )
  assert.equal(request.payload.spec.goal, draft.spec.goal)
})

test('StrongFlow projection renders Delivery facts and links activity to its DSH Session', () => {
  const client = loadStrongFlowClient()
  const delivery = deliveryFixture()
  const markup = renderToStaticMarkup(React.createElement(
    client.StrongFlowDeliveryProjection,
    {
      delivery,
      runtimeExecution: runtimeExecutionFixture(delivery),
      sessionId: 'dsh-workbench-executor',
      refreshing: false,
      onRefresh() {},
      onClose() {},
      openSession() {},
      async onPlanReviewDecision() {},
    },
  ))

  assert.match(markup, /实现可审核的邀请流程/u)
  assert.match(markup, /邀请链接只能使用一次/u)
  assert.match(markup, /邀请 API/u)
  assert.match(markup, /打开 Chat Session/u)
  assert.match(markup, /dsh-workbench-executor/u)
  assert.match(markup, /Codex · codex-workbench-executor/u)
  assert.match(markup, /Codex 执行视图/u)
  assert.match(markup, /实现原子消费 API/u)
  assert.match(markup, /codex-workbench-reviewer/u)
  assert.match(markup, /父节点：codex-workbench-executor/u)
  assert.match(markup, /pnpm test:integration/u)
  assert.match(markup, /默认有效期使用 24 小时吗/u)
  assert.match(markup, /第一次测试进程退出/u)
  assert.match(markup, /已恢复/u)
  assert.match(markup, /2 个文件 · \+18 \/ -4/u)
  assert.match(markup, /total_tokens：200/u)
  assert.match(markup, /推进下一阶段/u)
  assert.doesNotMatch(markup, /src\/invitations\/api\.ts|@@ -1 \+1 @@/u)
  assert.doesNotMatch(markup, /task scheduler|team roster|mailbox/iu)
})

test('StrongFlow local delivery review is writable only in its bound human Session', () => {
  const client = loadStrongFlowClient()
  const base = deliveryFixture()
  const candidateRef = `git-candidate:sha256:${'a'.repeat(64)}`
  const reviewRunId = 'delivery-workbench:stage:delivery-review'
  const attention = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'delivery-workbench:attention:delivery-review',
    deliveryId: base.id,
    deliverySpecId: base.spec.id,
    stageRunId: reviewRunId,
    type: 'delivery_approval',
    title: '审核当前候选和验收结论',
    context: JSON.stringify({
      candidateRef,
      deliveryVerdictId: 'delivery-workbench:verdict:1',
      message: '当前冻结候选已经通过独立 reviewer 和 verifier。',
    }),
    options: [],
    assignedTo: 'dsh-workbench-delivery-review',
    blocking: true,
    status: 'open',
    resolution: null,
    resolvedBy: null,
    createdAtMillis: base.updatedAtMillis + 1,
    resolvedAtMillis: null,
  }
  const delivery = {
    ...base,
    revision: base.revision + 1,
    status: 'needs-attention',
    stageRuns: [{
      ...base.stageRuns[0],
      status: 'succeeded',
      finishedAtMillis: base.updatedAtMillis + 1,
    }, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: reviewRunId,
      deliveryId: base.id,
      deliveryTaskId: base.tasks[0].id,
      stage: 'delivery-review',
      actorType: 'human',
      role: 'approver',
      status: 'waiting',
      attempt: 1,
      startedAtMillis: base.updatedAtMillis + 2,
      finishedAtMillis: null,
    }],
    sessionBindings: [...base.sessionBindings, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-workbench:binding:delivery-review',
      deliveryId: base.id,
      stageRunId: reviewRunId,
      dshSessionId: 'dsh-workbench-delivery-review',
      codexSessionId: null,
      boundAtMillis: base.updatedAtMillis + 2,
    }],
    attentionItems: [attention],
    verdict: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-workbench:verdict:1',
      deliveryId: base.id,
      deliverySpecId: base.spec.id,
      deliverySpecRevision: base.spec.revision,
      candidateRef,
      status: 'pass',
      criteria: [],
      unresolvedFindings: [],
      producedAtMillis: base.updatedAtMillis + 1,
    },
  }
  const ownerMarkup = renderToStaticMarkup(React.createElement(
    client.StrongFlowDeliveryProjection,
    {
      delivery,
      sessionId: 'dsh-workbench-delivery-review',
      refreshing: false,
      onRefresh() {},
      onClose() {},
      openSession() {},
      async onPlanReviewDecision() {},
    },
  ))
  assert.match(ownerMarkup, /本地交付审核/u)
  assert.match(ownerMarkup, /批准当前本地候选/u)

  const observerMarkup = renderToStaticMarkup(React.createElement(
    client.StrongFlowDeliveryProjection,
    {
      delivery,
      sessionId: 'dsh-workbench-observer',
      refreshing: false,
      onRefresh() {},
      onClose() {},
      openSession() {},
      async onPlanReviewDecision() {},
    },
  ))
  assert.match(observerMarkup, /当前页面为只读交付审核视图/u)
  assert.doesNotMatch(observerMarkup, /批准当前本地候选/u)

  const request = client.createLocalDeliveryApprovalRequest({
    delivery,
    attentionItemId: attention.id,
    comments: '批准当前冻结候选。',
    requestId: 'ui:local-delivery-approval:fixture',
  })
  assert.equal(request.operation, 'resolveAttention')
  assert.equal(request.payload.expectedRevision, delivery.revision)
  assert.equal(request.payload.status, 'resolved')
  assert.equal(request.payload.authentication.proof, 'dsh-reference-only')
})

test('StrongFlow renders the exact DeliverySpec before a separate solution review set', () => {
  const client = loadStrongFlowClient()
  const delivery = planReviewDeliveryFixture()
  const markup = renderToStaticMarkup(React.createElement(
    client.StrongFlowDeliveryProjection,
    {
      delivery,
      sessionId: 'dsh-workbench-review',
      refreshing: false,
      onRefresh() {},
      onClose() {},
      openSession() {},
      async onPlanReviewDecision() {},
    },
  ))

  assert.ok(markup.indexOf('DeliverySpec') < markup.indexOf('Acceptance Criteria'))
  assert.ok(markup.indexOf('Acceptance Criteria') < markup.indexOf('Solution Review Set'))
  assert.match(markup, /交付目标/u)
  assert.match(markup, /邀请 API/u)
  assert.match(markup, /通用项目管理/u)
  assert.match(markup, /Codex Core remains the execution authority/u)
  assert.match(markup, /邀请链接只能使用一次/u)
  assert.match(markup, /在现有邀请模块中增加一次性令牌和并发消费保护/u)
  assert.match(markup, /系统架构图/u)
  assert.match(markup, /交付流程图/u)
  assert.match(markup, /并发请求可能重复消费同一个邀请令牌/u)
  assert.match(markup, /确认邀请链接的默认有效期/u)
  assert.equal((markup.match(/<figure /gu) ?? []).length, 2)
  assert.match(markup, /aria-labelledby="strongflow-diagram-/u)
  assert.match(markup, /data-diagram-kind="system-architecture"/u)
  assert.match(markup, /data-diagram-kind="process-flow"/u)
  assert.match(markup, /data-node-id="platform:codex-core"/u)
  assert.match(markup, /for="strongflow-plan-review-[^"]+-comments"/u)
  assert.match(markup, /for="strongflow-plan-review-[^"]+-changes"/u)
  assert.match(markup, /type="button">批准执行<\/button>/u)
  assert.match(markup, /aria-live="polite"/u)
  assert.doesNotMatch(markup, /winwincode\.plan-review-context\.v1/u)
})

test('StrongFlow binds a decision request to the visible revision, identities, and review digest', () => {
  const client = loadStrongFlowClient()
  const delivery = planReviewDeliveryFixture()
  const item = delivery.attentionItems[0]
  const context = parseStrongFlowPlanReviewContextText(item.context)
  const request = client.createPlanReviewDecisionRequest({
    delivery,
    attentionItemId: item.id,
    action: 'request_changes',
    comments: '请按逐项意见修改。',
    requestedChanges: ['补充令牌过期路径。'],
    requestId: 'ui:plan-review:fixture',
  })

  assert.equal(request.operation, 'resolveAttention')
  assert.equal(request.payload.expectedRevision, delivery.revision)
  assert.equal(request.payload.attentionItemId, item.id)
  assert.equal(request.payload.status, 'dismissed')
  assert.equal(request.payload.remediation, null)
  assert.equal(request.payload.authentication.scheme, 'local-session')
  assert.equal(request.payload.authentication.proof, 'dsh-reference-only')
  const decision = parseStrongFlowPlanReviewDecisionText(request.payload.resolution)
  assert.equal(decision.deliverySpecId, delivery.spec.id)
  assert.equal(decision.deliverySpecRevision, delivery.spec.revision)
  assert.equal(decision.reviewStageRunId, context.reviewStageRunId)
  assert.equal(decision.attentionItemId, context.attentionItemId)
  assert.equal(decision.reviewSetSha256, context.reviewSetSha256)
  assert.deepEqual(Array.from(decision.requestedChanges), ['补充令牌过期路径。'])
  assert.doesNotMatch(JSON.stringify(request), /ui-proof|localSessionProof/u)
})

test('StrongFlow keeps plan review read-only outside the bound DSH Session', () => {
  const client = loadStrongFlowClient()
  const markup = renderToStaticMarkup(React.createElement(
    client.StrongFlowDeliveryProjection,
    {
      delivery: planReviewDeliveryFixture(),
      sessionId: 'dsh-workbench-observer',
      refreshing: false,
      onRefresh() {},
      onClose() {},
      openSession() {},
      async onPlanReviewDecision() {},
    },
  ))

  assert.match(markup, /当前页面为只读审核视图/u)
  assert.match(markup, /打开审核 Session/u)
  assert.doesNotMatch(markup, /<textarea/u)
  assert.doesNotMatch(markup, /<button[^>]*>批准执行<\/button>/u)
})

test('StrongFlow overlays live changes without exposing concrete diff details', () => {
  const client = loadStrongFlowClient()
  const delivery = planReviewDeliveryFixture()
  const markup = renderToStaticMarkup(React.createElement(
    client.StrongFlowDeliveryProjection,
    {
      delivery,
      diagramExecution: diagramExecutionFixture(delivery, 'executing'),
      sessionId: 'dsh-workbench-review',
      refreshing: false,
      onRefresh() {},
      onClose() {},
      openSession() {},
      async onPlanReviewDecision() {},
    },
  ))

  assert.match(markup, /执行中状态/u)
  assert.match(markup, /data-execution-state="affected-live"/u)
  assert.match(markup, /执行中已发生变化/u)
  assert.match(markup, /data-node-id="platform:dsh"[^>]*data-execution-state="normal"/u)
  assert.doesNotMatch(markup, /src\/invitations\/api\.ts/u)
  assert.doesNotMatch(markup, /@@ -1 \+1 @@/u)
  assert.doesNotMatch(markup, /查看 1 个变更文件/u)
  assert.doesNotMatch(markup, /临时|暂存|provisional|temporary/iu)
})

test('StrongFlow finished nodes are selectable and remediation binds the exact visible hunk', async () => {
  const client = loadStrongFlowClient()
  const delivery = planReviewDeliveryFixture()
  const projection = diagramExecutionFixture(delivery, 'execution-finished')
  const markup = renderToStaticMarkup(React.createElement(
    client.StrongFlowDeliveryProjection,
    {
      delivery,
      diagramExecution: projection,
      sessionId: 'dsh-workbench-review',
      refreshing: false,
      onRefresh() {},
      onClose() {},
      openSession() {},
      async onPlanReviewDecision() {},
    },
  ))

  assert.match(markup, /执行结束状态/u)
  assert.match(markup, /data-execution-state="affected-finished"/u)
  assert.match(markup, /执行结束，等待审核/u)
  assert.match(markup, /type="button"[^>]*>查看 1 个变更文件<\/button>/u)
  assert.match(markup, /Frozen Candidate Diff/u)
  assert.match(markup, /选择黄色节点后查看它对应的文件和精确 hunk/u)

  const reviewAttention = {
    id: 'delivery-workbench:attention:delivery-review',
    status: 'open',
    stageRunId: 'delivery-workbench:stage:delivery-review',
  }
  const requestDelivery = {
    ...delivery,
    attentionItems: [...delivery.attentionItems, reviewAttention],
    stageRuns: [...delivery.stageRuns, {
      id: reviewAttention.stageRunId,
      stage: 'delivery-review',
    }],
  }
  const request = client.createDiagramRemediationRequest({
    delivery: requestDelivery,
    projection,
    attentionItemId: reviewAttention.id,
    annotations: [{
      diagramKind: 'system-architecture',
      nodeId: 'delivery-workbench:component:invitation',
      hunkId: projection.details.hunks[0].id,
      note: '保留原子消费，并修正这一处实现。',
    }],
    summary: '按图上标注返工。',
    requestId: 'ui:diagram-remediation:fixture',
  })
  assert.equal(request.operation, 'resolveAttention')
  assert.equal(request.payload.expectedRevision, delivery.revision)
  assert.equal(request.payload.status, 'dismissed')
  assert.equal(request.payload.remediation.schemaVersion, STRONGFLOW_DELIVERY_API_SCHEMA_VERSION)
  assert.equal(request.payload.remediation.candidate.candidateRef, projection.details.candidate.candidateRef)
  assert.equal(request.payload.remediation.annotations[0].diagramId, projection.architecture.diagramId)
  assert.equal(request.payload.remediation.annotations[0].filePath, 'src/invitations/api.ts')
  assert.equal(request.payload.remediation.annotations[0].hunkSha256, '2'.repeat(64))
  assert.deepEqual(
    Array.from(request.payload.remediation.annotations[0].evidenceRefIds),
    [`evidence:${'9'.repeat(64)}`],
  )
  assert.equal(request.payload.authentication.proof, 'dsh-reference-only')

  const tasklessProjection = {
    ...projection,
    details: {
      ...projection.details,
      provenance: {
        ...projection.details.provenance,
        deliveryTaskId: null,
      },
    },
  }
  const tasklessRequest = client.createDiagramRemediationRequest({
    delivery: { ...requestDelivery, tasks: [] },
    projection: tasklessProjection,
    attentionItemId: reviewAttention.id,
    annotations: [{
      diagramKind: 'system-architecture',
      nodeId: 'delivery-workbench:component:invitation',
      hunkId: projection.details.hunks[0].id,
      note: '仅修正这个已批准的交付级 hunk。',
    }],
    summary: '按图上标注执行交付级返工。',
    requestId: 'ui:diagram-remediation:taskless-fixture',
  })
  assert.equal(tasklessRequest.payload.remediation.deliveryTaskId, null)
  let advanceRequest
  const advanced = await client.advanceResolvedDiagramRemediation({
    request: tasklessRequest,
    delivery: {
      ...requestDelivery,
      revision: requestDelivery.revision + 1,
      status: 'reworking',
    },
    requestId: 'ui:advance-remediation:fixture',
    async invokeAdvance(value) {
      advanceRequest = value
      return {
        delivery: { ...requestDelivery, status: 'verifying' },
        outcome: {
          kind: 'candidate-ready-for-review',
          message: 'remediator 已完成。',
          stageRunId: 'stage-remediator-review',
          dshSessionId: null,
        },
      }
    },
  })
  assert.equal(advanceRequest.deliveryId, requestDelivery.id)
  assert.equal(advanceRequest.expectedRevision, requestDelivery.revision + 1)
  assert.equal(advanced.outcome.kind, 'candidate-ready-for-review')
})
