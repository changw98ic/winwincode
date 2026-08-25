import assert from 'node:assert/strict'
import { readFileSync, writeFileSync } from 'node:fs'
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import test from 'node:test'

import {
  assertDifferentialResult,
  buildCanonicalMigrationPlan,
  buildDifferentialExecutionPlan,
  findFirstJsonDifference,
  normalizeDifferentialResult,
  runDifferentialGate,
  validateCanonicalExpected,
  validateDifferentialContract,
} from '../scripts/delivery-strongflow-differential-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'delivery-strongflow-rust-differential.rules.json',
)
const oraclePath = join(
  root,
  'tests',
  'fixtures',
  'oracles',
  'delivery-strongflow-typescript.v1.json',
)

async function json(path) {
  return JSON.parse(await readFile(path, 'utf8'))
}

function testBindings(overrides = {}) {
  return {
    AUTH_PROOF: 'contract-gate-auth-proof',
    NODE_EXECUTABLE: '/fixture/bin/node',
    ORACLE_ROOT: '/fixture/oracle-root',
    fixtureRandomIdentities: {},
    ...overrides,
  }
}

async function fixtureRepository(rules, oracle, options = {}) {
  const fixtureRoot = await mkdtemp(join(tmpdir(), 'winwincode-differential-gate-'))
  const fixtureRulesPath = join(
    fixtureRoot,
    'docs/contracts/delivery-strongflow-rust-differential.rules.json',
  )
  const fixtureOraclePath = join(
    fixtureRoot,
    'tests/fixtures/oracles/delivery-strongflow-typescript.v1.json',
  )
  await mkdir(dirname(fixtureRulesPath), { recursive: true })
  await mkdir(dirname(fixtureOraclePath), { recursive: true })
  await writeFile(fixtureRulesPath, `${JSON.stringify(rules, null, 2)}\n`)
  await writeFile(fixtureOraclePath, `${JSON.stringify(oracle, null, 2)}\n`)
  if (options.expected !== undefined) {
    const fixtureExpectedPath = join(fixtureRoot, rules.canonicalExpected.path)
    await mkdir(dirname(fixtureExpectedPath), { recursive: true })
    await writeFile(
      fixtureExpectedPath,
      `${JSON.stringify(options.expected, null, 2)}\n`,
    )
  }
  if (options.activate === true) {
    for (const path of rules.runner.requiredPaths) {
      const absolute = join(fixtureRoot, path)
      await mkdir(dirname(absolute), { recursive: true })
      await writeFile(absolute, '// differential runner fixture\n')
    }
  }
  return fixtureRoot
}

