import { createHash, randomUUID } from 'node:crypto'
import {
  mkdir,
  open,
  readFile,
  realpath,
  rename,
  rm,
  stat,
  unlink,
} from 'node:fs/promises'
import { dirname, isAbsolute, join, resolve } from 'node:path'

import {
  AttemptId,
  CandidateId,
  JobId,
  KernelSessionId,
  SourceSnapshotId,
  StageRunId,
  STRONGFLOW_ROLE_IDS,
  StrongFlowWorkspaceId,
  VerificationSnapshotId,
  parseStrongFlowRoleConfiguration,
  type AttemptId as AttemptIdentifier,
  type JobId as JobIdentifier,
  type KernelSessionId as KernelSessionIdentifier,
  type StageRunId as StageRunIdentifier,
  type StrongFlowRoleConfiguration,
  type StrongFlowRoleId,
  type StrongFlowRoleModelCatalogEntry,
  type StrongFlowRoleSpec,
  type StrongFlowRoleWorkspaceAssignment,
} from '@winwincode/contracts'
import type {
  ApprovalResponse,
  DynamicToolResponse,
  EventStreamOptions,
  GovernedSessionEffectivePolicy,
  KernelEvent,
  ResumeOptions,
  SessionInfo,
  SessionOptions,
  SubmissionInfo,
} from '@winwincode/native'

import {
  StrongFlowRoleAuthorityError,
  createStrongFlowRoleKernelAuthority,
  verifyStrongFlowRoleKernelEvidence,
} from './role-authority.js'

export const STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION = 2 as const

const LINEAGE_PREFIX = 'kernel-lineage-sha256-'
const CONTEXT_PREFIX = 'role-context-sha256-'
const ROLE_SPEC_PREFIX = 'role-spec-sha256-'
const STREAM_PREFIX = 'kernel-stream-sha256-'
const DEFAULT_EVENT_BUFFER_CAPACITY = 256
const MAX_EVENT_BUFFER_CAPACITY = 65_536
const PORTABLE_REASON_MAX_LENGTH = 500
const MAX_TURN_INPUT_BYTES = 16 * 1024 * 1024

declare const strongFlowRoleSessionIdentifierBrand: unique symbol

type StrongFlowRoleSessionIdentifier<Name extends string> = string & {
  readonly [strongFlowRoleSessionIdentifierBrand]: Name
}

export type StrongFlowKernelSessionLineageId = StrongFlowRoleSessionIdentifier<
  'StrongFlowKernelSessionLineageId'
>
export type StrongFlowRoleContextId = StrongFlowRoleSessionIdentifier<'StrongFlowRoleContextId'>
export type StrongFlowRoleSpecId = StrongFlowRoleSessionIdentifier<'StrongFlowRoleSpecId'>

export type StrongFlowRoleSessionErrorCode =
  | 'INVALID_MANAGER_OPTIONS'
  | 'INVALID_ASSIGNMENT'
  | 'SESSION_ALREADY_EXISTS'
  | 'SESSION_NOT_FOUND'
  | 'SESSION_BUSY'
  | 'SESSION_ACTIVE'
  | 'SESSION_TERMINAL'
  | 'ROLE_SNAPSHOT_MISMATCH'
  | 'CONTEXT_SNAPSHOT_MISMATCH'
  | 'CONTEXT_INSTALLATION_MISMATCH'
  | 'ENFORCEMENT_UNAVAILABLE'
  | 'SESSION_SETUP_FAILED'
  | 'SESSION_SETUP_ROLLBACK_FAILED'
  | 'SESSION_STORE_CORRUPT'
  | 'EVENT_SUBSCRIBER_EXISTS'
  | 'EVENT_SEQUENCE_INVALID'
  | 'EVENT_STREAM_FAILED'
  | 'SESSION_NOT_READY'
  | 'TURN_ALREADY_SUBMITTED'
  | 'TURN_INPUT_INVALID'
  | 'TURN_SUBMISSION_FAILED'
  | 'TEARDOWN_FAILED'

/** Stable failure at the governed StrongFlow role-session boundary. */
export class StrongFlowRoleSessionError extends Error {
  readonly code: StrongFlowRoleSessionErrorCode

  constructor(
    code: StrongFlowRoleSessionErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowRoleSessionError'
    this.code = code
  }
}

export interface StrongFlowRoleSessionAssignment {
  readonly jobId: JobIdentifier
  readonly stageRunId: StageRunIdentifier
  readonly attemptId: AttemptIdentifier
  readonly roleId: StrongFlowRoleId
  readonly workspace: StrongFlowRoleWorkspaceAssignment
}

export interface StrongFlowRoleSessionContext {
  readonly schemaVersion: typeof STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION
  readonly kernelSessionLineageId: StrongFlowKernelSessionLineageId
  readonly contextId: StrongFlowRoleContextId
  readonly roleSpecId: StrongFlowRoleSpecId
  readonly jobId: JobIdentifier
  readonly stageRunId: StageRunIdentifier
  readonly attemptId: AttemptIdentifier
  readonly roleSpec: StrongFlowRoleSpec
  readonly workspace: StrongFlowRoleWorkspaceAssignment
}

export interface StrongFlowRoleKernelLifecycle {
  readonly generation: number
  readonly source: 'create' | 'resume'
  readonly kernelSessionId: KernelSessionIdentifier
  readonly kernelStreamId: string
  readonly rolloutPath: string
  readonly acceptedAtMillis: number
  readonly effectivePolicy: GovernedSessionEffectivePolicy
}

export interface StrongFlowRoleKernelEvent {
  readonly kernelSessionLineageId: StrongFlowKernelSessionLineageId
  readonly contextId: StrongFlowRoleContextId
  readonly generation: number
  readonly kernelSessionId: KernelSessionIdentifier
  readonly kernelStreamId: string
  readonly event: KernelEvent
}

export interface StrongFlowRoleContextInstallationRequest {
  readonly source: 'create' | 'resume'
  readonly context: StrongFlowRoleSessionContext
  readonly kernel: StrongFlowRoleKernelLifecycle
  readonly signal: AbortSignal
}

export interface StrongFlowRoleContextInstallationDisposal {
  readonly outcome: 'completed' | 'cancelled' | 'failed' | 'rollback'
  readonly reason: string
}

export interface StrongFlowRoleContextInstallation {
  readonly contextId: StrongFlowRoleContextId
  handleEvent(event: StrongFlowRoleKernelEvent): Promise<void> | void
  dispose(disposal: StrongFlowRoleContextInstallationDisposal): Promise<void> | void
}

/** Installs the immutable role prompt, tools, sandbox, workspace, and budget before publication. */
export interface StrongFlowRoleContextInstaller {
  install(
    request: StrongFlowRoleContextInstallationRequest,
  ): Promise<StrongFlowRoleContextInstallation>
}

/** Exact native methods owned by the StrongFlow role-session manager. */
export interface StrongFlowRoleKernelPort {
  createSession(options: SessionOptions): Promise<SessionInfo>
  resumeSession(options: ResumeOptions): Promise<SessionInfo>
  submitTurn(sessionId: string, text: string): Promise<SubmissionInfo>
  interrupt(sessionId: string): Promise<string>
  resolveApproval(response: ApprovalResponse): Promise<string>
  resolveDynamicTool(response: DynamicToolResponse): Promise<string>
  closeSession(sessionId: string): Promise<void>
  events(sessionId: string, options?: EventStreamOptions): AsyncIterable<KernelEvent>
}

export type StrongFlowRoleSessionState =
  | 'ready'
  | 'cancelling'
  | 'disposing'
  | 'cancelled'
  | 'closed'
  | 'failed'

export interface StrongFlowRoleSessionSummary {
  readonly kernelSessionLineageId: StrongFlowKernelSessionLineageId
  readonly contextId: StrongFlowRoleContextId
  readonly jobId: JobIdentifier
  readonly stageRunId: StageRunIdentifier
  readonly attemptId: AttemptIdentifier
  readonly roleId: StrongFlowRoleId
  readonly kernelSessionId: KernelSessionIdentifier
  readonly kernelStreamId: string
  readonly generation: number
  readonly state: StrongFlowRoleSessionState
}

export interface StrongFlowRoleSessionFailureOptions {
  readonly interrupt?: boolean
}

