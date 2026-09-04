#!/usr/bin/env node

import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import {
  closeSync,
  cpSync,
  mkdirSync,
  openSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  unlinkSync,
  writeFileSync,
  writeSync,
} from 'node:fs'
import { join, relative, resolve } from 'node:path'

const packageRoot = resolve(import.meta.dirname, '..')
const repositoryRoot = resolve(packageRoot, '../..')
const outputRoot = join(packageRoot, 'dist')
const publicOutputRoot = join(outputRoot, 'public')
// Every browser suite rebuilds this bundle, and the TypeScript lane runs those
// suites concurrently.  A rebuild empties `dist` before writing it again, so
// two overlapping builds used to race inside that tree and fail on `rmSync`.
// The build therefore holds one exclusive lock for its whole write phase.
const buildLockPath = join(packageRoot, '.client-build.lock')
const buildLockStaleMillis = 5 * 60_000
const buildLockWaitMillis = 50
const buildLockDeadlineMillis = 10 * 60_000
let holdsBuildLock = false
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
  if (result.status !== 0) {
    releaseBuildLock()
    process.exit(result.status ?? 1)
  }
}

function waitSync(millis) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, millis)
}

function acquireBuildLock() {
  const deadline = Date.now() + buildLockDeadlineMillis
  for (;;) {
    try {
      const handle = openSync(buildLockPath, 'wx')
      try {
        writeSync(handle, `${process.pid}\n`)
      } finally {
        closeSync(handle)
      }
      holdsBuildLock = true
      return
    } catch (error) {
      if (error.code !== 'EEXIST') throw error
    }
    let stolen = false
    try {
      stolen = Date.now() - statSync(buildLockPath).mtimeMs > buildLockStaleMillis
    } catch {
      stolen = true
    }
    if (stolen) {
      try {
        unlinkSync(buildLockPath)
      } catch {
        // Another waiter removed it first; retry the acquisition.
      }
      continue
    }
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for the client build lock at ${buildLockPath}`)
    }
    waitSync(buildLockWaitMillis)
  }
}

function releaseBuildLock() {
  if (!holdsBuildLock) return
  holdsBuildLock = false
  try {
    unlinkSync(buildLockPath)
  } catch (error) {
    if (error.code !== 'ENOENT') throw error
  }
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

acquireBuildLock()
try {
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
    'src/styles/client.css',
    '--bundle',
    '--minify',
    '--outfile=dist/public/assets/client.css',
    '--banner:css=/* SPDX-License-Identifier: Apache-2.0 */',
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
} finally {
  releaseBuildLock()
}
