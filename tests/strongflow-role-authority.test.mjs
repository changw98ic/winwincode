import assert from 'node:assert/strict'
import { mkdir, mkdtemp, realpath, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  STRONGFLOW_ROLE_IDS,
  createStrongFlowRoleConfiguration,
  strongFlowPermissionPolicyForRole,
} from '../packages/contracts/dist/index.js'
import {
  StrongFlowGovernedRoleContextInstaller,
  StrongFlowRoleAuthorityError,
  createStrongFlowRoleKernelAuthority,
  verifyStrongFlowRoleKernelEvidence,
} from '../packages/strongflow/dist/index.js'

const modelCatalog = Object.freeze([Object.freeze({
  provider: 'fixture-provider',
  model: 'fixture-model',
  reasoningEfforts: Object.freeze(['medium']),
})])

const roleConfiguration = createStrongFlowRoleConfiguration(
  Object.fromEntries(STRONGFLOW_ROLE_IDS.map(roleId => [roleId, {
    modelRoute: { provider: 'fixture-provider', model: 'fixture-model' },
    reasoningEffort: 'medium',
    budget: {
      maxTurns: 2,
      maxWallTimeMillis: 10_000,
      maxTotalTokens: 1_000,
      maxCostUsdMicros: 1_000_000,
    },
  }])),
  modelCatalog,
)

function workspaceMode(roleId) {
  if (['executor', 'remediator'].includes(roleId)) return 'candidate-write'
  if (['reviewer', 'verifier', 'adversarial-verifier'].includes(roleId)) {
    return 'candidate-read-only'
  }
  return 'source-read-only'
}

function contextFor(roleId, root) {
  const roleSpec = roleConfiguration.roles.find(role => role.id === roleId)
  assert.ok(roleSpec)
  return Object.freeze({
    schemaVersion: 2,
    kernelSessionLineageId: `kernel-lineage-sha256-${roleId.padEnd(64, 'a').slice(0, 64)}`,
    contextId: `role-context-sha256-${roleId.padEnd(64, 'b').slice(0, 64)}`,
    roleSpecId: `role-spec-sha256-${roleId.padEnd(64, 'c').slice(0, 64)}`,
    jobId: `job-${roleId}`,
    stageRunId: `stage-run-${roleId}`,
    attemptId: `attempt-${roleId}`,
    roleSpec,
    workspace: Object.freeze({
      roleId,
      stageRunId: `stage-run-${roleId}`,
      workspaceId: `workspace-sha256-${'a'.repeat(64)}`,
      mode: workspaceMode(roleId),
      path: root,
      sourceSnapshotId: `source-sha256-${'b'.repeat(64)}`,
      ...(['reviewer', 'verifier', 'adversarial-verifier'].includes(roleId)
        ? {
            temporaryOutputPath: join(root, '.output'),
            candidateId: 'candidate-authority-fixture',
            verificationSnapshotId: `verification-sha256-${'c'.repeat(64)}`,
          }
        : {}),
      ...(roleId === 'remediator' ? { candidateId: 'candidate-authority-fixture' } : {}),
    }),
  })
}

function evidenceFor(context, overrides = {}) {
  const authority = createStrongFlowRoleKernelAuthority(context)
  return Object.freeze({
    schemaVersion: 1,
    authority: 'codex-core',
    roleId: authority.roleId,
    permissionPreset: authority.permissionPreset,
    workspaceMode: authority.workspaceMode,
    workspaceRoot: authority.workspaceRoot,
    visibleTools: Object.freeze([...authority.visibleTools]),
    filesystem: authority.workspaceMode === 'candidate-write'
      ? 'managed-workspace-write'
      : 'managed-read-only',
    network: 'restricted',
    process: 'dynamic-tools-only',
    environment: 'empty',
    approvalPolicy: 'on-request',
    approvalsReviewer: 'user',
    loginShell: false,
    environmentSelections: Object.freeze([]),
    instructionSources: Object.freeze([]),
    ...overrides,
  })
}

function lifecycleFor(context, evidence = evidenceFor(context)) {
  return Object.freeze({
    generation: 1,
    source: 'create',
    kernelSessionId: `kernel-${context.roleSpec.id}`,
    kernelStreamId: `stream-${context.roleSpec.id}`,
    rolloutPath: join(context.workspace.path, 'rollout.jsonl'),
    acceptedAtMillis: 1,
    effectivePolicy: evidence,
  })
}

