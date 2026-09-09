// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use sha2::{Digest as _, Sha256};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DebugHypothesisId, DebugSessionId, ExecutionJobId, FencingToken,
    Instant, LeaseId, ProbeId, ProbeRoundId, ProductSessionId, RepositoryId, SessionIdentity,
    Sha256Digest, WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::{
    debug_hypothesis_ledger::{
        DebugHypothesisLedgerReducer, ValidatedDebugHypothesisRoundEvidence,
        canonical_probe_round_receipt_bytes, seal_debug_hypothesis_round_evidence,
    },
    debug_probe_contract::{
        ValidatedDebugProbePlan, ValidatedProbeExecutionIntent, derive_debug_probe_plan_digest,
        derive_probe_budget_digest, derive_probe_command_arg_bytes, derive_probe_definition_digest,
        seal_debug_probe_plan, seal_probe_execution_intent,
    },
    debug_probe_delta_context::{
        ContextSafetyScanError, DebugContextSafetyScanner, DebugContextSnippetInput,
        DebugDeltaContextError, DebugProbeDeltaContextInput, canonical_debug_delta_context_budget,
        prepare_debug_probe_delta_context, reopen_debug_probe_delta_context,
        seal_debug_context_snippet, validate_debug_probe_delta_context_evidence,
    },
    generated::{
        ArtifactReference, DebugContextSafetyProfile, DebugContextSafetyScannerVersion,
        DebugDeltaContextEstimatorVersion, DebugHypothesis, DebugHypothesisEvidenceAssessment,
        DebugHypothesisEvidencePolarity, DebugHypothesisLedgerSeed, DebugHypothesisLedgerUpdate,
        DebugHypothesisMutation, DebugHypothesisMutationKind, DebugHypothesisStatus,
        DebugProbeKind, DebugProbePlan, DebugProbeRoundAuthority, DebugSessionStatus,
        ProbeCommandSpec, ProbeCompletionRule, ProbeCompletionRuleKind, ProbeExecutionIntent,
        ProbeExecutionReceipt, ProbeNetworkAccess, ProbeNormalizerProfile, ProbeNormalizerVersion,
        ProbeRawStream, ProbeReceiptStatus, ProbeResourceClaim, ProbeRoundBudget,
        ProbeRoundBudgetUsage, ProbeRoundCompletionReason, ProbeRoundReceipt,
        ProbeRoundReceiptStatus, ProbeSideEffectClass, ProbeSpec, ProbeWorkspaceAccess,
    },
    probe_result_normalizer::{
        ProbeRawStreamInput, canonical_probe_evidence_bundle_bytes,
        derive_probe_normalizer_profile_digest, normalize_probe_evidence,
        probe_baseline_not_applicable, project_probe_evidence, seal_probe_normalizer_profile,
    },
};

struct AcceptingScanner(DebugContextSafetyProfile);

impl DebugContextSafetyScanner for AcceptingScanner {
    fn profile(&self) -> &DebugContextSafetyProfile {
        &self.0
    }

    fn validate(&self, _text: &str) -> Result<(), ContextSafetyScanError> {
        Ok(())
    }
}

struct PolicyScanner(DebugContextSafetyProfile);

impl DebugContextSafetyScanner for PolicyScanner {
    fn profile(&self) -> &DebugContextSafetyProfile {
        &self.0
    }

    fn validate(&self, text: &str) -> Result<(), ContextSafetyScanError> {
        if text.contains("secret") {
            Err(ContextSafetyScanError::SensitiveMaterial)
        } else if text.contains("raw log") {
            Err(ContextSafetyScanError::RawContextMaterial)
        } else {
            Ok(())
        }
    }
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn content_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn artifact(character: char, bytes: &[u8]) -> ArtifactReference {
    ArtifactReference {
        artifact_id: ArtifactId(format!("art_{}", character.to_string().repeat(26))),
        digest: content_digest(bytes),
    }
}

fn authority() -> DebugProbeRoundAuthority {
    DebugProbeRoundAuthority {
        attempt: 1,
        debug_session_id: DebugSessionId("dbg_00000000000000000000000000".to_owned()),
        environment_digest: digest('1'),
        fencing_token: FencingToken("7".to_owned()),
        job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
        lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
        repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
        round_id: ProbeRoundId("prn_00000000000000000000000000".to_owned()),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
            product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
            work_run_id: None,
            worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
        },
        workspace_revision: WorkspaceRevision(format!("git-tree:{}", "0".repeat(40))),
    }
}

