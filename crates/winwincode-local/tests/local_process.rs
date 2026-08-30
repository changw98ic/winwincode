use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use winwincode_control_plane::ControlPlaneInstanceRuntimeConfig;
use winwincode_local::{LocalLauncher, LocalLauncherConfig, LocalRuntimeTrace};
use winwincode_worker::composition::domain::{
    CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionEventId, ExecutionJobId,
    ExecutionMessageId, FencingToken, Instant, LeaseId, ProductSessionId, RepositoryId, RequestId,
    SchemaVersion, SessionIdentity, Sha256Digest, StageRunId, WorkerId, WorkerInstanceId,
    WorkerSessionId,
};
use winwincode_worker::composition::generated::{
    ArtifactAckMessage, ArtifactReference, DeliveryStageAcceptanceCriterionInput,
    DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, DeliveryStageInput,
    ExecutionEventCategory, ExecutionEventRecord, ExecutionJob, ExecutionLeaseStamp,
    ExecutionLimits, ExecutionOutcomeUsage, ExecutionPortMessage, ExecutionScope,
    ExecutionWorkspace, ExecutionWorkspaceWriteMode, JobDispatchMessage, JobDispatchMessageKind,
    JobOutcomeAckMessage, JobOutcomeAckMessageKind, JobOutcomeAckMessageStatus, LeaseWriteStatus,
    RuntimeAckMessage, RuntimeAckMessageKind, RuntimeEventMessage, RuntimeEventMessageKind,
    WorkerCapabilityFeature, WorkerCapabilitySet, WorkerCapabilitySetPlatform,
    WorkerRegisterMessage, WorkerRegistrationResultMessage, WorkerRegistrationResultMessageKind,
    WorkerRegistrationResultMessageLeaseRecovery, WorkerRegistrationResultMessageStatus,
};
use winwincode_worker::composition::{
    EndpointSide, ExecutionPortCore, FrameDirection, RemoteTransportAdapter, TypedFrame,
};
use winwincode_worker::{
    CandidateArtifactAckOutcome, CandidateArtifactAuthority, CandidateArtifactUpload,
    CodexCoreAdapter, CodexPoll, CodexThreadStart, CodexTurnCompletion, DurableExecutionDelivery,
    RetainedCandidateArtifact, WorkerConfig, WorkerExecutionPort, WorkerLifecycleState, WorkerMain,
    secret_safe_runtime_summary, workspace_runtime::JobWorkspaceRuntime,
};

const NOW: &str = "2032-01-02T03:04:05.000Z";
const SECRET: &str = "TOKEN-local-launcher-private-fixture";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, suffix: char) -> String {
    format!("{prefix}_{}", suffix.to_string().repeat(26))
}

fn temporary_directory(label: &str) -> PathBuf {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-local-{label}-{}-{sequence}",
        std::process::id()
    ))
}

fn controlled_sources(root: &std::path::Path) -> PathBuf {
    let sources = root.join("sources");
    let repository = sources.join(id("rpo", 'L'));
    if !repository.join(".git").exists() {
        fs::create_dir_all(&repository).expect("create controlled source");
        run_git(&repository, &["init", "-q"]);
        fs::write(repository.join("fixture.txt"), b"source\n").expect("write fixture source");
        run_git(&repository, &["add", "fixture.txt"]);
        run_git(
            &repository,
            &[
                "-c",
                "user.name=WinWinCode Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "source",
            ],
        );
    }
    sources
}

fn run_git(repository: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .status()
        .expect("run Git");
    assert!(status.success(), "Git command failed: {args:?}");
}

fn now() -> Instant {
    Instant(NOW.to_owned())
}

fn worker_config() -> WorkerConfig {
    WorkerConfig {
        worker_id: WorkerId(id("wrk", 'L')),
        worker_instance_id: WorkerInstanceId(id("wki", 'L')),
        started_at: now(),
        capabilities: WorkerCapabilitySet {
            capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            features: vec![
                WorkerCapabilityFeature::ArtifactStream,
                WorkerCapabilityFeature::Sandbox,
                WorkerCapabilityFeature::Shell,
            ],
            max_concurrent_jobs: 1,
            platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        },
    }
}

