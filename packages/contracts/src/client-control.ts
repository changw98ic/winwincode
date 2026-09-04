/**
 * ClientControlPort contracts for the multi-user shared Client model.
 *
 * The authoritative wire contract is `schema/winwincode/v1/client-control.schema.json`.
 * This module is the Phase 0 TypeScript projection of that schema and must be
 * re-aligned with it during Phase 0 integration once the schema file lands.
 * Field names and message kinds are taken verbatim from the approved plan:
 * §7 domain objects, §9.5 envelope, §9.3 Client→Server kinds, §9.4 Server→Client kinds.
 *
 * Boundary invariants enforced by this module:
 * - no field carries a local absolute path; repository bindings resolve only
 *   inside the Device Client and the Server never sees or stores a path;
 * - `ClientConnectCode` carries only `codeDigest`, never connect-code plaintext;
 * - every command payload carries `expectedRevision` and `idempotencyKey`;
 * - command payloads that touch occupancy or task execution additionally carry
 *   `occupancyLeaseId` and `occupancyFencingToken`, which the Device Client must
 *   fence locally before acting.
 *
 * All timestamp fields are Unix epoch milliseconds; nullable timestamps stay
 * null until the fact they describe first occurs.
 */

export const CLIENT_CONTROL_SCHEMA_VERSION = 1 as const

export const CLIENT_CONTROL_PROTOCOL = 'winwincode.client-control.v1' as const

import {
  StageRunId,
  type StageRunId as StageRunIdentifier,
} from './delivery.js'

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

declare const clientControlIdentifierBrand: unique symbol

type ClientControlIdentifier<Name extends string> = string & {
  readonly [clientControlIdentifierBrand]: Name
}

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u
const DIGEST_PATTERN = /^[0-9a-f]{64}$/u
const GIT_COMMIT_PATTERN = /^[0-9a-f]{40}$|^[0-9a-f]{64}$/u
const MAX_TEXT_LENGTH = 65_536
const MAX_COLLECTION_LENGTH = 1_000

export type UserId = ClientControlIdentifier<'UserId'>
export type ClientNodeId = ClientControlIdentifier<'ClientNodeId'>
export type ClientInstanceId = ClientControlIdentifier<'ClientInstanceId'>
export type ClientConnectCodeId = ClientControlIdentifier<'ClientConnectCodeId'>
export type ClientAccessGrantId = ClientControlIdentifier<'ClientAccessGrantId'>
export type ClientOccupancyLeaseId = ClientControlIdentifier<'ClientOccupancyLeaseId'>
export type RepositoryBindingId = ClientControlIdentifier<'RepositoryBindingId'>
export type RepositoryAccessGrantId = ClientControlIdentifier<'RepositoryAccessGrantId'>
export type WorkerLaunchGrantId = ClientControlIdentifier<'WorkerLaunchGrantId'>
export type ProductSessionId = ClientControlIdentifier<'ProductSessionId'>
export type WorkerSessionId = ClientControlIdentifier<'WorkerSessionId'>
export type WorkerId = ClientControlIdentifier<'WorkerId'>
export type WorkerInstanceId = ClientControlIdentifier<'WorkerInstanceId'>
export type LocalCandidateReceiptId = ClientControlIdentifier<'LocalCandidateReceiptId'>
export type LocalApplyReceiptId = ClientControlIdentifier<'LocalApplyReceiptId'>

function clientControlIdentifier<Name extends string>(
  value: string,
  name: Name,
): ClientControlIdentifier<Name> {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    controlError(
      'INVALID_IDENTIFIER',
      name,
      `${name} must be a portable identifier of at most 200 characters`,
    )
  }
  return value as ClientControlIdentifier<Name>
}

function identifierAt<Identifier>(
  value: unknown,
  path: string,
  factory: (input: string) => Identifier,
): Identifier {
  try {
    if (typeof value !== 'string') throw new Error('identifier must be a string')
    return factory(value)
  } catch (error) {
    controlError('INVALID_IDENTIFIER', path, `${path} is invalid`, { cause: error })
  }
}

export function UserId(value: string): UserId {
  return clientControlIdentifier(value, 'UserId')
}

export function ClientNodeId(value: string): ClientNodeId {
  return clientControlIdentifier(value, 'ClientNodeId')
}

export function ClientInstanceId(value: string): ClientInstanceId {
  return clientControlIdentifier(value, 'ClientInstanceId')
}

export function ClientConnectCodeId(value: string): ClientConnectCodeId {
  return clientControlIdentifier(value, 'ClientConnectCodeId')
}

export function ClientAccessGrantId(value: string): ClientAccessGrantId {
  return clientControlIdentifier(value, 'ClientAccessGrantId')
}

export function ClientOccupancyLeaseId(value: string): ClientOccupancyLeaseId {
  return clientControlIdentifier(value, 'ClientOccupancyLeaseId')
}

export function RepositoryBindingId(value: string): RepositoryBindingId {
  return clientControlIdentifier(value, 'RepositoryBindingId')
}

export function RepositoryAccessGrantId(value: string): RepositoryAccessGrantId {
  return clientControlIdentifier(value, 'RepositoryAccessGrantId')
}

export function WorkerLaunchGrantId(value: string): WorkerLaunchGrantId {
  return clientControlIdentifier(value, 'WorkerLaunchGrantId')
}

export function ProductSessionId(value: string): ProductSessionId {
  return clientControlIdentifier(value, 'ProductSessionId')
}

export function WorkerSessionId(value: string): WorkerSessionId {
  return clientControlIdentifier(value, 'WorkerSessionId')
}

export function WorkerId(value: string): WorkerId {
  return clientControlIdentifier(value, 'WorkerId')
}

export function WorkerInstanceId(value: string): WorkerInstanceId {
  return clientControlIdentifier(value, 'WorkerInstanceId')
}

export function LocalCandidateReceiptId(value: string): LocalCandidateReceiptId {
  return clientControlIdentifier(value, 'LocalCandidateReceiptId')
}

export function LocalApplyReceiptId(value: string): LocalApplyReceiptId {
  return clientControlIdentifier(value, 'LocalApplyReceiptId')
}

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
  keys: readonly string[],
  path: string,
): void {
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    controlError('INVALID_SHAPE', path, `${path} has an unexpected shape`)
  }
}

function schemaVersion(value: unknown, path: string): typeof CLIENT_CONTROL_SCHEMA_VERSION {
  if (value !== CLIENT_CONTROL_SCHEMA_VERSION) {
    controlError(
      'UNSUPPORTED_SCHEMA_VERSION',
      path,
      `${path} must be ${String(CLIENT_CONTROL_SCHEMA_VERSION)}`,
    )
  }
  return CLIENT_CONTROL_SCHEMA_VERSION
}

function boundedText(
  value: unknown,
  path: string,
  maximum = MAX_TEXT_LENGTH,
): string {
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > maximum
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    controlError('INVALID_VALUE', path, `${path} must be non-empty bounded text`)
  }
  return value
}

function nullableText(
  value: unknown,
  path: string,
  maximum = MAX_TEXT_LENGTH,
): string | null {
  return value === null ? null : boundedText(value, path, maximum)
}

function portableReference(value: unknown, path: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    controlError('INVALID_IDENTIFIER', path, `${path} must be a portable identifier`)
  }
  return value
}

function digestText(value: unknown, path: string): string {
  if (typeof value !== 'string' || !DIGEST_PATTERN.test(value)) {
    controlError(
      'INVALID_VALUE',
      path,
      `${path} must be a lowercase hexadecimal SHA-256 digest; plaintext secrets never cross this port`,
    )
  }
  return value
}

function gitCommitId(value: unknown, path: string): string {
  if (typeof value !== 'string' || !GIT_COMMIT_PATTERN.test(value)) {
    controlError('INVALID_VALUE', path, `${path} must be a full lowercase Git commit id`)
  }
  return value
}

function nullableGitCommitId(value: unknown, path: string): string | null {
  return value === null ? null : gitCommitId(value, path)
}

