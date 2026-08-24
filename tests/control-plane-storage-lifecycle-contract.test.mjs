import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import {
  existsSync,
  readFileSync,
  readdirSync,
} from 'node:fs'
import {
  extname,
  join,
  relative,
  resolve,
} from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-storage-lifecycle.rules.json',
)
const documentationPath = join(
  root,
  'docs',
  'contracts',
  'control-plane-storage-lifecycle.md',
)
const targetGraphPath = join(
  root,
  'docs',
  'decisions',
  '0028-control-plane-worker-target-graph.json',
)

const expectedCommitOrder = [
  'begin-transaction',
  'validate-command-and-revision',
  'append-canonical-state',
  'append-outbox-event',
  'commit-transaction',
  'publish-committed-outbox-event',
]

const expectedStartupOrder = [
  'load-configuration',
  'create-owned-temporary-root',
  'open-storage',
  'apply-pending-migrations',
  'recover-committed-outbox',
  'start-owned-services',
  'accept-commands',
]

const expectedShutdownOrder = [
  'stop-accepting-commands',
  'stop-producing-new-outbox-events',
  'wait-for-owned-command-work',
  'flush-committed-outbox',
  'close-event-publisher',
  'close-storage',
  'release-owned-temporary-root',
]

const expectedLifecycleTests = [
  'startup_migrates_storage_before_the_control_plane_accepts_commits',
  'failed_startup_closes_storage_and_releases_temporary_directory',
  'commit_persists_state_and_outbox_before_publishing_the_event',
  'failed_outbox_insert_rolls_back_the_state_write',
  'publish_failure_keeps_committed_state_and_pending_outbox_for_restart',
  'restart_replays_committed_but_unpublished_outbox_events',
  'shutdown_flushes_outbox_then_closes_publisher_and_storage',
  'shutdown_publish_failure_still_closes_storage_and_releases_temporary_directory',
  'shutdown_releases_the_sqlite_connection_and_temporary_directory',
  'command_receipts_use_canonical_actor_full_scope_request_and_payload_digest',
  'command_digest_is_stable_when_json_object_keys_arrive_in_another_order',
  'invalid_scope_ids_fail_before_the_storage_port_is_called',
]

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function commandFailure(result) {
  return [result.stdout, result.stderr, result.error?.stack]
    .filter(Boolean)
    .join('\n')
}

function filesBelow(path) {
  if (!existsSync(path)) return []
  const files = []
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    const entryPath = join(path, entry.name)
    if (entry.isDirectory()) files.push(...filesBelow(entryPath))
    else if (entry.isFile()) files.push(entryPath)
  }
  return files
}

function transitivePackageDependencies(rootPackageName, packagesByName) {
  const visited = new Set()
  const pending = [rootPackageName]
  while (pending.length > 0) {
    const packageName = pending.pop()
    if (visited.has(packageName)) continue
    visited.add(packageName)
    const package_ = packagesByName.get(packageName)
    if (package_ === undefined) continue
    for (const dependency of package_.dependencies) {
      if (packagesByName.has(dependency.name)) pending.push(dependency.name)
    }
  }
  return visited
}

function packageMatchesPattern(packageName, pattern) {
  if (pattern.endsWith('*')) return packageName.startsWith(pattern.slice(0, -1))
  return packageName === pattern
}

test('phase 2.1 freezes one Control Plane storage authority without claiming implementation', () => {
  const rules = json(rulesPath)

  assert.deepEqual({
    schemaVersion: rules.schemaVersion,
    status: rules.status,
    decision: rules.decision,
    targetGraph: rules.targetGraph,
    phaseTask: rules.phaseTask,
    implementationCompletionSource: rules.implementationCompletionSource,
  }, {
    schemaVersion: 1,
    status: 'required-contract-not-implementation-proof',
    decision: 'docs/decisions/0028-control-plane-worker-migration.md',
    targetGraph: 'docs/decisions/0028-control-plane-worker-target-graph.json',
    phaseTask: 'winwincode-9c4.16.2.1',
    implementationCompletionSource: 'rust-black-box-tests-and-beads',
  })

  assert.deepEqual(rules.ownership, {
    canonicalStateWriter: 'winwincode-control-plane',
    storagePortOwner: 'winwincode-storage',
    transactionCoordinator: 'control-plane-application-command',
    outboxPublisherOwner: 'control-plane-lifecycle',
    workerMayWriteCanonicalState: false,
    processGlobalBackgroundTasksAllowed: false,
  })
})

