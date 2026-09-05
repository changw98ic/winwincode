// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  DEVICE_CLIENT_COMPONENTS,
  DEVICE_CLIENT_RELEASE_CHECKSUMS,
  DEVICE_CLIENT_RELEASE_CHECKS,
  DEVICE_CLIENT_RELEASE_KIND,
  DEVICE_CLIENT_RELEASE_MANIFEST,
  DEVICE_CLIENT_SBOM_FORMAT,
  DeviceClientReleaseError,
  buildDeviceClientReleasePlan,
  cargoLockPackages,
  createDeviceClientReleaseManifest,
  deviceClientCargoArguments,
  deviceClientChecksums,
  deviceClientLibraryIdentity,
  deviceClientSbom,
  parseDeviceClientReleaseArguments,
  verifyDeviceClientReleaseDirectory,
} from '../scripts/build-device-client-release.mjs'
import {
  RELEASE_TARGETS,
  canonicalJson,
  targetConfiguration,
} from '../scripts/release-artifact-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const sourceCommit = '1234567890abcdef1234567890abcdef12345678'
const sourceDateEpoch = 1_700_000_000

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function makePackageFixture({ options = {} } = {}) {
  const artifactRoot = mkdtempSync(join(tmpdir(), 'winwincode-device-client-package-'))
  mkdirSync(join(artifactRoot, 'bin'), { recursive: true })
  mkdirSync(join(artifactRoot, 'legal'), { recursive: true })
  const binaryContents = new Map([
    ['wwc', Buffer.from('fixture wwc device-client cli\n')],
    ['winwincode-worker', Buffer.from('fixture winwincode-worker\n')],
    ['winwincode-kernel-helper', Buffer.from('fixture winwincode-kernel-helper\n')],
  ])
  for (const [binaryName, bytes] of binaryContents) {
    const path = join(artifactRoot, 'bin', binaryName)
    writeFileSync(path, options.binaryBytes?.[binaryName] ?? bytes)
    chmodSync(path, 0o755)
  }
  for (const name of ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']) {
    const destination = join(artifactRoot, 'legal', name)
    copyFileSync(join(root, name), destination)
    chmodSync(destination, 0o644)
  }
  return artifactRoot
}

function writeManifestAndChecksums(artifactRoot) {
  const manifest = createDeviceClientReleaseManifest({
    root,
    artifactRoot,
    target: 'aarch64-apple-darwin',
    sourceCommit,
    sourceDateEpoch,
  })
  writeFileSync(join(artifactRoot, DEVICE_CLIENT_RELEASE_MANIFEST), canonicalJson(manifest))
  writeFileSync(join(artifactRoot, DEVICE_CLIENT_RELEASE_CHECKSUMS), deviceClientChecksums(manifest))
  return manifest
}

function assertDeviceClientReleaseError(operation, code) {
  assert.throws(operation, error => {
    assert.ok(error instanceof DeviceClientReleaseError)
    assert.equal(error.code, code)
    return true
  })
}

test('device client components cover the CLI, Worker and Kernel helper', () => {
  assert.deepEqual(
    DEVICE_CLIENT_COMPONENTS.map(({ packageName, binaryName, role, distribution }) => ({
      packageName,
      binaryName,
      role,
      distribution,
    })),
    [
      {
        packageName: 'winwincode-cli',
        binaryName: 'wwc',
        role: 'device-client-cli',
        distribution: 'product',
      },
      {
        packageName: 'winwincode-worker',
        binaryName: 'winwincode-worker',
        role: 'worker',
        distribution: 'product',
      },
      {
        packageName: 'winwincode-kernel-helper',
        binaryName: 'winwincode-kernel-helper',
        role: 'worker-helper',
        distribution: 'worker-internal',
      },
    ],
  )
})

test('cargo arguments build every component locked and released for each supported target', () => {
  for (const { target } of RELEASE_TARGETS) {
    const arguments_ = deviceClientCargoArguments(target)
    assert.ok(arguments_.includes('--locked'))
    assert.ok(arguments_.includes('--release'))
    const targetFlag = arguments_.indexOf('--target')
    assert.notEqual(targetFlag, -1)
    assert.equal(arguments_[targetFlag + 1], target)
    for (const component of DEVICE_CLIENT_COMPONENTS) {
      const packageFlag = arguments_.indexOf(component.packageName)
      assert.notEqual(packageFlag, -1)
      assert.equal(arguments_[packageFlag - 1], '-p')
      assert.equal(arguments_[packageFlag + 1], '--bin')
      assert.equal(arguments_[packageFlag + 2], component.binaryName)
    }
  }
  assert.throws(
    () => deviceClientCargoArguments('x86_64-pc-windows-msvc'),
    /unsupported release target/u,
  )
})

