// SPDX-License-Identifier: Apache-2.0

//! Deterministic recovery planning for browser reconnects, Control Plane
//! restarts, and Worker replacement.
//!
//! This module plans exact mutations from the existing Worker-slot and
//! runtime-replay authority types. The caller persists
//! [`RecoveryRouterSnapshot`] with its recovery transaction and then dispatches
//! the returned replay command.

use std::collections::HashMap;
use std::fmt;

use winwincode_domain::{
    CodexThreadId, ExecutionMessageId, Instant, ProductSessionId, RequestId, SessionIdentity,
    Sha256Digest, StageRunId,
};
use winwincode_execution_port::generated::ExecutionLeaseStamp;
use winwincode_execution_port::replay::{
    ReplayAuthority, ReplayBatch, ReplayDecision, ReplayError, ReplayFrame, ReplayStateMachine,
    ReplayStore, ReplayStreamKey,
};
use winwincode_execution_port::runtime_replay::RuntimeReplayIdentity;
use winwincode_storage::{
    WorkerSlotAuthority, WorkerSlotCloseRequest, WorkerSlotOpenRequest, WorkerSlotResources,
    WorkerSlotState,
};

use crate::RuntimeReplayRequestCommand;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Lease, Worker process, `WorkerSession`, and `CodexThread` authority for one attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryExecutionAuthority {
    pub slot: WorkerSlotAuthority,
    pub issued_at: Instant,
    pub expires_at: Instant,
}

/// Whether the Codex thread has state that a replacement Worker can resume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadRecoveryCapability {
    /// The checkpoint is durable and the same `CodexThread` may continue.
    TransferableCheckpoint { checkpoint_sha256: Sha256Digest },
    /// State exists only inside the old process; a new thread is required.
    ProcessLocalOnly,
}

/// Persisted facts required to recover one active product execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecoveryState {
    pub product_session_id: ProductSessionId,
    pub stage_run_id: Option<StageRunId>,
    pub browser_stream_id: String,
    pub browser_authorization_epoch: u64,
    pub revision: u64,
    pub slot_revision: u64,
    pub slot_resources: WorkerSlotResources,
    pub authority: RecoveryExecutionAuthority,
    pub confirmed_runtime_sequence: u64,
    pub thread_capability: ThreadRecoveryCapability,
}

/// Replacement lease and command metadata supplied after scheduler placement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecoveryRequest {
    pub request_id: RequestId,
    pub product_session_id: ProductSessionId,
    pub expected_revision: u64,
    pub replacement: RecoveryExecutionAuthority,
    pub recovered_at: Instant,
    /// Required only for a transferable-thread replay request.
    pub replay_message_id: Option<ExecutionMessageId>,
    pub max_replay_events: u64,
}

/// Slot close/open pair applied for every replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerReplacementPlan {
    pub close_old_slot: WorkerSlotCloseRequest,
    pub open_replacement_slot: WorkerSlotOpenRequest,
}

/// Exact recovery action selected from the persisted thread capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionRecoveryPlan {
    /// Continue the same durable thread and replay after the confirmed cursor.
    ResumeTransferableThread {
        worker: WorkerReplacementPlan,
        checkpoint_sha256: Sha256Digest,
        resume_after_sequence: u64,
        runtime_replay: RuntimeReplayRequestCommand,
    },
    /// End the non-transferable thread and start an explicit fresh attempt.
    StartFreshAttempt {
        worker: WorkerReplacementPlan,
        ended_codex_thread_id: CodexThreadId,
        new_codex_thread_id: CodexThreadId,
    },
}

/// Whether a recovery request was newly applied or was an exact replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryWriteStatus {
    Applied,
    Duplicate,
}

/// Complete deterministic output of one recovery request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecoveryReceipt {
    pub status: RecoveryWriteStatus,
    pub request_id: RequestId,
    pub product_session_id: ProductSessionId,
    pub previous_revision: u64,
    pub current_revision: u64,
    pub plan: SessionRecoveryPlan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredRecoveryReceipt {
    request: SessionRecoveryRequest,
    receipt: SessionRecoveryReceipt,
}

/// Cloneable state persisted by the Control Plane before executing a plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryRouterSnapshot {
    sessions: Vec<SessionRecoveryState>,
    receipts: Vec<StoredRecoveryReceipt>,
}

