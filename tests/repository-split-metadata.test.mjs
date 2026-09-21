import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { appendFileSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  validateRepositorySplitMetadata,
  validateRepositorySplitFiles,
} from '../scripts/verify-repository-split-metadata.mjs'

const root = resolve(import.meta.dirname, '..')
const verifierPath = resolve(root, 'scripts/verify-repository-split-metadata.mjs')

function readJson(path) {
  return JSON.parse(readFileSync(resolve(root, path), 'utf8'))
}

function fixture() {
  return structuredClone({
    editions: readJson('docs/decisions/0031-product-editions.json'),
    inventory: readJson('docs/decisions/0031-repository-split.inventory.json'),
    taskMap: readJson('docs/decisions/0031-task-repository-map.json'),
  })
}

function rejected(mutate, pattern) {
  const documents = fixture()
  mutate(documents)
  assert.throws(
    () => validateRepositorySplitMetadata(documents),
    error => {
      assert.match(error.message, /^REPOSITORY_SPLIT_METADATA_INVALID:/u)
      assert.match(error.message, pattern)
      return true
    },
  )
}

function currentTrackedPaths() {
  return trackedPathsAt(root, readJson('docs/decisions/0031-repository-split.inventory.json'))
}

function trackedPathsAt(repositoryRoot, inventory) {
  return execFileSync('git', ['ls-files', '-z'], { cwd: repositoryRoot })
    .toString()
    .split('\0')
    .filter(Boolean)
    .filter(path => inventory.currentHead.includedRoots.includes(path.split('/')[0]))
}

function rejectedAgainstCurrentHead(mutate, pattern) {
  const documents = fixture()
  mutate(documents)
  assert.throws(
    () => validateRepositorySplitMetadata({
      ...documents,
      repositoryRoot: root,
      trackedPaths: currentTrackedPaths(),
    }),
    error => {
      assert.match(error.message, /^REPOSITORY_SPLIT_METADATA_INVALID:/u)
      assert.match(error.message, pattern)
      return true
    },
  )
}

const EXPECTED_VALIDATION_RESULT = {
  repositoryIds: ['winwincode', 'winwincode-cloud', 'winwincode-enterprise'],
  stableTasks: 178,
  targetTasks: 49,
  cloudTasks: 32,
  enterpriseTasks: 17,
  mixedSeams: 12,
  enterpriseDirectPaths: 0,
  communityPrunePermission: 'community-owned-ready-for-1.1-freeze',
}

// Documented re-audit pins (keep lockstep with scripts/verify-repository-split-metadata.mjs)
// 2026-09-21 follow-up on accepted main a878d4c5 after JEV/fusion/device merges.
const AUDITED_COMMUNITY_HEAD = 'a878d4c53b927c80357becb29bed7e897a1c9b16'
const AUDITED_MIGRATE_OUT_RECOVERY_HEAD = '52f2eda69fd0b03e6f42b980be3b9a4965092497'
const AUDITED_CURRENT_HEAD_TRACKED_FILE_COUNT = 1234
const AUDITED_OWNERSHIP_LOCKSTEP_PATHS = [
  'apps/client/src/public-redaction.ts',
  'crates/winwincode-client-port/src/managed_app.rs',
  'crates/winwincode-codex/src/diagnostic_artifact_outbox.rs',
  'crates/winwincode-codex/src/parallel_model_runner.rs',
  'crates/winwincode-control-plane/src/fusion_adjudication_host.rs',
  'crates/winwincode-control-plane/src/fusion_analysis.rs',
  'crates/winwincode-control-plane/src/fusion_compose.rs',
  'crates/winwincode-control-plane/src/page_annotation_delivery.rs',
  'crates/winwincode-device-client/src/managed_app.rs',
  'crates/winwincode-execution-port/src/jev_decision.rs',
  'crates/winwincode-fusion/Cargo.toml',
  'crates/winwincode-fusion/src/contract.rs',
  'crates/winwincode-fusion/src/lib.rs',
  'crates/winwincode-fusion/src/panel.rs',
  'crates/winwincode-fusion/src/provider.rs',
  'crates/winwincode-fusion/tests/blind_panel.rs',
  'crates/winwincode-provider/src/jev.rs',
  'crates/winwincode-provider/tests/jev_openjev.rs',
  'crates/winwincode-storage/src/managed_app.rs',
  'docs/jev-session-replay.md',
  'packages/contracts/src/fusion.ts',
  'scripts/device-production-fixture.mjs',
  'scripts/evaluate-jev-session-replay.mjs',
  'scripts/run-00os-device-live-vertical.mjs',
  'tests/api-production-device-prerequisites.test.mjs',
  'tests/fixtures/jev-session-replay/fixture-evidence-detgc.txt',
  'tests/fixtures/jev-session-replay/fixture-evidence-gcjev.txt',
  'tests/fixtures/jev-session-replay/fixture-evidence.txt',
  'tests/fixtures/jev-session-replay/phase1.skeleton.json',
  'tests/jev-session-replay.test.mjs',
  'tests/projects-page-ui.test.mjs',
]

