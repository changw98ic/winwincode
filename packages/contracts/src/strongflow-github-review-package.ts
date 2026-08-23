import {
  DeliveryId,
  DeliverySpecId,
  DeliveryVerdictId,
  parseGitHubIssueSourceRef,
  parseGitHubPullRequestTargetRef,
  type DeliveryId as DeliveryIdentifier,
  type DeliverySpecId as DeliverySpecIdentifier,
  type DeliveryVerdictId as DeliveryVerdictIdentifier,
  type GitHubIssueSourceRef,
  type GitHubPullRequestTargetRef,
} from './delivery.js'

/** Derived local review files; this protocol adds no persisted Delivery object. */
export const STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION = 1 as const

export const STRONGFLOW_GITHUB_REVIEW_PACKAGE_PROTOCOL =
  'winwincode.github-review-package.v1' as const

export const STRONGFLOW_GITHUB_PR_PREVIEW_PROTOCOL =
  'winwincode.github-pr-preview.v1' as const

export const STRONGFLOW_GITHUB_DRY_RUN_PROTOCOL =
  'winwincode.github-publication-dry-run.v1' as const

export const STRONGFLOW_GITHUB_REVIEW_PACKAGE_MEDIA_TYPES = Object.freeze([
  'application/json',
  'text/markdown',
  'text/x-diff',
] as const)

export type StrongFlowGitHubReviewPackageMediaType =
  typeof STRONGFLOW_GITHUB_REVIEW_PACKAGE_MEDIA_TYPES[number]

export interface StrongFlowGitHubReviewPackageFile {
  readonly path: string
  readonly mediaType: StrongFlowGitHubReviewPackageMediaType
  readonly sha256: string
  readonly bytes: number
}

export interface StrongFlowGitHubDryRunRecord {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_GITHUB_DRY_RUN_PROTOCOL
  readonly mode: 'dry-run'
  readonly publicationOccurred: false
  readonly remoteWriteCount: 0
  readonly publicationSetSha256: string
  readonly recordedAtMillis: number
}

export interface StrongFlowGitHubPullRequestPreview {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_GITHUB_PR_PREVIEW_PROTOCOL
  readonly sourceRef: GitHubIssueSourceRef
  readonly publicationTarget: GitHubPullRequestTargetRef
  readonly title: string
  readonly body: string
  readonly candidateRef: string
  readonly deliveryVerdictId: DeliveryVerdictIdentifier
  readonly publicationSetSha256: string
}

export interface StrongFlowGitHubReviewPackageManifest {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_GITHUB_REVIEW_PACKAGE_PROTOCOL
  readonly packageId: string
  readonly deliveryId: DeliveryIdentifier
  readonly deliverySpecId: DeliverySpecIdentifier
  readonly deliverySpecRevision: number
  readonly sourceRef: GitHubIssueSourceRef
  readonly publicationTarget: GitHubPullRequestTargetRef
  readonly candidateRef: string
  readonly deliveryVerdictId: DeliveryVerdictIdentifier
  readonly planReviewSetSha256: string
  readonly publicationSetSha256: string
  readonly providerIdempotencyKey: string
  readonly generatedFromMillis: number
  readonly files: readonly StrongFlowGitHubReviewPackageFile[]
  readonly dryRun: StrongFlowGitHubDryRunRecord
}

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u
const CANDIDATE_REF_PATTERN = /^git-candidate:sha256:[a-f0-9]{64}$/u
const PACKAGE_ID_PATTERN = /^github-review-package:sha256:[a-f0-9]{64}$/u
const PROVIDER_KEY_PATTERN = /^github:pull-request:sha256:[a-f0-9]{64}$/u
const SHA256_PATTERN = /^[a-f0-9]{64}$/u
const MAX_TEXT_LENGTH = 1_048_576
const MAX_FILES = 100

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

function packagePath(value: unknown, path: string): string {
  if (typeof value !== 'string'
    || value.length === 0
    || value.length > 255
    || value.startsWith('/')
    || value.includes('\\')
    || value.split('/').some(segment => (
      segment.length === 0 || segment === '.' || segment === '..'
    ))) return failure(path, 'must be a portable relative package path')
  return value
}

export function parseStrongFlowGitHubDryRunRecord(
  value: unknown,
  path = 'githubReviewPackage.dryRun',
): StrongFlowGitHubDryRunRecord {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'mode',
    'publicationOccurred',
    'remoteWriteCount',
    'publicationSetSha256',
    'recordedAtMillis',
  ], path)
  if (input.schemaVersion !== STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION
    || input.protocol !== STRONGFLOW_GITHUB_DRY_RUN_PROTOCOL
    || input.mode !== 'dry-run'
    || input.publicationOccurred !== false
    || input.remoteWriteCount !== 0) {
    return failure(path, 'must record a zero-write dry run')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_DRY_RUN_PROTOCOL,
    mode: 'dry-run',
    publicationOccurred: false,
    remoteWriteCount: 0,
    publicationSetSha256: pattern(
      input.publicationSetSha256,
      SHA256_PATTERN,
      `${path}.publicationSetSha256`,
      'a lowercase SHA-256 digest',
    ),
    recordedAtMillis: nonNegativeInteger(input.recordedAtMillis, `${path}.recordedAtMillis`),
  })
}

