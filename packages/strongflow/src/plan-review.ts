import { createHash } from 'node:crypto'

import {
  ATTENTION_ITEM_TYPES,
  DELIVERY_SCHEMA_VERSION,
  STRONGFLOW_ARCHITECTURE_PLATFORM_NODE_IDS,
  STRONGFLOW_PLAN_REVIEW_CONTEXT_PROTOCOL,
  STRONGFLOW_PLAN_REVIEW_DECISION_PROTOCOL,
  STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION,
  AttentionItemId,
  StageRunId,
  parseAttentionItem,
  parseDelivery,
  parseStrongFlowPlanReviewContext,
  parseStrongFlowPlanReviewContextText,
  parseStrongFlowPlanReviewDecision,
  parseStrongFlowPlanReviewDecisionText,
  parseStrongFlowPlanReviewDiagram,
  parseStrongFlowPlanReviewSolution,
  serializeStrongFlowPlanReviewDecision,
  type AttentionItem,
  type AttentionItemStatus,
  type Delivery,
  type DeliveryStatus,
  type SessionBinding,
  type StageRun,
  type StrongFlowPlanReviewAction,
  type StrongFlowPlanReviewContext,
  type StrongFlowPlanReviewDecision,
  type StrongFlowPlanReviewDiagram,
  type StrongFlowPlanReviewSolution,
} from '@winwincode/contracts'

export type StrongFlowPlanReviewErrorCode =
  | 'INVALID_REVIEW_INPUT'
  | 'STALE_REVIEW_SET'
  | 'INVALID_REVIEW_DECISION'

export class StrongFlowPlanReviewError extends Error {
  readonly code: StrongFlowPlanReviewErrorCode

  constructor(code: StrongFlowPlanReviewErrorCode, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'StrongFlowPlanReviewError'
    this.code = code
  }
}

export interface CreateStrongFlowPlanReviewAttentionInput {
  readonly delivery: Delivery
  readonly attentionItemId: string
  readonly reviewStageRunId: string
  readonly assignedTo: string | null
  readonly solution: StrongFlowPlanReviewSolution
  readonly risks: readonly string[]
  readonly unresolvedItems: readonly string[]
  readonly preparedAtMillis: number
}

export interface CreateStrongFlowPlanReviewDecisionInput {
  readonly context: StrongFlowPlanReviewContext
  readonly action: StrongFlowPlanReviewAction
  readonly comments: string
  readonly requestedChanges: readonly string[]
}

export interface ValidatedStrongFlowPlanReviewDecision {
  readonly decision: StrongFlowPlanReviewDecision
  readonly storedResolution: string
  readonly nextStatus: DeliveryStatus
}

export interface CurrentStrongFlowPlanReview {
  readonly context: StrongFlowPlanReviewContext
  readonly decision: StrongFlowPlanReviewDecision
  readonly attention: AttentionItem
  readonly planningStageRun: StageRun
  readonly planningSessionBinding: SessionBinding
  readonly reviewStageRun: StageRun
  readonly reviewSessionBinding: SessionBinding
}

const ARCHITECTURE_PLATFORM_NODES = Object.freeze([
  Object.freeze({
    id: STRONGFLOW_ARCHITECTURE_PLATFORM_NODE_IDS[0],
    label: 'DSH',
    description: '承载聊天、Session、模型设置、审批交互和工作台展示。',
    kind: 'interaction' as const,
    trustBoundary: 'DSH product shell',
    unresolved: false,
  }),
  Object.freeze({
    id: STRONGFLOW_ARCHITECTURE_PLATFORM_NODE_IDS[1],
    label: 'WinWinCode',
    description: '保存交付目标、阶段、人工决定、验收依据和交付结论。',
    kind: 'delivery-control' as const,
    trustBoundary: 'Delivery control plane',
    unresolved: false,
  }),
  Object.freeze({
    id: STRONGFLOW_ARCHITECTURE_PLATFORM_NODE_IDS[2],
    label: 'Codex Core',
    description: '独占计划、Agent、工具、Shell、沙箱、权限和代码执行。',
    kind: 'execution' as const,
    trustBoundary: 'Execution authority',
    unresolved: false,
  }),
  Object.freeze({
    id: STRONGFLOW_ARCHITECTURE_PLATFORM_NODE_IDS[3],
    label: 'Repository',
    description: '保存基线代码和执行后形成的候选变更。',
    kind: 'repository' as const,
    trustBoundary: 'Candidate boundary',
    unresolved: false,
  }),
])

