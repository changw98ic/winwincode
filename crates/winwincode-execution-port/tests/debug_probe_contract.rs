// SPDX-License-Identifier: Apache-2.0

use winwincode_domain::{
    CodexThreadId, DebugHypothesisId, DebugSessionId, ExecutionJobId, ExecutionSequence,
    FencingToken, Instant, LeaseId, ProbeId, ProbeRoundId, ProductSessionId, RepositoryId,
    SessionIdentity, Sha256Digest, WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::{
    debug_probe_contract::{
        ValidatedDebugProbePlan, ValidatedProbeExecutionIntent, derive_debug_probe_plan_digest,
        derive_probe_budget_digest, derive_probe_command_arg_bytes, derive_probe_definition_digest,
        derive_probe_execution_id, seal_debug_probe_plan, seal_probe_execution_intent,
        validate_probe_execution_events, validate_probe_execution_history,
        validate_probe_execution_receipt, validate_probe_round_events,
        validate_probe_round_history, validate_probe_round_receipt,
    },
    generated::{
        DebugProbeError, DebugProbeErrorCode, DebugProbeIdentity, DebugProbeKind, DebugProbePlan,
        DebugProbeRoundAuthority, ProbeCommandSpec, ProbeCompletionRule, ProbeCompletionRuleKind,
        ProbeExecutionEvent, ProbeExecutionEventKind, ProbeExecutionIntent, ProbeExecutionReceipt,
        ProbeExecutionStatus, ProbeNetworkAccess, ProbeReceiptStatus, ProbeResourceClaim,
        ProbeRoundBudget, ProbeRoundBudgetUsage, ProbeRoundCompletionReason, ProbeRoundEvent,
        ProbeRoundEventKind, ProbeRoundReceipt, ProbeRoundReceiptStatus, ProbeRoundStatus,
        ProbeSideEffectClass, ProbeSpec, ProbeWorkspaceAccess,
    },
};

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
            stage_run_id: None,
            worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
        },
        workspace_revision: WorkspaceRevision(format!("git-tree:{}", "0".repeat(40))),
    }
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn base_probe() -> ProbeSpec {
    ProbeSpec {
        command: ProbeCommandSpec {
            argv: vec![
                "corepack".to_owned(),
                "pnpm".to_owned(),
                "typecheck".to_owned(),
            ],
            command_arg_bytes: 1,
            working_directory: ".".to_owned(),
        },
        kind: DebugProbeKind::StaticAnalysis,
        output_limit_bytes: 1_048_576,
        probe_definition_digest: digest('2'),
        probe_id: ProbeId("prb_00000000000000000000000000".to_owned()),
        required: true,
        resources: ProbeResourceClaim {
            cpu_limit_millis: 10_000,
            database_keys: Vec::new(),
            exclusive_keys: Vec::new(),
            memory_limit_bytes: 134_217_728,
            network_access: ProbeNetworkAccess::None,
            paths: vec!["src/example.ts".to_owned()],
            port_numbers: Vec::new(),
            service_keys: Vec::new(),
            side_effect_class: ProbeSideEffectClass::PureRead,
            workspace_access: ProbeWorkspaceAccess::ReadOnly,
        },
        target_hypothesis_ids: vec![DebugHypothesisId(
            "hyp_00000000000000000000000000".to_owned(),
        )],
        timeout_millis: 300_000,
    }
}

fn base_plan() -> DebugProbePlan {
    let mut plan = DebugProbePlan {
        authority: authority(),
        budget: ProbeRoundBudget {
            budget_digest: digest('6'),
            parallel_probe_limit: 1,
            peak_memory_limit_bytes: 1_073_741_824,
            probe_limit: 8,
            total_command_arg_limit_bytes: 262_144,
            total_cpu_limit_millis: 2_400_000,
            total_output_limit_bytes: 8_388_608,
            wall_time_limit_millis: 600_000,
        },
        completion_rule: ProbeCompletionRule {
            kind: ProbeCompletionRuleKind::AllTerminal,
            minimum_completed_probes: 1,
            minimum_successful_probes: 1,
            stop_on_required_probe_failure: true,
        },
        created_at: Instant("2026-09-06T08:00:00.000Z".to_owned()),
        plan_digest: digest('3'),
        probes: vec![base_probe()],
        schema_version: 1,
    };
    refresh_derivations(&mut plan);
    plan
}

