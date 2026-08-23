import { createHash } from 'node:crypto'
import { existsSync } from 'node:fs'
import { basename, join, resolve } from 'node:path'

import { scanRepositoryCpbBoundary } from './cpb-boundary-contract.mjs'
import {
  NATIVE_TARGETS,
  nativeTargetConfiguration,
  verifyNativePrebuild,
} from './native-package-contract.mjs'
import {
  NATIVE_RELEASE_REQUIRED_CHECKS,
  PRODUCT_COMMON_RELEASE_PACKAGE_DIRECTORIES,
  PRODUCT_RELEASE_SCHEMA_VERSION,
  fileDescriptor,
  productPackageManifests,
  projectRepositorySlug,
  readCanonicalJson,
  releaseSourceSha256,
  verifyReleaseLegalBoundary,
} from './release-source-contract.mjs'

export const NATIVE_RELEASE_EVIDENCE_KIND = 'winwincode.native-release-evidence'

const gitCommitPattern = /^[0-9a-f]{40}$/u

function immutable(value) {
  const clone = structuredClone(value)
  const pending = []
  if (typeof clone === 'object' && clone !== null) pending.push(clone)
  while (pending.length > 0) {
    const current = pending.pop()
    if (Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

export function jsonSha256(value) {
  return createHash('sha256').update(JSON.stringify(value)).digest('hex')
}

function nonEmpty(value, label) {
  if (typeof value !== 'string' || value.trim().length === 0) {
    throw new Error(`${label} is required`)
  }
  return value
}

function githubCiIdentity(environment, sourceCommit, expectedRepository) {
  if (environment.GITHUB_ACTIONS !== 'true') {
    throw new Error('native release evidence must be produced by GitHub Actions')
  }
  const commit = nonEmpty(environment.GITHUB_SHA, 'GITHUB_SHA')
  if (commit !== sourceCommit) throw new Error('GITHUB_SHA does not match --source-commit')
  const repository = nonEmpty(environment.GITHUB_REPOSITORY, 'GITHUB_REPOSITORY').toLowerCase()
  if (repository !== expectedRepository) {
    throw new Error('GITHUB_REPOSITORY does not match the project repository')
  }
  return Object.freeze({
    provider: 'github-actions',
    repository,
    commit,
    workflow: nonEmpty(environment.GITHUB_WORKFLOW, 'GITHUB_WORKFLOW'),
    runId: nonEmpty(environment.GITHUB_RUN_ID, 'GITHUB_RUN_ID'),
    runAttempt: nonEmpty(environment.GITHUB_RUN_ATTEMPT, 'GITHUB_RUN_ATTEMPT'),
    runnerOs: nonEmpty(environment.RUNNER_OS, 'RUNNER_OS'),
    runnerArch: nonEmpty(environment.RUNNER_ARCH, 'RUNNER_ARCH'),
  })
}

function assertSourcedMeasures(value, path = 'measures') {
  if (Array.isArray(value)) {
    value.forEach((entry, index) => assertSourcedMeasures(entry, `${path}[${String(index)}]`))
    return
  }
  if (typeof value !== 'object' || value === null) return
  for (const key of Object.keys(value)) {
    if (key.toLowerCase().includes('score')) {
      throw new Error(`${path}.${key} is an opaque score field`)
    }
  }
  if (Object.hasOwn(value, 'value')) {
    if (!Array.isArray(value.sourceRefs) || value.sourceRefs.length === 0) {
      throw new Error(`${path} has no source reference`)
    }
  }
  for (const [key, child] of Object.entries(value)) {
    assertSourcedMeasures(child, `${path}.${key}`)
  }
}

export function verifyPassingMeasuresProjection(measures, runKind) {
  if (measures?.schemaVersion !== 1
    || measures.runKind !== runKind
    || measures.runState !== 'completed'
    || typeof measures.runId !== 'string'
    || measures.runId.length === 0
    || typeof measures.deliveryId !== 'string'
    || !Number.isSafeInteger(measures.deliveryRevision)
    || measures.outcome?.classification?.value !== 'proven-success'
    || measures.outcome?.falseSuccessRisk?.value !== false
    || measures.outcome?.falseFailureRisk?.value !== false
    || measures.dimensions?.completeness?.status?.value !== 'complete'
    || measures.dimensions?.confidence?.status?.value !== 'independently-supported'
    || typeof measures.dimensions?.stability?.status?.value !== 'string'
    || typeof measures.dimensions?.humanDependence?.status?.value !== 'string'
    || !Number.isSafeInteger(measures.dimensions?.efficiency?.stageCount?.value)
    || !Number.isSafeInteger(measures.dimensions?.efficiency?.modelCallCount?.value)
    || !Number.isSafeInteger(measures.dimensions?.efficiency?.totalTokens?.value)) {
    throw new Error(`${runKind} Delivery evaluation did not produce supported completion`)
  }
  assertSourcedMeasures(measures)
  return immutable(measures)
}

export function verifyPassingDeterministicEvaluation(result) {
  if (typeof result !== 'object' || result === null || Array.isArray(result)) {
    throw new Error('deterministic evaluation result must be an object')
  }
  if (result.finalStatus !== 'delivered' || result.verdicts?.passed !== 'pass') {
    throw new Error('deterministic Delivery evaluation did not deliver a passing Verdict')
  }
  return verifyPassingMeasuresProjection(result.measures, 'deterministic')
}

function packageManifestByDirectory(root) {
  return new Map(productPackageManifests(root).map(entry => [entry.directory, entry.manifest]))
}

function verifiedReleasePackages(root, releaseDirectory, target) {
  const manifestPath = join(releaseDirectory, 'release-packages.json')
  const manifest = readCanonicalJson(manifestPath)
  if (manifest.schemaVersion !== PRODUCT_RELEASE_SCHEMA_VERSION || manifest.target !== target) {
    throw new Error('release-packages.json does not match the release target')
  }
  if (!Array.isArray(manifest.packages)) {
    throw new Error('release-packages.json has no package list')
  }
  const configuration = nativeTargetConfiguration(target)
  const manifests = packageManifestByDirectory(root)
  const expectedDirectories = [
    ...PRODUCT_COMMON_RELEASE_PACKAGE_DIRECTORIES,
    configuration.packageDirectory,
  ]
  const expected = expectedDirectories.map(directory => manifests.get(directory))
  const expectedNames = expected.map(value => value.name).sort()
  const actualNames = manifest.packages.map(value => value.name).sort()
  if (JSON.stringify(actualNames) !== JSON.stringify(expectedNames)) {
    throw new Error('release package names do not match the product package set')
  }
  const seenFiles = new Set()
  const packages = manifest.packages.map((entry) => {
    if (typeof entry.file !== 'string'
      || basename(entry.file) !== entry.file
      || !entry.file.endsWith('.tgz')
      || seenFiles.has(entry.file)) {
      throw new Error('release package has an invalid or duplicate file name')
    }
    seenFiles.add(entry.file)
    const expectedManifest = expected.find(value => value.name === entry.name)
    if (expectedManifest === undefined || entry.version !== expectedManifest.version) {
      throw new Error(`release package ${String(entry.name)} has the wrong version`)
    }
    const path = join(releaseDirectory, entry.file)
    if (!existsSync(path)) throw new Error(`release package ${entry.file} is missing`)
    const descriptor = fileDescriptor(path)
    if (descriptor.sha256 !== entry.sha256 || descriptor.bytes !== entry.bytes) {
      throw new Error(`release package ${entry.file} does not match its descriptor`)
    }
    return Object.freeze({
      name: entry.name,
      version: entry.version,
      file: entry.file,
      sha256: entry.sha256,
      bytes: entry.bytes,
    })
  })
  return Object.freeze({
    manifest: Object.freeze({
      file: 'release-packages.json',
      ...fileDescriptor(manifestPath),
    }),
    checksums: Object.freeze({
      file: 'SHA256SUMS',
      ...fileDescriptor(join(releaseDirectory, 'SHA256SUMS')),
    }),
    packages: Object.freeze(packages.toSorted((left, right) => left.name.localeCompare(right.name))),
  })
}

function expectedRunner(target) {
  const configuration = NATIVE_TARGETS.find(entry => entry.target === target)
  if (configuration === undefined) throw new Error(`unsupported native release target ${target}`)
  return Object.freeze({
    runnerOs: configuration.os === 'darwin' ? 'macOS' : 'Linux',
    runnerArch: configuration.cpu === 'arm64' ? 'ARM64' : 'X64',
  })
}

export function createNativeReleaseEvidence({
  root,
  target,
  releaseDirectory,
  sourceCommit,
  deterministicResult,
  environment = process.env,
}) {
  if (!gitCommitPattern.test(sourceCommit)) {
    throw new Error('source commit must be one full lowercase Git commit ID')
  }
  const workspace = readCanonicalJson(join(root, 'package.json'))
  const sourceLockPath = join(root, 'upstream', 'sources.lock.json')
  const sourceLock = readCanonicalJson(sourceLockPath)
  const repository = projectRepositorySlug(workspace.repository)
  if (repository === null) throw new Error('root repository is not a canonical GitHub URL')
  const ci = githubCiIdentity(environment, sourceCommit, repository)
  const runner = expectedRunner(target)
  if (ci.runnerOs !== runner.runnerOs || ci.runnerArch !== runner.runnerArch) {
    throw new Error('GitHub runner does not match the native release target')
  }
  const legalErrors = verifyReleaseLegalBoundary(root)
  if (legalErrors.length > 0) throw new Error(legalErrors.join('\n'))
  const cpbErrors = scanRepositoryCpbBoundary(root)
  if (cpbErrors.length > 0) throw new Error(cpbErrors.join('\n'))
  const native = verifyNativePrebuild({
    root,
    target,
    requireRelease: true,
    requireCurrentHost: true,
  })
  if (native.errors.length > 0) throw new Error(native.errors.join('\n'))
  const releaseRoot = resolve(releaseDirectory)
  const releasePackages = verifiedReleasePackages(root, releaseRoot, target)
  const measures = verifyPassingDeterministicEvaluation(deterministicResult)
  return immutable({
    schemaVersion: PRODUCT_RELEASE_SCHEMA_VERSION,
    kind: NATIVE_RELEASE_EVIDENCE_KIND,
    target,
    platformFamily: native.configuration.os,
    source: {
      repository: workspace.repository,
      repositorySlug: repository,
      commit: sourceCommit,
      version: workspace.version,
      license: workspace.license,
      releaseSourceSha256: releaseSourceSha256(root),
      rootPackage: fileDescriptor(join(root, 'package.json')),
      pnpmLock: fileDescriptor(join(root, 'pnpm-lock.yaml')),
      cargoLock: fileDescriptor(join(root, 'Cargo.lock')),
      rustToolchain: fileDescriptor(join(root, 'rust-toolchain.toml')),
      upstreamSourcesLock: fileDescriptor(sourceLockPath),
      codexCommit: sourceLock.codex.commit,
      dshCommit: sourceLock.dsh.commit,
    },
    native: {
      package: native.buildInfo.package,
      profile: native.buildInfo.profile,
      nativeInterfaceVersion: native.buildInfo.nativeInterfaceVersion,
      buildInfo: fileDescriptor(join(native.prebuildRoot, 'build-info.json')),
      artifacts: native.buildInfo.artifacts,
    },
    releasePackages,
    deterministicEvaluation: {
      runId: measures.runId,
      measuresSha256: jsonSha256(measures),
      measures,
    },
    checks: NATIVE_RELEASE_REQUIRED_CHECKS,
    boundaries: {
      externalProgrammingAgentRequired: false,
      cpbRuntimeRequired: false,
      projectLicense: 'Apache-2.0',
      thirdPartyNoticesPresent: true,
    },
    ci,
  })
}
