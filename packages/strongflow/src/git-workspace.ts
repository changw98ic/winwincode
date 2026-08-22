import { createHash, randomUUID } from 'node:crypto'
import { spawn } from 'node:child_process'
import {
  lstat,
  link,
  mkdir,
  open,
  readFile,
  realpath,
  readdir,
  rename,
  rm,
  unlink,
} from 'node:fs/promises'
import { dirname, isAbsolute, join, resolve } from 'node:path'

import {
  CandidateId,
  JobId,
  type AttemptId,
  type JobId as JobIdentifier,
  StageRunId,
  type StrongFlowCandidateIdentity,
  type StrongFlowCandidateWriterLease,
  type StrongFlowRoleId,
  type StrongFlowSourceSnapshotIdentity,
  type StrongFlowVerificationSnapshotIdentity,
} from '@winwincode/contracts'

import {
  admitStrongFlowSource,
  claimStrongFlowCandidateWriter,
  createStrongFlowCandidateIdentity,
  createStrongFlowGitDiffId,
  createStrongFlowVerificationSnapshotIdentity,
  createStrongFlowWorkspaceLayout,
  releaseStrongFlowCandidateWriter,
  strongFlowRoleWorkspace,
  strongFlowWorkspaceRootForJob,
  type AdmittedStrongFlowSource,
  type ObservedGitOperation,
  type ObservedGitSourceState,
  type StrongFlowWorkspaceLayout,
} from './workspace-policy.js'

export const STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_CANDIDATE_SCHEMA_VERSION = 1 as const
const OWNER_MAGIC = 'winwincode-strongflow-git-workspace'
const OPERATION_LOCK_MAGIC = 'winwincode-strongflow-operation-lock'

export type StrongFlowGitWorkspaceErrorCode =
  | 'INVALID_MANAGER_OPTIONS'
  | 'INVALID_REPOSITORY'
  | 'INVALID_REVISION'
  | 'GIT_COMMAND_FAILED'
  | 'GIT_COMMAND_TIMEOUT'
  | 'GIT_OUTPUT_LIMIT'
  | 'SOURCE_CHANGED'
  | 'WORKSPACE_EXISTS'
  | 'WORKSPACE_NOT_FOUND'
  | 'WORKSPACE_CORRUPT'
  | 'WORKSPACE_NOT_OWNED'
  | 'SOURCE_SNAPSHOT_MUTATED'
  | 'WRITER_ACTIVE'
  | 'WRITER_OPERATION_TIMEOUT'
  | 'CANDIDATE_CONFLICT'
  | 'CANDIDATE_SCOPE_VIOLATION'
  | 'CANDIDATE_NOT_FROZEN'
  | 'CANDIDATE_CHANGED'
  | 'VERIFICATION_SNAPSHOT_NOT_FOUND'
  | 'VERIFICATION_SNAPSHOT_MISMATCH'
  | 'VERIFICATION_ACTIVE'
  | 'WORKSPACE_IO_ERROR'
  | 'CLEANUP_FAILED'

export class StrongFlowGitWorkspaceError extends Error {
  readonly code: StrongFlowGitWorkspaceErrorCode
  readonly retainedWorkspacePath?: string

  constructor(
    code: StrongFlowGitWorkspaceErrorCode,
    message: string,
    options?: ErrorOptions & { readonly retainedWorkspacePath?: string },
  ) {
    super(message, options)
    this.name = 'StrongFlowGitWorkspaceError'
    this.code = code
    if (options?.retainedWorkspacePath !== undefined) {
      this.retainedWorkspacePath = options.retainedWorkspacePath
    }
  }
}

export interface StrongFlowGitWorkspaceManagerOptions {
  readonly home: string
  readonly gitExecutable?: string
  readonly commandTimeoutMillis?: number
  readonly maxCommandOutputBytes?: number
  readonly clock?: () => number
  readonly ownerIdFactory?: () => string
}

export interface InspectStrongFlowGitSourceInput {
  readonly repositoryPath: string
  readonly revision?: string
}

export interface CreateStrongFlowGitWorkspaceInput extends InspectStrongFlowGitSourceInput {
  readonly jobId: JobIdentifier
}

export interface StrongFlowGitWorkspaceManifest {
  readonly schemaVersion: typeof STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION
  readonly ownerMagic: typeof OWNER_MAGIC
  readonly ownerId: string
  readonly createdAtMillis: number
  readonly jobId: JobIdentifier
  readonly repositoryPath: string
  readonly repositoryCommonDir: string
  readonly requestedRevision: string
  readonly sourceSnapshot: StrongFlowSourceSnapshotIdentity
  readonly workspaceId: StrongFlowWorkspaceLayout['workspaceId']
  readonly root: string
  readonly sourcePath: string
  readonly candidatePath: string
  readonly verificationRoot: string
  readonly metadataPath: string
}

export interface StrongFlowGitWorkspaceHandle {
  readonly manifest: StrongFlowGitWorkspaceManifest
  readonly layout: StrongFlowWorkspaceLayout
}

export interface StrongFlowGitWorkspaceStatus {
  readonly handle: StrongFlowGitWorkspaceHandle
  readonly source: {
    readonly commitId: string
    readonly treeId: string
    readonly clean: true
  }
  readonly candidate: {
    readonly commitId: string
    readonly treeId: string
    readonly clean: boolean
    readonly status: string
  }
  readonly writer?: StrongFlowCandidateWriterLease
}

export interface ClaimGitWorkspaceWriterInput {
  readonly leaseId: string
  readonly roleId: StrongFlowRoleId
  readonly stageRunId: StageRunId
  readonly attemptId: AttemptId
}

export interface StrongFlowGitWorkspaceDisposeResult {
  readonly status: 'removed' | 'absent'
  readonly root: string
}

export interface StrongFlowGitWorkspaceReconciliationResult {
  readonly status: 'absent' | 'ready' | 'operation-active' | 'cleanup-required'
  readonly root: string
  readonly operationLock: 'none' | 'active' | 'reclaimed'
  readonly retainedWorkspacePath?: string
  readonly operationProcessId?: number
}

export type StrongFlowCandidateScope =
  | { readonly mode: 'repository' }
  | { readonly mode: 'paths'; readonly paths: readonly string[] }

export interface FreezeStrongFlowCandidateInput {
  readonly scope: StrongFlowCandidateScope
}

export interface StrongFlowCandidateRecord {
  readonly schemaVersion: typeof STRONGFLOW_CANDIDATE_SCHEMA_VERSION
  readonly jobId: JobIdentifier
  readonly workspaceId: StrongFlowWorkspaceLayout['workspaceId']
  readonly sourceSnapshot: StrongFlowSourceSnapshotIdentity
  readonly candidate: StrongFlowCandidateIdentity
  readonly scope: StrongFlowCandidateScope
  readonly changedPaths: readonly string[]
  readonly diffByteLength: number
  readonly diffFileName: string
}

export interface CreateStrongFlowVerificationWorkspaceInput {
  readonly candidateId: string
  readonly roleId: 'reviewer' | 'verifier' | 'adversarial-verifier'
  readonly stageRunId: string
}

export interface StrongFlowVerificationWorkspaceManifest {
  readonly schemaVersion: typeof STRONGFLOW_CANDIDATE_SCHEMA_VERSION
  readonly jobId: JobIdentifier
  readonly workspaceId: StrongFlowWorkspaceLayout['workspaceId']
  readonly candidate: StrongFlowCandidateIdentity
  readonly verificationSnapshot: StrongFlowVerificationSnapshotIdentity
  readonly roleId: CreateStrongFlowVerificationWorkspaceInput['roleId']
  readonly stageRunId: StageRunId
  readonly path: string
  readonly temporaryOutputPath: string
}

export interface StrongFlowVerificationWorkspaceHandle {
  readonly manifest: StrongFlowVerificationWorkspaceManifest
  readonly candidateRecord: StrongFlowCandidateRecord
}

export interface DisposeStrongFlowVerificationWorkspaceInput {
  readonly candidateId: string
  readonly roleId: CreateStrongFlowVerificationWorkspaceInput['roleId']
  readonly stageRunId: string
}

export interface StrongFlowVerificationWorkspaceDisposeResult {
  readonly status: 'removed' | 'absent'
  readonly path: string
  readonly temporaryOutputPath: string
}

interface GitCommandResult {
  readonly exitCode: number
  readonly stdout: string
  readonly stdoutBytes: Uint8Array
  readonly stderr: string
}

interface GitSourceObservation {
  readonly admitted: AdmittedStrongFlowSource
  readonly repositoryCommonDir: string
  readonly requestedRevision: string
  readonly status: string
  readonly headCommitId: string
  readonly headTreeId: string
}

interface WorkspaceOwnerRecord {
  readonly schemaVersion: typeof STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION
  readonly ownerMagic: typeof OWNER_MAGIC
  readonly ownerId: string
  readonly createdAtMillis: number
  readonly jobId: JobIdentifier
  readonly repositoryPath: string
  readonly repositoryCommonDir: string
  readonly requestedRevision: string
  readonly sourceSnapshot: StrongFlowSourceSnapshotIdentity
}

interface VerificationWorkspaceOwnerRecord {
  readonly schemaVersion: typeof STRONGFLOW_CANDIDATE_SCHEMA_VERSION
  readonly ownerMagic: 'winwincode-strongflow-verification-workspace'
  readonly jobId: JobIdentifier
  readonly workspaceId: StrongFlowWorkspaceLayout['workspaceId']
  readonly candidateId: StrongFlowCandidateIdentity['candidateId']
  readonly roleId: CreateStrongFlowVerificationWorkspaceInput['roleId']
  readonly stageRunId: StageRunId
  readonly path: string
  readonly temporaryOutputPath: string
}

interface VerificationWorkspaceRequest {
  readonly candidateId: StrongFlowCandidateIdentity['candidateId']
  readonly roleId: CreateStrongFlowVerificationWorkspaceInput['roleId']
  readonly stageRunId: StageRunId
}

interface WorkspaceOperationLockRecord {
  readonly schemaVersion: typeof STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION
  readonly ownerMagic: typeof OPERATION_LOCK_MAGIC
  readonly ownerToken: string
  readonly processId: number
  readonly acquiredAtMillis: number
  readonly jobId: JobIdentifier
  readonly workspaceId: StrongFlowWorkspaceLayout['workspaceId']
}

interface WorkspaceOperationLockReconciliation {
  readonly state: 'none' | 'active' | 'reclaimed'
  readonly processId?: number
}

function workspaceError(
  code: StrongFlowGitWorkspaceErrorCode,
  message: string,
  options?: ErrorOptions & { readonly retainedWorkspacePath?: string },
): never {
  throw new StrongFlowGitWorkspaceError(code, message, options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function errorCode(value: unknown): string | undefined {
  if (typeof value !== 'object' || value === null || !('code' in value)) return undefined
  return typeof value.code === 'string' ? value.code : undefined
}

function isBoundedCommandFailure(
  error: unknown,
): error is StrongFlowGitWorkspaceError {
  return error instanceof StrongFlowGitWorkspaceError
    && (error.code === 'GIT_COMMAND_TIMEOUT' || error.code === 'GIT_OUTPUT_LIMIT')
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
  label: string,
  failureCode: StrongFlowGitWorkspaceErrorCode = 'WORKSPACE_CORRUPT',
): void {
  const allowed = new Set([...required, ...optional])
  if (
    required.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !allowed.has(key))
  ) workspaceError(failureCode, `${label} has an unexpected shape`)
}

function portableText(
  value: unknown,
  label: string,
  max = 500,
  failureCode: StrongFlowGitWorkspaceErrorCode = 'INVALID_MANAGER_OPTIONS',
): string {
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.length > max
    || value.trim() !== value
    || /[\u0000-\u001f\u007f]/u.test(value)
  ) workspaceError(failureCode, `${label} must be non-empty portable text`)
  return value
}

function parseJobId(
  value: unknown,
  failureCode: StrongFlowGitWorkspaceErrorCode,
): JobIdentifier {
  try {
    return JobId(value as string)
  } catch (error) {
    return workspaceError(failureCode, 'StrongFlow job id is invalid', { cause: error })
  }
}

function parseCandidateId(
  value: unknown,
  failureCode: StrongFlowGitWorkspaceErrorCode,
): StrongFlowCandidateIdentity['candidateId'] {
  try {
    return CandidateId(value as string)
  } catch (error) {
    return workspaceError(failureCode, 'StrongFlow candidate id is invalid', { cause: error })
  }
}

