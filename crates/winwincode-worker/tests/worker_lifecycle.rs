// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::large_futures, clippy::too_many_lines)]

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, ChangeBatchId, CodexThreadId, CriterionId, ExecutionAckSequence, ExecutionEventId,
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    ProductSessionId, RepositoryId, RequestId, Revision, SchemaVersion, Sha256Digest, WorkContract,
    WorkContractId, WorkItem, WorkItemId, WorkItemState, WorkRunId, WorkerId, WorkerInstanceId,
    WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::agent_config::{AgentProfileSettings, resolve_agent_session_config};
use winwincode_execution_port::change_batch_identity::derive_change_batch_id;
use winwincode_execution_port::generated::{
    AppliedFileOperation, AppliedFileSummary, ArtifactAckMessage, ArtifactAckMessageKind,
    ArtifactChunkMessage, ArtifactChunkMessageKind, ArtifactDescriptor, ArtifactKind,
    ArtifactOpenMessage, ArtifactOpenMessageKind, ArtifactReference, ChangeBatchIdentity,
    ChangeBatchProgressEvent, ChangeBatchProgressState, ChangeBatchProposal,
    ChangeBatchProposalDisposition, ChangeBatchProposalEvent, EncodedPayload,
    ExecutionEventCategory, ExecutionEventRecord, ExecutionJob, ExecutionJobReplacementAuthority,
    ExecutionLeaseStamp, ExecutionLimits, ExecutionOutcomeStatus, ExecutionOutcomeUsage,
    ExecutionPortMessage, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
    FinalCandidateFreezeFact, JobCancelAckMessageStatus, JobCancelMessage, JobCancelMessageKind,
    JobCancelMessageReason, JobDispatchMessage, JobDispatchMessageKind,
    JobDispatchResultMessageStatus, LeaseWriteStatus, ModelGatewayRoute,
    ProductSessionExecutionScope, ProductSessionExecutionScopeKind, RepairLoopCounters,
    RepairLoopStopReason, RuntimeEventMessage, RuntimeEventMessageKind, ValidationProfileName,
    WorkRunExecutionScope, WorkRunExecutionScopeKind, WorkRunInput, WorkerCapabilityFeature,
    WorkerCapabilitySet, WorkerCapabilitySetPlatform, WorkerRegistrationResultMessage,
    WorkerRegistrationResultMessageKind, WorkerRegistrationResultMessageLeaseRecovery,
    WorkerRegistrationResultMessageStatus,
};
use winwincode_execution_port::transport::{
    ExecutionPortCore, FrameDirection, RemoteTransportAdapter, TypedFrame,
};
use winwincode_worker::validation_artifact::DurableValidationArtifactStore;
use winwincode_worker::{
    ArtifactAckOutcome, CandidateArtifactAckOutcome, CandidateArtifactAuthority,
    CandidateArtifactUpload, CodexCoreAdapter, CodexPoll, CodexRunKey, CodexThreadSession,
    CodexThreadStart, CodexTurnCompletion, DelegatedLoopStopFact, DelegatedObserverPreflight,
    DelegatedObserverPreflightOutcome, DelegatedPollOutcome, DiagnosticArtifactAuthority,
    DiagnosticArtifactUpload, DurableExecutionDelivery, RetainedCandidateArtifact,
    RetainedDiagnosticArtifact, WorkerConfig, WorkerErrorCode, WorkerExecutionPort,
    WorkerLifecycleState, WorkerMain, secret_safe_runtime_summary,
    workspace_runtime::{
        ChangeBatchExecutionRequest, ChangeBatchExecutionResult, ChangeBatchExecutor,
        ChangeBatchExecutorFuture, JobWorkspaceRuntime, ObservationModelConfiguration,
    },
};

const NOW: &str = "2027-01-15T08:00:02.000Z";
const OBSERVER_VALIDATION_CONFIG: &str = r#"schemaVersion = 1

[[commands]]
id = "typescript-check"
phase = "validation"
language = "typescript"
diagnosticParserVersion = "typescript_v1"
allowedCompanionPaths = []
argv = ["/usr/bin/python3", "-B", "-c", 'print("delegated.txt(1,1): error TS2307: Cannot find module existing-module."); raise SystemExit(1)']
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[commands]]
id = "rust-placeholder"
phase = "validation"
language = "rust"
allowedCompanionPaths = []
argv = ["/usr/bin/python3", "-B", "-c", "raise SystemExit(0)"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[commands]]
id = "python-placeholder"
phase = "validation"
language = "python"
allowedCompanionPaths = []
argv = ["/usr/bin/python3", "-B", "-c", "raise SystemExit(0)"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[profiles]]
name = "changed"
commandIds = ["typescript-check"]

[[profiles]]
name = "fast"
commandIds = ["rust-placeholder"]

[[profiles]]
name = "affected"
commandIds = ["python-placeholder"]

[[profiles]]
name = "final"
commandIds = ["typescript-check", "rust-placeholder", "python-placeholder"]
"#;
const PASSING_VALIDATION_CONFIG: &str = r#"schemaVersion = 1

[[commands]]
id = "typescript-check"
phase = "validation"
language = "typescript"
diagnosticParserVersion = "typescript_v1"
allowedCompanionPaths = []
argv = ["/usr/bin/python3", "-B", "-c", "raise SystemExit(0)"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[commands]]
id = "rust-check"
phase = "validation"
language = "rust"
allowedCompanionPaths = []
argv = ["/usr/bin/python3", "-B", "-c", "raise SystemExit(0)"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[commands]]
id = "python-check"
phase = "validation"
language = "python"
allowedCompanionPaths = []
argv = ["/usr/bin/python3", "-B", "-c", "raise SystemExit(0)"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[profiles]]
name = "changed"
commandIds = ["typescript-check"]

[[profiles]]
name = "fast"
commandIds = ["rust-check"]

[[profiles]]
name = "affected"
commandIds = ["python-check"]

[[profiles]]
name = "final"
commandIds = ["typescript-check", "rust-check", "python-check"]
"#;
type TestFuture<'a, Output> = Pin<Box<dyn Future<Output = Output> + 'a>>;

#[derive(Clone, Default)]
struct RecordingPort {
    messages: Rc<RefCell<Vec<ExecutionPortMessage>>>,
    failures_remaining: Rc<Cell<usize>>,
    backpressured: Rc<Cell<bool>>,
    attempts: Rc<Cell<usize>>,
    latency: Rc<Cell<std::time::Duration>>,
    reject_next: Rc<Cell<bool>>,
}

impl RecordingPort {
    fn fail_once() -> Self {
        Self {
            messages: Rc::new(RefCell::new(Vec::new())),
            failures_remaining: Rc::new(Cell::new(1)),
            ..Self::default()
        }
    }

    fn failures_handle(&self) -> Rc<Cell<usize>> {
        Rc::clone(&self.failures_remaining)
    }
}

#[derive(Debug)]
enum RecordingPortError {
    Unavailable,
    Backpressure,
    MessageRejected,
}

impl WorkerExecutionPort for RecordingPort {
    type Error = RecordingPortError;

    fn failure_kind(error: &Self::Error) -> winwincode_codex::ExecutionPortFailureKind {
        match error {
            RecordingPortError::Backpressure => {
                winwincode_codex::ExecutionPortFailureKind::Backpressure
            }
            RecordingPortError::Unavailable => {
                winwincode_codex::ExecutionPortFailureKind::Unavailable
            }
            RecordingPortError::MessageRejected => {
                winwincode_codex::ExecutionPortFailureKind::MessageRejected
            }
        }
    }

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        Box::pin(async move {
            self.attempts.set(self.attempts.get() + 1);
            if self.reject_next.replace(false) {
                return Err(RecordingPortError::MessageRejected);
            }
            if self.backpressured.get() {
                return Err(RecordingPortError::Backpressure);
            }
            if self.failures_remaining.get() > 0 {
                self.failures_remaining
                    .set(self.failures_remaining.get().saturating_sub(1));
                return Err(RecordingPortError::Unavailable);
            }
            let latency = self.latency.get();
            if !latency.is_zero() {
                tokio::time::sleep(latency).await;
            }
            self.messages.borrow_mut().push(message);
            Ok(())
        })
    }
}

#[derive(Default)]
struct CodexState {
    retained_usage: HashMap<String, ExecutionOutcomeUsage>,
    calls: Vec<String>,
    threads: VecDeque<CodexThreadId>,
    workspaces: HashMap<String, PathBuf>,
    workspace_revisions: HashMap<String, WorkspaceRevision>,
    polls: HashMap<String, VecDeque<Result<CodexPoll, ()>>>,
    failures: HashSet<FailurePoint>,
    durable_deliveries: Vec<DurableExecutionDelivery>,
    pending_delivery_ids: HashSet<String>,
    candidate_delivery_ids: HashSet<String>,
    queued_execution_messages: Vec<ExecutionPortMessage>,
    candidate_upload: Option<CandidateArtifactUpload>,
    candidate_reference: Option<ArtifactReference>,
    candidate_completed_predecessor: Option<ArtifactReference>,
    accepted_candidate: Option<ArtifactReference>,
    candidate_cancel_failures_remaining: usize,
    model_open_acknowledged: bool,
    model_start_requests: Vec<winwincode_execution_port::generated::ModelOpenMessage>,
    delegated_stops: HashMap<String, DelegatedLoopStopFact>,
    final_freezes: Vec<FinalCandidateFreezeFact>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum FailurePoint {
    Ensure,
    Submit,
    ModelChunk,
    Interrupt,
    Close,
    RetainOutcome,
    Shutdown,
}

#[derive(Clone, Default)]
struct FakeCodex {
    state: Arc<Mutex<CodexState>>,
}

#[derive(Clone, Debug)]
struct AppliedBatchExecutor {
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl ChangeBatchExecutor for AppliedBatchExecutor {
    fn execute<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        self.calls.lock().expect("batch calls").push("execute");
        std::fs::write(request.checkout.join("delegated.txt"), b"fixture\n")
            .expect("write applied ChangeBatch fixture");
        Box::pin(async { Ok(applied_batch_result()) })
    }

    fn recover<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        self.calls.lock().expect("batch calls").push("recover");
        std::fs::write(request.checkout.join("delegated.txt"), b"fixture\n")
            .expect("recover applied ChangeBatch fixture");
        Box::pin(async { Ok(applied_batch_result()) })
    }

    fn cancel<'operation>(
        &'operation mut self,
        _request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        self.calls.lock().expect("batch calls").push("cancel");
        Box::pin(async { Ok(ChangeBatchExecutionResult::RolledBack { artifact_ref: None }) })
    }
}

fn applied_batch_result() -> ChangeBatchExecutionResult {
    ChangeBatchExecutionResult::Applied {
        files: vec![AppliedFileSummary {
            after_sha256: Some(Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(b"fixture\n")
            ))),
            before_sha256: None,
            bytes_after: 8,
            bytes_before: 0,
            mode_after: Some("0644".to_owned()),
            mode_before: None,
            move_path: None,
            operation: AppliedFileOperation::Create,
            path: "delegated.txt".to_owned(),
        }],
        artifact_ref: None,
    }
}

impl FakeCodex {
    fn with_threads(threads: impl IntoIterator<Item = CodexThreadId>) -> Self {
        let state = CodexState {
            threads: threads.into_iter().collect(),
            ..CodexState::default()
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.state.lock().expect("FakeCodex state").calls.clone()
    }

    fn queue_poll(&self, thread_id: &CodexThreadId, poll: Result<CodexPoll, ()>) {
        self.state
            .lock()
            .expect("FakeCodex state")
            .polls
            .entry(thread_id.0.clone())
            .or_default()
            .push_back(poll);
    }

    fn workspace(&self, thread_id: &CodexThreadId) -> PathBuf {
        self.state
            .lock()
            .expect("FakeCodex state")
            .workspaces
            .get(&thread_id.0)
            .expect("captured Job workspace")
            .clone()
    }

    fn workspace_revision(&self, thread_id: &CodexThreadId) -> WorkspaceRevision {
        self.state
            .lock()
            .expect("FakeCodex state")
            .workspace_revisions
            .get(&thread_id.0)
            .expect("captured Job workspace revision")
            .clone()
    }

    fn fail_next_candidate_cancel(&self) {
        self.state
            .lock()
            .expect("FakeCodex state")
            .candidate_cancel_failures_remaining = 1;
    }

    fn set_delegated_stop(&self, thread_id: &CodexThreadId, fact: DelegatedLoopStopFact) {
        self.state
            .lock()
            .expect("FakeCodex state")
            .delegated_stops
            .insert(thread_id.0.clone(), fact);
    }

    fn final_freezes(&self) -> Vec<FinalCandidateFreezeFact> {
        self.state
            .lock()
            .expect("FakeCodex state")
            .final_freezes
            .clone()
    }

    /// Injects one durable pending delivery (used to model frames retained
    /// while the Server exchange was down / restarting).
    fn inject_pending_delivery(&self, message: &ExecutionPortMessage) {
        let delivery = fixture_delivery(message);
        let mut state = self.state.lock().expect("FakeCodex state");
        state
            .pending_delivery_ids
            .insert(delivery.delivery_id.clone());
        state.durable_deliveries.push(delivery);
    }

    /// Queues a Core execution message for `take_execution_messages`
    /// (the production path that can emit runtime.event after Job teardown).
    fn queue_execution_message(&self, message: ExecutionPortMessage) {
        self.state
            .lock()
            .expect("FakeCodex state")
            .queued_execution_messages
            .push(message);
    }

    fn pending_kinds(&self) -> Vec<String> {
        let state = self.state.lock().expect("FakeCodex state");
        state
            .durable_deliveries
            .iter()
            .filter(|delivery| state.pending_delivery_ids.contains(&delivery.delivery_id))
            .map(|delivery| match &delivery.message {
                ExecutionPortMessage::RuntimeEventMessage(_) => "runtime.event".to_owned(),
                ExecutionPortMessage::ArtifactChunkMessage(_) => "artifact.chunk".to_owned(),
                ExecutionPortMessage::ArtifactOpenMessage(_) => "artifact.open".to_owned(),
                ExecutionPortMessage::JobOutcomeMessage(_) => "job.outcome".to_owned(),
                ExecutionPortMessage::ModelChunkMessage(_) => "model.chunk".to_owned(),
                ExecutionPortMessage::WorkerHeartbeatMessage(_) => "worker.heartbeat".to_owned(),
                ExecutionPortMessage::JobDispatchResultMessage(_) => {
                    "job.dispatch_result".to_owned()
                }
                _ => "other".to_owned(),
            })
            .collect()
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

fn execution_port_fixture(kind: &str) -> ExecutionPortMessage {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("decode execution port fixtures");
    let message = fixture["messages"]
        .as_array()
        .expect("fixture messages")
        .iter()
        .find(|message| message["kind"] == kind)
        .expect("fixture kind")
        .clone();
    serde_json::from_value(message).expect("decode generated message")
}

impl CodexCoreAdapter for FakeCodex {
    type Error = ();

    fn local_model_start_guard(
        &mut self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
        _now: &Instant,
        _observed_at: std::time::Instant,
    ) -> Result<Option<winwincode_codex::LocalModelStartGuard>, Self::Error> {
        let mut state = self.state.lock().unwrap();
        assert!(
            state
                .durable_deliveries
                .iter()
                .any(|delivery| delivery.message
                    == ExecutionPortMessage::ModelOpenMessage(open.clone())),
            "every role hands off through the original durable request"
        );
        state.model_start_requests.push(open.clone());
        Ok(None)
    }

    fn retained_outcome_usage(
        &mut self,
        thread_id: &CodexThreadId,
    ) -> Result<Option<ExecutionOutcomeUsage>, Self::Error> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .retained_usage
            .get(&thread_id.0)
            .cloned())
    }

    fn renew_lease(
        &mut self,
        _thread_id: &CodexThreadId,
        _renewal: &winwincode_execution_port::generated::LeaseRenewMessage,
        _now: &Instant,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    fn ensure_thread(
        &mut self,
        start: CodexThreadStart<'_>,
    ) -> impl Future<Output = Result<CodexThreadSession, Self::Error>> {
        let agent_config = resolve_agent_session_config(
            start.worker_id,
            &worker_config(1).capabilities,
            &start.job.execution_profile,
            AgentProfileSettings {
                fusion: None,
                jev_judge: None,
                jev_context: None,
                provider: "fixture-provider".to_owned(),
                model: "fixture-model".to_owned(),
                reasoning: "provider_default".to_owned(),
                tools: Vec::new(),
                sandbox: match start.job.workspace.write_mode {
                    ExecutionWorkspaceWriteMode::ReadOnly => "read-only",
                    ExecutionWorkspaceWriteMode::Candidate => "candidate",
                }
                .to_owned(),
                instructions: None,
            },
        )
        .map_err(|_| ());
        let mut state = self.state.lock().expect("FakeCodex state");
        state.calls.push(format!(
            "ensure:{}:{}:{}",
            start.run_key.job_id.0, start.run_key.attempt, start.worker_session_id.0
        ));
        state.workspaces.insert(
            start.run_key.canonical_thread_id().expect("thread").0,
            start.workspace.to_path_buf(),
        );
        state.workspace_revisions.insert(
            start.run_key.canonical_thread_id().expect("thread").0,
            start.workspace_revision.clone(),
        );
        let result = if state.failures.contains(&FailurePoint::Ensure) {
            Err(())
        } else {
            state.threads.pop_front().ok_or(())
        }
        .and_then(|thread_id| {
            agent_config.map(|agent_config| CodexThreadSession {
                thread_id,
                agent_config,
            })
        });
        std::future::ready(result)
    }

    fn submit_turn(
        &mut self,
        thread_id: &CodexThreadId,
        _goal: &str,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let mut state = self.state.lock().expect("FakeCodex state");
        state.calls.push(format!("submit:{}", thread_id.0));
        std::future::ready(if state.failures.contains(&FailurePoint::Submit) {
            Err(())
        } else {
            Ok(())
        })
    }

    fn preflight_delegated_observer(
        &mut self,
        _thread_id: &CodexThreadId,
        preflight: DelegatedObserverPreflight,
    ) -> Result<DelegatedObserverPreflightOutcome, Self::Error> {
        Ok(DelegatedObserverPreflightOutcome::Allowed {
            counters: preflight.worker_counters,
        })
    }

    fn delegated_loop_stop(
        &mut self,
        thread_id: &CodexThreadId,
    ) -> Result<Option<DelegatedLoopStopFact>, Self::Error> {
        Ok(self
            .state
            .lock()
            .expect("FakeCodex state")
            .delegated_stops
            .get(&thread_id.0)
            .cloned())
    }

    fn retain_final_candidate_freeze(
        &mut self,
        _thread_id: &CodexThreadId,
        fact: &FinalCandidateFreezeFact,
    ) -> Result<FinalCandidateFreezeFact, Self::Error> {
        let mut state = self.state.lock().expect("FakeCodex state");
        if let Some(existing) = state.final_freezes.first() {
            return (existing == fact).then(|| existing.clone()).ok_or(());
        }
        state.final_freezes.push(fact.clone());
        Ok(fact.clone())
    }

    fn poll(
        &mut self,
        thread_id: &CodexThreadId,
        _now: &Instant,
    ) -> impl Future<Output = Result<CodexPoll, Self::Error>> {
        let mut state = self.state.lock().expect("FakeCodex state");
        state.calls.push(format!("poll:{}", thread_id.0));
        let result = state
            .polls
            .get_mut(&thread_id.0)
            .and_then(VecDeque::pop_front)
            .unwrap_or(Ok(CodexPoll::Pending));
        std::future::ready(result)
    }

    fn accept_model_chunk(
        &mut self,
        chunk: &winwincode_execution_port::generated::ModelChunkMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let mut state = self.state.lock().expect("FakeCodex state");
        state
            .calls
            .push(format!("model_chunk:{}", chunk.sequence.0));
        std::future::ready(if state.failures.contains(&FailurePoint::ModelChunk) {
            Err(())
        } else {
            Ok(())
        })
    }

    fn accept_action_receipt(
        &mut self,
        _receipt: &winwincode_execution_port::generated::ActionEnforcementReceiptMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push("action_receipt".into());
        std::future::ready(Ok(()))
    }

    fn accept_approval_decision(
        &mut self,
        _decision: &winwincode_execution_port::generated::ApprovalDecisionMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push("approval_decision".into());
        std::future::ready(Ok(()))
    }

    fn accept_input_response(
        &mut self,
        response: &winwincode_execution_port::generated::InputResponseMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.state
            .lock()
            .expect("FakeCodex state")
            .calls
            .push(format!("input_response:{}", response.input_request_id.0));
        std::future::ready(Ok(()))
    }

    fn retain_execution_delivery(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        let delivery = fixture_delivery(message);
        let mut state = self.state.lock().expect("FakeCodex state");
        if let Some(existing) = state
            .durable_deliveries
            .iter()
            .find(|existing| existing.delivery_id == delivery.delivery_id)
            .cloned()
        {
            return if existing.message == delivery.message {
                state
                    .pending_delivery_ids
                    .insert(existing.delivery_id.clone());
                Ok(existing)
            } else {
                Err(())
            };
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
        let state = self.state.lock().expect("FakeCodex state");
        Ok(state
            .durable_deliveries
            .iter()
            .filter(|delivery| state.pending_delivery_ids.contains(&delivery.delivery_id))
            .cloned()
            .collect())
    }

    fn pending_execution_delivery_batch(
        &mut self,
        after_delivery: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        let state = self.state.lock().expect("FakeCodex state");
        // Production locates the cursor in the durable ledger, including sent
        // rows, before selecting the next pending deliveries.
        let start = after_delivery
            .and_then(|id| {
                state
                    .durable_deliveries
                    .iter()
                    .position(|delivery| delivery.delivery_id == id)
            })
            .map_or(0, |index| index + 1);
        Ok(state
            .durable_deliveries
            .iter()
            .skip(start)
            .filter(|delivery| state.pending_delivery_ids.contains(&delivery.delivery_id))
            .take(limit)
            .cloned()
            .collect())
    }

    fn record_execution_delivery_sent(&mut self, delivery_id: &str) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("FakeCodex state");
        if !state.pending_delivery_ids.contains(delivery_id) {
            return Err(());
        }
        let transport_only = state
            .durable_deliveries
            .iter()
            .find(|delivery| delivery.delivery_id == delivery_id)
            .is_some_and(|delivery| {
                matches!(
                    delivery.message,
                    ExecutionPortMessage::JobDispatchResultMessage(_)
                        | ExecutionPortMessage::SessionBindingMessage(_)
                        | ExecutionPortMessage::JobCancelAckMessage(_)
                )
            });
        if !state.candidate_delivery_ids.contains(delivery_id) {
            state.pending_delivery_ids.remove(delivery_id);
        }
        // Transport-only frames have no later response and are compacted after
        // a successful send.  Keeping them would make a successor process
        // collide with a fresh local message sequence while retaining the
        // response-bearing Job/Candidate frames needed for replay.  Registration
        // remains here until its matching result is accepted below.
        if transport_only {
            state
                .durable_deliveries
                .retain(|delivery| delivery.delivery_id != delivery_id);
        }
        Ok(())
    }

    fn record_execution_delivery_rejected(
        &mut self,
        delivery_id: &str,
    ) -> Result<bool, Self::Error> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .pending_delivery_ids
            .remove(delivery_id))
    }

    fn accept_execution_delivery_ack(
        &mut self,
        acknowledgement: &ExecutionPortMessage,
    ) -> Result<(), Self::Error> {
        if let ExecutionPortMessage::ModelChunkMessage(chunk) = acknowledgement {
            let mut state = self.state.lock().expect("FakeCodex state");
            state
                .calls
                .push(format!("model_open_ack:{}", chunk.sequence.0));
            if state.model_open_acknowledged {
                return Err(());
            }
            state.model_open_acknowledged = true;
        }
        if let ExecutionPortMessage::InputResponseMessage(response) = acknowledgement {
            self.state
                .lock()
                .expect("FakeCodex state")
                .calls
                .push(format!(
                    "input_response_ack:{}",
                    response.input_request_id.0
                ));
        }
        if let ExecutionPortMessage::WorkerRegistrationResultMessage(result) = acknowledgement {
            let mut state = self.state.lock().expect("FakeCodex state");
            let registration_id = state
                .durable_deliveries
                .iter()
                .find(|delivery| {
                    matches!(
                        &delivery.message,
                        ExecutionPortMessage::WorkerRegisterMessage(register)
                            if register.request_id == result.request_id
                                && register.worker_id == result.worker_id
                                && register.worker_instance_id == result.worker_instance_id
                    )
                })
                .map(|delivery| delivery.delivery_id.clone());
            if let Some(registration_id) = registration_id {
                state.pending_delivery_ids.remove(&registration_id);
                state
                    .durable_deliveries
                    .retain(|delivery| delivery.delivery_id != registration_id);
            }
        }
        Ok(())
    }

