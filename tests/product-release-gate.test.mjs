import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import { measureLiveEvaluationResult } from '../scripts/evaluation-measures.mjs'
import {
  NATIVE_RELEASE_EVIDENCE_KIND,
  jsonSha256,
} from '../scripts/native-release-evidence.mjs'
import {
  PRODUCT_RELEASE_GATE_KIND,
  ProductReleaseGateError,
  createProductReleaseGateReport,
} from '../scripts/product-release-gate.mjs'
import {
  NATIVE_RELEASE_REQUIRED_CHECKS,
  PRODUCT_COMMON_RELEASE_PACKAGE_DIRECTORIES,
  PRODUCT_RELEASE_SCHEMA_VERSION,
  fileDescriptor,
  productPackageManifests,
  projectRepositorySlug,
  readCanonicalJson,
  releaseSourcePaths,
  releaseSourceSha256,
} from '../scripts/release-source-contract.mjs'
import {
  NATIVE_TARGETS,
  nativeTargetConfiguration,
} from '../scripts/native-package-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const sourceCommit = '1234567890abcdef1234567890abcdef12345678'
const credentialNamePattern = /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu

function scenarioResult() {
  const environment = Object.fromEntries(Object.entries(process.env).filter(([name]) => (
    !credentialNamePattern.test(name)
  )))
  const result = spawnSync(process.execPath, [
    'tests/fixtures/full-delivery-scenario.mjs',
  ], {
    cwd: root,
    env: environment,
    encoding: 'utf8',
    maxBuffer: 64 * 1_024 * 1_024,
  })
  assert.equal(result.status, 0, result.stderr)
  const line = result.stdout.split('\n').find(value => value.trim().startsWith('{'))
  assert.notEqual(line, undefined)
  return JSON.parse(line)
}

function sourceIdentity() {
  const workspace = readCanonicalJson(join(root, 'package.json'))
  const sourceLockPath = join(root, 'upstream', 'sources.lock.json')
  const sourceLock = readCanonicalJson(sourceLockPath)
  return Object.freeze({
    repository: workspace.repository,
    repositorySlug: projectRepositorySlug(workspace.repository),
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
  })
}

function nativeArtifactDescriptors(target) {
  const configuration = nativeTargetConfiguration(target)
  const names = [
    'winwincode-kernel-helper',
    'winwincode_native.node',
    ...(configuration.os === 'linux'
      ? ['codex-linux-sandbox', 'codex-resources/bwrap']
      : []),
  ]
  return Object.fromEntries(names.map((name, index) => [name, {
    path: name,
    sha256: String(index + 1).padStart(64, '0'),
    bytes: index + 1,
  }]))
}