export interface StrongFlowRoleSession {
  readonly context: StrongFlowRoleSessionContext
  readonly kernel: StrongFlowRoleKernelLifecycle
  readonly state: StrongFlowRoleSessionState
  readonly eventFailure: StrongFlowRoleSessionError | undefined
  readonly summary: StrongFlowRoleSessionSummary
  events(): AsyncIterable<StrongFlowRoleKernelEvent>
  submitTurn(text: string): Promise<SubmissionInfo>
  cancel(reason: string): Promise<void>
  fail(reason: string, options?: StrongFlowRoleSessionFailureOptions): Promise<void>
  dispose(): Promise<void>
}

export interface StrongFlowRoleSessionManagerOptions {
  readonly home: string
  readonly kernel: StrongFlowRoleKernelPort
  readonly roleConfiguration: StrongFlowRoleConfiguration
  readonly modelCatalog: readonly StrongFlowRoleModelCatalogEntry[]
  readonly installer: StrongFlowRoleContextInstaller
  readonly eventBufferCapacity?: number
  readonly now?: () => number
}

interface AcceptedLifecycleRecord extends StrongFlowRoleKernelLifecycle {
  readonly schemaVersion: typeof STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION
  readonly recordType: 'kernel.accepted'
}

interface TerminalLifecycleRecord {
  readonly schemaVersion: typeof STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION
  readonly recordType: 'session.terminal'
  readonly generation: number
  readonly outcome: 'completed' | 'cancelled' | 'failed'
  readonly reason: string
  readonly occurredAtMillis: number
}

type LifecycleRecord = AcceptedLifecycleRecord | TerminalLifecycleRecord

interface StoredRoleSession {
  readonly directory: string
  readonly contextPath: string
  readonly lifecyclePath: string
  readonly ownerPath: string
  readonly context: StrongFlowRoleSessionContext
  readonly records: readonly LifecycleRecord[]
  readonly latestAccepted: AcceptedLifecycleRecord
  readonly terminal?: TerminalLifecycleRecord
}

interface OwnerRecord {
  readonly schemaVersion: typeof STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION
  readonly kernelSessionLineageId: StrongFlowKernelSessionLineageId
  readonly token: string
  readonly pid: number
  readonly acquiredAtMillis: number
}

interface PreparedSession {
  readonly context: StrongFlowRoleSessionContext
  readonly kernel: StrongFlowRoleKernelLifecycle
  readonly runtime: ManagedStrongFlowRoleSession
  readonly owner: OwnerRecord
}

interface Deferred<Value> {
  readonly promise: Promise<Value>
  readonly resolve: (value: Value | PromiseLike<Value>) => void
  readonly reject: (reason?: unknown) => void
}

function deferred<Value>(): Deferred<Value> {
  let resolvePromise: Deferred<Value>['resolve'] = () => {}
  let rejectPromise: Deferred<Value>['reject'] = () => {}
  const promise = new Promise<Value>((resolvePromiseInput, rejectPromiseInput) => {
    resolvePromise = resolvePromiseInput
    rejectPromise = rejectPromiseInput
  })
  return { promise, resolve: resolvePromise, reject: rejectPromise }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function isObject(value: unknown): value is object {
  return typeof value === 'object' && value !== null
}

function errorCode(error: unknown): string | undefined {
  if (!isObject(error) || !('code' in error)) return undefined
  return typeof error.code === 'string' ? error.code : undefined
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
  label: string,
): void {
  const allowed = new Set([...required, ...optional])
  if (
    required.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !allowed.has(key))
  ) throw new Error(`${label} has an unexpected shape`)
}

function immutableJson<Value>(value: Value): Value {
  if (Array.isArray(value)) {
    for (const entry of value) immutableJson(entry)
    return Object.freeze(value)
  }
  if (isRecord(value)) {
    for (const entry of Object.values(value)) immutableJson(entry)
    return Object.freeze(value) as Value
  }
  return value
}

function digest(kind: string, fields: readonly string[]): string {
  const hash = createHash('sha256')
  hash.update(`${Buffer.byteLength(kind)}:${kind}`)
  for (const field of fields) hash.update(`${Buffer.byteLength(field)}:${field}`)
  return hash.digest('hex')
}

function portableReason(value: string, label: string): string {
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.length > PORTABLE_REASON_MAX_LENGTH
    || value.trim() !== value
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)
  ) throw new StrongFlowRoleSessionError('INVALID_ASSIGNMENT', `${label} is invalid`)
  return value
}

function safeNow(now: () => number): number {
  const value = now()
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new StrongFlowRoleSessionError(
      'INVALID_MANAGER_OPTIONS',
      'role-session clock returned an invalid time',
    )
  }
  return value
}

function validateHome(value: string): string {
  if (typeof value !== 'string' || value.length === 0) {
    throw new StrongFlowRoleSessionError(
      'INVALID_MANAGER_OPTIONS',
      'StrongFlow role-session home must be a non-empty path',
    )
  }
  return resolve(value)
}

function validateEventCapacity(value: number | undefined): number {
  const capacity = value ?? DEFAULT_EVENT_BUFFER_CAPACITY
  if (
    !Number.isSafeInteger(capacity)
    || capacity < 1
    || capacity > MAX_EVENT_BUFFER_CAPACITY
  ) {
    throw new StrongFlowRoleSessionError(
      'INVALID_MANAGER_OPTIONS',
      `role-session event buffer capacity must be between 1 and ${MAX_EVENT_BUFFER_CAPACITY}`,
    )
  }
  return capacity
}

function validatedRoleConfiguration(
  input: StrongFlowRoleConfiguration,
  modelCatalog: readonly StrongFlowRoleModelCatalogEntry[],
): StrongFlowRoleConfiguration {
  try {
    return parseStrongFlowRoleConfiguration(
      structuredClone(input),
      structuredClone(modelCatalog),
    )
  } catch (error) {
    throw new StrongFlowRoleSessionError(
      'INVALID_MANAGER_OPTIONS',
      'StrongFlow role configuration is not a validated canonical configuration',
      { cause: error },
    )
  }
}

function lineageIdFor(
  jobId: JobIdentifier,
  stageRunId: StageRunIdentifier,
  attemptId: AttemptIdentifier,
  roleId: StrongFlowRoleId,
): StrongFlowKernelSessionLineageId {
  return `${LINEAGE_PREFIX}${digest('kernel-session-lineage', [
    jobId,
    stageRunId,
    attemptId,
    roleId,
  ])}` as StrongFlowKernelSessionLineageId
}

/** Returns the stable lookup identity for one job, attempt, stage run, and role. */
export function createStrongFlowKernelSessionLineageId(
  input: Pick<StrongFlowRoleSessionAssignment, 'jobId' | 'stageRunId' | 'attemptId' | 'roleId'>,
): StrongFlowKernelSessionLineageId {
  try {
    if (!isRecord(input)) throw new Error('lineage input must be an object')
    exactKeys(input, ['jobId', 'stageRunId', 'attemptId', 'roleId'], [], 'lineage input')
    const jobId = JobId(input.jobId)
    const stageRunId = StageRunId(input.stageRunId)
    const attemptId = AttemptId(input.attemptId)
    if (
      typeof input.roleId !== 'string'
      || !STRONGFLOW_ROLE_IDS.includes(input.roleId as StrongFlowRoleId)
    ) throw new Error('roleId is unknown')
    return lineageIdFor(jobId, stageRunId, attemptId, input.roleId as StrongFlowRoleId)
  } catch (error) {
    if (error instanceof StrongFlowRoleSessionError) throw error
    throw new StrongFlowRoleSessionError(
      'INVALID_ASSIGNMENT',
      'StrongFlow role-session lineage input is invalid',
      { cause: error },
    )
  }
}

function roleSpecIdFor(roleSpec: StrongFlowRoleSpec): StrongFlowRoleSpecId {
  return `${ROLE_SPEC_PREFIX}${digest('role-spec', [JSON.stringify(roleSpec)])}` as StrongFlowRoleSpecId
}

function contextIdFor(
  lineageId: StrongFlowKernelSessionLineageId,
  roleSpecId: StrongFlowRoleSpecId,
  workspace: StrongFlowRoleWorkspaceAssignment,
): StrongFlowRoleContextId {
  return `${CONTEXT_PREFIX}${digest('role-context', [
    lineageId,
    roleSpecId,
    JSON.stringify(workspace),
  ])}` as StrongFlowRoleContextId
}

function kernelStreamIdFor(
  lineageId: StrongFlowKernelSessionLineageId,
  generation: number,
  kernelSessionId: KernelSessionIdentifier,
): string {
  return `${STREAM_PREFIX}${digest('kernel-stream', [
    lineageId,
    String(generation),
    kernelSessionId,
  ])}`
}