test('current repository split documents agree on repositories, tasks, and source counts', () => {
  assert.deepEqual(validateRepositorySplitMetadata(fixture()), EXPECTED_VALIDATION_RESULT)
})

test('current HEAD tracked paths have exactly one ownership bucket', () => {
  assert.deepEqual(validateRepositorySplitFiles(root), EXPECTED_VALIDATION_RESULT)
})

test('target evidence is seam-scoped and cannot authorize Community deletion', () => {
  const inventory = fixture().inventory
  const evidence = inventory.currentHeadTargetEvidence
  assert.equal(
    inventory.currentHead.trackedFileCount,
    AUDITED_CURRENT_HEAD_TRACKED_FILE_COUNT,
  )
  assert.equal(inventory.currentHead.gitHead, AUDITED_COMMUNITY_HEAD)
  assert.deepEqual(inventory.currentHead.includedRoots, [
    'apps', 'packages', 'crates', 'schema', 'scripts', 'tests', 'docs',
  ])
  assert.deepEqual(evidence.currentDirectMigrations, [])
  assert.equal(evidence.mixedSeamEvidence.length, 12)
  assert.equal(evidence.migrationReviewPathCount, 250)
  assert.equal(evidence.deletionPermission, 'none')
  assert.equal(evidence.deletionPermissionMeaning.includes('community-first'), true)
  assert.equal(evidence.crossRepositoryEvidenceRole, 'observational-only-not-a-community-prune-gate')
  assert.equal(evidence.migrationReviewRequiresSeparateOwnershipDecision, false)
  assert.equal(evidence.communityPrunePermission, 'community-owned-ready-for-1.1-freeze')
  assert.equal(evidence.sourcePins.cloud.sourceHeadMatches, true)
  assert.equal(evidence.sourcePins.enterprise.sourceHeadMatches, true)
  assert.equal(evidence.reconciliationSummary.cloudPassCount, 12)
  assert.equal(evidence.reconciliationSummary.enterprisePassCount, 2)
  assert.equal(evidence.reconciliationSummary.enterpriseGapCount, 10)
  assert.equal(evidence.reconciliationSummary.overallPassCount, 2)
  assert.equal(evidence.reconciliationSummary.overallGapCount, 10)
  assert.ok(evidence.mixedSeamEvidence.every(entry => (
    entry.status === 'target-shape-observation-only'
    && entry.deletionPermission === false
    && entry.targetRepositories.length === 2
  )))
  assert.deepEqual(
    evidence.mixedSeamEvidence.map(entry => entry.reconciliationStatus),
    ['gap', 'pass', 'gap', 'gap', 'gap', 'gap', 'gap', 'gap', 'gap', 'pass', 'gap', 'gap'],
  )
})

