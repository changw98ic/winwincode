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
    ArtifactId, ChangeBatchId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionAckSequence,
    ExecutionEventId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant,
    LeaseId, ProductSessionId, RepositoryId, RequestId, SchemaVersion, Sha256Digest, StageRunId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::change_batch_identity::derive_change_batch_id;
use winwincode_execution_port::generated::{
    AppliedFileOperation, AppliedFileSummary, ArtifactAckMessage, ArtifactAckMessageKind,
    ArtifactChunkMessage, ArtifactChunkMessageKind, ArtifactDescriptor, ArtifactKind,
    ArtifactOpenMessage, ArtifactOpenMessageKind, ArtifactReference, ChangeBatchIdentity,
    ChangeBatchProgressEvent, ChangeBatchProgressState, ChangeBatchProposal,
    ChangeBatchProposalDisposition, ChangeBatchProposalEvent,
    DeliveryStageAcceptanceCriterionInput, DeliveryStageExecutionScope,
    DeliveryStageExecutionScopeKind, DeliveryStageInput, DeliveryStageTaskInput, EncodedPayload,
    ExecutionEventCategory, ExecutionEventRecord, ExecutionJob, ExecutionJobReplacementAuthority,
    ExecutionLeaseStamp, ExecutionLimits, ExecutionOutcomeStatus, ExecutionOutcomeUsage,
    ExecutionPortMessage, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
    JobCancelAckMessageStatus, JobCancelMessage, JobCancelMessageKind, JobCancelMessageReason,
    JobDispatchMessage, JobDispatchMessageKind, JobDispatchResultMessageStatus, LeaseWriteStatus,
    ProductSessionExecutionScope, ProductSessionExecutionScopeKind, RuntimeEventMessage,
    RuntimeEventMessageKind, ValidationProfileName, WorkerCapabilityFeature, WorkerCapabilitySet,
    WorkerCapabilitySetPlatform, WorkerRegistrationResultMessage,
    WorkerRegistrationResultMessageKind, WorkerRegistrationResultMessageLeaseRecovery,
    WorkerRegistrationResultMessageStatus,
};
use winwincode_execution_port::transport::{
    ExecutionPortCore, FrameDirection, RemoteTransportAdapter, TypedFrame,
};
use winwincode_worker::{
    CandidateArtifactAckOutcome, CandidateArtifactAuthority, CandidateArtifactUpload,
    CodexCoreAdapter, CodexPoll, CodexRunKey, CodexThreadStart, CodexTurnCompletion,
    DelegatedPollOutcome, DurableExecutionDelivery, RetainedCandidateArtifact, WorkerConfig,
    WorkerErrorCode, WorkerExecutionPort, WorkerLifecycleState, WorkerMain,
    secret_safe_runtime_summary,
    workspace_runtime::{
        ChangeBatchExecutionRequest, ChangeBatchExecutionResult, ChangeBatchExecutor,
        ChangeBatchExecutorFuture, JobWorkspaceRuntime,
    },
};

const NOW: &str = "2027-01-15T08:00:02.000Z";
type TestFuture<'a, Output> = Pin<Box<dyn Future<Output = Output> + 'a>>;

#[derive(Clone, Default)]
struct RecordingPort {
    messages: Rc<RefCell<Vec<ExecutionPortMessage>>>,
    failures_remaining: Rc<Cell<usize>>,
}

impl RecordingPort {
    fn fail_once() -> Self {
        Self {
            messages: Rc::new(RefCell::new(Vec::new())),
            failures_remaining: Rc::new(Cell::new(1)),
        }
    }
}

impl WorkerExecutionPort for RecordingPort {
    type Error = ();

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        if self.failures_remaining.get() > 0 {
            self.failures_remaining
                .set(self.failures_remaining.get().saturating_sub(1));
            return std::future::ready(Err(()));
        }
        self.messages.borrow_mut().push(message);
        std::future::ready(Ok(()))
    }
}

#[derive(Default)]
struct CodexState {
    calls: Vec<String>,
    threads: VecDeque<CodexThreadId>,
    workspaces: HashMap<String, PathBuf>,
    workspace_revisions: HashMap<String, WorkspaceRevision>,
    polls: HashMap<String, VecDeque<Result<CodexPoll, ()>>>,
    failures: HashSet<FailurePoint>,
    durable_deliveries: Vec<DurableExecutionDelivery>,
    pending_delivery_ids: HashSet<String>,
    candidate_delivery_ids: HashSet<String>,
    candidate_upload: Option<CandidateArtifactUpload>,
    candidate_reference: Option<ArtifactReference>,
    accepted_candidate: Option<ArtifactReference>,
    candidate_cancel_failures_remaining: usize,
    model_open_acknowledged: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum FailurePoint {
    Ensure,
    Submit,
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

    fn ensure_thread(
        &mut self,
        start: CodexThreadStart<'_>,
    ) -> impl Future<Output = Result<CodexThreadId, Self::Error>> {
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
        };
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
        self.state
            .lock()
            .expect("FakeCodex state")
            .calls
            .push(format!("model_chunk:{}", chunk.sequence.0));
        std::future::ready(Ok(()))
    }