test('machine rules freeze the ten scenario execution plan and complete result shape', async () => {
  const [rules, oracle] = await Promise.all([json(rulesPath), json(oraclePath)])

  const validated = validateDifferentialContract(rules, oracle)
  assert.equal(validated.scenarioCount, 10)
  assert.deepEqual(validated.scenarioIds, [
    'success-closed-loop',
    'request-id-replay',
    'revision-conflict',
    'corruption-recovery',
    'task-dag',
    'candidate-invalidation',
    'attention',
    'inconclusive',
    'infra-error',
    'rework',
  ])

  const plan = buildDifferentialExecutionPlan(oracle, testBindings(), rules)
  assert.deepEqual(Object.keys(plan).sort(), [
    'bindings',
    'oracleSchemaVersion',
    'scenarios',
    'schemaVersion',
  ])
  assert.deepEqual(Object.keys(plan.scenarios[0]).sort(), ['commands', 'id'])
  assert.equal(JSON.stringify(plan).includes('"response"'), false)
  assert.equal(JSON.stringify(plan).includes('"assertions"'), false)
  assert.equal(JSON.stringify(plan).includes('"observation"'), false)
  assert.equal(
    plan.scenarios.find(scenario => scenario.id === 'task-dag')
      .commands[0].kind,
    'fixture.store.seed-snapshot',
  )

  const migrationPlan = buildCanonicalMigrationPlan(oracle, rules)
  assert.deepEqual(Object.keys(migrationPlan).sort(), [
    'mappingVersion',
    'scenarios',
    'schemaVersion',
    'sourceOracleSha256',
  ])
  assert.equal(migrationPlan.scenarios.length, 10)
  assert.deepEqual(
    migrationPlan.scenarios[0].commands[0].canonicalTargets,
    ['delivery.create'],
  )
  assert.deepEqual(
    migrationPlan.scenarios[0].commands.slice(-2)
      .flatMap(command => command.canonicalTargets),
    ['delivery.get', 'runtime.projection.get'],
  )
  assert.deepEqual(
    rules.canonicalMigration.taskPromotionInsertion.appliesToScenarioIds,
    [
      'success-closed-loop',
      'candidate-invalidation',
      'attention',
      'inconclusive',
      'infra-error',
      'rework',
    ],
  )
  assert.match(
    rules.canonicalMigration.taskPromotionInsertion.taskIdentityAlgorithm,
    /winwincode\.solution-review-default-task\.v1\\0/u,
  )
  for (const id of rules.canonicalMigration.taskPromotionInsertion.appliesToScenarioIds) {
    const groups = rules.scenarios.find(scenario => scenario.id === id).canonicalGroups
    assert.deepEqual(
      groups.find(group => group.target === 'delivery.advance'
        && group.sourceCommandIndexes.includes(5)).sourceCommandIndexes,
      [5],
      id,
    )
    assert.deepEqual(
      groups.find(group => group.kind === 'execution-port.message'
        && group.sourceCommandIndexes.includes(6))
        .sourceCommandIndexes,
      [6],
      id,
    )
    assert.deepEqual(
      groups.find(group => group.kind === 'execution-port.message'
        && group.sourceCommandIndexes.includes(7)).revisionEffect,
      { delta: 2 },
      id,
    )
    assert.deepEqual(
      groups.find(group => group.target === 'delivery.advance'
        && group.sourceCommandIndexes.includes(8)).sourceCommandIndexes,
      [8, 9],
      id,
    )
    const approvalIndex = groups.findIndex(group => (
      group.target === 'delivery.approve_task_breakdown'
    ))
    assert.equal(approvalIndex > 0, true, id)
    assert.equal(groups[approvalIndex - 1].target, 'delivery.resolve_attention', id)
    assert.deepEqual(groups[approvalIndex].sourceCommandIndexes, [10], id)
  }
  assert.equal(
    rules.scenarios.find(scenario => scenario.id === 'task-dag')
      .canonicalGroups.find(group => group.sourceCommandIndexes.includes(2)).target,
    'delivery.approve_task_breakdown',
  )
  assert.deepEqual(
    rules.scenarios.find(scenario => scenario.id === 'corruption-recovery')
      .canonicalGroups.filter(group => group.sourceCommandIndexes.includes(2))
      .map(group => group.target),
    ['delivery.get'],
  )
  assert.deepEqual(
    rules.canonicalMigration.errorMappings
      .filter(mapping => ['DELIVERY_CONFLICT', 'STORE_FAILURE'].includes(mapping.legacyCode))
      .map(mapping => mapping.canonicalCode),
    ['SERVICE_UNAVAILABLE', 'WRONG_STATE'],
  )
  assert.deepEqual(
    Object.fromEntries(
      ['success-closed-loop', 'attention', 'inconclusive', 'infra-error',
        'candidate-invalidation', 'rework'].map(id => {
        const assertions = rules.scenarios.find(scenario => scenario.id === id)
          .canonicalAssertions
        return [id, [
          assertions.finalRevision,
          assertions.stageRunCount,
          assertions.sessionBindingCount,
          assertions.taskCount,
        ]]
      }),
    ),
    {
      'attention': [8, 2, 1, 1],
      'candidate-invalidation': [29, 8, 7, 1],
      'inconclusive': [18, 5, 4, 1],
      'infra-error': [18, 5, 4, 1],
      'rework': [29, 8, 7, 1],
      'success-closed-loop': [20, 6, 4, 1],
    },
  )
  assert.deepEqual(
    rules.scenarios.find(scenario => scenario.id === 'task-dag').canonicalAssertions,
    {
      advancedTaskId: 'oracle-task-prerequisite',
      cycleError: 'INVALID_REQUEST',
      cycleRejectedWithoutWrite: true,
      durableTaskOrder: ['oracle-task-prerequisite', 'oracle-task-dependent'],
      finalRevision: 2,
      sessionBindingCount: 1,
      stageRunCount: 1,
      taskCount: 2,
    },
  )
})