test('argument parser accepts the documented release invocation', () => {
  const parsed = parseDeviceClientReleaseArguments([
    '--target', 'x86_64-unknown-linux-gnu',
    '--source-commit', sourceCommit,
    '--source-date-epoch', String(sourceDateEpoch),
    '--output', 'release-device-client-artifacts',
    '--dry-run',
  ])
  assert.equal(parsed.target, 'x86_64-unknown-linux-gnu')
  assert.equal(parsed.sourceCommit, sourceCommit)
  assert.equal(parsed.sourceDateEpoch, sourceDateEpoch)
  assert.equal(parsed.dryRun, true)
  assert.equal(parsed.helperReleaseManifest, null)
  const equalsForm = parseDeviceClientReleaseArguments([
    `--target=x86_64-apple-darwin`,
    `--source-commit=${sourceCommit}`,
    `--source-date-epoch=${sourceDateEpoch}`,
    '--output=release-device-client-artifacts',
  ])
  assert.equal(equalsForm.target, 'x86_64-apple-darwin')
  assert.equal(equalsForm.dryRun, false)
  assert.throws(() => parseDeviceClientReleaseArguments([
    '--target', 'x86_64-apple-darwin',
    '--source-commit', sourceCommit,
  ]), /--source-date-epoch is required/u)
  assert.throws(() => parseDeviceClientReleaseArguments([
    '--target', 'x86_64-apple-darwin',
    '--source-commit', sourceCommit,
    '--source-date-epoch', String(sourceDateEpoch),
    '--output', 'out',
    '--unexpected',
  ]), /--unexpected requires a value/u)
  assert.throws(() => parseDeviceClientReleaseArguments([
    '--target', 'x86_64-apple-darwin',
    '--source-commit', sourceCommit,
    '--source-date-epoch', 'not-a-number',
    '--output', 'out',
  ]), /--source-date-epoch must be a positive integer/u)
})

test('cargo lock SBOM is a canonically sorted package inventory', () => {
  const lockText = [
    'version = 4',
    '',
    '[[package]]',
    'name = "zebra"',
    'version = "9.9.9"',
    'source = "registry+https://github.com/rust-lang/crates.io-index"',
    '',
    '[[package]]',
    'name = "alpha"',
    'version = "0.1.0"',
    'dependencies = ["zebra"]',
    '',
  ].join('\n')
  assert.deepEqual(cargoLockPackages(lockText), [
    { name: 'alpha', version: '0.1.0' },
    { name: 'zebra', version: '9.9.9' },
  ])
  const sbom = deviceClientSbom(root)
  assert.equal(sbom.format, DEVICE_CLIENT_SBOM_FORMAT)
  assert.equal(sbom.lock.path, 'Cargo.lock')
  assert.equal(sbom.lock.sha256, sha256(readFileSync(join(root, 'Cargo.lock'))))
  assert.equal(sbom.packageCount, cargoLockPackages(readFileSync(join(root, 'Cargo.lock'), 'utf8')).length)
  assert.ok(sbom.packageCount > 100)
  assert.deepEqual(
    sbom.packages,
    [...sbom.packages].toSorted((left, right) => left.name.localeCompare(right.name)
      || left.version.localeCompare(right.version)),
  )
})

test('device client library identity is stable and pins the embedded daemon crate', () => {
  const identity = deviceClientLibraryIdentity(root)
  assert.equal(identity.mode, 'cli-embedded-daemon')
  assert.equal(identity.cliBinary, 'wwc')
  assert.equal(identity.libraryPackage, 'winwincode-device-client')
  assert.match(identity.sourceSha256, /^[0-9a-f]{64}$/u)
  assert.equal(identity.sourceSha256, deviceClientLibraryIdentity(root).sourceSha256)
})

