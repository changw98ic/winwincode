import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFile, readdir } from 'node:fs/promises'
import { extname, join, relative, resolve } from 'node:path'
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
const sourceExtensions = new Set([
  '.cjs',
  '.cts',
  '.js',
  '.jsx',
  '.mjs',
  '.mts',
  '.ts',
  '.tsx',
])

async function json(path) {
  return JSON.parse(await readFile(path, 'utf8'))
}

function exactKeys(value, expected, label) {
  assert.deepEqual(Object.keys(value).sort(), [...expected].sort(), label)
}

async function sourceFiles(directory) {
  const files = []
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (entry.name === 'dist' || entry.name === 'node_modules') continue
    const path = join(directory, entry.name)
    if (entry.isDirectory()) {
      files.push(...await sourceFiles(path))
    } else if (sourceExtensions.has(extname(entry.name))
      && !entry.name.includes('.test.')) {
      files.push(path)
    }
  }
  return files
}

async function pathsContaining(pattern) {
  const files = (
    await Promise.all(['apps', 'packages', 'scripts'].map(path => sourceFiles(join(root, path))))
  ).flat()
  const matches = []
  for (const path of files) {
    if (pattern.test(await readFile(path, 'utf8'))) {
      matches.push(relative(root, path))
    }
  }
  return matches.sort()
}

test('phase 2.7 freezes the canonical Rust Delivery backend and exact result', async () => {
  const [rules, documentation, expected, differentialRules] = await Promise.all([
    json(rulesPath),
    readFile(documentationPath, 'utf8'),
    json(expectedPath),
    json(join(root, 'docs/contracts/delivery-strongflow-rust-differential.rules.json')),
  ])

  exactKeys(rules, [
    'canonicalBackend',
    'cleanup',
    'coverage',
    'dependencyBoundaries',
    'documentation',
    'issueId',
    'legacyBoundary',
    'migration',
    'schemaVersion',
    'status',
    'verification',
  ], 'cutover rules top-level shape')
  assert.equal(rules.schemaVersion, 'winwincode.delivery-rust-cutover-gate.v1')
  assert.equal(rules.issueId, 'winwincode-9c4.16.2.7')
  assert.equal(rules.status, 'implemented-enforced')
  assert.equal(rules.documentation, 'docs/contracts/delivery-rust-cutover.md')
  assert.deepEqual(rules.coverage, {
    mode: 'git-file-inventory+rg-direct-read-fallback',
    symbolGraphComplete: false,
    reason: 'The repository-local index script is absent; this gate claims file-level coverage only.',
  })

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
  assert.equal(
    rules.canonicalBackend.typedQueryEntry,
    'StrongFlowProjectionQueryPort',
  )
  assert.deepEqual(rules.canonicalBackend.atomicPersistenceMembers, [
    'product_state',
    'aggregate_journal_records',
    'command_receipts',
    'outbox_events',
  ])

  assert.equal(
    rules.migration.inputSchemaVersion,
    'winwincode.delivery-strongflow-differential-plan.v2',
  )
  assert.equal(
    rules.migration.mappingVersion,
    'winwincode.delivery-strongflow-legacy-to-canonical.v1',
  )
  assert.equal(
    rules.migration.expected.path,
    'tests/fixtures/oracles/delivery-strongflow-rust-expected.v1.json',
  )
  assert.equal(
    rules.migration.expected.sha256,
    '4aaab65259218df5df814b9d9743d71e779aad81a552c0540df63ad8490f1c71',
  )
  assert.equal(
    createHash('sha256').update(await readFile(expectedPath)).digest('hex'),
    rules.migration.expected.sha256,
  )
  assert.deepEqual(
    rules.migration.checkpoints,
    [
      ['success-closed-loop', 21],
      ['request-id-replay', 1],
      ['revision-conflict', 2],
      ['corruption-recovery', 1],
      ['task-dag', 2],
      ['candidate-invalidation', 31],
      ['attention', 8],
      ['inconclusive', 19],
      ['infra-error', 19],
      ['rework', 31],
    ].map(([id, revision]) => ({ id, revision })),
  )
  assert.equal(rules.migration.oldDtoPolicy, 'one-time-test-migration-only')
  assert.equal(rules.migration.runtimeFallbackAllowed, false)
  assert.equal(
    differentialRules.runner.inputSchemaVersion,
    rules.migration.inputSchemaVersion,
  )
  assert.equal(
    differentialRules.canonicalMigration.mappingVersion,
    rules.migration.mappingVersion,
  )

  const scenarios = Object.fromEntries(
    expected.result.scenarios.map(scenario => [scenario.id, scenario]),
  )
  assert.equal(scenarios['success-closed-loop'].observation.snapshot.status, 'delivered')
  assert.equal(scenarios['success-closed-loop'].observation.verdict.status, 'pass')
  assert.deepEqual(
    differentialRules.scenarios.find(scenario => scenario.id === 'rework')
      .canonicalAssertions.verdicts,
    ['fail', 'pass'],
  )
  assert.equal(scenarios['infra-error'].observation.verdict.status, 'infra_error')
  assert.match(documentation, /Rust Control Plane 是迁移后 Delivery 后端的唯一正式写入者/u)
  assert.match(documentation, /不代表浏览器和 Host 已完成切换/u)
})

