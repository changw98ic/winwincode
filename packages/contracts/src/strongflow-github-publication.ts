import {
  AttentionItemId,
  DeliveryId,
  DeliverySpecId,
  DeliveryVerdictId,
  StageRunId,
  parseGitHubIssueSourceRef,
  parseGitHubPullRequestTargetRef,
  type AttentionItemId as AttentionItemIdentifier,
  type DeliveryId as DeliveryIdentifier,
  type DeliverySpecId as DeliverySpecIdentifier,
  type DeliveryVerdictId as DeliveryVerdictIdentifier,
  type GitHubIssueSourceRef,
  type GitHubPullRequestTargetRef,
  type StageRunId as StageRunIdentifier,
} from './delivery.js'

/**
 * Frozen publication data stored in one delivery_approval AttentionItem.
 * These values are protocol fragments, not additional Delivery objects.
 */
export const STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION = 1 as const

export const STRONGFLOW_GITHUB_PUBLICATION_CONTEXT_PROTOCOL =
  'winwincode.github-publication-context.v1' as const

export const STRONGFLOW_GITHUB_PUBLICATION_DECISION_PROTOCOL =
  'winwincode.github-publication-decision.v1' as const

export const STRONGFLOW_GITHUB_PUBLICATION_ACTIONS = Object.freeze([
  'approve-publication',
] as const)

export type StrongFlowGitHubPublicationAction =
  typeof STRONGFLOW_GITHUB_PUBLICATION_ACTIONS[number]

export interface StrongFlowGitHubPublicationContext {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_GITHUB_PUBLICATION_CONTEXT_PROTOCOL
  readonly deliveryId: DeliveryIdentifier
  readonly deliverySpecId: DeliverySpecIdentifier
  readonly deliverySpecRevision: number
  readonly sourceRef: GitHubIssueSourceRef
  readonly publicationTarget: GitHubPullRequestTargetRef
  readonly candidateRef: string
  readonly deliveryVerdictId: DeliveryVerdictIdentifier
  readonly reviewStageRunId: StageRunIdentifier
  readonly attentionItemId: AttentionItemIdentifier
  readonly providerIdempotencyKey: string
  readonly publicationSetSha256: string
  readonly preparedAtMillis: number
}

/** Exact human authorization serialized into AttentionItem.resolution. */
export interface StrongFlowGitHubPublicationDecision {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_GITHUB_PUBLICATION_DECISION_PROTOCOL
  readonly action: StrongFlowGitHubPublicationAction
  readonly deliveryId: DeliveryIdentifier
  readonly deliverySpecId: DeliverySpecIdentifier
  readonly deliverySpecRevision: number
  readonly candidateRef: string
  readonly deliveryVerdictId: DeliveryVerdictIdentifier
  readonly reviewStageRunId: StageRunIdentifier
  readonly attentionItemId: AttentionItemIdentifier
  readonly providerIdempotencyKey: string
  readonly publicationSetSha256: string
  readonly comments: string
}

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u
const CANDIDATE_REF_PATTERN = /^git-candidate:sha256:[a-f0-9]{64}$/u
const PROVIDER_IDEMPOTENCY_KEY_PATTERN = /^github:pull-request:sha256:[a-f0-9]{64}$/u
const SHA256_PATTERN = /^[a-f0-9]{64}$/u
const MAX_TEXT_LENGTH = 65_536

function failure(path: string, message: string): never {
  throw new TypeError(`${path} ${message}`)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) return failure(path, 'must be an object')
  return value
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  keys: readonly string[],
  path: string,
): void {
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    failure(path, 'has an unexpected shape')
  }
}

function portableId(value: unknown, path: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    return failure(path, 'must be a portable identifier')
  }
  return value
}

function positiveInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1 || Object.is(value, -0)) {
    return failure(path, 'must be a positive safe integer')
  }
  return Number(value)
}

function nonNegativeInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    return failure(path, 'must be a non-negative safe integer')
  }
  return Number(value)
}

function pattern(value: unknown, expected: RegExp, path: string, label: string): string {
  if (typeof value !== 'string' || !expected.test(value)) {
    return failure(path, `must be ${label}`)
  }
  return value
}

function text(value: unknown, path: string): string {
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > MAX_TEXT_LENGTH
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    return failure(path, 'must be non-empty bounded text')
  }
  return value
}

