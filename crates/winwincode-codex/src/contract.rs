// SPDX-License-Identifier: Apache-2.0

//! Canonical Worker-facing contract for the embedded Codex adapter.

use std::{fmt, future::Future, path::Path, sync::Arc};

use sha2::{Digest as _, Sha256};
use winwincode_domain::{CodexThreadId, Instant, WorkerId, WorkerInstanceId, WorkerSessionId};
use winwincode_execution_port::{
    generated::{
        ActionEnforcementReceiptMessage, ApprovalDecisionMessage, ArtifactAckMessage,
        ArtifactReference, ExecutionJob, ExecutionLeaseStamp, ExecutionOutcomeUsage,
        ExecutionPortMessage, InputResponseMessage, JobDispatchMessage, JobOutcomeMessage,
        ModelChunkMessage, RuntimeEventMessage, RuntimeReplayRequestMessage,
    },
    runtime_trace_outbox::{RuntimeTraceInputError, SecretSafeTraceSummary},
};

use crate::candidate_artifact_outbox::{
    CandidateArtifactAckOutcome, CandidateArtifactAuthority, CandidateArtifactUpload,
    RetainedCandidateArtifact,
};

/// Stable create-or-load identity for one Codex thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodexRunKey {
    pub job_id: winwincode_domain::ExecutionJobId,
    pub attempt: i64,
    pub fencing_token: winwincode_domain::FencingToken,
    pub payload_digest: winwincode_domain::Sha256Digest,
}

impl CodexRunKey {
    /// Builds the only run identity from an already validated dispatch.
    #[must_use]
    pub fn from_dispatch(dispatch: &JobDispatchMessage) -> Self {
        Self {
            job_id: dispatch.job.job_id.clone(),
            attempt: dispatch.job.attempt,
            fencing_token: dispatch.lease.fencing_token.clone(),
            payload_digest: dispatch.job.payload_digest.clone(),
        }
    }

    /// Derives the sole stable Codex thread identity for this exact dispatch.
    ///
    /// # Errors
    ///
    /// Returns an opaque contract error when the canonical identity facts
    /// cannot be encoded.
    pub fn canonical_thread_id(&self) -> Result<CodexThreadId, CodexRunKeyError> {
        let digest = format!("{:x}", Sha256::digest(self.canonical_bytes()?));
        Ok(CodexThreadId(format!(
            "cdx_{}",
            &digest[..26].to_ascii_uppercase()
        )))
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, CodexRunKeyError> {
        serde_json::to_vec(&(
            &self.job_id,
            self.attempt,
            &self.fencing_token,
            &self.payload_digest,
        ))
        .map_err(|_| CodexRunKeyError)
    }
}

/// Secret-safe failure to derive a canonical Codex run identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexRunKeyError;

impl fmt::Display for CodexRunKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("canonical Codex run identity is invalid")
    }
}

impl std::error::Error for CodexRunKeyError {}

/// Start data passed to the sole embedded Codex adapter.
#[derive(Debug, Clone, Copy)]
pub struct CodexThreadStart<'job> {
    pub run_key: &'job CodexRunKey,
    pub job: &'job ExecutionJob,
    pub lease: &'job ExecutionLeaseStamp,
    pub worker_session_id: &'job WorkerSessionId,
    /// Exact detached checkout sealed to this Job before Kernel session open.
    pub workspace: &'job Path,
}

/// Secret-safe terminal result emitted by Codex Core.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexTurnCompletion {
    pub summary: SecretSafeTraceSummary,
    pub artifacts: Vec<ArtifactReference>,
    pub usage: ExecutionOutcomeUsage,
}

/// One result from polling the embedded Codex adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexPoll {
    Pending,
    RuntimeTrace(Box<RuntimeEventMessage>),
    Completed(CodexTurnCompletion),
    Failed(SecretSafeTraceSummary),
    Cancelled(SecretSafeTraceSummary),
    InfrastructureFailed(SecretSafeTraceSummary),
}

