import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readdirSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')

test('GATE-X4 runner passes automation/notification/git-reflow/effect lanes', () => {
  const result = spawnSync(
    'node',
    ['scripts/run-wwx-gate-x4.mjs'],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(result.status, 0, result.stdout + result.stderr)
  const payload = JSON.parse(result.stdout)
  assert.equal(payload.gateId, 'WWX-GATE-X4')
  assert.equal(payload.protocolVersion, 'wwx-gate-x4/v1')
  assert.equal(payload.result, 'pass')
  assert.match(payload.gitHead, /^[0-9a-f]{40}$/)
  for (const lane of ['automation', 'notification', 'gitReflow', 'effect']) {
    assert.equal(payload.lanes[lane], 'pass', lane)
  }
  const evidence = JSON.parse(readFileSync(payload.outputPath, 'utf8'))
  assert.equal(evidence.gitHead, payload.gitHead)
  for (const lane of ['automation', 'notification', 'gitReflow', 'effect']) {
    assert.equal(evidence.lanes[lane].status, 'passed', lane)
    assert.equal(evidence.lanes[lane].exitCode, 0, lane)
    assert.match(evidence.lanes[lane].stdoutSha256, /^[0-9a-f]{64}$/u, lane)
    assert.match(evidence.lanes[lane].stderrSha256, /^[0-9a-f]{64}$/u, lane)
    assert.ok(evidence.lanes[lane].coverage.includes('success'), lane)
    assert.ok(evidence.lanes[lane].coverage.includes('reject'), lane)
    assert.ok(evidence.lanes[lane].coverage.some(value => value.includes('replay')), lane)
  }
  assert.deepEqual(
    evidence.lanes.automation.command,
    ['cargo', 'test', '-p', 'winwincode-control-plane', '--test', 'automation_recipe', '--locked'],
  )
  assert.deepEqual(
    evidence.lanes.notification.command,
    ['node', '--test', 'tests/attention-notifications-client.test.mjs'],
  )
  const files = readdirSync(join(root, 'test-results/gates'))
  assert.ok(files.some(name => name.startsWith('wwx-gate-x4-')))
})