const ARCHITECTURE_PLATFORM_EDGES = Object.freeze([
  Object.freeze({
    id: 'platform-edge:dsh-to-strongflow',
    from: 'platform:dsh',
    to: 'platform:strongflow',
    label: '交付定义、展示和人工决定',
  }),
  Object.freeze({
    id: 'platform-edge:strongflow-to-codex',
    from: 'platform:strongflow',
    to: 'platform:codex-core',
    label: '批准后的目标与阶段边界',
  }),
  Object.freeze({
    id: 'platform-edge:codex-to-repository',
    from: 'platform:codex-core',
    to: 'platform:repository',
    label: '工具和沙箱内的代码变更',
  }),
  Object.freeze({
    id: 'platform-edge:codex-to-dsh',
    from: 'platform:codex-core',
    to: 'platform:dsh',
    label: 'Session 事件与执行活动',
  }),
])

const PROCESS_NODES = Object.freeze([
  Object.freeze({
    id: 'process:delivery-spec',
    label: '交付定义',
    description: '固定目标、范围、约束和验收条件。',
    kind: 'stage' as const,
    trustBoundary: null,
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:solution',
    label: '方案设计',
    description: 'Planner 形成与当前 DeliverySpec 对应的方案。',
    kind: 'stage' as const,
    trustBoundary: null,
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:plan-review',
    label: '人工方案审核',
    description: '人检查方案、风险、未决项和两张图。',
    kind: 'decision' as const,
    trustBoundary: 'Human decision boundary',
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:clarifying',
    label: '重新澄清',
    description: '拒绝当前方案后重新检查交付定义。',
    kind: 'stage' as const,
    trustBoundary: null,
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:solution-revision',
    label: '修改方案',
    description: '按人工意见重新运行方案阶段。',
    kind: 'stage' as const,
    trustBoundary: null,
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:executing',
    label: '执行',
    description: '只有当前审核集合获批后，Codex 才进入代码执行。',
    kind: 'stage' as const,
    trustBoundary: 'Execution authority',
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:verifying',
    label: '独立验证',
    description: '独立 Session 根据验收条件检查候选结果。',
    kind: 'stage' as const,
    trustBoundary: 'Verification boundary',
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:attention',
    label: '需要人工处理',
    description: '证据不足或运行环境错误时暂停并等待决定。',
    kind: 'decision' as const,
    trustBoundary: 'Human decision boundary',
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:reworking',
    label: '返工',
    description: '验证失败或审核标注触发有上限的候选修改。',
    kind: 'stage' as const,
    trustBoundary: 'Execution authority',
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:delivery-review',
    label: '交付审核',
    description: '人检查冻结候选、变更和验收结论。',
    kind: 'decision' as const,
    trustBoundary: 'Human decision boundary',
    unresolved: false,
  }),
  Object.freeze({
    id: 'process:delivered',
    label: '已交付',
    description: '当前候选和证据集合完成最终批准。',
    kind: 'stage' as const,
    trustBoundary: null,
    unresolved: false,
  }),
])