/// One canonical `ExecutionPort` frame retained before its first delivery attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct DurableExecutionDelivery {
    /// Stable outbox identity. Exact retries retain this value and the original frame bytes.
    pub delivery_id: String,
    /// Original generated message decoded from the durable canonical frame.
    pub message: ExecutionPortMessage,
}

/// Optional local transport for an action request emitted while Core is
/// awaiting the signed Control Plane receipt.
///
/// Separated Worker transports leave this unset and deliver the request and
/// receipt through the ordinary durable `ExecutionPort` path.  The local
/// launcher installs the exact same typed Control Plane core here so a
/// synchronous in-process composition cannot deadlock its Worker poll while
/// waiting for the response that the poll itself must flush.
pub type ActionRequestTransport =
    Arc<dyn Fn(ExecutionPortMessage) -> Result<Vec<ExecutionPortMessage>, ()> + Send + Sync>;

/// Sole embedded Codex lifecycle seam used by the Execution Worker.
pub trait CodexCoreAdapter {
    type Error: Send + 'static;

    fn ensure_thread(
        &mut self,
        start: CodexThreadStart<'_>,
    ) -> impl Future<Output = Result<CodexThreadId, Self::Error>> + Send;

    /// Advances adapter-owned trusted time immediately before a turn is
    /// submitted.  Production adapters use this hook to make the lease and
    /// action gates observe the Worker dispatch timestamp before any Kernel
    /// model request can reach the Provider.  Test and lightweight adapters
    /// may retain the no-op default.
    ///
    /// # Errors
    ///
    /// The default implementation always succeeds.
    fn observe_now(&mut self, _now: &Instant) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Installs the local-only action request transport.  The default keeps
    /// remote/test adapters on the ordinary typed response path.
    fn install_action_request_transport(&mut self, _transport: ActionRequestTransport) {}

