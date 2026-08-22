import { createHash } from 'node:crypto'
import { realpath } from 'node:fs/promises'
import {
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from 'node:path'

import {
  AttemptId,
  CandidateId,
  CandidateWriterLeaseId,
  GitCommitId,
  GitDiffId,
  GitTreeId,
  JobId,
  SourceSnapshotId,
  StageRunId,
  StrongFlowWorkspaceId,
  VerificationSnapshotId,
  type GitObjectFormat,
  type JobId as JobIdentifier,
  type SourceHeadKind,
  type StrongFlowCandidateIdentity,
  type StrongFlowCandidateWriterLease,
  type StrongFlowRoleId,
  type StrongFlowRoleWorkspaceAssignment,
  type StrongFlowRoleWorkspaceMode,
  type StrongFlowSourceSnapshotIdentity,
  type StrongFlowVerificationSnapshotIdentity,
  type StrongFlowWorkspaceId as WorkspaceIdentifier,
  type VerificationSnapshotId as VerificationIdentifier,
} from '@winwincode/contracts'

export type StrongFlowWorkspacePolicyErrorCode =
  | 'INVALID_POLICY_INPUT'
  | 'DIRTY_SOURCE'
  | 'AMBIGUOUS_SOURCE'
  | 'UNSUPPORTED_SOURCE'
  | 'PATH_TRAVERSAL'
  | 'PATH_NOT_FOUND'
  | 'SYMLINK_ESCAPE'
  | 'VERIFICATION_SNAPSHOT_REQUIRED'
  | 'CANDIDATE_MISMATCH'
  | 'WRITER_ROLE_DENIED'
  | 'WRITER_CONFLICT'
  | 'WRITER_LEASE_MISMATCH'
  | 'NO_ACTIVE_WRITER'

export class StrongFlowWorkspacePolicyError extends Error {
  readonly code: StrongFlowWorkspacePolicyErrorCode

  constructor(
    code: StrongFlowWorkspacePolicyErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowWorkspacePolicyError'
    this.code = code
  }
}

export type ObservedGitOperation =
  | 'none'
  | 'merge'
  | 'rebase'
  | 'cherry-pick'
  | 'revert'
  | 'bisect'

export interface ObservedGitSourceState {
  readonly repositoryPath: string
  readonly objectFormat: GitObjectFormat
  readonly headKind: SourceHeadKind | 'unborn'
  readonly branchName?: string
  readonly commitId?: string
  readonly treeId?: string
  readonly indexState: 'clean' | 'dirty' | 'conflicted'
  readonly worktreeState: 'clean' | 'dirty'
  readonly operation: ObservedGitOperation
  readonly bare: boolean
}

export interface AdmittedStrongFlowSource {
  readonly repositoryPath: string
  readonly identity: StrongFlowSourceSnapshotIdentity
}

export interface CreateStrongFlowCandidateIdentityInput {
  readonly source: StrongFlowSourceSnapshotIdentity
  readonly candidateCommitId: string
  readonly candidateTreeId: string
  readonly diffId: string
}

export interface StrongFlowWorkspaceLayout {
  readonly home: string
  readonly jobId: JobIdentifier
  readonly workspaceId: WorkspaceIdentifier
  readonly sourceSnapshot: StrongFlowSourceSnapshotIdentity
  readonly root: string
  readonly sourcePath: string
  readonly candidatePath: string
  readonly verificationRoot: string
  readonly metadataPath: string
}

export interface CreateStrongFlowWorkspaceLayoutInput {
  readonly home: string
  readonly jobId: JobIdentifier
  readonly sourceSnapshot: StrongFlowSourceSnapshotIdentity
}

export interface StrongFlowRoleWorkspaceInput {
  readonly layout: StrongFlowWorkspaceLayout
  readonly roleId: StrongFlowRoleId
  readonly stageRunId: string
  readonly candidate?: StrongFlowCandidateIdentity
  readonly verificationSnapshot?: StrongFlowVerificationSnapshotIdentity
}

export interface StrongFlowVerificationWorkspacePaths {
  readonly path: string
  readonly temporaryOutputPath: string
}

export interface ClaimStrongFlowCandidateWriterInput {
  readonly leaseId: string
  readonly jobId: string
  readonly workspaceId: string
  readonly roleId: StrongFlowRoleId
  readonly stageRunId: string
  readonly attemptId: string
  readonly acquiredAtMillis: number
}

export const STRONGFLOW_WORKSPACE_MODE_BY_ROLE: Readonly<
  Record<StrongFlowRoleId, StrongFlowRoleWorkspaceMode>
> = Object.freeze({
  requirements: 'source-read-only',
  solution: 'source-read-only',
  planner: 'source-read-only',
  executor: 'candidate-write',
  reviewer: 'candidate-read-only',
  verifier: 'candidate-read-only',
  'adversarial-verifier': 'candidate-read-only',
  remediator: 'candidate-write',
})

export const STRONGFLOW_WORKSPACE_RETENTION_POLICY = Object.freeze({
  originalRepository: 'unmanaged-never-modified-or-deleted',
  sourceSnapshot: 'retain-until-job-terminal',
  candidateWorktree: 'retain-until-job-terminal-and-writer-released',
  verificationSnapshot: 'delete-after-associated-read-only-run-settles',
  durableArtifacts: 'outside-workspace-cleanup',
} as const)

function policyError(
  code: StrongFlowWorkspacePolicyErrorCode,
  message: string,
  options?: ErrorOptions,
): never {
  throw new StrongFlowWorkspacePolicyError(code, message, options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
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
  ) return policyError('INVALID_POLICY_INPUT', `${label} has an unexpected shape`)
}

function digest(kind: string, fields: readonly string[]): string {
  const hash = createHash('sha256')
  hash.update(`${kind.length}:${kind}`)
  for (const field of fields) hash.update(`${Buffer.byteLength(field)}:${field}`)
  return hash.digest('hex')
}

function objectLength(format: GitObjectFormat): number {
  return format === 'sha1' ? 40 : 64
}

function objectId(value: string, format: GitObjectFormat, kind: 'commit' | 'tree'): string {
  if (
    typeof value !== 'string'
    || value.length !== objectLength(format)
    || !/^[0-9a-f]+$/u.test(value)
  ) return policyError('INVALID_POLICY_INPUT', `${kind} id does not match ${format}`)
  return value
}

function portableBranch(value: string | undefined): string {
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.length > 200
    || value.trim() !== value
    || /[\u0000-\u001f\u007f]/u.test(value)
  ) return policyError('AMBIGUOUS_SOURCE', 'branch source has no valid branch name')
  return value
}