const PROCESS_EDGES = Object.freeze([
  Object.freeze({ id: 'process-edge:spec-solution', from: 'process:delivery-spec', to: 'process:solution', label: '需求已确认' }),
  Object.freeze({ id: 'process-edge:solution-review', from: 'process:solution', to: 'process:plan-review', label: '冻结审核集合' }),
  Object.freeze({ id: 'process-edge:review-execute', from: 'process:plan-review', to: 'process:executing', label: '批准' }),
  Object.freeze({ id: 'process-edge:review-revise', from: 'process:plan-review', to: 'process:solution-revision', label: '要求修改' }),
  Object.freeze({ id: 'process-edge:revise-solution', from: 'process:solution-revision', to: 'process:solution', label: '重新设计' }),
  Object.freeze({ id: 'process-edge:review-clarify', from: 'process:plan-review', to: 'process:clarifying', label: '拒绝' }),
  Object.freeze({ id: 'process-edge:clarify-spec', from: 'process:clarifying', to: 'process:delivery-spec', label: '更新定义' }),
  Object.freeze({ id: 'process-edge:execute-verify', from: 'process:executing', to: 'process:verifying', label: '候选完成' }),
  Object.freeze({ id: 'process-edge:verify-rework', from: 'process:verifying', to: 'process:reworking', label: '失败' }),
  Object.freeze({ id: 'process-edge:rework-verify', from: 'process:reworking', to: 'process:verifying', label: '再次验证' }),
  Object.freeze({ id: 'process-edge:verify-attention', from: 'process:verifying', to: 'process:attention', label: '证据不足 / 环境错误' }),
  Object.freeze({ id: 'process-edge:attention-verify', from: 'process:attention', to: 'process:verifying', label: '继续验证' }),
  Object.freeze({ id: 'process-edge:verify-delivery-review', from: 'process:verifying', to: 'process:delivery-review', label: '全部通过' }),
  Object.freeze({ id: 'process-edge:delivery-review-delivered', from: 'process:delivery-review', to: 'process:delivered', label: '批准交付' }),
  Object.freeze({ id: 'process-edge:delivery-review-rework', from: 'process:delivery-review', to: 'process:reworking', label: '标注返工' }),
])

export const STRONGFLOW_PLAN_REVIEW_OPTIONS = Object.freeze([
  Object.freeze({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'approve',
    label: '批准执行',
    description: '批准当前交付定义、方案和两张图，允许进入执行阶段。',
  }),
  Object.freeze({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'request_changes',
    label: '要求修改',
    description: '保持当前 DeliverySpec，返回方案阶段处理逐项意见。',
  }),
  Object.freeze({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'reject',
    label: '拒绝方案',
    description: '拒绝当前方案并返回需求澄清阶段。',
  }),
])

function reviewError(
  code: StrongFlowPlanReviewErrorCode,
  message: string,
  cause?: unknown,
): never {
  throw new StrongFlowPlanReviewError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function digest(value: unknown): string {
  return createHash('sha256').update(JSON.stringify(value)).digest('hex')
}

function shortDigest(value: unknown): string {
  return digest(value).slice(0, 32)
}

function activePlanningRun(delivery: Delivery): StageRun {
  const runs = delivery.stageRuns.filter(run => (
    run.stage === 'planning' && run.status === 'running'
  ))
  if (delivery.status !== 'planning' || runs.length !== 1) {
    return reviewError(
      'INVALID_REVIEW_INPUT',
      'a plan-review set requires exactly one active planning StageRun',
    )
  }
  return runs[0]!
}

function planningBinding(delivery: Delivery, run: StageRun): SessionBinding {
  const bindings = delivery.sessionBindings.filter(binding => binding.stageRunId === run.id)
  if (bindings.length !== 1
    || bindings[0]?.dshSessionId === null
    || bindings[0]?.codexSessionId === null) {
    return reviewError(
      'INVALID_REVIEW_INPUT',
      'the planning StageRun must have one complete DSH and Codex SessionBinding',
    )
  }
  return bindings[0]!
}

function architectureDiagram(
  delivery: Delivery,
  planningRun: StageRun,
  solution: StrongFlowPlanReviewSolution,
): StrongFlowPlanReviewDiagram {
  const nodes = [
    ...ARCHITECTURE_PLATFORM_NODES,
    ...solution.components.map(component => Object.freeze({
      id: component.id,
      label: component.label,
      description: component.responsibility,
      kind: component.kind,
      trustBoundary: component.trustBoundary,
      unresolved: component.unresolved,
    })),
  ]
  const edges = [
    ...ARCHITECTURE_PLATFORM_EDGES,
    ...solution.connections.map(connection => Object.freeze({
      id: `solution-edge:${shortDigest(connection.id)}`,
      from: connection.from,
      to: connection.to,
      label: connection.label,
    })),
  ]
  return parseStrongFlowPlanReviewDiagram({
    id: `architecture:${shortDigest({
      deliveryId: delivery.id,
      specId: delivery.spec.id,
      specRevision: delivery.spec.revision,
      planningRunId: planningRun.id,
      solution,
    })}`,
    kind: 'system-architecture',
    title: '系统架构图',
    nodes,
    edges,
  }, 'generatedPlanReview.architectureDiagram')
}

function processDiagram(
  delivery: Delivery,
  planningRun: StageRun,
): StrongFlowPlanReviewDiagram {
  return parseStrongFlowPlanReviewDiagram({
    id: `process:${shortDigest({
      deliveryId: delivery.id,
      specId: delivery.spec.id,
      specRevision: delivery.spec.revision,
      planningRunId: planningRun.id,
    })}`,
    kind: 'process-flow',
    title: '交付流程图',
    nodes: PROCESS_NODES,
    edges: PROCESS_EDGES,
  }, 'generatedPlanReview.processDiagram')
}

function reviewSetWithoutDigest(
  value: Omit<StrongFlowPlanReviewContext, 'reviewSetSha256'>,
): Omit<StrongFlowPlanReviewContext, 'reviewSetSha256'> {
  return Object.freeze({
    schemaVersion: STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION,
    protocol: STRONGFLOW_PLAN_REVIEW_CONTEXT_PROTOCOL,
    deliveryId: value.deliveryId,
    deliverySpecId: value.deliverySpecId,
    deliverySpecRevision: value.deliverySpecRevision,
    planningStageRunId: value.planningStageRunId,
    planningSessionBindingId: value.planningSessionBindingId,
    reviewStageRunId: value.reviewStageRunId,
    attentionItemId: value.attentionItemId,
    solution: value.solution,
    architectureDiagram: value.architectureDiagram,
    processDiagram: value.processDiagram,
    risks: value.risks,
    unresolvedItems: value.unresolvedItems,
    preparedAtMillis: value.preparedAtMillis,
  })
}

function reviewSetDigest(value: Omit<StrongFlowPlanReviewContext, 'reviewSetSha256'>): string {
  return digest(reviewSetWithoutDigest(value))
}

function equal(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right)
}