test('canonical state and its outbox event commit atomically before publication', () => {
  const protocol = json(rulesPath).commitProtocol

  assert.deepEqual(protocol.order, expectedCommitOrder)
  assert.deepEqual(protocol.atomicTransactionMembers, [
    'canonical-state-append',
    'outbox-event-append',
  ])
  assert.equal(protocol.publicationSource, 'durable-committed-outbox-only')
  assert.equal(protocol.publishBeforeCommitAllowed, false)
  assert.deepEqual(protocol.onAnyPreCommitFailure, [
    'rollback-transaction',
    'persist-no-canonical-state',
    'persist-no-outbox-event',
    'publish-no-event',
  ])
  assert.deepEqual(protocol.onPostCommitPublicationFailure, {
    canonicalState: 'remain-committed',
    outboxEvent: 'remain-pending',
    commandResult: 'committed-publication-pending',
    recovery: 'retry-without-reexecuting-command',
  })
})

test('command receipt identity is actor and full scope aware without persisting secrets', () => {
  const identity = json(rulesPath).receiptIdentity

  assert.deepEqual(identity, {
    keyFields: [
      'actor.kind',
      'actor.id',
      'scope.kind',
      'scope.organizationId',
      'scope.workspaceId-when-present',
      'scope.projectId-when-present',
      'scope.repositoryId-when-present',
      'requestId',
    ],
    sameIdentitySameDigest: 'replay-durable-receipt',
    sameIdentityDifferentDigest: 'idempotency-conflict',
    differentActorOrScopeSameRequestId: 'independent-command',
    commandDigest: 'sha256-of-canonical-command-envelope',
    semanticJsonObjectKeyOrderAffectsDigest: false,
    replayEventIdsSource: 'durable-outbox',
    persistedSecretsAllowed: false,
    persistedActorProofAllowed: false,
    legacyV1Migration: {
      targetSchemaVersion: 2,
      receiptIdentity: 'reserved-legacy-v1-migration-identity',
      preserves: ['canonical-state', 'outbox-sequence', 'outbox-publication-state'],
      legacyRuntimeLookupPath: false,
    },
  })
})

test('startup, crash recovery, shutdown, and temporary ownership have deterministic order', () => {
  const rules = json(rulesPath)

  assert.deepEqual(rules.startupProtocol.order, expectedStartupOrder)
  assert.equal(rules.startupProtocol.acceptCommandsBeforeMigration, false)
  assert.deepEqual(rules.crashRecovery, {
    uncommittedTransaction: 'discard-completely',
    committedUnpublishedOutbox: 'replay-in-outbox-sequence-order',
    duplicatePublication: 'deduplicate-by-event-id',
    migrationInterruption: 'resume-or-rollback-before-serving',
    staleTemporaryRoot: 'remove-only-after-owned-lease-is-stale',
  })
  assert.deepEqual(rules.shutdownProtocol.order, expectedShutdownOrder)
  assert.equal(rules.shutdownProtocol.storageClosesAfterPublisher, true)
  assert.equal(rules.shutdownProtocol.returnsWithOwnedTasksRunning, false)
  assert.deepEqual(rules.shutdownProtocol.onFlushFailure, [
    'retain-unpublished-outbox',
    'close-event-publisher',
    'close-storage',
    'release-owned-temporary-root',
    'return-typed-shutdown-error',
  ])
  assert.deepEqual(rules.startupProtocol.onMigrationFailure, [
    'accept-no-commands',
    'close-storage',
    'release-owned-temporary-root',
    'return-typed-start-error',
  ])
  assert.deepEqual(rules.temporaryResources, {
    ownershipMarkerRequired: true,
    gracefulShutdownReleasesAll: true,
    arbitraryDirectoryDeletionAllowed: false,
  })
})

