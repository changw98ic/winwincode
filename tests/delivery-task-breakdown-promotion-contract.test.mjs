import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const contractPath = join(
  root,
  'docs',
  'contracts',
  'delivery-task-breakdown-promotion.md',
)
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'delivery-task-breakdown-promotion.rules.json',
)

const REQUIRED_RULE_IDS = Object.freeze([
  'authority.approved_seal_is_the_only_promotion_input',
  'authority.planner_proposals_are_review_digest_input',
  'authority.transition_is_the_only_task_constructor',
  'bypass.generic_append_and_commit_are_rejected',
  'freshness.current_delivery_spec_review_and_digest_are_required',
  'freshness.revision_race_and_second_approval_fail_closed',
  'graph.invalid_graphs_change_no_fact',
  'http.payload_contains_stale_identity_only',
  'mapping.control_plane_derives_owner_and_status',
  'mapping.proposal_fields_and_order_are_exact',
  'outbox.only_committed_event_is_published',
  'replay.receipt_first_returns_original_graph',
  'store.specialized_command_is_the_only_append',
  'transaction.four_members_commit_or_rollback',
].sort())

const AUTHORITY_CHAIN = Object.freeze([
  'DeliveryTaskProposal',
  'ValidatedSolutionReviewSet',
  'ApprovedTaskPromotion',
  'TaskBreakdownPromotionTransition',
  'DeliveryCommand::ApproveTaskBreakdown',
  'DeliveryTaskBreakdownApprovedEvent',
])

const PROPOSAL_FIELDS = Object.freeze([
  'acceptanceCriterionIds',
  'blockedByTaskIds',
  'goal',
  'id',
  'title',
].sort())

const TASK_FIELDS = Object.freeze([
  'acceptanceCriterionIds',
  'blockedByTaskIds',
  'deliveryId',
  'goal',
  'id',
  'owner',
  'status',
  'title',
].sort())

const ATOMIC_MEMBERS = Object.freeze([
  'canonical-delivery-state',
  'delivery-journal-record',
  'scoped-command-receipt',
  'outbox-event',
])

const DOMAIN_UNIT_TESTS = Object.freeze([
  {
    name: 'approved_task_promotion_maps_exact_ordered_proposals',
    required: [
      'approved_task_promotion',
      'prepare_task_breakdown_promotion',
      'task_proposals',
      'DeliveryTaskStatus::Pending',
      'owner',
      'None',
      'assert_eq!',
    ],
  },
  {
    name: 'task_breakdown_transition_rejects_changed_source_or_seal',
    required: [
      'prepare_task_breakdown_promotion',
      'validate_for_delivery',
      'validate_source',
      'review_set_sha256',
    ],
    rejected: true,
  },
])

const SOLUTION_REVIEW_TESTS = Object.freeze([
  {
    name: 'solution_review_rejects_empty_task_proposals',
    required: ['taskProposals', 'resolve_current_solution_review'],
    rejected: true,
  },
  {
    name: 'solution_review_rejects_duplicate_task_and_criterion_ids',
    required: ['taskProposals', 'acceptanceCriterionIds', 'duplicate'],
    rejected: true,
  },
  {
    name: 'solution_review_rejects_self_missing_duplicate_and_cyclic_dependencies',
    required: ['blockedByTaskIds', 'self', 'missing', 'duplicate', 'cycle'],
    rejected: true,
  },
])

const DELIVERY_INTEGRATION_TESTS = Object.freeze([
  {
    name: 'task_breakdown_store_promotes_the_exact_ordered_graph_once',
    required: [
      'DeliveryCommand::ApproveTaskBreakdown',
      'review_set_sha256',
      'snapshot().tasks',
      'DeliveryTaskStatus::Pending',
      'assert_eq!',
    ],
  },
  {
    name: 'task_breakdown_store_rejects_stale_foreign_revised_or_changed_review',
    required: ['DeliveryCommand::ApproveTaskBreakdown', 'review_set_sha256'],
    rejected: true,
  },
  {
    name: 'task_breakdown_store_replay_returns_the_original_graph',
    required: [
      'DeliveryCommand::ApproveTaskBreakdown',
      'replayed',
      'snapshot().tasks',
      'assert_eq!',
    ],
  },
  {
    name: 'generic_append_cannot_write_task_breakdown_approved',
    required: [
      'DeliveryCommand::Append',
      'DeliveryMutationOperation::TaskBreakdownApproved',
    ],
    rejected: true,
  },
  {
    name: 'task_breakdown_revision_race_has_one_winner_and_no_partial_graph',
    required: [
      'DeliveryCommand::ApproveTaskBreakdown',
      'RevisionConflict',
      'snapshot().tasks',
    ],
    rejected: true,
  },
])