function gitRefName(value: unknown, path: string): string {
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
      segment.length === 0
      || segment.startsWith('.')
      || segment.endsWith('.lock')
    ))) {
    controlError('INVALID_VALUE', path, `${path} must be a valid Git ref name`)
  }
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

function booleanValue(value: unknown, path: string): boolean {
  if (typeof value !== 'boolean') controlError('INVALID_VALUE', path, `${path} must be boolean`)
  return value
}

function nonNegativeInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    controlError('INVALID_VALUE', path, `${path} must be a non-negative safe integer`)
  }
  return Number(value)
}

function positiveInteger(value: unknown, path: string): number {
  const parsed = nonNegativeInteger(value, path)
  if (parsed === 0) controlError('INVALID_VALUE', path, `${path} must be positive`)
  return parsed
}

function timestamp(value: unknown, path: string): number {
  return nonNegativeInteger(value, path)
}

function nullableTimestamp(value: unknown, path: string): number | null {
  return value === null ? null : timestamp(value, path)
}

function enumList<const Values extends readonly string[]>(
  value: unknown,
  values: Values,
  path: string,
): readonly Values[number][] {
  if (!Array.isArray(value) || value.length > values.length) {
    controlError('INVALID_VALUE', path, `${path} must be an array of permitted values`)
  }
  const entries = value.map((entry, index) => enumValue(entry, values, `${path}[${String(index)}]`))
  if (new Set(entries).size !== entries.length) {
    controlError('DUPLICATE_ID', path, `${path} contains duplicate entries`)
  }
  return Object.freeze(entries)
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

export const CLIENT_NODE_LOCK_STATES = Object.freeze(['unlocked', 'locked'] as const)
export type ClientNodeLockState = typeof CLIENT_NODE_LOCK_STATES[number]

export const CLIENT_NODE_PLATFORMS = Object.freeze(['darwin', 'linux'] as const)
export type ClientNodePlatform = typeof CLIENT_NODE_PLATFORMS[number]

export const CLIENT_NODE_ARCHITECTURES = Object.freeze(['aarch64', 'x86_64'] as const)
export type ClientNodeArchitecture = typeof CLIENT_NODE_ARCHITECTURES[number]

export const CLIENT_CONNECT_CODE_STATES = Object.freeze([
  'active',
  'consumed',
  'expired',
  'revoked',
] as const)
export type ClientConnectCodeState = typeof CLIENT_CONNECT_CODE_STATES[number]

export const CLIENT_ACCESS_PERMISSIONS = Object.freeze(['use', 'manage', 'share'] as const)
export type ClientAccessPermission = typeof CLIENT_ACCESS_PERMISSIONS[number]

export const CLIENT_ACCESS_TRUST_MODES = Object.freeze(['temporary', 'trusted'] as const)
export type ClientAccessTrustMode = typeof CLIENT_ACCESS_TRUST_MODES[number]

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

export const REPOSITORY_KINDS = Object.freeze(['git'] as const)
export type RepositoryKind = typeof REPOSITORY_KINDS[number]

export const REPOSITORY_DIRTY_STATES = Object.freeze(['clean', 'dirty'] as const)
export type RepositoryDirtyState = typeof REPOSITORY_DIRTY_STATES[number]

export const REPOSITORY_AVAILABILITIES = Object.freeze([
  'available',
  'unavailable',
  'moved',
  'invalid_git',
  'permission_denied',
  'scan_failed',
] as const)
export type RepositoryAvailability = typeof REPOSITORY_AVAILABILITIES[number]

export const REPOSITORY_ACCESS_PERMISSIONS = Object.freeze(['use', 'manage'] as const)
export type RepositoryAccessPermission = typeof REPOSITORY_ACCESS_PERMISSIONS[number]

export const REPOSITORY_ACCESS_GRANT_STATES = Object.freeze(['active', 'revoked'] as const)
export type RepositoryAccessGrantState = typeof REPOSITORY_ACCESS_GRANT_STATES[number]

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

/**
 * §7.1 UserAccount. `passwordHash` is a Control-Plane-only secret: it is part
 * of the domain object for completeness and never crosses ClientControlPort.
 */
export interface UserAccount {
  readonly userId: UserId
  readonly username: string
  readonly normalizedUsername: string
  /** Argon2id hash. Server-side only; never transported over ClientControlPort. */
  readonly passwordHash: string
  readonly role: UserAccountRole
  readonly state: UserAccountState
  readonly createdAt: number
  readonly updatedAt: number
  readonly revision: number
}

/**
 * §7.2 ClientNode. `publicClientId` is a stable, non-secret lookup id.
 * This projection never contains local filesystem paths.
 */
export interface ClientNode {
  readonly clientNodeId: ClientNodeId
  readonly publicClientId: string
  readonly displayName: string
  readonly platform: ClientNodePlatform
  readonly architecture: ClientNodeArchitecture
  readonly clientVersion: string
  /** Digest of the device credential; the credential itself stays local. */
  readonly deviceCredentialDigest: string
  /** Instance identity of the live daemon; null until first instance reports. */
  readonly currentInstanceId: ClientInstanceId | null
  readonly presenceState: ClientNodePresenceState
  readonly acceptingConnections: boolean
  readonly lockState: ClientNodeLockState
  readonly maxConcurrentWorkerSessions: number
  readonly reportedRunningWorkerSessions: number
  readonly lastHeartbeatAt: number | null
  readonly createdAt: number
  readonly revision: number
}

/** Enrollment facts a Device Client reports about itself before it has a node id. */
export type ClientNodeEnrollment = Pick<
  ClientNode,
  | 'publicClientId'
  | 'displayName'
  | 'platform'
  | 'architecture'
  | 'clientVersion'
  | 'deviceCredentialDigest'
  | 'maxConcurrentWorkerSessions'
>

/**
 * §7.3 ClientConnectCode. Only `codeDigest` is stored or transported; the
 * plaintext code never appears in any contract, log, or projection.
 */
export interface ClientConnectCode {
  readonly connectCodeId: ClientConnectCodeId
  readonly clientNodeId: ClientNodeId
  readonly codeDigest: string
  readonly issuedByInstanceId: ClientInstanceId
  readonly expiresAt: number
  readonly remainingAttempts: number
  readonly state: ClientConnectCodeState
  readonly createdAt: number
  readonly revision: number
}

/** §7.4 ClientAccessGrant. Many users may hold an active grant for one Client. */
export interface ClientAccessGrant {
  readonly clientAccessGrantId: ClientAccessGrantId
  readonly clientNodeId: ClientNodeId
  readonly userId: UserId
  readonly permissions: readonly ClientAccessPermission[]
  readonly trustMode: ClientAccessTrustMode
  readonly state: ClientAccessGrantState
  readonly grantedByUserId: UserId
  readonly grantSource: ClientAccessGrantSource
  readonly expiresAt: number | null
  readonly createdAt: number
  readonly revision: number
}

/**
 * §7.5 ClientOccupancyLease. At most one active lease per ClientNode.
 * `fencingToken` is monotonic per ClientNode; stale tokens are always rejected.
 */
export interface ClientOccupancyLease {
  readonly clientOccupancyLeaseId: ClientOccupancyLeaseId
  readonly clientNodeId: ClientNodeId
  readonly holderUserId: UserId
  readonly state: ClientOccupancyLeaseState
  readonly fencingToken: number
  readonly claimRequestId: string
  readonly claimedAt: number | null
  readonly acknowledgedAt: number | null
  readonly lastRenewedAt: number | null
  readonly idleExpiresAt: number | null
  readonly recoveryDeadlineAt: number | null
  readonly releaseRequestedAt: number | null
  readonly releasedAt: number | null
  readonly releaseReason: string | null
  readonly revision: number
}

/**
 * §7.6 RepositoryBinding. Security metadata projection only: this object has
 * no path field by contract. The binding-to-path mapping lives exclusively in
 * the Device Client local database (§8.1) and is never uploaded.
 */
export interface RepositoryBinding {
  readonly repositoryBindingId: RepositoryBindingId
  readonly clientNodeId: ClientNodeId
  readonly displayName: string
  readonly repositoryKind: RepositoryKind
  readonly defaultBranch: string
  readonly headCommit: string | null
  readonly dirtyState: RepositoryDirtyState
  readonly availability: RepositoryAvailability
  readonly repositoryFingerprint: string
  readonly lastScannedAt: number | null
  readonly revision: number
}

/** §7.7 RepositoryAccessGrant. Visibility requires an active grant per user. */
export interface RepositoryAccessGrant {
  readonly repositoryAccessGrantId: RepositoryAccessGrantId
  readonly repositoryBindingId: RepositoryBindingId
  readonly userId: UserId
  readonly permissions: readonly RepositoryAccessPermission[]
  readonly state: RepositoryAccessGrantState
  readonly grantedByUserId: UserId
  readonly createdAt: number
  readonly revision: number
}

/**
 * §7.8 WorkerLaunchGrant. Every identity field must match on launch; any
 * mismatch is rejected. Occupancy fields bind the grant to one fenced lease.
 */
export interface WorkerLaunchGrant {
  readonly workerLaunchGrantId: WorkerLaunchGrantId
  readonly clientNodeId: ClientNodeId
  readonly clientInstanceId: ClientInstanceId
  readonly occupancyLeaseId: ClientOccupancyLeaseId
  readonly occupancyFencingToken: number
  readonly repositoryBindingId: RepositoryBindingId
  readonly productSessionId: ProductSessionId
  readonly stageRunId: StageRunIdentifier
  readonly workerSessionId: WorkerSessionId
  readonly workerId: WorkerId
  readonly workerInstanceId: WorkerInstanceId
  /** Digest of the short-lived worker session credential; never the credential. */
  readonly credentialDigest: string
  readonly expiresAt: number
  readonly state: WorkerLaunchGrantState
  readonly revision: number
}

/** §7.9 LocalCandidateReceipt. Candidate facts survive local worktree cleanup. */
export interface LocalCandidateReceipt {
  readonly localCandidateReceiptId: LocalCandidateReceiptId
  readonly candidateRef: string
  readonly repositoryBindingId: RepositoryBindingId
  readonly candidateCommit: string
  readonly localRefName: string
  readonly state: LocalCandidateReceiptState
  readonly createdAt: number
  readonly revision: number
}

/** §7.10 LocalApplyReceipt. One audited attempt to land a Candidate locally. */
export interface LocalApplyReceipt {
  readonly localApplyReceiptId: LocalApplyReceiptId
  readonly candidateRef: string
  readonly repositoryBindingId: RepositoryBindingId
  readonly targetBranch: string
  readonly expectedHead: string
  readonly strategy: LocalApplyStrategy
  readonly result: LocalApplyResult
  readonly resultingCommit: string | null
  readonly conflictArtifactRef: string | null
  readonly createdAt: number
  readonly revision: number
}

/** One local Worker observation reported during recovery reconciliation (§18.3). */
export interface ClientWorkerReconcileReport {
  readonly workerSessionId: WorkerSessionId
  readonly state: WorkerReconcileState
}

/**
 * Base shared by every ClientControlPort command payload (§9.5). Every command
 * carries an optimistic-concurrency revision and a replay-safe idempotency key.
 */
export interface ClientControlCommand {
  readonly expectedRevision: number
  readonly idempotencyKey: string
}

/**
 * Base for command payloads that touch occupancy or task execution. The pair
 * binds the command to the single active lease; the Device Client must reject
 * any command whose fencing token is not the current maximum.
 */
export interface ClientControlOccupancyCommand extends ClientControlCommand {
  readonly occupancyLeaseId: ClientOccupancyLeaseId
  readonly occupancyFencingToken: number
}

export interface ClientEnrollPayload extends ClientControlCommand {
  readonly node: ClientNodeEnrollment
}

export interface ClientHelloPayload extends ClientControlCommand {
  readonly clientVersion: string
  readonly currentInstanceId: ClientInstanceId
  readonly presenceState: ClientNodePresenceState
}

export interface ClientHeartbeatPayload extends ClientControlCommand {
  readonly presenceState: ClientNodePresenceState
  readonly acceptingConnections: boolean
  readonly lockState: ClientNodeLockState
  readonly maxConcurrentWorkerSessions: number
  readonly reportedRunningWorkerSessions: number
}

export interface ClientConnectCodePublishedPayload extends ClientControlCommand {
  readonly connectCode: ClientConnectCode
}

export interface ClientAccessChallengeAckPayload extends ClientControlCommand {
  readonly challengeId: string
  readonly connectCodeId: ClientConnectCodeId
  readonly accepted: boolean
  readonly rejectionReason: string | null
}

export interface ClientOccupancyAckPayload extends ClientControlOccupancyCommand {
  readonly acknowledgedAt: number
}

export interface ClientOccupancyRejectedPayload extends ClientControlOccupancyCommand {
  readonly reason: string
}

export interface ClientRepositoryUpsertPayload extends ClientControlCommand {
  readonly binding: RepositoryBinding
}

export interface ClientRepositoryRemovedPayload extends ClientControlCommand {
  readonly repositoryBindingId: RepositoryBindingId
}

export interface ClientRepositoryStatusPayload extends ClientControlCommand {
  readonly repositoryBindingId: RepositoryBindingId
  readonly dirtyState: RepositoryDirtyState
  readonly availability: RepositoryAvailability
  readonly headCommit: string | null
  readonly lastScannedAt: number
}

export interface ClientWorkerLaunchAckPayload extends ClientControlOccupancyCommand {
  readonly workerLaunchGrantId: WorkerLaunchGrantId
  readonly workerSessionId: WorkerSessionId
  readonly workerInstanceId: WorkerInstanceId
  readonly accepted: boolean
  readonly reason: string | null
}

/** Bounded runtime report label; the exact vocabulary is owned by the schema. */
export interface ClientWorkerStatePayload extends ClientControlOccupancyCommand {
  readonly workerSessionId: WorkerSessionId
  readonly state: string
  readonly detail: string | null
}

export interface ClientWorkerReconcilePayload extends ClientControlOccupancyCommand {
  readonly reports: readonly ClientWorkerReconcileReport[]
}

export interface ClientCandidateRetainedPayload extends ClientControlOccupancyCommand {
  readonly receipt: LocalCandidateReceipt
}

export interface ClientCandidateApplyResultPayload extends ClientControlOccupancyCommand {
  readonly receipt: LocalApplyReceipt
}

export interface ClientCommandAckPayload extends ClientControlCommand {
  readonly acknowledgedMessageId: string
  readonly accepted: boolean
  readonly reason: string | null
}

export interface ClientEnrollmentAcceptedPayload extends ClientControlCommand {
  readonly presenceState: ClientNodePresenceState
}

export interface ClientAccessChallengePayload extends ClientControlCommand {
  readonly challengeId: string
  readonly connectCodeId: ClientConnectCodeId
  readonly expiresAt: number
}

export interface ClientOccupancyOfferPayload extends ClientControlOccupancyCommand {
  readonly holderUserId: UserId
  readonly claimRequestId: string
  readonly idleExpiresAt: number | null
}

export interface ClientOccupancyReleasePayload extends ClientControlOccupancyCommand {
  readonly releaseReason: string | null
  /** True when running WorkerSessions must finish before the lease releases. */
  readonly drain: boolean
}

export interface ClientOccupancyForceFencePayload extends ClientControlOccupancyCommand {
  readonly reason: string | null
}

/** Null repositoryBindingId asks the Client to rescan its whole registry. */
export interface ClientRepositoryRescanPayload extends ClientControlCommand {
  readonly repositoryBindingId: RepositoryBindingId | null
}

export interface ClientWorkerLaunchPayload extends ClientControlOccupancyCommand {
  readonly grant: WorkerLaunchGrant
}

export interface ClientWorkerStopPayload extends ClientControlOccupancyCommand {
  readonly workerSessionId: WorkerSessionId
  readonly reason: string | null
}

export interface ClientCandidateApplyPayload extends ClientControlOccupancyCommand {
  readonly candidateRef: string
  readonly repositoryBindingId: RepositoryBindingId
  readonly targetBranch: string
  readonly expectedHead: string
  readonly strategy: LocalApplyStrategy
}

export interface ClientLockPayload extends ClientControlCommand {
  readonly locked: boolean
  readonly reason: string | null
}

export interface ClientCredentialRotatePayload extends ClientControlCommand {
  readonly reason: string | null
}

/**
 * §9.3 Client → Server message kinds, verbatim. The list is frozen; the
 * compile-time asserts below keep it in exact sync with the payload mapping.
 */
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

/**
 * §9.4 Server → Client message kinds, verbatim.
 */
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

/**
 * Occupancy/execution class of each direction: these command payloads must
 * carry `occupancyLeaseId` and `occupancyFencingToken` on top of the shared
 * command base (§9.5), and the other kinds must not carry them.
 */
export const CLIENT_TO_SERVER_OCCUPANCY_FENCED_MESSAGE_KINDS = Object.freeze([
  'client.occupancy.ack',
  'client.occupancy.rejected',
  'client.worker.launch_ack',
  'client.worker.state',
  'client.worker.reconcile',
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

/** Kind → payload mapping for the Client → Server direction (§9.3). */
export interface ClientToServerPayloadByKind {
  'client.enroll': ClientEnrollPayload
  'client.hello': ClientHelloPayload
  'client.heartbeat': ClientHeartbeatPayload
  'client.connect_code.published': ClientConnectCodePublishedPayload
  'client.access.challenge_ack': ClientAccessChallengeAckPayload
  'client.occupancy.ack': ClientOccupancyAckPayload
  'client.occupancy.rejected': ClientOccupancyRejectedPayload
  'client.repository.upsert': ClientRepositoryUpsertPayload
  'client.repository.removed': ClientRepositoryRemovedPayload
  'client.repository.status': ClientRepositoryStatusPayload
  'client.worker.launch_ack': ClientWorkerLaunchAckPayload
  'client.worker.state': ClientWorkerStatePayload
  'client.worker.reconcile': ClientWorkerReconcilePayload
  'client.candidate.retained': ClientCandidateRetainedPayload
  'client.candidate.apply_result': ClientCandidateApplyResultPayload
  'client.command_ack': ClientCommandAckPayload
}

/** Kind → payload mapping for the Server → Client direction (§9.4). */
export interface ServerToClientPayloadByKind {
  'client.enrollment_accepted': ClientEnrollmentAcceptedPayload
  'client.access.challenge': ClientAccessChallengePayload
  'client.occupancy.offer': ClientOccupancyOfferPayload
  'client.occupancy.release': ClientOccupancyReleasePayload
  'client.occupancy.force_fence': ClientOccupancyForceFencePayload
  'client.repository.rescan': ClientRepositoryRescanPayload
  'client.worker.launch': ClientWorkerLaunchPayload
  'client.worker.stop': ClientWorkerStopPayload
  'client.candidate.apply': ClientCandidateApplyPayload
  'client.client_lock': ClientLockPayload
  'client.credential_rotate': ClientCredentialRotatePayload
}

export type ClientToServerKind = keyof ClientToServerPayloadByKind
export type ServerToClientKind = keyof ServerToClientPayloadByKind
export type ClientControlKind = ClientToServerKind | ServerToClientKind

export type ClientToServerPayload = ClientToServerPayloadByKind[ClientToServerKind]
export type ServerToClientPayload = ServerToClientPayloadByKind[ServerToClientKind]

export type ClientControlPayloadFor<K extends ClientControlKind> =
  (ClientToServerPayloadByKind & ServerToClientPayloadByKind)[K]

type IsNever<T> = [T] extends [never] ? true : false

type Assert<T extends true> = T

// Compile-time guards: the frozen kind lists and the kind→payload mappings must
// stay exactly aligned in both directions.
type AssertClientToServerKindListExact = Assert<
  IsNever<Exclude<ClientToServerKind, typeof CLIENT_TO_SERVER_MESSAGE_KINDS[number]>> extends true
    ? IsNever<Exclude<typeof CLIENT_TO_SERVER_MESSAGE_KINDS[number], ClientToServerKind>>
    : false
>
type AssertServerToClientKindListExact = Assert<
  IsNever<Exclude<ServerToClientKind, typeof SERVER_TO_CLIENT_MESSAGE_KINDS[number]>> extends true
    ? IsNever<Exclude<typeof SERVER_TO_CLIENT_MESSAGE_KINDS[number], ServerToClientKind>>
    : false
>
type AssertClientToServerFencedListExact = Assert<
  IsNever<
    Exclude<
      typeof CLIENT_TO_SERVER_OCCUPANCY_FENCED_MESSAGE_KINDS[number],
      ClientToServerKind
    >
  > extends true
    ? true
    : false
>
type AssertServerToClientFencedListExact = Assert<
  IsNever<
    Exclude<
      typeof SERVER_TO_CLIENT_OCCUPANCY_FENCED_MESSAGE_KINDS[number],
      ServerToClientKind
    >
  > extends true
    ? true
    : false
>

/** §9.5 Envelope. `kind` selects the payload shape; direction is contextual. */
export interface ClientControlEnvelope<K extends ClientControlKind = ClientControlKind> {
  readonly schemaVersion: typeof CLIENT_CONTROL_SCHEMA_VERSION
  readonly messageId: string
  readonly clientNodeId: ClientNodeId
  readonly clientInstanceId: ClientInstanceId
  readonly sequence: number
  readonly occurredAt: number
  readonly kind: K
  readonly payload: ClientControlPayloadFor<K>
}

export type ClientToServerMessage = ClientControlEnvelope<ClientToServerKind>

export type ServerToClientMessage = ClientControlEnvelope<ServerToClientKind>

export const CLIENT_CONTROL_DIRECTIONS = Object.freeze([
  'client-to-server',
  'server-to-client',
] as const)
export type ClientControlDirection = typeof CLIENT_CONTROL_DIRECTIONS[number]

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
  const createdAt = timestamp(input.createdAt, `${path}.createdAt`)
  const updatedAt = timestamp(input.updatedAt, `${path}.updatedAt`)
  if (updatedAt < createdAt) {
    controlError('INVALID_VALUE', `${path}.updatedAt`, 'user account update precedes creation')
  }
  return Object.freeze({
    userId: identifierAt(input.userId, `${path}.userId`, UserId),
    username: boundedText(input.username, `${path}.username`, 256),
    normalizedUsername: boundedText(input.normalizedUsername, `${path}.normalizedUsername`, 256),
    passwordHash: boundedText(input.passwordHash, `${path}.passwordHash`, 512),
    role: enumValue(input.role, USER_ACCOUNT_ROLES, `${path}.role`),
    state: enumValue(input.state, USER_ACCOUNT_STATES, `${path}.state`),
    createdAt,
    updatedAt,
    revision: positiveInteger(input.revision, `${path}.revision`),
  })
}

export function parseClientNodeEnrollment(
  value: unknown,
  path = 'clientNodeEnrollment',
): ClientNodeEnrollment {
  const input = record(value, path)
  exactKeys(input, [
    'publicClientId',
    'displayName',
    'platform',
    'architecture',
    'clientVersion',
    'deviceCredentialDigest',
    'maxConcurrentWorkerSessions',
  ], path)
  return Object.freeze({
    publicClientId: boundedText(input.publicClientId, `${path}.publicClientId`, 128),
    displayName: boundedText(input.displayName, `${path}.displayName`, 256),
    platform: enumValue(input.platform, CLIENT_NODE_PLATFORMS, `${path}.platform`),
    architecture: enumValue(input.architecture, CLIENT_NODE_ARCHITECTURES, `${path}.architecture`),
    clientVersion: boundedText(input.clientVersion, `${path}.clientVersion`, 128),
    deviceCredentialDigest: digestText(input.deviceCredentialDigest, `${path}.deviceCredentialDigest`),
    maxConcurrentWorkerSessions: positiveInteger(
      input.maxConcurrentWorkerSessions,
      `${path}.maxConcurrentWorkerSessions`,
    ),
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
  const running = nonNegativeInteger(
    input.reportedRunningWorkerSessions,
    `${path}.reportedRunningWorkerSessions`,
  )
  const maximum = positiveInteger(
    input.maxConcurrentWorkerSessions,
    `${path}.maxConcurrentWorkerSessions`,
  )
  if (running > maximum) {
    controlError(
      'INVALID_VALUE',
      `${path}.reportedRunningWorkerSessions`,
      'reported running WorkerSessions exceed the reported capacity',
    )
  }
  return Object.freeze({
    clientNodeId: identifierAt(input.clientNodeId, `${path}.clientNodeId`, ClientNodeId),
    publicClientId: boundedText(input.publicClientId, `${path}.publicClientId`, 128),
    displayName: boundedText(input.displayName, `${path}.displayName`, 256),
    platform: enumValue(input.platform, CLIENT_NODE_PLATFORMS, `${path}.platform`),
    architecture: enumValue(input.architecture, CLIENT_NODE_ARCHITECTURES, `${path}.architecture`),
    clientVersion: boundedText(input.clientVersion, `${path}.clientVersion`, 128),
    deviceCredentialDigest: digestText(input.deviceCredentialDigest, `${path}.deviceCredentialDigest`),
    currentInstanceId: input.currentInstanceId === null
      ? null
      : identifierAt(input.currentInstanceId, `${path}.currentInstanceId`, ClientInstanceId),
    presenceState: enumValue(input.presenceState, CLIENT_NODE_PRESENCE_STATES, `${path}.presenceState`),
    acceptingConnections: booleanValue(input.acceptingConnections, `${path}.acceptingConnections`),
    lockState: enumValue(input.lockState, CLIENT_NODE_LOCK_STATES, `${path}.lockState`),
    maxConcurrentWorkerSessions: maximum,
    reportedRunningWorkerSessions: running,
    lastHeartbeatAt: nullableTimestamp(input.lastHeartbeatAt, `${path}.lastHeartbeatAt`),
    createdAt: timestamp(input.createdAt, `${path}.createdAt`),
    revision: positiveInteger(input.revision, `${path}.revision`),
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
    connectCodeId: identifierAt(input.connectCodeId, `${path}.connectCodeId`, ClientConnectCodeId),
    clientNodeId: identifierAt(input.clientNodeId, `${path}.clientNodeId`, ClientNodeId),
    codeDigest: digestText(input.codeDigest, `${path}.codeDigest`),
    issuedByInstanceId: identifierAt(
      input.issuedByInstanceId,
      `${path}.issuedByInstanceId`,
      ClientInstanceId,
    ),
    expiresAt: timestamp(input.expiresAt, `${path}.expiresAt`),
    remainingAttempts: nonNegativeInteger(input.remainingAttempts, `${path}.remainingAttempts`),
    state: enumValue(input.state, CLIENT_CONNECT_CODE_STATES, `${path}.state`),
    createdAt: timestamp(input.createdAt, `${path}.createdAt`),
    revision: positiveInteger(input.revision, `${path}.revision`),
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
    clientAccessGrantId: identifierAt(
      input.clientAccessGrantId,
      `${path}.clientAccessGrantId`,
      ClientAccessGrantId,
    ),
    clientNodeId: identifierAt(input.clientNodeId, `${path}.clientNodeId`, ClientNodeId),
    userId: identifierAt(input.userId, `${path}.userId`, UserId),
    permissions: enumList(input.permissions, CLIENT_ACCESS_PERMISSIONS, `${path}.permissions`),
    trustMode: enumValue(input.trustMode, CLIENT_ACCESS_TRUST_MODES, `${path}.trustMode`),
    state: enumValue(input.state, CLIENT_ACCESS_GRANT_STATES, `${path}.state`),
    grantedByUserId: identifierAt(input.grantedByUserId, `${path}.grantedByUserId`, UserId),
    grantSource: enumValue(input.grantSource, CLIENT_ACCESS_GRANT_SOURCES, `${path}.grantSource`),
    expiresAt: nullableTimestamp(input.expiresAt, `${path}.expiresAt`),
    createdAt: timestamp(input.createdAt, `${path}.createdAt`),
    revision: positiveInteger(input.revision, `${path}.revision`),
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
    clientOccupancyLeaseId: identifierAt(
      input.clientOccupancyLeaseId,
      `${path}.clientOccupancyLeaseId`,
      ClientOccupancyLeaseId,
    ),
    clientNodeId: identifierAt(input.clientNodeId, `${path}.clientNodeId`, ClientNodeId),
    holderUserId: identifierAt(input.holderUserId, `${path}.holderUserId`, UserId),
    state: enumValue(input.state, CLIENT_OCCUPANCY_LEASE_STATES, `${path}.state`),
    fencingToken: positiveInteger(input.fencingToken, `${path}.fencingToken`),
    claimRequestId: portableReference(input.claimRequestId, `${path}.claimRequestId`),
    claimedAt: nullableTimestamp(input.claimedAt, `${path}.claimedAt`),
    acknowledgedAt: nullableTimestamp(input.acknowledgedAt, `${path}.acknowledgedAt`),
    lastRenewedAt: nullableTimestamp(input.lastRenewedAt, `${path}.lastRenewedAt`),
    idleExpiresAt: nullableTimestamp(input.idleExpiresAt, `${path}.idleExpiresAt`),
    recoveryDeadlineAt: nullableTimestamp(input.recoveryDeadlineAt, `${path}.recoveryDeadlineAt`),
    releaseRequestedAt: nullableTimestamp(input.releaseRequestedAt, `${path}.releaseRequestedAt`),
    releasedAt: nullableTimestamp(input.releasedAt, `${path}.releasedAt`),
    releaseReason: nullableText(input.releaseReason, `${path}.releaseReason`, 512),
    revision: positiveInteger(input.revision, `${path}.revision`),
  })
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
    repositoryBindingId: identifierAt(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
      RepositoryBindingId,
    ),
    clientNodeId: identifierAt(input.clientNodeId, `${path}.clientNodeId`, ClientNodeId),
    displayName: boundedText(input.displayName, `${path}.displayName`, 256),
    repositoryKind: enumValue(input.repositoryKind, REPOSITORY_KINDS, `${path}.repositoryKind`),
    defaultBranch: gitRefName(input.defaultBranch, `${path}.defaultBranch`),
    headCommit: nullableGitCommitId(input.headCommit, `${path}.headCommit`),
    dirtyState: enumValue(input.dirtyState, REPOSITORY_DIRTY_STATES, `${path}.dirtyState`),
    availability: enumValue(input.availability, REPOSITORY_AVAILABILITIES, `${path}.availability`),
    repositoryFingerprint: digestText(
      input.repositoryFingerprint,
      `${path}.repositoryFingerprint`,
    ),
    lastScannedAt: nullableTimestamp(input.lastScannedAt, `${path}.lastScannedAt`),
    revision: positiveInteger(input.revision, `${path}.revision`),
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
    repositoryAccessGrantId: identifierAt(
      input.repositoryAccessGrantId,
      `${path}.repositoryAccessGrantId`,
      RepositoryAccessGrantId,
    ),
    repositoryBindingId: identifierAt(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
      RepositoryBindingId,
    ),
    userId: identifierAt(input.userId, `${path}.userId`, UserId),
    permissions: enumList(
      input.permissions,
      REPOSITORY_ACCESS_PERMISSIONS,
      `${path}.permissions`,
    ),
    state: enumValue(input.state, REPOSITORY_ACCESS_GRANT_STATES, `${path}.state`),
    grantedByUserId: identifierAt(input.grantedByUserId, `${path}.grantedByUserId`, UserId),
    createdAt: timestamp(input.createdAt, `${path}.createdAt`),
    revision: positiveInteger(input.revision, `${path}.revision`),
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
    workerLaunchGrantId: identifierAt(
      input.workerLaunchGrantId,
      `${path}.workerLaunchGrantId`,
      WorkerLaunchGrantId,
    ),
    clientNodeId: identifierAt(input.clientNodeId, `${path}.clientNodeId`, ClientNodeId),
    clientInstanceId: identifierAt(
      input.clientInstanceId,
      `${path}.clientInstanceId`,
      ClientInstanceId,
    ),
    occupancyLeaseId: identifierAt(
      input.occupancyLeaseId,
      `${path}.occupancyLeaseId`,
      ClientOccupancyLeaseId,
    ),
    occupancyFencingToken: positiveInteger(
      input.occupancyFencingToken,
      `${path}.occupancyFencingToken`,
    ),
    repositoryBindingId: identifierAt(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
      RepositoryBindingId,
    ),
    productSessionId: identifierAt(
      input.productSessionId,
      `${path}.productSessionId`,
      ProductSessionId,
    ),
    stageRunId: identifierAt(input.stageRunId, `${path}.stageRunId`, StageRunId),
    workerSessionId: identifierAt(input.workerSessionId, `${path}.workerSessionId`, WorkerSessionId),
    workerId: identifierAt(input.workerId, `${path}.workerId`, WorkerId),
    workerInstanceId: identifierAt(
      input.workerInstanceId,
      `${path}.workerInstanceId`,
      WorkerInstanceId,
    ),
    credentialDigest: digestText(input.credentialDigest, `${path}.credentialDigest`),
    expiresAt: timestamp(input.expiresAt, `${path}.expiresAt`),
    state: enumValue(input.state, WORKER_LAUNCH_GRANT_STATES, `${path}.state`),
    revision: positiveInteger(input.revision, `${path}.revision`),
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
    localCandidateReceiptId: identifierAt(
      input.localCandidateReceiptId,
      `${path}.localCandidateReceiptId`,
      LocalCandidateReceiptId,
    ),
    candidateRef: boundedText(input.candidateRef, `${path}.candidateRef`, 4_096),
    repositoryBindingId: identifierAt(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
      RepositoryBindingId,
    ),
    candidateCommit: gitCommitId(input.candidateCommit, `${path}.candidateCommit`),
    localRefName: gitRefName(input.localRefName, `${path}.localRefName`),
    state: enumValue(input.state, LOCAL_CANDIDATE_RECEIPT_STATES, `${path}.state`),
    createdAt: timestamp(input.createdAt, `${path}.createdAt`),
    revision: positiveInteger(input.revision, `${path}.revision`),
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
    localApplyReceiptId: identifierAt(
      input.localApplyReceiptId,
      `${path}.localApplyReceiptId`,
      LocalApplyReceiptId,
    ),
    candidateRef: boundedText(input.candidateRef, `${path}.candidateRef`, 4_096),
    repositoryBindingId: identifierAt(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
      RepositoryBindingId,
    ),
    targetBranch: gitRefName(input.targetBranch, `${path}.targetBranch`),
    expectedHead: gitCommitId(input.expectedHead, `${path}.expectedHead`),
    strategy: enumValue(input.strategy, LOCAL_APPLY_STRATEGIES, `${path}.strategy`),
    result: enumValue(input.result, LOCAL_APPLY_RESULTS, `${path}.result`),
    resultingCommit: nullableGitCommitId(input.resultingCommit, `${path}.resultingCommit`),
    conflictArtifactRef: nullableText(input.conflictArtifactRef, `${path}.conflictArtifactRef`, 4_096),
    createdAt: timestamp(input.createdAt, `${path}.createdAt`),
    revision: positiveInteger(input.revision, `${path}.revision`),
  })
}

export function parseClientWorkerReconcileReport(
  value: unknown,
  path = 'clientWorkerReconcileReport',
): ClientWorkerReconcileReport {
  const input = record(value, path)
  exactKeys(input, ['workerSessionId', 'state'], path)
  return Object.freeze({
    workerSessionId: identifierAt(input.workerSessionId, `${path}.workerSessionId`, WorkerSessionId),
    state: enumValue(input.state, WORKER_RECONCILE_STATES, `${path}.state`),
  })
}

function parseCommandBase(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientControlCommand {
  return {
    expectedRevision: nonNegativeInteger(input.expectedRevision, `${path}.expectedRevision`),
    idempotencyKey: portableReference(input.idempotencyKey, `${path}.idempotencyKey`),
  }
}

function parseOccupancyCommandBase(
  input: Readonly<Record<string, unknown>>,
  path: string,
): ClientControlOccupancyCommand {
  return {
    ...parseCommandBase(input, path),
    occupancyLeaseId: identifierAt(
      input.occupancyLeaseId,
      `${path}.occupancyLeaseId`,
      ClientOccupancyLeaseId,
    ),
    occupancyFencingToken: positiveInteger(
      input.occupancyFencingToken,
      `${path}.occupancyFencingToken`,
    ),
  }
}

function parseClientEnrollPayload(value: unknown, path: string): ClientEnrollPayload {
  const input = record(value, path)
  exactKeys(input, ['expectedRevision', 'idempotencyKey', 'node'], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    node: parseClientNodeEnrollment(input.node, `${path}.node`),
  })
}

function parseClientHelloPayload(value: unknown, path: string): ClientHelloPayload {
  const input = record(value, path)
  exactKeys(input, ['expectedRevision', 'idempotencyKey', 'clientVersion', 'currentInstanceId', 'presenceState'], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    clientVersion: boundedText(input.clientVersion, `${path}.clientVersion`, 128),
    currentInstanceId: identifierAt(
      input.currentInstanceId,
      `${path}.currentInstanceId`,
      ClientInstanceId,
    ),
    presenceState: enumValue(input.presenceState, CLIENT_NODE_PRESENCE_STATES, `${path}.presenceState`),
  })
}

function parseClientHeartbeatPayload(value: unknown, path: string): ClientHeartbeatPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'presenceState',
    'acceptingConnections',
    'lockState',
    'maxConcurrentWorkerSessions',
    'reportedRunningWorkerSessions',
  ], path)
  const running = nonNegativeInteger(
    input.reportedRunningWorkerSessions,
    `${path}.reportedRunningWorkerSessions`,
  )
  const maximum = positiveInteger(
    input.maxConcurrentWorkerSessions,
    `${path}.maxConcurrentWorkerSessions`,
  )
  if (running > maximum) {
    controlError(
      'INVALID_VALUE',
      `${path}.reportedRunningWorkerSessions`,
      'reported running WorkerSessions exceed the reported capacity',
    )
  }
  return Object.freeze({
    ...parseCommandBase(input, path),
    presenceState: enumValue(input.presenceState, CLIENT_NODE_PRESENCE_STATES, `${path}.presenceState`),
    acceptingConnections: booleanValue(input.acceptingConnections, `${path}.acceptingConnections`),
    lockState: enumValue(input.lockState, CLIENT_NODE_LOCK_STATES, `${path}.lockState`),
    maxConcurrentWorkerSessions: maximum,
    reportedRunningWorkerSessions: running,
  })
}

