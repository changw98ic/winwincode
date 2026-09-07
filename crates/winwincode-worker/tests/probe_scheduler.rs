// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tempfile::TempDir;
use winwincode_domain::{
    CodexThreadId, DebugHypothesisId, DebugSessionId, ExecutionJobId, FencingToken, Instant,
    LeaseId, ProbeId, ProbeRoundId, ProductSessionId, RepositoryId, SessionIdentity, Sha256Digest,
    WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::{
    debug_probe_contract::{
        derive_debug_probe_plan_digest, derive_probe_budget_digest, derive_probe_command_arg_bytes,
        derive_probe_definition_digest,
    },
    generated::{
        DebugProbeErrorCode, DebugProbeKind, DebugProbePlan, DebugProbeRoundAuthority,
        ProbeCommandSpec, ProbeCompletionRule, ProbeCompletionRuleKind, ProbeNetworkAccess,
        ProbeReceiptStatus, ProbeResourceClaim, ProbeRoundBudget, ProbeRoundCompletionReason,
        ProbeRoundReceiptStatus, ProbeSideEffectClass, ProbeSpec, ProbeWorkspaceAccess,
    },
};
use winwincode_worker::probe_scheduler::{
    AdmittedProbe, ProbeClock, ProbeRoundRequest, ProbeRunCancellation, ProbeRunResult,
    ProbeRunTermination, ProbeRunner, ProbeRunnerFuture, ProbeScheduler, ProbeSchedulerError,
    TrustedPureReadTemplate,
};

#[derive(Clone, Debug)]
struct RecordingRunner {
    state: Arc<RunnerState>,
}

#[derive(Debug)]
struct RunnerState {
    calls: AtomicUsize,
    inflight: AtomicUsize,
    max_inflight: AtomicUsize,
    events: Mutex<Vec<String>>,
    outcomes: BTreeMap<String, ProbeRunTermination>,
    delay: Duration,
    validation_delay: Duration,
    reject: AtomicBool,
    cancel_before_return: AtomicBool,
    pause: AtomicBool,
    entered: AtomicBool,
    released: AtomicBool,
}

impl RecordingRunner {
    fn new(delay: Duration) -> Self {
        Self {
            state: Arc::new(RunnerState {
                calls: AtomicUsize::new(0),
                inflight: AtomicUsize::new(0),
                max_inflight: AtomicUsize::new(0),
                events: Mutex::new(Vec::new()),
                outcomes: BTreeMap::new(),
                delay,
                validation_delay: Duration::ZERO,
                reject: AtomicBool::new(false),
                cancel_before_return: AtomicBool::new(false),
                pause: AtomicBool::new(false),
                entered: AtomicBool::new(false),
                released: AtomicBool::new(false),
            }),
        }
    }

    fn with_outcomes(mut self, outcomes: BTreeMap<String, ProbeRunTermination>) -> Self {
        Arc::get_mut(&mut self.state)
            .expect("runner is not shared while configuring outcomes")
            .outcomes = outcomes;
        self
    }

    fn rejecting(self) -> Self {
        self.state.reject.store(true, Ordering::Release);
        self
    }

    fn cancelling_before_return(self) -> Self {
        self.state
            .cancel_before_return
            .store(true, Ordering::Release);
        self
    }

    fn with_validation_delay(mut self, delay: Duration) -> Self {
        Arc::get_mut(&mut self.state)
            .expect("runner is not shared while configuring validation delay")
            .validation_delay = delay;
        self
    }

    fn pausing(self) -> Self {
        self.state.pause.store(true, Ordering::Release);
        self
    }

    fn entered(&self) -> bool {
        self.state.entered.load(Ordering::Acquire)
    }

    fn release(&self) {
        self.state.released.store(true, Ordering::Release);
    }

    fn calls(&self) -> usize {
        self.state.calls.load(Ordering::Acquire)
    }

    fn max_inflight(&self) -> usize {
        self.state.max_inflight.load(Ordering::Acquire)
    }

    fn events(&self) -> Vec<String> {
        self.state.events.lock().expect("runner event lock").clone()
    }
}

impl ProbeRunner for RecordingRunner {
    fn validate(&self, _probe: &AdmittedProbe) -> Result<(), ProbeSchedulerError> {
        std::thread::sleep(self.state.validation_delay);
        if self.state.reject.load(Ordering::Acquire) {
            return Err(ProbeSchedulerError::host_failure(
                DebugProbeErrorCode::InvalidProbe,
                "fixture runner rejected probe",
            ));
        }
        Ok(())
    }

    fn execute(
        &self,
        probe: AdmittedProbe,
        cancellation: ProbeRunCancellation,
    ) -> ProbeRunnerFuture<'_> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            state.calls.fetch_add(1, Ordering::AcqRel);
            let current = state.inflight.fetch_add(1, Ordering::AcqRel) + 1;
            state.max_inflight.fetch_max(current, Ordering::AcqRel);
            let probe_id = probe.intent().identity.probe_id.0.clone();
            state
                .events
                .lock()
                .expect("runner event lock")
                .push(format!("start:{probe_id}"));
            state.entered.store(true, Ordering::Release);
            while state.pause.load(Ordering::Acquire) && !state.released.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            let slices = (state.delay.as_millis() / 2).max(1);
            for _ in 0..slices {
                tokio::time::sleep(Duration::from_millis(2)).await;
                if cancellation.is_cancelled() {
                    break;
                }
            }
            state
                .events
                .lock()
                .expect("runner event lock")
                .push(format!("end:{probe_id}"));
            state.inflight.fetch_sub(1, Ordering::AcqRel);
            if state.cancel_before_return.load(Ordering::Acquire) {
                cancellation.cancel();
            }
            let configured = state
                .outcomes
                .get(&probe_id)
                .copied()
                .unwrap_or(ProbeRunTermination::Exited);
            let termination = if configured == ProbeRunTermination::CleanupFailed {
                configured
            } else if cancellation.is_cancelled() {
                ProbeRunTermination::Cancelled
            } else {
                configured
            };
            let exit_code = match termination {
                ProbeRunTermination::TimedOut => Some(9),
                ProbeRunTermination::Exited | ProbeRunTermination::CleanupFailed => Some(0),
                _ => None,
            };
            ProbeRunResult::new(termination, exit_code, None, state.delay, 3, false)
        })
    }
}