const CONTROL_PLANE_TESTS = Object.freeze([
  {
    name: 'task_breakdown_command_commits_state_journal_receipt_and_outbox_together',
    required: [
      'commit_delivery_task_breakdown',
      'load_state',
      'load_journal',
      'load_receipt',
      'pending_events',
      'DeliveryTaskBreakdownApprovedEvent',
      'assert_eq!',
    ],
  },
  {
    name: 'task_breakdown_receipt_first_replay_returns_original_graph_revision_and_event',
    required: [
      'commit_delivery_task_breakdown',
      'idempotent_replay',
      'load_journal',
      'tasks',
      'events',
      'assert_eq!',
    ],
  },
  {
    name: 'task_breakdown_same_scoped_request_with_changed_digest_is_a_conflict',
    required: ['commit_delivery_task_breakdown', 'RequestConflict'],
    rejected: true,
  },
  {
    name: 'task_breakdown_revision_race_commits_no_partial_loser_facts',
    required: [
      'commit_delivery_task_breakdown',
      'RevisionConflict',
      'load_journal',
      'pending_events',
    ],
    rejected: true,
  },
  {
    name: 'task_breakdown_failure_at_each_atomic_member_rolls_back_all_four',
    required: [
      'commit_delivery_task_breakdown',
      'product_state',
      'aggregate_journal_records',
      'command_receipts',
      'outbox',
      'expect_err',
    ],
  },
  {
    name: 'task_breakdown_publish_failure_keeps_the_committed_event_for_replay',
    required: [
      'commit_delivery_task_breakdown',
      'PublicationPending',
      'pending_events',
      'DeliveryTaskBreakdownApprovedEvent',
    ],
  },
  {
    name: 'generic_control_plane_commit_cannot_bypass_task_breakdown_authority',
    required: ['ControlPlane', 'commit', 'DeliveryApproveTaskBreakdown'],
    rejected: true,
  },
])

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function repositoryPath(path) {
  assert.equal(path.startsWith('/'), false, `${path} must be repository-relative`)
  assert.equal(path.split('/').includes('..'), false, `${path} must not escape the repository`)
  return join(root, path)
}

function stripRustComments(source) {
  return source
    .replace(/\/\*[\s\S]*?\*\//gu, '')
    .replace(/\/\/[^\n]*/gu, '')
}

function escaped(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&')
}

function namedBlock(source, signature, description) {
  const match = signature.exec(source)
  assert.ok(match, `${description} is missing`)
  const openingBrace = source.indexOf('{', match.index)
  assert.notEqual(openingBrace, -1, `${description} has no body`)
  let depth = 0
  for (let index = openingBrace; index < source.length; index += 1) {
    if (source[index] === '{') depth += 1
    if (source[index] === '}') depth -= 1
    if (depth === 0) return source.slice(openingBrace + 1, index)
  }
  assert.fail(`${description} has an unclosed body`)
}

function namedRustFunction(source, name) {
  return namedBlock(
    source,
    new RegExp(`\\bfn\\s+${escaped(name)}\\s*\\(`, 'u'),
    `Rust function ${name}`,
  )
}

function namedRustStruct(source, name) {
  return namedBlock(
    source,
    new RegExp(`\\bstruct\\s+${escaped(name)}(?:\\s*<[^>{}]+>)?\\b`, 'u'),
    `Rust struct ${name}`,
  )
}

function rustStructFields(body) {
  return [...body.matchAll(
    /(?:^|\n)\s*(?:pub(?:\([^)]*\))?\s+)?([a-z_][a-z0-9_]*)\s*:/gu,
  )].map(match => match[1])
}

function namedTypeScriptType(source, name) {
  return namedBlock(
    source,
    new RegExp(`\\bexport\\s+type\\s+${escaped(name)}\\s*=`, 'u'),
    `TypeScript type ${name}`,
  )
}

function typescriptTypeFields(body) {
  return [...body.matchAll(/(?:^|\n)\s*readonly\s+"([^"]+)"\??\s*:/gu)]
    .map(match => match[1])
}

