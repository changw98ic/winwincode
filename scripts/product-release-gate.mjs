import { createHash } from 'node:crypto'
import { existsSync, readFileSync } from 'node:fs'
import { basename, dirname, join, resolve } from 'node:path'

import { measureLiveEvaluationResult } from './evaluation-measures.mjs'
import {
  NATIVE_RELEASE_EVIDENCE_KIND,
  jsonSha256,
  verifyPassingMeasuresProjection,
} from './native-release-evidence.mjs'
import {
  NATIVE_TARGETS,
  nativeTargetConfiguration,
} from './native-package-contract.mjs'
import { scanRepositoryCpbBoundary } from './cpb-boundary-contract.mjs'
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

export const PRODUCT_RELEASE_GATE_KIND = 'winwincode.product-release-gate'

const gitCommitPattern = /^[0-9a-f]{40}$/u
const sha256Pattern = /^[0-9a-f]{64}$/u

export class ProductReleaseGateError extends Error {
  constructor(code, message) {
    super(message)
    this.name = 'ProductReleaseGateError'
    this.code = code
  }
}

function fail(code, message) {
  throw new ProductReleaseGateError(code, message)
}

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

function isRecord(value) {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function exactJson(left, right) {
  return JSON.stringify(left) === JSON.stringify(right)
}

function descriptorMatches(actual, expected) {
  return isRecord(actual)
    && actual.sha256 === expected.sha256
    && actual.bytes === expected.bytes
}

function validDescriptor(value) {
  return isRecord(value)
    && sha256Pattern.test(value.sha256)
    && Number.isSafeInteger(value.bytes)
    && value.bytes > 0
}

function currentSourceIdentity(root, expectedCommit) {
  const workspace = readCanonicalJson(join(root, 'package.json'))
  const sourceLockPath = join(root, 'upstream', 'sources.lock.json')
  const sourceLock = readCanonicalJson(sourceLockPath)
  const repositorySlug = projectRepositorySlug(workspace.repository)
  if (repositorySlug === null) fail('SOURCE_INVALID', 'project repository is not a GitHub URL')
  return Object.freeze({
    repository: workspace.repository,
    repositorySlug,
    commit: expectedCommit,
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
  })
}

function expectedPackageNames(root, target) {
  const byDirectory = new Map(productPackageManifests(root).map(entry => (
    [entry.directory, entry.manifest]
  )))
  const configuration = nativeTargetConfiguration(target)
  if (configuration === undefined) fail('NATIVE_EVIDENCE_INVALID', `unsupported target ${target}`)
  return Object.freeze([
    ...PRODUCT_COMMON_RELEASE_PACKAGE_DIRECTORIES,
    configuration.packageDirectory,
  ].map(directory => byDirectory.get(directory)?.name).sort())
}

function verifyPackageArtifacts(root, evidencePath, evidence) {
  const releaseRoot = dirname(evidencePath)
  const packages = evidence.releasePackages?.packages
  if (!Array.isArray(packages)) {
    fail('NATIVE_EVIDENCE_INVALID', `${evidencePath}: release package list is missing`)
  }
  const expectedNames = expectedPackageNames(root, evidence.target)
  if (!exactJson(packages.map(entry => entry.name).sort(), expectedNames)) {
    fail('NATIVE_EVIDENCE_INVALID', `${evidencePath}: release package names do not match`)
  }
  const seenFiles = new Set()
  for (const entry of packages) {
    if (!isRecord(entry)
      || typeof entry.file !== 'string'
      || basename(entry.file) !== entry.file
      || seenFiles.has(entry.file)
      || !sha256Pattern.test(entry.sha256)
      || !Number.isSafeInteger(entry.bytes)
      || entry.bytes < 1) {
      fail('NATIVE_EVIDENCE_INVALID', `${evidencePath}: release package descriptor is invalid`)
    }
    seenFiles.add(entry.file)
    const path = join(releaseRoot, entry.file)
    if (!existsSync(path) || !descriptorMatches(entry, fileDescriptor(path))) {
      fail('ARTIFACT_MISMATCH', `${evidencePath}: ${entry.file} does not match its SHA-256`)
    }
  }
  for (const key of ['manifest', 'checksums']) {
    const descriptor = evidence.releasePackages?.[key]
    if (!isRecord(descriptor)
      || typeof descriptor.file !== 'string'
      || basename(descriptor.file) !== descriptor.file) {
      fail('NATIVE_EVIDENCE_INVALID', `${evidencePath}: ${key} descriptor is invalid`)
    }
    const path = join(releaseRoot, descriptor.file)
    if (!existsSync(path) || !descriptorMatches(descriptor, fileDescriptor(path))) {
      fail('ARTIFACT_MISMATCH', `${evidencePath}: ${descriptor.file} does not match`)
    }
  }
  const checksumLines = readFileSync(
    join(releaseRoot, evidence.releasePackages.checksums.file),
    'utf8',
  ).trim().split('\n').sort()
  const expectedLines = packages.map(entry => `${entry.sha256}  ${entry.file}`).sort()
  if (!exactJson(checksumLines, expectedLines)) {
    fail('ARTIFACT_MISMATCH', `${evidencePath}: SHA256SUMS does not match package artifacts`)
  }
  return Object.freeze(packages)
}

function verifyNativeArtifactIdentity(evidencePath, evidence) {
  const configuration = nativeTargetConfiguration(evidence.target)
  if (configuration === undefined
    || evidence.platformFamily !== configuration.os
    || evidence.native?.package?.name !== configuration.packageName
    || evidence.native?.profile !== 'release'
    || evidence.native?.nativeInterfaceVersion !== 3
    || !validDescriptor(evidence.native?.buildInfo)) {
    fail('NATIVE_EVIDENCE_INVALID', `${evidencePath}: native identity is invalid`)
  }
  const expectedArtifacts = [
    'winwincode-kernel-helper',
    'winwincode_native.node',
    ...(configuration.os === 'linux'
      ? ['codex-linux-sandbox', 'codex-resources/bwrap']
      : []),
  ].sort()
  const artifacts = evidence.native.artifacts
  if (!isRecord(artifacts)
    || !exactJson(Object.keys(artifacts).sort(), expectedArtifacts)) {
    fail('NATIVE_EVIDENCE_INVALID', `${evidencePath}: native artifacts are incomplete`)
  }
  for (const descriptor of Object.values(artifacts)) {
    if (!isRecord(descriptor)
      || typeof descriptor.path !== 'string'
      || !sha256Pattern.test(descriptor.sha256)
      || !Number.isSafeInteger(descriptor.bytes)
      || descriptor.bytes < 1) {
      fail('NATIVE_EVIDENCE_INVALID', `${evidencePath}: native artifact descriptor is invalid`)
    }
  }
}

function verifyCiIdentity(evidencePath, evidence, source) {
  const configuration = nativeTargetConfiguration(evidence.target)
  const expectedOs = configuration.os === 'darwin' ? 'macOS' : 'Linux'
  const expectedArch = configuration.cpu === 'arm64' ? 'ARM64' : 'X64'
  const ci = evidence.ci
  if (!isRecord(ci)
    || ci.provider !== 'github-actions'
    || ci.repository !== source.repositorySlug
    || ci.commit !== source.commit
    || ci.runnerOs !== expectedOs
    || ci.runnerArch !== expectedArch
    || typeof ci.workflow !== 'string'
    || ci.workflow.length === 0
    || typeof ci.runId !== 'string'
    || ci.runId.length === 0
    || typeof ci.runAttempt !== 'string'
    || ci.runAttempt.length === 0) {
    fail('NATIVE_EVIDENCE_INVALID', `${evidencePath}: CI identity is invalid`)
  }
}

function verifyNativeEvidence(root, evidencePath, source) {
  const evidence = readCanonicalJson(evidencePath)
  if (evidence.schemaVersion !== PRODUCT_RELEASE_SCHEMA_VERSION
    || evidence.kind !== NATIVE_RELEASE_EVIDENCE_KIND
    || !NATIVE_TARGETS.some(entry => entry.target === evidence.target)) {
    fail('NATIVE_EVIDENCE_INVALID', `${evidencePath}: unsupported native release evidence`)
  }
  if (!exactJson(evidence.source, source)) {
    fail('SOURCE_MISMATCH', `${evidencePath}: source identity does not match this release`)
  }
  if (!exactJson(evidence.checks, NATIVE_RELEASE_REQUIRED_CHECKS)) {
    fail('CHECK_MISSING', `${evidencePath}: required native release checks are incomplete`)
  }
  if (evidence.boundaries?.externalProgrammingAgentRequired !== false
    || evidence.boundaries?.cpbRuntimeRequired !== false
    || evidence.boundaries?.projectLicense !== 'Apache-2.0'
    || evidence.boundaries?.thirdPartyNoticesPresent !== true) {
    fail('BOUNDARY_MISMATCH', `${evidencePath}: release boundaries are invalid`)
  }
  verifyCiIdentity(evidencePath, evidence, source)
  verifyNativeArtifactIdentity(evidencePath, evidence)
  const packages = verifyPackageArtifacts(root, evidencePath, evidence)
  const measures = verifyPassingMeasuresProjection(
    evidence.deterministicEvaluation?.measures,
    'deterministic',
  )
  if (evidence.deterministicEvaluation.measuresSha256 !== jsonSha256(measures)) {
    fail('EVALUATION_MISMATCH', `${evidencePath}: deterministic measures digest differs`)
  }
  return Object.freeze({ evidence, packages, measures })
}

function evaluatorSourceIdentities(root) {
  const paths = {
    runnerSha256: 'scripts/live-evaluation.mjs',
    cliSha256: 'scripts/run-live-evaluation.mjs',
    measuresAdapterSha256: 'scripts/evaluation-measures.mjs',
    measuresCliSha256: 'scripts/run-evaluation-measures.mjs',
    measuresProjectionSha256: 'packages/strongflow/src/evaluation-measures.ts',
    measuresRuntimeSha256: 'packages/strongflow/dist/evaluation-measures.js',
    preflightTestSha256: 'tests/delivery-full-keyless.test.mjs',
  }
  return Object.freeze(Object.fromEntries(Object.entries(paths).map(([key, path]) => (
    [key, fileDescriptor(join(root, path)).sha256]
  ))))
}

function verifyLiveSourceIdentity(root, path, result, source) {
  const identities = result.sourceIdentities
  const expectedEvaluator = evaluatorSourceIdentities(root)
  if (!isRecord(identities)
    || identities.evaluator?.schemaVersion !== 1
    || identities.evaluator?.projectionVersion !== 2
    || !Object.entries(expectedEvaluator).every(([key, value]) => (
      identities.evaluator[key] === value
    ))) {
    fail('LIVE_EVALUATION_STALE', `${path}: evaluator source identity is stale`)
  }
  if (identities.project?.repository !== source.repository
    || identities.project?.version !== source.version
    || identities.project?.releaseSourceSha256 !== source.releaseSourceSha256
    || !descriptorMatches(identities.project?.rootPackage, source.rootPackage)
    || !descriptorMatches(identities.project?.pnpmLock, source.pnpmLock)
    || !descriptorMatches(identities.project?.upstreamSourcesLock, source.upstreamSourcesLock)
    || identities.codex?.commit !== source.codexCommit
    || identities.dsh?.commit !== source.dshCommit) {
    fail('LIVE_EVALUATION_STALE', `${path}: project or upstream source identity is stale`)
  }
}

function verifyLiveEvaluation(root, path, source, nativeTargets) {
  const result = readCanonicalJson(path)
  if (result.schemaVersion !== 1
    || result.projectionVersion !== 2
    || result.state !== 'completed'
    || result.phase !== 'completed'
    || result.error !== null
    || result.preflight?.status !== 'passed'
    || result.budget?.violation !== null
    || result.delivery?.status !== 'delivered'
    || result.delivery?.verdict?.status !== 'pass'
    || result.candidate?.candidateRef !== result.delivery?.verdict?.candidateRef
    || result.runtimeProjection?.deliveryId !== result.delivery?.id
    || result.runtimeProjection?.deliveryRevision !== result.delivery?.revision) {
    fail('LIVE_EVALUATION_FAILED', `${path}: live Delivery result is not a passing current run`)
  }
  verifyLiveSourceIdentity(root, path, result, source)
  if (!nativeTargets.has(result.sourceIdentities.native?.target)) {
    fail('LIVE_EVALUATION_STALE', `${path}: native target is absent from release evidence`)
  }
  const recomputed = measureLiveEvaluationResult(result)
  if (!exactJson(recomputed, result.measures)) {
    fail('EVALUATION_MISMATCH', `${path}: saved live measures do not reproduce`)
  }
  const measures = verifyPassingMeasuresProjection(recomputed, 'live')
  if (measures.dimensions.efficiency.missingUsageCallCount.value !== 0
    || measures.dimensions.efficiency.modelCallCount.value < 1
    || measures.dimensions.humanDependence.openBlockingAttentionCount.value !== 0) {
    fail('LIVE_EVALUATION_FAILED', `${path}: live usage or human decisions are incomplete`)
  }
  return Object.freeze({ result, measures })
}

/** Verify four target artifacts and current live Delivery evidence without publishing. */
export function createProductReleaseGateReport({
  root,
  expectedCommit,
  nativeEvidencePaths,
  liveEvaluationPaths,
}) {
  const repositoryRoot = resolve(root)
  if (!gitCommitPattern.test(expectedCommit)) {
    fail('INVALID_INPUT', 'expectedCommit must be one full lowercase Git commit ID')
  }
  if (!Array.isArray(nativeEvidencePaths)
    || !Array.isArray(liveEvaluationPaths)
    || liveEvaluationPaths.length === 0) {
    fail('INVALID_INPUT', 'four native evidence files and at least one live result are required')
  }
  const legalErrors = verifyReleaseLegalBoundary(repositoryRoot)
  if (legalErrors.length > 0) fail('LEGAL_BOUNDARY_FAILED', legalErrors.join('\n'))
  const cpbErrors = scanRepositoryCpbBoundary(repositoryRoot)
  if (cpbErrors.length > 0) fail('DESIGN_BOUNDARY_FAILED', cpbErrors.join('\n'))
  const source = currentSourceIdentity(repositoryRoot, expectedCommit)
  const native = nativeEvidencePaths.map(path => verifyNativeEvidence(
    repositoryRoot,
    resolve(path),
    source,
  ))
  const expectedTargets = NATIVE_TARGETS.map(entry => entry.target).sort()
  const actualTargets = native.map(entry => entry.evidence.target).sort()
  if (!exactJson(actualTargets, expectedTargets)) {
    fail('NATIVE_MATRIX_INCOMPLETE', 'native evidence must contain each supported target exactly once')
  }
  const nativeTargets = new Set(actualTargets)
  const live = liveEvaluationPaths.map(path => verifyLiveEvaluation(
    repositoryRoot,
    resolve(path),
    source,
    nativeTargets,
  ))
  const nativeSummaries = native.map(({ evidence, packages, measures }, index) => Object.freeze({
    target: evidence.target,
    platformFamily: evidence.platformFamily,
    evidenceSha256: fileDescriptor(resolve(nativeEvidencePaths[index])).sha256,
    ci: evidence.ci,
    native: evidence.native,
    packages,
    deterministicEvaluation: {
      runId: measures.runId,
      measuresSha256: evidence.deterministicEvaluation.measuresSha256,
      outcome: measures.outcome.classification,
      completeness: measures.dimensions.completeness.status,
      confidence: measures.dimensions.confidence.status,
    },
  })).toSorted((left, right) => left.target.localeCompare(right.target))
  const liveSummaries = live.map(({ result, measures }, index) => Object.freeze({
    runId: result.runId,
    resultSha256: fileDescriptor(resolve(liveEvaluationPaths[index])).sha256,
    nativeTarget: result.sourceIdentities.native.target,
    deliveryId: result.delivery.id,
    deliveryRevision: result.delivery.revision,
    deliverySpecId: result.delivery.spec.id,
    deliverySpecRevision: result.delivery.spec.revision,
    candidateRef: result.candidate.candidateRef,
    verdictId: result.delivery.verdict.id,
    measuresSha256: jsonSha256(measures),
    measures,
  })).toSorted((left, right) => left.runId.localeCompare(right.runId))
  return immutable({
    schemaVersion: PRODUCT_RELEASE_SCHEMA_VERSION,
    kind: PRODUCT_RELEASE_GATE_KIND,
    status: 'passed',
    source,
    nativeTargets: nativeSummaries,
    platformFamilies: ['darwin', 'linux'],
    evaluations: {
      deterministic: nativeSummaries.map(entry => entry.deterministicEvaluation),
      live: liveSummaries,
    },
    boundaries: {
      executionAuthority: 'embedded-codex-core',
      interactionShell: 'deepseek-harness',
      deliveryAuthority: 'winwincode-delivery',
      externalProgrammingAgentRequired: false,
      cpbRuntimeRequired: false,
      projectLicense: 'Apache-2.0',
      thirdPartyNoticesPresent: true,
    },
  })
}

export function productReleaseReportSha256(report) {
  return createHash('sha256').update(JSON.stringify(report)).digest('hex')
}