function absolutePath(value: string, label: string): string {
  if (typeof value !== 'string' || value.length === 0 || !isAbsolute(value)) {
    return policyError('INVALID_POLICY_INPUT', `${label} must be an absolute path`)
  }
  return resolve(value)
}

function validateSourceIdentity(
  source: StrongFlowSourceSnapshotIdentity,
): StrongFlowSourceSnapshotIdentity {
  if (!isRecord(source)) {
    return policyError('INVALID_POLICY_INPUT', 'source snapshot identity must be an object')
  }
  exactKeys(
    source,
    ['sourceSnapshotId', 'objectFormat', 'headKind', 'commitId', 'treeId'],
    ['branchName'],
    'source snapshot identity',
  )
  const objectFormat = source.objectFormat
  if (objectFormat !== 'sha1' && objectFormat !== 'sha256') {
    return policyError('INVALID_POLICY_INPUT', 'source snapshot object format is unknown')
  }
  let sourceSnapshotId
  let commitId
  let treeId
  try {
    sourceSnapshotId = SourceSnapshotId(source.sourceSnapshotId)
    commitId = GitCommitId(objectId(source.commitId, objectFormat, 'commit'))
    treeId = GitTreeId(objectId(source.treeId, objectFormat, 'tree'))
  } catch (error) {
    if (error instanceof StrongFlowWorkspacePolicyError) throw error
    return policyError('INVALID_POLICY_INPUT', 'source snapshot identity is invalid', {
      cause: error,
    })
  }
  if (source.headKind !== 'branch' && source.headKind !== 'detached') {
    return policyError('INVALID_POLICY_INPUT', 'source snapshot head kind is unknown')
  }
  const branchName = source.headKind === 'branch'
    ? portableBranch(source.branchName)
    : undefined
  if (source.headKind === 'detached' && source.branchName !== undefined) {
    return policyError('INVALID_POLICY_INPUT', 'detached source cannot name a branch')
  }
  const expectedId = SourceSnapshotId(
    `source-sha256-${digest('source', [objectFormat, commitId, treeId])}`,
  )
  if (sourceSnapshotId !== expectedId) {
    return policyError('INVALID_POLICY_INPUT', 'source snapshot id does not match its Git objects')
  }
  return Object.freeze({
    sourceSnapshotId,
    objectFormat,
    headKind: source.headKind,
    ...(branchName === undefined ? {} : { branchName }),
    commitId,
    treeId,
  })
}

