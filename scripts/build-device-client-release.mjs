#!/usr/bin/env node

// SPDX-License-Identifier: Apache-2.0

//! Device Client four-target release package builder (OPS-100.1).
//!
//! Builds and packages the Device Client release bundle for one of the four
//! supported release targets (`RELEASE_TARGETS`): the `wwc` CLI that embeds
//! the `winwincode-device-client` daemon library, the Worker binary the
//! Device Client supervisor spawns, and the Worker-internal Kernel helper,
//! plus the legal files, a versioned package manifest, and a Cargo-lock
//! derived SBOM.
//!
//! The script reuses the existing release mechanism: the same target set,
//! deterministic build environment, staging layout, canonical JSON manifest,
//! and SHA256SUMS conventions as `run-release-artifact-gate.mjs`. It never
//! publishes; it only produces and verifies one target directory.
//!
//! Usage:
//!
//! ```bash
//! node scripts/build-device-client-release.mjs \
//!   --target aarch64-apple-darwin \
//!   --source-commit SOURCE_COMMIT \
//!   --source-date-epoch SOURCE_DATE_EPOCH \
//!   --output release-device-client-artifacts
//! ```
//!
//! `--dry-run` prints the canonical build/package plan without building or
//! writing anything. `CARGO_TARGET_DIR` selects the physical Cargo target
//! directory, exactly like `scripts/build-products.mjs`.

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, relative, resolve, sep } from 'node:path'

import {
  HELPER_RELEASE_MANIFEST_NAME,
  assertSourceCommit,
  assertSourceDateEpoch,
  canonicalJson,
  createReleaseBuildEnvironment,
  descriptorForFile,
  helperReleaseBuildBaseEnvironment,
  sourceIdentity,
  targetConfiguration,
} from './release-artifact-contract.mjs'
import { scanReleaseArtifactContent } from './verify-release-artifact-security.mjs'

const root = resolve(import.meta.dirname, '..')

export const DEVICE_CLIENT_RELEASE_SCHEMA_VERSION = 1
export const DEVICE_CLIENT_RELEASE_KIND = 'winwincode.device-client-release.v1'
export const DEVICE_CLIENT_RELEASE_PLAN_KIND = 'winwincode.device-client-release-plan.v1'
export const DEVICE_CLIENT_RELEASE_MANIFEST = 'device-client-release-manifest.json'
export const DEVICE_CLIENT_RELEASE_CHECKSUMS = 'SHA256SUMS'
export const DEVICE_CLIENT_SBOM_FORMAT = 'winwincode.cargo-lock-sbom.v1'

const EXECUTABLE_MODE = 0o755
const TEXT_MODE = 0o644
const LEGAL_FILES = Object.freeze(['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md'])
const COMMIT_PATTERN = /^[0-9a-f]{40}$/u
const SHA256_PATTERN = /^[0-9a-f]{64}$/u

/**
 * The shipped Device Client package binaries. The `wwc` CLI is the Device
 * Client user surface and statically embeds the `winwincode-device-client`
 * daemon library (see `deviceClientLibraryIdentity`); the supervisor spawns
 * the Worker binary located next to the current executable, and the Worker
 * carries the Kernel helper as its internal file.
 */
