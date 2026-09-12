import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import test from 'node:test'

import {
  buildCommunityCoreSourceManifest,
  CommunityCoreSourceError,
} from '../scripts/build-community-core-release.mjs'

function runGit(root, arguments_) {
  const result = spawnSync('git', arguments_, { cwd: root, encoding: 'utf8' })
  assert.equal(result.status, 0, result.stderr)
  return result.stdout.trim()
}

async function write(root, path, contents) {
  const target = join(root, path)
  await mkdir(join(target, '..'), { recursive: true })
  await writeFile(target, contents)
}

function contract(overrides = {}) {
  const value = {
    schemaVersion: 1,
    kind: 'winwincode.community-core-release-contract.v1',
    status: 'proposed',
    ownerRepository: 'winwincode',
    currentState: {
      rustPackages: {
        intendedLinkableCore: [{ name: 'winwincode-domain', manifest: 'crates/domain/Cargo.toml' }],
        communityOnlyAdapters: [{
          name: 'winwincode-observability-sqlite',
          manifest: 'crates/observability-sqlite/Cargo.toml',
        }],
        runtimeOnlySourcePackages: ['winwincode-worker'],
      },
      npmPackages: {
        packages: [{ name: '@winwincode/contracts', manifest: 'packages/contracts/package.json' }],
      },
      contracts: { generator: 'scripts/generate-contracts.mjs' },
    },
    targetState: {
      consumableRustCrates: [{ name: 'winwincode-domain' }],
      consumableNpmPackages: [{ name: '@winwincode/contracts' }],
      contractBundle: {
        canonicalSchemas: ['schema/v1/domain.schema.json'],
        generatedContracts: ['schema/v1/openapi.generated.json'],
        protocolSamples: ['schema/v1/domain.samples.json'],
      },
      coreLockManifest: {
        schemaPath: 'schema/core-lock.schema.json',
      },
      releaseManifest: {
        signature: {
          algorithm: 'Ed25519',
          publicKeyFile: 'release-key.pem',
          publicKeySha256: 'a'.repeat(64),
        },
      },
      forbiddenCoreContent: {
        rustCrates: ['winwincode-server', 'winwincode-enterprise', 'winwincode-observability-sqlite'],
        npmPackages: ['@winwincode/client'],
      },
    },
  }
  return { ...value, ...overrides }
}

async function createFixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'winwincode-core-source-'))
  t.after(() => rm(root, { recursive: true, force: true }))
  await write(root, 'release-contract.json', `${JSON.stringify(contract(), null, 2)}\n`)
  await write(root, 'crates/domain/Cargo.toml', '[package]\nname = "winwincode-domain"\nversion = "0.0.0"\n')
  await write(root, 'crates/domain/src/lib.rs', 'pub struct Domain;\n')
  await write(root, 'crates/worker/Cargo.toml', '[package]\nname = "winwincode-worker"\nversion = "0.0.0"\n')
  await write(root, 'crates/worker/src/main.rs', 'fn main() {}\n')
  await write(root, 'crates/observability-sqlite/Cargo.toml', '[package]\nname = "winwincode-observability-sqlite"\nversion = "0.0.0"\n')
  await write(root, 'crates/observability-sqlite/src/lib.rs', 'pub struct SqliteObservability;\n')
  await write(root, 'packages/contracts/package.json', '{"name":"@winwincode/contracts"}\n')
  await write(root, 'packages/contracts/src/index.ts', 'export type Contract = string\n')
  await write(root, 'schema/v1/domain.schema.json', '{}\n')
  await write(root, 'schema/v1/openapi.generated.json', '{}\n')
  await write(root, 'schema/v1/domain.samples.json', '[]\n')
  await write(root, 'schema/core-lock.schema.json', '{}\n')
  await write(root, 'release-key.pem', 'fixture public key\n')
  await write(root, 'scripts/generate-contracts.mjs', 'export {}\n')
  await write(root, 'LICENSE', 'Apache-2.0\n')
  await write(root, 'NOTICE', 'fixture\n')
  await write(root, 'THIRD_PARTY_NOTICES.md', 'none\n')
  runGit(root, ['init', '-q'])
  runGit(root, ['config', 'user.email', 'fixture@example.invalid'])
  runGit(root, ['config', 'user.name', 'Fixture'])
  runGit(root, ['add', '.'])
  runGit(root, ['commit', '-qm', 'fixture'])
  return root
}

