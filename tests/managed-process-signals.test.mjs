// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { once } from 'node:events'
import test from 'node:test'

import { signalIfPresent } from './fixtures/chat-execution-transport-probe.mjs'

test('managed process signaling accepts a normally reaped process', async () => {
  const child = spawn(process.execPath, ['-e', 'process.exit(0)'], { stdio: 'ignore' })
  const [code] = await once(child, 'exit')
  assert.equal(code, 0)
  assert.equal(signalIfPresent(child.pid, 'SIGSTOP'), false)
  assert.equal(signalIfPresent(child.pid, 'SIGCONT'), false)
})

test('managed process signaling preserves active signals and unexpected errors', async t => {
  const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' })
  const exited = once(child, 'exit')
  t.after(async () => {
    signalIfPresent(child.pid, 'SIGTERM')
    await exited
  })
  await once(child, 'spawn')
  assert.equal(signalIfPresent(child.pid, 0), true)
  assert.throws(() => signalIfPresent(child.pid, 'INVALID_MANAGED_SIGNAL'), { code: 'ERR_UNKNOWN_SIGNAL' })
  assert.equal(signalIfPresent(child.pid, 'SIGTERM'), true)
  const [code, signal] = await exited
  assert.equal(code, null)
  assert.equal(signal, 'SIGTERM')
})
