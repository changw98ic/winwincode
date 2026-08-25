/// <reference lib="dom" />

import type { Context } from '@deepseek-ai/cordis'
import type { TypertClientRemote } from '@deepseek-ai/dsh-typert-protocol'
import {
  AcceptanceCriterionId,
  DELIVERY_SCHEMA_VERSION,
  DeliveryId,
  DeliverySpecId,
  DeliveryValidationError,
  STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
  STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL,
  STRONGFLOW_GITHUB_PUBLICATION_DECISION_PROTOCOL,
  STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION,
  STRONGFLOW_PLAN_REVIEW_DECISION_PROTOCOL,
  STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION,
  materializeStrongFlowDeliveryAdvanceRequest,
  materializeStrongFlowDeliveryRequest,
  deliveryIdForGitHubIssueSource,
  generateDeliveryId,
  parseGitHubIssueSourceRef,
  parseGitHubPullRequestTargetRef,
  parseStrongFlowPlanReviewContextText,
  parseStrongFlowPlanReviewDecision,
  parseStrongFlowPlanReviewDecisionText,
  parseStrongFlowGitHubPublicationContextText,
  parseStrongFlowGitHubPublicationDecision,
  parseStrongFlowGitHubPublicationDecisionText,
  serializeStrongFlowGitHubPublicationDecision,
  serializeStrongFlowPlanReviewDecision,
  type AttentionItem,
  type Delivery,
  type DeliveryStatus,
  type StrongFlowDeliveryRequest,
  type StrongFlowDeliveryAdvanceOutcome,
  type StrongFlowDeliveryAdvanceRequest,
  type StrongFlowDiagramExecutionProjection,
  type StrongFlowDiagramExecutionDiagram,
  type StrongFlowRuntimeExecutionProjection,
  type StrongFlowRemediationDiagramKind,
  type StrongFlowPlanReviewAction,
  type StrongFlowPlanReviewContext,
  type StrongFlowPlanReviewDiagram,
  type StrongFlowGitHubPublicationContext,
} from '@winwincode/contracts'
import {
  createElement,
  Fragment,
  useCallback,
  useEffect,
  useMemo,
  useState,
  type ChangeEvent,
  type FormEvent,
  type ReactElement,
  type ReactNode,
} from 'react'

import {
  STRONGFLOW_LOCAL_SESSION_AUTH_REFERENCE,
  mountStrongFlowDeliveryRemote,
} from './delivery-remote-client.js'

const POLL_INTERVAL_MILLIS = 2_000
const SELECTION_STORAGE_PREFIX = 'winwincode.strongflow.delivery'
const STYLE_ELEMENT_ID = 'winwincode-strongflow-styles'
let requestSequence = 0

interface ConversationViewRegistration {
  readonly name: 'conversation.view'
  readonly id: 'strongflow'
  readonly order: number
  readonly label: () => string
  readonly inject: (sessionId: string) => StrongFlowViewInjected
}

interface ConversationSlots {
  inject(name: 'conversation.view', register: () => unknown): () => void
  register(
    options: ConversationViewRegistration,
    component: (props: StrongFlowViewProps) => ReactElement,
  ): unknown
}

interface SessionListSnapshot {
  readonly byId: Readonly<Record<string, {
    readonly cwd?: string
  }>>
}

type StrongFlowRemoteResult = Awaited<ReturnType<
  TypertClientRemote['strongflow']['invoke']
>>

type StrongFlowAdvanceRemoteResult = Awaited<ReturnType<
  TypertClientRemote['strongflow']['advance']
>>

interface StrongFlowScopedRemote {
  readonly strongflow: {
    advance(
      request: StrongFlowDeliveryAdvanceRequest,
      signal?: AbortSignal,
    ): Promise<StrongFlowAdvanceRemoteResult>
    invoke(
      request: StrongFlowDeliveryRequest,
      signal?: AbortSignal,
    ): Promise<StrongFlowRemoteResult>
  }
}

interface StrongFlowClientContext {
  readonly remote: TypertClientRemote
  readonly sessions: {
    readonly list: {
      getSnapshot(): SessionListSnapshot
    }
    open(sessionId: string): void
    scope(sessionId: string): Context | undefined
  }
  readonly slots: ConversationSlots
}

export interface StrongFlowViewInjected {
  readonly defaultRepository: string
  readonly invokeAdvance: (
    request: StrongFlowDeliveryAdvanceRequest,
    signal?: AbortSignal,
  ) => Promise<StrongFlowDeliveryAdvanceInvocation>
  readonly invokeDelivery: (
    request: StrongFlowDeliveryRequest,
    signal?: AbortSignal,
  ) => Promise<StrongFlowDeliveryInvocation>
  readonly openSession: (sessionId: string) => void
}

export interface StrongFlowDeliveryAdvanceInvocation {
  readonly delivery: Delivery
  readonly outcome: StrongFlowDeliveryAdvanceOutcome
}

export interface StrongFlowDeliveryInvocation {
  readonly delivery: Delivery
  readonly diagramExecution: StrongFlowDiagramExecutionProjection | null
  readonly runtimeExecution: StrongFlowRuntimeExecutionProjection | null
}

export interface StrongFlowViewProps extends StrongFlowViewInjected {
  readonly sessionId: string
}

export interface StrongFlowCreateDraft {
  readonly deliveryId: string
  readonly title: string
  readonly goal: string
  readonly scope: string
  readonly outOfScope: string
  readonly constraints: string
  readonly criteria: string
  readonly repositoryKind: 'local-git' | 'github'
  readonly repositoryLocator: string
  readonly baseRevision: string
  readonly maxReworkAttempts: string
  readonly githubIssue: string
  readonly githubBaseBranch: string
  readonly githubHeadRepository: string
  readonly githubHeadBranch: string
}

export type StrongFlowClientErrorCode =
  | 'REMOTE_FAILURE'
  | 'DELIVERY_FAILURE'
  | 'ADVANCE_FAILURE'

export class StrongFlowClientError extends Error {
  readonly code: StrongFlowClientErrorCode
  readonly currentRevision: number | null

  constructor(
    code: StrongFlowClientErrorCode,
    message: string,
    currentRevision: number | null = null,
  ) {
    super(message)
    this.name = 'StrongFlowClientError'
    this.code = code
    this.currentRevision = currentRevision
  }
}

const STATUS_LABELS: Readonly<Record<DeliveryStatus, string>> = Object.freeze({
  draft: '草稿',
  clarifying: '需求澄清',
  ready: '需求已确认',
  planning: '方案设计',
  'plan-review': '方案审核',
  executing: '执行中',
  verifying: '验证中',
  reworking: '返工中',
  'needs-attention': '等待人工处理',
  'ready-to-deliver': '待交付审核',
  delivered: '已交付',
})

const VERDICT_LABELS = Object.freeze({
  pass: '通过',
  fail: '失败',
  inconclusive: '证据不足',
  infra_error: '运行环境错误',
} as const)

const STAGE_LABELS = Object.freeze({
  clarifying: '需求澄清',
  planning: '方案设计',
  'plan-review': '方案审核',
  executing: '执行',
  verifying: '独立验证',
  reworking: '返工',
  'delivery-review': '交付审核',
} as const)

function lines(value: string): readonly string[] {
  return Object.freeze([...new Set(value
    .split(/\r?\n/u)
    .map(entry => entry.trim())
    .filter(entry => entry.length > 0))])
}

function criterionLine(value: string): {
  readonly description: string
  readonly verificationMethod: string | null
} {
  const separator = value.indexOf('|')
  if (separator < 0) {
    return Object.freeze({ description: value.trim(), verificationMethod: null })
  }
  const description = value.slice(0, separator).trim()
  const verificationMethod = value.slice(separator + 1).trim()
  return Object.freeze({
    description,
    verificationMethod: verificationMethod.length === 0 ? null : verificationMethod,
  })
}

function githubIssueSource(value: string) {
  const match = /^([A-Za-z0-9][A-Za-z0-9-]{0,38}\/[A-Za-z0-9._-]{1,100})#([1-9][0-9]*)$/u
    .exec(value.trim())
  if (match === null) throw new TypeError('GitHub Issue 必须使用 owner/repository#number 格式')
  return parseGitHubIssueSourceRef({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    provider: 'github',
    kind: 'issue',
    repository: match[1],
    number: Number(match[2]),
  })
}

/** Convert one browser form into the same strict create request used by the CLI. */
export function createDeliveryRequestFromDraft(
  draft: StrongFlowCreateDraft,
  requestId: string,
  createdAtMillis: number,
): StrongFlowDeliveryRequest {
  const sourceRef = draft.repositoryKind === 'github'
    ? githubIssueSource(draft.githubIssue)
    : null
  const requestedDeliveryId = draft.deliveryId.trim()
  const parsedDeliveryId = sourceRef === null
    ? requestedDeliveryId.length === 0
      ? generateDeliveryId(createdAtMillis)
      : DeliveryId(requestedDeliveryId)
    : deliveryIdForGitHubIssueSource(sourceRef)
  if (sourceRef !== null
    && requestedDeliveryId.length > 0
    && DeliveryId(requestedDeliveryId) !== parsedDeliveryId) {
    throw new DeliveryValidationError(
      'RELATIONSHIP_MISMATCH',
      'deliveryId',
      'GitHub Issue 对应的 Delivery ID 与来源不一致',
    )
  }
  const deliveryId = parsedDeliveryId
  const publicationTarget = sourceRef === null
    ? null
    : parseGitHubPullRequestTargetRef({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      provider: 'github',
      kind: 'pull-request',
      repository: draft.repositoryLocator.trim(),
      baseBranch: draft.githubBaseBranch.trim(),
      headRepository: draft.githubHeadRepository.trim()
        || draft.repositoryLocator.trim(),
      headBranch: draft.githubHeadBranch.trim(),
    })
  const criteria = lines(draft.criteria).map(criterionLine)
  return materializeStrongFlowDeliveryRequest('createDelivery', requestId, {
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: DeliverySpecId(`${deliveryId}:spec:1`),
      deliveryId: parsedDeliveryId,
      revision: 1,
      title: draft.title.trim(),
      goal: draft.goal.trim(),
      scope: lines(draft.scope),
      outOfScope: lines(draft.outOfScope),
      constraints: lines(draft.constraints),
      acceptanceCriteria: criteria.map((criterion, index) => ({
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: AcceptanceCriterionId(`${deliveryId}:criterion:${String(index + 1)}`),
        description: criterion.description,
        verificationMethod: criterion.verificationMethod,
        required: true,
      })),
      sourceRef,
      publicationTarget,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: draft.repositoryKind,
        locator: draft.repositoryLocator.trim(),
      },
      baseRevision: draft.baseRevision.trim(),
      maxReworkAttempts: Number(draft.maxReworkAttempts),
      createdAtMillis,
    },
    tasks: [],
  })
}

/** Confirm the exact draft on screen as the next immutable DeliverySpec revision. */
export function createRequirementsApprovalRequest(
  delivery: Delivery,
  requestIdValue: string,
): StrongFlowDeliveryRequest {
  if (delivery.status !== 'draft' && delivery.status !== 'clarifying') {
    throw new TypeError('只有草稿或需求澄清状态可以确认交付定义')
  }
  const nextRevision = delivery.spec.revision + 1
  const suffix = `:approved:${String(nextRevision)}`
  const nextSpecId = DeliverySpecId(
    `${delivery.spec.id.slice(0, 200 - suffix.length)}${suffix}`,
  )
  return materializeStrongFlowDeliveryRequest('updateDeliverySpec', requestIdValue, {
    deliveryId: delivery.id,
    expectedRevision: delivery.revision,
    spec: {
      schemaVersion: delivery.spec.schemaVersion,
      id: nextSpecId,
      deliveryId: delivery.spec.deliveryId,
      revision: nextRevision,
      title: delivery.spec.title,
      goal: delivery.spec.goal,
      scope: [...delivery.spec.scope],
      outOfScope: [...delivery.spec.outOfScope],
      constraints: [...delivery.spec.constraints],
      acceptanceCriteria: delivery.spec.acceptanceCriteria.map(criterion => ({
        schemaVersion: criterion.schemaVersion,
        id: criterion.id,
        description: criterion.description,
        verificationMethod: criterion.verificationMethod,
        required: criterion.required,
      })),
      sourceRef: delivery.spec.sourceRef === null
        ? null
        : { ...delivery.spec.sourceRef },
      publicationTarget: delivery.spec.publicationTarget === null
        ? null
        : { ...delivery.spec.publicationTarget },
      repository: { ...delivery.spec.repository },
      baseRevision: delivery.spec.baseRevision,
      maxReworkAttempts: delivery.spec.maxReworkAttempts,
      createdAtMillis: delivery.spec.createdAtMillis,
    },
  })
}

function requestId(operation: string, identity: string): string {
  requestSequence += 1
  const compactIdentity = identity.slice(0, 160)
  return `ui:${operation}:${compactIdentity}:${Date.now().toString(36)}:${requestSequence.toString(36)}`
}

export interface StrongFlowPlanReviewDecisionRequestInput {
  readonly delivery: Delivery
  readonly attentionItemId: string
  readonly action: StrongFlowPlanReviewAction
  readonly comments: string
  readonly requestedChanges: readonly string[]
  readonly requestId: string
}

/** Bind one browser decision to the exact open review set and Delivery revision on screen. */
export function createPlanReviewDecisionRequest(
  input: StrongFlowPlanReviewDecisionRequestInput,
): StrongFlowDeliveryRequest {
  const item = input.delivery.attentionItems.find(entry => (
    entry.id === input.attentionItemId
    && entry.status === 'open'
  ))
  if (item === undefined) throw new TypeError('当前方案审核项已经关闭或不存在')
  const context = parseStrongFlowPlanReviewContextText(item.context)
  if (context.deliveryId !== input.delivery.id
    || context.deliverySpecId !== input.delivery.spec.id
    || context.deliverySpecRevision !== input.delivery.spec.revision
    || context.reviewStageRunId !== item.stageRunId
    || context.attentionItemId !== item.id) {
    throw new TypeError('当前方案审核集合与 Delivery 版本不一致')
  }
  const decision = parseStrongFlowPlanReviewDecision({
    schemaVersion: STRONGFLOW_PLAN_REVIEW_SCHEMA_VERSION,
    protocol: STRONGFLOW_PLAN_REVIEW_DECISION_PROTOCOL,
    action: input.action,
    deliveryId: context.deliveryId,
    deliverySpecId: context.deliverySpecId,
    deliverySpecRevision: context.deliverySpecRevision,
    reviewStageRunId: context.reviewStageRunId,
    attentionItemId: context.attentionItemId,
    reviewSetSha256: context.reviewSetSha256,
    comments: input.comments,
    requestedChanges: input.requestedChanges,
  })
  return materializeStrongFlowDeliveryRequest('resolveAttention', input.requestId, {
    deliveryId: input.delivery.id,
    expectedRevision: input.delivery.revision,
    attentionItemId: item.id,
    status: input.action === 'approve' ? 'resolved' : 'dismissed',
    resolution: serializeStrongFlowPlanReviewDecision(decision),
    remediation: null,
    channel: 'local-ui',
    authentication: {
      scheme: 'local-session',
      proof: STRONGFLOW_LOCAL_SESSION_AUTH_REFERENCE,
    },
  })
}

export interface StrongFlowGitHubPublicationDecisionRequestInput {
  readonly delivery: Delivery
  readonly attentionItemId: string
  readonly comments: string
  readonly requestId: string
}