test('normalization substitutes only exact runtime bindings and preserves product facts', async () => {
  const rules = await json(rulesPath)
  const actual = {
    authentication: { proof: 'contract-gate-auth-proof' },
    candidate: { candidateRef: 'candidate-exact' },
    error: { code: 'REVISION_CONFLICT', currentRevision: 7 },
    event: { command: ['/fixture/bin/node', '/fixture/oracle-root/check.mjs'] },
    evidence: [{ id: 'evidence-exact', sequence: 3 }],
    revision: 7,
    store: { records: [{ digest: 'digest-exact', sequence: 1 }] },
    verdict: { status: 'inconclusive' },
  }

  assert.deepEqual(normalizeDifferentialResult(actual, testBindings(), rules), {
    authentication: { proof: '<AUTH_PROOF>' },
    candidate: { candidateRef: 'candidate-exact' },
    error: { code: 'REVISION_CONFLICT', currentRevision: 7 },
    event: { command: ['<NODE_EXECUTABLE>', '<ORACLE_ROOT>/check.mjs'] },
    evidence: [{ id: 'evidence-exact', sequence: 3 }],
    revision: 7,
    store: { records: [{ digest: 'digest-exact', sequence: 1 }] },
    verdict: { status: 'inconclusive' },
  })

  assert.throws(
    () => normalizeDifferentialResult(
      { path: '<ORACLE_ROOT>/already-normalized' },
      testBindings(),
      rules,
    ),
    /raw Rust result contains reserved placeholder <ORACLE_ROOT>/u,
  )
  assert.throws(
    () => normalizeDifferentialResult(actual, testBindings({ extra: 'forbidden' }), rules),
    /unknown normalization binding: extra/u,
  )
  assert.throws(
    () => normalizeDifferentialResult(
      actual,
      testBindings({ fixtureRandomIdentities: { candidateRef: 'random' } }),
      rules,
    ),
    /fixture random identity candidateRef is not declared/u,
  )
})

test('full-value comparison reports the first exact RFC 6901 leaf path', async () => {
  const rules = await json(rulesPath)
  const expected = {
    scenarios: [{
      id: 'revision-conflict',
      observation: {
        events: [{ sequence: 1 }, { sequence: 2 }],
        snapshot: { revision: 7 },
      },
    }],
  }
  const changedRevision = structuredClone(expected)
  changedRevision.scenarios[0].observation.snapshot.revision = 8
  assert.deepEqual(findFirstJsonDifference(expected, changedRevision), {
    actual: 8,
    expected: 7,
    kind: 'value',
    path: '/scenarios/0/observation/snapshot/revision',
  })

  const reorderedEvents = structuredClone(expected)
  reorderedEvents.scenarios[0].observation.events.reverse()
  assert.deepEqual(findFirstJsonDifference(expected, reorderedEvents), {
    actual: 2,
    expected: 1,
    kind: 'value',
    path: '/scenarios/0/observation/events/0/sequence',
  })

  assert.throws(
    () => assertDifferentialResult(expected, changedRevision, testBindings(), rules),
    error => {
      assert.equal(error.code, 'DIFFERENTIAL_MISMATCH')
      assert.equal(error.difference.path, '/scenarios/0/observation/snapshot/revision')
      assert.match(error.message, /expected 7, actual 8/u)
      return true
    },
  )
})