export const DEVICE_CLIENT_COMPONENTS = Object.freeze([
  Object.freeze({
    packageName: 'winwincode-cli',
    binaryName: 'wwc',
    role: 'device-client-cli',
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

export const DEVICE_CLIENT_RELEASE_CHECKS = Object.freeze([
  'cargo-locked-release-build',
  'credential-content-scan',
  'exact-package-file-set',
  'executable-mode-preserved',
])

export class DeviceClientReleaseError extends Error {
  constructor(code, message) {
    super(`${code}: ${message}`)
    this.name = 'DeviceClientReleaseError'
    this.code = code
  }
}

function fail(code, message) {
  throw new DeviceClientReleaseError(code, message)
}

function normalizedRelative(root, path) {
  return relative(root, path).split(sep).join('/')
}

export function deviceClientCargoArguments(target) {
  targetConfiguration(target)
  const arguments_ = ['build', '--locked', '--release', '--target', target]
  for (const component of DEVICE_CLIENT_COMPONENTS) {
    arguments_.push('-p', component.packageName, '--bin', component.binaryName)
  }
  return Object.freeze(arguments_)
}

export function parseDeviceClientReleaseArguments(argv) {
  const values = new Map()
  let dryRun = false
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--dry-run') {
      dryRun = true
      continue
    }
    if (!argument.startsWith('--')) throw new Error(`unexpected argument ${argument}`)
    const separator = argument.indexOf('=')
    if (separator !== -1) {
      values.set(argument.slice(2, separator), argument.slice(separator + 1))
      continue
    }
    const value = argv[index + 1]
    if (value === undefined || value.startsWith('--')) throw new Error(`${argument} requires a value`)
    values.set(argument.slice(2), value)
    index += 1
  }
  const allowed = new Set(['target', 'source-commit', 'source-date-epoch', 'output', 'helper-release-manifest'])
  for (const key of values.keys()) {
    if (!allowed.has(key)) throw new Error(`unknown argument --${key}`)
  }
  for (const key of ['target', 'source-commit', 'source-date-epoch', 'output']) {
    if (!values.has(key)) throw new Error(`--${key} is required`)
  }
  const sourceDateEpoch = Number(values.get('source-date-epoch'))
  if (!Number.isSafeInteger(sourceDateEpoch) || sourceDateEpoch <= 0) {
    throw new Error('--source-date-epoch must be a positive integer')
  }
  const helperReleaseManifest = values.get('helper-release-manifest') ?? null
  return Object.freeze({
    target: values.get('target'),
    sourceCommit: values.get('source-commit'),
    sourceDateEpoch,
    output: resolve(root, values.get('output')),
    helperReleaseManifest: helperReleaseManifest === null ? null : resolve(root, helperReleaseManifest),
    dryRun,
  })
}

/** Digest the `winwincode-device-client` crate sources the CLI embeds. */
export function deviceClientLibraryIdentity(root) {
  const crateRoot = resolve(root, 'crates/winwincode-device-client')
  const paths = [join(crateRoot, 'Cargo.toml')]
  for (const entry of readdirSync(join(crateRoot, 'src'), { withFileTypes: true })) {
    if (entry.isFile()) paths.push(join(crateRoot, 'src', entry.name))
  }
  return Object.freeze({
    mode: 'cli-embedded-daemon',
    cliBinary: 'wwc',
    libraryPackage: 'winwincode-device-client',
    sourceSha256: digestPaths(root, paths),
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

/**
 * Mechanical SBOM inventory derived from the pinned Cargo lock: one
 * `{name, version}` entry per `[[package]]` block, canonically sorted.
 */
export function cargoLockPackages(lockText) {
  const packages = []
  let current = null
  for (const rawLine of lockText.split('\n')) {
    const line = rawLine.trim()
    if (line === '[[package]]') {
      if (current !== null) packages.push(current)
      current = {}
      continue
    }
    if (current !== null) {
      const match = /^(name|version)\s=\s"(.*)"$/u.exec(line)
      if (match !== null) current[match[1]] = match[2]
    }
  }
  if (current !== null) packages.push(current)
  return Object.freeze(packages
    .filter(entry => typeof entry.name === 'string' && typeof entry.version === 'string')
    .map(entry => Object.freeze({ name: entry.name, version: entry.version }))
    .toSorted((left, right) => left.name.localeCompare(right.name) || left.version.localeCompare(right.version)))
}

export function deviceClientSbom(root) {
  const lockPath = resolve(root, 'Cargo.lock')
  if (!existsSync(lockPath)) fail('ARTIFACT_MISSING', 'missing Cargo.lock for the Device Client SBOM')
  const lock = descriptorForFile(root, lockPath)
  const packages = cargoLockPackages(readFileSync(lockPath, 'utf8'))
  if (packages.length === 0) fail('SBOM_INVALID', 'Cargo.lock contains no package inventory')
  return Object.freeze({
    format: DEVICE_CLIENT_SBOM_FORMAT,
    lock: Object.freeze({ path: 'Cargo.lock', ...lock }),
    packageCount: packages.length,
    packages: Object.freeze(packages),
  })
}

function componentDescriptor(artifactRoot, component) {
  const path = resolve(artifactRoot, 'bin', component.binaryName)
  if (!existsSync(path) || !statSync(path).isFile()) {
    fail('ARTIFACT_MISSING', `missing Device Client artifact ${component.binaryName}`)
  }
  const mode = statSync(path).mode & 0o777
  if (mode !== EXECUTABLE_MODE) {
    fail('ARTIFACT_MODE_INVALID', `${component.binaryName} must keep executable mode 0755, found ${mode.toString(8)}`)
  }
  return Object.freeze({
    ...component,
    path: `bin/${component.binaryName}`,
    ...descriptorForFile(artifactRoot, path),
    mode,
  })
}

function legalDescriptors(artifactRoot) {
  return LEGAL_FILES.map(name => {
    const path = resolve(artifactRoot, 'legal', name)
    if (!existsSync(path) || !statSync(path).isFile()) {
      fail('ARTIFACT_MISSING', `missing legal artifact ${name}`)
    }
    const mode = statSync(path).mode & 0o777
    if (mode !== TEXT_MODE) {
      fail('ARTIFACT_MODE_INVALID', `legal/${name} must have mode 0644, found ${mode.toString(8)}`)
    }
    return Object.freeze({ path: `legal/${name}`, ...descriptorForFile(artifactRoot, path), mode })
  })
}

function helperReleaseManifestDescriptor(root, artifactRoot, source) {
  const path = resolve(artifactRoot, 'bin', HELPER_RELEASE_MANIFEST_NAME)
  if (!existsSync(path) || !statSync(path).isFile()) return null
  const mode = statSync(path).mode & 0o777
  if (mode !== TEXT_MODE) {
    fail('HELPER_RELEASE_MANIFEST_INVALID', `${HELPER_RELEASE_MANIFEST_NAME} must have mode 0644`)
  }
  const manifest = JSON.parse(readFileSync(path, 'utf8'))
  const helper = descriptorForFile(artifactRoot, resolve(artifactRoot, 'bin', 'winwincode-kernel-helper'))
  const helperSourceSha256 = `sha256:${createHash('sha256')
    .update(readFileSync(resolve(root, 'crates', 'helper', 'src', 'main.rs')))
    .digest('hex')}`
  if (manifest.schemaVersion !== 1
    || manifest.protocol !== 'winwincode-kernel-helper-release'
    || manifest.version !== 1
    || manifest.packageVersion !== source.version
    || manifest.sourceSha256 !== helperSourceSha256
    || manifest.binaryPath !== 'winwincode-kernel-helper'
    || manifest.binaryMode !== EXECUTABLE_MODE
    || manifest.binarySha256 !== `sha256:${helper.sha256}`
    || typeof manifest.signature !== 'string'
    || !/^[A-Za-z0-9_-]{86}$/u.test(manifest.signature)) {
    fail('HELPER_RELEASE_MANIFEST_INVALID', `staged ${HELPER_RELEASE_MANIFEST_NAME} does not bind the packaged helper`)
  }
  return Object.freeze({
    path: `bin/${HELPER_RELEASE_MANIFEST_NAME}`,
    ...descriptorForFile(artifactRoot, path),
    mode,
  })
}

function checksumLines(manifest) {
  const descriptors = [
    ...manifest.components,
    ...manifest.legal,
    ...(manifest.helperReleaseManifest === null ? [] : [manifest.helperReleaseManifest]),
  ]
  return descriptors
    .map(entry => `${entry.sha256}  ${entry.path}`)
    .toSorted((left, right) => left.localeCompare(right))
}

export function deviceClientChecksums(manifest) {
  return `${checksumLines(manifest).join('\n')}\n`
}

export function createDeviceClientReleaseManifest({
  root,
  artifactRoot,
  target,
  sourceCommit,
  sourceDateEpoch,
}) {
  const targetIdentity = targetConfiguration(target)
  const source = sourceIdentity(root, sourceCommit, sourceDateEpoch)
  const components = DEVICE_CLIENT_COMPONENTS.map(component => componentDescriptor(artifactRoot, component))
  const legal = legalDescriptors(artifactRoot)
  const helperReleaseManifest = helperReleaseManifestDescriptor(root, artifactRoot, source)
  return Object.freeze({
    schemaVersion: DEVICE_CLIENT_RELEASE_SCHEMA_VERSION,
    kind: DEVICE_CLIENT_RELEASE_KIND,
    target: targetIdentity,
    source,
    deviceClient: deviceClientLibraryIdentity(root),
    components: Object.freeze(components),
    helperReleaseManifest,
    sbom: deviceClientSbom(root),
    legal: Object.freeze(legal),
    checks: DEVICE_CLIENT_RELEASE_CHECKS,
  })
}

function assertDescriptor(artifactRoot, descriptor, label) {
  if (descriptor === null || typeof descriptor !== 'object'
    || typeof descriptor.path !== 'string'
    || descriptor.path.startsWith('/')
    || descriptor.path.split('/').includes('..')
    || !SHA256_PATTERN.test(descriptor.sha256)
    || !Number.isSafeInteger(descriptor.bytes)
    || descriptor.bytes < 0
    || !Number.isSafeInteger(descriptor.mode)) {
    fail('MANIFEST_INVALID', `${label} descriptor is invalid`)
  }
  const path = resolve(artifactRoot, descriptor.path)
  if (!path.startsWith(`${resolve(artifactRoot)}${sep}`) || !existsSync(path) || !statSync(path).isFile()) {
    fail('ARTIFACT_MISSING', `${label} artifact ${descriptor.path} is missing`)
  }
  const actual = descriptorForFile(artifactRoot, path)
  const actualMode = statSync(path).mode & 0o777
  if (actual.bytes !== descriptor.bytes || actual.sha256 !== descriptor.sha256) {
    fail('ARTIFACT_MISMATCH', `${label} artifact ${descriptor.path} changed`)
  }
  if (actualMode !== descriptor.mode) {
    fail('ARTIFACT_MODE_INVALID', `${label} artifact ${descriptor.path} changed mode`)
  }
}

function assertExactIdentity(actual, expected, code, message) {
  if (canonicalJson(actual) !== canonicalJson(expected)) fail(code, message)
}

function contentScanFindings(artifactRoot, target, path, binary) {
  const report = scanReleaseArtifactContent({
    bytes: readFileSync(path),
    target,
    path: normalizedRelative(artifactRoot, path),
    binary,
  })
  if (report.length > 0) {
    fail('CREDENTIAL_FINDING', report.map(entry => `${entry.rule}@${entry.path}`).join('; '))
  }
}

export function verifyDeviceClientReleaseDirectory({ root, artifactRoot, expectedTarget }) {
  const targetIdentity = targetConfiguration(expectedTarget)
  const manifestPath = resolve(artifactRoot, DEVICE_CLIENT_RELEASE_MANIFEST)
  if (!existsSync(manifestPath) || !statSync(manifestPath).isFile()) {
    fail('ARTIFACT_MISSING', `missing ${DEVICE_CLIENT_RELEASE_MANIFEST} for ${expectedTarget}`)
  }
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'))
  if (manifest.schemaVersion !== DEVICE_CLIENT_RELEASE_SCHEMA_VERSION
    || manifest.kind !== DEVICE_CLIENT_RELEASE_KIND) {
    fail('MANIFEST_INVALID', 'Device Client release manifest identity is invalid')
  }
  assertExactIdentity(manifest.target, targetIdentity, 'TARGET_MISMATCH', 'target does not match')
  assertExactIdentity(manifest.checks, DEVICE_CLIENT_RELEASE_CHECKS, 'MANIFEST_INVALID', 'release check set is invalid')
  assertSourceCommit(manifest.source?.commit)
  assertSourceDateEpoch(manifest.source?.sourceDateEpoch)
  assertExactIdentity(
    manifest.source,
    sourceIdentity(root, manifest.source.commit, manifest.source.sourceDateEpoch),
    'SOURCE_MISMATCH',
    'source manifests, locks, version or digest changed',
  )
  assertExactIdentity(
    manifest.deviceClient,
    deviceClientLibraryIdentity(root),
    'DEVICE_CLIENT_MISMATCH',
    'Device Client library identity changed',
  )
  assertExactIdentity(
    manifest.sbom,
    deviceClientSbom(root),
    'SBOM_MISMATCH',
    'SBOM does not match the pinned Cargo lock',
  )
  assertExactIdentity(
    manifest.components.map(({ packageName, binaryName, role, distribution, path, mode }) => ({
      packageName,
      binaryName,
      role,
      distribution,
      path,
      mode,
    })),
    DEVICE_CLIENT_COMPONENTS.map(component => ({
      ...component,
      path: `bin/${component.binaryName}`,
      mode: EXECUTABLE_MODE,
    })),
    'MANIFEST_INVALID',
    'Device Client component identities are invalid',
  )
  for (const descriptor of manifest.components) {
    assertDescriptor(artifactRoot, descriptor, 'Device Client')
  }
  if (manifest.helperReleaseManifest !== null) {
    assertDescriptor(artifactRoot, manifest.helperReleaseManifest, 'helper release manifest')
    assertExactIdentity(
      manifest.helperReleaseManifest,
      helperReleaseManifestDescriptor(root, artifactRoot, manifest.source),
      'HELPER_RELEASE_MANIFEST_INVALID',
      'helper release manifest descriptor is invalid',
    )
  }
  assertExactIdentity(
    manifest.legal.map(({ path }) => path),
    LEGAL_FILES.map(name => `legal/${name}`),
    'MANIFEST_INVALID',
    'legal artifact set is invalid',
  )
  for (const descriptor of manifest.legal) {
    assertDescriptor(artifactRoot, descriptor, 'legal')
    const name = descriptor.path.slice('legal/'.length)
    if (readFileSync(resolve(artifactRoot, descriptor.path)).compare(readFileSync(resolve(root, name))) !== 0) {
      fail('LEGAL_BOUNDARY_FAILED', `${descriptor.path} does not match the project legal file`)
    }
  }
  const checksumPath = resolve(artifactRoot, DEVICE_CLIENT_RELEASE_CHECKSUMS)
  if (!existsSync(checksumPath)
    || readFileSync(checksumPath, 'utf8') !== deviceClientChecksums(manifest)) {
    fail('CHECKSUM_MISMATCH', 'SHA256SUMS does not match the Device Client release manifest')
  }
  const expectedPaths = [
    DEVICE_CLIENT_RELEASE_MANIFEST,
    DEVICE_CLIENT_RELEASE_CHECKSUMS,
    ...manifest.components.map(entry => entry.path),
    ...manifest.legal.map(entry => entry.path),
    ...(manifest.helperReleaseManifest === null ? [] : [manifest.helperReleaseManifest.path]),
  ].toSorted((left, right) => left.localeCompare(right))
  const actualPaths = packageFilesBelow(artifactRoot)
    .toSorted((left, right) => left.localeCompare(right))
  assertExactIdentity(actualPaths, expectedPaths, 'ARTIFACT_SET_MISMATCH', 'package contains an unlisted file')
  for (const relativePath of actualPaths) {
    // Legal files are byte-bound to the project legal files above; NOTICE
    // legitimately carries third-party attribution text the legacy rules
    // match, so like `createReleaseArtifactSecurityReport` they are not
    // content-scanned.
    if (relativePath.startsWith('legal/')) continue
    const binary = relativePath.startsWith('bin/') && !relativePath.endsWith('.json')
    contentScanFindings(artifactRoot, targetIdentity.target, resolve(artifactRoot, relativePath), binary)
  }
  return Object.freeze(manifest)
}

function packageFilesBelow(rootDirectory) {
  const files = []
  const visit = directory => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name)
      if (entry.isDirectory()) visit(path)
      else if (entry.isFile()) files.push(normalizedRelative(rootDirectory, path))
      else fail('ARTIFACT_TYPE_INVALID', `${path} is not a regular file`)
    }
  }
  visit(rootDirectory)
  return files
}

/**
 * Pure dry-run plan: everything the build would do, computed without
 * building or writing anything.
 */
export function buildDeviceClientReleasePlan({
  root,
  target,
  sourceCommit,
  sourceDateEpoch,
  output,
  cargoTargetDirectory,
}) {
  const targetIdentity = targetConfiguration(target)
  assertSourceCommit(sourceCommit)
  assertSourceDateEpoch(sourceDateEpoch)
  const environment = createReleaseBuildEnvironment({
    baseEnvironment: helperReleaseBuildBaseEnvironment(process.env),
    root,
    buildRoot: '<build-root>',
    targetDirectory: cargoTargetDirectory,
    targetDirectories: [cargoTargetDirectory],
    target,
    runnerTemp: process.env.RUNNER_TEMP ?? '',
    home: process.env.HOME ?? '',
    sourceDateEpoch,
  })
  const source = sourceIdentity(root, sourceCommit, sourceDateEpoch)
  const sbom = deviceClientSbom(root)
  return Object.freeze({
    schemaVersion: DEVICE_CLIENT_RELEASE_SCHEMA_VERSION,
    kind: DEVICE_CLIENT_RELEASE_PLAN_KIND,
    mode: 'dry-run',
    target: targetIdentity,
    sourceCommit,
    sourceDateEpoch,
    version: source.version,
    output: normalizedRelative(root, output).startsWith('..') ? output : normalizedRelative(root, output),
    cargo: Object.freeze({
      program: 'cargo',
      arguments: deviceClientCargoArguments(target),
      targetDirectory: cargoTargetDirectory,
    }),
    environment: Object.freeze({
      CARGO_INCREMENTAL: environment.CARGO_INCREMENTAL,
      CARGO_TARGET_DIR: environment.CARGO_TARGET_DIR,
      CI: environment.CI,
      SOURCE_DATE_EPOCH: environment.SOURCE_DATE_EPOCH,
      RUSTFLAGS: environment.RUSTFLAGS,
    }),
    package: Object.freeze({
      manifest: DEVICE_CLIENT_RELEASE_MANIFEST,
      checksums: DEVICE_CLIENT_RELEASE_CHECKSUMS,
      files: Object.freeze([
        { path: DEVICE_CLIENT_RELEASE_MANIFEST, mode: TEXT_MODE },
        { path: DEVICE_CLIENT_RELEASE_CHECKSUMS, mode: TEXT_MODE },
        ...DEVICE_CLIENT_COMPONENTS.map(component => ({
          path: `bin/${component.binaryName}`,
          mode: EXECUTABLE_MODE,
        })),
        ...LEGAL_FILES.map(name => ({ path: `legal/${name}`, mode: TEXT_MODE })),
      ]),
    }),
    components: DEVICE_CLIENT_COMPONENTS,
    deviceClient: deviceClientLibraryIdentity(root),
    sbom: Object.freeze({
      format: sbom.format,
      lockSha256: sbom.lock.sha256,
      packageCount: sbom.packageCount,
    }),
    checks: DEVICE_CLIENT_RELEASE_CHECKS,
  })
}

function run(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    cwd: root,
    env: options.env ?? helperReleaseBuildBaseEnvironment(process.env),
    encoding: 'utf8',
    stdio: options.capture === true ? 'pipe' : 'inherit',
    maxBuffer: 64 * 1_024 * 1_024,
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0 || result.signal !== null) {
    if (options.capture === true) {
      process.stderr.write(result.stdout)
      process.stderr.write(result.stderr)
    }
    throw new Error(`${command} ${arguments_.join(' ')} failed with ${result.signal ?? `exit code ${result.status}`}`)
  }
  return result.stdout
}

/**
 * Release builds bind the exact candidate: HEAD must be the requested
 * commit, `SOURCE_DATE_EPOCH` its commit time, and the checkout clean.
 */
function assertReleaseSource(sourceCommit, sourceDateEpoch) {
  assertSourceCommit(sourceCommit)
  assertSourceDateEpoch(sourceDateEpoch)
  const head = run('git', ['rev-parse', 'HEAD'], { capture: true }).trim()
  if (head !== sourceCommit) {
    fail('SOURCE_COMMIT_INVALID', `source commit ${sourceCommit} does not match HEAD ${head}`)
  }
  const commitEpoch = Number(run('git', ['show', '-s', '--format=%ct', sourceCommit], { capture: true }))
  if (commitEpoch !== sourceDateEpoch) {
    fail('SOURCE_DATE_EPOCH_INVALID', `SOURCE_DATE_EPOCH ${String(sourceDateEpoch)} does not match commit time ${String(commitEpoch)}`)
  }
  const status = run('git', ['status', '--porcelain=v1', '--untracked-files=all'], { capture: true })
  if (status.length > 0) fail('SOURCE_NOT_CLEAN', 'Device Client release build requires a clean checkout')
}

function stagePackage({ artifactRoot, targetDirectory, target, helperReleaseManifest }) {
  const targetIdentity = targetConfiguration(target)
  rmSync(artifactRoot, { recursive: true, force: true })
  mkdirSync(resolve(artifactRoot, 'bin'), { recursive: true })
  mkdirSync(resolve(artifactRoot, 'legal'), { recursive: true })
  for (const component of DEVICE_CLIENT_COMPONENTS) {
    const source = resolve(targetDirectory, target, 'release', component.binaryName)
    if (!existsSync(source) || !statSync(source).isFile()) {
      fail('ARTIFACT_MISSING', `cargo did not produce ${component.binaryName} for ${target}`)
    }
    const destination = resolve(artifactRoot, 'bin', component.binaryName)
    copyFileSync(source, destination)
    chmodSync(destination, EXECUTABLE_MODE)
    if (targetIdentity.os === 'macos') {
      run('codesign', ['--force', '--sign', '-', destination])
    }
  }
  if (helperReleaseManifest !== null) {
    const destination = resolve(artifactRoot, 'bin', HELPER_RELEASE_MANIFEST_NAME)
    copyFileSync(helperReleaseManifest, destination)
    chmodSync(destination, TEXT_MODE)
  }
  for (const name of LEGAL_FILES) {
    const destination = resolve(artifactRoot, 'legal', name)
    copyFileSync(resolve(root, name), destination)
    chmodSync(destination, TEXT_MODE)
  }
}

function cargoTargetDirectoryOverride(buildRoot) {
  const configured = process.env.CARGO_TARGET_DIR
  if (configured === undefined || configured.length === 0) return resolve(buildRoot, 'cargo-target')
  return resolve(root, configured)
}

if (process.argv[1] !== undefined && resolve(process.argv[1]) === resolve(import.meta.filename)) {
  main(process.argv.slice(2))
}

function main(argv) {
  const options = parseDeviceClientReleaseArguments(argv)
  targetConfiguration(options.target)

  if (options.dryRun) {
    const buildRootPreview = '<build-root>'
    const plan = buildDeviceClientReleasePlan({
      root,
      target: options.target,
      sourceCommit: options.sourceCommit,
      sourceDateEpoch: options.sourceDateEpoch,
      output: options.output,
      cargoTargetDirectory: cargoTargetDirectoryOverride(buildRootPreview),
    })
    process.stdout.write(canonicalJson(plan))
    return
  }

  assertReleaseSource(options.sourceCommit, options.sourceDateEpoch)
  const buildBase = process.env.WWC_RELEASE_BUILD_ROOT ?? tmpdir()
  const buildRoot = mkdtempSync(resolve(buildBase, 'winwincode-device-client-release-'))
  const artifactRoot = resolve(options.output, options.target)
  try {
    const targetDirectory = cargoTargetDirectoryOverride(buildRoot)
    const environment = createReleaseBuildEnvironment({
      baseEnvironment: helperReleaseBuildBaseEnvironment(process.env),
      root,
      buildRoot,
      targetDirectory,
      targetDirectories: [targetDirectory],
      target: options.target,
      runnerTemp: process.env.RUNNER_TEMP ?? '',
      home: process.env.HOME ?? '',
      sourceDateEpoch: options.sourceDateEpoch,
    })
    run('cargo', [...deviceClientCargoArguments(options.target)], { env: environment })
    stagePackage({
      artifactRoot,
      targetDirectory,
      target: options.target,
      helperReleaseManifest: options.helperReleaseManifest,
    })
    const manifest = createDeviceClientReleaseManifest({
      root,
      artifactRoot,
      target: options.target,
      sourceCommit: options.sourceCommit,
      sourceDateEpoch: options.sourceDateEpoch,
    })
    writeFileSync(resolve(artifactRoot, DEVICE_CLIENT_RELEASE_MANIFEST), canonicalJson(manifest))
    chmodSync(resolve(artifactRoot, DEVICE_CLIENT_RELEASE_MANIFEST), TEXT_MODE)
    writeFileSync(resolve(artifactRoot, DEVICE_CLIENT_RELEASE_CHECKSUMS), deviceClientChecksums(manifest))
    chmodSync(resolve(artifactRoot, DEVICE_CLIENT_RELEASE_CHECKSUMS), TEXT_MODE)
    verifyDeviceClientReleaseDirectory({
      root,
      artifactRoot,
      expectedTarget: options.target,
    })
    process.stdout.write(canonicalJson({
      status: 'passed',
      target: options.target,
      sourceCommit: options.sourceCommit,
      sourceDateEpoch: options.sourceDateEpoch,
      artifactRoot,
      manifest: DEVICE_CLIENT_RELEASE_MANIFEST,
      checksums: DEVICE_CLIENT_RELEASE_CHECKSUMS,
    }))
  } finally {
    rmSync(buildRoot, { recursive: true, force: true })
  }
}