async function canonicalWorkspace(
  input: StrongFlowRoleWorkspaceAssignment,
  roleSpec: StrongFlowRoleSpec,
  stageRunId: StageRunIdentifier,
): Promise<StrongFlowRoleWorkspaceAssignment> {
  try {
    if (!isRecord(input)) throw new Error('workspace assignment must be an object')
    const baseKeys = [
      'roleId',
      'stageRunId',
      'workspaceId',
      'mode',
      'path',
      'sourceSnapshotId',
    ]
    const required = [...baseKeys]
    const optional: string[] = []
    if (roleSpec.workspaceMode === 'candidate-write') {
      if (roleSpec.id === 'remediator') required.push('candidateId')
      else optional.push('candidateId')
    } else if (roleSpec.workspaceMode === 'candidate-read-only') {
      required.push('temporaryOutputPath', 'candidateId', 'verificationSnapshotId')
    }
    exactKeys(input, required, optional, 'workspace assignment')
    if (input.roleId !== roleSpec.id) throw new Error('workspace role does not match role spec')
    if (StageRunId(input.stageRunId) !== stageRunId) {
      throw new Error('workspace stage run does not match assignment')
    }
    if (input.mode !== roleSpec.workspaceMode) {
      throw new Error('workspace mode does not match role spec')
    }
    const workspaceId = StrongFlowWorkspaceId(input.workspaceId)
    const sourceSnapshotId = SourceSnapshotId(input.sourceSnapshotId)
    if (typeof input.path !== 'string' || !isAbsolute(input.path)) {
      throw new Error('workspace path must be absolute')
    }
    const path = await realpath(input.path)
    let temporaryOutputPath: string | undefined
    if (input.temporaryOutputPath !== undefined) {
      if (
        typeof input.temporaryOutputPath !== 'string'
        || !isAbsolute(input.temporaryOutputPath)
      ) throw new Error('temporary output path must be absolute')
      temporaryOutputPath = await realpath(input.temporaryOutputPath)
      if (temporaryOutputPath === path) {
        throw new Error('temporary output path must be separate from the role workspace')
      }
    }
    const candidateId = input.candidateId === undefined
      ? undefined
      : CandidateId(input.candidateId)
    const verificationSnapshotId = input.verificationSnapshotId === undefined
      ? undefined
      : VerificationSnapshotId(input.verificationSnapshotId)
    return Object.freeze({
      roleId: roleSpec.id,
      stageRunId,
      workspaceId,
      mode: roleSpec.workspaceMode,
      path,
      ...(temporaryOutputPath === undefined ? {} : { temporaryOutputPath }),
      sourceSnapshotId,
      ...(candidateId === undefined ? {} : { candidateId }),
      ...(verificationSnapshotId === undefined ? {} : { verificationSnapshotId }),
    })
  } catch (error) {
    if (error instanceof StrongFlowRoleSessionError) throw error
    throw new StrongFlowRoleSessionError(
      'INVALID_ASSIGNMENT',
      `workspace assignment for role ${roleSpec.id} is invalid`,
      { cause: error },
    )
  }
}

async function contextFromAssignment(
  input: StrongFlowRoleSessionAssignment,
  roleById: ReadonlyMap<StrongFlowRoleId, StrongFlowRoleSpec>,
): Promise<StrongFlowRoleSessionContext> {
  try {
    if (!isRecord(input)) throw new Error('role-session assignment must be an object')
    exactKeys(
      input,
      ['jobId', 'stageRunId', 'attemptId', 'roleId', 'workspace'],
      [],
      'role-session assignment',
    )
    const jobId = JobId(input.jobId)
    const stageRunId = StageRunId(input.stageRunId)
    const attemptId = AttemptId(input.attemptId)
    if (typeof input.roleId !== 'string') throw new Error('roleId must be text')
    const roleSpec = roleById.get(input.roleId as StrongFlowRoleId)
    if (roleSpec === undefined) throw new Error('roleId is not configured')
    const roleSpecSnapshot = immutableJson(structuredClone(roleSpec))
    const workspace = await canonicalWorkspace(input.workspace, roleSpecSnapshot, stageRunId)
    const kernelSessionLineageId = lineageIdFor(
      jobId,
      stageRunId,
      attemptId,
      roleSpecSnapshot.id,
    )
    const roleSpecId = roleSpecIdFor(roleSpecSnapshot)
    const contextId = contextIdFor(kernelSessionLineageId, roleSpecId, workspace)
    return Object.freeze({
      schemaVersion: STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION,
      kernelSessionLineageId,
      contextId,
      roleSpecId,
      jobId,
      stageRunId,
      attemptId,
      roleSpec: roleSpecSnapshot,
      workspace,
    })
  } catch (error) {
    if (error instanceof StrongFlowRoleSessionError) throw error
    throw new StrongFlowRoleSessionError(
      'INVALID_ASSIGNMENT',
      'StrongFlow role-session assignment is invalid',
      { cause: error },
    )
  }
}

async function parseStoredContext(
  value: unknown,
  currentRole: StrongFlowRoleSpec,
): Promise<StrongFlowRoleSessionContext> {
  if (!isRecord(value)) {
    throw new StrongFlowRoleSessionError(
      'SESSION_STORE_CORRUPT',
      'stored role-session context is not an object',
    )
  }
  try {
    exactKeys(value, [
      'schemaVersion',
      'kernelSessionLineageId',
      'contextId',
      'roleSpecId',
      'jobId',
      'stageRunId',
      'attemptId',
      'roleSpec',
      'workspace',
    ], [], 'stored role-session context')
    if (value.schemaVersion !== STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION) {
      throw new Error('stored role-session schema version is unsupported')
    }
    if (JSON.stringify(value.roleSpec) !== JSON.stringify(currentRole)) {
      throw new StrongFlowRoleSessionError(
        'ROLE_SNAPSHOT_MISMATCH',
        `stored role snapshot for ${currentRole.id} differs from current configuration`,
      )
    }
    const jobId = JobId(String(value.jobId))
    const stageRunId = StageRunId(String(value.stageRunId))
    const attemptId = AttemptId(String(value.attemptId))
    const workspace = await canonicalWorkspace(
      value.workspace as StrongFlowRoleWorkspaceAssignment,
      currentRole,
      stageRunId,
    )
    const kernelSessionLineageId = lineageIdFor(
      jobId,
      stageRunId,
      attemptId,
      currentRole.id,
    )
    const roleSpecId = roleSpecIdFor(currentRole)
    const contextId = contextIdFor(kernelSessionLineageId, roleSpecId, workspace)
    if (
      value.kernelSessionLineageId !== kernelSessionLineageId
      || value.roleSpecId !== roleSpecId
      || value.contextId !== contextId
    ) throw new Error('stored role-session hashes do not match their content')
    return Object.freeze({
      schemaVersion: STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION,
      kernelSessionLineageId,
      contextId,
      roleSpecId,
      jobId,
      stageRunId,
      attemptId,
      roleSpec: currentRole,
      workspace,
    })
  } catch (error) {
    if (error instanceof StrongFlowRoleSessionError) throw error
    throw new StrongFlowRoleSessionError(
      'SESSION_STORE_CORRUPT',
      'stored role-session context is corrupt',
      { cause: error },
    )
  }
}

function parseAcceptedRecord(value: Record<string, unknown>): AcceptedLifecycleRecord {
  exactKeys(value, [
    'schemaVersion',
    'recordType',
    'generation',
    'source',
    'kernelSessionId',
    'kernelStreamId',
    'rolloutPath',
    'acceptedAtMillis',
    'effectivePolicy',
  ], [], 'accepted lifecycle record')
  if (value.schemaVersion !== STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION) {
    throw new Error('accepted lifecycle schema version is unsupported')
  }
  if (!Number.isSafeInteger(value.generation) || Number(value.generation) < 1) {
    throw new Error('accepted lifecycle generation is invalid')
  }
  if (value.source !== 'create' && value.source !== 'resume') {
    throw new Error('accepted lifecycle source is invalid')
  }
  const kernelSessionId = KernelSessionId(String(value.kernelSessionId))
  if (
    typeof value.kernelStreamId !== 'string'
    || !value.kernelStreamId.startsWith(STREAM_PREFIX)
  ) throw new Error('accepted lifecycle stream id is invalid')
  if (typeof value.rolloutPath !== 'string' || !isAbsolute(value.rolloutPath)) {
    throw new Error('accepted lifecycle rollout path is invalid')
  }
  if (!Number.isSafeInteger(value.acceptedAtMillis) || Number(value.acceptedAtMillis) < 0) {
    throw new Error('accepted lifecycle time is invalid')
  }
  if (!isRecord(value.effectivePolicy)) {
    throw new Error('accepted lifecycle effective policy is invalid')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION,
    recordType: 'kernel.accepted',
    generation: Number(value.generation),
    source: value.source,
    kernelSessionId,
    kernelStreamId: value.kernelStreamId,
    rolloutPath: resolve(value.rolloutPath),
    acceptedAtMillis: Number(value.acceptedAtMillis),
    effectivePolicy: immutableJson(
      structuredClone(value.effectivePolicy),
    ) as unknown as GovernedSessionEffectivePolicy,
  })
}

