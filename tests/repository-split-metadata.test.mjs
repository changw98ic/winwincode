import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  validateRepositorySplitMetadata,
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

test('current repository split documents agree on repositories, tasks, and source counts', () => {
  assert.deepEqual(validateRepositorySplitMetadata(fixture()), {
    repositoryIds: ['winwincode', 'winwincode-cloud', 'winwincode-enterprise'],
    stableTasks: 178,
    targetTasks: 49,
    cloudTasks: 32,
    enterpriseTasks: 17,
    mixedSeams: 17,
    enterpriseDirectPaths: 103,
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
      repositoryIds: ['winwincode', 'winwincode-cloud', 'winwincode-enterprise'],
      stableTasks: 178,
      targetTasks: 49,
      cloudTasks: 32,
      enterpriseTasks: 17,
      mixedSeams: 17,
      enterpriseDirectPaths: 103,
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
      /mixed seam count/u,
    )
  })
  await t.test('Enterprise direct path', () => {
    rejected(
      documents => {
        documents.inventory.ownership['winwincode-enterprise'].pop()
      },
      /Enterprise direct path count/u,
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
        const previous = documents.inventory.ownership['winwincode-enterprise'][0]
        documents.inventory.ownership['winwincode-enterprise'][0] = absolute
        const move = documents.inventory.historyPreservingMoves.find(
          entry => entry.path === previous,
        )
        move.path = absolute
      },
      /must not reference an absolute or parent repository path/u,
    )
  })
})
