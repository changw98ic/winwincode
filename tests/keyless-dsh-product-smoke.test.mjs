import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')

function keylessEnvironment() {
  return Object.fromEntries(Object.entries(process.env).filter(([name]) => (
    !/(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/u.test(name)
  )))
}

test('fresh DSH home runs stock Chat and a StrongFlow role through one keyless kernel', () => {
  const child = spawnSync(process.execPath, [
    resolve(root, 'tests/fixtures/keyless-dsh-product-smoke.mjs'),
  ], {
    cwd: root,
    encoding: 'utf8',
    env: keylessEnvironment(),
    timeout: 45_000,
  })

  assert.equal(child.signal, null, `product smoke terminated by ${child.signal ?? 'no signal'}`)
  assert.equal(child.status, 0, child.stderr || child.stdout)
  const report = JSON.parse(child.stdout.trim().split('\n').at(-1))
  assert.deepEqual(report.surfaces, [
    { id: 'chat', label: 'Chat', default: true },
    { id: 'strongflow', label: 'StrongFlow', default: false },
  ])
  assert.equal(report.kernelCreations, 1)
  assert.equal(new Set(report.kernelSessionIds).size, 2)
  assert.deepEqual(report.roles, ['chat', 'requirements'])
  assert.deepEqual(report.calls.map(call => [call.provider, call.model]), [
    ['fixture', 'fixture-coder'],
    ['fixture', 'fixture-coder'],
  ])
  assert.deepEqual(report.assistantMessages, [
    'stock chat complete',
    'requirements role complete',
  ])
  for (const events of Object.values(report.runtimeEvents)) {
    assert.ok(events.includes('turn.started'))
    assert.ok(events.includes('message.completed'))
    assert.ok(events.includes('turn.completed'))
  }
  assert.equal(report.roleSourcesMatch, true)
  assert.deepEqual(report.credentialEnvironment, [])
  assert.deepEqual(report.shutdown, {
    completed: [],
    submitFailed: [],
    timedOut: [],
  })
})