export function admitStrongFlowSource(
  observed: ObservedGitSourceState,
): AdmittedStrongFlowSource {
  if (!isRecord(observed)) {
    return policyError('INVALID_POLICY_INPUT', 'observed Git source must be an object')
  }
  exactKeys(
    observed,
    [
      'repositoryPath',
      'objectFormat',
      'headKind',
      'indexState',
      'worktreeState',
      'operation',
      'bare',
    ],
    ['branchName', 'commitId', 'treeId'],
    'observed Git source',
  )
  const repositoryPath = absolutePath(observed.repositoryPath, 'repositoryPath')
  if (typeof observed.bare !== 'boolean') {
    return policyError('INVALID_POLICY_INPUT', 'Git source bare flag must be boolean')
  }
  if (observed.bare) {
    return policyError('UNSUPPORTED_SOURCE', 'bare Git repositories have no source worktree')
  }
  if (!['none', 'merge', 'rebase', 'cherry-pick', 'revert', 'bisect'].includes(
    observed.operation,
  )) return policyError('INVALID_POLICY_INPUT', 'Git source operation is unknown')
  if (!['clean', 'dirty', 'conflicted'].includes(observed.indexState)) {
    return policyError('INVALID_POLICY_INPUT', 'Git source index state is unknown')
  }
  if (observed.worktreeState !== 'clean' && observed.worktreeState !== 'dirty') {
    return policyError('INVALID_POLICY_INPUT', 'Git source worktree state is unknown')
  }
  if (observed.headKind === 'unborn' || observed.commitId === undefined
    || observed.treeId === undefined) {
    return policyError('AMBIGUOUS_SOURCE', 'Git source has no resolved HEAD commit and tree')
  }
  if (observed.operation !== 'none') {
    return policyError(
      'AMBIGUOUS_SOURCE',
      `Git source has an active ${observed.operation} operation`,
    )
  }
  if (observed.indexState === 'conflicted') {
    return policyError('AMBIGUOUS_SOURCE', 'Git source index contains conflicts')
  }
  if (observed.indexState !== 'clean' || observed.worktreeState !== 'clean') {
    return policyError(
      'DIRTY_SOURCE',
      'Git source has tracked, staged, or untracked changes; commit or remove them first',
    )
  }
  if (observed.objectFormat !== 'sha1' && observed.objectFormat !== 'sha256') {
    return policyError('UNSUPPORTED_SOURCE', 'Git source object format is unsupported')
  }
  if (observed.headKind !== 'branch' && observed.headKind !== 'detached') {
    return policyError('AMBIGUOUS_SOURCE', 'Git source HEAD kind is unresolved')
  }
  const commitId = GitCommitId(objectId(
    observed.commitId,
    observed.objectFormat,
    'commit',
  ))
  const treeId = GitTreeId(objectId(observed.treeId, observed.objectFormat, 'tree'))
  const branchName = observed.headKind === 'branch'
    ? portableBranch(observed.branchName)
    : undefined
  if (observed.headKind === 'detached' && observed.branchName !== undefined) {
    return policyError('AMBIGUOUS_SOURCE', 'detached Git source cannot name a branch')
  }
  const sourceSnapshotId = SourceSnapshotId(
    `source-sha256-${digest('source', [observed.objectFormat, commitId, treeId])}`,
  )
  return Object.freeze({
    repositoryPath,
    identity: Object.freeze({
      sourceSnapshotId,
      objectFormat: observed.objectFormat,
      headKind: observed.headKind,
      ...(branchName === undefined ? {} : { branchName }),
      commitId,
      treeId,
    }),
  })
}