/** Bind a browser approval to the exact GitHub source, candidate, verdict, and PR target. */
export function createGitHubPublicationDecisionRequest(
  input: StrongFlowGitHubPublicationDecisionRequestInput,
): StrongFlowDeliveryRequest {
  const item = input.delivery.attentionItems.find(entry => (
    entry.id === input.attentionItemId && entry.status === 'open'
  ))
  if (item === undefined) throw new TypeError('当前 GitHub 发布审核项已经关闭或不存在')
  const context = parseStrongFlowGitHubPublicationContextText(item.context)
  const sourceRef = input.delivery.spec.sourceRef
  const publicationTarget = input.delivery.spec.publicationTarget
  if (sourceRef === null
    || publicationTarget === null
    || context.deliveryId !== input.delivery.id
    || context.deliverySpecId !== input.delivery.spec.id
    || context.deliverySpecRevision !== input.delivery.spec.revision
    || context.sourceRef.repository !== sourceRef.repository
    || context.sourceRef.number !== sourceRef.number
    || JSON.stringify(context.publicationTarget)
      !== JSON.stringify(publicationTarget)
    || context.candidateRef !== input.delivery.verdict?.candidateRef
    || context.deliveryVerdictId !== input.delivery.verdict.id
    || context.reviewStageRunId !== item.stageRunId
    || context.attentionItemId !== item.id) {
    throw new TypeError('当前 GitHub 发布集合与 Delivery 版本不一致')
  }
  const decision = parseStrongFlowGitHubPublicationDecision({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_DECISION_PROTOCOL,
    action: 'approve-publication',
    deliveryId: context.deliveryId,
    deliverySpecId: context.deliverySpecId,
    deliverySpecRevision: context.deliverySpecRevision,
    candidateRef: context.candidateRef,
    deliveryVerdictId: context.deliveryVerdictId,
    reviewStageRunId: context.reviewStageRunId,
    attentionItemId: context.attentionItemId,
    providerIdempotencyKey: context.providerIdempotencyKey,
    publicationSetSha256: context.publicationSetSha256,
    comments: input.comments.trim(),
  })
  return materializeStrongFlowDeliveryRequest('resolveAttention', input.requestId, {
    deliveryId: input.delivery.id,
    expectedRevision: input.delivery.revision,
    attentionItemId: item.id,
    status: 'resolved',
    resolution: serializeStrongFlowGitHubPublicationDecision(decision),
    remediation: null,
    channel: 'local-ui',
    authentication: {
      scheme: 'local-session',
      proof: STRONGFLOW_LOCAL_SESSION_AUTH_REFERENCE,
    },
  })
}

export interface StrongFlowLocalDeliveryApprovalRequestInput {
  readonly delivery: Delivery
  readonly attentionItemId: string
  readonly comments: string
  readonly requestId: string
}

/** Approve one exact local candidate and passing verdict from its bound review Session. */
export function createLocalDeliveryApprovalRequest(
  input: StrongFlowLocalDeliveryApprovalRequestInput,
): StrongFlowDeliveryRequest {
  const item = input.delivery.attentionItems.find(entry => (
    entry.id === input.attentionItemId
    && entry.type === 'delivery_approval'
    && entry.status === 'open'
    && entry.stageRunId !== null
  ))
  const run = item === undefined
    ? undefined
    : input.delivery.stageRuns.find(entry => (
      entry.id === item.stageRunId
      && entry.stage === 'delivery-review'
      && entry.actorType === 'human'
      && entry.status === 'waiting'
    ))
  const verdict = input.delivery.verdict
  let context: unknown
  try {
    context = item === undefined ? null : JSON.parse(item.context) as unknown
  } catch {
    context = null
  }
  if (item === undefined
    || run === undefined
    || verdict?.status !== 'pass'
    || typeof context !== 'object'
    || context === null
    || Array.isArray(context)
    || Reflect.get(context, 'candidateRef') !== verdict.candidateRef
    || Reflect.get(context, 'deliveryVerdictId') !== verdict.id
    || input.comments.trim().length === 0) {
    throw new TypeError('当前本地交付审核集合与通过的候选版本不一致')
  }
  return materializeStrongFlowDeliveryRequest('resolveAttention', input.requestId, {
    deliveryId: input.delivery.id,
    expectedRevision: input.delivery.revision,
    attentionItemId: item.id,
    status: 'resolved',
    resolution: input.comments.trim(),
    remediation: null,
    channel: 'local-ui',
    authentication: {
      scheme: 'local-session',
      proof: STRONGFLOW_LOCAL_SESSION_AUTH_REFERENCE,
    },
  })
}

export interface StrongFlowDiagramAnnotationDraft {
  readonly diagramKind: StrongFlowRemediationDiagramKind
  readonly nodeId: string
  readonly hunkId: string
  readonly note: string
}

export interface StrongFlowDiagramRemediationRequestInput {
  readonly delivery: Delivery
  readonly projection: StrongFlowDiagramExecutionProjection
  readonly attentionItemId: string
  readonly annotations: readonly StrongFlowDiagramAnnotationDraft[]
  readonly summary: string
  readonly requestId: string
}

/** Bind visible node annotations to the exact candidate, diff, file, hunk, and evidence. */
export function createDiagramRemediationRequest(
  input: StrongFlowDiagramRemediationRequestInput,
): StrongFlowDeliveryRequest {
  if (input.projection.state !== 'execution-finished'
    || input.projection.details === null
    || input.projection.deliveryId !== input.delivery.id
    || input.projection.deliveryRevision !== input.delivery.revision) {
    throw new TypeError('当前图不是这版 Delivery 的执行结束审核状态')
  }
  const item = input.delivery.attentionItems.find(entry => (
    entry.id === input.attentionItemId
    && entry.status === 'open'
    && entry.stageRunId !== null
  ))
  const reviewRun = item === undefined
    ? undefined
    : input.delivery.stageRuns.find(run => (
      run.id === item.stageRunId && run.stage === 'delivery-review'
    ))
  if (item === undefined || reviewRun === undefined) {
    throw new TypeError('当前交付审核项已经关闭或不存在')
  }
  if (input.annotations.length === 0) throw new TypeError('至少标注一个具体变更 hunk')
  const details = input.projection.details
  if (details.provenance.evidenceRefIds.length === 0) {
    throw new TypeError('当前候选还没有可绑定的验收依据')
  }
  const annotations = input.annotations.map((draft, index) => {
    const diagram = draft.diagramKind === 'system-architecture'
      ? input.projection.architecture
      : input.projection.process
    const node = diagram.nodes.find(entry => entry.nodeId === draft.nodeId)
    const hunk = details.hunks.find(entry => entry.id === draft.hunkId)
    const file = hunk === undefined
      ? undefined
      : details.files.find(entry => entry.id === hunk.fileId)
    if (node?.state !== 'affected-finished'
      || hunk === undefined
      || file === undefined
      || !node.fileIds.includes(file.id)
      || !file.nodeIds.includes(node.nodeId)
      || draft.note.trim().length === 0) {
      throw new TypeError('图标注没有指向当前黄色节点中的精确变更 hunk')
    }
    return Object.freeze({
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      id: `diagram-annotation:${String(index + 1)}:${hunk.sha256.slice(0, 32)}`,
      diagramKind: draft.diagramKind,
      diagramId: diagram.diagramId,
      nodeId: node.nodeId,
      filePath: file.path,
      hunkSha256: hunk.sha256,
      evidenceRefIds: details.provenance.evidenceRefIds,
      note: draft.note.trim(),
    })
  })
  return materializeStrongFlowDeliveryRequest('resolveAttention', input.requestId, {
    deliveryId: input.delivery.id,
    expectedRevision: input.delivery.revision,
    attentionItemId: item.id,
    status: 'dismissed',
    resolution: input.summary.trim(),
    remediation: {
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      protocol: STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL,
      deliveryTaskId: details.provenance.deliveryTaskId,
      candidate: {
        ...details.candidate,
        changedPaths: details.candidate.changedPaths.map(path => ({ ...path })),
      },
      annotations,
    },
    channel: 'local-ui',
    authentication: {
      scheme: 'local-session',
      proof: STRONGFLOW_LOCAL_SESSION_AUTH_REFERENCE,
    },
  })
}

export interface StrongFlowResolvedRemediationAdvanceInput {
  readonly request: StrongFlowDeliveryRequest
  readonly delivery: Delivery
  readonly requestId: string
  readonly invokeAdvance: (
    request: StrongFlowDeliveryAdvanceRequest,
  ) => Promise<StrongFlowDeliveryAdvanceInvocation>
}

/** Continue a validated diagram decision through its one remediator stage. */
export async function advanceResolvedDiagramRemediation(
  input: StrongFlowResolvedRemediationAdvanceInput,
): Promise<StrongFlowDeliveryAdvanceInvocation | null> {
  if (input.request.operation !== 'resolveAttention'
    || input.request.payload.remediation === null
    || input.delivery.id !== input.request.payload.deliveryId
    || input.delivery.status !== 'reworking') return null
  return input.invokeAdvance(materializeStrongFlowDeliveryAdvanceRequest(
    input.requestId,
    input.delivery.id,
    input.delivery.revision,
  ))
}

function selectionStorageKey(sessionId: string): string {
  return `${SELECTION_STORAGE_PREFIX}:${sessionId}`
}

function readSelection(sessionId: string): string {
  try {
    return globalThis.localStorage?.getItem(selectionStorageKey(sessionId)) ?? ''
  } catch {
    return ''
  }
}

function writeSelection(sessionId: string, deliveryId: string): void {
  try {
    if (deliveryId.length === 0) globalThis.localStorage?.removeItem(selectionStorageKey(sessionId))
    else globalThis.localStorage?.setItem(selectionStorageKey(sessionId), deliveryId)
  } catch {
    // Browser storage is a convenience for the selected view, never Delivery authority.
  }
}

function initialDraft(defaultRepository: string): StrongFlowCreateDraft {
  return Object.freeze({
    deliveryId: '',
    title: '',
    goal: '',
    scope: '',
    outOfScope: '',
    constraints: 'Codex Core remains the execution authority',
    criteria: '',
    repositoryKind: 'local-git',
    repositoryLocator: defaultRepository,
    baseRevision: 'HEAD',
    maxReworkAttempts: '2',
    githubIssue: '',
    githubBaseBranch: 'main',
    githubHeadRepository: '',
    githubHeadBranch: '',
  })
}

function errorText(error: unknown): string {
  if (error instanceof StrongFlowClientError && error.currentRevision !== null) {
    return `${error.message} 当前版本为 ${String(error.currentRevision)}。`
  }
  return error instanceof Error ? error.message : String(error)
}

function formatTime(value: number | null): string {
  if (value === null) return '进行中'
  try {
    return new Intl.DateTimeFormat('zh-CN', {
      dateStyle: 'medium',
      timeStyle: 'short',
    }).format(new Date(value))
  } catch {
    return String(value)
  }
}

function shortReference(value: string): string {
  return value.length <= 24 ? value : `${value.slice(0, 12)}…${value.slice(-8)}`
}

function inputValue(event: ChangeEvent<HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement>): string {
  return event.currentTarget.value
}

interface FieldProps {
  readonly label: string
  readonly name: string
  readonly value: string
  readonly onChange: (value: string) => void
  readonly placeholder?: string
  readonly required?: boolean
  readonly multiline?: boolean
  readonly help?: string
  readonly maxLength?: number
}

function Field(props: FieldProps): ReactElement {
  const controlProps = {
    id: `strongflow-${props.name}`,
    name: props.name,
    value: props.value,
    placeholder: props.placeholder,
    required: props.required,
    maxLength: props.maxLength,
    onChange: (event: ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) => {
      props.onChange(inputValue(event))
    },
  }
  return createElement('label', { className: 'wwc-field', htmlFor: controlProps.id },
    createElement('span', { className: 'wwc-field__label' }, props.label),
    props.multiline === true
      ? createElement('textarea', { ...controlProps, rows: 4 })
      : createElement('input', controlProps),
    props.help === undefined
      ? null
      : createElement('small', { className: 'wwc-field__help' }, props.help),
  )
}

interface EmptyStateProps {
  readonly defaultRepository: string
  readonly loading: boolean
  readonly loadValue: string
  readonly onLoadValue: (value: string) => void
  readonly onLoad: (event: FormEvent<HTMLFormElement>) => void
  readonly onCreate: (event: FormEvent<HTMLFormElement>) => void
  readonly draft: StrongFlowCreateDraft
  readonly onDraft: (draft: StrongFlowCreateDraft) => void
}

function EmptyState(props: EmptyStateProps): ReactElement {
  const patchDraft = (changes: Partial<StrongFlowCreateDraft>): void => {
    props.onDraft(Object.freeze({ ...props.draft, ...changes }))
  }
  return createElement('main', { className: 'wwc-empty' },
    createElement('section', { className: 'wwc-panel wwc-open-panel' },
      createElement('div', { className: 'wwc-panel__heading' },
        createElement('div', null,
          createElement('p', { className: 'wwc-eyebrow' }, '已有交付'),
          createElement('h2', null, '打开 Delivery'),
        ),
      ),
      createElement('form', { className: 'wwc-inline-form', onSubmit: props.onLoad },
        createElement('input', {
          'aria-label': 'Delivery ID',
          value: props.loadValue,
          placeholder: 'Delivery ID',
          required: true,
          maxLength: 200,
          onChange: (event: ChangeEvent<HTMLInputElement>) => {
            props.onLoadValue(inputValue(event))
          },
        }),
        createElement('button', { type: 'submit', disabled: props.loading },
          props.loading ? '读取中…' : '打开',
        ),
      ),
    ),
    createElement('section', { className: 'wwc-panel' },
      createElement('div', { className: 'wwc-panel__heading' },
        createElement('div', null,
          createElement('p', { className: 'wwc-eyebrow' }, '新的交付目标'),
          createElement('h2', null, '创建 Delivery'),
          createElement('p', { className: 'wwc-muted' },
            '先固定目标和验收条件。创建后仍需进入需求、方案和人工审核阶段。',
          ),
        ),
      ),
      createElement('form', { className: 'wwc-create-form', onSubmit: props.onCreate },
        createElement('div', { className: 'wwc-form-grid' },
          createElement(Field, {
            label: 'Delivery ID',
            name: 'delivery-id',
            value: props.draft.deliveryId,
            required: false,
            maxLength: 160,
            placeholder: '留空时自动生成',
            help: '使用 dlv_ 加 26 位大写标识；留空时由创建页面自动生成。',
            onChange: value => patchDraft({ deliveryId: value }),
          }),
          createElement(Field, {
            label: '标题',
            name: 'title',
            value: props.draft.title,
            required: true,
            maxLength: 256,
            placeholder: '实现邀请注册流程',
            onChange: value => patchDraft({ title: value }),
          }),
        ),
        createElement(Field, {
          label: '交付目标',
          name: 'goal',
          value: props.draft.goal,
          required: true,
          multiline: true,
          placeholder: '说明最终要解决的问题和可见结果',
          onChange: value => patchDraft({ goal: value }),
        }),
        createElement('div', { className: 'wwc-form-grid' },
          createElement(Field, {
            label: '范围',
            name: 'scope',
            value: props.draft.scope,
            required: true,
            multiline: true,
            placeholder: '每行一项',
            onChange: value => patchDraft({ scope: value }),
          }),
          createElement(Field, {
            label: '明确排除',
            name: 'out-of-scope',
            value: props.draft.outOfScope,
            multiline: true,
            placeholder: '每行一项，可留空',
            onChange: value => patchDraft({ outOfScope: value }),
          }),
        ),
        createElement(Field, {
          label: '约束',
          name: 'constraints',
          value: props.draft.constraints,
          multiline: true,
          placeholder: '每行一项，可留空',
          onChange: value => patchDraft({ constraints: value }),
        }),
        createElement(Field, {
          label: '验收条件',
          name: 'criteria',
          value: props.draft.criteria,
          required: true,
          multiline: true,
          placeholder: '每行一项；可写“条件 | 验证方法”',
          help: '没有验证方法的条件会在审核时保持为需要人工处理的事项。',
          onChange: value => patchDraft({ criteria: value }),
        }),
        createElement('div', { className: 'wwc-form-grid wwc-form-grid--repository' },
          createElement('label', { className: 'wwc-field', htmlFor: 'strongflow-repository-kind' },
            createElement('span', { className: 'wwc-field__label' }, '仓库类型'),
            createElement('select', {
              id: 'strongflow-repository-kind',
              value: props.draft.repositoryKind,
              onChange: (event: ChangeEvent<HTMLSelectElement>) => {
                const value = inputValue(event)
                patchDraft({ repositoryKind: value === 'github' ? 'github' : 'local-git' })
              },
            },
            createElement('option', { value: 'local-git' }, '本地 Git'),
            createElement('option', { value: 'github' }, 'GitHub'),
            ),
          ),
          createElement(Field, {
            label: '仓库位置',
            name: 'repository',
            value: props.draft.repositoryLocator,
            required: true,
            placeholder: props.draft.repositoryKind === 'github'
              ? 'owner/repository'
              : props.defaultRepository || '/workspace/repository',
            onChange: value => patchDraft({ repositoryLocator: value }),
          }),
          createElement(Field, {
            label: '基线版本',
            name: 'base-revision',
            value: props.draft.baseRevision,
            required: true,
            placeholder: 'Git commit 或 HEAD',
            onChange: value => patchDraft({ baseRevision: value }),
          }),
          createElement(Field, {
            label: '最多返工次数',
            name: 'max-rework',
            value: props.draft.maxReworkAttempts,
            required: true,
            placeholder: '2',
            onChange: value => patchDraft({ maxReworkAttempts: value }),
          }),
        ),
        props.draft.repositoryKind !== 'github'
          ? null
          : createElement(Fragment, null,
            createElement('div', { className: 'wwc-form-grid' },
              createElement(Field, {
                label: '来源 GitHub Issue',
                name: 'github-issue',
                value: props.draft.githubIssue,
                required: true,
                placeholder: 'owner/repository#42',
                help: 'WinWinCode 只保存仓库和 Issue 编号。标题、标签、负责人和讨论仍由 GitHub 管理。',
                onChange: value => patchDraft({ githubIssue: value }),
              }),
              createElement(Field, {
                label: 'PR 基础分支',
                name: 'github-base-branch',
                value: props.draft.githubBaseBranch,
                required: true,
                placeholder: 'main',
                onChange: value => patchDraft({ githubBaseBranch: value }),
              }),
            ),
            createElement('div', { className: 'wwc-form-grid' },
              createElement(Field, {
                label: 'PR Head 仓库',
                name: 'github-head-repository',
                value: props.draft.githubHeadRepository,
                placeholder: '留空时使用目标仓库',
                onChange: value => patchDraft({ githubHeadRepository: value }),
              }),
              createElement(Field, {
                label: 'PR Head 分支',
                name: 'github-head-branch',
                value: props.draft.githubHeadBranch,
                required: true,
                placeholder: 'winwincode/issue-42',
                onChange: value => patchDraft({ githubHeadBranch: value }),
              }),
            ),
          ),
        createElement('div', { className: 'wwc-form-actions' },
          createElement('button', { type: 'submit', disabled: props.loading },
            props.loading ? '创建中…' : '创建 Delivery',
          ),
        ),
      ),
    ),
  )
}