test('SQLite and PostgreSQL remain adapters behind one product storage port', () => {
  const adapters = json(rulesPath).storageAdapters

  assert.deepEqual(adapters.port, {
    crate: 'winwincode-storage',
    trait: 'ProductStateStorage',
    semanticsOwner: 'control-plane-domain',
  })
  assert.deepEqual(adapters.local, {
    kind: 'sqlite',
    type: 'SqliteStorage',
    status: 'phase-2.1-required',
  })
  assert.deepEqual(adapters.enterprise, {
    kind: 'postgresql',
    type: 'PostgresStorage',
    status: 'future-adapter',
  })
  assert.deepEqual(adapters.requiredParity, [
    'transaction-boundaries',
    'revision-conflict-result',
    'canonical-append-order',
    'outbox-recovery',
    'migration-before-serving',
    'deterministic-close',
  ])
})

test('storage lifecycle rules agree with the accepted ADR-0028 target graph', () => {
  const rules = json(rulesPath)
  const graph = json(targetGraphPath)
  const nodes = new Map(graph.nodes.map(node => [node.id, node]))
  const controlPlane = nodes.get(rules.ownership.canonicalStateWriter)
  const storage = nodes.get(rules.ownership.storagePortOwner)
  const worker = nodes.get('winwincode-worker')

  assert.ok(controlPlane)
  assert.equal(controlPlane.zone, 'control-plane')
  assert.ok(controlPlane.responsibilities.includes('remain-the-only-product-state-writer'))
  assert.ok(controlPlane.allowedInternalDependencies.includes(storage.id))
  assert.ok(storage)
  assert.equal(storage.zone, 'shared')
  assert.deepEqual(storage.responsibilities, [
    'provide-control-plane-storage-adapters',
    'provide-transaction-and-revision-primitives',
  ])
  assert.deepEqual(storage.allowedInternalDependencies, ['winwincode-domain'])
  assert.equal(worker.allowedInternalDependencies.includes(storage.id), false)
  for (const node of graph.nodes) {
    if (!node.allowedInternalDependencies.includes(storage.id)) continue
    assert.equal(
      node.zone,
      'control-plane',
      `${node.id} reaches product storage from outside the Control Plane zone`,
    )
  }
  assert.equal(
    graph.interfaces.some(interface_ => interface_.provider === storage.id),
    false,
    'storage is an internal application port, not a Worker or Web protocol',
  )
})

test('storage lifecycle documentation explains every enforced outcome in plain terms', () => {
  const rules = json(rulesPath)
  const text = readFileSync(documentationPath, 'utf8')

  for (const statement of [
    '目标门禁，不是实现完成声明',
    '同一个数据库事务',
    '先完成迁移，再接收命令',
    '提交前不得发布事件',
    '只重放已经提交但尚未发布的 outbox 事件',
    'Control Plane 是产品状态的唯一写入方',
    'SQLite',
    'PostgreSQL',
    'Control Plane 不依赖 Codex Core',
    '关闭完成后不得留下仍在运行的全局后台任务',
    '`ProductStateStorage`',
    '`ControlPlane::start_local`',
    '`ControlPlane::commit`',
    '`ControlPlane::shutdown`',
  ]) assert.ok(text.includes(statement), `missing documentation statement: ${statement}`)

  for (const step of [
    ...expectedCommitOrder,
    ...expectedStartupOrder,
    ...expectedShutdownOrder,
  ]) assert.ok(text.includes(`\`${step}\``), `undocumented lifecycle step: ${step}`)

  assert.equal(rules.documentation, relative(root, documentationPath))
})