fn sealed_plan_and_intent() -> (ValidatedDebugProbePlan, ValidatedProbeExecutionIntent) {
    sealed_plan_and_intent_for(authority())
}

fn sealed_plan_and_intent_for(
    round_authority: DebugProbeRoundAuthority,
) -> (ValidatedDebugProbePlan, ValidatedProbeExecutionIntent) {
    let mut plan = DebugProbePlan {
        authority: round_authority,
        budget: ProbeRoundBudget {
            budget_digest: digest('2'),
            parallel_probe_limit: 1,
            peak_memory_limit_bytes: 134_217_728,
            probe_limit: 1,
            total_command_arg_limit_bytes: 262_144,
            total_cpu_limit_millis: 10_000,
            total_output_limit_bytes: 1_024,
            wall_time_limit_millis: 300_000,
        },
        completion_rule: ProbeCompletionRule {
            kind: ProbeCompletionRuleKind::AllTerminal,
            minimum_completed_probes: 1,
            minimum_successful_probes: 1,
            stop_on_required_probe_failure: true,
        },
        created_at: Instant("2026-09-07T08:00:00.000Z".to_owned()),
        plan_digest: digest('3'),
        probes: vec![ProbeSpec {
            command: ProbeCommandSpec {
                argv: vec!["fixture-probe".to_owned()],
                command_arg_bytes: 1,
                working_directory: ".".to_owned(),
            },
            kind: DebugProbeKind::StaticAnalysis,
            output_limit_bytes: 1_024,
            probe_definition_digest: digest('4'),
            probe_id: ProbeId("prb_00000000000000000000000000".to_owned()),
            required: true,
            resources: ProbeResourceClaim {
                cpu_limit_millis: 10_000,
                database_keys: Vec::new(),
                exclusive_keys: Vec::new(),
                memory_limit_bytes: 134_217_728,
                network_access: ProbeNetworkAccess::None,
                paths: vec!["src".to_owned()],
                port_numbers: Vec::new(),
                service_keys: Vec::new(),
                side_effect_class: ProbeSideEffectClass::PureRead,
                workspace_access: ProbeWorkspaceAccess::ReadOnly,
            },
            target_hypothesis_ids: vec![DebugHypothesisId(
                "hyp_00000000000000000000000000".to_owned(),
            )],
            timeout_millis: 300_000,
        }],
        schema_version: 1,
    };
    plan.probes[0].command.command_arg_bytes =
        derive_probe_command_arg_bytes(&plan.probes[0].command.argv).expect("command bytes");
    plan.probes[0].probe_definition_digest =
        derive_probe_definition_digest(&plan.probes[0]).expect("probe digest");
    plan.budget.budget_digest = derive_probe_budget_digest(&plan.budget).expect("budget digest");
    plan.plan_digest = derive_debug_probe_plan_digest(&plan).expect("plan digest");
    let plan = seal_debug_probe_plan(plan.clone(), &plan.authority).expect("sealed plan");
    let intent = seal_probe_execution_intent(
        ProbeExecutionIntent {
            created_at: Instant("2026-09-07T08:00:01.000Z".to_owned()),
            identity: plan
                .probe_identity(&plan.probes()[0].spec().probe_id)
                .expect("probe identity"),
            plan_digest: plan.plan().plan_digest.clone(),
            schema_version: 1,
            spec: plan.probes()[0].spec().clone(),
        },
        &plan,
    )
    .expect("sealed intent");
    (plan, intent)
}

