#!/usr/bin/env node

import { createHash, createPrivateKey, createPublicKey, sign, verify } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import {
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { resolve } from 'node:path'

import { validateCommunityCoreReleaseContract } from './build-community-core-release.mjs'
import {
  assertSourceCommit,
  assertSourceDateEpoch,
  canonicalJson,
  descriptorForFile,
} from './release-artifact-contract.mjs'

const repositoryRoot = resolve(import.meta.dirname, '..')
const CONTRACT_PATH = 'docs/decisions/0031-community-core-release.json'
const RELEASE_MANIFEST = 'community-core-release-manifest.json'
const RELEASE_SIGNATURE = `${RELEASE_MANIFEST}.sig`
const RELEASE_CHECKSUMS = 'SHA256SUMS'

export class CommunityCoreFinalizeError extends Error {
  constructor(code, message) {
    super(`${code}: ${message}`)
    this.name = 'CommunityCoreFinalizeError'
    this.code = code
  }
}

function fail(code, message) {
  throw new CommunityCoreFinalizeError(code, message)
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function replaceTemplate(template, values) {
  return template.replaceAll(/\{([^}]+)\}/gu, (_, key) => values[key] ?? fail(
    'CORE_TEMPLATE_INVALID',
    `unknown release file template field ${key}`,
  ))
}

function npmArchiveName(name, version) {
  return `${name.slice(1).replace('/', '-')}-${version}.tgz`
}

function releaseUri(tag, fileName) {
  return `https://github.com/changw98ic/winwincode/releases/download/${tag}/${fileName}`
}

function artifactRecord(inputRoot, definition, version, tag) {
  const path = resolve(inputRoot, definition.fileName)
  const descriptor = descriptorForFile(inputRoot, path)
  if (descriptor.bytes === 0) fail('CORE_ARTIFACT_EMPTY', `${definition.fileName} is empty`)
  return Object.freeze({
    name: definition.name,
    kind: definition.kind,
    version,
    uri: definition.uri ?? releaseUri(tag, definition.fileName),
    bytes: descriptor.bytes,
    sha256: descriptor.sha256,
    licenseSpdx: 'Apache-2.0',
    ...(definition.registry === undefined ? {} : { registry: definition.registry }),
    ...(definition.target === undefined ? {} : { target: definition.target }),
    fileName: definition.fileName,
  })
}

function inputDefinitions(contract, version) {
  const tag = `core-v${version}`
  const cargo = contract.targetState.consumableRustCrates.map(({ name }) => Object.freeze({
    name,
    kind: 'cargo-crate',
    fileName: `${name}-${version}.crate`,
    registry: 'https://crates.io',
    uri: `https://crates.io/api/v1/crates/${name}/${version}/download`,
  }))
  const npm = contract.targetState.consumableNpmPackages.map(({ name }) => {
    const fileName = npmArchiveName(name, version)
    return Object.freeze({
      name,
      kind: 'npm-package',
      fileName,
      registry: 'https://registry.npmjs.org',
      uri: `https://registry.npmjs.org/${name}/-/${fileName}`,
    })
  })
  const contracts = Object.freeze({
    name: 'winwincode-contracts',
    kind: 'contract-bundle',
    fileName: replaceTemplate(contract.targetState.contractBundle.fileName, { version }),
  })
  const workerRuntimes = contract.targetState.workerRuntimeBundles.targets.map(target => Object.freeze({
    name: `winwincode-worker-${target}`,
    kind: 'worker-runtime',
    target,
    fileName: replaceTemplate(contract.targetState.workerRuntimeBundles.fileName, { target, version }),
  }))
  return Object.freeze({ cargo, npm, contracts, workerRuntimes, tag })
}

function assertExactInputFiles(inputRoot, definitions) {
  const expected = [
    ...definitions.cargo,
    ...definitions.npm,
    definitions.contracts,
    ...definitions.workerRuntimes,
  ].map(entry => entry.fileName).toSorted()
  const actual = readdirSync(inputRoot).toSorted()
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    fail('CORE_ARTIFACT_SET_INVALID', 'release input files do not match the exact Community Core set')
  }
  for (const name of actual) {
    const stats = lstatSync(resolve(inputRoot, name))
    if (!stats.isFile() || stats.isSymbolicLink()) {
      fail('CORE_ARTIFACT_TYPE_INVALID', `${name} is not a regular release input file`)
    }
  }
}

