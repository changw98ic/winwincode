#!/usr/bin/env node

import assert from 'node:assert/strict'
import { createHash, createPublicKey, verify } from 'node:crypto'
import { lstatSync, readFileSync, readdirSync } from 'node:fs'
import { resolve } from 'node:path'

import { validateCommunityCoreReleaseContract } from './build-community-core-release.mjs'

const repositoryRoot = resolve(import.meta.dirname, '..')
const MANIFEST = 'community-core-release-manifest.json'
const SIGNATURE = `${MANIFEST}.sig`
const CHECKSUMS = 'SHA256SUMS'

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function fail(code, message) {
  throw new Error(`${code}: ${message}`)
}

function template(value, replacements) {
  return value.replaceAll(/\{([^}]+)\}/gu, (_, key) => replacements[key] ?? fail(
    'CORE_TEMPLATE_INVALID',
    `unknown template field ${key}`,
  ))
}

function npmArchiveName(name, version) {
  return `${name.slice(1).replace('/', '-')}-${version}.tgz`
}

function readJson(path, label) {
  try {
    return JSON.parse(readFileSync(path, 'utf8'))
  } catch (error) {
    fail('CORE_JSON_INVALID', `${label}: ${error.message}`)
  }
}

function expectedArtifacts(contract, version) {
  return [
    ...contract.targetState.consumableRustCrates.map(({ name }) => ({
      name,
      kind: 'cargo-crate',
      fileName: `${name}-${version}.crate`,
    })),
    ...contract.targetState.consumableNpmPackages.map(({ name }) => ({
      name,
      kind: 'npm-package',
      fileName: npmArchiveName(name, version),
    })),
    {
      name: 'winwincode-contracts',
      kind: 'contract-bundle',
      fileName: template(contract.targetState.contractBundle.fileName, { version }),
    },
    ...contract.targetState.workerRuntimeBundles.targets.map(target => ({
      name: `winwincode-worker-${target}`,
      kind: 'worker-runtime',
      target,
      fileName: template(contract.targetState.workerRuntimeBundles.fileName, { target, version }),
    })),
  ]
}

function assertRegularFiles(inputRoot, expectedNames) {
  const actual = readdirSync(inputRoot).toSorted()
  assert.deepEqual(actual, expectedNames.toSorted(), 'release file set differs from the signed Community Core contract')
  for (const name of actual) {
    const stats = lstatSync(resolve(inputRoot, name))
    if (!stats.isFile() || stats.isSymbolicLink()) fail('CORE_ARTIFACT_TYPE_INVALID', name)
  }
}

function assertChecksums(inputRoot, expectedNames) {
  const entries = readFileSync(resolve(inputRoot, CHECKSUMS), 'utf8')
    .trimEnd()
    .split('\n')
    .map(line => /^([0-9a-f]{64})  ([^/]+)$/u.exec(line) ?? fail('CORE_CHECKSUMS_INVALID', line))
  const byName = new Map(entries.map(([, digest, name]) => [name, digest]))
  assert.equal(byName.size, entries.length, 'SHA256SUMS contains duplicate files')
  assert.deepEqual([...byName.keys()].toSorted(), expectedNames.filter(name => name !== CHECKSUMS).toSorted())
  for (const [name, digest] of byName) {
    assert.equal(sha256(readFileSync(resolve(inputRoot, name))), digest, `SHA-256 mismatch for ${name}`)
  }
}

