#!/usr/bin/env node

import { isAbsolute, relative, resolve } from 'node:path'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, readFileSync } from 'node:fs'
import { pathToFileURL } from 'node:url'

const EXPECTED_REPOSITORIES = Object.freeze([
  'winwincode',
  'winwincode-cloud',
  'winwincode-enterprise',
])
const CROSS_REPOSITORY_TARGET = 'cross-repository-migration'
const EXPECTED_STABLE_TASKS = 178
const EXPECTED_TARGET_TASKS = 49
const EXPECTED_CLOUD_TASKS = 32
const EXPECTED_ENTERPRISE_TASKS = 17
const EXPECTED_ENTERPRISE_DIRECT_PATHS = 103
// This is the reviewed Community tree from which the split inventory was audited.
// Keep this independent from the inventory: changing every recorded pin must not
// move the source of truth used by this verifier.
// 2026-09-20 documented re-audit: accepted Community head is main c622e0c1
// (managed-app/00os product landings + docs pages). Ownership lockstep covers
// scoped managed-app/00os paths; jev-runtime-spec/ and winwincode-bd-tracks/
// remain out-of-scope support/docs roots outside includedRoots.
const AUDITED_COMMUNITY_HEAD = 'c622e0c185288395fa82db17287c0995ed6f64d4'
const AUDITED_COMMUNITY_TREE = '0b35596548ada043ff517ea9848a843d1a5f8271'
// vault_kms_network migrate-out sources remain recoverable from the pre-prune
// Community tree that still contained those paths. This pin is intentionally
// distinct from the live audited Community head after the prune commit landed.
const AUDITED_MIGRATE_OUT_RECOVERY_HEAD = '52f2eda69fd0b03e6f42b980be3b9a4965092497'
const AUDITED_MIGRATE_OUT_RECOVERY_TREE = '2a49c265e95d4160016618ba0cf17377849a134f'
const SPLIT_METADATA_ALLOWLIST = new Set([
  'docs/decisions/0031-repository-split.inventory.json',
  'scripts/verify-repository-split-metadata.mjs',
  'tests/repository-split-metadata.test.mjs',
])
const ALLOWED_COMMUNITY_DISPOSITION_ACTIONS = Object.freeze([
  'retain',
  'retain-and-narrow',
  'retain-infrastructure-addon',
  'retain-historical',
  'rewrite-mixed',
  'migrate-out',
])
const COMMUNITY_PRUNE_PERMISSION_READY = 'community-owned-ready-for-1.1-freeze'
const EXPECTED_CURRENT_HEAD_MIXED_SEAM_TARGETS = Object.freeze({
  'apps/client/src/application.ts': {
    cloud: {
      targetPaths: ['apps/cloud/src/application.mjs'],
      tests: ['apps/cloud/tests/cloud-web-start.test.mjs'],
    },
    enterprise: {
      targetPaths: ['apps/enterprise/src/application.ts'],
      tests: [],
    },
  },
  'apps/client/src/generated/contracts.ts': {
    cloud: {
      targetPaths: ['apps/cloud/src/generated/contracts.ts'],
      tests: ['tests/cloud-contracts.test.mjs'],
    },
    enterprise: {
      targetPaths: ['apps/enterprise/src/generated/contracts.ts'],
      tests: ['tests/enterprise-contract-generation.test.mjs'],
    },
  },
  'schema/winwincode/v1/domain.schema.json': {
    cloud: {
      targetPaths: ['schema/cloud/v1/domain.schema.json'],
      tests: ['tests/cloud-contracts.test.mjs'],
    },
    enterprise: {
      targetPaths: ['schema/enterprise/v1/domain.schema.json'],
      tests: [],
    },
  },
  'schema/winwincode/v1/control-plane-http.schema.json': {
    cloud: {
      targetPaths: ['schema/cloud/v1/cloud-http.schema.json'],
      tests: ['tests/cloud-contracts.test.mjs'],
    },
    enterprise: {
      targetPaths: ['schema/enterprise/v1/management-http.schema.json'],
      tests: [],
    },
  },
  'schema/winwincode/v1/control-plane-events.schema.json': {
    cloud: {
      targetPaths: ['schema/cloud/v1/cloud-events.schema.json'],
      tests: ['tests/cloud-contracts.test.mjs'],
    },
    enterprise: {
      targetPaths: ['schema/enterprise/v1/management-events.schema.json'],
      tests: [],
    },
  },
  'schema/winwincode/v1/execution-port.schema.json': {
    cloud: {
      targetPaths: ['schema/cloud/v1/execution-admission.schema.json'],
      tests: ['tests/cloud-contracts.test.mjs'],
    },
    enterprise: {
      targetPaths: ['schema/enterprise/v1/execution-policy-extension.schema.json'],
      tests: [],
    },
  },
  'crates/winwincode-control-plane/src/lib.rs': {
    cloud: {
      targetPaths: ['crates/winwincode-cloud/src/lib.rs'],
      tests: ['crates/winwincode-cloud/tests/cloud_server_bootstrap.rs'],
    },
    enterprise: {
      targetPaths: ['crates/winwincode-enterprise/src/control_plane/mod.rs'],
      tests: [],
    },
  },
  'crates/winwincode-storage/src/lib.rs': {
    cloud: {
      targetPaths: ['crates/winwincode-cloud/src/hosted_storage.rs'],
      tests: ['crates/winwincode-cloud/tests/hosted_postgres.rs'],
    },
    enterprise: {
      targetPaths: ['crates/winwincode-enterprise/src/storage/mod.rs'],
      tests: [],
    },
  },
  'crates/winwincode-server/src/application.rs': {
    cloud: {
      targetPaths: ['crates/winwincode-cloud/src/application.rs'],
      tests: ['crates/winwincode-cloud/tests/cloud_server_bootstrap.rs'],
    },
    enterprise: {
      targetPaths: ['crates/winwincode-enterprise/src/application.rs'],
      tests: [],
    },
  },
  'crates/winwincode-domain/src/generated.rs': {
    cloud: {
      targetPaths: ['crates/winwincode-cloud/src/generated/domain.rs'],
      tests: ['tests/cloud-contracts.test.mjs'],
    },
    enterprise: {
      targetPaths: ['crates/winwincode-enterprise/src/generated/domain.rs'],
      tests: ['crates/winwincode-enterprise/tests/enterprise_hierarchy_domain.rs'],
    },
  },
  'crates/winwincode-api/src/generated.rs': {
    cloud: {
      targetPaths: ['crates/winwincode-cloud/src/generated/api.rs'],
      tests: ['tests/cloud-contracts.test.mjs'],
    },
    enterprise: {
      targetPaths: ['crates/winwincode-enterprise/src/generated/api.rs'],
      tests: [],
    },
  },
  'docs/contracts/control-plane-api-coverage.matrix.json': {
    cloud: {
      targetPaths: ['docs/contracts/cloud-api-coverage.matrix.json'],
      tests: ['tests/cloud-api-coverage.test.mjs'],
    },
    enterprise: {
      targetPaths: ['docs/contracts/enterprise-api-coverage.matrix.json'],
      tests: [],
    },
  },
})

