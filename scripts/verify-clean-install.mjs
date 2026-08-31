#!/usr/bin/env node

import { spawn } from 'node:child_process'
import { cpSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, join, relative, resolve, sep } from 'node:path'

import { reclaimReleaseCargoTarget } from './release-cargo-target-reclaim.mjs'

const root = resolve(import.meta.dirname, '..')
const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-clean-'))
const temporaryCargoTarget = join(temporaryRoot, 'target')
const COMMAND_TIMEOUT_MILLIS = 600_000
const TERMINATION_GRACE_MILLIS = 5_000
const excludedNames = new Set([
  '.agents',
  '.beads',
  '.cache',
  '.claude',
  '.codex',
  '.git',
  'dist',
  'node_modules',
  'target',
])
const commands = [
  ['install', '--frozen-lockfile', '--prefer-offline'],
  ['format:check'],
  ['lint'],
  ['build'],
  ['test'],
  ['verify:products'],
  ['verify:phase-6.6'],
]

function terminateProcessGroup(child, signal) {
  if (child.pid === undefined) return
  try {
    process.kill(-child.pid, signal)
  } catch (error) {
    if (error?.code !== 'ESRCH') {
      try {
        child.kill(signal)
      } catch {
        // The command may have settled between the group and direct kill attempts.
      }
    }
  }
}

function runCommand(args) {
  return new Promise(resolvePromise => {
    const child = spawn('corepack', ['pnpm', ...args], {
      cwd: temporaryRoot,
      detached: true,
      env: {
        ...process.env,
        CARGO_TARGET_DIR: temporaryCargoTarget,
        CI: '1',
        COREPACK_ENABLE_DOWNLOAD_PROMPT: '0',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    const stdout = []
    const stderr = []
    let timedOut = false
    let spawnError
    let killTimer
    child.stdout.on('data', chunk => stdout.push(chunk))
    child.stderr.on('data', chunk => stderr.push(chunk))
    child.on('error', error => {
      spawnError = error
    })
    const timeout = setTimeout(() => {
      timedOut = true
      terminateProcessGroup(child, 'SIGTERM')
      killTimer = setTimeout(() => terminateProcessGroup(child, 'SIGKILL'), TERMINATION_GRACE_MILLIS)
      killTimer.unref()
    }, COMMAND_TIMEOUT_MILLIS)
    timeout.unref()
    child.on('close', (status, signal) => {
      clearTimeout(timeout)
      if (killTimer !== undefined) clearTimeout(killTimer)
      resolvePromise({
        status,
        signal,
        timedOut,
        error: spawnError,
        stdout: Buffer.concat(stdout).toString('utf8'),
        stderr: Buffer.concat(stderr).toString('utf8'),
      })
    })
  })
}

try {
  const reclaim = reclaimReleaseCargoTarget({
    environment: process.env,
    sourceRoot: root,
  })
  if (reclaim.reclaimed) {
    process.stdout.write(
      `release Cargo target reclaimed before clean checkout: ${reclaim.path}; `
      + `available bytes ${reclaim.availableBytesBefore} -> ${reclaim.availableBytesAfter} `
      + `(delta ${reclaim.availableBytesDelta})\n`,
    )
  }
  cpSync(root, temporaryRoot, {
    recursive: true,
    filter(source) {
      const name = basename(source)
      if (source !== root && excludedNames.has(name)) return false
      const path = relative(root, source)
      return path === '' || !path.split(sep).some(segment => excludedNames.has(segment))
    },
  })

  for (const args of commands) {
    const result = await runCommand(args)
    if (result.status !== 0 || result.error !== undefined) {
      process.stderr.write(`clean checkout command failed: corepack pnpm ${args.join(' ')}\n`)
      if (result.timedOut) {
        process.stderr.write(
          `command exceeded ${COMMAND_TIMEOUT_MILLIS} ms; its complete process group was stopped before cleanup\n`,
        )
      }
      if (result.error !== undefined) process.stderr.write(`${result.error.stack}\n`)
      process.stderr.write(result.stdout)
      process.stderr.write(result.stderr)
      process.exitCode = 1
      break
    }
    process.stdout.write(`clean checkout passed: pnpm ${args.join(' ')}\n`)
  }
} finally {
  rmSync(temporaryRoot, { force: true, recursive: true })
}
