import { createHash, createPublicKey, verify } from 'node:crypto'
import {
  existsSync,
  readFileSync,
  readdirSync,
  statSync,
} from 'node:fs'
import { join, relative, resolve, sep } from 'node:path'

import {
  fileDescriptor,
  readCanonicalJson,
  releaseSourceSha256,
  verifyReleaseLegalBoundary,
} from './release-source-contract.mjs'

export const RELEASE_ARTIFACT_SCHEMA_VERSION = 1
export const RELEASE_ARTIFACT_KIND = 'winwincode.release-artifact.v1'
export const RELEASE_REPORT_KIND = 'winwincode.release-report.v1'
export const RELEASE_ARTIFACT_MANIFEST = 'release-artifact-manifest.json'
export const RELEASE_CHECKSUMS = 'SHA256SUMS'
export const HELPER_RELEASE_MANIFEST_NAME = 'winwincode-kernel-helper.release.json'

export function helperReleaseManifestArtifactPath(artifactRoot) {
  return resolve(artifactRoot, 'bin', HELPER_RELEASE_MANIFEST_NAME)
}

export const RELEASE_TARGETS = Object.freeze([
  Object.freeze({ target: 'aarch64-apple-darwin', os: 'macos', arch: 'arm64' }),
  Object.freeze({ target: 'x86_64-apple-darwin', os: 'macos', arch: 'x64' }),
  Object.freeze({ target: 'aarch64-unknown-linux-gnu', os: 'linux', arch: 'arm64' }),
  Object.freeze({ target: 'x86_64-unknown-linux-gnu', os: 'linux', arch: 'x64' }),
])

export const RUST_RELEASE_ARTIFACTS = Object.freeze([
  Object.freeze({
    packageName: 'winwincode-server',
    binaryName: 'winwincode-server',
    role: 'server',
    distribution: 'product',
  }),
  Object.freeze({
    packageName: 'winwincode-worker',
    binaryName: 'winwincode-worker',
    role: 'worker',
    distribution: 'product',
  }),
  Object.freeze({
    packageName: 'winwincode-kernel-helper',
    binaryName: 'winwincode-kernel-helper',
    role: 'worker-helper',
    distribution: 'worker-internal',
  }),
])

export const RELEASE_ARTIFACT_CHECKS = Object.freeze([
  'frozen-install',
  'format',
  'lint-and-typecheck',
  'tests',
  'clean-install',
  'product-build',
  'api-production-vertical',
  'isolated-rebuild',
])

const LEGAL_FILES = Object.freeze(['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md'])
const HELPER_RELEASE_MANIFEST_KEYS = Object.freeze([
  'binaryMode',
  'binaryPath',
  'binarySha256',
  'packageVersion',
  'protocol',
  'schemaVersion',
  'signature',
  'sourceSha256',
  'version',
].toSorted())
const COMMIT_PATTERN = /^[0-9a-f]{40}$/u
const SHA256_PATTERN = /^[0-9a-f]{64}$/u
const MACH_O_MAGIC_64 = 0xfeedfacf
const MACH_O_HEADER_BYTES = 32
const MACH_O_LC_UUID = 0x1b
const MACH_O_UUID_COMMAND_BYTES = 24
const MACH_O_UUID_PATTERN = /^[0-9a-f]{32}$/u

export class ReleaseArtifactError extends Error {
  constructor(code, message) {
    super(`${code}: ${message}`)
    this.name = 'ReleaseArtifactError'
    this.code = code
  }
}

function fail(code, message) {
  throw new ReleaseArtifactError(code, message)
}

function sortedObject(value) {
  if (Array.isArray(value)) return value.map(sortedObject)
  if (value === null || typeof value !== 'object') return value
  return Object.fromEntries(
    Object.keys(value)
      .toSorted((left, right) => left.localeCompare(right))
      .map(key => [key, sortedObject(value[key])]),
  )
}

export function canonicalJson(value) {
  return `${JSON.stringify(sortedObject(value), null, 2)}\n`
}

export function jsonSha256(value) {
  return createHash('sha256').update(canonicalJson(value)).digest('hex')
}

function normalizedRelative(root, path) {
  return relative(root, path).split(sep).join('/')
}

function filesBelow(directory) {
  const files = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) files.push(...filesBelow(path))
    else if (entry.isFile()) files.push(path)
    else fail('ARTIFACT_TYPE_INVALID', `${path} is not a regular file`)
  }
  return files.toSorted((left, right) => left.localeCompare(right))
}

export function descriptorForFile(root, path) {
  const descriptor = fileDescriptor(path)
  return Object.freeze({
    path: normalizedRelative(root, path),
    ...descriptor,
  })
}

