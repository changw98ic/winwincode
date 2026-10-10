// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, openSync, writeSync, fsyncSync, closeSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { setTimeout as delay } from 'node:timers/promises'

const processIdentity = pid => {
  try { return execFileSync('ps', ['-p', String(pid), '-o', 'lstart='], { encoding: 'utf8' }).trim() || null }
  catch { return null }
}
const codeOf = error => /^[A-Z][A-Z0-9_]{0,127}$/u.test(error?.code ?? '') ? error.code : 'DEVICE_DRIVER_FAILED'
const fail = code => { throw Object.assign(new Error(code), { code }) }

// This journal owns supervision, not execution. Recovery observes the same
// registered Job and never submits a replacement model/task operation.
export function acquireDeviceTaskSupervisor({ directory, identity }) {
  identity = structuredClone(identity)
  const path = join(directory, 'task-supervision.json')
  const lock = join(directory, 'task-supervision.owner')
  const takeoverGuard = `${lock}.takeover`
  const owner = { id: randomUUID(), pid: process.pid, startedAt: processIdentity(process.pid) }
  assert.ok(owner.startedAt, 'supervisor requires an OS process identity')
  mkdirSync(directory, { recursive: true, mode: 0o700 })
  const durableWrite = (target, value) => {
    const temporary = `${target}.tmp-${owner.id}`
    const fd = openSync(temporary, 'wx', 0o600)
    try { writeSync(fd, `${JSON.stringify(value, null, 2)}\n`); fsyncSync(fd) } finally { closeSync(fd) }
    renameSync(temporary, target)
    const parent = openSync(dirname(target), 'r')
    try { fsyncSync(parent) } finally { closeSync(parent) }
  }
  let previous = existsSync(path) ? JSON.parse(readFileSync(path, 'utf8')) : null
  if (previous) assert.deepEqual(previous.identity, identity, 'supervision identity changed')
  let ownsTakeoverGuard = false
  try {
    // Serialize inspection through replacement, including first acquisition.
    // An unconfirmed guard remains blocked for explicit recovery.
    try { mkdirSync(takeoverGuard, { mode: 0o700 }); ownsTakeoverGuard = true } catch (guardError) {
      if (guardError.code !== 'EEXIST') throw guardError
      fail('DEVICE_TASK_SUPERVISOR_OWNER_UNCONFIRMED')
    }
    durableWrite(join(takeoverGuard, 'owner.json'), owner)
    try { mkdirSync(lock, { mode: 0o700 }) } catch (error) {
      if (error.code !== 'EEXIST') throw error
      let retained
      try { retained = JSON.parse(readFileSync(join(lock, 'owner.json'), 'utf8')) }
      catch { fail('DEVICE_TASK_SUPERVISOR_OWNER_UNCONFIRMED') }
      if (!retained.id || !Number.isInteger(retained.pid) || !retained.startedAt) fail('DEVICE_TASK_SUPERVISOR_OWNER_UNCONFIRMED')
      if (processIdentity(retained.pid) === retained.startedAt) fail('DEVICE_TASK_SUPERVISOR_OWNED')
      renameSync(lock, `${lock}.stale-${retained.id}-${owner.id}`)
      mkdirSync(lock, { mode: 0o700 })
    }
    durableWrite(join(lock, 'owner.json'), owner)
    previous = existsSync(path) ? JSON.parse(readFileSync(path, 'utf8')) : null
    if (previous) assert.deepEqual(previous.identity, identity, 'supervision identity changed')
  } finally {
    if (ownsTakeoverGuard) rmSync(takeoverGuard, { recursive: true })
  }
  let state = { schemaVersion: 1, identity, generation: (previous?.generation ?? 0) + 1, owner,
    phase: previous?.phase ?? 'preparing', workRunId: previous?.workRunId ?? null,
    revision: previous?.revision ?? 0, failures: previous?.failures ?? [],
    lastProgressAt: previous?.lastProgressAt ?? new Date().toISOString(), updatedAt: new Date().toISOString() }
  let closed = false
  let heartbeatFailure = null
  const assertActive = () => {
    if (closed) fail('DEVICE_TASK_SUPERVISOR_CLOSED')
    if (heartbeatFailure) throw heartbeatFailure
    let current
    try { current = JSON.parse(readFileSync(join(lock, 'owner.json'), 'utf8')) }
    catch { fail('DEVICE_TASK_SUPERVISOR_FENCED') }
    if (current.id !== owner.id || current.startedAt !== owner.startedAt) fail('DEVICE_TASK_SUPERVISOR_FENCED')
  }
  const checkpoint = (phase, details = {}) => {
    assertActive()
    if (details.workRunId && state.workRunId && details.workRunId !== state.workRunId) {
      fail('DEVICE_TASK_SUPERVISOR_IDENTITY_MISMATCH')
    }
    for (const key of ['identity', 'owner', 'generation', 'revision']) {
      if (Object.hasOwn(details, key)) fail('DEVICE_TASK_SUPERVISOR_IDENTITY_MISMATCH')
    }
    const changed = phase !== state.phase || Object.entries(details)
      .some(([key, value]) => JSON.stringify(value) !== JSON.stringify(state[key]))
    const now = new Date().toISOString()
    state = { ...state, ...details, phase, revision: state.revision + 1, updatedAt: now,
      lastProgressAt: changed ? now : state.lastProgressAt }
    durableWrite(path, state)
    return state
  }
  checkpoint(state.workRunId ? 'recovering' : 'preparing')
  const heartbeat = setInterval(() => {
    try { checkpoint(state.phase) } catch (error) {
      heartbeatFailure = Object.assign(new Error(codeOf(error)), { code: codeOf(error), unresolvedDeviceExecution: true })
      state = { ...state, phase: 'blocked', blockedReason: codeOf(error) }
      clearInterval(heartbeat)
    }
  }, 5_000)
  heartbeat.unref()
  return {
    assertActive,
    checkpoint,
    snapshot: () => structuredClone(state),
    async drive(action, { terminalError = () => false, retryMillis = 1_000, recoveryLimit = 3 } = {}) {
      for (let attempt = 0; ; attempt++) {
        try { const result = await action(); checkpoint(state.phase); return result } catch (error) {
          const failure = { code: codeOf(error), observedAt: new Date().toISOString(), attempt }
          checkpoint('recovering', { failures: [...state.failures, failure] })
          if (terminalError(error)) throw error
          if (attempt >= recoveryLimit) {
            checkpoint('blocked', { blockedReason: failure.code })
            throw Object.assign(error, { supervision: this.snapshot(), unresolvedDeviceExecution: true })
          }
          await delay(retryMillis)
        }
      }
    },
    close(error = null) {
      if (closed) return
      clearInterval(heartbeat)
      try {
        if (!['completed', 'failed'].includes(state.phase)) checkpoint('blocked', {
          blockedReason: error ? codeOf(error) : 'DEVICE_DRIVER_ENDED_WITHOUT_TERMINAL',
        })
      } catch (failure) {
        state = { ...state, phase: 'blocked', blockedReason: codeOf(failure) }
      } finally { closed = true }
      // A fenced owner cannot write or delete its successor's authority.
      try {
        const current = JSON.parse(readFileSync(join(lock, 'owner.json'), 'utf8'))
        if (current.id === owner.id && current.startedAt === owner.startedAt) rmSync(lock, { recursive: true })
      } catch { /* No provable ownership: leave the lock for explicit recovery. */ }
    },
  }
}