#[derive(Debug)]
struct FailAfterFirstClock(AtomicUsize);

impl ProbeClock for FailAfterFirstClock {
    fn now(&self) -> Result<Instant, ProbeSchedulerError> {
        if self.0.fetch_add(1, Ordering::AcqRel) == 0 {
            Ok(Instant("2026-09-06T08:00:01.000Z".to_owned()))
        } else {
            Err(ProbeSchedulerError::host_failure(
                DebugProbeErrorCode::InfrastructureError,
                "fixture clock unavailable",
            ))
        }
    }
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn ulid_character(symbol: char) -> char {
    match symbol.to_ascii_uppercase() {
        'I' => 'J',
        'L' => 'M',
        'O' => 'P',
        'U' => 'V',
        character => character,
    }
}

fn probe_identity(symbol: char) -> String {
    format!("prb_{}", ulid_character(symbol).to_string().repeat(26))
}

fn authority(round: char) -> DebugProbeRoundAuthority {
    DebugProbeRoundAuthority {
        attempt: 1,
        debug_session_id: DebugSessionId("dbg_00000000000000000000000000".to_owned()),
        environment_digest: digest('1'),
        fencing_token: FencingToken("7".to_owned()),
        job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
        lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
        repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
        round_id: ProbeRoundId(format!(
            "prn_{}",
            ulid_character(round).to_string().repeat(26)
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

fn probe(identity: char, port: Option<i64>, required: bool) -> ProbeSpec {
    ProbeSpec {
        command: ProbeCommandSpec {
            argv: vec!["fixture-probe".to_owned(), identity.to_string()],
            command_arg_bytes: 1,
            working_directory: ".".to_owned(),
        },
        kind: DebugProbeKind::StaticAnalysis,
        output_limit_bytes: 1_024,
        probe_definition_digest: digest('2'),
        probe_id: ProbeId(probe_identity(identity)),
        required,
        resources: ProbeResourceClaim {
            cpu_limit_millis: 100,
            database_keys: Vec::new(),
            exclusive_keys: Vec::new(),
            memory_limit_bytes: 1_048_576,
            network_access: ProbeNetworkAccess::None,
            paths: vec!["src".to_owned()],
            port_numbers: port.into_iter().collect(),
            service_keys: Vec::new(),
            side_effect_class: ProbeSideEffectClass::PureRead,
            workspace_access: ProbeWorkspaceAccess::ReadOnly,
        },
        target_hypothesis_ids: vec![DebugHypothesisId(
            "hyp_00000000000000000000000000".to_owned(),
        )],
        timeout_millis: 250,
    }
}

fn plan(
    round: char,
    probes: Vec<ProbeSpec>,
    parallel: i64,
    completion_rule: ProbeCompletionRule,
) -> DebugProbePlan {
    let count = i64::try_from(probes.len()).expect("probe count");
    let mut plan = DebugProbePlan {
        authority: authority(round),
        budget: ProbeRoundBudget {
            budget_digest: digest('6'),
            parallel_probe_limit: parallel,
            peak_memory_limit_bytes: 16_777_216,
            probe_limit: count,
            total_command_arg_limit_bytes: 4_096,
            total_cpu_limit_millis: count * 100,
            total_output_limit_bytes: count * 1_024,
            wall_time_limit_millis: 5_000,
        },
        completion_rule,
        created_at: Instant("2026-09-06T00:00:00.000Z".to_owned()),
        plan_digest: digest('3'),
        probes,
        schema_version: 1,
    };
    refresh(&mut plan);
    plan
}

fn refresh(plan: &mut DebugProbePlan) {
    for probe in &mut plan.probes {
        probe.command.command_arg_bytes =
            derive_probe_command_arg_bytes(&probe.command.argv).expect("command bytes");
        probe.probe_definition_digest =
            derive_probe_definition_digest(probe).expect("probe digest");
    }
    plan.budget.budget_digest = derive_probe_budget_digest(&plan.budget).expect("budget digest");
    plan.plan_digest = derive_debug_probe_plan_digest(plan).expect("plan digest");
}

fn all_terminal() -> ProbeCompletionRule {
    ProbeCompletionRule {
        kind: ProbeCompletionRuleKind::AllTerminal,
        minimum_completed_probes: 1,
        minimum_successful_probes: 0,
        stop_on_required_probe_failure: true,
    }
}

fn templates(plan: &DebugProbePlan) -> Vec<TrustedPureReadTemplate> {
    plan.probes
        .iter()
        .map(|probe| {
            TrustedPureReadTemplate::try_new(
                probe.kind.clone(),
                probe.command.clone(),
                probe.resources.clone(),
                probe.timeout_millis,
                probe.output_limit_bytes,
            )
            .expect("trusted fixture template")
        })
        .collect()
}

fn request(plan: &DebugProbePlan, workspace: &TempDir) -> ProbeRoundRequest {
    ProbeRoundRequest::new(plan.authority.clone(), plan.clone(), workspace.path())
}

async fn wait_until(predicate: impl Fn() -> bool, message: &'static str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect(message);
}

#[tokio::test]
async fn parallel_round_commits_every_intent_before_execution_and_replays_one_terminal_receipt() {
    let journal = TempDir::new().expect("journal root");
    let workspace = TempDir::new().expect("workspace");
    let plan = plan(
        '1',
        vec![
            probe('a', None, true),
            probe('b', None, true),
            probe('c', None, true),
        ],
        3,
        all_terminal(),
    );
    let runner = RecordingRunner::new(Duration::from_millis(30)).pausing();
    let observer = runner.clone();
    let scheduler = Arc::new(
        ProbeScheduler::open(journal.path(), runner, templates(&plan)).expect("scheduler"),
    );
    let task_scheduler = Arc::clone(&scheduler);
    let task_plan = plan.clone();
    let task_workspace = workspace.path().to_path_buf();
    let task = tokio::spawn(async move {
        task_scheduler
            .run_round(ProbeRoundRequest::new(
                task_plan.authority.clone(),
                task_plan,
                task_workspace,
            ))
            .await
    });
    wait_until(|| observer.entered(), "parallel runner did not enter").await;
    let restart_runner = RecordingRunner::new(Duration::ZERO);
    let restart_observer = restart_runner.clone();
    let restarted =
        ProbeScheduler::open(journal.path(), restart_runner, templates(&plan)).expect("restart");
    assert_eq!(
        restarted
            .run_round(request(&plan, &workspace))
            .await
            .expect_err("claimed intents replay as unresolved")
            .code(),
        &DebugProbeErrorCode::InfrastructureError
    );
    assert_eq!(restart_observer.calls(), 0);
    observer.release();
    let receipt = task.await.expect("parallel task").expect("parallel round");
    assert_eq!(receipt.status, ProbeRoundReceiptStatus::Completed);
    assert_eq!(receipt.probe_receipts.len(), 3);
    assert_eq!(observer.calls(), 3);
    assert_eq!(observer.max_inflight(), 3);

    drop(scheduler);
    let replay_workspace = workspace.path().to_path_buf();
    std::fs::remove_dir_all(&replay_workspace).expect("remove completed checkout");
    let replay_runner = RecordingRunner::new(Duration::ZERO).rejecting();
    let replay_observer = replay_runner.clone();
    let reopened = ProbeScheduler::open(journal.path(), replay_runner, Vec::new()).expect("reopen");
    let replay = reopened
        .run_round(ProbeRoundRequest::new(
            plan.authority.clone(),
            plan.clone(),
            replay_workspace,
        ))
        .await
        .expect("terminal replay");
    assert_eq!(replay, receipt);
    assert_eq!(replay_observer.calls(), 0);
}

#[tokio::test]
async fn conflicting_claims_form_stable_non_overlapping_waves() {
    let journal = TempDir::new().expect("journal root");
    let workspace = TempDir::new().expect("workspace");
    let plan = plan(
        '2',
        vec![
            probe('b', Some(4_000), true),
            probe('c', None, true),
            probe('a', Some(4_000), true),
        ],
        3,
        all_terminal(),
    );
    let runner = RecordingRunner::new(Duration::from_millis(24));
    let observer = runner.clone();
    let scheduler =
        ProbeScheduler::open(journal.path(), runner, templates(&plan)).expect("scheduler");

    let receipt = scheduler
        .run_round(request(&plan, &workspace))
        .await
        .expect("conflict round");
    let events = observer.events();
    let a = probe_identity('a');
    let b = probe_identity('b');
    let c = probe_identity('c');
    let position = |event: &str| events.iter().position(|item| item == event).expect("event");
    assert!(position(&format!("start:{c}")) < position(&format!("end:{a}")));
    assert!(position(&format!("end:{a}")) < position(&format!("start:{b}")));
    assert_eq!(observer.max_inflight(), 2);
    assert_eq!(
        receipt
            .probe_receipts
            .iter()
            .map(|item| item.identity.probe_id.0.clone())
            .collect::<Vec<_>>(),
        vec![b, c, a],
        "terminal receipt preserves plan ordinal rather than scheduling order"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn rejection_cancellation_and_unfinished_restart_are_fail_closed() {
    let workspace = TempDir::new().expect("workspace");
    let rejection_root = TempDir::new().expect("rejection journal");
    let safe = plan('3', vec![probe('a', None, true)], 1, all_terminal());
    let mut undeclared = safe.clone();
    undeclared.probes[0].resources.database_keys = vec!["db:test".to_owned()];
    refresh(&mut undeclared);
    let reject_runner = RecordingRunner::new(Duration::ZERO);
    let reject_observer = reject_runner.clone();
    let scheduler =
        ProbeScheduler::open(rejection_root.path(), reject_runner, templates(&safe)).expect("open");
    let error = scheduler
        .run_round(request(&undeclared, &workspace))
        .await
        .expect_err("undeclared resource must fail");
    assert_eq!(error.code(), &DebugProbeErrorCode::UndeclaredResource);
    assert_eq!(reject_observer.calls(), 0);
    let mut stale_plan = safe.clone();
    stale_plan.authority.fencing_token = FencingToken("8".to_owned());
    refresh(&mut stale_plan);
    let stale_error = scheduler
        .run_round(ProbeRoundRequest::new(
            safe.authority.clone(),
            stale_plan,
            workspace.path(),
        ))
        .await
        .expect_err("stale plan authority");
    assert_eq!(stale_error.code(), &DebugProbeErrorCode::StaleAuthority);
    let accepted = scheduler
        .run_round(request(&safe, &workspace))
        .await
        .expect("rejected attempts retained no journal progress");
    assert_eq!(accepted.status, ProbeRoundReceiptStatus::Completed);
    assert_eq!(reject_observer.calls(), 1);

    let cancellation_root = TempDir::new().expect("cancellation journal");
    let cancellation_plan = plan('4', vec![probe('a', None, true)], 1, all_terminal());
    let cancel_runner = RecordingRunner::new(Duration::from_millis(200));
    let cancel_observer = cancel_runner.clone();
    let scheduler = Arc::new(
        ProbeScheduler::open(
            cancellation_root.path(),
            cancel_runner,
            templates(&cancellation_plan),
        )
        .expect("open cancellation scheduler"),
    );
    let task_scheduler = Arc::clone(&scheduler);
    let task_plan = cancellation_plan.clone();
    let workspace_path = workspace.path().to_path_buf();
    let task = tokio::spawn(async move {
        task_scheduler
            .run_round(ProbeRoundRequest::new(
                task_plan.authority.clone(),
                task_plan,
                workspace_path,
            ))
            .await
    });
    wait_until(
        || cancel_observer.calls() != 0,
        "cancellation runner did not enter",
    )
    .await;
    let mut stale = cancellation_plan.authority.clone();
    stale.fencing_token = FencingToken("8".to_owned());
    assert_eq!(
        scheduler
            .cancel_round(&stale)
            .expect_err("stale cancel")
            .code(),
        &DebugProbeErrorCode::StaleAuthority
    );
    scheduler
        .cancel_round(&cancellation_plan.authority)
        .expect("exact cancel");
    let cancelled = task
        .await
        .expect("cancellation task")
        .expect("cancel receipt");
    assert_eq!(cancelled.status, ProbeRoundReceiptStatus::Cancelled);
    assert_eq!(
        cancelled.probe_receipts[0].status,
        ProbeReceiptStatus::Cancelled
    );

    let unfinished_root = TempDir::new().expect("unfinished journal");
    let unfinished_plan = plan('5', vec![probe('a', None, true)], 1, all_terminal());
    let no_run = RecordingRunner::new(Duration::ZERO);
    let no_run_observer = no_run.clone();
    let failing = ProbeScheduler::open_with_clock(
        unfinished_root.path(),
        no_run,
        templates(&unfinished_plan),
        FailAfterFirstClock(AtomicUsize::new(0)),
    )
    .expect("open failing clock scheduler");
    assert_eq!(
        failing
            .run_round(request(&unfinished_plan, &workspace))
            .await
            .expect_err("post-claim clock failure")
            .code(),
        &DebugProbeErrorCode::InfrastructureError
    );
    assert_eq!(no_run_observer.calls(), 0);
    drop(failing);
    let restart_runner = RecordingRunner::new(Duration::ZERO);
    let restart_observer = restart_runner.clone();
    let restarted = ProbeScheduler::open(
        unfinished_root.path(),
        restart_runner,
        templates(&unfinished_plan),
    )
    .expect("restart scheduler");
    assert_eq!(
        restarted
            .run_round(request(&unfinished_plan, &workspace))
            .await
            .expect_err("unfinished intent fails closed")
            .code(),
        &DebugProbeErrorCode::InfrastructureError
    );
    assert_eq!(restart_observer.calls(), 0);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn completion_and_receipt_boundaries_are_table_driven() {
    struct Case {
        name: &'static str,
        rule: ProbeCompletionRule,
        order: Vec<(char, bool)>,
        outcomes: Vec<(char, ProbeRunTermination)>,
        expected_calls: usize,
        expected_status: ProbeRoundReceiptStatus,
    }

    let cases = [
        Case {
            name: "all-terminal-does-not-stop-at-minimum",
            rule: all_terminal(),
            order: vec![('a', false), ('b', false)],
            outcomes: Vec::new(),
            expected_calls: 2,
            expected_status: ProbeRoundReceiptStatus::Completed,
        },
        Case {
            name: "optional-success-does-not-skip-later-required",
            rule: ProbeCompletionRule {
                kind: ProbeCompletionRuleKind::MinimumSuccesses,
                minimum_completed_probes: 1,
                minimum_successful_probes: 1,
                stop_on_required_probe_failure: true,
            },
            order: vec![('z', true), ('a', false)],
            outcomes: Vec::new(),
            expected_calls: 2,
            expected_status: ProbeRoundReceiptStatus::Completed,
        },
        Case {
            name: "minimum-successes-unmet",
            rule: ProbeCompletionRule {
                kind: ProbeCompletionRuleKind::MinimumSuccesses,
                minimum_completed_probes: 2,
                minimum_successful_probes: 2,
                stop_on_required_probe_failure: false,
            },
            order: vec![('a', false), ('b', false)],
            outcomes: vec![
                ('a', ProbeRunTermination::InfrastructureError),
                ('b', ProbeRunTermination::InfrastructureError),
            ],
            expected_calls: 2,
            expected_status: ProbeRoundReceiptStatus::Failed,
        },
        Case {
            name: "required-failure-stops-when-configured",
            rule: ProbeCompletionRule {
                kind: ProbeCompletionRuleKind::MinimumSuccesses,
                minimum_completed_probes: 1,
                minimum_successful_probes: 1,
                stop_on_required_probe_failure: true,
            },
            order: vec![('a', true), ('b', false)],
            outcomes: vec![('a', ProbeRunTermination::InfrastructureError)],
            expected_calls: 1,
            expected_status: ProbeRoundReceiptStatus::Failed,
        },
        Case {
            name: "required-failure-continues-when-configured",
            rule: ProbeCompletionRule {
                kind: ProbeCompletionRuleKind::MinimumSuccesses,
                minimum_completed_probes: 1,
                minimum_successful_probes: 1,
                stop_on_required_probe_failure: false,
            },
            order: vec![('a', true), ('b', false)],
            outcomes: vec![('a', ProbeRunTermination::InfrastructureError)],
            expected_calls: 2,
            expected_status: ProbeRoundReceiptStatus::Failed,
        },
        Case {
            name: "timeout-normalizes-exit-code",
            rule: all_terminal(),
            order: vec![('a', false)],
            outcomes: vec![('a', ProbeRunTermination::TimedOut)],
            expected_calls: 1,
            expected_status: ProbeRoundReceiptStatus::Completed,
        },
        Case {
            name: "cleanup-failure-normalizes-zero-exit",
            rule: all_terminal(),
            order: vec![('a', false)],
            outcomes: vec![('a', ProbeRunTermination::CleanupFailed)],
            expected_calls: 1,
            expected_status: ProbeRoundReceiptStatus::Completed,
        },
    ];

    for (index, case) in cases.into_iter().enumerate() {
        let journal = TempDir::new().expect("journal root");
        let workspace = TempDir::new().expect("workspace");
        let plan = plan(
            char::from(b'a' + u8::try_from(index).expect("case index")),
            case.order
                .iter()
                .map(|(identity, required)| probe(*identity, None, *required))
                .collect(),
            1,
            case.rule,
        );
        let outcomes = case
            .outcomes
            .into_iter()
            .map(|(identity, outcome)| (probe_identity(identity), outcome))
            .collect();
        let runner = RecordingRunner::new(Duration::from_millis(2)).with_outcomes(outcomes);
        let observer = runner.clone();
        let scheduler =
            ProbeScheduler::open(journal.path(), runner, templates(&plan)).expect(case.name);
        let receipt = scheduler
            .run_round(request(&plan, &workspace))
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        assert_eq!(receipt.status, case.expected_status, "{}", case.name);
        assert_eq!(observer.calls(), case.expected_calls, "{}", case.name);
        assert_eq!(observer.max_inflight(), 1, "parallel cap: {}", case.name);
        if case.name == "required-failure-stops-when-configured" {
            assert_eq!(
                receipt.completion_reason,
                ProbeRoundCompletionReason::AllProbesTerminal
            );
            assert_eq!(
                receipt.probe_receipts[1].status,
                ProbeReceiptStatus::Skipped
            );
        }
        if case.name == "optional-success-does-not-skip-later-required" {
            assert_eq!(
                receipt.probe_receipts[0].identity.probe_id, plan.probes[0].probe_id,
                "receipt order is plan ordinal"
            );
        }
        if case.name == "timeout-normalizes-exit-code" {
            assert_eq!(
                receipt.probe_receipts[0].status,
                ProbeReceiptStatus::TimedOut
            );
            assert_eq!(receipt.probe_receipts[0].exit_code, None);
        }
        if case.name == "cleanup-failure-normalizes-zero-exit" {
            assert_eq!(receipt.probe_receipts[0].status, ProbeReceiptStatus::Failed);
            assert_eq!(receipt.probe_receipts[0].exit_code, None);
        }
    }

    let wall_root = TempDir::new().expect("wall journal");
    let workspace = TempDir::new().expect("wall workspace");
    let mut wall_plan = plan(
        'h',
        vec![
            probe('a', Some(4_001), false),
            probe('b', Some(4_001), false),
        ],
        2,
        all_terminal(),
    );
    for probe in &mut wall_plan.probes {
        probe.timeout_millis = 3_000;
    }
    refresh(&mut wall_plan);
    let wall_runner = RecordingRunner::new(Duration::ZERO);
    let wall_observer = wall_runner.clone();
    let wall_scheduler =
        ProbeScheduler::open(wall_root.path(), wall_runner, templates(&wall_plan)).expect("open");
    assert_eq!(
        wall_scheduler
            .run_round(request(&wall_plan, &workspace))
            .await
            .expect_err("serial waves exceed wall budget")
            .code(),
        &DebugProbeErrorCode::BudgetExceeded
    );
    assert_eq!(wall_observer.calls(), 0);

    let deadline_root = TempDir::new().expect("deadline journal");
    let mut deadline_plan = plan('i', vec![probe('a', None, false)], 1, all_terminal());
    deadline_plan.probes[0].timeout_millis = 50;
    deadline_plan.budget.wall_time_limit_millis = 50;
    refresh(&mut deadline_plan);
    let deadline_scheduler = ProbeScheduler::open(
        deadline_root.path(),
        RecordingRunner::new(Duration::from_millis(150)),
        templates(&deadline_plan),
    )
    .expect("deadline scheduler");
    let deadline_receipt = deadline_scheduler
        .run_round(request(&deadline_plan, &workspace))
        .await
        .expect("durable deadline receipt");
    assert_eq!(deadline_receipt.status, ProbeRoundReceiptStatus::Failed);
    assert_eq!(
        deadline_receipt.completion_reason,
        ProbeRoundCompletionReason::BudgetExhausted
    );
    assert_eq!(
        deadline_receipt.error.expect("budget error").code,
        DebugProbeErrorCode::BudgetExceeded
    );

    let cleanup_root = TempDir::new().expect("cleanup journal");
    let cleanup_plan = plan('m', vec![probe('a', None, false)], 1, all_terminal());
    let cleanup_runner = RecordingRunner::new(Duration::from_millis(2))
        .with_outcomes(BTreeMap::from([(
            probe_identity('a'),
            ProbeRunTermination::CleanupFailed,
        )]))
        .cancelling_before_return();
    let cleanup_scheduler = ProbeScheduler::open(
        cleanup_root.path(),
        cleanup_runner,
        templates(&cleanup_plan),
    )
    .expect("cleanup scheduler");
    let cleanup_receipt = cleanup_scheduler
        .run_round(request(&cleanup_plan, &workspace))
        .await
        .expect("cleanup failure receipt");
    assert_eq!(cleanup_receipt.status, ProbeRoundReceiptStatus::Cancelled);
    assert_eq!(
        cleanup_receipt.probe_receipts[0].status,
        ProbeReceiptStatus::Failed
    );
    assert_eq!(
        cleanup_receipt.probe_receipts[0]
            .error
            .as_ref()
            .expect("cleanup error")
            .code,
        DebugProbeErrorCode::ProcessCleanupFailed
    );

    let pre_spawn_root = TempDir::new().expect("pre-spawn deadline journal");
    let mut pre_spawn_plan = plan('n', vec![probe('a', None, false)], 1, all_terminal());
    pre_spawn_plan.probes[0].timeout_millis = 20;
    pre_spawn_plan.budget.wall_time_limit_millis = 20;
    refresh(&mut pre_spawn_plan);
    let pre_spawn_runner =
        RecordingRunner::new(Duration::ZERO).with_validation_delay(Duration::from_millis(35));
    let pre_spawn_observer = pre_spawn_runner.clone();
    let pre_spawn_scheduler = ProbeScheduler::open(
        pre_spawn_root.path(),
        pre_spawn_runner,
        templates(&pre_spawn_plan),
    )
    .expect("pre-spawn deadline scheduler");
    let pre_spawn_receipt = pre_spawn_scheduler
        .run_round(request(&pre_spawn_plan, &workspace))
        .await
        .expect("pre-spawn budget terminal");
    assert_eq!(pre_spawn_observer.calls(), 0);
    assert_eq!(pre_spawn_receipt.status, ProbeRoundReceiptStatus::Failed);
    assert_eq!(
        pre_spawn_receipt.completion_reason,
        ProbeRoundCompletionReason::BudgetExhausted
    );

    for name in ["network", "service", "database", "isolated", "exclusive"] {
        let mut resource = probe('q', None, false).resources;
        match name {
            "network" => resource.network_access = ProbeNetworkAccess::Loopback,
            "service" => resource.service_keys.push("svc:test".to_owned()),
            "database" => resource.database_keys.push("db:test".to_owned()),
            "isolated" => {
                resource.side_effect_class = ProbeSideEffectClass::IsolatedSideEffect;
            }
            "exclusive" => resource.side_effect_class = ProbeSideEffectClass::Exclusive,
            _ => unreachable!(),
        }
        let specification = probe('q', None, false);
        let error = TrustedPureReadTemplate::try_new(
            specification.kind,
            specification.command,
            resource,
            specification.timeout_millis,
            specification.output_limit_bytes,
        )
        .expect_err(name);
        assert_eq!(error.code(), &DebugProbeErrorCode::InvalidProbe, "{name}");
    }

    let unsorted_root = TempDir::new().expect("unsorted journal");
    let mut unsorted_plan = plan('j', vec![probe('a', None, false)], 1, all_terminal());
    unsorted_plan.probes[0].resources.port_numbers = vec![4_002, 4_001];
    refresh(&mut unsorted_plan);
    let unsorted_scheduler = ProbeScheduler::open(
        unsorted_root.path(),
        RecordingRunner::new(Duration::ZERO),
        templates(&unsorted_plan),
    )
    .expect("canonical host template");
    assert_eq!(
        unsorted_scheduler
            .run_round(request(&unsorted_plan, &workspace))
            .await
            .expect("equivalent resource sets")
            .status,
        ProbeRoundReceiptStatus::Completed
    );

    for (name, memory_limit, exclusive_key) in [
        ("peak-memory", 1_048_576, None),
        ("exclusive-key", 16_777_216, Some("fixture:key")),
    ] {
        let grouping_root = TempDir::new().expect("grouping journal");
        let mut grouping_plan = plan(
            if name == "peak-memory" { 'k' } else { 'l' },
            vec![probe('a', None, false), probe('b', None, false)],
            2,
            all_terminal(),
        );
        grouping_plan.budget.peak_memory_limit_bytes = memory_limit;
        if let Some(key) = exclusive_key {
            for probe in &mut grouping_plan.probes {
                probe.resources.exclusive_keys = vec![key.to_owned()];
            }
        }
        refresh(&mut grouping_plan);
        let grouping_runner = RecordingRunner::new(Duration::from_millis(8));
        let grouping_observer = grouping_runner.clone();
        let grouping_scheduler = ProbeScheduler::open(
            grouping_root.path(),
            grouping_runner,
            templates(&grouping_plan),
        )
        .expect(name);
        grouping_scheduler
            .run_round(request(&grouping_plan, &workspace))
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(grouping_observer.max_inflight(), 1, "{name}");
    }

    #[cfg(unix)]
    {
        use std::{
            fs,
            os::unix::fs::{PermissionsExt as _, symlink},
        };

        let target = TempDir::new().expect("symlink target");
        let symlink_root = TempDir::new().expect("symlink journal root");
        symlink(target.path(), symlink_root.path().join(".probe-scheduler"))
            .expect("directory symlink");
        assert_eq!(
            ProbeScheduler::open(
                symlink_root.path(),
                RecordingRunner::new(Duration::ZERO),
                templates(&deadline_plan),
            )
            .expect_err("symlinked journal directory")
            .code(),
            &DebugProbeErrorCode::InfrastructureError
        );

        let public_root = TempDir::new().expect("public journal root");
        let public_directory = public_root.path().join(".probe-scheduler");
        fs::create_dir(&public_directory).expect("create public directory");
        fs::set_permissions(&public_directory, fs::Permissions::from_mode(0o755))
            .expect("set public mode");
        assert_eq!(
            ProbeScheduler::open(
                public_root.path(),
                RecordingRunner::new(Duration::ZERO),
                templates(&deadline_plan),
            )
            .expect_err("public journal directory")
            .code(),
            &DebugProbeErrorCode::InfrastructureError
        );

        let public_file_root = TempDir::new().expect("public file root");
        let private_directory = public_file_root.path().join(".probe-scheduler");
        fs::create_dir(&private_directory).expect("private database directory");
        fs::set_permissions(&private_directory, fs::Permissions::from_mode(0o700))
            .expect("set private directory mode");
        let public_database = private_directory.join("probe-scheduler.sqlite3");
        fs::write(&public_database, []).expect("create public database");
        fs::set_permissions(&public_database, fs::Permissions::from_mode(0o644))
            .expect("set public database mode");
        assert_eq!(
            ProbeScheduler::open(
                public_file_root.path(),
                RecordingRunner::new(Duration::ZERO),
                templates(&deadline_plan),
            )
            .expect_err("public journal file")
            .code(),
            &DebugProbeErrorCode::InfrastructureError
        );

        let file_target = public_root.path().join("target.sqlite3");
        fs::write(&file_target, []).expect("file symlink target");
        let file_symlink_root = TempDir::new().expect("file symlink root");
        let file_directory = file_symlink_root.path().join(".probe-scheduler");
        fs::create_dir(&file_directory).expect("private file directory");
        fs::set_permissions(&file_directory, fs::Permissions::from_mode(0o700))
            .expect("set private directory mode");
        symlink(&file_target, file_directory.join("probe-scheduler.sqlite3"))
            .expect("database symlink");
        assert_eq!(
            ProbeScheduler::open(
                file_symlink_root.path(),
                RecordingRunner::new(Duration::ZERO),
                templates(&deadline_plan),
            )
            .expect_err("symlinked journal file")
            .code(),
            &DebugProbeErrorCode::InfrastructureError
        );
    }
}