fn refresh_derivations(plan: &mut DebugProbePlan) {
    for probe in &mut plan.probes {
        probe.command.command_arg_bytes =
            derive_probe_command_arg_bytes(&probe.command.argv).expect("command arg bytes");
        probe.probe_definition_digest =
            derive_probe_definition_digest(probe).expect("probe definition digest");
    }
    plan.budget.budget_digest = derive_probe_budget_digest(&plan.budget).expect("budget digest");
    plan.plan_digest = derive_debug_probe_plan_digest(plan).expect("plan digest");
}

fn sealed_plan() -> ValidatedDebugProbePlan {
    let plan = base_plan();
    seal_debug_probe_plan(plan.clone(), &plan.authority).expect("sealed plan")
}

fn base_intent(plan: &ValidatedDebugProbePlan) -> ProbeExecutionIntent {
    let probe = &plan.probes()[0];
    ProbeExecutionIntent {
        created_at: Instant("2026-09-06T08:00:01.000Z".to_owned()),
        identity: plan
            .probe_identity(&probe.spec().probe_id)
            .expect("probe identity"),
        plan_digest: plan.plan().plan_digest.clone(),
        schema_version: 1,
        spec: probe.spec().clone(),
    }
}

fn sealed_intent(plan: &ValidatedDebugProbePlan) -> ValidatedProbeExecutionIntent {
    seal_probe_execution_intent(base_intent(plan), plan).expect("sealed intent")
}

fn probe_error(code: DebugProbeErrorCode) -> DebugProbeError {
    DebugProbeError {
        code,
        message: "bounded failure".to_owned(),
        retryable: false,
    }
}

fn success_receipt(intent: &ValidatedProbeExecutionIntent) -> ProbeExecutionReceipt {
    ProbeExecutionReceipt {
        artifact_refs: Vec::new(),
        duration_millis: 10,
        error: None,
        exit_code: Some(0),
        finished_at: Instant("2026-09-06T08:00:03.000Z".to_owned()),
        identity: intent.intent().identity.clone(),
        output_bytes: 5,
        output_truncated: false,
        plan_digest: intent.intent().plan_digest.clone(),
        schema_version: 1,
        signal: None,
        started_at: Instant("2026-09-06T08:00:02.000Z".to_owned()),
        status: ProbeReceiptStatus::Succeeded,
        timed_out: false,
    }
}

enum ReceiptMutation {
    StaleIdentity,
    SuccessExit,
    TimeoutFlag,
    SkippedOutput,
    CacheHitExit,
    StaleError,
    SignalAndExit,
    OutputOverflow,
    TimeOrder,
}

enum RoundMutation {
    ReceiptOrder,
    UsageDigest,
    OutputAccounting,
    StatusReason,
    ProbeOutsideRound,
}

fn terminal_receipt(
    intent: &ValidatedProbeExecutionIntent,
    status: &ProbeReceiptStatus,
) -> ProbeExecutionReceipt {
    let mut receipt = success_receipt(intent);
    receipt.status = status.clone();
    match status {
        ProbeReceiptStatus::Succeeded => {}
        ProbeReceiptStatus::Failed => {
            receipt.exit_code = Some(1);
            receipt.error = Some(probe_error(DebugProbeErrorCode::InvalidProbe));
        }
        ProbeReceiptStatus::TimedOut => {
            receipt.exit_code = None;
            receipt.timed_out = true;
            receipt.error = Some(probe_error(DebugProbeErrorCode::TimedOut));
        }
        ProbeReceiptStatus::Cancelled => {
            receipt.exit_code = None;
            receipt.error = Some(probe_error(DebugProbeErrorCode::Cancelled));
        }
        ProbeReceiptStatus::Skipped => {
            receipt.exit_code = None;
            receipt.duration_millis = 0;
            receipt.output_bytes = 0;
        }
        ProbeReceiptStatus::CacheHit => {
            receipt.exit_code = None;
            receipt.duration_millis = 0;
        }
        ProbeReceiptStatus::Stale => {
            receipt.exit_code = None;
            receipt.error = Some(probe_error(DebugProbeErrorCode::StaleAuthority));
        }
    }
    receipt
}

