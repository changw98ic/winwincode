import assert from 'node:assert/strict'
import {
  createHash,
  createPrivateKey,
  createPublicKey,
  sign,
} from 'node:crypto'
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
  RELEASE_ARTIFACT_MANIFEST,
  RELEASE_CHECKSUMS,
  HELPER_RELEASE_MANIFEST_NAME,
  RELEASE_REPORT_KIND,
  RELEASE_TARGETS,
  RUST_RELEASE_ARTIFACTS,
  ReleaseArtifactError,
  canonicalJson,
  createReleaseBuildEnvironment,
  createReleaseArtifactManifest,
  createReleaseReport,
  descriptorForFile,
  helperReleaseManifestArtifactPath,
  machoUuidForFile,
  releaseChecksums,
  targetConfiguration,
  verifyReleaseArtifactDirectory,
} from '../scripts/release-artifact-contract.mjs'
import {
  releaseSourcePaths,
  releaseSourceSha256,
} from '../scripts/release-source-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const sourceCommit = '1234567890abcdef1234567890abcdef12345678'
const sourceDateEpoch = 1_700_000_000
const helperReleasePrivateKey = createPrivateKey({
  key: Buffer.from(`302e020100300506032b657004220420${'2a'.repeat(32)}`, 'hex'),
  format: 'der',
  type: 'pkcs8',
})
const helperReleasePublicKeyHex = createPublicKey(helperReleasePrivateKey)
  .export({ format: 'der', type: 'spki' })
  .subarray(-32)
  .toString('hex')

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function writeFixtureFile(root_, path, bytes) {
  const destination = join(root_, path)
  mkdirSync(resolve(destination, '..'), { recursive: true })
  writeFileSync(destination, bytes)
  return destination
}

function replayRustDescriptors(artifactRoot, target) {
  const targetIdentity = targetConfiguration(target)
  return RUST_RELEASE_ARTIFACTS.map(artifact => {
    const path = join(artifactRoot, 'bin', artifact.binaryName)
    const bytes = readFileSync(path)
    return {
      packageName: artifact.packageName,
      binaryName: artifact.binaryName,
      role: artifact.role,
      distribution: artifact.distribution,
      bytes: bytes.length,
      sha256: sha256(bytes),
      ...(targetIdentity.os === 'macos' ? { machoUuid: machoUuidForFile(path) } : {}),
    }
  })
}

function thinMachoFixture(target, binaryName) {
  const targetIdentity = targetConfiguration(target)
  const header = Buffer.alloc(32 + 24)
  header.writeUInt32LE(0xfeedfacf, 0)
  header.writeUInt32LE(targetIdentity.arch === 'arm64' ? 0x0100000c : 0x01000007, 4)
  header.writeUInt32LE(0, 8)
  header.writeUInt32LE(2, 12)
  header.writeUInt32LE(1, 16)
  header.writeUInt32LE(24, 20)
  header.writeUInt32LE(0, 24)
  header.writeUInt32LE(0, 28)
  header.writeUInt32LE(0x1b, 32)
  header.writeUInt32LE(24, 36)
  createHash('sha256').update(`${target}\0${binaryName}`).digest().copy(header, 40, 0, 16)
  return Buffer.concat([header, Buffer.from(`fixture ${binaryName} ${helperReleasePublicKeyHex}\n`)])
}

function replayClientDescriptors(artifactRoot) {
  return [
    'asset-manifest.json',
    'assets/client.js',
    'index.html',
    'runtime-config.js',
    'version.json',
  ].map(path => {
    const bytes = readFileSync(join(artifactRoot, 'client', path))
    return { path: `client/${path}`, bytes: bytes.length, sha256: sha256(bytes) }
  })
}

