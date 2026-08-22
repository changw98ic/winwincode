import assert from 'node:assert/strict'
import {
  mkdir,
  mkdtemp,
  realpath,
  rm,
  symlink,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  AttemptId,
  GitCommitId,
  GitDiffId,
  GitTreeId,
  JobId,
  StageRunId,
} from '../packages/contracts/dist/index.js'
import {
  STRONGFLOW_WORKSPACE_MODE_BY_ROLE,
  STRONGFLOW_WORKSPACE_RETENTION_POLICY,
  StrongFlowWorkspacePolicyError,
  admitStrongFlowSource,
  claimStrongFlowCandidateWriter,
  createStrongFlowCandidateIdentity,
  createStrongFlowGitDiffId,
  createStrongFlowVerificationSnapshotIdentity,
  createStrongFlowWorkspaceLayout,
  releaseStrongFlowCandidateWriter,
  resolveExistingStrongFlowWorkspacePath,
  strongFlowRoleWorkspace,
  strongFlowVerificationWorkspacePaths,
} from '../packages/strongflow/dist/index.js'

const baseCommitId = '1'.repeat(40)
const baseTreeId = '2'.repeat(40)
const candidateCommitId = '3'.repeat(40)
const candidateTreeId = '4'.repeat(40)

function sourceState(overrides = {}) {
  return {
    repositoryPath: '/workspace/source-repository',
    objectFormat: 'sha1',
    headKind: 'branch',
    branchName: 'main',
    commitId: baseCommitId,
    treeId: baseTreeId,
    indexState: 'clean',
    worktreeState: 'clean',
    operation: 'none',
    bare: false,
    ...overrides,
  }
}

function expectedWorkspaceError(code) {
  return error => error instanceof StrongFlowWorkspacePolicyError && error.code === code
}

function candidate(source, suffix = '') {
  return createStrongFlowCandidateIdentity({
    source,
    candidateCommitId: suffix.length === 0 ? candidateCommitId : '5'.repeat(40),
    candidateTreeId: suffix.length === 0 ? candidateTreeId : '6'.repeat(40),
    diffId: createStrongFlowGitDiffId(`diff fixture ${suffix}`),
  })
}

test('admits only a clean resolved Git source and derives a content identity', () => {
  const admitted = admitStrongFlowSource(sourceState())
  const repeated = admitStrongFlowSource(sourceState({ repositoryPath: '/different/clone' }))
  const detached = admitStrongFlowSource(sourceState({
    headKind: 'detached',
    branchName: undefined,
  }))

  assert.equal(admitted.identity.commitId, GitCommitId(baseCommitId))
  assert.equal(admitted.identity.treeId, GitTreeId(baseTreeId))
  assert.match(admitted.identity.sourceSnapshotId, /^source-sha256-[0-9a-f]{64}$/u)
  assert.equal(repeated.identity.sourceSnapshotId, admitted.identity.sourceSnapshotId)
  assert.equal(detached.identity.sourceSnapshotId, admitted.identity.sourceSnapshotId)
  assert.equal(detached.identity.headKind, 'detached')
  assert.equal(detached.identity.branchName, undefined)
  assert.ok(Object.isFrozen(admitted))
  assert.ok(Object.isFrozen(admitted.identity))
})

test('dirty, conflicted, unresolved, active, and bare sources have explicit outcomes', () => {
  for (const change of [
    { indexState: 'dirty' },
    { worktreeState: 'dirty' },
  ]) {
    assert.throws(
      () => admitStrongFlowSource(sourceState(change)),
      expectedWorkspaceError('DIRTY_SOURCE'),
    )
  }
  for (const change of [
    { indexState: 'conflicted' },
    { headKind: 'unborn', commitId: undefined, treeId: undefined },
    { operation: 'merge' },
  ]) {
    assert.throws(
      () => admitStrongFlowSource(sourceState(change)),
      expectedWorkspaceError('AMBIGUOUS_SOURCE'),
    )
  }
  assert.throws(
    () => admitStrongFlowSource(sourceState({ bare: true })),
    expectedWorkspaceError('UNSUPPORTED_SOURCE'),
  )
  assert.throws(
    () => admitStrongFlowSource(sourceState({ repositoryPath: 'relative/source' })),
    expectedWorkspaceError('INVALID_POLICY_INPUT'),
  )
  assert.throws(
    () => admitStrongFlowSource(sourceState({ commitId: 'a'.repeat(64) })),
    expectedWorkspaceError('INVALID_POLICY_INPUT'),
  )
})