function purl(artifact) {
  if (artifact.kind === 'cargo-crate') return `pkg:cargo/${artifact.name}@${artifact.version}`
  if (artifact.kind === 'npm-package') {
    const [scope, name] = artifact.name.split('/')
    return `pkg:npm/${encodeURIComponent(scope)}/${name}@${artifact.version}`
  }
  const qualifier = artifact.target === undefined ? '' : `?target=${artifact.target}`
  return `pkg:generic/${artifact.name}@${artifact.version}${qualifier}`
}

function buildSbom(artifacts, version, sourceCommit, timestamp) {
  return Object.freeze({
    bomFormat: 'CycloneDX',
    specVersion: '1.6',
    serialNumber: `urn:uuid:${sourceCommit.slice(0, 8)}-${sourceCommit.slice(8, 12)}-4${sourceCommit.slice(13, 16)}-a${sourceCommit.slice(17, 20)}-${sourceCommit.slice(20, 32)}`,
    version: 1,
    metadata: Object.freeze({
      timestamp,
      component: Object.freeze({
        type: 'framework',
        name: 'WinWinCode Community Core',
        version,
        'bom-ref': `pkg:generic/winwincode-community-core@${version}`,
      }),
      properties: Object.freeze([
        Object.freeze({ name: 'winwincode:source-commit', value: sourceCommit }),
      ]),
    }),
    components: Object.freeze(artifacts.map(artifact => Object.freeze({
      type: artifact.kind === 'worker-runtime' ? 'application' : 'library',
      name: artifact.name,
      version,
      purl: purl(artifact),
      'bom-ref': purl(artifact),
      hashes: Object.freeze([Object.freeze({ alg: 'SHA-256', content: artifact.sha256 })]),
      licenses: Object.freeze([Object.freeze({ license: Object.freeze({ id: 'Apache-2.0' }) })]),
      properties: Object.freeze([
        Object.freeze({ name: 'winwincode:artifact-kind', value: artifact.kind }),
        Object.freeze({ name: 'winwincode:artifact-uri', value: artifact.uri }),
        ...(artifact.target === undefined
          ? []
          : [Object.freeze({ name: 'winwincode:target', value: artifact.target })]),
      ]),
    }))),
  })
}

function buildLicenseManifest(artifacts, version, sourceCommit) {
  return Object.freeze({
    schemaVersion: 1,
    kind: 'winwincode.community-core-license-manifest.v1',
    version,
    sourceCommit,
    projectLicenseSpdx: 'Apache-2.0',
    projectFiles: Object.freeze(['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']),
    components: Object.freeze(artifacts.map(artifact => Object.freeze({
      name: artifact.name,
      version,
      purl: purl(artifact),
      licenseSpdx: artifact.licenseSpdx,
      source: artifact.uri,
      noticeFiles: Object.freeze(['NOTICE', 'THIRD_PARTY_NOTICES.md']),
      artifactSha256: artifact.sha256,
    }))),
  })
}

function metadataArtifact(stagingRoot, name, kind, fileName, version, tag) {
  const descriptor = descriptorForFile(stagingRoot, resolve(stagingRoot, fileName))
  return Object.freeze({
    name,
    kind,
    version,
    uri: releaseUri(tag, fileName),
    bytes: descriptor.bytes,
    sha256: descriptor.sha256,
    licenseSpdx: 'Apache-2.0',
    fileName,
  })
}

function signingKey(root, contract, privateKeyPem) {
  let privateKey
  try {
    privateKey = createPrivateKey(privateKeyPem)
  } catch {
    fail('CORE_SIGNING_KEY_INVALID', 'Community Core release private key is invalid')
  }
  if (privateKey.asymmetricKeyType !== 'ed25519') {
    fail('CORE_SIGNING_KEY_INVALID', 'Community Core release private key must be Ed25519')
  }
  const publicKey = createPublicKey(privateKey)
  const fingerprint = sha256(publicKey.export({ type: 'spki', format: 'der' }))
  const expected = contract.targetState.releaseManifest.signature.publicKeySha256
  if (fingerprint !== expected) fail('CORE_SIGNING_KEY_MISMATCH', 'Community Core release signing key does not match the pinned public key')
  const pinnedPublicKey = createPublicKey(readFileSync(resolve(
    root,
    contract.targetState.releaseManifest.signature.publicKeyFile,
  )))
  if (sha256(pinnedPublicKey.export({ type: 'spki', format: 'der' })) !== expected) {
    fail('CORE_PUBLIC_KEY_MISMATCH', 'pinned Community Core public key fingerprint changed')
  }
  return Object.freeze({ privateKey, publicKey, fingerprint })
}