    fn retain_candidate_artifact(
        &mut self,
        upload: &CandidateArtifactUpload,
    ) -> Result<RetainedCandidateArtifact, Self::Error> {
        let mut state = self.state.lock().expect("FakeCodex state");
        if let Some(existing) = &state.candidate_upload {
            let replacement_matches = upload
                .replacement_authority
                .as_ref()
                .and_then(|replacement| {
                    replacement
                        .predecessor_session_identity
                        .as_ref()
                        .map(|session| (replacement, session))
                })
                .is_some_and(|(replacement, session)| {
                    existing.lease == replacement.predecessor_lease
                        && existing.worker_session_id == session.worker_session_id
                        && existing.session_identity == *session
                        && upload.lease == replacement.successor_lease
                        && existing.execution_profile == upload.execution_profile
                        && existing.bytes == upload.bytes
                        && existing.digest == upload.digest
                });
            if existing != upload && !replacement_matches {
                return Err(());
            }
            return Ok(RetainedCandidateArtifact {
                artifact: state.candidate_reference.clone().ok_or(())?,
                authority: existing.authority(),
                deliveries: Vec::new(),
                already_accepted: state.accepted_candidate.is_some(),
            });
        }

        let artifact = ArtifactReference {
            artifact_id: ArtifactId(id("art", 'C')),
            digest: upload.digest.clone(),
        };
        let descriptor = ArtifactDescriptor {
            artifact_id: artifact.artifact_id.clone(),
            digest: artifact.digest.clone(),
            file_name: Some("candidate.json".to_owned()),
            kind: ArtifactKind::Candidate,
            media_type: winwincode_worker::stage_product::CANDIDATE_MEDIA_TYPE.to_owned(),
            size_bytes: i64::try_from(upload.bytes.len()).map_err(|_| ())?,
        };
        let open = ExecutionPortMessage::ArtifactOpenMessage(ArtifactOpenMessage {
            replaces_artifact_id: None,
            artifact: descriptor,
            kind: ArtifactOpenMessageKind::ArtifactOpen,
            lease: upload.lease.clone(),
            message_id: ExecutionMessageId(id("msg", 'O')),
            request_id: RequestId(id("req", 'O')),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: upload.created_at.clone(),
            session_identity: upload.session_identity.clone(),
            snapshot_id: None,
            worker_session_id: upload.worker_session_id.clone(),
        });
        let chunk = ExecutionPortMessage::ArtifactChunkMessage(ArtifactChunkMessage {
            artifact_id: artifact.artifact_id.clone(),
            is_final: true,
            kind: ArtifactChunkMessageKind::ArtifactChunk,
            lease: upload.lease.clone(),
            message_id: ExecutionMessageId(id("msg", 'K')),
            payload: EncodedPayload {
                content_type: winwincode_worker::stage_product::CANDIDATE_MEDIA_TYPE.to_owned(),
                data_base64: "Y2FuZGlkYXRl".to_owned(),
                payload_digest: upload.digest.clone(),
            },
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: upload.created_at.clone(),
            sequence: ExecutionSequence(1),
            session_identity: upload.session_identity.clone(),
            snapshot_id: None,
            worker_session_id: upload.worker_session_id.clone(),
        });
        let deliveries = [open, chunk]
            .iter()
            .map(fixture_delivery)
            .collect::<Vec<_>>();
        for delivery in &deliveries {
            state
                .pending_delivery_ids
                .insert(delivery.delivery_id.clone());
            state
                .candidate_delivery_ids
                .insert(delivery.delivery_id.clone());
            state.durable_deliveries.push(delivery.clone());
        }
        state.candidate_upload = Some(upload.clone());
        state.candidate_reference = Some(artifact.clone());
        Ok(RetainedCandidateArtifact {
            artifact,
            authority: upload.authority(),
            deliveries,
            already_accepted: false,
        })
    }

    fn accept_candidate_artifact_ack(
        &mut self,
        acknowledgement: &ArtifactAckMessage,
    ) -> Result<CandidateArtifactAckOutcome, Self::Error> {
        let mut state = self.state.lock().expect("FakeCodex state");
        let upload = state.candidate_upload.clone().ok_or(())?;
        let artifact = state.candidate_reference.clone().ok_or(())?;
        if acknowledgement.artifact_id != artifact.artifact_id
            || acknowledgement.lease != upload.lease
            || acknowledgement.worker_session_id != upload.worker_session_id
            || acknowledgement.session_identity != upload.session_identity
            || !matches!(
                acknowledgement.status,
                LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
            )
            || acknowledgement.error.is_some()
            || acknowledgement.replay_from_sequence.is_some()
        {
            return Err(());
        }
        if let Some(predecessor) = &acknowledgement.retained_artifact {
            if state.candidate_completed_predecessor.as_ref() != Some(predecessor)
                || predecessor.digest != artifact.digest
                || acknowledgement.ack_sequence.0 != 0
            {
                return Err(());
            }
            state.pending_delivery_ids.remove(&id("msg", 'O'));
            state.pending_delivery_ids.remove(&id("msg", 'K'));
            state.accepted_candidate = Some(predecessor.clone());
            return Ok(CandidateArtifactAckOutcome::Accepted(predecessor.clone()));
        }
        match acknowledgement.ack_sequence.0 {
            0 => {
                state.pending_delivery_ids.remove(&id("msg", 'O'));
                Ok(CandidateArtifactAckOutcome::Pending)
            }
            1 => {
                state.pending_delivery_ids.remove(&id("msg", 'O'));
                state.pending_delivery_ids.remove(&id("msg", 'K'));
                state.accepted_candidate = Some(artifact.clone());
                Ok(CandidateArtifactAckOutcome::Accepted(artifact))
            }
            _ => Err(()),
        }
    }

    fn accept_artifact_ack(
        &mut self,
        acknowledgement: &ArtifactAckMessage,
    ) -> Result<ArtifactAckOutcome, Self::Error> {
        self.accept_candidate_artifact_ack(acknowledgement)
            .map(|outcome| match outcome {
                CandidateArtifactAckOutcome::Pending => ArtifactAckOutcome::Pending,
                CandidateArtifactAckOutcome::Replay(deliveries) => {
                    ArtifactAckOutcome::Replay(deliveries)
                }
                CandidateArtifactAckOutcome::Accepted(reference) => {
                    ArtifactAckOutcome::Accepted(reference)
                }
            })
    }

    fn retain_diagnostic_artifact(
        &mut self,
        _upload: &DiagnosticArtifactUpload,
    ) -> Result<RetainedDiagnosticArtifact, Self::Error> {
        Err(())
    }

    fn accepted_diagnostic_artifacts(
        &mut self,
        _authority: &DiagnosticArtifactAuthority,
    ) -> Result<Vec<ArtifactReference>, Self::Error> {
        Err(())
    }

    fn accepted_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<Option<ArtifactReference>, Self::Error> {
        let state = self.state.lock().expect("FakeCodex state");
        if let Some(upload) = state.candidate_upload.as_ref()
            && upload.authority() != *authority
            && state.accepted_candidate.is_some()
        {
            return Err(());
        }
        Ok(state.accepted_candidate.clone())
    }

    fn cancel_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("FakeCodex state");
        if state.candidate_cancel_failures_remaining > 0 {
            state.candidate_cancel_failures_remaining =
                state.candidate_cancel_failures_remaining.saturating_sub(1);
            return Err(());
        }
        let Some(upload) = state.candidate_upload.as_ref() else {
            return Ok(());
        };
        if upload.authority() != *authority || state.accepted_candidate.is_some() {
            return Err(());
        }
        let delivery_ids = state.candidate_delivery_ids.drain().collect::<Vec<_>>();
        for delivery_id in delivery_ids {
            state.pending_delivery_ids.remove(&delivery_id);
        }
        state.candidate_upload = None;
        state.candidate_reference = None;
        Ok(())
    }

    fn replay_execution_deliveries(
        &mut self,
        _request: &winwincode_execution_port::generated::RuntimeReplayRequestMessage,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        Ok(Vec::new())
    }

    async fn retain_job_outcome(
        &mut self,
        _thread_id: &CodexThreadId,
        outcome: &winwincode_execution_port::generated::JobOutcomeMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        if self
            .state
            .lock()
            .expect("FakeCodex state")
            .failures
            .remove(&FailurePoint::RetainOutcome)
        {
            return Err(());
        }
        self.retain_execution_delivery(&ExecutionPortMessage::JobOutcomeMessage(outcome.clone()))
    }

    fn take_execution_messages(&mut self) -> Result<Vec<ExecutionPortMessage>, Self::Error> {
        let mut state = self.state.lock().expect("FakeCodex state");
        Ok(std::mem::take(&mut state.queued_execution_messages))
    }

    fn interrupt(
        &mut self,
        thread_id: &CodexThreadId,
        _interrupted_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let mut state = self.state.lock().expect("FakeCodex state");
        state.calls.push(format!("interrupt:{}", thread_id.0));
        std::future::ready(if state.failures.contains(&FailurePoint::Interrupt) {
            Err(())
        } else {
            Ok(())
        })
    }

    fn close_thread(
        &mut self,
        thread_id: &CodexThreadId,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let mut state = self.state.lock().expect("FakeCodex state");
        state.calls.push(format!("close:{}", thread_id.0));
        std::future::ready(if state.failures.contains(&FailurePoint::Close) {
            Err(())
        } else {
            Ok(())
        })
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Self::Error>> {
        let mut state = self.state.lock().expect("FakeCodex state");
        state.calls.push("shutdown".to_owned());
        std::future::ready(if state.failures.contains(&FailurePoint::Shutdown) {
            Err(())
        } else {
            Ok(())
        })
    }
}

fn id(prefix: &str, suffix: char) -> String {
    format!("{prefix}_{}", suffix.to_string().repeat(26))
}

fn now() -> Instant {
    Instant(NOW.to_owned())
}

fn measured_completion_usage() -> ExecutionOutcomeUsage {
    ExecutionOutcomeUsage {
        runtime_millis: 17,
        tokens: Some(23),
        known_tokens: 23,
        accounting_status:
            winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Known,
        cost_microunits: Some(29),
    }
}

fn worker_config(max_concurrent_jobs: i64) -> WorkerConfig {
    WorkerConfig {
        worker_id: WorkerId(id("wrk", 'A')),
        worker_instance_id: WorkerInstanceId(id("wki", 'A')),
        started_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
        capabilities: WorkerCapabilitySet {
            capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            features: vec![
                WorkerCapabilityFeature::ArtifactStream,
                WorkerCapabilityFeature::Mcp,
                WorkerCapabilityFeature::Sandbox,
                WorkerCapabilityFeature::Shell,
            ],
            max_concurrent_jobs,
            platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        },
    }
}

fn test_worker(
    config: WorkerConfig,
    port: RecordingPort,
    codex: FakeCodex,
) -> WorkerMain<RecordingPort, FakeCodex> {
    WorkerMain::new(config, port, codex, test_workspaces())
}

fn test_workspaces() -> JobWorkspaceRuntime {
    let (workspaces, sources) = test_workspace_paths();
    JobWorkspaceRuntime::open(workspaces, sources).expect("open fixture workspace runtime")
}

fn test_workspace_paths() -> (PathBuf, PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let base =
        std::env::var_os("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let root = base.join(format!(
        "winwincode-worker-lifecycle-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let sources = root.join("sources");
    let workspaces = root.join("workspaces");
    std::fs::create_dir_all(&sources).expect("create fixture sources");
    for suffix in ['A', 'B'] {
        let repository = sources.join(id("rpo", suffix));
        std::fs::create_dir_all(&repository).expect("create fixture repository");
        run_git(&repository, &["init", "-q"]);
        std::fs::write(repository.join("fixture.txt"), b"source\n").expect("write fixture source");
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
    (workspaces, sources)
}

fn lease(job_suffix: char) -> ExecutionLeaseStamp {
    ExecutionLeaseStamp {
        attempt: 1,
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken("7".to_owned()),
        issued_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
        job_id: ExecutionJobId(id("job", job_suffix)),
        lease_id: LeaseId(id("lse", job_suffix)),
        worker_id: WorkerId(id("wrk", 'A')),
        worker_instance_id: WorkerInstanceId(id("wki", 'A')),
    }
}

fn dispatch(job_suffix: char, scope: ExecutionScope) -> JobDispatchMessage {
    let lease = lease(job_suffix);
    let delivery_stage = matches!(&scope, ExecutionScope::WorkRunExecutionScope(_));
    let goal = "Perform the approved fixture change.";
    JobDispatchMessage {
        job: ExecutionJob {
            attachments: None,
            model_selection: None,
            attempt: 1,
            execution_profile: if delivery_stage { "planner" } else { "fixture" }.to_owned(),
            goal: goal.to_owned(),
            job_id: lease.job_id.clone(),
            limits: ExecutionLimits {
                deadline_at: Some(Instant("2027-01-15T08:04:30.000Z".to_owned())),
                max_artifact_bytes: 1_000_000,
                max_runtime_seconds: Some(240),
            },
            payload_digest: Sha256Digest(format!(
                "sha256:{}",
                job_suffix.to_ascii_lowercase().to_string().repeat(64)
            )),
            scope,
            work_input: delivery_stage.then(|| WorkRunInput {
                work_plan: None,
                device_target: None,
                delivery_spec_id: "spec-fixture".into(),
                delivery_spec_revision: Revision(2),
                schema_version: SchemaVersion::WinwincodeV1,
                snapshot_id: None,
                work_contract: WorkContract {
                    constraints: Vec::new(),
                    created_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
                    criteria: vec![winwincode_domain::Criterion {
                        id: CriterionId("crt_00000000000000000000000001".to_owned()),
                        description: "The fixture behavior is verified.".to_owned(),
                        required: true,
                        required_evidence_class: "machine".into(),
                        verification_method: Some("Run the fixture test.".to_owned()),
                    }],
                    id: WorkContractId("wct_00000000000000000000000001".to_owned()),
                    objective: goal.to_owned(),
                    protected_scope: Vec::new(),
                    required_human_authority: "approval".to_owned(),
                    revision: Revision(1),
                    schema_version: SchemaVersion::WinwincodeV1,
                    scope: vec!["Fixture source".to_owned()],
                },
                work_item: WorkItem {
                    criterion_ids: vec![CriterionId("crt_00000000000000000000000001".to_owned())],
                    depends_on: Vec::new(),
                    goal: goal.to_owned(),
                    id: WorkItemId("wit_00000000000000000000000001".to_owned()),
                    revision: Revision(1),
                    schema_version: SchemaVersion::WinwincodeV1,
                    state: WorkItemState::Ready,
                    title: "Fixture Delivery".to_owned(),
                    work_contract_id: WorkContractId("wct_00000000000000000000000001".to_owned()),
                    work_contract_revision: Revision(1),
                },
                candidate_ref: None,
            }),
            workspace: ExecutionWorkspace {
                checkout_revision: "HEAD".to_owned(),
                repository_id: RepositoryId(id("rpo", job_suffix)),
                write_mode: ExecutionWorkspaceWriteMode::ReadOnly,
            },
        },
        kind: JobDispatchMessageKind::JobDispatch,
        lease,
        message_id: ExecutionMessageId(id("msg", job_suffix)),
        replacement_authority: None,
        request_id: RequestId(id("req", job_suffix)),
        schema_version: SchemaVersion::WinwincodeV1,
        snapshot_id: None,
        sent_at: now(),
    }
}

fn writer_dispatch(job_suffix: char) -> JobDispatchMessage {
    let mut dispatch = dispatch(job_suffix, delivery_scope(job_suffix));
    let ExecutionScope::WorkRunExecutionScope(_scope) = &mut dispatch.job.scope else {
        unreachable!("writer fixture is a Delivery stage")
    };
    "executor".clone_into(&mut dispatch.job.execution_profile);
    dispatch.job.workspace.write_mode = ExecutionWorkspaceWriteMode::Candidate;
    "HEAD".clone_into(&mut dispatch.job.workspace.checkout_revision);
    "Produce candidate".clone_into(
        &mut dispatch
            .job
            .work_input
            .as_mut()
            .expect("writer work input")
            .work_item
            .title,
    );
    dispatch
}

fn delegated_task_dispatch(job_suffix: char) -> JobDispatchMessage {
    let mut dispatch = dispatch(job_suffix, delivery_scope(job_suffix));
    "executor".clone_into(&mut dispatch.job.execution_profile);
    let ExecutionScope::WorkRunExecutionScope(_scope) = &mut dispatch.job.scope else {
        unreachable!("delegated fixture is a Delivery stage")
    };
    "Delegated task".clone_into(
        &mut dispatch
            .job
            .work_input
            .as_mut()
            .expect("delegated work input")
            .work_item
            .title,
    );
    dispatch
}

fn replacement_dispatch(predecessor: &winwincode_worker::ActiveJob) -> JobDispatchMessage {
    let mut replacement = writer_dispatch('A');
    replacement.job.attempt = 2;
    replacement.lease.attempt = 2;
    replacement.lease.lease_id = LeaseId(id("lse", 'B'));
    replacement.lease.fencing_token = FencingToken("8".to_owned());
    replacement.lease.issued_at = Instant("2027-01-15T08:01:00.000Z".to_owned());
    replacement.lease.expires_at = Instant("2027-01-15T08:06:00.000Z".to_owned());
    replacement.lease.worker_instance_id = WorkerInstanceId(id("wki", 'B'));
    replacement.message_id = ExecutionMessageId(id("msg", 'B'));
    replacement.request_id = RequestId(id("req", 'B'));
    if let ExecutionScope::WorkRunExecutionScope(scope) = &mut replacement.job.scope {
        scope.work_run_id = WorkRunId(id("wrn", 'B'));
        scope.attempt = 2;
    }
    replacement.replacement_authority = Some(ExecutionJobReplacementAuthority {
        created_at: Instant("2027-01-15T08:00:59.000Z".to_owned()),
        logical_job_digest: logical_job_digest(&replacement.job),
        predecessor_lease: predecessor.lease.clone(),
        predecessor_session_identity: Some(predecessor.session_identity.clone()),
        receipt_digest: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        receipt_id: RequestId(id("req", 'Z')),
        scope: predecessor.job.scope.clone(),
        successor_lease: replacement.lease.clone(),
    });
    replacement
}

fn logical_job_digest(job: &ExecutionJob) -> Sha256Digest {
    let mut value = serde_json::to_value(job).expect("ExecutionJob value");
    value
        .as_object_mut()
        .expect("ExecutionJob object")
        .remove("attempt")
        .expect("ExecutionJob attempt");
    let scope = value
        .get_mut("scope")
        .and_then(serde_json::Value::as_object_mut)
        .expect("ExecutionJob scope");
    scope.remove("attempt");
    scope.remove("workRunId");
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&value).expect("logical Job bytes"))
    ))
}

fn delivery_scope(suffix: char) -> ExecutionScope {
    ExecutionScope::WorkRunExecutionScope(WorkRunExecutionScope {
        kind: WorkRunExecutionScopeKind::WorkRun,
        product_session_id: ProductSessionId(id("psn", suffix)),
        rework_authorization: None,
        work_contract_id: WorkContractId("wct_00000000000000000000000001".to_owned()),
        work_contract_revision: Revision(1),
        work_item_id: WorkItemId("wit_00000000000000000000000001".to_owned()),
        work_item_revision: Revision(1),
        work_run_id: WorkRunId(id("wrn", suffix)),
        attempt: 1,
    })
}

fn product_scope(suffix: char) -> ExecutionScope {
    ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
        kind: ProductSessionExecutionScopeKind::ProductSession,
        product_session_id: ProductSessionId(id("psn", suffix)),
    })
}

fn thread(suffix: char) -> CodexThreadId {
    CodexRunKey::from_dispatch(&dispatch(suffix, product_scope(suffix)))
        .canonical_thread_id()
        .expect("canonical fixture thread")
}

fn candidate_ack(
    active: &winwincode_worker::ActiveJob,
    artifact: &ArtifactReference,
    sequence: i64,
    suffix: char,
) -> ArtifactAckMessage {
    ArtifactAckMessage {
        retained_artifact: None,
        ack_sequence: ExecutionAckSequence(sequence),
        artifact_id: artifact.artifact_id.clone(),
        error: None,
        kind: ArtifactAckMessageKind::ArtifactAck,
        lease: active.lease.clone(),
        message_id: ExecutionMessageId(id("msg", suffix)),
        replay_from_sequence: None,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        session_identity: active.session_identity.clone(),
        status: LeaseWriteStatus::Accepted,
        worker_session_id: active.worker_session_id.clone(),
    }
}

fn assert_no_outcome(messages: &Rc<RefCell<Vec<ExecutionPortMessage>>>) {
    assert!(
        messages
            .borrow()
            .iter()
            .all(|message| !matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)))
    );
}

fn assert_no_candidate_product(messages: &Rc<RefCell<Vec<ExecutionPortMessage>>>) {
    assert!(messages.borrow().iter().all(|message| !matches!(
        message,
        ExecutionPortMessage::ArtifactOpenMessage(_)
            | ExecutionPortMessage::ArtifactChunkMessage(_)
    )));
}

fn observed_candidate_messages(
    messages: &Rc<RefCell<Vec<ExecutionPortMessage>>>,
) -> Vec<ExecutionPortMessage> {
    messages
        .borrow()
        .iter()
        .filter(|message| {
            matches!(
                message,
                ExecutionPortMessage::ArtifactOpenMessage(_)
                    | ExecutionPortMessage::ArtifactChunkMessage(_)
            )
        })
        .cloned()
        .collect()
}

fn observed_candidate_reference(
    messages: &Rc<RefCell<Vec<ExecutionPortMessage>>>,
) -> ArtifactReference {
    messages
        .borrow()
        .iter()
        .find_map(|message| match message {
            ExecutionPortMessage::ArtifactOpenMessage(open) => Some(ArtifactReference {
                artifact_id: open.artifact.artifact_id.clone(),
                digest: open.artifact.digest.clone(),
            }),
            _ => None,
        })
        .expect("candidate open")
}

fn acknowledge_candidate<'a>(
    worker: &'a mut WorkerMain<RecordingPort, FakeCodex>,
    active: &'a winwincode_worker::ActiveJob,
    artifact: &'a ArtifactReference,
    sequence: i64,
    suffix: char,
) -> TestFuture<'a, Result<(), winwincode_worker::WorkerError>> {
    Box::pin(async move {
        worker
            .accept_control(
                &ExecutionPortMessage::ArtifactAckMessage(candidate_ack(
                    active, artifact, sequence, suffix,
                )),
                now(),
            )
            .await?;
        worker.flush_durable_outbox().await
    })
}

fn observed_outcomes(
    messages: &Rc<RefCell<Vec<ExecutionPortMessage>>>,
) -> Vec<winwincode_execution_port::generated::JobOutcomeMessage> {
    messages
        .borrow()
        .iter()
        .filter_map(|message| match message {
            ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome.clone()),
            _ => None,
        })
        .collect()
}

fn run_git(repository: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run Git fixture command");
    assert!(
        output.status.success(),
        "Git fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn register(worker: &mut WorkerMain<RecordingPort, FakeCodex>) -> TestFuture<'_, ()> {
    Box::pin(async move {
        worker.start(now()).await.unwrap();
        let (request_id, worker_id, worker_instance_id) = worker.registration_for_test();
        let result = WorkerRegistrationResultMessage {
            error: None,
            heartbeat_interval_ms: 2_000,
            kind: WorkerRegistrationResultMessageKind::WorkerRegistrationResult,
            lease_recovery: WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
            message_id: ExecutionMessageId(id("msg", 'R')),
            request_id,
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now(),
            server_time: now(),
            status: WorkerRegistrationResultMessageStatus::Accepted,
            worker_id,
            worker_instance_id,
        };
        worker
            .accept_control(
                &ExecutionPortMessage::WorkerRegistrationResultMessage(result),
                now(),
            )
            .await
            .unwrap();
    })
}

trait WorkerTestAccess {
    fn accept_control_and_drive<'a>(
        &'a mut self,
        message: &'a ExecutionPortMessage,
        now: Instant,
    ) -> TestFuture<'a, Result<(), winwincode_worker::WorkerError>>;
    fn registration_for_test(&self) -> (RequestId, WorkerId, WorkerInstanceId);
    fn poll_codex_boxed(&mut self) -> TestFuture<'_, Result<(), winwincode_worker::WorkerError>>;
}

impl WorkerTestAccess for WorkerMain<RecordingPort, FakeCodex> {
    fn accept_control_and_drive<'a>(
        &'a mut self,
        message: &'a ExecutionPortMessage,
        now: Instant,
    ) -> TestFuture<'a, Result<(), winwincode_worker::WorkerError>> {
        Box::pin(async move {
            self.accept_control(message, now).await?;
            self.flush_durable_outbox().await
        })
    }
    fn registration_for_test(&self) -> (RequestId, WorkerId, WorkerInstanceId) {
        let active = self.lifecycle();
        assert_eq!(active, WorkerLifecycleState::Registering);
        // Registration values are deterministic and are asserted again against
        // the emitted message in every parity script.
        (
            RequestId("req_00000000000000000000000001".to_owned()),
            WorkerId(id("wrk", 'A')),
            WorkerInstanceId(id("wki", 'A')),
        )
    }

    fn poll_codex_boxed(&mut self) -> TestFuture<'_, Result<(), winwincode_worker::WorkerError>> {
        Box::pin(WorkerMain::poll_codex(self, now()))
    }
}

fn routed(message: ExecutionPortMessage, remote: bool) -> ExecutionPortMessage {
    let frame = TypedFrame::new(FrameDirection::ControlPlaneToWorker, message).unwrap();
    if remote {
        let bytes = RemoteTransportAdapter::<CaptureCore>::encode(&frame).unwrap();
        RemoteTransportAdapter::<CaptureCore>::decode(&bytes)
            .unwrap()
            .message()
            .clone()
    } else {
        frame.message().clone()
    }
}