fn dispatch() -> JobDispatchMessage {
    let goal = format!("execute without retaining {SECRET}");
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: Instant("2032-01-02T03:09:05.000Z".to_owned()),
        fencing_token: FencingToken("17".to_owned()),
        issued_at: now(),
        job_id: ExecutionJobId(id("job", 'L')),
        lease_id: LeaseId(id("lse", 'L')),
        worker_id: WorkerId(id("wrk", 'L')),
        worker_instance_id: WorkerInstanceId(id("wki", 'L')),
    };
    JobDispatchMessage {
        job: ExecutionJob {
            attempt: 1,
            execution_profile: "planner".to_owned(),
            goal: goal.clone(),
            job_id: lease.job_id.clone(),
            limits: ExecutionLimits {
                deadline_at: Instant("2032-01-02T03:08:05.000Z".to_owned()),
                max_artifact_bytes: 1_000_000,
                max_runtime_seconds: 180,
            },
            payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
                delivery_id: DeliveryId(id("dlv", 'L')),
                delivery_task_id: None,
                kind: DeliveryStageExecutionScopeKind::DeliveryStage,
                product_session_id: ProductSessionId(id("psn", 'L')),
                rework_authorization: None,
                stage_run_id: StageRunId(id("run", 'L')),
            }),
            stage_input: Some(DeliveryStageInput {
                acceptance_criteria: vec![DeliveryStageAcceptanceCriterionInput {
                    criterion_id: "criterion-local-fixture".to_owned(),
                    description: "The local fixture behavior is verified.".to_owned(),
                    required: true,
                    verification_method: Some("Run the local fixture test.".to_owned()),
                }],
                candidate_ref: None,
                constraints: Vec::new(),
                delivery_spec_id: "spec-local-fixture".to_owned(),
                delivery_spec_revision: 1,
                goal,
                out_of_scope: Vec::new(),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: vec!["Local fixture source".to_owned()],
                task: None,
                title: "Local Fixture Delivery".to_owned(),
            }),
            workspace: ExecutionWorkspace {
                checkout_revision: "HEAD".to_owned(),
                repository_id: RepositoryId(id("rpo", 'L')),
                write_mode: ExecutionWorkspaceWriteMode::ReadOnly,
            },
        },
        kind: JobDispatchMessageKind::JobDispatch,
        lease,
        message_id: ExecutionMessageId(id("xmsg", 'D')),
        replacement_authority: None,
        request_id: RequestId(id("req", 'D')),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
    }
}

fn thread() -> CodexThreadId {
    winwincode_worker::CodexRunKey::from_dispatch(&dispatch())
        .canonical_thread_id()
        .expect("canonical local thread")
}

#[derive(Default)]
struct FixtureControlPlane {
    accepted: usize,
}

impl ExecutionPortCore for FixtureControlPlane {
    type Output = Vec<ExecutionPortMessage>;
    type Error = ();

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        self.accepted += 1;
        let response = match message {
            ExecutionPortMessage::WorkerRegisterMessage(register) => {
                ExecutionPortMessage::WorkerRegistrationResultMessage(registration(register))
            }
            ExecutionPortMessage::RuntimeEventMessage(event) => {
                ExecutionPortMessage::RuntimeAckMessage(runtime_ack(event))
            }
            ExecutionPortMessage::JobOutcomeMessage(outcome) => {
                ExecutionPortMessage::JobOutcomeAckMessage(JobOutcomeAckMessage {
                    error: None,
                    kind: JobOutcomeAckMessageKind::JobOutcomeAck,
                    lease: outcome.lease.clone(),
                    message_id: ExecutionMessageId(id("xmsg", 'Y')),
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: now(),
                    session_identity: outcome.session_identity.clone(),
                    status: JobOutcomeAckMessageStatus::Accepted,
                    worker_session_id: outcome.worker_session_id.clone(),
                })
            }
            _ => return Ok(Vec::new()),
        };
        Ok(vec![response])
    }
}

fn runtime_ack(event: &RuntimeEventMessage) -> RuntimeAckMessage {
    RuntimeAckMessage {
        ack_sequence: ExecutionAckSequence(event.event.sequence.0),
        error: None,
        kind: RuntimeAckMessageKind::RuntimeAck,
        lease: event.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", 'X')),
        replay_from_sequence: None,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        session_identity: event.session_identity.clone(),
        status: LeaseWriteStatus::Accepted,
        worker_session_id: event.worker_session_id.clone(),
    }
}