test('candidate and verification identities change only when their content inputs change', () => {
  const source = admitStrongFlowSource(sourceState()).identity
  const first = candidate(source)
  const repeated = candidate(source)
  const changed = candidate(source, 'changed')
  const verification = createStrongFlowVerificationSnapshotIdentity(first)

  assert.equal(first.candidateId, repeated.candidateId)
  assert.notEqual(first.candidateId, changed.candidateId)
  assert.equal(first.baseCommitId, source.commitId)
  assert.equal(first.baseTreeId, source.treeId)
  assert.equal(first.diffId, GitDiffId(createStrongFlowGitDiffId('diff fixture ')))
  assert.match(first.candidateId, /^candidate-sha256-[0-9a-f]{64}$/u)
  assert.match(
    verification.verificationSnapshotId,
    /^verification-sha256-[0-9a-f]{64}$/u,
  )
  assert.equal(verification.candidateId, first.candidateId)

  assert.throws(
    () => createStrongFlowCandidateIdentity({
      source: { ...source, sourceSnapshotId: `source-sha256-${'0'.repeat(64)}` },
      candidateCommitId,
      candidateTreeId,
      diffId: first.diffId,
    }),
    expectedWorkspaceError('INVALID_POLICY_INPUT'),
  )
  assert.throws(
    () => createStrongFlowVerificationSnapshotIdentity({
      ...first,
      candidateId: `candidate-sha256-${'f'.repeat(64)}`,
    }),
    expectedWorkspaceError('INVALID_POLICY_INPUT'),
  )
})