struct CaptureCore;

impl ExecutionPortCore for CaptureCore {
    type Output = ();
    type Error = ();

    fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}

fn cancel_for(active: &winwincode_worker::ActiveJob, suffix: char) -> JobCancelMessage {
    JobCancelMessage {
        kind: JobCancelMessageKind::JobCancel,
        lease: active.lease.clone(),
        message_id: ExecutionMessageId(id("msg", suffix)),
        reason: JobCancelMessageReason::UserRequested,
        requested_at: now(),
        request_id: RequestId(id("req", suffix)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        session_identity: active.session_identity.clone(),
        worker_session_id: active.worker_session_id.clone(),
    }
}

fn output_kinds(messages: &[ExecutionPortMessage]) -> Vec<&'static str> {
    messages
        .iter()
        .map(|message| match message {
            ExecutionPortMessage::WorkerRegisterMessage(_) => "register",
            ExecutionPortMessage::WorkerHeartbeatMessage(_) => "heartbeat",
            ExecutionPortMessage::JobDispatchResultMessage(_) => "dispatch_result",
            ExecutionPortMessage::SessionBindingMessage(_) => "binding",
            ExecutionPortMessage::RuntimeEventMessage(_) => "runtime",
            ExecutionPortMessage::JobCancelAckMessage(_) => "cancel_ack",
            ExecutionPortMessage::JobOutcomeMessage(_) => "outcome",
            _ => "other",
        })
        .collect()
}

#[test]
fn standalone_binary_starts_without_an_external_execution_fallback() {
    let output = Command::new(env!("CARGO_BIN_EXE_winwincode-worker"))
        .arg("--check")
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(output.status.success());
    let identity: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(identity["role"], "execution-worker");
    assert_eq!(identity["executionKernel"], "embedded-codex-core");
    assert_eq!(identity["externalFallback"], false);

    let manifest = include_str!("../Cargo.toml");
    let source = include_str!("../src/lib.rs");
    assert!(!manifest.contains("winwincode-cli"));
    assert!(!source.contains("Command::new"));
    assert!(!source.contains("codex app-server"));
}

#[tokio::test]
async fn registration_send_loss_retries_the_durable_original_before_new_work() {
    let port = RecordingPort::fail_once();
    let observed = port.clone();
    let codex = FakeCodex::default();
    let mut worker = test_worker(worker_config(1), port, codex);

    let first = worker.start(now()).await.expect_err("first send is lost");
    assert_eq!(first.code, WorkerErrorCode::ExecutionPort);
    assert_eq!(worker.lifecycle(), WorkerLifecycleState::Registering);
    assert!(observed.messages.borrow().is_empty());

    worker.start(now()).await.expect("retry retained register");
    let sent = observed.messages.borrow();
    assert_eq!(sent.len(), 1);
    let ExecutionPortMessage::WorkerRegisterMessage(register) = &sent[0] else {
        panic!("durable retry must be the original registration frame")
    };
    assert_eq!(register.message_id.0, "xmsg_00000000000000000000000001");
    assert_eq!(register.request_id.0, "req_00000000000000000000000001");
}

#[tokio::test]
async fn streaming_model_chunks_acknowledge_the_open_only_on_sequence_one() {
    let port = RecordingPort::default();
    let codex = FakeCodex::default();
    let observed = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    let ExecutionPortMessage::ModelChunkMessage(first) = execution_port_fixture("model.chunk")
    else {
        panic!("model chunk fixture")
    };
    let mut second = first.clone();
    second.message_id = ExecutionMessageId(id("msg", '2'));
    second.sequence = ExecutionSequence(2);
    second.is_final = true;

    worker
        .accept_control(&ExecutionPortMessage::ModelChunkMessage(first), now())
        .await
        .expect("first chunk accepts and acknowledges the retained ModelOpen");
    worker
        .accept_control(&ExecutionPortMessage::ModelChunkMessage(second), now())
        .await
        .expect("later chunks advance only the model cursor");

    assert_eq!(
        observed
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("model_"))
            .collect::<Vec<_>>(),
        ["model_chunk:1", "model_open_ack:1", "model_chunk:2"]
    );
}

#[tokio::test]
async fn input_response_reaches_codex_once_and_acknowledges_the_retained_request() {
    let port = RecordingPort::default();
    let codex = FakeCodex::default();
    let observed = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    let response = execution_port_fixture("input.response");

    worker
        .accept_control(&response, now())
        .await
        .expect("input response reaches Codex");

    assert_eq!(
        observed
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("input_response"))
            .collect::<Vec<_>>(),
        [
            "input_response:inp_0000000000000000000000000E",
            "input_response_ack:inp_0000000000000000000000000E",
        ]
    );
}

#[tokio::test]
async fn remote_model_replay_requires_the_producers_durable_handoff() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let mut producer = codex.clone();
    let original = execution_port_fixture("model.open");
    producer.retain_execution_delivery(&original).unwrap();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    assert!(
        !messages
            .borrow()
            .iter()
            .any(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_))),
        "queue recovery cannot independently repeat a paid model request"
    );
    producer
        .state
        .lock()
        .unwrap()
        .queued_execution_messages
        .push(original.clone());
    worker.poll_codex_boxed().await.unwrap();
    worker.poll_codex_boxed().await.unwrap();
    let opens = messages
        .borrow()
        .iter()
        .filter(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        opens,
        vec![original],
        "one producer handoff dispatches the original identity once"
    );
    worker.shutdown(now()).await.unwrap();
}

#[tokio::test]
async fn applied_interactive_controls_do_not_depend_on_outbound_transport() {
    for kind in [
        "input.response",
        "approval.decision",
        "action.enforcement_receipt",
    ] {
        let port = RecordingPort::default();
        port.backpressured.set(true);
        let attempts = Rc::clone(&port.attempts);
        let codex = FakeCodex::default();
        let mut retained = codex.clone();
        retained
            .retain_execution_delivery(&execution_port_fixture("runtime.event"))
            .unwrap();
        let mut worker = test_worker(worker_config(1), port, codex);
        worker
            .accept_control(&execution_port_fixture(kind), now())
            .await
            .unwrap_or_else(|error| {
                panic!("{kind} is reliably applied before later sends: {error:?}")
            });
        assert_eq!(
            attempts.get(),
            0,
            "control consumption must not send unrelated frames"
        );
        assert_eq!(retained.pending_execution_deliveries().unwrap().len(), 1);
        assert_eq!(
            worker.flush_durable_outbox().await.unwrap_err().code,
            WorkerErrorCode::ExecutionBackpressure
        );
    }
}

#[tokio::test]
async fn dispatch_is_durably_accepted_before_outbound_backpressure() {
    let port = RecordingPort::default();
    let blocked = Rc::clone(&port.backpressured);
    let attempts = Rc::clone(&port.attempts);
    let codex = FakeCodex::with_threads([thread('A')]);
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    blocked.set(true);
    let before = attempts.get();
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    assert_eq!(worker.active_jobs().len(), 1);
    assert_eq!(attempts.get(), before);
    assert_eq!(
        worker.flush_durable_outbox().await.unwrap_err().code,
        WorkerErrorCode::ExecutionBackpressure
    );
}

#[tokio::test]
async fn local_and_remote_frames_drive_value_identical_worker_semantics() {
    fn script(remote: bool) -> TestFuture<'static, (Vec<ExecutionPortMessage>, Vec<String>)> {
        Box::pin(async move {
            let port = RecordingPort::default();
            let codex = FakeCodex::with_threads([thread('A')]);
            let calls = codex.clone();
            let messages = Rc::clone(&port.messages);
            let mut worker = test_worker(worker_config(2), port, codex);
            worker.start(now()).await.unwrap();
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
                .accept_control_and_drive(
                    &routed(
                        ExecutionPortMessage::WorkerRegistrationResultMessage(registration),
                        remote,
                    ),
                    now(),
                )
                .await
                .unwrap();
            let dispatch = dispatch('A', delivery_scope('A'));
            worker
                .accept_control_and_drive(
                    &routed(
                        ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                        remote,
                    ),
                    now(),
                )
                .await
                .unwrap();
            worker
                .accept_control_and_drive(
                    &routed(ExecutionPortMessage::JobDispatchMessage(dispatch), remote),
                    now(),
                )
                .await
                .unwrap();
            worker.heartbeat(now()).await.unwrap();
            let captured = messages.borrow().clone();
            (captured, calls.calls())
        })
    }

    let local = script(false).await;
    let remote = script(true).await;
    assert_eq!(local, remote);
    assert_eq!(
        output_kinds(&local.0),
        [
            "register",
            "dispatch_result",
            "binding",
            "dispatch_result",
            "heartbeat",
        ]
    );
    assert_eq!(
        local
            .0
            .iter()
            .filter_map(|message| match message {
                ExecutionPortMessage::JobDispatchResultMessage(result) => Some(&result.status),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [
            &JobDispatchResultMessageStatus::Accepted,
            &JobDispatchResultMessageStatus::Duplicate,
        ]
    );
    assert_eq!(
        local
            .1
            .iter()
            .filter(|call| call.starts_with("ensure:"))
            .count(),
        1
    );
    assert_eq!(
        local
            .1
            .iter()
            .filter(|call| call.starts_with("submit:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn snapshot_freeze_replays_after_restart_without_starting_codex() {
    use winwincode_execution_port::generated::SnapshotFreezeRequestMessage;
    use winwincode_execution_port::snapshot_freeze::validate_freeze_receipt;

    let (workspace_root, source_root) = test_workspace_paths();
    let repository = source_root.join(id("rep", 'A'));
    std::fs::rename(source_root.join(id("rpo", 'A')), &repository).unwrap();
    let common_repository = workspace_root
        .parent()
        .unwrap()
        .join("snapshot-common-source");
    std::fs::rename(&repository, &common_repository).unwrap();
    run_git(
        &common_repository,
        &[
            "worktree",
            "add",
            "--detach",
            repository.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(repository.join(".git").is_file());
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    };
    let text = |args: &[&str]| String::from_utf8(git(args)).unwrap().trim().to_owned();
    let base = text(&["rev-parse", "HEAD"]);
    let base_tree = text(&["rev-parse", "HEAD^{tree}"]);
    std::fs::write(repository.join("fixture.txt"), b"candidate\n").unwrap();
    run_git(&repository, &["add", "fixture.txt"]);
    run_git(
        &repository,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "candidate",
        ],
    );
    let commit = text(&["rev-parse", "HEAD"]);
    let tree = text(&["rev-parse", "HEAD^{tree}"]);
    let diff = git(&[
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--binary",
        "--full-index",
        &format!("{base}..{commit}"),
    ]);
    let mut content = Sha256::new();
    for field in [b"fixture.txt".as_slice(), b"100644", b"candidate\n"] {
        content.update(u64::try_from(field.len()).unwrap().to_be_bytes());
        content.update(field);
    }
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut request: SnapshotFreezeRequestMessage = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["kind"] == "snapshot.freeze_request")
            .unwrap()
            .clone(),
    )
    .unwrap();
    request.dispatch = dispatch('A', delivery_scope('A'));
    request.dispatch.job.execution_profile = "verifier".into();
    request.dispatch.job.workspace.repository_id = RepositoryId(id("rep", 'A'));
    request.dispatch.job.workspace.checkout_revision = commit.clone();
    request.lease = request.dispatch.lease.clone();
    request.repository_id = request.dispatch.job.workspace.repository_id.clone();
    request.candidate.base_commit = base;
    request.candidate.candidate_commit = commit.clone();
    request.candidate.candidate_tree = tree;
    request.candidate.candidate_ref = format!("refs/winwincode/candidates/{commit}");
    request.candidate.diff_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(diff)));
    request.base_tree_id.0 = base_tree;
    request.content_digest = Sha256Digest(format!("sha256:{:x}", content.finalize()));
    request
        .dispatch
        .job
        .work_input
        .as_mut()
        .unwrap()
        .candidate_ref = Some(request.candidate.candidate_ref.clone());
    let mut responses = Vec::new();
    let mut predecessor = None;
    for replay in 0..2 {
        let port = RecordingPort::default();
        let messages = Rc::clone(&port.messages);
        let codex = FakeCodex::with_threads([thread('A')]);
        let calls = codex.clone();
        let mut worker = WorkerMain::new(
            worker_config(1),
            port,
            codex,
            JobWorkspaceRuntime::open(&workspace_root, &source_root).unwrap(),
        );
        register(&mut worker).await;
        if replay == 0 {
            let mut foreign = request.clone();
            foreign.dispatch.lease.fencing_token.0 = "999".into();
            assert!(
                worker
                    .accept_control_and_drive(
                        &ExecutionPortMessage::SnapshotFreezeRequestMessage(foreign),
                        now()
                    )
                    .await
                    .is_err()
            );
            assert!(std::fs::read_dir(&workspace_root).unwrap().next().is_none());
        }
        worker
            .accept_control_and_drive(
                &ExecutionPortMessage::SnapshotFreezeRequestMessage(request.clone()),
                now(),
            )
            .await
            .unwrap();
        let response = messages
            .borrow()
            .iter()
            .find_map(|message| match message {
                ExecutionPortMessage::SnapshotFreezeReceiptMessage(receipt) => {
                    Some(receipt.clone())
                }
                _ => None,
            })
            .unwrap();
        validate_freeze_receipt(&request, &response).unwrap();
        responses.push(response);
        assert!(worker.active_jobs().is_empty());
        assert!(
            !calls
                .calls()
                .iter()
                .any(|call| call.starts_with("ensure:") || call.starts_with("submit:"))
        );
        let mut changed = request.clone();
        changed.content_digest.0 = format!("sha256:{}", "f".repeat(64));
        assert!(
            worker
                .accept_control_and_drive(
                    &ExecutionPortMessage::SnapshotFreezeRequestMessage(changed),
                    now()
                )
                .await
                .is_err()
        );
        assert_eq!(
            messages
                .borrow()
                .iter()
                .filter(|message| matches!(
                    message,
                    ExecutionPortMessage::SnapshotFreezeReceiptMessage(_)
                ))
                .count(),
            1
        );
        if replay == 1 {
            use winwincode_domain::{Snapshot, SnapshotId, seal_snapshot};
            use winwincode_execution_port::generated::{
                SnapshotVerificationDispatchMessage, SnapshotVerificationDispatchMessageKind,
            };
            worker
                .accept_control_and_drive(
                    &ExecutionPortMessage::JobDispatchMessage(request.dispatch.clone()),
                    now(),
                )
                .await
                .unwrap();
            assert!(
                worker.active_jobs().is_empty(),
                "missing Snapshot must reject before execution"
            );
            let receipt = &responses[1].receipt;
            let mut snapshot = Snapshot {
                schema_version: SchemaVersion::WinwincodeV1,
                snapshot_id: SnapshotId(id("snap", 'A')),
                candidate_id: receipt.candidate_id.clone(),
                work_run_id: receipt.work_run_id.clone(),
                repository_id: receipt.repository_id.clone(),
                base_commit_id: receipt.base_commit_id.clone(),
                base_tree_id: receipt.base_tree_id.clone(),
                candidate_commit_id: receipt.candidate_commit_id.clone(),
                candidate_tree_id: receipt.candidate_tree_id.clone(),
                diff_sha256: receipt.diff_sha256.clone(),
                content_digest: receipt.content_digest.clone(),
                created_at_millis: 1_800_000_000_000,
                immutable: true,
                validation_seal: Sha256Digest(String::new()),
            };
            snapshot.validation_seal = seal_snapshot(&snapshot);
            let mut dispatch = request.dispatch.clone();
            dispatch.snapshot_id = Some(snapshot.snapshot_id.clone());
            let valid = SnapshotVerificationDispatchMessage {
                kind: SnapshotVerificationDispatchMessageKind::SnapshotVerify,
                schema_version: SchemaVersion::WinwincodeV1,
                message_id: dispatch.message_id.clone(),
                sent_at: now(),
                dispatch,
                snapshot,
            };
            let mut foreign = valid.clone();
            foreign.snapshot.candidate_id.0 = id("cnd", 'Z');
            foreign.snapshot.validation_seal = seal_snapshot(&foreign.snapshot);
            assert!(
                worker
                    .accept_control_and_drive(
                        &ExecutionPortMessage::SnapshotVerificationDispatchMessage(foreign),
                        now()
                    )
                    .await
                    .is_err()
            );
            assert!(
                !calls
                    .calls()
                    .iter()
                    .any(|call| call.starts_with("ensure:") || call.starts_with("submit:"))
            );
            worker
                .accept_control_and_drive(
                    &ExecutionPortMessage::SnapshotVerificationDispatchMessage(valid),
                    now(),
                )
                .await
                .unwrap();
            worker
                .accept_control_and_drive(
                    &ExecutionPortMessage::SnapshotFreezeRequestMessage(request.clone()),
                    now(),
                )
                .await
                .unwrap();
            assert_eq!(
                messages
                    .borrow()
                    .iter()
                    .filter(|message| matches!(
                        message,
                        ExecutionPortMessage::SnapshotFreezeReceiptMessage(_)
                    ))
                    .count(),
                2,
                "late exact freeze replay must not start another model turn"
            );
            assert_eq!(worker.active_jobs().len(), 1);
            assert_eq!(
                calls
                    .calls()
                    .iter()
                    .filter(|call| call.starts_with("ensure:"))
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .calls()
                    .iter()
                    .filter(|call| call.starts_with("submit:"))
                    .count(),
                1
            );
            predecessor = Some(worker.active_jobs()[0].clone());
        }
    }
    assert_eq!(responses[0], responses[1]);

    let predecessor = predecessor.unwrap();
    let template = replacement_dispatch(&predecessor);
    let mut successor = request.clone();
    successor.dispatch.lease = template.lease;
    successor.dispatch.job.attempt = 2;
    if let ExecutionScope::WorkRunExecutionScope(scope) = &mut successor.dispatch.job.scope {
        scope.attempt = 2;
        scope.work_run_id = WorkRunId(id("wrn", 'B'));
    }
    let mut proof = template.replacement_authority.unwrap();
    proof.logical_job_digest = logical_job_digest(&successor.dispatch.job);
    proof.scope = successor.dispatch.job.scope.clone();
    successor.dispatch.replacement_authority = Some(proof);
    successor.dispatch.message_id = ExecutionMessageId(id("msg", 'B'));
    successor.dispatch.request_id = RequestId(id("req", 'B'));
    successor.message_id = ExecutionMessageId(id("msg", 'C'));
    successor.request_id = RequestId(id("req", 'C'));
    successor.lease = successor.dispatch.lease.clone();
    let successor_now = Instant("2027-01-15T08:01:02.000Z".into());
    successor.sent_at = successor_now.clone();
    successor.dispatch.sent_at = successor_now.clone();
    let mut config = worker_config(1);
    config.worker_instance_id = successor.lease.worker_instance_id.clone();
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::default();
    let calls = codex.clone();
    let mut worker = WorkerMain::new(
        config,
        port,
        codex,
        JobWorkspaceRuntime::open(&workspace_root, &source_root).unwrap(),
    );
    worker.start(now()).await.unwrap();
    let result = WorkerRegistrationResultMessage {
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
        worker_instance_id: WorkerInstanceId(id("wki", 'B')),
    };
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::WorkerRegistrationResultMessage(result),
            now(),
        )
        .await
        .unwrap();
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::SnapshotFreezeRequestMessage(successor.clone()),
            successor_now,
        )
        .await
        .expect("sealed replacement freezes the retained candidate without a model call");
    let response = messages
        .borrow()
        .iter()
        .find_map(|message| match message {
            ExecutionPortMessage::SnapshotFreezeReceiptMessage(receipt) => Some(receipt.clone()),
            _ => None,
        })
        .unwrap();
    validate_freeze_receipt(&successor, &response).unwrap();
    assert_eq!(
        response.receipt.candidate_commit_id,
        responses[0].receipt.candidate_commit_id
    );
    assert!(
        !calls
            .calls()
            .iter()
            .any(|call| call.starts_with("ensure:") || call.starts_with("submit:"))
    );
    let manifest_path = std::fs::read_dir(&workspace_root)
        .unwrap()
        .map(|entry| entry.unwrap().path().join(".winwincode-workspace.json"))
        .find(|path| path.exists())
        .unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
    assert_eq!(
        manifest["snapshotFreezeHistory"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        manifest["snapshotFreezeHistory"][0]["request"],
        serde_json::to_value(&request).unwrap()
    );
    assert_eq!(
        manifest["snapshotFreezeHistory"][0]["response"],
        serde_json::to_value(&responses[0]).unwrap()
    );
    assert!(manifest["snapshotFreezeHistory"][0]["snapshot"].is_object());
    assert_eq!(
        manifest["snapshotFreeze"]["response"],
        serde_json::to_value(&response).unwrap()
    );
}

#[tokio::test]
async fn mismatched_work_run_input_is_rejected_before_workspace_or_codex() {
    let (workspace_root, source_root) = test_workspace_paths();
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let calls = codex.clone();
    let mut worker = WorkerMain::new(
        worker_config(1),
        port,
        codex,
        JobWorkspaceRuntime::open(&workspace_root, &source_root)
            .expect("open fixture workspace runtime"),
    );
    register(&mut worker).await;

    let mut dispatch = dispatch('A', delivery_scope('A'));
    let ExecutionScope::WorkRunExecutionScope(scope) = &mut dispatch.job.scope else {
        unreachable!("delivery fixture is a WorkRun")
    };
    scope.work_item_id = WorkItemId("wit_foreign_000000000000000000000".to_owned());
    worker
        .accept_control_and_drive(&ExecutionPortMessage::JobDispatchMessage(dispatch), now())
        .await
        .expect("invalid WorkRun input returns a rejection result");

    assert!(worker.active_jobs().is_empty());
    assert!(
        std::fs::read_dir(&workspace_root)
            .expect("read workspace root")
            .next()
            .transpose()
            .expect("read workspace entry")
            .is_none(),
        "invalid WorkRun input must not create a checkout, manifest, or owner lock"
    );
    assert!(
        calls
            .calls()
            .iter()
            .all(|call| !call.starts_with("ensure:"))
    );
    assert!(
        calls
            .calls()
            .iter()
            .all(|call| !call.starts_with("submit:"))
    );
    assert!(messages.borrow().iter().any(|message| matches!(
        message,
        ExecutionPortMessage::JobDispatchResultMessage(result)
            if result.status == JobDispatchResultMessageStatus::RejectedCapability
                && result.error.as_ref().is_some_and(|error| {
                    error.code == winwincode_execution_port::generated::ExecutionPortErrorCode::CapabilityMismatch
                        && !error.retryable
                })
    )));
}

#[tokio::test]
async fn duplicate_or_conflicting_dispatch_never_creates_a_second_thread() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A'), thread('A')]);
    let calls = codex.clone();
    let mut worker = test_worker(worker_config(3), port, codex);
    register(&mut worker).await;
    let first = dispatch('A', delivery_scope('A'));
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(first.clone()),
            now(),
        )
        .await
        .unwrap();
    let handoff = worker
        .recovery_handoff_snapshot(&first.job.job_id)
        .expect("active task handoff snapshot");
    assert!(!handoff.canonical_fact);
    assert_eq!(handoff.current_task.title, "Fixture Delivery");
    assert!(handoff.progress.completed.is_empty());
    assert!(handoff.progress.current.is_empty());
    assert_eq!(handoff.progress.remaining.len(), 1);
    assert!(!handoff.git_head.is_empty());
    assert_eq!(handoff.dirty.total_entries, 0);
    assert!(handoff.last_test.is_none());
    worker
        .accept_control_and_drive(&ExecutionPortMessage::JobDispatchMessage(first), now())
        .await
        .unwrap();
    let second = dispatch('B', delivery_scope('B'));
    worker
        .accept_control_and_drive(&ExecutionPortMessage::JobDispatchMessage(second), now())
        .await
        .unwrap();

    assert_eq!(worker.active_jobs().len(), 1);
    assert_eq!(
        calls
            .calls()
            .iter()
            .filter(|call| call.starts_with("ensure:"))
            .count(),
        2
    );
    assert_eq!(
        calls
            .calls()
            .iter()
            .filter(|call| call.starts_with("submit:"))
            .count(),
        1
    );
    assert!(
        calls
            .calls()
            .iter()
            .any(|call| call == &format!("close:{}", thread('A').0))
    );
    assert!(messages.borrow().iter().any(|message| matches!(
        message,
        ExecutionPortMessage::JobDispatchResultMessage(result)
            if result.job_id == ExecutionJobId(id("job", 'B'))
                && result.status == JobDispatchResultMessageStatus::Conflict
    )));
}

