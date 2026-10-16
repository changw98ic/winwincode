#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { readdirSync, statSync } from 'node:fs'
import { resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
if (!process.env.CARGO_BUILD_BUILD_DIR && process.env.WWC_RUST_CACHE !== 'off') {
  const result = spawnSync('python3', [resolve(root, 'scripts/rust-build-cache.py'),
    '--variant', 'all-features', '--', process.execPath, ...process.argv.slice(1)],
  { cwd: root, env: process.env, stdio: 'inherit' })
  if (result.error) throw result.error
  process.exit(result.status ?? 1)
}
const e2eOnly = process.env.WWC_RUST_E2E === '1'
const standardOnly = process.env.WWC_RUST_E2E === '0'
const heavyBins = ['production_vertical', 'worker_lifecycle', 'code_mode_kernel']
const heavyExpression = heavyBins.map(name => `binary(${name})`).join(' | ')
process.env.CARGO_TARGET_DIR = resolve(root, process.env.CARGO_TARGET_DIR || 'target')
process.env.WWC_WORKER_HELPER_EXECUTABLE ??= resolve(process.env.CARGO_TARGET_DIR, 'debug/winwincode-kernel-helper')
process.env.WINWINCODE_CODE_MODE_TEST_HELPER ??= process.env.WWC_WORKER_HELPER_EXECUTABLE

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, env: process.env, stdio: 'inherit' })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) process.exit(result.status ?? 1)
}

function hasNextest() {
  const result = spawnSync('cargo', ['nextest', '--version'], {
    cwd: root,
    env: process.env,
    stdio: 'ignore',
  })
  return result.status === 0
}

function integrationTests(dir) {
  return readdirSync(dir).flatMap(name => {
    const path = resolve(dir, name)
    if (!statSync(path).isFile() || !name.endsWith('.rs')) return []
    return [name.slice(0, -3)]
  })
}

if (process.argv.includes('--build-helper')) {
  run('cargo', ['build', '--locked', '-p', 'winwincode-kernel-helper', '--bin', 'winwincode-kernel-helper'])
}
run('cargo', ['test', '--all-features', '--locked', '-p', 'winwincode-kernel-helper'])

if (hasNextest()) {
  const filter = e2eOnly ? heavyExpression : standardOnly ? `not (${heavyExpression})` : null
  run('cargo', [
    'nextest',
    'run',
    '--all-features',
    '--locked',
    '--workspace',
    '--exclude',
    'winwincode-kernel-helper',
    ...(filter === null ? [] : ['-E', filter]),
  ])
  process.exit(0)
}

if (e2eOnly) {
  run('cargo', ['test', '--all-features', '--locked', '-p', 'winwincode-worker', '--test', 'production_vertical'])
  run('cargo', ['test', '--all-features', '--locked', '-p', 'winwincode-worker', '--test', 'worker_lifecycle'])
  run('cargo', ['test', '--all-features', '--locked', '-p', 'winwincode-kernel', '--test', 'code_mode_kernel'])
  process.exit(0)
}

if (!standardOnly) {
  run('cargo', ['test', '--all-features', '--locked', '--workspace', '--exclude', 'winwincode-kernel-helper'])
  process.exit(0)
}

run('cargo', [
  'test',
  '--all-features',
  '--locked',
  '--workspace',
  '--exclude',
  'winwincode-kernel-helper',
  '--exclude',
  'winwincode-worker',
  '--exclude',
  'winwincode-kernel',
])
run('cargo', ['test', '--all-features', '--locked', '-p', 'winwincode-kernel', '--lib', '--bins'])
run('cargo', ['test', '--all-features', '--locked', '-p', 'winwincode-worker', '--lib', '--bins'])
for (const name of integrationTests(resolve(root, 'crates/winwincode-worker/tests'))) {
  if (heavyBins.includes(name)) continue
  run('cargo', ['test', '--all-features', '--locked', '-p', 'winwincode-worker', '--test', name])
}
