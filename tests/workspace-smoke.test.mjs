import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

import { describeHost } from '../apps/host/dist/index.js'
import {
  UnsupportedPlatformError,
  nativePackageName,
  resolveReleaseTarget,
} from '../packages/native/dist/index.js'

const root = resolve(import.meta.dirname, '..')

test('maps every first-release host to its Rust target', () => {
  assert.equal(resolveReleaseTarget('darwin', 'arm64'), 'aarch64-apple-darwin')
  assert.equal(resolveReleaseTarget('darwin', 'x64'), 'x86_64-apple-darwin')
  assert.equal(resolveReleaseTarget('linux', 'arm64'), 'aarch64-unknown-linux-gnu')
  assert.equal(resolveReleaseTarget('linux', 'x64'), 'x86_64-unknown-linux-gnu')
})

test('rejects an unsupported platform with an actionable message', () => {
  assert.throws(
    () => resolveReleaseTarget('win32', 'x64'),
    error => error instanceof UnsupportedPlatformError
      && error.message.includes('macOS and Linux')
      && error.message.includes('win32/x64'),
  )
})

test('keeps DSH chat as default and StrongFlow as advanced surface', () => {
  const host = describeHost('linux', 'x64')
  assert.equal(host.defaultSurface.id, 'chat')
  assert.deepEqual(host.surfaces.map(surface => surface.id), ['chat', 'strongflow'])
  assert.equal(host.surfaces[1]?.default, false)
})

test('maps every Rust target to one deterministic optional package', () => {
  assert.deepEqual(
    [
      'aarch64-apple-darwin',
      'x86_64-apple-darwin',
      'aarch64-unknown-linux-gnu',
      'x86_64-unknown-linux-gnu',
    ].map(target => [target, nativePackageName(target)]),
    [
      ['aarch64-apple-darwin', '@winwincode/native-darwin-arm64'],
      ['x86_64-apple-darwin', '@winwincode/native-darwin-x64'],
      ['aarch64-unknown-linux-gnu', '@winwincode/native-linux-arm64'],
      ['x86_64-unknown-linux-gnu', '@winwincode/native-linux-x64'],
    ],
  )
})

test('native release workflow exposes separate manual Linux and macOS lanes', () => {
  const workflow = readFileSync(
    resolve(root, '.github/workflows/native-release.yml'),
    'utf8',
  )
  assert.match(workflow, /^  workflow_dispatch:$/mu)
  assert.doesNotMatch(workflow, /^  (?:pull_request|push):$/mu)
  const platformOptions = [...workflow.matchAll(/^          - (linux|macos)$/gmu)]
    .map(match => match[1])
  assert.deepEqual(platformOptions, ['linux', 'macos'])
  assert.ok(workflow.includes('binutils bubblewrap pkg-config libcap-dev'))
  assert.ok(workflow.includes('kernel.apparmor_restrict_unprivileged_userns=0'))
  assert.ok(workflow.includes('bwrap --ro-bind / / --unshare-user --unshare-pid --unshare-net'))
  for (const [target, runner] of [
    ['aarch64-unknown-linux-gnu', 'ubuntu-24.04-arm'],
    ['x86_64-unknown-linux-gnu', 'ubuntu-24.04'],
    ['aarch64-apple-darwin', 'macos-15'],
    ['x86_64-apple-darwin', 'macos-15-intel'],
  ]) {
    assert.ok(
      workflow.includes(`\"target\":\"${target}\",\"runner\":\"${runner}\"`),
      `native release workflow is missing ${target} on ${runner}`,
    )
  }
  assert.equal(readFileSync(resolve(root, '.node-version'), 'utf8').trim(), '24.19.0')
  assert.ok(workflow.includes('node scripts/run-native-release-gate.mjs'))
  assert.ok(workflow.includes('--source-commit "${GITHUB_SHA}"'))
  assert.ok(workflow.includes('--output release-artifacts'))
  const releaseRunner = readFileSync(
    resolve(root, 'scripts/run-native-release-gate.mjs'),
    'utf8',
  )
  for (const command of [
    "['pnpm', 'format:check']",
    "['pnpm', 'lint']",
    "['pnpm', 'verify:fixture-cleanup']",
    "['pnpm', 'test']",
    "['scripts/verify-cpb-boundary.mjs']",
    "['scripts/verify-upstream-lock.mjs']",
    "['scripts/verify-native-package.mjs', '--target', options.target, '--require-release']",
    "['scripts/verify-native-install.mjs', '--target', options.target, '--require-release']",
    "['scripts/verify-installed-host.mjs', '--target', options.target, '--require-release']",
    "['scripts/pack-native-release.mjs', '--target', options.target, '--output', options.output]",
  ]) {
    assert.ok(releaseRunner.includes(command), `native release runner is missing ${command}`)
  }
  assert.doesNotMatch(workflow, /windows|win32|msvc/iu)
})

test('CLI package smoke exposes version and scaffold descriptor', () => {
  const hostManifest = JSON.parse(
    readFileSync(resolve(root, 'apps/host/package.json'), 'utf8'),
  )
  const version = spawnSync(process.execPath, ['apps/host/dist/cli.js', '--version'], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(version.status, 0, version.stderr)
  assert.equal(version.stdout.trim(), hostManifest.version)

  const descriptor = spawnSync(process.execPath, ['apps/host/dist/cli.js', '--print-scaffold'], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(descriptor.status, 0, descriptor.stderr)
  const parsed = JSON.parse(descriptor.stdout)
  assert.equal(parsed.defaultSurface.id, 'chat')
  assert.equal(parsed.components.length, 3)
})

test('preinstall guard fails clearly on unsupported hosts', () => {
  const result = spawnSync(process.execPath, [
    'scripts/check-runtime.mjs',
    '--platform',
    'win32',
    '--arch',
    'x64',
  ], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(result.status, 1)
  assert.match(result.stderr, /Unsupported platform win32\/x64/u)
})

test('preinstall guard fails clearly outside Node 24', () => {
  const result = spawnSync(process.execPath, [
    'scripts/check-runtime.mjs',
    '--node-version',
    '22.19.0',
  ], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(result.status, 1)
  assert.match(result.stderr, /requires Node\.js 24\.x/u)
})
