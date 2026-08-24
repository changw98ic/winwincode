import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const contractPath = join(
  root,
  'docs',
  'contracts',
  'delivery-evidence-verdict-rework.md',
)
const rulesPath = join(
  root,
  'docs',
  'contracts',
  'delivery-evidence-verdict-rework.rules.json',
)

const REQUIRED_RULE_IDS = Object.freeze([
  'attention.combined_actions_use_safest_transition',
  'candidate.freeze_exact_writer_facts',
  'candidate.git_snapshot_is_rebuilt',
  'candidate.invalidated_by_spec_or_writer_change',
  'candidate.producer_outcome_artifact_exact',
  'evidence.current_candidate',
  'evidence.current_session_binding',
  'evidence.current_spec_revision',
  'evidence.current_stage_run',
  'evidence.accepted_runtime_ledger_only',
  'evidence.runtime_checkout_matches_candidate',
  'evidence.source_identity_exact',
  'evidence.source_time_within_stage_and_terminal',
  'rework.attempt_uses_total_delivery_history',
  'rework.bounded_remediator_only',
  'rework.invalidates_previous_candidate',
  'rework.precise_current_candidate_scope',
  'rework.result_stays_within_approved_scope',
  'rework.repeated_or_exhausted_failure_clarifies',
  'verdict.agent_message_cannot_pass',
  'verdict.all_criteria_exactly_once',
  'verdict.conflict_is_inconclusive',
  'verdict.derived_facts_are_recomputed',
  'verdict.direct_evidence_controls_classification',
  'verdict.environment_failure_is_infra_error',
  'verdict.failed_check_cannot_pass',
  'verdict.insufficient_evidence_is_inconclusive',
  'verdict.missing_verification_method_is_inconclusive',
  'verdict.missing_session_is_inconclusive',
  'verdict.pass_or_fail_requires_evidence',
  'verdict.required_roles_agree_on_product_outcome',
  'verification.read_only_candidate_policy',
  'verification.reviewer_and_verifier_required',
  'verification.role_sessions_are_independent',
  'verification.stage_and_runtime_terminal_agree',
  'verification.successful_candidate_write_rejected',
].sort())

const CANDIDATE_IDENTITY_FIELDS = Object.freeze([
  'baseCommitId',
  'baseRevision',
  'baseTreeId',
  'candidateCommitId',
  'candidateTreeId',
  'changedPaths',
  'deliveryId',
  'deliverySpecId',
  'deliverySpecRevision',
  'diffSha256',
  'producerAttempt',
  'producerCodexThreadId',
  'producerDeliveryTaskId',
  'producerExecutionJobId',
  'producerProductSessionId',
  'producerRole',
  'producerSessionBindingId',
  'producerStage',
  'producerStageRunId',
  'producerWorkerSessionId',
  'repository',
].sort())

const PERSISTED_EVIDENCE_BINDINGS = Object.freeze([
  'candidateRef',
  'deliveryId',
  'deliverySpecId',
  'deliverySpecRevision',
  'sessionBindingId',
  'sourceRef',
  'stageRunId',
  'type',
].sort())

const RUNTIME_SOURCE_IDENTITY = Object.freeze([
  'attempt',
  'codexThreadId',
  'executionJobId',
  'fencingToken',
  'leaseId',
  'productSessionId',
  'roleId',
  'sourceSequence',
  'stageRunId',
  'terminalLastEventSequence',
  'workerId',
  'workerInstanceId',
  'workerSessionId',
].sort())

const PROHIBITED_RUST_SESSION_FIELDS = Object.freeze([
  'codexSessionId',
  'codex_session_id',
  'dshSessionId',
  'dsh_session_id',
].sort())

const SEALED_FACT_AVAILABILITY = Object.freeze({
  AcceptedRuntimeSourceFact: 'follow-up-adapter',
  ValidatedCheckoutAttestationFact: 'follow-up-adapter',
  ValidatedGitSnapshotFact: 'follow-up-adapter',
  VerifiedTerminalOutcome: 'phase-2.3-extension',
})

const ADAPTER_BACKED_RULE_IDS = Object.freeze([
  'candidate.freeze_exact_writer_facts',
  'candidate.git_snapshot_is_rebuilt',
  'candidate.producer_outcome_artifact_exact',
  'evidence.accepted_runtime_ledger_only',
  'evidence.runtime_checkout_matches_candidate',
  'evidence.source_identity_exact',
  'evidence.source_time_within_stage_and_terminal',
  'rework.result_stays_within_approved_scope',
  'verdict.agent_message_cannot_pass',
  'verdict.direct_evidence_controls_classification',
  'verdict.failed_check_cannot_pass',
  'verdict.pass_or_fail_requires_evidence',
  'verification.stage_and_runtime_terminal_agree',
  'verification.successful_candidate_write_rejected',
].sort())

