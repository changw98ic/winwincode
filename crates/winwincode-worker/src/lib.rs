// SPDX-License-Identifier: Apache-2.0

//! Standalone Execution Worker lifecycle.
//!
//! This crate owns process and job orchestration only. The embedded Codex Core
//! remains behind one injected [`CodexCoreAdapter`]. That adapter is also the
//! only production assembly point for the existing Action Gateway, Capability
//! Adapter, and Runtime `TraceOutbox`. Local and remote transports deliver the
//! same generated `ExecutionPortMessage` values to [`WorkerMain`].

pub mod action_enforcement;
pub mod candidate_ref;
pub mod managed_session;
pub mod remote_transport;
pub mod stage_product;
pub mod workspace_runtime;

/// Canonical types used by deployment adapters that compose this Worker.
///
/// The local launcher intentionally depends on the Worker, rather than on a
/// second copy of the execution protocol. Re-exporting the existing generated
/// messages and transport adapters keeps local and separated deployments on
/// the same typed boundary.
pub mod composition {
    pub use winwincode_domain as domain;
    pub use winwincode_domain::Instant;
    pub use winwincode_execution_port::generated;
    pub use winwincode_execution_port::generated::ExecutionPortMessage;
    pub use winwincode_execution_port::transport::{
        AdapterError, EndpointSide, ExecutionPortCore, FrameDirection, FrameError,
        LocalWorkerAdapter, RemoteTransportAdapter, TypedFrame,
    };
}

use std::collections::{HashMap, HashSet};
use std::fmt;

use sha2::{Digest, Sha256};
pub use winwincode_codex::candidate_artifact_outbox::{
    CandidateArtifactAckOutcome, CandidateArtifactAuthority, CandidateArtifactUpload,
    RetainedCandidateArtifact,
};
pub use winwincode_codex::{
    ActionRequestTransport, CodexCoreAdapter, CodexPoll, CodexRunKey, CodexThreadStart,
    CodexTurnCompletion, DurableExecutionDelivery, WorkerExecutionPort,
    secret_safe_runtime_summary,
};
use winwincode_domain::{
    CodexThreadId, ExecutionAckSequence, ExecutionEventId, ExecutionJobId, ExecutionMessageId,
    ExecutionSequence, Instant, ProductSessionId, RequestId, SchemaVersion,
    SessionBindingSourceIdentity, SessionBindingSourceIdentityKind, SessionIdentity, StageRunId,
    WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    ActiveLeaseSummary, ApprovalDecisionMessage, ArtifactAckMessage, ArtifactKind,
    ArtifactReference, DeliveryStageExecutionScope, ExecutionEventCategory, ExecutionJob,
    ExecutionJobReplacementAuthority, ExecutionLeaseStamp, ExecutionOutcome,
    ExecutionOutcomeStatus, ExecutionOutcomeUsage, ExecutionPortError, ExecutionPortErrorCode,
    ExecutionPortMessage, ExecutionScope, InputResponseMessage, JobCancelAckMessage,
    JobCancelAckMessageKind, JobCancelAckMessageStatus, JobCancelMessage, JobDispatchMessage,
    JobDispatchResultMessage, JobDispatchResultMessageKind, JobDispatchResultMessageStatus,
    JobOutcomeMessage, JobOutcomeMessageKind, ProductSessionExecutionScope, RuntimeEventMessage,
    RuntimeReplayRequestMessage, SessionBindingMessage, SessionBindingMessageKind,
    WorkerCapabilitySet, WorkerCapacity, WorkerHeartbeatMessage, WorkerHeartbeatMessageKind,
    WorkerRegisterMessage, WorkerRegisterMessageKind, WorkerRegistrationResultMessage,
    WorkerRegistrationResultMessageKind, WorkerRegistrationResultMessageLeaseRecovery,
    WorkerRegistrationResultMessageStatus,
};
use workspace::WorkspaceCloseReason;
use workspace_runtime::JobWorkspaceRuntime;

pub mod workspace;

/// Static Worker process identity and registration profile.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerConfig {
    /// Stable Worker identity.
    pub worker_id: WorkerId,
    /// Unique identity for this process boot.
    pub worker_instance_id: WorkerInstanceId,
    /// Exact process start time.
    pub started_at: Instant,
    /// Capability profile registered before any job is accepted.
    pub capabilities: WorkerCapabilitySet,
}

/// Process-level lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerLifecycleState {
    /// Constructed, but not registered.
    Booting,
    /// Registration request sent; awaiting the matching result.
    Registering,
    /// Registered and accepting work.
    Active,
    /// Refusing new work while active jobs are cancelled and closed.
    Draining,
    /// Clean shutdown completed.
    Stopped,
    /// A process-level protocol invariant failed.
    Faulted,
}

/// State of one non-terminal execution attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveJobLifecycle {
    /// One Codex thread and turn are active.
    Running,
    /// One cooperative interrupt has been requested.
    Cancelling,
}

/// One exact active Job/lease/WorkerSession/CodexThread binding.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveJob {
    /// Immutable dispatched job.
    pub job: ExecutionJob,
    /// Current exact lease and fencing authority.
    pub lease: ExecutionLeaseStamp,
    /// Worker-owned execution session.
    pub worker_session_id: WorkerSessionId,
    /// Four-part product, stage, Worker, and Codex binding.
    pub session_identity: SessionIdentity,
    /// Only live Codex thread for this attempt.
    pub codex_thread_id: CodexThreadId,
    /// Current local job lifecycle.
    pub lifecycle: ActiveJobLifecycle,
    /// Highest runtime trace sequence forwarded for this attempt.
    pub last_event_sequence: ExecutionAckSequence,
}

/// Stable Worker lifecycle rejection category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerErrorCode {
    /// Operation does not fit the current process lifecycle.
    InvalidLifecycle,
    /// Control Plane message is not accepted by this lifecycle module.
    UnexpectedMessage,
    /// Registration result is for another Worker, process, or request.
    RegistrationMismatch,
    /// Registration was rejected.
    RegistrationRejected,
    /// Dispatch contains invalid or stale Worker/lease authority.
    InvalidDispatchAuthority,
    /// A trace message does not match its exact active session.
    RuntimeTraceMismatch,
    /// A detached per-Job checkout could not be created, recovered, or removed.
    Workspace,
    /// Candidate bytes or acknowledgement differ from the pending writer Job.
    CandidateArtifactMismatch,
    /// Outbound `ExecutionPort` failed.
    ExecutionPort,
}

/// Secret-free Worker lifecycle error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerError {
    /// Stable failure category.
    pub code: WorkerErrorCode,
    /// Secret-free explanation.
    pub reason: String,
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for WorkerError {}

/// Derives the exact `WorkerSession` and `CodexThread` identities for one sealed
/// dispatch.  The scheduler uses this before opening the durable Worker slot;
/// keeping the derivation here means the scheduler and Worker cannot drift
/// into two identity authorities.
///
/// # Errors
///
/// Returns the same bounded identity error used by the Worker dispatch path.
pub fn canonical_dispatch_session_identity(
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
    dispatch: &JobDispatchMessage,
) -> Result<(WorkerSessionId, CodexThreadId), WorkerError> {
    let run_key = CodexRunKey::from_dispatch(dispatch);
    let codex_thread_id = run_key
        .canonical_thread_id()
        .map_err(|_| workspace_error())?;
    let canonical = serde_json::to_vec(&(
        worker_id,
        worker_instance_id,
        &dispatch.lease,
        &run_key.job_id,
        run_key.attempt,
        &run_key.fencing_token,
        &run_key.payload_digest,
    ))
    .map_err(|_| workspace_error())?;
    let digest = format!("{:x}", Sha256::digest(canonical));
    let worker_session_id = WorkerSessionId(format!("wsn_{}", &digest[..26].to_ascii_uppercase()));
    Ok((worker_session_id, codex_thread_id))
}

/// Deterministic graceful shutdown result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerShutdownReport {
    /// Jobs for which a terminal cancellation outcome was emitted.
    pub cancelled_jobs: Vec<ExecutionJobId>,
    /// Number of Codex interrupt/close/shutdown calls that failed.
    pub codex_failures: usize,
}

#[derive(Debug, Clone)]
struct DispatchRecord {
    run_key: CodexRunKey,
    job_digest: winwincode_domain::Sha256Digest,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    replacement_authority: Option<ExecutionJobReplacementAuthority>,
    terminal: bool,
}

struct PreparedDispatch {
    active: ActiveJob,
    checkout: std::path::PathBuf,
}

/// Test-only process stop around the adapter submission intent.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSubmissionFault {
    BeforeIntent,
    AfterIntent,
}

#[derive(Clone, Debug, PartialEq)]
struct PendingCandidateCompletion {
    summary: String,
    usage: ExecutionOutcomeUsage,
    authority: CandidateArtifactAuthority,
    artifact: Option<ArtifactReference>,
}

/// Single semantic Worker core shared by local and remote `ExecutionPort` IO.
pub struct WorkerMain<Port, Codex> {
    config: WorkerConfig,
    port: Port,
    codex: Codex,
    workspaces: JobWorkspaceRuntime,
    lifecycle: WorkerLifecycleState,
    registration_request_id: Option<RequestId>,
    /// Optional process-scoped namespace for an exact registration frame.
    ///
    /// The lightweight Worker fixtures intentionally keep their historical
    /// sequence ids.  The local production launcher opts into this namespace
    /// so a replacement process cannot collide with a predecessor's durable
    /// `(worker_id, request_id)` registration receipt. Restarting the same
    /// process identity reconstructs the exact request id, message id, and
    /// timestamp for Registry replay.
    registration_request_namespace: Option<String>,
    heartbeat_interval_ms: Option<u64>,
    heartbeat_sequence: i64,
    request_sequence: u64,
    message_sequence: u64,
    active: HashMap<String, ActiveJob>,
    dispatches: HashMap<String, DispatchRecord>,
    pending_candidates: HashMap<String, PendingCandidateCompletion>,
    /// On a recovered run, let the resumed Core task recreate an interactive
    /// request before replaying the durable transport frame.  A durable prompt
    /// can otherwise be observed before Core has installed its pending-input
    /// waiter, making a fast response a no-op.
    defer_core_interactions: bool,
    /// Delivery ids sent during the current Worker scheduling turn.  A
    /// response batch can contain acknowledgements for several earlier
    /// frames; suppressing an already-sent frame until the next poll avoids
    /// sending it twice while still allowing a later poll to retry a lost
    /// response.
    sent_delivery_ids: HashSet<String>,
    /// Interactive frames sent while a recovered Core turn is rebuilding its
    /// in-memory waiter. The durable request remains visible to the Control
    /// Plane, but the live Core event must not produce a second copy in the
    /// same recovery turn.
    recovery_sent_delivery_ids: HashSet<String>,
    /// Recovered interactive requests stay behind the resumed Core event. A
    /// replacement Worker must not let the Control Plane answer the durable
    /// replay before Core has rebuilt its pending request in memory.
    deferred_core_interaction_jobs: HashSet<String>,
    /// Job ids for interactive frames emitted by the current Core poll. This
    /// clears the recovery deferral only after the exact resumed event has
    /// been observed and forwarded.
    core_interaction_jobs: HashSet<String>,
    #[cfg(feature = "test-support")]
    submission_fault: Option<WorkerSubmissionFault>,
}

