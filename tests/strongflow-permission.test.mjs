import assert from 'node:assert/strict'
import test from 'node:test'

import {
  STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION,
  STRONGFLOW_PERMISSION_PRESET_IDS,
  STRONGFLOW_PERMISSION_SUPPORTED_HOSTS,
  STRONGFLOW_PRIVILEGED_OPERATION_KINDS,
  STRONGFLOW_ROLE_IDS,
  STRONGFLOW_ROLE_TOOLS,
  StrongFlowPermissionPolicyError,
  parseStrongFlowPermissionPolicy,
  resolveStrongFlowPermissionPolicy,
  strongFlowDeterministicFinalizerPermissionPolicy,
  strongFlowHumanReviewerPermissionPolicy,
  strongFlowPermissionPolicyForPreset,
  strongFlowPermissionPolicyForRole,
} from '../packages/contracts/dist/index.js'

const EXPECTED_ROLE_PRESETS = Object.freeze({
  requirements: 'definition-read',
  solution: 'solution-read',
  planner: 'source-read',
  executor: 'candidate-write',
  reviewer: 'snapshot-verify',
  verifier: 'snapshot-verify',
  'adversarial-verifier': 'snapshot-verify',
  remediator: 'remediation-write',
})

function jsonClone(value) {
  return JSON.parse(JSON.stringify(value))
}

function expectedPolicyError(code) {
  return error => error instanceof StrongFlowPermissionPolicyError && error.code === code
}

function fullEnforcement(host) {
  const [platform, architecture] = host.split('/')
  return {
    schemaVersion: STRONGFLOW_PERMISSION_POLICY_SCHEMA_VERSION,
    platform,
    architecture,
    filesystem: 'codex-restricted',
    process: 'codex-sandboxed',
    network: 'codex-restricted',
    environment: 'explicit-allowlist',
    approvals: 'source-identified-human',
    credentials: 'dsh-reference-only',
    publication: 'exact-identity-guard',
    audit: 'durable-redacted',
  }
}

test('every model role resolves to one complete immutable least-authority preset', () => {
  assert.deepEqual(Object.keys(EXPECTED_ROLE_PRESETS), STRONGFLOW_ROLE_IDS)
  assert.ok(Object.isFrozen(STRONGFLOW_PERMISSION_PRESET_IDS))
  assert.ok(Object.isFrozen(STRONGFLOW_PRIVILEGED_OPERATION_KINDS))
  assert.ok(Object.isFrozen(STRONGFLOW_ROLE_TOOLS))

  for (const roleId of STRONGFLOW_ROLE_IDS) {
    const policy = strongFlowPermissionPolicyForRole(roleId)
    const writer = roleId === 'executor' || roleId === 'remediator'
    assert.equal(policy.presetId, EXPECTED_ROLE_PRESETS[roleId])
    assert.equal(policy.subject, 'model-role')
    assert.equal(policy.filesystem.mode, writer ? 'candidate-write' : 'read-only')
    assert.equal(policy.filesystem.rootScope, 'assigned-workspace')
    assert.equal(policy.filesystem.symlinkEscape, 'deny')
    assert.equal(policy.tools.allowed.includes('candidate.patch'), writer)
    assert.equal(policy.tools.approvalTool, 'absent')
    assert.equal(policy.network.access, 'disabled')
    assert.equal(policy.network.requestElevation, 'human-operation-approval')
    assert.equal(policy.approval.definitionDecision, 'forbidden')
    assert.deepEqual(
      policy.approval.operationRequests,
      STRONGFLOW_PRIVILEGED_OPERATION_KINDS,
    )
    assert.deepEqual(policy.approval.operationDecisions, [])
    assert.equal(policy.approval.selfApproval, 'forbidden')
    assert.equal(policy.budget.consume, 'assigned-role-budget')
    assert.equal(policy.budget.requestIncrease, 'human-operation-approval')
    assert.equal(policy.budget.decideIncrease, 'forbidden')
    assert.equal(policy.publication.remoteExecute, 'forbidden')
    assert.equal(policy.publication.requestRemote, 'human-operation-approval')
    assert.equal(policy.publication.decideRemote, 'forbidden')
    assert.equal(policy.credentials.use, 'dsh-selected-model-reference')
    assert.equal(policy.credentials.rawValues, 'forbidden')
    assert.equal(policy.credentials.environment, 'excluded')
    assert.equal(policy.credentials.decideAdditional, 'forbidden')
    assert.equal(policy.audit.required, true)
    assert.equal(policy.audit.credentialRedaction, 'required')
    assert.ok(Object.isFrozen(policy))
    assert.ok(Object.isFrozen(policy.filesystem))
    assert.ok(Object.isFrozen(policy.tools))
    assert.ok(Object.isFrozen(policy.tools.allowed))
    assert.ok(Object.isFrozen(policy.approval.operationRequests))
  }

  assert.equal(
    strongFlowPermissionPolicyForRole('executor').process.mode,
    'approved-plan-commands',
  )
  assert.equal(
    strongFlowPermissionPolicyForRole('remediator').process.mode,
    'approved-remediation-commands',
  )
  for (const roleId of ['reviewer', 'verifier', 'adversarial-verifier']) {
    assert.equal(
      strongFlowPermissionPolicyForRole(roleId).process.mode,
      'approved-snapshot-probes',
    )
  }
  for (const roleId of ['requirements', 'solution', 'planner']) {
    assert.equal(strongFlowPermissionPolicyForRole(roleId).process.mode, 'disabled')
  }

  const serialized = JSON.stringify(STRONGFLOW_PERMISSION_PRESET_IDS.map(
    strongFlowPermissionPolicyForPreset,
  ))
  assert.equal(serialized.includes('danger-full-access'), false)
})

