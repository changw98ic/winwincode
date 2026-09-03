//! Worker-side handling for the generated `runtime.replay_request` message.
//!
//! The Control Plane sends this request to a Worker after reconnect.  This
//! module owns only the Worker-side response seam: the caller injects the
//! Worker-owned [`ReplayStore`] and the active-lease [`ReplayAuthority`].
//! Product state, Delivery state, and the public Control Plane event cursor do
//! not participate in the lookup.

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_domain::{
    CodexThreadId, ExecutionAckSequence, ExecutionSequence, SchemaVersion, SessionIdentity,
    WorkerSessionId,
};

use crate::generated::{
    ExecutionLeaseStamp, ExecutionPortMessage, LeaseWriteStatus, RuntimeAckMessage,
    RuntimeAckMessageKind, RuntimeEventMessage, RuntimeEventMessageKind,
    RuntimeReplayRequestMessage, RuntimeReplayRequestMessageKind,
};
use crate::replay::{
    ReplayAcknowledgementStore, ReplayAuthority, ReplayDecision, ReplayError, ReplayFrame,
    ReplaySequence, ReplayStateMachine, ReplayStore, ReplayStreamKey,
};
use crate::transport::ExecutionPortCore;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_REPLAY_EVENTS: i64 = 10_000;

/// Complete identity supplied to the Worker lease authority for replay.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeReplayIdentity {
    /// Current lease, attempt, Worker instance, and fencing token.
    pub lease: ExecutionLeaseStamp,
    /// Worker process session identity.
    pub worker_session_id: WorkerSessionId,
    /// Product session, optional stage run, Worker session, and Codex thread.
    pub session_identity: SessionIdentity,
    /// Top-level runtime-event Codex thread identity.
    pub codex_thread_id: CodexThreadId,
}

impl RuntimeReplayIdentity {
    /// Returns the canonical Worker-owned runtime stream for this exact lease
    /// and source identity.
    #[must_use]
    pub fn stream_key(&self) -> ReplayStreamKey {
        runtime_stream_key(
            &self.lease,
            &self.session_identity,
            &self.worker_session_id,
            &self.codex_thread_id,
        )
    }

    fn from_request(request: &RuntimeReplayRequestMessage) -> Self {
        Self {
            lease: request.lease.clone(),
            worker_session_id: request.worker_session_id.clone(),
            session_identity: request.session_identity.clone(),
            codex_thread_id: request.session_identity.codex_thread_id.clone(),
        }
    }

    fn from_event(message: &RuntimeEventMessage) -> Self {
        Self {
            lease: message.lease.clone(),
            worker_session_id: message.worker_session_id.clone(),
            session_identity: message.session_identity.clone(),
            codex_thread_id: message.codex_thread_id.clone(),
        }
    }

    fn from_ack(message: &RuntimeAckMessage) -> Self {
        Self {
            lease: message.lease.clone(),
            worker_session_id: message.worker_session_id.clone(),
            session_identity: message.session_identity.clone(),
            codex_thread_id: message.session_identity.codex_thread_id.clone(),
        }
    }
}

/// Stable Worker-owned stream identity for one runtime lease/session.
///
/// The key contains the complete execution identity and is length-prefixed so
/// arbitrary field boundaries cannot collide.  It deliberately excludes the
/// request id, message id, and cursor: retries and pages address the same
/// Worker stream rather than creating a second cursor.
#[must_use]
pub fn runtime_replay_stream_key(request: &RuntimeReplayRequestMessage) -> ReplayStreamKey {
    runtime_stream_key(
        &request.lease,
        &request.session_identity,
        &request.worker_session_id,
        &request.session_identity.codex_thread_id,
    )
}

/// Returns the stream key used when a Worker retains one original runtime
/// event frame.
///
/// A valid runtime event and its corresponding replay request produce the same
/// key.  The top-level event `codexThreadId` is included separately so a
/// malformed event cannot alias a valid session stream.
#[must_use]
pub fn runtime_event_stream_key(message: &RuntimeEventMessage) -> ReplayStreamKey {
    runtime_stream_key(
        &message.lease,
        &message.session_identity,
        &message.worker_session_id,
        &message.codex_thread_id,
    )
}