fn probe_event_history(intent: &ValidatedProbeExecutionIntent) -> Vec<ProbeExecutionEvent> {
    let identity: DebugProbeIdentity = intent.intent().identity.clone();
    vec![
        ProbeExecutionEvent {
            artifact_refs: Vec::new(),
            identity: identity.clone(),
            kind: ProbeExecutionEventKind::Scheduled,
            occurred_at: Instant("2026-09-06T08:00:01.000Z".to_owned()),
            sequence: ExecutionSequence(1),
            status: ProbeExecutionStatus::Scheduled,
            summary: "scheduled".to_owned(),
        },
        ProbeExecutionEvent {
            artifact_refs: Vec::new(),
            identity: identity.clone(),
            kind: ProbeExecutionEventKind::Started,
            occurred_at: Instant("2026-09-06T08:00:02.000Z".to_owned()),
            sequence: ExecutionSequence(2),
            status: ProbeExecutionStatus::Running,
            summary: "started".to_owned(),
        },
        ProbeExecutionEvent {
            artifact_refs: Vec::new(),
            identity,
            kind: ProbeExecutionEventKind::Finished,
            occurred_at: Instant("2026-09-06T08:00:03.000Z".to_owned()),
            sequence: ExecutionSequence(3),
            status: ProbeExecutionStatus::Succeeded,
            summary: "finished".to_owned(),
        },
    ]
}

fn round_event_history(plan: &ValidatedDebugProbePlan) -> Vec<ProbeRoundEvent> {
    vec![
        ProbeRoundEvent {
            authority: plan.plan().authority.clone(),
            kind: ProbeRoundEventKind::Planned,
            occurred_at: Instant("2026-09-06T08:00:00.000Z".to_owned()),
            sequence: ExecutionSequence(1),
            status: ProbeRoundStatus::Planned,
            summary: "planned".to_owned(),
        },
        ProbeRoundEvent {
            authority: plan.plan().authority.clone(),
            kind: ProbeRoundEventKind::Started,
            occurred_at: Instant("2026-09-06T08:00:01.000Z".to_owned()),
            sequence: ExecutionSequence(2),
            status: ProbeRoundStatus::Running,
            summary: "started".to_owned(),
        },
        ProbeRoundEvent {
            authority: plan.plan().authority.clone(),
            kind: ProbeRoundEventKind::Finished,
            occurred_at: Instant("2026-09-06T08:00:04.000Z".to_owned()),
            sequence: ExecutionSequence(3),
            status: ProbeRoundStatus::Completed,
            summary: "finished".to_owned(),
        },
    ]
}

fn successful_round_receipt(
    plan: &ValidatedDebugProbePlan,
    intent: &ValidatedProbeExecutionIntent,
) -> ProbeRoundReceipt {
    let receipt = success_receipt(intent);
    ProbeRoundReceipt {
        authority: plan.plan().authority.clone(),
        completion_reason: ProbeRoundCompletionReason::AllProbesTerminal,
        error: None,
        finished_at: Instant("2026-09-06T08:00:04.000Z".to_owned()),
        plan_digest: plan.plan().plan_digest.clone(),
        probe_receipts: vec![receipt.clone()],
        schema_version: 1,
        started_at: Instant("2026-09-06T08:00:01.000Z".to_owned()),
        status: ProbeRoundReceiptStatus::Completed,
        usage: ProbeRoundBudgetUsage {
            budget_digest: plan.plan().budget.budget_digest.clone(),
            elapsed_millis: 3_000,
            peak_memory_bytes: 100,
            peak_parallel_probes: 1,
            probe_count: 1,
            total_command_arg_bytes: intent.probe().command_arg_bytes(),
            total_cpu_millis: 10,
            total_output_bytes: receipt.output_bytes,
        },
    }
}

