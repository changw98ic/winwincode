import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION,
  STRONGFLOW_ROLE_IDS,
  STRONGFLOW_ROLE_SESSION_POLICY_SCHEMA_VERSION,
  RoleExecutionMode,
  StrongFlowRoleConfigurationError,
  createStrongFlowRoleConfiguration,
  migratePersistedRoleSessionPolicyV1,
  parseRoleSessionPolicy,
  parseStrongFlowRoleConfiguration,
  strongFlowRoleSessionPolicy,
  strongFlowRoleWorkspaceMode,
} from '../packages/contracts/dist/index.js'

const modelCatalog = Object.freeze([
  Object.freeze({
    provider: 'fixture-provider',
    model: 'fixture-model',
    reasoningEfforts: Object.freeze([null, 'low', 'medium', 'high']),
  }),
  Object.freeze({
    provider: 'fixture-provider',
    model: 'fixture-verifier',
    reasoningEfforts: Object.freeze(['high']),
  }),
])

const root = resolve(import.meta.dirname, '..')

function assignments() {
  return Object.fromEntries(STRONGFLOW_ROLE_IDS.map((roleId, index) => [
    roleId,
    {
      modelRoute: {
        provider: 'fixture-provider',
        model: roleId === 'adversarial-verifier'
          ? 'fixture-verifier'
          : 'fixture-model',
      },
      reasoningEffort: roleId === 'adversarial-verifier' ? 'high' : 'medium',
      budget: {
        maxTurns: 8 + index,
        maxWallTimeMillis: 60_000 + index,
        maxTotalTokens: 10_000 + index,
        maxCostUsdMicros: 1_000_000 + index,
      },
    },
  ]))
}

function expectedRoleError(code) {
  return error => error instanceof StrongFlowRoleConfigurationError && error.code === code
}

function jsonClone(value) {
  return JSON.parse(JSON.stringify(value))
}

test('creates eight canonical roles with only two candidate writers', () => {
  const configuration = createStrongFlowRoleConfiguration(assignments(), modelCatalog)

  assert.equal(configuration.schemaVersion, STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION)
  assert.deepEqual(configuration.roles.map(role => role.id), STRONGFLOW_ROLE_IDS)
  assert.ok(Object.isFrozen(STRONGFLOW_ROLE_IDS))
  assert.deepEqual(
    configuration.roles
      .filter(role => role.workspaceMode === 'candidate-write')
      .map(role => role.id),
    ['executor', 'remediator'],
  )
  for (const role of configuration.roles) {
    assert.equal(role.workspaceMode, strongFlowRoleWorkspaceMode(role.id))
    assert.ok(role.developerInstructions.length > 0)
    assert.ok(role.budget.maxTurns > 0)
    assert.ok(role.budget.maxWallTimeMillis > 0)
    assert.ok(role.budget.maxTotalTokens > 0)
    assert.ok(role.budget.maxCostUsdMicros > 0)
    assert.ok(Object.isFrozen(role))
  }

  assert.match(
    configuration.roles.find(role => role.id === 'requirements').developerInstructions,
    /DeliverySpec/u,
  )
  assert.match(
    configuration.roles.find(role => role.id === 'solution').developerInstructions,
    /system architecture and process-flow diagram data/u,
  )
  assert.match(
    configuration.roles.find(role => role.id === 'executor').developerInstructions,
    /Codex tools, sandbox, approvals, plan, and subagents/u,
  )
  assert.ok(Object.isFrozen(configuration))
  assert.ok(Object.isFrozen(configuration.roles))
})

