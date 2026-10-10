#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { validateRuntime } from './check-runtime.mjs'

// Data digest from the Cargo.lock-verified deno_core_icudata 0.77.0 archive.
const ICU_DATA_SHA256 = '1cf67874b5a87a8363a86fb3f81e3cbbed54d389062dab8fb52308d5cf8c8612'

export function runNinja(arguments_, expectedDataSha256 = ICU_DATA_SHA256) {
  const runtimeError = validateRuntime({
    nodeVersion: process.versions.node,
    platform: process.platform,
    architecture: process.arch,
  })
  if (runtimeError !== undefined) throw new Error(runtimeError)

  const directoryIndex = arguments_.indexOf('-C')
  if (directoryIndex < 0 || arguments_[directoryIndex + 1] === undefined) {
    throw new Error('Code Mode Ninja requires the V8 build directory')
  }
  const output = resolve(arguments_[directoryIndex + 1])
  const dataPackage = resolve(process.cwd(), '../deno_core_icudata-0.77.0')
  const data = readFileSync(join(dataPackage, 'src/icudtl.dat'))
  if (createHash('sha256').update(data).digest('hex') !== expectedDataSha256) {
    throw new Error('Code Mode ICU data differs from its pinned Cargo source')
  }
  const dataFile = join(output, 'icudtl.dat')
  const stagingFile = `${dataFile}.${process.pid}.tmp`
  writeFileSync(stagingFile, data, { mode: 0o600 })
  renameSync(stagingFile, dataFile)

  const downloadedNinja = join(dirname(output), 'ninja_gn_binaries/ninja/ninja')
  const result = spawnSync(existsSync(downloadedNinja) ? downloadedNinja : 'ninja', arguments_, {
    stdio: 'inherit',
  })
  if (result.error !== undefined) throw result.error
  if (result.signal !== null) throw new Error(`Code Mode Ninja ended with ${result.signal}`)
  return result.status ?? 1
}

if (process.argv[1] !== undefined && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  process.exitCode = runNinja(process.argv.slice(2))
}