test('canonical expected rejects open public and fixture envelopes', async () => {
  const [rules, oracle] = await Promise.all([json(rulesPath), json(oraclePath)])
  const expected = canonicalExpectedFixture(oracle, rules)
  validateCanonicalExpected(rules, oracle, expected)

  const openPublicRequest = structuredClone(expected)
  openPublicRequest.result.scenarios[0].commands[0].request.legacyOperation = 'createDelivery'
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, openPublicRequest),
    /control-plane\.command request keys differ/u,
  )

  const oldErrorCode = structuredClone(expected)
  oldErrorCode.result.scenarios[0].commands[1].response.error.code = 'DELIVERY_CONFLICT'
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, oldErrorCode),
    /unknown canonical error code DELIVERY_CONFLICT/u,
  )

  const fixtureCommand = expected.result.scenarios[0].commands.find(command => (
    command.request.kind === 'fixture.runtime-source.replace'
  ))
  const openFixtureResult = structuredClone(expected)
  const openFixtureCommand = openFixtureResult.result.scenarios[0].commands.find(command => (
    command.request.kind === fixtureCommand.request.kind
  ))
  openFixtureCommand.response.result.legacyResponse = true
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, openFixtureResult),
    /fixture result keys differ/u,
  )

  const openJournalRecord = structuredClone(expected)
  openJournalRecord.result.scenarios[0].observation.store.journal
    .records[0].snapshotRevision = 12
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, openJournalRecord),
    /journal\.records\[0\] keys differ/u,
  )

  const replayAsPersistedFact = structuredClone(expected)
  replayAsPersistedFact.result.scenarios[0].observation.store.receipts.push({
    actorKey: 'actor:fixture',
    events: [],
    idempotentReplay: true,
    requestId: 'req_00000000000000000000000000',
    revision: 1,
    scopeKey: 'scope:fixture',
    streamId: 'delivery:fixture',
  })
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, replayAsPersistedFact),
    /receipts\[0\]\.idempotentReplay must be false/u,
  )
})

test('gate validates the plan while absent and runs the exact Rust target once triggered', async (t) => {
  const [rules, oracle] = await Promise.all([json(rulesPath), json(oraclePath)])
  const absentRoot = await fixtureRepository(rules, oracle)
  t.after(() => rm(absentRoot, { recursive: true, force: true }))

  let absentSpawned = false
  const absent = await runDifferentialGate({
    root: absentRoot,
    spawn() {
      absentSpawned = true
      return { status: 0, signal: null, stdout: '', stderr: '' }
    },
  })
  assert.equal(absent.status, 'contract-only')
  assert.equal(absent.scenarioCount, 10)
  assert.equal(absentSpawned, false)

  const expected = canonicalExpectedFixture(oracle, rules)
  validateCanonicalExpected(rules, oracle, expected)
  const activeRoot = await fixtureRepository(rules, oracle, {
    activate: true,
    expected,
  })
  t.after(() => rm(activeRoot, { recursive: true, force: true }))
  const calls = []
  const active = await runDifferentialGate({
    root: activeRoot,
    spawn(command, arguments_, options) {
      calls.push({ command, arguments_, options })
      const plan = JSON.parse(
        // The gate writes the plan before starting Cargo.
        requireRead(options.env.WINWINCODE_DELIVERY_DIFFERENTIAL_INPUT),
      )
      assert.equal(JSON.stringify(plan).includes('"response"'), false)
      const raw = hydrateExpectedResult(expected.result, plan.bindings)
      requireWrite(options.env.WINWINCODE_DELIVERY_DIFFERENTIAL_OUTPUT, raw)
      return { status: 0, signal: null, stdout: 'runner completed', stderr: '' }
    },
  })

  assert.equal(active.status, 'matched')
  assert.equal(active.scenarioCount, 10)
  assert.equal(calls.length, 1)
  assert.equal(calls[0].command, 'cargo')
  assert.deepEqual(calls[0].arguments_, [
    'test',
    '-p',
    'winwincode-control-plane',
    '--features',
    'test-support',
    '--test',
    'delivery_strongflow_differential_runner',
  ])
})

test('triggered gate executes Cargo before reporting a missing typed baseline', async (t) => {
  const [rules, oracle] = await Promise.all([json(rulesPath), json(oraclePath)])
  const activeRoot = await fixtureRepository(rules, oracle, { activate: true })
  t.after(() => rm(activeRoot, { recursive: true, force: true }))

  let calls = 0
  await assert.rejects(
    runDifferentialGate({
      root: activeRoot,
      spawn(_command, _arguments, options) {
        calls += 1
        requireWrite(options.env.WINWINCODE_DELIVERY_DIFFERENTIAL_OUTPUT, {
          oracleSchemaVersion: oracle.schemaVersion,
          scenarios: [],
          schemaVersion: rules.canonicalExpected.resultSchemaVersion,
        })
        return { status: 0, signal: null, stderr: '', stdout: '' }
      },
    }),
    /triggered Rust differential runner requires tests\/fixtures\/oracles\//u,
  )
  assert.equal(calls, 1)
})