function parseClientConnectCodePublishedPayload(
  value: unknown,
  path: string,
): ClientConnectCodePublishedPayload {
  const input = record(value, path)
  exactKeys(input, ['expectedRevision', 'idempotencyKey', 'connectCode'], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    connectCode: parseClientConnectCode(input.connectCode, `${path}.connectCode`),
  })
}

function parseClientAccessChallengeAckPayload(
  value: unknown,
  path: string,
): ClientAccessChallengeAckPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'challengeId',
    'connectCodeId',
    'accepted',
    'rejectionReason',
  ], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    challengeId: portableReference(input.challengeId, `${path}.challengeId`),
    connectCodeId: identifierAt(
      input.connectCodeId,
      `${path}.connectCodeId`,
      ClientConnectCodeId,
    ),
    accepted: booleanValue(input.accepted, `${path}.accepted`),
    rejectionReason: nullableText(input.rejectionReason, `${path}.rejectionReason`, 512),
  })
}

function parseClientOccupancyAckPayload(value: unknown, path: string): ClientOccupancyAckPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'acknowledgedAt',
  ], path)
  const base = parseOccupancyCommandBase(input, path)
  const acknowledgedAt = timestamp(input.acknowledgedAt, `${path}.acknowledgedAt`)
  return Object.freeze({ ...base, acknowledgedAt })
}