interface DeliveryProjectionProps {
  readonly delivery: Delivery
  readonly diagramExecution?: StrongFlowDiagramExecutionProjection | null
  readonly runtimeExecution?: StrongFlowRuntimeExecutionProjection | null
  readonly sessionId: string
  readonly refreshing: boolean
  readonly advancing: boolean
  readonly advanceMessage: string
  readonly onRefresh: () => void
  readonly onAdvance: () => void
  readonly onRequirementsApproval: () => void
  readonly onClose: () => void
  readonly openSession: (sessionId: string) => void
  readonly onPlanReviewDecision: (request: StrongFlowDeliveryRequest) => Promise<void>
}

function Metric(props: { readonly label: string; readonly value: ReactNode }): ReactElement {
  return createElement('div', { className: 'wwc-metric' },
    createElement('span', null, props.label),
    createElement('strong', null, props.value),
  )
}

function EmptyRow(props: { readonly children: ReactNode }): ReactElement {
  return createElement('p', { className: 'wwc-empty-row' }, props.children)
}

const RUNTIME_PLAN_STATUS_LABELS = Object.freeze({
  pending: '待执行',
  in_progress: '执行中',
  completed: '已完成',
} as const)

const RUNTIME_RECOVERY_LABELS = Object.freeze({
  none: '未发生故障',
  required: '等待恢复',
  'in-progress': '恢复中',
  recovered: '已恢复',
} as const)

function RuntimeExecutionPanel(props: {
  readonly delivery: Delivery
  readonly projection: StrongFlowRuntimeExecutionProjection | null
  readonly openSession: (sessionId: string) => void
}): ReactElement {
  const runs = new Map(props.delivery.stageRuns.map(run => [run.id, run]))
  const sessions = props.projection?.sessions ?? []
  return createElement('section', { className: 'wwc-panel wwc-runtime-execution' },
    createElement('div', { className: 'wwc-panel__heading' },
      createElement('div', null,
        createElement('p', { className: 'wwc-eyebrow' }, 'Codex Runtime'),
        createElement('h2', null, 'Codex 执行视图'),
        createElement('p', { className: 'wwc-muted' },
          '这里按绑定的 Session 重建当前计划、Agent 关系和执行活动；实时变更只显示数量。',
        ),
      ),
    ),
    sessions.length === 0
      ? createElement(EmptyRow, null, '当前还没有可展示的 Codex 运行事件。')
      : createElement('div', { className: 'wwc-stack' }, sessions.map((session) => {
        const run = runs.get(session.stageRunId)
        const pendingInteractions = session.interactions.filter(entry => entry.status === 'pending')
        return createElement('article', {
          className: 'wwc-runtime-session',
          key: session.sessionBindingId,
        },
        createElement('div', { className: 'wwc-row__title' },
          createElement('strong', null,
            run === undefined
              ? '已绑定 Codex Session'
              : `${STAGE_LABELS[run.stage]} · ${run.role}`,
          ),
          createElement('span', { className: 'wwc-code' },
            `事件 ${session.asOfSequence}`,
          ),
        ),
        createElement('div', { className: 'wwc-binding' },
          session.dshSessionId === null
            ? null
            : createElement('button', {
              className: 'wwc-session-link',
              type: 'button',
              onClick: () => props.openSession(session.dshSessionId!),
            }, `打开 Chat Session · ${session.dshSessionId}`),
          session.codexSessionId === null
            ? null
            : createElement('span', { className: 'wwc-code' },
              `Codex · ${session.codexSessionId}`,
            ),
        ),
        createElement('div', { className: 'wwc-runtime-grid' },
          createElement('section', { className: 'wwc-runtime-block' },
            createElement('h3', null, '当前 Plan'),
            session.plan === null
              ? createElement(EmptyRow, null, '当前 Session 尚未发布 Plan。')
              : createElement(Fragment, null,
                session.plan.explanation === null
                  ? null
                  : createElement('p', null, session.plan.explanation),
                session.plan.items.length === 0
                  ? createElement('pre', { className: 'wwc-runtime-text' },
                    session.plan.text ?? 'Plan 尚未形成结构化步骤。',
                  )
                  : createElement('ol', { className: 'wwc-runtime-list' },
                    session.plan.items.map((item, index) => createElement('li', {
                      key: `${item.step}:${String(index)}`,
                    },
                    createElement('span', {
                      className: `wwc-pill wwc-pill--${item.status}`,
                    }, RUNTIME_PLAN_STATUS_LABELS[item.status]),
                    createElement('span', null, item.step),
                    )),
                  ),
              ),
          ),
          createElement('section', { className: 'wwc-runtime-block' },
            createElement('h3', null, 'Agent Graph'),
            session.agents.length === 0
              ? createElement(EmptyRow, null, '当前还没有 Agent 节点。')
              : createElement('ul', { className: 'wwc-runtime-list' },
                session.agents.map(agent => createElement('li', { key: agent.threadId },
                  createElement('div', { className: 'wwc-row__title' },
                    createElement('strong', null, agent.nickname ?? agent.threadId),
                    createElement('span', {
                      className: `wwc-pill wwc-pill--${agent.status}`,
                    }, agent.status),
                  ),
                  agent.nickname === null
                    ? null
                    : createElement('p', { className: 'wwc-code' }, agent.threadId),
                  createElement('p', { className: 'wwc-muted' }, [
                    agent.role === null ? null : `角色：${agent.role}`,
                    agent.parentThreadId === null
                      ? '根节点'
                      : `父节点：${agent.parentThreadId}`,
                    agent.path === null ? null : `路径：${agent.path}`,
                  ].filter(Boolean).join(' · ')),
                )),
              ),
          ),
          createElement('section', { className: 'wwc-runtime-block' },
            createElement('h3', null, '最近命令与验证'),
            session.activities.length === 0
              ? createElement(EmptyRow, null, '当前还没有命令或测试活动。')
              : createElement('ul', { className: 'wwc-runtime-list' },
                session.activities.map(activity => createElement('li', { key: activity.callId },
                  createElement('div', { className: 'wwc-row__title' },
                    createElement('strong', null,
                      activity.activityType === 'test' ? '测试' : '命令',
                    ),
                    createElement('span', {
                      className: `wwc-pill wwc-pill--${activity.status}`,
                    }, `${activity.status} · ${activity.outcome}`),
                  ),
                  createElement('code', null, activity.command ?? '命令内容已隐藏'),
                  createElement('p', { className: 'wwc-muted' },
                    activity.exitCode === null
                      ? activity.callId
                      : `${activity.callId} · exit ${String(activity.exitCode)}`,
                  ),
                )),
              ),
          ),
          createElement('section', { className: 'wwc-runtime-block' },
            createElement('h3', null, '待处理交互'),
            pendingInteractions.length === 0
              ? createElement(EmptyRow, null, '当前没有等待处理的问题或执行审批。')
              : createElement('ul', { className: 'wwc-runtime-list' },
                pendingInteractions.map(interaction => createElement('li', { key: interaction.id },
                  createElement('div', { className: 'wwc-row__title' },
                    createElement('strong', null,
                      interaction.interactionType === 'user-input' ? 'Agent 提问' : '执行审批',
                    ),
                    interaction.blocking
                      ? createElement('span', { className: 'wwc-pill wwc-pill--blocking' },
                        '阻止当前 Turn',
                      )
                      : null,
                  ),
                  interaction.questions.length === 0
                    ? createElement('p', null, '等待用户处理。')
                    : createElement('ul', null, interaction.questions.map(question => (
                      createElement('li', { key: question.id },
                        createElement('strong', null, question.header),
                        createElement('p', null, question.question),
                      )
                    ))),
                )),
              ),
          ),
          createElement('section', { className: 'wwc-runtime-block' },
            createElement('h3', null, '失败与恢复'),
            createElement('p', null,
              `${RUNTIME_RECOVERY_LABELS[session.recovery.state]} · ${String(session.recovery.failureCount)} 次失败 · ${String(session.recovery.recoveryCount)} 次恢复`,
            ),
            session.failures.length === 0
              ? createElement(EmptyRow, null, '当前没有失败记录。')
              : createElement('ul', null, session.failures.map(failure => (
                createElement('li', { key: failure.event.eventId },
                  failure.code === null
                    ? failure.message
                    : `${failure.message}（${failure.code}）`,
                )
              ))),
          ),
          createElement('section', { className: 'wwc-runtime-block' },
            createElement('h3', null, '变更与用量摘要'),
            session.diffSummary === null
              ? createElement('p', null, '当前还没有代码变更摘要。')
              : createElement(Fragment, null,
                createElement('p', null,
                  `${String(session.diffSummary.changedFileCount)} 个文件 · +${String(session.diffSummary.additions)} / -${String(session.diffSummary.deletions)}`,
                ),
                createElement('p', { className: 'wwc-muted' },
                  '执行中不展示文件路径和具体变更；候选冻结后从黄色图节点进入审核。',
                ),
              ),
            session.usage === null
              ? createElement('p', { className: 'wwc-muted' }, '当前还没有模型用量。')
              : createElement('div', { className: 'wwc-runtime-usage' },
                Object.entries(session.usage.totals).map(([key, value]) => (
                  createElement('span', { className: 'wwc-code', key },
                    `${key}：${String(value)}`,
                  )
                )),
              ),
          ),
        ),
        )
      })),
  )
}

function domToken(value: string): string {
  return value.replace(/[^A-Za-z0-9_-]/gu, '-')
}

function planReviewContext(item: AttentionItem): StrongFlowPlanReviewContext | null {
  try {
    return parseStrongFlowPlanReviewContextText(item.context)
  } catch {
    return null
  }
}

function githubPublicationContext(
  item: AttentionItem,
): StrongFlowGitHubPublicationContext | null {
  try {
    return parseStrongFlowGitHubPublicationContextText(item.context)
  } catch {
    return null
  }
}

function PlanReviewDiagram(props: {
  readonly diagram: StrongFlowPlanReviewDiagram
  readonly execution?: StrongFlowDiagramExecutionDiagram | null
  readonly selectedNodeId?: string | null
  readonly onSelectNode?: (nodeId: string) => void
}): ReactElement {
  const diagram = props.diagram
  const captionId = `strongflow-diagram-${domToken(diagram.id)}-caption`
  const nodeLabels = new Map(diagram.nodes.map(node => [node.id, node.label]))
  const executionNodes = new Map((props.execution?.nodes ?? []).map(node => [node.nodeId, node]))
  const stateLabel = Object.freeze({
    normal: '正常流转',
    'affected-live': '执行中已发生变化',
    'affected-finished': '执行结束，等待审核',
  } as const)
  const stateIcon = Object.freeze({
    normal: '✓',
    'affected-live': '↻',
    'affected-finished': '!',
  } as const)
  return createElement('figure', {
    className: `wwc-diagram wwc-diagram--${diagram.kind}`,
    'data-diagram-id': diagram.id,
    'data-diagram-kind': diagram.kind,
    'aria-labelledby': captionId,
  },
  createElement('figcaption', { id: captionId, className: 'wwc-diagram__caption' },
    createElement('span', { className: 'wwc-eyebrow' },
      diagram.kind === 'system-architecture' ? 'System Architecture' : 'Process Flow',
    ),
    createElement('strong', null, diagram.title),
  ),
  createElement('ul', {
    className: 'wwc-diagram__nodes',
    'aria-label': `${diagram.title}节点`,
  }, diagram.nodes.map(node => (
    (() => {
      const execution = executionNodes.get(node.id)
      const state = execution?.state ?? 'normal'
      const selectable = state === 'affected-finished'
        && execution !== undefined
        && execution.fileIds.length > 0
        && props.onSelectNode !== undefined
      return createElement('li', {
      key: node.id,
      className: `wwc-diagram-node wwc-diagram-node--${node.kind}`,
      'data-node-id': node.id,
      'data-node-kind': node.kind,
      'data-definition-state': node.unresolved ? 'unresolved' : 'defined',
      'data-execution-state': state,
      'aria-current': props.selectedNodeId === node.id ? 'true' : undefined,
    },
    createElement('div', { className: 'wwc-diagram-node__heading' },
      createElement('strong', null, node.label),
      createElement('span', { className: 'wwc-pill' }, node.kind),
    ),
    createElement('span', {
      className: `wwc-diagram-node__state wwc-diagram-node__state--${state}`,
      'data-state-icon': stateIcon[state],
    }, `${stateIcon[state]} ${stateLabel[state]}`),
    createElement('p', null, node.description),
    node.trustBoundary === null
      ? null
      : createElement('p', { className: 'wwc-diagram-node__boundary' },
        `边界：${node.trustBoundary}`,
      ),
    node.unresolved
      ? createElement('span', { className: 'wwc-pill wwc-pill--blocking' }, '仍需确认')
      : null,
    selectable
      ? createElement('button', {
        className: 'wwc-diagram-node__review',
        type: 'button',
        'aria-pressed': props.selectedNodeId === node.id,
        onClick: () => props.onSelectNode?.(node.id),
      }, `查看 ${String(execution.affectedFileCount)} 个变更文件`)
      : null,
    )
    })()
  ))),
  createElement('div', { className: 'wwc-diagram__relations' },
    createElement('h4', null, '节点关系'),
    createElement('ol', { 'aria-label': `${diagram.title}节点关系` }, diagram.edges.map(edge => (
      createElement('li', { key: edge.id, 'data-edge-id': edge.id },
        createElement('span', null, nodeLabels.get(edge.from) ?? edge.from),
        createElement('span', { className: 'wwc-diagram__arrow', 'aria-hidden': 'true' }, '→'),
        createElement('span', null, nodeLabels.get(edge.to) ?? edge.to),
        createElement('strong', null, edge.label),
      )
    ))),
  ),
  )
}

const PLAN_REVIEW_ACTION_LABELS: Readonly<Record<StrongFlowPlanReviewAction, string>> =
  Object.freeze({
    approve: '批准执行',
    request_changes: '要求修改方案',
    reject: '拒绝并返回需求澄清',
  })

interface PlanReviewPanelProps {
  readonly delivery: Delivery
  readonly item: AttentionItem
  readonly context: StrongFlowPlanReviewContext
  readonly diagramExecution: StrongFlowDiagramExecutionProjection | null
  readonly sessionId: string
  readonly openSession: (sessionId: string) => void
  readonly onDecision: (request: StrongFlowDeliveryRequest) => Promise<void>
}

interface SelectedDiagramNode {
  readonly diagramKind: StrongFlowRemediationDiagramKind
  readonly nodeId: string
}

function annotationDraftKey(
  diagramKind: StrongFlowRemediationDiagramKind,
  nodeId: string,
  hunkId: string,
): string {
  return `${diagramKind}\u0000${nodeId}\u0000${hunkId}`
}