fn next_authority() -> DebugProbeRoundAuthority {
    let mut value = authority();
    value.job_id = ExecutionJobId("job_11111111111111111111111111".to_owned());
    value.lease_id = LeaseId("lse_11111111111111111111111111".to_owned());
    value.round_id = ProbeRoundId("prn_11111111111111111111111111".to_owned());
    value
}

fn empty_round_evidence() -> ValidatedDebugHypothesisRoundEvidence {
    let (plan, intent) = sealed_plan_and_intent();
    let probe_receipt = ProbeExecutionReceipt {
        artifact_refs: Vec::new(),
        duration_millis: 10,
        error: None,
        exit_code: Some(0),
        finished_at: Instant("2026-09-07T08:00:03.000Z".to_owned()),
        identity: intent.intent().identity.clone(),
        output_bytes: 0,
        output_truncated: false,
        plan_digest: plan.plan().plan_digest.clone(),
        schema_version: 1,
        signal: None,
        started_at: Instant("2026-09-07T08:00:02.000Z".to_owned()),
        status: ProbeReceiptStatus::Succeeded,
        timed_out: false,
    };
    let receipt = ProbeRoundReceipt {
        authority: plan.plan().authority.clone(),
        completion_reason: ProbeRoundCompletionReason::AllProbesTerminal,
        error: None,
        finished_at: Instant("2026-09-07T08:00:04.000Z".to_owned()),
        plan_digest: plan.plan().plan_digest.clone(),
        probe_receipts: vec![probe_receipt],
        reducer: None,
        schema_version: 1,
        started_at: Instant("2026-09-07T08:00:01.000Z".to_owned()),
        status: ProbeRoundReceiptStatus::Completed,
        usage: ProbeRoundBudgetUsage {
            budget_digest: plan.plan().budget.budget_digest.clone(),
            elapsed_millis: 3_000,
            peak_memory_bytes: 100,
            peak_parallel_probes: 1,
            probe_count: 1,
            total_command_arg_bytes: intent.probe().command_arg_bytes(),
            total_cpu_millis: 10,
            total_output_bytes: 0,
        },
    };
    let receipt_bytes = canonical_probe_round_receipt_bytes(&receipt).expect("receipt bytes");
    seal_debug_hypothesis_round_evidence(
        &plan,
        &receipt,
        ArtifactReference {
            artifact_id: ArtifactId("art_00000000000000000000000000".to_owned()),
            digest: content_digest(&receipt_bytes),
        },
        &[],
    )
    .expect("empty round evidence")
}