#[test]
fn canonical_derivations_are_framed_stable_and_domain_separated() {
    let plan = base_plan();
    assert_eq!(
        derive_probe_command_arg_bytes(&plan.probes[0].command.argv).expect("argv bytes"),
        53
    );
    assert_eq!(
        derive_probe_definition_digest(&plan.probes[0]).expect("probe digest"),
        plan.probes[0].probe_definition_digest
    );
    assert_eq!(
        derive_probe_budget_digest(&plan.budget).expect("budget digest"),
        plan.budget.budget_digest
    );
    assert_eq!(
        derive_debug_probe_plan_digest(&plan).expect("plan digest"),
        plan.plan_digest
    );
    let execution_id =
        derive_probe_execution_id(&plan.authority, &plan.plan_digest, &plan.probes[0])
            .expect("execution id");
    assert_eq!(
        plan.probes[0].probe_definition_digest.0,
        "sha256:3da855e7f2724a7c0599b19bd19d93512fe853c9e6b522998dc650f0d8805854"
    );
    assert_eq!(
        plan.budget.budget_digest.0,
        "sha256:5e6c2f7ac96ac63108c50ef73085a2ba66cd8a95b0783adb65ed243a20a9e2ac"
    );
    assert_eq!(
        plan.plan_digest.0,
        "sha256:3834065a34f140fc17cd02c472257901d850d7307d6538ddfcbb62bc1d81f912"
    );
    assert_eq!(
        execution_id.0,
        "sha256:9e4a7a5060eecad867999f9ff6f8407f1fdedaa083b2d82d05ec1675998b6d8f"
    );
    assert_ne!(execution_id.0, plan.plan_digest.0);
    assert_ne!(execution_id.0, plan.probes[0].probe_definition_digest.0);

    let mut reordered = plan.probes[0].clone();
    reordered.resources.paths = vec!["tests/b.ts".to_owned(), "src/a.ts".to_owned()];
    reordered.resources.service_keys = vec!["service:b".to_owned(), "service:a".to_owned()];
    let left = derive_probe_definition_digest(&reordered).expect("set digest");
    reordered.resources.paths.reverse();
    reordered.resources.service_keys.reverse();
    assert_eq!(
        derive_probe_definition_digest(&reordered).expect("reordered set digest"),
        left,
        "set-valued resource order must not change identity"
    );
    reordered.command.argv.swap(1, 2);
    assert_ne!(
        derive_probe_definition_digest(&reordered).expect("ordered argv digest"),
        left,
        "argv order must remain significant"
    );
}