fn registration(register: &WorkerRegisterMessage) -> WorkerRegistrationResultMessage {
    WorkerRegistrationResultMessage {
        error: None,
        heartbeat_interval_ms: 2_000,
        kind: WorkerRegistrationResultMessageKind::WorkerRegistrationResult,
        lease_recovery: WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
        message_id: ExecutionMessageId(id("xmsg", 'R')),
        request_id: register.request_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        server_time: now(),
        status: WorkerRegistrationResultMessageStatus::Accepted,
        worker_id: register.worker_id.clone(),
        worker_instance_id: register.worker_instance_id.clone(),
    }
}

#[derive(Clone, Default)]
struct FixtureCodex {
    state: Arc<Mutex<FixtureCodexState>>,
}

#[derive(Default)]
struct FixtureCodexState {
    polls: VecDeque<FixturePoll>,
    calls: Vec<String>,
    runtime_identity: Option<RuntimeIdentity>,
    durable_deliveries: Vec<DurableExecutionDelivery>,
    pending_delivery_ids: HashSet<String>,
    send_counts: HashMap<&'static str, usize>,
}

enum FixturePoll {
    Runtime,
    Exact(CodexPoll),
}

#[derive(Clone)]
struct RuntimeIdentity {
    lease: ExecutionLeaseStamp,
    session_identity: SessionIdentity,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
}

impl FixtureCodex {
    fn completed() -> Self {
        let codex = Self::default();
        codex
            .state
            .lock()
            .expect("FixtureCodex state")
            .polls
            .push_back(FixturePoll::Exact(CodexPoll::Completed(
                CodexTurnCompletion {
                    summary: secret_safe_runtime_summary("local fixture completed").unwrap(),
                    artifacts: Vec::new(),
                    usage: ExecutionOutcomeUsage {
                        runtime_millis: 17,
                        tokens: 23,
                        cost_microunits: 29,
                    },
                },
            )));
        codex
    }

    fn runtime_then_completed() -> Self {
        let codex = Self::completed();
        codex
            .state
            .lock()
            .expect("FixtureCodex state")
            .polls
            .push_front(FixturePoll::Runtime);
        codex
    }

    fn calls(&self) -> Vec<String> {
        self.state.lock().expect("FixtureCodex state").calls.clone()
    }

    fn send_count(&self, kind: &'static str) -> usize {
        self.state
            .lock()
            .expect("FixtureCodex state")
            .send_counts
            .get(kind)
            .copied()
            .unwrap_or_default()
    }

    fn pending_acknowledged_products(&self) -> Vec<String> {
        let state = self.state.lock().expect("FixtureCodex state");
        state
            .durable_deliveries
            .iter()
            .filter(|delivery| state.pending_delivery_ids.contains(&delivery.delivery_id))
            .filter_map(|delivery| acknowledged_product_kind(&delivery.message))
            .map(str::to_owned)
            .collect()
    }
}

fn acknowledged_product_kind(message: &ExecutionPortMessage) -> Option<&'static str> {
    match message {
        ExecutionPortMessage::WorkerRegisterMessage(_) => Some("worker.register"),
        ExecutionPortMessage::RuntimeEventMessage(_) => Some("runtime.event"),
        ExecutionPortMessage::JobOutcomeMessage(_) => Some("job.outcome"),
        ExecutionPortMessage::ArtifactOpenMessage(_) => Some("artifact.open"),
        ExecutionPortMessage::ArtifactChunkMessage(_) => Some("artifact.chunk"),
        _ => None,
    }
}

fn fixture_delivery(message: &ExecutionPortMessage) -> DurableExecutionDelivery {
    let value = serde_json::to_value(message).expect("serialize fixture delivery");
    DurableExecutionDelivery {
        delivery_id: value["messageId"]
            .as_str()
            .expect("fixture message id")
            .to_owned(),
        message: message.clone(),
    }
}

