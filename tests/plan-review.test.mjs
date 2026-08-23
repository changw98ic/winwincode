import assert from 'node:assert/strict'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseAttentionItem,
  parseDelivery,
  parseStrongFlowPlanReviewContextText,
} from '../packages/contracts/dist/index.js'
import {
  StrongFlowPlanReviewError,
  createStrongFlowPlanReviewAttention,
  createStrongFlowPlanReviewDecision,
  validateStrongFlowPlanReviewAttention,
  validateStrongFlowPlanReviewDecision,
} from '../packages/strongflow/dist/index.js'

const now = 2_400_000_000_000

function planningDelivery() {
  const deliveryId = 'delivery-plan-review-contract'
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 4,
    status: 'planning',
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'delivery-plan-review-contract:spec:2',
      deliveryId,
      revision: 2,
      title: '审核方案后再执行',
      goal: '把方案和交付定义绑定到同一个人工决定。',
      scope: ['方案审核'],
      outOfScope: ['执行调度'],
      constraints: ['Codex Core remains the execution authority'],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'delivery-plan-review-contract:criterion:1',
        description: '当前方案必须经过人工批准。',
        verificationMethod: '检查冻结审核集合和决定记录。',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/plan-review',
      },
      baseRevision: '0123456789012345678901234567890123456789',
      maxReworkAttempts: 2,
      createdAtMillis: now,
    },
    tasks: [],
    stageRuns: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'stage-plan-review-planning',
      deliveryId,
      deliveryTaskId: null,
      stage: 'planning',
      actorType: 'codex',
      role: 'planner',
      status: 'running',
      attempt: 1,
      startedAtMillis: now + 1,
      finishedAtMillis: null,
    }],
    sessionBindings: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'binding-plan-review-planning',
      deliveryId,
      stageRunId: 'stage-plan-review-planning',
      dshSessionId: 'dsh-plan-review-planning',
      codexSessionId: 'codex-plan-review-planning',
      boundAtMillis: now + 2,
    }],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 2,
  })
}

function solution() {
  return {
    id: 'solution-plan-review-contract',
    summary: '在现有 StrongFlow 服务上增加严格方案审核协议。',
    approach: ['冻结当前 DeliverySpec。', '生成两张默认图。', '绑定人工决定。'],
    components: [{
      id: 'component-plan-review-protocol',
      label: '方案审核协议',
      responsibility: '绑定当前方案、图、风险和人工决定。',
      kind: 'component',
      trustBoundary: 'Human review boundary',
      unresolved: false,
      repositoryPathPrefixes: ['packages/strongflow'],
    }],
    connections: [{
      id: 'connection-plan-review-protocol',
      from: 'platform:strongflow',
      to: 'component-plan-review-protocol',
      label: '保存冻结审核集合',
    }],
  }
}

function attention() {
  return createStrongFlowPlanReviewAttention({
    delivery: planningDelivery(),
    attentionItemId: 'attention-plan-review-contract',
    reviewStageRunId: 'stage-plan-review-human',
    assignedTo: 'reviewer',
    solution: solution(),
    risks: ['错误的版本绑定会批准过期方案。'],
    unresolvedItems: ['确认审核 Session 的负责人。'],
    preparedAtMillis: now + 3,
  })
}

function reviewRun() {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'stage-plan-review-human',
    deliveryId: 'delivery-plan-review-contract',
    deliveryTaskId: null,
    stage: 'plan-review',
    actorType: 'human',
    role: 'reviewer',
    status: 'waiting',
    attempt: 1,
    startedAtMillis: now + 4,
    finishedAtMillis: null,
  }
}

function expectReviewError(code) {
  return error => {
    assert.ok(error instanceof StrongFlowPlanReviewError)
    assert.equal(error.code, code)
    return true
  }
}

test('plan review freezes one deterministic solution, architecture, process, and digest', () => {
  const first = attention()
  const second = attention()
  assert.equal(first.context, second.context)
  assert.deepEqual(Array.from(first.options, option => option.id), [
    'approve',
    'request_changes',
    'reject',
  ])

  const context = parseStrongFlowPlanReviewContextText(first.context)
  assert.equal(context.deliverySpecId, 'delivery-plan-review-contract:spec:2')
  assert.equal(context.deliverySpecRevision, 2)
  assert.equal(context.reviewSetSha256.length, 64)
  assert.equal(context.architectureDiagram.kind, 'system-architecture')
  assert.equal(context.processDiagram.kind, 'process-flow')
  assert.deepEqual(
    Array.from(context.architectureDiagram.nodes, node => node.id).slice(0, 4),
    ['platform:dsh', 'platform:strongflow', 'platform:codex-core', 'platform:repository'],
  )
  assert.ok(context.architectureDiagram.nodes.some(node => (
    node.id === 'component-plan-review-protocol'
  )))
  assert.deepEqual(
    new Set(context.processDiagram.edges.map(edge => edge.label)),
    new Set([
      '需求已确认',
      '冻结审核集合',
      '批准',
      '要求修改',
      '重新设计',
      '拒绝',
      '更新定义',
      '候选完成',
      '失败',
      '再次验证',
      '证据不足 / 环境错误',
      '继续验证',
      '全部通过',
      '批准交付',
      '标注返工',
    ]),
  )

  const validated = validateStrongFlowPlanReviewAttention(
    planningDelivery(),
    planningDelivery().stageRuns[0],
    reviewRun(),
    first,
    now + 4,
  )
  assert.deepEqual(validated, context)
})

test('plan review rejects a diagram changed after the review digest was prepared', () => {
  const current = attention()
  const parsed = JSON.parse(current.context)
  parsed.architectureDiagram.nodes[0].description = 'tampered description'
  const tampered = parseAttentionItem({
    ...current,
    context: JSON.stringify(parsed),
  })
  assert.throws(() => validateStrongFlowPlanReviewAttention(
    planningDelivery(),
    planningDelivery().stageRuns[0],
    reviewRun(),
    tampered,
    now + 4,
  ), expectReviewError('STALE_REVIEW_SET'))
})

test('plan review maps exact human actions to execution, replanning, or clarification', () => {
  const delivery = planningDelivery()
  const item = attention()
  const context = parseStrongFlowPlanReviewContextText(item.context)
  const cases = [{
    action: 'approve',
    status: 'resolved',
    comments: '批准当前审核集合。',
    requestedChanges: [],
    nextStatus: 'executing',
  }, {
    action: 'request_changes',
    status: 'dismissed',
    comments: '按逐项意见修改。',
    requestedChanges: ['补充失败路径。'],
    nextStatus: 'planning',
  }, {
    action: 'reject',
    status: 'dismissed',
    comments: '交付目标仍需澄清。',
    requestedChanges: [],
    nextStatus: 'clarifying',
  }]

  for (const fixture of cases) {
    const decision = createStrongFlowPlanReviewDecision({
      context,
      action: fixture.action,
      comments: fixture.comments,
      requestedChanges: fixture.requestedChanges,
    })
    const result = validateStrongFlowPlanReviewDecision(
      delivery,
      item,
      fixture.status,
      JSON.stringify(decision),
    )
    assert.equal(result.nextStatus, fixture.nextStatus)
    assert.equal(result.decision.reviewSetSha256, context.reviewSetSha256)
  }

  assert.throws(() => validateStrongFlowPlanReviewDecision(
    delivery,
    item,
    'resolved',
    'approve this plan',
  ), expectReviewError('INVALID_REVIEW_DECISION'))
})