/// Recovery planning failure. No slot or replay command is emitted on error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryRoutingError {
    InvalidField(&'static str),
    DuplicateSession,
    UnknownSession,
    RevisionConflict { expected: u64, actual: u64 },
    IdentityMismatch(&'static str),
    LeaseNotNewer,
    WorkerInstanceNotReplaced,
    WorkerSessionNotReplaced,
    TransferableThreadChanged,
    NonTransferableThreadReused,
    ReplayMetadataRequired,
    ReplayMetadataForbidden,
    IdempotencyConflict,
}

impl fmt::Display for RecoveryRoutingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidField(field) => write!(formatter, "invalid recovery field: {field}"),
            Self::DuplicateSession => formatter.write_str("recovery session is already registered"),
            Self::UnknownSession => formatter.write_str("recovery session is not registered"),
            Self::RevisionConflict { expected, actual } => {
                write!(
                    formatter,
                    "revision conflict: expected {expected}, actual {actual}"
                )
            }
            Self::IdentityMismatch(field) => {
                write!(formatter, "recovery identity mismatch: {field}")
            }
            Self::LeaseNotNewer => {
                formatter.write_str("replacement lease is not a newer attempt and fence")
            }
            Self::WorkerInstanceNotReplaced => {
                formatter.write_str("replacement uses the old Worker process instance")
            }
            Self::WorkerSessionNotReplaced => {
                formatter.write_str("replacement uses the old WorkerSession")
            }
            Self::TransferableThreadChanged => {
                formatter.write_str("transferable recovery changed the CodexThread")
            }
            Self::NonTransferableThreadReused => {
                formatter.write_str("non-transferable recovery reused the CodexThread")
            }
            Self::ReplayMetadataRequired => {
                formatter.write_str("transferable recovery requires runtime replay metadata")
            }
            Self::ReplayMetadataForbidden => {
                formatter.write_str("fresh recovery attempt must not request old runtime replay")
            }
            Self::IdempotencyConflict => {
                formatter.write_str("recovery request ID was replayed with different input")
            }
        }
    }
}

impl std::error::Error for RecoveryRoutingError {}

/// Authority failure returned by the shared runtime replay state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryReplayAuthorityError {
    OldWorkerFenced,
    ForeignIdentity,
    StreamMismatch,
}

/// Exact runtime frame submitted after reconnect or Worker replacement.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryRuntimeEvent {
    pub product_session_id: ProductSessionId,
    pub identity: RuntimeReplayIdentity,
    pub frame: ReplayFrame,
}

/// Browser subscription identity. Authorization epoch prevents a cursor from
/// crossing a login/session authorization change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserRecoveryStream {
    pub product_session_id: ProductSessionId,
    pub stream_id: String,
    pub authorization_epoch: u64,
}

/// Persisted browser cursor used for reconnect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserReconnectRequest {
    pub stream: BrowserRecoveryStream,
    pub after_sequence: u64,
    pub max_events: usize,
}

/// Browser cursor authority failure returned through [`ReplayError`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserReplayAuthorityError {
    UnknownProductSession,
    ForeignStream,
}

/// Recovery aggregate. Persist [`snapshot`](Self::snapshot) atomically with an
/// applied receipt, then execute the returned frozen plan through existing ports.
#[derive(Default)]
pub struct RecoveryRouter {
    sessions: HashMap<ProductSessionId, SessionRecoveryState>,
    receipts: HashMap<RequestId, StoredRecoveryReceipt>,
}

#[allow(clippy::missing_errors_doc)]
impl RecoveryRouter {
    /// Registers one persisted active execution.
    pub fn register_session(
        &mut self,
        state: SessionRecoveryState,
    ) -> Result<(), RecoveryRoutingError> {
        validate_state(&state)?;
        if self.sessions.contains_key(&state.product_session_id) {
            return Err(RecoveryRoutingError::DuplicateSession);
        }
        self.sessions
            .insert(state.product_session_id.clone(), state);
        Ok(())
    }

    /// Restores exactly the previously persisted sessions and receipts.
    pub fn restore(snapshot: RecoveryRouterSnapshot) -> Result<Self, RecoveryRoutingError> {
        let mut router = Self::default();
        for state in snapshot.sessions {
            router.register_session(state)?;
        }
        for stored in snapshot.receipts {
            validate_request(&stored.request)?;
            if router
                .receipts
                .insert(stored.request.request_id.clone(), stored)
                .is_some()
            {
                return Err(RecoveryRoutingError::IdempotencyConflict);
            }
        }
        Ok(router)
    }