/// Returns the stream key addressed by a Control Plane runtime acknowledgement.
#[must_use]
pub fn runtime_ack_stream_key(message: &RuntimeAckMessage) -> ReplayStreamKey {
    runtime_stream_key(
        &message.lease,
        &message.session_identity,
        &message.worker_session_id,
        &message.session_identity.codex_thread_id,
    )
}

fn runtime_stream_key(
    lease: &ExecutionLeaseStamp,
    session_identity: &SessionIdentity,
    worker_session_id: &WorkerSessionId,
    codex_thread_id: &CodexThreadId,
) -> ReplayStreamKey {
    let mut value = String::from("runtime-worker-replay:v1");
    let attempt = lease.attempt.to_string();
    for component in [
        lease.job_id.0.as_str(),
        lease.lease_id.0.as_str(),
        lease.worker_id.0.as_str(),
        lease.worker_instance_id.0.as_str(),
        attempt.as_str(),
        lease.fencing_token.0.as_str(),
        worker_session_id.0.as_str(),
        session_identity.product_session_id.0.as_str(),
        session_identity
            .stage_run_id
            .as_ref()
            .map_or("", |stage_run_id| stage_run_id.0.as_str()),
        session_identity.codex_thread_id.0.as_str(),
        codex_thread_id.0.as_str(),
    ] {
        value.push('/');
        value.push_str(&component.len().to_string());
        value.push(':');
        value.push_str(component);
    }
    ReplayStreamKey::new(value)
}

/// A validated batch of original Worker runtime frames.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeReplayBatch {
    /// Highest sequence the Control Plane has confirmed as accepted.
    pub ack_sequence: ExecutionAckSequence,
    /// Highest contiguous sequence retained by the Worker.
    pub highest_sequence: ExecutionAckSequence,
    /// Original typed runtime messages after the requested sequence.
    pub events: Vec<RuntimeEventMessage>,
}

/// Result of processing one Control Plane `runtime.ack` message.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeReplayAckReceipt {
    /// Status carried by the Control Plane acknowledgement.
    pub status: LeaseWriteStatus,
    /// Durable Control Plane acknowledgement after processing.
    pub ack_sequence: ExecutionAckSequence,
    /// Highest contiguous frame retained by the Worker.
    pub highest_sequence: ExecutionAckSequence,
    /// Replay start carried by a gap acknowledgement.
    pub replay_from_sequence: Option<ExecutionSequence>,
    /// Original frames returned for a gap acknowledgement.
    pub replay: Option<RuntimeReplayBatch>,
}

/// Output of the Worker-side runtime replay core.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeReplayOutput {
    /// Original runtime frames requested after a cursor.
    Replay(RuntimeReplayBatch),
    /// Acknowledgement watermark processing result.
    Ack(RuntimeReplayAckReceipt),
}

/// Request-shape failures detected before the Worker store is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeReplayRequestError {
    /// The generated discriminator is not `runtime.replay_request`.
    WrongKind,
    /// The generated schema version is not supported by this Worker core.
    UnsupportedSchema,
    /// A required identity string is empty.
    EmptyIdentity,
    /// The lease attempt is not positive.
    InvalidAttempt,
    /// The lease fencing token is not canonical decimal text.
    InvalidFencingToken,
    /// The requested acknowledgement is negative.
    NegativeAfterSequence,
    /// The requested acknowledgement exceeds the public safe integer range.
    SequenceOutOfRange,
    /// The requested page size is outside the wire contract.
    InvalidMaxEvents,
    /// The duplicated `WorkerSession` fields do not agree.
    WorkerSessionMismatch,
}

impl fmt::Display for RuntimeReplayRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WrongKind => "runtime replay request kind is invalid",
            Self::UnsupportedSchema => "runtime replay request schema is unsupported",
            Self::EmptyIdentity => "runtime replay request identity is empty",
            Self::InvalidAttempt => "runtime replay request attempt is invalid",
            Self::InvalidFencingToken => "runtime replay request fencing token is invalid",
            Self::NegativeAfterSequence => "runtime replay request after sequence is negative",
            Self::SequenceOutOfRange => {
                "runtime replay request after sequence is outside the safe range"
            }
            Self::InvalidMaxEvents => "runtime replay request max events is invalid",
            Self::WorkerSessionMismatch => "runtime replay request WorkerSession identities differ",
        })
    }
}