const PRECISE_REWORK_BINDINGS = Object.freeze([
  'candidateRef',
  'deliveryTaskId',
  'diagramId',
  'diffSha256',
  'evidenceRefIds',
  'filePath',
  'hunkSha256',
  'nodeId',
].sort())

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function repositoryPath(relativePath) {
  assert.equal(relativePath.startsWith('/'), false, `${relativePath} must be repository-relative`)
  assert.equal(relativePath.split('/').includes('..'), false, `${relativePath} must not escape the repository`)
  return join(root, relativePath)
}

function assertPublicSymbol(mapping) {
  const source = readFileSync(repositoryPath(mapping.path), 'utf8')
  assert.match(
    source,
    new RegExp(`export\\s+(?:const|type|class|interface|function)\\s+${mapping.name}\\b`, 'u'),
    `${mapping.path} does not export ${mapping.name}`,
  )
}

function assertTestCase(mapping) {
  const source = readFileSync(repositoryPath(mapping.path), 'utf8')
  assert.equal(
    source.includes(`test('${mapping.name}'`) || source.includes(`test("${mapping.name}"`),
    true,
    `${mapping.path} does not define test ${mapping.name}`,
  )
}

test('phase 2.4 rules freeze candidate, evidence, verdict, and rework behavior', () => {
  const rules = json(rulesPath)
  assert.equal(rules.schemaVersion, 'winwincode.delivery-evidence-verdict-rework-rules.v1')
  assert.equal(rules.issueId, 'winwincode-9c4.16.2.4')
  assert.equal(rules.rustTarget.crate, 'crates/winwincode-delivery')
  assert.equal(rules.rustTarget.markerModule, 'domain/candidate.rs')
  assert.equal(rules.rustTarget.scanWhenMarkerPresent, true)
  assert.deepEqual(
    [...rules.rustTarget.prohibitedIdentityFields].sort(),
    PROHIBITED_RUST_SESSION_FIELDS,
  )
  assert.match(rules.implementationBoundary.phase24DomainPolicy, /pure fail-closed domain/u)
  assert.match(
    rules.implementationBoundary.phase24DomainPolicy,
    /cannot construct a sealed fact/u,
  )
  assert.match(
    rules.implementationBoundary.productionEnablementPolicy,
    /crate-private test support/u,
  )
  assert.match(
    rules.implementationBoundary.productionEnablementPolicy,
    /remain unavailable until/u,
  )
  const sealedFactAvailability = Object.fromEntries(
    rules.implementationBoundary.adapterDependencies.map(dependency => [
      dependency.factType,
      dependency.availability,
    ]),
  )
  assert.deepEqual(sealedFactAvailability, SEALED_FACT_AVAILABILITY)
  const adapterBackedRuleIds = [
    ...new Set(
      rules.implementationBoundary.adapterDependencies.flatMap(
        dependency => dependency.ruleIds,
      ),
    ),
  ].sort()
  assert.deepEqual(adapterBackedRuleIds, ADAPTER_BACKED_RULE_IDS)
  for (const dependency of rules.implementationBoundary.adapterDependencies) {
    assert.ok(dependency.owner.length > 0, `${dependency.factType} needs an adapter owner`)
    assert.ok(dependency.produces.length > 0, `${dependency.factType} needs a fact contract`)
    assert.match(dependency.integrationGate, /^[a-z][a-z0-9_]+$/u)
  }

  assert.deepEqual([...rules.candidate.identityFields].sort(), CANDIDATE_IDENTITY_FIELDS)
  assert.deepEqual(rules.candidate.invalidators, [
    'delivery-spec-revision-changed',
    'candidate-facts-changed',
    'later-executing-or-reworking-writer-started',
  ])
  assert.deepEqual(
    [...rules.evidence.persistedBindings].sort(),
    PERSISTED_EVIDENCE_BINDINGS,
  )
  assert.deepEqual(
    [...rules.evidence.runtimeSourceIdentity].sort(),
    RUNTIME_SOURCE_IDENTITY,
  )
  assert.deepEqual(rules.verification.requiredRoles, ['reviewer', 'verifier'])
  assert.equal(rules.verification.workspaceMode, 'candidate-read-only')
  assert.match(rules.verification.assignmentPolicy, /exactly one current assignment/u)

  assert.deepEqual(rules.verdict.failClosedClassifications, {
    agent_claim_conflicts_with_direct_outcome: 'inconclusive',
    agent_message_only: 'inconclusive',
    failed_check_claimed_as_pass: 'inconclusive',
    insufficient_direct_evidence: 'inconclusive',
    missing_verification_method: 'inconclusive',
    missing_required_session: 'inconclusive',
    required_role_product_outcome_disagreement: 'inconclusive',
    reviewer_verifier_conflict: 'inconclusive',
    runtime_environment_failure: 'infra_error',
    stage_runtime_terminal_mismatch: 'inconclusive',
    unattested_candidate_checkout: 'inconclusive',
  })
  assert.deepEqual(rules.verdict.directPassEvidenceTypes, [
    'test',
    'command',
    'diff',
    'file',
    'commit',
  ])
  assert.deepEqual(rules.verdict.requiredResultPrecedence, [
    'fail',
    'infra_error',
    'inconclusive',
    'pass',
  ])
  assert.deepEqual([...rules.rework.preciseBindings].sort(), PRECISE_REWORK_BINDINGS)
  assert.equal(rules.rework.attemptLimitSource, 'DeliverySpec.maxReworkAttempts')
  assert.equal(rules.rework.writerRole, 'remediator')
  assert.match(rules.candidate.factSourcePolicy, /sealed ValidatedGitSnapshotFact/u)
  assert.match(rules.candidate.factSourcePolicy, /cannot itself read Git/u)
  assert.match(rules.evidence.candidateCheckoutPolicy, /ValidatedCheckoutAttestationFact/u)
  assert.match(rules.evidence.ledgerPolicy, /persisted source positions/u)
  assert.match(rules.verification.candidateIntegrityPolicy, /starts and ends/u)
  assert.match(rules.verification.terminalPolicy, /sealed VerifiedTerminalOutcome/u)
  assert.match(rules.verdict.requiredRoleAgreementPolicy, /every required role/u)
  assert.match(rules.rework.resultPolicy, /rejects any added path/u)
  assert.equal(
    rules.rework.attemptSequenceSource,
    'all current Delivery StageRuns whose stage is reworking',
  )

  const ruleIds = rules.rules.map(rule => rule.id)
  assert.equal(new Set(ruleIds).size, ruleIds.length)
  assert.deepEqual([...ruleIds].sort(), REQUIRED_RULE_IDS)
  for (const rule of rules.rules) {
    assert.match(rule.id, /^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+$/u)
    assert.ok(rule.statement.length > 0, `${rule.id} needs a statement`)
    assert.ok(rule.adrRefs.length > 0, `${rule.id} needs an accepted decision`)
    for (const path of rule.adrRefs) assert.equal(existsSync(repositoryPath(path)), true)

    assert.ok(rule.typescript.publicSymbols.length > 0, `${rule.id} needs an old public seam`)
    for (const mapping of rule.typescript.publicSymbols) assertPublicSymbol(mapping)
    assert.ok(['covered', 'partial'].includes(rule.typescript.coverage))
    assert.ok(rule.typescript.tests.length > 0, `${rule.id} needs an old behavior test`)
    for (const mapping of rule.typescript.tests) assertTestCase(mapping)
    if (rule.typescript.coverage === 'partial') {
      assert.ok(rule.typescript.gap.length > 0, `${rule.id} must state the old behavior gap`)
    } else {
      assert.equal(rule.typescript.gap, '')
    }

    assert.match(rule.rust.module, /^domain\/(?:candidate|evidence|rework|verdict|verification)\.rs$/u)
    assert.match(rule.rust.testName, /^[a-z][a-z0-9_]+$/u)
  }
})