test('human definition review, operation decisions, and deterministic finalization stay separate', () => {
  const human = strongFlowHumanReviewerPermissionPolicy()
  assert.equal(human.subject, 'human-reviewer')
  assert.equal(human.filesystem.mode, 'none')
  assert.deepEqual(human.tools.allowed, [])
  assert.equal(human.process.mode, 'disabled')
  assert.equal(human.approval.definitionDecision, 'human-reviewer-only')
  assert.deepEqual(human.approval.operationRequests, [])
  assert.deepEqual(
    human.approval.operationDecisions,
    STRONGFLOW_PRIVILEGED_OPERATION_KINDS,
  )
  assert.equal(human.budget.decideIncrease, 'human-reviewer-only')
  assert.equal(human.publication.local, 'human-review-record-only')
  assert.equal(human.publication.remoteExecute, 'forbidden')
  assert.equal(human.publication.decideRemote, 'human-reviewer-only')
  assert.equal(human.credentials.decideAdditional, 'human-reviewer-only')

  const finalizer = strongFlowDeterministicFinalizerPermissionPolicy()
  assert.equal(finalizer.subject, 'deterministic-finalizer')
  assert.equal(finalizer.approval.definitionDecision, 'forbidden')
  assert.deepEqual(finalizer.approval.operationDecisions, [])
  assert.equal(finalizer.publication.local, 'delivery-receipt-only')
  assert.equal(finalizer.publication.remoteExecute, 'human-approved-finalizer-only')
  assert.equal(finalizer.credentials.use, 'none')
  assert.equal(finalizer.budget.consume, 'none')

  assert.notEqual(human, finalizer)
  assert.equal(
    strongFlowPermissionPolicyForPreset('human-definition-review'),
    human,
  )
  assert.equal(
    strongFlowPermissionPolicyForPreset('deterministic-finalizer'),
    finalizer,
  )
})