function writeChecksums(stagingRoot) {
  const lines = readdirSync(stagingRoot)
    .filter(name => name !== RELEASE_CHECKSUMS)
    .map(name => `${sha256(readFileSync(resolve(stagingRoot, name)))}  ${name}`)
    .toSorted()
  writeFileSync(resolve(stagingRoot, RELEASE_CHECKSUMS), `${lines.join('\n')}\n`, { flag: 'wx' })
}

export function finalizeCommunityCoreRelease({
  root = repositoryRoot,
  contractPath = resolve(root, CONTRACT_PATH),
  inputRoot,
  outputRoot,
  sourceCommit,
  sourceDateEpoch,
  privateKeyPem,
}) {
  assertSourceCommit(sourceCommit)
  assertSourceDateEpoch(sourceDateEpoch)
  if (typeof privateKeyPem !== 'string' || privateKeyPem.length === 0) {
    fail('CORE_SIGNING_KEY_REQUIRED', 'Community Core release private key is required')
  }
  const contract = JSON.parse(readFileSync(contractPath, 'utf8'))
  const errors = validateCommunityCoreReleaseContract(contract)
  if (errors.length > 0) fail('CORE_CONTRACT_INVALID', errors.join('; '))
  const version = JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8')).version
  const definitions = inputDefinitions(contract, version)
  assertExactInputFiles(inputRoot, definitions)
  if (existsSync(outputRoot)) fail('CORE_OUTPUT_EXISTS', `${outputRoot} already exists`)

  const key = signingKey(root, contract, privateKeyPem)
  const stagingRoot = `${outputRoot}.tmp-${process.pid}`
  if (existsSync(stagingRoot)) fail('CORE_OUTPUT_EXISTS', `${stagingRoot} already exists`)
  mkdirSync(stagingRoot, { recursive: true })
  try {
    const allDefinitions = [
      ...definitions.cargo,
      ...definitions.npm,
      definitions.contracts,
      ...definitions.workerRuntimes,
    ]
    for (const definition of allDefinitions) {
      copyFileSync(resolve(inputRoot, definition.fileName), resolve(stagingRoot, definition.fileName))
    }
    const records = allDefinitions.map(definition => artifactRecord(
      stagingRoot,
      definition,
      version,
      definitions.tag,
    ))
    const byFile = new Map(records.map(record => [record.fileName, record]))
    const timestamp = new Date(sourceDateEpoch * 1_000).toISOString()
    const sbomFile = replaceTemplate(contract.targetState.releaseManifest.sbom.fileName, { version })
    writeFileSync(
      resolve(stagingRoot, sbomFile),
      canonicalJson(buildSbom(records, version, sourceCommit, timestamp)),
      { flag: 'wx' },
    )
    const licenseFile = replaceTemplate(contract.targetState.releaseManifest.licenseManifest.fileName, { version })
    writeFileSync(
      resolve(stagingRoot, licenseFile),
      canonicalJson(buildLicenseManifest(records, version, sourceCommit)),
      { flag: 'wx' },
    )
    const sbom = metadataArtifact(stagingRoot, 'community-core-sbom', 'cyclonedx-sbom', sbomFile, version, definitions.tag)
    const licenseManifest = metadataArtifact(
      stagingRoot,
      'community-core-licenses',
      'license-manifest',
      licenseFile,
      version,
      definitions.tag,
    )
    const manifest = Object.freeze({
      schemaVersion: 1,
      kind: contract.targetState.releaseManifest.kind,
      version,
      sourceCommit,
      sourceTag: definitions.tag,
      sourceDateEpoch,
      protocolVersion: 'winwincode/v1',
      releasedAt: timestamp,
      artifacts: Object.freeze({
        cargo: Object.freeze(definitions.cargo.map(entry => byFile.get(entry.fileName))),
        npm: Object.freeze(definitions.npm.map(entry => byFile.get(entry.fileName))),
        contracts: byFile.get(definitions.contracts.fileName),
        workerRuntimes: Object.freeze(definitions.workerRuntimes.map(entry => byFile.get(entry.fileName))),
      }),
      sbom,
      licenseManifest,
      checksums: Object.freeze({ fileName: RELEASE_CHECKSUMS, algorithm: 'SHA-256' }),
      signature: Object.freeze({
        fileName: RELEASE_SIGNATURE,
        algorithm: 'Ed25519',
        encoding: 'base64url',
        signingKeySha256: key.fingerprint,
      }),
    })
    const manifestBytes = Buffer.from(canonicalJson(manifest))
    writeFileSync(resolve(stagingRoot, RELEASE_MANIFEST), manifestBytes, { flag: 'wx' })
    const signatureBytes = Buffer.from(`${sign(null, manifestBytes, key.privateKey).toString('base64url')}\n`)
    if (!verify(null, manifestBytes, key.publicKey, Buffer.from(signatureBytes.toString('utf8').trim(), 'base64url'))) {
      fail('CORE_SIGNATURE_INVALID', 'Community Core release signature did not verify')
    }
    writeFileSync(resolve(stagingRoot, RELEASE_SIGNATURE), signatureBytes, { flag: 'wx' })
    writeChecksums(stagingRoot)
    renameSync(stagingRoot, outputRoot)
    return Object.freeze({
      outputRoot,
      version,
      sourceCommit,
      sourceTag: definitions.tag,
      artifactCount: records.length,
      releaseManifestSha256: sha256(manifestBytes),
      signingKeySha256: key.fingerprint,
    })
  } finally {
    rmSync(stagingRoot, { recursive: true, force: true })
  }
}

