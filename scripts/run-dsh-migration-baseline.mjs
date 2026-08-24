#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const inventoryPath = resolve(
  root,
  'docs/decisions/0028-control-plane-worker-migration.inventory.json',
)
const inventory = JSON.parse(readFileSync(inventoryPath, 'utf8'))

function regularExpressionLiteral(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&')
}

const baselines = inventory.behaviorBaselines
const testFiles = [...new Set(baselines.map(baseline => baseline.testFile))].sort()
const testNamePattern = `^(?:${baselines
  .map(baseline => regularExpressionLiteral(baseline.testName))
  .join('|')})$`
const result = spawnSync(process.execPath, [
  '--test',
  '--test-concurrency=1',
  `--test-name-pattern=${testNamePattern}`,
  ...testFiles,
], {
  cwd: root,
  env: process.env,
  stdio: 'inherit',
})

if (result.error !== undefined) throw result.error
if (result.signal !== null) {
  throw new Error(`DSH migration baseline ended with ${result.signal}`)
}
if (result.status !== 0) process.exit(result.status ?? 1)

console.log(JSON.stringify({
  schemaVersion: inventory.schemaVersion,
  status: 'passed',
  scenarios: baselines.map(({ id, scenario, testFile, testName }) => ({
    id,
    scenario,
    testFile,
    testName,
  })),
}))
