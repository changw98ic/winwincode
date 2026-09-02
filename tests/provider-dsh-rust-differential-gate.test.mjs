import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  assertNodeTest,
  assertRustTestSource,
  runProviderDifferentialGate,
  validateProviderDifferential,
} from '../scripts/run-provider-dsh-rust-differential.mjs'

const root = resolve(import.meta.dirname, '..')

async function contract() {
  const rulesText = await readFile(
    join(root, 'docs/contracts/provider-dsh-rust-differential.rules.json'),
    'utf8',
  )
  const rules = JSON.parse(rulesText)
  const fixtureText = await readFile(join(root, rules.fixturePath), 'utf8')
  return { fixture: JSON.parse(fixtureText), fixtureText, rules, rulesText }
}

test('provider differential freezes nine offline scenarios with DSH and Rust evidence', async () => {
  const { fixture, fixtureText, rules, rulesText } = await contract()
  assert.equal(rulesText, `${JSON.stringify(rules, null, 2)}\n`)
  assert.equal(fixtureText, `${JSON.stringify(fixture, null, 2)}\n`)
  const validated = validateProviderDifferential({ fixture, root, rules })
  assert.equal(validated.scenarioCount, 9)
  assert.deepEqual(
    fixture.scenarios.map(scenario => scenario.id),
    [
      'catalog-routing',
      'request-translation',
      'reasoning-and-parallel-tools',
      'namespaced-parallel-tools',
      'stream-success',
      'typed-errors',
      'cancel-and-disconnect',
      'hot-update',
      'credential-failure',
    ],
  )
  assert.deepEqual(
    rules.normalization.map(entry => entry.field),
    ['providerRequestIdentity', 'chunkBoundaries'],
  )
  assert.deepEqual(
    rules.productionIsolation.allowedLegacySourcePaths,
    [
      'crates/winwincode-control-plane/tests/native_model_port_cutover.rs',
      'packages/dsh-profile/src/model-port.ts',
      'tests/dsh-model-port.test.mjs',
      'tests/dsh-rust-model-port-production-cutover.test.mjs',
      'tests/fixtures/native-dsh-model-turn.mjs',
    ],
  )
  assert.deepEqual(
    rules.approvedDifferences.map(difference => difference.id),
    [
      'versioned-catalog-authority',
      'reference-only-credential-authority',
      'canonical-unknown-provider-error',
      'typed-worker-cancellation',
      'canonical-namespace-identity-validation',
      'visible-reasoning-without-forged-provider-signature',
      'provider-auto-tool-choice-shape',
    ],
  )
  assert.deepEqual(
    fixture.scenarios.find(scenario => scenario.id === 'typed-errors')
      .input.failures.map(failure => failure.code),
    [
      'AUTH',
      'QUOTA',
      'RATE_LIMIT',
      'INVALID_REQUEST',
      'SERVER',
      'TIMEOUT',
      'TRANSPORT',
      'CONTEXT_WINDOW_EXCEEDED',
      'PI_AI_ERROR',
      'EMPTY_RESPONSE',
    ],
  )
  assert.deepEqual(
    fixture.scenarios.find(scenario => scenario.id === 'cancel-and-disconnect').input,
    {
      cancelFinishOrderings: ['cancel-before-finish', 'finish-before-cancel'],
      restartAfterDisconnect: true,
      terminalCodes: ['CANCELLED', 'STREAM_CLOSED'],
    },
  )
})

test('provider differential executes the DSH baseline then each exact offline Rust test once', () => {
  const calls = []
  const result = runProviderDifferentialGate({
    execute(program, arguments_) {
      calls.push({ arguments: [...arguments_], program })
      return { error: undefined, signal: null, status: 0 }
    },
    root,
  })
  assert.equal(result.scenarioCount, 9)
  assert.equal(calls.length, result.commandCount)
  assert.equal(calls[0].program, process.execPath)
  assert.deepEqual(calls[0].arguments, ['--test', 'tests/dsh-model-port.test.mjs'])
  for (const call of calls.slice(1)) {
    assert.equal(call.program, 'cargo')
    assert.equal(call.arguments.at(-1), '--exact')
    assert.equal(call.arguments.filter(argument => argument === '--exact').length, 1)
    assert.ok(call.arguments.includes('--locked'))
    assert.ok(call.arguments.includes('--offline'))
    assert.ok(!call.arguments.includes('--ignored'))
  }
})