    /// Returns the complete durable recovery state in deterministic order.
    #[must_use]
    pub fn snapshot(&self) -> RecoveryRouterSnapshot {
        let mut sessions = self.sessions.values().cloned().collect::<Vec<_>>();
        sessions.sort_unstable_by(|left, right| {
            left.product_session_id.0.cmp(&right.product_session_id.0)
        });
        let mut receipts = self.receipts.values().cloned().collect::<Vec<_>>();
        receipts.sort_unstable_by(|left, right| {
            left.request.request_id.0.cmp(&right.request.request_id.0)
        });
        RecoveryRouterSnapshot { sessions, receipts }
    }

    /// Selects thread continuation or a fresh attempt from persisted capability.
    /// The current authority changes before the plan is returned, fencing the
    /// old Worker from all later runtime-event writes.
    pub fn recover_session(
        &mut self,
        request: &SessionRecoveryRequest,
    ) -> Result<SessionRecoveryReceipt, RecoveryRoutingError> {
        validate_request(request)?;
        if let Some(stored) = self.receipts.get(&request.request_id) {
            if &stored.request != request {
                return Err(RecoveryRoutingError::IdempotencyConflict);
            }
            let mut duplicate = stored.receipt.clone();
            duplicate.status = RecoveryWriteStatus::Duplicate;
            return Ok(duplicate);
        }
        let state = self
            .sessions
            .get_mut(&request.product_session_id)
            .ok_or(RecoveryRoutingError::UnknownSession)?;
        if state.revision != request.expected_revision {
            return Err(RecoveryRoutingError::RevisionConflict {
                expected: request.expected_revision,
                actual: state.revision,
            });
        }
        validate_replacement(state, request)?;
        let previous_revision = state.revision;
        let current_revision = next_revision(previous_revision)?;
        let worker = worker_replacement(state, request);
        let plan = match &state.thread_capability {
            ThreadRecoveryCapability::TransferableCheckpoint { checkpoint_sha256 } => {
                if request.max_replay_events == 0 {
                    return Err(RecoveryRoutingError::InvalidField("maxReplayEvents"));
                }
                let message_id = request
                    .replay_message_id
                    .clone()
                    .ok_or(RecoveryRoutingError::ReplayMetadataRequired)?;
                if request.replacement.slot.codex_thread_id != state.authority.slot.codex_thread_id
                {
                    return Err(RecoveryRoutingError::TransferableThreadChanged);
                }
                SessionRecoveryPlan::ResumeTransferableThread {
                    worker,
                    checkpoint_sha256: checkpoint_sha256.clone(),
                    resume_after_sequence: state.confirmed_runtime_sequence,
                    runtime_replay: RuntimeReplayRequestCommand {
                        job_id: request.replacement.slot.job_id.clone(),
                        max_events: i64::try_from(request.max_replay_events)
                            .map_err(|_| RecoveryRoutingError::InvalidField("maxReplayEvents"))?,
                        message_id,
                        request_id: request.request_id.clone(),
                        sent_at: request.recovered_at.clone(),
                    },
                }
            }
            ThreadRecoveryCapability::ProcessLocalOnly => {
                if request.replay_message_id.is_some() || request.max_replay_events != 0 {
                    return Err(RecoveryRoutingError::ReplayMetadataForbidden);
                }
                if request.replacement.slot.codex_thread_id == state.authority.slot.codex_thread_id
                {
                    return Err(RecoveryRoutingError::NonTransferableThreadReused);
                }
                SessionRecoveryPlan::StartFreshAttempt {
                    worker,
                    ended_codex_thread_id: state.authority.slot.codex_thread_id.clone(),
                    new_codex_thread_id: request.replacement.slot.codex_thread_id.clone(),
                }
            }
        };
        state.authority = request.replacement.clone();
        state.slot_revision = 1;
        state.revision = current_revision;
        if matches!(
            state.thread_capability,
            ThreadRecoveryCapability::ProcessLocalOnly
        ) {
            state.confirmed_runtime_sequence = 0;
        }
        let receipt = SessionRecoveryReceipt {
            status: RecoveryWriteStatus::Applied,
            request_id: request.request_id.clone(),
            product_session_id: request.product_session_id.clone(),
            previous_revision,
            current_revision,
            plan,
        };
        self.receipts.insert(
            request.request_id.clone(),
            StoredRecoveryReceipt {
                request: request.clone(),
                receipt: receipt.clone(),
            },
        );
        Ok(receipt)
    }

