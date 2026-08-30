#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const RULES_PATH = 'docs/contracts/session-worker-fault-differential.rules.json'
const EXPECTED_DIFFERENCES = Object.freeze([
  {
    behaviorBaselineId: 'chat-turn-success',
    canonicalFact: 'ProductSession, WorkerSession, and CodexThread are separate canonical identities; the real fixture still creates one embedded Codex session per accepted job.',
    id: 'session-identity-split',
    legacyFact: 'DSH exposed one product session mapped to one kernel session.',
    userVisibleImpact: 'none',
  },
  {
    behaviorBaselineId: 'stage-cancel',
    canonicalFact: 'Rust records one typed, session-bound cancellation; the DSH baseline still verifies AbortError at the caller boundary.',
    id: 'typed-cancellation-boundary',
    legacyFact: 'The DSH caller observed AbortError directly.',
    userVisibleImpact: 'none',
  },
  {
    behaviorBaselineId: 'native-close',
    canonicalFact: 'Worker shutdown owns embedded Codex cleanup and the local fixture proves cleanup before temporary files are removed.',
    id: 'worker-owned-codex-cleanup',
    legacyFact: 'The Node native bridge owned kernel session close and cleanup.',
    userVisibleImpact: 'none',
  },
])

function fail(message) {
  throw new Error(message)
}

function requireObject(value, label) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    fail(`${label} must be an object`)
  }
  return value
}

function requireArray(value, label) {
  if (!Array.isArray(value)) fail(`${label} must be an array`)
  return value
}

function requireExactKeys(value, keys, label) {
  requireObject(value, label)
  const actual = Object.keys(value).toSorted()
  const expected = [...keys].toSorted()
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    fail(`${label} keys differ: expected ${expected.join(', ')}, actual ${actual.join(', ')}`)
  }
}

function requireDeepEqual(actual, expected, label) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) fail(`${label} differs`)
}

function requireNonEmptyString(value, label) {
  if (typeof value !== 'string' || value.length === 0) fail(`${label} must be a non-empty string`)
}

function requireUnique(values, label) {
  if (new Set(values).size !== values.length) fail(`${label} must be unique`)
}

