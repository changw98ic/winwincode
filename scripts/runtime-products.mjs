// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { appendFileSync, closeSync, mkdirSync, openSync, readFileSync, readSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { verifyApiProductionSourceSeal } from './run-api-production-vertical.mjs'

export const runtimeProductFiles = Object.freeze([
  'winwincode-server', 'wwc', 'winwincode-worker', 'winwincode-kernel-helper',
  'winwincode-api-production.source.json',
])

function run(command, args, root, env, capture = false) {
  const result = spawnSync(command, args, { cwd: root, env, encoding: 'utf8',
    stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit' })
  if (result.error) throw result.error
  assert.equal(result.status, 0, `${command} failed: ${result.signal ?? result.status}`)
  return result.stdout
}

function identity(root, env) {
  return {
    commit: run('git', ['rev-parse', 'HEAD'], root, env, true).trim(),
    run: env.GITHUB_RUN_ID ?? null,
    attempt: env.GITHUB_RUN_ATTEMPT ?? null,
    platform: process.platform,
    arch: process.arch,
  }
}

function archiveDigest(path) {
  const hash = createHash('sha256')
  const fd = openSync(path, 'r')
  try {
    const buffer = Buffer.alloc(1024 * 1024)
    for (let bytes; (bytes = readSync(fd, buffer)) > 0;) hash.update(buffer.subarray(0, bytes))
    return hash.digest('hex')
  } finally { closeSync(fd) }
}

function verify(root) {
  return verifyApiProductionSourceSeal({ root,
    serverBinary: join(root, 'target/debug/winwincode-server'),
    helperExecutable: join(root, 'target/debug/winwincode-kernel-helper') })
}

export function produceRuntimeProducts(root, output, env = process.env) {
  const before = identity(root, env)
  const originalSeal = readFileSync(verify(root).path)
  mkdirSync(output, { recursive: true })
  const archive = join(output, 'products.tar')
  run('tar', ['-cf', archive, '-C', join(root, 'target/debug'), ...runtimeProductFiles], root, env)
  assert.deepEqual(identity(root, env), before)
  assert.deepEqual(readFileSync(verify(root).path), originalSeal, 'producer must retain the original source seal')
  const digest = archiveDigest(archive)
  writeFileSync(join(output, 'manifest.json'), `${JSON.stringify({ ...before, sha256: digest }, null, 2)}\n`)
  if (env.GITHUB_OUTPUT) {
    appendFileSync(env.GITHUB_OUTPUT, `archive_sha256=${digest}\nproducer_attempt=${before.attempt}\n`)
  }
}

export function restoreRuntimeProducts(root, output, env = process.env) {
  const manifest = JSON.parse(readFileSync(join(output, 'manifest.json'), 'utf8'))
  const expected = identity(root, env)
  expected.attempt = env.WWC_RUNTIME_PRODUCER_ATTEMPT ?? null
  if (env.GITHUB_RUN_ID) {
    assert.match(expected.attempt ?? '', /^\d+$/u, 'runtime producer attempt output is required')
    assert.match(env.WWC_RUNTIME_ARCHIVE_SHA256 ?? '', /^[a-f0-9]{64}$/u, 'runtime producer digest output is required')
  }
  for (const [key, value] of Object.entries(expected)) {
    assert.equal(manifest[key], value, `runtime product ${key} mismatch`)
  }
  if (env.WWC_RUNTIME_ARCHIVE_SHA256) {
    assert.equal(manifest.sha256, env.WWC_RUNTIME_ARCHIVE_SHA256, 'runtime producer digest mismatch')
  }
  const archive = join(output, 'products.tar')
  assert.equal(archiveDigest(archive), manifest.sha256, 'runtime archive checksum mismatch')
  const entries = run('tar', ['-tf', archive], root, env, true).trim().split('\n')
  assert.deepEqual(entries.toSorted(), [...runtimeProductFiles].toSorted(), 'runtime archive file inventory mismatch')
  const directory = join(root, 'target/debug')
  mkdirSync(directory, { recursive: true })
  run('tar', ['-xpf', archive, '-C', directory], root, env)
  verify(root)
  process.stdout.write('Restored and source-seal verified four runtime products; no compilation\n')
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  const root = resolve(import.meta.dirname, '..')
  const [mode] = process.argv.slice(2)
  const output = join(root, '.cache/runtime-products')
  if (mode === 'produce') produceRuntimeProducts(root, output)
  else if (mode === 'restore') restoreRuntimeProducts(root, output)
  else throw new Error('usage: runtime-products.mjs produce|restore')
}