impl std::error::Error for RuntimeReplayRequestError {}

/// Event-shape failures detected before retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeReplayEventError {
    /// The generated discriminator is not `runtime.event`.
    WrongKind,
    /// The generated schema version is unsupported.
    UnsupportedSchema,
    /// A required identity string is empty.
    EmptyIdentity,
    /// The event sequence is negative or zero.
    InvalidSequence,
    /// The event sequence exceeds the public safe integer range.
    SequenceOutOfRange,
    /// The lease attempt is not positive.
    InvalidAttempt,
    /// The lease fencing token is not canonical decimal text.
    InvalidFencingToken,
    /// The duplicated `WorkerSession` fields do not agree.
    WorkerSessionMismatch,
    /// The duplicated Codex thread fields do not agree.
    CodexThreadMismatch,
    /// The event has no stable event identifier.
    EmptyEventId,
    /// The generated event could not be encoded canonically.
    Serialization,
}

impl fmt::Display for RuntimeReplayEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WrongKind => "runtime event kind is invalid",
            Self::UnsupportedSchema => "runtime event schema is unsupported",
            Self::EmptyIdentity => "runtime event identity is empty",
            Self::InvalidSequence => "runtime event sequence is invalid",
            Self::SequenceOutOfRange => "runtime event sequence is outside the safe range",
            Self::InvalidAttempt => "runtime event attempt is invalid",
            Self::InvalidFencingToken => "runtime event fencing token is invalid",
            Self::WorkerSessionMismatch => "runtime event WorkerSession identities differ",
            Self::CodexThreadMismatch => "runtime event CodexThread identities differ",
            Self::EmptyEventId => "runtime event identifier is empty",
            Self::Serialization => "runtime event cannot be encoded canonically",
        })
    }
}

impl std::error::Error for RuntimeReplayEventError {}

/// Acknowledgement-shape failures detected before the Worker store changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeReplayAckError {
    /// The generated discriminator is not `runtime.ack`.
    WrongKind,
    /// The generated schema version is unsupported.
    UnsupportedSchema,
    /// A required identity string is empty.
    EmptyIdentity,
    /// The lease attempt is not positive.
    InvalidAttempt,
    /// The lease fencing token is not canonical decimal text.
    InvalidFencingToken,
    /// The acknowledgement sequence is negative.
    NegativeSequence,
    /// A sequence exceeds the public safe integer range.
    SequenceOutOfRange,
    /// The duplicated `WorkerSession` fields do not agree.
    WorkerSessionMismatch,
    /// A gap acknowledgement omitted its replay start.
    GapReplayMissing,
    /// A gap acknowledgement did not point to the next missing sequence.
    GapReplayMismatch,
    /// A non-gap acknowledgement supplied a replay start.
    UnexpectedReplayStart,
}

impl fmt::Display for RuntimeReplayAckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WrongKind => "runtime acknowledgement kind is invalid",
            Self::UnsupportedSchema => "runtime acknowledgement schema is unsupported",
            Self::EmptyIdentity => "runtime acknowledgement identity is empty",
            Self::InvalidAttempt => "runtime acknowledgement attempt is invalid",
            Self::InvalidFencingToken => "runtime acknowledgement fencing token is invalid",
            Self::NegativeSequence => "runtime acknowledgement sequence is negative",
            Self::SequenceOutOfRange => {
                "runtime acknowledgement sequence is outside the safe range"
            }
            Self::WorkerSessionMismatch => {
                "runtime acknowledgement WorkerSession identities differ"
            }
            Self::GapReplayMissing => "runtime gap acknowledgement has no replay start",
            Self::GapReplayMismatch => {
                "runtime gap acknowledgement replay start is not ack plus one"
            }
            Self::UnexpectedReplayStart => {
                "runtime non-gap acknowledgement has an unexpected replay start"
            }
        })
    }
}

impl std::error::Error for RuntimeReplayAckError {}

