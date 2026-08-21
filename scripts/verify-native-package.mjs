#!/usr/bin/env node

import { resolve } from 'node:path'

import {
  NATIVE_TARGETS,
  hostNativeTarget,
  verifyNativePrebuild,
} from './native-package-contract.mjs'

function parseArguments(argv) {
  let target
  let requireRelease = false
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--require-release') {
      requireRelease = true
      continue
    }
    if (argument === '--target') {
      target = argv[index + 1]
      if (target === undefined) throw new Error('--target requires a Rust target triple')
      index += 1
      continue
    }
    if (argument.startsWith('--target=')) {
      target = argument.slice('--target='.length)
      continue
    }
    throw new Error(`unknown verify-native-package argument: ${argument}`)
  }
  return { target: target ?? hostNativeTarget(), requireRelease }
}

const root = resolve(import.meta.dirname, '..')
const { target, requireRelease } = parseArguments(process.argv.slice(2))
if (target === undefined) {
  throw new Error(
    `unsupported host ${process.platform}/${process.arch}; expected one of `
    + NATIVE_TARGETS.map(configuration => configuration.host).join(', '),
  )
}
const result = verifyNativePrebuild({
  root,
  target,
  requireRelease,
  requireCurrentHost: true,
})
if (result.errors.length > 0) {
  for (const error of result.errors) process.stderr.write(`${error}\n`)
  process.exit(1)
}
process.stdout.write(`native package verified for ${target} (${result.buildInfo.profile})\n`)
