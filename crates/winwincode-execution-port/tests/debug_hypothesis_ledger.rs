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
        DebugHypothesisLedgerApplyOutcome, DebugHypothesisLedgerErrorKind,
        DebugHypothesisLedgerReducer, ValidatedDebugHypothesisRoundEvidence,
        canonical_debug_hypothesis_ledger_event_bytes,
        canonical_debug_hypothesis_round_evidence_bytes, canonical_probe_round_receipt_bytes,
        decode_canonical_debug_hypothesis_ledger_event, derive_debug_confirmed_fact_digest,
        derive_debug_reproduction_recipe_digest, derive_debug_reproduction_recipe_step_digest,
        derive_debug_unresolved_question_digest, reopen_debug_hypothesis_round_evidence,
        seal_debug_hypothesis_round_evidence,
    },
    debug_probe_contract::{
        ValidatedDebugProbePlan, derive_debug_probe_plan_digest, derive_probe_budget_digest,
        derive_probe_command_arg_bytes, derive_probe_definition_digest, seal_debug_probe_plan,
        seal_probe_execution_intent,
    },
    generated::{
        ArtifactReference, DebugConfirmedFact, DebugHypothesis, DebugHypothesisEvidenceAssessment,
        DebugHypothesisEvidencePolarity, DebugHypothesisLedgerSeed, DebugHypothesisLedgerUpdate,
        DebugHypothesisMutation, DebugHypothesisMutationKind, DebugHypothesisStatus,
        DebugProbeKind, DebugProbePlan, DebugProbeRoundAuthority, DebugReproductionRecipe,
        DebugReproductionRecipeStep, DebugReproductionRecipeUpdate, DebugSessionStatus,
        DebugUnresolvedQuestion, HypothesisEvidenceCandidate, ProbeCommandSpec,
        ProbeCompletionRule, ProbeCompletionRuleKind, ProbeExecutionIntent, ProbeExecutionReceipt,
        ProbeNetworkAccess, ProbeNormalizerProfile, ProbeNormalizerVersion, ProbeRawStream,
        ProbeReceiptStatus, ProbeResourceClaim, ProbeRoundBudget, ProbeRoundBudgetUsage,
        ProbeRoundCompletionReason, ProbeRoundReceipt, ProbeRoundReceiptStatus,
        ProbeSideEffectClass, ProbeSpec, ProbeWorkspaceAccess,
    },
    probe_result_normalizer::{
        ProbeEvidenceProjection, ProbeRawStreamInput, derive_probe_normalizer_profile_digest,
        normalize_probe_evidence, probe_baseline_not_applicable, project_probe_evidence,
        seal_probe_normalizer_profile,
    },
};

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

fn hypothesis_id(index: usize) -> DebugHypothesisId {
    DebugHypothesisId(format!("hyp_{index:026}"))
}

fn authority(round: usize) -> DebugProbeRoundAuthority {
    DebugProbeRoundAuthority {
        attempt: 1,
        debug_session_id: DebugSessionId("dbg_00000000000000000000000000".to_owned()),
        environment_digest: digest('1'),
        fencing_token: FencingToken(format!("{}", round + 7)),
        job_id: ExecutionJobId(format!("job_{round:026}")),
        lease_id: LeaseId(format!("lse_{round:026}")),
        repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
        round_id: ProbeRoundId(format!("prn_{round:026}")),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
            product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
            work_run_id: None,
            worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
        },
        workspace_revision: WorkspaceRevision(format!("git-tree:{}", "0".repeat(40))),
    }
}

fn timestamp(second: usize) -> Instant {
    Instant(format!("2026-09-07T00:00:{second:02}.000Z"))
}

