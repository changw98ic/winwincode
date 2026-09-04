/**
 * ClientControlPort contracts for the multi-user shared Client model.
 *
 * Authoritative source: `schema/winwincode/v1/client-control.schema.json`
 * (shared scalars come from its `domain.schema.json` sibling). This module is
 * the Phase 0 TypeScript projection of that schema: the schema inlines the
 * envelope and command fields into every message, so message fields live flat
 * on the message beside `kind` and there is no payload sub-object. Every field
 * name, enumeration, pattern, identifier prefix, and classifier is taken
 * verbatim from the schema.
 *
 * Boundary invariants enforced by this module:
 * - no field carries a local filesystem path; repository bindings resolve only
 *   inside the Device Client and the Server never sees or stores a path;
 * - `ClientConnectCode` carries only `codeDigest`, never connect-code plaintext;
 * - exactly the 19 command-class messages carry `expectedRevision` plus
 *   `idempotencyKey` on the message envelope; the 8 non-command messages
 *   (heartbeat, hello, worker.state, worker.reconcile, and repository.status
 *   reports, the command ack, enrollment_accepted, and access.challenge)
 *   reject both fields;
 * - exactly 11 command-class messages are stamped with the occupancy lease
 *   pair `occupancyLeaseId` + `occupancyFencingToken`; `schemaVersion` is the
 *   string constant "winwincode/v1"; `occupancyFencingToken` is a decimal
 *   string matching ^[1-9][0-9]{0,19}$; every timestamp is an RFC3339
 *   `Instant` string with milliseconds.
 */

export const CLIENT_CONTROL_SCHEMA_VERSION = 'winwincode/v1' as const

export type SchemaVersion = typeof CLIENT_CONTROL_SCHEMA_VERSION

export type ClientControlValidationErrorCode =
  | 'INVALID_SHAPE'
  | 'UNSUPPORTED_SCHEMA_VERSION'
  | 'INVALID_IDENTIFIER'
  | 'INVALID_VALUE'
  | 'DUPLICATE_ID'
  | 'RELATIONSHIP_MISMATCH'

export class ClientControlValidationError extends Error {
  readonly code: ClientControlValidationErrorCode
  readonly path: string

  constructor(
    code: ClientControlValidationErrorCode,
    path: string,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'ClientControlValidationError'
    this.code = code
    this.path = path
  }
}

function controlError(
  code: ClientControlValidationErrorCode,
  path: string,
  message: string,
  options?: ErrorOptions,
): never {
  throw new ClientControlValidationError(code, path, message, options)
}

declare const clientControlBrand: unique symbol

type BrandedText<Name extends string> = string & {
  readonly [clientControlBrand]: Name
}

const CROCKFORD_BODY = '[0-9A-HJKMNP-TV-Z]{26}'

function crockfordIdentifierPattern(prefix: string): RegExp {
  return new RegExp(`^${prefix}_${CROCKFORD_BODY}$`, 'u')
}

const CLIENT_CONTROL_MESSAGE_ID_PATTERN = crockfordIdentifierPattern('cmsg')
const CLIENT_NODE_ID_PATTERN = crockfordIdentifierPattern('cnd')
const CLIENT_INSTANCE_ID_PATTERN = crockfordIdentifierPattern('cix')
const CLIENT_OCCUPANCY_LEASE_ID_PATTERN = crockfordIdentifierPattern('ocl')
const CLIENT_CONNECT_CODE_ID_PATTERN = crockfordIdentifierPattern('cct')
const CLIENT_ACCESS_CHALLENGE_ID_PATTERN = crockfordIdentifierPattern('cac')
const CLIENT_ACCESS_GRANT_ID_PATTERN = crockfordIdentifierPattern('cag')
const OCCUPANCY_CLAIM_ID_PATTERN = crockfordIdentifierPattern('ocq')
const REPOSITORY_BINDING_ID_PATTERN = crockfordIdentifierPattern('rbd')
const REPOSITORY_ACCESS_GRANT_ID_PATTERN = crockfordIdentifierPattern('rag')
const WORKER_LAUNCH_GRANT_ID_PATTERN = crockfordIdentifierPattern('wlg')
const LOCAL_CANDIDATE_RECEIPT_ID_PATTERN = crockfordIdentifierPattern('lcr')
const LOCAL_APPLY_RECEIPT_ID_PATTERN = crockfordIdentifierPattern('lar')
const USER_ID_PATTERN = crockfordIdentifierPattern('usr')
const PRODUCT_SESSION_ID_PATTERN = crockfordIdentifierPattern('psn')
const STAGE_RUN_ID_PATTERN = crockfordIdentifierPattern('run')
const WORKER_SESSION_ID_PATTERN = crockfordIdentifierPattern('wsn')
const WORKER_ID_PATTERN = crockfordIdentifierPattern('wrk')
const WORKER_INSTANCE_ID_PATTERN = crockfordIdentifierPattern('wki')

const PUBLIC_CLIENT_ID_PATTERN = /^[0-9]{9,12}$/u
const OCCUPANCY_FENCING_TOKEN_PATTERN = /^[1-9][0-9]{0,19}$/u
const IDEMPOTENCY_KEY_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{7,199}$/u
const SHA256_DIGEST_PATTERN = /^sha256:[0-9a-f]{64}$/u
const GIT_COMMIT_SHA_PATTERN = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u
const GIT_REF_NAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9/._-]{0,254}$/u
const CANDIDATE_REF_PATTERN
  = new RegExp('^refs/winwincode/candidates/[A-Za-z0-9][A-Za-z0-9._-]{0,199}$', 'u')
const CONFLICT_ARTIFACT_REF_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u
const CLIENT_VERSION_PATTERN = /^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$/u
const USERNAME_PATTERN = /^[A-Za-z0-9][A-Za-z0-9_.@-]{0,63}$/u
const NORMALIZED_USERNAME_PATTERN = /^[a-z0-9][a-z0-9_.@-]{0,63}$/u
const PASSWORD_HASH_PATTERN = /^\$argon2id\$[A-Za-z0-9$+=/.,-]{10,500}$/u
const INSTANT_PATTERN
  = /^[0-9]{4}-(0[1-9]|1[0-2])-([0-2][0-9]|3[01])T([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]\.[0-9]{3}Z$/u

const MAX_REVISION = 9_007_199_254_740_991
const MAX_CLIENT_VERSION_LENGTH = 64
const MAX_DISPLAY_NAME_LENGTH = 200
const MAX_ERROR_MESSAGE_LENGTH = 500

export type ClientControlMessageId = BrandedText<'ClientControlMessageId'>
export type ClientNodeId = BrandedText<'ClientNodeId'>
export type ClientInstanceId = BrandedText<'ClientInstanceId'>
export type ClientOccupancyLeaseId = BrandedText<'ClientOccupancyLeaseId'>
export type ClientConnectCodeId = BrandedText<'ClientConnectCodeId'>
export type ClientAccessChallengeId = BrandedText<'ClientAccessChallengeId'>
export type ClientAccessGrantId = BrandedText<'ClientAccessGrantId'>
export type OccupancyClaimId = BrandedText<'OccupancyClaimId'>
export type RepositoryBindingId = BrandedText<'RepositoryBindingId'>
export type RepositoryAccessGrantId = BrandedText<'RepositoryAccessGrantId'>
export type WorkerLaunchGrantId = BrandedText<'WorkerLaunchGrantId'>
export type LocalCandidateReceiptId = BrandedText<'LocalCandidateReceiptId'>
export type LocalApplyReceiptId = BrandedText<'LocalApplyReceiptId'>
export type UserId = BrandedText<'UserId'>
export type ProductSessionId = BrandedText<'ProductSessionId'>
/**
 * Local alias for the domain StageRunId scalar (^run_ + 26 Crockford chars).
 * The contracts barrel already exports the delivery-lane StageRunId, so this
 * projection keeps its schema-validated alias module-local.
 */
type StageRunIdentifier = BrandedText<'StageRunId'>
export type WorkerSessionId = BrandedText<'WorkerSessionId'>
export type WorkerId = BrandedText<'WorkerId'>
export type WorkerInstanceId = BrandedText<'WorkerInstanceId'>
export type PublicClientId = BrandedText<'PublicClientId'>
export type OccupancyFencingToken = BrandedText<'OccupancyFencingToken'>
export type IdempotencyKey = BrandedText<'IdempotencyKey'>
export type Sha256Digest = BrandedText<'Sha256Digest'>
export type GitCommitSha = BrandedText<'GitCommitSha'>
export type GitRefName = BrandedText<'GitRefName'>
export type CandidateRef = BrandedText<'CandidateRef'>

/** `domain.schema.json` Instant: RFC3339 UTC string with milliseconds. */
export type Instant = string

/** `domain.schema.json` Revision: a non-negative safe integer. */
export type Revision = number

/** Per-client exchange stream position: a positive safe integer. */
export type ClientExchangeSequence = number

function brandedText<Name extends string>(
  value: unknown,
  path: string,
  name: Name,
  pattern: RegExp,
  expectation: string,
): BrandedText<Name> {
  if (typeof value !== 'string' || !pattern.test(value)) {
    controlError('INVALID_IDENTIFIER', path, `${path} must be ${expectation}`)
  }
  return value as BrandedText<Name>
}

function crockfordId<Name extends string>(
  name: Name,
  pattern: RegExp,
): (value: unknown, path: string) => BrandedText<Name> {
  return (value, path) => brandedText(
    value,
    path,
    name,
    pattern,
    `a ${name} matching ${String(pattern)}`,
  )
}

export const CLIENT_CONTROL_MESSAGE_ID = crockfordId(
  'ClientControlMessageId',
  CLIENT_CONTROL_MESSAGE_ID_PATTERN,
)
export const CLIENT_NODE_ID = crockfordId('ClientNodeId', CLIENT_NODE_ID_PATTERN)
export const CLIENT_INSTANCE_ID = crockfordId('ClientInstanceId', CLIENT_INSTANCE_ID_PATTERN)
export const CLIENT_OCCUPANCY_LEASE_ID = crockfordId(
  'ClientOccupancyLeaseId',
  CLIENT_OCCUPANCY_LEASE_ID_PATTERN,
)
export const CLIENT_CONNECT_CODE_ID = crockfordId(
  'ClientConnectCodeId',
  CLIENT_CONNECT_CODE_ID_PATTERN,
)
export const CLIENT_ACCESS_CHALLENGE_ID = crockfordId(
  'ClientAccessChallengeId',
  CLIENT_ACCESS_CHALLENGE_ID_PATTERN,
)
export const CLIENT_ACCESS_GRANT_ID = crockfordId(
  'ClientAccessGrantId',
  CLIENT_ACCESS_GRANT_ID_PATTERN,
)
export const OCCUPANCY_CLAIM_ID = crockfordId('OccupancyClaimId', OCCUPANCY_CLAIM_ID_PATTERN)
export const REPOSITORY_BINDING_ID = crockfordId(
  'RepositoryBindingId',
  REPOSITORY_BINDING_ID_PATTERN,
)
export const REPOSITORY_ACCESS_GRANT_ID = crockfordId(
  'RepositoryAccessGrantId',
  REPOSITORY_ACCESS_GRANT_ID_PATTERN,
)
export const WORKER_LAUNCH_GRANT_ID = crockfordId(
  'WorkerLaunchGrantId',
  WORKER_LAUNCH_GRANT_ID_PATTERN,
)
export const LOCAL_CANDIDATE_RECEIPT_ID = crockfordId(
  'LocalCandidateReceiptId',
  LOCAL_CANDIDATE_RECEIPT_ID_PATTERN,
)
export const LOCAL_APPLY_RECEIPT_ID = crockfordId(
  'LocalApplyReceiptId',
  LOCAL_APPLY_RECEIPT_ID_PATTERN,
)
export const USER_ID = crockfordId('UserId', USER_ID_PATTERN)
export const PRODUCT_SESSION_ID = crockfordId('ProductSessionId', PRODUCT_SESSION_ID_PATTERN)
export const STAGE_RUN_ID = crockfordId('StageRunId', STAGE_RUN_ID_PATTERN)
export const WORKER_SESSION_ID = crockfordId('WorkerSessionId', WORKER_SESSION_ID_PATTERN)
export const WORKER_ID = crockfordId('WorkerId', WORKER_ID_PATTERN)
export const WORKER_INSTANCE_ID = crockfordId('WorkerInstanceId', WORKER_INSTANCE_ID_PATTERN)

export const PUBLIC_CLIENT_ID = (
  value: unknown,
  path: string,
): PublicClientId => brandedText(
  value,
  path,
  'PublicClientId',
  PUBLIC_CLIENT_ID_PATTERN,
  'a PublicClientId of 9 to 12 digits',
)

export const OCCUPANCY_FENCING_TOKEN = (
  value: unknown,
  path: string,
): OccupancyFencingToken => brandedText(
  value,
  path,
  'OccupancyFencingToken',
  OCCUPANCY_FENCING_TOKEN_PATTERN,
  'a decimal-string OccupancyFencingToken without a leading zero',
)

export const IDEMPOTENCY_KEY = (value: unknown, path: string): IdempotencyKey => brandedText(
  value,
  path,
  'IdempotencyKey',
  IDEMPOTENCY_KEY_PATTERN,
  'an opaque IdempotencyKey of 8 to 200 portable characters',
)

export const SHA256_DIGEST = (value: unknown, path: string): Sha256Digest => brandedText(
  value,
  path,
  'Sha256Digest',
  SHA256_DIGEST_PATTERN,
  'a "sha256:"-prefixed lowercase hexadecimal digest; plaintext secrets never cross this port',
)

export const GIT_COMMIT_SHA = (value: unknown, path: string): GitCommitSha => brandedText(
  value,
  path,
  'GitCommitSha',
  GIT_COMMIT_SHA_PATTERN,
  'a full lowercase Git commit object name',
)

export const GIT_REF_NAME = (value: unknown, path: string): GitRefName => brandedText(
  value,
  path,
  'GitRefName',
  GIT_REF_NAME_PATTERN,
  'a Git ref name accepted by git check-ref-format',
)

export const CANDIDATE_REF = (value: unknown, path: string): CandidateRef => brandedText(
  value,
  path,
  'CandidateRef',
  CANDIDATE_REF_PATTERN,
  'a stable local CandidateRef under refs/winwincode/candidates',
)

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) controlError('INVALID_SHAPE', path, `${path} must be an object`)
  return value
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  requiredKeys: readonly string[],
  path: string,
  optionalKeys: readonly string[] = [],
): void {
  const permitted = new Set([...requiredKeys, ...optionalKeys])
  if (Object.keys(value).some(key => !permitted.has(key))) {
    controlError('INVALID_SHAPE', path, `${path} has an unexpected key`)
  }
  if (requiredKeys.some(key => !Object.hasOwn(value, key))) {
    controlError('INVALID_SHAPE', path, `${path} is missing a required key`)
  }
}

