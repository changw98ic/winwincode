#!/usr/bin/env node

import { spawn } from 'node:child_process'
import { mkdir, mkdtemp, readdir, rm } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import { tmpdir } from 'node:os'

const root = resolve(import.meta.dirname, '..')
const credentialNamePattern = /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu
const requestedIterations = process.env.WINWINCODE_CLEANUP_STRESS_ITERATIONS ?? '4'
const iterations = Number(requestedIterations)
const COMMAND_TIMEOUT_MILLIS = 180_000
const TERMINATION_GRACE_MILLIS = 5_000
const expectedOutput = 'Rust differential runner matched all 10 canonical scenarios\n'

if (!Number.isSafeInteger(iterations) || iterations < 1 || iterations > 32) {
  throw new TypeError('WINWINCODE_CLEANUP_STRESS_ITERATIONS must be an integer from 1 to 32')
}

function terminateProcessGroup(child, signal) {
  if (child.pid === undefined) return
  try {
    process.kill(-child.pid, signal)
  } catch (error) {
    if (error?.code !== 'ESRCH') {
      try {
        child.kill(signal)
      } catch {
        // The process may have ended between the group and direct kill attempts.
      }
    }
  }
}

function runDifferential(environment) {
  return new Promise(resolvePromise => {
    const child = spawn(
      process.execPath,
      ['scripts/run-delivery-strongflow-rust-differential.mjs', '--check'],
      {
        cwd: root,
        detached: true,
        env: environment,
        stdio: ['ignore', 'pipe', 'pipe'],
      },
    )
    const stdout = []
    const stderr = []
    let spawnError
    let timedOut = false
    let killTimer
    child.stdout.on('data', chunk => stdout.push(chunk))
    child.stderr.on('data', chunk => stderr.push(chunk))
    child.on('error', error => {
      spawnError = error
    })
    const timeout = setTimeout(() => {
      timedOut = true
      terminateProcessGroup(child, 'SIGTERM')
      killTimer = setTimeout(
        () => terminateProcessGroup(child, 'SIGKILL'),
        TERMINATION_GRACE_MILLIS,
      )
      killTimer.unref()
    }, COMMAND_TIMEOUT_MILLIS)
    timeout.unref()
    child.on('close', (status, signal) => {
      clearTimeout(timeout)
      if (killTimer !== undefined) clearTimeout(killTimer)
      resolvePromise({
        error: spawnError,
        signal,
        status,
        stderr: Buffer.concat(stderr).toString('utf8'),
        stdout: Buffer.concat(stdout).toString('utf8'),
        timedOut,
      })
    })
  })
}

const environment = Object.fromEntries(Object.entries(process.env).filter(([name]) => (
  !credentialNamePattern.test(name)
)))
delete environment.WINWINCODE_DELIVERY_DIFFERENTIAL_INPUT
delete environment.WINWINCODE_DELIVERY_DIFFERENTIAL_OUTPUT
environment.CI = '1'
environment.WINWINCODE_CLEANUP_STRESS_ITERATIONS = String(iterations)

const stressRoot = await mkdtemp(join(tmpdir(), 'winwincode-rust-delivery-cleanup-'))
try {
  for (let iteration = 0; iteration < iterations; iteration += 1) {
    const isolatedTemp = join(stressRoot, `iteration-${String(iteration)}`)
    await mkdir(isolatedTemp)
    const result = await runDifferential({
      ...environment,
      TEMP: isolatedTemp,
      TMP: isolatedTemp,
      TMPDIR: isolatedTemp,
    })
    if (result.error !== undefined
      || result.status !== 0
      || result.signal !== null
      || result.timedOut) {
      throw new Error([
        `Rust Delivery cleanup iteration ${String(iteration)} failed`,
        `status=${String(result.status)}`,
        `signal=${result.signal ?? 'none'}`,
        `timedOut=${String(result.timedOut)}`,
        result.error?.stack ?? '',
        result.stderr.trim(),
        result.stdout.trim(),
      ].filter(Boolean).join('\n'))
    }
    if (result.stderr !== '') {
      throw new Error(`Rust Delivery cleanup iteration ${String(iteration)} emitted diagnostics\n${result.stderr}`)
    }
    if (result.stdout !== expectedOutput) {
      throw new Error(`Rust Delivery cleanup iteration ${String(iteration)} returned unexpected output`)
    }
    const remaining = await readdir(isolatedTemp)
    if (remaining.length !== 0) {
      throw new Error(
        `Rust Delivery cleanup iteration ${String(iteration)} left temporary resources: ${remaining.join(', ')}`,
      )
    }
  }
} finally {
  await rm(stressRoot, { recursive: true, force: true })
}

process.stdout.write(`${JSON.stringify({ iterations, status: 'clean' })}\n`)