fn sealed_plan(round: usize) -> ValidatedDebugProbePlan {
    let mut plan = DebugProbePlan {
        authority: authority(round),
        budget: ProbeRoundBudget {
            budget_digest: digest('2'),
            parallel_probe_limit: 2,
            peak_memory_limit_bytes: 268_435_456,
            probe_limit: 2,
            total_command_arg_limit_bytes: 1024,
            total_cpu_limit_millis: 20_000,
            total_output_limit_bytes: 4096,
            wall_time_limit_millis: 60_000,
        },
        completion_rule: ProbeCompletionRule {
            kind: ProbeCompletionRuleKind::AllTerminal,
            minimum_completed_probes: 2,
            minimum_successful_probes: 2,
            stop_on_required_probe_failure: true,
        },
        created_at: timestamp(round * 10),
        plan_digest: digest('3'),
        probes: (0..2)
            .map(|index| ProbeSpec {
                command: ProbeCommandSpec {
                    argv: vec![format!("fixture-probe-{index}")],
                    command_arg_bytes: 1,
                    working_directory: ".".to_owned(),
                },
                kind: DebugProbeKind::StaticAnalysis,
                output_limit_bytes: 2048,
                probe_definition_digest: digest('4'),
                probe_id: ProbeId(format!("prb_{index:026}")),
                required: true,
                resources: ProbeResourceClaim {
                    cpu_limit_millis: 10_000,
                    database_keys: Vec::new(),
                    exclusive_keys: Vec::new(),
                    memory_limit_bytes: 134_217_728,
                    network_access: ProbeNetworkAccess::None,
                    paths: vec![format!("src/{index}.rs")],
                    port_numbers: Vec::new(),
                    service_keys: Vec::new(),
                    side_effect_class: ProbeSideEffectClass::PureRead,
                    workspace_access: ProbeWorkspaceAccess::ReadOnly,
                },
                target_hypothesis_ids: (1..=3).map(hypothesis_id).collect(),
                timeout_millis: 30_000,
            })
            .collect(),
        schema_version: 1,
    };
    for probe in &mut plan.probes {
        probe.command.command_arg_bytes =
            derive_probe_command_arg_bytes(&probe.command.argv).expect("command bytes");
        probe.probe_definition_digest =
            derive_probe_definition_digest(probe).expect("probe digest");
    }
    plan.budget.budget_digest = derive_probe_budget_digest(&plan.budget).expect("budget digest");
    plan.plan_digest = derive_debug_probe_plan_digest(&plan).expect("plan digest");
    seal_debug_probe_plan(plan.clone(), &plan.authority).expect("sealed plan")
}

fn normalizer_profile()
-> winwincode_execution_port::probe_result_normalizer::ValidatedProbeNormalizerProfile {
    let mut profile = ProbeNormalizerProfile {
        diagnostic_parser_version: None,
        normalizer_version: ProbeNormalizerVersion::L0L1V1,
        profile_digest: digest('5'),
        stack_parser_version: None,
    };
    profile.profile_digest =
        derive_probe_normalizer_profile_digest(&profile).expect("profile digest");
    seal_probe_normalizer_profile(profile).expect("sealed profile")
}

struct RoundFixture {
    plan: ValidatedDebugProbePlan,
    evidence: ValidatedDebugHypothesisRoundEvidence,
    projections: Vec<ProbeEvidenceProjection>,
}

#[derive(Clone, Copy)]
enum MutationCase {
    ContradictionRaises,
    MixedMoves,
    WrongTarget,
    IncompleteTerminal,
    EmptyRaises,
    EmptyTerminal,
    IncompleteCancelledSession,
    IncompleteFailedSession,
    StaleAuthority,
}

fn round_fixture(incomplete_second: bool) -> RoundFixture {
    round_fixture_at(1, incomplete_second)
}