/**
 * Freeze planner output into the existing blocking AttentionItem. The diagrams
 * are regenerated from safe structured data rather than accepting markup.
 */
export function createStrongFlowPlanReviewAttention(
  input: CreateStrongFlowPlanReviewAttentionInput,
): AttentionItem {
  const delivery = input.delivery
  const planningRun = activePlanningRun(delivery)
  const binding = planningBinding(delivery, planningRun)
  if (!Number.isSafeInteger(input.preparedAtMillis)
    || input.preparedAtMillis < planningRun.startedAtMillis
    || input.preparedAtMillis < binding.boundAtMillis) {
    return reviewError(
      'INVALID_REVIEW_INPUT',
      'plan-review preparation time must follow the bound planning StageRun',
    )
  }
  let solution: StrongFlowPlanReviewSolution
  try {
    solution = parseStrongFlowPlanReviewSolution(input.solution)
  } catch (error) {
    return reviewError('INVALID_REVIEW_INPUT', 'plan-review solution is invalid', error)
  }
  const attentionItemId = AttentionItemId(input.attentionItemId)
  const reviewStageRunId = StageRunId(input.reviewStageRunId)
  if (delivery.attentionItems.some(item => item.id === attentionItemId)
    || delivery.stageRuns.some(run => run.id === reviewStageRunId)) {
    return reviewError(
      'INVALID_REVIEW_INPUT',
      'plan-review Attention or StageRun identity already exists',
    )
  }
  const architecture = architectureDiagram(delivery, planningRun, solution)
  const process = processDiagram(delivery, planningRun)
  const withoutDigest = reviewSetWithoutDigest({
    schemaVersion: STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION,
    protocol: STRONGFLOW_PLAN_REVIEW_CONTEXT_PROTOCOL,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    deliverySpecRevision: delivery.spec.revision,
    planningStageRunId: planningRun.id,
    planningSessionBindingId: binding.id,
    reviewStageRunId,
    attentionItemId,
    solution,
    architectureDiagram: architecture,
    processDiagram: process,
    risks: input.risks,
    unresolvedItems: input.unresolvedItems,
    preparedAtMillis: input.preparedAtMillis,
  })
  let context: StrongFlowPlanReviewContext
  try {
    context = parseStrongFlowPlanReviewContext({
      ...withoutDigest,
      reviewSetSha256: reviewSetDigest(withoutDigest),
    })
  } catch (error) {
    return reviewError('INVALID_REVIEW_INPUT', 'plan-review context is invalid', error)
  }
  return parseAttentionItem({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: attentionItemId,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    stageRunId: reviewStageRunId,
    type: ATTENTION_ITEM_TYPES[1],
    title: '审核交付方案和执行边界',
    context: JSON.stringify(context),
    options: STRONGFLOW_PLAN_REVIEW_OPTIONS,
    assignedTo: input.assignedTo,
    blocking: true,
    status: 'open',
    resolution: null,
    resolvedBy: null,
    createdAtMillis: input.preparedAtMillis,
    resolvedAtMillis: null,
  })
}

