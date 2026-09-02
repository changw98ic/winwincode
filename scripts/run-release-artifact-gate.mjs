#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { createPrivateKey, createPublicKey } from 'node:crypto'
import {
  chmodSync,
  copyFileSync,
  cpSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'

import {
  RELEASE_ARTIFACT_MANIFEST,
  RELEASE_CHECKSUMS,
  HELPER_RELEASE_MANIFEST_NAME,
  RUST_RELEASE_ARTIFACTS,
  canonicalJson,
  createReleaseBuildEnvironment,
  createReleaseArtifactManifest,
  descriptorForFile,
  descriptorsBelow,
  helperReleaseBuildBaseEnvironment,
  helperReleaseManifestArtifactPath,
  helperReleaseVerificationBaseEnvironment,
  machoUuidForFile,
  releaseChecksums,
  targetConfiguration,
  verifyReleaseArtifactDirectory,
} from './release-artifact-contract.mjs'
import {
  writeApiProductionSourceSeal,
  writeHelperReleaseManifest,
} from './run-api-production-vertical.mjs'
import { capturedStandardOutput } from './child-process-output.mjs'
import { releaseSourceSha256 } from './release-source-contract.mjs'

const root = resolve(import.meta.dirname, '..')

function verificationChildEnvironment() {
  return helperReleaseVerificationBaseEnvironment(process.env)
}

function parseArguments(argv) {
  const values = new Map()
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
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
  const allowed = new Set(['target', 'source-commit', 'source-date-epoch', 'output'])
  for (const key of values.keys()) {
    if (!allowed.has(key)) throw new Error(`unknown argument --${key}`)
  }
  for (const key of allowed) {
    if (!values.has(key)) throw new Error(`--${key} is required`)
  }
  return Object.freeze({
    target: values.get('target'),
    sourceCommit: values.get('source-commit'),
    sourceDateEpoch: Number(values.get('source-date-epoch')),
    output: resolve(root, values.get('output')),
  })
}

function run(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    cwd: root,
    env: options.env ?? verificationChildEnvironment(),
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
  return capturedStandardOutput(result, options.capture)
}

function releaseEnvironment(target, targetDirectory, sourceDateEpoch, buildPaths) {
  return createReleaseBuildEnvironment({
    baseEnvironment: helperReleaseBuildBaseEnvironment(process.env),
    root,
    buildRoot: buildPaths.buildRoot,
    targetDirectory,
    targetDirectories: buildPaths.targetDirectories,
    target,
    runnerTemp: process.env.RUNNER_TEMP,
    home: process.env.HOME,
    sourceDateEpoch,
  })
}

function releaseSigningConfiguration() {
  const privateKeyHex = process.env.WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX
  const publicKeyHex = process.env.WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX
  if (!/^[0-9a-f]{64}$/u.test(privateKeyHex ?? '')) {
    throw new Error('WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX must be configured for a release build')
  }
  if (!/^[0-9a-f]{64}$/u.test(publicKeyHex ?? '')) {
    throw new Error('WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX must be configured for a release build')
  }
  const privateKey = createPrivateKey({
    key: Buffer.from(`302e020100300506032b657004220420${privateKeyHex}`, 'hex'),
    format: 'der',
    type: 'pkcs8',
  })
  const derivedPublicKeyHex = createPublicKey(privateKey)
    .export({ format: 'der', type: 'spki' })
    .subarray(-32)
    .toString('hex')
  if (publicKeyHex !== derivedPublicKeyHex) {
    throw new Error('helper release public key does not match the configured private key')
  }
  return Object.freeze({ publicKeyHex })
}

function assertCleanCommit(sourceCommit, sourceDateEpoch, expectedReleaseSourceSha256) {
  const head = run('git', ['rev-parse', 'HEAD'], { capture: true })
  if (head !== sourceCommit) throw new Error(`source commit ${sourceCommit} does not match HEAD ${head}`)
  const commitEpoch = Number(run('git', ['show', '-s', '--format=%ct', sourceCommit], { capture: true }))
  if (commitEpoch !== sourceDateEpoch) {
    throw new Error(`SOURCE_DATE_EPOCH ${String(sourceDateEpoch)} does not match commit time ${String(commitEpoch)}`)
  }
  const status = run('git', ['status', '--porcelain=v1', '--untracked-files=all'], { capture: true })
  if (status.length > 0) throw new Error('release artifact gate requires a clean checkout')
  const actualReleaseSourceSha256 = releaseSourceSha256(root)
  if (expectedReleaseSourceSha256 !== undefined
    && actualReleaseSourceSha256 !== expectedReleaseSourceSha256) {
    throw new Error(
      `SOURCE_MUTATION: release source changed from ${expectedReleaseSourceSha256} to ${actualReleaseSourceSha256}`,
    )
  }
  const verifiedHead = run('git', ['rev-parse', 'HEAD'], { capture: true })
  const verifiedStatus = run(
    'git',
    ['status', '--porcelain=v1', '--untracked-files=all'],
    { capture: true },
  )
  if (verifiedHead !== sourceCommit || verifiedStatus.length > 0) {
    throw new Error('SOURCE_MUTATION: release source changed while its digest was being verified')
  }
  return actualReleaseSourceSha256
}

function isolatedReleaseBuild({
  label,
  target,
  targetDirectory,
  sourceCommit,
  sourceDateEpoch,
  expectedReleaseSourceSha256,
  buildPaths,
}) {
  assertCleanCommit(sourceCommit, sourceDateEpoch, expectedReleaseSourceSha256)
  const client = clientBuild(sourceDateEpoch)
  const rust = rustBuild(target, targetDirectory, sourceDateEpoch, buildPaths)
  assertCleanCommit(sourceCommit, sourceDateEpoch, expectedReleaseSourceSha256)
  return Object.freeze({ label, client, rust })
}

function signMacReleaseArtifacts(targetIdentity, artifactPaths) {
  if (targetIdentity.os !== 'macos') return
  for (const path of artifactPaths) {
    run('codesign', ['--force', '--sign', '-', path])
  }
}

function rustBuild(target, targetDirectory, sourceDateEpoch, buildPaths) {
  const env = releaseEnvironment(target, targetDirectory, sourceDateEpoch, buildPaths)
  const targetIdentity = targetConfiguration(target)
  run(process.execPath, [
    'scripts/build-products.mjs',
    '--release',
    '--target',
    target,
  ], { env })
  const artifactPaths = RUST_RELEASE_ARTIFACTS.map(artifact => (
    resolve(targetDirectory, target, 'release', artifact.binaryName)
  ))
  for (const path of artifactPaths) chmodSync(path, 0o755)
  signMacReleaseArtifacts(targetIdentity, artifactPaths)
  const rust = RUST_RELEASE_ARTIFACTS.map(artifact => {
    const path = resolve(targetDirectory, target, 'release', artifact.binaryName)
    const { bytes, sha256 } = descriptorForFile(dirname(path), path)
    return Object.freeze({
      packageName: artifact.packageName,
      binaryName: artifact.binaryName,
      role: artifact.role,
      distribution: artifact.distribution,
      bytes,
      sha256,
      ...(targetIdentity.os === 'macos' ? { machoUuid: machoUuidForFile(path) } : {}),
      sourcePath: path,
    })
  })
  const helper = rust.find(artifact => artifact.binaryName === 'winwincode-kernel-helper')
  if (helper === undefined) throw new Error('Kernel helper release artifact is missing')
  const helperReleaseManifestPath = writeHelperReleaseManifest(root, helper.sourcePath)
  chmodSync(helperReleaseManifestPath, 0o644)
  const helperReleaseManifest = descriptorForFile(
    dirname(helperReleaseManifestPath),
    helperReleaseManifestPath,
  )
  return Object.freeze({ rust: Object.freeze(rust), helperReleaseManifest, helperReleaseManifestPath })
}

function snapshotRustBuild(rustBuildResult, snapshotRoot) {
  rmSync(snapshotRoot, { recursive: true, force: true })
  mkdirSync(snapshotRoot, { recursive: true })
  const rust = rustBuildResult.rust.map(artifact => {
    const sourcePath = resolve(snapshotRoot, artifact.binaryName)
    copyFileSync(artifact.sourcePath, sourcePath)
    chmodSync(sourcePath, 0o555)
    const { bytes, sha256 } = descriptorForFile(snapshotRoot, sourcePath)
    return Object.freeze({
      ...artifact,
      bytes,
      sha256,
      sourcePath,
    })
  })
  const helperReleaseManifestPath = resolve(snapshotRoot, HELPER_RELEASE_MANIFEST_NAME)
  copyFileSync(rustBuildResult.helperReleaseManifestPath, helperReleaseManifestPath)
  chmodSync(helperReleaseManifestPath, 0o444)
  return Object.freeze({
    rust: Object.freeze(rust),
    helperReleaseManifest: descriptorForFile(snapshotRoot, helperReleaseManifestPath),
    helperReleaseManifestPath,
  })
}

function resetCargoTarget(targetDirectory) {
  rmSync(targetDirectory, { recursive: true, force: true })
  mkdirSync(targetDirectory, { recursive: true })
}

function clientBuild(sourceDateEpoch) {
  run('corepack', ['pnpm', 'build:ts'], {
    env: {
      ...verificationChildEnvironment(),
      CI: '1',
      SOURCE_DATE_EPOCH: String(sourceDateEpoch),
    },
  })
  return descriptorsBelow(resolve(root, 'apps/client/dist/public')).map(entry => Object.freeze({
    path: `client/${entry.path}`,
    bytes: entry.bytes,
    sha256: entry.sha256,
  }))
}

function stageArtifacts(artifactRoot, rustBuildResult, clientSnapshot) {
  rmSync(artifactRoot, { recursive: true, force: true })
  mkdirSync(resolve(artifactRoot, 'bin'), { recursive: true })
  mkdirSync(resolve(artifactRoot, 'client'), { recursive: true })
  mkdirSync(resolve(artifactRoot, 'legal'), { recursive: true })
  for (const artifact of rustBuildResult.rust) {
    const destination = resolve(artifactRoot, 'bin', artifact.binaryName)
    copyFileSync(artifact.sourcePath, destination)
    chmodSync(destination, 0o755)
  }
  const helperReleaseManifestDestination = helperReleaseManifestArtifactPath(artifactRoot)
  copyFileSync(rustBuildResult.helperReleaseManifestPath, helperReleaseManifestDestination)
  chmodSync(helperReleaseManifestDestination, 0o644)
  cpSync(clientSnapshot, resolve(artifactRoot, 'client'), { recursive: true, force: true })
  for (const name of ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']) {
    const destination = resolve(artifactRoot, 'legal', name)
    copyFileSync(resolve(root, name), destination)
    chmodSync(destination, 0o644)
  }
}

function assertReleaseApiReport(report) {
  if (report?.health?.initial !== 'ready' || report.health.afterRestart !== 'ready') {
    throw new Error('release API vertical did not keep the Server ready across restart')
  }
  if (report.flow?.chat?.status !== 'Completed'
    || report.flow?.cancel?.state !== 'cancelled'
    || report.flow?.strongflow?.status !== 'delivered'
    || report.flow?.strongflow?.verdictStatus !== 'pass') {
    throw new Error('release API vertical did not complete Chat, cancel and StrongFlow')
  }
  if (report.deterministic?.contentEqual !== true
    || report.restart?.messageBytesStable !== true
    || report.restart?.deliveryBytesStable !== true) {
    throw new Error('release API vertical was not byte-stable across repeat and restart')
  }
  if (report.remoteWorker?.terminalAfterWorkerRestart !== true
    || report.remoteWorker?.terminalAfterServerRestart !== true
    || !Number.isInteger(report.remoteWorker.restartedPid)
    || report.remoteWorker.restartedPid === report.remoteWorker.initialPid
    || report.remoteWorker.survivedServerRestartPid !== report.remoteWorker.restartedPid) {
    throw new Error('release API vertical did not prove the remote Worker restart boundary')
  }
}

function runReleaseApiVertical(buildRoot, rustBuildResult) {
  const runtimeRoot = resolve(buildRoot, 'api-production-runtime')
  const binaryRoot = resolve(runtimeRoot, 'bin')
  const fixtureRoot = resolve(runtimeRoot, 'fixture')
  rmSync(runtimeRoot, { recursive: true, force: true })
  mkdirSync(binaryRoot, { recursive: true })
  const paths = Object.fromEntries(rustBuildResult.rust.map(artifact => {
    const path = resolve(binaryRoot, artifact.binaryName)
    copyFileSync(artifact.sourcePath, path)
    chmodSync(path, 0o755)
    return [artifact.binaryName, path]
  }))
  const helperReleaseManifest = resolve(binaryRoot, HELPER_RELEASE_MANIFEST_NAME)
  copyFileSync(rustBuildResult.helperReleaseManifestPath, helperReleaseManifest)
  chmodSync(helperReleaseManifest, 0o644)
  writeApiProductionSourceSeal({
    root,
    serverBinary: paths['winwincode-server'],
    helperExecutable: paths['winwincode-kernel-helper'],
    helperReleaseManifest,
  })
  const report = JSON.parse(run(process.execPath, [
    'scripts/run-api-production-vertical.mjs',
    '--skip-build',
    '--server-binary', paths['winwincode-server'],
    '--worker-binary', paths['winwincode-worker'],
    '--directory', fixtureRoot,
  ], { capture: true, env: verificationChildEnvironment() }))
  assertReleaseApiReport(report)
}

const { target, sourceCommit, sourceDateEpoch, output } = parseArguments(process.argv.slice(2))
targetConfiguration(target)
const expectedReleaseSourceSha256 = assertCleanCommit(sourceCommit, sourceDateEpoch)
const { publicKeyHex: helperReleasePublicKeyHex } = releaseSigningConfiguration()

const buildBase = process.env.WWC_RELEASE_BUILD_ROOT
  ?? process.env.RUNNER_TEMP
  ?? tmpdir()
const buildRoot = mkdtempSync(resolve(buildBase, 'winwincode-release-'))
const cargoTarget = resolve(buildRoot, 'cargo-target')
const buildPaths = Object.freeze({
  buildRoot,
  targetDirectories: Object.freeze([cargoTarget]),
})
const clientSnapshot = resolve(buildRoot, 'client-primary')
const rustSnapshot = resolve(buildRoot, 'rust-primary')
const artifactRoot = resolve(output, target)

try {
  resetCargoTarget(cargoTarget)
  const primary = isolatedReleaseBuild({
    label: 'primary',
    target,
    targetDirectory: cargoTarget,
    sourceCommit,
    sourceDateEpoch,
    expectedReleaseSourceSha256,
    buildPaths,
  })
  cpSync(resolve(root, 'apps/client/dist/public'), clientSnapshot, {
    recursive: true,
    force: true,
  })
  const primaryRustSnapshot = snapshotRustBuild(primary.rust, rustSnapshot)
  resetCargoTarget(cargoTarget)
  const replay = isolatedReleaseBuild({
    label: 'replay',
    target,
    targetDirectory: cargoTarget,
    sourceCommit,
    sourceDateEpoch,
    expectedReleaseSourceSha256,
    buildPaths,
  })

  assertCleanCommit(sourceCommit, sourceDateEpoch, expectedReleaseSourceSha256)
  runReleaseApiVertical(buildRoot, primaryRustSnapshot)
  assertCleanCommit(sourceCommit, sourceDateEpoch, expectedReleaseSourceSha256)
  stageArtifacts(artifactRoot, primaryRustSnapshot, clientSnapshot)
  const manifest = createReleaseArtifactManifest({
    root,
    artifactRoot,
    target,
    sourceCommit,
    sourceDateEpoch,
    replayRust: replay.rust.rust.map(({
      packageName,
      binaryName,
      role,
      distribution,
      bytes,
      sha256,
      machoUuid,
    }) => ({
      packageName,
      binaryName,
      role,
      distribution,
      bytes,
      sha256,
      ...(targetConfiguration(target).os === 'macos' ? { machoUuid } : {}),
    })),
    replayClient: replay.client,
    helperReleasePublicKeyHex,
    replayHelperReleaseManifest: Object.freeze({
      bytes: replay.rust.helperReleaseManifest.bytes,
      sha256: replay.rust.helperReleaseManifest.sha256,
      publicKeyHex: helperReleasePublicKeyHex,
    }),
  })
  writeFileSync(resolve(artifactRoot, RELEASE_ARTIFACT_MANIFEST), canonicalJson(manifest))
  writeFileSync(resolve(artifactRoot, RELEASE_CHECKSUMS), releaseChecksums(manifest))
  verifyReleaseArtifactDirectory({
    root,
    artifactRoot,
    expectedCommit: sourceCommit,
    expectedTarget: target,
    expectedSourceDateEpoch: sourceDateEpoch,
  })
  assertCleanCommit(sourceCommit, sourceDateEpoch, expectedReleaseSourceSha256)
  process.stdout.write(`${canonicalJson({
    status: 'passed',
    target,
    sourceCommit,
    sourceDateEpoch,
    artifactRoot,
    manifest: RELEASE_ARTIFACT_MANIFEST,
    checksums: RELEASE_CHECKSUMS,
  })}`)
} finally {
  rmSync(buildRoot, { recursive: true, force: true })
}