fn round_fixture_at(round: usize, incomplete_second: bool) -> RoundFixture {
    let plan = sealed_plan(round);
    let profile = normalizer_profile();
    let mut receipts = Vec::new();
    let mut projections = Vec::new();
    for index in 0..2 {
        let probe = &plan.probes()[index];
        let intent = seal_probe_execution_intent(
            ProbeExecutionIntent {
                created_at: timestamp(round * 10 + 1),
                identity: plan
                    .probe_identity(&probe.spec().probe_id)
                    .expect("probe identity"),
                plan_digest: plan.plan().plan_digest.clone(),
                schema_version: 1,
                spec: probe.spec().clone(),
            },
            &plan,
        )
        .expect("sealed intent");
        let output = if incomplete_second && index == 1 {
            vec![b'x'; 2_048]
        } else {
            format!("ok-{index}\n").into_bytes()
        };
        let raw_ref = artifact(['A', 'B'][index], &output);
        let receipt = ProbeExecutionReceipt {
            artifact_refs: vec![raw_ref.clone()],
            duration_millis: 10,
            error: None,
            exit_code: Some(0),
            finished_at: timestamp(round * 10 + 3),
            identity: intent.intent().identity.clone(),
            output_bytes: i64::try_from(output.len()).expect("output bytes"),
            output_truncated: incomplete_second && index == 1,
            plan_digest: plan.plan().plan_digest.clone(),
            schema_version: 1,
            signal: None,
            started_at: timestamp(round * 10 + 2),
            status: ProbeReceiptStatus::Succeeded,
            timed_out: false,
        };
        let raw = [ProbeRawStreamInput::new(
            ProbeRawStream::Stdout,
            raw_ref,
            &output,
        )];
        let bundle = normalize_probe_evidence(
            &intent,
            &receipt,
            &profile,
            &raw,
            &probe_baseline_not_applicable(),
            Path::new("/workspace"),
        )
        .expect("normalized evidence");
        let bundle_bytes =
            winwincode_execution_port::probe_result_normalizer::canonical_probe_evidence_bundle_bytes(
                &bundle,
            )
            .expect("bundle bytes");
        projections.push(
            project_probe_evidence(&bundle, artifact(['C', 'D'][index], &bundle_bytes))
                .expect("projection"),
        );
        receipts.push(receipt);
    }
    let round_receipt = build_round_receipt(&plan, receipts, round);
    let receipt_bytes =
        canonical_probe_round_receipt_bytes(&round_receipt).expect("round receipt bytes");
    let evidence = seal_debug_hypothesis_round_evidence(
        &plan,
        &round_receipt,
        artifact('E', &receipt_bytes),
        &projections,
    )
    .expect("round evidence");
    RoundFixture {
        plan,
        evidence,
        projections,
    }
}

fn build_round_receipt(
    plan: &ValidatedDebugProbePlan,
    receipts: Vec<ProbeExecutionReceipt>,
    round: usize,
) -> ProbeRoundReceipt {
    let total_output_bytes = receipts.iter().map(|value| value.output_bytes).sum();
    ProbeRoundReceipt {
        authority: plan.plan().authority.clone(),
        completion_reason: ProbeRoundCompletionReason::AllProbesTerminal,
        error: None,
        finished_at: timestamp(round * 10 + 4),
        plan_digest: plan.plan().plan_digest.clone(),
        probe_receipts: receipts,
        reducer: None,
        schema_version: 1,
        started_at: timestamp(round * 10 + 1),
        status: ProbeRoundReceiptStatus::Completed,
        usage: ProbeRoundBudgetUsage {
            budget_digest: plan.plan().budget.budget_digest.clone(),
            elapsed_millis: 3_000,
            peak_memory_bytes: 268_435_456,
            peak_parallel_probes: 2,
            probe_count: 2,
            total_command_arg_bytes: plan
                .probes()
                .iter()
                .map(winwincode_execution_port::debug_probe_contract::ValidatedProbe::command_arg_bytes)
                .sum(),
            total_cpu_millis: 20_000,
            total_output_bytes,
        },
    }
}

fn question(index: usize, round_id: &ProbeRoundId) -> DebugUnresolvedQuestion {
    let mut value = DebugUnresolvedQuestion {
        last_updated_round_id: round_id.clone(),
        opened_round_id: round_id.clone(),
        question_digest: digest('0'),
        summary: format!("question {index}"),
    };
    value.question_digest =
        derive_debug_unresolved_question_digest(&value).expect("question digest");
    value
}

fn seed() -> DebugHypothesisLedgerSeed {
    let authority = authority(0);
    DebugHypothesisLedgerSeed {
        created_at: Instant("2026-09-07T00:00:00.000Z".to_owned()),
        hypotheses: (1..=3)
            .map(|index| DebugHypothesis {
                confidence_bps: 0,
                contradicting_evidence: Vec::new(),
                created_round_id: authority.round_id.clone(),
                hypothesis_id: hypothesis_id(index),
                last_updated_round_id: authority.round_id.clone(),
                status: DebugHypothesisStatus::Active,
                summary: format!("hypothesis {index}"),
                supporting_evidence: Vec::new(),
            })
            .collect(),
        unresolved_questions: vec![question(1, &authority.round_id)],
        authority,
    }
}

