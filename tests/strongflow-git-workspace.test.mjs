import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { once } from 'node:events'
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  symlink,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'
import { setTimeout as delay } from 'node:timers/promises'

import { JobId } from '../packages/contracts/dist/index.js'
import {
  StrongFlowGitWorkspaceError,
  StrongFlowGitWorkspaceManager,
  StrongFlowWorkspacePolicyError,
  strongFlowWorkspaceRootForJob,
} from '../packages/strongflow/dist/index.js'

const root = resolve(import.meta.dirname, '..')
const operationFixture = resolve(
  root,
  'tests/fixtures/strongflow-workspace-operation.mjs',
)

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
  const root = await mkdtemp(join(tmpdir(), 'winwincode-git-workspace-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const repositoryPath = join(root, 'repository')
  const home = join(root, 'runtime')
  const fixturePath = join(repositoryPath, 'fixture.bin')
  const fixtureBytes = Buffer.from('first revision\0content\n', 'utf8')
  await mkdir(repositoryPath)
  runGit(repositoryPath, ['init'])
  runGit(repositoryPath, ['config', 'user.email', 'fixture@winwincode.local'])
  runGit(repositoryPath, ['config', 'user.name', 'WinWinCode Fixture'])
  await writeFile(fixturePath, fixtureBytes)
  runGit(repositoryPath, ['add', '--', 'fixture.bin'])
  runGit(repositoryPath, ['commit', '-m', 'Create fixture'])
  const initialCommit = runGit(repositoryPath, ['rev-parse', 'HEAD^{commit}'])
  return {
    root,
    repositoryPath,
    home,
    fixturePath,
    fixtureBytes,
    initialCommit,
  }
}

async function sourceState(fixture) {
  return {
    bytes: await readFile(fixture.fixturePath),
    status: runGit(fixture.repositoryPath, [
      'status',
      '--porcelain=v2',
      '--untracked-files=normal',
    ]),
    commitId: runGit(fixture.repositoryPath, ['rev-parse', 'HEAD^{commit}']),
    treeId: runGit(fixture.repositoryPath, ['rev-parse', 'HEAD^{tree}']),
  }
}

async function assertSourceUnchanged(fixture, before) {
  const after = await sourceState(fixture)
  assert.deepEqual(after.bytes, before.bytes)
  assert.deepEqual(after, before)
}

function managerError(code) {
  return error => error instanceof StrongFlowGitWorkspaceError && error.code === code
}

function policyError(code) {
  return error => error instanceof StrongFlowWorkspacePolicyError && error.code === code
}

async function createInterruptingGitProxy(fixture, name) {
  const path = join(fixture.root, `interrupting-git-${name}`)
  await writeFile(path, `#!/bin/sh
mode="\${WWC_TEST_GIT_MODE:-}"
marker="\${WWC_TEST_GIT_MARKER:-}"
worktree=false
add=false
remove=false
candidate=false
source=false
commit_tree=false
for argument in "$@"; do
  if [ "$argument" = "worktree" ]; then worktree=true; fi
  if [ "$argument" = "add" ]; then add=true; fi
  if [ "$argument" = "remove" ]; then remove=true; fi
  if [ "$argument" = "commit-tree" ]; then commit_tree=true; fi
  case "$argument" in
    */candidate) candidate=true ;;
    */source) source=true ;;
  esac
done
if [ "$mode" = "create" ] && [ "$worktree" = true ] && [ "$add" = true ] && [ "$candidate" = true ]; then
  : > "$marker"
  exec sleep 60
fi
if [ "$mode" = "freeze" ] && [ "$commit_tree" = true ]; then
  : > "$marker"
  exec sleep 60
fi
if [ "$mode" = "dispose" ] && [ "$worktree" = true ] && [ "$remove" = true ] && [ "$source" = true ]; then
  git "$@"
  status=$?
  : > "$marker"
  sleep 60
  exit "$status"
fi
exec git "$@"
`, 'utf8')
  await chmod(path, 0o755)
  return path
}

function startWorkspaceOperation(t, fixture, operation, jobId, gitExecutable, markerPath) {
  const child = spawn(process.execPath, [
    operationFixture,
    operation,
    fixture.home,
    jobId,
    fixture.repositoryPath,
    gitExecutable,
  ], {
    cwd: root,
    detached: true,
    env: {
      ...process.env,
      WWC_TEST_GIT_MODE: operation,
      WWC_TEST_GIT_MARKER: markerPath,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  let stderr = ''
  child.stderr.setEncoding('utf8')
  child.stderr.on('data', chunk => {
    stderr += chunk
  })
  const running = { child, stderr: () => stderr }
  t.after(() => terminateWorkspaceOperation(running))
  return running
}

async function terminateWorkspaceOperation(running) {
  const { child } = running
  if (child.exitCode !== null || child.signalCode !== null) return
  assert.notEqual(child.pid, undefined)
  const closed = once(child, 'close')
  try {
    process.kill(-child.pid, 'SIGKILL')
  } catch (error) {
    if (error?.code !== 'ESRCH') throw error
  }
  const [, signal] = await closed
  assert.equal(signal, 'SIGKILL', running.stderr())
}

async function waitForMarker(path, running) {
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    if (await exists(path)) return
    if (running.child.exitCode !== null || running.child.signalCode !== null) {
      assert.fail(`workspace operation exited before its marker: ${running.stderr()}`)
    }
    await delay(10)
  }
  assert.fail(`workspace operation did not reach its marker: ${running.stderr()}`)
}

test('creates distinct detached worktrees and never changes the source checkout', async t => {
  const fixture = await createFixture(t)
  const before = await sourceState(fixture)
  let ownerSequence = 0
  const manager = new StrongFlowGitWorkspaceManager({
    home: fixture.home,
    clock: () => 1_920_000_000_000,
    ownerIdFactory: () => `owner-${ownerSequence += 1}`,
  })

  const first = await manager.create({
    jobId: JobId('job-worktree-first'),
    repositoryPath: fixture.repositoryPath,
  })
  const second = await manager.create({
    jobId: JobId('job-worktree-second'),
    repositoryPath: fixture.repositoryPath,
  })

  assert.notEqual(first.layout.root, second.layout.root)
  assert.notEqual(first.layout.sourcePath, first.layout.candidatePath)
  assert.equal(
    runGit(first.layout.sourcePath, ['symbolic-ref', '--quiet', 'HEAD'], [1]),
    '',
  )
  assert.equal(
    runGit(first.layout.candidatePath, ['symbolic-ref', '--quiet', 'HEAD'], [1]),
    '',
  )
  assert.equal(first.manifest.sourceSnapshot.commitId, before.commitId)
  assert.equal(first.manifest.sourceSnapshot.treeId, before.treeId)
  assert.deepEqual(await readFile(first.layout.sourcePath + '/fixture.bin'), before.bytes)

  await writeFile(join(first.layout.candidatePath, 'fixture.bin'), 'candidate change\n', 'utf8')
  const status = await manager.inspect(first.manifest.jobId)
  assert.equal(status.source.clean, true)
  assert.equal(status.candidate.clean, false)
  assert.match(status.candidate.status, /fixture\.bin/u)
  await assertSourceUnchanged(fixture, before)

  assert.deepEqual(await manager.dispose(first.manifest.jobId), {
    status: 'removed',
    root: first.layout.root,
  })
  assert.deepEqual(await manager.dispose(first.manifest.jobId), {
    status: 'absent',
    root: first.layout.root,
  })
  assert.equal((await manager.dispose(second.manifest.jobId)).status, 'removed')
  await assertSourceUnchanged(fixture, before)
})

test('resolves an explicit base revision without moving the source branch', async t => {
  const fixture = await createFixture(t)
  await writeFile(fixture.fixturePath, 'second revision\n', 'utf8')
  runGit(fixture.repositoryPath, ['add', '--', 'fixture.bin'])
  runGit(fixture.repositoryPath, ['commit', '-m', 'Advance source branch'])
  const before = await sourceState(fixture)
  const manager = new StrongFlowGitWorkspaceManager({ home: fixture.home })

  const handle = await manager.create({
    jobId: JobId('job-explicit-base'),
    repositoryPath: fixture.repositoryPath,
    revision: fixture.initialCommit,
  })

  assert.equal(handle.manifest.requestedRevision, fixture.initialCommit)
  assert.equal(handle.manifest.sourceSnapshot.commitId, fixture.initialCommit)
  assert.deepEqual(
    await readFile(join(handle.layout.sourcePath, 'fixture.bin')),
    fixture.fixtureBytes,
  )
  await assertSourceUnchanged(fixture, before)
  await manager.dispose(handle.manifest.jobId)
  await assertSourceUnchanged(fixture, before)
})

test('rejects invalid repositories, revisions, and dirty sources before creating a job root', async t => {
  const fixture = await createFixture(t)
  const manager = new StrongFlowGitWorkspaceManager({ home: fixture.home })
  const nonRepository = join(fixture.root, 'not-a-repository')
  const sourceSubdirectory = join(fixture.repositoryPath, 'source-subdirectory')
  await mkdir(nonRepository)
  await mkdir(sourceSubdirectory)

  for (const [jobId, repositoryPath, revision, expectedCode] of [
    ['job-invalid-repository', nonRepository, undefined, 'INVALID_REPOSITORY'],
    ['job-source-subdirectory', sourceSubdirectory, undefined, 'INVALID_REPOSITORY'],
    ['job-invalid-revision', fixture.repositoryPath, 'refs/heads/missing', 'INVALID_REVISION'],
  ]) {
    await assert.rejects(
      manager.create({
        jobId: JobId(jobId),
        repositoryPath,
        ...(revision === undefined ? {} : { revision }),
      }),
      managerError(expectedCode),
    )
    assert.equal(
      await exists(strongFlowWorkspaceRootForJob(fixture.home, jobId)),
      false,
    )
  }

  await writeFile(join(fixture.repositoryPath, 'untracked.txt'), 'dirty\n', 'utf8')
  await assert.rejects(
    manager.create({
      jobId: JobId('job-dirty-source'),
      repositoryPath: fixture.repositoryPath,
    }),
    managerError('INVALID_REPOSITORY'),
  )
  assert.equal(
    await exists(strongFlowWorkspaceRootForJob(fixture.home, 'job-dirty-source')),
    false,
  )
})

test('bounds Git command duration and output before workspace creation', async t => {
  const fixture = await createFixture(t)
  const hangingGit = join(fixture.root, 'hanging-git.mjs')
  const noisyGit = join(fixture.root, 'noisy-git.mjs')
  await writeFile(
    hangingGit,
    '#!/bin/sh\nexec sleep 60\n',
    'utf8',
  )
  await writeFile(
    noisyGit,
    '#!/bin/sh\nexec yes x\n',
    'utf8',
  )
  await Promise.all([chmod(hangingGit, 0o755), chmod(noisyGit, 0o755)])

  const timeoutManager = new StrongFlowGitWorkspaceManager({
    home: fixture.home,
    gitExecutable: hangingGit,
    commandTimeoutMillis: 50,
  })
  await assert.rejects(
    timeoutManager.inspectSource({ repositoryPath: fixture.repositoryPath }),
    managerError('GIT_COMMAND_TIMEOUT'),
  )

  const outputManager = new StrongFlowGitWorkspaceManager({
    home: fixture.home,
    gitExecutable: noisyGit,
    maxCommandOutputBytes: 128,
  })
  await assert.rejects(
    outputManager.inspectSource({ repositoryPath: fixture.repositoryPath }),
    managerError('GIT_OUTPUT_LIMIT'),
  )
})

test('detects source snapshot mutation while reporting candidate changes', async t => {
  const fixture = await createFixture(t)
  const manager = new StrongFlowGitWorkspaceManager({ home: fixture.home })
  const handle = await manager.create({
    jobId: JobId('job-source-mutation'),
    repositoryPath: fixture.repositoryPath,
  })

  await writeFile(join(handle.layout.sourcePath, 'fixture.bin'), 'forbidden source edit\n', 'utf8')
  await assert.rejects(
    manager.inspect(handle.manifest.jobId),
    managerError('SOURCE_SNAPSHOT_MUTATED'),
  )
  assert.equal((await manager.dispose(handle.manifest.jobId)).status, 'removed')
})

test('publishes one writer lease atomically and permits an idempotent retry', async t => {
  const fixture = await createFixture(t)
  const creator = new StrongFlowGitWorkspaceManager({ home: fixture.home })
  const handle = await creator.create({
    jobId: JobId('job-writer-race'),
    repositoryPath: fixture.repositoryPath,
  })
  const managers = [
    new StrongFlowGitWorkspaceManager({ home: fixture.home, clock: () => 100 }),
    new StrongFlowGitWorkspaceManager({ home: fixture.home, clock: () => 200 }),
  ]
  const claims = [
    {
      leaseId: 'lease-executor-a',
      roleId: 'executor',
      stageRunId: 'run-executor-a',
      attemptId: 'attempt-executor-a',
    },
    {
      leaseId: 'lease-executor-b',
      roleId: 'executor',
      stageRunId: 'run-executor-b',
      attemptId: 'attempt-executor-b',
    },
  ]

  const results = await Promise.allSettled(managers.map((manager, index) => (
    manager.claimWriter(handle.manifest.jobId, claims[index])
  )))
  assert.equal(results.filter(result => result.status === 'fulfilled').length, 1)
  assert.equal(results.filter(result => result.status === 'rejected').length, 1)
  const rejected = results.find(result => result.status === 'rejected')
  assert.ok(
    policyError('WRITER_CONFLICT')(rejected.reason),
    [
      `${rejected.reason?.name}: ${rejected.reason?.code}: ${rejected.reason?.message}`,
      `${rejected.reason?.cause?.name}: ${rejected.reason?.cause?.code}: ${rejected.reason?.cause?.message}`,
    ].join('\n'),
  )
  const winnerIndex = results.findIndex(result => result.status === 'fulfilled')
  const winner = results[winnerIndex].value

  const retryManager = new StrongFlowGitWorkspaceManager({
    home: fixture.home,
    clock: () => 999,
  })
  const retried = await retryManager.claimWriter(
    handle.manifest.jobId,
    claims[winnerIndex],
  )
  assert.deepEqual(retried, (await retryManager.inspect(handle.manifest.jobId)).writer)
  assert.equal(retried.acquiredAtMillis, winner.acquiredAtMillis)
  await assert.rejects(
    retryManager.dispose(handle.manifest.jobId),
    managerError('WRITER_ACTIVE'),
  )

  await retryManager.releaseWriter(handle.manifest.jobId, winner.leaseId)
  const remediation = await retryManager.claimWriter(handle.manifest.jobId, {
    leaseId: 'lease-remediator',
    roleId: 'remediator',
    stageRunId: 'run-remediator',
    attemptId: 'attempt-remediator',
  })
  assert.equal(remediation.roleId, 'remediator')
  await retryManager.releaseWriter(handle.manifest.jobId, remediation.leaseId)
  assert.equal((await retryManager.dispose(handle.manifest.jobId)).status, 'removed')
})

test('cleanup refuses a replaced worktree and does not touch its symlink target', async t => {
  const fixture = await createFixture(t)
  const manager = new StrongFlowGitWorkspaceManager({ home: fixture.home })
  const handle = await manager.create({
    jobId: JobId('job-safe-cleanup'),
    repositoryPath: fixture.repositoryPath,
  })
  const external = join(fixture.root, 'external')
  const externalMarker = join(external, 'keep.txt')
  await mkdir(external)
  await writeFile(externalMarker, 'keep\n', 'utf8')
  await rm(handle.layout.candidatePath, { recursive: true, force: true })
  await symlink(external, handle.layout.candidatePath)

  await assert.rejects(
    manager.dispose(handle.manifest.jobId),
    managerError('WORKSPACE_NOT_OWNED'),
  )
  assert.equal(await readFile(externalMarker, 'utf8'), 'keep\n')
  assert.equal(await exists(handle.layout.root), true)
  assert.equal(await exists(handle.layout.sourcePath), true)
})

test('retains an owned diagnostic root after partial Git creation failure', async t => {
  const fixture = await createFixture(t)
  const proxyPath = join(fixture.root, 'git-proxy.mjs')
  await writeFile(proxyPath, `#!/bin/sh
worktree=false
add=false
candidate=false
for argument in "$@"; do
  if [ "$argument" = "worktree" ]; then worktree=true; fi
  if [ "$argument" = "add" ]; then add=true; fi
  case "$argument" in
    */candidate) candidate=true ;;
  esac
done
if [ "$worktree" = true ] && [ "$add" = true ] && [ "$candidate" = true ]; then
  printf '%s\\n' 'fixture candidate creation failure' >&2
  exit 73
fi
exec git "$@"
`, 'utf8')
  await chmod(proxyPath, 0o755)
  const manager = new StrongFlowGitWorkspaceManager({
    home: fixture.home,
    gitExecutable: proxyPath,
  })
  const jobId = JobId('job-retained-failure')
  const root = strongFlowWorkspaceRootForJob(fixture.home, jobId)
  const before = await sourceState(fixture)

  let failure
  try {
    await manager.create({ jobId, repositoryPath: fixture.repositoryPath })
  } catch (error) {
    failure = error
  }
  assert.ok(managerError('GIT_COMMAND_FAILED')(failure))
  assert.equal(failure.retainedWorkspacePath, root)
  assert.equal(await exists(join(root, 'metadata', 'owner.json')), true)
  assert.equal(await exists(join(root, 'metadata', 'failure.json')), true)
  assert.equal(await exists(join(root, 'metadata', 'manifest.json')), false)
  assert.equal(await exists(join(root, 'source')), true)
  await assertSourceUnchanged(fixture, before)

  assert.equal((await manager.dispose(jobId)).status, 'removed')
  assert.equal(await exists(root), false)
  await assertSourceUnchanged(fixture, before)
})

test('concurrent starts publish one owned workspace and preserve the source', async t => {
  const fixture = await createFixture(t)
  const before = await sourceState(fixture)
  const managers = [
    new StrongFlowGitWorkspaceManager({ home: fixture.home }),
    new StrongFlowGitWorkspaceManager({ home: fixture.home }),
  ]
  const jobId = JobId('job-concurrent-create')
  const results = await Promise.allSettled(managers.map(manager => manager.create({
    jobId,
    repositoryPath: fixture.repositoryPath,
  })))

  assert.equal(results.filter(result => result.status === 'fulfilled').length, 1)
  assert.equal(results.filter(result => result.status === 'rejected').length, 1)
  const rejected = results.find(result => result.status === 'rejected')
  assert.ok(managerError('WORKSPACE_EXISTS')(rejected.reason))
  const opened = await managers[0].open(jobId)
  assert.equal(opened.manifest.jobId, jobId)
  assert.deepEqual(await managers[0].reconcile(jobId), {
    status: 'ready',
    root: opened.layout.root,
    operationLock: 'none',
    retainedWorkspacePath: opened.layout.root,
  })
  await assertSourceUnchanged(fixture, before)
  assert.equal((await managers[1].dispose(jobId)).status, 'removed')
  await assertSourceUnchanged(fixture, before)
})

test('an exact nested repository remains isolated from its clean parent', async t => {
  const fixture = await createFixture(t)
  const nestedPath = join(fixture.repositoryPath, 'nested')
  const nestedFile = join(nestedPath, 'nested.txt')
  await mkdir(nestedPath)
  runGit(nestedPath, ['init'])
  runGit(nestedPath, ['config', 'user.email', 'nested@winwincode.local'])
  runGit(nestedPath, ['config', 'user.name', 'Nested Fixture'])
  await writeFile(nestedFile, 'nested source\n', 'utf8')
  runGit(nestedPath, ['add', '--all'])
  runGit(nestedPath, ['commit', '-m', 'Create nested fixture'])
  runGit(fixture.repositoryPath, ['add', '--', 'nested'])
  runGit(fixture.repositoryPath, ['commit', '-m', 'Track nested repository'])
  const parentBefore = await sourceState(fixture)
  const nestedFixture = {
    repositoryPath: nestedPath,
    fixturePath: nestedFile,
  }
  const nestedBefore = await sourceState(nestedFixture)
  const manager = new StrongFlowGitWorkspaceManager({ home: fixture.home })

  const handle = await manager.create({
    jobId: JobId('job-nested-repository'),
    repositoryPath: nestedPath,
  })
  assert.equal(handle.manifest.repositoryPath, await realpath(nestedPath))
  await writeFile(join(handle.layout.candidatePath, 'nested.txt'), 'nested candidate\n', 'utf8')
  await assertSourceUnchanged(fixture, parentBefore)
  await assertSourceUnchanged(nestedFixture, nestedBefore)
  await manager.dispose(handle.manifest.jobId)
  await assertSourceUnchanged(fixture, parentBefore)
  await assertSourceUnchanged(nestedFixture, nestedBefore)
})

test('reconciles a process killed during workspace creation into safe cleanup', async t => {
  const fixture = await createFixture(t)
  const before = await sourceState(fixture)
  const jobId = JobId('job-killed-create')
  const marker = join(fixture.root, 'killed-create.marker')
  const proxy = await createInterruptingGitProxy(fixture, 'create')
  const running = startWorkspaceOperation(t, fixture, 'create', jobId, proxy, marker)
  await waitForMarker(marker, running)
  await terminateWorkspaceOperation(running)

  const manager = new StrongFlowGitWorkspaceManager({ home: fixture.home })
  const result = await manager.reconcile(jobId)
  assert.deepEqual(result, {
    status: 'cleanup-required',
    root: strongFlowWorkspaceRootForJob(fixture.home, jobId),
    operationLock: 'none',
    retainedWorkspacePath: strongFlowWorkspaceRootForJob(fixture.home, jobId),
  })
  assert.equal((await manager.dispose(jobId)).status, 'removed')
  assert.equal((await manager.reconcile(jobId)).status, 'absent')
  await assertSourceUnchanged(fixture, before)
})

test('reclaims a dead freeze lock and deterministically freezes the candidate again', async t => {
  const fixture = await createFixture(t)
  const before = await sourceState(fixture)
  const manager = new StrongFlowGitWorkspaceManager({ home: fixture.home })
  const handle = await manager.create({
    jobId: JobId('job-killed-freeze'),
    repositoryPath: fixture.repositoryPath,
  })
  const lease = await manager.claimWriter(handle.manifest.jobId, {
    leaseId: 'lease-killed-freeze',
    roleId: 'executor',
    stageRunId: 'run-killed-freeze',
    attemptId: 'attempt-killed-freeze',
  })
  await writeFile(join(handle.layout.candidatePath, 'fixture.bin'), 'candidate after crash\n')
  await manager.releaseWriter(handle.manifest.jobId, lease.leaseId)
  const marker = join(fixture.root, 'killed-freeze.marker')
  const proxy = await createInterruptingGitProxy(fixture, 'freeze')
  const running = startWorkspaceOperation(
    t,
    fixture,
    'freeze',
    handle.manifest.jobId,
    proxy,
    marker,
  )
  await waitForMarker(marker, running)
  const killedProcessId = running.child.pid
  const active = await manager.reconcile(handle.manifest.jobId)
  assert.equal(active.status, 'operation-active')
  assert.equal(active.operationLock, 'active')
  assert.equal(active.operationProcessId, killedProcessId)
  await terminateWorkspaceOperation(running)

  const reconciliation = await manager.reconcile(handle.manifest.jobId)
  assert.equal(reconciliation.status, 'ready')
  assert.equal(reconciliation.operationLock, 'reclaimed')
  assert.equal(reconciliation.operationProcessId, killedProcessId)
  assert.equal(reconciliation.retainedWorkspacePath, handle.layout.root)
  const frozen = await manager.freezeCandidate(handle.manifest.jobId, {
    scope: { mode: 'repository' },
  })
  assert.deepEqual(frozen.changedPaths, ['fixture.bin'])
  assert.equal((await manager.dispose(handle.manifest.jobId)).status, 'removed')
  await assertSourceUnchanged(fixture, before)
})

test('reclaims a dead cleanup lock and completes the retained cleanup', async t => {
  const fixture = await createFixture(t)
  const before = await sourceState(fixture)
  const manager = new StrongFlowGitWorkspaceManager({ home: fixture.home })
  const handle = await manager.create({
    jobId: JobId('job-killed-cleanup'),
    repositoryPath: fixture.repositoryPath,
  })
  const marker = join(fixture.root, 'killed-cleanup.marker')
  const proxy = await createInterruptingGitProxy(fixture, 'dispose')
  const running = startWorkspaceOperation(
    t,
    fixture,
    'dispose',
    handle.manifest.jobId,
    proxy,
    marker,
  )
  await waitForMarker(marker, running)
  assert.equal(await exists(handle.layout.sourcePath), false)
  const killedProcessId = running.child.pid
  await terminateWorkspaceOperation(running)

  const reconciliation = await manager.reconcile(handle.manifest.jobId)
  assert.equal(reconciliation.status, 'cleanup-required')
  assert.equal(reconciliation.operationLock, 'reclaimed')
  assert.equal(reconciliation.operationProcessId, killedProcessId)
  assert.equal(reconciliation.retainedWorkspacePath, handle.layout.root)
  assert.equal((await manager.dispose(handle.manifest.jobId)).status, 'removed')
  assert.equal((await manager.dispose(handle.manifest.jobId)).status, 'absent')
  await assertSourceUnchanged(fixture, before)
})