function fail(errors) {
  throw new Error(`REPOSITORY_SPLIT_METADATA_INVALID:\n- ${errors.join('\n- ')}`)
}

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function arrayAt(value, label, errors) {
  if (!Array.isArray(value)) {
    errors.push(`${label} must be an array`)
    return []
  }
  return value
}

function objectAt(value, label, errors) {
  if (!isObject(value)) {
    errors.push(`${label} must be an object`)
    return {}
  }
  return value
}

function sorted(values) {
  return [...values].sort((left, right) => left.localeCompare(right))
}

function sameStrings(actual, expected) {
  return JSON.stringify(sorted(actual)) === JSON.stringify(sorted(expected))
}

function requireUniqueStrings(values, label, errors) {
  if (!values.every(value => typeof value === 'string' && value.length > 0)) {
    errors.push(`${label} must contain non-empty strings`)
    return
  }
  if (new Set(values).size !== values.length) errors.push(`${label} contains duplicates`)
}

function countBy(values, key) {
  const counts = {}
  for (const value of values) {
    const name = value?.[key]
    counts[name] = (counts[name] ?? 0) + 1
  }
  return counts
}

function requireCount(actual, expected, label, errors) {
  if (actual !== expected) errors.push(`${label} must be ${expected}; found ${actual}`)
}

function requireRecordedCounts(recorded, actual, label, errors) {
  const keys = new Set([...Object.keys(recorded), ...Object.keys(actual)])
  for (const key of keys) {
    if (recorded[key] !== actual[key]) {
      errors.push(
        `${label}.${key} must be ${actual[key] ?? 0}; found ${recorded[key] ?? 0}`,
      )
    }
  }
}

function requireExactRecordedCounts(recorded, actual, label, errors) {
  if (!isObject(recorded)) {
    errors.push(`${label} must be an object`)
    return
  }
  if (!sameStrings(Object.keys(recorded), Object.keys(actual))) {
    errors.push(`${label} keys must match the derived counts`)
  }
  requireRecordedCounts(recorded, actual, label, errors)
}

function requireRepositoryRelative(path, label, errors) {
  if (typeof path !== 'string' || path.length === 0) {
    errors.push(`${label} must be a non-empty repository-relative path`)
    return
  }
  if (isAbsolute(path) || path === '..' || path.startsWith('../') || path.includes('/../')) {
    errors.push(`${label} must not reference an absolute or parent repository path: ${path}`)
  }
  if (path.includes('\\')) errors.push(`${label} must use repository-relative POSIX separators`)
}

function trackedPaths(root) {
  return execFileSync('git', ['-C', root, 'ls-files', '-z'])
    .toString()
    .split('\0')
    .filter(Boolean)
}

function gitRevision(root, revision) {
  try {
    return execFileSync('git', ['-C', root, 'rev-parse', revision], { encoding: 'utf8' }).trim()
  } catch {
    return undefined
  }
}

function gitTreePaths(root, revision) {
  try {
    return execFileSync('git', ['-C', root, 'ls-tree', '-r', '-z', '--name-only', revision])
      .toString()
      .split('\0')
      .filter(Boolean)
  } catch {
    return undefined
  }
}

function gitTreeEntries(root, revision) {
  try {
    const entries = new Map()
    const records = execFileSync('git', ['-C', root, 'ls-tree', '-r', '-z', revision])
      .toString()
      .split('\0')
      .filter(Boolean)
    for (const record of records) {
      const separator = record.indexOf('\t')
      if (separator < 0) continue
      const [mode, type, object] = record.slice(0, separator).split(' ')
      entries.set(record.slice(separator + 1), { mode, type, object })
    }
    return entries
  } catch {
    return undefined
  }
}

function validateAuditedTree(repositoryRoot, liveRevision, errors) {
  const auditedTree = gitRevision(repositoryRoot, `${AUDITED_COMMUNITY_HEAD}^{tree}`)
  if (auditedTree !== AUDITED_COMMUNITY_TREE) {
    errors.push('audited Community baseline commit does not resolve to the pinned tree')
    return
  }
  const baselineEntries = gitTreeEntries(repositoryRoot, AUDITED_COMMUNITY_HEAD)
  const liveEntries = gitTreeEntries(repositoryRoot, liveRevision)
  if (baselineEntries === undefined || liveEntries === undefined) {
    errors.push('audited Community baseline/live git tree could not be read')
    return
  }
  const baselinePaths = [...baselineEntries.keys()].filter(path => !SPLIT_METADATA_ALLOWLIST.has(path))
  const livePaths = [...liveEntries.keys()].filter(path => !SPLIT_METADATA_ALLOWLIST.has(path))
  if (!sameStrings(livePaths, baselinePaths)) {
    errors.push('live Community tracked paths differ from the audited baseline outside the split metadata allowlist')
    return
  }
  for (const path of baselinePaths) {
    const expected = baselineEntries.get(path)
    const actual = liveEntries.get(path)
    if (
      actual?.mode !== expected?.mode
      || actual?.type !== expected?.type
      || actual?.object !== expected?.object
    ) {
      errors.push(`live Community tracked content differs from the audited baseline: ${path}`)
    }
  }
}

function pathListSha256(paths) {
  return createHash('sha256').update(`${paths.join('\n')}\n`).digest('hex')
}

function countPathsByRoot(paths) {
  const counts = {}
  for (const path of paths) {
    const root = typeof path === 'string' ? path.split('/')[0] : undefined
    if (root !== undefined) counts[root] = (counts[root] ?? 0) + 1
  }
  return counts
}

function countPathsByBucketAndRoot(ownership, buckets) {
  return Object.fromEntries(
    buckets.map(bucket => [bucket, countPathsByRoot(ownership[bucket] ?? [])]),
  )
}