/// Corruption found in one retained Worker frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeReplayFrameError {
    /// The durable frame has no bytes.
    Empty,
    /// The durable frame is not a generated runtime event.
    Malformed,
    /// The frame bytes differ from canonical generated JSON.
    NonCanonical,
    /// The frame digest differs from canonical generated JSON.
    DigestMismatch,
    /// The frame discriminator is not `runtime.event`.
    WrongKind,
    /// The frame schema version is unsupported.
    UnsupportedSchema,
    /// The frame identity differs from the stored row or request.
    IdentityMismatch,
    /// The frame sequence differs from the stored row.
    SequenceMismatch,
}

impl fmt::Display for RuntimeReplayFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "runtime replay frame is empty",
            Self::Malformed => "runtime replay frame is malformed",
            Self::NonCanonical => "runtime replay frame is not canonical",
            Self::DigestMismatch => "runtime replay frame digest is invalid",
            Self::WrongKind => "runtime replay frame kind is invalid",
            Self::UnsupportedSchema => "runtime replay frame schema is unsupported",
            Self::IdentityMismatch => "runtime replay frame identity is foreign",
            Self::SequenceMismatch => "runtime replay frame sequence is foreign",
        })
    }
}

impl std::error::Error for RuntimeReplayFrameError {}

/// Failure from request validation, Worker authority, durable state, or a
/// retained frame.
#[derive(Debug, PartialEq, Eq)]
pub enum RuntimeReplayError<AuthorityError, StoreError> {
    /// The request shape was rejected before any store read.
    Request(RuntimeReplayRequestError),
    /// The event shape was rejected before retention.
    Event(RuntimeReplayEventError),
    /// The acknowledgement shape was rejected before state mutation.
    Acknowledgement(RuntimeReplayAckError),
    /// The active Worker lease/fence authority rejected the request.
    Replay(ReplayError<AuthorityError, StoreError>),
    /// A retained frame could not be returned as the requested generated type.
    Frame {
        sequence: ReplaySequence,
        error: RuntimeReplayFrameError,
    },
    /// The durable acknowledgement cannot be represented by the generated
    /// `ExecutionAckSequence` field.
    AckOutOfRange,
    /// A caller sent a different message union member to this Worker core.
    UnsupportedMessage,
}

impl<AuthorityError: fmt::Debug, StoreError: fmt::Debug> fmt::Display
    for RuntimeReplayError<AuthorityError, StoreError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(error) => error.fmt(formatter),
            Self::Event(error) => error.fmt(formatter),
            Self::Acknowledgement(error) => error.fmt(formatter),
            Self::Replay(error) => write!(formatter, "Worker replay state failed: {error:?}"),
            Self::Frame { sequence, error } => {
                write!(formatter, "Worker replay frame {sequence} failed: {error}")
            }
            Self::AckOutOfRange => {
                formatter.write_str("Worker replay acknowledgement is out of range")
            }
            Self::UnsupportedMessage => {
                formatter.write_str("Worker replay core received an unsupported message")
            }
        }
    }
}

impl<AuthorityError: fmt::Debug, StoreError: fmt::Debug> std::error::Error
    for RuntimeReplayError<AuthorityError, StoreError>
{
}

/// Stateless Worker-side responder over an injected durable replay store.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeReplayResponder {
    machine: ReplayStateMachine,
}