#[test]
fn plan_semantics_reject_identity_digest_limit_and_completion_drift() {
    enum Mutation {
        StaleAuthority,
        CommandBytes,
        ProbeDigest,
        BudgetDigest,
        PlanDigest,
        DuplicateProbe,
        AggregateOutput,
        Timeout,
        CompletionThreshold,
        FirstConclusive,
        InvalidInstant,
    }
    let cases = [
        (
            Mutation::StaleAuthority,
            DebugProbeErrorCode::StaleAuthority,
        ),
        (Mutation::CommandBytes, DebugProbeErrorCode::InvalidProbe),
        (Mutation::ProbeDigest, DebugProbeErrorCode::InvalidProbe),
        (Mutation::BudgetDigest, DebugProbeErrorCode::InvalidPlan),
        (Mutation::PlanDigest, DebugProbeErrorCode::InvalidPlan),
        (Mutation::DuplicateProbe, DebugProbeErrorCode::InvalidPlan),
        (
            Mutation::AggregateOutput,
            DebugProbeErrorCode::BudgetExceeded,
        ),
        (Mutation::Timeout, DebugProbeErrorCode::BudgetExceeded),
        (
            Mutation::CompletionThreshold,
            DebugProbeErrorCode::InvalidPlan,
        ),
        (Mutation::FirstConclusive, DebugProbeErrorCode::InvalidPlan),
        (Mutation::InvalidInstant, DebugProbeErrorCode::InvalidPlan),
    ];

    for (mutation, expected_code) in cases {
        let mut plan = base_plan();
        let mut expected = plan.authority.clone();
        match mutation {
            Mutation::StaleAuthority => expected.fencing_token = FencingToken("8".to_owned()),
            Mutation::CommandBytes => plan.probes[0].command.command_arg_bytes += 1,
            Mutation::ProbeDigest => plan.probes[0].probe_definition_digest = digest('a'),
            Mutation::BudgetDigest => plan.budget.budget_digest = digest('b'),
            Mutation::PlanDigest => plan.plan_digest = digest('c'),
            Mutation::DuplicateProbe => plan.probes.push(plan.probes[0].clone()),
            Mutation::AggregateOutput => {
                plan.budget.total_output_limit_bytes = 1;
                refresh_derivations(&mut plan);
            }
            Mutation::Timeout => {
                plan.budget.wall_time_limit_millis = 1;
                refresh_derivations(&mut plan);
            }
            Mutation::CompletionThreshold => {
                let mut second = plan.probes[0].clone();
                second.probe_id = ProbeId("prb_11111111111111111111111111".to_owned());
                plan.probes.push(second);
                plan.completion_rule.kind = ProbeCompletionRuleKind::MinimumSuccesses;
                plan.completion_rule.minimum_completed_probes = 1;
                plan.completion_rule.minimum_successful_probes = 2;
                refresh_derivations(&mut plan);
            }
            Mutation::FirstConclusive => {
                plan.completion_rule.kind = ProbeCompletionRuleKind::FirstConclusive;
                refresh_derivations(&mut plan);
            }
            Mutation::InvalidInstant => {
                plan.created_at = Instant("2026-09-39T08:00:00.000Z".to_owned());
            }
        }
        let error = seal_debug_probe_plan(plan, &expected).expect_err("invalid plan");
        assert_eq!(error.code(), &expected_code);
    }
}

#[test]
fn intent_binds_to_one_exact_execution() {
    let plan = sealed_plan();
    let intent = sealed_intent(&plan);
    validate_probe_execution_receipt(&success_receipt(&intent), &intent)
        .expect("valid success receipt");
    let mut stale_intent = base_intent(&plan);
    stale_intent.identity.fencing_token = FencingToken("8".to_owned());
    assert_eq!(
        seal_probe_execution_intent(stale_intent, &plan)
            .expect_err("stale intent")
            .code(),
        &DebugProbeErrorCode::StaleAuthority
    );
}

#[test]
fn terminal_receipt_status_table_is_exact() {
    let plan = sealed_plan();
    let intent = sealed_intent(&plan);
    for status in [
        ProbeReceiptStatus::Succeeded,
        ProbeReceiptStatus::Failed,
        ProbeReceiptStatus::TimedOut,
        ProbeReceiptStatus::Cancelled,
        ProbeReceiptStatus::Skipped,
        ProbeReceiptStatus::CacheHit,
        ProbeReceiptStatus::Stale,
    ] {
        validate_probe_execution_receipt(&terminal_receipt(&intent, &status), &intent)
            .expect("valid terminal status");
    }
    let mut cleanup_failed = success_receipt(&intent);
    cleanup_failed.status = ProbeReceiptStatus::Failed;
    cleanup_failed.error = Some(probe_error(DebugProbeErrorCode::ProcessCleanupFailed));
    validate_probe_execution_receipt(&cleanup_failed, &intent)
        .expect("successful child exit may still have failed cleanup");
}

