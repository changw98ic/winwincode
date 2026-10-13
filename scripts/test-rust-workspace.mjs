#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { readdirSync, statSync } from 'node:fs'
import { resolve } from 'node:path'

import { prepareCompactKernelHelper } from './compact-kernel-helper.mjs'

const root = resolve(import.meta.dirname, '..')
const e2eOnly = process.env.WWC_RUST_E2E === '1'
const standardOnly = process.env.WWC_RUST_E2E === '0'
const heavyBins = ['production_vertical', 'worker_lifecycle', 'code_mode_kernel']
const heavyExpression = heavyBins.map(name => `binary(${name})`).join(' | ')

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

run('cargo', ['test', '--all-features', '--locked', '-p', 'winwincode-kernel-helper', '--no-run'])
run('cargo', ['test', '--all-features', '--locked', '-p', 'winwincode-kernel-helper'])
prepareCompactKernelHelper({
  root,
  targetDirectory: resolve(root, process.env.CARGO_TARGET_DIR || 'target'),
  environment: process.env,
})

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
