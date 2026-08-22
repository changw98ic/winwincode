import assert from 'node:assert/strict'
import test from 'node:test'

import {
  STRONGFLOW_ROLE_ARTIFACT_KINDS,
  STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION,
  STRONGFLOW_ROLE_IDS,
  STRONGFLOW_ROLE_TOOLS,
  StrongFlowRoleConfigurationError,
  createStrongFlowRoleConfiguration,
  parseStrongFlowRoleConfiguration,
  strongFlowPermissionPolicyForRole,
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

test('creates the eight canonical roles with only two candidate writers', () => {
  const configuration = createStrongFlowRoleConfiguration(assignments(), modelCatalog)

  assert.equal(
    configuration.schemaVersion,
    STRONGFLOW_ROLE_CONFIGURATION_SCHEMA_VERSION,
  )
  assert.deepEqual(configuration.roles.map(role => role.id), STRONGFLOW_ROLE_IDS)
  assert.ok(Object.isFrozen(STRONGFLOW_ROLE_IDS))
  assert.ok(Object.isFrozen(STRONGFLOW_ROLE_TOOLS))
  assert.ok(Object.isFrozen(STRONGFLOW_ROLE_ARTIFACT_KINDS))
  assert.deepEqual(
    configuration.roles
      .filter(role => role.workspaceMode === 'candidate-write')
      .map(role => role.id),
    ['executor', 'remediator'],
  )
  for (const role of configuration.roles) {
    const writer = ['executor', 'remediator'].includes(role.id)
    const permission = strongFlowPermissionPolicyForRole(role.id)
    assert.equal(role.permissionPreset, permission.presetId)
    assert.equal(permission.filesystem.mode, writer ? 'candidate-write' : 'read-only')
    assert.equal(permission.network.access, 'disabled')
    assert.equal(permission.approval.definitionDecision, 'forbidden')
    assert.equal(permission.tools.allowed.includes('candidate.patch'), writer)
    assert.equal(permission.tools.allowed.includes('human-approval'), false)
    assert.ok(role.budget.maxTurns > 0)
    assert.ok(role.budget.maxWallTimeMillis > 0)
    assert.ok(role.budget.maxTotalTokens > 0)
    assert.ok(role.budget.maxCostUsdMicros > 0)
    assert.ok(Object.isFrozen(role))
    assert.ok(Object.isFrozen(permission))
    assert.ok(Object.isFrozen(permission.tools.allowed))
  }

  const requirements = configuration.roles.find(role => role.id === 'requirements')
  assert.deepEqual(requirements.acceptedInputArtifacts, ['USER_REQUEST'])
  assert.deepEqual(requirements.requiredOutputArtifacts, ['REQUIREMENT_SPEC'])
  assert.match(requirements.systemInstructions, /Do not choose architecture/u)

  const solution = configuration.roles.find(role => role.id === 'solution')
  assert.deepEqual(solution.acceptedInputArtifacts, ['REQUIREMENT_SPEC'])
  assert.deepEqual(solution.requiredOutputArtifacts, [
    'SOLUTION_DESIGN',
    'SYSTEM_ARCHITECTURE_DIAGRAM',
    'PROCESS_FLOW_DIAGRAM',
  ])

  const planner = configuration.roles.find(role => role.id === 'planner')
  assert.ok(planner.acceptedInputArtifacts.includes('HUMAN_REVIEW_RECORD'))
  const executor = configuration.roles.find(role => role.id === 'executor')
  assert.deepEqual(executor.acceptedInputArtifacts, ['EXECUTION_PLAN'])
  assert.ok(Object.isFrozen(configuration))
  assert.ok(Object.isFrozen(configuration.roles))
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

test('startup rejects unknown roles, models, efforts, permission presets, and artifacts', () => {
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
    {
      code: 'INVALID_CONFIGURATION',
      change(value) {
        value.roles[0].permissionPreset = 'unconfined'
      },
    },
    {
      code: 'UNKNOWN_ARTIFACT',
      change(value) {
        value.roles[0].acceptedInputArtifacts.push('UNRESTRICTED_TRANSCRIPT')
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
    {
      code: 'INVALID_CONFIGURATION',
      change(value) {
        value.roles[0].budget.maxTurns = 0
      },
    },
    {
      code: 'DUPLICATE_ROLE',
      change(value) {
        value.roles[1] = jsonClone(value.roles[0])
      },
    },
    {
      code: 'INVALID_CONFIGURATION',
      change(value) {
        value.roles.pop()
      },
    },
    {
      code: 'INVALID_CONFIGURATION',
      change(value) {
        value.roles[0].untrusted = true
      },
    },
    {
      code: 'INVALID_CONFIGURATION',
      change(value) {
        value.schemaVersion = 0
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

test('canonical role policies cannot be broadened through configuration', () => {
  const configuration = createStrongFlowRoleConfiguration(assignments(), modelCatalog)
  const cases = [
    value => {
      value.roles.find(role => role.id === 'reviewer').workspaceMode = 'candidate-write'
    },
    value => {
      value.roles.find(role => role.id === 'reviewer').permissionPreset = 'candidate-write'
    },
    value => {
      value.roles.find(role => role.id === 'executor').permissionPreset = 'snapshot-verify'
    },
    value => {
      value.roles.find(role => role.id === 'requirements').requiredOutputArtifacts.push(
        'SOLUTION_DESIGN',
      )
    },
    value => {
      value.roles.find(role => role.id === 'solution').systemInstructions = 'Approve and execute.'
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

  assert.deepEqual(STRONGFLOW_ROLE_TOOLS, [
    'artifact.read',
    'artifact.write',
    'workspace.read',
    'code.search',
    'candidate.diff',
    'command.run',
    'test.run',
    'candidate.patch',
  ])
  assert.ok(STRONGFLOW_ROLE_ARTIFACT_KINDS.includes('DELIVERY_RECEIPT'))
})