function assertRejected(body, name) {
  assert.match(
    body,
    /(?:expect_err\s*\(|\.is_err\s*\(|assert_eq!\s*\([\s\S]*?(?:Err|Conflict)|matches!\s*\([\s\S]*?Err)/u,
    `${name} must execute and assert a rejected case`,
  )
}

function assertTestBody(source, specification) {
  const body = namedRustFunction(source, specification.name)
  for (const token of specification.required) {
    assert.equal(body.includes(token), true, `${specification.name} does not exercise ${token}`)
  }
  if (specification.rejected) assertRejected(body, specification.name)
}

function implementationTriggered(gate) {
  return gate.activation.triggerPaths.some(path => existsSync(repositoryPath(path)))
}

function runCargoTest(packageName, target, expectedNames) {
  const args = ['test', '-p', packageName, '--locked']
  if (target.kind === 'integration') args.push('--test', target.name)
  else args.push('--lib', target.filter)
  args.push('--', '--test-threads=1')
  const result = spawnSync('cargo', args, { cwd: root, encoding: 'utf8' })
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`)
  const output = `${result.stdout}\n${result.stderr}`
  for (const name of expectedNames) {
    assert.match(output, new RegExp(`test .*${escaped(name)} \\.\\.\\. ok`, 'u'), name)
  }
}

test('phase 2.5.7.1 freezes one reviewed-proposal to canonical-task authority chain', () => {
  const rules = json(rulesPath)
  assert.deepEqual({
    schemaVersion: rules.schemaVersion,
    status: rules.status,
    issueId: rules.issueId,
    preflightIssueId: rules.preflightIssueId,
    documentation: rules.documentation,
  }, {
    schemaVersion: 'winwincode.delivery-task-breakdown-promotion-rules.v1',
    status: 'required-contract-not-implementation-proof',
    issueId: 'winwincode-9c4.16.2.5.7',
    preflightIssueId: 'winwincode-9c4.16.2.5.7.1',
    documentation: relative(root, contractPath),
  })

  assert.deepEqual(
    rules.authorityChain.map(step => step.fact),
    AUTHORITY_CHAIN,
  )
  assert.equal(rules.authorityChain[0].storedOutsideReviewSet, false)
  assert.equal(rules.authorityChain[1].constructor, 'resolve_current_solution_review')
  assert.equal(
    rules.authorityChain[2].constructor,
    'ValidatedSolutionReviewSet::approved_task_promotion',
  )
  assert.equal(rules.authorityChain[2].decision, 'approved-only')
  assert.equal(
    rules.authorityChain[2].currentAttemptValidator,
    'ApprovedTaskPromotion::validate_for_delivery',
  )
  assert.equal(
    rules.authorityChain[3].constructor,
    'prepare_task_breakdown_promotion',
  )
  assert.equal(rules.authorityChain[4].callerTasksAllowed, false)

  assert.deepEqual([...rules.proposal.fields].sort(), PROPOSAL_FIELDS)
  assert.equal(rules.proposal.minimumCount, 1)
  assert.equal(rules.proposal.maximumCount, 200)
  assert.equal(rules.proposal.orderIsAuthority, true)
  assert.equal(rules.proposal.digestIncludesExactOrder, true)
  assert.deepEqual([...rules.task.fields].sort(), TASK_FIELDS)
  assert.deepEqual(rules.task.derivedFields, {
    deliveryId: 'current-delivery-id',
    owner: null,
    status: 'pending',
  })
  assert.equal(rules.task.order, 'exact-reviewed-proposal-order')

  assert.deepEqual(rules.graph.invalidCases, [
    'empty',
    'duplicate-task-id',
    'empty-criterion-set',
    'duplicate-criterion-id',
    'foreign-or-missing-current-criterion',
    'incomplete-current-criterion-coverage',
    'self-dependency',
    'duplicate-dependency',
    'missing-dependency',
    'cycle',
  ])
  assert.equal(rules.graph.invalidCaseEffect, 'no-delivery-journal-receipt-or-outbox-change')

  assert.deepEqual(rules.http.payload, {
    type: 'DeliveryApproveTaskBreakdownPayload',
    exactFields: ['deliveryId', 'reviewSetSha256'],
    additionalProperties: false,
    callerTasksAllowed: false,
  })
  assert.equal(rules.http.command, 'delivery.approve_task_breakdown')
  assert.equal(rules.http.digestTransport, 'Sha256Digest-sha256-prefix-lowercase-64hex')
  assert.equal(rules.http.digestComparison, 'strip-one-prefix-and-compare-exact-approved-review')

  assert.deepEqual(rules.domain.interface, {
    module: 'application::task_breakdown',
    constructor: 'prepare_task_breakdown_promotion',
    input: ['&Delivery', '&ApprovedTaskPromotion'],
    output: 'Result<TaskBreakdownPromotionTransition, TaskBreakdownPromotionError>',
  })
  assert.deepEqual(rules.store.command, {
    variant: 'DeliveryCommand::ApproveTaskBreakdown',
    input: 'Box<ApproveDeliveryTaskBreakdown>',
    genericAppendAllowed: false,
  })
  assert.equal(rules.controlPlane.module, 'task_breakdown_transaction.rs')
  assert.equal(rules.controlPlane.entry, 'ControlPlane::commit_delivery_task_breakdown')
  assert.deepEqual(rules.controlPlane.atomicMembers, ATOMIC_MEMBERS)
  assert.equal(rules.controlPlane.event.type, 'DeliveryTaskBreakdownApprovedEvent')
  assert.equal(rules.controlPlane.publishSource, 'committed-outbox-only')
  assert.equal(rules.controlPlane.preCommitFailure, 'rollback-all-four-and-publish-nothing')

  assert.deepEqual(rules.replay.order.slice(0, 2), [
    'derive-scoped-receipt-identity-and-command-digest',
    'load-receipt-before-current-delivery-or-review',
  ])
  assert.equal(rules.replay.identicalRequest, 'return-original-revision-graph-and-event-bytes')
  assert.equal(rules.replay.changedDigest, 'request-conflict')
  assert.equal(rules.replay.recomputeCurrentReviewOnReplay, false)

  const ruleIds = rules.rules.map(rule => rule.id)
  assert.equal(new Set(ruleIds).size, ruleIds.length)
  assert.deepEqual([...ruleIds].sort(), REQUIRED_RULE_IDS)
  for (const rule of rules.rules) {
    assert.ok(rule.statement.length > 0, `${rule.id} needs a statement`)
    assert.ok(rule.sources.length > 0, `${rule.id} needs source traces`)
    for (const path of rule.sources) {
      assert.equal(existsSync(repositoryPath(path)), true, `${rule.id}: ${path} is missing`)
    }
  }
})

test('HTTP preflight records the caller-tasks gap or enforces the one canonical payload', () => {
  const rules = json(rulesPath)
  const schema = json(repositoryPath(rules.http.schemaPath))
  const payload = schema.$defs.DeliveryApproveTaskBreakdownPayload
  const fields = Object.keys(payload.properties).sort()
  const required = [...payload.required].sort()
  const canonical = [...rules.http.payload.exactFields].sort()

  assert.equal(payload.additionalProperties, false)
  if (fields.includes('tasks')) {
    assert.deepEqual(fields, ['deliveryId', 'tasks'])
    assert.deepEqual(required, ['deliveryId', 'tasks'])
    assert.equal(
      rules.currentFindings.some(finding => finding.id === 'caller-supplied-tasks-http'),
      true,
    )
    assert.equal(rules.implementationGate.absentTriggerStatus, 'planned-not-implemented')
    return
  }

  assert.deepEqual(fields, canonical)
  assert.deepEqual(required, canonical)
  assert.equal(
    payload.properties.reviewSetSha256.$ref,
    './domain.schema.json#/$defs/Sha256Digest',
  )

  const rust = stripRustComments(readFileSync(
    repositoryPath(rules.http.generatedRustPath),
    'utf8',
  ))
  const rustPayload = namedRustStruct(rust, rules.http.payload.type)
  assert.deepEqual(rustStructFields(rustPayload).sort(), [
    'delivery_id',
    'review_set_sha256',
  ])
  assert.doesNotMatch(rustPayload, /\btasks\s*:/u)

  const typescript = readFileSync(repositoryPath(rules.http.generatedTypeScriptPath), 'utf8')
  const typescriptPayload = namedTypeScriptType(typescript, rules.http.payload.type)
  assert.deepEqual(typescriptTypeFields(typescriptPayload).sort(), canonical)
  assert.doesNotMatch(typescriptPayload, /\b(?:tasks|ownerActorId)\b/u)

  assert.equal(
    schema['x-winwincode-semantics'].taskBreakdownApproval.staleDigestError,
    'REVISION_CONFLICT',
  )
  assert.deepEqual(schema['x-winwincode-semantics'].errors.REVISION_CONFLICT, {
    httpStatus: 409,
    retryable: false,
  })

  const transaction = readFileSync(repositoryPath(
    'crates/winwincode-control-plane/src/task_breakdown_transaction.rs',
  ), 'utf8')
  assert.match(
    transaction,
    /DeliveryStoreErrorCode::ReviewSetStale\s*=>\s*\{[\s\S]*?StorageError::revision_token_conflict\("reviewSetSha256"\)/u,
  )

  const transactionTests = readFileSync(repositoryPath(
    'crates/winwincode-control-plane/tests/task_breakdown_transaction.rs',
  ), 'utf8')
  const staleDigestTest = namedRustFunction(
    transactionTests,
    'task_breakdown_same_revision_stale_review_digest_maps_to_revision_conflict',
  )
  assert.match(staleDigestTest, /StorageErrorKind::RevisionConflict/u)
  assert.match(
    staleDigestTest,
    /reviewSetSha256 no longer identifies the current solution review/u,
  )
})

test('plain-language contract states every concrete promotion and replay outcome', () => {
  const rules = json(rulesPath)
  const contract = readFileSync(contractPath, 'utf8')
  for (const phrase of [
    'Planner 提出的任务必须先进入同一个方案审核集合',
    '调用方只能提交 `deliveryId` 和 `reviewSetSha256`',
    '`tasks` 不再是这个命令的输入',
    '只有 `approved` 能产生 `ApprovedTaskPromotion`',
    '顺序和五个提案字段逐项不变',
    '`owner = None`',
    '`status = pending`',
    '状态、Delivery journal、命令回执和 outbox 事件',
    '先查原命令回执，再读取当前 Delivery 或重新解析审核集合',
    '返回第一次提交的任务图、revision 和事件字节',
    '普通 `Append` 和通用 `ControlPlane::commit`',
  ]) assert.equal(contract.includes(phrase), true, phrase)
  for (const rule of rules.rules) {
    assert.equal(contract.includes(`\`${rule.id}\``), true, `${rule.id} is absent from prose`)
  }
})

