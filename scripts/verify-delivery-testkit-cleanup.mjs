#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'

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

const result = spawnSync(process.execPath, [
  '--expose-gc',
  'tests/fixtures/delivery-testkit-cleanup-stress.mjs',
], {
  cwd: root,
  env: environment,
  encoding: 'utf8',
  maxBuffer: 8 * 1_024 * 1_024,
  timeout: 60_000,
})

if (result.error !== undefined) throw result.error
if (result.status !== 0 || result.signal !== null) {
  throw new Error([
    'Delivery testkit cleanup stress process failed',
    `status=${String(result.status)}`,
    `signal=${result.signal ?? 'none'}`,
    result.stderr.trim(),
    result.stdout.trim(),
  ].filter(Boolean).join('\n'))
}
if (result.stderr !== '') {
  throw new Error(`Delivery testkit cleanup emitted native diagnostics\n${result.stderr}`)
}

const lines = result.stdout.split('\n').filter(line => line.trim().length > 0)
if (lines.length !== 1) throw new Error('Delivery testkit cleanup returned unexpected output')
const report = JSON.parse(lines[0])
if (report.iterations !== iterations || report.status !== 'clean') {
  throw new Error('Delivery testkit cleanup returned an invalid report')
}

process.stdout.write(`Delivery testkit cleanup passed ${String(iterations)} iterations\n`)