test('the differential executor reaches only typed Control Plane and SQLite seams', async () => {
  const [rules, entry, support, controlPlaneManifest, deliveryManifest] = await Promise.all([
    json(rulesPath),
    readFile(runnerEntryPath, 'utf8'),
    readFile(runnerSupportPath, 'utf8'),
    readFile(join(root, 'crates/winwincode-control-plane/Cargo.toml'), 'utf8'),
    readFile(join(root, 'crates/winwincode-delivery/Cargo.toml'), 'utf8'),
  ])
  const runner = `${entry}\n${support}`

  assert.match(entry, /machine_entry_executes_node_authored_plan_when_supplied/u)
  assert.match(support, /ControlPlane::start_local/u)
  for (const qualified of rules.canonicalBackend.typedMutationEntries) {
    assert.match(support, new RegExp(`\\.${qualified.split('::').at(-1)}\\b`, 'u'), qualified)
  }
  assert.match(support, /StrongFlowProjectionQueryPort/u)
  assert.equal((support.match(/\.commit\(/gu) ?? []).length, 1)
  assert.match(
    support,
    /fn seed_snapshot_sqlite[\s\S]+SqliteStorage::open[\s\S]+\.commit\(/u,
  )
  for (const forbidden of [
    /FileDeliveryJournal/u,
    /DeliveryStore::/u,
    /StrongFlowService/u,
    /packages\/strongflow/u,
    /delivery-strongflow-typescript\.v1\.json/u,
  ]) {
    assert.doesNotMatch(runner, forbidden)
  }

  for (const denied of rules.dependencyBoundaries.controlPlane.deniedDependencies) {
    assert.doesNotMatch(controlPlaneManifest, new RegExp(`(?:^|["'])${denied}(?:["'.]|$)`, 'mu'))
  }
  for (const denied of rules.dependencyBoundaries.delivery.deniedDependencies) {
    assert.doesNotMatch(deliveryManifest, new RegExp(`(?:^|["'])${denied}(?:["'.]|$)`, 'mu'))
  }
})

test('the remaining TypeScript writer is a closed Phase 6 handoff, not a new backend path', async () => {
  const [rules, documentation, inventory] = await Promise.all([
    json(rulesPath),
    readFile(documentationPath, 'utf8'),
    json(join(root, 'docs/decisions/0028-control-plane-worker-migration.inventory.json')),
  ])
  const boundary = rules.legacyBoundary

  assert.equal(boundary.phase2CutoverClaim, 'canonical-rust-backend-only')
  assert.equal(boundary.browserHostCutoverComplete, false)
  assert.equal(boundary.newBackendCallersAllowed, false)
  assert.equal(boundary.uiCutoverTask, 'winwincode-9c4.16.6.3')
  assert.equal(boundary.removalTask, 'winwincode-9c4.16.6.6')
  assert.deepEqual(boundary.strongFlowServiceConstructorPaths, [
    'apps/host/src/cli.ts',
    'packages/strongflow/src/index.ts',
    'scripts/live-evaluation.mjs',
  ])
  assert.deepEqual(boundary.deliveryStoreWritePaths, [
    'packages/dsh-profile/src/delivery-recovery.ts',
    'packages/strongflow/src/delivery-service.ts',
  ])
  assert.deepEqual(boundary.deliveryStorePublicExportPaths, [
    'packages/strongflow/src/index.ts',
  ])
  assert.deepEqual(boundary.deliveryStoreSymbolPaths, [
    'packages/dsh-profile/src/delivery-recovery.ts',
    'packages/strongflow/src/delivery-service.ts',
    'packages/strongflow/src/delivery-store.ts',
  ])
  assert.deepEqual(boundary.deliveryStoreModuleReferencePaths, [
    'packages/strongflow/src/delivery-service.ts',
    'packages/strongflow/src/index.ts',
    'scripts/verify-packages.mjs',
  ])
  assert.deepEqual(boundary.strongFlowServiceSymbolPaths, [
    'apps/host/src/cli.ts',
    'packages/strongflow/src/delivery-invoker.ts',
    'packages/strongflow/src/delivery-service.ts',
    'packages/strongflow/src/delivery-stage-coordinator.ts',
    'packages/strongflow/src/index.ts',
    'scripts/live-evaluation.mjs',
  ])

  assert.deepEqual(
    await pathsContaining(/new\s+StrongFlowService\b/u),
    boundary.strongFlowServiceConstructorPaths,
  )
  assert.deepEqual(
    await pathsContaining(/DeliveryStore\s*\.\s*(?:create|open|append)\b/u),
    boundary.deliveryStoreWritePaths,
  )
  assert.deepEqual(
    await pathsContaining(/export \* from ['"]\.\/delivery-store\.js['"]/u),
    boundary.deliveryStorePublicExportPaths,
  )
  assert.deepEqual(
    await pathsContaining(/\bDeliveryStore\b/u),
    boundary.deliveryStoreSymbolPaths,
  )
  assert.deepEqual(
    await pathsContaining(/delivery-store\.js/u),
    boundary.deliveryStoreModuleReferencePaths,
  )
  assert.deepEqual(
    await pathsContaining(/\bStrongFlowService\b/u),
    boundary.strongFlowServiceSymbolPaths,
  )

  const deliverySurface = inventory.surfaces.find(
    surface => surface.id === 'strongflow-delivery-domain',
  )
  const remoteSurface = inventory.surfaces.find(
    surface => surface.id === 'strongflow-cordis-remote',
  )
  assert.equal(deliverySurface.phase, '2')
  assert.equal(deliverySurface.disposition, 'translate')
  assert.equal(remoteSurface.phase, '6')
  assert.equal(remoteSurface.disposition, 'delete')
  assert.match(documentation, /winwincode-9c4\.16\.6\.3/u)
  assert.match(documentation, /winwincode-9c4\.16\.6\.6/u)
})

test('the release gate repeats Rust recovery in an isolated temporary directory', async () => {
  const [rules, cleanup, aggregateCleanup, packageManifest] = await Promise.all([
    json(rulesPath),
    readFile(join(root, 'scripts/verify-rust-delivery-cleanup.mjs'), 'utf8'),
    readFile(join(root, 'scripts/verify-runtime-cleanup.mjs'), 'utf8'),
    json(join(root, 'package.json')),
  ])

  assert.deepEqual(rules.cleanup, {
    script: 'scripts/verify-rust-delivery-cleanup.mjs',
    aggregateScript: 'scripts/verify-runtime-cleanup.mjs',
    iterationsEnvironment: 'WINWINCODE_CLEANUP_STRESS_ITERATIONS',
    defaultIterations: 4,
    maximumIterations: 32,
    command: 'node scripts/run-delivery-strongflow-rust-differential.mjs --check',
    expectedOutput: 'Rust differential runner matched all 10 canonical scenarios',
    isolation: 'fresh-process-and-empty-TMPDIR-after-each-iteration',
    exercisedRecoveryScenarios: ['corruption-recovery', 'request-id-replay'],
  })
  for (const token of [
    'WINWINCODE_CLEANUP_STRESS_ITERATIONS',
    'run-delivery-strongflow-rust-differential.mjs',
    'Rust differential runner matched all 10 canonical scenarios',
    'TMPDIR',
    'readdir',
    'recursive: true',
  ]) {
    assert.match(cleanup, new RegExp(token.replaceAll('.', '\\.'), 'u'))
  }
  assert.match(aggregateCleanup, /verify-rust-delivery-cleanup\.mjs/u)
  assert.match(aggregateCleanup, /Rust Control Plane/u)
  assert.equal(
    packageManifest.scripts['verify:delivery-rust-cutover'],
    'node --test tests/delivery-rust-cutover-gate.test.mjs && pnpm oracle:delivery:rust:check',
  )
  assert.match(
    packageManifest.scripts.verify,
    /pnpm verify:delivery-rust-cutover/u,
  )
})