test('implementation trigger activates real Rust seams, test bodies, and executable gates', () => {
  const rules = json(rulesPath)
  const gate = rules.implementationGate
  assert.deepEqual(gate.activation, {
    triggerPaths: [
      'crates/winwincode-delivery/src/application/task_breakdown.rs',
      'crates/winwincode-control-plane/src/task_breakdown_transaction.rs',
      'crates/winwincode-delivery/tests/task_breakdown_promotion.rs',
      'crates/winwincode-control-plane/tests/task_breakdown_transaction.rs',
    ],
    whenMissing: 'verify-current-gaps-and-keep-planned-gate',
    whenAnyPresent: 'require-all-production-seams-tests-and-no-old-path',
  })
  assert.deepEqual(gate.domainUnitTests.requiredTests, DOMAIN_UNIT_TESTS.map(test => test.name))
  assert.deepEqual(
    gate.solutionReviewTests.requiredTests,
    SOLUTION_REVIEW_TESTS.map(test => test.name),
  )
  assert.deepEqual(
    gate.deliveryIntegrationTest.requiredTests,
    DELIVERY_INTEGRATION_TESTS.map(test => test.name),
  )
  assert.deepEqual(
    gate.controlPlaneIntegrationTest.requiredTests,
    CONTROL_PLANE_TESTS.map(test => test.name),
  )

  if (!implementationTriggered(gate)) {
    const legacyTaskSource = readFileSync(
      repositoryPath(gate.currentGapEvidence.legacyTaskPath),
      'utf8',
    )
    assert.match(legacyTaskSource, /pub\s+fn\s+approve_task_breakdown\b/u)
    assert.match(legacyTaskSource, /tasks\s*:\s*Vec<DeliveryTask>/u)
    assert.doesNotMatch(legacyTaskSource, /\bApprovedTaskPromotion\b/u)
    assert.equal(gate.absentTriggerStatus, 'planned-not-implemented')
    return
  }

  for (const path of gate.requiredPaths) {
    assert.equal(existsSync(repositoryPath(path)), true, `${path} is missing after trigger`)
  }

  const solutionReview = stripRustComments(readFileSync(
    repositoryPath(gate.solutionReviewPath),
    'utf8',
  ))
  const taskBreakdown = stripRustComments(readFileSync(
    repositoryPath(gate.taskBreakdownPath),
    'utf8',
  ))
  const applicationModule = stripRustComments(readFileSync(
    repositoryPath(gate.applicationModulePath),
    'utf8',
  ))
  const store = stripRustComments(readFileSync(repositoryPath(gate.storePath), 'utf8'))
  const controlPlane = stripRustComments(readFileSync(
    repositoryPath(gate.controlPlanePath),
    'utf8',
  ))
  const transaction = stripRustComments(readFileSync(
    repositoryPath(gate.controlPlaneTransactionPath),
    'utf8',
  ))

  assert.match(applicationModule, /\bpub\s+mod\s+task_breakdown\s*;/u)
  assert.match(
    solutionReview,
    /pub\(crate\)\s+struct\s+ApprovedTaskPromotion\s*</u,
  )
  assert.doesNotMatch(
    solutionReview.slice(
      Math.max(0, solutionReview.indexOf('struct ApprovedTaskPromotion') - 300),
      solutionReview.indexOf('struct ApprovedTaskPromotion'),
    ),
    /(?:Deserialize|Serialize|Clone)/u,
  )
  assert.match(
    solutionReview,
    /pub\(crate\)\s+fn\s+approved_task_promotion\s*\([^)]*\)\s*->\s*Option<ApprovedTaskPromotion<'_>>/u,
  )
  const approvedBody = namedRustFunction(solutionReview, 'approved_task_promotion')
  assert.match(approvedBody, /ValidatedReviewStatus::Approved/u)
  assert.match(approvedBody, /task_proposals/u)
  const currentAttemptValidator = namedRustFunction(solutionReview, 'validate_for_delivery')
  for (const token of [
    'delivery_id',
    'delivery_spec_id',
    'delivery_spec_revision',
    'review_stage_run_id',
    'attention_item_id',
  ]) assert.equal(
    currentAttemptValidator.includes(token),
    true,
    `approved promotion current-attempt validation omits ${token}`,
  )

  assert.match(
    taskBreakdown,
    /pub\(crate\)\s+fn\s+prepare_task_breakdown_promotion\s*\([\s\S]*?&\s*Delivery\s*,[\s\S]*?&\s*ApprovedTaskPromotion(?:<'_>)?\s*\)\s*->\s*Result\s*<\s*TaskBreakdownPromotionTransition\s*,\s*TaskBreakdownPromotionError\s*>/u,
  )
  const prepareBody = namedRustFunction(taskBreakdown, 'prepare_task_breakdown_promotion')
  for (const token of [
    'task_proposals()',
    'validate_for_delivery',
    'DeliveryTask {',
    'owner: None',
    'DeliveryTaskStatus::Pending',
    'TaskBreakdownPromotionTransition',
  ]) assert.equal(prepareBody.includes(token), true, `prepare transition omits ${token}`)
  assert.ok(
    prepareBody.indexOf('validate_for_delivery') < prepareBody.indexOf('task_proposals()'),
    'current Delivery attempt must be validated before proposals are promoted',
  )
  assert.doesNotMatch(taskBreakdown, /tasks\s*:\s*Vec<DeliveryTask>/u)

  const transitionBody = namedRustStruct(taskBreakdown, 'TaskBreakdownPromotionTransition')
  assert.ok(
    rustStructFields(transitionBody).length >= gate.minimumTransitionPrivateFieldCount,
    'TaskBreakdownPromotionTransition does not seal enough authority facts',
  )
  assert.doesNotMatch(
    transitionBody,
    /(?:^|\n)\s*pub(?:\([^)]*\))?\s+[a-z_][a-z0-9_]*\s*:/u,
  )
  const eventBody = namedRustStruct(taskBreakdown, 'DeliveryTaskBreakdownApprovedEvent')
  assert.deepEqual(rustStructFields(eventBody).sort(), [...gate.eventFields].sort())
  assert.match(taskBreakdown, /#\[serde\([^\]]*deny_unknown_fields[^\]]*\)\]/u)

  const legacyTaskSource = readFileSync(
    repositoryPath(gate.currentGapEvidence.legacyTaskPath),
    'utf8',
  )
  assert.doesNotMatch(legacyTaskSource, /pub\s+fn\s+approve_task_breakdown\b/u)
  assert.doesNotMatch(legacyTaskSource, /tasks\s*:\s*Vec<DeliveryTask>/u)

  const commandBody = namedRustStruct(store, 'ApproveDeliveryTaskBreakdown')
  assert.deepEqual(rustStructFields(commandBody).sort(), [...gate.storeCommandFields].sort())
  assert.doesNotMatch(commandBody, /\b(?:tasks|snapshot|transition)\s*:/u)
  assert.match(
    store,
    /DeliveryCommand::ApproveTaskBreakdown|ApproveTaskBreakdown\s*\(\s*Box\s*<\s*ApproveDeliveryTaskBreakdown\s*>\s*\)/u,
  )
  const genericAppend = namedRustFunction(store, 'append')
  assert.match(genericAppend, /DeliveryMutationOperation::TaskBreakdownApproved/u)
  assert.match(store, /prepare_task_breakdown_promotion/u)
  assert.match(store, /approved_task_promotion/u)
  assert.match(store, /review_set_sha256/u)

  assert.match(controlPlane, /\bmod\s+task_breakdown_transaction\s*;/u)
  assert.match(controlPlane, /pub\s+fn\s+commit_delivery_task_breakdown\s*\(/u)
  const publicEntry = namedRustFunction(controlPlane, 'commit_delivery_task_breakdown')
  assert.match(publicEntry, /task_breakdown_transaction::execute/u)
  assert.match(publicEntry, /flush_outbox/u)
  assert.ok(
    publicEntry.indexOf('task_breakdown_transaction::execute') < publicEntry.indexOf('flush_outbox'),
    'task breakdown must commit before publishing the durable outbox',
  )
  const genericDeliveryCommands = namedRustFunction(controlPlane, 'delivery_command')
  assert.match(genericDeliveryCommands, /CommandName::DeliveryApproveTaskBreakdown/u)

  assert.match(transaction, /DeliveryApproveTaskBreakdownPayload/u)
  assert.doesNotMatch(transaction, /\b(?:DeliveryTaskInput|DeliveryTaskProposalProjection)\b/u)
  assert.doesNotMatch(transaction, /\btasks\s*:\s*(?:Vec|\[|&\[)/u)
  const executeBody = namedRustFunction(transaction, 'execute')
  for (const token of [
    'load_receipt',
    'load_journal',
    'DeliveryCommand::ApproveTaskBreakdown',
    'StateChange::new',
    'NewOutboxEvent::new',
    'with_journal_publication',
  ]) assert.equal(executeBody.includes(token), true, `transaction omits ${token}`)
  assert.match(executeBody, /\bstorage\s*\.\s*commit\s*\(/u)
  assert.ok(
    executeBody.indexOf('load_receipt') < executeBody.indexOf('load_journal'),
    'receipt replay must be checked before reading current Delivery authority',
  )
  assert.match(transaction, /DeliveryTaskBreakdownApprovedEvent/u)
  assert.match(transaction, /serde_json::to_value[\s\S]*!=/u)
  assert.match(transaction, /delivery\.task_breakdown\.approved/u)

  const domainUnitSource = `${solutionReview}\n${taskBreakdown}`
  for (const specification of [...DOMAIN_UNIT_TESTS, ...SOLUTION_REVIEW_TESTS]) {
    assertTestBody(domainUnitSource, specification)
  }
  const deliveryTestSource = stripRustComments(readFileSync(
    repositoryPath(gate.deliveryIntegrationTest.path),
    'utf8',
  ))
  for (const specification of DELIVERY_INTEGRATION_TESTS) {
    assertTestBody(deliveryTestSource, specification)
  }
  const controlPlaneTestSource = stripRustComments(readFileSync(
    repositoryPath(gate.controlPlaneIntegrationTest.path),
    'utf8',
  ))
  for (const specification of CONTROL_PLANE_TESTS) {
    assertTestBody(controlPlaneTestSource, specification)
  }

  runCargoTest('winwincode-delivery', {
    kind: 'unit',
    filter: 'task_breakdown',
  }, [...DOMAIN_UNIT_TESTS, ...SOLUTION_REVIEW_TESTS].map(test => test.name))
  runCargoTest('winwincode-delivery', {
    kind: 'integration',
    name: gate.deliveryIntegrationTest.target,
  }, DELIVERY_INTEGRATION_TESTS.map(test => test.name))
  runCargoTest('winwincode-control-plane', {
    kind: 'integration',
    name: gate.controlPlaneIntegrationTest.target,
  }, CONTROL_PLANE_TESTS.map(test => test.name))
})