function parseClientOccupancyRejectedPayload(
  value: unknown,
  path: string,
): ClientOccupancyRejectedPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'reason',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    reason: boundedText(input.reason, `${path}.reason`, 512),
  })
}

function parseClientRepositoryUpsertPayload(
  value: unknown,
  path: string,
): ClientRepositoryUpsertPayload {
  const input = record(value, path)
  exactKeys(input, ['expectedRevision', 'idempotencyKey', 'binding'], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    binding: parseRepositoryBinding(input.binding, `${path}.binding`),
  })
}

function parseClientRepositoryRemovedPayload(
  value: unknown,
  path: string,
): ClientRepositoryRemovedPayload {
  const input = record(value, path)
  exactKeys(input, ['expectedRevision', 'idempotencyKey', 'repositoryBindingId'], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    repositoryBindingId: identifierAt(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
      RepositoryBindingId,
    ),
  })
}

function parseClientRepositoryStatusPayload(
  value: unknown,
  path: string,
): ClientRepositoryStatusPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'repositoryBindingId',
    'dirtyState',
    'availability',
    'headCommit',
    'lastScannedAt',
  ], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    repositoryBindingId: identifierAt(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
      RepositoryBindingId,
    ),
    dirtyState: enumValue(input.dirtyState, REPOSITORY_DIRTY_STATES, `${path}.dirtyState`),
    availability: enumValue(input.availability, REPOSITORY_AVAILABILITIES, `${path}.availability`),
    headCommit: nullableGitCommitId(input.headCommit, `${path}.headCommit`),
    lastScannedAt: timestamp(input.lastScannedAt, `${path}.lastScannedAt`),
  })
}

