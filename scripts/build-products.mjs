#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { existsSync, statSync } from 'node:fs'
import { resolve } from 'node:path'

import { LOCAL_PACKAGE_NAME, PRODUCT_TARGETS } from './product-build-contract.mjs'

const root = resolve(import.meta.dirname, '..')

function parseArguments(argv) {
  let release = false
  let target
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--release') {
      release = true
      continue
    }
    if (argument === '--target') {
      target = argv[index + 1]
      if (target === undefined || target.startsWith('--')) {
        throw new Error('--target requires a Rust target triple')
      }
      index += 1
      continue
    }
    if (argument.startsWith('--target=')) {
      target = argument.slice('--target='.length)
      if (target.length === 0) throw new Error('--target requires a Rust target triple')
      continue
    }
    throw new Error(`unknown product build argument: ${argument}`)
  }
  return { release, target }
}

function targetDirectory(target) {
  const configured = process.env.CARGO_TARGET_DIR
  const base = configured === undefined || configured.length === 0
    ? resolve(root, 'target')
    : resolve(root, configured)
  return target === undefined ? base : resolve(base, target)
}

const { release, target } = parseArguments(process.argv.slice(2))
const cargoArguments = ['build', '--locked']
for (const product of PRODUCT_TARGETS) {
  cargoArguments.push('-p', product.packageName, '--bin', product.binaryName)
}
cargoArguments.push('-p', LOCAL_PACKAGE_NAME)
if (release) cargoArguments.push('--release')
if (target !== undefined) cargoArguments.push('--target', target)

const result = spawnSync('cargo', cargoArguments, {
  cwd: root,
  env: process.env,
  stdio: 'inherit',
})
if (result.error !== undefined) throw result.error
if (result.status !== 0 || result.signal !== null) {
  throw new Error(`product build failed with ${result.signal ?? `exit code ${result.status}`}`)
}

const profile = release ? 'release' : 'debug'
const output = targetDirectory(target)
const artifacts = PRODUCT_TARGETS.map(product => {
  const path = resolve(output, profile, product.binaryName)
  if (!existsSync(path) || !statSync(path).isFile()) {
    throw new Error(`product build did not produce ${path}`)
  }
  return `${product.binaryName}:${path}`
})
process.stdout.write(
  `built ${profile} products (${artifacts.join(', ')}) and ${LOCAL_PACKAGE_NAME}\n`,
)