export function descriptorsBelow(directory) {
  if (!existsSync(directory) || !statSync(directory).isDirectory()) {
    fail('ARTIFACT_MISSING', `missing artifact directory ${directory}`)
  }
  return Object.freeze(filesBelow(directory).map(path => descriptorForFile(directory, path)))
}

export function targetConfiguration(target) {
  const configuration = RELEASE_TARGETS.find(entry => entry.target === target)
  if (configuration === undefined) fail('TARGET_UNSUPPORTED', `unsupported release target ${target}`)
  return configuration
}

export function assertSourceCommit(sourceCommit) {
  if (!COMMIT_PATTERN.test(sourceCommit)) {
    fail('SOURCE_COMMIT_INVALID', 'source commit must be 40 lowercase hexadecimal characters')
  }
}

export function assertSourceDateEpoch(sourceDateEpoch) {
  if (!Number.isSafeInteger(sourceDateEpoch) || sourceDateEpoch <= 0) {
    fail('SOURCE_DATE_EPOCH_INVALID', 'SOURCE_DATE_EPOCH must be a positive integer')
  }
}

/** Read the linker-generated LC_UUID from one thin 64-bit Mach-O artifact. */
export function machoUuidForFile(path) {
  const bytes = readFileSync(path)
  if (bytes.length < MACH_O_HEADER_BYTES) {
    fail('ARTIFACT_FORMAT_INVALID', `${path} is not a 64-bit Mach-O artifact`)
  }
  const magicLittleEndian = bytes.readUInt32LE(0)
  const magicBigEndian = bytes.readUInt32BE(0)
  let readUInt32
  if (magicLittleEndian === MACH_O_MAGIC_64) readUInt32 = offset => bytes.readUInt32LE(offset)
  else if (magicBigEndian === MACH_O_MAGIC_64) readUInt32 = offset => bytes.readUInt32BE(offset)
  else fail('ARTIFACT_FORMAT_INVALID', `${path} is not a 64-bit Mach-O artifact`)

  const commandCount = readUInt32(16)
  const commandBytes = readUInt32(20)
  const commandEnd = MACH_O_HEADER_BYTES + commandBytes
  if (commandEnd > bytes.length) {
    fail('ARTIFACT_FORMAT_INVALID', `${path} has truncated Mach-O load commands`)
  }

  let offset = MACH_O_HEADER_BYTES
  const uuids = []
  for (let index = 0; index < commandCount; index += 1) {
    if (offset + 8 > commandEnd) {
      fail('ARTIFACT_FORMAT_INVALID', `${path} has truncated Mach-O load commands`)
    }
    const command = readUInt32(offset)
    const size = readUInt32(offset + 4)
    if (size < 8 || offset + size > commandEnd) {
      fail('ARTIFACT_FORMAT_INVALID', `${path} has an invalid Mach-O load command`)
    }
    if (command === MACH_O_LC_UUID) {
      if (size !== MACH_O_UUID_COMMAND_BYTES) {
        fail('ARTIFACT_FORMAT_INVALID', `${path} has an invalid LC_UUID command`)
      }
      uuids.push(bytes.subarray(offset + 8, offset + MACH_O_UUID_COMMAND_BYTES).toString('hex'))
    }
    offset += size
  }
  if (offset !== commandEnd || uuids.length !== 1 || !MACH_O_UUID_PATTERN.test(uuids[0])) {
    fail('ARTIFACT_FORMAT_INVALID', `${path} must contain exactly one LC_UUID command`)
  }
  return uuids[0]
}

/**
 * Build one deterministic Cargo environment for either isolated release build.
 *
 * Both cold builds use one stable physical Cargo target directory. Rustc gives
 * the last matching remap precedence, so mappings are ordered from broad host
 * paths to that specific Cargo target.
 */
