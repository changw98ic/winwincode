#!/usr/bin/env node

import { existsSync, readFileSync, statSync } from 'node:fs'
import { resolve } from 'node:path'
import { spawnSync } from 'node:child_process'

import { LOCAL_PACKAGE_NAME, PRODUCT_TARGETS } from './product-build-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const arguments_ = process.argv.slice(2)
let scope = 'all'
if (arguments_.length > 0) {
  if (arguments_.length !== 2 || arguments_[0] !== '--scope') {
    throw new Error('usage: verify-products.mjs [--scope typescript|rust]')
  }
  scope = arguments_[1]
  if (scope !== 'typescript' && scope !== 'rust') {
    throw new Error('product verification scope must be typescript or rust')
  }
}
const targetRoot = process.env.CARGO_TARGET_DIR === undefined
  ? resolve(root, 'target')
  : resolve(root, process.env.CARGO_TARGET_DIR)
const profile = process.env.WWC_PRODUCT_PROFILE === 'release' ? 'release' : 'debug'

function runMetadata() {
  const result = spawnSync('cargo', [
    'metadata',
    '--locked',
    '--no-deps',
    '--format-version',
    '1',
  ], {
    cwd: root,
    encoding: 'utf8',
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) {
    throw new Error(`locked Cargo metadata failed\n${result.stdout}${result.stderr}`)
  }
  return JSON.parse(result.stdout)
}

if (scope === 'all' || scope === 'rust') {
  const metadata = runMetadata()
  const packages = new Map(metadata.packages.map(package_ => [package_.name, package_]))
  for (const product of PRODUCT_TARGETS) {
    const package_ = packages.get(product.packageName)
    if (package_ === undefined) throw new Error(`missing product package ${product.packageName}`)
    if (!package_.targets.some(target => target.name === product.binaryName && target.kind.includes('bin'))) {
      throw new Error(`product package ${product.packageName} has no ${product.binaryName} binary target`)
    }
    const path = resolve(targetRoot, profile, product.binaryName)
    if (!existsSync(path) || !statSync(path).isFile()) throw new Error(`missing product binary ${path}`)
  }
  const local = packages.get(LOCAL_PACKAGE_NAME)
  if (local === undefined) throw new Error(`missing product package ${LOCAL_PACKAGE_NAME}`)
  if (!local.targets.some(target => target.kind.includes('lib'))) {
    throw new Error(`${LOCAL_PACKAGE_NAME} must expose the local composition library`)
  }
}

if (scope === 'all' || scope === 'typescript') {
  const clientManifest = JSON.parse(readFileSync(resolve(root, 'apps/client/package.json'), 'utf8'))
  if (clientManifest.name !== '@winwincode/client') throw new Error('client package identity is invalid')
  for (const [directory, packageName] of [
    ['packages/contracts', '@winwincode/contracts'],
    ['packages/strongflow', '@winwincode/strongflow'],
  ]) {
    const manifest = JSON.parse(readFileSync(resolve(root, directory, 'package.json'), 'utf8'))
    if (manifest.name !== packageName) throw new Error(`${directory} package identity is invalid`)
    for (const path of ['dist/index.js', 'dist/index.d.ts']) {
      if (!existsSync(resolve(root, directory, path))) {
        throw new Error(`missing ${packageName} artifact ${path}`)
      }
    }
  }
  const clientDist = resolve(root, 'apps/client/dist')
  for (const path of [
    'module/index.js',
    'module/index.d.ts',
    'public/index.html',
    'public/runtime-config.js',
    'public/version.json',
    'public/asset-manifest.json',
    'public/assets/client.js',
    'public/assets/client.css',
  ]) {
    if (!existsSync(resolve(clientDist, path))) throw new Error(`missing Client artifact ${path}`)
  }
}

const summary = scope === 'all'
  ? `Client and ${String(PRODUCT_TARGETS.length)} Rust binaries plus ${LOCAL_PACKAGE_NAME}`
  : scope === 'typescript'
    ? 'Client and TypeScript packages'
    : `${String(PRODUCT_TARGETS.length)} Rust binaries plus ${LOCAL_PACKAGE_NAME}`
process.stdout.write(`verified ${summary}\n`)
