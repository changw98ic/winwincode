import { createHash } from 'node:crypto'

import {
  type FrozenDeliveryCandidate,
  type StrongFlowGitHubReviewPackageManifest,
} from '@winwincode/contracts'

import {
  type GeneratedStrongFlowGitHubReviewPackage,
  verifyStrongFlowGitHubReviewPackage,
} from './github-review-package.js'
import { containsRawCredentialMaterial } from './credential-boundary.js'

/** Provider operations are derived side-effect requests, not Delivery business objects. */
export const STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION = 1 as const

export const STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL =
  'winwincode.github-provider-operation.v1' as const

export const STRONGFLOW_GITHUB_PROVIDER_OPERATION_KINDS = Object.freeze([
  'branch',
  'pull-request',
  'issue-comment',
  'commit-status',
] as const)

export type StrongFlowGitHubProviderOperationKind =
  typeof STRONGFLOW_GITHUB_PROVIDER_OPERATION_KINDS[number]

interface StrongFlowGitHubProviderOperationBase {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL
  readonly kind: StrongFlowGitHubProviderOperationKind
  readonly operationKey: string
  readonly requestSha256: string
}

export interface StrongFlowGitHubBranchOperation
  extends StrongFlowGitHubProviderOperationBase {
  readonly kind: 'branch'
  readonly payload: {
    readonly repository: string
    readonly branch: string
    readonly commitId: string
  }
}

export interface StrongFlowGitHubPullRequestOperation
  extends StrongFlowGitHubProviderOperationBase {
  readonly kind: 'pull-request'
  readonly payload: {
    readonly repository: string
    readonly baseBranch: string
    readonly headRepository: string
    readonly headBranch: string
    readonly title: string
    readonly body: string
  }
}

export interface StrongFlowGitHubIssueCommentOperation
  extends StrongFlowGitHubProviderOperationBase {
  readonly kind: 'issue-comment'
  readonly payload: {
    readonly repository: string
    readonly issueNumber: number
    readonly body: string
  }
}

export interface StrongFlowGitHubCommitStatusOperation
  extends StrongFlowGitHubProviderOperationBase {
  readonly kind: 'commit-status'
  readonly payload: {
    readonly repository: string
    readonly commitId: string
    readonly context: string
    readonly state: 'success'
    readonly description: string
    readonly targetUrl: string
  }
}

export type StrongFlowGitHubProviderOperation =
  | StrongFlowGitHubBranchOperation
  | StrongFlowGitHubPullRequestOperation
  | StrongFlowGitHubIssueCommentOperation
  | StrongFlowGitHubCommitStatusOperation

export type StrongFlowGitHubProviderObservation =
  | {
      readonly state: 'found'
      readonly operationKey: string
      readonly requestSha256: string
      readonly resourceRef: string
    }
  | { readonly state: 'absent'; readonly operationKey: string }
  | {
      readonly state: 'unknown' | 'conflict'
      readonly operationKey: string
      readonly code: string
    }

export type StrongFlowGitHubProviderMutation =
  | {
      readonly state: 'applied'
      readonly operationKey: string
      readonly requestSha256: string
      readonly resourceRef: string
      readonly remoteWritePerformed: boolean
    }
  | {
      readonly state: 'unknown' | 'rejected'
      readonly operationKey: string
      readonly code: string
    }

/**
 * DSH or another product-shell plugin supplies this adapter with its existing
 * credential boundary. `apply` must converge by operationKey when called more
 * than once; the coordinator also performs lookup before each call.
 */
export interface StrongFlowGitHubPublicationProvider {
  readonly lookup: (
    operation: StrongFlowGitHubProviderOperation,
  ) => Promise<StrongFlowGitHubProviderObservation>
  readonly apply: (
    operation: StrongFlowGitHubProviderOperation,
  ) => Promise<StrongFlowGitHubProviderMutation>
}

export type StrongFlowGitHubPublicationProviderErrorCode =
  | 'INVALID_OPERATION'
  | 'INVALID_PROVIDER_RESULT'