function writeClientAssetManifest(artifactRoot) {
  const assets = ['assets/client.js', 'index.html', 'version.json'].map(path => {
    const bytes = readFileSync(join(artifactRoot, 'client', path))
    return { path, bytes: bytes.length, sha256: sha256(bytes) }
  })
  const targets = Object.fromEntries(RELEASE_TARGETS.map(({ target }) => [target, assets]))
  writeFixtureFile(artifactRoot, 'client/asset-manifest.json', `${JSON.stringify({
    schemaVersion: 1,
    package: '@winwincode/client',
    version: '0.1.0-alpha.1',
    entry: 'index.html',
    runtimeConfig: {
      path: 'runtime-config.js',
      field: 'serverUrl',
      mutableAtDeployment: true,
    },
    assets,
    targets,
  }, null, 2)}\n`)
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

function writeHelperReleaseManifest(artifactRoot) {
  const helperPath = join(artifactRoot, 'bin/winwincode-kernel-helper')
  const fields = {
    schemaVersion: 1,
    protocol: 'winwincode-kernel-helper-release',
    version: 1,
    packageVersion: '0.1.0-alpha.1',
    sourceSha256: `sha256:${sha256(readFileSync(join(root, 'crates/helper/src/main.rs')))}`,
    binarySha256: `sha256:${sha256(readFileSync(helperPath))}`,
    binaryPath: 'winwincode-kernel-helper',
    binaryMode: 0o755,
  }
  const manifest = {
    ...fields,
    signature: sign(null, helperReleaseSigningBytes(fields), helperReleasePrivateKey)
      .toString('base64url'),
  }
  const path = writeFixtureFile(
    artifactRoot,
    `bin/${HELPER_RELEASE_MANIFEST_NAME}`,
    `${JSON.stringify(manifest, null, 2)}\n`,
  )
  chmodSync(path, 0o644)
}

function replayHelperReleaseManifest(artifactRoot) {
  const { bytes, sha256: digest } = descriptorForFile(
    join(artifactRoot, 'bin'),
    join(artifactRoot, 'bin', HELPER_RELEASE_MANIFEST_NAME),
  )
  return { bytes, sha256: digest, publicKeyHex: helperReleasePublicKeyHex }
}

function createArtifactFixture(evidenceRoot, target) {
  const artifactRoot = join(evidenceRoot, target)
  const targetIdentity = targetConfiguration(target)
  for (const artifact of RUST_RELEASE_ARTIFACTS) {
    const path = writeFixtureFile(
      artifactRoot,
      `bin/${artifact.binaryName}`,
      targetIdentity.os === 'macos'
        ? thinMachoFixture(target, artifact.binaryName)
        : `fixture ${artifact.binaryName} ${target} ${helperReleasePublicKeyHex}\n`,
    )
    chmodSync(path, 0o755)
  }
  writeHelperReleaseManifest(artifactRoot)
  writeFixtureFile(artifactRoot, 'client/index.html', '<!doctype html>\n')
  writeFixtureFile(artifactRoot, 'client/runtime-config.js', 'globalThis.WINWINCODE_CONFIG = {}\n')
  writeFixtureFile(artifactRoot, 'client/version.json', `${JSON.stringify({
    schemaVersion: 1,
    product: 'WinWinCode',
    package: '@winwincode/client',
    version: '0.1.0-alpha.1',
    controlPlaneSchemaVersion: 'winwincode/v1',
  }, null, 2)}\n`)
  writeFixtureFile(artifactRoot, 'client/assets/client.js', 'console.log("fixture")\n')
  writeClientAssetManifest(artifactRoot)
  for (const name of ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']) {
    const destination = writeFixtureFile(artifactRoot, `legal/${name}`, '')
    copyFileSync(join(root, name), destination)
  }
  const manifest = createReleaseArtifactManifest({
    root,
    artifactRoot,
    target,
    sourceCommit,
    sourceDateEpoch,
    replayRust: replayRustDescriptors(artifactRoot, target),
    replayClient: replayClientDescriptors(artifactRoot),
    helperReleasePublicKeyHex,
    replayHelperReleaseManifest: replayHelperReleaseManifest(artifactRoot),
  })
  writeFileSync(join(artifactRoot, RELEASE_ARTIFACT_MANIFEST), canonicalJson(manifest))
  writeFileSync(join(artifactRoot, RELEASE_CHECKSUMS), releaseChecksums(manifest))
  return { artifactRoot, manifest }
}

test('release target contract is the exact supported four-platform matrix', () => {
  assert.deepEqual(
    RELEASE_TARGETS.map(entry => entry.target),
    [
      'aarch64-apple-darwin',
      'x86_64-apple-darwin',
      'aarch64-unknown-linux-gnu',
      'x86_64-unknown-linux-gnu',
    ],
  )
  assert.equal(targetConfiguration('aarch64-apple-darwin').arch, 'arm64')
  assert.throws(
    () => targetConfiguration('x86_64-pc-windows-msvc'),
    error => error instanceof ReleaseArtifactError && error.code === 'TARGET_UNSUPPORTED',
  )
})

test('release source identity excludes generated output and remains deterministic', () => {
  const paths = releaseSourcePaths(root)
  assert.equal(paths.some(path => path.includes('/dist/')), false)
  assert.equal(paths.some(path => path.includes('/prebuild/')), false)
  assert.equal(paths.some(path => path.startsWith('third_party/codex/')), false)
  assert.match(releaseSourceSha256(root), /^[0-9a-f]{64}$/u)
  assert.equal(releaseSourceSha256(root), releaseSourceSha256(root))
})

test('release runner keeps the private signing key inside the signing process', () => {
  const runner = readFileSync(join(root, 'scripts/run-release-artifact-gate.mjs'), 'utf8')
  assert.match(
    runner,
    /delete environment\.WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX/u,
  )
  assert.match(
    runner,
    /const privateKeyHex = process\.env\.WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX/u,
  )
})

test('release runner rejects source mutation around both isolated builds', () => {
  const runner = readFileSync(join(root, 'scripts/run-release-artifact-gate.mjs'), 'utf8')
  assert.match(
    runner,
    /const expectedReleaseSourceSha256 = assertCleanCommit\(sourceCommit, sourceDateEpoch\)/u,
  )
  assert.match(runner, /label: 'primary'[\s\S]*label: 'replay'/u)
  assert.match(
    runner,
    /function isolatedReleaseBuild[\s\S]*assertCleanCommit\([\s\S]*clientBuild[\s\S]*rustBuild[\s\S]*assertCleanCommit/u,
  )
  assert.match(runner, /SOURCE_MUTATION: release source changed/u)
})

test('release runner cold-builds twice at one physical Cargo target outside its snapshot', () => {
  const runner = readFileSync(join(root, 'scripts/run-release-artifact-gate.mjs'), 'utf8')
  assert.match(runner, /const cargoTarget = resolve\(buildRoot, 'cargo-target'\)/u)
  assert.match(
    runner,
    /function resetCargoTarget[\s\S]*rmSync\(targetDirectory,[\s\S]*mkdirSync\(targetDirectory/u,
  )
  assert.equal((runner.match(/resetCargoTarget\(cargoTarget\)/gu) ?? []).length, 2)
  assert.equal(runner.includes('cargo-primary'), false)
  assert.equal(runner.includes('cargo-replay'), false)

  const snapshot = runner.indexOf('const primaryRustSnapshot = snapshotRustBuild')
  const coldReplay = runner.lastIndexOf('resetCargoTarget(cargoTarget)')
  const replay = runner.indexOf("label: 'replay'")
  assert.equal(snapshot > 0 && snapshot < coldReplay && coldReplay < replay, true)
  assert.match(runner, /const rustSnapshot = resolve\(buildRoot, 'rust-primary'\)/u)
  assert.match(runner, /chmodSync\(sourcePath, 0o555\)/u)
  assert.match(runner, /chmodSync\(helperReleaseManifestPath, 0o444\)/u)
  assert.match(runner, /stageArtifacts\(artifactRoot, primaryRustSnapshot, clientSnapshot\)/u)
})

test('macOS release builds ad-hoc sign every Rust artifact before hashing and the helper sidecar', () => {
  const runner = readFileSync(join(root, 'scripts/run-release-artifact-gate.mjs'), 'utf8')
  assert.match(
    runner,
    /function signMacReleaseArtifacts\(targetIdentity, artifactPaths\) \{[\s\S]*if \(targetIdentity\.os !== 'macos'\) return[\s\S]*run\('codesign', \['--force', '--sign', '-', path\]\)/u,
  )

  const rustBuild = runner.indexOf('function rustBuild(')
  const sign = runner.indexOf('signMacReleaseArtifacts(targetIdentity, artifactPaths)', rustBuild)
  const descriptor = runner.indexOf('descriptorForFile(dirname(path), path)', rustBuild)
  const helperSidecar = runner.indexOf('writeHelperReleaseManifest(root, helper.sourcePath)', rustBuild)
  assert.equal(rustBuild > 0 && sign > rustBuild && sign < descriptor && descriptor < helperSidecar, true)
  assert.match(
    runner,
    /const artifactPaths = RUST_RELEASE_ARTIFACTS\.map[\s\S]*for \(const path of artifactPaths\) chmodSync\(path, 0o755\)[\s\S]*signMacReleaseArtifacts/u,
  )
})

test('isolated release builds use byte-identical raw Rust flags and preserve linker UUIDs', () => {
  const home = '/home/runner'
  const runnerTemp = `${home}/work/_temp`
  const sourceRoot = `${home}/work/winwincode/winwincode`
  const buildRoot = `${runnerTemp}/winwincode-release-ABC123`
  const cargoTarget = `${buildRoot}/cargo-target`
  const common = {
    baseEnvironment: { RUSTFLAGS: '-Cdebuginfo=0', SAFE_VALUE: 'preserved' },
    root: sourceRoot,
    buildRoot,
    targetDirectories: [cargoTarget],
    target: 'aarch64-apple-darwin',
    runnerTemp,
    home,
    sourceDateEpoch,
  }
  const primary = createReleaseBuildEnvironment({
    ...common,
    targetDirectory: cargoTarget,
  })
  const replay = createReleaseBuildEnvironment({
    ...common,
    targetDirectory: cargoTarget,
  })

  assert.deepEqual(Buffer.from(primary.RUSTFLAGS), Buffer.from(replay.RUSTFLAGS))
  assert.equal(primary.CARGO_TARGET_DIR, cargoTarget)
  assert.equal(replay.CARGO_TARGET_DIR, cargoTarget)
  assert.equal(Object.hasOwn(primary, 'CARGO_BUILD_JOBS'), false)
  assert.equal(Object.hasOwn(replay, 'CARGO_BUILD_JOBS'), false)
  assert.equal(primary.SAFE_VALUE, 'preserved')
  assert.equal(primary.SOURCE_DATE_EPOCH, String(sourceDateEpoch))
  assert.deepEqual(primary.RUSTFLAGS.split(' ').slice(-1), [
    `--remap-path-prefix=${cargoTarget}=.target`,
  ])
  assert.equal(primary.RUSTFLAGS.includes('-Wl,-no_uuid'), false)
  assert.equal(primary.RUSTFLAGS.includes('-Clink-arg=-Wl,-reproducible'), true)
  for (const mapping of [
    `--remap-path-prefix=${home}=.home`,
    `--remap-path-prefix=${runnerTemp}=.runner-temp`,
    `--remap-path-prefix=${sourceRoot}=.`,
    `--remap-path-prefix=${buildRoot}=.release-build`,
  ]) assert.equal(primary.RUSTFLAGS.includes(mapping), true, mapping)

  const linux = createReleaseBuildEnvironment({
    ...common,
    target: 'aarch64-unknown-linux-gnu',
    targetDirectory: cargoTarget,
  })
  assert.equal(linux.RUSTFLAGS.includes('-Wl,-no_uuid'), false)
  assert.equal(linux.RUSTFLAGS.includes('-Wl,-reproducible'), false)

})

test('Mach-O artifacts require exactly one linker-generated LC_UUID', t => {
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-macho-uuid-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const path = writeFixtureFile(
    directory,
    'winwincode-server',
    thinMachoFixture('aarch64-apple-darwin', 'winwincode-server'),
  )
  assert.match(machoUuidForFile(path), /^[0-9a-f]{32}$/u)

  const missing = Buffer.from(readFileSync(path))
  missing.writeUInt32LE(0, 16)
  missing.writeUInt32LE(0, 20)
  writeFileSync(path, missing)
  assert.throws(
    () => machoUuidForFile(path),
    error => error instanceof ReleaseArtifactError
      && error.code === 'ARTIFACT_FORMAT_INVALID',
  )

})

test('per-target evidence binds source, protocols, legal files and reproducible artifacts', t => {
  const evidenceRoot = mkdtempSync(join(tmpdir(), 'winwincode-release-artifact-'))
  t.after(() => rmSync(evidenceRoot, { recursive: true, force: true }))
  const { artifactRoot, manifest } = createArtifactFixture(
    evidenceRoot,
    'aarch64-apple-darwin',
  )
  const helperManifestPath = helperReleaseManifestArtifactPath(artifactRoot)
  assert.equal(helperManifestPath, join(artifactRoot, 'bin', HELPER_RELEASE_MANIFEST_NAME))
  assert.equal(existsSync(helperManifestPath), true)
  assert.equal(
    existsSync(join(artifactRoot, 'bin', 'bin', HELPER_RELEASE_MANIFEST_NAME)),
    false,
  )
  assert.equal(manifest.source.version, '0.1.0-alpha.1')
  assert.equal(manifest.source.license, 'Apache-2.0')
  assert.equal(manifest.protocols.controlPlane.schemaVersion, 'winwincode/v1')
  assert.equal(manifest.protocols.executionPort.title, 'WinWinCode ExecutionPort v1')
  assert.equal(manifest.artifacts.localComposition.mode, 'server-local-composition')
  assert.equal(manifest.artifacts.localComposition.launcherBinary, 'winwincode-server')
  assert.equal(
    manifest.artifacts.rust.every(entry => /^[0-9a-f]{32}$/u.test(entry.machoUuid)),
    true,
  )
  assert.deepEqual(manifest.build.pathRemapping, [
    'home',
    'runner-temp',
    'source-root',
    'release-build',
    'cargo-target',
  ])
  assert.equal(manifest.build.linkerMode, 'reproducible-lc-uuid')
  assert.deepEqual(
    manifest.artifacts.rust.map(entry => [entry.role, entry.distribution]),
    [
      ['server', 'product'],
      ['worker', 'product'],
      ['worker-helper', 'worker-internal'],
    ],
  )
  assert.equal(
    verifyReleaseArtifactDirectory({
      root,
      artifactRoot,
      expectedCommit: sourceCommit,
      expectedTarget: 'aarch64-apple-darwin',
      expectedSourceDateEpoch: sourceDateEpoch,
    }).reproducibility.verified,
    true,
  )
  assert.deepEqual(
    descriptorForFile(artifactRoot, join(artifactRoot, RELEASE_ARTIFACT_MANIFEST)),
    descriptorForFile(artifactRoot, join(artifactRoot, RELEASE_ARTIFACT_MANIFEST)),
  )

  const changedIdentity = {
    ...manifest,
    artifacts: {
      ...manifest.artifacts,
      rust: manifest.artifacts.rust.map((entry, index) => (
        index === 0 ? { ...entry, role: 'unexpected-role' } : entry
      )),
    },
  }
  writeFileSync(
    join(artifactRoot, RELEASE_ARTIFACT_MANIFEST),
    canonicalJson(changedIdentity),
  )
  assert.throws(
    () => verifyReleaseArtifactDirectory({
      root,
      artifactRoot,
      expectedCommit: sourceCommit,
      expectedTarget: 'aarch64-apple-darwin',
      expectedSourceDateEpoch: sourceDateEpoch,
    }),
    error => error instanceof ReleaseArtifactError && error.code === 'MANIFEST_INVALID',
  )
  writeFileSync(join(artifactRoot, RELEASE_ARTIFACT_MANIFEST), canonicalJson(manifest))

  writeFixtureFile(artifactRoot, 'unlisted.txt', 'unlisted\n')
  assert.throws(
    () => verifyReleaseArtifactDirectory({
      root,
      artifactRoot,
      expectedCommit: sourceCommit,
      expectedTarget: 'aarch64-apple-darwin',
      expectedSourceDateEpoch: sourceDateEpoch,
    }),
    error => error instanceof ReleaseArtifactError && error.code === 'ARTIFACT_SET_MISMATCH',
  )
  rmSync(join(artifactRoot, 'unlisted.txt'), { force: true })

  writeFileSync(join(artifactRoot, 'bin/winwincode-worker'), 'tampered\n')
  assert.throws(
    () => verifyReleaseArtifactDirectory({
      root,
      artifactRoot,
      expectedCommit: sourceCommit,
      expectedTarget: 'aarch64-apple-darwin',
      expectedSourceDateEpoch: sourceDateEpoch,
    }),
    error => error instanceof ReleaseArtifactError && error.code === 'ARTIFACT_MISMATCH',
  )
})

test('aggregate report requires and verifies the exact four-target evidence set', t => {
  const evidenceRoot = mkdtempSync(join(tmpdir(), 'winwincode-release-report-'))
  t.after(() => rmSync(evidenceRoot, { recursive: true, force: true }))
  for (const { target } of RELEASE_TARGETS) createArtifactFixture(evidenceRoot, target)
  const first = createReleaseReport({ root, evidenceRoot, expectedCommit: sourceCommit, sourceDateEpoch })
  const second = createReleaseReport({ root, evidenceRoot, expectedCommit: sourceCommit, sourceDateEpoch })
  assert.equal(first.kind, RELEASE_REPORT_KIND)
  assert.equal(first.status, 'passed')
  assert.deepEqual(first, second)
  assert.equal(first.targets.length, 4)
  assert.equal(new Set(first.targets.map(entry => entry.clientSha256)).size, 1)

  mkdirSync(join(evidenceRoot, 'extra-target'))
  assert.throws(
    () => createReleaseReport({ root, evidenceRoot, expectedCommit: sourceCommit, sourceDateEpoch }),
    error => error instanceof ReleaseArtifactError && error.code === 'ARTIFACT_SET_MISMATCH',
  )
  rmSync(join(evidenceRoot, 'extra-target'), { recursive: true, force: true })

  rmSync(join(evidenceRoot, 'x86_64-unknown-linux-gnu'), { recursive: true, force: true })
  assert.throws(
    () => createReleaseReport({ root, evidenceRoot, expectedCommit: sourceCommit, sourceDateEpoch }),
    error => error instanceof ReleaseArtifactError && error.code === 'ARTIFACT_MISSING',
  )
})

test('reproducibility comparison rejects a changed isolated binary or LC_UUID', t => {
  const evidenceRoot = mkdtempSync(join(tmpdir(), 'winwincode-release-replay-'))
  t.after(() => rmSync(evidenceRoot, { recursive: true, force: true }))
  const { artifactRoot } = createArtifactFixture(evidenceRoot, 'aarch64-apple-darwin')
  const replay = replayRustDescriptors(artifactRoot, 'aarch64-apple-darwin')
  replay[1] = { ...replay[1], machoUuid: 'f'.repeat(32) }
  assert.throws(
    () => createReleaseArtifactManifest({
      root,
      artifactRoot,
      target: 'aarch64-apple-darwin',
      sourceCommit,
      sourceDateEpoch,
      replayRust: replay,
      replayClient: replayClientDescriptors(artifactRoot),
      helperReleasePublicKeyHex,
      replayHelperReleaseManifest: replayHelperReleaseManifest(artifactRoot),
    }),
    error => error instanceof ReleaseArtifactError && error.code === 'REPRODUCIBILITY_FAILED',
  )

  const helperManifestPath = join(artifactRoot, 'bin', HELPER_RELEASE_MANIFEST_NAME)
  const helperManifest = JSON.parse(readFileSync(helperManifestPath, 'utf8'))
  helperManifest.signature = `${helperManifest.signature.slice(0, -1)}${
    helperManifest.signature.endsWith('A') ? 'B' : 'A'
  }`
  writeFileSync(helperManifestPath, `${JSON.stringify(helperManifest, null, 2)}\n`)
  assert.throws(
    () => createReleaseArtifactManifest({
      root,
      artifactRoot,
      target: 'aarch64-apple-darwin',
      sourceCommit,
      sourceDateEpoch,
      replayRust: replayRustDescriptors(artifactRoot, 'aarch64-apple-darwin'),
      replayClient: replayClientDescriptors(artifactRoot),
      helperReleasePublicKeyHex,
      replayHelperReleaseManifest: replayHelperReleaseManifest(artifactRoot),
    }),
    error => error instanceof ReleaseArtifactError && error.code === 'HELPER_RELEASE_INVALID',
  )
})