function parseClientWorkerLaunchAckPayload(
  value: unknown,
  path: string,
): ClientWorkerLaunchAckPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'workerLaunchGrantId',
    'workerSessionId',
    'workerInstanceId',
    'accepted',
    'reason',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    workerLaunchGrantId: identifierAt(
      input.workerLaunchGrantId,
      `${path}.workerLaunchGrantId`,
      WorkerLaunchGrantId,
    ),
    workerSessionId: identifierAt(input.workerSessionId, `${path}.workerSessionId`, WorkerSessionId),
    workerInstanceId: identifierAt(
      input.workerInstanceId,
      `${path}.workerInstanceId`,
      WorkerInstanceId,
    ),
    accepted: booleanValue(input.accepted, `${path}.accepted`),
    reason: nullableText(input.reason, `${path}.reason`, 512),
  })
}

function parseClientWorkerStatePayload(value: unknown, path: string): ClientWorkerStatePayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'workerSessionId',
    'state',
    'detail',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    workerSessionId: identifierAt(input.workerSessionId, `${path}.workerSessionId`, WorkerSessionId),
    state: boundedText(input.state, `${path}.state`, 64),
    detail: nullableText(input.detail, `${path}.detail`, 1_024),
  })
}

function parseClientWorkerReconcilePayload(
  value: unknown,
  path: string,
): ClientWorkerReconcilePayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'reports',
  ], path)
  if (!Array.isArray(input.reports) || input.reports.length > MAX_COLLECTION_LENGTH) {
    controlError(
      'INVALID_VALUE',
      `${path}.reports`,
      `${path}.reports must be an array with at most ${String(MAX_COLLECTION_LENGTH)} entries`,
    )
  }
  const reports = input.reports.map((entry, index) => (
    parseClientWorkerReconcileReport(entry, `${path}.reports[${String(index)}]`)
  ))
  const workerSessionIds = new Set(reports.map(entry => entry.workerSessionId))
  if (workerSessionIds.size !== reports.length) {
    controlError('DUPLICATE_ID', `${path}.reports`, `${path}.reports contains duplicate identities`)
  }
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    reports: Object.freeze(reports),
  })
}