function schemaVersion(value: unknown, path: string): SchemaVersion {
  if (value !== CLIENT_CONTROL_SCHEMA_VERSION) {
    controlError(
      'UNSUPPORTED_SCHEMA_VERSION',
      path,
      `${path} must be the string "${CLIENT_CONTROL_SCHEMA_VERSION}"`,
    )
  }
  return CLIENT_CONTROL_SCHEMA_VERSION
}

function clientDisplayName(value: unknown, path: string): string {
  if (typeof value !== 'string'
    || value.length === 0
    || value.length > MAX_DISPLAY_NAME_LENGTH) {
    controlError('INVALID_VALUE', path, `${path} must be 1 to 200 characters of display name text`)
  }
  return value
}

function clientVersion(value: unknown, path: string): ClientVersion {
  return brandedText(
    value,
    path,
    'ClientVersion',
    CLIENT_VERSION_PATTERN,
    `a semantic ClientVersion of at most ${String(MAX_CLIENT_VERSION_LENGTH)} characters`,
  )
}

type ClientVersion = BrandedText<'ClientVersion'>

function instant(value: unknown, path: string): Instant {
  if (typeof value !== 'string' || !INSTANT_PATTERN.test(value)) {
    controlError(
      'INVALID_VALUE',
      path,
      `${path} must be an RFC3339 Instant string with milliseconds (for example 2026-09-01T09:30:15.000Z)`,
    )
  }
  return value
}

function nullable<T>(
  value: unknown,
  path: string,
  parse: (nested: unknown, nestedPath: string) => T,
): T | null {
  return value === null ? null : parse(value, path)
}

function revision(value: unknown, path: string): Revision {
  if (typeof value !== 'number'
    || !Number.isSafeInteger(value)
    || value < 0
    || Object.is(value, -0)
    || value > MAX_REVISION) {
    controlError('INVALID_VALUE', path, `${path} must be a non-negative safe integer Revision`)
  }
  return value
}

function exchangeSequence(value: unknown, path: string): ClientExchangeSequence {
  if (typeof value !== 'number'
    || !Number.isSafeInteger(value)
    || value < 1
    || Object.is(value, -0)
    || value > MAX_REVISION) {
    controlError('INVALID_VALUE', path, `${path} must be a positive safe integer sequence`)
  }
  return value
}

function boundedInteger(
  value: unknown,
  path: string,
  minimum: number,
  maximum: number,
): number {
  if (typeof value !== 'number'
    || !Number.isSafeInteger(value)
    || value < minimum
    || value > maximum) {
    controlError(
      'INVALID_VALUE',
      path,
      `${path} must be an integer between ${String(minimum)} and ${String(maximum)}`,
    )
  }
  return value
}

function int32OrNull(value: unknown, path: string): number | null {
  if (value === null) return null
  return boundedInteger(value, path, -2_147_483_648, 2_147_483_647)
}

function booleanValue(value: unknown, path: string): boolean {
  if (typeof value !== 'boolean') controlError('INVALID_VALUE', path, `${path} must be boolean`)
  return value
}

function enumValue<const Values extends readonly string[]>(
  value: unknown,
  values: Values,
  path: string,
): Values[number] {
  if (typeof value !== 'string' || !values.includes(value)) {
    controlError('INVALID_VALUE', path, `${path} is unsupported`)
  }
  return value as Values[number]
}

function enumList<const Values extends readonly string[]>(
  value: unknown,
  values: Values,
  path: string,
  minimum: number,
): readonly Values[number][] {
  if (!Array.isArray(value) || value.length < minimum || value.length > values.length) {
    controlError(
      'INVALID_VALUE',
      path,
      `${path} must be an array of ${String(minimum)} to ${String(values.length)} permitted values`,
    )
  }
  const entries = value.map((entry, index) => enumValue(entry, values, `${path}[${String(index)}]`))
  if (new Set(entries).size !== entries.length) {
    controlError('DUPLICATE_ID', path, `${path} contains duplicate entries`)
  }
  return Object.freeze(entries)
}

function boundedArray(value: unknown, path: string, maximum: number): readonly unknown[] {
  if (!Array.isArray(value) || value.length > maximum) {
    controlError(
      'INVALID_VALUE',
      path,
      `${path} must be an array with at most ${String(maximum)} entries`,
    )
  }
  return value
}

function conflictArtifactRef(value: unknown, path: string): string | null {
  if (value === null) return null
  return brandedText(
    value,
    path,
    'ConflictArtifactRef',
    CONFLICT_ARTIFACT_REF_PATTERN,
    'an opaque client-local ConflictArtifactRef; never a filesystem path',
  )
}

export const USER_ACCOUNT_ROLES = Object.freeze(['owner', 'member'] as const)
export type UserAccountRole = typeof USER_ACCOUNT_ROLES[number]

export const USER_ACCOUNT_STATES = Object.freeze(['active', 'disabled'] as const)
export type UserAccountState = typeof USER_ACCOUNT_STATES[number]

export const CLIENT_NODE_PRESENCE_STATES = Object.freeze([
  'pending_enrollment',
  'online',
  'degraded',
  'offline',
  'locked',
  'revoked',
] as const)
export type ClientNodePresenceState = typeof CLIENT_NODE_PRESENCE_STATES[number]

export const CLIENT_LOCK_STATES = Object.freeze(['unlocked', 'locked'] as const)
export type ClientLockState = typeof CLIENT_LOCK_STATES[number]

export const CLIENT_PLATFORM_TARGETS = Object.freeze([
  'aarch64-apple-darwin',
  'x86_64-apple-darwin',
  'aarch64-unknown-linux-gnu',
  'x86_64-unknown-linux-gnu',
] as const)
export type ClientPlatformTarget = typeof CLIENT_PLATFORM_TARGETS[number]

export const CLIENT_ARCHITECTURES = Object.freeze(['aarch64', 'x86_64'] as const)
export type ClientArchitecture = typeof CLIENT_ARCHITECTURES[number]

export const CLIENT_CONNECT_CODE_STATES = Object.freeze([
  'active',
  'consumed',
  'expired',
  'revoked',
] as const)
export type ClientConnectCodeState = typeof CLIENT_CONNECT_CODE_STATES[number]

export const CLIENT_ACCESS_GRANT_PERMISSIONS = Object.freeze(['use', 'manage', 'share'] as const)
export type ClientAccessGrantPermission = typeof CLIENT_ACCESS_GRANT_PERMISSIONS[number]

export const CLIENT_TRUST_MODES = Object.freeze(['temporary', 'trusted'] as const)
export type ClientTrustMode = typeof CLIENT_TRUST_MODES[number]

export const CLIENT_ACCESS_GRANT_STATES = Object.freeze([
  'active',
  'revoked',
  'expired',
] as const)
export type ClientAccessGrantState = typeof CLIENT_ACCESS_GRANT_STATES[number]

export const CLIENT_ACCESS_GRANT_SOURCES = Object.freeze([
  'connect_code',
  'administrator',
  'local_confirmation',
] as const)
export type ClientAccessGrantSource = typeof CLIENT_ACCESS_GRANT_SOURCES[number]

export const CLIENT_OCCUPANCY_LEASE_STATES = Object.freeze([
  'available',
  'reserving',
  'occupied',
  'draining',
  'recovery_pending',
  'released',
  'expired',
] as const)
export type ClientOccupancyLeaseState = typeof CLIENT_OCCUPANCY_LEASE_STATES[number]

export const CLIENT_OCCUPANCY_RELEASE_MODES = Object.freeze([
  'immediate',
  'drain_then_release',
  'cancel_tasks_and_release',
] as const)
export type ClientOccupancyReleaseMode = typeof CLIENT_OCCUPANCY_RELEASE_MODES[number]

export const CLIENT_OCCUPANCY_RELEASE_REASONS = Object.freeze([
  'user_requested',
  'cancel_tasks_and_release',
  'drained',
  'idle_timeout',
  'revoked',
  'force_fenced',
] as const)
export type ClientOccupancyReleaseReason = typeof CLIENT_OCCUPANCY_RELEASE_REASONS[number]

export const CLIENT_OCCUPANCY_FORCE_FENCE_REASONS = Object.freeze([
  'recovery_deadline_exceeded',
  'administrator_force_clean',
] as const)
export type ClientOccupancyForceFenceReason = typeof CLIENT_OCCUPANCY_FORCE_FENCE_REASONS[number]

export const OCCUPANCY_REJECT_REASONS = Object.freeze([
  'unknown_lease',
  'stale_fencing_token',
  'local_state_conflict',
  'client_locked',
  'capacity_exhausted',
] as const)
export type OccupancyRejectReason = typeof OCCUPANCY_REJECT_REASONS[number]

export const REPOSITORY_KINDS = Object.freeze(['git'] as const)
export type RepositoryKind = typeof REPOSITORY_KINDS[number]

export const REPOSITORY_DIRTY_STATES = Object.freeze(['clean', 'dirty'] as const)
export type RepositoryDirtyState = typeof REPOSITORY_DIRTY_STATES[number]

export const REPOSITORY_AVAILABILITIES = Object.freeze([
  'available',
  'dirty',
  'unavailable',
  'moved',
  'invalid_git',
  'permission_denied',
  'scan_failed',
] as const)
export type RepositoryAvailability = typeof REPOSITORY_AVAILABILITIES[number]