test('future phase 2.1 crates must expose and pass the frozen Rust black-box seam', () => {
  const rules = json(rulesPath)
  const gate = rules.rustGate
  assert.deepEqual(gate.activation, {
    condition: 'either-required-manifest-exists',
    effect: 'require-both-manifests-workspace-membership-interface-and-tests',
  })
  assert.deepEqual(gate.requiredPackages, [
    {
      id: 'winwincode-storage',
      manifest: 'crates/winwincode-storage/Cargo.toml',
    },
    {
      id: 'winwincode-control-plane',
      manifest: 'crates/winwincode-control-plane/Cargo.toml',
    },
  ])
  assert.deepEqual(gate.publicInterfaces, {
    'winwincode-control-plane': [
      'ControlPlane',
      'ControlPlaneConfig',
      'EventPublisher',
      'StateChange',
      'CommitReceipt',
      'ShutdownReport',
      'StartError',
      'CommitError',
      'ShutdownError',
      'ControlPlane::start_local',
      'ControlPlane::commit',
      'ControlPlane::shutdown',
    ],
    'winwincode-storage': [
      'ReceiptActorKey',
      'ReceiptScopeKey',
      'ReceiptIdentity',
      'StateCommit',
      'ProductStateStorage',
      'SqliteStorage',
      'SqliteStorage::open',
      'ProductStateStorage::commit',
      'ProductStateStorage::load_state',
      'ProductStateStorage::pending_events',
      'ProductStateStorage::mark_published',
      'ProductStateStorage::close',
    ],
  })
  assert.equal(gate.integrationTest.path, 'crates/winwincode-control-plane/tests/lifecycle.rs')
  assert.deepEqual(gate.integrationTest.requiredTests, expectedLifecycleTests)

  const manifests = gate.requiredPackages.map(package_ => join(root, package_.manifest))
  if (manifests.every(path => !existsSync(path))) return
  for (const path of manifests) assert.equal(existsSync(path), true, `${relative(root, path)} is required`)

  const metadataResult = spawnSync(
    'cargo',
    ['metadata', '--format-version', '1', '--locked', '--no-deps'],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(metadataResult.status, 0, commandFailure(metadataResult))
  const packages = JSON.parse(metadataResult.stdout).packages
  const packagesByName = new Map(packages.map(package_ => [package_.name, package_]))
  const graph = json(targetGraphPath)
  const graphNodes = new Map(graph.nodes.map(node => [node.id, node]))

  for (const requiredPackage of gate.requiredPackages) {
    const package_ = packagesByName.get(requiredPackage.id)
    const targetNode = graphNodes.get(requiredPackage.id)
    assert.ok(package_, `${requiredPackage.id} is not a Rust workspace package`)
    assert.ok(targetNode, `${requiredPackage.id} is not in the accepted target graph`)
    for (const dependency of package_.dependencies) {
      if (!dependency.name.startsWith('winwincode-')) continue
      assert.ok(
        targetNode.allowedInternalDependencies.includes(dependency.name),
        `${requiredPackage.id} has forbidden product dependency ${dependency.name}`,
      )
    }
  }

  const controlPlaneClosure = transitivePackageDependencies(
    'winwincode-control-plane',
    packagesByName,
  )
  for (const packageName of controlPlaneClosure) {
    const dependencyNames = [
      packageName,
      ...packagesByName.get(packageName).dependencies.map(dependency => dependency.name),
    ]
    for (const dependencyName of dependencyNames) {
      for (const pattern of gate.forbiddenDependencyPatterns) {
        assert.equal(
          packageMatchesPattern(dependencyName, pattern),
          false,
          `Control Plane reaches forbidden dependency ${dependencyName}`,
        )
      }
    }
  }

  const implementationSources = gate.requiredPackages.flatMap(package_ => (
    filesBelow(join(root, package_.manifest, '..', 'src'))
      .filter(path => extname(path) === '.rs')
  ))
  for (const path of implementationSources) {
    const source = readFileSync(path, 'utf8')
    for (const pattern of gate.forbiddenGlobalSourcePatterns) {
      assert.doesNotMatch(
        source,
        new RegExp(pattern, 'u'),
        `${relative(root, path)} declares a process-global background runtime`,
      )
    }
  }

  const integrationTestPath = join(root, gate.integrationTest.path)
  assert.equal(existsSync(integrationTestPath), true)
  const integrationTestSource = readFileSync(integrationTestPath, 'utf8')
  for (const interfaceName of Object.values(gate.publicInterfaces).flat()) {
    const symbol = interfaceName.split('::').at(-1)
    assert.ok(
      integrationTestSource.includes(symbol),
      `${gate.integrationTest.path} does not exercise ${interfaceName}`,
    )
  }

  const listed = spawnSync('cargo', [
    'test',
    '--locked',
    '--package',
    gate.integrationTest.package,
    '--test',
    gate.integrationTest.target,
    '--',
    '--list',
  ], { cwd: root, encoding: 'utf8' })
  assert.equal(listed.status, 0, commandFailure(listed))
  for (const testName of gate.integrationTest.requiredTests) {
    assert.match(listed.stdout, new RegExp(`^${testName}: test$`, 'mu'))
  }

  const executed = spawnSync('cargo', [
    'test',
    '--locked',
    '--package',
    gate.integrationTest.package,
    '--test',
    gate.integrationTest.target,
  ], { cwd: root, encoding: 'utf8' })
  assert.equal(executed.status, 0, commandFailure(executed))
})