fn delivery_matches_ack(
    retained: &ExecutionPortMessage,
    acknowledgement: &ExecutionPortMessage,
) -> bool {
    match (retained, acknowledgement) {
        (
            ExecutionPortMessage::WorkerRegisterMessage(register),
            ExecutionPortMessage::WorkerRegistrationResultMessage(result),
        ) => {
            result.request_id == register.request_id
                && result.worker_id == register.worker_id
                && result.worker_instance_id == register.worker_instance_id
                && matches!(
                    result.status,
                    WorkerRegistrationResultMessageStatus::Accepted
                        | WorkerRegistrationResultMessageStatus::Duplicate
                )
        }
        (
            ExecutionPortMessage::RuntimeEventMessage(event),
            ExecutionPortMessage::RuntimeAckMessage(acknowledgement),
        ) => {
            acknowledgement.lease == event.lease
                && acknowledgement.session_identity == event.session_identity
                && acknowledgement.worker_session_id == event.worker_session_id
                && acknowledgement.ack_sequence.0 >= event.event.sequence.0
                && acknowledgement.status == LeaseWriteStatus::Accepted
        }
        (
            ExecutionPortMessage::JobOutcomeMessage(outcome),
            ExecutionPortMessage::JobOutcomeAckMessage(acknowledgement),
        ) => {
            acknowledgement.lease == outcome.lease
                && acknowledgement.session_identity == outcome.session_identity
                && acknowledgement.worker_session_id == outcome.worker_session_id
                && matches!(
                    acknowledgement.status,
                    JobOutcomeAckMessageStatus::Accepted | JobOutcomeAckMessageStatus::Duplicate
                )
        }
        _ => false,
    }
}

impl CodexCoreAdapter for FixtureCodex {
    type Error = ();

    fn ensure_thread(
        &mut self,
        start: CodexThreadStart<'_>,
    ) -> impl Future<Output = Result<CodexThreadId, Self::Error>> {
        let thread_id = start
            .run_key
            .canonical_thread_id()
            .expect("canonical local thread");
        let (product_session_id, stage_run_id) = match &start.job.scope {
            ExecutionScope::DeliveryStageExecutionScope(scope) => (
                scope.product_session_id.clone(),
                Some(scope.stage_run_id.clone()),
            ),
            ExecutionScope::ProductSessionExecutionScope(scope) => {
                (scope.product_session_id.clone(), None)
            }
        };
        let mut state = self.state.lock().expect("FixtureCodex state");
        state
            .calls
            .push(format!("ensure:{}", start.run_key.job_id.0));
        state.runtime_identity = Some(RuntimeIdentity {
            lease: start.lease.clone(),
            session_identity: SessionIdentity {
                codex_thread_id: thread_id.clone(),
                product_session_id,
                stage_run_id,
                worker_session_id: start.worker_session_id.clone(),
            },
            worker_session_id: start.worker_session_id.clone(),
            codex_thread_id: thread_id.clone(),
        });
        std::future::ready(Ok(thread_id))
    }

    fn submit_turn(
        &mut self,
        thread_id: &CodexThreadId,
        _goal: &str,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.state
            .lock()
            .expect("FixtureCodex state")
            .calls
            .push(format!("submit:{}", thread_id.0));
        std::future::ready(Ok(()))
    }

    fn poll(
        &mut self,
        thread_id: &CodexThreadId,
        _now: &Instant,
    ) -> impl Future<Output = Result<CodexPoll, Self::Error>> {
        let mut state = self.state.lock().expect("FixtureCodex state");
        state.calls.push(format!("poll:{}", thread_id.0));
        let poll = match state.polls.pop_front() {
            Some(FixturePoll::Exact(poll)) => poll,
            Some(FixturePoll::Runtime) => {
                let identity = state
                    .runtime_identity
                    .clone()
                    .expect("runtime identity captured by ensure_thread");
                CodexPoll::RuntimeTrace(Box::new(RuntimeEventMessage {
                    codex_thread_id: identity.codex_thread_id,
                    event: ExecutionEventRecord {
                        category: ExecutionEventCategory::Activity,
                        event_id: ExecutionEventId(id("evt", 'L')),
                        occurred_at: now(),
                        payload: None,
                        sequence: winwincode_worker::composition::domain::ExecutionSequence(1),
                        summary: "local durable runtime fixture".to_owned(),
                    },
                    kind: RuntimeEventMessageKind::RuntimeEvent,
                    lease: identity.lease,
                    message_id: ExecutionMessageId(id("xmsg", 'T')),
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: now(),
                    session_identity: identity.session_identity,
                    worker_session_id: identity.worker_session_id,
                }))
            }
            None => CodexPoll::Pending,
        };
        std::future::ready(Ok(poll))
    }