export const REPOSITORY_ACCESS_PERMISSIONS = Object.freeze(['use', 'manage'] as const)
export type RepositoryAccessPermission = typeof REPOSITORY_ACCESS_PERMISSIONS[number]

export const WORKER_LAUNCH_GRANT_STATES = Object.freeze([
  'issued',
  'consumed',
  'revoked',
  'expired',
] as const)
export type WorkerLaunchGrantState = typeof WORKER_LAUNCH_GRANT_STATES[number]

export const LOCAL_CANDIDATE_RECEIPT_STATES = Object.freeze([
  'retained',
  'branch_created',
  'applied',
  'discarded',
  'failed',
] as const)
export type LocalCandidateReceiptState = typeof LOCAL_CANDIDATE_RECEIPT_STATES[number]

export const LOCAL_APPLY_STRATEGIES = Object.freeze([
  'create_branch',
  'fast_forward',
  'cherry_pick',
  'merge',
] as const)
export type LocalApplyStrategy = typeof LOCAL_APPLY_STRATEGIES[number]

export const LOCAL_APPLY_RESULTS = Object.freeze([
  'retained',
  'branch_created',
  'applied',
  'base_stale',
  'working_tree_dirty',
  'merge_conflict',
  'candidate_missing',
  'permission_denied',
  'discarded',
  'failed',
] as const)
export type LocalApplyResult = typeof LOCAL_APPLY_RESULTS[number]

export const WORKER_RECONCILE_STATES = Object.freeze([
  'still_running',
  'terminal',
  'missing',
  'unknown',
] as const)
export type WorkerReconcileState = typeof WORKER_RECONCILE_STATES[number]

export const CLIENT_WORKER_RUN_STATES = Object.freeze([
  'starting',
  'running',
  'draining',
  'stopping',
  'stopped',
  'crashed',
  'missing',
  'unknown',
] as const)
export type ClientWorkerRunState = typeof CLIENT_WORKER_RUN_STATES[number]

export const CLIENT_CHALLENGE_ACK_STATUSES = Object.freeze([
  'confirmed',
  'stale_generation',
] as const)
export type ClientChallengeAckStatus = typeof CLIENT_CHALLENGE_ACK_STATUSES[number]

export const CLIENT_CREDENTIAL_ROTATE_REASONS = Object.freeze([
  'scheduled',
  'suspected_compromise',
] as const)
export type ClientCredentialRotateReason = typeof CLIENT_CREDENTIAL_ROTATE_REASONS[number]

export const CLIENT_REPOSITORY_RESCAN_REASONS = Object.freeze([
  'occupant_requested',
  'occupancy_recovered',
  'policy',
] as const)
export type ClientRepositoryRescanReason = typeof CLIENT_REPOSITORY_RESCAN_REASONS[number]

export const CLIENT_WORKER_STOP_REASONS = Object.freeze([
  'occupant_requested',
  'draining_complete',
  'lease_recovered',
  'grant_revoked',
  'superseded',
] as const)
export type ClientWorkerStopReason = typeof CLIENT_WORKER_STOP_REASONS[number]

export const COMMAND_ACK_STATUSES = Object.freeze([
  'accepted',
  'duplicate',
  'rejected_unknown_command',
  'rejected_revision_conflict',
  'rejected_stale_fencing_token',
  'rejected_lease_mismatch',
  'rejected_wrong_state',
  'rejected_unauthorized',
  'rejected_capacity_exhausted',
] as const)
export type CommandAckStatus = typeof COMMAND_ACK_STATUSES[number]

export const WORKER_LAUNCH_ACK_STATUSES = Object.freeze([
  'accepted',
  'duplicate',
  'rejected_stale_fencing_token',
  'rejected_lease_mismatch',
  'rejected_capacity_exhausted',
  'rejected_repository_unavailable',
  'rejected_unknown_grant',
  'rejected_wrong_state',
] as const)
export type WorkerLaunchAckStatus = typeof WORKER_LAUNCH_ACK_STATUSES[number]

export const CLIENT_CONTROL_ERROR_CODES = Object.freeze([
  'PROTOCOL_VERSION_UNSUPPORTED',
  'DEVICE_NOT_ENROLLED',
  'DEVICE_INSTANCE_CHANGED',
  'UNKNOWN_LEASE',
  'STALE_FENCING_TOKEN',
  'REVISION_CONFLICT',
  'IDEMPOTENCY_CONFLICT',
  'CAPACITY_EXHAUSTED',
  'REPOSITORY_UNAVAILABLE',
  'GRANT_INVALID',
  'WRONG_STATE',
  'RATE_LIMITED',
  'INTERNAL_ERROR',
] as const)
export type ClientControlErrorCode = typeof CLIENT_CONTROL_ERROR_CODES[number]

/**
 * `UserAccount`. `passwordHash` is a Control-Plane-only secret: it is part of
 * the domain object for completeness and never crosses ClientControlPort.
 */
export interface UserAccount {
  readonly userId: UserId
  readonly username: string
  readonly normalizedUsername: string
  readonly passwordHash: string
  readonly role: UserAccountRole
  readonly state: UserAccountState
  readonly createdAt: Instant
  readonly updatedAt: Instant
  readonly revision: Revision
}

/**
 * `ClientNode`. `publicClientId` is a stable, non-secret lookup id. This
 * projection never contains local filesystem paths.
 */
export interface ClientNode {
  readonly clientNodeId: ClientNodeId
  readonly publicClientId: PublicClientId
  readonly displayName: string
  readonly platform: ClientPlatformTarget
  readonly architecture: ClientArchitecture
  readonly clientVersion: ClientVersion
  readonly deviceCredentialDigest: Sha256Digest
  readonly currentInstanceId: ClientInstanceId
  readonly presenceState: ClientNodePresenceState
  readonly acceptingConnections: boolean
  readonly lockState: ClientLockState
  readonly maxConcurrentWorkerSessions: number
  readonly reportedRunningWorkerSessions: number
  readonly lastHeartbeatAt: Instant
  readonly createdAt: Instant
  readonly revision: Revision
}

/**
 * `ClientConnectCode`. Only `codeDigest` is stored or transported; the
 * plaintext code never appears in a contract, log, or projection.
 */
export interface ClientConnectCode {
  readonly connectCodeId: ClientConnectCodeId
  readonly clientNodeId: ClientNodeId
  readonly codeDigest: Sha256Digest
  readonly issuedByInstanceId: ClientInstanceId
  readonly expiresAt: Instant
  readonly remainingAttempts: number
  readonly state: ClientConnectCodeState
  readonly createdAt: Instant
  readonly revision: Revision
}

/** `ClientAccessGrant`. Many users may hold an active grant for one Client. */
export interface ClientAccessGrant {
  readonly clientAccessGrantId: ClientAccessGrantId
  readonly clientNodeId: ClientNodeId
  readonly userId: UserId
  readonly permissions: readonly ClientAccessGrantPermission[]
  readonly trustMode: ClientTrustMode
  readonly state: ClientAccessGrantState
  readonly grantedByUserId: UserId
  readonly grantSource: ClientAccessGrantSource
  readonly expiresAt: Instant | null
  readonly createdAt: Instant
  readonly revision: Revision
}

/**
 * `ClientOccupancyLease`. At most one active lease per ClientNode. The
 * `fencingToken` is monotonic per ClientNode; stale tokens are always rejected.
 */
export interface ClientOccupancyLease {
  readonly clientOccupancyLeaseId: ClientOccupancyLeaseId
  readonly clientNodeId: ClientNodeId
  readonly holderUserId: UserId
  readonly state: ClientOccupancyLeaseState
  readonly fencingToken: OccupancyFencingToken
  readonly claimRequestId: OccupancyClaimId
  readonly claimedAt: Instant
  readonly acknowledgedAt: Instant | null
  readonly lastRenewedAt: Instant | null
  readonly idleExpiresAt: Instant | null
  readonly recoveryDeadlineAt: Instant | null
  readonly releaseRequestedAt: Instant | null
  readonly releasedAt: Instant | null
  readonly releaseReason: ClientOccupancyReleaseReason | null
  readonly revision: Revision
}

/**
 * `RepositoryBinding`. Security metadata projection only: this object has no
 * path field by contract. The binding-to-path mapping lives exclusively in the
 * Device Client local database and is never uploaded.
 */
export interface RepositoryBinding {
  readonly repositoryBindingId: RepositoryBindingId
  readonly clientNodeId: ClientNodeId
  readonly displayName: string
  readonly repositoryKind: RepositoryKind
  readonly defaultBranch: GitRefName
  readonly headCommit: GitCommitSha
  readonly dirtyState: RepositoryDirtyState
  readonly availability: RepositoryAvailability
  readonly repositoryFingerprint: Sha256Digest
  readonly lastScannedAt: Instant | null
  readonly revision: Revision
}

/**
 * `RepositoryBindingProjection`. The secret-safe repository metadata a Device
 * Client reports on repository.upsert; no clientNodeId, revision, or path.
 */
export interface RepositoryBindingProjection {
  readonly repositoryBindingId: RepositoryBindingId
  readonly displayName: string
  readonly repositoryKind: RepositoryKind
  readonly defaultBranch: GitRefName
  readonly headCommit: GitCommitSha
  readonly dirtyState: RepositoryDirtyState
  readonly availability: RepositoryAvailability
  readonly repositoryFingerprint: Sha256Digest
  readonly lastScannedAt: Instant
}

/** `RepositoryAccessGrant`. Visibility requires an active grant per user. */
export interface RepositoryAccessGrant {
  readonly repositoryAccessGrantId: RepositoryAccessGrantId
  readonly repositoryBindingId: RepositoryBindingId
  readonly userId: UserId
  readonly permissions: readonly RepositoryAccessPermission[]
  readonly state: typeof REPOSITORY_ACCESS_GRANT_STATES[number]
  readonly grantedByUserId: UserId
  readonly createdAt: Instant
  readonly revision: Revision
}

export const REPOSITORY_ACCESS_GRANT_STATES = Object.freeze(['active', 'revoked'] as const)
export type RepositoryAccessGrantState = typeof REPOSITORY_ACCESS_GRANT_STATES[number]

/**
 * `WorkerLaunchGrant`. Every identity field must match on launch; a mismatch
 * is rejected. Occupancy fields bind the grant to one fenced lease.
 */
export interface WorkerLaunchGrant {
  readonly workerLaunchGrantId: WorkerLaunchGrantId
  readonly clientNodeId: ClientNodeId
  readonly clientInstanceId: ClientInstanceId
  readonly occupancyLeaseId: ClientOccupancyLeaseId
  readonly occupancyFencingToken: OccupancyFencingToken
  readonly repositoryBindingId: RepositoryBindingId
  readonly productSessionId: ProductSessionId
  readonly stageRunId: StageRunIdentifier
  readonly workerSessionId: WorkerSessionId
  readonly workerId: WorkerId
  readonly workerInstanceId: WorkerInstanceId
  /** Digest of the short-lived worker session credential; never the credential. */
  readonly credentialDigest: Sha256Digest
  readonly expiresAt: Instant
  readonly state: WorkerLaunchGrantState
  readonly revision: Revision
}

/** `LocalCandidateReceipt`. Candidate facts survive local worktree cleanup. */
export interface LocalCandidateReceipt {
  readonly localCandidateReceiptId: LocalCandidateReceiptId
  readonly candidateRef: CandidateRef
  readonly repositoryBindingId: RepositoryBindingId
  readonly candidateCommit: GitCommitSha
  readonly localRefName: GitRefName
  readonly state: LocalCandidateReceiptState
  readonly createdAt: Instant
  readonly revision: Revision
}

/** `LocalApplyReceipt`. One audited attempt to land a Candidate locally. */
export interface LocalApplyReceipt {
  readonly localApplyReceiptId: LocalApplyReceiptId
  readonly candidateRef: CandidateRef
  readonly repositoryBindingId: RepositoryBindingId
  readonly targetBranch: GitRefName
  readonly expectedHead: GitCommitSha
  readonly strategy: LocalApplyStrategy
  readonly result: LocalApplyResult
  readonly resultingCommit: GitCommitSha | null
  readonly conflictArtifactRef: string | null
  readonly createdAt: Instant
  readonly revision: Revision
}