async function nativeEvidenceFixture(directory, target, source, deterministicMeasures) {
  const configuration = nativeTargetConfiguration(target)
  const targetRoot = join(directory, target)
  await mkdir(targetRoot, { recursive: true })
  const manifestByDirectory = new Map(productPackageManifests(root).map(entry => (
    [entry.directory, entry.manifest]
  )))
  const directories = [
    ...PRODUCT_COMMON_RELEASE_PACKAGE_DIRECTORIES,
    configuration.packageDirectory,
  ]
  const packages = []
  for (const packageDirectory of directories) {
    const manifest = manifestByDirectory.get(packageDirectory)
    const file = `${manifest.name.replace('@', '').replace('/', '-')}-${target}.tgz`
    const path = join(targetRoot, file)
    await writeFile(path, `fixture package ${manifest.name} ${target}\n`)
    packages.push({
      name: manifest.name,
      version: manifest.version,
      file,
      ...fileDescriptor(path),
    })
  }
  packages.sort((left, right) => left.name.localeCompare(right.name))
  const releaseManifestPath = join(targetRoot, 'release-packages.json')
  await writeFile(releaseManifestPath, `${JSON.stringify({
    schemaVersion: PRODUCT_RELEASE_SCHEMA_VERSION,
    target,
    packages,
  }, null, 2)}\n`)
  const checksumsPath = join(targetRoot, 'SHA256SUMS')
  await writeFile(
    checksumsPath,
    `${packages.map(entry => `${entry.sha256}  ${entry.file}`).sort().join('\n')}\n`,
  )
  const runnerOs = configuration.os === 'darwin' ? 'macOS' : 'Linux'
  const runnerArch = configuration.cpu === 'arm64' ? 'ARM64' : 'X64'
  const evidence = {
    schemaVersion: PRODUCT_RELEASE_SCHEMA_VERSION,
    kind: NATIVE_RELEASE_EVIDENCE_KIND,
    target,
    platformFamily: configuration.os,
    source,
    native: {
      package: {
        name: configuration.packageName,
        version: source.version,
      },
      profile: 'release',
      nativeInterfaceVersion: 3,
      buildInfo: {
        sha256: 'f'.repeat(64),
        bytes: 100,
      },
      artifacts: nativeArtifactDescriptors(target),
    },
    releasePackages: {
      manifest: {
        file: 'release-packages.json',
        ...fileDescriptor(releaseManifestPath),
      },
      checksums: {
        file: 'SHA256SUMS',
        ...fileDescriptor(checksumsPath),
      },
      packages,
    },
    deterministicEvaluation: {
      runId: deterministicMeasures.runId,
      measuresSha256: jsonSha256(deterministicMeasures),
      measures: deterministicMeasures,
    },
    checks: NATIVE_RELEASE_REQUIRED_CHECKS,
    boundaries: {
      externalProgrammingAgentRequired: false,
      cpbRuntimeRequired: false,
      projectLicense: 'Apache-2.0',
      thirdPartyNoticesPresent: true,
    },
    ci: {
      provider: 'github-actions',
      repository: source.repositorySlug,
      commit: source.commit,
      workflow: 'Native release matrix',
      runId: `run-${target}`,
      runAttempt: '1',
      runnerOs,
      runnerArch,
    },
  }
  const evidencePath = join(targetRoot, 'native-release-evidence.json')
  await writeFile(evidencePath, `${JSON.stringify(evidence, null, 2)}\n`)
  return Object.freeze({ evidencePath, evidence, targetRoot })
}

function evaluatorIdentities() {
  return {
    schemaVersion: 1,
    projectionVersion: 2,
    runnerSha256: fileDescriptor(join(root, 'scripts/live-evaluation.mjs')).sha256,
    cliSha256: fileDescriptor(join(root, 'scripts/run-live-evaluation.mjs')).sha256,
    measuresAdapterSha256: fileDescriptor(join(root, 'scripts/evaluation-measures.mjs')).sha256,
    measuresCliSha256: fileDescriptor(join(root, 'scripts/run-evaluation-measures.mjs')).sha256,
    measuresProjectionSha256: fileDescriptor(
      join(root, 'packages/strongflow/src/evaluation-measures.ts'),
    ).sha256,
    measuresRuntimeSha256: fileDescriptor(
      join(root, 'packages/strongflow/dist/evaluation-measures.js'),
    ).sha256,
    preflightTestSha256: fileDescriptor(
      join(root, 'tests/delivery-full-keyless.test.mjs'),
    ).sha256,
  }
}