#[tokio::test]
async fn same_run_changed_job_fields_are_conflicts_before_codex_submission() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let calls = codex.clone();
    let mut worker = test_worker(worker_config(2), port, codex);
    register(&mut worker).await;

    let original = dispatch('A', delivery_scope('A'));
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(original.clone()),
            now(),
        )
        .await
        .expect("first dispatch should be accepted");

    let mut changed_jobs = Vec::new();
    let mut changed_goal = original.job.clone();
    changed_goal.goal.push_str(" changed");
    changed_jobs.push(changed_goal);
    let mut changed_profile = original.job.clone();
    changed_profile.execution_profile = "reviewer".to_owned();
    changed_jobs.push(changed_profile);
    let mut changed_limits = original.job.clone();
    changed_limits.limits.max_runtime_seconds = changed_limits
        .limits
        .max_runtime_seconds
        .map(|seconds| seconds - 1);
    changed_jobs.push(changed_limits);
    let mut changed_workspace = original.job.clone();
    changed_workspace.workspace.checkout_revision =
        "1123456789abcdef0123456789abcdef01234567".to_owned();
    changed_jobs.push(changed_workspace);

    for job in changed_jobs {
        assert_eq!(job.payload_digest, original.job.payload_digest);
        let mut replay = original.clone();
        replay.job = job;
        worker
            .accept_control_and_drive(&ExecutionPortMessage::JobDispatchMessage(replay), now())
            .await
            .expect("changed same-run dispatch should return a conflict result");
    }

    let statuses = messages
        .borrow()
        .iter()
        .filter_map(|message| match message {
            ExecutionPortMessage::JobDispatchResultMessage(result) => Some(result.status.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        [
            JobDispatchResultMessageStatus::Accepted,
            JobDispatchResultMessageStatus::Conflict,
            JobDispatchResultMessageStatus::Conflict,
            JobDispatchResultMessageStatus::Conflict,
            JobDispatchResultMessageStatus::Conflict,
        ]
    );
    assert_eq!(
        calls
            .calls()
            .iter()
            .filter(|call| call.starts_with("ensure:"))
            .count(),
        1
    );
    assert_eq!(
        calls
            .calls()
            .iter()
            .filter(|call| call.starts_with("submit:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn worker_sessions_and_workspaces_survive_reverse_job_recovery_order() {
    let (workspaces, sources) = test_workspace_paths();
    let first_codex = FakeCodex::with_threads([thread('A'), thread('B')]);
    let first_observer = first_codex.clone();
    let mut first = WorkerMain::new(
        worker_config(2),
        RecordingPort::default(),
        first_codex,
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("first workspace runtime"),
    );
    register(&mut first).await;
    for suffix in ['A', 'B'] {
        first
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch(suffix, delivery_scope(suffix))),
                now(),
            )
            .await
            .expect("first dispatch");
    }
    let original = first
        .active_jobs()
        .iter()
        .map(|active| {
            (
                active.job.job_id.0.clone(),
                (
                    active.worker_session_id.clone(),
                    active.codex_thread_id.clone(),
                    first_observer.workspace(&active.codex_thread_id),
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    drop(first);

    let restarted_codex = FakeCodex::with_threads([thread('B'), thread('A')]);
    let restarted_observer = restarted_codex.clone();
    let mut restarted = WorkerMain::new(
        worker_config(2),
        RecordingPort::default(),
        restarted_codex,
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("restarted workspace runtime"),
    );
    register(&mut restarted).await;
    for suffix in ['B', 'A'] {
        restarted
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch(suffix, delivery_scope(suffix))),
                now(),
            )
            .await
            .expect("reversed recovery dispatch");
    }

    for active in restarted.active_jobs() {
        let expected = original
            .get(&active.job.job_id.0)
            .expect("original exact Job authority");
        assert_eq!(&active.worker_session_id, &expected.0);
        assert_eq!(&active.codex_thread_id, &expected.1);
        assert_eq!(
            restarted_observer.workspace(&active.codex_thread_id),
            expected.2
        );
    }
    restarted
        .shutdown(now())
        .await
        .expect("terminally clean recovered workspaces");
}

#[tokio::test]
async fn sealed_replacement_reuses_the_writer_checkout_and_auto_emits_one_candidate() {
    let (workspaces, sources) = test_workspace_paths();
    let first_port = RecordingPort::default();
    let first_messages = Rc::clone(&first_port.messages);
    let first_codex = FakeCodex::with_threads([thread('A')]);
    let first_observer = first_codex.clone();
    let mut first = WorkerMain::new(
        worker_config(1),
        first_port,
        first_codex,
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("first workspace runtime"),
    );
    register(&mut first).await;
    first
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .expect("predecessor writer dispatch");
    let predecessor = first.active_jobs()[0].clone();
    let predecessor_checkout = first_observer.workspace(&predecessor.codex_thread_id);
    std::fs::write(predecessor_checkout.join("candidate.txt"), b"replacement\n")
        .expect("write predecessor change");
    first_observer.queue_poll(
        &predecessor.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("predecessor writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    first
        .poll_codex_boxed()
        .await
        .expect("retain predecessor candidate before replacement");
    let original_candidate_frames = observed_candidate_messages(&first_messages);
    let original_artifact = observed_candidate_reference(&first_messages);
    let replacement = replacement_dispatch(&predecessor);
    let successor_thread = CodexRunKey::from_dispatch(&replacement)
        .canonical_thread_id()
        .expect("successor thread");
    let (_, successor_codex) = first.into_parts();
    successor_codex
        .state
        .lock()
        .expect("FakeCodex state")
        .threads
        .push_back(successor_thread.clone());

    let successor_observer = successor_codex.clone();
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let mut config = worker_config(1);
    config.worker_instance_id = WorkerInstanceId(id("wki", 'B'));
    let mut restarted = WorkerMain::new(
        config,
        port,
        successor_codex,
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("successor workspace runtime"),
    );
    restarted.start(now()).await.expect("register successor");
    restarted
        .accept_control_and_drive(
            &ExecutionPortMessage::WorkerRegistrationResultMessage(
                WorkerRegistrationResultMessage {
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
                    worker_instance_id: WorkerInstanceId(id("wki", 'B')),
                },
            ),
            now(),
        )
        .await
        .expect("accept successor registration");
    let replacement_proof = replacement
        .replacement_authority
        .clone()
        .expect("replacement proof");
    restarted
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(replacement),
            now(),
        )
        .await
        .expect("accept sealed successor dispatch");
    let successor = restarted.active_jobs()[0].clone();
    let message_snapshot = messages.borrow().clone();
    let successor_binding = message_snapshot
        .iter()
        .filter_map(|message| match message {
            ExecutionPortMessage::SessionBindingMessage(binding) => Some(binding),
            _ => None,
        })
        .next_back()
        .unwrap_or_else(|| panic!("successor session binding; messages={message_snapshot:?}"));
    assert_ne!(successor.worker_session_id, predecessor.worker_session_id);
    assert_ne!(successor.codex_thread_id, predecessor.codex_thread_id);
    assert_ne!(
        successor.session_identity.work_run_id,
        predecessor.session_identity.work_run_id
    );
    assert_eq!(
        successor_binding.work_run_id,
        successor.session_identity.work_run_id
    );
    assert_eq!(
        successor_binding.session_identity,
        successor.session_identity
    );
    assert_eq!(
        successor_binding.worker_session_id,
        successor.worker_session_id
    );
    assert_eq!(successor_binding.codex_thread_id, successor.codex_thread_id);
    assert_eq!(successor_binding.lease, successor.lease);
    assert_eq!(successor_binding.attempt, successor.job.attempt);
    assert_eq!(
        replacement_proof.predecessor_session_identity.as_ref(),
        Some(&predecessor.session_identity)
    );
    assert_eq!(
        successor_observer.workspace(&successor.codex_thread_id),
        predecessor_checkout
    );
    assert_eq!(
        std::fs::read(predecessor_checkout.join("candidate.txt"))
            .expect("read recovered predecessor change"),
        b"replacement\n"
    );
    successor_observer.queue_poll(
        &successor.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("replacement writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    restarted
        .poll_codex_boxed()
        .await
        .expect("resume predecessor candidate stream under replacement receipt");
    assert_eq!(
        observed_candidate_messages(&messages),
        original_candidate_frames
    );
    let artifact = observed_candidate_reference(&messages);
    assert_eq!(artifact, original_artifact);
    acknowledge_candidate(&mut restarted, &successor, &artifact, 0, 'O')
        .await
        .expect("ack replacement candidate open");
    acknowledge_candidate(&mut restarted, &successor, &artifact, 1, 'F')
        .await
        .expect("ack replacement candidate final");
    assert_eq!(observed_outcomes(&messages).len(), 1);
}

#[tokio::test]
async fn cancellation_is_session_scoped_and_interrupts_exactly_once() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let calls = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let mut wrong = cancel_for(&active, 'B');
    wrong.worker_session_id = WorkerSessionId(id("wsn", 'Z'));
    worker
        .accept_control_and_drive(&ExecutionPortMessage::JobCancelMessage(wrong), now())
        .await
        .unwrap();
    let exact = cancel_for(&active, 'C');
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobCancelMessage(exact.clone()),
            now(),
        )
        .await
        .unwrap();
    worker
        .accept_control_and_drive(&ExecutionPortMessage::JobCancelMessage(exact), now())
        .await
        .unwrap();

    assert_eq!(
        calls
            .calls()
            .iter()
            .filter(|call| call.starts_with("interrupt:"))
            .count(),
        1
    );
    let statuses = messages
        .borrow()
        .iter()
        .filter_map(|message| match message {
            ExecutionPortMessage::JobCancelAckMessage(ack) => Some(ack.status.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        [
            JobCancelAckMessageStatus::RejectedWorkerInstance,
            JobCancelAckMessageStatus::Accepted,
            JobCancelAckMessageStatus::AlreadyCancelling,
        ]
    );
}

#[tokio::test]
async fn retained_trace_and_terminal_outcome_preserve_exact_session_order() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let trace = RuntimeEventMessage {
        codex_thread_id: active.codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Lifecycle,
            event_id: ExecutionEventId(id("evt", 'A')),
            occurred_at: now(),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "Codex turn started".to_owned(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: active.lease.clone(),
        message_id: ExecutionMessageId(id("msg", 'T')),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        session_identity: active.session_identity.clone(),
        worker_session_id: active.worker_session_id.clone(),
    };
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::RuntimeTrace(Box::new(trace))),
    );
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("Codex turn completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );

    worker.poll_codex_boxed().await.unwrap();
    worker.poll_codex_boxed().await.unwrap();

    let captured = messages.borrow();
    let tail = output_kinds(&captured)[captured.len() - 2..].to_vec();
    assert_eq!(tail, ["runtime", "outcome"]);
    assert!(worker.active_jobs().is_empty());
    assert!(captured.iter().any(|message| matches!(
        message,
        ExecutionPortMessage::JobOutcomeMessage(outcome)
            if outcome.outcome.last_event_sequence.0 == 1
                && outcome.outcome.codex_thread_id == Some(thread('A'))
                && outcome.outcome.usage == Some(measured_completion_usage())
    )));
}

const DELEGATED_PATCH: &str =
    "*** Begin Patch\n*** Add File: delegated.txt\n+fixture\n*** End Patch\n";

fn delegated_identity(
    active: &winwincode_worker::ActiveJob,
    workspace_revision: WorkspaceRevision,
) -> ChangeBatchIdentity {
    let run_key = CodexRunKey {
        job_id: active.job.job_id.clone(),
        attempt: active.job.attempt,
        fencing_token: active.lease.fencing_token.clone(),
        payload_digest: active.job.payload_digest.clone(),
    }
    .canonical_digest()
    .expect("canonical delegated run key")
    .0;
    let patch_digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(DELEGATED_PATCH.as_bytes())
    ));
    ChangeBatchIdentity {
        attempt: active.job.attempt,
        batch_id: derive_change_batch_id(&run_key, "turn-fixture", None, &patch_digest)
            .expect("canonical delegated batch id"),
        call_id: None,
        fencing_token: active.lease.fencing_token.clone(),
        job_id: active.job.job_id.clone(),
        lease_id: active.lease.lease_id.clone(),
        patch_digest,
        repository_id: active.job.workspace.repository_id.clone(),
        run_key,
        session_identity: active.session_identity.clone(),
        turn_id: "turn-fixture".to_owned(),
        workspace_revision,
    }
}

fn delegated_progress(
    identity: ChangeBatchIdentity,
    sequence: i64,
    state: ChangeBatchProgressState,
) -> ChangeBatchProgressEvent {
    ChangeBatchProgressEvent {
        artifact_refs: Vec::new(),
        identity,
        occurred_at: now(),
        sequence,
        state,
        summary: "bounded change batch progress".to_owned(),
    }
}

#[tokio::test]
async fn delegated_proposal_is_executed_once_and_job_remains_active() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let workspaces = test_workspaces().with_change_batch_executor(AppliedBatchExecutor {
        calls: Arc::clone(&calls),
    });
    let mut worker = WorkerMain::new(worker_config(1), port, codex, workspaces);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProposed(Box::new(
            ChangeBatchProposalEvent {
                identity: identity.clone(),
                occurred_at: now(),
                proposal: ChangeBatchProposal {
                    acceptance_criteria_ids: vec!["crt_00000000000000000000000001".to_owned()],
                    disposition: ChangeBatchProposalDisposition::Final,
                    patch: DELEGATED_PATCH.to_owned(),
                    schema_version: 1,
                    validation_profile: ValidationProfileName::Changed,
                },
            },
        ))),
    );
    worker.poll_codex_boxed().await.unwrap();
    assert_eq!(worker.active_jobs().len(), 1);
    assert!(observed_outcomes(&messages).is_empty());
    assert_eq!(*calls.lock().expect("batch calls"), vec!["execute"]);
    let outcomes = worker.take_delegated_poll_outcomes();
    assert!(
        matches!(
            outcomes.as_slice(),
            [
                DelegatedPollOutcome::ChangeBatchProposed(_),
                DelegatedPollOutcome::ChangeBatchProgress(proposed),
                DelegatedPollOutcome::ChangeBatchProgress(authorized),
                DelegatedPollOutcome::ChangeBatchProgress(started),
                DelegatedPollOutcome::ChangeBatchProgress(applied),
                DelegatedPollOutcome::ChangeBatchReceipt(_)
            ]
            if proposed.state == ChangeBatchProgressState::Proposed
                && authorized.state == ChangeBatchProgressState::Authorized
                && started.state == ChangeBatchProgressState::ApplyStarted
                && applied.state == ChangeBatchProgressState::Applied
        ),
        "unexpected delegated outcomes: {outcomes:#?}"
    );
    assert!(worker.take_delegated_poll_outcomes().is_empty());
}

#[tokio::test]
async fn accepted_final_delegated_batch_freezes_without_another_primary_turn() {
    let (workspaces_root, sources) = test_workspace_paths();
    let repository = sources.join(id("rpo", 'A'));
    std::fs::create_dir_all(repository.join(".winwincode"))
        .expect("create passing validation config directory");
    std::fs::write(
        repository.join(".winwincode/validation.toml"),
        PASSING_VALIDATION_CONFIG,
    )
    .expect("write passing validation config");
    run_git(&repository, &["add", ".winwincode/validation.toml"]);
    run_git(
        &repository,
        &[
            "-c",
            "user.name=WinWinCode Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "Passing validation config",
        ],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let artifact_store = DurableValidationArtifactStore::open(
        workspaces_root
            .parent()
            .expect("delegated final fixture root")
            .join("validation-artifacts"),
    )
    .expect("open delegated final validation Artifact store");
    let workspaces = JobWorkspaceRuntime::open(workspaces_root, sources)
        .expect("open delegated final workspace runtime")
        .with_validation_artifact_port(artifact_store)
        .with_change_batch_executor(AppliedBatchExecutor {
            calls: Arc::clone(&calls),
        });
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = WorkerMain::new(worker_config(1), port, codex, workspaces);
    register(&mut worker).await;
    let mut delegated = writer_dispatch('A');
    delegated.job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
    worker
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(delegated), now())
        .await
        .expect("dispatch delegated writer");
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProposed(Box::new(
            ChangeBatchProposalEvent {
                identity,
                occurred_at: now(),
                proposal: ChangeBatchProposal {
                    acceptance_criteria_ids: vec!["crt_00000000000000000000000001".to_owned()],
                    disposition: ChangeBatchProposalDisposition::Final,
                    patch: DELEGATED_PATCH.to_owned(),
                    schema_version: 1,
                    validation_profile: ValidationProfileName::Changed,
                },
            },
        ))),
    );

    worker
        .poll_codex_boxed()
        .await
        .expect("accept, validate and freeze delegated final batch");
    assert_eq!(*calls.lock().expect("batch calls"), vec!["execute"]);
    assert_eq!(
        pump.calls()
            .iter()
            .filter(|call| call.starts_with("submit:"))
            .count(),
        1,
        "accepted Final must not submit another Primary Model turn"
    );
    assert_eq!(observed_candidate_messages(&messages).len(), 2);
    assert_no_outcome(&messages);
    let artifact = observed_candidate_reference(&messages);
    acknowledge_candidate(&mut worker, &active, &artifact, 0, 'O')
        .await
        .expect("acknowledge delegated candidate open");
    acknowledge_candidate(&mut worker, &active, &artifact, 1, 'F')
        .await
        .expect("acknowledge delegated candidate final chunk");
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].outcome.status,
        ExecutionOutcomeStatus::Succeeded
    );
    assert_eq!(outcomes[0].outcome.artifacts, vec![artifact]);
    assert_eq!(
        outcomes[0].outcome.usage,
        Some(ExecutionOutcomeUsage::unknown(0, 0))
    );
    assert!(outcomes[0].outcome.error.is_none());
    let freezes = pump.final_freezes();
    assert_eq!(freezes.len(), 1);
    assert!(freezes[0].final_observation.is_none());
    assert_eq!(freezes[0].counters.change_batches, 1);
    assert!(freezes[0].counters.context_pack_bytes > 0);
    assert!(worker.active_jobs().is_empty());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn delegated_freeze_before_persist_restarts_from_one_accepted_candidate() {
    let (workspaces_root, sources) = test_workspace_paths();
    let repository = sources.join(id("rpo", 'A'));
    std::fs::create_dir_all(repository.join(".winwincode"))
        .expect("create passing validation config directory");
    std::fs::write(
        repository.join(".winwincode/validation.toml"),
        PASSING_VALIDATION_CONFIG,
    )
    .expect("write passing validation config");
    run_git(&repository, &["add", ".winwincode/validation.toml"]);
    run_git(
        &repository,
        &[
            "-c",
            "user.name=WinWinCode Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "Passing validation config",
        ],
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let artifact_root = workspaces_root
        .parent()
        .expect("delegated freeze fixture root")
        .join("validation-artifacts");
    let workspaces = JobWorkspaceRuntime::open(&workspaces_root, &sources)
        .expect("open delegated freeze workspace runtime")
        .with_validation_artifact_port(
            DurableValidationArtifactStore::open(&artifact_root)
                .expect("open delegated freeze Artifact store"),
        )
        .with_change_batch_executor(AppliedBatchExecutor {
            calls: Arc::clone(&calls),
        });
    let first_port = RecordingPort::default();
    let first_messages = Rc::clone(&first_port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = WorkerMain::new(worker_config(1), first_port, codex, workspaces);
    register(&mut worker).await;
    let mut dispatch = writer_dispatch('A');
    dispatch.job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            now(),
        )
        .await
        .expect("dispatch delegated freeze fixture");
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProposed(Box::new(
            ChangeBatchProposalEvent {
                identity,
                occurred_at: now(),
                proposal: ChangeBatchProposal {
                    acceptance_criteria_ids: vec!["crt_00000000000000000000000001".to_owned()],
                    disposition: ChangeBatchProposalDisposition::Final,
                    patch: DELEGATED_PATCH.to_owned(),
                    schema_version: 1,
                    validation_profile: ValidationProfileName::Changed,
                },
            },
        ))),
    );
    worker
        .poll_codex_boxed()
        .await
        .expect("retain one accepted candidate");
    let artifact = observed_candidate_reference(&first_messages);
    acknowledge_candidate(&mut worker, &active, &artifact, 0, 'O')
        .await
        .expect("acknowledge delegated candidate open");
    worker.inject_final_freeze_fault(winwincode_worker::WorkerFinalFreezeFault::BeforePersist);
    acknowledge_candidate(&mut worker, &active, &artifact, 1, 'F')
        .await
        .expect_err("stop after candidate acceptance and before freeze persistence");
    assert!(pump.final_freezes().is_empty());
    assert_no_outcome(&first_messages);
    assert_eq!(*calls.lock().expect("batch calls"), vec!["execute"]);

    let (_, recovered_codex) = worker.into_parts();
    recovered_codex
        .state
        .lock()
        .expect("FakeCodex state")
        .threads
        .push_back(thread('A'));
    let restart_port = RecordingPort::default();
    let restart_messages = Rc::clone(&restart_port.messages);
    let recovered_workspaces = JobWorkspaceRuntime::open(&workspaces_root, &sources)
        .expect("reopen delegated freeze workspace runtime")
        .with_validation_artifact_port(
            DurableValidationArtifactStore::open(&artifact_root)
                .expect("reopen delegated freeze Artifact store"),
        )
        .with_change_batch_executor(AppliedBatchExecutor {
            calls: Arc::clone(&calls),
        });
    let mut restarted = WorkerMain::new(
        worker_config(1),
        restart_port,
        recovered_codex,
        recovered_workspaces,
    );
    register(&mut restarted).await;
    restarted
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(dispatch), now())
        .await
        .expect("recover exact delegated freeze dispatch");
    restarted
        .poll_codex_boxed()
        .await
        .expect("recover accepted candidate and persist one freeze");
    assert_eq!(observed_candidate_messages(&restart_messages).len(), 0);
    let outcomes = observed_outcomes(&restart_messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].outcome.status,
        ExecutionOutcomeStatus::Succeeded
    );
    assert_eq!(outcomes[0].outcome.artifacts, vec![artifact]);
    assert_eq!(pump.final_freezes().len(), 1);
    assert_eq!(*calls.lock().expect("batch calls"), vec!["execute"]);
}

struct ObserverWorkerFixture {
    worker: WorkerMain<RecordingPort, FakeCodex>,
    pump: FakeCodex,
    messages: Rc<RefCell<Vec<ExecutionPortMessage>>>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    port_failures: Rc<Cell<usize>>,
}

async fn observer_worker_fixture() -> ObserverWorkerFixture {
    let (workspaces_root, sources) = test_workspace_paths();
    let repository = sources.join(id("rpo", 'A'));
    std::fs::create_dir_all(repository.join(".winwincode"))
        .expect("create Observer validation config directory");
    std::fs::write(
        repository.join(".winwincode/validation.toml"),
        OBSERVER_VALIDATION_CONFIG,
    )
    .expect("write Observer validation config");
    run_git(&repository, &["add", ".winwincode/validation.toml"]);
    run_git(
        &repository,
        &[
            "-c",
            "user.name=WinWinCode Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "Observer validation config",
        ],
    );
    let artifact_store = DurableValidationArtifactStore::open(
        workspaces_root
            .parent()
            .expect("fixture root")
            .join("validation-artifacts"),
    )
    .expect("open validation Artifact store");
    let workspaces = JobWorkspaceRuntime::open(workspaces_root, sources)
        .expect("open Observer workspace runtime")
        .with_validation_artifact_port(artifact_store);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let workspaces = workspaces.with_change_batch_executor(AppliedBatchExecutor {
        calls: Arc::clone(&calls),
    });
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let port_failures = Rc::clone(&port.failures_remaining);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = WorkerMain::new(worker_config(1), port, codex, workspaces)
        .with_observer_mode(winwincode_codex::ObserverMode::AmbiguousOnly)
        .with_observation_model(
            ObservationModelConfiguration::try_new(
                "observer-provider",
                "observer-model",
                ModelGatewayRoute {
                    capability: "observer-strict-json".to_owned(),
                    route: "enterprise-observer".to_owned(),
                },
            )
            .expect("independent Observer route"),
        );
    register(&mut worker).await;
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .expect("dispatch Observer fixture");
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProposed(Box::new(
            ChangeBatchProposalEvent {
                identity,
                occurred_at: now(),
                proposal: ChangeBatchProposal {
                    acceptance_criteria_ids: vec!["crt_00000000000000000000000001".to_owned()],
                    disposition: ChangeBatchProposalDisposition::Final,
                    patch: DELEGATED_PATCH.to_owned(),
                    schema_version: 1,
                    validation_profile: ValidationProfileName::Changed,
                },
            },
        ))),
    );

    ObserverWorkerFixture {
        worker,
        pump,
        messages,
        calls,
        port_failures,
    }
}