test('canonical layout assigns one exact directory to every role', () => {
  const source = admitStrongFlowSource(sourceState()).identity
  const frozenCandidate = candidate(source)
  const verification = createStrongFlowVerificationSnapshotIdentity(frozenCandidate)
  const layout = createStrongFlowWorkspaceLayout({
    home: '/runtime/home',
    jobId: JobId('job/path:fixture'),
    sourceSnapshot: source,
  })
  const stageRunId = StageRunId('run-workspace-fixture')

  assert.ok(layout.root.startsWith('/runtime/home/strongflow-workspaces/'))
  assert.equal(layout.root.includes('job/path:fixture'), false)
  assert.equal(layout.sourcePath, join(layout.root, 'source'))
  assert.equal(layout.candidatePath, join(layout.root, 'candidate'))
  assert.equal(layout.verificationRoot, join(layout.root, 'verification'))
  assert.equal(layout.metadataPath, join(layout.root, 'metadata'))
  assert.ok(Object.isFrozen(layout))
  assert.ok(Object.isFrozen(STRONGFLOW_WORKSPACE_MODE_BY_ROLE))

  for (const roleId of ['requirements', 'solution', 'planner']) {
    const assigned = strongFlowRoleWorkspace({ layout, roleId, stageRunId })
    assert.equal(assigned.mode, 'source-read-only')
    assert.equal(assigned.path, layout.sourcePath)
    assert.equal(assigned.stageRunId, stageRunId)
  }
  for (const roleId of ['executor', 'remediator']) {
    const assigned = strongFlowRoleWorkspace({
      layout,
      roleId,
      stageRunId,
      candidate: frozenCandidate,
    })
    assert.equal(assigned.mode, 'candidate-write')
    assert.equal(assigned.path, layout.candidatePath)
    assert.equal(assigned.candidateId, frozenCandidate.candidateId)
    assert.equal(assigned.stageRunId, stageRunId)
  }
  const verificationPaths = []
  for (const roleId of ['reviewer', 'verifier', 'adversarial-verifier']) {
    const assigned = strongFlowRoleWorkspace({
      layout,
      roleId,
      stageRunId,
      candidate: frozenCandidate,
      verificationSnapshot: verification,
    })
    assert.equal(assigned.mode, 'candidate-read-only')
    assert.ok(assigned.path.startsWith(`${layout.verificationRoot}/`))
    assert.ok(assigned.temporaryOutputPath.startsWith(`${layout.root}/verification-output/`))
    assert.equal(assigned.verificationSnapshotId, verification.verificationSnapshotId)
    assert.equal(assigned.stageRunId, stageRunId)
    verificationPaths.push(assigned.path)
  }
  assert.equal(new Set(verificationPaths).size, 3)
  const repeatedRole = strongFlowRoleWorkspace({
    layout,
    roleId: 'reviewer',
    stageRunId: StageRunId('run-workspace-fixture-next'),
    candidate: frozenCandidate,
    verificationSnapshot: verification,
  })
  assert.notEqual(repeatedRole.path, verificationPaths[0])
  assert.throws(
    () => strongFlowVerificationWorkspacePaths(
      layout,
      verification.verificationSnapshotId,
      'reviewer',
      '',
    ),
    expectedWorkspaceError('INVALID_POLICY_INPUT'),
  )
  assert.throws(
    () => strongFlowVerificationWorkspacePaths(
      layout,
      'verification-sha256-invalid',
      'reviewer',
      stageRunId,
    ),
    expectedWorkspaceError('INVALID_POLICY_INPUT'),
  )
  assert.throws(
    () => strongFlowRoleWorkspace({ layout, roleId: 'reviewer', stageRunId }),
    expectedWorkspaceError('VERIFICATION_SNAPSHOT_REQUIRED'),
  )
  assert.throws(
    () => strongFlowRoleWorkspace({ layout, roleId: 'remediator', stageRunId }),
    expectedWorkspaceError('CANDIDATE_MISMATCH'),
  )
  assert.throws(
    () => strongFlowRoleWorkspace({
      layout,
      roleId: 'reviewer',
      stageRunId,
      verificationSnapshot: verification,
    }),
    expectedWorkspaceError('VERIFICATION_SNAPSHOT_REQUIRED'),
  )
  assert.throws(
    () => strongFlowRoleWorkspace({
      layout,
      roleId: 'reviewer',
      stageRunId,
      candidate: changedCandidate(source),
      verificationSnapshot: verification,
    }),
    expectedWorkspaceError('CANDIDATE_MISMATCH'),
  )
  assert.throws(
    () => strongFlowRoleWorkspace({
      layout: { ...layout, candidatePath: '/outside/candidate' },
      roleId: 'executor',
      stageRunId,
    }),
    expectedWorkspaceError('INVALID_POLICY_INPUT'),
  )
  const otherSource = admitStrongFlowSource(sourceState({
    commitId: '7'.repeat(40),
    treeId: '8'.repeat(40),
  })).identity
  assert.throws(
    () => strongFlowRoleWorkspace({
      layout,
      roleId: 'executor',
      stageRunId,
      candidate: candidate(otherSource),
    }),
    expectedWorkspaceError('CANDIDATE_MISMATCH'),
  )
  assert.throws(
    () => strongFlowRoleWorkspace({
      layout,
      roleId: 'reviewer',
      stageRunId,
      candidate: frozenCandidate,
      verificationSnapshot: {
        ...verification,
        verificationSnapshotId: `verification-sha256-${'e'.repeat(64)}`,
      },
    }),
    expectedWorkspaceError('INVALID_POLICY_INPUT'),
  )
})

function changedCandidate(source) {
  return candidate(source, 'mismatch')
}

