import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import test from 'node:test'

import {
  scanProductRepositoryBoundary,
  validateEditionContract,
  validateRepositoryIdentity,
} from '../scripts/check-product-repository-boundary.mjs'

const ROOT = resolve(import.meta.dirname, '..')
const SCRIPT = join(ROOT, 'scripts/check-product-repository-boundary.mjs')
const CONTRACT_PATH = join(ROOT, 'docs/decisions/0031-product-editions.json')
const IDENTITY_PATH = join(ROOT, 'product-repository.json')

async function json(path) {
  return JSON.parse(await readFile(path, 'utf8'))
}

async function writeJson(path, value) {
  await mkdir(resolve(path, '..'), { recursive: true })
  await writeFile(path, `${JSON.stringify(value, null, 2)}\n`)
}

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'winwincode-repository-boundary-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  await writeJson(join(root, 'docs/decisions/0031-product-editions.json'), await json(CONTRACT_PATH))
  await writeJson(join(root, 'product-repository.json'), await json(IDENTITY_PATH))
  return root
}

function run(root, mode) {
  return spawnSync(process.execPath, [SCRIPT, '--root', root, '--mode', mode], {
    encoding: 'utf8',
  })
}

test('ADR-0031 contract and root identity define one Community repository boundary', async () => {
  const contract = await json(CONTRACT_PATH)
  const identity = await json(IDENTITY_PATH)

  assert.deepEqual(validateEditionContract(contract), [])
  assert.deepEqual(validateRepositoryIdentity(contract, identity), [])
  assert.deepEqual(Object.keys(contract.repositories).sort(), ['cloud', 'community', 'enterprise'])
  assert.equal(identity.product, 'community')
  assert.equal(identity.repository, contract.repositories.community.repository)
  assert.deepEqual(identity.allowedSourceOwners, ['winwincode'])
  assert.deepEqual(identity.allowedCoreReleaseOwners, [contract.communityCoreRelease.ownerRepository])
  assert.equal(contract.communityCoreRelease.localPathDependency, false)
})

test('audit reports foreign product files while enforce rejects the same repository', async t => {
  const root = await fixture(t)
  const foreignPath = join(root, 'apps/client/src/enterprise-application.ts')
  await mkdir(resolve(foreignPath, '..'), { recursive: true })
  await writeFile(foreignPath, 'export const product = "enterprise"\n')

  const audit = run(root, 'audit')
  assert.equal(audit.status, 0, audit.stderr)
  const auditReport = JSON.parse(audit.stdout)
  assert.equal(auditReport.mode, 'audit')
  assert.equal(auditReport.status, 'violations-found')
  assert.deepEqual(auditReport.violations, [
    {
      code: 'FOREIGN_PRODUCT_PATH',
      path: 'apps/client/src/enterprise-application.ts',
      product: 'enterprise',
      message: 'path belongs to the enterprise product repository',
    },
  ])

  const enforce = run(root, 'enforce')
  assert.equal(enforce.status, 1, enforce.stderr)
  const enforceReport = JSON.parse(enforce.stdout)
  assert.equal(enforceReport.mode, 'enforce')
  assert.deepEqual(enforceReport.violations, auditReport.violations)
})

test('cross-repository npm and Cargo paths are rejected while same-repository paths remain valid', async t => {
  const root = await fixture(t)
  await writeJson(join(root, 'apps/client/package.json'), {
    name: '@winwincode/client',
    dependencies: {
      '@winwincode/contracts': 'file:../../packages/contracts',
      '@winwincode/cloud-private': 'file:../../../winwincode-cloud/packages/private',
      '@winwincode/strongflow': 'workspace:*',
    },
  })
  await mkdir(join(root, 'packages/contracts'), { recursive: true })
  await mkdir(join(root, 'crates/community/src'), { recursive: true })
  await writeFile(
    join(root, 'crates/community/Cargo.toml'),
    `[package]\nname = "winwincode-community"\n\n[dependencies]\ninside = { path = "../../crates/shared" }\noutside = { path = "../../../winwincode-enterprise/crates/private" }\n`,
  )
  await mkdir(join(root, 'crates/shared'), { recursive: true })

  const report = await scanProductRepositoryBoundary({ root })
  const pathViolations = report.violations.filter(
    violation => violation.code === 'CROSS_REPOSITORY_PATH_DEPENDENCY',
  )
  assert.deepEqual(pathViolations, [
    {
      code: 'CROSS_REPOSITORY_PATH_DEPENDENCY',
      path: 'apps/client/package.json',
      dependency: 'dependencies.@winwincode/cloud-private',
      target: '../winwincode-cloud/packages/private',
      message: 'package dependency resolves outside this repository',
    },
    {
      code: 'CROSS_REPOSITORY_PATH_DEPENDENCY',
      path: 'crates/community/Cargo.toml',
      dependency: 'path:../../../winwincode-enterprise/crates/private',
      target: '../winwincode-enterprise/crates/private',
      message: 'Cargo path resolves outside this repository',
    },
  ])
})