function DiagramExecutionReview(props: {
  readonly delivery: Delivery
  readonly context: StrongFlowPlanReviewContext
  readonly projection: StrongFlowDiagramExecutionProjection
  readonly selected: SelectedDiagramNode | null
  readonly sessionId: string
  readonly openSession: (sessionId: string) => void
  readonly onDecision: (request: StrongFlowDeliveryRequest) => Promise<void>
}): ReactElement | null {
  const [notes, setNotes] = useState<Readonly<Record<string, string>>>({})
  const [summary, setSummary] = useState('按图上标注修正当前候选')
  const [submitting, setSubmitting] = useState(false)
  const [message, setMessage] = useState('')
  if (props.projection.state !== 'execution-finished' || props.projection.details === null) {
    return null
  }
  const details = props.projection.details
  const reviewItem = props.delivery.attentionItems.find((item) => {
    if (item.status !== 'open' || item.stageRunId === null) return false
    return props.delivery.stageRuns.some(run => (
      run.id === item.stageRunId && run.stage === 'delivery-review'
    ))
  })
  const reviewBinding = reviewItem?.stageRunId === null || reviewItem === undefined
    ? undefined
    : props.delivery.sessionBindings.find(binding => (
      binding.stageRunId === reviewItem.stageRunId
      && binding.dshSessionId !== null
      && binding.codexSessionId === null
    ))
  const ownsReview = reviewBinding?.dshSessionId === props.sessionId
  const diagram = props.selected?.diagramKind === 'system-architecture'
    ? props.context.architectureDiagram
    : props.context.processDiagram
  const projectedDiagram = props.selected?.diagramKind === 'system-architecture'
    ? props.projection.architecture
    : props.projection.process
  const selectedNode = props.selected === null
    ? undefined
    : projectedDiagram.nodes.find(node => node.nodeId === props.selected?.nodeId)
  const definitionNode = props.selected === null
    ? undefined
    : diagram.nodes.find(node => node.id === props.selected?.nodeId)
  const files = selectedNode === undefined
    ? []
    : selectedNode.fileIds.flatMap(fileId => (
      details.files.find(file => file.id === fileId) ?? []
    ))
  const drafts: StrongFlowDiagramAnnotationDraft[] = []
  for (const [key, note] of Object.entries(notes)) {
    if (note.trim().length === 0) continue
    const [diagramKind, nodeId, hunkId] = key.split('\u0000')
    if ((diagramKind === 'system-architecture' || diagramKind === 'process-flow')
      && nodeId !== undefined
      && hunkId !== undefined) {
      drafts.push(Object.freeze({ diagramKind, nodeId, hunkId, note }))
    }
  }
  const submit = async (): Promise<void> => {
    if (reviewItem === undefined) return
    setSubmitting(true)
    setMessage('')
    try {
      await props.onDecision(createDiagramRemediationRequest({
        delivery: props.delivery,
        projection: props.projection,
        attentionItemId: reviewItem.id,
        annotations: drafts,
        summary,
        requestId: requestId('diagram-remediation', reviewItem.id),
      }))
      setMessage('返工标注已绑定到当前候选，交付已重新进入执行。')
      setNotes({})
    } catch (error) {
      setMessage(errorText(error))
    } finally {
      setSubmitting(false)
    }
  }
  const producer = details.provenance
  return createElement('section', {
    className: 'wwc-diff-review',
    'aria-labelledby': `strongflow-diff-review-${domToken(props.context.attentionItemId)}`,
  },
  createElement('div', { className: 'wwc-section-heading' },
    createElement('p', { className: 'wwc-eyebrow' }, 'Frozen Candidate Diff'),
    createElement('h3', { id: `strongflow-diff-review-${domToken(props.context.attentionItemId)}` },
      '执行结束变更审核',
    ),
  ),
  createElement('dl', { className: 'wwc-review-identity' },
    createElement('div', null,
      createElement('dt', null, '候选'),
      createElement('dd', { className: 'wwc-code' }, details.candidate.candidateRef),
    ),
    createElement('div', null,
      createElement('dt', null, 'Diff SHA-256'),
      createElement('dd', { className: 'wwc-code' }, details.diffSha256),
    ),
    createElement('div', null,
      createElement('dt', null, '变更总计'),
      createElement('dd', null,
        `${String(details.files.length)} 文件 · +${String(details.additions)} / -${String(details.deletions)}`,
      ),
    ),
    createElement('div', null,
      createElement('dt', null, '执行来源'),
      createElement('dd', null,
        `${producer.role} · ${producer.stage} · 第 ${String(producer.attempt)} 次`,
      ),
    ),
  ),
  props.selected === null || selectedNode === undefined || definitionNode === undefined
    ? createElement(EmptyRow, null, '选择黄色节点后查看它对应的文件和精确 hunk。')
    : createElement(Fragment, null,
      createElement('div', { className: 'wwc-diff-review__selected' },
        createElement('strong', null, definitionNode.label),
        createElement('span', { className: 'wwc-code' }, props.selected.nodeId),
      ),
      files.map(file => createElement('article', {
        className: 'wwc-diff-file',
        key: `${props.selected!.diagramKind}:${props.selected!.nodeId}:${file.id}`,
      },
      createElement('div', { className: 'wwc-diff-file__heading' },
        createElement('strong', { className: 'wwc-code' }, file.path),
        createElement('span', null,
          `+${String(file.additions)} / -${String(file.deletions)}`,
        ),
      ),
      file.previousPath === null
        ? null
        : createElement('p', { className: 'wwc-muted' }, `原路径：${file.previousPath}`),
      file.hunkIds.map(hunkId => {
        const hunk = details.hunks.find(entry => entry.id === hunkId)
        if (hunk === undefined) return null
        const key = annotationDraftKey(props.selected!.diagramKind, props.selected!.nodeId, hunk.id)
        return createElement('section', { className: 'wwc-diff-hunk', key: hunk.id },
          createElement('div', { className: 'wwc-diff-hunk__heading' },
            createElement('strong', { className: 'wwc-code' }, hunk.header),
            createElement('span', { className: 'wwc-code' }, hunk.sha256),
          ),
          createElement('pre', { tabIndex: 0, 'aria-label': `${file.path} ${hunk.header} 变更内容` },
            createElement('code', null, hunk.content),
          ),
          !ownsReview
            ? null
            : createElement('label', { className: 'wwc-field' },
              createElement('span', { className: 'wwc-field__label' }, '对这个 hunk 的返工标注'),
              createElement('textarea', {
                value: notes[key] ?? '',
                rows: 3,
                maxLength: 65_536,
                onChange: (event: ChangeEvent<HTMLTextAreaElement>) => {
                  setNotes(current => ({ ...current, [key]: inputValue(event) }))
                },
              }),
            ),
        )
      }),
      )),
    ),
  createElement('section', { className: 'wwc-diff-provenance', 'aria-label': '变更来源' },
    createElement('h4', null, 'Agent 与执行依据'),
    createElement('p', null,
      `${producer.stageRunId} · ${producer.sessionBindingId} · ${producer.dshSessionId} · ${producer.codexSessionId}`,
    ),
    createElement('p', { className: 'wwc-muted' },
      `${formatTime(producer.startedAtMillis)} → ${formatTime(producer.finishedAtMillis)}`,
    ),
    producer.agents.length === 0
      ? createElement(EmptyRow, null, '当前运行记录没有子 Agent 节点。')
      : createElement('ul', null, producer.agents.map(agent => (
        createElement('li', { key: agent.threadId },
          `${agent.role ?? producer.role} · ${agent.status} · ${agent.path ?? agent.threadId}`,
        )
      ))),
    producer.activities.length === 0
      ? createElement(EmptyRow, null, '当前运行记录没有命令或测试。')
      : createElement('ul', null, producer.activities.map(activity => (
        createElement('li', { key: activity.callId },
          createElement('strong', null, activity.type === 'test' ? '测试' : '命令'),
          ' · ',
          createElement('code', null, activity.command ?? activity.callId),
          ` · ${activity.outcome}`,
          activity.exitCode === null ? '' : ` · exit ${String(activity.exitCode)}`,
          activity.occurredAtMillis === null ? '' : ` · ${formatTime(activity.occurredAtMillis)}`,
        )
      ))),
    createElement('p', { className: 'wwc-code' },
      producer.evidenceRefIds.length === 0
        ? '尚无当前候选 EvidenceRef'
        : `Evidence · ${producer.evidenceRefIds.join(' · ')}`,
    ),
  ),
  reviewItem === undefined
    ? createElement(EmptyRow, null, '当前没有开放的交付审核，变更内容仅供查看。')
    : !ownsReview
      ? createElement('section', { className: 'wwc-review-session-notice', role: 'status' },
        createElement('strong', null, '当前页面为只读变更视图'),
        createElement('p', null, '进入绑定的交付审核 Session 后才能提交返工标注。'),
        reviewBinding?.dshSessionId === null || reviewBinding === undefined
          ? null
          : createElement('button', {
            className: 'wwc-session-link',
            type: 'button',
            onClick: () => props.openSession(reviewBinding.dshSessionId!),
          }, `打开交付审核 Session · ${shortReference(reviewBinding.dshSessionId)}`),
      )
      : createElement('form', {
        className: 'wwc-remediation-form',
        'aria-busy': submitting,
        onSubmit: (event: FormEvent<HTMLFormElement>) => {
          event.preventDefault()
          void submit()
        },
      },
      createElement('label', { className: 'wwc-field' },
        createElement('span', { className: 'wwc-field__label' }, '本轮返工说明'),
        createElement('textarea', {
          value: summary,
          rows: 3,
          maxLength: 65_536,
          onChange: (event: ChangeEvent<HTMLTextAreaElement>) => setSummary(inputValue(event)),
        }),
      ),
      createElement('button', {
        className: 'wwc-button--warning',
        type: 'submit',
        disabled: submitting || drafts.length === 0 || summary.trim().length === 0
          || producer.evidenceRefIds.length === 0,
      }, submitting ? '正在提交返工…' : `提交 ${String(drafts.length)} 项标注并重新执行`),
      createElement('p', { className: 'wwc-review-result', role: 'status', 'aria-live': 'polite' }, message),
      ),
  )
}

function PlanReviewPanel(props: PlanReviewPanelProps): ReactElement {
  const [comments, setComments] = useState('')
  const [requestedChangesText, setRequestedChangesText] = useState('')
  const [submitting, setSubmitting] = useState<StrongFlowPlanReviewAction | null>(null)
  const [decisionMessage, setDecisionMessage] = useState('')
  const [selectedNode, setSelectedNode] = useState<SelectedDiagramNode | null>(null)
  const context = props.context
  const item = props.item
  const controlId = `strongflow-plan-review-${domToken(item.id)}`
  const reviewBinding = props.delivery.sessionBindings.find(binding => (
    binding.stageRunId === context.reviewStageRunId
    && binding.dshSessionId !== null
    && binding.codexSessionId === null
  ))
  const currentSessionOwnsReview = reviewBinding?.dshSessionId === props.sessionId
  const requestedChanges = lines(requestedChangesText)
  const diagramExecution = props.diagramExecution?.reviewSetSha256 === context.reviewSetSha256
    ? props.diagramExecution
    : null
  let recordedDecision: ReturnType<typeof parseStrongFlowPlanReviewDecisionText> | null = null
  if (item.resolution !== null) {
    try {
      recordedDecision = parseStrongFlowPlanReviewDecisionText(item.resolution)
    } catch {
      recordedDecision = null
    }
  }

  const decide = async (action: StrongFlowPlanReviewAction): Promise<void> => {
    setDecisionMessage('')
    setSubmitting(action)
    try {
      const request = createPlanReviewDecisionRequest({
        delivery: props.delivery,
        attentionItemId: item.id,
        action,
        comments: comments.trim(),
        requestedChanges: action === 'approve' ? [] : requestedChanges,
        requestId: requestId(`plan-review-${action}`, item.id),
      })
      await props.onDecision(request)
      setDecisionMessage(`${PLAN_REVIEW_ACTION_LABELS[action]}已记录。`)
    } catch (error) {
      setDecisionMessage(errorText(error))
    } finally {
      setSubmitting(null)
    }
  }

  return createElement('section', {
    className: 'wwc-panel wwc-plan-review',
    'aria-labelledby': `${controlId}-title`,
    'data-review-set-sha256': context.reviewSetSha256,
  },
  createElement('div', { className: 'wwc-panel__heading' },
    createElement('div', null,
      createElement('p', { className: 'wwc-eyebrow' }, 'Solution Review Set'),
      createElement('h2', { id: `${controlId}-title` }, '方案与人工审核'),
      createElement('p', { className: 'wwc-muted' },
        '以下方案、风险和两张图已绑定到当前交付定义。只有绑定的审核 Session 可以决定是否进入执行。',
      ),
    ),
    createElement('span', {
      className: `wwc-pill wwc-pill--${item.status}`,
    }, item.status === 'open' ? '等待决定' : item.status),
  ),
  createElement('dl', { className: 'wwc-review-identity' },
    createElement('div', null,
      createElement('dt', null, '交付定义'),
      createElement('dd', { className: 'wwc-code' },
        `${context.deliverySpecId} · v${String(context.deliverySpecRevision)}`,
      ),
    ),
    createElement('div', null,
      createElement('dt', null, '审核集合'),
      createElement('dd', { className: 'wwc-code' }, context.reviewSetSha256),
    ),
    createElement('div', null,
      createElement('dt', null, '方案来源'),
      createElement('dd', { className: 'wwc-code' }, context.planningStageRunId),
    ),
    createElement('div', null,
      createElement('dt', null, '冻结时间'),
      createElement('dd', null, formatTime(context.preparedAtMillis)),
    ),
  ),
  createElement('section', {
    className: 'wwc-solution',
    'aria-labelledby': `${controlId}-solution-title`,
  },
  createElement('div', { className: 'wwc-section-heading' },
    createElement('p', { className: 'wwc-eyebrow' }, 'Proposed Solution'),
    createElement('h3', { id: `${controlId}-solution-title` }, '实施方案'),
  ),
  createElement('p', { className: 'wwc-solution__summary' }, context.solution.summary),
  createElement('div', { className: 'wwc-solution__grid' },
    createElement('div', null,
      createElement('h4', null, '实施路径'),
      createElement('ol', null, context.solution.approach.map(step => (
        createElement('li', { key: step }, step)
      ))),
    ),
    createElement('div', null,
      createElement('h4', null, '方案组件'),
      createElement('ul', { className: 'wwc-component-list' }, context.solution.components.map(component => (
        createElement('li', { key: component.id, 'data-solution-component-id': component.id },
          createElement('strong', null, component.label),
          createElement('p', null, component.responsibility),
          component.trustBoundary === null
            ? null
            : createElement('span', { className: 'wwc-muted' }, component.trustBoundary),
          component.repositoryPathPrefixes.length === 0
            ? createElement('span', { className: 'wwc-muted' }, '未映射仓库路径')
            : createElement('span', { className: 'wwc-code' },
              `路径：${component.repositoryPathPrefixes.join(' · ')}`,
            ),
        )
      ))),
    ),
  ),
  ),
  createElement('div', {
    className: `wwc-diagram-cycle wwc-diagram-cycle--${diagramExecution?.state ?? 'before-execution'}`,
    role: 'status',
    'aria-live': 'polite',
  }, diagramExecution?.state === 'executing'
    ? '↻ 执行中状态：发生变化的节点为浅蓝色；执行结束前不开放具体变更内容。'
    : diagramExecution?.state === 'execution-finished'
      ? `! 执行结束状态：${String(diagramExecution.affectedFileCount)} 个变更文件已冻结；黄色节点可以审核。`
      : '✓ 执行前状态：全部节点为绿色，表示已批准的正常流转。'),
  createElement('div', { className: 'wwc-diagram-grid' },
    createElement(PlanReviewDiagram, {
      diagram: context.architectureDiagram,
      execution: diagramExecution?.architecture ?? null,
      selectedNodeId: selectedNode?.diagramKind === 'system-architecture'
        ? selectedNode.nodeId
        : null,
      onSelectNode: nodeId => setSelectedNode({ diagramKind: 'system-architecture', nodeId }),
    }),
    createElement(PlanReviewDiagram, {
      diagram: context.processDiagram,
      execution: diagramExecution?.process ?? null,
      selectedNodeId: selectedNode?.diagramKind === 'process-flow'
        ? selectedNode.nodeId
        : null,
      onSelectNode: nodeId => setSelectedNode({ diagramKind: 'process-flow', nodeId }),
    }),
  ),
  diagramExecution === null
    ? null
    : createElement(DiagramExecutionReview, {
      delivery: props.delivery,
      context,
      projection: diagramExecution,
      selected: selectedNode,
      sessionId: props.sessionId,
      openSession: props.openSession,
      onDecision: props.onDecision,
    }),
  createElement('div', { className: 'wwc-review-findings' },
    createElement('section', { 'aria-labelledby': `${controlId}-risks-title` },
      createElement('h3', { id: `${controlId}-risks-title` }, '风险'),
      context.risks.length === 0
        ? createElement(EmptyRow, null, '当前方案未记录风险。')
        : createElement('ul', null, context.risks.map(risk => (
          createElement('li', { key: risk }, risk)
        ))),
    ),
    createElement('section', { 'aria-labelledby': `${controlId}-unresolved-title` },
      createElement('h3', { id: `${controlId}-unresolved-title` }, '未决事项'),
      context.unresolvedItems.length === 0
        ? createElement(EmptyRow, null, '当前没有未决事项。')
        : createElement('ul', null, context.unresolvedItems.map(entry => (
          createElement('li', { key: entry }, entry)
        ))),
    ),
  ),
  item.status !== 'open'
    ? createElement('section', { className: 'wwc-review-decision' },
      createElement('h3', null, '已记录的人工决定'),
      recordedDecision === null
        ? createElement('p', { className: 'wwc-muted' }, '决定记录格式无效。')
        : createElement(Fragment, null,
          createElement('strong', null, PLAN_REVIEW_ACTION_LABELS[recordedDecision.action]),
          recordedDecision.comments.length === 0
            ? null
            : createElement('p', null, recordedDecision.comments),
          recordedDecision.requestedChanges.length === 0
            ? null
            : createElement('ul', null, recordedDecision.requestedChanges.map(change => (
              createElement('li', { key: change }, change)
            ))),
        ),
    )
    : !currentSessionOwnsReview
      ? createElement('section', { className: 'wwc-review-session-notice', role: 'status' },
        createElement('strong', null, '当前页面为只读审核视图'),
        createElement('p', null,
          reviewBinding === undefined
            ? '这项审核尚未绑定人工 DSH Session。'
            : '请进入绑定的审核 Session 后再提交决定。',
        ),
        reviewBinding?.dshSessionId === null || reviewBinding === undefined
          ? null
          : createElement('button', {
            className: 'wwc-session-link',
            type: 'button',
            onClick: () => props.openSession(reviewBinding.dshSessionId!),
          }, `打开审核 Session · ${shortReference(reviewBinding.dshSessionId)}`),
      )
      : createElement('form', {
        className: 'wwc-review-form',
        'aria-busy': submitting !== null,
        onSubmit: (event: FormEvent<HTMLFormElement>) => { event.preventDefault() },
      },
      createElement('fieldset', { disabled: submitting !== null },
        createElement('legend', null, '提交当前审核集合的人工决定'),
        createElement('label', { className: 'wwc-field', htmlFor: `${controlId}-comments` },
          createElement('span', { className: 'wwc-field__label' }, '审核意见'),
          createElement('textarea', {
            id: `${controlId}-comments`,
            value: comments,
            rows: 4,
            maxLength: 65_536,
            placeholder: '批准时可选；拒绝时必须说明原因。',
            onChange: (event: ChangeEvent<HTMLTextAreaElement>) => {
              setComments(inputValue(event))
            },
          }),
        ),
        createElement('label', { className: 'wwc-field', htmlFor: `${controlId}-changes` },
          createElement('span', { className: 'wwc-field__label' }, '要求修改项'),
          createElement('textarea', {
            id: `${controlId}-changes`,
            value: requestedChangesText,
            rows: 4,
            maxLength: 65_536,
            placeholder: '每行一项；选择“要求修改方案”时至少填写一项。',
            onChange: (event: ChangeEvent<HTMLTextAreaElement>) => {
              setRequestedChangesText(inputValue(event))
            },
          }),
        ),
        createElement('div', { className: 'wwc-review-actions' },
          createElement('button', {
            type: 'button',
            disabled: requestedChanges.length > 0,
            onClick: () => { void decide('approve') },
          }, submitting === 'approve' ? '正在批准…' : '批准执行'),
          createElement('button', {
            className: 'wwc-button--warning',
            type: 'button',
            disabled: requestedChanges.length === 0,
            onClick: () => { void decide('request_changes') },
          }, submitting === 'request_changes' ? '正在提交…' : '要求修改方案'),
          createElement('button', {
            className: 'wwc-button--danger',
            type: 'button',
            disabled: comments.trim().length === 0,
            onClick: () => { void decide('reject') },
          }, submitting === 'reject' ? '正在拒绝…' : '拒绝方案'),
        ),
        requestedChanges.length > 0
          ? createElement('p', { className: 'wwc-field__help' },
            '已填写修改项；清空后才可直接批准。',
          )
          : null,
      ),
      createElement('p', {
        className: 'wwc-review-result',
        role: decisionMessage.length === 0 ? undefined : 'status',
        'aria-live': 'polite',
      }, decisionMessage),
      ),
  )
}