function countTargetRepositories(evidence) {
  const counts = {}
  for (const entry of evidence) {
    for (const repository of entry?.targetRepositories ?? []) {
      counts[repository] = (counts[repository] ?? 0) + 1
    }
  }
  return counts
}

function validateCurrentHeadCoverage(inventory, actualPaths, repositoryRoot, errors) {
  const current = objectAt(inventory?.currentHead, 'inventory.currentHead', errors)
  const classification = objectAt(
    inventory?.currentHeadClassification,
    'inventory.currentHeadClassification',
    errors,
  )
  const ownership = objectAt(inventory?.ownership, 'inventory.ownership', errors)
  const buckets = ['winwincode', 'winwincode-cloud', 'winwincode-enterprise', 'migration-review']
  const classified = buckets.flatMap(bucket => (
    arrayAt(ownership[bucket], `inventory.ownership.${bucket}`, errors)
  ))
  const actualGitHead = gitRevision(repositoryRoot, 'HEAD')
  const actualGitTree = gitRevision(repositoryRoot, 'HEAD^{tree}')
  if (actualGitHead === undefined || actualGitTree === undefined) {
    errors.push('Community repository HEAD/tree could not be read from git')
  } else {
    if (current.gitHead !== AUDITED_COMMUNITY_HEAD) {
      errors.push(`inventory current HEAD must match audited Community baseline ${AUDITED_COMMUNITY_HEAD}`)
    }
    if (current.gitTree !== AUDITED_COMMUNITY_TREE) {
      errors.push(`inventory current tree must match audited Community baseline ${AUDITED_COMMUNITY_TREE}`)
    }
    validateAuditedTree(repositoryRoot, actualGitHead, errors)
  }
  const bucketCounts = Object.fromEntries(
    buckets.map(bucket => [bucket, ownership[bucket]?.length ?? 0]),
  )
  const bucketCountsByRoot = countPathsByBucketAndRoot(ownership, buckets)
  const actualCountsByRoot = countPathsByRoot(actualPaths)
  const summary = objectAt(inventory?.summary, 'inventory.summary', errors)
  const baseline = objectAt(inventory?.baseline, 'inventory.baseline', errors)
  const baselinePaths = gitTreePaths(repositoryRoot, baseline.gitHead)
  if (baselinePaths === undefined) {
    errors.push('Community baseline tree could not be read from git')
  } else {
    const scopedBaselinePaths = baselinePaths.filter(path => (
      Array.isArray(baseline.includedRoots) && baseline.includedRoots.includes(path.split('/')[0])
    ))
    requireCount(baseline.trackedFileCount, scopedBaselinePaths.length, 'baseline tracked path count', errors)
    requireExactRecordedCounts(
      baseline.trackedFileCountByRoot,
      countPathsByRoot(scopedBaselinePaths),
      'baseline tracked file count by root',
      errors,
    )
    if (baseline.trackedPathListSha256 !== pathListSha256(scopedBaselinePaths)) {
      errors.push('baseline tracked path hash does not match the baseline git tree')
    }
  }
  requireExactRecordedCounts(
    current.trackedFileCountByRoot,
    actualCountsByRoot,
    'current HEAD tracked file count by root',
    errors,
  )
  requireUniqueStrings(classified, 'current HEAD ownership paths', errors)
  const actual = new Set(actualPaths)
  const listed = new Set(classified)
  const missing = actualPaths.filter(path => !listed.has(path))
  const extra = classified.filter(path => !actual.has(path))
  if (missing.length > 0) errors.push(`current HEAD has ${missing.length} unowned tracked paths`)
  if (extra.length > 0) errors.push(`ownership lists ${extra.length} paths absent from current HEAD`)
  if (current.trackedFileCount !== actualPaths.length) {
    errors.push(`current HEAD tracked file count must be ${actualPaths.length}; found ${current.trackedFileCount}`)
  }
  const hash = pathListSha256(actualPaths)
  if (current.trackedPathListSha256 !== hash) {
    errors.push(`current HEAD tracked path hash must be ${hash}; found ${current.trackedPathListSha256}`)
  }
  requireCount(classification.trackedPathCount, actualPaths.length, 'current HEAD tracked path count', errors)
  requireCount(classification.classifiedPathCount, actualPaths.length, 'current HEAD classified path count', errors)
  requireExactRecordedCounts(
    classification.classifiedPathCountByBucket,
    bucketCounts,
    'current HEAD classified path counts by bucket',
    errors,
  )
  const classificationByBucketAndRoot = objectAt(
    classification.classifiedPathCountByBucketAndRoot,
    'inventory.currentHeadClassification.classifiedPathCountByBucketAndRoot',
    errors,
  )
  for (const bucket of buckets) {
    requireExactRecordedCounts(
      classificationByBucketAndRoot[bucket],
      bucketCountsByRoot[bucket],
      `current HEAD ${bucket} classified path counts by root`,
      errors,
    )
  }
  requireCount(classification.communityRetainedPathCount, ownership.winwincode?.length ?? 0, 'Community retained path count', errors)
  requireCount(classification.cloudDirectMigrationPathCount, ownership['winwincode-cloud']?.length ?? 0, 'Cloud direct migration path count', errors)
  requireCount(classification.enterpriseDirectMigrationPathCount, ownership['winwincode-enterprise']?.length ?? 0, 'Enterprise direct migration path count', errors)
  requireCount(classification.migrationReviewPathCount, ownership['migration-review']?.length ?? 0, 'migration-review path count', errors)
  if (summary.currentHeadTrackedPathCount !== actualPaths.length) {
    errors.push(`recorded current HEAD path count must be ${actualPaths.length}; found ${summary.currentHeadTrackedPathCount}`)
  }
  if (summary.currentHeadTrackedPathListSha256 !== hash) {
    errors.push(`recorded current HEAD tracked path hash must be ${hash}`)
  }
  for (const bucket of buckets) {
    requireCount(
      classification.classifiedPathCountByBucket?.[bucket],
      ownership[bucket]?.length ?? 0,
      `current HEAD ${bucket} path count`,
      errors,
    )
  }
  requireCount(summary.classifiedPathCount, actualPaths.length, 'summary classified path count', errors)
  requireExactRecordedCounts(summary.classifiedPathCountByBucket, bucketCounts, 'summary classified path counts by bucket', errors)
  const summaryByBucketAndRoot = objectAt(
    summary.classifiedPathCountByBucketAndRoot,
    'inventory.summary.classifiedPathCountByBucketAndRoot',
    errors,
  )
  for (const bucket of buckets) {
    requireExactRecordedCounts(
      summaryByBucketAndRoot[bucket],
      bucketCountsByRoot[bucket],
      `summary ${bucket} classified path counts by root`,
      errors,
    )
  }
  const targetEvidence = objectAt(
    inventory?.currentHeadTargetEvidence,
    'inventory.currentHeadTargetEvidence',
    errors,
  )
  const directMigrations = arrayAt(
    targetEvidence.currentDirectMigrations,
    'inventory.currentHeadTargetEvidence.currentDirectMigrations',
    errors,
  )
  if (directMigrations.length !== 0) {
    errors.push('current HEAD direct migrations must be empty until target evidence grants removal permission')
  }
  const evidence = arrayAt(
    targetEvidence.mixedSeamEvidence,
    'inventory.currentHeadTargetEvidence.mixedSeamEvidence',
    errors,
  )
  const currentMixed = arrayAt(
    inventory?.highRiskMixedPaths,
    'inventory.highRiskMixedPaths',
    errors,
  )
  const mixedPaths = currentMixed.map(seam => seam?.path)
  const evidencePaths = evidence.map(entry => entry?.sourcePath)
  requireUniqueStrings(evidencePaths, 'current HEAD mixed seam evidence paths', errors)
  if (!sameStrings(evidencePaths, mixedPaths)) {
    errors.push('current HEAD target evidence must cover current mixed seams exactly once')
  }
  if (!sameStrings(mixedPaths, Object.keys(EXPECTED_CURRENT_HEAD_MIXED_SEAM_TARGETS))) {
    errors.push('current HEAD mixed seam paths differ from the canonical target mapping')
  }
  requireCount(classification.mixedSeamPathCount, mixedPaths.length, 'current HEAD mixed seam path count', errors)
  requireCount(classification.targetEvidenceRequiredPathCount, evidence.length, 'current HEAD target evidence path count', errors)
  const targetEvidenceByRepository = countTargetRepositories(evidence)
  requireExactRecordedCounts(
    classification.targetEvidenceRequiredByRepository,
    targetEvidenceByRepository,
    'current HEAD target evidence counts by repository',
    errors,
  )
  requireCount(summary.migrationReviewPathCount, ownership['migration-review']?.length ?? 0, 'summary migration-review path count', errors)
  requireCount(summary.highRiskMixedPathCount, mixedPaths.length, 'summary high-risk mixed path count', errors)
  requireCount(summary.historicalHighRiskMixedPathCount, inventory?.historicalHighRiskMixedPaths?.length ?? 0, 'summary historical mixed path count', errors)
  requireCount(summary.historyPreservingMovePathCount, inventory?.historyPreservingMoves?.length ?? 0, 'summary history-preserving move path count', errors)
  requireCount(summary.baselineTrackedPathCount, inventory?.baseline?.trackedFileCount, 'summary baseline tracked path count', errors)
  requireCount(summary.targetEvidenceRequiredPathCount, evidence.length, 'summary target evidence path count', errors)
  requireExactRecordedCounts(
    summary.targetEvidenceRequiredByRepository,
    targetEvidenceByRepository,
    'summary target evidence counts by repository',
    errors,
  )
  if (targetEvidence.deletionPermission !== 'none') {
    errors.push('current HEAD target evidence must grant no deletion permission')
  }
  if (targetEvidence.crossRepositoryEvidenceRole !== 'observational-only-not-a-community-prune-gate') {
    errors.push('cross-repository target evidence must remain observational and non-authorizing for Community prune')
  }
  if (targetEvidence.migrationReviewRequiresSeparateOwnershipDecision !== false) {
    errors.push('migration-review ownership decision must be recorded in currentHeadCommunityDisposition')
  }
  validateCommunityDisposition(inventory, ownership, mixedPaths, errors, repositoryRoot)
  const sourcePins = objectAt(targetEvidence.sourcePins, 'inventory.currentHeadTargetEvidence.sourcePins', errors)
  for (const repository of ['cloud', 'enterprise']) {
    const pin = objectAt(sourcePins[repository], `target evidence ${repository} source pin`, errors)
    if (pin.sourceHeadMatches !== true) errors.push(`target evidence ${repository} source HEAD/tree does not match Community current HEAD`)
    const source = repository === 'cloud' ? pin.sourceAudit : pin.sourcePin
    if (source?.gitHead !== AUDITED_COMMUNITY_HEAD || source?.gitTree !== AUDITED_COMMUNITY_TREE) {
      errors.push(`target evidence ${repository} source pin differs from audited Community baseline HEAD/tree`)
    }
  }
  const sourcePathState = objectAt(
    targetEvidence.sourcePathState,
    'inventory.currentHeadTargetEvidence.sourcePathState',
    errors,
  )
  const cloudState = objectAt(sourcePathState.cloud, 'target evidence cloud source path state', errors)
  const cloudPresent = new Set(arrayAt(cloudState.present, 'target evidence cloud present paths', errors))
  for (const path of mixedPaths) {
    if (!cloudPresent.has(path)) errors.push(`Cloud current-head audit does not mark mixed seam present: ${path}`)
  }
  const enterpriseState = objectAt(
    sourcePathState.enterprise,
    'target evidence enterprise source path state',
    errors,
  )
  if (enterpriseState.allCurrentMixedSourcePathsPresent !== true) {
    errors.push('Enterprise current-head review does not confirm all current mixed seams are present')
  }
  const reconciliationSummary = objectAt(
    targetEvidence.reconciliationSummary,
    'inventory.currentHeadTargetEvidence.reconciliationSummary',
    errors,
  )
  requireCount(reconciliationSummary.seamCount, evidence.length, 'reconciled seam count', errors)
  requireCount(
    reconciliationSummary.cloudPassCount,
    evidence.filter(entry => entry?.cloud?.status === 'pass').length,
    'reconciled Cloud pass count',
    errors,
  )
  requireCount(
    reconciliationSummary.enterprisePassCount,
    evidence.filter(entry => entry?.enterprise?.status === 'pass').length,
    'reconciled Enterprise pass count',
    errors,
  )
  requireCount(
    reconciliationSummary.overallGapCount,
    evidence.filter(entry => entry?.reconciliationStatus === 'gap').length,
    'reconciled overall gap count',
    errors,
  )
  for (const entry of evidence) {
    if (!Array.isArray(entry?.targetRepositories) || entry.targetRepositories.length === 0) {
      errors.push(`target evidence ${entry?.sourcePath} must name at least one target repository`)
    }
    if (entry?.status !== 'target-shape-observation-only' || entry?.deletionPermission !== false) {
      errors.push(`target evidence ${entry?.sourcePath} must remain observational and non-authorizing`)
    }
    if (
      entry?.sourceHead?.gitHead !== AUDITED_COMMUNITY_HEAD
      || entry?.sourceHead?.gitTree !== AUDITED_COMMUNITY_TREE
      || entry?.sourceHead?.cloudAuditMatches !== true
      || entry?.sourceHead?.enterpriseProvenanceMatches !== true
    ) {
      errors.push(`target evidence ${entry?.sourcePath} source HEAD/tree is not reconciled`)
    }
    for (const repository of ['cloud', 'enterprise']) {
      const record = objectAt(entry?.[repository], `target evidence ${entry?.sourcePath}.${repository}`, errors)
      const targetPaths = arrayAt(
        record.targetPaths,
        `target evidence ${entry?.sourcePath}.${repository}.targetPaths`,
        errors,
      )
      const tests = arrayAt(
        record.tests,
        `target evidence ${entry?.sourcePath}.${repository}.tests`,
        errors,
      )
      const paths = [...targetPaths, ...tests]
      if (paths.some(path => path.includes('upstream/source-snapshots/'))) {
        errors.push(`target evidence ${entry?.sourcePath}.${repository} uses a source snapshot`)
      }
      if (!['pass', 'gap'].includes(record.status)) {
        errors.push(`target evidence ${entry?.sourcePath}.${repository} must be pass or gap`)
      }
      if (record.status === 'pass' && (targetPaths.length === 0 || tests.length === 0)) {
        errors.push(`target evidence ${entry?.sourcePath}.${repository} cannot pass without target paths and tests`)
      }
      if (record.status === 'pass' && (record.targetPathsTracked !== true || record.testPathsTracked !== true)) {
        errors.push(`target evidence ${entry?.sourcePath}.${repository} pass is missing tracked-path confirmation`)
      }
      const expected = EXPECTED_CURRENT_HEAD_MIXED_SEAM_TARGETS[entry?.sourcePath]?.[repository]
      if (!expected) {
        errors.push(`target evidence ${entry?.sourcePath}.${repository} has no canonical target mapping`)
      } else {
        if (!sameStrings(targetPaths, expected.targetPaths)) {
          errors.push(`target evidence ${entry?.sourcePath}.${repository}.targetPaths differ from the canonical mapping`)
        }
        if (!sameStrings(tests, expected.tests)) {
          errors.push(`target evidence ${entry?.sourcePath}.${repository}.tests differ from the canonical mapping`)
        }
      }
    }
  }
}