export function createReleaseBuildEnvironment({
  baseEnvironment,
  root,
  buildRoot,
  targetDirectory,
  targetDirectories,
  target,
  runnerTemp,
  home,
  sourceDateEpoch,
}) {
  assertSourceDateEpoch(sourceDateEpoch)
  const targetIdentity = targetConfiguration(target)
  if (baseEnvironment === null || typeof baseEnvironment !== 'object'
    || typeof root !== 'string' || root.length === 0
    || typeof buildRoot !== 'string' || buildRoot.length === 0
    || typeof targetDirectory !== 'string' || targetDirectory.length === 0
    || !Array.isArray(targetDirectories)
    || targetDirectories.length !== 1
    || new Set(targetDirectories).size !== 1
    || targetDirectories.some(path => typeof path !== 'string' || path.length === 0)
    || !targetDirectories.includes(targetDirectory)) {
    fail('BUILD_ENVIRONMENT_INVALID', 'release builds must share one stable Cargo target path')
  }
  const pathRemapping = [
    [home, '.home'],
    [runnerTemp, '.runner-temp'],
    [root, '.'],
    [buildRoot, '.release-build'],
    ...targetDirectories.map(path => [path, '.target']),
  ]
    .filter(([source]) => typeof source === 'string' && source.length > 0)
    .map(([source, destination]) => `--remap-path-prefix=${source}=${destination}`)
  const deterministicLinkFlags = targetIdentity.os === 'macos'
    ? ['-Clink-arg=-Wl,-reproducible']
    : []
  return Object.freeze({
    ...baseEnvironment,
    CARGO_INCREMENTAL: '0',
    CARGO_TARGET_DIR: targetDirectory,
    CI: '1',
    COREPACK_ENABLE_DOWNLOAD_PROMPT: '0',
    RUSTFLAGS: [
      baseEnvironment.RUSTFLAGS,
      ...deterministicLinkFlags,
      ...pathRemapping,
    ].filter(Boolean).join(' '),
    SOURCE_DATE_EPOCH: String(sourceDateEpoch),
  })
}

function digestPaths(root, paths) {
  const hash = createHash('sha256')
  for (const path of paths.toSorted((left, right) => left.localeCompare(right))) {
    hash.update(normalizedRelative(root, path))
    hash.update('\0')
    hash.update(readFileSync(path))
    hash.update('\0')
  }
  return hash.digest('hex')
}

export function localCompositionIdentity(root) {
  const crateRoot = resolve(root, 'crates/winwincode-local')
  const sourcePaths = [join(crateRoot, 'Cargo.toml'), ...filesBelow(join(crateRoot, 'src'))]
  return Object.freeze({
    mode: 'server-local-composition',
    launcherBinary: 'winwincode-server',
    libraryPackage: 'winwincode-local',
    sourceSha256: digestPaths(root, sourcePaths),
    dependencyLock: descriptorForFile(root, resolve(root, 'Cargo.lock')),
  })
}

export function protocolIdentity(root) {
  const openapiPath = resolve(root, 'schema/winwincode/v1/openapi.generated.json')
  const executionPortPath = resolve(root, 'schema/winwincode/v1/execution-port.schema.json')
  const openapi = readCanonicalJson(openapiPath)
  const executionPort = readCanonicalJson(executionPortPath)
  if (openapi?.info?.version !== '1.0.0') {
    fail('PROTOCOL_INVALID', 'Control Plane OpenAPI version must be 1.0.0')
  }
  if (executionPort?.$id !== 'https://schemas.winwincode.dev/winwincode/v1/execution-port.schema.json'
    || executionPort?.title !== 'WinWinCode ExecutionPort v1') {
    fail('PROTOCOL_INVALID', 'ExecutionPort schema identity must be canonical v1')
  }
  return Object.freeze({
    controlPlane: Object.freeze({
      schemaVersion: 'winwincode/v1',
      openapiVersion: openapi.info.version,
      contract: descriptorForFile(root, openapiPath),
    }),
    executionPort: Object.freeze({
      schemaVersion: 'winwincode/v1',
      id: executionPort.$id,
      title: executionPort.title,
      contract: descriptorForFile(root, executionPortPath),
    }),
  })
}

export function sourceIdentity(root, sourceCommit, sourceDateEpoch) {
  assertSourceCommit(sourceCommit)
  assertSourceDateEpoch(sourceDateEpoch)
  const workspace = readCanonicalJson(resolve(root, 'package.json'))
  const cargo = readFileSync(resolve(root, 'Cargo.toml'), 'utf8')
  const cargoVersion = cargo.match(/\[workspace\.package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/u)?.[1]
  const cargoLicense = cargo.match(/\[workspace\.package\][\s\S]*?\nlicense\s*=\s*"([^"]+)"/u)?.[1]
  if (cargoVersion !== workspace.version) {
    fail(
      'VERSION_MISMATCH',
      `Cargo workspace ${String(cargoVersion)} does not match package version ${workspace.version}`,
    )
  }
  if (cargoLicense !== workspace.license) {
    fail(
      'LICENSE_MISMATCH',
      `Cargo workspace ${String(cargoLicense)} does not match package license ${workspace.license}`,
    )
  }
  const legalErrors = verifyReleaseLegalBoundary(root)
  if (legalErrors.length > 0) fail('LEGAL_BOUNDARY_FAILED', legalErrors.join('; '))
  return Object.freeze({
    repository: workspace.repository,
    commit: sourceCommit,
    sourceDateEpoch,
    version: workspace.version,
    license: workspace.license,
    releaseSourceSha256: releaseSourceSha256(root),
    manifests: Object.freeze({
      cargo: descriptorForFile(root, resolve(root, 'Cargo.toml')),
      package: descriptorForFile(root, resolve(root, 'package.json')),
    }),
    locks: Object.freeze({
      cargo: descriptorForFile(root, resolve(root, 'Cargo.lock')),
      pnpm: descriptorForFile(root, resolve(root, 'pnpm-lock.yaml')),
    }),
  })
}