/** `ClientCapacityReport`. Client-reported worker session capacity. */
export interface ClientCapacityReport {
  readonly maxConcurrentWorkerSessions: number
  readonly runningWorkerSessions: number
  readonly reservedWorkerSessions: number
  readonly drainingWorkerSessions: number
}

/** `ClientWorkerReconciliation`. One worker verdict after a Client restart. */
export interface ClientWorkerReconciliation {
  readonly workerSessionId: WorkerSessionId
  readonly workerInstanceId: WorkerInstanceId
  readonly reconcileState: WorkerReconcileState
  readonly observedAt: Instant
}

/**
 * `ClientControlError`. Machine-readable error fact whose optional details
 * reuse the public redaction rules: authority-sensitive property names are
 * rejected recursively.
 */
export interface ClientControlError {
  readonly code: ClientControlErrorCode
  readonly message: string
  readonly retryable: boolean
  readonly details?: Readonly<Record<string, unknown>>
}

const ERROR_DETAIL_FORBIDDEN_KEYS = Object.freeze([
  'accessToken',
  'agentGraph',
  'apiKey',
  'codexPlan',
  'database',
  'databaseRow',
  'deliveryPatch',
  'deliveryVerdict',
  'password',
  'providerCredential',
  'rawProviderRequest',
  'rawProviderResponse',
  'secret',
  'secretMaterial',
  'socketPath',
  'sql',
  'table',
  'toolPayload',
  'turn',
  'vaultLocator',
] as const)

function parseErrorDetailValue(value: unknown, path: string): unknown {
  if (value === null) return null
  if (typeof value === 'boolean' || typeof value === 'number' || typeof value === 'string') {
    if (typeof value === 'number' && !Number.isFinite(value)) {
      controlError('INVALID_VALUE', path, `${path} must be a JSON-safe number`)
    }
    return value
  }
  if (Array.isArray(value)) {
    return Object.freeze(value.map((entry, index) => (
      parseErrorDetailValue(entry, `${path}[${String(index)}]`)
    )))
  }
  return parseErrorDetails(value, path)
}

function parseErrorDetails(
  value: unknown,
  path: string,
): Readonly<Record<string, unknown>> {
  const input = record(value, path)
  const details: Record<string, unknown> = {}
  for (const key of Object.keys(input)) {
    if (ERROR_DETAIL_FORBIDDEN_KEYS.includes(key as ClientControlErrorDetailForbiddenKey)) {
      controlError(
        'INVALID_VALUE',
        `${path}.${key}`,
        `${path}.${key} is an authority-sensitive property name rejected at the public boundary`,
      )
    }
    details[key] = parseErrorDetailValue(input[key], `${path}.${key}`)
  }
  return Object.freeze(details)
}

type ClientControlErrorDetailForbiddenKey = typeof ERROR_DETAIL_FORBIDDEN_KEYS[number]

export function parseClientControlError(value: unknown, path = 'clientControlError'): ClientControlError {
  const input = record(value, path)
  exactKeys(input, ['code', 'message', 'retryable'], path, ['details'])
  const parsed: {
    code: ClientControlErrorCode
    message: string
    retryable: boolean
    details?: Readonly<Record<string, unknown>>
  } = {
    code: enumValue(input.code, CLIENT_CONTROL_ERROR_CODES, `${path}.code`),
    message: boundedErrorText(input.message, `${path}.message`),
    retryable: booleanValue(input.retryable, `${path}.retryable`),
  }
  if (Object.hasOwn(input, 'details')) {
    parsed.details = parseErrorDetails(input.details, `${path}.details`)
  }
  return Object.freeze(parsed)
}

function boundedErrorText(value: unknown, path: string): string {
  if (typeof value !== 'string'
    || value.length === 0
    || value.length > MAX_ERROR_MESSAGE_LENGTH) {
    controlError(
      'INVALID_VALUE',
      path,
      `${path} must be 1 to ${String(MAX_ERROR_MESSAGE_LENGTH)} characters of error text`,
    )
  }
  return value
}

export function parseUserAccount(value: unknown, path = 'userAccount'): UserAccount {
  const input = record(value, path)
  exactKeys(input, [
    'userId',
    'username',
    'normalizedUsername',
    'passwordHash',
    'role',
    'state',
    'createdAt',
    'updatedAt',
    'revision',
  ], path)
  return Object.freeze({
    userId: USER_ID(input.userId, `${path}.userId`),
    username: brandedText(
      input.username,
      `${path}.username`,
      'Username',
      USERNAME_PATTERN,
      'a username of 1 to 64 portable characters',
    ),
    normalizedUsername: brandedText(
      input.normalizedUsername,
      `${path}.normalizedUsername`,
      'NormalizedUsername',
      NORMALIZED_USERNAME_PATTERN,
      'a lowercase normalized username of 1 to 64 portable characters',
    ),
    passwordHash: brandedText(
      input.passwordHash,
      `${path}.passwordHash`,
      'PasswordHash',
      PASSWORD_HASH_PATTERN,
      'an Argon2id PHC password hash; server-side only, never transported',
    ),
    role: enumValue(input.role, USER_ACCOUNT_ROLES, `${path}.role`),
    state: enumValue(input.state, USER_ACCOUNT_STATES, `${path}.state`),
    createdAt: instant(input.createdAt, `${path}.createdAt`),
    updatedAt: instant(input.updatedAt, `${path}.updatedAt`),
    revision: revision(input.revision, `${path}.revision`),
  })
}

export function parseClientNode(value: unknown, path = 'clientNode'): ClientNode {
  const input = record(value, path)
  exactKeys(input, [
    'clientNodeId',
    'publicClientId',
    'displayName',
    'platform',
    'architecture',
    'clientVersion',
    'deviceCredentialDigest',
    'currentInstanceId',
    'presenceState',
    'acceptingConnections',
    'lockState',
    'maxConcurrentWorkerSessions',
    'reportedRunningWorkerSessions',
    'lastHeartbeatAt',
    'createdAt',
    'revision',
  ], path)
  return Object.freeze({
    clientNodeId: CLIENT_NODE_ID(input.clientNodeId, `${path}.clientNodeId`),
    publicClientId: PUBLIC_CLIENT_ID(input.publicClientId, `${path}.publicClientId`),
    displayName: clientDisplayName(input.displayName, `${path}.displayName`),
    platform: enumValue(input.platform, CLIENT_PLATFORM_TARGETS, `${path}.platform`),
    architecture: enumValue(input.architecture, CLIENT_ARCHITECTURES, `${path}.architecture`),
    clientVersion: clientVersion(input.clientVersion, `${path}.clientVersion`),
    deviceCredentialDigest: SHA256_DIGEST(
      input.deviceCredentialDigest,
      `${path}.deviceCredentialDigest`,
    ),
    currentInstanceId: CLIENT_INSTANCE_ID(
      input.currentInstanceId,
      `${path}.currentInstanceId`,
    ),
    presenceState: enumValue(input.presenceState, CLIENT_NODE_PRESENCE_STATES, `${path}.presenceState`),
    acceptingConnections: booleanValue(input.acceptingConnections, `${path}.acceptingConnections`),
    lockState: enumValue(input.lockState, CLIENT_LOCK_STATES, `${path}.lockState`),
    maxConcurrentWorkerSessions: boundedInteger(
      input.maxConcurrentWorkerSessions,
      `${path}.maxConcurrentWorkerSessions`,
      0,
      1_024,
    ),
    reportedRunningWorkerSessions: boundedInteger(
      input.reportedRunningWorkerSessions,
      `${path}.reportedRunningWorkerSessions`,
      0,
      1_024,
    ),
    lastHeartbeatAt: instant(input.lastHeartbeatAt, `${path}.lastHeartbeatAt`),
    createdAt: instant(input.createdAt, `${path}.createdAt`),
    revision: revision(input.revision, `${path}.revision`),
  })
}

export function parseClientConnectCode(value: unknown, path = 'clientConnectCode'): ClientConnectCode {
  const input = record(value, path)
  exactKeys(input, [
    'connectCodeId',
    'clientNodeId',
    'codeDigest',
    'issuedByInstanceId',
    'expiresAt',
    'remainingAttempts',
    'state',
    'createdAt',
    'revision',
  ], path)
  return Object.freeze({
    connectCodeId: CLIENT_CONNECT_CODE_ID(input.connectCodeId, `${path}.connectCodeId`),
    clientNodeId: CLIENT_NODE_ID(input.clientNodeId, `${path}.clientNodeId`),
    codeDigest: SHA256_DIGEST(input.codeDigest, `${path}.codeDigest`),
    issuedByInstanceId: CLIENT_INSTANCE_ID(
      input.issuedByInstanceId,
      `${path}.issuedByInstanceId`,
    ),
    expiresAt: instant(input.expiresAt, `${path}.expiresAt`),
    remainingAttempts: boundedInteger(input.remainingAttempts, `${path}.remainingAttempts`, 0, 100),
    state: enumValue(input.state, CLIENT_CONNECT_CODE_STATES, `${path}.state`),
    createdAt: instant(input.createdAt, `${path}.createdAt`),
    revision: revision(input.revision, `${path}.revision`),
  })
}

export function parseClientAccessGrant(value: unknown, path = 'clientAccessGrant'): ClientAccessGrant {
  const input = record(value, path)
  exactKeys(input, [
    'clientAccessGrantId',
    'clientNodeId',
    'userId',
    'permissions',
    'trustMode',
    'state',
    'grantedByUserId',
    'grantSource',
    'expiresAt',
    'createdAt',
    'revision',
  ], path)
  return Object.freeze({
    clientAccessGrantId: CLIENT_ACCESS_GRANT_ID(
      input.clientAccessGrantId,
      `${path}.clientAccessGrantId`,
    ),
    clientNodeId: CLIENT_NODE_ID(input.clientNodeId, `${path}.clientNodeId`),
    userId: USER_ID(input.userId, `${path}.userId`),
    permissions: enumList(
      input.permissions,
      CLIENT_ACCESS_GRANT_PERMISSIONS,
      `${path}.permissions`,
      1,
    ),
    trustMode: enumValue(input.trustMode, CLIENT_TRUST_MODES, `${path}.trustMode`),
    state: enumValue(input.state, CLIENT_ACCESS_GRANT_STATES, `${path}.state`),
    grantedByUserId: USER_ID(input.grantedByUserId, `${path}.grantedByUserId`),
    grantSource: enumValue(input.grantSource, CLIENT_ACCESS_GRANT_SOURCES, `${path}.grantSource`),
    expiresAt: nullable(input.expiresAt, `${path}.expiresAt`, instant),
    createdAt: instant(input.createdAt, `${path}.createdAt`),
    revision: revision(input.revision, `${path}.revision`),
  })
}

export function parseClientOccupancyLease(
  value: unknown,
  path = 'clientOccupancyLease',
): ClientOccupancyLease {
  const input = record(value, path)
  exactKeys(input, [
    'clientOccupancyLeaseId',
    'clientNodeId',
    'holderUserId',
    'state',
    'fencingToken',
    'claimRequestId',
    'claimedAt',
    'acknowledgedAt',
    'lastRenewedAt',
    'idleExpiresAt',
    'recoveryDeadlineAt',
    'releaseRequestedAt',
    'releasedAt',
    'releaseReason',
    'revision',
  ], path)
  return Object.freeze({
    clientOccupancyLeaseId: CLIENT_OCCUPANCY_LEASE_ID(
      input.clientOccupancyLeaseId,
      `${path}.clientOccupancyLeaseId`,
    ),
    clientNodeId: CLIENT_NODE_ID(input.clientNodeId, `${path}.clientNodeId`),
    holderUserId: USER_ID(input.holderUserId, `${path}.holderUserId`),
    state: enumValue(input.state, CLIENT_OCCUPANCY_LEASE_STATES, `${path}.state`),
    fencingToken: OCCUPANCY_FENCING_TOKEN(input.fencingToken, `${path}.fencingToken`),
    claimRequestId: OCCUPANCY_CLAIM_ID(input.claimRequestId, `${path}.claimRequestId`),
    claimedAt: instant(input.claimedAt, `${path}.claimedAt`),
    acknowledgedAt: nullable(input.acknowledgedAt, `${path}.acknowledgedAt`, instant),
    lastRenewedAt: nullable(input.lastRenewedAt, `${path}.lastRenewedAt`, instant),
    idleExpiresAt: nullable(input.idleExpiresAt, `${path}.idleExpiresAt`, instant),
    recoveryDeadlineAt: nullable(input.recoveryDeadlineAt, `${path}.recoveryDeadlineAt`, instant),
    releaseRequestedAt: nullable(input.releaseRequestedAt, `${path}.releaseRequestedAt`, instant),
    releasedAt: nullable(input.releasedAt, `${path}.releasedAt`, instant),
    releaseReason: enumValueOrNull(input.releaseReason, CLIENT_OCCUPANCY_RELEASE_REASONS, `${path}.releaseReason`),
    revision: revision(input.revision, `${path}.revision`),
  })
}