test('triggered gate rejects one deliberate product-fact difference at its first leaf', async (t) => {
  const [rules, oracle] = await Promise.all([json(rulesPath), json(oraclePath)])
  const expected = canonicalExpectedFixture(oracle, rules)
  const activeRoot = await fixtureRepository(rules, oracle, {
    activate: true,
    expected,
  })
  t.after(() => rm(activeRoot, { recursive: true, force: true }))

  await assert.rejects(
    runDifferentialGate({
      root: activeRoot,
      spawn(_command, _arguments, options) {
        const plan = JSON.parse(requireRead(
          options.env.WINWINCODE_DELIVERY_DIFFERENTIAL_INPUT,
        ))
        const raw = hydrateExpectedResult(expected.result, plan.bindings)
        raw.scenarios[2].observation.snapshot.revision += 1
        requireWrite(options.env.WINWINCODE_DELIVERY_DIFFERENTIAL_OUTPUT, raw)
        return { status: 0, signal: null, stdout: '', stderr: '' }
      },
    }),
    error => {
      assert.equal(error.code, 'DIFFERENTIAL_MISMATCH')
      assert.equal(
        error.difference.path,
        '/scenarios/2/observation/snapshot/revision',
      )
      return true
    },
  )
})

test('workspace test lane always invokes the trigger-aware Rust differential gate', async () => {
  const [manifest, runner] = await Promise.all([
    json(join(root, 'package.json')),
    readFile(join(root, 'scripts/run-ts-tests.mjs'), 'utf8'),
  ])
  assert.equal(
    manifest.scripts['oracle:delivery:rust:check'],
    'node scripts/run-delivery-strongflow-rust-differential.mjs --check',
  )
  const legacyCheck = "runTests(['scripts/export-delivery-strongflow-oracle.mjs', '--check'])"
  const rustCheck = "runTests(['scripts/run-delivery-strongflow-rust-differential.mjs', '--check'])"
  assert.equal(runner.includes(legacyCheck), true)
  assert.equal(runner.includes(rustCheck), true)
  assert.equal(runner.indexOf(rustCheck) > runner.indexOf(legacyCheck), true)
})

// Synchronous helpers keep the injected spawn seam identical to spawnSync.
function requireRead(path) {
  return readFileSync(path, 'utf8')
}

function requireWrite(path, value) {
  writeFileSync(path, `${JSON.stringify(value)}\n`)
}

function hydrateExpectedResult(value, bindings) {
  if (typeof value === 'string') {
    return value
      .split('<ORACLE_ROOT>').join(bindings.ORACLE_ROOT)
      .split('<NODE_EXECUTABLE>').join(bindings.NODE_EXECUTABLE)
      .split('<AUTH_PROOF>').join(bindings.AUTH_PROOF)
  }
  if (Array.isArray(value)) return value.map(entry => hydrateExpectedResult(entry, bindings))
  if (value === null || typeof value !== 'object') return value
  return Object.fromEntries(
    Object.entries(value).map(([key, entry]) => [key, hydrateExpectedResult(entry, bindings)]),
  )
}