function sameDescriptors(left, right, label) {
  if (canonicalJson(left) !== canonicalJson(right)) {
    fail('REPRODUCIBILITY_FAILED', `${label} differs between isolated builds`)
  }
}

function expectedRustDescriptors(artifactRoot, target) {
  const targetIdentity = targetConfiguration(target)
  return RUST_RELEASE_ARTIFACTS.map(artifact => {
    const path = resolve(artifactRoot, 'bin', artifact.binaryName)
    if (!existsSync(path) || !statSync(path).isFile()) {
      fail('ARTIFACT_MISSING', `missing Rust artifact ${artifact.binaryName}`)
    }
    return Object.freeze({
      ...artifact,
      ...descriptorForFile(artifactRoot, path),
      ...(targetIdentity.os === 'macos' ? { machoUuid: machoUuidForFile(path) } : {}),
    })
  })
}

function legalDescriptors(artifactRoot) {
  return LEGAL_FILES.map(name => {
    const path = resolve(artifactRoot, 'legal', name)
    if (!existsSync(path)) fail('ARTIFACT_MISSING', `missing legal artifact ${name}`)
    return descriptorForFile(artifactRoot, path)
  })
}

function helperReleaseSigningBytes(fields) {
  return Buffer.from([
    'winwincode-kernel-helper.release.v1',
    String(fields.schemaVersion),
    fields.protocol,
    String(fields.version),
    fields.packageVersion,
    fields.sourceSha256,
    fields.binarySha256,
    fields.binaryPath,
    String(fields.binaryMode),
  ].join('\0'))
}

function helperReleasePublicKey(publicKeyHex) {
  if (!/^[0-9a-f]{64}$/u.test(publicKeyHex)) {
    fail('HELPER_RELEASE_INVALID', 'helper release public key must be 32 lowercase hexadecimal bytes')
  }
  return createPublicKey({
    key: Buffer.concat([
      Buffer.from('302a300506032b6570032100', 'hex'),
      Buffer.from(publicKeyHex, 'hex'),
    ]),
    format: 'der',
    type: 'spki',
  })
}

function helperReleaseManifestIdentity({ root, artifactRoot, publicKeyHex, productVersion }) {
  const path = helperReleaseManifestArtifactPath(artifactRoot)
  if (!existsSync(path) || !statSync(path).isFile()) {
    fail('ARTIFACT_MISSING', `missing ${HELPER_RELEASE_MANIFEST_NAME}`)
  }
  if ((statSync(path).mode & 0o777) !== 0o644) {
    fail('HELPER_RELEASE_INVALID', `${HELPER_RELEASE_MANIFEST_NAME} must have mode 0644`)
  }
  const manifest = readCanonicalJson(path)
  assertExactIdentity(
    Object.keys(manifest).toSorted(),
    HELPER_RELEASE_MANIFEST_KEYS,
    'HELPER_RELEASE_INVALID',
    'helper release manifest fields are not canonical',
  )
  const helperPath = resolve(artifactRoot, 'bin/winwincode-kernel-helper')
  const helper = descriptorForFile(artifactRoot, helperPath)
  const helperMode = statSync(helperPath).mode & 0o777
  const sourceSha256 = `sha256:${createHash('sha256')
    .update(readFileSync(resolve(root, 'crates/helper/src/main.rs')))
    .digest('hex')}`
  if (manifest.schemaVersion !== 1
    || manifest.protocol !== 'winwincode-kernel-helper-release'
    || manifest.version !== 1
    || manifest.packageVersion !== productVersion
    || manifest.sourceSha256 !== sourceSha256
    || manifest.binarySha256 !== `sha256:${helper.sha256}`
    || manifest.binaryPath !== 'winwincode-kernel-helper'
    || manifest.binaryMode !== 0o755
    || helperMode !== manifest.binaryMode
    || typeof manifest.signature !== 'string'
    || !/^[A-Za-z0-9_-]{86}$/u.test(manifest.signature)) {
    fail('HELPER_RELEASE_INVALID', 'helper release manifest identity is invalid')
  }
  const signatureValid = verify(
    null,
    helperReleaseSigningBytes(manifest),
    helperReleasePublicKey(publicKeyHex),
    Buffer.from(manifest.signature, 'base64url'),
  )
  if (!signatureValid) fail('HELPER_RELEASE_INVALID', 'helper release signature is invalid')
  for (const binaryName of ['winwincode-server', 'winwincode-worker']) {
    const binary = readFileSync(resolve(artifactRoot, 'bin', binaryName))
    if (binary.indexOf(Buffer.from(publicKeyHex)) === -1) {
      fail(
        'HELPER_RELEASE_INVALID',
        `${binaryName} does not contain the helper release public key`,
      )
    }
  }
  return Object.freeze({
    ...descriptorForFile(artifactRoot, path),
    publicKeyHex,
  })
}