function parseArguments(argv) {
  const values = new Map()
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (!argument.startsWith('--')) throw new Error(`unexpected argument ${argument}`)
    const separator = argument.indexOf('=')
    if (separator !== -1) values.set(argument.slice(2, separator), argument.slice(separator + 1))
    else {
      const value = argv[index + 1]
      if (value === undefined || value.startsWith('--')) throw new Error(`${argument} requires a value`)
      values.set(argument.slice(2), value)
      index += 1
    }
  }
  const required = ['input', 'output', 'source-commit', 'source-date-epoch']
  for (const key of values.keys()) if (!required.includes(key)) throw new Error(`unknown argument --${key}`)
  for (const key of required) if (!values.has(key)) throw new Error(`--${key} is required`)
  return Object.freeze({
    inputRoot: resolve(values.get('input')),
    outputRoot: resolve(values.get('output')),
    sourceCommit: values.get('source-commit'),
    sourceDateEpoch: Number(values.get('source-date-epoch')),
  })
}

function assertReleaseSource(root, sourceCommit, sourceDateEpoch) {
  const run = arguments_ => {
    const result = spawnSync('git', arguments_, { cwd: root, encoding: 'utf8' })
    if (result.status !== 0) fail('CORE_GIT_ERROR', result.stderr.trim() || `git ${arguments_.join(' ')} failed`)
    return result.stdout.trim()
  }
  if (run(['rev-parse', 'HEAD']) !== sourceCommit) {
    fail('CORE_SOURCE_MISMATCH', 'Community Core release source commit does not match HEAD')
  }
  if (Number(run(['show', '-s', '--format=%ct', sourceCommit])) !== sourceDateEpoch) {
    fail('CORE_SOURCE_MISMATCH', 'Community Core release source date does not match the commit')
  }
  if (run(['status', '--porcelain=v1', '--untracked-files=all']).length > 0) {
    fail('CORE_SOURCE_NOT_CLEAN', 'Community Core release requires a clean checkout')
  }
}

if (process.argv[1] !== undefined && resolve(process.argv[1]) === resolve(import.meta.filename)) {
  try {
    const options = parseArguments(process.argv.slice(2))
    assertReleaseSource(repositoryRoot, options.sourceCommit, options.sourceDateEpoch)
    const result = finalizeCommunityCoreRelease({
      ...options,
      privateKeyPem: process.env.WINWINCODE_CORE_RELEASE_PRIVATE_KEY_PEM,
    })
    process.stdout.write(canonicalJson({ status: 'passed', ...result }))
  } catch (error) {
    process.stderr.write(`${error.message}\n`)
    process.exitCode = 1
  }
}