test('portable relative paths reject traversal and symlink escape', async t => {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-containment-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const root = join(home, 'root')
  const inside = join(root, 'inside')
  const outside = join(home, 'outside')
  await mkdir(inside, { recursive: true })
  await mkdir(outside, { recursive: true })
  await writeFile(join(inside, 'inside.txt'), 'inside', 'utf8')
  await writeFile(join(outside, 'outside.txt'), 'outside', 'utf8')
  await symlink(outside, join(root, 'escape'))
  await symlink(inside, join(root, 'inside-link'))

  assert.equal(
    await resolveExistingStrongFlowWorkspacePath(root, 'inside/inside.txt'),
    await realpath(join(inside, 'inside.txt')),
  )
  assert.equal(
    await resolveExistingStrongFlowWorkspacePath(root, 'inside-link/inside.txt'),
    await realpath(join(inside, 'inside.txt')),
  )
  for (const path of [
    '../outside/outside.txt',
    '/tmp/outside.txt',
    'inside\\inside.txt',
    'inside//inside.txt',
    './inside/inside.txt',
  ]) {
    await assert.rejects(
      resolveExistingStrongFlowWorkspacePath(root, path),
      expectedWorkspaceError('PATH_TRAVERSAL'),
    )
  }
  await assert.rejects(
    resolveExistingStrongFlowWorkspacePath(root, 'escape/outside.txt'),
    expectedWorkspaceError('SYMLINK_ESCAPE'),
  )
  await assert.rejects(
    resolveExistingStrongFlowWorkspacePath(root, 'inside/missing.txt'),
    expectedWorkspaceError('PATH_NOT_FOUND'),
  )
})

test('one workspace permits at most one active executor or remediator lease', () => {
  const source = admitStrongFlowSource(sourceState()).identity
  const layout = createStrongFlowWorkspaceLayout({
    home: '/runtime/home',
    jobId: JobId('job-writer'),
    sourceSnapshot: source,
  })
  const request = {
    leaseId: 'lease-executor-1',
    jobId: layout.jobId,
    workspaceId: layout.workspaceId,
    roleId: 'executor',
    stageRunId: StageRunId('run-executor-1'),
    attemptId: AttemptId('attempt-executor-1'),
    acquiredAtMillis: 1_920_000_000_000,
  }
  const lease = claimStrongFlowCandidateWriter(undefined, request)
  assert.ok(Object.isFrozen(lease))
  assert.equal(claimStrongFlowCandidateWriter(lease, request), lease)

  assert.throws(
    () => claimStrongFlowCandidateWriter(lease, {
      ...request,
      leaseId: 'lease-remediator-1',
      roleId: 'remediator',
      stageRunId: StageRunId('run-remediator-1'),
      attemptId: AttemptId('attempt-remediator-1'),
    }),
    expectedWorkspaceError('WRITER_CONFLICT'),
  )
  assert.throws(
    () => claimStrongFlowCandidateWriter(undefined, {
      ...request,
      leaseId: 'lease-reviewer-1',
      roleId: 'reviewer',
    }),
    expectedWorkspaceError('WRITER_ROLE_DENIED'),
  )
  assert.throws(
    () => releaseStrongFlowCandidateWriter(lease, 'different-lease'),
    expectedWorkspaceError('WRITER_LEASE_MISMATCH'),
  )
  assert.equal(releaseStrongFlowCandidateWriter(lease, lease.leaseId), undefined)
  assert.throws(
    () => releaseStrongFlowCandidateWriter(undefined, lease.leaseId),
    expectedWorkspaceError('NO_ACTIVE_WRITER'),
  )
})

test('workspace cleanup never owns source repositories or durable evidence', () => {
  assert.deepEqual(STRONGFLOW_WORKSPACE_RETENTION_POLICY, {
    originalRepository: 'unmanaged-never-modified-or-deleted',
    sourceSnapshot: 'retain-until-job-terminal',
    candidateWorktree: 'retain-until-job-terminal-and-writer-released',
    verificationSnapshot: 'delete-after-associated-read-only-run-settles',
    durableArtifacts: 'outside-workspace-cleanup',
  })
  assert.ok(Object.isFrozen(STRONGFLOW_WORKSPACE_RETENTION_POLICY))
})
