import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import test from 'node:test'

import {
  buildCommunityCoreMetadata,
  CommunityCoreMetadataError,
} from '../scripts/build-community-core-metadata.mjs'

function digest(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function compareText(left, right) {
  if (left < right) return -1
  if (left > right) return 1
  return 0
}

function sourceManifest(files = defaultFiles()) {
  const manifest = {
    schemaVersion: 1,
    kind: 'winwincode.community-core-source-manifest.v1',
    state: 'source-inventory-only',
    contract: {
      kind: 'winwincode.community-core-release-contract.v1',
      status: 'proposed',
      ownerRepository: 'winwincode',
    },
    sourceCommit: 'a'.repeat(40),
    fileCount: files.length,
    files,
  }
  manifest.sourceSetSha256 = digest(Buffer.from(JSON.stringify(files), 'utf8'))
  return manifest
}

function file(path, scopes, contents = path) {
  return {
    path,
    bytes: Buffer.byteLength(contents),
    sha256: digest(Buffer.from(contents, 'utf8')),
    scopes,
  }
}

function defaultFiles() {
  return [
    file('LICENSE', ['legal']),
    file('NOTICE', ['legal']),
    file('THIRD_PARTY_NOTICES.md', ['legal']),
    file('crates/domain/Cargo.toml', ['rust:winwincode-domain']),
    file('crates/domain/src/lib.rs', ['rust:winwincode-domain']),
    file('crates/worker/src/main.rs', ['runtime-source:winwincode-worker']),
    file('packages/contracts/package.json', ['npm:@winwincode/contracts']),
    file('schema/v1/domain.schema.json', ['contract-schema']),
  ]
}

async function fixture(t, manifest = sourceManifest()) {
  const root = await mkdtemp(join(tmpdir(), 'winwincode-metadata-root-'))
  const sourceManifestPath = join(root, 'community-core-source-manifest.json')
  await writeFile(sourceManifestPath, `${JSON.stringify(manifest, null, 2)}\n`)
  t.after(() => rm(root, { recursive: true, force: true }))
  return { root, sourceManifestPath }
}

async function output(t, prefix) {
  const path = await mkdtemp(join(tmpdir(), prefix))
  t.after(() => rm(path, { recursive: true, force: true }))
  return path
}

async function readOutputs(directory) {
  return {
    sbom: await readFile(join(directory, 'community-core-source.cdx.json'), 'utf8'),
    licenses: await readFile(join(directory, 'community-core-source.licenses.json'), 'utf8'),
    checksums: await readFile(join(directory, 'SHA256SUMS'), 'utf8'),
  }
}

async function expectCode(promise, code) {
  await assert.rejects(promise, error => {
    assert.ok(error instanceof CommunityCoreMetadataError)
    assert.equal(error.code, code)
    return true
  })
}

test('SBOM, license mapping, and checksums are deterministic source-only metadata', async t => {
  const { root, sourceManifestPath } = await fixture(t)
  const firstDirectory = await output(t, 'winwincode-metadata-a-')
  const secondDirectory = await output(t, 'winwincode-metadata-b-')
  const firstResult = await buildCommunityCoreMetadata({ root, sourceManifestPath, outputDirectory: firstDirectory })
  const secondResult = await buildCommunityCoreMetadata({ root, sourceManifestPath, outputDirectory: secondDirectory })
  const first = await readOutputs(firstDirectory)
  const second = await readOutputs(secondDirectory)

  assert.deepEqual(first, second)
  assert.equal(firstResult.sourceManifestSha256, secondResult.sourceManifestSha256)
  const sbom = JSON.parse(first.sbom)
  assert.equal(sbom.bomFormat, 'CycloneDX')
  assert.equal(sbom.specVersion, '1.6')
  assert.equal('serialNumber' in sbom, false)
  assert.equal(JSON.stringify(sbom).includes('published'), true)
  assert.equal(JSON.stringify(sbom).includes('not-published'), true)
  assert.equal(JSON.stringify(sbom).includes('timestamp'), false)
  assert.ok(sbom.components.some(component => component.name === 'winwincode-domain'))
  assert.ok(sbom.components.some(component => component.name === 'crates/domain/src/lib.rs'))

  const licenses = JSON.parse(first.licenses)
  assert.equal(licenses.state, 'source-inventory-only')
  assert.equal(licenses.artifacts.length, defaultFiles().length)
  assert.ok(licenses.artifacts.every(artifact => artifact.licenseSpdx === 'Apache-2.0'))
})

test('SHA256SUMS exactly matches both generated JSON files', async t => {
  const { root, sourceManifestPath } = await fixture(t)
  const directory = await output(t, 'winwincode-metadata-checksums-')
  await buildCommunityCoreMetadata({ root, sourceManifestPath, outputDirectory: directory })
  const generated = await readOutputs(directory)
  const actual = new Map(
    generated.checksums.trim().split('\n').map(line => {
      const [sha256, name] = line.split('  ')
      return [name, sha256]
    }),
  )

  assert.deepEqual([...actual.keys()].sort(), [
    'community-core-source.cdx.json',
    'community-core-source.licenses.json',
  ])
  assert.equal(actual.get('community-core-source.cdx.json'), digest(Buffer.from(generated.sbom, 'utf8')))
  assert.equal(actual.get('community-core-source.licenses.json'), digest(Buffer.from(generated.licenses, 'utf8')))
})

test('metadata generation rejects a source manifest without complete license evidence', async t => {
  const files = defaultFiles().filter(entry => entry.path !== 'NOTICE')
  const { root, sourceManifestPath } = await fixture(t, sourceManifest(files))
  const directory = await output(t, 'winwincode-metadata-license-')
  await expectCode(
    buildCommunityCoreMetadata({ root, sourceManifestPath, outputDirectory: directory }),
    'CORE_METADATA_LICENSE_MISSING',
  )
})

test('metadata generation rejects Cloud and Enterprise product paths', async t => {
  const files = [...defaultFiles(), file('apps/client/src/enterprise-application.ts', ['rust:winwincode-domain'])]
    .sort((left, right) => compareText(left.path, right.path))
  const { root, sourceManifestPath } = await fixture(t, sourceManifest(files))
  const directory = await output(t, 'winwincode-metadata-scope-')
  await expectCode(
    buildCommunityCoreMetadata({ root, sourceManifestPath, outputDirectory: directory }),
    'CORE_METADATA_SCOPE_REJECTED',
  )
})

test('metadata output must be explicit and outside the source repository', async t => {
  const { root, sourceManifestPath } = await fixture(t)
  await expectCode(
    buildCommunityCoreMetadata({ root, sourceManifestPath }),
    'CORE_METADATA_OUTPUT_REQUIRED',
  )
  await expectCode(
    buildCommunityCoreMetadata({ root, sourceManifestPath, outputDirectory: join(root, 'generated') }),
    'CORE_METADATA_OUTPUT_INSIDE_REPOSITORY',
  )
})