    fn submit_turn(
        &mut self,
        thread_id: &CodexThreadId,
        goal: &str,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn poll(
        &mut self,
        thread_id: &CodexThreadId,
        now: &Instant,
    ) -> impl Future<Output = Result<CodexPoll, Self::Error>> + Send;

    fn accept_model_chunk(
        &mut self,
        chunk: &ModelChunkMessage,
        received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn accept_action_receipt(
        &mut self,
        receipt: &ActionEnforcementReceiptMessage,
        received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Resolves one previously retained approval request through the adapter's
    /// exact durable operation mapping.
    fn accept_approval_decision(
        &mut self,
        decision: &ApprovalDecisionMessage,
        received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Resolves one previously retained interactive-input request through the
    /// adapter's exact durable operation mapping.
    fn accept_input_response(
        &mut self,
        response: &InputResponseMessage,
        received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Durably retains one outbound generated message before transport send.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when the canonical frame cannot be retained.
    fn retain_execution_delivery(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error>;

    /// Returns retained frames whose next delivery attempt is pending, in durable order.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when the durable delivery ledger cannot be read.
    fn pending_execution_deliveries(
        &mut self,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error>;

    /// Returns the highest numeric Worker message id retained in durable
    /// storage, including transport-only frames whose send attempt already
    /// succeeded. A replacement Worker uses this cursor before allocating a
    /// new transport identity.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when the durable delivery ledger cannot be
    /// read.
    fn recovered_message_sequence(&mut self) -> Result<u64, Self::Error> {
        Ok(0)
    }

    /// Returns the highest heartbeat sequence retained in the Worker's
    /// canonical durable store. A replacement process with the same Worker
    /// instance continues after this cursor rather than changing an already
    /// accepted Registry request.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when the durable transport cursor cannot be
    /// read.
    fn recovered_heartbeat_sequence(
        &mut self,
        _worker_id: &WorkerId,
        _worker_instance_id: &WorkerInstanceId,
    ) -> Result<i64, Self::Error> {
        Ok(0)
    }

    /// Records a successful transport attempt without discarding acknowledgement-required frames.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when the delivery identity is unknown or cannot be updated.
    fn record_execution_delivery_sent(&mut self, delivery_id: &str) -> Result<(), Self::Error>;

    /// Applies a canonical response or acknowledgement after its domain validation succeeds.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when the acknowledgement conflicts with the retained frame.
    fn accept_execution_delivery_ack(
        &mut self,
        acknowledgement: &ExecutionPortMessage,
    ) -> Result<(), Self::Error>;

    /// Atomically retains one exact candidate Artifact and all original upload frames.
    ///
    /// # Errors
    ///
    /// Rejects changed bytes, Job, role, lease, or session authority.
    fn retain_candidate_artifact(
        &mut self,
        upload: &CandidateArtifactUpload,
    ) -> Result<RetainedCandidateArtifact, Self::Error>;

    /// Applies one typed Artifact acknowledgement to the exact durable upload.
    ///
    /// # Errors
    ///
    /// Rejects stale, foreign, rejected, or non-contiguous acknowledgements.
    fn accept_candidate_artifact_ack(
        &mut self,
        acknowledgement: &ArtifactAckMessage,
    ) -> Result<CandidateArtifactAckOutcome, Self::Error>;

    /// Recovers the final accepted candidate reference after restart.
    ///
    /// # Errors
    ///
    /// Rejects an authority which differs from the original upload.
    fn accepted_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<Option<ArtifactReference>, Self::Error>;

    /// Commits a cancellation intent before candidate frames are removed.
    ///
    /// Adapters with durable candidate storage should make this idempotent so
    /// a retry after a process stop can suppress replay before cleanup.
    /// Lightweight adapters may use the default no-op because the Worker also
    /// gates candidate delivery on its in-memory cancelling state.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when the cancellation intent cannot be
    /// committed.
    fn begin_candidate_artifact_cancel(
        &mut self,
        _authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Checks the durable cancellation marker before sending a candidate frame.
    ///
    /// A missing candidate record is represented as `false` by durable
    /// adapters, preventing stale frames from becoming a new upload.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when the durable candidate record cannot be
    /// inspected.
    fn candidate_artifact_delivery_allowed(
        &mut self,
        _message: &ExecutionPortMessage,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    /// Removes an unaccepted candidate upload and every retained upload frame.
    ///
    /// # Errors
    ///
    /// Rejects changed authority or cancellation after final acceptance.
    fn cancel_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error>;

    /// Requeues original runtime frames strictly after the request cursor.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when the replay cursor is invalid or the ledger cannot be read.
    fn replay_execution_deliveries(
        &mut self,
        request: &RuntimeReplayRequestMessage,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error>;

    /// Atomically finalizes the adapter run and retains its first canonical outcome frame.
    /// Exact retries return the originally retained frame.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when finalization or durable retention cannot be committed.
    fn retain_job_outcome(
        &mut self,
        thread_id: &CodexThreadId,
        outcome: &JobOutcomeMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error>;

    /// Drains newly produced adapter messages for durable retention by the worker.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when pending adapter messages cannot be collected.
    fn take_execution_messages(&mut self) -> Result<Vec<ExecutionPortMessage>, Self::Error>;

    fn interrupt(
        &mut self,
        thread_id: &CodexThreadId,
        interrupted_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn close_thread(
        &mut self,
        thread_id: &CodexThreadId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Outbound canonical `ExecutionPort` used identically by local and remote IO.
pub trait WorkerExecutionPort {
    type Error;

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>>;
}

/// Builds a secret-safe runtime summary through the canonical trace validator.
///
/// # Errors
///
/// Rejects empty or credential-shaped summaries.
pub fn secret_safe_runtime_summary(
    value: impl Into<String>,
) -> Result<SecretSafeTraceSummary, RuntimeTraceInputError> {
    SecretSafeTraceSummary::new(value)
}