export function createReleaseArtifactManifest({
  root,
  artifactRoot,
  target,
  sourceCommit,
  sourceDateEpoch,
  replayRust,
  replayClient,
  helperReleasePublicKeyHex,
  replayHelperReleaseManifest,
}) {
  const targetIdentity = targetConfiguration(target)
  const rust = expectedRustDescriptors(artifactRoot, target)
  const source = sourceIdentity(root, sourceCommit, sourceDateEpoch)
  const helperReleaseManifest = helperReleaseManifestIdentity({
    root,
    artifactRoot,
    publicKeyHex: helperReleasePublicKeyHex,
    productVersion: source.version,
  })
  const client = descriptorsBelow(resolve(artifactRoot, 'client')).map(entry => Object.freeze({
    ...entry,
    path: `client/${entry.path}`,
  }))
  if (!client.some(entry => entry.path === 'client/index.html')
    || !client.some(entry => entry.path === 'client/asset-manifest.json')
    || !client.some(entry => entry.path === 'client/version.json')) {
    fail('ARTIFACT_MISSING', 'Client static artifact is missing its entry or manifests')
  }
  sameDescriptors(
    rust.map(({ packageName, binaryName, role, distribution, bytes, sha256, machoUuid }) => ({
      packageName,
      binaryName,
      role,
      distribution,
      bytes,
      sha256,
      ...(targetIdentity.os === 'macos' ? { machoUuid } : {}),
    })),
    replayRust,
    'Rust artifacts',
  )
  sameDescriptors(
    client.map(({ path, bytes, sha256 }) => ({ path, bytes, sha256 })),
    replayClient,
    'Client static artifact',
  )
  sameDescriptors(
    {
      bytes: helperReleaseManifest.bytes,
      sha256: helperReleaseManifest.sha256,
      publicKeyHex: helperReleaseManifest.publicKeyHex,
    },
    replayHelperReleaseManifest,
    'helper release manifest',
  )
  return Object.freeze({
    schemaVersion: RELEASE_ARTIFACT_SCHEMA_VERSION,
    kind: RELEASE_ARTIFACT_KIND,
    target: targetIdentity,
    source,
    protocols: protocolIdentity(root),
    build: Object.freeze({
      profile: 'release',
      sourceDateEpoch,
      cargoIncremental: false,
      linkerMode: targetIdentity.os === 'macos'
        ? 'reproducible-lc-uuid'
        : 'platform-default',
      pathRemapping: Object.freeze([
        'home',
        'runner-temp',
        'source-root',
        'release-build',
        'cargo-target',
      ]),
      isolatedBuilds: 2,
      helperReleasePublicKeyHex,
    }),
    checks: RELEASE_ARTIFACT_CHECKS,
    artifacts: Object.freeze({
      rust: Object.freeze(rust),
      helperReleaseManifest,
      client: Object.freeze({
        package: '@winwincode/client',
        staticOnly: true,
        files: Object.freeze(client),
      }),
      localComposition: localCompositionIdentity(root),
      legal: Object.freeze(legalDescriptors(artifactRoot)),
    }),
    reproducibility: Object.freeze({
      verified: true,
      rustSha256Equal: true,
      clientSha256Equal: true,
      helperReleaseManifestSha256Equal: true,
    }),
  })
}

function assertDescriptor(artifactRoot, descriptor, label) {
  if (descriptor === null || typeof descriptor !== 'object'
    || typeof descriptor.path !== 'string'
    || descriptor.path.startsWith('/')
    || descriptor.path.split('/').includes('..')
    || !SHA256_PATTERN.test(descriptor.sha256)
    || !Number.isSafeInteger(descriptor.bytes)
    || descriptor.bytes < 0) {
    fail('MANIFEST_INVALID', `${label} descriptor is invalid`)
  }
  const path = resolve(artifactRoot, descriptor.path)
  if (!path.startsWith(`${resolve(artifactRoot)}${sep}`) || !existsSync(path) || !statSync(path).isFile()) {
    fail('ARTIFACT_MISSING', `${label} artifact ${descriptor.path} is missing`)
  }
  const actual = descriptorForFile(artifactRoot, path)
  if (actual.bytes !== descriptor.bytes || actual.sha256 !== descriptor.sha256) {
    fail('ARTIFACT_MISMATCH', `${label} artifact ${descriptor.path} changed`)
  }
}