fn candidates_for(
    evidence: &ValidatedDebugHypothesisRoundEvidence,
    target: usize,
) -> Vec<&HypothesisEvidenceCandidate> {
    evidence
        .cut()
        .evidence_candidates
        .iter()
        .filter(|value| value.target_hypothesis_id == hypothesis_id(target))
        .collect()
}

fn assessment(
    candidate: &HypothesisEvidenceCandidate,
    polarity: DebugHypothesisEvidencePolarity,
) -> DebugHypothesisEvidenceAssessment {
    DebugHypothesisEvidenceAssessment {
        candidate: candidate.clone(),
        polarity,
    }
}

fn update_mutation(
    assessments: Vec<DebugHypothesisEvidenceAssessment>,
    confidence_bps: i64,
    status: DebugHypothesisStatus,
    summary: &str,
) -> DebugHypothesisMutation {
    DebugHypothesisMutation {
        assessments,
        confidence_bps,
        hypothesis_id: hypothesis_id(1),
        kind: DebugHypothesisMutationKind::Update,
        previous_confidence_bps: Some(0),
        previous_status: Some(DebugHypothesisStatus::Active),
        status,
        summary: summary.to_owned(),
    }
}

fn invalid_mutation(
    case: MutationCase,
    evidence: &ValidatedDebugHypothesisRoundEvidence,
) -> DebugHypothesisMutation {
    let h1 = candidates_for(evidence, 1);
    let h2 = candidates_for(evidence, 2);
    match case {
        MutationCase::ContradictionRaises => update_mutation(
            vec![assessment(
                h1[0],
                DebugHypothesisEvidencePolarity::Contradicts,
            )],
            1,
            DebugHypothesisStatus::Active,
            "contradiction raised",
        ),
        MutationCase::MixedMoves => update_mutation(
            vec![
                assessment(h1[0], DebugHypothesisEvidencePolarity::Supports),
                assessment(h1[1], DebugHypothesisEvidencePolarity::Contradicts),
            ],
            1,
            DebugHypothesisStatus::Active,
            "mixed moved",
        ),
        MutationCase::WrongTarget => update_mutation(
            vec![assessment(h2[0], DebugHypothesisEvidencePolarity::Supports)],
            1,
            DebugHypothesisStatus::Active,
            "wrong target",
        ),
        MutationCase::IncompleteTerminal => update_mutation(
            vec![assessment(h1[0], DebugHypothesisEvidencePolarity::Supports)],
            10_000,
            DebugHypothesisStatus::Confirmed,
            "incomplete terminal",
        ),
        MutationCase::EmptyRaises => update_mutation(
            Vec::new(),
            1,
            DebugHypothesisStatus::Active,
            "empty evidence raised",
        ),
        MutationCase::EmptyTerminal => update_mutation(
            Vec::new(),
            10_000,
            DebugHypothesisStatus::Confirmed,
            "empty evidence terminal",
        ),
        MutationCase::IncompleteCancelledSession
        | MutationCase::IncompleteFailedSession
        | MutationCase::StaleAuthority => update_mutation(
            vec![assessment(h1[0], DebugHypothesisEvidencePolarity::Supports)],
            1,
            DebugHypothesisStatus::Active,
            "invalid session or authority",
        ),
    }
}

