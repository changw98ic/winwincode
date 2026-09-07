// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::large_futures)]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream;
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId,
    ProductSessionId, RepositoryId, RequestId, SchemaVersion, Sha256Digest, WorkerId,
    WorkerInstanceId,
};
use winwincode_execution_port::generated::{
    ArtifactAckMessage, ArtifactReference, ExecutionJob, ExecutionLeaseStamp, ExecutionLimits,
    ExecutionOutcomeStatus, ExecutionOutcomeUsage, ExecutionPortMessage, ExecutionScope,
    ExecutionWorkspace, ExecutionWorkspaceWriteMode, JobCancelAckMessageStatus, JobCancelMessage,
    JobCancelMessageKind, JobCancelMessageReason, JobDispatchMessage, JobDispatchMessageKind,
    JobDispatchResultMessageStatus, ProductSessionExecutionScope, ProductSessionExecutionScopeKind,
    WorkerCapabilityFeature, WorkerCapabilitySet, WorkerCapabilitySetPlatform,
    WorkerRegistrationResultMessage, WorkerRegistrationResultMessageKind,
    WorkerRegistrationResultMessageLeaseRecovery, WorkerRegistrationResultMessageStatus,
};
use winwincode_kernel::{
    EventPoll, Kernel, KernelOptions, ModelPort, ModelPortFailure, ModelPortRequest,
    ModelPortStream, SessionOptions, TurnSubmissionOptions,
};
use winwincode_worker::{
    ActiveJob, CandidateArtifactAckOutcome, CandidateArtifactAuthority, CandidateArtifactUpload,
    CodexCoreAdapter, CodexPoll, CodexThreadStart, CodexTurnCompletion, DurableExecutionDelivery,
    RetainedCandidateArtifact, WorkerConfig, WorkerErrorCode, WorkerExecutionPort,
    WorkerLifecycleState, WorkerMain, secret_safe_runtime_summary,
    workspace_runtime::JobWorkspaceRuntime,
};

const NOW: &str = "2027-01-15T08:00:02.000Z";

#[derive(Debug)]
struct FixtureRoot(PathBuf);

impl FixtureRoot {
    fn create() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-worker-real-codex-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).expect("create fixture home");
        let sources = root.join("sources");
        for suffix in ['A', 'B'] {
            let repository = sources.join(id("rpo", suffix));
            std::fs::create_dir_all(&repository).expect("create fixture source repository");
            run_git(&repository, &["init", "-q"]);
            std::fs::write(repository.join("fixture.txt"), b"source\n")
                .expect("write fixture source");
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
        std::fs::write(
            root.join("home/config.toml"),
            "approval_policy = \"never\"\ndefault_permissions = \":workspace\"\n",
        )
        .expect("write deterministic Codex fixture config");
        Self(root)
    }

    fn home(&self) -> PathBuf {
        self.0.join("home")
    }

    fn sources(&self) -> PathBuf {
        self.0.join("sources")
    }

    fn workspaces(&self) -> PathBuf {
        self.0.join("worker-workspaces")
    }
}

fn run_git(repository: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .status()
        .expect("run Git fixture");
    assert!(status.success(), "Git fixture command failed: {args:?}");
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Default)]
struct DeterministicModelPort {
    requests: AtomicUsize,
}

impl DeterministicModelPort {
    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl ModelPort for DeterministicModelPort {
    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>> {
        let ordinal = self.requests.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if ordinal > 0 {
                return Ok(Box::pin(stream::pending()) as ModelPortStream);
            }
            let response_id = request.request_id;
            let messages = vec![
                r#"{"type":"created"}"#.to_owned(),
                r#"{"type":"server_model","model":"fixture-coder"}"#.to_owned(),
                r#"{"type":"output_item_added","item":{"type":"message","id":"fixture-message","role":"assistant","content":[]}}"#.to_owned(),
                r#"{"type":"output_text_delta","delta":"fixture complete"}"#.to_owned(),
                r#"{"type":"output_item_done","item":{"type":"message","id":"fixture-message","role":"assistant","content":[{"type":"output_text","text":"fixture complete"}],"phase":"final_answer"}}"#.to_owned(),
                format!(
                    r#"{{"type":"completed","responseId":"{response_id}","endTurn":true}}"#
                ),
            ];
            Ok(Box::pin(stream::iter(messages.into_iter().map(Ok))) as ModelPortStream)
        })
    }
}