impl RuntimeReplayResponder {
    /// Creates a responder with no in-memory authority or cursor state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            machine: ReplayStateMachine::new(),
        }
    }

    /// Retains one generated runtime event before the Worker sends it.
    ///
    /// The canonical generated JSON is retained as the original frame, while
    /// the nested event record supplies the semantic SHA-256 duplicate digest.
    /// The replay state machine owns lease, ordering, duplicate, and conflict
    /// handling.
    ///
    /// # Errors
    ///
    /// Returns before a store write for malformed event fields, then forwards
    /// authority, durable state, and append failures.
    pub fn retain_runtime_event<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        message: &RuntimeEventMessage,
    ) -> Result<ReplayDecision, RuntimeReplayError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        validate_event(message).map_err(RuntimeReplayError::Event)?;
        let canonical = serde_json::to_vec(message)
            .map_err(|_| RuntimeReplayError::Event(RuntimeReplayEventError::Serialization))?;
        let sequence = u64::try_from(message.event.sequence.0)
            .map_err(|_| RuntimeReplayError::Event(RuntimeReplayEventError::InvalidSequence))?;
        let event_canonical = serde_json::to_vec(&message.event)
            .map_err(|_| RuntimeReplayError::Event(RuntimeReplayEventError::Serialization))?;
        let digest = format!("sha256:{:x}", Sha256::digest(&event_canonical));
        let frame = ReplayFrame::new(
            message.event.event_id.0.clone(),
            sequence,
            digest,
            canonical,
        );
        let identity = RuntimeReplayIdentity::from_event(message);
        self.machine
            .accept(
                store,
                authority,
                &runtime_event_stream_key(message),
                &identity,
                &frame,
            )
            .map_err(RuntimeReplayError::Replay)
    }

    /// Reads and decodes original frames after the request's acknowledged
    /// sequence.  The injected store remains the only source of durable state.
    ///
    /// # Errors
    ///
    /// Returns before a store read for malformed request fields, then forwards
    /// active-lease, store, corruption, cursor, and frame failures.
    pub fn resume<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        request: &RuntimeReplayRequestMessage,
    ) -> Result<RuntimeReplayBatch, RuntimeReplayError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        let after_sequence = validate_request(request).map_err(RuntimeReplayError::Request)?;
        let stream = runtime_replay_stream_key(request);
        let identity = RuntimeReplayIdentity::from_request(request);
        let batch = self
            .machine
            .resume(
                store,
                authority,
                &stream,
                &identity,
                after_sequence,
                usize::try_from(request.max_events).map_err(|_| {
                    RuntimeReplayError::Request(RuntimeReplayRequestError::InvalidMaxEvents)
                })?,
            )
            .map_err(RuntimeReplayError::Replay)?;
        let ack_sequence = i64::try_from(batch.ack_sequence)
            .map(ExecutionAckSequence)
            .map_err(|_| RuntimeReplayError::AckOutOfRange)?;
        let highest_sequence = i64::try_from(batch.highest_sequence)
            .map(ExecutionAckSequence)
            .map_err(|_| RuntimeReplayError::AckOutOfRange)?;
        let events = batch
            .events
            .into_iter()
            .map(|frame| decode_frame(&identity, &frame))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RuntimeReplayBatch {
            ack_sequence,
            highest_sequence,
            events,
        })
    }

    /// Processes one Control Plane `runtime.ack` message.
    ///
    /// Accepted and duplicate statuses advance the durable acknowledgement
    /// watermark monotonically. A gap status leaves that watermark unchanged
    /// and returns original retained frames from `replayFromSequence`.
    ///
    /// # Errors
    ///
    /// Returns malformed acknowledgement fields before a store write, then
    /// forwards authority, durable state, cursor, and frame failures.
    pub fn acknowledge<S, A>(
        &self,
        store: &mut S,
        authority: &A,
        message: &RuntimeAckMessage,
    ) -> Result<RuntimeReplayAckReceipt, RuntimeReplayError<A::Error, S::Error>>
    where
        S: ReplayAcknowledgementStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        let validated = validate_ack(message).map_err(RuntimeReplayError::Acknowledgement)?;
        let identity = RuntimeReplayIdentity::from_ack(message);
        let stream = runtime_ack_stream_key(message);
        let before = self
            .machine
            .resume(store, authority, &stream, &identity, 0, 1)
            .map_err(RuntimeReplayError::Replay)?;

        let (ack_sequence, replay) = match message.status {
            LeaseWriteStatus::Duplicate if validated.ack_sequence <= before.ack_sequence => {
                // The Control Plane can deliver an older duplicate response
                // after a later contiguous acknowledgement was already
                // applied. Its authority and shape were validated above; the
                // durable high-water mark remains the canonical receipt.
                (before.ack_sequence, None)
            }
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate => {
                let ack_sequence = self
                    .machine
                    .acknowledge(store, authority, &stream, &identity, validated.ack_sequence)
                    .map_err(RuntimeReplayError::Replay)?;
                (ack_sequence, None)
            }
            LeaseWriteStatus::Gap => {
                let Some(replay_from) = validated.replay_from_sequence else {
                    return Err(RuntimeReplayError::Acknowledgement(
                        RuntimeReplayAckError::GapReplayMissing,
                    ));
                };
                let replay_batch = self
                    .machine
                    .resume(
                        store,
                        authority,
                        &stream,
                        &identity,
                        replay_from.saturating_sub(1),
                        usize::try_from(MAX_REPLAY_EVENTS).map_err(|_| {
                            RuntimeReplayError::Acknowledgement(
                                RuntimeReplayAckError::SequenceOutOfRange,
                            )
                        })?,
                    )
                    .map_err(RuntimeReplayError::Replay)?;
                (before.ack_sequence, Some(replay_batch))
            }
            LeaseWriteStatus::RejectedConflict
            | LeaseWriteStatus::RejectedExpiredLease
            | LeaseWriteStatus::RejectedStaleFencingToken
            | LeaseWriteStatus::RejectedWorkerInstance => (before.ack_sequence, None),
        };
        let after = self
            .machine
            .resume(store, authority, &stream, &identity, 0, 1)
            .map_err(RuntimeReplayError::Replay)?;
        let replay = replay
            .map(|batch| runtime_batch(&batch, &identity))
            .transpose()?;
        Ok(RuntimeReplayAckReceipt {
            status: message.status.clone(),
            ack_sequence: to_ack_sequence(ack_sequence)?,
            highest_sequence: to_ack_sequence(after.highest_sequence)?,
            replay_from_sequence: message.replay_from_sequence.clone(),
            replay,
        })
    }
}