function parseClientCandidateRetainedPayload(
  value: unknown,
  path: string,
): ClientCandidateRetainedPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'receipt',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    receipt: parseLocalCandidateReceipt(input.receipt, `${path}.receipt`),
  })
}

function parseClientCandidateApplyResultPayload(
  value: unknown,
  path: string,
): ClientCandidateApplyResultPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'receipt',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    receipt: parseLocalApplyReceipt(input.receipt, `${path}.receipt`),
  })
}

function parseClientCommandAckPayload(value: unknown, path: string): ClientCommandAckPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'acknowledgedMessageId',
    'accepted',
    'reason',
  ], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    acknowledgedMessageId: portableReference(
      input.acknowledgedMessageId,
      `${path}.acknowledgedMessageId`,
    ),
    accepted: booleanValue(input.accepted, `${path}.accepted`),
    reason: nullableText(input.reason, `${path}.reason`, 512),
  })
}

function parseClientEnrollmentAcceptedPayload(
  value: unknown,
  path: string,
): ClientEnrollmentAcceptedPayload {
  const input = record(value, path)
  exactKeys(input, ['expectedRevision', 'idempotencyKey', 'presenceState'], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    presenceState: enumValue(input.presenceState, CLIENT_NODE_PRESENCE_STATES, `${path}.presenceState`),
  })
}