#[derive(Debug, Default)]
struct AdapterStats {
    ensure_calls: usize,
    created_sessions: usize,
    submit_calls: usize,
    interrupt_calls: usize,
    close_calls: usize,
    shutdown_calls: usize,
}

struct RealKernelAdapter {
    kernel: Arc<Kernel>,
    sessions: HashMap<String, String>,
    runs: HashMap<winwincode_worker::CodexRunKey, CodexThreadId>,
    interrupted: HashSet<String>,
    stats: AdapterStats,
}

impl RealKernelAdapter {
    fn new(kernel: Arc<Kernel>) -> Self {
        Self {
            kernel,
            sessions: HashMap::new(),
            runs: HashMap::new(),
            interrupted: HashSet::new(),
            stats: AdapterStats::default(),
        }
    }

    fn session_id(&self, thread_id: &CodexThreadId) -> Result<&str, String> {
        self.sessions
            .get(&thread_id.0)
            .map(String::as_str)
            .ok_or_else(|| "Codex thread is not registered by the fixture adapter".to_owned())
    }

    fn canonical_thread_id(start: CodexThreadStart<'_>) -> CodexThreadId {
        start
            .run_key
            .canonical_thread_id()
            .expect("canonical fixture thread")
    }
}

fn measured_fixture_usage() -> ExecutionOutcomeUsage {
    ExecutionOutcomeUsage {
        runtime_millis: 50,
        tokens: 2,
        cost_microunits: 3,
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

impl CodexCoreAdapter for RealKernelAdapter {
    type Error = String;

    async fn ensure_thread(
        &mut self,
        start: CodexThreadStart<'_>,
    ) -> Result<CodexThreadId, Self::Error> {
        self.stats.ensure_calls += 1;
        if let Some(thread_id) = self.runs.get(start.run_key) {
            return Ok(thread_id.clone());
        }
        let session = self
            .kernel
            .create_session(SessionOptions {
                cwd: start.workspace.to_path_buf(),
                provider: "fixture-provider".to_owned(),
                model: "fixture-coder".to_owned(),
                role_policy: None,
            })
            .await
            .map_err(|error| error.to_string())?;
        let thread_id = Self::canonical_thread_id(start);
        self.sessions
            .insert(thread_id.0.clone(), session.session_id.clone());
        self.runs.insert(start.run_key.clone(), thread_id.clone());
        self.stats.created_sessions += 1;
        Ok(thread_id)
    }

    async fn submit_turn(
        &mut self,
        thread_id: &CodexThreadId,
        goal: &str,
    ) -> Result<(), Self::Error> {
        self.stats.submit_calls += 1;
        let session_id = self.session_id(thread_id)?.to_owned();
        let submission = self
            .kernel
            .submit_turn(
                &session_id,
                goal.to_owned(),
                TurnSubmissionOptions::default(),
            )
            .await
            .map_err(|error| error.to_string())?;
        if submission.status != "started" {
            return Err(format!(
                "fixture turn was not started: {}",
                submission.status
            ));
        }
        Ok(())
    }

    async fn poll(
        &mut self,
        thread_id: &CodexThreadId,
        _now: &Instant,
    ) -> Result<CodexPoll, Self::Error> {
        if self.interrupted.remove(&thread_id.0) {
            return Ok(CodexPoll::Completed(CodexTurnCompletion {
                summary: secret_safe_runtime_summary("embedded Codex fixture interrupted")
                    .map_err(|error| error.to_string())?,
                artifacts: Vec::new(),
                usage: measured_fixture_usage(),
            }));
        }
        let session_id = self.session_id(thread_id)?.to_owned();
        match self
            .kernel
            .next_event(&session_id, Some(Duration::from_millis(50)))
            .await
            .map_err(|error| error.to_string())?
        {
            EventPoll::Event(event) if event.kind == "turn_complete" => {
                Ok(CodexPoll::Completed(CodexTurnCompletion {
                    summary: secret_safe_runtime_summary("embedded Codex fixture completed")
                        .map_err(|error| error.to_string())?,
                    artifacts: Vec::new(),
                    usage: measured_fixture_usage(),
                }))
            }
            EventPoll::Event(event) if event.kind == "error" => Ok(CodexPoll::Failed(
                secret_safe_runtime_summary("embedded Codex fixture failed")
                    .map_err(|error| error.to_string())?,
            )),
            EventPoll::Event(_) | EventPoll::Timeout => Ok(CodexPoll::Pending),
            EventPoll::Closed => Err("embedded Codex fixture event stream closed".to_owned()),
        }
    }

    async fn accept_model_chunk(
        &mut self,
        _chunk: &winwincode_execution_port::generated::ModelChunkMessage,
        _received_at: &Instant,
    ) -> Result<(), Self::Error> {
        Err("fixture adapter does not accept ExecutionPort model chunks".to_owned())
    }

    async fn accept_action_receipt(
        &mut self,
        _receipt: &winwincode_execution_port::generated::ActionEnforcementReceiptMessage,
        _received_at: &Instant,
    ) -> Result<(), Self::Error> {
        Err("fixture adapter does not accept action receipts".to_owned())
    }

    async fn accept_approval_decision(
        &mut self,
        _decision: &winwincode_execution_port::generated::ApprovalDecisionMessage,
        _received_at: &Instant,
    ) -> Result<(), Self::Error> {
        Err("fixture adapter does not accept approval decisions".to_owned())
    }

    async fn accept_input_response(
        &mut self,
        _response: &winwincode_execution_port::generated::InputResponseMessage,
        _received_at: &Instant,
    ) -> Result<(), Self::Error> {
        Err("fixture adapter does not accept input responses".to_owned())
    }

    fn retain_execution_delivery(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        Ok(fixture_delivery(message))
    }

    fn pending_execution_deliveries(
        &mut self,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        Ok(Vec::new())
    }

    fn record_execution_delivery_sent(&mut self, _delivery_id: &str) -> Result<(), Self::Error> {
        Ok(())
    }

    fn accept_execution_delivery_ack(
        &mut self,
        _acknowledgement: &ExecutionPortMessage,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn retain_candidate_artifact(
        &mut self,
        _upload: &CandidateArtifactUpload,
    ) -> Result<RetainedCandidateArtifact, Self::Error> {
        Err("ProductSession fixture cannot retain a candidate Artifact".to_owned())
    }

    fn accept_candidate_artifact_ack(
        &mut self,
        _acknowledgement: &ArtifactAckMessage,
    ) -> Result<CandidateArtifactAckOutcome, Self::Error> {
        Err("ProductSession fixture cannot accept a candidate Artifact ack".to_owned())
    }

    fn accepted_candidate_artifact(
        &mut self,
        _authority: &CandidateArtifactAuthority,
    ) -> Result<Option<ArtifactReference>, Self::Error> {
        Err("ProductSession fixture has no candidate Artifact".to_owned())
    }

    fn cancel_candidate_artifact(
        &mut self,
        _authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error> {
        Err("ProductSession fixture has no candidate Artifact".to_owned())
    }

    fn replay_execution_deliveries(
        &mut self,
        _request: &winwincode_execution_port::generated::RuntimeReplayRequestMessage,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        Ok(Vec::new())
    }

    fn retain_job_outcome(
        &mut self,
        _thread_id: &CodexThreadId,
        outcome: &winwincode_execution_port::generated::JobOutcomeMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        self.retain_execution_delivery(&ExecutionPortMessage::JobOutcomeMessage(outcome.clone()))
    }

    fn take_execution_messages(&mut self) -> Result<Vec<ExecutionPortMessage>, Self::Error> {
        Ok(Vec::new())
    }

    async fn interrupt(
        &mut self,
        thread_id: &CodexThreadId,
        _interrupted_at: &Instant,
    ) -> Result<(), Self::Error> {
        self.stats.interrupt_calls += 1;
        let session_id = self.session_id(thread_id)?.to_owned();
        self.kernel
            .interrupt(&session_id)
            .await
            .map_err(|error| error.to_string())?;
        self.interrupted.insert(thread_id.0.clone());
        Ok(())
    }

    async fn close_thread(&mut self, thread_id: &CodexThreadId) -> Result<(), Self::Error> {
        self.stats.close_calls += 1;
        let session_id = self
            .sessions
            .remove(&thread_id.0)
            .ok_or_else(|| "Codex thread is not registered by the fixture adapter".to_owned())?;
        self.kernel
            .close_session(&session_id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn shutdown(&mut self) -> Result<(), Self::Error> {
        self.stats.shutdown_calls += 1;
        self.kernel
            .shutdown()
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecoveringPort {
    connected: Arc<AtomicBool>,
    messages: Arc<Mutex<Vec<ExecutionPortMessage>>>,
}

impl RecoveringPort {
    fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::SeqCst);
    }

    fn messages(&self) -> Vec<ExecutionPortMessage> {
        self.messages
            .lock()
            .expect("lock recorded messages")
            .clone()
    }
}

impl WorkerExecutionPort for RecoveringPort {
    type Error = ();

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let connected = self.connected.load(Ordering::SeqCst);
        if connected {
            self.messages
                .lock()
                .expect("lock recorded messages")
                .push(message);
        }
        std::future::ready(connected.then_some(()).ok_or(()))
    }
}

fn id(prefix: &str, suffix: char) -> String {
    format!("{prefix}_{}", suffix.to_string().repeat(26))
}

fn now() -> Instant {
    Instant(NOW.to_owned())
}

fn later() -> Instant {
    Instant("2027-01-15T08:00:03.000Z".to_owned())
}

fn worker_config() -> WorkerConfig {
    WorkerConfig {
        worker_id: WorkerId(id("wrk", 'A')),
        worker_instance_id: WorkerInstanceId(id("wki", 'A')),
        started_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
        capabilities: WorkerCapabilitySet {
            capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            features: vec![WorkerCapabilityFeature::Sandbox],
            max_concurrent_jobs: 1,
            platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        },
    }
}

fn lease(suffix: char) -> ExecutionLeaseStamp {
    ExecutionLeaseStamp {
        attempt: 1,
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken("7".to_owned()),
        issued_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
        job_id: ExecutionJobId(id("job", suffix)),
        lease_id: LeaseId(id("lse", suffix)),
        worker_id: WorkerId(id("wrk", 'A')),
        worker_instance_id: WorkerInstanceId(id("wki", 'A')),
    }
}

fn dispatch(suffix: char, goal: &str) -> JobDispatchMessage {
    let lease = lease(suffix);
    JobDispatchMessage {
        job: ExecutionJob {
            attempt: 1,
            execution_profile: "real-local-codex-fixture".to_owned(),
            goal: goal.to_owned(),
            job_id: lease.job_id.clone(),
            limits: ExecutionLimits {
                deadline_at: Instant("2027-01-15T08:04:30.000Z".to_owned()),
                max_artifact_bytes: 1_000_000,
                max_runtime_seconds: 240,
            },
            payload_digest: Sha256Digest(format!(
                "sha256:{}",
                suffix.to_ascii_lowercase().to_string().repeat(64)
            )),
            scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
                kind: ProductSessionExecutionScopeKind::ProductSession,
                product_session_id: ProductSessionId(id("psn", suffix)),
            }),
            stage_input: None,
            workspace: ExecutionWorkspace {
                checkout_revision: "HEAD".to_owned(),
                repository_id: RepositoryId(id("rpo", suffix)),
                write_mode: ExecutionWorkspaceWriteMode::Candidate,
            },
        },
        kind: JobDispatchMessageKind::JobDispatch,
        lease,
        message_id: ExecutionMessageId(id("msg", suffix)),
        replacement_authority: None,
        request_id: RequestId(id("req", suffix)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
    }
}

async fn register(worker: &mut WorkerMain<RecoveringPort, RealKernelAdapter>) {
    worker.start(now()).await.expect("send registration");
    let registration = WorkerRegistrationResultMessage {
        error: None,
        heartbeat_interval_ms: 2_000,
        kind: WorkerRegistrationResultMessageKind::WorkerRegistrationResult,
        lease_recovery: WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
        message_id: ExecutionMessageId(id("msg", 'R')),
        request_id: RequestId("req_00000000000000000000000001".to_owned()),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        server_time: now(),
        status: WorkerRegistrationResultMessageStatus::Accepted,
        worker_id: WorkerId(id("wrk", 'A')),
        worker_instance_id: WorkerInstanceId(id("wki", 'A')),
    };
    worker
        .accept_control(
            &ExecutionPortMessage::WorkerRegistrationResultMessage(registration),
            now(),
        )
        .await
        .expect("accept registration");
}

fn cancel(active: &ActiveJob) -> JobCancelMessage {
    JobCancelMessage {
        kind: JobCancelMessageKind::JobCancel,
        lease: active.lease.clone(),
        message_id: ExecutionMessageId(id("msg", 'C')),
        reason: JobCancelMessageReason::UserRequested,
        requested_at: later(),
        request_id: RequestId(id("req", 'C')),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: later(),
        session_identity: active.session_identity.clone(),
        worker_session_id: active.worker_session_id.clone(),
    }
}

async fn poll_until_terminal(worker: &mut WorkerMain<RecoveringPort, RealKernelAdapter>) {
    for _ in 0..200 {
        worker.poll_codex(later()).await.expect("poll real Codex");
        if worker.active_jobs().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("real embedded Codex fixture did not reach a terminal result");
}

#[test]
fn worker_runs_real_local_codex_once_across_disconnect_duplicate_cancel_and_cleanup() {
    std::thread::Builder::new()
        .name("real-codex-fixture".to_owned())
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(32 * 1024 * 1024)
                .build()
                .expect("build fixture runtime")
                .block_on(run_real_local_codex_fixture());
        })
        .expect("spawn fixture thread")
        .join()
        .expect("real Codex fixture thread completed");
}

#[allow(clippy::too_many_lines)]
async fn run_real_local_codex_fixture() {
    let root = FixtureRoot::create();
    let model_port = Arc::new(DeterministicModelPort::default());
    let kernel = Arc::new(
        Kernel::new(
            KernelOptions::new(
                root.home(),
                std::env::current_exe().expect("test executable"),
            ),
            model_port.clone(),
            Arc::new(winwincode_kernel::RejectingKernelActionGate),
        )
        .expect("construct embedded Codex kernel"),
    );
    let port = RecoveringPort::default();
    port.set_connected(true);
    let observer = port.clone();
    let adapter = RealKernelAdapter::new(kernel.clone());
    let workspaces = JobWorkspaceRuntime::open(root.workspaces(), root.sources())
        .expect("open Job workspace runtime");
    let mut worker = WorkerMain::new(worker_config(), port, adapter, workspaces);
    register(&mut worker).await;

    observer.set_connected(false);
    let disconnected = worker
        .heartbeat(now())
        .await
        .expect_err("disconnected port rejects heartbeat");
    assert_eq!(disconnected.code, WorkerErrorCode::ExecutionPort);
    observer.set_connected(true);
    worker
        .heartbeat(later())
        .await
        .expect("heartbeat resumes on the same ExecutionPort");

    let successful = dispatch('A', "Return one deterministic fixture answer.");
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(successful.clone()),
            now(),
        )
        .await
        .expect("dispatch real Codex job");
    poll_until_terminal(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(successful.clone()),
            later(),
        )
        .await
        .expect("classify exact replay");

    let cancelling = dispatch('B', "Wait for deterministic cancellation.");
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(cancelling),
            later(),
        )
        .await
        .expect("dispatch cancellation fixture");
    for _ in 0..200 {
        if model_port.request_count() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        model_port.request_count(),
        2,
        "the second real Codex turn reached the injected ModelPort"
    );
    let cancel_message = cancel(worker.active_jobs()[0]);
    worker
        .accept_control(
            &ExecutionPortMessage::JobCancelMessage(cancel_message),
            later(),
        )
        .await
        .expect("cancel exact active session");
    poll_until_terminal(&mut worker).await;

    let shutdown = worker.shutdown(later()).await.expect("shutdown Worker");
    assert!(shutdown.cancelled_jobs.is_empty());
    assert_eq!(shutdown.codex_failures, 0);
    assert_eq!(worker.lifecycle(), WorkerLifecycleState::Stopped);

    let messages = observer.messages();
    let dispatch_statuses = messages
        .iter()
        .filter_map(|message| match message {
            ExecutionPortMessage::JobDispatchResultMessage(result)
                if result.job_id == successful.job.job_id =>
            {
                Some(result.status.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        dispatch_statuses,
        [
            JobDispatchResultMessageStatus::Accepted,
            JobDispatchResultMessageStatus::Duplicate,
        ]
    );
    let outcomes = messages
        .iter()
        .filter_map(|message| match message {
            ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(outcomes.len(), 2, "one terminal fact per distinct Job");
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.lease.job_id == successful.job.job_id)
            .count(),
        1,
        "replaying one dispatch must not add a second execution fact"
    );
    assert_eq!(
        outcomes[0].outcome.status,
        ExecutionOutcomeStatus::Succeeded
    );
    assert_eq!(
        outcomes[0].outcome.usage,
        Some(measured_fixture_usage()),
        "the successful terminal fact keeps the adapter-measured usage on exact dispatch replay"
    );
    assert_eq!(
        outcomes[1].outcome.status,
        ExecutionOutcomeStatus::Cancelled
    );
    assert!(
        outcomes[1].outcome.usage.is_none(),
        "a cancelled terminal fact does not invent usage"
    );
    assert_ne!(
        outcomes[0].worker_session_id, outcomes[1].worker_session_id,
        "distinct executions keep distinct Worker sessions"
    );
    assert_ne!(
        outcomes[0].session_identity.codex_thread_id, outcomes[1].session_identity.codex_thread_id,
        "distinct executions keep distinct Codex threads"
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        ExecutionPortMessage::JobCancelAckMessage(ack)
            if ack.status == JobCancelAckMessageStatus::Accepted
    )));

    let (port, adapter) = worker.into_parts();
    assert_eq!(adapter.stats.ensure_calls, 2);
    assert_eq!(adapter.stats.created_sessions, 2);
    assert_eq!(adapter.stats.submit_calls, 2);
    assert_eq!(adapter.stats.interrupt_calls, 1);
    assert_eq!(adapter.stats.close_calls, 2);
    assert_eq!(adapter.stats.shutdown_calls, 1);
    assert!(adapter.sessions.is_empty());
    assert_eq!(model_port.request_count(), 2);
    assert_eq!(
        kernel.list_sessions().await.unwrap_err().code(),
        "KERNEL_CLOSED"
    );
    assert_eq!(
        port.messages()
            .iter()
            .filter(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)))
            .count(),
        2
    );
}
