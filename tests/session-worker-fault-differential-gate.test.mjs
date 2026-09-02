import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  runFaultDifferentialGate,
  validateFaultDifferentialRules,
} from '../scripts/run-session-worker-fault-differential.mjs'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(root, 'docs/contracts/session-worker-fault-differential.rules.json')
const inventoryPath = join(
  root,
  'docs/decisions/0028-control-plane-worker-migration.inventory.json',
)

async function fixture() {
  const [rulesText, inventoryText] = await Promise.all([
    readFile(rulesPath, 'utf8'),
    readFile(inventoryPath, 'utf8'),
  ])
  return {
    inventory: JSON.parse(inventoryText),
    rules: JSON.parse(rulesText),
    rulesText,
  }
}

test('phase 4.7 maps every frozen DSH fact to exact canonical executable evidence', async () => {
  const { inventory, rules, rulesText } = await fixture()
  assert.equal(rulesText, `${JSON.stringify(rules, null, 2)}\n`)
  assert.equal(validateFaultDifferentialRules({ inventory, root, rules }), rules)
  assert.deepEqual(
    rules.behaviorMappings.map(mapping => mapping.id),
    [
      'chat-turn-success',
      'model-failure',
      'stage-cancel',
      'restart-recovery',
      'approval-block',
      'native-close',
    ],
  )
  assert.deepEqual(
    rules.approvedDifferences.map(difference => difference.id),
    [
      'session-identity-split',
      'typed-cancellation-boundary',
      'worker-owned-codex-cleanup',
    ],
  )
  assert.ok(rules.approvedDifferences.every(difference => difference.userVisibleImpact === 'none'))
  const realFixture = rules.canonicalEvidence.find(
    evidence => evidence.id === 'real-codex-worker-fault-fixture',
  )
  assert.equal(realFixture.package, 'winwincode-worker')
  assert.equal(realFixture.binary, 'codex_core_fixture')
  assert.equal(
    realFixture.testName,
    'worker_runs_real_local_codex_once_across_disconnect_duplicate_cancel_and_cleanup',
  )
})

test('the gate executes the six old baselines and every exact Rust command once in fixed order', () => {
  const calls = []
  const result = runFaultDifferentialGate({
    execute(program, arguments_, options) {
      calls.push({ arguments: [...arguments_], cwd: options.cwd, program })
      return { error: undefined, signal: null, status: 0 }
    },
    root,
  })

  assert.equal(result.status, 'passed')
  assert.equal(calls[0].program, process.execPath)
  assert.deepEqual(calls[0].arguments, ['scripts/run-dsh-migration-baseline.mjs'])
  assert.equal(calls.length, result.commands.length + 1)
  assert.equal(new Set(result.commands.map(command => command.evidenceId)).size, result.commands.length)
  for (const [index, command] of result.commands.entries()) {
    assert.equal(calls[index + 1].program, 'cargo')
    assert.deepEqual(calls[index + 1].arguments, command.arguments)
    assert.equal(command.arguments[0], 'test')
    assert.equal(command.arguments.at(-1), '--exact')
    assert.equal(command.arguments.filter(argument => argument === '--exact').length, 1)
    assert.ok(!command.arguments.includes('--ignored'))
    assert.ok(!command.arguments.includes('--include-ignored'))
    assert.ok(command.arguments.every(argument => !/[*?\[\]]/u.test(argument)))
  }
})

test('the gate stops on the first failed command and never retries it', () => {
  const calls = []
  assert.throws(
    () => runFaultDifferentialGate({
      execute(program, arguments_) {
        calls.push({ arguments: [...arguments_], program })
        return {
          error: undefined,
          signal: null,
          status: calls.length === 1 ? 0 : 9,
        }
      },
      root,
    }),
    /canonical evidence real-codex-worker-fault-fixture exited with 9/u,
  )
  assert.equal(calls.length, 2)
  assert.deepEqual(calls[1].arguments.slice(-2), ['--', '--exact'])
})

test('the gate rejects missing fact coverage and undeclared differences', async () => {
  const { inventory, rules } = await fixture()
  const missingFact = structuredClone(rules)
  missingFact.canonicalEvidence.find(
    evidence => evidence.id === 'product-session-model-route',
  ).coversFacts['chat-turn-success'] = []
  assert.throws(
    () => validateFaultDifferentialRules({ inventory, root, rules: missingFact }),
    /does not cover facts for chat-turn-success/u,
  )

  const hiddenDifference = structuredClone(rules)
  hiddenDifference.approvedDifferences.push({
    behaviorBaselineId: 'restart-recovery',
    canonicalFact: 'ignore a stale result',
    id: 'hidden-stale-result',
    legacyFact: 'accept the current result',
    userVisibleImpact: 'none',
  })
  assert.throws(
    () => validateFaultDifferentialRules({ inventory, root, rules: hiddenDifference }),
    /approvedDifferences differs/u,
  )
})

test('workspace test wiring invokes the actual differential runner after parallel Node tests', async () => {
  const [packageText, runnerText] = await Promise.all([
    readFile(join(root, 'package.json'), 'utf8'),
    readFile(join(root, 'scripts/run-ts-tests.mjs'), 'utf8'),
  ])
  const packageJson = JSON.parse(packageText)
  assert.equal(
    packageJson.scripts['verify:session-worker-fault-differential'],
    'pnpm build:ts && pnpm build:native && node scripts/run-session-worker-fault-differential.mjs --check',
  )
  assert.match(
    runnerText,
    /runTests\(\['scripts\/run-session-worker-fault-differential\.mjs', '--check'\]\)/u,
  )
})