    fn accept_model_chunk(
        &mut self,
        _chunk: &winwincode_worker::composition::generated::ModelChunkMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        std::future::ready(Err(()))
    }

    fn accept_action_receipt(
        &mut self,
        _receipt: &winwincode_worker::composition::generated::ActionEnforcementReceiptMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        std::future::ready(Err(()))
    }

    fn accept_approval_decision(
        &mut self,
        _decision: &winwincode_worker::composition::generated::ApprovalDecisionMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        std::future::ready(Err(()))
    }

    fn accept_input_response(
        &mut self,
        _response: &winwincode_worker::composition::generated::InputResponseMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        std::future::ready(Err(()))
    }

    fn retain_execution_delivery(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        let delivery = fixture_delivery(message);
        let mut state = self.state.lock().expect("FixtureCodex state");
        if let Some(existing) = state
            .durable_deliveries
            .iter()
            .find(|existing| existing.delivery_id == delivery.delivery_id)
            .cloned()
        {
            if existing.message != delivery.message {
                return Err(());
            }
            state
                .pending_delivery_ids
                .insert(existing.delivery_id.clone());
            return Ok(existing);
        }
        state
            .pending_delivery_ids
            .insert(delivery.delivery_id.clone());
        state.durable_deliveries.push(delivery.clone());
        Ok(delivery)
    }

    fn pending_execution_deliveries(
        &mut self,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        let state = self.state.lock().expect("FixtureCodex state");
        Ok(state
            .durable_deliveries
            .iter()
            .filter(|delivery| state.pending_delivery_ids.contains(&delivery.delivery_id))
            .cloned()
            .collect())
    }