/// Worker-side `ExecutionPortCore` for CP→Worker replay requests and
/// acknowledgements. Local and remote adapters call this same responder.
pub struct RuntimeReplayCore<S, A> {
    responder: RuntimeReplayResponder,
    store: S,
    authority: A,
}

impl<S, A> RuntimeReplayCore<S, A> {
    /// Creates a Worker replay core over caller-owned durable state and lease
    /// authority.
    #[must_use]
    pub const fn new(store: S, authority: A) -> Self {
        Self {
            responder: RuntimeReplayResponder::new(),
            store,
            authority,
        }
    }

    /// Retains one runtime event in the injected Worker store before sending.
    ///
    /// # Errors
    ///
    /// Forwards retention validation, authority, and durable-state errors.
    pub fn retain_runtime_event(
        &mut self,
        message: &RuntimeEventMessage,
    ) -> Result<ReplayDecision, RuntimeReplayError<A::Error, S::Error>>
    where
        S: ReplayStore,
        A: ReplayAuthority<Context = RuntimeReplayIdentity>,
    {
        self.responder
            .retain_runtime_event(&mut self.store, &self.authority, message)
    }

    /// Returns the injected store so a caller can close or replace the Worker
    /// process while preserving durable state in the adapter.
    #[must_use]
    pub fn into_store(self) -> S {
        self.store
    }
}

impl<S, A> ExecutionPortCore for RuntimeReplayCore<S, A>
where
    S: ReplayAcknowledgementStore,
    A: ReplayAuthority<Context = RuntimeReplayIdentity>,
{
    type Error = RuntimeReplayError<A::Error, S::Error>;
    type Output = RuntimeReplayOutput;

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        match message {
            ExecutionPortMessage::RuntimeReplayRequestMessage(request) => self
                .responder
                .resume(&mut self.store, &self.authority, request)
                .map(RuntimeReplayOutput::Replay),
            ExecutionPortMessage::RuntimeAckMessage(ack) => self
                .responder
                .acknowledge(&mut self.store, &self.authority, ack)
                .map(RuntimeReplayOutput::Ack),
            _ => Err(RuntimeReplayError::UnsupportedMessage),
        }
    }
}

fn validate_request(
    request: &RuntimeReplayRequestMessage,
) -> Result<ReplaySequence, RuntimeReplayRequestError> {
    if request.kind != RuntimeReplayRequestMessageKind::RuntimeReplayRequest {
        return Err(RuntimeReplayRequestError::WrongKind);
    }
    if request.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(RuntimeReplayRequestError::UnsupportedSchema);
    }
    if request.message_id.0.is_empty() || request.request_id.0.is_empty() {
        return Err(RuntimeReplayRequestError::EmptyIdentity);
    }
    validate_identity(
        &request.lease,
        &request.worker_session_id,
        &request.session_identity,
    )
    .map_err(|error| match error {
        IdentityError::Empty => RuntimeReplayRequestError::EmptyIdentity,
        IdentityError::Attempt => RuntimeReplayRequestError::InvalidAttempt,
        IdentityError::Fence => RuntimeReplayRequestError::InvalidFencingToken,
    })?;
    if request.session_identity.worker_session_id != request.worker_session_id {
        return Err(RuntimeReplayRequestError::WorkerSessionMismatch);
    }
    let after_sequence = u64::try_from(request.after_sequence.0)
        .map_err(|_| RuntimeReplayRequestError::NegativeAfterSequence)?;
    if after_sequence > MAX_SAFE_INTEGER {
        return Err(RuntimeReplayRequestError::SequenceOutOfRange);
    }
    if !(1..=MAX_REPLAY_EVENTS).contains(&request.max_events) {
        return Err(RuntimeReplayRequestError::InvalidMaxEvents);
    }
    Ok(after_sequence)
}