function enumValueOrNull<Values extends readonly string[]>(
  value: unknown,
  values: Values,
  path: string,
): Values[number] | null {
  return value === null ? null : enumValue(value, values, path)
}

export function parseRepositoryBinding(value: unknown, path = 'repositoryBinding'): RepositoryBinding {
  const input = record(value, path)
  exactKeys(input, [
    'repositoryBindingId',
    'clientNodeId',
    'displayName',
    'repositoryKind',
    'defaultBranch',
    'headCommit',
    'dirtyState',
    'availability',
    'repositoryFingerprint',
    'lastScannedAt',
    'revision',
  ], path)
  return Object.freeze({
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
    clientNodeId: CLIENT_NODE_ID(input.clientNodeId, `${path}.clientNodeId`),
    displayName: clientDisplayName(input.displayName, `${path}.displayName`),
    repositoryKind: enumValue(input.repositoryKind, REPOSITORY_KINDS, `${path}.repositoryKind`),
    defaultBranch: GIT_REF_NAME(input.defaultBranch, `${path}.defaultBranch`),
    headCommit: GIT_COMMIT_SHA(input.headCommit, `${path}.headCommit`),
    dirtyState: enumValue(input.dirtyState, REPOSITORY_DIRTY_STATES, `${path}.dirtyState`),
    availability: enumValue(input.availability, REPOSITORY_AVAILABILITIES, `${path}.availability`),
    repositoryFingerprint: SHA256_DIGEST(
      input.repositoryFingerprint,
      `${path}.repositoryFingerprint`,
    ),
    lastScannedAt: nullable(input.lastScannedAt, `${path}.lastScannedAt`, instant),
    revision: revision(input.revision, `${path}.revision`),
  })
}

export function parseRepositoryBindingProjection(
  value: unknown,
  path = 'repositoryBindingProjection',
): RepositoryBindingProjection {
  const input = record(value, path)
  exactKeys(input, [
    'repositoryBindingId',
    'displayName',
    'repositoryKind',
    'defaultBranch',
    'headCommit',
    'dirtyState',
    'availability',
    'repositoryFingerprint',
    'lastScannedAt',
  ], path)
  return Object.freeze({
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
    displayName: clientDisplayName(input.displayName, `${path}.displayName`),
    repositoryKind: enumValue(input.repositoryKind, REPOSITORY_KINDS, `${path}.repositoryKind`),
    defaultBranch: GIT_REF_NAME(input.defaultBranch, `${path}.defaultBranch`),
    headCommit: GIT_COMMIT_SHA(input.headCommit, `${path}.headCommit`),
    dirtyState: enumValue(input.dirtyState, REPOSITORY_DIRTY_STATES, `${path}.dirtyState`),
    availability: enumValue(input.availability, REPOSITORY_AVAILABILITIES, `${path}.availability`),
    repositoryFingerprint: SHA256_DIGEST(
      input.repositoryFingerprint,
      `${path}.repositoryFingerprint`,
    ),
    lastScannedAt: instant(input.lastScannedAt, `${path}.lastScannedAt`),
  })
}

export function parseRepositoryAccessGrant(
  value: unknown,
  path = 'repositoryAccessGrant',
): RepositoryAccessGrant {
  const input = record(value, path)
  exactKeys(input, [
    'repositoryAccessGrantId',
    'repositoryBindingId',
    'userId',
    'permissions',
    'state',
    'grantedByUserId',
    'createdAt',
    'revision',
  ], path)
  return Object.freeze({
    repositoryAccessGrantId: REPOSITORY_ACCESS_GRANT_ID(
      input.repositoryAccessGrantId,
      `${path}.repositoryAccessGrantId`,
    ),
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
    userId: USER_ID(input.userId, `${path}.userId`),
    permissions: enumList(
      input.permissions,
      REPOSITORY_ACCESS_PERMISSIONS,
      `${path}.permissions`,
      1,
    ),
    state: enumValue(input.state, REPOSITORY_ACCESS_GRANT_STATES, `${path}.state`),
    grantedByUserId: USER_ID(input.grantedByUserId, `${path}.grantedByUserId`),
    createdAt: instant(input.createdAt, `${path}.createdAt`),
    revision: revision(input.revision, `${path}.revision`),
  })
}

export function parseWorkerLaunchGrant(value: unknown, path = 'workerLaunchGrant'): WorkerLaunchGrant {
  const input = record(value, path)
  exactKeys(input, [
    'workerLaunchGrantId',
    'clientNodeId',
    'clientInstanceId',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'repositoryBindingId',
    'productSessionId',
    'stageRunId',
    'workerSessionId',
    'workerId',
    'workerInstanceId',
    'credentialDigest',
    'expiresAt',
    'state',
    'revision',
  ], path)
  return Object.freeze({
    workerLaunchGrantId: WORKER_LAUNCH_GRANT_ID(
      input.workerLaunchGrantId,
      `${path}.workerLaunchGrantId`,
    ),
    clientNodeId: CLIENT_NODE_ID(input.clientNodeId, `${path}.clientNodeId`),
    clientInstanceId: CLIENT_INSTANCE_ID(
      input.clientInstanceId,
      `${path}.clientInstanceId`,
    ),
    occupancyLeaseId: CLIENT_OCCUPANCY_LEASE_ID(
      input.occupancyLeaseId,
      `${path}.occupancyLeaseId`,
    ),
    occupancyFencingToken: OCCUPANCY_FENCING_TOKEN(
      input.occupancyFencingToken,
      `${path}.occupancyFencingToken`,
    ),
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
    productSessionId: PRODUCT_SESSION_ID(input.productSessionId, `${path}.productSessionId`),
    stageRunId: STAGE_RUN_ID(input.stageRunId, `${path}.stageRunId`),
    workerSessionId: WORKER_SESSION_ID(input.workerSessionId, `${path}.workerSessionId`),
    workerId: WORKER_ID(input.workerId, `${path}.workerId`),
    workerInstanceId: WORKER_INSTANCE_ID(input.workerInstanceId, `${path}.workerInstanceId`),
    credentialDigest: SHA256_DIGEST(input.credentialDigest, `${path}.credentialDigest`),
    expiresAt: instant(input.expiresAt, `${path}.expiresAt`),
    state: enumValue(input.state, WORKER_LAUNCH_GRANT_STATES, `${path}.state`),
    revision: revision(input.revision, `${path}.revision`),
  })
}

export function parseLocalCandidateReceipt(
  value: unknown,
  path = 'localCandidateReceipt',
): LocalCandidateReceipt {
  const input = record(value, path)
  exactKeys(input, [
    'localCandidateReceiptId',
    'candidateRef',
    'repositoryBindingId',
    'candidateCommit',
    'localRefName',
    'state',
    'createdAt',
    'revision',
  ], path)
  return Object.freeze({
    localCandidateReceiptId: LOCAL_CANDIDATE_RECEIPT_ID(
      input.localCandidateReceiptId,
      `${path}.localCandidateReceiptId`,
    ),
    candidateRef: CANDIDATE_REF(input.candidateRef, `${path}.candidateRef`),
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
    candidateCommit: GIT_COMMIT_SHA(input.candidateCommit, `${path}.candidateCommit`),
    localRefName: GIT_REF_NAME(input.localRefName, `${path}.localRefName`),
    state: enumValue(input.state, LOCAL_CANDIDATE_RECEIPT_STATES, `${path}.state`),
    createdAt: instant(input.createdAt, `${path}.createdAt`),
    revision: revision(input.revision, `${path}.revision`),
  })
}

export function parseLocalApplyReceipt(value: unknown, path = 'localApplyReceipt'): LocalApplyReceipt {
  const input = record(value, path)
  exactKeys(input, [
    'localApplyReceiptId',
    'candidateRef',
    'repositoryBindingId',
    'targetBranch',
    'expectedHead',
    'strategy',
    'result',
    'resultingCommit',
    'conflictArtifactRef',
    'createdAt',
    'revision',
  ], path)
  return Object.freeze({
    localApplyReceiptId: LOCAL_APPLY_RECEIPT_ID(
      input.localApplyReceiptId,
      `${path}.localApplyReceiptId`,
    ),
    candidateRef: CANDIDATE_REF(input.candidateRef, `${path}.candidateRef`),
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
    targetBranch: GIT_REF_NAME(input.targetBranch, `${path}.targetBranch`),
    expectedHead: GIT_COMMIT_SHA(input.expectedHead, `${path}.expectedHead`),
    strategy: enumValue(input.strategy, LOCAL_APPLY_STRATEGIES, `${path}.strategy`),
    result: enumValue(input.result, LOCAL_APPLY_RESULTS, `${path}.result`),
    resultingCommit: nullable(input.resultingCommit, `${path}.resultingCommit`, GIT_COMMIT_SHA),
    conflictArtifactRef: conflictArtifactRef(input.conflictArtifactRef, `${path}.conflictArtifactRef`),
    createdAt: instant(input.createdAt, `${path}.createdAt`),
    revision: revision(input.revision, `${path}.revision`),
  })
}

export function parseClientCapacityReport(
  value: unknown,
  path = 'clientCapacityReport',
): ClientCapacityReport {
  const input = record(value, path)
  exactKeys(input, [
    'maxConcurrentWorkerSessions',
    'runningWorkerSessions',
    'reservedWorkerSessions',
    'drainingWorkerSessions',
  ], path)
  return Object.freeze({
    maxConcurrentWorkerSessions: boundedInteger(
      input.maxConcurrentWorkerSessions,
      `${path}.maxConcurrentWorkerSessions`,
      0,
      1_024,
    ),
    runningWorkerSessions: boundedInteger(
      input.runningWorkerSessions,
      `${path}.runningWorkerSessions`,
      0,
      1_024,
    ),
    reservedWorkerSessions: boundedInteger(
      input.reservedWorkerSessions,
      `${path}.reservedWorkerSessions`,
      0,
      1_024,
    ),
    drainingWorkerSessions: boundedInteger(
      input.drainingWorkerSessions,
      `${path}.drainingWorkerSessions`,
      0,
      1_024,
    ),
  })
}

export function parseClientWorkerReconciliation(
  value: unknown,
  path = 'clientWorkerReconciliation',
): ClientWorkerReconciliation {
  const input = record(value, path)
  exactKeys(input, ['workerSessionId', 'workerInstanceId', 'reconcileState', 'observedAt'], path)
  return Object.freeze({
    workerSessionId: WORKER_SESSION_ID(input.workerSessionId, `${path}.workerSessionId`),
    workerInstanceId: WORKER_INSTANCE_ID(input.workerInstanceId, `${path}.workerInstanceId`),
    reconcileState: enumValue(input.reconcileState, WORKER_RECONCILE_STATES, `${path}.reconcileState`),
    observedAt: instant(input.observedAt, `${path}.observedAt`),
  })
}

/**
 * Envelope identity shared by every message. The schema inlines these fields
 * into each message; command and fencing fields are siblings of `kind`, never
 * members of a payload sub-object.
 */