impl<Port, Codex> WorkerMain<Port, Codex>
where
    Port: WorkerExecutionPort,
    Codex: CodexCoreAdapter + Send + 'static,
{
    /// Creates a booting Worker without starting IO or Codex services.
    #[must_use]
    pub fn new(
        config: WorkerConfig,
        port: Port,
        mut codex: Codex,
        workspaces: JobWorkspaceRuntime,
    ) -> Self {
        let message_sequence = recover_message_sequence(&mut codex);
        let heartbeat_sequence =
            recover_heartbeat_sequence(&mut codex, &config.worker_id, &config.worker_instance_id);
        Self {
            config,
            port,
            codex,
            workspaces,
            lifecycle: WorkerLifecycleState::Booting,
            registration_request_id: None,
            registration_request_namespace: None,
            heartbeat_interval_ms: None,
            heartbeat_sequence,
            request_sequence: 0,
            message_sequence,
            active: HashMap::new(),
            dispatches: HashMap::new(),
            pending_candidates: HashMap::new(),
            defer_core_interactions: false,
            sent_delivery_ids: HashSet::new(),
            recovery_sent_delivery_ids: HashSet::new(),
            deferred_core_interaction_jobs: HashSet::new(),
            core_interaction_jobs: HashSet::new(),
            #[cfg(feature = "test-support")]
            submission_fault: None,
        }
    }

    /// Uses the Worker process identity to namespace its registration request.
    ///
    /// A stable Worker id is deliberately reused across replacement boots,
    /// while the process identity changes.  Registration receipts are keyed
    /// by Worker id and request id, so production local composition must
    /// derive a fresh request id from that process identity.  This opt-in
    /// keeps direct Worker contract fixtures deterministic.
    #[must_use]
    pub fn with_registration_request_namespace(
        mut self,
        worker_instance_id: &WorkerInstanceId,
        started_at: &Instant,
    ) -> Self {
        self.registration_request_namespace =
            Some(format!("{}\0{}", worker_instance_id.0, started_at.0));
        self
    }

    /// Installs one test-only process stop around submission intent persistence.
    #[cfg(feature = "test-support")]
    pub fn inject_submission_fault(&mut self, fault: WorkerSubmissionFault) {
        self.submission_fault = Some(fault);
    }

    /// Returns the current process lifecycle.
    #[must_use]
    pub const fn lifecycle(&self) -> WorkerLifecycleState {
        self.lifecycle
    }

    /// Returns the negotiated heartbeat interval after registration.
    #[must_use]
    pub const fn heartbeat_interval_ms(&self) -> Option<u64> {
        self.heartbeat_interval_ms
    }

    /// Returns active jobs sorted by Job id.
    #[must_use]
    pub fn active_jobs(&self) -> Vec<&ActiveJob> {
        let mut jobs = self.active.values().collect::<Vec<_>>();
        jobs.sort_by(|left, right| left.job.job_id.0.cmp(&right.job.job_id.0));
        jobs
    }

    /// Consumes the Worker and returns its injected ports for deterministic tests
    /// or outer process composition.
    #[must_use]
    pub fn into_parts(self) -> (Port, Codex) {
        (self.port, self.codex)
    }

    /// Starts the Worker by sending exactly one registration request.
    ///
    /// # Errors
    ///
    /// Returns an invalid-lifecycle or outbound-port failure.
    pub async fn start(&mut self, now: Instant) -> Result<(), WorkerError> {
        if self.lifecycle == WorkerLifecycleState::Registering {
            return self.flush_durable_execution_deliveries().await;
        }
        if self.lifecycle != WorkerLifecycleState::Booting {
            return Err(worker_error(
                WorkerErrorCode::InvalidLifecycle,
                "Worker registration can start only once",
            ));
        }
        self.retire_stale_registration_deliveries()?;
        if let Some(delivery) = self.recovered_registration_delivery()? {
            let ExecutionPortMessage::WorkerRegisterMessage(register) = &delivery.message else {
                unreachable!("recovered registration selector returned another message")
            };
            self.registration_request_id = Some(register.request_id.clone());
            self.lifecycle = WorkerLifecycleState::Registering;
            return self.send_retained_delivery(delivery).await;
        }
        let request_id = self.next_request_id();
        let (message_id, sent_at) =
            if let Some(namespace) = self.registration_request_namespace.as_deref() {
                (
                    namespaced_registration_message_id(namespace),
                    self.config.started_at.clone(),
                )
            } else {
                (self.next_message_id(), now)
            };
        let message = WorkerRegisterMessage {
            capabilities: self.config.capabilities.clone(),
            kind: WorkerRegisterMessageKind::WorkerRegister,
            message_id,
            request_id: request_id.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at,
            started_at: self.config.started_at.clone(),
            worker_id: self.config.worker_id.clone(),
            worker_instance_id: self.config.worker_instance_id.clone(),
        };
        let delivery =
            self.retain_execution_message(&ExecutionPortMessage::WorkerRegisterMessage(message))?;
        self.registration_request_id = Some(request_id);
        self.lifecycle = WorkerLifecycleState::Registering;
        self.send_retained_delivery(delivery).await
    }

    /// Applies one Control Plane message to the shared Worker lifecycle core.
    ///
    /// Local and remote adapters must decode into the same generated union
    /// before calling this method.
    ///
    /// # Errors
    ///
    /// Returns a stable protocol, lifecycle, authority, Codex-start, or outbound
    /// transport failure. Adapter error text is never copied into this error.
    pub async fn accept_control(
        &mut self,
        message: &ExecutionPortMessage,
        now: Instant,
    ) -> Result<(), WorkerError> {
        match message {
            ExecutionPortMessage::WorkerRegistrationResultMessage(result) => {
                self.accept_registration(result)?;
                self.codex
                    .accept_execution_delivery_ack(message)
                    .map_err(|_| codex_model_error())?;
                self.flush_durable_execution_deliveries().await
            }
            ExecutionPortMessage::JobDispatchMessage(dispatch) => {
                // A recovered Core turn owns the re-emission of pending
                // Approval/Input requests.  Do not expose a durable prompt
                // before the resumed Core has rebuilt its in-memory waiter;
                // otherwise an immediate response can be accepted into the
                // ledger while the Core has nowhere to deliver it.
                self.flush_durable_execution_deliveries_with_core_replay(true)
                    .await?;
                self.accept_dispatch(dispatch, now).await
            }
            ExecutionPortMessage::JobCancelMessage(cancel) => self.accept_cancel(cancel, now).await,
            ExecutionPortMessage::ArtifactAckMessage(acknowledgement) => {
                self.accept_candidate_artifact_ack(acknowledgement, now)
                    .await
            }
            ExecutionPortMessage::ModelChunkMessage(chunk) => {
                self.codex
                    .accept_model_chunk(chunk, &now)
                    .await
                    .map_err(|_| codex_model_error())?;
                if chunk.sequence.0 == 1 {
                    self.codex
                        .accept_execution_delivery_ack(message)
                        .map_err(|_| codex_model_error())?;
                }
                self.flush_durable_execution_deliveries().await?;
                self.flush_codex_execution_messages().await
            }
            ExecutionPortMessage::ActionEnforcementReceiptMessage(receipt) => {
                self.codex
                    .accept_action_receipt(receipt, &now)
                    .await
                    .map_err(|_| codex_model_error())?;
                self.codex
                    .accept_execution_delivery_ack(message)
                    .map_err(|_| codex_model_error())?;
                self.flush_durable_execution_deliveries().await?;
                self.flush_codex_execution_messages().await
            }
            ExecutionPortMessage::ApprovalDecisionMessage(decision) => {
                self.accept_approval_decision(decision, &now).await?;
                self.flush_durable_execution_deliveries().await?;
                self.flush_codex_execution_messages().await
            }
            ExecutionPortMessage::InputResponseMessage(response) => {
                self.accept_input_response(response, &now).await?;
                self.flush_durable_execution_deliveries().await?;
                self.flush_codex_execution_messages().await
            }
            ExecutionPortMessage::RuntimeAckMessage(_)
            | ExecutionPortMessage::JobOutcomeAckMessage(_)
            | ExecutionPortMessage::WorkerHeartbeatAckMessage(_) => {
                self.codex
                    .accept_execution_delivery_ack(message)
                    .map_err(|_| codex_model_error())?;
                self.flush_durable_execution_deliveries().await
            }
            ExecutionPortMessage::RuntimeReplayRequestMessage(request) => {
                self.replay_runtime(request).await
            }
            _ => Err(worker_error(
                WorkerErrorCode::UnexpectedMessage,
                "message is not handled by the Worker lifecycle core",
            )),
        }
    }

    /// Sends one explicit heartbeat with current capacity and lease progress.
    ///
    /// # Errors
    ///
    /// Returns an invalid-lifecycle or outbound-port failure.
    pub async fn heartbeat(&mut self, now: Instant) -> Result<(), WorkerError> {
        if self.lifecycle != WorkerLifecycleState::Active {
            return Err(worker_error(
                WorkerErrorCode::InvalidLifecycle,
                "heartbeat requires an active registered Worker",
            ));
        }
        self.flush_durable_execution_deliveries().await?;
        self.heartbeat_sequence = self.heartbeat_sequence.saturating_add(1);
        let mut active_leases = self
            .active
            .values()
            .map(|job| ActiveLeaseSummary {
                attempt: job.lease.attempt,
                expires_at: job.lease.expires_at.clone(),
                fencing_token: job.lease.fencing_token.clone(),
                job_id: job.job.job_id.clone(),
                last_event_sequence: job.last_event_sequence.clone(),
                lease_id: job.lease.lease_id.clone(),
            })
            .collect::<Vec<_>>();
        active_leases.sort_by(|left, right| left.job_id.0.cmp(&right.job_id.0));
        let max = self.config.capabilities.max_concurrent_jobs;
        let running = i64::try_from(self.active.len()).unwrap_or(i64::MAX);
        let heartbeat = WorkerHeartbeatMessage {
            active_leases,
            capacity: WorkerCapacity {
                available_slots: max.saturating_sub(running),
                running_jobs: running,
            },
            heartbeat_sequence: ExecutionSequence(self.heartbeat_sequence),
            kind: WorkerHeartbeatMessageKind::WorkerHeartbeat,
            message_id: self.next_message_id(),
            observed_at: now.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now,
            worker_id: self.config.worker_id.clone(),
            worker_instance_id: self.config.worker_instance_id.clone(),
        };
        self.retain_and_send(ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat))
            .await
    }

    /// Polls each active Codex thread once, forwarding only exact retained trace
    /// messages and deterministic terminal outcomes.
    ///
    /// # Errors
    ///
    /// Returns an exact trace-identity or outbound-port failure. Codex polling
    /// failures become infrastructure outcomes without exposing adapter text.
    pub async fn poll_codex(&mut self, now: Instant) -> Result<(), WorkerError> {
        if self.lifecycle != WorkerLifecycleState::Active {
            return Err(worker_error(
                WorkerErrorCode::InvalidLifecycle,
                "Codex polling requires an active Worker",
            ));
        }
        // A response can be delivered after the poll that sent its request,
        // and the response batch may also contain an acknowledgement for a
        // different frame.  Give each new poll a fresh retry window so a
        // lost response is retried, while the current window does not resend
        // the same frame between those acknowledgements.
        self.sent_delivery_ids.clear();
        if self.deferred_core_interaction_jobs.is_empty() {
            self.flush_durable_execution_deliveries().await?;
        } else {
            self.flush_recovered_durable_execution_deliveries().await?;
        }
        let mut job_ids = self.active.keys().cloned().collect::<Vec<_>>();
        job_ids.sort();
        for job_id in job_ids {
            let Some(thread_id) = self
                .active
                .get(&job_id)
                .map(|job| job.codex_thread_id.clone())
            else {
                continue;
            };
            let Ok(polled) = self.codex.poll(&thread_id, &now).await else {
                self.finish_unavailable_codex_job(&job_id, now.clone())
                    .await?;
                continue;
            };
            self.flush_codex_execution_messages().await?;
            if self.core_interaction_jobs.remove(&job_id) {
                self.deferred_core_interaction_jobs.remove(&job_id);
            }
            self.handle_codex_poll(&job_id, polled, now.clone()).await?;
        }
        Ok(())
    }

    async fn handle_codex_poll(
        &mut self,
        job_id: &str,
        polled: CodexPoll,
        now: Instant,
    ) -> Result<(), WorkerError> {
        match polled {
            CodexPoll::Pending => Ok(()),
            CodexPoll::RuntimeTrace(message) => self.forward_runtime_trace(job_id, *message).await,
            CodexPoll::Completed(completion) => {
                self.complete_codex_job(job_id, completion, now).await
            }
            CodexPoll::Failed(summary) => {
                self.finish_job(
                    job_id,
                    ExecutionOutcomeStatus::Failed,
                    summary.as_str(),
                    Vec::new(),
                    None,
                    Some(port_error(
                        ExecutionPortErrorCode::ExecutionFailed,
                        "embedded Codex execution failed",
                        false,
                    )),
                    now,
                )
                .await
            }
            CodexPoll::Cancelled(summary) => {
                self.finish_job(
                    job_id,
                    ExecutionOutcomeStatus::Cancelled,
                    summary.as_str(),
                    Vec::new(),
                    None,
                    Some(port_error(
                        ExecutionPortErrorCode::Cancelled,
                        "embedded Codex turn cancelled",
                        false,
                    )),
                    now,
                )
                .await
            }
            CodexPoll::InfrastructureFailed(summary) => {
                self.finish_job(
                    job_id,
                    ExecutionOutcomeStatus::InfrastructureError,
                    summary.as_str(),
                    Vec::new(),
                    None,
                    Some(port_error(
                        ExecutionPortErrorCode::InfrastructureError,
                        "embedded Codex infrastructure failed",
                        true,
                    )),
                    now,
                )
                .await
            }
        }
    }

    async fn complete_codex_job(
        &mut self,
        job_id: &str,
        completion: CodexTurnCompletion,
        now: Instant,
    ) -> Result<(), WorkerError> {
        let cancelling = self
            .active
            .get(job_id)
            .is_some_and(|job| job.lifecycle == ActiveJobLifecycle::Cancelling);
        let candidate_writer = self
            .active
            .get(job_id)
            .is_some_and(|job| candidate_writer_role(&job.job.execution_profile));
        let verification_role = self
            .active
            .get(job_id)
            .is_some_and(|job| verification_artifact_role(&job.job.execution_profile));
        if !cancelling && (candidate_writer || verification_role) {
            if candidate_writer {
                return self
                    .complete_candidate_writer(job_id, completion, now)
                    .await;
            }
            return self
                .complete_verification_job(job_id, completion, now)
                .await;
        }
        let (status, summary, error) = if cancelling {
            (
                ExecutionOutcomeStatus::Cancelled,
                "Codex turn stopped after cancellation",
                Some(port_error(
                    ExecutionPortErrorCode::Cancelled,
                    "Codex turn stopped after cancellation",
                    false,
                )),
            )
        } else {
            (
                ExecutionOutcomeStatus::Succeeded,
                completion.summary.as_str(),
                None,
            )
        };
        let usage = (!cancelling).then_some(completion.usage);
        self.finish_job(
            job_id,
            status,
            summary,
            completion.artifacts,
            usage,
            error,
            now,
        )
        .await
    }

    async fn complete_candidate_writer(
        &mut self,
        job_id: &str,
        completion: CodexTurnCompletion,
        now: Instant,
    ) -> Result<(), WorkerError> {
        if let Some(artifact) = self.hold_candidate_completion(job_id, completion)? {
            return self.finish_candidate_job(job_id, artifact, now).await;
        }
        let active = self.active.get(job_id).cloned().ok_or_else(|| {
            candidate_artifact_error("candidate completion has no active workspace authority")
        })?;
        if let Ok(prepared) = self.workspaces.prepare_candidate(&active) {
            return self
                .retain_candidate_artifact(&active.job.job_id, prepared, now)
                .await;
        }
        self.pending_candidates.remove(job_id);
        self.finish_job(
            job_id,
            ExecutionOutcomeStatus::Failed,
            "writer completed without a valid candidate",
            Vec::new(),
            None,
            Some(port_error(
                ExecutionPortErrorCode::ExecutionFailed,
                "writer completed without a valid candidate",
                false,
            )),
            now,
        )
        .await
    }

    async fn complete_verification_job(
        &mut self,
        job_id: &str,
        completion: CodexTurnCompletion,
        now: Instant,
    ) -> Result<(), WorkerError> {
        if let Some(artifact) = self.hold_candidate_completion(job_id, completion)? {
            return self.finish_candidate_job(job_id, artifact, now).await;
        }
        let active = self.active.get(job_id).cloned().ok_or_else(|| {
            candidate_artifact_error("verification completion has no active workspace authority")
        })?;
        if let Ok(prepared) = self.workspaces.prepare_verification(&active) {
            return self
                .retain_candidate_artifact(&active.job.job_id, prepared, now)
                .await;
        }
        self.pending_candidates.remove(job_id);
        self.finish_job(
            job_id,
            ExecutionOutcomeStatus::Failed,
            "verification completed without a valid candidate",
            Vec::new(),
            None,
            Some(port_error(
                ExecutionPortErrorCode::ExecutionFailed,
                "verification completed without a valid candidate",
                false,
            )),
            now,
        )
        .await
    }

    async fn finish_unavailable_codex_job(
        &mut self,
        job_id: &str,
        now: Instant,
    ) -> Result<(), WorkerError> {
        self.finish_job(
            job_id,
            ExecutionOutcomeStatus::InfrastructureError,
            "embedded Codex runtime became unavailable",
            Vec::new(),
            None,
            Some(port_error(
                ExecutionPortErrorCode::InfrastructureError,
                "embedded Codex runtime became unavailable",
                true,
            )),
            now,
        )
        .await
    }

    /// Retains the verified detached writer candidate before its first upload.
    ///
    /// A successful Codex completion for an executor or remediator remains
    /// non-terminal until this method retains the exact candidate and a final
    /// matching `artifact.ack` is accepted.
    ///
    /// # Errors
    ///
    /// Rejects a foreign, cancelling, non-writer, or non-completed Job and any
    /// candidate whose sealed authority differs from the active attempt.
    #[allow(clippy::too_many_lines)]
    pub async fn retain_candidate_artifact(
        &mut self,
        job_id: &ExecutionJobId,
        prepared: stage_product::PreparedCandidateArtifact,
        now: Instant,
    ) -> Result<(), WorkerError> {
        if self.lifecycle != WorkerLifecycleState::Active {
            return Err(candidate_artifact_error(
                "candidate retention requires an active Worker",
            ));
        }
        let active = self.active.get(&job_id.0).cloned().ok_or_else(|| {
            candidate_artifact_error("candidate retention has no active candidate-producing Job")
        })?;
        if active.lifecycle != ActiveJobLifecycle::Running
            || !candidate_artifact_role(&active.job.execution_profile)
        {
            return Err(candidate_artifact_error(
                "candidate retention requires a running candidate-producing Job",
            ));
        }
        let expected = self
            .pending_candidates
            .get(&job_id.0)
            .ok_or_else(|| {
                candidate_artifact_error("candidate retention requires a completed candidate turn")
            })?
            .authority
            .clone();
        let mut upload = prepared.into_upload(now);
        upload.replacement_authority = self
            .dispatches
            .get(&job_id.0)
            .and_then(|record| record.replacement_authority.clone());
        if upload.authority() != expected {
            return Err(candidate_artifact_error(
                "candidate upload authority differs from the completed writer Job",
            ));
        }
        let retained = self
            .codex
            .retain_candidate_artifact(&upload)
            .map_err(|_| codex_model_error())?;
        let pending = self
            .pending_candidates
            .get_mut(&job_id.0)
            .ok_or_else(|| candidate_artifact_error("candidate completion disappeared"))?;
        if retained.authority != pending.authority {
            let replacement = upload.replacement_authority.as_ref().ok_or_else(|| {
                candidate_artifact_error(
                    "candidate upload resumed a predecessor without replacement authority",
                )
            })?;
            if !replacement_candidate_authority_matches(
                replacement,
                &retained.authority,
                &pending.authority,
            ) {
                return Err(candidate_artifact_error(
                    "candidate upload predecessor differs from sealed replacement authority",
                ));
            }
            pending.authority = retained.authority.clone();
        }
        if pending
            .artifact
            .as_ref()
            .is_some_and(|artifact| artifact != &retained.artifact)
        {
            return Err(candidate_artifact_error(
                "candidate Artifact identity changed after retention",
            ));
        }
        pending.artifact = Some(retained.artifact.clone());
        if retained.already_accepted {
            let accepted = self
                .codex
                .accepted_candidate_artifact(&retained.authority)
                .map_err(|_| codex_model_error())?
                .ok_or_else(|| {
                    candidate_artifact_error("accepted candidate reference is missing")
                })?;
            if accepted != retained.artifact {
                return Err(candidate_artifact_error(
                    "accepted candidate reference changed after restart",
                ));
            }
            return self
                .finish_candidate_job(&job_id.0, accepted, upload.created_at)
                .await;
        }
        if retained.deliveries.is_empty() {
            return self.flush_durable_execution_deliveries().await;
        }
        for delivery in retained.deliveries {
            if self
                .codex
                .candidate_artifact_delivery_allowed(&delivery.message)
                .map_err(|_| codex_model_error())?
            {
                self.send_retained_delivery(delivery).await?;
            }
        }
        Ok(())
    }

    /// Stops accepting work, interrupts every active turn, emits terminal
    /// outcomes, closes threads, and shuts down the embedded Codex adapter.
    ///
    /// # Errors
    ///
    /// Returns an outbound-port or durable candidate-cancellation failure. Codex
    /// interrupt/shutdown failures are counted without leaking adapter text.
    pub async fn shutdown(&mut self, now: Instant) -> Result<WorkerShutdownReport, WorkerError> {
        if matches!(
            self.lifecycle,
            WorkerLifecycleState::Stopped | WorkerLifecycleState::Faulted
        ) {
            return Err(worker_error(
                WorkerErrorCode::InvalidLifecycle,
                "Worker is already terminal",
            ));
        }
        let candidate_authorities = self
            .pending_candidates
            .values()
            .map(|pending| pending.authority.clone())
            .collect::<Vec<_>>();
        for authority in candidate_authorities {
            self.codex
                .begin_candidate_artifact_cancel(&authority)
                .map_err(|_| codex_model_error())?;
            self.codex
                .cancel_candidate_artifact(&authority)
                .map_err(|_| codex_model_error())?;
        }
        self.pending_candidates.clear();
        self.flush_durable_execution_deliveries().await?;
        self.lifecycle = WorkerLifecycleState::Draining;
        let mut job_ids = self.active.keys().cloned().collect::<Vec<_>>();
        job_ids.sort();
        let mut codex_failures = 0;
        let mut cancelled_jobs = Vec::with_capacity(job_ids.len());
        for job_id in job_ids {
            let Some(thread_id) = self
                .active
                .get(&job_id)
                .map(|job| job.codex_thread_id.clone())
            else {
                continue;
            };
            if self.codex.interrupt(&thread_id, &now).await.is_err() {
                codex_failures += 1;
            }
            let Some(id) = self.active.get(&job_id).map(|job| job.job.job_id.clone()) else {
                continue;
            };
            self.finish_job(
                &job_id,
                ExecutionOutcomeStatus::Cancelled,
                "Worker shutdown cancelled the active turn",
                Vec::new(),
                None,
                Some(port_error(
                    ExecutionPortErrorCode::Cancelled,
                    "Worker shutdown cancelled the active turn",
                    false,
                )),
                now.clone(),
            )
            .await?;
            cancelled_jobs.push(id);
        }
        if self.codex.shutdown().await.is_err() {
            codex_failures += 1;
        }
        self.lifecycle = WorkerLifecycleState::Stopped;
        Ok(WorkerShutdownReport {
            cancelled_jobs,
            codex_failures,
        })
    }

    fn accept_registration(
        &mut self,
        result: &WorkerRegistrationResultMessage,
    ) -> Result<(), WorkerError> {
        if self.lifecycle != WorkerLifecycleState::Registering {
            return Err(worker_error(
                WorkerErrorCode::InvalidLifecycle,
                "registration result arrived outside registration",
            ));
        }
        if result.worker_id != self.config.worker_id
            || result.worker_instance_id != self.config.worker_instance_id
            || Some(&result.request_id) != self.registration_request_id.as_ref()
        {
            self.lifecycle = WorkerLifecycleState::Faulted;
            return Err(worker_error(
                WorkerErrorCode::RegistrationMismatch,
                "registration result does not match this Worker process",
            ));
        }
        if result.status == WorkerRegistrationResultMessageStatus::Rejected {
            self.lifecycle = WorkerLifecycleState::Faulted;
            return Err(worker_error(
                WorkerErrorCode::RegistrationRejected,
                "Control Plane rejected Worker registration",
            ));
        }
        let interval = u64::try_from(result.heartbeat_interval_ms).map_err(|_| {
            worker_error(
                WorkerErrorCode::RegistrationMismatch,
                "registration heartbeat interval is invalid",
            )
        })?;
        if interval == 0 {
            return Err(worker_error(
                WorkerErrorCode::RegistrationMismatch,
                "registration heartbeat interval is invalid",
            ));
        }
        if result.lease_recovery == WorkerRegistrationResultMessageLeaseRecovery::ReacquireRequired
        {
            self.active.clear();
        }
        self.heartbeat_interval_ms = Some(interval);
        self.lifecycle = WorkerLifecycleState::Active;
        Ok(())
    }

    async fn accept_dispatch(
        &mut self,
        dispatch: &JobDispatchMessage,
        now: Instant,
    ) -> Result<(), WorkerError> {
        if self.lifecycle != WorkerLifecycleState::Active {
            return Err(worker_error(
                WorkerErrorCode::InvalidLifecycle,
                "dispatch requires an active registered Worker",
            ));
        }
        if let Some((status, error)) = dispatch_authority_rejection(&self.config, dispatch, &now) {
            self.send_dispatch_result(dispatch, status, None, Some(error), now)
                .await?;
            return Ok(());
        }
        let run_key = CodexRunKey::from_dispatch(dispatch);
        let job_digest = winwincode_codex::stage_product::stage_product_job_digest(&dispatch.job)
            .map_err(|_| {
            worker_error(
                WorkerErrorCode::UnexpectedMessage,
                "dispatch Job cannot be sealed for duplicate detection",
            )
        })?;
        if let Some(record) = self.dispatches.get(&dispatch.job.job_id.0).cloned() {
            let status = if record.run_key == run_key && record.job_digest == job_digest {
                JobDispatchResultMessageStatus::Duplicate
            } else {
                JobDispatchResultMessageStatus::Conflict
            };
            self.send_dispatch_result(dispatch, status, Some(record.worker_session_id), None, now)
                .await?;
            return Ok(());
        }
        let max = usize::try_from(self.config.capabilities.max_concurrent_jobs).unwrap_or(0);
        if max == 0 || self.active.len() >= max {
            self.send_dispatch_result(
                dispatch,
                JobDispatchResultMessageStatus::RejectedCapacity,
                None,
                None,
                now,
            )
            .await?;
            return Ok(());
        }
        let (product_session_id, stage_run_id) = scope_identity(&dispatch.job.scope);
        self.start_dispatch(
            dispatch,
            run_key,
            job_digest,
            product_session_id,
            stage_run_id,
            now,
        )
        .await
    }

    #[allow(clippy::too_many_lines)]
    async fn start_dispatch(
        &mut self,
        dispatch: &JobDispatchMessage,
        run_key: CodexRunKey,
        job_digest: winwincode_domain::Sha256Digest,
        product_session_id: ProductSessionId,
        stage_run_id: Option<StageRunId>,
        now: Instant,
    ) -> Result<(), WorkerError> {
        let mut prepared = match self.prepare_dispatch_workspace(
            dispatch,
            &run_key,
            product_session_id,
            stage_run_id,
        ) {
            Ok(prepared) => prepared,
            Err(_error) => {
                self.reject_dispatch_failure(
                    dispatch,
                    JobDispatchResultMessageStatus::RejectedCapability,
                    ExecutionPortErrorCode::InfrastructureError,
                    "detached Job workspace is unavailable",
                    true,
                    now,
                )
                .await?;
                return Ok(());
            }
        };
        let thread_id = match self
            .codex
            .ensure_thread(CodexThreadStart {
                run_key: &run_key,
                job: &dispatch.job,
                lease: &dispatch.lease,
                worker_session_id: &prepared.active.worker_session_id,
                workspace: &prepared.checkout,
            })
            .await
        {
            Ok(thread_id) => thread_id,
            Err(_error) => {
                let _ = self
                    .workspaces
                    .close_job(&dispatch.job.job_id, WorkspaceCloseReason::Failed);
                self.reject_dispatch_failure(
                    dispatch,
                    JobDispatchResultMessageStatus::RejectedCapability,
                    ExecutionPortErrorCode::InfrastructureError,
                    "embedded Codex Core is unavailable",
                    true,
                    now,
                )
                .await?;
                return Ok(());
            }
        };
        if self.dispatch_thread_conflicts(&thread_id, &prepared.active.codex_thread_id) {
            let _ = self.codex.close_thread(&thread_id).await;
            let _ = self
                .workspaces
                .close_job(&dispatch.job.job_id, WorkspaceCloseReason::Failed);
            self.reject_dispatch_failure(
                dispatch,
                JobDispatchResultMessageStatus::Conflict,
                ExecutionPortErrorCode::JobDispatchConflict,
                "Codex thread is already bound to another Job",
                false,
                now,
            )
            .await?;
            return Ok(());
        }
        prepared.active.last_event_sequence =
            self.recovered_runtime_cursor(&prepared.active.session_identity)?;
        self.install_dispatch_binding(dispatch, run_key, job_digest, prepared.active, now.clone())
            .await?;
        // Synchronize the adapter's trusted clock after the binding is
        // installed but before replay or submission can cause a Provider
        // request.  This closes the gap where a lease expires between
        // dispatch validation and the first embedded Kernel model call.
        if let Err(_error) = self.codex.observe_now(&now) {
            return Err(codex_model_error());
        }
        if self.recovered_core_interaction_pending(&dispatch.job.job_id.0)? {
            self.deferred_core_interaction_jobs
                .insert(dispatch.job.job_id.0.clone());
        }
        // A restart can leave response-bearing approval/input frames in the
        // adapter outbox while the in-memory Worker has no active Job yet.
        // Flush again after installing this dispatch so those exact retained
        // frames are replayed before a recovered Core turn is submitted.
        self.flush_recovered_durable_execution_deliveries().await?;
        self.defer_core_interactions = true;
        #[cfg(feature = "test-support")]
        let submission_fault = self.submission_fault.take();
        #[cfg(feature = "test-support")]
        if submission_fault == Some(WorkerSubmissionFault::BeforeIntent) {
            return Err(worker_error(
                WorkerErrorCode::UnexpectedMessage,
                "test process stopped before submission intent",
            ));
        }
        let submitted = match winwincode_codex::stage_product::stage_product_prompt(&dispatch.job) {
            Ok(prompt) => match self.codex.submit_turn(&thread_id, &prompt).await {
                Ok(()) => true,
                Err(_error) => false,
            },
            Err(_error) => false,
        };
        self.flush_codex_execution_messages().await?;
        if !submitted {
            #[cfg(feature = "test-support")]
            if submission_fault == Some(WorkerSubmissionFault::AfterIntent) {
                return Err(worker_error(
                    WorkerErrorCode::UnexpectedMessage,
                    "test process stopped after submission intent",
                ));
            }
            self.flush_durable_execution_deliveries().await?;
            self.finish_job(
                &dispatch.job.job_id.0,
                ExecutionOutcomeStatus::InfrastructureError,
                "embedded Codex turn did not start",
                Vec::new(),
                None,
                Some(port_error(
                    ExecutionPortErrorCode::InfrastructureError,
                    "embedded Codex turn did not start",
                    true,
                )),
                now,
            )
            .await?;
        }
        Ok(())
    }

    fn dispatch_thread_conflicts(
        &self,
        thread_id: &CodexThreadId,
        expected_thread_id: &CodexThreadId,
    ) -> bool {
        thread_id != expected_thread_id
            || self
                .dispatches
                .values()
                .any(|record| record.codex_thread_id == *thread_id)
    }

    fn prepare_dispatch_workspace(
        &mut self,
        dispatch: &JobDispatchMessage,
        run_key: &CodexRunKey,
        product_session_id: ProductSessionId,
        stage_run_id: Option<StageRunId>,
    ) -> Result<PreparedDispatch, WorkerError> {
        let worker_session_id = self.worker_session_id(dispatch, run_key)?;
        let expected_thread_id = run_key
            .canonical_thread_id()
            .map_err(|_| workspace_error())?;
        let active = ActiveJob {
            job: dispatch.job.clone(),
            lease: dispatch.lease.clone(),
            worker_session_id: worker_session_id.clone(),
            session_identity: SessionIdentity {
                codex_thread_id: expected_thread_id.clone(),
                product_session_id,
                stage_run_id,
                worker_session_id,
            },
            codex_thread_id: expected_thread_id,
            lifecycle: ActiveJobLifecycle::Running,
            last_event_sequence: ExecutionAckSequence(0),
        };
        let checkout = self
            .workspaces
            .open_for_job(&active, dispatch.replacement_authority.as_ref())
            .map_err(|_| workspace_error())?;
        Ok(PreparedDispatch { active, checkout })
    }

    async fn reject_dispatch_failure(
        &mut self,
        dispatch: &JobDispatchMessage,
        status: JobDispatchResultMessageStatus,
        code: ExecutionPortErrorCode,
        message: &str,
        retryable: bool,
        now: Instant,
    ) -> Result<(), WorkerError> {
        self.send_dispatch_result(
            dispatch,
            status,
            None,
            Some(port_error(code, message, retryable)),
            now,
        )
        .await
    }

    async fn install_dispatch_binding(
        &mut self,
        dispatch: &JobDispatchMessage,
        run_key: CodexRunKey,
        job_digest: winwincode_domain::Sha256Digest,
        active: ActiveJob,
        now: Instant,
    ) -> Result<(), WorkerError> {
        let worker_session_id = active.worker_session_id.clone();
        let thread_id = active.codex_thread_id.clone();
        let session_identity = active.session_identity.clone();
        let product_session_id = session_identity.product_session_id.clone();
        let stage_run_id = session_identity.stage_run_id.clone();
        self.active.insert(dispatch.job.job_id.0.clone(), active);
        self.dispatches.insert(
            dispatch.job.job_id.0.clone(),
            DispatchRecord {
                run_key,
                job_digest,
                worker_session_id: worker_session_id.clone(),
                codex_thread_id: thread_id.clone(),
                replacement_authority: dispatch.replacement_authority.clone(),
                terminal: false,
            },
        );
        self.send_dispatch_result(
            dispatch,
            JobDispatchResultMessageStatus::Accepted,
            Some(worker_session_id.clone()),
            None,
            now.clone(),
        )
        .await?;
        let binding = SessionBindingMessage {
            attempt: dispatch.job.attempt,
            bound_at: now.clone(),
            codex_thread_id: thread_id.clone(),
            fencing_token: dispatch.lease.fencing_token.clone(),
            kind: SessionBindingMessageKind::SessionBinding,
            lease: dispatch.lease.clone(),
            lease_id: dispatch.lease.lease_id.clone(),
            message_id: self.next_message_id(),
            product_session_id,
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now.clone(),
            session_identity,
            source_identity: SessionBindingSourceIdentity {
                kind: SessionBindingSourceIdentityKind::ExecutionWorker,
                lease_id: dispatch.lease.lease_id.clone(),
                worker_id: self.config.worker_id.clone(),
                worker_instance_id: self.config.worker_instance_id.clone(),
                worker_session_id: worker_session_id.clone(),
            },
            stage_run_id,
            worker_id: self.config.worker_id.clone(),
            worker_session_id,
        };
        self.retain_and_send(ExecutionPortMessage::SessionBindingMessage(binding))
            .await?;
        Ok(())
    }

    fn hold_candidate_completion(
        &mut self,
        job_id: &str,
        completion: CodexTurnCompletion,
    ) -> Result<Option<ArtifactReference>, WorkerError> {
        if !completion.artifacts.is_empty() {
            return Err(candidate_artifact_error(
                "writer completion cannot inject an unacknowledged Artifact reference",
            ));
        }
        let active = self.active.get(job_id).ok_or_else(|| {
            candidate_artifact_error("writer completion has no active Job authority")
        })?;
        let authority = candidate_artifact_authority(active)?;
        let pending = PendingCandidateCompletion {
            summary: completion.summary.as_str().to_owned(),
            usage: completion.usage,
            authority: authority.clone(),
            artifact: None,
        };
        if let Some(existing) = self.pending_candidates.get(job_id) {
            if existing.summary != pending.summary
                || existing.usage != pending.usage
                || existing.authority != pending.authority
            {
                return Err(candidate_artifact_error(
                    "writer completion changed before candidate acceptance",
                ));
            }
        } else {
            self.pending_candidates.insert(job_id.to_owned(), pending);
        }
        let accepted = self
            .codex
            .accepted_candidate_artifact(&authority)
            .map_err(|_| codex_model_error())?;
        if let Some(artifact) = &accepted {
            let pending = self
                .pending_candidates
                .get_mut(job_id)
                .ok_or_else(|| candidate_artifact_error("candidate completion disappeared"))?;
            if pending
                .artifact
                .as_ref()
                .is_some_and(|existing| existing != artifact)
            {
                return Err(candidate_artifact_error(
                    "accepted candidate identity changed after restart",
                ));
            }
            pending.artifact = Some(artifact.clone());
        }
        Ok(accepted)
    }

    async fn accept_candidate_artifact_ack(
        &mut self,
        acknowledgement: &ArtifactAckMessage,
        now: Instant,
    ) -> Result<(), WorkerError> {
        let job_id = acknowledgement.lease.job_id.0.clone();
        if self.active.contains_key(&job_id) && self.pending_candidates.contains_key(&job_id) {
            self.validate_candidate_ack_state(acknowledgement, None)?;
        }
        let durable_acknowledgement = self.durable_candidate_ack(acknowledgement)?;
        let outcome = self
            .codex
            .accept_candidate_artifact_ack(&durable_acknowledgement)
            .map_err(|_| {
                candidate_artifact_error("Artifact acknowledgement conflicts with durable upload")
            })?;
        let accepted = match &outcome {
            CandidateArtifactAckOutcome::Accepted(artifact) => Some(artifact),
            CandidateArtifactAckOutcome::Pending | CandidateArtifactAckOutcome::Replay(_) => None,
        };
        if self.active.contains_key(&job_id) && self.pending_candidates.contains_key(&job_id) {
            self.validate_candidate_ack_state(acknowledgement, accepted)?;
        }
        match outcome {
            CandidateArtifactAckOutcome::Pending => self.flush_durable_execution_deliveries().await,
            CandidateArtifactAckOutcome::Replay(deliveries) => {
                if self.active.contains_key(&job_id)
                    && self.pending_candidates.contains_key(&job_id)
                {
                    for delivery in deliveries {
                        if self
                            .codex
                            .candidate_artifact_delivery_allowed(&delivery.message)
                            .map_err(|_| codex_model_error())?
                        {
                            self.send_retained_delivery(delivery).await?;
                        }
                    }
                    return Ok(());
                }
                self.flush_durable_execution_deliveries().await
            }
            CandidateArtifactAckOutcome::Accepted(artifact) => {
                if !self.active.contains_key(&job_id)
                    || !self.pending_candidates.contains_key(&job_id)
                {
                    return self.flush_durable_execution_deliveries().await;
                }
                self.finish_candidate_job(&job_id, artifact, now).await
            }
        }
    }

    fn validate_candidate_ack_state(
        &self,
        acknowledgement: &ArtifactAckMessage,
        accepted: Option<&ArtifactReference>,
    ) -> Result<(), WorkerError> {
        let job_id = &acknowledgement.lease.job_id.0;
        let active = self.active.get(job_id).ok_or_else(|| {
            candidate_artifact_error("Artifact acknowledgement has no active writer Job")
        })?;
        let pending = self.pending_candidates.get(job_id).ok_or_else(|| {
            candidate_artifact_error("Artifact acknowledgement arrived before writer completion")
        })?;
        let active_authority = candidate_artifact_authority(active)?;
        let authority_matches = pending.authority == active_authority
            || self
                .dispatches
                .get(job_id)
                .and_then(|record| record.replacement_authority.as_ref())
                .is_some_and(|replacement| {
                    replacement_candidate_authority_matches(
                        replacement,
                        &pending.authority,
                        &active_authority,
                    )
                });
        let acknowledgement_matches_pending = acknowledgement.lease == pending.authority.lease
            && acknowledgement.worker_session_id == pending.authority.worker_session_id
            && acknowledgement.session_identity == pending.authority.session_identity;
        let acknowledgement_matches_successor = authority_matches
            && acknowledgement.lease == active_authority.lease
            && acknowledgement.worker_session_id == active_authority.worker_session_id
            && acknowledgement.session_identity == active_authority.session_identity;
        if active.lifecycle != ActiveJobLifecycle::Running
            || !authority_matches
            || (!acknowledgement_matches_pending && !acknowledgement_matches_successor)
            || pending
                .artifact
                .as_ref()
                .is_some_and(|artifact| artifact.artifact_id != acknowledgement.artifact_id)
            || accepted.is_some_and(|artifact| {
                pending
                    .artifact
                    .as_ref()
                    .is_some_and(|expected| expected != artifact)
            })
        {
            return Err(candidate_artifact_error(
                "Artifact acknowledgement differs from the pending writer Job",
            ));
        }
        Ok(())
    }

    /// Rebinds a successor-authority ACK to the predecessor authority which
    /// owns the retained candidate frames. A replacement receipt may expose a
    /// successor Job to the Worker while the durable Artifact stream remains
    /// byte- and session-identical to its predecessor.
    fn durable_candidate_ack(
        &self,
        acknowledgement: &ArtifactAckMessage,
    ) -> Result<ArtifactAckMessage, WorkerError> {
        let job_id = &acknowledgement.lease.job_id.0;
        let Some(active) = self.active.get(job_id) else {
            return Ok(acknowledgement.clone());
        };
        let Some(pending) = self.pending_candidates.get(job_id) else {
            return Ok(acknowledgement.clone());
        };
        let active_authority = candidate_artifact_authority(active)?;
        if pending.authority == active_authority {
            return Ok(acknowledgement.clone());
        }
        let Some(replacement) = self
            .dispatches
            .get(job_id)
            .and_then(|record| record.replacement_authority.as_ref())
        else {
            return Ok(acknowledgement.clone());
        };
        if !replacement_candidate_authority_matches(
            replacement,
            &pending.authority,
            &active_authority,
        ) {
            return Ok(acknowledgement.clone());
        }
        if acknowledgement.lease != active_authority.lease
            || acknowledgement.worker_session_id != active_authority.worker_session_id
            || acknowledgement.session_identity != active_authority.session_identity
        {
            return Ok(acknowledgement.clone());
        }
        let Some(predecessor_session) = replacement.predecessor_session_identity.as_ref() else {
            return Ok(acknowledgement.clone());
        };
        let mut durable = acknowledgement.clone();
        durable.lease = replacement.predecessor_lease.clone();
        durable.worker_session_id = predecessor_session.worker_session_id.clone();
        durable.session_identity = predecessor_session.clone();
        Ok(durable)
    }

    async fn finish_candidate_job(
        &mut self,
        job_id: &str,
        artifact: ArtifactReference,
        now: Instant,
    ) -> Result<(), WorkerError> {
        let pending = self.pending_candidates.get_mut(job_id).ok_or_else(|| {
            candidate_artifact_error("accepted candidate has no pending writer completion")
        })?;
        if pending
            .artifact
            .as_ref()
            .is_some_and(|expected| expected != &artifact)
        {
            return Err(candidate_artifact_error(
                "final candidate reference differs from the retained upload",
            ));
        }
        pending.artifact = Some(artifact.clone());
        let pending = pending.clone();
        let result = self
            .finish_job(
                job_id,
                ExecutionOutcomeStatus::Succeeded,
                &pending.summary,
                vec![artifact],
                Some(pending.usage),
                None,
                now,
            )
            .await;
        if result.is_ok() || !self.active.contains_key(job_id) {
            self.pending_candidates.remove(job_id);
        }
        result
    }

    async fn accept_cancel(
        &mut self,
        cancel: &JobCancelMessage,
        now: Instant,
    ) -> Result<(), WorkerError> {
        let key = cancel.lease.job_id.0.clone();
        let (status, thread_id) = match self.active.get_mut(&key) {
            Some(active)
                if active.lease.worker_instance_id != cancel.lease.worker_instance_id
                    || active.lease.worker_id != cancel.lease.worker_id
                    || active.worker_session_id != cancel.worker_session_id
                    || active.session_identity != cancel.session_identity =>
            {
                (JobCancelAckMessageStatus::RejectedWorkerInstance, None)
            }
            Some(active)
                if active.lease.fencing_token != cancel.lease.fencing_token
                    || active.lease.lease_id != cancel.lease.lease_id =>
            {
                (JobCancelAckMessageStatus::RejectedStaleFencingToken, None)
            }
            Some(active) if now.0 >= active.lease.expires_at.0 => {
                (JobCancelAckMessageStatus::RejectedExpiredLease, None)
            }
            Some(active) if active.lifecycle == ActiveJobLifecycle::Cancelling => (
                JobCancelAckMessageStatus::AlreadyCancelling,
                self.pending_candidates
                    .contains_key(&key)
                    .then(|| active.codex_thread_id.clone()),
            ),
            Some(active) => {
                active.lifecycle = ActiveJobLifecycle::Cancelling;
                (
                    JobCancelAckMessageStatus::Accepted,
                    Some(active.codex_thread_id.clone()),
                )
            }
            None => (JobCancelAckMessageStatus::AlreadyTerminal, None),
        };
        if let Some(thread_id) = thread_id {
            let candidate_authority = if let Some(authority) = self
                .pending_candidates
                .get(&key)
                .map(|pending| pending.authority.clone())
            {
                Some(authority)
            } else {
                self.active
                    .get(&key)
                    .filter(|active| candidate_artifact_role(&active.job.execution_profile))
                    .map(candidate_artifact_authority)
                    .transpose()?
            };
            if let Some(authority) = candidate_authority {
                self.codex
                    .begin_candidate_artifact_cancel(&authority)
                    .map_err(|_| codex_model_error())?;
                self.codex
                    .cancel_candidate_artifact(&authority)
                    .map_err(|_| codex_model_error())?;
                self.pending_candidates.remove(&key);
            }
            let _ = self.codex.interrupt(&thread_id, &now).await;
            self.flush_codex_execution_messages().await?;
        }
        let ack = JobCancelAckMessage {
            error: None,
            kind: JobCancelAckMessageKind::JobCancelAck,
            lease: cancel.lease.clone(),
            message_id: self.next_message_id(),
            request_id: cancel.request_id.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now,
            session_identity: cancel.session_identity.clone(),
            status,
            worker_session_id: cancel.worker_session_id.clone(),
        };
        self.retain_and_send(ExecutionPortMessage::JobCancelAckMessage(ack))
            .await
    }

    async fn forward_runtime_trace(
        &mut self,
        job_id: &str,
        message: RuntimeEventMessage,
    ) -> Result<(), WorkerError> {
        let active = self.active.get(job_id).ok_or_else(|| {
            worker_error(
                WorkerErrorCode::RuntimeTraceMismatch,
                "runtime trace has no active Job",
            )
        })?;
        let sequence = message.event.sequence.0;
        let last_sequence = active.last_event_sequence.0;
        // A recovered adapter owns the durable runtime cursor. When every
        // earlier frame was already acknowledged there is no pending frame
        // from which WorkerMain can reconstruct that cursor, so the first
        // canonical trace resumes at the adapter-provided sequence.
        let resumes_fully_acknowledged_cursor = last_sequence == 0 && sequence > 0;
        if message.lease != active.lease
            || message.worker_session_id != active.worker_session_id
            || message.session_identity != active.session_identity
            || message.codex_thread_id != active.codex_thread_id
            || (sequence > last_sequence.saturating_add(1) && !resumes_fully_acknowledged_cursor)
        {
            return Err(worker_error(
                WorkerErrorCode::RuntimeTraceMismatch,
                "runtime trace identity or sequence differs from the active Job",
            ));
        }
        let delivery =
            self.retain_execution_message(&ExecutionPortMessage::RuntimeEventMessage(message))?;
        // `install_active_run` queues unacknowledged original frames for
        // direct adapter replay, while `flush_durable_execution_deliveries`
        // may have sent the same frame before the adapter poll.  Retaining
        // first still checks that a same-sequence frame is byte-identical;
        // the already-forwarded copy then needs no second transport attempt.
        if sequence <= last_sequence {
            return Ok(());
        }
        self.active
            .get_mut(job_id)
            .ok_or_else(|| {
                worker_error(
                    WorkerErrorCode::RuntimeTraceMismatch,
                    "runtime trace has no active Job",
                )
            })?
            .last_event_sequence = ExecutionAckSequence(sequence);
        self.send_retained_delivery(delivery).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_job(
        &mut self,
        job_id: &str,
        status: ExecutionOutcomeStatus,
        summary: &str,
        artifacts: Vec<ArtifactReference>,
        usage: Option<ExecutionOutcomeUsage>,
        error: Option<ExecutionPortError>,
        now: Instant,
    ) -> Result<(), WorkerError> {
        let active = self.active.get(job_id).cloned().ok_or_else(|| {
            worker_error(
                WorkerErrorCode::UnexpectedMessage,
                "terminal result has no active Job",
            )
        })?;
        if status == ExecutionOutcomeStatus::Succeeded
            && candidate_artifact_role(&active.job.execution_profile)
        {
            let accepted = self
                .pending_candidates
                .get(job_id)
                .and_then(|pending| pending.artifact.as_ref());
            if artifacts.len() != 1 || accepted != artifacts.first() {
                return Err(candidate_artifact_error(
                    "candidate-producing success requires one final acknowledged candidate reference",
                ));
            }
        }
        let close_reason = match status {
            ExecutionOutcomeStatus::Succeeded => WorkspaceCloseReason::Completed,
            ExecutionOutcomeStatus::Cancelled => WorkspaceCloseReason::Cancelled,
            ExecutionOutcomeStatus::Failed | ExecutionOutcomeStatus::InfrastructureError => {
                WorkspaceCloseReason::Failed
            }
        };
        let outcome = JobOutcomeMessage {
            kind: JobOutcomeMessageKind::JobOutcome,
            lease: active.lease.clone(),
            message_id: self.next_message_id(),
            outcome: ExecutionOutcome {
                artifacts,
                codex_thread_id: Some(active.codex_thread_id.clone()),
                error,
                finished_at: now.clone(),
                last_event_sequence: active.last_event_sequence.clone(),
                status,
                summary: summary.to_owned(),
                usage,
            },
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now,
            session_identity: active.session_identity.clone(),
            worker_session_id: active.worker_session_id.clone(),
        };
        let delivery = self
            .codex
            .retain_job_outcome(&active.codex_thread_id, &outcome)
            .map_err(|_| codex_model_error())?;
        // Retain the terminal outcome before consuming the checkout.  If the
        // process stops between these durable boundaries, the accepted
        // candidate and the outcome remain discoverable and the original
        // mutable workspace is still recoverable on the next dispatch.
        self.workspaces
            .close_job_if_open(&active.job.job_id, close_reason)
            .map_err(|_| workspace_error())?;
        if let Some(record) = self.dispatches.get_mut(job_id) {
            record.terminal = true;
        }
        self.active.remove(job_id);
        let _ = self.codex.close_thread(&active.codex_thread_id).await;
        self.send_retained_delivery(delivery).await?;
        self.flush_codex_execution_messages().await
    }

    async fn send_dispatch_result(
        &mut self,
        dispatch: &JobDispatchMessage,
        status: JobDispatchResultMessageStatus,
        worker_session_id: Option<WorkerSessionId>,
        error: Option<ExecutionPortError>,
        now: Instant,
    ) -> Result<(), WorkerError> {
        let result = JobDispatchResultMessage {
            error,
            job_id: dispatch.job.job_id.clone(),
            kind: JobDispatchResultMessageKind::JobDispatchResult,
            lease: dispatch.lease.clone(),
            message_id: self.next_message_id(),
            payload_digest: dispatch.job.payload_digest.clone(),
            request_id: dispatch.request_id.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now,
            status,
            worker_session_id,
        };
        self.retain_and_send(ExecutionPortMessage::JobDispatchResultMessage(result))
            .await
    }

    fn next_message_id(&mut self) -> ExecutionMessageId {
        self.message_sequence = self.message_sequence.saturating_add(1);
        ExecutionMessageId(format!("xmsg_{:026}", self.message_sequence))
    }

    fn next_request_id(&mut self) -> RequestId {
        self.request_sequence = self.request_sequence.saturating_add(1);
        let sequence = self.request_sequence;
        if let Some(namespace) = &self.registration_request_namespace {
            let mut digest = Sha256::new();
            digest.update(b"winwincode.worker-registration-request.v1\0");
            digest.update((namespace.len() as u64).to_be_bytes());
            digest.update(namespace.as_bytes());
            digest.update(sequence.to_be_bytes());
            return RequestId(format!(
                "req_{}",
                &format!("{:x}", digest.finalize())[..26].to_ascii_uppercase()
            ));
        }
        RequestId(format!("req_{sequence:026}"))
    }

    fn worker_session_id(
        &self,
        dispatch: &JobDispatchMessage,
        run_key: &CodexRunKey,
    ) -> Result<WorkerSessionId, WorkerError> {
        let (worker_session_id, _) = canonical_dispatch_session_identity(
            &self.config.worker_id,
            &self.config.worker_instance_id,
            dispatch,
        )?;
        // The run key is already validated by the dispatch path. Keep the
        // parameter in this private seam because callers use it to derive the
        // Codex thread immediately beside the WorkerSession identity.
        debug_assert_eq!(run_key, &CodexRunKey::from_dispatch(dispatch));
        Ok(worker_session_id)
    }

    async fn accept_approval_decision(
        &mut self,
        decision: &ApprovalDecisionMessage,
        received_at: &Instant,
    ) -> Result<(), WorkerError> {
        self.codex
            .accept_approval_decision(decision, received_at)
            .await
            .map_err(|_| codex_model_error())?;
        self.codex
            .accept_execution_delivery_ack(&ExecutionPortMessage::ApprovalDecisionMessage(
                decision.clone(),
            ))
            .map_err(|_| codex_model_error())
    }

    async fn accept_input_response(
        &mut self,
        response: &InputResponseMessage,
        received_at: &Instant,
    ) -> Result<(), WorkerError> {
        if let Err(_error) = self
            .codex
            .accept_input_response(response, received_at)
            .await
        {
            return Err(codex_model_error());
        }
        self.codex
            .accept_execution_delivery_ack(&ExecutionPortMessage::InputResponseMessage(
                response.clone(),
            ))
            .map_err(|_| codex_model_error())
    }

    async fn replay_runtime(
        &mut self,
        request: &RuntimeReplayRequestMessage,
    ) -> Result<(), WorkerError> {
        let deliveries = self
            .codex
            .replay_execution_deliveries(request)
            .map_err(|_| codex_model_error())?;
        for delivery in deliveries {
            self.send_retained_delivery(delivery).await?;
        }
        Ok(())
    }

    fn retain_execution_message(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, WorkerError> {
        self.codex
            .retain_execution_delivery(message)
            .map_err(|_| codex_model_error())
    }

    fn recovered_registration_delivery(
        &mut self,
    ) -> Result<Option<DurableExecutionDelivery>, WorkerError> {
        Ok(self
            .codex
            .pending_execution_deliveries()
            .map_err(|_| codex_model_error())?
            .into_iter()
            .find(|delivery| {
                matches!(
                    &delivery.message,
                    ExecutionPortMessage::WorkerRegisterMessage(register)
                        if register.worker_id == self.config.worker_id
                            && register.worker_instance_id == self.config.worker_instance_id
                            && register.started_at == self.config.started_at
                )
            }))
    }

    /// Removes registration frames retained by a predecessor process before
    /// creating this process's replacement registration.  Such frames cannot
    /// be replayed: the Control Plane correctly answers with the predecessor
    /// instance, which this Worker must reject.  Treating the frame as an
    /// internal superseded receipt preserves the durable outbox invariant for
    /// every other response-bearing frame.
    fn retire_stale_registration_deliveries(&mut self) -> Result<(), WorkerError> {
        let stale = self
            .codex
            .pending_execution_deliveries()
            .map_err(|_| codex_model_error())?
            .into_iter()
            .filter_map(|delivery| {
                let ExecutionPortMessage::WorkerRegisterMessage(register) = &delivery.message
                else {
                    return None;
                };
                (register.worker_id == self.config.worker_id
                    && (register.worker_instance_id != self.config.worker_instance_id
                        || register.started_at != self.config.started_at))
                    .then_some(register.clone())
            })
            .collect::<Vec<_>>();
        for register in stale {
            let acknowledgement = WorkerRegistrationResultMessage {
                error: None,
                heartbeat_interval_ms: 1,
                kind: WorkerRegistrationResultMessageKind::WorkerRegistrationResult,
                lease_recovery: WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
                message_id: register.message_id.clone(),
                request_id: register.request_id.clone(),
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: register.sent_at.clone(),
                server_time: register.sent_at.clone(),
                status: WorkerRegistrationResultMessageStatus::Accepted,
                worker_id: register.worker_id,
                worker_instance_id: register.worker_instance_id,
            };
            self.codex
                .accept_execution_delivery_ack(
                    &ExecutionPortMessage::WorkerRegistrationResultMessage(acknowledgement),
                )
                .map_err(|_| codex_model_error())?;
        }
        Ok(())
    }

    fn recovered_runtime_cursor(
        &mut self,
        session_identity: &SessionIdentity,
    ) -> Result<ExecutionAckSequence, WorkerError> {
        let first_unacknowledged = self
            .codex
            .pending_execution_deliveries()
            .map_err(|_| codex_model_error())?
            .into_iter()
            .filter_map(|delivery| {
                let ExecutionPortMessage::RuntimeEventMessage(event) = delivery.message else {
                    return None;
                };
                (event.session_identity == *session_identity && event.event.sequence.0 > 0)
                    .then_some(event.event.sequence.0)
            })
            .min();
        Ok(ExecutionAckSequence(
            first_unacknowledged.map_or(0, |sequence| sequence.saturating_sub(1)),
        ))
    }

    fn recovered_core_interaction_pending(&mut self, job_id: &str) -> Result<bool, WorkerError> {
        Ok(self
            .codex
            .pending_execution_deliveries()
            .map_err(|_| codex_model_error())?
            .into_iter()
            .any(|delivery| {
                active_delivery_job_id(&delivery.message) == Some(job_id)
                    && matches!(
                        delivery.message,
                        ExecutionPortMessage::ApprovalRequestMessage(_)
                            | ExecutionPortMessage::InputRequestMessage(_)
                            | ExecutionPortMessage::ActionEnforcementRequestMessage(_)
                    )
            }))
    }

    async fn retain_and_send(&mut self, message: ExecutionPortMessage) -> Result<(), WorkerError> {
        let delivery = self.retain_execution_message(&message)?;
        // A recovered request can be present in the durable outbox before the
        // embedded Core re-emits the same semantic event.  The first flush in
        // this scheduling turn already sent the exact retained bytes; do not
        // send that delivery a second time when the Core event is drained.
        let recovery_duplicate = matches!(
            &message,
            ExecutionPortMessage::ApprovalRequestMessage(_)
                | ExecutionPortMessage::InputRequestMessage(_)
                | ExecutionPortMessage::ActionEnforcementRequestMessage(_)
        ) && active_delivery_job_id(&message)
            .is_some_and(|job_id| self.deferred_core_interaction_jobs.contains(job_id))
            && self
                .recovery_sent_delivery_ids
                .contains(&delivery.delivery_id);
        if self.sent_delivery_ids.contains(&delivery.delivery_id) || recovery_duplicate {
            return Ok(());
        }
        self.send_retained_delivery(delivery).await
    }

    async fn send_retained_delivery(
        &mut self,
        delivery: DurableExecutionDelivery,
    ) -> Result<(), WorkerError> {
        self.port
            .send(delivery.message)
            .await
            .map_err(|_| execution_port_error())?;
        self.codex
            .record_execution_delivery_sent(&delivery.delivery_id)
            .map_err(|_| codex_model_error())?;
        self.sent_delivery_ids.insert(delivery.delivery_id);
        Ok(())
    }

    async fn flush_durable_execution_deliveries(&mut self) -> Result<(), WorkerError> {
        self.flush_durable_execution_deliveries_with_core_replay(false)
            .await
    }

    /// Flushes durable frames after binding a recovered run.  Interactive
    /// requests are deliberately left for the resumed Core event: sending the
    /// retained request here and then sending the same retained request again
    /// when Core re-emits its blocking event would create two visible prompts
    /// in one recovery turn.
    async fn flush_recovered_durable_execution_deliveries(&mut self) -> Result<(), WorkerError> {
        self.flush_durable_execution_deliveries_with_core_replay(true)
            .await
    }

    async fn flush_durable_execution_deliveries_with_core_replay(
        &mut self,
        replay_core_interactions: bool,
    ) -> Result<(), WorkerError> {
        let deliveries = self
            .codex
            .pending_execution_deliveries()
            .map_err(|_| codex_model_error())?;
        let mut blocked_inactive_jobs = HashSet::new();
        for delivery in deliveries {
            if self.sent_delivery_ids.contains(&delivery.delivery_id) {
                continue;
            }
            if matches!(&delivery.message, ExecutionPortMessage::ModelOpenMessage(_)) {
                continue;
            }
            let recovered_interaction = (replay_core_interactions || self.defer_core_interactions)
                && matches!(
                    &delivery.message,
                    ExecutionPortMessage::ApprovalRequestMessage(_)
                        | ExecutionPortMessage::InputRequestMessage(_)
                        | ExecutionPortMessage::ActionEnforcementRequestMessage(_)
                )
                && active_delivery_job_id(&delivery.message)
                    .is_some_and(|job_id| self.deferred_core_interaction_jobs.contains(job_id));
            if recovered_interaction
                && self
                    .recovery_sent_delivery_ids
                    .contains(&delivery.delivery_id)
            {
                continue;
            }
            if let Some(job_id) = active_delivery_job_id(&delivery.message)
                && !self.active.contains_key(job_id)
            {
                match &delivery.message {
                    ExecutionPortMessage::RuntimeEventMessage(_) => {
                        blocked_inactive_jobs.insert(job_id.to_owned());
                        continue;
                    }
                    ExecutionPortMessage::JobOutcomeMessage(_)
                        if blocked_inactive_jobs.contains(job_id) =>
                    {
                        continue;
                    }
                    ExecutionPortMessage::JobOutcomeMessage(_) => {}
                    _ => continue,
                }
            }
            if candidate_delivery_job_id(&delivery.message).is_some_and(|job_id| {
                !self.pending_candidates.contains_key(job_id)
                    || !self
                        .active
                        .get(job_id)
                        .is_some_and(|active| active.lifecycle == ActiveJobLifecycle::Running)
            }) {
                continue;
            }
            if candidate_delivery_job_id(&delivery.message).is_some()
                && !self
                    .codex
                    .candidate_artifact_delivery_allowed(&delivery.message)
                    .map_err(|_| codex_model_error())?
            {
                continue;
            }
            let runtime_cursor = match &delivery.message {
                ExecutionPortMessage::RuntimeEventMessage(event) => {
                    Some((event.lease.job_id.0.clone(), event.event.sequence.0))
                }
                _ => None,
            };
            let delivery_id = delivery.delivery_id.clone();
            self.send_retained_delivery(delivery).await?;
            if recovered_interaction {
                self.recovery_sent_delivery_ids.insert(delivery_id);
            }
            if let Some((job_id, sequence)) = runtime_cursor
                && let Some(active) = self.active.get_mut(&job_id)
                && sequence > active.last_event_sequence.0
            {
                active.last_event_sequence = ExecutionAckSequence(sequence);
            }
        }
        Ok(())
    }

    async fn flush_codex_execution_messages(&mut self) -> Result<(), WorkerError> {
        let messages = self
            .codex
            .take_execution_messages()
            .map_err(|_| codex_model_error())?;
        for message in messages {
            if !matches!(
                message,
                ExecutionPortMessage::ModelOpenMessage(_)
                    | ExecutionPortMessage::ModelAckMessage(_)
                    | ExecutionPortMessage::ActionEnforcementRequestMessage(_)
                    | ExecutionPortMessage::ApprovalRequestMessage(_)
                    | ExecutionPortMessage::InputRequestMessage(_)
                    | ExecutionPortMessage::RuntimeEventMessage(_)
            ) {
                return Err(codex_model_error());
            }
            let terminal_model_ack = matches!(
                &message,
                ExecutionPortMessage::ModelAckMessage(acknowledgement)
                    if acknowledgement.error.is_some()
            );
            if matches!(
                &message,
                ExecutionPortMessage::ApprovalRequestMessage(_)
                    | ExecutionPortMessage::InputRequestMessage(_)
                    | ExecutionPortMessage::ActionEnforcementRequestMessage(_)
            ) && let Some(job_id) = active_delivery_job_id(&message)
            {
                self.core_interaction_jobs.insert(job_id.to_owned());
            }
            let acknowledgement = terminal_model_ack.then(|| message.clone());
            self.retain_and_send(message).await?;
            if let Some(acknowledgement) = acknowledgement {
                self.codex
                    .accept_execution_delivery_ack(&acknowledgement)
                    .map_err(|_| codex_model_error())?;
            }
        }
        Ok(())
    }
}

fn namespaced_registration_message_id(namespace: &str) -> ExecutionMessageId {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.worker-registration-message.v1\0");
    digest.update((namespace.len() as u64).to_be_bytes());
    digest.update(namespace.as_bytes());
    ExecutionMessageId(format!(
        "xmsg_{}",
        &format!("{:x}", digest.finalize())[..26].to_ascii_uppercase()
    ))
}

fn active_delivery_job_id(message: &ExecutionPortMessage) -> Option<&str> {
    match message {
        ExecutionPortMessage::RuntimeEventMessage(event) => Some(&event.lease.job_id.0),
        ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(&outcome.lease.job_id.0),
        ExecutionPortMessage::ModelOpenMessage(open) => Some(&open.lease.job_id.0),
        ExecutionPortMessage::ActionEnforcementRequestMessage(request) => Some(&request.job_id.0),
        ExecutionPortMessage::ApprovalRequestMessage(request) => Some(&request.lease.job_id.0),
        ExecutionPortMessage::InputRequestMessage(request) => Some(&request.lease.job_id.0),
        _ => None,
    }
}

fn candidate_delivery_job_id(message: &ExecutionPortMessage) -> Option<&str> {
    match message {
        ExecutionPortMessage::ArtifactOpenMessage(open)
            if open.artifact.kind == ArtifactKind::Candidate
                && open.artifact.media_type == stage_product::CANDIDATE_MEDIA_TYPE =>
        {
            Some(&open.lease.job_id.0)
        }
        ExecutionPortMessage::ArtifactChunkMessage(chunk)
            if chunk.payload.content_type == stage_product::CANDIDATE_MEDIA_TYPE =>
        {
            Some(&chunk.lease.job_id.0)
        }
        _ => None,
    }
}

/// Resumes the numeric Worker message-id cursor from the durable adapter
/// outbox.  Transport frames intentionally remain retained after a successful
/// send, so a replacement Worker must not start at `xmsg_...01` and collide
/// with an earlier frame that has a different timestamp or lifecycle status.
/// Non-numeric ids belong to externally supplied protocol frames and do not
/// participate in this local cursor.
fn recover_message_sequence<Codex: CodexCoreAdapter>(codex: &mut Codex) -> u64 {
    codex.recovered_message_sequence().unwrap_or(0)
}

fn recover_heartbeat_sequence<Codex: CodexCoreAdapter>(
    codex: &mut Codex,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> i64 {
    codex
        .recovered_heartbeat_sequence(worker_id, worker_instance_id)
        .unwrap_or(0)
        .max(0)
}

fn candidate_writer_role(profile: &str) -> bool {
    matches!(profile, "executor" | "remediator")
}

fn verification_artifact_role(profile: &str) -> bool {
    matches!(profile, "reviewer" | "verifier" | "adversarial-verifier")
}

fn candidate_artifact_role(profile: &str) -> bool {
    candidate_writer_role(profile) || verification_artifact_role(profile)
}

fn scope_identity(scope: &ExecutionScope) -> (ProductSessionId, Option<StageRunId>) {
    match scope {
        ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
            product_session_id,
            ..
        }) => (product_session_id.clone(), None),
        ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            product_session_id,
            stage_run_id,
            ..
        }) => (product_session_id.clone(), Some(stage_run_id.clone())),
    }
}