fn validate_event(message: &RuntimeEventMessage) -> Result<(), RuntimeReplayEventError> {
    if message.kind != RuntimeEventMessageKind::RuntimeEvent {
        return Err(RuntimeReplayEventError::WrongKind);
    }
    if message.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(RuntimeReplayEventError::UnsupportedSchema);
    }
    if message.message_id.0.is_empty() {
        return Err(RuntimeReplayEventError::EmptyIdentity);
    }
    validate_identity(
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
    )
    .map_err(|error| match error {
        IdentityError::Empty => RuntimeReplayEventError::EmptyIdentity,
        IdentityError::Attempt => RuntimeReplayEventError::InvalidAttempt,
        IdentityError::Fence => RuntimeReplayEventError::InvalidFencingToken,
    })?;
    if message.session_identity.worker_session_id != message.worker_session_id {
        return Err(RuntimeReplayEventError::WorkerSessionMismatch);
    }
    if message.codex_thread_id != message.session_identity.codex_thread_id {
        return Err(RuntimeReplayEventError::CodexThreadMismatch);
    }
    if message.event.event_id.0.is_empty() {
        return Err(RuntimeReplayEventError::EmptyEventId);
    }
    let sequence = u64::try_from(message.event.sequence.0)
        .map_err(|_| RuntimeReplayEventError::InvalidSequence)?;
    if sequence == 0 {
        return Err(RuntimeReplayEventError::InvalidSequence);
    }
    if sequence > MAX_SAFE_INTEGER {
        return Err(RuntimeReplayEventError::SequenceOutOfRange);
    }
    Ok(())
}

fn validate_ack(message: &RuntimeAckMessage) -> Result<ValidatedAck, RuntimeReplayAckError> {
    if message.kind != RuntimeAckMessageKind::RuntimeAck {
        return Err(RuntimeReplayAckError::WrongKind);
    }
    if message.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(RuntimeReplayAckError::UnsupportedSchema);
    }
    if message.message_id.0.is_empty() {
        return Err(RuntimeReplayAckError::EmptyIdentity);
    }
    validate_identity(
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
    )
    .map_err(|error| match error {
        IdentityError::Empty => RuntimeReplayAckError::EmptyIdentity,
        IdentityError::Attempt => RuntimeReplayAckError::InvalidAttempt,
        IdentityError::Fence => RuntimeReplayAckError::InvalidFencingToken,
    })?;
    if message.session_identity.worker_session_id != message.worker_session_id {
        return Err(RuntimeReplayAckError::WorkerSessionMismatch);
    }
    let ack_sequence = u64::try_from(message.ack_sequence.0)
        .map_err(|_| RuntimeReplayAckError::NegativeSequence)?;
    if ack_sequence > MAX_SAFE_INTEGER {
        return Err(RuntimeReplayAckError::SequenceOutOfRange);
    }
    let replay_from_sequence = if message.status == LeaseWriteStatus::Gap {
        let replay_from = message
            .replay_from_sequence
            .as_ref()
            .ok_or(RuntimeReplayAckError::GapReplayMissing)?;
        let replay_from =
            u64::try_from(replay_from.0).map_err(|_| RuntimeReplayAckError::SequenceOutOfRange)?;
        if replay_from > MAX_SAFE_INTEGER {
            return Err(RuntimeReplayAckError::SequenceOutOfRange);
        }
        if replay_from == 0 || replay_from != ack_sequence.saturating_add(1) {
            return Err(RuntimeReplayAckError::GapReplayMismatch);
        }
        Some(replay_from)
    } else {
        if message.replay_from_sequence.is_some() {
            return Err(RuntimeReplayAckError::UnexpectedReplayStart);
        }
        None
    };
    Ok(ValidatedAck {
        ack_sequence,
        replay_from_sequence,
    })
}

