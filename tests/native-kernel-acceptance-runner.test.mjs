// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { runNativeKernelAcceptance, selectKernelProcess } from '../scripts/run-native-kernel-acceptance.mjs'

function blockedCommand(t, exitCode) {
  const root = mkdtempSync(join(tmpdir(), 'wwc-native-probe-')), release = join(root, 'release')
  let pid
  t.after(() => {
    if (pid !== undefined) { try { process.kill(pid, 'SIGTERM') } catch {} }
    rmSync(root, { recursive: true, force: true })
  })
  return {
    args: ['-e', `const fs = require('node:fs'); setInterval(() => {
      if (fs.existsSync(${JSON.stringify(release)})) process.exit(${exitCode});
    }, 5);`],
    observe: value => { pid = value; process.kill(pid, 0) },
    release: () => writeFileSync(release, ''),
    finished: () => { pid = undefined },
  }
}

test('startup sampling selects only a Kernel test owned by this Cargo process', () => {
  const table = `
  10 1 /tools/cargo
  11 10 /build directory/code_mode_kernel-a123
  12 11 /tools/node
  20 1 /tools/cargo
  21 20 /other/code_mode_kernel-a123
  22 21 /tools/node
  `
  assert.equal(selectKernelProcess(table, 10)?.pid, 11)
  assert.equal(selectKernelProcess(table, 20)?.pid, 21)
  assert.equal(selectKernelProcess(table, 30), undefined)
  assert.equal(selectKernelProcess('10 1 /tools/cargo', 10), undefined)
})

test('startup evidence is captured while the failed test runs and preserves its exit code', { timeout: 5_000 }, async t => {
  const command = blockedCommand(t, 17)
  let inspections = 0
  const samples = []
  const result = await runNativeKernelAcceptance(process.execPath, command.args, {
    platform: 'darwin', stdio: 'pipe', captureAfterMillis: 0, pollMillis: 5,
    inspect: async rootPid => {
      inspections += 1
      // Compilation has no Kernel child to sample. The test appears later.
      if (inspections < 3) return ''
      command.observe(rootPid)
      return `${process.pid + 100_000} ${process.pid + 200_000} /irrelevant/code_mode_kernel-other\n`
        + `${process.pid + 300_000} ${rootPid} /build/code_mode_kernel-owned`
    },
    capture: async target => {
      samples.push(target.pid)
      assert.equal(inspections, 3)
      command.release()
    },
  })
  command.finished()
  assert.deepEqual(result, { code: 17, signal: null })
  assert.deepEqual(samples, [process.pid + 300_000])
})

test('non-macOS acceptance preserves success without invoking the macOS sampler', async () => {
  const result = await runNativeKernelAcceptance(process.execPath, ['-e', 'process.exit(0)'], {
    platform: 'linux', stdio: 'pipe',
    inspect: async () => assert.fail('Linux must not invoke sample'),
  })
  assert.deepEqual(result, { code: 0, signal: null })
})

test('failed sampling preserves the command failure and does not retry the test', { timeout: 5_000 }, async t => {
  const command = blockedCommand(t, 23)
  let attempts = 0
  const result = await runNativeKernelAcceptance(process.execPath, command.args, {
    platform: 'darwin', stdio: 'pipe', captureAfterMillis: 0, pollMillis: 5,
    inspect: async rootPid => { command.observe(rootPid); return `${rootPid + 1} ${rootPid} /build/code_mode_kernel-owned` },
    capture: async () => {
      attempts += 1
      command.release()
      throw Object.assign(new Error('sampler failed'), { code: 'EACCES' })
    },
  })
  command.finished()
  assert.deepEqual(result, { code: 23, signal: null })
  assert.equal(attempts, 1)
})

test('acceptance preserves test termination by signal', async () => {
  const result = await runNativeKernelAcceptance(process.execPath, ['-e', 'process.kill(process.pid, "SIGTERM")'], {
    platform: 'linux', stdio: 'pipe',
  })
  assert.deepEqual(result, { code: null, signal: 'SIGTERM' })
})
