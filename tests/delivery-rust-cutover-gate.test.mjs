import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { existsSync } from 'node:fs'
import { readFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(root, 'docs/contracts/delivery-rust-cutover.rules.json')
const documentationPath = join(root, 'docs/contracts/delivery-rust-cutover.md')
const expectedPath = join(
  root,
  'tests/fixtures/oracles/delivery-strongflow-rust-expected.v1.json',
)
const runnerEntryPath = join(
  root,
  'crates/winwincode-control-plane/tests/delivery_strongflow_differential_runner.rs',
)
const runnerSupportPath = join(
  root,
  'crates/winwincode-control-plane/tests/support/differential_runner.rs',
)

async function json(path) {
  return JSON.parse(await readFile(path, 'utf8'))
}

function exactKeys(value, expected, label) {
  assert.deepEqual(Object.keys(value).sort(), [...expected].sort(), label)
}

function repositoryPath(path) {
  assert.equal(path.startsWith('/'), false, `${path} must be repository-relative`)
  assert.equal(path.split('/').includes('..'), false, `${path} leaves the repository`)
  return join(root, path)
}

function dependencyLine(manifest, name) {
  return new RegExp(`^${name.replaceAll('-', '[_-]')}(?:\\.workspace)?\\s*=`, 'mu').test(manifest)
}

test('release contract freezes the canonical Rust Delivery result', async () => {
  const [rules, documentation, expected] = await Promise.all([
    json(rulesPath),
    readFile(documentationPath, 'utf8'),
    json(expectedPath),
  ])

  exactKeys(rules, [
    'canonicalBackend',
    'cleanup',
    'coverage',
    'dependencyBoundaries',
    'documentation',
    'issueId',
    'migration',
    'releaseBoundary',
    'schemaVersion',
    'status',
    'verification',
  ], 'cutover rules top-level shape')
  assert.equal(rules.schemaVersion, 'winwincode.delivery-rust-cutover-gate.v2')
  assert.equal(rules.issueId, 'winwincode-9c4.16.6.6.5')
  assert.equal(rules.status, 'implemented-enforced')
  assert.equal(rules.documentation, 'docs/contracts/delivery-rust-cutover.md')
  assert.equal(rules.coverage.symbolGraphComplete, false)

  assert.equal(rules.canonicalBackend.owner, 'winwincode-control-plane')
  assert.equal(rules.canonicalBackend.scenarioCount, 10)
  assert.deepEqual(rules.canonicalBackend.typedMutationEntries, [
    'ControlPlane::commit_delivery_command',
    'ControlPlane::commit_delivery_execution',
    'ControlPlane::commit_delivery_session_binding',
    'ControlPlane::commit_delivery_terminal_outcome',
    'ControlPlane::commit_delivery_task_breakdown',
    'ControlPlane::commit_delivery_verdict',
  ])
  assert.equal(rules.canonicalBackend.typedQueryEntry, 'StrongFlowProjectionQueryPort')
  assert.deepEqual(rules.canonicalBackend.atomicPersistenceMembers, [
    'product_state',
    'aggregate_journal_records',
    'command_receipts',
    'outbox_events',
  ])

  assert.equal(rules.migration.planAuthority, 'node-authored-closed-plan-only')
  assert.equal(rules.migration.inputPolicy, 'canonical-fixture-only')
  assert.equal(rules.migration.runtimeFallbackAllowed, false)
  assert.equal(
    createHash('sha256').update(await readFile(expectedPath)).digest('hex'),
    rules.migration.expected.sha256,
  )
  assert.deepEqual(
    rules.migration.checkpoints.map(entry => entry.id),
    expected.result.scenarios.map(entry => entry.id),
  )

  const scenarios = Object.fromEntries(
    expected.result.scenarios.map(scenario => [scenario.id, scenario]),
  )
  assert.equal(scenarios['success-closed-loop'].observation.snapshot.status, 'delivered')
  assert.equal(scenarios['success-closed-loop'].observation.verdict.status, 'pass')
  assert.equal(scenarios['infra-error'].observation.verdict.status, 'infra_error')
  assert.match(documentation, /Rust Control Plane 统一持有 Delivery 业务事实/u)
  assert.match(documentation, /apps\/client/u)
  assert.match(documentation, /attempt\+1/u)
})

test('typed Delivery executor and production dependency boundaries are current', async () => {
  const [rules, entry, support, ...manifests] = await Promise.all([
    json(rulesPath),
    readFile(runnerEntryPath, 'utf8'),
    readFile(runnerSupportPath, 'utf8'),
    ...[
      'crates/winwincode-control-plane/Cargo.toml',
      'crates/winwincode-server/Cargo.toml',
      'crates/winwincode-worker/Cargo.toml',
      'crates/winwincode-local/Cargo.toml',
      'crates/helper/Cargo.toml',
    ].map(path => readFile(join(root, path), 'utf8')),
  ])
  const runner = `${entry}\n${support}`

  assert.match(entry, /machine_entry_executes_node_authored_plan_when_supplied/u)
  assert.match(support, /ControlPlane::start_local/u)
  for (const qualified of rules.canonicalBackend.typedMutationEntries) {
    assert.match(support, new RegExp(`\\.${qualified.split('::').at(-1)}\\b`, 'u'), qualified)
  }
  assert.match(support, /StrongFlowProjectionQueryPort/u)
  assert.equal((support.match(/\.commit\(/gu) ?? []).length, 1)
  for (const forbidden of [
    /FileDeliveryJournal/u,
    /DeliveryStore::/u,
    /StrongFlowService/u,
    /packages\/strongflow/u,
  ]) assert.doesNotMatch(runner, forbidden)

  const manifestNames = ['controlPlane', 'server', 'worker', 'local', 'helper']
  for (const [index, name] of manifestNames.entries()) {
    const manifest = manifests[index]
    for (const dependency of rules.dependencyBoundaries[name].allowedProductionDependencies) {
      assert.equal(dependencyLine(manifest, dependency), true, `${name} is missing ${dependency}`)
    }
  }
  assert.equal(rules.releaseBoundary.cutoverComplete, true)
  assert.equal(rules.releaseBoundary.newBackendCallersAllowed, false)
  for (const path of [
    rules.releaseBoundary.clientRoot,
    rules.releaseBoundary.clientFacade,
    rules.releaseBoundary.generatedNetworkOwner,
    rules.releaseBoundary.serverRoot,
    rules.releaseBoundary.controlPlaneRoot,
    rules.releaseBoundary.workerRoot,
    rules.releaseBoundary.localRoot,
    rules.releaseBoundary.helperRoot,
  ]) assert.equal(existsSync(repositoryPath(path)), true, path)
})

test('release gate asserts no second product writer or transport boundary', async () => {
  const [rules, documentation, packageManifest] = await Promise.all([
    json(rulesPath),
    readFile(documentationPath, 'utf8'),
    json(join(root, 'package.json')),
  ])

  assert.deepEqual(rules.cleanup, {
    gate: 'scripts/phase-6-6-negative-gate.mjs',
    command: 'corepack pnpm verify:phase-6.6',
    expectedOutput: 'phase-6.6.6 negative gate GREEN',
    isolation: 'fresh-process-and-empty-TMPDIR-after-each-iteration',
    exercisedRecoveryScenarios: [
      'corruption-recovery',
      'request-id-replay',
      'cancel-replay',
      'attempt-retry',
    ],
  })
  assert.equal(packageManifest.scripts['verify:phase-6.6'], 'node scripts/phase-6-6-negative-gate.mjs')
  for (const path of [
    rules.verification.cutoverCommand,
    rules.verification.sourceCommand,
    rules.verification.contractCommand,
    rules.verification.formatCommand,
    rules.verification.gateTest,
    rules.verification.apiCoverageTest,
    rules.verification.dependencyTest,
  ]) {
    if (path.startsWith('corepack ')) continue
    assert.equal(existsSync(repositoryPath(path)), true, path)
  }
  for (const phrase of [
    'Client 不连接 Worker',
    '新业务调用方必须先进入生成 schema 和 Control Plane typed command',
    'receipt',
    'outbox',
    'corepack pnpm verify:phase-6.6',
  ]) assert.equal(documentation.includes(phrase), true, phrase)
})
