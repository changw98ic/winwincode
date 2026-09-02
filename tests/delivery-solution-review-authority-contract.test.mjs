import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const contractPath = join(
  root,
  'docs',
  'contracts',
  'delivery-solution-review-authority.md',
)
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'delivery-solution-review-authority.rules.json',
)

const AUTHORITY_IDENTITY_FIELDS = Object.freeze([
  'attentionItemId',
  'decision',
  'deliveryId',
  'deliverySpecId',
  'deliverySpecRevision',
  'planningSessionBindingId',
  'planningStageRunId',
  'reviewSetSha256',
  'reviewStageRunId',
  'reviewStatus',
  'reviewedAt',
  'reviewerId',
].sort())

const DIGEST_INPUT_FIELDS = Object.freeze([
  'architectureDiagram',
  'attentionItemId',
  'deliveryId',
  'deliverySpecId',
  'deliverySpecRevision',
  'planningSessionBindingId',
  'planningStageRunId',
  'preparedAt',
  'processDiagram',
  'protocol',
  'reviewStageRunId',
  'risks',
  'schemaVersion',
  'solution',
  'taskProposals',
  'unresolvedItems',
].sort())

const TASK_PROPOSAL_FIELDS = Object.freeze([
  'acceptanceCriterionIds',
  'blockedByTaskIds',
  'goal',
  'id',
  'title',
].sort())

const PUBLIC_PROJECTION_FIELDS = Object.freeze([
  'approach',
  'architectureDiagram',
  'attentionItemId',
  'components',
  'comments',
  'connections',
  'decision',
  'deliveryId',
  'deliverySpecId',
  'deliverySpecRevision',
  'planningSessionBindingId',
  'planningStageRunId',
  'processDiagram',
  'reviewStageRunId',
  'reviewedAt',
  'reviewerId',
  'reviewSetSha256',
  'reviewStatus',
  'requestedChanges',
  'risks',
  'solutionId',
  'summary',
  'taskProposals',
  'unresolvedItems',
].sort())

const FORBIDDEN_PUBLIC_FIELDS = Object.freeze([
  'apiKey',
  'assignedTo',
  'authorization',
  'codexSessionId',
  'credential',
  'dshSessionId',
  'executionJobId',
  'providerRequest',
  'providerResponse',
  'rawAttentionContext',
  'rawAttentionResolution',
  'rawRuntimeLog',
  'reviewSessionBinding',
  'reviewSessionBindingId',
  'stderr',
  'stdout',
  'toolOutput',
  'toolPayload',
].sort())

const REQUIRED_RULE_IDS = Object.freeze([
  'authority.current_exact_review_set',
  'authority.pending_and_settled_are_one_type',
  'authority.production_resolver_only',
  'decision.exact_settlement',
  'digest.covers_ordered_task_proposals',
  'encoding.one_strict_v1',
  'legacy.human_review_binding_is_rejected',
  'projection.pending_review_is_visible',
  'projection.safe_fields_only',
  'promotion.approved_only',
].sort())