test('community-owned disposition freezes retain / rewrite / migrate-out scopes', () => {
  const inventory = fixture().inventory
  const disposition = inventory.currentHeadCommunityDisposition
  assert.equal(disposition.status, 'current-head-community-disposition-complete')
  assert.equal(disposition.userSequencing.decision, '2026-09-19 community-first')
  assert.equal(disposition.productBoundary.multiTenant, false)
  assert.equal(disposition.productBoundary.owners, 'exactly one local Owner')
  assert.ok(disposition.productBoundary.retainCapabilities.includes('DSH chat UI'))
  assert.ok(disposition.productBoundary.retainCapabilities.includes('StrongFlow advanced UI'))
  assert.ok(disposition.productBoundary.retainCapabilities.includes('embedded Codex Core'))
  const mr = disposition.migrationReviewDispositions
  assert.equal(Object.keys(mr).length, 252)
  assert.deepEqual(disposition.dispositionCounts.migrationReviewActions, {
    retain: 148,
    'rewrite-mixed': 12,
    'retain-and-narrow': 85,
    'migrate-out': 2,
    'retain-historical': 5,
  })
  assert.equal(mr['crates/winwincode-control-plane/src/vault_kms_network.rs'].action, 'migrate-out')
  assert.equal(
    mr['crates/winwincode-control-plane/src/vault_kms_network.rs'].recoverableSource.gitHead,
    AUDITED_MIGRATE_OUT_RECOVERY_HEAD,
  )
  assert.equal(
    mr['crates/winwincode-control-plane/src/vault_kms_network.rs'].execution.state,
    'executed-removed-from-community-worktree',
  )
  assert.equal(mr['crates/winwincode-domain/src/user_account.rs'].action, 'retain-and-narrow')
  assert.equal(mr['apps/client/src/application.ts'].action, 'rewrite-mixed')
  assert.equal(
    mr['apps/client/src/application.ts'].rewritePlan.communityCanonicalPath,
    'apps/client/src/application.ts',
  )
  assert.equal(disposition.mixedSeamRewritePlans.length, 12)
  const permission = disposition.communityPrunePermission
  assert.equal(permission.status, 'community-owned-ready-for-1.1-freeze')
  assert.equal(permission.cloudEnterpriseTargetAcceptanceRequired, false)
  assert.equal(permission.formalCoreLockRequired, false)
  assert.equal(permission.coreReleaseRequired, false)
  assert.deepEqual(permission.currentMigrateOutPathsStillPresent, [])
  assert.equal(
    permission.pruneExecution.status,
    'migrate-out-executed-community-surface-narrowed-protocol-scope-keys-retained-localdefault-identity-clean-checkout-reaudit-metadata-lockstep',
  )
  assert.equal(
    permission.pruneExecution.residualProtocolScopeKeyDecision.decision,
    'keep-protocol-keys-product-identity-localdefault-scope',
  )
  assert.equal(
    permission.pruneExecution.cleanCheckoutReadiness.requiresCommitForAcPass,
    true,
  )
  assert.equal(
    permission.pruneExecution.cleanCheckoutReadiness.acceptedCommunityHead,
    AUDITED_COMMUNITY_HEAD,
  )
  assert.deepEqual(
    permission.pruneExecution.cleanCheckoutReadiness.ownershipLockstepPathsAdded,
    AUDITED_OWNERSHIP_LOCKSTEP_PATHS,
  )
  assert.deepEqual(
    permission.pruneExecution.executedMigrateOut.map(entry => entry.path),
    [
      'crates/winwincode-control-plane/src/vault_kms_network.rs',
      'crates/winwincode-control-plane/tests/vault_kms_network.rs',
    ],
  )
  assert.equal(permission.historicalMigrateOutAlreadyAbsentCount, 89)
  assert.equal(disposition.worktreeIncrements.untrackedProductCandidates.length, 10)
  assert.match(disposition.worktreeIncrements.policy, /unfinished features are not complete/iu)
  assert.equal(disposition.auditedHead.gitHead, AUDITED_COMMUNITY_HEAD)
})