function parseClientAccessChallengePayload(
  value: unknown,
  path: string,
): ClientAccessChallengePayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'challengeId',
    'connectCodeId',
    'expiresAt',
  ], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    challengeId: portableReference(input.challengeId, `${path}.challengeId`),
    connectCodeId: identifierAt(input.connectCodeId, `${path}.connectCodeId`, ClientConnectCodeId),
    expiresAt: timestamp(input.expiresAt, `${path}.expiresAt`),
  })
}

function parseClientOccupancyOfferPayload(value: unknown, path: string): ClientOccupancyOfferPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'holderUserId',
    'claimRequestId',
    'idleExpiresAt',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    holderUserId: identifierAt(input.holderUserId, `${path}.holderUserId`, UserId),
    claimRequestId: portableReference(input.claimRequestId, `${path}.claimRequestId`),
    idleExpiresAt: nullableTimestamp(input.idleExpiresAt, `${path}.idleExpiresAt`),
  })
}

function parseClientOccupancyReleasePayload(
  value: unknown,
  path: string,
): ClientOccupancyReleasePayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'releaseReason',
    'drain',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    releaseReason: nullableText(input.releaseReason, `${path}.releaseReason`, 512),
    drain: booleanValue(input.drain, `${path}.drain`),
  })
}

function parseClientOccupancyForceFencePayload(
  value: unknown,
  path: string,
): ClientOccupancyForceFencePayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'reason',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    reason: nullableText(input.reason, `${path}.reason`, 512),
  })
}

function parseClientRepositoryRescanPayload(
  value: unknown,
  path: string,
): ClientRepositoryRescanPayload {
  const input = record(value, path)
  exactKeys(input, ['expectedRevision', 'idempotencyKey', 'repositoryBindingId'], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    repositoryBindingId: input.repositoryBindingId === null
      ? null
      : identifierAt(input.repositoryBindingId, `${path}.repositoryBindingId`, RepositoryBindingId),
  })
}

function parseClientWorkerLaunchPayload(value: unknown, path: string): ClientWorkerLaunchPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'grant',
  ], path)
  const base = parseOccupancyCommandBase(input, path)
  const grant = parseWorkerLaunchGrant(input.grant, `${path}.grant`)
  if (grant.occupancyLeaseId !== base.occupancyLeaseId
    || grant.occupancyFencingToken !== base.occupancyFencingToken) {
    controlError(
      'RELATIONSHIP_MISMATCH',
      `${path}.grant`,
      'worker launch grant does not match the occupancy identity on its command',
    )
  }
  return Object.freeze({
    ...base,
    grant,
  })
}

