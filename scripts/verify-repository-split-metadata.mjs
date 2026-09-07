#!/usr/bin/env node

import { isAbsolute, relative, resolve } from 'node:path'
import { readFileSync } from 'node:fs'
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
const EXPECTED_MIXED_SEAMS = 17
const EXPECTED_ENTERPRISE_DIRECT_PATHS = 103

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

export function validateRepositorySplitMetadata({ editions, inventory, taskMap }) {
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
    EXPECTED_ENTERPRISE_DIRECT_PATHS,
    'Enterprise direct path count',
    errors,
  )
  requireUniqueStrings(enterprisePaths, 'Enterprise direct paths', errors)
  enterprisePaths.forEach((path, index) => {
    requireRepositoryRelative(path, `Enterprise direct path ${index}`, errors)
  })
  requireCount(
    inventory?.summary?.classifiedPathCountByBucket?.['winwincode-enterprise'],
    EXPECTED_ENTERPRISE_DIRECT_PATHS,
    'recorded Enterprise classified path count',
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
  if (!sameStrings(movePaths, enterprisePaths)) {
    errors.push('history-preserving move paths must exactly match Enterprise direct paths')
  }
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
  requireCount(mixedSeams.length, EXPECTED_MIXED_SEAMS, 'mixed seam count', errors)
  const mixedPaths = mixedSeams.map(seam => seam?.path)
  requireUniqueStrings(mixedPaths, 'mixed seam paths', errors)
  mixedPaths.forEach((path, index) => {
    requireRepositoryRelative(path, `mixed seam path ${index}`, errors)
  })
  requireCount(
    inventory?.summary?.highRiskMixedPathCount,
    EXPECTED_MIXED_SEAMS,
    'recorded mixed seam count',
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
  })
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

export function validateRepositorySplitFiles(root) {
  return validateRepositorySplitMetadata({
    editions: readJson(resolve(root, 'docs/decisions/0031-product-editions.json')),
    inventory: readJson(resolve(root, 'docs/decisions/0031-repository-split.inventory.json')),
    taskMap: readJson(resolve(root, 'docs/decisions/0031-task-repository-map.json')),
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