function GitHubPublicationPanel(props: {
  readonly delivery: Delivery
  readonly item: AttentionItem
  readonly context: StrongFlowGitHubPublicationContext
  readonly sessionId: string
  readonly openSession: (sessionId: string) => void
  readonly onDecision: (request: StrongFlowDeliveryRequest) => Promise<void>
}): ReactElement {
  const [comments, setComments] = useState('已核对当前候选、验收结论和 GitHub 目标。')
  const [submitting, setSubmitting] = useState(false)
  const [message, setMessage] = useState('')
  const reviewBinding = props.delivery.sessionBindings.find(binding => (
    binding.stageRunId === props.context.reviewStageRunId
    && binding.dshSessionId !== null
    && binding.codexSessionId === null
  ))
  const ownsReview = reviewBinding?.dshSessionId === props.sessionId
  let recordedDecision: ReturnType<
    typeof parseStrongFlowGitHubPublicationDecisionText
  > | null = null
  if (props.item.resolution !== null) {
    try {
      recordedDecision = parseStrongFlowGitHubPublicationDecisionText(props.item.resolution)
    } catch {
      recordedDecision = null
    }
  }
  const approve = async (): Promise<void> => {
    setSubmitting(true)
    setMessage('')
    try {
      await props.onDecision(createGitHubPublicationDecisionRequest({
        delivery: props.delivery,
        attentionItemId: props.item.id,
        comments,
        requestId: requestId('github-publication-approve', props.item.id),
      }))
      setMessage('当前 GitHub 发布集合已经批准。')
    } catch (error) {
      setMessage(errorText(error))
    } finally {
      setSubmitting(false)
    }
  }
  const target = props.context.publicationTarget
  return createElement('section', {
    className: 'wwc-panel wwc-publication-review',
    'aria-labelledby': `github-publication-${domToken(props.item.id)}`,
    'data-publication-set-sha256': props.context.publicationSetSha256,
  },
  createElement('div', { className: 'wwc-panel__heading' },
    createElement('div', null,
      createElement('p', { className: 'wwc-eyebrow' }, 'GitHub Publication Set'),
      createElement('h2', { id: `github-publication-${domToken(props.item.id)}` },
        'GitHub 交付审核',
      ),
      createElement('p', { className: 'wwc-muted' },
        '这里批准的是精确发布集合；远端写入由后续发布步骤按同一身份执行。',
      ),
    ),
    createElement('span', { className: `wwc-pill wwc-pill--${props.item.status}` },
      props.item.status === 'open' ? '等待决定' : props.item.status,
    ),
  ),
  createElement('dl', { className: 'wwc-review-identity' },
    createElement('div', null,
      createElement('dt', null, '来源 Issue'),
      createElement('dd', { className: 'wwc-code' },
        `${props.context.sourceRef.repository}#${String(props.context.sourceRef.number)}`,
      ),
    ),
    createElement('div', null,
      createElement('dt', null, '目标 Pull Request'),
      createElement('dd', { className: 'wwc-code' },
        `${target.headRepository}:${target.headBranch} → ${target.repository}:${target.baseBranch}`,
      ),
    ),
    createElement('div', null,
      createElement('dt', null, '冻结候选'),
      createElement('dd', { className: 'wwc-code' }, props.context.candidateRef),
    ),
    createElement('div', null,
      createElement('dt', null, '交付结论'),
      createElement('dd', { className: 'wwc-code' }, props.context.deliveryVerdictId),
    ),
    createElement('div', null,
      createElement('dt', null, '发布集合'),
      createElement('dd', { className: 'wwc-code' }, props.context.publicationSetSha256),
    ),
    createElement('div', null,
      createElement('dt', null, '幂等身份'),
      createElement('dd', { className: 'wwc-code' }, props.context.providerIdempotencyKey),
    ),
  ),
  props.item.status !== 'open'
    ? createElement('section', { className: 'wwc-review-decision' },
      createElement('h3', null, '已记录的人工决定'),
      recordedDecision === null
        ? createElement('p', { className: 'wwc-muted' }, '决定记录格式无效。')
        : createElement(Fragment, null,
          createElement('strong', null, '已批准当前发布集合'),
          createElement('p', null, recordedDecision.comments),
        ),
    )
    : !ownsReview
      ? createElement('section', { className: 'wwc-review-session-notice', role: 'status' },
        createElement('strong', null, '当前页面为只读发布审核视图'),
        createElement('p', null,
          reviewBinding === undefined
            ? '这项审核尚未绑定人工 DSH Session。'
            : '请进入绑定的交付审核 Session 后再提交决定。',
        ),
        reviewBinding?.dshSessionId === null || reviewBinding === undefined
          ? null
          : createElement('button', {
            className: 'wwc-session-link',
            type: 'button',
            onClick: () => props.openSession(reviewBinding.dshSessionId!),
          }, `打开交付审核 Session · ${shortReference(reviewBinding.dshSessionId)}`),
      )
      : createElement('form', {
        className: 'wwc-review-form',
        'aria-busy': submitting,
        onSubmit: (event: FormEvent<HTMLFormElement>) => {
          event.preventDefault()
          void approve()
        },
      },
      createElement('label', { className: 'wwc-field' },
        createElement('span', { className: 'wwc-field__label' }, '发布审核意见'),
        createElement('textarea', {
          value: comments,
          rows: 3,
          maxLength: 65_536,
          onChange: (event: ChangeEvent<HTMLTextAreaElement>) => {
            setComments(inputValue(event))
          },
        }),
      ),
      createElement('button', {
        type: 'submit',
        disabled: submitting || comments.trim().length === 0,
      }, submitting ? '正在批准…' : '批准当前发布集合'),
      createElement('p', {
        className: 'wwc-review-result',
        role: message.length === 0 ? undefined : 'status',
        'aria-live': 'polite',
      }, message),
      ),
  )
}

function LocalDeliveryReviewPanel(props: {
  readonly delivery: Delivery
  readonly item: AttentionItem
  readonly sessionId: string
  readonly openSession: (sessionId: string) => void
  readonly onDecision: (request: StrongFlowDeliveryRequest) => Promise<void>
}): ReactElement {
  const [comments, setComments] = useState('已核对当前冻结候选、独立验证结果和验收依据。')
  const [submitting, setSubmitting] = useState(false)
  const [message, setMessage] = useState('')
  const reviewBinding = props.delivery.sessionBindings.find(binding => (
    binding.stageRunId === props.item.stageRunId
    && binding.dshSessionId !== null
    && binding.codexSessionId === null
  ))
  const ownsReview = reviewBinding?.dshSessionId === props.sessionId

  const approve = async (): Promise<void> => {
    setSubmitting(true)
    setMessage('')
    try {
      await props.onDecision(createLocalDeliveryApprovalRequest({
        delivery: props.delivery,
        attentionItemId: props.item.id,
        comments,
        requestId: requestId('local-delivery-approval', props.item.id),
      }))
      setMessage('当前本地候选已批准交付。')
    } catch (error) {
      setMessage(errorText(error))
    } finally {
      setSubmitting(false)
    }
  }

  return createElement('section', { className: 'wwc-panel wwc-local-delivery-review' },
    createElement('div', { className: 'wwc-panel__heading' },
      createElement('div', null,
        createElement('p', { className: 'wwc-eyebrow' }, 'Local Delivery Review'),
        createElement('h2', null, '本地交付审核'),
        createElement('p', { className: 'wwc-muted' },
          '审核冻结候选、黄色变更节点和通过的独立验收结论。',
        ),
      ),
      createElement('span', {
        className: `wwc-pill wwc-pill--${props.item.status}`,
      }, props.item.status === 'open' ? '等待决定' : props.item.status),
    ),
    props.item.status !== 'open'
      ? createElement('section', { className: 'wwc-review-decision' },
        createElement('h3', null, '已记录的人工决定'),
        createElement('p', null, props.item.resolution ?? '已批准当前本地候选。'),
      )
      : !ownsReview
        ? createElement('section', { className: 'wwc-review-session-notice', role: 'status' },
          createElement('strong', null, '当前页面为只读交付审核视图'),
          createElement('p', null,
            reviewBinding === undefined
              ? '这项审核尚未绑定人工 DSH Session。'
              : '请进入绑定的交付审核 Session 后再批准候选。',
          ),
          reviewBinding?.dshSessionId === null || reviewBinding === undefined
            ? null
            : createElement('button', {
              className: 'wwc-session-link',
              type: 'button',
              onClick: () => props.openSession(reviewBinding.dshSessionId!),
            }, `打开交付审核 Session · ${shortReference(reviewBinding.dshSessionId)}`),
        )
        : createElement('form', {
          className: 'wwc-review-form',
          'aria-busy': submitting,
          onSubmit: (event: FormEvent<HTMLFormElement>) => {
            event.preventDefault()
            void approve()
          },
        },
        createElement('label', { className: 'wwc-field' },
          createElement('span', { className: 'wwc-field__label' }, '交付审核意见'),
          createElement('textarea', {
            value: comments,
            rows: 3,
            maxLength: 65_536,
            onChange: (event: ChangeEvent<HTMLTextAreaElement>) => {
              setComments(inputValue(event))
            },
          }),
        ),
        createElement('button', {
          type: 'submit',
          disabled: submitting || comments.trim().length === 0,
        }, submitting ? '正在批准…' : '批准当前本地候选'),
        createElement('p', {
          className: 'wwc-review-result',
          role: message.length === 0 ? undefined : 'status',
          'aria-live': 'polite',
        }, message),
        ),
  )
}