#[test]
fn receipt_status_table_rejects_cross_field_drift() {
    let plan = sealed_plan();
    let intent = sealed_intent(&plan);
    for mutation in [
        ReceiptMutation::StaleIdentity,
        ReceiptMutation::SuccessExit,
        ReceiptMutation::TimeoutFlag,
        ReceiptMutation::SkippedOutput,
        ReceiptMutation::CacheHitExit,
        ReceiptMutation::StaleError,
        ReceiptMutation::SignalAndExit,
        ReceiptMutation::OutputOverflow,
        ReceiptMutation::TimeOrder,
    ] {
        let mut receipt = success_receipt(&intent);
        match mutation {
            ReceiptMutation::StaleIdentity => {
                receipt.identity.environment_digest = digest('9');
            }
            ReceiptMutation::SuccessExit => receipt.exit_code = Some(1),
            ReceiptMutation::TimeoutFlag => {
                receipt.status = ProbeReceiptStatus::TimedOut;
                receipt.exit_code = None;
                receipt.error = Some(probe_error(DebugProbeErrorCode::TimedOut));
            }
            ReceiptMutation::SkippedOutput => {
                receipt.status = ProbeReceiptStatus::Skipped;
                receipt.exit_code = None;
                receipt.duration_millis = 0;
            }
            ReceiptMutation::CacheHitExit => receipt.status = ProbeReceiptStatus::CacheHit,
            ReceiptMutation::StaleError => {
                receipt.status = ProbeReceiptStatus::Stale;
                receipt.exit_code = None;
                receipt.error = Some(probe_error(DebugProbeErrorCode::Cancelled));
            }
            ReceiptMutation::SignalAndExit => receipt.signal = Some("SIGTERM".to_owned()),
            ReceiptMutation::OutputOverflow => {
                receipt.output_bytes = intent.probe().spec().output_limit_bytes + 1;
            }
            ReceiptMutation::TimeOrder => {
                receipt.finished_at = Instant("2026-09-06T08:00:01.000Z".to_owned());
            }
        }
        assert!(
            validate_probe_execution_receipt(&receipt, &intent).is_err(),
            "invalid receipt mutation must fail"
        );
    }
}

#[test]
fn minimum_successes_does_not_count_skipped_probes_as_completed() {
    let mut plan = base_plan();
    let mut second = plan.probes[0].clone();
    second.probe_id = ProbeId("prb_11111111111111111111111111".to_owned());
    plan.probes.push(second);
    plan.completion_rule.kind = ProbeCompletionRuleKind::MinimumSuccesses;
    plan.completion_rule.minimum_completed_probes = 2;
    plan.completion_rule.minimum_successful_probes = 1;
    plan.completion_rule.stop_on_required_probe_failure = false;
    refresh_derivations(&mut plan);
    let sealed = seal_debug_probe_plan(plan.clone(), &plan.authority).expect("two-probe plan");
    let first_intent =
        seal_probe_execution_intent(base_intent(&sealed), &sealed).expect("first sealed intent");
    let second_probe = &sealed.probes()[1];
    let second_intent = seal_probe_execution_intent(
        ProbeExecutionIntent {
            created_at: Instant("2026-09-06T08:00:01.000Z".to_owned()),
            identity: sealed
                .probe_identity(&second_probe.spec().probe_id)
                .expect("second identity"),
            plan_digest: sealed.plan().plan_digest.clone(),
            schema_version: 1,
            spec: second_probe.spec().clone(),
        },
        &sealed,
    )
    .expect("second sealed intent");
    let success = success_receipt(&first_intent);
    let mut skipped = success_receipt(&second_intent);
    skipped.status = ProbeReceiptStatus::Skipped;
    skipped.exit_code = None;
    skipped.duration_millis = 0;
    skipped.output_bytes = 0;
    let receipt = ProbeRoundReceipt {
        authority: sealed.plan().authority.clone(),
        completion_reason: ProbeRoundCompletionReason::CompletionRuleSatisfied,
        error: None,
        finished_at: Instant("2026-09-06T08:00:04.000Z".to_owned()),
        plan_digest: sealed.plan().plan_digest.clone(),
        probe_receipts: vec![success.clone(), skipped],
        schema_version: 1,
        started_at: Instant("2026-09-06T08:00:01.000Z".to_owned()),
        status: ProbeRoundReceiptStatus::Completed,
        usage: ProbeRoundBudgetUsage {
            budget_digest: sealed.plan().budget.budget_digest.clone(),
            elapsed_millis: 3_000,
            peak_memory_bytes: 100,
            peak_parallel_probes: 1,
            probe_count: 2,
            total_command_arg_bytes: first_intent.probe().command_arg_bytes(),
            total_cpu_millis: 10,
            total_output_bytes: success.output_bytes,
        },
    };
    assert_eq!(
        validate_probe_round_receipt(&receipt, &sealed)
            .expect_err("skipped probe is not completed")
            .code(),
        &DebugProbeErrorCode::InvalidPlan
    );
}

