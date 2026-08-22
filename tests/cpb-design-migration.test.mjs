import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  cpbStatePathReason,
  findCpbRuntimeMarker,
  findForbiddenPackageDependencies,
  scanPackedPackageCpbBoundary,
  scanRepositoryCpbBoundary,
} from '../scripts/cpb-boundary-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const decisionPath = join(root, 'docs', 'decisions', '0022-cpb-design-knowledge-migration.md')
const sourceCommit = '68bb0d591b0333b57a8be863458367123885b52a'
const representativeSources = Object.freeze([
  'docs/architecture/runtime-boundaries.md',
  'docs/architecture/cpb-trace-replay.md',
  'docs/architecture/cpb-hub-registry-consistency.md',
  'docs/architecture/cpb-coding-comparison.md',
  'docs/architecture/cpb-v0.5-runtime-release-stabilization-spec.md',
  'docs/security/cpb-agent-secret-boundary.md',
  'docs/product/cpb-full-product-vision-plan.md',
  'docs/product/cpb-closed-loop-mvp-plan.md',
  'docs/product/cpb-product-entry-execution-kernel-plan-2026-07-27.md',
  'docs/product/cpb-runtime-independent-evolution-plan.md',
  'docs/product/cpb-flagship-validation-gate.md',
  'docs/multi-agent-orchestration-roadmap.md',
  'docs/superpowers/specs/2026-06-12-checklist-first-task-verification-design.md',
  'docs/superpowers/plans/2026-05-13-24h-unattended-fixed-role-agents.md',
  'docs/superpowers/plans/2026-07-28-cpb-agent-platform-maturity-rfc.md',
])

test('CPB design inventory is pinned, classified, and linked from the README', () => {
  const decision = readFileSync(decisionPath, 'utf8')
  const readme = readFileSync(join(root, 'README.md'), 'utf8')

  assert.match(decision, new RegExp(sourceCommit, 'u'))
  for (const path of representativeSources) {
    assert.equal(decision.includes(`\`${path}\``), true, `missing source inventory entry ${path}`)
  }
  for (const disposition of ['采用并重写', '采用原则并重写', '只采用证明原则', '删除，不迁移']) {
    assert.match(decision, new RegExp(disposition, 'u'))
  }
  assert.match(decision, /不提供 CPB 数据迁移器/u)
  assert.match(decision, /未提交内容；这些文件以及它们表达的新增方案都没有进入本次迁移/u)
  assert.match(decision, /docs\/product\/evidence\/\*\*/u)
  assert.match(decision, /winwincode-9c4\.9\.5/u)
  assert.match(decision, /winwincode-9c4\.9\.6/u)
  assert.match(readme, /0022-cpb-design-knowledge-migration\.md/u)
})

test('current product source has no CPB runtime dependency or internal state path', () => {
  const errors = scanRepositoryCpbBoundary(root)
  assert.deepEqual(errors, [], errors.join('\n'))
})

test('boundary scanner rejects representative CPB runtime inputs', () => {
  assert.equal(findCpbRuntimeMarker('const root = process.env.CPB_ROOT')?.label, 'CPB environment or configuration key')
  assert.equal(findCpbRuntimeMarker("import runtime from '@codepatchbay/runtime'")?.label, 'CodePatchBay package or runtime name')
  assert.equal(findCpbRuntimeMarker('cpb stream --port 4318')?.label, 'CPB runtime command')
  assert.equal(cpbStatePathReason('package/.cpb/jobs.sqlite'), 'contains a .cpb state directory')
  assert.equal(cpbStatePathReason('docs/decisions/0022-cpb-design-knowledge-migration.md'), undefined)
  assert.deepEqual(
    findForbiddenPackageDependencies({ dependencies: { '@codepatchbay/runtime': '1.0.0' } }),
    ['dependencies contains forbidden package @codepatchbay/runtime'],
  )
})

test('repository scanner works in a clean source tree without Git metadata', t => {
  const cleanRoot = mkdtempSync(join(tmpdir(), 'winwincode-cpb-clean-source-'))
  t.after(() => rmSync(cleanRoot, { force: true, recursive: true }))
  mkdirSync(join(cleanRoot, 'apps', 'fixture'), { recursive: true })
  writeFileSync(join(cleanRoot, 'package.json'), `${JSON.stringify({
    name: '@winwincode/clean-source-fixture',
    version: '1.0.0',
  }, null, 2)}\n`)
  writeFileSync(join(cleanRoot, 'apps', 'fixture', 'index.js'), 'export const stateRoot = process.env.CPB_ROOT\n')

  const errors = scanRepositoryCpbBoundary(cleanRoot)
  assert.deepEqual(errors, [
    'apps/fixture/index.js: contains CPB environment or configuration key',
  ])
})

test('published package scanner rejects CPB state and runtime content', t => {
  const packageDirectory = mkdtempSync(join(tmpdir(), 'winwincode-cpb-package-boundary-'))
  t.after(() => rmSync(packageDirectory, { force: true, recursive: true }))
  mkdirSync(join(packageDirectory, 'dist'))
  mkdirSync(join(packageDirectory, '.cpb'))
  writeFileSync(join(packageDirectory, 'package.json'), `${JSON.stringify({
    name: '@winwincode/boundary-fixture',
    version: '1.0.0',
    dependencies: { '@codepatchbay/runtime': '1.0.0' },
  }, null, 2)}\n`)
  writeFileSync(join(packageDirectory, 'dist', 'index.js'), 'export const runtimeRoot = process.env.CPB_ROOT\n')
  writeFileSync(join(packageDirectory, '.cpb', 'jobs.jsonl'), '{}\n')

  const errors = scanPackedPackageCpbBoundary({
    packageDirectory,
    files: ['package.json', 'dist/index.js', '.cpb/jobs.jsonl'],
  })
  assert.equal(errors.some(error => error.includes('forbidden package @codepatchbay/runtime')), true)
  assert.equal(errors.some(error => error.includes('CPB environment or configuration key')), true)
  assert.equal(errors.some(error => error.includes('contains a .cpb state directory')), true)
})