export function createStrongFlowGitDiffId(diff: string | Uint8Array): ReturnType<
  typeof GitDiffId
> {
  if (typeof diff !== 'string' && !(diff instanceof Uint8Array)) {
    return policyError('INVALID_POLICY_INPUT', 'Git diff must be text or bytes')
  }
  return GitDiffId(createHash('sha256').update(diff).digest('hex'))
}

export function createStrongFlowCandidateIdentity(
  input: CreateStrongFlowCandidateIdentityInput,
): StrongFlowCandidateIdentity {
  if (!isRecord(input)) {
    return policyError('INVALID_POLICY_INPUT', 'candidate identity input must be an object')
  }
  exactKeys(
    input,
    ['source', 'candidateCommitId', 'candidateTreeId', 'diffId'],
    [],
    'candidate identity input',
  )
  const source = validateSourceIdentity(input.source)
  const candidateCommitId = GitCommitId(objectId(
    input.candidateCommitId,
    source.objectFormat,
    'commit',
  ))
  const candidateTreeId = GitTreeId(objectId(
    input.candidateTreeId,
    source.objectFormat,
    'tree',
  ))
  let diffId
  try {
    diffId = GitDiffId(input.diffId)
  } catch (error) {
    return policyError('INVALID_POLICY_INPUT', 'candidate diff id is invalid', { cause: error })
  }
  const candidateId = CandidateId(`candidate-sha256-${digest('candidate', [
    source.sourceSnapshotId,
    source.commitId,
    source.treeId,
    candidateCommitId,
    candidateTreeId,
    diffId,
  ])}`)
  return Object.freeze({
    candidateId,
    sourceSnapshotId: source.sourceSnapshotId,
    baseCommitId: source.commitId,
    baseTreeId: source.treeId,
    candidateCommitId,
    candidateTreeId,
    diffId,
  })
}

function validateCandidateIdentity(
  candidate: StrongFlowCandidateIdentity,
): StrongFlowCandidateIdentity {
  if (!isRecord(candidate)) {
    return policyError('INVALID_POLICY_INPUT', 'candidate identity must be an object')
  }
  exactKeys(candidate, [
    'candidateId',
    'sourceSnapshotId',
    'baseCommitId',
    'baseTreeId',
    'candidateCommitId',
    'candidateTreeId',
    'diffId',
  ], [], 'candidate identity')
  const format: GitObjectFormat = typeof candidate.baseCommitId === 'string'
    && candidate.baseCommitId.length === 40
    ? 'sha1'
    : 'sha256'
  let candidateId
  let sourceSnapshotId
  let baseCommitId
  let baseTreeId
  let candidateCommitId
  let candidateTreeId
  let diffId
  try {
    candidateId = CandidateId(candidate.candidateId)
    sourceSnapshotId = SourceSnapshotId(candidate.sourceSnapshotId)
    baseCommitId = GitCommitId(objectId(candidate.baseCommitId, format, 'commit'))
    baseTreeId = GitTreeId(objectId(candidate.baseTreeId, format, 'tree'))
    candidateCommitId = GitCommitId(objectId(
      candidate.candidateCommitId,
      format,
      'commit',
    ))
    candidateTreeId = GitTreeId(objectId(candidate.candidateTreeId, format, 'tree'))
    diffId = GitDiffId(candidate.diffId)
  } catch (error) {
    if (error instanceof StrongFlowWorkspacePolicyError) throw error
    return policyError('INVALID_POLICY_INPUT', 'candidate identity is invalid', { cause: error })
  }
  const expectedSourceSnapshotId = SourceSnapshotId(
    `source-sha256-${digest('source', [format, baseCommitId, baseTreeId])}`,
  )
  if (sourceSnapshotId !== expectedSourceSnapshotId) {
    return policyError('INVALID_POLICY_INPUT', 'candidate source identity is inconsistent')
  }
  const expectedCandidateId = CandidateId(`candidate-sha256-${digest('candidate', [
    sourceSnapshotId,
    baseCommitId,
    baseTreeId,
    candidateCommitId,
    candidateTreeId,
    diffId,
  ])}`)
  if (candidateId !== expectedCandidateId) {
    return policyError('INVALID_POLICY_INPUT', 'candidate id does not match its content')
  }
  return Object.freeze({
    candidateId,
    sourceSnapshotId,
    baseCommitId,
    baseTreeId,
    candidateCommitId,
    candidateTreeId,
    diffId,
  })
}

