#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { readFileSync, readdirSync } from 'node:fs'
import { extname, join, resolve } from 'node:path'

const RULES_PATH = 'docs/contracts/provider-dsh-rust-differential.rules.json'

function fail(message) {
  throw new Error(message)
}

function object(value, label) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    fail(`${label} must be an object`)
  }
  return value
}

function array(value, label) {
  if (!Array.isArray(value)) fail(`${label} must be an array`)
  return value
}

function exactKeys(value, expected, label) {
  object(value, label)
  const actual = Object.keys(value).toSorted()
  if (JSON.stringify(actual) !== JSON.stringify([...expected].toSorted())) {
    fail(`${label} keys differ`)
  }
}

function nonEmpty(value, label) {
  if (typeof value !== 'string' || value.length === 0) fail(`${label} must be a string`)
}

function unique(values, label) {
  if (new Set(values).size !== values.length) fail(`${label} must be unique`)
}

function sourceFilesBelow(root, directory) {
  const paths = []
  for (const entry of readdirSync(join(root, directory), { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) {
      if (!['dist', 'node_modules', 'target'].includes(entry.name)) {
        paths.push(...sourceFilesBelow(root, path))
      }
    } else if (['.md', '.mjs', '.rs', '.ts'].includes(extname(entry.name))) {
      paths.push(path)
    }
  }
  return paths
}

function rustArguments(evidence) {
  return [
    'test',
    '-p',
    evidence.package,
    '--test',
    evidence.binary,
    evidence.testName,
    '--locked',
    '--offline',
    '--',
    '--exact',
  ]
}

export function assertRustTestSource(source, testName) {
  const escaped = testName.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&')
  const declaration = new RegExp(`(?:async\\s+)?fn\\s+${escaped}\\s*\\(\\s*\\)`, 'gu')
  const matches = [...source.matchAll(declaration)]
  if (matches.length !== 1) fail(`${testName} must name one Rust test`)
  const prefix = source.slice(Math.max(0, matches[0].index - 1_000), matches[0].index)
  const attributes = []
  for (const line of prefix.split('\n').reverse()) {
    const value = line.trim()
    if (value === '' || value.startsWith('//')) continue
    if (value.startsWith('#[')) {
      attributes.push(value)
      continue
    }
    break
  }
  if (!attributes.some(attribute => (
    /^#\[(?:[A-Za-z_][A-Za-z0-9_]*::)*test(?:\([^\]]*\))?\]$/u.test(attribute)
  ))) {
    fail(`${testName} is not a Rust test`)
  }
  if (attributes.some(attribute => /\bignore\b/u.test(attribute))) {
    fail(`${testName} must not be ignored`)
  }
}

function assertRustTest(root, evidence) {
  const source = readFileSync(join(root, evidence.sourcePath), 'utf8')
  assertRustTestSource(source, evidence.testName)
}

export function assertNodeTest(source, testName) {
  for (const modifier of ['only', 'skip', 'todo']) {
    if (
      source.includes(`test.${modifier}('${testName}'`)
      || source.includes(`test.${modifier}(\`${testName}\``)
    ) {
      fail(`${testName} must not use test.${modifier}`)
    }
  }
  const single = `test('${testName}'`
  const template = `test(\`${testName}\``
  let count = source.split(single).length - 1 + source.split(template).length - 1
  const providerFamily = /^streams Codex output through the DSH (deepseek|anthropic) provider family$/u
    .exec(testName)
  if (
    count === 0
    && providerFamily !== null
    && source.includes('test(`streams Codex output through the DSH ${route.provider} provider family`')
    && source.includes(`provider: '${providerFamily[1]}'`)
  ) {
    count = 1
  }
  if (count !== 1) fail(`${testName} must name one DSH test`)
}

export function validateProviderDifferential({ fixture, root, rules }) {
  exactKeys(rules, [
    'approvedDifferences',
    'dshBaseline',
    'dshSourceEvidence',
    'executionPolicy',
    'fixturePath',
    'issueId',
    'normalization',
    'productionIsolation',
    'rustEvidence',
    'rustUnitEvidence',
    'schemaVersion',
  ], 'rules')
  if (rules.schemaVersion !== 'winwincode.provider-dsh-rust-differential-rules.v1') {
    fail('rules schemaVersion differs')
  }
  if (rules.issueId !== 'winwincode-9c4.16.5.6') fail('rules issueId differs')
  if (rules.fixturePath !== 'tests/fixtures/provider-dsh-rust-differential.v1.json') {
    fail('rules fixturePath differs')
  }
  exactKeys(fixture, ['scenarios', 'schemaVersion'], 'fixture')
  if (fixture.schemaVersion !== 'winwincode.provider-dsh-rust-differential-fixture.v1') {
    fail('fixture schemaVersion differs')
  }

  exactKeys(rules.executionPolicy, [
    'attemptsPerCommand',
    'commandOrder',
    'exactCargoFilterRequired',
    'ignoredTestsAllowed',
    'networkAllowed',
    'unexpectedDifferencePolicy',
    'wildcardAllowed',
  ], 'executionPolicy')
  const expectedPolicy = {
    attemptsPerCommand: 1,
    commandOrder: 'dsh-baseline-then-rust-evidence-as-listed',
    exactCargoFilterRequired: true,
    ignoredTestsAllowed: false,
    networkAllowed: false,
    unexpectedDifferencePolicy: 'reject',
    wildcardAllowed: false,
  }
  if (JSON.stringify(rules.executionPolicy) !== JSON.stringify(expectedPolicy)) {
    fail('executionPolicy differs')
  }

  const scenarios = array(fixture.scenarios, 'fixture.scenarios')
  const scenarioIds = scenarios.map((scenario, index) => {
    exactKeys(scenario, ['expectedFacts', 'id', 'input'], `fixture.scenarios[${index}]`)
    nonEmpty(scenario.id, `fixture.scenarios[${index}].id`)
    object(scenario.input, `fixture.scenarios[${index}].input`)
    const facts = array(scenario.expectedFacts, `fixture.scenarios[${index}].expectedFacts`)
    if (facts.length === 0) fail(`${scenario.id} must freeze at least one fact`)
    facts.forEach((fact, factIndex) => nonEmpty(fact, `${scenario.id}.expectedFacts[${factIndex}]`))
    unique(facts, `${scenario.id}.expectedFacts`)
    return scenario.id
  })
  unique(scenarioIds, 'scenario ids')
  const knownScenarios = new Set(scenarioIds)

  exactKeys(rules.productionIsolation, [
    'allowedLegacySourcePaths',
    'inventoryRoots',
    'legacyTokens',
    'sourceRequirements',
  ], 'productionIsolation')
  const legacyTokens = array(
    rules.productionIsolation.legacyTokens,
    'productionIsolation.legacyTokens',
  )
  legacyTokens.forEach((token, index) => nonEmpty(
    token,
    `productionIsolation.legacyTokens[${index}]`,
  ))
  unique(legacyTokens, 'productionIsolation.legacyTokens')
  const legacyInventory = []
  for (const directory of array(
    rules.productionIsolation.inventoryRoots,
    'productionIsolation.inventoryRoots',
  )) {
    nonEmpty(directory, 'productionIsolation.inventoryRoots entry')
    for (const path of sourceFilesBelow(root, directory)) {
      const source = readFileSync(join(root, path), 'utf8')
      if (legacyTokens.some(token => source.includes(token))) legacyInventory.push(path)
    }
  }
  const allowedLegacySourcePaths = array(
    rules.productionIsolation.allowedLegacySourcePaths,
    'productionIsolation.allowedLegacySourcePaths',
  )
  if (JSON.stringify(legacyInventory.toSorted()) !== JSON.stringify(allowedLegacySourcePaths.toSorted())) {
    fail('legacy Provider source inventory differs')
  }
  for (const [index, requirement] of array(
    rules.productionIsolation.sourceRequirements,
    'productionIsolation.sourceRequirements',
  ).entries()) {
    exactKeys(requirement, [
      'forbiddenFragments',
      'path',
      'requiredFragments',
    ], `productionIsolation.sourceRequirements[${index}]`)
    nonEmpty(requirement.path, `productionIsolation.sourceRequirements[${index}].path`)
    const source = readFileSync(join(root, requirement.path), 'utf8')
    for (const fragment of requirement.requiredFragments) {
      nonEmpty(fragment, `productionIsolation.sourceRequirements[${index}].requiredFragments`)
      if (!source.includes(fragment)) fail(`${requirement.path} is missing: ${fragment}`)
    }
    for (const fragment of requirement.forbiddenFragments) {
      nonEmpty(fragment, `productionIsolation.sourceRequirements[${index}].forbiddenFragments`)
      if (source.includes(fragment)) fail(`${requirement.path} contains forbidden: ${fragment}`)
    }
  }

  const normalizations = array(rules.normalization, 'normalization')
  if (normalizations.length !== 2) fail('normalization must contain exactly two rules')
  unique(normalizations.map(entry => entry.field), 'normalization fields')
  for (const [index, entry] of normalizations.entries()) {
    exactKeys(entry, ['field', 'rule'], `normalization[${index}]`)
    nonEmpty(entry.field, `normalization[${index}].field`)
    nonEmpty(entry.rule, `normalization[${index}].rule`)
  }

  const nodeSource = readFileSync(join(root, rules.dshBaseline.sourcePath), 'utf8')
  const dshCoverage = new Set()
  for (const [index, baseline] of array(rules.dshBaseline.tests, 'dshBaseline.tests').entries()) {
    exactKeys(baseline, ['scenarioIds', 'testName'], `dshBaseline.tests[${index}]`)
    assertNodeTest(nodeSource, baseline.testName)
    for (const scenarioId of baseline.scenarioIds) {
      if (!knownScenarios.has(scenarioId)) fail(`DSH evidence references ${scenarioId}`)
      dshCoverage.add(scenarioId)
    }
  }
  const pnpmDirectory = join(root, 'node_modules/.pnpm')
  for (const [index, evidence] of array(
    rules.dshSourceEvidence,
    'dshSourceEvidence',
  ).entries()) {
    exactKeys(evidence, [
      'packageDirectoryPrefix',
      'relativePath',
      'requiredFragments',
      'scenarioIds',
    ], `dshSourceEvidence[${index}]`)
    const matches = readdirSync(pnpmDirectory)
      .filter(name => name.startsWith(evidence.packageDirectoryPrefix))
    if (matches.length !== 1) fail(`${evidence.packageDirectoryPrefix} must resolve once`)
    const source = readFileSync(join(pnpmDirectory, matches[0], evidence.relativePath), 'utf8')
    for (const fragment of evidence.requiredFragments) {
      nonEmpty(fragment, `dshSourceEvidence[${index}].requiredFragments`)
      if (!source.includes(fragment)) fail(`DSH source is missing pinned fragment: ${fragment}`)
    }
    for (const scenarioId of evidence.scenarioIds) {
      if (!knownScenarios.has(scenarioId)) fail(`DSH source evidence references ${scenarioId}`)
      dshCoverage.add(scenarioId)
    }
  }

  const rustCoverage = new Set()
  const commands = array(rules.rustEvidence, 'rustEvidence').map((evidence, index) => {
    exactKeys(evidence, [
      'binary',
      'package',
      'scenarioIds',
      'sourcePath',
      'testName',
    ], `rustEvidence[${index}]`)
    for (const key of ['binary', 'package', 'sourcePath', 'testName']) {
      nonEmpty(evidence[key], `rustEvidence[${index}].${key}`)
    }
    if (evidence.sourcePath !== `crates/${evidence.package}/tests/${evidence.binary}.rs`) {
      fail(`${evidence.testName} sourcePath differs from its Cargo target`)
    }
    assertRustTest(root, evidence)
    for (const scenarioId of evidence.scenarioIds) {
      if (!knownScenarios.has(scenarioId)) fail(`Rust evidence references ${scenarioId}`)
      rustCoverage.add(scenarioId)
    }
    const arguments_ = rustArguments(evidence)
    if (arguments_.some(argument => /[*?[\]]/u.test(argument))) fail('wildcard Cargo filter')
    if (arguments_.includes('--ignored') || arguments_.includes('--include-ignored')) {
      fail('ignored Rust evidence is forbidden')
    }
    return { arguments: arguments_, testName: evidence.testName }
  })
  for (const [index, evidence] of array(rules.rustUnitEvidence, 'rustUnitEvidence').entries()) {
    exactKeys(evidence, [
      'package',
      'requiredFragments',
      'scenarioIds',
      'sourcePath',
      'testName',
    ], `rustUnitEvidence[${index}]`)
    const source = readFileSync(join(root, evidence.sourcePath), 'utf8')
    const functionName = evidence.testName.split('::').at(-1)
    assertRustTest(root, { ...evidence, testName: functionName })
    for (const fragment of evidence.requiredFragments) {
      if (!source.includes(fragment)) fail(`Rust unit source is missing: ${fragment}`)
    }
    for (const scenarioId of evidence.scenarioIds) {
      if (!knownScenarios.has(scenarioId)) fail(`Rust unit evidence references ${scenarioId}`)
      rustCoverage.add(scenarioId)
    }
    commands.push({
      arguments: [
        'test',
        '-p',
        evidence.package,
        '--lib',
        evidence.testName,
        '--locked',
        '--offline',
        '--',
        '--exact',
      ],
      testName: evidence.testName,
    })
  }

  for (const scenarioId of scenarioIds) {
    if (!dshCoverage.has(scenarioId)) fail(`${scenarioId} lacks DSH evidence`)
    if (!rustCoverage.has(scenarioId)) fail(`${scenarioId} lacks Rust evidence`)
  }
  const differences = array(rules.approvedDifferences, 'approvedDifferences')
  for (const [index, difference] of differences.entries()) {
    exactKeys(difference, [
      'dshBehavior',
      'id',
      'rustBehavior',
      'scenarioId',
      'userVisibleImpact',
    ], `approvedDifferences[${index}]`)
    if (!knownScenarios.has(difference.scenarioId)) fail('difference references unknown scenario')
    if (difference.userVisibleImpact !== 'none') fail('approved difference has user-visible impact')
  }
  unique(differences.map(difference => difference.id), 'approved difference ids')
  return { commands, scenarioCount: scenarioIds.length }
}

function executeOrFail(execute, program, arguments_, root, label) {
  const result = execute(program, arguments_, { cwd: root, stdio: 'inherit' })
  if (result.error !== undefined) throw result.error
  if (result.signal !== null) fail(`${label} ended with ${result.signal}`)
  if (result.status !== 0) fail(`${label} exited with ${result.status ?? 1}`)
}

export function runProviderDifferentialGate({ execute = spawnSync, root }) {
  const rules = JSON.parse(readFileSync(join(root, RULES_PATH), 'utf8'))
  const fixture = JSON.parse(readFileSync(join(root, rules.fixturePath), 'utf8'))
  const validated = validateProviderDifferential({ fixture, root, rules })
  executeOrFail(
    execute,
    process.execPath,
    rules.dshBaseline.arguments,
    root,
    'DSH provider baseline',
  )
  for (const command of validated.commands) {
    executeOrFail(execute, 'cargo', command.arguments, root, `Rust evidence ${command.testName}`)
  }
  return { commandCount: validated.commands.length + 1, scenarioCount: validated.scenarioCount }
}

if (process.argv[1] !== undefined && resolve(process.argv[1]) === resolve(import.meta.filename)) {
  if (process.argv.length !== 3 || process.argv[2] !== '--check') {
    fail('usage: run-provider-dsh-rust-differential.mjs --check')
  }
  const result = runProviderDifferentialGate({ root: resolve(import.meta.dirname, '..') })
  process.stdout.write(
    `Provider differential matched ${result.scenarioCount} offline scenarios with ${result.commandCount} exact commands\n`,
  )
}