test('community prune permission gaps are rejected', async t => {
  await t.test('cloud/enterprise acceptance required again', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadCommunityDisposition.communityPrunePermission.cloudEnterpriseTargetAcceptanceRequired = true
      },
      /must not require Cloud\/Enterprise target acceptance/u,
    )
  })
  await t.test('core lock required again', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadCommunityDisposition.communityPrunePermission.formalCoreLockRequired = true
      },
      /must not require formal core.lock/u,
    )
  })
  await t.test('migration-review disposition omitted without execute record', () => {
    rejectedAgainstCurrentHead(
      documents => {
        delete documents.inventory.currentHeadCommunityDisposition.migrationReviewDispositions[
          'crates/winwincode-control-plane/src/vault_kms_network.rs'
        ]
        documents.inventory.currentHeadCommunityDisposition.dispositionCounts.migrationReviewActions['migrate-out'] = 1
      },
      /freeze migrate-out disposition must remain recorded|executed migrate-out path must keep a migrate-out freeze disposition/u,
    )
  })
  await t.test('migrate-out without recoverable source', () => {
    rejectedAgainstCurrentHead(
      documents => {
        delete documents.inventory.currentHeadCommunityDisposition.migrationReviewDispositions[
          'crates/winwincode-control-plane/src/vault_kms_network.rs'
        ].recoverableSource
      },
      /recoverableSource/u,
    )
  })
  await t.test('mixed seam rewrite plan missing communityKeeps', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadCommunityDisposition.migrationReviewDispositions[
          'apps/client/src/application.ts'
        ].rewritePlan.communityKeeps = []
      },
      /must record communityKeeps/u,
    )
  })
  await t.test('stale migrate-out still-present list is rejected after execution', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadCommunityDisposition.communityPrunePermission.currentMigrateOutPathsStillPresent = [
          'crates/winwincode-control-plane/src/vault_kms_network.rs',
          'crates/winwincode-control-plane/tests/vault_kms_network.rs',
        ]
      },
      /currentMigrateOutPathsStillPresent must match migrate-out disposition paths still on disk/u,
    )
  })
})

test('current HEAD drift and forged target evidence are rejected', async t => {
  await t.test('tracked path digest drift', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHead.trackedPathListSha256 = 'forged'
      },
      /current HEAD tracked path hash/u,
    )
  })
  await t.test('ownership path drift', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.ownership.winwincode.pop()
      },
      /unowned tracked paths/u,
    )
  })
  await t.test('snapshot is not target evidence', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadTargetEvidence.mixedSeamEvidence[0].cloud.tests = [
          'upstream/source-snapshots/fake.test.mjs',
        ]
      },
      /uses a source snapshot/u,
    )
  })
  await t.test('all declared source pins cannot replace actual Community HEAD/tree', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHead.gitHead = 'forged-head'
        documents.inventory.currentHead.gitTree = 'forged-tree'
        const evidence = documents.inventory.currentHeadTargetEvidence
        evidence.sourcePins.cloud.sourceAudit.gitHead = 'forged-head'
        evidence.sourcePins.cloud.sourceAudit.gitTree = 'forged-tree'
        evidence.sourcePins.enterprise.sourcePin.gitHead = 'forged-head'
        evidence.sourcePins.enterprise.sourcePin.gitTree = 'forged-tree'
        for (const entry of evidence.mixedSeamEvidence) {
          entry.sourceHead.gitHead = 'forged-head'
          entry.sourceHead.gitTree = 'forged-tree'
        }
      },
      /inventory current HEAD must match audited Community baseline/u,
    )
  })
  await t.test('tracked file root counts are derived from the current path list', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHead.trackedFileCountByRoot.apps += 1
      },
      /tracked file count by root/u,
    )
  })
  await t.test('bucket root counts are derived from ownership paths', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadClassification.classifiedPathCountByBucketAndRoot.winwincode.apps += 1
      },
      /current HEAD winwincode classified path counts by root/u,
    )
  })
  await t.test('summary counts are derived from ownership paths', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.summary.classifiedPathCountByBucket['migration-review'] += 1
      },
      /summary classified path counts by bucket/u,
    )
  })
  await t.test('baseline root counts are derived from the baseline git tree', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.baseline.trackedFileCountByRoot.apps += 1
      },
      /baseline tracked file count by root/u,
    )
  })
})

function cloneRepository() {
  const parent = mkdtempSync(join(tmpdir(), 'winwincode-repository-split-clone-'))
  const repositoryRoot = join(parent, 'repo')
  execFileSync('git', ['clone', '--no-local', root, repositoryRoot], { stdio: 'ignore' })
  execFileSync('git', ['-C', repositoryRoot, 'config', 'user.email', 'split-test@example.invalid'])
  execFileSync('git', ['-C', repositoryRoot, 'config', 'user.name', 'repository-split-test'])
  return { parent, repositoryRoot }
}