test('provider differential rejects missing coverage and undeclared scenario drift', async () => {
  const { fixture, rules } = await contract()
  const missing = structuredClone(rules)
  for (const evidence of missing.rustEvidence) {
    evidence.scenarioIds = evidence.scenarioIds.filter(id => id !== 'hot-update')
  }
  for (const evidence of missing.rustUnitEvidence) {
    evidence.scenarioIds = evidence.scenarioIds.filter(id => id !== 'hot-update')
  }
  assert.throws(
    () => validateProviderDifferential({ fixture, root, rules: missing }),
    /hot-update lacks Rust evidence/u,
  )

  const drift = structuredClone(fixture)
  drift.scenarios.push({ expectedFacts: ['unmapped'], id: 'unmapped', input: {} })
  assert.throws(
    () => validateProviderDifferential({ fixture: drift, root, rules }),
    /unmapped lacks DSH evidence/u,
  )
})

test('provider differential rejects a legacy Provider source outside the exact test inventory', async () => {
  const { fixture, rules } = await contract()
  const drift = structuredClone(rules)
  drift.productionIsolation.allowedLegacySourcePaths.push('packages/native/src/index.ts')
  assert.throws(
    () => validateProviderDifferential({ fixture, root, rules: drift }),
    /legacy Provider source inventory differs/u,
  )
})

test('provider differential stops at the first failed command without retry', () => {
  const calls = []
  assert.throws(
    () => runProviderDifferentialGate({
      execute(program, arguments_) {
        calls.push({ arguments: [...arguments_], program })
        return { error: undefined, signal: null, status: calls.length === 2 ? 7 : 0 }
      },
      root,
    }),
    /Rust evidence cancel_and_provider_finish_race_linearizes_to_one_terminal_and_one_release exited with 7/u,
  )
  assert.equal(calls.length, 2)
})

test('provider differential rejects skipped Node and ignored Rust evidence', () => {
  assert.throws(
    () => assertNodeTest("test.skip('exact provider evidence', () => {})", 'exact provider evidence'),
    /must not use test\.skip/u,
  )
  assert.throws(
    () => assertNodeTest("test.only('exact provider evidence', () => {})", 'exact provider evidence'),
    /must not use test\.only/u,
  )
  assert.throws(
    () => assertRustTestSource('#[test]\n#[ignore]\nfn exact_provider_evidence() {}\n', 'exact_provider_evidence'),
    /must not be ignored/u,
  )
  assert.doesNotThrow(
    () => assertRustTestSource('#[test]\nfn exact_provider_evidence() {}\n', 'exact_provider_evidence'),
  )
  assert.doesNotThrow(
    () => assertRustTestSource(
      '#[tokio::test]\nasync fn exact_provider_evidence() {}\n',
      'exact_provider_evidence',
    ),
  )
})

test('workspace test wiring runs the provider differential after the parallel Node suite', async () => {
  const [packageText, runnerText] = await Promise.all([
    readFile(join(root, 'package.json'), 'utf8'),
    readFile(join(root, 'scripts/run-ts-tests.mjs'), 'utf8'),
  ])
  assert.equal(
    JSON.parse(packageText).scripts['verify:provider-dsh-rust-differential'],
    'pnpm build:ts && node scripts/run-provider-dsh-rust-differential.mjs --check',
  )
  assert.match(
    runnerText,
    /runTests\(\['scripts\/run-provider-dsh-rust-differential\.mjs', '--check'\]\)/u,
  )
})
