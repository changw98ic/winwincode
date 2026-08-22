import type {
  AttemptId,
  CandidateId,
  JobId,
  StageRunId,
} from './strongflow-job.js'
import type { StrongFlowRoleId, StrongFlowRoleWorkspaceMode } from './strongflow-role.js'

declare const workspaceIdentifierBrand: unique symbol

type WorkspaceIdentifier<Name extends string> = string & {
  readonly [workspaceIdentifierBrand]: Name
}

export type GitCommitId = WorkspaceIdentifier<'GitCommitId'>
export type GitTreeId = WorkspaceIdentifier<'GitTreeId'>
export type GitDiffId = WorkspaceIdentifier<'GitDiffId'>
export type SourceSnapshotId = WorkspaceIdentifier<'SourceSnapshotId'>
export type StrongFlowWorkspaceId = WorkspaceIdentifier<'StrongFlowWorkspaceId'>
export type VerificationSnapshotId = WorkspaceIdentifier<'VerificationSnapshotId'>
export type CandidateWriterLeaseId = WorkspaceIdentifier<'CandidateWriterLeaseId'>

const GIT_OBJECT_PATTERN = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u
const PORTABLE_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/-]{0,199}$/u

function workspaceIdentifier<Name extends string>(
  value: string,
  name: Name,
  pattern: RegExp,
): WorkspaceIdentifier<Name> {
  if (typeof value !== 'string' || !pattern.test(value)) {
    throw new TypeError(`${name} is invalid`)
  }
  return value as WorkspaceIdentifier<Name>
}

export function GitCommitId(value: string): GitCommitId {
  return workspaceIdentifier(value, 'GitCommitId', GIT_OBJECT_PATTERN)
}

export function GitTreeId(value: string): GitTreeId {
  return workspaceIdentifier(value, 'GitTreeId', GIT_OBJECT_PATTERN)
}

export function GitDiffId(value: string): GitDiffId {
  return workspaceIdentifier(value, 'GitDiffId', /^[0-9a-f]{64}$/u)
}

export function SourceSnapshotId(value: string): SourceSnapshotId {
  return workspaceIdentifier(value, 'SourceSnapshotId', /^source-sha256-[0-9a-f]{64}$/u)
}

export function StrongFlowWorkspaceId(value: string): StrongFlowWorkspaceId {
  return workspaceIdentifier(
    value,
    'StrongFlowWorkspaceId',
    /^workspace-sha256-[0-9a-f]{64}$/u,
  )
}

export function VerificationSnapshotId(value: string): VerificationSnapshotId {
  return workspaceIdentifier(
    value,
    'VerificationSnapshotId',
    /^verification-sha256-[0-9a-f]{64}$/u,
  )
}

export function CandidateWriterLeaseId(value: string): CandidateWriterLeaseId {
  return workspaceIdentifier(value, 'CandidateWriterLeaseId', PORTABLE_ID_PATTERN)
}

export type GitObjectFormat = 'sha1' | 'sha256'
export type SourceHeadKind = 'branch' | 'detached'

export interface StrongFlowSourceSnapshotIdentity {
  readonly sourceSnapshotId: SourceSnapshotId
  readonly objectFormat: GitObjectFormat
  readonly headKind: SourceHeadKind
  readonly branchName?: string
  readonly commitId: GitCommitId
  readonly treeId: GitTreeId
}

export interface StrongFlowCandidateIdentity {
  readonly candidateId: CandidateId
  readonly sourceSnapshotId: SourceSnapshotId
  readonly baseCommitId: GitCommitId
  readonly baseTreeId: GitTreeId
  readonly candidateCommitId: GitCommitId
  readonly candidateTreeId: GitTreeId
  readonly diffId: GitDiffId
}

export interface StrongFlowVerificationSnapshotIdentity {
  readonly verificationSnapshotId: VerificationSnapshotId
  readonly candidateId: CandidateId
  readonly candidateCommitId: GitCommitId
  readonly candidateTreeId: GitTreeId
}

export interface StrongFlowRoleWorkspaceAssignment {
  readonly roleId: StrongFlowRoleId
  readonly stageRunId: StageRunId
  readonly workspaceId: StrongFlowWorkspaceId
  readonly mode: StrongFlowRoleWorkspaceMode
  readonly path: string
  readonly temporaryOutputPath?: string
  readonly sourceSnapshotId: SourceSnapshotId
  readonly candidateId?: CandidateId
  readonly verificationSnapshotId?: VerificationSnapshotId
}

export interface StrongFlowCandidateWriterLease {
  readonly leaseId: CandidateWriterLeaseId
  readonly jobId: JobId
  readonly workspaceId: StrongFlowWorkspaceId
  readonly roleId: 'executor' | 'remediator'
  readonly stageRunId: StageRunId
  readonly attemptId: AttemptId
  readonly acquiredAtMillis: number
}
