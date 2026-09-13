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
    assert.equal(evidence.lanes[lane].success, true, lane)
    assert.equal(evidence.lanes[lane].reject, true, lane)
    assert.equal(evidence.lanes[lane].replay, true, lane)
  }
  const files = readdirSync(join(root, 'test-results/gates'))
  assert.ok(files.some(name => name.startsWith('wwx-gate-x4-')))
})

test('GATE-X4 evidence is bound to protocol and HEAD only', () => {
  const files = readdirSync(join(root, 'test-results/gates'))
    .filter(name => name.startsWith('wwx-gate-x4-'))
  assert.ok(files.length >= 1)
  const latest = files.sort().at(-1)
  const evidence = JSON.parse(
    readFileSync(join(root, 'test-results/gates', latest), 'utf8'),
  )
  assert.equal(evidence.protocolVersion, 'wwx-gate-x4/v1')
  assert.equal(evidence.result, 'pass')
  assert.match(evidence.gitHead, /^[0-9a-f]{40}$/)
  assert.equal(typeof evidence.lanes.automation.details.unmanaged, 'string')
  assert.equal(evidence.lanes.notification.details.schema, 'winwincode/v1')
})