test('missing, unknown, or broadened policy fields fail closed', () => {
  const canonical = strongFlowPermissionPolicyForPreset('candidate-write')
  assert.equal(parseStrongFlowPermissionPolicy(jsonClone(canonical)), canonical)

  for (const key of Object.keys(canonical)) {
    const value = jsonClone(canonical)
    delete value[key]
    assert.throws(
      () => parseStrongFlowPermissionPolicy(value),
      expectedPolicyError('INVALID_POLICY'),
      `missing ${key}`,
    )
  }

  const missingNested = jsonClone(canonical)
  delete missingNested.credentials.rawValues
  assert.throws(
    () => parseStrongFlowPermissionPolicy(missingNested),
    expectedPolicyError('INVALID_POLICY'),
  )

  const extra = jsonClone(canonical)
  extra.unrestricted = true
  assert.throws(
    () => parseStrongFlowPermissionPolicy(extra),
    expectedPolicyError('INVALID_POLICY'),
  )

  const unknown = jsonClone(canonical)
  unknown.presetId = 'unknown-preset'
  assert.throws(
    () => parseStrongFlowPermissionPolicy(unknown),
    expectedPolicyError('UNKNOWN_PRESET'),
  )

  const oldVersion = jsonClone(canonical)
  oldVersion.schemaVersion = 0
  assert.throws(
    () => parseStrongFlowPermissionPolicy(oldVersion),
    expectedPolicyError('INVALID_POLICY'),
  )

  const broadenings = [
    value => { value.filesystem.mode = 'danger-full-access' },
    value => { value.filesystem.rootScope = 'host' },
    value => { value.tools.allowed.push('human-approval') },
    value => { value.process.mode = 'unrestricted' },
    value => { value.network.access = 'enabled' },
    value => { value.approval.definitionDecision = 'model' },
    value => { value.approval.operationDecisions.push('permission-expansion') },
    value => { value.budget.decideIncrease = 'model' },
    value => { value.publication.remoteExecute = 'allowed' },
    value => { value.credentials.rawValues = 'allowed' },
    value => { value.audit.credentialRedaction = 'optional' },
  ]
  for (const broaden of broadenings) {
    const value = jsonClone(canonical)
    broaden(value)
    assert.throws(
      () => parseStrongFlowPermissionPolicy(value),
      expectedPolicyError('POLICY_MISMATCH'),
    )
  }
})

test('only fully enforced macOS and Linux profiles resolve before actor startup', () => {
  assert.deepEqual(STRONGFLOW_PERMISSION_SUPPORTED_HOSTS, [
    'darwin/arm64',
    'darwin/x64',
    'linux/arm64',
    'linux/x64',
  ])
  const policy = strongFlowPermissionPolicyForRole('executor')
  for (const host of STRONGFLOW_PERMISSION_SUPPORTED_HOSTS) {
    const resolved = resolveStrongFlowPermissionPolicy(
      jsonClone(policy),
      fullEnforcement(host),
    )
    assert.equal(resolved.policy, policy)
    assert.equal(
      `${resolved.enforcement.platform}/${resolved.enforcement.architecture}`,
      host,
    )
    assert.ok(Object.isFrozen(resolved))
    assert.ok(Object.isFrozen(resolved.enforcement))
  }

  for (const [platform, architecture] of [['win32', 'x64'], ['linux', 'riscv64']]) {
    const profile = fullEnforcement('linux/x64')
    profile.platform = platform
    profile.architecture = architecture
    assert.throws(
      () => resolveStrongFlowPermissionPolicy(policy, profile),
      expectedPolicyError('UNSUPPORTED_PLATFORM'),
    )
  }

  const enforcementFields = [
    'filesystem',
    'process',
    'network',
    'environment',
    'approvals',
    'credentials',
    'publication',
    'audit',
  ]
  for (const field of enforcementFields) {
    const unavailable = fullEnforcement('darwin/arm64')
    unavailable[field] = 'partial'
    assert.throws(
      () => resolveStrongFlowPermissionPolicy(policy, unavailable),
      expectedPolicyError('ENFORCEMENT_UNAVAILABLE'),
      field,
    )
  }

  const incomplete = fullEnforcement('linux/x64')
  delete incomplete.audit
  assert.throws(
    () => resolveStrongFlowPermissionPolicy(policy, incomplete),
    expectedPolicyError('ENFORCEMENT_UNAVAILABLE'),
  )
})