    /// Accepts one runtime frame through the shared replay state machine and
    /// the router's current lease/fencing authority.
    pub fn accept_runtime_event<S>(
        &self,
        store: &mut S,
        event: &RecoveryRuntimeEvent,
    ) -> Result<ReplayDecision, ReplayError<RecoveryReplayAuthorityError, S::Error>>
    where
        S: ReplayStore,
    {
        let Some(state) = self.sessions.get(&event.product_session_id) else {
            return Err(ReplayError::Authority(
                RecoveryReplayAuthorityError::ForeignIdentity,
            ));
        };
        let expected = runtime_identity(state)
            .map_err(|_| ReplayError::Authority(RecoveryReplayAuthorityError::ForeignIdentity))?;
        let expected_stream = expected.stream_key();
        let event_stream = event.identity.stream_key();
        let authority = CurrentRecoveryReplayAuthority {
            expected: &expected,
            expected_stream: &expected_stream,
        };
        ReplayStateMachine::new().accept(
            store,
            &authority,
            &event_stream,
            &event.identity,
            &event.frame,
        )
    }

    /// Replays persisted browser events strictly after the confirmed cursor.
    pub fn resume_browser<S>(
        &self,
        store: &mut S,
        request: &BrowserReconnectRequest,
    ) -> Result<ReplayBatch, ReplayError<BrowserReplayAuthorityError, S::Error>>
    where
        S: ReplayStore,
    {
        let Some(state) = self.sessions.get(&request.stream.product_session_id) else {
            return Err(ReplayError::Authority(
                BrowserReplayAuthorityError::UnknownProductSession,
            ));
        };
        if request.stream.stream_id != state.browser_stream_id
            || request.stream.authorization_epoch != state.browser_authorization_epoch
        {
            return Err(ReplayError::Authority(
                BrowserReplayAuthorityError::ForeignStream,
            ));
        }
        let stream = browser_replay_stream_key(&request.stream);
        let authority = BrowserReplayAuthority {
            expected: &request.stream,
        };
        ReplayStateMachine::new().resume(
            store,
            &authority,
            &stream,
            &request.stream,
            request.after_sequence,
            request.max_events,
        )
    }
}

/// Stable public event stream key used by the browser reconnect store.
#[must_use]
pub fn browser_replay_stream_key(stream: &BrowserRecoveryStream) -> ReplayStreamKey {
    ReplayStreamKey::new(format!(
        "browser-recovery:v1/{}/{}/{}",
        stream.product_session_id.0, stream.authorization_epoch, stream.stream_id
    ))
}

struct CurrentRecoveryReplayAuthority<'authority> {
    expected: &'authority RuntimeReplayIdentity,
    expected_stream: &'authority ReplayStreamKey,
}

impl ReplayAuthority for CurrentRecoveryReplayAuthority<'_> {
    type Context = RuntimeReplayIdentity;
    type Error = RecoveryReplayAuthorityError;

    fn validate_active_lease(
        &self,
        stream: &ReplayStreamKey,
        identity: &Self::Context,
    ) -> Result<(), Self::Error> {
        if identity == self.expected && stream == self.expected_stream {
            return Ok(());
        }
        if identity.lease.job_id == self.expected.lease.job_id
            && (identity.lease.attempt < self.expected.lease.attempt
                || decimal_less(
                    &identity.lease.fencing_token.0,
                    &self.expected.lease.fencing_token.0,
                )
                || identity.lease.worker_instance_id != self.expected.lease.worker_instance_id)
        {
            return Err(RecoveryReplayAuthorityError::OldWorkerFenced);
        }
        if stream != self.expected_stream {
            return Err(RecoveryReplayAuthorityError::StreamMismatch);
        }
        Err(RecoveryReplayAuthorityError::ForeignIdentity)
    }
}

struct BrowserReplayAuthority<'authority> {
    expected: &'authority BrowserRecoveryStream,
}