function validateVerificationSnapshotIdentity(
  snapshot: StrongFlowVerificationSnapshotIdentity,
): StrongFlowVerificationSnapshotIdentity {
  if (!isRecord(snapshot)) {
    return policyError('INVALID_POLICY_INPUT', 'verification snapshot identity must be an object')
  }
  exactKeys(snapshot, [
    'verificationSnapshotId',
    'candidateId',
    'candidateCommitId',
    'candidateTreeId',
  ], [], 'verification snapshot identity')
  let verificationSnapshotId
  let candidateId
  let candidateCommitId
  let candidateTreeId
  try {
    verificationSnapshotId = VerificationSnapshotId(snapshot.verificationSnapshotId)
    candidateId = CandidateId(snapshot.candidateId)
    candidateCommitId = GitCommitId(snapshot.candidateCommitId)
    candidateTreeId = GitTreeId(snapshot.candidateTreeId)
  } catch (error) {
    return policyError('INVALID_POLICY_INPUT', 'verification snapshot identity is invalid', {
      cause: error,
    })
  }
  if (candidateCommitId.length !== candidateTreeId.length) {
    return policyError(
      'INVALID_POLICY_INPUT',
      'verification commit and tree use different Git object formats',
    )
  }
  const expectedId = VerificationSnapshotId(
    `verification-sha256-${digest('verification', [
      candidateId,
      candidateCommitId,
      candidateTreeId,
    ])}`,
  )
  if (verificationSnapshotId !== expectedId) {
    return policyError(
      'INVALID_POLICY_INPUT',
      'verification snapshot id does not match its candidate content',
    )
  }
  return Object.freeze({
    verificationSnapshotId,
    candidateId,
    candidateCommitId,
    candidateTreeId,
  })
}

export function createStrongFlowVerificationSnapshotIdentity(
  candidate: StrongFlowCandidateIdentity,
): StrongFlowVerificationSnapshotIdentity {
  const validated = validateCandidateIdentity(candidate)
  const candidateId = validated.candidateId
  const candidateCommitId = validated.candidateCommitId
  const candidateTreeId = validated.candidateTreeId
  const verificationSnapshotId = VerificationSnapshotId(
    `verification-sha256-${digest('verification', [
      candidateId,
      candidateCommitId,
      candidateTreeId,
    ])}`,
  )
  return Object.freeze({
    verificationSnapshotId,
    candidateId,
    candidateCommitId,
    candidateTreeId,
  })
}

/** Returns the only managed root that may belong to a StrongFlow job. */
export function strongFlowWorkspaceRootForJob(
  homeInput: string,
  jobIdInput: string,
): string {
  const home = absolutePath(homeInput, 'workspace home')
  const jobId = JobId(jobIdInput)
  return join(home, 'strongflow-workspaces', digest('job-directory', [jobId]))
}

export function createStrongFlowWorkspaceLayout(
  input: CreateStrongFlowWorkspaceLayoutInput,
): StrongFlowWorkspaceLayout {
  if (!isRecord(input)) {
    return policyError('INVALID_POLICY_INPUT', 'workspace layout input must be an object')
  }
  exactKeys(input, ['home', 'jobId', 'sourceSnapshot'], [], 'workspace layout input')
  const home = absolutePath(input.home, 'workspace home')
  const jobId = JobId(input.jobId)
  const sourceSnapshot = validateSourceIdentity(input.sourceSnapshot)
  const workspaceId = StrongFlowWorkspaceId(
    `workspace-sha256-${digest('workspace', [jobId, sourceSnapshot.sourceSnapshotId])}`,
  )
  const root = strongFlowWorkspaceRootForJob(home, jobId)
  return Object.freeze({
    home,
    jobId,
    workspaceId,
    sourceSnapshot,
    root,
    sourcePath: join(root, 'source'),
    candidatePath: join(root, 'candidate'),
    verificationRoot: join(root, 'verification'),
    metadataPath: join(root, 'metadata'),
  })
}

