// SPDX-License-Identifier: Apache-2.0

import { execFileSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { signalIfPresent } from './chat-execution-transport-probe.mjs'

function processState(pid) {
  if (process.platform === 'linux') {
    return /^State:\s+(\S)/mu.exec(readFileSync(`/proc/${String(pid)}/status`, 'utf8'))?.[1]
  }
  return execFileSync('ps', ['-p', String(pid), '-o', 'stat='], { encoding: 'utf8' }).trim()[0]
}

export async function pauseManagedProcess(pid, idle, {
  timeoutMillis = 3000, readState = processState, signal = signalIfPresent,
} = {}) {
  const deadline = performance.now() + timeoutMillis
  let paused = false
  try {
    while (performance.now() < deadline) {
      if (!paused) {
        if (!signal(pid, 'SIGSTOP')) throw new Error('owned managed process exited before pause')
        paused = true
      }
      if (readState(pid) === 'T') {
        const ready = idle()
        if (ready) return ready
        signal(pid, 'SIGCONT')
        paused = false
      }
      await new Promise(resolvePromise => setTimeout(resolvePromise, 10))
    }
    throw new Error('owned managed process did not reach a stopped idle state')
  } catch (error) {
    if (paused) signal(pid, 'SIGCONT')
    throw error
  }
}
