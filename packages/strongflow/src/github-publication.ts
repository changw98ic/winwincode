import { createHash } from 'node:crypto'

import {
  DELIVERY_SCHEMA_VERSION,
  STRONGFLOW_GITHUB_PUBLICATION_CONTEXT_PROTOCOL,
  STRONGFLOW_GITHUB_PUBLICATION_DECISION_PROTOCOL,
  STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION,
  AttentionItemId,
  StageRunId,
  parseAttentionItem,
  parseDelivery,
  parseStrongFlowGitHubPublicationContext,
  parseStrongFlowGitHubPublicationContextText,
  parseStrongFlowGitHubPublicationDecision,
  parseStrongFlowGitHubPublicationDecisionText,
  serializeStrongFlowGitHubPublicationDecision,
  type AttentionItem,
  type AttentionItemStatus,
  type Delivery,
  type DeliveryVerdict,
  type FrozenDeliveryCandidate,
  type SessionBinding,
  type StageRun,
  type StrongFlowGitHubPublicationContext,
  type StrongFlowGitHubPublicationDecision,
} from '@winwincode/contracts'

import {
  DeliveryCandidateEvidenceError,
  assertFrozenDeliveryCandidateCurrent,
} from './candidate-evidence.js'

export type StrongFlowGitHubPublicationErrorCode =
  | 'INVALID_PUBLICATION_INPUT'
  | 'STALE_PUBLICATION_SET'
  | 'INVALID_PUBLICATION_DECISION'

export class StrongFlowGitHubPublicationError extends Error {
  readonly code: StrongFlowGitHubPublicationErrorCode

  constructor(
    code: StrongFlowGitHubPublicationErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowGitHubPublicationError'
    this.code = code
  }
}

export interface CreateStrongFlowGitHubPublicationAttentionInput {
  readonly delivery: Delivery
  readonly candidate: FrozenDeliveryCandidate
  readonly attentionItemId: string
  readonly reviewStageRunId: string
  readonly assignedTo: string | null
  readonly preparedAtMillis: number
}

export interface CreateStrongFlowGitHubPublicationDecisionInput {
  readonly context: StrongFlowGitHubPublicationContext
  readonly comments: string
}

export interface ValidatedStrongFlowGitHubPublicationDecision {
  readonly decision: StrongFlowGitHubPublicationDecision
  readonly storedResolution: string
}

/** Rebuildable publication authorization; no provider call has occurred. */
export interface CurrentStrongFlowGitHubPublication {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION
  readonly context: StrongFlowGitHubPublicationContext
  readonly decision: StrongFlowGitHubPublicationDecision
  readonly candidate: FrozenDeliveryCandidate
  readonly verdict: DeliveryVerdict
  readonly approvedBy: string
  readonly approvedAtMillis: number
}

export interface CurrentStrongFlowGitHubPublicationReview {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION
  readonly context: StrongFlowGitHubPublicationContext
  readonly decision: StrongFlowGitHubPublicationDecision | null
  readonly attention: AttentionItem
  readonly reviewStageRun: StageRun
  readonly reviewSessionBinding: SessionBinding
  readonly candidate: FrozenDeliveryCandidate
  readonly verdict: DeliveryVerdict
}

export const STRONGFLOW_GITHUB_PUBLICATION_OPTIONS = Object.freeze([
  Object.freeze({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'approve-publication',
    label: '批准发布 Pull Request',
    description: '批准当前交付定义、冻结候选、验收结论和唯一 GitHub 目标。',
  }),
  Object.freeze({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'annotate-rework',
    label: '标注后返工',
    description: '在执行结束图上绑定具体变更标注，然后重新验证新候选。',
  }),
])

function publicationError(
  code: StrongFlowGitHubPublicationErrorCode,
  message: string,
  cause?: unknown,
): never {
  throw new StrongFlowGitHubPublicationError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function digest(value: unknown): string {
  return createHash('sha256').update(JSON.stringify(value)).digest('hex')
}

function equal(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right)
}

function parsedDelivery(value: Delivery): Delivery {
  try {
    return parseDelivery(value)
  } catch (error) {
    return publicationError(
      'INVALID_PUBLICATION_INPUT',
      'GitHub publication requires a valid Delivery',
      error,
    )
  }
}