function validateLayout(layout: StrongFlowWorkspaceLayout): StrongFlowWorkspaceLayout {
  if (!isRecord(layout)) {
    return policyError('INVALID_POLICY_INPUT', 'workspace layout must be an object')
  }
  exactKeys(layout, [
    'home',
    'jobId',
    'workspaceId',
    'sourceSnapshot',
    'root',
    'sourcePath',
    'candidatePath',
    'verificationRoot',
    'metadataPath',
  ], [], 'workspace layout')
  const expected = createStrongFlowWorkspaceLayout({
    home: layout.home,
    jobId: layout.jobId,
    sourceSnapshot: layout.sourceSnapshot,
  })
  if (
    layout.workspaceId !== expected.workspaceId
    || layout.root !== expected.root
    || layout.sourcePath !== expected.sourcePath
    || layout.candidatePath !== expected.candidatePath
    || layout.verificationRoot !== expected.verificationRoot
    || layout.metadataPath !== expected.metadataPath
  ) return policyError('INVALID_POLICY_INPUT', 'workspace layout is not canonical')
  return expected
}

export function strongFlowVerificationWorkspacePaths(
  layout: StrongFlowWorkspaceLayout,
  verificationSnapshotIdInput: string,
  roleId: StrongFlowRoleId,
  stageRunIdInput: string,
): StrongFlowVerificationWorkspacePaths {
  const validatedLayout = validateLayout(layout)
  if (!['reviewer', 'verifier', 'adversarial-verifier'].includes(roleId)) {
    return policyError(
      'INVALID_POLICY_INPUT',
      'only review and verification roles receive verification workspaces',
    )
  }
  let verificationSnapshotId: VerificationIdentifier
  let stageRunId
  try {
    verificationSnapshotId = VerificationSnapshotId(verificationSnapshotIdInput)
    stageRunId = StageRunId(stageRunIdInput)
  } catch (error) {
    return policyError('INVALID_POLICY_INPUT', 'verification workspace identity is invalid', {
      cause: error,
    })
  }
  const directory = digest('verification-directory', [
    verificationSnapshotId,
    roleId,
    stageRunId,
  ])
  return Object.freeze({
    path: join(validatedLayout.verificationRoot, directory),
    temporaryOutputPath: join(validatedLayout.root, 'verification-output', directory),
  })
}