function canonicalExpectedFixture(oracle, rules) {
  const scenarioRules = new Map(rules.scenarios.map(scenario => [scenario.id, scenario]))
  return {
    schemaVersion: rules.canonicalExpected.schemaVersion,
    migration: {
      mappingVersion: rules.canonicalMigration.mappingVersion,
      sourceOracleSchemaVersion: oracle.schemaVersion,
      sourceOracleSha256: rules.oracle.sha256,
    },
    result: {
      schemaVersion: rules.canonicalExpected.resultSchemaVersion,
      oracleSchemaVersion: oracle.schemaVersion,
      scenarios: oracle.scenarios.map(scenario => {
        const mapping = scenarioRules.get(scenario.id)
        const migrated = {
          id: scenario.id,
          commands: mapping.canonicalGroups.map((group, targetIndex) => {
          const sourceCommand = scenario.commands[group.sourceCommandIndexes.at(-1)]
          const legacy = sourceCommand.kind === 'strongflow.request'
            ? sourceCommand.request.operation
            : sourceCommand.kind
          return {
            sourceCommandIndexes: group.sourceCommandIndexes,
            kind: group.kind,
            request: canonicalFixtureRequest(group, sourceCommand, targetIndex),
            response: canonicalFixtureResponse(
              group,
              sourceCommand,
              migrateFixtureResponse(sourceCommand.response, legacy, rules),
              targetIndex,
              scenario,
              mapping,
            ),
          }
        }),
        observation: {
          events: structuredClone(scenario.observation.events),
          projection: {
            delivery: structuredClone(scenario.observation.snapshot),
            runtime: structuredClone(scenario.observation.projection.runtimeExecution),
          },
          snapshot: structuredClone(scenario.observation.snapshot),
          store: {
            journal: canonicalFixtureJournal(scenario),
            outbox: [],
            receipts: [],
            state: {
              revision: scenario.observation.snapshot.revision,
              snapshot: structuredClone(scenario.observation.snapshot),
              streamId: scenario.observation.snapshot.id,
            },
          },
          verdict: structuredClone(scenario.observation.verdict),
        },
        }
        applyCanonicalFixtureFacts(migrated, mapping)
        return migrated
      }),
    },
  }
}

function applyCanonicalFixtureFacts(scenario, mapping) {
  const assertions = mapping.canonicalAssertions
  const snapshot = scenario.observation.snapshot
  if (assertions.finalRevision !== undefined) snapshot.revision = assertions.finalRevision
  if (assertions.stageRunCount !== undefined) {
    snapshot.stageRuns.length = assertions.stageRunCount
  }
  if (assertions.sessionBindingCount !== undefined) {
    snapshot.sessionBindings.length = assertions.sessionBindingCount
  }
  if (assertions.taskCount !== undefined) {
    while (snapshot.tasks.length < assertions.taskCount) {
      snapshot.tasks.push({
        id: `dtk_${snapshot.tasks.length.toString().padStart(26, '0')}`,
        status: 'pending',
      })
    }
    snapshot.tasks.length = assertions.taskCount
  }
  if (scenario.id === 'task-dag') {
    snapshot.revision = 2
    snapshot.tasks[0].status = 'active'
    snapshot.tasks[1].status = 'blocked'
  }

  scenario.observation.store.state.revision = snapshot.revision
  scenario.observation.store.state.snapshot = structuredClone(snapshot)
  scenario.observation.store.journal.snapshot = structuredClone(snapshot)
  const lastRecord = scenario.observation.store.journal.records.at(-1)
  if (lastRecord !== undefined) lastRecord.snapshot = structuredClone(snapshot)
}

function canonicalFixtureJournal(scenario) {
  const snapshots = []
  for (const command of scenario.commands) {
    const snapshot = command.response?.result?.delivery
    if (snapshot === undefined) continue
    if (snapshots.some(entry => entry.revision === snapshot.revision)) continue
    snapshots.push(structuredClone(snapshot))
  }
  if (snapshots.length === 0) snapshots.push(structuredClone(scenario.observation.snapshot))
  const recordCount = scenario.observation.store.records.length
  while (snapshots.length < recordCount) {
    snapshots.push(structuredClone(scenario.observation.snapshot))
  }
  return {
    manifest: structuredClone(scenario.observation.store.manifest),
    records: scenario.observation.store.records.map((record, index) => {
      const snapshot = snapshots[index] ?? structuredClone(scenario.observation.snapshot)
      return {
        deliveryId: snapshot.id,
        digest: record.digest,
        operation: record.operation,
        previousDigest: record.previousDigest,
        requestDigest: record.requestDigest,
        requestId: record.requestId,
        schemaVersion: record.schemaVersion,
        sequence: record.sequence,
        snapshot,
      }
    }),
    snapshot: structuredClone(scenario.observation.snapshot),
  }
}

function migrateFixtureResponse(response, legacy, rules) {
  const migrated = structuredClone(response)
  if (migrated?.error?.code === undefined) return migrated
  const rule = rules.canonicalMigration.errorMappings.find(mapping => (
    mapping.legacyOperation === legacy
      && mapping.legacyCode === migrated.error.code
  ))
  if (rule !== undefined) migrated.error.code = rule.canonicalCode
  return migrated
}

