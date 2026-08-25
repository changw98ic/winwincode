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
        if (group.kind === 'execution-port.message' && ![0, 2].includes(effect.delta)) {
          fail(`${mapping.id} session.binding revision delta must be zero or two`)
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
      group.kind === 'execution-port.message' && group.revisionEffect.delta === 2
    )).length,
  ]))
  requireDeepEqual(
    successfulMessages,
    rules.canonicalMigration.sessionBindingMessages.successfulLegacyBindCountByScenario,
    'successful session binding message counts',
  )

  validateMigrationRules(rules, oracle)
  validateNormalizationRules(rules)
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

export function buildDifferentialExecutionPlan(oracle, bindings, rules) {
  validateBindings(bindings, rules)
  const scenarios = oracle.scenarios.map(scenario => ({
    id: scenario.id,
    commands: scenario.commands.map(command => {
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
    }),
  }))
  return {
    bindings: structuredClone(bindings),
    oracleSchemaVersion: oracle.schemaVersion,
    scenarios,
    schemaVersion: rules.runner.inputSchemaVersion,
  }
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
  requireExactKeys(command.request, requestContract.exactKeys, `${label} message request`)
  requireEqual(command.request.kind, requestContract.kind, `${label} message kind`)
  requireEqual(command.request.schemaVersion, 'winwincode/v1', `${label} message schemaVersion`)
  requireExactKeys(command.request.lease, requestContract.leaseExactKeys, `${label} message lease`)

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
    command.response.previousRevision + 2,
    `${label} message revision span`,
  )
  const commits = requireArray(command.response.commits, `${label} message commits`)
  requireEqual(commits.length, 2, `${label} message commit count`)
  for (let index = 0; index < commits.length; index += 1) {
    const commit = commits[index]
    const commitLabel = `${label} message commits[${index}]`
    requireExactKeys(commit, responseContracts.executionPortCommitExactKeys, commitLabel)
    requireEqual(
      commit.operation,
      responseContracts.executionPortCommitOperations[index],
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
  requireEqual(
    commits[1].previousRevision,
    commits[0].currentRevision,
    `${label} commit continuity`,
  )
  requireEqual(
    commits[1].currentRevision,
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
        cycleError: responseErrorCode(scenario, 2, 'delivery.approve_task_breakdown'),
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