#[test]
fn event_and_round_ledgers_enforce_order_status_and_budget_binding() {
    let plan = sealed_plan();
    let intent = sealed_intent(&plan);
    let probe_events = probe_event_history(&intent);
    validate_probe_execution_events(&probe_events, &intent).expect("ordered probe events");
    let round_events = round_event_history(&plan);
    validate_probe_round_events(&round_events, &plan).expect("ordered round events");

    let mut gap = probe_events.clone();
    gap[1].sequence = ExecutionSequence(3);
    assert!(validate_probe_execution_events(&gap, &intent).is_err());
    let mut wrong_transition = round_events.clone();
    wrong_transition[1].status = ProbeRoundStatus::Completed;
    assert!(validate_probe_round_events(&wrong_transition, &plan).is_err());

    let round_receipt = successful_round_receipt(&plan, &intent);
    validate_probe_round_receipt(&round_receipt, &plan).expect("valid round receipt");
    validate_probe_execution_history(&probe_events, &round_receipt.probe_receipts[0], &intent)
        .expect("probe event and receipt history");
    validate_probe_round_history(&round_events, &round_receipt, &plan)
        .expect("round event and receipt history");
    let mut mismatched_probe_terminal = probe_events.clone();
    mismatched_probe_terminal[2].status = ProbeExecutionStatus::Failed;
    assert!(
        validate_probe_execution_history(
            &mismatched_probe_terminal,
            &round_receipt.probe_receipts[0],
            &intent,
        )
        .is_err()
    );
    let mut mismatched_round_terminal = round_events.clone();
    mismatched_round_terminal[2].status = ProbeRoundStatus::Failed;
    assert!(
        validate_probe_round_history(&mismatched_round_terminal, &round_receipt, &plan).is_err()
    );
    for mutation in [
        RoundMutation::ReceiptOrder,
        RoundMutation::UsageDigest,
        RoundMutation::OutputAccounting,
        RoundMutation::StatusReason,
        RoundMutation::ProbeOutsideRound,
    ] {
        let mut changed = round_receipt.clone();
        match mutation {
            RoundMutation::ReceiptOrder => {
                changed.probe_receipts[0].identity.probe_id =
                    ProbeId("prb_11111111111111111111111111".to_owned());
            }
            RoundMutation::UsageDigest => changed.usage.budget_digest = digest('8'),
            RoundMutation::OutputAccounting => changed.usage.total_output_bytes += 1,
            RoundMutation::StatusReason => {
                changed.status = ProbeRoundReceiptStatus::Cancelled;
            }
            RoundMutation::ProbeOutsideRound => {
                changed.probe_receipts[0].started_at =
                    Instant("2026-09-06T08:00:00.500Z".to_owned());
            }
        }
        assert!(validate_probe_round_receipt(&changed, &plan).is_err());
    }
}