function canonicalFixtureRequest(group, sourceCommand, targetIndex) {
  if (group.kind === 'fixture.command') {
    return { input: structuredClone(sourceCommand.input ?? sourceCommand.request.payload), kind: group.target }
  }
  const source = sourceCommand.request
  const shared = {
    actor: { id: 'usr_00000000000000000000000000', kind: 'user' },
    requestId: canonicalFixtureRequestId(group.sourceCommandIndexes[0], targetIndex),
    schemaVersion: 'winwincode/v1',
    scope: {
      kind: 'repository',
      organizationId: 'org_00000000000000000000000000',
      projectId: 'prj_00000000000000000000000000',
      repositoryId: 'rep_00000000000000000000000000',
      workspaceId: 'wsp_00000000000000000000000000',
    },
  }
  if (group.kind === 'execution-port.message') {
    const instant = '2026-01-01T00:00:00Z'
    return {
      boundAt: instant,
      codexThreadId: 'cdx_00000000000000000000000000',
      kind: 'session.binding',
      lease: {
        attempt: 1,
        expiresAt: '2026-01-01T00:01:00Z',
        fencingToken: '1',
        issuedAt: instant,
        jobId: 'job_00000000000000000000000000',
        leaseId: 'lea_00000000000000000000000000',
        workerId: 'wrk_00000000000000000000000000',
        workerInstanceId: 'wri_00000000000000000000000000',
      },
      messageId: `xmsg_${canonicalFixtureRequestId(
        group.sourceCommandIndexes[0],
        targetIndex,
      ).slice(4)}`,
      productSessionId: 'psn_00000000000000000000000000',
      schemaVersion: 'winwincode/v1',
      sentAt: instant,
      workerSessionId: 'wsn_00000000000000000000000000',
    }
  }
  if (group.kind === 'control-plane.query') {
    const parameters = group.target === 'runtime.projection.get'
      ? {
          atCursor: 'cursor_fixture_00000000000000000000000000',
          deliveryId: source.payload.deliveryId,
          kind: 'delivery-stage',
          productSessionId: 'psn_00000000000000000000000000',
          stageRunId: 'str_00000000000000000000000000',
        }
      : { deliveryId: source.payload.deliveryId }
    return {
      ...shared,
      page: { cursor: null, limit: 50 },
      parameters,
      query: group.target,
    }
  }
  return {
    ...shared,
    command: group.target,
    expectedRevision: source.payload.expectedRevision ?? 0,
    payload: canonicalFixturePayload(group.target, source.payload),
  }
}

function canonicalFixturePayload(target, payload) {
  switch (target) {
    case 'delivery.create':
      return { deliveryId: payload.spec.deliveryId, spec: payload.spec, tasks: payload.tasks }
    case 'delivery.update_spec':
      return { deliveryId: payload.deliveryId, spec: payload.spec }
    case 'delivery.advance':
      return { deliveryId: payload.deliveryId }
    case 'delivery.resolve_attention':
      return {
        attentionItemId: payload.attentionItemId,
        decision: payload.status === 'resolved' ? 'resolve' : 'dismiss',
        deliveryId: payload.deliveryId,
        remediation: payload.remediation,
        resolution: payload.resolution,
      }
    case 'delivery.submit_verdict':
      return {
        candidateDigest: `sha256:${payload.candidate.diffSha256}`,
        deliveryId: payload.deliveryId,
      }
    case 'delivery.approve_task_breakdown':
      return {
        deliveryId: payload.deliveryId ?? payload.spec.deliveryId,
        reviewSetSha256: `sha256:${'0'.repeat(64)}`,
      }
    default:
      throw new Error(`unknown canonical fixture target: ${target}`)
  }
}