/** Pure rendering of the ten canonical Delivery facts; it owns no execution state. */
export function StrongFlowDeliveryProjection(props: DeliveryProjectionProps): ReactElement {
  const delivery = props.delivery
  const advanceMessage = props.advanceMessage ?? ''
  const bindingsByRun = useMemo(() => new Map(
    delivery.sessionBindings.map(binding => [binding.stageRunId, binding]),
  ), [delivery.sessionBindings])
  const criterionResults = useMemo(() => new Map(
    (delivery.verdict?.criteria ?? []).map(result => [result.criterionId, result]),
  ), [delivery.verdict])
  const openAttentionCount = delivery.attentionItems.filter(item => item.status === 'open').length
  const planReviews = delivery.attentionItems.flatMap(item => {
    const context = planReviewContext(item)
    return context === null ? [] : [{ item, context }]
  })
  const publicationReviews = delivery.attentionItems.flatMap(item => {
    const context = githubPublicationContext(item)
    return context === null ? [] : [{ item, context }]
  })
  const localDeliveryReviews = delivery.attentionItems.filter(item => (
    item.type === 'delivery_approval'
    && item.stageRunId !== null
    && delivery.spec.publicationTarget === null
    && delivery.stageRuns.some(run => (
      run.id === item.stageRunId && run.stage === 'delivery-review'
    ))
  ))
  const planReviewIds = new Set(planReviews.map(review => review.item.id))
  const publicationReviewIds = new Set(publicationReviews.map(review => review.item.id))
  const localDeliveryReviewIds = new Set(localDeliveryReviews.map(item => item.id))
  const otherAttentionItems = delivery.attentionItems.filter(item => (
    !planReviewIds.has(item.id)
    && !publicationReviewIds.has(item.id)
    && !localDeliveryReviewIds.has(item.id)
  ))
  const openBlockingAttention = delivery.attentionItems.some(item => (
    item.blocking && item.status === 'open'
  ))
  const requirementsNeedApproval = delivery.status === 'draft'
    || delivery.status === 'clarifying'
  const canAdvance = !requirementsNeedApproval
    && delivery.status !== 'needs-attention'
    && delivery.status !== 'delivered'
    && !openBlockingAttention

  return createElement('main', { className: 'wwc-delivery' },
    createElement('section', { className: 'wwc-hero' },
      createElement('div', null,
        createElement('div', { className: 'wwc-hero__meta' },
          createElement('span', { className: `wwc-status wwc-status--${delivery.status}` },
            STATUS_LABELS[delivery.status],
          ),
          createElement('span', { className: 'wwc-code' }, delivery.id),
          createElement('span', { className: 'wwc-muted' }, `版本 ${String(delivery.revision)}`),
        ),
        createElement('h1', null, delivery.spec.title),
        createElement('p', { className: 'wwc-hero__goal' }, delivery.spec.goal),
      ),
      createElement('div', { className: 'wwc-hero__actions' },
        requirementsNeedApproval
          ? createElement('button', {
            className: 'wwc-button',
            type: 'button',
            disabled: props.advancing,
            onClick: props.onRequirementsApproval,
          }, props.advancing ? '正在确认…' : '确认需求定义')
          : createElement('button', {
            className: 'wwc-button',
            type: 'button',
            disabled: props.advancing || !canAdvance,
            onClick: props.onAdvance,
          }, props.advancing ? '正在推进…' : '推进下一阶段'),
        createElement('button', {
          className: 'wwc-button wwc-button--quiet',
          type: 'button',
          disabled: props.refreshing,
          onClick: props.onRefresh,
        }, props.refreshing ? '更新中…' : '立即更新'),
        createElement('button', {
          className: 'wwc-button wwc-button--quiet',
          type: 'button',
          onClick: props.onClose,
        }, '打开其他 Delivery'),
        advanceMessage.length === 0
          ? null
          : createElement('p', {
            className: 'wwc-advance-result',
            role: 'status',
            'aria-live': 'polite',
          }, advanceMessage),
      ),
    ),
    createElement('section', { className: 'wwc-metrics', 'aria-label': 'Delivery 摘要' },
      createElement(Metric, { label: '交付任务', value: delivery.tasks.length }),
      createElement(Metric, { label: '阶段运行', value: delivery.stageRuns.length }),
      createElement(Metric, { label: '待处理', value: openAttentionCount }),
      createElement(Metric, { label: '验收依据', value: delivery.evidence.length }),
    ),
    createElement('div', { className: 'wwc-columns' },
      createElement('div', { className: 'wwc-column' },
        createElement('section', { className: 'wwc-panel' },
          createElement('div', { className: 'wwc-panel__heading' },
            createElement('div', null,
              createElement('p', { className: 'wwc-eyebrow' }, 'DeliverySpec'),
              createElement('h2', null, '交付定义'),
            ),
            createElement('span', { className: 'wwc-code' }, delivery.spec.id),
          ),
          createElement('div', { className: 'wwc-spec-goal' },
            createElement('h3', null, '交付目标'),
            createElement('p', null, delivery.spec.goal),
          ),
          createElement('dl', { className: 'wwc-definition-grid' },
            createElement('div', null,
              createElement('dt', null, '仓库'),
              createElement('dd', null, delivery.spec.repository.locator),
            ),
            createElement('div', null,
              createElement('dt', null, '基线'),
              createElement('dd', { className: 'wwc-code' }, delivery.spec.baseRevision),
            ),
            createElement('div', null,
              createElement('dt', null, '返工上限'),
              createElement('dd', null, String(delivery.spec.maxReworkAttempts)),
            ),
            createElement('div', null,
              createElement('dt', null, '定义版本'),
              createElement('dd', null, String(delivery.spec.revision)),
            ),
            delivery.spec.sourceRef === null
              ? null
              : createElement('div', null,
                createElement('dt', null, '来源 Issue'),
                createElement('dd', { className: 'wwc-code' },
                  `${delivery.spec.sourceRef.repository}#${String(delivery.spec.sourceRef.number)}`,
                ),
              ),
            delivery.spec.publicationTarget === null
              ? null
              : createElement('div', null,
                createElement('dt', null, '目标 Pull Request'),
                createElement('dd', { className: 'wwc-code' },
                  `${delivery.spec.publicationTarget.headRepository}:${delivery.spec.publicationTarget.headBranch} → ${delivery.spec.publicationTarget.repository}:${delivery.spec.publicationTarget.baseBranch}`,
                ),
              ),
          ),
          createElement('div', { className: 'wwc-list-groups' },
            createElement('div', null,
              createElement('h3', null, '范围'),
              createElement('ul', null, delivery.spec.scope.map(item => (
                createElement('li', { key: item }, item)
              ))),
            ),
            createElement('div', null,
              createElement('h3', null, '明确排除'),
              delivery.spec.outOfScope.length === 0
                ? createElement(EmptyRow, null, '未设置')
                : createElement('ul', null, delivery.spec.outOfScope.map(item => (
                  createElement('li', { key: item }, item)
                ))),
            ),
            createElement('div', null,
              createElement('h3', null, '约束'),
              delivery.spec.constraints.length === 0
                ? createElement(EmptyRow, null, '未设置')
                : createElement('ul', null, delivery.spec.constraints.map(item => (
                  createElement('li', { key: item }, item)
                ))),
            ),
          ),
        ),
        createElement('section', { className: 'wwc-panel' },
          createElement('div', { className: 'wwc-panel__heading' },
            createElement('div', null,
              createElement('p', { className: 'wwc-eyebrow' }, 'Acceptance Criteria'),
              createElement('h2', null, '验收条件'),
            ),
          ),
          createElement('div', { className: 'wwc-stack' },
            delivery.spec.acceptanceCriteria.map(criterion => {
              const result = criterionResults.get(criterion.id)
              return createElement('article', { className: 'wwc-row', key: criterion.id },
                createElement('div', { className: 'wwc-row__body' },
                  createElement('div', { className: 'wwc-row__title' },
                    createElement('strong', null, criterion.description),
                    createElement('span', { className: 'wwc-code' }, criterion.id),
                  ),
                  createElement('p', { className: 'wwc-muted' },
                    criterion.verificationMethod ?? '验证方法待人工确认',
                  ),
                  result === undefined
                    ? null
                    : createElement('p', { className: 'wwc-explanation' }, result.explanation),
                ),
                createElement('span', {
                  className: `wwc-pill wwc-pill--${result?.verdict ?? 'pending'}`,
                }, result === undefined ? '待验证' : VERDICT_LABELS[result.verdict]),
              )
            }),
          ),
        ),
        ...planReviews.map(review => createElement(PlanReviewPanel, {
          key: review.item.id,
          delivery,
          item: review.item,
          context: review.context,
          diagramExecution: props.diagramExecution ?? null,
          sessionId: props.sessionId,
          openSession: props.openSession,
          onDecision: props.onPlanReviewDecision,
        })),
        ...publicationReviews.map(review => createElement(GitHubPublicationPanel, {
          key: review.item.id,
          delivery,
          item: review.item,
          context: review.context,
          sessionId: props.sessionId,
          openSession: props.openSession,
          onDecision: props.onPlanReviewDecision,
        })),
        ...localDeliveryReviews.map(item => createElement(LocalDeliveryReviewPanel, {
          key: item.id,
          delivery,
          item,
          sessionId: props.sessionId,
          openSession: props.openSession,
          onDecision: props.onPlanReviewDecision,
        })),
        createElement('section', { className: 'wwc-panel' },
          createElement('div', { className: 'wwc-panel__heading' },
            createElement('div', null,
              createElement('p', { className: 'wwc-eyebrow' }, 'DeliveryTask'),
              createElement('h2', null, '独立交付单元'),
            ),
          ),
          delivery.tasks.length === 0
            ? createElement(EmptyRow, null, '尚未提升出需要独立验收的 DeliveryTask。')
            : createElement('div', { className: 'wwc-stack' }, delivery.tasks.map(task => (
              createElement('article', { className: 'wwc-row', key: task.id },
                createElement('div', { className: 'wwc-row__body' },
                  createElement('div', { className: 'wwc-row__title' },
                    createElement('strong', null, task.title),
                    createElement('span', { className: 'wwc-code' }, task.id),
                  ),
                  createElement('p', null, task.goal),
                  createElement('p', { className: 'wwc-muted' },
                    task.owner === null ? '未指定负责人' : `负责人：${task.owner}`,
                  ),
                ),
                createElement('span', { className: `wwc-pill wwc-pill--${task.status}` }, task.status),
              )
            ))),
        ),
      ),
      createElement('div', { className: 'wwc-column' },
        createElement('section', { className: 'wwc-panel' },
          createElement('div', { className: 'wwc-panel__heading' },
            createElement('div', null,
              createElement('p', { className: 'wwc-eyebrow' }, 'StageRun + SessionBinding'),
              createElement('h2', null, '交付阶段'),
              createElement('p', { className: 'wwc-muted' },
                '阶段由 WinWinCode 记录；下方视图从绑定的 Codex Session 只读重建执行事实。',
              ),
            ),
          ),
          delivery.stageRuns.length === 0
            ? createElement(EmptyRow, null, '尚未开始交付阶段。')
            : createElement('ol', { className: 'wwc-timeline' }, delivery.stageRuns.map(run => {
              const binding = bindingsByRun.get(run.id)
              return createElement('li', { key: run.id },
                createElement('div', { className: `wwc-timeline__dot wwc-timeline__dot--${run.status}` }),
                createElement('div', { className: 'wwc-timeline__content' },
                  createElement('div', { className: 'wwc-row__title' },
                    createElement('strong', null, STAGE_LABELS[run.stage]),
                    createElement('span', { className: `wwc-pill wwc-pill--${run.status}` }, run.status),
                  ),
                  createElement('p', null, `${run.role} · 第 ${String(run.attempt)} 次`),
                  createElement('p', { className: 'wwc-muted' },
                    `${formatTime(run.startedAtMillis)} → ${formatTime(run.finishedAtMillis)}`,
                  ),
                  binding === undefined
                    ? createElement('p', { className: 'wwc-muted' }, 'Session 尚未绑定')
                    : createElement('div', { className: 'wwc-binding' },
                      binding.dshSessionId === null
                        ? null
                        : createElement('button', {
                          className: 'wwc-session-link',
                          type: 'button',
                          onClick: () => props.openSession(binding.dshSessionId!),
                        }, `打开 Chat Session · ${shortReference(binding.dshSessionId)}`),
                      binding.codexSessionId === null
                        ? null
                        : createElement('span', { className: 'wwc-code' },
                          `Codex · ${shortReference(binding.codexSessionId)}`,
                        ),
                    ),
                ),
              )
            })),
        ),
        createElement(RuntimeExecutionPanel, {
          delivery,
          projection: props.runtimeExecution ?? null,
          openSession: props.openSession,
        }),
        createElement('section', { className: 'wwc-panel' },
          createElement('div', { className: 'wwc-panel__heading' },
            createElement('div', null,
              createElement('p', { className: 'wwc-eyebrow' }, 'Attention'),
              createElement('h2', null, '需要人工处理'),
            ),
          ),
          otherAttentionItems.length === 0
            ? createElement(EmptyRow, null,
              planReviews.length === 0 ? '当前没有业务决定。' : '当前没有其他业务决定。',
            )
            : createElement('div', { className: 'wwc-stack' }, otherAttentionItems.map(item => (
              createElement('article', { className: `wwc-row wwc-row--attention-${item.status}`, key: item.id },
                createElement('div', { className: 'wwc-row__body' },
                  createElement('div', { className: 'wwc-row__title' },
                    createElement('strong', null, item.title),
                    item.blocking
                      ? createElement('span', { className: 'wwc-pill wwc-pill--blocking' }, '阻止流转')
                      : null,
                  ),
                  createElement('p', null, item.context),
                  item.resolution === null
                    ? null
                    : createElement('p', { className: 'wwc-explanation' }, item.resolution),
                  createElement('p', { className: 'wwc-muted' },
                    item.assignedTo === null ? item.type : `${item.type} · ${item.assignedTo}`,
                  ),
                ),
                createElement('span', { className: `wwc-pill wwc-pill--${item.status}` }, item.status),
              )
            ))),
        ),
        createElement('section', { className: 'wwc-panel' },
          createElement('div', { className: 'wwc-panel__heading' },
            createElement('div', null,
              createElement('p', { className: 'wwc-eyebrow' }, 'Evidence'),
              createElement('h2', null, '验收依据'),
            ),
          ),
          delivery.evidence.length === 0
            ? createElement(EmptyRow, null, '尚未形成当前候选的验收依据。')
            : createElement('div', { className: 'wwc-stack' }, delivery.evidence.map(evidence => (
              createElement('article', { className: 'wwc-row wwc-row--compact', key: evidence.id },
                createElement('div', { className: 'wwc-row__body' },
                  createElement('strong', null, evidence.type),
                  createElement('p', { className: 'wwc-code' }, evidence.sourceRef),
                ),
                createElement('span', { className: 'wwc-code' }, shortReference(evidence.candidateRef)),
              )
            ))),
        ),
        createElement('section', { className: 'wwc-panel wwc-verdict' },
          createElement('div', { className: 'wwc-panel__heading' },
            createElement('div', null,
              createElement('p', { className: 'wwc-eyebrow' }, 'DeliveryVerdict'),
              createElement('h2', null, '交付结论'),
            ),
            delivery.verdict === null
              ? createElement('span', { className: 'wwc-pill wwc-pill--pending' }, '待验证')
              : createElement('span', {
                className: `wwc-pill wwc-pill--${delivery.verdict.status}`,
              }, VERDICT_LABELS[delivery.verdict.status]),
          ),
          delivery.verdict === null
            ? createElement(EmptyRow, null, '只有独立验证和全部依据通过后，才会产生交付结论。')
            : createElement(Fragment, null,
              createElement('p', { className: 'wwc-code' }, delivery.verdict.candidateRef),
              delivery.verdict.unresolvedFindings.length === 0
                ? createElement('p', { className: 'wwc-success-copy' }, '当前没有未解决发现。')
                : createElement('ul', null, delivery.verdict.unresolvedFindings.map(finding => (
                  createElement('li', { key: finding }, finding)
                ))),
            ),
        ),
      ),
    ),
  )
}