function parseStageRunId(
  value: unknown,
  failureCode: StrongFlowGitWorkspaceErrorCode,
): StageRunId {
  try {
    return StageRunId(value as string)
  } catch (error) {
    return workspaceError(failureCode, 'StrongFlow stage run id is invalid', { cause: error })
  }
}

function nonNegativeInteger(
  value: unknown,
  label: string,
  failureCode: StrongFlowGitWorkspaceErrorCode,
): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0) {
    return workspaceError(failureCode, `${label} must be a non-negative safe integer`)
  }
  return Number(value)
}

function safeInteger(value: unknown, label: string, maximum: number): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1 || Number(value) > maximum) {
    workspaceError(
      'INVALID_MANAGER_OPTIONS',
      `${label} must be a positive integer no greater than ${maximum}`,
    )
  }
  return Number(value)
}

function candidateScope(
  value: unknown,
  failureCode: StrongFlowGitWorkspaceErrorCode = 'INVALID_MANAGER_OPTIONS',
): StrongFlowCandidateScope {
  if (!isRecord(value)) {
    return workspaceError(failureCode, 'candidate scope must be an object')
  }
  if (value.mode === 'repository') {
    exactKeys(value, ['mode'], [], 'repository candidate scope', failureCode)
    return Object.freeze({ mode: 'repository' as const })
  }
  if (value.mode !== 'paths') {
    return workspaceError(failureCode, 'candidate scope mode is unknown')
  }
  exactKeys(value, ['mode', 'paths'], [], 'path candidate scope', failureCode)
  if (!Array.isArray(value.paths)) {
    return workspaceError(failureCode, 'candidate scope paths must be an array')
  }
  const paths = value.paths.map((path, index) => portableCandidatePath(
    path,
    `candidate scope path ${index}`,
    failureCode,
  )).sort()
  if (paths.length === 0 || new Set(paths).size !== paths.length) {
    return workspaceError(
      failureCode,
      'path candidate scope must contain distinct paths',
    )
  }
  return Object.freeze({ mode: 'paths' as const, paths: Object.freeze(paths) })
}

function portableCandidatePath(
  value: unknown,
  label: string,
  failureCode: StrongFlowGitWorkspaceErrorCode,
): string {
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.length > 4_096
    || isAbsolute(value)
    || value.includes('\\')
    || /^[A-Za-z]:/u.test(value)
    || /[\u0000-\u001f\u007f]/u.test(value)
  ) return workspaceError(failureCode, `${label} must be a portable relative path`)
  const segments = value.split('/')
  if (segments.some(segment => segment.length === 0 || segment === '.' || segment === '..')) {
    return workspaceError(failureCode, `${label} contains an invalid path segment`)
  }
  return value
}

function candidateRecordKey(candidateId: string): string {
  return createHash('sha256').update(candidateId).digest('hex')
}

function pathIsInScope(path: string, scope: StrongFlowCandidateScope): boolean {
  return scope.mode === 'repository'
    || scope.paths.some(root => path === root || path.startsWith(`${root}/`))
}

function nullDelimitedPaths(bytes: Uint8Array): readonly string[] {
  let text: string
  try {
    text = new TextDecoder('utf-8', { fatal: true }).decode(bytes)
  } catch (error) {
    return workspaceError('CANDIDATE_SCOPE_VIOLATION', 'candidate paths must be valid UTF-8', {
      cause: error,
    })
  }
  if (text.length === 0) return Object.freeze([])
  if (!text.endsWith('\0')) {
    return workspaceError('GIT_COMMAND_FAILED', 'Git path output was not NUL terminated')
  }
  const paths = text.slice(0, -1).split('\0').map((path, index) => portableCandidatePath(
    path,
    `changed candidate path ${index}`,
    'CANDIDATE_SCOPE_VIOLATION',
  )).sort()
  if (new Set(paths).size !== paths.length) {
    return workspaceError('GIT_COMMAND_FAILED', 'Git returned duplicate changed paths')
  }
  return Object.freeze(paths)
}

function sameBytes(left: Uint8Array, right: Uint8Array): boolean {
  return left.length === right.length && left.every((byte, index) => byte === right[index])
}