export function strongFlowRoleWorkspace(
  input: StrongFlowRoleWorkspaceInput,
): StrongFlowRoleWorkspaceAssignment {
  if (!isRecord(input)) {
    return policyError('INVALID_POLICY_INPUT', 'role workspace input must be an object')
  }
  exactKeys(
    input,
    ['layout', 'roleId', 'stageRunId'],
    ['candidate', 'verificationSnapshot'],
    'role workspace input',
  )
  if (
    typeof input.roleId !== 'string'
    || !Object.hasOwn(STRONGFLOW_WORKSPACE_MODE_BY_ROLE, input.roleId)
  ) {
    return policyError('INVALID_POLICY_INPUT', 'StrongFlow role is unknown')
  }
  const roleId = input.roleId as StrongFlowRoleId
  let stageRunId
  try {
    stageRunId = StageRunId(input.stageRunId)
  } catch (error) {
    return policyError('INVALID_POLICY_INPUT', 'StrongFlow stage run id is invalid', {
      cause: error,
    })
  }
  const mode = STRONGFLOW_WORKSPACE_MODE_BY_ROLE[roleId]
  const layout = validateLayout(input.layout)
  if (mode === 'source-read-only') {
    return Object.freeze({
      roleId,
      stageRunId,
      workspaceId: layout.workspaceId,
      mode,
      path: layout.sourcePath,
      sourceSnapshotId: layout.sourceSnapshot.sourceSnapshotId,
    })
  }
  if (mode === 'candidate-write') {
    if (roleId === 'remediator' && input.candidate === undefined) {
      return policyError('CANDIDATE_MISMATCH', 'remediator requires the current candidate')
    }
    const selectedCandidate = input.candidate === undefined
      ? undefined
      : validateCandidateIdentity(input.candidate)
    if (
      selectedCandidate !== undefined
      && selectedCandidate.sourceSnapshotId !== layout.sourceSnapshot.sourceSnapshotId
    ) {
      return policyError(
        'CANDIDATE_MISMATCH',
        'candidate does not belong to the assigned source snapshot',
      )
    }
    const candidateId = selectedCandidate?.candidateId
    return Object.freeze({
      roleId,
      stageRunId,
      workspaceId: layout.workspaceId,
      mode,
      path: layout.candidatePath,
      sourceSnapshotId: layout.sourceSnapshot.sourceSnapshotId,
      ...(candidateId === undefined ? {} : { candidateId }),
    })
  }
  if (input.verificationSnapshot === undefined || input.candidate === undefined) {
    return policyError(
      'VERIFICATION_SNAPSHOT_REQUIRED',
      `role ${roleId} requires one frozen candidate and verification snapshot`,
    )
  }
  const snapshot = validateVerificationSnapshotIdentity(input.verificationSnapshot)
  const selectedCandidate = validateCandidateIdentity(input.candidate)
  if (
    selectedCandidate.sourceSnapshotId !== layout.sourceSnapshot.sourceSnapshotId
    || snapshot.candidateId !== selectedCandidate.candidateId
    || snapshot.candidateCommitId !== selectedCandidate.candidateCommitId
    || snapshot.candidateTreeId !== selectedCandidate.candidateTreeId
  ) {
    return policyError(
      'CANDIDATE_MISMATCH',
      'verification snapshot does not reference the selected candidate',
    )
  }
  const verificationWorkspace = strongFlowVerificationWorkspacePaths(
    layout,
    snapshot.verificationSnapshotId,
    roleId,
    stageRunId,
  )
  return Object.freeze({
    roleId,
    stageRunId,
    workspaceId: layout.workspaceId,
    mode,
    path: verificationWorkspace.path,
    temporaryOutputPath: verificationWorkspace.temporaryOutputPath,
    sourceSnapshotId: layout.sourceSnapshot.sourceSnapshotId,
    candidateId: snapshot.candidateId,
    verificationSnapshotId: snapshot.verificationSnapshotId,
  })
}

function portableRelativePath(value: string): readonly string[] {
  if (
    typeof value !== 'string'
    || value.length === 0
    || isAbsolute(value)
    || value.includes('\\')
    || /^[A-Za-z]:/u.test(value)
    || /[\u0000-\u001f\u007f]/u.test(value)
  ) return policyError('PATH_TRAVERSAL', 'workspace path must be a portable relative path')
  const segments = value.split('/')
  if (segments.some(segment => segment.length === 0 || segment === '.' || segment === '..')) {
    return policyError('PATH_TRAVERSAL', 'workspace path contains traversal segments')
  }
  return segments
}

function isContained(root: string, candidate: string): boolean {
  const fromRoot = relative(root, candidate)
  return fromRoot === ''
    || (!fromRoot.startsWith(`..${sep}`) && fromRoot !== '..' && !isAbsolute(fromRoot))
}

export async function resolveExistingStrongFlowWorkspacePath(
  rootInput: string,
  relativePath: string,
): Promise<string> {
  const root = absolutePath(rootInput, 'workspace containment root')
  const segments = portableRelativePath(relativePath)
  let realRoot: string
  let realCandidate: string
  try {
    realRoot = await realpath(root)
    realCandidate = await realpath(join(realRoot, ...segments))
  } catch (error) {
    return policyError('PATH_NOT_FOUND', 'workspace path does not exist', { cause: error })
  }
  if (!isContained(realRoot, realCandidate)) {
    return policyError('SYMLINK_ESCAPE', 'workspace path resolves outside its assigned root')
  }
  return realCandidate
}

/**
 * Resolve an existing or new write target while rejecting traversal and every existing symlink
 * ancestor that leaves the assigned workspace. The executor must still open the returned path
 * through its sandbox because the filesystem can change after this admission check.
 */
