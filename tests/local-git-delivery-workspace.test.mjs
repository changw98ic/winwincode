import assert from 'node:assert/strict'
import { execFile } from 'node:child_process'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { promisify } from 'node:util'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseDelivery,
} from '../packages/contracts/dist/index.js'
import {
  LocalGitDeliveryWorkspace,
} from '../packages/strongflow/dist/index.js'

const exec = promisify(execFile)
const now = 3_100_000_000_000

async function git(repository, ...args) {
  return (await exec('git', ['-C', repository, ...args], { encoding: 'utf8' })).stdout.trim()
}

function deliveryFixture(repository, baseRevision, { reviewer = false } = {}) {
  const deliveryId = 'delivery-local-git-workspace'
  const writer = {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'stage-local-git-executor',
    deliveryId,
    deliveryTaskId: null,
    stage: 'executing',
    actorType: 'codex',
    role: 'executor',
    status: reviewer ? 'succeeded' : 'running',
    attempt: 1,
    startedAtMillis: now + 1,
    finishedAtMillis: reviewer ? now + 3 : null,
  }
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: reviewer ? 6 : 4,
    status: reviewer ? 'verifying' : 'executing',
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'spec-local-git-workspace',
      deliveryId,
      revision: 1,
      title: 'Isolate one candidate',
      goal: 'Keep the source checkout untouched while Codex writes a candidate.',
      scope: ['candidate worktree'],
      outOfScope: ['source checkout changes'],
      constraints: ['base revision remains pinned'],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: 'criterion-local-git-workspace',
        description: 'Candidate contains the expected file change.',
        verificationMethod: 'Read the frozen Git diff.',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: repository,
      },
      baseRevision,
      maxReworkAttempts: 2,
      createdAtMillis: now,
    },
    tasks: [],
    stageRuns: reviewer ? [writer, {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'stage-local-git-reviewer',
      deliveryId,
      deliveryTaskId: null,
      stage: 'verifying',
      actorType: 'codex',
      role: 'reviewer',
      status: 'running',
      attempt: 1,
      startedAtMillis: now + 3,
      finishedAtMillis: null,
    }] : [writer],
    sessionBindings: reviewer ? [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'binding-local-git-executor',
      deliveryId,
      stageRunId: writer.id,
      dshSessionId: 'dsh-local-git-executor',
      codexSessionId: 'codex-local-git-executor',
      boundAtMillis: now + 2,
    }] : [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'binding-local-git-executor',
      deliveryId,
      stageRunId: writer.id,
      dshSessionId: 'dsh-local-git-executor',
      codexSessionId: 'codex-local-git-executor',
      boundAtMillis: now + 2,
    }],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 3,
  })
}

test('local Git Delivery workspace freezes an isolated candidate and rebuilds it after restart', async t => {
  const root = await mkdtemp(join(tmpdir(), 'winwincode-local-git-workspace-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  const repository = join(root, 'source')
  const home = join(root, 'home')
  await exec('git', ['init', repository])
  await git(repository, 'config', 'user.name', 'Fixture')
  await git(repository, 'config', 'user.email', 'fixture@example.test')
  await writeFile(join(repository, 'value.txt'), 'before\n')
  await git(repository, 'add', 'value.txt')
  await git(repository, 'commit', '-m', 'base')
  const baseRevision = await git(repository, 'rev-parse', 'HEAD')
  const sourceBranch = await git(repository, 'branch', '--show-current')
  const delivery = deliveryFixture(repository, baseRevision)
  const manager = new LocalGitDeliveryWorkspace({ home })

  const prepared = await manager.prepare(delivery)
  assert.notEqual(prepared.path, repository)
  assert.equal(await readFile(join(prepared.path, 'value.txt'), 'utf8'), 'before\n')
  await writeFile(join(prepared.path, 'value.txt'), 'after\n')
  await writeFile(join(prepared.path, 'added.txt'), 'new\n')
  const facts = await manager.freezeCandidateFacts(delivery)

  assert.equal(facts.baseCommitId, baseRevision)
  assert.notEqual(facts.candidateCommitId, baseRevision)
  assert.match(facts.unifiedDiff, /diff --git a\/value\.txt b\/value\.txt/u)
  assert.deepEqual(facts.changedPaths.map(entry => entry.path), ['added.txt', 'value.txt'])
  assert.equal(await git(repository, 'rev-parse', 'HEAD'), baseRevision)
  assert.equal(await git(repository, 'branch', '--show-current'), sourceBranch)
  assert.equal(await git(repository, 'status', '--porcelain=v1'), '')
  assert.equal(await manager.currentCandidate(delivery), null)

  const reviewing = deliveryFixture(repository, baseRevision, { reviewer: true })
  const candidate = await manager.currentCandidate(reviewing)
  assert.ok(candidate)
  assert.equal(candidate.candidateCommitId, facts.candidateCommitId)
  assert.equal(candidate.producerStageRunId, 'stage-local-git-executor')
  await manager.assertCandidate(reviewing, candidate)

  const reopened = new LocalGitDeliveryWorkspace({ home })
  assert.deepEqual(await reopened.currentCandidate(reviewing), candidate)
})