function parseTerminalRecord(value: Record<string, unknown>): TerminalLifecycleRecord {
  exactKeys(value, [
    'schemaVersion',
    'recordType',
    'generation',
    'outcome',
    'reason',
    'occurredAtMillis',
  ], [], 'terminal lifecycle record')
  if (value.schemaVersion !== STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION) {
    throw new Error('terminal lifecycle schema version is unsupported')
  }
  if (!Number.isSafeInteger(value.generation) || Number(value.generation) < 1) {
    throw new Error('terminal lifecycle generation is invalid')
  }
  if (!['completed', 'cancelled', 'failed'].includes(String(value.outcome))) {
    throw new Error('terminal lifecycle outcome is invalid')
  }
  if (
    typeof value.reason !== 'string'
    || value.reason.length === 0
    || value.reason.length > PORTABLE_REASON_MAX_LENGTH
    || value.reason.trim() !== value.reason
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value.reason)
  ) throw new Error('terminal lifecycle reason is invalid')
  if (!Number.isSafeInteger(value.occurredAtMillis) || Number(value.occurredAtMillis) < 0) {
    throw new Error('terminal lifecycle time is invalid')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION,
    recordType: 'session.terminal',
    generation: Number(value.generation),
    outcome: value.outcome as TerminalLifecycleRecord['outcome'],
    reason: value.reason,
    occurredAtMillis: Number(value.occurredAtMillis),
  })
}

function parseLifecycleRecords(text: string): readonly LifecycleRecord[] {
  try {
    const records: LifecycleRecord[] = []
    let latestAccepted: AcceptedLifecycleRecord | undefined
    let terminal = false
    let previousTime = -1
    for (const [index, line] of text.split('\n').entries()) {
      if (line.length === 0) continue
      const value = JSON.parse(line) as unknown
      if (!isRecord(value)) throw new Error(`lifecycle line ${index + 1} is not an object`)
      if (terminal) throw new Error('terminal lifecycle record is not final')
      if (value.recordType === 'kernel.accepted') {
        const record = parseAcceptedRecord(value)
        const expectedGeneration = (latestAccepted?.generation ?? 0) + 1
        if (
          record.generation !== expectedGeneration
          || (record.generation === 1 && record.source !== 'create')
          || (record.generation > 1 && record.source !== 'resume')
        ) throw new Error('accepted lifecycle generation or source is inconsistent')
        if (record.acceptedAtMillis < previousTime) {
          throw new Error('accepted lifecycle time moved backwards')
        }
        latestAccepted = record
        previousTime = record.acceptedAtMillis
        records.push(record)
      } else if (value.recordType === 'session.terminal') {
        const record = parseTerminalRecord(value)
        if (latestAccepted === undefined || record.generation !== latestAccepted.generation) {
          throw new Error('terminal lifecycle generation is inconsistent')
        }
        if (record.occurredAtMillis < previousTime) {
          throw new Error('terminal lifecycle time moved backwards')
        }
        terminal = true
        previousTime = record.occurredAtMillis
        records.push(record)
      } else {
        throw new Error('lifecycle record type is unknown')
      }
    }
    if (latestAccepted === undefined) throw new Error('role session has no accepted lifecycle')
    return Object.freeze(records)
  } catch (error) {
    if (error instanceof StrongFlowRoleSessionError) throw error
    throw new StrongFlowRoleSessionError(
      'SESSION_STORE_CORRUPT',
      'stored role-session lifecycle is corrupt',
      { cause: error },
    )
  }
}

function roleSessionsRoot(home: string): string {
  return join(home, 'strongflow-role-sessions')
}

function sessionKey(lineageId: StrongFlowKernelSessionLineageId): string {
  return lineageId.slice(LINEAGE_PREFIX.length)
}

function sessionDirectory(home: string, lineageId: StrongFlowKernelSessionLineageId): string {
  return join(roleSessionsRoot(home), sessionKey(lineageId))
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await stat(path)
    return true
  } catch (error) {
    if (errorCode(error) === 'ENOENT') return false
    throw error
  }
}

async function syncDirectory(path: string): Promise<void> {
  const handle = await open(path, 'r')
  try {
    await handle.sync()
  } finally {
    await handle.close()
  }
}

async function writeNewFile(path: string, value: unknown, pretty: boolean): Promise<void> {
  const text = pretty ? JSON.stringify(value, null, 2) : JSON.stringify(value)
  const handle = await open(path, 'wx', 0o600)
  try {
    await handle.writeFile(`${text}\n`, 'utf8')
    await handle.sync()
  } finally {
    await handle.close()
  }
}

async function appendRecord(path: string, record: LifecycleRecord): Promise<void> {
  const handle = await open(path, 'a', 0o600)
  try {
    await handle.writeFile(`${JSON.stringify(record)}\n`, 'utf8')
    await handle.sync()
  } finally {
    await handle.close()
  }
}

async function loadStoredSession(
  home: string,
  lineageId: StrongFlowKernelSessionLineageId,
  currentRole: StrongFlowRoleSpec,
): Promise<StoredRoleSession> {
  const directory = sessionDirectory(home, lineageId)
  const contextPath = join(directory, 'context.json')
  const lifecyclePath = join(directory, 'lifecycle.jsonl')
  const ownerPath = join(directory, 'owner.json')
  if (!(await pathExists(directory))) {
    throw new StrongFlowRoleSessionError(
      'SESSION_NOT_FOUND',
      `StrongFlow role session ${lineageId} was not found`,
    )
  }
  if (!(await pathExists(contextPath)) || !(await pathExists(lifecyclePath))) {
    throw new StrongFlowRoleSessionError(
      'SESSION_STORE_CORRUPT',
      `StrongFlow role session ${lineageId} is missing required records`,
    )
  }
  try {
    const context = await parseStoredContext(
      JSON.parse(await readFile(contextPath, 'utf8')) as unknown,
      currentRole,
    )
    if (context.kernelSessionLineageId !== lineageId) {
      throw new Error('stored role-session lineage does not match its directory')
    }
    const records = parseLifecycleRecords(await readFile(lifecyclePath, 'utf8'))
    for (const record of records) {
      if (
        record.recordType === 'kernel.accepted'
        && record.kernelStreamId !== kernelStreamIdFor(
          context.kernelSessionLineageId,
          record.generation,
          record.kernelSessionId,
        )
      ) throw new Error('stored kernel stream identity is inconsistent')
      if (record.recordType === 'kernel.accepted') {
        verifyStrongFlowRoleKernelEvidence(context, record.effectivePolicy)
      }
    }
    const latestAccepted = records.findLast(
      (record): record is AcceptedLifecycleRecord => record.recordType === 'kernel.accepted',
    )
    if (latestAccepted === undefined) throw new Error('role session has no accepted lifecycle')
    const terminal = records.at(-1)?.recordType === 'session.terminal'
      ? records.at(-1) as TerminalLifecycleRecord
      : undefined
    return Object.freeze({
      directory,
      contextPath,
      lifecyclePath,
      ownerPath,
      context,
      records,
      latestAccepted,
      ...(terminal === undefined ? {} : { terminal }),
    })
  } catch (error) {
    if (error instanceof StrongFlowRoleSessionError) throw error
    throw new StrongFlowRoleSessionError(
      'SESSION_STORE_CORRUPT',
      `StrongFlow role session ${lineageId} is corrupt`,
      { cause: error },
    )
  }
}

function ownerRecord(
  context: StrongFlowRoleSessionContext,
  now: () => number,
): OwnerRecord {
  return Object.freeze({
    schemaVersion: STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION,
    kernelSessionLineageId: context.kernelSessionLineageId,
    token: randomUUID(),
    pid: process.pid,
    acquiredAtMillis: safeNow(now),
  })
}

