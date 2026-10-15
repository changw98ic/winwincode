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
  RELEASE_ARTIFACT_MANIFEST,
  RELEASE_ARTIFACT_CHECKS,
  RELEASE_CHECKSUMS,
  RELEASE_REPORT_KIND,
  RELEASE_TARGETS,
  RUST_RELEASE_ARTIFACTS,
  ReleaseArtifactError,
  canonicalJson,
  createReleaseBuildEnvironment,
  createReleaseArtifactManifest,
  createReleaseReport,
  descriptorForFile,
  machoUuidForFile,
  releaseChecksums,
  stageCommunityCoreWorkerRuntime,
  targetConfiguration,
  verifyReleaseArtifactDirectory,
} from '../scripts/release-artifact-contract.mjs'
import {
  releaseSourcePaths,
  releaseSourceSha256,
} from '../scripts/release-source-contract.mjs'
import { capturedStandardOutput } from '../scripts/child-process-output.mjs'

const root = resolve(import.meta.dirname, '..')
const workspaceVersion = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')).version
const sourceCommit = '1234567890abcdef1234567890abcdef12345678'
const sourceDateEpoch = 1_700_000_000
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
  return Buffer.concat([header, Buffer.from(`fixture ${binaryName}\n`)])
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
    version: workspaceVersion,
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