impl ReplayAuthority for BrowserReplayAuthority<'_> {
    type Context = BrowserRecoveryStream;
    type Error = BrowserReplayAuthorityError;

    fn validate_active_lease(
        &self,
        stream: &ReplayStreamKey,
        context: &Self::Context,
    ) -> Result<(), Self::Error> {
        if context == self.expected && stream == &browser_replay_stream_key(self.expected) {
            Ok(())
        } else {
            Err(BrowserReplayAuthorityError::ForeignStream)
        }
    }
}

fn worker_replacement(
    state: &SessionRecoveryState,
    request: &SessionRecoveryRequest,
) -> WorkerReplacementPlan {
    WorkerReplacementPlan {
        close_old_slot: WorkerSlotCloseRequest {
            authority: state.authority.slot.clone(),
            request_id: request.request_id.clone(),
            expected_revision: state.slot_revision,
            outcome: WorkerSlotState::RecoveryFailed,
            closed_at: request.recovered_at.clone(),
        },
        open_replacement_slot: WorkerSlotOpenRequest {
            authority: request.replacement.slot.clone(),
            resources: state.slot_resources,
            request_id: request.request_id.clone(),
            opened_at: request.recovered_at.clone(),
        },
    }
}

fn validate_state(state: &SessionRecoveryState) -> Result<(), RecoveryRoutingError> {
    validate_id(&state.product_session_id.0, "productSessionId", "psn_")?;
    if let Some(stage_run_id) = &state.stage_run_id {
        validate_id(&stage_run_id.0, "stageRunId", "run_")?;
    }
    if state.browser_stream_id.is_empty()
        || state.browser_stream_id.len() > 200
        || !state.browser_stream_id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
    {
        return Err(RecoveryRoutingError::InvalidField("browserStreamId"));
    }
    validate_revision(
        state.browser_authorization_epoch,
        "browserAuthorizationEpoch",
    )?;
    validate_revision(state.revision, "revision")?;
    validate_revision(state.slot_revision, "slotRevision")?;
    validate_authority(&state.authority)?;
    if state.confirmed_runtime_sequence > MAX_SAFE_INTEGER {
        return Err(RecoveryRoutingError::InvalidField(
            "confirmedRuntimeSequence",
        ));
    }
    if let ThreadRecoveryCapability::TransferableCheckpoint { checkpoint_sha256 } =
        &state.thread_capability
    {
        validate_digest(checkpoint_sha256)?;
    }
    Ok(())
}

fn validate_request(request: &SessionRecoveryRequest) -> Result<(), RecoveryRoutingError> {
    validate_id(&request.request_id.0, "requestId", "req_")?;
    validate_id(&request.product_session_id.0, "productSessionId", "psn_")?;
    validate_revision(request.expected_revision, "expectedRevision")?;
    validate_authority(&request.replacement)?;
    validate_instant(&request.recovered_at)?;
    if let Some(message_id) = &request.replay_message_id {
        validate_id(&message_id.0, "replayMessageId", "xmsg_")?;
    }
    if request.max_replay_events > 10_000 {
        return Err(RecoveryRoutingError::InvalidField("maxReplayEvents"));
    }
    Ok(())
}

fn validate_replacement(
    state: &SessionRecoveryState,
    request: &SessionRecoveryRequest,
) -> Result<(), RecoveryRoutingError> {
    let current = &state.authority.slot;
    let replacement = &request.replacement.slot;
    if current.job_id != replacement.job_id {
        return Err(RecoveryRoutingError::IdentityMismatch("executionJobId"));
    }
    if current.worker_instance_id == replacement.worker_instance_id {
        return Err(RecoveryRoutingError::WorkerInstanceNotReplaced);
    }
    if current.worker_session_id == replacement.worker_session_id {
        return Err(RecoveryRoutingError::WorkerSessionNotReplaced);
    }
    if replacement.lease_id == current.lease_id
        || replacement.attempt != current.attempt.saturating_add(1)
        || !decimal_less(&current.fencing_token.0, &replacement.fencing_token.0)
    {
        return Err(RecoveryRoutingError::LeaseNotNewer);
    }
    if request.recovered_at.0 < request.replacement.issued_at.0
        || request.recovered_at.0 >= request.replacement.expires_at.0
    {
        return Err(RecoveryRoutingError::InvalidField("recoveredAt"));
    }
    Ok(())
}