export interface ClientControlMessageEnvelopeFields {
  readonly schemaVersion: SchemaVersion
  readonly messageId: ClientControlMessageId
  readonly clientNodeId: ClientNodeId
  readonly clientInstanceId: ClientInstanceId
  readonly sequence: ClientExchangeSequence
  readonly occurredAt: Instant
}

/**
 * Command base carried by exactly the 19 command-class messages: an
 * optimistic-concurrency Revision plus a replay-safe idempotency key.
 */
export interface ClientControlCommandFields {
  readonly expectedRevision: Revision
  readonly idempotencyKey: IdempotencyKey
}

/**
 * Occupancy stamp carried by exactly 11 command-class messages. The pair binds
 * the command to the single active lease; the Device Client must reject every
 * command whose fencing token is not the current maximum.
 */
export interface ClientControlFencedCommandFields extends ClientControlCommandFields {
  readonly occupancyLeaseId: ClientOccupancyLeaseId
  readonly occupancyFencingToken: OccupancyFencingToken
}

export interface ClientEnrollMessage extends ClientControlMessageEnvelopeFields, ClientControlCommandFields {
  readonly kind: 'client.enroll'
  readonly displayName: string
  readonly platform: ClientPlatformTarget
  readonly architecture: ClientArchitecture
  readonly clientVersion: ClientVersion
}

export interface ClientHelloMessage extends ClientControlMessageEnvelopeFields {
  readonly kind: 'client.hello'
  readonly clientVersion: ClientVersion
  readonly presenceState: ClientNodePresenceState
  readonly acceptingConnections: boolean
  readonly lockState: ClientLockState
  readonly capacity: ClientCapacityReport
}

export interface ClientHeartbeatMessage extends ClientControlMessageEnvelopeFields {
  readonly kind: 'client.heartbeat'
  readonly presenceState: ClientNodePresenceState
  readonly acceptingConnections: boolean
  readonly lockState: ClientLockState
  readonly capacity: ClientCapacityReport
  readonly occupancyLeaseId: ClientOccupancyLeaseId | null
}

export interface ClientConnectCodePublishedMessage extends ClientControlMessageEnvelopeFields, ClientControlCommandFields {
  readonly kind: 'client.connect_code.published'
  readonly connectCodeId: ClientConnectCodeId
  readonly codeDigest: Sha256Digest
  readonly expiresAt: Instant
}

export interface ClientAccessChallengeAckMessage extends ClientControlMessageEnvelopeFields, ClientControlCommandFields {
  readonly kind: 'client.access.challenge_ack'
  readonly challengeId: ClientAccessChallengeId
  readonly connectCodeId: ClientConnectCodeId
  readonly status: ClientChallengeAckStatus
}

export interface ClientOccupancyAckMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.occupancy.ack'
}

export interface ClientOccupancyRejectedMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.occupancy.rejected'
  readonly reason: OccupancyRejectReason
}

export interface ClientRepositoryUpsertMessage extends ClientControlMessageEnvelopeFields, ClientControlCommandFields {
  readonly kind: 'client.repository.upsert'
  readonly repository: RepositoryBindingProjection
}

export interface ClientRepositoryRemovedMessage extends ClientControlMessageEnvelopeFields, ClientControlCommandFields {
  readonly kind: 'client.repository.removed'
  readonly repositoryBindingId: RepositoryBindingId
}

export interface ClientRepositoryStatusMessage extends ClientControlMessageEnvelopeFields {
  readonly kind: 'client.repository.status'
  readonly repositoryBindingId: RepositoryBindingId
  readonly availability: RepositoryAvailability
  readonly dirtyState: RepositoryDirtyState
  readonly headCommit: GitCommitSha
  readonly lastScannedAt: Instant
}

export interface ClientWorkerLaunchAckMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.worker.launch_ack'
  readonly workerLaunchGrantId: WorkerLaunchGrantId
  readonly workerSessionId: WorkerSessionId
  readonly workerId: WorkerId
  readonly workerInstanceId: WorkerInstanceId
  readonly status: WorkerLaunchAckStatus
  readonly error?: ClientControlError
}

export interface ClientWorkerStateMessage extends ClientControlMessageEnvelopeFields {
  readonly kind: 'client.worker.state'
  readonly workerSessionId: WorkerSessionId
  readonly workerInstanceId: WorkerInstanceId
  readonly occupancyLeaseId: ClientOccupancyLeaseId | null
  readonly state: ClientWorkerRunState
  readonly observedAt: Instant
  readonly exitCode?: number | null
}

export interface ClientWorkerReconcileMessage extends ClientControlMessageEnvelopeFields {
  readonly kind: 'client.worker.reconcile'
  readonly occupancyLeaseId: ClientOccupancyLeaseId | null
  readonly workers: readonly ClientWorkerReconciliation[]
}

export interface ClientCandidateRetainedMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.candidate.retained'
  readonly workerSessionId: WorkerSessionId
  readonly receipt: LocalCandidateReceipt
}

export interface ClientCandidateApplyResultMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.candidate.apply_result'
  readonly receipt: LocalApplyReceipt
}

export interface ClientCommandAckMessage extends ClientControlMessageEnvelopeFields {
  readonly kind: 'client.command_ack'
  readonly commandMessageId: ClientControlMessageId
  readonly commandKind: ClientControlMessageKind
  readonly status: CommandAckStatus
  readonly currentRevision?: Revision
  readonly error?: ClientControlError
}

export interface ClientEnrollmentAcceptedMessage extends ClientControlMessageEnvelopeFields {
  readonly kind: 'client.enrollment_accepted'
  readonly publicClientId: PublicClientId
  readonly serverTime: Instant
  readonly heartbeatIntervalMs: number
}

export interface ClientAccessChallengeMessage extends ClientControlMessageEnvelopeFields {
  readonly kind: 'client.access.challenge'
  readonly challengeId: ClientAccessChallengeId
  readonly connectCodeId: ClientConnectCodeId
  readonly codeDigest: Sha256Digest
  readonly requesterUserId: UserId
  readonly expiresAt: Instant
}

export interface ClientOccupancyOfferMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.occupancy.offer'
  readonly holderUserId: UserId
  readonly claimRequestId: OccupancyClaimId
  readonly claimedAt: Instant
  readonly idleExpiresAt: Instant | null
}

export interface ClientOccupancyReleaseMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.occupancy.release'
  readonly mode: ClientOccupancyReleaseMode
}

export interface ClientOccupancyForceFenceMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.occupancy.force_fence'
  readonly supersededLeaseId: ClientOccupancyLeaseId | null
  readonly reason: ClientOccupancyForceFenceReason
}

export interface ClientRepositoryRescanMessage extends ClientControlMessageEnvelopeFields, ClientControlCommandFields {
  readonly kind: 'client.repository.rescan'
  readonly repositoryBindingId: RepositoryBindingId
  readonly reason: ClientRepositoryRescanReason
}

export interface ClientWorkerLaunchMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.worker.launch'
  readonly launchGrant: WorkerLaunchGrant
}

export interface ClientWorkerStopMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.worker.stop'
  readonly workerSessionId: WorkerSessionId
  readonly workerId: WorkerId
  readonly reason: ClientWorkerStopReason
}

export interface ClientCandidateApplyMessage extends ClientControlMessageEnvelopeFields, ClientControlFencedCommandFields {
  readonly kind: 'client.candidate.apply'
  readonly repositoryBindingId: RepositoryBindingId
  readonly candidateRef: CandidateRef
  readonly targetBranch: GitRefName
  readonly expectedHead: GitCommitSha
  readonly strategy: LocalApplyStrategy
  readonly requesterUserId: UserId
}

export interface ClientLockMessage extends ClientControlMessageEnvelopeFields, ClientControlCommandFields {
  readonly kind: 'client.client_lock'
  readonly lockState: ClientLockState
}

export interface ClientCredentialRotateMessage extends ClientControlMessageEnvelopeFields, ClientControlCommandFields {
  readonly kind: 'client.credential_rotate'
  readonly reason: ClientCredentialRotateReason
}

/** Kind → message mapping for the Client → Server direction (16 kinds). */
export interface ClientToServerMessageByKind {
  'client.enroll': ClientEnrollMessage
  'client.hello': ClientHelloMessage
  'client.heartbeat': ClientHeartbeatMessage
  'client.connect_code.published': ClientConnectCodePublishedMessage
  'client.access.challenge_ack': ClientAccessChallengeAckMessage
  'client.occupancy.ack': ClientOccupancyAckMessage
  'client.occupancy.rejected': ClientOccupancyRejectedMessage
  'client.repository.upsert': ClientRepositoryUpsertMessage
  'client.repository.removed': ClientRepositoryRemovedMessage
  'client.repository.status': ClientRepositoryStatusMessage
  'client.worker.launch_ack': ClientWorkerLaunchAckMessage
  'client.worker.state': ClientWorkerStateMessage
  'client.worker.reconcile': ClientWorkerReconcileMessage
  'client.candidate.retained': ClientCandidateRetainedMessage
  'client.candidate.apply_result': ClientCandidateApplyResultMessage
  'client.command_ack': ClientCommandAckMessage
}

/** Kind → message mapping for the Server → Client direction (11 kinds). */
export interface ServerToClientMessageByKind {
  'client.enrollment_accepted': ClientEnrollmentAcceptedMessage
  'client.access.challenge': ClientAccessChallengeMessage
  'client.occupancy.offer': ClientOccupancyOfferMessage
  'client.occupancy.release': ClientOccupancyReleaseMessage
  'client.occupancy.force_fence': ClientOccupancyForceFenceMessage
  'client.repository.rescan': ClientRepositoryRescanMessage
  'client.worker.launch': ClientWorkerLaunchMessage
  'client.worker.stop': ClientWorkerStopMessage
  'client.candidate.apply': ClientCandidateApplyMessage
  'client.client_lock': ClientLockMessage
  'client.credential_rotate': ClientCredentialRotateMessage
}

export type ClientToServerKind = keyof ClientToServerMessageByKind
export type ServerToClientKind = keyof ServerToClientMessageByKind
export type ClientControlMessageKind = ClientToServerKind | ServerToClientKind

export type ClientToServerMessage = ClientToServerMessageByKind[ClientToServerKind]
export type ServerToClientMessage = ServerToClientMessageByKind[ServerToClientKind]
export type ClientControlMessage = ClientToServerMessage | ServerToClientMessage

/** §9.3 Client → Server message kinds, verbatim. */
export const CLIENT_TO_SERVER_MESSAGE_KINDS = Object.freeze([
  'client.enroll',
  'client.hello',
  'client.heartbeat',
  'client.connect_code.published',
  'client.access.challenge_ack',
  'client.occupancy.ack',
  'client.occupancy.rejected',
  'client.repository.upsert',
  'client.repository.removed',
  'client.repository.status',
  'client.worker.launch_ack',
  'client.worker.state',
  'client.worker.reconcile',
  'client.candidate.retained',
  'client.candidate.apply_result',
  'client.command_ack',
] as const)

/** §9.4 Server → Client message kinds, verbatim. */
export const SERVER_TO_CLIENT_MESSAGE_KINDS = Object.freeze([
  'client.enrollment_accepted',
  'client.access.challenge',
  'client.occupancy.offer',
  'client.occupancy.release',
  'client.occupancy.force_fence',
  'client.repository.rescan',
  'client.worker.launch',
  'client.worker.stop',
  'client.candidate.apply',
  'client.client_lock',
  'client.credential_rotate',
] as const)

/** Every ClientControlPort message kind, in schema ClientControlMessageKind order. */
export const CLIENT_CONTROL_MESSAGE_KINDS = Object.freeze([
  ...CLIENT_TO_SERVER_MESSAGE_KINDS,
  ...SERVER_TO_CLIENT_MESSAGE_KINDS,
] as const)

/**
 * Exactly the 19 command-class messages per the schema `x-message-class`.
 * Each carries `expectedRevision` + `idempotencyKey` on the envelope.
 */
