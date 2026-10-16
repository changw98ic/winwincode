#!/usr/bin/env node

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { accessSync, closeSync, constants, mkdirSync, mkdtempSync, openSync, readFileSync, readSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

export const nextestVersion = '0.9.143'
export const heavyExpression = ['production_vertical', 'worker_lifecycle', 'code_mode_kernel']
  .map(name => `binary(${name})`).join(' | ')
export const laneFilter = lane => lane === 'e2e' ? heavyExpression : `not (${heavyExpression})`

function archiveDigest(path) {
  const digest = createHash('sha256')
  const fd = openSync(path, 'r')
  try {
    const buffer = Buffer.alloc(1024 * 1024)
    for (let bytes; (bytes = readSync(fd, buffer)) > 0;) digest.update(buffer.subarray(0, bytes))
    return digest.digest('hex')
  } finally {
    closeSync(fd)
  }
}

function run(command, args, root, env, capture = false) {
  const result = spawnSync(command, args, {
    cwd: root, env, encoding: 'utf8', stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit',
    maxBuffer: 32 * 1024 * 1024,
  })
  if (result.error) throw result.error
  if (result.status !== 0) {
    const error = new Error(`${command} failed: ${result.signal ?? result.status}`)
    error.exitCode = result.status ?? 1
    throw error
  }
  return result.stdout
}

export function testIds(list, excludeHelper = false) {
  return Object.values(list['rust-suites']).flatMap(suite => {
    if (excludeHelper && suite['package-name'] === 'winwincode-kernel-helper') return []
    return Object.entries(suite.testcases ?? {})
      .filter(([, value]) => value['filter-match'].status === 'matches')
      .map(([name]) => `${suite['binary-id']}::${name}`)
  }).sort()
}

function identity(root, env) {
  const nextest = run('cargo-nextest', ['--version'], root, env, true).split('\n')[0]
  assert.match(nextest, new RegExp(`^cargo-nextest ${nextestVersion.replaceAll('.', '\\.')} `))
  return {
    commit: run('git', ['rev-parse', 'HEAD'], root, env, true).trim(),
    run: env.GITHUB_RUN_ID ?? null,
    attempt: env.GITHUB_RUN_ATTEMPT ?? null,
    platform: process.platform,
    arch: process.arch,
    nextest: nextestVersion,
  }
}

export function produceArchive(root, output, env = process.env) {
  const before = identity(root, env)
  mkdirSync(output, { recursive: true })
  const file = join(output, 'tests.tar.zst')
  run('cargo', ['build', '--locked', '-p', 'winwincode-kernel-helper', '--bin', 'winwincode-kernel-helper'], root, env)
  run('cargo-nextest', ['nextest', 'archive', '--workspace', '--all-features', '--locked', '--archive-file', file], root, env)
  const baseline = {}
  for (const lane of ['test', 'e2e']) {
    baseline[lane] = testIds(JSON.parse(run('cargo-nextest', [
      'nextest', 'list', '--workspace', '--exclude', 'winwincode-kernel-helper', '--all-features', '--locked',
      '-E', laneFilter(lane), '--message-format', 'json',
    ], root, env, true)))
  }
  assert.deepEqual(identity(root, env), before)
  writeFileSync(join(output, 'manifest.json'), `${JSON.stringify({
    ...before, sha256: archiveDigest(file), baseline,
  }, null, 2)}\n`)
  process.stdout.write(`Rust test archive: ${file} (${statSync(file).size} bytes)\n`)
}

export function consumeArchive(root, output, lane, env = process.env) {
  assert.ok(['test', 'e2e'].includes(lane), 'archive consumer requires test or e2e lane')
  const manifest = JSON.parse(readFileSync(join(output, 'manifest.json'), 'utf8'))
  for (const [key, value] of Object.entries(identity(root, env))) {
    assert.equal(manifest[key], value, `archive ${key} mismatch`)
  }
  const file = join(output, 'tests.tar.zst')
  assert.equal(archiveDigest(file), manifest.sha256, 'archive checksum mismatch')
  const extraction = mkdtempSync(join(tmpdir(), 'wwc-nextest-'))
  try {
    const target = join(extraction, 'target')
    const environment = { ...env, CARGO_TARGET_DIR: target,
      WWC_WORKER_HELPER_EXECUTABLE: join(target, 'debug/winwincode-kernel-helper'),
      WINWINCODE_CODE_MODE_TEST_HELPER: join(target, 'debug/winwincode-kernel-helper'),
    }
    const filter = `package(winwincode-kernel-helper) | (${laneFilter(lane)})`
    const list = JSON.parse(run('cargo-nextest', [
      'nextest', 'list', '--archive-file', file, '--workspace-remap', root, '--extract-to', extraction,
      '-E', filter, '--message-format', 'json',
    ], root, environment, true))
    accessSync(environment.WWC_WORKER_HELPER_EXECUTABLE, constants.R_OK | constants.X_OK)
    assert.deepEqual(testIds(list, true), manifest.baseline[lane], `${lane} archive test IDs differ from original filter`)
    process.stdout.write(`Verified ${lane} archive: ${testIds(list).length} test IDs; no Cargo build\n`)
    run('cargo-nextest', [
      'nextest', 'run', '--binaries-metadata', join(target, 'nextest/binaries-metadata.json'),
      '--cargo-metadata', join(target, 'nextest/cargo-metadata.json'),
      '--workspace-remap', root, '--target-dir-remap', target, '--build-dir-remap', target,
      '-E', filter,
    ], root, environment)
  } finally {
    rmSync(extraction, { recursive: true, force: true })
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  const root = resolve(import.meta.dirname, '..')
  const [mode, directory] = process.argv.slice(2)
  const output = resolve(root, directory ?? '.cache/rust-test-archive')
  try {
    if (mode === 'produce') produceArchive(root, output)
    else if (mode === 'test' || mode === 'e2e') consumeArchive(root, output, mode)
    else throw new Error('usage: rust-test-archive.mjs produce|test|e2e [directory]')
  } catch (error) {
    process.stderr.write(`${error.stack}\n`)
    process.exitCode = error.exitCode ?? 1
  }
}
