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
const rulesPath = join(root, 'docs', 'contracts', 'delivery-stage-coordination.rules.json')
const documentationPath = join(root, 'docs', 'contracts', 'delivery-stage-coordination.md')
const decisionPath = join(root, 'docs', 'decisions', '0028-control-plane-worker-migration.md')
const ownershipDecisionPath = join(root, 'docs', 'decisions', '0023-canonical-delivery-ownership.md')
const targetGraphPath = join(
  root,
  'docs',
  'decisions',
  '0028-control-plane-worker-target-graph.json',
)
const httpSchemaPath = join(root, 'schema', 'winwincode', 'v1', 'control-plane-http.schema.json')
const executionPortSchemaPath = join(
  root,
  'schema',
  'winwincode',
  'v1',
  'execution-port.schema.json',
)

const expectedStartTransitions = Object.freeze([
  ['draft', null, 'clarifying', 'clarifying'],
  ['clarifying', null, 'clarifying', 'clarifying'],
  ['ready', null, 'planning', 'planning'],
  ['planning', null, 'planning', 'planning'],
  ['planning', 'planning', 'plan-review', 'needs-attention'],
  ['executing', null, 'executing', 'executing'],
  ['executing', 'executing', 'verifying', 'verifying'],
  ['verifying', null, 'verifying', 'verifying'],
  ['verifying', 'verifying', 'verifying', 'verifying'],
  ['reworking', null, 'reworking', 'reworking'],
  ['reworking', 'reworking', 'verifying', 'verifying'],
  ['ready-to-deliver', null, 'delivery-review', 'needs-attention'],
].map(([deliveryStatus, activeStage, nextStage, nextDeliveryStatus]) => ({
  deliveryStatus,
  activeStage,
  nextStage,
  nextDeliveryStatus,
})))

const expectedTaskStatusTransitions = Object.freeze([
  ['pending', 'start-executing', 'active'],
  ['active', 'start-verifying', 'verifying'],
  ['verifying', 'verification-pass', 'completed'],
  ['verifying', 'verification-fail', 'failed'],
  ['failed', 'start-reworking', 'active'],
  ['active', 'executing-cancelled', 'pending'],
  ['verifying', 'verifying-cancelled', 'verifying'],
  ['active', 'reworking-cancelled', 'failed'],
].map(([from, event, to]) => ({ from, event, to })))

const expectedDeliveryTests = Object.freeze([
  'advance_selects_only_the_legal_next_stage',
  'advance_rejects_when_more_than_one_stage_run_is_active',
  'stage_actor_and_role_policy_rejects_wrong_executor',
  'starting_next_stage_settles_bound_previous_run_atomically',
  'review_stage_opens_linked_blocking_attention_atomically',
  'active_stage_run_resumes_without_new_run_or_attempt',
  'cancel_request_waits_for_terminal_job_outcome',
  'cancelled_outcome_settles_the_same_stage_run',
  'task_breakdown_approval_replaces_empty_graph_once',
  'task_breakdown_rejects_missing_self_and_cyclic_dependencies',
  'blocked_task_never_becomes_runnable',
  'task_status_tracks_execution_verification_rework_and_cancel',
  'session_binding_matches_exact_delivery_task_stage_job_and_session_identities',
  'conflicting_session_binding_stops_recovery',
  'open_blocking_attention_prevents_stage_advance',
  'stale_attention_resolution_is_rejected_without_state_change',
  'resolving_one_of_multiple_blockers_keeps_delivery_blocked',
  'replayed_advance_returns_original_stage_run_without_new_state',
])

const expectedControlPlaneTests = Object.freeze([
  'delivery_advance_dispatches_one_execution_job_after_commit',
  'replayed_delivery_advance_does_not_dispatch_a_second_execution_job',
  'delivery_stage_scope_carries_exact_product_delivery_task_and_run_identity',
  'job_cancel_ack_does_not_settle_stage_before_terminal_outcome',
  'delivery_dispatch_does_not_persist_codex_plan_agent_or_tool_state',
])

