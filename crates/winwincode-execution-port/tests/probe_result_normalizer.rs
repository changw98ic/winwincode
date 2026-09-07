// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write as _;
use std::path::Path;

use sha2::{Digest as _, Sha256};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DebugHypothesisId, DebugSessionId, ExecutionJobId, FencingToken,
    Instant, LeaseId, ProbeId, ProbeRoundId, ProductSessionId, RepositoryId, SessionIdentity,
    Sha256Digest, WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::{
    debug_probe_contract::{
        ValidatedProbeExecutionIntent, derive_debug_probe_plan_digest, derive_probe_budget_digest,
        derive_probe_command_arg_bytes, derive_probe_definition_digest, seal_debug_probe_plan,
        seal_probe_execution_intent,
    },
    generated::{
        ArtifactReference, DebugProbeKind, DebugProbePlan, DebugProbeRoundAuthority,
        DiagnosticChangeStatus, DiagnosticParserVersion, ProbeBaselineSelection,
        ProbeBaselineState, ProbeCommandSpec, ProbeCompletionRule, ProbeCompletionRuleKind,
        ProbeEvidenceCompletenessStatus, ProbeEvidenceIncompleteReason, ProbeExecutionReceipt,
        ProbeNetworkAccess, ProbeNormalizerProfile, ProbeNormalizerVersion, ProbeRawStream,
        ProbeRawStreamEncoding, ProbeReceiptStatus, ProbeResourceClaim, ProbeRoundBudget,
        ProbeSideEffectClass, ProbeSpec, ProbeStackParserVersion, ProbeWorkspaceAccess,
        ProbeWorkspaceDeltaState,
    },
    probe_result_normalizer::{
        ProbeRawStreamInput, bind_prior_probe_evidence_bundle,
        canonical_probe_evidence_bundle_bytes, derive_probe_evidence_bundle_digest,
        derive_probe_normalizer_profile_digest, normalize_probe_evidence,
        probe_baseline_not_applicable, probe_baseline_unavailable, project_probe_evidence,
        reopen_probe_evidence_bundle_from_journal, seal_prior_probe_evidence_bundle_bytes,
        seal_probe_baseline_selection, seal_probe_normalizer_profile,
        select_probe_evidence_baseline, validate_probe_evidence_bundle,
    },
};

const WORKSPACE_ROOT: &str = "/workspace";

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

fn authority(revision: char) -> DebugProbeRoundAuthority {
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
            stage_run_id: None,
            worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
        },
        workspace_revision: WorkspaceRevision(format!(
            "git-tree:{}",
            revision.to_string().repeat(40)
        )),
    }
}