fn projected_round_evidence(
    round_authority: DebugProbeRoundAuthority,
) -> ValidatedDebugHypothesisRoundEvidence {
    let output = b"ok";
    let (plan, intent) = sealed_plan_and_intent_for(round_authority);
    let raw_ref = artifact('A', output);
    let probe_receipt = ProbeExecutionReceipt {
        artifact_refs: vec![raw_ref.clone()],
        duration_millis: 10,
        error: None,
        exit_code: Some(0),
        finished_at: Instant("2026-09-07T08:00:03.000Z".to_owned()),
        identity: intent.intent().identity.clone(),
        output_bytes: i64::try_from(output.len()).expect("output bytes"),
        output_truncated: false,
        plan_digest: plan.plan().plan_digest.clone(),
        schema_version: 1,
        signal: None,
        started_at: Instant("2026-09-07T08:00:02.000Z".to_owned()),
        status: ProbeReceiptStatus::Succeeded,
        timed_out: false,
    };
    let mut profile = ProbeNormalizerProfile {
        diagnostic_parser_version: None,
        normalizer_version: ProbeNormalizerVersion::L0L1V1,
        profile_digest: digest('5'),
        stack_parser_version: None,
    };
    profile.profile_digest =
        derive_probe_normalizer_profile_digest(&profile).expect("profile digest");
    let profile = seal_probe_normalizer_profile(profile).expect("normalizer profile");
    let raw = [ProbeRawStreamInput::new(
        ProbeRawStream::Stdout,
        raw_ref,
        output,
    )];
    let bundle = normalize_probe_evidence(
        &intent,
        &probe_receipt,
        &profile,
        &raw,
        &probe_baseline_not_applicable(),
        Path::new("/workspace"),
    )
    .expect("normalized evidence");
    let bundle_bytes = canonical_probe_evidence_bundle_bytes(&bundle).expect("bundle bytes");
    let projection =
        project_probe_evidence(&bundle, artifact('B', &bundle_bytes)).expect("evidence projection");
    let receipt = ProbeRoundReceipt {
        authority: plan.plan().authority.clone(),
        completion_reason: ProbeRoundCompletionReason::AllProbesTerminal,
        error: None,
        finished_at: Instant("2026-09-07T08:00:04.000Z".to_owned()),
        plan_digest: plan.plan().plan_digest.clone(),
        probe_receipts: vec![probe_receipt],
        reducer: None,
        schema_version: 1,
        started_at: Instant("2026-09-07T08:00:01.000Z".to_owned()),
        status: ProbeRoundReceiptStatus::Completed,
        usage: ProbeRoundBudgetUsage {
            budget_digest: plan.plan().budget.budget_digest.clone(),
            elapsed_millis: 3_000,
            peak_memory_bytes: 100,
            peak_parallel_probes: 1,
            probe_count: 1,
            total_command_arg_bytes: intent.probe().command_arg_bytes(),
            total_cpu_millis: 10,
            total_output_bytes: i64::try_from(output.len()).expect("output bytes"),
        },
    };
    let receipt_bytes = canonical_probe_round_receipt_bytes(&receipt).expect("receipt bytes");
    seal_debug_hypothesis_round_evidence(
        &plan,
        &receipt,
        artifact('R', &receipt_bytes),
        &[projection],
    )
    .expect("projected round evidence")
}

fn initialized_ledger() -> DebugHypothesisLedgerReducer {
    DebugHypothesisLedgerReducer::initialize(DebugHypothesisLedgerSeed {
        authority: authority(),
        created_at: Instant("2026-09-07T07:59:59.000Z".to_owned()),
        hypotheses: vec![DebugHypothesis {
            confidence_bps: 0,
            contradicting_evidence: Vec::new(),
            created_round_id: authority().round_id.clone(),
            hypothesis_id: DebugHypothesisId("hyp_00000000000000000000000000".to_owned()),
            last_updated_round_id: authority().round_id,
            status: DebugHypothesisStatus::Active,
            summary: "Revision validation rejects a valid snapshot".to_owned(),
            supporting_evidence: Vec::new(),
        }],
        unresolved_questions: Vec::new(),
    })
    .expect("initialized ledger")
}

fn scanner() -> AcceptingScanner {
    AcceptingScanner(DebugContextSafetyProfile {
        scanner_policy_digest: digest('a'),
        scanner_version: DebugContextSafetyScannerVersion::WorkspaceSecretScanV1,
    })
}

fn policy_scanner() -> PolicyScanner {
    PolicyScanner(scanner().0)
}

#[test]
fn canonical_budget_is_fixed_and_self_authenticated() {
    let budget = canonical_debug_delta_context_budget();

    assert_eq!(budget.max_serialized_bytes, 32_768);
    assert_eq!(budget.max_estimated_tokens, 8_192);
    assert_eq!(budget.max_snippets, 8);
    assert_eq!(budget.max_snippet_bytes, 2_000);
    assert_eq!(budget.max_total_snippet_bytes, 16_000);
    assert_eq!(
        budget.estimator_version,
        DebugDeltaContextEstimatorVersion::Utf8BytesV1
    );
    assert_eq!(
        budget.policy_digest.0,
        "sha256:65a2fc8b4f4feca8a774637b32814b8ed768da724504a535d1f5f247296f5051"
    );
}

