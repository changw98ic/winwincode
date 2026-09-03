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
  migrateLegacyTaskGraph,
  migrateLegacyTaskId,
  normalizeDifferentialResult,
  runDifferentialGate,
  validateCanonicalExpected,
  validateDifferentialContract,
  validateDifferentialExecutionPlan,
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
  assert.equal(
    plan.schemaVersion,
    'winwincode.delivery-strongflow-differential-plan.v2',
  )
  assert.deepEqual(rules.executionPlan.exactScenarioKeys, [
    'commands',
    'id',
    'terminalOutcomeStatusBySourceCommandIndex',
  ])
  assert.deepEqual(Object.keys(plan.scenarios[0]).sort(), [
    'commands',
    'id',
    'terminalOutcomeStatusBySourceCommandIndex',
  ])
  assert.deepEqual(
    Object.fromEntries(plan.scenarios.map(scenario => [
      scenario.id,
      scenario.terminalOutcomeStatusBySourceCommandIndex,
    ])),
    {
      'attention': {},
      'candidate-invalidation': { 17: 'succeeded', 25: 'succeeded' },
      'corruption-recovery': {},
      'inconclusive': { 17: 'succeeded' },
      'infra-error': { 17: 'infrastructure_error' },
      'request-id-replay': {},
      'revision-conflict': {},
      'rework': { 17: 'succeeded', 25: 'succeeded' },
      'success-closed-loop': { 17: 'succeeded' },
      'task-dag': {},
    },
  )
  const infraPlanScenario = plan.scenarios.find(scenario => scenario.id === 'infra-error')
  for (const [name, mutate] of [
    ['missing', statuses => { delete statuses[17] }],
    ['extra', statuses => { statuses[18] = 'infrastructure_error' }],
    ['wrong-index', statuses => {
      delete statuses[17]
      statuses[18] = 'infrastructure_error'
    }],
  ]) {
    const malformed = structuredClone(plan)
    const statuses = malformed.scenarios.find(scenario => scenario.id === 'infra-error')
      .terminalOutcomeStatusBySourceCommandIndex
    mutate(statuses)
    assert.throws(
      () => validateDifferentialExecutionPlan(malformed, oracle, rules),
      /infra-error terminal outcome plan source indexes/u,
      name,
    )
  }
  for (const [name, status, pattern] of [
    ['wrong-value', 'succeeded', /infra-error terminal outcome plan status/u],
    ['non-schema-alias', 'infrastructure-error', /status is not allowed/u],
  ]) {
    const malformed = structuredClone(plan)
    malformed.scenarios.find(scenario => scenario.id === 'infra-error')
      .terminalOutcomeStatusBySourceCommandIndex[17] = status
    assert.throws(
      () => validateDifferentialExecutionPlan(malformed, oracle, rules),
      pattern,
      name,
    )
  }
  assert.deepEqual(infraPlanScenario.terminalOutcomeStatusBySourceCommandIndex, {
    17: 'infrastructure_error',
  })
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
  assert.deepEqual(
    rules.canonicalMigration.commandMappings
      .find(mapping => mapping.legacy === 'createDelivery').canonicalTargets,
    ['delivery.create', 'fixture.solution-review.validate'],
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
  const terminalOutcomeSources = {
    'attention': [],
    'candidate-invalidation': [17, 25],
    'corruption-recovery': [],
    'inconclusive': [17],
    'infra-error': [17],
    'request-id-replay': [],
    'revision-conflict': [],
    'rework': [17, 25],
    'success-closed-loop': [17],
    'task-dag': [],
  }
  assert.deepEqual(
    rules.canonicalMigration.terminalOutcomeMessages.sourceCommandIndexesByScenario,
    terminalOutcomeSources,
  )
  for (const [id, sourceIndexes] of Object.entries(terminalOutcomeSources)) {
    const groups = rules.scenarios.find(scenario => scenario.id === id).canonicalGroups
    assert.deepEqual(
      groups.filter(group => group.target === 'job.outcome')
        .map(group => group.sourceCommandIndexes[0]),
      sourceIndexes,
      id,
    )
    for (const sourceIndex of sourceIndexes) {
      const outcomeIndex = groups.findIndex(group => (
        group.target === 'job.outcome'
          && group.sourceCommandIndexes.includes(sourceIndex)
      ))
      assert.equal(groups[outcomeIndex].kind, 'execution-port.message', id)
      assert.deepEqual(groups[outcomeIndex].revisionEffect, { delta: 1 }, id)
      assert.equal(groups[outcomeIndex + 1].target, 'delivery.submit_verdict', id)
      assert.deepEqual(groups[outcomeIndex + 1].sourceCommandIndexes, [sourceIndex], id)
    }
  }
  assert.equal(
    rules.scenarios.find(scenario => scenario.id === 'task-dag')
      .canonicalGroups.find(group => group.sourceCommandIndexes.includes(2)).target,
    'fixture.solution-review.validate',
  )
  assert.equal(
    rules.scenarios.find(scenario => scenario.id === 'task-dag')
      .canonicalGroups.find(group => group.sourceCommandIndexes.includes(2)).kind,
    'fixture.command',
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
      'candidate-invalidation': [31, 8, 7, 1],
      'inconclusive': [19, 5, 4, 1],
      'infra-error': [19, 5, 4, 1],
      'rework': [31, 8, 7, 1],
      'success-closed-loop': [21, 6, 4, 1],
    },
  )
  assert.deepEqual(
    rules.scenarios.find(scenario => scenario.id === 'task-dag').canonicalAssertions,
    {
      advancedTaskId: 'dtk_59X1F156B8YGG0P7G1K9KR5KB1',
      cycleError: 'INVALID_REQUEST',
      cycleRejectedWithoutWrite: true,
      durableTaskOrder: [
        'dtk_59X1F156B8YGG0P7G1K9KR5KB1',
        'dtk_7HT0EYAWGG4MD098E2F2Z5XNTW',
      ],
      finalRevision: 2,
      sessionBindingCount: 1,
      stageRunCount: 1,
      taskCount: 2,
    },
  )
})