function parseClientWorkerStopPayload(value: unknown, path: string): ClientWorkerStopPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'workerSessionId',
    'reason',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    workerSessionId: identifierAt(input.workerSessionId, `${path}.workerSessionId`, WorkerSessionId),
    reason: nullableText(input.reason, `${path}.reason`, 512),
  })
}

function parseClientCandidateApplyPayload(value: unknown, path: string): ClientCandidateApplyPayload {
  const input = record(value, path)
  exactKeys(input, [
    'expectedRevision',
    'idempotencyKey',
    'occupancyLeaseId',
    'occupancyFencingToken',
    'candidateRef',
    'repositoryBindingId',
    'targetBranch',
    'expectedHead',
    'strategy',
  ], path)
  return Object.freeze({
    ...parseOccupancyCommandBase(input, path),
    candidateRef: boundedText(input.candidateRef, `${path}.candidateRef`, 4_096),
    repositoryBindingId: identifierAt(
      input.repositoryBindingId,
      `${path}.repositoryBindingId`,
      RepositoryBindingId,
    ),
    targetBranch: gitRefName(input.targetBranch, `${path}.targetBranch`),
    expectedHead: gitCommitId(input.expectedHead, `${path}.expectedHead`),
    strategy: enumValue(input.strategy, LOCAL_APPLY_STRATEGIES, `${path}.strategy`),
  })
}

function parseClientLockPayload(value: unknown, path: string): ClientLockPayload {
  const input = record(value, path)
  exactKeys(input, ['expectedRevision', 'idempotencyKey', 'locked', 'reason'], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    locked: booleanValue(input.locked, `${path}.locked`),
    reason: nullableText(input.reason, `${path}.reason`, 512),
  })
}

function parseClientCredentialRotatePayload(
  value: unknown,
  path: string,
): ClientCredentialRotatePayload {
  const input = record(value, path)
  exactKeys(input, ['expectedRevision', 'idempotencyKey', 'reason'], path)
  return Object.freeze({
    ...parseCommandBase(input, path),
    reason: nullableText(input.reason, `${path}.reason`, 512),
  })
}

function parseClientToServerPayload(
  kind: ClientToServerKind,
  value: unknown,
  path: string,
): ClientToServerPayload {
  switch (kind) {
    case 'client.enroll':
      return parseClientEnrollPayload(value, path)
    case 'client.hello':
      return parseClientHelloPayload(value, path)
    case 'client.heartbeat':
      return parseClientHeartbeatPayload(value, path)
    case 'client.connect_code.published':
      return parseClientConnectCodePublishedPayload(value, path)
    case 'client.access.challenge_ack':
      return parseClientAccessChallengeAckPayload(value, path)
    case 'client.occupancy.ack':
      return parseClientOccupancyAckPayload(value, path)
    case 'client.occupancy.rejected':
      return parseClientOccupancyRejectedPayload(value, path)
    case 'client.repository.upsert':
      return parseClientRepositoryUpsertPayload(value, path)
    case 'client.repository.removed':
      return parseClientRepositoryRemovedPayload(value, path)
    case 'client.repository.status':
      return parseClientRepositoryStatusPayload(value, path)
    case 'client.worker.launch_ack':
      return parseClientWorkerLaunchAckPayload(value, path)
    case 'client.worker.state':
      return parseClientWorkerStatePayload(value, path)
    case 'client.worker.reconcile':
      return parseClientWorkerReconcilePayload(value, path)
    case 'client.candidate.retained':
      return parseClientCandidateRetainedPayload(value, path)
    case 'client.candidate.apply_result':
      return parseClientCandidateApplyResultPayload(value, path)
    case 'client.command_ack':
      return parseClientCommandAckPayload(value, path)
  }
}

function parseServerToClientPayload(
  kind: ServerToClientKind,
  value: unknown,
  path: string,
): ServerToClientPayload {
  switch (kind) {
    case 'client.enrollment_accepted':
      return parseClientEnrollmentAcceptedPayload(value, path)
    case 'client.access.challenge':
      return parseClientAccessChallengePayload(value, path)
    case 'client.occupancy.offer':
      return parseClientOccupancyOfferPayload(value, path)
    case 'client.occupancy.release':
      return parseClientOccupancyReleasePayload(value, path)
    case 'client.occupancy.force_fence':
      return parseClientOccupancyForceFencePayload(value, path)
    case 'client.repository.rescan':
      return parseClientRepositoryRescanPayload(value, path)
    case 'client.worker.launch':
      return parseClientWorkerLaunchPayload(value, path)
    case 'client.worker.stop':
      return parseClientWorkerStopPayload(value, path)
    case 'client.candidate.apply':
      return parseClientCandidateApplyPayload(value, path)
    case 'client.client_lock':
      return parseClientLockPayload(value, path)
    case 'client.credential_rotate':
      return parseClientCredentialRotatePayload(value, path)
  }
}

function parseClientControlMessageBase(
  value: unknown,
  allowedKinds: readonly string[],
  path: string,
): {
  readonly schemaVersion: typeof CLIENT_CONTROL_SCHEMA_VERSION
  readonly messageId: string
  readonly clientNodeId: ClientNodeId
  readonly clientInstanceId: ClientInstanceId
  readonly sequence: number
  readonly occurredAt: number
  readonly kind: string
  readonly payload: Record<string, unknown>
} {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'messageId',
    'clientNodeId',
    'clientInstanceId',
    'sequence',
    'occurredAt',
    'kind',
    'payload',
  ], path)
  return {
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    messageId: portableReference(input.messageId, `${path}.messageId`),
    clientNodeId: identifierAt(input.clientNodeId, `${path}.clientNodeId`, ClientNodeId),
    clientInstanceId: identifierAt(
      input.clientInstanceId,
      `${path}.clientInstanceId`,
      ClientInstanceId,
    ),
    sequence: nonNegativeInteger(input.sequence, `${path}.sequence`),
    occurredAt: timestamp(input.occurredAt, `${path}.occurredAt`),
    kind: enumValue(input.kind, allowedKinds, `${path}.kind`),
    payload: record(input.payload, `${path}.payload`),
  }
}

/** Parse and validate one Client → Server message (§9.3 kinds, §9.5 envelope). */
export function parseClientToServerMessage(
  value: unknown,
  path = 'clientToServerMessage',
): ClientToServerMessage {
  const parsed = parseClientControlMessageBase(
    value,
    CLIENT_TO_SERVER_MESSAGE_KINDS,
    path,
  )
  const kind = parsed.kind as ClientToServerKind
  const payload = parseClientToServerPayload(kind, parsed.payload, `${path}.payload`)
  return Object.freeze({
    schemaVersion: parsed.schemaVersion,
    messageId: parsed.messageId,
    clientNodeId: parsed.clientNodeId,
    clientInstanceId: parsed.clientInstanceId,
    sequence: parsed.sequence,
    occurredAt: parsed.occurredAt,
    kind,
    payload,
  })
}

/** Parse and validate one Server → Client message (§9.4 kinds, §9.5 envelope). */
export function parseServerToClientMessage(
  value: unknown,
  path = 'serverToClientMessage',
): ServerToClientMessage {
  const parsed = parseClientControlMessageBase(
    value,
    SERVER_TO_CLIENT_MESSAGE_KINDS,
    path,
  )
  const kind = parsed.kind as ServerToClientKind
  const payload = parseServerToClientPayload(kind, parsed.payload, `${path}.payload`)
  return Object.freeze({
    schemaVersion: parsed.schemaVersion,
    messageId: parsed.messageId,
    clientNodeId: parsed.clientNodeId,
    clientInstanceId: parsed.clientInstanceId,
    sequence: parsed.sequence,
    occurredAt: parsed.occurredAt,
    kind,
    payload,
  })
}

type ClientControlKindListsCheck = Assert<
  AssertClientToServerKindListExact extends true
    ? AssertServerToClientKindListExact extends true
      ? AssertClientToServerFencedListExact extends true
        ? AssertServerToClientFencedListExact
        : false
      : false
    : false
>