#[tokio::test]
async fn unresolved_validation_retains_one_observer_open_before_sending_it() {
    let ObserverWorkerFixture {
        mut worker,
        pump,
        messages,
        calls,
        port_failures,
    } = observer_worker_fixture().await;
    let active = worker.active_jobs()[0].clone();

    worker
        .poll_codex_boxed()
        .await
        .expect("execute unresolved validation");
    let outcomes = worker.take_delegated_poll_outcomes();
    assert!(outcomes.iter().any(|outcome| matches!(
        outcome,
        DelegatedPollOutcome::ChangeBatchProgress(progress)
            if progress.state == ChangeBatchProgressState::ObservationRequested
    )));
    assert_eq!(
        messages
            .borrow()
            .iter()
            .filter(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
            .count(),
        1,
        "the durable one-shot intent produces one Provider open"
    );
    worker
        .poll_codex_boxed()
        .await
        .expect("pending Codex poll does not resend the open");
    assert_eq!(
        messages
            .borrow()
            .iter()
            .filter(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
            .count(),
        1,
        "the same process never starts a second Provider call"
    );
    assert_eq!(*calls.lock().expect("batch calls"), vec!["execute"]);
    assert_eq!(worker.active_jobs().len(), 1);

    port_failures.set(1);
    let cancel = cancel_for(&active, 'C');
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobCancelMessage(cancel.clone()),
            now(),
        )
        .await
        .expect_err("first Observer cancel acknowledgement send fails");
    worker
        .accept_control_and_drive(&ExecutionPortMessage::JobCancelMessage(cancel), now())
        .await
        .expect("AlreadyCancelling replays the durable Observer cancellation");
    assert_eq!(
        messages
            .borrow()
            .iter()
            .filter(|message| matches!(message, ExecutionPortMessage::ModelAckMessage(_)))
            .count(),
        1,
        "failed cancellation delivery is retried with one retained acknowledgement"
    );
    assert_eq!(
        pump.calls()
            .iter()
            .filter(|call| call.starts_with("interrupt:"))
            .count(),
        1,
        "the first cancellation interrupts Codex before a transport retry"
    );
}

#[tokio::test]
async fn observer_renewal_control_accepts_original_chunks_without_error_ack() {
    use base64::Engine as _;
    let ObserverWorkerFixture {
        mut worker,
        messages,
        ..
    } = observer_worker_fixture().await;
    worker.poll_codex_boxed().await.unwrap();
    let open = messages
        .borrow()
        .iter()
        .find_map(|message| match message {
            ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
            _ => None,
        })
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let mut lease = active.lease.clone();
    lease.expires_at = Instant("2027-01-15T08:10:00.000Z".into());
    worker
        .accept_control(
            &ExecutionPortMessage::LeaseRenewMessage(
                winwincode_execution_port::generated::LeaseRenewMessage {
                    kind: winwincode_execution_port::generated::LeaseRenewMessageKind::LeaseRenew,
                    schema_version: SchemaVersion::WinwincodeV1,
                    message_id: ExecutionMessageId(id("xmsg", 'R')),
                    sent_at: now(),
                    request_id: RequestId(id("req", 'R')),
                    lease,
                    prior_expires_at: active.lease.expires_at.clone(),
                },
            ),
            now(),
        )
        .await
        .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&open.request.data_base64)
        .unwrap();
    let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let observation: serde_json::Value = serde_json::from_str(
        request["request"]["input"][1]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let response = serde_json::json!({
        "schemaVersion": 1, "observationId": observation["intent"]["observationId"],
        "decision": "accept", "reasonCode": "criteria_satisfied",
        "summary": "The evidence satisfies the requested criterion.",
        "rootCauses": [], "repairClass": null, "confidenceBps": 9000
    })
    .to_string();
    let payloads = [
        serde_json::json!({"type":"output_text_delta", "delta": response}),
        serde_json::json!({"type":"completed", "responseId":"observer-renewal-fixture", "endTurn":true}),
    ];
    worker.take_delegated_poll_outcomes();
    let mut terminal = None;
    for (index, payload) in payloads.into_iter().enumerate() {
        let ExecutionPortMessage::ModelChunkMessage(mut chunk) =
            execution_port_fixture("model.chunk")
        else {
            unreachable!()
        };
        let bytes = serde_json::to_vec(&payload).unwrap();
        chunk.error = None;
        chunk.is_final = index == 1;
        chunk.lease = open.lease.clone();
        chunk.worker_session_id = open.worker_session_id.clone();
        chunk.session_identity = open.session_identity.clone();
        chunk.model_exchange_id = open.model_exchange_id.clone();
        chunk.message_id = ExecutionMessageId(id("xmsg", if index == 0 { 'D' } else { 'T' }));
        chunk.sequence = ExecutionSequence(i64::try_from(index + 1).unwrap());
        chunk.payload = Some(EncodedPayload {
            content_type: "application/json".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes))),
        });
        terminal = Some(chunk.clone());
        worker
            .accept_control_and_drive(
                &ExecutionPortMessage::ModelChunkMessage(chunk),
                Instant("2027-01-15T08:06:00.000Z".into()),
            )
            .await
            .unwrap();
    }
    let decisions = worker.take_delegated_poll_outcomes();
    assert_eq!(
        decisions
            .iter()
            .filter(|outcome| matches!(outcome,
        DelegatedPollOutcome::ChangeBatchProgress(progress)
            if progress.state == ChangeBatchProgressState::ObservationCompleted))
            .count(),
        1
    );
    let receipt = decisions
        .iter()
        .find_map(|outcome| match outcome {
            DelegatedPollOutcome::ChangeBatchReceipt(receipt) => receipt.observation.as_ref(),
            _ => None,
        })
        .unwrap();
    let usage = receipt.model_usage.as_ref().unwrap();
    assert_eq!(
        receipt.source,
        winwincode_execution_port::generated::ObservationSource::Model
    );
    assert_eq!(
        usage.accounting_status,
        winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Unknown
    );
    assert_eq!(usage.tokens, None);
    assert_eq!(usage.cost_microunits, None);
    assert!(
        messages
            .borrow()
            .iter()
            .filter_map(|message| match message {
                ExecutionPortMessage::ModelAckMessage(ack) => Some(ack),
                _ => None,
            })
            .all(|ack| ack.error.is_none()),
        "legal renewal must never send an error/cancel ACK"
    );
    for timestamp in ["2027-01-15T08:06:01.000Z", "2027-01-15T08:06:02.000Z"] {
        worker
            .accept_control_and_drive(
                &ExecutionPortMessage::ModelChunkMessage(terminal.clone().unwrap()),
                Instant(timestamp.into()),
            )
            .await
            .unwrap();
        assert!(
            worker
                .take_delegated_poll_outcomes()
                .iter()
                .all(|outcome| !matches!(outcome,
            DelegatedPollOutcome::ChangeBatchProgress(progress)
                if progress.state == ChangeBatchProgressState::ObservationCompleted))
        );
    }
}

#[tokio::test]
async fn deferred_observer_start_uses_common_outbox_after_renewal() {
    let ObserverWorkerFixture { worker, pump, .. } = observer_worker_fixture().await;
    let root = tempfile::tempdir().unwrap();
    let providers = root.path().join("providers");
    let mut worker = worker.with_device_providers(&providers).unwrap();
    assert!(worker.inject_device_start_fault());
    assert_eq!(
        worker.poll_codex_boxed().await.unwrap_err().code,
        WorkerErrorCode::ModelStartDeferred
    );
    worker
        .flush_durable_outbox()
        .await
        .expect("finish the deferred scan before retrying its original intent");
    let original = pump.state.lock().unwrap().model_start_requests[0].clone();
    assert!(
        pump.state
            .lock()
            .unwrap()
            .durable_deliveries
            .iter()
            .any(|delivery| delivery.message
                == ExecutionPortMessage::ModelOpenMessage(original.clone())),
        "Observer adopts its journaled request into the shared durable outbox"
    );
    let active = worker.active_jobs()[0].clone();
    let mut lease = active.lease.clone();
    lease.expires_at = Instant("2027-01-15T08:10:00.000Z".into());
    worker
        .accept_control(
            &ExecutionPortMessage::LeaseRenewMessage(
                winwincode_execution_port::generated::LeaseRenewMessage {
                    kind: winwincode_execution_port::generated::LeaseRenewMessageKind::LeaseRenew,
                    schema_version: SchemaVersion::WinwincodeV1,
                    message_id: ExecutionMessageId(id("xmsg", 'R')),
                    sent_at: now(),
                    request_id: RequestId(id("req", 'R')),
                    lease,
                    prior_expires_at: active.lease.expires_at.clone(),
                },
            ),
            now(),
        )
        .await
        .unwrap();
    let later = Instant("2027-01-15T08:06:00.000Z".into());
    Box::pin(worker.poll_codex(later.clone())).await.unwrap();
    let store = winwincode_provider::DeviceProviderStore::open(&providers).unwrap();
    for _ in 0..100 {
        if !store.list_stored_model_exchanges().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        store.list_stored_model_exchanges().unwrap(),
        vec![original.model_exchange_id.0.clone()]
    );
    {
        let state = pump.state.lock().unwrap();
        assert!(state.model_start_requests.len() >= 2);
        assert!(
            state
                .model_start_requests
                .iter()
                .all(|proof| proof == &original),
            "renewal preserves exact original request proof at every local handoff"
        );
    }

    // No Provider is configured: the exact started exchange ends with a durable
    // configuration error, exercising result restoration without any API charge.
    for _ in 0..100 {
        if !store
            .replay_model(&original.model_exchange_id.0, 1)
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        !store
            .replay_model(&original.model_exchange_id.0, 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.list_stored_model_exchanges().unwrap().len(), 1);
    drop(worker);
}

#[tokio::test]
async fn credential_shaped_observation_input_retains_no_model_open() {
    let (workspaces_root, sources) = test_workspace_paths();
    let fixture_root = workspaces_root
        .parent()
        .expect("fixture root")
        .to_path_buf();
    let repository = sources.join(id("rpo", 'A'));
    let secret = format!("{}{}{}", "github_", "pat_", "A".repeat(20));
    std::fs::create_dir_all(repository.join(".winwincode"))
        .expect("create secret-scan validation config directory");
    std::fs::write(
        repository.join(".winwincode/validation.toml"),
        OBSERVER_VALIDATION_CONFIG,
    )
    .expect("write secret-scan validation config");
    run_git(&repository, &["add", ".winwincode/validation.toml"]);
    run_git(
        &repository,
        &[
            "-c",
            "user.name=WinWinCode Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "Secret scan validation config",
        ],
    );
    let artifact_store =
        DurableValidationArtifactStore::open(fixture_root.join("validation-artifacts"))
            .expect("open validation Artifact store");
    let workspaces = JobWorkspaceRuntime::open(workspaces_root, sources)
        .expect("open secret-scan workspace runtime")
        .with_validation_artifact_port(artifact_store)
        .with_change_batch_executor(AppliedBatchExecutor {
            calls: Arc::new(Mutex::new(Vec::new())),
        });
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = WorkerMain::new(worker_config(1), port, codex, workspaces)
        .with_observer_mode(winwincode_codex::ObserverMode::AmbiguousOnly)
        .with_observation_model(
            ObservationModelConfiguration::try_new(
                "observer-provider",
                "observer-model",
                ModelGatewayRoute {
                    capability: "observer-strict-json".to_owned(),
                    route: "enterprise-observer".to_owned(),
                },
            )
            .expect("independent Observer route"),
        );
    register(&mut worker).await;
    let dispatch = delegated_task_dispatch('A');
    worker
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(dispatch), now())
        .await
        .expect("dispatch secret-scan fixture");
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProposed(Box::new(
            ChangeBatchProposalEvent {
                identity,
                occurred_at: now(),
                proposal: ChangeBatchProposal {
                    acceptance_criteria_ids: vec![secret.clone()],
                    disposition: ChangeBatchProposalDisposition::Final,
                    patch: DELEGATED_PATCH.to_owned(),
                    schema_version: 1,
                    validation_profile: ValidationProfileName::Changed,
                },
            },
        ))),
    );

    let rejection = worker
        .poll_codex_boxed()
        .await
        .expect_err("reject credential-shaped acceptance criterion identity");
    assert_eq!(rejection.code, WorkerErrorCode::DelegatedPollMismatch);
    assert!(
        messages
            .borrow()
            .iter()
            .all(|message| !matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
    );
    let journal_database = fixture_root
        .join(".workspaces-change-batches")
        .join("change-batch.sqlite3");
    let observation_count = rusqlite::Connection::open(journal_database)
        .expect("open secret-scan journal")
        .query_row("SELECT COUNT(*) FROM change_batch_observation", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count Observer intents");
    assert_eq!(observation_count, 0);
    let output = serde_json::to_string(&*messages.borrow()).expect("encode Worker output");
    assert!(!output.contains(&secret));
}

#[tokio::test]
async fn delegated_codex_poll_rejects_foreign_authority_before_return() {
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let workspaces = test_workspaces().with_change_batch_executor(AppliedBatchExecutor {
        calls: Arc::clone(&calls),
    });
    let mut worker = WorkerMain::new(
        worker_config(1),
        RecordingPort::default(),
        codex,
        workspaces,
    );
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let mut identity =
        delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    identity.repository_id = RepositoryId(id("repo", 'Z'));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProposed(Box::new(
            ChangeBatchProposalEvent {
                identity,
                occurred_at: now(),
                proposal: ChangeBatchProposal {
                    acceptance_criteria_ids: vec!["crt_00000000000000000000000001".to_owned()],
                    disposition: ChangeBatchProposalDisposition::Final,
                    patch: DELEGATED_PATCH.to_owned(),
                    schema_version: 1,
                    validation_profile: ValidationProfileName::Changed,
                },
            },
        ))),
    );

    let error = worker.poll_codex_boxed().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::DelegatedPollMismatch);
    assert!(worker.take_delegated_poll_outcomes().is_empty());
    assert!(calls.lock().expect("batch calls").is_empty());
}

#[tokio::test]
async fn delegated_poll_rejects_a_rederived_foreign_run_key() {
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let mut identity =
        delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    identity.run_key = format!("sha256:{}", "b".repeat(64));
    identity.batch_id = derive_change_batch_id(
        &identity.run_key,
        &identity.turn_id,
        identity.call_id.as_deref(),
        &identity.patch_digest,
    )
    .expect("rederive foreign batch identity");
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProgress(Box::new(
            delegated_progress(identity, 1, ChangeBatchProgressState::Proposed),
        ))),
    );

    let error = worker.poll_codex_boxed().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::DelegatedPollMismatch);
    assert!(worker.take_delegated_poll_outcomes().is_empty());
}

#[tokio::test]
async fn delegated_poll_rejects_a_noncanonical_batch_derivation() {
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let mut identity =
        delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    identity.batch_id = ChangeBatchId(format!("sha256:{}", "c".repeat(64)));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProgress(Box::new(
            delegated_progress(identity, 1, ChangeBatchProgressState::Proposed),
        ))),
    );

    let error = worker.poll_codex_boxed().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::DelegatedPollMismatch);
    assert!(worker.take_delegated_poll_outcomes().is_empty());
}

#[tokio::test]
async fn delegated_proposal_rejects_patch_bytes_outside_the_sealed_identity() {
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProposed(Box::new(
            ChangeBatchProposalEvent {
                identity,
                occurred_at: now(),
                proposal: ChangeBatchProposal {
                    acceptance_criteria_ids: vec!["crt_00000000000000000000000001".to_owned()],
                    disposition: ChangeBatchProposalDisposition::Final,
                    patch:
                        "*** Begin Patch\n*** Add File: delegated.txt\n+changed\n*** End Patch\n"
                            .to_owned(),
                    schema_version: 1,
                    validation_profile: ValidationProfileName::Changed,
                },
            },
        ))),
    );

    let error = worker.poll_codex_boxed().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::DelegatedPollMismatch);
    assert!(worker.take_delegated_poll_outcomes().is_empty());
}

#[tokio::test]
async fn delegated_progress_rejects_an_out_of_order_initial_sequence() {
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProgress(Box::new(
            delegated_progress(
                delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id)),
                2,
                ChangeBatchProgressState::Proposed,
            ),
        ))),
    );

    let error = worker.poll_codex_boxed().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::DelegatedPollMismatch);
    assert!(worker.take_delegated_poll_outcomes().is_empty());
}

#[tokio::test]
async fn delegated_progress_rejects_identity_change_within_one_batch() {
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProgress(Box::new(
            delegated_progress(identity.clone(), 1, ChangeBatchProgressState::Proposed),
        ))),
    );
    let mut changed = identity;
    changed.turn_id = "turn-changed".to_owned();
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProgress(Box::new(
            delegated_progress(changed, 2, ChangeBatchProgressState::Authorized),
        ))),
    );

    worker.poll_codex_boxed().await.unwrap();
    let error = worker.poll_codex_boxed().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::DelegatedPollMismatch);
    assert_eq!(worker.take_delegated_poll_outcomes().len(), 1);
}

#[tokio::test]
async fn delegated_progress_rejects_a_successor_after_terminal_state() {
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    for (sequence, state) in [
        (1, ChangeBatchProgressState::Proposed),
        (2, ChangeBatchProgressState::RepairRequired),
        (3, ChangeBatchProgressState::Authorized),
    ] {
        pump.queue_poll(
            &active.codex_thread_id,
            Ok(CodexPoll::ChangeBatchProgress(Box::new(
                delegated_progress(identity.clone(), sequence, state),
            ))),
        );
    }

    worker.poll_codex_boxed().await.unwrap();
    worker.poll_codex_boxed().await.unwrap();
    let error = worker.poll_codex_boxed().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::DelegatedPollMismatch);
    assert_eq!(worker.take_delegated_poll_outcomes().len(), 2);
}

#[tokio::test]
async fn delegated_outcome_survives_job_end_and_new_run_progress_starts_fresh() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A'), thread('B')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let first = worker.active_jobs()[0].clone();
    let batch_id =
        delegated_identity(&first, pump.workspace_revision(&first.codex_thread_id)).batch_id;
    pump.queue_poll(
        &first.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProgress(Box::new(
            delegated_progress(
                delegated_identity(&first, pump.workspace_revision(&first.codex_thread_id)),
                1,
                ChangeBatchProgressState::Proposed,
            ),
        ))),
    );
    pump.queue_poll(
        &first.codex_thread_id,
        Ok(CodexPoll::Inconclusive(
            secret_safe_runtime_summary("delegated proposal was inconclusive").unwrap(),
        )),
    );
    worker.poll_codex_boxed().await.unwrap();
    worker.poll_codex_boxed().await.unwrap();
    assert!(worker.active_jobs().is_empty());
    assert_eq!(
        observed_outcomes(&messages)[0].outcome.status,
        ExecutionOutcomeStatus::Failed
    );

    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('B', delivery_scope('B'))),
            now(),
        )
        .await
        .unwrap();
    let second = worker.active_jobs()[0].clone();
    let second_identity =
        delegated_identity(&second, pump.workspace_revision(&second.codex_thread_id));
    assert_ne!(second_identity.batch_id, batch_id);
    pump.queue_poll(
        &second.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProgress(Box::new(
            delegated_progress(second_identity, 1, ChangeBatchProgressState::Proposed),
        ))),
    );
    worker.poll_codex_boxed().await.unwrap();

    let outcomes = worker.take_delegated_poll_outcomes();
    assert_eq!(outcomes.len(), 2);
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, DelegatedPollOutcome::ChangeBatchProgress(_)))
    );
}

#[tokio::test]
async fn missing_usage_preserves_success_and_keeps_candidate() {
    for retained_usage in [
        None,
        Some(ExecutionOutcomeUsage::unknown(123, 48)),
        Some(measured_completion_usage()),
    ] {
        let port = RecordingPort::default();
        let messages = Rc::clone(&port.messages);
        let codex = FakeCodex::with_threads([thread('A')]);
        let pump = codex.clone();
        let mut worker = test_worker(worker_config(1), port, codex);
        register(&mut worker).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
                now(),
            )
            .await
            .unwrap();
        let active = worker.active_jobs()[0].clone();
        if let Some(usage) = &retained_usage {
            pump.state
                .lock()
                .unwrap()
                .retained_usage
                .insert(active.codex_thread_id.0.clone(), usage.clone());
        }
        std::fs::write(
            pump.workspace(&active.codex_thread_id)
                .join("candidate.txt"),
            b"candidate\n",
        )
        .unwrap();
        pump.queue_poll(
            &active.codex_thread_id,
            Ok(CodexPoll::Completed(CodexTurnCompletion {
                summary: secret_safe_runtime_summary("successful retry with unknown earlier usage")
                    .unwrap(),
                artifacts: Vec::new(),
                usage: None,
            })),
        );
        worker.poll_codex_boxed().await.unwrap();
        let artifact = observed_candidate_reference(&messages);
        acknowledge_candidate(&mut worker, &active, &artifact, 0, 'O')
            .await
            .unwrap();
        acknowledge_candidate(&mut worker, &active, &artifact, 1, 'F')
            .await
            .unwrap();
        let outcomes = observed_outcomes(&messages);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].outcome.status,
            ExecutionOutcomeStatus::Succeeded
        );
        assert_eq!(
            outcomes[0].outcome.usage,
            Some(retained_usage.unwrap_or_else(|| ExecutionOutcomeUsage::unknown(0, 0)))
        );
        assert!(outcomes[0].outcome.error.is_none());
        assert!(outcomes[0].outcome.artifacts.contains(&artifact));
    }
}

#[tokio::test]
async fn invalid_writer_candidate_fails_without_losing_completion_usage() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed without changing source")
                .unwrap(),
            artifacts: vec![],
            usage: Some(measured_completion_usage()),
        })),
    );
    worker.poll_codex_boxed().await.unwrap();
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome.status, ExecutionOutcomeStatus::Failed);
    assert_eq!(outcomes[0].outcome.usage, Some(measured_completion_usage()));
    assert_no_candidate_product(&messages);
    assert!(worker.active_jobs().is_empty());
}

#[tokio::test]
async fn generated_bytecode_rework_reports_failure_and_retains_usage_and_source() {
    let (workspaces, sources) = test_workspace_paths();
    let repository = sources.join(id("rpo", 'A'));
    let revision = |name: &str| {
        let result = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["rev-parse", name])
            .output()
            .unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    };
    let original = (0..24).fold(String::new(), |mut lines, line| {
        use std::fmt::Write as _;
        writeln!(lines, "line-{line}").unwrap();
        lines
    });
    std::fs::write(repository.join("fixture.txt"), &original).unwrap();
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
            "expanded source",
        ],
    );
    let base = revision("HEAD");
    std::fs::write(
        repository.join("fixture.txt"),
        original.replace("line-2\n", "source-change\n"),
    )
    .unwrap();
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
            "source candidate",
        ],
    );
    let commit = revision("HEAD");
    let tree = revision("HEAD^{tree}");
    let candidate_ref = format!("refs/winwincode/candidates/{commit}");
    run_git(&repository, &["update-ref", &candidate_ref, &commit]);
    let source_diff = Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--full-index",
            &format!("{base}..{commit}"),
            "--",
            "fixture.txt",
        ])
        .output()
        .unwrap();
    assert!(source_diff.status.success());
    let source_hunk_sha256 =
        winwincode_domain::rework_hunk_origins(&source_diff.stdout, &source_diff.stdout)
            .unwrap()
            .remove(0)
            .1;
    let mut dispatch = writer_dispatch('A');
    dispatch.job.execution_profile = "remediator".into();
    dispatch.job.workspace.checkout_revision = commit.clone();
    dispatch.job.work_input.as_mut().unwrap().candidate_ref = Some(candidate_ref.clone());
    let ExecutionScope::WorkRunExecutionScope(scope) = &mut dispatch.job.scope else {
        unreachable!()
    };
    scope.rework_authorization = Some(
        serde_json::from_value(serde_json::json!({
            "authorizationDigest": format!("sha256:{}", "b".repeat(64)),
            "candidateRef": candidate_ref, "diffSha256": "c".repeat(64),
            "requiresFullReverification": true, "sourceCandidateCommitId": commit,
            "sourceCandidateTreeId": tree, "targets": [{"workItemId": scope.work_item_id,
                "filePath": "fixture.txt", "sourceHunkSha256": source_hunk_sha256,
                "evidenceRefIds": ["evd_00000000000000000000000001"]}]
        }))
        .unwrap(),
    );
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = WorkerMain::new(
        worker_config(1),
        port,
        codex,
        JobWorkspaceRuntime::open(workspaces, &sources).unwrap(),
    );
    register(&mut worker).await;
    worker
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(dispatch), now())
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let checkout = pump.workspace(&active.codex_thread_id);
    std::fs::write(
        checkout.join("fixture.txt"),
        original.replace("line-2\n", "reworked-source\n"),
    )
    .unwrap();
    let generated = checkout.join("__pycache__/main.cpython-314.pyc");
    std::fs::create_dir_all(generated.parent().unwrap()).unwrap();
    std::fs::write(&generated, b"generated bytecode").unwrap();
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("rework completed").unwrap(),
            artifacts: vec![],
            usage: Some(measured_completion_usage()),
        })),
    );
    worker.poll_codex_boxed().await.unwrap();
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome.status, ExecutionOutcomeStatus::Failed);
    assert_eq!(outcomes[0].outcome.usage, Some(measured_completion_usage()));
    assert_no_candidate_product(&messages);
    assert!(worker.active_jobs().is_empty());
    let refs = Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/winwincode/candidates/",
        ])
        .output()
        .unwrap();
    assert!(refs.status.success());
    let reference = String::from_utf8(refs.stdout).unwrap();
    assert_eq!(reference.lines().count(), 2);
    let failed_candidate = reference
        .lines()
        .find(|candidate| *candidate != candidate_ref)
        .expect("rejected output source is retained");
    let retained = Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args(["show", &format!("{failed_candidate}:fixture.txt")])
        .output()
        .unwrap();
    assert!(retained.status.success());
    assert!(
        String::from_utf8(retained.stdout)
            .unwrap()
            .contains("reworked-source")
    );
    let retained_generated = Command::new("git")
        .arg("-C")
        .arg(&repository)
        .args([
            "show",
            &format!("{failed_candidate}:__pycache__/main.cpython-314.pyc"),
        ])
        .output()
        .unwrap();
    assert!(retained_generated.status.success());
    assert_eq!(retained_generated.stdout, b"generated bytecode");
}