function currentCandidate(
  delivery: Delivery,
  value: FrozenDeliveryCandidate,
): FrozenDeliveryCandidate {
  try {
    return assertFrozenDeliveryCandidateCurrent(delivery, value)
  } catch (error) {
    return publicationError(
      error instanceof DeliveryCandidateEvidenceError
        ? 'STALE_PUBLICATION_SET'
        : 'INVALID_PUBLICATION_INPUT',
      'GitHub publication does not identify the current frozen candidate',
      error,
    )
  }
}

function passingVerdict(delivery: Delivery): DeliveryVerdict {
  if (delivery.verdict?.status !== 'pass') {
    return publicationError(
      'INVALID_PUBLICATION_INPUT',
      'GitHub publication requires the current passing DeliveryVerdict',
    )
  }
  return delivery.verdict
}

function githubBinding(delivery: Delivery): {
  readonly sourceRef: NonNullable<Delivery['spec']['sourceRef']>
  readonly publicationTarget: NonNullable<Delivery['spec']['publicationTarget']>
} {
  const sourceRef = delivery.spec.sourceRef
  const publicationTarget = delivery.spec.publicationTarget
  if (sourceRef === null || publicationTarget === null) {
    return publicationError(
      'INVALID_PUBLICATION_INPUT',
      'GitHub publication requires one source issue and one pull-request target',
    )
  }
  return Object.freeze({ sourceRef, publicationTarget })
}

function providerIdempotencyKey(delivery: Delivery): string {
  const binding = githubBinding(delivery)
  return `github:pull-request:sha256:${digest({
    deliveryId: delivery.id,
    sourceRef: binding.sourceRef,
    publicationTarget: binding.publicationTarget,
  })}`
}

function contextWithoutDigest(
  value: Omit<StrongFlowGitHubPublicationContext, 'publicationSetSha256'>,
): Omit<StrongFlowGitHubPublicationContext, 'publicationSetSha256'> {
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_CONTEXT_PROTOCOL,
    deliveryId: value.deliveryId,
    deliverySpecId: value.deliverySpecId,
    deliverySpecRevision: value.deliverySpecRevision,
    sourceRef: value.sourceRef,
    publicationTarget: value.publicationTarget,
    candidateRef: value.candidateRef,
    deliveryVerdictId: value.deliveryVerdictId,
    reviewStageRunId: value.reviewStageRunId,
    attentionItemId: value.attentionItemId,
    providerIdempotencyKey: value.providerIdempotencyKey,
    preparedAtMillis: value.preparedAtMillis,
  })
}

function publicationSetDigest(
  value: Omit<StrongFlowGitHubPublicationContext, 'publicationSetSha256'>,
): string {
  return digest(contextWithoutDigest(value))
}

function publicationContextFromItem(item: AttentionItem): StrongFlowGitHubPublicationContext {
  try {
    return parseStrongFlowGitHubPublicationContextText(item.context)
  } catch (error) {
    return publicationError(
      'INVALID_PUBLICATION_INPUT',
      'delivery approval does not contain a structured GitHub publication set',
      error,
    )
  }
}

function assertPublicationSetCurrent(
  delivery: Delivery,
  item: AttentionItem,
  context: StrongFlowGitHubPublicationContext,
): void {
  const binding = githubBinding(delivery)
  const verdict = passingVerdict(delivery)
  const unsigned = contextWithoutDigest({
    schemaVersion: context.schemaVersion,
    protocol: context.protocol,
    deliveryId: context.deliveryId,
    deliverySpecId: context.deliverySpecId,
    deliverySpecRevision: context.deliverySpecRevision,
    sourceRef: context.sourceRef,
    publicationTarget: context.publicationTarget,
    candidateRef: context.candidateRef,
    deliveryVerdictId: context.deliveryVerdictId,
    reviewStageRunId: context.reviewStageRunId,
    attentionItemId: context.attentionItemId,
    providerIdempotencyKey: context.providerIdempotencyKey,
    preparedAtMillis: context.preparedAtMillis,
  })
  if (item.deliveryId !== delivery.id
    || item.deliverySpecId !== delivery.spec.id
    || item.type !== 'delivery_approval'
    || item.stageRunId === null
    || context.deliveryId !== delivery.id
    || context.deliverySpecId !== delivery.spec.id
    || context.deliverySpecRevision !== delivery.spec.revision
    || context.reviewStageRunId !== item.stageRunId
    || context.attentionItemId !== item.id
    || !equal(context.sourceRef, binding.sourceRef)
    || !equal(context.publicationTarget, binding.publicationTarget)
    || context.candidateRef !== verdict.candidateRef
    || context.deliveryVerdictId !== verdict.id
    || context.preparedAtMillis < verdict.producedAtMillis
    || context.providerIdempotencyKey !== providerIdempotencyKey(delivery)
    || context.publicationSetSha256 !== publicationSetDigest(unsigned)) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'GitHub publication set does not match the current spec, candidate, verdict, or destination',
    )
  }
}