export function verifyCommunityCoreRelease({
  root = repositoryRoot,
  inputRoot,
  expectedTag,
  contractPath = resolve(root, 'docs/decisions/0031-community-core-release.json'),
}) {
  const contract = readJson(contractPath, 'release contract')
  const errors = validateCommunityCoreReleaseContract(contract)
  if (errors.length > 0) fail('CORE_CONTRACT_INVALID', errors.join('; '))
  const version = readJson(resolve(root, 'package.json'), 'package.json').version
  const tag = `core-v${version}`
  assert.equal(expectedTag, tag, 'requested tag differs from the workspace version')

  const artifacts = expectedArtifacts(contract, version)
  const sbom = template(contract.targetState.releaseManifest.sbom.fileName, { version })
  const licenses = template(contract.targetState.releaseManifest.licenseManifest.fileName, { version })
  const expectedNames = [...artifacts.map(({ fileName }) => fileName), sbom, licenses, MANIFEST, SIGNATURE, CHECKSUMS]
  assertRegularFiles(inputRoot, expectedNames)
  assertChecksums(inputRoot, expectedNames)

  const manifestBytes = readFileSync(resolve(inputRoot, MANIFEST))
  const manifest = readJson(resolve(inputRoot, MANIFEST), MANIFEST)
  assert.equal(manifest.kind, contract.targetState.releaseManifest.kind)
  assert.equal(manifest.version, version)
  assert.equal(manifest.sourceTag, tag)
  assert.match(manifest.sourceCommit, /^[0-9a-f]{40}$/u)
  assert.equal(manifest.protocolVersion, 'winwincode/v1')
  assert.equal(manifest.sbom.fileName, sbom)
  assert.equal(manifest.licenseManifest.fileName, licenses)

  const records = [
    ...manifest.artifacts.cargo,
    ...manifest.artifacts.npm,
    manifest.artifacts.contracts,
    ...manifest.artifacts.workerRuntimes,
  ]
  assert.deepEqual(
    records.map(({ name, kind, target, fileName }) => ({ name, kind, ...(target === undefined ? {} : { target }), fileName })),
    artifacts,
  )
  for (const record of records) {
    const bytes = readFileSync(resolve(inputRoot, record.fileName))
    assert.equal(record.version, version)
    assert.equal(record.bytes, bytes.length)
    assert.equal(record.sha256, sha256(bytes))
    assert.equal(record.licenseSpdx, 'Apache-2.0')
  }

  const publicKey = createPublicKey(readFileSync(resolve(
    root,
    contract.targetState.releaseManifest.signature.publicKeyFile,
  )))
  const fingerprint = sha256(publicKey.export({ type: 'spki', format: 'der' }))
  assert.equal(fingerprint, contract.targetState.releaseManifest.signature.publicKeySha256)
  assert.equal(manifest.signature.signingKeySha256, fingerprint)
  const signature = Buffer.from(readFileSync(resolve(inputRoot, SIGNATURE), 'utf8').trim(), 'base64url')
  assert.equal(verify(null, manifestBytes, publicKey, signature), true, 'Community Core signature is invalid')

  assert.equal(readJson(resolve(inputRoot, sbom), sbom).components.length, artifacts.length)
  assert.equal(readJson(resolve(inputRoot, licenses), licenses).components.length, artifacts.length)
  return Object.freeze({ version, sourceCommit: manifest.sourceCommit, tag, artifactCount: artifacts.length, fingerprint })
}

function parseArguments(argv) {
  const values = new Map()
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index]
    const value = argv[index + 1]
    if (!['--input', '--root', '--tag'].includes(key) || value === undefined) fail('CORE_ARGUMENT_INVALID', key ?? '')
    values.set(key, value)
  }
  if (!values.has('--input') || !values.has('--tag')) fail('CORE_ARGUMENT_INVALID', '--input and --tag are required')
  return {
    ...(values.has('--root') ? { root: resolve(values.get('--root')) } : {}),
    inputRoot: resolve(values.get('--input')),
    expectedTag: values.get('--tag'),
  }
}

if (process.argv[1] !== undefined && resolve(process.argv[1]) === resolve(import.meta.filename)) {
  try {
    process.stdout.write(`${JSON.stringify(verifyCommunityCoreRelease(parseArguments(process.argv.slice(2))))}\n`)
  } catch (error) {
    process.stderr.write(`${error.message}\n`)
    process.exitCode = 1
  }
}