test('plain-language phase 2.4 contract and machine rules stay paired', () => {
  const contract = readFileSync(contractPath, 'utf8')
  const rules = json(rulesPath)
  assert.match(contract, /^# Rust Delivery 候选、证据、结论与返工合同$/mu)
  assert.match(contract, /缺少必需 Session.*`inconclusive`/u)
  assert.match(contract, /运行环境失败.*`infra_error`/u)
  assert.match(contract, /失败的测试.*不能.*`pass`/u)
  assert.match(contract, /ProductSession.*WorkerSession.*CodexThread/u)
  for (const rule of rules.rules) {
    assert.equal(contract.includes(`\`${rule.id}\``), true, `${rule.id} is absent from the prose`)
  }
})

test('phase 2.4 Rust marker turns the planned rule matrix into an implementation gate', () => {
  const rules = json(rulesPath)
  const crateRoot = repositoryPath(rules.rustTarget.crate)
  const marker = join(crateRoot, 'src', rules.rustTarget.markerModule)
  if (!existsSync(marker)) {
    assert.equal(rules.rustTarget.scanWhenMarkerPresent, true)
    return
  }

  const scannedModules = new Set()
  for (const rule of rules.rules) {
    const modulePath = join(crateRoot, 'src', rule.rust.module)
    assert.equal(existsSync(modulePath), true, `${rule.id} target module is missing`)
    const source = readFileSync(modulePath, 'utf8')
    assert.match(
      source,
      new RegExp(`\\bfn\\s+${rule.rust.testName}\\s*\\(`, 'u'),
      `${rule.id} target test ${rule.rust.testName} is missing`,
    )
    scannedModules.add(modulePath)
  }
  for (const modulePath of scannedModules) {
    const source = readFileSync(modulePath, 'utf8')
    for (const legacyField of rules.rustTarget.prohibitedIdentityFields) {
      assert.equal(
        source.includes(legacyField),
        false,
        `${modulePath} restores prohibited identity field ${legacyField}`,
      )
    }
  }
})
