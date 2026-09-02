import assert from 'node:assert/strict'
import { readFile, readdir } from 'node:fs/promises'
import { join, relative, resolve, sep } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const differentialTestPath = 'tests/delivery-strongflow-differential-oracle.test.mjs'
const exporterPath = 'scripts/export-delivery-strongflow-oracle.mjs'
const liveEvaluationTestPath = 'tests/live-evaluation-runner.test.mjs'
const runnerPath = 'scripts/run-ts-tests.mjs'

async function source(path) {
  return readFile(join(root, path), 'utf8')
}

async function modulesBelow(path) {
  const directory = join(root, path)
  const entries = await readdir(directory, { withFileTypes: true })
  const modules = []
  for (const entry of entries) {
    const absolute = join(directory, entry.name)
    if (entry.isDirectory()) {
      modules.push(...await modulesBelow(relative(root, absolute)))
    } else if (entry.isFile() && entry.name.endsWith('.mjs')) {
      modules.push(relative(root, absolute).split(sep).join('/'))
    }
  }
  return modules
}

test('parallel Delivery oracle tests read the committed baseline and run only the replay tracer', async () => {
  const testSource = await source(differentialTestPath)
  const builderCalls = [
    ...testSource.matchAll(/\bbuildLegacyDeliveryStrongFlowOracle\s*\(([^)]*)\)/gu),
  ]

  assert.equal(builderCalls.length, 1)
  assert.match(builderCalls[0][1], /scenarioIds:\s*\['request-id-replay'\]/u)
  assert.doesNotMatch(testSource, /\bfullOracle(?:Promise)?\b/u)
  assert.match(testSource, /readCommittedOracle/u)
})

test('TypeScript test runner checks the full oracle after the Node test suite without rebuilding', async () => {
  const [runnerSource, manifestSource] = await Promise.all([
    source(runnerPath),
    source('package.json'),
  ])
  const manifest = JSON.parse(manifestSource)
  const parallelSuite = "runTests(['--test', '--test-concurrency=4', ...parallelTests])"
  const serialSuite = "for (const path of serialProcessTests) runTests(['--test', path])"
  const oracleCheck = `runTests(['${exporterPath}', '--check'])`

  assert.equal(
    manifest.scripts['test:ts'],
    'pnpm build:ts && pnpm build:native && node scripts/run-ts-tests.mjs',
  )
  assert.equal(runnerSource.includes(parallelSuite), true)
  assert.equal(runnerSource.includes(serialSuite), true)
  assert.equal(runnerSource.includes(oracleCheck), true)
  assert.equal(runnerSource.indexOf(oracleCheck) > runnerSource.indexOf(parallelSuite), true)
  assert.equal(runnerSource.indexOf(oracleCheck) > runnerSource.indexOf(serialSuite), true)
  assert.doesNotMatch(runnerSource, /\b(?:pnpm|build:ts)\b/u)
  assert.doesNotMatch(runnerSource, /oracle:delivery:check/u)
})

test('TypeScript test runner isolates the process-boundary live evaluation lane', async () => {
  const runnerSource = await source(runnerPath)

  assert.match(
    runnerSource,
    new RegExp(`serialProcessTests = Object\\.freeze\\(\\[[^\\]]*'${liveEvaluationTestPath}'`, 'u'),
  )
  assert.match(
    runnerSource,
    /parallelTests = testFiles\.filter\(path => !serialProcessTests\.includes\(path\)\)/u,
  )
  assert.match(
    runnerSource,
    /for \(const path of serialProcessTests\) runTests\(\['--test', path\]\)/u,
  )
})

test('the exporter remains the only source that builds the full committed oracle', async () => {
  const modules = [
    ...await modulesBelow('scripts'),
    ...await modulesBelow('tests'),
  ]
  const builderCall = /(?<!function\s)\bbuildLegacyDeliveryStrongFlowOracle\s*\(/gu
  const fullBuilderCall = /(?<!function\s)\bbuildLegacyDeliveryStrongFlowOracle\s*\(\s*\)/gu
  const builderConsumers = []
  const fullBuilderSources = []

  for (const path of modules) {
    const moduleSource = await source(path)
    if (builderCall.test(moduleSource)) builderConsumers.push(path)
    builderCall.lastIndex = 0
    if (fullBuilderCall.test(moduleSource)) fullBuilderSources.push(path)
    fullBuilderCall.lastIndex = 0
  }

  assert.deepEqual(builderConsumers, [exporterPath, differentialTestPath])
  assert.deepEqual(fullBuilderSources, [exporterPath])

  const [exporterSource, manifestSource] = await Promise.all([
    source(exporterPath),
    source('package.json'),
  ])
  const manifest = JSON.parse(manifestSource)
  assert.match(exporterSource, /await buildLegacyDeliveryStrongFlowOracle\(\)/u)
  assert.match(exporterSource, /writeFile\(outputPath, serialized\)/u)
  assert.equal(
    manifest.scripts['oracle:delivery:export'],
    'pnpm build:ts && node scripts/export-delivery-strongflow-oracle.mjs --write',
  )
})