export class StrongFlowGitHubPublicationProviderError extends Error {
  readonly code: StrongFlowGitHubPublicationProviderErrorCode

  constructor(
    code: StrongFlowGitHubPublicationProviderErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowGitHubPublicationProviderError'
    this.code = code
  }
}

const SHA256_PATTERN = /^[a-f0-9]{64}$/u
const GIT_OBJECT_PATTERN = /^(?:[a-f0-9]{40}|[a-f0-9]{64})$/u
const OPERATION_KEY_PATTERN =
  /^github:pull-request:sha256:[a-f0-9]{64}:(?:branch|pull-request|issue-comment|commit-status)$/u
const REPOSITORY_PATTERN = /^[a-z0-9](?:[a-z0-9._-]{0,99})\/[a-z0-9](?:[a-z0-9._-]{0,99})$/u
const PROVIDER_CODE_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,99}$/u
const MAX_TEXT_LENGTH = 1_048_576
const MAX_RESOURCE_REF_LENGTH = 8_192

function providerError(
  code: StrongFlowGitHubPublicationProviderErrorCode,
  message: string,
  cause?: unknown,
): never {
  throw new StrongFlowGitHubPublicationProviderError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (!isRecord(value)) return providerError('INVALID_OPERATION', `${label} must be an object`)
  return value
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  keys: readonly string[],
  label: string,
): void {
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    return providerError('INVALID_OPERATION', `${label} has an unexpected shape`)
  }
}

function pattern(value: unknown, expected: RegExp, label: string): string {
  if (typeof value !== 'string' || !expected.test(value)) {
    return providerError('INVALID_OPERATION', `${label} is invalid`)
  }
  return value
}

function text(value: unknown, label: string, maximum = MAX_TEXT_LENGTH): string {
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > maximum
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    return providerError('INVALID_OPERATION', `${label} must be bounded text`)
  }
  return value
}

function positiveInteger(value: unknown, label: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1 || Object.is(value, -0)) {
    return providerError('INVALID_OPERATION', `${label} must be a positive safe integer`)
  }
  return Number(value)
}

function gitBranch(value: unknown, label: string): string {
  if (typeof value !== 'string'
    || value.length === 0
    || value.length > 255
    || value === '@'
    || value.startsWith('/')
    || value.endsWith('/')
    || value.endsWith('.')
    || value.includes('..')
    || value.includes('@{')
    || value.includes('//')
    || /[\u0000-\u0020\u007f~^:?*\\]/u.test(value)
    || value.includes('[')
    || value.split('/').some(segment => (
      segment.length === 0 || segment.startsWith('.') || segment.endsWith('.lock')
    ))) return providerError('INVALID_OPERATION', `${label} is not a valid Git branch`)
  return value
}

function digest(value: unknown): string {
  return createHash('sha256').update(JSON.stringify(value)).digest('hex')
}

function operationWithoutDigest(
  operation: StrongFlowGitHubProviderOperation,
): Omit<StrongFlowGitHubProviderOperation, 'requestSha256'> {
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
    kind: operation.kind,
    operationKey: operation.operationKey,
    payload: operation.payload,
  }) as Omit<StrongFlowGitHubProviderOperation, 'requestSha256'>
}

function operation<Value extends StrongFlowGitHubProviderOperation>(
  value: Omit<Value, 'requestSha256'>,
): Value {
  return Object.freeze({ ...value, requestSha256: digest(value) }) as Value
}

function operationKey(
  manifest: StrongFlowGitHubReviewPackageManifest,
  kind: StrongFlowGitHubProviderOperationKind,
): string {
  return `${manifest.providerIdempotencyKey}:${kind}`
}