fn dispatch_authority_rejection(
    config: &WorkerConfig,
    dispatch: &JobDispatchMessage,
    now: &Instant,
) -> Option<(JobDispatchResultMessageStatus, ExecutionPortError)> {
    if dispatch.lease.worker_id != config.worker_id
        || dispatch.lease.worker_instance_id != config.worker_instance_id
    {
        return Some((
            JobDispatchResultMessageStatus::RejectedWorkerInstance,
            port_error(
                ExecutionPortErrorCode::WorkerInstanceChanged,
                "dispatch belongs to another Worker process",
                false,
            ),
        ));
    }
    if dispatch.job.job_id != dispatch.lease.job_id
        || dispatch.job.attempt != dispatch.lease.attempt
        || dispatch.lease.issued_at.0 >= dispatch.lease.expires_at.0
    {
        return Some((
            JobDispatchResultMessageStatus::RejectedStaleFencingToken,
            port_error(
                ExecutionPortErrorCode::StaleFencingToken,
                "dispatch Job, attempt, or lease authority is inconsistent",
                false,
            ),
        ));
    }
    if now.0 >= dispatch.lease.expires_at.0 {
        return Some((
            JobDispatchResultMessageStatus::RejectedExpiredLease,
            port_error(
                ExecutionPortErrorCode::LeaseExpired,
                "dispatch lease has expired",
                false,
            ),
        ));
    }
    None
}