export const CLIENT_CONTROL_COMMAND_MESSAGE_KINDS = Object.freeze([
  'client.enroll',
  'client.connect_code.published',
  'client.access.challenge_ack',
  'client.occupancy.ack',
  'client.occupancy.rejected',
  'client.repository.upsert',
  'client.repository.removed',
  'client.worker.launch_ack',
  'client.candidate.retained',
  'client.candidate.apply_result',
  'client.occupancy.offer',
  'client.occupancy.release',
  'client.occupancy.force_fence',
  'client.repository.rescan',
  'client.worker.launch',
  'client.worker.stop',
  'client.candidate.apply',
  'client.client_lock',
  'client.credential_rotate',
] as const)

/**
 * Occupancy-stamped command kinds per direction: exactly 11 across the
 * protocol. Every one carries `occupancyLeaseId` + `occupancyFencingToken`;
 * worker.state, worker.reconcile, and the repository kinds are not stamped.
 */
export const CLIENT_TO_SERVER_OCCUPANCY_FENCED_MESSAGE_KINDS = Object.freeze([
  'client.occupancy.ack',
  'client.occupancy.rejected',
  'client.worker.launch_ack',
  'client.candidate.retained',
  'client.candidate.apply_result',
] as const)

export const SERVER_TO_CLIENT_OCCUPANCY_FENCED_MESSAGE_KINDS = Object.freeze([
  'client.occupancy.offer',
  'client.occupancy.release',
  'client.occupancy.force_fence',
  'client.worker.launch',
  'client.worker.stop',
  'client.candidate.apply',
] as const)

export const CLIENT_CONTROL_OCCUPANCY_FENCED_MESSAGE_KINDS = Object.freeze([
  ...CLIENT_TO_SERVER_OCCUPANCY_FENCED_MESSAGE_KINDS,
  ...SERVER_TO_CLIENT_OCCUPANCY_FENCED_MESSAGE_KINDS,
] as const)

type IsNever<T> = [T] extends [never] ? true : false

type Assert<T extends true> = T

type AssertListMatchesClientToServerMap = Assert<
  IsNever<Exclude<typeof CLIENT_TO_SERVER_MESSAGE_KINDS[number], ClientToServerKind>> extends true
    ? IsNever<Exclude<ClientToServerKind, typeof CLIENT_TO_SERVER_MESSAGE_KINDS[number]>>
    : false
>

type AssertListMatchesServerToClientMap = Assert<
  IsNever<Exclude<typeof SERVER_TO_CLIENT_MESSAGE_KINDS[number], ServerToClientKind>> extends true
    ? IsNever<Exclude<ServerToClientKind, typeof SERVER_TO_CLIENT_MESSAGE_KINDS[number]>>
    : false
>

// Compile-time guards: the frozen kind lists and the kind → message mappings
// must stay exactly aligned in both directions.
type AssertClientToServerKindList = AssertListMatchesClientToServerMap
type AssertServerToClientKindList = AssertListMatchesServerToClientMap
type AssertCommandKindList = Assert<
  IsNever<
    Exclude<typeof CLIENT_CONTROL_COMMAND_MESSAGE_KINDS[number], ClientControlMessageKind>
  > extends true
    ? true
    : false
>
type AssertFencedKindLists = Assert<
  IsNever<
    | Exclude<typeof CLIENT_TO_SERVER_OCCUPANCY_FENCED_MESSAGE_KINDS[number], ClientToServerKind>
    | Exclude<typeof SERVER_TO_CLIENT_OCCUPANCY_FENCED_MESSAGE_KINDS[number], ServerToClientKind>
  > extends true
    ? true
    : false
>

type ClientControlKindListsCheck = Assert<
  AssertClientToServerKindList extends true
    ? AssertServerToClientKindList extends true
      ? AssertCommandKindList extends true
        ? AssertFencedKindLists
        : false
      : false
    : false
>

function parseMessageKind(
  input: Readonly<Record<string, unknown>>,
  kinds: readonly string[],
  path: string,
): string {
  if (!Object.hasOwn(input, 'kind')) {
    controlError('INVALID_SHAPE', `${path}.kind`, `${path}.kind is required`)
  }
  return enumValue(input.kind, kinds, `${path}.kind`)
}

function parseEnvelopeBase(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientControlMessageEnvelopeFields {
  return {
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    messageId: CLIENT_CONTROL_MESSAGE_ID(input.messageId, `${path}.messageId`),
    clientNodeId: CLIENT_NODE_ID(input.clientNodeId, `${path}.clientNodeId`),
    clientInstanceId: CLIENT_INSTANCE_ID(input.clientInstanceId, `${path}.clientInstanceId`),
    sequence: exchangeSequence(input.sequence, `${path}.sequence`),
    occurredAt: instant(input.occurredAt, `${path}.occurredAt`),
  }
}

function parseCommandFields(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientControlCommandFields {
  return {
    expectedRevision: revision(input.expectedRevision, `${path}.expectedRevision`),
    idempotencyKey: IDEMPOTENCY_KEY(input.idempotencyKey, `${path}.idempotencyKey`),
  }
}

function parseFencedCommandFields(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientControlFencedCommandFields {
  return {
    ...parseCommandFields(input, path),
    occupancyLeaseId: CLIENT_OCCUPANCY_LEASE_ID(
      input.occupancyLeaseId,
      `${path}.occupancyLeaseId`,
    ),
    occupancyFencingToken: OCCUPANCY_FENCING_TOKEN(
      input.occupancyFencingToken,
      `${path}.occupancyFencingToken`,
    ),
  }
}

function parseClientEnrollMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientEnrollMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'displayName',
    'platform',
    'architecture',
    'clientVersion',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseCommandFields(input, path),
    kind: 'client.enroll',
    displayName: clientDisplayName(input.displayName, `${path}.displayName`),
    platform: enumValue(input.platform, CLIENT_PLATFORM_TARGETS, `${path}.platform`),
    architecture: enumValue(input.architecture, CLIENT_ARCHITECTURES, `${path}.architecture`),
    clientVersion: clientVersion(input.clientVersion, `${path}.clientVersion`),
  })
}

function parseClientHelloMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientHelloMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'clientVersion',
    'presenceState',
    'acceptingConnections',
    'lockState',
    'capacity',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    kind: 'client.hello',
    clientVersion: clientVersion(input.clientVersion, `${path}.clientVersion`),
    presenceState: enumValue(input.presenceState, CLIENT_NODE_PRESENCE_STATES, `${path}.presenceState`),
    acceptingConnections: booleanValue(input.acceptingConnections, `${path}.acceptingConnections`),
    lockState: enumValue(input.lockState, CLIENT_LOCK_STATES, `${path}.lockState`),
    capacity: parseClientCapacityReport(input.capacity, `${path}.capacity`),
  })
}

function parseClientHeartbeatMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientHeartbeatMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'presenceState',
    'acceptingConnections',
    'lockState',
    'capacity',
    'occupancyLeaseId',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    kind: 'client.heartbeat',
    presenceState: enumValue(input.presenceState, CLIENT_NODE_PRESENCE_STATES, `${path}.presenceState`),
    acceptingConnections: booleanValue(input.acceptingConnections, `${path}.acceptingConnections`),
    lockState: enumValue(input.lockState, CLIENT_LOCK_STATES, `${path}.lockState`),
    capacity: parseClientCapacityReport(input.capacity, `${path}.capacity`),
    occupancyLeaseId: nullable(
      input.occupancyLeaseId,
      `${path}.occupancyLeaseId`,
      CLIENT_OCCUPANCY_LEASE_ID,
    ),
  })
}

function parseClientConnectCodePublishedMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientConnectCodePublishedMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'connectCodeId',
    'codeDigest',
    'expiresAt',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseCommandFields(input, path),
    kind: 'client.connect_code.published',
    connectCodeId: CLIENT_CONNECT_CODE_ID(input.connectCodeId, `${path}.connectCodeId`),
    codeDigest: SHA256_DIGEST(input.codeDigest, `${path}.codeDigest`),
    expiresAt: instant(input.expiresAt, `${path}.expiresAt`),
  })
}

function parseClientAccessChallengeAckMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientAccessChallengeAckMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'challengeId',
    'connectCodeId',
    'status',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseCommandFields(input, path),
    kind: 'client.access.challenge_ack',
    challengeId: CLIENT_ACCESS_CHALLENGE_ID(input.challengeId, `${path}.challengeId`),
    connectCodeId: CLIENT_CONNECT_CODE_ID(input.connectCodeId, `${path}.connectCodeId`),
    status: enumValue(input.status, CLIENT_CHALLENGE_ACK_STATUSES, `${path}.status`),
  })
}

function parseClientOccupancyAckMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientOccupancyAckMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.occupancy.ack',
  })
}

function parseClientOccupancyRejectedMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientOccupancyRejectedMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'reason',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.occupancy.rejected',
    reason: enumValue(input.reason, OCCUPANCY_REJECT_REASONS, `${path}.reason`),
  })
}

function parseClientRepositoryUpsertMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientRepositoryUpsertMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'repository',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseCommandFields(input, path),
    kind: 'client.repository.upsert',
    repository: parseRepositoryBindingProjection(input.repository, `${path}.repository`),
  })
}

function parseClientRepositoryRemovedMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientRepositoryRemovedMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'repositoryBindingId',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseCommandFields(input, path),
    kind: 'client.repository.removed',
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
  })
}

function parseClientRepositoryStatusMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientRepositoryStatusMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'repositoryBindingId',
    'availability',
    'dirtyState',
    'headCommit',
    'lastScannedAt',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    kind: 'client.repository.status',
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
    availability: enumValue(input.availability, REPOSITORY_AVAILABILITIES, `${path}.availability`),
    dirtyState: enumValue(input.dirtyState, REPOSITORY_DIRTY_STATES, `${path}.dirtyState`),
    headCommit: GIT_COMMIT_SHA(input.headCommit, `${path}.headCommit`),
    lastScannedAt: instant(input.lastScannedAt, `${path}.lastScannedAt`),
  })
}

function parseClientWorkerLaunchAckMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientWorkerLaunchAckMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'workerLaunchGrantId',
    'workerSessionId',
    'workerId',
    'workerInstanceId',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'status',
    'expectedRevision',
    'idempotencyKey',
  ], path, ['error'])
  const error = Object.hasOwn(input, 'error')
    ? parseClientControlError(input.error, `${path}.error`)
    : undefined
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.worker.launch_ack',
    workerLaunchGrantId: WORKER_LAUNCH_GRANT_ID(
      input.workerLaunchGrantId,
      `${path}.workerLaunchGrantId`,
    ),
    workerSessionId: WORKER_SESSION_ID(input.workerSessionId, `${path}.workerSessionId`),
    workerId: WORKER_ID(input.workerId, `${path}.workerId`),
    workerInstanceId: WORKER_INSTANCE_ID(input.workerInstanceId, `${path}.workerInstanceId`),
    status: enumValue(input.status, WORKER_LAUNCH_ACK_STATUSES, `${path}.status`),
    ...(error === undefined ? {} : { error }),
  })
}

function parseClientWorkerStateMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientWorkerStateMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'workerSessionId',
    'workerInstanceId',
    'occupancyLeaseId',
    'state',
    'observedAt',
  ], path, ['exitCode'])
  const exitCode = Object.hasOwn(input, 'exitCode')
    ? int32OrNull(input.exitCode, `${path}.exitCode`)
    : undefined
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    kind: 'client.worker.state',
    workerSessionId: WORKER_SESSION_ID(input.workerSessionId, `${path}.workerSessionId`),
    workerInstanceId: WORKER_INSTANCE_ID(input.workerInstanceId, `${path}.workerInstanceId`),
    occupancyLeaseId: nullable(
      input.occupancyLeaseId,
      `${path}.occupancyLeaseId`,
      CLIENT_OCCUPANCY_LEASE_ID,
    ),
    state: enumValue(input.state, CLIENT_WORKER_RUN_STATES, `${path}.state`),
    observedAt: instant(input.observedAt, `${path}.observedAt`),
    ...(exitCode === undefined ? {} : { exitCode }),
  })
}

function parseClientWorkerReconcileMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientWorkerReconcileMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'occupancyLeaseId',
    'workers',
  ], path)
  const entries = boundedArray(input.workers, `${path}.workers`, 1_024)
  const workers = entries.map((entry, index) => (
    parseClientWorkerReconciliation(entry, `${path}.workers[${String(index)}]`)
  ))
  if (new Set(workers.map(worker => JSON.stringify(worker))).size !== workers.length) {
    controlError('DUPLICATE_ID', `${path}.workers`, `${path}.workers contains duplicate entries`)
  }
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    kind: 'client.worker.reconcile',
    occupancyLeaseId: nullable(
      input.occupancyLeaseId,
      `${path}.occupancyLeaseId`,
      CLIENT_OCCUPANCY_LEASE_ID,
    ),
    workers: Object.freeze(workers),
  })
}

function parseClientCandidateRetainedMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientCandidateRetainedMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'workerSessionId',
    'receipt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.candidate.retained',
    workerSessionId: WORKER_SESSION_ID(input.workerSessionId, `${path}.workerSessionId`),
    receipt: parseLocalCandidateReceipt(input.receipt, `${path}.receipt`),
  })
}

function parseClientCandidateApplyResultMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientCandidateApplyResultMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'receipt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.candidate.apply_result',
    receipt: parseLocalApplyReceipt(input.receipt, `${path}.receipt`),
  })
}

function parseClientCommandAckMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientCommandAckMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'commandMessageId',
    'commandKind',
    'status',
  ], path, ['currentRevision', 'error'])
  const currentRevision = Object.hasOwn(input, 'currentRevision')
    ? revision(input.currentRevision, `${path}.currentRevision`)
    : undefined
  const error = Object.hasOwn(input, 'error')
    ? parseClientControlError(input.error, `${path}.error`)
    : undefined
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    kind: 'client.command_ack',
    commandMessageId: CLIENT_CONTROL_MESSAGE_ID(
      input.commandMessageId,
      `${path}.commandMessageId`,
    ),
    commandKind: enumValue(input.commandKind, CLIENT_CONTROL_MESSAGE_KINDS, `${path}.commandKind`),
    status: enumValue(input.status, COMMAND_ACK_STATUSES, `${path}.status`),
    ...(currentRevision === undefined ? {} : { currentRevision }),
    ...(error === undefined ? {} : { error }),
  })
}

function parseClientEnrollmentAcceptedMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientEnrollmentAcceptedMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'publicClientId',
    'serverTime',
    'heartbeatIntervalMs',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    kind: 'client.enrollment_accepted',
    publicClientId: PUBLIC_CLIENT_ID(input.publicClientId, `${path}.publicClientId`),
    serverTime: instant(input.serverTime, `${path}.serverTime`),
    heartbeatIntervalMs: boundedInteger(
      input.heartbeatIntervalMs,
      `${path}.heartbeatIntervalMs`,
      1_000,
      300_000,
    ),
  })
}

function parseClientAccessChallengeMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientAccessChallengeMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'challengeId',
    'connectCodeId',
    'codeDigest',
    'requesterUserId',
    'expiresAt',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    kind: 'client.access.challenge',
    challengeId: CLIENT_ACCESS_CHALLENGE_ID(input.challengeId, `${path}.challengeId`),
    connectCodeId: CLIENT_CONNECT_CODE_ID(input.connectCodeId, `${path}.connectCodeId`),
    codeDigest: SHA256_DIGEST(input.codeDigest, `${path}.codeDigest`),
    requesterUserId: USER_ID(input.requesterUserId, `${path}.requesterUserId`),
    expiresAt: instant(input.expiresAt, `${path}.expiresAt`),
  })
}

function parseClientOccupancyOfferMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientOccupancyOfferMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'holderUserId',
    'claimRequestId',
    'claimedAt',
    'idleExpiresAt',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.occupancy.offer',
    holderUserId: USER_ID(input.holderUserId, `${path}.holderUserId`),
    claimRequestId: OCCUPANCY_CLAIM_ID(input.claimRequestId, `${path}.claimRequestId`),
    claimedAt: instant(input.claimedAt, `${path}.claimedAt`),
    idleExpiresAt: nullable(input.idleExpiresAt, `${path}.idleExpiresAt`, instant),
  })
}

function parseClientOccupancyReleaseMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientOccupancyReleaseMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'mode',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.occupancy.release',
    mode: enumValue(input.mode, CLIENT_OCCUPANCY_RELEASE_MODES, `${path}.mode`),
  })
}

function parseClientOccupancyForceFenceMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientOccupancyForceFenceMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'supersededLeaseId',
    'reason',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.occupancy.force_fence',
    supersededLeaseId: nullable(
      input.supersededLeaseId,
      `${path}.supersededLeaseId`,
      CLIENT_OCCUPANCY_LEASE_ID,
    ),
    reason: enumValue(
      input.reason,
      CLIENT_OCCUPANCY_FORCE_FENCE_REASONS,
      `${path}.reason`,
    ),
  })
}

function parseClientRepositoryRescanMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientRepositoryRescanMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'repositoryBindingId',
    'reason',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseCommandFields(input, path),
    kind: 'client.repository.rescan',
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
    reason: enumValue(
      input.reason,
      CLIENT_REPOSITORY_RESCAN_REASONS,
      `${path}.reason`,
    ),
  })
}

function parseClientWorkerLaunchMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientWorkerLaunchMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'launchGrant',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  const fenced = parseFencedCommandFields(input, path)
  const launchGrant = parseWorkerLaunchGrant(input.launchGrant, `${path}.launchGrant`)
  if (launchGrant.occupancyLeaseId !== fenced.occupancyLeaseId
    || launchGrant.occupancyFencingToken !== fenced.occupancyFencingToken) {
    controlError(
      'RELATIONSHIP_MISMATCH',
      `${path}.launchGrant`,
      'the worker launch grant does not match the occupancy identity of its command; a field mismatch is rejected',
    )
  }
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...fenced,
    kind: 'client.worker.launch',
    launchGrant,
  })
}

function parseClientWorkerStopMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientWorkerStopMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'workerSessionId',
    'workerId',
    'reason',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.worker.stop',
    workerSessionId: WORKER_SESSION_ID(input.workerSessionId, `${path}.workerSessionId`),
    workerId: WORKER_ID(input.workerId, `${path}.workerId`),
    reason: enumValue(input.reason, CLIENT_WORKER_STOP_REASONS, `${path}.reason`),
  })
}

function parseClientCandidateApplyMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientCandidateApplyMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'repositoryBindingId',
    'candidateRef',
    'targetBranch',
    'expectedHead',
    'strategy',
    'requesterUserId',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseFencedCommandFields(input, path),
    kind: 'client.candidate.apply',
    repositoryBindingId: REPOSITORY_BINDING_ID(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
    ),
    candidateRef: CANDIDATE_REF(input.candidateRef, `${path}.candidateRef`),
    targetBranch: GIT_REF_NAME(input.targetBranch, `${path}.targetBranch`),
    expectedHead: GIT_COMMIT_SHA(input.expectedHead, `${path}.expectedHead`),
    strategy: enumValue(input.strategy, LOCAL_APPLY_STRATEGIES, `${path}.strategy`),
    requesterUserId: USER_ID(input.requesterUserId, `${path}.requesterUserId`),
  })
}

function parseClientLockMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientLockMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'lockState',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseCommandFields(input, path),
    kind: 'client.client_lock',
    lockState: enumValue(input.lockState, CLIENT_LOCK_STATES, `${path}.lockState`),
  })
}

function parseClientCredentialRotateMessage(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientCredentialRotateMessage {
  exactKeys(input, [
    'kind',
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'reason',
    'expectedRevision',
    'idempotencyKey',
  ], path)
  return Object.freeze({
    ...parseEnvelopeBase(input, path),
    ...parseCommandFields(input, path),
    kind: 'client.credential_rotate',
    reason: enumValue(
      input.reason,
      CLIENT_CREDENTIAL_ROTATE_REASONS,
      `${path}.reason`,
    ),
  })
}

function parseClientToServerByKind(
  kind: ClientToServerKind,
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientToServerMessage {
  switch (kind) {
    case 'client.enroll':
      return parseClientEnrollMessage(input, path)
    case 'client.hello':
      return parseClientHelloMessage(input, path)
    case 'client.heartbeat':
      return parseClientHeartbeatMessage(input, path)
    case 'client.connect_code.published':
      return parseClientConnectCodePublishedMessage(input, path)
    case 'client.access.challenge_ack':
      return parseClientAccessChallengeAckMessage(input, path)
    case 'client.occupancy.ack':
      return parseClientOccupancyAckMessage(input, path)
    case 'client.occupancy.rejected':
      return parseClientOccupancyRejectedMessage(input, path)
    case 'client.repository.upsert':
      return parseClientRepositoryUpsertMessage(input, path)
    case 'client.repository.removed':
      return parseClientRepositoryRemovedMessage(input, path)
    case 'client.repository.status':
      return parseClientRepositoryStatusMessage(input, path)
    case 'client.worker.launch_ack':
      return parseClientWorkerLaunchAckMessage(input, path)
    case 'client.worker.state':
      return parseClientWorkerStateMessage(input, path)
    case 'client.worker.reconcile':
      return parseClientWorkerReconcileMessage(input, path)
    case 'client.candidate.retained':
      return parseClientCandidateRetainedMessage(input, path)
    case 'client.candidate.apply_result':
      return parseClientCandidateApplyResultMessage(input, path)
    case 'client.command_ack':
      return parseClientCommandAckMessage(input, path)
  }
}

function parseServerToClientByKind(
  kind: ServerToClientKind,
  input: Readonly<Record<string, unknown>>,
  path: string,
): ServerToClientMessage {
  switch (kind) {
    case 'client.enrollment_accepted':
      return parseClientEnrollmentAcceptedMessage(input, path)
    case 'client.access.challenge':
      return parseClientAccessChallengeMessage(input, path)
    case 'client.occupancy.offer':
      return parseClientOccupancyOfferMessage(input, path)
    case 'client.occupancy.release':
      return parseClientOccupancyReleaseMessage(input, path)
    case 'client.occupancy.force_fence':
      return parseClientOccupancyForceFenceMessage(input, path)
    case 'client.repository.rescan':
      return parseClientRepositoryRescanMessage(input, path)
    case 'client.worker.launch':
      return parseClientWorkerLaunchMessage(input, path)
    case 'client.worker.stop':
      return parseClientWorkerStopMessage(input, path)
    case 'client.candidate.apply':
      return parseClientCandidateApplyMessage(input, path)
    case 'client.client_lock':
      return parseClientLockMessage(input, path)
    case 'client.credential_rotate':
      return parseClientCredentialRotateMessage(input, path)
  }
}

/** Parse and validate one Client → Server message (16 kinds). */
export function parseClientToServerMessage(
  value: unknown,
  path = 'clientToServerMessage',
): ClientToServerMessage {
  const input = record(value, path)
  const kind = parseMessageKind(input, CLIENT_TO_SERVER_MESSAGE_KINDS, path) as ClientToServerKind
  return parseClientToServerByKind(kind, input, path)
}

/** Parse and validate one Server → Client message (11 kinds). */
export function parseServerToClientMessage(
  value: unknown,
  path = 'serverToClientMessage',
): ServerToClientMessage {
  const input = record(value, path)
  const kind = parseMessageKind(input, SERVER_TO_CLIENT_MESSAGE_KINDS, path) as ServerToClientKind
  return parseServerToClientByKind(kind, input, path)
}

/**
 * Parse and validate one message of the full ClientControlMessage union
 * (all 27 kinds), trying the Client → Server direction first.
 */
export function parseClientControlMessage(
  value: unknown,
  path = 'clientControlMessage',
): ClientControlMessage {
  try {
    return parseClientToServerMessage(value, path)
  } catch (clientToServerError) {
    try {
      return parseServerToClientMessage(value, path)
    } catch (serverToClientError) {
      controlError(
        'INVALID_VALUE',
        path,
        `${path} matches neither ClientControlPort direction`,
        { cause: serverToClientError },
      )
    }
  }
}

// The kind-list asserts are compile-time only; this export keeps the check in
// the type graph without emitting runtime code.
export type { ClientControlKindListsCheck }