async function liveEvaluationFixture(directory, source, scenario, nativeTarget) {
  const fixture = scenario.releaseGateFixture
  const startedAtMillis = fixture.delivery.createdAtMillis
  const finishedAtMillis = fixture.delivery.updatedAtMillis
  const sourceLock = readCanonicalJson(join(root, 'upstream/sources.lock.json'))
  const result = {
    schemaVersion: 1,
    runId: 'live-release-gate-fixture',
    state: 'completed',
    phase: 'completed',
    startedAtMillis,
    finishedAtMillis,
    projectionVersion: 2,
    preflight: { status: 'passed' },
    budget: {
      limits: { pricing: { source: 'fixture live pricing' } },
      modelCalls: fixture.modelCalls.length,
      calls: fixture.modelCalls.map((call, index) => ({
        index: index + 1,
        status: 'completed',
        startedAtMillis: startedAtMillis + index,
        finishedAtMillis: startedAtMillis + index + 1,
        usage: {
          inputTokens: call.usage.inputTokens,
          outputTokens: call.usage.outputTokens,
          cacheReadTokens: call.usage.cacheReadTokens ?? 0,
          cacheWriteTokens: call.usage.cacheWriteTokens ?? 0,
        },
        costUsdMicros: index + 1,
      })),
      violation: null,
    },
    delivery: fixture.delivery,
    candidate: fixture.candidate,
    runtimeProjection: fixture.runtimeProjection,
    sourceIdentities: {
      project: {
        repository: source.repository,
        version: source.version,
        releaseSourceSha256: source.releaseSourceSha256,
        rootPackage: source.rootPackage,
        pnpmLock: source.pnpmLock,
        upstreamSourcesLock: source.upstreamSourcesLock,
      },
      evaluator: evaluatorIdentities(),
      codex: { commit: sourceLock.codex.commit },
      dsh: { commit: sourceLock.dsh.commit },
      native: { target: nativeTarget },
    },
    error: null,
    measures: null,
  }
  result.measures = measureLiveEvaluationResult(result)
  const path = join(directory, 'live-result.json')
  await writeFile(path, `${JSON.stringify(result, null, 2)}\n`)
  return Object.freeze({ path, result })
}

test('release source identity excludes generated output and remains deterministic', () => {
  const paths = releaseSourcePaths(root)
  assert.equal(paths.some(path => path.includes('/dist/')), false)
  assert.equal(paths.some(path => path.includes('/prebuild/')), false)
  assert.equal(paths.some(path => path.startsWith('third_party/codex/')), false)
  assert.match(releaseSourceSha256(root), /^[0-9a-f]{64}$/u)
  assert.equal(releaseSourceSha256(root), releaseSourceSha256(root))
})