fn sealed_intent(revision: char, output_limit_bytes: i64) -> ValidatedProbeExecutionIntent {
    let mut plan = DebugProbePlan {
        authority: authority(revision),
        budget: ProbeRoundBudget {
            budget_digest: digest('6'),
            parallel_probe_limit: 1,
            peak_memory_limit_bytes: 134_217_728,
            probe_limit: 1,
            total_command_arg_limit_bytes: 262_144,
            total_cpu_limit_millis: 10_000,
            total_output_limit_bytes: output_limit_bytes,
            wall_time_limit_millis: 300_000,
        },
        completion_rule: ProbeCompletionRule {
            kind: ProbeCompletionRuleKind::AllTerminal,
            minimum_completed_probes: 1,
            minimum_successful_probes: 1,
            stop_on_required_probe_failure: true,
        },
        created_at: Instant("2026-09-06T08:00:00.000Z".to_owned()),
        plan_digest: digest('3'),
        probes: vec![ProbeSpec {
            command: ProbeCommandSpec {
                argv: vec!["fixture-probe".to_owned()],
                command_arg_bytes: 1,
                working_directory: ".".to_owned(),
            },
            kind: DebugProbeKind::StaticAnalysis,
            output_limit_bytes,
            probe_definition_digest: digest('2'),
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
    let sealed = seal_debug_probe_plan(plan.clone(), &plan.authority).expect("sealed plan");
    let probe = &sealed.probes()[0];
    seal_probe_execution_intent(
        winwincode_execution_port::generated::ProbeExecutionIntent {
            created_at: Instant("2026-09-06T08:00:01.000Z".to_owned()),
            identity: sealed
                .probe_identity(&probe.spec().probe_id)
                .expect("probe identity"),
            plan_digest: sealed.plan().plan_digest.clone(),
            schema_version: 1,
            spec: probe.spec().clone(),
        },
        &sealed,
    )
    .expect("sealed intent")
}

fn profile(
    diagnostic: Option<DiagnosticParserVersion>,
    stack: Option<ProbeStackParserVersion>,
) -> winwincode_execution_port::probe_result_normalizer::ValidatedProbeNormalizerProfile {
    let mut profile = ProbeNormalizerProfile {
        diagnostic_parser_version: diagnostic,
        normalizer_version: ProbeNormalizerVersion::L0L1V1,
        profile_digest: digest('0'),
        stack_parser_version: stack,
    };
    profile.profile_digest =
        derive_probe_normalizer_profile_digest(&profile).expect("profile digest");
    seal_probe_normalizer_profile(profile).expect("sealed profile")
}

fn receipt(
    intent: &ValidatedProbeExecutionIntent,
    artifacts: Vec<ArtifactReference>,
    output_bytes: usize,
    truncated: bool,
) -> ProbeExecutionReceipt {
    ProbeExecutionReceipt {
        artifact_refs: artifacts,
        duration_millis: 10,
        error: None,
        exit_code: Some(0),
        finished_at: Instant("2026-09-06T08:00:03.000Z".to_owned()),
        identity: intent.intent().identity.clone(),
        output_bytes: i64::try_from(output_bytes).expect("output bytes"),
        output_truncated: truncated,
        plan_digest: intent.intent().plan_digest.clone(),
        schema_version: 1,
        signal: None,
        started_at: Instant("2026-09-06T08:00:02.000Z".to_owned()),
        status: ProbeReceiptStatus::Succeeded,
        timed_out: false,
    }
}

fn bundle_artifact(
    bundle: &winwincode_execution_port::probe_result_normalizer::ValidatedProbeEvidenceBundle,
    character: char,
) -> ArtifactReference {
    let bytes = canonical_probe_evidence_bundle_bytes(bundle).expect("bundle bytes");
    artifact(character, &bytes)
}

#[test]
fn status_only_flow_round_trips_and_projects_without_raw_output() {
    let output = b"ok\n";
    let intent = sealed_intent('0', 4_194_304);
    let raw_ref = artifact('A', output);
    let raw = [ProbeRawStreamInput::new(
        ProbeRawStream::Stdout,
        raw_ref.clone(),
        output,
    )];
    let receipt = receipt(&intent, vec![raw_ref], output.len(), false);
    let normalizer = profile(None, None);
    let baseline = probe_baseline_not_applicable();
    let bundle = normalize_probe_evidence(
        &intent,
        &receipt,
        &normalizer,
        &raw,
        &baseline,
        Path::new(WORKSPACE_ROOT),
    )
    .expect("normalized bundle");

    assert_eq!(
        bundle.bundle().completeness.status,
        ProbeEvidenceCompletenessStatus::Complete
    );
    assert!(bundle.bundle().diagnostics.is_empty());
    assert_eq!(
        bundle.bundle().workspace_delta.state,
        ProbeWorkspaceDeltaState::NotApplicable
    );
    let bytes = canonical_probe_evidence_bundle_bytes(&bundle).expect("bundle bytes");
    let reopened_ref = artifact('C', &bytes);
    let reopened = reopen_probe_evidence_bundle_from_journal(
        &bytes,
        &intent,
        &reopened_ref,
        &bundle.bundle().bundle_digest,
        &normalizer,
        &baseline,
    )
    .expect("reopened bundle");
    assert_eq!(reopened, bundle);

    let bundle_ref = bundle_artifact(&bundle, 'B');
    let projection = project_probe_evidence(&bundle, bundle_ref.clone()).expect("projection");
    assert_eq!(projection.summary().unique_diagnostic_count, 0);
    assert_eq!(projection.summary().diagnostic_occurrence_count, 0);
    assert_eq!(projection.candidates().len(), 1);
    assert_eq!(projection.summary().bundle_artifact_ref, bundle_ref);
    assert!(!projection.summary().summary.contains("ok"));

    let prior = seal_prior_probe_evidence_bundle_bytes(
        &bytes,
        bundle_artifact(&bundle, 'D'),
        &bundle.bundle().bundle_digest,
    )
    .expect("journal-bound prior");
    assert_eq!(prior.bundle(), bundle.bundle());
}

#[test]
fn explicit_stack_profiles_parse_three_formats_and_cluster_repeats() {
    let cases = [
        (
            ProbeStackParserVersion::RustV1,
            "thread 'main' panicked\nstack backtrace:\n  0: fixture::run\n     at /workspace/src/main.rs:12:7\n",
            1,
        ),
        (
            ProbeStackParserVersion::NodeV1,
            "Error: fixture\n    at run (file:///workspace/src/main.mjs:12:7)\nError: fixture\n    at run (file:///workspace/src/main.mjs:12:7)\n",
            2,
        ),
        (
            ProbeStackParserVersion::PythonV1,
            "Traceback (most recent call last):\n  File \"/workspace/src/main.py\", line 12, in run\n    fail()\nRuntimeError: fixture\n",
            1,
        ),
    ];

    for (index, (version, output, occurrence_count)) in cases.into_iter().enumerate() {
        let intent = sealed_intent('0', 4_194_304);
        let raw_ref = artifact(
            char::from(b'D' + u8::try_from(index).expect("case index")),
            output.as_bytes(),
        );
        let raw = [ProbeRawStreamInput::new(
            ProbeRawStream::Stderr,
            raw_ref.clone(),
            output.as_bytes(),
        )];
        let receipt = receipt(&intent, vec![raw_ref], output.len(), false);
        let normalizer = profile(None, Some(version));
        let bundle = normalize_probe_evidence(
            &intent,
            &receipt,
            &normalizer,
            &raw,
            &probe_baseline_not_applicable(),
            Path::new(WORKSPACE_ROOT),
        )
        .expect("stack bundle");
        assert_eq!(bundle.bundle().stack_clusters.len(), 1);
        assert_eq!(
            bundle.bundle().stack_clusters[0].occurrence_count,
            occurrence_count
        );
        assert!(
            bundle.bundle().stack_clusters[0].frames[0]
                .function
                .is_some()
        );
    }
}

#[test]
fn marker_like_text_is_incomplete_and_never_becomes_a_stack_fact() {
    let cases = [
        (
            ProbeStackParserVersion::RustV1,
            "note: not a stack backtrace:\n0: fabricated\n",
        ),
        (
            ProbeStackParserVersion::NodeV1,
            "note only\n    at fabricated (file:///workspace/src/main.mjs:1:1)\n",
        ),
        (
            ProbeStackParserVersion::PythonV1,
            "note: Traceback (most recent call last): ignored\n  File \"/workspace/src/main.py\", line 1, in fabricated\n",
        ),
    ];
    for (index, (version, output)) in cases.into_iter().enumerate() {
        let intent = sealed_intent('0', 4_194_304);
        let raw_ref = artifact(['G', 'H', 'J'][index], output.as_bytes());
        let raw = [ProbeRawStreamInput::new(
            ProbeRawStream::Stderr,
            raw_ref.clone(),
            output.as_bytes(),
        )];
        let receipt = receipt(&intent, vec![raw_ref], output.len(), false);
        let bundle = normalize_probe_evidence(
            &intent,
            &receipt,
            &profile(None, Some(version)),
            &raw,
            &probe_baseline_not_applicable(),
            Path::new(WORKSPACE_ROOT),
        )
        .expect("incomplete bundle");
        assert!(bundle.bundle().stack_clusters.is_empty());
        assert_eq!(
            bundle.bundle().completeness.reasons,
            vec![ProbeEvidenceIncompleteReason::InvalidPayload]
        );
    }
}

#[test]
fn stack_limits_truncate_only_with_explicit_incompleteness() {
    let mut frames = String::new();
    for index in 0..65 {
        writeln!(&mut frames, "  {index}: fixture::{index}").expect("frame fixture");
    }
    let over_frames = format!("stack backtrace:\n{frames}");
    let mut clusters = String::new();
    for index in 0..257 {
        writeln!(&mut clusters, "stack backtrace:\n  0: fixture::{index}")
            .expect("cluster fixture");
    }
    let cases = [
        (
            over_frames,
            ProbeEvidenceIncompleteReason::TooManyStackFrames,
            1_usize,
        ),
        (
            clusters,
            ProbeEvidenceIncompleteReason::TooManyStackClusters,
            256_usize,
        ),
    ];
    for (index, (output, reason, expected_clusters)) in cases.into_iter().enumerate() {
        let intent = sealed_intent('0', 4_194_304);
        let raw_ref = artifact(['T', 'V'][index], output.as_bytes());
        let raw = [ProbeRawStreamInput::new(
            ProbeRawStream::Stderr,
            raw_ref.clone(),
            output.as_bytes(),
        )];
        let receipt = receipt(&intent, vec![raw_ref], output.len(), false);
        let bundle = normalize_probe_evidence(
            &intent,
            &receipt,
            &profile(None, Some(ProbeStackParserVersion::RustV1)),
            &raw,
            &probe_baseline_not_applicable(),
            Path::new(WORKSPACE_ROOT),
        )
        .expect("bounded stack bundle");
        assert_eq!(bundle.bundle().stack_clusters.len(), expected_clusters);
        assert!(bundle.bundle().completeness.reasons.contains(&reason));
    }
}

#[test]
fn unavailable_baseline_bootstraps_comparable_30k_occurrence_evidence() {
    let prior_output =
        b"src/probe.ts(12,7): error TS2322: Type 'string' is not assignable to type 'number'.\n";
    let normalizer = profile(Some(DiagnosticParserVersion::TypescriptV1), None);
    let prior_intent = sealed_intent('0', 4_194_304);
    let prior_raw_ref = artifact('J', prior_output);
    let prior_raw = [ProbeRawStreamInput::new(
        ProbeRawStream::Stdout,
        prior_raw_ref.clone(),
        prior_output,
    )];
    let prior_receipt = receipt(
        &prior_intent,
        vec![prior_raw_ref],
        prior_output.len(),
        false,
    );
    let prior_bundle = normalize_probe_evidence(
        &prior_intent,
        &prior_receipt,
        &normalizer,
        &prior_raw,
        &probe_baseline_unavailable(),
        Path::new(WORKSPACE_ROOT),
    )
    .expect("first diagnostic bundle");
    assert_eq!(
        prior_bundle.bundle().completeness.reasons,
        vec![ProbeEvidenceIncompleteReason::BaselineUnavailable]
    );
    let prior =
        bind_prior_probe_evidence_bundle(&prior_bundle, bundle_artifact(&prior_bundle, 'K'))
            .expect("bound prior");

    let intent = sealed_intent('1', 4_194_304);
    let selected =
        select_probe_evidence_baseline(&prior, &intent, &normalizer).expect("comparable baseline");
    let selection_bytes = serde_json::to_vec(selected.selection()).expect("selection bytes");
    let recovered_selection: ProbeBaselineSelection =
        serde_json::from_slice(&selection_bytes).expect("selection wire");
    let recovered =
        seal_probe_baseline_selection(&recovered_selection, Some(&prior), &intent, &normalizer)
            .expect("recovered selection");
    assert_eq!(recovered.selection(), selected.selection());
    let output = prior_output.repeat(30_000);
    let raw_ref = artifact('M', &output);
    let raw = [ProbeRawStreamInput::new(
        ProbeRawStream::Stdout,
        raw_ref.clone(),
        &output,
    )];
    let current_receipt = receipt(&intent, vec![raw_ref], output.len(), false);
    let bundle = normalize_probe_evidence(
        &intent,
        &current_receipt,
        &normalizer,
        &raw,
        &selected,
        Path::new(WORKSPACE_ROOT),
    )
    .expect("comparable diagnostic bundle");

    assert_eq!(
        bundle.bundle().completeness.status,
        ProbeEvidenceCompletenessStatus::Complete
    );
    assert_eq!(bundle.bundle().diagnostics.len(), 1);
    assert_eq!(bundle.bundle().diagnostics[0].occurrence_count, 30_000);
    assert_eq!(
        bundle.bundle().baseline.state,
        ProbeBaselineState::Available
    );
    assert_eq!(
        bundle
            .bundle()
            .baseline
            .comparison
            .as_ref()
            .expect("comparison")
            .entries[0]
            .status,
        DiagnosticChangeStatus::Unchanged
    );
    let projection = project_probe_evidence(&bundle, bundle_artifact(&bundle, 'N'))
        .expect("diagnostic projection");
    assert_eq!(projection.summary().unique_diagnostic_count, 1);
    assert_eq!(projection.summary().diagnostic_occurrence_count, 30_000);

    assert_result_incomplete_retains_baseline(&intent, &normalizer, &recovered);
}

fn assert_result_incomplete_retains_baseline(
    intent: &ValidatedProbeExecutionIntent,
    normalizer: &winwincode_execution_port::probe_result_normalizer::ValidatedProbeNormalizerProfile,
    recovered: &winwincode_execution_port::probe_result_normalizer::ValidatedProbeBaselineInput,
) {
    let mut truncated_output = vec![b'x'; 4_194_304];
    truncated_output[0] = 0xff;
    let truncated_ref = artifact('P', &truncated_output);
    let truncated_raw = [ProbeRawStreamInput::new(
        ProbeRawStream::Stdout,
        truncated_ref.clone(),
        &truncated_output,
    )];
    let truncated_receipt = receipt(intent, vec![truncated_ref], truncated_output.len(), true);
    let incomplete = normalize_probe_evidence(
        intent,
        &truncated_receipt,
        normalizer,
        &truncated_raw,
        recovered,
        Path::new(WORKSPACE_ROOT),
    )
    .expect("result-incomplete baseline bundle");
    assert_eq!(
        incomplete.bundle().baseline.state,
        ProbeBaselineState::ResultIncomplete
    );
    assert_eq!(
        incomplete.bundle().baseline.baseline_bundle_artifact_ref,
        recovered.selection().prior_bundle_artifact_ref
    );
    assert_eq!(
        incomplete.bundle().baseline.baseline_bundle_digest,
        recovered.selection().prior_bundle_digest
    );
}

#[test]
fn truncated_invalid_utf8_is_explicitly_incomplete() {
    let mut output = vec![b'x'; 64];
    output[0] = 0xff;
    let intent = sealed_intent('0', 64);
    let raw_ref = artifact('P', &output);
    let raw = [ProbeRawStreamInput::new(
        ProbeRawStream::Stderr,
        raw_ref.clone(),
        &output,
    )];
    let receipt = receipt(&intent, vec![raw_ref], output.len(), true);
    let bundle = normalize_probe_evidence(
        &intent,
        &receipt,
        &profile(None, None),
        &raw,
        &probe_baseline_not_applicable(),
        Path::new(WORKSPACE_ROOT),
    )
    .expect("incomplete bundle");
    assert_eq!(
        bundle.bundle().completeness.reasons,
        vec![
            ProbeEvidenceIncompleteReason::OutputTruncated,
            ProbeEvidenceIncompleteReason::InvalidUtf8,
        ]
    );
    assert_eq!(
        bundle.bundle().raw_streams[0].encoding,
        ProbeRawStreamEncoding::InvalidUtf8
    );
}

#[test]
fn durable_validator_rejects_cross_field_mutations_after_digest_refresh() {
    let output = b"ok\n";
    let intent = sealed_intent('0', 4_194_304);
    let raw_ref = artifact('Q', output);
    let raw = [ProbeRawStreamInput::new(
        ProbeRawStream::Stdout,
        raw_ref.clone(),
        output,
    )];
    let receipt = receipt(&intent, vec![raw_ref], output.len(), false);
    let base = normalize_probe_evidence(
        &intent,
        &receipt,
        &profile(None, None),
        &raw,
        &probe_baseline_not_applicable(),
        Path::new(WORKSPACE_ROOT),
    )
    .expect("base bundle")
    .into_bundle();

    let mutations: [fn(&mut winwincode_execution_port::generated::ProbeEvidenceBundle); 4] = [
        |bundle| bundle.raw_streams[0].retained_bytes += 1,
        |bundle| bundle.raw_streams[0].encoding = ProbeRawStreamEncoding::InvalidUtf8,
        |bundle| bundle.workspace_delta.state = ProbeWorkspaceDeltaState::Available,
        |bundle| {
            bundle
                .completeness
                .reasons
                .push(ProbeEvidenceIncompleteReason::InvalidUtf8);
        },
    ];
    for mutate in mutations {
        let mut candidate = base.clone();
        mutate(&mut candidate);
        candidate.bundle_digest =
            derive_probe_evidence_bundle_digest(&candidate).expect("refreshed digest");
        assert!(validate_probe_evidence_bundle(&candidate, &intent).is_err());
    }
}

#[test]
fn failed_test_projection_is_exact_in_both_directions() {
    let output = br#"<testsuite><testcase name="fixture" file="tests/f.rs" line="12"><failure message="boom"/></testcase></testsuite>"#;
    let intent = sealed_intent('0', 4_194_304);
    let raw_ref = artifact('R', output);
    let raw = [ProbeRawStreamInput::new(
        ProbeRawStream::Stdout,
        raw_ref.clone(),
        output,
    )];
    let receipt = receipt(&intent, vec![raw_ref], output.len(), false);
    let bundle = normalize_probe_evidence(
        &intent,
        &receipt,
        &profile(Some(DiagnosticParserVersion::JunitXmlV1), None),
        &raw,
        &probe_baseline_unavailable(),
        Path::new(WORKSPACE_ROOT),
    )
    .expect("failed test bundle");
    assert_eq!(bundle.bundle().failed_tests.len(), 1);

    for mutate in [
        |bundle: &mut winwincode_execution_port::generated::ProbeEvidenceBundle| {
            bundle.failed_tests.clear();
        },
        |bundle: &mut winwincode_execution_port::generated::ProbeEvidenceBundle| {
            bundle.failed_tests[0].occurrence_count += 1;
        },
    ] {
        let mut candidate = bundle.bundle().clone();
        mutate(&mut candidate);
        candidate.bundle_digest =
            derive_probe_evidence_bundle_digest(&candidate).expect("refreshed digest");
        assert!(validate_probe_evidence_bundle(&candidate, &intent).is_err());
    }
}
