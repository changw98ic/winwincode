import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const source = file => readFileSync(join(root, file), 'utf8')
const contract = JSON.parse(source('config/engineering-runtime/runtime-contract.design.json'))
const backlog = JSON.parse(source('config/engineering-runtime/backlog-migration.json'))

test('accepted E00 designs retain exact source evidence and implementation boundaries', () => {
  assert.equal(contract.status, 'accepted_implemented')
  assert.equal(contract.source.planSha256, backlog.source_plan_sha256)
  assert.equal(contract.source.baseline, backlog.baseline_head)
  assert.doesNotMatch(JSON.stringify(contract), /\/(?:Users|Volumes|private|tmp)\//)
  const result = execFileSync(process.execPath, ['config/engineering-runtime/runtime-contract.design.check.mjs'], { cwd: root, encoding: 'utf8' })
  assert.match(result, /8 source hashes/)
})
test('alpha cutover keeps one WorkRun path without a legacy migration subsystem', () => {
  assert.equal(contract.cardinality['WorkRun.executionAttempt'], 'exactly 1')
  assert.equal(contract.cardinality['WorkRun.acceptedCandidate'], '0..1')
  assert.doesNotMatch(JSON.stringify(contract), /StageRun|stageRun|stage_run/)
  for (const file of [
    'crates/winwincode-delivery/src/workrun_migration.rs',
    'crates/winwincode-delivery/src/sqlite_workrun_migration.rs',
    'crates/winwincode-delivery/src/bin/migrate_workrun.rs',
    'config/engineering-runtime/workrun-migration.design.json',
  ]) assert.equal(existsSync(join(root, file)), false, file)
  assert.doesNotMatch(source('crates/winwincode-delivery/src/lib.rs'), /workrun_migration/)
  assert.doesNotMatch(source('crates/winwincode-delivery/Cargo.toml'), /rusqlite/)
})
test('snapshot and design checks are registered in the existing CI test lane', () => {
  const runner = source('scripts/acceptance/run-ts-tests.mjs')
  for (const file of ['tests/engineering-runtime-backlog.test.mjs', 'tests/engineering-runtime-design.test.mjs']) {
    assert.ok(runner.includes(`'${file}'`))
  }
  assert.match(source('.github/workflows/mainline.yml'), /corepack pnpm verify:typescript/)
  const scripts = JSON.parse(source('package.json')).scripts
  assert.ok(scripts['verify:typescript'].includes('pnpm test:ts'))
  assert.ok(scripts['test:ts'].includes('node scripts/acceptance/run-ts-tests.mjs'))
})