function canonicalFixtureResponse(
  group,
  sourceCommand,
  response,
  targetIndex,
  scenario,
  mapping,
) {
  if (group.kind === 'fixture.command') {
    return canonicalFixtureCommandResponse(group.target, sourceCommand)
  }
  if (group.kind === 'execution-port.message') {
    return canonicalFixtureMessageResponse(group, sourceCommand, targetIndex, mapping)
  }
  const requestId = canonicalFixtureRequestId(group.sourceCommandIndexes[0], targetIndex)
  if (scenario.id === 'task-dag'
    && group.target === 'delivery.advance'
    && group.sourceCommandIndexes.includes(1)) {
    return {
      command: group.target,
      currentRevision: 2,
      outcome: 'completed',
      previousRevision: 1,
      requestId,
      result: structuredClone(scenario.observation.snapshot),
      schemaVersion: 'winwincode/v1',
    }
  }
  if (response?.error !== undefined) {
    return {
      error: {
        code: response.error.code,
        details: response.error.currentRevision === null
          ? {}
          : { currentRevision: response.error.currentRevision },
        message: response.error.message,
        retryable: response.error.code === 'SERVICE_UNAVAILABLE',
      },
      requestId,
      schemaVersion: 'winwincode/v1',
    }
  }
  if (group.kind === 'control-plane.query') {
    return {
      page: { hasMore: false, nextCursor: null },
      query: group.target,
      requestId,
      result: group.target === 'delivery.get'
        ? structuredClone(response.result.delivery)
        : structuredClone(response.result.runtimeExecution),
      schemaVersion: 'winwincode/v1',
    }
  }
  const revision = response.result.delivery.revision
  return {
    command: group.target,
    currentRevision: revision,
    outcome: 'completed',
    previousRevision: Math.max(0, revision - 1),
    requestId,
    result: structuredClone(response.result.delivery),
    schemaVersion: 'winwincode/v1',
  }
}

function canonicalFixtureMessageResponse(group, sourceCommand, targetIndex, mapping) {
  const request = canonicalFixtureRequest(group, sourceCommand, targetIndex)
  const previousRevision = mapping.canonicalGroups.slice(0, targetIndex)
    .reduce((revision, entry) => (
      entry.revisionEffect.seed ?? revision + entry.revisionEffect.delta
    ), 0)
  if (group.revisionEffect.delta === 0) {
    return {
      currentRevision: previousRevision,
      error: {
        code: 'INVALID_REQUEST',
        details: { currentRevision: previousRevision },
        message: 'session binding does not match an active Codex job',
        retryable: false,
      },
      messageId: request.messageId,
      outcome: 'rejected',
    }
  }
  const commits = ['accept_worker_session', 'report_codex_thread'].map((operation, index) => ({
    currentRevision: previousRevision + index + 1,
    operation,
    previousRevision: previousRevision + index,
    receipt: canonicalFixtureReceipt(previousRevision + index + 1, request.messageId, operation),
  }))
  return {
    commits,
    currentRevision: previousRevision + 2,
    messageId: request.messageId,
    outcome: 'completed',
    previousRevision,
  }
}

function canonicalFixtureReceipt(revision, messageId, operation) {
  return {
    actorKey: 'system:differential-runner',
    events: [],
    idempotentReplay: false,
    requestId: `${messageId}:${operation}`,
    revision,
    scopeKey: 'repository:differential-fixture',
    streamId: 'delivery:differential-fixture',
  }
}

function canonicalFixtureCommandResponse(target, sourceCommand) {
  const input = sourceCommand.input ?? sourceCommand.request.payload
  switch (target) {
    case 'fixture.runtime-source.replace':
      return {
        outcome: 'completed',
        result: {
          candidatePresent: input.candidate !== null,
          runtimeEventCount: input.runtimeEvents.length,
        },
      }
    case 'fixture.service.restart':
      return { outcome: 'completed', result: { durableStoreReopened: true } }
    case 'fixture.store.seed-snapshot':
      return {
        outcome: 'completed',
        result: {
          currentRevision: input.snapshot.revision,
          deliveryId: input.snapshot.id,
        },
      }
    case 'fixture.store.corrupt-record':
    case 'fixture.store.restore-record':
      return { outcome: 'completed', result: { sequence: input.sequence } }
    default:
      throw new Error(`unknown fixture response target: ${target}`)
  }
}

function canonicalFixtureRequestId(sourceCommandIndex, targetIndex) {
  const suffix = `${sourceCommandIndex.toString().padStart(13, '0')}${targetIndex.toString().padStart(13, '0')}`
  return `req_${suffix}`
}