test('task-dag migration remaps legacy task identities and rejects the cycle before sealing', async () => {
  const [rules, oracle] = await Promise.all([json(rulesPath), json(oraclePath)])
  const taskDag = oracle.scenarios.find(scenario => scenario.id === 'task-dag')
  const seeded = taskDag.commands[0].input.snapshot
  const expectedPrerequisite = 'dtk_59X1F156B8YGG0P7G1K9KR5KB1'
  const expectedDependent = 'dtk_7HT0EYAWGG4MD098E2F2Z5XNTW'

  assert.equal(
    migrateLegacyTaskId(seeded.id, 'oracle-task-prerequisite'),
    expectedPrerequisite,
  )
  assert.equal(
    migrateLegacyTaskId(seeded.id, 'oracle-task-dependent'),
    expectedDependent,
  )
  assert.equal(
    migrateLegacyTaskId(seeded.id, expectedPrerequisite),
    expectedPrerequisite,
  )

  const migrated = migrateLegacyTaskGraph(seeded.id, seeded.tasks)
  assert.deepEqual(migrated.map(task => task.id), [expectedPrerequisite, expectedDependent])
  assert.deepEqual(migrated[1].blockedByTaskIds, [expectedPrerequisite])
  assert.deepEqual(migrated.map(task => task.owner), [null, null])
  assert.deepEqual(
    migrated.map(task => [task.title, task.goal, task.acceptanceCriterionIds]),
    seeded.tasks.map(task => [task.title, task.goal, task.acceptanceCriterionIds]),
  )

  const policy = rules.canonicalMigration.legacyTaskIdMigration
  assert.equal(policy.namespace, 'winwincode.oracle-task-id-migration.v1\0')
  assert.equal(policy.preserveCanonicalTaskIds, true)
  assert.deepEqual(policy.passOrder, ['map-task-ids', 'map-dependencies'])
  assert.deepEqual(policy.knownVectors, [
    {
      canonicalTaskId: expectedPrerequisite,
      deliveryId: seeded.id,
      legacyTaskId: 'oracle-task-prerequisite',
    },
    {
      canonicalTaskId: expectedDependent,
      deliveryId: seeded.id,
      legacyTaskId: 'oracle-task-dependent',
    },
  ])
  assert.deepEqual(policy.invalidCycleValidation, {
    errorCode: 'INVALID_REQUEST',
    invalidProposalKind: 'dependency-cycle',
    mainScenarioStoreWrites: 0,
    proposalBuilder: 'invalid_task_proposals_fixture(DependencyCycle)',
    resolver: 'prepare_solution_review_fixture',
    setup: 'isolated canonical planning handoff built from the source spec',
    sourceCommandIndex: 2,
    specSource: 'legacy source command 2 payload.spec',
    target: 'fixture.solution-review.validate',
  })

  const expected = canonicalExpectedFixture(oracle, rules)
  validateCanonicalExpected(rules, oracle, expected)
  const migratedTaskDag = expected.result.scenarios.find(scenario => scenario.id === 'task-dag')
  const seedCommand = migratedTaskDag.commands.find(command => (
    command.sourceCommandIndexes.includes(0)
  ))
  assert.deepEqual(
    seedCommand.request.input.snapshot.tasks.map(task => task.id),
    [expectedPrerequisite, expectedDependent],
  )
  assert.deepEqual(seedCommand.request.input.snapshot.tasks[1].blockedByTaskIds, [
    expectedPrerequisite,
  ])
  const cycle = migratedTaskDag.commands.find(command => (
    command.sourceCommandIndexes.includes(2)
  ))
  assert.deepEqual(Object.keys(cycle.request.input).sort(), [
    'invalidProposalKind',
    'spec',
  ])
  assert.equal(cycle.request.input.invalidProposalKind, 'dependency-cycle')
  assert.deepEqual(cycle.request.input.spec, taskDag.commands[2].request.payload.spec)
  assert.equal(cycle.response.outcome, 'rejected')
  assert.equal(cycle.response.error.code, 'INVALID_REQUEST')

  const wrongCycleSpec = structuredClone(expected)
  const wrongCycle = wrongCycleSpec.result.scenarios
    .find(scenario => scenario.id === 'task-dag').commands
    .find(command => command.request.kind === 'fixture.solution-review.validate')
  wrongCycle.request.input.spec.title = 'A different DeliverySpec'
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, wrongCycleSpec),
    /task-dag cycle spec differs/u,
  )
})

