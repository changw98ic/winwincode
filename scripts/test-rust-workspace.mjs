#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'

import { prepareCompactKernelHelper } from './compact-kernel-helper.mjs'

const root = resolve(import.meta.dirname, '..')
const args = ['test', '--all-features', '--locked']
function run(extra) {
  const result = spawnSync('cargo', [...args, ...extra], { cwd: root, env: process.env, stdio: 'inherit' })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) process.exit(result.status ?? 1)
}

// Cargo restores its debug binary when running the helper's integration tests.
// Finish those tests before preparing the authenticated product fixture.
run(['--workspace', '--no-run'])
run(['-p', 'winwincode-kernel-helper'])
prepareCompactKernelHelper({
  root,
  targetDirectory: resolve(root, process.env.CARGO_TARGET_DIR || 'target'),
  environment: process.env,
})
run(['--workspace', '--exclude', 'winwincode-kernel-helper'])