/** Revalidate a caller-provided review Attention against current Delivery facts. */
export function validateStrongFlowPlanReviewAttention(
  delivery: Delivery,
  planningRun: StageRun,
  reviewRun: StageRun,
  attention: AttentionItem,
  now: number,
): StrongFlowPlanReviewContext {
  let context: StrongFlowPlanReviewContext
  try {
    context = parseStrongFlowPlanReviewContextText(attention.context)
  } catch (error) {
    return reviewError('INVALID_REVIEW_INPUT', 'plan-review Attention context is invalid', error)
  }
  const binding = delivery.sessionBindings.find(entry => (
    entry.id === context.planningSessionBindingId
    && entry.stageRunId === planningRun.id
  ))
  if (delivery.status !== 'planning'
    || planningRun.stage !== 'planning'
    || planningRun.status !== 'running'
    || reviewRun.stage !== 'plan-review'
    || reviewRun.actorType !== 'human'
    || context.deliveryId !== delivery.id
    || context.deliverySpecId !== delivery.spec.id
    || context.deliverySpecRevision !== delivery.spec.revision
    || context.planningStageRunId !== planningRun.id
    || context.reviewStageRunId !== reviewRun.id
    || context.attentionItemId !== attention.id
    || attention.deliveryId !== delivery.id
    || attention.deliverySpecId !== delivery.spec.id
    || attention.stageRunId !== reviewRun.id
    || attention.type !== 'decision_required'
    || attention.createdAtMillis !== context.preparedAtMillis
    || context.preparedAtMillis > now
    || binding === undefined
    || binding.dshSessionId === null
    || binding.codexSessionId === null
    || !equal(attention.options, STRONGFLOW_PLAN_REVIEW_OPTIONS)) {
    return reviewError(
      'STALE_REVIEW_SET',
      'plan-review Attention does not match the current spec, planning run, or session binding',
    )
  }
  const expectedArchitecture = architectureDiagram(delivery, planningRun, context.solution)
  const expectedProcess = processDiagram(delivery, planningRun)
  const withoutDigest = reviewSetWithoutDigest({
    schemaVersion: context.schemaVersion,
    protocol: context.protocol,
    deliveryId: context.deliveryId,
    deliverySpecId: context.deliverySpecId,
    deliverySpecRevision: context.deliverySpecRevision,
    planningStageRunId: context.planningStageRunId,
    planningSessionBindingId: context.planningSessionBindingId,
    reviewStageRunId: context.reviewStageRunId,
    attentionItemId: context.attentionItemId,
    solution: context.solution,
    architectureDiagram: context.architectureDiagram,
    processDiagram: context.processDiagram,
    risks: context.risks,
    unresolvedItems: context.unresolvedItems,
    preparedAtMillis: context.preparedAtMillis,
  })
  if (!equal(context.architectureDiagram, expectedArchitecture)
    || !equal(context.processDiagram, expectedProcess)
    || context.reviewSetSha256 !== reviewSetDigest(withoutDigest)) {
    return reviewError(
      'STALE_REVIEW_SET',
      'plan-review diagrams or review-set digest do not match regenerated content',
    )
  }
  return context
}

