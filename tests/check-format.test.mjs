import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')

test('source formatting ignores generated test results', () => {
  const testResults = join(root, 'test-results')
  mkdirSync(testResults, { recursive: true })
  const fixture = mkdtempSync(join(testResults, 'check-format-'))
  writeFileSync(join(fixture, 'generated.json'), '{"generated":true}')

  try {
    const result = spawnSync(process.execPath, ['scripts/check-format.mjs'], {
      cwd: root,
      encoding: 'utf8',
    })
    assert.equal(result.status, 0, result.stderr)
  } finally {
    rmSync(fixture, { recursive: true, force: true })
  }
})
