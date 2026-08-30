#!/usr/bin/env node

import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import {
  cpSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { join, relative, resolve } from 'node:path'

const packageRoot = resolve(import.meta.dirname, '..')
const repositoryRoot = resolve(packageRoot, '../..')
const outputRoot = join(packageRoot, 'dist')
const publicOutputRoot = join(outputRoot, 'public')
const manifest = JSON.parse(readFileSync(join(packageRoot, 'package.json'), 'utf8'))
const releaseTargets = Object.freeze([
  'aarch64-apple-darwin',
  'x86_64-apple-darwin',
  'aarch64-unknown-linux-gnu',
  'x86_64-unknown-linux-gnu',
])

function run(command, arguments_, cwd) {
  const result = spawnSync(command, arguments_, { cwd, encoding: 'utf8', stdio: 'inherit' })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) process.exit(result.status ?? 1)
}

function filesBelow(directory) {
  const files = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) files.push(...filesBelow(path))
    else if (entry.isFile()) files.push(path)
  }
  return files
}

function descriptor(path) {
  const bytes = readFileSync(join(publicOutputRoot, path))
  return Object.freeze({
    path,
    bytes: bytes.length,
    sha256: createHash('sha256').update(bytes).digest('hex'),
  })
}

rmSync(outputRoot, { recursive: true, force: true })
mkdirSync(publicOutputRoot, { recursive: true })
run('corepack', [
  'pnpm',
  'exec',
  'tsc',
  '-b',
  'apps/client/tsconfig.json',
  '--force',
  '--pretty',
  'false',
], repositoryRoot)
run('corepack', [
  'pnpm',
  'exec',
  'esbuild',
  'src/boot.ts',
  '--bundle',
  '--splitting',
  '--format=esm',
  '--platform=browser',
  '--target=es2023',
  '--minify',
  '--entry-names=client',
  '--chunk-names=chunk-[hash]',
  '--outdir=dist/public/assets',
  '--banner:js=// SPDX-License-Identifier: Apache-2.0',
], packageRoot)
run('corepack', [
  'pnpm',
  'exec',
  'esbuild',
  'src/enterprise-application.ts',
  'src/enterprise-resource-page.ts',
  'src/enterprise-operations-page.ts',
  '--bundle',
  '--splitting',
  '--format=esm',
  '--platform=browser',
  '--target=es2023',
  '--minify',
  '--entry-names=[name]-[hash]',
  '--chunk-names=chunk-[hash]',
  '--outdir=dist/public/assets',
  '--banner:js=// SPDX-License-Identifier: Apache-2.0',
], packageRoot)
cpSync(join(packageRoot, 'public'), publicOutputRoot, { recursive: true, force: true })

const version = Object.freeze({
  schemaVersion: 1,
  product: 'WinWinCode',
  package: manifest.name,
  version: manifest.version,
  controlPlaneSchemaVersion: 'winwincode/v1',
})
writeFileSync(join(publicOutputRoot, 'version.json'), `${JSON.stringify(version, null, 2)}\n`)

const assets = filesBelow(publicOutputRoot)
  .map(path => relative(publicOutputRoot, path).replaceAll('\\', '/'))
  .filter(path => !['asset-manifest.json', 'runtime-config.js'].includes(path))
  .sort((left, right) => left.localeCompare(right))
  .map(descriptor)
const targets = Object.fromEntries(releaseTargets.map(target => [target, assets]))
const assetManifest = {
  schemaVersion: 1,
  package: manifest.name,
  version: manifest.version,
  entry: 'index.html',
  runtimeConfig: {
    path: 'runtime-config.js',
    field: 'serverUrl',
    mutableAtDeployment: true,
  },
  assets,
  targets,
}
writeFileSync(
  join(publicOutputRoot, 'asset-manifest.json'),
  `${JSON.stringify(assetManifest, null, 2)}\n`,
)

const totalBytes = filesBelow(outputRoot)
  .map(path => statSync(path).size)
  .reduce((sum, bytes) => sum + bytes, 0)
process.stdout.write(
  `built ${manifest.name}@${manifest.version}: ${String(assets.length)} browser assets, ${String(totalBytes)} bytes\n`,
)