function validateCommunityDisposition(inventory, ownership, mixedPaths, errors, repositoryRoot) {
  const disposition = objectAt(
    inventory?.currentHeadCommunityDisposition,
    'inventory.currentHeadCommunityDisposition',
    errors,
  )
  if (disposition.status !== 'current-head-community-disposition-complete') {
    errors.push('current head community disposition status must be complete')
  }
  const auditedHead = objectAt(disposition.auditedHead, 'community disposition auditedHead', errors)
  if (auditedHead.gitHead !== AUDITED_COMMUNITY_HEAD || auditedHead.gitTree !== AUDITED_COMMUNITY_TREE) {
    errors.push('community disposition audited head/tree must match the audited Community baseline')
  }
  const boundary = objectAt(disposition.productBoundary, 'community disposition productBoundary', errors)
  if (boundary.multiTenant !== false) errors.push('Community productBoundary.multiTenant must be false')
  if (!Array.isArray(boundary.retainCapabilities) || boundary.retainCapabilities.length === 0) {
    errors.push('Community productBoundary.retainCapabilities must be non-empty')
  }
  const mrDispositions = objectAt(
    disposition.migrationReviewDispositions,
    'inventory.currentHeadCommunityDisposition.migrationReviewDispositions',
    errors,
  )
  const mrPaths = arrayAt(ownership['migration-review'], 'inventory.ownership.migration-review', errors)
  const listedMr = Object.keys(mrDispositions)
  const permission = objectAt(
    disposition.communityPrunePermission,
    'inventory.currentHeadCommunityDisposition.communityPrunePermission',
    errors,
  )
  const pruneExecution = objectAt(
    permission?.pruneExecution,
    'community prune permission pruneExecution',
    errors,
  )
  const executedMigrateOut = arrayAt(
    pruneExecution?.executedMigrateOut,
    'community prune execution executedMigrateOut',
    errors,
  )
  const executedPaths = executedMigrateOut.map(entry => entry?.path)
  requireUniqueStrings(executedPaths, 'community executed migrate-out paths', errors)
  const freezeMigrateOut = arrayAt(
    permission?.freezeMigrateOutPaths,
    'community prune permission freezeMigrateOutPaths',
    errors,
  )
  if (!sameStrings(freezeMigrateOut, executedPaths)) {
    errors.push('community freeze migrate-out paths must match executed migrate-out paths')
  }
  for (const path of freezeMigrateOut) {
    if (mrDispositions[path]?.action !== 'migrate-out') {
      errors.push(`freeze migrate-out disposition must remain recorded: ${path}`)
    }
  }
  for (const entry of executedMigrateOut) {
    if (mrDispositions[entry?.path]?.action !== 'migrate-out') {
      errors.push(`executed migrate-out path must keep a migrate-out freeze disposition: ${entry?.path}`)
    }
    const method = entry?.recoverableSource?.method
    if (typeof method !== 'string' || method.length === 0) {
      errors.push(`executed migrate-out ${entry?.path} must record recoverableSource.method`)
    }
  }
  const executedSet = new Set(executedPaths)
  const ownershipMr = new Set(mrPaths)
  for (const path of mrPaths) {
    if (!mrDispositions[path]) {
      errors.push(`migration-review ownership path lacks a freeze disposition: ${path}`)
    }
  }
  for (const path of listedMr) {
    if (!ownershipMr.has(path) && !executedSet.has(path)) {
      errors.push(`migration-review disposition is neither current ownership nor executed migrate-out: ${path}`)
    }
  }
  const actionCounts = {}
  for (const path of listedMr) {
    const entry = objectAt(mrDispositions[path], `migration-review disposition ${path}`, errors)
    if (!ALLOWED_COMMUNITY_DISPOSITION_ACTIONS.includes(entry?.action)) {
      errors.push(`migration-review disposition ${path} has unsupported action: ${entry?.action}`)
    } else {
      actionCounts[entry.action] = (actionCounts[entry.action] ?? 0) + 1
    }
    if (typeof entry?.reason !== 'string' || entry.reason.length === 0) {
      errors.push(`migration-review disposition ${path} must record a reason`)
    }
    if (entry?.action === 'migrate-out') {
      const recoverable = objectAt(
        entry?.recoverableSource,
        `migration-review disposition ${path}.recoverableSource`,
        errors,
      )
      if (recoverable?.gitHead !== AUDITED_MIGRATE_OUT_RECOVERY_HEAD) {
        errors.push(`migrate-out disposition ${path} recoverableSource.gitHead must pin the audited migrate-out recovery HEAD`)
      }
      if (typeof recoverable?.method !== 'string' || recoverable.method.length === 0) {
        errors.push(`migrate-out disposition ${path} must record a recoverableSource.method`)
      }
    }
    if (entry?.action === 'rewrite-mixed') {
      if (!mixedPaths.includes(path)) {
        errors.push(`rewrite-mixed disposition is not a current mixed seam: ${path}`)
      }
      const rewrite = objectAt(entry?.rewritePlan, `migration-review disposition ${path}.rewritePlan`, errors)
      if (rewrite?.communityCanonicalPath !== path) {
        errors.push(`rewrite-mixed disposition ${path} must keep one Community canonical path`)
      }
      if (!Array.isArray(rewrite?.communityKeeps) || rewrite.communityKeeps.length === 0) {
        errors.push(`rewrite-mixed disposition ${path} must record communityKeeps`)
      }
      if (typeof rewrite?.verificationMethod !== 'string' || rewrite.verificationMethod.length === 0) {
        errors.push(`rewrite-mixed disposition ${path} must record verificationMethod`)
      }
    }
    if (entry?.action === 'rewrite-mixed' || mixedPaths.includes(path)) {
      if (entry?.action !== 'rewrite-mixed') {
        errors.push(`current mixed seam must be rewrite-mixed: ${path}`)
      }
    }
  }
  requireExactRecordedCounts(
    objectAt(
      disposition.dispositionCounts?.migrationReviewActions,
      'community dispositionCounts.migrationReviewActions',
      errors,
    ),
    actionCounts,
    'community migration-review action counts',
    errors,
  )
  const rewritePlans = arrayAt(
    disposition.mixedSeamRewritePlans,
    'inventory.currentHeadCommunityDisposition.mixedSeamRewritePlans',
    errors,
  )
  const rewritePaths = rewritePlans.map(entry => entry?.path)
  if (!sameStrings(rewritePaths, mixedPaths)) {
    errors.push('community mixed seam rewrite plans must cover current mixed seams exactly once')
  }
  const highRisk = arrayAt(inventory?.highRiskMixedPaths, 'inventory.highRiskMixedPaths', errors)
  for (const seam of highRisk) {
    if (seam?.dispositionAction !== 'rewrite-mixed') {
      errors.push(`high-risk mixed seam must record rewrite-mixed disposition: ${seam?.path}`)
    }
    if (seam?.communityCanonicalPath !== seam?.path) {
      errors.push(`high-risk mixed seam must keep one Community canonical path: ${seam?.path}`)
    }
  }
  if (permission?.status !== COMMUNITY_PRUNE_PERMISSION_READY) {
    errors.push(`community prune permission must be ${COMMUNITY_PRUNE_PERMISSION_READY}`)
  }
  if (permission?.cloudEnterpriseTargetAcceptanceRequired !== false) {
    errors.push('Community prune permission must not require Cloud/Enterprise target acceptance')
  }
  if (permission?.formalCoreLockRequired !== false) {
    errors.push('Community prune permission must not require formal core.lock')
  }
  if (permission?.coreReleaseRequired !== false) {
    errors.push('Community prune permission must not require core release')
  }
  const conditions = objectAt(permission?.conditions, 'community prune permission conditions', errors)
  for (const [key, value] of Object.entries(conditions ?? {})) {
    if (value !== true) errors.push(`community prune permission condition must be true: ${key}`)
  }
  const migrateOutStillPresent = arrayAt(
    permission?.currentMigrateOutPathsStillPresent,
    'community prune permission currentMigrateOutPathsStillPresent',
    errors,
  )
  const migrateOutDispositionPaths = listedMr.filter(path => mrDispositions[path]?.action === 'migrate-out')
  const onDiskStillPresent = migrateOutDispositionPaths.filter(path => {
    if (typeof repositoryRoot !== 'string' || repositoryRoot.length === 0) return false
    try {
      return existsSync(resolve(repositoryRoot, path))
    } catch {
      return false
    }
  }).sort()
  if (!sameStrings(migrateOutStillPresent, onDiskStillPresent)) {
    errors.push('community prune permission currentMigrateOutPathsStillPresent must match migrate-out disposition paths still on disk')
  }
  if (migrateOutStillPresent.length > 0) {
    for (const path of migrateOutStillPresent) {
      if (!executedSet.has(path) && mrDispositions[path]?.action !== 'migrate-out') {
        errors.push(`still-present migrate-out path must remain a migrate-out disposition: ${path}`)
      }
    }
  }
  if (typeof pruneExecution?.status !== 'string' || pruneExecution.status.length === 0) {
    errors.push('community prune execution must record a status')
  }
  const historical = objectAt(
    disposition.historicalMigrateOutRecoverableSources,
    'community disposition historicalMigrateOutRecoverableSources',
    errors,
  )
  if (historical?.status !== 'recoverable-sources-recorded') {
    errors.push('historical migrate-out recoverable sources must be recorded')
  }
  const historicalList = arrayAt(historical?.list, 'historical migrate-out list', errors)
  const moves = arrayAt(inventory?.historyPreservingMoves, 'inventory.historyPreservingMoves', errors)
  if (historicalList.length !== moves.length) {
    errors.push('historical migrate-out recoverable list must cover historyPreservingMoves exactly')
  }
  const worktree = objectAt(
    disposition.worktreeIncrements,
    'community disposition worktreeIncrements',
    errors,
  )
  const untracked = arrayAt(worktree?.untrackedProductCandidates, 'worktree untrackedProductCandidates', errors)
  if (untracked.length === 0) {
    errors.push('worktree untracked product candidates must be classified')
  }
  for (const candidate of untracked) {
    if (typeof candidate?.path !== 'string' || candidate.path.length === 0) {
      errors.push('worktree untracked candidate must record a path')
    }
    if (!ALLOWED_COMMUNITY_DISPOSITION_ACTIONS.includes(candidate?.proposedAction)) {
      errors.push(`worktree untracked candidate has unsupported proposedAction: ${candidate?.proposedAction}`)
    }
  }
}