export function createStrongFlowPlanReviewDecision(
  input: CreateStrongFlowPlanReviewDecisionInput,
): StrongFlowPlanReviewDecision {
  return parseStrongFlowPlanReviewDecision({
    schemaVersion: STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION,
    protocol: STRONGFLOW_PLAN_REVIEW_DECISION_PROTOCOL,
    action: input.action,
    deliveryId: input.context.deliveryId,
    deliverySpecId: input.context.deliverySpecId,
    deliverySpecRevision: input.context.deliverySpecRevision,
    reviewStageRunId: input.context.reviewStageRunId,
    attentionItemId: input.context.attentionItemId,
    reviewSetSha256: input.context.reviewSetSha256,
    comments: input.comments,
    requestedChanges: input.requestedChanges,
  })
}

/** Require an action tied to the exact frozen review set before changing stage. */
export function validateStrongFlowPlanReviewDecision(
  delivery: Delivery,
  item: AttentionItem,
  status: Exclude<AttentionItemStatus, 'open'>,
  resolution: string,
): ValidatedStrongFlowPlanReviewDecision {
  let context: StrongFlowPlanReviewContext
  let decision: StrongFlowPlanReviewDecision
  try {
    context = parseStrongFlowPlanReviewContextText(item.context)
    decision = parseStrongFlowPlanReviewDecisionText(resolution)
  } catch (error) {
    return reviewError(
      'INVALID_REVIEW_DECISION',
      'plan-review decision must use the current structured decision protocol',
      error,
    )
  }
  if (context.deliveryId !== delivery.id
    || context.deliverySpecId !== delivery.spec.id
    || context.deliverySpecRevision !== delivery.spec.revision
    || context.reviewStageRunId !== item.stageRunId
    || context.attentionItemId !== item.id
    || decision.deliveryId !== context.deliveryId
    || decision.deliverySpecId !== context.deliverySpecId
    || decision.deliverySpecRevision !== context.deliverySpecRevision
    || decision.reviewStageRunId !== context.reviewStageRunId
    || decision.attentionItemId !== context.attentionItemId
    || decision.reviewSetSha256 !== context.reviewSetSha256) {
    return reviewError(
      'STALE_REVIEW_SET',
      'plan-review decision references a stale or different review set',
    )
  }
  const expectedStatus = decision.action === 'approve' ? 'resolved' : 'dismissed'
  if (status !== expectedStatus) {
    return reviewError(
      'INVALID_REVIEW_DECISION',
      `plan-review action ${decision.action} requires Attention status ${expectedStatus}`,
    )
  }
  const nextStatus: DeliveryStatus = decision.action === 'approve'
    ? 'executing'
    : decision.action === 'request_changes'
      ? 'planning'
      : 'clarifying'
  return Object.freeze({
    decision,
    storedResolution: serializeStrongFlowPlanReviewDecision(decision),
    nextStatus,
  })
}