test('creates a minimal Codex role-session policy without a second tool surface', () => {
  const schema = JSON.parse(readFileSync(
    join(root, 'schema/winwincode/v1/execution-port.schema.json'),
    'utf8',
  ))
  assert.deepEqual(Object.values(RoleExecutionMode), schema.$defs.RoleExecutionMode.enum)
  assert.deepEqual(schema.$defs.RoleSessionPolicy.required, [
    'schemaVersion',
    'roleId',
    'workspaceMode',
    'developerInstructions',
    'executionMode',
  ])
  assert.equal(schema.$defs.RoleSessionPolicy.additionalProperties, false)

  for (const roleId of STRONGFLOW_ROLE_IDS) {
    const policy = strongFlowRoleSessionPolicy(roleId)
    assert.deepEqual(Object.keys(policy), [
      'schemaVersion',
      'roleId',
      'workspaceMode',
      'developerInstructions',
      'executionMode',
    ])
    assert.equal(policy.schemaVersion, STRONGFLOW_ROLE_SESSION_POLICY_SCHEMA_VERSION)
    assert.equal(policy.roleId, roleId)
    assert.equal(policy.workspaceMode, strongFlowRoleWorkspaceMode(roleId))
    assert.equal(policy.executionMode, RoleExecutionMode.React)
    assert.ok(policy.developerInstructions.length > 0)
    assert.ok(Object.isFrozen(policy))
  }
})

test('delegated batch keeps Composer roles read-only while React retains direct writers', () => {
  for (const roleId of STRONGFLOW_ROLE_IDS) {
    const react = strongFlowRoleSessionPolicy(roleId, RoleExecutionMode.React)
    const delegated = strongFlowRoleSessionPolicy(roleId, RoleExecutionMode.DelegatedBatch)
    const composer = roleId === 'executor' || roleId === 'remediator'

    assert.equal(
      react.workspaceMode,
      composer ? 'candidate-write' : strongFlowRoleWorkspaceMode(roleId),
    )
    assert.equal(
      delegated.workspaceMode,
      composer ? 'candidate-read-only' : react.workspaceMode,
    )
    assert.equal(delegated.executionMode, RoleExecutionMode.DelegatedBatch)
    if (composer) {
      assert.notEqual(delegated.developerInstructions, react.developerInstructions)
      assert.match(delegated.developerInstructions, /bounded (?:ChangeBatch|Repair) proposal/u)
      assert.match(delegated.developerInstructions, /read-only candidate workspace/u)
      assert.doesNotMatch(delegated.developerInstructions, /^(?:Implement|Apply) only/u)
    } else {
      assert.equal(delegated.developerInstructions, react.developerInstructions)
    }
  }
})

test('runtime role-policy parsing accepts only canonical v2 without a legacy read path', () => {
  const canonical = strongFlowRoleSessionPolicy('executor', RoleExecutionMode.DelegatedBatch)
  assert.deepEqual(parseRoleSessionPolicy(jsonClone(canonical)), canonical)

  const cases = [
    { ...canonical, schemaVersion: 1 },
    { ...canonical, executionMode: 'composer' },
    { ...canonical, workspaceMode: 'candidate-write' },
    { ...canonical, unknownField: true },
  ]
  for (const value of cases) {
    assert.throws(
      () => parseRoleSessionPolicy(value),
      error => error instanceof StrongFlowRoleConfigurationError,
    )
  }
})

test('the explicit v1 migration writes canonical v2 before replay', () => {
  const react = strongFlowRoleSessionPolicy('executor', RoleExecutionMode.React)
  const persistedV1 = {
    schemaVersion: 1,
    roleId: react.roleId,
    workspaceMode: react.workspaceMode,
    developerInstructions: react.developerInstructions,
  }
  const migrated = migratePersistedRoleSessionPolicyV1(jsonClone(persistedV1))
  assert.deepEqual(migrated, react)
  assert.deepEqual(parseRoleSessionPolicy(jsonClone(migrated)), react)

  assert.throws(
    () => migratePersistedRoleSessionPolicyV1(jsonClone(migrated)),
    expectedRoleError('INVALID_ROLE_POLICY'),
  )
  assert.throws(
    () => migratePersistedRoleSessionPolicyV1({ ...persistedV1, unknownField: true }),
    expectedRoleError('INVALID_ROLE_POLICY'),
  )
})