    fn record_execution_delivery_sent(&mut self, delivery_id: &str) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("FixtureCodex state");
        let delivery = state
            .durable_deliveries
            .iter()
            .find(|delivery| delivery.delivery_id == delivery_id)
            .cloned()
            .ok_or(())?;
        if !state.pending_delivery_ids.contains(delivery_id) {
            return Err(());
        }
        if let Some(kind) = acknowledged_product_kind(&delivery.message) {
            *state.send_counts.entry(kind).or_default() += 1;
        } else {
            state.pending_delivery_ids.remove(delivery_id);
        }
        Ok(())
    }

    fn accept_execution_delivery_ack(
        &mut self,
        acknowledgement: &ExecutionPortMessage,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("FixtureCodex state");
        let acknowledged = state
            .durable_deliveries
            .iter()
            .filter(|delivery| state.pending_delivery_ids.contains(&delivery.delivery_id))
            .filter(|delivery| delivery_matches_ack(&delivery.message, acknowledgement))
            .map(|delivery| delivery.delivery_id.clone())
            .collect::<Vec<_>>();
        if acknowledged.is_empty() {
            return Err(());
        }
        for delivery_id in acknowledged {
            state.pending_delivery_ids.remove(&delivery_id);
        }
        Ok(())
    }

    fn retain_candidate_artifact(
        &mut self,
        _upload: &CandidateArtifactUpload,
    ) -> Result<RetainedCandidateArtifact, Self::Error> {
        Err(())
    }

    fn accept_candidate_artifact_ack(
        &mut self,
        _acknowledgement: &ArtifactAckMessage,
    ) -> Result<CandidateArtifactAckOutcome, Self::Error> {
        Err(())
    }

    fn accepted_candidate_artifact(
        &mut self,
        _authority: &CandidateArtifactAuthority,
    ) -> Result<Option<ArtifactReference>, Self::Error> {
        Err(())
    }

    fn cancel_candidate_artifact(
        &mut self,
        _authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error> {
        Err(())
    }

    fn replay_execution_deliveries(
        &mut self,
        _request: &winwincode_worker::composition::generated::RuntimeReplayRequestMessage,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        Ok(Vec::new())
    }

    fn retain_job_outcome(
        &mut self,
        _thread_id: &CodexThreadId,
        outcome: &winwincode_worker::composition::generated::JobOutcomeMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        self.retain_execution_delivery(&ExecutionPortMessage::JobOutcomeMessage(outcome.clone()))
    }

    fn take_execution_messages(&mut self) -> Result<Vec<ExecutionPortMessage>, Self::Error> {
        Ok(Vec::new())
    }

    fn interrupt(
        &mut self,
        thread_id: &CodexThreadId,
        _interrupted_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.state
            .lock()
            .expect("FixtureCodex state")
            .calls
            .push(format!("interrupt:{}", thread_id.0));
        std::future::ready(Ok(()))
    }

    fn close_thread(
        &mut self,
        thread_id: &CodexThreadId,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.state
            .lock()
            .expect("FixtureCodex state")
            .calls
            .push(format!("close:{}", thread_id.0));
        std::future::ready(Ok(()))
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Self::Error>> {
        self.state
            .lock()
            .expect("FixtureCodex state")
            .calls
            .push("shutdown".to_owned());
        std::future::ready(Ok(()))
    }
}

async fn run_local(label: &str) -> (Vec<u8>, Vec<String>) {
    let root = temporary_directory(label);
    let codex = FixtureCodex::completed();
    let calls = codex.clone();
    let config = LocalLauncherConfig::try_new(
        root.clone(),
        controlled_sources(&root),
        1_000,
        ControlPlaneInstanceRuntimeConfig::try_new(10_000, 10_000).unwrap(),
        32,
    )
    .unwrap();
    let mut launcher = Box::pin(LocalLauncher::start(
        config,
        worker_config(),
        FixtureControlPlane::default(),
        codex,
        now(),
    ))
    .await
    .unwrap();
    assert_eq!(launcher.worker_lifecycle(), WorkerLifecycleState::Active);
    launcher
        .execution_port()
        .enqueue_control(ExecutionPortMessage::JobDispatchMessage(dispatch()))
        .unwrap();
    assert_eq!(Box::pin(launcher.drive(now())).await.unwrap(), 1);
    launcher.heartbeat(now()).await.unwrap();
    Box::pin(launcher.poll_codex(now())).await.unwrap();
    assert_eq!(launcher.active_job_count(), 0);
    assert_eq!(
        launcher
            .control_plane()
            .with(|control_plane| control_plane.accepted)
            .unwrap(),
        5
    );
    let report = Box::pin(launcher.shutdown(now(), 1_001)).await.unwrap();
    assert!(report.worker.cancelled_jobs.is_empty());
    assert_eq!(report.worker.codex_failures, 0);
    assert!(launcher.is_stopped());
    assert_eq!(launcher.pending_frame_count(), 0);
    let trace = launcher.trace_json().unwrap();
    assert!(
        !trace
            .windows(SECRET.len())
            .any(|window| window == SECRET.as_bytes())
    );
    assert!(trace.len() < 16 * 1024);
    assert!(fs::remove_dir_all(&root).is_ok());
    (trace, calls.calls())
}

#[tokio::test(flavor = "current_thread")]
async fn same_process_launcher_starts_runs_and_cleans_up_with_reproducible_secret_safe_trace() {
    let first = Box::pin(run_local("first")).await;
    let second = Box::pin(run_local("second")).await;
    assert_eq!(first.0, second.0);
    assert_eq!(first.1, second.1);
    let value: serde_json::Value = serde_json::from_slice(&first.0).unwrap();
    let frames = value["frames"].as_array().unwrap();
    assert!(frames.len() <= 32);
    let outcome = frames
        .iter()
        .find(|frame| frame["kind"] == "job.outcome")
        .unwrap();
    assert_eq!(outcome["jobId"], id("job", 'L'));
    assert_eq!(outcome["leaseId"], id("lse", 'L'));
    assert_eq!(outcome["productSessionId"], id("psn", 'L'));
    assert_eq!(outcome["stageRunId"], id("run", 'L'));
    assert_eq!(outcome["codexThreadId"], thread().0);
    assert_eq!(
        first.1,
        [
            format!("ensure:{}", id("job", 'L')),
            format!("submit:{}", thread().0),
            format!("poll:{}", thread().0),
            format!("close:{}", thread().0),
            "shutdown".to_owned(),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_and_outcome_acknowledgements_compact_before_local_restart() {
    let root = temporary_directory("durable-ack-restart");
    let codex = FixtureCodex::runtime_then_completed();
    let durable = codex.clone();
    let config = LocalLauncherConfig::try_new(
        root.clone(),
        controlled_sources(&root),
        2_000,
        ControlPlaneInstanceRuntimeConfig::try_new(10_000, 10_000).unwrap(),
        64,
    )
    .unwrap();
    let mut first = Box::pin(LocalLauncher::start(
        config,
        worker_config(),
        FixtureControlPlane::default(),
        codex,
        now(),
    ))
    .await
    .unwrap();
    Box::pin(first.accept_control(ExecutionPortMessage::JobDispatchMessage(dispatch()), now()))
        .await
        .unwrap();
    Box::pin(first.poll_codex(now())).await.unwrap();
    Box::pin(first.poll_codex(now())).await.unwrap();

    assert_eq!(durable.send_count("runtime.event"), 1);
    assert_eq!(durable.send_count("job.outcome"), 1);
    assert!(durable.pending_acknowledged_products().is_empty());
    let trace: serde_json::Value = serde_json::from_slice(&first.trace_json().unwrap()).unwrap();
    let kinds = trace["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|frame| frame["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"runtime.event"));
    assert!(kinds.contains(&"runtime.ack"));
    assert!(kinds.contains(&"job.outcome"));
    assert!(kinds.contains(&"job.outcome_ack"));
    Box::pin(first.shutdown(now(), 2_001)).await.unwrap();
    drop(first);

    let config = LocalLauncherConfig::try_new(
        root.clone(),
        controlled_sources(&root),
        2_002,
        ControlPlaneInstanceRuntimeConfig::try_new(10_000, 10_000).unwrap(),
        32,
    )
    .unwrap();
    let mut restarted = Box::pin(LocalLauncher::start(
        config,
        worker_config(),
        FixtureControlPlane::default(),
        durable.clone(),
        now(),
    ))
    .await
    .unwrap();
    assert_eq!(durable.send_count("runtime.event"), 1);
    assert_eq!(durable.send_count("job.outcome"), 1);
    assert!(durable.pending_acknowledged_products().is_empty());
    Box::pin(restarted.shutdown(now(), 2_003)).await.unwrap();
    assert!(fs::remove_dir_all(&root).is_ok());
}

#[derive(Clone)]
struct RemotePort {
    state: Rc<RefCell<RemoteState>>,
}

struct RemoteState {
    control_plane: FixtureControlPlane,
    inbox: VecDeque<ExecutionPortMessage>,
    trace: LocalRuntimeTrace,
}

impl RemotePort {
    fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(RemoteState {
                control_plane: FixtureControlPlane::default(),
                inbox: VecDeque::new(),
                trace: LocalRuntimeTrace::try_new(32).unwrap(),
            })),
        }
    }

    fn route_control(&self, message: ExecutionPortMessage) {
        let frame = TypedFrame::new(FrameDirection::ControlPlaneToWorker, message).unwrap();
        let bytes = RemoteTransportAdapter::<FixtureControlPlane>::encode(&frame).unwrap();
        let decoded = RemoteTransportAdapter::<FixtureControlPlane>::decode(&bytes).unwrap();
        let mut state = self.state.borrow_mut();
        state.trace.record(&decoded).unwrap();
        state.inbox.push_back(decoded.message().clone());
    }

    fn pop(&self) -> Option<ExecutionPortMessage> {
        self.state.borrow_mut().inbox.pop_front()
    }

    fn trace_json(&self) -> Vec<u8> {
        self.state.borrow().trace.to_json().unwrap()
    }
}