const expectedRuleIds = Object.freeze([
  'attention.blocks_advance',
  'attention.current_resolution',
  'execution.execution_job_only',
  'idempotency.advance_replay',
  'session.exact_binding',
  'stage.actor_role',
  'stage.atomic_handoff',
  'stage.cancel_terminal_outcome',
  'stage.legal_next',
  'stage.resume_identity',
  'stage.single_active',
  'task.blocked_dependency',
  'task.breakdown_approval',
  'task.dag',
  'task.status_transition',
].sort())

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function repositoryPath(path) {
  assert.equal(path.startsWith('/'), false, `${path} must be repository-relative`)
  assert.equal(path.split('/').includes('..'), false, `${path} must not escape the repository`)
  return join(root, path)
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

function assertExportedSymbol(mapping) {
  const source = readFileSync(repositoryPath(mapping.path), 'utf8')
  assert.match(
    source,
    new RegExp(
      `export\\s+(?:(?:async\\s+)?function|const|type|class|interface)\\s+${mapping.name}\\b`,
      'u',
    ),
    `${mapping.path} does not export ${mapping.name}`,
  )
}

function assertNamedNodeTest(mapping) {
  const source = readFileSync(repositoryPath(mapping.path), 'utf8')
  assert.equal(
    source.includes(`test('${mapping.name}'`) || source.includes(`test("${mapping.name}"`),
    true,
    `${mapping.path} does not define test ${mapping.name}`,
  )
}

function commandFailure(result) {
  return [result.stdout, result.stderr, result.error?.stack]
    .filter(Boolean)
    .join('\n')
}

function packageMatchesPattern(packageName, pattern) {
  if (pattern.endsWith('*')) return packageName.startsWith(pattern.slice(0, -1))
  return packageName === pattern
}

test('phase 2.3 freezes Delivery stage coordination without claiming implementation', () => {
  const rules = json(rulesPath)
  assert.deepEqual({
    schemaVersion: rules.schemaVersion,
    status: rules.status,
    phaseTask: rules.phaseTask,
    decision: rules.decision,
    ownershipDecision: rules.ownershipDecision,
    documentation: rules.documentation,
    implementationCompletionSource: rules.implementationCompletionSource,
  }, {
    schemaVersion: 1,
    status: 'required-contract-not-implementation-proof',
    phaseTask: 'winwincode-9c4.16.2.3',
    decision: relative(root, decisionPath),
    ownershipDecision: relative(root, ownershipDecisionPath),
    documentation: relative(root, documentationPath),
    implementationCompletionSource: 'rust-black-box-tests-and-beads',
  })

  assert.deepEqual(rules.ownership, {
    canonicalDeliveryWriter: 'winwincode-control-plane',
    deliveryStateMachineOwner: 'winwincode-delivery',
    executionCommandBoundary: 'ExecutionPort',
    executionCommandType: 'ExecutionJob',
    executionFactOwner: 'codex-core',
    workerMayWriteDeliveryState: false,
    controlPlaneMayOwnCodexPlanAgentOrToolState: false,
    websocketMaySubmitBusinessMutations: false,
  })
})

test('the legal next-stage table has one active StageRun and explicit actor/role rules', () => {
  const protocol = json(rulesPath).stageProtocol
  assert.deepEqual(protocol.activeStatuses, ['running', 'waiting'])
  assert.deepEqual(protocol.terminalStatuses, ['succeeded', 'failed', 'cancelled'])
  assert.equal(protocol.maximumActiveStageRunsPerDelivery, 1)
  assert.equal(protocol.callerSuppliesAttempt, false)
  assert.deepEqual(protocol.startTransitions, expectedStartTransitions)
  assert.deepEqual(protocol.actorRolePolicy, {
    clarifying: { actor: 'codex', roles: ['requirements'] },
    planning: { actor: 'codex', roles: ['planner'] },
    'plan-review': { actor: 'human', roles: ['reviewer'] },
    executing: { actor: 'codex', roles: ['executor'] },
    verifying: {
      actor: 'codex',
      roles: ['reviewer', 'verifier', 'adversarial-verifier'],
    },
    reworking: { actor: 'codex', roles: ['remediator'] },
    'delivery-review': { actor: 'human', roles: ['approver'] },
  })
  assert.deepEqual(protocol.atomicStart, [
    'validate-current-revision-and-request',
    'validate-one-legal-next-stage',
    'settle-bound-previous-stage-run-when-handing-off',
    'append-new-stage-run',
    'update-delivery-and-task-status',
    'append-linked-review-attention-when-required',
    'append-execution-job-intent-for-codex-stage',
    'commit-state-and-outbox',
  ])
})

test('resume and cancel retain one StageRun identity and wait for the terminal Worker outcome', () => {
  const lifecycle = json(rulesPath).stageRunLifecycle
  assert.deepEqual(lifecycle.resume, {
    reuseStageRunId: true,
    reuseAttempt: true,
    reuseExecutionJobId: true,
    reuseAcceptedSessionBinding: true,
    createSecondRunAllowed: false,
    finalModelOutputAcceptedWithoutOutcomeReview: false,
  })
  assert.deepEqual(lifecycle.cancel, {
    appliesTo: 'active-codex-stage-run',
    command: 'job.cancel',
    acknowledgementIsTerminal: false,
    terminalFact: 'leased-job.outcome.cancelled',
    onTerminalFact: [
      'settle-same-stage-run-as-cancelled',
      'set-finished-at',
      'release-active-stage-slot',
      'restore-task-to-stage-retry-status',
    ],
    createReplacementRunAutomatically: false,
  })
})

test('approved DeliveryTask graph is one durable product graph with fixed status changes', () => {
  const taskProtocol = json(rulesPath).taskProtocol
  assert.deepEqual(taskProtocol.breakdownApproval, {
    command: 'delivery.approve_task_breakdown',
    createCommandTaskListMustBeEmpty: true,
    requiresCurrentPlanApproval: true,
    writesGraphOncePerSpecRevision: true,
    laterReplacementRequiresNewSpecRevisionAndReview: true,
    codexPlanImportedAsDeliveryTasks: false,
  })
  assert.deepEqual(taskProtocol.graphRules, {
    minimumApprovedTasks: 1,
    acceptanceCriteriaMustBelongToCurrentSpec: true,
    dependenciesMustExistInSameDelivery: true,
    selfDependencyAllowed: false,
    cyclesAllowed: false,
  })
  assert.deepEqual(taskProtocol.runnableRule, {
    dependencyStatus: 'completed',
    blockedTaskMayStart: false,
    schedulerChoosesAmongMultipleRunnableTasks: false,
  })
  assert.deepEqual(taskProtocol.statusTransitions, expectedTaskStatusTransitions)
})

test('SessionBinding and Attention fail closed on incomplete or stale identity', () => {
  const rules = json(rulesPath)
  assert.deepEqual(rules.sessionBinding, {
    exactIdentityFields: [
      'deliveryId',
      'deliveryTaskId',
      'stageRunId',
      'productSessionId',
      'executionJobId',
      'workerSessionId',
      'codexThreadId',
    ],
    deliveryTaskIdNullableOnlyForDeliveryLevelStage: true,
    workerSessionIdNullableUntilDispatchAccepted: true,
    codexThreadIdNullableUntilWorkerReportsThread: true,
    genericSessionIdAllowed: false,
    conflictingBindingResult: 'fail-closed-without-selecting-next-action',
  })
  assert.deepEqual(rules.attentionProtocol, {
    openBlockingItemStopsAdvanceAndDispatch: true,
    reviewStageCreatesLinkedOpenBlockerAtomically: true,
    resolutionRequiresCurrentRevisionActorItemAndStageRun: true,
    resolvingOneOfManyKeepsNeedsAttention: true,
    resolveCommandStartsExecutionDirectly: false,
    runtimeApprovalIsBusinessAttention: false,
  })
})

test('request replay cannot create or dispatch a second stage attempt', () => {
  const replay = json(rulesPath).idempotency
  assert.deepEqual(replay, {
    key: 'requestId',
    equality: ['command', 'actor', 'scope', 'expectedRevision', 'payloadDigest'],
    identicalReplay: {
      returnOriginalCommandResult: true,
      appendStageRun: false,
      appendSessionBinding: false,
      appendExecutionJob: false,
      appendOutboxEvent: false,
      dispatchExecutionJob: false,
    },
    conflictingReuse: 'IDEMPOTENCY_CONFLICT',
  })
})

test('rules remain anchored to accepted TypeScript behavior and public contracts', () => {
  const rules = json(rulesPath)
  const ruleIds = rules.rules.map(rule => rule.id)
  assert.equal(new Set(ruleIds).size, ruleIds.length)
  assert.deepEqual([...ruleIds].sort(), expectedRuleIds)

  for (const rule of rules.rules) {
    assert.match(rule.id, /^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+$/u)
    assert.ok(rule.statement.length > 0, `${rule.id} needs a statement`)
    assert.ok(rule.refs.length > 0, `${rule.id} needs a source reference`)
    for (const path of rule.refs) assert.equal(existsSync(repositoryPath(path)), true, path)
    assert.ok(
      ['covered', 'partial', 'target_only'].includes(rule.typescript.coverage),
      `${rule.id} has an unknown TypeScript coverage state`,
    )
    for (const mapping of rule.typescript.publicSymbols) assertExportedSymbol(mapping)
    for (const mapping of rule.typescript.tests) assertNamedNodeTest(mapping)
    if (rule.typescript.coverage !== 'covered') {
      assert.ok(rule.typescript.gap.length > 0, `${rule.id} must explain the baseline gap`)
    }
    assert.ok(
      rule.rust.module.startsWith('crates/winwincode-delivery/')
        || rule.rust.module.startsWith('crates/winwincode-control-plane/'),
      `${rule.id} has an unexpected target module`,
    )
    assert.match(rule.rust.testName, /^[a-z][a-z0-9_]+$/u)
  }

  const http = json(httpSchemaPath)
  assert.equal(
    http.$defs.DeliveryApproveTaskBreakdownCommand.allOf[1].properties.command.const,
    rules.taskProtocol.breakdownApproval.command,
  )
  assert.equal(
    http.$defs.DeliveryAdvanceCommand.allOf[1].properties.command.const,
    'delivery.advance',
  )
  assert.equal(
    http.$defs.DeliveryResolveAttentionCommand.allOf[1].properties.command.const,
    'delivery.resolve_attention',
  )

  const execution = json(executionPortSchemaPath)
  assert.equal(execution.$defs.JobDispatchMessage.properties.kind.const, 'job.dispatch')
  assert.equal(execution.$defs.JobCancelMessage.properties.kind.const, 'job.cancel')
  assert.ok(execution.$defs.ExecutionJob)
  assert.ok(execution.$defs.DeliveryStageExecutionScope)
})

test('accepted target graph keeps Delivery in Control Plane and Codex execution outside it', () => {
  const rules = json(rulesPath)
  const graph = json(targetGraphPath)
  const nodes = new Map(graph.nodes.map(node => [node.id, node]))
  const delivery = nodes.get(rules.ownership.deliveryStateMachineOwner)
  const controlPlane = nodes.get(rules.ownership.canonicalDeliveryWriter)
  const worker = nodes.get('winwincode-worker')

  assert.ok(delivery)
  assert.equal(delivery.zone, 'control-plane')
  assert.ok(delivery.responsibilities.includes('advance-delivery-state-machine'))
  assert.ok(controlPlane)
  assert.equal(controlPlane.zone, 'control-plane')
  assert.ok(controlPlane.responsibilities.includes('remain-the-only-product-state-writer'))
  assert.ok(controlPlane.allowedInternalDependencies.includes(delivery.id))
  assert.ok(worker)
  assert.equal(worker.zone, 'execution-worker')
  assert.equal(worker.allowedInternalDependencies.includes(delivery.id), false)
  assert.ok(graph.interfaces.some(interface_ => (
    interface_.id === 'execution-port'
    && interface_.consumers.includes(controlPlane.id)
    && interface_.consumers.includes(worker.id)
  )))
})

test('plain-language contract explains every enforced stage outcome', () => {
  const rules = json(rulesPath)
  const text = readFileSync(documentationPath, 'utf8')
  const plainText = text.replaceAll('`', '').replace(/\s+/gu, ' ')
  for (const phrase of [
    '目标门禁，不是实现完成声明',
    '一个 Delivery 同时最多只有一个活动 StageRun',
    '只创建一个 ExecutionJob',
    '不会创建第二个 StageRun',
    'job.cancel_ack 只表示 Worker 收到了取消请求',
    'delivery.approve_task_breakdown',
    'Codex Plan 不会被复制成 DeliveryTask',
    '所有依赖任务都已经 completed',
    'ProductSession、WorkerSession、CodexThread 和 StageRun',
    '开放且阻塞的 AttentionItem',
    '同一个 requestId',
    '不保存 Codex Plan、Agent Graph 或 Tool Call',
  ]) assert.ok(plainText.includes(phrase), `missing documentation phrase: ${phrase}`)

  for (const transition of expectedStartTransitions) {
    assert.ok(
      text.includes(`\`${transition.deliveryStatus}\``)
        && text.includes(`\`${transition.nextStage}\``),
      `missing transition ${transition.deliveryStatus} -> ${transition.nextStage}`,
    )
  }
  assert.equal(rules.documentation, relative(root, documentationPath))
})

test('future phase 2.3 Rust modules must satisfy the frozen black-box seam', () => {
  const rules = json(rulesPath)
  const gate = rules.rustGate
  assert.deepEqual(gate.activation, {
    condition: 'any-required-phase-2.3-module-or-test-exists',
    effect: 'require-all-modules-tests-dependency-boundary-and-generated-execution-types',
  })
  assert.deepEqual(gate.requiredModules, [
    'crates/winwincode-delivery/src/application/stage.rs',
    'crates/winwincode-delivery/src/application/task.rs',
    'crates/winwincode-delivery/src/application/session_binding.rs',
    'crates/winwincode-delivery/src/application/attention.rs',
    'crates/winwincode-control-plane/src/delivery_execution.rs',
  ])
  assert.deepEqual(gate.integrationTests, [
    {
      package: 'winwincode-delivery',
      target: 'stage_task_lifecycle',
      path: 'crates/winwincode-delivery/tests/stage_task_lifecycle.rs',
      requiredTests: expectedDeliveryTests,
    },
    {
      package: 'winwincode-control-plane',
      target: 'delivery_execution_dispatch',
      path: 'crates/winwincode-control-plane/tests/delivery_execution_dispatch.rs',
      requiredTests: expectedControlPlaneTests,
    },
  ])

  const activationPaths = [
    ...gate.requiredModules,
    ...gate.integrationTests.map(entry => entry.path),
  ].map(repositoryPath)
  if (activationPaths.every(path => !existsSync(path))) return

  for (const path of activationPaths) {
    assert.equal(existsSync(path), true, `${relative(root, path)} is required`)
  }

  const metadata = spawnSync(
    'cargo',
    ['metadata', '--format-version', '1', '--locked', '--no-deps'],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(metadata.status, 0, commandFailure(metadata))
  const packages = new Map(
    JSON.parse(metadata.stdout).packages.map(package_ => [package_.name, package_]),
  )
  const graph = json(targetGraphPath)
  const nodes = new Map(graph.nodes.map(node => [node.id, node]))
  for (const packageName of ['winwincode-delivery', 'winwincode-control-plane']) {
    const package_ = packages.get(packageName)
    const node = nodes.get(packageName)
    assert.ok(package_, `${packageName} is not a Rust workspace package`)
    assert.ok(node, `${packageName} is not in the accepted target graph`)
    for (const dependency of package_.dependencies) {
      if (!dependency.name.startsWith('winwincode-')) continue
      assert.ok(
        node.allowedInternalDependencies.includes(dependency.name),
        `${packageName} has forbidden product dependency ${dependency.name}`,
      )
    }
    for (const dependency of package_.dependencies) {
      for (const pattern of gate.forbiddenDependencyPatterns) {
        assert.equal(
          packageMatchesPattern(dependency.name, pattern),
          false,
          `${packageName} reaches forbidden dependency ${dependency.name}`,
        )
      }
    }
  }

  for (const integrationTest of gate.integrationTests) {
    const source = readFileSync(repositoryPath(integrationTest.path), 'utf8')
    for (const name of integrationTest.requiredTests) {
      assert.match(source, new RegExp(`\\bfn\\s+${name}\\s*\\(`, 'u'))
    }
  }

  const controlPlaneSource = readFileSync(
    repositoryPath('crates/winwincode-control-plane/src/delivery_execution.rs'),
    'utf8',
  )
  for (const symbol of gate.requiredGeneratedExecutionSymbols) {
    assert.ok(controlPlaneSource.includes(symbol), `delivery_execution.rs must use ${symbol}`)
  }
  for (const pattern of gate.forbiddenControlPlaneSourcePatterns) {
    assert.doesNotMatch(controlPlaneSource, new RegExp(pattern, 'u'))
  }

  const applicationSources = gate.requiredModules.flatMap(path => {
    const fullPath = repositoryPath(path)
    return extname(fullPath) === '.rs' ? [fullPath] : filesBelow(fullPath)
  })
  for (const path of applicationSources) {
    const source = readFileSync(path, 'utf8')
    for (const pattern of gate.forbiddenApplicationSourcePatterns) {
      assert.doesNotMatch(
        source,
        new RegExp(pattern, 'u'),
        `${relative(root, path)} duplicates Codex execution state`,
      )
    }
  }
})
