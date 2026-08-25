import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { mkdir, mkdtemp, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { isAbsolute, join } from 'node:path'

const RULES_PATH = 'docs/contracts/delivery-strongflow-rust-differential.rules.json'
const REQUIRED_BINDINGS = Object.freeze([
  'AUTH_PROOF',
  'NODE_EXECUTABLE',
  'ORACLE_ROOT',
  'fixtureRandomIdentities',
])

function fail(message) {
  throw new Error(message)
}

function sortedKeys(value) {
  return Object.keys(value).toSorted()
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

function requireExactKeys(value, expected, label) {
  requireObject(value, label)
  const actual = sortedKeys(value)
  const wanted = [...expected].toSorted()
  if (JSON.stringify(actual) !== JSON.stringify(wanted)) {
    fail(`${label} keys differ: expected ${wanted.join(', ')}, actual ${actual.join(', ')}`)
  }
}

function requireEqual(actual, expected, label) {
  if (actual !== expected) fail(`${label} must be ${JSON.stringify(expected)}`)
}

function requireDeepEqual(actual, expected, label) {
  const difference = findFirstJsonDifference(expected, actual)
  if (difference !== null) fail(`${label} differs at ${difference.path || '<root>'}`)
}

const CROCKFORD_BASE32_ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'
const CANONICAL_DELIVERY_TASK_ID = /^dtk_[0-9A-HJKMNP-TV-Z]{26}$/u

export function migrateLegacyTaskId(deliveryId, legacyTaskId) {
  if (typeof deliveryId !== 'string' || deliveryId.length === 0) {
    fail('legacy task deliveryId must be a non-empty string')
  }
  if (typeof legacyTaskId !== 'string' || legacyTaskId.length === 0) {
    fail('legacy task id must be a non-empty string')
  }
  if (CANONICAL_DELIVERY_TASK_ID.test(legacyTaskId)) return legacyTaskId
  const digest = createHash('sha256')
    .update(`winwincode.oracle-task-id-migration.v1\0${deliveryId}\0${legacyTaskId}`, 'utf8')
    .digest()
  let value = BigInt(`0x${digest.subarray(0, 16).toString('hex')}`)
  let suffix = ''
  for (let index = 0; index < 26; index += 1) {
    suffix = CROCKFORD_BASE32_ALPHABET[Number(value & 31n)] + suffix
    value >>= 5n
  }
  return `dtk_${suffix}`
}

export function migrateLegacyTaskGraph(deliveryId, tasks) {
  requireArray(tasks, 'legacy task graph')
  const idMap = new Map()
  const canonicalIds = new Set()
  for (let index = 0; index < tasks.length; index += 1) {
    const task = requireObject(tasks[index], `legacy task graph[${index}]`)
    if (typeof task.id !== 'string' || task.id.length === 0) {
      fail(`legacy task graph[${index}].id must be a non-empty string`)
    }
    if (idMap.has(task.id)) fail(`legacy task graph has duplicate task id ${task.id}`)
    const canonicalId = migrateLegacyTaskId(deliveryId, task.id)
    if (canonicalIds.has(canonicalId)) {
      fail(`legacy task graph maps more than one task to ${canonicalId}`)
    }
    idMap.set(task.id, canonicalId)
    canonicalIds.add(canonicalId)
  }

  return tasks.map((task, taskIndex) => {
    const dependencies = requireArray(
      task.blockedByTaskIds,
      `legacy task graph[${taskIndex}].blockedByTaskIds`,
    ).map(legacyDependencyId => {
      const canonicalDependencyId = idMap.get(legacyDependencyId)
      if (canonicalDependencyId === undefined) {
        fail(
          `legacy task graph[${taskIndex}] has unknown dependency ${legacyDependencyId}`,
        )
      }
      return canonicalDependencyId
    })
    return {
      ...structuredClone(task),
      blockedByTaskIds: dependencies,
      id: idMap.get(task.id),
      owner: null,
    }
  })
}

function commandSignature(command, label) {
  requireObject(command, label)
  if (command.kind === 'strongflow.request') {
    requireExactKeys(command, ['kind', 'request', 'response'], label)
    requireObject(command.request, `${label}.request`)
    if (typeof command.request.operation !== 'string') {
      fail(`${label}.request.operation must be a string`)
    }
    return `${command.kind}:${command.request.operation}`
  }
  requireExactKeys(command, ['input', 'kind', 'response'], label)
  requireObject(command.input, `${label}.input`)
  return command.kind
}

function knownLegacyBranches(oracle) {
  const commands = new Set()
  const errors = new Set()
  for (const scenario of oracle.scenarios) {
    for (const command of scenario.commands) {
      const legacy = command.kind === 'strongflow.request'
        ? command.request.operation
        : command.kind
      commands.add(legacy)
      if (command.response?.error?.code !== undefined) {
        errors.add(`${legacy}\0${command.response.error.code}`)
      }
    }
  }
  return { commands, errors }
}

function validateMigrationRules(rules, oracle) {
  const known = knownLegacyBranches(oracle)
  const mappedCommands = rules.canonicalMigration.commandMappings.map(mapping => mapping.legacy)
  requireDeepEqual([...mappedCommands].toSorted(), [...known.commands].toSorted(), 'command mappings')

  const mappedErrors = new Set(rules.canonicalMigration.errorMappings.map(mapping => (
    `${mapping.legacyOperation}\0${mapping.legacyCode}`
  )))
  const canonicalErrorCodes = new Set(
    rules.canonicalExpected.responseContracts.errorCodes,
  )
  for (const mapping of rules.canonicalMigration.errorMappings) {
    if (!canonicalErrorCodes.has(mapping.canonicalCode)) {
      fail(
        `legacy error mapping ${mapping.legacyOperation}/${mapping.legacyCode}`
          + ` has unknown canonical code ${mapping.canonicalCode}`,
      )
    }
  }
  for (const branch of known.errors) {
    if (!mappedErrors.has(branch)) {
      const [operation, code] = branch.split('\0')
      fail(`unmapped legacy error branch: ${operation}/${code}`)
    }
  }
  for (const branch of mappedErrors) {
    if (!known.errors.has(branch)) {
      const [operation, code] = branch.split('\0')
      fail(`machine rules map unused legacy error branch: ${operation}/${code}`)
    }
  }
}

function validateNormalizationRules(rules) {
  const allowed = rules.normalization.allowedBindings
  requireArray(allowed, 'normalization.allowedBindings')
  requireDeepEqual(
    allowed.map(binding => binding.name).toSorted(),
    ['AUTH_PROOF', 'NODE_EXECUTABLE', 'ORACLE_ROOT'],
    'normalization binding names',
  )
  requireDeepEqual(
    allowed.map(binding => binding.placeholder).toSorted(),
    ['<AUTH_PROOF>', '<NODE_EXECUTABLE>', '<ORACLE_ROOT>'],
    'normalization placeholders',
  )
  requireDeepEqual(
    rules.normalization.fixtureRandomIdentities,
    [],
    'normalization fixture random identities',
  )
  requireEqual(rules.normalization.arrayOrder, 'exact', 'normalization.arrayOrder')
  requireEqual(rules.comparison.noSummaryComparison, true, 'noSummaryComparison')
  requireEqual(rules.comparison.noTestNameEvidence, true, 'noTestNameEvidence')
}

function validateExecutionPlanRules(rules) {
  requireEqual(
    rules.runner.inputSchemaVersion,
    'winwincode.delivery-strongflow-differential-plan.v2',
    'runner input schemaVersion',
  )
  const contract = rules.executionPlan
  requireExactKeys(contract, [
    'exactScenarioKeys',
    'exactTopLevelKeys',
    'expectedStateInputPolicy',
    'fixtureCommandKeys',
    'forbiddenExpectedKeys',
    'strongFlowCommandKeys',
  ], 'execution plan contract')
  requireDeepEqual(contract.exactTopLevelKeys, [
    'bindings',
    'oracleSchemaVersion',
    'scenarios',
    'schemaVersion',
  ], 'execution plan top-level keys')
  requireDeepEqual(contract.exactScenarioKeys, [
    'commands',
    'id',
    'terminalOutcomeStatusBySourceCommandIndex',
  ], 'execution plan scenario keys')
  requireDeepEqual(
    contract.strongFlowCommandKeys,
    ['kind', 'request'],
    'execution plan StrongFlow command keys',
  )
  requireDeepEqual(
    contract.fixtureCommandKeys,
    ['input', 'kind'],
    'execution plan fixture command keys',
  )
  requireDeepEqual(
    contract.forbiddenExpectedKeys,
    ['assertions', 'observation', 'response'],
    'execution plan forbidden expected keys',
  )
  requireEqual(
    contract.expectedStateInputPolicy,
    'Only fixture.store.seed-snapshot.input.snapshot may carry seeded Delivery state. '
      + 'terminalOutcomeStatusBySourceCommandIndex is the sole additional closed migration '
      + 'provenance derived from a legacy completed response; it is not expected product state. '
      + 'No raw command response, assertion, final observation, journal record, manifest, '
      + 'receipt, outbox, or final snapshot enters the runner plan.',
    'execution plan expected-state input policy',
  )
}

function validateRuntimeProjectionRules(rules) {
  const policy = rules.canonicalMigration.runtimeProjectionFollowup
  requireExactKeys(policy, [
    'completeBindingRequirements',
    'deliveryQueryTarget',
    'failedDeliveryGetPolicy',
    'noFallbackIdentities',
    'noQueryScenarioIds',
    'observationAbsentValue',
    'observationPresentSource',
    'pendingBindingPolicy',
    'queryScenarioIds',
    'requestValueSources',
    'runtimeQueryTarget',
    'runtimeResponseSessionCount',
    'selectionRule',
    'selectionSource',
    'sourceOperation',
  ], 'runtime projection follow-up policy')
  requireEqual(policy.sourceOperation, 'getDeliveryProjection', 'runtime source operation')
  requireEqual(policy.deliveryQueryTarget, 'delivery.get', 'runtime delivery query')
  requireEqual(policy.runtimeQueryTarget, 'runtime.projection.get', 'runtime query')
  requireEqual(
    policy.selectionRule,
    'last-complete-codex-stage-in-delivery-projection-order',
    'runtime selection rule',
  )
  requireDeepEqual(policy.completeBindingRequirements, {
    actorType: 'codex',
    codexThreadId: 'non-null',
    sessionBinding: 'non-null',
    workerSessionId: 'non-null',
  }, 'runtime complete binding requirements')
  requireDeepEqual(policy.requestValueSources, {
    atCursor: 'delivery.get.response.result.readCursor',
    deliveryId: 'delivery.get.response.result.deliveryId',
    productSessionId: 'selectedStage.sessionBinding.productSessionId',
    stageRunId: 'selectedStage.id',
  }, 'runtime request value sources')
  requireEqual(policy.runtimeResponseSessionCount, 1, 'runtime response session count')
  requireEqual(policy.observationAbsentValue, null, 'runtime absent observation')
  requireEqual(policy.noFallbackIdentities, true, 'runtime fallback identity policy')
  const observation = rules.canonicalExpected.runtimeProjectionObservation
  requireExactKeys(
    observation,
    ['exactKeys', 'runtimeAbsentValue', 'runtimePresentSource'],
    'runtime projection observation contract',
  )
  requireDeepEqual(observation.exactKeys, ['delivery', 'runtime'], 'runtime observation keys')
  requireEqual(observation.runtimeAbsentValue, null, 'runtime observation absent value')
  const responseContracts = rules.canonicalExpected.responseContracts
  requireEqual(
    responseContracts.runtimeProjectionGetSessionsExactLength,
    policy.runtimeResponseSessionCount,
    'runtime response contract session count',
  )
  requireDeepEqual(responseContracts.runtimeProjectionGetResultExactKeys, [
    'deliveryId',
    'eventCursor',
    'kind',
    'lastProjectionSequence',
    'productSessionId',
    'readCursor',
    'rebuiltAt',
    'revision',
    'sessions',
    'stageRunId',
  ], 'runtime result keys')
  requireDeepEqual(responseContracts.runtimeProjectionSessionExactKeys, [
    'activities',
    'agentEdges',
    'agents',
    'asOfSequence',
    'attempt',
    'codexThreadId',
    'deliveryTaskId',
    'diffSummary',
    'executionJobId',
    'fencingToken',
    'leaseId',
    'plan',
    'productSessionId',
    'recovery',
    'sessionBindingId',
    'stageRunId',
    'usage',
    'workerSessionId',
  ], 'runtime session keys')

  const scenarioIds = rules.scenarios.map(scenario => scenario.id)
  const declaredIds = [...policy.queryScenarioIds, ...policy.noQueryScenarioIds]
  requireDeepEqual(declaredIds.toSorted(), scenarioIds.toSorted(), 'runtime scenario partition')
  requireEqual(new Set(declaredIds).size, scenarioIds.length, 'runtime scenario uniqueness')

  const queryScenarios = new Set(policy.queryScenarioIds)
  for (const scenario of rules.scenarios) {
    const runtimeGroups = scenario.canonicalGroups.filter(group => (
      group.target === policy.runtimeQueryTarget
    ))
    requireEqual(
      runtimeGroups.length,
      queryScenarios.has(scenario.id) ? 1 : 0,
      `${scenario.id} runtime query count`,
    )
    for (const [sourceIndex, signature] of scenario.sourceCommandSignatures.entries()) {
      if (signature !== `strongflow.request:${policy.sourceOperation}`) continue
      const groups = scenario.canonicalGroups.filter(group => (
        group.sourceCommandIndexes.includes(sourceIndex)
      ))
      const targets = groups.map(group => group.target)
      const runtimeForSource = runtimeGroups.some(group => (
        group.sourceCommandIndexes.includes(sourceIndex)
      ))
      requireDeepEqual(
        targets,
        runtimeForSource
          ? [policy.deliveryQueryTarget, policy.runtimeQueryTarget]
          : [policy.deliveryQueryTarget],
        `${scenario.id} source ${sourceIndex} projection query mapping`,
      )
    }
  }
}

function terminalVerifierFact(command, label) {
  requireEqual(command.kind, 'strongflow.request', `${label} kind`)
  requireEqual(command.request.operation, 'submitVerdict', `${label} operation`)
  const runtimeEvents = requireArray(
    command.request.payload.runtimeEvents,
    `${label}.request.payload.runtimeEvents`,
  )
  const terminalEvents = runtimeEvents.filter(event => (
    event.kind === 'turn.completed'
      && event.source?.roleId === 'verifier'
      && event.terminalReason === 'completed'
      && event.data?.error === null
  ))
  requireEqual(terminalEvents.length, 1, `${label} terminal verifier fact count`)
  const event = terminalEvents[0]
  for (const [field, value] of [
    ['cursor.sequence', event.cursor?.sequence],
    ['occurredAtMillis', event.occurredAtMillis],
    ['source.sessionId', event.source?.sessionId],
  ]) {
    if ((typeof value !== 'string' && typeof value !== 'number') || value === '') {
      fail(`${label} terminal verifier fact ${field} is missing`)
    }
  }
  return event
}

function terminalVerifierFactKey(command, label) {
  const event = terminalVerifierFact(command, label)
  return JSON.stringify([
    event.source.sessionId,
    event.cursor.sequence,
    event.occurredAtMillis,
  ])
}

function terminalOutcomeStatusForFact(sourceScenario, sourceCommandIndex, label) {
  const sourceCommand = sourceScenario.commands[sourceCommandIndex]
  const factKey = terminalVerifierFactKey(sourceCommand, `${label} source`)
  const completedSubmissions = []
  for (let commandIndex = 0; commandIndex < sourceScenario.commands.length; commandIndex += 1) {
    const command = sourceScenario.commands[commandIndex]
    if (command.request?.operation !== 'submitVerdict' || command.response?.ok !== true) {
      continue
    }
    const candidateKey = terminalVerifierFactKey(
      command,
      `${label} completed source command ${commandIndex}`,
    )
    if (candidateKey === factKey) completedSubmissions.push({ command, commandIndex })
  }
  requireEqual(
    completedSubmissions.length,
    1,
    `${label} completed submit response count`,
  )

  const [{ command, commandIndex }] = completedSubmissions
  const delivery = requireObject(
    command.response.result?.delivery,
    `${label} completed source command ${commandIndex} delivery`,
  )
  const stageRuns = requireArray(
    delivery.stageRuns,
    `${label} completed source command ${commandIndex} stageRuns`,
  )
  const verifier = requireObject(
    stageRuns.at(-1),
    `${label} completed source command ${commandIndex} final stageRun`,
  )
  requireEqual(
    verifier.role,
    'verifier',
    `${label} completed source command ${commandIndex} final stageRun role`,
  )
  const verdict = requireObject(
    delivery.verdict,
    `${label} completed source command ${commandIndex} verdict`,
  )
  if (verifier.status === 'succeeded'
    && ['fail', 'inconclusive', 'pass'].includes(verdict.status)) {
    return 'succeeded'
  }
  if (verifier.status === 'failed' && verdict.status === 'infra_error') {
    return 'infrastructure_error'
  }
  fail(
    `${label} has unmigratable verifier/verdict status pair `
      + `${JSON.stringify(verifier.status)}/${JSON.stringify(verdict.status)}`,
  )
}

function validateTerminalOutcomeRules(rules, oracle) {
  const policy = rules.canonicalMigration.terminalOutcomeMessages
  requireExactKeys(policy, [
    'deduplicationPolicy',
    'kind',
    'outcomeStatusByScenario',
    'requestAuthority',
    'requestType',
    'requestValueSources',
    'revisionIncrementPerSuccessfulMessage',
    'revisionMapPolicy',
    'sourceCommandIndexesByScenario',
    'sourceFactSelection',
    'sourceOperation',
    'successfulMessageCommitSequence',
  ], 'terminal outcome message policy')
  requireEqual(policy.kind, 'execution-port.message', 'terminal outcome kind')
  requireEqual(policy.requestType, 'JobOutcomeMessage', 'terminal outcome request type')
  requireEqual(policy.sourceOperation, 'submitVerdict', 'terminal outcome source operation')
  requireEqual(
    policy.revisionIncrementPerSuccessfulMessage,
    1,
    'terminal outcome revision increment',
  )
  requireDeepEqual(
    policy.successfulMessageCommitSequence,
    ['apply_terminal_outcome'],
    'terminal outcome commit sequence',
  )
  requireDeepEqual(
    rules.canonicalMigration.commandMappings
      .find(mapping => mapping.legacy === policy.sourceOperation)?.canonicalTargets,
    ['execution-port.message:job.outcome', 'delivery.submit_verdict'],
    'terminal outcome command mapping',
  )
  requireDeepEqual(policy.requestValueSources, {
    artifacts: 'empty array because the legacy terminal fact has no ExecutionPort ArtifactReference',
    codexThreadId: 'exact current verifier SessionBinding.codexThreadId',
    finishedAt: 'selected terminal turn.completed occurredAtMillis encoded as an exact UTC Instant',
    lastEventSequence: 'selected terminal turn.completed cursor.sequence',
    lease: 'exact current verifier ExecutionJob active lease',
    messageId: 'deterministic terminal-outcome identity mapping v1',
    sentAt: 'same exact Instant as outcome.finishedAt',
    status: 'exact status derived from the completed legacy submit response current verifier StageRun and Verdict',
    summary: 'selected terminal turn.completed data.last_agent_message',
    workerSessionId: 'exact current verifier SessionBinding.workerSessionId',
  }, 'terminal outcome request value sources')

  const outcomeStatusByScenario = requireObject(
    policy.outcomeStatusByScenario,
    'terminal outcome status by scenario',
  )
  requireDeepEqual(
    Object.keys(outcomeStatusByScenario).toSorted(),
    rules.scenarios.map(scenario => scenario.id).toSorted(),
    'terminal outcome status scenario partition',
  )
  requireDeepEqual(
    rules.canonicalExpected.requestContracts['execution-port.message']
      .outcomeStatusesByTarget,
    { 'job.outcome': ['infrastructure_error', 'succeeded'] },
    'terminal outcome request status contract',
  )

  const sourceIndexesByScenario = requireObject(
    policy.sourceCommandIndexesByScenario,
    'terminal outcome source indexes',
  )
  requireDeepEqual(
    Object.keys(sourceIndexesByScenario).toSorted(),
    rules.scenarios.map(scenario => scenario.id).toSorted(),
    'terminal outcome scenario partition',
  )
  for (let scenarioIndex = 0; scenarioIndex < oracle.scenarios.length; scenarioIndex += 1) {
    const source = oracle.scenarios[scenarioIndex]
    const mapping = rules.scenarios[scenarioIndex]
    const seenFacts = new Set()
    const derivedSourceIndexes = []
    for (let commandIndex = 0; commandIndex < source.commands.length; commandIndex += 1) {
      const command = source.commands[commandIndex]
      if (command.request?.operation !== policy.sourceOperation) continue
      const factKey = terminalVerifierFactKey(
        command,
        `${mapping.id} source command ${commandIndex}`,
      )
      if (!seenFacts.has(factKey)) {
        seenFacts.add(factKey)
        derivedSourceIndexes.push(commandIndex)
      }
    }
    requireDeepEqual(
      sourceIndexesByScenario[mapping.id],
      derivedSourceIndexes,
      `${mapping.id} terminal outcome source indexes`,
    )
    const derivedStatuses = derivedSourceIndexes.map(sourceCommandIndex => (
      terminalOutcomeStatusForFact(
        source,
        sourceCommandIndex,
        `${mapping.id} terminal outcome`,
      )
    ))
    const uniqueStatuses = [...new Set(derivedStatuses)]
    if (uniqueStatuses.length > 1) {
      fail(`${mapping.id} terminal outcome statuses must have one scenario value`)
    }
    requireEqual(
      outcomeStatusByScenario[mapping.id],
      uniqueStatuses[0] ?? null,
      `${mapping.id} terminal outcome status policy`,
    )
    const outcomeGroups = mapping.canonicalGroups.filter(group => group.target === 'job.outcome')
    requireDeepEqual(
      outcomeGroups.map(group => group.sourceCommandIndexes[0]),
      derivedSourceIndexes,
      `${mapping.id} terminal outcome groups`,
    )
    for (const group of outcomeGroups) {
      requireEqual(group.kind, policy.kind, `${mapping.id} terminal outcome group kind`)
      requireDeepEqual(
        group.revisionEffect,
        { delta: policy.revisionIncrementPerSuccessfulMessage },
        `${mapping.id} terminal outcome revision effect`,
      )
      const groupIndex = mapping.canonicalGroups.indexOf(group)
      const verdict = mapping.canonicalGroups[groupIndex + 1]
      requireEqual(verdict?.target, 'delivery.submit_verdict', `${mapping.id} outcome successor`)
      requireDeepEqual(
        verdict.sourceCommandIndexes,
        group.sourceCommandIndexes,
        `${mapping.id} outcome provenance`,
      )
    }
  }
}

function validateLegacyTaskIdMigrationRules(rules, oracle) {
  const policy = rules.canonicalMigration.legacyTaskIdMigration
  requireExactKeys(policy, [
    'alphabet',
    'canonicalPrefix',
    'digest',
    'digestBytes',
    'encoding',
    'invalidCycleValidation',
    'knownVectors',
    'namespace',
    'owner',
    'passOrder',
    'preserveCanonicalTaskIds',
    'preservedFields',
  ], 'legacy task id migration policy')
  requireEqual(
    policy.namespace,
    'winwincode.oracle-task-id-migration.v1\0',
    'legacy task id namespace',
  )
  requireEqual(policy.digest, 'SHA-256', 'legacy task id digest')
  requireEqual(policy.digestBytes, 16, 'legacy task id digest bytes')
  requireEqual(policy.encoding, 'big-endian-u128-crockford-base32-26', 'legacy task id encoding')
  requireEqual(policy.alphabet, CROCKFORD_BASE32_ALPHABET, 'legacy task id alphabet')
  requireEqual(policy.canonicalPrefix, 'dtk_', 'legacy task id prefix')
  requireEqual(policy.preserveCanonicalTaskIds, true, 'canonical task id preservation')
  requireDeepEqual(policy.passOrder, ['map-task-ids', 'map-dependencies'], 'task migration passes')
  requireEqual(policy.owner, null, 'migrated task owner')
  requireDeepEqual(
    policy.preservedFields,
    ['order', 'title', 'goal', 'acceptanceCriterionIds'],
    'migrated task preserved fields',
  )
  requireDeepEqual(
    rules.canonicalMigration.commandMappings
      .find(mapping => mapping.legacy === 'createDelivery')?.canonicalTargets,
    ['delivery.create', 'fixture.solution-review.validate'],
    'createDelivery command mappings',
  )

  const sourceScenario = oracle.scenarios.find(scenario => scenario.id === 'task-dag')
  if (sourceScenario === undefined) fail('task-dag source scenario is missing')
  const seed = sourceScenario.commands[0]
  requireEqual(seed.kind, 'fixture.store.seed-snapshot', 'task-dag seed command')
  const sourceSnapshot = requireObject(seed.input.snapshot, 'task-dag seed snapshot')
  const migrated = migrateLegacyTaskGraph(sourceSnapshot.id, sourceSnapshot.tasks)
  const knownVectors = sourceSnapshot.tasks.map((task, index) => ({
    canonicalTaskId: migrated[index].id,
    deliveryId: sourceSnapshot.id,
    legacyTaskId: task.id,
  }))
  requireDeepEqual(policy.knownVectors, knownVectors, 'legacy task id known vectors')

  const cycle = policy.invalidCycleValidation
  requireExactKeys(cycle, [
    'errorCode',
    'invalidProposalKind',
    'mainScenarioStoreWrites',
    'proposalBuilder',
    'resolver',
    'setup',
    'sourceCommandIndex',
    'specSource',
    'target',
  ], 'task-dag invalid cycle policy')
  requireDeepEqual(cycle, {
    errorCode: 'INVALID_REQUEST',
    invalidProposalKind: 'dependency-cycle',
    mainScenarioStoreWrites: 0,
    proposalBuilder: 'invalid_task_proposals_fixture(DependencyCycle)',
    resolver: 'prepare_solution_review_fixture',
    setup: 'isolated canonical planning handoff built from the source spec',
    sourceCommandIndex: 2,
    specSource: 'legacy source command 2 payload.spec',
    target: 'fixture.solution-review.validate',
  }, 'task-dag invalid cycle policy')
  requireEqual(
    sourceScenario.commands[cycle.sourceCommandIndex].request?.operation,
    'createDelivery',
    'task-dag invalid cycle source operation',
  )
  const taskDagMapping = rules.scenarios.find(scenario => scenario.id === 'task-dag')
  const cycleGroups = taskDagMapping.canonicalGroups.filter(group => (
    group.sourceCommandIndexes.includes(cycle.sourceCommandIndex)
  ))
  requireEqual(cycleGroups.length, 1, 'task-dag invalid cycle group count')
  requireDeepEqual(cycleGroups[0], {
    kind: 'fixture.command',
    revisionEffect: { delta: 0 },
    sourceCommandIndexes: [cycle.sourceCommandIndex],
    target: cycle.target,
  }, 'task-dag invalid cycle group')
  requireDeepEqual(
    taskDagMapping.canonicalAssertions.durableTaskOrder,
    migrated.map(task => task.id),
    'task-dag durable task order',
  )
  requireEqual(
    taskDagMapping.canonicalAssertions.advancedTaskId,
    migrated[0].id,
    'task-dag advanced task id',
  )
}

export function validateDifferentialContract(rules, oracle, options = {}) {
  requireEqual(
    rules.schemaVersion,
    'winwincode.delivery-strongflow-rust-differential-rules.v1',
    'rules.schemaVersion',
  )
  requireEqual(rules.issueId, 'winwincode-9c4.16.2.6.5', 'rules.issueId')
  requireExactKeys(oracle, rules.oracle.exactTopLevelKeys, 'oracle')
  requireEqual(oracle.schemaVersion, rules.oracle.schemaVersion, 'oracle.schemaVersion')
  requireEqual(oracle.source, rules.oracle.source, 'oracle.source')
  if (options.sourceBytes !== undefined) {
    const digest = createHash('sha256').update(options.sourceBytes).digest('hex')
    requireEqual(digest, rules.oracle.sha256, 'oracle sha256')
  }

  const oracleScenarios = requireArray(oracle.scenarios, 'oracle.scenarios')
  const scenarioRules = requireArray(rules.scenarios, 'rules.scenarios')
  requireEqual(oracleScenarios.length, 10, 'oracle scenario count')
  requireEqual(scenarioRules.length, oracleScenarios.length, 'scenario rule count')

  for (let index = 0; index < oracleScenarios.length; index += 1) {
    const scenario = oracleScenarios[index]
    const mapping = scenarioRules[index]
    const label = `oracle.scenarios[${index}]`
    requireExactKeys(scenario, rules.oracle.exactScenarioKeys, label)
    requireExactKeys(scenario.observation, rules.oracle.exactObservationKeys, `${label}.observation`)
    requireEqual(mapping.order, index, `rules.scenarios[${index}].order`)
    requireEqual(mapping.id, scenario.id, `rules.scenarios[${index}].id`)
    requireDeepEqual(mapping.legacyAssertions, scenario.assertions, `${mapping.id} legacy assertions`)
    requireDeepEqual(mapping.observationKeys, rules.oracle.exactObservationKeys, `${mapping.id} observation keys`)

    const signatures = scenario.commands.map((command, commandIndex) => (
      commandSignature(command, `${label}.commands[${commandIndex}]`)
    ))
    requireDeepEqual(mapping.sourceCommandSignatures, signatures, `${mapping.id} command mapping`)
    const covered = mapping.canonicalGroups.flatMap(group => group.sourceCommandIndexes)
    for (let commandIndex = 0; commandIndex < scenario.commands.length; commandIndex += 1) {
      if (!covered.includes(commandIndex)) {
        fail(`${mapping.id} source command ${commandIndex} has no canonical disposition`)
      }
    }
    let calculatedRevision = 0
    for (const group of mapping.canonicalGroups) {
      requireArray(group.sourceCommandIndexes, `${mapping.id} canonical source indexes`)
      if (group.sourceCommandIndexes.length === 0) {
        fail(`${mapping.id} canonical group ${group.target} has no source command`)
      }
      for (const commandIndex of group.sourceCommandIndexes) {
        if (!Number.isInteger(commandIndex)
          || commandIndex < 0
          || commandIndex >= scenario.commands.length) {
          fail(`${mapping.id} canonical group ${group.target} has invalid source index`)
        }
      }
      if (!rules.canonicalExpected.commandKinds.includes(group.kind)) {
        fail(`${mapping.id} canonical group ${group.target} has unknown kind ${group.kind}`)
      }
      const effect = requireObject(
        group.revisionEffect,
        `${mapping.id} canonical group ${group.target} revisionEffect`,
      )
      if (Object.hasOwn(effect, 'seed')) {
        requireExactKeys(effect, ['seed'], `${mapping.id} revision seed`)
        if (group.target !== 'fixture.store.seed-snapshot') {
          fail(`${mapping.id} only fixture.store.seed-snapshot may seed a revision`)
        }
        if (!Number.isSafeInteger(effect.seed) || effect.seed < 0) {
          fail(`${mapping.id} revision seed must be a safe non-negative integer`)
        }
        calculatedRevision = effect.seed
      } else {
        requireExactKeys(effect, ['delta'], `${mapping.id} revision delta`)
        if (!Number.isSafeInteger(effect.delta) || effect.delta < 0) {
          fail(`${mapping.id} revision delta must be a safe non-negative integer`)
        }
        if (group.kind === 'execution-port.message') {
          const allowed = group.target === 'session.binding'
            ? [0, 2]
            : group.target === 'job.outcome'
              ? [1]
              : []
          if (!allowed.includes(effect.delta)) {
            fail(
              `${mapping.id} ${group.target} revision delta must be ${allowed.join(' or ')}`,
            )
          }
        }
        calculatedRevision += effect.delta
      }
    }
    requireEqual(
      calculatedRevision,
      mapping.canonicalFinalRevision,
      `${mapping.id} calculated final revision`,
    )
    requireEqual(
      mapping.canonicalAssertions.finalRevision,
      calculatedRevision,
      `${mapping.id} asserted final revision`,
    )
  }

  const successfulMessages = Object.fromEntries(scenarioRules.map(mapping => [
    mapping.id,
    mapping.canonicalGroups.filter(group => (
      group.target === 'session.binding' && group.revisionEffect.delta === 2
    )).length,
  ]))
  requireDeepEqual(
    successfulMessages,
    rules.canonicalMigration.sessionBindingMessages.successfulLegacyBindCountByScenario,
    'successful session binding message counts',
  )

  validateMigrationRules(rules, oracle)
  validateNormalizationRules(rules)
  validateExecutionPlanRules(rules)
  validateRuntimeProjectionRules(rules)
  validateTerminalOutcomeRules(rules, oracle)
  validateLegacyTaskIdMigrationRules(rules, oracle)
  return {
    scenarioCount: oracleScenarios.length,
    scenarioIds: oracleScenarios.map(scenario => scenario.id),
  }
}

function validateBindings(bindings, rules) {
  requireExactKeys(bindings, REQUIRED_BINDINGS, 'normalization bindings')
  for (const name of ['AUTH_PROOF', 'NODE_EXECUTABLE', 'ORACLE_ROOT']) {
    if (typeof bindings[name] !== 'string' || bindings[name].length === 0) {
      fail(`${name} binding must be a non-empty string`)
    }
  }
  if (!isAbsolute(bindings.ORACLE_ROOT)) fail('ORACLE_ROOT binding must be absolute')
  if (!isAbsolute(bindings.NODE_EXECUTABLE)) fail('NODE_EXECUTABLE binding must be absolute')
  requireObject(bindings.fixtureRandomIdentities, 'fixtureRandomIdentities binding')

  const declared = new Set(
    rules.normalization.fixtureRandomIdentities.map(identity => identity.name),
  )
  for (const name of Object.keys(bindings.fixtureRandomIdentities)) {
    if (!declared.has(name)) fail(`fixture random identity ${name} is not declared`)
  }
  for (const name of declared) {
    if (!Object.hasOwn(bindings.fixtureRandomIdentities, name)) {
      fail(`fixture random identity ${name} has no runtime binding`)
    }
  }
}

function replaceEvery(value, from, to) {
  return value.split(from).join(to)
}

function hydrateString(value, bindings, rules) {
  let hydrated = replaceEvery(value, '<ORACLE_ROOT>', bindings.ORACLE_ROOT)
  hydrated = replaceEvery(hydrated, '<NODE_EXECUTABLE>', bindings.NODE_EXECUTABLE)
  hydrated = replaceEvery(hydrated, '<AUTH_PROOF>', bindings.AUTH_PROOF)
  for (const identity of rules.normalization.fixtureRandomIdentities) {
    hydrated = replaceEvery(
      hydrated,
      identity.placeholder,
      bindings.fixtureRandomIdentities[identity.name],
    )
  }
  return hydrated
}

function mapJson(value, mapString) {
  if (typeof value === 'string') return mapString(value)
  if (Array.isArray(value)) return value.map(entry => mapJson(entry, mapString))
  if (value === null || typeof value !== 'object') return value
  return Object.fromEntries(
    Object.entries(value).map(([key, entry]) => [key, mapJson(entry, mapString)]),
  )
}

function differentialPlanCommands(scenario, bindings, rules) {
  return scenario.commands.map(command => {
    if (command.kind === 'strongflow.request') {
      return {
        kind: command.kind,
        request: mapJson(command.request, value => hydrateString(value, bindings, rules)),
      }
    }
    return {
      input: mapJson(command.input, value => hydrateString(value, bindings, rules)),
      kind: command.kind,
    }
  })
}

function terminalOutcomePlanStatusMap(sourceScenario, rules) {
  const policy = rules.canonicalMigration.terminalOutcomeMessages
  const sourceIndexes = policy.sourceCommandIndexesByScenario[sourceScenario.id]
  const declaredStatus = policy.outcomeStatusByScenario[sourceScenario.id]
  return Object.fromEntries(sourceIndexes.map(sourceCommandIndex => {
    const derivedStatus = terminalOutcomeStatusForFact(
      sourceScenario,
      sourceCommandIndex,
      `${sourceScenario.id} terminal outcome plan`,
    )
    requireEqual(
      declaredStatus,
      derivedStatus,
      `${sourceScenario.id} terminal outcome plan status policy`,
    )
    return [String(sourceCommandIndex), declaredStatus]
  }))
}

export function validateDifferentialExecutionPlan(plan, oracle, rules) {
  validateExecutionPlanRules(rules)
  requireExactKeys(
    plan,
    rules.executionPlan.exactTopLevelKeys,
    'differential execution plan',
  )
  requireEqual(plan.schemaVersion, rules.runner.inputSchemaVersion, 'plan schemaVersion')
  requireEqual(plan.oracleSchemaVersion, oracle.schemaVersion, 'plan oracle schemaVersion')
  validateBindings(plan.bindings, rules)
  const scenarios = requireArray(plan.scenarios, 'plan scenarios')
  requireEqual(scenarios.length, oracle.scenarios.length, 'plan scenario count')
  const allowedStatuses = rules.canonicalExpected
    .requestContracts['execution-port.message'].outcomeStatusesByTarget['job.outcome']
  for (let scenarioIndex = 0; scenarioIndex < scenarios.length; scenarioIndex += 1) {
    const scenario = scenarios[scenarioIndex]
    const sourceScenario = oracle.scenarios[scenarioIndex]
    const label = `plan scenarios[${scenarioIndex}]`
    requireExactKeys(
      scenario,
      rules.executionPlan.exactScenarioKeys,
      label,
    )
    requireEqual(scenario.id, sourceScenario.id, `${label}.id`)
    const commands = requireArray(scenario.commands, `${label}.commands`)
    requireEqual(commands.length, sourceScenario.commands.length, `${label} command count`)
    for (let commandIndex = 0; commandIndex < commands.length; commandIndex += 1) {
      const sourceCommand = sourceScenario.commands[commandIndex]
      requireExactKeys(
        commands[commandIndex],
        sourceCommand.kind === 'strongflow.request'
          ? rules.executionPlan.strongFlowCommandKeys
          : rules.executionPlan.fixtureCommandKeys,
        `${label}.commands[${commandIndex}]`,
      )
    }
    requireDeepEqual(
      commands,
      differentialPlanCommands(sourceScenario, plan.bindings, rules),
      `${scenario.id} plan commands`,
    )
    const statuses = requireObject(
      scenario.terminalOutcomeStatusBySourceCommandIndex,
      `${scenario.id} terminal outcome plan statuses`,
    )
    const expectedStatuses = terminalOutcomePlanStatusMap(sourceScenario, rules)
    requireDeepEqual(
      Object.keys(statuses).toSorted(),
      Object.keys(expectedStatuses).toSorted(),
      `${scenario.id} terminal outcome plan source indexes`,
    )
    for (const [sourceCommandIndex, status] of Object.entries(statuses)) {
      if (!allowedStatuses.includes(status)) {
        fail(`${scenario.id} terminal outcome plan status is not allowed`)
      }
      requireEqual(
        status,
        expectedStatuses[sourceCommandIndex],
        `${scenario.id} terminal outcome plan status`,
      )
    }
  }
  return { scenarioCount: scenarios.length }
}

export function buildDifferentialExecutionPlan(oracle, bindings, rules) {
  validateBindings(bindings, rules)
  const plan = {
    bindings: structuredClone(bindings),
    oracleSchemaVersion: oracle.schemaVersion,
    scenarios: oracle.scenarios.map(scenario => ({
      commands: differentialPlanCommands(scenario, bindings, rules),
      id: scenario.id,
      terminalOutcomeStatusBySourceCommandIndex: terminalOutcomePlanStatusMap(
        scenario,
        rules,
      ),
    })),
    schemaVersion: rules.runner.inputSchemaVersion,
  }
  validateDifferentialExecutionPlan(plan, oracle, rules)
  return plan
}

export function buildCanonicalMigrationPlan(oracle, rules) {
  return {
    mappingVersion: rules.canonicalMigration.mappingVersion,
    scenarios: rules.scenarios.map(scenario => ({
      commands: scenario.canonicalGroups.map(group => ({
        canonicalTargets: [group.target],
        kind: group.kind,
        revisionEffect: structuredClone(group.revisionEffect),
        sourceCommandIndexes: [...group.sourceCommandIndexes],
      })),
      id: scenario.id,
    })),
    schemaVersion: 'winwincode.delivery-strongflow-canonical-migration-plan.v1',
    sourceOracleSha256: rules.oracle.sha256,
  }
}

function commandTarget(command) {
  if (command.kind === 'control-plane.command') return command.request?.command
  if (command.kind === 'control-plane.query') return command.request?.query
  if (command.kind === 'execution-port.message') return command.request?.kind
  if (command.kind === 'fixture.command') return command.request?.kind
  return undefined
}

function requireRequiredOptionalKeys(value, required, optional, label) {
  requireObject(value, label)
  const actual = sortedKeys(value)
  const requiredSet = new Set(required)
  const allowed = new Set([...required, ...optional])
  for (const key of requiredSet) {
    if (!Object.hasOwn(value, key)) fail(`${label} is missing required key ${key}`)
  }
  for (const key of actual) {
    if (!allowed.has(key)) fail(`${label} has unknown key ${key}`)
  }
}

function validateErrorEnvelope(response, contracts, label) {
  requireExactKeys(response, contracts.errorEnvelopeExactKeys, `${label} error envelope`)
  validateCanonicalError(response.error, contracts, label)
}

function validateCanonicalError(error, contracts, label) {
  requireExactKeys(error, contracts.errorExactKeys, `${label}.error`)
  if (!contracts.errorCodes.includes(error.code)) {
    fail(`${label} has unknown canonical error code ${error.code}`)
  }
  if (typeof error.message !== 'string' || error.message.length === 0) {
    fail(`${label}.error.message must be a non-empty string`)
  }
  if (typeof error.retryable !== 'boolean') {
    fail(`${label}.error.retryable must be a boolean`)
  }
  requireEqual(
    error.retryable,
    contracts.retryableErrorCodes.includes(error.code),
    `${label}.error.retryable`,
  )
  requireObject(error.details, `${label}.error.details`)
}

function validateControlPlaneRequest(command, target, contracts, label) {
  const contract = contracts[command.kind]
  requireExactKeys(command.request, contract.exactKeys, `${label} ${command.kind} request`)
  requireEqual(command.request.schemaVersion, 'winwincode/v1', `${label} request schemaVersion`)
  if (command.kind === 'control-plane.command') {
    const payloadKeys = contract.payloadExactKeysByTarget[target]
    if (payloadKeys === undefined) fail(`${label} has unknown command target ${target}`)
    requireExactKeys(command.request.payload, payloadKeys, `${label} command payload`)
    if (target === 'delivery.create') {
      requireDeepEqual(command.request.payload.tasks, [], `${label} delivery.create tasks`)
    }
    return
  }

  requireExactKeys(command.request.page, contract.pageExactKeys, `${label} query page`)
  const parameters = contract.parameterKeysByTarget[target]
  if (parameters === undefined) fail(`${label} has unknown query target ${target}`)
  requireRequiredOptionalKeys(
    command.request.parameters,
    parameters.required,
    parameters.optional,
    `${label} query parameters`,
  )
  if (parameters.kind !== undefined) {
    requireEqual(
      command.request.parameters.kind,
      parameters.kind,
      `${label} query parameter kind`,
    )
  }
}

function validateControlPlaneResponse(command, target, contracts, label) {
  if (Object.hasOwn(command.response, 'error')) {
    validateErrorEnvelope(command.response, contracts, label)
    return
  }
  if (command.kind === 'control-plane.command') {
    requireExactKeys(
      command.response,
      contracts.controlPlaneCompletedExactKeys,
      `${label} completed response`,
    )
    requireEqual(command.response.command, target, `${label} response command`)
    requireEqual(command.response.outcome, 'completed', `${label} response outcome`)
    return
  }
  requireExactKeys(
    command.response,
    contracts.controlPlaneQueryExactKeys,
    `${label} query response`,
  )
  requireEqual(command.response.query, target, `${label} response query`)
  requireExactKeys(command.response.page, contracts.queryPageExactKeys, `${label} response page`)
}

function validateFixtureCommand(command, target, requestContract, responseContracts, label) {
  requireExactKeys(command.request, requestContract.exactKeys, `${label} fixture request`)
  if (!requestContract.targets.includes(target)) {
    fail(`${label} has unknown fixture target ${target}`)
  }
  const inputKeys = requestContract.inputExactKeysByTarget[target]
  if (inputKeys === undefined) fail(`${label} has no fixture input contract for ${target}`)
  requireExactKeys(command.request.input, inputKeys, `${label} fixture input`)
  const constants = requestContract.inputConstantsByTarget[target] ?? {}
  for (const [name, expected] of Object.entries(constants)) {
    requireDeepEqual(command.request.input[name], expected, `${label} fixture input ${name}`)
  }
  if (responseContracts.fixtureRejectedTargets.includes(target)) {
    requireExactKeys(
      command.response,
      responseContracts.fixtureRejectedExactKeys,
      `${label} rejected fixture response`,
    )
    requireEqual(command.response.outcome, 'rejected', `${label} fixture outcome`)
    validateCanonicalError(command.response.error, responseContracts, label)
    return
  }
  requireExactKeys(
    command.response,
    responseContracts.fixtureSuccessExactKeys,
    `${label} fixture response`,
  )
  requireEqual(command.response.outcome, 'completed', `${label} fixture outcome`)
  const resultKeys = responseContracts.fixtureResultExactKeysByTarget[target]
  if (resultKeys === undefined) fail(`${label} has no fixture result contract for ${target}`)
  requireExactKeys(command.response.result, resultKeys, `${label} fixture result`)
}

function validateExecutionPortMessage(command, group, rules, label) {
  const requestContract = rules.canonicalExpected.requestContracts['execution-port.message']
  const responseContracts = rules.canonicalExpected.responseContracts
  const requestKeys = requestContract.exactKeysByTarget[group.target]
  if (requestKeys === undefined) fail(`${label} has unknown message target ${group.target}`)
  requireExactKeys(command.request, requestKeys, `${label} message request`)
  requireEqual(command.request.kind, group.target, `${label} message kind`)
  requireEqual(command.request.schemaVersion, 'winwincode/v1', `${label} message schemaVersion`)
  requireExactKeys(command.request.lease, requestContract.leaseExactKeys, `${label} message lease`)
  if (group.target === 'job.outcome') {
    requireExactKeys(
      command.request.outcome,
      requestContract.outcomeExactKeysByTarget[group.target],
      `${label} message outcome`,
    )
    const allowedStatuses = requestContract.outcomeStatusesByTarget[group.target]
    if (!Array.isArray(allowedStatuses)
      || !allowedStatuses.includes(command.request.outcome.status)) {
      fail(`${label} message outcome status is not allowed`)
    }
    const artifacts = requireArray(
      command.request.outcome.artifacts,
      `${label} message outcome artifacts`,
    )
    for (let index = 0; index < artifacts.length; index += 1) {
      requireExactKeys(
        artifacts[index],
        requestContract.artifactExactKeys,
        `${label} message outcome artifacts[${index}]`,
      )
    }
  }

  if (group.revisionEffect.delta === 0) {
    requireExactKeys(
      command.response,
      responseContracts.executionPortMessageRejectedExactKeys,
      `${label} rejected message response`,
    )
    requireEqual(command.response.outcome, 'rejected', `${label} message outcome`)
    requireEqual(command.response.messageId, command.request.messageId, `${label} response messageId`)
    validateCanonicalError(command.response.error, responseContracts, label)
    return
  }

  requireExactKeys(
    command.response,
    responseContracts.executionPortMessageCompletedExactKeys,
    `${label} completed message response`,
  )
  requireEqual(command.response.outcome, 'completed', `${label} message outcome`)
  requireEqual(command.response.messageId, command.request.messageId, `${label} response messageId`)
  requireEqual(
    command.response.currentRevision,
    command.response.previousRevision + group.revisionEffect.delta,
    `${label} message revision span`,
  )
  const commits = requireArray(command.response.commits, `${label} message commits`)
  const operations = responseContracts.executionPortCommitOperationsByTarget[group.target]
  if (operations === undefined) fail(`${label} has no message response contract for ${group.target}`)
  requireEqual(commits.length, operations.length, `${label} message commit count`)
  for (let index = 0; index < commits.length; index += 1) {
    const commit = commits[index]
    const commitLabel = `${label} message commits[${index}]`
    requireExactKeys(commit, responseContracts.executionPortCommitExactKeys, commitLabel)
    requireEqual(
      commit.operation,
      operations[index],
      `${commitLabel}.operation`,
    )
    requireEqual(
      commit.currentRevision,
      commit.previousRevision + 1,
      `${commitLabel} revision span`,
    )
    requireExactKeys(
      commit.receipt,
      rules.canonicalExpected.exactStoreReceiptKeys,
      `${commitLabel}.receipt`,
    )
    const events = requireArray(commit.receipt.events, `${commitLabel}.receipt.events`)
    for (let eventIndex = 0; eventIndex < events.length; eventIndex += 1) {
      requireExactKeys(
        events[eventIndex],
        rules.canonicalExpected.exactStoreReceiptEventKeys,
        `${commitLabel}.receipt.events[${eventIndex}]`,
      )
    }
  }
  requireEqual(
    commits[0].previousRevision,
    command.response.previousRevision,
    `${label} first commit revision`,
  )
  for (let index = 1; index < commits.length; index += 1) {
    requireEqual(
      commits[index].previousRevision,
      commits[index - 1].currentRevision,
      `${label} commit ${index} continuity`,
    )
  }
  requireEqual(
    commits.at(-1).currentRevision,
    command.response.currentRevision,
    `${label} final commit revision`,
  )
}

function validateCanonicalCommand(command, group, rules, label) {
  const target = commandTarget(command)
  requireEqual(target, group.target, `${label} target`)
  const requestContracts = rules.canonicalExpected.requestContracts
  const responseContracts = rules.canonicalExpected.responseContracts
  if (command.kind === 'execution-port.message') {
    validateExecutionPortMessage(command, group, rules, label)
    return
  }
  if (command.kind === 'fixture.command') {
    validateFixtureCommand(
      command,
      target,
      requestContracts['fixture.command'],
      responseContracts,
      label,
    )
    return
  }
  validateControlPlaneRequest(command, target, requestContracts, label)
  validateControlPlaneResponse(command, target, responseContracts, label)
}

function selectRuntimeProjectionStage(deliveryResult, label) {
  requireObject(deliveryResult, `${label} delivery result`)
  const stages = requireArray(deliveryResult.stages, `${label} delivery result stages`)
  let selected = null
  for (let index = 0; index < stages.length; index += 1) {
    const stage = requireObject(stages[index], `${label} delivery result stages[${index}]`)
    if (stage.actorType !== 'codex' || stage.sessionBinding === null) continue
    const binding = requireObject(
      stage.sessionBinding,
      `${label} delivery result stages[${index}].sessionBinding`,
    )
    if (binding.workerSessionId == null || binding.codexThreadId == null) continue
    if (typeof stage.id !== 'string' || stage.id.length === 0) {
      fail(`${label} selected stage id must be a non-empty string`)
    }
    if (typeof binding.productSessionId !== 'string'
      || binding.productSessionId.length === 0) {
      fail(`${label} selected ProductSession identity must be a non-empty string`)
    }
    selected = {
      codexThreadId: binding.codexThreadId,
      productSessionId: binding.productSessionId,
      stageRunId: stage.id,
      workerSessionId: binding.workerSessionId,
    }
  }
  return selected
}

function validateRuntimeProjectionScenario(mapping, scenario, rules, label) {
  const policy = rules.canonicalMigration.runtimeProjectionFollowup
  const responseContracts = rules.canonicalExpected.responseContracts
  let lastRuntimeResult = null

  for (const [sourceIndex, signature] of mapping.sourceCommandSignatures.entries()) {
    if (signature !== `strongflow.request:${policy.sourceOperation}`) continue
    const deliveryCommands = scenario.commands.filter(command => (
      command.sourceCommandIndexes.includes(sourceIndex)
        && commandTarget(command) === policy.deliveryQueryTarget
    ))
    requireEqual(
      deliveryCommands.length,
      1,
      `${label} source ${sourceIndex} delivery query count`,
    )
    const deliveryCommand = deliveryCommands[0]
    const runtimeCommands = scenario.commands.filter(command => (
      command.sourceCommandIndexes.includes(sourceIndex)
        && commandTarget(command) === policy.runtimeQueryTarget
    ))
    const selected = Object.hasOwn(deliveryCommand.response, 'error')
      ? null
      : selectRuntimeProjectionStage(
          deliveryCommand.response.result,
          `${label} source ${sourceIndex}`,
        )
    requireEqual(
      runtimeCommands.length,
      selected === null ? 0 : 1,
      `${label} source ${sourceIndex} runtime query count`,
    )
    if (selected === null) {
      if (!Object.hasOwn(deliveryCommand.response, 'error')) lastRuntimeResult = null
      continue
    }

    const runtimeCommand = runtimeCommands[0]
    const deliveryIndex = scenario.commands.indexOf(deliveryCommand)
    requireEqual(
      scenario.commands[deliveryIndex + 1],
      runtimeCommand,
      `${label} source ${sourceIndex} runtime query position`,
    )
    requireDeepEqual(
      runtimeCommand.request.parameters.atCursor,
      deliveryCommand.response.result.readCursor,
      `${label} source ${sourceIndex} runtime atCursor`,
    )
    requireEqual(
      runtimeCommand.request.parameters.deliveryId,
      deliveryCommand.response.result.deliveryId,
      `${label} source ${sourceIndex} runtime deliveryId`,
    )
    requireEqual(
      runtimeCommand.request.parameters.productSessionId,
      selected.productSessionId,
      `${label} source ${sourceIndex} runtime productSessionId`,
    )
    requireEqual(
      runtimeCommand.request.parameters.stageRunId,
      selected.stageRunId,
      `${label} source ${sourceIndex} runtime stageRunId`,
    )
    if (Object.hasOwn(runtimeCommand.response, 'error')) {
      fail(`${label} source ${sourceIndex} selected runtime query must complete`)
    }
    const result = runtimeCommand.response.result
    requireExactKeys(
      result,
      responseContracts.runtimeProjectionGetResultExactKeys,
      `${label} source ${sourceIndex} runtime result`,
    )
    requireEqual(result.kind, 'runtime_projection', `${label} runtime result kind`)
    requireEqual(
      result.deliveryId,
      deliveryCommand.response.result.deliveryId,
      `${label} runtime result deliveryId`,
    )
    requireEqual(
      result.productSessionId,
      selected.productSessionId,
      `${label} runtime result productSessionId`,
    )
    requireEqual(
      result.stageRunId,
      selected.stageRunId,
      `${label} runtime result stageRunId`,
    )
    requireDeepEqual(
      result.readCursor,
      deliveryCommand.response.result.readCursor,
      `${label} runtime result readCursor`,
    )
    const sessions = requireArray(result.sessions, `${label} runtime result sessions`)
    requireEqual(
      sessions.length,
      responseContracts.runtimeProjectionGetSessionsExactLength,
      `${label} runtime result session count`,
    )
    const session = sessions[0]
    requireExactKeys(
      session,
      responseContracts.runtimeProjectionSessionExactKeys,
      `${label} runtime result session`,
    )
    requireEqual(
      session.stageRunId,
      selected.stageRunId,
      `${label} runtime session stageRunId`,
    )
    requireEqual(
      session.productSessionId,
      selected.productSessionId,
      `${label} runtime session productSessionId`,
    )
    requireEqual(
      session.workerSessionId,
      selected.workerSessionId,
      `${label} runtime session workerSessionId`,
    )
    requireEqual(
      session.codexThreadId,
      selected.codexThreadId,
      `${label} runtime session codexThreadId`,
    )
    lastRuntimeResult = result
  }

  requireDeepEqual(
    scenario.observation.projection.runtime,
    lastRuntimeResult,
    `${label} runtime observation`,
  )
}

function commandFromSource(scenario, sourceCommandIndex, target) {
  const matches = scenario.commands.filter(command => (
    command.sourceCommandIndexes.includes(sourceCommandIndex)
      && (target === undefined || commandTarget(command) === target)
  ))
  if (matches.length !== 1) {
    fail(
      `${scenario.id} expected one canonical command for source ${sourceCommandIndex}`
        + `${target === undefined ? '' : ` and target ${target}`}, found ${matches.length}`,
    )
  }
  return matches[0]
}

function responseErrorCode(scenario, sourceCommandIndex, target) {
  return commandFromSource(scenario, sourceCommandIndex, target).response.error?.code ?? null
}

function responseCurrentRevision(scenario, sourceCommandIndex, target) {
  return commandFromSource(scenario, sourceCommandIndex, target)
    .response.error?.details?.currentRevision ?? null
}

function journalSnapshots(scenario) {
  return scenario.observation.store.journal.records
    .map(record => record.snapshot)
    .filter(snapshot => snapshot !== undefined)
}

function verdictStatuses(scenario) {
  const statuses = []
  for (const snapshot of journalSnapshots(scenario)) {
    const status = snapshot.verdict?.status
    if (status !== undefined && statuses.at(-1) !== status) statuses.push(status)
  }
  return statuses
}

function canonicalCheckpoint(snapshot) {
  return {
    finalRevision: snapshot.revision,
    sessionBindingCount: snapshot.sessionBindings.length,
    stageRunCount: snapshot.stageRuns.length,
    taskCount: snapshot.tasks.length,
  }
}

function deriveCanonicalAssertions(scenario) {
  const snapshot = scenario.observation.snapshot
  switch (scenario.id) {
    case 'success-closed-loop':
      return {
        ...canonicalCheckpoint(snapshot),
        finalStatus: snapshot.status,
        verdict: scenario.observation.verdict?.status ?? null,
      }
    case 'request-id-replay': {
      const first = commandFromSource(scenario, 0, 'delivery.create').response.result
      const replay = commandFromSource(scenario, 1, 'delivery.create').response.result
      return {
        durableRecordCount: scenario.observation.store.journal.records.length,
        finalRevision: snapshot.revision,
        replayedSnapshotEqual: findFirstJsonDifference(first, replay) === null,
      }
    }
    case 'revision-conflict': {
      const currentRevision = responseCurrentRevision(
        scenario,
        2,
        'delivery.update_spec',
      )
      return {
        currentRevision,
        errorCode: responseErrorCode(scenario, 2, 'delivery.update_spec'),
        finalRevision: snapshot.revision,
        snapshotUnchanged: snapshot.revision === currentRevision,
      }
    }
    case 'corruption-recovery':
      return {
        corruptedReadError: responseErrorCode(scenario, 2, 'delivery.get'),
        finalRevision: snapshot.revision,
        restoredSnapshotEqual: findFirstJsonDifference(
          scenario.observation.store.journal.snapshot,
          snapshot,
        ) === null,
      }
    case 'task-dag':
      return {
        advancedTaskId: snapshot.tasks.find(task => task.status === 'active')?.id ?? null,
        ...canonicalCheckpoint(snapshot),
        cycleError: responseErrorCode(scenario, 2, 'fixture.solution-review.validate'),
        cycleRejectedWithoutWrite: scenario.observation.store.state.revision
          === commandFromSource(scenario, 1, 'delivery.advance').response.currentRevision,
        durableTaskOrder: snapshot.tasks.map(task => task.id),
      }
    case 'candidate-invalidation': {
      const candidates = new Set(
        journalSnapshots(scenario)
          .map(entry => entry.verdict?.candidateRef)
          .filter(candidate => candidate !== undefined),
      )
      return {
        candidateChanged: candidates.size > 1,
        ...canonicalCheckpoint(snapshot),
        staleCandidateError: responseErrorCode(scenario, 25, 'delivery.submit_verdict'),
      }
    }
    case 'attention':
      return {
        attentionItemStatus: snapshot.attentionItems.at(-1)?.status ?? null,
        ...canonicalCheckpoint(snapshot),
        openAttentionStatus: journalSnapshots(scenario)
          .some(entry => entry.status === 'needs-attention')
          ? 'needs-attention'
          : null,
        resolvedStatus: snapshot.status,
      }
    case 'inconclusive':
    case 'infra-error':
      return {
        attentionTypes: snapshot.attentionItems
          .filter(item => item.status === 'open')
          .map(item => item.type),
        ...canonicalCheckpoint(snapshot),
        verdictStatus: scenario.observation.verdict?.status ?? null,
      }
    case 'rework': {
      const candidates = new Set(
        journalSnapshots(scenario)
          .map(entry => entry.verdict?.candidateRef)
          .filter(candidate => candidate !== undefined),
      )
      return {
        candidateChanged: candidates.size > 1,
        enteredRework: journalSnapshots(scenario)
          .some(entry => entry.status === 'reworking'),
        ...canonicalCheckpoint(snapshot),
        verdicts: verdictStatuses(scenario),
      }
    }
    default:
      fail(`canonical assertion evaluator has no scenario ${scenario.id}`)
  }
}

function validateCanonicalAssertions(mapping, scenario) {
  const derived = deriveCanonicalAssertions(scenario)
  requireExactKeys(
    derived,
    Object.keys(mapping.canonicalAssertions),
    `${scenario.id} canonical assertions`,
  )
  for (const [name, expected] of Object.entries(mapping.canonicalAssertions)) {
    if (expected === '$capture-exact') {
      if (derived[name] === undefined) {
        fail(`${scenario.id} canonical assertion ${name} has no exact value`)
      }
      continue
    }
    requireDeepEqual(derived[name], expected, `${scenario.id} canonical assertion ${name}`)
  }
}

function validateTaskDagCanonicalScenario(sourceScenario, scenario) {
  if (scenario.id !== 'task-dag') return
  const seed = commandFromSource(scenario, 0, 'fixture.store.seed-snapshot')
  const sourceSnapshot = sourceScenario.commands[0].input.snapshot
  const expectedSnapshot = structuredClone(sourceSnapshot)
  expectedSnapshot.tasks = migrateLegacyTaskGraph(expectedSnapshot.id, expectedSnapshot.tasks)
  requireDeepEqual(
    seed.request.input.snapshot,
    expectedSnapshot,
    'task-dag migrated seed snapshot',
  )

  const cycle = commandFromSource(scenario, 2, 'fixture.solution-review.validate')
  requireDeepEqual(
    cycle.request.input.spec,
    sourceScenario.commands[2].request.payload.spec,
    'task-dag cycle spec',
  )
  requireEqual(
    cycle.request.input.invalidProposalKind,
    'dependency-cycle',
    'task-dag invalid proposal kind',
  )
  requireEqual(cycle.response.outcome, 'rejected', 'task-dag cycle fixture outcome')
  requireEqual(cycle.response.error.code, 'INVALID_REQUEST', 'task-dag cycle fixture error')
}

function validateTerminalOutcomeCanonicalScenario(sourceScenario, mapping, scenario, rules) {
  const groups = mapping.canonicalGroups.filter(group => group.target === 'job.outcome')
  for (const group of groups) {
    const sourceCommandIndex = group.sourceCommandIndexes[0]
    const sourceCommand = sourceScenario.commands[sourceCommandIndex]
    const terminal = terminalVerifierFact(
      sourceCommand,
      `${scenario.id} source command ${sourceCommandIndex}`,
    )
    const outcome = commandFromSource(scenario, sourceCommandIndex, 'job.outcome')
    const expectedStatus = terminalOutcomeStatusForFact(
      sourceScenario,
      sourceCommandIndex,
      `${scenario.id} terminal outcome`,
    )
    requireEqual(
      rules.canonicalMigration.terminalOutcomeMessages
        .outcomeStatusByScenario[scenario.id],
      expectedStatus,
      `${scenario.id} terminal outcome status policy`,
    )
    requireEqual(
      outcome.request.outcome.status,
      expectedStatus,
      `${scenario.id} terminal outcome status`,
    )
    const outcomeIndex = scenario.commands.indexOf(outcome)
    const binding = scenario.commands.slice(0, outcomeIndex).findLast(command => (
      command.request.kind === 'session.binding'
        && command.response.outcome === 'completed'
    ))
    if (binding === undefined) {
      fail(`${scenario.id} terminal outcome has no preceding completed SessionBindingMessage`)
    }
    requireDeepEqual(
      outcome.request.lease,
      binding.request.lease,
      `${scenario.id} terminal outcome lease`,
    )
    requireEqual(
      outcome.request.workerSessionId,
      binding.request.workerSessionId,
      `${scenario.id} terminal outcome workerSessionId`,
    )
    requireEqual(
      outcome.request.outcome.codexThreadId,
      binding.request.codexThreadId,
      `${scenario.id} terminal outcome codexThreadId`,
    )
    const finishedAt = new Date(terminal.occurredAtMillis).toISOString()
    requireEqual(
      outcome.request.outcome.finishedAt,
      finishedAt,
      `${scenario.id} terminal outcome finishedAt`,
    )
    requireEqual(
      outcome.request.sentAt,
      finishedAt,
      `${scenario.id} terminal outcome sentAt`,
    )
    requireEqual(
      outcome.request.outcome.lastEventSequence,
      Number(terminal.cursor.sequence),
      `${scenario.id} terminal outcome lastEventSequence`,
    )
    requireEqual(
      outcome.request.outcome.summary,
      terminal.data.last_agent_message,
      `${scenario.id} terminal outcome summary`,
    )
    requireDeepEqual(
      outcome.request.outcome.artifacts,
      [],
      `${scenario.id} terminal outcome artifacts`,
    )
  }
}

export function validateCanonicalExpected(rules, oracle, expected) {
  requireExactKeys(expected, rules.canonicalExpected.exactTopLevelKeys, 'canonical expected')
  requireEqual(
    expected.schemaVersion,
    rules.canonicalExpected.schemaVersion,
    'canonical expected schemaVersion',
  )
  requireExactKeys(
    expected.migration,
    rules.canonicalExpected.exactMigrationKeys,
    'canonical expected migration',
  )
  requireEqual(
    expected.migration.mappingVersion,
    rules.canonicalMigration.mappingVersion,
    'canonical migration version',
  )
  requireEqual(
    expected.migration.sourceOracleSchemaVersion,
    oracle.schemaVersion,
    'canonical source oracle schema',
  )
  requireEqual(
    expected.migration.sourceOracleSha256,
    rules.oracle.sha256,
    'canonical source oracle sha256',
  )
  requireExactKeys(expected.result, rules.canonicalExpected.exactResultKeys, 'canonical result')
  requireEqual(
    expected.result.schemaVersion,
    rules.canonicalExpected.resultSchemaVersion,
    'canonical result schemaVersion',
  )
  requireEqual(
    expected.result.oracleSchemaVersion,
    oracle.schemaVersion,
    'canonical result oracleSchemaVersion',
  )

  const scenarios = requireArray(expected.result.scenarios, 'canonical result scenarios')
  requireEqual(scenarios.length, rules.scenarios.length, 'canonical expected scenario count')
  for (let index = 0; index < scenarios.length; index += 1) {
    const scenario = scenarios[index]
    const mapping = rules.scenarios[index]
    const label = `canonical result scenarios[${index}]`
    requireExactKeys(scenario, rules.canonicalExpected.exactScenarioKeys, label)
    requireEqual(scenario.id, mapping.id, `${label}.id`)
    requireExactKeys(
      scenario.observation,
      rules.canonicalExpected.exactObservationKeys,
      `${label}.observation`,
    )
    requireArray(scenario.observation.events, `${label}.observation.events`)
    requireExactKeys(
      scenario.observation.projection,
      rules.canonicalExpected.exactProjectionKeys,
      `${label}.observation.projection`,
    )
    requireExactKeys(
      scenario.observation.store,
      rules.canonicalExpected.exactStoreKeys,
      `${label}.observation.store`,
    )
    requireExactKeys(
      scenario.observation.store.journal,
      rules.canonicalExpected.exactStoreJournalKeys,
      `${label}.observation.store.journal`,
    )
    requireExactKeys(
      scenario.observation.store.state,
      rules.canonicalExpected.exactStoreStateKeys,
      `${label}.observation.store.state`,
    )
    requireExactKeys(
      scenario.observation.store.journal.manifest,
      rules.canonicalExpected.exactStoreManifestKeys,
      `${label}.observation.store.journal.manifest`,
    )
    const records = requireArray(
      scenario.observation.store.journal.records,
      `${label}.observation.store.journal.records`,
    )
    for (let recordIndex = 0; recordIndex < records.length; recordIndex += 1) {
      requireExactKeys(
        records[recordIndex],
        rules.canonicalExpected.exactStoreRecordKeys,
        `${label}.observation.store.journal.records[${recordIndex}]`,
      )
    }
    const receipts = requireArray(
      scenario.observation.store.receipts,
      `${label}.observation.store.receipts`,
    )
    for (let receiptIndex = 0; receiptIndex < receipts.length; receiptIndex += 1) {
      const receipt = receipts[receiptIndex]
      const receiptLabel = `${label}.observation.store.receipts[${receiptIndex}]`
      requireExactKeys(receipt, rules.canonicalExpected.exactStoreReceiptKeys, receiptLabel)
      requireEqual(
        receipt.idempotentReplay,
        rules.canonicalExpected.persistedReceiptIdempotentReplay,
        `${receiptLabel}.idempotentReplay`,
      )
      const receiptEvents = requireArray(receipt.events, `${receiptLabel}.events`)
      for (let eventIndex = 0; eventIndex < receiptEvents.length; eventIndex += 1) {
        requireExactKeys(
          receiptEvents[eventIndex],
          rules.canonicalExpected.exactStoreReceiptEventKeys,
          `${receiptLabel}.events[${eventIndex}]`,
        )
      }
    }
    const outbox = requireArray(
      scenario.observation.store.outbox,
      `${label}.observation.store.outbox`,
    )
    for (let eventIndex = 0; eventIndex < outbox.length; eventIndex += 1) {
      requireExactKeys(
        outbox[eventIndex],
        rules.canonicalExpected.exactStoreOutboxKeys,
        `${label}.observation.store.outbox[${eventIndex}]`,
      )
      if (typeof outbox[eventIndex].published !== 'boolean') {
        fail(`${label}.observation.store.outbox[${eventIndex}].published must be a boolean`)
      }
    }

    const commands = requireArray(scenario.commands, `${label}.commands`)
    requireEqual(commands.length, mapping.canonicalGroups.length, `${label} command count`)
    for (let commandIndex = 0; commandIndex < commands.length; commandIndex += 1) {
      const command = commands[commandIndex]
      const group = mapping.canonicalGroups[commandIndex]
      requireExactKeys(
        command,
        rules.canonicalExpected.exactCommandKeys,
        `${label}.commands[${commandIndex}]`,
      )
      requireEqual(command.kind, group.kind, `${label}.commands[${commandIndex}].kind`)
      requireDeepEqual(
        command.sourceCommandIndexes,
        group.sourceCommandIndexes,
        `${label}.commands[${commandIndex}].sourceCommandIndexes`,
      )
      requireObject(command.request, `${label}.commands[${commandIndex}].request`)
      requireObject(command.response, `${label}.commands[${commandIndex}].response`)
      validateCanonicalCommand(
        command,
        group,
        rules,
        `${label}.commands[${commandIndex}]`,
      )
    }
    validateRuntimeProjectionScenario(mapping, scenario, rules, label)
    validateTaskDagCanonicalScenario(oracle.scenarios[index], scenario)
    validateTerminalOutcomeCanonicalScenario(oracle.scenarios[index], mapping, scenario, rules)
    validateCanonicalAssertions(mapping, scenario)
  }
  return { scenarioCount: scenarios.length }
}

function rawPlaceholder(value, placeholders) {
  if (typeof value === 'string') return placeholders.find(placeholder => value.includes(placeholder))
  if (Array.isArray(value)) {
    for (const entry of value) {
      const found = rawPlaceholder(entry, placeholders)
      if (found !== undefined) return found
    }
    return undefined
  }
  if (value === null || typeof value !== 'object') return undefined
  for (const entry of Object.values(value)) {
    const found = rawPlaceholder(entry, placeholders)
    if (found !== undefined) return found
  }
  return undefined
}

export function normalizeDifferentialResult(actual, bindings, rules) {
  for (const key of Object.keys(bindings)) {
    if (!REQUIRED_BINDINGS.includes(key)) fail(`unknown normalization binding: ${key}`)
  }
  validateBindings(bindings, rules)
  const placeholders = rules.normalization.allowedBindings.map(binding => binding.placeholder)
  placeholders.push(...rules.normalization.fixtureRandomIdentities.map(identity => identity.placeholder))
  const found = rawPlaceholder(actual, placeholders)
  if (found !== undefined) fail(`raw Rust result contains reserved placeholder ${found}`)

  return mapJson(actual, value => {
    let normalized = replaceEvery(value, bindings.ORACLE_ROOT, '<ORACLE_ROOT>')
    normalized = replaceEvery(normalized, bindings.NODE_EXECUTABLE, '<NODE_EXECUTABLE>')
    if (normalized === bindings.AUTH_PROOF) normalized = '<AUTH_PROOF>'
    for (const identity of rules.normalization.fixtureRandomIdentities) {
      const source = bindings.fixtureRandomIdentities[identity.name]
      if (normalized === source) normalized = identity.placeholder
    }
    return normalized
  })
}

function valueKind(value) {
  if (value === null) return 'null'
  if (Array.isArray(value)) return 'array'
  return typeof value
}

function pointer(path, component) {
  const escaped = String(component).replaceAll('~', '~0').replaceAll('/', '~1')
  return `${path}/${escaped}`
}

export function findFirstJsonDifference(expected, actual, path = '') {
  const expectedKind = valueKind(expected)
  const actualKind = valueKind(actual)
  if (expectedKind !== actualKind) {
    return { actual, expected, kind: 'type', path }
  }
  if (expectedKind === 'array') {
    const sharedLength = Math.min(expected.length, actual.length)
    for (let index = 0; index < sharedLength; index += 1) {
      const difference = findFirstJsonDifference(expected[index], actual[index], pointer(path, index))
      if (difference !== null) return difference
    }
    if (expected.length !== actual.length) {
      const index = sharedLength
      return expected.length > actual.length
        ? { actual: '<missing>', expected: expected[index], kind: 'missing', path: pointer(path, index) }
        : { actual: actual[index], expected: '<missing>', kind: 'extra', path: pointer(path, index) }
    }
    return null
  }
  if (expectedKind === 'object') {
    const keys = [...new Set([...Object.keys(expected), ...Object.keys(actual)])].toSorted()
    for (const key of keys) {
      const childPath = pointer(path, key)
      if (!Object.hasOwn(actual, key)) {
        return { actual: '<missing>', expected: expected[key], kind: 'missing', path: childPath }
      }
      if (!Object.hasOwn(expected, key)) {
        return { actual: actual[key], expected: '<missing>', kind: 'extra', path: childPath }
      }
      const difference = findFirstJsonDifference(expected[key], actual[key], childPath)
      if (difference !== null) return difference
    }
    return null
  }
  return Object.is(expected, actual)
    ? null
    : { actual, expected, kind: 'value', path }
}

function printable(value) {
  return typeof value === 'string' ? JSON.stringify(value) : JSON.stringify(value)
}

export function assertDifferentialResult(expected, rawActual, bindings, rules) {
  const actual = normalizeDifferentialResult(rawActual, bindings, rules)
  const difference = findFirstJsonDifference(expected, actual)
  if (difference === null) return actual
  const error = new Error(
    `Rust differential mismatch at ${difference.path || '<root>'}: `
      + `expected ${printable(difference.expected)}, actual ${printable(difference.actual)}`,
  )
  error.code = 'DIFFERENTIAL_MISMATCH'
  error.difference = difference
  throw error
}

function parseJson(bytes, label) {
  try {
    return JSON.parse(bytes)
  } catch (error) {
    throw new Error(`${label} is not valid JSON: ${error.message}`, { cause: error })
  }
}

function defaultBindings(fixtureRoot) {
  return {
    AUTH_PROOF: `winwincode-differential-auth-${process.pid}`,
    NODE_EXECUTABLE: process.execPath,
    ORACLE_ROOT: fixtureRoot,
    fixtureRandomIdentities: {},
  }
}

export async function runDifferentialGate(options = {}) {
  const root = options.root ?? join(import.meta.dirname, '..')
  const spawn = options.spawn ?? spawnSync
  const rulesBytes = await readFile(join(root, RULES_PATH), 'utf8')
  const rules = parseJson(rulesBytes, RULES_PATH)
  const oracleBytes = await readFile(join(root, rules.oracle.path))
  const oracle = parseJson(oracleBytes, rules.oracle.path)
  const validated = validateDifferentialContract(rules, oracle, { sourceBytes: oracleBytes })
  const active = rules.runner.triggerPaths.some(path => existsSync(join(root, path)))

  if (!active) {
    const bindings = {
      AUTH_PROOF: 'contract-only-auth-proof',
      NODE_EXECUTABLE: process.execPath,
      ORACLE_ROOT: join(root, '.contract-only-oracle-root'),
      fixtureRandomIdentities: {},
    }
    buildDifferentialExecutionPlan(oracle, bindings, rules)
    buildCanonicalMigrationPlan(oracle, rules)
    return { ...validated, status: 'contract-only' }
  }

  for (const path of rules.runner.requiredPaths) {
    if (!existsSync(join(root, path))) fail(`triggered Rust differential runner is missing ${path}`)
  }

  const temporaryRoot = await mkdtemp(join(tmpdir(), 'winwincode-rust-differential-'))
  try {
    const fixtureRoot = join(temporaryRoot, 'oracle-root')
    await mkdir(fixtureRoot, { recursive: true })
    const bindings = defaultBindings(fixtureRoot)
    const plan = buildDifferentialExecutionPlan(oracle, bindings, rules)
    const inputPath = join(temporaryRoot, 'plan.json')
    const outputPath = join(temporaryRoot, 'actual.json')
    writeFileSync(inputPath, `${JSON.stringify(plan, null, 2)}\n`, { mode: 0o600 })
    const [command, ...arguments_] = rules.runner.command
    const result = spawn(command, arguments_, {
      cwd: root,
      encoding: 'utf8',
      env: {
        ...process.env,
        [rules.runner.inputEnvironment]: inputPath,
        [rules.runner.outputEnvironment]: outputPath,
      },
      maxBuffer: 64 * 1024 * 1024,
    })
    if (result.error !== undefined) throw result.error
    if (result.signal !== null) fail(`Rust differential runner ended with ${result.signal}`)
    if (result.status !== 0) {
      fail(
        `Rust differential runner failed with status ${result.status}\n`
          + `${result.stdout ?? ''}${result.stderr ?? ''}`,
      )
    }
    if (!existsSync(outputPath)) fail('Rust differential runner did not write its output file')
    const rawActual = parseJson(readFileSync(outputPath, 'utf8'), 'Rust differential result')

    const expectedPath = join(root, rules.canonicalExpected.path)
    if (!existsSync(expectedPath)) {
      fail(`triggered Rust differential runner requires ${rules.canonicalExpected.path}`)
    }
    const expected = parseJson(readFileSync(expectedPath, 'utf8'), 'canonical expected result')
    validateCanonicalExpected(rules, oracle, expected)
    assertDifferentialResult(expected.result, rawActual, bindings, rules)
    return { ...validated, status: 'matched' }
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true })
  }
}