function parseOwner(value: unknown, lineageId: StrongFlowKernelSessionLineageId): OwnerRecord {
  if (!isRecord(value)) throw new Error('owner record must be an object')
  exactKeys(value, [
    'schemaVersion',
    'kernelSessionLineageId',
    'token',
    'pid',
    'acquiredAtMillis',
  ], [], 'owner record')
  if (
    value.schemaVersion !== STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION
    || value.kernelSessionLineageId !== lineageId
    || typeof value.token !== 'string'
    || value.token.length === 0
    || !Number.isSafeInteger(value.pid)
    || Number(value.pid) < 1
    || !Number.isSafeInteger(value.acquiredAtMillis)
    || Number(value.acquiredAtMillis) < 0
  ) throw new Error('owner record is invalid')
  return Object.freeze({
    schemaVersion: STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION,
    kernelSessionLineageId: lineageId,
    token: value.token,
    pid: Number(value.pid),
    acquiredAtMillis: Number(value.acquiredAtMillis),
  })
}

function processIsAlive(pid: number): boolean {
  try {
    process.kill(pid, 0)
    return true
  } catch (error) {
    if (errorCode(error) === 'ESRCH') return false
    return true
  }
}

async function claimStoredOwner(
  stored: StoredRoleSession,
  now: () => number,
): Promise<OwnerRecord> {
  if (await pathExists(stored.ownerPath)) {
    let existing: OwnerRecord
    try {
      existing = parseOwner(
        JSON.parse(await readFile(stored.ownerPath, 'utf8')) as unknown,
        stored.context.kernelSessionLineageId,
      )
    } catch (error) {
      throw new StrongFlowRoleSessionError(
        'SESSION_STORE_CORRUPT',
        `role-session owner for ${stored.context.kernelSessionLineageId} is corrupt`,
        { cause: error },
      )
    }
    if (processIsAlive(existing.pid)) {
      throw new StrongFlowRoleSessionError(
        'SESSION_ACTIVE',
        `StrongFlow role session ${stored.context.kernelSessionLineageId} is owned by process ${existing.pid}`,
      )
    }
    await unlink(stored.ownerPath)
    await syncDirectory(stored.directory)
  }
  const owner = ownerRecord(stored.context, now)
  await writeNewFile(stored.ownerPath, owner, true)
  await syncDirectory(stored.directory)
  return owner
}

async function releaseOwner(ownerPath: string, expected: OwnerRecord): Promise<void> {
  const current = parseOwner(
    JSON.parse(await readFile(ownerPath, 'utf8')) as unknown,
    expected.kernelSessionLineageId,
  )
  if (current.token !== expected.token || current.pid !== expected.pid) {
    throw new Error('role-session owner changed before release')
  }
  await unlink(ownerPath)
  await syncDirectory(dirname(ownerPath))
}

async function acquireOperationLock(
  home: string,
  lineageId: StrongFlowKernelSessionLineageId,
): Promise<() => Promise<void>> {
  const root = roleSessionsRoot(home)
  await mkdir(root, { recursive: true, mode: 0o700 })
  const path = join(root, `.operation-${sessionKey(lineageId)}.lock`)
  let handle
  try {
    handle = await open(path, 'wx', 0o600)
  } catch (error) {
    if (errorCode(error) === 'EEXIST') {
      throw new StrongFlowRoleSessionError(
        'SESSION_BUSY',
        `StrongFlow role session ${lineageId} has another setup operation`,
      )
    }
    throw error
  }
  try {
    await handle.writeFile(`${JSON.stringify({ pid: process.pid, token: randomUUID() })}\n`)
    await handle.sync()
  } finally {
    await handle.close()
  }
  let released = false
  return async () => {
    if (released) return
    released = true
    await rm(path, { force: true })
    await syncDirectory(root)
  }
}

function acceptedLifecycle(
  context: StrongFlowRoleSessionContext,
  info: SessionInfo,
  generation: number,
  source: 'create' | 'resume',
  now: () => number,
  notBeforeMillis = 0,
): AcceptedLifecycleRecord {
  try {
    if (!isRecord(info)) throw new Error('native session info must be an object')
    if (!Object.hasOwn(info, 'effectivePolicy')) {
      throw new StrongFlowRoleSessionError(
        'ENFORCEMENT_UNAVAILABLE',
        `native kernel returned no effective policy for role ${context.roleSpec.id}`,
      )
    }
    exactKeys(info, ['sessionId', 'effectivePolicy'], ['rolloutPath'], 'native session info')
    const kernelSessionId = KernelSessionId(info.sessionId)
    if (typeof info.rolloutPath !== 'string' || !isAbsolute(info.rolloutPath)) {
      throw new Error('native session has no absolute rollout path')
    }
    const effectivePolicy = verifyStrongFlowRoleKernelEvidence(context, info.effectivePolicy)
    return Object.freeze({
      schemaVersion: STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION,
      recordType: 'kernel.accepted',
      generation,
      source,
      kernelSessionId,
      kernelStreamId: kernelStreamIdFor(
        context.kernelSessionLineageId,
        generation,
        kernelSessionId,
      ),
      rolloutPath: resolve(info.rolloutPath),
      acceptedAtMillis: Math.max(safeNow(now), notBeforeMillis),
      effectivePolicy,
    })
  } catch (error) {
    if (error instanceof StrongFlowRoleSessionError) throw error
    if (
      error instanceof StrongFlowRoleAuthorityError
      && error.code === 'ENFORCEMENT_UNAVAILABLE'
    ) {
      throw new StrongFlowRoleSessionError(
        'ENFORCEMENT_UNAVAILABLE',
        `native kernel enforcement is incomplete for role ${context.roleSpec.id}`,
        { cause: error },
      )
    }
    throw new StrongFlowRoleSessionError(
      'SESSION_SETUP_FAILED',
      'native kernel returned invalid role-session identity',
      { cause: error },
    )
  }
}

function publicKernelLifecycle(record: AcceptedLifecycleRecord): StrongFlowRoleKernelLifecycle {
  return Object.freeze({
    generation: record.generation,
    source: record.source,
    kernelSessionId: record.kernelSessionId,
    kernelStreamId: record.kernelStreamId,
    rolloutPath: record.rolloutPath,
    acceptedAtMillis: record.acceptedAtMillis,
    effectivePolicy: record.effectivePolicy,
  })
}

function validateInstallation(
  value: StrongFlowRoleContextInstallation,
  context: StrongFlowRoleSessionContext,
): StrongFlowRoleContextInstallation {
  if (!isObject(value)) {
    throw new StrongFlowRoleSessionError(
      'CONTEXT_INSTALLATION_MISMATCH',
      'role context installer returned no installation record',
    )
  }
  try {
    if (
      value.contextId !== context.contextId
      || typeof value.handleEvent !== 'function'
      || typeof value.dispose !== 'function'
    ) {
      throw new Error('installed context identity or disposer is invalid')
    }
    return value as unknown as StrongFlowRoleContextInstallation
  } catch (error) {
    throw new StrongFlowRoleSessionError(
      'CONTEXT_INSTALLATION_MISMATCH',
      `role context installer did not confirm ${context.contextId}`,
      { cause: error },
    )
  }
}

class BoundedEventQueue<Value> {
  readonly #capacity: number
  readonly #values: Value[] = []
  readonly #readers: Array<Deferred<IteratorResult<Value>>> = []
  readonly #writers: Array<Deferred<void>> = []
  #closed = false
  #failure: unknown
  #claimed = false

  constructor(capacity: number) {
    this.#capacity = capacity
  }