function cargoArguments(evidence) {
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

function assertExactRustTestSource(root, evidence) {
  const source = readFileSync(join(root, evidence.sourcePath), 'utf8')
  const escapedName = evidence.testName.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&')
  const declaration = new RegExp(`(?:async\\s+)?fn\\s+${escapedName}\\s*\\(\\s*\\)`, 'gu')
  const matches = [...source.matchAll(declaration)]
  if (matches.length !== 1) {
    fail(`${evidence.id} must have exactly one Rust test declaration, found ${matches.length}`)
  }
  const prefix = source.slice(Math.max(0, matches[0].index - 80), matches[0].index)
  if (!/#\[(?:tokio::)?test\]\s*$/u.test(prefix)) {
    fail(`${evidence.id} exact Rust function is not marked as a test`)
  }
  for (const fragment of evidence.requiredSourceFragments) {
    if (!source.includes(fragment)) fail(`${evidence.id} is missing source assertion: ${fragment}`)
  }
}

export function validateFaultDifferentialRules({ inventory, root, rules }) {
  requireExactKeys(rules, [
    'approvedDifferences',
    'behaviorMappings',
    'canonicalEvidence',
    'executionPolicy',
    'inventoryPath',
    'issueId',
    'schemaVersion',
  ], 'rules')
  if (rules.schemaVersion !== 'winwincode.session-worker-fault-differential.v1') {
    fail('rules.schemaVersion differs')
  }
  if (rules.issueId !== 'winwincode-9c4.16.4.7') fail('rules.issueId differs')
  if (rules.inventoryPath !== 'docs/decisions/0028-control-plane-worker-migration.inventory.json') {
    fail('rules.inventoryPath differs')
  }

  requireExactKeys(rules.executionPolicy, [
    'approvedDifferencePolicy',
    'attemptsPerCommand',
    'baselineRunner',
    'commandOrder',
    'exactCargoFilterRequired',
    'ignoredTestsAllowed',
    'staleResultPolicy',
    'unexpectedDifferencePolicy',
    'wildcardAllowed',
  ], 'executionPolicy')
  requireDeepEqual(rules.executionPolicy, {
    approvedDifferencePolicy: 'exact-list-only',
    attemptsPerCommand: 1,
    baselineRunner: {
      arguments: ['scripts/run-dsh-migration-baseline.mjs'],
      program: 'NODE_EXECUTABLE',
    },
    commandOrder: 'baseline-then-evidence-as-listed',
    exactCargoFilterRequired: true,
    ignoredTestsAllowed: false,
    staleResultPolicy: 'reject-before-authority-write',
    unexpectedDifferencePolicy: 'reject',
    wildcardAllowed: false,
  }, 'executionPolicy')
  requireDeepEqual(rules.approvedDifferences, EXPECTED_DIFFERENCES, 'approvedDifferences')

  const baselines = requireArray(inventory.behaviorBaselines, 'inventory.behaviorBaselines')
  const mappings = requireArray(rules.behaviorMappings, 'behaviorMappings')
  requireDeepEqual(
    mappings.map(mapping => mapping.id),
    baselines.map(baseline => baseline.id),
    'behavior baseline order and coverage',
  )

  const evidenceList = requireArray(rules.canonicalEvidence, 'canonicalEvidence')
  const evidenceIds = evidenceList.map(evidence => evidence.id)
  requireUnique(evidenceIds, 'canonical evidence ids')
  const evidenceById = new Map(evidenceList.map(evidence => [evidence.id, evidence]))
  const referencedEvidence = new Set()

  for (const [index, evidence] of evidenceList.entries()) {
    const label = `canonicalEvidence[${index}]`
    requireExactKeys(evidence, [
      'binary',
      'coversFacts',
      'id',
      'package',
      'requiredSourceFragments',
      'sourcePath',
      'testName',
    ], label)
    for (const key of ['binary', 'id', 'package', 'sourcePath', 'testName']) {
      requireNonEmptyString(evidence[key], `${label}.${key}`)
    }
    if (!/^[a-z0-9_-]+$/u.test(evidence.package)) fail(`${label}.package is not canonical`)
    if (!/^[a-z0-9_]+$/u.test(evidence.binary)) fail(`${label}.binary is not canonical`)
    if (!/^[a-z0-9_]+$/u.test(evidence.testName)) fail(`${label}.testName is not exact`)
    if (evidence.sourcePath !== `crates/${evidence.package}/tests/${evidence.binary}.rs`) {
      fail(`${label}.sourcePath does not match package and binary`)
    }
    const fragments = requireArray(evidence.requiredSourceFragments, `${label}.requiredSourceFragments`)
    if (fragments.length === 0) fail(`${label}.requiredSourceFragments must not be empty`)
    fragments.forEach((fragment, fragmentIndex) => {
      requireNonEmptyString(fragment, `${label}.requiredSourceFragments[${fragmentIndex}]`)
    })
    requireUnique(fragments, `${label}.requiredSourceFragments`)
    requireObject(evidence.coversFacts, `${label}.coversFacts`)
    if (Object.keys(evidence.coversFacts).length === 0) fail(`${label}.coversFacts must not be empty`)
    assertExactRustTestSource(root, evidence)

    const arguments_ = cargoArguments(evidence)
    if (arguments_.some(argument => /[*?\[\]]/u.test(argument))) {
      fail(`${label} Cargo command contains a wildcard`)
    }
    if (arguments_.includes('--ignored') || arguments_.includes('--include-ignored')) {
      fail(`${label} Cargo command enables ignored tests`)
    }
    if (arguments_.at(-1) !== '--exact') fail(`${label} Cargo command is not exact`)
  }

  for (const [index, mapping] of mappings.entries()) {
    const label = `behaviorMappings[${index}]`
    requireExactKeys(mapping, ['canonicalEvidenceIds', 'id', 'legacyBaseline'], label)
    const baseline = baselines[index]
    requireDeepEqual(mapping.legacyBaseline, {
      expectedFacts: baseline.expectedFacts,
      scenario: baseline.scenario,
      testFile: baseline.testFile,
      testName: baseline.testName,
    }, `${label}.legacyBaseline`)
    const ids = requireArray(mapping.canonicalEvidenceIds, `${label}.canonicalEvidenceIds`)
    if (ids.length === 0) fail(`${label}.canonicalEvidenceIds must not be empty`)
    requireUnique(ids, `${label}.canonicalEvidenceIds`)
    const coveredFacts = new Set()
    for (const evidenceId of ids) {
      const evidence = evidenceById.get(evidenceId)
      if (evidence === undefined) fail(`${label} references unknown evidence ${evidenceId}`)
      referencedEvidence.add(evidenceId)
      const facts = evidence.coversFacts[mapping.id]
      if (!Array.isArray(facts) || facts.length === 0) {
        fail(`${evidenceId} does not cover facts for ${mapping.id}`)
      }
      for (const fact of facts) {
        if (!baseline.expectedFacts.includes(fact)) {
          fail(`${evidenceId} claims an unknown ${mapping.id} fact: ${fact}`)
        }
        coveredFacts.add(fact)
      }
    }
    requireDeepEqual(
      [...coveredFacts].toSorted(),
      [...baseline.expectedFacts].toSorted(),
      `${mapping.id} fact coverage`,
    )
  }
  requireDeepEqual([...referencedEvidence].toSorted(), evidenceIds.toSorted(), 'referenced evidence')

  for (const evidence of evidenceList) {
    for (const behaviorId of Object.keys(evidence.coversFacts)) {
      const mapping = mappings.find(candidate => candidate.id === behaviorId)
      if (mapping === undefined || !mapping.canonicalEvidenceIds.includes(evidence.id)) {
        fail(`${evidence.id} has unreferenced fact coverage for ${behaviorId}`)
      }
    }
  }
  return rules
}

function defaultExecute(program, arguments_, options) {
  return spawnSync(program, arguments_, options)
}

function requireSuccessfulProcess(result, label) {
  if (result.error !== undefined) throw result.error
  if (result.signal !== null && result.signal !== undefined) fail(`${label} ended with ${result.signal}`)
  if (result.status !== 0) fail(`${label} exited with ${result.status ?? 'no status'}`)
}

export function runFaultDifferentialGate({ execute = defaultExecute, root }) {
  const rules = JSON.parse(readFileSync(join(root, RULES_PATH), 'utf8'))
  const inventory = JSON.parse(readFileSync(join(root, rules.inventoryPath), 'utf8'))
  validateFaultDifferentialRules({ inventory, root, rules })

  const baseline = execute(process.execPath, rules.executionPolicy.baselineRunner.arguments, {
    cwd: root,
    env: process.env,
    stdio: 'inherit',
  })
  requireSuccessfulProcess(baseline, 'legacy DSH baseline')

  const commands = []
  for (const evidence of rules.canonicalEvidence) {
    const arguments_ = cargoArguments(evidence)
    const result = execute('cargo', arguments_, {
      cwd: root,
      env: { ...process.env, CARGO_NET_OFFLINE: 'true' },
      stdio: 'inherit',
    })
    requireSuccessfulProcess(result, `canonical evidence ${evidence.id}`)
    commands.push({ arguments: arguments_, evidenceId: evidence.id, program: 'cargo' })
  }

  return {
    approvedDifferenceIds: rules.approvedDifferences.map(difference => difference.id),
    behaviorBaselineIds: rules.behaviorMappings.map(mapping => mapping.id),
    commands,
    schemaVersion: rules.schemaVersion,
    status: 'passed',
  }
}

function main() {
  if (process.argv.length !== 3 || process.argv[2] !== '--check') {
    throw new TypeError('usage: run-session-worker-fault-differential.mjs --check')
  }
  const result = runFaultDifferentialGate({ root: resolve(import.meta.dirname, '..') })
  process.stdout.write(`${JSON.stringify(result)}\n`)
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) main()