/** Build the complete, ordered remote intent before the first provider read or write. */
export function buildStrongFlowGitHubProviderOperations(
  reviewPackageValue: GeneratedStrongFlowGitHubReviewPackage,
  candidate: FrozenDeliveryCandidate,
): readonly StrongFlowGitHubProviderOperation[] {
  const reviewPackage = verifyStrongFlowGitHubReviewPackage(reviewPackageValue)
  const manifest = reviewPackage.manifest
  if (candidate.candidateRef !== manifest.candidateRef) {
    return providerError('INVALID_OPERATION', 'provider candidate does not match review package')
  }
  const issueUrl = `https://github.com/${manifest.sourceRef.repository}/issues/${String(manifest.sourceRef.number)}`
  const marker = `<!-- winwincode-publication:${manifest.providerIdempotencyKey} -->`
  const commentBody = [
    marker,
    '',
    `WinWinCode Delivery \`${manifest.deliveryId}\` has a reviewed publication candidate.`,
    '',
    `- Candidate: \`${manifest.candidateRef}\``,
    `- DeliveryVerdict: \`${manifest.deliveryVerdictId}\``,
    `- Review package: \`${manifest.packageId}\``,
    `- Target: \`${manifest.publicationTarget.headRepository}:${manifest.publicationTarget.headBranch}\` → \`${manifest.publicationTarget.repository}:${manifest.publicationTarget.baseBranch}\``,
    '',
  ].join('\n')
  const operations: readonly StrongFlowGitHubProviderOperation[] = [
    operation<StrongFlowGitHubBranchOperation>({
      schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
      kind: 'branch',
      operationKey: operationKey(manifest, 'branch'),
      payload: Object.freeze({
        repository: manifest.publicationTarget.headRepository,
        branch: manifest.publicationTarget.headBranch,
        commitId: candidate.candidateCommitId,
      }),
    }),
    operation<StrongFlowGitHubPullRequestOperation>({
      schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
      kind: 'pull-request',
      operationKey: operationKey(manifest, 'pull-request'),
      payload: Object.freeze({
        repository: manifest.publicationTarget.repository,
        baseBranch: manifest.publicationTarget.baseBranch,
        headRepository: manifest.publicationTarget.headRepository,
        headBranch: manifest.publicationTarget.headBranch,
        title: reviewPackage.preview.title,
        body: reviewPackage.preview.body,
      }),
    }),
    operation<StrongFlowGitHubIssueCommentOperation>({
      schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
      kind: 'issue-comment',
      operationKey: operationKey(manifest, 'issue-comment'),
      payload: Object.freeze({
        repository: manifest.sourceRef.repository,
        issueNumber: manifest.sourceRef.number,
        body: commentBody,
      }),
    }),
    operation<StrongFlowGitHubCommitStatusOperation>({
      schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
      kind: 'commit-status',
      operationKey: operationKey(manifest, 'commit-status'),
      payload: Object.freeze({
        repository: manifest.publicationTarget.headRepository,
        commitId: candidate.candidateCommitId,
        context: 'winwincode/delivery',
        state: 'success',
        description: 'WinWinCode verified all required acceptance criteria.',
        targetUrl: issueUrl,
      }),
    }),
  ]
  return Object.freeze(operations.map(entry => parseStrongFlowGitHubProviderOperation(entry)))
}