  async push(value: Value): Promise<void> {
    while (!this.#closed && this.#readers.length === 0 && this.#values.length >= this.#capacity) {
      const writer = deferred<void>()
      this.#writers.push(writer)
      await writer.promise
    }
    if (this.#closed) return
    const reader = this.#readers.shift()
    if (reader !== undefined) reader.resolve({ done: false, value })
    else this.#values.push(value)
  }

  close(failure?: unknown): void {
    if (this.#closed) return
    this.#closed = true
    this.#failure = failure
    for (const writer of this.#writers.splice(0)) writer.resolve()
    if (this.#values.length === 0) this.#settleReaders()
  }

  iterable(onClaim: () => void): AsyncIterable<Value> {
    if (this.#claimed) {
      throw new StrongFlowRoleSessionError(
        'EVENT_SUBSCRIBER_EXISTS',
        'StrongFlow role session already has an event subscriber',
      )
    }
    this.#claimed = true
    onClaim()
    return {
      [Symbol.asyncIterator]: () => ({
        next: () => this.#next(),
      }),
    }
  }

  #next(): Promise<IteratorResult<Value>> {
    const value = this.#values.shift()
    if (value !== undefined) {
      this.#writers.shift()?.resolve()
      if (this.#closed && this.#values.length === 0) this.#settleReaders()
      return Promise.resolve({ done: false, value })
    }
    if (this.#closed) {
      return this.#failure === undefined
        ? Promise.resolve({ done: true, value: undefined })
        : Promise.reject(this.#failure)
    }
    const reader = deferred<IteratorResult<Value>>()
    this.#readers.push(reader)
    return reader.promise
  }

  #settleReaders(): void {
    for (const reader of this.#readers.splice(0)) {
      if (this.#failure === undefined) reader.resolve({ done: true, value: undefined })
      else reader.reject(this.#failure)
    }
  }
}

interface ManagedSessionOptions {
  readonly context: StrongFlowRoleSessionContext
  readonly kernelLifecycle: StrongFlowRoleKernelLifecycle
  readonly kernel: StrongFlowRoleKernelPort
  readonly eventBufferCapacity: number
  readonly now: () => number
  readonly onTerminal: (lineageId: StrongFlowKernelSessionLineageId) => void
}

class ManagedStrongFlowRoleSession implements StrongFlowRoleSession {
  readonly context: StrongFlowRoleSessionContext
  readonly kernel: StrongFlowRoleKernelLifecycle

  readonly #kernelPort: StrongFlowRoleKernelPort
  readonly #eventQueue: BoundedEventQueue<StrongFlowRoleKernelEvent>
  readonly #eventAbort = new AbortController()
  readonly #subscriptionStarted = deferred<void>()
  readonly #eventPump: Promise<void>
  readonly #now: () => number
  readonly #onTerminal: ManagedSessionOptions['onTerminal']

  #installation: StrongFlowRoleContextInstallation | undefined
  #lifecyclePath: string | undefined
  #ownerPath: string | undefined
  #owner: OwnerRecord | undefined
  #state: StrongFlowRoleSessionState = 'ready'
  #eventFailure: StrongFlowRoleSessionError | undefined
  #turnSubmitted = false
  #settlement: Promise<void> | undefined

  constructor(options: ManagedSessionOptions) {
    this.context = options.context
    this.kernel = options.kernelLifecycle
    this.#kernelPort = options.kernel
    this.#eventQueue = new BoundedEventQueue(options.eventBufferCapacity)
    this.#now = options.now
    this.#onTerminal = options.onTerminal
    this.#eventPump = this.#pumpEvents()
  }

  get state(): StrongFlowRoleSessionState {
    return this.#state
  }

  get eventFailure(): StrongFlowRoleSessionError | undefined {
    return this.#eventFailure
  }

  get summary(): StrongFlowRoleSessionSummary {
    return Object.freeze({
      kernelSessionLineageId: this.context.kernelSessionLineageId,
      contextId: this.context.contextId,
      jobId: this.context.jobId,
      stageRunId: this.context.stageRunId,
      attemptId: this.context.attemptId,
      roleId: this.context.roleSpec.id,
      kernelSessionId: this.kernel.kernelSessionId,
      kernelStreamId: this.kernel.kernelStreamId,
      generation: this.kernel.generation,
      state: this.#state,
    })
  }

  get setupSignal(): AbortSignal {
    return this.#eventAbort.signal
  }

  events(): AsyncIterable<StrongFlowRoleKernelEvent> {
    return this.#eventQueue.iterable(() => {})
  }

  async submitTurn(text: string): Promise<SubmissionInfo> {
    if (this.#state !== 'ready' || this.#settlement !== undefined) {
      throw new StrongFlowRoleSessionError(
        'SESSION_NOT_READY',
        `StrongFlow role session ${this.context.kernelSessionLineageId} is not ready for a turn`,
      )
    }
    if (this.#turnSubmitted) {
      throw new StrongFlowRoleSessionError(
        'TURN_ALREADY_SUBMITTED',
        `StrongFlow role session ${this.context.kernelSessionLineageId} already submitted its governed turn`,
      )
    }
    if (
      typeof text !== 'string'
      || text.length === 0
      || text.trim().length === 0
      || Buffer.byteLength(text) > MAX_TURN_INPUT_BYTES
      || /\u0000/u.test(text)
    ) {
      throw new StrongFlowRoleSessionError(
        'TURN_INPUT_INVALID',
        'StrongFlow role turn input must be non-empty UTF-8 text within the size limit',
      )
    }
    this.#turnSubmitted = true
    try {
      const submission = await this.#kernelPort.submitTurn(this.kernel.kernelSessionId, text)
      if (!isRecord(submission)) throw new Error('native submission must be an object')
      exactKeys(submission, ['status'], ['turnId', 'reason'], 'native turn submission')
      if (!['started', 'steered', 'not_submitted'].includes(String(submission.status))) {
        throw new Error('native submission status is unknown')
      }
      if (
        submission.turnId !== undefined
        && (typeof submission.turnId !== 'string' || submission.turnId.length === 0)
      ) throw new Error('native submission turn identity is invalid')
      if (
        submission.reason !== undefined
        && (typeof submission.reason !== 'string' || submission.reason.length === 0)
      ) throw new Error('native submission reason is invalid')
      return Object.freeze({
        status: submission.status as SubmissionInfo['status'],
        ...(submission.turnId === undefined ? {} : { turnId: submission.turnId }),
        ...(submission.reason === undefined ? {} : { reason: submission.reason }),
      })
    } catch (error) {
      throw new StrongFlowRoleSessionError(
        'TURN_SUBMISSION_FAILED',
        `StrongFlow role session ${this.context.kernelSessionLineageId} could not submit its governed turn`,
        { cause: error },
      )
    }
  }

  async awaitSubscription(): Promise<void> {
    await this.#subscriptionStarted.promise
  }

  install(installation: StrongFlowRoleContextInstallation): void {
    this.#installation = installation
  }

  acceptDurable(lifecyclePath: string, ownerPath: string, owner: OwnerRecord): void {
    this.#lifecyclePath = lifecyclePath
    this.#ownerPath = ownerPath
    this.#owner = owner
  }

  cancel(reason: string): Promise<void> {
    const validated = portableReason(reason, 'role-session cancellation reason')
    return (this.#settlement ??= this.#settle('cancelled', validated, true))
  }

  fail(
    reason: string,
    options: StrongFlowRoleSessionFailureOptions = {},
  ): Promise<void> {
    const validated = portableReason(reason, 'role-session failure reason')
    if (!isRecord(options)) {
      throw new StrongFlowRoleSessionError(
        'INVALID_ASSIGNMENT',
        'role-session failure options must be an object',
      )
    }
    exactKeys(options, [], ['interrupt'], 'role-session failure options')
    if (options.interrupt !== undefined && typeof options.interrupt !== 'boolean') {
      throw new StrongFlowRoleSessionError(
        'INVALID_ASSIGNMENT',
        'role-session failure interrupt option must be a boolean',
      )
    }
    return (this.#settlement ??= this.#settle(
      'failed',
      validated,
      options.interrupt ?? false,
    ))
  }

  dispose(): Promise<void> {
    return (this.#settlement ??= this.#settle(
      'completed',
      'StrongFlow role session completed and was disposed',
      false,
    ))
  }

  async rollback(reason: string): Promise<void> {
    const failures: unknown[] = []
    this.#eventAbort.abort(reason)
    this.#eventQueue.close()
    try {
      await this.#kernelPort.closeSession(this.kernel.kernelSessionId)
    } catch (error) {
      failures.push(error)
    }
    await this.#eventPump
    if (this.#installation !== undefined) {
      try {
        await this.#installation.dispose({ outcome: 'rollback', reason })
      } catch (error) {
        failures.push(error)
      }
    }
    this.#eventQueue.close()
    if (failures.length > 0) {
      throw new StrongFlowRoleSessionError(
        'SESSION_SETUP_ROLLBACK_FAILED',
        'role-session setup failed and rollback did not release every resource',
        { cause: new AggregateError(failures) },
      )
    }
  }

  async #pumpEvents(): Promise<void> {
    let previousSequence: bigint | undefined
    try {
      const iterable = this.#kernelPort.events(this.kernel.kernelSessionId, {
        signal: this.#eventAbort.signal,
      })
      const iterator = iterable[Symbol.asyncIterator]()
      const first = iterator.next()
      this.#subscriptionStarted.resolve()
      let result = await first
      while (!result.done) {
        const event = result.value
        if (previousSequence !== undefined && event.sequence <= previousSequence) {
          throw new StrongFlowRoleSessionError(
            'EVENT_SEQUENCE_INVALID',
            `kernel session ${this.kernel.kernelSessionId} returned a non-increasing event sequence`,
          )
        }
        previousSequence = event.sequence
        const governedEvent = Object.freeze({
          kernelSessionLineageId: this.context.kernelSessionLineageId,
          contextId: this.context.contextId,
          generation: this.kernel.generation,
          kernelSessionId: this.kernel.kernelSessionId,
          kernelStreamId: this.kernel.kernelStreamId,
          event,
        })
        if (this.#installation !== undefined) {
          await this.#installation.handleEvent(governedEvent)
        }
        await this.#eventQueue.push(governedEvent)
        result = await iterator.next()
      }
      if (!this.#eventAbort.signal.aborted) {
        throw new StrongFlowRoleSessionError(
          'EVENT_STREAM_FAILED',
          `kernel event stream closed unexpectedly for role ${this.context.roleSpec.id}`,
        )
      }
      this.#eventQueue.close()
    } catch (error) {
      if (!this.#eventAbort.signal.aborted) {
        const failure = error instanceof StrongFlowRoleSessionError
          ? error
          : new StrongFlowRoleSessionError(
            'EVENT_STREAM_FAILED',
            `kernel event stream failed for role ${this.context.roleSpec.id}`,
            { cause: error },
          )
        this.#eventFailure = failure
        this.#eventQueue.close(failure)
      } else {
        this.#eventQueue.close()
      }
      this.#subscriptionStarted.resolve()
    }
  }

  async #settle(
    requestedOutcome: 'completed' | 'cancelled' | 'failed',
    reason: string,
    interrupt: boolean,
  ): Promise<void> {
    if (!['ready', 'cancelling', 'disposing'].includes(this.#state)) return
    this.#state = interrupt ? 'cancelling' : 'disposing'
    const failures: unknown[] = []
    this.#eventAbort.abort(reason)
    this.#eventQueue.close()
    if (interrupt) {
      try {
        await this.#kernelPort.interrupt(this.kernel.kernelSessionId)
      } catch (error) {
        failures.push(error)
      }
    }
    try {
      await this.#kernelPort.closeSession(this.kernel.kernelSessionId)
    } catch (error) {
      failures.push(error)
    }
    await this.#eventPump
    if (this.#eventFailure !== undefined) failures.push(this.#eventFailure)
    if (this.#installation !== undefined) {
      try {
        await this.#installation.dispose({
          outcome: failures.length === 0 ? requestedOutcome : 'failed',
          reason,
        })
      } catch (error) {
        failures.push(error)
      }
    }
    const outcome = failures.length === 0 ? requestedOutcome : 'failed'
    const lifecyclePath = this.#lifecyclePath
    const ownerPath = this.#ownerPath
    const owner = this.#owner
    if (lifecyclePath === undefined || ownerPath === undefined || owner === undefined) {
      failures.push(new Error('role session has no durable ownership record'))
    } else {
      try {
        await appendRecord(lifecyclePath, Object.freeze({
          schemaVersion: STRONGFLOW_ROLE_SESSION_SCHEMA_VERSION,
          recordType: 'session.terminal',
          generation: this.kernel.generation,
          outcome,
          reason: failures.length === 0 ? reason : 'Role-session teardown failed',
          occurredAtMillis: Math.max(safeNow(this.#now), this.kernel.acceptedAtMillis),
        }))
        await syncDirectory(dirname(lifecyclePath))
      } catch (error) {
        failures.push(error)
      }
      if (failures.length === 0 || outcome === 'failed') {
        try {
          await releaseOwner(ownerPath, owner)
        } catch (error) {
          failures.push(error)
        }
      }
    }
    this.#state = failures.length > 0 || requestedOutcome === 'failed'
      ? 'failed'
      : requestedOutcome === 'cancelled' ? 'cancelled' : 'closed'
    this.#onTerminal(this.context.kernelSessionLineageId)
    this.#eventQueue.close()
    if (failures.length > 0) {
      throw new StrongFlowRoleSessionError(
        'TEARDOWN_FAILED',
        `StrongFlow role session ${this.context.kernelSessionLineageId} did not release cleanly`,
        { cause: new AggregateError(failures) },
      )
    }
  }
}

/** Creates, resumes, publishes, cancels, and disposes governed StrongFlow role sessions. */
export class StrongFlowRoleSessionManager {
  readonly home: string

  readonly #kernel: StrongFlowRoleKernelPort
  readonly #installer: StrongFlowRoleContextInstaller
  readonly #roleById: ReadonlyMap<StrongFlowRoleId, StrongFlowRoleSpec>
  readonly #eventBufferCapacity: number
  readonly #now: () => number
  readonly #live = new Map<StrongFlowKernelSessionLineageId, ManagedStrongFlowRoleSession>()

  constructor(options: StrongFlowRoleSessionManagerOptions) {
    if (!isRecord(options)) {
      throw new StrongFlowRoleSessionError(
        'INVALID_MANAGER_OPTIONS',
        'StrongFlow role-session manager options must be an object',
      )
    }
    this.home = validateHome(options.home)
    if (
      !isObject(options.kernel)
      || typeof options.kernel.createSession !== 'function'
      || typeof options.kernel.resumeSession !== 'function'
      || typeof options.kernel.submitTurn !== 'function'
      || typeof options.kernel.interrupt !== 'function'
      || typeof options.kernel.resolveApproval !== 'function'
      || typeof options.kernel.resolveDynamicTool !== 'function'
      || typeof options.kernel.closeSession !== 'function'
      || typeof options.kernel.events !== 'function'
    ) {
      throw new StrongFlowRoleSessionError(
        'INVALID_MANAGER_OPTIONS',
        'StrongFlow role-session manager requires the embedded kernel lifecycle port',
      )
    }
    if (!isObject(options.installer) || typeof options.installer.install !== 'function') {
      throw new StrongFlowRoleSessionError(
        'INVALID_MANAGER_OPTIONS',
        'StrongFlow role-session manager requires a role context installer',
      )
    }
    const configuration = validatedRoleConfiguration(
      options.roleConfiguration,
      options.modelCatalog,
    )
    this.#kernel = options.kernel
    this.#installer = options.installer
    this.#roleById = new Map(configuration.roles.map(role => [role.id, role]))
    this.#eventBufferCapacity = validateEventCapacity(options.eventBufferCapacity)
    this.#now = options.now ?? Date.now
    safeNow(this.#now)
  }

  listSessions(): readonly StrongFlowRoleSessionSummary[] {
    return Object.freeze([...this.#live.values()].map(session => session.summary))
  }

  session(
    lineageId: StrongFlowKernelSessionLineageId,
  ): StrongFlowRoleSession | undefined {
    return this.#live.get(lineageId)
  }

  async create(
    assignment: StrongFlowRoleSessionAssignment,
  ): Promise<StrongFlowRoleSession> {
    const context = await contextFromAssignment(assignment, this.#roleById)
    if (this.#live.has(context.kernelSessionLineageId)) {
      throw new StrongFlowRoleSessionError(
        'SESSION_ACTIVE',
        `StrongFlow role session ${context.kernelSessionLineageId} is already active`,
      )
    }
    const releaseLock = await acquireOperationLock(this.home, context.kernelSessionLineageId)
    try {
      const directory = sessionDirectory(this.home, context.kernelSessionLineageId)
      if (await pathExists(directory)) {
        throw new StrongFlowRoleSessionError(
          'SESSION_ALREADY_EXISTS',
          `StrongFlow role session ${context.kernelSessionLineageId} already exists`,
        )
      }
      const info = await this.#kernel.createSession({
        cwd: context.workspace.path,
        provider: context.roleSpec.modelRoute.provider,
        model: context.roleSpec.modelRoute.model,
        governedAuthority: createStrongFlowRoleKernelAuthority(context),
      })
      const accepted = await this.#acceptNativeSession(context, info, 1, 'create')
      const prepared = await this.#prepare(context, accepted)
      try {
        const temporary = join(
          roleSessionsRoot(this.home),
          `.creating-${sessionKey(context.kernelSessionLineageId)}-${randomUUID()}`,
        )
        let renamed = false
        try {
          await mkdir(temporary, { mode: 0o700 })
          await writeNewFile(join(temporary, 'context.json'), context, true)
          await writeNewFile(join(temporary, 'lifecycle.jsonl'), accepted, false)
          await writeNewFile(join(temporary, 'owner.json'), prepared.owner, true)
          await syncDirectory(temporary)
          await rename(temporary, directory)
          renamed = true
          await syncDirectory(roleSessionsRoot(this.home))
        } catch (error) {
          await rm(renamed ? directory : temporary, { recursive: true, force: true })
          await syncDirectory(roleSessionsRoot(this.home))
          throw error
        }
        prepared.runtime.acceptDurable(
          join(directory, 'lifecycle.jsonl'),
          join(directory, 'owner.json'),
          prepared.owner,
        )
        this.#live.set(context.kernelSessionLineageId, prepared.runtime)
        return prepared.runtime
      } catch (error) {
        return this.#rollbackPrepared(prepared, 'Role-session publication failed', error)
      }
    } catch (error) {
      if (error instanceof StrongFlowRoleSessionError) throw error
      throw new StrongFlowRoleSessionError(
        'SESSION_SETUP_FAILED',
        `StrongFlow role session ${context.kernelSessionLineageId} could not be created`,
        { cause: error },
      )
    } finally {
      await releaseLock()
    }
  }

  async resume(
    assignment: StrongFlowRoleSessionAssignment,
  ): Promise<StrongFlowRoleSession> {
    const expected = await contextFromAssignment(assignment, this.#roleById)
    if (this.#live.has(expected.kernelSessionLineageId)) {
      throw new StrongFlowRoleSessionError(
        'SESSION_ACTIVE',
        `StrongFlow role session ${expected.kernelSessionLineageId} is already active`,
      )
    }
    const releaseLock = await acquireOperationLock(this.home, expected.kernelSessionLineageId)
    let stored: StoredRoleSession | undefined
    let owner: OwnerRecord | undefined
    try {
      const currentRole = this.#roleById.get(expected.roleSpec.id)
      if (currentRole === undefined) {
        throw new StrongFlowRoleSessionError(
          'ROLE_SNAPSHOT_MISMATCH',
          `role ${expected.roleSpec.id} is not configured`,
        )
      }
      stored = await loadStoredSession(
        this.home,
        expected.kernelSessionLineageId,
        currentRole,
      )
      if (stored.terminal !== undefined) {
        throw new StrongFlowRoleSessionError(
          'SESSION_TERMINAL',
          `StrongFlow role session ${expected.kernelSessionLineageId} ended as ${stored.terminal.outcome}`,
        )
      }
      if (JSON.stringify(stored.context) !== JSON.stringify(expected)) {
        throw new StrongFlowRoleSessionError(
          'CONTEXT_SNAPSHOT_MISMATCH',
          `resume assignment for ${expected.kernelSessionLineageId} differs from its stored context`,
        )
      }
      owner = await claimStoredOwner(stored, this.#now)
      const info = await this.#kernel.resumeSession({
        rolloutPath: stored.latestAccepted.rolloutPath,
        cwd: stored.context.workspace.path,
        provider: stored.context.roleSpec.modelRoute.provider,
        model: stored.context.roleSpec.modelRoute.model,
        governedAuthority: createStrongFlowRoleKernelAuthority(stored.context),
      })
      const accepted = await this.#acceptNativeSession(
        stored.context,
        info,
        stored.latestAccepted.generation + 1,
        'resume',
        stored.latestAccepted.acceptedAtMillis,
      )
      const prepared = await this.#prepare(stored.context, accepted, owner)
      try {
        await appendRecord(stored.lifecyclePath, accepted)
        prepared.runtime.acceptDurable(stored.lifecyclePath, stored.ownerPath, owner)
        this.#live.set(stored.context.kernelSessionLineageId, prepared.runtime)
        return prepared.runtime
      } catch (error) {
        return this.#rollbackPrepared(
          prepared,
          'Role-session resume publication failed',
          error,
        )
      }
    } catch (error) {
      let ownerReleaseFailure: unknown
      if (owner !== undefined && stored !== undefined) {
        try {
          await releaseOwner(stored.ownerPath, owner)
        } catch (releaseError) {
          ownerReleaseFailure = releaseError
        }
      }
      if (ownerReleaseFailure !== undefined) {
        throw new StrongFlowRoleSessionError(
          'SESSION_SETUP_ROLLBACK_FAILED',
          `role-session resume ownership could not be released for ${expected.kernelSessionLineageId}`,
          { cause: new AggregateError([error, ownerReleaseFailure]) },
        )
      }
      if (error instanceof StrongFlowRoleSessionError) throw error
      throw new StrongFlowRoleSessionError(
        'SESSION_SETUP_FAILED',
        `StrongFlow role session ${expected.kernelSessionLineageId} could not be resumed`,
        { cause: error },
      )
    } finally {
      await releaseLock()
    }
  }

  async #prepare(
    context: StrongFlowRoleSessionContext,
    accepted: AcceptedLifecycleRecord,
    existingOwner?: OwnerRecord,
  ): Promise<PreparedSession> {
    const kernel = publicKernelLifecycle(accepted)
    const runtime = new ManagedStrongFlowRoleSession({
      context,
      kernelLifecycle: kernel,
      kernel: this.#kernel,
      eventBufferCapacity: this.#eventBufferCapacity,
      now: this.#now,
      onTerminal: lineageId => {
        if (this.#live.get(lineageId) === runtime) this.#live.delete(lineageId)
      },
    })
    const owner = existingOwner ?? ownerRecord(context, this.#now)
    try {
      await runtime.awaitSubscription()
      await Promise.resolve()
      if (runtime.eventFailure !== undefined) throw runtime.eventFailure
      const installed = await this.#installer.install(Object.freeze({
        source: accepted.source,
        context,
        kernel,
        signal: runtime.setupSignal,
      }))
      if (isObject(installed) && typeof installed.dispose === 'function') {
        runtime.install(installed)
      }
      runtime.install(validateInstallation(installed, context))
      return Object.freeze({ context, kernel, runtime, owner })
    } catch (error) {
      try {
        await runtime.rollback('Role context installation failed')
      } catch (rollbackError) {
        throw new StrongFlowRoleSessionError(
          'SESSION_SETUP_ROLLBACK_FAILED',
          `role context setup and rollback failed for ${context.kernelSessionLineageId}`,
          { cause: new AggregateError([error, rollbackError]) },
        )
      }
      if (error instanceof StrongFlowRoleSessionError) throw error
      throw new StrongFlowRoleSessionError(
        'SESSION_SETUP_FAILED',
        `role context setup failed for ${context.kernelSessionLineageId}`,
        { cause: error },
      )
    }
  }

  async #acceptNativeSession(
    context: StrongFlowRoleSessionContext,
    info: SessionInfo,
    generation: number,
    source: 'create' | 'resume',
    notBeforeMillis = 0,
  ): Promise<AcceptedLifecycleRecord> {
    try {
      return acceptedLifecycle(
        context,
        info,
        generation,
        source,
        this.#now,
        notBeforeMillis,
      )
    } catch (error) {
      const sessionId = isRecord(info) && typeof info.sessionId === 'string'
        ? info.sessionId
        : undefined
      if (sessionId !== undefined && sessionId.length > 0) {
        try {
          await this.#kernel.closeSession(sessionId)
        } catch (rollbackError) {
          throw new StrongFlowRoleSessionError(
            'SESSION_SETUP_ROLLBACK_FAILED',
            `invalid native session for ${context.kernelSessionLineageId} could not be closed`,
            { cause: new AggregateError([error, rollbackError]) },
          )
        }
      }
      throw error
    }
  }

  async #rollbackPrepared(
    prepared: PreparedSession,
    reason: string,
    sourceError: unknown,
  ): Promise<never> {
    try {
      await prepared.runtime.rollback(reason)
    } catch (rollbackError) {
      throw new StrongFlowRoleSessionError(
        'SESSION_SETUP_ROLLBACK_FAILED',
        `role-session publication and rollback failed for ${prepared.context.kernelSessionLineageId}`,
        { cause: new AggregateError([sourceError, rollbackError]) },
      )
    }
    throw new StrongFlowRoleSessionError(
      'SESSION_SETUP_FAILED',
      `role-session publication failed for ${prepared.context.kernelSessionLineageId}`,
      { cause: sourceError },
    )
  }
}
