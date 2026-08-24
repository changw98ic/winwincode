#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { createRequire } from 'node:module'
import { dirname, resolve } from 'node:path'

import {
  nativePackageName,
  resolveReleaseTarget,
} from '../packages/native/dist/index.js'

const root = resolve(import.meta.dirname, '..')
const credentialNamePattern = /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu
const requestedIterations = process.env.WINWINCODE_CLEANUP_STRESS_ITERATIONS ?? '4'
const iterations = Number(requestedIterations)

if (!Number.isSafeInteger(iterations) || iterations < 1 || iterations > 32) {
  throw new TypeError('WINWINCODE_CLEANUP_STRESS_ITERATIONS must be an integer from 1 to 32')
}

const environment = Object.fromEntries(Object.entries(process.env).filter(([name]) => (
  !credentialNamePattern.test(name)
)))
environment.WINWINCODE_CLEANUP_STRESS_ITERATIONS = String(iterations)

function runCleanupFixture({ arguments_, expectedReport, label, timeout }) {
  const result = spawnSync(process.execPath, arguments_, {
    cwd: root,
    env: environment,
    encoding: 'utf8',
    maxBuffer: 8 * 1_024 * 1_024,
    timeout,
  })

  if (result.error !== undefined) throw result.error
  if (result.status !== 0 || result.signal !== null) {
    throw new Error([
      `${label} failed`,
      `status=${String(result.status)}`,
      `signal=${result.signal ?? 'none'}`,
      result.stderr.trim(),
      result.stdout.trim(),
    ].filter(Boolean).join('\n'))
  }
  if (result.stderr !== '') {
    throw new Error(`${label} emitted native diagnostics\n${result.stderr}`)
  }

  const lines = result.stdout.split('\n').filter(line => line.trim().length > 0)
  if (lines.length !== 1) throw new Error(`${label} returned unexpected output`)
  const report = JSON.parse(lines[0])
  if (JSON.stringify(report) !== JSON.stringify(expectedReport)) {
    throw new Error(`${label} returned an invalid report`)
  }
}

runCleanupFixture({
  arguments_: [
    '--expose-gc',
    'tests/fixtures/delivery-testkit-cleanup-stress.mjs',
  ],
  expectedReport: { iterations, status: 'clean' },
  label: 'Delivery testkit cleanup stress process',
  timeout: 60_000,
})

const require = createRequire(resolve(root, 'packages/native/dist/index.js'))
const target = resolveReleaseTarget()
const prebuildRoot = dirname(require.resolve(`${nativePackageName(target)}/build-info.json`))
runCleanupFixture({
  arguments_: [
    '--expose-gc',
    'tests/fixtures/native-close-session.mjs',
    resolve(prebuildRoot, 'winwincode_native.node'),
    resolve(prebuildRoot, 'winwincode-kernel-helper'),
    String(iterations),
  ],
  expectedReport: { iterations, status: 'clean' },
  label: 'Native kernel cleanup stress process',
  timeout: 60_000,
})

process.stdout.write(
  `Runtime cleanup passed ${String(iterations)} iterations for Delivery testkit and native kernel\n`,
)