export async function resolveStrongFlowWorkspaceWritePath(
  rootInput: string,
  relativePath: string,
): Promise<string> {
  const root = absolutePath(rootInput, 'workspace containment root')
  const segments = portableRelativePath(relativePath)
  let realRoot: string
  try {
    realRoot = await realpath(root)
  } catch (error) {
    return policyError('PATH_NOT_FOUND', 'workspace root does not exist', { cause: error })
  }
  let current = realRoot
  for (const [index, segment] of segments.entries()) {
    const candidate = join(current, segment)
    try {
      const resolvedCandidate = await realpath(candidate)
      if (!isContained(realRoot, resolvedCandidate)) {
        return policyError('SYMLINK_ESCAPE', 'workspace path resolves outside its assigned root')
      }
      current = resolvedCandidate
    } catch (error) {
      const code = typeof error === 'object' && error !== null && 'code' in error
        ? String(error.code)
        : undefined
      if (code !== 'ENOENT') {
        return policyError('PATH_NOT_FOUND', 'workspace write path cannot be resolved', {
          cause: error,
        })
      }
      const unresolved = join(current, ...segments.slice(index))
      if (!isContained(realRoot, unresolved)) {
        return policyError('SYMLINK_ESCAPE', 'workspace write path leaves its assigned root')
      }
      return unresolved
    }
  }
  return current
}

function sameLease(
  current: StrongFlowCandidateWriterLease,
  request: StrongFlowCandidateWriterLease,
): boolean {
  return current.leaseId === request.leaseId
    && current.jobId === request.jobId
    && current.workspaceId === request.workspaceId
    && current.roleId === request.roleId
    && current.stageRunId === request.stageRunId
    && current.attemptId === request.attemptId
    && current.acquiredAtMillis === request.acquiredAtMillis
}

export function claimStrongFlowCandidateWriter(
  current: StrongFlowCandidateWriterLease | undefined,
  input: ClaimStrongFlowCandidateWriterInput,
): StrongFlowCandidateWriterLease {
  if (!isRecord(input)) {
    return policyError('INVALID_POLICY_INPUT', 'writer lease input must be an object')
  }
  exactKeys(input, [
    'leaseId',
    'jobId',
    'workspaceId',
    'roleId',
    'stageRunId',
    'attemptId',
    'acquiredAtMillis',
  ], [], 'writer lease input')
  if (input.roleId !== 'executor' && input.roleId !== 'remediator') {
    return policyError(
      'WRITER_ROLE_DENIED',
      'requested role cannot write the candidate workspace',
    )
  }
  if (!Number.isSafeInteger(input.acquiredAtMillis) || input.acquiredAtMillis < 0) {
    return policyError('INVALID_POLICY_INPUT', 'writer acquisition time is invalid')
  }
  let requested: StrongFlowCandidateWriterLease
  try {
    requested = Object.freeze({
      leaseId: CandidateWriterLeaseId(input.leaseId),
      jobId: JobId(input.jobId),
      workspaceId: StrongFlowWorkspaceId(input.workspaceId),
      roleId: input.roleId,
      stageRunId: StageRunId(input.stageRunId),
      attemptId: AttemptId(input.attemptId),
      acquiredAtMillis: input.acquiredAtMillis,
    })
  } catch (error) {
    return policyError('INVALID_POLICY_INPUT', 'writer lease identity is invalid', {
      cause: error,
    })
  }
  if (current === undefined) return requested
  if (sameLease(current, requested)) return current
  return policyError(
    'WRITER_CONFLICT',
    `candidate workspace already has active writer ${current.roleId}`,
  )
}

export function releaseStrongFlowCandidateWriter(
  current: StrongFlowCandidateWriterLease | undefined,
  leaseIdInput: string,
): undefined {
  if (current === undefined) {
    return policyError('NO_ACTIVE_WRITER', 'candidate workspace has no active writer')
  }
  let leaseId
  try {
    leaseId = CandidateWriterLeaseId(leaseIdInput)
  } catch (error) {
    return policyError('INVALID_POLICY_INPUT', 'writer lease id is invalid', { cause: error })
  }
  if (current.leaseId !== leaseId) {
    return policyError('WRITER_LEASE_MISMATCH', 'writer release names a different lease')
  }
  return undefined
}
