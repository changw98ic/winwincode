import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { JobId } from '../packages/contracts/dist/index.js'
import {
  StrongFlowGitWorkspaceError,
  StrongFlowGitWorkspaceManager,
} from '../packages/strongflow/dist/index.js'

function runGit(repositoryPath, args, allowedExitCodes = [0]) {
  const result = spawnSync('git', ['-C', repositoryPath, ...args], {
    encoding: 'utf8',
    env: {
      ...process.env,
      GIT_TERMINAL_PROMPT: '0',
      LC_ALL: 'C',
    },
  })
  assert.equal(result.error, undefined, `Git could not start: ${result.error?.message}`)
  assert.ok(
    allowedExitCodes.includes(result.status ?? -1),
    `git ${args.join(' ')} failed (${result.status}): ${result.stderr}`,
  )
  return result.stdout.trim()
}

async function exists(path) {
  try {
    await lstat(path)
    return true
  } catch (error) {
    if (error?.code === 'ENOENT') return false
    throw error
  }
}

async function createFixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'winwincode-candidate-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const repositoryPath = join(root, 'repository')
  const home = join(root, 'runtime')
  await mkdir(join(repositoryPath, 'src'), { recursive: true })
  runGit(repositoryPath, ['init'])
  runGit(repositoryPath, ['config', 'user.email', 'fixture@winwincode.local'])
  runGit(repositoryPath, ['config', 'user.name', 'WinWinCode Fixture'])
  await writeFile(join(repositoryPath, 'src', 'base.txt'), 'base\n', 'utf8')
  await writeFile(join(repositoryPath, 'package.json'), '{"private":true}\n', 'utf8')
  runGit(repositoryPath, ['add', '--all'])
  runGit(repositoryPath, ['commit', '-m', 'Create candidate fixture'])
  return { root, repositoryPath, home }
}

function managerError(code) {
  return error => error instanceof StrongFlowGitWorkspaceError && error.code === code
}

function writerInput(suffix, roleId = 'executor') {
  return {
    leaseId: `lease-${suffix}`,
    roleId,
    stageRunId: `run-${suffix}`,
    attemptId: `attempt-${suffix}`,
  }
}

async function createWorkspace(t, jobId = 'job-candidate') {
  const fixture = await createFixture(t)
  const manager = new StrongFlowGitWorkspaceManager({ home: fixture.home })
  const workspace = await manager.create({
    jobId: JobId(jobId),
    repositoryPath: fixture.repositoryPath,
  })
  return { fixture, manager, workspace }
}

test('freezes one exact candidate with base, commit, tree, diff, and path scope', async t => {
  const { fixture, manager, workspace } = await createWorkspace(t, 'job-freeze')
  const writer = writerInput('freeze')
  const lease = await manager.claimWriter(workspace.manifest.jobId, writer)
  await writeFile(join(workspace.layout.candidatePath, 'src', 'base.txt'), 'changed\n', 'utf8')
  await writeFile(join(workspace.layout.candidatePath, 'src', 'added.txt'), 'added\n', 'utf8')

  await assert.rejects(
    manager.freezeCandidate(workspace.manifest.jobId, {
      scope: { mode: 'paths', paths: ['src'] },
    }),
    managerError('WRITER_ACTIVE'),
  )
  await manager.releaseWriter(workspace.manifest.jobId, lease.leaseId)
  const frozen = await manager.freezeCandidate(workspace.manifest.jobId, {
    scope: { mode: 'paths', paths: ['src'] },
  })

  assert.equal(frozen.candidate.baseCommitId, workspace.manifest.sourceSnapshot.commitId)
  assert.equal(frozen.candidate.baseTreeId, workspace.manifest.sourceSnapshot.treeId)
  assert.equal(
    frozen.candidate.candidateCommitId,
    runGit(workspace.layout.candidatePath, ['rev-parse', 'HEAD^{commit}']),
  )
  assert.equal(
    frozen.candidate.candidateTreeId,
    runGit(workspace.layout.candidatePath, ['rev-parse', 'HEAD^{tree}']),
  )
  assert.match(frozen.candidate.diffId, /^[0-9a-f]{64}$/u)
  assert.deepEqual(frozen.changedPaths, ['src/added.txt', 'src/base.txt'])
  assert.deepEqual(frozen.scope, { mode: 'paths', paths: ['src'] })
  assert.ok(frozen.diffByteLength > 0)
  assert.equal(runGit(workspace.layout.candidatePath, ['status', '--porcelain=v2']), '')
  assert.equal(
    await exists(join(workspace.layout.metadataPath, 'candidates', frozen.diffFileName)),
    true,
  )
  const exactDiff = new TextDecoder().decode(await manager.readFrozenCandidateDiff(
    workspace.manifest.jobId,
    frozen.candidate.candidateId,
  ))
  assert.match(exactDiff, /src\/added\.txt/u)
  assert.match(exactDiff, /src\/base\.txt/u)
  assert.deepEqual(
    await manager.inspectFrozenCandidate(workspace.manifest.jobId, frozen.candidate.candidateId),
    frozen,
  )

  const repeated = await manager.freezeCandidate(workspace.manifest.jobId, {
    scope: { mode: 'paths', paths: ['src'] },
  })
  assert.deepEqual(repeated, frozen)
  assert.equal(runGit(fixture.repositoryPath, ['status', '--porcelain=v2']), '')
  await manager.dispose(workspace.manifest.jobId)
})

