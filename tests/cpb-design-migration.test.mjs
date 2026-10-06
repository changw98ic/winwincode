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
} from '../scripts/lib/cpb-boundary-contract.mjs'

const root = resolve(import.meta.dirname, '..')
test('current product source has no CPB runtime dependency or internal state path', () => {
  const errors = scanRepositoryCpbBoundary(root)
  assert.deepEqual(errors, [], errors.join('\n'))
})

test('boundary scanner rejects representative CPB runtime inputs', () => {
  assert.equal(findCpbRuntimeMarker('const root = process.env.CPB_ROOT')?.label, 'CPB environment or configuration key')
  assert.equal(findCpbRuntimeMarker("import runtime from '@codepatchbay/runtime'")?.label, 'CodePatchBay package or runtime name')
  assert.equal(findCpbRuntimeMarker('cpb stream --port 4318')?.label, 'CPB runtime command')
  assert.equal(cpbStatePathReason('package/.cpb/jobs.sqlite'), 'contains a .cpb state directory')
  assert.equal(cpbStatePathReason('config/decisions/0022-cpb-design-knowledge-migration.md'), undefined)
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

test('published package scanner reads a workspace legal file inherited by pnpm', t => {
  const root = mkdtempSync(join(tmpdir(), 'winwincode-pnpm-inherited-file-'))
  const packageDirectory = join(root, 'packages', 'fixture')
  t.after(() => rmSync(root, { force: true, recursive: true }))
  mkdirSync(packageDirectory, { recursive: true })
  writeFileSync(join(root, 'LICENSE'), 'Apache License\n')

  const errors = scanPackedPackageCpbBoundary({
    packageDirectory,
    files: ['LICENSE'],
    inheritedFileDirectory: root,
  })
  assert.deepEqual(errors, [])
})