test('terminal outcome messages bind the exact final verifier fact before verdict', async () => {
  const [rules, oracle] = await Promise.all([json(rulesPath), json(oraclePath)])
  const expected = canonicalExpectedFixture(oracle, rules)
  const outcomeStatusByScenario = {
    'attention': null,
    'candidate-invalidation': 'succeeded',
    'corruption-recovery': null,
    'inconclusive': 'succeeded',
    'infra-error': 'infrastructure_error',
    'request-id-replay': null,
    'revision-conflict': null,
    'rework': 'succeeded',
    'success-closed-loop': 'succeeded',
    'task-dag': null,
  }
  assert.deepEqual(
    rules.canonicalMigration.terminalOutcomeMessages.outcomeStatusByScenario,
    outcomeStatusByScenario,
  )
  assert.deepEqual(
    rules.canonicalExpected.requestContracts['execution-port.message']
      .outcomeStatusesByTarget,
    { 'job.outcome': ['infrastructure_error', 'succeeded'] },
  )
  for (const scenario of expected.result.scenarios) {
    const statuses = scenario.commands
      .filter(command => command.request.kind === 'job.outcome')
      .map(command => command.request.outcome.status)
    const expectedStatus = outcomeStatusByScenario[scenario.id]
    assert.deepEqual(
      statuses,
      expectedStatus === null
        ? []
        : Array.from({ length: statuses.length }, () => expectedStatus),
      scenario.id,
    )
  }
  const source = oracle.scenarios.find(scenario => scenario.id === 'success-closed-loop')
  const scenario = expected.result.scenarios.find(entry => entry.id === source.id)
  const sourceVerdict = source.commands[17]
  const terminal = sourceVerdict.request.payload.runtimeEvents.find(event => (
    event.kind === 'turn.completed'
      && event.source.roleId === 'verifier'
  ))
  const outcomeIndex = scenario.commands.findIndex(command => (
    command.request.kind === 'job.outcome'
  ))
  const outcome = scenario.commands[outcomeIndex]
  const binding = scenario.commands.slice(0, outcomeIndex).findLast(command => (
    command.request.kind === 'session.binding'
      && command.response.outcome === 'completed'
  ))

  assert.deepEqual(outcome.sourceCommandIndexes, [17])
  assert.deepEqual(outcome.request.lease, binding.request.lease)
  assert.equal(outcome.request.workerSessionId, binding.request.workerSessionId)
  assert.equal(outcome.request.outcome.codexThreadId, binding.request.codexThreadId)
  assert.equal(
    outcome.request.outcome.finishedAt,
    new Date(terminal.occurredAtMillis).toISOString(),
  )
  assert.equal(outcome.request.sentAt, outcome.request.outcome.finishedAt)
  assert.equal(outcome.request.outcome.lastEventSequence, Number(terminal.cursor.sequence))
  assert.equal(outcome.request.outcome.summary, terminal.data.last_agent_message)
  assert.deepEqual(outcome.request.outcome.artifacts, [])
  assert.equal(scenario.commands[outcomeIndex + 1].request.command, 'delivery.submit_verdict')
  validateCanonicalExpected(rules, oracle, expected)

  const wrongTerminalFact = structuredClone(expected)
  wrongTerminalFact.result.scenarios.find(entry => entry.id === source.id).commands
    .find(command => command.request.kind === 'job.outcome')
    .request.outcome.finishedAt = '2026-01-01T00:00:00Z'
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, wrongTerminalFact),
    /terminal outcome finishedAt/u,
  )

  const infraSource = oracle.scenarios.find(entry => entry.id === 'infra-error')
  const infraSourceVerdict = infraSource.commands[17]
  assert.equal(infraSourceVerdict.response.result.delivery.stageRuns.at(-1).status, 'failed')
  assert.equal(infraSourceVerdict.response.result.delivery.verdict.status, 'infra_error')
  const infraScenario = expected.result.scenarios.find(entry => entry.id === 'infra-error')
  const infraOutcome = infraScenario.commands.find(command => (
    command.request.kind === 'job.outcome'
  ))
  assert.equal(infraOutcome.request.outcome.status, 'infrastructure_error')

  const wrongInfraStatus = structuredClone(expected)
  wrongInfraStatus.result.scenarios.find(entry => entry.id === 'infra-error').commands
    .find(command => command.request.kind === 'job.outcome')
    .request.outcome.status = 'succeeded'
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, wrongInfraStatus),
    /infra-error terminal outcome status/u,
  )
})