function createArtifactFixture(evidenceRoot, target) {
  const artifactRoot = join(evidenceRoot, target)
  const targetIdentity = targetConfiguration(target)
  for (const artifact of RUST_RELEASE_ARTIFACTS) {
    const path = writeFixtureFile(
      artifactRoot,
      `bin/${artifact.binaryName}`,
      targetIdentity.os === 'macos'
        ? thinMachoFixture(target, artifact.binaryName)
        : `fixture ${artifact.binaryName} ${target}\n`,
    )
    chmodSync(path, 0o755)
  }
  writeFixtureFile(artifactRoot, 'client/index.html', '<!doctype html>\n')
  writeFixtureFile(artifactRoot, 'client/runtime-config.js', 'globalThis.WINWINCODE_CONFIG = {}\n')
  writeFixtureFile(artifactRoot, 'client/version.json', `${JSON.stringify({
    schemaVersion: 1,
    product: 'WinWinCode',
    package: '@winwincode/client',
    version: workspaceVersion,
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
  assert.equal(paths.includes('.cargo/config.toml'), true)
  assert.equal(paths.some(path => path.includes('/dist/')), false)
  assert.equal(paths.some(path => path.includes('/prebuild/')), false)
  assert.equal(paths.some(path => path.startsWith('third_party/codex/')), false)
  assert.match(releaseSourceSha256(root), /^[0-9a-f]{64}$/u)
  assert.equal(releaseSourceSha256(root), releaseSourceSha256(root))
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

test('release runner accepts inherited output and trims only captured output', () => {
  assert.equal(capturedStandardOutput({ stdout: null }, false), '')
  assert.equal(capturedStandardOutput({ stdout: '  captured output\n' }, true), 'captured output')
  assert.throws(
    () => capturedStandardOutput({ stdout: null }, true),
    /captured child process stdout must be a string/u,
  )

  const runner = readFileSync(join(root, 'scripts/run-release-artifact-gate.mjs'), 'utf8')
  assert.match(runner, /return capturedStandardOutput\(result, options\.capture\)/u)
})

test('release runner leaves the complete workspace gate to mainline and runs one release API vertical', () => {
  const runner = readFileSync(join(root, 'scripts/run-release-artifact-gate.mjs'), 'utf8')
  assert.doesNotMatch(runner, /pnpm', 'verify|pnpm verify/u)
  assert.deepEqual(RELEASE_ARTIFACT_CHECKS, [
    'product-build',
    'api-production-vertical',
    'isolated-rebuild',
  ])
  assert.equal(
    [...runner.matchAll(/\n  runReleaseApiVertical\(buildRoot, primaryRustSnapshot\)/gu)].length,
    1,
  )
  assert.match(runner, /'scripts\/run-api-production-vertical\.mjs',[\s\S]*'--skip-build'/u)
  assert.match(runner, /'--server-binary', paths\['winwincode-server'\]/u)
  assert.match(runner, /'--worker-binary', paths\['winwincode-worker'\]/u)
  assert.match(runner, /const runtimeRoot = resolve\(buildRoot, 'api-production-runtime'\)/u)
  assert.match(runner, /report\.flow\?\.chat\?\.status !== 'Completed'/u)
  assert.match(runner, /report\.flow\?\.strongflow\?\.status !== 'done'/u)
  assert.match(runner, /report\.flow\?\.strongflow\?\.verdictStatus !== 'pass'/u)
  assert.match(runner, /report\.remoteWorker\?\.terminalAfterWorkerRestart !== true/u)
  assert.match(runner, /report\.remoteWorker\?\.terminalAfterServerRestart !== true/u)
  const coldReplay = runner.lastIndexOf('resetCargoTarget(cargoTarget)')
  const replay = runner.indexOf("label: 'replay'")
  const api = runner.indexOf('  runReleaseApiVertical(buildRoot, primaryRustSnapshot)')
  const stage = runner.indexOf('  stageArtifacts(artifactRoot, primaryRustSnapshot, clientSnapshot)')
  assert.equal(api < coldReplay && coldReplay < replay && replay < stage, true)
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
  assert.match(runner, /stageArtifacts\(artifactRoot, primaryRustSnapshot, clientSnapshot\)/u)
})

test('macOS release builds ad-hoc sign every Rust artifact before hashing', () => {
  const runner = readFileSync(join(root, 'scripts/run-release-artifact-gate.mjs'), 'utf8')
  assert.match(
    runner,
    /function signMacReleaseArtifacts\(targetIdentity, artifactPaths\) \{[\s\S]*if \(targetIdentity\.os !== 'macos'\) return[\s\S]*run\('codesign', \['--force', '--sign', '-', path\]\)/u,
  )

  const rustBuild = runner.indexOf('function rustBuild(')
  const sign = runner.indexOf('signMacReleaseArtifacts(targetIdentity, artifactPaths)', rustBuild)
  const descriptor = runner.indexOf('descriptorForFile(dirname(path), path)', rustBuild)
  assert.equal(rustBuild > 0 && sign > rustBuild && sign < descriptor, true)
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
  assert.equal(manifest.source.version, workspaceVersion)
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
      ['device-client-cli', 'product'],
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

test('Community Core worker runtime excludes Server and Client product files', t => {
  const evidenceRoot = mkdtempSync(join(tmpdir(), 'winwincode-core-worker-'))
  t.after(() => rmSync(evidenceRoot, { recursive: true, force: true }))
  const { artifactRoot } = createArtifactFixture(evidenceRoot, 'x86_64-unknown-linux-gnu')
  const outputRoot = join(evidenceRoot, 'core-worker-runtime')
  const result = stageCommunityCoreWorkerRuntime({
    root,
    artifactRoot,
    expectedCommit: sourceCommit,
    expectedTarget: 'x86_64-unknown-linux-gnu',
    expectedSourceDateEpoch: sourceDateEpoch,
    outputRoot,
  })
  assert.equal(result.version, workspaceVersion)
  assert.deepEqual(result.files.map(entry => entry.path), [
    'bin/winwincode-worker',
    'bin/winwincode-kernel-helper',
    'LICENSE',
    'NOTICE',
    'THIRD_PARTY_NOTICES.md',
  ])
  assert.equal(existsSync(join(outputRoot, 'bin/winwincode-server')), false)
  assert.equal(existsSync(join(outputRoot, 'client')), false)
  assert.throws(
    () => stageCommunityCoreWorkerRuntime({
      root,
      artifactRoot,
      expectedCommit: sourceCommit,
      expectedTarget: 'x86_64-unknown-linux-gnu',
      expectedSourceDateEpoch: sourceDateEpoch,
      outputRoot,
    }),
    error => error instanceof ReleaseArtifactError && error.code === 'OUTPUT_EXISTS',
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
        }),
    error => error instanceof ReleaseArtifactError && error.code === 'REPRODUCIBILITY_FAILED',
  )

})
