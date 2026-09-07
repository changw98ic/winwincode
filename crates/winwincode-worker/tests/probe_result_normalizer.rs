// SPDX-License-Identifier: Apache-2.0

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use winwincode_domain::{
    ArtifactId, CodexThreadId, DebugHypothesisId, DebugSessionId, ExecutionJobId, FencingToken,
    Instant, LeaseId, ProbeId, ProbeRoundId, ProductSessionId, RepositoryId, SessionIdentity,
    Sha256Digest, WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::{
    debug_hypothesis_ledger::{
        DebugHypothesisLedgerReducer, canonical_probe_round_receipt_bytes,
        seal_debug_hypothesis_round_evidence,
    },
    debug_probe_contract::{
        derive_debug_probe_plan_digest, derive_probe_budget_digest, derive_probe_command_arg_bytes,
        derive_probe_definition_digest, seal_debug_probe_plan,
    },
    generated::{
        ArtifactReference, DebugHypothesis, DebugHypothesisLedgerSeed, DebugHypothesisStatus,
        DebugProbeErrorCode, DebugProbeKind, DebugProbePlan, DebugProbeRoundAuthority,
        DiagnosticParserVersion, ProbeCommandSpec, ProbeCompletionRule, ProbeCompletionRuleKind,
        ProbeEvidenceCompletenessStatus, ProbeEvidenceIncompleteReason, ProbeNetworkAccess,
        ProbeNormalizerProfile, ProbeNormalizerVersion, ProbeReceiptStatus, ProbeResourceClaim,
        ProbeRoundBudget, ProbeRoundCompletionReason, ProbeRoundReceiptStatus,
        ProbeSideEffectClass, ProbeSpec, ProbeStackParserVersion, ProbeWorkspaceAccess,
    },
    probe_result_normalizer::{derive_probe_normalizer_profile_digest, project_probe_evidence},
};

#[path = "fixtures/probe-result-normalizer/v1/mod.rs"]
mod fixtures;
use winwincode_worker::probe_scheduler::{
    AdmittedProbe, ProbeClock, ProbeExecutionCompletion, ProbePriorEvidence, ProbeRoundRequest,
    ProbeRunCancellation, ProbeRunFacts, ProbeRunTermination, ProbeRunner, ProbeRunnerFuture,
    ProbeScheduler, ProbeSchedulerError, TrustedPureReadTemplate,
};
use winwincode_worker::{
    context_safety::WorkerContextSafetyScanner,
    debug_probe_context::{WorkerDebugProbeContextInput, prepare_worker_debug_probe_context},
};

#[derive(Clone, Debug)]
struct ArtifactRunner {
    stdout: Arc<Vec<u8>>,
    stderr: Arc<Vec<u8>>,
    truncated: bool,
    fail_after_retain: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    delay: Duration,
    termination: ProbeRunTermination,
}

impl ArtifactRunner {
    fn new(stdout: Vec<u8>, truncated: bool) -> Self {
        Self {
            stdout: Arc::new(stdout),
            stderr: Arc::new(Vec::new()),
            truncated,
            fail_after_retain: Arc::new(AtomicBool::new(false)),
            calls: Arc::new(AtomicUsize::new(0)),
            delay: Duration::ZERO,
            termination: ProbeRunTermination::Exited,
        }
    }

    fn with_stderr(mut self, stderr: Vec<u8>) -> Self {
        self.stderr = Arc::new(stderr);
        self
    }

    fn fail_after_retain(self) -> Self {
        self.fail_after_retain.store(true, Ordering::Release);
        self
    }

    fn with_process(mut self, termination: ProbeRunTermination, delay: Duration) -> Self {
        self.termination = termination;
        self.delay = delay;
        self
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

impl ProbeRunner for ArtifactRunner {
    fn validate(&self, _probe: &AdmittedProbe) -> Result<(), ProbeSchedulerError> {
        Ok(())
    }

    fn execute(
        &self,
        _probe: AdmittedProbe,
        cancellation: ProbeRunCancellation,
        completion: ProbeExecutionCompletion,
    ) -> ProbeRunnerFuture<'_> {
        let stdout = Arc::clone(&self.stdout);
        let stderr = Arc::clone(&self.stderr);
        let calls = Arc::clone(&self.calls);
        let fail_after_retain = Arc::clone(&self.fail_after_retain);
        let truncated = self.truncated;
        let delay = self.delay;
        let termination = self.termination;
        Box::pin(async move {
            calls.fetch_add(1, Ordering::AcqRel);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if termination == ProbeRunTermination::Cancelled {
                cancellation.cancel();
            }
            let retained = completion.retain(
                ProbeRunFacts::new(
                    termination,
                    (termination == ProbeRunTermination::Exited).then_some(0),
                    None,
                    delay.max(Duration::from_millis(4)),
                ),
                &stdout,
                &stderr,
                truncated,
            )?;
            if fail_after_retain.load(Ordering::Acquire) {
                return Err(ProbeSchedulerError::host_failure(
                    DebugProbeErrorCode::InfrastructureError,
                    "fixture stopped after durable process completion",
                ));
            }
            Ok(retained)
        })
    }
}

#[derive(Debug, Default)]
struct StepClock(AtomicUsize);

impl ProbeClock for StepClock {
    fn now(&self) -> Result<Instant, ProbeSchedulerError> {
        let step = self.0.fetch_add(1, Ordering::AcqRel);
        Ok(Instant(format!(
            "2026-09-06T08:00:{:02}.000Z",
            step.min(59)
        )))
    }
}

fn digest(symbol: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", symbol.to_string().repeat(64)))
}

fn content_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn authority(round: char, environment: char) -> DebugProbeRoundAuthority {
    DebugProbeRoundAuthority {
        attempt: 1,
        debug_session_id: DebugSessionId("dbg_00000000000000000000000000".to_owned()),
        environment_digest: digest(environment),
        fencing_token: FencingToken("7".to_owned()),
        job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
        lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
        repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
        round_id: ProbeRoundId(format!(
            "prn_{}",
            round.to_ascii_uppercase().to_string().repeat(26)
        )),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
            product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
            stage_run_id: None,
            worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
        },
        workspace_revision: WorkspaceRevision(format!("git-tree:{}", "0".repeat(40))),
    }
}

fn plan(round: char, environment: char, output_limit: usize) -> DebugProbePlan {
    let mut probe = ProbeSpec {
        command: ProbeCommandSpec {
            argv: vec!["fixture-probe".to_owned(), "check".to_owned()],
            command_arg_bytes: 1,
            working_directory: ".".to_owned(),
        },
        kind: DebugProbeKind::StaticAnalysis,
        output_limit_bytes: i64::try_from(output_limit).expect("output limit"),
        probe_definition_digest: digest('2'),
        probe_id: ProbeId("prb_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned()),
        required: true,
        resources: ProbeResourceClaim {
            cpu_limit_millis: 100,
            database_keys: Vec::new(),
            exclusive_keys: Vec::new(),
            memory_limit_bytes: 1_048_576,
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
        timeout_millis: 250,
    };
    probe.command.command_arg_bytes =
        derive_probe_command_arg_bytes(&probe.command.argv).expect("command bytes");
    probe.probe_definition_digest = derive_probe_definition_digest(&probe).expect("probe digest");
    let mut budget = ProbeRoundBudget {
        budget_digest: digest('3'),
        parallel_probe_limit: 1,
        peak_memory_limit_bytes: 1_048_576,
        probe_limit: 1,
        total_command_arg_limit_bytes: 4_096,
        total_cpu_limit_millis: 100,
        total_output_limit_bytes: probe.output_limit_bytes,
        wall_time_limit_millis: 5_000,
    };
    budget.budget_digest = derive_probe_budget_digest(&budget).expect("budget digest");
    let mut plan = DebugProbePlan {
        authority: authority(round, environment),
        budget,
        completion_rule: ProbeCompletionRule {
            kind: ProbeCompletionRuleKind::AllTerminal,
            minimum_completed_probes: 1,
            minimum_successful_probes: 1,
            stop_on_required_probe_failure: true,
        },
        created_at: Instant("2026-09-06T00:00:00.000Z".to_owned()),
        plan_digest: digest('4'),
        probes: vec![probe],
        schema_version: 1,
    };
    plan.plan_digest = derive_debug_probe_plan_digest(&plan).expect("plan digest");
    plan
}

fn refresh(plan: &mut DebugProbePlan) {
    plan.probes[0].probe_definition_digest =
        derive_probe_definition_digest(&plan.probes[0]).expect("probe digest");
    plan.budget.budget_digest = derive_probe_budget_digest(&plan.budget).expect("budget digest");
    plan.plan_digest = derive_debug_probe_plan_digest(plan).expect("plan digest");
}

fn profile(parser: Option<DiagnosticParserVersion>) -> ProbeNormalizerProfile {
    let mut profile = ProbeNormalizerProfile {
        diagnostic_parser_version: parser,
        normalizer_version: ProbeNormalizerVersion::L0L1V1,
        profile_digest: digest('5'),
        stack_parser_version: None,
    };
    profile.profile_digest =
        derive_probe_normalizer_profile_digest(&profile).expect("profile digest");
    profile
}

fn stack_profile(parser: ProbeStackParserVersion) -> ProbeNormalizerProfile {
    let mut profile = profile(None);
    profile.stack_parser_version = Some(parser);
    profile.profile_digest =
        derive_probe_normalizer_profile_digest(&profile).expect("stack profile digest");
    profile
}

fn template(
    plan: &DebugProbePlan,
    profile: ProbeNormalizerProfile,
    prior: Option<ProbePriorEvidence>,
) -> TrustedPureReadTemplate {
    let probe = &plan.probes[0];
    TrustedPureReadTemplate::try_new(
        probe.kind.clone(),
        probe.command.clone(),
        probe.resources.clone(),
        probe.timeout_millis,
        probe.output_limit_bytes,
        profile,
        prior,
    )
    .expect("trusted template")
}

fn request(plan: &DebugProbePlan, workspace: &TempDir) -> ProbeRoundRequest {
    ProbeRoundRequest::new(plan.authority.clone(), plan.clone(), workspace.path())
}

async fn assert_stop_restart(
    round: char,
    termination: ProbeRunTermination,
    delay: Duration,
    wall_limit_millis: i64,
    expected_status: ProbeRoundReceiptStatus,
    expected_reason: ProbeRoundCompletionReason,
) {
    let root = TempDir::new().expect("stop durable root");
    let workspace = TempDir::new().expect("stop workspace");
    let mut plan = plan(round, '1', 1024);
    plan.probes[0].timeout_millis = wall_limit_millis;
    plan.budget.wall_time_limit_millis = wall_limit_millis;
    refresh(&mut plan);
    let runner = ArtifactRunner::new(Vec::new(), false)
        .with_process(termination, delay)
        .fail_after_retain();
    let observer = runner.clone();
    let first = ProbeScheduler::open_with_clock(
        root.path(),
        runner,
        vec![template(&plan, profile(None), None)],
        StepClock::default(),
    )
    .expect("stop scheduler");
    assert_eq!(
        first
            .run_round(request(&plan, &workspace))
            .await
            .expect_err("fault before round receipt")
            .code(),
        &DebugProbeErrorCode::InfrastructureError
    );
    assert_eq!(observer.calls(), 1);
    drop(first);

    let no_run = ArtifactRunner::new(Vec::new(), false);
    let no_run_observer = no_run.clone();
    let reopened =
        ProbeScheduler::open_with_clock(root.path(), no_run, Vec::new(), StepClock::default())
            .expect("reopen stop scheduler");
    let request = request(&plan, &workspace);
    let receipt = reopened
        .run_round(request.clone())
        .await
        .expect("resume exact stop");
    assert_eq!(receipt.status, expected_status);
    assert_eq!(receipt.completion_reason, expected_reason);
    assert!(receipt.usage.elapsed_millis > 0);
    assert!(receipt.usage.elapsed_millis <= wall_limit_millis);
    assert_eq!(
        reopened.run_round(request).await.expect("terminal replay"),
        receipt
    );
    assert_eq!(no_run_observer.calls(), 0);
}

#[tokio::test]
async fn worker_assembles_only_bounded_round_evidence_with_its_fixed_scanner() {
    let raw = b"raw log token=abcdefghijklmnop\n".to_vec();
    let root = TempDir::new().expect("durable root");
    let workspace = TempDir::new().expect("workspace");
    let plan = plan('J', '1', raw.len());
    let scheduler = ProbeScheduler::open_with_clock(
        root.path(),
        ArtifactRunner::new(raw.clone(), false),
        vec![template(&plan, profile(None), None)],
        StepClock::default(),
    )
    .expect("scheduler");
    let request = request(&plan, &workspace);
    let receipt = scheduler
        .run_round(request.clone())
        .await
        .expect("terminal round");
    let records = scheduler
        .evidence_for_round(&request)
        .expect("bounded evidence");
    let projections = records
        .iter()
        .map(|record| {
            project_probe_evidence(record.bundle(), record.bundle_artifact_ref().clone())
                .expect("reproject exact retained evidence")
        })
        .collect::<Vec<_>>();
    let sealed_plan = seal_debug_probe_plan(plan.clone(), &plan.authority).expect("validated plan");
    let receipt_bytes = canonical_probe_round_receipt_bytes(&receipt).expect("receipt bytes");
    let round_evidence = seal_debug_hypothesis_round_evidence(
        &sealed_plan,
        &receipt,
        ArtifactReference {
            artifact_id: ArtifactId("art_RRRRRRRRRRRRRRRRRRRRRRRRRR".to_owned()),
            digest: content_digest(&receipt_bytes),
        },
        &projections,
    )
    .expect("sealed round evidence");
    let hypothesis = &plan.probes[0].target_hypothesis_ids[0];
    let ledger = DebugHypothesisLedgerReducer::initialize(DebugHypothesisLedgerSeed {
        authority: plan.authority.clone(),
        created_at: plan.created_at.clone(),
        hypotheses: vec![DebugHypothesis {
            confidence_bps: 0,
            contradicting_evidence: Vec::new(),
            created_round_id: plan.authority.round_id.clone(),
            hypothesis_id: hypothesis.clone(),
            last_updated_round_id: plan.authority.round_id.clone(),
            status: DebugHypothesisStatus::Active,
            summary: "The current revision may fail its static check".to_owned(),
            supporting_evidence: Vec::new(),
        }],
        unresolved_questions: Vec::new(),
    })
    .expect("validated ledger");

    let prepared = prepare_worker_debug_probe_context(&WorkerDebugProbeContextInput {
        current_ledger: ledger.ledger(),
        previous_context: None,
        previous_ledger: None,
        round_evidence: &round_evidence,
    })
    .expect("Worker context");
    assert!(prepared.context().snippets.is_empty());
    assert_eq!(prepared.context().omitted_snippet_count, 0);
    assert_eq!(
        prepared.context().safety_profile,
        *WorkerContextSafetyScanner.profile()
    );
    let payload = std::str::from_utf8(prepared.canonical_bytes()).expect("UTF-8 context");
    assert!(!payload.contains("raw log token=abcdefghijklmnop"));
}

#[tokio::test]
async fn host_selected_prior_bootstraps_an_exact_comparable_baseline() {
    let root = TempDir::new().expect("durable root");
    let workspace = TempDir::new().expect("workspace");
    let first_plan = plan('A', '1', 1024);
    let parser = profile(Some(DiagnosticParserVersion::TypescriptV1));
    let first_runner = ArtifactRunner::new(
        b"src/one.ts(1,2): error TS2304: Cannot find name 'missing'.\n".to_vec(),
        false,
    );
    let first = ProbeScheduler::open_with_clock(
        root.path(),
        first_runner,
        vec![template(&first_plan, parser.clone(), None)],
        StepClock::default(),
    )
    .expect("first scheduler");
    first
        .run_round(request(&first_plan, &workspace))
        .await
        .expect("first round");
    let first_evidence = first
        .evidence_for_round(&request(&first_plan, &workspace))
        .expect("first evidence");
    assert_eq!(first_evidence.len(), 1);
    assert_eq!(
        first_evidence[0].bundle().bundle().baseline.state,
        winwincode_execution_port::generated::ProbeBaselineState::Unavailable
    );

    let second_plan = plan('B', '1', 1024);
    let second = ProbeScheduler::open_with_clock(
        root.path(),
        ArtifactRunner::new(
            b"src/one.ts(1,2): error TS2304: Cannot find name 'missing'.\n".to_vec(),
            false,
        ),
        vec![template(
            &second_plan,
            parser,
            Some(first_evidence[0].prior_evidence()),
        )],
        StepClock::default(),
    )
    .expect("second scheduler");
    second
        .run_round(request(&second_plan, &workspace))
        .await
        .expect("second round");
    let second_evidence = second
        .evidence_for_round(&request(&second_plan, &workspace))
        .expect("second evidence");
    let bundle = second_evidence[0].bundle().bundle();
    assert_eq!(
        bundle.baseline.state,
        winwincode_execution_port::generated::ProbeBaselineState::Available
    );
    assert!(bundle.baseline.comparison.is_some());
    assert_eq!(
        bundle.profile.profile_digest,
        profile(Some(DiagnosticParserVersion::TypescriptV1)).profile_digest
    );
}

#[tokio::test]
async fn thirty_thousand_duplicates_are_one_bounded_occurrence_and_replay_exactly() {
    let output = fixtures::duplicate_log_30k();
    let root = TempDir::new().expect("durable root");
    let workspace = TempDir::new().expect("workspace");
    let plan = plan('C', '1', output.len());
    let runner = ArtifactRunner::new(output, false);
    let observer = runner.clone();
    let scheduler = ProbeScheduler::open_with_clock(
        root.path(),
        runner,
        vec![template(
            &plan,
            profile(Some(DiagnosticParserVersion::TypescriptV1)),
            None,
        )],
        StepClock::default(),
    )
    .expect("scheduler");
    let request = request(&plan, &workspace);
    let first = scheduler.run_round(request.clone()).await.expect("round");
    let record = scheduler
        .evidence_for_round(&request)
        .expect("evidence")
        .pop()
        .expect("record");
    assert_eq!(record.summary().unique_diagnostic_count, 1);
    assert_eq!(record.summary().diagnostic_occurrence_count, 30_000);
    assert_eq!(
        record.bundle().bundle().diagnostics[0].occurrence_count,
        30_000
    );
    assert!(!record.summary().summary.contains("fixture validation"));
    assert!(record.summary().summary.chars().count() <= 500);
    let candidates = serde_json::to_string(record.candidates()).expect("candidate JSON");
    assert!(!candidates.contains("confidence"));
    assert!(!candidates.contains("polarity"));
    assert_eq!(first.probe_receipts[0].artifact_refs.len(), 2);
    assert_ne!(
        first.probe_receipts[0].artifact_refs[0],
        *record.bundle_artifact_ref()
    );
    assert_eq!(scheduler.run_round(request).await.expect("replay"), first);
    assert_eq!(observer.calls(), 1);
}

#[tokio::test]
async fn receipt_retained_restart_normalizes_without_rerunning_or_reselecting_profile() {
    let root = TempDir::new().expect("durable root");
    let workspace = TempDir::new().expect("workspace");
    let plan = plan('D', '1', 1024);
    let mut stderr =
        include_bytes!("fixtures/probe-result-normalizer/v1/generic-command.stderr.txt").to_vec();
    let canonical_workspace = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    let stack = String::from_utf8(
        include_bytes!("fixtures/probe-result-normalizer/v1/rust-stack.stderr.txt").to_vec(),
    )
    .expect("UTF-8 fixture")
    .replace(
        "/workspace",
        canonical_workspace.to_str().expect("UTF-8 workspace"),
    );
    stderr.extend_from_slice(stack.as_bytes());
    let runner = ArtifactRunner::new(Vec::new(), false)
        .with_stderr(stderr)
        .fail_after_retain();
    let observer = runner.clone();
    let first = ProbeScheduler::open_with_clock(
        root.path(),
        runner,
        vec![template(
            &plan,
            stack_profile(ProbeStackParserVersion::RustV1),
            None,
        )],
        StepClock::default(),
    )
    .expect("first scheduler");
    assert_eq!(
        first
            .run_round(request(&plan, &workspace))
            .await
            .expect_err("fault after retain")
            .code(),
        &DebugProbeErrorCode::InfrastructureError
    );
    assert_eq!(observer.calls(), 1);
    drop(first);

    let no_run = ArtifactRunner::new(b"different bytes\n".to_vec(), false);
    let no_run_observer = no_run.clone();
    let reopened =
        ProbeScheduler::open_with_clock(root.path(), no_run, Vec::new(), StepClock::default())
            .expect("reopen");
    let changed_workspace = TempDir::new().expect("changed workspace");
    assert_eq!(
        reopened
            .run_round(request(&plan, &changed_workspace))
            .await
            .expect_err("changed canonical root")
            .code(),
        &DebugProbeErrorCode::StaleAuthority
    );
    let receipt = reopened
        .run_round(request(&plan, &workspace))
        .await
        .expect("resume retained receipt");
    assert_eq!(
        receipt.probe_receipts[0].status,
        ProbeReceiptStatus::Succeeded
    );
    assert_eq!(no_run_observer.calls(), 0);
    let evidence = reopened
        .evidence_for_round(&request(&plan, &workspace))
        .expect("recovered evidence");
    assert_eq!(
        evidence[0].bundle().bundle().profile,
        stack_profile(ProbeStackParserVersion::RustV1)
    );
    assert_eq!(evidence[0].summary().stack_cluster_count, 1);
    assert_eq!(
        evidence[0].summary().completeness.status,
        ProbeEvidenceCompletenessStatus::Complete
    );

    assert_stop_restart(
        'G',
        ProbeRunTermination::Cancelled,
        Duration::from_millis(5),
        5_000,
        ProbeRoundReceiptStatus::Cancelled,
        ProbeRoundCompletionReason::Cancelled,
    )
    .await;
    assert_stop_restart(
        'H',
        ProbeRunTermination::Exited,
        Duration::from_millis(25),
        10,
        ProbeRoundReceiptStatus::Failed,
        ProbeRoundCompletionReason::BudgetExhausted,
    )
    .await;
}

#[tokio::test]
async fn incomplete_raw_inputs_are_table_driven_and_never_project_clean() {
    struct Case {
        round: char,
        bytes: Vec<u8>,
        truncated: bool,
        reason: ProbeEvidenceIncompleteReason,
    }
    let cases = [
        Case {
            round: 'E',
            bytes: vec![0xff, b'!'],
            truncated: false,
            reason: ProbeEvidenceIncompleteReason::InvalidUtf8,
        },
        Case {
            round: 'F',
            bytes: b"src/x.ts(1,1): error TS2304: missing".to_vec(),
            truncated: true,
            reason: ProbeEvidenceIncompleteReason::OutputTruncated,
        },
    ];
    for case in cases {
        let root = TempDir::new().expect("durable root");
        let workspace = TempDir::new().expect("workspace");
        let plan = plan(case.round, '1', case.bytes.len());
        let scheduler = ProbeScheduler::open_with_clock(
            root.path(),
            ArtifactRunner::new(case.bytes, case.truncated),
            vec![template(
                &plan,
                profile(Some(DiagnosticParserVersion::TypescriptV1)),
                None,
            )],
            StepClock::default(),
        )
        .expect("scheduler");
        scheduler
            .run_round(request(&plan, &workspace))
            .await
            .expect("round");
        let evidence = scheduler
            .evidence_for_round(&request(&plan, &workspace))
            .expect("evidence");
        assert_eq!(
            evidence[0].summary().completeness.status,
            ProbeEvidenceCompletenessStatus::Incomplete
        );
        assert!(
            evidence[0]
                .summary()
                .completeness
                .reasons
                .contains(&case.reason)
        );
    }
}