test('rejects candidate paths outside the approved freeze scope', async t => {
  const { manager, workspace } = await createWorkspace(t, 'job-scope')
  const lease = await manager.claimWriter(workspace.manifest.jobId, writerInput('scope'))
  await writeFile(join(workspace.layout.candidatePath, 'src', 'base.txt'), 'in scope\n', 'utf8')
  await writeFile(join(workspace.layout.candidatePath, 'package.json'), '{"changed":true}\n', 'utf8')
  await manager.releaseWriter(workspace.manifest.jobId, lease.leaseId)

  await assert.rejects(
    manager.freezeCandidate(workspace.manifest.jobId, {
      scope: { mode: 'paths', paths: ['src'] },
    }),
    managerError('CANDIDATE_SCOPE_VIOLATION'),
  )
  const frozen = await manager.freezeCandidate(workspace.manifest.jobId, {
    scope: { mode: 'repository' },
  })
  assert.deepEqual(frozen.changedPaths, ['package.json', 'src/base.txt'])
  await manager.dispose(workspace.manifest.jobId)
})

test('gives each review run an independent disposable copy of the frozen candidate', async t => {
  const { manager, workspace } = await createWorkspace(t, 'job-verification')
  const executionLease = await manager.claimWriter(
    workspace.manifest.jobId,
    writerInput('verification'),
  )
  await writeFile(join(workspace.layout.candidatePath, 'src', 'base.txt'), 'candidate\n', 'utf8')
  await manager.releaseWriter(workspace.manifest.jobId, executionLease.leaseId)
  const first = await manager.freezeCandidate(workspace.manifest.jobId, {
    scope: { mode: 'repository' },
  })
  const reviewerInput = {
    candidateId: first.candidate.candidateId,
    roleId: 'reviewer',
    stageRunId: 'run-reviewer-first',
  }
  const verifierInput = {
    candidateId: first.candidate.candidateId,
    roleId: 'verifier',
    stageRunId: 'run-verifier-first',
  }

  const [reviewer, verifier] = await Promise.all([
    manager.createVerificationWorkspace(workspace.manifest.jobId, reviewerInput),
    manager.createVerificationWorkspace(workspace.manifest.jobId, verifierInput),
  ])
  assert.notEqual(reviewer.manifest.path, verifier.manifest.path)
  assert.notEqual(
    reviewer.manifest.temporaryOutputPath,
    verifier.manifest.temporaryOutputPath,
  )
  for (const snapshot of [reviewer, verifier]) {
    assert.equal(
      runGit(snapshot.manifest.path, ['rev-parse', 'HEAD^{commit}']),
      first.candidate.candidateCommitId,
    )
    assert.equal(
      runGit(snapshot.manifest.path, ['rev-parse', 'HEAD^{tree}']),
      first.candidate.candidateTreeId,
    )
  }

  await writeFile(join(reviewer.manifest.path, 'src', 'base.txt'), 'review note\n', 'utf8')
  await writeFile(join(reviewer.manifest.temporaryOutputPath, 'build.log'), 'output\n', 'utf8')
  assert.equal(
    await readFile(join(workspace.layout.candidatePath, 'src', 'base.txt'), 'utf8'),
    'candidate\n',
  )
  assert.equal(
    await readFile(join(verifier.manifest.path, 'src', 'base.txt'), 'utf8'),
    'candidate\n',
  )
  assert.deepEqual(
    await manager.inspectFrozenCandidate(workspace.manifest.jobId, first.candidate.candidateId),
    first,
  )
  assert.equal(
    (await manager.openVerificationWorkspace(workspace.manifest.jobId, reviewerInput))
      .manifest.path,
    reviewer.manifest.path,
  )
  await assert.rejects(
    manager.dispose(workspace.manifest.jobId),
    managerError('VERIFICATION_ACTIVE'),
  )

  const remediationLease = await manager.claimWriter(
    workspace.manifest.jobId,
    writerInput('remediation', 'remediator'),
  )
  await assert.rejects(
    manager.openVerificationWorkspace(workspace.manifest.jobId, reviewerInput),
    managerError('CANDIDATE_CHANGED'),
  )
  await writeFile(join(workspace.layout.candidatePath, 'src', 'added.txt'), 'remediated\n', 'utf8')
  await manager.releaseWriter(workspace.manifest.jobId, remediationLease.leaseId)
  await assert.rejects(
    manager.inspectFrozenCandidate(workspace.manifest.jobId, first.candidate.candidateId),
    managerError('CANDIDATE_CHANGED'),
  )
  const second = await manager.freezeCandidate(workspace.manifest.jobId, {
    scope: { mode: 'repository' },
  })
  assert.notEqual(second.candidate.candidateId, first.candidate.candidateId)
  await assert.rejects(
    manager.createVerificationWorkspace(workspace.manifest.jobId, reviewerInput),
    managerError('VERIFICATION_SNAPSHOT_MISMATCH'),
  )
  await assert.rejects(
    manager.createVerificationWorkspace(workspace.manifest.jobId, {
      ...reviewerInput,
      candidateId: `candidate-sha256-${'f'.repeat(64)}`,
      stageRunId: 'run-mismatched-candidate',
    }),
    managerError('VERIFICATION_SNAPSHOT_MISMATCH'),
  )

  assert.equal(
    (await manager.disposeVerificationWorkspace(workspace.manifest.jobId, reviewerInput)).status,
    'removed',
  )
  assert.equal(
    (await manager.disposeVerificationWorkspace(workspace.manifest.jobId, reviewerInput)).status,
    'absent',
  )
  assert.equal(
    (await manager.disposeVerificationWorkspace(workspace.manifest.jobId, verifierInput)).status,
    'removed',
  )
  assert.equal(await exists(reviewer.manifest.path), false)
  assert.equal(await exists(reviewer.manifest.temporaryOutputPath), false)
  assert.equal((await manager.dispose(workspace.manifest.jobId)).status, 'removed')
})