fn candidate_artifact_authority(
    active: &ActiveJob,
) -> Result<CandidateArtifactAuthority, WorkerError> {
    let job_digest = winwincode_codex::stage_product::stage_product_job_digest(&active.job)
        .map_err(|_| {
            candidate_artifact_error("writer Job cannot be sealed for candidate replay")
        })?;
    let logical_job_digest = winwincode_codex::stage_product::stage_product_logical_job_digest(
        &active.job,
    )
    .map_err(|_| {
        candidate_artifact_error("writer logical Job cannot be sealed for candidate replay")
    })?;
    Ok(CandidateArtifactAuthority {
        job_digest,
        logical_job_digest,
        execution_profile: active.job.execution_profile.clone(),
        scope: active.job.scope.clone(),
        lease: active.lease.clone(),
        worker_session_id: active.worker_session_id.clone(),
        session_identity: active.session_identity.clone(),
    })
}

fn replacement_candidate_authority_matches(
    replacement: &ExecutionJobReplacementAuthority,
    predecessor: &CandidateArtifactAuthority,
    successor: &CandidateArtifactAuthority,
) -> bool {
    replacement
        .predecessor_session_identity
        .as_ref()
        .is_some_and(|session| {
            predecessor.execution_profile == successor.execution_profile
                && predecessor.logical_job_digest == replacement.logical_job_digest
                && successor.logical_job_digest == replacement.logical_job_digest
                && predecessor.scope == replacement.scope
                && successor.scope == replacement.scope
                && predecessor.lease == replacement.predecessor_lease
                && predecessor.worker_session_id == session.worker_session_id
                && predecessor.session_identity == *session
                && successor.lease == replacement.successor_lease
                && predecessor.lease.job_id == successor.lease.job_id
                && predecessor.lease.attempt.saturating_add(1) == successor.lease.attempt
        })
}