test('metadata-only commit may advance live HEAD while audited product tree stays fixed', () => {
  const { parent, repositoryRoot } = cloneRepository()
  try {
    // Live HEAD may already be a metadata-only descendant of the audited
    // Community head (a878d4c5). Product-tree authority stays on the audited
    // baseline: inventory currentHead.gitHead remains pinned, and the clone
    // must still carry the authorized migrate-out removals.
    const cloneHead = execFileSync('git', ['-C', repositoryRoot, 'rev-parse', 'HEAD'], {
      encoding: 'utf8',
    }).trim()
    assert.ok(cloneHead.length === 40)
    assert.equal(fixture().inventory.currentHead.gitHead, AUDITED_COMMUNITY_HEAD)
    for (const path of [
      'crates/winwincode-control-plane/src/vault_kms_network.rs',
      'crates/winwincode-control-plane/tests/vault_kms_network.rs',
    ]) {
      assert.throws(
        () => execFileSync('git', ['-C', repositoryRoot, 'cat-file', '-e', `HEAD:${path}`]),
        /exit|fatal|Missing/u,
      )
    }
    appendFileSync(
      join(repositoryRoot, 'docs/decisions/0031-repository-split.inventory.json'),
      '\n',
    )
    execFileSync('git', ['-C', repositoryRoot, 'add', 'docs/decisions/0031-repository-split.inventory.json'])
    execFileSync('git', [
      '-C', repositoryRoot,
      'commit',
      '-m', 'test: update split metadata',
      '--', 'docs/decisions/0031-repository-split.inventory.json',
    ])
    assert.notEqual(
      execFileSync('git', ['-C', repositoryRoot, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
      AUDITED_COMMUNITY_HEAD,
    )
    assert.deepEqual(validateRepositorySplitMetadata({
      ...fixture(),
      repositoryRoot,
      trackedPaths: trackedPathsAt(repositoryRoot, fixture().inventory),
    }), EXPECTED_VALIDATION_RESULT)
  } finally {
    rmSync(parent, { recursive: true, force: true })
  }
})

test('tracked path or product blob drift from the audited baseline is rejected', async t => {
  await t.test('tracked path added', () => {
    const { parent, repositoryRoot } = cloneRepository()
    try {
      appendFileSync(join(repositoryRoot, 'apps/community-split-drift.txt'), 'drift\n')
      execFileSync('git', ['-C', repositoryRoot, 'add', 'apps/community-split-drift.txt'])
      execFileSync('git', ['-C', repositoryRoot, 'commit', '-m', 'test: add tracked drift'])
      assert.throws(
        () => validateRepositorySplitMetadata({
          ...fixture(),
          repositoryRoot,
          trackedPaths: trackedPathsAt(repositoryRoot, fixture().inventory),
        }),
        /live Community tracked paths differ from the audited baseline/u,
      )
    } finally {
      rmSync(parent, { recursive: true, force: true })
    }
  })
  await t.test('product blob changed', () => {
    const { parent, repositoryRoot } = cloneRepository()
    try {
      appendFileSync(join(repositoryRoot, 'apps/client/src/application.ts'), '\n// drift\n')
      execFileSync('git', ['-C', repositoryRoot, 'add', 'apps/client/src/application.ts'])
      execFileSync('git', ['-C', repositoryRoot, 'commit', '-m', 'test: change tracked product'])
      assert.throws(
        () => validateRepositorySplitMetadata({
          ...fixture(),
          repositoryRoot,
          trackedPaths: trackedPathsAt(repositoryRoot, fixture().inventory),
        }),
        /live Community tracked content differs from the audited baseline/u,
      )
    } finally {
      rmSync(parent, { recursive: true, force: true })
    }
  })
})

test('mixed seam target and test mappings are exact and current-head pinned', async t => {
  for (let index = 0; index < 12; index += 1) {
    await t.test(`Cloud seam ${index + 1} cannot exchange target and test paths`, () => {
      rejectedAgainstCurrentHead(
        documents => {
          const evidence = documents.inventory.currentHeadTargetEvidence.mixedSeamEvidence
          const current = evidence[index].cloud
          const next = evidence[(index + 1) % evidence.length].cloud
          ;[current.targetPaths, next.targetPaths] = [next.targetPaths, current.targetPaths]
          ;[current.tests, next.tests] = [next.tests, current.tests]
        },
        /canonical mapping/u,
      )
    })
  }
  await t.test('an unrelated existing Community path is rejected', () => {
    rejectedAgainstCurrentHead(
      documents => {
        const cloud = documents.inventory.currentHeadTargetEvidence.mixedSeamEvidence[0].cloud
        cloud.targetPaths = ['docs/decisions/0031-repository-split.inventory.json']
        cloud.tests = ['tests/repository-split-metadata.test.mjs']
      },
      /canonical mapping/u,
    )
  })
  await t.test('deleting a target path is rejected', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadTargetEvidence.mixedSeamEvidence[1].cloud.targetPaths = []
      },
      /cannot pass without target paths and tests/u,
    )
  })
  await t.test('deleting a test path is rejected', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadTargetEvidence.mixedSeamEvidence[1].cloud.tests = []
      },
      /cannot pass without target paths and tests/u,
    )
  })
  await t.test('source HEAD drift is rejected', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadTargetEvidence.sourcePins.cloud.sourceAudit.gitHead = 'forged'
      },
      /source pin differs from audited Community baseline HEAD\/tree/u,
    )
  })
  await t.test('source tree drift is rejected', () => {
    rejectedAgainstCurrentHead(
      documents => {
        documents.inventory.currentHeadTargetEvidence.mixedSeamEvidence[0].sourceHead.gitTree = 'forged'
      },
      /source HEAD\/tree is not reconciled/u,
    )
  })
})