/** StrongFlow is an opt-in Delivery view inside the existing DSH conversation shell. */
export function StrongFlowView(props: StrongFlowViewProps): ReactElement {
  const [selectedDeliveryId, setSelectedDeliveryId] = useState(() => readSelection(props.sessionId))
  const [loadValue, setLoadValue] = useState(() => readSelection(props.sessionId))
  const [draft, setDraft] = useState(() => initialDraft(props.defaultRepository))
  const [delivery, setDelivery] = useState<Delivery | null>(null)
  const [diagramExecution, setDiagramExecution] =
    useState<StrongFlowDiagramExecutionProjection | null>(null)
  const [runtimeExecution, setRuntimeExecution] =
    useState<StrongFlowRuntimeExecutionProjection | null>(null)
  const [loading, setLoading] = useState(false)
  const [refreshing, setRefreshing] = useState(false)
  const [advancing, setAdvancing] = useState(false)
  const [advanceMessage, setAdvanceMessage] = useState('')
  const [error, setError] = useState<string | null>(null)

  const fetchDelivery = useCallback(async (
    deliveryId: string,
    signal?: AbortSignal,
  ): Promise<StrongFlowDeliveryInvocation> => props.invokeDelivery(
    materializeStrongFlowDeliveryRequest(
      'getDeliveryProjection',
      requestId('get', deliveryId),
      { deliveryId: DeliveryId(deliveryId) },
    ),
    signal,
  ), [props.invokeDelivery])

  useEffect(() => {
    if (selectedDeliveryId.length === 0) {
      setDelivery(null)
      setDiagramExecution(null)
      setRuntimeExecution(null)
      return undefined
    }
    const controller = new AbortController()
    let timer: ReturnType<typeof setTimeout> | undefined
    let active = true
    const poll = async (): Promise<void> => {
      try {
        const next = await fetchDelivery(selectedDeliveryId, controller.signal)
        if (!active) return
        setDelivery(previous => previous?.revision === next.delivery.revision
          ? previous
          : next.delivery)
        setDiagramExecution(next.diagramExecution)
        setRuntimeExecution(next.runtimeExecution)
        setError(null)
      } catch (cause) {
        if (!active || controller.signal.aborted) return
        setError(errorText(cause))
      } finally {
        if (active) {
          setLoading(false)
          timer = setTimeout(() => { void poll() }, POLL_INTERVAL_MILLIS)
        }
      }
    }
    setLoading(true)
    void poll()
    return () => {
      active = false
      controller.abort()
      if (timer !== undefined) clearTimeout(timer)
    }
  }, [fetchDelivery, selectedDeliveryId])

  const load = (event: FormEvent<HTMLFormElement>): void => {
    event.preventDefault()
    const deliveryId = loadValue.trim()
    if (deliveryId.length === 0) return
    setError(null)
    setAdvanceMessage('')
    setLoading(true)
    setSelectedDeliveryId(deliveryId)
    writeSelection(props.sessionId, deliveryId)
  }

  const create = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault()
    setError(null)
    setLoading(true)
    try {
      const request = createDeliveryRequestFromDraft(
        draft,
        requestId('create', draft.deliveryId.trim()),
        Date.now(),
      )
      const next = await props.invokeDelivery(request)
      setDelivery(next.delivery)
      setDiagramExecution(next.diagramExecution)
      setRuntimeExecution(next.runtimeExecution)
      setLoadValue(next.delivery.id)
      setSelectedDeliveryId(next.delivery.id)
      setAdvanceMessage('Delivery 草稿已创建，请确认需求定义。')
      writeSelection(props.sessionId, next.delivery.id)
    } catch (cause) {
      setError(errorText(cause))
    } finally {
      setLoading(false)
    }
  }

  const refresh = async (): Promise<void> => {
    if (selectedDeliveryId.length === 0) return
    setRefreshing(true)
    try {
      const next = await fetchDelivery(selectedDeliveryId)
      setDelivery(next.delivery)
      setDiagramExecution(next.diagramExecution)
      setRuntimeExecution(next.runtimeExecution)
      setError(null)
    } catch (cause) {
      setError(errorText(cause))
    } finally {
      setRefreshing(false)
    }
  }

  const decidePlanReview = async (request: StrongFlowDeliveryRequest): Promise<void> => {
    const startsDiagramRemediation = request.operation === 'resolveAttention'
      && request.payload.remediation !== null
    setError(null)
    if (startsDiagramRemediation) {
      setAdvanceMessage('正在确认候选绑定的返工标注…')
      setAdvancing(true)
    }
    try {
      const next = await props.invokeDelivery(request)
      setDelivery(next.delivery)
      setDiagramExecution(next.diagramExecution)
      setRuntimeExecution(next.runtimeExecution)
      if (startsDiagramRemediation) {
        setAdvanceMessage('返工标注已确认，remediator 正在执行。')
        const advanced = await advanceResolvedDiagramRemediation({
          request,
          delivery: next.delivery,
          requestId: requestId('advance-remediation', next.delivery.id),
          invokeAdvance: props.invokeAdvance,
        })
        if (advanced === null) {
          throw new StrongFlowClientError(
            'ADVANCE_FAILURE',
            '返工标注已记录，但 Delivery 尚未进入可执行的返工状态。',
            next.delivery.revision,
          )
        }
        setDelivery(advanced.delivery)
        setAdvanceMessage(advanced.outcome.message)
        const refreshed = await fetchDelivery(advanced.delivery.id)
        setDelivery(refreshed.delivery)
        setDiagramExecution(refreshed.diagramExecution)
        setRuntimeExecution(refreshed.runtimeExecution)
      }
    } catch (cause) {
      setError(errorText(cause))
      throw cause
    } finally {
      if (startsDiagramRemediation) setAdvancing(false)
    }
  }

  const approveRequirements = async (): Promise<void> => {
    if (delivery === null) return
    setError(null)
    setAdvanceMessage('')
    setAdvancing(true)
    try {
      const next = await props.invokeDelivery(createRequirementsApprovalRequest(
        delivery,
        requestId('approve-requirements', delivery.id),
      ))
      setDelivery(next.delivery)
      setDiagramExecution(next.diagramExecution)
      setRuntimeExecution(next.runtimeExecution)
      setAdvanceMessage('需求定义已确认，可以生成实施方案。')
    } catch (cause) {
      setError(errorText(cause))
    } finally {
      setAdvancing(false)
    }
  }

  const advance = async (): Promise<void> => {
    if (delivery === null) return
    setError(null)
    setAdvanceMessage('')
    setAdvancing(true)
    try {
      const advanced = await props.invokeAdvance(
        materializeStrongFlowDeliveryAdvanceRequest(
          requestId('advance', delivery.id),
          delivery.id,
          delivery.revision,
        ),
      )
      setDelivery(advanced.delivery)
      setAdvanceMessage(advanced.outcome.message)
      const next = await fetchDelivery(advanced.delivery.id)
      setDelivery(next.delivery)
      setDiagramExecution(next.diagramExecution)
      setRuntimeExecution(next.runtimeExecution)
    } catch (cause) {
      setError(errorText(cause))
    } finally {
      setAdvancing(false)
    }
  }

  const close = (): void => {
    setSelectedDeliveryId('')
    setLoadValue('')
    setDelivery(null)
    setDiagramExecution(null)
    setRuntimeExecution(null)
    setAdvanceMessage('')
    setError(null)
    writeSelection(props.sessionId, '')
  }

  const openSession = (sessionId: string): void => {
    if (delivery !== null) writeSelection(sessionId, delivery.id)
    props.openSession(sessionId)
  }

  return createElement('div', { className: 'wwc-workbench' },
    createElement('header', { className: 'wwc-workbench__bar' },
      createElement('div', null,
        createElement('span', { className: 'wwc-workbench__brand' }, 'WinWinCode'),
        createElement('span', { className: 'wwc-workbench__mode' }, 'StrongFlow'),
      ),
      createElement('span', { className: 'wwc-muted' },
        '交付目标 · 阶段 · 验收依据 · 结论',
      ),
    ),
    error === null
      ? null
      : createElement('div', { className: 'wwc-error', role: 'alert' }, error),
    delivery === null
      ? createElement(EmptyState, {
        defaultRepository: props.defaultRepository,
        loading,
        loadValue,
        onLoadValue: setLoadValue,
        onLoad: load,
        onCreate: event => { void create(event) },
        draft,
        onDraft: setDraft,
      })
      : createElement(StrongFlowDeliveryProjection, {
        delivery,
        diagramExecution,
        runtimeExecution,
        sessionId: props.sessionId,
        refreshing,
        advancing,
        advanceMessage,
        onRefresh: () => { void refresh() },
        onAdvance: () => { void advance() },
        onRequirementsApproval: () => { void approveRequirements() },
        onClose: close,
        openSession,
        onPlanReviewDecision: decidePlanReview,
      }),
  )
}

async function invokeDeliveryRemote(
  remote: StrongFlowScopedRemote,
  request: StrongFlowDeliveryRequest,
  signal?: AbortSignal,
): Promise<StrongFlowDeliveryInvocation> {
  const result = await remote.strongflow.invoke(request, signal)
  if (!result.ok) {
    throw new StrongFlowClientError('REMOTE_FAILURE', result.error.message)
  }
  if (!result.value.ok) {
    throw new StrongFlowClientError(
      'DELIVERY_FAILURE',
      result.value.error.message,
      result.value.error.currentRevision,
    )
  }
  return result.value.result
}

async function invokeAdvanceRemote(
  remote: StrongFlowScopedRemote,
  request: StrongFlowDeliveryAdvanceRequest,
  signal?: AbortSignal,
): Promise<StrongFlowDeliveryAdvanceInvocation> {
  const result = await remote.strongflow.advance(request, signal)
  if (!result.ok) {
    throw new StrongFlowClientError('REMOTE_FAILURE', result.error.message)
  }
  if (!result.value.ok) {
    throw new StrongFlowClientError(
      'ADVANCE_FAILURE',
      result.value.error.message,
      result.value.error.currentRevision,
    )
  }
  return result.value.result
}