export function validateRepositorySplitMetadata({
  editions,
  inventory,
  taskMap,
  repositoryRoot,
  trackedPaths: actualTrackedPaths,
}) {
  const errors = []
  const editionRepositories = objectAt(editions?.repositories, 'editions.repositories', errors)
  const editionRepositoryIds = Object.values(editionRepositories)
    .map(entry => entry?.repository)
  requireUniqueStrings(editionRepositoryIds, 'editions repository IDs', errors)
  if (!sameStrings(editionRepositoryIds, EXPECTED_REPOSITORIES)) {
    errors.push('editions repository IDs must be winwincode, winwincode-cloud, and winwincode-enterprise')
  }

  const sourceOwnership = arrayAt(editions?.sourceOwnership, 'editions.sourceOwnership', errors)
  const sourceOwnerIds = sourceOwnership.map(entry => entry?.repository)
  requireUniqueStrings(sourceOwnerIds, 'editions source owner IDs', errors)
  if (!sameStrings(sourceOwnerIds, EXPECTED_REPOSITORIES)) {
    errors.push('editions source owners must match the three repository IDs')
  }

  const coreRelease = objectAt(
    editions?.communityCoreRelease,
    'editions.communityCoreRelease',
    errors,
  )
  if (coreRelease.ownerRepository !== 'winwincode') {
    errors.push('Community Core owner repository must be winwincode')
  }
  if (!sameStrings(coreRelease.consumers ?? [], [
    'winwincode-cloud',
    'winwincode-enterprise',
  ])) {
    errors.push('Community Core consumers must be Cloud and Enterprise')
  }
  if (coreRelease.localPathDependency !== false) {
    errors.push('Community Core localPathDependency must be false')
  }

  const gitRepositoryIds = arrayAt(
    taskMap?.gitRepositoryIds,
    'taskMap.gitRepositoryIds',
    errors,
  )
  requireUniqueStrings(gitRepositoryIds, 'task map Git repository IDs', errors)
  if (!sameStrings(gitRepositoryIds, EXPECTED_REPOSITORIES)) {
    errors.push('task map Git repository IDs must match editions repository IDs')
  }
  const routingTargets = arrayAt(taskMap?.routingTargets, 'taskMap.routingTargets', errors)
  const expectedRoutingTargets = [...EXPECTED_REPOSITORIES, CROSS_REPOSITORY_TARGET]
  requireUniqueStrings(routingTargets, 'task map routing targets', errors)
  if (!sameStrings(routingTargets, expectedRoutingTargets)) {
    errors.push('task map routing targets must contain three repositories and the migration target')
  }

  if (editions?.decision !== inventory?.decision || editions?.decision !== taskMap?.decision) {
    errors.push('all three documents must reference the same repository split decision')
  }

  const inventoryOwnership = objectAt(inventory?.ownership, 'inventory.ownership', errors)
  for (const repository of EXPECTED_REPOSITORIES) {
    if (!Array.isArray(inventoryOwnership[repository])) {
      errors.push(`inventory.ownership.${repository} must be an array`)
    }
  }

  if (actualTrackedPaths !== undefined) {
    if (typeof repositoryRoot !== 'string' || repositoryRoot.length === 0) {
      errors.push('repositoryRoot is required when validating current HEAD coverage')
    } else {
      validateCurrentHeadCoverage(inventory, actualTrackedPaths, repositoryRoot, errors)
    }
  }

  const stableTasks = arrayAt(taskMap?.stableTasks, 'taskMap.stableTasks', errors)
  requireCount(stableTasks.length, EXPECTED_STABLE_TASKS, 'stable task count', errors)
  const stableTaskIds = stableTasks.map(task => task?.stableTaskId)
  requireUniqueStrings(stableTaskIds, 'stable task IDs', errors)
  const stableTargets = stableTasks.map(task => task?.targetRepository)
  for (const target of stableTargets) {
    if (!routingTargets.includes(target)) errors.push(`unknown stable task target: ${target}`)
  }
  const stableCounts = countBy(stableTasks, 'targetRepository')
  requireCount(stableCounts.winwincode ?? 0, 124, 'winwincode stable task count', errors)
  requireCount(
    stableCounts['winwincode-cloud'] ?? 0,
    EXPECTED_CLOUD_TASKS,
    'Cloud stable task count',
    errors,
  )
  requireCount(
    stableCounts['winwincode-enterprise'] ?? 0,
    EXPECTED_ENTERPRISE_TASKS,
    'Enterprise stable task count',
    errors,
  )
  requireCount(
    stableCounts[CROSS_REPOSITORY_TARGET] ?? 0,
    5,
    'cross-repository stable task count',
    errors,
  )
  requireRecordedCounts(
    objectAt(
      taskMap?.statistics?.stableTasksByTargetRepository,
      'taskMap.statistics.stableTasksByTargetRepository',
      errors,
    ),
    stableCounts,
    'recorded stable task counts',
    errors,
  )

  const createdTargetTasks = stableTasks.filter(task => task?.targetBeadId !== undefined)
  requireCount(
    createdTargetTasks.length,
    EXPECTED_TARGET_TASKS,
    'created Cloud and Enterprise target task count',
    errors,
  )
  const targetBeadIds = createdTargetTasks.map(task => task?.targetBeadId)
  requireUniqueStrings(targetBeadIds, 'target Bead IDs', errors)
  for (const task of createdTargetTasks) {
    if (!['winwincode-cloud', 'winwincode-enterprise'].includes(task?.targetRepository)) {
      errors.push(`${task?.stableTaskId} has targetBeadId outside Cloud or Enterprise`)
    }
    if (task?.targetCreationState !== 'created') {
      errors.push(`${task?.stableTaskId} targetCreationState must be created`)
    }
  }
  const createdCounts = countBy(createdTargetTasks, 'targetRepository')
  requireCount(
    createdCounts['winwincode-cloud'] ?? 0,
    EXPECTED_CLOUD_TASKS,
    'created Cloud target task count',
    errors,
  )
  requireCount(
    createdCounts['winwincode-enterprise'] ?? 0,
    EXPECTED_ENTERPRISE_TASKS,
    'created Enterprise target task count',
    errors,
  )
  requireRecordedCounts(
    objectAt(
      taskMap?.statistics?.targetStableTasksCreatedByRepository,
      'taskMap.statistics.targetStableTasksCreatedByRepository',
      errors,
    ),
    createdCounts,
    'recorded created target task counts',
    errors,
  )
  requireCount(
    taskMap?.statistics?.targetStableTasksCreated,
    EXPECTED_TARGET_TASKS,
    'recorded created target task total',
    errors,
  )

  const batches = arrayAt(taskMap?.targetCreationBatches, 'taskMap.targetCreationBatches', errors)
  const batchedStableIds = batches.flatMap(batch => (
    arrayAt(batch?.stableTaskIds, `batch ${batch?.batchId}.stableTaskIds`, errors)
  ))
  requireCount(batchedStableIds.length, EXPECTED_STABLE_TASKS, 'batched stable task count', errors)
  requireUniqueStrings(batchedStableIds, 'batched stable task IDs', errors)
  if (!sameStrings(batchedStableIds, stableTaskIds)) {
    errors.push('target creation batches must cover the stable task set exactly once')
  }

  const enterprisePaths = arrayAt(
    inventoryOwnership['winwincode-enterprise'],
    'inventory Enterprise direct paths',
    errors,
  )
  requireCount(
    enterprisePaths.length,
    inventory?.currentHeadClassification?.enterpriseDirectMigrationPathCount ?? 0,
    'current HEAD Enterprise direct path count',
    errors,
  )
  requireUniqueStrings(enterprisePaths, 'Enterprise direct paths', errors)
  enterprisePaths.forEach((path, index) => {
    requireRepositoryRelative(path, `Enterprise direct path ${index}`, errors)
  })
  requireCount(
    inventory?.summary?.classifiedPathCountByBucket?.['winwincode-enterprise'],
    enterprisePaths.length,
    'recorded current HEAD Enterprise classified path count',
    errors,
  )
  requireCount(
    inventory?.summary?.historyPreservingMovePathCount,
    EXPECTED_ENTERPRISE_DIRECT_PATHS,
    'recorded history-preserving move count',
    errors,
  )
  const moves = arrayAt(
    inventory?.historyPreservingMoves,
    'inventory.historyPreservingMoves',
    errors,
  )
  requireCount(
    moves.length,
    EXPECTED_ENTERPRISE_DIRECT_PATHS,
    'history-preserving move count',
    errors,
  )
  const movePaths = moves.map(move => move?.path)
  requireUniqueStrings(movePaths, 'history-preserving move paths', errors)
  for (const [index, move] of moves.entries()) {
    requireRepositoryRelative(move?.path, `history-preserving move ${index}`, errors)
    if (move?.toRepository !== 'winwincode-enterprise') {
      errors.push(`history-preserving move ${move?.path} must target winwincode-enterprise`)
    }
  }

  const mixedSeams = arrayAt(
    inventory?.highRiskMixedPaths,
    'inventory.highRiskMixedPaths',
    errors,
  )
  requireCount(
    mixedSeams.length,
    inventory?.currentHeadClassification?.mixedSeamPathCount ?? 0,
    'current HEAD mixed seam count',
    errors,
  )
  const mixedPaths = mixedSeams.map(seam => seam?.path)
  requireUniqueStrings(mixedPaths, 'mixed seam paths', errors)
  mixedPaths.forEach((path, index) => {
    requireRepositoryRelative(path, `mixed seam path ${index}`, errors)
  })
  requireCount(
    inventory?.summary?.highRiskMixedPathCount,
    mixedSeams.length,
    'recorded current HEAD mixed seam count',
    errors,
  )
  const migrationReviewPaths = new Set(arrayAt(
    inventoryOwnership['migration-review'],
    'inventory migration-review paths',
    errors,
  ))
  const temporarilyUnclassifiedPaths = new Set(arrayAt(
    inventory?.temporarilyUnclassifiedPaths,
    'inventory temporarily unclassified paths',
    errors,
  ))
  for (const path of mixedPaths) {
    if (!migrationReviewPaths.has(path)) errors.push(`mixed seam is not in migration-review: ${path}`)
    if (!temporarilyUnclassifiedPaths.has(path)) {
      errors.push(`mixed seam is not temporarily unclassified: ${path}`)
    }
  }

  if (errors.length > 0) fail(errors)
  return Object.freeze({
    repositoryIds: Object.freeze([...EXPECTED_REPOSITORIES]),
    stableTasks: stableTasks.length,
    targetTasks: createdTargetTasks.length,
    cloudTasks: createdCounts['winwincode-cloud'],
    enterpriseTasks: createdCounts['winwincode-enterprise'],
    mixedSeams: mixedSeams.length,
    enterpriseDirectPaths: enterprisePaths.length,
    communityPrunePermission: inventory?.currentHeadCommunityDisposition?.communityPrunePermission?.status,
  })
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

export function validateRepositorySplitFiles(root) {
  const inventory = readJson(resolve(root, 'docs/decisions/0031-repository-split.inventory.json'))
  const includedRoots = inventory?.currentHead?.includedRoots
  if (!Array.isArray(includedRoots) || includedRoots.length === 0) {
    throw new Error('REPOSITORY_SPLIT_METADATA_INVALID: inventory.currentHead.includedRoots must be non-empty')
  }
  const sourcePaths = trackedPaths(root).filter(path => includedRoots.includes(path.split('/')[0]))
  return validateRepositorySplitMetadata({
    editions: readJson(resolve(root, 'docs/decisions/0031-product-editions.json')),
    inventory,
    taskMap: readJson(resolve(root, 'docs/decisions/0031-task-repository-map.json')),
    repositoryRoot: root,
    trackedPaths: sourcePaths,
  })
}

function main(argv) {
  if (argv.length !== 0) {
    throw new Error('REPOSITORY_SPLIT_METADATA_INVALID: this verifier accepts no arguments')
  }
  const root = resolve(import.meta.dirname, '..')
  const result = validateRepositorySplitFiles(root)
  process.stdout.write(`${JSON.stringify({ status: 'passed', ...result })}\n`)
}

const isMain = process.argv[1] !== undefined
  && pathToFileURL(process.argv[1]).href === import.meta.url
if (isMain) main(process.argv.slice(2))