fn canonical_update(
    reducer: &DebugHypothesisLedgerReducer,
    evidence: &ValidatedDebugHypothesisRoundEvidence,
) -> DebugHypothesisLedgerUpdate {
    let h1 = candidates_for(evidence, 1);
    let h2 = candidates_for(evidence, 2);
    let h3 = candidates_for(evidence, 3);
    let mutations = vec![
        DebugHypothesisMutation {
            assessments: vec![
                assessment(h1[0], DebugHypothesisEvidencePolarity::Supports),
                assessment(h1[1], DebugHypothesisEvidencePolarity::Supports),
            ],
            confidence_bps: 10_000,
            hypothesis_id: hypothesis_id(1),
            kind: DebugHypothesisMutationKind::Update,
            previous_confidence_bps: Some(0),
            previous_status: Some(DebugHypothesisStatus::Active),
            status: DebugHypothesisStatus::Confirmed,
            summary: "hypothesis 1 confirmed".to_owned(),
        },
        DebugHypothesisMutation {
            assessments: vec![assessment(
                h2[0],
                DebugHypothesisEvidencePolarity::Contradicts,
            )],
            confidence_bps: 0,
            hypothesis_id: hypothesis_id(2),
            kind: DebugHypothesisMutationKind::Update,
            previous_confidence_bps: Some(0),
            previous_status: Some(DebugHypothesisStatus::Active),
            status: DebugHypothesisStatus::Rejected,
            summary: "hypothesis 2 rejected".to_owned(),
        },
        DebugHypothesisMutation {
            assessments: vec![
                assessment(h3[0], DebugHypothesisEvidencePolarity::Supports),
                assessment(h3[1], DebugHypothesisEvidencePolarity::Contradicts),
            ],
            confidence_bps: 0,
            hypothesis_id: hypothesis_id(3),
            kind: DebugHypothesisMutationKind::Update,
            previous_confidence_bps: Some(0),
            previous_status: Some(DebugHypothesisStatus::Active),
            status: DebugHypothesisStatus::Active,
            summary: "hypothesis 3 remains active".to_owned(),
        },
    ];
    let mut fact = DebugConfirmedFact {
        confirmed_round_id: evidence.receipt_reference().authority.round_id.clone(),
        evidence: vec![h1[1].clone(), h1[0].clone()],
        fact_digest: digest('0'),
        summary: "the first hypothesis is confirmed".to_owned(),
    };
    fact.fact_digest = derive_debug_confirmed_fact_digest(&fact).expect("fact digest");
    let mut step = DebugReproductionRecipeStep {
        probe_definition_digest: None,
        step_digest: digest('0'),
        summary: "run the exact admitted probe".to_owned(),
    };
    step.step_digest = derive_debug_reproduction_recipe_step_digest(&step).expect("step digest");
    let mut recipe = DebugReproductionRecipe {
        evidence: vec![h1[1].clone(), h1[0].clone()],
        last_updated_round_id: evidence.receipt_reference().authority.round_id.clone(),
        recipe_digest: digest('0'),
        steps: vec![step],
    };
    recipe.recipe_digest = derive_debug_reproduction_recipe_digest(&recipe).expect("recipe digest");
    DebugHypothesisLedgerUpdate {
        confirmed_facts: vec![fact],
        mutations,
        occurred_at: timestamp(15),
        opened_questions: vec![question(
            2,
            &evidence.receipt_reference().authority.round_id,
        )],
        previous_ledger_digest: reducer.ledger().ledger().ledger_digest.clone(),
        reproduction_recipe_update: Some(DebugReproductionRecipeUpdate {
            previous_recipe_digest: None,
            recipe,
        }),
        resolved_question_digests: vec![
            reducer.ledger().ledger().unresolved_questions[0]
                .question_digest
                .clone(),
        ],
        session_status: DebugSessionStatus::RootCauseIdentified,
        source_context_digest: digest('7'),
        source_request_digest: digest('8'),
        source_round_receipt: evidence.receipt_reference().clone(),
    }
}

#[test]
fn h1_h2_h3_transitions_are_exact_evidence_bound_and_replayable() {
    let round = round_fixture(false);
    let mut reducer = DebugHypothesisLedgerReducer::initialize(seed()).expect("seed Ledger");
    let update = canonical_update(&reducer, &round.evidence);
    let outcome = reducer
        .apply_round(
            update.clone(),
            &round.evidence,
            &round.evidence.receipt_reference().authority,
        )
        .expect("applied update");
    assert!(matches!(
        outcome,
        DebugHypothesisLedgerApplyOutcome::Applied(_)
    ));
    let ledger = reducer.ledger().ledger();
    assert_eq!(
        ledger.hypotheses[0].status,
        DebugHypothesisStatus::Confirmed
    );
    assert_eq!(ledger.hypotheses[0].confidence_bps, 10_000);
    assert_eq!(ledger.hypotheses[1].status, DebugHypothesisStatus::Rejected);
    assert_eq!(ledger.hypotheses[1].confidence_bps, 0);
    assert_eq!(ledger.hypotheses[2].status, DebugHypothesisStatus::Active);
    assert_eq!(ledger.hypotheses[2].confidence_bps, 0);
    assert_eq!(ledger.confirmed_facts.len(), 1);
    assert!(ledger.reproduction_recipe.is_some());
    assert_eq!(ledger.unresolved_questions.len(), 1);

    let events = reducer
        .events()
        .iter()
        .map(|value| value.event().clone())
        .collect::<Vec<_>>();
    let replayed =
        DebugHypothesisLedgerReducer::replay(&events, std::slice::from_ref(&round.evidence))
            .expect("replayed Ledger");
    assert_eq!(replayed.ledger().ledger(), reducer.ledger().ledger());
    let event_bytes = canonical_debug_hypothesis_ledger_event_bytes(&reducer.events()[1]);
    assert_eq!(
        decode_canonical_debug_hypothesis_ledger_event(&event_bytes).expect("event bytes"),
        *reducer.events()[1].event()
    );
}