function checksumLines(manifest) {
  const descriptors = [
    ...manifest.artifacts.rust,
    manifest.artifacts.helperReleaseManifest,
    ...manifest.artifacts.client.files,
    ...manifest.artifacts.legal,
  ]
  return descriptors
    .map(entry => `${entry.sha256}  ${entry.path}`)
    .toSorted((left, right) => left.localeCompare(right))
}

function assertExactIdentity(actual, expected, code, message) {
  if (canonicalJson(actual) !== canonicalJson(expected)) fail(code, message)
}

function validateClientManifests(artifactRoot, client, source) {
  const versionPath = resolve(artifactRoot, 'client/version.json')
  const assetManifestPath = resolve(artifactRoot, 'client/asset-manifest.json')
  const version = readCanonicalJson(versionPath)
  const assetManifest = readCanonicalJson(assetManifestPath)
  assertExactIdentity(version, {
    schemaVersion: 1,
    product: 'WinWinCode',
    package: '@winwincode/client',
    version: source.version,
    controlPlaneSchemaVersion: 'winwincode/v1',
  }, 'CLIENT_MANIFEST_INVALID', 'Client version.json does not match the release identity')

  const assets = client.files
    .filter(entry => !['client/asset-manifest.json', 'client/runtime-config.js'].includes(entry.path))
    .map(({ path, bytes, sha256 }) => ({
      path: path.slice('client/'.length),
      bytes,
      sha256,
    }))
  const targets = Object.fromEntries(RELEASE_TARGETS.map(({ target }) => [target, assets]))
  assertExactIdentity(assetManifest, {
    schemaVersion: 1,
    package: '@winwincode/client',
    version: source.version,
    entry: 'index.html',
    runtimeConfig: {
      path: 'runtime-config.js',
      field: 'serverUrl',
      mutableAtDeployment: true,
    },
    assets,
    targets,
  }, 'CLIENT_MANIFEST_INVALID', 'Client asset-manifest.json does not match its static files')
}

export function releaseChecksums(manifest) {
  return `${checksumLines(manifest).join('\n')}\n`
}

