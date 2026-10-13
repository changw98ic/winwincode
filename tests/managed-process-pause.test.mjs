// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { once } from 'node:events'
import test from 'node:test'
import { pauseManagedProcess } from './fixtures/managed-process-pause.mjs'
import { signalIfPresent } from './fixtures/chat-execution-transport-probe.mjs'

test('pause waits for STOP delivery before considering the idle guard', async () => {
  const signals = []
  const states = ['R', 'T']
  let idleCalls = 0
  const paused = await pauseManagedProcess(123, () => { idleCalls += 1; return true }, {
    readState: () => states.shift() ?? 'T',
    signal: (_pid, signal) => { signals.push(signal); return true },
  })
  assert.equal(paused, true)
  assert.equal(idleCalls, 1)
  assert.deepEqual(signals, ['SIGSTOP'])
})

test('pause resumes a stopped writer and restarts the handshake', async () => {
  const signals = []
  let checks = 0
  await pauseManagedProcess(123, () => ++checks === 2, {
    readState: () => 'T',
    signal: (_pid, signal) => { signals.push(signal); return true },
  })
  assert.deepEqual(signals, ['SIGSTOP', 'SIGCONT', 'SIGSTOP'])
})

test('pause failure always resumes the owned process', async () => {
  const signals = []
  const original = new Error('idle read failed')
  await assert.rejects(pauseManagedProcess(123, () => { throw original }, {
    readState: () => 'T',
    signal: (_pid, signal) => { signals.push(signal); return true },
  }), error => error === original)
  assert.deepEqual(signals, ['SIGSTOP', 'SIGCONT'])
})

test('pause confirms and resumes an actual owned process', async t => {
  const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' })
  const exited = once(child, 'exit')
  t.after(async () => {
    signalIfPresent(child.pid, 'SIGCONT')
    signalIfPresent(child.pid, 'SIGTERM')
    await exited
  })
  await once(child, 'spawn')
  assert.equal(await pauseManagedProcess(child.pid, () => true), true)
  assert.equal(signalIfPresent(child.pid, 'SIGCONT'), true)
})
