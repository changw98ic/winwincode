import assert from 'node:assert/strict'
import { readFileSync, readdirSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import test from 'node:test'

import {
  validateInfrastructureContract,
  validateInfrastructureLock,
} from '../scripts/build-community-infrastructure-release.mjs'

const root = resolve(import.meta.dirname, '..')
const contract = JSON.parse(readFileSync(join(root, 'docs/decisions/0031-infrastructure-ownership.json'), 'utf8'))
const expectedPackages = [
  'winwincode-connector-github',
  'winwincode-connector-jira',
  'winwincode-connector-linear',
  'winwincode-connector-slack',
  'winwincode-connector-teams',
  'winwincode-connector-webhook',
  'winwincode-data-export',
  'winwincode-domain',
  'winwincode-integration-core',
  'winwincode-observability-core',
  'winwincode-s3-artifact-adapter',
]

function descriptor(fileName = 'artifact.tar.gz') {
  return {
    fileName,
    artifactUri: `https://example.invalid/releases/${fileName}`,
    sha256: 'a'.repeat(64),
    bytes: 1,
  }
}

function lock() {
  return {
    schemaVersion: 1,
    kind: 'winwincode.infrastructure-lock.v1',
    consumerRepository: 'winwincode-cloud',
    release: {
      version: '0.1.0-alpha.1',
      sourceCommit: 'b'.repeat(40),
      sourceTag: 'community-infrastructure-v0.1.0-alpha.1',
    },
    manifest: descriptor('manifest.json'),
    signature: {
      ...descriptor('manifest.json.sig'),
      algorithm: 'Ed25519',
      signingKeySha256: 'c'.repeat(64),
    },
    sbom: descriptor('sbom.json'),
    licenseManifest: descriptor('licenses.json'),
    persistenceContract: descriptor('persistence.json'),
    fixtureCargoLock: descriptor('Cargo.lock'),
    packages: [{
      name: 'winwincode-integration-core',
      role: 'integration-core',
      version: '0.1.0-alpha.1',
      sourceCommit: 'b'.repeat(40),
      bundlePath: 'bundle/crates/winwincode-integration-core',
      format: 'cargo-workspace-tar-gzip',
      ...descriptor(),
    }],
  }
}

function filesUnder(directory) {
  const files = []
  const pending = [directory]
  while (pending.length > 0) {
    const current = pending.pop()
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const path = join(current, entry.name)
      if (entry.isDirectory()) pending.push(path)
      if (entry.isFile()) files.push(path)
    }
  }
  return files
}

test('accepted infrastructure contract names the complete neutral release', () => {
  assert.deepEqual(validateInfrastructureContract(contract), [])
  assert.deepEqual(contract.packages.map(entry => entry.name).sort(), expectedPackages)
  assert.equal(contract.release.baseUri.includes(contract.release.sourceTag), true)
  assert.equal(contract.persistenceContract.crate, 'winwincode-data-export')
})

test('release package sources and production manifests contain no product database runtime', () => {
  const forbiddenSource = contract.forbiddenReleaseContent.sourcePatterns.map(value => new RegExp(value, 'u'))
  const forbiddenDependencies = new Set(contract.forbiddenReleaseContent.runtimeDependencies)
  for (const entry of contract.packages) {
    const manifestPath = join(root, entry.manifest)
    const manifest = readFileSync(manifestPath, 'utf8')
    assert.equal(/^name\s*=\s*"([^"]+)"/mu.exec(manifest)?.[1], entry.name)
    assert.doesNotMatch(manifest, /^publish\s*=\s*false$/mu)
    const production = /^\[dependencies\]\n(?<body>[\s\S]*?)(?=\n\[|$)/mu.exec(manifest)?.groups.body ?? ''
    for (const dependency of forbiddenDependencies) {
      assert.doesNotMatch(production, new RegExp(`^${dependency}\\s*=`, 'mu'))
    }
    const sourceRoot = join(root, dirname(entry.manifest), 'src')
    assert.equal(statSync(sourceRoot).isDirectory(), true)
    for (const path of filesUnder(sourceRoot)) {
      const source = readFileSync(path, 'utf8')
      for (const pattern of forbiddenSource) assert.doesNotMatch(source, pattern, path)
    }
  }
  const github = readFileSync(join(root, 'crates/winwincode-connector-github/src/lib.rs'), 'utf8')
  assert.doesNotMatch(github, /winwincode_publication|GitHubPublicationAdapter/u)
})

test('consumer lock rejects mutable, incomplete, and local dependency identities', () => {
  assert.deepEqual(validateInfrastructureLock(lock(), 'winwincode-cloud'), [])
  const mutable = lock()
  mutable.packages[0].artifactUri = '../local-package'
  mutable.local = { git: 'main' }
  const errors = validateInfrastructureLock(mutable, 'winwincode-cloud')
  assert.ok(errors.some(error => error.includes('HTTPS artifact')))
  assert.ok(errors.some(error => error.includes('git or local path')))
})

test('lock schema requires signed supply-chain evidence', () => {
  const schema = JSON.parse(readFileSync(join(root, 'schema/winwincode/infrastructure-lock.schema.json'), 'utf8'))
  for (const field of ['manifest', 'signature', 'sbom', 'licenseManifest', 'persistenceContract', 'fixtureCargoLock', 'packages']) {
    assert.ok(schema.required.includes(field))
  }
  assert.equal(schema.properties.signature.allOf[1].properties.algorithm.const, 'Ed25519')
})