export function verifyReleaseArtifactDirectory({
  root,
  artifactRoot,
  expectedCommit,
  expectedTarget,
  expectedSourceDateEpoch,
}) {
  const manifestPath = resolve(artifactRoot, RELEASE_ARTIFACT_MANIFEST)
  if (!existsSync(manifestPath) || !statSync(manifestPath).isFile()) {
    fail('ARTIFACT_MISSING', `missing ${RELEASE_ARTIFACT_MANIFEST} for ${expectedTarget}`)
  }
  const manifest = readCanonicalJson(manifestPath)
  if (manifest.schemaVersion !== RELEASE_ARTIFACT_SCHEMA_VERSION
    || manifest.kind !== RELEASE_ARTIFACT_KIND) {
    fail('MANIFEST_INVALID', 'release artifact manifest identity is invalid')
  }
  assertExactIdentity(
    manifest.target,
    targetConfiguration(expectedTarget),
    'TARGET_MISMATCH',
    'target does not match',
  )
  if (manifest.source?.commit !== expectedCommit) fail('SOURCE_MISMATCH', 'source commit does not match')
  if (manifest.source?.sourceDateEpoch !== expectedSourceDateEpoch) {
    fail('SOURCE_DATE_EPOCH_MISMATCH', 'SOURCE_DATE_EPOCH does not match')
  }
  const currentSource = sourceIdentity(root, expectedCommit, expectedSourceDateEpoch)
  if (canonicalJson(manifest.source) !== canonicalJson(currentSource)) {
    fail('SOURCE_MISMATCH', 'source manifests, locks, version or digest changed')
  }
  if (canonicalJson(manifest.protocols) !== canonicalJson(protocolIdentity(root))) {
    fail('PROTOCOL_MISMATCH', 'protocol identity changed')
  }
  if (canonicalJson(manifest.artifacts.localComposition) !== canonicalJson(localCompositionIdentity(root))) {
    fail('LOCAL_COMPOSITION_MISMATCH', 'Local composition source or dependencies changed')
  }
  assertExactIdentity(manifest.build, {
    profile: 'release',
    sourceDateEpoch: expectedSourceDateEpoch,
    cargoIncremental: false,
    linkerMode: manifest.target.os === 'macos'
      ? 'reproducible-lc-uuid'
      : 'platform-default',
    pathRemapping: [
      'home',
      'runner-temp',
      'source-root',
      'release-build',
      'cargo-target',
    ],
    isolatedBuilds: 2,
    helperReleasePublicKeyHex: manifest.artifacts?.helperReleaseManifest?.publicKeyHex,
  }, 'MANIFEST_INVALID', 'release build identity is invalid')
  assertExactIdentity(
    manifest.checks,
    RELEASE_ARTIFACT_CHECKS,
    'MANIFEST_INVALID',
    'release check set is invalid',
  )
  assertExactIdentity(manifest.reproducibility, {
    verified: true,
    rustSha256Equal: true,
    clientSha256Equal: true,
    helperReleaseManifestSha256Equal: true,
  }, 'REPRODUCIBILITY_FAILED', 'isolated build comparison is not verified')

  if (!Array.isArray(manifest.artifacts?.rust)
    || manifest.artifacts.rust.length !== RUST_RELEASE_ARTIFACTS.length) {
    fail('MANIFEST_INVALID', 'Rust artifact set is invalid')
  }
  for (const descriptor of manifest.artifacts.rust) {
    if (manifest.target.os === 'macos') {
      if (!MACH_O_UUID_PATTERN.test(descriptor.machoUuid ?? '')) {
        fail('MANIFEST_INVALID', 'macOS Rust artifact LC_UUID is invalid')
      }
    } else if (Object.hasOwn(descriptor, 'machoUuid')) {
      fail('MANIFEST_INVALID', 'Linux Rust artifact must not declare a Mach-O UUID')
    }
  }
  assertExactIdentity(
    manifest.artifacts.rust.map(({
      packageName,
      binaryName,
      role,
      distribution,
      path,
      machoUuid,
    }) => ({
      packageName,
      binaryName,
      role,
      distribution,
      path,
      ...(manifest.target.os === 'macos' ? { machoUuid } : {}),
    })),
    RUST_RELEASE_ARTIFACTS.map(artifact => ({
      ...artifact,
      path: `bin/${artifact.binaryName}`,
      ...(manifest.target.os === 'macos'
        ? { machoUuid: manifest.artifacts.rust.find(entry => entry.binaryName === artifact.binaryName)?.machoUuid }
        : {}),
    })),
    'MANIFEST_INVALID',
    'Rust artifact identities are invalid',
  )
  if (manifest.artifacts?.client?.package !== '@winwincode/client'
    || manifest.artifacts.client.staticOnly !== true
    || !Array.isArray(manifest.artifacts.client.files)) {
    fail('MANIFEST_INVALID', 'Client artifact identity is invalid')
  }
  if (!Array.isArray(manifest.artifacts.legal)) {
    fail('MANIFEST_INVALID', 'legal artifact set is invalid')
  }
  assertExactIdentity(
    manifest.artifacts.legal.map(({ path }) => path),
    LEGAL_FILES.map(name => `legal/${name}`),
    'LEGAL_BOUNDARY_FAILED',
    'legal artifact set is invalid',
  )
  for (const descriptor of manifest.artifacts.rust) {
    assertDescriptor(artifactRoot, descriptor, 'Rust')
    if (manifest.target.os === 'macos') {
      const actualUuid = machoUuidForFile(resolve(artifactRoot, descriptor.path))
      if (actualUuid !== descriptor.machoUuid) {
        fail('ARTIFACT_MISMATCH', `${descriptor.path} LC_UUID changed`)
      }
    }
  }
  if (manifest.artifacts?.helperReleaseManifest === undefined) {
    fail('MANIFEST_INVALID', 'helper release manifest descriptor is missing')
  }
  assertDescriptor(
    artifactRoot,
    manifest.artifacts.helperReleaseManifest,
    'helper release manifest',
  )
  const helperReleaseManifest = helperReleaseManifestIdentity({
    root,
    artifactRoot,
    publicKeyHex: manifest.artifacts.helperReleaseManifest.publicKeyHex,
    productVersion: manifest.source.version,
  })
  assertExactIdentity(
    manifest.artifacts.helperReleaseManifest,
    helperReleaseManifest,
    'HELPER_RELEASE_INVALID',
    'helper release manifest descriptor is invalid',
  )
  for (const descriptor of manifest.artifacts.client.files) {
    assertDescriptor(artifactRoot, descriptor, 'Client')
  }
  validateClientManifests(artifactRoot, manifest.artifacts.client, manifest.source)
  for (const descriptor of manifest.artifacts.legal) {
    assertDescriptor(artifactRoot, descriptor, 'legal')
    const name = descriptor.path.slice('legal/'.length)
    if (!LEGAL_FILES.includes(name)
      || readFileSync(resolve(artifactRoot, descriptor.path)).compare(readFileSync(resolve(root, name))) !== 0) {
      fail('LEGAL_BOUNDARY_FAILED', `${descriptor.path} does not match the project legal file`)
    }
  }
  const checksumPath = resolve(artifactRoot, RELEASE_CHECKSUMS)
  if (!existsSync(checksumPath)
    || readFileSync(checksumPath, 'utf8') !== releaseChecksums(manifest)) {
    fail('CHECKSUM_MISMATCH', 'SHA256SUMS does not match the artifact manifest')
  }
  const expectedPaths = [
    RELEASE_ARTIFACT_MANIFEST,
    RELEASE_CHECKSUMS,
    ...manifest.artifacts.rust.map(entry => entry.path),
    manifest.artifacts.helperReleaseManifest.path,
    ...manifest.artifacts.client.files.map(entry => entry.path),
    ...manifest.artifacts.legal.map(entry => entry.path),
  ].toSorted((left, right) => left.localeCompare(right))
  const actualPaths = filesBelow(artifactRoot)
    .map(path => normalizedRelative(artifactRoot, path))
    .toSorted((left, right) => left.localeCompare(right))
  assertExactIdentity(
    actualPaths,
    expectedPaths,
    'ARTIFACT_SET_MISMATCH',
    'artifact directory contains an unlisted or duplicate file',
  )
  return manifest
}

