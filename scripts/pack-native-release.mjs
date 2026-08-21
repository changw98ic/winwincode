#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { mkdirSync, readdirSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

import {
  hostNativeTarget,
  nativeTargetConfiguration,
  sha256,
  verifyNativePrebuild,
} from './native-package-contract.mjs'

function parseArguments(argv) {
  let target
  let output
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--target') {
      target = argv[index + 1]
      if (target === undefined) throw new Error('--target requires a Rust target triple')
      index += 1
      continue
    }
    if (argument === '--output') {
      output = argv[index + 1]
      if (output === undefined) throw new Error('--output requires a directory')
      index += 1
      continue
    }
    if (argument.startsWith('--target=')) {
      target = argument.slice('--target='.length)
      continue
    }
    if (argument.startsWith('--output=')) {
      output = argument.slice('--output='.length)
      continue
    }
    throw new Error(`unknown pack-native-release argument: ${argument}`)
  }
  return { target: target ?? hostNativeTarget(), output }
}

function pack(root, directory, output) {
  const result = spawnSync('corepack', [
    'pnpm',
    'pack',
    '--pack-destination',
    output,
  ], {
    cwd: join(root, directory),
    encoding: 'utf8',
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) {
    throw new Error(`${directory}: pnpm pack failed\n${result.stdout}${result.stderr}`)
  }
}

const root = resolve(import.meta.dirname, '..')
const { target, output } = parseArguments(process.argv.slice(2))
if (target === undefined) throw new Error(`unsupported host ${process.platform}/${process.arch}`)
if (output === undefined) throw new Error('--output is required')
const configuration = nativeTargetConfiguration(target)
if (configuration === undefined) throw new Error(`unsupported native target ${target}`)
const verification = verifyNativePrebuild({
  root,
  target,
  requireRelease: true,
  requireCurrentHost: true,
})
if (verification.errors.length > 0) throw new Error(verification.errors.join('\n'))

const outputRoot = resolve(output)
mkdirSync(outputRoot, { recursive: true })
for (const directory of [
  'packages/contracts',
  'packages/native',
  configuration.packageDirectory,
]) {
  pack(root, directory, outputRoot)
}
const tarballs = readdirSync(outputRoot).filter(name => name.endsWith('.tgz')).sort()
if (tarballs.length !== 3) throw new Error(`expected 3 native release tarballs, found ${tarballs.length}`)
const checksums = tarballs.map(name => `${sha256(join(outputRoot, name))}  ${name}`)
writeFileSync(join(outputRoot, 'SHA256SUMS'), `${checksums.join('\n')}\n`)
process.stdout.write(`packed ${target} release with SHA256SUMS in ${outputRoot}\n`)