test('execution-port identity joins reject missing and foreign nested facts', async () => {
  const [rules, oracle] = await Promise.all([json(rulesPath), json(oraclePath)])
  const expected = canonicalExpectedFixture(oracle, rules)
  validateCanonicalExpected(rules, oracle, expected)

  for (const [name, mutate, pattern] of [
    ['missing sessionIdentity', (binding, _outcome) => {
      delete binding.request.sessionIdentity
    }, /message request keys differ/u],
    ['foreign binding sessionIdentity', (binding, _outcome) => {
      binding.request.sessionIdentity.stageRunId = 'str_00000000000000000000000001'
    }, /binding sessionIdentity stageRunId/u],
    ['attempt does not join lease', (binding, _outcome) => {
      binding.request.attempt = 2
    }, /binding attempt/u],
    ['fencing token does not join lease', (binding, _outcome) => {
      binding.request.fencingToken = '2'
    }, /binding fencingToken/u],
    ['source identity does not join payload', (binding, _outcome) => {
      binding.request.sourceIdentity.workerId = 'wrk_00000000000000000000000001'
    }, /binding sourceIdentity workerId/u],
    ['job outcome uses a foreign accepted identity', (_binding, outcome) => {
      outcome.request.sessionIdentity.stageRunId = 'str_00000000000000000000000001'
    }, /terminal outcome sessionIdentity/u],
  ]) {
    const malformed = structuredClone(expected)
    const scenario = malformed.result.scenarios.find(entry => (
      entry.id === 'success-closed-loop'
    ))
    const outcome = scenario.commands.find(command => (
      command.request.kind === 'job.outcome'
    ))
    const binding = scenario.commands.slice(0, scenario.commands.indexOf(outcome)).findLast(
      command => command.request.kind === 'session.binding'
        && command.response.outcome === 'completed',
    )
    mutate(binding, outcome)
    assert.throws(
      () => validateCanonicalExpected(rules, oracle, malformed),
      pattern,
      name,
    )
  }
})