async function expectCode(promise, code) {
  await assert.rejects(promise, error => {
    assert.ok(error instanceof CommunityCoreSourceError)
    assert.equal(error.code, code)
    return true
  })
}

test('source manifest is deterministic and hashes exact Git HEAD bytes', async t => {
  const root = await createFixture(t)
  const outputA = await mkdtemp(join(tmpdir(), 'winwincode-core-output-a-'))
  const outputB = await mkdtemp(join(tmpdir(), 'winwincode-core-output-b-'))
  t.after(() => Promise.all([rm(outputA, { recursive: true, force: true }), rm(outputB, { recursive: true, force: true })]))

  const first = await buildCommunityCoreSourceManifest({
    root,
    contractPath: join(root, 'release-contract.json'),
    outputDirectory: outputA,
  })
  await write(root, 'crates/domain/src/lib.rs', 'dirty working tree bytes must not enter the manifest\n')
  const second = await buildCommunityCoreSourceManifest({
    root,
    contractPath: join(root, 'release-contract.json'),
    outputDirectory: outputB,
  })

  assert.equal(first.bytes, second.bytes)
  assert.equal(await readFile(first.outputPath, 'utf8'), first.bytes)
  assert.equal(first.manifest.state, 'source-inventory-only')
  assert.equal(first.manifest.sourceCommit, runGit(root, ['rev-parse', 'HEAD']))
  assert.deepEqual(first.manifest.files.map(file => file.path), [...first.manifest.files.map(file => file.path)].sort())
  const domain = first.manifest.files.find(file => file.path === 'crates/domain/src/lib.rs')
  assert.equal(domain.sha256, createHash('sha256').update('pub struct Domain;\n').digest('hex'))
  assert.deepEqual(
    first.manifest.files.find(file => file.path === 'schema/core-lock.schema.json').scopes,
    ['core-lock-schema'],
  )
  assert.match(first.manifest.sourceSetSha256, /^[0-9a-f]{64}$/u)
  assert.equal(first.manifest.files.some(file => file.path.startsWith('crates/observability-sqlite/')), false)
})

test('foreign product package paths are rejected before they can enter a source manifest', async t => {
  const root = await createFixture(t)
  const malicious = contract()
  malicious.currentState.rustPackages.intendedLinkableCore = [
    { name: 'winwincode-enterprise', manifest: 'crates/winwincode-enterprise/Cargo.toml' },
  ]
  malicious.targetState.consumableRustCrates = [{ name: 'winwincode-enterprise' }]
  await write(root, 'release-contract.json', `${JSON.stringify(malicious, null, 2)}\n`)
  const output = await mkdtemp(join(tmpdir(), 'winwincode-core-output-scope-'))
  t.after(() => rm(output, { recursive: true, force: true }))

  await expectCode(
    buildCommunityCoreSourceManifest({ root, contractPath: join(root, 'release-contract.json'), outputDirectory: output }),
    'CORE_SCOPE_REJECTED',
  )
})

test('contract paths absent from Git HEAD fail with a stable missing-source error', async t => {
  const root = await createFixture(t)
  const missing = contract()
  missing.targetState.contractBundle.canonicalSchemas = ['schema/v1/missing.schema.json']
  await write(root, 'release-contract.json', `${JSON.stringify(missing, null, 2)}\n`)
  const output = await mkdtemp(join(tmpdir(), 'winwincode-core-output-missing-'))
  t.after(() => rm(output, { recursive: true, force: true }))

  await expectCode(
    buildCommunityCoreSourceManifest({ root, contractPath: join(root, 'release-contract.json'), outputDirectory: output }),
    'CORE_SOURCE_MISSING',
  )
})
