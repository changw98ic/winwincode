// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync, spawn } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { setTimeout as delay } from 'node:timers/promises'
import test from 'node:test'

import { stopProcessGroup, waitFor } from '../scripts/lib/device-production-fixture.mjs'

function processRunning(pid) {
  try {
    const state = execFileSync('ps', ['-p', String(pid), '-o', 'stat='], { encoding: 'utf8' }).trim()
    return state.length > 0 && !state.startsWith('Z')
  } catch (error) {
    if (error.status === 1) return false
    throw error
  }
}

async function fixtureTree({ ignoresTerminate = false, nested = false } = {}) {
  const leafScript = `${ignoresTerminate ? "process.on('SIGTERM', () => {})" : ''}
    console.log(process.pid); setInterval(() => {}, 1000)`
  const workerScript = nested
    ? `const { spawn } = require('node:child_process');
      const leaf = spawn(process.execPath, ['-e', ${JSON.stringify(leafScript)}],
        { detached: true, stdio: ['ignore', 'pipe', 'ignore'] });
      leaf.stdout.once('data', bytes => console.log(JSON.stringify([process.pid, Number(bytes)])));
      setInterval(() => {}, 1000)`
    : leafScript
  const parentScript = `const { spawn } = require('node:child_process');
    const worker = spawn(process.execPath, ['-e', ${JSON.stringify(workerScript)}],
      { detached: true, stdio: ['ignore', 'pipe', 'ignore'] });
    worker.stdout.once('data', bytes => console.log(bytes.toString().trim()));
    setInterval(() => {}, 1000)`
  const parent = spawn(process.execPath, ['-e', parentScript], {
    detached: true, stdio: ['ignore', 'pipe', 'pipe'],
  })
  let output = ''
  parent.stdout.on('data', bytes => { output += bytes.toString() })
  try {
    const line = await waitFor(() => output.includes('\n') && output.trim(), 'detached Worker ready', 5000, 10)
    return { parent, pids: [parent.pid, ...[JSON.parse(line)].flat()] }
  } catch (error) {
    process.kill(-parent.pid, 'SIGKILL')
    throw error
  }
}

async function cleanup(pids) {
  for (const pid of pids) {
    if (!processRunning(pid)) continue
    try { process.kill(-pid, 'SIGKILL') } catch (error) {
      if (error.code !== 'ESRCH') throw error
    }
  }
  await delay(50)
}

function bootIdentity(pid) {
  if (process.platform === 'linux') {
    const stat = readFileSync(`/proc/${pid}/stat`, 'utf8')
    return `linux-${stat.slice(stat.lastIndexOf(')') + 1).trim().split(/\s+/u)[19]}`
  }
  return `darwin-ps-${execFileSync('ps', ['-p', String(pid), '-o', 'lstart='], {
    encoding: 'utf8', env: { ...process.env, LC_ALL: 'C' },
  }).trim().replace(/\s+/gu, ' ')}`
}

function fixtureRegistry(directory, pid, identity) {
  const database = new DatabaseSync(join(directory, 'device-client.sqlite3'))
  try {
    database.exec(`CREATE TABLE worker_process_registry (
      pid INTEGER NOT NULL, process_start_identity TEXT NOT NULL, state TEXT NOT NULL)`)
    database.prepare('INSERT INTO worker_process_registry VALUES (?, ?, ?)').run(pid, identity, 'running')
  } finally { database.close() }
}

test('fixture shutdown stops a Worker in its own process group and preserves unrelated processes', async () => {
  const { parent, pids } = await fixtureTree()
  const unrelated = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {
    detached: true, stdio: 'ignore',
  })
  try {
    await stopProcessGroup(parent, { graceMillis: 100 })
    await waitFor(() => pids.every(pid => !processRunning(pid)), 'owned processes stopped', 1000, 20)
    assert.equal(processRunning(unrelated.pid), true)
    assert.notEqual(parent.exitCode ?? parent.signalCode, null, 'parent must be reaped')
  } finally {
    await cleanup([...pids, unrelated.pid])
  }
})

test('fixture shutdown escalates nested detached children after their parent exits', async () => {
  const { parent, pids } = await fixtureTree({ ignoresTerminate: true, nested: true })
  try {
    await stopProcessGroup(parent, { graceMillis: 100 })
    await waitFor(() => pids.every(pid => !processRunning(pid)), 'nested owned processes stopped', 1000, 20)
    assert.notEqual(parent.exitCode ?? parent.signalCode, null, 'parent must be reaped')
    await stopProcessGroup(parent, { graceMillis: 100 })
  } finally {
    await cleanup(pids)
  }
})

test('fixture shutdown uses durable Worker boot identity after the Device has already exited', async () => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-fixture-stop-'))
  const { parent, pids } = await fixtureTree()
  try {
    fixtureRegistry(directory, pids[1], bootIdentity(pids[1]))
    process.kill(-parent.pid, 'SIGTERM')
    await waitFor(() => parent.signalCode !== null, 'Device reaped', 1000, 10)
    assert.equal(processRunning(pids[1]), true)
    await stopProcessGroup(parent, { graceMillis: 100, deviceData: directory })
    assert.equal(processRunning(pids[1]), false)
  } finally {
    await cleanup(pids)
    rmSync(directory, { recursive: true, force: true })
  }
})

test('fixture shutdown refuses a registry PID whose boot identity has changed', async () => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-fixture-stop-'))
  const { parent, pids } = await fixtureTree()
  try {
    fixtureRegistry(directory, pids[1], `${bootIdentity(pids[1])}-different-boot`)
    process.kill(-parent.pid, 'SIGTERM')
    await waitFor(() => parent.signalCode !== null, 'Device reaped', 1000, 10)
    await stopProcessGroup(parent, { graceMillis: 100, deviceData: directory })
    assert.equal(processRunning(pids[1]), true)
  } finally {
    await cleanup(pids)
    rmSync(directory, { recursive: true, force: true })
  }
})
