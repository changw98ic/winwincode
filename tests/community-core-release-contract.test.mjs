import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import test from 'node:test'
import { resolve } from 'node:path'

import Ajv2020 from 'ajv/dist/2020.js'

const root = resolve(import.meta.dirname, '..')
const contractPath = resolve(root, 'docs/decisions/0031-community-core-release.json')
const contract = JSON.parse(readFileSync(contractPath, 'utf8'))
const schemaPath = resolve(root, contract.targetState.coreLockManifest.schemaPath)
const schema = JSON.parse(readFileSync(schemaPath, 'utf8'))

const RUST_PACKAGES = Object.freeze([
  'winwincode-domain',
  'winwincode-api',
  'winwincode-client-port',
  'winwincode-execution-port',
  'winwincode-repository-context',
  'winwincode-observability-core',
  'winwincode-session',
  'winwincode-delivery',
  'winwincode-publication',
])
const NPM_PACKAGES = Object.freeze([
  '@winwincode/contracts',
  '@winwincode/strongflow',
  '@winwincode/browser-ui',
  '@winwincode/control-plane-client',
  '@winwincode/browser-core',
])
const CANONICAL_SCHEMAS = Object.freeze([
  'schema/winwincode/v1/client-control.schema.json',
  'schema/winwincode/v1/control-plane-events.schema.json',
  'schema/winwincode/v1/control-plane-http.schema.json',
  'schema/winwincode/v1/domain.schema.json',
  'schema/winwincode/v1/execution-port.schema.json',
])
const GENERATED_CONTRACTS = Object.freeze([
  'schema/winwincode/v1/schema-collection.generated.json',
  'schema/winwincode/v1/openapi.generated.json',
])
const PROTOCOL_SAMPLES = Object.freeze([
  'schema/winwincode/v1/domain.samples.json',
  'schema/winwincode/v1/examples/control-plane-http.examples.json',
])
const TARGETS = Object.freeze([
  'aarch64-apple-darwin',
  'x86_64-apple-darwin',
  'aarch64-unknown-linux-gnu',
  'x86_64-unknown-linux-gnu',
])

function validator() {
  const ajv = new Ajv2020({ allErrors: true, strict: true })
  return ajv.compile(schema)
}