export function parseStrongFlowGitHubPullRequestPreview(
  value: unknown,
  path = 'githubReviewPackage.pullRequestPreview',
): StrongFlowGitHubPullRequestPreview {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'sourceRef',
    'publicationTarget',
    'title',
    'body',
    'candidateRef',
    'deliveryVerdictId',
    'publicationSetSha256',
  ], path)
  if (input.schemaVersion !== STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION
    || input.protocol !== STRONGFLOW_GITHUB_PR_PREVIEW_PROTOCOL) {
    return failure(path, 'uses an unsupported protocol')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PR_PREVIEW_PROTOCOL,
    sourceRef: parseGitHubIssueSourceRef(input.sourceRef, `${path}.sourceRef`),
    publicationTarget: parseGitHubPullRequestTargetRef(
      input.publicationTarget,
      `${path}.publicationTarget`,
    ),
    title: text(input.title, `${path}.title`),
    body: text(input.body, `${path}.body`),
    candidateRef: pattern(
      input.candidateRef,
      CANDIDATE_REF_PATTERN,
      `${path}.candidateRef`,
      'a frozen candidate reference',
    ),
    deliveryVerdictId: DeliveryVerdictId(
      portableId(input.deliveryVerdictId, `${path}.deliveryVerdictId`),
    ),
    publicationSetSha256: pattern(
      input.publicationSetSha256,
      SHA256_PATTERN,
      `${path}.publicationSetSha256`,
      'a lowercase SHA-256 digest',
    ),
  })
}

export function parseStrongFlowGitHubReviewPackageManifest(
  value: unknown,
  path = 'githubReviewPackage.manifest',
): StrongFlowGitHubReviewPackageManifest {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'packageId',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'sourceRef',
    'publicationTarget',
    'candidateRef',
    'deliveryVerdictId',
    'planReviewSetSha256',
    'publicationSetSha256',
    'providerIdempotencyKey',
    'generatedFromMillis',
    'files',
    'dryRun',
  ], path)
  if (input.schemaVersion !== STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION
    || input.protocol !== STRONGFLOW_GITHUB_REVIEW_PACKAGE_PROTOCOL) {
    return failure(path, 'uses an unsupported protocol')
  }
  if (!Array.isArray(input.files)
    || input.files.length === 0
    || input.files.length > MAX_FILES) {
    return failure(`${path}.files`, 'must be a bounded non-empty array')
  }
  const files = input.files.map((entry, index) => {
    const filePath = `${path}.files[${String(index)}]`
    const file = record(entry, filePath)
    exactKeys(file, ['path', 'mediaType', 'sha256', 'bytes'], filePath)
    if (typeof file.mediaType !== 'string'
      || !STRONGFLOW_GITHUB_REVIEW_PACKAGE_MEDIA_TYPES.includes(
        file.mediaType as StrongFlowGitHubReviewPackageMediaType,
      )) return failure(`${filePath}.mediaType`, 'is unsupported')
    return Object.freeze({
      path: packagePath(file.path, `${filePath}.path`),
      mediaType: file.mediaType as StrongFlowGitHubReviewPackageMediaType,
      sha256: pattern(
        file.sha256,
        SHA256_PATTERN,
        `${filePath}.sha256`,
        'a lowercase SHA-256 digest',
      ),
      bytes: nonNegativeInteger(file.bytes, `${filePath}.bytes`),
    })
  })
  if (new Set(files.map(file => file.path)).size !== files.length) {
    return failure(`${path}.files`, 'contains duplicate paths')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_REVIEW_PACKAGE_PROTOCOL,
    packageId: pattern(
      input.packageId,
      PACKAGE_ID_PATTERN,
      `${path}.packageId`,
      'a GitHub review package identity',
    ),
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
    planReviewSetSha256: pattern(
      input.planReviewSetSha256,
      SHA256_PATTERN,
      `${path}.planReviewSetSha256`,
      'a lowercase SHA-256 digest',
    ),
    publicationSetSha256: pattern(
      input.publicationSetSha256,
      SHA256_PATTERN,
      `${path}.publicationSetSha256`,
      'a lowercase SHA-256 digest',
    ),
    providerIdempotencyKey: pattern(
      input.providerIdempotencyKey,
      PROVIDER_KEY_PATTERN,
      `${path}.providerIdempotencyKey`,
      'a GitHub pull-request idempotency key',
    ),
    generatedFromMillis: nonNegativeInteger(
      input.generatedFromMillis,
      `${path}.generatedFromMillis`,
    ),
    files: Object.freeze(files),
    dryRun: parseStrongFlowGitHubDryRunRecord(input.dryRun, `${path}.dryRun`),
  })
}