#[test]
fn snippet_seal_binds_exact_whole_utf8_content_and_host_scanner() {
    let sealed = seal_debug_context_snippet(
        DebugContextSnippetInput {
            content: "let result = validate_revision();".to_owned(),
            end_line: Some(42),
            path: Some("src/lib.rs".to_owned()),
            source_artifact_ref: ArtifactReference {
                artifact_id: ArtifactId("art_00000000000000000000000000".to_owned()),
                digest: digest('b'),
            },
            source_evidence_digest: digest('c'),
            start_line: Some(42),
        },
        &scanner(),
    )
    .expect("seal exact safe snippet");

    assert_eq!(
        sealed.snippet().content,
        "let result = validate_revision();"
    );
    assert_eq!(
        sealed.snippet().content_digest.0,
        "sha256:623931390cb0bf4b0b93c687146d74d877eff66993a42ce55c42a6c9f9ee8858"
    );
    assert_eq!(sealed.snippet().start_line, Some(42));
    assert_eq!(sealed.snippet().end_line, Some(42));
    assert_eq!(sealed.snippet().safety_profile, *scanner().profile());
}

#[test]
fn first_round_context_is_exact_seed_delta_and_reopens_without_history() {
    let ledger = initialized_ledger();
    let evidence = empty_round_evidence();
    let prepared = prepare_debug_probe_delta_context(
        &DebugProbeDeltaContextInput {
            current_ledger: ledger.ledger(),
            previous_context: None,
            previous_ledger: None,
            round_evidence: &evidence,
            snippets: &[],
        },
        &scanner(),
    )
    .expect("first prepared delta context");
    let context = prepared.context();

    assert_eq!(
        context.source_request_digest.0,
        "sha256:6f8bc70bf431b2913bf90249c5b6e67295f645110b4797c2d1d09820ff990705"
    );
    assert_eq!(
        context.context_digest.0,
        "sha256:44030b1973e4030695a522089f1c81d2bd9c54d0d52f2570c312708671fc0d80"
    );
    assert_eq!(context.previous_context_digest, None);
    assert_eq!(context.previous_ledger_digest, None);
    assert_eq!(context.hypothesis_changes.len(), 1);
    assert_eq!(context.hypothesis_changes[0].previous_status, None);
    assert_eq!(context.hypothesis_changes[0].previous_confidence_bps, None);
    assert!(context.new_evidence_summaries.is_empty());
    assert!(context.new_evidence_candidates.is_empty());
    assert!(context.snippets.is_empty());
    assert_eq!(
        usize::try_from(context.estimated_token_count).expect("token count"),
        prepared.canonical_bytes().len()
    );
    assert!(prepared.canonical_bytes().len() <= 8_192);
    assert!(!String::from_utf8_lossy(prepared.canonical_bytes()).contains("raw"));

    let reopened =
        reopen_debug_probe_delta_context(prepared.canonical_bytes(), &context.context_digest)
            .expect("exact reopened context");
    assert_eq!(reopened.context(), context);
    assert_eq!(reopened.canonical_bytes(), prepared.canonical_bytes());
}

