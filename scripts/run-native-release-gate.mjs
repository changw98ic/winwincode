#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

import { createNativeReleaseEvidence } from './native-release-evidence.mjs'

const root = resolve(import.meta.dirname, '..')
const credentialNamePattern = /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu

function parseArguments(argv) {
  const options = { target: null, output: null, sourceCommit: null }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (['--target', '--output', '--source-commit'].includes(argument)) {
      const value = argv[index + 1]
      if (value === undefined || value.startsWith('--')) {
        throw new Error(`${argument} requires a value`)
      }
      const key = argument === '--source-commit' ? 'sourceCommit' : argument.slice(2)
      options[key] = value
      index += 1
      continue
    }
    throw new Error(`unknown native release gate argument: ${argument}`)
  }
  if (options.target === null || options.output === null || options.sourceCommit === null) {
    throw new Error('--target, --output, and --source-commit are required')
  }
  return Object.freeze({
    target: options.target,
    output: resolve(options.output),
    sourceCommit: options.sourceCommit,
  })
}

function run(command, arguments_, label) {
  process.stdout.write(`release check: ${label}\n`)
  const result = spawnSync(command, arguments_, {
    cwd: root,
    env: process.env,
    stdio: 'inherit',
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0 || result.signal !== null) {
    throw new Error(`${label} failed`)
  }
}

function deterministicEvaluation() {
  const environment = Object.fromEntries(Object.entries(process.env).filter(([name]) => (
    !credentialNamePattern.test(name)
  )))
  const result = spawnSync(process.execPath, [
    'tests/fixtures/full-delivery-scenario.mjs',
  ], {
    cwd: root,
    env: environment,
    encoding: 'utf8',
    maxBuffer: 64 * 1_024 * 1_024,
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0 || result.signal !== null) {
    process.stderr.write(result.stderr)
    throw new Error('release-native full Delivery evaluation failed')
  }
  const line = result.stdout.split('\n').find(value => value.trim().startsWith('{'))
  if (line === undefined) throw new Error('full Delivery evaluation returned no JSON result')
  return JSON.parse(line)
}

const options = parseArguments(process.argv.slice(2))
mkdirSync(options.output, { recursive: true })

run('corepack', ['pnpm', 'format:check'], 'format and Rust formatting')
run('corepack', ['pnpm', 'lint'], 'source lint, typecheck, and Clippy')
run(process.execPath, ['scripts/verify-cpb-boundary.mjs'], 'CPB design-only boundary')
run('corepack', ['pnpm', 'test'], 'TypeScript and Rust product tests')
run(process.execPath, ['scripts/verify-upstream-lock.mjs'], 'pinned upstream source')
run('corepack', ['pnpm', 'build:ts'], 'TypeScript release packages')
run(
  'corepack',
  ['pnpm', 'build:native', '--release', '--target', options.target],
  'release native package',
)
run(
  process.execPath,
  ['scripts/verify-native-package.mjs', '--target', options.target, '--require-release'],
  'native identity, checksums, and notices',
)
run(process.execPath, ['scripts/verify-packages.mjs'], 'publish file allowlists')
run(
  process.execPath,
  ['scripts/verify-native-install.mjs', '--target', options.target, '--require-release'],
  'clean native install, kernel, and sandbox',
)
run(
  process.execPath,
  ['scripts/verify-installed-host.mjs', '--target', options.target, '--require-release'],
  'installed DSH Chat and StrongFlow host',
)
process.stdout.write('release check: full Delivery human review and rework\n')
const deterministicResult = deterministicEvaluation()
run(
  process.execPath,
  ['scripts/pack-native-release.mjs', '--target', options.target, '--output', options.output],
  'pack six release packages',
)
const evidence = createNativeReleaseEvidence({
  root,
  target: options.target,
  releaseDirectory: options.output,
  sourceCommit: options.sourceCommit,
  deterministicResult,
})
const evidencePath = join(options.output, 'native-release-evidence.json')
writeFileSync(evidencePath, `${JSON.stringify(evidence, null, 2)}\n`, { mode: 0o600 })
process.stdout.write(`native release gate passed for ${options.target}: ${evidencePath}\n`)
