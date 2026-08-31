#!/usr/bin/env node

import { spawn } from 'node:child_process'
import { cpSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, join, relative, resolve, sep } from 'node:path'
import { pathToFileURL } from 'node:url'

import { reclaimReleaseCargoTarget } from './release-cargo-target-reclaim.mjs'

const root = resolve(import.meta.dirname, '..')
export const DEFAULT_COMMAND_TIMEOUT_MILLIS = 600_000
// A clean Linux runner needs about 15.5 minutes to compile and run the full
// workspace test lane. Keep that one command bounded at 20 minutes while the
// other clean-install commands retain their existing 10-minute limit.
export const COLD_TEST_TIMEOUT_MILLIS = 1_200_000
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

function command(args, timeoutMillis = DEFAULT_COMMAND_TIMEOUT_MILLIS) {
  return Object.freeze({
    args: Object.freeze(args),
    timeoutMillis,
  })
}

export const CLEAN_INSTALL_COMMANDS = Object.freeze([
  command(['install', '--frozen-lockfile', '--prefer-offline']),
  command(['format:check']),
  command(['lint']),
  command(['build']),
  command(['test'], COLD_TEST_TIMEOUT_MILLIS),
  command(['verify:products']),
  command(['verify:phase-6.6']),
])

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

export function runCleanCheckoutCommand({
  args,
  cwd,
  cargoTarget,
  timeoutMillis,
  environment = process.env,
  executable = 'corepack',
  argumentPrefix = ['pnpm'],
  terminationGraceMillis = TERMINATION_GRACE_MILLIS,
}) {
  if (!Array.isArray(args) || !Array.isArray(argumentPrefix)
    || !Number.isSafeInteger(timeoutMillis) || timeoutMillis <= 0
    || !Number.isSafeInteger(terminationGraceMillis) || terminationGraceMillis <= 0) {
    throw new Error('clean checkout command requires a positive bounded timeout')
  }
  return new Promise(resolvePromise => {
    const child = spawn(executable, [...argumentPrefix, ...args], {
      cwd,
      detached: true,
      env: {
        ...environment,
        CARGO_TARGET_DIR: cargoTarget,
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
      killTimer = setTimeout(
        () => terminateProcessGroup(child, 'SIGKILL'),
        terminationGraceMillis,
      )
      killTimer.unref()
    }, timeoutMillis)
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

async function withCleanCheckoutCleanup(temporaryRoot, operation) {
  try {
    return await operation()
  } finally {
    rmSync(temporaryRoot, { force: true, recursive: true })
  }
}

export async function verifyCleanInstall({
  sourceRoot = root,
  environment = process.env,
  temporaryBase = tmpdir(),
  commandRunner = runCleanCheckoutCommand,
} = {}) {
  const temporaryRoot = mkdtempSync(join(temporaryBase, 'winwincode-clean-'))
  const temporaryCargoTarget = join(temporaryRoot, 'target')
  return withCleanCheckoutCleanup(temporaryRoot, async () => {
    const reclaim = reclaimReleaseCargoTarget({ environment, sourceRoot })
    if (reclaim.reclaimed) {
      process.stdout.write(
        `release Cargo target reclaimed before clean checkout: ${reclaim.path}; `
        + `available bytes ${reclaim.availableBytesBefore} -> ${reclaim.availableBytesAfter} `
        + `(delta ${reclaim.availableBytesDelta})\n`,
      )
    }
    cpSync(sourceRoot, temporaryRoot, {
      recursive: true,
      filter(source) {
        const name = basename(source)
        if (source !== sourceRoot && excludedNames.has(name)) return false
        const path = relative(sourceRoot, source)
        return path === '' || !path.split(sep).some(segment => excludedNames.has(segment))
      },
    })

    for (const { args, timeoutMillis } of CLEAN_INSTALL_COMMANDS) {
      const result = await commandRunner({
        args,
        cwd: temporaryRoot,
        cargoTarget: temporaryCargoTarget,
        timeoutMillis,
        environment,
      })
      if (result.status !== 0 || result.error !== undefined) {
        process.stderr.write(`clean checkout command failed: corepack pnpm ${args.join(' ')}\n`)
        if (result.timedOut) {
          process.stderr.write(
            `command exceeded ${timeoutMillis} ms; `
            + 'its complete process group was stopped before cleanup\n',
          )
        }
        if (result.error !== undefined) process.stderr.write(`${result.error.stack}\n`)
        process.stderr.write(result.stdout)
        process.stderr.write(result.stderr)
        return false
      }
      process.stdout.write(`clean checkout passed: pnpm ${args.join(' ')}\n`)
    }
    return true
  })
}

const isMain = process.argv[1] !== undefined
  && pathToFileURL(resolve(process.argv[1])).href === import.meta.url
if (isMain && !(await verifyCleanInstall())) process.exitCode = 1