export function parseStrongFlowGitHubProviderOperation(
  value: unknown,
  label = 'githubProvider.operation',
): StrongFlowGitHubProviderOperation {
  const input = record(value, label)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'kind',
    'operationKey',
    'requestSha256',
    'payload',
  ], label)
  if (input.schemaVersion !== STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION
    || input.protocol !== STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL
    || typeof input.kind !== 'string'
    || !STRONGFLOW_GITHUB_PROVIDER_OPERATION_KINDS.includes(
      input.kind as StrongFlowGitHubProviderOperationKind,
    )) return providerError('INVALID_OPERATION', `${label} uses an unsupported protocol`)
  const kind = input.kind as StrongFlowGitHubProviderOperationKind
  const key = pattern(input.operationKey, OPERATION_KEY_PATTERN, `${label}.operationKey`)
  if (!key.endsWith(`:${kind}`)) {
    return providerError('INVALID_OPERATION', `${label} operation key names another kind`)
  }
  const payload = record(input.payload, `${label}.payload`)
  let parsed: StrongFlowGitHubProviderOperation
  if (kind === 'branch') {
    exactKeys(payload, ['repository', 'branch', 'commitId'], `${label}.payload`)
    parsed = {
      schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
      kind,
      operationKey: key,
      requestSha256: pattern(input.requestSha256, SHA256_PATTERN, `${label}.requestSha256`),
      payload: Object.freeze({
        repository: pattern(payload.repository, REPOSITORY_PATTERN, `${label}.repository`),
        branch: gitBranch(payload.branch, `${label}.branch`),
        commitId: pattern(payload.commitId, GIT_OBJECT_PATTERN, `${label}.commitId`),
      }),
    }
  } else if (kind === 'pull-request') {
    exactKeys(
      payload,
      ['repository', 'baseBranch', 'headRepository', 'headBranch', 'title', 'body'],
      `${label}.payload`,
    )
    parsed = {
      schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
      kind,
      operationKey: key,
      requestSha256: pattern(input.requestSha256, SHA256_PATTERN, `${label}.requestSha256`),
      payload: Object.freeze({
        repository: pattern(payload.repository, REPOSITORY_PATTERN, `${label}.repository`),
        baseBranch: gitBranch(payload.baseBranch, `${label}.baseBranch`),
        headRepository: pattern(
          payload.headRepository,
          REPOSITORY_PATTERN,
          `${label}.headRepository`,
        ),
        headBranch: gitBranch(payload.headBranch, `${label}.headBranch`),
        title: text(payload.title, `${label}.title`, 512),
        body: text(payload.body, `${label}.body`),
      }),
    }
  } else if (kind === 'issue-comment') {
    exactKeys(payload, ['repository', 'issueNumber', 'body'], `${label}.payload`)
    parsed = {
      schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
      kind,
      operationKey: key,
      requestSha256: pattern(input.requestSha256, SHA256_PATTERN, `${label}.requestSha256`),
      payload: Object.freeze({
        repository: pattern(payload.repository, REPOSITORY_PATTERN, `${label}.repository`),
        issueNumber: positiveInteger(payload.issueNumber, `${label}.issueNumber`),
        body: text(payload.body, `${label}.body`),
      }),
    }
  } else {
    exactKeys(
      payload,
      ['repository', 'commitId', 'context', 'state', 'description', 'targetUrl'],
      `${label}.payload`,
    )
    if (payload.state !== 'success') {
      return providerError('INVALID_OPERATION', `${label}.state must be success`)
    }
    parsed = {
      schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
      kind: 'commit-status',
      operationKey: key,
      requestSha256: pattern(input.requestSha256, SHA256_PATTERN, `${label}.requestSha256`),
      payload: Object.freeze({
        repository: pattern(payload.repository, REPOSITORY_PATTERN, `${label}.repository`),
        commitId: pattern(payload.commitId, GIT_OBJECT_PATTERN, `${label}.commitId`),
        context: text(payload.context, `${label}.context`, 255),
        state: 'success',
        description: text(payload.description, `${label}.description`, 255),
        targetUrl: text(payload.targetUrl, `${label}.targetUrl`, 8_192),
      }),
    }
  }
  const frozen = Object.freeze(parsed)
  if (frozen.requestSha256 !== digest(operationWithoutDigest(frozen))) {
    return providerError('INVALID_OPERATION', `${label} request digest changed`)
  }
  if (containsRawCredentialMaterial(frozen)) {
    return providerError('INVALID_OPERATION', `${label} contains raw credential material`)
  }
  return frozen
}

function providerCode(value: unknown, label: string): string {
  return pattern(value, PROVIDER_CODE_PATTERN, label)
}

function providerResource(value: unknown, label: string): string {
  return text(value, label, MAX_RESOURCE_REF_LENGTH)
}