async function pathKind(path: string): Promise<'missing' | 'file' | 'directory' | 'symlink'> {
  try {
    const entry = await lstat(path)
    if (entry.isSymbolicLink()) return 'symlink'
    if (entry.isDirectory()) return 'directory'
    return 'file'
  } catch (error) {
    if (errorCode(error) === 'ENOENT') return 'missing'
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

async function writeNewBytes(path: string, value: string | Uint8Array): Promise<void> {
  const handle = await open(path, 'wx', 0o600)
  try {
    await handle.writeFile(value)
    await handle.sync()
  } finally {
    await handle.close()
  }
  await syncDirectory(dirname(path))
}

async function writeNewJson(path: string, value: unknown): Promise<void> {
  await writeNewBytes(path, `${JSON.stringify(value)}\n`)
}

async function replaceJson(path: string, value: unknown): Promise<void> {
  const temporaryPath = join(dirname(path), `.replace-${randomUUID()}.json`)
  await writeNewJson(temporaryPath, value)
  try {
    await rename(temporaryPath, path)
    await syncDirectory(dirname(path))
  } finally {
    await unlink(temporaryPath).catch(() => undefined)
  }
}

async function publishExclusiveJson(path: string, value: unknown): Promise<void> {
  const temporaryPath = join(dirname(path), `.publish-${randomUUID()}.json`)
  await writeNewJson(temporaryPath, value)
  try {
    await link(temporaryPath, path)
    await syncDirectory(dirname(path))
  } finally {
    await unlink(temporaryPath).catch(() => undefined)
    await syncDirectory(dirname(path)).catch(() => undefined)
  }
}

function oneLine(value: string): string {
  const lines = value.trim().split(/\r?\n/u).filter(line => line.length > 0)
  if (lines.length !== 1) {
    return workspaceError('GIT_COMMAND_FAILED', 'Git returned an unexpected multi-line value')
  }
  return lines[0]!
}

function revisionText(value: string | undefined): string {
  if (value === undefined) return 'HEAD'
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.length > 500
    || value.trim() !== value
    || /[\u0000-\u001f\u007f]/u.test(value)
  ) return workspaceError('INVALID_REVISION', 'Git revision is invalid')
  return value
}

function immutable<Value>(value: Value): Value {
  if (Array.isArray(value)) {
    for (const entry of value) immutable(entry)
    return Object.freeze(value)
  }
  if (isRecord(value)) {
    for (const entry of Object.values(value)) immutable(entry)
    return Object.freeze(value) as Value
  }
  return value
}

/** Owns local Git worktrees for one runtime home and never accepts caller-chosen cleanup paths. */
export class StrongFlowGitWorkspaceManager {
  readonly home: string
  readonly #gitExecutable: string
  readonly #commandTimeoutMillis: number
  readonly #maxCommandOutputBytes: number
  readonly #clock: () => number
  readonly #ownerIdFactory: () => string

  constructor(options: StrongFlowGitWorkspaceManagerOptions) {
    if (!isRecord(options)) {
      workspaceError('INVALID_MANAGER_OPTIONS', 'Git workspace manager options must be an object')
    }
    exactKeys(
      options,
      ['home'],
      [
        'gitExecutable',
        'commandTimeoutMillis',
        'maxCommandOutputBytes',
        'clock',
        'ownerIdFactory',
      ],
      'Git workspace manager options',
      'INVALID_MANAGER_OPTIONS',
    )
    if (typeof options.home !== 'string' || !isAbsolute(options.home)) {
      workspaceError('INVALID_MANAGER_OPTIONS', 'Git workspace home must be an absolute path')
    }
    this.home = resolve(options.home)
    this.#gitExecutable = options.gitExecutable === undefined
      ? 'git'
      : portableText(options.gitExecutable, 'gitExecutable')
    this.#commandTimeoutMillis = options.commandTimeoutMillis === undefined
      ? 30_000
      : safeInteger(options.commandTimeoutMillis, 'commandTimeoutMillis', 600_000)
    this.#maxCommandOutputBytes = options.maxCommandOutputBytes === undefined
      ? 1_048_576
      : safeInteger(options.maxCommandOutputBytes, 'maxCommandOutputBytes', 16_777_216)
    if (options.clock !== undefined && typeof options.clock !== 'function') {
      workspaceError('INVALID_MANAGER_OPTIONS', 'workspace clock must be a function')
    }
    if (options.ownerIdFactory !== undefined && typeof options.ownerIdFactory !== 'function') {
      workspaceError('INVALID_MANAGER_OPTIONS', 'ownerIdFactory must be a function')
    }
    this.#clock = options.clock ?? Date.now
    this.#ownerIdFactory = options.ownerIdFactory ?? randomUUID
  }

  async inspectSource(
    input: InspectStrongFlowGitSourceInput,
  ): Promise<AdmittedStrongFlowSource> {
    return (await this.#observeSource(input)).admitted
  }

  async create(
    input: CreateStrongFlowGitWorkspaceInput,
  ): Promise<StrongFlowGitWorkspaceHandle> {
    if (!isRecord(input)) {
      return workspaceError('INVALID_MANAGER_OPTIONS', 'workspace creation input must be an object')
    }
    exactKeys(
      input,
      ['jobId', 'repositoryPath'],
      ['revision'],
      'workspace creation input',
      'INVALID_MANAGER_OPTIONS',
    )
    const jobId = parseJobId(input.jobId, 'INVALID_MANAGER_OPTIONS')
    const observation = await this.#observeSource({
      repositoryPath: input.repositoryPath,
      ...(input.revision === undefined ? {} : { revision: input.revision }),
    })
    const layout = createStrongFlowWorkspaceLayout({
      home: this.home,
      jobId,
      sourceSnapshot: observation.admitted.identity,
    })
    const managedParent = await this.#ensureManagedParent()
    try {
      await mkdir(layout.root)
    } catch (error) {
      if (errorCode(error) === 'EEXIST') {
        return workspaceError(
          'WORKSPACE_EXISTS',
          `StrongFlow workspace already exists for job ${jobId}`,
        )
      }
      return workspaceError('WORKSPACE_IO_ERROR', 'workspace root could not be created', {
        cause: error,
      })
    }
    try {
      if (
        await pathKind(layout.root) !== 'directory'
        || dirname(await realpath(layout.root)) !== managedParent
      ) {
        return workspaceError('WORKSPACE_NOT_OWNED', 'new workspace root escaped its managed parent')
      }
    } catch (error) {
      if (error instanceof StrongFlowGitWorkspaceError) throw error
      return workspaceError('WORKSPACE_IO_ERROR', 'new workspace root could not be verified', {
        cause: error,
        retainedWorkspacePath: layout.root,
      })
    }

    let owner: WorkspaceOwnerRecord
    try {
      await mkdir(layout.metadataPath)
      owner = this.#ownerRecord(jobId, observation, layout)
      await writeNewJson(join(layout.metadataPath, 'owner.json'), owner)
    } catch (error) {
      return workspaceError(
        'WORKSPACE_IO_ERROR',
        'workspace owner record could not be published',
        { cause: error, retainedWorkspacePath: layout.root },
      )
    }

    try {
      await this.#git([
        '-C',
        observation.admitted.repositoryPath,
        'worktree',
        'add',
        '--detach',
        layout.sourcePath,
        observation.admitted.identity.commitId,
      ])
      await this.#git([
        '-C',
        observation.admitted.repositoryPath,
        'worktree',
        'add',
        '--detach',
        layout.candidatePath,
        observation.admitted.identity.commitId,
      ])
      await this.#verifyCreatedWorktree(
        layout.sourcePath,
        observation.admitted.identity,
        observation.repositoryCommonDir,
        true,
      )
      await this.#verifyCreatedWorktree(
        layout.candidatePath,
        observation.admitted.identity,
        observation.repositoryCommonDir,
        true,
      )
      await this.#assertOriginalUnchanged(observation)
      const manifest = this.#manifest(owner, layout)
      await writeNewJson(join(layout.metadataPath, 'manifest.json'), manifest)
      await syncDirectory(layout.root)
      return Object.freeze({ manifest, layout })
    } catch (error) {
      await this.#recordFailure(layout, error)
      if (error instanceof StrongFlowGitWorkspaceError) {
        return workspaceError(error.code, error.message, {
          cause: error,
          retainedWorkspacePath: layout.root,
        })
      }
      return workspaceError(
        'WORKSPACE_IO_ERROR',
        'Git workspace creation failed after ownership was recorded',
        { cause: error, retainedWorkspacePath: layout.root },
      )
    }
  }

  async open(jobIdInput: string): Promise<StrongFlowGitWorkspaceHandle> {
    const jobId = parseJobId(jobIdInput, 'INVALID_MANAGER_OPTIONS')
    const root = strongFlowWorkspaceRootForJob(this.home, jobId)
    if (await pathKind(root) === 'missing') {
      return workspaceError('WORKSPACE_NOT_FOUND', `workspace for job ${jobId} does not exist`)
    }
    const owner = await this.#readOwner(root, jobId)
    const layout = createStrongFlowWorkspaceLayout({
      home: this.home,
      jobId,
      sourceSnapshot: owner.sourceSnapshot,
    })
    await this.#assertOwnedRoot(root, layout, owner)
    const manifest = await this.#readManifest(layout, owner)
    return Object.freeze({ manifest, layout })
  }

  async reconcile(
    jobIdInput: string,
  ): Promise<StrongFlowGitWorkspaceReconciliationResult> {
    const jobId = parseJobId(jobIdInput, 'INVALID_MANAGER_OPTIONS')
    const root = strongFlowWorkspaceRootForJob(this.home, jobId)
    if (await pathKind(root) === 'missing') {
      return Object.freeze({
        status: 'absent' as const,
        root,
        operationLock: 'none' as const,
      })
    }
    const owner = await this.#readOwner(root, jobId)
    const layout = createStrongFlowWorkspaceLayout({
      home: this.home,
      jobId,
      sourceSnapshot: owner.sourceSnapshot,
    })
    await this.#assertOwnedRoot(root, layout, owner)
    const operationLock = await this.#reconcileOperationLock(layout)
    if (operationLock.state === 'active') {
      if (operationLock.processId === undefined) {
        return workspaceError('WORKSPACE_CORRUPT', 'active writer lock has no process id')
      }
      return Object.freeze({
        status: 'operation-active' as const,
        root,
        operationLock: 'active' as const,
        retainedWorkspacePath: root,
        operationProcessId: operationLock.processId,
      })
    }
    const manifestKind = await pathKind(join(layout.metadataPath, 'manifest.json'))
    if (manifestKind !== 'missing' && manifestKind !== 'file') {
      return workspaceError('WORKSPACE_CORRUPT', 'workspace manifest was replaced', {
        retainedWorkspacePath: root,
      })
    }
    if (manifestKind === 'file') await this.#readManifest(layout, owner)
    let cleanupRequired = manifestKind === 'missing'
    for (const path of [layout.sourcePath, layout.candidatePath]) {
      const kind = await pathKind(path)
      if (kind === 'missing') {
        cleanupRequired = true
        continue
      }
      if (kind !== 'directory') {
        return workspaceError('WORKSPACE_NOT_OWNED', 'managed worktree path was replaced', {
          retainedWorkspacePath: root,
        })
      }
      await this.#assertWorktreeRegistration(path, owner.repositoryCommonDir)
    }
    return Object.freeze({
      status: cleanupRequired ? 'cleanup-required' as const : 'ready' as const,
      root,
      operationLock: operationLock.state,
      retainedWorkspacePath: root,
      ...(operationLock.processId === undefined
        ? {}
        : { operationProcessId: operationLock.processId }),
    })
  }

  async inspect(jobIdInput: string): Promise<StrongFlowGitWorkspaceStatus> {
    const handle = await this.open(jobIdInput)
    const source = await this.#worktreeStatus(
      handle.layout.sourcePath,
      handle.manifest.repositoryCommonDir,
    )
    if (
      !source.clean
      || source.commitId !== handle.manifest.sourceSnapshot.commitId
      || source.treeId !== handle.manifest.sourceSnapshot.treeId
    ) {
      return workspaceError(
        'SOURCE_SNAPSHOT_MUTATED',
        'read-only source snapshot changed after workspace creation',
      )
    }
    const candidate = await this.#worktreeStatus(
      handle.layout.candidatePath,
      handle.manifest.repositoryCommonDir,
    )
    const writer = await this.#readWriter(handle, false)
    return Object.freeze({
      handle,
      source: Object.freeze({
        commitId: source.commitId,
        treeId: source.treeId,
        clean: true as const,
      }),
      candidate: Object.freeze(candidate),
      ...(writer === undefined ? {} : { writer }),
    })
  }

  async claimWriter(
    jobIdInput: string,
    input: ClaimGitWorkspaceWriterInput,
  ): Promise<StrongFlowCandidateWriterLease> {
    if (!isRecord(input)) {
      return workspaceError('INVALID_MANAGER_OPTIONS', 'writer claim input must be an object')
    }
    exactKeys(
      input,
      ['leaseId', 'roleId', 'stageRunId', 'attemptId'],
      [],
      'writer claim input',
      'INVALID_MANAGER_OPTIONS',
    )
    const handle = await this.open(jobIdInput)
    return this.#withWriterOperationLock(handle.layout, async () => {
      const request = {
        ...input,
        jobId: handle.manifest.jobId,
        workspaceId: handle.manifest.workspaceId,
        acquiredAtMillis: this.#now(),
      }
      const requested = claimStrongFlowCandidateWriter(undefined, request)
      const current = await this.#readWriter(handle, false)
      if (current !== undefined) {
        if (
          current.leaseId === requested.leaseId
          && current.jobId === requested.jobId
          && current.workspaceId === requested.workspaceId
          && current.roleId === requested.roleId
          && current.stageRunId === requested.stageRunId
          && current.attemptId === requested.attemptId
        ) return current
        return claimStrongFlowCandidateWriter(current, request)
      }
      try {
        await publishExclusiveJson(join(handle.layout.metadataPath, 'writer.json'), requested)
        return requested
      } catch (error) {
        return workspaceError('WORKSPACE_IO_ERROR', 'writer lease could not be published', {
          cause: error,
        })
      }
    })
  }

  async releaseWriter(jobIdInput: string, leaseId: string): Promise<void> {
    const handle = await this.open(jobIdInput)
    return this.#withWriterOperationLock(handle.layout, async () => {
      const current = await this.#readWriter(handle, true)
      releaseStrongFlowCandidateWriter(current, leaseId)
      try {
        await unlink(join(handle.layout.metadataPath, 'writer.json'))
        await syncDirectory(handle.layout.metadataPath)
      } catch (error) {
        return workspaceError('WORKSPACE_IO_ERROR', 'writer lease could not be released', {
          cause: error,
        })
      }
    })
  }

  async dispose(jobIdInput: string): Promise<StrongFlowGitWorkspaceDisposeResult> {
    const jobId = parseJobId(jobIdInput, 'INVALID_MANAGER_OPTIONS')
    const root = strongFlowWorkspaceRootForJob(this.home, jobId)
    if (await pathKind(root) === 'missing') {
      return Object.freeze({ status: 'absent', root })
    }
    const owner = await this.#readOwner(root, jobId)
    const layout = createStrongFlowWorkspaceLayout({
      home: this.home,
      jobId,
      sourceSnapshot: owner.sourceSnapshot,
    })
    await this.#assertOwnedRoot(root, layout, owner)
    return this.#withWriterOperationLock(layout, async () => {
      const writerPath = join(layout.metadataPath, 'writer.json')
      const writerKind = await pathKind(writerPath)
      if (writerKind !== 'missing') {
        if (writerKind !== 'file') {
          return workspaceError('WORKSPACE_NOT_OWNED', 'writer lease path was replaced')
        }
        return workspaceError(
          'WRITER_ACTIVE',
          'active writer lease must be released before cleanup',
        )
      }
      for (const path of [
        layout.verificationRoot,
        join(layout.root, 'verification-output'),
      ]) {
        const kind = await pathKind(path)
        if (kind === 'missing') continue
        if (kind !== 'directory') {
          return workspaceError(
            'WORKSPACE_NOT_OWNED',
            'verification workspace parent was replaced',
          )
        }
        if ((await readdir(path)).length > 0) {
          return workspaceError(
            'VERIFICATION_ACTIVE',
            'verification workspaces must be disposed before job cleanup',
          )
        }
      }
      try {
        const worktrees: string[] = []
        for (const path of [layout.sourcePath, layout.candidatePath]) {
          const kind = await pathKind(path)
          if (kind === 'missing') continue
          if (kind !== 'directory') {
            return workspaceError(
              'WORKSPACE_NOT_OWNED',
              'owned worktree path was replaced by a non-directory entry',
            )
          }
          await this.#assertWorktreeRegistration(path, owner.repositoryCommonDir)
          worktrees.push(path)
        }
        for (const path of worktrees) {
          await this.#git([
            '-C',
            owner.repositoryPath,
            'worktree',
            'remove',
            '--force',
            path,
          ])
        }
        await this.#git(['-C', owner.repositoryPath, 'worktree', 'prune'])
        await rm(root, { recursive: true, force: true })
        await syncDirectory(dirname(root))
        return Object.freeze({ status: 'removed' as const, root })
      } catch (error) {
        if (error instanceof StrongFlowGitWorkspaceError) throw error
        return workspaceError('CLEANUP_FAILED', 'owned Git workspace cleanup failed', {
          cause: error,
          retainedWorkspacePath: root,
        })
      }
    })
  }

  async freezeCandidate(
    jobIdInput: string,
    input: FreezeStrongFlowCandidateInput,
  ): Promise<StrongFlowCandidateRecord> {
    if (!isRecord(input)) {
      return workspaceError('INVALID_MANAGER_OPTIONS', 'candidate freeze input must be an object')
    }
    exactKeys(
      input,
      ['scope'],
      [],
      'candidate freeze input',
      'INVALID_MANAGER_OPTIONS',
    )
    const scope = candidateScope(input.scope)
    const handle = await this.open(jobIdInput)
    return this.#withWriterOperationLock(handle.layout, async () => {
      if (await this.#readWriter(handle, false) !== undefined) {
        return workspaceError('WRITER_ACTIVE', 'writer must finish before candidate freeze')
      }
      await this.#assertSourceSnapshot(handle)
      return this.#materializeCandidate(handle, scope)
    })
  }

  async inspectFrozenCandidate(
    jobIdInput: string,
    candidateIdInput?: string,
  ): Promise<StrongFlowCandidateRecord> {
    const expectedCandidateId = candidateIdInput === undefined
      ? undefined
      : parseCandidateId(candidateIdInput, 'VERIFICATION_SNAPSHOT_MISMATCH')
    const handle = await this.open(jobIdInput)
    return this.#withWriterOperationLock(handle.layout, async () => {
      if (await this.#readWriter(handle, false) !== undefined) {
        return workspaceError('CANDIDATE_CHANGED', 'candidate has an active writer')
      }
      const record = await this.#readCurrentCandidate(handle)
      if (
        expectedCandidateId !== undefined
        && record.candidate.candidateId !== expectedCandidateId
      ) {
        return workspaceError(
          'VERIFICATION_SNAPSHOT_MISMATCH',
          'requested candidate is not the current freeze',
        )
      }
      await this.#assertSourceSnapshot(handle)
      await this.#assertFrozenCandidate(handle, record)
      return record
    })
  }

  async readFrozenCandidateDiff(
    jobIdInput: string,
    candidateIdInput: string,
  ): Promise<Uint8Array> {
    const candidateId = parseCandidateId(
      candidateIdInput,
      'VERIFICATION_SNAPSHOT_MISMATCH',
    )
    const handle = await this.open(jobIdInput)
    return this.#withWriterOperationLock(handle.layout, async () => {
      const record = await this.#readCandidateRecord(handle, candidateId)
      const paths = this.#candidateMetadataPaths(handle.layout, candidateId)
      const diff = await readFile(paths.diffPath)
      if (
        diff.length !== record.diffByteLength
        || createStrongFlowGitDiffId(diff) !== record.candidate.diffId
      ) return workspaceError('WORKSPACE_CORRUPT', 'candidate exact diff changed while reading')
      return Uint8Array.from(diff)
    })
  }

  async createVerificationWorkspace(
    jobIdInput: string,
    input: CreateStrongFlowVerificationWorkspaceInput,
  ): Promise<StrongFlowVerificationWorkspaceHandle> {
    const request = this.#verificationRequest(input)
    const handle = await this.open(jobIdInput)
    return this.#withWriterOperationLock(handle.layout, async () => {
      if (await this.#readWriter(handle, false) !== undefined) {
        return workspaceError('CANDIDATE_CHANGED', 'candidate has an active writer')
      }
      const candidateRecord = await this.#readCurrentCandidate(handle)
      if (candidateRecord.candidate.candidateId !== request.candidateId) {
        return workspaceError(
          'VERIFICATION_SNAPSHOT_MISMATCH',
          'verification request names a stale candidate',
        )
      }
      await this.#assertSourceSnapshot(handle)
      await this.#assertFrozenCandidate(handle, candidateRecord)
      const manifest = this.#verificationManifest(handle, candidateRecord, request)
      const owner = this.#verificationOwner(manifest)
      const metadata = this.#verificationMetadataPaths(handle.layout, request)
      await this.#ensureVerificationParents(handle.layout)
      const ownerKind = await pathKind(metadata.ownerPath)
      if (ownerKind !== 'missing') {
        if (ownerKind !== 'file') {
          return workspaceError(
            'VERIFICATION_SNAPSHOT_MISMATCH',
            'verification owner marker was replaced',
          )
        }
        return this.#openVerificationWorkspaceLocked(
          handle,
          candidateRecord,
          request,
          manifest,
        )
      }
      try {
        await writeNewJson(metadata.ownerPath, owner)
      } catch (error) {
        return workspaceError(
          'WORKSPACE_IO_ERROR',
          'verification workspace owner could not be published',
          { cause: error, retainedWorkspacePath: manifest.path },
        )
      }
      try {
        await this.#git([
          '-C',
          handle.manifest.repositoryPath,
          'worktree',
          'add',
          '--detach',
          manifest.path,
          manifest.candidate.candidateCommitId,
        ])
        await mkdir(manifest.temporaryOutputPath)
        const status = await this.#worktreeStatus(
          manifest.path,
          handle.manifest.repositoryCommonDir,
        )
        if (
          !status.clean
          || status.commitId !== manifest.verificationSnapshot.candidateCommitId
          || status.treeId !== manifest.verificationSnapshot.candidateTreeId
        ) {
          return workspaceError(
            'VERIFICATION_SNAPSHOT_MISMATCH',
            'created verification workspace does not match the frozen candidate',
          )
        }
        await writeNewJson(metadata.manifestPath, manifest)
        await this.#assertFrozenCandidate(handle, candidateRecord)
        return Object.freeze({ manifest, candidateRecord })
      } catch (error) {
        await this.#recordVerificationFailure(metadata.failurePath, error)
        if (error instanceof StrongFlowGitWorkspaceError) {
          return workspaceError(error.code, error.message, {
            cause: error,
            retainedWorkspacePath: manifest.path,
          })
        }
        return workspaceError(
          'WORKSPACE_IO_ERROR',
          'verification workspace creation failed after ownership was recorded',
          { cause: error, retainedWorkspacePath: manifest.path },
        )
      }
    })
  }

  async openVerificationWorkspace(
    jobIdInput: string,
    input: CreateStrongFlowVerificationWorkspaceInput,
  ): Promise<StrongFlowVerificationWorkspaceHandle> {
    const request = this.#verificationRequest(input)
    const handle = await this.open(jobIdInput)
    return this.#withWriterOperationLock(handle.layout, async () => {
      if (await this.#readWriter(handle, false) !== undefined) {
        return workspaceError('CANDIDATE_CHANGED', 'candidate has an active writer')
      }
      const candidateRecord = await this.#readCurrentCandidate(handle)
      if (candidateRecord.candidate.candidateId !== request.candidateId) {
        return workspaceError(
          'VERIFICATION_SNAPSHOT_MISMATCH',
          'verification workspace refers to a stale candidate',
        )
      }
      await this.#assertSourceSnapshot(handle)
      await this.#assertFrozenCandidate(handle, candidateRecord)
      const manifest = this.#verificationManifest(handle, candidateRecord, request)
      return this.#openVerificationWorkspaceLocked(
        handle,
        candidateRecord,
        request,
        manifest,
      )
    })
  }

  async disposeVerificationWorkspace(
    jobIdInput: string,
    input: DisposeStrongFlowVerificationWorkspaceInput,
  ): Promise<StrongFlowVerificationWorkspaceDisposeResult> {
    const request = this.#verificationRequest(input)
    const handle = await this.open(jobIdInput)
    return this.#withWriterOperationLock(handle.layout, async () => {
      const candidateRecord = await this.#readCandidateRecord(
        handle,
        request.candidateId,
      )
      const manifest = this.#verificationManifest(handle, candidateRecord, request)
      const metadata = this.#verificationMetadataPaths(handle.layout, request)
      const pathEntry = await pathKind(manifest.path)
      const outputEntry = await pathKind(manifest.temporaryOutputPath)
      const ownerEntry = await pathKind(metadata.ownerPath)
      if (
        pathEntry === 'missing'
        && outputEntry === 'missing'
        && ownerEntry === 'missing'
      ) {
        return Object.freeze({
          status: 'absent' as const,
          path: manifest.path,
          temporaryOutputPath: manifest.temporaryOutputPath,
        })
      }
      await this.#assertVerificationOwner(metadata.ownerPath, this.#verificationOwner(manifest))
      if (outputEntry !== 'missing' && outputEntry !== 'directory') {
        return workspaceError(
          'VERIFICATION_SNAPSHOT_MISMATCH',
          'verification output path was replaced',
        )
      }
      if (pathEntry !== 'missing') {
        if (pathEntry !== 'directory') {
          return workspaceError(
            'VERIFICATION_SNAPSHOT_MISMATCH',
            'verification workspace path was replaced',
          )
        }
        await this.#assertWorktreeRegistration(
          manifest.path,
          handle.manifest.repositoryCommonDir,
        )
        await this.#git([
          '-C',
          handle.manifest.repositoryPath,
          'worktree',
          'remove',
          '--force',
          manifest.path,
        ])
      }
      await rm(manifest.temporaryOutputPath, { recursive: true, force: true })
      for (const path of [
        metadata.manifestPath,
        metadata.failurePath,
        metadata.ownerPath,
      ]) await unlink(path).catch(error => {
        if (errorCode(error) !== 'ENOENT') throw error
      })
      await this.#git(['-C', handle.manifest.repositoryPath, 'worktree', 'prune'])
      await syncDirectory(metadata.directory)
      return Object.freeze({
        status: 'removed' as const,
        path: manifest.path,
        temporaryOutputPath: manifest.temporaryOutputPath,
      })
    })
  }

  async #observeSource(
    input: InspectStrongFlowGitSourceInput,
  ): Promise<GitSourceObservation> {
    if (!isRecord(input) || typeof input.repositoryPath !== 'string') {
      return workspaceError('INVALID_REPOSITORY', 'repository input is invalid')
    }
    exactKeys(
      input,
      ['repositoryPath'],
      ['revision'],
      'repository input',
      'INVALID_REPOSITORY',
    )
    let requestedPath: string
    try {
      requestedPath = await realpath(resolve(input.repositoryPath))
    } catch (error) {
      return workspaceError('INVALID_REPOSITORY', 'repository path does not exist', {
        cause: error,
      })
    }
    let repositoryPath: string
    let repositoryCommonDir: string
    let bare: boolean
    let objectFormat: 'sha1' | 'sha256'
    let status: string
    let operation: ObservedGitOperation
    let symbolic: GitCommandResult
    try {
      const inside = oneLine((await this.#git([
        '-C',
        requestedPath,
        'rev-parse',
        '--is-inside-work-tree',
      ])).stdout)
      if (inside !== 'true') {
        return workspaceError('INVALID_REPOSITORY', 'path is not a Git worktree')
      }
      repositoryPath = await realpath(oneLine((await this.#git([
        '-C',
        requestedPath,
        'rev-parse',
        '--show-toplevel',
      ])).stdout))
      if (repositoryPath !== requestedPath) {
        return workspaceError('INVALID_REPOSITORY', 'repository path must name the worktree root')
      }
      bare = oneLine((await this.#git([
        '-C',
        repositoryPath,
        'rev-parse',
        '--is-bare-repository',
      ])).stdout) === 'true'
      const format = oneLine((await this.#git([
        '-C',
        repositoryPath,
        'rev-parse',
        '--show-object-format',
      ])).stdout)
      if (format !== 'sha1' && format !== 'sha256') {
        return workspaceError('INVALID_REPOSITORY', 'Git object format is unsupported')
      }
      objectFormat = format
      repositoryCommonDir = await this.#repositoryCommonDir(repositoryPath)
      status = (await this.#git([
        '-C',
        repositoryPath,
        'status',
        '--porcelain=v2',
        '--untracked-files=normal',
      ])).stdout.trim()
      operation = await this.#activeOperation(repositoryPath)
      symbolic = await this.#git([
        '-C',
        repositoryPath,
        'symbolic-ref',
        '--quiet',
        '--short',
        'HEAD',
      ], [0, 1])
    } catch (error) {
      if (
        error instanceof StrongFlowGitWorkspaceError
        && error.code === 'INVALID_REPOSITORY'
      ) throw error
      if (isBoundedCommandFailure(error)) throw error
      return workspaceError('INVALID_REPOSITORY', 'path is not a usable Git worktree', {
        cause: error,
      })
    }
    const headKind = symbolic.exitCode === 0 ? 'branch' : 'detached'
    const branchName = symbolic.exitCode === 0 ? oneLine(symbolic.stdout) : undefined
    const requestedRevision = revisionText(input.revision)
    let commitId: string
    let treeId: string
    try {
      commitId = oneLine((await this.#git([
        '-C',
        repositoryPath,
        'rev-parse',
        '--verify',
        '--end-of-options',
        `${requestedRevision}^{commit}`,
      ])).stdout)
      treeId = oneLine((await this.#git([
        '-C',
        repositoryPath,
        'rev-parse',
        '--verify',
        '--end-of-options',
        `${commitId}^{tree}`,
      ])).stdout)
    } catch (error) {
      if (isBoundedCommandFailure(error)) throw error
      if (error instanceof StrongFlowGitWorkspaceError) {
        return workspaceError('INVALID_REVISION', 'Git revision does not resolve to a commit', {
          cause: error,
        })
      }
      throw error
    }
    const headCommit = await this.#git([
      '-C',
      repositoryPath,
      'rev-parse',
      '--verify',
      '--end-of-options',
      'HEAD^{commit}',
    ], [0, 128])
    const headTree = headCommit.exitCode === 0
      ? await this.#git([
        '-C',
        repositoryPath,
        'rev-parse',
        '--verify',
        '--end-of-options',
        'HEAD^{tree}',
      ])
      : undefined
    const conflict = status.split(/\r?\n/u).some(line => line.startsWith('u '))
    const observed: ObservedGitSourceState = {
      repositoryPath,
      objectFormat,
      headKind: headCommit.exitCode === 0
        ? input.revision === undefined ? headKind : 'detached'
        : 'unborn',
      ...(input.revision === undefined && branchName !== undefined ? { branchName } : {}),
      ...(headCommit.exitCode === 0 ? { commitId, treeId } : {}),
      indexState: conflict ? 'conflicted' : status.length === 0 ? 'clean' : 'dirty',
      worktreeState: status.length === 0 ? 'clean' : 'dirty',
      operation,
      bare,
    }
    let admitted: AdmittedStrongFlowSource
    try {
      admitted = admitStrongFlowSource(observed)
    } catch (error) {
      return workspaceError('INVALID_REPOSITORY', 'Git source state is not admissible', {
        cause: error,
      })
    }
    return Object.freeze({
      admitted,
      repositoryCommonDir,
      requestedRevision,
      status,
      headCommitId: headCommit.exitCode === 0 ? oneLine(headCommit.stdout) : '',
      headTreeId: headTree === undefined ? '' : oneLine(headTree.stdout),
    })
  }

  async #activeOperation(repositoryPath: string): Promise<ObservedGitOperation> {
    const checks: readonly [ObservedGitOperation, string][] = [
      ['merge', 'MERGE_HEAD'],
      ['rebase', 'rebase-merge'],
      ['rebase', 'rebase-apply'],
      ['cherry-pick', 'CHERRY_PICK_HEAD'],
      ['revert', 'REVERT_HEAD'],
      ['bisect', 'BISECT_LOG'],
    ]
    for (const [operation, marker] of checks) {
      const markerPath = oneLine((await this.#git([
        '-C',
        repositoryPath,
        'rev-parse',
        '--git-path',
        marker,
      ])).stdout)
      if (await pathKind(resolve(repositoryPath, markerPath)) !== 'missing') return operation
    }
    return 'none'
  }

  async #ensureManagedParent(): Promise<string> {
    const parent = join(this.home, 'strongflow-workspaces')
    try {
      await mkdir(parent, { recursive: true })
      if (await pathKind(parent) !== 'directory') {
        return workspaceError(
          'WORKSPACE_NOT_OWNED',
          'managed workspace parent was replaced by a non-directory entry',
        )
      }
      return realpath(parent)
    } catch (error) {
      if (error instanceof StrongFlowGitWorkspaceError) throw error
      return workspaceError('WORKSPACE_IO_ERROR', 'managed workspace parent is unavailable', {
        cause: error,
      })
    }
  }

  async #repositoryCommonDir(repositoryPath: string): Promise<string> {
    const value = oneLine((await this.#git([
      '-C',
      repositoryPath,
      'rev-parse',
      '--git-common-dir',
    ])).stdout)
    return realpath(resolve(repositoryPath, value))
  }

  async #assertSourceSnapshot(handle: StrongFlowGitWorkspaceHandle): Promise<void> {
    const source = await this.#worktreeStatus(
      handle.layout.sourcePath,
      handle.manifest.repositoryCommonDir,
    )
    if (
      !source.clean
      || source.commitId !== handle.manifest.sourceSnapshot.commitId
      || source.treeId !== handle.manifest.sourceSnapshot.treeId
    ) {
      return workspaceError(
        'SOURCE_SNAPSHOT_MUTATED',
        'read-only source snapshot changed after workspace creation',
      )
    }
  }

  async #materializeCandidate(
    handle: StrongFlowGitWorkspaceHandle,
    scope: StrongFlowCandidateScope,
  ): Promise<StrongFlowCandidateRecord> {
    const candidatePath = handle.layout.candidatePath
    await this.#assertWorktreeRegistration(
      candidatePath,
      handle.manifest.repositoryCommonDir,
    )
    const conflictPaths = await this.#git([
      '-C',
      candidatePath,
      'diff',
      '--name-only',
      '--diff-filter=U',
      '-z',
      '--',
    ])
    if (conflictPaths.stdoutBytes.length > 0) {
      return workspaceError('CANDIDATE_CONFLICT', 'candidate contains unresolved Git conflicts')
    }
    await this.#git(['-C', candidatePath, 'add', '--all', '--', '.'])
    await this.#assertCandidateIndexStable(candidatePath)
    const candidateTreeId = oneLine((await this.#git([
      '-C',
      candidatePath,
      'write-tree',
    ])).stdout)
    const diff = await this.#candidateDiff(
      candidatePath,
      handle.manifest.sourceSnapshot.commitId,
      'index',
    )
    const diffId = createStrongFlowGitDiffId(diff.stdoutBytes)
    const changedPaths = await this.#changedCandidatePaths(
      candidatePath,
      handle.manifest.sourceSnapshot.commitId,
      'index',
    )
    const outsideScope = changedPaths.filter(path => !pathIsInScope(path, scope))
    if (outsideScope.length > 0) {
      return workspaceError(
        'CANDIDATE_SCOPE_VIOLATION',
        `candidate changed ${outsideScope.length} path(s) outside its approved scope`,
      )
    }
    const candidateCommitId = candidateTreeId === handle.manifest.sourceSnapshot.treeId
      && diff.stdoutBytes.length === 0
      ? handle.manifest.sourceSnapshot.commitId
      : oneLine((await this.#git([
        '-C',
        candidatePath,
        '-c',
        'commit.gpgSign=false',
        'commit-tree',
        candidateTreeId,
        '-p',
        handle.manifest.sourceSnapshot.commitId,
        '-m',
        [
          'WinWinCode StrongFlow candidate',
          '',
          `Base: ${handle.manifest.sourceSnapshot.commitId}`,
          `Tree: ${candidateTreeId}`,
          `Diff: ${diffId}`,
        ].join('\n'),
      ], [0], {
        GIT_AUTHOR_NAME: 'WinWinCode',
        GIT_AUTHOR_EMAIL: 'candidate@winwincode.local',
        GIT_AUTHOR_DATE: '2000-01-01T00:00:00Z',
        GIT_COMMITTER_NAME: 'WinWinCode',
        GIT_COMMITTER_EMAIL: 'candidate@winwincode.local',
        GIT_COMMITTER_DATE: '2000-01-01T00:00:00Z',
      })).stdout)
    const candidate = createStrongFlowCandidateIdentity({
      source: handle.manifest.sourceSnapshot,
      candidateCommitId,
      candidateTreeId,
      diffId,
    })
    await this.#assertCandidateIndexStable(candidatePath, {
      treeId: candidate.candidateTreeId,
      diffId: candidate.diffId,
      baseCommitId: candidate.baseCommitId,
    })
    await this.#git(['-C', candidatePath, 'reset', '--hard', candidate.candidateCommitId])
    const status = await this.#worktreeStatus(
      candidatePath,
      handle.manifest.repositoryCommonDir,
    )
    if (
      !status.clean
      || status.commitId !== candidate.candidateCommitId
      || status.treeId !== candidate.candidateTreeId
    ) {
      return workspaceError(
        'CANDIDATE_CHANGED',
        'candidate changed while its immutable identity was being published',
      )
    }
    const committedDiff = await this.#candidateDiff(
      candidatePath,
      candidate.baseCommitId,
      candidate.candidateCommitId,
    )
    if (!sameBytes(diff.stdoutBytes, committedDiff.stdoutBytes)) {
      return workspaceError(
        'CANDIDATE_CHANGED',
        'materialized candidate diff does not match the staged candidate',
      )
    }
    const key = candidateRecordKey(candidate.candidateId)
    const record = immutable({
      schemaVersion: STRONGFLOW_CANDIDATE_SCHEMA_VERSION,
      jobId: handle.manifest.jobId,
      workspaceId: handle.manifest.workspaceId,
      sourceSnapshot: handle.manifest.sourceSnapshot,
      candidate,
      scope,
      changedPaths,
      diffByteLength: diff.stdoutBytes.length,
      diffFileName: `${key}.diff`,
    })
    await this.#publishCandidateRecord(handle, record, diff.stdoutBytes)
    return record
  }

  async #assertCandidateIndexStable(
    candidatePath: string,
    expected?: {
      readonly treeId: string
      readonly diffId: string
      readonly baseCommitId: string
    },
  ): Promise<void> {
    const unstaged = await this.#git([
      '-C',
      candidatePath,
      'diff',
      '--quiet',
      '--no-ext-diff',
      '--',
    ], [0, 1])
    const untracked = await this.#git([
      '-C',
      candidatePath,
      'ls-files',
      '--others',
      '--exclude-standard',
      '-z',
    ])
    const conflicts = await this.#git([
      '-C',
      candidatePath,
      'diff',
      '--name-only',
      '--diff-filter=U',
      '-z',
      '--',
    ])
    if (unstaged.exitCode !== 0 || untracked.stdoutBytes.length > 0) {
      return workspaceError('CANDIDATE_CHANGED', 'candidate changed during freeze')
    }
    if (conflicts.stdoutBytes.length > 0) {
      return workspaceError('CANDIDATE_CONFLICT', 'candidate contains unresolved Git conflicts')
    }
    if (expected === undefined) return
    const treeId = oneLine((await this.#git(['-C', candidatePath, 'write-tree'])).stdout)
    const diff = await this.#candidateDiff(candidatePath, expected.baseCommitId, 'index')
    if (
      treeId !== expected.treeId
      || createStrongFlowGitDiffId(diff.stdoutBytes) !== expected.diffId
    ) {
      return workspaceError('CANDIDATE_CHANGED', 'candidate index changed during freeze')
    }
  }

  async #candidateDiff(
    candidatePath: string,
    baseCommitId: string,
    target: 'index' | string,
  ): Promise<GitCommandResult> {
    return this.#git([
      '-C',
      candidatePath,
      'diff',
      ...(target === 'index' ? ['--cached'] : []),
      '--binary',
      '--full-index',
      '--no-color',
      '--no-ext-diff',
      '--no-textconv',
      '--no-renames',
      '--submodule=diff',
      '--src-prefix=a/',
      '--dst-prefix=b/',
      baseCommitId,
      ...(target === 'index' ? [] : [target]),
      '--',
    ])
  }

  async #changedCandidatePaths(
    candidatePath: string,
    baseCommitId: string,
    target: 'index' | string,
  ): Promise<readonly string[]> {
    const result = await this.#git([
      '-C',
      candidatePath,
      'diff',
      ...(target === 'index' ? ['--cached'] : []),
      '--name-only',
      '-z',
      '--no-renames',
      baseCommitId,
      ...(target === 'index' ? [] : [target]),
      '--',
    ])
    return nullDelimitedPaths(result.stdoutBytes)
  }

  #candidateMetadataPaths(
    layout: StrongFlowWorkspaceLayout,
    candidateId: StrongFlowCandidateIdentity['candidateId'],
  ): {
    readonly directory: string
    readonly recordPath: string
    readonly diffPath: string
    readonly currentPath: string
  } {
    const directory = join(layout.metadataPath, 'candidates')
    const key = candidateRecordKey(candidateId)
    return Object.freeze({
      directory,
      recordPath: join(directory, `${key}.json`),
      diffPath: join(directory, `${key}.diff`),
      currentPath: join(layout.metadataPath, 'current-candidate.json'),
    })
  }

  async #publishCandidateRecord(
    handle: StrongFlowGitWorkspaceHandle,
    record: StrongFlowCandidateRecord,
    diff: Uint8Array,
  ): Promise<void> {
    const paths = this.#candidateMetadataPaths(
      handle.layout,
      record.candidate.candidateId,
    )
    await mkdir(paths.directory, { recursive: true })
    if (await pathKind(paths.directory) !== 'directory') {
      return workspaceError('WORKSPACE_CORRUPT', 'candidate metadata directory was replaced')
    }
    try {
      await writeNewBytes(paths.diffPath, diff)
    } catch (error) {
      if (errorCode(error) !== 'EEXIST') {
        return workspaceError('WORKSPACE_IO_ERROR', 'candidate diff could not be published', {
          cause: error,
        })
      }
      const existing = await readFile(paths.diffPath)
      if (!sameBytes(existing, diff)) {
        return workspaceError('WORKSPACE_CORRUPT', 'candidate diff identity was reused')
      }
    }
    try {
      await writeNewJson(paths.recordPath, record)
    } catch (error) {
      if (errorCode(error) !== 'EEXIST') {
        return workspaceError('WORKSPACE_IO_ERROR', 'candidate record could not be published', {
          cause: error,
        })
      }
      const existing = await this.#readCandidateRecord(handle, record.candidate.candidateId)
      if (JSON.stringify(existing) !== JSON.stringify(record)) {
        return workspaceError('WORKSPACE_CORRUPT', 'candidate identity record was reused')
      }
    }
    await replaceJson(paths.currentPath, record)
  }

  async #readCurrentCandidate(
    handle: StrongFlowGitWorkspaceHandle,
  ): Promise<StrongFlowCandidateRecord> {
    const path = join(handle.layout.metadataPath, 'current-candidate.json')
    if (await pathKind(path) !== 'file') {
      return workspaceError('CANDIDATE_NOT_FROZEN', 'workspace has no frozen candidate')
    }
    const value = await this.#readJson(path, 'current candidate record')
    const candidateId = isRecord(value) && isRecord(value.candidate)
      ? parseCandidateId(value.candidate.candidateId, 'WORKSPACE_CORRUPT')
      : workspaceError('WORKSPACE_CORRUPT', 'current candidate identity is missing')
    const current = this.#validateCandidateRecord(handle, value)
    const durable = await this.#readCandidateRecord(handle, candidateId)
    if (JSON.stringify(current) !== JSON.stringify(durable)) {
      return workspaceError('WORKSPACE_CORRUPT', 'current candidate record is not durable')
    }
    return durable
  }

  async #readCandidateRecord(
    handle: StrongFlowGitWorkspaceHandle,
    candidateId: StrongFlowCandidateIdentity['candidateId'],
  ): Promise<StrongFlowCandidateRecord> {
    const paths = this.#candidateMetadataPaths(handle.layout, candidateId)
    if (await pathKind(paths.recordPath) !== 'file' || await pathKind(paths.diffPath) !== 'file') {
      return workspaceError('CANDIDATE_NOT_FROZEN', 'candidate record or exact diff is missing')
    }
    const value = await this.#readJson(paths.recordPath, 'candidate record')
    const record = this.#validateCandidateRecord(handle, value)
    if (record.candidate.candidateId !== candidateId) {
      return workspaceError('WORKSPACE_CORRUPT', 'candidate record path has another identity')
    }
    const diff = await readFile(paths.diffPath)
    if (
      diff.length !== record.diffByteLength
      || createStrongFlowGitDiffId(diff) !== record.candidate.diffId
    ) return workspaceError('WORKSPACE_CORRUPT', 'candidate exact diff does not match its record')
    return record
  }

  #validateCandidateRecord(
    handle: StrongFlowGitWorkspaceHandle,
    value: unknown,
  ): StrongFlowCandidateRecord {
    if (!isRecord(value)) {
      return workspaceError('WORKSPACE_CORRUPT', 'candidate record must be an object')
    }
    exactKeys(value, [
      'schemaVersion',
      'jobId',
      'workspaceId',
      'sourceSnapshot',
      'candidate',
      'scope',
      'changedPaths',
      'diffByteLength',
      'diffFileName',
    ], [], 'candidate record')
    if (
      value.schemaVersion !== STRONGFLOW_CANDIDATE_SCHEMA_VERSION
      || value.jobId !== handle.manifest.jobId
      || value.workspaceId !== handle.manifest.workspaceId
      || !isRecord(value.sourceSnapshot)
      || !isRecord(value.candidate)
      || !Array.isArray(value.changedPaths)
      || typeof value.diffFileName !== 'string'
    ) return workspaceError('WORKSPACE_CORRUPT', 'candidate record identity is invalid')
    exactKeys(value.candidate, [
      'candidateId',
      'sourceSnapshotId',
      'baseCommitId',
      'baseTreeId',
      'candidateCommitId',
      'candidateTreeId',
      'diffId',
    ], [], 'candidate content identity')
    let sourceSnapshot: StrongFlowSourceSnapshotIdentity
    let candidate: StrongFlowCandidateIdentity
    try {
      sourceSnapshot = createStrongFlowWorkspaceLayout({
        home: this.home,
        jobId: handle.manifest.jobId,
        sourceSnapshot: value.sourceSnapshot as unknown as StrongFlowSourceSnapshotIdentity,
      }).sourceSnapshot
      candidate = createStrongFlowCandidateIdentity({
        source: sourceSnapshot,
        candidateCommitId: value.candidate.candidateCommitId as string,
        candidateTreeId: value.candidate.candidateTreeId as string,
        diffId: value.candidate.diffId as string,
      })
    } catch (error) {
      return workspaceError('WORKSPACE_CORRUPT', 'candidate content identity is invalid', {
        cause: error,
      })
    }
    if (
      JSON.stringify(sourceSnapshot) !== JSON.stringify(handle.manifest.sourceSnapshot)
      || value.candidate.candidateId !== candidate.candidateId
      || value.candidate.sourceSnapshotId !== candidate.sourceSnapshotId
      || value.candidate.baseCommitId !== candidate.baseCommitId
      || value.candidate.baseTreeId !== candidate.baseTreeId
    ) return workspaceError('WORKSPACE_CORRUPT', 'candidate record does not match its source')
    const scope = candidateScope(value.scope, 'WORKSPACE_CORRUPT')
    const changedPaths = value.changedPaths.map((path, index) => portableCandidatePath(
      path,
      `recorded candidate path ${index}`,
      'WORKSPACE_CORRUPT',
    ))
    const sortedChangedPaths = [...changedPaths].sort()
    if (
      changedPaths.some((path, index) => path !== sortedChangedPaths[index])
      || new Set(changedPaths).size !== changedPaths.length
      || changedPaths.some(path => !pathIsInScope(path, scope))
    ) return workspaceError('WORKSPACE_CORRUPT', 'candidate changed paths are not canonical')
    const diffByteLength = nonNegativeInteger(
      value.diffByteLength,
      'candidate diff byte length',
      'WORKSPACE_CORRUPT',
    )
    const expectedDiffFileName = `${candidateRecordKey(candidate.candidateId)}.diff`
    if (value.diffFileName !== expectedDiffFileName) {
      return workspaceError('WORKSPACE_CORRUPT', 'candidate diff file name is not canonical')
    }
    return immutable({
      schemaVersion: STRONGFLOW_CANDIDATE_SCHEMA_VERSION,
      jobId: handle.manifest.jobId,
      workspaceId: handle.manifest.workspaceId,
      sourceSnapshot,
      candidate,
      scope,
      changedPaths: Object.freeze(changedPaths),
      diffByteLength,
      diffFileName: expectedDiffFileName,
    })
  }

  async #assertFrozenCandidate(
    handle: StrongFlowGitWorkspaceHandle,
    record: StrongFlowCandidateRecord,
  ): Promise<void> {
    const current = await this.#readCurrentCandidate(handle)
    if (current.candidate.candidateId !== record.candidate.candidateId) {
      return workspaceError('CANDIDATE_CHANGED', 'candidate freeze is no longer current')
    }
    const status = await this.#worktreeStatus(
      handle.layout.candidatePath,
      handle.manifest.repositoryCommonDir,
    )
    if (
      !status.clean
      || status.commitId !== record.candidate.candidateCommitId
      || status.treeId !== record.candidate.candidateTreeId
    ) return workspaceError('CANDIDATE_CHANGED', 'candidate changed after it was frozen')
    const paths = this.#candidateMetadataPaths(handle.layout, record.candidate.candidateId)
    const recordedDiff = await readFile(paths.diffPath)
    const actualDiff = await this.#candidateDiff(
      handle.layout.candidatePath,
      record.candidate.baseCommitId,
      record.candidate.candidateCommitId,
    )
    if (!sameBytes(recordedDiff, actualDiff.stdoutBytes)) {
      return workspaceError('CANDIDATE_CHANGED', 'candidate exact diff changed after freeze')
    }
  }

  #verificationRequest(
    input: CreateStrongFlowVerificationWorkspaceInput,
  ): VerificationWorkspaceRequest {
    if (!isRecord(input)) {
      return workspaceError(
        'INVALID_MANAGER_OPTIONS',
        'verification workspace input must be an object',
      )
    }
    exactKeys(
      input,
      ['candidateId', 'roleId', 'stageRunId'],
      [],
      'verification workspace input',
      'INVALID_MANAGER_OPTIONS',
    )
    if (
      input.roleId !== 'reviewer'
      && input.roleId !== 'verifier'
      && input.roleId !== 'adversarial-verifier'
    ) {
      return workspaceError(
        'INVALID_MANAGER_OPTIONS',
        'verification workspace role is not read-oriented',
      )
    }
    return Object.freeze({
      candidateId: parseCandidateId(input.candidateId, 'INVALID_MANAGER_OPTIONS'),
      roleId: input.roleId,
      stageRunId: parseStageRunId(input.stageRunId, 'INVALID_MANAGER_OPTIONS'),
    })
  }

  #verificationManifest(
    handle: StrongFlowGitWorkspaceHandle,
    candidateRecord: StrongFlowCandidateRecord,
    request: VerificationWorkspaceRequest,
  ): StrongFlowVerificationWorkspaceManifest {
    const verificationSnapshot = createStrongFlowVerificationSnapshotIdentity(
      candidateRecord.candidate,
    )
    const assignment = strongFlowRoleWorkspace({
      layout: handle.layout,
      roleId: request.roleId,
      stageRunId: request.stageRunId,
      candidate: candidateRecord.candidate,
      verificationSnapshot,
    })
    if (assignment.temporaryOutputPath === undefined) {
      return workspaceError('WORKSPACE_CORRUPT', 'verification output path is missing')
    }
    return immutable({
      schemaVersion: STRONGFLOW_CANDIDATE_SCHEMA_VERSION,
      jobId: handle.manifest.jobId,
      workspaceId: handle.manifest.workspaceId,
      candidate: candidateRecord.candidate,
      verificationSnapshot,
      roleId: request.roleId,
      stageRunId: request.stageRunId,
      path: assignment.path,
      temporaryOutputPath: assignment.temporaryOutputPath,
    })
  }

  #verificationOwner(
    manifest: StrongFlowVerificationWorkspaceManifest,
  ): VerificationWorkspaceOwnerRecord {
    return immutable({
      schemaVersion: STRONGFLOW_CANDIDATE_SCHEMA_VERSION,
      ownerMagic: 'winwincode-strongflow-verification-workspace' as const,
      jobId: manifest.jobId,
      workspaceId: manifest.workspaceId,
      candidateId: manifest.candidate.candidateId,
      roleId: manifest.roleId,
      stageRunId: manifest.stageRunId,
      path: manifest.path,
      temporaryOutputPath: manifest.temporaryOutputPath,
    })
  }

  #verificationMetadataPaths(
    layout: StrongFlowWorkspaceLayout,
    request: VerificationWorkspaceRequest,
  ): {
    readonly directory: string
    readonly ownerPath: string
    readonly manifestPath: string
    readonly failurePath: string
  } {
    const directory = join(layout.metadataPath, 'verification')
    const key = candidateRecordKey([
      request.candidateId,
      request.roleId,
      request.stageRunId,
    ].join('\0'))
    return Object.freeze({
      directory,
      ownerPath: join(directory, `${key}.owner.json`),
      manifestPath: join(directory, `${key}.manifest.json`),
      failurePath: join(directory, `${key}.failure.json`),
    })
  }

  async #ensureVerificationParents(layout: StrongFlowWorkspaceLayout): Promise<void> {
    const paths = [
      layout.verificationRoot,
      join(layout.root, 'verification-output'),
      join(layout.metadataPath, 'verification'),
    ]
    for (const path of paths) {
      await mkdir(path, { recursive: true })
      if (await pathKind(path) !== 'directory') {
        return workspaceError(
          'VERIFICATION_SNAPSHOT_MISMATCH',
          'verification workspace parent was replaced',
        )
      }
    }
  }

  async #openVerificationWorkspaceLocked(
    handle: StrongFlowGitWorkspaceHandle,
    candidateRecord: StrongFlowCandidateRecord,
    request: VerificationWorkspaceRequest,
    manifest: StrongFlowVerificationWorkspaceManifest,
  ): Promise<StrongFlowVerificationWorkspaceHandle> {
    const metadata = this.#verificationMetadataPaths(handle.layout, request)
    await this.#assertVerificationOwner(metadata.ownerPath, this.#verificationOwner(manifest))
    if (await pathKind(metadata.manifestPath) !== 'file') {
      return workspaceError(
        'VERIFICATION_SNAPSHOT_NOT_FOUND',
        'completed verification workspace manifest is missing',
      )
    }
    const storedManifest = await this.#readJson(
      metadata.manifestPath,
      'verification workspace manifest',
    )
    if (JSON.stringify(storedManifest) !== JSON.stringify(manifest)) {
      return workspaceError(
        'VERIFICATION_SNAPSHOT_MISMATCH',
        'verification workspace manifest does not match its candidate',
      )
    }
    const status = await this.#worktreeStatus(
      manifest.path,
      handle.manifest.repositoryCommonDir,
    )
    if (
      status.commitId !== manifest.verificationSnapshot.candidateCommitId
      || status.treeId !== manifest.verificationSnapshot.candidateTreeId
    ) {
      return workspaceError(
        'VERIFICATION_SNAPSHOT_MISMATCH',
        'verification workspace no longer points at the frozen candidate',
      )
    }
    if (await pathKind(manifest.temporaryOutputPath) !== 'directory') {
      return workspaceError(
        'VERIFICATION_SNAPSHOT_MISMATCH',
        'verification output directory is missing or replaced',
      )
    }
    return Object.freeze({ manifest, candidateRecord })
  }

  async #assertVerificationOwner(
    path: string,
    expected: VerificationWorkspaceOwnerRecord,
  ): Promise<void> {
    if (await pathKind(path) !== 'file') {
      return workspaceError(
        'VERIFICATION_SNAPSHOT_NOT_FOUND',
        'verification owner marker is missing or replaced',
      )
    }
    const value = await this.#readJson(path, 'verification owner marker')
    if (JSON.stringify(value) !== JSON.stringify(expected)) {
      return workspaceError(
        'VERIFICATION_SNAPSHOT_MISMATCH',
        'verification owner marker does not match the requested workspace',
      )
    }
  }

  async #recordVerificationFailure(path: string, error: unknown): Promise<void> {
    try {
      if (await pathKind(path) !== 'missing') return
      await writeNewJson(path, {
        schemaVersion: STRONGFLOW_CANDIDATE_SCHEMA_VERSION,
        occurredAtMillis: this.#now(),
        code: error instanceof StrongFlowGitWorkspaceError ? error.code : 'WORKSPACE_IO_ERROR',
        message: error instanceof StrongFlowGitWorkspaceError
          ? error.message
          : 'verification workspace creation failed unexpectedly',
      })
    } catch {
      // The owner marker retains the cleanup identity if detail publication fails.
    }
  }

  async #readJson(path: string, label: string): Promise<unknown> {
    try {
      return JSON.parse(await readFile(path, 'utf8')) as unknown
    } catch (error) {
      return workspaceError('WORKSPACE_CORRUPT', `${label} is unreadable`, { cause: error })
    }
  }

  #ownerRecord(
    jobId: JobIdentifier,
    observation: GitSourceObservation,
    _layout: StrongFlowWorkspaceLayout,
  ): WorkspaceOwnerRecord {
    const now = this.#now()
    let ownerId: string
    try {
      ownerId = portableText(this.#ownerIdFactory(), 'ownerId', 200)
    } catch (error) {
      if (error instanceof StrongFlowGitWorkspaceError) throw error
      return workspaceError('INVALID_MANAGER_OPTIONS', 'ownerIdFactory failed', { cause: error })
    }
    return Object.freeze({
      schemaVersion: STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION,
      ownerMagic: OWNER_MAGIC,
      ownerId,
      createdAtMillis: now,
      jobId,
      repositoryPath: observation.admitted.repositoryPath,
      repositoryCommonDir: observation.repositoryCommonDir,
      requestedRevision: observation.requestedRevision,
      sourceSnapshot: observation.admitted.identity,
    })
  }

  #manifest(
    owner: WorkspaceOwnerRecord,
    layout: StrongFlowWorkspaceLayout,
  ): StrongFlowGitWorkspaceManifest {
    return immutable({
      ...owner,
      workspaceId: layout.workspaceId,
      root: layout.root,
      sourcePath: layout.sourcePath,
      candidatePath: layout.candidatePath,
      verificationRoot: layout.verificationRoot,
      metadataPath: layout.metadataPath,
    })
  }

  async #verifyCreatedWorktree(
    path: string,
    expected: StrongFlowSourceSnapshotIdentity,
    expectedCommonDir: string,
    requireClean: boolean,
  ): Promise<void> {
    const status = await this.#worktreeStatus(path, expectedCommonDir)
    if (
      status.commitId !== expected.commitId
      || status.treeId !== expected.treeId
      || (requireClean && !status.clean)
    ) workspaceError('SOURCE_CHANGED', 'created worktree does not match the resolved source')
  }

  async #assertOriginalUnchanged(observation: GitSourceObservation): Promise<void> {
    const currentStatus = (await this.#git([
      '-C',
      observation.admitted.repositoryPath,
      'status',
      '--porcelain=v2',
      '--untracked-files=normal',
    ])).stdout.trim()
    const headCommit = oneLine((await this.#git([
      '-C',
      observation.admitted.repositoryPath,
      'rev-parse',
      'HEAD^{commit}',
    ])).stdout)
    const headTree = oneLine((await this.#git([
      '-C',
      observation.admitted.repositoryPath,
      'rev-parse',
      'HEAD^{tree}',
    ])).stdout)
    if (
      currentStatus !== observation.status
      || headCommit !== observation.headCommitId
      || headTree !== observation.headTreeId
    ) workspaceError('SOURCE_CHANGED', 'source checkout changed during workspace creation')
  }

  async #worktreeStatus(path: string, expectedCommonDir: string): Promise<{
    readonly commitId: string
    readonly treeId: string
    readonly clean: boolean
    readonly status: string
  }> {
    await this.#assertWorktreeRegistration(path, expectedCommonDir)
    const commitId = oneLine((await this.#git(['-C', path, 'rev-parse', 'HEAD^{commit}'])).stdout)
    const treeId = oneLine((await this.#git(['-C', path, 'rev-parse', 'HEAD^{tree}'])).stdout)
    const status = (await this.#git([
      '-C',
      path,
      'status',
      '--porcelain=v2',
      '--untracked-files=normal',
    ])).stdout.trim()
    return Object.freeze({ commitId, treeId, clean: status.length === 0, status })
  }

  async #assertWorktreeRegistration(path: string, expectedCommonDir: string): Promise<void> {
    if (await pathKind(path) !== 'directory') {
      return workspaceError('WORKSPACE_NOT_OWNED', 'managed worktree is missing or replaced')
    }
    const actualPath = await realpath(path)
    const top = await realpath(oneLine((await this.#git([
      '-C',
      path,
      'rev-parse',
      '--show-toplevel',
    ])).stdout))
    if (top !== actualPath) {
      return workspaceError('WORKSPACE_NOT_OWNED', 'worktree path changed identity')
    }
    const commonDirText = oneLine((await this.#git([
      '-C',
      path,
      'rev-parse',
      '--git-common-dir',
    ])).stdout)
    const commonDir = await realpath(resolve(path, commonDirText))
    if (commonDir !== expectedCommonDir) {
      return workspaceError('WORKSPACE_NOT_OWNED', 'worktree belongs to a different repository')
    }
  }

  async #readOwner(root: string, expectedJobId: JobIdentifier): Promise<WorkspaceOwnerRecord> {
    const metadataPath = join(root, 'metadata')
    const ownerPath = join(metadataPath, 'owner.json')
    if (await pathKind(root) !== 'directory'
      || await pathKind(metadataPath) !== 'directory'
      || await pathKind(ownerPath) !== 'file') {
      return workspaceError('WORKSPACE_NOT_OWNED', 'workspace owner marker is missing or replaced')
    }
    let value: unknown
    try {
      value = JSON.parse(await readFile(ownerPath, 'utf8')) as unknown
    } catch (error) {
      return workspaceError('WORKSPACE_CORRUPT', 'workspace owner marker is unreadable', {
        cause: error,
      })
    }
    if (!isRecord(value)) {
      return workspaceError('WORKSPACE_CORRUPT', 'workspace owner marker must be an object')
    }
    exactKeys(value, [
      'schemaVersion',
      'ownerMagic',
      'ownerId',
      'createdAtMillis',
      'jobId',
      'repositoryPath',
      'repositoryCommonDir',
      'requestedRevision',
      'sourceSnapshot',
    ], [], 'workspace owner marker')
    if (
      value.schemaVersion !== STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION
      || value.ownerMagic !== OWNER_MAGIC
      || value.jobId !== expectedJobId
      || typeof value.ownerId !== 'string'
      || typeof value.createdAtMillis !== 'number'
      || typeof value.repositoryPath !== 'string'
      || typeof value.repositoryCommonDir !== 'string'
      || typeof value.requestedRevision !== 'string'
      || !isRecord(value.sourceSnapshot)
    ) return workspaceError('WORKSPACE_NOT_OWNED', 'workspace owner marker identity is invalid')
    const ownerId = portableText(
      value.ownerId,
      'workspace owner id',
      200,
      'WORKSPACE_NOT_OWNED',
    )
    const createdAtMillis = nonNegativeInteger(
      value.createdAtMillis,
      'workspace creation time',
      'WORKSPACE_NOT_OWNED',
    )
    if (
      !isAbsolute(value.repositoryPath)
      || resolve(value.repositoryPath) !== value.repositoryPath
      || !isAbsolute(value.repositoryCommonDir)
      || resolve(value.repositoryCommonDir) !== value.repositoryCommonDir
    ) return workspaceError('WORKSPACE_NOT_OWNED', 'workspace repository paths are invalid')
    const requestedRevision = portableText(
      value.requestedRevision,
      'workspace requested revision',
      500,
      'WORKSPACE_NOT_OWNED',
    )
    let sourceSnapshot: StrongFlowSourceSnapshotIdentity
    try {
      sourceSnapshot = createStrongFlowWorkspaceLayout({
        home: this.home,
        jobId: expectedJobId,
        sourceSnapshot: value.sourceSnapshot as unknown as StrongFlowSourceSnapshotIdentity,
      }).sourceSnapshot
    } catch (error) {
      return workspaceError('WORKSPACE_NOT_OWNED', 'workspace source identity is invalid', {
        cause: error,
      })
    }
    return immutable({
      schemaVersion: STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION,
      ownerMagic: OWNER_MAGIC,
      ownerId,
      createdAtMillis,
      jobId: expectedJobId,
      repositoryPath: value.repositoryPath,
      repositoryCommonDir: value.repositoryCommonDir,
      requestedRevision,
      sourceSnapshot,
    })
  }

  async #assertOwnedRoot(
    root: string,
    layout: StrongFlowWorkspaceLayout,
    owner: WorkspaceOwnerRecord,
  ): Promise<void> {
    try {
      const parent = dirname(root)
      if (
        root !== layout.root
        || parent !== join(this.home, 'strongflow-workspaces')
        || await pathKind(parent) !== 'directory'
        || await pathKind(root) !== 'directory'
      ) return workspaceError('WORKSPACE_NOT_OWNED', 'workspace root is not canonical')
      const realParent = await realpath(parent)
      const realRoot = await realpath(root)
      if (dirname(realRoot) !== realParent) {
        return workspaceError('WORKSPACE_NOT_OWNED', 'workspace root escapes its managed parent')
      }
      if (
        await pathKind(owner.repositoryPath) !== 'directory'
        || await realpath(owner.repositoryPath) !== owner.repositoryPath
        || await pathKind(owner.repositoryCommonDir) !== 'directory'
        || await realpath(owner.repositoryCommonDir) !== owner.repositoryCommonDir
      ) {
        return workspaceError('WORKSPACE_NOT_OWNED', 'source repository path changed identity')
      }
      if (await this.#repositoryCommonDir(owner.repositoryPath) !== owner.repositoryCommonDir) {
        return workspaceError('WORKSPACE_NOT_OWNED', 'source repository registration changed')
      }
    } catch (error) {
      if (error instanceof StrongFlowGitWorkspaceError) throw error
      return workspaceError('WORKSPACE_NOT_OWNED', 'workspace ownership could not be verified', {
        cause: error,
      })
    }
  }

  async #readManifest(
    layout: StrongFlowWorkspaceLayout,
    owner: WorkspaceOwnerRecord,
  ): Promise<StrongFlowGitWorkspaceManifest> {
    const path = join(layout.metadataPath, 'manifest.json')
    if (await pathKind(path) !== 'file') {
      return workspaceError('WORKSPACE_CORRUPT', 'completed workspace manifest is missing')
    }
    let value: unknown
    try {
      value = JSON.parse(await readFile(path, 'utf8')) as unknown
    } catch (error) {
      return workspaceError('WORKSPACE_CORRUPT', 'workspace manifest is unreadable', {
        cause: error,
      })
    }
    const expected = this.#manifest(owner, layout)
    if (JSON.stringify(value) !== JSON.stringify(expected)) {
      return workspaceError('WORKSPACE_CORRUPT', 'workspace manifest does not match its owner')
    }
    return expected
  }

  async #readWriter(
    handle: StrongFlowGitWorkspaceHandle,
    required: boolean,
  ): Promise<StrongFlowCandidateWriterLease | undefined> {
    const layout = handle.layout
    const path = join(layout.metadataPath, 'writer.json')
    if (await pathKind(path) === 'missing') {
      if (required) {
        return workspaceError('WORKSPACE_CORRUPT', 'active writer lease is missing')
      }
      return undefined
    }
    if (await pathKind(path) !== 'file') {
      return workspaceError('WORKSPACE_CORRUPT', 'writer lease path was replaced')
    }
    let value: unknown
    try {
      value = JSON.parse(await readFile(path, 'utf8')) as unknown
    } catch (error) {
      return workspaceError('WORKSPACE_CORRUPT', 'writer lease is unreadable', { cause: error })
    }
    if (!isRecord(value)) {
      return workspaceError('WORKSPACE_CORRUPT', 'writer lease must be an object')
    }
    exactKeys(value, [
      'leaseId',
      'jobId',
      'workspaceId',
      'roleId',
      'stageRunId',
      'attemptId',
      'acquiredAtMillis',
    ], [], 'writer lease')
    try {
      const lease = claimStrongFlowCandidateWriter(undefined, {
        leaseId: value.leaseId as string,
        jobId: value.jobId as string,
        workspaceId: value.workspaceId as string,
        roleId: value.roleId as StrongFlowRoleId,
        stageRunId: value.stageRunId as string,
        attemptId: value.attemptId as string,
        acquiredAtMillis: value.acquiredAtMillis as number,
      })
      if (
        lease.jobId !== handle.manifest.jobId
        || lease.workspaceId !== handle.manifest.workspaceId
      ) return workspaceError('WORKSPACE_CORRUPT', 'writer lease belongs to another workspace')
      return lease
    } catch (error) {
      if (error instanceof StrongFlowGitWorkspaceError) throw error
      return workspaceError('WORKSPACE_CORRUPT', 'writer lease is invalid', { cause: error })
    }
  }

  #operationLockRecord(
    layout: StrongFlowWorkspaceLayout,
  ): WorkspaceOperationLockRecord {
    return immutable({
      schemaVersion: STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION,
      ownerMagic: OPERATION_LOCK_MAGIC,
      ownerToken: randomUUID(),
      processId: process.pid,
      acquiredAtMillis: this.#now(),
      jobId: layout.jobId,
      workspaceId: layout.workspaceId,
    })
  }

  async #readOperationLock(
    layout: StrongFlowWorkspaceLayout,
    path: string,
  ): Promise<WorkspaceOperationLockRecord> {
    if (await pathKind(path) !== 'directory') {
      return workspaceError('WORKSPACE_CORRUPT', 'writer operation lock was replaced')
    }
    const value = await this.#readJson(join(path, 'owner.json'), 'writer operation lock')
    if (!isRecord(value)) {
      return workspaceError('WORKSPACE_CORRUPT', 'writer operation lock must be an object')
    }
    exactKeys(value, [
      'schemaVersion',
      'ownerMagic',
      'ownerToken',
      'processId',
      'acquiredAtMillis',
      'jobId',
      'workspaceId',
    ], [], 'writer operation lock')
    const ownerToken = portableText(
      value.ownerToken,
      'writer operation owner token',
      200,
      'WORKSPACE_CORRUPT',
    )
    const processId = nonNegativeInteger(
      value.processId,
      'writer operation process id',
      'WORKSPACE_CORRUPT',
    )
    const acquiredAtMillis = nonNegativeInteger(
      value.acquiredAtMillis,
      'writer operation acquisition time',
      'WORKSPACE_CORRUPT',
    )
    if (
      value.schemaVersion !== STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION
      || value.ownerMagic !== OPERATION_LOCK_MAGIC
      || processId < 1
      || value.jobId !== layout.jobId
      || value.workspaceId !== layout.workspaceId
    ) return workspaceError('WORKSPACE_CORRUPT', 'writer operation lock identity is invalid')
    return immutable({
      schemaVersion: STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION,
      ownerMagic: OPERATION_LOCK_MAGIC,
      ownerToken,
      processId,
      acquiredAtMillis,
      jobId: layout.jobId,
      workspaceId: layout.workspaceId,
    })
  }

  #operationProcessIsAlive(processId: number): boolean {
    try {
      process.kill(processId, 0)
      return true
    } catch (error) {
      if (errorCode(error) === 'ESRCH') return false
      if (errorCode(error) === 'EPERM') return true
      return workspaceError('WORKSPACE_IO_ERROR', 'writer process state could not be checked', {
        cause: error,
      })
    }
  }

  async #reconcileOperationLock(
    layout: StrongFlowWorkspaceLayout,
  ): Promise<WorkspaceOperationLockReconciliation> {
    const lockPath = join(layout.metadataPath, 'writer-operation.lock')
    for (;;) {
      const kind = await pathKind(lockPath)
      if (kind === 'missing') return Object.freeze({ state: 'none' as const })
      if (kind !== 'directory') {
        return workspaceError('WORKSPACE_CORRUPT', 'writer operation lock was replaced')
      }
      const owner = await this.#readOperationLock(layout, lockPath)
      if (this.#operationProcessIsAlive(owner.processId)) {
        return Object.freeze({
          state: 'active' as const,
          processId: owner.processId,
        })
      }
      const reclaimedPath = join(
        layout.metadataPath,
        `.writer-operation-${owner.ownerToken}.reclaimed`,
      )
      try {
        await rename(lockPath, reclaimedPath)
      } catch (error) {
        if (errorCode(error) === 'ENOENT') continue
        return workspaceError('WORKSPACE_IO_ERROR', 'abandoned writer lock could not be moved', {
          cause: error,
        })
      }
      const reclaimed = await this.#readOperationLock(layout, reclaimedPath)
      if (JSON.stringify(reclaimed) !== JSON.stringify(owner)) {
        return workspaceError('WORKSPACE_CORRUPT', 'moved writer operation lock changed identity')
      }
      await rm(reclaimedPath, { recursive: true, force: true })
      await syncDirectory(layout.metadataPath)
      return Object.freeze({
        state: 'reclaimed' as const,
        processId: owner.processId,
      })
    }
  }

  async #releaseOperationLock(
    layout: StrongFlowWorkspaceLayout,
    owner: WorkspaceOperationLockRecord,
  ): Promise<void> {
    if (await pathKind(layout.metadataPath) === 'missing') return
    const lockPath = join(layout.metadataPath, 'writer-operation.lock')
    const current = await this.#readOperationLock(layout, lockPath)
    if (JSON.stringify(current) !== JSON.stringify(owner)) {
      return workspaceError('WORKSPACE_CORRUPT', 'writer operation lock changed ownership')
    }
    const releasedPath = join(
      layout.metadataPath,
      `.writer-operation-${owner.ownerToken}.released`,
    )
    try {
      await rename(lockPath, releasedPath)
      const released = await this.#readOperationLock(layout, releasedPath)
      if (JSON.stringify(released) !== JSON.stringify(owner)) {
        return workspaceError('WORKSPACE_CORRUPT', 'released writer lock changed identity')
      }
      await rm(releasedPath, { recursive: true, force: true })
      await syncDirectory(layout.metadataPath)
    } catch (error) {
      if (error instanceof StrongFlowGitWorkspaceError) throw error
      return workspaceError('WORKSPACE_IO_ERROR', 'writer operation lock could not be released', {
        cause: error,
      })
    }
  }

  async #withWriterOperationLock<Value>(
    layout: StrongFlowWorkspaceLayout,
    operation: () => Promise<Value>,
  ): Promise<Value> {
    const owner = this.#operationLockRecord(layout)
    const lockPath = join(layout.metadataPath, 'writer-operation.lock')
    const pendingPath = join(
      layout.metadataPath,
      `.writer-operation-${owner.ownerToken}.pending`,
    )
    const deadline = Date.now() + this.#commandTimeoutMillis
    let acquired = false
    try {
      await mkdir(pendingPath)
      await writeNewJson(join(pendingPath, 'owner.json'), owner)
      await syncDirectory(pendingPath)
      for (;;) {
        const currentKind = await pathKind(lockPath)
        if (currentKind === 'directory') {
          try {
            await this.#readOperationLock(layout, lockPath)
          } catch (lockError) {
            if (await pathKind(lockPath) === 'missing') continue
            throw lockError
          }
          if (Date.now() >= deadline) {
            return workspaceError(
              'WRITER_OPERATION_TIMEOUT',
              'another writer operation did not finish before the deadline',
            )
          }
          await new Promise(resolvePromise => setTimeout(resolvePromise, 5))
          continue
        }
        if (currentKind !== 'missing') {
          return workspaceError('WORKSPACE_CORRUPT', 'writer operation lock was replaced')
        }
        try {
          await rename(pendingPath, lockPath)
        } catch (error) {
          const kind = await pathKind(lockPath)
          if (kind === 'directory') continue
          if (kind !== 'missing') {
            return workspaceError('WORKSPACE_CORRUPT', 'writer operation lock was replaced')
          }
          return workspaceError(
            'WORKSPACE_IO_ERROR',
            'writer operation lock could not be published',
            { cause: error },
          )
        }
        const published = await this.#readOperationLock(layout, lockPath)
        if (JSON.stringify(published) !== JSON.stringify(owner)) {
          return workspaceError('WORKSPACE_CORRUPT', 'writer operation lock changed identity')
        }
        await syncDirectory(layout.metadataPath)
        acquired = true
        break
      }
      try {
        return await operation()
      } finally {
        await this.#releaseOperationLock(layout, owner)
      }
    } catch (error) {
      if (acquired) throw error
      if (error instanceof StrongFlowGitWorkspaceError) throw error
      return workspaceError('WORKSPACE_IO_ERROR', 'writer operation lock is unavailable', {
        cause: error,
      })
    } finally {
      if (!acquired) await rm(pendingPath, { recursive: true, force: true })
    }
  }

  async #recordFailure(layout: StrongFlowWorkspaceLayout, error: unknown): Promise<void> {
    try {
      const path = join(layout.metadataPath, 'failure.json')
      if (await pathKind(path) !== 'missing') return
      await writeNewJson(path, {
        schemaVersion: STRONGFLOW_GIT_WORKSPACE_SCHEMA_VERSION,
        occurredAtMillis: this.#now(),
        code: error instanceof StrongFlowGitWorkspaceError ? error.code : 'WORKSPACE_IO_ERROR',
        message: error instanceof StrongFlowGitWorkspaceError
          ? error.message
          : 'workspace creation failed unexpectedly',
      })
    } catch {
      // The owned root remains the diagnostic marker even if the detail file cannot be written.
    }
  }

  #now(): number {
    const value = this.#clock()
    return nonNegativeInteger(value, 'workspace clock value', 'INVALID_MANAGER_OPTIONS')
  }

  async #git(
    args: readonly string[],
    allowedExitCodes: readonly number[] = [0],
    environment: Readonly<Record<string, string>> = {},
  ): Promise<GitCommandResult> {
    return new Promise((resolvePromise, rejectPromise) => {
      let stdoutBytes = 0
      let stderrBytes = 0
      const stdout: Buffer[] = []
      const stderr: Buffer[] = []
      let failure: StrongFlowGitWorkspaceError | undefined
      let settled = false
      const child = spawn(this.#gitExecutable, [...args], {
        env: {
          ...process.env,
          ...environment,
          GIT_TERMINAL_PROMPT: '0',
          LC_ALL: 'C',
        },
        shell: false,
        stdio: ['ignore', 'pipe', 'pipe'],
      })
      const timer = setTimeout(() => {
        failure = new StrongFlowGitWorkspaceError(
          'GIT_COMMAND_TIMEOUT',
          `Git command exceeded ${this.#commandTimeoutMillis} ms`,
        )
        child.kill('SIGKILL')
      }, this.#commandTimeoutMillis)
      timer.unref()
      const append = (target: Buffer[], chunk: Buffer, stream: 'stdout' | 'stderr'): void => {
        if (stream === 'stdout') stdoutBytes += chunk.length
        else stderrBytes += chunk.length
        if (stdoutBytes + stderrBytes > this.#maxCommandOutputBytes) {
          failure = new StrongFlowGitWorkspaceError(
            'GIT_OUTPUT_LIMIT',
            `Git command exceeded ${this.#maxCommandOutputBytes} output bytes`,
          )
          child.kill('SIGKILL')
          return
        }
        target.push(chunk)
      }
      child.stdout.on('data', (chunk: Buffer) => append(stdout, chunk, 'stdout'))
      child.stderr.on('data', (chunk: Buffer) => append(stderr, chunk, 'stderr'))
      child.once('error', error => {
        if (settled) return
        settled = true
        clearTimeout(timer)
        rejectPromise(new StrongFlowGitWorkspaceError(
          'GIT_COMMAND_FAILED',
          'Git executable could not be started',
          { cause: error },
        ))
      })
      child.once('close', code => {
        if (settled) return
        settled = true
        clearTimeout(timer)
        if (failure !== undefined) {
          rejectPromise(failure)
          return
        }
        const exitCode = code ?? -1
        const stdoutBuffer = Buffer.concat(stdout)
        const result = Object.freeze({
          exitCode,
          stdout: stdoutBuffer.toString('utf8'),
          stdoutBytes: Uint8Array.from(stdoutBuffer),
          stderr: Buffer.concat(stderr).toString('utf8'),
        })
        if (!allowedExitCodes.includes(exitCode)) {
          rejectPromise(new StrongFlowGitWorkspaceError(
            'GIT_COMMAND_FAILED',
            `Git command exited with status ${exitCode}`,
          ))
          return
        }
        resolvePromise(result)
      })
    })
  }
}