export function parseStrongFlowGitHubPublicationContext(
  value: unknown,
  path = 'githubPublication.context',
): StrongFlowGitHubPublicationContext {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'sourceRef',
    'publicationTarget',
    'candidateRef',
    'deliveryVerdictId',
    'reviewStageRunId',
    'attentionItemId',
    'providerIdempotencyKey',
    'publicationSetSha256',
    'preparedAtMillis',
  ], path)
  if (input.schemaVersion !== STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION) {
    failure(`${path}.schemaVersion`, 'is unsupported')
  }
  if (input.protocol !== STRONGFLOW_GITHUB_PUBLICATION_CONTEXT_PROTOCOL) {
    failure(`${path}.protocol`, 'is unsupported')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_CONTEXT_PROTOCOL,
    deliveryId: DeliveryId(portableId(input.deliveryId, `${path}.deliveryId`)),
    deliverySpecId: DeliverySpecId(
      portableId(input.deliverySpecId, `${path}.deliverySpecId`),
    ),
    deliverySpecRevision: positiveInteger(
      input.deliverySpecRevision,
      `${path}.deliverySpecRevision`,
    ),
    sourceRef: parseGitHubIssueSourceRef(input.sourceRef, `${path}.sourceRef`),
    publicationTarget: parseGitHubPullRequestTargetRef(
      input.publicationTarget,
      `${path}.publicationTarget`,
    ),
    candidateRef: pattern(
      input.candidateRef,
      CANDIDATE_REF_PATTERN,
      `${path}.candidateRef`,
      'a frozen candidate reference',
    ),
    deliveryVerdictId: DeliveryVerdictId(
      portableId(input.deliveryVerdictId, `${path}.deliveryVerdictId`),
    ),
    reviewStageRunId: StageRunId(
      portableId(input.reviewStageRunId, `${path}.reviewStageRunId`),
    ),
    attentionItemId: AttentionItemId(
      portableId(input.attentionItemId, `${path}.attentionItemId`),
    ),
    providerIdempotencyKey: pattern(
      input.providerIdempotencyKey,
      PROVIDER_IDEMPOTENCY_KEY_PATTERN,
      `${path}.providerIdempotencyKey`,
      'a GitHub pull-request idempotency key',
    ),
    publicationSetSha256: pattern(
      input.publicationSetSha256,
      SHA256_PATTERN,
      `${path}.publicationSetSha256`,
      'a lowercase SHA-256 digest',
    ),
    preparedAtMillis: nonNegativeInteger(
      input.preparedAtMillis,
      `${path}.preparedAtMillis`,
    ),
  })
}

export function parseStrongFlowGitHubPublicationContextText(
  value: string,
  path = 'githubPublication.context',
): StrongFlowGitHubPublicationContext {
  try {
    return parseStrongFlowGitHubPublicationContext(JSON.parse(value) as unknown, path)
  } catch (error) {
    if (error instanceof TypeError) throw error
    return failure(path, 'must be valid JSON')
  }
}

export function parseStrongFlowGitHubPublicationDecision(
  value: unknown,
  path = 'githubPublication.decision',
): StrongFlowGitHubPublicationDecision {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'action',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'candidateRef',
    'deliveryVerdictId',
    'reviewStageRunId',
    'attentionItemId',
    'providerIdempotencyKey',
    'publicationSetSha256',
    'comments',
  ], path)
  if (input.schemaVersion !== STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION) {
    failure(`${path}.schemaVersion`, 'is unsupported')
  }
  if (input.protocol !== STRONGFLOW_GITHUB_PUBLICATION_DECISION_PROTOCOL) {
    failure(`${path}.protocol`, 'is unsupported')
  }
  if (typeof input.action !== 'string'
    || !STRONGFLOW_GITHUB_PUBLICATION_ACTIONS.includes(
      input.action as StrongFlowGitHubPublicationAction,
    )) failure(`${path}.action`, 'is unsupported')
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_DECISION_PROTOCOL,
    action: input.action as StrongFlowGitHubPublicationAction,
    deliveryId: DeliveryId(portableId(input.deliveryId, `${path}.deliveryId`)),
    deliverySpecId: DeliverySpecId(
      portableId(input.deliverySpecId, `${path}.deliverySpecId`),
    ),
    deliverySpecRevision: positiveInteger(
      input.deliverySpecRevision,
      `${path}.deliverySpecRevision`,
    ),
    candidateRef: pattern(
      input.candidateRef,
      CANDIDATE_REF_PATTERN,
      `${path}.candidateRef`,
      'a frozen candidate reference',
    ),
    deliveryVerdictId: DeliveryVerdictId(
      portableId(input.deliveryVerdictId, `${path}.deliveryVerdictId`),
    ),
    reviewStageRunId: StageRunId(
      portableId(input.reviewStageRunId, `${path}.reviewStageRunId`),
    ),
    attentionItemId: AttentionItemId(
      portableId(input.attentionItemId, `${path}.attentionItemId`),
    ),
    providerIdempotencyKey: pattern(
      input.providerIdempotencyKey,
      PROVIDER_IDEMPOTENCY_KEY_PATTERN,
      `${path}.providerIdempotencyKey`,
      'a GitHub pull-request idempotency key',
    ),
    publicationSetSha256: pattern(
      input.publicationSetSha256,
      SHA256_PATTERN,
      `${path}.publicationSetSha256`,
      'a lowercase SHA-256 digest',
    ),
    comments: text(input.comments, `${path}.comments`),
  })
}

export function parseStrongFlowGitHubPublicationDecisionText(
  value: string,
  path = 'githubPublication.decision',
): StrongFlowGitHubPublicationDecision {
  try {
    return parseStrongFlowGitHubPublicationDecision(JSON.parse(value) as unknown, path)
  } catch (error) {
    if (error instanceof TypeError) throw error
    return failure(path, 'must be valid JSON')
  }
}

export function serializeStrongFlowGitHubPublicationDecision(
  value: StrongFlowGitHubPublicationDecision,
): string {
  return JSON.stringify(parseStrongFlowGitHubPublicationDecision(value))
}