    fn accept_action_receipt(
        &mut self,
        _receipt: &winwincode_execution_port::generated::ActionEnforcementReceiptMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        std::future::ready(Err(()))
    }

    fn accept_approval_decision(
        &mut self,
        _decision: &winwincode_execution_port::generated::ApprovalDecisionMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        std::future::ready(Err(()))
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
            artifact: descriptor,
            kind: ArtifactOpenMessageKind::ArtifactOpen,
            lease: upload.lease.clone(),
            message_id: ExecutionMessageId(id("msg", 'O')),
            request_id: RequestId(id("req", 'O')),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: upload.created_at.clone(),
            session_identity: upload.session_identity.clone(),
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

    fn retain_job_outcome(
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
        Ok(Vec::new())
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
        tokens: 23,
        cost_microunits: 29,
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
    let delivery_stage = matches!(&scope, ExecutionScope::DeliveryStageExecutionScope(_));
    let goal = "Perform the approved fixture change.";
    JobDispatchMessage {
        job: ExecutionJob {
            attempt: 1,
            execution_profile: if delivery_stage { "planner" } else { "fixture" }.to_owned(),
            goal: goal.to_owned(),
            job_id: lease.job_id.clone(),
            limits: ExecutionLimits {
                deadline_at: Instant("2027-01-15T08:04:30.000Z".to_owned()),
                max_artifact_bytes: 1_000_000,
                max_runtime_seconds: 240,
            },
            payload_digest: Sha256Digest(format!(
                "sha256:{}",
                job_suffix.to_ascii_lowercase().to_string().repeat(64)
            )),
            scope,
            stage_input: delivery_stage.then(|| DeliveryStageInput {
                acceptance_criteria: vec![DeliveryStageAcceptanceCriterionInput {
                    criterion_id: "criterion-fixture".to_owned(),
                    description: "The fixture behavior is verified.".to_owned(),
                    required: true,
                    verification_method: Some("Run the fixture test.".to_owned()),
                }],
                candidate_ref: None,
                constraints: Vec::new(),
                delivery_spec_id: "spec-fixture".to_owned(),
                delivery_spec_revision: 1,
                goal: goal.to_owned(),
                out_of_scope: Vec::new(),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: vec!["Fixture source".to_owned()],
                task: None,
                title: "Fixture Delivery".to_owned(),
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
        sent_at: now(),
    }
}

fn writer_dispatch(job_suffix: char) -> JobDispatchMessage {
    let mut dispatch = dispatch(job_suffix, delivery_scope(job_suffix));
    let task_id = DeliveryTaskId(id("dtk", job_suffix));
    let criterion_id = "criterion-fixture".to_owned();
    let ExecutionScope::DeliveryStageExecutionScope(scope) = &mut dispatch.job.scope else {
        unreachable!("writer fixture is a Delivery stage")
    };
    scope.delivery_task_id = Some(task_id.clone());
    "executor".clone_into(&mut dispatch.job.execution_profile);
    dispatch.job.workspace.write_mode = ExecutionWorkspaceWriteMode::Candidate;
    "HEAD".clone_into(&mut dispatch.job.workspace.checkout_revision);
    let input = dispatch
        .job
        .stage_input
        .as_mut()
        .expect("writer stage input");
    input.task = Some(DeliveryStageTaskInput {
        acceptance_criterion_ids: vec![criterion_id],
        goal: dispatch.job.goal.clone(),
        task_id,
        title: "Produce candidate".to_owned(),
    });
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
    replacement.replacement_authority = Some(ExecutionJobReplacementAuthority {
        created_at: Instant("2027-01-15T08:00:59.000Z".to_owned()),
        logical_job_digest: logical_job_digest(&replacement.job),
        predecessor_lease: predecessor.lease.clone(),
        predecessor_session_identity: Some(predecessor.session_identity.clone()),
        receipt_digest: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        receipt_id: RequestId(id("req", 'Z')),
        scope: replacement.job.scope.clone(),
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
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&value).expect("logical Job bytes"))
    ))
}

fn delivery_scope(suffix: char) -> ExecutionScope {
    ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
        delivery_id: DeliveryId(id("dlv", suffix)),
        delivery_task_id: None,
        kind: DeliveryStageExecutionScopeKind::DeliveryStage,
        product_session_id: ProductSessionId(id("psn", suffix)),
        rework_authorization: None,
        stage_run_id: StageRunId(id("run", suffix)),
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
            .await
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
    fn registration_for_test(&self) -> (RequestId, WorkerId, WorkerInstanceId);
    fn poll_codex_boxed(&mut self) -> TestFuture<'_, Result<(), winwincode_worker::WorkerError>>;
}

impl WorkerTestAccess for WorkerMain<RecordingPort, FakeCodex> {
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
                .accept_control(
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
                .accept_control(
                    &routed(
                        ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                        remote,
                    ),
                    now(),
                )
                .await
                .unwrap();
            worker
                .accept_control(
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
async fn duplicate_or_conflicting_dispatch_never_creates_a_second_thread() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A'), thread('A')]);
    let calls = codex.clone();
    let mut worker = test_worker(worker_config(3), port, codex);
    register(&mut worker).await;
    let first = dispatch('A', delivery_scope('A'));
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(first.clone()),
            now(),
        )
        .await
        .unwrap();
    worker
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(first), now())
        .await
        .unwrap();
    let second = dispatch('B', delivery_scope('B'));
    worker
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(second), now())
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
        .accept_control(
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
    changed_limits.limits.max_runtime_seconds -= 1;
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
            .accept_control(&ExecutionPortMessage::JobDispatchMessage(replay), now())
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
        .accept_control(
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
            usage: measured_completion_usage(),
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
        .accept_control(
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
    restarted
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(replacement),
            now(),
        )
        .await
        .expect("accept sealed successor dispatch");
    let successor = restarted.active_jobs()[0].clone();
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
            usage: measured_completion_usage(),
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
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', delivery_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let mut wrong = cancel_for(&active, 'B');
    wrong.worker_session_id = WorkerSessionId(id("wsn", 'Z'));
    worker
        .accept_control(&ExecutionPortMessage::JobCancelMessage(wrong), now())
        .await
        .unwrap();
    let exact = cancel_for(&active, 'C');
    worker
        .accept_control(
            &ExecutionPortMessage::JobCancelMessage(exact.clone()),
            now(),
        )
        .await
        .unwrap();
    worker
        .accept_control(&ExecutionPortMessage::JobCancelMessage(exact), now())
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
            usage: measured_completion_usage(),
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
                    acceptance_criteria_ids: vec!["criterion-fixture".to_owned()],
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
                    acceptance_criteria_ids: vec!["criterion-fixture".to_owned()],
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
                    acceptance_criteria_ids: vec!["criterion-fixture".to_owned()],
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
            usage: measured_completion_usage(),
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
            usage: measured_completion_usage(),
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
            usage: measured_completion_usage(),
        })),
    );
    first.poll_codex_boxed().await.expect("retain candidate");
    let artifact = observed_candidate_reference(&first_messages);
    acknowledge_candidate(&mut first, &active, &artifact, 0, 'O')
        .await
        .expect("ack candidate open");
    let final_ack = candidate_ack(&active, &artifact, 1, 'F');
    failures.set(1);
    let failure = first
        .accept_control(
            &ExecutionPortMessage::ArtifactAckMessage(final_ack.clone()),
            now(),
        )
        .await
        .expect_err("outcome transport fails after durable retention");
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
        .expect("restart-first duplicate final ACK flushes retained outcome");
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
            usage: measured_completion_usage(),
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
            usage: measured_completion_usage(),
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
            usage: measured_completion_usage(),
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
            usage: measured_completion_usage(),
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
            usage: measured_completion_usage(),
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
async fn failed_candidate_cancel_is_retryable_and_never_flushes_a_post_cancel_artifact() {
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
            usage: measured_completion_usage(),
        })),
    );
    worker.poll_codex_boxed().await.expect("retain candidate");
    messages.borrow_mut().clear();

    pump.fail_next_candidate_cancel();
    let cancel = cancel_for(&active, 'C');
    let failure = worker
        .accept_control(
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
        .accept_control(&ExecutionPortMessage::JobCancelMessage(cancel), now())
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
            usage: measured_completion_usage(),
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
        .accept_control(
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
    assert!(binding.stage_run_id.is_none());
    assert!(binding.session_identity.stage_run_id.is_none());
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