#[tokio::test]
async fn replacement_open_can_finish_the_writer_with_its_durable_completed_predecessor() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    std::fs::write(
        pump.workspace(&active.codex_thread_id)
            .join("candidate.txt"),
        b"candidate\n",
    )
    .unwrap();
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    worker.poll_codex_boxed().await.unwrap();
    let artifact = observed_candidate_reference(&messages);
    let predecessor = ArtifactReference {
        artifact_id: ArtifactId(id("art", 'P')),
        digest: artifact.digest.clone(),
    };
    pump.state.lock().unwrap().candidate_completed_predecessor = Some(predecessor.clone());
    let mut ack = candidate_ack(&active, &artifact, 0, 'O');
    ack.retained_artifact = Some(predecessor.clone());
    worker
        .accept_control(&ExecutionPortMessage::ArtifactAckMessage(ack), now())
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome.artifacts, vec![predecessor]);
}

#[tokio::test]
async fn worker_backpressure_stops_the_batch_without_marking_refused_frames_sent() {
    let port = RecordingPort::default();
    let blocked = Rc::clone(&port.backpressured);
    let attempts = Rc::clone(&port.attempts);
    let messages = Rc::clone(&port.messages);
    let latency = Rc::clone(&port.latency);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    let active = worker.active_jobs()[0].clone();
    let template: RuntimeEventMessage = serde_json::from_value(serde_json::json!({
        "kind":"runtime.event", "schemaVersion":SchemaVersion::WinwincodeV1,
        "messageId":id("xmsg",'Z'), "sentAt":now(), "lease":active.lease,
        "workerSessionId":active.worker_session_id, "sessionIdentity":active.session_identity,
        "codexThreadId":active.codex_thread_id,
        "event":{"eventId":id("evt",'Z'), "sequence":1, "category":"command",
            "occurredAt":now(), "summary":"durable evidence"}
    }))
    .unwrap();
    for index in 1..=256 {
        let mut frame = template.clone();
        frame.message_id = ExecutionMessageId(format!("xmsg_{:026}", index + 1000));
        frame.event.event_id = ExecutionEventId(format!("evt_{index:026}"));
        frame.event.sequence = ExecutionSequence(index);
        pump.clone()
            .retain_execution_delivery(&ExecutionPortMessage::RuntimeEventMessage(frame))
            .unwrap();
    }
    blocked.set(true);
    let before = attempts.get();
    let error = worker.flush_durable_outbox().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::ExecutionBackpressure);
    assert_eq!(
        attempts.get() - before,
        1,
        "first worker-wide rejection yields the batch"
    );
    assert_eq!(pump.state.lock().unwrap().pending_delivery_ids.len(), 256);
    blocked.set(false);
    latency.set(std::time::Duration::from_millis(100));
    let before = attempts.get();
    worker.flush_durable_outbox().await.unwrap();
    assert!(
        attempts.get() - before <= 3,
        "250 ms budget yields after the current exchange"
    );
    let before = attempts.get();
    worker.heartbeat(now()).await.unwrap();
    assert_eq!(
        attempts.get() - before,
        1,
        "heartbeat never scans the business outbox"
    );
    latency.set(std::time::Duration::ZERO);
    let before = attempts.get();
    worker.flush_durable_outbox().await.unwrap();
    assert!(attempts.get() - before <= 64, "each turn is bounded");
    for _ in 0..8 {
        worker.flush_durable_outbox().await.unwrap();
    }
    assert_eq!(
        messages
            .borrow()
            .iter()
            .filter(|message| matches!(message, ExecutionPortMessage::RuntimeEventMessage(_)))
            .count(),
        256
    );
}

#[tokio::test]
async fn backpressure_keeps_core_facts_retained_for_the_next_send() {
    let port = RecordingPort::default();
    let blocked = Rc::clone(&port.backpressured);
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let ExecutionPortMessage::RuntimeEventMessage(mut frame) =
        execution_port_fixture("runtime.event")
    else {
        unreachable!()
    };
    frame.lease = active.lease.clone();
    frame.worker_session_id = active.worker_session_id.clone();
    frame.session_identity = active.session_identity.clone();
    frame.codex_thread_id = active.codex_thread_id.clone();
    frame.event.sequence = ExecutionSequence(1);
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::RuntimeTrace(Box::new(frame.clone()))),
    );
    blocked.set(true);
    assert_eq!(
        worker.poll_codex_boxed().await.unwrap_err().code,
        WorkerErrorCode::ExecutionBackpressure
    );
    assert!(
        pump.pending_kinds()
            .iter()
            .any(|kind| kind == "runtime.event"),
        "the shared drive retains Core evidence while the transport batch is refused"
    );
    blocked.set(false);
    worker.poll_codex_boxed().await.unwrap();
    assert_eq!(
        messages
            .borrow()
            .iter()
            .filter(|message| matches!(message,
        ExecutionPortMessage::RuntimeEventMessage(delivered) if delivered == &frame))
            .count(),
        1
    );
}

#[tokio::test]
async fn candidate_ack_keeps_original_upload_authority_after_current_lease_renewal() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .unwrap();
    let original = worker.active_jobs()[0].clone();
    std::fs::write(
        pump.workspace(&original.codex_thread_id)
            .join("candidate.txt"),
        b"candidate\n",
    )
    .unwrap();
    pump.queue_poll(
        &original.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    worker.poll_codex_boxed().await.unwrap();
    let artifact = observed_candidate_reference(&messages);
    let mut lease = original.lease.clone();
    lease.expires_at = Instant("2027-01-15T09:00:00.000Z".into());
    worker
        .accept_control(
            &ExecutionPortMessage::LeaseRenewMessage(
                winwincode_execution_port::generated::LeaseRenewMessage {
                    kind: winwincode_execution_port::generated::LeaseRenewMessageKind::LeaseRenew,
                    schema_version: SchemaVersion::WinwincodeV1,
                    message_id: ExecutionMessageId(id("xmsg", 'R')),
                    sent_at: now(),
                    request_id: RequestId(id("req", 'R')),
                    lease,
                    prior_expires_at: original.lease.expires_at.clone(),
                },
            ),
            now(),
        )
        .await
        .unwrap();
    acknowledge_candidate(&mut worker, &original, &artifact, 0, 'O')
        .await
        .unwrap();
    let current = worker.active_jobs()[0].clone();
    acknowledge_candidate(&mut worker, &current, &artifact, 1, 'F')
        .await
        .unwrap();
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].outcome.status,
        ExecutionOutcomeStatus::Succeeded
    );
    assert_eq!(outcomes[0].lease, current.lease);
}

#[tokio::test]
async fn deferred_local_model_start_obeys_current_lease_and_keeps_the_original_identity() {
    for renewed in [false, true] {
        let codex = FakeCodex::with_threads([thread('A')]);
        let mut pump = codex.clone();
        let root = tempfile::tempdir().unwrap();
        let providers = root.path().join("providers");
        let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex)
            .with_device_providers(&providers)
            .unwrap();
        register(&mut worker).await;
        worker
            .accept_control_and_drive(
                &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
                now(),
            )
            .await
            .unwrap();
        let active = worker.active_jobs()[0].clone();
        let ExecutionPortMessage::ModelOpenMessage(mut open) = execution_port_fixture("model.open")
        else {
            unreachable!()
        };
        open.lease = active.lease.clone();
        open.worker_session_id = active.worker_session_id.clone();
        open.session_identity = active.session_identity.clone();
        let original = open.clone();
        let retained = pump
            .retain_execution_delivery(&ExecutionPortMessage::ModelOpenMessage(open))
            .unwrap();
        assert!(worker.inject_device_start_fault());
        assert_eq!(
            worker.flush_durable_outbox().await.unwrap_err().code,
            WorkerErrorCode::ModelStartDeferred
        );
        worker
            .flush_durable_outbox()
            .await
            .expect("finish the deferred scan before the next start attempt");
        if renewed {
            let mut lease = active.lease.clone();
            lease.expires_at = Instant("2027-01-15T08:10:00.000Z".into());
            worker.accept_control(&ExecutionPortMessage::LeaseRenewMessage(
                winwincode_execution_port::generated::LeaseRenewMessage {
                    kind: winwincode_execution_port::generated::LeaseRenewMessageKind::LeaseRenew,
                    schema_version: SchemaVersion::WinwincodeV1, message_id: ExecutionMessageId(id("xmsg",'R')),
                    sent_at: now(), request_id: RequestId(id("req",'R')), lease, prior_expires_at: active.lease.expires_at.clone(),
                }), now()).await.unwrap();
        }
        let later = Instant("2027-01-15T08:06:00.000Z".into());
        worker.heartbeat(later.clone()).await.unwrap();
        let sent = worker.flush_durable_outbox().await;
        if renewed {
            sent.unwrap();
        } else {
            assert_eq!(
                sent.unwrap_err().code,
                WorkerErrorCode::ExecutionMessageRejected
            );
        }
        let store = winwincode_provider::DeviceProviderStore::open(&providers).unwrap();
        for _ in 0..100 {
            if store.list_stored_model_exchanges().unwrap().len() == usize::from(renewed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            store.list_stored_model_exchanges().unwrap().len(),
            usize::from(renewed),
            "an expired unstarted call never enters the Provider execution ledger"
        );
        assert!(
            pump.state
                .lock()
                .unwrap()
                .durable_deliveries
                .iter()
                .any(|delivery| delivery.delivery_id == retained.delivery_id
                    && delivery.message
                        == ExecutionPortMessage::ModelOpenMessage(original.clone()))
        );
        if renewed {
            for _ in 0..100 {
                Box::pin(worker.poll_codex(later.clone())).await.unwrap();
                if pump.state.lock().unwrap().model_open_acknowledged {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            worker.flush_durable_outbox().await.unwrap();
            assert_eq!(
                store.list_stored_model_exchanges().unwrap().len(),
                1,
                "replaying the same authorized identity cannot start twice"
            );
        }
        worker.shutdown(later).await.unwrap();
    }
}

#[tokio::test]
async fn preserved_clock_anchor_allows_new_leases_without_extending_expiry() {
    for (elapsed_ms, sample, allowed) in [
        (10_005, "2027-01-15T08:00:12.000Z", true),
        (10_005, "2027-01-15T08:00:01.000Z", true),
        (12_005, "2027-01-15T08:00:12.000Z", false),
        (12_005, "2027-01-15T08:00:01.000Z", false),
    ] {
        let codex = FakeCodex::with_threads([thread('A')]);
        let mut pump = codex.clone();
        let root = tempfile::tempdir().unwrap();
        let providers = root.path().join("providers");
        let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex)
            .with_device_providers(&providers)
            .unwrap();
        register(&mut worker).await;
        worker.inject_driver_clock(now(), std::time::Duration::from_millis(10_005));
        let mut dispatch = dispatch('A', delivery_scope('A'));
        dispatch.lease.issued_at = Instant("2027-01-15T08:00:11.000Z".into());
        dispatch.lease.expires_at = Instant(
            if allowed {
                "2027-01-15T08:05:00.000Z"
            } else {
                "2027-01-15T08:00:13.000Z"
            }
            .into(),
        );
        let dispatch_at = Instant("2027-01-15T08:00:12.000Z".into());
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch),
                dispatch_at,
            )
            .await
            .unwrap();
        let active = worker.active_jobs()[0].clone();
        let ExecutionPortMessage::ModelOpenMessage(mut open) = execution_port_fixture("model.open")
        else {
            unreachable!()
        };
        open.lease = active.lease;
        open.worker_session_id = active.worker_session_id;
        open.session_identity = active.session_identity;
        let retained = pump
            .retain_execution_delivery(&ExecutionPortMessage::ModelOpenMessage(open.clone()))
            .unwrap();
        worker.inject_driver_clock(now(), std::time::Duration::from_millis(elapsed_ms));
        let result = worker.flush_durable_outbox_at(Instant(sample.into())).await;
        if allowed {
            result.unwrap();
        } else {
            assert_eq!(
                result.unwrap_err().code,
                WorkerErrorCode::ExecutionMessageRejected
            );
        }
        let store = winwincode_provider::DeviceProviderStore::open(&providers).unwrap();
        for _ in 0..100 {
            if store.list_stored_model_exchanges().unwrap().len() == usize::from(allowed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            store.list_stored_model_exchanges().unwrap().len(),
            usize::from(allowed),
            "current authority uses elapsed time for both issuance and expiry"
        );
        assert!(
            pump.state
                .lock()
                .unwrap()
                .durable_deliveries
                .iter()
                .any(|delivery| delivery.delivery_id == retained.delivery_id
                    && delivery.message == ExecutionPortMessage::ModelOpenMessage(open.clone()))
        );
        if allowed {
            worker
                .flush_durable_outbox_at(Instant(sample.into()))
                .await
                .unwrap();
            assert_eq!(
                store.list_stored_model_exchanges().unwrap().len(),
                1,
                "the immutable original request starts at most once"
            );
        }
        worker
            .shutdown(Instant("2027-01-15T08:00:14.000Z".into()))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn stale_driver_time_cannot_reset_elapsed_first_start_authority() {
    for renewed in [false, true] {
        let codex = FakeCodex::with_threads([thread('A')]);
        let mut pump = codex.clone();
        let root = tempfile::tempdir().unwrap();
        let providers = root.path().join("providers");
        let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex)
            .with_device_providers(&providers)
            .unwrap();
        register(&mut worker).await;
        let mut dispatch = dispatch('A', delivery_scope('A'));
        dispatch.lease.expires_at = Instant("2027-01-15T08:00:03.000Z".into());
        worker
            .accept_control_and_drive(&ExecutionPortMessage::JobDispatchMessage(dispatch), now())
            .await
            .unwrap();
        let active = worker.active_jobs()[0].clone();
        let ExecutionPortMessage::ModelOpenMessage(mut open) = execution_port_fixture("model.open")
        else {
            unreachable!()
        };
        open.lease = active.lease.clone();
        open.worker_session_id = active.worker_session_id.clone();
        open.session_identity = active.session_identity.clone();
        pump.retain_execution_delivery(&ExecutionPortMessage::ModelOpenMessage(open))
            .unwrap();
        assert!(worker.inject_device_start_fault());
        assert_eq!(
            worker
                .flush_durable_outbox_at(now())
                .await
                .unwrap_err()
                .code,
            WorkerErrorCode::ModelStartDeferred
        );
        worker
            .flush_durable_outbox_at(now())
            .await
            .expect("finish the deferred scan before advancing elapsed time");
        if renewed {
            let mut lease = active.lease.clone();
            lease.expires_at = Instant("2027-01-15T08:10:00.000Z".into());
            worker.accept_control(&ExecutionPortMessage::LeaseRenewMessage(
                winwincode_execution_port::generated::LeaseRenewMessage {
                    kind: winwincode_execution_port::generated::LeaseRenewMessageKind::LeaseRenew,
                    schema_version: SchemaVersion::WinwincodeV1,
                    message_id: ExecutionMessageId(id("xmsg", 'R')), sent_at: now(),
                    request_id: RequestId(id("req", 'R')), lease,
                    prior_expires_at: active.lease.expires_at.clone(),
                }), now()).await.unwrap();
        }
        // The driver sampled its wall clock before a control/network await.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let result = worker.flush_durable_outbox_at(now()).await;
        if renewed {
            result.unwrap();
        } else {
            assert_eq!(
                result.unwrap_err().code,
                WorkerErrorCode::ExecutionMessageRejected,
                "reusing a pre-await wall timestamp must retain elapsed monotonic time"
            );
        }
        let store = winwincode_provider::DeviceProviderStore::open(&providers).unwrap();
        for _ in 0..100 {
            if store.list_stored_model_exchanges().unwrap().len() == usize::from(renewed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            store.list_stored_model_exchanges().unwrap().len(),
            usize::from(renewed)
        );
        // A later poll with the same stale timestamp must not reset the origin either.
        Box::pin(worker.poll_codex(now())).await.unwrap();
        assert_eq!(
            store.list_stored_model_exchanges().unwrap().len(),
            usize::from(renewed)
        );
        worker.shutdown(now()).await.unwrap();
    }
}

fn provider_quota_messages(
    active: &winwincode_worker::ActiveJob,
) -> (
    winwincode_execution_port::generated::ModelOpenMessage,
    winwincode_execution_port::generated::ModelOpenMessage,
    winwincode_execution_port::generated::ModelAckMessage,
) {
    use base64::Engine as _;
    let ExecutionPortMessage::ModelOpenMessage(mut open_a) = execution_port_fixture("model.open")
    else {
        unreachable!()
    };
    open_a.lease = active.lease.clone();
    open_a.worker_session_id = active.worker_session_id.clone();
    open_a.session_identity = active.session_identity.clone();
    open_a.message_id = ExecutionMessageId(id("xmsg", 'P'));
    open_a.model_exchange_id = winwincode_domain::ModelExchangeId(id("mdl", 'A'));
    let payload_a = serde_json::to_vec(&serde_json::json!({
        "provider": "quota-provider-A",
        "request": {"model": "quota-model", "input": []}
    }))
    .unwrap();
    open_a.request.data_base64 = base64::engine::general_purpose::STANDARD.encode(&payload_a);
    open_a.request.payload_digest =
        Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload_a)));
    let mut open_b = open_a.clone();
    open_b.message_id = ExecutionMessageId(id("xmsg", 'Q'));
    open_b.model_exchange_id = winwincode_domain::ModelExchangeId(id("mdl", 'B'));
    let payload_b = serde_json::to_vec(&serde_json::json!({
        "provider": "quota-provider-B",
        "request": {"model": "quota-model", "input": []}
    }))
    .unwrap();
    open_b.request.data_base64 = base64::engine::general_purpose::STANDARD.encode(&payload_b);
    open_b.request.payload_digest =
        Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload_b)));
    let ExecutionPortMessage::ModelAckMessage(mut cancellation) =
        execution_port_fixture("model.ack")
    else {
        unreachable!()
    };
    cancellation.message_id = ExecutionMessageId(id("xmsg", 'R'));
    cancellation.lease = active.lease.clone();
    cancellation.worker_session_id = active.worker_session_id.clone();
    cancellation.session_identity = active.session_identity.clone();
    cancellation.model_exchange_id = winwincode_domain::ModelExchangeId(id("mdl", 'C'));
    cancellation.error = Some(winwincode_execution_port::generated::ExecutionPortError {
        code: winwincode_execution_port::generated::ExecutionPortErrorCode::ModelStreamFailed,
        message: "model exchange cancelled".to_owned(),
        retryable: false,
    });
    (open_a, open_b, cancellation)
}

#[tokio::test]
async fn full_provider_slots_do_not_block_another_provider_or_model_cancellation() {
    use winwincode_provider::{DeviceModelAdmission, DeviceProviderStore};

    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let mut pump = codex.clone();
    let root = tempfile::tempdir().unwrap();
    let providers = root.path().join("providers");
    let mut worker = test_worker(worker_config(1), port, codex)
        .with_device_providers(&providers)
        .unwrap();
    register(&mut worker).await;
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let (open_a, open_b, cancellation) = provider_quota_messages(worker.active_jobs()[0]);

    let store = DeviceProviderStore::open(&providers).unwrap();
    let mut held_slots = Vec::new();
    for _ in 0..3 {
        let DeviceModelAdmission::Ready(permit) = store.try_model_permit(&open_a).unwrap() else {
            panic!("each of the first three Provider A calls acquires a slot");
        };
        held_slots.push(permit);
    }
    assert!(matches!(
        store.try_model_permit(&open_a).unwrap(),
        DeviceModelAdmission::Deferred
    ));
    let retained_a = pump
        .retain_execution_delivery(&ExecutionPortMessage::ModelOpenMessage(open_a.clone()))
        .unwrap();
    let retained_b = pump
        .retain_execution_delivery(&ExecutionPortMessage::ModelOpenMessage(open_b.clone()))
        .unwrap();
    let retained_cancellation = pump
        .retain_execution_delivery(&ExecutionPortMessage::ModelAckMessage(cancellation.clone()))
        .unwrap();

    assert_eq!(
        worker.flush_durable_outbox().await.unwrap_err().code,
        WorkerErrorCode::ModelStartDeferred
    );
    assert!(
        store
            .model_cancelled(&cancellation.model_exchange_id.0)
            .unwrap()
    );
    {
        let state = pump.state.lock().unwrap();
        assert!(state.pending_delivery_ids.contains(&retained_a.delivery_id));
        assert!(!state.pending_delivery_ids.contains(&retained_b.delivery_id));
        assert!(
            !state
                .pending_delivery_ids
                .contains(&retained_cancellation.delivery_id),
            "the same bounded batch handles cancellation after the full Provider"
        );
    }
    assert!(
        !store.model_start_recorded(&open_a).unwrap(),
        "quota refusal must leave the first-start exchange record absent"
    );
    for _ in 0..100 {
        if !store
            .replay_model(&open_b.model_exchange_id.0, 1)
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let chunks_b = store.replay_model(&open_b.model_exchange_id.0, 1).unwrap();
    assert!(
        chunks_b
            .iter()
            .any(|chunk| chunk.is_final && chunk.error.is_some()),
        "Provider B completes its existing unconfigured-provider error path without waiting for A"
    );
    assert!(store.model_start_recorded(&open_b).unwrap());
    assert!(!store.model_start_recorded(&open_a).unwrap());

    drop(held_slots.pop().unwrap());
    worker
        .flush_durable_outbox()
        .await
        .expect("an empty batch finishes the scan after the sent cancellation");
    assert!(!store.model_start_recorded(&open_a).unwrap());
    worker
        .flush_durable_outbox()
        .await
        .expect("the next scan retries the original Provider A intent in the freed slot");
    for _ in 0..100 {
        if !store
            .replay_model(&open_a.model_exchange_id.0, 1)
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(store.model_start_recorded(&open_a).unwrap());
    assert_eq!(
        store.list_stored_model_exchanges().unwrap(),
        vec![
            open_a.model_exchange_id.0.clone(),
            open_b.model_exchange_id.0.clone()
        ]
    );
    {
        let state = pump.state.lock().unwrap();
        assert!(!state.pending_delivery_ids.contains(&retained_a.delivery_id));
        assert_eq!(
            state
                .durable_deliveries
                .iter()
                .filter(|delivery| delivery.delivery_id == retained_a.delivery_id)
                .map(|delivery| &delivery.message)
                .collect::<Vec<_>>(),
            vec![&ExecutionPortMessage::ModelOpenMessage(open_a)],
            "retry reuses the one durable intent and its exact original identity"
        );
    }
    assert!(
        !messages.borrow().iter().any(|message| matches!(
            message,
            ExecutionPortMessage::ModelOpenMessage(_) | ExecutionPortMessage::ModelAckMessage(_)
        )),
        "Provider admission and cancellation remain local to the Device"
    );
    drop(worker);
}

#[tokio::test]
async fn owned_model_chunk_retry_does_not_starve_cancelled_core_facts() {
    use base64::Engine as _;
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let root = tempfile::tempdir().unwrap();
    let providers = root.path().join("providers");
    let mut worker = test_worker(worker_config(1), port, codex)
        .with_device_providers(&providers)
        .unwrap();
    register(&mut worker).await;
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let (open, _, _) = provider_quota_messages(&active);
    let store = winwincode_provider::DeviceProviderStore::open(&providers).unwrap();
    let original = store.reject_model_start(&open).unwrap();
    worker
        .accept_control(
            &ExecutionPortMessage::JobCancelMessage(cancel_for(&active, 'C')),
            now(),
        )
        .await
        .unwrap();
    pump.state
        .lock()
        .unwrap()
        .failures
        .insert(FailurePoint::ModelChunk);
    let bytes = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": "winwincode.core-tool-fact.v1", "sourceThreadId": "core-thread",
        "sourceSequence": 184, "factJson": "{\"kind\":\"cell\",\"fact\":{\"cell_id\":\"1\",\"lifecycle\":\"closed\"}}",
    })).unwrap();
    let frame: RuntimeEventMessage = serde_json::from_value(serde_json::json!({
        "kind":"runtime.event", "schemaVersion":SchemaVersion::WinwincodeV1,
        "messageId":id("xmsg",'Z'), "sentAt":now(), "lease":active.lease,
        "workerSessionId":active.worker_session_id, "sessionIdentity":active.session_identity,
        "codexThreadId":active.codex_thread_id,
        "event":{"eventId":id("evt",'Z'), "sequence":1, "category":"activity",
            "occurredAt":now(), "summary":"Core cell closed", "payload": {
                "contentType":"application/vnd.winwincode.core-tool-fact+json",
                "dataBase64":base64::engine::general_purpose::STANDARD.encode(&bytes),
                "payloadDigest":format!("sha256:{:x}", Sha256::digest(&bytes)),
            }},
    }))
    .unwrap();
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::RuntimeTrace(Box::new(frame.clone()))),
    );
    for _ in 0..3 {
        let error = worker.poll_codex_boxed().await.unwrap_err();
        assert_eq!(error.code, WorkerErrorCode::UnexpectedMessage);
    }
    assert!(
        messages.borrow().iter().any(|message| matches!(message,
        ExecutionPortMessage::RuntimeEventMessage(delivered) if delivered == &frame)),
        "retrying an owned Provider frame must not strand already recorded Core cancellation facts"
    );
    assert_eq!(
        pump.calls()
            .iter()
            .filter(|call| *call == "model_chunk:1")
            .count(),
        3,
        "the exact refused input remains retryable on the next poll"
    );
    assert_eq!(
        store.replay_model(&open.model_exchange_id.0, 1).unwrap(),
        original
    );
    assert!(
        observed_outcomes(&messages).is_empty(),
        "input retry cannot invent a business outcome"
    );
}