export function createReleaseReport({ root, evidenceRoot, expectedCommit, sourceDateEpoch }) {
  assertSourceCommit(expectedCommit)
  assertSourceDateEpoch(sourceDateEpoch)
  if (!existsSync(evidenceRoot) || !statSync(evidenceRoot).isDirectory()) {
    fail('ARTIFACT_MISSING', `missing release evidence directory ${evidenceRoot}`)
  }
  const evidenceEntries = readdirSync(evidenceRoot, { withFileTypes: true })
  if (evidenceEntries.some(entry => !entry.isDirectory())) {
    fail('ARTIFACT_SET_MISMATCH', 'release evidence root may contain only target directories')
  }
  const expectedTargets = RELEASE_TARGETS.map(({ target }) => target)
  const actualTargets = evidenceEntries.map(entry => entry.name)
  const missingTargets = expectedTargets.filter(target => !actualTargets.includes(target))
  if (missingTargets.length > 0) {
    fail('ARTIFACT_MISSING', `missing release target evidence: ${missingTargets.join(', ')}`)
  }
  assertExactIdentity(
    actualTargets.toSorted((left, right) => left.localeCompare(right)),
    expectedTargets.toSorted((left, right) => left.localeCompare(right)),
    'ARTIFACT_SET_MISMATCH',
    'release evidence must contain the exact supported target set',
  )
  const targets = RELEASE_TARGETS.map(configuration => {
    const artifactRoot = resolve(evidenceRoot, configuration.target)
    const manifest = verifyReleaseArtifactDirectory({
      root,
      artifactRoot,
      expectedCommit,
      expectedTarget: configuration.target,
      expectedSourceDateEpoch: sourceDateEpoch,
    })
    return Object.freeze({
      target: configuration.target,
      manifest: descriptorForFile(evidenceRoot, resolve(artifactRoot, RELEASE_ARTIFACT_MANIFEST)),
      checksums: descriptorForFile(evidenceRoot, resolve(artifactRoot, RELEASE_CHECKSUMS)),
      rust: manifest.artifacts.rust,
      helperReleaseManifest: manifest.artifacts.helperReleaseManifest,
      clientSha256: jsonSha256(manifest.artifacts.client.files),
    })
  })
  if (new Set(targets.map(entry => entry.clientSha256)).size !== 1) {
    fail('CLIENT_MATRIX_MISMATCH', 'Client static artifact differs across targets')
  }
  if (new Set(targets.map(entry => entry.helperReleaseManifest.publicKeyHex)).size !== 1) {
    fail('HELPER_RELEASE_INVALID', 'helper release public key differs across targets')
  }
  return Object.freeze({
    schemaVersion: RELEASE_ARTIFACT_SCHEMA_VERSION,
    kind: RELEASE_REPORT_KIND,
    status: 'passed',
    source: sourceIdentity(root, expectedCommit, sourceDateEpoch),
    protocols: protocolIdentity(root),
    targets: Object.freeze(targets),
  })
}