test('requires four current native targets and one reproducible passing live Delivery', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'winwincode-product-release-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const scenario = scenarioResult()
  const source = sourceIdentity()
  const native = []
  for (const { target } of NATIVE_TARGETS) {
    native.push(await nativeEvidenceFixture(directory, target, source, scenario.measures))
  }
  const live = await liveEvaluationFixture(
    directory,
    source,
    scenario,
    'aarch64-apple-darwin',
  )
  const input = {
    root,
    expectedCommit: sourceCommit,
    nativeEvidencePaths: native.map(entry => entry.evidencePath),
    liveEvaluationPaths: [live.path],
  }
  const first = createProductReleaseGateReport(input)
  const second = createProductReleaseGateReport(input)
  assert.deepEqual(first, second)
  assert.equal(first.kind, PRODUCT_RELEASE_GATE_KIND)
  assert.equal(first.status, 'passed')
  assert.deepEqual(first.platformFamilies, ['darwin', 'linux'])
  assert.deepEqual(
    first.nativeTargets.map(entry => entry.target),
    NATIVE_TARGETS.map(entry => entry.target).sort(),
  )
  assert.equal(first.evaluations.deterministic.length, 4)
  assert.equal(first.evaluations.live.length, 1)
  assert.equal(first.evaluations.live[0].measures.outcome.falseSuccessRisk.value, false)
  assert.equal(first.evaluations.live[0].measures.outcome.falseFailureRisk.value, false)
  assert.equal(first.boundaries.externalProgrammingAgentRequired, false)
  assert.equal(first.boundaries.cpbRuntimeRequired, false)
  assert.equal(first.boundaries.projectLicense, 'Apache-2.0')
  assert.equal(JSON.stringify(first).toLowerCase().includes('overallscore'), false)

  const output = join(directory, 'product-release-gate.json')
  const cliArguments = [
    'scripts/run-product-release-gate.mjs',
    '--expected-commit',
    sourceCommit,
    '--native-evidence',
    directory,
    '--live-evaluation',
    live.path,
    '--output',
    output,
  ]
  const cli = spawnSync(process.execPath, cliArguments, {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(cli.status, 0, cli.stderr)
  assert.deepEqual(JSON.parse(await readFile(output, 'utf8')), first)
  const replayedCli = spawnSync(process.execPath, cliArguments, {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(replayedCli.status, 0, replayedCli.stderr)

  assert.throws(
    () => createProductReleaseGateReport({
      ...input,
      nativeEvidencePaths: input.nativeEvidencePaths.slice(1),
    }),
    error => error instanceof ProductReleaseGateError
      && error.code === 'NATIVE_MATRIX_INCOMPLETE',
  )

  const alteredPackage = native[0].evidence.releasePackages.packages[0]
  const alteredPath = join(native[0].targetRoot, alteredPackage.file)
  const originalArtifact = await readFile(alteredPath)
  await writeFile(alteredPath, 'changed release artifact\n')
  assert.throws(
    () => createProductReleaseGateReport(input),
    error => error instanceof ProductReleaseGateError && error.code === 'ARTIFACT_MISMATCH',
  )
  await writeFile(alteredPath, originalArtifact)

  await writeFile(
    alteredPath,
    'release payload contains sk-fixturecredentialleakgate1234567890\n',
  )
  assert.throws(
    () => createProductReleaseGateReport(input),
    error => error instanceof ProductReleaseGateError
      && error.code === 'CREDENTIAL_LEAK_DETECTED'
      && !error.message.includes('sk-fixturecredentialleakgate1234567890'),
  )
  await writeFile(alteredPath, originalArtifact)

  const originalNativeEvidence = await readFile(native[0].evidencePath, 'utf8')
  const missingCheck = JSON.parse(originalNativeEvidence)
  missingCheck.checks = missingCheck.checks.slice(1)
  await writeFile(native[0].evidencePath, `${JSON.stringify(missingCheck, null, 2)}\n`)
  assert.throws(
    () => createProductReleaseGateReport(input),
    error => error instanceof ProductReleaseGateError && error.code === 'CHECK_MISSING',
  )
  await writeFile(native[0].evidencePath, originalNativeEvidence)

  const staleSource = JSON.parse(originalNativeEvidence)
  staleSource.source.releaseSourceSha256 = '0'.repeat(64)
  await writeFile(native[0].evidencePath, `${JSON.stringify(staleSource, null, 2)}\n`)
  assert.throws(
    () => createProductReleaseGateReport(input),
    error => error instanceof ProductReleaseGateError && error.code === 'SOURCE_MISMATCH',
  )
  await writeFile(native[0].evidencePath, originalNativeEvidence)

  const originalLive = await readFile(live.path, 'utf8')
  const staleLive = JSON.parse(originalLive)
  staleLive.candidate.candidateRef = 'different-candidate'
  await writeFile(live.path, `${JSON.stringify(staleLive, null, 2)}\n`)
  assert.throws(
    () => createProductReleaseGateReport(input),
    error => error instanceof ProductReleaseGateError
      && error.code === 'LIVE_EVALUATION_FAILED',
  )
  await writeFile(live.path, originalLive)

  const changedMeasures = JSON.parse(originalLive)
  changedMeasures.measures.outcome.falseSuccessRisk.value = true
  await writeFile(live.path, `${JSON.stringify(changedMeasures, null, 2)}\n`)
  assert.throws(
    () => createProductReleaseGateReport(input),
    error => error instanceof ProductReleaseGateError
      && error.code === 'EVALUATION_MISMATCH',
  )
})

test('product release CLI requires explicit artifact, evaluation, commit, and output inputs', () => {
  const result = spawnSync(process.execPath, ['scripts/run-product-release-gate.mjs'], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(result.status, 2)
  assert.match(result.stderr, /all release gate inputs are required/u)
})