export function parseStrongFlowGitHubProviderObservation(
  operationValue: StrongFlowGitHubProviderOperation,
  value: unknown,
): StrongFlowGitHubProviderObservation {
  const operation = parseStrongFlowGitHubProviderOperation(operationValue)
  const input = record(value, 'githubProvider.observation')
  if (input.state === 'found') {
    exactKeys(input, ['state', 'operationKey', 'requestSha256', 'resourceRef'], 'observation')
    const parsed = Object.freeze({
      state: 'found' as const,
      operationKey: pattern(input.operationKey, OPERATION_KEY_PATTERN, 'observation.operationKey'),
      requestSha256: pattern(input.requestSha256, SHA256_PATTERN, 'observation.requestSha256'),
      resourceRef: providerResource(input.resourceRef, 'observation.resourceRef'),
    })
    if (parsed.operationKey !== operation.operationKey) {
      return providerError('INVALID_PROVIDER_RESULT', 'provider observed another operation')
    }
    if (containsRawCredentialMaterial(parsed)) {
      return providerError('INVALID_PROVIDER_RESULT', 'provider observation contains credentials')
    }
    return parsed
  }
  if (input.state === 'absent') {
    exactKeys(input, ['state', 'operationKey'], 'observation')
    const key = pattern(input.operationKey, OPERATION_KEY_PATTERN, 'observation.operationKey')
    if (key !== operation.operationKey) {
      return providerError('INVALID_PROVIDER_RESULT', 'provider observed another operation')
    }
    return Object.freeze({ state: 'absent', operationKey: key })
  }
  if (input.state !== 'unknown' && input.state !== 'conflict') {
    return providerError('INVALID_PROVIDER_RESULT', 'provider observation state is unsupported')
  }
  exactKeys(input, ['state', 'operationKey', 'code'], 'observation')
  const key = pattern(input.operationKey, OPERATION_KEY_PATTERN, 'observation.operationKey')
  if (key !== operation.operationKey) {
    return providerError('INVALID_PROVIDER_RESULT', 'provider observed another operation')
  }
  return Object.freeze({
    state: input.state,
    operationKey: key,
    code: providerCode(input.code, 'observation.code'),
  })
}

export function parseStrongFlowGitHubProviderMutation(
  operationValue: StrongFlowGitHubProviderOperation,
  value: unknown,
): StrongFlowGitHubProviderMutation {
  const operation = parseStrongFlowGitHubProviderOperation(operationValue)
  const input = record(value, 'githubProvider.mutation')
  if (input.state === 'applied') {
    exactKeys(
      input,
      ['state', 'operationKey', 'requestSha256', 'resourceRef', 'remoteWritePerformed'],
      'mutation',
    )
    if (typeof input.remoteWritePerformed !== 'boolean') {
      return providerError('INVALID_PROVIDER_RESULT', 'mutation write marker must be boolean')
    }
    const parsed = Object.freeze({
      state: 'applied' as const,
      operationKey: pattern(input.operationKey, OPERATION_KEY_PATTERN, 'mutation.operationKey'),
      requestSha256: pattern(input.requestSha256, SHA256_PATTERN, 'mutation.requestSha256'),
      resourceRef: providerResource(input.resourceRef, 'mutation.resourceRef'),
      remoteWritePerformed: input.remoteWritePerformed,
    })
    if (parsed.operationKey !== operation.operationKey
      || parsed.requestSha256 !== operation.requestSha256) {
      return providerError('INVALID_PROVIDER_RESULT', 'provider applied another operation request')
    }
    if (containsRawCredentialMaterial(parsed)) {
      return providerError('INVALID_PROVIDER_RESULT', 'provider mutation contains credentials')
    }
    return parsed
  }
  if (input.state !== 'unknown' && input.state !== 'rejected') {
    return providerError('INVALID_PROVIDER_RESULT', 'provider mutation state is unsupported')
  }
  exactKeys(input, ['state', 'operationKey', 'code'], 'mutation')
  const key = pattern(input.operationKey, OPERATION_KEY_PATTERN, 'mutation.operationKey')
  if (key !== operation.operationKey) {
    return providerError('INVALID_PROVIDER_RESULT', 'provider mutated another operation')
  }
  return Object.freeze({
    state: input.state,
    operationKey: key,
    code: providerCode(input.code, 'mutation.code'),
  })
}