function priorProviderKeys(delivery: Delivery): readonly string[] {
  const keys: string[] = []
  for (const item of delivery.attentionItems) {
    let value: unknown
    try {
      value = JSON.parse(item.context) as unknown
    } catch {
      continue
    }
    if (typeof value !== 'object'
      || value === null
      || !('protocol' in value)
      || value.protocol !== STRONGFLOW_GITHUB_PUBLICATION_CONTEXT_PROTOCOL) continue
    try {
      keys.push(parseStrongFlowGitHubPublicationContext(value).providerIdempotencyKey)
    } catch (error) {
      return publicationError(
        'STALE_PUBLICATION_SET',
        'Delivery contains a malformed GitHub publication identity',
        error,
      )
    }
  }
  return Object.freeze(keys)
}

/** Freeze one exact source → Delivery → candidate → PR target → verdict review set. */
export function createStrongFlowGitHubPublicationAttention(
  input: CreateStrongFlowGitHubPublicationAttentionInput,
): AttentionItem {
  const delivery = parsedDelivery(input.delivery)
  if (delivery.status !== 'ready-to-deliver') {
    return publicationError(
      'INVALID_PUBLICATION_INPUT',
      'GitHub publication review starts only after verification passes',
    )
  }
  const binding = githubBinding(delivery)
  const verdict = passingVerdict(delivery)
  const candidate = currentCandidate(delivery, input.candidate)
  if (candidate.candidateRef !== verdict.candidateRef) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'GitHub publication candidate does not match the current DeliveryVerdict',
    )
  }
  if (!Number.isSafeInteger(input.preparedAtMillis)
    || input.preparedAtMillis < delivery.updatedAtMillis
    || input.preparedAtMillis < verdict.producedAtMillis
    || Object.is(input.preparedAtMillis, -0)) {
    return publicationError(
      'INVALID_PUBLICATION_INPUT',
      'GitHub publication preparation time must follow the current verdict',
    )
  }
  const attentionItemId = AttentionItemId(input.attentionItemId)
  const reviewStageRunId = StageRunId(input.reviewStageRunId)
  if (delivery.attentionItems.some(item => item.id === attentionItemId)
    || delivery.stageRuns.some(run => run.id === reviewStageRunId)) {
    return publicationError(
      'INVALID_PUBLICATION_INPUT',
      'GitHub publication Attention or StageRun identity already exists',
    )
  }
  const key = providerIdempotencyKey(delivery)
  if (priorProviderKeys(delivery).some(prior => prior !== key)) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'Delivery already identifies another intended GitHub pull request',
    )
  }
  const unsigned = contextWithoutDigest({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_CONTEXT_PROTOCOL,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    deliverySpecRevision: delivery.spec.revision,
    sourceRef: binding.sourceRef,
    publicationTarget: binding.publicationTarget,
    candidateRef: candidate.candidateRef,
    deliveryVerdictId: verdict.id,
    reviewStageRunId,
    attentionItemId,
    providerIdempotencyKey: key,
    preparedAtMillis: input.preparedAtMillis,
  })
  const context = parseStrongFlowGitHubPublicationContext({
    ...unsigned,
    publicationSetSha256: publicationSetDigest(unsigned),
  })
  return parseAttentionItem({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: attentionItemId,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    stageRunId: reviewStageRunId,
    type: 'delivery_approval',
    title: '审核当前候选并批准 GitHub Pull Request',
    context: JSON.stringify(context),
    options: STRONGFLOW_GITHUB_PUBLICATION_OPTIONS,
    assignedTo: input.assignedTo,
    blocking: true,
    status: 'open',
    resolution: null,
    resolvedBy: null,
    createdAtMillis: input.preparedAtMillis,
    resolvedAtMillis: null,
  })
}