#[tokio::test]
async fn invalid_provider_slot_permissions_complete_with_failure_without_blocking_the_batch() {
    use std::os::unix::fs::PermissionsExt as _;
    use winwincode_execution_port::generated::ExecutionPortErrorCode;
    use winwincode_provider::{DeviceModelAdmission, DeviceProviderStore};

    for corrupt_slot_file in [false, true] {
        let codex = FakeCodex::with_threads([thread('A')]);
        let mut pump = codex.clone();
        let root = tempfile::tempdir().unwrap();
        let providers = root.path().join("providers");
        let mut worker = test_worker(worker_config(1), RecordingPort::default(), codex)
            .with_device_providers(&providers)
            .unwrap();
        register(&mut worker).await;
        worker
            .accept_control_and_drive(
                &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
                now(),
            )
            .await
            .unwrap();
        let (open_a, open_b, cancellation) = provider_quota_messages(worker.active_jobs()[0]);
        let store = DeviceProviderStore::open(&providers).unwrap();
        let DeviceModelAdmission::Ready(permit) = store.try_model_permit(&open_a).unwrap() else {
            panic!("initial Provider A slot is available before changing permissions");
        };
        drop(permit);
        let slots = providers
            .join("model-provider-slots")
            .join(format!("{:x}", Sha256::digest(b"quota-provider-A")));
        let (path, mode) = if corrupt_slot_file {
            (slots.join("slot-0"), 0o644)
        } else {
            (slots, 0o755)
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            store.try_model_permit(&open_a).is_err(),
            "unsafe permissions are an admission failure, not a full quota"
        );
        let retained_a = pump
            .retain_execution_delivery(&ExecutionPortMessage::ModelOpenMessage(open_a.clone()))
            .unwrap();
        let retained_b = pump
            .retain_execution_delivery(&ExecutionPortMessage::ModelOpenMessage(open_b.clone()))
            .unwrap();
        let retained_cancellation = pump
            .retain_execution_delivery(&ExecutionPortMessage::ModelAckMessage(cancellation.clone()))
            .unwrap();

        worker
            .flush_durable_outbox()
            .await
            .expect("the admission failure becomes a terminal response while the batch continues");
        let chunks_a = store.replay_model(&open_a.model_exchange_id.0, 1).unwrap();
        assert_eq!(chunks_a.len(), 1);
        assert!(chunks_a[0].is_final);
        assert_eq!(
            chunks_a[0].error.as_ref().unwrap().code,
            ExecutionPortErrorCode::DeviceProviderUnavailable
        );
        assert!(!chunks_a[0].error.as_ref().unwrap().retryable);
        assert!(store.model_start_recorded(&open_a).unwrap());
        assert!(
            store
                .model_cancelled(&cancellation.model_exchange_id.0)
                .unwrap(),
            "the cancellation after the failed Provider is durably applied"
        );
        {
            let state = pump.state.lock().unwrap();
            for delivery in [&retained_a, &retained_b, &retained_cancellation] {
                assert!(
                    !state.pending_delivery_ids.contains(&delivery.delivery_id),
                    "the failed Provider and later effects must not remain pending"
                );
            }
        }
        for _ in 0..100 {
            if !store
                .replay_model(&open_b.model_exchange_id.0, 1)
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            store
                .replay_model(&open_b.model_exchange_id.0, 1)
                .unwrap()
                .iter()
                .any(|chunk| chunk.is_final && chunk.error.is_some()),
            "Provider B still completes its normal local configuration error path"
        );
        for _ in 0..3 {
            worker
                .flush_durable_outbox()
                .await
                .expect("later scans must not indefinitely retry unavailable admission storage");
        }
        assert_eq!(
            store.replay_model(&open_a.model_exchange_id.0, 1).unwrap(),
            chunks_a,
            "the admission failure remains one exact durable terminal response"
        );
        assert_eq!(
            store.list_stored_model_exchanges().unwrap(),
            vec![
                open_a.model_exchange_id.0.clone(),
                open_b.model_exchange_id.0.clone()
            ]
        );
        drop(worker);
    }
}

#[tokio::test]
async fn never_started_local_model_retries_through_the_worker_outbox_and_releases_its_request() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let mut pump = codex.clone();
    let root = std::env::temp_dir().join(format!(
        "wwc-local-start-retry-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut worker = test_worker(worker_config(1), port, codex)
        .with_device_providers(&root)
        .unwrap();
    register(&mut worker).await;
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let ExecutionPortMessage::ModelOpenMessage(mut open) = execution_port_fixture("model.open")
    else {
        unreachable!()
    };
    open.lease = active.lease;
    open.worker_session_id = active.worker_session_id;
    open.session_identity = active.session_identity;
    let retained = pump
        .retain_execution_delivery(&ExecutionPortMessage::ModelOpenMessage(open))
        .unwrap();
    assert!(worker.inject_device_start_fault());
    assert_eq!(
        worker.flush_durable_outbox().await.unwrap_err().code,
        WorkerErrorCode::ModelStartDeferred
    );
    assert!(
        pump.state
            .lock()
            .unwrap()
            .pending_delivery_ids
            .contains(&retained.delivery_id)
    );
    for _ in 0..100 {
        worker.poll_codex_boxed().await.unwrap();
        if pump.state.lock().unwrap().model_open_acknowledged {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        pump.state.lock().unwrap().model_open_acknowledged,
        "the original never-started open later starts and its local response is consumed"
    );
    assert!(
        !pump
            .state
            .lock()
            .unwrap()
            .pending_delivery_ids
            .contains(&retained.delivery_id)
    );
    assert!(
        !messages
            .borrow()
            .iter()
            .any(|frame| matches!(frame, ExecutionPortMessage::ModelOpenMessage(_)))
    );
    worker.shutdown(now()).await.unwrap();
    drop(worker);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn permanently_rejected_evidence_ends_the_affected_job_and_does_not_pin_pending() {
    let port = RecordingPort::default();
    let reject_next = Rc::clone(&port.reject_next);
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A'), thread('B')]);
    let mut pump = codex.clone();
    let mut worker = test_worker(worker_config(2), port, codex);
    register(&mut worker).await;
    for suffix in ['A', 'B'] {
        worker
            .accept_control_and_drive(
                &ExecutionPortMessage::JobDispatchMessage(dispatch(suffix, delivery_scope(suffix))),
                now(),
            )
            .await
            .unwrap();
    }
    let active = worker.active_jobs()[0].clone();
    let mut frame = serde_json::to_value(execution_port_fixture("runtime.event")).unwrap();
    frame["lease"] = serde_json::to_value(&active.lease).unwrap();
    frame["workerSessionId"] = serde_json::to_value(&active.worker_session_id).unwrap();
    frame["sessionIdentity"] = serde_json::to_value(&active.session_identity).unwrap();
    frame["codexThreadId"] = serde_json::to_value(&active.codex_thread_id).unwrap();
    let frame = serde_json::from_value(frame).unwrap();
    let retained = pump.retain_execution_delivery(&frame).unwrap();
    reject_next.set(true);
    assert_eq!(
        worker.flush_durable_outbox().await.unwrap_err().code,
        WorkerErrorCode::ExecutionMessageRejected
    );
    assert!(
        !pump
            .state
            .lock()
            .unwrap()
            .pending_delivery_ids
            .contains(&retained.delivery_id)
    );
    assert_eq!(worker.active_jobs().len(), 1, "unrelated job stays active");
    worker.flush_durable_outbox().await.unwrap();
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].lease.job_id, active.job.job_id);
    assert_eq!(
        outcomes[0].outcome.status,
        ExecutionOutcomeStatus::InfrastructureError
    );
    worker.flush_durable_outbox().await.unwrap();
    assert_eq!(
        observed_outcomes(&messages).len(),
        1,
        "permanent refusal is not scanned again"
    );
}

#[tokio::test]
async fn artifact_ack_consumption_does_not_depend_on_followup_transport() {
    let port = RecordingPort::default();
    let failures = port.failures_handle();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    std::fs::write(
        pump.workspace(&active.codex_thread_id)
            .join("candidate.txt"),
        b"candidate\n",
    )
    .unwrap();
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: vec![],
            usage: Some(measured_completion_usage()),
        })),
    );
    worker.poll_codex_boxed().await.unwrap();
    let artifact = observed_candidate_reference(&messages);
    acknowledge_candidate(&mut worker, &active, &artifact, 0, 'O')
        .await
        .unwrap();
    failures.set(1);
    worker
        .accept_control(
            &ExecutionPortMessage::ArtifactAckMessage(candidate_ack(&active, &artifact, 1, 'F')),
            now(),
        )
        .await
        .expect("durably consumed ACK can be confirmed while outcome transport is unavailable");
    assert_eq!(
        failures.get(),
        1,
        "control consumption never attempts followup IO"
    );
    assert_no_outcome(&messages);
    assert!(worker.active_jobs().is_empty());
    assert!(pump.state.lock().unwrap().durable_deliveries.iter().any(
        |delivery| matches!(&delivery.message, ExecutionPortMessage::JobOutcomeMessage(outcome)
            if outcome.outcome.artifacts == vec![artifact.clone()])
    ));
    worker
        .flush_durable_outbox()
        .await
        .expect_err("outcome remains retryable");
    worker.flush_durable_outbox().await.unwrap();
    assert_eq!(observed_outcomes(&messages).len(), 1);
}

#[tokio::test]
async fn writer_outcome_waits_for_one_exact_final_candidate_ack() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let checkout = pump.workspace(&active.codex_thread_id);
    std::fs::write(checkout.join("candidate.txt"), b"candidate\n")
        .expect("write candidate in the Worker-owned checkout");
    let injected = ArtifactReference {
        artifact_id: ArtifactId(id("art", 'Z')),
        digest: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
    };
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: vec![injected],
            usage: Some(measured_completion_usage()),
        })),
    );
    let rejected = worker
        .poll_codex_boxed()
        .await
        .expect_err("writer cannot inject an unacknowledged reference");
    assert_eq!(rejected.code, WorkerErrorCode::CandidateArtifactMismatch);
    assert_no_outcome(&messages);

    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    worker.poll_codex_boxed().await.unwrap();
    let artifact = observed_candidate_reference(&messages);
    assert_eq!(
        messages
            .borrow()
            .iter()
            .filter(|message| matches!(
                message,
                ExecutionPortMessage::ArtifactOpenMessage(_)
                    | ExecutionPortMessage::ArtifactChunkMessage(_)
            ))
            .count(),
        2
    );
    assert_no_outcome(&messages);

    let mut wrong = candidate_ack(&active, &artifact, 0, 'W');
    wrong.artifact_id = ArtifactId(id("art", 'W'));
    let rejected = worker
        .accept_control(&ExecutionPortMessage::ArtifactAckMessage(wrong), now())
        .await
        .expect_err("foreign Artifact ack");
    assert_eq!(rejected.code, WorkerErrorCode::CandidateArtifactMismatch);
    assert_no_outcome(&messages);

    acknowledge_candidate(&mut worker, &active, &artifact, 0, 'O')
        .await
        .expect("open acknowledgement");
    assert_no_outcome(&messages);
    acknowledge_candidate(&mut worker, &active, &artifact, 1, 'F')
        .await
        .expect("final candidate acknowledgement");

    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome.artifacts, vec![artifact]);
    assert_eq!(
        outcomes[0].outcome.status,
        ExecutionOutcomeStatus::Succeeded
    );
    assert!(worker.active_jobs().is_empty());
}

#[tokio::test]
async fn chat_files_survive_cleanup_and_wait_for_final_artifact_ack() {
    for failed in [false, true] {
        let port = RecordingPort::default();
        let messages = Rc::clone(&port.messages);
        let codex = FakeCodex::with_threads([thread('A')]);
        let pump = codex.clone();
        let mut worker = test_worker(worker_config(1), port, codex);
        register(&mut worker).await;
        assert!(
            !worker.work_drained(),
            "a new Worker waits for its first job"
        );
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage({
                    let mut message = dispatch('A', product_scope('A'));
                    message.job.execution_profile = "codex-chat".into();
                    message.job.workspace.write_mode = ExecutionWorkspaceWriteMode::Candidate;
                    message
                }),
                now(),
            )
            .await
            .unwrap();
        let active = worker.active_jobs()[0].clone();
        assert!(
            !worker.work_drained(),
            "running work keeps the Worker alive"
        );
        let checkout = pump.workspace(&active.codex_thread_id);
        let common = Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()
            .expect("common Git directory");
        assert!(common.status.success());
        let common = PathBuf::from(String::from_utf8(common.stdout).expect("Git path").trim());
        let user_file = common
            .parent()
            .expect("source repository")
            .join("user-kept.txt");
        std::fs::write(&user_file, "user edit").expect("unrelated user file");
        std::fs::write(checkout.join("candidate.txt"), b"candidate\n")
            .expect("write candidate in the Worker-owned checkout");
        let injected = ArtifactReference {
            artifact_id: ArtifactId(id("art", 'Z')),
            digest: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        };
        pump.queue_poll(
            &active.codex_thread_id,
            Ok(CodexPoll::Completed(CodexTurnCompletion {
                summary: secret_safe_runtime_summary("writer completed").unwrap(),
                artifacts: vec![injected],
                usage: Some(measured_completion_usage()),
            })),
        );
        let rejected = worker
            .poll_codex_boxed()
            .await
            .expect_err("writer cannot inject an unacknowledged reference");
        assert_eq!(rejected.code, WorkerErrorCode::CandidateArtifactMismatch);
        assert_no_outcome(&messages);

        pump.queue_poll(
            &active.codex_thread_id,
            Ok(if failed {
                CodexPoll::Failed(
                    secret_safe_runtime_summary("model failed after writing files").unwrap(),
                )
            } else {
                CodexPoll::Completed(CodexTurnCompletion {
                    summary: secret_safe_runtime_summary("writer completed").unwrap(),
                    artifacts: Vec::new(),
                    usage: Some(measured_completion_usage()),
                })
            }),
        );
        worker.poll_codex_boxed().await.unwrap();
        let artifact = observed_candidate_reference(&messages);
        assert_eq!(
            messages
                .borrow()
                .iter()
                .filter(|message| matches!(
                    message,
                    ExecutionPortMessage::ArtifactOpenMessage(_)
                        | ExecutionPortMessage::ArtifactChunkMessage(_)
                ))
                .count(),
            2
        );
        assert_no_outcome(&messages);

        let mut wrong = candidate_ack(&active, &artifact, 0, 'W');
        wrong.artifact_id = ArtifactId(id("art", 'W'));
        let rejected = worker
            .accept_control(&ExecutionPortMessage::ArtifactAckMessage(wrong), now())
            .await
            .expect_err("foreign Artifact ack");
        assert_eq!(rejected.code, WorkerErrorCode::CandidateArtifactMismatch);
        assert_no_outcome(&messages);

        acknowledge_candidate(&mut worker, &active, &artifact, 0, 'O')
            .await
            .expect("open acknowledgement");
        assert_no_outcome(&messages);
        assert!(
            !worker.work_drained(),
            "artifacts must reach the Server before exit"
        );
        acknowledge_candidate(&mut worker, &active, &artifact, 1, 'F')
            .await
            .expect("final candidate acknowledgement");

        let outcomes = observed_outcomes(&messages);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].outcome.artifacts, vec![artifact]);
        assert_eq!(
            outcomes[0].outcome.status,
            if failed {
                ExecutionOutcomeStatus::Failed
            } else {
                ExecutionOutcomeStatus::Succeeded
            }
        );
        assert!(worker.active_jobs().is_empty());
        assert!(
            worker.work_drained(),
            "success and failure both release the managed Worker"
        );
        assert!(
            !checkout.exists(),
            "temporary checkout is removed after acknowledgement"
        );
        let refs = Command::new("git")
            .arg("--git-dir")
            .arg(&common)
            .args([
                "for-each-ref",
                "--format=%(refname)",
                "refs/winwincode/candidates/",
            ])
            .output()
            .expect("retained candidate");
        assert!(refs.status.success());
        let reference = String::from_utf8(refs.stdout).expect("ref name");
        let content = Command::new("git")
            .arg("--git-dir")
            .arg(&common)
            .args(["show", &format!("{}:candidate.txt", reference.trim())])
            .output()
            .expect("saved source after cleanup");
        assert!(content.status.success());
        assert_eq!(content.stdout, b"candidate\n");
        assert_eq!(
            std::fs::read_to_string(&user_file).expect("user file remains"),
            "user edit"
        );
    }
}

#[tokio::test]
async fn duplicate_final_candidate_ack_after_outcome_send_loss_replays_one_original_outcome() {
    let (workspaces, sources) = test_workspace_paths();
    let root = workspaces
        .parent()
        .expect("workspace fixture root")
        .to_path_buf();
    let first_port = RecordingPort::default();
    let failures = Rc::clone(&first_port.failures_remaining);
    let first_messages = Rc::clone(&first_port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut first = WorkerMain::new(
        worker_config(1),
        first_port,
        codex,
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("first workspace runtime"),
    );
    register(&mut first).await;
    first
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .expect("writer dispatch");
    let active = first.active_jobs()[0].clone();
    std::fs::write(
        pump.workspace(&active.codex_thread_id)
            .join("candidate.txt"),
        b"candidate\n",
    )
    .expect("write candidate");
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    first.poll_codex_boxed().await.expect("retain candidate");
    let artifact = observed_candidate_reference(&first_messages);
    acknowledge_candidate(&mut first, &active, &artifact, 0, 'O')
        .await
        .expect("ack candidate open");
    let final_ack = candidate_ack(&active, &artifact, 1, 'F');
    failures.set(1);
    first
        .accept_control(
            &ExecutionPortMessage::ArtifactAckMessage(final_ack.clone()),
            now(),
        )
        .await
        .expect("final ACK is consumed before outcome transport");
    let failure = first
        .flush_durable_outbox()
        .await
        .expect_err("outcome transport remains retryable");
    assert_eq!(failure.code, WorkerErrorCode::ExecutionPort);
    assert_no_outcome(&first_messages);

    let (_, recovered_codex) = first.into_parts();
    let restart_port = RecordingPort::default();
    let restart_messages = Rc::clone(&restart_port.messages);
    let mut restarted = WorkerMain::new(
        worker_config(1),
        restart_port,
        recovered_codex,
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("restart workspace runtime"),
    );
    restarted
        .accept_control(
            &ExecutionPortMessage::ArtifactAckMessage(final_ack.clone()),
            now(),
        )
        .await
        .expect("restart-first duplicate final ACK consumes retained receipt");
    restarted
        .flush_durable_outbox()
        .await
        .expect("driver flushes retained outcome");
    restarted
        .accept_control(&ExecutionPortMessage::ArtifactAckMessage(final_ack), now())
        .await
        .expect("second duplicate final ACK is a no-op");
    let outcomes = observed_outcomes(&restart_messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome.artifacts, vec![artifact]);
    assert!(restarted.active_jobs().is_empty());
    std::fs::remove_dir_all(root).expect("remove workspace fixture");
}

#[tokio::test]
async fn accepted_candidate_survives_outcome_retention_restart_before_workspace_cleanup() {
    let (workspaces, sources) = test_workspace_paths();
    let root = workspaces
        .parent()
        .expect("workspace fixture root")
        .to_path_buf();
    let first_port = RecordingPort::default();
    let first_messages = Rc::clone(&first_port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut first = WorkerMain::new(
        worker_config(1),
        first_port,
        codex,
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("first workspace runtime"),
    );
    register(&mut first).await;
    let dispatch = writer_dispatch('A');
    first
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            now(),
        )
        .await
        .expect("first writer dispatch");
    let active = first.active_jobs()[0].clone();
    let checkout = pump.workspace(&active.codex_thread_id);
    std::fs::write(checkout.join("candidate.txt"), b"candidate\n")
        .expect("write candidate in the Worker-owned checkout");
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    first.poll_codex_boxed().await.expect("retain candidate");
    let artifact = observed_candidate_reference(&first_messages);
    acknowledge_candidate(&mut first, &active, &artifact, 0, 'O')
        .await
        .expect("ack candidate open");

    // Stop after the candidate ledger commits but before the terminal outcome
    // is retained.  The workspace must stay durable for the replacement.
    pump.state
        .lock()
        .expect("FakeCodex state")
        .failures
        .insert(FailurePoint::RetainOutcome);
    let final_ack = candidate_ack(&active, &artifact, 1, 'F');
    first
        .accept_control(&ExecutionPortMessage::ArtifactAckMessage(final_ack), now())
        .await
        .expect_err("inject the outcome-retention stop");
    assert!(
        checkout.is_dir(),
        "outcome failure must not consume checkout"
    );
    drop(first);

    pump.state
        .lock()
        .expect("FakeCodex state")
        .threads
        .push_back(thread('A'));
    let restart_port = RecordingPort::default();
    let restart_messages = Rc::clone(&restart_port.messages);
    let mut restarted = WorkerMain::new(
        worker_config(1),
        restart_port,
        pump.clone(),
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("restarted workspace runtime"),
    );
    register(&mut restarted).await;
    restarted
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(dispatch), now())
        .await
        .expect("recovered writer dispatch");
    assert_eq!(
        pump.workspace(&thread('A')),
        checkout,
        "recovery must bind the original detached checkout"
    );
    assert_eq!(
        std::fs::read(checkout.join("candidate.txt")).expect("read recovered candidate"),
        b"candidate\n"
    );
    pump.queue_poll(
        &thread('A'),
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    restarted
        .poll_codex_boxed()
        .await
        .expect("recover accepted candidate before cleanup");
    let outcomes = observed_outcomes(&restart_messages);
    assert_eq!(
        outcomes.len(),
        1,
        "calls={:?} messages={:#?}",
        pump.calls(),
        restart_messages.borrow()
    );
    assert_eq!(outcomes[0].outcome.artifacts, vec![artifact]);
    assert!(
        !checkout.exists(),
        "terminal recovery must consume checkout"
    );
    std::fs::remove_dir_all(root).expect("remove workspace fixture");
}

#[tokio::test]
async fn writer_restart_defers_and_replays_the_original_candidate_until_completion_recovers() {
    let (workspaces, sources) = test_workspace_paths();
    let root = workspaces
        .parent()
        .expect("workspace fixture root")
        .to_path_buf();
    let first_port = RecordingPort::default();
    let first_messages = Rc::clone(&first_port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut first = WorkerMain::new(
        worker_config(1),
        first_port,
        codex,
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("first workspace runtime"),
    );
    register(&mut first).await;
    let dispatch = writer_dispatch('A');
    first
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            now(),
        )
        .await
        .expect("first writer dispatch");
    let active = first.active_jobs()[0].clone();
    std::fs::write(
        pump.workspace(&active.codex_thread_id)
            .join("candidate.txt"),
        b"candidate\n",
    )
    .expect("write candidate in the Worker-owned checkout");
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    first
        .poll_codex_boxed()
        .await
        .expect("auto-retain candidate before crash");
    let original = observed_candidate_messages(&first_messages);
    assert_eq!(original.len(), 2);

    let (_, recovered_codex) = first.into_parts();
    recovered_codex
        .state
        .lock()
        .expect("FakeCodex state")
        .threads
        .push_back(thread('A'));
    let restart_port = RecordingPort::default();
    let restart_messages = Rc::clone(&restart_port.messages);
    let mut restarted = WorkerMain::new(
        worker_config(1),
        restart_port,
        recovered_codex,
        JobWorkspaceRuntime::open(&workspaces, &sources).expect("restarted workspace runtime"),
    );
    register(&mut restarted).await;
    restarted
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(dispatch), now())
        .await
        .expect("recovered writer dispatch");
    assert_no_candidate_product(&restart_messages);

    pump.queue_poll(
        &thread('A'),
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    restarted
        .poll_codex_boxed()
        .await
        .expect("recover completion before replay");
    assert_eq!(observed_candidate_messages(&restart_messages), original);
    assert_no_outcome(&restart_messages);

    let artifact = observed_candidate_reference(&restart_messages);
    acknowledge_candidate(&mut restarted, &active, &artifact, 0, 'O')
        .await
        .expect("recovered open acknowledgement");
    acknowledge_candidate(&mut restarted, &active, &artifact, 1, 'F')
        .await
        .expect("recovered final acknowledgement");
    let outcomes = observed_outcomes(&restart_messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome.artifacts, vec![artifact]);
    std::fs::remove_dir_all(root).expect("remove workspace fixture");
}

#[tokio::test]
async fn cancelling_a_writer_before_completion_emits_no_candidate_product() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    worker
        .accept_control(
            &ExecutionPortMessage::JobCancelMessage(cancel_for(&active, 'C')),
            now(),
        )
        .await
        .expect("cancel held writer");
    assert_no_candidate_product(&messages);
    assert_no_outcome(&messages);

    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer stopped").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    worker.poll_codex_boxed().await.unwrap();
    let outcome = messages
        .borrow()
        .iter()
        .find_map(|message| match message {
            ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome.clone()),
            _ => None,
        })
        .expect("cancelled outcome");
    assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Cancelled);
    assert!(outcome.outcome.artifacts.is_empty());
    assert_no_candidate_product(&messages);
}