test('runtime projection follows only the last complete Codex binding', async () => {
  const [rules, oracle] = await Promise.all([json(rulesPath), json(oraclePath)])
  const followup = rules.canonicalMigration.runtimeProjectionFollowup
  assert.equal(
    followup.selectionRule,
    'last-complete-codex-stage-in-delivery-projection-order',
  )
  assert.deepEqual(followup.requestValueSources, {
    atCursor: 'delivery.get.response.result.readCursor',
    deliveryId: 'delivery.get.response.result.deliveryId',
    productSessionId: 'selectedStage.sessionBinding.productSessionId',
    stageRunId: 'selectedStage.id',
  })
  assert.equal(followup.runtimeResponseSessionCount, 1)
  assert.equal(followup.observationAbsentValue, null)
  assert.equal(followup.noFallbackIdentities, true)

  const queryScenarioIds = [
    'success-closed-loop',
    'candidate-invalidation',
    'attention',
    'inconclusive',
    'infra-error',
    'rework',
  ]
  const noQueryScenarioIds = [
    'request-id-replay',
    'revision-conflict',
    'corruption-recovery',
    'task-dag',
  ]
  assert.deepEqual(followup.queryScenarioIds, queryScenarioIds)
  assert.deepEqual(followup.noQueryScenarioIds, noQueryScenarioIds)

  for (const mapping of rules.scenarios) {
    const runtimeGroups = mapping.canonicalGroups.filter(group => (
      group.target === 'runtime.projection.get'
    ))
    assert.equal(
      runtimeGroups.length,
      queryScenarioIds.includes(mapping.id) ? 1 : 0,
      mapping.id,
    )
    for (const [sourceIndex, signature] of mapping.sourceCommandSignatures.entries()) {
      if (signature !== 'strongflow.request:getDeliveryProjection') continue
      const targets = mapping.canonicalGroups
        .filter(group => group.sourceCommandIndexes.includes(sourceIndex))
        .map(group => group.target)
      assert.equal(targets[0], 'delivery.get', `${mapping.id}/${sourceIndex}`)
      assert.deepEqual(
        targets,
        runtimeGroups.some(group => group.sourceCommandIndexes.includes(sourceIndex))
          ? ['delivery.get', 'runtime.projection.get']
          : ['delivery.get'],
        `${mapping.id}/${sourceIndex}`,
      )
    }
  }

  const expected = canonicalExpectedFixture(oracle, rules)
  validateCanonicalExpected(rules, oracle, expected)
  for (const scenario of expected.result.scenarios) {
    if (queryScenarioIds.includes(scenario.id)) {
      const deliveryQuery = scenario.commands.find(command => (
        command.request.query === 'delivery.get'
      ))
      const runtimeQuery = scenario.commands.find(command => (
        command.request.query === 'runtime.projection.get'
      ))
      const selected = deliveryQuery.response.result.stages
        .filter(stage => stage.actorType === 'codex'
          && stage.sessionBinding?.workerSessionId != null
          && stage.sessionBinding?.codexThreadId != null)
        .at(-1)
      assert.equal(runtimeQuery.request.parameters.stageRunId, selected.id)
      assert.equal(
        runtimeQuery.request.parameters.productSessionId,
        selected.sessionBinding.productSessionId,
      )
      assert.deepEqual(
        scenario.observation.projection.runtime,
        runtimeQuery.response.result,
      )
    } else {
      assert.equal(
        scenario.commands.some(command => (
          command.request.query === 'runtime.projection.get'
        )),
        false,
      )
      assert.equal(scenario.observation.projection.runtime, null)
      if (scenario.id === 'task-dag') {
        const pending = scenario.commands
          .find(command => command.request.query === 'delivery.get')
          .response.result.stages.at(-1)
        assert.equal(pending.actorType, 'codex')
        assert.equal(pending.sessionBinding.workerSessionId, null)
        assert.equal(pending.sessionBinding.codexThreadId, null)
      }
    }
  }

  const wrongSelectedStage = structuredClone(expected)
  const success = wrongSelectedStage.result.scenarios.find(scenario => (
    scenario.id === 'success-closed-loop'
  ))
  success.commands.find(command => command.request.query === 'runtime.projection.get')
    .request.parameters.stageRunId = 'str_99999999999999999999999999'
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, wrongSelectedStage),
    /runtime stageRunId/u,
  )

  const multipleRuntimeSessions = structuredClone(expected)
  const runtimeResult = multipleRuntimeSessions.result.scenarios
    .find(scenario => scenario.id === 'success-closed-loop')
    .commands.find(command => command.request.query === 'runtime.projection.get')
    .response.result
  runtimeResult.sessions.push(structuredClone(runtimeResult.sessions[0]))
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, multipleRuntimeSessions),
    /runtime result session count/u,
  )

  const inventedRuntimeObservation = structuredClone(expected)
  inventedRuntimeObservation.result.scenarios
    .find(scenario => scenario.id === 'task-dag')
    .observation.projection.runtime = { inventedProductSessionId: true }
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, inventedRuntimeObservation),
    /runtime observation differs/u,
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

  const openJobOutcome = structuredClone(expected)
  const jobOutcome = openJobOutcome.result.scenarios[0].commands.find(command => (
    command.request.kind === 'job.outcome'
  ))
  jobOutcome.request.candidateDigest = `sha256:${'0'.repeat(64)}`
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, openJobOutcome),
    /message request keys differ/u,
  )

  const foldedJobOutcome = structuredClone(expected)
  const folded = foldedJobOutcome.result.scenarios[0].commands.find(command => (
    command.request.kind === 'job.outcome'
  ))
  folded.response.commits[0].operation = 'delivery.submit_verdict'
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, foldedJobOutcome),
    /operation must be "apply_terminal_outcome"/u,
  )

  const openCycleFixture = structuredClone(expected)
  const cycleFixture = openCycleFixture.result.scenarios
    .find(scenario => scenario.id === 'task-dag').commands
    .find(command => command.request.kind === 'fixture.solution-review.validate')
  cycleFixture.request.input.legacyTasks = []
  assert.throws(
    () => validateCanonicalExpected(rules, oracle, openCycleFixture),
    /fixture input keys differ/u,
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

test('Rust runner consumes only the Node-authored execution plan', async () => {
  const sources = await Promise.all([
    'crates/winwincode-control-plane/tests/delivery_strongflow_differential_runner.rs',
    'crates/winwincode-control-plane/tests/support/differential_runner.rs',
  ].map(path => readFile(join(root, path), 'utf8')))

  assert.match(
    sources[0],
    /fn machine_entry_executes_node_authored_plan_when_supplied\(\)/u,
  )

  for (const source of sources) {
    assert.equal(source.includes('delivery-strongflow-typescript.v1.json'), false)
    assert.equal(source.includes('local_plan_paths'), false)
    assert.equal(source.includes('local_fixture_terminal_outcome_statuses'), false)
    assert.equal(source.includes('fixture_terminal_outcome_status'), false)
  }
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
        const terminalOutcomeStatus = rules.canonicalMigration.terminalOutcomeMessages
          .outcomeStatusByScenario[scenario.id]
        const commands = mapping.canonicalGroups.map((group, targetIndex) => {
          const sourceCommand = scenario.commands[group.sourceCommandIndexes.at(-1)]
          const legacy = sourceCommand.kind === 'strongflow.request'
            ? sourceCommand.request.operation
            : sourceCommand.kind
          return {
            sourceCommandIndexes: group.sourceCommandIndexes,
            kind: group.kind,
            request: canonicalFixtureRequest(
              group,
              sourceCommand,
              targetIndex,
              mapping,
              terminalOutcomeStatus,
            ),
            response: canonicalFixtureResponse(
              group,
              sourceCommand,
              migrateFixtureResponse(sourceCommand.response, legacy, rules),
              targetIndex,
              scenario,
              mapping,
              terminalOutcomeStatus,
            ),
          }
        })
        const deliveryProjection = commands
          .filter(command => command.request.query === 'delivery.get'
            && command.response.error === undefined)
          .at(-1)?.response.result ?? null
        const runtimeProjection = commands
          .findLast(command => command.request.query === 'runtime.projection.get')
          ?.response.result ?? null
        const migrated = {
          id: scenario.id,
          commands,
          observation: {
            events: structuredClone(scenario.observation.events),
            projection: {
              delivery: structuredClone(deliveryProjection),
              runtime: structuredClone(runtimeProjection),
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
    snapshot.tasks = migrateLegacyTaskGraph(snapshot.id, snapshot.tasks)
    snapshot.tasks[0].status = 'active'
    snapshot.tasks[1].status = 'blocked'
  }

  scenario.observation.store.state.revision = snapshot.revision
  scenario.observation.store.state.snapshot = structuredClone(snapshot)
  scenario.observation.store.journal.snapshot = structuredClone(snapshot)
  const lastRecord = scenario.observation.store.journal.records.at(-1)
  if (lastRecord !== undefined) lastRecord.snapshot = structuredClone(snapshot)
  if (scenario.id === 'task-dag') {
    for (const record of scenario.observation.store.journal.records) {
      record.snapshot.tasks = migrateLegacyTaskGraph(record.snapshot.id, record.snapshot.tasks)
    }
  }
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

function canonicalFixtureRequest(
  group,
  sourceCommand,
  targetIndex,
  mapping,
  terminalOutcomeStatus,
) {
  if (group.kind === 'fixture.command') {
    if (group.target === 'fixture.store.seed-snapshot') {
      const input = structuredClone(sourceCommand.input)
      input.snapshot.tasks = migrateLegacyTaskGraph(
        input.snapshot.id,
        input.snapshot.tasks,
      )
      return { input, kind: group.target }
    }
    if (group.target === 'fixture.solution-review.validate') {
      return {
        input: {
          invalidProposalKind: 'dependency-cycle',
          spec: structuredClone(sourceCommand.request.payload.spec),
        },
        kind: group.target,
      }
    }
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
    const messageId = `xmsg_${canonicalFixtureRequestId(
      group.sourceCommandIndexes[0],
      targetIndex,
    ).slice(4)}`
    const lease = {
      attempt: 1,
      expiresAt: '2026-01-01T00:01:00Z',
      fencingToken: '1',
      issuedAt: instant,
      jobId: 'job_00000000000000000000000000',
      leaseId: 'lea_00000000000000000000000000',
      workerId: 'wrk_00000000000000000000000000',
      workerInstanceId: 'wri_00000000000000000000000000',
    }
    if (group.target === 'job.outcome') {
      const terminal = source.payload.runtimeEvents.findLast(event => (
        event.kind === 'turn.completed'
          && event.source?.roleId === 'verifier'
      ))
      const terminalInstant = new Date(terminal.occurredAtMillis).toISOString()
      return {
        kind: 'job.outcome',
        lease,
        messageId,
        outcome: {
          artifacts: [],
          codexThreadId: 'cdx_00000000000000000000000000',
          finishedAt: terminalInstant,
          lastEventSequence: Number(terminal.cursor.sequence),
          status: terminalOutcomeStatus,
          summary: terminal.data.last_agent_message,
          usage: { costMicrounits: 0, runtimeMillis: 0, tokens: 0 },
        },
        schemaVersion: 'winwincode/v1',
        sentAt: terminalInstant,
        sessionIdentity: {
          codexThreadId: 'cdx_00000000000000000000000000',
          productSessionId: 'psn_00000000000000000000000000',
          stageRunId: 'str_00000000000000000000000000',
          workerSessionId: 'wsn_00000000000000000000000000',
        },
        workerSessionId: 'wsn_00000000000000000000000000',
      }
    }
    return {
      attempt: lease.attempt,
      boundAt: instant,
      codexThreadId: 'cdx_00000000000000000000000000',
      fencingToken: lease.fencingToken,
      kind: 'session.binding',
      lease,
      leaseId: lease.leaseId,
      messageId,
      productSessionId: 'psn_00000000000000000000000000',
      schemaVersion: 'winwincode/v1',
      sentAt: instant,
      sessionIdentity: {
        codexThreadId: 'cdx_00000000000000000000000000',
        productSessionId: 'psn_00000000000000000000000000',
        stageRunId: 'str_00000000000000000000000000',
        workerSessionId: 'wsn_00000000000000000000000000',
      },
      sourceIdentity: {
        kind: 'execution-worker',
        leaseId: lease.leaseId,
        workerId: lease.workerId,
        workerInstanceId: lease.workerInstanceId,
        workerSessionId: 'wsn_00000000000000000000000000',
      },
      stageRunId: 'str_00000000000000000000000000',
      workerId: lease.workerId,
      workerSessionId: 'wsn_00000000000000000000000000',
    }
  }
  if (group.kind === 'control-plane.query') {
    const selected = canonicalFixtureSelectedBinding(mapping)
    const parameters = group.target === 'runtime.projection.get'
      ? {
          atCursor: canonicalFixtureReadCursor(source.payload.deliveryId, mapping),
          deliveryId: source.payload.deliveryId,
          kind: 'delivery-stage',
          productSessionId: selected.productSessionId,
          stageRunId: selected.stageRunId,
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

function canonicalFixtureStageId(index) {
  return `str_${index.toString().padStart(26, '0')}`
}

function canonicalFixtureProductSessionId(index) {
  return `psn_${index.toString().padStart(26, '0')}`
}

function canonicalFixtureSelectedBinding(mapping) {
  const index = mapping.canonicalAssertions.sessionBindingCount - 1
  return {
    codexThreadId: `cdx_${index.toString().padStart(26, '0')}`,
    executionJobId: `job_${index.toString().padStart(26, '0')}`,
    productSessionId: canonicalFixtureProductSessionId(index),
    sessionBindingId: `sbd_${index.toString().padStart(26, '0')}`,
    stageRunId: canonicalFixtureStageId(index),
    workerSessionId: `wsn_${index.toString().padStart(26, '0')}`,
  }
}

function canonicalFixtureReadCursor(deliveryId, mapping) {
  return {
    deliveryId,
    deliveryRevision: mapping.canonicalFinalRevision,
    eventCursor: {
      eventId: 'evt_00000000000000000000000000',
      scope: {
        kind: 'repository',
        organizationId: 'org_00000000000000000000000000',
        projectId: 'prj_00000000000000000000000000',
        repositoryId: 'rep_00000000000000000000000000',
        workspaceId: 'wsp_00000000000000000000000000',
      },
      sequence: 1,
      stream: { deliveryId, kind: 'delivery' },
    },
    publicationRevision: 0,
    runtimeAcceptedSequence: 1,
    runtimeLedgerRevision: 1,
    scope: {
      kind: 'repository',
      organizationId: 'org_00000000000000000000000000',
      projectId: 'prj_00000000000000000000000000',
      repositoryId: 'rep_00000000000000000000000000',
      workspaceId: 'wsp_00000000000000000000000000',
    },
    token: 'fixture_read_cursor_token_0000000000000000',
  }
}

function canonicalFixtureDeliveryProjection(delivery, mapping) {
  const stageCount = mapping.canonicalAssertions.stageRunCount ?? 0
  const bindingCount = mapping.canonicalAssertions.sessionBindingCount ?? 0
  const hasRuntimeQuery = mapping.canonicalGroups.some(group => (
    group.target === 'runtime.projection.get'
  ))
  const stages = Array.from({ length: stageCount }, (_unused, index) => {
    const hasBinding = index < bindingCount
    const complete = hasBinding && hasRuntimeQuery
    return {
      actorType: hasBinding ? 'codex' : 'human',
      id: canonicalFixtureStageId(index),
      sessionBinding: hasBinding
        ? {
            bindingId: `sbd_${index.toString().padStart(26, '0')}`,
            boundAt: '2026-01-01T00:00:00Z',
            codexThreadId: complete
              ? `cdx_${index.toString().padStart(26, '0')}`
              : null,
            executionJobId: `job_${index.toString().padStart(26, '0')}`,
            productSessionId: canonicalFixtureProductSessionId(index),
            workerSessionId: complete
              ? `wsn_${index.toString().padStart(26, '0')}`
              : null,
          }
        : null,
    }
  })
  return {
    deliveryId: delivery.id,
    readCursor: canonicalFixtureReadCursor(delivery.id, mapping),
    stages,
  }
}

function canonicalFixtureRuntimeProjection(sourceCommand, mapping) {
  const deliveryId = sourceCommand.request.payload.deliveryId
  const selected = canonicalFixtureSelectedBinding(mapping)
  return {
    deliveryId,
    eventCursor: canonicalFixtureReadCursor(deliveryId, mapping).eventCursor,
    kind: 'runtime_projection',
    lastProjectionSequence: 1,
    productSessionId: selected.productSessionId,
    readCursor: canonicalFixtureReadCursor(deliveryId, mapping),
    rebuiltAt: '2026-01-01T00:00:00Z',
    revision: mapping.canonicalFinalRevision,
    sessions: [{
      activities: [],
      agentEdges: [],
      agents: [],
      asOfSequence: 1,
      attempt: 1,
      codexThreadId: selected.codexThreadId,
      deliveryTaskId: null,
      diffSummary: null,
      executionJobId: selected.executionJobId,
      fencingToken: '1',
      leaseId: 'lea_00000000000000000000000000',
      plan: null,
      productSessionId: selected.productSessionId,
      recovery: {
        failureCount: 0,
        lastFailureSourceRef: null,
        latestRecoverySourceRef: null,
        recoveryCount: 0,
        state: 'none',
      },
      sessionBindingId: selected.sessionBindingId,
      stageRunId: selected.stageRunId,
      usage: null,
      workerSessionId: selected.workerSessionId,
    }],
    stageRunId: selected.stageRunId,
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
  terminalOutcomeStatus,
) {
  if (group.kind === 'fixture.command') {
    return canonicalFixtureCommandResponse(group.target, sourceCommand)
  }
  if (group.kind === 'execution-port.message') {
    return canonicalFixtureMessageResponse(
      group,
      sourceCommand,
      targetIndex,
      mapping,
      terminalOutcomeStatus,
    )
  }
  const requestId = canonicalFixtureRequestId(group.sourceCommandIndexes[0], targetIndex)
  if (scenario.id === 'task-dag'
    && group.target === 'delivery.advance'
    && group.sourceCommandIndexes.includes(1)) {
    const result = structuredClone(scenario.observation.snapshot)
    result.tasks = migrateLegacyTaskGraph(result.id, result.tasks)
    return {
      command: group.target,
      currentRevision: 2,
      outcome: 'completed',
      previousRevision: 1,
      requestId,
      result,
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
        ? canonicalFixtureDeliveryProjection(response.result.delivery, mapping)
        : canonicalFixtureRuntimeProjection(sourceCommand, mapping),
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

function canonicalFixtureMessageResponse(
  group,
  sourceCommand,
  targetIndex,
  mapping,
  terminalOutcomeStatus,
) {
  const request = canonicalFixtureRequest(
    group,
    sourceCommand,
    targetIndex,
    mapping,
    terminalOutcomeStatus,
  )
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
  const operations = group.target === 'session.binding'
    ? ['accept_worker_session', 'report_codex_thread']
    : ['apply_terminal_outcome']
  const commits = operations.map((operation, index) => ({
    currentRevision: previousRevision + index + 1,
    operation,
    previousRevision: previousRevision + index,
    receipt: canonicalFixtureReceipt(previousRevision + index + 1, request.messageId, operation),
  }))
  return {
    commits,
    currentRevision: previousRevision + group.revisionEffect.delta,
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
    case 'fixture.solution-review.validate':
      return {
        error: {
          code: 'INVALID_REQUEST',
          details: {},
          message: 'solution-review task proposals contain a dependency cycle',
          retryable: false,
        },
        outcome: 'rejected',
      }
    default:
      throw new Error(`unknown fixture response target: ${target}`)
  }
}

function canonicalFixtureRequestId(sourceCommandIndex, targetIndex) {
  const suffix = `${sourceCommandIndex.toString().padStart(13, '0')}${targetIndex.toString().padStart(13, '0')}`
  return `req_${suffix}`
}