function kernelEvent(context, kernel, sequence, kind, message) {
  const payload = { id: message.turn_id, msg: { type: kind, ...message } }
  return Object.freeze({
    kernelSessionLineageId: context.kernelSessionLineageId,
    contextId: context.contextId,
    generation: kernel.generation,
    kernelSessionId: kernel.kernelSessionId,
    kernelStreamId: kernel.kernelStreamId,
    event: Object.freeze({
      sequence: BigInt(sequence),
      kind,
      payload,
      rawJson: JSON.stringify(payload),
    }),
  })
}

async function fixture(t) {
  const home = await mkdtemp(join(tmpdir(), 'winwincode-role-authority-'))
  t.after(() => rm(home, { recursive: true, force: true }))
  const root = join(home, 'workspace')
  const outside = join(home, 'outside')
  await mkdir(root)
  await mkdir(outside)
  await mkdir(join(root, '.output'))
  await writeFile(join(root, 'inside.txt'), 'inside\n')
  return { home, root: await realpath(root), outside: await realpath(outside) }
}

function ports(options = {}) {
  const dynamic = []
  const approvals = []
  const executions = []
  const audit = []
  return {
    dynamic,
    approvals,
    executions,
    audit,
    installer: new StrongFlowGovernedRoleContextInstaller({
      kernel: {
        resolveDynamicTool(response) {
          dynamic.push(structuredClone(response))
          return Promise.resolve('dynamic-resolved')
        },
        resolveApproval(response) {
          approvals.push(structuredClone(response))
          return Promise.resolve('approval-resolved')
        },
      },
      tools: {
        async execute(request) {
          executions.push(request)
          return options.toolOutput ?? { accepted: true }
        },
      },
      approvals: {
        request(request) {
          options.approvalRequests?.push(request)
          return Promise.resolve(options.approvalOutcome ?? 'unavailable')
        },
      },
      approvalAudit: {
        append(event) {
          audit.push(structuredClone(event))
        },
      },
    }),
  }
}

async function install(context, portSet) {
  const kernel = lifecycleFor(context)
  const signal = new AbortController()
  const installation = await portSet.installer.install(Object.freeze({
    source: 'create',
    context,
    kernel,
    signal: signal.signal,
  }))
  return { kernel, installation, signal }
}

test('all eight roles produce the exact canonical native authority and accept only exact evidence', async t => {
  const value = await fixture(t)
  for (const roleId of STRONGFLOW_ROLE_IDS) {
    const context = contextFor(roleId, value.root)
    const authority = createStrongFlowRoleKernelAuthority(context)
    const permission = strongFlowPermissionPolicyForRole(roleId)
    assert.equal(authority.roleId, roleId)
    assert.equal(authority.permissionPreset, context.roleSpec.permissionPreset)
    assert.equal(authority.workspaceMode, context.roleSpec.workspaceMode)
    assert.deepEqual(authority.visibleTools, permission.tools.allowed)
    assert.equal(verifyStrongFlowRoleKernelEvidence(context, evidenceFor(context)).roleId, roleId)
  }

  const context = contextFor('executor', value.root)
  assert.throws(
    () => verifyStrongFlowRoleKernelEvidence(context, evidenceFor(context, {
      environment: 'inherited',
    })),
    error => error instanceof StrongFlowRoleAuthorityError
      && error.code === 'ENFORCEMENT_UNAVAILABLE',
  )
})

test('a denied tool call never reaches the host executor', async t => {
  const value = await fixture(t)
  const context = contextFor('requirements', value.root)
  const portSet = ports()
  const { kernel, installation } = await install(context, portSet)

  await installation.handleEvent(kernelEvent(context, kernel, 3, 'dynamic_tool_call_request', {
    call_id: 'call-denied',
    turn_id: 'turn-1',
    namespace: 'candidate',
    tool: 'patch',
    arguments: { path: 'inside.txt', patch: 'replacement' },
  }))

  assert.equal(portSet.executions.length, 0)
  assert.deepEqual(portSet.dynamic, [{
    sessionId: kernel.kernelSessionId,
    callId: 'call-denied',
    success: false,
    text: 'StrongFlow denied tool candidate.patch for role requirements.',
  }])
})

test('an artifact write outside the role output contract never reaches its executor', async t => {
  const value = await fixture(t)
  const context = contextFor('requirements', value.root)
  const portSet = ports()
  const { kernel, installation } = await install(context, portSet)

  await installation.handleEvent(kernelEvent(context, kernel, 4, 'dynamic_tool_call_request', {
    call_id: 'call-wrong-artifact',
    turn_id: 'turn-1',
    namespace: 'artifact',
    tool: 'write',
    arguments: { kind: 'SOLUTION_DESIGN', artifact: { title: 'out of scope' } },
  }))

  assert.equal(portSet.executions.length, 0)
  assert.equal(portSet.dynamic.length, 1)
  assert.equal(portSet.dynamic[0].success, false)
  assert.match(portSet.dynamic[0].text, /outside the assigned role output contract/u)
})