/** Rebuild the one approved current plan-review set for evidence packaging. */
export function assertStrongFlowPlanReviewCurrent(
  deliveryValue: Delivery,
): CurrentStrongFlowPlanReview {
  let delivery: Delivery
  try {
    delivery = parseDelivery(deliveryValue)
  } catch (error) {
    return reviewError('INVALID_REVIEW_INPUT', 'plan-review lookup requires a valid Delivery', error)
  }
  const matches = delivery.attentionItems.flatMap((attention) => {
    if (attention.type !== 'decision_required'
      || attention.status !== 'resolved'
      || attention.resolution === null) return []
    try {
      const context = parseStrongFlowPlanReviewContextText(attention.context)
      const decision = parseStrongFlowPlanReviewDecisionText(attention.resolution)
      return decision.action === 'approve'
        && context.deliveryId === delivery.id
        && context.deliverySpecId === delivery.spec.id
        && context.deliverySpecRevision === delivery.spec.revision
        ? [{ attention, context, decision }]
        : []
    } catch {
      return []
    }
  })
  if (matches.length !== 1) {
    return reviewError(
      'STALE_REVIEW_SET',
      'Delivery must contain exactly one approved plan-review set for its current spec',
    )
  }
  const { attention, context, decision } = matches[0]!
  const planningStageRun = delivery.stageRuns.find(run => (
    run.id === context.planningStageRunId
  ))
  const reviewStageRun = delivery.stageRuns.find(run => (
    run.id === context.reviewStageRunId
  ))
  const planningSessionBinding = delivery.sessionBindings.find(binding => (
    binding.id === context.planningSessionBindingId
    && binding.stageRunId === context.planningStageRunId
  ))
  const reviewBindings = delivery.sessionBindings.filter(binding => (
    binding.stageRunId === context.reviewStageRunId
    && binding.dshSessionId !== null
    && binding.codexSessionId === null
  ))
  const reviewSessionBinding = reviewBindings[0]
  const expectedArchitecture = planningStageRun === undefined
    ? null
    : architectureDiagram(delivery, planningStageRun, context.solution)
  const expectedProcess = planningStageRun === undefined
    ? null
    : processDiagram(delivery, planningStageRun)
  const unsigned = reviewSetWithoutDigest({
    schemaVersion: context.schemaVersion,
    protocol: context.protocol,
    deliveryId: context.deliveryId,
    deliverySpecId: context.deliverySpecId,
    deliverySpecRevision: context.deliverySpecRevision,
    planningStageRunId: context.planningStageRunId,
    planningSessionBindingId: context.planningSessionBindingId,
    reviewStageRunId: context.reviewStageRunId,
    attentionItemId: context.attentionItemId,
    solution: context.solution,
    architectureDiagram: context.architectureDiagram,
    processDiagram: context.processDiagram,
    risks: context.risks,
    unresolvedItems: context.unresolvedItems,
    preparedAtMillis: context.preparedAtMillis,
  })
  if (planningStageRun?.stage !== 'planning'
    || planningStageRun.actorType !== 'codex'
    || planningStageRun.role !== 'planner'
    || planningStageRun.status !== 'succeeded'
    || planningStageRun.finishedAtMillis === null
    || planningSessionBinding === undefined
    || planningSessionBinding.dshSessionId === null
    || planningSessionBinding.codexSessionId === null
    || reviewStageRun?.stage !== 'plan-review'
    || reviewStageRun.actorType !== 'human'
    || reviewStageRun.role !== 'reviewer'
    || reviewStageRun.status !== 'succeeded'
    || reviewStageRun.finishedAtMillis === null
    || reviewBindings.length !== 1
    || reviewSessionBinding === undefined
    || attention.stageRunId !== reviewStageRun.id
    || attention.id !== context.attentionItemId
    || !attention.blocking
    || attention.createdAtMillis !== context.preparedAtMillis
    || attention.resolvedBy === null
    || attention.resolvedAtMillis === null
    || !equal(attention.options, STRONGFLOW_PLAN_REVIEW_OPTIONS)
    || decision.deliveryId !== context.deliveryId
    || decision.deliverySpecId !== context.deliverySpecId
    || decision.deliverySpecRevision !== context.deliverySpecRevision
    || decision.reviewStageRunId !== context.reviewStageRunId
    || decision.attentionItemId !== context.attentionItemId
    || decision.reviewSetSha256 !== context.reviewSetSha256
    || context.preparedAtMillis < planningStageRun.startedAtMillis
    || context.preparedAtMillis < planningSessionBinding.boundAtMillis
    || context.preparedAtMillis > planningStageRun.finishedAtMillis
    || planningStageRun.finishedAtMillis > reviewStageRun.startedAtMillis
    || context.preparedAtMillis > reviewStageRun.startedAtMillis
    || reviewSessionBinding.boundAtMillis < reviewStageRun.startedAtMillis
    || reviewSessionBinding.boundAtMillis > reviewStageRun.finishedAtMillis
    || reviewStageRun.finishedAtMillis !== attention.resolvedAtMillis
    || expectedArchitecture === null
    || expectedProcess === null
    || !equal(context.architectureDiagram, expectedArchitecture)
    || !equal(context.processDiagram, expectedProcess)
    || context.reviewSetSha256 !== reviewSetDigest(unsigned)) {
    return reviewError(
      'STALE_REVIEW_SET',
      'approved plan-review set does not match its current spec, sessions, diagrams, or decision',
    )
  }
  return Object.freeze({
    context,
    decision,
    attention,
    planningStageRun,
    planningSessionBinding,
    reviewStageRun,
    reviewSessionBinding,
  })
}