test('JSON round-trip validates model routes and restores canonical role order', () => {
  const configuration = createStrongFlowRoleConfiguration(assignments(), modelCatalog)
  const serialized = jsonClone(configuration)
  serialized.roles.reverse()

  const parsed = parseStrongFlowRoleConfiguration(serialized, modelCatalog)
  assert.deepEqual(parsed, configuration)
  assert.deepEqual(parsed.roles.map(role => role.id), STRONGFLOW_ROLE_IDS)
  assert.equal(
    parsed.roles.find(role => role.id === 'adversarial-verifier').modelRoute.model,
    'fixture-verifier',
  )
  assert.equal(
    parsed.roles.find(role => role.id === 'adversarial-verifier').reasoningEffort,
    'high',
  )
})

test('startup rejects unknown roles, models, and reasoning efforts', () => {
  const configuration = createStrongFlowRoleConfiguration(assignments(), modelCatalog)
  const cases = [
    {
      code: 'UNKNOWN_ROLE',
      change(value) {
        value.roles[0].id = 'supervisor'
      },
    },
    {
      code: 'UNKNOWN_MODEL_ROUTE',
      change(value) {
        value.roles[0].modelRoute.model = 'missing-model'
      },
    },
    {
      code: 'UNKNOWN_REASONING_EFFORT',
      change(value) {
        value.roles[0].reasoningEffort = 'unsupported'
      },
    },
  ]

  for (const fixture of cases) {
    const value = jsonClone(configuration)
    fixture.change(value)
    assert.throws(
      () => parseStrongFlowRoleConfiguration(value, modelCatalog),
      expectedRoleError(fixture.code),
    )
  }
})

test('startup rejects missing limits, duplicate roles, extra fields, and old versions', () => {
  const configuration = createStrongFlowRoleConfiguration(assignments(), modelCatalog)
  const cases = [
    value => { value.roles[0].budget.maxTurns = 0 },
    value => { value.roles[1] = jsonClone(value.roles[0]) },
    value => { value.roles.pop() },
    value => { value.roles[0].untrusted = true },
    value => { value.schemaVersion = 2 },
  ]

  for (const change of cases) {
    const value = jsonClone(configuration)
    change(value)
    assert.throws(
      () => parseStrongFlowRoleConfiguration(value, modelCatalog),
      error => error instanceof StrongFlowRoleConfigurationError,
    )
  }
})

test('canonical role policies cannot be broadened through configuration', () => {
  const configuration = createStrongFlowRoleConfiguration(assignments(), modelCatalog)
  const cases = [
    value => {
      value.roles.find(role => role.id === 'reviewer').workspaceMode = 'candidate-write'
    },
    value => {
      value.roles.find(role => role.id === 'solution').developerInstructions = 'Approve and execute.'
    },
    value => {
      value.roles.find(role => role.id === 'executor').displayName = 'Unrestricted Executor'
    },
  ]

  for (const change of cases) {
    const value = jsonClone(configuration)
    change(value)
    assert.throws(
      () => parseStrongFlowRoleConfiguration(value, modelCatalog),
      expectedRoleError('POLICY_MISMATCH'),
    )
  }
})

test('the model catalog and runtime assignment map are validated before startup', () => {
  assert.throws(
    () => createStrongFlowRoleConfiguration(assignments(), []),
    expectedRoleError('INVALID_MODEL_CATALOG'),
  )

  const duplicateCatalog = [...modelCatalog, jsonClone(modelCatalog[0])]
  assert.throws(
    () => createStrongFlowRoleConfiguration(assignments(), duplicateCatalog),
    expectedRoleError('INVALID_MODEL_CATALOG'),
  )

  const missingAssignment = assignments()
  delete missingAssignment.remediator
  assert.throws(
    () => createStrongFlowRoleConfiguration(missingAssignment, modelCatalog),
    expectedRoleError('INVALID_CONFIGURATION'),
  )
})