fn validate_authority(authority: &RecoveryExecutionAuthority) -> Result<(), RecoveryRoutingError> {
    let slot = &authority.slot;
    validate_id(&slot.worker_id.0, "workerId", "wrk_")?;
    validate_id(&slot.worker_instance_id.0, "workerInstanceId", "wki_")?;
    validate_id(&slot.worker_session_id.0, "workerSessionId", "wsn_")?;
    validate_id(&slot.codex_thread_id.0, "codexThreadId", "cdx_")?;
    validate_id(&slot.job_id.0, "executionJobId", "job_")?;
    validate_id(&slot.lease_id.0, "leaseId", "lse_")?;
    if !(1..=1_000).contains(&slot.attempt) {
        return Err(RecoveryRoutingError::InvalidField("attempt"));
    }
    validate_fencing_token(&slot.fencing_token.0)?;
    validate_instant(&authority.issued_at)?;
    validate_instant(&authority.expires_at)?;
    if authority.issued_at.0 >= authority.expires_at.0 {
        return Err(RecoveryRoutingError::InvalidField("leaseInterval"));
    }
    Ok(())
}

fn runtime_identity(
    state: &SessionRecoveryState,
) -> Result<RuntimeReplayIdentity, RecoveryRoutingError> {
    let attempt = i64::try_from(state.authority.slot.attempt)
        .map_err(|_| RecoveryRoutingError::InvalidField("attempt"))?;
    Ok(RuntimeReplayIdentity {
        lease: ExecutionLeaseStamp {
            attempt,
            expires_at: state.authority.expires_at.clone(),
            fencing_token: state.authority.slot.fencing_token.clone(),
            issued_at: state.authority.issued_at.clone(),
            job_id: state.authority.slot.job_id.clone(),
            lease_id: state.authority.slot.lease_id.clone(),
            worker_id: state.authority.slot.worker_id.clone(),
            worker_instance_id: state.authority.slot.worker_instance_id.clone(),
        },
        worker_session_id: state.authority.slot.worker_session_id.clone(),
        session_identity: SessionIdentity {
            codex_thread_id: state.authority.slot.codex_thread_id.clone(),
            product_session_id: state.product_session_id.clone(),
            stage_run_id: state.stage_run_id.clone(),
            worker_session_id: state.authority.slot.worker_session_id.clone(),
        },
        codex_thread_id: state.authority.slot.codex_thread_id.clone(),
    })
}

fn validate_revision(revision: u64, field: &'static str) -> Result<(), RecoveryRoutingError> {
    if !(1..=MAX_SAFE_INTEGER).contains(&revision) {
        return Err(RecoveryRoutingError::InvalidField(field));
    }
    Ok(())
}

fn next_revision(revision: u64) -> Result<u64, RecoveryRoutingError> {
    revision
        .checked_add(1)
        .filter(|next| *next <= MAX_SAFE_INTEGER)
        .ok_or(RecoveryRoutingError::InvalidField("revision"))
}

fn validate_id(value: &str, field: &'static str, prefix: &str) -> Result<(), RecoveryRoutingError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    });
    if !valid {
        return Err(RecoveryRoutingError::InvalidField(field));
    }
    Ok(())
}

fn validate_fencing_token(value: &str) -> Result<(), RecoveryRoutingError> {
    if value.is_empty()
        || value.len() > 20
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(RecoveryRoutingError::InvalidField("fencingToken"));
    }
    Ok(())
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), RecoveryRoutingError> {
    let Some(value) = digest.0.strip_prefix("sha256:") else {
        return Err(RecoveryRoutingError::InvalidField("checkpointSha256"));
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RecoveryRoutingError::InvalidField("checkpointSha256"));
    }
    Ok(())
}

fn validate_instant(instant: &Instant) -> Result<(), RecoveryRoutingError> {
    let value = instant.0.as_bytes();
    let valid = value.len() == 24
        && value[4] == b'-'
        && value[7] == b'-'
        && value[10] == b'T'
        && value[13] == b':'
        && value[16] == b':'
        && value[19] == b'.'
        && value[23] == b'Z'
        && value.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if !valid {
        return Err(RecoveryRoutingError::InvalidField("instant"));
    }
    Ok(())
}

fn decimal_less(left: &str, right: &str) -> bool {
    left.len() < right.len() || (left.len() == right.len() && left < right)
}