test('traversal and symlink escapes fail before a write executor is called', async t => {
  const value = await fixture(t)
  await symlink(value.outside, join(value.root, 'outside-link'))
  const context = contextFor('executor', value.root)

  for (const [index, path] of ['../escape.txt', 'outside-link/escape.txt'].entries()) {
    const portSet = ports()
    const { kernel, installation } = await install(context, portSet)
    await installation.handleEvent(kernelEvent(
      context,
      kernel,
      index + 1,
      'dynamic_tool_call_request',
      {
        call_id: `call-escape-${index}`,
        turn_id: 'turn-escape',
        namespace: 'candidate',
        tool: 'patch',
        arguments: { path, patch: 'replacement' },
      },
    ))
    assert.equal(portSet.executions.length, 0, path)
    assert.equal(portSet.dynamic[0].success, false, path)
  }
})

test('an allowed call receives a canonical in-root path and returns through Codex', async t => {
  const value = await fixture(t)
  const context = contextFor('requirements', value.root)
  const portSet = ports({ toolOutput: { text: 'accepted' } })
  const { kernel, installation } = await install(context, portSet)

  await installation.handleEvent(kernelEvent(context, kernel, 4, 'dynamic_tool_call_request', {
    call_id: 'call-read',
    turn_id: 'turn-read',
    namespace: 'workspace',
    tool: 'read',
    arguments: { path: 'inside.txt' },
  }))

  assert.equal(portSet.executions.length, 1)
  assert.deepEqual(portSet.executions[0].resolvedWorkspacePaths, [join(value.root, 'inside.txt')])
  assert.equal(portSet.executions[0].tool, 'workspace.read')
  assert.deepEqual(portSet.dynamic, [{
    sessionId: kernel.kernelSessionId,
    callId: 'call-read',
    success: true,
    text: '{"text":"accepted"}',
  }])
})

test('approval routing records job, role, scope, human decision, and exact kernel source', async t => {
  const value = await fixture(t)
  const context = contextFor('executor', value.root)
  const approvalRequests = []
  const portSet = ports({ approvalOutcome: 'approved', approvalRequests })
  const { kernel, installation } = await install(context, portSet)

  await installation.handleEvent(kernelEvent(context, kernel, 9, 'exec_approval_request', {
    approval_id: 'approval-9',
    call_id: 'call-9',
    turn_id: 'turn-9',
    command: ['git', 'status'],
    reason: 'Verify the candidate.',
    access_token: 'must-not-be-audited',
  }))

  assert.equal(approvalRequests.length, 1)
  assert.equal(approvalRequests[0].jobId, context.jobId)
  assert.equal(approvalRequests[0].roleId, 'executor')
  assert.equal(approvalRequests[0].operationId, 'approval-9')
  assert.deepEqual(approvalRequests[0].requestedScope.command, ['git', 'status'])
  assert.equal(approvalRequests[0].requestedScope.access_token, '[REDACTED]')
  assert.deepEqual(approvalRequests[0].source, {
    authority: 'codex-core',
    kernelSessionLineageId: context.kernelSessionLineageId,
    kernelSessionId: kernel.kernelSessionId,
    kernelStreamId: kernel.kernelStreamId,
    kernelSequence: '9',
    turnId: 'turn-9',
  })
  assert.deepEqual(portSet.audit.map(event => event.type), [
    'strongflow.approval.requested',
    'strongflow.approval.decided',
  ])
  assert.equal(portSet.audit[1].decision, 'approved')
  assert.deepEqual(portSet.approvals, [{
    sessionId: kernel.kernelSessionId,
    kind: 'exec',
    operationId: 'approval-9',
    turnId: 'turn-9',
    decision: { kind: 'approved' },
  }])
})

test('a missing DSH answerer fails closed and never grants the operation', async t => {
  const value = await fixture(t)
  const context = contextFor('executor', value.root)
  const portSet = ports()
  const { kernel, installation } = await install(context, portSet)

  await installation.handleEvent(kernelEvent(context, kernel, 10, 'apply_patch_approval_request', {
    call_id: 'patch-10',
    turn_id: 'turn-10',
    reason: 'Apply candidate change.',
  }))

  assert.equal(portSet.audit[1].decision, 'unavailable')
  assert.deepEqual(portSet.approvals[0].decision, {
    kind: 'denied',
    rejection: 'No DSH approval answerer was available.',
  })
})