struct ValidatedAck {
    ack_sequence: ReplaySequence,
    replay_from_sequence: Option<ReplaySequence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityError {
    Empty,
    Attempt,
    Fence,
}

fn validate_identity(
    lease: &ExecutionLeaseStamp,
    worker_session_id: &WorkerSessionId,
    session_identity: &SessionIdentity,
) -> Result<(), IdentityError> {
    if lease.job_id.0.is_empty()
        || lease.lease_id.0.is_empty()
        || lease.worker_id.0.is_empty()
        || lease.worker_instance_id.0.is_empty()
        || worker_session_id.0.is_empty()
        || session_identity.product_session_id.0.is_empty()
        || session_identity
            .stage_run_id
            .as_ref()
            .is_some_and(|stage_run_id| stage_run_id.0.is_empty())
        || session_identity.worker_session_id.0.is_empty()
        || session_identity.codex_thread_id.0.is_empty()
    {
        return Err(IdentityError::Empty);
    }
    if lease.attempt <= 0 {
        return Err(IdentityError::Attempt);
    }
    if lease.fencing_token.0.is_empty()
        || lease.fencing_token.0.len() > 20
        || lease.fencing_token.0.starts_with('0')
        || !lease
            .fencing_token
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(IdentityError::Fence);
    }
    Ok(())
}

fn runtime_batch<AuthorityError, StoreError>(
    batch: &crate::replay::ReplayBatch,
    identity: &RuntimeReplayIdentity,
) -> Result<RuntimeReplayBatch, RuntimeReplayError<AuthorityError, StoreError>> {
    let events = batch
        .events
        .iter()
        .map(|frame| decode_frame(identity, frame))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RuntimeReplayBatch {
        ack_sequence: to_ack_sequence(batch.ack_sequence)?,
        highest_sequence: to_ack_sequence(batch.highest_sequence)?,
        events,
    })
}

fn to_ack_sequence<AuthorityError, StoreError>(
    sequence: ReplaySequence,
) -> Result<ExecutionAckSequence, RuntimeReplayError<AuthorityError, StoreError>> {
    i64::try_from(sequence)
        .map(ExecutionAckSequence)
        .map_err(|_| RuntimeReplayError::AckOutOfRange)
}

fn decode_frame<AuthorityError, StoreError>(
    identity: &RuntimeReplayIdentity,
    frame: &ReplayFrame,
) -> Result<RuntimeEventMessage, RuntimeReplayError<AuthorityError, StoreError>> {
    let sequence = frame.sequence;
    if frame.frame.is_empty() {
        return Err(RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::Empty,
        });
    }
    let message: RuntimeEventMessage =
        serde_json::from_slice(&frame.frame).map_err(|_| RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::Malformed,
        })?;
    let canonical = serde_json::to_vec(&message).map_err(|_| RuntimeReplayError::Frame {
        sequence,
        error: RuntimeReplayFrameError::NonCanonical,
    })?;
    if canonical != frame.frame {
        return Err(RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::NonCanonical,
        });
    }
    let event_canonical =
        serde_json::to_vec(&message.event).map_err(|_| RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::NonCanonical,
        })?;
    let digest = format!("sha256:{:x}", Sha256::digest(&event_canonical));
    if digest != frame.digest {
        return Err(RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::DigestMismatch,
        });
    }
    if message.kind != RuntimeEventMessageKind::RuntimeEvent {
        return Err(RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::WrongKind,
        });
    }
    if message.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::UnsupportedSchema,
        });
    }
    let message_sequence =
        u64::try_from(message.event.sequence.0).map_err(|_| RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::SequenceMismatch,
        })?;
    if message_sequence != sequence || message.event.event_id.0 != frame.event_id {
        return Err(RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::SequenceMismatch,
        });
    }
    if RuntimeReplayIdentity::from_event(&message) != *identity {
        return Err(RuntimeReplayError::Frame {
            sequence,
            error: RuntimeReplayFrameError::IdentityMismatch,
        });
    }
    Ok(message)
}