test('command runs from an unrelated directory without reading adjacent product repositories', () => {
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-repository-split-'))
  try {
    const output = execFileSync(process.execPath, [verifierPath], {
      cwd: directory,
      encoding: 'utf8',
    })
    assert.deepEqual(JSON.parse(output), {
      status: 'passed',
      ...EXPECTED_VALIDATION_RESULT,
    })
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})

test('repository ID drift is rejected', () => {
  rejected(
    documents => {
      documents.editions.repositories.cloud.repository = 'wrong-cloud'
    },
    /editions repository IDs/u,
  )
})

test('stable task omission and duplication are rejected', async t => {
  await t.test('omission', () => {
    rejected(
      documents => {
        documents.taskMap.stableTasks.pop()
      },
      /stable task count/u,
    )
  })
  await t.test('duplicate', () => {
    rejected(
      documents => {
        documents.taskMap.stableTasks[1].stableTaskId = (
          documents.taskMap.stableTasks[0].stableTaskId
        )
      },
      /stable task IDs contains duplicates/u,
    )
  })
})

test('target Bead omission, duplication, and product count drift are rejected', async t => {
  await t.test('omission', () => {
    rejected(
      documents => {
        delete documents.taskMap.stableTasks.find(task => task.targetBeadId).targetBeadId
      },
      /created Cloud and Enterprise target task count/u,
    )
  })
  await t.test('duplicate', () => {
    rejected(
      documents => {
        const targets = documents.taskMap.stableTasks.filter(task => task.targetBeadId)
        targets[1].targetBeadId = targets[0].targetBeadId
      },
      /target Bead IDs contains duplicates/u,
    )
  })
  await t.test('Cloud count', () => {
    rejected(
      documents => {
        const task = documents.taskMap.stableTasks.find(
          entry => entry.targetRepository === 'winwincode-cloud',
        )
        task.targetRepository = 'winwincode-enterprise'
      },
      /Cloud stable task count/u,
    )
  })
})

test('mixed seam and Enterprise direct path count drift are rejected', async t => {
  await t.test('mixed seam', () => {
    rejected(
      documents => {
        documents.inventory.highRiskMixedPaths.pop()
      },
      /current HEAD mixed seam count/u,
    )
  })
  await t.test('Enterprise direct path', () => {
    rejected(
      documents => {
        documents.inventory.ownership['winwincode-enterprise'].push('apps/enterprise/index.ts')
      },
      /current HEAD Enterprise direct path count/u,
    )
  })
})

test('local cross-repository dependency and absolute inventory path are rejected', async t => {
  await t.test('local dependency', () => {
    rejected(
      documents => {
        documents.editions.communityCoreRelease.localPathDependency = (
          '/Volumes/EXAMPLE/winwincode-cloud'
        )
      },
      /localPathDependency must be false/u,
    )
  })
  await t.test('absolute Enterprise path', () => {
    rejected(
      documents => {
        const absolute = '/Volumes/EXAMPLE/winwincode-enterprise/apps/enterprise/index.ts'
        documents.inventory.historyPreservingMoves[0].path = absolute
      },
      /must not reference an absolute or parent repository path/u,
    )
  })
})