#[tokio::test]
async fn cancelling_delegated_job_rejects_late_batch_after_interrupt_failure() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let workspaces = test_workspaces().with_change_batch_executor(AppliedBatchExecutor {
        calls: Arc::clone(&calls),
    });
    let mut worker = WorkerMain::new(worker_config(1), port, codex, workspaces);
    register(&mut worker).await;
    let mut delegated = writer_dispatch('A');
    delegated.job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
    worker
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(delegated), now())
        .await
        .expect("dispatch delegated writer");
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    pump.state
        .lock()
        .expect("FakeCodex state")
        .failures
        .insert(FailurePoint::Interrupt);
    worker
        .accept_control(
            &ExecutionPortMessage::JobCancelMessage(cancel_for(&active, 'C')),
            now(),
        )
        .await
        .expect("accept cancellation even when the first Core interrupt fails");
    assert_eq!(
        worker.active_jobs()[0].lifecycle,
        winwincode_worker::ActiveJobLifecycle::Cancelling
    );

    let late_proposal = CodexPoll::ChangeBatchProposed(Box::new(ChangeBatchProposalEvent {
        identity,
        occurred_at: now(),
        proposal: ChangeBatchProposal {
            acceptance_criteria_ids: vec!["crt_00000000000000000000000001".to_owned()],
            disposition: ChangeBatchProposalDisposition::Final,
            patch: DELEGATED_PATCH.to_owned(),
            schema_version: 1,
            validation_profile: ValidationProfileName::Changed,
        },
    }));
    pump.queue_poll(&active.codex_thread_id, Ok(late_proposal.clone()));
    worker
        .poll_codex_boxed()
        .await
        .expect_err("failed interrupt retry is reported without applying the late batch");
    assert!(calls.lock().expect("batch calls").is_empty());
    assert_no_candidate_product(&messages);
    assert_no_outcome(&messages);

    pump.state
        .lock()
        .expect("FakeCodex state")
        .failures
        .remove(&FailurePoint::Interrupt);
    pump.queue_poll(&active.codex_thread_id, Ok(late_proposal));
    worker
        .poll_codex_boxed()
        .await
        .expect("interrupt retry fences the late delegated batch");
    assert!(calls.lock().expect("batch calls").is_empty());
    assert_no_candidate_product(&messages);
    assert_no_outcome(&messages);

    let diagnostic = ArtifactReference {
        artifact_id: ArtifactId(id("art", 'D')),
        digest: Sha256Digest(format!("sha256:{}", "d".repeat(64))),
    };
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::CancelledWithDiagnostics(
            secret_safe_runtime_summary("embedded Codex turn cancelled").unwrap(),
            vec![diagnostic.clone()],
        )),
    );
    worker
        .poll_codex_boxed()
        .await
        .expect("cancelled terminal wins over every late delegated result");
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].outcome.status,
        ExecutionOutcomeStatus::Cancelled
    );
    assert_eq!(outcomes[0].outcome.artifacts, vec![diagnostic]);
    assert!(calls.lock().expect("batch calls").is_empty());
}

#[tokio::test]
async fn failed_candidate_cancel_is_retryable_and_never_flushes_a_post_cancel_artifact() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .expect("writer dispatch");
    let active = worker.active_jobs()[0].clone();
    std::fs::write(
        pump.workspace(&active.codex_thread_id)
            .join("candidate.txt"),
        b"candidate\n",
    )
    .expect("write candidate");
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    worker.poll_codex_boxed().await.expect("retain candidate");
    messages.borrow_mut().clear();

    pump.fail_next_candidate_cancel();
    let cancel = cancel_for(&active, 'C');
    let failure = worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobCancelMessage(cancel.clone()),
            now(),
        )
        .await
        .expect_err("first durable candidate cancellation fails");
    assert_eq!(failure.code, WorkerErrorCode::UnexpectedMessage);
    assert_eq!(
        worker.active_jobs()[0].lifecycle,
        winwincode_worker::ActiveJobLifecycle::Cancelling
    );
    worker
        .heartbeat(now())
        .await
        .expect("heartbeat cannot flush a Cancelling candidate");
    assert_no_candidate_product(&messages);

    worker
        .accept_control_and_drive(&ExecutionPortMessage::JobCancelMessage(cancel), now())
        .await
        .expect("repeated cancel retries the durable candidate ledger");
    assert_no_candidate_product(&messages);
    assert!(messages.borrow().iter().any(|message| matches!(
        message,
        ExecutionPortMessage::JobCancelAckMessage(ack)
            if ack.status == JobCancelAckMessageStatus::AlreadyCancelling
    )));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("writer stopped").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    worker
        .poll_codex_boxed()
        .await
        .expect("cancelled writer reaches one terminal outcome");
    assert_no_candidate_product(&messages);
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].outcome.status,
        ExecutionOutcomeStatus::Cancelled
    );
    assert!(outcomes[0].outcome.artifacts.is_empty());
}

#[tokio::test]
async fn runtime_trace_mismatch_identifies_sequence_and_authority_without_payloads() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let trace = RuntimeEventMessage {
        codex_thread_id: active.codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Activity,
            event_id: ExecutionEventId(id("evt", 'I')),
            occurred_at: now(),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "PRIVATE_TRACE_CANARY".to_owned(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: active.lease.clone(),
        message_id: ExecutionMessageId(id("msg", 'I')),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        session_identity: active.session_identity.clone(),
        worker_session_id: active.worker_session_id.clone(),
    };
    pump.queue_poll(
        &thread('A'),
        Ok(CodexPoll::RuntimeTrace(Box::new(trace.clone()))),
    );
    worker.poll_codex_boxed().await.unwrap();
    let delivered = messages.borrow().len();
    let mut gap = trace.clone();
    gap.event.sequence = ExecutionSequence(3);
    gap.message_id = ExecutionMessageId(id("msg", 'G'));
    gap.event.event_id = ExecutionEventId(id("evt", 'G'));
    pump.queue_poll(&thread('A'), Ok(CodexPoll::RuntimeTrace(Box::new(gap))));
    let error = worker.poll_codex_boxed().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::RuntimeTraceMismatch);
    assert_eq!(
        error.reason,
        "runtime trace differs from active Job: trace_next=2 trace_observed=3 trace_lease=true trace_worker=true trace_session=true trace_thread=true"
    );
    assert_eq!(
        messages.borrow().len(),
        delivered,
        "a sequence gap remains rejected"
    );
    let mut foreign = trace;
    foreign.event.sequence = ExecutionSequence(2);
    foreign.worker_session_id.0 = id("wsn", 'Z');
    pump.queue_poll(&thread('A'), Ok(CodexPoll::RuntimeTrace(Box::new(foreign))));
    let error = worker.poll_codex_boxed().await.unwrap_err();
    assert_eq!(error.code, WorkerErrorCode::RuntimeTraceMismatch);
    assert_eq!(
        error.reason,
        "runtime trace differs from active Job: trace_next=2 trace_observed=2 trace_lease=true trace_worker=false trace_session=true trace_thread=true"
    );
    assert!(!error.reason.contains("PRIVATE_TRACE_CANARY"));
    assert!(!error.reason.contains(&active.worker_session_id.0));
    assert_eq!(
        messages.borrow().len(),
        delivered,
        "foreign authority remains rejected"
    );
}

#[tokio::test]
async fn durable_codex_infrastructure_terminal_emits_stopped_before_one_outcome() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    pump.queue_poll(
        &thread('A'),
        Ok(CodexPoll::RuntimeTrace(Box::new(RuntimeEventMessage {
            codex_thread_id: active.codex_thread_id.clone(),
            event: ExecutionEventRecord {
                category: ExecutionEventCategory::Lifecycle,
                event_id: ExecutionEventId(id("evt", 'I')),
                occurred_at: now(),
                payload: None,
                sequence: ExecutionSequence(1),
                summary: "embedded Codex infrastructure failure".to_owned(),
            },
            kind: RuntimeEventMessageKind::RuntimeEvent,
            lease: active.lease,
            message_id: ExecutionMessageId(id("msg", 'I')),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now(),
            session_identity: active.session_identity,
            worker_session_id: active.worker_session_id,
        }))),
    );
    pump.queue_poll(
        &thread('A'),
        Ok(CodexPoll::InfrastructureFailed(
            secret_safe_runtime_summary("embedded Codex infrastructure failure").unwrap(),
        )),
    );

    worker.poll_codex_boxed().await.unwrap();
    worker.poll_codex_boxed().await.unwrap();

    let captured = messages.borrow();
    let tail = output_kinds(&captured)[captured.len() - 2..].to_vec();
    assert_eq!(tail, ["runtime", "outcome"]);
    let outcome = captured
        .iter()
        .find_map(|message| match message {
            ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        outcome.outcome.status,
        ExecutionOutcomeStatus::InfrastructureError
    );
    assert_eq!(
        outcome.outcome.summary,
        "embedded Codex infrastructure failure"
    );
    assert_eq!(outcome.outcome.last_event_sequence, ExecutionAckSequence(1));
}

#[tokio::test]
async fn recovered_delegated_stop_finishes_before_observer_or_core_poll() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .expect("dispatch recovered stop fixture");
    let active = worker.active_jobs()[0].clone();
    pump.set_delegated_stop(
        &active.codex_thread_id,
        DelegatedLoopStopFact {
            batch_id: ChangeBatchId(id("bat", 'S')),
            reason: RepairLoopStopReason::WallTimeLimitReached,
            counters: RepairLoopCounters {
                change_batches: 1,
                context_pack_bytes: 1_024,
                elapsed_millis: 3_600_000,
                observer_calls: 1,
                primary_model_calls: 1,
                repair_rounds: 0,
                total_cost_microunits: 1,
                total_tokens: 1,
            },
            stopped_at: now(),
        },
    );

    worker
        .poll_codex_boxed()
        .await
        .expect("finish the durable delegated stop");

    assert_eq!(worker.active_jobs().len(), 0);
    assert_eq!(
        pump.calls()
            .iter()
            .filter(|call| call.starts_with("poll:"))
            .count(),
        0,
        "the durable stop is a barrier before another Core turn"
    );
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome.status, ExecutionOutcomeStatus::Failed);
    assert_eq!(
        outcomes[0].outcome.summary,
        "delegated wall-time limit reached"
    );
}

#[tokio::test]
async fn graceful_shutdown_drains_jobs_and_reports_codex_shutdown_failures() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A'), thread('B')]);
    codex
        .state
        .lock()
        .expect("FakeCodex state")
        .failures
        .extend([FailurePoint::Interrupt, FailurePoint::Shutdown]);
    let mut worker = test_worker(worker_config(2), port, codex);
    register(&mut worker).await;
    for suffix in ['A', 'B'] {
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch(suffix, delivery_scope(suffix))),
                now(),
            )
            .await
            .unwrap();
    }

    let report = worker.shutdown(now()).await.unwrap();

    assert_eq!(worker.lifecycle(), WorkerLifecycleState::Stopped);
    assert_eq!(report.cancelled_jobs.len(), 2);
    assert_eq!(report.codex_failures, 3);
    assert_eq!(
        messages
            .borrow()
            .iter()
            .filter(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)))
            .count(),
        2
    );
}

#[tokio::test]
async fn product_session_dispatch_binds_without_a_stage_run() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let calls = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;

    worker
        .accept_control_and_drive(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', product_scope('A'))),
            now(),
        )
        .await
        .unwrap();

    let binding = messages
        .borrow()
        .iter()
        .find_map(|message| match message {
            ExecutionPortMessage::SessionBindingMessage(binding) => Some(binding.clone()),
            _ => None,
        })
        .expect("ProductSession dispatch emits a SessionBinding");
    assert!(binding.work_run_id.is_none());
    assert!(binding.session_identity.work_run_id.is_none());
    assert_eq!(worker.active_jobs().len(), 1);
    assert_eq!(
        calls.calls(),
        [
            format!(
                "ensure:{}:1:{}",
                id("job", 'A'),
                binding.worker_session_id.0
            ),
            format!("submit:{}", thread('A').0),
        ]
    );
}

#[tokio::test]
async fn a_lost_binding_response_resumes_the_prepared_turn_once() {
    let port = RecordingPort::default();
    let failures = Rc::clone(&port.failures_remaining);
    let codex = FakeCodex::with_threads([thread('A')]);
    let calls = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    failures.set(2);
    let message = ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A')));
    worker
        .accept_control_and_drive(&message, now())
        .await
        .expect_err("binding response unavailable");
    worker
        .poll_codex(now())
        .await
        .expect_err("one refused batch yields instead of retrying in the same poll");
    assert_eq!(failures.get(), 0);
    worker
        .poll_codex(now())
        .await
        .expect("poll without duplicate submission");
    assert_eq!(
        calls
            .calls()
            .iter()
            .filter(|call| call.starts_with("submit:"))
            .count(),
        1
    );
}

#[tokio::test]
async fn pending_runtime_and_diagnostic_artifact_flush_after_server_restart_window() {
    // Models the RUN-03 residual: Core produces verification evidence while
    // the Worker→Server exchange is down (Server restart / Connection
    // refused). After the in-memory Job is gone, the durable outbox must
    // still flush runtime.event + diagnostic artifact.chunk + JobOutcome on
    // the same exchange protocol when the Server returns.
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let failures = port.failures_handle();
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;

    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let trace = RuntimeEventMessage {
        codex_thread_id: active.codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Command,
            event_id: ExecutionEventId(id("evt", 'V')),
            occurred_at: now(),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "verification command produced direct evidence".to_owned(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: active.lease.clone(),
        message_id: ExecutionMessageId(id("msg", 'V')),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        session_identity: active.session_identity.clone(),
        worker_session_id: active.worker_session_id.clone(),
    };
    // Diagnostic (non-candidate) artifact chunk — the exact class that used to
    // be skipped by `_ => continue` once the in-memory Job was gone.
    let diagnostic_chunk = ExecutionPortMessage::ArtifactChunkMessage(ArtifactChunkMessage {
        artifact_id: ArtifactId(id("art", 'D')),
        is_final: true,
        kind: ArtifactChunkMessageKind::ArtifactChunk,
        lease: active.lease.clone(),
        message_id: ExecutionMessageId(id("msg", 'D')),
        payload: EncodedPayload {
            content_type: "text/plain; charset=utf-8".to_owned(),
            data_base64: "dmVyaWZ5Cg==".to_owned(),
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(b"verify\n"))),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        sequence: ExecutionSequence(1),
        session_identity: active.session_identity.clone(),
        snapshot_id: None,
        worker_session_id: active.worker_session_id.clone(),
    });

    // Simulate Server restart: every outbound send fails while Core finishes.
    failures.set(usize::MAX);
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::RuntimeTrace(Box::new(trace))),
    );
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("Codex turn completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    pump.inject_pending_delivery(&diagnostic_chunk);

    let first = worker.poll_codex_boxed().await;
    assert!(
        first.is_err(),
        "runtime send must fail while Server is down"
    );
    let second = worker.poll_codex_boxed().await;
    assert!(
        second.is_err(),
        "terminal outcome send must fail while Server is down"
    );
    assert!(
        worker.active_jobs().is_empty(),
        "finish_job must drop the in-memory Job even when transport fails"
    );
    assert!(
        worker.has_pending_durable_evidence(),
        "durable evidence must remain pending after a failed Server exchange"
    );
    assert!(
        !worker.work_drained(),
        "Worker must not report drained while evidence is pending"
    );
    let pending_before = pump.pending_kinds();
    assert!(
        pending_before.iter().any(|kind| kind == "runtime.event"),
        "runtime.event must stay pending, got {pending_before:?}"
    );
    assert!(
        pending_before.iter().any(|kind| kind == "artifact.chunk"),
        "diagnostic artifact.chunk must stay pending, got {pending_before:?}"
    );
    assert!(
        pending_before.iter().any(|kind| kind == "job.outcome"),
        "JobOutcome must stay pending, got {pending_before:?}"
    );

    // Server returns: heartbeat/poll flush must send every pending evidence
    // frame even though the in-memory Job is already gone.
    failures.set(0);
    worker
        .flush_durable_outbox()
        .await
        .expect("drive flush after Server restart");
    worker
        .heartbeat(now())
        .await
        .expect("heartbeat after Server restart");
    let pending_after = pump.pending_kinds();
    assert!(
        !pending_after.iter().any(|kind| matches!(
            kind.as_str(),
            "runtime.event" | "artifact.chunk" | "job.outcome"
        )),
        "pending evidence must flush after Server returns, still pending {pending_after:?}"
    );
    assert!(
        worker.work_drained(),
        "Worker may drain only after durable evidence flushed"
    );

    let captured = messages.borrow();
    assert!(
        captured.iter().any(|message| matches!(
            message,
            ExecutionPortMessage::RuntimeEventMessage(event)
                if event.event.summary.contains("verification command")
        )),
        "runtime.event must reach the Server port after reconnect"
    );
    assert!(
        captured.iter().any(|message| matches!(
            message,
            ExecutionPortMessage::ArtifactChunkMessage(chunk)
                if chunk.payload.content_type.starts_with("text/plain")
        )),
        "diagnostic artifact.chunk must reach the Server port after reconnect"
    );
    assert!(
        captured
            .iter()
            .any(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_))),
        "JobOutcome must reach the Server port after reconnect"
    );
}

#[tokio::test]
async fn forward_runtime_trace_retains_and_sends_when_in_memory_job_is_gone() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let failures = port.failures_handle();
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let lease = active.lease.clone();
    let session_identity = active.session_identity.clone();
    let worker_session_id = active.worker_session_id.clone();
    let codex_thread_id = active.codex_thread_id.clone();

    // Drop the Job while transport is down (finish without a live Job).
    failures.set(usize::MAX);
    pump.queue_poll(
        &codex_thread_id,
        Ok(CodexPoll::InfrastructureFailed(
            secret_safe_runtime_summary("embedded Codex infrastructure failure").unwrap(),
        )),
    );
    let _ = worker.poll_codex_boxed().await;
    assert!(worker.active_jobs().is_empty());

    // A late Core runtime frame for the already-closed Job must still retain
    // and send once the Server is reachable — not hard-fail with
    // RuntimeTraceMismatch and never leave the process.
    let late = RuntimeEventMessage {
        codex_thread_id: codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Command,
            event_id: ExecutionEventId(id("evt", 'L')),
            occurred_at: now(),
            payload: None,
            sequence: ExecutionSequence(2),
            summary: "late verification runtime frame".to_owned(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: lease.clone(),
        message_id: ExecutionMessageId(id("msg", 'L')),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
        session_identity,
        worker_session_id,
    };
    pump.queue_execution_message(ExecutionPortMessage::RuntimeEventMessage(late));
    // poll_codex requires Active lifecycle; the Worker is still Active after
    // finish_job. Transport is still down, so the late frame stays pending.
    let _ = worker.poll_codex_boxed().await;
    assert!(
        worker.has_pending_durable_evidence(),
        "late runtime frame for a gone Job must be retained durably"
    );

    failures.set(0);
    worker
        .flush_durable_outbox()
        .await
        .expect("flush late runtime frame after Server returns");
    assert!(
        messages.borrow().iter().any(|message| matches!(
            message,
            ExecutionPortMessage::RuntimeEventMessage(event)
                if event.event.summary.contains("late verification")
        )),
        "late runtime frame must flush after Server returns"
    );
}

#[tokio::test]
async fn lease_renewal_extends_current_attempt_without_restarting_core() {
    use winwincode_execution_port::generated::{LeaseRenewMessage, LeaseRenewMessageKind};

    let port = RecordingPort::default();
    let codex = FakeCodex::with_threads([thread('A')]);
    let calls = codex.clone();
    let mut worker = test_worker(worker_config(1), port.clone(), codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .expect("dispatch");
    assert_eq!(worker.active_jobs().len(), 1);
    let before = worker.active_jobs()[0].clone();
    let initial_calls = calls.calls();
    let mut extended = before.lease.clone();
    extended.expires_at = Instant("2027-01-15T08:10:00.000Z".into());
    let renewal = ExecutionPortMessage::LeaseRenewMessage(LeaseRenewMessage {
        kind: LeaseRenewMessageKind::LeaseRenew,
        lease: extended.clone(),
        message_id: ExecutionMessageId(id("xmsg", 'R')),
        prior_expires_at: before.lease.expires_at.clone(),
        request_id: RequestId(id("req", 'R')),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now(),
    });
    assert_invalid_renewals(&mut worker, &renewal, &before).await;
    worker
        .accept_control(&renewal, now())
        .await
        .expect("current lease renewal must be handled");
    worker
        .accept_control(&renewal, now())
        .await
        .expect("exact renewal replay is idempotent");
    assert_eq!(worker.active_jobs().len(), 1);
    let current = worker.active_jobs()[0];
    assert_eq!(current.lease, extended);
    assert_eq!(current.job, before.job);
    assert_eq!(current.codex_thread_id, before.codex_thread_id);
    assert_eq!(current.session_identity, before.session_identity);
    assert_eq!(calls.calls(), initial_calls);
    worker.heartbeat(now()).await.expect("renewed heartbeat");
    assert!(port.messages.borrow().iter().any(|message| matches!(message,
        ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat)
        if heartbeat.active_leases.iter().any(|lease| lease.expires_at == extended.expires_at)
    )));
}

async fn assert_invalid_renewals(
    worker: &mut WorkerMain<RecordingPort, FakeCodex>,
    message: &ExecutionPortMessage,
    before: &winwincode_worker::ActiveJob,
) {
    use winwincode_execution_port::generated::LeaseRenewMessage;
    let ExecutionPortMessage::LeaseRenewMessage(renewal) = message else {
        panic!("renewal");
    };
    let mutations: [fn(&mut LeaseRenewMessage); 9] = [
        |value| value.lease.worker_instance_id.0 = id("wki", 'Z'),
        |value| value.lease.worker_id.0 = id("wrk", 'Z'),
        |value| value.lease.attempt += 1,
        |value| "99".clone_into(&mut value.lease.fencing_token.0),
        |value| value.lease.lease_id.0 = id("lse", 'Z'),
        |value| "2027-01-15T08:04:00.000Z".clone_into(&mut value.prior_expires_at.0),
        |value| "2027-01-15T08:03:00.000Z".clone_into(&mut value.lease.expires_at.0),
        |value| "2027-01-15T08:06:00.000Z".clone_into(&mut value.sent_at.0),
        |value| "invalid".clone_into(&mut value.lease.expires_at.0),
    ];
    for mutate in mutations {
        let mut invalid = renewal.clone();
        mutate(&mut invalid);
        assert!(
            worker
                .accept_control(&ExecutionPortMessage::LeaseRenewMessage(invalid), now())
                .await
                .is_err()
        );
        assert_eq!(worker.active_jobs()[0], before);
    }
    assert!(
        worker
            .accept_control(message, before.lease.expires_at.clone())
            .await
            .is_err()
    );
    assert_eq!(worker.active_jobs()[0], before);
}

#[tokio::test]
async fn unsuccessful_outcomes_retain_known_usage_without_changing_status() {
    for status in [
        ExecutionOutcomeStatus::Failed,
        ExecutionOutcomeStatus::Cancelled,
        ExecutionOutcomeStatus::InfrastructureError,
    ] {
        let port = RecordingPort::default();
        let messages = Rc::clone(&port.messages);
        let codex = FakeCodex::with_threads([thread('A')]);
        let pump = codex.clone();
        let mut usage = measured_completion_usage();
        usage.cost_microunits = None;
        pump.state
            .lock()
            .unwrap()
            .retained_usage
            .insert(thread('A').0, usage.clone());
        let mut worker = test_worker(worker_config(1), port, codex);
        register(&mut worker).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
                now(),
            )
            .await
            .unwrap();
        let summary = secret_safe_runtime_summary("unsuccessful measured run").unwrap();
        let poll = match status {
            ExecutionOutcomeStatus::Failed => CodexPoll::Failed(summary),
            ExecutionOutcomeStatus::Cancelled => CodexPoll::Cancelled(summary),
            ExecutionOutcomeStatus::InfrastructureError => CodexPoll::InfrastructureFailed(summary),
            ExecutionOutcomeStatus::Succeeded => unreachable!(),
        };
        pump.queue_poll(&thread('A'), Ok(poll));
        worker.poll_codex_boxed().await.unwrap();
        let outcomes = observed_outcomes(&messages);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].outcome.status, status);
        assert_eq!(outcomes[0].outcome.usage, Some(usage));
    }
}

#[tokio::test]
async fn same_running_job_accepts_stop_after_lease_expiry() {
    let port = RecordingPort::default();
    let codex = FakeCodex::with_threads([thread('A')]);
    let observed = codex.clone();
    let mut worker = test_worker(worker_config(1), port, codex);
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', product_scope('A'))),
            now(),
        )
        .await
        .expect("start exact job");
    let active = worker.active_jobs()[0].clone();
    worker
        .accept_control(
            &ExecutionPortMessage::JobCancelMessage(cancel_for(&active, 'C')),
            active.lease.expires_at.clone(),
        )
        .await
        .expect("consume cancel");
    let state = observed.state.lock().unwrap();
    assert!(state.durable_deliveries.iter().any(|delivery| matches!(&delivery.message, ExecutionPortMessage::JobCancelAckMessage(ack) if ack.status == JobCancelAckMessageStatus::Accepted)));
    assert!(
        state
            .calls
            .iter()
            .any(|call| call.starts_with("interrupt:")),
        "the exact stop reaches Core"
    );
}