/** Revalidate a caller-provided delivery-review Attention before storing it. */
export function validateStrongFlowGitHubPublicationAttention(
  deliveryValue: Delivery,
  reviewRun: StageRun,
  item: AttentionItem,
  now: number,
): StrongFlowGitHubPublicationContext {
  const delivery = parsedDelivery(deliveryValue)
  const context = publicationContextFromItem(item)
  assertPublicationSetCurrent(delivery, item, context)
  if (delivery.status !== 'ready-to-deliver'
    || reviewRun.stage !== 'delivery-review'
    || reviewRun.actorType !== 'human'
    || reviewRun.status !== 'waiting'
    || reviewRun.id !== context.reviewStageRunId
    || item.status !== 'open'
    || !item.blocking
    || item.createdAtMillis !== context.preparedAtMillis
    || context.preparedAtMillis > now
    || !equal(item.options, STRONGFLOW_GITHUB_PUBLICATION_OPTIONS)) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'GitHub publication Attention does not match the current review stage or approval set',
    )
  }
  const keys = [...priorProviderKeys(delivery), context.providerIdempotencyKey]
  if (new Set(keys).size !== 1) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'Delivery identifies more than one intended GitHub pull request',
    )
  }
  return context
}

export function createStrongFlowGitHubPublicationDecision(
  input: CreateStrongFlowGitHubPublicationDecisionInput,
): StrongFlowGitHubPublicationDecision {
  const context = parseStrongFlowGitHubPublicationContext(input.context)
  return parseStrongFlowGitHubPublicationDecision({
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
    comments: input.comments,
  })
}

/** Require an approval tied to the exact current publication set. */
export function validateStrongFlowGitHubPublicationDecision(
  deliveryValue: Delivery,
  item: AttentionItem,
  status: Exclude<AttentionItemStatus, 'open'>,
  resolution: string,
): ValidatedStrongFlowGitHubPublicationDecision {
  const delivery = parsedDelivery(deliveryValue)
  const context = publicationContextFromItem(item)
  assertPublicationSetCurrent(delivery, item, context)
  let decision: StrongFlowGitHubPublicationDecision
  try {
    decision = parseStrongFlowGitHubPublicationDecisionText(resolution)
  } catch (error) {
    return publicationError(
      'INVALID_PUBLICATION_DECISION',
      'GitHub publication approval must use the structured decision protocol',
      error,
    )
  }
  if (item.status !== 'open'
    || status !== 'resolved'
    || decision.deliveryId !== context.deliveryId
    || decision.deliverySpecId !== context.deliverySpecId
    || decision.deliverySpecRevision !== context.deliverySpecRevision
    || decision.candidateRef !== context.candidateRef
    || decision.deliveryVerdictId !== context.deliveryVerdictId
    || decision.reviewStageRunId !== context.reviewStageRunId
    || decision.attentionItemId !== context.attentionItemId
    || decision.providerIdempotencyKey !== context.providerIdempotencyKey
    || decision.publicationSetSha256 !== context.publicationSetSha256) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'GitHub publication decision references a stale or different publication set',
    )
  }
  return Object.freeze({
    decision,
    storedResolution: serializeStrongFlowGitHubPublicationDecision(decision),
  })
}

/**
 * Fail closed before a provider adapter is allowed to publish. The returned
 * value is derived from current Delivery, candidate, Attention, and verdict facts.
 */