fn port_error(code: ExecutionPortErrorCode, message: &str, retryable: bool) -> ExecutionPortError {
    ExecutionPortError {
        code,
        message: message.to_owned(),
        retryable,
    }
}

fn worker_error(code: WorkerErrorCode, reason: &str) -> WorkerError {
    WorkerError {
        code,
        reason: reason.to_owned(),
    }
}

fn candidate_artifact_error(reason: &str) -> WorkerError {
    worker_error(WorkerErrorCode::CandidateArtifactMismatch, reason)
}

fn workspace_error() -> WorkerError {
    worker_error(
        WorkerErrorCode::Workspace,
        "detached Job workspace operation failed",
    )
}

fn execution_port_error() -> WorkerError {
    worker_error(
        WorkerErrorCode::ExecutionPort,
        "ExecutionPort transport failed",
    )
}

fn codex_model_error() -> WorkerError {
    worker_error(
        WorkerErrorCode::UnexpectedMessage,
        "embedded Codex model bridge rejected an ExecutionPort frame",
    )
}

/// Stable binary identity emitted by `winwincode-worker --check`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerBinaryIdentity {
    /// Binary role.
    pub role: &'static str,
    /// Canonical `ExecutionPort` schema version.
    pub execution_port: &'static str,
    /// Execution authority implementation.
    pub execution_kernel: &'static str,
    /// Whether a CLI or external-agent fallback exists.
    pub external_fallback: bool,
    /// Whether this binary accepts `--managed-session <config-file>` (plan
    /// §14.4) and reports the managed session identity surface.
    pub managed_session: bool,
}

/// Returns compile-time Worker role identity without starting runtime services.
#[must_use]
pub const fn binary_identity() -> WorkerBinaryIdentity {
    WorkerBinaryIdentity {
        role: "execution-worker",
        execution_port: "winwincode/v1",
        execution_kernel: "embedded-codex-core",
        external_fallback: false,
        managed_session: true,
    }
}

// Keep these exact canonical types reachable from this crate's public API so
// workspace and integration modules never introduce parallel event identities.
#[allow(dead_code)]
fn canonical_runtime_identity_types(
    _event_id: ExecutionEventId,
    _sequence: ExecutionSequence,
    _category: ExecutionEventCategory,
) {
}
