#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { readdirSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const testsDirectory = join(root, 'tests')
const serialProcessTests = Object.freeze([
  'tests/live-evaluation-runner.test.mjs',
])
const testFiles = readdirSync(testsDirectory, { withFileTypes: true })
  .filter(entry => entry.isFile() && entry.name.endsWith('.test.mjs'))
  .map(entry => `tests/${entry.name}`)
  .sort()

for (const path of serialProcessTests) {
  if (!testFiles.includes(path)) {
    throw new Error(`required process-boundary test is missing: ${path}`)
  }
}

function runTests(arguments_) {
  const result = spawnSync(process.execPath, arguments_, {
    cwd: root,
    stdio: 'inherit',
  })
  if (result.error !== undefined) throw result.error
  if (result.signal !== null) {
    throw new Error(`Node test runner ended with ${result.signal}`)
  }
  if (result.status !== 0) process.exit(result.status ?? 1)
}

const parallelTests = testFiles.filter(path => !serialProcessTests.includes(path))
runTests(['--test', '--test-concurrency=4', ...parallelTests])

// Process-boundary tests run after the parallel suite so their nested gates
// observe the repository without competing test-runner load.
for (const path of serialProcessTests) runTests(['--test', path])

// The full ten-scenario oracle runs after the parallel Node suite and reuses
// the TypeScript build already produced by test:ts.
runTests(['scripts/export-delivery-strongflow-oracle.mjs', '--check'])