function artifact(name, version = '1.2.3-alpha.1') {
  return {
    name,
    version,
    uri: `https://artifacts.invalid/core-v${version}/${encodeURIComponent(name)}`,
    bytes: 1024,
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
      protocolVersion: 'winwincode/v1',
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

function clone(value) {
  return structuredClone(value)
}

function assertRejected(lock, label) {
  const validate = validator()
  assert.equal(validate(lock), false, `${label} unexpectedly passed the core lock schema`)
  assert.ok(validate.errors?.length, `${label} did not produce a schema error`)
}

test('Community core contract has one exact cross-repository package and protocol set', () => {
  assert.equal(contract.ownerRepository, 'winwincode')
  assert.deepEqual(contract.consumerRepositories, [
    'winwincode-cloud',
    'winwincode-enterprise',
  ])
  assert.deepEqual(
    contract.targetState.consumableRustCrates.map(entry => entry.name),
    RUST_PACKAGES,
  )
  assert.deepEqual(
    contract.currentState.rustPackages.communityOnlyAdapters.map(entry => entry.name),
    ['winwincode-observability-sqlite'],
  )
  assert.ok(contract.targetState.forbiddenCoreContent.rustCrates.includes('winwincode-observability-sqlite'))
  assert.equal(contract.targetState.forbiddenCoreContent.rustCrates.includes('winwincode-observability-core'), false)
  assert.deepEqual(
    contract.targetState.consumableNpmPackages.map(entry => entry.name),
    NPM_PACKAGES,
  )
  assert.deepEqual(contract.targetState.contractBundle.canonicalSchemas, CANONICAL_SCHEMAS)
  assert.deepEqual(contract.targetState.contractBundle.generatedContracts, GENERATED_CONTRACTS)
  assert.deepEqual(contract.targetState.contractBundle.protocolSamples, PROTOCOL_SAMPLES)
  assert.deepEqual(contract.targetState.workerRuntimeBundles.targets, TARGETS)
  assert.equal(contract.targetState.workerRuntimeBundles.serverBinaryIncluded, false)
  assert.equal(contract.targetState.workerRuntimeBundles.clientStaticFilesIncluded, false)
  assert.equal(contract.targetState.coreLockManifest.schema, undefined)
  assert.equal(contract.targetState.coreLockManifest.schemaId, schema.$id)
})

test('every audited current source path exists in the repository', () => {
  assert.ok(contract.currentState.currentSourcePaths.length > 0)
  assert.equal(
    new Set(contract.currentState.currentSourcePaths).size,
    contract.currentState.currentSourcePaths.length,
  )
  for (const path of contract.currentState.currentSourcePaths) {
    assert.equal(existsSync(resolve(root, path)), true, `missing currentSourcePath: ${path}`)
  }
})

test('canonical schema accepts complete Cloud and Enterprise locks', () => {
  const validate = validator()
  for (const consumer of ['winwincode-cloud', 'winwincode-enterprise']) {
    const lock = completeLock(consumer)
    assert.equal(validate(lock), true, JSON.stringify(validate.errors))
  }
})

test('canonical schema rejects floating core and package versions', () => {
  for (const mutate of [
    lock => { lock.core.version = '^1.2.3' },
    lock => { lock.core.cargo[0].version = '>=1.2.3' },
    lock => { lock.core.npm[0].version = 'workspace:*' },
  ]) {
    const lock = completeLock()
    mutate(lock)
    assertRejected(lock, 'floating version')
  }
})

test('canonical schema rejects incomplete or relabeled package sets', () => {
  for (const mutate of [
    lock => { lock.core.cargo.pop() },
    lock => { lock.core.cargo[0].name = 'winwincode-server' },
    lock => { lock.core.npm[1].name = '@winwincode/client' },
  ]) {
    const lock = completeLock()
    mutate(lock)
    assertRejected(lock, 'wrong package set')
  }
})

test('canonical schema rejects local paths and non-HTTPS artifact sources', () => {
  for (const mutate of [
    lock => { lock.core.cargo[0].uri = '../vendor/winwincode-domain.crate' },
    lock => { lock.core.cargo[0].registry = 'file:./cargo-index' },
    lock => { lock.core.npm[0].uri = 'workspace:../contracts' },
  ]) {
    const lock = completeLock()
    mutate(lock)
    assertRejected(lock, 'local path')
  }
})

test('canonical schema rejects the wrong owner repository', () => {
  const lock = completeLock()
  lock.ownerRepository = 'winwincode-community'
  assertRejected(lock, 'wrong owner')
})

test('canonical schema rejects malformed SHA-256 values', () => {
  for (const mutate of [
    lock => { lock.core.releaseManifest.sha256 = 'sha256:abc' },
    lock => { lock.core.cargo[0].sha256 = 'A'.repeat(64) },
    lock => { lock.core.sbom.sha256 = '0'.repeat(63) },
  ]) {
    const lock = completeLock()
    mutate(lock)
    assertRejected(lock, 'malformed digest')
  }
})

test('canonical schema requires the SBOM and license manifest', () => {
  for (const field of ['sbom', 'licenseManifest']) {
    const lock = completeLock()
    delete lock.core[field]
    assertRejected(lock, `missing ${field}`)
  }
})

test('canonical schema requires the release manifest signature and trust anchor', () => {
  for (const field of ['signatureUri', 'signingKeySha256']) {
    const lock = completeLock()
    delete lock.core.releaseManifest[field]
    assertRejected(lock, `missing ${field}`)
  }
})