#[test]
fn current_evidence_candidates_and_whole_snippets_are_order_stable_and_bounded() {
    let ledger = initialized_ledger();
    let evidence = projected_round_evidence(authority());
    let source = &evidence.cut().evidence_candidates[0].evidence;
    let mut snippets = (0..10)
        .map(|index| {
            let content = format!("snippet-{index}-{}", "x".repeat(900));
            let sealed = seal_debug_context_snippet(
                DebugContextSnippetInput {
                    content: content.clone(),
                    end_line: Some(index + 1),
                    path: Some(format!("src/{index}.rs")),
                    source_artifact_ref: source.artifact_ref.clone(),
                    source_evidence_digest: source.evidence_digest.clone(),
                    start_line: Some(index + 1),
                },
                &scanner(),
            )
            .expect("sealed candidate snippet");
            (sealed, content)
        })
        .collect::<Vec<_>>();
    let original_content = snippets
        .iter()
        .map(|(_, content)| content.clone())
        .collect::<Vec<_>>();
    let ordered = snippets
        .iter()
        .map(|(snippet, _)| snippet.clone())
        .collect::<Vec<_>>();
    snippets.reverse();
    let reversed = snippets
        .iter()
        .map(|(snippet, _)| snippet.clone())
        .collect::<Vec<_>>();

    let prepare = |values: &[winwincode_execution_port::debug_probe_delta_context::ValidatedDebugContextSnippet]| {
        prepare_debug_probe_delta_context(
            &DebugProbeDeltaContextInput {
                current_ledger: ledger.ledger(),
                previous_context: None,
                previous_ledger: None,
                round_evidence: &evidence,
                snippets: values,
            },
            &scanner(),
        )
        .expect("prepared evidence delta")
    };
    let first = prepare(&ordered);
    let second = prepare(&reversed);

    assert_eq!(first.canonical_bytes(), second.canonical_bytes());
    assert_eq!(first.context().new_evidence_summaries.len(), 1);
    assert_eq!(first.context().new_evidence_candidates.len(), 1);
    validate_debug_probe_delta_context_evidence(&first, &evidence)
        .expect("Prepared context matches durable evidence cut");
    assert_eq!(
        validate_debug_probe_delta_context_evidence(
            &first,
            &projected_round_evidence(next_authority())
        ),
        Err(DebugDeltaContextError::InvalidInput)
    );
    assert!(first.context().omitted_snippet_count > 0);
    assert_eq!(
        usize::try_from(first.context().omitted_snippet_count).expect("omitted count"),
        10 - first.context().snippets.len()
    );
    assert!(first.context().snippets.len() <= 8);
    assert!(first.canonical_bytes().len() <= 8_192);
    for included in &first.context().snippets {
        assert!(
            original_content
                .iter()
                .any(|value| value == &included.content)
        );
        assert_eq!(included.content.len(), 910);
    }
}

#[test]
fn next_round_contains_only_changes_since_the_durable_context_cursor() {
    let mut reducer = initialized_ledger();
    let previous_ledger = reducer.ledger().clone();
    let first_evidence = projected_round_evidence(authority());
    let first = prepare_debug_probe_delta_context(
        &DebugProbeDeltaContextInput {
            current_ledger: &previous_ledger,
            previous_context: None,
            previous_ledger: None,
            round_evidence: &first_evidence,
            snippets: &[],
        },
        &scanner(),
    )
    .expect("first context");
    let candidate = first_evidence.cut().evidence_candidates[0].clone();
    reducer
        .apply_round(
            DebugHypothesisLedgerUpdate {
                confirmed_facts: Vec::new(),
                mutations: vec![DebugHypothesisMutation {
                    assessments: vec![DebugHypothesisEvidenceAssessment {
                        candidate: candidate.clone(),
                        polarity: DebugHypothesisEvidencePolarity::Supports,
                    }],
                    confidence_bps: 5_000,
                    hypothesis_id: candidate.target_hypothesis_id.clone(),
                    kind: DebugHypothesisMutationKind::Update,
                    previous_confidence_bps: Some(0),
                    previous_status: Some(DebugHypothesisStatus::Active),
                    status: DebugHypothesisStatus::Active,
                    summary: "Revision validation fails after canonicalization".to_owned(),
                }],
                occurred_at: Instant("2026-09-07T08:00:05.000Z".to_owned()),
                opened_questions: Vec::new(),
                previous_ledger_digest: previous_ledger.ledger().ledger_digest.clone(),
                reproduction_recipe_update: None,
                resolved_question_digests: Vec::new(),
                session_status: DebugSessionStatus::Active,
                source_context_digest: first.context().context_digest.clone(),
                source_request_digest: first.context().source_request_digest.clone(),
                source_round_receipt: first_evidence.receipt_reference().clone(),
            },
            &first_evidence,
            &authority(),
        )
        .expect("apply first round");
    let second_evidence = projected_round_evidence(next_authority());
    let second = prepare_debug_probe_delta_context(
        &DebugProbeDeltaContextInput {
            current_ledger: reducer.ledger(),
            previous_context: Some(&first),
            previous_ledger: Some(&previous_ledger),
            round_evidence: &second_evidence,
            snippets: &[],
        },
        &scanner(),
    )
    .expect("second context");

    assert_eq!(
        second.context().previous_context_digest.as_ref(),
        Some(&first.context().context_digest)
    );
    assert_eq!(
        second.context().previous_ledger_digest.as_ref(),
        Some(&previous_ledger.ledger().ledger_digest)
    );
    assert_eq!(second.context().hypothesis_changes.len(), 1);
    let change = &second.context().hypothesis_changes[0];
    assert_eq!(change.previous_confidence_bps, Some(0));
    assert_eq!(change.confidence_bps, 5_000);
    assert_eq!(change.added_supporting_evidence, vec![candidate]);
    assert!(change.added_contradicting_evidence.is_empty());
    assert_eq!(second.context().new_evidence_candidates.len(), 1);
    assert_ne!(
        second.context().new_evidence_candidates[0]
            .evidence
            .identity
            .round_id,
        change.added_supporting_evidence[0]
            .evidence
            .identity
            .round_id
    );
}