impl WorkerExecutionPort for RemotePort {
    type Error = ();

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let result = (|| {
            let frame =
                TypedFrame::new(FrameDirection::WorkerToControlPlane, message).map_err(|_| ())?;
            let bytes =
                RemoteTransportAdapter::<FixtureControlPlane>::encode(&frame).map_err(|_| ())?;
            let mut state = self.state.borrow_mut();
            let decoded =
                RemoteTransportAdapter::<FixtureControlPlane>::decode(&bytes).map_err(|_| ())?;
            state.trace.record(&decoded).map_err(|_| ())?;
            let responses =
                RemoteTransportAdapter::new(&mut state.control_plane, EndpointSide::ControlPlane)
                    .accept(&bytes)
                    .map_err(|_| ())?;
            for response in responses {
                let frame = TypedFrame::new(FrameDirection::ControlPlaneToWorker, response)
                    .map_err(|_| ())?;
                let bytes = RemoteTransportAdapter::<FixtureControlPlane>::encode(&frame)
                    .map_err(|_| ())?;
                let decoded = RemoteTransportAdapter::<FixtureControlPlane>::decode(&bytes)
                    .map_err(|_| ())?;
                state.trace.record(&decoded).map_err(|_| ())?;
                state.inbox.push_back(decoded.message().clone());
            }
            Ok(())
        })();
        std::future::ready(result)
    }
}