#[test]
fn fact_and_recipe_candidate_sets_have_one_order_and_reject_duplicates() {
    let round = round_fixture(false);
    let reducer = DebugHypothesisLedgerReducer::initialize(seed()).expect("seed Ledger");
    let update = canonical_update(&reducer, &round.evidence);
    let fact_digest = derive_debug_confirmed_fact_digest(&update.confirmed_facts[0])
        .expect("reverse-order fact digest");
    let mut reordered_fact = update.confirmed_facts[0].clone();
    reordered_fact.evidence.reverse();
    assert_eq!(
        derive_debug_confirmed_fact_digest(&reordered_fact).expect("ordered fact digest"),
        fact_digest
    );
    let recipe = &update
        .reproduction_recipe_update
        .as_ref()
        .expect("recipe update")
        .recipe;
    let recipe_digest =
        derive_debug_reproduction_recipe_digest(recipe).expect("reverse-order recipe digest");
    let mut reordered_recipe = recipe.clone();
    reordered_recipe.evidence.reverse();
    assert_eq!(
        derive_debug_reproduction_recipe_digest(&reordered_recipe).expect("ordered recipe digest"),
        recipe_digest
    );

    let mut duplicate_fact_update = update.clone();
    let candidate = duplicate_fact_update.confirmed_facts[0].evidence[0].clone();
    duplicate_fact_update.confirmed_facts[0].evidence = vec![candidate.clone(), candidate];
    duplicate_fact_update.confirmed_facts[0].fact_digest =
        derive_debug_confirmed_fact_digest(&duplicate_fact_update.confirmed_facts[0])
            .expect("duplicate fact digest");
    let mut fact_reducer = DebugHypothesisLedgerReducer::initialize(seed()).expect("seed Ledger");
    assert!(
        fact_reducer
            .apply_round(
                duplicate_fact_update,
                &round.evidence,
                &round.evidence.receipt_reference().authority,
            )
            .is_err()
    );

    let mut duplicate_recipe_update = update;
    let recipe_update = duplicate_recipe_update
        .reproduction_recipe_update
        .as_mut()
        .expect("recipe update");
    let candidate = recipe_update.recipe.evidence[0].clone();
    recipe_update.recipe.evidence = vec![candidate.clone(), candidate];
    recipe_update.recipe.recipe_digest =
        derive_debug_reproduction_recipe_digest(&recipe_update.recipe)
            .expect("duplicate recipe digest");
    let mut recipe_reducer = DebugHypothesisLedgerReducer::initialize(seed()).expect("seed Ledger");
    assert!(
        recipe_reducer
            .apply_round(
                duplicate_recipe_update,
                &round.evidence,
                &round.evidence.receipt_reference().authority,
            )
            .is_err()
    );
}