export function assertStrongFlowGitHubPublicationCurrent(
  deliveryValue: Delivery,
  candidateValue: FrozenDeliveryCandidate,
  itemValue: AttentionItem,
): CurrentStrongFlowGitHubPublication {
  const delivery = parsedDelivery(deliveryValue)
  const suppliedItem = parseAttentionItem(itemValue)
  const item = delivery.attentionItems.find(entry => entry.id === suppliedItem.id)
  if (item === undefined || !equal(item, suppliedItem)) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'GitHub publication approval is not the canonical Delivery Attention',
    )
  }
  const context = publicationContextFromItem(item)
  assertPublicationSetCurrent(delivery, item, context)
  const candidate = currentCandidate(delivery, candidateValue)
  const verdict = passingVerdict(delivery)
  const reviewRun = item.stageRunId === null
    ? undefined
    : delivery.stageRuns.find(run => run.id === item.stageRunId)
  if (delivery.status !== 'delivered'
    || item.status !== 'resolved'
    || item.resolution === null
    || item.resolvedBy === null
    || item.resolvedAtMillis === null
    || reviewRun?.stage !== 'delivery-review'
    || reviewRun.actorType !== 'human'
    || reviewRun.status !== 'succeeded'
    || candidate.candidateRef !== context.candidateRef
    || verdict.id !== context.deliveryVerdictId) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'GitHub publication approval is not current and complete',
    )
  }
  let validated: ValidatedStrongFlowGitHubPublicationDecision
  try {
    const openItem = parseAttentionItem({
      ...item,
      status: 'open',
      resolution: null,
      resolvedBy: null,
      resolvedAtMillis: null,
    })
    validated = validateStrongFlowGitHubPublicationDecision(
      delivery,
      openItem,
      'resolved',
      item.resolution,
    )
  } catch (error) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'GitHub publication approval cannot be rebuilt from current facts',
      error,
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION,
    context,
    decision: validated.decision,
    candidate,
    verdict,
    approvedBy: item.resolvedBy,
    approvedAtMillis: item.resolvedAtMillis,
  })
}

/** Resolve the current open or approved publication review without performing a provider call. */
export function assertStrongFlowGitHubPublicationReviewCurrent(
  deliveryValue: Delivery,
  candidateValue: FrozenDeliveryCandidate,
  attentionItemId?: string,
): CurrentStrongFlowGitHubPublicationReview {
  const delivery = parsedDelivery(deliveryValue)
  const candidate = currentCandidate(delivery, candidateValue)
  const verdict = passingVerdict(delivery)
  const matches = delivery.attentionItems.flatMap((attention) => {
    if (attentionItemId !== undefined && attention.id !== attentionItemId) return []
    try {
      const context = parseStrongFlowGitHubPublicationContextText(attention.context)
      return context.deliverySpecId === delivery.spec.id
        && context.deliverySpecRevision === delivery.spec.revision
        && context.candidateRef === candidate.candidateRef
        && context.deliveryVerdictId === verdict.id
        ? [{ attention, context }]
        : []
    } catch {
      return []
    }
  })
  if (matches.length !== 1) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'Delivery must contain one current GitHub publication review set',
    )
  }
  const { attention, context } = matches[0]!
  assertPublicationSetCurrent(delivery, attention, context)
  const reviewStageRun = attention.stageRunId === null
    ? undefined
    : delivery.stageRuns.find(run => run.id === attention.stageRunId)
  const reviewBindings = reviewStageRun === undefined
    ? []
    : delivery.sessionBindings.filter(binding => (
      binding.stageRunId === reviewStageRun.id
      && binding.dshSessionId !== null
      && binding.codexSessionId === null
    ))
  if (reviewStageRun?.stage !== 'delivery-review'
    || reviewStageRun.actorType !== 'human'
    || reviewStageRun.role !== 'approver'
    || reviewBindings.length !== 1
    || !attention.blocking
    || attention.createdAtMillis !== context.preparedAtMillis
    || !equal(attention.options, STRONGFLOW_GITHUB_PUBLICATION_OPTIONS)) {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'GitHub publication review lacks its exact human StageRun and DSH Session',
    )
  }
  let decision: StrongFlowGitHubPublicationDecision | null = null
  if (attention.status === 'open') {
    if (delivery.status !== 'needs-attention'
      || reviewStageRun.status !== 'waiting'
      || attention.resolution !== null) {
      return publicationError(
        'STALE_PUBLICATION_SET',
        'open GitHub publication review does not match the current waiting stage',
      )
    }
  } else if (attention.status === 'resolved') {
    const approved = assertStrongFlowGitHubPublicationCurrent(delivery, candidate, attention)
    decision = approved.decision
  } else {
    return publicationError(
      'STALE_PUBLICATION_SET',
      'dismissed GitHub publication review cannot authorize a review package',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION,
    context,
    decision,
    attention,
    reviewStageRun,
    reviewSessionBinding: reviewBindings[0]!,
    candidate,
    verdict,
  })
}