async fn drain_remote(worker: &mut WorkerMain<RemotePort, FixtureCodex>, port: &RemotePort) {
    while let Some(message) = port.pop() {
        Box::pin(worker.accept_control(&message, now()))
            .await
            .unwrap();
    }
}

async fn run_remote() -> Vec<u8> {
    let root = temporary_directory("remote");
    let sources = controlled_sources(&root);
    let workspaces = JobWorkspaceRuntime::open(root.join("worker-workspaces"), sources)
        .expect("open remote fixture workspaces");
    let port = RemotePort::new();
    let handle = port.clone();
    let mut worker = WorkerMain::new(worker_config(), port, FixtureCodex::completed(), workspaces);
    worker.start(now()).await.unwrap();
    Box::pin(drain_remote(&mut worker, &handle)).await;
    handle.route_control(ExecutionPortMessage::JobDispatchMessage(dispatch()));
    Box::pin(drain_remote(&mut worker, &handle)).await;
    worker.heartbeat(now()).await.unwrap();
    Box::pin(worker.poll_codex(now())).await.unwrap();
    Box::pin(drain_remote(&mut worker, &handle)).await;
    Box::pin(worker.shutdown(now())).await.unwrap();
    let trace = handle.trace_json();
    drop(worker);
    fs::remove_dir_all(root).expect("remove remote fixture");
    trace
}

#[tokio::test(flavor = "current_thread")]
async fn same_process_and_separated_json_transport_have_identical_typed_trace() {
    let local = Box::pin(run_local("parity")).await.0;
    let remote = Box::pin(run_remote()).await;
    assert_eq!(local, remote);
}

#[test]
fn trace_capacity_is_bounded_before_any_process_starts() {
    let root = temporary_directory("capacity");
    let source_root = temporary_directory("capacity-source");
    fs::create_dir_all(&source_root).expect("create source root");
    let error = LocalLauncherConfig::try_new(
        &root,
        &source_root,
        1_000,
        ControlPlaneInstanceRuntimeConfig::default(),
        0,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "local launcher configuration is invalid");
    assert!(!root.exists());
    fs::remove_dir_all(source_root).expect("remove source root");
}

#[test]
fn trace_fails_closed_on_capacity_or_secret_shaped_identity() {
    let frame = TypedFrame::new(
        FrameDirection::ControlPlaneToWorker,
        ExecutionPortMessage::JobDispatchMessage(dispatch()),
    )
    .unwrap();
    let mut bounded = LocalRuntimeTrace::try_new(1).unwrap();
    bounded.record(&frame).unwrap();
    let capacity = bounded.record(&frame).unwrap_err();
    assert_eq!(capacity.to_string(), "local runtime trace rejected a frame");

    let mut unsafe_dispatch = dispatch();
    unsafe_dispatch.lease.worker_id = WorkerId(SECRET.to_owned());
    let unsafe_frame = TypedFrame::new(
        FrameDirection::ControlPlaneToWorker,
        ExecutionPortMessage::JobDispatchMessage(unsafe_dispatch),
    )
    .unwrap();
    let mut trace = LocalRuntimeTrace::try_new(2).unwrap();
    let unsafe_error = trace.record(&unsafe_frame).unwrap_err();
    assert_eq!(
        unsafe_error.to_string(),
        "local runtime trace rejected a frame"
    );
    assert!(!unsafe_error.to_string().contains(SECRET));
    assert!(trace.frames().is_empty());
}

#[test]
fn launcher_has_no_cli_or_javascript_callback_fallback() {
    let manifest = include_str!("../Cargo.toml");
    let source = include_str!("../src/lib.rs");
    assert!(!manifest.contains("winwincode-cli"));
    assert!(!manifest.contains("winwincode-native"));
    assert!(!source.contains("Command::new"));
    assert!(!source.contains("std::process"));
    assert!(!source.contains("napi"));
}