#[test]
fn exact_receipt_replay_is_duplicate_even_after_authority_and_ledger_advance() {
    let round = round_fixture(false);
    let mut reducer = DebugHypothesisLedgerReducer::initialize(seed()).expect("seed Ledger");
    let update = canonical_update(&reducer, &round.evidence);
    reducer
        .apply_round(
            update.clone(),
            &round.evidence,
            &round.evidence.receipt_reference().authority,
        )
        .expect("first apply");
    let ledger = reducer.ledger().ledger().clone();
    let mut no_longer_active = round.evidence.receipt_reference().authority.clone();
    no_longer_active.fencing_token = FencingToken("999".to_owned());
    let duplicate = reducer
        .apply_round(update.clone(), &round.evidence, &no_longer_active)
        .expect("exact duplicate");
    assert!(duplicate.is_duplicate());
    assert_eq!(reducer.ledger().ledger(), &ledger);
    assert_eq!(reducer.events().len(), 2);

    let mut conflict = update;
    conflict.mutations[0].summary = "changed replay body".to_owned();
    assert_eq!(
        reducer
            .apply_round(conflict, &round.evidence, &no_longer_active)
            .expect_err("changed replay must conflict")
            .kind(),
        DebugHypothesisLedgerErrorKind::ReplayConflict
    );
}

#[test]
fn durable_evidence_cut_reopens_and_omitted_projection_is_rejected() {
    let round = round_fixture(true);
    let bytes = canonical_debug_hypothesis_round_evidence_bytes(&round.evidence);
    let reopened = reopen_debug_hypothesis_round_evidence(
        &bytes,
        &round.plan,
        &round.evidence.cut().evidence_cut_digest,
    )
    .expect("reopened evidence cut");
    assert_eq!(reopened.cut(), round.evidence.cut());

    let receipt = round.evidence.receipt();
    let receipt_bytes = canonical_probe_round_receipt_bytes(receipt).expect("receipt bytes");
    assert_eq!(
        seal_debug_hypothesis_round_evidence(
            &round.plan,
            receipt,
            artifact('E', &receipt_bytes),
            &round.projections[..1],
        )
        .expect_err("omitted incomplete projection must fail")
        .kind(),
        DebugHypothesisLedgerErrorKind::MissingCurrentEvidence
    );
}

#[test]
fn support_only_cannot_lower_an_existing_positive_confidence() {
    let first = round_fixture_at(1, false);
    let second = round_fixture_at(2, false);
    let mut reducer = DebugHypothesisLedgerReducer::initialize(seed()).expect("seed Ledger");
    let first_candidate = candidates_for(&first.evidence, 1)[0];
    let first_update = DebugHypothesisLedgerUpdate {
        confirmed_facts: Vec::new(),
        mutations: vec![DebugHypothesisMutation {
            assessments: vec![assessment(
                first_candidate,
                DebugHypothesisEvidencePolarity::Supports,
            )],
            confidence_bps: 500,
            hypothesis_id: hypothesis_id(1),
            kind: DebugHypothesisMutationKind::Update,
            previous_confidence_bps: Some(0),
            previous_status: Some(DebugHypothesisStatus::Active),
            status: DebugHypothesisStatus::Active,
            summary: "first support raises confidence".to_owned(),
        }],
        occurred_at: timestamp(15),
        opened_questions: Vec::new(),
        previous_ledger_digest: reducer.ledger().ledger().ledger_digest.clone(),
        reproduction_recipe_update: None,
        resolved_question_digests: Vec::new(),
        session_status: DebugSessionStatus::Active,
        source_context_digest: digest('7'),
        source_request_digest: digest('8'),
        source_round_receipt: first.evidence.receipt_reference().clone(),
    };
    reducer
        .apply_round(
            first_update,
            &first.evidence,
            &first.evidence.receipt_reference().authority,
        )
        .expect("first support update");

    let second_candidate = candidates_for(&second.evidence, 1)[0];
    let second_update = DebugHypothesisLedgerUpdate {
        confirmed_facts: Vec::new(),
        mutations: vec![DebugHypothesisMutation {
            assessments: vec![assessment(
                second_candidate,
                DebugHypothesisEvidencePolarity::Supports,
            )],
            confidence_bps: 499,
            hypothesis_id: hypothesis_id(1),
            kind: DebugHypothesisMutationKind::Update,
            previous_confidence_bps: Some(500),
            previous_status: Some(DebugHypothesisStatus::Active),
            status: DebugHypothesisStatus::Active,
            summary: "later support tries to lower confidence".to_owned(),
        }],
        occurred_at: timestamp(25),
        opened_questions: Vec::new(),
        previous_ledger_digest: reducer.ledger().ledger().ledger_digest.clone(),
        reproduction_recipe_update: None,
        resolved_question_digests: Vec::new(),
        session_status: DebugSessionStatus::Active,
        source_context_digest: digest('9'),
        source_request_digest: digest('a'),
        source_round_receipt: second.evidence.receipt_reference().clone(),
    };
    let before = reducer.ledger().ledger().clone();
    assert_eq!(
        reducer
            .apply_round(
                second_update,
                &second.evidence,
                &second.evidence.receipt_reference().authority,
            )
            .expect_err("support-only lowering must fail")
            .kind(),
        DebugHypothesisLedgerErrorKind::InvalidTransition
    );
    assert_eq!(reducer.ledger().ledger(), &before);
}