#[test]
fn unsafe_or_inexact_snippet_and_token_boundaries_fail_closed() {
    let base = DebugContextSnippetInput {
        content: "bounded source observation".to_owned(),
        end_line: Some(2),
        path: Some("src/lib.rs".to_owned()),
        source_artifact_ref: ArtifactReference {
            artifact_id: ArtifactId("art_00000000000000000000000000".to_owned()),
            digest: digest('b'),
        },
        source_evidence_digest: digest('c'),
        start_line: Some(1),
    };
    let cases = [
        (
            DebugContextSnippetInput {
                content: String::new(),
                ..base.clone()
            },
            DebugDeltaContextError::InvalidText,
        ),
        (
            DebugContextSnippetInput {
                content: "secret credential value".to_owned(),
                ..base.clone()
            },
            DebugDeltaContextError::UnsafeText(ContextSafetyScanError::SensitiveMaterial),
        ),
        (
            DebugContextSnippetInput {
                content: "界".repeat(667),
                ..base.clone()
            },
            DebugDeltaContextError::TextTooLarge,
        ),
        (
            DebugContextSnippetInput {
                path: Some("../secret.rs".to_owned()),
                ..base.clone()
            },
            DebugDeltaContextError::InvalidSnippetLocation,
        ),
        (
            DebugContextSnippetInput {
                end_line: None,
                ..base
            },
            DebugDeltaContextError::InvalidSnippetLocation,
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            seal_debug_context_snippet(input, &policy_scanner())
                .expect_err("invalid snippet must fail"),
            expected
        );
    }

    let mut seed = initialized_ledger().ledger().ledger().clone();
    seed.hypotheses = (0..64)
        .map(|index| DebugHypothesis {
            confidence_bps: 0,
            contradicting_evidence: Vec::new(),
            created_round_id: authority().round_id.clone(),
            hypothesis_id: DebugHypothesisId(format!("hyp_{index:026}")),
            last_updated_round_id: authority().round_id,
            status: DebugHypothesisStatus::Active,
            summary: "x".repeat(500),
            supporting_evidence: Vec::new(),
        })
        .collect();
    let oversized = DebugHypothesisLedgerReducer::initialize(DebugHypothesisLedgerSeed {
        authority: seed.authority,
        created_at: seed.updated_at,
        hypotheses: seed.hypotheses,
        unresolved_questions: Vec::new(),
    })
    .expect("large initialized ledger");
    assert_eq!(
        prepare_debug_probe_delta_context(
            &DebugProbeDeltaContextInput {
                current_ledger: oversized.ledger(),
                previous_context: None,
                previous_ledger: None,
                round_evidence: &empty_round_evidence(),
                snippets: &[],
            },
            &scanner(),
        )
        .expect_err("conservative token upper bound must fail"),
        DebugDeltaContextError::ContextTooLarge
    );
}