const REQUIRED_RUST_TESTS = Object.freeze([
  'human_solution_review_rejects_execution_session_binding',
  'only_approved_solution_review_authorizes_task_promotion',
  'pending_solution_review_projects_safe_solution_and_non_empty_ordered_task_proposals',
  'settled_solution_review_projects_exact_decision_reviewer_and_time',
  'solution_review_digest_covers_ordered_task_proposals',
  'solution_review_projection_excludes_raw_context_resolution_secrets_tools_and_logs',
  'solution_review_resolver_rejects_duplicate_current_review',
  'solution_review_resolver_rejects_stale_or_foreign_authority',
  'solution_review_v1_rejects_unknown_keys_and_legacy_v2',
  'solution_review_v1_restart_parse_is_byte_stable',
].sort())

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function repositoryPath(path) {
  assert.equal(path.startsWith('/'), false, `${path} must be repository-relative`)
  assert.equal(path.split('/').includes('..'), false, `${path} must not escape the repository`)
  return join(root, path)
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

function stripRustComments(source) {
  return source
    .replace(/\/\*[\s\S]*?\*\//gu, '')
    .replace(/\/\/[^\n]*/gu, '')
}

function namedRustBlock(source, signature, description) {
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
  return namedRustBlock(
    source,
    new RegExp(`\\bfn\\s+${name}\\s*\\(`, 'u'),
    `Rust test ${name}`,
  )
}

function namedRustStruct(source, name, visibility = 'public') {
  const prefix = visibility === 'crate'
    ? 'pub\\(crate\\)\\s+'
    : visibility === 'private'
      ? '(?<!pub\\s)'
      : 'pub\\s+'
  return namedRustBlock(
    source,
    new RegExp(`\\b${prefix}struct\\s+${name}\\b`, 'u'),
    `Rust type ${name}`,
  )
}

function rustStructFields(body) {
  return [...body.matchAll(
    /(?:^|\n)\s*(?:pub(?:\([^)]*\))?\s+)?([a-z_][a-z0-9_]*)\s*:/gu,
  )].map(match => match[1])
}

function assertRejected(body, name) {
  assert.match(
    body,
    /(?:is_err\s*\(|expect_err\s*\(|assert_eq!\s*\([\s\S]*?Err\b)/u,
    `${name} must execute and assert a rejected case`,
  )
}

test('phase 2.5.1.1 freezes one typed pending-or-settled solution review authority', () => {
  const rules = json(rulesPath)
  assert.deepEqual({
    schemaVersion: rules.schemaVersion,
    status: rules.status,
    phaseTask: rules.phaseTask,
    documentation: rules.documentation,
  }, {
    schemaVersion: 1,
    status: 'implemented-enforced',
    phaseTask: 'winwincode-9c4.16.2.5.1.1',
    documentation: relative(root, contractPath),
  })

  assert.equal(rules.authority.type, 'ValidatedSolutionReviewSet')
  assert.equal(rules.authority.owner, 'winwincode-delivery')
  assert.deepEqual([...rules.authority.identityFields].sort(), AUTHORITY_IDENTITY_FIELDS)
  assert.equal(rules.authority.cardinality, 'exactly-one-current-review-set')
  assert.deepEqual(rules.authority.reviewStatuses, [
    'pending',
    'approved',
    'changes_requested',
    'rejected',
  ])
  assert.deepEqual(rules.authority.decisions, [
    null,
    'approve',
    'request_changes',
    'reject',
  ])

  assert.deepEqual(rules.encoding, {
    schemaVersion: 1,
    contextProtocol: 'winwincode.solution-review-context.v1',
    decisionProtocol: 'winwincode.solution-review-decision.v1',
    exactKeys: true,
    legacyAliasesAllowed: false,
    dualReadAllowed: false,
  })
  assert.deepEqual([...rules.digest.inputFields].sort(), DIGEST_INPUT_FIELDS)
  assert.equal(rules.digest.algorithm, 'sha256')
  assert.equal(rules.digest.encoding, 'serde-json-struct-order-utf8-no-whitespace')
  assert.equal(rules.digest.format, 'lowercase-hex-64')
  assert.equal(rules.digest.orderedTaskProposalsIncluded, true)

  assert.deepEqual([...rules.taskProposal.fields].sort(), TASK_PROPOSAL_FIELDS)
  assert.equal(rules.taskProposal.minimumCount, 1)
  assert.equal(rules.taskProposal.orderIsAuthority, true)
  assert.equal(rules.taskProposal.ownerSuppliedByPlanner, false)
  assert.equal(rules.taskProposal.initialStatusSuppliedByPlanner, false)
  assert.equal(rules.taskProposal.graphPolicy, 'unique-current-criteria-complete-acyclic-dag')

  assert.deepEqual(rules.settlement.pending, {
    decision: null,
    reviewerId: null,
    reviewedAt: null,
    attentionStatus: 'open',
    reviewStageRunStatuses: ['waiting', 'running'],
    authorizesTaskPromotion: false,
  })
  for (const status of ['approved', 'changes_requested', 'rejected']) {
    assert.equal(rules.settlement[status].reviewerId, 'authenticated-actor')
    assert.equal(rules.settlement[status].reviewedAt, 'resolvedAt-and-stage-finishedAt')
  }
  assert.equal(rules.settlement.approved.authorizesTaskPromotion, true)
  assert.equal(rules.settlement.changes_requested.authorizesTaskPromotion, false)
  assert.equal(rules.settlement.rejected.authorizesTaskPromotion, false)

  assert.deepEqual([...rules.safeProjection.fields].sort(), PUBLIC_PROJECTION_FIELDS)
  assert.equal(rules.safeProjection.wireField, 'solutionReview')
  assert.equal(rules.safeProjection.type, 'SolutionReviewProjection')
  assert.equal(rules.safeProjection.solutionAliasAllowed, false)
  assert.deepEqual(
    [...rules.safeProjection.forbiddenFields].sort(),
    FORBIDDEN_PUBLIC_FIELDS,
  )

  assert.deepEqual(rules.productionConstruction, {
    factDeserializeAllowed: false,
    publicRawConstructorAllowed: false,
    callerSubmittedFactAllowed: false,
    httpRawFactConstructorAllowed: false,
    resolver: 'winwincode-delivery::resolve_current_solution_review',
    resolverSource: 'current-canonical-delivery-attention',
    testSupport: 'module-local-cfg-test-only',
  })

  assert.equal(Object.hasOwn(rules, 'currentFindings'), false)
  assert.deepEqual(
    rules.closedFindings.map(finding => ({
      id: finding.id,
      status: finding.status,
    })),
    [
      { id: 'approved-only-authority', status: 'closed' },
      { id: 'caller-injected-review-fact', status: 'closed' },
      { id: 'pending-task-proposals-missing', status: 'closed' },
      { id: 'legacy-solution-wire', status: 'closed' },
    ],
  )
  assert.equal(Object.hasOwn(rules, 'implementationPlan'), false)
  assert.equal(rules.verificationSteps.length, 8)
  assert.deepEqual(
    rules.verificationSteps.map(step => step.order),
    [1, 2, 3, 4, 5, 6, 7, 8],
  )
  for (const step of rules.verificationSteps) {
    assert.ok(step.action.length > 0)
    assert.ok(step.proof.length > 0)
  }

  const ruleIds = rules.rules.map(rule => rule.id)
  assert.equal(new Set(ruleIds).size, ruleIds.length)
  assert.deepEqual([...ruleIds].sort(), REQUIRED_RULE_IDS)
  for (const rule of rules.rules) {
    assert.ok(rule.statement.length > 0, `${rule.id} needs a statement`)
    assert.ok(rule.sources.length > 0, `${rule.id} needs a source trace`)
    for (const path of rule.sources) {
      assert.equal(existsSync(repositoryPath(path)), true, `${rule.id}: ${path} is missing`)
    }
  }
})

test('the legacy TypeScript source is traced but its human binding and v2 wire are rejected', () => {
  const trace = json(rulesPath).sourceTrace
  for (const mapping of trace.typescriptPublicSymbols) assertExportedSymbol(mapping)

  const contractSource = readFileSync(
    repositoryPath('packages/contracts/src/strongflow-plan-review.ts'),
    'utf8',
  )
  assert.match(contractSource, /const MAX_TEXT_LENGTH = 65_536/u)
  assert.match(contractSource, /const MAX_COLLECTION_LENGTH = 200/u)
  assert.match(contractSource, /winwincode\.plan-review-context\.v2/u)

  const legacySource = readFileSync(
    repositoryPath('packages/strongflow/src/plan-review.ts'),
    'utf8',
  )
  assert.match(legacySource, /readonly reviewSessionBinding: SessionBinding/u)
  assert.match(legacySource, /reviewSetDigest\(withoutDigest\)/u)

  const rustStageSource = readFileSync(
    repositoryPath('crates/winwincode-delivery/src/application/stage.rs'),
    'utf8',
  )
  assert.match(
    rustStageSource,
    /human review stages are not ExecutionJob SessionBindings/u,
  )
  assert.deepEqual(trace.corrections, {
    reviewSessionBinding: 'rejected-not-migrated',
    legacyV2Encoding: 'rejected-not-aliased',
    rawContextParser: 'source-trace-not-production-authority',
    humanPlanReviewExecutionJob: 'forbidden',
  })
})

test('plain-language solution review authority contract and machine rules stay paired', () => {
  const rules = json(rulesPath)
  const contract = readFileSync(contractPath, 'utf8')
  assert.match(contract, /^# Rust 方案审核权威合同$/mu)
  assert.match(contract, /pending.*方案.*任务提案/iu)
  assert.match(contract, /Human plan-review.*不是.*`ExecutionJob`/u)
  assert.match(contract, /`reviewSessionBinding`.*不进入/u)
  assert.match(contract, /`solutionReview`.*`SolutionReviewProjection`/u)
  assert.match(contract, /旧.*v2.*拒绝/u)
  assert.match(contract, /raw Attention.*context.*resolution/iu)
  assert.doesNotMatch(contract, /当前实现缺口|planned gate/u)
  assert.match(contract, /implemented\/enforced/u)
  for (const rule of rules.rules) {
    assert.equal(contract.includes(`\`${rule.id}\``), true, `${rule.id} is absent from prose`)
  }
})

test('the phase 2.5.1.2 application trigger activates typed resolver and adversarial gates', () => {
  const gate = json(rulesPath).rustGate
  assert.deepEqual(gate.activation, {
    trigger: 'crates/winwincode-delivery/src/application/solution_review.rs',
    status: 'active',
  })
  assert.equal(gate.factTypeName, 'ValidatedSolutionReviewSet')
  assert.equal(gate.factVisibility, 'pub(crate)')
  assert.equal(gate.projectionTypeName, 'SolutionReviewProjection')
  assert.equal(gate.taskProposalTypeName, 'DeliveryTaskProposal')
  assert.equal(gate.taskProposalVisibility, 'pub(crate)')
  assert.equal(gate.resolver, 'resolve_current_solution_review')
  assert.equal(gate.consumer, 'project_current_solution_review')
  assert.deepEqual([...gate.requiredTests].sort(), REQUIRED_RUST_TESTS)

  const trigger = repositoryPath(gate.activation.trigger)
  assert.equal(existsSync(trigger), true)

  const rules = json(rulesPath)
  for (const finding of rules.closedFindings) assert.equal(finding.status, 'closed')
  const rawApplicationSource = readFileSync(repositoryPath(gate.applicationPath), 'utf8')
  const rawProjectionSource = readFileSync(repositoryPath(gate.projectionPath), 'utf8')
  const applicationSource = stripRustComments(rawApplicationSource)
  const projectionSource = stripRustComments(rawProjectionSource)
  const combinedSource = `${applicationSource}\n${projectionSource}`
  const factBody = namedRustStruct(applicationSource, gate.factTypeName, 'crate')
  assert.deepEqual(
    rustStructFields(factBody).sort(),
    [...gate.requiredFactPrivateFields].sort(),
    'ValidatedSolutionReviewSet authority fields changed',
  )
  assert.doesNotMatch(
    factBody,
    /(?:^|\n)\s*pub(?:\([^)]*\))?\s+[a-z_][a-z0-9_]*\s*:/u,
    'ValidatedSolutionReviewSet fields must remain private',
  )
  const projectionBody = namedRustStruct(projectionSource, gate.projectionTypeName)
  assert.deepEqual(
    rustStructFields(projectionBody).sort(),
    [...gate.requiredProjectionPrivateFields].sort(),
    'SolutionReviewProjection public JSON fields changed outside the allowlist',
  )
  const proposalBody = namedRustStruct(applicationSource, gate.taskProposalTypeName, 'crate')
  assert.deepEqual(
    rustStructFields(proposalBody).sort(),
    [...gate.requiredTaskProposalPrivateFields].sort(),
  )

  const factDeclaration = applicationSource.slice(
    Math.max(0, applicationSource.indexOf(`pub(crate) struct ${gate.factTypeName}`) - 400),
    applicationSource.indexOf(`pub(crate) struct ${gate.factTypeName}`),
  )
  assert.doesNotMatch(factDeclaration, /Deserialize/u)
  assert.doesNotMatch(
    applicationSource,
    new RegExp(`impl(?:<[^>]+>)?\\s+[^\\n{]*Deserialize[^\\n{]*for\\s+${gate.factTypeName}\\b`, 'u'),
  )
  assert.doesNotMatch(
    applicationSource,
    new RegExp(
      `impl\\s+${gate.factTypeName}\\s*\\{[\\s\\S]*?\\bpub(?:\\(crate\\))?\\s+fn\\s+(?:new|parse|decode|from_raw|try_from_raw|from_context)\\b`,
      'u',
    ),
    'ValidatedSolutionReviewSet must not expose a public raw constructor',
  )
  assert.match(
    applicationSource,
    new RegExp(`pub\\(crate\\)\\s+fn\\s+${gate.resolver}\\b`, 'u'),
  )
  assert.match(
    projectionSource,
    new RegExp(`pub\\(super\\)\\s+fn\\s+${gate.consumer}\\b`, 'u'),
  )
  assert.match(applicationSource, /#\[serde\([^\]]*deny_unknown_fields[^\]]*\)\]/u)

  const deliveryProjectionSource = stripRustComments(readFileSync(
    repositoryPath(gate.deliveryProjectionPath),
    'utf8',
  ))
  const deliveryProjectionBody = namedRustStruct(deliveryProjectionSource, 'DeliveryProjection')
  assert.match(deliveryProjectionBody, /solution_review\s*:\s*Option<SolutionReviewProjection>/u)
  assert.doesNotMatch(deliveryProjectionBody, /(?:^|\n)\s*solution\s*:/u)
  assert.doesNotMatch(combinedSource, /\bApprovedSolutionReviewSet\b/u)
  assert.doesNotMatch(combinedSource, /\bwith_approved_solution\b/u)
  for (const legacy of gate.prohibitedLegacyWireMarkers) {
    assert.equal(
      `${rawApplicationSource}\n${rawProjectionSource}`.includes(legacy),
      false,
      `legacy wire marker ${legacy} remains`,
    )
  }
  for (const field of gate.prohibitedFields) {
    assert.doesNotMatch(combinedSource, new RegExp(`\\b${field}\\s*:`, 'u'))
  }
  assert.match(rawApplicationSource, /compile_fail/u)
  assert.match(
    rawApplicationSource,
    /ValidatedSolutionReviewSet[\s\S]*serde_json::from_str/u,
  )
  assert.match(combinedSource, /#\[cfg\(test\)\]\s*mod\s+tests\s*\{/u)
  assert.doesNotMatch(
    combinedSource,
    /\bpub(?:\([^)]*\))?\s+fn\s+validated_solution_review_set\b/u,
  )

  const testBodies = new Map(
    gate.requiredTests.map(name => [name, namedRustFunction(combinedSource, name)]),
  )
  for (const name of [
    'human_solution_review_rejects_execution_session_binding',
    'solution_review_digest_covers_ordered_task_proposals',
    'solution_review_resolver_rejects_duplicate_current_review',
    'solution_review_resolver_rejects_stale_or_foreign_authority',
    'solution_review_v1_rejects_unknown_keys_and_legacy_v2',
  ]) assertRejected(testBodies.get(name), name)

  const pending = testBodies.get(
    'pending_solution_review_projects_safe_solution_and_non_empty_ordered_task_proposals',
  )
  for (const token of ['Pending', 'task_proposals', 'is_none', 'solution_review']) {
    assert.match(pending, new RegExp(token, 'u'))
  }
  const settled = testBodies.get(
    'settled_solution_review_projects_exact_decision_reviewer_and_time',
  )
  for (const token of ['Approved', 'ChangesRequested', 'Rejected', 'reviewer_id', 'reviewed_at']) {
    assert.match(settled, new RegExp(token, 'u'))
  }
  const promotion = testBodies.get('only_approved_solution_review_authorizes_task_promotion')
  for (const token of ['Approved', 'Pending', 'ChangesRequested', 'Rejected']) {
    assert.match(promotion, new RegExp(token, 'u'))
  }
  const digest = testBodies.get('solution_review_digest_covers_ordered_task_proposals')
  assert.match(digest, /task_proposals/u)
  assert.match(digest, /swap|reverse|order/u)
  const human = testBodies.get('human_solution_review_rejects_execution_session_binding')
  assert.match(human, /session_bindings/u)
  assert.match(human, /ExecutionJobId/u)
  const redaction = testBodies.get(
    'solution_review_projection_excludes_raw_context_resolution_secrets_tools_and_logs',
  )
  for (const token of ['context', 'resolution', 'secret', 'toolPayload', 'rawRuntimeLog']) {
    assert.match(redaction, new RegExp(token, 'u'))
  }
  assert.match(redaction, /assert!\s*\(\s*!encoded\.contains/u)
  const restart = testBodies.get('solution_review_v1_restart_parse_is_byte_stable')
  assert.match(restart, /encode|encoded|bytes/u)
  assert.match(restart, /restart|reparse|decode/u)
})