#[test]
fn non_crockford_hypothesis_and_artifact_ids_are_rejected() {
    let mut invalid_seed = seed();
    invalid_seed.hypotheses[0].hypothesis_id =
        DebugHypothesisId("hyp_!!!!!!!!!!!!!!!!!!!!!!!!!!".to_owned());
    assert_eq!(
        DebugHypothesisLedgerReducer::initialize(invalid_seed)
            .expect_err("invalid hypothesis identity")
            .kind(),
        DebugHypothesisLedgerErrorKind::InvalidInput
    );

    let round = round_fixture(false);
    let receipt_bytes =
        canonical_probe_round_receipt_bytes(round.evidence.receipt()).expect("receipt bytes");
    assert_eq!(
        seal_debug_hypothesis_round_evidence(
            &round.plan,
            round.evidence.receipt(),
            artifact('L', &receipt_bytes),
            &round.projections,
        )
        .expect_err("Crockford excludes L")
        .kind(),
        DebugHypothesisLedgerErrorKind::InvalidInput
    );
}

#[test]
fn table_driven_invalid_seed_direction_target_terminal_and_stale_cases_fail_closed() {
    let round = round_fixture(false);
    let incomplete = round_fixture(true);

    let mut invalid_seed = seed();
    invalid_seed.hypotheses[0].confidence_bps = 1;
    assert_eq!(
        DebugHypothesisLedgerReducer::initialize(invalid_seed)
            .expect_err("nonzero seed")
            .kind(),
        DebugHypothesisLedgerErrorKind::InvalidSeed
    );

    let cases = [
        MutationCase::ContradictionRaises,
        MutationCase::MixedMoves,
        MutationCase::WrongTarget,
        MutationCase::IncompleteTerminal,
        MutationCase::EmptyRaises,
        MutationCase::EmptyTerminal,
        MutationCase::IncompleteCancelledSession,
        MutationCase::IncompleteFailedSession,
        MutationCase::StaleAuthority,
    ];
    for case in cases {
        let selected = if matches!(
            case,
            MutationCase::IncompleteTerminal
                | MutationCase::IncompleteCancelledSession
                | MutationCase::IncompleteFailedSession
        ) {
            &incomplete.evidence
        } else {
            &round.evidence
        };
        let mut reducer = DebugHypothesisLedgerReducer::initialize(seed()).expect("seed Ledger");
        let mutation = invalid_mutation(case, selected);
        let session_status = match case {
            MutationCase::IncompleteCancelledSession => DebugSessionStatus::Cancelled,
            MutationCase::IncompleteFailedSession => DebugSessionStatus::Failed,
            _ => DebugSessionStatus::Active,
        };
        let update = DebugHypothesisLedgerUpdate {
            confirmed_facts: Vec::new(),
            mutations: vec![mutation],
            occurred_at: timestamp(15),
            opened_questions: Vec::new(),
            previous_ledger_digest: reducer.ledger().ledger().ledger_digest.clone(),
            reproduction_recipe_update: None,
            resolved_question_digests: Vec::new(),
            session_status,
            source_context_digest: digest('7'),
            source_request_digest: digest('8'),
            source_round_receipt: selected.receipt_reference().clone(),
        };
        let mut expected = selected.receipt_reference().authority.clone();
        if matches!(case, MutationCase::StaleAuthority) {
            expected.fencing_token = FencingToken("999".to_owned());
        }
        let before = reducer.ledger().ledger().clone();
        assert!(reducer.apply_round(update, selected, &expected).is_err());
        assert_eq!(reducer.ledger().ledger(), &before);
        assert_eq!(reducer.events().len(), 1);
    }
}
