import assert from 'node:assert/strict'
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { spawnSync } from 'node:child_process'
import test from 'node:test'

import {
  verifyCoreLock,
  verifyCoreLockUpgrade,
  CORE_PROTOCOL_VERSION,
} from '../scripts/verify-core-lock.mjs'

const root = resolve(import.meta.dirname, '..')

const RUST_PACKAGES = [
  'winwincode-domain',
  'winwincode-api',
  'winwincode-client-port',
  'winwincode-execution-port',
  'winwincode-repository-context',
  'winwincode-observability-core',
  'winwincode-session',
  'winwincode-delivery',
  'winwincode-publication',
]
const NPM_PACKAGES = [
  '@winwincode/contracts',
  '@winwincode/strongflow',
  '@winwincode/browser-ui',
  '@winwincode/control-plane-client',
  '@winwincode/browser-core',
]
const TARGETS = [
  'aarch64-apple-darwin',
  'x86_64-apple-darwin',
  'aarch64-unknown-linux-gnu',
  'x86_64-unknown-linux-gnu',
]

function artifact(name, version = '1.2.3-alpha.1') {
  return {
    name,
    version,
    uri: `https://artifacts.invalid/core-v${version}/${encodeURIComponent(name)}`,
    bytes: 3,
    sha256: 'a'.repeat(64),
    licenseSpdx: 'Apache-2.0',
  }
}

function completeLock(consumerRepository = 'winwincode-cloud') {
  const version = '1.2.3-alpha.1'
  return {
    schemaVersion: 1,
    kind: 'winwincode.community-core-lock.v1',
    ownerRepository: 'winwincode',
    consumerRepository,
    core: {
      version,
      sourceCommit: 'b'.repeat(40),
      sourceTag: `core-v${version}`,
      protocolVersion: CORE_PROTOCOL_VERSION,
      releaseManifest: {
        ...artifact('community-core-release-manifest.json', version),
        signatureUri: `https://artifacts.invalid/core-v${version}/community-core-release-manifest.json.sig`,
        signingKeySha256: 'c'.repeat(64),
      },
      cargo: RUST_PACKAGES.map(name => ({
        ...artifact(name, version),
        registry: 'https://cargo.invalid/index',
      })),
      npm: NPM_PACKAGES.map(name => ({
        ...artifact(name, version),
        registry: 'https://npm.invalid/',
      })),
      contracts: artifact('winwincode-contracts', version),
      workerRuntimes: TARGETS.map(target => ({
        ...artifact(`winwincode-worker-${target}`, version),
        target,
      })),
      sbom: artifact('community-core-sbom', version),
      licenseManifest: artifact('community-core-licenses', version),
    },
  }
}

test('edition.3: complete Cloud and Enterprise locks pass beyond-schema rules', () => {
  for (const consumer of ['winwincode-cloud', 'winwincode-enterprise']) {
    const result = verifyCoreLock(completeLock(consumer))
    assert.equal(result.ok, true, JSON.stringify(result.errors))
  }
})

test('edition.3: protocol mismatch and sourceTag drift fail the build gate', () => {
  const wrongProtocol = completeLock()
  wrongProtocol.core.protocolVersion = 'winwincode/v0'
  assert.equal(verifyCoreLock(wrongProtocol).ok, false)
  assert.ok(verifyCoreLock(wrongProtocol).errors.some(error => error.includes('protocolVersion')))

  const wrongTag = completeLock()
  wrongTag.core.sourceTag = 'core-v9.9.9'
  assert.equal(verifyCoreLock(wrongTag).ok, false)
  assert.ok(verifyCoreLock(wrongTag).errors.some(error => error.includes('sourceTag')))
})

test('edition.3: mixed package versions are rejected', () => {
  const mixed = completeLock()
  mixed.core.cargo[1].version = '0.0.1'
  const result = verifyCoreLock(mixed)
  assert.equal(result.ok, false)
  assert.ok(result.errors.some(error => error.includes('mixed core versions') || error.includes('must equal core.version')))
})

test('edition.3: upgrade replaces the whole core; same-version digest drift fails', () => {
  const previous = completeLock()
  const next = completeLock()
  next.core.version = '2.0.0'
  next.core.sourceTag = 'core-v2.0.0'
  for (const list of [next.core.cargo, next.core.npm, next.core.workerRuntimes]) {
    for (const item of list) item.version = '2.0.0'
  }
  next.core.contracts.version = '2.0.0'
  next.core.sbom.version = '2.0.0'
  next.core.licenseManifest.version = '2.0.0'
  next.core.releaseManifest.version = '2.0.0'
  assert.equal(verifyCoreLockUpgrade(previous, next).ok, true)

  const drifted = completeLock()
  drifted.core.cargo[0].sha256 = 'd'.repeat(64)
  assert.equal(verifyCoreLockUpgrade(completeLock(), drifted).ok, false)
})

test('edition.3: local artifact digest verification fails closed', () => {
  const directory = mkdtempSync(join(tmpdir(), 'core-lock-'))
  try {
    // schema expects matching bytes and sha256; write a wrong byte stream.
    writeFileSync(join(directory, 'winwincode-contracts'), 'nope')
    const lock = completeLock()
    const result = verifyCoreLock(lock, { artifactDir: directory })
    assert.equal(result.ok, false)
    assert.ok(result.errors.some(error => error.includes('missing local artifact') || error.includes('mismatch')))
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})

test('edition.3: CLI exits non-zero on protocol mismatch', () => {
  const directory = mkdtempSync(join(tmpdir(), 'core-lock-cli-'))
  try {
    const lock = completeLock()
    lock.core.protocolVersion = 'winwincode/v0'
    const lockPath = join(directory, 'core.lock.json')
    writeFileSync(lockPath, `${JSON.stringify(lock, null, 2)}\n`)
    const result = spawnSync(
      'node',
      ['scripts/verify-core-lock.mjs', '--lock', lockPath],
      { cwd: root, encoding: 'utf8' },
    )
    assert.notEqual(result.status, 0)
    assert.match(result.stderr, /protocolVersion/)
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})

test('edition.3: CLI exits zero for a valid lock', () => {
  const directory = mkdtempSync(join(tmpdir(), 'core-lock-cli-ok-'))
  try {
    const lockPath = join(directory, 'core.lock.json')
    writeFileSync(lockPath, `${JSON.stringify(completeLock('winwincode-enterprise'), null, 2)}\n`)
    const result = spawnSync(
      'node',
      ['scripts/verify-core-lock.mjs', '--lock', lockPath],
      { cwd: root, encoding: 'utf8' },
    )
    assert.equal(result.status, 0, result.stdout + result.stderr)
    const payload = JSON.parse(result.stdout)
    assert.equal(payload.ok, true)
    assert.equal(payload.consumerRepository, 'winwincode-enterprise')
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})