test('staged package manifest and checksums verify end to end', () => {
  const artifactRoot = makePackageFixture()
  const manifest = writeManifestAndChecksums(artifactRoot)
  try {
    assert.equal(manifest.schemaVersion, 1)
    assert.equal(manifest.kind, DEVICE_CLIENT_RELEASE_KIND)
    assert.deepEqual(manifest.target, targetConfiguration('aarch64-apple-darwin'))
    assert.equal(manifest.source.commit, sourceCommit)
    assert.equal(manifest.source.sourceDateEpoch, sourceDateEpoch)
    assert.equal(manifest.components.length, DEVICE_CLIENT_COMPONENTS.length)
    for (const descriptor of manifest.components) {
      assert.equal(descriptor.mode, 0o755)
      assert.equal(descriptor.bytes, readFileSync(join(artifactRoot, descriptor.path)).length)
      assert.equal(descriptor.sha256, sha256(readFileSync(join(artifactRoot, descriptor.path))))
    }
    assert.equal(manifest.helperReleaseManifest, null)
    assert.deepEqual(manifest.checks, DEVICE_CLIENT_RELEASE_CHECKS)
    assert.deepEqual(manifest.sbom, deviceClientSbom(root))
    const verified = verifyDeviceClientReleaseDirectory({
      root,
      artifactRoot,
      expectedTarget: 'aarch64-apple-darwin',
    })
    assert.equal(verified.kind, DEVICE_CLIENT_RELEASE_KIND)
    const checksumLines = readFileSync(join(artifactRoot, DEVICE_CLIENT_RELEASE_CHECKSUMS), 'utf8')
      .trim()
      .split('\n')
    assert.equal(checksumLines.length, manifest.components.length + manifest.legal.length)
    for (const line of checksumLines) {
      assert.match(line, /^[0-9a-f]{64}  (bin|legal)\//u)
    }
  } finally {
    rmSync(artifactRoot, { recursive: true, force: true })
  }
})

test('verification fails when the executable bit is dropped', () => {
  const artifactRoot = makePackageFixture()
  writeManifestAndChecksums(artifactRoot)
  try {
    chmodSync(join(artifactRoot, 'bin', 'wwc'), 0o644)
    assertDeviceClientReleaseError(
      () => verifyDeviceClientReleaseDirectory({
        root,
        artifactRoot,
        expectedTarget: 'aarch64-apple-darwin',
      }),
      'ARTIFACT_MODE_INVALID',
    )
  } finally {
    rmSync(artifactRoot, { recursive: true, force: true })
  }
})

test('verification fails when a packaged binary changes', () => {
  const artifactRoot = makePackageFixture()
  writeManifestAndChecksums(artifactRoot)
  try {
    writeFileSync(join(artifactRoot, 'bin', 'winwincode-worker'), 'tampered\n')
    chmodSync(join(artifactRoot, 'bin', 'winwincode-worker'), 0o755)
    assertDeviceClientReleaseError(
      () => verifyDeviceClientReleaseDirectory({
        root,
        artifactRoot,
        expectedTarget: 'aarch64-apple-darwin',
      }),
      'ARTIFACT_MISMATCH',
    )
  } finally {
    rmSync(artifactRoot, { recursive: true, force: true })
  }
})

test('verification rejects development source files inside the package', () => {
  const artifactRoot = makePackageFixture()
  writeManifestAndChecksums(artifactRoot)
  try {
    mkdirSync(join(artifactRoot, 'src'), { recursive: true })
    writeFileSync(join(artifactRoot, 'src', 'main.rs'), 'fn main() {}\n')
    assertDeviceClientReleaseError(
      () => verifyDeviceClientReleaseDirectory({
        root,
        artifactRoot,
        expectedTarget: 'aarch64-apple-darwin',
      }),
      'ARTIFACT_SET_MISMATCH',
    )
  } finally {
    rmSync(artifactRoot, { recursive: true, force: true })
  }
})

test('verification rejects credential material inside the package', () => {
  const artifactRoot = makePackageFixture()
  const manifest = writeManifestAndChecksums(artifactRoot)
  try {
    const poisoned = { ...manifest, comment: '-----BEGIN RSA PRIVATE KEY-----' }
    writeFileSync(join(artifactRoot, DEVICE_CLIENT_RELEASE_MANIFEST), canonicalJson(poisoned))
    assertDeviceClientReleaseError(
      () => verifyDeviceClientReleaseDirectory({
        root,
        artifactRoot,
        expectedTarget: 'aarch64-apple-darwin',
      }),
      'CREDENTIAL_FINDING',
    )
  } finally {
    rmSync(artifactRoot, { recursive: true, force: true })
  }
})

test('verification rejects a checksum file that does not match the manifest', () => {
  const artifactRoot = makePackageFixture()
  writeManifestAndChecksums(artifactRoot)
  try {
    writeFileSync(join(artifactRoot, DEVICE_CLIENT_RELEASE_CHECKSUMS), '0'.repeat(64) + '  bin/wwc\n')
    assertDeviceClientReleaseError(
      () => verifyDeviceClientReleaseDirectory({
        root,
        artifactRoot,
        expectedTarget: 'aarch64-apple-darwin',
      }),
      'CHECKSUM_MISMATCH',
    )
  } finally {
    rmSync(artifactRoot, { recursive: true, force: true })
  }
})

test('verification rejects a target mismatch', () => {
  const artifactRoot = makePackageFixture()
  writeManifestAndChecksums(artifactRoot)
  try {
    assertDeviceClientReleaseError(
      () => verifyDeviceClientReleaseDirectory({
        root,
        artifactRoot,
        expectedTarget: 'x86_64-unknown-linux-gnu',
      }),
      'TARGET_MISMATCH',
    )
  } finally {
    rmSync(artifactRoot, { recursive: true, force: true })
  }
})

test('optional helper release manifest must bind the packaged helper', () => {
  const artifactRoot = makePackageFixture()
  const helperReleaseManifest = {
    schemaVersion: 1,
    protocol: 'winwincode-kernel-helper-release',
    version: 1,
    packageVersion: JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')).version,
    sourceSha256: `sha256:${sha256(readFileSync(join(root, 'crates', 'helper', 'src', 'main.rs')))}`,
    binarySha256: `sha256:${sha256(readFileSync(join(artifactRoot, 'bin', 'winwincode-kernel-helper')))}`,
    binaryPath: 'winwincode-kernel-helper',
    binaryMode: 0o755,
    signature: 'A'.repeat(86),
  }
  const destination = join(artifactRoot, 'bin', 'winwincode-kernel-helper.release.json')
  writeFileSync(destination, JSON.stringify(helperReleaseManifest))
  chmodSync(destination, 0o644)
  const manifest = writeManifestAndChecksums(artifactRoot)
  try {
    assert.notEqual(manifest.helperReleaseManifest, null)
    assert.equal(manifest.helperReleaseManifest.mode, 0o644)
    verifyDeviceClientReleaseDirectory({
      root,
      artifactRoot,
      expectedTarget: 'aarch64-apple-darwin',
    })
    const tampered = { ...helperReleaseManifest, binarySha256: `sha256:${'0'.repeat(64)}` }
    writeFileSync(destination, JSON.stringify(tampered))
    // Binding a tampered helper manifest into a package manifest must fail
    // closed; a stale package manifest instead reports the byte mismatch
    // already covered by the binary-tamper test above.
    assertDeviceClientReleaseError(
      () => writeManifestAndChecksums(artifactRoot),
      'HELPER_RELEASE_MANIFEST_INVALID',
    )
  } finally {
    rmSync(artifactRoot, { recursive: true, force: true })
  }
})

test('dry-run plans cover all four release targets without building', () => {
  for (const { target } of RELEASE_TARGETS) {
    const plan = buildDeviceClientReleasePlan({
      root,
      target,
      sourceCommit,
      sourceDateEpoch,
      output: join(root, 'release-device-client-artifacts'),
      cargoTargetDirectory: join(root, 'target'),
    })
    assert.equal(plan.kind, 'winwincode.device-client-release-plan.v1')
    assert.equal(plan.mode, 'dry-run')
    assert.deepEqual(plan.target, targetConfiguration(target))
    assert.ok(plan.cargo.arguments.includes('--target'))
    assert.ok(plan.cargo.arguments.includes(target))
    assert.ok(plan.cargo.arguments.includes('--locked'))
    assert.ok(plan.cargo.arguments.includes('--release'))
    assert.equal(plan.environment.SOURCE_DATE_EPOCH, String(sourceDateEpoch))
    assert.equal(plan.environment.CARGO_TARGET_DIR, join(root, 'target'))
    assert.equal(plan.environment.CARGO_INCREMENTAL, '0')
    assert.deepEqual(
      plan.package.files.filter(entry => entry.path.startsWith('bin/')),
      DEVICE_CLIENT_COMPONENTS.map(component => ({
        path: `bin/${component.binaryName}`,
        mode: 0o755,
      })),
    )
    assert.equal(plan.package.manifest, DEVICE_CLIENT_RELEASE_MANIFEST)
    assert.equal(plan.package.checksums, DEVICE_CLIENT_RELEASE_CHECKSUMS)
    assert.deepEqual(plan.checks, DEVICE_CLIENT_RELEASE_CHECKS)
    assert.ok(!existsSync(join(root, 'release-device-client-artifacts')))
  }
})
