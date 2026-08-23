#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import {
  mkdirSync,
  readFileSync,
  readdirSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { join, resolve } from 'node:path'

import {
  hostNativeTarget,
  nativeTargetConfiguration,
  sha256,
  verifyNativePrebuild,
} from './native-package-contract.mjs'
import {
  PRODUCT_COMMON_RELEASE_PACKAGE_DIRECTORIES,
  PRODUCT_RELEASE_SCHEMA_VERSION,
} from './release-source-contract.mjs'

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
  const before = new Set(readdirSync(output).filter(name => name.endsWith('.tgz')))
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
  const added = readdirSync(output)
    .filter(name => name.endsWith('.tgz') && !before.has(name))
  if (added.length !== 1) {
    throw new Error(`${directory}: pnpm pack produced ${String(added.length)} new tarballs`)
  }
  const manifest = JSON.parse(readFileSync(join(root, directory, 'package.json'), 'utf8'))
  const path = join(output, added[0])
  return Object.freeze({
    name: manifest.name,
    version: manifest.version,
    file: added[0],
    sha256: sha256(path),
    bytes: statSync(path).size,
  })
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
if (readdirSync(outputRoot).some(name => (
  name.endsWith('.tgz') || name === 'release-packages.json' || name === 'SHA256SUMS'
))) {
  throw new Error('release artifact output must not contain prior package artifacts')
}
const packages = [
  ...PRODUCT_COMMON_RELEASE_PACKAGE_DIRECTORIES,
  configuration.packageDirectory,
].map(directory => pack(root, directory, outputRoot))
const tarballs = readdirSync(outputRoot).filter(name => name.endsWith('.tgz')).sort()
if (tarballs.length !== packages.length) {
  throw new Error(
    `expected ${String(packages.length)} release tarballs, found ${String(tarballs.length)}`,
  )
}
writeFileSync(join(outputRoot, 'release-packages.json'), `${JSON.stringify({
  schemaVersion: PRODUCT_RELEASE_SCHEMA_VERSION,
  target,
  packages: packages.toSorted((left, right) => left.name.localeCompare(right.name)),
}, null, 2)}\n`)
const checksums = tarballs.map(name => `${sha256(join(outputRoot, name))}  ${name}`)
writeFileSync(join(outputRoot, 'SHA256SUMS'), `${checksums.join('\n')}\n`)
process.stdout.write(
  `packed ${target} release with release-packages.json and SHA256SUMS in ${outputRoot}\n`,
)
