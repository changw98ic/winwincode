// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync('corepack', [
  'pnpm', 'exec', 'tsc',
  '-p', 'apps/client/tsconfig.strongflow-page-tests.json',
  '--pretty', 'false',
  '--incremental', 'false',
], { cwd: root, encoding: 'utf8' })
assert.equal(
  compiler.status,
  0,
  `StrongFlow Candidate modules did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const candidateModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-candidate.js',
)).href}`)

const {
  candidateFileSummary,
  candidateFileTreeRows,
} = candidateModule

const files = [{
  path: 'src/new.ts',
  oldPath: null,
  status: 'added',
  additions: 9,
  deletions: 0,
  binary: false,
  encoding: 'utf-8',
}, {
  path: 'src/core/app.ts',
  oldPath: null,
  status: 'modified',
  additions: 7,
  deletions: 2,
  binary: false,
  encoding: 'utf-8',
}, {
  path: 'src/current.ts',
  oldPath: 'src/legacy.ts',
  status: 'renamed',
  additions: 1,
  deletions: 1,
  binary: false,
  encoding: 'utf-8',
}, {
  path: 'public/logo.png',
  oldPath: null,
  status: 'modified',
  additions: null,
  deletions: null,
  binary: true,
  encoding: 'binary',
}, {
  path: 'docs/legacy.md',
  oldPath: null,
  status: 'deleted',
  additions: 0,
  deletions: 12,
  binary: false,
  encoding: 'utf-8',
}, {
  path: 'vendor/data.txt',
  oldPath: null,
  status: 'type_changed',
  additions: null,
  deletions: null,
  binary: false,
  encoding: 'unknown-8bit',
}]

test('Candidate file summary distinguishes statuses, line totals, binary, and preview availability', () => {
  assert.deepEqual(candidateFileSummary(files), {
    total: 6,
    additions: 17,
    deletions: 15,
    binary: 1,
    unavailable: 2,
    statuses: {
      added: 1,
      modified: 2,
      deleted: 1,
      renamed: 1,
      copied: 0,
      type_changed: 1,
    },
  })
})

test('Candidate tree groups directories and applies search, status, collapse, and a visible bound', () => {
  const all = candidateFileTreeRows(files, {
    search: '',
    status: 'all',
    collapsedDirectories: new Set(),
    selectedPath: 'src/core/app.ts',
    limit: 200,
  })
  assert.deepEqual(all.rows.map(row => [row.kind, row.path, row.depth]), [
    ['directory', 'docs', 1],
    ['file', 'docs/legacy.md', 2],
    ['directory', 'public', 1],
    ['file', 'public/logo.png', 2],
    ['directory', 'src', 1],
    ['directory', 'src/core', 2],
    ['file', 'src/core/app.ts', 3],
    ['file', 'src/current.ts', 2],
    ['file', 'src/new.ts', 2],
    ['directory', 'vendor', 1],
    ['file', 'vendor/data.txt', 2],
  ])
  assert.equal(all.rows.find(row => row.path === 'src/core/app.ts').selected, true)

  const filtered = candidateFileTreeRows(files, {
    search: 'legacy',
    status: 'renamed',
    collapsedDirectories: new Set(['src']),
    selectedPath: null,
    limit: 200,
  })
  assert.deepEqual(filtered.rows.map(row => row.path), ['src', 'src/current.ts'])
  assert.equal(filtered.totalMatches, 1)

  const collapsed = candidateFileTreeRows(files, {
    search: '',
    status: 'all',
    collapsedDirectories: new Set(['src']),
    selectedPath: null,
    limit: 4,
  })
  assert.deepEqual(collapsed.rows.map(row => row.path), [
    'docs',
    'docs/legacy.md',
    'public',
    'public/logo.png',
  ])
  assert.equal(collapsed.hiddenRows > 0, true)
})
