#!/usr/bin/env node

import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const SUPPORTED_HOSTS = new Set([
  'darwin/arm64',
  'darwin/x64',
  'linux/arm64',
  'linux/x64',
])

export function validateRuntime({ nodeVersion, platform, architecture }) {
  const major = Number.parseInt(nodeVersion.split('.')[0] ?? '', 10)
  if (major !== 24) {
    return `Unsupported Node.js ${nodeVersion}. WinWinCode requires Node.js 24.x.`
  }
  if (!SUPPORTED_HOSTS.has(`${platform}/${architecture}`)) {
    return `Unsupported platform ${platform}/${architecture}. WinWinCode supports macOS and Linux on arm64 or x64.`
  }
  return undefined
}

function optionValue(args, name, fallback) {
  const index = args.indexOf(name)
  return index === -1 ? fallback : args[index + 1] ?? ''
}

export function main(args = process.argv.slice(2)) {
  const error = validateRuntime({
    nodeVersion: optionValue(args, '--node-version', process.versions.node),
    platform: optionValue(args, '--platform', process.platform),
    architecture: optionValue(args, '--arch', process.arch),
  })
  if (error !== undefined) {
    process.stderr.write(`${error}\n`)
    return 1
  }
  return 0
}

if (process.argv[1] !== undefined && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  process.exitCode = main()
}