const STRONGFLOW_STYLES = `
.wwc-workbench {
  min-height: 100%;
  overflow: auto;
  color: var(--dsw-alias-label-primary, #172033);
  background: var(--dsw-alias-bg-base, #f6f8fb);
  font-family: var(--dsw-font-family, Inter, ui-sans-serif, system-ui, sans-serif);
}
.wwc-workbench * { box-sizing: border-box; }
.wwc-workbench__bar {
  position: sticky;
  top: 0;
  z-index: 5;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  min-height: 52px;
  padding: 10px clamp(20px, 4vw, 56px);
  border-bottom: 1px solid var(--dsw-alias-border-l1, #e3e8f0);
  background: color-mix(in srgb, var(--dsw-alias-bg-base, #fff) 92%, transparent);
  backdrop-filter: blur(16px);
}
.wwc-workbench__brand { font-weight: 760; letter-spacing: -.02em; }
.wwc-workbench__mode {
  margin-left: 10px;
  padding: 3px 8px;
  border-radius: 999px;
  color: var(--dsw-alias-state-business-primary, #2f6fed);
  background: color-mix(in srgb, var(--dsw-alias-state-business-primary, #2f6fed) 11%, transparent);
  font-size: 12px;
  font-weight: 700;
}
.wwc-empty, .wwc-delivery {
  width: min(1440px, 100%);
  margin: 0 auto;
  padding: clamp(20px, 4vw, 52px);
}
.wwc-empty { display: grid; gap: 18px; max-width: 980px; }
.wwc-panel, .wwc-hero, .wwc-metrics {
  border: 1px solid var(--dsw-alias-border-l1, #e3e8f0);
  border-radius: 16px;
  background: var(--dsw-alias-bg-module-platform, #fff);
  box-shadow: var(--dsw-shadow-lv2, 0 10px 30px rgba(24, 33, 51, .05));
}
.wwc-panel { padding: 20px; }
.wwc-open-panel { display: grid; grid-template-columns: 1fr minmax(280px, 460px); gap: 28px; align-items: end; }
.wwc-panel__heading, .wwc-row__title, .wwc-hero__meta, .wwc-hero__actions {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
.wwc-panel__heading { align-items: flex-start; margin-bottom: 18px; }
.wwc-panel h1, .wwc-panel h2, .wwc-panel h3, .wwc-hero h1 { margin: 0; }
.wwc-panel h2 { font-size: 18px; }
.wwc-panel h3 { margin-bottom: 8px; font-size: 13px; }
.wwc-eyebrow {
  margin: 0 0 4px;
  color: var(--dsw-alias-label-tertiary, #738096);
  font-size: 11px;
  font-weight: 750;
  letter-spacing: .09em;
  text-transform: uppercase;
}
.wwc-muted { color: var(--dsw-alias-label-secondary, #657086); font-size: 13px; }
.wwc-code { overflow-wrap: anywhere; font-family: var(--ds-font-family-code, ui-monospace, monospace); font-size: 12px; }
.wwc-inline-form { display: flex; gap: 10px; }
.wwc-inline-form input { flex: 1; min-width: 0; }
.wwc-create-form, .wwc-stack, .wwc-column { display: grid; gap: 14px; }
.wwc-form-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 14px; }
.wwc-form-grid--repository { grid-template-columns: .75fr 1.8fr 1fr .75fr; }
.wwc-field { display: grid; gap: 7px; min-width: 0; }
.wwc-field__label { color: var(--dsw-alias-label-secondary, #657086); font-size: 12px; font-weight: 700; }
.wwc-field__help { color: var(--dsw-alias-label-tertiary, #738096); line-height: 1.45; }
.wwc-workbench input, .wwc-workbench textarea, .wwc-workbench select {
  width: 100%;
  border: 1px solid var(--dsw-alias-border-l2, #d8dee9);
  border-radius: 10px;
  padding: 10px 12px;
  color: inherit;
  background: var(--dsw-alias-bg-base, #fff);
  font: inherit;
  outline: none;
}
.wwc-workbench textarea { min-height: 92px; resize: vertical; line-height: 1.5; }
.wwc-workbench input:focus, .wwc-workbench textarea:focus, .wwc-workbench select:focus {
  border-color: var(--dsw-alias-state-business-primary, #2f6fed);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--dsw-alias-state-business-primary, #2f6fed) 13%, transparent);
}
.wwc-workbench button {
  border: 0;
  border-radius: 10px;
  padding: 10px 15px;
  color: white;
  background: var(--dsw-alias-state-business-primary, #2f6fed);
  font: inherit;
  font-weight: 700;
  cursor: pointer;
}
.wwc-workbench button:hover { filter: brightness(.96); }
.wwc-workbench button:disabled { cursor: wait; opacity: .55; }
.wwc-workbench button:focus-visible,
.wwc-workbench input:focus-visible,
.wwc-workbench textarea:focus-visible,
.wwc-workbench select:focus-visible,
.wwc-workbench pre:focus-visible {
  outline: 3px solid color-mix(in srgb, var(--dsw-alias-state-business-primary, #2f6fed) 34%, transparent);
  outline-offset: 2px;
}
.wwc-form-actions { display: flex; justify-content: flex-end; }
.wwc-error {
  width: min(1336px, calc(100% - 40px));
  margin: 18px auto 0;
  padding: 12px 16px;
  border: 1px solid color-mix(in srgb, var(--dsw-alias-state-error-primary, #cf3f4f) 32%, transparent);
  border-radius: 10px;
  color: var(--dsw-alias-state-error-primary, #b82f3f);
  background: color-mix(in srgb, var(--dsw-alias-state-error-primary, #cf3f4f) 8%, transparent);
}
.wwc-hero { display: flex; justify-content: space-between; gap: 28px; padding: 24px; }
.wwc-hero h1 { margin-top: 10px; font-size: clamp(24px, 4vw, 38px); letter-spacing: -.035em; }
.wwc-hero__goal { max-width: 820px; margin: 10px 0 0; color: var(--dsw-alias-label-secondary, #657086); line-height: 1.6; }
.wwc-hero__actions { display: grid; gap: 8px; align-self: flex-start; flex: 0 0 auto; min-width: 190px; }
.wwc-advance-result { max-width: 300px; margin: 3px 0 0; color: var(--dsw-alias-label-secondary, #657086); font-size: 12px; line-height: 1.45; }
.wwc-button--quiet, .wwc-session-link {
  color: var(--dsw-alias-label-primary, #172033) !important;
  background: var(--dsw-alias-interactive-bg-hover, #f0f3f8) !important;
}
.wwc-status, .wwc-pill {
  display: inline-flex;
  align-items: center;
  width: fit-content;
  border-radius: 999px;
  padding: 4px 9px;
  color: var(--dsw-alias-label-secondary, #657086);
  background: var(--dsw-alias-interactive-bg-hover, #f0f3f8);
  font-size: 11px;
  font-weight: 750;
  white-space: nowrap;
}
.wwc-status--executing, .wwc-status--verifying, .wwc-status--reworking,
.wwc-pill--running, .wwc-pill--active, .wwc-pill--verifying {
  color: var(--dsw-alias-state-business-primary, #2f6fed);
  background: color-mix(in srgb, var(--dsw-alias-state-business-primary, #2f6fed) 12%, transparent);
}
.wwc-status--delivered, .wwc-pill--pass, .wwc-pill--completed, .wwc-pill--succeeded,
.wwc-pill--resolved {
  color: var(--dsw-alias-state-success-primary, #23845d);
  background: color-mix(in srgb, var(--dsw-alias-state-success-primary, #23845d) 12%, transparent);
}
.wwc-status--needs-attention, .wwc-status--plan-review, .wwc-status--ready-to-deliver,
.wwc-pill--blocking, .wwc-pill--waiting, .wwc-pill--inconclusive, .wwc-pill--pending {
  color: var(--dsw-alias-state-warn-primary, #a96713);
  background: color-mix(in srgb, var(--dsw-alias-state-warn-primary, #a96713) 12%, transparent);
}
.wwc-pill--fail, .wwc-pill--infra_error, .wwc-pill--failed, .wwc-pill--cancelled,
.wwc-pill--dismissed {
  color: var(--dsw-alias-state-error-primary, #b82f3f);
  background: color-mix(in srgb, var(--dsw-alias-state-error-primary, #b82f3f) 10%, transparent);
}
.wwc-metrics { display: grid; grid-template-columns: repeat(4, 1fr); margin-top: 16px; overflow: hidden; }
.wwc-metric { display: grid; gap: 4px; padding: 16px 20px; border-right: 1px solid var(--dsw-alias-border-l1, #e3e8f0); }
.wwc-metric:last-child { border-right: 0; }
.wwc-metric span { color: var(--dsw-alias-label-tertiary, #738096); font-size: 12px; }
.wwc-metric strong { font-size: 22px; }
.wwc-columns { display: grid; grid-template-columns: minmax(0, 1.15fr) minmax(360px, .85fr); gap: 16px; margin-top: 16px; align-items: start; }
.wwc-definition-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 1px; margin: 0; overflow: hidden; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 10px; background: var(--dsw-alias-border-l1, #e3e8f0); }
.wwc-definition-grid div { min-width: 0; padding: 11px 12px; background: var(--dsw-alias-bg-base, #fff); }
.wwc-definition-grid dt { color: var(--dsw-alias-label-tertiary, #738096); font-size: 11px; }
.wwc-definition-grid dd { margin: 4px 0 0; overflow-wrap: anywhere; }
.wwc-spec-goal { margin-bottom: 16px; padding: 14px; border-left: 3px solid var(--dsw-alias-state-business-primary, #2f6fed); background: color-mix(in srgb, var(--dsw-alias-state-business-primary, #2f6fed) 5%, transparent); }
.wwc-spec-goal h3, .wwc-spec-goal p { margin: 0; }
.wwc-spec-goal p { margin-top: 6px; line-height: 1.6; }
.wwc-list-groups { display: grid; grid-template-columns: repeat(3, 1fr); gap: 18px; margin-top: 18px; }
.wwc-list-groups ul, .wwc-verdict ul { margin: 0; padding-left: 18px; line-height: 1.6; }
.wwc-row { display: flex; justify-content: space-between; gap: 16px; padding: 14px; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 11px; }
.wwc-row--compact { align-items: center; padding: 11px 13px; }
.wwc-row__body { min-width: 0; flex: 1; }
.wwc-row__body p { margin: 6px 0 0; line-height: 1.5; overflow-wrap: anywhere; }
.wwc-row__title { justify-content: flex-start; flex-wrap: wrap; }
.wwc-explanation { padding-left: 10px; border-left: 2px solid var(--dsw-alias-border-l2, #d8dee9); }
.wwc-empty-row { margin: 0; padding: 16px; border: 1px dashed var(--dsw-alias-border-l2, #d8dee9); border-radius: 10px; color: var(--dsw-alias-label-tertiary, #738096); text-align: center; }
.wwc-timeline { display: grid; gap: 0; margin: 0; padding: 0; list-style: none; }
.wwc-timeline li { position: relative; display: grid; grid-template-columns: 18px 1fr; gap: 12px; min-height: 74px; }
.wwc-timeline li:not(:last-child)::before { content: ''; position: absolute; top: 17px; bottom: -1px; left: 6px; width: 1px; background: var(--dsw-alias-border-l2, #d8dee9); }
.wwc-timeline__dot { z-index: 1; width: 13px; height: 13px; margin-top: 3px; border: 3px solid var(--dsw-alias-bg-module-platform, #fff); border-radius: 50%; background: var(--dsw-alias-label-tertiary, #738096); box-shadow: 0 0 0 1px var(--dsw-alias-border-l2, #d8dee9); }
.wwc-timeline__dot--running, .wwc-timeline__dot--waiting { background: var(--dsw-alias-state-business-primary, #2f6fed); }
.wwc-timeline__dot--succeeded { background: var(--dsw-alias-state-success-primary, #23845d); }
.wwc-timeline__dot--failed, .wwc-timeline__dot--cancelled { background: var(--dsw-alias-state-error-primary, #b82f3f); }
.wwc-timeline__content { min-width: 0; padding-bottom: 18px; }
.wwc-timeline__content p { margin: 5px 0 0; }
.wwc-binding { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-top: 8px; }
.wwc-runtime-session { display: grid; gap: 14px; padding: 14px; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 12px; }
.wwc-runtime-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; }
.wwc-runtime-block { min-width: 0; padding: 12px; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 10px; background: var(--dsw-alias-bg-base, #fff); }
.wwc-runtime-block h3 { margin: 0 0 9px; font-size: 13px; }
.wwc-runtime-block p { margin: 6px 0; overflow-wrap: anywhere; }
.wwc-runtime-list { display: grid; gap: 9px; margin: 0; padding-left: 20px; }
.wwc-runtime-list code { overflow-wrap: anywhere; }
.wwc-runtime-text { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
.wwc-runtime-usage { display: flex; flex-wrap: wrap; gap: 6px 12px; margin-top: 8px; }
.wwc-session-link { padding: 6px 9px !important; font-size: 12px !important; }
.wwc-success-copy { color: var(--dsw-alias-state-success-primary, #23845d); }
.wwc-plan-review { display: grid; gap: 20px; }
.wwc-review-identity { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 1px; margin: 0; overflow: hidden; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 10px; background: var(--dsw-alias-border-l1, #e3e8f0); }
.wwc-review-identity div { min-width: 0; padding: 10px 12px; background: var(--dsw-alias-bg-base, #fff); }
.wwc-review-identity dt { color: var(--dsw-alias-label-tertiary, #738096); font-size: 11px; }
.wwc-review-identity dd { margin: 4px 0 0; overflow-wrap: anywhere; }
.wwc-section-heading h3, .wwc-section-heading p, .wwc-solution h4, .wwc-diagram h4,
.wwc-review-findings h3, .wwc-review-decision h3 { margin: 0; }
.wwc-solution { padding: 17px; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 12px; background: color-mix(in srgb, var(--dsw-alias-state-business-primary, #2f6fed) 3%, var(--dsw-alias-bg-base, #fff)); }
.wwc-solution__summary { margin: 10px 0 18px; line-height: 1.65; }
.wwc-solution__grid { display: grid; grid-template-columns: minmax(0, .8fr) minmax(0, 1.2fr); gap: 20px; }
.wwc-solution__grid ol, .wwc-review-findings ul, .wwc-review-decision ul { margin: 10px 0 0; padding-left: 20px; line-height: 1.65; }
.wwc-component-list { display: grid; gap: 8px; margin: 10px 0 0; padding: 0; list-style: none; }
.wwc-component-list li { padding: 10px 12px; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 9px; background: var(--dsw-alias-bg-module-platform, #fff); }
.wwc-component-list p { margin: 5px 0; line-height: 1.5; }
.wwc-component-list .wwc-code, .wwc-component-list .wwc-muted { display: block; margin-top: 7px; overflow-wrap: anywhere; }
.wwc-diagram-cycle { padding: 11px 13px; border: 1px solid color-mix(in srgb, var(--dsw-alias-state-success-primary, #23845d) 42%, transparent); border-radius: 10px; color: var(--dsw-alias-state-success-primary, #23845d); background: color-mix(in srgb, var(--dsw-alias-state-success-primary, #23845d) 7%, transparent); font-size: 13px; font-weight: 720; }
.wwc-diagram-cycle--executing { border-color: #72b7dc; color: #126891; background: color-mix(in srgb, #8ed0ef 22%, transparent); }
.wwc-diagram-cycle--execution-finished { border-color: #d6a92a; color: #795800; background: color-mix(in srgb, #f5cf5c 25%, transparent); }
.wwc-diagram-grid { display: grid; gap: 16px; }
.wwc-diagram { min-width: 0; margin: 0; padding: 16px; overflow: hidden; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 12px; background: var(--dsw-alias-bg-base, #fff); }
.wwc-diagram__caption { display: grid; gap: 3px; margin-bottom: 14px; }
.wwc-diagram__nodes { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px; margin: 0; padding: 0; list-style: none; }
.wwc-diagram-node { min-width: 0; padding: 12px; border: 1px solid color-mix(in srgb, var(--dsw-alias-state-success-primary, #23845d) 45%, transparent); border-left: 4px solid var(--dsw-alias-state-success-primary, #23845d); border-radius: 10px; background: color-mix(in srgb, var(--dsw-alias-state-success-primary, #23845d) 7%, var(--dsw-alias-bg-base, #fff)); }
.wwc-diagram-node[data-execution-state='affected-live'] { border-color: #72b7dc; border-left-color: #58a9d5; background: color-mix(in srgb, #8ed0ef 24%, var(--dsw-alias-bg-base, #fff)); }
.wwc-diagram-node[data-execution-state='affected-finished'] { border-color: #d6a92a; border-left-color: #c4940d; background: color-mix(in srgb, #f5cf5c 28%, var(--dsw-alias-bg-base, #fff)); }
.wwc-diagram-node[data-definition-state='unresolved'] { border-style: dashed; }
.wwc-diagram-node__heading { display: flex; align-items: flex-start; justify-content: space-between; gap: 8px; }
.wwc-diagram-node p { margin: 7px 0 0; overflow-wrap: anywhere; line-height: 1.5; }
.wwc-diagram-node__boundary { color: var(--dsw-alias-label-tertiary, #738096); font-size: 12px; }
.wwc-diagram-node__state { display: inline-flex; gap: 5px; align-items: center; margin-top: 9px; font-size: 12px; font-weight: 760; }
.wwc-diagram-node__state--normal { color: var(--dsw-alias-state-success-primary, #23845d); }
.wwc-diagram-node__state--affected-live { color: #126891; }
.wwc-diagram-node__state--affected-finished { color: #795800; }
.wwc-diagram-node__review { width: 100%; margin-top: 11px; color: #493400 !important; background: color-mix(in srgb, #f5cf5c 42%, white) !important; }
.wwc-diagram-node__review[aria-pressed='true'] { box-shadow: 0 0 0 2px #8d6a08; }
.wwc-diagram__relations { margin-top: 14px; padding-top: 14px; border-top: 1px solid var(--dsw-alias-border-l1, #e3e8f0); }
.wwc-diagram__relations ol { display: grid; gap: 7px; margin: 10px 0 0; padding: 0; list-style: none; }
.wwc-diagram__relations li { display: grid; grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr); gap: 5px 8px; align-items: center; padding: 8px 10px; border-radius: 8px; background: var(--dsw-alias-interactive-bg-hover, #f0f3f8); font-size: 12px; }
.wwc-diagram__relations li strong { grid-column: 1 / -1; color: var(--dsw-alias-label-secondary, #657086); font-weight: 600; }
.wwc-diagram__arrow { color: var(--dsw-alias-state-business-primary, #2f6fed); font-size: 16px; }
.wwc-diff-review { display: grid; gap: 15px; padding: 17px; border: 1px solid #d6a92a; border-radius: 12px; background: color-mix(in srgb, #f5cf5c 8%, var(--dsw-alias-bg-base, #fff)); }
.wwc-diff-review__selected, .wwc-diff-file__heading, .wwc-diff-hunk__heading { display: flex; flex-wrap: wrap; justify-content: space-between; gap: 8px 14px; align-items: center; }
.wwc-diff-file { display: grid; gap: 12px; min-width: 0; padding: 14px; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 10px; background: var(--dsw-alias-bg-module-platform, #fff); }
.wwc-diff-hunk { display: grid; gap: 10px; min-width: 0; padding-top: 12px; border-top: 1px solid var(--dsw-alias-border-l1, #e3e8f0); }
.wwc-diff-hunk__heading .wwc-code { max-width: 100%; overflow-wrap: anywhere; }
.wwc-diff-hunk pre { max-height: 440px; margin: 0; padding: 13px; overflow: auto; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 8px; background: #111827; color: #e5edf7; font: 12px/1.55 ui-monospace, SFMono-Regular, Menlo, monospace; white-space: pre; }
.wwc-diff-provenance { min-width: 0; padding: 14px; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 10px; background: var(--dsw-alias-bg-module-platform, #fff); }
.wwc-diff-provenance h4, .wwc-diff-provenance p { margin: 0; }
.wwc-diff-provenance p, .wwc-diff-provenance ul { margin-top: 9px; overflow-wrap: anywhere; line-height: 1.55; }
.wwc-remediation-form { display: grid; gap: 12px; padding: 14px; border: 1px solid #d6a92a; border-radius: 10px; background: var(--dsw-alias-bg-module-platform, #fff); }
.wwc-review-findings { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 14px; }
.wwc-review-findings section { min-width: 0; padding: 14px; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 10px; }
.wwc-review-session-notice, .wwc-review-decision { padding: 14px; border: 1px solid color-mix(in srgb, var(--dsw-alias-state-warn-primary, #a96713) 32%, transparent); border-radius: 10px; background: color-mix(in srgb, var(--dsw-alias-state-warn-primary, #a96713) 7%, transparent); }
.wwc-review-session-notice p, .wwc-review-decision p { margin: 6px 0 10px; line-height: 1.55; }
.wwc-review-form fieldset { display: grid; gap: 14px; min-width: 0; margin: 0; padding: 16px; border: 1px solid var(--dsw-alias-border-l1, #e3e8f0); border-radius: 11px; }
.wwc-review-form legend { padding: 0 7px; font-weight: 750; }
.wwc-review-actions { display: flex; flex-wrap: wrap; gap: 9px; justify-content: flex-end; }
.wwc-button--warning { color: #6f4200 !important; background: color-mix(in srgb, var(--dsw-alias-state-warn-primary, #a96713) 20%, white) !important; }
.wwc-button--danger { background: var(--dsw-alias-state-error-primary, #b82f3f) !important; }
.wwc-review-result { min-height: 1.4em; margin: 8px 0 0; color: var(--dsw-alias-label-secondary, #657086); }
@media (max-width: 1280px) {
  .wwc-columns { grid-template-columns: 1fr; }
}
@media (max-width: 980px) {
  .wwc-open-panel { grid-template-columns: 1fr; }
  .wwc-form-grid--repository { grid-template-columns: repeat(2, minmax(0, 1fr)); }
  .wwc-list-groups { grid-template-columns: 1fr; }
  .wwc-solution__grid { grid-template-columns: 1fr; }
  .wwc-runtime-grid { grid-template-columns: 1fr; }
}
@media (max-width: 680px) {
  .wwc-workbench__bar { align-items: flex-start; flex-direction: column; }
  .wwc-empty, .wwc-delivery { padding: 16px; }
  .wwc-form-grid, .wwc-form-grid--repository { grid-template-columns: 1fr; }
  .wwc-hero { flex-direction: column; }
  .wwc-hero__actions { width: 100%; }
  .wwc-hero__actions button { flex: 1; }
  .wwc-metrics { grid-template-columns: repeat(2, 1fr); }
  .wwc-metric:nth-child(2) { border-right: 0; }
  .wwc-metric:nth-child(-n + 2) { border-bottom: 1px solid var(--dsw-alias-border-l1, #e3e8f0); }
  .wwc-definition-grid { grid-template-columns: 1fr; }
  .wwc-review-identity, .wwc-review-findings, .wwc-diagram__nodes { grid-template-columns: 1fr; }
  .wwc-review-actions { align-items: stretch; flex-direction: column; }
}
`

function installStrongFlowStyles(): () => void {
  if (typeof document === 'undefined' || document.getElementById(STYLE_ELEMENT_ID) !== null) {
    return () => undefined
  }
  const element = document.createElement('style')
  element.id = STYLE_ELEMENT_ID
  element.textContent = STRONGFLOW_STYLES
  document.head.append(element)
  return () => { element.remove() }
}

/** Client services required before the advanced conversation view is mounted. */
export const inject = ['slots', 'remote', 'sessions'] as const

/** Add one opt-in Delivery tab while DSH's built-in Chat tab remains the default. */
export async function apply(ctx: Context): Promise<() => Promise<void>> {
  const disposeRemote = await mountStrongFlowDeliveryRemote(ctx)
  const disposeStyles = installStrongFlowStyles()
  const remoteScope = ctx.inject(['remote.strongflow'], (scope) => {
    const product = scope as unknown as StrongFlowClientContext
    return product.slots.inject('conversation.view', () => product.slots.register({
      name: 'conversation.view',
      id: 'strongflow',
      order: 100,
      label: () => 'StrongFlow',
      inject: sessionId => {
        const sessionScope = product.sessions.scope(sessionId)
        if (sessionScope === undefined) {
          throw new Error(`StrongFlow cannot resolve DSH Session ${sessionId}`)
        }
        const remoteNamespace = sessionScope.get('remote.strongflow') as (
          StrongFlowScopedRemote['strongflow'] | undefined
        )
        if (remoteNamespace === undefined) {
          throw new Error(`StrongFlow Remote is unavailable for DSH Session ${sessionId}`)
        }
        return {
          defaultRepository: product.sessions.list.getSnapshot().byId[sessionId]?.cwd ?? '',
          invokeAdvance: (request, signal) => (
            invokeAdvanceRemote({ strongflow: remoteNamespace }, request, signal)
          ),
          invokeDelivery: (request, signal) => (
            invokeDeliveryRemote({ strongflow: remoteNamespace }, request, signal)
          ),
          openSession: sessionIdToOpen => { product.sessions.open(sessionIdToOpen) },
        }
      },
    }, StrongFlowView))
  })
  try {
    await remoteScope.await()
  } catch (error) {
    await remoteScope.dispose()
    disposeStyles()
    await disposeRemote()
    throw error
  }
  return async () => {
    await remoteScope.dispose()
    disposeStyles()
    await disposeRemote()
  }
}
